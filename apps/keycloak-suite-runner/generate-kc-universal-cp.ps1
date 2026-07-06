<#
.SYNOPSIS
  Find (and optionally apply) missing jar entries for apps/keycloak/kc-universal-cp.txt
  instead of hand-hunting jars one NoClassDefFoundError at a time.

.DESCRIPTION
  `kc-universal-cp.txt` is the single semicolon-joined classpath that
  run-keycloak-suite.ps1 and KcRunner load to run any Keycloak test class. It
  lives under apps/keycloak, which is entirely gitignored (a local Maven
  checkout + build), so the file itself cannot be committed or diffed — until
  now it was hand-patched jar-by-jar whenever a NoClassDefFoundError /
  NoSuchMethodError surfaced a missing dependency (see e.g. the smallrye-config
  and quarkus-core classpath-gap fixes in docs/known-issues and
  docs/internal/fixed-suite-bugs).

  Every Maven module under apps/keycloak already has its own
  `cratonvm-full-cp.txt` (a `mvn dependency:build-classpath` dump for that
  module) — so instead of grepping the local .m2 repo by hand for a jar that
  satisfies one NoClassDefFoundError at a time, point this script at the
  module whose dependency actually needs pulling in.

  THIS SCRIPT NEVER WRITES BY DEFAULT. It always prints what it would add;
  pass -Apply to actually update OutFile. This is deliberate:
    1. A blind union of EVERY module's resolved jars produces ~475 entries
       (~47 KB classpath string) vs. the hand-curated file's ~250 (~22 KB) —
       large enough to hit "Argument list too long" when
       run-keycloak-suite.ps1 passes it directly on a command line (confirmed
       empirically 2026-07-06). Most modules' resolved dependencies are never
       actually needed by the specific test classes anyone runs, so blindly
       unioning everything bloats the classpath for no benefit.
    2. Some artifacts resolve to multiple versions across different modules
       (e.g. commons-io 2.18.0 vs 2.21.0) — pulling in an unrelated module's
       full dependency set risks introducing a version conflict that was
       never there before.
  So: use -Modules to name the SPECIFIC module(s) whose cratonvm-full-cp.txt
  you actually want considered (e.g. the module that declares the dependency
  your NoClassDefFoundError/NoSuchMethodError is missing), review the printed
  diff, then re-run with -Apply once you're happy with it. Only module
  target/classes and target/test-classes output DIRECTORIES (never jars) are
  auto-included unconditionally — those aren't versioned artifacts, so they
  carry none of the version-conflict/bloat risk jars do.

  A short list of runtime-only jars discovered by hand (needed transitively
  via reflection/ServiceLoader/codegen, without being a *declared* dependency
  of any checked-out module, so no per-module cratonvm-full-cp.txt dump ever
  contains them) is always offered too — see $script:SupplementalRuntimeJars.

