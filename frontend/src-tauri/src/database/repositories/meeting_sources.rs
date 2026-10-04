use serde::{Deserialize, Serialize};
use sqlx::{FromRow, Sqlite, SqlitePool};

/// Where an imported meeting came from. Written once when the import
/// creates the meeting; read when exporting its Obsidian note.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, FromRow)]
pub struct MeetingSource {
    /// `sharepoint`, `url`, `teams-transcript`, `copilot-recap` or `file`.
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
}
