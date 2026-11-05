//! What the SharePoint scan does with a recording of a meeting Meetily recorded
//! live: it links the file to that meeting instead of importing it. The rule and
//! the reasons for it are in docs/TEAMS_RECAP_SYNC_SPEC.md, under "Matching a
//! recording to a live recording".

use sqlx::SqlitePool;

use super::teams_recap_live_match::{match_live_recording, LiveMatch, RecordingFacts};
use crate::database::repositories::{
    meeting_calendar::MeetingCalendarRepository,
    meeting_sources::{MeetingSource, LIVE_RECORDING_SOURCE},
};

#[derive(Debug, PartialEq)]
pub enum RecordingTarget {
    /// A live meeting already holds this file or another part of the recording.
    /// The file needs no import and the meeting nothing more.
    Held(String),
    /// The live recording of this meeting. The file needs no import.
    Live(String),
    /// Import it as its own meeting. `overlapping` names live recordings that
    /// fit in time but could not be confirmed as this meeting.
    New { overlapping: Vec<String> },
}

/// Decide from the database as it is now, so a recording earlier in the same
/// scan can claim a meeting and leave its other parts held. `created` is
/// SharePoint's creation time.
pub async fn find_recording_target(
    pool: &SqlitePool,
    file_url: &str,
    created: &str,
) -> Result<RecordingTarget, sqlx::Error> {
    if let Some(meeting_id) = MeetingSource::live_recording_keys(pool)
        .await?
        .remove(&MeetingSource::live_key(file_url))
    {
        return Ok(RecordingTarget::Held(meeting_id));
    }
    let Some(facts) = RecordingFacts::from_file(file_url, created) else {
        return Ok(RecordingTarget::New { overlapping: Vec::new() });
    };
    let windows = MeetingCalendarRepository::live_windows_for_recording(pool).await?;
    Ok(match match_live_recording(&facts, &windows) {
        LiveMatch::None => RecordingTarget::New { overlapping: Vec::new() },
        LiveMatch::Ambiguous(overlapping) => RecordingTarget::New { overlapping },
        // The recording may be the only record of what was said.
        LiveMatch::Attach(meeting_id) if !has_transcript(pool, &meeting_id).await? => {
            RecordingTarget::New { overlapping: vec![meeting_id] }
        }
        LiveMatch::Attach(meeting_id) => RecordingTarget::Live(meeting_id),
    })
}

async fn has_transcript(pool: &SqlitePool, meeting_id: &str) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM transcripts WHERE meeting_id = ?)")
        .bind(meeting_id)
        .fetch_one(pool)
        .await
}

