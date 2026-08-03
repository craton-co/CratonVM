<#
  run-one.ps1 - run ONE Tomcat test class (or a probe main class) under CratonVM
  or HotSpot with exactly the suite's environment, and print the wall time.

  Exists because reproducing a doc-04 throughput measurement previously meant
  hand-assembling the classpath + the four CRATONVM_* suite variables every
  time, and getting one of them wrong silently changes the number.

  "Exactly the suite's environment" means the JVM ARGUMENTS too, not just the
  env vars: run-tomcat-suite.ps1's Invoke-Mode passes four --add-opens flags
  and the tomcat.test.* system properties, and a repro that omits them is a
  different experiment. Omitting --add-opens=java.base/java.lang made every
  EasyMock-based class (TestSSLValve, TestJNDIRealm, TestLoadBalancerDraining-
  Valve, ...) fail here with "must be defined in the same package as
  org.easymock.internal.ClassProxyFactory" while passing under the suite
  runner - a pure launch-environment artifact that reads like a real defect.

  ASCII-only on purpose: Windows PowerShell 5.1 reads a BOM-less UTF-8 script
  as ANSI, and a stray non-ASCII byte inside a comment breaks the parse.

  Examples:
    run-one.ps1 -Class org.apache.catalina.startup.TestTomcat -Exe C:\...\cratonvm-x.exe
    run-one.ps1 -Vm hotspot -Class org.apache.catalina.startup.TestTomcat
    run-one.ps1 -Main TryCatchC2Probe -ExtraCp C:\probe -Exe C:\...\cratonvm-x.exe
#>
[CmdletBinding()]
param(
  [string]$Class    = '',            # JUnit test class (run via JUnitCore)
  [string]$Main     = '',            # plain main() class (mutually exclusive with -Class)
  [string[]]$Args2  = @(),           # arguments passed to -Main
  [ValidateSet('craton','hotspot')] [string]$Vm = 'craton',
  [string]$Exe      = '',            # CratonVM exe (required for -Vm craton)
  [string]$ExtraCp  = '',            # extra classpath entries (';'-separated)
  [string]$MaxHeap  = '2g',
  [int]$TimeoutSec  = 1500,
  [string]$LogFile  = '',            # capture stdout+stderr here instead of the console
  [switch]$NoJit
)

$ErrorActionPreference = 'Stop'
$TC      = 'C:\craton\CratonVM\apps\tomcat'
$JdkHome = 'C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot'
$CpFile  = Join-Path $TC '.suite\cp.txt'
if (-not (Test-Path $CpFile)) { throw "classpath file missing: $CpFile (run run-tomcat-suite.ps1 -Setup)" }
$cp = (Get-Content $CpFile -Raw).Trim()
if ($ExtraCp) { $cp = "$ExtraCp;$cp" }

if ($Class -and $Main) { throw 'pass -Class or -Main, not both' }
if (-not $Class -and -not $Main) { throw 'pass -Class or -Main' }

# The four variables the suite runner exports. An isolated repro that omits any
# of them is not measuring the same thing the suite measured.
$env:CRATONVM_REAL_NET_SOCKETS         = '1'
$env:CRATONVM_REAL_AQS                 = '1'
$env:CRATONVM_DISABLE_DEFAULT_WATCHDOG = '1'
$env:CRATONVM_ROOTSNAP_CACHE           = '1'
if ($NoJit) { $env:CRATONVM_DISABLE_JIT = '1' } else { Remove-Item Env:CRATONVM_DISABLE_JIT -ErrorAction SilentlyContinue }

if ($Vm -eq 'hotspot') {
  $exePath = Join-Path $JdkHome 'bin\java.exe'
} else {
  if (-not $Exe) { throw '-Exe is required for -Vm craton' }
  $exePath = $Exe
}
# Keep this list byte-for-byte in step with $jvmArgs in run-tomcat-suite.ps1's
# Invoke-Mode - it is the whole point of this script.
$argv = @(
  "-Xmx$MaxHeap", '-Dfile.encoding=UTF-8', '-Djava.net.preferIPv4Stack=true',
  "-Dtomcat.test.basedir=$TC\output\build",
  "-Dtomcat.test.temp=$TC\output\test-tmp",
  "-Dtomcat.test.tomcatbuild=$TC\output\build",
  '-Dtomcat.test.relaxTiming=true',
  '--add-opens','java.base/java.lang=ALL-UNNAMED',
  '--add-opens','java.base/java.io=ALL-UNNAMED',
  '--add-opens','java.base/java.util=ALL-UNNAMED',
  '--add-opens','java.base/java.util.concurrent=ALL-UNNAMED'
)
if ($Vm -eq 'craton') {
  if ($NoJit) { $argv += '--nojit' }
} elseif ($NoJit) {
  $argv += '-Xint'
}
$argv += @('-cp', $cp)
if ($Class) { $argv += @('org.junit.runner.JUnitCore', $Class) }
else        { $argv += @($Main) + $Args2 }

Push-Location $TC
try {
  $sw = [Diagnostics.Stopwatch]::StartNew()
  if ($LogFile) {
    $p = Start-Process -FilePath $exePath -ArgumentList $argv -NoNewWindow -PassThru `
           -RedirectStandardOutput $LogFile -RedirectStandardError ($LogFile + '.err')
  } else {
    $p = Start-Process -FilePath $exePath -ArgumentList $argv -NoNewWindow -PassThru
  }
  # Touching .Handle caches the native process handle in the returned object.
  # Without it, Start-Process -PassThru gives back a Process whose .ExitCode
  # reads back as $null once the child is gone, so this script printed "rc="
  # and then `exit $null` -> exit 0, unconditionally. Any caller that scores
  # on the exit code alone read a FAILING class as a PASS; run-doc04-
  # residuals.ps1 survived only because it re-reads the JUnit banner from the
  # log, but its `$rc -eq 124` HANG arm could never fire either.
  $null = $p.Handle
  if (-not $p.WaitForExit($TimeoutSec * 1000)) {
    $sw.Stop()
    Write-Host ('[run-one] TIMEOUT after ' + $TimeoutSec + ' s - killing pid ' + $p.Id)
    try { & taskkill /F /T /PID $p.Id 2>$null | Out-Null } catch {}
    try { $p.Kill() } catch {}
    exit 124
  }
  $sw.Stop()
  $label = $Class + $Main
  $secs  = [math]::Round($sw.Elapsed.TotalSeconds, 1)
  $rc    = $p.ExitCode
  if ($null -eq $rc) { $rc = 125 }   # never silently degrade to 0
  Write-Host ('[run-one] ' + $label + ' rc=' + $rc + ' wall=' + $secs + 's')
  exit $rc
} finally { Pop-Location }
