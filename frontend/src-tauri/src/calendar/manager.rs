use super::{
    outlook_com::OutlookComSource,
    scheduler::NudgePlanner,
    source::CalendarSource,
    types::{
        CalendarEvent, CalendarMeetingUpcomingPayload, CalendarSettings, CalendarStatus, SourceStatus,
        LOOKAHEAD_HOURS, LOOKBACK_HOURS, MAX_NUDGE_LEAD_MINUTES, POLL_INTERVAL_SECONDS, STATUS_EVENT,
        TICK_SECONDS, UPCOMING_EVENT,
    },
};
use crate::notifications::{commands::NotificationManagerState, types::Notification};
use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use log::{error, info, warn};
use std::{path::PathBuf, sync::Arc, time::Instant};
use tauri::{AppHandle, Emitter, Manager, Runtime, Wry};
use tokio::sync::{Notify, RwLock};
use tokio_util::sync::CancellationToken;

pub struct CalendarManager<R: Runtime = Wry> {
    app_handle: AppHandle<R>,
    settings: Arc<RwLock<CalendarSettings>>,
    status: Arc<RwLock<CalendarStatus>>,
    events: Arc<RwLock<Vec<CalendarEvent>>>,
    source: Arc<dyn CalendarSource>,
    /// Woken by a settings change or a manual refresh to poll before the interval.
    refresh: Arc<Notify>,
    cancellation: Arc<RwLock<Option<CancellationToken>>>,
}

impl<R: Runtime> CalendarManager<R> {
    pub async fn new(app_handle: AppHandle<R>) -> Result<Self> {
        let settings = load_settings().await.unwrap_or_default();

        Ok(Self {
            app_handle,
            status: Arc::new(RwLock::new(CalendarStatus::stopped(settings.clone()))),
            settings: Arc::new(RwLock::new(settings)),
            events: Arc::new(RwLock::new(Vec::new())),
            source: Arc::new(OutlookComSource::new()),
            refresh: Arc::new(Notify::new()),
            cancellation: Arc::new(RwLock::new(None)),
        })
    }

    pub async fn start_if_enabled(&self) {
        if self.settings.read().await.calendar_enabled {
            self.start().await;
        }
    }

