//! Scheduled SharePoint scan: find recordings newer than the watermark and
//! import them, without ever interrupting the user for a sign-in.

use crate::audio::sharepoint::{is_auth_required_error, AuthMode};
use crate::audio::sharepoint_live_match::{attach_recording, find_recording_target, RecordingTarget};
use crate::audio::sharepoint_sync::{
    load_sync_state_public, mark_imported_public, meeting_title_for, scan_recordings,
    set_last_sync_date, SharePointRecording, SharePointScanItem,
};
use crate::pipeline::settings::{self, PipelineRunState, PipelineSettings};
use crate::pipeline::transcribe_stage;
use crate::pipeline::{handle, idle};
use chrono::Utc;
use sqlx::SqlitePool;
use std::collections::HashMap;
use tauri::{AppHandle, Manager};

/// Newest-first is how the scan returns items; import oldest-first so a long
/// backlog arrives in chronological order.
const MAX_IMPORTS_PER_SCAN: usize = 5;

pub struct ScanError {
    pub message: String,
    /// The SharePoint session expired and needs an interactive sign-in.
    pub auth_required: bool,
}

impl ScanError {
    fn from_message(message: String) -> Self {
        let auth_required = is_auth_required_error(&message);
        Self {
            message,
            auth_required,
        }
    }
}

/// Put the pipeline into the "needs sign-in" state and tell the user once.
pub async fn enter_auth_required(app: &AppHandle, detail: &str) {
    let Some(pipeline) = handle() else { return };

    // Already waiting on the user: do not notify again.
    if matches!(pipeline.run_state().await, PipelineRunState::AuthRequired { .. }) {
        return;
    }

    let host = detail
        .rsplit(' ')
        .next()
        .unwrap_or("SharePoint")
        .to_string();
    let state = PipelineRunState::AuthRequired {
        host: host.clone(),
        since: Utc::now().to_rfc3339(),
    };

    log::info!("[pipeline] SharePoint sign-in required; pausing imports");
    pipeline.set_run_state(state.clone()).await;
    settings::save_run_state(app, &state).await;
    notify_auth_required(app, &host).await;
}

async fn notify_auth_required(app: &AppHandle, host: &str) {
    use crate::notifications::types::{
        Notification, NotificationPriority, NotificationTimeout, NotificationType,
    };

    let manager_state = app.state::<crate::NotificationManagerState<tauri::Wry>>();
    let manager = manager_state.read().await;
    let Some(manager) = manager.as_ref() else {
        return;
    };

    let notification = Notification::new(
        "Meetily — SharePoint sign-in needed",
        format!("Open Meetily to sign in to {host} and resume importing meetings."),
        NotificationType::PipelineAuthRequired,
    )
    .with_priority(NotificationPriority::High)
    .with_timeout(NotificationTimeout::Never);

    if let Err(e) = manager.show_notification(notification).await {
        log::warn!("[pipeline] failed to show sign-in notification: {}", e);
    }
}

/// Clear the auth-required state after a successful interactive sign-in.
/// Generic so both the command layer and the tray handler can call it.
pub async fn clear_auth_required<R: tauri::Runtime>(app: &AppHandle<R>) {
    let Some(pipeline) = handle() else { return };
    if matches!(pipeline.run_state().await, PipelineRunState::AuthRequired { .. }) {
        pipeline.set_run_state(PipelineRunState::Running).await;
        settings::save_run_state(app, &PipelineRunState::Running).await;
        pipeline.wake();
    }
}

/// Whether importing may start now. A recording in progress stops it whatever
/// the provider, because the meeting being recorded has no row to match a new
/// file against until it is saved. Imports also transcribe as part of the import,
/// so a local transcription provider makes them subject to the same idle rule as
/// the transcribe stage.
async fn imports_allowed(pool: &SqlitePool, config: &PipelineSettings) -> bool {
    if crate::audio::recording_commands::is_recording().await {
        return false;
    }
    let transcript_config = transcribe_stage::load_transcript_config(pool).await;
    transcript_config.is_remote() || idle::is_idle_for(config.idle_minutes)
}

