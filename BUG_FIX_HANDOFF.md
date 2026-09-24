# Bug Fix Handoff

Use this document to start a new coding session with a low-cost model. Work on
one issue at a time. Verify the issue against the current code before editing,
keep the patch narrow, and validate immediately after each change.

## Session Prompt

Paste this prompt into the new session:

```text
Read AGENTS.md, CLAUDE.md, and BUG_FIX_HANDOFF.md before making changes.

Implement the first unchecked issue in the Recommended Order section only.
Before editing, verify the reported behavior in the referenced code and state
a short hypothesis plus the narrow check that will disprove it. Add or update a
focused regression test where practical. Preserve existing APIs and unrelated
working-tree changes. Do not modify the archived backend/ directory and do not
commit.

After editing, run the narrowest relevant test first. Then run the validation
commands listed for that issue. Report changed files, test results, and any
remaining risk. Mark the issue complete in BUG_FIX_HANDOFF.md only after its
validation passes.
```

For a specific issue instead, replace "the first unchecked issue" with its
number and title.

## Repository Constraints

- The supported app is `frontend/`: Next.js/TypeScript plus the Rust/Tauri core.
- `backend/` is archived and must not receive fixes for current app behavior.
- Do not revert unrelated user changes.
- Do not create MSI or NSIS installers.
- Recording issues 1-4 overlap. Complete and validate them sequentially.
- Rust hot-path logs should use the repository's performance-aware patterns.
- Meeting transcript text is private and must not appear in normal release logs.

## Recommended Order

- [x] **1. Correct Tauri recording argument names**

  In `frontend/src/services/recordingService.ts`, verify the JavaScript argument
  naming expected by `start_recording_with_devices_and_meeting` in
  `frontend/src-tauri/src/lib.rs`. Selected microphone, system device, and
  meeting name must reach Rust instead of becoming `None`. Add a focused service
  test if the current test setup supports mocking `invoke`.

  Validation: `cd frontend; pnpm exec tsc --noEmit`

- [x] **2. Persist the Ollama fallback model**

  In `frontend/src/app/meeting-details/page.tsx`, the `gemma3:1b` fallback branch
  currently saves an empty model. Save the detected fallback model and cover the
  configuration payload with a focused test where practical.

  Validation: `cd frontend; pnpm exec tsc --noEmit`

- [x] **3. Persist recording-folder changes**

  In `frontend/src/components/RecordingSettings.tsx`, folder selection updates
  local state and displays success without persisting the new preferences. Use
  the existing preference persistence path. Show success only after persistence
  succeeds and retain all unrelated preferences.

  Validation: `cd frontend; pnpm exec tsc --noEmit`

- [x] **4. Prevent stale meeting requests from overwriting navigation**

  In `frontend/src/hooks/usePaginatedTranscripts.ts`, requests started for
  meeting A can update state after navigation to meeting B. Add request
  cancellation or a generation/meeting-ID guard to every asynchronous state
  update, including errors and pagination. Add a race-focused hook test if the
  existing test tools support it.

  Validation: `cd frontend; pnpm exec tsc --noEmit`

- [ ] **5. Return VAD initialization failures instead of panicking**

  In `frontend/src-tauri/src/audio/pipeline.rs`,
  `ContinuousVadProcessor::new` errors are converted to `panic!`. Propagate a
  normal startup error through the owning constructor and ensure partially
  initialized recording resources are cleaned up. Add a focused failure-path
  test where dependency injection permits it.

  Validation: `cargo test -p meetily --lib audio:: --no-fail-fast`

- [ ] **6. Remove transcript text from normal release logs**

  In `frontend/src-tauri/src/audio/transcription/worker.rs` and any equivalent
  transcription result logging, avoid logging recognized text at `info` level.
  Keep only non-sensitive metadata such as worker ID, character count,
  confidence, and partial status. Search the active Rust app for other normal
  logs that print transcript contents.

  Validation: run relevant transcription tests and
  `cargo test -p meetily --lib --no-fail-fast`.

