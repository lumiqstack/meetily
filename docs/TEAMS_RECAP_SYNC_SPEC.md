# Teams recap sync to Obsidian (Windows)

Status: 2026-11-04. Steps 1 (identity and dedupe in the link import), 2 (sync state, commands, settings UI) and 3 (Meet app scanner with its probe mode) merged; the selectors are still being learned on the Windows machine. Step 4 (QA checklist) is `docs/TEAMS_RECAP_SYNC_QA.md`. Step 5 (matching a recap to a live recording) is in review.

## Goal

Keep the Obsidian vault complete for every Teams meeting that has a Copilot
recap, without pasting links one at a time. One button in Meetily reads the
recap list in the Teams Meet app, imports every recap it has not seen, attaches
recaps to meetings Meetily already holds from their recordings, and writes or
refreshes one vault note per meeting. No duplicates, no misses, and a count of
what happened at the end.

Windows only, like the existing recap link import. The macOS and Linux builds
keep compiling, show the button disabled with a short note, and run no sync
code.

## Decisions already made

| Question | Decision |
| --- | --- |
| Source page | The Meet app in Teams (its list of recent meetings with recaps), read in the persisted WebView2 profile the recap link import already uses. |
| Microsoft Graph | Not used. No Entra app registration, no admin consent. |
| Trigger | Manual "Sync Teams recaps" button. No schedule in this version. |
| Backfill | A since date in settings, default 30 days before the first sync. Each later sync starts from the last confirmed sync date minus an overlap window. |
| Existing meeting from the recording | Attach the recap to that meeting. One meeting, one note. Match on the recording file URL. |
| Existing meeting recorded live | Attach the recap when the meeting's calendar window fits the recap's time and its thread id or title confirms it. When that is ambiguous, import the recap separately and report it. See "Matching a recap to a live recording". |
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
  `live-recording` once a recap attaches to a live meeting),
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

## Architecture

```
Settings (since date)          Teams Meet app, hidden WebView2
        │                                  │
        ▼                                  ▼
sync_teams_recaps_command ──► RecapListScanner ──► Vec<RecapCandidate>
        │                                           (recap link, title,
        │                                            recorded_at, fileUrl)
        ▼
   for each candidate
        ├── identity: canonical fileUrl key
        ├── lookup meeting by key (recap sync state, then meeting_sources)
        ├── no key found: match a live recording by calendar window
        ├── extract recap through existing extract_teams_recap
        ├── create meeting | attach summary to existing meeting
        ├── export_copilot_recap_note (new or overwrite)
        └── record outcome in sync state
        ▼
   RecapSyncReport ──► toast
```

The scanner is new. Everything after it reuses the existing extractor, summary
repository, and Obsidian exporter.

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

### Matching a recap to a live recording

A meeting Meetily recorded live has no file URL, so the identity lookup cannot
find it and its recap becomes a second meeting. Live recordings started from
the calendar prompt do store the event they matched: `calendar_event_id`,
`calendar_start`, `calendar_end` and `calendar_join_url` on the meeting row.
This rule uses them, in both the link import and the sync loop.

The rule runs only when the identity lookup finds nothing. Identity stays first
because it is exact, and the rule writes the recap's key onto the meeting it
attaches to, so every later import of that recap goes straight there.

**When the recap happened.** A recap can carry two times. They are tried in
this order, and the second only if the first fits no live recording:

- In the sync loop, the start time the Meet app row shows, which Teams renders
  in this user's zone.
- The stamp in the `fileUrl` file name, which is when the recording started,
  read in this machine's time zone (`teams_recap_metadata.rs`).

A recap with neither, such as a pasted link without `fileUrl`, is not matched.

**Candidates.** Meetings with `calendar_start` and `calendar_end` and no
`recording_url` in `meeting_sources`. A live meeting that already holds another
recap is out, so the recap of a second recording cannot overwrite the first.

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
  compared with the meeting's title, on lowercase letters and digits only,
  because Teams drops punctuation from file names. They agree exactly when
  equal, and weakly when one is a prefix of the other of at least 8 characters,
  because long titles get truncated and users add to them.

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
- A recording imported through SharePoint after a live recording still creates
  its own meeting. The rule's matcher is reusable for it, but what to do with
  the second transcript is a separate decision.

