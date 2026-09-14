<#
.SYNOPSIS
    CratonBench reliability gate (Windows twin of reliability-gate.sh).

.DESCRIPTION
    Decides whether a benchmark run is allowed to count as evidence at all.
    Identical check IDs, identical reason-string format and identical exit
    codes to the shell version, so a result directory produced on either
    platform can be judged by either script and compare.py needs to know
    only one contract.

    Checks (all of them refuse the run - non-zero exit, explicit reason):
      * checksum drift between runs, or against the baseline's reference
        checksum;
      * a placeholder / zero / missing baseline being used as a regression
        threshold;
      * fewer samples than the protocol requires;
      * host load above the ceiling at the start of the run or at any point
        during it. On Windows the load source is the '\System\Processor Queue
        Length' performance counter, NOT a Unix load average: it counts
        threads waiting for a processor, so the same numeric ceiling means
        "nothing else is queued for a core", which is the property the gate
        actually needs;
      * CPU migration, a pin that is not a single CPU, thermal throttling,
        excessive core-frequency drift, or run-to-run variance (CV) above the
        ceiling;
      * a missing or incomplete environment manifest.

.PARAMETER Mode
    preflight | postflight | check-baseline

.EXAMPLE
    powershell -File reliability-gate.ps1 -Mode postflight -Results .\results\v1\<run-id> -Baseline .\cratonbench-baseline-azure-epyc.tsv

.NOTES
    Exit codes (same as reliability-gate.sh):
       0  every check passed
       2  usage / setup error
       3  host too loaded or contended to measure
      10  checksum drift / checksum != reference / a run that did not complete
      11  placeholder, zero or missing baseline used as a threshold
      12  too few samples
      13  measurement instability (pin, migration, thermal, frequency, CV)
      14  missing or incomplete environment manifest

    See docs/benchmarking/reliability-gate.md.
#>

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet('preflight', 'postflight', 'check-baseline')]
    [string]$Mode,

    [string]$Results = '',
    [string]$Baseline = '',
    [string]$Phases = '',
    [double]$MaxLoad = 2.0,
    [int]$MinSamples = 7,
    [double]$MaxCv = 5.0,
    [double]$MaxFreqDrift = 20.0,
    [string]$Cpu = '',
    [int]$Reps = 0,
    [switch]$RequireFreqData,
    [switch]$Calibrate
)

$ErrorActionPreference = 'Stop'
$SchemaVersion = 1

$script:FailCount = 0
$script:WarnCount = 0
$script:ExitCode = 0
$script:Checks = New-Object System.Collections.ArrayList

# Manifest keys that must be present and non-empty for a run to be citable.
# '-' counts as absent: a runner writes '-' for anything it could not read,
# and a field it could not read is exactly the field a later reader would
# otherwise assume had been checked.
$RequiredManifestKeys = @(
    'schema_version', 'run_id', 'created_utc', 'host', 'cpu_model', 'cpu_pinned',
    'revision', 'binary_path', 'binary_sha256', 'vm_flags', 'command_line',
    'jdk_version', 'baseline_file', 'reps', 'load1_start'
)

# Numbers are parsed and formatted with the INVARIANT culture, never the
# host's. On a ru-RU (or de-DE, fr-FR, ...) Windows box the decimal separator
# is a comma, so [double]::TryParse('9.9') returns $false and every check that
# guards itself with TryParse silently stops checking - the load ceiling, the
# frequency-drift ceiling and the CV ceiling all quietly pass. A gate that
# turns itself off on someone's laptop locale is worse than no gate, because
# it still prints PASS.
function Convert-ToDouble {
    param([string]$Text, [ref]$Value)
    return [double]::TryParse(
        $Text,
        [System.Globalization.NumberStyles]::Float,
        [System.Globalization.CultureInfo]::InvariantCulture,
        $Value)
}

function Convert-ToInt {
    param([string]$Text, [ref]$Value)
    return [int]::TryParse(
        $Text,
        [System.Globalization.NumberStyles]::Integer,
        [System.Globalization.CultureInfo]::InvariantCulture,
        $Value)
}

function Format-Num {
    param([double]$Value, [string]$Format = '0.##')
    return $Value.ToString($Format, [System.Globalization.CultureInfo]::InvariantCulture)
}

function Add-Check {
    param([string]$Status, [string]$Id, [string]$Detail)
    [void]$script:Checks.Add([pscustomobject]@{ id = $Id; status = $Status; detail = $Detail })
}

function Pass-Check {
    param([string]$Id, [string]$Detail = 'ok')
    Add-Check -Status 'PASS' -Id $Id -Detail $Detail
}

