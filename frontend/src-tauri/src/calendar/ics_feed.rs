//! Published ICS feed source. Outlook on the web and the new Outlook app
//! publish a calendar as a private `.ics` link (Settings, Calendar, Shared
//! calendars). The URL is fetched on a timer with conditional headers; the
//! body is parsed into events with recurring series expanded.
//!
//! Outlook writes `TZID` parameters with Windows time zone names such as
//! `Eastern Standard Time`, so zone lookup tries IANA first and then the
//! Windows name table before falling back to the machine's local zone.

use super::{
    source::{CalendarSource, SourcePoll},
    teams_link::extract_teams_join_url,
    types::{CalendarEvent, SourceHealth, SourceKind},
};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, TimeZone, Utc};
use icalendar::parser::{read_calendar, unfold, Component, Property};
use log::{debug, warn};
use reqwest::{
    header::{ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED},
    StatusCode,
};
use rrule::{RRule, RRuleSet, Tz, Unvalidated};
use std::collections::HashMap;
use tokio::sync::{Mutex, RwLock};

const FETCH_TIMEOUT_SECONDS: u64 = 20;
/// Upper bound on expanded occurrences per series inside the window.
const MAX_OCCURRENCES: u16 = 1000;

pub struct IcsFeedSource {
    client: reqwest::Client,
    url: RwLock<Option<String>>,
    cache: Mutex<FeedCache>,
}

#[derive(Default)]
struct FeedCache {
    etag: Option<String>,
    last_modified: Option<String>,
    text: Option<String>,
}

impl IcsFeedSource {
    pub fn new(url: Option<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(FETCH_TIMEOUT_SECONDS))
            .user_agent(concat!("Meetily/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default();

        Self {
            client,
            url: RwLock::new(url),
            cache: Mutex::new(FeedCache::default()),
        }
    }

    pub async fn set_url(&self, url: Option<String>) {
        *self.url.write().await = url;
        *self.cache.lock().await = FeedCache::default();
    }

    pub async fn is_configured(&self) -> bool {
        self.url.read().await.is_some()
    }

    async fn fetch_text(&self, url: &str) -> std::result::Result<String, SourceHealth> {
        let mut cache = self.cache.lock().await;

        let mut request = self.client.get(url);
        if let Some(etag) = &cache.etag {
            request = request.header(IF_NONE_MATCH, etag);
        }
        if let Some(last_modified) = &cache.last_modified {
            request = request.header(IF_MODIFIED_SINCE, last_modified);
        }

        let response = request.send().await.map_err(|error| SourceHealth::Error {
            reason: format!("Could not reach the calendar link: {error}"),
        })?;

        let status = response.status();
        if status == StatusCode::NOT_MODIFIED {
            return cache.text.clone().ok_or_else(|| SourceHealth::Error {
                reason: "Calendar link answered 304 before any body was cached".to_string(),
            });
        }

        if matches!(
            status,
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND | StatusCode::GONE
        ) {
            return Err(SourceHealth::Unavailable {
                reason: format!("The calendar link is no longer valid (HTTP {})", status.as_u16()),
            });
        }

        if !status.is_success() {
            return Err(SourceHealth::Error {
                reason: format!("Calendar link answered HTTP {}", status.as_u16()),
            });
        }

        let header_string = |name| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
        };
        let etag = header_string(ETAG);
        let last_modified = header_string(LAST_MODIFIED);

        let text = response.text().await.map_err(|error| SourceHealth::Error {
            reason: format!("Could not read the calendar body: {error}"),
        })?;

        cache.etag = etag;
        cache.last_modified = last_modified;
        cache.text = Some(text.clone());
        Ok(text)
    }
}

#[async_trait]
impl CalendarSource for IcsFeedSource {
    fn kind(&self) -> SourceKind {
        SourceKind::IcsFeed
    }

