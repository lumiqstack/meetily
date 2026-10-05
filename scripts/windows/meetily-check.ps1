<#
.SYNOPSIS
  Mechanical safety checks around running a new Meetily build on Windows.
  Prints a short summary block meant to be pasted into a report as-is.

.DESCRIPTION
  -Mode Before  (run before building/launching a new build)
     1. Stops Meetily.
     2. Backs up the data folder's database (+ -wal/-shm) and JSON settings,
        with SHA-256 hashes.
     3. Reads integrity and applied migration versions from the BACKUP copy.
     4. Compares them with the migrations of -Branch on origin.
     5. Records git branch/status/stash and saves a patch copy of local changes.
     Verdict PASS: safe to build -Branch. Verdict STOP: do not build; report.

  -Mode After   (run after the first launch, with Meetily closed again)
     1. Reads the LIVE database read-only: integrity + migration versions.
     2. Summarizes the app log since -Since: counts of key lines and the last
        few error lines. Never prints transcript text: only lines matching
        known diagnostic patterns.

  Needs Python 3 (its built-in sqlite3 module) or sqlite3.exe on PATH.
  Exit code: 0 = PASS, 2 = STOP, 1 = the script itself failed.

.EXAMPLE
  pwsh -File scripts\windows\meetily-check.ps1 -Mode Before -Branch main
  pwsh -File scripts\windows\meetily-check.ps1 -Mode After -Since '2026-10-05 09:00'
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][ValidateSet('Before', 'After')][string]$Mode,
    # Branch whose migrations the database must be compatible with (Before).
    [string]$Branch = 'main',
    [string]$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path,
    # Overrides for testing; defaults are the real app locations.
    [string]$DataDir = (Join-Path $env:APPDATA 'com.meetily.ai'),
    [string]$LogFile = (Join-Path $env:LOCALAPPDATA 'com.meetily.ai\logs\meetily.log'),
    [string]$BackupRoot = (Join-Path $env:USERPROFILE 'meetily-backups'),
    # After: only consider log lines from this time on (default: last 24h).
    [datetime]$Since = (Get-Date).AddDays(-1),
    # Testing only: compare against this migrations folder instead of git.
    [string]$MigrationsDir,
    [switch]$NoStop
)

$ErrorActionPreference = 'Stop'
$lines = [System.Collections.Generic.List[string]]::new()
function Say([string]$text) { $lines.Add($text); Write-Host $text }
function Finish([string]$verdict, [string]$reason) {
    Say "VERDICT: $verdict$(if ($reason) { " - $reason" })"
    Say '--- end of meetily-check summary ---'
    exit $(if ($verdict -eq 'PASS') { 0 } else { 2 })
}

function Read-Db([string]$path) {
    # Returns @{ integrity = 'ok'|...; rows = @(@{version; success}) }
    $py = Get-Command python -ErrorAction SilentlyContinue
    if (-not $py) { $py = Get-Command python3 -ErrorAction SilentlyContinue }
    if ($py) {
        $code = @'
import json, sqlite3, sys
c = sqlite3.connect("file:" + sys.argv[1] + "?mode=ro", uri=True)
integrity = c.execute("PRAGMA integrity_check").fetchone()[0]
rows = [{"version": str(v), "success": int(s)} for v, s in c.execute("SELECT version, success FROM _sqlx_migrations ORDER BY version")]
print(json.dumps({"integrity": integrity, "rows": rows}))
'@
        $script = Join-Path ([IO.Path]::GetTempPath()) 'meetily-check-read.py'
        Set-Content -Path $script -Value $code -Encoding utf8
        $json = & $py.Source $script $path
        if ($LASTEXITCODE -ne 0) { throw "python could not read $path" }
        return $json | ConvertFrom-Json
    }
    $sqlite = Get-Command sqlite3 -ErrorAction SilentlyContinue
    if ($sqlite) {
        $integrity = (& $sqlite.Source -readonly $path 'PRAGMA integrity_check;' | Select-Object -First 1)
        $rows = & $sqlite.Source -readonly -separator '|' $path 'SELECT version, success FROM _sqlx_migrations ORDER BY version;' |
            ForEach-Object { $v, $s = $_ -split '\|'; [pscustomobject]@{ version = $v; success = [int]$s } }
        return [pscustomobject]@{ integrity = $integrity; rows = @($rows) }
    }
    return $null
}

