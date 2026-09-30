# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
#
# Interleaved A/B for the native-call funnel work
# (native-call-funnel-per-call-floor-RETIRED-20260804.md).
#
# Runs `apps/probes/NativeShapeProbe` alternately on two binaries in A-B-B-A order
# and reports the MINIMUM last-pass ns/op per rung for each arm.
#
# Why this shape:
#   * Interleaved, both orders. A block layout (all of A, then all of B) lets
#     a drifting box cluster into one arm; A-B-B-A does not remove drift but
#     it stops it aliasing onto the arm boundary.
#   * Minima, not means. This is a shared build host — a concurrent `cargo
#     build` moved every rung on this probe by 2-3x during development — and a
#     minimum is the closest thing to an uncontended sample. Means here price
#     the box.
#   * The LAST pass of each rung only. The probe is multi-pass by design; a
#     rung that has not gone flat is warm-up, not a measurement.
#
# Usage:
#   powershell -File scripts/ab-native-funnel.ps1 -Base <base.exe> -Fix <fix.exe> [-Rounds 2]

param(
    [Parameter(Mandatory = $true)][string]$Base,
    [Parameter(Mandatory = $true)][string]$Fix,
    [int]$Rounds = 2,
    [string]$JdkHome = 'C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot',
    [string]$ClassPath = 'out'
)

# `& 'name.exe'` searches PATH, not the working directory, so a bare file name
# fails with CommandNotFoundException. Resolve both arms to absolute paths up
# front rather than making the caller remember `.\`.
$Base = (Resolve-Path $Base).Path
$Fix = (Resolve-Path $Fix).Path

function Invoke-Probe {
    param([string]$Exe)
    $raw = & $Exe --java-home $JdkHome -cp $ClassPath NativeShapeProbe 2>&1 | Out-String
    $result = @{}
    foreach ($line in $raw -split "`r?`n") {
        # "<label> <p1> <p2> <p3> <p4>". The label is PADDED to 46 but not
        # truncated to it — two rungs are longer — so match it non-greedily
        # rather than with a width, or those rungs silently vanish from the
        # comparison table.
        if ($line -match '^(.*?\S)\s+([\d.,]+)\s+([\d.,]+)\s+([\d.,]+)\s+([\d.,]+)\s*$') {
            $label = $Matches[1].Trim()
            if ($label -eq 'rung') { continue }
            $last = [double](($Matches[5]) -replace ',', '.')
            $result[$label] = $last
        }
    }
    return $result
}

$arms = @{ 'base' = @(); 'fix' = @() }
for ($r = 0; $r -lt $Rounds; $r++) {
    # A-B-B-A: the second half reverses the order so a monotone drift cannot
    # be read as an effect.
    Write-Host "round $($r + 1)/$Rounds : base"; $arms['base'] += , (Invoke-Probe $Base)
    Write-Host "round $($r + 1)/$Rounds : fix ";  $arms['fix']  += , (Invoke-Probe $Fix)
    Write-Host "round $($r + 1)/$Rounds : fix ";  $arms['fix']  += , (Invoke-Probe $Fix)
    Write-Host "round $($r + 1)/$Rounds : base"; $arms['base'] += , (Invoke-Probe $Base)
}

$labels = @()
foreach ($run in $arms['base']) { foreach ($k in $run.Keys) { if ($labels -notcontains $k) { $labels += $k } } }

Write-Host ''
Write-Host ('{0,-46}{1,12}{2,12}{3,10}' -f 'rung (min of last passes, ns/op)', 'base', 'fix', 'ratio')
foreach ($label in $labels) {
    $b = ($arms['base'] | ForEach-Object { $_[$label] } | Measure-Object -Minimum).Minimum
    $f = ($arms['fix']  | ForEach-Object { $_[$label] } | Measure-Object -Minimum).Minimum
    if ($null -eq $b -or $null -eq $f -or $f -eq 0) { continue }
    Write-Host ('{0,-46}{1,12:N1}{2,12:N1}{3,10:N2}x' -f $label, $b, $f, ($b / $f))
}
