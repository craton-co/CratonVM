<#
.SYNOPSIS
  Apache Tomcat JUnit suite runner for CratonVM (and HotSpot baseline).

  Runs the compiled Tomcat test classes one-process-per-class, records per-class
  wall time + status (PASS / FAIL / HANG / NOSUMMARY / CRASH), and persists every
  class's stdout/stderr to its own log file. Supports:

    * class category .............. -Category passed | failed | all
                                     ("passed" = classes that PASSed in a reference
                                      results CSV; "failed" = every other class)
    * JIT toggle .................. -Jit on | off            (off => --nojit)
    * JDK mode ..................... -Jdk real | synthetic    (synthetic => --synthetic-jdk)
    * VM .......................... -Vm craton | hotspot      (hotspot = real JDK java.exe)
    * slice ....................... -Start <n> -Count <m>     (run classes [n, n+m) of the list)
    * 4 modes at once ............. -AllModes                 (craton real/synth x jit/nojit,
                                     each in its own background process, concurrently)
    * extra CratonVM knobs ........ set any CRATONVM_* env var before calling; it is
                                     inherited by every worker.

  All results go under: apps\tomcat\.suite\results\<RunName>\<mode>\
    - results.csv            class,rc,seconds,status   (resumable: existing rows are skipped)
    - <fqcn>.log / .log.err  full stdout / stderr per class
    - summary.txt            status counts + total wall time

.EXAMPLE
  # One-time setup (compiles the Tomcat tests + builds the classpath + class list):
  pwsh apps\run-tomcat-suite.ps1 -Setup

.EXAMPLE
  # Rebuild ONLY cp.txt + all-tests.txt (seconds, no ant recompile). Use after
  # dropping a previously-missing jar into apps\tomcat\.suite\lib:
  pwsh apps\run-tomcat-suite.ps1 -RefreshClasspath

.EXAMPLE
  # Run the first 50 FAILED classes on CratonVM, JIT off, real-JDK, 300s hang timeout:
  pwsh apps\run-tomcat-suite.ps1 -Category failed -Start 1 -Count 50 -Jit off -Jdk real -TimeoutSec 300

.EXAMPLE
  # Run all 4 modes concurrently over the first 100 failed classes:
  pwsh apps\run-tomcat-suite.ps1 -Category failed -Count 100 -AllModes

.EXAMPLE
  # Capture / refresh the HotSpot baseline (all classes) into a baseline file:
  pwsh apps\run-tomcat-suite.ps1 -Vm hotspot -Category all -RunName baseline
#>
[CmdletBinding()]
param(
  [ValidateSet('passed','failed','all')] [string]$Category = 'all',
  [ValidateSet('on','off')]              [string]$Jit      = 'on',
  [ValidateSet('real','synthetic')]      [string]$Jdk      = 'real',
  [ValidateSet('craton','hotspot')]      [string]$Vm       = 'craton',
  [int]$Start        = 1,              # 1-based index into the selected class list
  [int]$Count        = 0,             # 0 = run to the end of the list
  [int]$Parallel     = 4,
  [int]$TimeoutSec   = 300,
  [string]$RunName   = 'run',         # label for this run's results dir
  [string]$Exe       = '',            # CratonVM exe (default: target\release\cratonvm.exe)
  [string]$RefCsv    = '',            # results CSV used to classify passed vs failed
  [string]$MaxHeap   = '2g',
  [string]$GcFlag    = '',            # extra JVM GC-selection flag, e.g. '-XX:+UseG1GC' or '-XX:+UseZGC' (craton only; empty = default GC)
  [switch]$AllModes,                  # run the 4 craton modes concurrently
  [switch]$Setup,                     # (re)compile tests + build classpath + class list
  [switch]$RefreshClasspath,          # rebuild cp.txt + all-tests.txt only (no ant recompile)
  [switch]$AllowMissingLibs,          # do not abort when a classpath jar cannot be found
  [switch]$ListOnly                   # print the selected classes and exit
)

