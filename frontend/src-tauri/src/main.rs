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

    install_panic_hook();

    // Async logger will be initialized lazily when first needed (after Tauri runtime starts)
    log::info!("Starting application...");
    log::info!("{}", build_banner());
    app_lib::run();
}

/// One line identifying exactly which binary is running.
///
/// Every crash investigation starts with "is this build new enough to have the
/// fix?". Without this, answering it means reading the PE header timestamp out
/// of a crash dump and comparing it against `git log` — which is how the
/// 2026-08-12/13/14 crashes turned out to be an already-fixed tray bug running
/// on a binary built four hours before the fix landed.
fn build_banner() -> String {
    let built = match env!("MEETILY_BUILD_EPOCH").parse::<i64>() {
        Ok(secs) => chrono::DateTime::from_timestamp(secs, 0)
            .map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string())
            .unwrap_or_else(|| "unknown".to_string()),
        Err(_) => "unknown".to_string(),
    };

    format!(
        "Meetily {} | rev {} | built {} | {} | {}-{}",
        env!("CARGO_PKG_VERSION"),
        env!("MEETILY_GIT_SHA"),
        built,
        env!("MEETILY_BUILD_PROFILE"),
        std::env::consts::OS,
        std::env::consts::ARCH,
    )
}

/// Log panics with thread, location, and backtrace before the default hook runs.
///
/// Panics inside `tokio::spawn`/`async_runtime::spawn` tasks are captured by the
/// `JoinHandle`; when nobody awaits it — which is every spawn in this codebase —
/// the panic vanishes and the task simply stops. That is how a dead SharePoint
/// cookie-read leaves a wedged auth webview behind (see the `create_auth_window`
/// comment in `audio/sharepoint.rs`) with nothing in the log to say why. A panic
/// hook fires at panic time, before the unwind is swallowed, so these reach the
/// log file even though no caller ever sees the error.
///
/// This does not catch aborts: heap corruption and `Rc` refcount overflow trap
/// without unwinding, so they still surface only as a crash dump.
fn install_panic_hook() {
    let previous = std::panic::take_hook();

    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let thread_name = thread.name().unwrap_or("<unnamed>").to_string();

        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown location>".to_string());

        let payload = info.payload();
        let message = payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".to_string());

        log::error!(
            "PANIC in thread '{}' at {}: {}\n  {}\n{}",
            thread_name,
            location,
            message,
            build_banner(),
            std::backtrace::Backtrace::force_capture(),
        );

        previous(info);
    }));
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
