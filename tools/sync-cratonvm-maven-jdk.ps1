param(
    [string]$WorkspaceRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path,
    [ValidateSet("release", "debug")]
    [string]$Profile = "release",
    [string]$DestinationRoot
)

# Canonical Maven/Surefire launcher shim. Surefire requires the executable's
# parent directory to be named `bin` and its basename to start with `java`.
$ErrorActionPreference = 'Stop'
$Src = Join-Path $WorkspaceRoot "target\$Profile\java.exe"
if (-not (Test-Path $Src)) {
    Write-Error ("Missing $Src - the `java.exe` alias is opt-in since the " +
        "binary rename for crates.io safety. Build it with: " +
        "cargo build --$Profile -p cratonvm-cli --features java-bin-alias")
}
$shimRoot = if ($DestinationRoot) {
    $DestinationRoot
} else {
    Join-Path $WorkspaceRoot 'target\cratonvm-maven-jdk'
}
$Bin = Join-Path $shimRoot 'bin'
New-Item -ItemType Directory -Force -Path $Bin | Out-Null
Copy-Item -Path $Src -Destination (Join-Path $Bin 'java.exe') -Force
$Out = Join-Path $Bin 'java.exe'
Write-Host "OK: $Out"
