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

use super::sharepoint_sync::{classify_redirect, RedirectVerdict};
use anyhow::{anyhow, Context, Result};
use log::{debug, info, warn};
use std::path::PathBuf;
use std::time::Duration;
use tauri::webview::Cookie;
use tauri::{AppHandle, Manager, Runtime, WebviewUrl, WebviewWindowBuilder};
use url::Url;

const AUTH_WINDOW_LABEL: &str = "meetily-sp-auth";

/// All auth flows share one webview window label, and every flow starts by
/// closing any window that label still points at. Two concurrent flows (e.g. a
/// background sync scan racing a user-initiated URL import) therefore destroy
/// each other's window mid-poll, which surfaces as a `cookies_for_url` panic
/// (`RecvError`: the webview died while servicing the cookie read). Serialize
/// the flows instead — the loser waits, it does not kill the winner.
static AUTH_FLOW_LOCK: once_cell::sync::Lazy<tokio::sync::Mutex<()>> =
    once_cell::sync::Lazy::new(|| tokio::sync::Mutex::new(()));

/// Cookie names that prove a *host-scoped* SharePoint session.
///
/// `rtFa` is deliberately absent. It is set domain-wide on `.sharepoint.com`,
/// so it is already present the instant we touch any host in the tenant —
/// before that host has minted its own FedAuth. Accepting it ends the poll
/// early and hands out a cookie set that the host answers with 401s and
/// sign-in redirects.
const AUTH_COOKIE_NAMES: &[&str] = &["fedauth", "edgeaccesscookie"];

/// How long to wait for a silent (already-signed-in) session before showing
/// the login window.
///
/// The corporate SSO redirect chain takes ~10-13s to mint fresh FedAuth from
/// the persisted profile (measured 2026-07-28: a shown window completed
/// "interactively" in 5s with zero user input, and the sibling-host hop —
/// which gets 20s — completed silently in 10s). At 8s the silent phase gave
/// up moments before SSO finished, so every import prompted for sign-in.
const SILENT_TIMEOUT: Duration = Duration::from_secs(20);
/// How long to wait for the user to complete an interactive login.
const INTERACTIVE_TIMEOUT: Duration = Duration::from_secs(300);
/// Silent-SSO hop to a sibling host (e.g. `tenant-my.sharepoint.com`) after
/// the primary host is signed in.
const EXTRA_HOST_TIMEOUT: Duration = Duration::from_secs(20);
const POLL_INTERVAL: Duration = Duration::from_millis(1200);
/// While polling, re-probe an unchanged cookie set against the server at most
/// this often (a fresh set is probed immediately).
const VALIDATE_EVERY: Duration = Duration::from_secs(10);

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

/// Multi-host authentication result: per-host cookie sets for building
/// `Cookie:` headers on direct REST calls (FedAuth is per-host in SharePoint
/// Online, so e.g. `tenant.sharepoint.com` and `tenant-my.sharepoint.com`
/// each need their own set). Cookie values must never be logged.
pub struct MultiHostAuth {
    pub host_cookies: std::collections::HashMap<String, Vec<Cookie<'static>>>,
}

/// Whether an expired session may interrupt the user with a login window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    /// Show the sign-in window when the persisted session has expired.
    AllowInteractive,
    /// Never show UI: fail with [`AUTH_REQUIRED_MARKER`] instead, so
    /// background work can ask the user at a time of their choosing.
    SilentOnly,
}

/// Marker embedded in the error returned by a silent auth that needs the user
/// to sign in. Callers match on it rather than on prose.
pub const AUTH_REQUIRED_MARKER: &str = "sharepoint-auth-required";

/// Whether an error came from a session that needs an interactive sign-in.
pub fn is_auth_required_error(error: &str) -> bool {
    error.contains(AUTH_REQUIRED_MARKER)
}

