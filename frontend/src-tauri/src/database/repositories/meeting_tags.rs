use sqlx::SqlitePool;

/// Maximum tags per meeting and characters per tag; anything beyond is a
/// paste accident rather than intentional tagging.
const MAX_TAGS: usize = 50;
const MAX_TAG_CHARS: usize = 64;

pub struct MeetingTagsRepository;

impl MeetingTagsRepository {
    /// Normalize a user-entered tag into a valid Obsidian tag: no leading
    /// `#`, lowercase (Obsidian matches tags case-insensitively), whitespace
    /// runs become `-`, and only letters, digits, `_`, `-` and `/` (nested
    /// tags) survive. Returns `None` when nothing usable remains or the tag
    /// is purely numeric, which Obsidian does not accept as a tag.
    pub fn normalize_tag(raw: &str) -> Option<String> {
        let tag = raw
            .trim()
            .trim_start_matches('#')
            .split_whitespace()
            .collect::<Vec<_>>()
            .join("-")
            .to_lowercase()
            .chars()
            .filter(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '/'))
            .take(MAX_TAG_CHARS)
            .collect::<String>();
        let tag = tag.trim_matches('/').to_string();
        if tag.is_empty() || tag.chars().all(|c| c.is_ascii_digit()) {
            None
        } else {
            Some(tag)
        }
    }

    /// Normalize and dedupe, preserving first-seen order.
    pub fn normalize_tags(raw: &[String]) -> Vec<String> {
        let mut tags: Vec<String> = Vec::new();
        for tag in raw.iter().filter_map(|t| Self::normalize_tag(t)) {
            if !tags.contains(&tag) {
                tags.push(tag);
            }
        }
        tags.truncate(MAX_TAGS);
        tags
    }

    pub async fn get_tags(pool: &SqlitePool, meeting_id: &str) -> Result<Vec<String>, sqlx::Error> {
        sqlx::query_scalar::<_, String>(
            "SELECT tag FROM meeting_tags WHERE meeting_id = ? ORDER BY rowid",
        )
        .bind(meeting_id)
        .fetch_all(pool)
        .await
    }

    /// Replace the meeting's tags with `tags` (normalized). Returns the
    /// stored set.
    pub async fn set_tags(
        pool: &SqlitePool,
        meeting_id: &str,
        tags: &[String],
    ) -> Result<Vec<String>, sqlx::Error> {
        let tags = Self::normalize_tags(tags);
        let mut tx = pool.begin().await?;
        sqlx::query("DELETE FROM meeting_tags WHERE meeting_id = ?")
            .bind(meeting_id)
            .execute(&mut *tx)
            .await?;
        for tag in &tags {
            sqlx::query("INSERT INTO meeting_tags (meeting_id, tag) VALUES (?, ?)")
                .bind(meeting_id)
                .bind(tag)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(tags)
    }

    /// Every tag in use, most used first — for autocomplete.
    pub async fn all_tags(pool: &SqlitePool) -> Result<Vec<String>, sqlx::Error> {
        sqlx::query_scalar::<_, String>(
            "SELECT tag FROM meeting_tags GROUP BY tag ORDER BY COUNT(*) DESC, tag",
        )
        .fetch_all(pool)
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_user_input_into_obsidian_tags() {
        assert_eq!(MeetingTagsRepository::normalize_tag("#Client Work"), Some("client-work".into()));
        assert_eq!(MeetingTagsRepository::normalize_tag("  mercado/libre "), Some("mercado/libre".into()));
        assert_eq!(MeetingTagsRepository::normalize_tag("revisión"), Some("revisión".into()));
        assert_eq!(MeetingTagsRepository::normalize_tag("a,b!c"), Some("abc".into()));
        assert_eq!(MeetingTagsRepository::normalize_tag("2026"), None);
        assert_eq!(MeetingTagsRepository::normalize_tag("  # "), None);
    }

    #[test]
    fn dedupes_after_normalizing() {
        let tags = MeetingTagsRepository::normalize_tags(&[
            "Clients".into(),
            "#clients".into(),
            "q4".into(),
        ]);
        assert_eq!(tags, vec!["clients".to_string(), "q4".to_string()]);
    }
}
