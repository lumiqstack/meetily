//! Decides which Teams meetings get a nudge on this tick. Pure, so the timing
//! rules are tested without a clock or a feed.

use super::types::{CalendarEvent, MATCH_AFTER_START_MINUTES, MATCH_BEFORE_START_MINUTES, NUDGE_GRACE_SECONDS};
use chrono::{DateTime, Duration, Utc};
use std::collections::HashSet;

pub struct NudgePlanner {
    started_at: DateTime<Utc>,
    nudged: HashSet<String>,
}

impl NudgePlanner {
    pub fn new(started_at: DateTime<Utc>) -> Self {
        Self {
            started_at,
            nudged: HashSet::new(),
        }
    }

    /// Teams events whose nudge time has arrived and that have not been
    /// nudged yet. Each event id is nudged at most once. Meetings that were
    /// already running when the planner was created are never nudged, and a
    /// nudge missed by more than the grace period (the machine was asleep) is
    /// dropped rather than shown late.
    pub fn due(&mut self, events: &[CalendarEvent], now: DateTime<Utc>, lead: Duration) -> Vec<CalendarEvent> {
        let grace = Duration::seconds(NUDGE_GRACE_SECONDS);
        let mut due = Vec::new();

        for event in events {
            if !event.is_teams_meeting() || self.nudged.contains(&event.id) {
                continue;
            }
            if event.start <= self.started_at {
                self.nudged.insert(event.id.clone());
                continue;
            }
            if now < event.start - lead {
                continue;
            }
            self.nudged.insert(event.id.clone());
            if now >= event.start + grace {
                continue;
            }
            due.push(event.clone());
        }

        let live: HashSet<&str> = events.iter().map(|event| event.id.as_str()).collect();
        self.nudged.retain(|id| live.contains(id.as_str()));

        due
    }
}

/// The Teams event a call detected at `now` most likely belongs to: its
/// start lies between `MATCH_AFTER_START_MINUTES` ago and
/// `MATCH_BEFORE_START_MINUTES` ahead, and it has not ended. Ties go to the
/// start closest to `now`.
pub fn match_event_for_call(events: &[CalendarEvent], now: DateTime<Utc>) -> Option<CalendarEvent> {
    let earliest = now - Duration::minutes(MATCH_AFTER_START_MINUTES);
    let latest = now + Duration::minutes(MATCH_BEFORE_START_MINUTES);
    events
        .iter()
        .filter(|event| event.is_teams_meeting() && event.end > now)
        .filter(|event| event.start >= earliest && event.start <= latest)
        .min_by_key(|event| (event.start - now).num_seconds().abs())
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar::types::SourceKind;
    use chrono::TimeZone;

    fn at(h: u32, m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 3, h, m, 0).unwrap()
    }

    fn event(id: &str, start: DateTime<Utc>, teams: bool) -> CalendarEvent {
        CalendarEvent {
            id: id.to_string(),
            source: SourceKind::OutlookCom,
            title: id.to_string(),
            start,
            end: start + Duration::minutes(30),
            organizer: None,
            attendees: vec![],
            location: None,
            body: None,
            teams_join_url: teams.then(|| "https://teams.microsoft.com/l/meetup-join/x".to_string()),
            is_recurring_instance: false,
        }
    }

    #[test]
    fn nudges_once_when_lead_time_arrives() {
        let mut planner = NudgePlanner::new(at(9, 0));
        let events = vec![event("a", at(10, 0), true)];
        let lead = Duration::minutes(2);

        assert!(planner.due(&events, at(9, 57), lead).is_empty());
        assert_eq!(planner.due(&events, at(9, 58), lead).len(), 1);
        assert!(planner.due(&events, at(9, 59), lead).is_empty());
        assert!(planner.due(&events, at(10, 0), lead).is_empty());
    }

    #[test]
    fn ignores_non_teams_events() {
        let mut planner = NudgePlanner::new(at(9, 0));
        let events = vec![event("room", at(10, 0), false)];
        assert!(planner.due(&events, at(9, 59), Duration::minutes(2)).is_empty());
    }

    #[test]
    fn meeting_already_running_at_launch_is_not_nudged() {
        let mut planner = NudgePlanner::new(at(10, 5));
        let events = vec![event("a", at(10, 0), true)];
        assert!(planner.due(&events, at(10, 5), Duration::minutes(2)).is_empty());
        assert!(planner.due(&events, at(10, 6), Duration::minutes(2)).is_empty());
    }

    #[test]
    fn meeting_starting_right_after_launch_is_nudged() {
        let mut planner = NudgePlanner::new(at(9, 59));
        let events = vec![event("a", at(10, 0), true)];
        assert_eq!(planner.due(&events, at(9, 59), Duration::minutes(2)).len(), 1);
    }

    #[test]
    fn nudge_missed_past_grace_is_dropped() {
        let mut planner = NudgePlanner::new(at(9, 0));
        let events = vec![event("a", at(10, 0), true)];
        assert!(planner.due(&events, at(10, 3), Duration::minutes(2)).is_empty());
        assert!(planner.due(&events, at(10, 4), Duration::minutes(2)).is_empty());
    }

    #[test]
    fn zero_lead_nudges_at_start() {
        let mut planner = NudgePlanner::new(at(9, 0));
        let events = vec![event("a", at(10, 0), true)];
        assert!(planner.due(&events, at(9, 59), Duration::zero()).is_empty());
        assert_eq!(planner.due(&events, at(10, 0), Duration::zero()).len(), 1);
    }

    #[test]
    fn forgets_ids_that_leave_the_window() {
        let mut planner = NudgePlanner::new(at(9, 0));
        let events = vec![event("a", at(10, 0), true)];
        assert_eq!(planner.due(&events, at(9, 58), Duration::minutes(2)).len(), 1);
        planner.due(&[], at(11, 0), Duration::minutes(2));
        assert!(planner.nudged.is_empty());
    }

    #[test]
    fn match_prefers_the_start_closest_to_now() {
        let now = at(10, 0);
        let events = vec![
            event("earlier", at(9, 45), true),
            event("closest", at(9, 55), true),
            event("later", at(10, 8), true),
        ];
        assert_eq!(match_event_for_call(&events, now).unwrap().id, "closest");
    }

    #[test]
    fn match_window_is_20_minutes_back_and_10_ahead() {
        let now = at(10, 0);
        assert!(match_event_for_call(&[event("old", at(9, 39), true)], now).is_none());
        assert!(match_event_for_call(&[event("edge-back", at(9, 40), true)], now).is_some());
        assert!(match_event_for_call(&[event("edge-ahead", at(10, 10), true)], now).is_some());
        assert!(match_event_for_call(&[event("far", at(10, 11), true)], now).is_none());
    }

    #[test]
    fn match_skips_non_teams_and_ended_events() {
        let now = at(10, 0);
        let mut ended = event("ended", at(9, 50), true);
        ended.end = at(9, 58);
        let events = vec![event("in-room", at(9, 58), false), ended];
        assert!(match_event_for_call(&events, now).is_none());
    }
}
