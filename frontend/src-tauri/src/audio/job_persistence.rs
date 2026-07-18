// Durable journal for background batch jobs (imports, retranscriptions).
//
// The in-memory `JobRegistry` vanishes when the app quits, so a crash
// mid-import used to silently drop the job and leave a half-built meeting
// folder behind (NEXT_IMPROVEMENTS.md #7). This module persists one row per
// in-flight job in the `background_jobs` table: the row is inserted when the
// job starts and deleted on every clean finish (success, failure, and
// cancellation are all reported to the user live). Any row still present at
// the next startup therefore belongs to a job the app died under —
// `reconcile_interrupted_jobs` marks those rows so the frontend can offer
// retry/dismiss, and removes orphaned import folders.

use crate::state::AppState;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tauri::{AppHandle, Manager, Runtime};

/// One persisted background job, as recorded at job start.
///
/// `id` is the job key used by the in-memory registry: the import ID for
/// imports, the meeting ID for retranscriptions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
pub struct PersistedJob {
    pub id: String,
    /// "import" or "retranscription".
    pub kind: String,
    pub title: String,
    /// Import: the original file the user selected (used for retry).
    pub source_path: Option<String>,
    /// The meeting folder the job writes into. Imports learn this after
    /// creating the folder; retranscriptions know it upfront.
    pub folder_path: Option<String>,
    /// Retranscription: the meeting whose transcript is being replaced.
    pub meeting_id: Option<String>,
    pub language: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    /// RFC3339 timestamp of when the job started.
    pub created_at: String,
}

/// What startup reconciliation found and did.
#[derive(Debug, Default, PartialEq)]
pub struct ReconcileOutcome {
    /// Jobs that were still marked running — the app died under them.
    pub interrupted: Vec<PersistedJob>,
    /// Orphaned import meeting folders that were removed.
    pub removed_folders: Vec<String>,
}

const JOB_COLUMNS: &str =
    "id, kind, title, source_path, folder_path, meeting_id, language, model, provider, created_at";

