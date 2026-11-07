# Teams recap sync to Obsidian (Windows)

Status: 2026-11-06. Steps 1 to 6 merged. Step 3's scanner reads the Teams Recap app ("Meeting Recaps"): on the Windows machine its list check found the week's 23 meetings, each with a recap link naming its recording file. No real sync has run yet, so the import half of step 3, the QA checklist (step 4) and the live matching of steps 5 and 6 are not yet exercised on Windows.

## Goal

Keep the Obsidian vault complete for every Teams meeting that has a Copilot
recap, without pasting links one at a time. One button in Meetily reads the
meeting list of the Teams Recap app, imports every recap it has not seen, attaches
recaps to meetings Meetily already holds from their recordings, and writes or
refreshes one vault note per meeting. No duplicates, no misses, and a count of
what happened at the end.

Windows only, like the existing recap link import. The macOS and Linux builds
keep compiling, show the button disabled with a short note, and run no sync
code.

## Decisions already made

| Question | Decision |
| --- | --- |
| Source page | The Recap app in Teams ("Meeting Recaps" on the app bar), whose Home tab lists recent meetings with recaps, read in the persisted WebView2 profile the recap link import already uses. |
| Microsoft Graph | Not used. No Entra app registration, no admin consent. |
| Trigger | Manual "Sync Teams recaps" button. No schedule in this version. |
| Backfill | A since date in settings, default 30 days before the first sync. Each later sync starts from the last confirmed sync date minus an overlap window. |
| Existing meeting from the recording | Attach the recap to that meeting. One meeting, one note. Match on the recording file URL. |
| Existing meeting recorded live | Attach the recap when the meeting's calendar window fits the recap's time and its thread id or title confirms it. When that is ambiguous, import the recap separately and report it. See "Matching a recap to a live recording". |
| Recording of a meeting recorded live | Link the file to the live meeting without downloading or transcribing it. The live transcript stays, and Teams' own transcript of that meeting is never imported. An ambiguous match imports the recording as its own meeting. See "Matching a recording to a live recording". |
| Recap changed in Teams | Overwrite the vault note. Tags survive (they come from the meeting record); other edits made in Obsidian do not. |
| Recap not ready or absent | Leave the meeting unmarked so the next sync retries. Count it as "not ready" in the report. |
| Reporting | One toast with counts: new, updated, skipped, not ready, failed, and how many may duplicate a live recording. A link opens the vault Meetings folder. |
| Platform | Windows only. |

## What exists today

- `api_import_copilot_recap_from_link` (`summary/commands.rs`) takes one recap
  link, scrapes the AI summary tab in a hidden WebView2 window
  (`audio/teams_recap.rs`, script in `audio/teams_recap.js`), creates a meeting
  with an imported summary, and writes the vault note through
  `obsidian::export_copilot_recap_note`. The note is named
  `YYYY-MM-DD Title.md` and carries the meeting id in its front matter. A
  re-export of the same meeting overwrites the same file because the filename
  is pinned per meeting id.
- `audio/sharepoint_sync.rs` is the model for a manual scan with state: a since
  date, an `imported` map keyed by canonical file URL, a scan command, a debug
  enumeration command, and a results toast.
- `meeting_sources` records, per meeting, where it came from: `import_source`
  (`sharepoint`, `url`, `teams-transcript`, `copilot-recap`, `file`, and
  `live-recording` once a recap or a recording attaches to a live meeting),
  `recording_url`, `recording_file`, `recorded_at`. Recording imports store the
  direct file URL with any token query stripped.
- The recap link's `fileUrl` query parameter names the recording or transcript
  file on SharePoint. `recording_metadata` already parses it for the recorded
  timestamp and the frontend derives the suggested title from it.

## Gaps this spec closes

1. Nothing lists recaps. The user has to find and paste each link.
2. Nothing checks whether a recap was imported before. Pasting the same link
   twice creates two meetings and two notes.
3. Nothing remembers how far back the last import reached.
4. A meeting imported from its recording and the same meeting imported from
   its recap become two meetings.
5. A meeting Meetily recorded live and the same meeting imported from its recap
   become two meetings, because a live recording has no file URL to key on.
6. A meeting Meetily recorded live and the same meeting's recording arriving
   through SharePoint become two meetings, for the same reason.

## Architecture