function Warn-Check {
    param([string]$Id, [string]$Detail)
    $script:WarnCount++
    [Console]::Error.WriteLine("RELIABILITY-WARN[$Id]: $Detail")
    Add-Check -Status 'WARN' -Id $Id -Detail $Detail
}

function Fail-Check {
    param([int]$Code, [string]$Id, [string]$Detail)
    $script:FailCount++
    if ($script:ExitCode -eq 0) { $script:ExitCode = $Code }
    [Console]::Error.WriteLine("RELIABILITY-FAIL[$Id]: $Detail")
    Add-Check -Status 'FAIL' -Id $Id -Detail $Detail
}

function Test-WantedPhase {
    param([string]$Phase)
    if ([string]::IsNullOrEmpty($Phases)) { return $true }
    return (",$Phases," -like "*,$Phase,*")
}

# ---------------------------------------------------------------------------
# Manifest
# ---------------------------------------------------------------------------
$script:Manifest = @{}

function Read-Manifest {
    $path = Join-Path $Results 'manifest.tsv'
    if (-not (Test-Path -LiteralPath $path)) {
        Fail-Check -Code 14 -Id 'MANIFEST-MISSING' -Detail "no environment manifest at $path - a run with no recorded revision, binary hash, flags, JDK and CPU model cannot be cited or reproduced"
        return $false
    }
    foreach ($line in (Get-Content -LiteralPath $path)) {
        if ($line -match '^\s*#') { continue }
        $parts = $line -split "`t", 2
        if ($parts.Count -lt 2) { continue }
        if (-not $script:Manifest.ContainsKey($parts[0])) { $script:Manifest[$parts[0]] = $parts[1] }
    }
    $missing = @()
    foreach ($key in $RequiredManifestKeys) {
        $v = $null
        if ($script:Manifest.ContainsKey($key)) { $v = $script:Manifest[$key] }
        if ([string]::IsNullOrWhiteSpace($v) -or $v -eq '-' -or $v -eq 'unknown') { $missing += $key }
    }
    if ($missing.Count -gt 0) {
        Fail-Check -Code 14 -Id 'MANIFEST-FIELD' -Detail ("environment manifest is incomplete - missing or unrecorded: " + ($missing -join ' ') + " (manifest: $path)")
        return $false
    }
    Pass-Check -Id 'MANIFEST-FIELD' -Detail ("all " + $RequiredManifestKeys.Count + " required manifest fields recorded")
    return $true
}

function Get-ManifestValue {
    param([string]$Key)
    if ($script:Manifest.ContainsKey($Key)) { return $script:Manifest[$Key] }
    return ''
}

# ---------------------------------------------------------------------------
# Baseline
# ---------------------------------------------------------------------------
# Two dialects, exactly as in reliability-gate.sh:
#   TSV  - phase / baseline_ms / checksum / status / evidence. A row is a
#          placeholder when baseline_ms is 0, empty or non-numeric, when the
#          checksum column is empty, or when status is 'placeholder'.
#   JSON - {"phases": {"<phase>": {"baseline_ms": N, "checksum": "...",
#          "status": "...", "placeholder": true}}}. See baselines/README.md.
function Get-BaselineRows {
    param([string]$Path)
    $rows = @()
    if ($Path -like '*.json') {
        $obj = Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json
        if ($null -eq $obj.phases) { return $rows }
        foreach ($p in $obj.phases.PSObject.Properties) {
            $ph = $false
            if ($null -ne $p.Value.placeholder) { $ph = [bool]$p.Value.placeholder }
            $rows += [pscustomobject]@{
                phase       = $p.Name
                ms          = "$($p.Value.baseline_ms)"
                checksum    = "$($p.Value.checksum)"
                status      = "$($p.Value.status)"
                placeholder = $ph
            }
        }
        return $rows
    }
    foreach ($line in (Get-Content -LiteralPath $Path)) {
        if ($line -match '^\s*#' -or [string]::IsNullOrWhiteSpace($line)) { continue }
        $f = $line -split "`t"
        if ($f.Count -lt 3) { continue }
        $status = ''
        if ($f.Count -ge 4) { $status = $f[3] }
        $rows += [pscustomobject]@{
            phase       = $f[0]
            ms          = $f[1]
            checksum    = $f[2]
            status      = $status
            placeholder = ($status -eq 'placeholder' -or $status -like '*placeholder=true*')
        }
    }
    return $rows
}