/// Record a job that is about to start. Called right after the in-memory
/// registry accepts the job.
pub async fn record_job_started(pool: &SqlitePool, job: &PersistedJob) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT OR REPLACE INTO background_jobs \
         (id, kind, title, source_path, folder_path, meeting_id, language, model, provider, interrupted, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 0, ?)",
    )
    .bind(&job.id)
    .bind(&job.kind)
    .bind(&job.title)
    .bind(&job.source_path)
    .bind(&job.folder_path)
    .bind(&job.meeting_id)
    .bind(&job.language)
    .bind(&job.model)
    .bind(&job.provider)
    .bind(&job.created_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// Record the meeting folder a running job writes into. Imports create their
/// folder mid-job, after the row was inserted.
pub async fn set_job_folder(
    pool: &SqlitePool,
    job_id: &str,
    folder_path: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE background_jobs SET folder_path = ? WHERE id = ?")
        .bind(folder_path)
        .bind(job_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Remove a job's row after it finished in-process (success, failure, or
/// cancellation — all of those are already reported to the user live).
pub async fn clear_job(pool: &SqlitePool, job_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM background_jobs WHERE id = ?")
        .bind(job_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Startup reconciliation: every row still present belongs to a job the
/// previous process died under. Marks those rows interrupted (so
/// `list_interrupted_jobs` keeps reporting them until the user acts) and
/// returns them.
pub async fn reconcile_interrupted_jobs(
    pool: &SqlitePool,
) -> Result<ReconcileOutcome, sqlx::Error> {
    sqlx::query("UPDATE background_jobs SET interrupted = 1 WHERE interrupted = 0")
        .execute(pool)
        .await?;

    let interrupted = list_interrupted_jobs(pool).await?;

    let mut removed_folders = Vec::new();
    for job in &interrupted {
        // Only imports create their folder — a retranscription's folder is a
        // pre-existing meeting and must never be deleted.
        if job.kind != "import" {
            continue;
        }
        let Some(folder) = job.folder_path.as_deref() else {
            continue;
        };
        if !std::path::Path::new(folder).exists() {
            continue;
        }
        // metadata.json marks a fully saved meeting: the folder is data,
        // not a half-built orphan.
        if std::path::Path::new(folder).join("metadata.json").exists() {
            continue;
        }
        // The meeting may have been committed right before the crash
        // (metadata.json is written after the DB transaction). Compared with
        // normalization, not string equality: a rendering difference between
        // the two writers must never hide the reference (a miss here deletes
        // the folder of a real meeting). A DB error aborts reconciliation
        // before any deletion — fail-safe.
        let meeting_folders: Vec<String> =
            sqlx::query_scalar("SELECT folder_path FROM meetings WHERE folder_path IS NOT NULL")
                .fetch_all(pool)
                .await?;
        if meeting_folders.iter().any(|m| is_same_path(m, folder)) {
            continue;
        }
        match std::fs::remove_dir_all(folder) {
            Ok(()) => removed_folders.push(folder.to_string()),
            Err(e) => log::warn!("Failed to remove orphaned import folder {folder}: {e}"),
        }
    }

    Ok(ReconcileOutcome {
        interrupted,
        removed_folders,
    })
}

/// True when two recorded paths refer to the same folder even if the writers
/// rendered them differently (e.g. `C:/x/y` vs `C:\x\y`, a trailing
/// separator, or — on Windows, where filesystems are case-insensitive — a
/// case difference). Pure comparison that never errors, so the check stays
/// fail-safe.
fn is_same_path(a: &str, b: &str) -> bool {
    normalized_components(a) == normalized_components(b)
}

fn normalized_components(path: &str) -> Vec<String> {
    // `\\?\C:\x` and `C:\x` name the same folder; strip the verbatim prefix
    // so their drive components compare equal.
    let path = path.strip_prefix(r"\\?\").unwrap_or(path);
    std::path::Path::new(path)
        .components()
        .map(|c| {
            let component = c.as_os_str().to_string_lossy();
            if cfg!(windows) {
                component.to_lowercase()
            } else {
                component.into_owned()
            }
        })
        .collect()
}

/// Drop an interrupted job the user has dismissed (or retried — the retry
/// runs as a brand-new job with its own row).
///
/// Only rows still flagged interrupted are deleted: if a rerun job reused the
/// ID in the meantime (retranscriptions are keyed by meeting ID), its row
/// belongs to the running job and must survive the dismissal of the stale
/// notice.
pub async fn dismiss_interrupted_job(
    pool: &SqlitePool,
    job_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM background_jobs WHERE id = ? AND interrupted = 1")
        .bind(job_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Jobs a past process died under, still awaiting user retry/dismiss.
pub async fn list_interrupted_jobs(pool: &SqlitePool) -> Result<Vec<PersistedJob>, sqlx::Error> {
    sqlx::query_as::<_, PersistedJob>(&format!(
        "SELECT {JOB_COLUMNS} FROM background_jobs WHERE interrupted = 1 ORDER BY created_at"
    ))
    .fetch_all(pool)
    .await
}

// ============================================================================
// Best-effort wrappers for the job pipelines
//
// A journal failure must never fail the job itself, so these log and move on.
// They also tolerate the app state not being managed yet (unit tests, early
// startup).
// ============================================================================

fn pool_of<R: Runtime>(app: &AppHandle<R>) -> Option<SqlitePool> {
    app.try_state::<AppState>()
        .map(|state| state.db_manager.pool().clone())
}

pub(crate) async fn try_record_job_started<R: Runtime>(app: &AppHandle<R>, job: &PersistedJob) {
    let Some(pool) = pool_of(app) else { return };
    if let Err(e) = record_job_started(&pool, job).await {
        log::warn!("Failed to journal start of {} {}: {}", job.kind, job.id, e);
    }
}

pub(crate) async fn try_set_job_folder<R: Runtime>(
    app: &AppHandle<R>,
    job_id: &str,
    folder_path: &str,
) {
    let Some(pool) = pool_of(app) else { return };
    if let Err(e) = set_job_folder(&pool, job_id, folder_path).await {
        log::warn!("Failed to journal folder of job {}: {}", job_id, e);
    }
}

pub(crate) async fn try_clear_job<R: Runtime>(app: &AppHandle<R>, job_id: &str) {
    let Some(pool) = pool_of(app) else { return };
    if let Err(e) = clear_job(&pool, job_id).await {
        log::warn!("Failed to journal finish of job {}: {}", job_id, e);
    }
}

// ============================================================================
// Tauri Commands
// ============================================================================

/// List jobs a previous app process died under (for the startup notice).
#[tauri::command]
pub async fn list_interrupted_jobs_command<R: Runtime>(
    app: AppHandle<R>,
) -> Result<Vec<PersistedJob>, String> {
    let pool = pool_of(&app).ok_or("App state not available")?;
    list_interrupted_jobs(&pool).await.map_err(|e| e.to_string())
}

/// Drop an interrupted job the user dismissed or retried.
#[tauri::command]
pub async fn dismiss_interrupted_job_command<R: Runtime>(
    app: AppHandle<R>,
    job_id: String,
) -> Result<(), String> {
    let pool = pool_of(&app).ok_or("App state not available")?;
    dismiss_interrupted_job(&pool, &job_id)
        .await
        .map_err(|e| e.to_string())
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

    fn import_job(id: &str) -> PersistedJob {
        PersistedJob {
            id: id.to_string(),
            kind: "import".to_string(),
            title: "Quarterly Review.mp4".to_string(),
            source_path: Some("C:/recordings/quarterly-review.mp4".to_string()),
            folder_path: None,
            meeting_id: None,
            language: Some("en".to_string()),
            model: Some("large-v3".to_string()),
            provider: Some("openaiCompatible".to_string()),
            created_at: "2026-07-16T10:00:00Z".to_string(),
        }
    }

    #[tokio::test]
    async fn job_running_at_crash_is_reported_as_interrupted_after_restart() {
        let pool = test_pool().await;
        let job = import_job("import-crashed");
        record_job_started(&pool, &job).await.unwrap();

        // The app dies here: nothing clears the row. Next launch reconciles.
        let outcome = reconcile_interrupted_jobs(&pool).await.unwrap();
        assert_eq!(outcome.interrupted, vec![job.clone()]);
        assert_eq!(outcome.removed_folders, Vec::<String>::new());

        // The frontend can query the interrupted job later in the session.
        assert_eq!(list_interrupted_jobs(&pool).await.unwrap(), vec![job]);
    }

    #[tokio::test]
    async fn set_job_folder_records_the_folder_for_crash_reporting() {
        let pool = test_pool().await;
        let mut job = import_job("import-with-folder");
        assert_eq!(job.folder_path, None, "imports start without a folder");
        record_job_started(&pool, &job).await.unwrap();

        // Mid-job, the import creates its meeting folder.
        set_job_folder(&pool, "import-with-folder", "C:/recordings/quarterly-review").await.unwrap();

        // A crash after that point must report the folder.
        let outcome = reconcile_interrupted_jobs(&pool).await.unwrap();
        job.folder_path = Some("C:/recordings/quarterly-review".to_string());
        assert_eq!(outcome.interrupted, vec![job]);
    }

    fn retranscription_job(meeting_id: &str) -> PersistedJob {
        PersistedJob {
            id: meeting_id.to_string(),
            kind: "retranscription".to_string(),
            title: "Weekly Sync".to_string(),
            source_path: None,
            folder_path: Some("C:/recordings/weekly-sync".to_string()),
            meeting_id: Some(meeting_id.to_string()),
            language: None,
            model: None,
            provider: None,
            created_at: "2026-07-16T11:00:00Z".to_string(),
        }
    }

    #[tokio::test]
    async fn dismiss_does_not_touch_a_rerun_job_that_reused_the_id() {
        let pool = test_pool().await;
        // A retranscription of meeting-7 was interrupted by a crash...
        record_job_started(&pool, &retranscription_job("meeting-7")).await.unwrap();
        reconcile_interrupted_jobs(&pool).await.unwrap();

        // ...and the user reruns the retranscription (jobs are keyed by
        // meeting ID, so the ID repeats) before acting on the stale notice.
        let rerun = retranscription_job("meeting-7");
        record_job_started(&pool, &rerun).await.unwrap();
        assert_eq!(
            list_interrupted_jobs(&pool).await.unwrap(),
            vec![],
            "re-recording a job must supersede its stale interrupted notice"
        );

        // Dismissing the stale notice now must not delete the running job's
        // row — if the app crashed at this point, the rerun must still be
        // reported on the next launch.
        dismiss_interrupted_job(&pool, "meeting-7").await.unwrap();
        let outcome = reconcile_interrupted_jobs(&pool).await.unwrap();
        assert_eq!(outcome.interrupted, vec![rerun]);
    }

    #[tokio::test]
    async fn reconcile_removes_the_orphaned_folder_of_a_crashed_import() {
        let pool = test_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("Quarterly Review");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("audio.mp4"), b"partial copy").unwrap();

        // The import copied its audio and then the app died: the folder has
        // no metadata.json and no meeting row references it.
        let mut job = import_job("import-orphan");
        job.folder_path = Some(folder.to_string_lossy().to_string());
        record_job_started(&pool, &job).await.unwrap();

        let outcome = reconcile_interrupted_jobs(&pool).await.unwrap();

        assert!(!folder.exists(), "orphaned import folder must be removed");
        assert_eq!(
            outcome.removed_folders,
            vec![folder.to_string_lossy().to_string()]
        );
        // The job is still reported so the user can retry from the source file.
        assert_eq!(outcome.interrupted, vec![job]);

        // A second launch finds the folder already gone and stays quiet.
        let second = reconcile_interrupted_jobs(&pool).await.unwrap();
        assert_eq!(second.removed_folders, Vec::<String>::new());
    }

    #[tokio::test]
    async fn reconcile_keeps_a_folder_that_already_has_completed_content() {
        let pool = test_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("Finished Meeting");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("audio.mp4"), b"audio").unwrap();
        // metadata.json is written once the meeting is fully saved — the
        // crash happened after the interesting work, so the folder is data,
        // not garbage.
        std::fs::write(folder.join("metadata.json"), b"{}").unwrap();

        let mut job = import_job("import-finished-content");
        job.folder_path = Some(folder.to_string_lossy().to_string());
        record_job_started(&pool, &job).await.unwrap();

        let outcome = reconcile_interrupted_jobs(&pool).await.unwrap();

        assert!(folder.exists(), "a folder with metadata.json must be kept");
        assert_eq!(outcome.removed_folders, Vec::<String>::new());
    }

    #[tokio::test]
    async fn reconcile_keeps_a_folder_a_meeting_row_points_at() {
        let pool = test_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("Committed Meeting");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("audio.mp4"), b"audio").unwrap();
        let folder_str = folder.to_string_lossy().to_string();

        // The crash landed between the meeting's DB commit and the
        // metadata.json write: no completion marker on disk, but the folder
        // belongs to a real meeting now.
        sqlx::query(
            "INSERT INTO meetings (id, title, created_at, updated_at, folder_path)
             VALUES ('meeting-committed', 'Committed Meeting', '2026-07-16T10:00:00Z', '2026-07-16T10:00:00Z', ?)",
        )
        .bind(&folder_str)
        .execute(&pool)
        .await
        .unwrap();

        let mut job = import_job("import-committed");
        job.folder_path = Some(folder_str);
        record_job_started(&pool, &job).await.unwrap();

        let outcome = reconcile_interrupted_jobs(&pool).await.unwrap();

        assert!(
            folder.exists(),
            "a folder referenced by a meeting row must be kept"
        );
        assert_eq!(outcome.removed_folders, Vec::<String>::new());
    }

    #[tokio::test]
    async fn reconcile_keeps_a_committed_folder_despite_mixed_path_separators() {
        let pool = test_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("Committed Meeting");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("audio.mp4"), b"audio").unwrap();
        let native = folder.to_string_lossy().to_string();

        // The meeting row holds the native form while the journal recorded
        // the same folder rendered with forward slashes. The guard must still
        // see the reference — otherwise the folder of a committed meeting
        // gets deleted whenever metadata.json also failed to write.
        sqlx::query(
            "INSERT INTO meetings (id, title, created_at, updated_at, folder_path)
             VALUES ('meeting-mixed-sep', 'Committed Meeting', '2026-07-16T10:00:00Z', '2026-07-16T10:00:00Z', ?)",
        )
        .bind(&native)
        .execute(&pool)
        .await
        .unwrap();

        let mut job = import_job("import-mixed-separators");
        job.folder_path = Some(native.replace('\\', "/"));
        record_job_started(&pool, &job).await.unwrap();

        let outcome = reconcile_interrupted_jobs(&pool).await.unwrap();

        assert!(
            folder.exists(),
            "mixed separators must not defeat the meeting-reference guard"
        );
        assert_eq!(outcome.removed_folders, Vec::<String>::new());
    }

    #[tokio::test]
    async fn reconcile_keeps_a_committed_folder_despite_a_trailing_separator() {
        let pool = test_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("Committed Meeting");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("audio.mp4"), b"audio").unwrap();
        let native = folder.to_string_lossy().to_string();

        // The meeting row carries a trailing separator the journal lacks.
        sqlx::query(
            "INSERT INTO meetings (id, title, created_at, updated_at, folder_path)
             VALUES ('meeting-trailing-sep', 'Committed Meeting', '2026-07-16T10:00:00Z', '2026-07-16T10:00:00Z', ?)",
        )
        .bind(format!("{native}{}", std::path::MAIN_SEPARATOR))
        .execute(&pool)
        .await
        .unwrap();

        let mut job = import_job("import-trailing-separator");
        job.folder_path = Some(native);
        record_job_started(&pool, &job).await.unwrap();

        let outcome = reconcile_interrupted_jobs(&pool).await.unwrap();

        assert!(
            folder.exists(),
            "a trailing separator must not defeat the meeting-reference guard"
        );
        assert_eq!(outcome.removed_folders, Vec::<String>::new());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn reconcile_keeps_a_committed_folder_despite_a_case_difference() {
        let pool = test_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("Committed Meeting");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("audio.mp4"), b"audio").unwrap();
        let native = folder.to_string_lossy().to_string();

        // Windows filesystems are case-insensitive: a meeting row written
        // with different casing still names the same folder on disk.
        sqlx::query(
            "INSERT INTO meetings (id, title, created_at, updated_at, folder_path)
             VALUES ('meeting-case-diff', 'Committed Meeting', '2026-07-16T10:00:00Z', '2026-07-16T10:00:00Z', ?)",
        )
        .bind(native.to_uppercase())
        .execute(&pool)
        .await
        .unwrap();

        let mut job = import_job("import-case-difference");
        job.folder_path = Some(native);
        record_job_started(&pool, &job).await.unwrap();

        let outcome = reconcile_interrupted_jobs(&pool).await.unwrap();

        assert!(
            folder.exists(),
            "a case difference must not defeat the meeting-reference guard on Windows"
        );
        assert_eq!(outcome.removed_folders, Vec::<String>::new());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn reconcile_keeps_a_committed_folder_despite_a_verbatim_prefix() {
        let pool = test_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("Committed Meeting");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("audio.mp4"), b"audio").unwrap();
        let native = folder.to_string_lossy().to_string();

        // APIs like `canonicalize` render Windows paths with the `\\?\`
        // verbatim prefix; the path still names the same folder.
        sqlx::query(
            "INSERT INTO meetings (id, title, created_at, updated_at, folder_path)
             VALUES ('meeting-verbatim', 'Committed Meeting', '2026-07-16T10:00:00Z', '2026-07-16T10:00:00Z', ?)",
        )
        .bind(format!(r"\\?\{native}"))
        .execute(&pool)
        .await
        .unwrap();

        let mut job = import_job("import-verbatim-prefix");
        job.folder_path = Some(native);
        record_job_started(&pool, &job).await.unwrap();

        let outcome = reconcile_interrupted_jobs(&pool).await.unwrap();

        assert!(
            folder.exists(),
            "a \\\\?\\ verbatim prefix must not defeat the meeting-reference guard"
        );
        assert_eq!(outcome.removed_folders, Vec::<String>::new());
    }

    #[tokio::test]
    async fn reconcile_never_deletes_a_retranscription_folder() {
        let pool = test_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("Existing Meeting");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("audio.mp4"), b"the only copy").unwrap();

        // A retranscription works on a pre-existing meeting folder. Even if
        // no meeting row matches the recorded path (path normalization,
        // edited DB, ...), an interrupted retranscription must never take
        // the user's audio with it.
        let mut job = retranscription_job("meeting-retr-folder");
        job.folder_path = Some(folder.to_string_lossy().to_string());
        record_job_started(&pool, &job).await.unwrap();

        let outcome = reconcile_interrupted_jobs(&pool).await.unwrap();

        assert!(
            folder.exists(),
            "retranscription folders must never be deleted"
        );
        assert_eq!(outcome.removed_folders, Vec::<String>::new());
        assert_eq!(outcome.interrupted, vec![job]);
    }

    #[tokio::test]
    async fn cleanly_finished_job_is_not_reported_after_restart() {
        let pool = test_pool().await;
        record_job_started(&pool, &import_job("import-clean")).await.unwrap();

        // The job finished in-process (success, failure, or cancellation —
        // all already reported live), so its row is cleared.
        clear_job(&pool, "import-clean").await.unwrap();

        let outcome = reconcile_interrupted_jobs(&pool).await.unwrap();
        assert_eq!(outcome.interrupted, vec![]);
    }

    #[tokio::test]
    async fn job_started_after_reconcile_is_not_reported_as_interrupted() {
        let pool = test_pool().await;
        reconcile_interrupted_jobs(&pool).await.unwrap();

        // A job started in this session is running, not interrupted.
        let job = import_job("import-running-now");
        record_job_started(&pool, &job).await.unwrap();

        assert_eq!(list_interrupted_jobs(&pool).await.unwrap(), vec![]);
    }

    #[tokio::test]
    async fn dismissed_interrupted_job_is_gone_for_good() {
        let pool = test_pool().await;
        record_job_started(&pool, &import_job("import-dismissed")).await.unwrap();
        reconcile_interrupted_jobs(&pool).await.unwrap();

        dismiss_interrupted_job(&pool, "import-dismissed").await.unwrap();

        assert_eq!(list_interrupted_jobs(&pool).await.unwrap(), vec![]);
        // A later restart must not resurrect it either.
        let outcome = reconcile_interrupted_jobs(&pool).await.unwrap();
        assert_eq!(outcome.interrupted, vec![]);
    }
}
