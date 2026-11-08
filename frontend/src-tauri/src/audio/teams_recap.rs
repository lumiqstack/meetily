//! Read a Teams AI recap from the same persisted WebView2 profile used for
//! SharePoint sign-in. The script only reads the rendered recap DOM; it does
//! not expose Tauri IPC to the remote page or copy meeting text to a URL,
//! clipboard, log, or external service.

use anyhow::{anyhow, Result};
use url::Url;

#[path = "teams_recap_metadata.rs"]
pub(crate) mod metadata;

pub(crate) fn recording_metadata(url: &Url) -> Option<metadata::RecordingMetadata> {
    let (_, file_url) = url.query_pairs().find(|(key, _)| key == "fileUrl")?;
    let file_url = Url::parse(&file_url).ok()?;
    metadata::from_filename(file_url.path().rsplit('/').next()?)
}

/// The meeting a link stands for, as a key for matching a recap against a
/// meeting Meetily already holds. A recap link's `fileUrl` names the recording
/// or transcript file, which is what recording imports store as
/// `recording_url`, so both sides reduce to that file: host plus decoded,
/// lowercased path, with the query dropped (it carries only tokens). A
/// `stream.aspx?id=/path/file.mp4` player link reduces to the same file. A
/// recap link without `fileUrl` keys on its own host and path plus its query,
/// so repeated imports of that link still collide with each other.
pub fn meeting_identity_key(link: &str) -> String {
    let link = link.trim();
    let Ok(url) = Url::parse(link) else {
        return link.to_lowercase();
    };
    if is_recap_url(&url) {
        return match recap_file_url(&url) {
            Some(file) => file_identity(&file),
            None => format!(
                "{}{}?{}",
                url.host_str().unwrap_or("").to_ascii_lowercase(),
                url.path(),
                url.query().unwrap_or("")
            ),
        };
    }
    file_identity(&url)
}

fn is_recap_url(url: &Url) -> bool {
    url.path() == "/l/meetingrecap"
}

fn recap_file_url(url: &Url) -> Option<Url> {
    let (_, file_url) = url.query_pairs().find(|(key, _)| key == "fileUrl")?;
    Url::parse(file_url.trim()).ok()
}

/// What a stored `recording_url` or an incoming link says about the recording
/// behind it. A live meeting's row holds one link, and which recaps and files
/// may still attach depends on this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    /// A recording or transcript file, directly or through its player page.
    File,
    /// A recap link whose `fileUrl` names the file.
    RecapOfFile,
    /// A recap link that names no file, such as one built from a thread id.
    RecapWithoutFile,
}

pub fn link_kind(link: &str) -> LinkKind {
    match Url::parse(link.trim()) {
        Ok(url) if is_recap_url(&url) => {
            if recap_file_url(&url).is_some() {
                LinkKind::RecapOfFile
            } else {
                LinkKind::RecapWithoutFile
            }
        }
        _ => LinkKind::File,
    }
}

/// [`meeting_identity_key`] with the part of a long recording removed. Teams
/// cuts a recording at 4 hours into `…-Meeting Recording.mp4` and
/// `…-Meeting Recording 2.mp4`, and a recap may name any part, so parts share
/// this key. Every suffix in [`metadata::SUFFIXES`] counts, whatever the
/// language. Any other key comes back unchanged.
pub fn whole_recording_key(identity_key: &str) -> &str {
    metadata::SUFFIXES
        .into_iter()
        .find_map(|suffix| {
            let (stem, tail) = identity_key.rsplit_once(suffix)?;
            let part = tail.split('.').next()?.trim();
            part.bytes().all(|byte| byte.is_ascii_digit()).then_some(stem)
        })
        .unwrap_or(identity_key)
}

fn file_identity(url: &Url) -> String {
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    let raw_path = url.path();
    let path = if raw_path.to_ascii_lowercase().ends_with("/stream.aspx") {
        url.query_pairs()
            .find(|(key, _)| key == "id")
            .map(|(_, value)| value.into_owned())
            .unwrap_or_else(|| crate::audio::sharepoint_sync::percent_decode_component(raw_path))
    } else {
        crate::audio::sharepoint_sync::percent_decode_component(raw_path)
    };
    format!("{host}{}", path.to_lowercase())
}

#[cfg(any(target_os = "windows", test))]
#[path = "teams_recap_navigation.rs"]
mod navigation;

#[derive(Debug, Clone)]
pub struct TeamsRecap {
    pub markdown: String,
}

const WINDOWS_ONLY: &str =
    "Automatic Teams recap extraction requires Meetily on Windows with WebView2.";

/// Why reading one recap of a sync came back without it.
#[derive(Debug)]
pub enum MissedRecap {
    /// Teams opened the recap but showed no AI summary in time. It may still
    /// be writing one, so the next sync tries again.
    NoSummary,
    /// Teams did not open the recap in time.
    NotOpened { waited: std::time::Duration },
    /// The window or the page failed.
    Failed(anyhow::Error),
}

impl std::fmt::Display for MissedRecap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSummary => f.write_str("Teams opened this recap but showed no AI summary."),
            Self::NotOpened { waited } => write!(
                f,
                "Teams did not open this recap within {} seconds.",
                waited.as_secs()
            ),
            Self::Failed(error) => write!(f, "{error}"),
        }
    }
}

