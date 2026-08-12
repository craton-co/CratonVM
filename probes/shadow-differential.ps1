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
# TWO STANDING RULES FOR THIS AREA
# --------------------------------
#   * Diff against a transcript this script produced in the SAME run, and
#     against `PROBE-MANIFEST-DIGEST`. NEVER against W7-4's retired oracle:
#     it predates the manifest ledger, and diffing a current run against it
#     MANUFACTURES divergence out of rows the probe has since gained.
#     RETIREMENT-20260812.md retires it for exactly that reason. This script
#     stores no baseline transcript on purpose -- a frozen expected-output
#     file beside a probe that keeps growing is that same trap with a filename.
#   * A re-measurement is ONE compile, both sides. Two builds are two objects
#     and are not comparable; that is hole 1, and it is why the compile above
#     happens once and both `Invoke-Side` calls are handed the same `$classes`.
#
# WHAT THIS SCRIPT CHECKS BEYOND THE DIFF (added 2026-08-12)
# ----------------------------------------------------------
#   * On a digest mismatch it prints NO DIFF AT ALL. Printing a meaningless
#     diff under a warning is how a warning gets scrolled past, and scrolling
#     past exactly this condition is what produced hole 1.
#   * `PROBE-LEDGER` is parsed as key:value pairs and every field must be 0,
#     rather than matched against a frozen field list. A frozen list goes red
#     on a HEALTHY run the day the probe gains a counter, and a check that
#     fires on a healthy tree gets muted rather than read.
#   * The stderr sidecar is READ, not merely kept. W7-42 found a swallowed
#     `NoSuchMethodError` there (`Formatter.close()` invoking `close()` on a
#     `StringBuilder`): HotSpot would propagate it, this VM logs a WARN and
#     continues with the right answer, so no stdout observable moves and every
#     probe in this campaign is blind to it. Keeping the file and never
#     reading it is the same blindness one step further back.
#   * A `provenance.txt` is written beside the transcripts. The digest catches
#     two sides built from different probes WITHIN a run; it cannot tell a
#     reader later which probe, which binary and which javac produced a
#     transcript sitting on disk -- and "the scratchpad copy outlived the run"
#     is hole 1's actual mechanism.
#   * Both transcripts are read as UTF-8. The sides are pinned to
#     `-Dstdout.encoding=UTF-8`, and PowerShell 5.1's `Get-Content` otherwise
#     decodes them in the host ANSI codepage, which mangles the currency and
#     text rows this probe exists to adjudicate.
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
        Exe   = $Exe
        Argv  = ($Argv -join ' ')
        Out   = $out
        Err   = $err
        Code  = $p.ExitCode
    }
}

# Resolve an executable to something a later reader can identify. A transcript
# whose producer cannot be named is how a stale artefact survives a rebuild.
function Get-ExeIdentity {
    param([string]$Exe)
    $cmd = Get-Command $Exe -ErrorAction SilentlyContinue
    if ($null -eq $cmd) { return "$Exe (not resolvable on PATH)" }
    $path = $cmd.Source
    if ([string]::IsNullOrEmpty($path)) { return "$Exe (no file path)" }
    if (-not (Test-Path $path)) { return "$path (missing)" }
    $item = Get-Item $path
    $hash = (Get-FileHash -Algorithm SHA256 -Path $path).Hash
    return ("{0}  sha256={1}  mtime={2:o}  bytes={3}" -f $path, $hash, $item.LastWriteTimeUtc, $item.Length)
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

# Every field of `PROBE-LEDGER` must be zero, parsed rather than matched
# against a frozen field list. The list is expected to grow -- the probe gained
# `multiline` and `unrenderable` after the first two counters -- and a frozen
# pattern goes red on a HEALTHY run the day it does. A check that fires on a
# healthy tree gets muted, which is the failure this whole record is about.
function Get-LedgerViolations {
    param([string]$Ledger)
    $bad = @()
    foreach ($field in ($Ledger -split ',')) {
        $trimmed = $field.Trim()
        if ($trimmed -eq "") { continue }
        $kv = $trimmed -split ':', 2
        if ($kv.Count -ne 2) {
            $bad += "unparsable ledger field '$trimmed'"
            continue
        }
        $n = 0
        if (-not [int]::TryParse($kv[1].Trim(), [ref]$n)) {
            $bad += "non-numeric ledger field '$trimmed'"
            continue
        }
        if ($n -ne 0) { $bad += "$($kv[0])=$n" }
    }
    return $bad
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
    } else {
        $violations = @(Get-LedgerViolations -Ledger $ledger)
        if ($violations.Count -gt 0) {
            $problems += "$($side.Label): PROBE-LEDGER=$ledger -- the instrument lost or mangled rows on this side ($($violations -join '; '))"
        }
    }
}