/// Scan SharePoint, link what belongs to a live recording and import the rest.
/// Returns how many recordings were imported.
pub async fn run_scan_stage(
    app: &AppHandle,
    pool: &SqlitePool,
    config: &PipelineSettings,
) -> Result<usize, ScanError> {
    let sync_state = load_sync_state_public(app);
    let Some(hub_url) = sync_state.hub_url.filter(|u| !u.trim().is_empty()) else {
        // Never configured; nothing to do (not an error).
        return Ok(0);
    };
    let last_sync = sync_state.last_sync_date;
    let since_iso = last_sync
        .clone()
        .unwrap_or_else(|| (Utc::now() - chrono::Duration::days(30)).to_rfc3339());

    log::debug!("[pipeline] scanning SharePoint since {}", since_iso);
    let result = scan_recordings(app, &hub_url, &since_iso, AuthMode::SilentOnly)
        .await
        .map_err(ScanError::from_message)?;

    for file_url in held_but_not_in_ledger(&result.recordings, &sync_state.imported) {
        mark_imported_public(app, file_url);
    }

    let new_items: Vec<SharePointScanItem> = result
        .recordings
        .into_iter()
        .filter(|item| !item.already_imported)
        .collect();

    if new_items.is_empty() {
        return Ok(0);
    }

    if !imports_allowed(pool, config).await {
        log::debug!(
            "[pipeline] {} new recording(s) found; deferring import until the machine is idle",
            new_items.len()
        );
        return Ok(0);
    }

    let mut oldest_first: Vec<SharePointScanItem> = new_items;
    oldest_first.reverse();

    let transcript_config = transcribe_stage::load_transcript_config(pool).await;
    let outcome = process_new_recordings(
        pool,
        oldest_first,
        |file_url, meeting_id| {
            mark_imported_public(app, file_url);
            crate::api::emit_meetings_changed(app, meeting_id);
        },
        |recording| {
            let transcript_config = &transcript_config;
            async move {
                crate::audio::import::import_from_url_internal(
                    app.clone(),
                    recording.file_url.clone(),
                    // Same title the manual import dialog would produce.
                    meeting_title_for(&recording.name),
                    None,
                    transcript_config.model.clone(),
                    transcript_config.provider.clone(),
                    Some("audio".to_string()),
                    Some(recording.created.clone()).filter(|c| !c.is_empty()),
                    AuthMode::SilentOnly,
                )
                .await?;
                // Marked only after the import actually landed, so a failed
                // download is retried on the next scan.
                mark_imported_public(app, &recording.file_url);
                crate::pipeline::wake();
                Ok(())
            }
        },
    )
    .await?;

    // Advance the watermark only past recordings that actually landed; the
    // ledger stays authoritative for individual files either way.
    if let Some(newest) = next_watermark(&outcome.attempts, last_sync.as_deref()) {
        set_last_sync_date(app, &newest);
    }

    Ok(outcome.imported)
}

/// Files a live meeting holds, usually through a recap, that the ledger does not
/// list. Writing them there means deleting that meeting later does not bring the
/// recording back.
fn held_but_not_in_ledger<'a>(
    recordings: &'a [SharePointScanItem],
    ledger: &HashMap<String, String>,
) -> Vec<&'a str> {
    recordings
        .iter()
        .filter(|item| item.already_imported && item.live_meeting_id.is_some())
        .map(|item| item.recording.file_url.as_str())
        .filter(|file_url| !ledger.contains_key(*file_url))
        .collect()
}

/// What one scan did with its new recordings.
#[derive(Debug, Default, PartialEq)]
struct ScanOutcome {
    imported: usize,
    /// `(created, landed)` per attempted item, in processing order.
    attempts: Vec<(String, bool)>,
}

