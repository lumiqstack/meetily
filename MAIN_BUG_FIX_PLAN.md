# Main Branch Bug-Fix Plan

Status: **proposed — awaiting review**. No fixes have been implemented yet.

Baseline: `origin/main` at `613c2dc` (fork features rebased onto upstream
v0.4.1 + Whisper vocabulary priming + Teams Copilot recap import). Line
numbers below refer to that commit.

## For the reviewing agent

Read `AGENTS.md`, `CLAUDE.md`, and this file. For each item, check:

1. **Is the diagnosis correct?** Open the cited code and confirm the failure
   scenario. Mark any item you cannot reproduce by reading as `DISPUTED` with
   your reasoning.
2. **Is the proposed fix the narrowest correct one?** Flag over-engineering,
   missing edge cases, or a simpler alternative.
3. **Are the tests sufficient** to catch a regression?
4. **Is the ordering safe?** Items 2, 5 and 7 all touch the recording
   start/stop path and must not be developed in parallel.

Record your verdict inline under each item as
`Review: APPROVE | CHANGES REQUESTED | DISPUTED — <notes>`.

## Constraints (apply to every item)

- Supported app is `frontend/` (Next.js + Rust/Tauri). Never touch `backend/`.
- Rust cannot be compiled on the macOS dev machine (the `cidre` crate needs
  full Xcode). Rust validation runs on the Windows box:
  `cargo test -p meetily --lib --no-fail-fast` (executable-only builds, never
  MSI/NSIS bundles). On macOS, use `rustfmt --check` on touched files plus
  careful review.
- Frontend validation: `cd frontend && pnpm exec tsc --noEmit` and `bun test`.
  The lockfile is pnpm v9; if the system pnpm is newer, use `npx pnpm@9`.
- Meeting transcript/summary text must never appear in info-level or
  release logs.
- Hot-path Rust logs use `perf_debug!` / `perf_trace!`.
- One item per commit, each with a regression test where practical.
- Cross-reference: items 2–7 correspond to open items #6–#11 of
  `BUG_FIX_HANDOFF.md` on `codex/reapply-local-features`. Fixes should be
  portable to that branch (or land there first and be cherry-picked).

## Summary

| # | Severity | Area | Title | Handoff ref |
|---|----------|------|-------|-------------|
| 1 | High | Obsidian | UTF-8 panic in filename truncation | — |
| 2 | High | Recording | Tail of recorded audio is discarded on stop | #10 |
| 3 | High | Recording | Meeting not persisted after transcription timeout / poll error | #7 |
| 4 | High | Database | Corruption "recovery" deletes WAL/SHM | #11 |
| 5 | Medium | Recording | Audio-saver init failure is swallowed | #9 |
| 6 | Medium | Privacy | Transcript text in info-level logs | #6 |
| 7 | Medium | Recording | Concurrent start race | #8 |
| 8 | Low | Summary | Summary polling has no upper bound | — |
| 9 | Low | Summary | Stale Copilot CLI model ids offered | — |
| 10 | Low | Import | yt-dlp downloaded without timeout or integrity check | — |
| 11 | Hygiene | Repo | Personal data and machine paths in a public repo | — |
| 12 | Gap | Transcription | Vocabulary hint not sent to remote provider | — |
| 13 | Hygiene | Tests | Test suite fails without Playwright | — |

Recommended order: 1 → 6 → 4 → 2 → 5 → 7 → 3 → 8 → 9 → 10 → 13 → 11 → 12.
(Small isolated fixes first; the recording path 2/5/7 sequentially; 3 after
the Rust side is stable; repo hygiene and feature gaps last.)

---

## 1. UTF-8 panic in Obsidian filename truncation — High

**Where:** `frontend/src-tauri/src/obsidian.rs:155-158` (`sanitize_filename`).

**Problem:** `filename.truncate(120)` truncates at byte 120. If that byte is
inside a multi-byte character (e.g. `ó`, `é`, `ñ`), `String::truncate`
panics (`assertion failed: self.is_char_boundary(new_len)`). Reproduced with
a standalone program: 119 ASCII bytes followed by `ón…` panics.

