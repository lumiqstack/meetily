# Agent Instructions

Read this before making code changes or running builds in this workspace.

## Active Workspace

- Workspace: `D:\codex\meetily-0.4.0`
- Fork remote: `origin` (`lumiqstack/meetily`)
- Build and work from `main`. Pull requests target `main`.
  (`codex/reapply-local-features` is retired: a database opened by a `main`
  build cannot be opened by a build of that branch.)

## Building and running a new version

Follow `docs/windows/DATABASE_UPGRADE_CHECK.md` step by step: build without
launching, run `scripts\windows\meetily-check.ps1`, then launch. The first
launch of a new build upgrades the user's database irreversibly.

## Merging pull requests

- The `main` ruleset accepts only checks from pull-request-triggered runs. `windows-tests.yml` runs on `pull_request`, so a PR normally has them within minutes of a push. A manually dispatched run does not count.
- `windows-tests.yml` runs on every pull request (its `pull_request` trigger has no path filter on purpose; the `push` trigger keeps one). If a PR still shows no checks, close and reopen it to trigger them, then `gh pr merge N --merge --auto`.
- `--auto` merges immediately once the PR is already clean. The GitHub API's enable-auto-merge call refuses a clean PR for the same reason; merge it directly in that case.
- Use merge commits, never squash or rebase, so the test-then-fix commit order stays in history.
- Stacked PRs: merge the parent first, retarget the child to `main` (`gh pr edit N --base main`), wait for its mergeability to recompute, then merge it.
- After landing a batch, re-run `npx tsc --noEmit --incremental false`, `bun test` and `cargo test -p meetily --lib` on `main`: the per-PR checks ran against the base each PR was cut from, not against the combined result.

## Windows Build Rule

For local Windows builds, create the raw **application executable** only. Do not intentionally create or rely on MSI or NSIS installer artifacts going forward.

Prefer `scripts\windows\build-vulkan.ps1` (see the section above). The manual CPU commands below are a fallback only.

Use:

```powershell
cd D:\codex\meetily-0.4.0\frontend
$env:LIBCLANG_PATH = "D:\codex\.tools\libclang.runtime.win-x64.21.1.8\runtimes\win-x64\native\libclang.dll"
$env:PATH = "D:\codex\.tools\cmake-4.3.3-windows-x86_64\bin;D:\codex\.tools\libclang.runtime.win-x64.21.1.8\runtimes\win-x64\native;$env:PATH"
$env:TAURI_GPU_FEATURE = "none"
cargo build --release -p meetily
```

Expected output:

```text
D:\codex\meetily-0.4.0\target\release\meetily.exe
```

Notes:

- If `target\release\meetily.exe` is running, stop it before building; Windows will deny replacement.
- Do not use the default all-target Tauri build when the goal is a local executable; it tries to create installer bundles such as MSI and NSIS.
- Do not run `pnpm tauri build` or `pnpm tauri build --bundles ...` for local executable-only builds.
