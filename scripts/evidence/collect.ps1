<#
.SYNOPSIS
    Reproducibility evidence collector (PowerShell half; scripts/evidence/collect.sh
    is the bash twin and writes the same file names with the same section headers).

.DESCRIPTION
    Writes a manifest describing exactly which tree, toolchain and machine
    produced a result, so a benchmark number, a GC audit or a differential
    verdict can be re-run months later instead of being taken on trust.

    Files produced under the output directory (default: <repo>/evidence):

      source.txt            tree identity: UTC timestamp, HEAD, worktree
                            cleanliness, submodules, last 30 commits, and the
                            history of the five subsystems whose churn most
                            often invalidates a recorded result
                            (jit/, gc/, vm/src/runtime, types/src/value.rs,
                            classloading/).
      environment.txt       OS, CPU model, CPU features, memory, rustc -Vv,
                            cargo -V, java/javac, cc/cl, and the CRATONVM_* /
                            RUSTFLAGS environment that changes what gets built.
      cargo-metadata.json   machine-readable workspace + resolved dependencies.
      cargo-tree.txt        human-readable dependency tree.
      cargo-duplicates.txt  `cargo tree --duplicates` -- the workspace
                            Cargo.toml documents three ACCEPTED duplicate
                            families; this is how a fourth gets noticed.
      cargo-features.txt    every feature every workspace member declares.
                            Non-default features are how this repository once
                            lost 1,522 tests to a configuration nothing compiled.

.PARAMETER OutDir
    Where to write the manifest. Relative paths resolve against the repository
    root, not the current directory. Default: "evidence".

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File scripts\evidence\collect.ps1

.EXAMPLE
    powershell -File scripts\evidence\collect.ps1 -OutDir out\manifest

.NOTES
    Runnable from anywhere: the script relocates to the repository root itself.

    $ErrorActionPreference is deliberately "Continue", not "Stop". Half of what
    this collects is optional (javac on a JRE-only box, cc on a machine with
    only MSVC, git submodules in a source export). A collector that aborts on
    the first absent tool produces no manifest at all, which is strictly worse
    than a manifest that records "java: not found".

    Exit codes:
      0  manifest written (possibly with "not available" entries)
      2  could not locate the repository root or create the output directory
#>

param(
    [string]$OutDir = "evidence"
)

$ErrorActionPreference = "Continue"

# --------------------------------------------------------------------------
# Locate the repository root and relocate to it.
# --------------------------------------------------------------------------
$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$repoRoot = $null
try {
    Push-Location -LiteralPath $scriptDir
    $top = & git rev-parse --show-toplevel 2>$null
    Pop-Location
    if ($LASTEXITCODE -eq 0 -and $top) {
        $repoRoot = (Resolve-Path -LiteralPath $top).Path
    }
} catch {
    $repoRoot = $null
}
if (-not $repoRoot) {
    # Not a git checkout (source export, vendored copy). Fall back to the
    # script's own location: scripts\evidence\collect.ps1 -> ..\..
    try {
        $repoRoot = (Resolve-Path -LiteralPath (Join-Path $scriptDir "..\..")).Path
    } catch {
        Write-Error "cannot locate repository root"
        exit 2
    }
}

Set-Location -LiteralPath $repoRoot

if ([System.IO.Path]::IsPathRooted($OutDir)) {
    $outPath = $OutDir
} else {
    $outPath = Join-Path $repoRoot $OutDir
}

if (-not (Test-Path -LiteralPath $outPath)) {
    try {
        New-Item -ItemType Directory -Force -Path $outPath | Out-Null
    } catch {
        Write-Error "cannot create output directory: $outPath"
        exit 2
    }
}
$outPath = (Resolve-Path -LiteralPath $outPath).Path

$sourceTxt = Join-Path $outPath "source.txt"
$envTxt    = Join-Path $outPath "environment.txt"

function Test-Tool([string]$Name) {
    $c = Get-Command $Name -ErrorAction SilentlyContinue
    return ($null -ne $c)
}

# Section banner. Identical shape to collect.sh so a Windows manifest and a
# Linux manifest diff cleanly against each other.
function Format-Section([string]$Title) {
    return "`n===== $Title ====="
}

