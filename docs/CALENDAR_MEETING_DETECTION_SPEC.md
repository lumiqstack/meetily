# Outlook calendar and Teams meeting auto-detect (Windows)

Status: 2026-11-03. The calendar module, nudge and settings UI are on main.
The published ICS feed source shipped on 2026-11-02 and was removed on
2026-11-03: the tenant this is built for blocks calendar publishing and
sharing in Outlook, so the link could never be obtained. Classic Outlook COM
is now the only source. Window title capture (PR 2) is on main. PR 3 (calendar
match, "Join and record", storage, details pane, summary context) is built on
the COM source.

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
| Sources | Classic Outlook COM object model on the same machine, behind the `CalendarSource` trait so another source can be added. The published ICS link source was removed on 2026-11-03 (tenant blocks publishing). |
| Trigger | Prompt, never auto-start. The prompt is prefilled with the matched event. |
| Which events | Teams events only: an event counts if its location or body contains a Teams join link, or if Teams audio is detected at its start time. |
| Metadata stored | Title, start, end, organizer, attendees, join link, body. |
| Body on COM | Not read. The Outlook Object Model Guard can raise a security dialog on body access. `CalendarEvent.body` stays `None`; the summary toggle only matters when a future source fills it. |
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
├── outlook_com.rs      classic Outlook COM reader; COM code #[cfg(windows)], mapping is pure
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
    fn kind(&self) -> SourceKind;        // OutlookCom
    async fn poll(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> SourcePoll;
}
```

`CalendarEvent` fields: `id` (source id, stable across polls), `source`,
`title`, `start`, `end`, `organizer: Option<String>`, `attendees: Vec<String>`,
`location: Option<String>`, `body: Option<String>`, `teams_join_url:
Option<String>`, `is_recurring_instance`.

With one source there is nothing to merge. If a second source returns, two
events are the same meeting when their start times are within two minutes and
their titles match case-insensitively.

### ICS feed source (removed 2026-11-03)

Kept for the record. The feed needed a link from Outlook on the web (Settings,
Calendar, Shared calendars, Publish a calendar). On the target tenant that
page does not exist and the calendar's Sharing dialog offers no publishing,
which is the Outlook on the web mailbox policy with calendar publishing off.
Nothing on the user's side unlocks it, so the source, its keyring storage of
the link, the `icalendar`, `rrule`, `chrono-tz`, `windows-timezones` and
`keyring` crates, and the link block in the settings UI were removed. The
settings file still loads with the old `ics_feed_configured` and
`outlook_com_enabled` keys present; they are ignored.

What it did, in case it comes back for a tenant that allows publishing:

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

Implemented in `calendar/outlook_com.rs`.

- The COM code is `#[cfg(target_os = "windows")]`. Late-bound `IDispatch`
  calls through the `windows` crate (`Win32_System_Com`, `Win32_System_Ole`,
  `Win32_System_Variant` features). Property and method names are resolved
  with `GetIDsOfNames` on every call.
- `Outlook.Application` is obtained with `GetActiveObject` first, so a running
  Outlook is reused, then `CoCreateInstance(CLSCTX_LOCAL_SERVER)` as a
  fallback, which starts classic Outlook in the background. `CLSIDFromProgID`
  failing means classic Outlook is not installed; both start paths failing
  means it could not be started. Each is `Unavailable` with that fixed reason,
  shown under the switch in settings. The new Outlook app has no object model
  and is never read.
- Reads `GetNamespace("MAPI").GetDefaultFolder(9).Items` with
  `IncludeRecurrences = true`, `Sort("[Start]")`, then `Restrict` on
  `[Start]` and `[End]` for the window (one hour back, 48 hours ahead), walked
  with `GetFirst` and `GetNext`, at most 500 items. Properties read per item:
  `GlobalAppointmentID`, `Subject`, `Start`, `End`, `Location`, `Organizer`,
  `RequiredAttendees`, `OptionalAttendees` (display names, semicolon
  separated), `IsRecurring`. `Body` is never read.