.PARAMETER KeycloakRoot
  Path to the local Keycloak checkout (default: apps/keycloak next to this
  script's repo root).

.PARAMETER OutFile
  Classpath file to read (if present) and write (default:
  <KeycloakRoot>/kc-universal-cp.txt).

.PARAMETER Modules
  One or more module directories (relative to KeycloakRoot, e.g.
  "quarkus/config-api") whose cratonvm-full-cp.txt jar entries should be
  considered for merging. Omit to consider only module output directories +
  the hardcoded supplemental jar list (no full-repo jar scan).

.PARAMETER Full
  Consider EVERY module's cratonvm-full-cp.txt (not just -Modules). Prints a
  size warning — review carefully before -Apply; see the caveats above.

.PARAMETER Apply
  Actually write the merged result to OutFile. Without this, the script only
  prints what it would add (dry run).

.EXAMPLE
  # See what quarkus/config-api's resolved jars would add on top of the
  # current classpath, without changing anything:
  powershell -File apps/keycloak-suite-runner/generate-kc-universal-cp.ps1 -Modules "quarkus/config-api"

.EXAMPLE
  # Same, but actually write it:
  powershell -File apps/keycloak-suite-runner/generate-kc-universal-cp.ps1 -Modules "quarkus/config-api" -Apply

.EXAMPLE
  # Audit what a from-scratch full union would look like (do not -Apply this
  # straight to kc-universal-cp.txt without checking classpath length first):
  powershell -File apps/keycloak-suite-runner/generate-kc-universal-cp.ps1 -Full -OutFile C:\temp\full-cp.txt -Apply
#>
param(
  [string]$KeycloakRoot = (Join-Path (Split-Path -Parent (Split-Path -Parent $PSScriptRoot)) "apps/keycloak"),
  [string]$OutFile = "",
  [string[]]$Modules = @(),
  [switch]$Full,
  [switch]$Apply
)

$ErrorActionPreference = "Stop"

function Die($msg) {
  Write-Error $msg
  exit 1
}

if (-not (Test-Path $KeycloakRoot)) {
  Die "Keycloak root not found: $KeycloakRoot (apps/keycloak is gitignored/local-only; clone/build it first)"
}
$KeycloakRoot = (Resolve-Path $KeycloakRoot).Path

if ([string]::IsNullOrEmpty($OutFile)) {
  $OutFile = Join-Path $KeycloakRoot "kc-universal-cp.txt"
}

function Normalize-Path([string]$p) {
  return $p.Trim().Replace('\', '/')
}

# ---- 1. Candidate jars from cratonvm-full-cp.txt dumps --------------------
# Scope: -Full considers every module; otherwise only the named -Modules
# (each matched by its cratonvm-full-cp.txt living directly under
# KeycloakRoot/<module>). No jars are considered at all if neither is given.
$candidateJars = New-Object System.Collections.Generic.List[string]
$seenCandidateJars = New-Object 'System.Collections.Generic.HashSet[string]' ([System.StringComparer]::OrdinalIgnoreCase)

$cpFiles = @()
if ($Full) {
  $cpFiles = Get-ChildItem -Path $KeycloakRoot -Recurse -Filter "cratonvm-full-cp.txt" -File -ErrorAction SilentlyContinue
  Write-Host "[-Full] scanning ALL $($cpFiles.Count) module classpath dumps under $KeycloakRoot"
}
elseif ($Modules.Count -gt 0) {
  foreach ($m in $Modules) {
    $f = Join-Path (Join-Path $KeycloakRoot $m) "cratonvm-full-cp.txt"
    if (Test-Path $f) {
      $cpFiles += Get-Item $f
    }
    else {
      Write-Warning "No cratonvm-full-cp.txt found for module '$m' at $f (module not built yet?)"
    }
  }
}
else {
  Write-Host "No -Modules and no -Full given: not scanning any module's jar dependencies (only output dirs + supplemental jars below)."
}

foreach ($f in $cpFiles) {
  $raw = Get-Content -Path $f.FullName -Raw
  if ([string]::IsNullOrWhiteSpace($raw)) { continue }
  # Entries may be `;`-joined (Windows classpath style) and/or newline-separated.
  $parts = $raw -split '[;\r\n]+' | Where-Object { $_.Trim().Length -gt 0 }
  foreach ($part in $parts) {
    $norm = Normalize-Path $part
    if ($seenCandidateJars.Add($norm)) {
      $candidateJars.Add($norm) | Out-Null
    }
  }
}

# ---- 2. Every module's own build output directories (always safe/unconditional) --
$dirEntries = New-Object System.Collections.Generic.List[string]
$seenDirs = New-Object 'System.Collections.Generic.HashSet[string]' ([System.StringComparer]::OrdinalIgnoreCase)

$poms = Get-ChildItem -Path $KeycloakRoot -Recurse -Filter "pom.xml" -File -ErrorAction SilentlyContinue |
  Where-Object { $_.FullName -notmatch '[\\/]target[\\/]' }
foreach ($pom in $poms) {
  $moduleDir = $pom.DirectoryName
  foreach ($sub in @("target/classes", "target/test-classes")) {
    $candidate = Join-Path $moduleDir $sub
    if (Test-Path $candidate) {
      $norm = Normalize-Path $candidate
      if ($seenDirs.Add($norm)) {
        $dirEntries.Add($norm) | Out-Null
      }
    }
  }
}

# ---- 3. Supplemental runtime-only jars no module's cratonvm-full-cp.txt lists
#
# Always offered regardless of -Modules/-Full, since no per-module dump will
# ever surface these (see rationale in the header comment). Discovered by
# iterating the testframework/config/Config bootstrap repro (see
# docs/known-issues/keycloak-testframework-quarkus-config-classpath-gap.md):
#   - org.ow2.asm:asm - io.smallrye.config.ConfigMappingGenerator (used by
#     SmallRyeConfigBuilder.build(), reached from Config.initConfig())
#     generates @ConfigMapping proxy classes via raw ASM ClassWriter/
#     ClassVisitor at runtime; no module here declares an ASM dependency.
#   - io.quarkus:quarkus-bootstrap-runner -
#     io.quarkus.bootstrap.logging.InitialConfigurator (the real class, not
#     the Target_io_quarkus_bootstrap_logging_InitialConfigurator GraalVM
#     substitution stub that DOES ship inside quarkus-core) lives in this
#     separate artifact, which quarkus-core needs at runtime for
#     LoggingSetupRecorder.initializeLogging() but does not itself declare or
#     bundle.
$script:SupplementalRuntimeJars = @(
  "C:/Users/Victor/.m2/repository/org/ow2/asm/asm/9.7/asm-9.7.jar",
  "C:/Users/Victor/.m2/repository/io/quarkus/quarkus-bootstrap-runner/3.33.1.1/quarkus-bootstrap-runner-3.33.1.1.jar"
)
foreach ($j in $script:SupplementalRuntimeJars) {
  if ((Test-Path $j) -and $seenCandidateJars.Add((Normalize-Path $j))) {
    $candidateJars.Add((Normalize-Path $j)) | Out-Null
  }
}

# ---- 4. Read existing OutFile, compute the diff ---------------------------
$existing = New-Object System.Collections.Generic.List[string]
$seenExisting = New-Object 'System.Collections.Generic.HashSet[string]' ([System.StringComparer]::OrdinalIgnoreCase)
if (Test-Path $OutFile) {
  $raw = (Get-Content -Path $OutFile -Raw)
  if (-not [string]::IsNullOrWhiteSpace($raw)) {
    foreach ($part in ($raw -split '[;\r\n]+' | Where-Object { $_.Trim().Length -gt 0 })) {
      $norm = Normalize-Path $part
      if ($seenExisting.Add($norm)) { $existing.Add($norm) | Out-Null }
    }
  }
}

$kcRunnerDir = Join-Path $KeycloakRoot "kc-runner"
if ((Test-Path $kcRunnerDir) -and $seenExisting.Add((Normalize-Path $kcRunnerDir))) {
  $existing.Insert(0, (Normalize-Path $kcRunnerDir))
}

$toAdd = New-Object System.Collections.Generic.List[string]
foreach ($d in $dirEntries) {
  if (-not $seenExisting.Contains($d)) { $toAdd.Add($d) | Out-Null }
}
foreach ($j in $candidateJars) {
  if (-not $seenExisting.Contains($j)) { $toAdd.Add($j) | Out-Null }
}

if ($toAdd.Count -eq 0) {
  Write-Host "No new classpath entries found; $OutFile already covers everything in scope ($($existing.Count) existing entries)."
  exit 0
}

Write-Host "Found $($toAdd.Count) candidate entries not currently in $OutFile ($($existing.Count) existing entries):"
foreach ($a in $toAdd) { Write-Host "  + $a" }

if (-not $Apply) {
  Write-Host ""
  Write-Host "Dry run only (no changes written). Re-run with -Apply to write these $($toAdd.Count) entries to $OutFile."
  exit 0
}

foreach ($a in $toAdd) {
  $existing.Add($a) | Out-Null
  $seenExisting.Add($a) | Out-Null
}
$content = [string]::Join(";", $existing)
[System.IO.File]::WriteAllText($OutFile, $content, (New-Object System.Text.UTF8Encoding($false)))
Write-Host ""
Write-Host "Wrote $($existing.Count) total classpath entries to $OutFile ($($toAdd.Count) added)."
