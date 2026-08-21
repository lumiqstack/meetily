//! One-time relocation of existing data into the configured data root.
//!
//! Split into two phases because they need different things to be true:
//!
//! * [`run_files`] moves bytes. It must run before anything opens the database
//!   or scans the models directory, so it is the first thing in `setup()`.
//! * [`rewrite_db_paths`] fixes the absolute paths stored in `meetings.folder_path`
//!   and `background_jobs.folder_path`. It needs a live pool, so it runs just
//!   after the database is initialised. Skipping it would leave every
//!   pre-existing meeting pointing at a directory that no longer exists —
//!   playback and retranscription would silently fail.
//!
//! Both phases are idempotent and record their progress in
//! `<root>/.migration-state.json`, so a force-quit mid-copy resumes on the next
//! launch rather than leaving data half-moved.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use tauri::{AppHandle, Emitter, Runtime};

use super::{legacy_root, root};

const STATE_FILE: &str = ".migration-state.json";

/// Above this size we verify by length alone. Hashing a 3 GB model file adds
/// minutes to a startup that is already copying it once.
const HASH_LIMIT_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    completed: Vec<String>,
    /// Where meeting folders used to live. Persisted because the database
    /// rewrite may not happen until a later launch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    old_recordings_root: Option<PathBuf>,
    #[serde(default)]
    db_paths_rewritten: bool,
}

impl State {
    fn done(&self, step: &str) -> bool {
        self.completed.iter().any(|s| s == step)
    }

    fn mark(&mut self, step: &str) {
        if !self.done(step) {
            self.completed.push(step.to_string());
        }
    }
}

fn state_path() -> PathBuf {
    root().join(STATE_FILE)
}