/// Like `ensure_auth_cookies`, but additionally visits `extra_hosts` with the
/// same (hidden) webview session so their per-host auth cookies get minted —
/// SSO normally completes these hops silently once the primary host is signed
/// in. Hosts that fail to authenticate are simply absent from the result; the
/// caller decides whether that is fatal.
pub async fn ensure_multi_host_auth<R: Runtime, F: Fn(&str)>(
    app: &AppHandle<R>,
    target_url: &str,
    extra_hosts: &[String],
    on_status: F,
) -> Result<MultiHostAuth> {
    ensure_multi_host_auth_mode(
        app,
        target_url,
        extra_hosts,
        AuthMode::AllowInteractive,
        on_status,
    )
    .await
}

/// [`ensure_multi_host_auth`] with explicit control over whether the sign-in
/// window may appear. Scheduled background scans use
/// [`AuthMode::SilentOnly`] so a expired cookie never steals focus.
pub async fn ensure_multi_host_auth_mode<R: Runtime, F: Fn(&str)>(
    app: &AppHandle<R>,
    target_url: &str,
    extra_hosts: &[String],
    mode: AuthMode,
    on_status: F,
) -> Result<MultiHostAuth> {
    let url = Url::parse(target_url).context("The SharePoint link is not a valid URL")?;
    if !is_sharepoint_host(&url) {
        return Err(anyhow!(
            "This link is not a SharePoint URL (expected a *.sharepoint.com host)."
        ));
    }

    let _flow = AUTH_FLOW_LOCK.lock().await;

    let data_dir = crate::storage::webview_dir();

    on_status("Connecting to SharePoint…");

    let window = create_auth_window(app, &url, &data_dir).await?;

    // Authenticate the primary host: silent first, then interactive.
    let primary = match poll_for_auth(&window, &url, SILENT_TIMEOUT).await? {
        Some(cookies) => {
            info!("SharePoint session authenticated silently");
            cookies
        }
        None if mode == AuthMode::SilentOnly => {
            // Background work must not pop a login window; report that a
            // sign-in is needed and let the caller notify the user.
            let _ = window.close();
            debug!("Silent auth failed and interactive sign-in is not allowed here");
            return Err(anyhow!(
                "{AUTH_REQUIRED_MARKER}: SharePoint sign-in required for {}",
                url.host_str().unwrap_or("SharePoint")
            ));
        }
        None => {
            on_status("Waiting for you to sign in to SharePoint…");
            debug!("Silent auth failed; showing login window");
            let _ = window.show();
            let _ = window.set_focus();
            let result = poll_for_auth(&window, &url, INTERACTIVE_TIMEOUT).await;
            match result? {
                Some(cookies) => {
                    info!("SharePoint session authenticated interactively");
                    let _ = window.hide();
                    cookies
                }
                None => {
                    let _ = window.close();
                    return Err(anyhow!(
                        "Timed out waiting for SharePoint sign-in. Please try again."
                    ));
                }
            }
        }
    };

    let mut host_cookies = std::collections::HashMap::new();
    let primary_host = url.host_str().unwrap_or("").to_ascii_lowercase();
    host_cookies.insert(primary_host.clone(), primary);

    // Visit each extra host so SSO mints its per-host cookies; these hops are
    // normally silent, so a short timeout suffices.
    for host in extra_hosts {
        let host = host.to_ascii_lowercase();
        if host == primary_host || host_cookies.contains_key(&host) {
            continue;
        }
        let Ok(host_url) = Url::parse(&format!("https://{host}/")) else {
            continue;
        };
        if !is_sharepoint_host(&host_url) {
            warn!("Refusing to visit non-SharePoint host during auth: {host}");
            continue;
        }
        on_status(&format!("Authorizing {host}…"));
        if let Err(e) = window.navigate(host_url.clone()) {
            warn!("Could not navigate auth window to {host}: {e}");
            continue;
        }
        match poll_for_auth(&window, &host_url, EXTRA_HOST_TIMEOUT).await {
            Ok(Some(cookies)) => {
                info!("Authenticated silently on {host}");
                host_cookies.insert(host, cookies);
            }
            Ok(None) => warn!("No auth cookies appeared for {host}; continuing without it"),
            Err(e) => warn!("Cookie read failed for {host}: {e}"),
        }
    }

    let _ = window.close();
    Ok(MultiHostAuth { host_cookies })
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
    ensure_auth_cookies_mode(app, target_url, AuthMode::AllowInteractive, on_status).await
}

