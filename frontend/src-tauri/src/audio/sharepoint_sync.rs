// SharePoint video-hub sync: enumerate meeting recordings newer than a date.
//
// The Stream-on-SharePoint "video hub" (`/_layouts/15/videohub.aspx`) is a
// view over files in SharePoint/OneDrive document libraries — Teams meeting
// recordings land in the user's OneDrive `Documents/Recordings` folder on the
// tenant's `-my.sharepoint.com` host. Enumeration therefore goes straight to
// the SharePoint REST API using the auth cookies harvested by
// `sharepoint::ensure_multi_host_auth`:
//
//   (a) primary: list the OneDrive Recordings folder on the -my host,
//       filtered server-side by TimeCreated;
//   (b) fallback: the Search REST API on the root host (works with root
//       cookies only, tolerates search-index lag).
//
// Each hit's stream.aspx URL feeds the existing URL-import pipeline.
//
// SENSITIVE DATA: cookie values must never be logged or persisted beyond the
// existing cookies flow; log file names/paths/dates only.

use anyhow::{anyhow, Context, Result};
use log::{info, warn};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tauri::webview::Cookie;
use tauri::{AppHandle, Runtime};
use tauri_plugin_store::StoreExt;

/// One recording found on SharePoint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SharePointRecording {
    /// File name, e.g. "Weekly sync-20260721_100221-Meeting Recording.mp4".
    pub name: String,
    /// Absolute URL of the media file itself.
    pub file_url: String,
    /// The stream.aspx page for this file — what the URL importer takes.
    pub stream_url: String,
    /// ISO timestamp the file was created (recording end time).
    pub created: String,
    pub size_bytes: Option<u64>,
}

/// `tenant.sharepoint.com` → `tenant-my.sharepoint.com`. Returns None when
/// the host is already a -my host or not a *.sharepoint.com/us host.
pub(crate) fn derive_my_host(host: &str) -> Option<String> {
    let host = host.to_ascii_lowercase();
    let (tenant, suffix) = host
        .strip_suffix(".sharepoint.com")
        .map(|t| (t, ".sharepoint.com"))
        .or_else(|| host.strip_suffix(".sharepoint.us").map(|t| (t, ".sharepoint.us")))?;
    if tenant.is_empty() || tenant.contains('.') || tenant.ends_with("-my") {
        return None;
    }
    Some(format!("{tenant}-my{suffix}"))
}

/// `i:0#.f|membership|user@corp.com` (or a bare email) → `/personal/user_corp_com`.
pub(crate) fn personal_path_from_login(login: &str) -> Option<String> {
    let upn = login.rsplit('|').next().unwrap_or(login).trim();
    if upn.is_empty() || !upn.contains('@') {
        return None;
    }
    let mangled: String = upn
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c == '@' || c == '.' || c == '-' { '_' } else { c })
        .collect();
    Some(format!("/personal/{mangled}"))
}

/// OData string literals escape `'` by doubling it.
fn odata_escape(value: &str) -> String {
    value.replace('\'', "''")
}

/// REST URL listing the OneDrive Recordings folder, newest first, filtered
/// server-side to files created at/after `since_iso` (RFC3339, UTC).
pub(crate) fn build_recordings_query_url(
    my_host: &str,
    personal_path: &str,
    since_iso: &str,
) -> String {
    let folder = format!("{personal_path}/Documents/Recordings");
    format!(
        "https://{my_host}{personal_path}/_api/web/GetFolderByServerRelativeUrl('{}')/Files?\
         $select=Name,ServerRelativeUrl,TimeCreated,Length&\
         $filter=TimeCreated%20ge%20datetime'{}'&\
         $orderby=TimeCreated%20desc&$top=200",
        // The path segment goes inside an OData string literal; spaces and
        // unicode are fine there, but quotes must be doubled.
        odata_escape(&folder),
        odata_escape(since_iso),
    )
}