fn load_state() -> State {
    std::fs::read_to_string(state_path())
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save_state(state: &State) -> Result<()> {
    let path = state_path();
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(state)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct MigrationReport {
    pub files_moved: u64,
    pub bytes_moved: u64,
    pub skipped: Vec<String>,
}

/// Phase 1: move files. Returns immediately when no relocation is configured.
pub fn run_files<R: Runtime>(app: &AppHandle<R>) -> Result<MigrationReport> {
    let mut report = MigrationReport::default();

    if !super::is_relocated() {
        return Ok(report);
    }

    let legacy = legacy_root();
    let root = root();
    let mut state = load_state();

    log::info!(
        "Storage migration: {} -> {}",
        legacy.display(),
        root.display()
    );
    let _ = app.emit("storage-migration", serde_json::json!({ "phase": "start" }));

    // Plain subdirectory moves, in increasing order of "how bad is it if this is
    // the step that gets interrupted".
    for name in ["bin", "sp-webview", "models"] {
        if state.done(name) {
            continue;
        }
        let _ = app.emit(
            "storage-migration",
            serde_json::json!({ "phase": "step", "step": name }),
        );
        match move_tree(&legacy.join(name), &root.join(name), &mut report) {
            Ok(0) => {
                state.mark(name);
                let _ = save_state(&state);
            }
            // Some files stayed behind; retry on the next launch rather than
            // declaring the step done.
            Ok(failures) => {
                log::warn!(
                    "Storage migration step '{}': {} file(s) could not be moved yet",
                    name, failures
                );
            }
            Err(e) => {
                log::error!("Storage migration step '{}' failed: {:#}", name, e);
                report.skipped.push(format!("{}: {}", name, e));
            }
        }
    }

    // Logs previously lived under %LOCALAPPDATA%, and under app_data_dir for
    // anyone who ran a build after the storage module but before choosing a
    // root. Both are folded in here.
    //
    // These are moved *aside*, not over: the destination holds the log this
    // very process is writing to, and replacing it silently discarded the
    // migration's own error output on the first run.
    if !state.done("logs") {
        let mut ok = true;
        for old_logs in [legacy_logs_dir(), Some(legacy.join("logs"))]
            .into_iter()
            .flatten()
        {
            if let Err(e) = archive_logs(&old_logs, &root.join("logs"), &mut report) {
                log::warn!("Storage migration step 'logs' failed: {:#}", e);
                report.skipped.push(format!("logs: {}", e));
                ok = false;
            }
        }
        if ok {
            state.mark("logs");
            let _ = save_state(&state);
        }
    }

    if !state.done("recordings") {
        let _ = app.emit(
            "storage-migration",
            serde_json::json!({ "phase": "step", "step": "recordings" }),
        );
        match migrate_recordings(&legacy, &root, &mut state, &mut report) {
            Ok(true) => {
                state.mark("recordings");
                let _ = save_state(&state);
            }
            // Partial: state already holds old_recordings_root, so
            // rewrite_db_paths can repoint whatever did move, and the next
            // launch resumes the rest.
            Ok(false) => {
                let _ = save_state(&state);
            }
            Err(e) => {
                log::error!("Storage migration step 'recordings' failed: {:#}", e);
                report.skipped.push(format!("recordings: {}", e));
            }
        }
    }

    // The database goes last: it is the one file whose loss would be
    // unrecoverable, and by now everything it references has already moved.
    if !state.done("db") {
        let _ = app.emit(
            "storage-migration",
            serde_json::json!({ "phase": "step", "step": "database" }),
        );
        match migrate_database(&legacy, &root, &mut report) {
            Ok(()) => {
                state.mark("db");
                let _ = save_state(&state);
            }
            Err(e) => {
                log::error!("Storage migration step 'db' failed: {:#}", e);
                report.skipped.push(format!("database: {}", e));
            }
        }
    }

    log::info!(
        "Storage migration moved {} file(s), {:.1} MB{}",
        report.files_moved,
        report.bytes_moved as f64 / (1024.0 * 1024.0),
        if report.skipped.is_empty() {
            String::new()
        } else {
            format!(" ({} step(s) skipped)", report.skipped.len())
        }
    );
    let _ = app.emit(
        "storage-migration",
        serde_json::json!({ "phase": "complete", "report": &report }),
    );

    Ok(report)
}

/// Phase 2: repoint the absolute paths the database stores for each meeting.
///
/// Deliberately **not** a single prefix-swap UPDATE. The recordings move can be
/// partial (OneDrive placeholders), so rewriting every row at once would point
/// not-yet-moved meetings at folders that do not exist. Instead each row is
/// checked individually and only repointed once its folder is actually present
/// at the destination. Idempotent, and safe to run on every launch until the
/// move finishes.
pub async fn rewrite_db_paths(pool: &SqlitePool) -> Result<u64> {
    let mut state = load_state();

    if state.db_paths_rewritten {
        return Ok(0);
    }
    let Some(old_root) = state.old_recordings_root.clone() else {
        return Ok(0);
    };

    let new_root = root().join("recordings");
    if old_root == new_root {
        state.db_paths_rewritten = true;
        let _ = save_state(&state);
        return Ok(0);
    }

    let old_prefix = old_root.to_string_lossy().to_string();
    let mut total = 0u64;
    let mut pending = 0u64;

    for table in ["meetings", "background_jobs"] {
        if !table_exists(pool, table).await? {
            continue;
        }

        let select = format!(
            "SELECT rowid, folder_path FROM {table} WHERE folder_path IS NOT NULL"
        );
        let rows: Vec<(i64, String)> = sqlx::query_as(&select)
            .fetch_all(pool)
            .await
            .with_context(|| format!("reading folder_path from {table}"))?;

        for (rowid, folder) in rows {
            let Some(rest) = strip_prefix_ci(&folder, &old_prefix) else {
                continue;
            };
            let candidate = new_root.join(rest.trim_start_matches(['\\', '/']));
            if !candidate.exists() {
                pending += 1;
                continue; // Not moved yet — leave it pointing at the old copy.
            }

            let update = format!("UPDATE {table} SET folder_path = ?1 WHERE rowid = ?2");
            sqlx::query(&update)
                .bind(candidate.to_string_lossy().to_string())
                .bind(rowid)
                .execute(pool)
                .await
                .with_context(|| format!("repointing folder_path in {table}"))?;
            total += 1;
        }
    }

    if total > 0 {
        log::info!("Repointed {} meeting folder path(s) to {}", total, new_root.display());
    }

    // Only finished when the file move is done and nothing is still pointing at
    // the old root; otherwise stay armed for the next launch.
    if pending == 0 && state.done("recordings") {
        state.db_paths_rewritten = true;
    } else if pending > 0 {
        log::info!("{} meeting folder path(s) still awaiting their files", pending);
    }
    save_state(&state)?;

    Ok(total)
}

/// Windows paths compare case-insensitively; a stored path may differ in case
/// from the one we computed (drive letter, OneDrive folder name).
fn strip_prefix_ci<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    if value.len() < prefix.len() {
        return None;
    }
    let (head, rest) = value.split_at(prefix.len());
    if head.eq_ignore_ascii_case(prefix) {
        Some(rest)
    } else {
        None
    }
}

async fn table_exists(pool: &SqlitePool, name: &str) -> Result<bool> {
    let found: Option<(i64,)> =
        sqlx::query_as("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?")
            .bind(name)
            .fetch_optional(pool)
            .await?;
    Ok(found.is_some())
}

/// Move old log files into the new logs directory under a distinct name.
///
/// Never overwrites: `<dst>/meetily.log` is open for append by this process.
/// Move the main window's WebView2 profile under the data root.
///
/// Unlike every other step this cannot run from `setup()`: Tauri creates the
/// config-declared windows — and with them the WebView2 environment, which locks
/// the profile — before the setup hook fires. So `main()` calls this before
/// `run()`, while nothing has the folder open.
///
/// Returns the directory to point `WEBVIEW2_USER_DATA_FOLDER` at, or `None` when
/// the profile could not be moved. Returning `None` deliberately leaves the app
/// pointed at the system-drive copy: a half-moved profile that WebView2 then
/// recreates from scratch would silently drop localStorage and cookies, which is
/// a worse outcome than leaving 86 MB where it is.
#[cfg(windows)]
pub fn relocate_webview2_profile() -> Option<PathBuf> {
    if !super::is_relocated() {
        return None;
    }

    let target = super::webview2_dir();
    let dst = target.join("EBWebView");

    // %LOCALAPPDATA%, not app_data_dir: this is where the WebView2 runtime puts
    // an unpackaged app's default profile.
    let old = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)?
        .join("com.meetily.ai")
        .join("EBWebView");

    if !old.exists() {
        return Some(target); // Nothing to carry over — a fresh profile is fine.
    }
    if dst.exists() {
        // Already relocated on an earlier launch; the stale copy is dead weight.
        if let Err(e) = std::fs::remove_dir_all(&old) {
            log::warn!("Could not remove the old WebView2 profile: {}", e);
        }
        return Some(target);
    }

    // Copy first and only delete the original once the whole tree landed. A
    // move deletes as it goes, so an interrupted one leaves *both* copies
    // incomplete — and a WebView2 profile with a half-written cookie database is
    // worse than no relocation at all.
    let mut report = MigrationReport::default();
    match copy_tree(&old, &dst, &mut report) {
        Ok(()) => {
            log::info!(
                "Relocated WebView2 profile to {} ({} files, {:.1} MB)",
                dst.display(),
                report.files_moved,
                report.bytes_moved as f64 / (1024.0 * 1024.0)
            );
            if let Err(e) = std::fs::remove_dir_all(&old) {
                log::warn!("Copied the WebView2 profile but could not remove the original: {}", e);
            }
            Some(target)
        }
        Err(e) => {
            log::warn!(
                "Could not relocate the WebView2 profile ({:#}); keeping it on the system drive",
                e
            );
            // Leave nothing half-written for the next launch to mistake for a
            // finished relocation.
            let _ = std::fs::remove_dir_all(&dst);
            None
        }
    }
}

