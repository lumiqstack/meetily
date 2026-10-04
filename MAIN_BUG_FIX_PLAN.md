# Main Branch Bug-Fix Execution Plan

Status: **review corrections integrated — ready for implementation with the verification gates below (2026-10-04)**. No application fixes have been implemented by this document update.

This is the authoritative execution plan. Its instructions replace the original proposals and inline review comments. Implement the fixes; do not repeat the review as the deliverable. Record completed work and actual validation in STATUS.md. If a required environment is unavailable, complete independent work and identify the exact remaining check rather than claiming success.

## Baseline and preflight

Read AGENTS.md, STATUS.md, CLAUDE.md and this file before changes. AGENTS.md and the user's instructions govern executable-only builds, data protection and handoff procedures.

Reviewed remote main: `a9701b6`, whose only change from application-source baseline `613c2dc` is this plan. Recheck branch, HEAD, remotes, remote history and dirty state before integration. Historical line numbers are not authoritative; locate the named functions.

Known state at review:

- `/Users/marcos/meetily` is on main at `ae964258`, nine commits behind the reviewed remote. Preserve existing changes to AGENTS.md, Copilot CLI errors, SidebarProvider and summary tests, plus untracked STATUS.md, this plan and lowercase stash.md. Do not overwrite the reviewed plan during checkout or integration.
- `/Users/marcos/.codex/worktrees/recap-windows-compile-fix/meetily` is on `codex/whisper-vulkan-stability` at `613c2dc`, with 35 staged files for Whisper state reuse, native exception guards, tests and vendored whisper-rs. Preserve and explicitly reconcile that patch before touching overlapping Whisper code; do not silently include it in unrelated commits.
- Windows runtime checkout: `D:\codex\meetily-0.4.0`. Last report is October 1: `d036958` plus the local GPU patch, successful Vulkan build and user-reported transcription completion. It does not prove current runtime state or resolve the October 3 SharePoint crash.
- This plan was validated by source/control-flow inspection and official model-document research. No fresh full test suite, Windows build, inference or crash reproduction was performed during review.

Use a suitable clean checkout/worktree for implementation if needed, without resetting dirty work. Target current fork main; `codex/reapply-local-features` and its BUG_FIX_HANDOFF.md are references, not an instruction to change the target branch. Do not blindly cherry-pick `ce7a59c`.

## Operating constraints and validation

- Supported app is frontend/ (Next.js and Rust/Tauri); leave legacy backend/ alone.
- Preserve live recordings, models, database and applied migration bytes. Never rewrite SQLx history/checksums to bypass startup failures. Verify Windows data resolution and the `%APPDATA%\com.meetily.ai` junction to `D:\Meetily` before runtime work.
- Mac Rust compilation is not categorically unavailable: STATUS.md records an October 1 full library cargo check with a temporary externalBin override and focused native tests. Check current prerequisites; use focused tests/type/link checks where feasible. Do not commit temporary build overrides or substitute Mac checks for Windows runtime evidence.
- Frontend: use the existing compatible pnpm/dependencies, avoid incidental lockfile changes, run `pnpm exec tsc --noEmit --incremental false` and applicable Bun tests from frontend/. After item 13, run the default unit suite and separate recap DOM suite as documented there. Frontend changes need a production asset build before application embedding is verified.
- Rust: focused tests for changed behavior, touched-file formatting and diff checks, then applicable library tests (`cargo test -p meetily --lib --no-fail-fast`) in a supported environment. Report blockers rather than claiming tests ran. Avoid repository-wide formatting churn.
- Windows: follow the verified environment in STATUS.md. Build only the raw application in the standard target. For the CPU app use `cargo build --release -p meetily` from frontend/; preserve the selected GPU behavior when validating the patched Vulkan app and use the explicitly verified feature command. Never use a default Tauri packaging build or MSI/NSIS. Rebuild frontend assets and force correct embedding when needed. Stop the app only after active recordings/summaries finish, before replacement.
- Transcript, summary, prompt, credentials and authenticated URL content must not enter release logs or synced reports. Use metadata and perf_debug!/perf_trace! for hot-path diagnostics.
- Keep each item a separately reviewable change/commit when committing. Stage explicit files; preserve unrelated work. Publication and deployment must follow the actual execution-task authorization, not be inferred from this document.
- Windows handoffs must contain one current Stash action, automatically capture successes/failures with timestamps and native exit-code checks, preserve the report path header, and follow AGENTS.md. Do not replace another project's current Stash action without checking it.

