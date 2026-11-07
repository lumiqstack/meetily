//! List the meetings the Teams Recap app ("Meeting Recaps" on the Teams app
//! bar) shows on its Home tab, so the recap sync knows which recaps exist
//! without the user pasting links.
//!
//! The list is read from the same persisted WebView2 profile the recap import
//! uses, by a page script (`teams_recap_list.js`) that reads each row's
//! meeting from the props React gives the row and builds the recap sharing
//! link from it. The script returns the rows it has seen so far on every poll
//! and `done` once it has scrolled past the scan window. A list that stops
//! growing before that gets one page reload (`reloading`) and is read again:
//! the first listing after Meetily starts can show only the newest rows. This
//! module keeps the last snapshot, dedupes rows by meeting identity, counts
//! rows without a recap link as "not ready", and keeps why the script stopped
//! and the oldest slot it reached, so a list that stopped short can say so. A
//! probe mode adds the listing's counts and timings; the debug command writes
//! it to a local file, never to a log.
//!
//! If Teams changes its layout, `MEETILY_RECAP_LIST_SCRIPT` names a file whose
//! contents replace the bundled script, so the script can be tuned on the
//! Windows machine without a rebuild.

// Only the Windows driver calls the script helpers; the rest of the module
// stays compiled everywhere so its tests run on every platform.
#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use super::teams_recap::meeting_identity_key;
use super::teams_recap_sync::RecapCandidate;

pub const SCRIPT_OVERRIDE_ENV: &str = "MEETILY_RECAP_LIST_SCRIPT";
/// The Teams web shell, on the domain Teams now serves it from: starting on
/// teams.microsoft.com costs about 40 seconds on the redirect page. The script
/// steers from here to the Recap app by its app id or name, and the user can
/// do the same by hand in the interactive phase.
pub const TEAMS_WEB_URL: &str = "https://teams.cloud.microsoft/v2/";
/// More rows than any 30-day window of meetings; stops a runaway scroll.
pub const MAX_ROWS: usize = 500;
/// The title of a listed row that showed none.
pub const UNTITLED_ROW_TITLE: &str = "Teams meeting";
const LIST_SCRIPT: &str = include_str!("teams_recap_list.js");

/// Why the page script stopped reading the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ListEnd {
    /// It reached a row from before the since date, so the window is complete.
    Since,
    /// The Recap app showed no rows at all.
    Empty,
    /// The list stopped growing before it reached the since date: either the
    /// whole list is newer than the window, or older rows never loaded.
    Idle,
    /// It reached [`MAX_ROWS`].
    Cap,
}

impl ListEnd {
    /// The script's `stop` value. An unknown one, from an older override
    /// script, reads as no answer instead of failing the listing.
    fn from_script(stop: &str) -> Option<Self> {
        match stop {
            "since" => Some(Self::Since),
            "empty" => Some(Self::Empty),
            "idle" => Some(Self::Idle),
            "cap" => Some(Self::Cap),
            _ => None,
        }
    }

    /// Whether rows inside the window may be missing: the script stopped
    /// before it reached the since date while the list still had rows.
    pub fn may_be_short(self) -> bool {
        matches!(self, Self::Idle | Self::Cap)
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Since => "since",
            Self::Empty => "empty",
            Self::Idle => "idle",
            Self::Cap => "cap",
        }
    }
}

/// What one listing produced. `error` is set when the page never showed a
/// recognisable list; the probe (when requested) is still kept so the
/// failure can be diagnosed.
#[derive(Debug, Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecapListing {
    pub candidates: Vec<RecapCandidate>,
    /// Rows in the window that showed no recap link: counted as not ready.
    pub rows_without_link: usize,
    pub page_url: Option<String>,
    pub error: Option<String>,
    /// Why the script stopped, when it finished and said.
    pub end: Option<ListEnd>,
    /// The oldest calendar slot the list reached, RFC 3339.
    pub oldest_row: Option<String>,
    pub probe: Option<serde_json::Value>,
}

