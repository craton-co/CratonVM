# bench/kafka/run-under-cratonvm.ps1
# WP8.7 — run the staged kafka fixture under cratonvm (Windows PS 5.1).
# Env: CRATONVM_BIN override, TIMEOUT_SEC default 600 (10 minutes).

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

if (-not (Test-Path $Classes))  { Write-Error "run-kafka: staged classes missing; run stage.ps1 first"; exit 2 }
if (-not (Test-Path $MainFile)) { Write-Error "run-kafka: main-class.txt missing"; exit 2 }
$MainClass = (Get-Content $MainFile -Raw).Trim()
if (-not $MainClass) { Write-Error "run-kafka: main-class.txt empty"; exit 2 }

$Rustjvm = $null
if ($env:CRATONVM_BIN -and (Test-Path $env:CRATONVM_BIN)) {
    $Rustjvm = $env:CRATONVM_BIN
} elseif (Test-Path (Join-Path $RepoRoot 'target\release\cratonvm.exe')) {
    $Rustjvm = Join-Path $RepoRoot 'target\release\cratonvm.exe'
}
if (-not $Rustjvm) { Write-Error "run-kafka: cratonvm.exe not found; cargo build --release -p cratonvm-cli"; exit 3 }

$CpParts = @($Classes)
Get-ChildItem -Path $Staged -Filter '*.jar' -ErrorAction SilentlyContinue | ForEach-Object { $CpParts += $_.FullName }
$Cp = ($CpParts -join ';')

if ($env:TIMEOUT_SEC) { $TimeoutSec = [int]$env:TIMEOUT_SEC } else { $TimeoutSec = 600 }
$Ts = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')

Write-Output "run-kafka: binary=$Rustjvm"
Write-Output "run-kafka: cp=$Cp"
Write-Output "run-kafka: main=$MainClass"
Write-Output "run-kafka: timeout=${TimeoutSec}s"

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

Write-Output "run-kafka: rc=$Rc"
Write-Output "run-kafka: stdout -> $StdoutLog"
Write-Output "run-kafka: stderr -> $StderrLog"
exit 0