/// How long each recap of a sync may take, and how a read that ran out of
/// time ends. Kept apart from the window so every platform tests it.
#[cfg(any(target_os = "windows", test))]
mod reading {
    use super::MissedRecap;
    use std::time::Duration;

    /// The first recap of a sync also waits for Teams to start in its window.
    /// On 2026-11-06 a start took up to 64 s, and a recap opened 12 s after
    /// its link in a window where Teams had started.
    pub(super) const START_WAIT: Duration = Duration::from_secs(180);
    /// Every later recap opens in a window where Teams has started.
    pub(super) const UNATTENDED_WAIT: Duration = Duration::from_secs(60);

    pub(super) fn wait_for(earlier_reads: usize) -> Duration {
        if earlier_reads == 0 {
            START_WAIT
        } else {
            UNATTENDED_WAIT
        }
    }

    /// Where the page was while the script said `waiting`, as it reports it:
    /// host and path with ids masked, never the query or the page's text.
    #[derive(Debug, Default, Clone, PartialEq, serde::Deserialize)]
    pub(super) struct Whereabouts {
        #[serde(default)]
        page: Option<String>,
        #[serde(default)]
        visibility: Option<String>,
        #[serde(default)]
        focused: Option<bool>,
        #[serde(default)]
        frames: Option<u64>,
    }

    impl Whereabouts {
        /// The page and how Teams sees the window, for the log, or `None` when
        /// the script said nothing about it.
        pub(super) fn place(&self) -> Option<String> {
            let page: String = self.page.as_deref()?.chars().take(100).collect();
            let visibility = match self.visibility.as_deref() {
                Some("visible") => "visible",
                Some("hidden") => "hidden",
                _ => "visibility unknown",
            };
            let focus = match self.focused {
                Some(true) => "focused",
                Some(false) => "not focused",
                None => "focus unknown",
            };
            Some(format!("{page}, {visibility}, {focus}"))
        }

        /// Frames the page has drawn since it loaded.
        pub(super) fn frames(&self) -> Option<u64> {
            self.frames
        }
    }

    /// What one read has seen of its recap, from the page script's states.
    #[derive(Debug, Default)]
    pub(super) struct Sighting {
        opened_after: Option<Duration>,
        last_wait: Option<String>,
    }

    impl Sighting {
        pub(super) fn observe(&mut self, status: &str, elapsed: Duration) {
            if self.opened_after.is_none()
                && matches!(status, "recap_open" | "collecting_recap" | "waiting_tasks" | "ready")
            {
                self.opened_after = Some(elapsed);
            }
        }

        /// Note where the page waited, for the read's last log line.
        pub(super) fn waited_on(&mut self, whereabouts: &Whereabouts) {
            if let Some(place) = whereabouts.place() {
                self.last_wait = Some(match whereabouts.frames() {
                    Some(frames) => format!("{place}, {frames} frames"),
                    None => place,
                });
            }
        }

        /// Where the page last waited, with its frame count then.
        pub(super) fn last_wait(&self) -> Option<&str> {
            self.last_wait.as_deref()
        }

        /// How long after its link the recap first showed, if it did.
        pub(super) fn opened_after(&self) -> Option<Duration> {
            self.opened_after
        }

