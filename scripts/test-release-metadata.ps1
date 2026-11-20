# SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
# Copyright Craton Software Company

$ErrorActionPreference = "Stop"
$taskMetadata = & cargo metadata --no-deps --format-version 1 --offline | ConvertFrom-Json
if ($LASTEXITCODE -ne 0) { throw "cargo metadata failed" }

# Mock subsequent Cargo calls so even a broken gate cannot package or publish.
function cargo { $taskMetadata | ConvertTo-Json -Depth 100 }

& "$PSScriptRoot/release-crates-dry-run.ps1" -MetadataOnly 6>$null 3>$null

$originalLicense = $taskMetadata.packages[0].license
$taskMetadata.packages[0].license = "Apache-2.0"
try {
    & "$PSScriptRoot/release-crates-dry-run.ps1" -MetadataOnly 6>$null 3>$null
    throw "Incorrect license was accepted"
} catch {
    if ($_.Exception.Message -notlike "Incorrect release metadata*") { throw }
} finally {
    $taskMetadata.packages[0].license = $originalLicense
}

try {
    & "$PSScriptRoot/release-crates-dry-run.ps1" -Execute 6>$null 3>$null
    throw "Blocked dependency graph was accepted"
} catch {
    if ($_.Exception.Message -notlike "Resolve the registry dependency blockers*") { throw }
}
Write-Host "Release metadata checks: valid metadata, wrong license, blocked execution passed."
