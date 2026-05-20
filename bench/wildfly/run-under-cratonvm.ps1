# bench/wildfly/run-under-cratonvm.ps1
# WP0.5 — run the staged EJBCA minimum fixture under cratonvm and capture
# stdout/stderr/rc deterministically. Windows PowerShell 5.1 compatible.
#
# Usage: powershell -ExecutionPolicy Bypass -File bench\wildfly\run-under-cratonvm.ps1
# Env:
#   CRATONVM_BIN   override path to cratonvm.exe
#   TIMEOUT_SEC   seconds before kill (default 60)
#
# Artifacts mirror the bash counterpart:
#   last-run.stdout.log / stderr.log / rc / meta.json

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$Here     = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot = Resolve-Path (Join-Path $Here '..\..')
$Staged   = Join-Path $Here 'staged'
$Classes  = Join-Path $Staged 'classes'
$MainFile = Join-Path $Staged 'main-class.txt'

$StdoutLog = Join-Path $Here 'last-run.stdout.log'
$StderrLog = Join-Path $Here 'last-run.stderr.log'
$RcFile    = Join-Path $Here 'last-run.rc'
$MetaFile  = Join-Path $Here 'last-run.meta.json'

if (-not (Test-Path $Classes)) {
    Write-Error "run-under-cratonvm: staged classes missing; run stage-ejbca-min.ps1 first"
    exit 2
}
if (-not (Test-Path $MainFile)) {
    Write-Error "run-under-cratonvm: main-class.txt missing; staging incomplete"
    exit 2
}
$MainClass = (Get-Content $MainFile -Raw).Trim()
if (-not $MainClass) {
    Write-Error "run-under-cratonvm: main-class.txt is empty"
    exit 2
}

$Rustjvm = $null
if ($env:CRATONVM_BIN -and (Test-Path $env:CRATONVM_BIN)) {
    $Rustjvm = $env:CRATONVM_BIN
} elseif (Test-Path (Join-Path $RepoRoot 'target\release\cratonvm.exe')) {
    $Rustjvm = Join-Path $RepoRoot 'target\release\cratonvm.exe'
}
if (-not $Rustjvm) {
    Write-Error "run-under-cratonvm: cratonvm.exe not found; build with 'cargo build --release -p cratonvm-cli'"
    exit 3
}

$CpParts = @($Classes)
Get-ChildItem -Path $Staged -Filter '*.jar' -ErrorAction SilentlyContinue | ForEach-Object {
    $CpParts += $_.FullName
}
$Cp = ($CpParts -join ';')

if ($env:TIMEOUT_SEC) { $TimeoutSec = [int]$env:TIMEOUT_SEC } else { $TimeoutSec = 60 }
$Ts = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')

Write-Output "run-under-cratonvm: binary=$Rustjvm"
Write-Output "run-under-cratonvm: cp=$Cp"
Write-Output "run-under-cratonvm: main=$MainClass"
Write-Output "run-under-cratonvm: timeout=${TimeoutSec}s"

# Run with redirected stdout/stderr and a soft timeout.
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $Rustjvm
$psi.Arguments = ('-c "{0}" {1}' -f $Cp, $MainClass)
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

$GitRev = 'unknown'
try {
    $GitRev = (git -C $RepoRoot rev-parse --short HEAD) 2>$null
    if (-not $GitRev) { $GitRev = 'unknown' }
} catch { $GitRev = 'unknown' }

$meta = [ordered]@{
    generated_at = $Ts
    cratonvm_bin  = $Rustjvm
    cratonvm_rev  = $GitRev
    main_class   = $MainClass
    classpath    = $Cp
    rc           = $Rc
    timeout_sec  = $TimeoutSec
}
$json = ($meta | ConvertTo-Json -Depth 4)
Set-Content -Path $MetaFile -Value $json -Encoding utf8

Write-Output "run-under-cratonvm: rc=$Rc"
Write-Output "run-under-cratonvm: stdout -> $StdoutLog"
Write-Output "run-under-cratonvm: stderr -> $StderrLog"
exit 0