/// [`ensure_auth_cookies`] with explicit control over interactive sign-in.
pub async fn ensure_auth_cookies_mode<R: Runtime, F: Fn(&str)>(
    app: &AppHandle<R>,
    target_url: &str,
    mode: AuthMode,
    on_status: F,
) -> Result<AuthCookies> {
    let url = Url::parse(target_url).context("The recording link is not a valid URL")?;
    if !is_sharepoint_host(&url) {
        return Err(anyhow!(
            "This link is not a SharePoint/Stream recording URL (expected a *.sharepoint.com host)."
        ));
    }

    let _flow = AUTH_FLOW_LOCK.lock().await;

    let data_dir = crate::storage::webview_dir();

    on_status("Connecting to SharePoint…");

    let window = create_auth_window(app, &url, &data_dir).await?;

    // Phase 1: silent — maybe the persisted session is still valid.
    if let Some(cookies) = poll_for_auth(&window, &url, SILENT_TIMEOUT).await? {
        info!("SharePoint session authenticated silently");
        let _ = window.close();
        return write_cookies(app, &url, cookies).await;
    }

    // Background callers stop here rather than interrupting the user.
    if mode == AuthMode::SilentOnly {
        let _ = window.close();
        return Err(anyhow!(
            "{AUTH_REQUIRED_MARKER}: SharePoint sign-in required for {}",
            url.host_str().unwrap_or("SharePoint")
        ));
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

/// Create the hidden auth webview, evicting any leftover window first.
///
/// Eviction uses `destroy()`, not `close()`: a webview whose browser process
/// already died (e.g. after a cookie-read panic) ignores the polite close and
/// squats on the label forever, making every subsequent auth flow fail with
/// "Failed to open the SharePoint sign-in window" until the app restarts
/// (observed 2026-07-28). Creation is retried once after a longer settle for
/// the same reason.
async fn create_auth_window<R: Runtime>(
    app: &AppHandle<R>,
    url: &Url,
    data_dir: &std::path::Path,
) -> Result<tauri::WebviewWindow<R>> {
    if let Some(existing) = app.get_webview_window(AUTH_WINDOW_LABEL) {
        let _ = existing.destroy();
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    match build_auth_window(app, url, data_dir) {
        Ok(w) => Ok(w),
        Err(first) => {
            warn!("Auth window creation failed ({first}); destroying leftovers and retrying once");
            if let Some(existing) = app.get_webview_window(AUTH_WINDOW_LABEL) {
                let _ = existing.destroy();
            }
            tokio::time::sleep(Duration::from_millis(700)).await;
            build_auth_window(app, url, data_dir)
                .context("Failed to open the SharePoint sign-in window")
        }
    }
}

fn build_auth_window<R: Runtime>(
    app: &AppHandle<R>,
    url: &Url,
    data_dir: &std::path::Path,
) -> tauri::Result<tauri::WebviewWindow<R>> {
    WebviewWindowBuilder::new(app, AUTH_WINDOW_LABEL, WebviewUrl::External(url.clone()))
        .title("Sign in to SharePoint — Meetily")
        .inner_size(1024.0, 768.0)
        .data_directory(data_dir.to_path_buf())
        .visible(false)
        .build()
}

/// Build a yt-dlp cookie file from cookies already harvested by a
/// [`ensure_multi_host_auth_mode`] flow in the same import, so the fallback
/// from a blocked direct download to the player stream does not run a second
/// sign-in (each flow opens its own webview, and the fresh session is not
/// always visible to the next one immediately — observed re-prompting the
/// user 90 seconds after a successful login).
pub async fn auth_cookies_from_harvest<R: Runtime>(
    app: &AppHandle<R>,
    target_url: &str,
    cookies: Vec<Cookie<'static>>,
) -> Result<AuthCookies> {
    let url = Url::parse(target_url).context("The recording link is not a valid URL")?;
    write_cookies(app, &url, cookies).await
}

/// Poll the webview's cookie store until an auth cookie appears **and the
/// server still accepts it**, or `timeout` elapses. Cookie reads are done on a
/// blocking thread because the webview runtime blocks the caller while it
/// services the request on the main thread.
///
/// Presence alone is not enough: the persisted webview profile keeps FedAuth
/// cookies long after the server has expired them, and a stale cookie passing
/// the silent check means the sign-in window is never offered while every
/// download bounces to login (observed 2026-07-27). Rejected sets are
/// remembered by fingerprint so we only re-probe them occasionally — the
/// webview may complete a silent SSO refresh behind our back at any time.
async fn poll_for_auth<R: Runtime>(
    window: &tauri::WebviewWindow<R>,
    url: &Url,
    timeout: Duration,
) -> Result<Option<Vec<Cookie<'static>>>> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut rejected_fp = String::new();
    let mut last_probe: Option<tokio::time::Instant> = None;
    let mut read_failures: u32 = 0;
    loop {
        // A failed read is not fatal by itself: the webview can be mid-navigation
        // (SSO redirects) and drop the request. Only give up when the reads fail
        // repeatedly — that means the window itself is gone.
        let cookies = match read_cookies(window, url).await {
            Ok(c) => {
                read_failures = 0;
                c
            }
            Err(e) => {
                read_failures += 1;
                if read_failures == 1 {
                    warn!("Cookie read failed (will keep polling): {e}");
                }
                if read_failures >= 5 {
                    return Err(e.context("The SharePoint sign-in window is not responding"));
                }
                Vec::new()
            }
        };
        if has_auth_cookie(&cookies) {
            let fp = auth_fingerprint(&cookies);
            let probe_due = last_probe
                .map(|t| t.elapsed() >= VALIDATE_EVERY)
                .unwrap_or(true);
            if fp != rejected_fp || probe_due {
                last_probe = Some(tokio::time::Instant::now());
                if session_is_valid(url, &cookies).await {
                    return Ok(Some(cookies));
                }
                debug!("SharePoint rejected the current cookie set; waiting for a fresh sign-in");
                rejected_fp = fp;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(None);
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Opaque fingerprint of the auth cookies, to notice when a sign-in mints new
/// tokens. Compared, never logged.
fn auth_fingerprint(cookies: &[Cookie<'static>]) -> String {
    let mut parts: Vec<String> = cookies
        .iter()
        .filter(|c| AUTH_COOKIE_NAMES.contains(&c.name().to_ascii_lowercase().as_str()))
        .map(|c| format!("{}={}", c.name(), c.value()))
        .collect();
    parts.sort();
    parts.join(";")
}

/// Ask SharePoint whether this cookie set is still a live session. Only a
/// sign-in bounce or a 401 counts as dead — a page, a viewer redirect, or even
/// AccessDenied all prove the session itself is alive.
async fn session_is_valid(url: &Url, cookies: &[Cookie<'static>]) -> bool {
    let header = cookies
        .iter()
        .map(|c| format!("{}={}", c.name(), c.value()))
        .collect::<Vec<_>>()
        .join("; ");
    let client = match reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(_) => return true, // can't probe — don't block the flow
    };
    let resp = match client
        .get(url.clone())
        .header(reqwest::header::COOKIE, header)
        // Answer 401/403 instead of an HTML sign-in bounce where possible.
        .header("X-FORMS_BASED_AUTH_ACCEPTED", "f")
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!("Session validation probe failed ({e}); assuming the session is usable");
            return true;
        }
    };
    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return false;
    }
    if status.is_redirection() {
        let target = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        return !matches!(classify_redirect(target), RedirectVerdict::Auth);
    }
    true
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
    _app: &AppHandle<R>,
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

    let dir = crate::storage::tmp_dir();
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
pub fn sweep_stale_cookie_files<R: tauri::Runtime>(_app: &tauri::AppHandle<R>) {
    let Ok(entries) = std::fs::read_dir(crate::storage::tmp_dir()) else {
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
