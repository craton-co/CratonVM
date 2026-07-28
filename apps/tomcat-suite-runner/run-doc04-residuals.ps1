<#
  run-doc04-residuals.ps1 - run the exact class list that known-issue
  tomcat/04 (embedded-server throughput wall) still carries as evidence, on one
  VM, and write a CSV of status + wall time.

  ASCII-only (Windows PowerShell 5.1 reads a BOM-less UTF-8 script as ANSI).

  Example:
    run-doc04-residuals.ps1 -Exe C:\...\cratonvm-x.exe -Tag fix
    run-doc04-residuals.ps1 -Vm hotspot -Tag hotspot
#>
[CmdletBinding()]
param(
  [ValidateSet('craton','hotspot')] [string]$Vm = 'craton',
  [string]$Exe        = '',
  [string]$Tag        = 'run',
  [string]$OutDir     = '',
  [int]$TimeoutSec    = 1500,
  [string[]]$Only     = @()
)

$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$runOne = Join-Path $here 'run-one.ps1'
if (-not $OutDir) { $OutDir = Join-Path $here "output\doc04-$Tag" }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

$classes = @(
  'org.apache.catalina.startup.TestHostConfigAutomaticDeploymentAddition',
  'org.apache.catalina.startup.TestHostConfigAutomaticDeploymentModification',
  'org.apache.catalina.startup.TestHostConfigAutomaticDeploymentDeleteC',
  'org.apache.coyote.http2.TestHttp2Section_8_2',
  'org.apache.catalina.mapper.TestMapperPerformance',
  'org.apache.el.parser.TestELParserPerformance',
  'org.apache.tomcat.websocket.server.TestAsyncMessagesPerformance',
  'org.apache.juli.TestOneLineFormatterPerformance'
)
if ($Only.Count -gt 0) { $classes = $Only }

$csv = Join-Path $OutDir 'results.csv'
'class,status,seconds' | Set-Content -Encoding utf8 $csv

foreach ($c in $classes) {
  $log = Join-Path $OutDir "$c.log"
  $sw = [Diagnostics.Stopwatch]::StartNew()
  if ($Vm -eq 'hotspot') {
    & $runOne -Vm hotspot -Class $c -TimeoutSec $TimeoutSec -LogFile $log | Out-Null
  } else {
    & $runOne -Exe $Exe -Class $c -TimeoutSec $TimeoutSec -LogFile $log | Out-Null
  }
  $rc = $LASTEXITCODE
  $sw.Stop()
  $text = if (Test-Path $log) { Get-Content $log -Raw } else { '' }
  $status =
    if ($rc -eq 124)                      { 'HANG' }
    elseif ($text -match '(?m)^OK \(')    { 'PASS' }
    elseif ($text -match '(?m)^FAILURES') { 'FAIL' }
    elseif ($text -match '(?m)^Tests run') { 'FAIL' }
    else                                  { "NOSUMMARY(rc=$rc)" }
  $secs = [math]::Round($sw.Elapsed.TotalSeconds, 1)
  Write-Host ("[doc04] {0,-70} {1,-14} {2}s" -f $c, $status, $secs)
  "$c,$status,$secs" | Add-Content -Encoding utf8 $csv
}
Write-Host "[doc04] results -> $csv"
