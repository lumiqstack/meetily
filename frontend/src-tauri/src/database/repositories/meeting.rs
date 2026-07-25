use crate::api::{MeetingDetails, MeetingTranscript};
use crate::database::models::{MeetingModel, PendingMeetingModel, Transcript};
use chrono::Utc;
use sqlx::{Connection, Error as SqlxError, SqliteConnection, SqlitePool};
use tracing::{error, info};

pub struct MeetingsRepository;

impl MeetingsRepository {
    pub async fn get_meetings(pool: &SqlitePool) -> Result<Vec<MeetingModel>, sqlx::Error> {
        let meetings =
            sqlx::query_as::<_, MeetingModel>("SELECT * FROM meetings ORDER BY created_at DESC")
                .fetch_all(pool)
                .await?;
        Ok(meetings)
    }

    /// Meetings with outstanding work: a recording folder but no transcripts
    /// (pending transcription), or transcripts but no completed/in-flight
    /// summary (pending AI summary). In-flight `PENDING` summaries are
    /// excluded so callers never offer to duplicate a running job.
    pub async fn get_pending_meetings(
        pool: &SqlitePool,
    ) -> Result<Vec<PendingMeetingModel>, sqlx::Error> {
        let pending = sqlx::query_as::<_, PendingMeetingModel>(
            r#"
            SELECT m.id, m.title, m.created_at, m.folder_path,
                   COALESCE(t.cnt, 0) AS transcript_count,
                   sp.status AS summary_status
            FROM meetings m
            LEFT JOIN (
                SELECT meeting_id, COUNT(*) AS cnt FROM transcripts GROUP BY meeting_id
            ) t ON t.meeting_id = m.id
            LEFT JOIN summary_processes sp ON sp.meeting_id = m.id
            WHERE (m.folder_path IS NOT NULL AND m.folder_path <> '' AND COALESCE(t.cnt, 0) = 0)
               OR (COALESCE(t.cnt, 0) > 0 AND (sp.status IS NULL OR sp.status IN ('failed', 'cancelled')))
            ORDER BY m.created_at DESC
            "#,
        )
        .fetch_all(pool)
        .await?;
        Ok(pending)
    }

    pub async fn delete_meeting(pool: &SqlitePool, meeting_id: &str) -> Result<bool, SqlxError> {
        if meeting_id.trim().is_empty() {
            return Err(SqlxError::Protocol(
                "meeting_id cannot be empty".to_string(),
            ));
        }

        let mut conn = pool.acquire().await?;
        let mut transaction = conn.begin().await?;

        match delete_meeting_with_transaction(&mut transaction, meeting_id).await {
            Ok(success) => {
                if success {
                    transaction.commit().await?;
                    info!(
                        "Successfully deleted meeting {} and all associated data",
                        meeting_id
                    );
                    Ok(true)
                } else {
                    transaction.rollback().await?;
                    Ok(false)
                }
            }
            Err(e) => {
                let _ = transaction.rollback().await;
                error!("Failed to delete meeting {}: {}", meeting_id, e);
                Err(e)
            }
        }
    }

    pub async fn get_meeting(
        pool: &SqlitePool,
        meeting_id: &str,
    ) -> Result<Option<MeetingDetails>, SqlxError> {
        if meeting_id.trim().is_empty() {
            return Err(SqlxError::Protocol(
                "meeting_id cannot be empty".to_string(),
            ));
        }

        let mut conn = pool.acquire().await?;
        let mut transaction = conn.begin().await?;

        // Get meeting details
        let meeting: Option<MeetingModel> =
            sqlx::query_as("SELECT id, title, created_at, updated_at, folder_path FROM meetings WHERE id = ?")
                .bind(meeting_id)
                .fetch_optional(&mut *transaction)
                .await?;

        if meeting.is_none() {
            transaction.rollback().await?;
            return Err(SqlxError::RowNotFound);
        }

        if let Some(meeting) = meeting {
            // Get all transcripts for this meeting
            let transcripts =
                sqlx::query_as::<_, Transcript>("SELECT * FROM transcripts WHERE meeting_id = ?")
                    .bind(meeting_id)
                    .fetch_all(&mut *transaction)
                    .await?;

            transaction.commit().await?;

            // Convert Transcript to MeetingTranscript
            let meeting_transcripts = transcripts
                .into_iter()
                .map(|t| MeetingTranscript {
                    id: t.id,
                    text: t.transcript,
                    timestamp: t.timestamp,
                    audio_start_time: t.audio_start_time,
                    audio_end_time: t.audio_end_time,
                    duration: t.duration,
                    speaker: t.speaker,
                })
                .collect::<Vec<_>>();

            Ok(Some(MeetingDetails {
                id: meeting.id,
                title: meeting.title,
                created_at: meeting.created_at.0.to_rfc3339(),
                updated_at: meeting.updated_at.0.to_rfc3339(),
                transcripts: meeting_transcripts,
            }))
        } else {
            transaction.rollback().await?;
            Ok(None)
        }
    }

