use crate::audio::teams_recap::{meeting_identity_key, whole_recording_key};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, Sqlite, SqlitePool};
use std::collections::HashMap;

/// `import_source` of a meeting Meetily recorded live and later matched to its
/// Teams recap or recording. The row carries the recap's link or the file's URL,
/// so the next import of either finds this meeting by identity.
pub const LIVE_RECORDING_SOURCE: &str = "live-recording";

/// Where an imported meeting came from. Written once when the import
/// creates the meeting; read when exporting its Obsidian note.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, FromRow)]
pub struct MeetingSource {
    /// `sharepoint`, `url`, `teams-transcript`, `copilot-recap` or `file`, or
    /// [`LIVE_RECORDING_SOURCE`] for a live recording a recap or file attached to.
    pub import_source: String,
    pub recording_url: Option<String>,
    pub recording_file: Option<String>,
    /// Teams recording stamp, local time without offset (`2026-07-21T10:02:21`).
    pub recorded_at: Option<String>,
    pub duration_seconds: Option<f64>,
}

impl MeetingSource {
    /// Describe an import from what the importer knows: the meeting title,
    /// the link it was downloaded from (if any) and the local file it read.
    pub fn for_import(
        import_source: &str,
        title: &str,
        url: Option<&str>,
        local_file: Option<&str>,
        duration_seconds: Option<f64>,
    ) -> Self {
        let recording_file = url
            .and_then(file_name_from_url)
            .or_else(|| local_file.and_then(file_name_from_path));
        let recorded_at = [Some(title), recording_file.as_deref(), url]
            .into_iter()
            .flatten()
            .find_map(|name| {
                let last = name.rsplit(['/', '\\']).next().unwrap_or(name);
                let decoded = crate::audio::sharepoint_sync::percent_decode_component(last);
                crate::audio::teams_recap::metadata::from_filename(&decoded).map(|m| m.recorded_at)
            });
        Self {
            import_source: import_source.to_string(),
            recording_url: url.map(strip_token_query),
            recording_file,
            recorded_at,
            duration_seconds: duration_seconds.filter(|d| d.is_finite() && *d > 0.0),
        }
    }

    pub async fn insert<'e, E>(&self, executor: E, meeting_id: &str) -> Result<(), sqlx::Error>
    where
        E: sqlx::Executor<'e, Database = Sqlite>,
    {
        sqlx::query(
            "INSERT OR REPLACE INTO meeting_sources \
             (meeting_id, import_source, recording_url, recording_file, recorded_at, duration_seconds) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(meeting_id)
        .bind(&self.import_source)
        .bind(&self.recording_url)
        .bind(&self.recording_file)
        .bind(&self.recorded_at)
        .bind(self.duration_seconds)
        .execute(executor)
        .await?;
        Ok(())
    }

    /// The meeting imported from the same recording as `key` (see
    /// [`crate::audio::teams_recap::meeting_identity_key`]), so a recap can
    /// attach to the meeting that already holds the recording or an earlier
    /// copy of the recap. The file itself wins, then any other part of the same
    /// long recording; within each, the oldest meeting. Stored URLs are raw, so
    /// the comparison runs in Rust over the rows that have one.
    pub async fn find_meeting_by_identity(
        pool: &SqlitePool,
        key: &str,
    ) -> Result<Option<String>, sqlx::Error> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT ms.meeting_id, ms.recording_url FROM meeting_sources ms \
             JOIN meetings m ON m.id = ms.meeting_id \
             WHERE ms.recording_url IS NOT NULL \
             ORDER BY m.created_at, m.id",
        )
        .fetch_all(pool)
        .await?;
        let keyed: Vec<(String, String)> = rows
            .into_iter()
            .map(|(meeting_id, url)| (meeting_id, meeting_identity_key(&url)))
            .collect();
        let whole = whole_recording_key(key);
        Ok(keyed
            .iter()
            .find(|(_, stored)| stored == key)
            .or_else(|| keyed.iter().find(|(_, stored)| whole_recording_key(stored) == whole))
            .map(|(meeting_id, _)| meeting_id.clone()))
    }

    /// Whole-recording keys held by live meetings, each with its meeting, for
    /// looking up [`Self::live_key`]. The live transcript covers every part of a
    /// recording, so a file whose key is here need not be downloaded.
    pub async fn live_recording_keys(pool: &SqlitePool) -> Result<HashMap<String, String>, sqlx::Error> {
        Ok(Self::live_recording_links(pool)
            .await?
            .into_iter()
            .map(|(meeting_id, url)| (Self::live_key(&url), meeting_id))
            .collect())
    }

    /// The key a link goes by in [`Self::live_recording_keys`].
    pub fn live_key(link: &str) -> String {
        whole_recording_key(&meeting_identity_key(link)).to_owned()
    }

    async fn live_recording_links(pool: &SqlitePool) -> Result<Vec<(String, String)>, sqlx::Error> {
        sqlx::query_as(
            "SELECT meeting_id, recording_url FROM meeting_sources \
             WHERE import_source = ? AND recording_url IS NOT NULL",
        )
        .bind(LIVE_RECORDING_SOURCE)
        .fetch_all(pool)
        .await
    }

    /// The file was imported as its own meeting after all, so any live meeting
    /// it was attached to gives it up and a later recap follows the new meeting.
    pub async fn release_from_live_meetings(pool: &SqlitePool, file_url: &str) -> Result<(), sqlx::Error> {
        let key = meeting_identity_key(file_url);
        for (meeting_id, url) in Self::live_recording_links(pool).await? {
            if meeting_identity_key(&url) == key {
                Self::remove(pool, &meeting_id).await?;
            }
        }
        Ok(())
    }

    pub async fn remove(pool: &SqlitePool, meeting_id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM meeting_sources WHERE meeting_id = ?")
            .bind(meeting_id)
            .execute(pool)
            .await?;
        Ok(())
    }

    pub async fn get(pool: &SqlitePool, meeting_id: &str) -> Result<Option<Self>, sqlx::Error> {
        sqlx::query_as::<_, Self>(
            "SELECT import_source, recording_url, recording_file, recorded_at, duration_seconds \
             FROM meeting_sources WHERE meeting_id = ?",
        )
        .bind(meeting_id)
        .fetch_optional(pool)
        .await
    }
}