/// Search REST URL on the root host: video files newest first, optionally
/// scoped to the user's personal site. Search permission-trims results to
/// what the caller can read, so the unscoped variant is still safe — it just
/// also surfaces recordings shared from other people's OneDrives.
pub(crate) fn build_search_query_url(
    root_host: &str,
    my_host: Option<&str>,
    since_iso: &str,
) -> String {
    let date = since_iso.split('T').next().unwrap_or(since_iso);
    let scope = my_host
        .map(|h| format!(" AND Path:\"https://{h}/personal\""))
        .unwrap_or_default();
    let query =
        format!("(FileExtension:mp4 OR FileExtension:webm) AND LastModifiedTime>={date}{scope}");
    // querytext is a single-quoted OData string literal carrying KQL; percent-
    // encode the payload and double any single quotes.
    let encoded: String = url::form_urlencoded::byte_serialize(query.replace('\'', "''").as_bytes())
        .collect::<String>()
        .replace('+', "%20");
    format!(
        "https://{root_host}/_api/search/query?querytext='{encoded}'&rowlimit=200&\
         selectproperties='Title,Path,LastModifiedTime,Size'&\
         sortlist='LastModifiedTime:descending'"
    )
}

/// Percent-encode a server-relative path for use in a URL, keeping `/`.
fn encode_server_relative_path(path: &str) -> String {
    path.chars()
        .map(|c| match c {
            ' ' => "%20".to_string(),
            '\'' => "%27".to_string(),
            '#' => "%23".to_string(),
            '&' => "%26".to_string(),
            '+' => "%2B".to_string(),
            _ => c.to_string(),
        })
        .collect()
}

/// The stream.aspx player page for a OneDrive file — the URL shape the
/// existing yt-dlp import path understands.
pub(crate) fn stream_url_for(my_host: &str, server_relative_path: &str) -> String {
    format!(
        "https://{my_host}/_layouts/15/stream.aspx?id={}",
        encode_server_relative_path(server_relative_path)
    )
}

/// Parse the OneDrive folder listing (odata=nometadata shape). The folder
/// holds more than recordings — Teams also drops transcript documents,
/// shortcuts, etc. there — so only media files are kept.
pub(crate) fn parse_onedrive_files(
    json: &serde_json::Value,
    my_host: &str,
) -> Vec<SharePointRecording> {
    let Some(items) = json.get("value").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let name = item.get("Name")?.as_str()?.to_string();
            if !has_media_extension(&name) {
                return None;
            }
            let rel = item.get("ServerRelativeUrl")?.as_str()?.to_string();
            let created = item
                .get("TimeCreated")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let size_bytes = item.get("Length").and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            });
            Some(SharePointRecording {
                stream_url: stream_url_for(my_host, &rel),
                file_url: format!("https://{my_host}{rel}"),
                name,
                created,
                size_bytes,
            })
        })
        .collect()
}

/// Parse Search REST results (odata=nometadata shape): rows of Key/Value cells.
pub(crate) fn parse_search_results(json: &serde_json::Value) -> Vec<SharePointRecording> {
    let rows = json
        .pointer("/PrimaryQueryResult/RelevantResults/Table/Rows")
        .and_then(|v| v.as_array());
    let Some(rows) = rows else {
        return Vec::new();
    };

    rows.iter()
        .filter_map(|row| {
            let cells = row.get("Cells")?.as_array()?;
            let get = |key: &str| -> Option<String> {
                cells.iter().find_map(|c| {
                    (c.get("Key")?.as_str()? == key)
                        .then(|| c.get("Value")?.as_str().map(str::to_string))
                        .flatten()
                })
            };
            let path = get("Path")?;
            let url = url::Url::parse(&path).ok()?;
            let host = url.host_str()?.to_string();
            let name = path.rsplit('/').next().unwrap_or("recording").to_string();
            Some(SharePointRecording {
                stream_url: stream_url_for(&host, url.path()),
                file_url: path,
                name,
                created: get("LastModifiedTime").unwrap_or_default(),
                size_bytes: get("Size").and_then(|s| s.parse().ok()),
            })
        })
        .collect()
}

/// Extensions we can hand to the audio import pipeline after downloading.
const DIRECT_MEDIA_EXTENSIONS: &[&str] = &[
    ".mp4", ".m4a", ".mp3", ".wav", ".webm", ".mkv", ".mov", ".ogg", ".flac", ".wma",
];

fn has_media_extension(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    DIRECT_MEDIA_EXTENSIONS.iter().any(|ext| lower.ends_with(ext))
}