    async fn poll(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> SourcePoll {
        let Some(url) = self.url.read().await.clone() else {
            return SourcePoll {
                health: SourceHealth::NotConfigured,
                events: Some(vec![]),
            };
        };

        let text = match self.fetch_text(&url).await {
            Ok(text) => text,
            Err(health) => {
                return SourcePoll {
                    health,
                    events: None,
                }
            }
        };

        match parse_ics(&text, from, to) {
            Ok(events) => {
                debug!("ICS feed parsed: {} events in window", events.len());
                SourcePoll {
                    health: SourceHealth::Ok,
                    events: Some(events),
                }
            }
            Err(error) => SourcePoll {
                health: SourceHealth::Error {
                    reason: format!("Could not parse the calendar: {error}"),
                },
                events: None,
            },
        }
    }
}

/// A DTSTART-like value with the zone it was written in. `zoned` keeps the
/// original zone so recurrence expansion follows daylight-saving changes.
#[derive(Debug, Clone, Copy)]
struct ParsedDateTime {
    zoned: DateTime<Tz>,
    all_day: bool,
}

impl ParsedDateTime {
    fn utc(&self) -> DateTime<Utc> {
        self.zoned.with_timezone(&Utc)
    }
}

/// Events from `text` that overlap `[from, to)`, recurring series expanded,
/// sorted by start. Series without a usable DTSTART are skipped with a log
/// line rather than failing the whole feed.
pub fn parse_ics(text: &str, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Vec<CalendarEvent>> {
    let unfolded = unfold(text);
    let calendar = read_calendar(&unfolded).map_err(|error| anyhow!("{error}"))?;

    struct Series<'a> {
        master: Option<&'a Component<'a>>,
        overrides: Vec<&'a Component<'a>>,
    }

    let mut by_uid: HashMap<String, Series<'_>> = HashMap::new();
    let mut anonymous = 0usize;
    for component in &calendar.components {
        if !component.name.as_str().eq_ignore_ascii_case("VEVENT") {
            continue;
        }
        let uid = match prop_text(component, "UID") {
            Some(uid) if !uid.is_empty() => uid,
            _ => {
                anonymous += 1;
                format!("no-uid-{anonymous}")
            }
        };
        let series = by_uid.entry(uid).or_insert(Series {
            master: None,
            overrides: vec![],
        });
        if component.find_prop("RECURRENCE-ID").is_some() {
            series.overrides.push(component);
        } else {
            series.master = Some(component);
        }
    }

    let overlaps = |event: &CalendarEvent| event.end > from && event.start < to;
    let mut events = Vec::new();

    for (uid, series) in by_uid {
        let mut overridden: Vec<DateTime<Utc>> = Vec::new();

        for component in &series.overrides {
            let Some(recurrence_id) = component.find_prop("RECURRENCE-ID").and_then(parse_datetime_prop) else {
                warn!("ICS event {uid}: RECURRENCE-ID not parseable, skipping override");
                continue;
            };
            overridden.push(recurrence_id.utc());

            let Some(start) = component.find_prop("DTSTART").and_then(parse_datetime_prop) else {
                continue;
            };
            let end = resolve_end(component, &start);
            let id = format!("{uid}@{}", recurrence_id.utc().to_rfc3339());
            let event = build_event(component, id, start.utc(), end, true);
            if overlaps(&event) {
                events.push(event);
            }
        }

        let Some(master) = series.master else { continue };
        let Some(start) = master.find_prop("DTSTART").and_then(parse_datetime_prop) else {
            warn!("ICS event {uid}: DTSTART missing or not parseable, skipping");
            continue;
        };
        let end = resolve_end(master, &start);
        let duration = end - start.utc();

        let occurrences = match master.find_prop("RRULE") {
            Some(rrule) => match expand_series(master, &start, rrule, from, to) {
                Ok(occurrences) => Some(occurrences),
                Err(error) => {
                    warn!("ICS event {uid}: recurrence not expanded ({error}); using first occurrence only");
                    None
                }
            },
            None => None,
        };

        match occurrences {
            Some(occurrences) => {
                for occurrence in occurrences {
                    if overridden.contains(&occurrence) {
                        continue;
                    }
                    let id = format!("{uid}@{}", occurrence.to_rfc3339());
                    let event = build_event(master, id, occurrence, occurrence + duration, true);
                    if overlaps(&event) {
                        events.push(event);
                    }
                }
            }
            None => {
                let event = build_event(master, uid.clone(), start.utc(), end, false);
                if overlaps(&event) {
                    events.push(event);
                }
            }
        }
    }

    events.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| a.id.cmp(&b.id)));
    Ok(events)
}