### RecapListScanner

A second WebView2 script, `teams_recap_list.js`, driven by the same window,
poll and timeout machinery as `teams_recap.js`. It is given the since date and
returns one of:

- `{ status: "waiting" }` while the list renders or sign-in is in progress.
  The existing `launcher`, `opening_web` and `signed_in_chat` handling applies.
- `{ status: "page", items: [...], more: bool }` with every meeting row visible
  so far. An item carries the recap deep link, the title, the start time as
  shown, and whether the row shows a recap indicator.
- `{ status: "done" }` once a row older than the since date is reached or the
  list says there are no more.

The Rust side scrolls the list until `done` or a hard cap of 500 rows, then
dedupes items by key. Rows without a recap link are reported as "not ready"
and never written to the sync store.

The exact selectors are unknown until the Meet app is inspected on the Windows
machine. Learning them is the first delivery step and the main risk; the
script must fail with `waiting` and then a clear timeout error on any layout it
does not recognise, never with a partial list presented as complete.

How the layout is learned (`teams_recap_list.rs`, `teams_recap_list.js`):

- The window opens the Teams web shell (`https://teams.microsoft.com/v2/`),
  since no direct URL for the Meet app's recap tab is documented. Before any
  row counts, the script clicks, once each, a rail button named Meet or Recap
  (opening the "more apps" flyout if the rail has none) and a tab named Recap
  or similar, because the shell may open on Chat or Activity, whose rows carry
  dates too. After the silent phase the window is shown so the user can
  navigate by hand, and the script reads the list wherever it appears,
  including inside a same-origin frame.
- A row is any list item, row or article with a date it can parse or a recap
  link it can find. The link comes from an `href`, from a recap URL embedded
  in the row's markup, or, failing both, is built from a `19:meeting_…@thread.v2`
  id in the row's markup. Rows with no link count as not ready.
- In probe mode (the "Check recap list" button) the script also returns a
  compact description of the page: rail and tab names, headings, list
  containers, frames, the first rows with their lines, parsed date, link,
  attributes and id-looking React props, every `data-tid`, recap-looking hrefs
  and thread ids in the page. If no row holds a link, the probe opens one past
  row (the row itself, never a button inside it) and records what that showed.
  Token query values are redacted. The debug command writes the probe to
  `teams_recap_list_probe.json` in the app data folder. It contains meeting
  titles and page text, so it stays on that machine unless the user shares it.
- `MEETILY_RECAP_LIST_SCRIPT=<file>` replaces the bundled script with that
  file, so selectors can be tuned on Windows without a rebuild. The file must
  keep the `%SINCE%` and `%PROBE%` placeholders.

### Sync loop

For each candidate, oldest first:

1. Resolve identity and look up an existing meeting, then a live recording.
2. Call `extract_teams_recap` with the candidate's link. Outcomes:
   - ready: continue.
   - not ready (no AI notes, or Teams still generating): count "not ready",
     do not touch the store, continue.
   - error: count "failed" with the message, continue.
3. Build the recap markdown exactly as the link import does, including the
   "Open original Teams recap" link.
4. Compute `content_hash` over the markdown. If the store has the key with the
   same hash, count "skipped" and continue.
5. No existing meeting: create it through `create_meeting_with_imported_summary`
   with `import_source` `copilot-recap`, `recording_url` from `fileUrl`, and
   `recorded_at` from the link. Count "new".
   Existing meeting: write the recap as its summary through
   `update_meeting_summary`, keeping the meeting's title, tags, transcript and
   source. Count "updated" (or "new" if that meeting had no summary yet). A
   live recording also gets its `meeting_sources` row here.
6. Export the vault note through `export_copilot_recap_note` for recap-only
   meetings, or `export_meeting_note` with the stored transcript for meetings
   that have one. Both overwrite the pinned filename.
7. Record the key, meeting id, time and hash in the store.

After the loop, `last_sync_date` becomes the sync start time. The next sync
scans from `last_sync_date` minus 7 days, because a recap appears hours after
a meeting and Teams may finish generating one days later.

