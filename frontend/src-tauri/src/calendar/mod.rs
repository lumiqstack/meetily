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

pub use commands::CalendarManagerState;
pub use manager::CalendarManager;
pub use types::{CalendarEvent, CalendarSettings, CalendarStatus};
