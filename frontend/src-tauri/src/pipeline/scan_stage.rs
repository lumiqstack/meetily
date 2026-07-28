//! Scheduled SharePoint scan: find recordings newer than the watermark and
//! import them, without ever interrupting the user for a sign-in.

use crate::audio::sharepoint::{is_auth_required_error, AuthMode};
use crate::audio::sharepoint_sync::{
    load_sync_state_public, mark_imported_public, meeting_title_for, scan_recordings,
    set_last_sync_date, SharePointScanItem,
};
use crate::pipeline::settings::{self, PipelineRunState, PipelineSettings};
use crate::pipeline::transcribe_stage;
use crate::pipeline::{handle, idle};
use chrono::Utc;
use sqlx::SqlitePool;
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

/// Whether importing may start now. Imports transcribe as part of the import,
/// so a local transcription provider makes them subject to the same idle rule
/// as the transcribe stage.
async fn imports_allowed(pool: &SqlitePool, config: &PipelineSettings) -> bool {
    let transcript_config = transcribe_stage::load_transcript_config(pool).await;
    if transcript_config.is_remote() {
        return true;
    }
    !crate::audio::recording_commands::is_recording().await && idle::is_idle_for(config.idle_minutes)
}

/// Scan SharePoint and import anything new. Returns how many recordings were
/// imported.
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
    let since_iso = sync_state
        .last_sync_date
        .clone()
        .unwrap_or_else(|| (Utc::now() - chrono::Duration::days(30)).to_rfc3339());

    log::debug!("[pipeline] scanning SharePoint since {}", since_iso);
    let result = scan_recordings(app, &hub_url, &since_iso, AuthMode::SilentOnly)
        .await
        .map_err(ScanError::from_message)?;

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

    // Oldest first, and bounded so one scan cannot monopolise the loop; the
    // rest are picked up by the next scan.
    let mut to_import: Vec<SharePointScanItem> = new_items;
    to_import.reverse();
    let deferred = to_import.len().saturating_sub(MAX_IMPORTS_PER_SCAN);
    if deferred > 0 {
        log::info!(
            "[pipeline] importing {} of {} new recording(s); {} deferred to the next scan",
            MAX_IMPORTS_PER_SCAN,
            to_import.len(),
            deferred
        );
    }

    let transcript_config = transcribe_stage::load_transcript_config(pool).await;
    let mut imported = 0usize;
    let mut newest_imported: Option<String> = None;

    for item in to_import.into_iter().take(MAX_IMPORTS_PER_SCAN) {
        let file_url = item.recording.file_url.clone();
        let created = item.recording.created.clone();
        // Same title the manual import dialog would produce.
        let title = meeting_title_for(&item.recording.name);
        log::info!("[pipeline] importing recording \"{}\"", title);

        match crate::audio::import::import_from_url_internal(
            app.clone(),
            file_url.clone(),
            title.clone(),
            None,
            transcript_config.model.clone(),
            transcript_config.provider.clone(),
            Some("audio".to_string()),
            AuthMode::SilentOnly,
        )
        .await
        {
            Ok(()) => {
                // Marked only after the import actually landed, so a failed
                // download is retried on the next scan.
                mark_imported_public(app, &file_url);
                imported += 1;
                if newest_imported.as_deref().map_or(true, |n| created.as_str() > n) {
                    newest_imported = Some(created);
                }
                crate::pipeline::wake();
            }
            Err(e) => {
                let message = e.to_string();
                if is_auth_required_error(&message) {
                    return Err(ScanError::from_message(message));
                }
                log::warn!("[pipeline] import of \"{}\" failed: {}", title, message);
            }
        }
    }

    // Advance the watermark only past recordings that actually imported; the
    // ledger stays authoritative for individual files either way.
    if let Some(newest) = newest_imported {
        set_last_sync_date(app, &newest);
    }

    Ok(imported)
}
