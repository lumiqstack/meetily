# Teams Recap Sync Manual QA

Use this checklist on Windows, signed in to Teams in Meetily's recap window (the same WebView2 profile as the recap link import). The rules it checks are in `docs/TEAMS_RECAP_SYNC_SPEC.md`.

## Setup

- Build and launch as `docs/windows/DATABASE_UPGRADE_CHECK.md` describes.
- Set an Obsidian vault in Meetily preferences. Use an empty test vault for the first-sync scenarios.
- Have Copilot recaps for meetings in the last 14 days, including two occurrences of one recurring meeting and, if possible, one meeting whose recap is not ready yet.
- Note how many meetings Meeting Recaps (the Teams Recap app on the app bar) shows on its Home tab for the week. The list check below is compared against this number.
- Find `Teams recaps` under `Obsidian Vault` in preferences. It shows the last sync date and the known recap count.

## Availability

- No vault set: `Sync Teams recaps` and `Check recap list` are disabled, with the note `Set an Obsidian vault first.`
- macOS or Linux build: both buttons are disabled, with the note `Available on Windows.`
- `Start from` is visible until the first sync completes and is hidden after it. Leaving it empty means the last 30 days.

## List Check

- `Check recap list` imports nothing: no new meeting, no new note, last sync date unchanged, known recap count unchanged.
- The toast reports how many rows have a recap link, how many do not, and where the probe file was saved.
- The counts match the Home list for the scan window, including rows in collapsed groups and rows further down that only load on scrolling. A meeting listed twice, once for its transcript and once for its recording, counts once.
- The Teams window stays hidden when the session is still signed in, and closes itself when the list has been read.
- Signed out: the window appears titled `Teams recaps — sign in, then open Meeting Recaps`. After signing in, the list is read and the window closes.
- The Teams desktop app does not open at any point.
- Closing the window early reports that it was closed before the list was read, and writes nothing to the sync state.
- A page with no recognisable list ends in `Teams did not show the Meeting Recaps list`, never in a short list reported as complete.
- `teams_recap_list_probe.json` appears in the app data folder. It holds the candidates (titles, times and recap links, whose file paths name the OneDrive each file sits in) and the listing's counts and timings, and no token, summary or transcript.

## First Sync

- Set `Start from` to 14 days back and press `Sync Teams recaps`. Progress shows done and total counts with the current title.
- The toast reports new, updated, unchanged, not ready and failed counts, and they add up to the scanned count.
- The vault holds one note per meeting in `Meetings`, with `meeting_id` in its front matter and an `Open original Teams recap` link at the end of the summary. A recap link that names its recording gives the note the name `YYYY-MM-DD Title.md`.
- Each new meeting appears in Meetily without a restart, with the recap as its summary and no transcript.
- Two occurrences of a recurring meeting are two meetings and two notes, each with its own date.
- A row with no recap link counts as not ready and creates nothing.
- The last sync date updates, and `Start from` disappears.
- A sync that reads no recap at all (every row not ready or failed) leaves the last sync date and `Start from` as they were.

## Repeat Sync

- Press `Sync Teams recaps` again straight away: every recap counts as unchanged, no note's `exported` time moves, and the meeting count stays the same.
- A second press while a sync runs is not possible: the buttons are disabled. A second call reports `A Teams recap sync is already running.`
- Quit Meetily in the middle of a sync, relaunch and sync again: no duplicates, and the recaps not reached the first time are imported.

## Recap Changed In Teams

- Edit a recap in Teams and sync: one updated, the note is overwritten at the same file name.
- Tags added in Meetily survive the overwrite. Edits made to the note in Obsidian do not, as the spec decides.
- Rename the meeting in Meetily and sync after a recap change: the note keeps following the meeting, with no second file.

## Existing Meetings

- Import a meeting's recording through SharePoint sync, then sync recaps: its recap attaches to that meeting. One meeting, one note, transcript and title kept, recap as the summary.
- Paste a recap link the sync already imported into the import dialog: the toast says `Copilot recap attached to` the meeting, and no second meeting or note appears.
- Paste the same link twice: the second import attaches to the first.

## Failures

- Turn off the network during a sync: the recaps it reaches count as not ready or failed, and none of them is written. A failure toast names the first failed recap. Sync again with the network back: they import, and the others stay unchanged.
- Point the vault at a missing folder and press `Sync Teams recaps`: the sync refuses before reading Teams, with the same message as the link import.
- A recap with no AI notes yet counts as not ready on every sync until Teams finishes it, then counts as new. No window appears for it, and it takes about a minute.
- Delete an imported meeting in Meetily and sync: an unchanged recap stays skipped and does not bring the meeting back.

## Privacy And Diagnostics

