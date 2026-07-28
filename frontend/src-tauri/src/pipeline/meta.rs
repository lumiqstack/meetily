//! Retry bookkeeping for the automatic pipeline (`pipeline_meta` table).
//!
//! The work list itself is derived from the database on every tick, so this
//! table only answers "should I try this meeting right now, or is it backing
//! off / given up?". Advisory by design: wiping it just retries everything.

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use sqlx::SqlitePool;
use std::collections::HashMap;

/// Backoff ladder applied after each consecutive failure.
const BACKOFF_MINUTES: [i64; 4] = [1, 5, 15, 60];

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct MetaRow {
    pub meeting_id: String,
    pub attempts: i64,
    pub last_stage: Option<String>,
    pub last_error: Option<String>,
    pub next_retry_at: Option<String>,
    #[sqlx(rename = "suppressed")]
    pub suppressed_raw: i64,
}

impl MetaRow {
    pub fn suppressed(&self) -> bool {
        self.suppressed_raw != 0
    }

    /// Whether this meeting may be attempted at `now`.
    pub fn eligible_at(&self, now: DateTime<Utc>) -> bool {
        if self.suppressed() {
            return false;
        }
        match self.next_retry_at.as_deref() {
            None => true,
            Some(when) => match DateTime::parse_from_rfc3339(when) {
                Ok(retry_at) => now >= retry_at.with_timezone(&Utc),
                // An unparseable timestamp must not wedge the meeting.
                Err(_) => true,
            },
        }
    }
}

fn backoff_for(attempts: i64) -> Duration {
    let index = (attempts.max(1) - 1).min(BACKOFF_MINUTES.len() as i64 - 1) as usize;
    Duration::minutes(BACKOFF_MINUTES[index])
}

pub async fn load_all(pool: &SqlitePool) -> HashMap<String, MetaRow> {
    match sqlx::query_as::<_, MetaRow>(
        "SELECT meeting_id, attempts, last_stage, last_error, next_retry_at, suppressed FROM pipeline_meta",
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows
            .into_iter()
            .map(|row| (row.meeting_id.clone(), row))
            .collect(),
        Err(e) => {
            log::warn!("Failed to load pipeline retry state: {}", e);
            HashMap::new()
        }
    }
}

pub async fn load(pool: &SqlitePool, meeting_id: &str) -> Option<MetaRow> {
    sqlx::query_as::<_, MetaRow>(
        "SELECT meeting_id, attempts, last_stage, last_error, next_retry_at, suppressed \
         FROM pipeline_meta WHERE meeting_id = ?",
    )
    .bind(meeting_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
}

/// Clear all retry state for a meeting that just completed a stage.
pub async fn record_success(pool: &SqlitePool, meeting_id: &str) {
    if let Err(e) = sqlx::query("DELETE FROM pipeline_meta WHERE meeting_id = ?")
        .bind(meeting_id)
        .execute(pool)
        .await
    {
        log::warn!("Failed to clear pipeline retry state for {}: {}", meeting_id, e);
    }
}

/// Record a failed attempt and schedule the next one.
///
/// `retryable` marks transient conditions (endpoint unreachable, engine busy,
/// cancelled to yield to a recording). Those back off but are never
/// suppressed, so a summariser that is down for a day simply resumes when it
/// returns. Hard failures give up after `max_attempts` and wait for a manual
/// retry.
pub async fn record_failure(
    pool: &SqlitePool,
    meeting_id: &str,
    stage: &str,
    error: &str,
    retryable: bool,
    max_attempts: i64,
) {
    let previous = load(pool, meeting_id).await.map(|r| r.attempts).unwrap_or(0);
    let attempts = previous + 1;
    let next_retry_at = (Utc::now() + backoff_for(attempts)).to_rfc3339();
    let suppressed = if retryable { 0 } else { i64::from(attempts >= max_attempts) };
    // Keep stored errors bounded; some provider errors embed whole payloads.
    let error: String = error.chars().take(500).collect();

    if let Err(e) = sqlx::query(
        "INSERT INTO pipeline_meta (meeting_id, attempts, last_stage, last_error, next_retry_at, suppressed, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(meeting_id) DO UPDATE SET \
            attempts = excluded.attempts, last_stage = excluded.last_stage, \
            last_error = excluded.last_error, next_retry_at = excluded.next_retry_at, \
            suppressed = excluded.suppressed, updated_at = excluded.updated_at",
    )
    .bind(meeting_id)
    .bind(attempts)
    .bind(stage)
    .bind(&error)
    .bind(&next_retry_at)
    .bind(suppressed)
    .bind(Utc::now().to_rfc3339())
    .execute(pool)
    .await
    {
        log::warn!("Failed to record pipeline failure for {}: {}", meeting_id, e);
    }
}

/// Forget a meeting's failures entirely (manual "process now" / retry).
pub async fn reset(pool: &SqlitePool, meeting_id: &str) {
    record_success(pool, meeting_id).await;
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

    #[tokio::test]
    async fn unknown_meeting_has_no_retry_state() {
        let pool = test_pool().await;
        assert!(load(&pool, "never-seen").await.is_none());
    }

    #[tokio::test]
    async fn hard_failures_suppress_after_max_attempts() {
        let pool = test_pool().await;
        for _ in 0..3 {
            record_failure(&pool, "m1", "summarize", "bad template", false, 3).await;
        }
        let row = load(&pool, "m1").await.unwrap();
        assert_eq!(row.attempts, 3);
        assert!(row.suppressed());
        assert!(!row.eligible_at(Utc::now() + Duration::days(365)));
    }

    #[tokio::test]
    async fn transient_failures_back_off_but_never_give_up() {
        let pool = test_pool().await;
        for _ in 0..10 {
            record_failure(&pool, "m2", "summarize", "connection refused", true, 3).await;
        }
        let row = load(&pool, "m2").await.unwrap();
        assert!(!row.suppressed());
        // Backed off now, eligible again once the ladder's delay has passed.
        assert!(!row.eligible_at(Utc::now()));
        assert!(row.eligible_at(Utc::now() + Duration::minutes(61)));
    }

    #[tokio::test]
    async fn success_clears_previous_failures() {
        let pool = test_pool().await;
        record_failure(&pool, "m3", "transcribe", "boom", false, 3).await;
        record_success(&pool, "m3").await;
        assert!(load(&pool, "m3").await.is_none());
    }

    #[tokio::test]
    async fn backoff_grows_with_consecutive_failures() {
        let pool = test_pool().await;
        record_failure(&pool, "m4", "summarize", "boom", true, 3).await;
        let first = load(&pool, "m4").await.unwrap();
        assert!(first.eligible_at(Utc::now() + Duration::minutes(2)));

        record_failure(&pool, "m4", "summarize", "boom", true, 3).await;
        let second = load(&pool, "m4").await.unwrap();
        // Second failure waits longer than the first.
        assert!(!second.eligible_at(Utc::now() + Duration::minutes(2)));
    }
}
