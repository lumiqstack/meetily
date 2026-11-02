use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Emitted once per Teams meeting, `nudge_lead_minutes` before it starts.
pub const UPCOMING_EVENT: &str = "calendar-meeting-upcoming";
/// Emitted after every poll with the new [`CalendarStatus`].
pub const STATUS_EVENT: &str = "calendar-status-changed";

/// How often the manager wakes to check for due nudges.
pub const TICK_SECONDS: u64 = 30;
/// How often the ICS feed is re-fetched. Outlook refreshes published
/// calendars with a lag of minutes to an hour, so polling faster buys nothing.
pub const ICS_POLL_INTERVAL_SECONDS: u64 = 300;
/// Events are kept from this far back, so a meeting that started before the
/// app launched still matches a Teams call.
pub const LOOKBACK_HOURS: i64 = 1;
/// Events are kept this far ahead: today and tomorrow.
pub const LOOKAHEAD_HOURS: i64 = 48;
/// A nudge whose event has started this long ago is no longer worth showing.
pub const NUDGE_GRACE_SECONDS: i64 = 120;
/// A detected Teams call matches an event that started up to this long ago...
pub const MATCH_AFTER_START_MINUTES: i64 = 20;
/// ...or that starts up to this long from now (joined early).
pub const MATCH_BEFORE_START_MINUTES: i64 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    IcsFeed,
    OutlookCom,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarEvent {
    /// Stable across polls: the UID, plus the occurrence start for instances
    /// of a recurring series.
    pub id: String,
    pub source: SourceKind,
    pub title: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub organizer: Option<String>,
    pub attendees: Vec<String>,
    pub location: Option<String>,
    pub body: Option<String>,
    pub teams_join_url: Option<String>,
    pub is_recurring_instance: bool,
}

impl CalendarEvent {
    /// A Teams meeting carries a join link, or names Teams as its location.
    /// Classic Outlook puts the link in the body, which the COM source never
    /// reads, so the location text is the second signal.
    pub fn is_teams_meeting(&self) -> bool {
        self.teams_join_url.is_some()
            || self
                .location
                .as_deref()
                .map(|location| location.to_ascii_lowercase().contains("microsoft teams"))
                .unwrap_or(false)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SourceHealth {
    /// No URL (feed) or no client (COM) has been set up yet.
    NotConfigured,
    Ok,
    /// The source exists but cannot be used until the user acts: the feed link
    /// was revoked, or classic Outlook is not installed.
    Unavailable { reason: String },
    /// A transient failure. The last good event list is kept.
    Error { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarSettings {
    #[serde(default)]
    pub calendar_enabled: bool,
    /// Derived from the keyring at load time, never trusted from the file or
    /// the frontend.
    #[serde(default)]
    pub ics_feed_configured: bool,
    /// Reserved for the classic Outlook COM source. No effect yet.
    #[serde(default)]
    pub outlook_com_enabled: bool,
    #[serde(default = "default_nudge_lead_minutes")]
    pub nudge_lead_minutes: u64,
    #[serde(default = "default_true")]
    pub include_body_in_summary: bool,
}

pub const MAX_NUDGE_LEAD_MINUTES: u64 = 15;

fn default_nudge_lead_minutes() -> u64 {
    2
}

fn default_true() -> bool {
    true
}

impl Default for CalendarSettings {
    fn default() -> Self {
        Self {
            calendar_enabled: false,
            ics_feed_configured: false,
            outlook_com_enabled: false,
            nudge_lead_minutes: default_nudge_lead_minutes(),
            include_body_in_summary: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceStatus {
    pub kind: SourceKind,
    pub health: SourceHealth,
    pub last_poll_at: Option<DateTime<Utc>>,
    pub event_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarStatus {
    pub settings: CalendarSettings,
    pub running: bool,
    pub sources: Vec<SourceStatus>,
    /// The next Teams meeting that has not ended yet.
    pub next_event: Option<CalendarEvent>,
}

impl CalendarStatus {
    pub fn stopped(settings: CalendarSettings) -> Self {
        Self {
            settings,
            running: false,
            sources: vec![],
            next_event: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalendarMeetingUpcomingPayload {
    #[serde(rename = "type")]
    pub event_type: String,
    pub event: CalendarEvent,
    pub lead_minutes: u64,
    pub starts_in_seconds: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_missing_fields_take_defaults() {
        let parsed: CalendarSettings = serde_json::from_str(r#"{"calendar_enabled": true}"#).unwrap();
        assert!(parsed.calendar_enabled);
        assert_eq!(parsed.nudge_lead_minutes, 2);
        assert!(parsed.include_body_in_summary);
        assert!(!parsed.outlook_com_enabled);
    }

    #[test]
    fn settings_round_trip() {
        let settings = CalendarSettings {
            calendar_enabled: true,
            ics_feed_configured: true,
            outlook_com_enabled: false,
            nudge_lead_minutes: 5,
            include_body_in_summary: false,
        };
        let json = serde_json::to_string(&settings).unwrap();
        assert_eq!(serde_json::from_str::<CalendarSettings>(&json).unwrap(), settings);
    }
}
