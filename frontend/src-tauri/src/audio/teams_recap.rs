//! Read a Teams AI recap from the same persisted WebView2 profile used for
//! SharePoint sign-in. The script only reads the rendered recap DOM; it does
//! not expose Tauri IPC to the remote page or copy meeting text to a URL,
//! clipboard, log, or external service.

use anyhow::{anyhow, Result};
use url::Url;

#[derive(Debug, Clone)]
pub struct TeamsRecap {
    pub markdown: String,
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
mod windows {
    use super::*;
    use serde::Deserialize;
    use std::time::Duration;
    use tauri::{AppHandle, Manager, Runtime, WebviewWindow};
    use tokio::sync::oneshot;
    use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2_18;
    use webview2_com::{
        take_pwstr, CoTaskMemPWSTR, ExecuteScriptCompletedHandler,
        LaunchingExternalUriSchemeEventHandler,
    };
    use windows::core::{Interface, PWSTR};

    const SILENT_WAIT: Duration = Duration::from_secs(25);
    const INTERACTIVE_WAIT: Duration = Duration::from_secs(300);
    const POLL_INTERVAL: Duration = Duration::from_millis(1500);

    // Teams renders this view asynchronously. Choose the AI summary tab by
    // role/name, then read only its notes and task rows. A layout change
    // produces "waiting" and ultimately a clear error, never a transcript
    // or unrelated Activity text masquerading as a recap.
    const RECAP_SCRIPT: &str = r#"(() => {
      const host = location.hostname.toLowerCase();
      if (!['teams.microsoft.com', 'teams.cloud.microsoft',
            'teams.microsoft.com.mcas.ms', 'teams.cloud.microsoft.mcas.ms'].includes(host)) {
        return { status: 'waiting' };
      }
      const text = el => (el?.innerText || el?.textContent || '').replace(/\s+/g, ' ').trim();
      const tabs = [...document.querySelectorAll('[role="tab"]')];
      const tab = tabs.find(el => /^ai summary$/i.test(text(el)));
      if (!tab) {
        const choices = [...document.querySelectorAll('a,button,[role="button"]')];
        const webChoice = choices.find(el =>
          /^(continue (on|in) (this|the) browser|use the web app( instead)?|open (in|on) (the )?web( app)?|continue in browser)$/i
            .test(text(el) || el.getAttribute('aria-label') || ''));
        if (webChoice) {
          webChoice.click();
          return { status: 'opening_web' };
        }
        const page = text(document.body).slice(0, 1200);
        if (/open (your|the) teams app|join (on|in) (the )?teams app|use the web app/i.test(page)) {
          return { status: 'launcher' };
        }
        return { status: 'waiting' };
      }
      if (tab.getAttribute('aria-selected') !== 'true') {
        tab.click();
        return { status: 'waiting' };
      }
      const headings = [...document.querySelectorAll('h2,[role="heading"]')];
      const notesHeading = headings.find(el => /^meeting notes$/i.test(text(el)));
      const tasksHeading = headings.find(el => /^follow-up tasks$/i.test(text(el)));
      if (!notesHeading && !tasksHeading) return { status: 'waiting' };
      const expand = [...document.querySelectorAll('button')]
        .find(el => /^expand all$/i.test(text(el)));
      if (expand && !window.__meetilyRecapExpanded) {
        window.__meetilyRecapExpanded = true;
        expand.click();
        return { status: 'waiting' };
      }
      const follows = (first, second) =>
        !!(first.compareDocumentPosition(second) & Node.DOCUMENT_POSITION_FOLLOWING);
      const rowsAfter = (heading, before) => heading
        ? [...document.querySelectorAll('[role="row"]')]
            .filter(row => follows(heading, row) && (!before || follows(row, before)))
            .map(text).filter(Boolean)
        : [];
      const notes = rowsAfter(notesHeading, tasksHeading);
      const tasks = rowsAfter(tasksHeading, null)
        .map(s => s.replace(/^\s*[•.\-]\s*/, ''));
      if (!notes.length && !tasks.length) return { status: 'waiting' };
      return { status: 'ready', notes, tasks };
    })()"#;

    #[derive(Debug, Deserialize)]
    struct Probe {
        status: String,
        #[serde(default)]
        notes: Vec<String>,
        #[serde(default)]
        tasks: Vec<String>,
    }

    async fn block_desktop_handoff<R: Runtime>(window: &WebviewWindow<R>) -> Result<()> {
        let (sender, receiver) = oneshot::channel::<Result<(), String>>();
        window
            .with_webview(move |webview| {
                let outcome = (|| -> windows::core::Result<()> {
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

    async fn execute_script<R: Runtime>(window: &WebviewWindow<R>, script: &str) -> Result<String> {
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
                markdown.push_str("- ");
                markdown.push_str(note.trim());
                markdown.push('\n');
            }
        }
        if !probe.tasks.is_empty() {
            if !markdown.is_empty() {
                markdown.push('\n');
            }
            markdown.push_str("## Follow-up tasks\n\n");
            for task in probe.tasks {
                markdown.push_str("- ");
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
    ) -> Result<Option<TeamsRecap>> {
        let mut launcher_since = None;
        while tokio::time::Instant::now() < deadline {
            if let Ok(raw) = execute_script(window, RECAP_SCRIPT).await {
                if let Ok(probe) = serde_json::from_str::<Probe>(&raw) {
                    if probe.status == "ready" {
                        return format_recap(probe).map(Some);
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

    pub async fn extract<R: Runtime>(app: &AppHandle<R>, url: &Url) -> Result<TeamsRecap> {
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
            if let Some(recap) = poll(&window, tokio::time::Instant::now() + SILENT_WAIT).await? {
                return Ok(recap);
            }
            window.show()?;
            window.set_focus()?;
            poll(&window, tokio::time::Instant::now() + INTERACTIVE_WAIT)
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
}