function Test-Baseline {
    if ([string]::IsNullOrEmpty($Baseline)) {
        Fail-Check -Code 11 -Id 'BASELINE-MISSING' -Detail 'no -Baseline given; a regression verdict without a baseline is not a verdict'
        return $false
    }
    if (-not (Test-Path -LiteralPath $Baseline)) {
        Fail-Check -Code 11 -Id 'BASELINE-MISSING' -Detail "baseline file not found: $Baseline"
        return $false
    }
    $rows = Get-BaselineRows -Path $Baseline
    if ($rows.Count -eq 0) {
        Fail-Check -Code 11 -Id 'BASELINE-MISSING' -Detail "baseline $Baseline contains no phase rows"
        return $false
    }
    $seen = 0
    $bad = 0
    foreach ($r in $rows) {
        if (-not (Test-WantedPhase -Phase $r.phase)) { continue }
        $seen++
        if ($r.placeholder) {
            Fail-Check -Code 11 -Id 'BASELINE-PLACEHOLDER' -Detail "$($r.phase): baseline is marked placeholder (status='$($r.status)') in $Baseline - a placeholder must never act as a regression threshold; record a real one per docs/benchmarking/methodology.md"
            $bad++
            continue
        }
        $ms = 0.0
        if (-not (Convert-ToDouble -Text $r.ms -Value ([ref]$ms))) {
            Fail-Check -Code 11 -Id 'BASELINE-PLACEHOLDER' -Detail "$($r.phase): baseline_ms is missing or non-numeric ('$($r.ms)') in $Baseline - this is a placeholder in all but name"
            $bad++
            continue
        }
        if ($ms -le 0) {
            Fail-Check -Code 11 -Id 'BASELINE-PLACEHOLDER' -Detail "$($r.phase): baseline_ms is $($r.ms) in $Baseline - a zero/negative baseline makes the budget zero, so the gate stops being able to fail (or fails always) while still reporting a verdict"
            $bad++
            continue
        }
        if ([string]::IsNullOrWhiteSpace($r.checksum) -or $r.checksum -eq '-') {
            Fail-Check -Code 11 -Id 'BASELINE-CHECKSUM' -Detail "$($r.phase): no reference checksum in $Baseline - without one, a run can only prove it agreed with itself"
            $bad++
            continue
        }
    }
    if ($seen -eq 0) {
        $want = $Phases
        if ([string]::IsNullOrEmpty($want)) { $want = '<all>' }
        Fail-Check -Code 11 -Id 'BASELINE-MISSING' -Detail "none of the requested phases ($want) exist in $Baseline"
        return $false
    }
    if ($bad -eq 0) {
        Pass-Check -Id 'BASELINE-PLACEHOLDER' -Detail "$seen phase baseline(s) in $Baseline are real, non-zero and carry a reference checksum"
    }
    return $true
}

# ---------------------------------------------------------------------------
# Host state
# ---------------------------------------------------------------------------
function Get-HostLoad {
    # Processor Queue Length: threads ready to run but waiting for a core.
    # There is no Unix-style load average on Windows, and picking CPU% would
    # have been wrong here - the gate cares whether ANYTHING ELSE wants the
    # core, not how busy the core is with our own benchmark.
    try {
        $s = Get-Counter '\System\Processor Queue Length' -ErrorAction Stop
        return [double]$s.CounterSamples[0].CookedValue
    } catch {
        return $null
    }
}

function Test-HostLoadNow {
    $load = Get-HostLoad
    if ($null -eq $load) {
        Warn-Check -Id 'HOST-LOAD' -Detail 'cannot read the processor queue length on this host; the load ceiling is unenforced for the start-of-run check'
        return
    }
    if ($load -gt $MaxLoad) {
        Fail-Check -Code 3 -Id 'HOST-LOAD' -Detail "processor queue length $load exceeds the ceiling $MaxLoad at start of run - a contended host produces garbage passes as well as garbage failures, so this must not measure at all"
        return
    }
    Pass-Check -Id 'HOST-LOAD' -Detail "start-of-run processor queue length $load <= $MaxLoad"
}

function Test-HostContention {
    # BENCHMARK.md's own methodology warning: a concurrent session running its
    # own CratonBench pinned to the same core halves that core while every
    # other core stays idle, which no aggregate load metric can see.
    try {
        $others = @(Get-CimInstance Win32_Process -Filter "Name = 'java.exe' OR Name = 'cratonvm.exe'" -ErrorAction Stop |
            Where-Object { $_.CommandLine -like '*CratonBench*' -and $_.ProcessId -ne $PID })
    } catch {
        Warn-Check -Id 'HOST-CONTENTION' -Detail 'cannot enumerate processes; competing CratonBench runs are undetectable here'
        return
    }
    if ($others.Count -gt 0) {
        $pids = ($others | ForEach-Object { $_.ProcessId }) -join ' '
        Fail-Check -Code 3 -Id 'HOST-CONTENTION' -Detail "another CratonBench is already running (pids: $pids) - a co-pinned benchmark halves the core without moving any aggregate load metric"
        return
    }
    Pass-Check -Id 'HOST-CONTENTION' -Detail 'no competing CratonBench process'
}

