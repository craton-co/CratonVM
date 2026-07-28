<#
  Run TestHttp2Section_8_2 in bounded parameterized ranges, preserving the
  real JUnit fixture while giving every shard an explicit pass/fail record.
#>
[CmdletBinding()]
param(
  [Parameter(Mandatory = $true)] [string]$Exe,
  [Parameter(Mandatory = $true)] [string]$ProbeDir,
  [int]$FirstIndex = 0,
  # The current Tomcat data provider exposes indexes 0 through 6657.
  [int]$LastIndex = 6657,
  [int]$ShardSize = 200,
  [string]$LogDirectory = ''
)

$ErrorActionPreference = 'Stop'
if ($FirstIndex -lt 0 -or $LastIndex -lt $FirstIndex -or $ShardSize -le 0) {
  throw 'invalid index range or shard size'
}
if (-not $LogDirectory) { $LogDirectory = Split-Path -Parent $Exe }
New-Item -ItemType Directory -Force -Path $LogDirectory | Out-Null

$runner = Join-Path $PSScriptRoot 'run-one.ps1'
$class = 'org.apache.coyote.http2.TestHttp2Section_8_2'
$env:CRATONVM_LOCK_ORDER_CHECK = '1'

try {
  for ($start = $FirstIndex; $start -le $LastIndex; $start += $ShardSize) {
    $end = [Math]::Min($LastIndex, $start + $ShardSize - 1)
    $log = Join-Path $LogDirectory ("http2-section82-shard-{0:D4}-{1:D4}.log" -f $start, $end)
    & $runner -Exe $Exe -Main RunMethods -ExtraCp $ProbeDir -Args2 @($class, '--range', "$start", "$end") `
      -TimeoutSec 900 -LogFile $log
    if ($LASTEXITCODE -ne 0) { throw "range $start..$end runner exit code $LASTEXITCODE" }

    $summary = Select-String -Path $log -Pattern '^\[RunMethods\] ran=(\d+) failed=(\d+) ' |
      Select-Object -Last 1
    if (-not $summary -or $summary.Matches[0].Groups[1].Value -ne "$($end - $start + 1)" `
        -or $summary.Matches[0].Groups[2].Value -ne '0') {
      throw "range $start..$end did not produce the expected zero-failure RunMethods summary"
    }
    Write-Host "[section82-shards] PASS $start..$end"
  }
} finally {
  Remove-Item Env:CRATONVM_LOCK_ORDER_CHECK -ErrorAction SilentlyContinue
}
