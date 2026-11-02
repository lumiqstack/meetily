//! Outlook calendar integration without Microsoft Graph.
//!
//! Design: docs/CALENDAR_MEETING_DETECTION_SPEC.md. Sources implement
//! [`source::CalendarSource`]; the [`manager::CalendarManager`] polls them,
//! keeps the merged upcoming-event list, and raises a nudge shortly before
//! each Teams meeting. This PR ships the published ICS feed source; the
//! classic Outlook COM source comes later.

pub mod commands;
pub mod ics_feed;
pub mod manager;
pub mod scheduler;
pub mod secret;
pub mod source;
pub mod teams_link;
pub mod types;

use chrono::Utc;
use tauri::{Manager, Runtime};

/// The calendar event a Teams call detected right now belongs to, if the
/// calendar manager is running and has one. Safe to call from any runtime:
/// without the managed state (tests) it is `None`.
pub async fn match_event_for_detected_call<R: Runtime>(app_handle: &tauri::AppHandle<R>) -> Option<CalendarEvent> {
    let state = app_handle.try_state::<CalendarManagerState>()?;
    let guard = state.read().await;
    let manager = guard.as_ref()?;
    manager.match_for_call(Utc::now()).await
}

pub use commands::CalendarManagerState;
pub use manager::{load_settings, CalendarManager};
pub use types::{CalendarEvent, CalendarSettings, CalendarStatus};
