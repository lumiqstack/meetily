# Main Bug-Fix Plan Status

Updated: 2026-10-05

## Source state

- Branch: `main`
- Base at start: `857a0df604d8800d66b8cfb97567a4c555ed1f58` (matched `origin/main`)
- The plan's 13 fixes were inspected on the clean base. Most functional fixes were already present; this pass closed remaining logging/privacy, summary supervision, audio-save reporting, and verified Copilot catalog gaps.
- Changes are local and uncommitted. No database, meeting, model, or migration files were changed. No installer was built and the app was not launched.

## Plan items

| Item | Status |
| --- | --- |
| 1. UTF-8-safe Obsidian filenames | Implemented on base; focused Obsidian tests pass. |
| 2. Drain recording tail before finalization | Implemented on base; saver drain/storage tests pass. |
| 3. Preserve incomplete meetings and recover safely | Implemented on base; repository and summary-stage tests pass. Stop UI now distinguishes unavailable/incomplete audio from saved transcripts and does not promise recovery without retained audio. |
| 4. Fail closed on SQLite open errors | Implemented on base; database tests passed in the library run. |
| 5. Fail recording startup on required persistence errors | Implemented on base; storage tests pass. |
| 6. Remove content-bearing logs/reports | Removed transcript, provider-response, endpoint, and error-message content from relevant release logs and analytics. Added synthetic-sentinel regression checks. |
| 7. Serialize recording lifecycle transitions | Implemented on base with shared RAII start guard and coordinated stop behavior. |
| 8. Recover interrupted summaries | Startup reconciliation already existed. Added supervision to the pipeline-triggered summary path so unwind failures become terminal for the matching attempt and cancellation state is cleaned up. |
| 9. Copilot CLI catalog | Retirement exclusions and stale-selection handling retained. Offered models are Auto plus `claude-haiku-4.5`, `gemini-3.8-flash`, and `claude-opus-5.5`; all three IDs returned the expected response in the installed CLI/account. Other candidate IDs were not verified and are not offered. |
| 10. Bound and verify yt-dlp provisioning | Implemented on base with bounded, checksum-verified downloads and failure/concurrency tests. |
| 11. Remove personal defaults for new settings | New-row paths explicitly use the empty vocabulary default; the distributed launcher uses a gitignored local configuration file. Historical migration bytes and existing settings were preserved. |
| 12. Explicit remote vocabulary opt-in | Implemented on base, default-off, with mock multipart coverage. No real remote endpoint test was performed. |
| 13. Separate unit/browser test requirements | Documentation and explicit browser command are present. Default unit discovery passed; browser execution remains blocked by absent Playwright/Chrome prerequisites. |

## Validation

- `bun test` — passed: 124 tests, 0 failures, 373 expectations across 22 files.
- `frontend\node_modules\.bin\tsc.cmd --noEmit --incremental false` — passed.
- `node --test tests/browser/teams-recap-dom.browser.cjs` — not run successfully: Playwright is not installed; the test failed explicitly as designed. Chrome was also not found. Playwright was not added as a project dependency.
- Focused Rust filters — passed: 159 tests across Obsidian, recording saver, Gemini transcription/batch, summary processor/service, pipeline summary stage, and meeting repository.
- `cargo test -p meetily --lib --no-fail-fast` — 625 passed, 7 failed, 2 ignored. All 7 failures are VAD tests blocked by the bundled `onnxruntime.dll` reporting version 1.17.1 while `ort 2.0.0-rc.10` requires 1.22.x. The focused changed-area tests passed.
- `pnpm` could not start because the installed shim points to a missing global executable; the compatible local Bun and TypeScript binaries were used directly.
- `NEXT_TELEMETRY_DISABLED=1 next build` — passed (Next.js 14.2.35).
- `git diff --check` — passed.
- `rustfmt --check` on touched Rust files reported formatting differences in these pre-existing modules; broad formatting was avoided to prevent unrelated churn.
- GitHub Copilot CLI 1.0.91: representative one-line `OK` invocations passed for the three offered model IDs. Candidate IDs not verified: `claude-sonnet-5`, `gpt-5.6-sol`, `gpt-5.6-luna`, `kimi-k3`, and `grok-4.6`.
- Raw Windows executable build (`cargo build --release -p meetily`, CPU-only) — failed in `tauri-build` while removing/copying an existing sidecar: OS error 5, Access denied. The existing `target\release\meetily.exe` remained unchanged (last write 2026-10-01). No process was terminated.

## Remaining verification

1. Resolve the Windows Tauri sidecar access-denied condition, then rebuild the raw executable only.
2. Align the available ONNX Runtime DLL with the version required by `ort` and rerun the full Rust library suite.
3. Run the separate browser suite with the documented existing Playwright and Chrome prerequisites.
4. Before any app launch, follow `docs/windows/DATABASE_UPGRADE_CHECK.md` and obtain explicit user approval after the backup/check passes. No runtime/database verification was attempted here.
5. Revisit unverified Copilot replacement candidates only when their exact IDs and account availability can be verified.