$hsDigest = Get-Marker -Path $hs.Out -Key "PROBE-MANIFEST-DIGEST"
$cvDigest = Get-Marker -Path $cv.Out -Key "PROBE-MANIFEST-DIGEST"
$digestMismatch = ($hsDigest -ne $cvDigest)
if ($digestMismatch) {
    $problems += "MANIFEST DIGEST MISMATCH: hotspot=$hsDigest cratonvm=$cvDigest -- the two sides were NOT built from the same probe; NO diff is printed below"
}

# Same probe, same declarations, so the two sides must have emitted the same
# number of rows. Both ledgers can read `missing:0` while the counts differ --
# each side is only self-consistent -- and that combination is hole 1's shape.
$hsEmitted = Get-Marker -Path $hs.Out -Key "PROBE-OBSERVABLES-EMITTED"
$cvEmitted = Get-Marker -Path $cv.Out -Key "PROBE-OBSERVABLES-EMITTED"
if ($hsEmitted -ne $cvEmitted) {
    $problems += "PROBE-OBSERVABLES-EMITTED differs: hotspot=$hsEmitted cratonvm=$cvEmitted -- one side emitted a different number of observables while its own ledger was self-consistent"
}

if ($problems.Count -gt 0) {
    Write-Host ""
    Write-Host "=== INSTRUMENT ERRORS (read these before the diff) ==="
    foreach ($p in $problems) { Write-Host "  ! $p" }
}

# ---- provenance, written beside the transcripts ---------------------------
# `PROBE-MANIFEST-DIGEST` proves the two sides of ONE run agree. It cannot tell
# a reader who finds these files later which probe, which binary and which
# javac produced them -- and hole 1's actual mechanism was a scratchpad copy of
# the probe outliving the run that made it.
$javacIdentity = "unknown"
try {
    $javacIdentity = (& javac -version | Out-String).Trim()
} catch {
    $javacIdentity = "javac -version failed: $($_.Exception.Message)"
}
if ($javacIdentity -eq "") { $javacIdentity = "unknown (javac printed nothing on stdout)" }

$provenanceLines = @()
$provenanceLines += "# ShadowDifferentialProbe differential run -- provenance"
$provenanceLines += "# ONE compile, both sides. See probes/shadow-differential.ps1 and"
$provenanceLines += "# W7-42-differential-instrument-holes.md. Do NOT diff these transcripts"
$provenanceLines += "# against any stored oracle; diff them against each other."
$provenanceLines += "run-utc        = " + (Get-Date).ToUniversalTime().ToString("o")
$provenanceLines += "probe-source   = $probe"
$provenanceLines += "probe-sha256   = " + (Get-FileHash -Algorithm SHA256 -Path $probe).Hash
$provenanceLines += "classes-dir    = $classes"
$provenanceLines += "javac          = $javacIdentity"
$provenanceLines += "manifest-digest= hotspot=$hsDigest cratonvm=$cvDigest"
foreach ($side in @($hs, $cv)) {
    $provenanceLines += "$($side.Label)-exe   = " + (Get-ExeIdentity -Exe $side.Exe)
    $provenanceLines += "$($side.Label)-argv  = $($side.Argv)"
    $provenanceLines += "$($side.Label)-exit  = $($side.Code)"
}
$provenance = Join-Path $OutDir "provenance.txt"
$provenanceLines | Set-Content -Path $provenance -Encoding utf8
Write-Host ""
Write-Host "provenance: $provenance"