**Impact:** Meeting titles come from LLM summary headings
(`extract_meeting_name_from_markdown` in `summary/service.rs`) and Teams
recap titles, which are often long and Spanish-accented. Auto-export
(`SummaryService::auto_export_to_obsidian`) panics inside the summary
background task. The summary is already saved, but the export is lost and
`cleanup_cancellation_token` is skipped. Manual export fails the
`export_meeting_to_obsidian` command.

**Fix:** Truncate on a char boundary, for example:

```rust
if filename.len() > 120 {
    let mut end = 120;
    while !filename.is_char_boundary(end) { end -= 1; }
    filename.truncate(end);
    ...
}
```

Keep the 120-*byte* budget, because Windows `MAX_PATH` pressure is in
bytes/UTF-16 units, not chars. Audit other new string slicing in fork code; a
grep over the `a2cb62e..origin/main` diff found the remaining slices safe
(ASCII-validated).

**Tests:** Unit tests in `obsidian.rs`: a title whose 120th byte is mid-`ó`
returns ≤120 bytes, is valid UTF-8, and ends in `.md` after rendering. Also a
title made only of 3-byte characters (CJK).

**Validation:** `cargo test -p meetily --lib obsidian`.

---

## 2. Tail of recorded audio is discarded on stop — High (handoff #10)

**Where:** `frontend/src-tauri/src/audio/recording_saver.rs`:
`start_accumulation` (~L143-223) and `stop_and_save` (~L357-370).

**Problem:** `stop_and_save` sets `is_saving = false` and then sleeps 200 ms.
The accumulation task checks `is_saving` *after* receiving each chunk and
`break`s when it is false. Every chunk still queued at stop time is therefore
**dropped**, not drained. The finalize step then runs against whatever made
it in. The sleep is unrelated to the actual queue depth.

**Fix:**

- Store the accumulation task's `JoinHandle` in `RecordingSaver`
  (`accumulation_task: Option<JoinHandle<()>>`).
- Remove the `is_saving` early-break from the receive loop. The loop ends
  naturally when every `Sender<AudioChunk>` is dropped (`recv()` returns
  `None`).
- On stop, ensure the pipeline's `recording_sender` is dropped *before*
  awaiting the task. Confirm in `recording_manager.rs` (around L262-279 and
  the stop path at L376/L415) that stopping the pipeline drops its sender
  clone; if not, add an explicit close.
- `stop_and_save`: await the join handle with a generous bounded timeout
  (e.g. 30 s). On timeout, log a warning and proceed to finalize. Then
  finalize the incremental saver.
- Keep `is_saving` only if still needed for `get_stats`; otherwise remove it.

**Do not** replace the sleep with a longer sleep.

**Tests:** A unit test with an unbounded channel pre-filled with N chunks.
Start accumulation with a fake/real `IncrementalAudioSaver` in a temp dir,
drop the sender, call the drain path, and assert all N chunks were added. If
`IncrementalAudioSaver` is hard to fake, extract the loop into a function
that takes a `FnMut(AudioChunk)` sink.

**Validation:** `cargo test -p meetily --lib audio:: --no-fail-fast`.

---

## 3. Meeting not persisted after transcription timeout or poll error — High (handoff #7)

**Where:** `frontend/src/hooks/useRecordingStop.ts`: polling loop ~L194-226,
save guard ~L266, `else` branch ~L440.

**Problem:** With realtime transcription enabled, the save only happens if
`transcriptionComplete` is true. A 60 s timeout, or a single thrown
`getTranscriptionStatus()` call (`catch → break`), leaves it false. The hook
then silently sets status to IDLE: no DB row, no toast. Audio/checkpoints
exist on disk, but the meeting does not appear in the app.

**Fix:**

- Always call `storageService.saveMeeting(...)` once audio stop succeeded and
  `isCallApi` is true, with whatever transcripts are in `transcriptsRef`.
