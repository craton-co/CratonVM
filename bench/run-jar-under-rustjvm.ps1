# bench/run-jar-under-rustjvm.ps1
# Run a JAR under rustjvm.exe with async stdout/stderr reads before materializing exit code.
# Avoids PowerShell pipe / redirected-stream deadlock where WaitForExit fills OS buffers (symptom: rc=124 on timeout while process already exited).
#
# Environment (inherited by child unless you clear it):
#   RUSTJVM_BIN       - path to rustjvm.exe (overrides -RustJvmBin)
#   JAVA_HOME         - JDK root (overrides -JavaHome when param empty)
#   TIMEOUT_SEC       - default timeout when -TimeoutSec omitted (-1); 0 = wait forever
#   RUSTJVM_DISABLE_JIT, RUSTJVM_* - passed through like any env var
#   RUSTJVM_CLI_EXTRA - optional extra CLI tokens (space-separated) inserted before --jar

param(
    [Parameter(Mandatory = $true, Position = 0)]
    [string]$JarPath,
    [string]$RunName = '',
    [string]$JavaHome = '',
    [string]$RustJvmBin = '',
    [int]$TimeoutSec = -1,
    [string]$Xmx = '1g',
    [string[]]$JarProgramArgs = @()
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$Here = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot = Resolve-Path (Join-Path $Here '..')
$Applogs = Join-Path $RepoRoot 'applogs'
if (-not (Test-Path $Applogs)) {
    New-Item -ItemType Directory -Force -Path $Applogs | Out-Null
}

$JarResolved = Resolve-Path -LiteralPath $JarPath -ErrorAction Stop
if (-not $RunName) {
    $RunName = [System.IO.Path]::GetFileNameWithoutExtension($JarResolved.Path)
}
$RunName = ($RunName -replace '[^\w\-\.]+', '_').Trim('_')
if (-not $RunName) { $RunName = 'app' }

if (-not $JavaHome) {
    if ($env:JAVA_HOME) { $JavaHome = $env:JAVA_HOME }
    else { Write-Error 'run-jar-under-rustjvm: set JAVA_HOME or pass -JavaHome' }
}
if (-not (Test-Path (Join-Path $JavaHome 'bin\java.exe'))) {
    Write-Error "run-jar-under-rustjvm: JAVA_HOME invalid (no bin\java.exe): $JavaHome"
}

if (-not $RustJvmBin) {
    if ($env:RUSTJVM_BIN -and (Test-Path $env:RUSTJVM_BIN)) {
        $RustJvmBin = $env:RUSTJVM_BIN
    }
    elseif (Test-Path (Join-Path $RepoRoot 'target\release\rustjvm.exe')) {
        $RustJvmBin = Join-Path $RepoRoot 'target\release\rustjvm.exe'
    }
    elseif (Test-Path (Join-Path $RepoRoot 'target\wf-build2\release\rustjvm.exe')) {
        $RustJvmBin = Join-Path $RepoRoot 'target\wf-build2\release\rustjvm.exe'
    }
}
if (-not $RustJvmBin -or -not (Test-Path $RustJvmBin)) {
    Write-Error 'run-jar-under-rustjvm: rustjvm.exe not found; set RUSTJVM_BIN or build rustjvm-cli'
}

if ($TimeoutSec -lt 0) {
    if ($env:TIMEOUT_SEC) { $TimeoutSec = [int]$env:TIMEOUT_SEC }
    else { $TimeoutSec = 120 }
}

if ($env:STDERR_TAIL_LINES) { $TailN = [int]$env:STDERR_TAIL_LINES } else { $TailN = 80 }

$Ts = (Get-Date).ToUniversalTime().ToString('yyyyMMdd-HHmmss')
$Prefix = Join-Path $Applogs "run_${Ts}_${RunName}"
$OutLog = "$Prefix.out.txt"
$ErrLog = "$Prefix.err.txt"
$RcFile = "$Prefix.rc.txt"

function Quote-Arg([string]$a) {
    if ($a -match '[\s"]') { return '"' + ($a.Replace('"', '\"')) + '"' }
    return $a
}

$AllArgs = [System.Collections.Generic.List[string]]::new()
$AllArgs.Add('--java-home')
$AllArgs.Add(($JavaHome -replace '\\', '/'))
$AllArgs.Add('--Xmx')
$AllArgs.Add($Xmx)
if ($env:RUSTJVM_CLI_EXTRA) {
    foreach ($tok in ($env:RUSTJVM_CLI_EXTRA -split '\s+')) {
        if ($tok) { $AllArgs.Add($tok) }
    }
}
$AllArgs.Add('--jar')
$AllArgs.Add(($JarResolved.Path -replace '\\', '/'))
foreach ($j in $JarProgramArgs) { $AllArgs.Add($j) }

$argLine = (($AllArgs | ForEach-Object { Quote-Arg $_ }) -join ' ')

Write-Output "run-jar-under-rustjvm: binary=$RustJvmBin"
Write-Output "run-jar-under-rustjvm: argv=$argLine"
Write-Output "run-jar-under-rustjvm: timeout_sec=$TimeoutSec (0 = infinite)"

$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $RustJvmBin
$psi.Arguments = $argLine
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.UseShellExecute = $false
$psi.CreateNoWindow = $true

$p = [System.Diagnostics.Process]::Start($psi)
if (-not $p) { Write-Error 'run-jar-under-rustjvm: Process::Start returned null' }

$stdoutTask = $p.StandardOutput.ReadToEndAsync()
$stderrTask = $p.StandardError.ReadToEndAsync()

$Rc = 0
if ($TimeoutSec -eq 0) {
    $p.WaitForExit() | Out-Null
    $Rc = $p.ExitCode
}
else {
    $exited = $p.WaitForExit([Math]::Max(1, $TimeoutSec) * 1000)
    if (-not $exited) {
        try { $p.Kill() } catch {}
        $p.WaitForExit(5000) | Out-Null
        $Rc = 124
    }
    else {
        $Rc = $p.ExitCode
    }
}

$Stdout = $stdoutTask.Result
$Stderr = $stderrTask.Result

Set-Content -Path $OutLog -Value $Stdout -Encoding utf8
Set-Content -Path $ErrLog -Value $Stderr -Encoding utf8
Set-Content -Path $RcFile -Value "$Rc" -Encoding utf8

Write-Output "run-jar-under-rustjvm: rc=$Rc"
Write-Output "run-jar-under-rustjvm: stdout -> $OutLog"
Write-Output "run-jar-under-rustjvm: stderr -> $ErrLog"
Write-Output "run-jar-under-rustjvm: rc file -> $RcFile"

if ($Stderr -and $Stderr.Length -gt 0) {
    Write-Output "run-jar-under-rustjvm: --- last $TailN stderr lines ---"
    $Stderr -split "`r?`n" | Select-Object -Last $TailN | ForEach-Object { Write-Output $_ }
    Write-Output "run-jar-under-rustjvm: --- end stderr tail ---"
}

exit 0