/// `import_source` for a URL import: SharePoint/OneDrive hosts, else `url`.
pub fn url_import_source(url: &str) -> &'static str {
    let is_sharepoint = url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
        .is_some_and(|h| h.ends_with(".sharepoint.com") || h.ends_with(".sharepoint.us"));
    if is_sharepoint {
        "sharepoint"
    } else {
        "url"
    }
}

/// The recording's file name in a link: the last path segment when it is a
/// media file, or the `id` parameter of a `stream.aspx?id=/path/file.mp4` link.
fn file_name_from_url(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let from_path = crate::audio::sharepoint_sync::percent_decode_component(
        parsed.path().rsplit('/').next().unwrap_or(""),
    );
    let candidate = if is_media(&from_path) {
        from_path
    } else {
        let id = parsed.query_pairs().find(|(k, _)| k == "id")?.1.into_owned();
        id.rsplit('/').next()?.to_string()
    };
    is_media(&candidate).then_some(candidate)
}

fn file_name_from_path(path: &str) -> Option<String> {
    let name = path.rsplit(['/', '\\']).next()?.trim();
    (!name.is_empty()).then(|| name.to_string())
}

fn is_media(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [".mp4", ".m4a", ".mp3", ".wav", ".webm", ".mkv", ".mov", ".ogg", ".flac", ".wma", ".vtt"]
        .iter()
        .any(|ext| lower.ends_with(ext))
}

