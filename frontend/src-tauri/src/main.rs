#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

use log;

fn main() {
    if std::env::var_os("RUST_LOG").is_none() {
        std::env::set_var("RUST_LOG", "info");
    }

    let mut builder = env_logger::Builder::from_env(env_logger::Env::default());
    // Release Windows builds run with `windows_subsystem = "windows"` — there is
    // no console, so stderr logging vanishes. Keep a file trail instead.
    #[cfg(not(debug_assertions))]
    if let Some(file) = open_log_file() {
        builder.target(env_logger::Target::Pipe(Box::new(std::io::LineWriter::new(
            file,
        ))));
    }
    builder.init();

    // Async logger will be initialized lazily when first needed (after Tauri runtime starts)
    log::info!("Starting application...");
    app_lib::run();
}

/// Open (rotating at 5 MB) the release log file, e.g.
/// `%LOCALAPPDATA%\com.meetily.ai\logs\meetily.log` on Windows.
#[cfg(not(debug_assertions))]
fn open_log_file() -> Option<std::fs::File> {
    let base = dirs_base()?;
    let dir = base.join("com.meetily.ai").join("logs");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("meetily.log");
    const MAX_LEN: u64 = 5 * 1024 * 1024;
    if std::fs::metadata(&path).map(|m| m.len() > MAX_LEN).unwrap_or(false) {
        let _ = std::fs::rename(&path, dir.join("meetily.log.old"));
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()
}

#[cfg(not(debug_assertions))]
fn dirs_base() -> Option<std::path::PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA").map(std::path::PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/share"))
    }
}
