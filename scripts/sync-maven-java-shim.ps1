# SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
# Copyright 2024-2026 Craton Software Company

# Compatibility wrapper. The maintained implementation lives under tools/.
param(
    [string]$WorkspaceRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path,
    [ValidateSet("release", "debug")]
    [string]$Profile = "release",
    [string]$DestinationRoot
)
$ErrorActionPreference = "Stop"
& (Join-Path $WorkspaceRoot "tools\sync-cratonvm-maven-jdk.ps1") `
    -WorkspaceRoot $WorkspaceRoot `
    -Profile $Profile `
    -DestinationRoot $DestinationRoot
