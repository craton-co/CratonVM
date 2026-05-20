# bench/wave2-3/run-under-cratonvm.ps1
# WP2.3-D - Windows PowerShell 5.1 runner for the staged CGLIB +
# ByteBuddy probes. Mirrors run-under-cratonvm.sh.
#
# Pre-conditions: stage-cglib-probe.ps1 + stage-bytebuddy-probe.ps1
# already ran (each may have written staged-*/skipped.flag if the
# matching jar wasn't found - we still run the synthetic fallback).
#
# Env:
#   CRATONVM_BIN   override path to cratonvm.exe.
#   TIMEOUT_SEC   per-probe timeout in seconds (default 60).
#   PROBE         "cglib" | "bytebuddy" | "both" (default both).
#
# Artifacts mirror the bash counterpart.

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$Here     = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot = Resolve-Path (Join-Path $Here '..\..')

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

if ($env:TIMEOUT_SEC) { $TimeoutSec = [int]$env:TIMEOUT_SEC } else { $TimeoutSec = 60 }
$Ts = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')

$GitRev = 'unknown'
try {
    $GitRev = (& git -C $RepoRoot rev-parse --short HEAD)
    if (-not $GitRev) { $GitRev = 'unknown' }
} catch { $GitRev = 'unknown' }

function Run-Probe {
    param([string]$Probe)
    $stagedDir  = Join-Path $Here ("staged-" + $Probe)
    $classesDir = Join-Path $stagedDir 'classes'
    $mainFile   = Join-Path $stagedDir 'main-class.txt'
    $skipFlag   = Join-Path $stagedDir 'skipped.flag'
    $stdoutLog  = Join-Path $Here ("last-run-" + $Probe + ".stdout.log")
    $stderrLog  = Join-Path $Here ("last-run-" + $Probe + ".stderr.log")
    $rcFile     = Join-Path $Here ("last-run-" + $Probe + ".rc")
    $metaFile   = Join-Path $Here ("last-run-" + $Probe + ".meta.json")

    Set-Content -Path $stdoutLog -Value '' -Encoding utf8
    Set-Content -Path $stderrLog -Value '' -Encoding utf8

    if (-not (Test-Path $classesDir) -or -not (Test-Path $mainFile)) {
        Write-Output "run-under-cratonvm: probe=$Probe NOT-STAGED (run stage-$Probe-probe.ps1 first)"
        Set-Content -Path $rcFile -Value '127' -Encoding utf8
        $meta = [ordered]@{
            probe        = $Probe
            generated_at = $Ts
            cratonvm_bin  = $Rustjvm
            cratonvm_rev  = $GitRev
            main_class   = ''
            classpath    = ''
            rc           = 127
            status       = 'not-staged'
            timeout_sec  = $TimeoutSec
        }
        Set-Content -Path $metaFile -Value (($meta | ConvertTo-Json -Depth 4)) -Encoding utf8
        return
    }

    $mainClass = (Get-Content -Path $mainFile -Raw).Trim()
    if (-not $mainClass) {
        Write-Output "run-under-cratonvm: probe=$Probe ERROR main-class.txt empty"
        Set-Content -Path $rcFile -Value '126' -Encoding utf8
        return
    }

    $cpParts = @($classesDir)
    Get-ChildItem -Path $stagedDir -Filter '*.jar' -ErrorAction SilentlyContinue | ForEach-Object {
        $cpParts += $_.FullName
    }
    $cp = ($cpParts -join ';')

    $skipped = if (Test-Path $skipFlag) { 'yes' } else { 'no' }

    Write-Output "run-under-cratonvm: probe=$Probe binary=$Rustjvm"
    Write-Output "run-under-cratonvm: probe=$Probe cp=$cp"
    Write-Output "run-under-cratonvm: probe=$Probe main=$mainClass skipped-real-jar=$skipped"
    Write-Output "run-under-cratonvm: probe=$Probe timeout=${TimeoutSec}s"

    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $Rustjvm
    $psi.Arguments = ('-c "{0}" {1}' -f $cp, $mainClass)
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
        $rc = 124
    } else {
        $rc = $p.ExitCode
    }
    $stdout = $stdoutTask.Result
    $stderr = $stderrTask.Result

    Set-Content -Path $stdoutLog -Value $stdout -Encoding utf8
    Set-Content -Path $stderrLog -Value $stderr -Encoding utf8
    Set-Content -Path $rcFile    -Value $rc     -Encoding utf8

    $meta = [ordered]@{
        probe             = $Probe
        generated_at      = $Ts
        cratonvm_bin       = $Rustjvm
        cratonvm_rev       = $GitRev
        main_class        = $mainClass
        classpath         = $cp
        rc                = $rc
        status            = 'ran'
        skipped_real_jar  = $skipped
        timeout_sec       = $TimeoutSec
    }
    Set-Content -Path $metaFile -Value (($meta | ConvertTo-Json -Depth 4)) -Encoding utf8
    Write-Output "run-under-cratonvm: probe=$Probe rc=$rc"
}

$ProbeFilter = if ($env:PROBE) { $env:PROBE } else { 'both' }
if ($ProbeFilter -ne 'cglib' -and $ProbeFilter -ne 'bytebuddy' -and $ProbeFilter -ne 'both') {
    Write-Error "run-under-cratonvm: ERROR PROBE must be cglib | bytebuddy | both"
    exit 4
}

if ($ProbeFilter -eq 'cglib' -or $ProbeFilter -eq 'both')     { Run-Probe -Probe 'cglib' }
if ($ProbeFilter -eq 'bytebuddy' -or $ProbeFilter -eq 'both') { Run-Probe -Probe 'bytebuddy' }

# Summary.
function Read-Rc {
    param([string]$Path)
    if (Test-Path $Path) { return [int]((Get-Content $Path -Raw).Trim()) } else { return -1 }
}
function Skip-Flag-Yes {
    param([string]$Dir)
    if (Test-Path (Join-Path $Dir 'skipped.flag')) { return 'yes' } else { return 'no' }
}

$summary = [ordered]@{
    generated_at = $Ts
    cratonvm_bin  = $Rustjvm
    cratonvm_rev  = $GitRev
    probes = [ordered]@{
        cglib     = [ordered]@{
            rc                = Read-Rc (Join-Path $Here 'last-run-cglib.rc')
            skipped_real_jar  = Skip-Flag-Yes (Join-Path $Here 'staged-cglib')
        }
        bytebuddy = [ordered]@{
            rc                = Read-Rc (Join-Path $Here 'last-run-bytebuddy.rc')
            skipped_real_jar  = Skip-Flag-Yes (Join-Path $Here 'staged-bytebuddy')
        }
    }
}
Set-Content -Path (Join-Path $Here 'summary.json') -Value (($summary | ConvertTo-Json -Depth 6)) -Encoding utf8
Write-Output "run-under-cratonvm: summary -> $(Join-Path $Here 'summary.json')"
exit 0
