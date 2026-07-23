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

**Status: DONE (2026-07-17).** Scoping first (as the item asked): crash analysis showed
imports create their meeting folder early but commit the DB meeting row in one transaction
at the very end, and retranscriptions replace transcripts in one end-of-job transaction —
so a crash never leaves a partial DB meeting, it leaves (a) a silently vanished job and
(b) for imports, an orphaned folder with a copied audio file. Design chosen: a durable
job journal, not resume-from-checkpoint.

Implemented via a TDD'd `background_jobs` SQLite journal
(`audio/job_persistence.rs`, migration `20260716000000_add_background_jobs.sql`): one row
per in-flight job (id, kind, title, source/folder/meeting, language/model/provider,
created_at), inserted at job start in `start_import_with_guard` /
`start_retranscription_with_guard` and deleted on every in-process finish (success,
failure, cancel — all already surfaced live). Any row still present at startup is by
definition interrupted: `reconcile_interrupted_jobs` (called from
`database/setup.rs` after DB init) flags those rows and removes orphaned import folders,
guarded three ways (imports only, no `metadata.json` inside, no `meetings.folder_path`
row references it — the latter covers a crash between DB commit and metadata write).
Retranscription folders are never touched. `list_interrupted_jobs_command` /
`dismiss_interrupted_job_command` expose the journal; dismiss only deletes rows still
flagged interrupted so a rerun that reused the ID (retranscriptions key by meeting ID)
survives dismissal of its stale notice.

Frontend: `BackgroundJobStore` gained a TDD'd `interrupted` status plus
`registerInterrupted` / `dismissInterrupted` / `retryInterrupted` (retry re-invokes the
original start command with the journaled settings, then drops the stale notice and
registers the fresh job so the existing progress/completion listeners drive it).
`BackgroundJobToastProvider` queries the journal on mount and renders a sticky per-job
toast with Retry/Dismiss. Rust: 10 unit tests in `job_persistence.rs` (in-memory SQLite
running the real migrations; mutation-validated where behavior predated its test).
Frontend: 6 new store tests in `tests/lib/background-jobs.test.ts`.

## 8. End-to-end crash-recovery verification (quality)

