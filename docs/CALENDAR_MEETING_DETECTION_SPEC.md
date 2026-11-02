# Outlook calendar and Teams meeting auto-detect (Windows)

Status: 2026-11-02. PR 1 (ICS feed source, nudge, settings UI) implemented;
PRs 2 to 4 pending.

## Goal

Stop users forgetting to press record. Meetily learns about upcoming Outlook
meetings without Microsoft Graph, nudges the user shortly before each Teams
meeting, and when the Teams call starts it offers a one-click "join and record"
prompt with the meeting title already filled in. Calendar details are stored on
the meeting record and can feed the summary.

Windows only. The macOS and Linux builds keep compiling but show no calendar UI
and run no calendar code.

## Decisions already made

| Question | Decision |
| --- | --- |
| Calendar access | No Microsoft Graph, no Entra app registration. |
| Sources | Published ICS link (new Outlook, Outlook on the web) and classic Outlook COM object model. Both behind one trait. |
| Trigger | Prompt, never auto-start. The prompt is prefilled with the matched event. |
| Which events | Teams events only: an event counts if its location or body contains a Teams join link, or if Teams audio is detected at its start time. |
| Metadata stored | Title, start, end, organizer, attendees, join link, body. |
| Body on COM | Not read. The Outlook Object Model Guard can raise a security dialog on body access. Body comes from the ICS feed only. |
| Body use | Always stored and shown in meeting details. A setting controls whether it is added to the summary prompt. Default on. |
| Nudge actions | "Join and record" opens the Teams link and arms recording to start when Teams audio is detected. "Record" starts now. "Dismiss". |
| Licensing | Available to everyone. No PRO gate. |

## What exists today

`frontend/src-tauri/src/meeting_detection/` polls every 15 seconds. It finds
`ms-teams.exe` by process name (`process_detector.rs`), checks whether any of
those PIDs has an active WASAPI render session (`windows_audio.rs`), scores the
sample (`confidence.rs`) and emits `teams-call-likely-started` and
`teams-call-likely-ended`. `TeamsMeetingDetectionProvider.tsx` turns those into
start and stop prompts. Settings live in a JSON file next to the app data and
are edited from `PreferenceSettings.tsx`.

This spec extends that module and adds a sibling `calendar` module. It does not
replace the detector.

## Architecture

```
calendar/
├── mod.rs
├── types.rs            CalendarEvent, CalendarSettings, CalendarStatus, event names
├── source.rs           trait CalendarSource
├── ics_feed.rs         ICS URL poller + parser (all platforms, used on Windows)
├── outlook_com.rs      classic Outlook COM reader (#[cfg(windows)])
├── teams_link.rs       Teams join-link extraction from text
├── scheduler.rs        upcoming-event nudges, event matching
└── commands.rs         Tauri commands + managed state

meeting_detection/
├── window_title.rs     NEW: titles of top-level windows owned by Teams PIDs (#[cfg(windows)])
└── manager.rs          CHANGED: attaches calendar match + window title to prompt payloads
```

### CalendarSource trait

```rust
pub struct SourcePoll {
    pub health: SourceHealth,            // NotConfigured | Ok | Unavailable{reason} | Error{reason}
    pub events: Option<Vec<CalendarEvent>>, // None: keep the previous list
}

pub trait CalendarSource: Send + Sync {
    fn kind(&self) -> SourceKind;        // IcsFeed | OutlookCom
    async fn poll(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> SourcePoll;
}
```

`CalendarEvent` fields: `id` (source id, stable across polls), `source`,
`title`, `start`, `end`, `organizer: Option<String>`, `attendees: Vec<String>`,
`location: Option<String>`, `body: Option<String>`, `teams_join_url:
Option<String>`, `is_recurring_instance`.

The scheduler merges events from every enabled source. Two events are the same
meeting when their start times are within two minutes and their titles match
case-insensitively. The ICS copy wins for body; the COM copy wins for attendee
names when the feed only carries addresses.

### ICS feed source

- User pastes the URL from Outlook on the web: Settings, Calendar, Shared
  calendars, Publish a calendar, "Can view all details", ICS link. The "all
  details" variant is required for body and attendees; the spec UI says so.
- URL is stored in Windows Credential Manager via the `keyring` crate, never in
  the settings JSON. Settings JSON keeps only `ics_feed_configured: bool`.
- Poll interval 5 minutes, with jitter, `reqwest` with a 20 second timeout.
  `If-None-Match` and `If-Modified-Since` are sent when the server returned
  them. Parsing uses the `icalendar` crate; recurrences are expanded with the
  `rrule` crate over a window of now minus 1 hour to now plus 24 hours.
  `EXDATE` and overriding instances (`RECURRENCE-ID`) are honoured.