        /// How a read that ran out of time after `waited` ends.
        pub(super) fn missed(&self, waited: Duration) -> MissedRecap {
            match self.opened_after {
                Some(_) => MissedRecap::NoSummary,
                None => MissedRecap::NotOpened { waited },
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn secs(value: u64) -> Duration {
            Duration::from_secs(value)
        }

        #[test]
        fn the_first_recap_also_waits_for_teams_to_start_and_later_ones_get_a_minute() {
            assert_eq!(wait_for(0), secs(180));
            assert_eq!(wait_for(1), secs(60));
            assert_eq!(wait_for(29), secs(60));
        }

        #[test]
        fn a_read_that_ran_out_of_time_is_not_ready_only_if_the_recap_opened() {
            let mut sighting = Sighting::default();
            for status in ["waiting", "opening_web", "launcher", "signed_in_chat", "waiting"] {
                sighting.observe(status, secs(30));
            }
            assert_eq!(sighting.opened_after(), None);
            assert!(matches!(sighting.missed(secs(60)), MissedRecap::NotOpened { waited } if waited == secs(60)));
            assert_eq!(sighting.missed(secs(60)).to_string(), "Teams did not open this recap within 60 seconds.");

            sighting.observe("recap_open", secs(14));
            sighting.observe("collecting_recap", secs(20));
            assert_eq!(sighting.opened_after(), Some(secs(14)), "the first sighting counts");
            assert!(matches!(sighting.missed(secs(60)), MissedRecap::NoSummary));
        }

        #[test]
        fn a_waiting_page_is_logged_by_host_path_visibility_and_frames() {
            let seen: Whereabouts = serde_json::from_str(
                r#"{"status":"waiting","page":"login.microsoftonline.com/{id}/oauth2/v2.0/authorize","visibility":"hidden","focused":false,"frames":0}"#,
            )
            .unwrap();
            assert_eq!(
                seen.place().as_deref(),
                Some("login.microsoftonline.com/{id}/oauth2/v2.0/authorize, hidden, not focused")
            );
            let mut sighting = Sighting::default();
            assert_eq!(sighting.last_wait(), None);
            sighting.waited_on(&seen);
            assert_eq!(
                sighting.last_wait(),
                Some("login.microsoftonline.com/{id}/oauth2/v2.0/authorize, hidden, not focused, 0 frames")
            );

            // Only fixed words for visibility, and a page name of bounded length.
            let odd: Whereabouts = serde_json::from_str(&format!(
                r#"{{"page":"{}","visibility":"prerender <b>"}}"#,
                "a".repeat(300)
            ))
            .unwrap();
            assert_eq!(odd.place(), Some(format!("{}, visibility unknown, focus unknown", "a".repeat(100))));
            // A script that says nothing about the page leaves no line.
            let silent: Whereabouts = serde_json::from_str(r#"{"status":"waiting"}"#).unwrap();
            assert_eq!(silent.place(), None);
            sighting.waited_on(&silent);
            assert!(sighting.last_wait().unwrap().starts_with("login.microsoftonline.com"));
        }

        #[test]
        fn every_state_inside_the_recap_counts_as_open() {
            for status in ["recap_open", "collecting_recap", "waiting_tasks", "ready"] {
                let mut sighting = Sighting::default();
                sighting.observe(status, secs(5));
                assert_eq!(sighting.opened_after(), Some(secs(5)), "{status}");
            }
        }
    }
}

pub fn validate_recap_url(input: &str) -> Result<Url> {
    if input.len() > 8_192 {
        return Err(anyhow!(
            "Teams recap link is too long (maximum 8,192 characters)."
        ));
    }
    let url = Url::parse(input).map_err(|_| anyhow!("Enter a valid Teams meeting recap link."))?;
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    if url.scheme() != "https"
        || !matches!(
            host.as_str(),
            "teams.microsoft.com"
                | "teams.cloud.microsoft"
                | "teams.microsoft.com.mcas.ms"
                | "teams.cloud.microsoft.mcas.ms"
        )
        || url.path() != "/l/meetingrecap"
    {
        return Err(anyhow!(
            "Use a Teams meeting recap sharing link (https://teams.microsoft.com/l/meetingrecap?...)."
        ));
    }
    Ok(url)
}

#[cfg(target_os = "windows")]
pub(crate) mod windows {
    use super::*;
    use ::windows::core::{Interface, PWSTR};
    use serde::Deserialize;
    use std::time::Duration;
    use tauri::{AppHandle, Manager, Runtime, WebviewWindow};
    use tokio::sync::oneshot;
    use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2_18;
    use webview2_com::{
        take_pwstr, CoTaskMemPWSTR, ExecuteScriptCompletedHandler,
        LaunchingExternalUriSchemeEventHandler,
    };

    pub(crate) const SILENT_WAIT: Duration = Duration::from_secs(25);
    pub(crate) const INTERACTIVE_WAIT: Duration = Duration::from_secs(300);
    pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(1500);

    // Teams renders this view asynchronously. Choose the AI summary tab by
    // role/name, then read only its notes and task rows. A layout change
    // produces "waiting" and ultimately a clear error, never a transcript
    // or unrelated Activity text masquerading as a recap.
    const RECAP_SCRIPT: &str = include_str!("teams_recap.js");

    #[derive(Debug, Deserialize)]
    struct Probe {
        status: String,
        #[serde(default)]
        notes: Vec<String>,
        #[serde(default)]
        tasks: Vec<String>,
        #[serde(flatten)]
        whereabouts: super::reading::Whereabouts,
    }

    /// The window's title while a sync reads its recaps.
    const READING_TITLE: &str = "Teams recaps — Meetily is reading them. Leave this window open.";

    pub(crate) async fn block_desktop_handoff<R: Runtime>(window: &WebviewWindow<R>) -> Result<()> {
        let (sender, receiver) = oneshot::channel::<Result<(), String>>();
        window
            .with_webview(move |webview| {
                let outcome = (|| -> ::windows::core::Result<()> {
                    let core = unsafe { webview.controller().CoreWebView2()? };
                    let core18: ICoreWebView2_18 = core.cast()?;
                    let handler =
                        LaunchingExternalUriSchemeEventHandler::create(Box::new(move |_, args| {
                            if let Some(args) = args {
                                let mut uri = PWSTR::null();
                                unsafe { args.Uri(&mut uri)? };
                                let uri = take_pwstr(uri);
                                let scheme = uri.split(':').next().unwrap_or("");
                                if scheme.eq_ignore_ascii_case("msteams")
                                    || scheme.eq_ignore_ascii_case("ms-teams")
                                {
                                    unsafe { args.SetCancel(true)? };
                                    log::info!(
                                        "Blocked Teams desktop-app handoff during recap import"
                                    );
                                }
                            }
                            Ok(())
                        }));
                    let mut token = 0;
                    unsafe { core18.add_LaunchingExternalUriScheme(&handler, &mut token)? };
                    Ok(())
                })()
                .map_err(|error| error.to_string());
                let _ = sender.send(outcome);
            })
            .map_err(|e| anyhow!("Could not configure the Teams WebView2 window: {e}"))?;
        tokio::time::timeout(Duration::from_secs(10), receiver)
            .await
            .map_err(|_| anyhow!("Timed out configuring the Teams WebView2 window."))?
            .map_err(|_| anyhow!("The Teams WebView2 window closed during setup."))?
            .map_err(|e| anyhow!("Could not block Teams desktop-app handoff: {e}"))
    }

    pub(crate) async fn execute_script<R: Runtime>(window: &WebviewWindow<R>, script: &str) -> Result<String> {
        let (sender, receiver) = oneshot::channel::<Result<String, String>>();
        let script = script.to_owned();
        window
            .with_webview(move |webview| {
                let core = unsafe { webview.controller().CoreWebView2() };
                match core {
                    Ok(core) => {
                        let handler = ExecuteScriptCompletedHandler::create(Box::new(
                            move |error_code, result| {
                                let value = error_code
                                    .map(|()| result)
                                    .map_err(|error| format!("WebView2 script failed: {error}"));
                                let _ = sender.send(value);
                                Ok(())
                            },
                        ));
                        let wide = CoTaskMemPWSTR::from(script.as_str());
                        if let Err(error) =
                            unsafe { core.ExecuteScript(*wide.as_ref().as_pcwstr(), &handler) }
                        {
                            // The callback is not invoked if ExecuteScript itself fails.
                            // The receiver's closed channel reports that failure below.
                            log::warn!("Teams recap WebView2 script could not start: {error}");
                        }
                    }
                    Err(error) => {
                        let _ = sender.send(Err(format!("WebView2 is unavailable: {error}")));
                    }
                }
            })
            .map_err(|e| anyhow!("Could not access the Teams WebView2 window: {e}"))?;

        match tokio::time::timeout(Duration::from_secs(10), receiver).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(error))) => Err(anyhow!(error)),
            Ok(Err(_)) => Err(anyhow!("Teams WebView2 did not return a script result.")),
            Err(_) => Err(anyhow!("Timed out reading the Teams recap page.")),
        }
    }

