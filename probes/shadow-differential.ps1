# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
#
# Run ShadowDifferentialProbe on HotSpot and on CratonVM and diff the two
# transcripts.
#
# WHY THIS SCRIPT EXISTS
# ----------------------
# Both instrument holes W7-42 records were holes in how the differential was
# RUN, not in the probe's Java and not in the VM:
#
#   1. The two sides were compiled from different source. W7-33 excised five
#      statements into a scratchpad copy so it could measure two dead sections
#      on an unfixed binary; the next run took HotSpot from the tree and
#      CratonVM from the copy. Five observables were then present on one side
#      and simply absent on the other, and were read as five VM divergences.
#      A differential's whole premise is one class file, two VMs. There was
#      nothing in the pipeline that could notice the premise had lapsed.
#
#   2. VM diagnostics reached the transcript. `[SUREFIRE-NPE]` frames are on
#      stderr and always were -- measured on the binary -- so a transcript that
#      carried them was captured with the streams merged. Seven foreign lines
#      landed in the middle of the observable stream and shifted the alignment
#      of every row after them.
#
# So this script compiles ONCE, runs both sides against that one output
# directory, and keeps stdout and stderr apart at the OS level
# (`Start-Process -RedirectStandardOutput/-RedirectStandardError`, not a shell
# redirect -- PowerShell 5.1 wraps a native executable's stderr in an
# ErrorRecord when it is redirected inline). Only stdout is diffed; stderr is
# kept beside it, because a VM diagnostic is often the reason for a divergence
# and throwing it away is its own blindness.
#
# It then checks the probe's own ledger before reporting any diff at all. A
# differing `PROBE-MANIFEST-DIGEST` means the two sides were not built from
# the same probe and no comparison between them means anything; a non-zero
# `PROBE-LEDGER` means one side lost rows for a reason that is the
# instrument's, not the VM's. Both are reported as ERRORS, ahead of the diff,
# because reading a diff taken under either condition is what produced the two
# holes in the first place.
#
# Read the diff in this order: SECTION-DIED (that section is unmeasured, not
# clean), then MISSING-OBSERVABLE, then a missing PROBE-DONE tail, then
# `...-after-100` markers, then value differences.
#
#   .\probes\shadow-differential.ps1 `
#       -Cratonvm .\target\release\cratonvm.exe `
#       -Java "C:\jdk-25.0.3.9-hotspot\bin\java.exe"

[CmdletBinding()]
param(
    [string]$Cratonvm = ".\target\release\cratonvm.exe",
    [string]$Java = "java",
    [string]$OutDir = "",
    # Passed to CratonVM only. `--real-jdk` is the mode the shadow population
    # is adjudicated in; `--jdk-only` and Compatible are the other two arms.
    [string[]]$VmArgs = @("--real-jdk")
)

$ErrorActionPreference = "Stop"

$repo = Split-Path -Parent $PSScriptRoot
$probe = Join-Path $repo "probes\ShadowDifferentialProbe.java"
if (-not (Test-Path $probe)) { throw "probe not found: $probe" }

if ($OutDir -eq "") {
    $OutDir = Join-Path ([System.IO.Path]::GetTempPath()) ("shadowdiff-" + [System.Guid]::NewGuid().ToString("N").Substring(0, 8))
}
$classes = Join-Path $OutDir "classes"
New-Item -ItemType Directory -Force -Path $classes | Out-Null

# ---- ONE compile. Both sides run these exact class files. -----------------
Write-Host "compiling $probe -> $classes"
& javac -d $classes $probe
if ($LASTEXITCODE -ne 0) { throw "javac failed" }

# Both sides are pinned to the same encoding and locale. Without this the
# text/format sections diff on the HOST's defaults rather than on the VM.
$commonProps = @(
    "-Dstdout.encoding=UTF-8",
    "-Dstderr.encoding=UTF-8",
    "-Duser.language=en",
    "-Duser.country=US"
)