- Window: one hour back to 48 hours ahead. Events outside it are not kept in
  memory.
- Failure handling: HTTP 401 or 404 marks the source `Unavailable("link no
  longer valid")` and surfaces that in settings. Network errors keep the last
  good event list and mark the source `Error`. Nothing is retried faster than
  the poll interval.

### Classic Outlook COM source

- `#[cfg(windows)]`. Late-bound `IDispatch` calls through the `windows` crate
  (`Win32_System_Com`, `Win32_System_Ole`, `Win32_System_Variant` features;
  the first two are new).
- `Outlook.Application` is obtained with `GetActiveObject` first, so a running
  Outlook is reused, then `CoCreateInstance` as a fallback. If neither works,
  or `Outlook.Application` is not registered, the source reports
  `Unavailable("classic Outlook not installed")` and the settings toggle is
  shown disabled with that reason.
- Reads `Namespace("MAPI").GetDefaultFolder(9).Items` with `IncludeRecurrences
  = true`, `Sort("[Start]")`, then `Restrict` on `[Start]` and `[End]` for the
  lookahead window. Properties read per item: `EntryID`, `Subject`, `Start`,
  `End`, `Location`, `Organizer`, `RequiredAttendees`, `OptionalAttendees`
  (display names, semicolon separated). `Body` is never read.
- Teams join URL is taken from `Location`. Classic Outlook puts the Teams link
  in the body, so many COM events will have no URL. Those still count as Teams
  events if the location text says "Microsoft Teams Meeting".
- Runs on a dedicated thread with `CoInitializeEx(COINIT_APARTMENTTHREADED)`,
  because the Outlook object model is single-threaded apartment. Calls are
  sent to that thread over a channel; the async side awaits a oneshot reply.
  Poll interval 2 minutes.
- Every `IDispatch` and `BSTR` is released before the thread goes idle, or
  Outlook refuses to exit while Meetily is running.

### Teams join-link extraction

`teams_link.rs` finds the first `https://teams.microsoft.com/l/meetup-join/...`
or `https://teams.live.com/meet/...` URL in a text. It handles the HTML-escaped
and the `&amp;`-encoded forms that appear in ICS bodies. Pure function, unit
tested.

### Window title capture

`meeting_detection/window_title.rs`, `#[cfg(windows)]`. `EnumWindows` filtered
by `GetWindowThreadProcessId` against the Teams PIDs the process detector
already has, keeping visible windows with a non-empty title. The meeting
window title is the meeting subject followed by `| Microsoft Teams`. Returns
the first title that is not the main Teams window ("Microsoft Teams" alone,
"Chat | Microsoft Teams", "Calendar | Microsoft Teams"). The strip rules are a
pure function with tests. Needs the `Win32_UI_WindowsAndMessaging` feature.

### Scheduler and matching

- Nudge lead time is a setting, default 2 minutes, range 0 to 15.
- For each upcoming Teams event the scheduler emits `calendar-meeting-upcoming`
  once, at start minus lead time, and shows a system notification through
  `notifications::NotificationManager` with the event title. Events already
  running when Meetily starts are not nudged, but still match.
- Matching: when the detector produces a start prompt, the scheduler returns
  the Teams event whose start is within minus 10 to plus 20 minutes of now. If
  several match, the one with the closest start wins. The payload carries the
  match and the window title.
- Prompt title priority: calendar match title, then window title, then
  "Teams meeting".
- Armed recording: after "Join and record" the frontend opens the join URL via
  `tauri-plugin-opener` and sets an armed flag with a 10 minute expiry. The
  next `teams-call-likely-started` while armed starts recording without a
  second prompt. The detector itself still only emits prompts; arming is
  frontend state.
- A Teams call that starts with no calendar match behaves as today, with the
  window title added to the prompt.

### Events to the frontend

| Event | Payload | Emitted by |
| --- | --- | --- |
| `calendar-meeting-upcoming` | `CalendarEvent`, `lead_minutes` | scheduler |
| `teams-call-likely-started` | existing fields + `calendar_event: Option<CalendarEvent>`, `window_title: Option<String>` | detection manager |
| `calendar-status-changed` | `CalendarStatus` (per-source health, last poll, next event) | scheduler |

### Tauri commands