- Track `transcriptionIncomplete = realtimeTranscriptionEnabled && !transcriptionComplete`.
- When incomplete:
  - show a warning toast ("Transcription still finishing — the meeting was
    saved and will be completed in the background");
  - skip auto-summary for that meeting (do not summarize a partial
    transcript as if complete);
  - make sure the background pipeline can pick it up. The pipeline's
    work-list query treats "meetings with a folder but no transcripts" as
    pending (see `migrations/20260726010000_add_pipeline_meta.sql`). Verify
    whether a meeting with *partial* transcripts is also picked up. If not,
    add the smallest marker possible (e.g. a pipeline_meta row or a
    `transcription_incomplete` flag) and include it in the derived query.
    The reviewer should decide whether a schema change is warranted here.
- Polling errors: retry with backoff, up to ~3 consecutive failures, instead
  of `break`ing on the first one.
- Remove the transcript `sample_text` / `last_transcript` fields from the
  `console.log` (devtools are enabled in release builds).

**Tests:** Bun hook tests (pattern: `tests/hooks/summary-generation.test.tsx`)
covering (a) normal completion → save + summary, (b) timeout → save, warning,
no auto-summary, (c) one poll error then success → save normally,
(d) persistent poll errors → save as incomplete.

**Validation:** `bun test tests/hooks`, then `pnpm exec tsc --noEmit`.

---

## 4. SQLite corruption recovery deletes WAL/SHM — High (handoff #11)

**Where:** `frontend/src-tauri/src/database/manager.rs`, `new_from_app_handle`
(the `"malformed" || "corrupt"` branch).

**Problem:** Any open error whose message contains "malformed" or "corrupt"
causes `meeting_minutes.sqlite-wal` and `-shm` to be deleted before retrying.
The WAL can contain committed transactions (recent meetings) not yet
checkpointed. Deleting it silently loses that data, and the string match
also fires on main-file corruption, where deleting the WAL does not help.

**Fix:**

- Before touching anything, copy the full set (`.sqlite`, `-wal`, `-shm` if
  present) into `app_data_dir/db-backups/<timestamp>/`. Abort recovery if the
  backup fails.
- Only delete the **`-shm`** first (it is a rebuildable index), then retry.
  SQLite rebuilds shm from the WAL.
- If it still fails, move the WAL into the backup folder (not delete) and
  retry once more. If that succeeds, log an error-level message naming the
  backup path and emit a frontend event (`database-recovered`) so the UI can
  tell the user.
- If it still fails, return the error. Never delete the main file.
- Optionally run `PRAGMA quick_check` after a successful recovery open and
  log the result.

**Tests:** Use `tempfile`:

- (a) create a DB in WAL mode, commit rows without checkpoint, corrupt the
  `-shm` → recovery opens and the rows are present;
- (b) the recovery path always produces a backup directory containing all
  original files;
- (c) unrecoverable main file → error returned, files still present.

**Validation:** `cargo test -p meetily --lib database:: --no-fail-fast`.

---

## 5. Audio-saver initialization failure is swallowed — Medium (handoff #9)

**Where:** `recording_saver.rs` `start_accumulation` ~L155-175; caller
`recording_manager.rs:279`.

**Problem:** If `initialize_meeting_folder` fails (unwritable recordings
folder, encoder init failure in `IncrementalAudioSaver::new`), the error is
logged and recording continues. The user records a whole meeting and only at
stop gets "No incremental saver initialized".

**Fix:** Make `start_accumulation` return `anyhow::Result<()>`.

- With `auto_save = true`, a folder/saver init failure returns `Err`. The
  manager propagates it from `start_recording` *before* streams start
  (streams start at L290, after accumulation at L279). Map it to a
  `RecordingStartError` variant with a user-facing message ("Cannot save
  audio to <folder>: <reason>").
- With `auto_save = false` (transcript-only), keep current behavior: log
  and continue.
- Ensure the engine claim and the partially created pipeline are released
  on this error path. The RAII `engine_claim` in `recording_commands.rs`
  already drops on `?`; verify the pipeline task/channels are dropped too.

Do this **after** item 2 (same file).

**Tests:** Point the recordings folder at a read-only temp dir →
`start_accumulation(true, …)` returns `Err`; same with `false` → `Ok`.

**Validation:** `cargo test -p meetily --lib audio:: --no-fail-fast`.

---

## 6. Transcript text in info-level logs — Medium (handoff #6)

**Where:**

- `frontend/src-tauri/src/audio/transcription/worker.rs:180` (and the
  `✅ Worker … transcribed: {}` line right after);
- `frontend/src-tauri/src/whisper_engine/whisper_engine.rs:867` and `:872`.

**Fix:** Replace text with metadata (`chars={}`, confidence, partial flag).
Keep the text only under `perf_debug!` (compiled out in release). Then grep
the Rust app (excluding `lib_old_complex.rs` / `core-old.rs` if they are not
compiled, so confirm with `mod` declarations) for other info/warn logs that
print `transcript`, `text`, `markdown`, `summary`, or `prompt` content. Known
borderline case: `audio/url_import.rs` logs yt-dlp stderr at warn, which may
include URLs with auth tokens; redact query strings.

**Tests:** None required beyond compilation. Optionally add a test asserting
a helper `redact_for_log` keeps only metadata.

**Validation:** `cargo test -p meetily --lib --no-fail-fast`.

---

## 7. Concurrent recording start race — Medium (handoff #8)

**Where:** `frontend/src-tauri/src/audio/recording_commands.rs:334` (default
devices path) and ~L547 (custom devices path).

**Problem:** `IS_RECORDING.load()` is checked, then many `.await`s run (model
validation, preferences, device resolution, stream start) before
`finalize_recording_start()` sets it. Two near-simultaneous starts (tray +
UI, double click, meeting-detection prompt) both pass the check. With
realtime transcription enabled, the engine claim blocks the second one, but
with realtime off nothing does, and the second `RecordingManager` overwrites
`RECORDING_MANAGER`.

**Fix:** Add `static IS_RECORDING_STARTING: AtomicBool`, acquired at the top
of both start functions via `compare_exchange(false, true)`. Reject when
`IS_RECORDING` or `IS_RECORDING_STARTING` is set. Release it through an RAII
guard (mirror the existing `StoppingGuard` pattern near L71-90), so every
error path restores idle. Make `stop_recording` reject (or wait) while
starting. Prefer a shared helper so both start paths stay identical.

**Tests:** Guard unit tests: second acquire fails while first held; drop
releases; panic-unwind releases.

**Validation:** `cargo test -p meetily --lib audio:: --no-fail-fast`.

---

## 8. Summary polling has no upper bound — Low

**Where:** `frontend/src/components/Sidebar/SidebarProvider.tsx` (~L225-282);
introduced in `cffb18a`, which removed the 200-poll (≈16 min) cap when
raising `GENERATION_TIMEOUT_SECS` to 1800.

**Problem:** Polling now runs every 5 s until the backend reports a terminal
state. If the summary task panics (default `panic = unwind`; the spawned
task in `summary/commands.rs:679` has no panic handler), the process row
stays `processing` and the UI polls forever.

**Fix (both):**

- Frontend: restore a cap derived from the backend budget, e.g. stop after
  `ceil((GENERATION_TIMEOUT_SECS + 10 min margin) / 5 s)` polls. Show the
  same timeout error as before with an updated duration. Consider exposing
  the timeout via a command instead of duplicating the constant.
- Backend: wrap `process_transcript_background` in
  `futures::FutureExt::catch_unwind` (with `AssertUnwindSafe`) and call
  `update_process_failed` on panic, so status is always terminal.

**Tests:** Extend `tests/hooks/summary-generation.test.tsx` or the
SidebarProvider polling test to assert polling stops at the cap.

---

## 9. Stale Copilot CLI model ids offered — Low

**Where:** `frontend/src/components/ModelSettingsModal.tsx:~105-120`
(`COPILOT_CLI_MODELS`), `frontend/src/contexts/ConfigContext.tsx`
(`'copilot-cli'` model list), `frontend/src-tauri/src/summary/copilot_cli.rs`
(`run_copilot`).

**Problem:** Lists retired ids (`claude-sonnet-4.5`, `claude-sonnet-4`,
`gpt-5`, `gpt-5-mini`, `gpt-4.1`, `gemini-2.5-pro`). Users with a saved
legacy id get CLI failures.

**Fix:** Port commit `ce7a59c` from `codex/reapply-local-features`. Only the
Copilot parts: `frontend/src/lib/copilot-cli-models.ts` (single source of
truth + `normalizeCopilotCliModel`), its use in `ConfigContext.tsx` and
`ModelSettingsModal.tsx`, and the Rust alias mapping in `copilot_cli.rs`. Do
**not** cherry-pick the whole commit (it also contains `audio_test_log.txt`,
pipeline changes, and items that don't apply to main).

The Copilot model catalog is plan-dependent, so it is worth confirming
the list against `copilot --help` / current docs before merging.

**Tests:** A unit test for `normalizeCopilotCliModel` (legacy → new, unknown
passthrough, empty → `auto`); a Rust test for the alias mapping.

---

## 10. yt-dlp downloaded without timeout or integrity check — Low

**Where:** `frontend/src-tauri/src/audio/ytdlp.rs` `download_ytdlp` (~L90-140)
and the lookup order above it.

**Problems:**

- (a) `reqwest::Client` has no timeout, so a stalled download hangs the
  import forever;
- (b) the downloaded executable is run without verifying the published
  `SHA2-256SUMS`;
- (c) the cached copy is never refreshed, so SharePoint/Teams extractors go
  stale.

**Fix:**

- (a) `connect_timeout(15s)` + an overall timeout (e.g. 5 min), or a
  per-chunk idle timeout.
- (b) Download `SHA2-256SUMS` from the same release and verify the asset hash
  before the atomic rename (`sha2` crate; check whether it is already a
  dependency).
- (c) Optional: if the cached binary is older than N days, attempt a
  background refresh, falling back to the cached one.

**Tests:** Unit test the checksum-line parser and the mismatch → error path.

---

## 11. Personal data and machine paths in a public repo — Hygiene

`lumiqstack/meetily` is **public**.

- `frontend/src-tauri/src/config.rs:18` (`DEFAULT_WHISPER_VOCABULARY_HINT`)
  and `migrations/20260919000000_add_whisper_vocabulary_hint.sql` seed
  employer, client, and individual surnames as the default for every user.
- `Start-Meetily-Vulkan.cmd` hard-codes local `D:\` paths.

**Fix:**

- Change the default hint to empty (`""`) in `config.rs`.
- **Do not edit the already-applied migration.** sqlx checksums applied
  migrations, and editing it breaks startup for existing databases. Add a
  new migration only if needed. The safest path is to leave existing users'
  stored value alone, since it is their own setting.
- Move `Start-Meetily-Vulkan.cmd` out of the repo (or template it with
  environment variables and document it).
- Rewriting git history to remove the strings is the owner's decision. Flag
  it, don't do it.

---

## 12. Vocabulary hint not sent to the remote provider — Gap

**Where:**
`frontend/src-tauri/src/audio/transcription/openai_compatible_provider.rs`
`transcribe`.

**Problem:** The vocabulary hint is only applied to local whisper-rs. The
OpenAI-compatible endpoint (the oMLX path in daily use) supports a `prompt`
form field, but it is never sent.

**Fix:** Add `vocabulary_hint: Option<String>` to `OpenAICompatibleProvider`,
populate it in `from_saved_settings` and the realtime construction path, and
add `.text("prompt", hint)` when non-empty. Separately, evaluate whether
priming *every* short VAD segment causes the model to emit the prompt words
on silence (a known Whisper behavior). If observed, skip the prompt for
segments under ~1 s or add the hint terms to the hallucination filter.

**Tests:** Unit test that the multipart form includes `prompt` when set (use
a local mock server as the existing `llm_client.rs` tests do).

---

## 13. Test suite fails without Playwright — Hygiene

`frontend/tests/lib/teams-recap-dom.test.cjs` `require('playwright')`, which
is not a dependency, so `bun test` always exits 1 (100 pass / 1 fail).

**Fix:** Skip the file when Playwright is unavailable (wrap the `require` and
mark tests skipped), or rename it out of bun's discovery pattern and
document running it with `node --test`. Do not add Playwright as a
dependency just for this.

---

## Out of scope / notes

- **Migration divergence between branches:** `main` has
  `20260919000000_add_whisper_vocabulary_hint` and the integration branch
  doesn't. A DB opened by a `main` build will fail to open in an integration
  build until the branches are reconciled. Track this with the branch merge,
  not here.
- Handoff items #1–#5 are already fixed on `main` or don't apply there.
