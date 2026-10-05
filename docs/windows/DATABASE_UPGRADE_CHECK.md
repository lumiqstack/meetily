# Windows: build, check, first launch

**For the coding agent on the Windows laptop.** Follow these steps in order.
Keep your replies short. Paste script output as-is; do not summarize it.

## Rules

1. Never run `git reset --hard`, `git clean`, `git checkout -- .`,
   `git push --force`, or `git stash drop`. Never delete files.
2. Never open or edit the database yourself. Only the scripts below touch it.
3. Never run `pnpm tauri build` or create MSI/NSIS installers.
4. Never print meeting titles, transcript text, or summaries.
5. **When a step says STOP: write the report (step 6), tell the user, and end.
   Do not try to fix it.** The user will take the report to another assistant.

Why the order matters: building does not touch the user's data. **The first
launch of a new build upgrades the database, and older builds cannot open it
afterwards.** So step 2 runs between building and launching.

## Step 0: Local changes

```powershell
cd <repo root>      # e.g. D:\codex\meetily-0.4.0
git status --short
git branch --show-current
```

If the branch is not `main`, run `git switch main`. If git refuses, STOP.

## Step 1: Build (do not launch)

Run the build exactly as before, with the same paths, but **without
`-Launch`**:

```powershell
pwsh -File scripts\windows\build-vulkan.ps1 -Branch main -VulkanSdk <same as before> -LlamaHelper <same as before> -ReportPath <report file>
```

The values used last time are in `Start-Meetily-Vulkan.local.cmd`. If you
cannot find them, ask the user. If the build fails, STOP.

## Step 2: Check before launching

```powershell
pwsh -File scripts\windows\meetily-check.ps1 -Mode Before -Branch main
```

It stops Meetily, backs up the data, and checks that the new build can open
the database.

- `VERDICT: STOP` or `SCRIPT ERROR`: STOP.
- `VERDICT: PASS`: show the user the summary and ask: **"Backup done and the
  check passed. OK to launch the new build?"** Wait for a yes.

## Step 3: First launch and test (the user does this)

Note the current time, then run `Start-Meetily-Vulkan.cmd`. Ask the user to:

1. confirm the existing meetings are listed,
2. add a tag to a meeting and click **Obsidian**,
3. transcribe something short (record or import ~1 minute) to exercise
   Whisper on the GPU,
4. close Meetily and tell you when done.

## Step 4: Check after

```powershell
pwsh -File scripts\windows\meetily-check.ps1 -Mode After -Since '<time noted in step 3>'
```

## Step 5: Ask the user what they saw

Ask, and record the answers in the report:

- Did the meetings, tags and Obsidian export work?
- Did the transcription finish? Roughly how long did it take?
- Any crash, freeze or error message?

## Step 6: Report

Write this to the report file the user names. If its first line is a path
header, keep it.

```markdown
## Meetily Windows run <date>
- Step 0: branch <name>, <n> local changes
- Step 1 build: PASS/FAIL <one line from build report>
- Step 2 check: <paste the meetily-check Before summary>
- Step 3/5 user test: meetings ok? tags/Obsidian ok? transcription finished? time? crashes?
- Step 4 check: <paste the meetily-check After summary>
- Stopped at: <step and exact error text, or "completed">
```

## Undoing the upgrade (only if the user asks)

The previous build cannot open the upgraded database. To go back, the user
must restore the backup folder printed in step 2 (database and its `-wal` and
`-shm` files) while Meetily is closed. Meetings recorded after the upgrade are
not in that backup. Do not do this yourself; tell the user.
