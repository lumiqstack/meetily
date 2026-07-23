// SharePoint / Microsoft Stream authentication via an embedded webview.
//
// View-only Teams recordings live behind a corporate SSO login on
// `*.sharepoint.com`. We can't use Microsoft Graph, so instead we ride the
// user's own authenticated browser session: an embedded Tauri webview loads
// the recording URL, the user signs in once (SSO/MFA), and we harvest the
// resulting auth cookies — including the HttpOnly `FedAuth`/`rtFa` cookies that
// page JavaScript cannot read — via the native `cookies_for_url` API. Those
// cookies are handed to yt-dlp as a Netscape `cookies.txt`.
//
// The webview's data directory is persisted, so after the first login the
// session is reused and subsequent imports authenticate silently (the window
// never has to be shown).

use anyhow::{anyhow, Context, Result};
use log::{debug, info, warn};
use std::path::PathBuf;
use std::time::Duration;
use tauri::webview::Cookie;
use tauri::{AppHandle, Manager, Runtime, WebviewUrl, WebviewWindowBuilder};
use url::Url;

const AUTH_WINDOW_LABEL: &str = "meetily-sp-auth";

/// Cookie names that indicate an authenticated SharePoint session.
const AUTH_COOKIE_NAMES: &[&str] = &["fedauth", "rtfa", "edgeaccesscookie"];

/// How long to wait for a silent (already-signed-in) session before showing
/// the login window.
const SILENT_TIMEOUT: Duration = Duration::from_secs(8);
/// How long to wait for the user to complete an interactive login.
const INTERACTIVE_TIMEOUT: Duration = Duration::from_secs(300);
const POLL_INTERVAL: Duration = Duration::from_millis(1200);

/// Result of an authentication attempt: the path to a Netscape `cookies.txt`
/// the caller must delete when finished with it.
pub struct AuthCookies {
    pub cookies_txt: PathBuf,
}

impl AuthCookies {
    /// Best-effort removal of the temporary cookie file.
    pub fn cleanup(&self) {
        let _ = std::fs::remove_file(&self.cookies_txt);
    }
}

/// Ensure we have valid SharePoint auth cookies for `target_url`, showing an
/// embedded login window only if the persisted session is missing or expired.
///
/// `on_status` is called with short human-readable phase messages so the caller
/// can surface them as import progress.
pub async fn ensure_auth_cookies<R: Runtime, F: Fn(&str)>(
    app: &AppHandle<R>,
    target_url: &str,
    on_status: F,
) -> Result<AuthCookies> {
    let url = Url::parse(target_url).context("The recording link is not a valid URL")?;
    if !is_sharepoint_host(&url) {
        return Err(anyhow!(
            "This link is not a SharePoint/Stream recording URL (expected a *.sharepoint.com host)."
        ));
    }

    // Close any stale auth window from a previous attempt.
    if let Some(existing) = app.get_webview_window(AUTH_WINDOW_LABEL) {
        let _ = existing.close();
        // Give the runtime a moment to release the label.
        tokio::time::sleep(Duration::from_millis(150)).await;
    }

    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| anyhow!("Could not resolve app data dir: {e}"))?
        .join("sp-webview");
    std::fs::create_dir_all(&data_dir).ok();

    on_status("Connecting to SharePoint…");

    let window = WebviewWindowBuilder::new(
        app,
        AUTH_WINDOW_LABEL,
        WebviewUrl::External(url.clone()),
    )
    .title("Sign in to SharePoint — Meetily")
    .inner_size(1024.0, 768.0)
    .data_directory(data_dir)
    .visible(false)
    .build()
    .context("Failed to open the SharePoint sign-in window")?;

    // Phase 1: silent — maybe the persisted session is still valid.
    if let Some(cookies) = poll_for_auth(&window, &url, SILENT_TIMEOUT).await? {
        info!("SharePoint session authenticated silently");
        let _ = window.close();
        return write_cookies(app, &url, cookies).await;
    }

    // Phase 2: interactive — show the window and let the user sign in.
    on_status("Waiting for you to sign in to SharePoint…");
    debug!("Silent auth failed; showing login window");
    let _ = window.show();
    let _ = window.set_focus();

    let result = poll_for_auth(&window, &url, INTERACTIVE_TIMEOUT).await;
    let _ = window.close();

    match result? {
        Some(cookies) => {
            info!("SharePoint session authenticated interactively");
            write_cookies(app, &url, cookies).await
        }
        None => Err(anyhow!(
            "Timed out waiting for SharePoint sign-in. Please try the import again."
        )),
    }
}