$ErrorActionPreference = 'Stop'
$Root   = 'C:\craton\CratonVM'
$TC     = Join-Path $Root 'apps\tomcat'
$Work   = Join-Path $TC   '.suite'          # our self-contained working dir (replaces the old .tooling)
$LIB    = 'C:\Users\Victor\tomcat-build-libs'
$JdkHome= 'C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot'
$JAVA   = Join-Path $JdkHome 'bin\java.exe'
$CpFile = Join-Path $Work 'cp.txt'
$AllList= Join-Path $Work 'all-tests.txt'
# Apache httpd for the 9 org.apache.tomcat.integration.httpd.* classes, which
# proxy real traffic through an httpd each test starts itself. Windows has no
# httpd on PATH and TesterHttpd looks for a literal "httpd" unless
# -Dtomcat.test.httpd.path points at one, so without this all 9 fail with a
# connection-refused to the proxy port - on HotSpot exactly as on CratonVM.
# Provision with: pwsh apps\tomcat-suite-runner\setup-httpd-windows.ps1
$Httpd  = 'C:\craton\tools\Apache24\bin\httpd.exe'
New-Item -ItemType Directory -Force -Path $Work | Out-Null

function Write-Info($m) { Write-Host "[suite] $m" -ForegroundColor Cyan }
function Die($m) { Write-Host "[suite] ERROR: $m" -ForegroundColor Red; exit 1 }

# Parse a -Xmx-style heap string ('2g','512m','1024k', plain bytes) to bytes.
function ConvertTo-HeapBytes([string]$h) {
  if ($h -match '^(?i)\s*(\d+)\s*([gmk]?)\s*$') {
    $n = [int64]$Matches[1]
    switch ($Matches[2].ToLower()) {
      'g' { return $n * 1GB } 'm' { return $n * 1MB } 'k' { return $n * 1KB } default { return $n }
    }
  }
  return 0
}

# Per-class heap overrides.
#
# Tests whose name contains "LargeHeap" deliberately allocate multi-GiB
# payloads (e.g. TestEncryptInterceptorLargeHeap encrypts a 1 GiB array). The
# suite-wide default -Xmx (2g) OOMs them on BOTH CratonVM and HotSpot, so a
# flat default makes the test a guaranteed both-VM failure rather than a real
# signal. Give such classes a heap that fits the working set (HotSpot needs
# >=8g; CratonVM >=10g after the humongous-cap GC fix). Never downgrade a
# larger user-supplied -MaxHeap.
$LargeHeapXmx = '12g'
# TestChunkedTransferEncodingWithProxy is the same shape without the naming
# convention: PAYLOAD_SIZE is literally 10 * 1024 * 1024 * 100 = 1 GiB, and
# TomcatBaseTest.postUrl needs a second buffer of the same size. HotSpot fits
# that in the 2g default (26.6 s measured); CratonVM's default Generational
# collector caps old-gen at Xmx/2, so a 1 GiB humongous array cannot land and
# the class OOMs at exactly `native primitive array of length 1048576000`.
# 4g clears it (110 s measured). `-Xmx2g -XX:+UseG1GC` also passes - CratonVM's
# own G1 has no fixed split - but takes 275 s, close enough to the 300 s default
# timeout to score as a HANG, so prefer the heap bump.
$ChunkedProxyXmx = '4g'
function Resolve-ClassHeap([string]$cls, [string]$defaultHeap) {
  $want = if ($cls -match 'LargeHeap') { $LargeHeapXmx }
          elseif ($cls -eq 'org.apache.tomcat.integration.httpd.TestChunkedTransferEncodingWithProxy') { $ChunkedProxyXmx }
          else { return $defaultHeap }
  # Never downgrade a larger user-supplied -MaxHeap.
  if ((ConvertTo-HeapBytes $defaultHeap) -ge (ConvertTo-HeapBytes $want)) { return $defaultHeap }
  return $want
}

# ---------------------------------------------------------------------------
# Setup: locate ant, compile tests, build classpath + class list
# ---------------------------------------------------------------------------
function Find-Ant {
  $cands = @(
    (Join-Path $Work 'apache-ant\bin\ant.bat'),
    "$env:ANT_HOME\bin\ant.bat"
  ) + (Get-ChildItem -Path $Work -Recurse -Filter 'ant.bat' -ErrorAction SilentlyContinue | Select-Object -Expand FullName)
  foreach ($c in $cands) { if ($c -and (Test-Path $c)) { return $c } }
  return $null
}