/// Decode %XX sequences in a URL path component (no `+`-as-space).
pub(crate) fn percent_decode_component(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&input[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// If `raw` points straight at a media file on a SharePoint host — either a
/// direct file URL or a `stream.aspx?id=<path>` player page wrapping one —
/// return the direct file URL. Such files can be downloaded with a plain
/// authenticated GET, skipping yt-dlp (whose Stream page scraping breaks on
/// newer SharePoint UIs).
pub(crate) fn direct_sharepoint_media_url(raw: &str) -> Option<url::Url> {
    let parsed = url::Url::parse(raw).ok()?;
    let host = parsed.host_str()?.to_ascii_lowercase();
    if !(host.ends_with(".sharepoint.com") || host.ends_with(".sharepoint.us")) {
        return None;
    }

    if has_media_extension(parsed.path()) {
        return Some(parsed);
    }

    if parsed.path().to_ascii_lowercase().ends_with("/stream.aspx") {
        let id = parsed
            .query_pairs()
            .find(|(k, _)| k == "id")
            .map(|(_, v)| v.into_owned())?;
        if id.starts_with('/') && has_media_extension(&id) {
            return url::Url::parse(&format!(
                "https://{host}{}",
                encode_server_relative_path(&id)
            ))
            .ok();
        }
    }
    None
}

/// Download a direct SharePoint file with the host's auth cookies, streaming
/// to `work_dir` with percentage progress and cancellation.
pub(crate) async fn download_direct_file<F: Fn(u32)>(
    file_url: &url::Url,
    cookie_header: &str,
    work_dir: &std::path::Path,
    on_progress: F,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<std::path::PathBuf> {
    std::fs::create_dir_all(work_dir)
        .with_context(|| format!("Could not create work dir {}", work_dir.display()))?;

    let client = sp_client()?;
    let mut response = client
        .get(file_url.as_str())
        .header("Cookie", cookie_header)
        .send()
        .await
        .context("The download request to SharePoint failed")?;

    let status = response.status();
    if status.is_redirection() {
        return Err(anyhow!(
            "SharePoint redirected the download ({status}) — the sign-in was not accepted. Please try again."
        ));
    }
    if !status.is_success() {
        return Err(anyhow!(
            "Could not download the recording: SharePoint returned {status}. It may be inaccessible, deleted, or require different permissions."
        ));
    }
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if content_type.starts_with("text/html") {
        return Err(anyhow!(
            "SharePoint answered with a sign-in page instead of the recording. Please try again."
        ));
    }

    let total = response.content_length();
    let raw_name = file_url
        .path_segments()
        .and_then(|mut s| s.next_back())
        .filter(|s| !s.is_empty())
        .map(percent_decode_component)
        .unwrap_or_else(|| "recording.mp4".to_string());
    // Keep the name Windows-safe.
    let file_name: String = raw_name
        .chars()
        .map(|c| match c {
            '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
            _ => c,
        })
        .collect();
    let dest = work_dir.join(file_name);

    let mut file = tokio::fs::File::create(&dest)
        .await
        .with_context(|| format!("Could not create {}", dest.display()))?;
    let mut downloaded: u64 = 0;
    let mut last_pct: u32 = 0;
    while let Some(chunk) = response
        .chunk()
        .await
        .context("The download stream from SharePoint failed")?
    {
        if cancel.is_cancelled() {
            drop(file);
            let _ = tokio::fs::remove_file(&dest).await;
            return Err(anyhow!("Import cancelled"));
        }
        tokio::io::AsyncWriteExt::write_all(&mut file, &chunk)
            .await
            .context("Could not write the downloaded recording to disk")?;
        downloaded += chunk.len() as u64;
        if let Some(total) = total {
            let pct = ((downloaded.saturating_mul(100)) / total.max(1)).min(100) as u32;
            if pct != last_pct {
                last_pct = pct;
                on_progress(pct);
            }
        }
    }
    tokio::io::AsyncWriteExt::flush(&mut file)
        .await
        .context("Could not finish writing the downloaded recording")?;
    info!(
        "Direct SharePoint download complete: {} ({downloaded} bytes)",
        dest.display()
    );
    Ok(dest)
}

/// `Cookie:` header value from harvested webview cookies.
pub(crate) fn build_cookie_header(cookies: &[Cookie<'static>]) -> String {
    cookies
        .iter()
        .map(|c| format!("{}={}", c.name(), c.value()))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Reqwest client for SharePoint REST: redirects disabled so an auth bounce
/// to login.microsoftonline.com shows up as a diagnosable 3xx instead of a
/// confusing error deep in the redirect chain (reqwest also strips the Cookie
/// header on cross-host redirects, which would guarantee failure anyway).
fn sp_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) Meetily")
        .build()
        .context("Could not build HTTP client")
}

/// Authenticated JSON GET against SharePoint REST.
async fn sp_get_json(
    client: &reqwest::Client,
    url: &str,
    cookie_header: &str,
) -> Result<serde_json::Value> {
    let response = client
        .get(url)
        .header("Cookie", cookie_header)
        .header("Accept", "application/json;odata=nometadata")
        .send()
        .await
        .context("SharePoint request failed")?;

    let status = response.status();
    if status.is_redirection() {
        let target_host = response
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .and_then(|loc| url::Url::parse(loc).ok())
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_else(|| "<unknown>".to_string());
        return Err(anyhow!(
            "SharePoint redirected ({status}) to {target_host} — cookies not accepted for this host"
        ));
    }
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        // Log the status and a short, cookie-free prefix of the body.
        let prefix: String = body.chars().take(300).collect();
        return Err(anyhow!("SharePoint returned {status}: {prefix}"));
    }
    serde_json::from_str(&body).context("SharePoint response was not valid JSON")
}

/// Primary enumeration: OneDrive `Documents/Recordings` on the -my host.
pub(crate) async fn enumerate_onedrive_recordings(
    client: &reqwest::Client,
    my_host: &str,
    cookies: &[Cookie<'static>],
    since_iso: &str,
) -> Result<Vec<SharePointRecording>> {
    let cookie_header = build_cookie_header(cookies);

    // Discover the personal-site path from the signed-in identity.
    let me = sp_get_json(
        client,
        &format!("https://{my_host}/_api/web/currentuser?$select=LoginName,Email"),
        &cookie_header,
    )
    .await
    .context("Could not resolve the signed-in user on the OneDrive host")?;

    let login = me
        .get("LoginName")
        .and_then(|v| v.as_str())
        .or_else(|| me.get("Email").and_then(|v| v.as_str()))
        .ok_or_else(|| anyhow!("SharePoint did not return a usable login name"))?;
    let personal_path = personal_path_from_login(login)
        .ok_or_else(|| anyhow!("Could not derive a personal site path from the login name"))?;
    info!("OneDrive personal site path: {personal_path}");

    let listing = sp_get_json(
        client,
        &build_recordings_query_url(my_host, &personal_path, since_iso),
        &cookie_header,
    )
    .await
    .context("Could not list the OneDrive Recordings folder")?;

    Ok(parse_onedrive_files(&listing, my_host))
}

/// Fallback enumeration: Search REST on the root host. Tries the query
/// scoped to the user's personal site first; if that returns nothing (Path
/// scoping is finicky across tenants), retries unscoped — permission
/// trimming still limits results to what the user can read.
pub(crate) async fn enumerate_via_search(
    client: &reqwest::Client,
    root_host: &str,
    my_host: &str,
    cookies: &[Cookie<'static>],
    since_iso: &str,
) -> Result<Vec<SharePointRecording>> {
    let cookie_header = build_cookie_header(cookies);
    let scoped = sp_get_json(
        client,
        &build_search_query_url(root_host, Some(my_host), since_iso),
        &cookie_header,
    )
    .await
    .context("SharePoint search query failed")?;
    let recordings = parse_search_results(&scoped);
    if !recordings.is_empty() {
        return Ok(recordings);
    }

    info!("Scoped SharePoint search returned nothing; retrying unscoped");
    let unscoped = sp_get_json(
        client,
        &build_search_query_url(root_host, None, since_iso),
        &cookie_header,
    )
    .await
    .context("Unscoped SharePoint search query failed")?;
    Ok(parse_search_results(&unscoped))
}

/// Spike/debug command: authenticate, try both enumeration strategies against
/// the real tenant, and report what worked. Logs names/dates/paths only —
/// never cookie material. Invoke from devtools:
///   __TAURI__.core.invoke('sharepoint_enumerate_recordings_debug_command',
///     { hubUrl: 'https://tenant.sharepoint.com/_layouts/15/videohub.aspx',
///       sinceIso: '2026-07-01T00:00:00Z' })
#[tauri::command]
pub async fn sharepoint_enumerate_recordings_debug_command<R: Runtime>(
    app: AppHandle<R>,
    hub_url: String,
    since_iso: String,
) -> Result<serde_json::Value, String> {
    let url = url::Url::parse(&hub_url).map_err(|e| format!("Invalid hub URL: {e}"))?;
    let root_host = url
        .host_str()
        .ok_or("Hub URL has no host")?
        .to_ascii_lowercase();
    let my_host = derive_my_host(&root_host)
        .ok_or("Could not derive the -my.sharepoint.com host from the hub URL")?;

    let auth = super::sharepoint::ensure_multi_host_auth(
        &app,
        &hub_url,
        &[my_host.clone()],
        |msg| info!("[sp-sync spike] {msg}"),
    )
    .await
    .map_err(|e| e.to_string())?;

    let client = sp_client().map_err(|e| e.to_string())?;
    let empty: Vec<Cookie<'static>> = Vec::new();

    // Diagnostics: which cookie NAMES each host yielded (values never leave
    // the auth layer's cookies flow — do not add them here).
    let cookie_names: serde_json::Value = auth
        .host_cookies
        .iter()
        .map(|(host, cookies)| {
            (
                host.clone(),
                serde_json::json!(cookies.iter().map(|c| c.name()).collect::<Vec<_>>()),
            )
        })
        .collect::<serde_json::Map<String, serde_json::Value>>()
        .into();

    // Strategy (a): OneDrive folder listing on the -my host.
    let my_cookies = auth.host_cookies.get(&my_host).unwrap_or(&empty);
    let onedrive = if my_cookies.is_empty() {
        Err(anyhow!("no cookies harvested for {my_host}"))
    } else {
        enumerate_onedrive_recordings(&client, &my_host, my_cookies, &since_iso).await
    };
    match &onedrive {
        Ok(list) => {
            info!("[sp-sync spike] OneDrive listing OK: {} recording(s)", list.len());
            for r in list.iter().take(10) {
                info!("[sp-sync spike]   {} ({})", r.name, r.created);
            }
        }
        Err(e) => warn!("[sp-sync spike] OneDrive listing failed: {e:#}"),
    }

    // Strategy (b): Search REST on the root host.
    let root_cookies = auth.host_cookies.get(&root_host).unwrap_or(&empty);
    let search = enumerate_via_search(&client, &root_host, &my_host, root_cookies, &since_iso).await;
    match &search {
        Ok(list) => {
            info!("[sp-sync spike] Search REST OK: {} recording(s)", list.len());
            for r in list.iter().take(10) {
                info!("[sp-sync spike]   {} ({})", r.name, r.created);
            }
        }
        Err(e) => warn!("[sp-sync spike] Search REST failed: {e:#}"),
    }

    Ok(serde_json::json!({
        "onedrive": match onedrive {
            Ok(list) => serde_json::json!({ "ok": true, "count": list.len(), "recordings": list }),
            Err(e) => serde_json::json!({ "ok": false, "error": format!("{e:#}") }),
        },
        "search": match search {
            Ok(list) => serde_json::json!({ "ok": true, "count": list.len(), "recordings": list }),
            Err(e) => serde_json::json!({ "ok": false, "error": format!("{e:#}") }),
        },
        "cookieNames": cookie_names,
    }))
}

// ---------------------------------------------------------------------------
// Sync state: hub URL, last sync date, and which recordings were already
// imported. Stored in sharepoint_sync.json via tauri-plugin-store. Holds only
// hostnames, server-relative paths, names, and dates — never cookies or
// token-bearing query strings.
// ---------------------------------------------------------------------------

const SYNC_STORE_FILE: &str = "sharepoint_sync.json";
const SYNC_STORE_KEY: &str = "state";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SharePointSyncState {
    #[serde(default)]
    pub hub_url: Option<String>,
    /// ISO date of the last confirmed sync; the next scan defaults to it.
    #[serde(default)]
    pub last_sync_date: Option<String>,
    /// file_url -> ISO timestamp the import was queued.
    #[serde(default)]
    pub imported: HashMap<String, String>,
}

fn load_sync_state<R: Runtime>(app: &AppHandle<R>) -> SharePointSyncState {
    let Ok(store) = app.store(SYNC_STORE_FILE) else {
        return SharePointSyncState::default();
    };
    store
        .get(SYNC_STORE_KEY)
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

fn save_sync_state<R: Runtime>(app: &AppHandle<R>, state: &SharePointSyncState) -> Result<()> {
    let store = app
        .store(SYNC_STORE_FILE)
        .map_err(|e| anyhow!("Could not open the sync store: {e}"))?;
    store.set(
        SYNC_STORE_KEY,
        serde_json::to_value(state).context("Could not serialize sync state")?,
    );
    store
        .save()
        .map_err(|e| anyhow!("Could not persist the sync store: {e}"))
}

#[tauri::command]
pub async fn get_sharepoint_sync_state_command<R: Runtime>(
    app: AppHandle<R>,
) -> Result<SharePointSyncState, String> {
    Ok(load_sync_state(&app))
}

#[tauri::command]
pub async fn set_sharepoint_sync_prefs_command<R: Runtime>(
    app: AppHandle<R>,
    hub_url: Option<String>,
    last_sync_date: Option<String>,
) -> Result<(), String> {
    let mut state = load_sync_state(&app);
    if hub_url.is_some() {
        state.hub_url = hub_url;
    }
    if last_sync_date.is_some() {
        state.last_sync_date = last_sync_date;
    }
    save_sync_state(&app, &state).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn mark_sharepoint_imported_command<R: Runtime>(
    app: AppHandle<R>,
    file_url: String,
) -> Result<(), String> {
    let mut state = load_sync_state(&app);
    state
        .imported
        .insert(file_url, chrono::Utc::now().to_rfc3339());
    save_sync_state(&app, &state).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Production scan command
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct SharePointScanItem {
    #[serde(flatten)]
    pub recording: SharePointRecording,
    pub already_imported: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SharePointScanResult {
    /// Which enumeration strategy produced the list: "onedrive" or "search".
    pub strategy: String,
    pub recordings: Vec<SharePointScanItem>,
}

/// Enumerate recordings newer than `since_iso`, annotated with whether each
/// was already imported through this feature. OneDrive listing is the primary
/// strategy; Search REST is the fallback (known to return nothing on some
/// tenants — its failure only matters if OneDrive also failed).
#[tauri::command]
pub async fn sharepoint_scan_recordings_command<R: Runtime>(
    app: AppHandle<R>,
    hub_url: String,
    since_iso: String,
) -> Result<SharePointScanResult, String> {
    let url = url::Url::parse(&hub_url).map_err(|e| format!("Invalid hub URL: {e}"))?;
    let root_host = url
        .host_str()
        .ok_or("Hub URL has no host")?
        .to_ascii_lowercase();
    let my_host = derive_my_host(&root_host)
        .ok_or("Could not derive the OneDrive host from the hub URL")?;

    let auth = super::sharepoint::ensure_multi_host_auth(
        &app,
        &hub_url,
        &[my_host.clone()],
        |msg| info!("[sp-scan] {msg}"),
    )
    .await
    .map_err(|e| e.to_string())?;

    let client = sp_client().map_err(|e| e.to_string())?;
    let empty: Vec<Cookie<'static>> = Vec::new();

    let my_cookies = auth.host_cookies.get(&my_host).unwrap_or(&empty);
    let (strategy, recordings) = if my_cookies.is_empty() {
        ("search", None)
    } else {
        match enumerate_onedrive_recordings(&client, &my_host, my_cookies, &since_iso).await {
            Ok(list) => ("onedrive", Some(list)),
            Err(e) => {
                warn!("[sp-scan] OneDrive enumeration failed, falling back to search: {e:#}");
                ("search", None)
            }
        }
    };

    let recordings = match recordings {
        Some(list) => list,
        None => {
            let root_cookies = auth.host_cookies.get(&root_host).unwrap_or(&empty);
            enumerate_via_search(&client, &root_host, &my_host, root_cookies, &since_iso)
                .await
                .map_err(|e| format!("Both enumeration strategies failed: {e:#}"))?
        }
    };
    info!(
        "[sp-scan] {} recording(s) via {strategy} since {since_iso}",
        recordings.len()
    );

    let imported = load_sync_state(&app).imported;
    Ok(SharePointScanResult {
        strategy: strategy.to_string(),
        recordings: recordings
            .into_iter()
            .map(|recording| SharePointScanItem {
                already_imported: imported.contains_key(&recording.file_url),
                recording,
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_the_my_host_from_a_root_host() {
        assert_eq!(
            derive_my_host("mymurex.sharepoint.com").as_deref(),
            Some("mymurex-my.sharepoint.com")
        );
        assert_eq!(
            derive_my_host("Tenant.SharePoint.US").as_deref(),
            Some("tenant-my.sharepoint.us")
        );
        assert_eq!(derive_my_host("tenant-my.sharepoint.com"), None);
        assert_eq!(derive_my_host("example.com"), None);
        assert_eq!(derive_my_host(".sharepoint.com"), None);
    }

    #[test]
    fn personal_path_mangles_upn_like_sharepoint_does() {
        assert_eq!(
            personal_path_from_login("i:0#.f|membership|Jane.Doe@my-corp.com").as_deref(),
            Some("/personal/jane_doe_my_corp_com")
        );
        assert_eq!(
            personal_path_from_login("user@corp.com").as_deref(),
            Some("/personal/user_corp_com")
        );
        assert_eq!(personal_path_from_login("not-an-upn"), None);
        assert_eq!(personal_path_from_login(""), None);
    }

    #[test]
    fn recordings_query_url_embeds_path_filter_and_order() {
        let url = build_recordings_query_url(
            "t-my.sharepoint.com",
            "/personal/user_corp_com",
            "2026-07-01T00:00:00Z",
        );
        assert!(url.starts_with(
            "https://t-my.sharepoint.com/personal/user_corp_com/_api/web/GetFolderByServerRelativeUrl"
        ));
        assert!(url.contains("'/personal/user_corp_com/Documents/Recordings'"));
        assert!(url.contains("$filter=TimeCreated%20ge%20datetime'2026-07-01T00:00:00Z'"));
        assert!(url.contains("$orderby=TimeCreated%20desc"));
    }

    #[test]
    fn odata_quotes_are_doubled_in_paths() {
        let url = build_recordings_query_url(
            "t-my.sharepoint.com",
            "/personal/o'brien_corp_com",
            "2026-07-01T00:00:00Z",
        );
        assert!(url.contains("o''brien"));
    }

    #[test]
    fn stream_url_escapes_reserved_characters() {
        let url = stream_url_for(
            "t-my.sharepoint.com",
            "/personal/u/Documents/Recordings/Team Sync & Q'A #1.mp4",
        );
        assert_eq!(
            url,
            "https://t-my.sharepoint.com/_layouts/15/stream.aspx?id=/personal/u/Documents/Recordings/Team%20Sync%20%26%20Q%27A%20%231.mp4"
        );
    }

    #[test]
    fn parses_a_onedrive_folder_listing() {
        let json: serde_json::Value = serde_json::from_str(
            r#"{"value":[
                {"Name":"Standup-20260721.mp4",
                 "ServerRelativeUrl":"/personal/u_c_com/Documents/Recordings/Standup-20260721.mp4",
                 "TimeCreated":"2026-07-21T10:02:21Z","Length":"52428800"},
                {"Name":"Planning.mp4",
                 "ServerRelativeUrl":"/personal/u_c_com/Documents/Recordings/Planning.mp4",
                 "TimeCreated":"2026-07-22T09:00:00Z","Length":1024}
            ]}"#,
        )
        .unwrap();

        let recs = parse_onedrive_files(&json, "t-my.sharepoint.com");
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].name, "Standup-20260721.mp4");
        assert_eq!(recs[0].size_bytes, Some(52_428_800));
        assert_eq!(recs[1].size_bytes, Some(1024));
        assert!(recs[0]
            .stream_url
            .starts_with("https://t-my.sharepoint.com/_layouts/15/stream.aspx?id="));
        assert_eq!(
            recs[0].file_url,
            "https://t-my.sharepoint.com/personal/u_c_com/Documents/Recordings/Standup-20260721.mp4"
        );
    }

    #[test]
    fn onedrive_parser_drops_non_media_files() {
        let json = serde_json::json!({"value": [
            {"Name": "Standup.mp4",
             "ServerRelativeUrl": "/personal/u/Documents/Recordings/Standup.mp4",
             "TimeCreated": "2026-07-21T10:00:00Z", "Length": 1000},
            {"Name": "Kickoff-Meeting Transcript.docx",
             "ServerRelativeUrl": "/personal/u/Documents/Recordings/Kickoff-Meeting Transcript.docx",
             "TimeCreated": "2026-05-12T10:00:00Z", "Length": 495_000},
            {"Name": "Recordings.url",
             "ServerRelativeUrl": "/personal/u/Documents/Recordings/Recordings.url",
             "TimeCreated": "2026-05-27T10:00:00Z", "Length": 9_900},
            {"Name": "notes.vtt",
             "ServerRelativeUrl": "/personal/u/Documents/Recordings/notes.vtt",
             "TimeCreated": "2026-05-01T10:00:00Z", "Length": 20_000}
        ]});

        let recs = parse_onedrive_files(&json, "t-my.sharepoint.com");
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].name, "Standup.mp4");
    }

    #[test]
    fn onedrive_parser_tolerates_missing_or_odd_shapes() {
        assert!(parse_onedrive_files(&serde_json::json!({}), "h").is_empty());
        assert!(parse_onedrive_files(&serde_json::json!({"value": "nope"}), "h").is_empty());
        let partial = serde_json::json!({"value": [{"Name": "x.mp4"}]});
        assert!(parse_onedrive_files(&partial, "h").is_empty());
    }

    #[test]
    fn direct_media_url_accepts_files_and_stream_pages() {
        // Direct file URL (raw spaces get encoded by the parser).
        let direct = direct_sharepoint_media_url(
            "https://t-my.sharepoint.com/personal/u/Documents/Recordings/Team Sync-Meeting Recording.mp4",
        )
        .expect("direct file should match");
        assert!(direct.path().ends_with("Recording.mp4"));

        // stream.aspx wrapping a media path.
        let wrapped = direct_sharepoint_media_url(
            "https://t-my.sharepoint.com/_layouts/15/stream.aspx?id=/personal/u/Documents/Recordings/Standup%20Notes.mp4&ga=1",
        )
        .expect("stream.aspx with media id should match");
        assert_eq!(wrapped.host_str(), Some("t-my.sharepoint.com"));
        assert!(wrapped.path().to_ascii_lowercase().ends_with(".mp4"));

        // Non-media and non-SharePoint URLs don't match.
        assert!(direct_sharepoint_media_url(
            "https://t.sharepoint.com/_layouts/15/videohub.aspx"
        )
        .is_none());
        assert!(direct_sharepoint_media_url("https://example.com/video.mp4").is_none());
        assert!(direct_sharepoint_media_url(
            "https://t.sharepoint.com/_layouts/15/stream.aspx?id=/sites/x/page.aspx"
        )
        .is_none());
    }

    #[test]
    fn percent_decoding_handles_encoded_and_malformed_input() {
        assert_eq!(
            percent_decode_component("Team%20Sync%20%26%20Q%27A.mp4"),
            "Team Sync & Q'A.mp4"
        );
        assert_eq!(percent_decode_component("plain.mp4"), "plain.mp4");
        // Malformed escapes pass through instead of panicking.
        assert_eq!(percent_decode_component("bad%zz%2"), "bad%zz%2");
    }

    #[test]
    fn search_query_url_encodes_kql_and_scopes_optionally() {
        let scoped = build_search_query_url(
            "t.sharepoint.com",
            Some("t-my.sharepoint.com"),
            "2026-07-01T00:00:00Z",
        );
        assert!(scoped.starts_with("https://t.sharepoint.com/_api/search/query?querytext='"));
        assert!(scoped.contains("LastModifiedTime%3E%3D2026-07-01"));
        assert!(scoped.contains("t-my.sharepoint.com%2Fpersonal"));
        // Raw spaces/quotes must not survive into the querytext literal.
        let literal = scoped.split("querytext='").nth(1).unwrap().split('\'').next().unwrap();
        assert!(!literal.contains(' ') && !literal.contains('"'));

        let unscoped =
            build_search_query_url("t.sharepoint.com", None, "2026-07-01T00:00:00Z");
        assert!(!unscoped.contains("personal"));
        assert!(unscoped.contains("FileExtension%3Amp4"));
    }

    #[test]
    fn parses_search_rest_rows() {
        let json: serde_json::Value = serde_json::from_str(
            r#"{"PrimaryQueryResult":{"RelevantResults":{"Table":{"Rows":[
                {"Cells":[
                    {"Key":"Path","Value":"https://t-my.sharepoint.com/personal/u/Documents/Recordings/A.mp4"},
                    {"Key":"LastModifiedTime","Value":"2026-07-20T08:00:00Z"},
                    {"Key":"Size","Value":"2048"}
                ]},
                {"Cells":[{"Key":"Title","Value":"no path row"}]}
            ]}}}}"#,
        )
        .unwrap();

        let recs = parse_search_results(&json);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].name, "A.mp4");
        assert_eq!(recs[0].created, "2026-07-20T08:00:00Z");
        assert_eq!(recs[0].size_bytes, Some(2048));
    }

    #[test]
    fn search_parser_tolerates_empty_results() {
        assert!(parse_search_results(&serde_json::json!({})).is_empty());
    }
}