/// Poll the webview's cookie store until an auth cookie appears or `timeout`
/// elapses. Cookie reads are done on a blocking thread because the webview
/// runtime blocks the caller while it services the request on the main thread.
async fn poll_for_auth<R: Runtime>(
    window: &tauri::WebviewWindow<R>,
    url: &Url,
    timeout: Duration,
) -> Result<Option<Vec<Cookie<'static>>>> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let cookies = read_cookies(window, url).await?;
        if has_auth_cookie(&cookies) {
            return Ok(Some(cookies));
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(None);
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Read cookies for `url` off the async worker (the runtime call is blocking).
async fn read_cookies<R: Runtime>(
    window: &tauri::WebviewWindow<R>,
    url: &Url,
) -> Result<Vec<Cookie<'static>>> {
    let window = window.clone();
    let url = url.clone();
    tokio::task::spawn_blocking(move || window.cookies_for_url(url))
        .await
        .map_err(|e| anyhow!("Cookie read task failed: {e}"))?
        .map_err(|e| anyhow!("Failed to read webview cookies: {e}"))
}

fn has_auth_cookie(cookies: &[Cookie<'static>]) -> bool {
    cookies.iter().any(|c| {
        let name = c.name().to_ascii_lowercase();
        AUTH_COOKIE_NAMES.contains(&name.as_str())
    })
}

fn is_sharepoint_host(url: &Url) -> bool {
    url.host_str()
        .map(|h| {
            let h = h.to_ascii_lowercase();
            h.ends_with(".sharepoint.com") || h.ends_with(".sharepoint.us")
        })
        .unwrap_or(false)
}

/// Write the harvested cookies as a Netscape `cookies.txt` for yt-dlp.
async fn write_cookies<R: Runtime>(
    app: &AppHandle<R>,
    url: &Url,
    cookies: Vec<Cookie<'static>>,
) -> Result<AuthCookies> {
    let host = url.host_str().unwrap_or("").to_string();

    let mut out = String::from("# Netscape HTTP Cookie File\n");
    // Session cookies get a far-future expiry so yt-dlp keeps them.
    let far_future = (chrono::Utc::now() + chrono::Duration::days(365)).timestamp();

    for c in &cookies {
        let domain = c.domain().map(|d| d.to_string()).unwrap_or_else(|| host.clone());
        if domain.is_empty() {
            continue;
        }
        let include_sub = if domain.starts_with('.') { "TRUE" } else { "FALSE" };
        let path = c.path().unwrap_or("/");
        let secure = if c.secure().unwrap_or(false) { "TRUE" } else { "FALSE" };
        let expiry = c
            .expires_datetime()
            .map(|dt| dt.unix_timestamp())
            .unwrap_or(far_future);
        out.push_str(&format!(
            "{domain}\t{include_sub}\t{path}\t{secure}\t{expiry}\t{}\t{}\n",
            c.name(),
            c.value()
        ));
    }

    if cookies.is_empty() {
        warn!("SharePoint auth produced no cookies; download will likely fail");
    }

    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| anyhow!("Could not resolve app data dir: {e}"))?
        .join("tmp");
    tokio::fs::create_dir_all(&dir).await.ok();
    let path = dir.join(format!("sp-cookies-{}.txt", uuid::Uuid::new_v4()));
    tokio::fs::write(&path, out)
        .await
        .with_context(|| format!("Failed to write cookies file {}", path.display()))?;

    Ok(AuthCookies { cookies_txt: path })
}

/// Remove cookie files a previous process left behind. The files hold live
/// FedAuth/rtFa auth tokens in plain text and are normally deleted by
/// `AuthCookies::cleanup` when the import finishes — but a crash mid-import
/// skips that, so startup sweeps the whole directory. No import can be
/// running this early, so every `sp-cookies-*.txt` here is stale.
pub fn sweep_stale_cookie_files<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
    let Ok(data_dir) = app.path().app_data_dir() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(data_dir.join("tmp")) else {
        return; // No tmp dir yet — nothing to sweep.
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("sp-cookies-") && name.ends_with(".txt") {
            match std::fs::remove_file(entry.path()) {
                Ok(()) => log::info!("Removed stale SharePoint cookie file: {name}"),
                Err(e) => log::warn!("Failed to remove stale cookie file {name}: {e}"),
            }
        }
    }
}