function Invoke-Side {
    param([string]$Label, [string]$Exe, [string[]]$Argv)

    $out = Join-Path $OutDir "$Label.out"
    $err = Join-Path $OutDir "$Label.err"
    Write-Host "running $Label : $Exe $($Argv -join ' ')"
    $p = Start-Process -FilePath $Exe -ArgumentList $Argv -NoNewWindow -Wait -PassThru `
        -RedirectStandardOutput $out -RedirectStandardError $err
    [pscustomobject]@{
        Label = $Label
        Out   = $out
        Err   = $err
        Code  = $p.ExitCode
    }
}

$hsArgs = $commonProps + @("-cp", $classes, "ShadowDifferentialProbe")
$cvArgs = $VmArgs + $commonProps + @("-cp", $classes, "ShadowDifferentialProbe")

$hs = Invoke-Side -Label "hotspot" -Exe $Java -Argv $hsArgs
$cv = Invoke-Side -Label "cratonvm" -Exe $Cratonvm -Argv $cvArgs

# ---- the probe's own ledger, read BEFORE the diff -------------------------
function Get-Marker {
    param([string]$Path, [string]$Key)
    $hit = Select-String -Path $Path -Pattern ("^" + [regex]::Escape($Key) + "=") -SimpleMatch:$false |
        Select-Object -First 1
    if ($null -eq $hit) { return $null }
    return $hit.Line.Substring($Key.Length + 1)
}

$problems = @()

foreach ($side in @($hs, $cv)) {
    $done = Select-String -Path $side.Out -Pattern "^PROBE-DONE$" | Select-Object -First 1
    if ($null -eq $done) {
        $problems += "$($side.Label): no PROBE-DONE -- the run did not reach the end (exit $($side.Code))"
    }
    $ledger = Get-Marker -Path $side.Out -Key "PROBE-LEDGER"
    if ($null -eq $ledger) {
        $problems += "$($side.Label): no PROBE-LEDGER line"
    } elseif ($ledger -notmatch "^missing:0,undeclared:0,duplicate:0,multiline:0,unrenderable:0$") {
        $problems += "$($side.Label): PROBE-LEDGER=$ledger -- the instrument lost or mangled rows on this side"
    }
}

$hsDigest = Get-Marker -Path $hs.Out -Key "PROBE-MANIFEST-DIGEST"
$cvDigest = Get-Marker -Path $cv.Out -Key "PROBE-MANIFEST-DIGEST"
if ($hsDigest -ne $cvDigest) {
    $problems += "MANIFEST DIGEST MISMATCH: hotspot=$hsDigest cratonvm=$cvDigest -- the two sides were NOT built from the same probe; the diff below is meaningless"
}

if ($problems.Count -gt 0) {
    Write-Host ""
    Write-Host "=== INSTRUMENT ERRORS (read these before the diff) ==="
    foreach ($p in $problems) { Write-Host "  ! $p" }
}

# ---- the diff, on stdout only --------------------------------------------
$hsLines = Get-Content $hs.Out
$cvLines = Get-Content $cv.Out
$diff = Compare-Object -ReferenceObject $hsLines -DifferenceObject $cvLines -SyncWindow 200

Write-Host ""
Write-Host "=== transcripts ==="
Write-Host ("  hotspot  stdout {0,5} lines  stderr {1,5} lines  {2}" -f $hsLines.Count, (Get-Content $hs.Err).Count, $hs.Out)
Write-Host ("  cratonvm stdout {0,5} lines  stderr {1,5} lines  {2}" -f $cvLines.Count, (Get-Content $cv.Err).Count, $cv.Out)
Write-Host ""
Write-Host "=== divergent observables: $($diff.Count) ==="
# Compare-Object groups by side, which puts a row and its counterpart dozens of
# lines apart. Sort by the observable KEY so the pair for one observable reads
# as a pair; a row present on only one side then stands alone, which is the
# shape both instrument holes had and the shape worth noticing first.
$rows = foreach ($d in $diff) {
    $text = [string]$d.InputObject
    $eq = $text.IndexOf("=")
    if ($eq -lt 0) { $key = $text } else { $key = $text.Substring(0, $eq) }
    if ($d.SideIndicator -eq "<=") { $mark = "<" } else { $mark = ">" }
    [pscustomobject]@{ Key = $key; Mark = $mark; Text = $text }
}
foreach ($r in ($rows | Sort-Object Key, Mark)) {
    Write-Host "$($r.Mark) $($r.Text)"
}

if ($problems.Count -gt 0) { exit 2 }
if ($diff.Count -gt 0) { exit 1 }
exit 0
