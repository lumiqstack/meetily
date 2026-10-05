<#
.SYNOPSIS
  Update the Windows checkout and build the raw Meetily executable with Vulkan
  GPU acceleration for local Whisper transcription.

.DESCRIPTION
  Executable-only build (never MSI/NSIS, never `tauri build`), following
  AGENTS.md. Steps, each checked by native exit code:

    1. Preflight: Meetily must not be running; required tools must exist.
    2. Source: fetch origin and fast-forward the checked-out branch. Local
       changes (for example a locally applied patch) are stashed, the branch is
       updated, and the changes are re-applied on top. If they do not re-apply
       cleanly the checkout is restored exactly as it was and nothing is built.
    3. Frontend: production Next.js build, then force the Tauri crate to embed it.
    4. Rust tests (skip with -SkipTests): `cargo test -p meetily --lib`.
    5. Build: `cargo build --locked --release -p meetily --bin meetily --features vulkan`
       into the standard target directory. The previous executable is kept as
       target\release\meetily.previous.exe.
    6. Launcher: right after step 2, creates Start-Meetily-Vulkan.local.cmd
       (gitignored) from the parameters if it does not exist yet, so the
       existing shortcut keeps working even if the build later fails.
    7. Report: a timestamped Markdown report (PASS/FAIL per step) to -ReportPath,
       preserving that file's first "path header" line. No transcript text,
       credentials or authenticated URLs are written.

  Machine-specific paths are parameters on purpose; this file is public.