/// One row as the page script reports it.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ListedRow {
    #[serde(default)]
    pub recap_url: Option<String>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub started_at: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ListProbe {
    pub status: String,
    #[serde(default)]
    pub items: Vec<ListedRow>,
    #[serde(default)]
    pub href: Option<String>,
    /// On `done`: why the script stopped and the oldest slot it reached.
    #[serde(default)]
    pub stop: Option<String>,
    #[serde(default)]
    pub oldest: Option<String>,
    #[serde(default)]
    pub probe: Option<serde_json::Value>,
}

/// Fold one script result into the listing: the rows on `page` and `done`,
/// and on `done` why the list ended and how far back it got.
pub(crate) fn apply(listing: &mut RecapListing, probe: ListProbe) {
    if let Some(href) = probe.href {
        listing.page_url = Some(href);
    }
    if probe.probe.is_some() {
        listing.probe = probe.probe;
    }
    if probe.status == "page" || probe.status == "done" {
        let (candidates, without_link) = collect(&probe.items);
        listing.candidates = candidates;
        listing.rows_without_link = without_link;
    }
    if probe.status == "done" {
        listing.end = probe.stop.as_deref().and_then(ListEnd::from_script);
        listing.oldest_row = probe.oldest.filter(|at| !at.trim().is_empty());
    }
}

/// The script with its two placeholders filled. `since` is generated, never
/// user text, so the substitution cannot inject script.
pub(crate) fn render_script(template: &str, since: DateTime<Utc>, probe: bool) -> String {
    template
        .replace("%SINCE%", &since.to_rfc3339_opts(SecondsFormat::Secs, true))
        .replace("%PROBE%", if probe { "true" } else { "false" })
}

/// The bundled script, or the override file named by
/// [`SCRIPT_OVERRIDE_ENV`] when that variable is set.
pub(crate) fn script_template() -> Result<String> {
    match std::env::var_os(SCRIPT_OVERRIDE_ENV) {
        Some(path) if !path.is_empty() => script_template_from(std::path::Path::new(&path)),
        _ => Ok(LIST_SCRIPT.to_owned()),
    }
}

fn script_template_from(path: &std::path::Path) -> Result<String> {
    let script = std::fs::read_to_string(path)
        .with_context(|| format!("{SCRIPT_OVERRIDE_ENV} names an unreadable file"))?;
    for placeholder in ["%SINCE%", "%PROBE%"] {
        if !script.contains(placeholder) {
            return Err(anyhow!("{SCRIPT_OVERRIDE_ENV} file has no {placeholder} placeholder"));
        }
    }
    log::warn!("[recap-list] using the recap list script override from {SCRIPT_OVERRIDE_ENV}");
    Ok(script)
}

/// Turn the script's row snapshot into candidates: one per meeting identity,
/// first row wins, rows without a link counted separately, capped at
/// [`MAX_ROWS`] rows in all.
pub(crate) fn collect(rows: &[ListedRow]) -> (Vec<RecapCandidate>, usize) {
    let mut seen = HashSet::new();
    let mut candidates = Vec::new();
    let mut without_link = 0;
    for row in rows.iter().take(MAX_ROWS) {
        let Some(url) = row.recap_url.as_deref().map(str::trim).filter(|url| !url.is_empty()) else {
            without_link += 1;
            continue;
        };
        if !seen.insert(meeting_identity_key(url)) {
            continue;
        }
        let title = row.title.trim();
        candidates.push(RecapCandidate {
            recap_url: url.to_owned(),
            title: if title.is_empty() { UNTITLED_ROW_TITLE.to_owned() } else { title.to_owned() },
            started_at: row.started_at.clone().filter(|value| !value.trim().is_empty()),
        });
    }
    (candidates, without_link)
}

#[cfg(target_os = "windows")]
mod windows {
    use super::*;
    use std::time::Duration;
    use tauri::{AppHandle, Manager, Runtime, WebviewWindow};
    use url::Url;

    use crate::audio::teams_recap::windows::{
        block_desktop_handoff, execute_script, INTERACTIVE_WAIT, POLL_INTERVAL, SILENT_WAIT,
    };

