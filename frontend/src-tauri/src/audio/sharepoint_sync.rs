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
use tauri::{AppHandle, Manager, Runtime};
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

/// [`derive_my_host`] for callers outside this module (the pipeline's
/// sign-in command needs the same host list the scan authenticates).
pub fn derive_my_host_public(host: &str) -> Option<String> {
    derive_my_host(host)
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
        odata_escape(&odata_datetime(since_iso)),
    )
}

/// OData `datetime'…'` literals require a full ISO timestamp. Persisted sync
/// state can carry a bare date (`2026-07-27`), which SharePoint rejects with
/// InvalidClientQueryException — widen it to midnight UTC.
fn odata_datetime(since_iso: &str) -> String {
    if !since_iso.contains('T') {
        format!("{since_iso}T00:00:00Z")
    } else {
        since_iso.to_string()
    }
}

/// Search REST URL on the root host: tenant-wide video files newest first.
/// Search permission-trims results to what the caller can read, so this also
/// surfaces recordings shared from other people's OneDrives and team sites —
/// which is exactly what the video hub shows.
///
/// Tenant findings (validated 2026-07-25 via the probe ladder): `filetype:`
/// matches videos while `FileExtension:` silently returns zero for them, and
/// the `Path` cell for video rows is a `DispForm.aspx?ID=n` link rather than
/// the file — the direct URL lives in `DefaultEncodingURL`/`OriginalPath`,
/// so those are requested too.
pub(crate) fn build_search_query_url(root_host: &str, since_iso: &str) -> String {
    let date = since_iso.split('T').next().unwrap_or(since_iso);
    let query = format!("(filetype:mp4 OR filetype:webm) AND LastModifiedTime>={date}");
    // querytext is a single-quoted OData string literal carrying KQL; percent-
    // encode the payload and double any single quotes.
    let encoded: String = url::form_urlencoded::byte_serialize(query.replace('\'', "''").as_bytes())
        .collect::<String>()
        .replace('+', "%20");
    format!(
        "https://{root_host}/_api/search/query?querytext='{encoded}'&rowlimit=200&\
         selectproperties='Title,Path,OriginalPath,DefaultEncodingURL,LastModifiedTime,Created,Size'&\
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
/// For video rows the `Path` cell is often a `DispForm.aspx?ID=n` list-item
/// link, so the direct file URL is taken from the first of
/// `DefaultEncodingURL`/`OriginalPath`/`Path` that ends in a media extension;
/// rows where none does are dropped (they can't be downloaded directly).
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
            let (file_url, url) = ["DefaultEncodingURL", "OriginalPath", "Path"]
                .into_iter()
                .find_map(|key| {
                    let candidate = get(key)?;
                    let parsed = url::Url::parse(&candidate).ok()?;
                    has_media_extension(&percent_decode_component(parsed.path()))
                        .then_some((candidate, parsed))
                })?;
            let host = url.host_str()?.to_string();
            let file_name = percent_decode_component(
                url.path().rsplit('/').next().unwrap_or("recording"),
            );
            // Stream/town-hall videos are often stored under generic file
            // names ("audio.mp4"); the Title cell carries the real video
            // title, so prefer it and keep the extension from the file.
            let name = match get("Title").map(|t| t.trim().to_string()) {
                Some(title) if !title.is_empty() => {
                    let ext = file_name.rfind('.').map(|i| &file_name[i..]).unwrap_or("");
                    if title.to_lowercase().ends_with(&ext.to_lowercase()) {
                        title
                    } else {
                        format!("{title}{ext}")
                    }
                }
                _ => file_name,
            };
            Some(SharePointRecording {
                stream_url: stream_url_for(&host, &percent_decode_component(url.path())),
                file_url,
                name,
                created: get("Created")
                    .or_else(|| get("LastModifiedTime"))
                    .unwrap_or_default(),
                size_bytes: get("Size").and_then(|s| s.parse().ok()),
            })
        })
        .collect()
}

/// Canonical comparison key for a SharePoint file URL: percent-decoded and
/// lowercased, so the OneDrive listing (raw spaces) and Search REST (encoded
/// spaces) forms of the same file collide during dedupe.
pub(crate) fn file_url_key(file_url: &str) -> String {
    percent_decode_component(file_url).to_lowercase()
}

/// The meeting title a file would get when imported (mirrors the frontend's
/// `titleFromFileName`): the file name without its extension.
pub fn meeting_title_for(file_name: &str) -> String {
    let stem = match file_name.rfind('.') {
        Some(idx) if idx > 0 => &file_name[..idx],
        _ => file_name,
    };
    let stem = stem.trim();
    if stem.is_empty() {
        file_name.to_string()
    } else {
        stem.to_string()
    }
}