The whole loop holds a single `RECAP_SYNC_LOCK`; a second click while a sync
runs returns "already running". The loop is resumable: a crash midway loses
nothing because every imported recap is in the store before the next one
starts, and re-running converges to the same state.

### Dedupe in the manual link import

`api_import_copilot_recap_from_link` adopts the same identity lookup. Pasting a
link whose key is known attaches to the existing meeting and overwrites the
note instead of creating a second meeting. This lands first and on its own.

## Tauri commands

| Command | Purpose |
| --- | --- |
| `sync_teams_recaps_command` | Run one sync. Returns `RecapSyncReport { new, updated, skipped, not_ready, failed: Vec<{title, error}>, possible_duplicates: Vec<title>, scanned, meetings_folder }`. |
| `get_teams_recap_sync_state_command` | Since date, last sync date, count of known recaps. |
| `set_teams_recap_sync_prefs_command` | Since date. |
| `teams_recap_list_debug_command` | Return the raw candidate list without importing. For learning selectors and for the QA doc. |

Events: `teams-recap-sync-progress { done, total, current_title }` so the
background job toast can show progress; the existing `meetings-changed`
emission after each write.

## Settings

Under the existing Obsidian section in `PreferenceSettings.tsx`:

- "Sync Teams recaps" button with the last sync date beside it.
- Since date picker, default 30 days back, shown until the first sync completes.
- Disabled on non-Windows with the note "Available on Windows".

The button is disabled when no vault is configured, with the same message the
link import gives.

## Database

No schema change. `meeting_sources` already has the columns. Sync state lives
in the JSON store like the SharePoint sync state.

One query is added to `meeting_sources`: find a meeting id by canonical
`recording_url`. Because stored URLs are raw, the comparison decodes and
lowercases both sides in Rust over the candidate set of rows with a non-null
`recording_url`, which is small.

One query is added to `meetings`: the live recordings a recap may still attach
to, meaning those with `calendar_start` and `calendar_end` and no
`recording_url` in `meeting_sources`. The window comparison runs in Rust.
`attach_imported_summary` now keeps a replaced summary in `result_backup` unless
that summary is itself a recap.

## Privacy

Same boundary as the recap link import. The script reads the rendered DOM
only; no Tauri IPC is exposed to the Teams page; recap text is never written to
a URL, clipboard, log or external service. Logs record keys, counts and error
messages, not recap content. The since date and keys in the store are the only
persisted additions of a sync; the "Check recap list" button also writes the
probe file described above, on that machine only.

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
3. The Meet app scanner script, learned on the Windows machine through the
   Stash handoff: first the debug command until it lists the week's meetings
   correctly, then the real sync.
4. QA doc, `docs/TEAMS_RECAP_SYNC_QA.md`, mirroring
   `TEAMS_MEETING_DETECTION_QA.md`.
5. Matching a recap to a live recording, in the link import and the sync loop.

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
- Unit, `#[cfg(not(windows))]`-guarded where `mock_app` is used: the loop over a
  fake extractor returning ready, not ready and error in sequence.
- Manual on Windows: the checklist in `docs/TEAMS_RECAP_SYNC_QA.md`.

## Open questions

- Does the Meet app list show meetings whose recording lives on someone else's
  OneDrive? If not, recaps for meetings the user did not organise may be
  missing and the SharePoint scan is the only cross-check.
- Whether `extract_teams_recap` can distinguish "still generating" from "no
  recap for this meeting". If not, both count as "not ready" and retry on every
  sync until the overlap window passes them.
- Whether the stamp in a recording's file name is always this machine's local
  time. `teams_recap_metadata.rs` assumes so, but a recording started by someone
  in another zone may carry theirs. A wrong reading usually misses the window
  and costs a second meeting. On a pasted link, with no row time to try first, it
  can reach a wrong attach only if a meeting sharing the thread or title was
  recorded live at that wrong hour. The probe's rows show a stamp beside the time
  the Meet app displays, which will settle it.
- How many rows the Meet app keeps. Microsoft's admin documentation for the
  Meeting Recaps app says it shows recaps from the previous 30 days, which
  matches the default backfill; to be confirmed on the real list. If it is
  so, the since date
  cannot reach further back and the settings UI should say so.