function Test-Pin {
    param([string]$PinnedCpu)
    $n = 0
    if ([string]::IsNullOrWhiteSpace($PinnedCpu) -or -not (Convert-ToInt -Text $PinnedCpu -Value ([ref]$n))) {
        Fail-Check -Code 13 -Id 'CPU-PIN' -Detail "no single CPU recorded for this run ('$PinnedCpu') - an unpinned run cannot be compared with a pinned baseline"
        return
    }
    Pass-Check -Id 'CPU-PIN' -Detail "pinned to a single logical CPU ($PinnedCpu)"
}

# ---------------------------------------------------------------------------
# Samples
# ---------------------------------------------------------------------------
$script:SampleHeader = @()
$script:Samples = @()

function Read-Samples {
    $path = Join-Path $Results 'samples.tsv'
    if (-not (Test-Path -LiteralPath $path)) {
        Fail-Check -Code 14 -Id 'MANIFEST-MISSING' -Detail "no raw sample file at $path - a results directory that kept only the summary cannot be re-analysed, and 'we only kept the median' is how the retracted HashMap number survived"
        return $false
    }
    $lines = Get-Content -LiteralPath $path
    foreach ($line in $lines) {
        if ($line -match '^\s*#') {
            if ($script:SampleHeader.Count -eq 0) {
                $script:SampleHeader = ($line -replace '^#', '') -split "`t"
            }
            continue
        }
        if ([string]::IsNullOrWhiteSpace($line)) { continue }
        $f = $line -split "`t"
        if ($f.Count -lt 3) { continue }
        $row = @{}
        for ($i = 0; $i -lt $script:SampleHeader.Count; $i++) {
            $v = ''
            if ($i -lt $f.Count) { $v = $f[$i] }
            $row[$script:SampleHeader[$i]] = $v
        }
        $script:Samples += , $row
    }
    if ($script:SampleHeader.Count -eq 0) {
        Fail-Check -Code 14 -Id 'SUMMARY-INTEGRITY' -Detail "samples.tsv in $Results has no '#'-prefixed header row, so its columns cannot be identified"
        return $false
    }
    return $true
}

function Get-SamplePhases {
    return ($script:Samples | ForEach-Object { $_['phase'] } | Select-Object -Unique |
        Where-Object { Test-WantedPhase -Phase $_ })
}

function Get-PhaseSamples {
    param([string]$Phase)
    return @($script:Samples | Where-Object { $_['phase'] -eq $Phase })
}

function Test-SampleCounts {
    $phases = @(Get-SamplePhases)
    if ($phases.Count -eq 0) {
        $want = $Phases
        if ([string]::IsNullOrEmpty($want)) { $want = '<all>' }
        Fail-Check -Code 12 -Id 'SAMPLE-COUNT' -Detail "no samples recorded for the requested phases ($want)"
        return
    }
    $bad = 0
    foreach ($p in $phases) {
        $n = (Get-PhaseSamples -Phase $p).Count
        if ($n -lt $MinSamples) {
            Fail-Check -Code 12 -Id 'SAMPLE-COUNT' -Detail "${p}: $n sample(s) recorded, protocol requires >= $MinSamples - a median of fewer runs is not a median, it is a draw"
            $bad++
        }
    }
    if ($bad -eq 0) { Pass-Check -Id 'SAMPLE-COUNT' -Detail "every measured phase has >= $MinSamples samples" }
}

function Test-ExitCodes {
    if (-not ($script:SampleHeader -contains 'exit_code')) {
        Warn-Check -Id 'SAMPLE-EXIT' -Detail 'samples.tsv has no exit_code column; per-run completion is unverified'
        return
    }
    $bad = @()
    foreach ($s in $script:Samples) {
        if (-not (Test-WantedPhase -Phase $s['phase'])) { continue }
        if ($s['exit_code'] -ne '0') { $bad += "$($s['phase'])(rep $($s['rep']), exit $($s['exit_code']))" }
    }
    if ($bad.Count -gt 0) {
        Fail-Check -Code 10 -Id 'SAMPLE-EXIT' -Detail ("run(s) did not complete cleanly: " + ($bad -join ' ') + " - a crashed or timed-out run must never contribute a time")
        return
    }
    Pass-Check -Id 'SAMPLE-EXIT' -Detail 'every run exited 0'
}

