//! Pull every Copilot recap the Teams Meet app lists into Meetily and the
//! Obsidian vault, one button press at a time. Each recap is keyed on the
//! recording it belongs to (see `teams_recap::meeting_identity_key`), so a
//! recap never creates a second meeting or a second note, and a content hash
//! per key turns an unchanged recap into a skip instead of a rewrite.
//!
//! The list itself comes from `teams_recap_list`, which reads the Meet app's
//! recap tab in a hidden WebView2 window. Rows it lists without a recap link
//! count as not ready. See docs/TEAMS_RECAP_SYNC_SPEC.md.

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use log::{info, warn};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use tauri::{AppHandle, Emitter, Manager, Runtime};
use tauri_plugin_store::StoreExt;

const SYNC_STORE_FILE: &str = "teams_recap_sync.json";
const SYNC_STORE_KEY: &str = "state";
/// How far back the first sync reaches when the user has not chosen a date.
const DEFAULT_BACKFILL_DAYS: i64 = 30;
/// A recap appears hours after a meeting and Teams may finish generating one
/// days later, so each sync re-reads this much of the previous window.
const OVERLAP_DAYS: i64 = 7;
pub const PROGRESS_EVENT: &str = "teams-recap-sync-progress";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportedRecap {
    pub meeting_id: String,
    /// RFC 3339.
    pub imported_at: String,
    /// SHA-256 of the recap markdown last written, hex.
    pub content_hash: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamsRecapSyncState {
    /// `YYYY-MM-DD` the first sync starts from. Unset means
    /// [`DEFAULT_BACKFILL_DAYS`] before the first sync.
    #[serde(default)]
    pub since_date: Option<String>,
    /// RFC 3339 start time of the last completed sync.
    #[serde(default)]
    pub last_sync_date: Option<String>,
    /// Identity key -> what was written for it.
    #[serde(default)]
    pub imported: HashMap<String, ImportedRecap>,
}

/// What the settings panel shows.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamsRecapSyncStateView {
    pub since_date: Option<String>,
    pub last_sync_date: Option<String>,
    pub known_recaps: usize,
    /// The Meet app can be read on this platform.
    pub available: bool,
}