## Work order and completion criteria

Order: **1 → 6 → 4 → 2 → 5 → 7 → 3 → 8 → 9 → 10 → 13 → 11 → 12**.

Items 2/5/7 modify the same recording lifecycle and must be implemented sequentially; item 3 follows their stabilization. Reconcile the staged GPU patch before overlapping item 6 edits. Items 9 and 12 require external capability verification before their corresponding runtime behavior is accepted.

| # | Priority | Work | Handoff reference |
| --- | --- | --- | --- |
| 1 | High | UTF-8-safe Obsidian filename truncation | — |
| 2 | High | Drain recorded audio before finalization | #10 |
| 3 | High | Persist incomplete meetings and recover without partial summaries | #7 |
| 4 | High | Remove destructive SQLite recovery | #11 |
| 5 | Medium | Fail recording startup when required persistence fails | #9 |
| 6 | Medium | Remove content-bearing release logs | #6 |
| 7 | Medium | Serialize recording start/stop transitions | #8 |
| 8 | Low | Recover orphaned summary jobs without false UI timeouts | — |
| 9 | Low | Correct Copilot catalog with October retirement exclusions | — |
| 10 | Low | Bound and verify yt-dlp provisioning | — |
| 11 | Hygiene | Remove personal defaults from new settings and template launcher | — |
| 12 | Gap | Explicit opt-in remote vocabulary support | — |
| 13 | Hygiene | Separate unit and browser test requirements | — |

For each item, report source changes, meaningful tests and results, and remaining Windows/application verification separately. A launched process or successful build alone does not establish runtime success. Final Windows checks include intended executable/helper, visible recordings, representative transcription and completed representative summary as applicable.

## 1. UTF-8 panic in Obsidian filename truncation

**Location:** frontend/src-tauri/src/obsidian.rs, sanitize_filename and filename rendering/export callers.

**Diagnosis:** `filename.truncate(120)` panics when byte 120 splits a UTF-8 character. Auto-export happens after summary persistence; a panic loses export and skips cancellation cleanup. Manual export is affected too.

**Implementation:** Retain the existing conservative 120-byte sanitizer budget, move the endpoint backward to a character boundary, then truncate and apply existing trailing-character cleanup/fallback. Do not describe Windows MAX_PATH as a UTF-8 byte limit. Keep existing date prefixes, suffix collision handling and extensions. Audit nearby string slicing rather than assuming it is all safe.

**Acceptance:** Test 119 ASCII bytes followed by an accented character, CJK, emoji, ASCII and empty/trimmed input. Sanitizer output is valid UTF-8 and at most 120 bytes. Separately test rendered filenames including date, collision suffix and .md. Export no longer panics. Run focused Obsidian tests.

## 2. Drain the recording tail before finalization

**Location:** audio/recording_saver.rs start_accumulation/stop_and_save, audio/recording_manager.rs and audio/pipeline.rs stop paths.

**Diagnosis:** Stop sets is_saving=false before a 200 ms sleep; the consumer breaks after receiving a queued chunk instead of draining. Startup also sets the flag after spawning the consumer, creating an early-exit race. Pipeline stop already awaits its task and releases its recording sender.

**Implementation:**