/// Recursive copy that fails loudly on the first problem, leaving the source
/// untouched. Used where the source must stay valid until the copy is complete.
#[cfg(windows)]
fn copy_tree(src: &Path, dst: &Path, report: &mut MigrationReport) -> Result<()> {
    std::fs::create_dir_all(dst).with_context(|| format!("creating {}", dst.display()))?;

    for entry in std::fs::read_dir(src).with_context(|| format!("reading {}", src.display()))? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());

        if entry.file_type()?.is_dir() {
            copy_tree(&from, &to, report)?;
        } else {
            let len = std::fs::copy(&from, &to)
                .with_context(|| format!("copying {} -> {}", from.display(), to.display()))?;
            report.files_moved += 1;
            report.bytes_moved += len;
        }
    }

    Ok(())
}

fn archive_logs(src: &Path, dst: &Path, report: &mut MigrationReport) -> Result<()> {
    if !src.exists() || src == dst {
        return Ok(());
    }
    std::fs::create_dir_all(dst)?;

    for entry in std::fs::read_dir(src)? {
        let Ok(entry) = entry else { continue };
        let from = entry.path();
        if !from.is_file() {
            continue;
        }

        let name = entry.file_name();
        let mut to = dst.join(&name);
        if to.exists() {
            // e.g. meetily.log -> meetily.premigration.log
            let stem = Path::new(&name)
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "log".into());
            let ext = Path::new(&name)
                .extension()
                .map(|s| format!(".{}", s.to_string_lossy()))
                .unwrap_or_default();
            to = dst.join(format!("{stem}.premigration{ext}"));
        }
        if to.exists() {
            // Already archived on an earlier run; drop the duplicate source.
            let _ = std::fs::remove_file(&from);
            continue;
        }

        if let Err(e) = move_file(&from, &to, report) {
            log::warn!("Could not archive log {}: {:#}", from.display(), e);
        }
    }

    let _ = std::fs::remove_dir(src);
    Ok(())
}

