# Copies the built `java.exe` launcher next to a `bin` directory so Maven Surefire
# accepts `-Djvm=...` (Surefire requires parent dir name `bin` and basename starting with `java`).
param(
    [string]$WorkspaceRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path,
    [ValidateSet("release", "debug")]
    [string]$Profile = "release"
)
$ErrorActionPreference = "Stop"
$src = Join-Path $WorkspaceRoot "target\$Profile\java.exe"
if (-not (Test-Path $src)) {
    throw "Missing '$src'. The `java.exe` binary is opt-in since the binary " +
          "rename for crates.io safety. Build with the alias feature: " +
          "cargo build --$Profile -p cratonvm-cli --features java-bin-alias"
}
$dstDir = Join-Path $WorkspaceRoot "target\cratonvm-maven-shim\bin"
$dst = Join-Path $dstDir "java.exe"
New-Item -ItemType Directory -Force -Path $dstDir | Out-Null
Copy-Item -LiteralPath $src -Destination $dst -Force
Write-Host "Maven Surefire JVM shim: $dst"