- The event id is `GlobalAppointmentID`, plus the occurrence start for
  instances of a recurring series, so each occurrence is nudged once.
- The Teams join URL is read through `PropertyAccessor.GetProperty` from the
  named MAPI property Teams writes on the appointment
  (`SkypeTeamsMeetingUrl`, then `OnlineMeetingExternalLink`), then from the
  `Location` text. An appointment with neither still counts as a Teams
  meeting when its location says "Microsoft Teams Meeting". Whether the
  PropertyAccessor read raises the Object Model Guard on a managed PC is a
  Windows QA check; if it does, the fallback is location text only.
- `Restrict` is given dates as `MM/dd/yyyy hh:mm AM` and parses them with the
  machine locale. That is right for an English Windows locale; a non-English
  locale is a known gap and would surface as an `Error` status, not as
  missing meetings.
- Runs on one dedicated thread that calls `CoInitializeEx(COINIT_APARTMENTTHREADED)`
  and lives for the process, because the Outlook object model is
  single-threaded apartment. Polls are sent over a channel and awaited on a
  oneshot with a 30 second timeout; a timeout is an `Error` and keeps the
  previous event list. Poll interval 2 minutes, plus an immediate poll when
  the switch is turned on or Refresh now is pressed.
- Every `IDispatch`, `VARIANT` and `BSTR` is released at the end of each poll,
  so Outlook can exit while Meetily keeps running.
- Logs carry fixed state names and HRESULT codes only, never subjects, people
  or links.

### Teams join-link extraction

`teams_link.rs` finds the first `https://teams.microsoft.com/l/meetup-join/...`
or `https://teams.live.com/meet/...` URL in a text. It handles the HTML-escaped
and the `&amp;`-encoded forms that appear in ICS bodies. Pure function, unit
tested.

### Window title capture

`meeting_detection/window_title.rs`. The enumeration is `#[cfg(windows)]`:
`EnumWindows` filtered by `GetWindowThreadProcessId` against the Teams PIDs
the process detector already has, keeping visible windows with a non-empty
title. The meeting window title is the meeting subject followed by
`| Microsoft Teams`. Returns the first title (Z order, foreground first) that
is not the main Teams window: "Microsoft Teams" alone, or a navigation section
in front of the suffix (Activity, Chat, Teams, Calendar, Calls, Files,
OneDrive, Apps). The strip rules are a pure function with tests on every
platform; off Windows the capture always returns `None`. Needs the
`Win32_UI_WindowsAndMessaging` feature.

The manager captures the title only when it emits `teams-call-likely-started`,
so there is no per-poll window enumeration. The frontend shows it as the
prompt title ("Teams call: <subject>") and, on "Start Recording", hands it to
the recording start through `sessionStorage` `autoStartMeetingName`, the same
key the calendar nudge uses.

### Scheduler and matching

- Nudge lead time is a setting, default 2 minutes, range 0 to 15.
- For each upcoming Teams event the scheduler emits `calendar-meeting-upcoming`
  once, at start minus lead time, and shows a system notification through
  `notifications::NotificationManager` with the event title. Events already
  running when Meetily starts are not nudged, but still match.
- Matching: when the detector produces a start prompt, it asks the calendar
  manager for the Teams event whose start lies between 20 minutes ago and 10
  minutes ahead and that has not ended (`scheduler::match_event_for_call`).
  If several match, the one with the start closest to now wins. The payload
  carries the match and the window title. A disabled calendar never matches,
  even if an event list is still in memory.
- Prompt title priority: calendar match title, then window title, then
  "Teams meeting".
