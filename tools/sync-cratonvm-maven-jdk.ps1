# Copies cratonvm's java launcher into a JDK-shaped tree so Maven Surefire 3.x
# accepts -Djvm=.../bin/java.exe (see SystemUtils.endsWithJavaPath: parent must be "bin").
$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$Src = Join-Path $Root 'target\release\java.exe'
if (-not (Test-Path $Src)) {
    Write-Error ("Missing $Src - the `java.exe` alias is opt-in since the " +
        "binary rename for crates.io safety. Build it with: " +
        "cargo build --release -p cratonvm-cli --features java-bin-alias")
}
$Bin = Join-Path $Root 'target\cratonvm-mavenjdk\bin'
New-Item -ItemType Directory -Force -Path $Bin | Out-Null
Copy-Item -Path $Src -Destination (Join-Path $Bin 'java.exe') -Force
$Out = Join-Path $Bin 'java.exe'
Write-Host "OK: $Out"
