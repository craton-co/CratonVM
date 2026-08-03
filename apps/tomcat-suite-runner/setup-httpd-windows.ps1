<#
.SYNOPSIS
  Provision the Apache httpd fixture the org.apache.tomcat.integration.httpd.*
  test classes need, on Windows.

.DESCRIPTION
  The 9 classes under test/org/apache/tomcat/integration/httpd/ proxy real HTTP
  (and HTTPS) traffic through an Apache httpd reverse proxy that each test
  starts itself via TesterHttpd. Without httpd they all fail with a
  connection-refused to the proxy port - on HotSpot exactly as on CratonVM, so
  the whole family reads like a VM defect when it is only a missing fixture.
  See docs/internal/tomcat/httpd-proxy-integration-windows-connection-refused.md.

  Two things are needed, and this script does both:

    1. An httpd binary with mod_proxy / mod_proxy_http / mod_ssl / mod_headers /
       mod_env / mod_authz_core. Apache publishes no Windows binaries, so this
       pulls the Apache Lounge VS17 build and verifies its published SHA-256
       before unpacking. Nothing is installed system-wide: no service, no
       registry entry, no PATH change. Delete $InstallRoot to undo.

       Module paths in the generated test configs are relative ("modules/
       mod_proxy.so"), and httpd on Windows resolves them against the directory
       above its own .exe - so the tree works unpacked anywhere and needs no
       ServerRoot.

    2. A fixture patch to TesterHttpd.isHttpdReady(). Upstream allows httpd
       1000 ms to bind its listener, which is ample on Linux (tens of ms) but
       consistently short on Windows, where MPM WinNT startup measures 1.0-1.5 s.
       Without the patch every class in the family fails ~1 s in, even with a
       perfectly good httpd installed. apps/tomcat is not git-tracked, so the
       change ships as fixtures/httpd-ready-timeout.patch and is applied here.

  Idempotent: re-running skips whichever half is already in place.

.PARAMETER InstallRoot
  Where to unpack Apache24. Default C:\craton\tools\Apache24.

.PARAMETER SkipPatch
  Provision the binary only; leave apps/tomcat alone.

.EXAMPLE
  pwsh apps\tomcat-suite-runner\setup-httpd-windows.ps1

.EXAMPLE
  # then, in the suite runner (already wired):
  #   -Dtomcat.test.httpd.path=C:\craton\tools\Apache24\bin\httpd.exe
#>
[CmdletBinding()]
param(
  [string]$InstallRoot = 'C:\craton\tools\Apache24',
  [string]$RepoRoot    = 'C:\craton\CratonVM',
  [switch]$SkipPatch
)

$ErrorActionPreference = 'Stop'

# Apache Lounge VS17 build. VS17 (not the newer VS18) on purpose: it links the
# VC++ 2015-2022 runtime (vcruntime140.dll), which is already present on this
# box, whereas VS18 wants a newer redistributable.
$Zip    = 'httpd-2.4.66-251206-Win64-VS17.zip'
$Url    = "https://www.apachelounge.com/download/VS17/binaries/$Zip"
# Published alongside the zip as $Zip.txt (SHA1-SHA512 checksums).
$Sha256 = '2CD1F349B6705E43E784E876A233EEB1A859FA6B8ABC693A91F87B35A368F7F9'

$Patch  = Join-Path $PSScriptRoot 'fixtures\httpd-ready-timeout.patch'
$Httpd  = Join-Path $InstallRoot 'bin\httpd.exe'

function Write-Info($m) { Write-Host "[httpd-setup] $m" -ForegroundColor Cyan }
function Die($m) { Write-Host "[httpd-setup] ERROR: $m" -ForegroundColor Red; exit 1 }

# Windows PowerShell 5.1 wraps a native command's stderr in an ErrorRecord, so
# under ErrorActionPreference=Stop any exe that writes to stderr - httpd -t
# prints "Syntax OK" there, git apply --check prints its complaint there -
# terminates the script no matter what it exited with. Run natives inside a
# Continue window and judge them by $LASTEXITCODE.
function Invoke-Native([scriptblock]$sb) {
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $global:LASTEXITCODE = 0
        & $sb 2>&1 | Out-String
    } finally { $ErrorActionPreference = $prev }
}

