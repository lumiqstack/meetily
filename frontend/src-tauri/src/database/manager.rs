use sqlx::{migrate::MigrateDatabase, Result, Row, Sqlite, SqlitePool, Transaction};
use std::fs;
use std::path::Path;
use tauri::Manager;

const ORPHANED_CLOUD_TRANSCRIPT_PROVIDER_KEYS_MIGRATION_VERSION: i64 = 20260618000000;
const ORPHANED_CLOUD_TRANSCRIPT_PROVIDER_KEYS_MIGRATION_DESCRIPTION: &str =
    "add cloud transcript provider keys";

#[derive(Clone)]
pub struct DatabaseManager {
    pool: SqlitePool,
}

impl DatabaseManager {
    pub async fn new(tauri_db_path: &str, backend_db_path: &str) -> Result<Self> {
        if let Some(parent_dir) = Path::new(tauri_db_path).parent() {
            if !parent_dir.exists() {
                fs::create_dir_all(parent_dir).map_err(|e| sqlx::Error::Io(e))?;
            }
        }

        if !Path::new(tauri_db_path).exists() {
            if Path::new(backend_db_path).exists() {
                log::info!(
                    "Copying database from {} to {}",
                    backend_db_path,
                    tauri_db_path
                );
                fs::copy(backend_db_path, tauri_db_path).map_err(|e| sqlx::Error::Io(e))?;
            } else {
                log::info!("Creating database at {}", tauri_db_path);
                Sqlite::create_database(tauri_db_path).await?;
            }
        }

        let pool = SqlitePool::connect(tauri_db_path).await?;

        let initialized = async {
            Self::reconcile_orphaned_cloud_transcript_provider_migration(&pool).await?;
            sqlx::migrate!("./migrations").run(&pool).await?;
            Self::ensure_cloud_transcript_provider_columns(&pool).await
        }
        .await;
        if let Err(e) = initialized {
            // Release every connection before reporting failure so nothing
            // keeps the database files open behind the caller's back.
            pool.close().await;
            return Err(e);
        }

        Ok(DatabaseManager { pool })
    }

    // NOTE: So for the first time users they needs to start the application
    // after they can just delete the existing .sqlite file and then copy the existing .db file to
    // the current app dir, So the system detects legacy db and copy it and starts with that data
    // (Newly created .sqlite with the copied content from .db)
    pub async fn new_from_app_handle(app_handle: &tauri::AppHandle) -> Result<Self> {
        // Resolve the app's data directory
        let app_data_dir = app_handle
            .path()
            .app_data_dir()
            .expect("failed to get app data dir");
        if !app_data_dir.exists() {
            fs::create_dir_all(&app_data_dir).map_err(|e| sqlx::Error::Io(e))?;
        }

        Self::open_in_dir(&app_data_dir).await
    }

    /// Open (and migrate) `meeting_minutes.sqlite` inside `app_data_dir`.
    ///
    /// Fails closed: on any open/migration error the database set
    /// (`.sqlite`, `-wal`, `-shm`) is left exactly as found. The WAL can hold
    /// committed meetings that were never checkpointed into the main file, so
    /// deleting or moving it is never a safe automatic "recovery".
    pub(crate) async fn open_in_dir(app_data_dir: &Path) -> Result<Self> {
        let tauri_db_path = app_data_dir
            .join("meeting_minutes.sqlite")
            .to_string_lossy()
            .to_string();
        // Legacy backend DB path (for auto-migration if exists)
        let backend_db_path = app_data_dir
            .join("meeting_minutes.db")
            .to_string_lossy()
            .to_string();

        log::info!("Tauri DB path: {}", tauri_db_path);
        log::info!("Legacy backend DB path: {}", backend_db_path);

        match Self::new(&tauri_db_path, &backend_db_path).await {
            Ok(db_manager) => {
                log::info!("Database opened successfully");
                Ok(db_manager)
            }
            Err(e) => {
                let message = Self::open_failure_message(app_data_dir, &e);
                log::error!("{}", message);
                Err(sqlx::Error::Configuration(message.into()))
            }
        }
    }

    /// Actionable, data-preserving explanation for a failed database open.
    fn open_failure_message(app_data_dir: &Path, error: &sqlx::Error) -> String {
        let detail = error.to_string();
        let lower = detail.to_ascii_lowercase();
        let looks_damaged = lower.contains("malformed")
            || lower.contains("corrupt")
            || lower.contains("not a database");
        format!(
            "Meetily could not open its database in {} ({}): {}. \
             No database files were deleted or moved; meeting_minutes.sqlite and its \
             -wal/-shm files (which may contain recent meetings) are untouched. \
             Quit Meetily, copy that whole folder somewhere safe, and only then attempt \
             a repair from the copy. Do not delete the -wal file.",
            app_data_dir.display(),
            if looks_damaged { "the database appears damaged" } else { "startup or migration failed" },
            detail
        )
    }

    /// Check if this is the first launch (sqlite database doesn't exist yet)
    pub async fn is_first_launch(app_handle: &tauri::AppHandle) -> Result<bool> {
        let app_data_dir = app_handle
            .path()
            .app_data_dir()
            .expect("failed to get app data dir");

        let tauri_db_path = app_data_dir.join("meeting_minutes.sqlite");

        Ok(!tauri_db_path.exists())
    }

