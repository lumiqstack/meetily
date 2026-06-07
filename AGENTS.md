# Agent Instructions

Read this before making code changes or running builds in this workspace.

## Active Workspace

- Workspace: `D:\codex\meetily-0.4.0`
- Fork remote: `origin` (`lumiqstack/meetily`)
- Current local feature branch: `codex/reapply-local-features`
- Upstream release base: `v0.4.0`

## Windows Build Rule

For local Windows builds, create the **NSIS setup executable** only. Do not intentionally create or rely on MSI artifacts going forward.

Use:

```powershell
cd D:\codex\meetily-0.4.0\frontend
$env:LIBCLANG_PATH = "D:\codex\.tools\libclang.runtime.win-x64.21.1.8\runtimes\win-x64\native\libclang.dll"
$env:PATH = "D:\codex\.tools\cmake-4.3.3-windows-x86_64\bin;D:\codex\.tools\libclang.runtime.win-x64.21.1.8\runtimes\win-x64\native;$env:PATH"
$env:TAURI_GPU_FEATURE = "none"
pnpm tauri build --bundles nsis
```

Expected output:

```text
D:\codex\meetily-0.4.0\target\release\bundle\nsis\meetily_0.4.0_x64-setup.exe
```

Notes:

- If `target\release\meetily.exe` is running, stop it before building; Windows will deny replacement.
- A final updater-signing error is expected unless `TAURI_SIGNING_PRIVATE_KEY` is set. The NSIS installer can still be produced before that signing step fails.
- Do not use the default all-target Tauri build when the goal is a local Windows installer; it also tries to create MSI and other configured bundle targets.

