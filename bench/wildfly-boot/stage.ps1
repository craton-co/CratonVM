# bench/wildfly-boot/stage.ps1
# WP8.10.1 — Windows PowerShell 5.1 mirror of stage.sh. Downloads
# wildfly-32.0.1.Final.tar.gz, validates sha256, extracts under staged/.
#
# Idempotent. Uses tar.exe (shipped with Windows 10 1803+ / 11) or
# falls back to a Compress-Archive note. Net.WebClient handles HTTPS.
#
# Exit codes match stage.sh: 0 ok, 10 no http client + no cache,
# 11 sha mismatch, 12 extract failure, 13 no sha tool.

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$Here     = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot = Resolve-Path (Join-Path $Here '..\..')

$WildflyVersion = '32.0.1.Final'
$WildflyUrl = "https://github.com/wildfly/wildfly/releases/download/${WildflyVersion}/wildfly-${WildflyVersion}.tar.gz"
# UNVERIFIED: same placeholder as stage.sh — replace on first real run.
$WildflySha256 = 'REPLACE_WITH_REAL_SHA256_ON_FIRST_RUN'

if ($env:WILDFLY_CACHE_DIR) { $CacheDir = $env:WILDFLY_CACHE_DIR } else { $CacheDir = Join-Path $Here '.cache' }
$StagedDir   = Join-Path $Here 'staged'
$WildflyHome = Join-Path $StagedDir 'wildfly'
$Tarball     = Join-Path $CacheDir ("wildfly-${WildflyVersion}.tar.gz")
$Stamp       = Join-Path $StagedDir ".staged-${WildflyVersion}"

New-Item -ItemType Directory -Force -Path $CacheDir   | Out-Null
New-Item -ItemType Directory -Force -Path $StagedDir  | Out-Null

if ((Test-Path $Stamp) -and (Test-Path (Join-Path $WildflyHome 'jboss-modules.jar'))) {
    Write-Output "stage: WildFly $WildflyVersion already staged at $WildflyHome"
    exit 0
}

if (-not (Test-Path $Tarball)) {
    Write-Output "stage: downloading $WildflyUrl -> $Tarball"
    try {
        $oldPref = $ProgressPreference
        $ProgressPreference = 'SilentlyContinue'
        try {
            [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
            Invoke-WebRequest -Uri $WildflyUrl -OutFile $Tarball -UseBasicParsing
        } finally { $ProgressPreference = $oldPref }
    } catch {
        Write-Error "stage: download failed: $_"
        exit 10
    }
} else {
    Write-Output "stage: cache hit at $Tarball"
}

if ($env:WILDFLY_SKIP_SHA -ne '1') {
    try {
        $hashObj = Get-FileHash -Algorithm SHA256 -Path $Tarball
        $actual = $hashObj.Hash.ToLower()
    } catch {
        Write-Error "stage: Get-FileHash failed: $_"
        exit 13
    }
    if ($WildflySha256 -eq 'REPLACE_WITH_REAL_SHA256_ON_FIRST_RUN') {
        Write-Output "stage: WARN pinned sha256 is placeholder; observed=$actual"
        Write-Output "stage: WARN edit stage.ps1 and replace WildflySha256 with that value"
    } elseif ($actual -ne $WildflySha256.ToLower()) {
        Write-Error "stage: sha256 mismatch (expected=$WildflySha256, actual=$actual)"
        Remove-Item $Tarball -Force -ErrorAction SilentlyContinue
        exit 11
    } else {
        Write-Output "stage: sha256 OK ($actual)"
    }
}

$tarExe = Get-Command tar.exe -ErrorAction SilentlyContinue
if (-not $tarExe) {
    Write-Error "stage: tar.exe not found (Windows 10 1803+ ships it; otherwise install Git Bash or 7-Zip)"
    exit 12
}

$VersionedDir = Join-Path $StagedDir "wildfly-${WildflyVersion}"
if (Test-Path $WildflyHome)   { Remove-Item $WildflyHome   -Recurse -Force }
if (Test-Path $VersionedDir)  { Remove-Item $VersionedDir  -Recurse -Force }

Write-Output "stage: extracting $Tarball into $StagedDir"
& $tarExe.Source -xzf $Tarball -C $StagedDir
if ($LASTEXITCODE -ne 0) {
    Write-Error "stage: tar extract failed (rc=$LASTEXITCODE)"
    exit 12
}
if (Test-Path $VersionedDir) {
    Rename-Item -Path $VersionedDir -NewName 'wildfly'
}
if (-not (Test-Path (Join-Path $WildflyHome 'jboss-modules.jar'))) {
    Write-Error "stage: expected jboss-modules.jar at $WildflyHome after extract"
    exit 12
}

(Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ') | Set-Content -Path $Stamp -Encoding utf8
Write-Output "stage: OK ($WildflyHome)"
exit 0
