<#
  run-osr-differential.ps1 - the correctness verdict for lifting the OSR
  dead-local entry refusal (CRATONVM_JIT_OSR_DEAD_LOCALS).

  Runs OsrDeadLocalProbe under four configurations and prints the FNV
  accumulator from each. They must all be identical; the HotSpot row is the
  reference. ASCII-only (Windows PowerShell 5.1 reads a BOM-less file as ANSI).
#>
[CmdletBinding()]
param(
  [string]$Exe  = 'C:\craton\CratonVM-doc30-osr-20260731\target\release\cratonvm.exe',
  [int]$N       = 400000,
  [string]$Probes = 'C:\craton\CratonVM-doc30-osr-20260731\probes\out'
)
$ErrorActionPreference = 'Stop'
$jdk = 'C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot'

# NOTE: never wrap the child in PowerShell's `2>&1`. On 5.1 a native
# command's stderr comes back as ErrorRecords and the first line CratonVM
# writes there ("[cratonvm] main-vm run() returned Ok") aborts the pipeline.
# Redirect both streams to files via Start-Process instead.
$script:tmp = Join-Path $env:TEMP ('osrdiff-' + $PID)
New-Item -ItemType Directory -Force $script:tmp | Out-Null

function Run-Arm([string]$label, [string]$exePath, [string[]]$argv, [hashtable]$envs) {
  foreach ($k in $envs.Keys) { Set-Item -Path "Env:$k" -Value $envs[$k] }
  $safe = $label -replace '[^A-Za-z0-9]', '_'
  $o = Join-Path $script:tmp "$safe.out"
  $e = Join-Path $script:tmp "$safe.err"
  $p = Start-Process -FilePath $exePath -ArgumentList $argv -NoNewWindow -PassThru -Wait `
         -RedirectStandardOutput $o -RedirectStandardError $e
  foreach ($k in $envs.Keys) { Remove-Item -Path "Env:$k" -ErrorAction SilentlyContinue }
  $out = if (Test-Path $o) { Get-Content $o -Raw } else { '' }
  $acc = ($out -split "`n" | Where-Object { $_ -match 'acc=' }) -join ''
  if (-not $acc) {
    $errTail = if (Test-Path $e) { (Get-Content $e -Tail 4) -join ' | ' } else { '' }
    $acc = "NO-ACC rc=$($p.ExitCode) <<< $errTail"
  }
  Write-Host ("{0,-34} {1}" -f $label, $acc.Trim())
  return $acc.Trim()
}

$cp = $Probes
$results = @{}
$results['hotspot'] = Run-Arm 'HotSpot JDK 25' (Join-Path $jdk 'bin\java.exe') `
  @('-cp', $cp, 'OsrDeadLocalProbe', "$N") @{}
$results['craton-jit'] = Run-Arm 'CratonVM JIT (default)' $Exe `
  @('-cp', $cp, 'OsrDeadLocalProbe', "$N") @{}
$results['craton-strict'] = Run-Arm 'CratonVM JIT (DEAD_LOCALS=0)' $Exe `
  @('-cp', $cp, 'OsrDeadLocalProbe', "$N") @{ 'CRATONVM_JIT_OSR_DEAD_LOCALS' = '0' }
$results['craton-nojit'] = Run-Arm 'CratonVM --nojit' $Exe `
  @('--nojit', '-cp', $cp, 'OsrDeadLocalProbe', "$N") @{}
$results['craton-blanket'] = Run-Arm 'CratonVM blanket mask' $Exe `
  @('-cp', $cp, 'OsrDeadLocalProbe', "$N") @{ 'CRATONVM_JIT_OSR_DEAD_MASK_BLANKET' = '1' }

$distinct = $results.Values | Sort-Object -Unique
Write-Host ''
if ($distinct.Count -eq 1) {
  Write-Host '[differential] ALL ARMS IDENTICAL' -ForegroundColor Green
  exit 0
} else {
  Write-Host ('[differential] MISMATCH - ' + $distinct.Count + ' distinct results') -ForegroundColor Red
  $results.GetEnumerator() | ForEach-Object { Write-Host ("  {0,-18} {1}" -f $_.Key, $_.Value) }
  exit 1
}
