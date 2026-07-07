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

        Self::reconcile_orphaned_cloud_transcript_provider_migration(&pool).await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Self::ensure_cloud_transcript_provider_columns(&pool).await?;

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

        // Define database paths
        let tauri_db_path = app_data_dir
            .join("meeting_minutes.sqlite")
            .to_string_lossy()
            .to_string();
        // Legacy backend DB path (for auto-migration if exists)
        let backend_db_path = app_data_dir
            .join("meeting_minutes.db")
            .to_string_lossy()
            .to_string();

        // WAL file paths for defensive cleanup
        let wal_path = app_data_dir.join("meeting_minutes.sqlite-wal");
        let shm_path = app_data_dir.join("meeting_minutes.sqlite-shm");

        log::info!("Tauri DB path: {}", tauri_db_path);
        log::info!("Legacy backend DB path: {}", backend_db_path);

        // Try to open database with defensive WAL handling
        match Self::new(&tauri_db_path, &backend_db_path).await {
            Ok(db_manager) => {
                log::info!("Database opened successfully");
                Ok(db_manager)
            }
            Err(e) => {
                // Check if error is due to corrupted WAL file
                let error_msg = e.to_string();
                if error_msg.contains("malformed") || error_msg.contains("corrupt") {
                    log::warn!("Database appears corrupted, likely due to orphaned WAL file. Attempting recovery...");
                    log::warn!("Error details: {}", error_msg);

                    // Delete potentially corrupted WAL/SHM files
                    if wal_path.exists() {
                        match fs::remove_file(&wal_path) {
                            Ok(_) => log::info!("Removed orphaned WAL file: {:?}", wal_path),
                            Err(e) => log::warn!("Failed to remove WAL file: {}", e),
                        }
                    }
                    if shm_path.exists() {
                        match fs::remove_file(&shm_path) {
                            Ok(_) => log::info!("Removed orphaned SHM file: {:?}", shm_path),
                            Err(e) => log::warn!("Failed to remove SHM file: {}", e),
                        }
                    }

                    // Retry connection without WAL files
                    log::info!("Retrying database connection after WAL cleanup...");
                    match Self::new(&tauri_db_path, &backend_db_path).await {
                        Ok(db_manager) => {
                            log::info!("Database opened successfully after WAL recovery");
                            Ok(db_manager)
                        }
                        Err(retry_err) => {
                            log::error!("Database connection failed even after WAL cleanup: {}", retry_err);
                            Err(retry_err)
                        }
                    }
                } else {
                    // Not a WAL-related error, propagate original error
                    log::error!("Database connection failed: {}", error_msg);
                    Err(e)
                }
            }
        }
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