    /// Get meeting metadata without transcripts (for pagination)
    pub async fn get_meeting_metadata(
        pool: &SqlitePool,
        meeting_id: &str,
    ) -> Result<Option<MeetingModel>, SqlxError> {
        if meeting_id.trim().is_empty() {
            return Err(SqlxError::Protocol(
                "meeting_id cannot be empty".to_string(),
            ));
        }

        let meeting: Option<MeetingModel> =
            sqlx::query_as("SELECT id, title, created_at, updated_at, folder_path FROM meetings WHERE id = ?")
                .bind(meeting_id)
                .fetch_optional(pool)
                .await?;

        Ok(meeting)
    }

    /// Get meeting transcripts with pagination support
    pub async fn get_meeting_transcripts_paginated(
        pool: &SqlitePool,
        meeting_id: &str,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<Transcript>, i64), SqlxError> {
        if meeting_id.trim().is_empty() {
            return Err(SqlxError::Protocol(
                "meeting_id cannot be empty".to_string(),
            ));
        }

        // Get total count of transcripts for this meeting
        let total: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM transcripts WHERE meeting_id = ?"
        )
        .bind(meeting_id)
        .fetch_one(pool)
        .await?;

        // Get paginated transcripts ordered by audio_start_time
        let transcripts = sqlx::query_as::<_, Transcript>(
            "SELECT * FROM transcripts
             WHERE meeting_id = ?
             ORDER BY audio_start_time ASC
             LIMIT ? OFFSET ?"
        )
        .bind(meeting_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?;

        Ok((transcripts, total.0))
    }

    pub async fn update_meeting_title(
        pool: &SqlitePool,
        meeting_id: &str,
        new_title: &str,
    ) -> Result<bool, SqlxError> {
        if meeting_id.trim().is_empty() {
            return Err(SqlxError::Protocol(
                "meeting_id cannot be empty".to_string(),
            ));
        }

        let mut conn = pool.acquire().await?;
        let mut transaction = conn.begin().await?;

        let now = Utc::now().naive_utc();

        let rows_affected =
            sqlx::query("UPDATE meetings SET title = ?, updated_at = ? WHERE id = ?")
                .bind(new_title)
                .bind(now)
                .bind(meeting_id)
                .execute(&mut *transaction)
                .await?;
        if rows_affected.rows_affected() == 0 {
            transaction.rollback().await?;
            return Ok(false);
        }
        transaction.commit().await?;
        Ok(true)
    }

    pub async fn update_meeting_name(
        pool: &SqlitePool,
        meeting_id: &str,
        new_title: &str,
    ) -> Result<bool, SqlxError> {
        let mut transaction = pool.begin().await?;
        let now = Utc::now();

        // Update meetings table
        let meeting_update =
            sqlx::query("UPDATE meetings SET title = ?, updated_at = ? WHERE id = ?")
                .bind(new_title)
                .bind(now)
                .bind(meeting_id)
                .execute(&mut *transaction)
                .await?;

        if meeting_update.rows_affected() == 0 {
            transaction.rollback().await?;
            return Ok(false); // Meeting not found
        }

        // Update transcript_chunks table
        sqlx::query("UPDATE transcript_chunks SET meeting_name = ? WHERE meeting_id = ?")
            .bind(new_title)
            .bind(meeting_id)
            .execute(&mut *transaction)
            .await?;

        transaction.commit().await?;
        Ok(true)
    }
}