function Test-Checksums {
    if (-not ($script:SampleHeader -contains 'checksum')) {
        Fail-Check -Code 10 -Id 'CHECKSUM-DRIFT' -Detail 'samples.tsv has no checksum column - an unchecksummed benchmark measures how fast the wrong answer is produced'
        return
    }
    $refs = @{}
    if (-not [string]::IsNullOrEmpty($Baseline) -and (Test-Path -LiteralPath $Baseline)) {
        foreach ($r in (Get-BaselineRows -Path $Baseline)) { $refs[$r.phase] = $r.checksum }
    }
    $bad = 0
    foreach ($p in (Get-SamplePhases)) {
        $distinct = @((Get-PhaseSamples -Phase $p) | ForEach-Object { $_['checksum'] } | Select-Object -Unique)
        if ($distinct.Count -gt 1) {
            Fail-Check -Code 10 -Id 'CHECKSUM-DRIFT' -Detail ("${p}: runs disagreed with each other - checksums seen: " + ($distinct -join ' ') + " - this is a CORRECTNESS regression, not a perf result")
            $bad++
            continue
        }
        if (-not $refs.ContainsKey($p) -or [string]::IsNullOrWhiteSpace($refs[$p]) -or $refs[$p] -eq '-') {
            $b = $Baseline
            if ([string]::IsNullOrEmpty($b)) { $b = '<no baseline>' }
            Fail-Check -Code 11 -Id 'BASELINE-CHECKSUM' -Detail "${p}: no reference checksum recorded in $b to compare $($distinct[0]) against"
            $bad++
            continue
        }
        if ($distinct[0] -ne $refs[$p]) {
            Fail-Check -Code 10 -Id 'CHECKSUM-REFERENCE' -Detail "${p}: checksum $($distinct[0]) != reference $($refs[$p]) - a faster wrong answer is a bug, not a result"
            $bad++
        }
    }
    if ($bad -eq 0) { Pass-Check -Id 'CHECKSUM-REFERENCE' -Detail 'every run of every phase matched its recorded reference checksum' }
}

function Test-LoadDuringRun {
    if (-not ($script:SampleHeader -contains 'load1')) {
        Warn-Check -Id 'HOST-LOAD' -Detail 'samples.tsv has no load1 column; load during the run is unverified (only the opening reading was checked)'
        return
    }
    $over = @()
    foreach ($s in $script:Samples) {
        if (-not (Test-WantedPhase -Phase $s['phase'])) { continue }
        $v = 0.0
        if ($s['load1'] -ne '-' -and (Convert-ToDouble -Text $s['load1'] -Value ([ref]$v)) -and $v -gt $MaxLoad) {
            $over += "$($s['phase'])(rep $($s['rep']): $($s['load1']))"
        }
    }
    if ($over.Count -gt 0) {
        Fail-Check -Code 3 -Id 'HOST-LOAD' -Detail ("load rose above $MaxLoad during the run: " + ($over -join ' ') + " - the opening reading passing is not evidence the whole run was quiet")
        return
    }
    Pass-Check -Id 'HOST-LOAD' -Detail "every per-sample load reading <= $MaxLoad"
}