1. Own the accumulation task handle and its Result; propagate add_chunk failures and task failures.
2. End normal accumulation through channel closure after all producers stop. Remove the flag-driven early break; initialize any retained status flags before spawning.
3. Stop/await producers, release all sender clones, then await accumulation. Finalize only after successful drain and confirmed writer termination.
4. Bound waiting with an explicit incomplete/error outcome. A timeout must not proceed to successful finalization: dropping a JoinHandle does not cancel the task. Retain ownership and use a cancellation/termination strategy appropriate to the actual writer, awaiting termination where possible. If a writer cannot safely be stopped, quarantine that recording and preserve its artifacts; do not reuse/finalize its saver concurrently.
5. Preserve recoverable checkpoints on errors and propagate failure through manager/command/UI paths instead of logging and returning apparent success. Do not replace the sleep with a longer sleep.

**Acceptance:** Exercise production drain logic with prefilled N chunks, a slow sink, sink failure, task failure and timeout. Assert every normal-path chunk is written, no writer runs after finalization, timeout is visibly incomplete, and checkpoints remain recoverable. Run focused audio tests.

## 3. Persist incomplete meetings and recover correctly

**Location:** frontend/src/hooks/useRecordingStop.ts; storage save command/repository; database/repositories/meeting.rs get_pending_meetings; pipeline/orchestrator.rs stage_for and recovery writers.

**Diagnosis:** Timeout or the first failed status poll leaves transcriptionComplete=false and skips DB save. Partial transcripts are already selected by the pipeline, but stage_for chooses Summarize whenever transcript_count > 0. Skipping the hook's auto-summary alone does not prevent partial summarization.

**Implementation:**

1. When audio stop succeeds and isCallApi is true, save exactly one meeting with available transcripts, including zero/partial transcripts. Retry transient polls with bounded backoff and a small consecutive-failure budget; reset that budget on success.
2. Persist an explicit transcription-incomplete state atomically with the meeting/transcript save. Reuse an existing durable field only if its semantics fit; otherwise add a forward migration. Never modify an applied migration.
3. Update both pending-work selection and stage selection so incomplete transcripts require transcription recovery, never automatic summarization. Clear the marker only after successful complete persistence.
4. Coordinate the live tail and recovery writers: no overlapping retranscription, duplicate meetings or duplicate segments. Make retry and restart completion idempotent and preserve partial content until successful replacement/completion.
5. Warn accurately that the meeting was saved with incomplete transcription. Promise background completion only when the pipeline is enabled and recoverable audio plus a viable job exist. Otherwise provide an explicit recovery/manual action; handle transcript-only recordings without claiming audio exists.
6. Respect item 2 save errors; do not report complete audio after an incomplete drain. Ensure transcript logging removed in item 6 stays removed.

**Acceptance:** Hook tests cover normal completion/save/summary; timeout/save/warning/no summary; transient poll recovery; persistent errors. Repository/pipeline tests cover partial and zero transcripts, enabled/disabled pipeline, missing audio, restart, no premature summary, racing live-tail completion and retry idempotency. Run hooks, TypeScript and focused Rust tests.

## 4. Remove destructive SQLite recovery

**Location:** frontend/src-tauri/src/database/manager.rs, new_from_app_handle and failed-open resource cleanup.

**Diagnosis:** Matching malformed/corrupt errors triggers deletion of WAL/SHM. The WAL may contain committed meetings absent from the main database. Successful open or quick_check after WAL removal does not prove those meetings survived.

**Implementation:** Remove automatic deletion/movement of live sidecars. Fail closed with an actionable error and preserve the original database set. Do not attempt to repair migrations or SQLx history.