/// Record the file on the live meeting, so a recap of the same file attaches by
/// identity and a re-scan sees it as held. A meeting that already holds a link
/// keeps it: only a recap naming no file gets this far, and that link is its key.
pub async fn attach_recording(pool: &SqlitePool, meeting_id: &str, file_url: &str) -> Result<(), sqlx::Error> {
    let holds_link = MeetingSource::get(pool, meeting_id)
        .await?
        .is_some_and(|source| source.recording_url.is_some());
    if holds_link {
        return Ok(());
    }
    MeetingSource::for_import(LIVE_RECORDING_SOURCE, "", Some(file_url), None, None)
        .insert(pool, meeting_id)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::sharepoint_sync::{merge_scan_results, SharePointRecording};
    use crate::calendar::types::SourceKind;
    use crate::calendar::CalendarEvent;
    use chrono::{TimeZone, Utc};
    use serde_json::json;

    const FILE: &str = "https://c-my.sharepoint.com/personal/j/Documents/Recordings/Weekly%20sync-20261103_100200-Meeting%20Recording.mp4";
    const PART_TWO: &str = "https://c-my.sharepoint.com/personal/j/Documents/Recordings/Weekly%20sync-20261103_100200-Meeting%20Recording%202.mp4";
    /// Inside the 10:00 to 10:30 UTC slot of [`live_meeting`], whatever this
    /// machine's zone does to the stamp.
    const CREATED: &str = "2026-11-03T10:20:00Z";

    async fn test_pool() -> SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite pool");
        sqlx::migrate!("./migrations").run(&pool).await.expect("migrations apply");
        pool
    }

    async fn insert_meeting(pool: &SqlitePool, id: &str) {
        sqlx::query(
            "INSERT INTO meetings (id, title, created_at, updated_at) \
             VALUES (?, 'Generated heading', '2026-11-03T16:30:00Z', '2026-11-03T16:30:00Z')",
        )
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn add_transcript(pool: &SqlitePool, id: &str) {
        sqlx::query(
            "INSERT INTO transcripts (id, meeting_id, transcript, timestamp) \
             VALUES (?, ?, 'hello', '2026-11-03T10:00:00Z')",
        )
        .bind(format!("t-{id}"))
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    }

    /// Recorded live against a 10:00 to 10:30 UTC event, summarized since: its
    /// own title is the generated heading, not the subject.
    async fn live_meeting(pool: &SqlitePool, id: &str, subject: &str) {
        insert_meeting(pool, id).await;
        add_transcript(pool, id).await;
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

    fn scanned(file_url: &str) -> SharePointRecording {
        SharePointRecording {
            name: "Weekly sync.mp4".to_owned(),
            file_url: file_url.to_owned(),
            stream_url: String::new(),
            created: CREATED.to_owned(),
            size_bytes: None,
        }
    }

    /// One scan's view of `file_url`: `(already_imported, live_meeting_id)`.
    async fn rescan(pool: &SqlitePool, ledger: &[String], file_url: &str) -> (bool, Option<String>) {
        let keys = MeetingSource::live_recording_keys(pool).await.unwrap();
        let items = merge_scan_results(vec![scanned(file_url)], vec![], ledger, &[], &keys);
        (items[0].already_imported, items[0].live_meeting_id.clone())
    }

    #[tokio::test]
    async fn a_recording_attaches_by_the_subject_and_leaves_the_meeting_alone() {
        let pool = test_pool().await;
        live_meeting(&pool, "live", "Weekly sync").await;
        sqlx::query(
            "INSERT INTO summary_processes (meeting_id, status, created_at, updated_at, result) \
             VALUES ('live', 'completed', '2026-11-03T16:30:00Z', '2026-11-03T16:30:00Z', ?)",
        )
        .bind(json!({"markdown": "- from Meetily"}).to_string())
        .execute(&pool)
        .await
        .unwrap();

        let target = find_recording_target(&pool, FILE, CREATED).await.unwrap();
        assert_eq!(target, RecordingTarget::Live("live".to_owned()));
        attach_recording(&pool, "live", FILE).await.unwrap();

        let source = MeetingSource::get(&pool, "live").await.unwrap().unwrap();
        assert_eq!(source.import_source, LIVE_RECORDING_SOURCE);
        assert_eq!(source.recording_url.as_deref(), Some(FILE));
        assert_eq!(source.recorded_at.as_deref(), Some("2026-11-03T10:02:00"));
        assert_eq!(
            source.recording_file.as_deref(),
            Some("Weekly sync-20261103_100200-Meeting Recording.mp4")
        );

        let (title,): (String,) = sqlx::query_as("SELECT title FROM meetings WHERE id = 'live'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(title, "Generated heading");
        let (transcripts,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM transcripts WHERE meeting_id = 'live'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(transcripts, 1);
        let (summary,): (String,) = sqlx::query_as("SELECT result FROM summary_processes WHERE meeting_id = 'live'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(summary.contains("from Meetily"));

        // A recap of the file now finds this meeting by identity.
        let recap = format!(
            "https://teams.microsoft.com/l/meetingrecap?threadId=19%3Ax&fileUrl={}",
            url::form_urlencoded::byte_serialize(format!("{FILE}?web=1").as_bytes()).collect::<String>()
        );
        let key = crate::audio::teams_recap::meeting_identity_key(&recap);
        assert_eq!(
            MeetingSource::find_meeting_by_identity(&pool, &key).await.unwrap().as_deref(),
            Some("live")
        );
    }

    #[tokio::test]
    async fn a_recording_from_a_spanish_teams_links_like_an_english_one() {
        let pool = test_pool().await;
        live_meeting(&pool, "live", "Revisión semanal").await;
        // As search and the recap's Share button write the file, then as the
        // OneDrive listing does.
        let encoded = "https://c-my.sharepoint.com/personal/j/Documents/Recordings/Revisi%C3%B3n%20semanal-20261103_100200-Grabaci%C3%B3n%20de%20la%20reuni%C3%B3n.mp4";
        let listed = "https://c-my.sharepoint.com/personal/j/Documents/Recordings/Revisión%20semanal-20261103_100200-Grabación%20de%20la%20reunión.mp4";

        assert_eq!(
            find_recording_target(&pool, encoded, CREATED).await.unwrap(),
            RecordingTarget::Live("live".to_owned())
        );
        attach_recording(&pool, "live", encoded).await.unwrap();
        let source = MeetingSource::get(&pool, "live").await.unwrap().unwrap();
        assert_eq!(source.recorded_at.as_deref(), Some("2026-11-03T10:02:00"));
        assert_eq!(
            source.recording_file.as_deref(),
            Some("Revisión semanal-20261103_100200-Grabación de la reunión.mp4")
        );

        assert_eq!(rescan(&pool, &[], listed).await, (true, Some("live".to_owned())));
        let recap = format!(
            "https://teams.microsoft.com/l/meetingrecap?threadId=19%3Ax&fileUrl={}",
            url::form_urlencoded::byte_serialize(format!("{encoded}?web=1").as_bytes()).collect::<String>()
        );
        let key = crate::audio::teams_recap::meeting_identity_key(&recap);
        assert_eq!(
            MeetingSource::find_meeting_by_identity(&pool, &key).await.unwrap().as_deref(),
            Some("live")
        );
    }

    #[tokio::test]
    async fn an_ambiguous_recording_is_new_and_names_the_candidates() {
        let pool = test_pool().await;

        // Nothing recorded live in the slot: new, with nothing to report.
        assert_eq!(
            find_recording_target(&pool, FILE, CREATED).await.unwrap(),
            RecordingTarget::New { overlapping: vec![] }
        );

        // Double-booked: a live meeting fits, but its subject is another one.
        live_meeting(&pool, "other", "Budget review").await;
        assert_eq!(
            find_recording_target(&pool, FILE, CREATED).await.unwrap(),
            RecordingTarget::New { overlapping: vec!["other".to_owned()] }
        );

        // Meetily restarted during one call: both parts carry the event.
        live_meeting(&pool, "part1", "Weekly sync").await;
        live_meeting(&pool, "part2", "Weekly sync").await;
        assert_eq!(
            find_recording_target(&pool, FILE, CREATED).await.unwrap(),
            RecordingTarget::New { overlapping: vec!["part1".to_owned(), "part2".to_owned()] }
        );
    }

    #[tokio::test]
    async fn the_subject_is_what_confirms_not_the_generated_title() {
        let pool = test_pool().await;
        live_meeting(&pool, "live", "Budget review").await;
        sqlx::query("UPDATE meetings SET title = 'Weekly sync' WHERE id = 'live'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            find_recording_target(&pool, FILE, CREATED).await.unwrap(),
            RecordingTarget::New { overlapping: vec!["live".to_owned()] }
        );
    }

    #[tokio::test]
    async fn a_meeting_saved_before_the_subject_was_stored_falls_back_to_its_title() {
        let pool = test_pool().await;
        live_meeting(&pool, "live", "ignored").await;
        sqlx::query("UPDATE meetings SET calendar_title = NULL, title = 'Weekly sync' WHERE id = 'live'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            find_recording_target(&pool, FILE, CREATED).await.unwrap(),
            RecordingTarget::Live("live".to_owned())
        );
    }

    #[tokio::test]
    async fn a_live_meeting_with_no_transcript_does_not_take_the_recording() {
        let pool = test_pool().await;
        live_meeting(&pool, "live", "Weekly sync").await;
        sqlx::query("DELETE FROM transcripts").execute(&pool).await.unwrap();
        assert_eq!(
            find_recording_target(&pool, FILE, CREATED).await.unwrap(),
            RecordingTarget::New { overlapping: vec!["live".to_owned()] }
        );
    }

    #[tokio::test]
    async fn a_video_that_is_not_a_teams_recording_never_matches() {
        let pool = test_pool().await;
        live_meeting(&pool, "live", "Town hall").await;
        assert_eq!(
            find_recording_target(&pool, "https://c.sharepoint.com/sites/s/Videos/Town%20hall.mp4", CREATED)
                .await
                .unwrap(),
            RecordingTarget::New { overlapping: vec![] }
        );
    }

    #[tokio::test]
    async fn a_file_or_another_part_on_a_live_meeting_is_held_whatever_the_time_says() {
        let pool = test_pool().await;
        live_meeting(&pool, "live", "Weekly sync").await;
        attach_recording(&pool, "live", FILE).await.unwrap();
        for file in [FILE, PART_TWO] {
            assert_eq!(
                find_recording_target(&pool, file, "not a time").await.unwrap(),
                RecordingTarget::Held("live".to_owned()),
                "{file}"
            );
        }
    }

    #[tokio::test]
    async fn a_second_file_in_one_scan_cannot_claim_the_same_meeting() {
        let pool = test_pool().await;
        live_meeting(&pool, "live", "Weekly sync").await;
        attach_recording(&pool, "live", FILE).await.unwrap();

        let restarted = "https://c-my.sharepoint.com/personal/j/Documents/Recordings/Weekly%20sync-20261103_101500-Meeting%20Recording.mp4";
        assert_eq!(
            find_recording_target(&pool, restarted, CREATED).await.unwrap(),
            RecordingTarget::New { overlapping: vec![] }
        );
    }

    #[tokio::test]
    async fn a_rescan_sees_an_attached_file_as_held_and_attaching_again_changes_nothing() {
        let pool = test_pool().await;
        live_meeting(&pool, "live", "Weekly sync").await;
        assert_eq!(rescan(&pool, &[], FILE).await, (false, None));

        attach_recording(&pool, "live", FILE).await.unwrap();
        let first = MeetingSource::get(&pool, "live").await.unwrap();

        // The ledger write could have failed: the row alone is enough.
        assert_eq!(rescan(&pool, &[], FILE).await, (true, Some("live".to_owned())));
        // With the ledger too, nothing changes.
        assert_eq!(rescan(&pool, &[FILE.to_owned()], FILE).await, (true, Some("live".to_owned())));
        // The second part of the same recording needs no download either.
        assert_eq!(rescan(&pool, &[], PART_TWO).await, (true, Some("live".to_owned())));

        attach_recording(&pool, "live", FILE).await.unwrap();
        assert_eq!(MeetingSource::get(&pool, "live").await.unwrap(), first);
    }

    #[tokio::test]
    async fn the_ledger_keeps_a_file_imported_after_its_meeting_is_deleted() {
        let pool = test_pool().await;
        live_meeting(&pool, "live", "Weekly sync").await;
        attach_recording(&pool, "live", FILE).await.unwrap();
        crate::database::repositories::meeting::MeetingsRepository::delete_meeting(&pool, "live")
            .await
            .unwrap();
        assert_eq!(rescan(&pool, &[], FILE).await, (false, None));
        assert_eq!(rescan(&pool, &[FILE.to_owned()], FILE).await, (true, None));
    }

    #[tokio::test]
    async fn after_a_recap_attached_its_recording_is_already_held() {
        let pool = test_pool().await;
        live_meeting(&pool, "live", "Weekly sync").await;
        let recap = format!(
            "https://teams.microsoft.com/l/meetingrecap?threadId=19%3Ax&fileUrl={}",
            url::form_urlencoded::byte_serialize(format!("{FILE}?web=1").as_bytes()).collect::<String>()
        );
        MeetingSource::for_import(LIVE_RECORDING_SOURCE, "Weekly sync", Some(&recap), None, None)
            .insert(&pool, "live")
            .await
            .unwrap();

        assert_eq!(rescan(&pool, &[], FILE).await, (true, Some("live".to_owned())));
        assert_eq!(rescan(&pool, &[], PART_TWO).await, (true, Some("live".to_owned())));
        // Another recording in that slot cannot take a meeting with a recap of a file.
        let other = "https://c-my.sharepoint.com/personal/j/Documents/Recordings/Weekly%20sync-20261103_101500-Meeting%20Recording.mp4";
        assert_eq!(
            find_recording_target(&pool, other, CREATED).await.unwrap(),
            RecordingTarget::New { overlapping: vec![] }
        );
    }

    #[tokio::test]
    async fn a_recap_naming_no_file_keeps_its_link_when_the_recording_attaches() {
        let pool = test_pool().await;
        live_meeting(&pool, "live", "Weekly sync").await;
        let recap = "https://teams.microsoft.com/l/meetingrecap?threadId=19%3Ax";
        MeetingSource::for_import(LIVE_RECORDING_SOURCE, "Weekly sync", Some(recap), None, None)
            .insert(&pool, "live")
            .await
            .unwrap();

        assert_eq!(
            find_recording_target(&pool, FILE, CREATED).await.unwrap(),
            RecordingTarget::Live("live".to_owned())
        );
        attach_recording(&pool, "live", FILE).await.unwrap();

        let source = MeetingSource::get(&pool, "live").await.unwrap().unwrap();
        assert_eq!(source.recording_url.as_deref(), Some(recap));
        // Only the ledger remembers this file, and it is enough for a re-scan.
        assert_eq!(rescan(&pool, &[FILE.to_owned()], FILE).await.0, true);
    }

    #[tokio::test]
    async fn a_recap_only_meeting_does_not_stop_its_recording_from_importing() {
        let pool = test_pool().await;
        insert_meeting(&pool, "recap-only").await;
        let recap = format!(
            "https://teams.microsoft.com/l/meetingrecap?threadId=19%3Ax&fileUrl={}",
            url::form_urlencoded::byte_serialize(FILE.as_bytes()).collect::<String>()
        );
        MeetingSource::for_import("copilot-recap", "Weekly sync", Some(&recap), None, None)
            .insert(&pool, "recap-only")
            .await
            .unwrap();
        assert_eq!(rescan(&pool, &[], FILE).await, (false, None));
        assert_eq!(
            find_recording_target(&pool, FILE, CREATED).await.unwrap(),
            RecordingTarget::New { overlapping: vec![] }
        );
    }
}