fn expand_series(
    master: &Component<'_>,
    start: &ParsedDateTime,
    rrule: &Property<'_>,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<DateTime<Utc>>> {
    let rule: RRule<Unvalidated> = rrule
        .val
        .as_str()
        .parse()
        .map_err(|error| anyhow!("RRULE parse: {error}"))?;
    let rule = rule
        .validate(start.zoned)
        .map_err(|error| anyhow!("RRULE validate: {error}"))?;

    let mut set = RRuleSet::new(start.zoned).rrule(rule);
    for property in props(master, "EXDATE") {
        for value in split_datetime_list(property) {
            set = set.exdate(value.zoned);
        }
    }
    for property in props(master, "RDATE") {
        for value in split_datetime_list(property) {
            set = set.rdate(value.zoned);
        }
    }

    // `after` filters on occurrence start, so reach back a day to keep long
    // events that began before the window; the caller filters on end.
    let after = Tz::UTC.from_utc_datetime(&(from - Duration::hours(24)).naive_utc());
    let before = Tz::UTC.from_utc_datetime(&to.naive_utc());
    let result = set.after(after).before(before).all(MAX_OCCURRENCES);

    Ok(result
        .dates
        .into_iter()
        .map(|date| date.with_timezone(&Utc))
        .collect())
}

fn build_event(
    component: &Component<'_>,
    id: String,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    is_recurring_instance: bool,
) -> CalendarEvent {
    let title = prop_text(component, "SUMMARY")
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| "(No title)".to_string());
    let location = prop_text(component, "LOCATION").filter(|value| !value.trim().is_empty());
    let body = prop_text(component, "DESCRIPTION")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let html_body = prop_text(component, "X-ALT-DESC");

    let teams_join_url = prop_text(component, "X-MICROSOFT-SKYPETEAMSMEETINGURL")
        .as_deref()
        .and_then(extract_teams_join_url)
        .or_else(|| location.as_deref().and_then(extract_teams_join_url))
        .or_else(|| body.as_deref().and_then(extract_teams_join_url))
        .or_else(|| html_body.as_deref().and_then(extract_teams_join_url));

    let organizer = component.find_prop("ORGANIZER").map(person_name);
    let attendees = props(component, "ATTENDEE").map(person_name).collect();

    CalendarEvent {
        id,
        source: SourceKind::IcsFeed,
        title,
        start,
        end,
        organizer,
        attendees,
        location,
        body,
        teams_join_url,
        is_recurring_instance,
    }
}

/// Display name from CN, else the address without `mailto:`.
fn person_name(property: &Property<'_>) -> String {
    if let Some(cn) = param(property, "CN") {
        let cn = cn.trim().trim_matches('"');
        if !cn.is_empty() {
            return cn.to_string();
        }
    }
    let value = property.val.as_str().trim();
    let lower = value.to_ascii_lowercase();
    match lower.strip_prefix("mailto:") {
        Some(_) => value[7..].to_string(),
        None => value.to_string(),
    }
}