function Test-CpuStability {
    $pinned = Get-ManifestValue -Key 'cpu_pinned'
    Test-Pin -PinnedCpu $pinned

    if (-not ($script:SampleHeader -contains 'cpu_observed')) {
        Fail-Check -Code 13 -Id 'CPU-MIGRATION' -Detail 'samples.tsv has no cpu_observed column - CPU migration is undetectable, and a migrated run silently measures a different core'
    } else {
        $moved = @()
        $unobserved = 0
        foreach ($s in $script:Samples) {
            if (-not (Test-WantedPhase -Phase $s['phase'])) { continue }
            $o = $s['cpu_observed']
            if ($o -eq '-') { $unobserved++; continue }
            if ($o -like '*,*' -or $o -ne $pinned) { $moved += "$($s['phase'])(rep $($s['rep']) ran on $o)" }
        }
        $mask = Get-ManifestValue -Key 'cpu_affinity_mask'
        if ($moved.Count -gt 0) {
            Fail-Check -Code 13 -Id 'CPU-MIGRATION' -Detail ("process did not stay on the pinned CPU ${pinned}: " + ($moved -join ' ') + " - cross-core migration changes cache and frequency behaviour mid-measurement")
        } elseif ($unobserved -gt 0) {
            if (-not [string]::IsNullOrWhiteSpace($mask) -and $mask -eq $pinned) {
                Warn-Check -Id 'CPU-MIGRATION' -Detail "$unobserved sample(s) had no observed CPU, but the affinity mask was the single CPU $mask, which makes migration impossible"
            } else {
                Fail-Check -Code 13 -Id 'CPU-MIGRATION' -Detail "$unobserved sample(s) recorded no observed CPU and the affinity mask ('$mask') is not the single pinned CPU $pinned - migration cannot be ruled out"
            }
        } else {
            Pass-Check -Id 'CPU-MIGRATION' -Detail "every sample ran on the pinned CPU $pinned"
        }
    }

    if (-not ($script:SampleHeader -contains 'throttle_delta')) {
        Warn-Check -Id 'CPU-THERMAL' -Detail 'samples.tsv has no throttle_delta column; thermal throttling is unverified'
    } else {
        $thr = @()
        foreach ($s in $script:Samples) {
            if (-not (Test-WantedPhase -Phase $s['phase'])) { continue }
            $v = 0
            if ($s['throttle_delta'] -ne '-' -and (Convert-ToInt -Text $s['throttle_delta'] -Value ([ref]$v)) -and $v -gt 0) {
                $thr += "$($s['phase'])(rep $($s['rep']): +$v)"
            }
        }
        if ($thr.Count -gt 0) {
            Fail-Check -Code 13 -Id 'CPU-THERMAL' -Detail ("the pinned core throttled during the run: " + ($thr -join ' ') + " - a thermally-limited core is not the core the baseline was measured on")
        } else {
            Pass-Check -Id 'CPU-THERMAL' -Detail 'no thermal-throttle events on the pinned core'
        }
    }

    if (-not (($script:SampleHeader -contains 'khz_min') -and ($script:SampleHeader -contains 'khz_max'))) {
        if ($RequireFreqData) {
            Fail-Check -Code 13 -Id 'CPU-FREQ' -Detail 'no frequency columns in samples.tsv and -RequireFreqData was given'
        } else {
            Warn-Check -Id 'CPU-FREQ' -Detail 'samples.tsv has no frequency columns; core-frequency stability is unverified (pass -RequireFreqData to make this fatal)'
        }
        return
    }
    $drift = @()
    $unavailable = 0
    foreach ($s in $script:Samples) {
        if (-not (Test-WantedPhase -Phase $s['phase'])) { continue }
        $lo = 0.0; $hi = 0.0
        if ($s['khz_min'] -eq '-' -or $s['khz_max'] -eq '-' -or
            -not (Convert-ToDouble -Text $s['khz_min'] -Value ([ref]$lo)) -or
            -not (Convert-ToDouble -Text $s['khz_max'] -Value ([ref]$hi)) -or $hi -le 0) {
            $unavailable++
            continue
        }
        $d = ($hi - $lo) * 100.0 / $hi
        if ($d -gt $MaxFreqDrift) { $drift += ("$($s['phase'])(rep $($s['rep']): " + (Format-Num -Value $d -Format '0.0') + '%)') }
    }
    if ($drift.Count -gt 0) {
        Fail-Check -Code 13 -Id 'CPU-FREQ' -Detail ("pinned-core frequency moved more than $MaxFreqDrift% within a run: " + ($drift -join ' ') + " - boost/thermal drift of that size is larger than most regressions this gate is asked to detect")
    } elseif ($unavailable -gt 0 -and $RequireFreqData) {
        Fail-Check -Code 13 -Id 'CPU-FREQ' -Detail "$unavailable sample(s) had no readable frequency data and -RequireFreqData was given"
    } elseif ($unavailable -gt 0) {
        Warn-Check -Id 'CPU-FREQ' -Detail "$unavailable sample(s) had no readable frequency data; frequency stability is unverified for those"
    } else {
        Pass-Check -Id 'CPU-FREQ' -Detail "pinned-core frequency spread within $MaxFreqDrift% on every sample"
    }
}

function Get-Percentile {
    param([double[]]$Sorted, [int]$P)
    # Nearest-rank, ceil(P/100 * n) - the same definition the shell gate, the
    # runner and the VM's own G1 pause summary use.
    $n = $Sorted.Count
    if ($n -eq 0) { return 0 }
    $r = [math]::Ceiling($P * $n / 100.0)
    if ($r -lt 1) { $r = 1 }
    return $Sorted[$r - 1]
}