# Run a native command and return its combined output as text, or a recorded
# reason it could not run. Never throws.
function Invoke-Recorded([string]$Exe, [string[]]$CmdArgs) {
    if (-not (Test-Tool $Exe)) {
        return "(not available: $Exe is not on PATH)"
    }
    try {
        $out = & $Exe @CmdArgs 2>&1 | Out-String
        if ([string]::IsNullOrWhiteSpace($out)) { return "(no output)" }
        return $out.TrimEnd()
    } catch {
        return "(command failed: $($_.Exception.Message))"
    }
}

$utc = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")

# --------------------------------------------------------------------------
# evidence\source.txt -- what tree produced this result
# --------------------------------------------------------------------------
$src = New-Object System.Collections.Generic.List[string]
$src.Add("CratonVM evidence manifest: SOURCE")
$src.Add("generated (UTC): $utc")
$src.Add("generated by:    scripts/evidence/collect.ps1")
$src.Add("repository root: $repoRoot")

$src.Add((Format-Section "git rev-parse HEAD"))
$src.Add((Invoke-Recorded "git" @("rev-parse","HEAD")))

$src.Add((Format-Section "git rev-parse --abbrev-ref HEAD"))
$src.Add((Invoke-Recorded "git" @("rev-parse","--abbrev-ref","HEAD")))

$src.Add((Format-Section "git describe --tags --always --dirty"))
$src.Add((Invoke-Recorded "git" @("describe","--tags","--always","--dirty")))

# The single most important line in the manifest. A dirty worktree means the
# result is NOT reproducible from the recorded commit, and every downstream
# consumer must treat it as provisional.
$src.Add((Format-Section "git status --short --branch"))
$src.Add((Invoke-Recorded "git" @("status","--short","--branch")))

$src.Add((Format-Section "worktree cleanliness"))
if (Test-Tool "git") {
    & git diff --quiet 2>$null; $dirtyWork = ($LASTEXITCODE -ne 0)
    & git diff --cached --quiet 2>$null; $dirtyIndex = ($LASTEXITCODE -ne 0)
    if ($dirtyWork -or $dirtyIndex) {
        $src.Add("DIRTY: tracked files differ from HEAD -- results are NOT")
        $src.Add("reproducible from the commit recorded above. Diffstat:")
        $src.Add((Invoke-Recorded "git" @("diff","--stat","HEAD")))
    } else {
        $src.Add("CLEAN: tracked files match HEAD.")
    }
} else {
    $src.Add("(not available: git is not on PATH)")
}

$src.Add((Format-Section "git submodule status"))
# The repository currently has no .gitmodules; this section stays so the
# manifest keeps its shape if one is added, rather than silently omitting a
# submodule pin that a result depends on.
if (Test-Path -LiteralPath (Join-Path $repoRoot ".gitmodules")) {
    $src.Add((Invoke-Recorded "git" @("submodule","status","--recursive")))
} else {
    $src.Add("(no .gitmodules in this tree)")
}

$src.Add((Format-Section "last 30 commits"))
$src.Add((Invoke-Recorded "git" @("log","-n","30","--date=iso-strict","--pretty=format:%h %ad %an %d %s")))

# Subsystem churn. These five paths are where a change silently invalidates a
# previously recorded benchmark, GC audit or differential verdict, so the
# manifest carries their recent history separately from the trunk log.
$subsystems = @("jit","gc","vm/src/runtime","types/src/value.rs","classloading")

$src.Add((Format-Section "recent history: jit/ gc/ vm/src/runtime types/src/value.rs classloading/"))
$src.Add((Invoke-Recorded "git" (@("log","-n","30","--date=iso-strict","--pretty=format:%h %ad %an %s","--") + $subsystems)))

$src.Add((Format-Section "per-subsystem commit counts (last 200 commits)"))
foreach ($p in $subsystems) {
    if (Test-Tool "git") {
        $lines = & git log -n 200 --oneline -- $p 2>$null
        $count = 0
        if ($lines) { $count = @($lines).Count }
        $last = & git log -n 1 --date=iso-strict --pretty=format:"%h %ad" -- $p 2>$null
        if (-not $last) { $last = "none" }
        $src.Add(("{0,-24} commits={1,-4} last={2}" -f $p, $count, $last))
    }
}