function Branch-Versions {
    if ($MigrationsDir) {
        return @(Get-ChildItem -LiteralPath $MigrationsDir -Filter '*.sql' | ForEach-Object { $_.Name.Split('_')[0] })
    }
    & git -C $RepoRoot fetch --quiet origin $Branch
    if ($LASTEXITCODE -ne 0) { throw "git fetch origin $Branch failed" }
    $files = & git -C $RepoRoot ls-tree --name-only "origin/$Branch" frontend/src-tauri/migrations/
    if ($LASTEXITCODE -ne 0) { throw "could not list migrations on origin/$Branch" }
    return @($files | ForEach-Object { (Split-Path $_ -Leaf).Split('_')[0] })
}

try {
    Say "--- meetily-check $Mode ($(Get-Date -Format 'yyyy-MM-dd HH:mm')) ---"

    if (-not $NoStop) {
        Get-Process meetily -ErrorAction SilentlyContinue | Stop-Process
        Start-Sleep -Seconds 2
        if (Get-Process meetily -ErrorAction SilentlyContinue) { Finish 'STOP' 'Meetily is still running; close it and rerun' }
    }

    $item = Get-Item -LiteralPath $DataDir -Force -ErrorAction SilentlyContinue
    if (-not $item) { Finish 'STOP' "data folder not found: $DataDir" }
    $target = if ($item.LinkType) { "$($item.LinkType) -> $($item.Target)" } else { 'regular folder' }
    Say "Data folder: $DataDir ($target)"
    $db = Join-Path $DataDir 'meeting_minutes.sqlite'
    if (-not (Test-Path -LiteralPath $db)) { Finish 'STOP' 'meeting_minutes.sqlite not found (storage may be relocated; ask the user)' }

    if ($Mode -eq 'Before') {
        $backup = Join-Path $BackupRoot (Get-Date -Format 'yyyyMMdd-HHmmss')
        New-Item -ItemType Directory -Force -Path $backup | Out-Null
        Get-ChildItem -LiteralPath $DataDir -File |
            Where-Object { $_.Name -like 'meeting_minutes.sqlite*' -or $_.Extension -eq '.json' } |
            Copy-Item -Destination $backup
        Say "Backup: $backup"
        Get-ChildItem -LiteralPath $backup -File | ForEach-Object {
            Say ("  {0}  {1} bytes  {2}" -f $_.Name, $_.Length, (Get-FileHash -LiteralPath $_.FullName).Hash.Substring(0, 16))
        }

        $read = Read-Db (Join-Path $backup 'meeting_minutes.sqlite')
        if (-not $read) { Finish 'STOP' 'neither Python nor sqlite3 is available; ask the user before installing anything' }
        Say "Integrity (backup): $($read.integrity)"
        $applied = @($read.rows | ForEach-Object { $_.version })
        Say "Applied migrations: $($applied.Count), newest $($applied | Select-Object -Last 1)"
        if ($read.integrity -ne 'ok') { Finish 'STOP' 'database integrity check failed' }
        $failedRows = @($read.rows | Where-Object { $_.success -ne 1 })
        if ($failedRows) { Finish 'STOP' "migration(s) recorded as failed: $(($failedRows | ForEach-Object version) -join ', ')" }

        if (-not $MigrationsDir) {
            Say "Git branch: $(& git -C $RepoRoot branch --show-current)"
            $dirty = @(& git -C $RepoRoot status --porcelain)
            $stashes = @(& git -C $RepoRoot stash list)
            Say "Git local changes: $($dirty.Count) path(s); stashes: $($stashes.Count)"
            if ($dirty.Count -gt 0) {
                & git -C $RepoRoot diff --binary | Set-Content -Path (Join-Path $backup 'local-changes.patch') -Encoding utf8
                & git -C $RepoRoot ls-files --others --exclude-standard | Set-Content -Path (Join-Path $backup 'untracked-files.txt') -Encoding utf8
                Say "  saved local-changes.patch and untracked-files.txt in the backup folder"
            }
        }

        $known = Branch-Versions
        $unknown = @($applied | Where-Object { $_ -notin $known })
        $pending = @($known | Where-Object { $_ -notin $applied })
        Say "Branch '$Branch' migrations: $($known.Count); new for this database: $(if ($pending) { $pending -join ', ' } else { 'none' })"
        if ($unknown) { Finish 'STOP' "database has migration(s) this branch does not know: $($unknown -join ', ')" }
        Finish 'PASS' "safe to build '$Branch'"
    }

    # Mode After
    $read = Read-Db $db
    if (-not $read) { Finish 'STOP' 'neither Python nor sqlite3 is available' }
    $applied = @($read.rows | ForEach-Object { $_.version })
    Say "Integrity (live): $($read.integrity)"
    Say "Applied migrations: $($applied.Count), newest $($applied | Select-Object -Last 1)"
    $failedRows = @($read.rows | Where-Object { $_.success -ne 1 })

    $patterns = [ordered]@{
        'Startup'                  = 'Starting application'
        'ONNX Runtime bundled ok'  = 'Initialized bundled ONNX Runtime'
        'ONNX Runtime failure'     = 'Failed to (initialize|resolve) bundled ONNX Runtime'
        'COM keeper ready'         = 'WASAPI COM keeper ready'
        'Whisper state created'    = 'Creating reusable Whisper inference state'
        'Transcriptions'           = 'Transcription #\d+ result'
        'Native exceptions caught' = 'meetily_whisper_native_exception|NativeException'
        'Panics'                   = 'panicked'
        'Errors'                   = '\bERROR\b'
    }
    $logLines = @()
    if (Test-Path -LiteralPath $LogFile) {
        $logLines = @(Get-Content -LiteralPath $LogFile | Where-Object {
            # env_logger lines start with [YYYY-MM-DDTHH:MM:SSZ ...]; keep undated continuation lines.
            if ($_ -match '^\[(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2})') { [datetime]$Matches[1] -ge $Since.ToUniversalTime() } else { $true }
        })
        Say "Log: $LogFile ($($logLines.Count) lines since $($Since.ToString('yyyy-MM-dd HH:mm')))"
        foreach ($key in $patterns.Keys) {
            Say ("  {0}: {1}" -f $key, @($logLines | Where-Object { $_ -match $patterns[$key] }).Count)
        }
        $problems = @($logLines | Where-Object { $_ -match "$($patterns['Native exceptions caught'])|$($patterns['Panics'])|$($patterns['ONNX Runtime failure'])|\bERROR\b" } | Select-Object -Last 5)
        if ($problems) {
            Say '  Last problem lines (truncated to 200 chars):'
            $problems | ForEach-Object { Say ('    ' + $(if ($_.Length -gt 200) { $_.Substring(0, 200) + '...' } else { $_ })) }
        }
    } else {
        Say "Log: not found at $LogFile"
    }

    if ($read.integrity -ne 'ok') { Finish 'STOP' 'database integrity check failed' }
    if ($failedRows) { Finish 'STOP' "migration(s) recorded as failed: $(($failedRows | ForEach-Object version) -join ', ')" }
    Finish 'PASS' 'database upgraded cleanly; see log counts above'
}
catch {
    Say "SCRIPT ERROR: $($_.Exception.Message)"
    Say '--- end of meetily-check summary ---'
    exit 1
}
