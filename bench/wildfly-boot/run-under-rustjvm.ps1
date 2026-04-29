# bench/wildfly-boot/run-under-rustjvm.ps1
# WP8.10.2 — Windows PowerShell 5.1 mirror of run-under-rustjvm.sh.

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$Here     = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot = Resolve-Path (Join-Path $Here '..\..')
$Staged   = Join-Path $Here 'staged'
$WfHome   = Join-Path $Staged 'wildfly'

$StdoutLog = Join-Path $Here 'last-run.stdout.log'
$StderrLog = Join-Path $Here 'last-run.stderr.log'
$RcFile    = Join-Path $Here 'last-run.rc'
$MetaFile  = Join-Path $Here 'last-run.meta.json'

if (-not (Test-Path (Join-Path $WfHome 'jboss-modules.jar'))) {
    Write-Error "run-under-rustjvm: WildFly not staged; run stage.ps1 first (looking for $WfHome\jboss-modules.jar)"
    exit 2
}

$Rustjvm = $null
if ($env:RUSTJVM_BIN -and (Test-Path $env:RUSTJVM_BIN)) {
    $Rustjvm = $env:RUSTJVM_BIN
} elseif (Test-Path (Join-Path $RepoRoot 'target\release\rustjvm.exe')) {
    $Rustjvm = Join-Path $RepoRoot 'target\release\rustjvm.exe'
}
if (-not $Rustjvm) {
    Write-Error "run-under-rustjvm: rustjvm.exe not found; build with 'cargo build --release -p rustjvm-cli'"
    exit 3
}

$WfHomeFwd = ($WfHome -replace '\\','/')
$JbossModulesJar = "$WfHomeFwd/jboss-modules.jar"
$ModulesDir      = "$WfHomeFwd/modules"
$LogFile         = "$WfHomeFwd/standalone/log/server.log"
$LoggingCfg      = "file:$WfHomeFwd/standalone/configuration/logging.properties"

if ($env:SERVER_CONFIG) { $ServerConfig = $env:SERVER_CONFIG } else { $ServerConfig = 'standalone.xml' }

$LogDir = Join-Path $WfHome 'standalone\log'
if (-not (Test-Path $LogDir)) { New-Item -ItemType Directory -Force -Path $LogDir | Out-Null }

if ($env:TIMEOUT_SEC) { $TimeoutSec = [int]$env:TIMEOUT_SEC } else { $TimeoutSec = 60 }
if ($env:STDERR_TAIL_LINES) { $TailN = [int]$env:STDERR_TAIL_LINES } else { $TailN = 100 }
$Ts = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')

$SysProps = @(
    "-Dprogram.name=standalone.bat",
    "-Djboss.home.dir=$WfHomeFwd",
    "-Dorg.jboss.boot.log.file=$LogFile",
    "-Dlogging.configuration=$LoggingCfg",
    "-Djava.util.logging.manager=org.jboss.logmanager.LogManager",
    "-Djboss.modules.system.pkgs=org.jboss.byteman",
    "-Dfile.encoding=UTF-8"
)
$JbossArgs = @('-mp', $ModulesDir, 'org.jboss.as.standalone', "--server-config=$ServerConfig")

$AllArgs = @()
$AllArgs += $SysProps
$AllArgs += @('--jar', $JbossModulesJar)
$AllArgs += $JbossArgs

Write-Output "run-under-rustjvm: binary=$Rustjvm"
Write-Output "run-under-rustjvm: WILDFLY_HOME=$WfHomeFwd"
Write-Output "run-under-rustjvm: jar=$JbossModulesJar"
Write-Output "run-under-rustjvm: timeout=${TimeoutSec}s"
Write-Output "run-under-rustjvm: server-config=$ServerConfig"

function Quote-Arg([string]$a) {
    if ($a -match '\s') { return ('"' + $a.Replace('"','\"') + '"') }
    return $a
}
$argLine = (($AllArgs | ForEach-Object { Quote-Arg $_ }) -join ' ')

$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $Rustjvm
$psi.Arguments = $argLine
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError  = $true
$psi.UseShellExecute = $false

$p = [System.Diagnostics.Process]::Start($psi)
$stdoutTask = $p.StandardOutput.ReadToEndAsync()
$stderrTask = $p.StandardError.ReadToEndAsync()
$exited = $p.WaitForExit($TimeoutSec * 1000)
if (-not $exited) {
    try { $p.Kill() } catch {}
    $p.WaitForExit(5000) | Out-Null
    $Rc = 124
} else {
    $Rc = $p.ExitCode
}
$Stdout = $stdoutTask.Result
$Stderr = $stderrTask.Result

Set-Content -Path $StdoutLog -Value $Stdout -Encoding utf8
Set-Content -Path $StderrLog -Value $Stderr -Encoding utf8
Set-Content -Path $RcFile    -Value $Rc     -Encoding utf8

if ($Stderr -and $Stderr.Length -gt 0) {
    Write-Output "run-under-rustjvm: --- last $TailN stderr lines ---"
    $Stderr -split "`n" | Select-Object -Last $TailN | ForEach-Object { Write-Output $_ }
    Write-Output "run-under-rustjvm: --- end stderr tail ---"
}

$GitRev = 'unknown'
try {
    $rev = (git -C $RepoRoot rev-parse --short HEAD) 2>$null
    if ($rev) { $GitRev = $rev.Trim() }
} catch { $GitRev = 'unknown' }

$meta = [ordered]@{
    generated_at  = $Ts
    rustjvm_bin   = $Rustjvm
    rustjvm_rev   = $GitRev
    wildfly_home  = $WfHomeFwd
    jboss_modules = $JbossModulesJar
    server_config = $ServerConfig
    argv          = $AllArgs
    rc            = $Rc
    timeout_sec   = $TimeoutSec
}
($meta | ConvertTo-Json -Depth 4) | Set-Content -Path $MetaFile -Encoding utf8

Write-Output "run-under-rustjvm: rc=$Rc"
Write-Output "run-under-rustjvm: stdout -> $StdoutLog"
Write-Output "run-under-rustjvm: stderr -> $StderrLog"
Write-Output "run-under-rustjvm: meta   -> $MetaFile"
exit 0