    /// Import a legacy database from the specified path and initialize
    pub async fn import_legacy_database(
        app_handle: &tauri::AppHandle,
        legacy_db_path: &str,
    ) -> Result<Self> {
        let app_data_dir = app_handle
            .path()
            .app_data_dir()
            .expect("failed to get app data dir");

        if !app_data_dir.exists() {
            fs::create_dir_all(&app_data_dir).map_err(|e| sqlx::Error::Io(e))?;
        }

        // Copy legacy database to app data directory as meeting_minutes.db
        let target_legacy_path = app_data_dir.join("meeting_minutes.db");
        log::info!(
            "Copying legacy database from {} to {}",
            legacy_db_path,
            target_legacy_path.display()
        );

        fs::copy(legacy_db_path, &target_legacy_path).map_err(|e| sqlx::Error::Io(e))?;

        // Now use the standard initialization which will detect and migrate the legacy db
        Self::new_from_app_handle(app_handle).await
    }

    async fn reconcile_orphaned_cloud_transcript_provider_migration(
        pool: &SqlitePool,
    ) -> Result<()> {
        if !Self::table_exists(pool, "_sqlx_migrations").await? {
            return Ok(());
        }

        let orphaned_migration: Option<(String,)> = sqlx::query_as(
            "SELECT description FROM _sqlx_migrations WHERE version = ? AND success = 1",
        )
        .bind(ORPHANED_CLOUD_TRANSCRIPT_PROVIDER_KEYS_MIGRATION_VERSION)
        .fetch_optional(pool)
        .await?;

        let Some((description,)) = orphaned_migration else {
            return Ok(());
        };

        if description != ORPHANED_CLOUD_TRANSCRIPT_PROVIDER_KEYS_MIGRATION_DESCRIPTION {
            log::warn!(
                "Found unexpected SQLx migration version {} with description '{}'; leaving it for SQLx to validate",
                ORPHANED_CLOUD_TRANSCRIPT_PROVIDER_KEYS_MIGRATION_VERSION,
                description
            );
            return Ok(());
        }

        if !Self::table_exists(pool, "transcript_settings").await? {
            log::warn!(
                "Found orphaned SQLx migration {} but transcript_settings table is missing; leaving migration metadata unchanged",
                ORPHANED_CLOUD_TRANSCRIPT_PROVIDER_KEYS_MIGRATION_VERSION
            );
            return Ok(());
        }

        log::warn!(
            "Detected orphaned SQLx migration {} ('{}'); preserving schema columns and reconciling migration metadata",
            ORPHANED_CLOUD_TRANSCRIPT_PROVIDER_KEYS_MIGRATION_VERSION,
            ORPHANED_CLOUD_TRANSCRIPT_PROVIDER_KEYS_MIGRATION_DESCRIPTION
        );

        Self::ensure_cloud_transcript_provider_columns(pool).await?;

        let result = sqlx::query(
            "DELETE FROM _sqlx_migrations WHERE version = ? AND description = ?",
        )
        .bind(ORPHANED_CLOUD_TRANSCRIPT_PROVIDER_KEYS_MIGRATION_VERSION)
        .bind(ORPHANED_CLOUD_TRANSCRIPT_PROVIDER_KEYS_MIGRATION_DESCRIPTION)
        .execute(pool)
        .await?;

        log::info!(
            "Removed orphaned SQLx migration metadata rows: {}",
            result.rows_affected()
        );

        Ok(())
    }

    async fn ensure_cloud_transcript_provider_columns(pool: &SqlitePool) -> Result<()> {
        if !Self::table_exists(pool, "transcript_settings").await? {
            return Ok(());
        }

        Self::ensure_transcript_settings_column(pool, "deepinfraApiKey", "TEXT").await?;
        Self::ensure_transcript_settings_column(pool, "openRouterApiKey", "TEXT").await?;

        Ok(())
    }

