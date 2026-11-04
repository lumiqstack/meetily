use super::{
    manager::CalendarManager,
    types::{CalendarEvent, CalendarSettings, CalendarStatus},
};
use std::sync::Arc;
use tauri::{State, Wry};
use tokio::sync::RwLock;

pub type CalendarManagerState = Arc<RwLock<Option<CalendarManager>>>;

pub async fn initialize_calendar_manager(app_handle: tauri::AppHandle<Wry>) -> anyhow::Result<CalendarManager> {
    CalendarManager::new(app_handle).await
}

const NOT_READY: &str = "Calendar manager not initialized";

#[tauri::command]
pub async fn get_calendar_settings(state: State<'_, CalendarManagerState>) -> Result<CalendarSettings, String> {
    let guard = state.read().await;
    let manager = guard.as_ref().ok_or_else(|| NOT_READY.to_string())?;
    Ok(manager.get_settings().await)
}

#[tauri::command]
pub async fn set_calendar_settings(
    settings: CalendarSettings,
    state: State<'_, CalendarManagerState>,
) -> Result<CalendarSettings, String> {
    let guard = state.read().await;
    let manager = guard.as_ref().ok_or_else(|| NOT_READY.to_string())?;
    manager.update_settings(settings).await.map_err(|error| error.to_string())?;
    Ok(manager.get_settings().await)
}

#[tauri::command]
pub async fn get_calendar_status(state: State<'_, CalendarManagerState>) -> Result<CalendarStatus, String> {
    let guard = state.read().await;
    let manager = guard.as_ref().ok_or_else(|| NOT_READY.to_string())?;
    Ok(manager.get_status().await)
}

#[tauri::command]
pub async fn get_upcoming_calendar_events(state: State<'_, CalendarManagerState>) -> Result<Vec<CalendarEvent>, String> {
    let guard = state.read().await;
    let manager = guard.as_ref().ok_or_else(|| NOT_READY.to_string())?;
    Ok(manager.upcoming_events().await)
}

/// Hosts a Teams join link may point at. The command opens nothing else, so
/// a stored event body cannot turn into an arbitrary URL launch.
const TEAMS_HOSTS: [&str; 2] = ["teams.microsoft.com", "teams.live.com"];

pub fn is_teams_join_url(url: &str) -> bool {
    url::Url::parse(url)
        .ok()
        .filter(|parsed| parsed.scheme() == "https")
        .and_then(|parsed| parsed.host_str().map(|host| host.to_ascii_lowercase()))
        .is_some_and(|host| TEAMS_HOSTS.iter().any(|allowed| host == *allowed))
}

/// Opens a Teams join link in the system browser (which hands it to the
/// Teams app). Used by "Join and record" and the meeting details pane.
#[tauri::command]
pub async fn open_teams_join_url(url: String) -> Result<(), String> {
    if !is_teams_join_url(&url) {
        return Err("Not a Microsoft Teams join link".to_string());
    }
    tokio::task::spawn_blocking(move || open::that(&url))
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| format!("Could not open the Teams link: {error}"))
}

#[cfg(test)]
mod tests {
    use super::is_teams_join_url;

    #[test]
    fn accepts_teams_hosts_over_https_only() {
        assert!(is_teams_join_url("https://teams.microsoft.com/l/meetup-join/19%3ameeting_x"));
        assert!(is_teams_join_url("https://teams.live.com/meet/123"));
        assert!(!is_teams_join_url("http://teams.microsoft.com/l/meetup-join/x"));
        assert!(!is_teams_join_url("https://teams.microsoft.com.evil.example/x"));
        assert!(!is_teams_join_url("https://example.com/teams.microsoft.com"));
        assert!(!is_teams_join_url("file:///etc/passwd"));
        assert!(!is_teams_join_url("not a url"));
    }
}

#[tauri::command]
pub async fn refresh_calendar_now(state: State<'_, CalendarManagerState>) -> Result<(), String> {
    let guard = state.read().await;
    let manager = guard.as_ref().ok_or_else(|| NOT_READY.to_string())?;
    manager.refresh_now();
    Ok(())
}
