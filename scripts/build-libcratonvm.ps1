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
cmd /c "`"$vc`" >nul 2>&1 && cd /d `"$repo`" && cargo build $cargoProfileFlag -p libcratonvm"
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

function Build-And-Run($srcName, $exeName) {
    $src = Join-Path $examples $srcName
    $exe = Join-Path $work $exeName
    Write-Host "`n=== $srcName -> $exeName ==="
    cmd /c "`"$vc`" >nul 2>&1 && cd /d `"$work`" && cl /nologo /Fe:`"$exe`" `"$src`" `"$implib`""
    if ($LASTEXITCODE -ne 0) { throw "cl failed for $srcName (exit $LASTEXITCODE)" }
    & $exe
    if ($LASTEXITCODE -ne 0) { throw "$exeName exited with $LASTEXITCODE" }
}

Build-And-Run "embed_smoke.c" "embed_smoke_$Suffix.exe"
Build-And-Run "embed_flat.c"  "embed_flat_$Suffix.exe"

Write-Host "`nlibcratonvm acceptance: OK"