/// Link each recording of a meeting Meetily recorded live and import the rest,
/// oldest first. Imports are bounded so one scan cannot monopolise the loop, and
/// the rest are picked up by the next scan. Linking is cheap and uses no slot.
/// `on_linked` gets the file URL and the meeting; `import` does the whole import.
async fn process_new_recordings<Import>(
    pool: &SqlitePool,
    oldest_first: Vec<SharePointScanItem>,
    on_linked: impl Fn(&str, &str),
    import: impl Fn(SharePointRecording) -> Import,
) -> Result<ScanOutcome, ScanError>
where
    Import: std::future::Future<Output = anyhow::Result<()>>,
{
    let mut outcome = ScanOutcome::default();
    let mut imports_attempted = 0usize;

    for item in oldest_first {
        let SharePointRecording { file_url, created, name, .. } = item.recording.clone();

        // Decided here, not from the scan's annotation: a recording earlier in
        // this loop may have claimed the meeting. If the database cannot answer,
        // retry next scan rather than risk a second meeting.
        let overlapping = match find_recording_target(pool, &file_url, &created).await {
            Ok(RecordingTarget::Held(meeting_id)) => {
                log::info!("[pipeline] live meeting {} already holds this recording; not importing it", meeting_id);
                on_linked(&file_url, &meeting_id);
                outcome.attempts.push((created, true));
                continue;
            }
            Ok(RecordingTarget::Live(meeting_id)) => {
                let linked = attach_recording(pool, &meeting_id, &file_url).await;
                match &linked {
                    Ok(()) => {
                        log::info!("[pipeline] linked a recording to live meeting {}; not importing it", meeting_id);
                        on_linked(&file_url, &meeting_id);
                    }
                    Err(e) => log::warn!("[pipeline] could not link a recording to live meeting {}: {}", meeting_id, e),
                }
                outcome.attempts.push((created, linked.is_ok()));
                continue;
            }
            Ok(RecordingTarget::New { overlapping }) => overlapping,
            Err(e) => {
                log::warn!("[pipeline] could not match a recording to a live meeting: {}", e);
                outcome.attempts.push((created, false));
                continue;
            }
        };

        if imports_attempted == MAX_IMPORTS_PER_SCAN {
            log::info!(
                "[pipeline] reached the limit of {} imports; the rest wait for the next scan",
                MAX_IMPORTS_PER_SCAN
            );
            break;
        }
        imports_attempted += 1;

        if !overlapping.is_empty() {
            log::info!(
                "[pipeline] importing a recording that may duplicate live meetings {:?}: they fit its time, but nothing confirmed one",
                overlapping
            );
        }
        let title = meeting_title_for(&name);
        log::info!("[pipeline] importing recording \"{}\"", title);

        match import(item.recording).await {
            Ok(()) => {
                outcome.imported += 1;
                outcome.attempts.push((created, true));
            }
            Err(e) => {
                let message = e.to_string();
                if is_auth_required_error(&message) {
                    return Err(ScanError::from_message(message));
                }
                outcome.attempts.push((created, false));
                log::warn!("[pipeline] import of \"{}\" failed: {}", title, message);
            }
        }
    }

    Ok(outcome)
}