/// Where release logs lived before the data root existed.
fn legacy_logs_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA")
            .map(|p| PathBuf::from(p).join("com.meetily.ai").join("logs"))
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join(".local/share").join("com.meetily.ai").join("logs"))
    }
}

/// Move meeting folders. Returns true when everything moved cleanly.
///
/// Takes `state` so the source root is persisted **before** any file moves.
/// `rewrite_db_paths` keys off it to repoint meetings, and a run that dies
/// halfway must still leave enough information to finish the job — recording it
/// only on success is what stranded the first three meetings.
fn migrate_recordings(
    legacy: &Path,
    root: &Path,
    state: &mut State,
    report: &mut MigrationReport,
) -> Result<bool> {
    let old_root = state.old_recordings_root.clone().unwrap_or_else(|| {
        stored_recordings_folder(legacy).unwrap_or_else(super::historical_recordings_dir)
    });

    let new_root = root.join("recordings");
    if old_root == new_root {
        return Ok(true);
    }

    // Persist first, and point the stored preference at the destination now so
    // new recordings land there even if this run only gets partway.
    state.old_recordings_root = Some(old_root.clone());
    save_state(state)?;
    if let Err(e) = rewrite_stored_recordings_folder(legacy, &new_root) {
        log::warn!("Could not update save_folder in recording_preferences.json: {:#}", e);
    }

    if !old_root.exists() {
        return Ok(true);
    }

    let failures = move_tree(&old_root, &new_root, report)?;
    if failures > 0 {
        log::warn!(
            "{} recording file(s) could not be moved yet — most likely OneDrive \
             cloud-only placeholders. Mark the folder \"Always keep on this device\", \
             let it download, and relaunch to finish.",
            failures
        );
    }
    Ok(failures == 0)
}

/// tauri-plugin-store keeps its files in app_data_dir under a flat key/value
/// object; we read and patch the JSON directly rather than booting the plugin.
fn preferences_file(legacy: &Path) -> PathBuf {
    legacy.join("recording_preferences.json")
}

fn stored_recordings_folder(legacy: &Path) -> Option<PathBuf> {
    let raw = std::fs::read_to_string(preferences_file(legacy)).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let folder = value
        .get("preferences")?
        .get("save_folder")?
        .as_str()?
        .to_string();
    if folder.is_empty() {
        None
    } else {
        Some(PathBuf::from(folder))
    }
}