Set-Content -LiteralPath $sourceTxt -Value ($src -join "`r`n") -Encoding utf8

# --------------------------------------------------------------------------
# evidence\environment.txt -- what machine and toolchain produced this result
# --------------------------------------------------------------------------
$e = New-Object System.Collections.Generic.List[string]
$e.Add("CratonVM evidence manifest: ENVIRONMENT")
$e.Add("generated (UTC): $utc")

$e.Add((Format-Section "operating system"))
try {
    $os = Get-CimInstance Win32_OperatingSystem -ErrorAction Stop
    $e.Add("Caption:        $($os.Caption)")
    $e.Add("Version:        $($os.Version)")
    $e.Add("BuildNumber:    $($os.BuildNumber)")
    $e.Add("OSArchitecture: $($os.OSArchitecture)")
} catch {
    $e.Add("(Win32_OperatingSystem unavailable: $($_.Exception.Message))")
}
$e.Add("PSVersion:      $($PSVersionTable.PSVersion)")
$e.Add("ProcessorArch:  $env:PROCESSOR_ARCHITECTURE")

$e.Add((Format-Section "CPU"))
try {
    foreach ($cpu in (Get-CimInstance Win32_Processor -ErrorAction Stop)) {
        $e.Add("model name:    $($cpu.Name)")
        $e.Add("manufacturer:  $($cpu.Manufacturer)")
        $e.Add("cores:         $($cpu.NumberOfCores)")
        $e.Add("logical cpus:  $($cpu.NumberOfLogicalProcessors)")
        $e.Add("max clock MHz: $($cpu.MaxClockSpeed)")
    }
} catch {
    $e.Add("(Win32_Processor unavailable: $($_.Exception.Message))")
}

$e.Add((Format-Section "CPU features"))
# The workspace pins `-C target-feature=+sse4.2,+pclmulqdq` in .cargo/config.toml.
# A host that lacks them produces a binary that will not run; a host with AVX-512
# gets different auto-vectorisation. Both change results, so the actual feature
# set is part of the evidence. Win32 has no /proc/cpuinfo, so probe the
# individual features the runtime cares about.
try {
    Add-Type -ErrorAction Stop -TypeDefinition @"
using System.Runtime.InteropServices;
public static class CratonCpuProbe {
    [DllImport("kernel32.dll")]
    public static extern bool IsProcessorFeaturePresent(uint feature);
}
"@
    # PF_* constants from winnt.h that matter to this workspace.
    $pf = [ordered]@{
        "PF_XMMI_INSTRUCTIONS_AVAILABLE (SSE)"    = 6
        "PF_XMMI64_INSTRUCTIONS_AVAILABLE (SSE2)" = 10
        "PF_SSE3_INSTRUCTIONS_AVAILABLE"          = 13
        "PF_SSSE3_INSTRUCTIONS_AVAILABLE"         = 36
        "PF_SSE4_1_INSTRUCTIONS_AVAILABLE"        = 37
        "PF_SSE4_2_INSTRUCTIONS_AVAILABLE"        = 38
        "PF_AVX_INSTRUCTIONS_AVAILABLE"           = 39
        "PF_AVX2_INSTRUCTIONS_AVAILABLE"          = 40
        "PF_AVX512F_INSTRUCTIONS_AVAILABLE"       = 41
        "PF_COMPARE_EXCHANGE128 (cmpxchg16b)"     = 14
        "PF_ARM_V8_CRYPTO_INSTRUCTIONS_AVAILABLE" = 30
    }
    foreach ($k in $pf.Keys) {
        $present = [CratonCpuProbe]::IsProcessorFeaturePresent([uint32]$pf[$k])
        $e.Add(("{0,-42} {1}" -f $k, $present))
    }
} catch {
    $e.Add("(IsProcessorFeaturePresent probe unavailable: $($_.Exception.Message))")
}

$e.Add((Format-Section "memory"))
try {
    $cs = Get-CimInstance Win32_ComputerSystem -ErrorAction Stop
    $os2 = Get-CimInstance Win32_OperatingSystem -ErrorAction Stop
    $e.Add("TotalPhysicalMemory:     $($cs.TotalPhysicalMemory) bytes")
    $e.Add("FreePhysicalMemory:      $($os2.FreePhysicalMemory) KB")
    $e.Add("TotalVirtualMemorySize:  $($os2.TotalVirtualMemorySize) KB")
} catch {
    $e.Add("(memory query unavailable: $($_.Exception.Message))")
}

