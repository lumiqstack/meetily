//! Meeting audio on disk: which files it is, whether it may be removed, and
//! removing it. Nothing here is persisted; the disk is the only truth.

mod audio_files;
mod folder;
mod orphans;
mod verdict;

use crate::database::repositories::meeting::MeetingsRepository;
use crate::state::AppState;
use audio_files::{AudioFiles, FolderShape, Scan};
use folder::{remove_meeting_folder, FolderIndex, FolderRemoval};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;
use tauri::{AppHandle, Runtime};
use verdict::{judge, Activity, LiveRecording, Verdict};

pub(crate) use audio_files::{has_recoverable_audio, primary_audio};
pub(crate) use folder::FolderKey;
pub use verdict::{AbsentReason, HoldReason};
pub(crate) use verdict::Facts;

#[derive(Debug, Serialize, Clone, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AudioStatus {
    Absent { reason: AbsentReason },
    Held { bytes: u64, reason: HoldReason },
    Removable { bytes: u64 },
}

impl AudioStatus {
    fn bytes(&self) -> u64 {
        match self {
            AudioStatus::Absent { .. } => 0,
            AudioStatus::Held { bytes, .. } | AudioStatus::Removable { bytes } => *bytes,
        }
    }
}

impl From<&Verdict> for AudioStatus {
    fn from(verdict: &Verdict) -> Self {
        match verdict {
            Verdict::Absent(reason) => AudioStatus::Absent { reason: *reason },
            Verdict::Held { bytes, reason } => AudioStatus::Held { bytes: *bytes, reason: *reason },
            Verdict::Removable(r) => AudioStatus::Removable { bytes: r.bytes() },
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Survey {
    /// Only meetings whose audio is on this computer.
    pub meetings: Vec<MeetingAudio>,
    pub orphans: Vec<OrphanFolder>,
    pub volumes: Vec<Volume>,
    pub audio_bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct MeetingAudio {
    pub meeting_id: String,
    pub title: String,
    pub created_at: String,
    pub audio: AudioStatus,
}

#[derive(Debug, Serialize)]
pub struct OrphanFolder {
    pub folder: String,
    pub name: String,
    pub modified_at: String,
    pub bytes: u64,
    pub hold: Option<HoldReason>,
}

#[derive(Debug, Serialize)]
pub struct Volume {
    pub mount_point: String,
    pub free_bytes: u64,
    pub total_bytes: u64,
    pub audio_bytes: u64,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PurgeRequest {
    pub meeting_ids: Vec<String>,
    /// Lookup keys into a fresh orphan scan, never used as paths to delete.
    pub orphan_folders: Vec<String>,
}

#[derive(Debug, Serialize, Default)]
pub struct PurgeReport {
    pub freed_bytes: u64,
    pub purged_meetings: Vec<String>,
    pub purged_orphans: usize,
    /// Not purged because the rule said no at deletion time. Not an error.
    pub skipped: Vec<Skipped>,
    /// The rule said yes and the filesystem refused part of it.
    pub failed: Vec<Failed>,
}

#[derive(Debug, Serialize)]
pub struct Skipped {
    pub target: String,
    pub reason: HoldReason,
}

#[derive(Debug, Serialize)]
pub struct Failed {
    pub target: String,
    pub files: Vec<String>,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(tag = "folder", rename_all = "snake_case")]
pub enum DeleteMeetingReport {
    None,
    Removed { freed_bytes: u64 },
    /// A recording, transcription or import is using the folder. It becomes an
    /// orphan the Storage tab lists.
    KeptBusy,
    KeptShared,
    KeptUnrecognized,
    KeptForeignFiles { freed_bytes: u64, path: String },
    Failed { path: String },
}

impl DeleteMeetingReport {
    fn label(&self) -> &'static str {
        match self {
            DeleteMeetingReport::None => "none",
            DeleteMeetingReport::Removed { .. } => "removed",
            DeleteMeetingReport::KeptBusy => "kept_busy",
            DeleteMeetingReport::KeptShared => "kept_shared",
            DeleteMeetingReport::KeptUnrecognized => "kept_unrecognized",
            DeleteMeetingReport::KeptForeignFiles { .. } => "kept_foreign_files",
            DeleteMeetingReport::Failed { .. } => "failed",
        }
    }
}

static PURGE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn capture_activity(pool: &SqlitePool) -> sqlx::Result<Activity> {
    use crate::audio::recording_commands::{current_meeting_folder, recording_in_progress};

    let recording = match current_meeting_folder() {
        Some(folder) => LiveRecording::At(FolderKey::of(folder)),
        None if recording_in_progress() => LiveRecording::Unlocated,
        None => LiveRecording::None,
    };
    let rows: Vec<(Option<String>, Option<String>)> =
        sqlx::query_as("SELECT meeting_id, folder_path FROM background_jobs")
            .fetch_all(pool)
            .await?;
    let mut activity = Activity::idle();
    activity.recording = recording;
    activity.import_running = crate::audio::import::is_import_in_progress();
    for (meeting_id, folder) in rows {
        activity.journaled_meetings.extend(meeting_id);
        if let Some(folder) = folder.filter(|f| !f.trim().is_empty()) {
            activity.journaled_folders.push(FolderKey::of(folder));
        }
    }
    Ok(activity)
}

fn scan_of(facts: &Facts) -> std::io::Result<Scan> {
    match facts.folder() {
        Some(folder) => AudioFiles::scan(Path::new(folder)),
        None => Ok(Scan::FolderMissing),
    }
}

fn rfc3339(time: SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(time).to_rfc3339()
}

fn volumes(paths: &[(PathBuf, u64)]) -> Vec<Volume> {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let mounts: Vec<(FolderKey, &sysinfo::Disk)> =
        disks.list().iter().map(|d| (FolderKey::of(d.mount_point()), d)).collect();
    let mut out: Vec<Volume> = Vec::new();
    for (path, bytes) in paths {
        let key = FolderKey::of(path);
        let best = mounts
            .iter()
            .filter(|(mount, _)| mount.contains(&key))
            .max_by_key(|(_, disk)| disk.mount_point().components().count());
        let Some((_, disk)) = best else { continue };
        let mount_point = disk.mount_point().to_string_lossy().into_owned();
        match out.iter_mut().find(|v| v.mount_point == mount_point) {
            Some(volume) => volume.audio_bytes += bytes,
            None => out.push(Volume {
                mount_point,
                free_bytes: disk.available_space(),
                total_bytes: disk.total_space(),
                audio_bytes: *bytes,
            }),
        }
    }
    out
}

fn survey_blocking(facts: Vec<Facts>, index: FolderIndex, activity: Activity, now: SystemTime) -> Survey {
    let mut meetings = Vec::new();
    let mut on_disk: Vec<(PathBuf, u64)> = Vec::new();
    for facts in facts {
        let scan = match scan_of(&facts) {
            Ok(scan) => scan,
            Err(e) => {
                log::warn!("[recordings] could not scan the folder of meeting {}: {:?}", facts.meeting_id, e.kind());
                continue;
            }
        };
        let transcribing = crate::audio::retranscription::is_retranscription_active_for_meeting(&facts.meeting_id);
        let status = AudioStatus::from(&judge(&facts, transcribing, &activity, &index, scan));
        if matches!(status, AudioStatus::Absent { .. }) {
            continue;
        }
        if let Some(folder) = facts.folder() {
            on_disk.push((PathBuf::from(folder), status.bytes()));
        }
        meetings.push(MeetingAudio {
            created_at: facts.created_at.0.to_rfc3339(),
            meeting_id: facts.meeting_id,
            title: facts.title,
            audio: status,
        });
    }

    let orphans: Vec<OrphanFolder> = orphans::find_orphans(&index, &activity, now)
        .into_iter()
        .map(|o| {
            on_disk.push((o.folder.clone(), o.bytes));
            OrphanFolder {
                name: o.folder.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                folder: o.folder.to_string_lossy().into_owned(),
                modified_at: rfc3339(o.newest_change),
                bytes: o.bytes,
                hold: o.hold,
            }
        })
        .collect();

    on_disk.extend(index.recordings_roots().into_iter().map(|root| (root, 0)));
    let audio_bytes = meetings.iter().map(|m| m.audio.bytes()).sum::<u64>()
        + orphans.iter().map(|o| o.bytes).sum::<u64>();
    Survey { meetings, orphans, volumes: volumes(&on_disk), audio_bytes }
}

pub(crate) async fn survey(pool: &SqlitePool, roots: Vec<PathBuf>) -> anyhow::Result<Survey> {
    survey_at(pool, roots, SystemTime::now()).await
}

async fn survey_at(pool: &SqlitePool, roots: Vec<PathBuf>, now: SystemTime) -> anyhow::Result<Survey> {
    let facts = MeetingsRepository::audio_facts(pool, None).await?;
    let index = FolderIndex::load(pool, roots).await?;
    let activity = capture_activity(pool).await?;
    let survey =
        tokio::task::spawn_blocking(move || survey_blocking(facts, index, activity, now)).await?;
    log::info!(
        "[recordings] survey: {} meetings with audio, {} orphan folders, {} bytes",
        survey.meetings.len(),
        survey.orphans.len(),
        survey.audio_bytes
    );
    Ok(survey)
}

pub(crate) async fn status(pool: &SqlitePool, roots: Vec<PathBuf>, meeting_id: &str) -> anyhow::Result<AudioStatus> {
    let facts = MeetingsRepository::audio_facts(pool, Some(meeting_id))
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("Meeting not found"))?;
    let index = FolderIndex::load(pool, roots).await?;
    let activity = capture_activity(pool).await?;
    tokio::task::spawn_blocking(move || {
        let transcribing = crate::audio::retranscription::is_retranscription_active_for_meeting(&facts.meeting_id);
        let scan = scan_of(&facts)?;
        Ok::<_, anyhow::Error>(AudioStatus::from(&judge(&facts, transcribing, &activity, &index, scan)))
    })
    .await?
}

enum MeetingOutcome {
    Purged(u64),
    Skipped(HoldReason),
    Failed { freed_bytes: u64, files: Vec<String> },
}

fn purge_meeting(facts: &Facts, activity: &Activity, index: &FolderIndex) -> MeetingOutcome {
    let scan = match scan_of(facts) {
        Ok(scan) => scan,
        Err(e) => return MeetingOutcome::Failed { freed_bytes: 0, files: vec![format!("{:?}", e.kind())] },
    };
    // The caller holds the meeting's retranscription reservation, so nothing is transcribing it.
    match judge(facts, false, activity, index, scan) {
        Verdict::Absent(_) => MeetingOutcome::Purged(0),
        Verdict::Held { reason, .. } => MeetingOutcome::Skipped(reason),
        Verdict::Removable(removable) => {
            let deleted = removable.delete();
            if deleted.failed.is_empty() {
                MeetingOutcome::Purged(deleted.freed_bytes)
            } else {
                MeetingOutcome::Failed { freed_bytes: deleted.freed_bytes, files: deleted.failed }
            }
        }
    }
}

/// Removes the audio of the requested meetings and the requested orphan
/// folders. Every target is re-judged at deletion time; the request carries
/// ids and lookup keys only.
pub(crate) async fn purge(pool: &SqlitePool, roots: Vec<PathBuf>, request: PurgeRequest) -> anyhow::Result<PurgeReport> {
    purge_at(pool, roots, request, SystemTime::now()).await
}

async fn purge_at(
    pool: &SqlitePool,
    roots: Vec<PathBuf>,
    request: PurgeRequest,
    now: SystemTime,
) -> anyhow::Result<PurgeReport> {
    let _serial = PURGE_LOCK.lock().await;
    let mut report = PurgeReport::default();
    let index = Arc::new(FolderIndex::load(pool, roots.clone()).await?);

    let mut seen = std::collections::HashSet::new();
    for id in request.meeting_ids.into_iter().filter(|id| seen.insert(id.clone())) {
        let Some(reservation) = crate::audio::retranscription::reserve_meeting_audio(&id) else {
            report.skipped.push(Skipped { target: id, reason: HoldReason::Transcribing });
            continue;
        };
        // Read after the reservation: from here no retranscription can start on this meeting.
        let facts = match MeetingsRepository::audio_facts(pool, Some(&id)).await {
            Ok(rows) => rows.into_iter().next(),
            Err(e) => {
                log::warn!("[recordings] purge could not read meeting {}: {}", id, e);
                report.failed.push(Failed { target: id, files: vec!["database".into()] });
                continue;
            }
        };
        let Some(facts) = facts else {
            report.skipped.push(Skipped { target: id, reason: HoldReason::Vanished });
            continue;
        };
        let activity = match capture_activity(pool).await {
            Ok(activity) => activity,
            Err(e) => {
                log::warn!("[recordings] purge could not read the job journal for {}: {}", id, e);
                report.failed.push(Failed { target: id, files: vec!["database".into()] });
                continue;
            }
        };
        let index = Arc::clone(&index);
        let outcome = tokio::task::spawn_blocking(move || {
            let _reservation = reservation;
            purge_meeting(&facts, &activity, &index)
        })
        .await?;
        match outcome {
            MeetingOutcome::Purged(bytes) => {
                report.freed_bytes += bytes;
                report.purged_meetings.push(id);
            }
            MeetingOutcome::Skipped(reason) => report.skipped.push(Skipped { target: id, reason }),
            MeetingOutcome::Failed { freed_bytes, files } => {
                report.freed_bytes += freed_bytes;
                report.failed.push(Failed { target: id, files });
            }
        }
    }

    if !request.orphan_folders.is_empty() {
        // Fresh: a meeting saved since the start of this purge must not look like an orphan.
        let index = FolderIndex::load(pool, roots).await?;
        let activity = capture_activity(pool).await?;
        let requested = request.orphan_folders;
        let (freed, purged, skipped, failed) = tokio::task::spawn_blocking(move || {
            let fresh = orphans::find_orphans(&index, &activity, now);
            let mut freed = 0u64;
            let mut purged = 0usize;
            let mut skipped = Vec::new();
            let mut failed = Vec::new();
            let mut done = Vec::<FolderKey>::new();
            for requested_path in requested {
                let key = FolderKey::of(&requested_path);
                if done.contains(&key) {
                    continue;
                }
                done.push(key.clone());
                let Some(orphan) = fresh.iter().find(|o| FolderKey::of(&o.folder) == key) else {
                    skipped.push(Skipped { target: requested_path, reason: HoldReason::Vanished });
                    continue;
                };
                if let Some(reason) = orphan.hold {
                    skipped.push(Skipped { target: requested_path, reason });
                    continue;
                }
                match remove_meeting_folder(&orphan.folder) {
                    FolderRemoval::Removed { freed_bytes } | FolderRemoval::KeptForeignFiles { freed_bytes } => {
                        freed += freed_bytes;
                        purged += 1;
                    }
                    FolderRemoval::Failed { freed_bytes, failed: files } => {
                        freed += freed_bytes;
                        failed.push(Failed { target: requested_path, files });
                    }
                }
            }
            (freed, purged, skipped, failed)
        })
        .await?;
        report.freed_bytes += freed;
        report.purged_orphans = purged;
        report.skipped.extend(skipped);
        report.failed.extend(failed);
    }

    log::info!(
        "[recordings] purge: {} meetings and {} orphan folders purged, {} skipped, {} failed, {} bytes freed",
        report.purged_meetings.len(),
        report.purged_orphans,
        report.skipped.len(),
        report.failed.len(),
        report.freed_bytes
    );
    Ok(report)
}

/// Deletes the meeting's rows exactly as before, then its folder when the
/// folder is safely and only this meeting's. Rows go first: a crash after that
/// leaves an orphan the survey lists, never a row whose audio vanished.
pub(crate) async fn delete_meeting(
    pool: &SqlitePool,
    roots: Vec<PathBuf>,
    meeting_id: &str,
) -> anyhow::Result<DeleteMeetingReport> {
    let reservation = crate::audio::retranscription::reserve_meeting_audio(meeting_id);
    let facts = MeetingsRepository::audio_facts(pool, Some(meeting_id)).await?.into_iter().next();
    let index = FolderIndex::load(pool, roots).await?;
    let activity = capture_activity(pool).await?;

    if !MeetingsRepository::delete_meeting(pool, meeting_id).await? {
        anyhow::bail!("Meeting not found or could not be deleted: {}", meeting_id);
    }
    sqlx::query("DELETE FROM background_jobs WHERE meeting_id = ? AND interrupted = 1")
        .bind(meeting_id)
        .execute(pool)
        .await?;

    let Some(folder) = facts.as_ref().and_then(|f| f.folder()).map(PathBuf::from) else {
        return Ok(DeleteMeetingReport::None);
    };
    let key = FolderKey::of(&folder);
    if reservation.is_none() || activity.recording_holds(&key) || activity.journaled_folders.iter().any(|j| j.overlaps(&key)) {
        log::info!("[recordings] meeting {} deleted, folder kept because it is in use", meeting_id);
        return Ok(DeleteMeetingReport::KeptBusy);
    }
    if index.shared_or_nested(meeting_id, &key) {
        return Ok(DeleteMeetingReport::KeptShared);
    }

    let report = tokio::task::spawn_blocking(move || {
        let _reservation = reservation;
        if std::fs::symlink_metadata(&folder).is_err() {
            return DeleteMeetingReport::None;
        }
        let shape = FolderShape::probe(&folder);
        if shape.symlink || !shape.marked || index.is_root_or_above(&key) {
            return DeleteMeetingReport::KeptUnrecognized;
        }
        let path = folder.to_string_lossy().into_owned();
        match remove_meeting_folder(&folder) {
            FolderRemoval::Removed { freed_bytes } => DeleteMeetingReport::Removed { freed_bytes },
            FolderRemoval::KeptForeignFiles { freed_bytes } => DeleteMeetingReport::KeptForeignFiles { freed_bytes, path },
            FolderRemoval::Failed { .. } => DeleteMeetingReport::Failed { path },
        }
    })
    .await?;
    log::info!("[recordings] meeting {} deleted, folder outcome: {}", meeting_id, report.label());
    Ok(report)
}

async fn configured_roots<R: Runtime>(app: &AppHandle<R>) -> Vec<PathBuf> {
    let save_folder = crate::audio::recording_preferences::load_recording_preferences(app)
        .await
        .ok()
        .map(|p| p.save_folder);
    orphans::configured_roots(save_folder.as_deref())
}

pub(crate) async fn delete_meeting_command<R: Runtime>(
    app: &AppHandle<R>,
    pool: &SqlitePool,
    meeting_id: &str,
) -> anyhow::Result<DeleteMeetingReport> {
    delete_meeting(pool, configured_roots(app).await, meeting_id).await
}

#[tauri::command]
pub async fn recordings_survey<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
) -> Result<Survey, String> {
    let roots = configured_roots(&app).await;
    survey(state.db_manager.pool(), roots).await.map_err(|e| {
        log::error!("[recordings] survey failed: {}", e);
        "Could not read how much space recordings use.".to_string()
    })
}

#[tauri::command]
pub async fn recordings_audio_status<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    meeting_id: String,
) -> Result<AudioStatus, String> {
    let roots = configured_roots(&app).await;
    status(state.db_manager.pool(), roots, &meeting_id).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn recordings_purge<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    request: PurgeRequest,
) -> Result<PurgeReport, String> {
    let roots = configured_roots(&app).await;
    let requested: std::collections::HashSet<String> = request.meeting_ids.iter().cloned().collect();
    let report = purge(state.db_manager.pool(), roots, request).await.map_err(|e| {
        log::error!("[recordings] purge failed: {}", e);
        "Removing audio stopped with an error. Open Storage again to see what is left.".to_string()
    })?;
    let touched = report
        .purged_meetings
        .iter()
        .chain(report.failed.iter().map(|f| &f.target).filter(|t| requested.contains(*t)));
    for id in touched {
        crate::api::emit_meetings_changed(&app, id);
    }
    Ok(report)
}

#[cfg(test)]
mod tests;