/// [`meeting_title_for`], lowercased for case-insensitive comparison against
/// existing meeting titles.
pub(crate) fn title_stem(file_name: &str) -> String {
    meeting_title_for(file_name).to_lowercase()
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

/// Marker embedded in the error when SharePoint answers but refuses to serve
/// the file bytes — the "view-only" case, where playback is allowed and
/// download is not. Callers match on it rather than on prose.
pub(crate) const DOWNLOAD_BLOCKED_MARKER: &str = "sharepoint-download-blocked";

/// Whether a download failed because SharePoint would not release the file,
/// as opposed to a network, session, or not-found failure. Such a recording
/// can still be captured from the player stream.
pub(crate) fn is_download_blocked_error(error: &str) -> bool {
    error.contains(DOWNLOAD_BLOCKED_MARKER)
}

/// The site collection a file lives in — `https://host/personal/<user>/`,
/// `https://host/sites/<name>/`, or the host root for anything else.
///
/// Both auth and the `download.aspx` handler are site-scoped: a shared
/// recording lives in someone else's site collection, so signing in at the
/// host root is not enough to guarantee a session cookie that site accepts.
pub(crate) fn site_collection_url(file_url: &url::Url) -> url::Url {
    let segments: Vec<&str> = file_url
        .path_segments()
        .map(|s| s.collect())
        .unwrap_or_default();
    let managed = segments.first().map(|s| s.to_ascii_lowercase());
    let path = match (managed.as_deref(), segments.get(1)) {
        (Some("personal" | "sites" | "teams"), Some(name)) if !name.is_empty() => {
            format!("/{}/{}/", segments[0], name)
        }
        _ => "/".to_string(),
    };

    let mut site = file_url.clone();
    site.set_query(None);
    site.set_fragment(None);
    site.set_path(&path);
    site
}

/// The Stream player page for a file, scoped to its own site collection —
/// the URL shape yt-dlp's `SharePoint` extractor matches.
///
/// When SharePoint blocks the download but allows playback, the player's
/// manifest is the only path to the audio, so this is what the yt-dlp
/// fallback is pointed at. Built from the file URL rather than reusing a
/// pasted link so it carries no `referrer=`/`nav=` query junk.
pub(crate) fn player_page_url(file_url: &url::Url) -> String {
    let site = site_collection_url(file_url);
    let server_relative = percent_decode_component(file_url.path());
    let encoded: String = url::form_urlencoded::byte_serialize(server_relative.as_bytes())
        .collect::<String>()
        .replace('+', "%20");
    format!("{site}_layouts/15/stream.aspx?id={encoded}")
}

/// URLs that may yield the bytes of a SharePoint media file, in preference
/// order.
///
/// A plain GET on a media path is not reliable: SharePoint intercepts it and
/// 302s into the Stream player page, which is HTML, not the file. The download
/// switches below ask for the bytes explicitly, so they come first; the bare
/// URL stays as a last resort because it does work on some libraries.
pub(crate) fn download_url_candidates(file_url: &url::Url) -> Vec<url::Url> {
    let mut with_switch = file_url.clone();
    with_switch.query_pairs_mut().append_pair("download", "1");

    let mut handler = site_collection_url(file_url);
    handler.set_path(&format!("{}_layouts/15/download.aspx", handler.path()));
    handler
        .query_pairs_mut()
        .append_pair("SourceUrl", file_url.as_str());

    vec![with_switch, handler, file_url.clone()]
}

/// What a 3xx off a download request means. Only the first two are worth
/// giving up over; the rest just mean this URL form was the wrong ask.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RedirectVerdict {
    /// Bounced to a sign-in page: our cookies were not accepted.
    Auth,
    /// Signed in, but not allowed to read the file.
    Denied,
    /// Bounced into the web player or library UI — HTML, never bytes.
    Viewer,
    /// An ordinary hop, usually to a short-lived pre-authenticated storage URL.
    Follow,
}

pub(crate) fn classify_redirect(location: &str) -> RedirectVerdict {
    let loc = location.to_ascii_lowercase();
    if loc.contains("accessdenied.aspx") {
        return RedirectVerdict::Denied;
    }
    if loc.contains("login.microsoftonline.")
        || loc.contains("login.microsoft.com")
        || loc.contains("/_layouts/15/authenticate.aspx")
        || loc.contains("/_forms/default.aspx")
        || loc.contains("/adfs/ls")
        || loc.contains("returnurl=")
    {
        return RedirectVerdict::Auth;
    }
    if loc.contains("stream.aspx")
        || loc.contains("embed.aspx")
        || loc.contains("onedrive.aspx")
        || loc.contains("allitems.aspx")
        || loc.contains("videoplayerpage.aspx")
    {
        return RedirectVerdict::Viewer;
    }
    RedirectVerdict::Follow
}

/// Host and path of a redirect target, for logs. The query is dropped: it can
/// carry access tokens on storage-CDN hops.
fn redirect_summary(url: &url::Url) -> String {
    format!("{}{}", url.host_str().unwrap_or("<unknown>"), url.path())
}

/// Outcome of asking one candidate URL for the file bytes.
enum FetchOutcome {
    /// Headers say this is the file; the body is ready to stream.
    Bytes(reqwest::Response),
    /// This URL form did not work, but another might. Carries a log reason.
    TryNext(String),
    /// Signed in, but SharePoint refused to hand over the bytes. Downloads
    /// can be blocked per-link, per-site, or by Conditional Access while
    /// playback stays allowed. Other URL forms are still worth trying, but if
    /// they all fail this is the reason worth reporting.
    Denied(String),
    /// No URL form will work — stop and tell the user why.
    Fatal(anyhow::Error),
}

/// How many hops to follow before treating a redirect chain as a loop.
const MAX_DOWNLOAD_HOPS: usize = 3;

