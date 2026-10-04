# Windows: database check before running a new build

**Audience:** the coding agent (for example GitHub Copilot) running on the
Windows build machine. Follow the steps in order. **Stop and report** whenever
a step says so; do not improvise around a failed check.

## Why this exists

Meetily upgrades its local SQLite database on startup by applying the files in
`frontend/src-tauri/migrations/`. Applied versions are recorded in the
database table `_sqlx_migrations`. Two rules follow:

1. A build **can** open a database whose applied versions are all present in
   its own `migrations/` folder (it applies any missing newer ones).
2. A build **cannot** open a database that has an applied version missing from
   its folder. It refuses to start ("migration … was previously applied but is
   missing"). So once a newer build has upgraded the database, an older build,
   or a build of a branch that lacks those files, will not open it.

As of 2026-10-04, `main` contains every migration on
`codex/reapply-local-features` plus three more, and the branch
`enhance/meeting-tags-and-dates` adds one more on top of `main`:

| Version | File | On codex | On main | On enhance/meeting-tags-and-dates |
| --- | --- | --- | --- | --- |
| 20260919000000 | add_whisper_vocabulary_hint | no | yes | yes |
| 20261004000000 | add_meeting_transcription_incomplete | no | yes | yes |
| 20261004000100 | add_remote_vocabulary_opt_in | no | yes | yes |
| 20261004000200 | add_meeting_tags | no | no | yes |

Running that branch is therefore a one-way door for this database: afterwards
only builds that contain all four files can open it, unless the backup from
step 2 is restored.

## Ground rules

- Do not start `meetily.exe` until step 6.
- Never edit the live database. Every read in this document runs on the
  **backup copy**.
- Do not run `git reset --hard`, `git clean`, `git checkout -- .`, or delete a
  stash. This checkout may carry uncommitted local work, such as a Whisper GPU
  patch, that exists nowhere else.
- Do not write meeting titles, transcript text, or summaries into the report.
  Versions, counts, paths, hashes and PASS/FAIL only.
- At the end, write the report described in step 7 to the file the user names.

## Step 1: Stop Meetily and find the database

```powershell
Get-Process meetily -ErrorAction SilentlyContinue | Stop-Process
Start-Sleep -Seconds 2
if (Get-Process meetily -ErrorAction SilentlyContinue) { throw 'Meetily is still running' }

$data = Join-Path $env:APPDATA 'com.meetily.ai'
$item = Get-Item -LiteralPath $data -Force
$item | Select-Object FullName, LinkType, Target
$db = Join-Path $data 'meeting_minutes.sqlite'
Test-Path -LiteralPath $db
```

`com.meetily.ai` may be a junction to a data drive. Record `LinkType` and
`Target` in the report. **Stop and report** if `meeting_minutes.sqlite` does
not exist: the app may be using a relocated storage folder, and the user must
point you to it.

## Step 2: Back up the data folder

Copy the database together with its `-wal` and `-shm` files. Recent changes
can live only in the WAL file. Also copy the JSON settings stores.

```powershell
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$backup = Join-Path $env:USERPROFILE "meetily-backups\$stamp"
New-Item -ItemType Directory -Force -Path $backup | Out-Null
Get-ChildItem -LiteralPath $data -File |
    Where-Object { $_.Name -like 'meeting_minutes.sqlite*' -or $_.Extension -eq '.json' } |
    Copy-Item -Destination $backup
Get-ChildItem -LiteralPath $backup | ForEach-Object {
    [pscustomobject]@{ Name = $_.Name; Bytes = $_.Length; Sha256 = (Get-FileHash -LiteralPath $_.FullName).Hash }
}
```

Recordings and meeting folders are not touched by a database upgrade and are
not part of this backup.

## Step 3: Read the applied versions from the backup copy

Use whichever is available: `sqlite3.exe`, or Python's built-in `sqlite3`
module. **Stop and ask the user** if neither is installed. Do not download
tools without their approval.

```powershell
$copy = Join-Path $backup 'meeting_minutes.sqlite'

# Option A: sqlite3.exe
sqlite3 $copy "PRAGMA integrity_check; SELECT version, success FROM _sqlx_migrations ORDER BY version;"

# Option B: Python
python -c "import sqlite3,sys; c=sqlite3.connect(sys.argv[1]); print(c.execute('PRAGMA integrity_check').fetchone()[0]); [print(v, s) for v, s in c.execute('SELECT version, success FROM _sqlx_migrations ORDER BY version')]" $copy
```

**Stop and report** if the integrity check is not `ok`, or if any row has
`success` = 0.

## Step 4: Compare with the branch you are about to build

```powershell
cd <repo root>   # for example D:\codex\meetily-0.4.0
git fetch origin
$branch = 'enhance/meeting-tags-and-dates'
$known = git ls-tree --name-only "origin/$branch" frontend/src-tauri/migrations/ |
    ForEach-Object { (Split-Path $_ -Leaf).Split('_')[0] }
$known
```

Put the versions from step 3 in `$applied`, then:

```powershell
$applied | Where-Object { $_ -notin $known }   # must print nothing
```

- **Nothing printed:** safe to build `$branch`. Continue.
- **Any version printed:** **stop and report it.** The database was upgraded
  by a build that has changes this branch lacks. Building and running this
  branch would fail to start. The branches must be reconciled first.

## Step 5: Switch branches without losing local work

```powershell
git status --porcelain
git stash list
git branch --show-current
```

Record all three outputs. If there are local changes, save an extra copy
before doing anything else:

```powershell
git diff --binary > (Join-Path $backup 'local-changes.patch')
git ls-files --others --exclude-standard > (Join-Path $backup 'untracked-files.txt')
```

Then switch branches. Git carries uncommitted changes across when they do not
conflict, and refuses otherwise:

```powershell
git switch $branch
```

If `git switch` refuses, **stop and report** the files it names. Do not force
it.

Build with the existing script. It stashes local changes, fast-forwards the
branch, and re-applies them; if they don't re-apply cleanly it restores the
checkout and builds nothing.

```powershell
pwsh -File scripts\windows\build-vulkan.ps1 -Branch $branch -VulkanSdk <sdk> -LlamaHelper <path> -ReportPath <report file>
```

Use the same `-VulkanSdk`, `-LlamaHelper` and other parameters as previous
builds on this machine. The existing `Start-Meetily-Vulkan.local.cmd` shows
them.

## Step 6: First run and verification

1. Start Meetily the usual way.
2. Confirm it opens and the existing meetings are listed.
3. Close it and re-run the step 3 query on the **live** database (read-only).
   `20261004000200` should now be present with `success` = 1.
4. Smoke test the new features: add a tag to a meeting, save it to Obsidian,
   and check that the note's front matter lists `meetings` plus the tag and
   that the sidebar shows the purple gem icon for that meeting.

## Rollback

`target\release\meetily.previous.exe` is the previous build, but **it cannot
open the upgraded database** (rule 2). To roll back:

1. Stop Meetily.
2. Move the current `meeting_minutes.sqlite`, `-wal` and `-shm` aside. Do not
   delete them.
3. Copy the three files from `$backup` back into the data folder.
4. Start `meetily.previous.exe` (or `git switch` back to the previous branch
   and rebuild).

Meetings recorded with the new build after the upgrade are in the files moved
aside in step 2, not in the backup. Tell the user before rolling back so they
can decide.

## Step 7: Report

Write a short Markdown report to the file the user names. If the file's first
line is a path header, keep it. Include:

- data folder path, junction target, and backup folder with file hashes
- integrity check result and the applied versions list (before and after)
- `git status`/stash/branch before switching, and where the patch copy is
- the step 4 decision, the build script's PASS/FAIL summary, the step 6
  results
- anything you stopped on, with the exact error text
