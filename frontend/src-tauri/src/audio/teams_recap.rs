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
    if url.path() == "/l/meetingrecap" {
        let file_url = url
            .query_pairs()
            .find(|(key, _)| key == "fileUrl")
            .map(|(_, value)| value.into_owned())
            .filter(|value| !value.trim().is_empty());
        return match file_url.as_deref().and_then(|value| Url::parse(value.trim()).ok()) {
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

/// An unattended extraction found no AI summary in time. The recap sync reads
/// this as "not ready", so keep the phrase `is_not_ready` looks for.
pub(crate) const UNATTENDED_NO_SUMMARY: &str =
    "Teams did not show an AI summary for this link within a minute.";

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
    /// All an unattended extraction gets, since nobody can help it along.
    const UNATTENDED_WAIT: Duration = Duration::from_secs(60);
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
    }

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
        started: tokio::time::Instant,
        interactive: bool,
    ) -> Result<Option<TeamsRecap>> {
        let mut launcher_since = None;
        let mut tasks_since = None;
        let mut previous_status = String::new();
        while tokio::time::Instant::now() < deadline {
            if window.url().is_err() {
                return Err(anyhow!(
                    "The Teams recap window was closed. Retry the import when ready."
                ));
            }
            if let Ok(raw) = execute_script(window, RECAP_SCRIPT).await {
                if let Ok(probe) = serde_json::from_str::<Probe>(&raw) {
                    // Only log fixed state names, never page text or private URLs.
                    let status = match probe.status.as_str() {
                        "ready" => "ready",
                        "launcher" => "launcher",
                        "opening_web" => "opening_web",
                        "signed_in_chat" => "signed_in_chat",
                        "waiting_tasks" => "waiting_tasks",
                        "collecting_recap" => "collecting_recap",
                        _ => "waiting",
                    };
                    if status != previous_status {
                        log::info!("Teams recap state: {status}");
                        previous_status = status.to_owned();
                    }
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
                            "Teams recap extracted: notes={}, tasks={}",
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

    /// `interactive` shows the window when the hidden phase finds nothing, so
    /// the user can sign in. Without it the window stays hidden throughout.
    pub async fn extract<R: Runtime>(
        app: &AppHandle<R>,
        url: &Url,
        interactive: bool,
    ) -> Result<TeamsRecap> {
        // The same lock and profile protect the existing SharePoint cookie
        // flow from a competing Teams window destroying its auth webview.
        let _auth_flow = super::super::sharepoint::AUTH_FLOW_LOCK.lock().await;
        let data_dir = app
            .path()
            .app_data_dir()
            .map_err(|e| anyhow!("Could not resolve app data directory: {e}"))?
            .join("sp-webview");
        std::fs::create_dir_all(&data_dir)?;
        let window =
            super::super::sharepoint::create_auth_window_deferred(app, url, &data_dir).await?;

        let result = async {
            block_desktop_handoff(&window).await?;
            window.navigate(url.clone())?;
            let started = tokio::time::Instant::now();
            let mut navigation = navigation::Navigation::default();
            let hidden_wait = if interactive { SILENT_WAIT } else { UNATTENDED_WAIT };
            if let Some(recap) =
                poll(&window, started + hidden_wait, url, &mut navigation, started, interactive).await?
            {
                return Ok(recap);
            }
            if !interactive {
                return Err(anyhow!(UNATTENDED_NO_SUMMARY));
            }
            window.set_title("Teams recap — sign in and open the requested AI summary")?;
            window.show()?;
            window.set_focus()?;
            poll(&window, tokio::time::Instant::now() + INTERACTIVE_WAIT, url, &mut navigation, started, true)
                .await?
                .ok_or_else(|| anyhow!(
                    "Teams did not show an AI summary for this link. Sign in if prompted, then check that the recap's AI summary tab is available."
                ))
        }
        .await;
        let _ = window.close();
        result
    }
}

/// Read one recap the user asked for: the window appears if Teams needs them.
pub async fn extract_teams_recap<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    source_url: &str,
) -> Result<TeamsRecap> {
    extract(app, source_url, true).await
}

/// Read one recap of many, with nobody watching: no window, and at most a
/// minute before it fails with [`UNATTENDED_NO_SUMMARY`].
pub async fn extract_teams_recap_unattended<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    source_url: &str,
) -> Result<TeamsRecap> {
    extract(app, source_url, false).await
}

async fn extract<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    source_url: &str,
    interactive: bool,
) -> Result<TeamsRecap> {
    let url = validate_recap_url(source_url)?;
    #[cfg(target_os = "windows")]
    {
        windows::extract(app, &url, interactive).await
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (app, url, interactive);
        Err(anyhow!(
            "Automatic Teams recap extraction requires Meetily on Windows with WebView2."
        ))
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

    #[test]
    fn recap_link_without_file_url_keys_on_itself() {
        let a = "https://teams.microsoft.com/l/meetingrecap?threadId=19%3Aabc&organizerId=1";
        let b = "https://teams.microsoft.com/l/meetingrecap?threadId=19%3Aabc&organizerId=2";
        assert_eq!(meeting_identity_key(a), meeting_identity_key(&format!(" {a} ")));
        assert_ne!(meeting_identity_key(a), meeting_identity_key(b));
        assert_eq!(meeting_identity_key("not a url"), "not a url");
    }
}
