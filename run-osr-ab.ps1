<#
  run-osr-ab.ps1 - interleaved A/B of the OSR dead-local lift on one binary.

  Arm A = defaults (entry admitted, trampoline skips the masked locals)
  Arm B = CRATONVM_JIT_OSR_DEAD_LOCALS=0 (the historical blanket refusal)

  One binary, one knob, rounds interleaved so host drift hits both arms
  equally. ASCII-only.
#>
[CmdletBinding()]
param(
  [string]$Exe    = 'C:\craton\CratonVM-doc30-osr-20260731\cratonvm-doc30-slotfix-v2.exe',
  [string]$Main   = 'OsrMessageBytesProbe',
  [string]$Arg    = '500000',
  [int]$Rounds    = 3,
  [string]$Probes = 'C:\craton\CratonVM-doc30-osr-20260731\probes\out'
)
$ErrorActionPreference = 'Stop'
$TC  = 'C:\craton\CratonVM\apps\tomcat'
$cp  = "$Probes;" + (Get-Content (Join-Path $TC '.suite\cp.txt') -Raw).Trim()
$tmp = Join-Path $env:TEMP ('osrab-' + $PID); New-Item -ItemType Directory -Force $tmp | Out-Null

$env:CRATONVM_REAL_NET_SOCKETS         = '1'
$env:CRATONVM_REAL_AQS                 = '1'
$env:CRATONVM_DISABLE_DEFAULT_WATCHDOG = '1'
$env:CRATONVM_ROOTSNAP_CACHE           = '1'

function Once([string]$arm, [int]$r) {
  if ($arm -eq 'B') { $env:CRATONVM_JIT_OSR_DEAD_LOCALS = '0' }
  else { Remove-Item Env:CRATONVM_JIT_OSR_DEAD_LOCALS -ErrorAction SilentlyContinue }
  $o = Join-Path $tmp "$arm-$r.out"
  $sw = [Diagnostics.Stopwatch]::StartNew()
  $p = Start-Process -FilePath $Exe -ArgumentList (@('-Xmx2g','-cp',$cp,$Main,$Arg)) `
        -NoNewWindow -PassThru -Wait -WorkingDirectory $TC `
        -RedirectStandardOutput $o -RedirectStandardError "$o.err"
  $sw.Stop()
  $line = (Get-Content $o -ErrorAction SilentlyContinue | Where-Object { $_ -match '=' }) -join ' '
  [pscustomobject]@{ arm=$arm; round=$r; wall=[math]::Round($sw.Elapsed.TotalSeconds,2); rc=$p.ExitCode; out=$line }
}

$rows = @()
for ($r = 1; $r -le $Rounds; $r++) {
  $rows += Once 'A' $r
  $rows += Once 'B' $r
  Write-Host ("round {0} done" -f $r)
}
Remove-Item Env:CRATONVM_JIT_OSR_DEAD_LOCALS -ErrorAction SilentlyContinue
$rows | Format-Table -AutoSize
foreach ($a in 'A','B') {
  $w = ($rows | Where-Object arm -eq $a | Measure-Object wall -Average).Average
  Write-Host ("arm {0} mean wall = {1:N2}s" -f $a, $w)
}