**Status: DONE (2026-07-17).** Driven against the real app (release exe rebuilt with the
#9 fix, run over the user's live `com.meetily.ai` data). Verified: (a) the
`20260716000000` migration applied cleanly to a real grown DB whose newest migration was
`20260706000000`; (b) the app survives `taskkill /f`; (c) after injecting a crashed
import (orphan folder + journal row) and a crashed retranscription (journal row pointing
at a real meeting folder), a relaunch flagged both rows `interrupted = 1`, logged "Found
2 background job(s) interrupted by a previous shutdown", removed the orphan folder
(logged), and left the real meeting folder byte-count-identical; (d) startup ordering is
race-free by construction — reconcile runs under `block_on` inside Tauri's `setup` hook
(`lib.rs`), which completes before the webview loads, so the frontend cannot query
before rows are flagged; (e) `list_interrupted_jobs_command` /
`dismiss_interrupted_job_command` are registered in `lib.rs` and invoked by name in
`BackgroundJobToast.tsx` / `background-jobs.ts`; (f) the 25 `background-jobs` store
tests (covering interrupted/retry/dismiss state transitions) pass. Injected rows and
scratch files were cleaned up afterwards.

Not exercised (requires interactive UI/a live remote endpoint): visually observing the
sticky toast, and clicking Retry through a full re-import. These are covered by the
store tests plus the verified command contract; a future manual pass can close them.

**Problem**: The crash-recovery path from #7 is unit-tested at the module level
(`job_persistence.rs` against in-memory SQLite, store tests for the frontend), but the
full loop — process dies mid-import → relaunch → reconcile runs → interrupted-job toast
appears → Retry actually restarts the job / Dismiss clears it — has never been exercised
in the real app. The riskiest untested glue is exactly the part unit tests can't reach:
Tauri command registration, the startup ordering in `database/setup.rs` (reconcile must
see the journal row before the frontend queries it), and the toast lifecycle for
Infinity-duration notices.

**Task**: Drive it once manually or via the `/verify` flow: start a remote import of a
long file, kill the process mid-transcription (`taskkill /f` on Windows), relaunch, and
confirm (a) the interrupted toast lists the job, (b) the orphaned folder was removed
(check the recordings dir and the startup log line), (c) Retry re-runs the import to
completion under a new import ID, (d) Dismiss clears the notice and the
`background_jobs` row. Repeat once for a retranscription to confirm its meeting folder
survives. If any step needs fixing, capture it as a regression test where one is
possible.

## 9. Path normalization in the orphan-folder meeting guard (robustness, small)

**Status: DONE (2026-07-17).** TDD'd in `job_persistence.rs`: the exact-match
`WHERE folder_path = ?` query is replaced by fetching all meeting `folder_path`s and
comparing with `is_same_path`, a pure normalized-component comparison
(`Path::components()` equality after stripping a `\\?\` verbatim prefix, with
per-component case-folding on Windows). Four RED→GREEN cycles, each watched failing
first: mixed separators (`C:/x/y` vs `C:\x\y`), trailing separator, case difference
(Windows-gated), and `\\?\` verbatim prefix (Windows-gated). Fail-safe is preserved:
the comparison itself cannot error, and a DB error aborts reconciliation before any
deletion. Full Rust suite green (241 passed).

**Problem**: `reconcile_interrupted_jobs` protects a crashed import's folder from cleanup
when a `meetings.folder_path` row references it, but the comparison is an exact string
match (`WHERE folder_path = ?`). The journaled path and the meeting row are both written
from the same `meeting_folder.to_string_lossy()` value today, so they match — but any
future divergence (different separators on Windows, trailing separator, case difference,
`\\?\` prefix) would silently disable the guard's protection… in the safe direction only
because two other guards remain (metadata.json presence, import-kind check). The inverse
risk is the real one: a *matching* meeting stored with a differently-normalized path
would not be found, and the folder of a committed meeting could be deleted if
metadata.json also failed to write.

**Task**: Normalize both sides before comparing — e.g. compare
`std::path::Path::new(a).components()` equality or canonicalize when the paths exist —
in `job_persistence.rs`'s meeting-reference check. Unit-test with mixed separators
(`C:/x/y` vs `C:\x\y`) and a trailing-slash variant. Keep the behavior fail-safe: if
normalization or canonicalization errors, treat the folder as referenced (skip
deletion).

## 10. Speaker attribution actually visible (correctness, small)

**Status: DONE (2026-07-23).** Found by an as-built plan review of the VTT transcript
import (`17f1695`): `TranscriptSegment` accepted and rendered a `speaker` prop, but both
call sites in `VirtualizedTranscriptView.tsx` (virtualized + simple-list paths) never
passed it — so the feature's whole point, speaker labels, never rendered. Fixed by
passing `speaker={segment.speaker}` at both sites; regression-tested with a
`renderToStaticMarkup` test (`tests/components/transcript-speaker.test.tsx`,
mutation-validated: fails with the prop removed). Speaker names are now also included in
transcript copy (`TranscriptContext.tsx`, `useCopyOperations.ts`), Obsidian export, and
the transcript text fed to summary generation (`useSummaryGeneration.ts`) — all as
`Speaker: text` prefixes, absent when `speaker` is null so recordings are unaffected.

## 11. URL/transcript imports covered by crash recovery + faithful retry (correctness)

**Status: DONE (2026-07-23).** Same review: `run_transcript_import` never journaled a
`background_jobs` row (crash → no interrupted notice, orphaned folder invisible to the
reconciler), and Retry was broken for *all* URL imports — the journal had no URL/mode,
and audio-URL jobs journaled the deleted temp media path. Fixes:
- Migration `20260723000000` adds nullable `source_url` + `mode` columns; `PersistedJob`
  carries both. Scope decision: persisting the raw link locally was judged acceptable for
  a local-only privacy-first app (the DB already holds full transcripts/audio) in
  exchange for a working Retry.
- `run_url_import` journals the job before any long phase (login/download/fetch crashes
  now surface a notice); the spawn wrapper clears the row on every in-process finish;
  the audio handoff threads `source_url`/`mode` through `start_import_with_guard`.
- `run_transcript_import` records its meeting folder via `try_set_job_folder`, removes
  the folder if the DB insert fails (mirroring the audio path), and no longer leaks its
  temp work dir on read/parse errors.
- Frontend `retryInterrupted` branches: jobs with `source_url` re-run through
  `start_import_from_url_command` with the journaled mode; file imports keep the old
  path. Tested in `background-jobs.test.ts` (URL-retry case) and
  `job_persistence.rs` (`url_import_journal_round_trips_source_url_and_mode`).

## 12. Startup sweep of stale SharePoint cookie files (security hygiene, small)

**Status: DONE (2026-07-23).** `sp-cookies-<uuid>.txt` files (plaintext FedAuth/rtFa
tokens) are deleted best-effort after each import, but a crash mid-import left them in
`<app_data_dir>/tmp` indefinitely. `sharepoint::sweep_stale_cookie_files` now removes
all `sp-cookies-*.txt` at startup (called first thing in
`database/setup.rs::initialize_database_on_startup`; no import can be running that
early). Also: the no-transcript-found `warn!` no longer dumps raw yt-dlp output (which
can contain tenant manifest URLs) — the raw dump moved to `debug!`.

## 13. Merge consecutive same-speaker cues (UX, small)

**Status: DONE (2026-07-23).** The VTT plan promised merging but it was dropped in
implementation: Teams emits one short cue per clause, so imports read as a choppy list
with the speaker label repeated on every row. `vtt::merge_cues` now merges consecutive
cues with an identical speaker when the gap is ≤3 s and the merged text stays ≤500
chars (unit-tested: same-speaker merge, gap limit, length cap, speakerless-vs-named
boundaries). Applied in `run_transcript_import` after parsing.

Deferred from the same review: the Microsoft Stream transcript-API fallback (only
needed if a real tenant link yields no yt-dlp subtitle track) and transcript/speaker
search (no transcript search exists at all).

---

**Suggested order**: 1 and 3 first (correctness in the current diff), then 2, 4, 5, 6, 7.
Post-#7 follow-ups: 9 (small, closes a latent data-loss edge), then 8 (one-time
verification pass). Items 10–13 came out of the 2026-07-23 as-built review of the VTT
transcript import. All items complete as of 2026-07-23.