/// New `last_sync_date` after a scan, or None to leave it unchanged. `outcomes`
/// is `(created, landed)` for each attempted item, oldest first.
///
/// The watermark is a "created after" filter, so it may only pass the leading
/// run of successes: moving it beyond a failed recording would hide that
/// recording from every later scan. A recording linked to a live meeting is a
/// success: it has nothing left to do.
fn next_watermark(outcomes: &[(String, bool)], current: Option<&str>) -> Option<String> {
    let candidate = outcomes
        .iter()
        .take_while(|(_, ok)| *ok)
        .map(|(created, _)| created.as_str())
        .filter(|created| !created.is_empty())
        .last()?;
    match current {
        Some(current) if candidate <= current => None,
        _ => Some(candidate.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcomes(items: &[(&str, bool)]) -> Vec<(String, bool)> {
        items.iter().map(|(c, ok)| (c.to_string(), *ok)).collect()
    }

    const A: &str = "2026-07-20T09:00:00Z";
    const B: &str = "2026-07-21T09:00:00Z";
    const C: &str = "2026-07-22T09:00:00Z";

    #[test]
    fn watermark_advances_to_newest_when_every_import_succeeds() {
        let o = outcomes(&[(A, true), (B, true)]);
        assert_eq!(next_watermark(&o, None), Some(B.to_string()));
    }

    #[test]
    fn watermark_does_not_move_when_the_oldest_import_failed() {
        let o = outcomes(&[(A, false), (B, true)]);
        assert_eq!(next_watermark(&o, None), None);
        assert_eq!(next_watermark(&o, Some("2026-07-01T00:00:00Z")), None);
    }

    #[test]
    fn watermark_stops_at_the_last_success_before_a_failure() {
        let o = outcomes(&[(A, true), (B, false), (C, true)]);
        assert_eq!(next_watermark(&o, None), Some(A.to_string()));
    }

    mod processing {
        use super::*;
        use crate::calendar::types::SourceKind;
        use crate::calendar::CalendarEvent;
        use crate::database::repositories::{
            meeting_calendar::MeetingCalendarRepository,
            meeting_sources::{MeetingSource, LIVE_RECORDING_SOURCE},
        };
        use chrono::TimeZone;
        use std::cell::RefCell;

        async fn test_pool() -> SqlitePool {
            let pool = sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect("sqlite::memory:")
                .await
                .expect("in-memory sqlite pool");
            sqlx::migrate!("./migrations").run(&pool).await.expect("migrations apply");
            pool
        }

        /// Recorded live against a 10:00 to 10:30 UTC event.
        async fn live_meeting(pool: &SqlitePool, id: &str, subject: &str) {
            sqlx::query(
                "INSERT INTO meetings (id, title, created_at, updated_at) \
                 VALUES (?, 'Generated heading', '2026-11-03T16:30:00Z', '2026-11-03T16:30:00Z')",
            )
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO transcripts (id, meeting_id, transcript, timestamp) \
                 VALUES (?, ?, 'hello', '2026-11-03T10:00:00Z')",
            )
            .bind(format!("t-{id}"))
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
            let event = CalendarEvent {
                id: format!("event-{id}"),
                source: SourceKind::OutlookCom,
                title: subject.to_owned(),
                start: Utc.with_ymd_and_hms(2026, 11, 3, 10, 0, 0).unwrap(),
                end: Utc.with_ymd_and_hms(2026, 11, 3, 10, 30, 0).unwrap(),
                organizer: None,
                attendees: vec![],
                location: None,
                body: None,
                teams_join_url: None,
                is_recurring_instance: false,
            };
            MeetingCalendarRepository::attach(pool, id, &event).await.unwrap();
        }

        /// A Teams recording of `subject`, created at `minute` past 10:00 UTC.
        fn item(subject: &str, minute: u32) -> SharePointScanItem {
            let name = format!("{subject}-20261103_10{minute:02}00-Meeting Recording.mp4");
            SharePointScanItem {
                recording: SharePointRecording {
                    file_url: format!("https://c-my.sharepoint.com/r/{}", name.replace(' ', "%20")),
                    stream_url: String::new(),
                    created: format!("2026-11-03T10:{minute:02}:00Z"),
                    size_bytes: None,
                    name,
                },
                already_imported: false,
                source: "mine".to_owned(),
                live_meeting_id: None,
            }
        }

        fn created_of(items: &[&SharePointScanItem]) -> Vec<String> {
            items.iter().map(|item| item.recording.created.clone()).collect()
        }

        /// Runs the loop with an import that succeeds unless it is told the name
        /// to fail on, and returns the outcome with what it imported and linked.
        async fn run(
            pool: &SqlitePool,
            items: Vec<SharePointScanItem>,
            fail_on: Option<(&str, &str)>,
        ) -> (Result<ScanOutcome, ScanError>, Vec<String>, Vec<(String, String)>) {
            let imported = RefCell::new(Vec::new());
            let linked = RefCell::new(Vec::new());
            let outcome = process_new_recordings(
                pool,
                items,
                |file_url, meeting_id| linked.borrow_mut().push((file_url.to_owned(), meeting_id.to_owned())),
                |recording| {
                    imported.borrow_mut().push(recording.name.clone());
                    let failure = fail_on.filter(|(name, _)| recording.name.starts_with(name)).map(|(_, error)| error.to_owned());
                    async move {
                        match failure {
                            Some(error) => Err(anyhow::anyhow!(error)),
                            None => Ok(()),
                        }
                    }
                },
            )
            .await;
            (outcome, imported.into_inner(), linked.into_inner())
        }

        #[test]
        fn a_file_held_by_a_live_meeting_goes_into_the_ledger_once() {
            let held = SharePointScanItem {
                already_imported: true,
                live_meeting_id: Some("live".to_owned()),
                ..item("Held", 1)
            };
            let in_ledger = SharePointScanItem {
                already_imported: true,
                live_meeting_id: Some("live".to_owned()),
                ..item("Listed", 2)
            };
            let imported = SharePointScanItem { already_imported: true, ..item("Imported", 3) };
            let matched_not_held = SharePointScanItem { live_meeting_id: Some("live".to_owned()), ..item("Matched", 4) };
            let held_url = held.recording.file_url.clone();
            let ledger = HashMap::from([(in_ledger.recording.file_url.clone(), "2026-11-03T10:00:00Z".to_owned())]);

            let items = [held, in_ledger, imported, matched_not_held, item("New", 5)];
            assert_eq!(held_but_not_in_ledger(&items, &ledger), vec![held_url.as_str()]);
        }

        #[tokio::test]
        async fn a_recording_of_a_live_meeting_is_linked_and_the_others_imported() {
            let pool = test_pool().await;
            live_meeting(&pool, "live", "Weekly sync").await;
            let weekly = item("Weekly sync", 2);
            let other = item("Vendor call", 3);
            let file = weekly.recording.file_url.clone();
            let created = created_of(&[&weekly, &other]);

            let (outcome, imported, linked) = run(&pool, vec![weekly, other], None).await;

            let outcome = outcome.ok().unwrap();
            assert_eq!(imported, vec!["Vendor call-20261103_100300-Meeting Recording.mp4"]);
            assert_eq!(linked, vec![(file.clone(), "live".to_owned())]);
            assert_eq!(outcome.imported, 1, "a linked recording is not an import");
            assert_eq!(outcome.attempts, vec![(created[0].clone(), true), (created[1].clone(), true)]);
            assert_eq!(next_watermark(&outcome.attempts, None), Some(created[1].clone()));

            let source = MeetingSource::get(&pool, "live").await.unwrap().unwrap();
            assert_eq!(source.import_source, LIVE_RECORDING_SOURCE);
            assert_eq!(source.recording_url.as_deref(), Some(file.as_str()));
        }

        #[tokio::test]
        async fn an_ambiguous_recording_imports_as_before() {
            let pool = test_pool().await;
            live_meeting(&pool, "other", "Budget review").await;

            let (outcome, imported, linked) = run(&pool, vec![item("Weekly sync", 2)], None).await;

            assert_eq!(outcome.ok().unwrap().imported, 1);
            assert_eq!(imported.len(), 1);
            assert!(linked.is_empty());
            assert_eq!(MeetingSource::get(&pool, "other").await.unwrap(), None);
        }

        #[tokio::test]
        async fn two_files_in_one_scan_cannot_both_take_the_meeting() {
            let pool = test_pool().await;
            live_meeting(&pool, "live", "Weekly sync").await;
            let first = item("Weekly sync", 2);
            let second = item("Weekly sync", 20);
            let first_url = first.recording.file_url.clone();

            let (outcome, imported, linked) = run(&pool, vec![first, second], None).await;

            assert_eq!(outcome.ok().unwrap().imported, 1);
            assert_eq!(imported, vec!["Weekly sync-20261103_102000-Meeting Recording.mp4"]);
            assert_eq!(linked, vec![(first_url, "live".to_owned())]);
        }

        #[tokio::test]
        async fn both_parts_of_a_long_recording_in_one_scan_leave_one_meeting() {
            let pool = test_pool().await;
            live_meeting(&pool, "live", "Weekly sync").await;
            let first = item("Weekly sync", 2);
            let mut second = item("Weekly sync", 2);
            second.recording.name = second.recording.name.replace("Recording.mp4", "Recording 2.mp4");
            second.recording.file_url = second.recording.file_url.replace("Recording.mp4", "Recording%202.mp4");
            let urls = [first.recording.file_url.clone(), second.recording.file_url.clone()];

            let (outcome, imported, linked) = run(&pool, vec![first, second], None).await;

            assert!(imported.is_empty(), "the live transcript covers every part: {imported:?}");
            let outcome = outcome.ok().unwrap();
            assert_eq!(outcome.attempts.iter().filter(|(_, landed)| *landed).count(), 2);
            // Both go into the ledger, and the row keeps the first.
            assert_eq!(
                linked,
                vec![(urls[0].clone(), "live".to_owned()), (urls[1].clone(), "live".to_owned())]
            );
            let source = MeetingSource::get(&pool, "live").await.unwrap().unwrap();
            assert_eq!(source.recording_url.as_deref(), Some(urls[0].as_str()));
        }

        #[tokio::test]
        async fn linking_uses_no_import_slot() {
            let pool = test_pool().await;
            live_meeting(&pool, "live", "Weekly sync").await;
            let mut items = vec![item("Weekly sync", 0)];
            items.extend((1..=MAX_IMPORTS_PER_SCAN as u32 + 1).map(|minute| item(&format!("Import {minute}"), minute)));

            let (outcome, imported, linked) = run(&pool, items, None).await;

            let outcome = outcome.ok().unwrap();
            assert_eq!(linked.len(), 1);
            assert_eq!(imported.len(), MAX_IMPORTS_PER_SCAN);
            assert_eq!(outcome.imported, MAX_IMPORTS_PER_SCAN);
            assert_eq!(outcome.attempts.len(), MAX_IMPORTS_PER_SCAN + 1, "the one past the limit is not attempted");
        }

        #[tokio::test]
        async fn when_the_database_cannot_answer_nothing_is_imported_and_the_scan_retries() {
            let pool = test_pool().await;
            pool.close().await;
            let weekly = item("Weekly sync", 2);
            let created = weekly.recording.created.clone();

            let (outcome, imported, linked) = run(&pool, vec![weekly], None).await;

            assert_eq!(outcome.ok().unwrap().attempts, vec![(created, false)]);
            assert!(imported.is_empty());
            assert!(linked.is_empty());
        }

        #[tokio::test]
        async fn a_failed_import_holds_the_watermark_and_an_expired_session_stops_the_scan() {
            let pool = test_pool().await;
            let (outcome, imported, _) = run(
                &pool,
                vec![item("Fails", 2), item("Works", 3)],
                Some(("Fails", "download failed")),
            )
            .await;
            let outcome = outcome.ok().unwrap();
            assert_eq!(imported.len(), 2);
            assert_eq!(outcome.imported, 1);
            assert_eq!(next_watermark(&outcome.attempts, None), None);

            let auth_error = format!(
                "{}: SharePoint sign-in required for contoso.sharepoint.com",
                crate::audio::sharepoint::AUTH_REQUIRED_MARKER
            );
            let (outcome, imported, _) = run(
                &pool,
                vec![item("Fails", 2), item("Works", 3)],
                Some(("Fails", auth_error.as_str())),
            )
            .await;
            assert!(outcome.err().is_some_and(|e| e.auth_required));
            assert_eq!(imported.len(), 1);
        }
    }
}
