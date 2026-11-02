use super::{
    ics_feed::IcsFeedSource,
    scheduler::NudgePlanner,
    secret,
    source::CalendarSource,
    types::{
        CalendarEvent, CalendarMeetingUpcomingPayload, CalendarSettings, CalendarStatus, SourceKind,
        SourceStatus, ICS_POLL_INTERVAL_SECONDS, LOOKAHEAD_HOURS, LOOKBACK_HOURS, MAX_NUDGE_LEAD_MINUTES,
        STATUS_EVENT, TICK_SECONDS, UPCOMING_EVENT,
    },
};
use crate::notifications::{commands::NotificationManagerState, types::Notification};
use anyhow::{Context, Result};
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
    feed: Arc<IcsFeedSource>,
    /// Woken by a URL change or a manual refresh to poll before the interval.
    refresh: Arc<Notify>,
    cancellation: Arc<RwLock<Option<CancellationToken>>>,
}

impl<R: Runtime> CalendarManager<R> {
    pub async fn new(app_handle: AppHandle<R>) -> Result<Self> {
        let mut settings = load_settings().await.unwrap_or_default();

        let feed_url = match tokio::task::spawn_blocking(secret::load_feed_url).await {
            Ok(Ok(url)) => url,
            Ok(Err(error)) => {
                warn!("Calendar feed URL could not be read from the credential store: {error}");
                None
            }
            Err(error) => {
                warn!("Calendar feed URL lookup task failed: {error}");
                None
            }
        };
        settings.ics_feed_configured = feed_url.is_some();

        Ok(Self {
            app_handle,
            status: Arc::new(RwLock::new(CalendarStatus::stopped(settings.clone()))),
            settings: Arc::new(RwLock::new(settings)),
            events: Arc::new(RwLock::new(Vec::new())),
            feed: Arc::new(IcsFeedSource::new(feed_url)),
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
        let feed = Arc::clone(&self.feed);
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
                    at.elapsed() >= std::time::Duration::from_secs(ICS_POLL_INTERVAL_SECONDS)
                });
                if poll_due {
                    last_poll = Some(Instant::now());
                    let from = now - Duration::hours(LOOKBACK_HOURS);
                    let to = now + Duration::hours(LOOKAHEAD_HOURS);
                    let poll = feed.poll(from, to).await;

                    if let Some(list) = poll.events {
                        *events.write().await = list;
                    }
                    let snapshot = {
                        let list = events.read().await;
                        let mut status = status.write().await;
                        status.settings = current.clone();
                        status.sources = vec![SourceStatus {
                            kind: SourceKind::IcsFeed,
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
                    info!("Calendar nudge: '{}' starts in {}s", event.title, starts_in_seconds);
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

    /// `ics_feed_configured` is owned by the keyring, so the caller's value is
    /// replaced with the real one.
    pub async fn update_settings(&self, mut next: CalendarSettings) -> Result<()> {
        next.nudge_lead_minutes = next.nudge_lead_minutes.min(MAX_NUDGE_LEAD_MINUTES);
        next.ics_feed_configured = self.feed.is_configured().await;
        self.persist_settings(next.clone()).await?;

        if next.calendar_enabled {
            self.start().await;
        } else {
            self.stop().await;
        }
        Ok(())
    }

    pub async fn set_feed_url(&self, raw_url: &str) -> Result<()> {
        let url = secret::normalize_feed_url(raw_url)?;
        let stored = url.clone();
        tokio::task::spawn_blocking(move || secret::store_feed_url(&stored))
            .await
            .context("credential store task")??;

        self.feed.set_url(Some(url)).await;
        let mut next = self.settings.read().await.clone();
        next.ics_feed_configured = true;
        self.persist_settings(next.clone()).await?;

        self.refresh.notify_one();
        if next.calendar_enabled {
            self.start().await;
        }
        Ok(())
    }

    pub async fn clear_feed_url(&self) -> Result<()> {
        tokio::task::spawn_blocking(secret::clear_feed_url)
            .await
            .context("credential store task")??;

        self.feed.set_url(None).await;
        self.events.write().await.clear();
        let mut next = self.settings.read().await.clone();
        next.ics_feed_configured = false;
        self.persist_settings(next).await?;

        let snapshot = {
            let mut status = self.status.write().await;
            status.sources.clear();
            status.next_event = None;
            status.clone()
        };
        if let Err(error) = self.app_handle.emit(STATUS_EVENT, snapshot) {
            error!("Failed to emit calendar status: {error}");
        }
        self.refresh.notify_one();
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
