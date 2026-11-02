//! The `<calendar_context>` block appended to the final summary prompt when a
//! meeting was recorded against an Outlook event. Pure, so the cap and the
//! toggle are tested without a database.

use crate::database::repositories::meeting_calendar::MeetingCalendar;

/// Event bodies are invitations, often with long boilerplate; this keeps the
/// prompt bounded.
pub const BODY_CHAR_CAP: usize = 4_000;

/// `None` when there is nothing worth telling the model: no organizer, no
/// attendees and no body (or the body is excluded by the toggle).
pub fn build_calendar_context(
    meeting_title: &str,
    calendar: &MeetingCalendar,
    include_body: bool,
) -> Option<String> {
    let body = include_body
        .then(|| calendar.body.as_deref())
        .flatten()
        .map(str::trim)
        .filter(|body| !body.is_empty())
        .map(cap_body);

    if calendar.organizer.is_none() && calendar.attendees.is_empty() && body.is_none() {
        return None;
    }

    let mut block = String::from("\n\nCalendar invitation for this meeting:\n\n<calendar_context>\n");
    block.push_str(&format!("Title: {}\n", meeting_title.trim()));
    block.push_str(&format!(
        "Scheduled: {} to {} (UTC)\n",
        calendar.start.format("%Y-%m-%d %H:%M"),
        calendar.end.format("%H:%M")
    ));
    if let Some(organizer) = calendar.organizer.as_deref().filter(|o| !o.trim().is_empty()) {
        block.push_str(&format!("Organizer: {}\n", organizer.trim()));
    }
    if !calendar.attendees.is_empty() {
        block.push_str(&format!("Attendees: {}\n", calendar.attendees.join(", ")));
    }
    if let Some(body) = body {
        block.push_str("Invitation text:\n");
        block.push_str(&body);
        block.push('\n');
    }
    block.push_str("</calendar_context>");
    Some(block)
}

fn cap_body(body: &str) -> String {
    if body.chars().count() <= BODY_CHAR_CAP {
        return body.to_string();
    }
    let mut capped: String = body.chars().take(BODY_CHAR_CAP).collect();
    capped.push_str("\n[invitation text truncated]");
    capped
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn calendar(body: Option<&str>) -> MeetingCalendar {
        MeetingCalendar {
            source: "ics_feed".to_string(),
            event_id: "uid".to_string(),
            start: Utc.with_ymd_and_hms(2026, 11, 2, 10, 0, 0).unwrap(),
            end: Utc.with_ymd_and_hms(2026, 11, 2, 10, 30, 0).unwrap(),
            organizer: Some("Ana".to_string()),
            attendees: vec!["Ana".to_string(), "Bo".to_string()],
            join_url: None,
            body: body.map(str::to_string),
        }
    }

    #[test]
    fn includes_people_schedule_and_body() {
        let block = build_calendar_context("Standup", &calendar(Some("Agenda: wins")), true).unwrap();
        assert!(block.starts_with("\n\nCalendar invitation for this meeting:\n\n<calendar_context>\n"));
        assert!(block.contains("Title: Standup\n"));
        assert!(block.contains("Scheduled: 2026-11-02 10:00 to 10:30 (UTC)\n"));
        assert!(block.contains("Organizer: Ana\n"));
        assert!(block.contains("Attendees: Ana, Bo\n"));
        assert!(block.contains("Invitation text:\nAgenda: wins\n"));
        assert!(block.ends_with("</calendar_context>"));
    }

    #[test]
    fn toggle_off_keeps_people_but_drops_the_body() {
        let block = build_calendar_context("Standup", &calendar(Some("secret agenda")), false).unwrap();
        assert!(block.contains("Attendees: Ana, Bo"));
        assert!(!block.contains("secret agenda"));
        assert!(!block.contains("Invitation text"));
    }

    #[test]
    fn body_is_capped() {
        let long = "x".repeat(BODY_CHAR_CAP + 50);
        let block = build_calendar_context("Standup", &calendar(Some(&long)), true).unwrap();
        assert!(block.contains(&"x".repeat(BODY_CHAR_CAP)));
        assert!(!block.contains(&"x".repeat(BODY_CHAR_CAP + 1)));
        assert!(block.contains("[invitation text truncated]"));
    }

    #[test]
    fn nothing_to_say_yields_none() {
        let mut bare = calendar(None);
        bare.organizer = None;
        bare.attendees.clear();
        assert!(build_calendar_context("Standup", &bare, true).is_none());
        let mut only_body = bare.clone();
        only_body.body = Some("  ".to_string());
        assert!(build_calendar_context("Standup", &only_body, true).is_none());
    }
}
