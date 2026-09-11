# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
#
# Paired USER-CPU A/B of one JIT flag, or of the two tiers, on Windows.
#
# # Why this file exists
#
# `cpu-ab.sh` is this directory's answer to a busy host, and
# `c2-one-carry-slot-is-the-frame-traffic-ceiling-FIXED-20260910.md` §8 is the
# worked example: the same 1.009x effect that `flag-ab.sh` reported as
# UNMEASURABLE twice, with the sign flipping, reproduced at a 0.1% floor here.
#
# It cannot run on Windows. It measures user CPU through `/usr/bin/time -f
# '%U'`, which is GNU coreutils and is absent from Git Bash -- `which time`
# finds nothing, so every sample is a `RUNFAIL` and the script reports nothing
# rather than failing loudly. That matters because the tiering inversion this
# directory exists to measure is a Win64 question as often as a Linux one:
# `CRATONVM_JIT_IR_GP_WIDE` is a silent no-op on System V (the narrow and wide
# files are both five registers there), so the register-file experiments can
# ONLY be asked here.
#
# .NET gives the same number without coreutils: `Process.UserProcessorTime` is
# the process's user-mode CPU, read after exit from the `Process` object that
# started it.
#
# # The method, unchanged from the shell scripts
#
#   * arms INTERLEAVED run-by-run (ABBA on even rounds, BAAB on odd), never
#     blocked, so host drift hits both arms and ordering bias cancels;
#   * a CONTROL arm -- A run a second time with an identical configuration --
#     every round. The A-vs-C spread is the noise floor;
#   * an effect inside the floor is UNMEASURABLE, not a small result;
#   * checksums compared across every run; a mismatch voids the result.
#
# And the warning `cpu-ab.sh` carries applies here word for word: user CPU
# cannot see anything that shows up as a stall rather than as instructions
# retired, so a wall-clock number on a QUIET host is still the better
# instrument when one is available. Give it seconds of work per sample.
#
# # Usage
#
#   # Does one flag pay, inside the optimizing tier?
#   pwsh tools/tier-ab/cpu-ab.ps1 -Exe .\cratonvm.exe -Cp .\pc -Class FieldLoop `
#       -Flag CRATONVM_JIT_IR_GP_WIDE -Rounds 8 -D probe.reps=20000
#
#   # Which tier is faster on this loop?
#   pwsh tools/tier-ab/cpu-ab.ps1 -Exe .\cratonvm.exe -Cp .\pc -Class FieldLoop `
#       -Tier -Rounds 8 -D probe.reps=20000
#
# `-D` is an ARRAY parameter, so several properties go in one comma-separated
# argument -- `-D probe.reps=20000,probe.wide=true`. Repeating the switch is a
# PowerShell binding error, not a second value.
#
# The probe must print a line containing `acc=<checksum>`; `probes/FieldLoop.java`
# is the reference shape.

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string] $Exe,
    [Parameter(Mandatory = $true)][string] $Cp,
    [Parameter(Mandatory = $true)][string] $Class,
    # Flag A/B: A is `<Flag>=0`, B is `<Flag>=1`, both under `-Base`.
    [string] $Flag,
    # Tier A/B: A is the single-pass tier, B the optimizing one. Mutually
    # exclusive with -Flag.
    [switch] $Tier,
    # Extra environment applied to every arm, as `NAME=VALUE` strings. Defaults
    # to forcing the optimizing tier, which is what a flag A/B inside that tier
    # wants; pass an empty array to measure the default tiering.
    [string[]] $Base = @('CRATONVM_JIT_FORCE_C2=1'),
    [int] $Rounds = 8,
    # `-D probe.reps=20000` style system properties, passed through to the probe.
    [string[]] $D = @(),
    [string] $Xmx = '8g'
)

$ErrorActionPreference = 'Stop'
# A decimal COMMA in a results table is a trap for whoever pastes it into a
# spreadsheet or a markdown file, and this project's hosts are not all
# en-US. Format in the invariant culture regardless of the operator's locale.
[System.Threading.Thread]::CurrentThread.CurrentCulture =
    [System.Globalization.CultureInfo]::InvariantCulture