fn rewrite_stored_recordings_folder(legacy: &Path, new_root: &Path) -> Result<()> {
    let path = preferences_file(legacy);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Ok(()); // No stored preferences yet; the new default applies.
    };
    let mut value: serde_json::Value = serde_json::from_str(&raw)?;
    let Some(prefs) = value.get_mut("preferences") else {
        return Ok(());
    };
    prefs["save_folder"] = serde_json::Value::String(new_root.to_string_lossy().to_string());

    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&value)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

fn migrate_database(legacy: &Path, root: &Path, report: &mut MigrationReport) -> Result<()> {
    for name in [
        "meeting_minutes.sqlite",
        "meeting_minutes.sqlite-wal",
        "meeting_minutes.sqlite-shm",
        "meeting_minutes.db",
    ] {
        let src = legacy.join(name);
        if src.exists() {
            move_file(&src, &root.join(name), report)?;
        }
    }
    Ok(())
}

/// Move every file under `src` into `dst`, then drop the emptied source tree.
///
/// Returns the number of files it could not move. Individual failures are
/// logged and skipped rather than aborting: the first run of this migration hit
/// a OneDrive cloud-only placeholder a few folders in, gave up on the entire
/// recordings step, and left the already-moved meetings stranded — their
/// database rows still pointing at the old location. Partial progress has to be
/// survivable, because with Files-On-Demand it is the normal case.
fn move_tree(src: &Path, dst: &Path, report: &mut MigrationReport) -> Result<usize> {
    if !src.exists() || src == dst {
        return Ok(0);
    }

    std::fs::create_dir_all(dst).with_context(|| format!("creating {}", dst.display()))?;

    let mut failures = 0;

    for entry in std::fs::read_dir(src).with_context(|| format!("reading {}", src.display()))? {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                log::warn!("Skipping unreadable entry in {}: {}", src.display(), e);
                failures += 1;
                continue;
            }
        };

        let from = entry.path();
        let to = dst.join(entry.file_name());

        let is_dir = match entry.file_type() {
            Ok(t) => t.is_dir(),
            Err(e) => {
                log::warn!("Skipping {} (cannot stat): {}", from.display(), e);
                failures += 1;
                continue;
            }
        };

        if is_dir {
            match move_tree(&from, &to, report) {
                Ok(n) => failures += n,
                Err(e) => {
                    log::warn!("Skipping {}: {:#}", from.display(), e);
                    failures += 1;
                }
            }
        } else if let Err(e) = move_file(&from, &to, report) {
            log::warn!("Could not move {}: {:#}", from.display(), e);
            report.skipped.push(format!("{}: {}", from.display(), e));
            failures += 1;
        }
    }

    // Only succeeds once every child moved, which is exactly the condition we
    // want before declaring the source gone.
    let _ = std::fs::remove_dir(src);
    Ok(failures)
}

/// Move one file, verifying the copy before deleting the original.
fn move_file(src: &Path, dst: &Path, report: &mut MigrationReport) -> Result<()> {
    if !src.exists() {
        return Ok(());
    }

    let len = std::fs::metadata(src)?.len();

    // Resuming a previous run: the destination may already hold a good copy.
    if dst.exists() {
        if verify(src, dst).unwrap_or(false) {
            std::fs::remove_file(src)
                .with_context(|| format!("removing already-copied {}", src.display()))?;
            return Ok(());
        }
        std::fs::remove_file(dst)
            .with_context(|| format!("clearing partial {}", dst.display()))?;
    }

    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Same-volume moves are a metadata operation; only pay for a copy when
    // rename refuses (which is what a C: -> D: move does).
    if std::fs::rename(src, dst).is_ok() {
        report.files_moved += 1;
        report.bytes_moved += len;
        return Ok(());
    }

    std::fs::copy(src, dst)
        .with_context(|| format!("copying {} -> {}", src.display(), dst.display()))?;

    if !verify(src, dst)? {
        let _ = std::fs::remove_file(dst);
        return Err(anyhow!(
            "verification failed for {} -> {}; original left in place",
            src.display(),
            dst.display()
        ));
    }

    std::fs::remove_file(src).with_context(|| format!("removing {}", src.display()))?;

    report.files_moved += 1;
    report.bytes_moved += len;
    log::info!("Moved {} ({:.1} MB)", dst.display(), len as f64 / (1024.0 * 1024.0));
    Ok(())
}