/// Ask one candidate URL for the file bytes, following benign redirects.
async fn fetch_media_bytes(
    client: &reqwest::Client,
    candidate: &url::Url,
    cookie_header: &str,
) -> FetchOutcome {
    let mut current = candidate.clone();
    // Storage-CDN hops carry their own token in the URL; sending SharePoint
    // cookies to another host is both useless and a needless disclosure.
    let mut send_cookies = true;

    for _ in 0..=MAX_DOWNLOAD_HOPS {
        let mut request = client
            .get(current.as_str())
            .header("Accept", "*/*")
            // Tells SharePoint we are not a browser, so an unusable session
            // answers 401/403 instead of bouncing us to an HTML sign-in page.
            .header("X-FORMS_BASED_AUTH_ACCEPTED", "f");
        if send_cookies {
            request = request.header("Cookie", cookie_header);
        }

        let response = match request.send().await {
            Ok(response) => response,
            Err(e) => {
                return FetchOutcome::Fatal(
                    anyhow!(e).context("The download request to SharePoint failed"),
                )
            }
        };

        let status = response.status();
        if !status.is_redirection() {
            if status == reqwest::StatusCode::FORBIDDEN {
                return FetchOutcome::Denied(format!("{status}"));
            }
            if !status.is_success() {
                return FetchOutcome::TryNext(format!("{status}"));
            }
            let content_type = response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_ascii_lowercase();
            if content_type.starts_with("text/html") {
                return FetchOutcome::TryNext("answered with an HTML page".to_string());
            }
            return FetchOutcome::Bytes(response);
        }

        let location = response
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let Some(location) = location else {
            return FetchOutcome::TryNext(format!("{status} without a Location header"));
        };
        let Ok(next) = current.join(&location) else {
            return FetchOutcome::TryNext(format!("{status} to an unparseable Location"));
        };

        match classify_redirect(next.as_str()) {
            RedirectVerdict::Auth => {
                return FetchOutcome::Fatal(anyhow!(
                    "SharePoint sent the download to a sign-in page — the saved session was not accepted for this site. Sign in to SharePoint again and retry."
                ))
            }
            RedirectVerdict::Denied => {
                return FetchOutcome::Denied(format!(
                    "{status} to {} (download refused)",
                    redirect_summary(&next)
                ))
            }
            RedirectVerdict::Viewer => {
                return FetchOutcome::TryNext(format!(
                    "{status} to the web player at {}",
                    redirect_summary(&next)
                ))
            }
            RedirectVerdict::Follow => {
                send_cookies = next.host_str() == current.host_str();
                current = next;
            }
        }
    }

    FetchOutcome::TryNext(format!("more than {MAX_DOWNLOAD_HOPS} redirects"))
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
    let candidates = download_url_candidates(file_url);
    let total_candidates = candidates.len();
    let mut accepted = None;
    let mut last_reason = None;
    let mut denied = false;

    for (idx, candidate) in candidates.iter().enumerate() {
        let outcome = fetch_media_bytes(&client, candidate, cookie_header).await;
        let reason = match outcome {
            FetchOutcome::Bytes(response) => {
                accepted = Some(response);
                break;
            }
            FetchOutcome::Fatal(e) => return Err(e),
            FetchOutcome::TryNext(reason) => reason,
            FetchOutcome::Denied(reason) => {
                denied = true;
                reason
            }
        };
        warn!(
            "SharePoint download form {}/{total_candidates} ({}) rejected: {reason}",
            idx + 1,
            redirect_summary(candidate)
        );
        last_reason = Some(reason);
        if cancel.is_cancelled() {
            return Err(anyhow!("Import cancelled"));
        }
    }

    let mut response = accepted.ok_or_else(|| {
        let reason = last_reason.unwrap_or_else(|| "no download URL was accepted".to_string());
        if denied {
            anyhow!(
                "{DOWNLOAD_BLOCKED_MARKER}: SharePoint allows playing this recording but not downloading it ({reason}). Downloads can be blocked per share link, per site, or by a Teams recording policy."
            )
        } else {
            anyhow!(
                "Could not download the recording — SharePoint would not serve the file bytes ({reason}). It may be inaccessible, deleted, or need different permissions."
            )
        }
    })?;

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

// ---------------------------------------------------------------------------
// Search diagnostics probes (spike): the tenant's Search REST returned zero
// rows where the video hub clearly finds results, so the debug command runs a
// ladder of read-only query variants and reports enough response shape to
// tell a parse bug from a query bug from a blocked endpoint. Outputs carry
// search metadata only — never cookie material.
// ---------------------------------------------------------------------------

pub(crate) fn build_search_probe_url(root_host: &str, kql: &str, rowlimit: u32) -> String {
    let encoded: String = url::form_urlencoded::byte_serialize(kql.replace('\'', "''").as_bytes())
        .collect::<String>()
        .replace('+', "%20");
    format!(
        "https://{root_host}/_api/search/query?querytext='{encoded}'&rowlimit={rowlimit}&\
         selectproperties='Title,Path,OriginalPath,DefaultEncodingURL,FileExtension,FileType'"
    )
}

fn rows_len_at(json: &serde_json::Value, pointer: &str) -> Option<usize> {
    json.pointer(pointer).and_then(|v| v.as_array()).map(|a| a.len())
}

/// Summarize a search response body: status, top-level keys, row counts at
/// the pointer we parse today plus the alternate wrappings SPO uses, and a
/// short body prefix for anything the counts don't explain.
pub(crate) fn summarize_search_body(
    method: &str,
    kql: &str,
    status: u16,
    body: &str,
) -> serde_json::Value {
    let parsed: Option<serde_json::Value> = serde_json::from_str(body).ok();
    let top_keys: Vec<String> = parsed
        .as_ref()
        .and_then(|v| v.as_object())
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    let p = parsed.as_ref();
    let prefix: String = body.chars().take(400).collect();
    // Titles/paths of the first few hits, so a non-zero probe shows WHAT
    // matched (file names and site paths only — no secrets).
    let samples = p
        .and_then(|v| v.pointer("/PrimaryQueryResult/RelevantResults/Table/Rows"))
        .and_then(|r| r.as_array())
        .map(|rows| {
            rows.iter()
                .take(3)
                .filter_map(|row| {
                    let cells = row.get("Cells")?.as_array()?;
                    let get = |key: &str| {
                        cells.iter().find_map(|c| {
                            (c.get("Key")?.as_str()? == key)
                                .then(|| c.get("Value")?.as_str().map(str::to_string))
                                .flatten()
                        })
                    };
                    Some(serde_json::json!({
                        "title": get("Title"),
                        "path": get("Path"),
                        "originalPath": get("OriginalPath"),
                        "defaultEncodingURL": get("DefaultEncodingURL"),
                    }))
                })
                .collect::<Vec<_>>()
        });
    serde_json::json!({
        "method": method,
        "query": kql,
        "status": status,
        "topLevelKeys": top_keys,
        "rowsAtPrimary": p.and_then(|v| rows_len_at(v, "/PrimaryQueryResult/RelevantResults/Table/Rows")),
        "rowsAtResultsWrapped": p.and_then(|v| rows_len_at(v, "/PrimaryQueryResult/RelevantResults/Table/Rows/results")),
        "rowsAtVerbose": p.and_then(|v| rows_len_at(v, "/d/query/PrimaryQueryResult/RelevantResults/Table/Rows/results")),
        "totalRows": p.and_then(|v| v.pointer("/PrimaryQueryResult/RelevantResults/TotalRows")).cloned(),
        "samples": samples,
        "bodyPrefix": prefix,
    })
}

async fn probe_search_get(
    client: &reqwest::Client,
    root_host: &str,
    cookie_header: &str,
    kql: &str,
    rowlimit: u32,
) -> serde_json::Value {
    let url = build_search_probe_url(root_host, kql, rowlimit);
    match client
        .get(&url)
        .header("Cookie", cookie_header)
        .header("Accept", "application/json;odata=nometadata")
        .send()
        .await
    {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            summarize_search_body("GET", kql, status, &body)
        }
        Err(e) => serde_json::json!({"method": "GET", "query": kql, "error": e.to_string()}),
    }
}