function Invoke-Setup {
  Write-Info "Setup: compiling Tomcat tests + building classpath/class list"
  $ant = Find-Ant
  if (-not $ant) {
    $zip = Join-Path $Work 'apache-ant.zip'
    Write-Info "Apache Ant not found; downloading apache-ant-1.10.14"
    Invoke-WebRequest -Uri 'https://archive.apache.org/dist/ant/binaries/apache-ant-1.10.14-bin.zip' -OutFile $zip
    Expand-Archive -Path $zip -DestinationPath $Work -Force
    $extracted = Get-ChildItem -Path $Work -Directory -Filter 'apache-ant-*' | Select-Object -First 1
    Rename-Item $extracted.FullName (Join-Path $Work 'apache-ant') -ErrorAction SilentlyContinue
    $ant = Find-Ant
  }
  if (-not $ant) { Die "could not provision Apache Ant" }
  $env:ANT_HOME  = Split-Path (Split-Path $ant)
  $env:JAVA_HOME = $JdkHome
  $env:PATH      = "$env:ANT_HOME\bin;$JdkHome\bin;$env:PATH"
  Push-Location $TC
  try {
    # Run ant via cmd /c so its (benign) stderr output does not trip
    # PowerShell's ErrorActionPreference=Stop (PS 5.1 wraps native stderr as a
    # terminating ErrorRecord). Check the ant exit code explicitly instead.
    $prevEAP = $ErrorActionPreference; $ErrorActionPreference = 'Continue'
    Write-Info "ant deploy"
    cmd /c "`"$ant`" deploy 2>&1"        | Tee-Object (Join-Path $Work 'setup-deploy.log')        | Out-Null
    if ($LASTEXITCODE -ne 0) { $ErrorActionPreference = $prevEAP; Die "ant deploy failed (see setup-deploy.log)" }
    Write-Info "ant test-compile"
    cmd /c "`"$ant`" test-compile 2>&1" | Tee-Object (Join-Path $Work 'setup-test-compile.log') | Out-Null
    if ($LASTEXITCODE -ne 0) { $ErrorActionPreference = $prevEAP; Die "ant test-compile failed (see setup-test-compile.log)" }
    $ErrorActionPreference = $prevEAP
  } finally { Pop-Location }
  Build-Classpath
  Build-ClassList
  Write-Info "Setup complete. classpath=$CpFile  classes=$((Get-Content $AllList).Count)"
}