function Test-Variance {
    if (-not ($script:SampleHeader -contains 'ms')) {
        Fail-Check -Code 14 -Id 'SUMMARY-INTEGRITY' -Detail 'samples.tsv has no ms column'
        return
    }
    $bad = @()
    foreach ($p in (Get-SamplePhases)) {
        $vals = @()
        foreach ($s in (Get-PhaseSamples -Phase $p)) {
            $v = 0.0
            if (Convert-ToDouble -Text $s['ms'] -Value ([ref]$v)) { $vals += $v }
        }
        if ($vals.Count -lt 2) { continue }
        $mean = ($vals | Measure-Object -Average).Average
        $sum = 0.0
        foreach ($v in $vals) { $sum += ($v - $mean) * ($v - $mean) }
        $sd = [math]::Sqrt($sum / ($vals.Count - 1))
        $cv = 0.0
        if ($mean -gt 0) { $cv = 100.0 * $sd / $mean }
        if ($cv -gt $MaxCv) {
            $bad += ("$p(CV " + (Format-Num -Value $cv -Format '0.00') + '%, mean ' + (Format-Num -Value $mean -Format '0.0') + " ms, n=$($vals.Count))")
        }
    }
    if ($bad.Count -gt 0) {
        Fail-Check -Code 13 -Id 'RUN-VARIANCE' -Detail ("run-to-run variance exceeds $MaxCv%: " + ($bad -join ' ') + " - that spread is the signature of an unstable host (frequency, co-tenancy, thermal), and a median drawn from it cannot resolve the regression sizes this gate is for")
        return
    }
    Pass-Check -Id 'RUN-VARIANCE' -Detail "every phase's coefficient of variation <= $MaxCv%"
}

function Test-SummaryIntegrity {
    $path = Join-Path $Results 'summary.tsv'
    if (-not (Test-Path -LiteralPath $path)) {
        Fail-Check -Code 14 -Id 'SUMMARY-INTEGRITY' -Detail "no summary.tsv in $Results"
        return
    }
    $header = @()
    $bad = @()
    foreach ($line in (Get-Content -LiteralPath $path)) {
        if ($line -match '^\s*#') {
            if ($header.Count -eq 0) { $header = ($line -replace '^#', '') -split "`t" }
            continue
        }
        if ([string]::IsNullOrWhiteSpace($line)) { continue }
        $f = $line -split "`t"
        if ($f.Count -lt 4) { continue }
        $phase = $f[0]
        if (-not (Test-WantedPhase -Phase $phase)) { continue }
        $rows = @(Get-PhaseSamples -Phase $phase)
        if ($rows.Count -eq 0) { $bad += "$phase(in summary, absent from samples)"; continue }
        $summaryN = 0
        [void](Convert-ToInt -Text $f[1] -Value ([ref]$summaryN))
        if ($summaryN -ne $rows.Count) { $bad += "$phase(summary n=$($f[1]), samples n=$($rows.Count))"; continue }
        $vals = @()
        foreach ($s in $rows) {
            $v = 0.0
            if (Convert-ToDouble -Text $s['ms'] -Value ([ref]$v)) { $vals += $v }
        }
        $sorted = @($vals | Sort-Object)
        $p50 = Get-Percentile -Sorted $sorted -P 50
        $summaryP50 = 0.0
        [void](Convert-ToDouble -Text $f[3] -Value ([ref]$summaryP50))
        if ($summaryP50 -ne $p50) { $bad += "$phase(summary p50=$($f[3]), samples p50=$p50)" }
    }
    if ($bad.Count -gt 0) {
        Fail-Check -Code 14 -Id 'SUMMARY-INTEGRITY' -Detail ("summary.tsv does not match the raw samples: " + ($bad -join ' ') + " - the raw samples are the record; a summary that cannot be rederived from them is not evidence")
        return
    }
    Pass-Check -Id 'SUMMARY-INTEGRITY' -Detail 'summary.tsv rederives exactly from samples.tsv (n and p50)'
}