fn resolve_end(component: &Component<'_>, start: &ParsedDateTime) -> DateTime<Utc> {
    if let Some(end) = component.find_prop("DTEND").and_then(parse_datetime_prop) {
        if end.utc() > start.utc() {
            return end.utc();
        }
    }
    if let Some(duration) = component
        .find_prop("DURATION")
        .and_then(|property| parse_duration(property.val.as_str()))
    {
        if duration > Duration::zero() {
            return start.utc() + duration;
        }
    }
    if start.all_day {
        start.utc() + Duration::days(1)
    } else {
        start.utc() + Duration::hours(1)
    }
}

fn props<'a, 'b>(component: &'a Component<'b>, name: &'a str) -> impl Iterator<Item = &'a Property<'b>> + 'a {
    component
        .properties
        .iter()
        .filter(move |property| property.name.as_str().eq_ignore_ascii_case(name))
}

fn prop_text(component: &Component<'_>, name: &str) -> Option<String> {
    component
        .find_prop(name)
        .map(|property| unescape_text(property.val.as_str()))
}

fn param<'a>(property: &'a Property<'_>, key: &str) -> Option<&'a str> {
    property
        .params
        .iter()
        .find(|parameter| parameter.key.as_str().eq_ignore_ascii_case(key))
        .and_then(|parameter| parameter.val.as_ref().map(|value| value.as_str()))
}

/// RFC 5545 TEXT unescaping: `\n` newline, `\,` `\;` `\\` literal.
fn unescape_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') | Some('N') => out.push('\n'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

fn split_datetime_list(property: &Property<'_>) -> Vec<ParsedDateTime> {
    let tzid = param(property, "TZID");
    let is_date = param(property, "VALUE").map(|v| v.eq_ignore_ascii_case("DATE")).unwrap_or(false);
    property
        .val
        .as_str()
        .split(',')
        .filter_map(|value| parse_datetime_value(value.trim(), tzid, is_date))
        .collect()
}

fn parse_datetime_prop(property: &Property<'_>) -> Option<ParsedDateTime> {
    let tzid = param(property, "TZID");
    let is_date = param(property, "VALUE").map(|v| v.eq_ignore_ascii_case("DATE")).unwrap_or(false);
    parse_datetime_value(property.val.as_str().trim(), tzid, is_date)
}

fn parse_datetime_value(value: &str, tzid: Option<&str>, is_date: bool) -> Option<ParsedDateTime> {
    if is_date || (value.len() == 8 && value.chars().all(|c| c.is_ascii_digit())) {
        let date = NaiveDate::parse_from_str(value, "%Y%m%d").ok()?;
        let local = date.and_hms_opt(0, 0, 0)?;
        let zoned = Tz::LOCAL.from_local_datetime(&local).earliest()?;
        return Some(ParsedDateTime { zoned, all_day: true });
    }

    if let Some(stripped) = value.strip_suffix(['Z', 'z']) {
        let naive = NaiveDateTime::parse_from_str(stripped, "%Y%m%dT%H%M%S").ok()?;
        return Some(ParsedDateTime {
            zoned: Tz::UTC.from_utc_datetime(&naive),
            all_day: false,
        });
    }

    let naive = NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S").ok()?;
    let tz = resolve_tz(tzid);
    // Inside a daylight-saving gap `earliest()` is None; shifting forward by
    // an hour is what Outlook itself does for such events.
    let zoned = tz
        .from_local_datetime(&naive)
        .earliest()
        .or_else(|| tz.from_local_datetime(&(naive + Duration::hours(1))).earliest())?;
    Some(ParsedDateTime { zoned, all_day: false })
}

/// IANA id, then a Windows display name (what Outlook writes), then any
/// `Area/City` tail of a vendor-prefixed id. Unknown zones become local time,
/// which is right for the common case of a feed published from this machine.
fn resolve_tz(tzid: Option<&str>) -> Tz {
    let Some(raw) = tzid else { return Tz::LOCAL };
    let id = raw.trim().trim_matches('"');
    if id.is_empty() {
        return Tz::LOCAL;
    }
    if let Ok(tz) = id.parse::<chrono_tz::Tz>() {
        return Tz::Tz(tz);
    }
    if let Ok(windows) = id.parse::<windows_timezones::WindowsTimezone>() {
        return Tz::Tz(windows.into());
    }
    let mut rest = id;
    while let Some(slash) = rest.find('/') {
        rest = &rest[slash + 1..];
        if let Ok(tz) = rest.parse::<chrono_tz::Tz>() {
            return Tz::Tz(tz);
        }
    }
    warn!("ICS time zone '{id}' not recognised; treating as local time");
    Tz::LOCAL
}

/// `P1D`, `PT1H30M`, `-PT15M`, `P1W`. Fractions and months are not used by
/// calendar clients and are rejected.
fn parse_duration(value: &str) -> Option<Duration> {
    let value = value.trim();
    let (negative, rest) = match value.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, value.strip_prefix('+').unwrap_or(value)),
    };
    let rest = rest.strip_prefix('P')?;
    let mut total = Duration::zero();
    let mut number = String::new();
    let mut in_time = false;
    for c in rest.chars() {
        match c {
            '0'..='9' => number.push(c),
            'T' => in_time = true,
            'W' | 'D' | 'H' | 'M' | 'S' => {
                let n: i64 = number.parse().ok()?;
                number.clear();
                total += match (c, in_time) {
                        ('W', _) => Duration::weeks(n),
                        ('D', _) => Duration::days(n),
                        ('H', true) => Duration::hours(n),
                        ('M', true) => Duration::minutes(n),
                        ('S', true) => Duration::seconds(n),
                        _ => return None,
                };
            }
            _ => return None,
        }
    }
    if !number.is_empty() {
        return None;
    }
    Some(if negative { -total } else { total })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    const FEED: &str = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
