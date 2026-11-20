# SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
# Copyright 2024-2026 Craton Software Company

param(
    [switch]$Execute,
    [string]$Until,
    [switch]$SkipPackage,
    [switch]$MetadataOnly
)

$ErrorActionPreference = "Stop"

# Dependency order: every crate appears after all of its non-dev workspace
# dependencies (verify with `cargo metadata --no-deps`). jfr, jit-aarch64 and
# the two native-builtins-* crates are required by jit/classloading/native-builtins.
$publishOrder = @(
    "cratonvm-types",
    "cratonvm-reader",
    "cratonvm-jfr",
    "cratonvm-gc-core",
    "cratonvm-gc-gen",
    "cratonvm-gc-g1",
    "cratonvm-gc-zgc",
    "cratonvm-gc",
    "cratonvm-native-api",
    "cratonvm-jit-api",
    "cratonvm-jit-aarch64",
    "cratonvm-native-builtins-crypto",
    "cratonvm-native-builtins-security",
    "cratonvm-native-collections",
    "cratonvm-native-io",
    "cratonvm-classloading",
    "cratonvm-jit",
    "cratonvm-native-builtins",
    "cratonvm-native-builtins-jfr",
    "cratonvm-native-builtins-jmx",
    "cratonvm-vm",
    "cratonvm-cli",
    "libcratonvm",
    "cratonvm-embed",
    "cratonvm-difftest"
)

$metadata = & cargo metadata --no-deps --format-version 1 --offline | ConvertFrom-Json
if ($LASTEXITCODE -ne 0) { throw "cargo metadata failed" }
$packages = @($metadata.packages | Where-Object { $_.id -in $metadata.workspace_members })
$withheld = @($packages | Where-Object { $null -ne $_.publish -and $_.publish.Count -eq 0 } | ForEach-Object name)
$withheld += "cratonvm-fuzz"
foreach ($package in $packages) {
    if ($package.license -ne "GPL-2.0-only WITH Classpath-exception-2.0" -or
        "Craton Software Company" -notin $package.authors -or
        $package.homepage -ne "https://craton.com.ar" -or
        $package.repository -ne "https://github.com/craton-co/CVM" -or
        -not $package.documentation -or -not $package.readme) {
        throw "Incorrect release metadata for $($package.name)"
    }
    $packageDir = Split-Path -Parent $package.manifest_path
    foreach ($file in @("LICENSE", "NOTICE", "THIRD-PARTY-NOTICES.md", $package.readme)) {
        if (-not (Test-Path -LiteralPath (Join-Path $packageDir $file))) {
            throw "$($package.name) is missing $file"
        }
    }
}

if ($Until) {
    $stopIndex = [array]::IndexOf($publishOrder, $Until)
    if ($stopIndex -lt 0) {
        throw "Unknown -Until crate '$Until'. Expected one of: $($publishOrder -join ', ')"
    }
    $publishOrder = $publishOrder[0..$stopIndex]
}

Write-Host "CratonVM crates.io dry-run checklist"
Write-Host "Publish order: $($publishOrder -join ' -> ')"
Write-Host "Withheld packages: $($withheld -join ', ')"
Write-Host "Metadata gate passed: Craton Software Company, craton.com.ar, GPLv2 with Classpath Exception, README and legal notices."
Write-Host "Dependency gate: registry packages must exist even for optional dependencies."
Write-Host ""

$blockers = @()
foreach ($package in $packages | Where-Object { $_.name -in $publishOrder }) {
    foreach ($dependency in $package.dependencies | Where-Object { $_.kind -ne "dev" }) {
        if ($dependency.name -in $withheld) {
            $blockers += "$($package.name) -> $($dependency.name) is withheld from crates.io"
        }
    }
    if ($package.name -eq "cratonvm-native-builtins") {
        $blockers += "cratonvm-native-builtins requires the local rustls patch; registry consumers do not inherit workspace patches"
    }
}
foreach ($blocker in $blockers) { Write-Warning $blocker }
if ($MetadataOnly) { return }
if ($Execute -and $blockers.Count) {
    throw "Resolve the registry dependency blockers above before this publish dry-run."
}

function Invoke-Step {
    param(
        [string]$Crate,
        [string[]]$CargoArgs
    )

    $display = "cargo $($CargoArgs -join ' ')"
    if (-not $Execute) {
        Write-Host "[preview][$Crate] $display"
        return
    }

    Write-Host "[run][$Crate] $display"
    & cargo @CargoArgs
    if ($LASTEXITCODE -ne 0) {
        throw "Command failed for $Crate with exit code $LASTEXITCODE`: $display"
    }
}

foreach ($crate in $publishOrder) {
    Invoke-Step -Crate $crate -CargoArgs @("package", "-p", $crate, "--list")
    if (-not $SkipPackage) {
        Invoke-Step -Crate $crate -CargoArgs @("package", "-p", $crate)
    }
    Invoke-Step -Crate $crate -CargoArgs @("publish", "-p", $crate, "--dry-run")
    Write-Host ""
}

if (-not $Execute) {
    Write-Host "Preview only. Re-run with -Execute after inspecting docs/RELEASE_READINESS.md."
}
