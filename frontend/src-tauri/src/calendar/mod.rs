//! Outlook calendar integration without Microsoft Graph.
//!
//! Design: docs/CALENDAR_MEETING_DETECTION_SPEC.md. Sources implement
//! [`source::CalendarSource`]; the [`manager::CalendarManager`] polls them,
//! keeps the upcoming-event list, and raises a nudge shortly before each
//! Teams meeting. The one source is classic Outlook on the same machine,
//! read through its COM object model (`outlook_com`). The published ICS feed
//! source was removed on 2026-11-03: the tenants this is built for block
//! calendar publishing, so it could never be configured.

pub mod commands;
pub mod manager;
pub mod outlook_com;
pub mod scheduler;
pub mod source;
pub mod teams_link;
pub mod types;

pub use commands::CalendarManagerState;
pub use manager::CalendarManager;
pub use types::{CalendarEvent, CalendarSettings, CalendarStatus};