# ---------------------------------------------------------------------------
# Report
# ---------------------------------------------------------------------------
function Write-Report {
    if ([string]::IsNullOrEmpty($Results) -or -not (Test-Path -LiteralPath $Results)) { return }
    $status = 'pass'
    if ($script:FailCount -gt 0) { $status = 'fail' }

    $tsv = New-Object System.Collections.ArrayList
    [void]$tsv.Add("# CratonBench reliability gate report - schema $SchemaVersion")
    [void]$tsv.Add("# check`tstatus`tdetail")
    [void]$tsv.Add("gate.mode`t$Mode`t-")
    [void]$tsv.Add("gate.status`t$status`texit=$($script:ExitCode) fail=$($script:FailCount) warn=$($script:WarnCount)")
    foreach ($c in $script:Checks) { [void]$tsv.Add("$($c.id)`t$($c.status)`t$($c.detail)") }
    $tsv | Set-Content -LiteralPath (Join-Path $Results "reliability-$Mode.tsv") -Encoding utf8

    $payload = [pscustomobject]@{
        schema_version = $SchemaVersion
        mode           = $Mode
        status         = $status
        exit_code      = $script:ExitCode
        failures       = $script:FailCount
        warnings       = $script:WarnCount
        min_samples    = $MinSamples
        max_load       = (Format-Num -Value $MaxLoad)
        max_cv_pct     = (Format-Num -Value $MaxCv)
        checks         = @($script:Checks)
    }
    $json = $payload | ConvertTo-Json -Depth 5
    Set-Content -LiteralPath (Join-Path $Results "reliability-$Mode.json") -Value $json -Encoding utf8
    # reliability.json always names the LATEST decision, which is what
    # compare.py reads: a run whose preflight passed and whose postflight
    # failed is a failed run.
    Set-Content -LiteralPath (Join-Path $Results 'reliability.json') -Value $json -Encoding utf8
    $tsv | Set-Content -LiteralPath (Join-Path $Results 'reliability.tsv') -Encoding utf8
}

# ---------------------------------------------------------------------------
# Drive
# ---------------------------------------------------------------------------
switch ($Mode) {
    'check-baseline' {
        [void](Test-Baseline)
    }
    'preflight' {
        if ([string]::IsNullOrEmpty($Results) -or -not (Test-Path -LiteralPath $Results)) {
            [Console]::Error.WriteLine('FATAL: -Results DIR required and must exist')
            exit 2
        }
        Write-Output "reliability-gate preflight: results=$Results min-samples=$MinSamples max-load=$MaxLoad"
        [void](Read-Manifest)
        # Checked here, not in postflight, so a run that cannot possibly
        # satisfy the protocol is rejected before it burns an hour of
        # bench-host time.
        if ($Reps -gt 0 -and $Reps -lt $MinSamples) {
            Fail-Check -Code 12 -Id 'SAMPLE-COUNT' -Detail "requested -Reps $Reps is below the required $MinSamples samples per phase; raise -Reps, or lower -MinSamples explicitly and say so in the write-up"
        } else {
            Pass-Check -Id 'SAMPLE-COUNT' -Detail "planned reps $Reps >= required $MinSamples"
        }
        Test-HostLoadNow
        Test-HostContention
        $pin = $Cpu
        if ([string]::IsNullOrEmpty($pin)) { $pin = Get-ManifestValue -Key 'cpu_pinned' }
        Test-Pin -PinnedCpu $pin
        if ($Calibrate) {
            Warn-Check -Id 'BASELINE-PLACEHOLDER' -Detail '-Calibrate: baseline threshold checks skipped because this run is recording a baseline rather than being gated by one'
        } else {
            [void](Test-Baseline)
        }
    }
    'postflight' {
        if ([string]::IsNullOrEmpty($Results) -or -not (Test-Path -LiteralPath $Results)) {
            [Console]::Error.WriteLine('FATAL: -Results DIR required and must exist')
            exit 2
        }
        Write-Output "reliability-gate postflight: results=$Results min-samples=$MinSamples max-load=$MaxLoad max-cv=$MaxCv%"
        [void](Read-Manifest)
        if (Read-Samples) {
            Test-SampleCounts
            Test-ExitCodes
            if ($Calibrate) {
                Warn-Check -Id 'BASELINE-PLACEHOLDER' -Detail '-Calibrate: baseline threshold checks skipped; runs must still agree with each other'
                foreach ($p in (Get-SamplePhases)) {
                    $distinct = @((Get-PhaseSamples -Phase $p) | ForEach-Object { $_['checksum'] } | Select-Object -Unique)
                    if ($distinct.Count -gt 1) {
                        Fail-Check -Code 10 -Id 'CHECKSUM-DRIFT' -Detail ("${p}: runs disagreed with each other - checksums seen: " + ($distinct -join ' '))
                    }
                }
            } else {
                [void](Test-Baseline)
                Test-Checksums
            }
            Test-LoadDuringRun
            Test-CpuStability
            Test-Variance
            Test-SummaryIntegrity
        }
    }
}

Write-Report

Write-Output '---------------------------------------------'
if ($script:FailCount -gt 0) {
    Write-Output "RELIABILITY GATE ($Mode): FAILED - $($script:FailCount) check(s), $($script:WarnCount) warning(s); exit $($script:ExitCode)"
    Write-Output 'This run is NOT usable as evidence. See docs/benchmarking/reliability-gate.md.'
    exit $script:ExitCode
}
Write-Output "RELIABILITY GATE ($Mode): PASS ($($script:WarnCount) warning(s))"
exit 0
