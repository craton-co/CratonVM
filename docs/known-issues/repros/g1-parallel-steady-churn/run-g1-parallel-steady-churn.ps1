param(
    [int] $Runs = 40,
    [int] $Iterations = 2000000,
    [string] $Xmx = "16m",
    [string] $VmPath = "",
    [string] $JavaHome = $env:JAVA_HOME
)

$ErrorActionPreference = "Stop"
if (Get-Variable -Name PSNativeCommandUseErrorActionPreference -Scope Global -ErrorAction SilentlyContinue) {
    $Global:PSNativeCommandUseErrorActionPreference = $false
}

$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$repo = Resolve-Path (Join-Path $scriptDir "..\..\..\..")
$classes = Join-Path $repo "target\g1-steady-churn\classes"
$expected = [string]([int64]$Iterations * 1001031L + 93760L)

New-Item -ItemType Directory -Force $classes | Out-Null

$javac = if ($JavaHome) { Join-Path $JavaHome "bin\javac.exe" } else { "javac" }
& $javac -d $classes (Join-Path $scriptDir "SteadyChurn.java")
if ($LASTEXITCODE -ne 0) {
    throw "javac failed with exit code $LASTEXITCODE"
}

if (-not $VmPath) {
    cargo build --release -p cratonvm-cli --bin cratonvm
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build failed with exit code $LASTEXITCODE"
    }
    $built = Join-Path $repo "target\release\cratonvm.exe"
    $stamp = Get-Date -Format "yyyyMMdd-HHmmss"
    $VmPath = Join-Path $repo "target\release\cratonvm-g1-steadychurn-$stamp.exe"
    Copy-Item -LiteralPath $built -Destination $VmPath -Force
}

$oldParallel = $env:CRATONVM_G1_PARALLEL_EVAC
$oldWatchdog = $env:CRATONVM_DISABLE_DEFAULT_WATCHDOG

try {
    $env:CRATONVM_G1_PARALLEL_EVAC = "1"
    $env:CRATONVM_DISABLE_DEFAULT_WATCHDOG = "1"

    $vmArgs = @("--nojit", "-XX:+UseG1GC", "-Xmx$Xmx", "-cp", $classes, "SteadyChurn", [string]$Iterations)
    if ($JavaHome) {
        $vmArgs = @("--java-home", $JavaHome) + $vmArgs
    }

    for ($i = 1; $i -le $Runs; $i++) {
        $oldErrorAction = $ErrorActionPreference
        $ErrorActionPreference = "Continue"
        $output = & $VmPath @vmArgs 2>&1
        $exit = $LASTEXITCODE
        $ErrorActionPreference = $oldErrorAction
        $numeric = @($output | Where-Object { $_ -match "^\d+$" } | Select-Object -Last 1)
        $actual = if ($numeric.Count -gt 0) { [string]$numeric[-1] } else { "" }
        if ($exit -ne 0 -or $actual -ne $expected) {
            Write-Host "FAILED run $i/$Runs exit=$exit expected=$expected actual=$actual"
            $output | ForEach-Object { Write-Host $_ }
            exit 1
        }
        Write-Host "ok $i/$Runs $actual"
    }
} finally {
    $env:CRATONVM_G1_PARALLEL_EVAC = $oldParallel
    $env:CRATONVM_DISABLE_DEFAULT_WATCHDOG = $oldWatchdog
}
