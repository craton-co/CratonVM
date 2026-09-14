# T17.E.1 — PowerShell port of scripts/capture-hotspot-baseline.sh.
#
# Captures HotSpot C2 median_ns for the 24 kernels consumed by
# `vm/src/bin/bench_hotspot_compare.rs`. Same schema, same host tag
# policy, same kernel bodies as the bash variant — kept in sync so a
# developer on Windows can refresh the baseline locally.
#
# The emitted JSON is byte-identical to the bash port for the same
# inputs (modulo host tag + timestamp), so we deliberately hand-build
# the document via string joins rather than `ConvertTo-Json`. See the
# note near the serializer below.
#
# Usage:
#   pwsh scripts/capture-hotspot-baseline.ps1
#   pwsh scripts/capture-hotspot-baseline.ps1 -Out bench/hotspot-baseline.json
#   pwsh scripts/capture-hotspot-baseline.ps1 -Iterations 20 -Warmup 5
#   pwsh scripts/capture-hotspot-baseline.ps1 -Host windows-latest
#
# Exit codes:
#   0  — JSON written
#   1  — java/javac missing or < 25
#   2  — kernel execution failed

[CmdletBinding()]
param(
    [string]$Out = "bench/hotspot-baseline.json",
    [int]$Iterations = 10,
    [int]$Warmup = 3,
    [Alias('Host')]
    [string]$HostTag = "windows-latest"
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Fail {
    param([Parameter(Mandatory)][string]$Message, [int]$Code = 1)
    Write-Error $Message
    exit $Code
}

# Run a native executable, capture stdout + stderr + exit code without
# tripping PowerShell 5.1's `2>&1`-on-native-exe trap (which wraps every
# stderr line as a NativeCommandError and aborts under `-Stop`). We
# funnel both streams through temp files so the caller sees plain
# strings even on Windows PowerShell.
function Invoke-Native {
    param(
        [Parameter(Mandatory)][string]$FilePath,
        [string[]]$Arguments = @()
    )
    $stdoutFile = [System.IO.Path]::GetTempFileName()
    $stderrFile = [System.IO.Path]::GetTempFileName()
    try {
        $startArgs = @{
            FilePath               = $FilePath
            NoNewWindow            = $true
            Wait                   = $true
            PassThru               = $true
            RedirectStandardOutput = $stdoutFile
            RedirectStandardError  = $stderrFile
        }
        if ($Arguments -and $Arguments.Count -gt 0) {
            $startArgs.ArgumentList = $Arguments
        }
        $proc = Start-Process @startArgs
        [pscustomobject]@{
            ExitCode = $proc.ExitCode
            StdOut   = (Get-Content -Raw -LiteralPath $stdoutFile -ErrorAction SilentlyContinue)
            StdErr   = (Get-Content -Raw -LiteralPath $stderrFile -ErrorAction SilentlyContinue)
        }
    } finally {
        Remove-Item -LiteralPath $stdoutFile, $stderrFile -Force -ErrorAction SilentlyContinue
    }
}

# --- Dependency checks ------------------------------------------------------
# Prefer `$env:JAVA_HOME` when set — keeps the Windows workflow stable
# even if `java.exe` on PATH is an older build — and fall back to the
# first `java`/`javac` on PATH otherwise.
$javaHome = $env:JAVA_HOME
if ($javaHome) {
    $javaHome = $javaHome.TrimEnd('\', '/')
    $javaExe  = Join-Path "$javaHome" 'bin\java.exe'
    $javacExe = Join-Path "$javaHome" 'bin\javac.exe'
    if (-not (Test-Path -LiteralPath "$javaExe" -PathType Leaf)) {
        Fail "JAVA_HOME is set to '$javaHome' but '$javaExe' does not exist"
    }
    if (-not (Test-Path -LiteralPath "$javacExe" -PathType Leaf)) {
        Fail "JAVA_HOME is set to '$javaHome' but '$javacExe' does not exist (JDK required, not JRE)"
    }
} else {
    $javaCmd = Get-Command java -ErrorAction SilentlyContinue
    if (-not $javaCmd) { Fail "java not on PATH; install OpenJDK 25 or set JAVA_HOME" }
    $javacCmd = Get-Command javac -ErrorAction SilentlyContinue
    if (-not $javacCmd) { Fail "javac not on PATH; install a JDK (not a JRE)" }
    $javaExe  = $javaCmd.Source
    $javacExe = $javacCmd.Source
}

$versionResult = Invoke-Native -FilePath "$javaExe" -Arguments @('-version')
if ($versionResult.ExitCode -ne 0) {
    Fail ("java -version failed (exit {0}): {1}" -f $versionResult.ExitCode, $versionResult.StdErr)
}
# `java -version` writes to stderr; stdout is usually empty. Prefer
# whichever stream actually has the text.
$versionLine = @($versionResult.StdErr, $versionResult.StdOut) |
    Where-Object { $_ } |
    ForEach-Object { ($_ -split "(`r`n|`n)")[0] } |
    Where-Object { $_ } |
    Select-Object -First 1
if (-not $versionLine) {
    Fail "cannot parse java -version output (empty)"
}
$versionLine = $versionLine.Trim()
$match = [regex]::Match($versionLine, 'version "(\d+)')
if (-not $match.Success) {
    Fail "cannot parse java -version output: $versionLine"
}
$major = [int]$match.Groups[1].Value
if ($major -lt 25) {
    Fail "OpenJDK 25+ required, got: $versionLine"
}

# --- Kernels (matches the bash variant line-for-line) -----------------------
$metrics = @(
    @{ name = "vm_startup";                        method = "startupBench";      param = "-" }
    @{ name = "shared_vm_startup";                 method = "startupBench";      param = "-" }
    @{ name = "startup_to_first_bytecode";         method = "startupBench";      param = "-" }
    @{ name = "object_allocation/100";             method = "allocate";          param = "100" }
    @{ name = "object_allocation/1000";            method = "allocate";          param = "1000" }
    @{ name = "gc_cycle_1000_objects";             method = "gcCycle";           param = "1000" }
    @{ name = "native_dispatch_noop";              method = "nativeNoop";        param = "-" }
    @{ name = "interpreter_counting_loop/1000";    method = "countingLoop";      param = "1000" }
    @{ name = "interpreter_counting_loop/10000";   method = "countingLoop";      param = "10000" }
    @{ name = "interpreter_counting_loop/100000";  method = "countingLoop";      param = "100000" }
    @{ name = "interpreter_fibonacci/10";          method = "fib";               param = "10" }
    @{ name = "interpreter_fibonacci/20";          method = "fib";               param = "20" }
    @{ name = "interpreter_fibonacci/30";          method = "fib";               param = "30" }
    @{ name = "interpreter_fibonacci/40";          method = "fib";               param = "40" }
    @{ name = "string_creation_100";               method = "stringCreation100"; param = "-" }
    @{ name = "shootout_nbody/100";                method = "nbody";             param = "100" }
    @{ name = "shootout_nbody/1000";               method = "nbody";             param = "1000" }
    @{ name = "shootout_binary_trees/8";           method = "binaryTreesSum";    param = "8" }
    @{ name = "shootout_binary_trees/12";          method = "binaryTreesSum";    param = "12" }
    @{ name = "specjvm_compiler_throughput";       method = "compilerLoop";      param = "-" }
    @{ name = "specjvm_crypto_dispatch_10k";       method = "cryptoDispatch10k"; param = "-" }
    @{ name = "specjvm_scimark_sor/10x5";          method = "sor10x5";           param = "-" }
    @{ name = "specjvm_scimark_sor/20x10";         method = "sor20x10";          param = "-" }
    @{ name = "dacapo_avrora_100k_loop";           method = "countingLoop";      param = "100000" }
)

# --- Emit + compile harness -------------------------------------------------
$tmpRoot = if ($env:TEMP) { $env:TEMP } else { [System.IO.Path]::GetTempPath() }
$tmp = Join-Path "$tmpRoot" ('cratonvm-hotspot-' + [guid]::NewGuid())
New-Item -ItemType Directory -Path "$tmp" -Force | Out-Null
try {
    $javaSrc = @'
// Auto-generated by scripts/capture-hotspot-baseline.ps1 — do not edit.
import java.util.Arrays;

public final class RustJvmHotSpotBench {
    public static void main(String[] args) {
        if (args.length < 3) {
            System.err.println("usage: RustJvmHotSpotBench <method> <param|-> <iterations>");
            System.exit(2);
        }
        String method = args[0];
        String paramStr = args[1];
        int iterations = Integer.parseInt(args[2]);
        int warmup = Integer.parseInt(args[3]);
        int param = "-".equals(paramStr) ? 0 : Integer.parseInt(paramStr);

        for (int i = 0; i < warmup; i++) {
            black(dispatch(method, param));
        }

        long[] samples = new long[iterations];
        for (int i = 0; i < iterations; i++) {
            long t0 = System.nanoTime();
            black(dispatch(method, param));
            samples[i] = System.nanoTime() - t0;
        }
        Arrays.sort(samples);
        long median = samples[samples.length / 2];
        System.out.println("MEDIAN_NS=" + median);
    }

    private static volatile long SINK;
    private static void black(long v) { SINK = v; }

    private static long dispatch(String m, int p) {
        switch (m) {
            case "startupBench":       return startupBench();
            case "allocate":           return allocate(p);
            case "gcCycle":            return gcCycle(p);
            case "nativeNoop":         return nativeNoop();
            case "countingLoop":       return countingLoop(p);
            case "fib":                return fib(p);
            case "stringCreation100":  return stringCreation100();
            case "nbody":              return nbody(p);
            case "binaryTreesSum":     return binaryTreesSum(p);
            case "compilerLoop":       return compilerLoop();
            case "cryptoDispatch10k":  return cryptoDispatch10k();
            case "sor10x5":            return sor(10, 5);
            case "sor20x10":           return sor(20, 10);
            default: throw new IllegalArgumentException("unknown kernel: " + m);
        }
    }

    private static long startupBench() {
        long s = 0;
        for (int i = 0; i < 128; i++) s += i;
        return s;
    }

    static final class Box { final int v; Box(int v) { this.v = v; } }
    private static long allocate(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) s += new Box(i).v;
        return s;
    }

    private static long gcCycle(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            byte[] b = new byte[64];
            s += b.length + i;
        }
        return s;
    }

    private static long nativeNoop() {
        Object o = new Object();
        long s = 0;
        for (int i = 0; i < 10_000; i++) s += o.hashCode();
        return s;
    }

    private static long countingLoop(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) s += i;
        return s;
    }

    private static long fib(int n) {
        long a = 0, b = 1;
        for (int i = 0; i < n; i++) { long t = a + b; a = b; b = t; }
        return a;
    }

    private static long stringCreation100() {
        long s = 0;
        for (int i = 0; i < 100; i++) {
            String x = "hello_" + i;
            s += x.length();
        }
        return s;
    }

    private static long nbody(int n) {
        double s = 0.0;
        for (int i = 0; i < n; i++) {
            for (int j = 0; j < 16; j++) {
                s += Math.sqrt((double) (i * j + 1));
            }
        }
        return Double.doubleToLongBits(s);
    }

    private static long binaryTreesSum(int depth) {
        return treeSum(depth);
    }
    private static long treeSum(int d) {
        if (d <= 0) return 1;
        return 1 + treeSum(d - 1) + treeSum(d - 1);
    }

    private static long compilerLoop() {
        long s = 0;
        for (int i = 0; i < 200; i++) {
            for (int j = 0; j < 200; j++) {
                if ((i ^ j) != 0) s += i * j;
            }
        }
        return s;
    }

    private static long cryptoDispatch10k() {
        long s = 0;
        for (int i = 0; i < 10_000; i++) s += dispatchStep(i);
        return s;
    }
    private static long dispatchStep(int x) { return x * 31L + 17; }

    private static long sor(int n, int iters) {
        long sum = 0;
        int limit = n - 1;
        for (int iter = 0; iter < iters; iter++) {
            for (int i = 1; i < limit; i++) {
                for (int j = 1; j < limit; j++) {
                    sum += (long) i * j;
                }
            }
        }
        return sum;
    }
}
'@

    $javaFile = Join-Path "$tmp" 'RustJvmHotSpotBench.java'
    # UTF-8 without BOM (Set-Content -Encoding UTF8 adds one in PS 5.1,
    # which `javac` on Windows tolerates — but writing via WriteAllText
    # keeps the file identical to what the bash port produces).
    [System.IO.File]::WriteAllText(
        $javaFile,
        $javaSrc,
        (New-Object System.Text.UTF8Encoding $false))
    Write-Host "compiling harness..." -ForegroundColor Yellow
    $compile = Invoke-Native -FilePath "$javacExe" -Arguments @('-d', "$tmp", "$javaFile")
    if ($compile.ExitCode -ne 0) {
        Fail ("javac failed (exit {0}): {1}" -f $compile.ExitCode, $compile.StdErr) 2
    }

    # --- Run each kernel, capture MEDIAN_NS ---
    # `-XX:+UseC2Compiler` was dropped in JDK 25 (C2 is always on when the
    # server VM is used). Only `-XX:TieredStopAtLevel=4` is passed so the
    # script works across JDK 22-26.
    function Invoke-Kernel {
        param([Parameter(Mandatory)][string]$Method, [Parameter(Mandatory)][string]$Param)
        $res = Invoke-Native -FilePath "$javaExe" -Arguments @(
            '-XX:TieredStopAtLevel=4',
            '-cp', "$tmp",
            'RustJvmHotSpotBench',
            $Method, $Param, "$Iterations", "$Warmup"
        )
        if ($res.ExitCode -ne 0) {
            Fail ("kernel failed: {0} {1} (exit {2}): {3}" -f $Method, $Param, $res.ExitCode, $res.StdErr) 2
        }
        $stdout = if ($res.StdOut) { $res.StdOut } else { '' }
        $line = $stdout -split "(`r`n|`n)" |
            Where-Object { $_ -match '^MEDIAN_NS=' } |
            Select-Object -First 1
        if (-not $line) { Fail ("no median parsed for {0} {1}" -f $Method, $Param) 2 }
        return [int64]($line -replace '^MEDIAN_NS=', '')
    }

    $capturedAt = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
    $outDir = Split-Path -Parent "$Out"
    if ($outDir -and -not (Test-Path -LiteralPath "$outDir")) {
        New-Item -ItemType Directory -Path "$outDir" -Force | Out-Null
    }

    $metricLines = New-Object System.Collections.Generic.List[string]
    for ($i = 0; $i -lt $metrics.Count; $i++) {
        $m = $metrics[$i]
        Write-Host ("  running {0} (method={1} param={2})" -f $m.name, $m.method, $m.param)
        $median = Invoke-Kernel -Method $m.method -Param $m.param
        $sep = if ($i -eq ($metrics.Count - 1)) { '' } else { ',' }
        $metricLines.Add(('    "{0}": {{ "median_ns": {1} }}{2}' -f $m.name, $median, $sep))
    }

    # We intentionally hand-build the JSON document rather than piping
    # through `ConvertTo-Json -Depth 10`. Two reasons:
    #   1. `ConvertTo-Json` expands each object across multiple lines
    #      (`"median_ns":` on its own line with PS 5.1 + 7), which would
    #      break byte-identity with `scripts/capture-hotspot-baseline.sh`
    #      — the schema guard test in bench_hotspot_compare.rs depends
    #      on the single-line `{ "median_ns": N }` form.
    #   2. Bash has no jq dependency on the host, so the bash port also
    #      emits JSON by hand. Keeping both sides aligned makes diffs
    #      trivial to audit.
    $json = @()
    $json += '{'
    $json += '  "schema_version": 1,'
    $json += ('  "host": "{0}",' -f $HostTag)
    $json += ('  "captured_at": "{0}",' -f $capturedAt)
    $json += ('  "capture_command": "scripts/capture-hotspot-baseline.ps1 -Iterations {0} -Warmup {1}",' -f $Iterations, $Warmup)
    $json += '  "metrics": {'
    $json += $metricLines.ToArray()
    $json += '  }'
    $json += '}'

    # UTF-8 without BOM + LF line endings so `serde_json` reads it
    # cleanly on any platform and the file matches the bash port byte
    # for byte (modulo host tag + timestamp).
    [System.IO.File]::WriteAllText(
        "$Out",
        ($json -join "`n") + "`n",
        (New-Object System.Text.UTF8Encoding $false))
    Write-Host ("wrote {0} ({1} bytes)" -f $Out, (Get-Item -LiteralPath "$Out").Length) -ForegroundColor Green
}
finally {
    if ($tmp -and (Test-Path -LiteralPath "$tmp")) {
        Remove-Item -LiteralPath "$tmp" -Recurse -Force -ErrorAction SilentlyContinue
    }
}
