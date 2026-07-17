# Next Improvements — codex/reapply-local-features

Proposals identified on 2026-07-08 after reviewing the branch and the (then-uncommitted)
concurrent import/retranscription work. Each item is self-contained so it can be picked up
in a separate session.

## Context

The branch layers local features back onto v0.4.0: realtime transcription, Teams detection,
Obsidian export, remote OpenAI-compatible transcription provider, Copilot CLI summary
provider, and the migration-drift startup fix (`f8e54e9`, crash documented in
`meetily-crash-diagnostics.txt`).

The working-tree changes at time of review added:

- Per-job import IDs (`import-<uuid>`), with `ImportProgress` / `ImportResult` /
  `ImportError` / `ImportWarning` events all carrying `import_id`.
- Remote (`openaiCompatible`) jobs run concurrently; local Whisper/Parakeet jobs stay
  exclusive via `IMPORT_IN_PROGRESS` / `RETRANSCRIPTION_IN_PROGRESS`.
- Per-job cancellation (`CANCELLED_IMPORTS` / `CANCELLED_RETRANSCRIPTION_MEETINGS` sets)
  plus a backward-compatible cancel-all flag.
- `BackgroundImportListener` in `frontend/src/app/layout.tsx` showing completion/error
  toasts for backgrounded remote imports and retranscriptions.

Key files:
- `frontend/src-tauri/src/audio/import.rs` — ImportGuard, job registry, commands
- `frontend/src-tauri/src/audio/retranscription.rs` — RetranscriptionGuard (near-copy)
- `frontend/src-tauri/src/audio/recording_commands.rs` — recording start paths
- `frontend/src-tauri/src/audio/common.rs` — `acquire_engine_lifecycle_lock()`
- `frontend/src/app/layout.tsx` — `BackgroundImportListener`
- `frontend/src/hooks/useImportAudio.ts` — per-job event filtering, cancel

---

## 1. Re-examine the removed engine lifecycle lock (highest risk — correctness)

**Status: DONE (2026-07-08).** Resolved via `audio/engine_coordinator.rs`: a synchronous
single-holder claim (`LocalEngineCoordinator`) shared by recording (claimed only when
realtime transcription is enabled, released in `stop_recording`), `ImportGuard`, and
`RetranscriptionGuard` (local jobs only; remote jobs skip it). Replaces the old
`IMPORT_IN_PROGRESS` / `RETRANSCRIPTION_IN_PROGRESS` flags and also closes the previously
unhandled local-import ↔ local-retranscription conflict. Never held across `.await`, so
the deadlock that motivated removing the lifecycle lock from recording start cannot recur.
Unit-tested in `engine_coordinator.rs`, `import.rs`, and `retranscription.rs`.

**Problem**: The diff deleted `acquire_engine_lifecycle_lock()` from both recording start
paths in `recording_commands.rs` (`start_recording_with_meeting_name` and
`start_recording_with_devices_and_meeting`), while `common.rs:220-225` still uses that lock
for engine load/unload. Recording with realtime transcription and local
imports/retranscriptions share the same global Whisper/Parakeet engines, and nothing in the
new `ImportGuard` coordinates with `IS_RECORDING`. A user starting a recording mid-local-import
can contend for or swap the loaded model mid-transcription.

**Task**:
1. Find out why the lock was removed (likely a deadlock with background-spawned import
   tasks holding it across `.await`).
2. Re-establish mutual exclusion between local-engine consumers another way — e.g. local
   `ImportGuard::acquire` fails while `IS_RECORDING` is true, and recording start fails
   while a local import/retranscription is active — or restore the lock with a
   deadlock-safe scope.
3. Whatever the outcome, leave a comment explaining the coordination story.

## 2. Background job visibility and control (UX)

**Status: DONE (2026-07-08).** Resolved via a TDD'd `BackgroundJobStore`
(`frontend/src/lib/background-jobs.ts`, tests in `frontend/tests/lib/background-jobs.test.ts`)
plus `BackgroundJobToastProvider` (`frontend/src/components/shared/BackgroundJobToast.tsx`),
which replaces `BackgroundImportListener` in `layout.tsx`. Backgrounded remote imports and
retranscriptions now show a per-job progress toast (top-right, `DownloadProgressToast`
pattern) with a cancel button wired to `cancel_import_command` /
`cancel_retranscription_command`; a cancel that the backend reports back via
`import-error("Import cancelled")` renders as "Cancelled", not a failure. Completion side
effects (analytics, pinned summary language, `refetchMeetings`, the
`meetily-background-retranscription-complete` window event) are preserved.

**Problem**: `BackgroundImportListener` (layout.tsx) only reacts to `import-complete` /
`import-error`. Once the dialog closes, a running remote job has no visible progress and no
cancel path — cancel only exists inside the still-open dialog's `useImportAudio` instance.

**Task**: Add a per-job progress toast or small jobs tray, using the existing
`DownloadProgressToastProvider` pattern as the template. The backend already emits
`import-progress` keyed by `import_id`; wire a cancel button to
`cancel_import_command({ importId })`. Same for background retranscriptions
(`retranscription-progress` keyed by `meeting_id`, if emitted).