- [ ] **7. Persist meetings after transcription timeout or polling failure**

  In `frontend/src/hooks/useRecordingStop.ts`, SQLite persistence is skipped
  when transcription exceeds 60 seconds or one status poll fails. Always persist
  the meeting and available transcript after audio stop succeeds. Represent
  incomplete transcription explicitly so the background pipeline can finish or
  retry it; do not falsely mark transcription complete. Add tests for timeout,
  polling failure, and the normal completion path.

  Validation: frontend tests for the hook, then
  `cd frontend; pnpm exec tsc --noEmit`.

- [ ] **8. Serialize recording startup**

  In `frontend/src-tauri/src/audio/recording_commands.rs`, concurrent start
  commands can both observe `IS_RECORDING == false` before either stores `true`,
  then overwrite the global recording manager. Introduce a startup guard or
  atomic state transition covering all awaited initialization. Failed starts
  must restore the idle state. Add a concurrent-start regression test.

  Validation: focused recording command tests, then
  `cargo test -p meetily --lib --no-fail-fast`.

- [ ] **9. Fail recording startup when required audio saving cannot initialize**

  In `frontend/src-tauri/src/audio/recording_saver.rs`, meeting-folder or encoder
  initialization errors are logged and swallowed even when auto-save is enabled.
  Return an error before recording reports success. Transcript-only mode must
  remain valid. Add tests for unwritable output and encoder initialization
  failure where practical.

  Validation: focused recording saver tests, then
  `cargo test -p meetily --lib audio:: --no-fail-fast`.

- [ ] **10. Drain queued audio before finalization**

  In `frontend/src-tauri/src/audio/recording_saver.rs`, `stop_and_save` clears
  `is_saving` before the accumulation receiver drains its queue and relies on a
  fixed 200 ms delay. Give the accumulation task explicit ownership/lifecycle:
  close its sender, drain accepted chunks, await completion, then finalize the
  encoder. Do not replace this with a longer sleep. Add a backlog regression
  test proving the final queued windows are committed.

  Validation: focused recording saver tests, then
  `cargo test -p meetily --lib audio:: --no-fail-fast`.

- [ ] **11. Make SQLite corruption recovery non-destructive**

  In `frontend/src-tauri/src/database/manager.rs`, corruption-like open errors
  cause unconditional deletion of WAL and SHM files. A WAL may contain committed
  meetings not checkpointed into the main database. Preserve or back up the full
  database set before recovery, distinguish an orphaned sidecar from main-file
  corruption, and use SQLite-supported integrity/recovery behavior. Never delete
  the only copy of user data. Add tests around a database with committed WAL
  content and a failed reopen.

  Validation: focused database tests, then
  `cargo test -p meetily --lib database:: --no-fail-fast`.

## Baseline Validation

At review time the repository was clean and these checks passed:

- Rust library suite: 488 passed, 2 ignored, 0 failed.
- TypeScript: `cd frontend; pnpm exec tsc --noEmit` passed.
- `pnpm run lint` is not currently non-interactive because Next.js prompts to
  create an ESLint configuration. Do not treat that prompt as a code failure.

For the final integration pass, run:

```powershell
cd D:\codex\meetily-0.4.0
$env:LIBCLANG_PATH = "D:\codex\.tools\libclang.runtime.win-x64.21.1.8\runtimes\win-x64\native\libclang.dll"
$env:PATH = "D:\codex\.tools\cmake-4.3.3-windows-x86_64\bin;D:\codex\.tools\libclang.runtime.win-x64.21.1.8\runtimes\win-x64\native;$env:PATH"
$env:TAURI_GPU_FEATURE = "none"
cargo test -p meetily --lib --no-fail-fast
cd frontend
pnpm exec tsc --noEmit
```

Only when an executable build is explicitly needed, create the raw application
executable with `cargo build --release -p meetily`; do not invoke a Tauri bundle
build.