/// Some tenants restrict GET search; POST /_api/search/postquery needs a
/// request digest from /_api/contextinfo first.
async fn probe_search_post(
    client: &reqwest::Client,
    root_host: &str,
    cookie_header: &str,
    kql: &str,
    rowlimit: u32,
) -> serde_json::Value {
    let digest = match client
        .post(format!("https://{root_host}/_api/contextinfo"))
        .header("Cookie", cookie_header)
        .header("Accept", "application/json;odata=nometadata")
        .header("Content-Length", "0")
        .send()
        .await
    {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            let digest = serde_json::from_str::<serde_json::Value>(&body).ok().and_then(|v| {
                v.pointer("/FormDigestValue")
                    .or_else(|| v.pointer("/d/GetContextWebInformation/FormDigestValue"))
                    .and_then(|d| d.as_str().map(String::from))
            });
            match digest {
                Some(d) => d,
                None => {
                    return serde_json::json!({
                        "method": "POST", "query": kql,
                        "error": format!("contextinfo {status} returned no digest"),
                    })
                }
            }
        }
        Err(e) => {
            return serde_json::json!({
                "method": "POST", "query": kql,
                "error": format!("contextinfo failed: {e}"),
            })
        }
    };

    let request_body = serde_json::json!({
        "request": {
            "__metadata": {"type": "Microsoft.Office.Server.Search.REST.SearchRequest"},
            "Querytext": kql,
            "RowLimit": rowlimit,
            "TrimDuplicates": false,
        }
    });
    match client
        .post(format!("https://{root_host}/_api/search/postquery"))
        .header("Cookie", cookie_header)
        .header("Accept", "application/json;odata=nometadata")
        .header("Content-Type", "application/json;odata=verbose")
        .header("X-RequestDigest", digest)
        .body(request_body.to_string())
        .send()
        .await
    {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            summarize_search_body("POST", kql, status, &body)
        }
        Err(e) => serde_json::json!({"method": "POST", "query": kql, "error": e.to_string()}),
    }
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

