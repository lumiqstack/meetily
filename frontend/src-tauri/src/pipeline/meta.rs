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

/// Placeholder [`begin_attempt`] leaves in `last_error` until the outcome is
/// known. Its presence tells [`record_failure`] the attempt has already been
/// counted, and it is what the user sees if the process never got that far.
const ATTEMPT_IN_FLIGHT: &str = "attempt started — the process exited before it finished";

async fn write_meta(
    pool: &SqlitePool,
    meeting_id: &str,
    attempts: i64,
    stage: &str,
    error: &str,
    next_retry_at: &str,
    suppressed: i64,
) {
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
    .bind(error)
    .bind(next_retry_at)
    .bind(suppressed)
    .bind(Utc::now().to_rfc3339())
    .execute(pool)
    .await
    {
        log::warn!("Failed to write pipeline retry state for {}: {}", meeting_id, e);
    }
}

/// Count an attempt before the stage runs, and return its number.
///
/// `record_failure` only runs if the stage returns. A stage that takes the whole
/// process down with it never reaches that call, so `attempts` stayed at zero
/// and the derived work list offered the same meeting again on the very next
/// tick — indefinitely. One 10.8-hour recording was retried eight times across
/// two days that way, aborting the app every time. Claiming the attempt up front
/// makes a crash cost an attempt like any other failure, so the backoff ladder
/// and `max_attempts` apply to it.
pub async fn begin_attempt(
    pool: &SqlitePool,
    meeting_id: &str,
    stage: &str,
    max_attempts: i64,
) -> i64 {
    let previous = load(pool, meeting_id).await.map(|r| r.attempts).unwrap_or(0);
    let attempts = previous + 1;
    let next_retry_at = (Utc::now() + backoff_for(attempts)).to_rfc3339();
    let suppressed = i64::from(attempts >= max_attempts);

    write_meta(
        pool,
        meeting_id,
        attempts,
        stage,
        ATTEMPT_IN_FLIGHT,
        &next_retry_at,
        suppressed,
    )
    .await;

    attempts
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
    let existing = load(pool, meeting_id).await;
    // A stage that ran through `begin_attempt` is already counted; anything else
    // (a pre-flight rejection, say) is counted here.
    let in_flight = existing
        .as_ref()
        .and_then(|r| r.last_error.as_deref())
        == Some(ATTEMPT_IN_FLIGHT);
    let previous = existing.map(|r| r.attempts).unwrap_or(0);
    let attempts = if in_flight { previous.max(1) } else { previous + 1 };

    let next_retry_at = (Utc::now() + backoff_for(attempts)).to_rfc3339();
    let suppressed = if retryable { 0 } else { i64::from(attempts >= max_attempts) };
    // Keep stored errors bounded; some provider errors embed whole payloads.
    let error: String = error.chars().take(500).collect();

    write_meta(
        pool,
        meeting_id,
        attempts,
        stage,
        &error,
        &next_retry_at,
        suppressed,
    )
    .await;
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

    /// The bug this exists to prevent: a stage that aborts the process never
    /// reaches `record_failure`, so the meeting was offered again every tick
    /// forever. Three crashed attempts must suppress just like three reported
    /// failures.
    #[tokio::test]
    async fn crashed_attempts_still_count_towards_the_limit() {
        let pool = test_pool().await;
        for _ in 0..3 {
            // No matching record_failure: this is what a crash looks like.
            begin_attempt(&pool, "m-crash", "transcribe", 3).await;
        }
        let row = load(&pool, "m-crash").await.unwrap();
        assert_eq!(row.attempts, 3);
        assert!(row.suppressed(), "a crash loop must eventually stop");
        assert!(!row.eligible_at(Utc::now() + Duration::days(365)));
    }

    #[tokio::test]
    async fn a_begun_attempt_that_reports_failure_counts_once() {
        let pool = test_pool().await;
        begin_attempt(&pool, "m-once", "transcribe", 3).await;
        record_failure(&pool, "m-once", "transcribe", "boom", false, 3).await;

        let row = load(&pool, "m-once").await.unwrap();
        assert_eq!(row.attempts, 1, "begin + fail is one attempt, not two");
        assert_eq!(row.last_error.as_deref(), Some("boom"));
        assert!(!row.suppressed());
    }

    /// Transient failures never give up, even after a crash counted an attempt.
    #[tokio::test]
    async fn a_transient_failure_clears_crash_suppression() {
        let pool = test_pool().await;
        for _ in 0..3 {
            begin_attempt(&pool, "m-transient", "summarize", 3).await;
        }
        assert!(load(&pool, "m-transient").await.unwrap().suppressed());

        begin_attempt(&pool, "m-transient", "summarize", 3).await;
        record_failure(&pool, "m-transient", "summarize", "connection refused", true, 3).await;

        assert!(!load(&pool, "m-transient").await.unwrap().suppressed());
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