# `rustc -Vv` carries the commit hash and the host triple -- the two facts that
# make a codegen difference explainable.
$e.Add((Format-Section "rustc -Vv"))
$e.Add((Invoke-Recorded "rustc" @("-Vv")))
$e.Add((Format-Section "cargo -V"))
$e.Add((Invoke-Recorded "cargo" @("-V")))
$e.Add((Format-Section "rustup show (active toolchain)"))
$e.Add((Invoke-Recorded "rustup" @("show")))

$e.Add((Format-Section "java -version"))
$e.Add((Invoke-Recorded "java" @("-version")))
if ($env:JAVA_HOME) {
    $e.Add("JAVA_HOME=$env:JAVA_HOME")
} else {
    $e.Add("JAVA_HOME=<unset>")
}

$e.Add((Format-Section "javac -version"))
if (Test-Tool "javac") {
    $e.Add((Invoke-Recorded "javac" @("-version")))
} else {
    $e.Add("(not available: javac is not on PATH -- build.rs steps that compile")
    $e.Add("Java fixtures will skip, and several test targets will report as")
    $e.Add("skipped rather than failed)")
}

$e.Add((Format-Section "cc --version"))
$e.Add((Invoke-Recorded "cc" @("--version")))
$e.Add((Format-Section "clang --version"))
$e.Add((Invoke-Recorded "clang" @("--version")))
$e.Add((Format-Section "cl (MSVC)"))
if (Test-Tool "cl") {
    # cl.exe prints its banner on stderr and then errors with no inputs; that
    # banner IS the version, so the non-zero exit is expected and harmless.
    $e.Add((Invoke-Recorded "cl" @()))
} else {
    $e.Add("(not available: cl.exe is not on PATH -- run from a Developer")
    $e.Add("Command Prompt, or see scripts/find-vcvars.bat)")
}
$e.Add((Format-Section "link (MSVC linker)"))
if (Test-Tool "link") {
    $e.Add((Invoke-Recorded "link" @()))
} else {
    $e.Add("(not available: link.exe is not on PATH)")
}

$e.Add((Format-Section "relevant environment variables"))
# Only the ones that change what gets built or how the VM behaves. Values, not
# just names: CRATONVM_JIT=off and CRATONVM_JIT=on are different runs.
$pattern = '^(CRATONVM_|RUSTFLAGS$|RUSTDOCFLAGS$|CARGO_|RUST_BACKTRACE$|JAVA_HOME$|KRUN_)'
$vars = Get-ChildItem Env: | Where-Object { $_.Name -match $pattern } | Sort-Object Name
if ($vars) {
    foreach ($v in $vars) { $e.Add("$($v.Name)=$($v.Value)") }
} else {
    $e.Add("(none set)")
}

Set-Content -LiteralPath $envTxt -Value ($e -join "`r`n") -Encoding utf8

# --------------------------------------------------------------------------
# cargo-derived evidence
# --------------------------------------------------------------------------
$metaPath = Join-Path $outPath "cargo-metadata.json"
$treePath = Join-Path $outPath "cargo-tree.txt"
$dupPath  = Join-Path $outPath "cargo-duplicates.txt"
$featPath = Join-Path $outPath "cargo-features.txt"

