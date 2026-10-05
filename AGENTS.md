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
