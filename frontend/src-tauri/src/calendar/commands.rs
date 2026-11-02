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
pub async fn set_calendar_feed_url(url: String, state: State<'_, CalendarManagerState>) -> Result<CalendarSettings, String> {
    let guard = state.read().await;
    let manager = guard.as_ref().ok_or_else(|| NOT_READY.to_string())?;
    manager.set_feed_url(&url).await.map_err(|error| error.to_string())?;
    Ok(manager.get_settings().await)
}

#[tauri::command]
pub async fn clear_calendar_feed_url(state: State<'_, CalendarManagerState>) -> Result<CalendarSettings, String> {
    let guard = state.read().await;
    let manager = guard.as_ref().ok_or_else(|| NOT_READY.to_string())?;
    manager.clear_feed_url().await.map_err(|error| error.to_string())?;
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

#[tauri::command]
pub async fn refresh_calendar_now(state: State<'_, CalendarManagerState>) -> Result<(), String> {
    let guard = state.read().await;
    let manager = guard.as_ref().ok_or_else(|| NOT_READY.to_string())?;
    manager.refresh_now();
    Ok(())
}
