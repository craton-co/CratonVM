# Runs hibernate-orm test classes on a chosen runtime and records @@RESULT lines.
# Fixture (classpath, runner classes) always comes from the MAIN worktree; only
# the cratonvm binary comes from this worktree.
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string[]]$Classes,
    [Parameter(Mandatory = $true)][string]$Label,
    # 'craton' | 'craton-nojit' | 'hotspot'
    [string]$Runtime = 'craton',
    [string]$Exe = 'C:\craton\CratonVM-hibfive-takeover-20260730\target-hibfive-takeover\release\cratonvm.exe',
    [hashtable]$EnvVars = @{},
    [int]$TimeoutSec = 900,
    [string]$Heap = '-Xmx2g',
    [string]$OutRoot = 'C:\craton\CratonVM-hibfive-takeover-20260730\.hibtake\runs'
)

$ErrorActionPreference = 'Stop'

# Terminate a process tree and BLOCK until it is really gone. Verified, because
# a silently-failed kill is indistinguishable from a clean one in the logs and
# poisons every subsequent timing on this host.
function Kill-Tree([int]$ProcId) {
    # `2>&1 |` turns taskkill's stderr into NativeCommandError records, which
    # under `$ErrorActionPreference='Stop'` abort the whole run — and taskkill
    # writes to stderr for the perfectly normal "process already exited" case.
    # Swallow it explicitly instead.
    try { & taskkill /PID $ProcId /T /F *> $null } catch {}
    for ($i = 0; $i -lt 60; $i++) {
        if (-not (Get-Process -Id $ProcId -ErrorAction SilentlyContinue)) { return $true }
        Start-Sleep -Milliseconds 500
    }
    Write-Warning "PID $ProcId survived taskkill /T /F after 30 s"
    return $false
}

# Strays belonging to THIS harness only — matched on the executable path, never
# on the process name. Other sessions on this shared box run their own
# `cratonvm.exe` out of their own worktrees; killing by name reaps their work
# and silently corrupts their measurements as well as ours.
function Get-OwnStrays([string]$ExePath) {
    $full = try { [System.IO.Path]::GetFullPath($ExePath) } catch { $ExePath }
    Get-CimInstance Win32_Process -Filter "Name='cratonvm.exe'" -ErrorAction SilentlyContinue |
        Where-Object { $_.ExecutablePath -and ($_.ExecutablePath -ieq $full) } |
        ForEach-Object { $_.ProcessId }
}

$fixture = 'C:\craton\CratonVM\apps\hib-suite-runner'
$commonArgs = Join-Path $fixture 'common.args'
$hotspotJava = 'C:\Program Files\Eclipse Adoptium\jdk-25.0.0.36-hotspot\bin\java.exe'
if (-not (Test-Path $hotspotJava)) {
    $cand = Get-ChildItem 'C:\Program Files\Eclipse Adoptium' -Directory -ErrorAction SilentlyContinue |
        Where-Object { Test-Path (Join-Path $_.FullName 'bin\java.exe') } | Select-Object -First 1
    if ($cand) { $hotspotJava = Join-Path $cand.FullName 'bin\java.exe' }
}

$runDir = Join-Path $OutRoot $Label
New-Item -ItemType Directory -Force $runDir | Out-Null

# Apply env vars for the child processes (and record them for the report).
$applied = @()
foreach ($k in $EnvVars.Keys) {
    Set-Item -Path "Env:$k" -Value ([string]$EnvVars[$k])
    $applied += "$k=$($EnvVars[$k])"
}
($applied -join "`n") | Out-File -FilePath (Join-Path $runDir '_env.txt') -Encoding utf8

$summary = @()
foreach ($cls in $Classes) {
    $short = $cls.Split('.')[-1]
    $outFile = Join-Path $runDir "$short.stdout.log"
    $errFile = Join-Path $runDir "$short.stderr.log"

    switch ($Runtime) {
        'hotspot'      { $exePath = $hotspotJava; $pre = @($Heap) }
        'craton-nojit' { $exePath = $Exe;         $pre = @($Heap, '--nojit') }
        default        { $exePath = $Exe;         $pre = @($Heap) }
    }
    $argList = $pre + @("@$commonArgs", 'CratonRunner', $cls)

    # Refuse to start while a previous run's VM is still alive — an ineffective
    # kill otherwise overlaps two JVMs and silently inflates the next class.
    # Scoped to our own executable path (see Get-OwnStrays).
    $exePathForStrays = $Exe
    $stray = @(Get-OwnStrays $exePathForStrays)
    if ($stray.Count -gt 0) {
        Write-Warning "killing $($stray.Count) stray process(es) from THIS harness before $short"
        foreach ($p in $stray) { Kill-Tree $p }
    }

    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $proc = Start-Process -FilePath $exePath -ArgumentList $argList `
        -RedirectStandardOutput $outFile -RedirectStandardError $errFile `
        -NoNewWindow -PassThru
    $done = $proc.WaitForExit($TimeoutSec * 1000)
    if (-not $done) {
        # Kill the whole tree, then block until it is really gone.
        # NB: `$proc.Kill($true)` is a .NET Core overload that does NOT exist on
        # Windows PowerShell 5.1 — it threw MethodNotFound into a swallowing
        # catch, so timed-out VMs kept running for HOURS and stole CPU from
        # every later measurement. Use taskkill /T /F and verify.
        Kill-Tree $proc.Id
        $sw.Stop()
        $status = 'TIMEOUT'
        $line = ''
        $exit = -1
    }
    else {
        $sw.Stop()
        $exit = $proc.ExitCode
        $line = (Select-String -Path $outFile -Pattern '^@@RESULT' -ErrorAction SilentlyContinue |
                 Select-Object -Last 1).Line
        if (-not $line) { $status = 'NORESULT' }
        else {
            # `aborted` is a legitimate outcome (JUnit `Assumptions.abort`, e.g.
            # dialect gating) and HotSpot reports the SAME aborted counts here —
            # so it must not be scored as a partial run. PASS == nothing failed
            # and every started test reached a terminal state.
            $started = [int]([regex]::Match($line, 'started=(\d+)').Groups[1].Value)
            $ok      = [int]([regex]::Match($line, ' ok=(\d+)').Groups[1].Value)
            $failed  = [int]([regex]::Match($line, 'failed=(\d+)').Groups[1].Value)
            $aborted = [int]([regex]::Match($line, 'aborted=(\d+)').Groups[1].Value)
            $status  = if ($failed -eq 0 -and $started -gt 0 -and $started -eq ($ok + $aborted)) { 'PASS' }
                       elseif ($failed -gt 0) { 'FAIL' }
                       else { 'PARTIAL' }
        }
    }

    $rec = [pscustomobject]@{
        Class    = $short
        Status   = $status
        WallSec  = [math]::Round($sw.Elapsed.TotalSeconds, 1)
        Exit     = $exit
        Result   = $line
    }
    $summary += $rec
    "{0,-42} {1,-9} {2,8}s exit={3}" -f $short, $status, $rec.WallSec, $exit | Write-Output
    if ($line) { "    $line" | Write-Output }
}

$summary | Export-Csv -Path (Join-Path $runDir '_summary.csv') -NoTypeInformation
Write-Output ""
Write-Output "=== $Label ($Runtime) ==="
$summary | Format-Table -AutoSize | Out-String | Write-Output
