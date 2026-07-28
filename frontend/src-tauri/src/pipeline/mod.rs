//! Automatic meeting pipeline.
//!
//! A single long-lived background task that turns new recordings into
//! finished, exported meeting notes without any UI interaction:
//!
//! ```text
//! SharePoint scan → import → transcribe → summarize → Obsidian export
//! ```
//!
//! Design notes:
//!
//! - **The work list is derived, never stored.** Every tick re-runs
//!   `MeetingsRepository::get_pending_meetings`, the same query the
//!   pending-work panel uses. A crash mid-stage therefore needs no
//!   reconciliation: whatever did not get committed simply still looks
//!   pending. The only persisted pipeline state is retry bookkeeping
//!   (`pipeline_meta`) and the run state / scan watermark in the settings
//!   store.
//! - **One meeting at a time.** Sequential processing means the pipeline
//!   needs at most one remote-summary slot and one engine claim, so it never
//!   has to queue against itself, and a conflict with user-initiated work
//!   just means "try again next tick".
//! - **The existing fail-fast guards are the lock.** Import, retranscription
//!   and recording already refuse to run concurrently; the pipeline treats
//!   those refusals as transient and backs off.

pub mod commands;
pub mod idle;
pub mod meta;
pub mod orchestrator;
pub mod scan_stage;
pub mod settings;
pub mod summary_stage;
pub mod transcribe_stage;

use serde::Serialize;
use settings::PipelineRunState;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::{Notify, RwLock};

/// A stage failure, classified for the retry policy.
#[derive(Debug, Clone)]
pub struct StageError {
    pub message: String,
    /// Transient errors back off and retry forever; hard errors count
    /// against the attempt budget and eventually stop.
    pub transient: bool,
}

impl StageError {
    pub fn hard(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            transient: false,
        }
    }

    pub fn transient(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            transient: true,
        }
    }
}

impl std::fmt::Display for StageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// What the pipeline is doing right now, for the UI and tray.
#[derive(Debug, Clone, Serialize)]
pub struct CurrentItem {
    pub meeting_id: String,
    pub title: String,
    /// "scan" | "import" | "transcribe" | "summarize"
    pub stage: String,
    pub started_at: String,
}

/// Shared handles the orchestrator task and the Tauri commands both touch.
pub struct PipelineHandle {
    notify: Notify,
    state: RwLock<PipelineRunState>,
    current: RwLock<Option<CurrentItem>>,
    /// Meetings the user explicitly asked to process now: these skip the
    /// grace window and the idle gate.
    forced: Mutex<Vec<String>>,
}

impl PipelineHandle {
    fn new(state: PipelineRunState) -> Self {
        Self {
            notify: Notify::new(),
            state: RwLock::new(state),
            current: RwLock::new(None),
            forced: Mutex::new(Vec::new()),
        }
    }

    pub async fn run_state(&self) -> PipelineRunState {
        self.state.read().await.clone()
    }

    pub async fn set_run_state(&self, state: PipelineRunState) {
        *self.state.write().await = state;
    }

    pub async fn current(&self) -> Option<CurrentItem> {
        self.current.read().await.clone()
    }

    /// Non-blocking reads for synchronous callers (the tray menu builder,
    /// which runs on the UI thread and must never block or `block_on`).
    /// Returns `None` when the lock is momentarily held, in which case the
    /// caller simply omits the pipeline section until the next refresh.
    pub fn run_state_hint(&self) -> Option<PipelineRunState> {
        self.state.try_read().ok().map(|state| state.clone())
    }

    pub fn current_hint(&self) -> Option<CurrentItem> {
        self.current.try_read().ok().and_then(|item| item.clone())
    }

    pub(crate) async fn set_current(&self, item: Option<CurrentItem>) {
        *self.current.write().await = item;
    }

    /// Queue meetings for immediate processing and wake the loop.
    pub fn force(&self, meeting_ids: &[String]) {
        if let Ok(mut forced) = self.forced.lock() {
            for id in meeting_ids {
                if !forced.contains(id) {
                    forced.push(id.clone());
                }
            }
        }
        self.notify.notify_one();
    }

    pub(crate) fn is_forced(&self, meeting_id: &str) -> bool {
        self.forced
            .lock()
            .map(|forced| forced.iter().any(|id| id == meeting_id))
            .unwrap_or(false)
    }

    pub(crate) fn clear_forced(&self, meeting_id: &str) {
        if let Ok(mut forced) = self.forced.lock() {
            forced.retain(|id| id != meeting_id);
        }
    }

    pub fn wake(&self) {
        self.notify.notify_one();
    }