# ---- the transcripts, decoded as UTF-8 ------------------------------------
# The sides are pinned to `-Dstdout.encoding=UTF-8`; PowerShell 5.1's
# `Get-Content` otherwise decodes in the host ANSI codepage and mangles the
# currency and text rows this probe exists to adjudicate. `@()` so `.Count` is
# a line count on an empty or single-line file rather than $null or a scalar.
$hsLines = @(Get-Content $hs.Out -Encoding UTF8)
$cvLines = @(Get-Content $cv.Out -Encoding UTF8)
$hsErrLines = @(Get-Content $hs.Err -Encoding UTF8)
$cvErrLines = @(Get-Content $cv.Err -Encoding UTF8)

Write-Host ""
Write-Host "=== transcripts ==="
Write-Host ("  hotspot  stdout {0,5} lines  stderr {1,5} lines  {2}" -f $hsLines.Count, $hsErrLines.Count, $hs.Out)
Write-Host ("  cratonvm stdout {0,5} lines  stderr {1,5} lines  {2}" -f $cvLines.Count, $cvErrLines.Count, $cv.Out)
Write-Host ("  hotspot  stderr {0}" -f $hs.Err)
Write-Host ("  cratonvm stderr {0}" -f $cv.Err)

# ---- the stderr sidecar, READ rather than merely kept ----------------------
# W7-42's one genuinely new finding came from here and had never appeared in any
# transcript: `Formatter.close()` invoking `close()` on a `StringBuilder`.
# HotSpot's `catch` on that path is `IOException`, so a `NoSuchMethodError`
# there PROPAGATES; this VM logs a WARN and continues with the right answer, so
# the stdout observable matches on both sides and every probe in this campaign
# is blind to it. A linkage error that becomes a log line is a fabricated
# success. Separating the streams and then never reading the sidecar is the
# same blindness one step further back, so these are printed ABOVE the diff.
$linkageNeedles = @(
    "NoSuchMethodError",
    "NoSuchFieldError",
    "AbstractMethodError",
    "IncompatibleClassChangeError",
    "NoClassDefFoundError",
    "ClassNotFoundException",
    "UnsatisfiedLinkError",
    "IllegalAccessError",
    "VerifyError"
)
# One alternation, one pass: a per-needle loop reports a line twice when it
# names two of them.
$linkagePattern = (($linkageNeedles | ForEach-Object { [regex]::Escape($_) }) -join '|')
$sidecar = @()
foreach ($side in @($hs, $cv)) {
    $hits = @(Select-String -Path $side.Err -Pattern $linkagePattern -Encoding UTF8)
    foreach ($h in $hits) {
        $sidecar += "$($side.Label) stderr:$($h.LineNumber)  $($h.Line.Trim())"
    }
}
if ($sidecar.Count -gt 0) {
    Write-Host ""
    Write-Host "=== STDERR SIDECAR: $($sidecar.Count) linkage/lookup lines -- INVISIBLE to any stdout diff ==="
    foreach ($s in $sidecar) { Write-Host "  ~ $s" }
    Write-Host "  A linkage error the VM logs and continues past is a fabricated success:"
    Write-Host "  HotSpot would propagate it, so the observable can match on both sides."
    Write-Host "  These are findings about the VM, not instrument errors, so they do not"
    Write-Host "  suppress the diff -- but a run with sidecar lines and a clean diff is"
    Write-Host "  NOT a clean run. See W7-42-differential-instrument-holes.md."
}

# ---- the diff, on stdout only --------------------------------------------
# A digest mismatch means the two sides were not built from the same probe. The
# earlier version of this script printed the diff anyway, under a warning --
# and a warning above a plausible-looking diff is a warning that gets scrolled
# past. Scrolling past exactly this condition is what produced hole 1, so the
# diff is now withheld.
if ($digestMismatch) {
    Write-Host ""
    Write-Host "=== NO DIFF PRINTED ==="
    Write-Host "  The two sides were not built from the same probe, so no comparison"
    Write-Host "  between them means anything. Re-run: ONE compile, both sides."
    Write-Host "  Transcripts and provenance are on disk in $OutDir if you need them."
    exit 2
}

$diff = @(Compare-Object -ReferenceObject $hsLines -DifferenceObject $cvLines -SyncWindow 200)
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