PRODID:-//Microsoft Corporation//Outlook 16.0 MIMEDIR//EN\r\n\
BEGIN:VEVENT\r\n\
UID:single@example\r\n\
DTSTART:20261103T150000Z\r\n\
DTEND:20261103T153000Z\r\n\
SUMMARY:Design review\\, part 2\r\n\
LOCATION:Microsoft Teams Meeting\r\n\
ORGANIZER;CN=Ana Ortiz:mailto:ana@example.com\r\n\
ATTENDEE;CN=Ben Lee;ROLE=REQ-PARTICIPANT:mailto:ben@example.com\r\n\
ATTENDEE;ROLE=OPT-PARTICIPANT:mailto:cy@example.com\r\n\
DESCRIPTION:Agenda\\nJoin: https://teams.microsoft.com/l/meetup-join/19%3am\r\n\
\x20eeting_x%40thread.v2/0?context=%7b%22Tid%22%3a%22t%22%7d\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:weekly@example\r\n\
DTSTART;TZID=Eastern Standard Time:20261102T090000\r\n\
DTEND;TZID=Eastern Standard Time:20261102T093000\r\n\
RRULE:FREQ=WEEKLY;BYDAY=MO,WE;WKST=SU\r\n\
EXDATE;TZID=Eastern Standard Time:20261104T090000\r\n\
SUMMARY:Standup\r\n\
X-MICROSOFT-SKYPETEAMSMEETINGURL:https://teams.microsoft.com/l/meetup-join/19%3astandup%40thread.v2/0\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:weekly@example\r\n\
RECURRENCE-ID;TZID=Eastern Standard Time:20261109T090000\r\n\
DTSTART;TZID=Eastern Standard Time:20261109T100000\r\n\
DTEND;TZID=Eastern Standard Time:20261109T103000\r\n\
SUMMARY:Standup (moved)\r\n\
LOCATION:Microsoft Teams Meeting\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:allday@example\r\n\
DTSTART;VALUE=DATE:20261105\r\n\
DTEND;VALUE=DATE:20261106\r\n\
SUMMARY:Office closed\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";

    #[test]
    fn single_event_fields_and_link() {
        let events = parse_ics(FEED, utc(2026, 11, 3, 0, 0), utc(2026, 11, 4, 0, 0)).unwrap();
        let event = events.iter().find(|e| e.id == "single@example").expect("single event");
        assert_eq!(event.title, "Design review, part 2");
        assert_eq!(event.start, utc(2026, 11, 3, 15, 0));
        assert_eq!(event.end, utc(2026, 11, 3, 15, 30));
        assert_eq!(event.organizer.as_deref(), Some("Ana Ortiz"));
        assert_eq!(event.attendees, vec!["Ben Lee".to_string(), "cy@example.com".to_string()]);
        assert_eq!(event.body.as_deref(), Some("Agenda\nJoin: https://teams.microsoft.com/l/meetup-join/19%3ameeting_x%40thread.v2/0?context=%7b%22Tid%22%3a%22t%22%7d"));
        assert_eq!(
            event.teams_join_url.as_deref(),
            Some("https://teams.microsoft.com/l/meetup-join/19%3ameeting_x%40thread.v2/0?context=%7b%22Tid%22%3a%22t%22%7d")
        );
        assert!(event.is_teams_meeting());
        assert!(!event.is_recurring_instance);
    }

    #[test]
    fn weekly_series_expands_with_windows_tzid_exdate_and_override() {
        // 2026-11-02 is a Monday. US Eastern is UTC-5 from 2026-11-01.
        let events = parse_ics(FEED, utc(2026, 11, 2, 0, 0), utc(2026, 11, 12, 0, 0)).unwrap();
        let standups: Vec<&CalendarEvent> = events.iter().filter(|e| e.id.starts_with("weekly@example")).collect();
        let starts: Vec<DateTime<Utc>> = standups.iter().map(|e| e.start).collect();

        assert_eq!(
            starts,
            vec![
                utc(2026, 11, 2, 14, 0),  // Mon
                // Wed 11-04 removed by EXDATE
                utc(2026, 11, 9, 15, 0),  // Mon, moved to 10:00 by the override
                utc(2026, 11, 11, 14, 0), // Wed
            ]
        );
        let moved = standups.iter().find(|e| e.start == utc(2026, 11, 9, 15, 0)).unwrap();
        assert_eq!(moved.title, "Standup (moved)");
        assert_eq!(moved.id, "weekly@example@2026-11-09T14:00:00+00:00");
        assert!(standups.iter().all(|e| e.is_recurring_instance));
        let regular = standups.iter().find(|e| e.start == utc(2026, 11, 2, 14, 0)).unwrap();
        assert_eq!(
            regular.teams_join_url.as_deref(),
            Some("https://teams.microsoft.com/l/meetup-join/19%3astandup%40thread.v2/0")
        );
    }

    #[test]
    fn window_excludes_events_outside_it() {
        let events = parse_ics(FEED, utc(2026, 12, 2, 0, 0), utc(2026, 12, 3, 0, 0)).unwrap(); // a Wednesday
        assert!(events.iter().all(|e| e.id.starts_with("weekly@example")));
        assert!(!events.is_empty());
    }

    #[test]
    fn all_day_event_is_not_a_teams_meeting() {
        let events = parse_ics(FEED, utc(2026, 11, 4, 0, 0), utc(2026, 11, 7, 0, 0)).unwrap();
        let closed = events.iter().find(|e| e.id == "allday@example").expect("all-day event");
        assert!(!closed.is_teams_meeting());
        assert_eq!(closed.end - closed.start, Duration::days(1));
    }

    #[test]
    fn missing_dtend_uses_duration_then_default_hour() {
        let feed = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:a\r\nDTSTART:20261103T100000Z\r\nDURATION:PT45M\r\nSUMMARY:A\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nUID:b\r\nDTSTART:20261103T120000Z\r\nSUMMARY:B\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let events = parse_ics(feed, utc(2026, 11, 3, 0, 0), utc(2026, 11, 4, 0, 0)).unwrap();
        assert_eq!(events[0].end - events[0].start, Duration::minutes(45));
        assert_eq!(events[1].end - events[1].start, Duration::hours(1));
    }

    #[test]
    fn bad_rrule_falls_back_to_first_occurrence() {
        let feed = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:a\r\nDTSTART:20261103T100000Z\r\nDTEND:20261103T103000Z\r\nRRULE:FREQ=NEVER\r\nSUMMARY:A\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let events = parse_ics(feed, utc(2026, 11, 3, 0, 0), utc(2026, 11, 4, 0, 0)).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, "a");
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        assert!(parse_ics("not a calendar", utc(2026, 1, 1, 0, 0), utc(2026, 1, 2, 0, 0)).is_err());
    }

    #[test]
    fn time_zone_resolution() {
        assert_eq!(resolve_tz(Some("Europe/Madrid")).name(), "Europe/Madrid");
        assert_eq!(resolve_tz(Some("W. Europe Standard Time")).name(), "Europe/Berlin");
        assert_eq!(resolve_tz(Some("/freeassociation.sourceforge.net/America/New_York")).name(), "America/New_York");
        assert!(resolve_tz(Some("Customized Time Zone")).is_local());
        assert!(resolve_tz(None).is_local());
    }

    #[test]
    fn duration_parsing() {
        assert_eq!(parse_duration("PT1H30M"), Some(Duration::minutes(90)));
        assert_eq!(parse_duration("P1D"), Some(Duration::days(1)));
        assert_eq!(parse_duration("-PT15M"), Some(Duration::minutes(-15)));
        assert_eq!(parse_duration("P1W"), Some(Duration::weeks(1)));
        assert_eq!(parse_duration("P1M"), None);
        assert_eq!(parse_duration("1H"), None);
    }

    #[tokio::test]
    async fn feed_source_uses_conditional_requests_and_keeps_cached_body_on_304() {
        use wiremock::{matchers::{header, method, path}, Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/calendar.ics"))
            .and(header("if-none-match", "\"v1\""))
            .respond_with(ResponseTemplate::new(304))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/calendar.ics"))
            .respond_with(ResponseTemplate::new(200).insert_header("etag", "\"v1\"").set_body_string(FEED))
            .mount(&server)
            .await;

        let source = IcsFeedSource::new(Some(format!("{}/calendar.ics", server.uri())));
        let from = utc(2026, 11, 3, 0, 0);
        let to = utc(2026, 11, 4, 0, 0);

        let first = source.poll(from, to).await;
        assert_eq!(first.health, SourceHealth::Ok);
        let first_events = first.events.expect("events on 200");
        assert!(first_events.iter().any(|e| e.id == "single@example"));

        let second = source.poll(from, to).await;
        assert_eq!(second.health, SourceHealth::Ok);
        assert_eq!(second.events, Some(first_events));

        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 2);
        assert!(received[0].headers.get("if-none-match").is_none());
    }

    #[tokio::test]
    async fn revoked_link_is_unavailable_and_keeps_no_events() {
        use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET")).respond_with(ResponseTemplate::new(404)).mount(&server).await;

        let source = IcsFeedSource::new(Some(format!("{}/gone.ics", server.uri())));
        let poll = source.poll(utc(2026, 11, 3, 0, 0), utc(2026, 11, 4, 0, 0)).await;
        assert!(matches!(poll.health, SourceHealth::Unavailable { .. }));
        assert_eq!(poll.events, None);
    }

    #[tokio::test]
    async fn unconfigured_source_reports_not_configured_with_empty_list() {
        let source = IcsFeedSource::new(None);
        let poll = source.poll(utc(2026, 11, 3, 0, 0), utc(2026, 11, 4, 0, 0)).await;
        assert_eq!(poll.health, SourceHealth::NotConfigured);
        assert_eq!(poll.events, Some(vec![]));
    }
}