If implementing a recovery attempt, first quiesce/close all connections, including failed-open pools, and establish exclusive ownership. Preserve a consistent complete original set (.sqlite, -wal, -shm when present), aborting on preservation failure. Perform any salvage only on a separate working copy; keep the preserved original immutable. Rebuilding the copy's SHM can be attempted, but loss of WAL contents must never be treated as transparent successful recovery. Do not promote a salvage copy to active data automatically. Require explicit user selection with a clear account of what was recovered and what remains uncertain. Recovery state must survive startup/UI listener timing; an event alone is insufficient. A full salvage UI is not required to land the narrow fail-closed fix.

**Acceptance:** Use a controlled uncheckpointed-WAL fixture (ordinary last-connection close can checkpoint it). Verify committed rows remain recoverable, failure causes no application deletion/movement of original files, isolated salvage leaves preserved copies byte-identical, copy failure stops recovery, and unrecoverable main data returns an actionable error. Avoid tests that confuse SQLite's own normal sidecar lifecycle with application cleanup. Run database tests; never exercise corruption fixtures on live data.

## 5. Fail startup on required persistence initialization errors

**Location:** audio/recording_saver.rs start_accumulation; audio/recording_manager.rs start_recording; audio/recording_commands.rs error mapping.

**Diagnosis:** Folder/encoder initialization errors are logged and swallowed. At stop, missing incremental_saver returns Ok(None), potentially disguising failure as audio saving being disabled.

**Implementation:** After item 2, return Result from initialization/accumulation startup. With auto_save=true, required folder/saver failure must fail before streams start. With auto_save=false, skip the audio encoder but still require the transcript/metadata persistence that this mode promises; fail clearly if that required storage cannot initialize. Return a useful recording-start error. Explicitly close channels and stop/await already-created pipeline tasks on failure; dropping a task handle is not cleanup. Release the engine claim and all transition guards and allow a later retry.

**Acceptance:** Use injected init failure or a file-as-directory fixture, not Windows-unreliable read-only-directory assumptions. Cover audio-on failure, transcript-only valid path without encoder, transcript-storage failure, no started capture after failure, no orphan tasks/engine claims and a successful subsequent start.

## 6. Remove content-bearing release logs

**Location:** audio/transcription/worker.rs; whisper_engine/whisper_engine.rs; useRecordingStop.ts; compiled Rust/TypeScript logging and audio/url_import.rs error paths.

**Implementation:** Replace transcript text with lengths/counts, confidence, partial flags and timing. Remove sample_text/last_transcript frontend fields. Audit info/warn/error and release console paths for transcript, summary, markdown, prompt and raw provider responses; confirm module declarations before excluding old files. Prefer no content logging; perf_debug!/perf_trace! may be used for appropriate non-sensitive diagnostics compiled out of release.

Do not persist raw yt-dlp stderr containing authenticated URLs, cookies, headers or private content. Omit it or emit narrowly allowlisted diagnostic categories; query-string removal alone is insufficient. Coordinate overlapping Whisper edits with the staged GPU patch.

**Acceptance:** Source/log audit plus focused compilation. If introducing a sanitizer, test representative secrets/content do not survive. Do not introduce real meeting content into tests or reports. No broad new test framework is needed for simple log removal.

## 7. Serialize recording lifecycle transitions

**Location:** audio/recording_commands.rs, both default/custom device start functions and stop_recording.

**Diagnosis:** IS_RECORDING is checked before awaits and set only at finalization. Realtime-off starts can both pass and replace RECORDING_MANAGER.

**Implementation:** Use a shared transition guard/protocol. Acquire start exclusivity before rechecking recording/stopping state, then hold it through manager publication and start finalization. Both entry points must use the same logic. Release on every error/unwind via RAII. Coordinate stop with the same protocol, returning a clear busy response or waiting safely during startup. Preserve the existing stop-tail recording semantics; do not permit a new start while the old tail owns resources. Avoid check-before-guard stale-state races and async deadlocks.

**Acceptance:** Guard tests plus command/lifecycle tests: two starts with realtime off across both entry points; first start finishes before second acquisition; init failure/retry; start/stop overlap; no manager overwrite or orphan stream. Run after items 2 and 5.

