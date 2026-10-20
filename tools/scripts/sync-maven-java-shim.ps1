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