/// One meeting row the Meet app lists.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecapCandidate {
    pub recap_url: String,
    pub title: String,
    /// When the meeting started, as the list shows it (RFC 3339 when known).
    pub started_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecapSyncFailure {
    pub title: String,
    pub error: String,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecapSyncReport {
    pub scanned: usize,
    pub new: usize,
    pub updated: usize,
    pub skipped: usize,
    pub not_ready: usize,
    pub failed: Vec<RecapSyncFailure>,
    /// Titles of recaps imported as their own meeting although a live
    /// recording overlaps them in time without being confirmed as the same.
    pub possible_duplicates: Vec<String>,
    pub meetings_folder: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SyncProgress<'a> {
    done: usize,
    total: usize,
    current_title: &'a str,
}

#[derive(Debug, PartialEq)]
enum Outcome {
    New { possible_duplicate: bool },
    Updated,
    Skipped,
    NotReady,
    Failed(String),
}

// ---------------------------------------------------------------------------
// State store
// ---------------------------------------------------------------------------

fn load_state<R: Runtime>(app: &AppHandle<R>) -> TeamsRecapSyncState {
    let Ok(store) = app.store(SYNC_STORE_FILE) else {
        return TeamsRecapSyncState::default();
    };
    store
        .get(SYNC_STORE_KEY)
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default()
}

/// Load-modify-save under one lock: the sync loop and the settings panel
/// both write, and each save rewrites the whole record.
static STATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn update_state<R: Runtime>(
    app: &AppHandle<R>,
    modify: impl FnOnce(&mut TeamsRecapSyncState),
) -> Result<()> {
    let _guard = STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut state = load_state(app);
    modify(&mut state);
    let store = app
        .store(SYNC_STORE_FILE)
        .map_err(|e| anyhow!("Could not open the recap sync store: {e}"))?;
    store.set(
        SYNC_STORE_KEY,
        serde_json::to_value(&state).context("Could not serialize recap sync state")?,
    );
    store
        .save()
        .map_err(|e| anyhow!("Could not persist the recap sync store: {e}"))
}

fn parse_since_date(value: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(value.trim(), "%Y-%m-%d")
        .map_err(|_| anyhow!("Enter the since date as YYYY-MM-DD."))
}

/// Where this sync starts reading: the previous sync's start minus the
/// overlap, else the chosen since date, else the default backfill.
fn scan_window_start(state: &TeamsRecapSyncState, now: DateTime<Utc>) -> DateTime<Utc> {
    if let Some(last) = state
        .last_sync_date
        .as_deref()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
    {
        return last.with_timezone(&Utc) - Duration::days(OVERLAP_DAYS);
    }
    if let Some(date) = state.since_date.as_deref().and_then(|value| parse_since_date(value).ok()) {
        return date.and_hms_opt(0, 0, 0).expect("midnight exists").and_utc();
    }
    now - Duration::days(DEFAULT_BACKFILL_DAYS)
}

fn content_hash(markdown: &str) -> String {
    let digest = Sha256::digest(markdown.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Whether an extraction error means "no AI summary yet" rather than a
/// broken session or layout. Such a recap stays unrecorded and is retried
/// on the next sync.
fn is_not_ready(error: &str) -> bool {
    error.contains("did not show an AI summary")
}

// ---------------------------------------------------------------------------
// Listing
// ---------------------------------------------------------------------------

/// The Meet app can only be read through the Windows WebView2 profile.
pub const fn sync_available() -> bool {
    cfg!(target_os = "windows")
}

/// The Meet app's rows for the window, or the error the listing hit.
async fn list_recaps<R: Runtime>(
    app: &AppHandle<R>,
    since: DateTime<Utc>,
) -> Result<super::teams_recap_list::RecapListing> {
    let listing = super::teams_recap_list::list_recaps(app, since, false).await?;
    if let Some(error) = &listing.error {
        return Err(anyhow!("{error}"));
    }
    Ok(listing)
}

// ---------------------------------------------------------------------------
// Sync loop
// ---------------------------------------------------------------------------

static SYNC_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn import_candidate<R: Runtime>(
    app: &AppHandle<R>,
    pool: &sqlx::SqlitePool,
    known: &HashMap<String, ImportedRecap>,
    candidate: &RecapCandidate,
) -> Outcome {
    let validated = match crate::audio::teams_recap::validate_recap_url(&candidate.recap_url) {
        Ok(url) => url,
        Err(e) => return Outcome::Failed(e.to_string()),
    };
    let source_url = validated.as_str();
    let key = crate::audio::teams_recap::meeting_identity_key(source_url);
    let recording = crate::audio::teams_recap::recording_metadata(&validated);

    let recap = match crate::audio::teams_recap::extract_teams_recap(app, source_url).await {
        Ok(recap) => recap,
        Err(e) if is_not_ready(&e.to_string()) => return Outcome::NotReady,
        Err(e) => return Outcome::Failed(e.to_string()),
    };
    let markdown = crate::summary::commands::recap_markdown_with_source(&recap.markdown, source_url);
    let hash = content_hash(&markdown);
    let seen_before = known.get(&key);
    if seen_before.is_some_and(|entry| entry.content_hash == hash) {
        return Outcome::Skipped;
    }

    let persisted = match crate::summary::commands::persist_recap_from_link(
        app,
        pool,
        candidate.title.clone(),
        source_url,
        recording.as_ref().map(|metadata| metadata.recorded_at.as_str()),
        candidate.started_at.as_deref(),
        &markdown,
    )
    .await
    {
        Ok(persisted) => persisted,
        Err(e) => return Outcome::Failed(e),
    };

    if let Err(e) = update_state(app, |state| {
        state.imported.insert(
            key,
            ImportedRecap {
                meeting_id: persisted.meeting_id.clone(),
                imported_at: Utc::now().to_rfc3339(),
                content_hash: hash,
            },
        );
    }) {
        warn!("[recap-sync] could not record recap import: {e}");
    }
    written_outcome(seen_before.is_some(), &persisted)
}

/// How a written recap counts: "updated" when it replaced something, whether an
/// earlier copy of itself or the meeting's own summary.
fn written_outcome(
    seen_before: bool,
    persisted: &crate::summary::commands::ImportedRecapFromLinkResponse,
) -> Outcome {
    if seen_before || persisted.replaced_summary {
        Outcome::Updated
    } else {
        Outcome::New { possible_duplicate: persisted.possible_duplicate }
    }
}

impl RecapSyncReport {
    fn tally(&mut self, title: &str, outcome: Outcome) {
        match outcome {
            Outcome::New { possible_duplicate } => {
                self.new += 1;
                if possible_duplicate {
                    self.possible_duplicates.push(title.to_owned());
                }
            }
            Outcome::Updated => self.updated += 1,
            Outcome::Skipped => self.skipped += 1,
            Outcome::NotReady => self.not_ready += 1,
            Outcome::Failed(error) => {
                warn!("[recap-sync] {title}: {error}");
                self.failed.push(RecapSyncFailure { title: title.to_owned(), error });
            }
        }
    }
}

/// Run one sync: list recaps from the scan window, import each, record the
/// window. Re-running converges: every imported recap is in the store
/// before the next one starts, and unchanged recaps are skipped.
pub async fn sync_recaps<R: Runtime>(app: &AppHandle<R>) -> Result<RecapSyncReport, String> {
    let Ok(_running) = SYNC_LOCK.try_lock() else {
        return Err("A Teams recap sync is already running.".into());
    };
    let vault_path = crate::summary::commands::require_obsidian_vault(app).await?;
    let meetings_folder = crate::obsidian::meetings_path(&vault_path)
        .to_string_lossy()
        .to_string();

    let started_at = Utc::now();
    let state = load_state(app);
    let since = scan_window_start(&state, started_at);
    info!("[recap-sync] listing recaps since {}", since.to_rfc3339());
    let listing = list_recaps(app, since).await.map_err(|e| e.to_string())?;

    let app_state = app.state::<crate::state::AppState>();
    let pool = app_state.db_manager.pool();
    let mut report = RecapSyncReport {
        scanned: listing.candidates.len() + listing.rows_without_link,
        // A listed meeting with no recap link yet is retried next sync.
        not_ready: listing.rows_without_link,
        meetings_folder,
        ..Default::default()
    };
    // Oldest first so a sync interrupted midway leaves a contiguous prefix.
    let mut ordered = listing.candidates;
    ordered.sort_by(|a, b| a.started_at.cmp(&b.started_at));
    let total = ordered.len();
    for (done, candidate) in ordered.iter().enumerate() {
        let _ = app.emit(
            PROGRESS_EVENT,
            SyncProgress { done, total, current_title: &candidate.title },
        );
        let outcome = import_candidate(app, pool, &state.imported, candidate).await;
        report.tally(&candidate.title, outcome);
    }
    let _ = app.emit(PROGRESS_EVENT, SyncProgress { done: total, total, current_title: "" });

    if let Err(e) = update_state(app, |state| state.last_sync_date = Some(started_at.to_rfc3339())) {
        warn!("[recap-sync] could not persist last sync date: {e}");
    }
    info!(
        "[recap-sync] scanned {} new {} updated {} skipped {} not ready {} failed {}",
        report.scanned, report.new, report.updated, report.skipped, report.not_ready, report.failed.len()
    );
    Ok(report)
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn sync_teams_recaps_command<R: Runtime>(app: AppHandle<R>) -> Result<RecapSyncReport, String> {
    sync_recaps(&app).await
}

#[tauri::command]
pub async fn get_teams_recap_sync_state_command<R: Runtime>(
    app: AppHandle<R>,
) -> Result<TeamsRecapSyncStateView, String> {
    let state = load_state(&app);
    Ok(TeamsRecapSyncStateView {
        since_date: state.since_date,
        last_sync_date: state.last_sync_date,
        known_recaps: state.imported.len(),
        available: sync_available(),
    })
}

#[tauri::command]
pub async fn set_teams_recap_sync_prefs_command<R: Runtime>(
    app: AppHandle<R>,
    since_date: Option<String>,
) -> Result<(), String> {
    let since_date = match since_date.as_deref().map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => Some(parse_since_date(value).map_err(|e| e.to_string())?.to_string()),
        None => None,
    };
    update_state(&app, |state| state.since_date = since_date).map_err(|e| e.to_string())
}

/// The raw candidate list, without importing anything. For learning the
/// Meet app layout and for the QA doc.
#[tauri::command]
pub async fn teams_recap_list_debug_command<R: Runtime>(
    app: AppHandle<R>,
) -> Result<RecapListDebug, String> {
    let Ok(_running) = SYNC_LOCK.try_lock() else {
        return Err("A Teams recap sync is already running.".into());
    };
    let state = load_state(&app);
    let since = scan_window_start(&state, Utc::now());
    let listing = super::teams_recap_list::list_recaps(&app, since, true)
        .await
        .map_err(|e| e.to_string())?;
    let probe_file = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Could not resolve app data directory: {e}"))?
        .join(PROBE_FILE);
    let json = serde_json::to_string_pretty(&listing).map_err(|e| e.to_string())?;
    std::fs::write(&probe_file, json)
        .map_err(|e| format!("Could not write {}: {e}", probe_file.display()))?;
    info!(
        "[recap-sync] list check: {} recaps, {} rows without a link, probe saved",
        listing.candidates.len(),
        listing.rows_without_link
    );
    Ok(RecapListDebug {
        candidates: listing.candidates,
        rows_without_link: listing.rows_without_link,
        page_url: listing.page_url,
        error: listing.error,
        probe_file: probe_file.to_string_lossy().to_string(),
    })
}

/// Written next to the app's data by the list check, for reading the Meet
/// app's layout on the machine that ran it. It holds meeting titles and page
/// text, so it stays local.
const PROBE_FILE: &str = "teams_recap_list_probe.json";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecapListDebug {
    pub candidates: Vec<RecapCandidate>,
    pub rows_without_link: usize,
    pub page_url: Option<String>,
    pub error: Option<String>,
    pub probe_file: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn first_sync_defaults_to_thirty_days_back() {
        let now = at("2026-11-02T10:00:00Z");
        assert_eq!(scan_window_start(&TeamsRecapSyncState::default(), now), at("2026-10-03T10:00:00Z"));
    }

    #[test]
    fn chosen_since_date_wins_until_a_sync_has_run() {
        let state = TeamsRecapSyncState { since_date: Some("2026-09-01".into()), ..Default::default() };
        assert_eq!(scan_window_start(&state, at("2026-11-02T10:00:00Z")), at("2026-09-01T00:00:00Z"));
        let bad = TeamsRecapSyncState { since_date: Some("September".into()), ..Default::default() };
        assert_eq!(scan_window_start(&bad, at("2026-11-02T10:00:00Z")), at("2026-10-03T10:00:00Z"));
    }

    #[test]
    fn later_syncs_overlap_the_previous_window() {
        let state = TeamsRecapSyncState {
            since_date: Some("2026-01-01".into()),
            last_sync_date: Some("2026-10-30T08:00:00+00:00".into()),
            ..Default::default()
        };
        assert_eq!(scan_window_start(&state, at("2026-11-02T10:00:00Z")), at("2026-10-23T08:00:00Z"));
    }

    #[test]
    fn since_date_must_be_a_calendar_date() {
        assert_eq!(parse_since_date(" 2026-02-28 ").unwrap().to_string(), "2026-02-28");
        assert!(parse_since_date("2026-02-30").is_err());
        assert!(parse_since_date("28/02/2026").is_err());
    }

    #[test]
    fn unchanged_markdown_hashes_the_same_and_edits_do_not() {
        assert_eq!(content_hash("- a\n"), content_hash("- a\n"));
        assert_ne!(content_hash("- a\n"), content_hash("- b\n"));
        assert_eq!(content_hash("").len(), 64);
    }

    #[test]
    fn only_a_missing_ai_summary_counts_as_not_ready() {
        assert!(is_not_ready("Teams did not show an AI summary for this link. Sign in if prompted."));
        assert!(!is_not_ready("WebView2 is unavailable: boom"));
        assert!(!is_not_ready("Teams stayed on its app-choice page."));
    }

    fn written(replaced_summary: bool, possible_duplicate: bool) -> crate::summary::commands::ImportedRecapFromLinkResponse {
        crate::summary::commands::ImportedRecapFromLinkResponse {
            meeting_id: "m1".into(),
            title: "T".into(),
            obsidian_file_path: "V/Meetings/T.md".into(),
            attached: replaced_summary,
            replaced_summary,
            possible_duplicate,
        }
    }

    #[test]
    fn a_written_recap_counts_as_updated_when_it_replaced_something() {
        // A brand new meeting, flagged or not.
        assert_eq!(written_outcome(false, &written(false, false)), Outcome::New { possible_duplicate: false });
        assert_eq!(written_outcome(false, &written(false, true)), Outcome::New { possible_duplicate: true });
        // An earlier copy of this recap, or a live recording's own summary.
        assert_eq!(written_outcome(true, &written(false, false)), Outcome::Updated);
        assert_eq!(written_outcome(false, &written(true, false)), Outcome::Updated);
    }

    #[test]
    fn the_report_counts_every_outcome_and_names_possible_duplicates() {
        let mut report = RecapSyncReport::default();
        report.tally("A", Outcome::New { possible_duplicate: false });
        report.tally("B", Outcome::New { possible_duplicate: true });
        report.tally("C", Outcome::Updated);
        report.tally("D", Outcome::Skipped);
        report.tally("E", Outcome::NotReady);
        report.tally("F", Outcome::Failed("boom".into()));
        assert_eq!((report.new, report.updated, report.skipped, report.not_ready), (2, 1, 1, 1));
        assert_eq!(report.possible_duplicates, vec!["B".to_string()]);
        assert_eq!(report.failed[0].title, "F");
        assert_eq!(report.failed[0].error, "boom");
    }

    #[test]
    fn state_round_trips_through_json() {
        let mut state = TeamsRecapSyncState::default();
        state.imported.insert(
            "host/path.mp4".into(),
            ImportedRecap { meeting_id: "m1".into(), imported_at: "2026-11-02T10:00:00+00:00".into(), content_hash: "ab".into() },
        );
        let json = serde_json::to_value(&state).unwrap();
        assert_eq!(json["imported"]["host/path.mp4"]["meetingId"], "m1");
        let back: TeamsRecapSyncState = serde_json::from_value(json).unwrap();
        assert_eq!(back.imported["host/path.mp4"].content_hash, "ab");
        let empty: TeamsRecapSyncState = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(empty.imported.is_empty());
    }
}
