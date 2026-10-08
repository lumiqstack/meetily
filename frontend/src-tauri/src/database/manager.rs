use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous,
};
use sqlx::migrate::{MigrateDatabase, Migrator};
use sqlx::{Result, Row, Sqlite, SqliteConnection, SqlitePool, Transaction};
use std::fs;
use std::path::Path;
use std::time::Duration;

const ORPHANED_CLOUD_TRANSCRIPT_PROVIDER_KEYS_MIGRATION_VERSION: i64 = 20260618000000;
const ORPHANED_CLOUD_TRANSCRIPT_PROVIDER_KEYS_MIGRATION_DESCRIPTION: &str =
    "add cloud transcript provider keys";
/// Name prefix for a legacy column parked while migrations run; see
/// `park_legacy_columns_that_migrations_add`.
const PARKED_COLUMN_PREFIX: &str = "__legacy_park_";

fn quote_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

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

        // The file is created above, so `create_if_missing` stays off: a path
        // that is still absent here is a real error, not a fresh install.
        // `synchronous = NORMAL` is only durable under WAL, so the two are set
        // together.
        let connect_options = SqliteConnectOptions::new()
            .filename(tauri_db_path)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(Duration::from_secs(15));

        // SQLite takes one writer at a time; extra connections queue on the
        // write lock instead of adding throughput.
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(connect_options)
            .await?;

        let initialized = async {
            Self::reconcile_orphaned_cloud_transcript_provider_migration(&pool).await?;
            let migrator = sqlx::migrate!("./migrations");
            if !Self::table_exists(&pool, "_sqlx_migrations").await? {
                Self::park_legacy_columns_that_migrations_add(&pool, &migrator).await?;
            }
            migrator.run(&pool).await?;
            Self::restore_parked_legacy_columns(&pool).await?;
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
    pub async fn new_from_app_handle<R: tauri::Runtime>(_app_handle: &tauri::AppHandle<R>) -> Result<Self> {
        // Resolve the configured data root (falls back to app_data_dir)
        let app_data_dir = crate::storage::db_dir();

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
    pub async fn is_first_launch(_app_handle: &tauri::AppHandle) -> Result<bool> {
        let tauri_db_path = crate::storage::db_dir().join("meeting_minutes.sqlite");

        Ok(!tauri_db_path.exists())
    }

    /// Import a legacy database from the specified path and initialize
    pub async fn import_legacy_database<R: tauri::Runtime>(
        app_handle: &tauri::AppHandle<R>,
        legacy_db_path: &str,
    ) -> Result<Self> {
        let app_data_dir = crate::storage::db_dir();

        // Copy legacy database to the data root as meeting_minutes.db
        let target_legacy_path = app_data_dir.join("meeting_minutes.db");

        // Onboarding passes the default location itself, which is already the
        // target. `fs::copy` onto the same file truncates it to zero bytes on
        // Unix, so the file is initialized in place instead.
        if Self::is_same_file(Path::new(legacy_db_path), &target_legacy_path) {
            log::info!(
                "Legacy database is already at {}; initializing it in place",
                target_legacy_path.display()
            );
        } else {
            log::info!(
                "Copying legacy database from {} to {}",
                legacy_db_path,
                target_legacy_path.display()
            );
            fs::copy(legacy_db_path, &target_legacy_path).map_err(|e| sqlx::Error::Io(e))?;
        }

        // Now use the standard initialization which will detect and migrate the legacy db
        Self::new_from_app_handle(app_handle).await
    }

    /// True only when both paths exist and resolve to the same file. Compared
    /// after canonicalization so that symlinks and differently spelled paths
    /// are caught too.
    fn is_same_file(a: &Path, b: &Path) -> bool {
        match (fs::canonicalize(a), fs::canonicalize(b)) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        }
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

    /// Databases written by the archived Python backend (backend/app/db.py
    /// `_legacy_init_db`) have no `_sqlx_migrations` table, so sqlx runs every
    /// migration on them. Some migrations `ADD COLUMN` a column the backend
    /// already created (e.g. `meetings.folder_path`), and that statement fails
    /// with "duplicate column name" on every launch.
    ///
    /// The colliding columns are renamed out of the way before migrating and
    /// copied back afterwards. A rename keeps the values in the file, so a
    /// migration that fails leaves them recoverable; the next successful open
    /// restores them. The migration files themselves are never touched.
    async fn park_legacy_columns_that_migrations_add(
        pool: &SqlitePool,
        migrator: &Migrator,
    ) -> Result<()> {
        let mut tx = pool.begin().await?;
        for (table, column) in Self::migration_added_columns(migrator) {
            if !Self::table_columns(&mut *tx, &table)
                .await?
                .contains(&column)
            {
                continue;
            }
            log::warn!(
                "Legacy database already has {}.{}; parking it so migrations can add it",
                table,
                column
            );
            let parked = format!("{PARKED_COLUMN_PREFIX}{column}");
            sqlx::query(&format!(
                "ALTER TABLE {} RENAME COLUMN {} TO {}",
                quote_identifier(&table),
                quote_identifier(&column),
                quote_identifier(&parked)
            ))
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await
    }

    /// Copy every parked column back into the column a migration re-added, then
    /// drop the parked copy. Runs after each successful migration pass, so it
    /// also finishes a restore that an earlier failed open left behind.
    async fn restore_parked_legacy_columns(pool: &SqlitePool) -> Result<()> {
        let mut tx = pool.begin().await?;
        let tables: Vec<String> =
            sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table'")
                .fetch_all(&mut *tx)
                .await?;
        for table in tables {
            let columns = Self::table_columns(&mut *tx, &table).await?;
            for parked in columns
                .iter()
                .filter(|c| c.starts_with(PARKED_COLUMN_PREFIX))
            {
                let column = &parked[PARKED_COLUMN_PREFIX.len()..];
                if !columns.iter().any(|c| c == column) {
                    return Err(sqlx::Error::Configuration(
                        format!("parked column {table}.{parked} has no migrated replacement")
                            .into(),
                    ));
                }
                log::info!("Restoring legacy values for {}.{}", table, column);
                sqlx::query(&format!(
                    "UPDATE {} SET {} = {}",
                    quote_identifier(&table),
                    quote_identifier(column),
                    quote_identifier(parked)
                ))
                .execute(&mut *tx)
                .await?;
                sqlx::query(&format!(
                    "ALTER TABLE {} DROP COLUMN {}",
                    quote_identifier(&table),
                    quote_identifier(parked)
                ))
                .execute(&mut *tx)
                .await?;
            }
        }
        tx.commit().await
    }

    /// `(table, column)` for every `ADD COLUMN` in the bundled migrations.
    /// Parsed from the SQL so that future migrations are covered without a list
    /// to maintain.
    fn migration_added_columns(migrator: &Migrator) -> Vec<(String, String)> {
        let mut added = Vec::new();
        for migration in migrator.iter() {
            let code: String = migration
                .sql
                .lines()
                .map(|line| line.split("--").next().unwrap_or(""))
                .collect::<Vec<_>>()
                .join(" ");
            let spaced = code.replace(';', " ; ").replace(',', " ");
            let tokens: Vec<&str> = spaced.split_whitespace().collect();
            let mut table: Option<&str> = None;
            for (i, token) in tokens.iter().enumerate() {
                if *token == ";" {
                    table = None;
                } else if token.eq_ignore_ascii_case("TABLE")
                    && i > 0
                    && tokens[i - 1].eq_ignore_ascii_case("ALTER")
                {
                    table = tokens.get(i + 1).copied();
                } else if token.eq_ignore_ascii_case("ADD")
                    && tokens
                        .get(i + 1)
                        .is_some_and(|next| next.eq_ignore_ascii_case("COLUMN"))
                {
                    if let (Some(table), Some(column)) = (table, tokens.get(i + 2)) {
                        added.push((table.to_string(), column.to_string()));
                    }
                }
            }
        }
        added
    }

    async fn table_columns(conn: &mut SqliteConnection, table: &str) -> Result<Vec<String>> {
        sqlx::query_scalar("SELECT name FROM pragma_table_info(?)")
            .bind(table)
            .fetch_all(conn)
            .await
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

    /// H6-F2: a legacy backend DB (backend/app/db.py `_legacy_init_db`) already
    /// has `meetings.folder_path`; migration 20251006000000 then runs
    /// `ALTER TABLE meetings ADD COLUMN folder_path` and fails.
    #[tokio::test]
    async fn legacy_db_with_folder_path_opens_and_keeps_rows() {
        let dir = tempfile::tempdir().unwrap();
        // Legacy backend DB is copied in by open_in_dir when meeting_minutes.sqlite is absent.
        let legacy_db = dir.path().join("meeting_minutes.db");
        let legacy = SqlitePool::connect_with(
            SqliteConnectOptions::new()
                .filename(&legacy_db)
                .create_if_missing(true),
        )
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE meetings (id TEXT PRIMARY KEY, title TEXT NOT NULL, \
             created_at TEXT NOT NULL, updated_at TEXT NOT NULL, folder_path TEXT)",
        )
        .execute(&legacy)
        .await
        .unwrap();
        sqlx::query("INSERT INTO meetings VALUES ('m1', 'Standup', '2025-01-01', '2025-01-01', NULL)")
            .execute(&legacy)
            .await
            .unwrap();
        sqlx::query("INSERT INTO meetings VALUES ('m2', 'Review', '2025-01-02', '2025-01-02', '/x/y')")
            .execute(&legacy)
            .await
            .unwrap();
        // Migration 20251006000000 also ADD COLUMNs three audio fields on transcripts.
        sqlx::query(
            "CREATE TABLE transcripts (id TEXT PRIMARY KEY, meeting_id TEXT NOT NULL, \
             transcript TEXT NOT NULL, timestamp TEXT NOT NULL, audio_start_time REAL, \
             audio_end_time REAL, duration REAL)",
        )
        .execute(&legacy)
        .await
        .unwrap();
        sqlx::query("INSERT INTO transcripts VALUES ('t1', 'm2', 'hello', '2025-01-02', 1.5, 2.25, 0.75)")
            .execute(&legacy)
            .await
            .unwrap();
        legacy.close().await;

        let db = match DatabaseManager::open_in_dir(dir.path()).await {
            Ok(db) => db,
            Err(e) => panic!("open_in_dir on legacy DB failed: {e}"),
        };
        let folder_path: Option<String> =
            sqlx::query_scalar("SELECT folder_path FROM meetings WHERE id = 'm2'")
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(folder_path.as_deref(), Some("/x/y"));
        let audio: (Option<f64>, Option<f64>, Option<f64>) = sqlx::query_as(
            "SELECT audio_start_time, audio_end_time, duration FROM transcripts WHERE id = 't1'",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(audio, (Some(1.5), Some(2.25), Some(0.75)));
        db.cleanup().await.unwrap();
    }
}

#[cfg(test)]
mod legacy_import_tests {
    use super::*;
    use sqlx::sqlite::SqliteConnectOptions;

    /// H6-F1: onboarding passes the default legacy path (the data root's
    /// `meeting_minutes.db`) back into `import_legacy_database`, which copies
    /// it onto itself and truncates it.
    // Not built on Windows: mock_app() makes tauri's menu/dialog code reachable, which
    // imports Common Controls v6 functions. tauri-build embeds the v6 manifest only in
    // bin targets, so the lib's unit-test exe fails to load (STATUS_ENTRYPOINT_NOT_FOUND).
    #[cfg(not(windows))]
    #[tokio::test]
    async fn import_from_default_location_keeps_meetings() {
        let root = tempfile::tempdir().unwrap();
        // Another test may already own the process-wide data root; never write
        // anywhere but the tempdir.
        let _ = crate::storage::DATA_ROOT.set(root.path().to_path_buf());
        assert_eq!(
            crate::storage::root(),
            root.path(),
            "storage::DATA_ROOT is claimed by a different path; this test cannot sandbox its writes"
        );
        assert!(root.path().starts_with(std::env::temp_dir()));

        let legacy_db = crate::storage::db_dir().join("meeting_minutes.db");
        let legacy = SqlitePool::connect_with(
            SqliteConnectOptions::new()
                .filename(&legacy_db)
                .create_if_missing(true),
        )
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE meetings (id TEXT PRIMARY KEY, title TEXT NOT NULL, \
             created_at TEXT NOT NULL, updated_at TEXT NOT NULL)",
        )
        .execute(&legacy)
        .await
        .unwrap();
        sqlx::query("INSERT INTO meetings VALUES ('m1', 'Standup', '2025-01-01', '2025-01-01')")
            .execute(&legacy)
            .await
            .unwrap();
        sqlx::query("INSERT INTO meetings VALUES ('m2', 'Review', '2025-01-02', '2025-01-02')")
            .execute(&legacy)
            .await
            .unwrap();
        legacy.close().await;

        let app = tauri::test::mock_app();
        let db = DatabaseManager::import_legacy_database(
            app.handle(),
            legacy_db.to_str().unwrap(),
        )
        .await
        .unwrap();
        let meetings: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM meetings")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(meetings, 2);
        db.cleanup().await.unwrap();
    }

    #[test]
    fn migration_column_parser_finds_every_add_column_form() {
        let migrator = sqlx::migrate!("./migrations");
        let added = DatabaseManager::migration_added_columns(&migrator);
        let has = |table: &str, column: &str| {
            added.iter().any(|(t, c)| t == table && c == column)
        };
        // Single-line ALTERs, a multi-line ALTER with the ADD on its own line,
        // and a column whose default contains commas.
        assert!(has("meetings", "folder_path"));
        assert!(has("transcripts", "duration"));
        assert!(has("summary_processes", "result_backup"));
        assert!(has("transcript_settings", "openaiCompatibleBaseUrl"));
        assert!(has("background_jobs", "wordTimestamps"));
        assert!(has("transcript_settings", "whisperVocabularyHint"));
    }
}