    /// Sleep until woken by new work or the tick deadline passes.
    pub(crate) async fn wait_for_work(&self, tick: Duration) {
        let _ = tokio::time::timeout(tick, self.notify.notified()).await;
    }
}

static PIPELINE: OnceLock<PipelineHandle> = OnceLock::new();

pub fn handle() -> Option<&'static PipelineHandle> {
    PIPELINE.get()
}

/// Nudge the pipeline to look for work now (e.g. an import just finished).
/// Safe to call before the pipeline starts.
pub fn wake() {
    if let Some(pipeline) = PIPELINE.get() {
        pipeline.wake();
    }
}

/// Tracks the retranscription the pipeline started, so a live recording can
/// reclaim the shared transcription engine from it.
pub(crate) struct PipelineRetranscription {
    meeting_id: Mutex<Option<String>>,
}

impl PipelineRetranscription {
    const fn new() -> Self {
        Self {
            meeting_id: Mutex::new(None),
        }
    }

    pub(crate) fn claim(&self, meeting_id: &str) {
        if let Ok(mut slot) = self.meeting_id.lock() {
            *slot = Some(meeting_id.to_string());
        }
    }

    pub(crate) fn release(&self, meeting_id: &str) {
        if let Ok(mut slot) = self.meeting_id.lock() {
            if slot.as_deref() == Some(meeting_id) {
                *slot = None;
            }
        }
    }

    fn active(&self) -> Option<String> {
        self.meeting_id.lock().ok().and_then(|slot| slot.clone())
    }
}

pub(crate) static PIPELINE_RETRANSCRIPTION: PipelineRetranscription = PipelineRetranscription::new();

/// Give the shared transcription engine back for a live recording.
///
/// Only cancels a retranscription the *pipeline* started — a user-initiated
/// one still fails the recording start with the existing error, because the
/// user chose to run it. Returns once the engine is free or `timeout`
/// elapses.
pub async fn yield_engine_for_recording(timeout: Duration) -> bool {
    let Some(meeting_id) = PIPELINE_RETRANSCRIPTION.active() else {
        return true;
    };

    log::info!(
        "[pipeline] cancelling background transcription of {} so recording can use the engine",
        meeting_id
    );
    crate::audio::retranscription::cancel_retranscription(Some(&meeting_id));

    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if !crate::audio::retranscription::is_retranscription_active_for_meeting(&meeting_id) {
            log::info!("[pipeline] engine released for recording");
            return true;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    log::warn!(
        "[pipeline] background transcription of {} did not stop within {:?}",
        meeting_id,
        timeout
    );
    false
}

/// Start the pipeline task. Called once during app setup.
pub fn spawn_pipeline(app: tauri::AppHandle) {
    if PIPELINE.get().is_some() {
        log::warn!("[pipeline] already running; ignoring duplicate spawn");
        return;
    }

    let app_for_state = app.clone();
    tauri::async_runtime::spawn(async move {
        // Restore the persisted run state so a pause or a pending sign-in
        // survives a restart.
        let restored = settings::load_settings(&app_for_state)
            .await
            .map(|s| s.run_state)
            .unwrap_or_default();

        if PIPELINE.set(PipelineHandle::new(restored)).is_err() {
            return;
        }

        orchestrator::run(app_for_state).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_errors_carry_their_retry_class() {
        assert!(StageError::transient("endpoint down").transient);
        assert!(!StageError::hard("bad config").transient);
        assert_eq!(StageError::hard("boom").to_string(), "boom");
    }

    #[test]
    fn pipeline_retranscription_only_releases_its_own_claim() {
        let tracker = PipelineRetranscription::new();
        tracker.claim("meeting-a");
        tracker.release("meeting-b");
        assert_eq!(tracker.active().as_deref(), Some("meeting-a"));
        tracker.release("meeting-a");
        assert!(tracker.active().is_none());
    }

    #[tokio::test]
    async fn forced_meetings_are_tracked_until_cleared() {
        let handle = PipelineHandle::new(PipelineRunState::Running);
        handle.force(&["m1".to_string(), "m2".to_string()]);
        assert!(handle.is_forced("m1"));
        assert!(handle.is_forced("m2"));
        assert!(!handle.is_forced("m3"));

        handle.clear_forced("m1");
        assert!(!handle.is_forced("m1"));
        assert!(handle.is_forced("m2"));
    }

    #[tokio::test]
    async fn yielding_is_a_no_op_when_the_pipeline_owns_nothing() {
        assert!(yield_engine_for_recording(Duration::from_millis(10)).await);
    }
}
