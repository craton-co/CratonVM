# Copies cratonvm's java launcher into a JDK-shaped tree so Maven Surefire 3.x
# accepts -Djvm=.../bin/java.exe (see SystemUtils.endsWithJavaPath: parent must be "bin").
$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$Src = Join-Path $Root 'target\release\java.exe'
if (-not (Test-Path $Src)) {
    Write-Error "Missing $Src - run: cargo build --release -p cratonvm-cli"
}
$Bin = Join-Path $Root 'target\cratonvm-mavenjdk\bin'
New-Item -ItemType Directory -Force -Path $Bin | Out-Null
Copy-Item -Path $Src -Destination (Join-Path $Bin 'java.exe') -Force
$Out = Join-Path $Bin 'java.exe'
Write-Host "OK: $Out"