# ---------------------------------------------------------------------------
# 1. httpd binary
# ---------------------------------------------------------------------------
if (Test-Path $Httpd) {
  Write-Info "httpd already present: $Httpd"
} else {
  $parent = Split-Path $InstallRoot -Parent
  New-Item -ItemType Directory -Force -Path $parent | Out-Null
  $tmp = Join-Path ([IO.Path]::GetTempPath()) $Zip
  Write-Info "downloading $Url"
  # curl.exe rather than Invoke-WebRequest: apachelounge.com rejects some
  # default PowerShell user agents.
  $dl = Invoke-Native { & curl.exe -sS -L -A 'Mozilla/5.0 (Windows NT 10.0; Win64; x64)' -o $tmp $Url }
  if ($LASTEXITCODE -ne 0) { Die "download failed (curl rc=${LASTEXITCODE}): $dl" }

  $got = (Get-FileHash $tmp -Algorithm SHA256).Hash
  if ($got -ne $Sha256) { Die "SHA-256 mismatch for ${Zip}: got $got, expected $Sha256" }
  Write-Info "SHA-256 verified"

  # The zip's single root entry is 'Apache24'; extracting to the parent lands it
  # exactly at $InstallRoot when $InstallRoot is named Apache24.
  Expand-Archive -Path $tmp -DestinationPath $parent -Force
  Remove-Item $tmp -Force -ErrorAction SilentlyContinue
  if (-not (Test-Path $Httpd)) { Die "expected $Httpd after unpacking; check the archive layout" }
  Write-Info "unpacked to $InstallRoot"
}

# Smoke-test the exact module set the test configs load. A missing VC++ runtime
# or a truncated unpack shows up here rather than as 9 opaque test failures.
$probe = Join-Path ([IO.Path]::GetTempPath()) 'cratonvm-httpd-probe.conf'
@'
Listen 54999
LoadModule authz_core_module modules/mod_authz_core.so
LoadModule proxy_module modules/mod_proxy.so
LoadModule proxy_http_module modules/mod_proxy_http.so
LoadModule env_module modules/mod_env.so
LoadModule headers_module modules/mod_headers.so
LoadModule ssl_module modules/mod_ssl.so
ErrorLog "|C:/Windows/System32/more.com"
LogLevel warn
ServerName localhost:54999
'@ | Set-Content -Path $probe -Encoding ascii
$out = Invoke-Native { & $Httpd -t -f $probe }
Remove-Item $probe -Force -ErrorAction SilentlyContinue
if ($LASTEXITCODE -ne 0 -or $out -notmatch 'Syntax OK') { Die "httpd cannot load the required modules (rc=${LASTEXITCODE}):`n$out" }
Write-Info "module smoke test: Syntax OK"

# ---------------------------------------------------------------------------
# 2. TesterHttpd readiness-timeout patch
# ---------------------------------------------------------------------------
if ($SkipPatch) {
  Write-Info "-SkipPatch given; not touching apps/tomcat"
} else {
  if (-not (Test-Path $Patch)) { Die "patch missing: $Patch" }
  Push-Location $RepoRoot
  try {
    # --reverse --check succeeds only when the patch is ALREADY applied.
    $null = Invoke-Native { & git apply --check --reverse $Patch }
    if ($LASTEXITCODE -eq 0) {
      Write-Info "TesterHttpd readiness patch already applied"
    } else {
      $chk = Invoke-Native { & git apply --check $Patch }
      if ($LASTEXITCODE -ne 0) { Die "fixtures/httpd-ready-timeout.patch does not apply - apps/tomcat's TesterHttpd.java has diverged; re-derive it.`n$chk" }
      $ap = Invoke-Native { & git apply $Patch }
      if ($LASTEXITCODE -ne 0) { Die "git apply failed: $ap" }
      Write-Info "applied TesterHttpd readiness patch"

      $tc = Join-Path $RepoRoot 'apps\tomcat'
      $cpFile = Join-Path $tc '.suite\cp.txt'
      if (Test-Path $cpFile) {
        $cp = (Get-Content $cpFile -Raw).Trim()
        $javac = 'C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot\bin\javac.exe'
        Push-Location $tc
        try {
          $jc = Invoke-Native { & $javac -nowarn -cp $cp -d "$tc\output\testclasses" 'test\org\apache\tomcat\integration\httpd\TesterHttpd.java' }
          if ($LASTEXITCODE -ne 0) { Die "javac failed on the patched TesterHttpd.java:`n$jc" }
          Write-Info "recompiled TesterHttpd into output\testclasses"
        } finally { Pop-Location }
      } else {
        Write-Warning "[httpd-setup] $cpFile missing - run run-tomcat-suite.ps1 -Setup, then recompile TesterHttpd.java"
      }
    }
  } finally { Pop-Location }
}

Write-Info "done. httpd=$Httpd"