    /// Poll until the script says `done`. `Ok(true)` when it did, `Ok(false)`
    /// at the deadline; the listing holds whatever was seen either way.
    async fn poll<R: Runtime>(
        window: &WebviewWindow<R>,
        script: &str,
        deadline: tokio::time::Instant,
        listing: &mut RecapListing,
    ) -> Result<bool> {
        let mut launcher_since: Option<tokio::time::Instant> = None;
        let mut previous_status = String::new();
        while tokio::time::Instant::now() < deadline {
            if window.url().is_err() {
                return Err(anyhow!(
                    "The Teams window was closed before the recap list was read."
                ));
            }
            if let Ok(raw) = execute_script(window, script).await {
                match serde_json::from_str::<ListProbe>(&raw) {
                    Ok(probe) => {
                        // Fixed state names only, never page text or URLs.
                        let status = match probe.status.as_str() {
                            "done" => "done",
                            "page" => "page",
                            "navigating" => "navigating",
                            "launcher" => "launcher",
                            "opening_web" => "opening_web",
                            "reloading" => "reloading",
                            _ => "waiting",
                        };
                        if status != previous_status {
                            log::info!("[recap-list] state: {status}");
                            previous_status = status.to_owned();
                        }
                        let done = status == "done";
                        apply(listing, probe);
                        if done {
                            log::info!(
                                "[recap-list] listed {} recaps, {} rows without a link, stopped: {}, oldest row: {}",
                                listing.candidates.len(),
                                listing.rows_without_link,
                                listing.end.map_or("not said", ListEnd::as_str),
                                listing.oldest_row.as_deref().unwrap_or("none")
                            );
                            return Ok(true);
                        }
                        if status == "launcher" || status == "opening_web" {
                            let since = launcher_since.get_or_insert_with(tokio::time::Instant::now);
                            if since.elapsed() > Duration::from_secs(45) {
                                return Err(anyhow!(
                                    "Teams stayed on its app-choice page. Select the web/browser option in Meetily's sign-in window, not the Teams desktop app, then retry."
                                ));
                            }
                        } else {
                            launcher_since = None;
                        }
                    }
                    Err(e) => log::warn!("[recap-list] script result was not valid JSON: {e}"),
                }
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        Ok(false)
    }

    pub async fn list<R: Runtime>(
        app: &AppHandle<R>,
        since: DateTime<Utc>,
        probe: bool,
    ) -> Result<RecapListing> {
        let script = render_script(&script_template()?, since, probe);
        let url = Url::parse(TEAMS_WEB_URL).expect("constant URL");
        // Same lock and profile as the SharePoint and recap flows, so two
        // hidden Teams windows never fight over the one profile.
        let _auth_flow = crate::audio::sharepoint::AUTH_FLOW_LOCK.lock().await;
        let data_dir = app
            .path()
            .app_data_dir()
            .map_err(|e| anyhow!("Could not resolve app data directory: {e}"))?
            .join("sp-webview");
        std::fs::create_dir_all(&data_dir)?;
        let window =
            crate::audio::sharepoint::create_auth_window_deferred(app, &url, &data_dir).await?;

        let mut listing = RecapListing::default();
        let result: Result<()> = async {
            block_desktop_handoff(&window).await?;
            window.navigate(url.clone())?;
            let started = tokio::time::Instant::now();
            if poll(&window, &script, started + SILENT_WAIT, &mut listing).await? {
                return Ok(());
            }
            log::info!("[recap-list] showing the window: the list was not read while it was hidden");
            window.set_title("Teams recaps — sign in, then open Meeting Recaps")?;
            window.show()?;
            window.set_focus()?;
            if poll(&window, &script, tokio::time::Instant::now() + INTERACTIVE_WAIT, &mut listing).await? {
                return Ok(());
            }
            Err(anyhow!(
                "Teams did not show the Meeting Recaps list. Sign in if prompted, open Meeting Recaps from the Teams app bar in the Meetily window, then retry."
            ))
        }
        .await;
        let _ = window.close();
        if let Err(e) = result {
            listing.error = Some(e.to_string());
        }
        Ok(listing)
    }
}

/// Read the Recap app's list for meetings since `since`. On Windows the
/// listing's `error` reports a page that never showed the list; elsewhere
/// this is an error outright.
pub async fn list_recaps<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    since: DateTime<Utc>,
    probe: bool,
) -> Result<RecapListing> {
    #[cfg(target_os = "windows")]
    {
        windows::list(app, since, probe).await
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (app, since, probe);
        Err(anyhow!(
            "Teams recap sync requires Meetily on Windows with WebView2."
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(url: Option<&str>, title: &str, at: Option<&str>) -> ListedRow {
        ListedRow {
            recap_url: url.map(str::to_owned),
            title: title.to_owned(),
            started_at: at.map(str::to_owned),
        }
    }

    #[test]
    fn bundled_script_carries_both_placeholders() {
        assert!(LIST_SCRIPT.contains("%SINCE%"));
        assert!(LIST_SCRIPT.contains("%PROBE%"));
        let rendered = render_script(
            LIST_SCRIPT,
            DateTime::parse_from_rfc3339("2026-10-03T10:00:00+00:00").unwrap().with_timezone(&Utc),
            true,
        );
        assert!(rendered.contains("new Date('2026-10-03T10:00:00Z')"));
        assert!(rendered.contains("const PROBE = true;"));
        for placeholder in ["%SINCE%", "%PROBE%"] {
            assert!(!rendered.contains(placeholder), "{placeholder} survived rendering");
        }
    }

    #[test]
    fn rows_dedupe_by_meeting_identity_and_count_missing_links() {
        let link = "https://teams.microsoft.com/l/meetingrecap?threadId=19%3Aa&fileUrl=https%3A%2F%2Fcontoso-my.sharepoint.com%2Fpersonal%2Fjdoe%2FDocuments%2FRecordings%2FSync-20261027_100000-Meeting%20Recording.mp4";
        let same_file = "https://teams.microsoft.com/l/meetingrecap?threadId=19%3Ab&fileUrl=https%3A%2F%2Fcontoso-my.sharepoint.com%2Fpersonal%2Fjdoe%2FDocuments%2FRecordings%2FSync-20261027_100000-Meeting%20Recording.mp4%3Ftempauth%3Dx";
        let rows = vec![
            row(Some(link), " Weekly sync ", Some("2026-10-27T10:00:00.000Z")),
            row(Some(same_file), "Weekly sync (dup)", Some("2026-10-27T10:00:00.000Z")),
            row(None, "No recap yet", Some("2026-10-28T09:00:00.000Z")),
            row(Some("  "), "Blank link", None),
            row(Some("https://teams.microsoft.com/l/meetingrecap?threadId=19%3Ac"), "", Some("")),
        ];
        let (candidates, without_link) = collect(&rows);
        assert_eq!(without_link, 2);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].title, "Weekly sync");
        assert_eq!(candidates[0].recap_url, link);
        assert_eq!(candidates[1].title, "Teams meeting");
        assert_eq!(candidates[1].started_at, None);
    }

    #[test]
    fn a_meetings_transcript_row_and_recording_row_become_one_candidate() {
        // The Recap app can list a meeting twice, once for its transcript and
        // once for its recording. Both rows name the same file, so the links
        // the script builds differ only in the fields each row knows.
        let file = "https%3A%2F%2Fcontoso-my.sharepoint.com%2Fpersonal%2Fjdoe%2FDocuments%2FRecordings%2FSync-20261105_133311-Meeting%2520Recording.mp4%3Fweb%3D1";
        let transcript_row = format!(
            "https://teams.cloud.microsoft/l/meetingrecap?fileUrl={file}&callId=call-a&recapType=ODSPTranscript"
        );
        let recording_row = format!(
            "https://teams.cloud.microsoft/l/meetingrecap?fileUrl={file}&callId=call-b&tenantId=t-1&recapType=RecordingAndTranscript"
        );
        let rows = vec![
            row(Some(&transcript_row), "Sync", Some("2026-11-05T19:33:55.000Z")),
            row(Some(&recording_row), "Sync", Some("2026-11-05T19:34:23.170Z")),
        ];
        let (candidates, without_link) = collect(&rows);
        assert_eq!((candidates.len(), without_link), (1, 0));
        assert_eq!(candidates[0].recap_url, transcript_row);
        assert_eq!(candidates[0].started_at.as_deref(), Some("2026-11-05T19:33:55.000Z"));
    }

    #[test]
    fn listing_stops_at_the_row_cap() {
        let rows: Vec<ListedRow> = (0..MAX_ROWS + 50)
            .map(|i| row(Some(&format!("https://teams.microsoft.com/l/meetingrecap?threadId={i}")), "m", None))
            .collect();
        assert_eq!(collect(&rows).0.len(), MAX_ROWS);
    }

    #[test]
    fn script_results_parse_with_optional_fields() {
        let done: ListProbe = serde_json::from_str(
            r#"{"status":"done","items":[{"recapUrl":"https://teams.microsoft.com/l/meetingrecap?threadId=1","linkKind":"href","title":"Standup","startedAt":"2026-10-30T09:00:00.000Z"},{"recapUrl":null,"title":"Later"}],"href":"https://teams.microsoft.com/v2/"}"#,
        )
        .unwrap();
        assert_eq!(done.status, "done");
        assert_eq!(done.items.len(), 2);
        assert_eq!(done.items[1].recap_url, None);
        assert!(done.probe.is_none());
        let waiting: ListProbe = serde_json::from_str(r#"{"status":"waiting"}"#).unwrap();
        assert!(waiting.items.is_empty() && waiting.href.is_none());
    }

    fn script_result(json: &str) -> ListProbe {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn a_finished_listing_says_why_it_stopped_and_how_far_back_it_got() {
        let item = r#"{"recapUrl":"https://teams.microsoft.com/l/meetingrecap?threadId=1","title":"Standup","startedAt":"2026-11-05T15:00:00.000Z"}"#;
        let mut listing = RecapListing::default();
        // A page snapshot carries rows but says nothing about the end yet.
        apply(&mut listing, script_result(&format!(r#"{{"status":"page","items":[{item}],"stop":"idle"}}"#)));
        assert_eq!((listing.candidates.len(), listing.end, listing.oldest_row.as_deref()), (1, None, None));

        apply(
            &mut listing,
            script_result(&format!(r#"{{"status":"done","items":[{item}],"stop":"idle","oldest":"2026-11-05T15:00:00.000Z"}}"#)),
        );
        assert_eq!(listing.end, Some(ListEnd::Idle));
        assert!(listing.end.unwrap().may_be_short());
        assert_eq!(listing.oldest_row.as_deref(), Some("2026-11-05T15:00:00.000Z"));

        let mut reached = RecapListing::default();
        apply(&mut reached, script_result(r#"{"status":"done","items":[],"stop":"since","oldest":"2026-10-28T09:00:00.000Z"}"#));
        assert!(!reached.end.unwrap().may_be_short());
        assert!(ListEnd::Cap.may_be_short() && !ListEnd::Empty.may_be_short());

        // An override script that predates `stop`, or says something new.
        let mut older = RecapListing::default();
        apply(&mut older, script_result(r#"{"status":"done","items":[]}"#));
        assert_eq!((older.end, older.oldest_row.as_deref()), (None, None));
        apply(&mut older, script_result(r#"{"status":"done","items":[],"stop":"later","oldest":" "}"#));
        assert_eq!((older.end, older.oldest_row.as_deref()), (None, None));
    }

    #[test]
    fn override_file_must_exist_and_hold_the_placeholder() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("none.js");
        assert!(script_template_from(&missing).is_err());
        let bad = dir.path().join("bad.js");
        std::fs::write(&bad, "(() => ({status:'done'}))()").unwrap();
        assert!(script_template_from(&bad).unwrap_err().to_string().contains("%SINCE%"));
        let no_probe = dir.path().join("no_probe.js");
        std::fs::write(&no_probe, "(() => ({status:'done', since:'%SINCE%'}))()").unwrap();
        assert!(script_template_from(&no_probe).unwrap_err().to_string().contains("%PROBE%"));
        let good = dir.path().join("good.js");
        std::fs::write(&good, "(() => ({status:'done', since:'%SINCE%', probe:%PROBE%}))()").unwrap();
        assert!(script_template_from(&good).unwrap().contains("%PROBE%"));
    }
}