## 3. Cancel-all race in guard logic (correctness, small)

**Status: DONE (2026-07-09).** The sticky globals are gone from both files. Cancel-all
already snapshotted the active set into `CANCELLED_IMPORTS` /
`CANCELLED_RETRANSCRIPTION_MEETINGS`, so the fix was to stop ORing `IMPORT_CANCELLED`
into `is_import_cancelled` and delete both write-only statics (in `retranscription.rs`
the flag was already dead code — never read). Regression-tested in both files:
`job_started_during_cancel_all_drain_is_not_cancelled` proves a job acquired while a
cancel-all is still draining runs normally.

**Problem**: In `import.rs`, cancel-all sets the global `IMPORT_CANCELLED` flag, cleared
only when the *last* active job drains (`Drop` checks `no_active_jobs`). `is_import_cancelled`
ORs that global flag, so a job started while a cancel-all is still draining is cancelled
instantly even though the user never asked — `acquire` clears the per-job flag but cannot
clear the global one. Same pattern in `retranscription.rs`.

**Task**: Remove the sticky global bool. On cancel-all, extend `CANCELLED_IMPORTS` with a
snapshot of the active set and don't keep a global flag (or use a generation counter
captured at acquire time). Apply to both files.

## 4. Cap remote concurrency (robustness)

**Status: DONE (2026-07-16).** Resolved via `audio/remote_concurrency.rs`: a static
`tokio::sync::Semaphore` capping concurrent remote jobs at 3
(`MAX_CONCURRENT_REMOTE_JOBS`), shared across imports and retranscriptions since both
consume the same local decode CPU and remote endpoint. `try_acquire_remote_slot()` is
fail-fast (matching the `engine_coordinator` style): jobs beyond the cap are rejected at
`ImportGuard::acquire` / `RetranscriptionGuard::acquire` with a clear message rather than
queued, and the RAII `RemoteJobPermit` held by the guard frees the slot when the job
drains. Local jobs don't consume remote slots. TDD'd: unit tests in
`remote_concurrency.rs`, `import.rs` (cap rejection, slot reuse, local-job independence),
and `retranscription.rs` (cap rejection, shared-cap behavior).

**Problem**: Remote jobs skip the local engine but still decode audio locally (CPU-heavy)
and hit the remote endpoint with no limit. Dropping ~20 files starts 20 simultaneous
decode + upload pipelines.

**Task**: Add a small semaphore (2–3 concurrent remote jobs, rest queued or rejected with
a clear message) around the remote import/retranscription path. `tokio::sync::Semaphore`
via `tauri::async_runtime` is the natural fit.

## 5. Consolidate and test the guard machinery (maintainability)

**Status: DONE (2026-07-16).** Resolved via `audio/job_registry.rs`: a TDD'd `JobRegistry`
(active set + cancel set + engine claim / remote permit, RAII `JobGuard` cleanup) that both
modules now instantiate as a `Lazy` static (`IMPORT_JOBS` keyed by import ID,
`RETRANSCRIPTION_JOBS` keyed by meeting ID) — the bespoke `ImportGuard` /
`RetranscriptionGuard` state machines are deleted and each module keeps only thin wrappers.
Error messages are parameterized (`kind_title`/`kind`/`id_noun`) and byte-identical to the
old ones. Unit tests cover: acquire/drop lifecycle, double-acquire same ID,
local-engine exclusivity, remote-cap sharing, single-cancel + drain cleanup, stale
cancel-flag clearing on ID reuse, cancel-all drain (mutation-validated against the old
sticky-flag bug), and poisoned-lock recovery (mutation-validated against `.unwrap()`).
The pre-existing tests in `import.rs` / `retranscription.rs` still pass unchanged in
substance, now exercising the registry through the module wrappers.

**Problem**: `ImportGuard` and `RetranscriptionGuard` are near-identical ~80-line state
machines (active set + exclusive local flag + cancel set + drain-time cleanup) that will
drift apart.

**Task**: Factor into one shared job-registry type (e.g. `audio/job_registry.rs`).
Unit-test it: double-acquire same ID, remote-vs-local exclusivity, cancel-all drain
behavior, poisoned-lock recovery (`unwrap_or_else(|e| e.into_inner())`). There is already a
`#[cfg(test)]` module in `import.rs` to build on.

## 6. Housekeeping

- Delete `meetily-crash-diagnostics.txt` — it documents the migration crash already fixed
  by `f8e54e9`; it's untracked and shouldn't be committed.
- Commit the concurrent-import working-tree changes (~700 lines) as their own logical unit
  before starting the items above (may already be done by the time this is picked up —
  check `git log`).

## 7. Longer-term: job persistence across restarts

Background jobs are fire-and-forget in-memory. If the app quits mid-import, the job
vanishes and a partially-created meeting may be left behind with no resume or cleanup path.
Options: persist a job table in SQLite and reconcile on startup (mark orphaned meetings,
offer retry/cleanup), or at minimum detect and flag partial meetings on launch. Bigger
design question — scope before implementing.

---

**Suggested order**: 1 and 3 first (correctness in the current diff), then 2, 4, 5, 6, 7.