## 8. Recover orphaned summary jobs without false UI timeouts

**Location:** SidebarProvider.tsx polling; summary/commands.rs spawned task; summary/service.rs completion/export/cancellation cleanup; summary process repository.

**Diagnosis:** An unwinding summary task can leave processing forever. GENERATION_TIMEOUT_SECS=1800 is a local sidecar request limit, not the complete multi-chunk/multi-stage job budget or all providers. Restoring a fixed 40-minute UI failure can reject healthy long jobs.

**Implementation:** Keep lifecycle authority in the backend. Supervise spawned tasks and make Rust unwind failures terminal for the matching meeting/started_at attempt, with cancellation registration cleanup on all exits. Separate post-save export failure from generation failure: never overwrite an already completed summary as failed because export panicked. Do not log panic payloads containing meeting text.

Add a backend liveness/reconciliation mechanism for truly orphaned jobs, including restart, using owned task state or heartbeat evidence as appropriate. The frontend should display backend state and bounded communication/stalled status accurately, with retry/cancel actions; it must not invent a terminal generation failure from poll count. Any whole-job deadline must be explicit, backend-owned and justified across providers/chunks, not copied from the request constant. Use elapsed time for time-based behavior. catch_unwind cannot contain native aborts/access violations and is not a fix for the unexplained SharePoint crash.

**Acceptance:** Long multi-request summary remains active beyond the old UI limit; panic before persistence fails only its own attempt; panic after persistence preserves completion; stale attempt cannot change a newer job; cancellation registry cleans up; restart reconciles orphaned processing; remount/suspended timers and transient IPC errors do not falsely fail healthy jobs. Retain the existing long-summary regression.

## 9. Correct the Copilot model catalog

**Location:** ModelSettingsModal.tsx, ConfigContext.tsx, proposed frontend/src/lib/copilot-cli-models.ts, summary/copilot_cli.rs and related tests.

**Scope:** Consolidate catalog/selection handling, remove retired choices, and make stale saved selections actionable. Adapt only the relevant structure from ce7a59c; never cherry-pick its unrelated audio log/pipeline changes or copy its obsolete catalog/aliases verbatim. Preserve existing structured Copilot error work.

### Mandatory October exclusions

Do not add any model retired or scheduled to retire during October 2026 to the new selectable catalog or any replacement alias target, even before its retirement date. Known exclusions from the validated notices:

| Retirement | Excluded IDs |
| --- | --- |
| October 2 | gemini-3.5-flash, gemini-3.6-flash, kimi-k2.7-code, claude-opus-4.7 |
| October 19 | gpt-5-mini, gpt-5.4, gpt-5.4-mini, gpt-5.5, gemini-3.7-flash, grok-4.5 |

Previously retired models also remain excluded. Existing saved excluded IDs must be visibly identified and require a supported replacement; retain the stored value for diagnosis until the user changes it. Do not silently remap an explicit selection to Auto or let excluded IDs reappear through unknown-ID passthrough. Other unknown custom IDs may be preserved without claiming availability. Empty selection may default to the explicitly documented Auto option.

### Replacement rules and verification gate

| Legacy choice | Required handling |
| --- | --- |
| claude-sonnet-4.5 | Sonnet 5 is a documented replacement family; verify its exact CLI ID and account availability |
| claude-sonnet-4 | Do not universally map to Sonnet 4.6: it is retired except for individual annual subscribers. Prefer a verified supported Sonnet choice |
| gpt-5 / gpt-4.1 | Do not target GPT-5.5, which is October-excluded. Evaluate GitHub's GPT-5.6 Sol replacement with exact CLI/account checks |
| gpt-5-mini | Exclude now by user requirement; retirement is October 19, not already effective on review day. Evaluate GPT-5.6 Luna |
| gemini-2.5-pro and excluded Flash models | Do not chain through retired Gemini 3.1 Pro/3.6 Flash. Evaluate Gemini 3.8 Flash |
| kimi-k2.7-code | Evaluate Kimi K3 |
| claude-opus-4.7 | Evaluate Claude Opus 5.5 |
| grok-4.5 | Evaluate Grok 4.6 |

