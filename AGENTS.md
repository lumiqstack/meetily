# Agent Instructions

Read this before making code changes or running builds in this workspace.

## Active Workspace

- Workspace: `D:\codex\meetily-0.4.0`
- Fork remote: `origin` (`lumiqstack/meetily`)
- Current local feature branch: `codex/reapply-local-features`
- Upstream release base: `v0.4.0`

## Windows Build Rule

For local Windows builds, create the raw **application executable** only. Do not intentionally create or rely on MSI or NSIS installer artifacts going forward.

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
