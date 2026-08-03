<#
  run-one.ps1 - run ONE Tomcat test class (or a probe main class) under CratonVM
  or HotSpot with exactly the suite's environment, and print the wall time.

  Exists because reproducing a doc-04 throughput measurement previously meant
  hand-assembling the classpath + the four CRATONVM_* suite variables every
  time, and getting one of them wrong silently changes the number.

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
# org.apache.tomcat.integration.httpd.* starts a real Apache httpd reverse
# proxy per test; TesterHttpd looks for a literal "httpd" on PATH unless this
# property points at one, and Windows has no httpd on PATH. See
# setup-httpd-windows.ps1, which unpacks the tree this default points at.
# Inert for every other class (nothing else reads the property), so it is set
# unconditionally rather than behind a switch.
$Httpd = 'C:\craton\tools\Apache24\bin\httpd.exe'
$argv = @("-Xmx$MaxHeap")
if (Test-Path $Httpd) { $argv += "-Dtomcat.test.httpd.path=$Httpd" }
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
  if (-not $p.WaitForExit($TimeoutSec * 1000)) {
    $sw.Stop()
    Write-Host ('[run-one] TIMEOUT after ' + $TimeoutSec + ' s - killing pid ' + $p.Id)
    try { $p.Kill() } catch {}
    exit 124
  }
  $sw.Stop()
  $label = $Class + $Main
  $secs  = [math]::Round($sw.Elapsed.TotalSeconds, 1)
  Write-Host ('[run-one] ' + $label + ' rc=' + $p.ExitCode + ' wall=' + $secs + 's')
  exit $p.ExitCode
} finally { Pop-Location }