async fn delete_meeting_with_transaction(
    transaction: &mut SqliteConnection,
    meeting_id: &str,
) -> Result<bool, SqlxError> {
    // Check if meeting exists
    let meeting_exists: Option<(i64,)> = sqlx::query_as("SELECT 1 FROM meetings WHERE id = ?")
        .bind(meeting_id)
        .fetch_optional(&mut *transaction)
        .await?;

    if meeting_exists.is_none() {
        error!("Meeting {} not found for deletion", meeting_id);
        return Ok(false);
    }

    // Delete from related tables in proper order
    // 1. Delete from transcript_chunks
    sqlx::query("DELETE FROM transcript_chunks WHERE meeting_id = ?")
        .bind(meeting_id)
        .execute(&mut *transaction)
        .await?;

    // 2. Delete from summary_processes
    sqlx::query("DELETE FROM summary_processes WHERE meeting_id = ?")
        .bind(meeting_id)
        .execute(&mut *transaction)
        .await?;

    // 3. Delete from transcripts
    sqlx::query("DELETE FROM transcripts WHERE meeting_id = ?")
        .bind(meeting_id)
        .execute(&mut *transaction)
        .await?;

    // 4. Finally, delete the meeting
    let result = sqlx::query("DELETE FROM meetings WHERE id = ?")
        .bind(meeting_id)
        .execute(&mut *transaction)
        .await?;

    Ok(result.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_pool() -> SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite pool");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("migrations must apply to a fresh database");
        pool
    }

    async fn insert_meeting(pool: &SqlitePool, id: &str, folder_path: Option<&str>) {
        sqlx::query(
            "INSERT INTO meetings (id, title, created_at, updated_at, folder_path)
             VALUES (?, ?, '2026-07-24T10:00:00Z', '2026-07-24T10:00:00Z', ?)",
        )
        .bind(id)
        .bind(format!("Meeting {}", id))
        .bind(folder_path)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn insert_transcript(pool: &SqlitePool, meeting_id: &str) {
        sqlx::query(
            "INSERT INTO transcripts (id, meeting_id, transcript, timestamp)
             VALUES (?, ?, 'hello', '2026-07-24T10:00:00Z')",
        )
        .bind(format!("t-{}", meeting_id))
        .bind(meeting_id)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn insert_summary_process(pool: &SqlitePool, meeting_id: &str, status: &str) {
        sqlx::query(
            "INSERT INTO summary_processes (meeting_id, status, created_at, updated_at)
             VALUES (?, ?, '2026-07-24T10:00:00Z', '2026-07-24T10:00:00Z')",
        )
        .bind(meeting_id)
        .bind(status)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn pending_meetings_covers_each_derived_state() {
        let pool = test_pool().await;

        // Recording but no transcripts -> pending transcription.
        insert_meeting(&pool, "needs-transcript", Some("C:/rec/a")).await;

        // Transcripts but no summary process row -> pending summary.
        insert_meeting(&pool, "needs-summary", None).await;
        insert_transcript(&pool, "needs-summary").await;

        // Transcripts with a failed summary -> pending summary (retry).
        insert_meeting(&pool, "failed-summary", Some("C:/rec/b")).await;
        insert_transcript(&pool, "failed-summary").await;
        insert_summary_process(&pool, "failed-summary", "failed").await;

        // Completed summary -> not pending.
        insert_meeting(&pool, "all-done", Some("C:/rec/c")).await;
        insert_transcript(&pool, "all-done").await;
        insert_summary_process(&pool, "all-done", "completed").await;

        // Summary already running -> excluded so it is never started twice.
        insert_meeting(&pool, "summary-running", None).await;
        insert_transcript(&pool, "summary-running").await;
        insert_summary_process(&pool, "summary-running", "PENDING").await;

        // Neither recording nor transcripts -> excluded.
        insert_meeting(&pool, "empty", None).await;

        let pending = MeetingsRepository::get_pending_meetings(&pool).await.unwrap();
        let mut ids: Vec<&str> = pending.iter().map(|p| p.id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, vec!["failed-summary", "needs-summary", "needs-transcript"]);

        let by_id = |id: &str| pending.iter().find(|p| p.id == id).unwrap();
        assert_eq!(by_id("needs-transcript").transcript_count, 0);
        assert_eq!(by_id("needs-summary").transcript_count, 1);
        assert_eq!(by_id("needs-summary").summary_status, None);
        assert_eq!(
            by_id("failed-summary").summary_status.as_deref(),
            Some("failed")
        );
    }
}