    pub async fn start(&self) {
        let token = {
            let mut cancellation = self.cancellation.write().await;
            if cancellation.is_some() {
                return;
            }
            let token = CancellationToken::new();
            *cancellation = Some(token.clone());
            token
        };

        self.status.write().await.running = true;

        let app_handle = self.app_handle.clone();
        let settings = Arc::clone(&self.settings);
        let status = Arc::clone(&self.status);
        let events = Arc::clone(&self.events);
        let source = Arc::clone(&self.source);
        let refresh = Arc::clone(&self.refresh);

        tauri::async_runtime::spawn(async move {
            info!("Starting calendar task");
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(TICK_SECONDS));
            let mut planner = NudgePlanner::new(Utc::now());
            let mut last_poll: Option<Instant> = None;

            loop {
                tokio::select! {
                    _ = token.cancelled() => {
                        info!("Calendar task stopped");
                        break;
                    }
                    _ = tick.tick() => {}
                    _ = refresh.notified() => {
                        last_poll = None;
                    }
                }

                let current = settings.read().await.clone();
                if !current.calendar_enabled {
                    continue;
                }
                let now = Utc::now();

                let poll_due = last_poll.map_or(true, |at| {
                    at.elapsed() >= std::time::Duration::from_secs(POLL_INTERVAL_SECONDS)
                });
                if poll_due {
                    last_poll = Some(Instant::now());
                    let from = now - Duration::hours(LOOKBACK_HOURS);
                    let to = now + Duration::hours(LOOKAHEAD_HOURS);
                    let poll = source.poll(from, to).await;

                    if let Some(list) = poll.events {
                        *events.write().await = list;
                    }
                    let snapshot = {
                        let list = events.read().await;
                        let mut status = status.write().await;
                        status.settings = current.clone();
                        status.sources = vec![SourceStatus {
                            kind: source.kind(),
                            health: poll.health,
                            last_poll_at: Some(now),
                            event_count: list.len(),
                        }];
                        status.next_event = next_teams_event(&list, now);
                        status.clone()
                    };
                    if let Err(error) = app_handle.emit(STATUS_EVENT, snapshot) {
                        error!("Failed to emit calendar status: {error}");
                    }
                }

                let due = {
                    let list = events.read().await;
                    planner.due(&list, now, Duration::minutes(current.nudge_lead_minutes as i64))
                };
                for event in due {
                    let starts_in_seconds = (event.start - now).num_seconds();
                    info!("Calendar nudge: a Teams meeting starts in {}s", starts_in_seconds);
                    show_nudge_notification(&app_handle, &event, starts_in_seconds).await;
                    let payload = CalendarMeetingUpcomingPayload {
                        event_type: UPCOMING_EVENT.to_string(),
                        event,
                        lead_minutes: current.nudge_lead_minutes,
                        starts_in_seconds,
                    };
                    if let Err(error) = app_handle.emit(UPCOMING_EVENT, payload) {
                        error!("Failed to emit calendar nudge: {error}");
                    }
                }
            }
        });
    }

    pub async fn stop(&self) {
        if let Some(token) = self.cancellation.write().await.take() {
            token.cancel();
        }
        self.status.write().await.running = false;
    }

    pub async fn get_settings(&self) -> CalendarSettings {
        self.settings.read().await.clone()
    }

    pub async fn update_settings(&self, mut next: CalendarSettings) -> Result<()> {
        next.nudge_lead_minutes = next.nudge_lead_minutes.min(MAX_NUDGE_LEAD_MINUTES);
        let was_enabled = self.settings.read().await.calendar_enabled;
        self.persist_settings(next.clone()).await?;

        if next.calendar_enabled {
            self.start().await;
            if !was_enabled {
                self.refresh.notify_one();
            }
        } else {
            self.stop().await;
        }
        Ok(())
    }

    pub fn refresh_now(&self) {
        self.refresh.notify_one();
    }

    pub async fn get_status(&self) -> CalendarStatus {
        self.status.read().await.clone()
    }

    /// Events that have not ended, soonest first.
    pub async fn upcoming_events(&self) -> Vec<CalendarEvent> {
        let now = Utc::now();
        self.events
            .read()
            .await
            .iter()
            .filter(|event| event.end > now)
            .cloned()
            .collect()
    }

    async fn persist_settings(&self, next: CalendarSettings) -> Result<()> {
        save_settings(&next).await?;
        *self.settings.write().await = next.clone();
        self.status.write().await.settings = next;
        Ok(())
    }
}

fn next_teams_event(events: &[CalendarEvent], now: DateTime<Utc>) -> Option<CalendarEvent> {
    events
        .iter()
        .find(|event| event.is_teams_meeting() && event.end > now)
        .cloned()
}

async fn show_nudge_notification<R: Runtime>(app_handle: &AppHandle<R>, event: &CalendarEvent, starts_in_seconds: i64) {
    let Some(state) = app_handle.try_state::<NotificationManagerState<R>>() else {
        return;
    };
    let guard = state.read().await;
    let Some(manager) = guard.as_ref() else {
        return;
    };
    let minutes = ((starts_in_seconds.max(0) + 59) / 60) as u64;
    let notification = Notification::meeting_reminder(minutes, Some(event.title.clone()));
    if let Err(error) = manager.show_notification(notification).await {
        warn!("Calendar nudge notification failed: {error}");
    }
}

fn settings_path() -> PathBuf {
    crate::storage::config_dir().join("calendar.json")
}

async fn load_settings() -> Result<CalendarSettings> {
    let path = settings_path();
    if !path.exists() {
        let defaults = CalendarSettings::default();
        save_settings(&defaults).await?;
        return Ok(defaults);
    }
    let content = tokio::fs::read_to_string(path).await?;
    Ok(serde_json::from_str(&content)?)
}

async fn save_settings(settings: &CalendarSettings) -> Result<()> {
    let path = settings_path();
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(path, serde_json::to_string_pretty(settings)?).await?;
    Ok(())
}