/// Length always; SHA-256 as well for anything small enough that hashing is not
/// a meaningful share of the copy time.
fn verify(src: &Path, dst: &Path) -> Result<bool> {
    let src_len = std::fs::metadata(src)?.len();
    let dst_len = std::fs::metadata(dst)?.len();
    if src_len != dst_len {
        return Ok(false);
    }
    if src_len > HASH_LIMIT_BYTES {
        return Ok(true);
    }
    Ok(hash_file(src)? == hash_file(dst)?)
}

fn hash_file(path: &Path) -> Result<[u8; 32]> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 128 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write(path: &Path, contents: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn move_file_verifies_then_removes_the_original() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("from/a.bin");
        let dst = dir.path().join("to/a.bin");
        write(&src, b"hello world");

        let mut report = MigrationReport::default();
        move_file(&src, &dst, &mut report).unwrap();

        assert!(!src.exists());
        assert_eq!(std::fs::read(&dst).unwrap(), b"hello world");
        assert_eq!(report.files_moved, 1);
        assert_eq!(report.bytes_moved, 11);
    }

    #[test]
    fn move_file_resumes_when_destination_already_matches() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("from/a.bin");
        let dst = dir.path().join("to/a.bin");
        write(&src, b"same");
        write(&dst, b"same");

        let mut report = MigrationReport::default();
        move_file(&src, &dst, &mut report).unwrap();

        assert!(!src.exists());
        assert_eq!(std::fs::read(&dst).unwrap(), b"same");
    }

    #[test]
    fn move_file_replaces_a_partial_destination() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("from/a.bin");
        let dst = dir.path().join("to/a.bin");
        write(&src, b"complete contents");
        write(&dst, b"partial");

        let mut report = MigrationReport::default();
        move_file(&src, &dst, &mut report).unwrap();

        assert!(!src.exists());
        assert_eq!(std::fs::read(&dst).unwrap(), b"complete contents");
    }

    #[test]
    fn move_tree_moves_nested_files_and_clears_the_source() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("models");
        let dst = dir.path().join("root/models");
        write(&src.join("ggml-base.bin"), b"weights");
        write(&src.join("summary/qwen.gguf"), b"more weights");

        let mut report = MigrationReport::default();
        let failures = move_tree(&src, &dst, &mut report).unwrap();

        assert_eq!(failures, 0);
        assert!(!src.exists());
        assert_eq!(std::fs::read(dst.join("ggml-base.bin")).unwrap(), b"weights");
        assert_eq!(
            std::fs::read(dst.join("summary/qwen.gguf")).unwrap(),
            b"more weights"
        );
        assert_eq!(report.files_moved, 2);
    }

    /// The WebView2 profile is copied rather than moved precisely so that a
    /// failure leaves the original intact — an interrupted move would corrupt
    /// both copies, and a half-written cookie database loses the session.
    #[cfg(windows)]
    #[test]
    fn copy_tree_leaves_the_source_intact() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("EBWebView");
        let dst = dir.path().join("root/webview2/EBWebView");
        write(&src.join("Local State"), b"state");
        write(&src.join("Default/Cookies"), b"cookiedb");

        let mut report = MigrationReport::default();
        copy_tree(&src, &dst, &mut report).unwrap();

        assert!(src.join("Local State").exists(), "source must survive the copy");
        assert_eq!(std::fs::read(dst.join("Local State")).unwrap(), b"state");
        assert_eq!(std::fs::read(dst.join("Default/Cookies")).unwrap(), b"cookiedb");
        assert_eq!(report.files_moved, 2);
    }

    #[cfg(windows)]
    #[test]
    fn copy_tree_reports_a_missing_source_instead_of_succeeding_empty() {
        let dir = tempdir().unwrap();
        let mut report = MigrationReport::default();
        let r = copy_tree(
            &dir.path().join("absent"),
            &dir.path().join("dst"),
            &mut report,
        );
        assert!(r.is_err(), "a missing profile must not read as a clean copy");
    }

    #[test]
    fn move_tree_on_a_missing_source_is_a_noop() {
        let dir = tempdir().unwrap();
        let mut report = MigrationReport::default();
        let failures =
            move_tree(&dir.path().join("absent"), &dir.path().join("dst"), &mut report).unwrap();
        assert_eq!(failures, 0);
        assert_eq!(report.files_moved, 0);
    }

    #[test]
    fn move_tree_keeps_going_past_an_unmovable_file() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("recordings");
        let dst = dir.path().join("root/recordings");

        write(&src.join("a/audio.mp4"), b"first");
        write(&src.join("b/audio.mp4"), b"second");
        write(&src.join("c/audio.mp4"), b"third");

        // Block exactly one destination by putting a *directory* where the file
        // needs to go — stands in for the OneDrive placeholder that aborted the
        // first real run.
        std::fs::create_dir_all(dst.join("b/audio.mp4")).unwrap();

        let mut report = MigrationReport::default();
        let failures = move_tree(&src, &dst, &mut report).unwrap();

        assert_eq!(failures, 1, "the blocked file should be counted, not fatal");
        // The other two still made it across.
        assert_eq!(std::fs::read(dst.join("a/audio.mp4")).unwrap(), b"first");
        assert_eq!(std::fs::read(dst.join("c/audio.mp4")).unwrap(), b"third");
        // And the one that failed is still safe at the source.
        assert_eq!(std::fs::read(src.join("b/audio.mp4")).unwrap(), b"second");
    }

    #[test]
    fn archive_logs_never_overwrites_the_active_log() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("old-logs");
        let dst = dir.path().join("new-logs");

        write(&src.join("meetily.log"), b"pre-migration content");
        write(&dst.join("meetily.log"), b"the log this process is writing");

        let mut report = MigrationReport::default();
        archive_logs(&src, &dst, &mut report).unwrap();

        assert_eq!(
            std::fs::read(dst.join("meetily.log")).unwrap(),
            b"the log this process is writing",
            "active log must survive untouched"
        );
        assert_eq!(
            std::fs::read(dst.join("meetily.premigration.log")).unwrap(),
            b"pre-migration content"
        );
    }

    #[test]
    fn strip_prefix_ci_matches_regardless_of_drive_letter_case() {
        assert_eq!(
            strip_prefix_ci(r"d:\OneDrive\rec\Meeting_1", r"D:\onedrive\rec"),
            Some(r"\Meeting_1")
        );
        assert_eq!(strip_prefix_ci(r"E:\other\Meeting_1", r"D:\rec"), None);
        assert_eq!(strip_prefix_ci("short", "a-much-longer-prefix"), None);
    }

    #[test]
    fn verify_rejects_a_length_mismatch() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        write(&a, b"1234");
        write(&b, b"12345");
        assert!(!verify(&a, &b).unwrap());
    }

    #[test]
    fn verify_rejects_same_length_different_content() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        write(&a, b"1234");
        write(&b, b"4321");
        assert!(!verify(&a, &b).unwrap());
    }

    #[test]
    fn stored_recordings_folder_reads_the_store_layout() {
        let dir = tempdir().unwrap();
        write(
            &dir.path().join("recording_preferences.json"),
            br#"{"preferences":{"save_folder":"C:\\Users\\x\\Music\\meetily-recordings","auto_save":true}}"#,
        );
        assert_eq!(
            stored_recordings_folder(dir.path()),
            Some(PathBuf::from(r"C:\Users\x\Music\meetily-recordings"))
        );
    }

    #[test]
    fn rewrite_stored_recordings_folder_patches_only_that_key() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("recording_preferences.json");
        write(
            &path,
            br#"{"preferences":{"save_folder":"C:\\old","auto_save":false,"file_format":"mp4"}}"#,
        );

        rewrite_stored_recordings_folder(dir.path(), Path::new(r"D:\new")).unwrap();

        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["preferences"]["save_folder"], r"D:\new");
        assert_eq!(value["preferences"]["auto_save"], false);
        assert_eq!(value["preferences"]["file_format"], "mp4");
    }
}