function Build-Classpath {
  # Jars are looked up in $LIB first (the layout `ant download-compile` creates:
  # <base.path>/<lib>-<ver>/<lib>-<ver>.jar with Tomcat's own RENAMED file
  # names), then in the Gradle module cache and the Maven local repo (where the
  # same artifacts live under their UPSTREAM Maven names), then in a
  # suite-local .suite\lib drop box. Every root is searched so this keeps
  # working on a machine that has any of those layouts.
  #
  # Each entry is a LIST of candidate file-name patterns: Tomcat's renamed name
  # first, then the Maven artifact name. Passing only the renamed name is what
  # silently dropped BouncyCastle and EasyMock off this box's classpath for
  # weeks - $LIB does not exist here, and the Gradle cache only ever holds the
  # Maven names (bcprov-jdk18on-1.84.jar, not bouncycastle-provider-1.84.jar).
  $LibRoots = @($LIB,
                (Join-Path $env:USERPROFILE '.gradle\caches\modules-2\files-2.1'),
                (Join-Path $env:USERPROFILE '.m2\repository'),
                (Join-Path $Work 'lib')) |
              Where-Object { $_ -and (Test-Path $_) }
  $missing = New-Object System.Collections.Generic.List[string]
  function J([string[]]$pats) {
    foreach ($root in $LibRoots) {
      foreach ($pat in $pats) {
        $f = Get-ChildItem -Path $root -Recurse -Filter $pat -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($f) { return $f.FullName }
      }
    }
    $missing.Add(($pats -join ' | ')); return $null
  }
  $parts = @(
    (Join-Path $TC 'output\build\webapps\examples\WEB-INF\classes'),
    (Join-Path $TC 'output\testclasses'),
    (Join-Path $TC 'output\i18n'),
    (Join-Path $TC 'output\classes'),
    (J 'junit-4.13.2.jar'), (J 'hamcrest-3.0.jar'),
    # EasyMock + its two runtime deps. TestSSLValve, TestJNDIRealm,
    # TestTldScanner, TestRestCsrfPreventionFilter, TestAsyncContextImpl,
    # TestPersistentManager, TestWebappServiceLoader,
    # TestCrawlerSessionManagerValve, TestLoadBalancerDrainingValve and
    # TestRequest all fail with NoClassDefFoundError: org/easymock/EasyMock
    # without it - on HotSpot exactly as on CratonVM.
    (J @('easymock-5.6.0.jar','easymock-5.*.jar')),
    (J @('objenesis-3.*.jar')), (J @('byte-buddy-1.*.jar')),
    (J 'unboundid-ldapsdk-7.0.4.jar'),
    (J 'derby-10.17.1.0.jar'), (J 'derbyshared-10.17.1.0.jar'), (J 'derbytools-10.17.1.0.jar'),
    # BouncyCastle provider + PKIX + util, at the version build.properties.default
    # pins (bouncycastle.version=1.84). TestPQC (post-quantum ML-DSA/ML-KEM),
    # TestLargeClientHello and TestSecurity2018 build their certificates and
    # providers through BC and fail with NoClassDefFoundError:
    # org/bouncycastle/... without these - again on HotSpot too.
    (J @('bouncycastle-provider-1.84.jar','bcprov-jdk18on-1.84.jar')),
    (J @('bouncycastle-pkix-1.84.jar','bcpkix-jdk18on-1.84.jar')),
    (J @('bouncycastle-util-1.84.jar','bcutil-jdk18on-1.84.jar')),
    (J 'ecj-*.jar'),
    # Ant's own jars. org.apache.catalina.ant.TestDeployTask drives Tomcat's
    # DeployTask (an org.apache.tools.ant.Task) and org.apache.jasper.JspC -
    # under test in org.apache.jasper.TestJspC - extends Task too. Without
    # these, BOTH classes fail with NoClassDefFoundError:
    # org/apache/tools/ant/Task on HotSpot as well as CratonVM, i.e. a fixture
    # gap that reads like a CratonVM regression. Same fix as the Linux
    # harness's cp-linux-fixed.txt.
    (J 'ant-1.*.jar'), (J 'ant-launcher-1.*.jar')
  ) | Where-Object { $_ -ne $null }
  ($parts -join ';') | Set-Content -Path $CpFile -Encoding ascii -NoNewline
  Write-Info "classpath: $($parts.Count) parts -> $CpFile"
  # A jar that cannot be found used to be a Write-Warning that scrolled past in
  # the setup log, leaving a short classpath and a pile of NoClassDefFoundError
  # "failures" that look like CratonVM regressions but reproduce on HotSpot.
  # Record them, shout about them, and stop unless the caller opted in.
  $MissFile = Join-Path $Work 'cp-missing.txt'
  if ($missing.Count -gt 0) {
    ($missing -join "`r`n") | Set-Content -Path $MissFile -Encoding ascii
    Write-Host "[suite] MISSING JARS ($($missing.Count)) - the classpath is INCOMPLETE:" -ForegroundColor Red
    $missing | ForEach-Object { Write-Host "[suite]   $_" -ForegroundColor Red }
    Write-Host "[suite] searched roots: $($LibRoots -join ' ; ')" -ForegroundColor Red
    Write-Host "[suite] drop the jars into $Work\lib (any layout) and re-run -RefreshClasspath." -ForegroundColor Red
    if (-not $AllowMissingLibs) {
      Die "classpath incomplete ($($missing.Count) jars); pass -AllowMissingLibs to run anyway"
    }
  } else {
    Remove-Item $MissFile -ErrorAction SilentlyContinue
  }
}

