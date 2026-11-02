# Teams recap sync to Obsidian (Windows)

Status: 2026-11-02. Step 1 (identity and dedupe in the link import) merged. Step 2 (sync state, commands, settings UI, scanner stub) in review. Steps 3 and 4 pending.

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
| Recap changed in Teams | Overwrite the vault note. Tags survive (they come from the meeting record); other edits made in Obsidian do not. |
| Recap not ready or absent | Leave the meeting unmarked so the next sync retries. Count it as "not ready" in the report. |
| Reporting | One toast with counts: new, updated, skipped, not ready, failed. A link opens the vault Meetings folder. |
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
  (`sharepoint`, `url`, `teams-transcript`, `copilot-recap`, `file`),
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
canonical `recording_url`. A hit in either means "existing meeting".

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

### Sync loop

For each candidate, oldest first:

1. Resolve identity and look up an existing meeting.
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
   source. Count "updated" (or "new" if that meeting had no summary yet).
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
| `sync_teams_recaps_command` | Run one sync. Returns `RecapSyncReport { new, updated, skipped, not_ready, failed: Vec<{title, error}>, scanned, meetings_folder }`. |
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

## Privacy

Same boundary as the recap link import. The script reads the rendered DOM
only; no Tauri IPC is exposed to the Teams page; recap text is never written to
a URL, clipboard, log or external service. Logs record keys, counts and error
messages, not recap content. The since date and keys in the store are the only
persisted additions.

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
4. QA doc in `docs/`, mirroring `TEAMS_MEETING_DETECTION_QA.md`.

## Testing

- Unit: canonical key from recap links with and without `fileUrl`, with raw
  and encoded spaces; store round trip; report counting for each outcome;
  overlap window arithmetic; the attach path keeps title, tags and transcript.
- Unit, `#[cfg(not(windows))]`-guarded where `mock_app` is used: the loop over a
  fake extractor returning ready, not ready and error in sequence.
- Manual on Windows: fresh vault, since date 14 days back, run sync, confirm
  one note per meeting, run again, confirm all "skipped", edit a recap in
  Teams, run again, confirm one "updated" and the note overwritten with tags
  intact, import the same meeting's recording through SharePoint sync before a
  recap sync and confirm the recap attaches to it.

## Open questions

- Does the Meet app list show meetings whose recording lives on someone else's
  OneDrive? If not, recaps for meetings the user did not organise may be
  missing and the SharePoint scan is the only cross-check.
- Whether `extract_teams_recap` can distinguish "still generating" from "no
  recap for this meeting". If not, both count as "not ready" and retry on every
  sync until the overlap window passes them.
- How many rows the Meet app keeps. If it caps at a few weeks, the since date
  cannot reach further back and the settings UI should say so.
