# Meetily executable-only build for Windows (no MSI/NSIS installer bundles).
# Follows the local build rule in ..\AGENTS.md: builds the Next.js static
# export, then compiles target\release\meetily.exe directly with cargo.
#
# Usage:  .\build_exe_windows.ps1 [-SkipFrontend]
#   -SkipFrontend   Reuse the existing frontend\out export (Rust-only rebuild)

param(
    [switch]$SkipFrontend
)

$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot

# Windows denies replacing a running executable.
$running = Get-Process meetily -ErrorAction SilentlyContinue
if ($running) {
    Write-Host "meetily.exe is running (PID $($running.Id)). Close it before building." -ForegroundColor Red
    exit 1
}

# Local toolchain (see AGENTS.md)
$env:LIBCLANG_PATH = "D:\codex\.tools\libclang.runtime.win-x64.21.1.8\runtimes\win-x64\native\libclang.dll"
$env:PATH = "D:\codex\.tools\cmake-4.3.3-windows-x86_64\bin;D:\codex\.tools\libclang.runtime.win-x64.21.1.8\runtimes\win-x64\native;$env:PATH"
$env:TAURI_GPU_FEATURE = "none"

if (-not $SkipFrontend) {
    Write-Host "Building Next.js static export..." -ForegroundColor Cyan
    pnpm build
    if ($LASTEXITCODE -ne 0) {
        Write-Host "Frontend build failed." -ForegroundColor Red
        exit $LASTEXITCODE
    }
}

Write-Host "Building release executable..." -ForegroundColor Cyan
cargo build --release -p meetily
if ($LASTEXITCODE -ne 0) {
    Write-Host "Cargo build failed." -ForegroundColor Red
    exit $LASTEXITCODE
}

$exe = Join-Path (git rev-parse --show-toplevel) "target\release\meetily.exe"
Write-Host ""
Write-Host "Build complete: $exe" -ForegroundColor Green
Get-Item $exe | Select-Object Length, LastWriteTime