/// Shared-scope enumeration: Search REST on the root host, tenant-wide.
/// Permission trimming limits results to what the user can read, so this
/// yields their own recordings plus anything shared with them — the same
/// population the video hub renders.
pub(crate) async fn enumerate_via_search(
    client: &reqwest::Client,
    root_host: &str,
    cookies: &[Cookie<'static>],
    since_iso: &str,
) -> Result<Vec<SharePointRecording>> {
    let cookie_header = build_cookie_header(cookies);
    let response = sp_get_json(
        client,
        &build_search_query_url(root_host, since_iso),
        &cookie_header,
    )
    .await
    .context("SharePoint search query failed")?;
    Ok(parse_search_results(&response))
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
    let search = enumerate_via_search(&client, &root_host, root_cookies, &since_iso).await;
    match &search {
        Ok(list) => {
            info!("[sp-sync spike] Search REST OK: {} recording(s)", list.len());
            for r in list.iter().take(10) {
                info!("[sp-sync spike]   {} ({})", r.name, r.created);
            }
        }
        Err(e) => warn!("[sp-sync spike] Search REST failed: {e:#}"),
    }

    // Diagnostics ladder for the search path: is search alive, does a plain
    // mp4 query return rows, does the Stream content type, and does POST
    // behave differently from GET.
    let mut search_probes = Vec::new();
    if root_cookies.is_empty() {
        search_probes.push(serde_json::json!({"error": "no root-host cookies harvested"}));
    } else {
        let header = build_cookie_header(root_cookies);
        // Round 2 ladder: `*` proved search alive but FileExtension:mp4
        // matched nothing tenant-wide (GET and POST alike). Distinguish "the
        // property is dead — use filetype:" from "video is excluded from the
        // classic index — cookies can't reach what the hub uses".
        for (kql, limit) in [
            ("*", 3u32),
            ("filetype:mp4", 10),          // modern property, same intent
            ("filetype:docx", 3),          // control: property queries work at all?
            ("FileExtension:docx", 3),     // control: is FileExtension itself dead?
            ("\"Meeting Recording\"", 10), // free text present in every recording name
            ("AMER AI Community", 10),     // free text for the known shared recording
        ] {
            search_probes.push(probe_search_get(&client, &root_host, &header, kql, limit).await);
        }
        search_probes
            .push(probe_search_post(&client, &root_host, &header, "filetype:mp4", 10).await);
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
        "searchProbes": search_probes,
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

/// Read the sync state (hub URL, watermark, imported ledger) from Rust —
/// used by the automatic pipeline's scheduled scan.
pub fn load_sync_state_public<R: Runtime>(app: &AppHandle<R>) -> SharePointSyncState {
    load_sync_state(app)
}

/// Record a file as imported. The pipeline calls this only after the import
/// actually landed, so a failed download is retried on the next scan.
pub fn mark_imported_public<R: Runtime>(app: &AppHandle<R>, file_url: &str) {
    let mut state = load_sync_state(app);
    state
        .imported
        .insert(file_url.to_string(), chrono::Utc::now().to_rfc3339());
    if let Err(e) = save_sync_state(app, &state) {
        warn!("[sp-scan] could not record import of {file_url}: {e}");
    }
}

/// Advance the "scanned up to" watermark.
pub fn set_last_sync_date<R: Runtime>(app: &AppHandle<R>, iso: &str) {
    let mut state = load_sync_state(app);
    state.last_sync_date = Some(iso.to_string());
    if let Err(e) = save_sync_state(app, &state) {
        warn!("[sp-scan] could not persist last sync date: {e}");
    }
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
    /// "mine" for the user's own OneDrive Recordings folder, "shared" for
    /// recordings surfaced via tenant-wide search (other people's OneDrives,
    /// team sites).
    pub source: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SharePointScanResult {
    /// Which enumeration strategies contributed, e.g. "onedrive+search".
    pub strategy: String,
    pub recordings: Vec<SharePointScanItem>,
}

/// Merge own-folder and search results into one annotated, newest-first list.
/// Split out from the command for testability.
pub(crate) fn merge_scan_results(
    own: Vec<SharePointRecording>,
    shared: Vec<SharePointRecording>,
    imported_urls: &[String],
    meeting_titles: &[String],
) -> Vec<SharePointScanItem> {
    let imported: std::collections::HashSet<String> =
        imported_urls.iter().map(|u| file_url_key(u)).collect();
    let titles: std::collections::HashSet<String> = meeting_titles
        .iter()
        .map(|t| t.trim().to_lowercase())
        .collect();

    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut items: Vec<SharePointScanItem> = Vec::new();
    for (recording, source) in own
        .into_iter()
        .map(|r| (r, "mine"))
        .chain(shared.into_iter().map(|r| (r, "shared")))
    {
        if !seen.insert(file_url_key(&recording.file_url)) {
            continue; // own-folder hit already covers this file
        }
        let already_imported = imported.contains(&file_url_key(&recording.file_url))
            || titles.contains(&title_stem(&recording.name));
        items.push(SharePointScanItem {
            already_imported,
            source: source.to_string(),
            recording,
        });
    }
    items.sort_by(|a, b| b.recording.created.cmp(&a.recording.created));
    items
}

/// Enumerate recordings newer than `since_iso` from both sources — the user's
/// own OneDrive Recordings folder and tenant-wide search (recordings shared
/// with them) — deduped by file URL and annotated with whether each is
/// already in Meetily (imported through this feature, or an existing meeting
/// carries the title the import would create). Either source may fail without
/// failing the scan, as long as the other succeeds.
#[tauri::command]
pub async fn sharepoint_scan_recordings_command<R: Runtime>(
    app: AppHandle<R>,
    hub_url: String,
    since_iso: String,
) -> Result<SharePointScanResult, String> {
    scan_recordings(
        &app,
        &hub_url,
        &since_iso,
        super::sharepoint::AuthMode::AllowInteractive,
    )
    .await
}

/// The scan itself, callable from the background pipeline with
/// [`AuthMode::SilentOnly`] so an expired session never pops a login window.
pub async fn scan_recordings<R: Runtime>(
    app: &AppHandle<R>,
    hub_url: &str,
    since_iso: &str,
    auth_mode: super::sharepoint::AuthMode,
) -> Result<SharePointScanResult, String> {
    let app = app.clone();
    let hub_url = hub_url.to_string();
    let since_iso = since_iso.to_string();
    let url = url::Url::parse(&hub_url).map_err(|e| format!("Invalid hub URL: {e}"))?;
    let root_host = url
        .host_str()
        .ok_or("Hub URL has no host")?
        .to_ascii_lowercase();
    let my_host = derive_my_host(&root_host)
        .ok_or("Could not derive the OneDrive host from the hub URL")?;

    let auth = super::sharepoint::ensure_multi_host_auth_mode(
        &app,
        &hub_url,
        &[my_host.clone()],
        auth_mode,
        |msg| info!("[sp-scan] {msg}"),
    )
    .await
    .map_err(|e| e.to_string())?;

    let client = sp_client().map_err(|e| e.to_string())?;
    let empty: Vec<Cookie<'static>> = Vec::new();

    // Own recordings: OneDrive folder listing on the -my host.
    let my_cookies = auth.host_cookies.get(&my_host).unwrap_or(&empty);
    let own = if my_cookies.is_empty() {
        Err(anyhow!("no cookies harvested for {my_host}"))
    } else {
        enumerate_onedrive_recordings(&client, &my_host, my_cookies, &since_iso).await
    };

    // Shared recordings: tenant-wide search on the root host.
    let root_cookies = auth.host_cookies.get(&root_host).unwrap_or(&empty);
    let shared = if root_cookies.is_empty() {
        Err(anyhow!("no cookies harvested for {root_host}"))
    } else {
        enumerate_via_search(&client, &root_host, root_cookies, &since_iso).await
    };

    let mut strategies: Vec<&str> = Vec::new();
    let own = match own {
        Ok(list) => {
            strategies.push("onedrive");
            list
        }
        Err(e) => {
            warn!("[sp-scan] OneDrive enumeration failed: {e:#}");
            Vec::new()
        }
    };
    let shared = match shared {
        Ok(list) => {
            strategies.push("search");
            list
        }
        Err(e) => {
            warn!("[sp-scan] Search enumeration failed: {e:#}");
            Vec::new()
        }
    };
    if strategies.is_empty() {
        return Err("Both enumeration strategies failed — check the log for details".to_string());
    }
    info!(
        "[sp-scan] {} own + {} shared recording(s) via {} since {since_iso}",
        own.len(),
        shared.len(),
        strategies.join("+")
    );

    // Existing meeting titles, for the "skip the ones I already have" rule.
    // Best-effort: a DB hiccup only disables title-based dedupe.
    let meeting_titles: Vec<String> = match app.try_state::<crate::state::AppState>() {
        Some(state) => sqlx::query_scalar::<_, String>("SELECT title FROM meetings")
            .fetch_all(state.db_manager.pool())
            .await
            .unwrap_or_else(|e| {
                warn!("[sp-scan] Could not read meeting titles for dedupe: {e}");
                Vec::new()
            }),
        None => Vec::new(),
    };
    let imported_urls: Vec<String> = load_sync_state(&app).imported.into_keys().collect();

    Ok(SharePointScanResult {
        strategy: strategies.join("+"),
        recordings: merge_scan_results(own, shared, &imported_urls, &meeting_titles),
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
    fn bare_date_since_is_widened_to_a_full_odata_datetime() {
        // Persisted sync state stores bare dates; OData rejects them.
        let url = build_recordings_query_url("t-my.sharepoint.com", "/personal/u_c_com", "2026-07-27");
        assert!(url.contains("datetime'2026-07-27T00:00:00Z'"));
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
    fn search_query_url_uses_filetype_and_requests_url_properties() {
        let url = build_search_query_url("t.sharepoint.com", "2026-07-01T00:00:00Z");
        assert!(url.starts_with("https://t.sharepoint.com/_api/search/query?querytext='"));
        // filetype: matches videos on tenants where FileExtension: is dead.
        assert!(url.contains("filetype%3Amp4"));
        assert!(!url.contains("FileExtension%3Amp4"));
        assert!(url.contains("LastModifiedTime%3E%3D2026-07-01"));
        // Tenant-wide — no personal-site scoping.
        assert!(!url.contains("personal"));
        // The direct file URL lives in these properties for video rows.
        assert!(url.contains("OriginalPath"));
        assert!(url.contains("DefaultEncodingURL"));
        // Raw spaces/quotes must not survive into the querytext literal.
        let literal = url.split("querytext='").nth(1).unwrap().split('\'').next().unwrap();
        assert!(!literal.contains(' ') && !literal.contains('"'));
    }

    #[test]
    fn search_probe_url_encodes_kql() {
        let url = build_search_probe_url("t.sharepoint.com", "filetype:mp4", 10);
        assert!(url.starts_with(
            "https://t.sharepoint.com/_api/search/query?querytext='filetype%3Amp4'&rowlimit=10"
        ));
        assert!(url.contains(
            "selectproperties='Title,Path,OriginalPath,DefaultEncodingURL,FileExtension,FileType'"
        ));

        let phrase = build_search_probe_url("t.sharepoint.com", "\"Meeting Recording\"", 10);
        assert!(phrase.contains("querytext='%22Meeting%20Recording%22'"));
    }

    #[test]
    fn search_body_summary_counts_rows_at_each_wrapping() {
        // nometadata shape (what we parse today), with sample extraction.
        let plain = r#"{"PrimaryQueryResult":{"RelevantResults":{"TotalRows":7,"Table":{"Rows":[
            {"Cells":[{"Key":"Title","Value":"AMER AI Community"},{"Key":"Path","Value":"https://t.sharepoint.com/sites/ai/x.mp4"}]},
            {}]}}},"ElapsedTime":12}"#;
        let s = summarize_search_body("GET", "*", 200, plain);
        assert_eq!(s["rowsAtPrimary"], serde_json::json!(2));
        assert_eq!(s["rowsAtResultsWrapped"], serde_json::Value::Null);
        assert_eq!(s["totalRows"], serde_json::json!(7));
        assert_eq!(s["samples"][0]["title"], serde_json::json!("AMER AI Community"));

        // minimalmetadata/verbose-style "results" wrapping — the suspected
        // silent-zero culprit.
        let wrapped = r#"{"PrimaryQueryResult":{"RelevantResults":{"Table":{"Rows":{"results":[{},{},{}]}}}}}"#;
        let s = summarize_search_body("GET", "*", 200, wrapped);
        assert_eq!(s["rowsAtPrimary"], serde_json::Value::Null);
        assert_eq!(s["rowsAtResultsWrapped"], serde_json::json!(3));

        // Non-JSON body doesn't panic and keeps a prefix.
        let s = summarize_search_body("GET", "*", 403, "<html>blocked</html>");
        assert_eq!(s["status"], serde_json::json!(403));
        assert_eq!(s["bodyPrefix"], serde_json::json!("<html>blocked</html>"));
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
    fn search_parser_prefers_direct_url_over_dispform_path() {
        // Real tenant shape for video rows: Path is a DispForm list-item
        // link; the file URL lives in DefaultEncodingURL/OriginalPath.
        let json = serde_json::json!({"PrimaryQueryResult":{"RelevantResults":{"Table":{"Rows":[
            {"Cells":[
                {"Key":"Title","Value":"AMER AI Community: Coding in Flow"},
                {"Key":"Path","Value":"https://t-my.sharepoint.com/personal/other_corp_com/Documents/Forms/DispForm.aspx?ID=69201"},
                {"Key":"DefaultEncodingURL","Value":"https://t-my.sharepoint.com/personal/other_corp_com/Documents/Recordings/AMER%20AI%20Community-20260710-Meeting%20Recording.mp4"},
                {"Key":"Created","Value":"2026-07-10T15:00:00Z"},
                {"Key":"Size","Value":"104857600"}
            ]},
            {"Cells":[
                {"Key":"Title","Value":"video without any direct url"},
                {"Key":"Path","Value":"https://t.sharepoint.com/sites/x/Forms/DispForm.aspx?ID=387"}
            ]}
        ]}}}});

        let recs = parse_search_results(&json);
        assert_eq!(recs.len(), 1);
        // Title cell wins over the URL's file name for display/meeting title.
        assert_eq!(recs[0].name, "AMER AI Community: Coding in Flow.mp4");
        assert!(recs[0].file_url.ends_with("Meeting%20Recording.mp4"));
        assert_eq!(recs[0].created, "2026-07-10T15:00:00Z");
        assert_eq!(recs[0].size_bytes, Some(104_857_600));
        assert!(recs[0]
            .stream_url
            .starts_with("https://t-my.sharepoint.com/_layouts/15/stream.aspx?id=/personal/"));
        // The stream URL re-encodes the decoded path exactly once.
        assert!(recs[0].stream_url.ends_with("Meeting%20Recording.mp4"));
        assert!(!recs[0].stream_url.contains("%2520"));
    }

    #[test]
    fn search_parser_uses_title_over_generic_file_names() {
        // Stream/town-hall videos are stored as e.g. "audio.mp4" — the row
        // Title is the only human-usable name.
        let json = serde_json::json!({"PrimaryQueryResult":{"RelevantResults":{"Table":{"Rows":[
            {"Cells":[
                {"Key":"Title","Value":"Company Town Hall Q3"},
                {"Key":"Path","Value":"https://t.sharepoint.com/sites/events/Recordings/audio.mp4"}
            ]},
            {"Cells":[
                {"Key":"Title","Value":"Already Suffixed.mp4"},
                {"Key":"Path","Value":"https://t.sharepoint.com/sites/events/Recordings/audio.mp4"}
            ]},
            {"Cells":[
                {"Key":"Path","Value":"https://t.sharepoint.com/sites/events/Recordings/No Title Row.mp4"}
            ]}
        ]}}}});

        let recs = parse_search_results(&json);
        assert_eq!(recs.len(), 3);
        assert_eq!(recs[0].name, "Company Town Hall Q3.mp4");
        assert_eq!(recs[1].name, "Already Suffixed.mp4");
        assert_eq!(recs[2].name, "No Title Row.mp4");
    }

    #[test]
    fn search_parser_tolerates_empty_results() {
        assert!(parse_search_results(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn site_collection_url_finds_the_owning_site() {
        let site = |raw: &str| site_collection_url(&url::Url::parse(raw).unwrap()).to_string();

        // Someone else's OneDrive — the case that fails when we sign in at the
        // host root instead.
        assert_eq!(
            site("https://t-my.sharepoint.com/personal/jpasquel_murex_com/Documents/Recordings/a.mp4"),
            "https://t-my.sharepoint.com/personal/jpasquel_murex_com/"
        );
        assert_eq!(
            site("https://t.sharepoint.com/sites/Events/Shared%20Documents/a.mp4?x=1"),
            "https://t.sharepoint.com/sites/Events/"
        );
        assert_eq!(
            site("https://t.sharepoint.com/teams/Eng/Recordings/a.mp4"),
            "https://t.sharepoint.com/teams/Eng/"
        );
        // Root-hosted libraries have no managed path to peel off.
        assert_eq!(
            site("https://t.sharepoint.com/Shared%20Documents/a.mp4"),
            "https://t.sharepoint.com/"
        );
    }

    #[test]
    fn download_candidates_ask_for_bytes_before_the_bare_url() {
        // Non-ASCII in the name (Spanish recordings are titled "Grabación de
        // la reunión") must survive into every candidate.
        let file_url = url::Url::parse(
            "https://t-my.sharepoint.com/personal/jpasquel_murex_com/Documents/Recordings/Copilot-Grabaci%C3%B3n.mp4",
        )
        .unwrap();
        let candidates = download_url_candidates(&file_url);
        assert_eq!(candidates.len(), 3);

        assert_eq!(
            candidates[0].as_str(),
            "https://t-my.sharepoint.com/personal/jpasquel_murex_com/Documents/Recordings/Copilot-Grabaci%C3%B3n.mp4?download=1"
        );
        assert_eq!(
            candidates[1].path(),
            "/personal/jpasquel_murex_com/_layouts/15/download.aspx"
        );
        assert_eq!(
            candidates[1]
                .query_pairs()
                .find(|(k, _)| k == "SourceUrl")
                .map(|(_, v)| v.into_owned()),
            Some(file_url.to_string())
        );
        assert_eq!(candidates[2], file_url);
    }

    #[test]
    fn player_page_url_is_site_scoped_and_fully_encoded() {
        // The shape yt-dlp's SharePoint extractor matches, rebuilt from the
        // file URL so no referrer/nav junk from a pasted link rides along.
        let file_url = url::Url::parse(
            "https://t-my.sharepoint.com/personal/jpasquel_murex_com/Documents/Recordings/Copilot-Grabaci%C3%B3n.mp4",
        )
        .unwrap();
        assert_eq!(
            player_page_url(&file_url),
            "https://t-my.sharepoint.com/personal/jpasquel_murex_com/_layouts/15/stream.aspx?\
             id=%2Fpersonal%2Fjpasquel_murex_com%2FDocuments%2FRecordings%2FCopilot-Grabaci%C3%B3n.mp4"
        );
    }

    #[test]
    fn download_candidates_keep_an_existing_query() {
        let file_url =
            url::Url::parse("https://t.sharepoint.com/sites/E/R/a.mp4?csf=1&web=1").unwrap();
        assert_eq!(
            download_url_candidates(&file_url)[0].query(),
            Some("csf=1&web=1&download=1")
        );
    }

    #[test]
    fn redirects_are_classified_by_target() {
        use RedirectVerdict::*;
        // The failure this ladder exists for: a plain GET on an .mp4 bounces
        // into the Stream player, which is not an auth problem.
        assert_eq!(
            classify_redirect("https://t-my.sharepoint.com/personal/u/_layouts/15/stream.aspx?id=%2Fa.mp4"),
            Viewer
        );
        assert_eq!(
            classify_redirect("https://login.microsoftonline.com/common/oauth2/authorize?x=1"),
            Auth
        );
        assert_eq!(
            classify_redirect("https://t.sharepoint.com/_layouts/15/Authenticate.aspx?Source=%2Fa"),
            Auth
        );
        assert_eq!(
            classify_redirect("https://t.sharepoint.com/_layouts/15/AccessDenied.aspx?Source=%2Fa"),
            Denied
        );
        // A short-lived storage URL is an ordinary hop we should follow.
        assert_eq!(
            classify_redirect("https://media.svc.ms/transform/videomanifest?token=abc"),
            Follow
        );
    }

    #[test]
    fn redirect_summary_drops_the_query() {
        assert_eq!(
            redirect_summary(&url::Url::parse("https://media.svc.ms/v/x?token=secret").unwrap()),
            "media.svc.ms/v/x"
        );
    }

    #[test]
    fn file_url_keys_collide_across_encodings_and_case() {
        assert_eq!(
            file_url_key("https://T-my.sharepoint.com/personal/u/Documents/Recordings/Team%20Sync.mp4"),
            file_url_key("https://t-my.sharepoint.com/personal/u/Documents/Recordings/Team Sync.mp4"),
        );
        assert_ne!(
            file_url_key("https://h/a.mp4"),
            file_url_key("https://h/b.mp4")
        );
    }

    #[test]
    fn title_stem_matches_the_frontend_rule() {
        // frontend: fileName.replace(/\.[^.]+$/, '').trim() || fileName — lowercased here.
        assert_eq!(title_stem("Weekly Sync-20260721-Meeting Recording.mp4"),
                   "weekly sync-20260721-meeting recording");
        assert_eq!(title_stem("archive.tar.gz"), "archive.tar");
        assert_eq!(title_stem(".hidden"), ".hidden");
        assert_eq!(title_stem("NoExtension"), "noextension");
    }

    fn rec(name: &str, file_url: &str, created: &str) -> SharePointRecording {
        SharePointRecording {
            name: name.to_string(),
            file_url: file_url.to_string(),
            stream_url: format!("https://h/_layouts/15/stream.aspx?id={name}"),
            created: created.to_string(),
            size_bytes: Some(1),
        }
    }

    #[test]
    fn merge_dedupes_by_url_tags_sources_and_flags_existing() {
        let own = vec![
            rec("Mine.mp4", "https://h/personal/me/Documents/Recordings/Mine.mp4", "2026-07-20T10:00:00Z"),
            rec("Old Sync.mp4", "https://h/personal/me/Documents/Recordings/Old Sync.mp4", "2026-07-01T10:00:00Z"),
        ];
        let shared = vec![
            // Same file as own[0], but percent-encoded the way search returns it.
            rec("Mine.mp4", "https://h/personal/me/Documents/Recordings/Mine.mp4", "2026-07-20T10:00:00Z"),
            rec("Theirs.mp4", "https://h/personal/other/Documents/Recordings/Theirs.mp4", "2026-07-22T10:00:00Z"),
            rec("Already Meeting.mp4", "https://h/personal/other/Documents/Recordings/Already%20Meeting.mp4", "2026-07-21T10:00:00Z"),
        ];
        let imported = vec![
            "https://h/personal/me/Documents/Recordings/Old%20Sync.mp4".to_string(),
        ];
        let titles = vec!["already meeting".to_string()];

        let items = merge_scan_results(own, shared, &imported, &titles);
        assert_eq!(items.len(), 4);
        // Newest first.
        assert_eq!(items[0].recording.name, "Theirs.mp4");
        assert_eq!(items[0].source, "shared");
        assert!(!items[0].already_imported);
        // Title match against an existing meeting flags it.
        assert_eq!(items[1].recording.name, "Already Meeting.mp4");
        assert!(items[1].already_imported);
        // Duplicate collapsed to the own-folder entry.
        assert_eq!(items[2].recording.name, "Mine.mp4");
        assert_eq!(items[2].source, "mine");
        // Imported map matches across percent-encoding.
        assert_eq!(items[3].recording.name, "Old Sync.mp4");
        assert!(items[3].already_imported);
    }
}
