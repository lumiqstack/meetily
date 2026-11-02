use crate::calendar::CalendarEvent;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool};

/// The Outlook calendar event a meeting was recorded against. Written once
/// when the meeting is saved from a prompt that carried a match; read for the
/// details pane and the summary prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeetingCalendar {
    /// `ics_feed` or `outlook_com`.
    pub source: String,
    pub event_id: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub organizer: Option<String>,
    pub attendees: Vec<String>,
    pub join_url: Option<String>,
    pub body: Option<String>,
}

#[derive(FromRow)]
struct MeetingCalendarRow {
    calendar_source: Option<String>,
    calendar_event_id: Option<String>,
    calendar_start: Option<String>,
    calendar_end: Option<String>,
    calendar_organizer: Option<String>,
    calendar_attendees: Option<String>,
    calendar_join_url: Option<String>,
    calendar_body: Option<String>,
}

impl MeetingCalendarRow {
    fn into_calendar(self) -> Option<MeetingCalendar> {
        let source = self.calendar_source?;
        let event_id = self.calendar_event_id?;
        let start = parse_time(self.calendar_start.as_deref())?;
        let end = parse_time(self.calendar_end.as_deref())?;
        let attendees = self
            .calendar_attendees
            .as_deref()
            .and_then(|json| serde_json::from_str::<Vec<String>>(json).ok())
            .unwrap_or_default();
        Some(MeetingCalendar {
            source,
            event_id,
            start,
            end,
            organizer: self.calendar_organizer,
            attendees,
            join_url: self.calendar_join_url,
            body: self.calendar_body,
        })
    }
}

fn parse_time(value: Option<&str>) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value?)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

pub struct MeetingCalendarRepository;

impl MeetingCalendarRepository {
    pub async fn attach(
        pool: &SqlitePool,
        meeting_id: &str,
        event: &CalendarEvent,
    ) -> Result<(), sqlx::Error> {
        let source = serde_json::to_value(event.source)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_else(|| "ics_feed".to_string());
        let attendees = serde_json::to_string(&event.attendees).unwrap_or_else(|_| "[]".to_string());
        sqlx::query(
            "UPDATE meetings SET calendar_source = ?, calendar_event_id = ?, calendar_start = ?, \
             calendar_end = ?, calendar_organizer = ?, calendar_attendees = ?, calendar_join_url = ?, \
             calendar_body = ? WHERE id = ?",
        )
        .bind(source)
        .bind(&event.id)
        .bind(event.start.to_rfc3339())
        .bind(event.end.to_rfc3339())
        .bind(&event.organizer)
        .bind(attendees)
        .bind(&event.teams_join_url)
        .bind(&event.body)
        .bind(meeting_id)
        .execute(pool)
        .await?;
        Ok(())
    }

    /// `None` when the meeting has no calendar match (or does not exist).
    pub async fn get(pool: &SqlitePool, meeting_id: &str) -> Result<Option<MeetingCalendar>, sqlx::Error> {
        let row: Option<MeetingCalendarRow> = sqlx::query_as(
            "SELECT calendar_source, calendar_event_id, calendar_start, calendar_end, calendar_organizer, \
             calendar_attendees, calendar_join_url, calendar_body FROM meetings WHERE id = ?",
        )
        .bind(meeting_id)
        .fetch_optional(pool)
        .await?;
        Ok(row.and_then(MeetingCalendarRow::into_calendar))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar::types::SourceKind;
    use chrono::TimeZone;

    async fn test_pool() -> SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite pool");
        sqlx::migrate!("./migrations").run(&pool).await.expect("migrations apply");
        sqlx::query(
            "INSERT INTO meetings (id, title, created_at, updated_at) \
             VALUES ('m1', 'Standup', '2026-11-02T10:00:00Z', '2026-11-02T10:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    fn event() -> CalendarEvent {
        CalendarEvent {
            id: "uid-1@2026-11-02T10:00:00+00:00".to_string(),
            source: SourceKind::IcsFeed,
            title: "Standup".to_string(),
            start: Utc.with_ymd_and_hms(2026, 11, 2, 10, 0, 0).unwrap(),
            end: Utc.with_ymd_and_hms(2026, 11, 2, 10, 30, 0).unwrap(),
            organizer: Some("Ana".to_string()),
            attendees: vec!["Ana".to_string(), "Bo".to_string()],
            location: None,
            body: Some("Agenda".to_string()),
            teams_join_url: Some("https://teams.microsoft.com/l/meetup-join/x".to_string()),
            is_recurring_instance: true,
        }
    }

    #[tokio::test]
    async fn round_trips_the_matched_event() {
        let pool = test_pool().await;
        MeetingCalendarRepository::attach(&pool, "m1", &event()).await.unwrap();

        let stored = MeetingCalendarRepository::get(&pool, "m1").await.unwrap().unwrap();
        assert_eq!(stored.source, "ics_feed");
        assert_eq!(stored.event_id, event().id);
        assert_eq!(stored.start, event().start);
        assert_eq!(stored.end, event().end);
        assert_eq!(stored.organizer.as_deref(), Some("Ana"));
        assert_eq!(stored.attendees, vec!["Ana", "Bo"]);
        assert_eq!(stored.join_url, event().teams_join_url);
        assert_eq!(stored.body.as_deref(), Some("Agenda"));
    }

    #[tokio::test]
    async fn unmatched_meeting_reads_as_none() {
        let pool = test_pool().await;
        assert_eq!(MeetingCalendarRepository::get(&pool, "m1").await.unwrap(), None);
        assert_eq!(MeetingCalendarRepository::get(&pool, "missing").await.unwrap(), None);
    }
}