- Armed recording: after "Join and record" the frontend opens the join URL
  through the `open_teams_join_url` command (the `open` crate, limited to
  `https` links on `teams.microsoft.com` and `teams.live.com`) and arms the
  event for 10 minutes in session storage. The next `teams-call-likely-started`
  while armed starts recording without a second prompt and attaches the event.
  The button is offered only when Teams call detection is on, since that is
  what fires the start. The detector itself still only emits prompts; arming is
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

`get_calendar_settings`, `set_calendar_settings`, `get_calendar_status`,
`get_upcoming_calendar_events` (for a small "next meetings" list in the UI),
`refresh_calendar_now`.

### Settings

```rust
pub struct CalendarSettings {
    pub calendar_enabled: bool,            // default false; the one switch
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
ALTER TABLE meetings ADD COLUMN calendar_source TEXT;        -- 'outlook_com' (was also 'ics_feed' before 2026-11-03)
ALTER TABLE meetings ADD COLUMN calendar_event_id TEXT;
ALTER TABLE meetings ADD COLUMN calendar_start TEXT;         -- ISO 8601 UTC
ALTER TABLE meetings ADD COLUMN calendar_end TEXT;
ALTER TABLE meetings ADD COLUMN calendar_organizer TEXT;
ALTER TABLE meetings ADD COLUMN calendar_attendees TEXT;     -- JSON array of strings
ALTER TABLE meetings ADD COLUMN calendar_join_url TEXT;
ALTER TABLE meetings ADD COLUMN calendar_body TEXT;
```

`20261105000000_add_meeting_calendar_title.sql` later added `calendar_title`, the
event's subject, because generating a summary replaces `meetings.title`. Teams
recaps and recordings are compared with it. See `docs/TEAMS_RECAP_SYNC_SPEC.md`.

Filled when a recording starts from a prompt that carried a match. The
The frontend starts recordings without a meeting argument and names
them at save time, so the match travels the same way: the prompt stashes the
event in session storage, the start marks it as the running recording's
event (every manual start clears it), and the save passes it to
`api_save_transcript` as the optional `calendar_event` argument, which
writes the columns after the meeting row exists. The meeting details pane shows organizer, attendees, join link and body
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
   upcoming nudge, settings UI. Shipped 2026-11-02; the feed source was
   removed the next day (see above).
2. Window title capture and the richer start prompt. Windows only, tested via
   the Stash handoff. Shipped.
3. Calendar match in the start prompt, "Join and record" arming, database
   migration, meeting details pane, summary context. In review.
4. Classic Outlook COM source, replacing the feed. Mapping and window logic
   unit-tested on the Mac; the COM calls verified on the Windows machine.

## Testing

- Unit, cross-platform: OLE date conversion, the `Restrict` filter text,
  attendee splitting, appointment to event mapping (join link precedence,
  recurring ids, blank subjects, dropped items), window filtering and
  capping; Teams link extraction; window-title stripping; event matching
  windows; nudge de-duplication; settings round-trip including a settings
  file written by the feed build.
- Unit, Windows only: window title enumeration against a running Teams call.
  Tests that touch `tauri::test::mock_app()` stay `#[cfg(not(windows))]` per
  the existing rule. The COM reader has no automated test; it is checked by
  hand.
- Manual, Windows, with classic Outlook running: turn the calendar switch on,
  confirm the status line says "Reading classic Outlook" with a plausible
  event count and the next Teams meeting matches Outlook's calendar, confirm
  no Outlook security dialog appeared, wait for the nudge, then close Outlook
  and confirm it exits with Meetily still running. With Outlook closed, the
  next poll should start it again in the background. Then "Join and record"
  and the saved calendar fields once PR 3 lands.

## Open questions

- Should the Outlook poll pause while a recording is running? Proposed: no,
  the reader is a separate STA thread and does not touch the audio threads.
- Starting classic Outlook from `CoCreateInstance` when the user closed it on
  purpose: acceptable for now, since the nudge needs the calendar, but a
  "only read Outlook while it is running" option may be wanted.
