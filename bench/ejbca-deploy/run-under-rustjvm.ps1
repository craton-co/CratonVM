# bench/ejbca-deploy/run-under-rustjvm.ps1
# WP8.11 — Boot WildFly+EJBCA under target/release/rustjvm.exe and poll
# https://localhost:8443/ejbca/. Mirrors run-under-rustjvm.sh.

$ErrorActionPreference = 'Stop'
$Here = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot = (Resolve-Path (Join-Path $Here '..\..')).Path
$WildflyFixture = Join-Path $RepoRoot 'bench\wildfly-boot'
$StagedDir = Join-Path $Here 'staged'
$RunLog = Join-Path $StagedDir 'run.log'
$RunStdout = Join-Path $StagedDir 'run.stdout'
$RunStderr = Join-Path $StagedDir 'run.stderr'
$ServerConfigMarker = Join-Path $StagedDir 'server-config.txt'

'' | Set-Content -Path $RunLog -Encoding utf8

function Log([string]$msg) {
    $line = "run-ejbca-deploy: $msg"
    Write-Host $line
    Add-Content -Path $RunLog -Value $line -Encoding utf8
}

$WildflyRunPs1 = Join-Path $WildflyFixture 'run-under-rustjvm.ps1'
if (-not (Test-Path $WildflyRunPs1)) {
    Log "WP8.10 prereq missing: $WildflyRunPs1"
    exit 11
}
if (-not (Test-Path $ServerConfigMarker)) {
    Log "stage not run: $ServerConfigMarker missing"
    exit 10
}

$ServerConfig = (Get-Content $ServerConfigMarker -Raw).Trim()
Log "delegating to $WildflyRunPs1 with --server-config=$ServerConfig"

$env:WILDFLY_HOME = (Join-Path $StagedDir 'wildfly')
$env:SERVER_CONFIG = $ServerConfig

$proc = Start-Process -FilePath 'powershell' `
    -ArgumentList @('-NoProfile','-ExecutionPolicy','Bypass','-File',$WildflyRunPs1) `
    -RedirectStandardOutput $RunStdout `
    -RedirectStandardError $RunStderr `
    -PassThru -NoNewWindow
Log "rust-jvm boot PID=$($proc.Id)"

$Timeout = if ($env:EJBCA_BOOT_TIMEOUT_S) { [int]$env:EJBCA_BOOT_TIMEOUT_S } else { 180 }
$deadline = (Get-Date).AddSeconds($Timeout)
$sawDeployed = $false
$httpCode = 0

[System.Net.ServicePointManager]::ServerCertificateValidationCallback = { $true }

while ((Get-Date) -lt $deadline) {
    if ($proc.HasExited) {
        Log "boot pid exited (rc=$($proc.ExitCode)) before acceptance"
        break
    }
    if (Test-Path $RunStdout) {
        $tail = Get-Content $RunStdout -Tail 200 -ErrorAction SilentlyContinue
        if ($tail -match 'Deployed "ejbca.ear"|WFLYSRV0010.*ejbca\.ear') {
            if (-not $sawDeployed) { Log "saw 'Deployed ejbca.ear' marker"; $sawDeployed = $true }
        }
    }
    try {
        $resp = Invoke-WebRequest -Uri 'https://localhost:8443/ejbca/' `
            -UseBasicParsing -TimeoutSec 5 -ErrorAction Stop -MaximumRedirection 0
        $httpCode = [int]$resp.StatusCode
    } catch {
        if ($_.Exception.Response) {
            $httpCode = [int]$_.Exception.Response.StatusCode
        } else { $httpCode = 0 }
    }
    if ($httpCode -eq 200 -or $httpCode -eq 302) {
        Log "ACCEPT https://localhost:8443/ejbca/ -> HTTP $httpCode"
        if (-not $proc.HasExited) { Stop-Process -Id $proc.Id -Force }
        exit 0
    }
    Start-Sleep -Seconds 3
}

Log "timeout after ${Timeout}s; sawDeployed=$sawDeployed httpCode=$httpCode"
if (-not $proc.HasExited) { Stop-Process -Id $proc.Id -Force }

if ((Get-Content $RunStderr -Raw -ErrorAction SilentlyContinue) -match 'WFLYSRV0025.*started in') {
    Log "WildFly started but ejbca.ear deploy failed"
    exit 1
} elseif ((Get-Content $RunStderr -Raw -ErrorAction SilentlyContinue) -match 'WFLYSRV') {
    Log "WildFly partial boot; deployment scan not reached"
    exit 2
} else {
    Log "JVM-level failure before any WFLYSRV log line"
    exit 124
}
