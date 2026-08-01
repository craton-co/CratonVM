<#
  run-tomcat-ab.ps1 - interleaved A/B of a Tomcat class list on ONE binary.

  Arm A = defaults: the OSR dead-local entry admitted, and the putfield-ctor
          JIT ban lifted.
  Arm B = CRATONVM_JIT_OSR_DEAD_LOCALS=0 + CRATONVM_JIT_PUTFIELD_INIT=0, i.e.
          both relaxations restored to their historical behaviour.

  The two arms run back to back per class so host drift hits both equally, and
  the same binary serves both, so nothing but the two knobs differs. Any class
  whose status differs between arms is the only thing worth looking at.

  ASCII-only (Windows PowerShell 5.1 reads a BOM-less file as ANSI).
#>
[CmdletBinding()]
param(
  [string]$Exe      = 'C:\craton\CratonVM-doc30-osr-20260731\cratonvm-doc30-slotfix-v2.exe',
  [string]$Pattern  = 'util\.(buf|collections|http)|catalina\.util',
  [int]$TimeoutSec  = 300,
  [string]$OutDir   = 'C:\craton\CratonVM-doc30-osr-20260731\.ab',
  [string]$MaxHeap  = '2g'
)
$ErrorActionPreference = 'Stop'
$TC   = 'C:\craton\CratonVM\apps\tomcat'
$cp   = (Get-Content (Join-Path $TC '.suite\cp.txt') -Raw).Trim()
$list = Get-Content (Join-Path $TC '.suite\all-tests.txt') | Where-Object { $_ -match $Pattern }
New-Item -ItemType Directory -Force $OutDir | Out-Null
$csv = Join-Path $OutDir 'ab-results.csv'
if (-not (Test-Path $csv)) { 'class,arm,rc,seconds,status' | Set-Content $csv -Encoding ascii }
$done = @{}
Import-Csv $csv | ForEach-Object { $done["$($_.class)|$($_.arm)"] = $_.status }

$env:CRATONVM_REAL_NET_SOCKETS         = '1'
$env:CRATONVM_REAL_AQS                 = '1'
$env:CRATONVM_DISABLE_DEFAULT_WATCHDOG = '1'
$env:CRATONVM_ROOTSNAP_CACHE           = '1'

$jvm = @("-Xmx$MaxHeap", '-Dfile.encoding=UTF-8', '-Djava.net.preferIPv4Stack=true',
  "-Dtomcat.test.basedir=$TC\output\build", "-Dtomcat.test.temp=$TC\output\test-tmp",
  "-Dtomcat.test.tomcatbuild=$TC\output\build", '-Dtomcat.test.relaxTiming=true',
  '--add-opens','java.base/java.lang=ALL-UNNAMED','--add-opens','java.base/java.io=ALL-UNNAMED',
  '--add-opens','java.base/java.util=ALL-UNNAMED','--add-opens','java.base/java.util.concurrent=ALL-UNNAMED')

function RunOne([string]$cls, [string]$arm) {
  if ($arm -eq 'B') {
    $env:CRATONVM_JIT_OSR_DEAD_LOCALS = '0'
    $env:CRATONVM_JIT_PUTFIELD_INIT   = '0'
  } else {
    Remove-Item Env:CRATONVM_JIT_OSR_DEAD_LOCALS -ErrorAction SilentlyContinue
    Remove-Item Env:CRATONVM_JIT_PUTFIELD_INIT   -ErrorAction SilentlyContinue
  }
  $safe = $cls -replace '[^A-Za-z0-9._]','_'
  $log  = Join-Path $OutDir "$safe.$arm.log"
  $sw   = [Diagnostics.Stopwatch]::StartNew()
  $p = Start-Process -FilePath $Exe -ArgumentList ($jvm + @('-cp',$cp,'org.junit.runner.JUnitCore',$cls)) `
        -PassThru -NoNewWindow -WorkingDirectory $TC -RedirectStandardOutput $log -RedirectStandardError "$log.err"
  if (-not $p.WaitForExit($TimeoutSec * 1000)) {
    try { & taskkill /F /T /PID $p.Id 2>$null | Out-Null } catch {}
    try { $p.Kill() } catch {}
    try { $p.WaitForExit(5000) | Out-Null } catch {}
    $sw.Stop()
    return @{ status='HANG'; rc='TIMEOUT'; secs=[math]::Round($sw.Elapsed.TotalSeconds,1) }
  }
  $sw.Stop()
  $tail = ''
  if (Test-Path $log)       { $tail += (Get-Content $log -Raw -ErrorAction SilentlyContinue) }
  if (Test-Path "$log.err") { $tail += "`n" + (Get-Content "$log.err" -Raw -ErrorAction SilentlyContinue) }
  if ($null -eq $tail) { $tail = '' }
  $crash = ($tail -match 'panicked|EXCEPTION_ACCESS_VIOLATION|SIGSEGV|0xC0000005|VM PANIC|stack backtrace|fatal runtime|not yet implemented|internal error: entered unreachable')
  $ok    = ($tail -match 'OK \(\d+ test')
  $fail  = ($tail -match 'FAILURES!!!|Tests run: \d+,\s+Failures')
  $status = if ($ok) { if ($crash) { 'CRASH' } else { 'PASS' } }
            elseif ($fail) { 'FAIL' }
            else { if ($crash) { 'CRASH' } else { 'NOSUMMARY' } }
  return @{ status=$status; rc=$p.ExitCode; secs=[math]::Round($sw.Elapsed.TotalSeconds,1) }
}

$i = 0
foreach ($cls in $list) {
  $i++
  foreach ($arm in 'A','B') {
    if ($done.ContainsKey("$cls|$arm")) { continue }
    $r = RunOne $cls $arm
    "`"$cls`",$arm,$($r.rc),$($r.secs),$($r.status)" | Add-Content $csv -Encoding ascii
    Write-Host ("[{0,3}/{1}] {2} {3,-9} {4,7}s  {5}" -f $i, $list.Count, $arm, $r.status, $r.secs, $cls)
  }
}
Remove-Item Env:CRATONVM_JIT_OSR_DEAD_LOCALS -ErrorAction SilentlyContinue
Remove-Item Env:CRATONVM_JIT_PUTFIELD_INIT   -ErrorAction SilentlyContinue

Write-Host ''
$rows = Import-Csv $csv
$byCls = $rows | Group-Object class
$diff = @()
foreach ($g in $byCls) {
  $a = ($g.Group | Where-Object arm -eq 'A').status
  $b = ($g.Group | Where-Object arm -eq 'B').status
  if ($a -and $b -and ($a -ne $b)) { $diff += [pscustomobject]@{ class=$g.Name; A=$a; B=$b } }
}
Write-Host ('classes={0}  A: ' -f $byCls.Count) -NoNewline
($rows | Where-Object arm -eq 'A' | Group-Object status | ForEach-Object { "$($_.Name)=$($_.Count)" }) -join ' ' | Write-Host
Write-Host '                B: ' -NoNewline
($rows | Where-Object arm -eq 'B' | Group-Object status | ForEach-Object { "$($_.Name)=$($_.Count)" }) -join ' ' | Write-Host
if ($diff.Count -eq 0) { Write-Host '[ab] NO STATUS DIFFERENCES' -ForegroundColor Green }
else { Write-Host ('[ab] {0} class(es) differ:' -f $diff.Count) -ForegroundColor Yellow; $diff | Format-Table -AutoSize }
