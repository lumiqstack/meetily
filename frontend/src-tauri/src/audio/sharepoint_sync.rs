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
use tauri::webview::Cookie;
use tauri::{AppHandle, Runtime};

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

/// Search REST URL on the root host: mp4 files under the user's personal
/// site, newest first. `LastModifiedTime>=` uses the search date format.
pub(crate) fn build_search_query_url(root_host: &str, my_host: &str, since_iso: &str) -> String {
    let date = since_iso.split('T').next().unwrap_or(since_iso);
    let query = format!(
        "(FileExtension:mp4 OR FileExtension:webm) AND Path:https://{my_host}/personal/* AND LastModifiedTime>={date}"
    );
    let encoded: String = query
        .chars()
        .map(|c| match c {
            ' ' => "%20".to_string(),
            '\'' => "''".to_string(),
            ':' => "%3A".to_string(),
            '/' => "%2F".to_string(),
            _ => c.to_string(),
        })
        .collect();
    format!(
        "https://{root_host}/_api/search/query?querytext='{encoded}'&rowlimit=200&\
         selectproperties='Title,Path,LastModifiedTime,Size'&\
         sortlist='LastModifiedTime:descending'"
    )
}

/// The stream.aspx player page for a OneDrive file — the URL shape the
/// existing yt-dlp import path understands.
pub(crate) fn stream_url_for(my_host: &str, server_relative_path: &str) -> String {
    let encoded: String = server_relative_path
        .chars()
        .map(|c| match c {
            ' ' => "%20".to_string(),
            '\'' => "%27".to_string(),
            '#' => "%23".to_string(),
            '&' => "%26".to_string(),
            '+' => "%2B".to_string(),
            _ => c.to_string(),
        })
        .collect();
    format!("https://{my_host}/_layouts/15/stream.aspx?id={encoded}")
}

/// Parse the OneDrive folder listing (odata=nometadata shape).
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

/// `Cookie:` header value from harvested webview cookies.
pub(crate) fn build_cookie_header(cookies: &[Cookie<'static>]) -> String {
    cookies
        .iter()
        .map(|c| format!("{}={}", c.name(), c.value()))
        .collect::<Vec<_>>()
        .join("; ")
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

/// Fallback enumeration: Search REST on the root host.
pub(crate) async fn enumerate_via_search(
    client: &reqwest::Client,
    root_host: &str,
    my_host: &str,
    cookies: &[Cookie<'static>],
    since_iso: &str,
) -> Result<Vec<SharePointRecording>> {
    let cookie_header = build_cookie_header(cookies);
    let results = sp_get_json(
        client,
        &build_search_query_url(root_host, my_host, since_iso),
        &cookie_header,
    )
    .await
    .context("SharePoint search query failed")?;
    Ok(parse_search_results(&results))
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

    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| e.to_string())?;
    let empty: Vec<Cookie<'static>> = Vec::new();

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
    }))
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
    fn onedrive_parser_tolerates_missing_or_odd_shapes() {
        assert!(parse_onedrive_files(&serde_json::json!({}), "h").is_empty());
        assert!(parse_onedrive_files(&serde_json::json!({"value": "nope"}), "h").is_empty());
        let partial = serde_json::json!({"value": [{"Name": "x.mp4"}]});
        assert!(parse_onedrive_files(&partial, "h").is_empty());
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
