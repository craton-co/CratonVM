<#
  repeat-diffs.ps1 - re-run the classes that differed between the two A/B arms,
  individually, several times per arm, with a timeout well clear of the 300s
  boundary the DoHead family straddles.

  A single crash-vs-pass pair is not attribution. This script exists because
  the previous revision of tomcat known-issue 30 records an entire
  investigation cycle lost to reading one such pair as a result, three separate
  times, each refuted by simply repeating the control.

  ASCII-only.
#>
[CmdletBinding()]
param(
  [string]$Exe   = 'C:\craton\CratonVM-doc30-osr-20260731\cratonvm-doc30-slotfix-v2.exe',
  [int]$Repeats  = 3,
  [int]$TimeoutSec = 900,
  [string]$OutDir = 'C:\craton\CratonVM-doc30-osr-20260731\.repeat'
)
$ErrorActionPreference = 'Stop'
$TC  = 'C:\craton\CratonVM\apps\tomcat'
$cp  = (Get-Content (Join-Path $TC '.suite\cp.txt') -Raw).Trim()
New-Item -ItemType Directory -Force $OutDir | Out-Null
$csv = Join-Path $OutDir 'repeat.csv'
if (-not (Test-Path $csv)) { 'class,arm,rep,rc,seconds,status' | Set-Content $csv -Encoding ascii }

$classes = @(
  'jakarta.servlet.http.TestHttpServletDoHeadInvalidWrite0ValidWrite1',
  'jakarta.servlet.http.TestHttpServletDoHeadInvalidWrite1023ValidWrite512',
  'jakarta.servlet.http.TestHttpServletDoHeadInvalidWrite1ValidWrite1',
  'org.apache.catalina.tribes.group.interceptors.TestNonBlockingCoordinator',
  'org.apache.catalina.tribes.group.interceptors.TestTcpFailureDetector',
  'org.apache.coyote.http11.TestHttp11InputBuffer',
  'org.apache.coyote.http2.TestAsyncFlush',
  'org.apache.jasper.compiler.TestCompiler'
)

$env:CRATONVM_REAL_NET_SOCKETS         = '1'
$env:CRATONVM_REAL_AQS                 = '1'
$env:CRATONVM_DISABLE_DEFAULT_WATCHDOG = '1'
$env:CRATONVM_ROOTSNAP_CACHE           = '1'

$jvm = @('-Xmx2g','-Dfile.encoding=UTF-8','-Djava.net.preferIPv4Stack=true',
  "-Dtomcat.test.basedir=$TC\output\build","-Dtomcat.test.temp=$TC\output\test-tmp",
  "-Dtomcat.test.tomcatbuild=$TC\output\build",'-Dtomcat.test.relaxTiming=true',
  '--add-opens','java.base/java.lang=ALL-UNNAMED','--add-opens','java.base/java.io=ALL-UNNAMED',
  '--add-opens','java.base/java.util=ALL-UNNAMED','--add-opens','java.base/java.util.concurrent=ALL-UNNAMED')

function RunOne([string]$cls, [string]$arm, [int]$rep) {
  if ($arm -eq 'B') {
    $env:CRATONVM_JIT_OSR_DEAD_LOCALS = '0'
    $env:CRATONVM_JIT_PUTFIELD_INIT   = '0'
  } else {
    Remove-Item Env:CRATONVM_JIT_OSR_DEAD_LOCALS -ErrorAction SilentlyContinue
    Remove-Item Env:CRATONVM_JIT_PUTFIELD_INIT   -ErrorAction SilentlyContinue
  }
  $safe = $cls -replace '[^A-Za-z0-9._]','_'
  $log  = Join-Path $OutDir "$safe.$arm.$rep.log"
  $sw = [Diagnostics.Stopwatch]::StartNew()
  $p = Start-Process -FilePath $Exe -ArgumentList ($jvm + @('-cp',$cp,'org.junit.runner.JUnitCore',$cls)) `
        -PassThru -NoNewWindow -WorkingDirectory $TC -RedirectStandardOutput $log -RedirectStandardError "$log.err"
  if (-not $p.WaitForExit($TimeoutSec * 1000)) {
    try { & taskkill /F /T /PID $p.Id 2>$null | Out-Null } catch {}
    try { $p.Kill() } catch {}
    try { $p.WaitForExit(5000) | Out-Null } catch {}
    $sw.Stop(); return @{ status='HANG'; rc='TIMEOUT'; secs=[math]::Round($sw.Elapsed.TotalSeconds,1) }
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

foreach ($cls in $classes) {
  foreach ($rep in 1..$Repeats) {
    foreach ($arm in 'A','B') {
      $r = RunOne $cls $arm $rep
      "`"$cls`",$arm,$rep,$($r.rc),$($r.secs),$($r.status)" | Add-Content $csv -Encoding ascii
      Write-Host ("{0} rep{1} {2,-9} {3,7}s  {4}" -f $arm, $rep, $r.status, $r.secs, ($cls -replace '^.*\.',''))
    }
  }
}
Remove-Item Env:CRATONVM_JIT_OSR_DEAD_LOCALS,Env:CRATONVM_JIT_PUTFIELD_INIT -ErrorAction SilentlyContinue

Write-Host ''
Write-Host 'class                                              A statuses          B statuses'
foreach ($cls in $classes) {
  $rows = Import-Csv $csv | Where-Object class -eq $cls
  $a = (($rows | Where-Object arm -eq 'A').status) -join '/'
  $b = (($rows | Where-Object arm -eq 'B').status) -join '/'
  Write-Host ("{0,-50} {1,-19} {2}" -f ($cls -replace '^.*\.',''), $a, $b)
}