    fn format_recap(probe: Probe) -> Result<TeamsRecap> {
        let mut markdown = String::new();
        if !probe.notes.is_empty() {
            markdown.push_str("## Meeting notes\n\n");
            for note in probe.notes {
                markdown.push_str(note.trim_end());
                markdown.push('\n');
            }
        }
        if !probe.tasks.is_empty() {
            if !markdown.is_empty() {
                markdown.push('\n');
            }
            markdown.push_str("## Follow-up tasks\n\n");
            for task in probe.tasks {
                markdown.push_str(task.trim());
                markdown.push('\n');
            }
        }
        if markdown.trim().is_empty() {
            return Err(anyhow!(
                "The Teams AI summary contained no meeting notes or tasks."
            ));
        }
        Ok(TeamsRecap { markdown })
    }

    async fn poll<R: Runtime>(
        window: &WebviewWindow<R>,
        deadline: tokio::time::Instant,
        source_url: &Url,
        navigation: &mut navigation::Navigation,
        sighting: &mut super::reading::Sighting,
        started: tokio::time::Instant,
        interactive: bool,
    ) -> Result<Option<TeamsRecap>> {
        let mut launcher_since = None;
        let mut tasks_since = None;
        let mut previous_state = String::new();
        while tokio::time::Instant::now() < deadline {
            if window.url().is_err() {
                return Err(anyhow!(
                    "The Teams recap window was closed. Retry the import when ready."
                ));
            }
            if let Ok(raw) = execute_script(window, RECAP_SCRIPT).await {
                if let Ok(probe) = serde_json::from_str::<Probe>(&raw) {
                    // Only fixed state names, and for a waiting page its host and
                    // path: never page text, queries or titles.
                    let status = match probe.status.as_str() {
                        "ready" => "ready",
                        "launcher" => "launcher",
                        "opening_web" => "opening_web",
                        "signed_in_chat" => "signed_in_chat",
                        "waiting_tasks" => "waiting_tasks",
                        "collecting_recap" => "collecting_recap",
                        "recap_open" => "recap_open",
                        _ => "waiting",
                    };
                    let place = if status == "waiting" { probe.whereabouts.place() } else { None };
                    let state = match &place {
                        Some(place) => format!("{status} on {place}"),
                        None => status.to_owned(),
                    };
                    if state != previous_state {
                        match probe.whereabouts.frames().filter(|_| place.is_some()) {
                            Some(frames) => log::info!("Teams recap state: {state}, {frames} frames"),
                            None => log::info!("Teams recap state: {state}"),
                        }
                        previous_state = state;
                    }
                    if place.is_some() {
                        sighting.waited_on(&probe.whereabouts);
                    }
                    sighting.observe(status, started.elapsed());
                    match navigation.observe(status, started.elapsed().as_millis() as u64) {
                        navigation::Action::RetryLink => {
                            log::info!(
                                "Teams recap: retrying original link after sign-in restored a chat"
                            );
                            if interactive {
                                window.set_title("Teams recap — reopening requested meeting")?;
                                window.show()?;
                            }
                            window.navigate(source_url.clone())?;
                        }
                        navigation::Action::ReportWrongPage => {
                            log::info!("Teams recap state: wrong_page_after_retry");
                            return Err(anyhow!(
                                "Teams signed in but opened a chat instead of the requested meeting recap, even after reopening the link. No recap was imported. Check that this link opens the correct recap in Teams on the web, then retry the link in Meetily."
                            ));
                        }
                        navigation::Action::Wait => {}
                    }
                    if probe.status == "ready" {
                        log::info!(
                            "Teams recap extracted after {} s: notes={}, tasks={}",
                            started.elapsed().as_secs(),
                            probe.notes.len(),
                            probe.tasks.len()
                        );
                        return format_recap(probe).map(Some);
                    }
                    if status == "waiting_tasks" {
                        let since = tasks_since.get_or_insert_with(tokio::time::Instant::now);
                        if since.elapsed() > Duration::from_secs(45) {
                            return Err(anyhow!("Teams meeting notes loaded, but its Follow-up tasks section could not be read. No partial recap was saved. Open Follow-up tasks in the recap window and retry."));
                        }
                    } else {
                        tasks_since = None;
                    }
                    if probe.status == "launcher" || probe.status == "opening_web" {
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
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        Ok(None)
    }

    /// The hidden window every Teams flow here uses, on the shared profile,
    /// with the desktop app hand-off blocked before any Teams page loads.
    /// Callers hold `AUTH_FLOW_LOCK` for as long as it is open.
    pub(crate) async fn open_window<R: Runtime>(app: &AppHandle<R>, url: &Url) -> Result<WebviewWindow<R>> {
        let data_dir = app
            .path()
            .app_data_dir()
            .map_err(|e| anyhow!("Could not resolve app data directory: {e}"))?
            .join("sp-webview");
        std::fs::create_dir_all(&data_dir)?;
        let window =
            super::super::sharepoint::create_auth_window_deferred(app, url, &data_dir).await?;
        if let Err(error) = block_desktop_handoff(&window).await {
            let _ = window.close();
            return Err(error);
        }
        Ok(window)
    }

    /// Read the one recap a user asked for. The window appears when the hidden
    /// phase finds nothing, so the user can sign in.
    pub async fn extract<R: Runtime>(app: &AppHandle<R>, url: &Url) -> Result<TeamsRecap> {
        // The same lock and profile protect the existing SharePoint cookie
        // flow from a competing Teams window destroying its auth webview.
        let _auth_flow = super::super::sharepoint::AUTH_FLOW_LOCK.lock().await;
        let window = open_window(app, url).await?;

        let result = async {
            window.navigate(url.clone())?;
            let started = tokio::time::Instant::now();
            let mut navigation = navigation::Navigation::default();
            let mut sighting = super::reading::Sighting::default();
            if let Some(recap) =
                poll(&window, started + SILENT_WAIT, url, &mut navigation, &mut sighting, started, true).await?
            {
                return Ok(recap);
            }
            window.set_title("Teams recap — sign in and open the requested AI summary")?;
            window.show()?;
            window.set_focus()?;
            poll(&window, tokio::time::Instant::now() + INTERACTIVE_WAIT, url, &mut navigation, &mut sighting, started, true)
                .await?
                .ok_or_else(|| anyhow!(
                    "Teams did not show an AI summary for this link. Sign in if prompted, then check that the recap's AI summary tab is available."
                ))
        }
        .await;
        let _ = window.close();
        result
    }

    /// A sync's one Teams window: it reads the Recap app's list, then the
    /// recaps one after another, so Teams starts and signs in once per sync.
    /// Dropping it closes the window.
    pub(crate) struct RecapReader<R: Runtime> {
        window: WebviewWindow<R>,
        reads: usize,
        /// Whether a recap has opened in this window yet.
        opened_any: bool,
        /// Held while the window is open: every Teams and SharePoint flow
        /// uses its profile and window label.
        _auth_flow: tokio::sync::MutexGuard<'static, ()>,
    }

    impl<R: Runtime> RecapReader<R> {
        pub(crate) async fn open(app: &AppHandle<R>) -> Result<Self> {
            let auth_flow = super::super::sharepoint::AUTH_FLOW_LOCK.lock().await;
            // Names the window only: it opens on about:blank.
            let teams = Url::parse(crate::audio::teams_recap_list::TEAMS_WEB_URL).expect("constant URL");
            let window = open_window(app, &teams).await?;
            Ok(Self { window, reads: 0, opened_any: false, _auth_flow: auth_flow })
        }

        /// Read the Recap app's list in this window. It shows itself when the
        /// list is not read while hidden, so the user can pick their account,
        /// and the recaps are then read where they signed in.
        pub(crate) async fn list(
            &self,
            since: chrono::DateTime<chrono::Utc>,
        ) -> Result<crate::audio::teams_recap_list::RecapListing> {
            crate::audio::teams_recap_list::windows::list_in(&self.window, since, false).await
        }

        /// Read one recap, giving up on it when it does not open in time. If
        /// the window is hidden and no recap has opened in it yet, it shows
        /// itself after [`SILENT_WAIT`], as the list does.
        pub(crate) async fn read(&mut self, url: &Url) -> std::result::Result<TeamsRecap, MissedRecap> {
            let wait = super::reading::wait_for(self.reads);
            if self.reads == 0 {
                let _ = self.window.set_title(READING_TITLE);
            }
            self.reads += 1;
            self.leave_page().await.map_err(MissedRecap::Failed)?;
            self.window
                .navigate(url.clone())
                .map_err(|error| MissedRecap::Failed(error.into()))?;
            let started = tokio::time::Instant::now();
            let deadline = started + wait;
            let mut navigation = navigation::Navigation::default();
            let mut sighting = super::reading::Sighting::default();
            let hidden = matches!(self.window.is_visible(), Ok(false));
            let reveal_at = (hidden && !self.opened_any).then(|| (started + SILENT_WAIT).min(deadline));
            let mut result =
                poll(&self.window, reveal_at.unwrap_or(deadline), url, &mut navigation, &mut sighting, started, false).await;
            if let Some(reveal_at) = reveal_at.filter(|at| *at < deadline) {
                if matches!(result, Ok(None)) {
                    if sighting.opened_after().is_none() {
                        log::info!(
                            "Teams recap: showing the window, no recap opened in {} s while it was hidden",
                            (reveal_at - started).as_secs()
                        );
                        let _ = self.window.show();
                    }
                    result = poll(&self.window, deadline, url, &mut navigation, &mut sighting, started, false).await;
                }
            }
            if sighting.opened_after().is_some() {
                self.opened_any = true;
            }
            match result {
                Ok(Some(recap)) => Ok(recap),
                Ok(None) => {
                    match sighting.opened_after() {
                        Some(after) => log::info!(
                            "Teams recap: open after {} s, but no AI summary within {} s",
                            after.as_secs(),
                            wait.as_secs()
                        ),
                        None => match sighting.last_wait() {
                            Some(place) => log::info!(
                                "Teams recap: not open within {} s, last waiting on {place}",
                                wait.as_secs()
                            ),
                            None => log::info!("Teams recap: not open within {} s", wait.as_secs()),
                        },
                    }
                    Err(sighting.missed(wait))
                }
                Err(error) => Err(MissedRecap::Failed(error)),
            }
        }

        /// Go to an empty page and wait until the previous recap's page is
        /// gone, so a poll that runs before the next recap loads cannot read
        /// the previous one in its place.
        async fn leave_page(&self) -> Result<()> {
            self.window.navigate(Url::parse("about:blank").expect("constant URL"))?;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            while tokio::time::Instant::now() < deadline {
                if execute_script(&self.window, "location.href").await.ok().as_deref() == Some("\"about:blank\"") {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Err(anyhow!("The Teams window did not leave the previous recap's page."))
        }
    }

    impl<R: Runtime> Drop for RecapReader<R> {
        fn drop(&mut self) {
            let _ = self.window.close();
        }
    }
}

#[cfg(target_os = "windows")]
pub(crate) use windows::RecapReader;

/// Elsewhere there is no WebView2 window to read recaps in.
#[cfg(not(target_os = "windows"))]
pub(crate) struct RecapReader<R: tauri::Runtime>(std::marker::PhantomData<fn() -> R>);

#[cfg(not(target_os = "windows"))]
impl<R: tauri::Runtime> RecapReader<R> {
    pub(crate) async fn open(_app: &tauri::AppHandle<R>) -> Result<Self> {
        Err(anyhow!(WINDOWS_ONLY))
    }

    pub(crate) async fn list(
        &self,
        _since: chrono::DateTime<chrono::Utc>,
    ) -> Result<crate::audio::teams_recap_list::RecapListing> {
        Err(anyhow!(WINDOWS_ONLY))
    }

    pub(crate) async fn read(&mut self, _url: &Url) -> std::result::Result<TeamsRecap, MissedRecap> {
        Err(MissedRecap::Failed(anyhow!(WINDOWS_ONLY)))
    }
}

/// Read one recap the user asked for: the window appears if Teams needs them.
/// A sync reads its recaps through [`RecapReader`] instead.
pub async fn extract_teams_recap<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    source_url: &str,
) -> Result<TeamsRecap> {
    let url = validate_recap_url(source_url)?;
    #[cfg(target_os = "windows")]
    {
        windows::extract(app, &url).await
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (app, url);
        Err(anyhow!(WINDOWS_ONLY))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_meeting_recap_links() {
        assert!(
            validate_recap_url("https://teams.microsoft.com/l/meetingrecap?threadId=test").is_ok()
        );
        assert!(validate_recap_url(
            "https://teams.microsoft.com.mcas.ms/l/meetingrecap?threadId=test"
        )
        .is_ok());
        assert!(validate_recap_url("https://teams.microsoft.com/v2/").is_err());
        assert!(validate_recap_url("https://evil.example/l/meetingrecap").is_err());
        assert!(validate_recap_url("http://teams.microsoft.com/l/meetingrecap").is_err());
    }

    #[test]
    fn recap_link_and_recording_link_share_one_identity() {
        let direct = "https://contoso-my.sharepoint.com/personal/jdoe/Documents/Recordings/Weekly sync-20260721_100221-Meeting Recording.mp4";
        let encoded = "https://Contoso-my.sharepoint.com/personal/jdoe/Documents/Recordings/Weekly%20sync-20260721_100221-Meeting%20Recording.mp4?tempauth=abc";
        let recap = format!(
            "https://teams.microsoft.com/l/meetingrecap?threadId=19%3Ameeting&fileUrl={}",
            url::form_urlencoded::byte_serialize(encoded.as_bytes()).collect::<String>()
        );
        let stream = "https://contoso-my.sharepoint.com/personal/jdoe/_layouts/15/stream.aspx?id=%2Fpersonal%2Fjdoe%2FDocuments%2FRecordings%2FWeekly%20sync-20260721_100221-Meeting%20Recording.mp4";
        let key = meeting_identity_key(direct);
        assert_eq!(
            key,
            "contoso-my.sharepoint.com/personal/jdoe/documents/recordings/weekly sync-20260721_100221-meeting recording.mp4"
        );
        assert_eq!(meeting_identity_key(encoded), key);
        assert_eq!(meeting_identity_key(&recap), key);
        assert_eq!(meeting_identity_key(stream), key);
    }

    /// JavaScript's `encodeURIComponent`, which `teams_recap_list.js` applies
    /// to every value of the links it builds.
    fn encode_uri_component(value: &str) -> String {
        value
            .bytes()
            .map(|byte| match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' => (byte as char).to_string(),
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')' => (byte as char).to_string(),
                _ => format!("%{byte:02X}"),
            })
            .collect()
    }

    /// A link as `teams_recap_list.js` builds it from a Recap app row whose
    /// `recommendationData.url` is `file`: the Share button's keys in its
    /// order, `fileUrl` with Office's `?web=1`, on the list page's host.
    fn listed_link(host: &str, file: &str) -> String {
        let occurrence = "040000008200E00074C5B7101A82E00807EA0B05000000000000000000000000000000001000000000112233445566778899AABBCCDDEEFF";
        let series = format!("{}00000000{}", &occurrence[..32], &occurrence[40..]);
        let file_url = format!("{file}?web=1");
        let params = [
            ("driveId", "b!drive"),
            ("driveItemId", "01ITEM"),
            ("sitePath", file),
            ("fileUrl", file_url.as_str()),
            ("iCalUid", occurrence),
            ("masterICalUid", series.as_str()),
            ("threadId", "19:meeting_abc@thread.v2"),
            ("organizerId", "org-1"),
            ("callId", "call-1"),
            ("threadType", "meeting"),
            ("meetingType", "Recurring"),
            ("subType", "RecapSharingLink_RecapCore"),
            ("recapType", "ODSPTranscript"),
        ];
        let query: Vec<String> = params
            .iter()
            .map(|(key, value)| format!("{key}={}", encode_uri_component(value)))
            .collect();
        format!("https://{host}/l/meetingrecap?{}", query.join("&"))
    }

    #[test]
    fn a_link_built_from_a_recap_app_row_keys_and_dates_on_its_recording() {
        let dir = "https://contoso-my.sharepoint.com/personal/jdoe_contoso_com/Documents/Recordings";
        // (as the row writes the file, as SharePoint lists it, its title)
        let files = [
            (
                format!("{dir}/Weekly%20sync-20261105_133311-Meeting%20Recording.mp4"),
                format!("{dir}/Weekly sync-20261105_133311-Meeting Recording.mp4"),
                "Weekly sync",
            ),
            (
                format!("{dir}/Revisi%C3%B3n%20semanal-20261105_133311-Grabaci%C3%B3n%20de%20la%20reuni%C3%B3n.mp4"),
                format!("{dir}/Revisión semanal-20261105_133311-Grabación de la reunión.mp4"),
                "Revisión semanal",
            ),
        ];
        for (row_file, listed_file, title) in &files {
            for host in ["teams.cloud.microsoft", "teams.microsoft.com"] {
                let link = listed_link(host, row_file);
                assert!(validate_recap_url(&link).is_ok(), "{link}");
                assert_eq!(link_kind(&link), LinkKind::RecapOfFile);
                assert_eq!(meeting_identity_key(&link), meeting_identity_key(listed_file));
                assert_eq!(meeting_identity_key(&link), meeting_identity_key(row_file));
                let metadata = recording_metadata(&Url::parse(&link).unwrap()).expect("the file name's stamp");
                assert_eq!(metadata.title, *title);
                assert_eq!(metadata.recorded_at, "2026-11-05T13:33:11");
            }
        }
    }

    #[test]
    fn a_listed_link_to_a_utc_stamped_file_keys_on_it_without_a_local_time() {
        // A recording in the user's own OneDrive carries a UTC stamp, which is
        // not the local time `recorded_at` means; the sync passes the row's
        // start time instead.
        let file = "https://contoso-my.sharepoint.com/personal/jdoe_contoso_com/Documents/Recordings/Weekly%20sync-20261105_233029UTC-Meeting%20Recording.mp4";
        let link = listed_link("teams.cloud.microsoft", file);
        assert_eq!(meeting_identity_key(&link), meeting_identity_key(file));
        assert_eq!(recording_metadata(&Url::parse(&link).unwrap()), None);
    }

    #[test]
    fn recap_link_without_file_url_keys_on_itself() {
        let a = "https://teams.microsoft.com/l/meetingrecap?threadId=19%3Aabc&organizerId=1";
        let b = "https://teams.microsoft.com/l/meetingrecap?threadId=19%3Aabc&organizerId=2";
        assert_eq!(meeting_identity_key(a), meeting_identity_key(&format!(" {a} ")));
        assert_ne!(meeting_identity_key(a), meeting_identity_key(b));
        assert_eq!(meeting_identity_key("not a url"), "not a url");
    }

    #[test]
    fn a_link_says_whether_it_is_a_file_or_a_recap_and_whether_that_names_one() {
        let file = "https://c-my.sharepoint.com/personal/j/Documents/Recordings/S-20261102_100000-Meeting%20Recording.mp4";
        assert_eq!(link_kind(file), LinkKind::File);
        assert_eq!(
            link_kind("https://c-my.sharepoint.com/personal/j/_layouts/15/stream.aspx?id=%2Fa%2FS.mp4"),
            LinkKind::File
        );
        assert_eq!(
            link_kind("https://teams.microsoft.com/l/meetingrecap?threadId=19%3Ax&fileUrl=https%3A%2F%2Fc.sharepoint.com%2FS.mp4%3Fweb%3D1"),
            LinkKind::RecapOfFile
        );
        assert_eq!(link_kind("https://teams.microsoft.com/l/meetingrecap?threadId=19%3Ax"), LinkKind::RecapWithoutFile);
        assert_eq!(
            link_kind("https://teams.microsoft.com/l/meetingrecap?threadId=19%3Ax&fileUrl="),
            LinkKind::RecapWithoutFile
        );
    }

    #[test]
    fn the_parts_of_a_long_recording_share_one_whole_recording_key() {
        let whole = |link: &str| whole_recording_key(&meeting_identity_key(link)).to_owned();
        let dir = "https://c-my.sharepoint.com/personal/j/Documents/Recordings";
        let first = whole(&format!("{dir}/Workshop-20261016_150059-Meeting%20Recording.mp4"));
        assert_eq!(first, "c-my.sharepoint.com/personal/j/documents/recordings/workshop-20261016_150059");
        assert_eq!(whole(&format!("{dir}/Workshop-20261016_150059-Meeting%20Recording%202.mp4")), first);
        assert_eq!(whole(&format!("{dir}/Workshop-20261016_150059-Meeting%20Transcript.mp4")), first);
        let recap = "https://teams.microsoft.com/l/meetingrecap?fileUrl=https%3A%2F%2Fc-my.sharepoint.com%2Fpersonal%2Fj%2FDocuments%2FRecordings%2FWorkshop-20261016_150059-Meeting%20Recording%202.mp4%3Fweb%3D1";
        assert_eq!(whole(recap), first);

        assert_ne!(whole(&format!("{dir}/Workshop-20261017_150059-Meeting%20Recording.mp4")), first);
        // Not the Teams form: the key stays whole.
        for key in ["h/a/meeting recording.mp4", "h/a/x-meeting recording notes.mp4", "teams.microsoft.com/l/meetingrecap?threadid=1"] {
            assert_eq!(whole_recording_key(key), key);
        }
        // The title may hold the words too; the last occurrence decides.
        assert_eq!(
            whole_recording_key("h/a/review-meeting recording rules-20261016_150059-meeting transcript.mp4"),
            "h/a/review-meeting recording rules-20261016_150059"
        );
    }

    #[test]
    fn a_spanish_recording_keys_and_dates_like_an_english_one() {
        let dir = "https://c-my.sharepoint.com/personal/j/Documents/Recordings";
        // As SharePoint lists the path, and as the recap's Share button encodes it.
        let raw = format!("{dir}/Revisión semanal-20261105_133311-Grabación de la reunión.mp4");
        let encoded = format!(
            "{dir}/Revisi%C3%B3n%20semanal-20261105_133311-Grabaci%C3%B3n%20de%20la%20reuni%C3%B3n.mp4"
        );
        let recap = format!(
            "https://teams.microsoft.com/l/meetingrecap?threadId=19%3Ax&fileUrl={}",
            url::form_urlencoded::byte_serialize(format!("{encoded}?web=1").as_bytes()).collect::<String>()
        );
        let key = meeting_identity_key(&raw);
        assert_eq!(meeting_identity_key(&encoded), key);
        assert_eq!(meeting_identity_key(&recap), key);
        assert_eq!(
            whole_recording_key(&key),
            "c-my.sharepoint.com/personal/j/documents/recordings/revisión semanal-20261105_133311"
        );

        let read = recording_metadata(&Url::parse(&recap).unwrap()).unwrap();
        assert_eq!(read.title, "Revisión semanal");
        assert_eq!(read.recorded_at, "2026-11-05T13:33:11");
    }
}
