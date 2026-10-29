# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
#
# Windows/PowerShell twin of `run-all.sh`: run every libFuzzer target in
# this crate for a bounded time and report which ones produced crash
# artifacts.
#
# The target list comes from `cargo fuzz list`, so a target added to
# `Cargo.toml` is picked up automatically.
#
# NOTE: `cargo-fuzz` needs a working libFuzzer, which on Windows means an
# `x86_64-pc-windows-msvc` nightly with the sanitizer runtime available.
# The canonical fuzzing host for this repo is Linux (see `README.md`); this
# script exists so a Windows checkout can at least drive short smoke runs.
#
# Usage:
#   .\run-all.ps1                        # 3600s per target
#   .\run-all.ps1 -Duration 300          # 300s per target (smoke)
#   .\run-all.ps1 -Duration 300 -Targets fuzz_zip_entry,fuzz_signed_jar
#
# Exits non-zero if any target crashed, so it can gate CI.

[CmdletBinding()]
param(
    [int]$Duration = 3600,
    [string[]]$Targets = @(),
    [int]$Timeout = 10,
    [int]$RssLimitMb = 4096,
    [int]$Jobs = 1
)

$ErrorActionPreference = 'Continue'
Set-Location -Path $PSScriptRoot

$artifactDir = Join-Path $PSScriptRoot 'artifacts'
$logDir = Join-Path $artifactDir 'logs'
New-Item -ItemType Directory -Force -Path $artifactDir | Out-Null
New-Item -ItemType Directory -Force -Path $logDir | Out-Null

if ($Targets.Count -eq 0) {
    $listed = & cargo +nightly fuzz list
    if ($LASTEXITCODE -ne 0 -or $null -eq $listed) {
        Write-Error "run-all: 'cargo fuzz list' failed. Is the nightly toolchain installed and cargo-fuzz on PATH?"
        exit 2
    }
    $Targets = @($listed | Where-Object { $_ -and $_.Trim() -ne '' } | ForEach-Object { $_.Trim() })
}

if ($Targets.Count -eq 0) {
    Write-Error 'run-all: no fuzz targets to run.'
    exit 2
}

Write-Host "run-all: $Duration s per target, timeout=$Timeout s, rss=$RssLimitMb MiB"
Write-Host "run-all: artifacts -> $artifactDir"
Write-Host ''

function Get-ArtifactCount([string]$dir) {
    if (-not (Test-Path $dir)) { return 0 }
    return @(Get-ChildItem -Path $dir -File -Recurse -ErrorAction SilentlyContinue).Count
}

$failed = @()
foreach ($t in $Targets) {
    $targetArtifacts = Join-Path $artifactDir $t
    $before = Get-ArtifactCount $targetArtifacts

    Write-Host ("run-all: {0,-26} " -f $t) -NoNewline
    $log = Join-Path $logDir "$t.log"

    # Built as an argument array rather than with backtick continuations:
    # the libFuzzer flags after `--` contain `=` and `-`, which PowerShell
    # would otherwise be tempted to parse.
    $fuzzArgs = @(
        '+nightly', 'fuzz', 'run', $t,
        '--jobs', "$Jobs",
        '--',
        "-max_total_time=$Duration",
        "-timeout=$Timeout",
        "-rss_limit_mb=$RssLimitMb"
    )
    & cargo @fuzzArgs *> $log
    $status = $LASTEXITCODE

    $after = Get-ArtifactCount $targetArtifacts
    $new = $after - $before

    if ($status -eq 0 -and $new -eq 0) {
        Write-Host 'clean'
    }
    else {
        Write-Host "FAILED (exit $status, $new new artifact(s)) -- see $log"
        $failed += $t
    }
}

Write-Host ''
if ($failed.Count -gt 0) {
    Write-Host "run-all: targets with findings: $($failed -join ' ')"
    Write-Host 'run-all: reproduce with'
    Write-Host '         cargo +nightly fuzz run <target> fuzz/artifacts/<target>/<artifact>'
    exit 1
}

Write-Host 'run-all: all targets clean'