```
Settings (since date)          the sync's one Teams window (RecapReader)
        │                                  │
        ▼                                  ▼
sync_teams_recaps_command ──► RecapListScanner ──► Vec<RecapCandidate>
        │                     (Teams Recap app)     (recap link, title,
        │                                            recorded_at, fileUrl)
        ▼
   for each candidate, then once more each one Teams did not open
        ├── identity: canonical fileUrl key
        ├── lookup meeting by key (recap sync state, then meeting_sources)
        ├── no key found: match a live recording by calendar window
        ├── read the recap in the same window
        ├── create meeting | attach summary to existing meeting
        ├── export_copilot_recap_note (new or overwrite)
        └── record outcome in sync state
        ▼
   RecapSyncReport ──► toast
```

The scanner and the reader are new. The reader's window reads the list first,
then every recap, with the link import's page script; everything after it
reuses the summary repository and Obsidian exporter.

### Identity

The canonical key of a recap is its `fileUrl` after percent decoding and
lowercasing, the same normalisation `sharepoint_sync` applies to file URLs so
the two sources collide on the same meeting. A recap link without `fileUrl`
falls back to the full link with its query sorted; such recaps can still be
imported and deduped against themselves but cannot be matched to a recording.

Stored in two places:

- `meeting_sources.recording_url` already holds the direct file URL for
  recording imports. The recap import sets it from `fileUrl` when the meeting
  is created by the sync.
- A new store `teams_recap_sync.json` (same mechanism as the SharePoint sync
  store) holds `last_sync_date`, `since_date`, and `imported`: key to
  `{ meeting_id, imported_at, content_hash }`.

Lookup order for a candidate: the sync store by key, then `meeting_sources` by
canonical `recording_url`. A hit in either means "existing meeting". With no
hit, the rule in the next section decides whether a live recording is that
meeting.

Teams cuts a long recording at 4 hours into `…-Meeting Recording.mp4` and
`…-Meeting Recording 2.mp4`, and a recap may name any part. The `meeting_sources`
lookup therefore looks for the file itself first and then for any other part of
the same recording, meaning the same folder, title and stamp. The sync store
keeps its exact keys.

The words after the stamp are localized: a Spanish Teams writes
`…-Grabación de la reunión.mp4`. `SUFFIXES` in `teams_recap_metadata.rs` lists
the forms Meetily knows, and both the stamp reader and this lookup use it.

### Matching a recap to a live recording

A meeting Meetily recorded live has no file URL, so the identity lookup cannot
find it and its recap becomes a second meeting. Live recordings started from
the calendar prompt do store the event they matched: `calendar_event_id`,
`calendar_title`, `calendar_start`, `calendar_end` and `calendar_join_url` on
the meeting row. This rule uses them, in both the link import and the sync loop.

The rule runs only when the identity lookup finds nothing. Identity stays first
because it is exact, and the rule writes the recap's key onto the meeting it
attaches to, so every later import of that recap goes straight there. The one
exception is a key that a recording attach put on the meeting, which is only as
good as the title it matched on: see "Matching a recording to a live recording".

**When the recap happened.** A recap can carry two times. They are tried in
this order, and the second only if the first fits no live recording:

- In the sync loop, the start time the Recap app row gives: when the meeting
  was recorded, as an exact instant, or else its calendar slot.
- The stamp in the `fileUrl` file name, which is when the recording started,
  read in this machine's time zone (`teams_recap_metadata.rs`).

A recap with neither, such as a pasted link without `fileUrl`, is not matched.

**Candidates.** Meetings with `calendar_start` and `calendar_end` and no
`recording_url` in `meeting_sources`. A live meeting that already holds another
recap is out, so the recap of a second recording cannot overwrite the first. A
live meeting that holds only a recording stays a candidate for a recap whose
link names no file, because nothing says that recap belongs to another
recording.

**The window.** A candidate fits when one of the recap's times lies inside
`calendar_start - 15 min` to `calendar_end + 15 min`. Its distance is zero
inside the scheduled slot and otherwise the minutes to the nearer edge. The
tolerance covers a recording started while people gather and a call that runs
past its slot. It is not wider because the next meeting is often that close.

**Time alone never attaches.** Double-booked meetings overlap by definition, so
a fitting candidate also needs evidence that it is the same meeting:

- *Thread id.* The recap link's `threadId` against the thread id in
  `calendar_join_url`. Different means a different meeting, whatever the clock
  says, and the candidate is dropped. Equal confirms it. A recurring series
  shares one thread id, and the window is what tells its occurrences apart.