.EXAMPLE
  pwsh -File scripts\windows\build-vulkan.ps1 -VulkanSdk 'C:\VulkanSDK\1.4.304.0' `
       -LlamaHelper 'C:\tools\llama-helper.exe' -ReportPath 'C:\notes\build-report.md' -Launch
#>
[CmdletBinding()]
param(
    # Repository root. Defaults to the checkout this script lives in.
    [string]$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path,
    # Branch to build (fast-forwarded from origin).
    [string]$Branch = 'main',
    # Vulkan SDK root (needed to compile the Vulkan shaders). Defaults to $env:VULKAN_SDK.
    [string]$VulkanSdk = $env:VULKAN_SDK,
    # Optional summary helper (llama-helper.exe) the launcher should use.
    [string]$LlamaHelper = $env:MEETILY_LLAMA_HELPER,
    # Optional libclang.dll used by bindgen. Defaults to $env:LIBCLANG_PATH.
    [string]$LibClangPath = $env:LIBCLANG_PATH,
    # Optional folder containing cmake.exe, prepended to PATH.
    [string]$CMakeBin,
    # Visual Studio developer shell script.
    [string]$VsDevShell = 'C:\Program Files\Microsoft Visual Studio\2022\Community\Common7\Tools\Launch-VsDevShell.ps1',
    # Optional expected target of the %APPDATA%\com.meetily.ai junction (e.g. a data drive folder).
    [string]$ExpectedDataTarget,
    # Optional Markdown report file to (re)write.
    [string]$ReportPath,
    # Build the current working tree as-is: no fetch, no branch update.
    [switch]$SkipGitSync,
    # Skip the Rust library tests (not recommended).
    [switch]$SkipTests,
    # Start Meetily through Start-Meetily-Vulkan.cmd after a successful build.
    [switch]$Launch
)

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false

$startedAt = [DateTimeOffset]::Now
$steps = [System.Collections.Generic.List[object]]::new()
$facts = [ordered]@{}
$failure = ''
$transcribing = $false
$previousLocation = Get-Location
$targetDir = Join-Path $RepoRoot 'target'
$exe = Join-Path $targetDir 'release\meetily.exe'
$buildLog = Join-Path $targetDir 'build-vulkan.log'

function Add-Step([string]$Name, [string]$Status, [string]$Detail = '') {
    $steps.Add([pscustomobject]@{ Step = $Name; Status = $Status; Detail = $Detail; At = [DateTimeOffset]::Now.ToString('HH:mm:ss') })
    $color = if ($Status -eq 'PASS') { 'Green' } elseif ($Status -eq 'FAIL') { 'Red' } else { 'Yellow' }
    Write-Host ("[{0}] {1}{2}" -f $Status, $Name, $(if ($Detail) { " - $Detail" } else { '' })) -ForegroundColor $color
}

function Invoke-Native([string]$Step, [scriptblock]$Command) {
    & $Command
    if ($LASTEXITCODE -ne 0) { throw "$Step failed (exit $LASTEXITCODE)." }
}

function Get-Git([string[]]$GitArgs) {
    $output = & git -C $RepoRoot @GitArgs
    if ($LASTEXITCODE -ne 0) { throw "git $($GitArgs -join ' ') failed (exit $LASTEXITCODE)." }
    return $output
}

function Sync-Source {
    $branchNow = (Get-Git @('branch', '--show-current'))
    if ($branchNow -ne $Branch) { throw "Checkout is on '$branchNow', expected '$Branch'. Switch branches yourself first." }
    $origin = (Get-Git @('remote', 'get-url', 'origin'))
    if ($origin -notmatch 'lumiqstack/meetily(\.git)?$') { throw "origin is '$origin', not the lumiqstack/meetily fork." }

    $before = (Get-Git @('rev-parse', 'HEAD'))
    $facts['Source before'] = $before
    Invoke-Native 'git fetch' { git -C $RepoRoot fetch origin $Branch }
    $remote = (Get-Git @('rev-parse', "origin/$Branch"))
    & git -C $RepoRoot merge-base --is-ancestor $before $remote
    if ($LASTEXITCODE -ne 0) { throw "Local $Branch has commits that are not on origin/$Branch; refusing to rewrite it." }

    $dirty = @(Get-Git @('status', '--porcelain'))
    $stashed = $false
    if ($dirty.Count -gt 0) {
        $facts['Local changes'] = "$($dirty.Count) path(s); stashed and re-applied on top"
        $label = "build-vulkan: local changes on $($before.Substring(0, 9)) at $($startedAt.ToString('yyyy-MM-dd HH:mm'))"
        Invoke-Native 'git stash' { git -C $RepoRoot stash push --include-untracked -m $label }
        $stashed = $true
    }

    try {
        Invoke-Native 'git fast-forward' { git -C $RepoRoot merge --ff-only "origin/$Branch" }
        if ($stashed) {
            # Three-way re-apply of the local changes on top of the new commits.
            & git -C $RepoRoot stash apply
            if ($LASTEXITCODE -ne 0) { throw 'Local changes do not apply cleanly on the updated branch.' }
            $conflicts = @(Get-Git @('diff', '--name-only', '--diff-filter=U'))
            if ($conflicts.Count -gt 0) { throw "Local changes conflict in: $($conflicts -join ', ')" }
            # Keep the stash entry as a backup; it is only dropped by hand.
            $facts['Local changes backup'] = 'kept as stash@{0} (git stash list)'
        }
    }
    catch {
        $reason = $_.Exception.Message
        # Restore the checkout exactly: branch back to its old commit, then the
        # stashed changes, which applied cleanly there by construction.
        & git -C $RepoRoot reset --hard $before | Out-Null
        if ($stashed) {
            & git -C $RepoRoot clean -fd | Out-Null   # only files from the failed apply exist untracked now
            & git -C $RepoRoot stash pop | Out-Null
            if ($LASTEXITCODE -ne 0) { $reason += ' WARNING: restoring the local changes failed; they are still in "git stash list".' }
        }
        throw "Source update stopped and the checkout was restored to $($before.Substring(0, 9)) with its local changes. $reason"
    }
    $after = (Get-Git @('rev-parse', 'HEAD'))
    $facts['Source built'] = $after
    return "$($before.Substring(0, 9)) -> $($after.Substring(0, 9))"
}

function Write-LauncherConfig {
    $local = Join-Path $RepoRoot 'Start-Meetily-Vulkan.local.cmd'
    if (Test-Path -LiteralPath $local) { return 'kept existing Start-Meetily-Vulkan.local.cmd' }
    $lines = @('@echo off', 'rem Machine-specific settings for Start-Meetily-Vulkan.cmd (not committed).')
    if ($LlamaHelper) { $lines += "set `"MEETILY_LLAMA_HELPER=$LlamaHelper`"" }
    if ($VulkanSdk) { $lines += "set `"VULKAN_SDK=$VulkanSdk`"" }
    Set-Content -LiteralPath $local -Value $lines -Encoding ascii
    return 'created Start-Meetily-Vulkan.local.cmd'
}

function Write-Report {
    if (-not $ReportPath) { return }
    $header = "This report is located in $ReportPath"
    if (Test-Path -LiteralPath $ReportPath) {
        $first = Get-Content -LiteralPath $ReportPath -TotalCount 1
        if ($first) { $header = $first }
    }
    $body = @($header, '', '# Meetily Vulkan build', "Captured: $([DateTimeOffset]::Now.ToString('o'))", "Started: $($startedAt.ToString('o'))", "Result: $(if ($failure) { 'FAIL' } else { 'PASS' })")
    if ($failure) { $body += "Failure: $failure" }
    $body += @('', '## Facts')
    foreach ($key in $facts.Keys) { $body += "- ${key}: $($facts[$key])" }
    $body += @('', '## Steps', '| Step | Status | Time | Detail |', '| --- | --- | --- | --- |')
    foreach ($s in $steps) { $body += "| $($s.Step) | $($s.Status) | $($s.At) | $(($s.Detail -replace '\|', '/')) |" }
    $body += @('', "Full build log (local only): $buildLog")
    $directory = Split-Path -Parent $ReportPath
    if ($directory -and -not (Test-Path -LiteralPath $directory)) { New-Item -ItemType Directory -Path $directory | Out-Null }
    Set-Content -LiteralPath $ReportPath -Value $body -Encoding utf8
    Write-Host "Report saved: $ReportPath"
}

try {
    New-Item -ItemType Directory -Force -Path $targetDir | Out-Null
    Start-Transcript -LiteralPath $buildLog -Force | Out-Null
    $transcribing = $true
    Write-Host "Meetily Vulkan build started $($startedAt.ToString('o'))"

    # 1. Preflight
    if (Get-Process -Name meetily -ErrorAction SilentlyContinue) {
        throw 'Meetily is running. Let recordings and summaries finish, quit it from the tray menu, then rerun.'
    }
    if (-not $VulkanSdk -or -not (Test-Path -LiteralPath (Join-Path $VulkanSdk 'Bin'))) {
        throw 'Vulkan SDK not found. Pass -VulkanSdk <SDK root> (the folder containing Bin\glslc.exe) or set VULKAN_SDK.'
    }
    if (-not (Test-Path -LiteralPath (Join-Path $VulkanSdk 'Bin\glslc.exe'))) {
        throw "glslc.exe is missing from $VulkanSdk\Bin; the Vulkan shaders cannot be compiled."
    }
    if ($LlamaHelper -and -not (Test-Path -LiteralPath $LlamaHelper)) { throw "Summary helper not found: $LlamaHelper" }
    if ($LibClangPath -and -not (Test-Path -LiteralPath $LibClangPath)) { throw "libclang not found: $LibClangPath" }
    if (-not (Test-Path -LiteralPath $VsDevShell)) { throw "Visual Studio developer shell not found: $VsDevShell" }
    foreach ($tool in @('git', 'cargo', 'node')) {
        if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) { throw "$tool is not on PATH." }
    }
    $dataLink = Get-Item -LiteralPath (Join-Path $env:APPDATA 'com.meetily.ai') -Force -ErrorAction SilentlyContinue
    if ($dataLink) {
        $resolved = if ($dataLink.LinkType) { "$($dataLink.LinkType) -> $(@($dataLink.Target)[0])" } else { 'regular folder' }
        $facts['App data'] = $resolved
        if ($ExpectedDataTarget -and (-not $dataLink.LinkType -or @($dataLink.Target)[0].TrimEnd('\') -ne $ExpectedDataTarget.TrimEnd('\'))) {
            throw "App data location differs from $ExpectedDataTarget ($resolved). Not touching anything; check it first."
        }
    }
    if (Test-Path -LiteralPath $exe) { $facts['Executable before'] = (Get-FileHash -LiteralPath $exe).Hash }
    Add-Step 'Preflight' 'PASS'

    # 2. Source
    if ($SkipGitSync) {
        $facts['Source built'] = (Get-Git @('rev-parse', 'HEAD'))
        Add-Step 'Source' 'SKIP' 'built current working tree'
    }
    else {
        Add-Step 'Source' 'PASS' (Sync-Source)
    }
    # Right after the source update (which may bring the templated launcher),
    # so the existing shortcut keeps its paths even if the build fails later.
    Add-Step 'Launcher config' 'PASS' (Write-LauncherConfig)

    # Toolchain environment (verified Windows setup; see STATUS.md)
    & $VsDevShell -Arch amd64 -HostArch amd64 -SkipAutomaticLocation | Out-Null
    if ($LibClangPath) {
        $env:LIBCLANG_PATH = $LibClangPath
        $env:PATH = "$(Split-Path -Parent $LibClangPath);$env:PATH"
    }
    if ($CMakeBin) { $env:PATH = "$CMakeBin;$env:PATH" }
    $env:VULKAN_SDK = $VulkanSdk
    $env:PATH = "$VulkanSdk\Bin;$env:PATH"
    $env:CARGO_TARGET_DIR = $targetDir
    $env:TAURI_GPU_FEATURE = 'vulkan'
    $vsRoot = (Resolve-Path (Join-Path (Split-Path -Parent $VsDevShell) '..\..')).Path
    $env:CMAKE_GENERATOR_INSTANCE = $vsRoot
    $env:CMAKE_GENERATOR_PLATFORM = 'x64'
    $env:CMAKE_GENERATOR_TOOLSET = 'host=x64'
    foreach ($name in @('CMAKE_GENERATOR', 'HOST_CMAKE_GENERATOR', 'TARGET_CMAKE_GENERATOR', 'CMAKE_GENERATOR_x86_64-pc-windows-msvc', 'CMAKE_GENERATOR_x86_64_pc_windows_msvc')) {
        [Environment]::SetEnvironmentVariable($name, 'Visual Studio 17 2022', 'Process')
    }

    # 3. Frontend
    $frontend = Join-Path $RepoRoot 'frontend'
    Set-Location -LiteralPath $frontend
    $next = Join-Path $frontend 'node_modules\next\dist\bin\next'
    if (-not (Test-Path -LiteralPath $next)) {
        throw 'frontend\node_modules is missing Next.js. Install dependencies from the lockfile (pnpm 9: pnpm install --frozen-lockfile), then rerun.'
    }
    $env:PATH = "$frontend\node_modules\.bin;$env:PATH"
    Invoke-Native 'Frontend build' { node $next build 2>&1 | ForEach-Object { Write-Host $_ } }
    $index = Join-Path $frontend 'out\index.html'
    if (-not (Test-Path -LiteralPath $index)) { throw 'Frontend build produced no out\index.html.' }
    # Force the Tauri crate to re-embed the rebuilt UI (contents unchanged).
    (Get-Item -LiteralPath (Join-Path $frontend 'src-tauri\build.rs')).LastWriteTime = Get-Date
    Add-Step 'Frontend build' 'PASS'

    if (Get-Command bun -ErrorAction SilentlyContinue) {
        & bun test 2>&1 | ForEach-Object { Write-Host $_ }
        if ($LASTEXITCODE -ne 0) { throw "Frontend unit tests failed (exit $LASTEXITCODE)." }
        Add-Step 'Frontend unit tests' 'PASS' 'bun test'
    }
    else {
        Add-Step 'Frontend unit tests' 'SKIP' 'bun is not installed on this machine'
    }

    # 4. Rust tests
    if ($SkipTests) {
        Add-Step 'Rust tests' 'SKIP' '-SkipTests'
    }
    else {
        $testOutput = [System.Collections.Generic.List[string]]::new()
        # Tests skip app setup, which loads the bundled ONNX Runtime; without
        # this they can pick up an older onnxruntime.dll from System32.
        $env:ORT_DYLIB_PATH = Join-Path $RepoRoot 'frontend\src-tauri\binaries\onnxruntime\onnxruntime.dll'
        & cargo test --locked -p meetily --lib --no-fail-fast --features vulkan 2>&1 | ForEach-Object { Write-Host $_; $testOutput.Add("$_") }
        $testExit = $LASTEXITCODE
        $summary = ($testOutput | Where-Object { $_ -match '^test result:' } | Select-Object -Last 1)
        if ($testExit -ne 0) {
            $failed = @($testOutput | Where-Object { $_ -match '^test .* FAILED$' } | ForEach-Object { ($_ -split ' ')[1] })
            throw "Rust tests failed (exit $testExit). $summary Failed: $($failed -join ', ')"
        }
        Add-Step 'Rust tests' 'PASS' $summary
    }

    # 5. Build
    if (Test-Path -LiteralPath $exe) {
        Copy-Item -LiteralPath $exe -Destination (Join-Path $targetDir 'release\meetily.previous.exe') -Force
    }
    Set-Location -LiteralPath $frontend
    $buildStart = Get-Date
    Invoke-Native 'Vulkan build' { cargo build --locked --release -p meetily --bin meetily --features vulkan 2>&1 | ForEach-Object { Write-Host $_ } }
    if (-not (Test-Path -LiteralPath $exe)) { throw "The build reported success but $exe does not exist." }
    $facts['Executable built'] = (Get-FileHash -LiteralPath $exe).Hash
    $facts['Build features'] = 'vulkan (Whisper GPU), --locked, release, raw executable only'
    Add-Step 'Vulkan build' 'PASS' ("{0:n0} min" -f ((Get-Date) - $buildStart).TotalMinutes)

    if ($Launch) {
        $launcher = Join-Path $RepoRoot 'Start-Meetily-Vulkan.cmd'
        Start-Process -FilePath $launcher -WorkingDirectory $RepoRoot
        Add-Step 'Launch' 'PASS' 'started through Start-Meetily-Vulkan.cmd'
    }
}
catch {
    $failure = $_.Exception.Message
    Add-Step 'Stopped' 'FAIL' $failure
}
finally {
    if ($transcribing) { Stop-Transcript | Out-Null }
    Set-Location -LiteralPath $previousLocation.Path
    Write-Report
    if ($failure) {
        Write-Host "BUILD FAILED: $failure" -ForegroundColor Red
        exit 1
    }
    Write-Host 'Build complete. Verify in the app: Vulkan backend in the log, recordings visible, a transcription and a summary complete.' -ForegroundColor Green
}