`get_calendar_settings`, `set_calendar_settings`, `set_calendar_feed_url`
(writes keyring, triggers immediate poll), `clear_calendar_feed_url`,
`get_calendar_status`, `get_upcoming_calendar_events` (for a small "next
meetings" list in the UI), `refresh_calendar_now`.

### Settings

```rust
pub struct CalendarSettings {
    pub calendar_enabled: bool,            // default false
    pub ics_feed_configured: bool,         // derived, read-only in UI
    pub outlook_com_enabled: bool,         // default false; disabled when COM source Unavailable
    pub nudge_lead_minutes: u64,           // default 2
    pub include_body_in_summary: bool,     // default true
}
```

Stored in `calendar.json` beside `meeting_detection.json` in the config
directory, same load and save pattern. The UI is `OutlookCalendarSettings.tsx`,
mounted from `PreferenceSettings.tsx` under the meeting detection block and
rendered only when the platform is Windows. The in-app nudge toast is
`CalendarNudgeProvider.tsx`; its "Record now" button reuses the sidebar's
auto-start path and passes the title through `autoStartMeetingName` in
session storage.

## Database

New sqlx migration `20261102000000_add_meeting_calendar.sql`:

```sql
ALTER TABLE meetings ADD COLUMN calendar_source TEXT;        -- 'ics_feed' | 'outlook_com'
ALTER TABLE meetings ADD COLUMN calendar_event_id TEXT;
ALTER TABLE meetings ADD COLUMN calendar_start TEXT;         -- ISO 8601 UTC
ALTER TABLE meetings ADD COLUMN calendar_end TEXT;
ALTER TABLE meetings ADD COLUMN calendar_organizer TEXT;
ALTER TABLE meetings ADD COLUMN calendar_attendees TEXT;     -- JSON array of strings
ALTER TABLE meetings ADD COLUMN calendar_join_url TEXT;
ALTER TABLE meetings ADD COLUMN calendar_body TEXT;
```

Filled when a recording starts from a prompt that carried a match. The
`start_recording` command takes an optional `calendar_event` argument for
this. The meeting details pane shows organizer, attendees, join link and body
when present.

## Summary integration

`summary/processor.rs` already appends a `<user_context>` block when the user
supplies a custom prompt. When `include_body_in_summary` is on and the meeting
has calendar data, a `<calendar_context>` block with title, organizer,
attendees and body is appended after it, capped at 4,000 characters of body.
The toggle is read at summary time, not at recording time, so turning it off
later stops the body from being sent.

## Privacy

- Feed URL is a bearer secret. Keyring only, masked in the UI, never logged.
- Event bodies are stored locally only, like transcripts. They go to the
  summary provider only when the toggle is on, and that provider may be
  remote. The settings copy says so.
- No calendar data is sent to analytics.

## Out of scope

- macOS and Linux. The `calendar` module compiles everywhere, the COM and
  window-title files are Windows only, and the UI is hidden elsewhere.
- Auto-start without a prompt.
- Non-Teams events (in-room, phone, Zoom).
- Reading Outlook or Teams notification toasts.
- Writing anything back to the calendar.
- Any Microsoft Graph, EWS or add-in path. EWS to Exchange Online is blocked
  as of October 2026.

## Delivery order

Each step is its own PR and leaves the app working.

1. `calendar` module with the ICS feed source, keyring storage, scheduler,
   upcoming nudge, settings UI. Testable on the Mac with a local ICS fixture
   and a fake HTTP server, then on Windows against a real Outlook feed.
2. Window title capture and the richer start prompt. Windows only, tested via
   the Stash handoff.
3. Calendar match in the start prompt, "Join and record" arming, database
   migration, meeting details pane, summary context.
4. Classic Outlook COM source. Last, since it can only be developed and tested
   on the Windows machine where both Outlooks are installed.

## Testing

- Unit, cross-platform: ICS parsing incl. recurrence expansion, `EXDATE`,
  `RECURRENCE-ID`; Teams link extraction; window-title stripping; event
  matching windows; nudge de-duplication; settings round-trip.
- Unit, Windows only: COM source against a running classic Outlook with a
  seeded test calendar; window title enumeration against a running Teams
  call. Tests that touch `tauri::test::mock_app()` stay `#[cfg(not(windows))]`
  per the existing rule.
- Manual, Windows: publish a calendar from Outlook on the web, paste the link,
  confirm the next event appears in status, wait for the nudge, click "Join
  and record", confirm recording starts when Teams audio begins and the
  meeting is saved with the calendar fields.

## Open questions

- Should the feed poll pause while a recording is running, to keep the audio
  pipeline free of network work? Proposed: no, the poller is a separate task
  and does not touch the audio threads.
- When the same event appears in both sources with different titles (user
  edited a recurring instance locally), which title wins? Proposed: COM,
  because it reflects the local cache the user sees in Outlook.