    async fn table_exists(pool: &SqlitePool, table_name: &str) -> Result<bool> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?",
        )
        .bind(table_name)
        .fetch_one(pool)
        .await?;

        Ok(count > 0)
    }

    async fn ensure_transcript_settings_column(
        pool: &SqlitePool,
        column_name: &str,
        column_type: &str,
    ) -> Result<()> {
        if Self::transcript_settings_column_exists(pool, column_name).await? {
            return Ok(());
        }

        log::info!(
            "Adding missing compatibility column transcript_settings.{}",
            column_name
        );

        let statement = format!(
            "ALTER TABLE transcript_settings ADD COLUMN {} {}",
            column_name, column_type
        );
        sqlx::query(&statement).execute(pool).await?;

        Ok(())
    }

    async fn transcript_settings_column_exists(
        pool: &SqlitePool,
        column_name: &str,
    ) -> Result<bool> {
        let rows = sqlx::query("PRAGMA table_info(transcript_settings)")
            .fetch_all(pool)
            .await?;

        for row in rows {
            let name: String = row.try_get("name")?;
            if name == column_name {
                return Ok(true);
            }
        }

        Ok(false)
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub async fn with_transaction<T, F, Fut>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Transaction<'_, Sqlite>) -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let mut tx = self.pool.begin().await?;
        let result = f(&mut tx).await;

        match result {
            Ok(val) => {
                tx.commit().await?;
                Ok(val)
            }
            Err(err) => {
                tx.rollback().await?;
                Err(err)
            }
        }
    }

    /// Cleanup database connection and checkpoint WAL
    /// This should be called on application shutdown to ensure:
    /// - All WAL changes are written to the main database file
    /// - The .wal and .shm files are deleted
    /// - Connection pool is gracefully closed
    pub async fn cleanup(&self) -> Result<()> {
        log::info!("Starting database cleanup...");

        // Force checkpoint of WAL to main database file and remove WAL file
        // TRUNCATE mode: checkpoints all pages AND deletes the WAL file
        match sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(&self.pool)
            .await
        {
            Ok(_) => log::info!("WAL checkpoint completed successfully"),
            Err(e) => log::warn!("WAL checkpoint failed (non-fatal): {}", e),
        }

        // Close the connection pool gracefully
        self.pool.close().await;
        log::info!("Database connection pool closed");

        Ok(())
    }
}

#[cfg(test)]
mod open_failure_tests {
    use super::*;
    use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
    use sqlx::ConnectOptions;
    use std::str::FromStr;

    fn read(path: &Path) -> Option<Vec<u8>> {
        fs::read(path).ok()
    }

    /// Copy of a live WAL-mode database whose committed rows are still only
    /// in the WAL: the writer stays open (no last-close checkpoint) while the
    /// set is copied. Returns (fixture dir, writer to drop afterwards).
    async fn uncheckpointed_fixture(
        setup: &[&str],
    ) -> (tempfile::TempDir, tempfile::TempDir, sqlx::SqliteConnection) {
        let source = tempfile::tempdir().unwrap();
        let source_db = source.path().join("meeting_minutes.sqlite");
        let mut writer = SqliteConnectOptions::from_str(source_db.to_str().unwrap())
            .unwrap()
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .pragma("wal_autocheckpoint", "0")
            .connect()
            .await
            .unwrap();
        sqlx::query("CREATE TABLE marker (id INTEGER PRIMARY KEY)")
            .execute(&mut writer)
            .await
            .unwrap();
        for statement in setup {
            sqlx::query(statement).execute(&mut writer).await.unwrap();
        }
        // Put the schema in the main file; only the rows below stay in the WAL.
        sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(&mut writer)
            .await
            .unwrap();
        for id in 1..=3 {
            sqlx::query("INSERT INTO marker (id) VALUES (?)")
                .bind(id)
                .execute(&mut writer)
                .await
                .unwrap();
        }
        let fixture = tempfile::tempdir().unwrap();
        for suffix in ["", "-wal", "-shm"] {
            let name = format!("meeting_minutes.sqlite{suffix}");
            fs::copy(source.path().join(&name), fixture.path().join(&name)).unwrap();
        }
        assert!(fs::metadata(fixture.path().join("meeting_minutes.sqlite-wal")).unwrap().len() > 0);
        (fixture, source, writer)
    }

    #[tokio::test]
    async fn damaged_main_file_is_left_byte_identical_with_actionable_error() {
        // No sidecars here on purpose: SQLite itself may discard an invalid
        // WAL on close, which is not application cleanup. The WAL-preservation
        // property is covered by `uncheckpointed_wal_rows_survive_a_failed_open`.
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("meeting_minutes.sqlite");
        fs::write(&main, vec![0xA5; 8192]).unwrap();
        let before = read(&main);

        let error = match DatabaseManager::open_in_dir(dir.path()).await {
            Ok(_) => panic!("garbage main file must not open"),
            Err(e) => e.to_string(),
        };

        assert!(read(&main) == before, "main file changed");
        assert!(error.contains("appears damaged"), "{error}");
        assert!(error.contains("untouched"), "{error}");
        assert!(error.contains("Do not delete the -wal file"), "{error}");
    }

    #[tokio::test]
    async fn uncheckpointed_wal_rows_survive_a_failed_open() {
        // An applied migration this build does not know makes startup fail
        // for a non-corruption reason, after SQLite has read the WAL.
        let (fixture, _source, writer) = uncheckpointed_fixture(&[
            "CREATE TABLE _sqlx_migrations (version BIGINT PRIMARY KEY, description TEXT NOT NULL, \
             installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP, success BOOLEAN NOT NULL, \
             checksum BLOB NOT NULL, execution_time BIGINT NOT NULL)",
            "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
             VALUES (99990101000000, 'from a newer build', 1, x'00', 0)",
        ])
        .await;

        assert!(DatabaseManager::open_in_dir(fixture.path()).await.is_err());

        let check = SqlitePool::connect(fixture.path().join("meeting_minutes.sqlite").to_str().unwrap())
            .await
            .unwrap();
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM marker")
            .fetch_one(&check)
            .await
            .unwrap();
        assert_eq!(rows, 3);
        check.close().await;
        drop(writer);
    }
}