These are model display-name candidates, not a verified allowlist of command-line identifiers. Inspect the actual Windows CLI version and supported model selection mechanism, confirm account/policy availability and complete a representative invocation before claiming runtime support. `/model`, `--model` and Auto are documented; do not assume an undocumented model-list command exists or ask an LLM to invent the catalog. Where that environment is unavailable, implement the independent shared validation structure and exclusion tests, and report candidate-ID/runtime verification as pending rather than guessing.

Make any intentional replacement visible to the user; keep frontend and Rust validation consistent. A maintained shared catalog or consistency test must prevent duplicate lists drifting. Help output alone does not establish entitlement or successful inference.

**Acceptance:** Both settings surfaces use consistent choices; every October ID is absent from offered entries and alias targets; saved excluded IDs cannot bypass validation; unknown non-excluded IDs and empty/Auto behave deliberately; valid selections are retained; policy errors are actionable; installed-CLI representative invocation is recorded separately from unit tests.

### Official evidence checked with Exa on 2026-10-04

- [GitHub model retirement history](https://docs.github.com/en/copilot/using-github-copilot/ai-models/supported-ai-models-in-copilot): Sonnet 4, Sonnet 4.5, GPT-5, GPT-4.1 and Gemini 2.5 Pro retirement evidence.
- [September 1 retirement notice](https://github.blog/changelog/2026-07-31-upcoming-august-2026-model-deprecations-in-github-copilot/): Sonnet 4.6 annual-plan exception and older replacement families.
- [October 2 retirements](https://github.blog/changelog/2026-10-02-selected-models-in-github-copilot-deprecated/): the four effective exclusions above and replacement families.
- [October 19 scheduled retirements](https://github.blog/changelog/2026-09-18-upcoming-deprecation-of-selected-github-copilot-models-in-mid-october/): the six scheduled exclusions above and replacement families.
- [CLI command reference](https://docs.github.com/en/copilot/reference/cli-command-reference): selection interface.

Retrieved documentation variants were inconsistent: catalog/examples retained deprecated entries, and extracted per-client availability cells were blank. Prefer explicit dated lifecycle notices, recheck new notices at implementation time, and verify actual CLI/account behavior. Do not treat a display name in a general Copilot table as proof of an exact usable CLI ID.

## 10. Bound and verify yt-dlp provisioning

**Location:** audio/ytdlp.rs download_ytdlp and discovery order.

**Diagnosis:** No explicit download timeout/checksum validation; cache reused indefinitely. Bundled and PATH binaries outrank the cache.

**Implementation:** Set connection and total/idle download limits with clear errors. Resolve one immutable release version, then obtain both platform asset and SHA2-256SUMS for that version. Verify the exact filename/checksum before installation, enforce a size bound, and preserve the working binary on any failure. Serialize concurrent provisioning and use safe unique temporary files with cleanup; replace atomically using Windows-compatible behavior. Reuse existing dependencies when suitable. HTTPS-delivered checksums detect asset mismatch but are not independent authenticity evidence.

Do not add automatic background updates in this item. A separate update policy must account for bundled/PATH precedence, running executables and rollback. A refreshed cache alone will not change which higher-priority binary is used.

**Acceptance:** Local HTTP fixtures cover bad status, connect/stall/total timeout, oversized/truncated download, malformed/missing checksum entry, mismatch and concurrent calls. No partial binary becomes runnable; an existing working binary survives failure. Verify selected path/version when later testing actual import.

## 11. Remove personal defaults for new settings; preserve existing data

**Location:** config.rs DEFAULT_WHISPER_VOCABULARY_HINT; database/repositories/setting.rs insert paths; historical migration 20260919000000_add_whisper_vocabulary_hint.sql; Start-Meetily-Vulkan.cmd.

**Diagnosis:** Public repository seeds personal terms. Clearing only config.rs is insufficient because SQL inserts omit whisperVocabularyHint and inherit the historical column default.

**Implementation:** Set the application default empty and explicitly supply the intended empty value for all new settings rows, or implement a carefully tested forward migration if necessary. Preserve applied migration files byte-for-byte and do not clear existing users' stored settings. Inspect fresh-install initialization and upgrade paths for indirect inheritance of the old default.

Template the distributed launcher using documented configuration/environment variables without deleting or breaking the active Windows launcher/helper paths. History rewriting is outside this task; document residual historical exposure without repeating personal strings in new files.

**Acceptance:** Fresh install and newly created settings rows have empty hints; upgrades preserve user values; original migration hashes unchanged; distributed launcher has no personal fixed paths and the installed launcher remains functional.

## 12. Add opt-in remote vocabulary support

**Location:** audio/transcription/openai_compatible_provider.rs and all saved-settings/import/realtime construction paths plus configuration UI/persistence.

**Diagnosis:** Multipart transcription omits prompt. The existing vocabulary setting is described as local whisper-rs configuration. Actual target endpoint/model support has not been verified.

**Implementation:** Add an explicit remote-vocabulary opt-in, default off for existing/new users, with clear destination semantics. Carry an optional hint consistently to provider instances; send prompt only when opted in, non-empty and supported by the configured provider/model. Verify endpoint acceptance rather than assuming all OpenAI-compatible endpoints support the field. Persist new configuration through forward-compatible changes without altering applied migrations. Preserve existing local priming behavior.

Do not add hint terms to hallucination filters: legitimate speech could be removed. Use representative silence/short-speech tests before introducing any duration heuristic. Unsupported endpoints must yield an actionable outcome without silently pretending the hint was used.

**Acceptance:** Mock-server multipart tests cover off/empty/on and all construction paths; default does not transmit local terms. Verify actual target endpoint/model with representative audio before claiming support and evaluate false prompt echoes on silence/short segments. No private vocabulary in logs or reports.

## 13. Make unit and browser test requirements explicit

**Location:** frontend/tests/lib/teams-recap-dom.test.cjs and test commands/documentation.

**Diagnosis:** The DOM suite unconditionally requires undeclared Playwright and requests installed Chrome. Missing either can fail the run; the historical 100-pass/1-fail count was not independently reproduced.

**Implementation:** Separate browser fixtures from default Bun test discovery and provide an explicit node --test command using the supported existing Playwright provisioning/NODE_PATH setup. Document both module and Chrome requirements and how to run the suite. Do not add Playwright as a project dependency solely for this task. Preserve all existing DOM assertions. The explicit browser command must clearly fail for missing prerequisites rather than silently pass/skip; require it when recap extraction changes.

**Acceptance:** Default unit suite discovery is deterministic without Playwright. With the documented browser prerequisites, the separate DOM suite actually executes and passes. Reports distinguish unit tests from browser coverage; unavailable browser execution remains explicitly unverified.

## Closeout and remaining boundaries

Update STATUS.md with dated source state, validation evidence, unresolved checks, relevant uncommitted files and the next action. Retire completed Stash commands only when the applicable Windows action is complete. Distinguish published source from installed executable; if publication is requested, review/stage explicit files, commit, fetch/integrate normally, push and verify remote hash without force-pushing ordinary divergence.

The October 3 SharePoint crash remains a separate incident requiring actual Windows fault module/exception/time evidence. None of these code diagnoses alone proves its cause. Preserve the staged GPU patch and its recovery artifacts. Cross-branch migration divergence must be reconciled before switching a live database between builds; neither deleting migration history nor changing applied SQL is an acceptable shortcut.
