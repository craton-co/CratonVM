# bench/cassandra/diff-baseline.ps1
# WP8.7 — compare the last cratonvm run against bench-baseline.json.
# Schema-v1 identical to bench/wildfly/. Windows PowerShell 5.1.

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$Here = Split-Path -Parent $MyInvocation.MyCommand.Path
$Baseline  = Join-Path $Here 'bench-baseline.json'
$StdoutLog = Join-Path $Here 'last-run.stdout.log'
$StderrLog = Join-Path $Here 'last-run.stderr.log'
$RcFile    = Join-Path $Here 'last-run.rc'

if (-not (Test-Path $Baseline))  { Write-Error "diff-baseline(cassandra): $Baseline missing"; exit 2 }
foreach ($f in @($StdoutLog, $StderrLog, $RcFile)) {
    if (-not (Test-Path $f)) { Write-Error "diff-baseline(cassandra): $f missing; run run-under-cratonvm.ps1 first"; exit 2 }
}

$ActualRc = (Get-Content $RcFile -Raw).Trim()
$json = Get-Content $Baseline -Raw | ConvertFrom-Json

$ExpectedRc       = "$($json.expected_final_rc)"
$StderrContains   = @(); if ($json.PSObject.Properties.Name -contains 'expected_stderr_contains')  { $StderrContains   = $json.expected_stderr_contains }
$StderrForbidden  = @(); if ($json.PSObject.Properties.Name -contains 'expected_stderr_forbidden') { $StderrForbidden  = $json.expected_stderr_forbidden }
$StdoutContains   = @(); if ($json.PSObject.Properties.Name -contains 'expected_stdout_contains')  { $StdoutContains   = $json.expected_stdout_contains }

$Stdout = Get-Content $StdoutLog -Raw -ErrorAction SilentlyContinue
$Stderr = Get-Content $StderrLog -Raw -ErrorAction SilentlyContinue
if ($null -eq $Stdout) { $Stdout = '' }
if ($null -eq $Stderr) { $Stderr = '' }

$Fails = 0
function Report-Fail([string]$msg) { Write-Error "diff-baseline(cassandra): FAIL $msg"; $script:Fails++ }

if ($ActualRc -ne $ExpectedRc) {
    Report-Fail "rc mismatch: got $ActualRc, baseline says $ExpectedRc"
}
foreach ($item in $StderrContains) {
    if ($item -and -not $Stderr.Contains($item)) { Report-Fail "stderr missing required signal '$item'" }
}
foreach ($item in $StderrForbidden) {
    if ($item -and $Stderr.Contains($item)) { Report-Fail "stderr unexpectedly contains '$item'" }
}
foreach ($item in $StdoutContains) {
    if ($item -and -not $Stdout.Contains($item)) { Report-Fail "stdout missing required signal '$item'" }
}

if ($Fails -eq 0) {
    Write-Output "diff-baseline(cassandra): baseline match (rc=$ActualRc)"
    exit 0
}
Write-Output "diff-baseline(cassandra): baseline DRIFT ($Fails check(s) failed)"
exit 1