if ($Tier -and $Flag) { throw 'pass -Flag or -Tier, not both' }
if (-not $Tier -and -not $Flag) { throw 'pass -Flag <NAME> or -Tier' }
if (-not (Test-Path -LiteralPath $Exe)) { throw "no such exe: $Exe" }

$exePath = (Resolve-Path -LiteralPath $Exe).Path
$cpPath = (Resolve-Path -LiteralPath $Cp).Path
# Split on commas as well as on array elements. `powershell -File` hands a
# comma-separated argument through as ONE string rather than binding it as an
# array, so a caller who follows the usage above would otherwise pass a single
# malformed `-Dprobe.reps=8000,probe.n=20000`.
$jprops = @($D | ForEach-Object { $_ -split ',' } | Where-Object { $_ } | ForEach-Object { "-D$_" })

# The environment each arm adds on top of `-Base`. In tier mode the two arms
# pin different tiers and `-Base` is dropped, because forcing C2 in both arms
# would make them the same arm -- the failure `tier-ab.sh` guards against with
# its `compiles: c1=N c2=M` witness.
function Get-ArmEnv([string] $arm) {
    if ($Tier) {
        switch ($arm) {
            'B' { return @('CRATONVM_JIT_FORCE_C2=1') }
            default { return @('CRATONVM_C2_SUPERSEDE=0') }
        }
    }
    $v = if ($arm -eq 'B') { '1' } else { '0' }
    return @($Base + @("$Flag=$v"))
}

# One timed run. Returns the process's USER CPU in seconds and the probe's own
# checksum, or $null when the run produced no `acc=`.
function Invoke-Sample([string] $arm) {
    $env_pairs = Get-ArmEnv $arm
    $psi = [System.Diagnostics.ProcessStartInfo]::new()
    $psi.FileName = $exePath
    # `ArgumentList` is .NET Core only; Windows PowerShell 5.1 ships the desktop
    # framework, where `ProcessStartInfo` has only the `Arguments` STRING. Quote
    # each argument so a classpath with a space survives.
    $argv = @("-Xmx$Xmx", '-cp', $cpPath) + $jprops + @($Class)
    $psi.Arguments = ($argv | ForEach-Object { '"' + $_ + '"' }) -join ' '
    foreach ($pair in $env_pairs) {
        $i = $pair.IndexOf('=')
        $psi.Environment[$pair.Substring(0, $i)] = $pair.Substring($i + 1)
    }
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.UseShellExecute = $false

    $p = [System.Diagnostics.Process]::Start($psi)
    # Read stdout BEFORE waiting: a probe that fills the pipe buffer while the
    # parent blocks in WaitForExit deadlocks, and this one prints per round.
    $stdout = $p.StandardOutput.ReadToEnd()
    [void] $p.StandardError.ReadToEnd()
    $p.WaitForExit()
    # Valid after exit on a Process this call started -- the handle is still
    # open, which is exactly why the run has to go through Process.Start and
    # not Start-Process -Wait.
    $cpu = $p.UserProcessorTime.TotalSeconds
    $acc = [regex]::Match($stdout, 'acc=(-?\d+)')
    $p.Dispose()
    if (-not $acc.Success) { return $null }
    return [pscustomobject]@{ Arm = $arm; Cpu = $cpu; Acc = $acc.Groups[1].Value }
}

function Get-Median([double[]] $xs) {
    $s = @($xs | Sort-Object)
    $n = $s.Count
    if ($n -eq 0) { return [double]::NaN }
    if ($n % 2 -eq 1) { return $s[[int](($n - 1) / 2)] }
    return ($s[$n / 2 - 1] + $s[$n / 2]) / 2.0
}

$what = if ($Tier) { 'A=single-pass  B=optimizing' } else { "A=$Flag=0  B=$Flag=1  base=$($Base -join ',')" }
Write-Host "# exe=$exePath class=$Class rounds=$Rounds props=$($jprops -join ' ')"
Write-Host "# $what   C=control(=A)   metric=USER CPU seconds"