/// Drop the query string when it carries a bearer-style token, so a
/// pre-authenticated download link never lands in a note.
fn strip_token_query(url: &str) -> String {
    let Ok(mut parsed) = url::Url::parse(url) else {
        return url.to_string();
    };
    let has_token = parsed.query_pairs().any(|(k, _)| {
        matches!(k.to_ascii_lowercase().as_str(), "tempauth" | "access_token" | "token" | "sig" | "code")
    });
    if has_token {
        parsed.set_query(None);
    }
    parsed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_a_sharepoint_file_import() {
        let url = "https://contoso-my.sharepoint.com/personal/jdoe/Documents/Recordings/Weekly%20sync-20260721_100221-Meeting%20Recording.mp4";
        let source = MeetingSource::for_import(url_import_source(url), "Weekly sync-20260721_100221-Meeting Recording", Some(url), Some("/tmp/x/download.mp4"), Some(2832.4));
        assert_eq!(source.import_source, "sharepoint");
        assert_eq!(source.recording_file.as_deref(), Some("Weekly sync-20260721_100221-Meeting Recording.mp4"));
        assert_eq!(source.recorded_at.as_deref(), Some("2026-07-21T10:02:21"));
        assert_eq!(source.recording_url.as_deref(), Some(url));
        assert_eq!(source.duration_seconds, Some(2832.4));
    }

    #[test]
    fn reads_the_file_from_a_stream_link_and_strips_tokens() {
        let url = "https://contoso.sharepoint.com/_layouts/15/stream.aspx?id=%2Fsites%2Ff%2FShared%20Documents%2FReview-20260915_140000-Meeting%20Recording.mp4";
        let source = MeetingSource::for_import("sharepoint", "Review", Some(url), None, None);
        assert_eq!(source.recording_file.as_deref(), Some("Review-20260915_140000-Meeting Recording.mp4"));
        assert_eq!(source.recorded_at.as_deref(), Some("2026-09-15T14:00:00"));
        assert_eq!(strip_token_query("https://h/a.mp4?tempauth=abc&x=1"), "https://h/a.mp4");
        assert_eq!(strip_token_query("https://h/s.aspx?id=%2Fa.mp4"), "https://h/s.aspx?id=%2Fa.mp4");
    }

    #[test]
    fn local_files_keep_their_name_and_never_invent_a_recorded_at() {
        let source = MeetingSource::for_import("file", "Interview", None, Some(r"C:\Users\me\Desktop\Interview.m4a"), Some(0.0));
        assert_eq!(source.recording_file.as_deref(), Some("Interview.m4a"));
        assert_eq!(source.recorded_at, None);
        assert_eq!(source.recording_url, None);
        assert_eq!(source.duration_seconds, None);
        assert_eq!(url_import_source("https://www.youtube.com/watch?v=1"), "url");
    }

    async fn test_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE meetings (id TEXT PRIMARY KEY, title TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE meeting_sources (meeting_id TEXT PRIMARY KEY, import_source TEXT NOT NULL, \
             recording_url TEXT, recording_file TEXT, recorded_at TEXT, duration_seconds REAL)",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    async fn insert_meeting(pool: &SqlitePool, id: &str, created_at: &str, source: &str, url: Option<&str>) {
        sqlx::query("INSERT INTO meetings (id, title, created_at, updated_at) VALUES (?, 'T', ?, ?)")
            .bind(id)
            .bind(created_at)
            .bind(created_at)
            .execute(pool)
            .await
            .unwrap();
        MeetingSource::for_import(source, "T", url, None, None)
            .insert(pool, id)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn finds_the_meeting_that_holds_the_recording_behind_a_recap_link() {
        let pool = test_pool().await;
        let recording = "https://contoso-my.sharepoint.com/personal/jdoe/Documents/Recordings/Weekly%20sync-20260721_100221-Meeting%20Recording.mp4";
        let recap = "https://teams.microsoft.com/l/meetingrecap?threadId=19%3Ax&fileUrl=https%3A%2F%2Fcontoso-my.sharepoint.com%2Fpersonal%2Fjdoe%2FDocuments%2FRecordings%2FWeekly%20sync-20260721_100221-Meeting%20Recording.mp4";
        let other_recap = "https://teams.microsoft.com/l/meetingrecap?threadId=19%3Ay";
        insert_meeting(&pool, "m-recording", "2026-07-21T15:00:00Z", "sharepoint", Some(recording)).await;
        insert_meeting(&pool, "m-local", "2026-07-22T15:00:00Z", "file", None).await;
        insert_meeting(&pool, "m-other-recap", "2026-07-23T15:00:00Z", "copilot-recap", Some(other_recap)).await;

        let key = crate::audio::teams_recap::meeting_identity_key(recap);
        assert_eq!(
            MeetingSource::find_meeting_by_identity(&pool, &key).await.unwrap().as_deref(),
            Some("m-recording")
        );
        let other_key = crate::audio::teams_recap::meeting_identity_key(other_recap);
        assert_eq!(
            MeetingSource::find_meeting_by_identity(&pool, &other_key).await.unwrap().as_deref(),
            Some("m-other-recap")
        );
        assert_eq!(
            MeetingSource::find_meeting_by_identity(&pool, "nothing").await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn ignores_sources_whose_meeting_was_deleted_and_prefers_the_oldest() {
        let pool = test_pool().await;
        let recap = "https://teams.microsoft.com/l/meetingrecap?threadId=19%3Ax&fileUrl=https%3A%2F%2Fh.sharepoint.com%2Fa%2FRec.mp4";
        insert_meeting(&pool, "m-new", "2026-07-22T15:00:00Z", "copilot-recap", Some(recap)).await;
        insert_meeting(&pool, "m-old", "2026-07-21T15:00:00Z", "copilot-recap", Some(recap)).await;
        insert_meeting(&pool, "m-gone", "2026-07-20T15:00:00Z", "copilot-recap", Some(recap)).await;
        sqlx::query("DELETE FROM meetings WHERE id = 'm-gone'").execute(&pool).await.unwrap();

        let key = crate::audio::teams_recap::meeting_identity_key(recap);
        assert_eq!(
            MeetingSource::find_meeting_by_identity(&pool, &key).await.unwrap().as_deref(),
            Some("m-old")
        );
    }

    const PART_ONE: &str = "https://h-my.sharepoint.com/personal/j/Documents/Recordings/Workshop-20261016_150059-Meeting%20Recording.mp4";
    const PART_TWO: &str = "https://h-my.sharepoint.com/personal/j/Documents/Recordings/Workshop-20261016_150059-Meeting%20Recording%202.mp4";
    const RECAP_OF_PART_TWO: &str = "https://teams.microsoft.com/l/meetingrecap?threadId=19%3Ax&fileUrl=https%3A%2F%2Fh-my.sharepoint.com%2Fpersonal%2Fj%2FDocuments%2FRecordings%2FWorkshop-20261016_150059-Meeting%20Recording%202.mp4%3Fweb%3D1";

    #[tokio::test]
    async fn a_recap_of_any_part_finds_the_meeting_holding_another_part() {
        let pool = test_pool().await;
        insert_meeting(&pool, "m-part-one", "2026-10-16T15:00:00Z", "sharepoint", Some(PART_ONE)).await;

        let key = crate::audio::teams_recap::meeting_identity_key(RECAP_OF_PART_TWO);
        assert_eq!(
            MeetingSource::find_meeting_by_identity(&pool, &key).await.unwrap().as_deref(),
            Some("m-part-one")
        );

        // A meeting holding the very file wins over an older one holding another part.
        insert_meeting(&pool, "m-part-two", "2026-10-16T19:00:00Z", "sharepoint", Some(PART_TWO)).await;
        assert_eq!(
            MeetingSource::find_meeting_by_identity(&pool, &key).await.unwrap().as_deref(),
            Some("m-part-two")
        );

        // A different stamp is a different recording.
        let other = "https://h-my.sharepoint.com/personal/j/Documents/Recordings/Workshop-20261017_150059-Meeting%20Recording.mp4";
        let other_key = crate::audio::teams_recap::meeting_identity_key(other);
        assert_eq!(MeetingSource::find_meeting_by_identity(&pool, &other_key).await.unwrap(), None);
    }

    #[tokio::test]
    async fn live_keys_cover_both_links_on_a_live_meeting_and_nothing_else() {
        let pool = test_pool().await;
        let recap_only = "https://teams.microsoft.com/l/meetingrecap?threadId=19%3Ax&fileUrl=https%3A%2F%2Fh.sharepoint.com%2Fa%2FRecapOnly-20261016_150059-Meeting%20Recording.mp4";
        let imported = "https://h.sharepoint.com/a/Imported-20261016_150059-Meeting%20Recording.mp4";
        insert_meeting(&pool, "live-file", "2026-10-16T15:00:00Z", LIVE_RECORDING_SOURCE, Some(PART_ONE)).await;
        insert_meeting(&pool, "live-no-link", "2026-10-16T15:00:00Z", LIVE_RECORDING_SOURCE, None).await;
        insert_meeting(&pool, "recap-only", "2026-10-16T15:00:00Z", "copilot-recap", Some(recap_only)).await;
        insert_meeting(&pool, "imported", "2026-10-16T15:00:00Z", "sharepoint", Some(imported)).await;

        let keys = MeetingSource::live_recording_keys(&pool).await.unwrap();
        assert_eq!(keys.len(), 1);
        let key = MeetingSource::live_key;
        assert_eq!(keys.get(&key(PART_TWO)).map(String::as_str), Some("live-file"));
        assert_eq!(keys.get(&key(RECAP_OF_PART_TWO)).map(String::as_str), Some("live-file"));
        assert!(!keys.contains_key(&key(recap_only)));
        assert!(!keys.contains_key(&key(imported)));
    }

    #[tokio::test]
    async fn an_imported_file_is_released_only_from_the_live_meeting_holding_exactly_it() {
        let pool = test_pool().await;
        insert_meeting(&pool, "live-part-one", "2026-10-16T15:00:00Z", LIVE_RECORDING_SOURCE, Some(PART_ONE)).await;
        insert_meeting(&pool, "imported-part-one", "2026-10-16T15:00:00Z", "sharepoint", Some(PART_ONE)).await;

        MeetingSource::release_from_live_meetings(&pool, PART_TWO).await.unwrap();
        assert!(MeetingSource::get(&pool, "live-part-one").await.unwrap().is_some());

        MeetingSource::release_from_live_meetings(&pool, PART_ONE).await.unwrap();
        assert_eq!(MeetingSource::get(&pool, "live-part-one").await.unwrap(), None);
        assert!(MeetingSource::get(&pool, "imported-part-one").await.unwrap().is_some());
    }
}