- The log's `[recap-list]` and `[recap-sync]` lines hold states, counts, and meeting titles or error messages on failures. They never hold recap text or recap links.
- `teams_recap_sync.json` holds the since date, the last sync date, and for each recap its key, meeting id, time and content hash. It holds no recap text.

## Live Recordings

Record these from the calendar prompt with classic Outlook running, so the meeting stores its calendar event. Wait for Teams to publish each recap before syncing.

- Record a Teams meeting live, let Meetily summarize it, then sync recaps: its recap attaches. One meeting, one note at the same file name, transcript, title and tags kept, recap as the summary, and the sync counts it as updated, not new.
- The note's front matter now has `import_source: live-recording` and `recorded_at`.
- Sync again: unchanged. Edit the recap in Teams and sync: updated, on the same meeting.
- Record a meeting live and paste its recap link into the import dialog instead: the toast says `Copilot recap attached to` the meeting.
- Record two occurrences of a recurring meeting live, then sync: each recap attaches to its own occurrence.
- A recap whose meeting was recorded live under a different title (rename it in Meetily first) still attaches when the calendar event's join link and the recap share a thread id.
- A `/meet/<code>` invitation or an Outlook event with no join link attaches when the recap's title agrees with the event's subject, also after the summary has renamed the meeting.
- Be double-booked: record meeting A live while meeting B runs in the same slot, and sync B's recap. If A's join link carries a thread id, B is a new meeting with no warning. If not, and the titles differ, B is a new meeting, and the toast says it may duplicate a live recording. Meeting A keeps its own summary in both cases.
- Stop and restart the Meetily recording during one call, then sync: the recap is its own meeting, the toast says it may duplicate a live recording, and both parts are untouched.
- Record a Teams call that has no calendar event, then sync: the recap is its own meeting, with no warning.
- The log's line for a possible duplicate holds meeting ids only.

## Recordings Of Live Meetings

The build that brings this adds a migration, so follow `docs/windows/DATABASE_UPGRADE_CHECK.md` before its first launch. Set the SharePoint hub and leave automatic processing on, so a scan runs every 30 minutes. Record these from the calendar prompt with classic Outlook running, and have Teams record the same call. Wait for the file to appear on SharePoint before judging a scan.

### Linking

- Record a Teams meeting live, let Meetily summarize it so that its title changes, and wait for a scan. Nothing downloads, no new meeting appears, and the log says `linked a recording to live meeting`. The meeting keeps its title, date, tags, transcript and summary.
- `sharepoint_sync.json` lists the file under `imported`. Later scans leave it alone, and the last sync date moves past it.
- Scan in the import dialog's SharePoint tab: the file shows `Recorded live` and starts unticked.
- Sync recaps, or paste the recap link: the recap attaches to the live meeting. Its note has `import_source: live-recording`, `recorded_at`, and the file's name and URL.
- Have a colleague in another time zone record the Teams side, so that the stamp in the file name is not your local time. The file still links.
- Have a colleague whose Teams is in Spanish organize and record a meeting you record live, so that the file is `…-Grabación de la reunión.mp4`. The file still links, and its recap attaches to the same meeting.
- Record a meeting of more than 4 hours, so that Teams writes `…-Meeting Recording.mp4` and `…-Meeting Recording 2.mp4`. Neither downloads, and there is one meeting. Paste a recap that names the second part: it attaches to that meeting.

### The Other Order

- Record a meeting live and sync its recap before the file appears, so that the recap attaches first. When the file appears, a scan imports nothing and the file shows `Recorded live`.
- Delete the live meeting after that scan. Later scans do not bring the recording back.

### Staying Apart

- Be double-booked: record meeting A live while someone records meeting B in the same slot. B imports as its own meeting, and the log's `may duplicate live meetings` line names A's id and no titles. A is untouched.
- Stop and restart the Meetily recording during one call. The file imports as its own meeting, the same log line names both parts, and both parts are untouched.
- Record a Teams call that has no calendar event. Its file imports as its own meeting, with no such log line.
- Upload a video to a team site, titled like the subject of a meeting you recorded live, with a creation time in that slot. It imports as before, because its file name is not a Teams recording's.
- Paste the link of a recording you recorded live into the import dialog. It imports as its own meeting: a pasted link is a request to import that file.

### Undoing

- Tick a `Recorded live` file in the dialog and import it. It becomes its own meeting with its own transcript, and the live meeting keeps everything it had. Paste that meeting's recap link: the toast names the new meeting, not the live one.

### Timing

- Choose a remote transcription provider, start a recording, and let a scan run while a new file waits on SharePoint. The scan imports nothing. After the recording is saved, the next scan links or imports the file.