if (Test-Tool "cargo") {
    # `--no-deps` would drop the resolved third-party graph, which is the half
    # that actually varies between machines. Keep the full resolve.
    $meta = & cargo metadata --format-version 1 2>$null
    if ($LASTEXITCODE -eq 0 -and $meta) {
        Set-Content -LiteralPath $metaPath -Value ($meta -join "`n") -Encoding utf8
    } else {
        Set-Content -LiteralPath $metaPath -Value "(cargo metadata failed)" -Encoding utf8
    }

    $tree = & cargo tree --workspace --edges normal,build 2>&1 | Out-String
    if ([string]::IsNullOrWhiteSpace($tree)) { $tree = & cargo tree 2>&1 | Out-String }
    Set-Content -LiteralPath $treePath -Value $tree -Encoding utf8

    $dupHeader = @(
        "cargo tree --duplicates --workspace",
        "",
        "The workspace Cargo.toml documents three ACCEPTED duplicate families",
        "(hashbrown, getrandom, windows-sys) with the reason each pair cannot be",
        "unified. Anything outside those three is new and should be triaged.",
        ""
    ) -join "`r`n"
    $dup = & cargo tree --duplicates --workspace 2>&1 | Out-String
    Set-Content -LiteralPath $dupPath -Value ($dupHeader + "`r`n" + $dup) -Encoding utf8

    # Feature inventory, parsed from the metadata we just wrote. Done in
    # PowerShell rather than shelling out to python so the Windows half has no
    # extra prerequisite. NOTE: ConvertFrom-Json in Windows PowerShell 5.1
    # returns PSCustomObject, so features are read via .PSObject.Properties.
    $feat = New-Object System.Collections.Generic.List[string]
    $feat.Add("CratonVM workspace feature inventory")
    $feat.Add("")
    $feat.Add("Every feature below is a distinct compile configuration. Features that")
    $feat.Add("are NOT in a crate's ``default`` set are compiled only by the jobs in")
    $feat.Add(".github/workflows/feature-matrix.yml (and the feature-gate jobs in")
    $feat.Add("ci.yml). A feature compiled by no job rots: this repository lost 1,522")
    $feat.Add("tests that way once already.")
    $feat.Add("")
    try {
        $md = Get-Content -LiteralPath $metaPath -Raw | ConvertFrom-Json
        $memberIds = @($md.workspace_members)
        $pkgs = @($md.packages | Where-Object { $memberIds -contains $_.id } | Sort-Object name)

        $feat.Add(("{0,-36} {1}" -f "PACKAGE", "DEFAULT FEATURES"))
        foreach ($p in $pkgs) {
            $def = "(none)"
            if ($p.features -and $p.features.PSObject.Properties.Name -contains "default") {
                $vals = @($p.features.default)
                if ($vals.Count -gt 0) { $def = ($vals | Sort-Object) -join ", " }
            }
            $feat.Add(("{0,-36} {1}" -f $p.name, $def))
        }

        $feat.Add("")
        $feat.Add("PER-PACKAGE FEATURE TABLE (package/feature -> enables)")
        $qualified = New-Object System.Collections.Generic.List[string]
        foreach ($p in $pkgs) {
            $feat.Add("")
            $feat.Add("[$($p.name)]  $($p.manifest_path)")
            $names = @()
            if ($p.features) { $names = @($p.features.PSObject.Properties.Name | Sort-Object) }
            if ($names.Count -eq 0) {
                $feat.Add("  (declares no [features] table)")
                continue
            }
            foreach ($f in $names) {
                $enables = @($p.features.$f)
                $rhs = "(leaf)"
                if ($enables.Count -gt 0) { $rhs = $enables -join ", " }
                $feat.Add(("  {0,-32} -> {1}" -f $f, $rhs))
                if ($f -ne "default") { $qualified.Add("$($p.name)/$f") }
            }
        }

        $feat.Add("")
        $feat.Add("QUALIFIED NON-DEFAULT FEATURE LIST ($($qualified.Count) entries)")
        $feat.Add("This is exactly the list feature-matrix.yml enumerates, minus the")
        $feat.Add("gpu/cuda entries it routes to cuda-bridge.yml and gpu-selfhosted.yml.")
        foreach ($q in $qualified) { $feat.Add("  $q") }
    } catch {
        $feat.Add("(could not parse cargo-metadata.json: $($_.Exception.Message))")
    }
    Set-Content -LiteralPath $featPath -Value ($feat -join "`r`n") -Encoding utf8
} else {
    foreach ($f in @($metaPath, $treePath, $dupPath, $featPath)) {
        Set-Content -LiteralPath $f -Value "(not available: cargo is not on PATH)" -Encoding utf8
    }
}

Write-Output "evidence written to: $outPath"
Get-ChildItem -LiteralPath $outPath | Select-Object Name, Length, LastWriteTime | Format-Table | Out-String | Write-Output