- *Title*, for candidates where either side has no thread id, as with a
  `/meet/<code>` join link or an Outlook event with no join link. The recap's
  title (the row's or the pasted one) and the title in its file name are each
  compared with the event's subject, on lowercase letters and digits only,
  because Teams drops punctuation from file names. They agree exactly when
  equal, and weakly when one is a prefix of the other of at least 8 characters,
  because long titles get truncated and users add to them. The subject is
  `calendar_title`. A meeting saved before that column existed falls back to its
  own title, which generating a summary replaces with the summary's first
  heading, so such a meeting usually stops agreeing once it is summarized.

**Deciding.** Stronger evidence is considered first: thread id, then an exact
title, then a prefix. A closer slot never outranks better evidence, so "Weekly
sync" cannot take the recap of "Weekly sync with vendors" recorded a minute
before its slot ended.

| What is left | Outcome |
| --- | --- |
| Confirmed candidates, one closest | Attach to it. |
| Confirmed candidates tied on distance | Ambiguous. |
| Candidates fit the window, none confirmed | Ambiguous. |
| Nothing fits | New meeting, as before. |

Two live meetings tie when Meetily was stopped and restarted during one call:
both carry the same event, so both have the same window.

**On ambiguity, never guess.** The recap is imported as its own meeting and
note, exactly as if nothing had matched, and the report says it may duplicate
a live recording: `possible_duplicates` in the sync report, a line in the link
import's toast, and the candidate meeting ids in the log. A wrong attach is the
expensive mistake. It replaces the meeting's summary, overwrites its note with
another meeting's recap, and leaves the recap's key on that meeting so later
syncs keep landing there. A missed attach costs one extra note the user can
delete. Because the recap's meeting then holds the key, later syncs update it
and do not ask again.

**Attaching.** The same as attaching to a meeting imported from its recording:
the meeting keeps its title, date, tags and transcript, the recap becomes its
summary, and `export_meeting_note` overwrites its note at the pinned file name.
Three additions:

- A `meeting_sources` row with `import_source` `live-recording`, `recording_url`
  from the recap link and `recorded_at` from the stamp. This is what makes the
  next import an identity hit. It is written before the summary is replaced, and
  if it cannot be written nothing is attached, so a meeting never holds a recap
  without its key.
- The summary the recap replaces stays in `summary_processes.result_backup`
  unless it is itself a recap, so a wrong attach can be recovered by hand. Nothing
  in the UI shows it yet, and regenerating the meeting's summary overwrites it.
- In the sync report the attach counts as "updated" if the meeting already had
  a summary, otherwise "new".

**What this rule does not cover.**

- A live meeting with no calendar match has no window. Its recap stays a second
  meeting. So does a meeting Meetily recorded before calendar matching existed.
- Meetings already duplicated before this rule landed stay two: the identity
  lookup finds the recap's own meeting first.

### Matching a recording to a live recording

Nothing ties the file URL of a recording found by the SharePoint scan to a meeting
Meetily recorded live, so Meetily downloads it, transcribes it and saves a second
meeting. This rule stops that in the automatic scan and marks the candidates in
the import dialog. A pasted link is an explicit request to import that file, so it
imports as before.

**What attaching means.** Meetily does not download or transcribe the file. The
live meeting keeps its title, date, tags, transcript and summary, and gets a
`meeting_sources` row with `import_source` `live-recording`, the file's URL and
its stamp. A stamp marked `UTC` stays out of that row, because `recorded_at` means
local time. The SharePoint ledger lists the file as imported, and a recap of the
same file then attaches by identity. The cost is that Meetily never imports Teams'
own transcript of that meeting, and that a wrong match leaves a recording
unimported. Attaching does not rewrite the note, because nothing it shows has
changed. Its front matter picks up the source at the next export.

**A recording has less evidence than a recap.** It has no thread id in any field
Meetily reads, so nothing can veto a meeting in the same slot, and tenant-wide
search can surface other people's recordings. Comparing with the meeting's own
title would not work either, because generating a summary replaces that title
with the summary's first heading, usually before the file appears. So the rule
is stricter than the recap rule, and compares with the event's subject:

- *Only Teams recordings qualify.* The file name must read
  `<title>-<YYYYMMDD>_<HHMMSS>[UTC]-<suffix>[ <part>].<ext>`, with a suffix of
  `Meeting Recording`, `Meeting Transcript`, or `Grabación de la reunión` from a
  Spanish Teams. Other videos the search finds never match.
- *When it happened.* The rule tries up to three times in order, each only if the
  earlier ones fit no live recording. First a stamp marked `UTC`, then SharePoint's
  `created`, which is an instant, then an unmarked stamp read in this machine's
  zone. The instant comes before the unmarked stamp because most recordings are in
  colleagues' OneDrives, stamped in their zones.
- *The window.* The same 15 minute tolerance as the recap rule.
- *Title.* The file name's title must equal `calendar_title`, on lowercase letters
  and digits only. One exception: a file title of at least 100 letters and digits
  may be the start of the subject, because Teams cuts titles at about 160
  characters. Shorter prefixes do not count, so "Weekly sync" cannot take "Weekly
  sync with vendors".

**Deciding.** The same table as the recap rule, with title as the only evidence.
One closest confirmed candidate attaches. A tie, or candidates that fit but are
not confirmed, is ambiguous. Nothing fitting means a new meeting. An ambiguous
recording imports as its own meeting, exactly as before, and the log names the
candidate meeting ids. There is no toast because the scan runs in the background.

**Candidates.** Meetings with `calendar_start` and `calendar_end` that hold no file
yet: no `recording_url`, or a recap link that names no file. A candidate that
wins the match but has no transcript does not attach, because the recording may
be the only record of what was said. It imports as its own meeting.

**Already held.** A file counts as imported when a `live-recording` row holds it
or another part of it, whether the URL there came from this rule or from a
recap link's `fileUrl`. That covers the order live, recap, recording, and the second
part of a long meeting. The live transcript covers every part, so no part is
downloaded. Other import sources stay as they were: a recording imported through
SharePoint does not stop its second part from importing, because that part's audio
is not in the meeting.

| The live meeting holds | A recording | A recap naming a file | A recap naming no file |
| --- | --- | --- | --- |
| Nothing | Candidate | Candidate | Candidate |
| A recording | Same recording: held. Otherwise its own meeting. | Same recording: identity. Otherwise its own meeting. | Candidate |
| A recap naming a file | Same recording: held. Otherwise its own meeting. | Same recording: identity. Otherwise its own meeting. | Its own meeting |
| A recap naming no file | Candidate. The row keeps the recap link and only the ledger records the file. | Its own meeting | Same link: identity. Otherwise its own meeting. |

A meeting has room for one URL. When a recap naming no file attaches to a meeting
holding a recording, the recap link replaces the file's URL and the ledger keeps
the file imported.

**In the automatic scan.** The scan takes recordings oldest first and decides again
for each one, so a recording earlier in the same scan can claim a meeting, and
another part of that recording then counts as held. Attaching does not use one of
the scan's import slots. It counts as a success for the watermark, so the watermark
passes it. A held file goes into the ledger too, so deleting that meeting later
does not bring the recording back. While a recording is in progress the scan
imports nothing, whichever transcription provider is set, because the meeting being
recorded has no row to match until it is saved.

**In the import dialog.** The scan reports the live meeting for each file a
`live-recording` row holds and each file the rule would attach. Those files show
`Recorded live` and start unticked. Leaving one unticked writes nothing. A later
automatic scan attaches it while the file is still inside the scan window, and
otherwise its recap can still attach by the recap rule.

**Undoing a wrong match.** Ticking such a file imports it as its own meeting. When
that import lands, any `live-recording` row holding exactly that file gives it up,
so a later recap of the file follows the new meeting.

**A recap can still refuse.** A recap that finds its file on a live meeting compares
threads before attaching. If its thread id and the one in `calendar_join_url` are
both known and differ, the recording rule attached that file to the wrong meeting.
The live meeting gives up the key and keeps its summary, and the recap rule
continues as if identity had found nothing. The recording itself stays unimported
until the user ticks it in the dialog. Without this, one wrong title match could replace the
meeting's summary, which is the mistake the recap rule exists to prevent.

**What this rule does not cover.**

- A live meeting with no calendar match has no window, so its recording stays a
  second meeting.
- Two different meetings with the same subject in one slot cannot be told apart.
  The wrong one attaches, and a recap only catches it when both thread ids are
  known.
- A meeting saved before `calendar_title` existed falls back to its own title, which
  usually stops agreeing once it is summarized. Its recording stays a second meeting.
- A second recording in one call, with a different stamp, stays its own meeting, as
  with the recap rule.
- A recording that arrives after a recap-only meeting (`copilot-recap`) still creates
  a second meeting. That meeting has no transcript, so dropping the recording would
  lose it, and moving its transcript across is a separate change.

### RecapListScanner

A second WebView2 script, `teams_recap_list.js`, driven by the same window,
poll and timeout machinery as `teams_recap.js`. It is given the since date and
returns one of:

- `{ status: "waiting" }` while the list renders or sign-in is in progress.
  The existing `launcher` and `opening_web` handling applies.
- `{ status: "navigating" }` while it steers to the list.
- `{ status: "page", items: [...], more: true }` with every meeting row read so
  far. An item carries the recap link the script built, the title and the
  start time.
- `{ status: "reloading" }` once per listing, when the list stopped growing
  before reaching the since date: the script reloads the page and reads the
  list again from the top.
- `{ status: "done", items: [...], stop, oldest }` once a row older than the
  since date is reached (`stop: "since"`), the list stops growing after that
  reload (`"idle"`), it reaches the row cap (`"cap"`), or it shows no meetings
  at all (`"empty"`). `oldest` is the oldest calendar slot it reached. `idle`
  and `cap` end before the since date, so the list may be missing older rows:
  the line beside the buttons says where it stopped.

The Rust side keeps the last snapshot, caps it at 500 rows and dedupes items
by key, first row wins. Rows without a recap link are reported as "not ready"
and never written to the sync store.

The list, as learned on the Windows machine in November 2026
(`teams_recap_list.rs`, `teams_recap_list.js`):

- The window opens the Teams web shell at `https://teams.cloud.microsoft/v2/`.
  Starting at `teams.microsoft.com` costs about 40 seconds on a redirect page
  (`?ocdiRedirect=index`) before Teams reloads on the new domain.
- The list lives in the Teams Recap app, "Meeting Recaps" on the app bar (app
  id `981bbfd0-5eb8-4393-8158-bfff69d2d6b5`). The script clicks it once, by id
  and then by name, opening "more apps" if the bar has no such button. The app
  has two tabs, Home and Audio recaps, and the list is Home.
- Home is an accordion of groups ("This week", "Last week", ...), each a list
  of `li[data-tid="podcast-meeting-item"]` rows, newest calendar slot first.
  The script opens collapsed groups, scrolls the list's own pane to load older
  rows, and stops at the first row whose slot is before the since date. Past
  that row it opens only a collapsed group above it, and it opens a group again
  if a re-render collapses it, up to three times. A recap left open from an
  earlier visit hides the list, so the script presses its "Go to Meeting
  Recaps" button once. It never clicks inside a row.
- The first listing after Meetily starts can stop at the newest rows. Twice,
  a check made within a minute of the start showed only the list's first rows
  (7 and 6 recaps) and never loaded older ones, while checks later in the same
  session read the whole window (28 and 29).
  So the list counts as stopped only after 12 seconds with no new row and no
  deeper scroll. While it waits, the script scrolls back up a screen every 3
  seconds so the list's end comes into view again. If it still stops before
  the since date, the script reloads the page once and reads the list again.
  The mark for that one reload lives in the window's sessionStorage, and every
  listing opens a new window.
- A row's markup holds no link. Its React component gets the meeting as a prop
  (`memoizedProps.meeting`, a few fiber levels up), and that object carries
  everything a recap sharing link holds: `subject`, `start` and `end`,
  `threadId`, `iCalUid`, `organizerId`, `transcripts[]` (with `callId`,
  `startDateTime` and the transcript's drive item), `recordings[]`, and
  `recommendationData` with the recording file's `driveId`, `driveItemId` and
  path (`url`). The same object holds the AI summary and action items; the
  script reads neither.
- The script builds the link the recap's Share button gives: the same keys in
  the same order, `fileUrl` set to the file's path with Office's `?web=1`, and
  `masterICalUid` for an occurrence of a series (the GlobalObjectId with its
  date bytes zeroed). The link uses the page's own host, so the recap import's
  fresh window should skip the redirect too (see "Open questions"). Every row
  seen so far names its file, even a meeting whose row lists no recording, so
  the link keys on the recording like a pasted link. A row without that path
  would get a link naming the transcript's drive item, which keys on its own
  query.
- A row's time is when it was recorded, the earliest transcript or recording
  start, not the slot it shows: a recurring row can show the series' next slot
  beside an earlier occurrence's recap.
- A meeting can have two rows, one for its transcript and one for its
  recording. Both name the same file, so they collapse to one candidate.
- In probe mode (the "Check recap list" button) the script also returns the
  listing's counts and timings: rows read, link kinds, rows without props, why
  it stopped, the clicks it made, when rows first appeared and when the list was
  done, and how many frames the page painted. The debug command writes them,
  with the candidates, to `teams_recap_list_probe.json` in the app data folder.
  The candidates carry titles and file paths, so the file stays on that machine
  unless the user shares it.
- `MEETILY_RECAP_LIST_SCRIPT=<file>` replaces the bundled script with that
  file, so the script can be tuned on Windows without a rebuild if Teams
  changes its layout. The file must keep the `%SINCE%` and `%PROBE%`
  placeholders.
- `frontend/tests/browser/teams-recap-list.browser.cjs` runs the script
  against a synthetic Recap app; see `frontend/tests/README.md`.

### Sync loop

For each candidate, oldest first:

1. Resolve identity and look up an existing meeting, then a live recording.
2. Read the candidate's recap in the sync's Teams window (see Reading recaps
   below). Outcomes:
   - ready: continue.
   - not ready: Teams opened the recap but showed no AI notes in time, for
     example because it is still generating them. Count "not ready", do not
     touch the store, continue.
   - not opened in time: read it once more after the other recaps. If it
     still does not open, count "failed" with the message.
   - any other error: count "failed" with the message, continue.
3. Build the recap markdown exactly as the link import does, including the
   "Open original Teams recap" link.
4. Compute `content_hash` over the markdown. If the store has the key with the
   same hash, count "skipped" and continue, unless the meeting needs a new
   date (see step 5).
5. No existing meeting: create it through `create_meeting_with_imported_summary`
   with `import_source` `copilot-recap`, `recording_url` from `fileUrl`, and
   `recorded_at` from the link. Date it by the row's start, the earliest
   transcript or recording start Teams gave. That start is an instant, and many
   recaps have no recording in their link, so no stamp to date them by. Count
   "new".
   Existing meeting: write the recap as its summary through
   `update_meeting_summary`, keeping the meeting's title, tags, transcript and
   source. Count "updated" (or "new" if that meeting had no summary yet). A
   live recording also gets its `meeting_sources` row here. A `copilot-recap`
   meeting dated more than 15 minutes from the row's start moves to that start.
   This repairs recaps imported before the sync dated by the row: those with no
   recording were dated at the import. Closer than 15 minutes is the same start
   read another way (recording, transcript or calendar slot), so it stays.
6. Export the vault note through `export_copilot_recap_note` for recap-only
   meetings, or `export_meeting_note` with the stored transcript for meetings
   that have one. Both overwrite the pinned filename. A meeting that moved in
   step 5 gets the name its new date gives instead, and the note under its old
   name is deleted if it is still that meeting's note.
7. Record the key, meeting id, time and hash in the store.

After the loop, `last_sync_date` becomes the sync start time. The next sync
scans from `last_sync_date` minus 7 days, because a recap appears hours after
a meeting and Teams may finish generating one days later. A sync that read no
recap at all (every row had no link, was not ready, or failed) leaves the date
where it was: it learned nothing and may have misread the list, and moving on
would lose the rest of the first sync's backfill.

The whole loop holds a single `RECAP_SYNC_LOCK`; a second click while a sync
runs returns "already running". The loop is resumable: a crash midway loses
nothing because every imported recap is in the store before the next one
starts, and re-running converges to the same state.

### Reading recaps

A sync opens one Teams window and closes it when the sync ends. The list is
read in it first. Teams may ask the user to pick their account there, so the
window shows itself when the list is not read within 25 s. Then the same window
reads every recap, where the user signed in, so Teams starts and signs in once
per sync. The first read may take 3 minutes; each later read gets 1 minute. If
the window is still hidden and no recap has opened in it 25 s into a read, it
shows itself, as the list's does.

Two real syncs led here. In the first, on 2026-11-06, each recap had a fresh
hidden window and a minute, and all 30 ran out of time while Teams started. In
the second, on 2026-11-07, the recaps had one hidden window, opened after the
list's window closed. The first 7 did not open in 10 minutes, and the 23 after
them opened 7 to 18 s after their links. Whether that window needed the account
choice again, or Teams waited for something else while hidden, the log did not
say.

Before each recap the window goes to `about:blank` and waits until the previous
recap's page is gone. Otherwise a poll that runs before the next page loads
could read the previous recap in its place. The page script reports
`recap_open` once the recap's Share button (`data-tid="recap-share-button"`) or
its AI summary tab is on the page. A read that runs out of time after that is
"not ready". A read that never saw it is tried once more after the other
recaps, and fails if it still does not open, with `Teams did not open this
recap within N seconds.`

While a read waits, the page script also reports where: the page's host and
path, with ids masked and no query; whether the page is visible and focused;
and how many frames it has drawn. The log names them each time they change, and
in the line ending a read that never opened, so a stall on a sign-in page can
be told from Teams not drawing in a hidden window.

The window holds `AUTH_FLOW_LOCK` while it is open, so a SharePoint scan or
recording import started during the sync waits for it to end.

### Dedupe in the manual link import

`api_import_copilot_recap_from_link` adopts the same identity lookup. Pasting a
link whose key is known attaches to the existing meeting and overwrites the
note instead of creating a second meeting. This lands first and on its own.

## Tauri commands

| Command | Purpose |
| --- | --- |
| `sync_teams_recaps_command` | Run one sync. Returns `RecapSyncReport { new, updated, skipped, not_ready, failed: Vec<{title, error}>, possible_duplicates: Vec<title>, scanned, meetings_folder }`. |
| `get_teams_recap_sync_state_command` | Since date, last sync date, count of known recaps, and the last listing: when the list was read, how many recaps it listed, how many of those are already synced and pending, rows without a link, its error, and where it stopped if it stopped before the since date. |
| `set_teams_recap_sync_prefs_command` | Since date. |
| `teams_recap_list_debug_command` | Return the raw candidate list without importing. For learning selectors and for the QA doc. |

Events: `teams-recap-sync-progress { done, total, current_title }` so the
background job toast can show progress; the existing `meetings-changed`
emission after each write.

## Settings

Under the existing Obsidian section in `PreferenceSettings.tsx`:

- "Sync Teams recaps" button with the last sync date beside it.
- "Check recap list" button. Its result stays on a line beside the buttons
  instead of a toast, for example `Checked at 12:40 PM: 28 recaps, 3 already
  synced, 25 pending.` The line shows the last listing, whether a check or a
  sync read the list. Its counts are taken against the synced recaps each time
  the panel loads, so a sync moves recaps from pending to synced. It names the
  day when the list was read on an earlier one, adds rows without a link as
  "without a recap yet", gives the error when the list was not read, and warns
  when the list stopped before the since date.
- Since date picker, default 30 days back, shown until the first sync completes.
- Disabled on non-Windows with the note "Available on Windows".

The button is disabled when no vault is configured, with the same message the
link import gives.

## Database

One column is added: `meetings.calendar_title`, the subject of the event a
recording was matched to, written beside the other calendar columns. Generating
a summary overwrites `meetings.title`, so nothing else keeps the subject. There
is no backfill. `meeting_sources` already has its columns. Sync state lives in the
JSON store like the SharePoint sync state.

One query is added to `meeting_sources`: find a meeting id by canonical
`recording_url`. Because stored URLs are raw, the comparison decodes and
lowercases both sides in Rust over the candidate set of rows with a non-null
`recording_url`, which is small.

One query is added to `meetings`: the live recordings a recap or recording may
still attach to, meaning those with `calendar_start` and `calendar_end`, filtered
in Rust by what their `meeting_sources` row holds. The window comparison runs in
Rust too. `attach_imported_summary` now keeps a replaced summary in `result_backup`
unless that summary is itself a recap.

## Privacy

Same boundary as the recap link import. The script reads the rendered DOM
only; no Tauri IPC is exposed to the Teams page; recap text is never written to
a URL, clipboard, log or external service. Logs record keys, counts and error
messages, not recap content. The since date and keys in the store are the only
persisted additions of a sync, with the last listing beside them (`lastListing`:
times, counts, keys and the listing's error, no titles); the "Check recap list"
button also writes the probe file described above, on that machine only.

## Out of scope

- Scheduled or background syncs.
- macOS and Linux.
- Pulling the Teams transcript when there are no AI notes (the SharePoint
  transcript import covers that path).
- Microsoft Graph.
- A sync log note in the vault.
- Merging edits made to a note in Obsidian back into Meetily.

## Delivery order

1. Identity and dedupe in the manual link import, with the `meeting_sources`
   lookup. Small PR, lands on its own, fixes duplicates today.
2. Sync store, report type, commands, settings UI, debug command, everything
   wired to a scanner stub that returns an empty list. Mac-testable.
3. The Recap app scanner script, learned on the Windows machine through the
   Stash handoff: first the debug command until it lists the week's meetings
   correctly (done in November 2026), then the real sync.
4. QA doc, `docs/TEAMS_RECAP_SYNC_QA.md`, mirroring
   `TEAMS_MEETING_DETECTION_QA.md`.
5. Matching a recap to a live recording, in the link import and the sync loop.
6. Matching a recording to a live recording, in the SharePoint scan.

## Testing

- Unit: canonical key from recap links with and without `fileUrl`, with raw
  and encoded spaces; store round trip; report counting for each outcome;
  overlap window arithmetic; the attach path keeps title, tags and transcript.
- Unit, for the live recording rule: tolerance edges; a different thread id
  vetoes a fitting window; a recurring series attaches only to its own
  occurrence; title agreement through punctuation and truncation; a
  double-booked slot and a restarted recording stay ambiguous; back-to-back
  meetings go to the closer one; a second import is an identity hit; the
  replaced summary lands in `result_backup`.
- Unit, for the recording rule: file name forms, including `UTC` stamps, part
  numbers and the Spanish suffix, raw and percent-encoded; the order of times;
  only an exact or long truncated title attaches; a tie and an unconfirmed fit
  stay ambiguous; a meeting without a transcript is not attached to; attaching
  writes the row and leaves the meeting alone; a re-scan sees the file as held
  through the row and through the ledger; the order live, recap, recording; a
  part of a held recording; a recap naming no file on either side of a
  recording; a contradicting thread takes the key back; a manual import
  releases the key.
- Unit, `#[cfg(not(windows))]`-guarded where `mock_app` is used: the loop over a
  fake extractor returning ready, not ready and error in sequence.
- Unit, for reading recaps: the first read's 3 minutes and later reads' 1
  minute, the verdict on a read that ran out of time (not ready only if the
  recap opened), and how a waiting page is logged. The loop over a scripted
  importer: a recap Teams did not open is read again after the others and
  counts once, by its second read; progress never runs back.
- Unit, for dates: a listed recap is dated by its row and a pasted one by its
  stamp; only a `copilot-recap` meeting more than 15 minutes off its row moves,
  once; a moved note takes its new date's name, and only that meeting's old
  note is deleted, whatever the case of its name.
- Browser fixture: `frontend/tests/browser/teams-recap-list.browser.cjs`
  checks the list script's links, times, groups, stop rule and probe against a
  synthetic Recap app. `teams-recap-dom.browser.cjs` checks the recap page
  script, including when it reports `recap_open` and what it says about a
  page it waits on.
- Manual on Windows: the checklist in `docs/TEAMS_RECAP_SYNC_QA.md`.

## Open questions

- Settled: the Recap app lists meetings whose recording lives in someone
  else's OneDrive. The week read in November 2026 named files in 16 OneDrives.
- Settled: an import in a fresh window does not finish within its minute. In
  the first real sync (2026-11-06) all 30 recaps ran out of time while Teams
  started. The 9th window reached a chat at 33 s. Meetily reopened the link,
  and the recap was being read 12 s later when its minute ran out. Recaps are
  now read in one window (see Reading recaps).
- Teams can render in a hidden window: that 9th window, never shown, reached
  the chat and then the recap's AI summary, and in the second sync 23 recaps
  were read in a hidden window. But the list has never been read while hidden:
  its window is shown after 25 s every time, and Teams asked the user to pick
  their account in it. In the second sync the hidden window opened after the
  list's did not open a recap for its first 10 minutes. The user's session had
  locked by the time recaps opened. Still open: what that window waited on.
  The sync now reads in the list's window, and the waiting log line will say
  if it ever stalls.
- A read tells a recap Teams opened without AI notes (not ready) from one it
  never opened (failed). It cannot tell "still generating" from "no AI notes
  for this meeting": both count as "not ready" and retry on every sync until
  the overlap window passes them.
- The stamp in a recording's file name is not always this machine's local
  time, which `teams_recap_metadata.rs` assumes. Of the 23 files the list named
  in November 2026, 11 carry a `UTC` stamp equal to the recorded time. Of the 12
  local stamps, 6 are this machine's zone (UTC−6) and 6 are an hour or two later
  (UTC−5 and UTC−4), presumably the zone of whoever recorded. The sync tries the
  row's exact time first, both to match and to date the meeting, and the
  recording rule tries SharePoint's `created` before an unmarked stamp, so this
  matters where the stamp is the only time: a pasted link, which then reads an
  hour or two off, usually misses the live
  meeting's window and costs a second meeting. It can reach a wrong attach only
  if a meeting sharing the thread or title was recorded live at that wrong hour.
  Still open: how to tell the recorder's zone from the file.
- How a Spanish Teams names a transcript-only file and the parts after the
  first. Neither has been seen. Until one is, a Spanish transcript file gives no
  stamp and keys on its own path, and a later part counts as a part only if it
  ends in a space and a number, as in English.
- What SharePoint's `created` means for a recording. The code comments call it
  the recording's end. In the one real case that can be read back it equals the
  file's UTC stamp, so it may be the start. The recording rule works with either
  as long as the file appears close to the meeting, but a value at the end can
  land in the next slot when two meetings with the same subject run back to back.
- How many rows the Recap app keeps. Microsoft's admin documentation for the
  Meeting Recaps app says it shows recaps from the previous 30 days, which
  matches the default backfill. The list check stops at the since date, so the
  end of the list has not been seen. If it is so, the since date cannot reach
  further back and the settings UI should say so.