$samples = [System.Collections.Generic.List[object]]::new()
$fails = 0
for ($r = 1; $r -le $Rounds; $r++) {
    $order = if ($r % 2 -eq 0) { @('A', 'B', 'C', 'B') } else { @('B', 'A', 'B', 'C') }
    foreach ($arm in $order) {
        $s = Invoke-Sample $arm
        if ($null -eq $s) { $fails++; Write-Host "  RUNFAIL $arm" ; continue }
        $samples.Add($s)
    }
    Write-Host "  round $r done"
}

$accs = @($samples | ForEach-Object { $_.Acc } | Sort-Object -Unique)
if ($accs.Count -ne 1) {
    Write-Host ''
    Write-Host '*** CHECKSUM MISMATCH *** the arms computed different answers:'
    Write-Host ($accs -join ' ')
    Write-Host 'VERDICT: VOID -- a faster wrong answer is not a result.'
    exit 1
}

$a = @($samples | Where-Object { $_.Arm -eq 'A' } | ForEach-Object { $_.Cpu })
$c = @($samples | Where-Object { $_.Arm -eq 'C' } | ForEach-Object { $_.Cpu })
$b = @($samples | Where-Object { $_.Arm -eq 'B' } | ForEach-Object { $_.Cpu })

Write-Host ''
Write-Host '--- samples (user CPU seconds) ---'
foreach ($s in $samples) { '{0}  {1:F3}' -f $s.Arm, $s.Cpu | Write-Host }

$ma = Get-Median $a; $mc = Get-Median $c; $mb = Get-Median $b
Write-Host ''
Write-Host '--- verdict ---'
'A              median {0,8:F3} s   n={1}' -f $ma, $a.Count | Write-Host
'C (control=A)  median {0,8:F3} s   n={1}' -f $mc, $c.Count | Write-Host
'B              median {0,8:F3} s   n={1}' -f $mb, $b.Count | Write-Host
if ($fails -gt 0) { Write-Host "($fails run(s) produced no acc= and were dropped)" }

if ($a.Count -eq 0 -or $c.Count -eq 0 -or $b.Count -eq 0) {
    Write-Host 'VERDICT: NO CONTROL -- an arm produced no samples.'
    exit 1
}

# NOT `$base`: PowerShell variable names are case-insensitive, so that would
# silently rebind the `-Base` PARAMETER (a string array) and the first division
# below fails with `String[] does not contain a method named 'op_Division'`.
$mid = ($ma + $mc) / 2.0
$floor = [math]::Abs($ma - $mc) / $mid * 100.0
$effect = ($mb - $mid) / $mid * 100.0
'noise floor (A vs C)      : {0,6:F1}%' -f $floor | Write-Host
'effect  (B vs mean(A,C))  : {0,6:F1}%   ratio {1:F3}x' -f $effect, ($mb / $mid) | Write-Host

if ([math]::Abs($effect) -le $floor) {
    Write-Host 'VERDICT: UNMEASURABLE -- the effect is inside the noise floor.'
}
elseif ($effect -lt 0) {
    'VERDICT: B IS FASTER -- {0:F1}% below the floor of {1:F1}%' -f [math]::Abs($effect), $floor | Write-Host
}
else {
    'VERDICT: B IS SLOWER -- {0:F1}% above the floor of {1:F1}%' -f $effect, $floor | Write-Host
}

# A within-invocation floor bounds the drift INSIDE this invocation and says
# nothing about the drift between invocations:
# `c2-the-gp-register-file-is-not-the-binding-constraint-20260910.md` §5.2
# records three runs whose own control arms agreed to 0.1-1.0% disagreeing with
# each other by 4.5 points. Anything claiming a few percent wants several
# invocations, not one clean one.
Write-Host ''
Write-Host '(a few-percent effect needs several INVOCATIONS, not one tight floor -- see 5.2 of'
Write-Host ' docs/internal/performance/c2-the-gp-register-file-is-not-the-binding-constraint-20260910.md)'
