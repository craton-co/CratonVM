# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
#
# build-libcratonvm.ps1 — build the libcratonvm C-ABI shared/static library and
# run the C acceptance harnesses (embed_smoke.c = JNI Invocation API, embed_flat.c
# = flat cratonvm_* API) against the produced artifacts.
#
# This is the reproducible form of the "orchestrator builds the harness against
# the produced library" acceptance step in docs/feature-designs/embedding-api.md.
#
# Usage (from anywhere; paths are resolved relative to the repo root):
#   pwsh -File scripts/build-libcratonvm.ps1               # release build + run harnesses
#   pwsh -File scripts/build-libcratonvm.ps1 -Profile dev  # debug build
#   pwsh -File scripts/build-libcratonvm.ps1 -NoRun        # build only, skip harnesses
#   pwsh -File scripts/build-libcratonvm.ps1 -Suffix mytag # unique harness exe suffix
#
# Why the INCLUDE dance: libffi-sys's MSVC build only augments INCLUDE with its
# own header dirs when cc surfaces an INCLUDE key; vcvars64 having already set
# INCLUDE suppresses that, so cl.exe can't find fficonfig.h. We prepend libffi's
# four header dirs to INCLUDE in the PARENT environment BEFORE vcvars (vcvars
# keeps %INCLUDE%), which fixes a cold/fresh-worktree build.

[CmdletBinding()]
param(
    [string]$Profile = "release",
    [switch]$NoRun,
    [string]$Suffix = "acc"
)

$ErrorActionPreference = "Stop"

# Repo root = parent of this script's dir.
$repo = Split-Path -Parent $PSScriptRoot
Write-Host "repo root: $repo"

# --- locate vcvars64.bat ---------------------------------------------------
$vcCandidates = @(
    "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat",
    "C:\Program Files\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat",
    "C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat",
    "C:\Program Files (x86)\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat"
)
$vc = $vcCandidates | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $vc) { throw "vcvars64.bat not found in any known location" }
Write-Host "vcvars: $vc"

# --- libffi-sys INCLUDE fix (discover registry copies dynamically) ---------
$registry = Join-Path $env:USERPROFILE ".cargo\registry\src"
$ffiDirs = @()
if (Test-Path $registry) {
    $ffiDirs = Get-ChildItem -Path $registry -Directory -Recurse -Filter "libffi-sys-*" -ErrorAction SilentlyContinue |
        ForEach-Object { $_.FullName }
}
$incParts = @()
foreach ($d in $ffiDirs) {
    $incParts += "$d\libffi", "$d\libffi\include", "$d\include\msvc", "$d\libffi\src\x86"
}
if ($incParts.Count -gt 0) {
    $env:INCLUDE = ($incParts -join ";")
    Write-Host "INCLUDE seeded with $($ffiDirs.Count) libffi-sys copy/copies"
}

$env:RUST_MIN_STACK = "536870912"
Remove-Item Env:\CARGO_TARGET_DIR -ErrorAction SilentlyContinue

# --- build the cdylib/staticlib --------------------------------------------
$cargoProfileFlag = if ($Profile -eq "release") { "--release" } else { "" }
$targetSub = if ($Profile -eq "release") { "release" } else { "debug" }
Write-Host "building libcratonvm ($Profile)..."
# `2>&1` is INSIDE the cmd string so cargo's stderr (progress + warnings) is
# merged into cmd's stdout and never reaches PowerShell as a native-stderr
# stream — under `$ErrorActionPreference = Stop`, a single cargo warning on
# stderr would otherwise be promoted to a terminating NativeCommandError.
cmd /c "`"$vc`" >nul 2>&1 && cd /d `"$repo`" && cargo build $cargoProfileFlag -p libcratonvm 2>&1"
if ($LASTEXITCODE -ne 0) { throw "cargo build failed (exit $LASTEXITCODE)" }

# cargo names the cdylib/import-lib after the crate (`libcratonvm`): on Windows
# that is `libcratonvm.dll` + `libcratonvm.dll.lib`.
$targetDir = Join-Path $repo "target\$targetSub"
$dll = Join-Path $targetDir "libcratonvm.dll"
$implib = Join-Path $targetDir "libcratonvm.dll.lib"
foreach ($f in @($dll, $implib)) {
    if (-not (Test-Path $f)) { throw "expected artifact missing: $f" }
}
Write-Host "artifacts OK: libcratonvm.dll + libcratonvm.dll.lib in $targetDir"

if ($NoRun) { Write-Host "-NoRun: skipping harnesses"; exit 0 }

# --- compile + run the two C harnesses (unique exe names) ------------------
$work = Join-Path $repo "scratch\embedacc"
New-Item -ItemType Directory -Force -Path $work | Out-Null
Copy-Item $dll (Join-Path $work "libcratonvm.dll") -Force
$examples = Join-Path $repo "libcratonvm\examples"
$includeDir = Join-Path $repo "libcratonvm\include"

function Build-And-Run($srcName, $exeName) {
    $src = Join-Path $examples $srcName
    $exe = Join-Path $work $exeName
    Write-Host "`n=== $srcName -> $exeName ==="
    # `/I include` lets a harness #include the public headers (cratonvm.h /
    # cratonvm_helpers.h); harmless for the standalone harnesses that re-declare.
    cmd /c "`"$vc`" >nul 2>&1 && cd /d `"$work`" && cl /nologo /I`"$includeDir`" /Fe:`"$exe`" `"$src`" `"$implib`" 2>&1"
    if ($LASTEXITCODE -ne 0) { throw "cl failed for $srcName (exit $LASTEXITCODE)" }
    # Run the harness with EAP relaxed: a native exe writing to stderr (even
    # benign VM diagnostics) must not be promoted to a terminating error — gate
    # on the real exit code instead.
    $prevEAP = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    & $exe
    $rc = $LASTEXITCODE
    $ErrorActionPreference = $prevEAP
    if ($rc -ne 0) { throw "$exeName exited with $rc" }
}

Build-And-Run "embed_smoke.c"   "embed_smoke_$Suffix.exe"
Build-And-Run "embed_flat.c"    "embed_flat_$Suffix.exe"
Build-And-Run "embed_helpers.c" "embed_helpers_$Suffix.exe"

Write-Host "`nlibcratonvm acceptance: OK"
