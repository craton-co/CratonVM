# SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
# Copyright 2024-2026 Craton Software Company

param(
    [switch]$Execute,
    [string]$Until,
    [switch]$SkipPackage
)

$ErrorActionPreference = "Stop"

# Dependency order: every crate appears after all of its non-dev workspace
# dependencies (verify with `cargo metadata --no-deps`). jfr, jit-aarch64 and
# the two native-builtins-* crates are required by jit/classloading/native-builtins.
$publishOrder = @(
    "cratonvm-types",
    "cratonvm-reader",
    "cratonvm-jfr",
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
    "cratonvm-vm",
    "cratonvm-cli",
    "libcratonvm",
    "cratonvm-embed",
    "cratonvm-difftest"
)

$withheld = @(
    "cratonvm-native-awt",
    "cratonvm-jit-cuda",
    "cratonvm-gpu",
    "cratonvm-cuda-bridge",
    "cratonvm-fuzz"
)

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
Write-Host "Metadata gate: Craton Software Company authors, Apache-2.0 license, README, and repository/homepage/documentation links."
Write-Host "Dependency gate: default features for publishable crates must not pull withheld packages."
Write-Host ""

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
