# bench/wave2-4/run-under-rustjvm.ps1
# WP2.4-D - Windows PowerShell 5.1 runner for the WP2.4 instrument /
# jacoco / mockito probes. Mirrors run-under-rustjvm.sh.
#
# Pre-conditions: stage-{instrument,jacoco,mockito}-probe.ps1 ran.
#
# Env:
#   RUSTJVM_BIN   override path to rustjvm.exe.
#   TIMEOUT_SEC   per-probe timeout in seconds (default 60).
#   PROBE         "instrument" | "jacoco" | "mockito" | "all" (default all).

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$Here     = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot = Resolve-Path (Join-Path $Here '..\..')

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
    $jacocoExec = Join-Path $Here ("last-run-" + $Probe + ".jacoco.exec")

    Set-Content -Path $stdoutLog -Value '' -Encoding utf8
    Set-Content -Path $stderrLog -Value '' -Encoding utf8
    if (Test-Path $jacocoExec) { Remove-Item $jacocoExec -Force }

    if (-not (Test-Path $classesDir) -or -not (Test-Path $mainFile)) {
        Write-Output "run-under-rustjvm: probe=$Probe NOT-STAGED (run stage-$Probe-probe.ps1 first)"
        Set-Content -Path $rcFile -Value '127' -Encoding utf8
        $meta = [ordered]@{
            probe        = $Probe
            generated_at = $Ts
            rustjvm_bin  = $Rustjvm
            rustjvm_rev  = $GitRev
            main_class   = ''
            classpath    = ''
            javaagent    = ''
            rc           = 127
            status       = 'not-staged'
            skipped_real_jar = 'no'
            timeout_sec  = $TimeoutSec
        }
        Set-Content -Path $metaFile -Value (($meta | ConvertTo-Json -Depth 4)) -Encoding utf8
        return
    }

    $mainClass = (Get-Content -Path $mainFile -Raw).Trim()
    $skipped   = if (Test-Path $skipFlag) { 'yes' } else { 'no' }

    if ($skipped -eq 'yes') {
        Write-Output "run-under-rustjvm: probe=$Probe SKIPPED (jar absent at stage time)"
        Set-Content -Path $rcFile -Value '-2' -Encoding utf8
        $meta = [ordered]@{
            probe        = $Probe
            generated_at = $Ts
            rustjvm_bin  = $Rustjvm
            rustjvm_rev  = $GitRev
            main_class   = $mainClass
            classpath    = ''
            javaagent    = ''
            rc           = -2
            status       = 'skipped'
            skipped_real_jar = 'yes'
            timeout_sec  = $TimeoutSec
        }
        Set-Content -Path $metaFile -Value (($meta | ConvertTo-Json -Depth 4)) -Encoding utf8
        return
    }

    # Build classpath + javaagent per probe.
    $cpParts = @($classesDir)
    $javaagentArg = ''
    switch ($Probe) {
        'instrument' {
            $javaagentArg = '-javaagent:' + (Join-Path $stagedDir 'agent.jar')
        }
        'jacoco' {
            $destfile = $jacocoExec
            $javaagentArg = '-javaagent:' + (Join-Path $stagedDir 'jacocoagent.jar') + '=destfile=' + $destfile
        }
        'mockito' {
            foreach ($jar in 'mockito-core.jar','byte-buddy.jar','byte-buddy-agent.jar','objenesis.jar') {
                $jp = Join-Path $stagedDir $jar
                if (Test-Path $jp) { $cpParts += $jp }
            }
            $javaagentArg = '-javaagent:' + (Join-Path $stagedDir 'byte-buddy-agent.jar')
        }
    }
    $cp = ($cpParts -join ';')

    Write-Output "run-under-rustjvm: probe=$Probe binary=$Rustjvm"
    Write-Output "run-under-rustjvm: probe=$Probe javaagent=$javaagentArg"
    Write-Output "run-under-rustjvm: probe=$Probe cp=$cp"
    Write-Output "run-under-rustjvm: probe=$Probe main=$mainClass"
    Write-Output "run-under-rustjvm: probe=$Probe timeout=${TimeoutSec}s"

    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $Rustjvm
    # Argument layout: <javaagent> -c <cp> <main>. Quote each cp segment.
    $argString = ('"{0}" -c "{1}" {2}' -f $javaagentArg, $cp, $mainClass)
    $psi.Arguments = $argString
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

    # Probe-specific post-run sanity (informational only).
    $jacocoPresent = $false
    $jacocoSize    = 0
    $jacocoMagicOk = $false
    if ($Probe -eq 'jacoco') {
        if (Test-Path $jacocoExec) {
            $jacocoPresent = $true
            $jacocoSize    = (Get-Item $jacocoExec).Length
            if ($jacocoSize -gt 3) {
                $bytes = [System.IO.File]::ReadAllBytes($jacocoExec)
                if ($bytes.Length -ge 3 -and $bytes[0] -eq 0x01 -and $bytes[1] -eq 0xC0 -and $bytes[2] -eq 0xC0) {
                    $jacocoMagicOk = $true
                }
            }
        }
    }

    $meta = [ordered]@{
        probe                = $Probe
        generated_at         = $Ts
        rustjvm_bin          = $Rustjvm
        rustjvm_rev          = $GitRev
        main_class           = $mainClass
        classpath            = $cp
        javaagent            = $javaagentArg
        rc                   = $rc
        status               = 'ran'
        skipped_real_jar     = $skipped
        timeout_sec          = $TimeoutSec
        jacoco_exec_present  = $jacocoPresent
        jacoco_exec_size     = $jacocoSize
        jacoco_magic_ok      = $jacocoMagicOk
    }
    Set-Content -Path $metaFile -Value (($meta | ConvertTo-Json -Depth 6)) -Encoding utf8
    Write-Output "run-under-rustjvm: probe=$Probe rc=$rc"
}

$ProbeFilter = if ($env:PROBE) { $env:PROBE } else { 'all' }
if ($ProbeFilter -ne 'instrument' -and $ProbeFilter -ne 'jacoco' -and $ProbeFilter -ne 'mockito' -and $ProbeFilter -ne 'all') {
    Write-Error "run-under-rustjvm: ERROR PROBE must be instrument | jacoco | mockito | all"
    exit 4
}
foreach ($p in 'instrument','jacoco','mockito') {
    if ($ProbeFilter -eq 'all' -or $ProbeFilter -eq $p) { Run-Probe -Probe $p }
}

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
    rustjvm_bin  = $Rustjvm
    rustjvm_rev  = $GitRev
    probes = [ordered]@{
        instrument = [ordered]@{
            rc                = Read-Rc (Join-Path $Here 'last-run-instrument.rc')
            skipped_real_jar  = Skip-Flag-Yes (Join-Path $Here 'staged-instrument')
        }
        jacoco     = [ordered]@{
            rc                = Read-Rc (Join-Path $Here 'last-run-jacoco.rc')
            skipped_real_jar  = Skip-Flag-Yes (Join-Path $Here 'staged-jacoco')
        }
        mockito    = [ordered]@{
            rc                = Read-Rc (Join-Path $Here 'last-run-mockito.rc')
            skipped_real_jar  = Skip-Flag-Yes (Join-Path $Here 'staged-mockito')
        }
    }
}
Set-Content -Path (Join-Path $Here 'summary.json') -Value (($summary | ConvertTo-Json -Depth 6)) -Encoding utf8
Write-Output "run-under-rustjvm: summary -> $(Join-Path $Here 'summary.json')"
exit 0
