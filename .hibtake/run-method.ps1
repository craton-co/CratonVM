# Runs ONE JUnit method N times and reports the per-run @@RESULT timings.
# Used for fast A/B iteration where a whole-class sweep is too slow.
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Class,
    [Parameter(Mandatory = $true)][string]$Method,
    [Parameter(Mandatory = $true)][string]$Label,
    [string]$Runtime = 'craton',
    [string]$Exe = 'C:\craton\CratonVM-hibfive-takeover-20260730\target-hibfive-takeover\release\cratonvm.exe',
    [hashtable]$EnvVars = @{},
    [int]$Repeat = 3,
    [int]$TimeoutSec = 300,
    [string]$Heap = '-Xmx2g',
    [string]$OutRoot = 'C:\craton\CratonVM-hibfive-takeover-20260730\.hibtake\runs'
)

$ErrorActionPreference = 'Stop'

function Kill-Tree([int]$ProcId) {
    & taskkill /PID $ProcId /T /F 2>&1 | Out-Null
    for ($i = 0; $i -lt 60; $i++) {
        if (-not (Get-Process -Id $ProcId -ErrorAction SilentlyContinue)) { return $true }
        Start-Sleep -Milliseconds 500
    }
    Write-Warning "PID $ProcId survived taskkill /T /F"
    return $false
}

$fixture = 'C:\craton\CratonVM\apps\hib-suite-runner'
$commonArgs = Join-Path $fixture 'common.args'
$hotspotJava = (Get-ChildItem 'C:\Program Files\Eclipse Adoptium' -Directory -ErrorAction SilentlyContinue |
    Where-Object { Test-Path (Join-Path $_.FullName 'bin\java.exe') } |
    Select-Object -First 1 | ForEach-Object { Join-Path $_.FullName 'bin\java.exe' })

$runDir = Join-Path $OutRoot "m-$Label"
New-Item -ItemType Directory -Force $runDir | Out-Null
foreach ($k in $EnvVars.Keys) { Set-Item -Path "Env:$k" -Value ([string]$EnvVars[$k]) }
(($EnvVars.Keys | ForEach-Object { "$_=$($EnvVars[$_])" }) -join "`n") |
    Out-File (Join-Path $runDir '_env.txt') -Encoding utf8

switch ($Runtime) {
    'hotspot'      { $exePath = $hotspotJava; $pre = @($Heap) }
    'craton-nojit' { $exePath = $Exe;         $pre = @($Heap, '--nojit') }
    default        { $exePath = $Exe;         $pre = @($Heap) }
}

$rows = @()
for ($r = 1; $r -le $Repeat; $r++) {
    $stray = @(Get-Process -Name 'cratonvm' -ErrorAction SilentlyContinue)
    foreach ($p in $stray) { Write-Warning "stray cratonvm $($p.Id)"; Kill-Tree $p.Id }

    $outFile = Join-Path $runDir "run$r.stdout.log"
    $errFile = Join-Path $runDir "run$r.stderr.log"
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $proc = Start-Process -FilePath $exePath `
        -ArgumentList ($pre + @("@$commonArgs", 'MethodRunner', $Class, $Method)) `
        -RedirectStandardOutput $outFile -RedirectStandardError $errFile -NoNewWindow -PassThru
    $done = $proc.WaitForExit($TimeoutSec * 1000)
    if (-not $done) { Kill-Tree $proc.Id; $sw.Stop() }
    else { $sw.Stop() }

    $line = (Select-String -Path $outFile -Pattern '^@@RESULT' -ErrorAction SilentlyContinue |
             Select-Object -Last 1).Line
    $testMs = if ($line -match 'test_ms=(-?\d+)') { [int]$Matches[1] } else { -1 }
    $ok     = if ($line -match ' ok=(\d+)') { [int]$Matches[1] } else { -1 }
    $failed = if ($line -match ' failed=(\d+)') { [int]$Matches[1] } else { -1 }
    $rows += [pscustomobject]@{
        Run = $r; Ok = $ok; Failed = $failed
        TestMs = $testMs; WallSec = [math]::Round($sw.Elapsed.TotalSeconds, 1)
    }
    "run $r : ok=$ok failed=$failed test_ms=$testMs wall=$([math]::Round($sw.Elapsed.TotalSeconds,1))s" |
        Write-Output
}

$rows | Export-Csv (Join-Path $runDir '_runs.csv') -NoTypeInformation
$good = $rows | Where-Object { $_.TestMs -gt 0 }
if ($good) {
    $med = ($good | Sort-Object TestMs)[[int]([math]::Floor($good.Count / 2))].TestMs
    Write-Output ""
    Write-Output "=== $Label ($Runtime) : median test_ms=$med over $($good.Count) run(s) ==="
}