function Build-ClassList {
  $testRoot  = Join-Path $TC 'test'
  $classRoot = Join-Path $TC 'output\testclasses'
  $names = New-Object System.Collections.Generic.List[string]
  Get-ChildItem -Path $testRoot -Recurse -Filter 'Test*.java' | ForEach-Object {
    $rel = $_.FullName.Substring($testRoot.Length + 1)
    if ($rel -match '[\\/]Tester') { return }
    $fqcn = ($rel -replace '\.java$','') -replace '[\\/]','.'
    $cls  = Join-Path $classRoot (($fqcn -replace '\.','\') + '.class')
    if (Test-Path $cls) { $names.Add($fqcn) }
  }
  ($names | Sort-Object -Unique) | Set-Content $AllList -Encoding ascii
  Write-Info "class list: $((Get-Content $AllList).Count) runnable classes -> $AllList"
}

# ---------------------------------------------------------------------------
# Class selection: category filter + start/count slice
# ---------------------------------------------------------------------------
function Get-SelectedClasses {
  if (-not (Test-Path $AllList)) { Die "class list missing; run with -Setup first" }
  $all = Get-Content $AllList | Where-Object { $_ }
  if ($Category -ne 'all') {
    $ref = $RefCsv
    if (-not $ref) { $ref = Join-Path $Work 'reference.csv' }
    if (-not (Test-Path $ref)) {
      Die "category '$Category' needs a reference results CSV. Pass -RefCsv <path>, or copy a prior run's results.csv to $ref"
    }
    $passed = @{}
    Import-Csv $ref | Where-Object { $_.status -eq 'PASS' } | ForEach-Object { $passed[$_.class] = $true }
    if ($Category -eq 'passed') { $all = $all | Where-Object { $passed.ContainsKey($_) } }
    else                        { $all = $all | Where-Object { -not $passed.ContainsKey($_) } }
  }
  # slice [Start, Start+Count)
  $from = [Math]::Max(1, $Start) - 1
  $list = @($all)
  if ($from -ge $list.Count) { return @() }
  $end  = if ($Count -gt 0) { [Math]::Min($list.Count, $from + $Count) } else { $list.Count }
  return $list[$from..($end-1)]
}

# ---------------------------------------------------------------------------
# Core runner for ONE mode (writes results.csv + per-class logs)
# ---------------------------------------------------------------------------
function Invoke-Mode {
  param([string]$ModeName, [bool]$NoJit, [bool]$Synthetic, [string[]]$Classes)

  $cp  = (Get-Content $CpFile -Raw).Trim()
  $out = Join-Path $Work "results\$RunName\$ModeName"
  New-Item -ItemType Directory -Force -Path $out | Out-Null
  $csv = Join-Path $out 'results.csv'
  if (-not (Test-Path $csv)) { 'class,rc,seconds,status' | Set-Content $csv -Encoding ascii }

  # CratonVM runtime knobs (HotSpot ignores CRATONVM_* so this is harmless for it).
  if ($Vm -eq 'craton') {
    $env:CRATONVM_REAL_NET_SOCKETS         = '1'
    $env:CRATONVM_REAL_AQS                 = '1'
    $env:CRATONVM_DISABLE_DEFAULT_WATCHDOG = '1'
    $env:CRATONVM_ROOTSNAP_CACHE           = '1'
    if ($NoJit) { $env:CRATONVM_DISABLE_JIT = '1' } else { Remove-Item Env:CRATONVM_DISABLE_JIT -ErrorAction SilentlyContinue }
  }

  $exe = if ($Vm -eq 'hotspot') { $JAVA } else { $script:CratonExe }
  $jvmArgs = @(
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
  # Inert for every class except org.apache.tomcat.integration.httpd.*.
  if (Test-Path $Httpd) { $jvmArgs += "-Dtomcat.test.httpd.path=$Httpd" }
  else { Write-Warning "[suite] httpd not found at $Httpd - the 9 org.apache.tomcat.integration.httpd.* classes will fail with connection-refused. Run setup-httpd-windows.ps1." }
  if ($Vm -eq 'craton') {
    if ($NoJit)     { $jvmArgs += '--nojit' }
    if ($Synthetic) { $jvmArgs += '--synthetic-jdk' }
    if ($GcFlag)    { $jvmArgs += $GcFlag }
  } elseif ($Vm -eq 'hotspot') {
    # HotSpot's JIT-off equivalent is the interpreter-only flag -Xint.
    if ($NoJit) { $jvmArgs += '-Xint' }
  }

  $done = @{}
  Import-Csv $csv | ForEach-Object { $done[$_.class] = $true }
  $todo = $Classes | Where-Object { -not $done.ContainsKey($_) }
  Write-Info "[$ModeName] todo=$($todo.Count) (skipping $($done.Count) done) vm=$Vm jit=$(if($NoJit){'off'}else{'on'}) jdk=$(if($Synthetic){'synthetic'}else{'real'}) timeout=${TimeoutSec}s parallel=$Parallel"

  $modeStart = Get-Date
  $script:running = @()
  function Finish-One($r, $rc, $secs) {
    $tail = ''
    if (Test-Path $r.outfile)          { $tail += (Get-Content $r.outfile -Raw -ErrorAction SilentlyContinue) }
    if (Test-Path "$($r.outfile).err") { $tail += "`n" + (Get-Content "$($r.outfile).err" -Raw -ErrorAction SilentlyContinue) }
    if ($null -eq $tail) { $tail = '' }
    $crash   = ($tail -match 'panicked|EXCEPTION_ACCESS_VIOLATION|SIGSEGV|0xC0000005|VM PANIC|stack backtrace|RUST_BACKTRACE|fatal runtime|not yet implemented|internal error: entered unreachable')
    $ok      = ($tail -match 'OK \(\d+ test')
    $fail    = ($tail -match 'FAILURES!!!|Tests run: \d+,\s+Failures')
    $status  = if ($rc -eq 'TIMEOUT') { 'HANG' }
               elseif ($ok)   { if ($crash) { 'CRASH' } else { 'PASS' } }
               elseif ($fail) { 'FAIL' }
               else           { if ($crash) { 'CRASH' } else { 'NOSUMMARY' } }
    "`"$($r.class)`",$rc,$secs,$status" | Add-Content $csv -Encoding ascii
    Write-Host ("  [{0}] {1,-9} {2,7}s  {3}" -f $ModeName, $status, $secs, $r.class)
  }
  function Drain {
    $still = @()
    foreach ($r in $script:running) {
      $el = ((Get-Date) - $r.start).TotalSeconds
      if ($r.proc.HasExited) {
        $rc = 0; try { $rc = $r.proc.ExitCode } catch { $rc = 'NA' }
        Finish-One $r $rc ([math]::Round($el,1))
      } elseif ($el -gt $TimeoutSec) {
        try { & taskkill /F /T /PID $r.proc.Id 2>$null | Out-Null } catch {}
        try { $r.proc.Kill($true) } catch { try { $r.proc.Kill() } catch {} }
        try { $r.proc.WaitForExit(5000) | Out-Null } catch {}
        Finish-One $r 'TIMEOUT' ([math]::Round($el,1))
      } else { $still += $r }
    }
    $script:running = $still
  }
  foreach ($cls in $todo) {
    while ($script:running.Count -ge $Parallel) { Drain; if ($script:running.Count -ge $Parallel) { Start-Sleep -Milliseconds 150 } }
    $safe    = $cls -replace '[^A-Za-z0-9._]','_'
    $outfile = Join-Path $out "$safe.log"
    # Per-class heap: bump *LargeHeap classes off the suite default (which
    # OOMs them on both VMs by design). $jvmArgs[0] is '-Xmx<MaxHeap>'.
    $clsHeap = Resolve-ClassHeap $cls $MaxHeap
    $clsJvm  = if ($clsHeap -eq $MaxHeap) { $jvmArgs } else { ,("-Xmx$clsHeap") + ($jvmArgs | Select-Object -Skip 1) }
    $allArgs = $clsJvm + @('-cp',$cp,'org.junit.runner.JUnitCore',$cls)
    $p = Start-Process -FilePath $exe -ArgumentList $allArgs -PassThru -NoNewWindow `
          -WorkingDirectory $TC -RedirectStandardOutput $outfile -RedirectStandardError "$outfile.err"
    try { $null = $p.Handle } catch {}
    $script:running += @{ proc=$p; class=$cls; start=(Get-Date); outfile=$outfile }
  }
  while ($script:running.Count -gt 0) { Drain; if ($script:running.Count -gt 0) { Start-Sleep -Milliseconds 150 } }

  $elapsed = [math]::Round(((Get-Date) - $modeStart).TotalMinutes,1)
  $rows = Import-Csv $csv
  $summary = "mode=$ModeName vm=$Vm jit=$(if($NoJit){'off'}else{'on'}) jdk=$(if($Synthetic){'synthetic'}else{'real'})`n" +
             "classes=$($rows.Count)  wall=${elapsed}min`n" +
             (($rows | Group-Object status | Sort-Object Count -Descending | ForEach-Object { "  {0,-10} {1}" -f $_.Name, $_.Count }) -join "`n")
  $summary | Set-Content (Join-Path $out 'summary.txt') -Encoding ascii
  Write-Info "[$ModeName] DONE  wall=${elapsed}min"
  Write-Host $summary
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------
if ($RefreshClasspath) {
  # Rebuild the classpath (and class list) WITHOUT the ~20 min ant recompile.
  # Needed whenever a previously-missing jar shows up, or a lib root moves.
  Write-Info "RefreshClasspath: rebuilding cp.txt + all-tests.txt (no ant recompile)"
  Build-Classpath
  Build-ClassList
  return
}
if ($Setup) { Invoke-Setup; if (-not $AllModes -and $Category -eq 'all' -and -not $ListOnly) { return } }

# resolve the CratonVM exe
if (-not $Exe) {
  $cands = @("$Root\target\release\cratonvm.exe") +
           (Get-ChildItem "$Root-*\target\release\cratonvm.exe" -ErrorAction SilentlyContinue | Sort-Object LastWriteTime -Descending | Select-Object -Expand FullName)
  $Exe = $cands | Where-Object { Test-Path $_ } | Select-Object -First 1
}
$script:CratonExe = $Exe
if ($Vm -eq 'craton' -and -not $ListOnly -and (-not $Exe -or -not (Test-Path $Exe))) {
  Die "CratonVM exe not found; pass -Exe <path to cratonvm.exe>"
}

$classes = Get-SelectedClasses
Write-Info "selected $($classes.Count) classes  (category=$Category start=$Start count=$(if($Count){$Count}else{'all'}))"
if ($ListOnly) { $classes | ForEach-Object { Write-Host "  $_" }; return }
if ($classes.Count -eq 0) { Die "no classes selected" }

if ($AllModes) {
  if ($Vm -ne 'craton') { Die "-AllModes is craton-only (the 4 modes are real/synthetic x jit/nojit)" }
  Write-Info "launching 4 modes concurrently, each in its own process"
  $modes = @(
    @{ name='real-jit';        nojit=$false; synth=$false },
    @{ name='real-nojit';      nojit=$true;  synth=$false },
    @{ name='synthetic-jit';   nojit=$false; synth=$true  },
    @{ name='synthetic-nojit'; nojit=$true;  synth=$true  }
  )
  $self = $PSCommandPath
  $procs = @()
  foreach ($m in $modes) {
    $jitArg = if ($m.nojit) { 'off' } else { 'on' }
    $jdkArg = if ($m.synth) { 'synthetic' } else { 'real' }
    $childArgs = @('-NoProfile','-File',$self,
      '-Vm','craton','-Category',$Category,'-Start',$Start,'-Count',$Count,
      '-Jit',$jitArg,'-Jdk',$jdkArg,'-Parallel',$Parallel,'-TimeoutSec',$TimeoutSec,
      '-RunName',$RunName,'-MaxHeap',$MaxHeap)
    if ($Exe)    { $childArgs += @('-Exe',$Exe) }
    if ($RefCsv) { $childArgs += @('-RefCsv',$RefCsv) }
    $log = Join-Path $Work "results\$RunName\$($m.name).console.log"
    New-Item -ItemType Directory -Force -Path (Split-Path $log) | Out-Null
    Write-Info "  -> mode $($m.name)"
    $procs += Start-Process -FilePath 'powershell.exe' -ArgumentList $childArgs -PassThru -RedirectStandardOutput $log -RedirectStandardError "$log.err"
  }
  Write-Info "waiting for all 4 modes ..."
  $procs | ForEach-Object { $_.WaitForExit() }
  Write-Info "ALL MODES DONE. Results under $Work\results\$RunName\{real-jit,real-nojit,synthetic-jit,synthetic-nojit}\"
} else {
  $modeName = if ($Vm -eq 'hotspot') { "hotspot-$(if($Jit -eq 'off'){'xint'}else{'jit'})" } else { "$Jdk-$(if($Jit -eq 'off'){'nojit'}else{'jit'})" }
  Invoke-Mode -ModeName $modeName -NoJit:($Jit -eq 'off') -Synthetic:($Jdk -eq 'synthetic') -Classes $classes
}
