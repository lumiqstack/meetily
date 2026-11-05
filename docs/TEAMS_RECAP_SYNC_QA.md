# Teams Recap Sync Manual QA

Use this checklist on Windows, signed in to Teams in Meetily's recap window (the same WebView2 profile as the recap link import). The rules it checks are in `docs/TEAMS_RECAP_SYNC_SPEC.md`.

## Setup

- Build and launch as `docs/windows/DATABASE_UPGRADE_CHECK.md` describes.
- Set an Obsidian vault in Meetily preferences. Use an empty test vault for the first-sync scenarios.
- Have Copilot recaps for meetings in the last 14 days, including two occurrences of one recurring meeting and, if possible, one meeting whose recap is not ready yet.
- Note how many meetings with a recap the Meet app shows for the week. The list check below is compared against this number.
- Find `Teams recaps` under `Obsidian Vault` in preferences. It shows the last sync date and the known recap count.

## Availability

- No vault set: `Sync Teams recaps` and `Check recap list` are disabled, with the note `Set an Obsidian vault first.`
- macOS or Linux build: both buttons are disabled, with the note `Available on Windows.`
- `Start from` is visible until the first sync completes and is hidden after it. Leaving it empty means the last 30 days.

## List Check

- `Check recap list` imports nothing: no new meeting, no new note, last sync date unchanged, known recap count unchanged.
- The toast reports how many rows have a recap link, how many do not, and where the probe file was saved.
- The counts match the Meet app's list for the scan window, including rows further down that only load on scrolling.
- The Teams window stays hidden when the session is still signed in, and closes itself when the list has been read.
- Signed out: the window appears titled `Teams recaps — sign in and open the Meet app's Recap list`. After signing in, the list is read and the window closes.
- The Teams desktop app does not open at any point.
- Closing the window early reports that it was closed before the list was read, and writes nothing to the sync state.
- A page with no recognisable list ends in `Teams did not show the Meet app's recap list`, never in a short list reported as complete.
- `teams_recap_list_probe.json` appears in the app data folder, and every token value in it reads `REDACTED`.

## First Sync

- Set `Start from` to 14 days back and press `Sync Teams recaps`. Progress shows done and total counts with the current title.
- The toast reports new, updated, unchanged, not ready and failed counts, and they add up to the scanned count.
- The vault holds one note per meeting in `Meetings`, with `meeting_id` in its front matter and an `Open original Teams recap` link at the end of the summary. A recap link that names its recording gives the note the name `YYYY-MM-DD Title.md`.
- Each new meeting appears in Meetily without a restart, with the recap as its summary and no transcript.
- Two occurrences of a recurring meeting are two meetings and two notes, each with its own date.
- A row with no recap link counts as not ready and creates nothing.
- The last sync date updates, and `Start from` disappears.

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
- A recap with no AI notes yet counts as not ready on every sync until Teams finishes it, then counts as new.
- Delete an imported meeting in Meetily and sync: an unchanged recap stays skipped and does not bring the meeting back.

## Privacy And Diagnostics

- The log's `[recap-list]` and `[recap-sync]` lines hold states, counts, and meeting titles or error messages on failures. They never hold recap text or recap links.
- `teams_recap_sync.json` holds the since date, the last sync date, and for each recap its key, meeting id, time and content hash. It holds no recap text.
