<#
.SYNOPSIS
  Generic single-class/jar runner for anything under apps/.
.DESCRIPTION
  The common "build a classpath, run one main class or jar, capture the
  result" operation duplicated across apps/*-suite-runner/*.ps1 scripts. Does
  not replace those scripts: framework-specific discovery, sharding, and
  categorization stay there. Use this for a one-off "does this class run" /
  "cratonvm vs hotspot" check. Mirrors scripts/run-app.sh.
.PARAMETER AppDir
  Directory under apps/ (e.g. apps/keycloak-suite-runner).
.PARAMETER Target
  Fully-qualified main class, or a .jar file.
.PARAMETER CpFile
  Classpath list file. Default: <AppDir>/craton-testcp.txt if present, else
  target\classes + target\test-classes + every jar under <AppDir>\lib.
.EXAMPLE
  scripts/run-app.ps1 apps/keycloak-suite-runner org.keycloak.Foo
.EXAMPLE
  scripts/run-app.ps1 apps/spring-boot-suite-runner app.jar -Vm hotspot
.EXAMPLE
  # Extra VM args are collected by -ExtraArgs (ValueFromRemainingArguments) -
  # pass them directly, with no bash-style `--` separator (unlike run-app.sh,
  # a bare `--` here is ambiguous against PowerShell's own parameter binding).
  scripts/run-app.ps1 apps/console_probe ConsoleProbe --java-home C:\jdk-25
#>
param(
    [Parameter(Mandatory = $true, Position = 0)][string]$AppDir,
    [Parameter(Mandatory = $true, Position = 1)][string]$Target,
    [string]$CpFile = "",
    [string]$Heap = "1g",
    [int]$TimeoutSec = 300,
    [ValidateSet("cratonvm", "hotspot")][string]$Vm = "cratonvm",
    [string]$OutDir = "",
    [Parameter(ValueFromRemainingArguments = $true)][string[]]$ExtraArgs
)

$ErrorActionPreference = "Stop"
$RepoRoot = Split-Path -Parent $PSScriptRoot

if (-not (Test-Path $AppDir)) { Write-Error "app dir not found: $AppDir"; exit 1 }
$AppDir = (Resolve-Path $AppDir).Path
if (-not $OutDir) { $OutDir = Join-Path $AppDir "out" }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

function Build-Classpath {
    if ($CpFile) {
        if (-not (Test-Path $CpFile)) { Write-Error "-CpFile not found: $CpFile"; exit 1 }
        return (Get-Content $CpFile -Raw).Trim()
    }
    $defaultCp = Join-Path $AppDir "craton-testcp.txt"
    if (Test-Path $defaultCp) { return (Get-Content $defaultCp -Raw).Trim() }

    $parts = @()
    $classes = Join-Path $AppDir "target\classes"
    $testClasses = Join-Path $AppDir "target\test-classes"
    if (Test-Path $classes) { $parts += $classes }
    if (Test-Path $testClasses) { $parts += $testClasses }
    $libDir = Join-Path $AppDir "lib"
    if (Test-Path $libDir) {
        $parts += (Get-ChildItem -Path $libDir -Filter *.jar -Recurse | ForEach-Object { $_.FullName })
    }
    if ($parts.Count -eq 0) {
        Write-Error "no classpath found; pass -CpFile or add $defaultCp"
        exit 1
    }
    return ($parts -join ";")
}

$Classpath = Build-Classpath

function Find-CratonVm {
    $candidates = @(
        $env:CRATONVM_BIN,
        (Join-Path $RepoRoot "target\release\cratonvm.exe"),
        (Join-Path $RepoRoot "target\release\cratonvm")
    )
    foreach ($c in $candidates) {
        if ($c -and (Test-Path $c)) { return $c }
    }
    return $null
}

if ($Vm -eq "cratonvm") {
    $VmBin = Find-CratonVm
    if (-not $VmBin) {
        Write-Error "cratonvm binary not found (set CRATONVM_BIN or build target/release/cratonvm)"
        exit 1
    }
} else {
    if (-not $env:JAVA_HOME) { Write-Error "-Vm hotspot needs JAVA_HOME"; exit 1 }
    $VmBin = Join-Path $env:JAVA_HOME "bin\java.exe"
}

$RunArgs = @("-Xmx$Heap", "-cp", $Classpath)
if ($Target -like "*.jar") {
    $RunArgs += @("--jar", $Target)
} else {
    $RunArgs += $Target
}
if ($ExtraArgs) { $RunArgs += $ExtraArgs }

$Stamp = Get-Date -Format "yyyyMMdd-HHmmss"
$SafeTarget = ($Target -replace '[^A-Za-z0-9_.-]', '_')
$OutStdout = Join-Path $OutDir "$SafeTarget.$Vm.$Stamp.out"
$OutStderr = Join-Path $OutDir "$SafeTarget.$Vm.$Stamp.err"

Write-Host "[run-app] $VmBin $($RunArgs -join ' ')"

# Start-Process's -PassThru ExitCode is unreliable once -RedirectStandardOutput
# / -RedirectStandardError are combined with it (a long-standing PowerShell
# quirk: it can read back empty/$null even after the process has exited,
# which made `exit $proc.ExitCode` silently report success on a real
# failure). Driving System.Diagnostics.Process directly avoids that.
#
# ProcessStartInfo.ArgumentList (a plain string array, no quoting needed) is
# .NET Core-only and is null under Windows PowerShell 5.1's .NET Framework
# runtime, so arguments have to go through the single .Arguments string with
# Win32-style quoting instead.
function ConvertTo-WindowsArgString([string[]]$argv) {
    ($argv | ForEach-Object { '"' + ($_ -replace '"', '""') + '"' }) -join ' '
}

$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $VmBin
$psi.Arguments = ConvertTo-WindowsArgString $RunArgs
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.UseShellExecute = $false

$sw = [System.Diagnostics.Stopwatch]::StartNew()
$proc = [System.Diagnostics.Process]::Start($psi)
$stdoutTask = $proc.StandardOutput.ReadToEndAsync()
$stderrTask = $proc.StandardError.ReadToEndAsync()
if (-not $proc.WaitForExit($TimeoutSec * 1000)) {
    $proc.Kill()
    Write-Error "timed out after ${TimeoutSec}s"
    exit 124
}
$proc.WaitForExit()
$sw.Stop()
Set-Content -Path $OutStdout -Value $stdoutTask.Result -NoNewline
Set-Content -Path $OutStderr -Value $stderrTask.Result -NoNewline
$exitCode = $proc.ExitCode
Write-Host "exit=$exitCode elapsed_ms=$($sw.ElapsedMilliseconds) stdout=$OutStdout stderr=$OutStderr"
exit $exitCode
