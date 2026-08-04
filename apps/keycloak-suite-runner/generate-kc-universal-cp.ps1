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
  fixed-suite-bugs).

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
  So: use -Modules to name the SPECIFIC module(s) whose dependency classpath
  should be considered (e.g. the module that declares the dependency your
  NoClassDefFoundError/NoSuchMethodError is missing), review the printed diff,
  then re-run with -Apply once you're happy with it. Only module
  target/classes and target/test-classes output DIRECTORIES (never jars) are
  auto-included unconditionally — those aren't versioned artifacts, so they
  carry none of the version-conflict/bloat risk jars do.

  A small default module list is also considered even when -Modules is omitted.
  It is reserved for universal-classpath service providers that the generator
  itself adds via target/classes and that therefore need their dependency
  closure available whenever ServiceLoader discovers them.

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
  "quarkus/config-api") whose Maven dependency classpath should be considered
  for merging. Omit to consider only the default universal provider modules,
  module output directories, and the hardcoded supplemental jar list.

.PARAMETER WorkDir
  Scratch/cache directory for Maven classpath files generated when a selected
  module has no precomputed cratonvm-full-cp.txt.

.PARAMETER RefreshModuleClasspaths
  Rebuild generated Maven classpath cache files even when a cached file exists.

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
  [string]$WorkDir = "",
  [string[]]$Modules = @(),
  [switch]$Full,
  [switch]$RefreshModuleClasspaths,
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
if ([string]::IsNullOrEmpty($WorkDir)) {
  $WorkDir = Join-Path $PSScriptRoot ".suite"
}
$WorkDir = [System.IO.Path]::GetFullPath($WorkDir)

function Normalize-Path([string]$p) {
  return $p.Trim().Replace('\', '/')
}

function Test-HasClassFile([string]$Directory) {
  if (-not (Test-Path $Directory)) { return $false }
  $classFile = Get-ChildItem -Path $Directory -Recurse -Filter "*.class" -File -ErrorAction SilentlyContinue |
    Select-Object -First 1
  return $null -ne $classFile
}

function Test-HasServiceDescriptor([string]$Directory) {
  if (-not (Test-Path $Directory)) { return $false }
  $serviceDir = Join-Path $Directory "META-INF\services"
  if (-not (Test-Path $serviceDir -PathType Container)) { return $false }
  $descriptor = Get-ChildItem -Path $serviceDir -File -ErrorAction SilentlyContinue |
    Select-Object -First 1
  return $null -ne $descriptor
}

function Test-StaleClassOutputEntry([string]$Entry) {
  if (-not $Entry) { return $false }
  $path = $Entry.Trim()
  if ($path -notmatch '[\\/]target[\\/](test-)?classes[\\/]?$') { return $false }
  if (-not (Test-Path $path -PathType Container)) { return $false }
  return (Test-HasServiceDescriptor $path) -and -not (Test-HasClassFile $path)
}

function ConvertTo-SafeFileStem([string]$Value) {
  $safe = ($Value -replace '[^A-Za-z0-9_.-]', '_')
  $maxLength = 96
  if ($safe.Length -le $maxLength) { return $safe }

  $sha = [System.Security.Cryptography.SHA256]::Create()
  try {
    $bytes = [System.Text.Encoding]::UTF8.GetBytes($safe)
    $hash = ([System.BitConverter]::ToString($sha.ComputeHash($bytes)) -replace '-', '').Substring(0, 12).ToLowerInvariant()
  } finally {
    $sha.Dispose()
  }
  $prefixLength = $maxLength - $hash.Length - 1
  return "$($safe.Substring(0, $prefixLength))-$hash"
}

function Resolve-MavenExe {
  $sh = Join-Path $KeycloakRoot 'mvnw'
  $cmd = Join-Path $KeycloakRoot 'mvnw.cmd'
  $candidates = if ([System.Environment]::OSVersion.Platform -eq [System.PlatformID]::Win32NT) {
    @($cmd, $sh)
  } else {
    @($sh, $cmd)
  }
  foreach ($candidate in $candidates) {
    if (Test-Path $candidate) { return [System.IO.Path]::GetFullPath($candidate) }
  }
  return 'mvn'
}

function Quote-WindowsArgument([string]$Argument) {
  if ($null -eq $Argument) { return '""' }
  if ($Argument.Length -eq 0) { return '""' }
  if ($Argument -notmatch '[\s"]') { return $Argument }

  $result = New-Object System.Text.StringBuilder
  [void]$result.Append('"')
  $backslashes = 0
  foreach ($ch in $Argument.ToCharArray()) {
    if ($ch -eq '\') {
      $backslashes++
    } elseif ($ch -eq '"') {
      if ($backslashes -gt 0) { [void]$result.Append('\' * ($backslashes * 2)) }
      $backslashes = 0
      [void]$result.Append('\"')
    } else {
      if ($backslashes -gt 0) { [void]$result.Append('\' * $backslashes) }
      $backslashes = 0
      [void]$result.Append($ch)
    }
  }
  if ($backslashes -gt 0) { [void]$result.Append('\' * ($backslashes * 2)) }
  [void]$result.Append('"')
  return $result.ToString()
}

function Set-ProcessArguments([System.Diagnostics.ProcessStartInfo]$StartInfo, [string[]]$Arguments) {
  $argListProp = $StartInfo.GetType().GetProperty('ArgumentList')
  if ($argListProp -and $null -ne $StartInfo.ArgumentList) {
    foreach ($arg in $Arguments) { [void]$StartInfo.ArgumentList.Add($arg) }
  } else {
    $StartInfo.Arguments = (($Arguments | ForEach-Object { Quote-WindowsArgument $_ }) -join ' ')
  }
}

function Invoke-ProcessChecked {
  param(
    [string]$FilePath,
    [string[]]$Arguments,
    [string]$WorkingDirectory,
    [string]$Description
  )

  $psi = [System.Diagnostics.ProcessStartInfo]::new()
  $psi.FileName = $FilePath
  $psi.WorkingDirectory = $WorkingDirectory
  $psi.UseShellExecute = $false
  $psi.CreateNoWindow = $true
  $psi.RedirectStandardOutput = $true
  $psi.RedirectStandardError = $true
  Set-ProcessArguments -StartInfo $psi -Arguments $Arguments

  $proc = [System.Diagnostics.Process]::new()
  $proc.StartInfo = $psi
  [void]$proc.Start()
  $stdoutTask = $proc.StandardOutput.ReadToEndAsync()
  $stderrTask = $proc.StandardError.ReadToEndAsync()
  $proc.WaitForExit()
  try { $stdoutTask.Wait(5000) | Out-Null } catch {}
  try { $stderrTask.Wait(5000) | Out-Null } catch {}
  $stdout = ''
  $stderr = ''
  try { $stdout = $stdoutTask.Result } catch {}
  try { $stderr = $stderrTask.Result } catch {}
  $exitCode = $proc.ExitCode
  $proc.Dispose()

  if ($exitCode -ne 0) {
    return [pscustomobject]@{
      ok = $false
      stdout = $stdout
      stderr = $stderr
      message = "$Description failed with exit $exitCode"
    }
  }
  return [pscustomobject]@{ ok = $true; stdout = $stdout; stderr = $stderr; message = "" }
}

function Get-ModuleClasspathFile {
  param(
    [string]$Module,
    [string]$IncludeScope = "test",
    [string[]]$IncludeGroupIds = @(),
    [string[]]$IncludeArtifactIds = @()
  )

  $moduleRoot = Join-Path $KeycloakRoot $Module
  $precomputed = Join-Path $moduleRoot "cratonvm-full-cp.txt"
  $hasFilters = $IncludeGroupIds.Count -gt 0 -or $IncludeArtifactIds.Count -gt 0 -or $IncludeScope -ne "test"
  if ((-not $hasFilters) -and (Test-Path $precomputed) -and -not $RefreshModuleClasspaths) {
    return (Get-Item $precomputed)
  }

  $pom = Join-Path $moduleRoot "pom.xml"
  if (-not (Test-Path $pom)) {
    Write-Warning "Module '$Module' has no pom.xml at $pom"
    return $null
  }

  $cacheDir = Join-Path $WorkDir "classpaths"
  New-Item -ItemType Directory -Force -Path $cacheDir | Out-Null
  $scopeSuffix = $IncludeScope
  if ($IncludeGroupIds.Count -gt 0) {
    $scopeSuffix += "." + ($IncludeGroupIds -join "_")
  }
  if ($IncludeArtifactIds.Count -gt 0) {
    $scopeSuffix += "." + ($IncludeArtifactIds -join "_")
  }
  $safe = ConvertTo-SafeFileStem "$Module.$scopeSuffix"
  $generated = Join-Path $cacheDir "$safe.full.cp.txt"
  if ((Test-Path $generated) -and -not $RefreshModuleClasspaths -and ((Get-Content -Path $generated -Raw -ErrorAction SilentlyContinue).Trim())) {
    return (Get-Item $generated)
  }

  $mvn = Resolve-MavenExe
  Write-Host "Building Maven classpath for module '$Module' into $generated"
  $mavenArgs = @(
    '-pl', $Module,
    "-DincludeScope=$IncludeScope",
    "-Dmdep.outputFile=$generated",
    '-DskipTests',
    'dependency:build-classpath'
  )
  if ($IncludeGroupIds.Count -gt 0) {
    $mavenArgs = @($mavenArgs[0..2]) + @("-DincludeGroupIds=$($IncludeGroupIds -join ',')") + @($mavenArgs[3..($mavenArgs.Count - 1)])
  }
  if ($IncludeArtifactIds.Count -gt 0) {
    $mavenArgs = @($mavenArgs[0..2]) + @("-DincludeArtifactIds=$($IncludeArtifactIds -join ',')") + @($mavenArgs[3..($mavenArgs.Count - 1)])
  }
  $result = Invoke-ProcessChecked -FilePath $mvn -WorkingDirectory $KeycloakRoot -Description "Maven dependency:build-classpath for $Module from root" -Arguments $mavenArgs
  if (-not $result.ok) {
    if (($result.stdout + $result.stderr) -notmatch 'Could not find the selected project in the reactor') {
      Die "$($result.message)`n$($result.stdout)`n$($result.stderr)"
    }
    Write-Host "Module '$Module' is not selectable from root reactor; retrying from module directory."
    $moduleArgs = @(
      "-DincludeScope=$IncludeScope",
      "-Dmdep.outputFile=$generated",
      '-DskipTests',
      'dependency:build-classpath'
    )
    if ($IncludeGroupIds.Count -gt 0) {
      $moduleArgs = @($moduleArgs[0]) + @("-DincludeGroupIds=$($IncludeGroupIds -join ',')") + @($moduleArgs[1..($moduleArgs.Count - 1)])
    }
    if ($IncludeArtifactIds.Count -gt 0) {
      $moduleArgs = @($moduleArgs[0]) + @("-DincludeArtifactIds=$($IncludeArtifactIds -join ',')") + @($moduleArgs[1..($moduleArgs.Count - 1)])
    }
    $result = Invoke-ProcessChecked -FilePath $mvn -WorkingDirectory $moduleRoot -Description "Maven dependency:build-classpath for $Module from module directory" -Arguments $moduleArgs
    if (-not $result.ok) {
      Die "$($result.message)`n$($result.stdout)`n$($result.stderr)"
    }
  }

  if (-not (Test-Path $generated)) {
    Die "Maven completed but did not write classpath file: $generated"
  }
  return (Get-Item $generated)
}

# ---- 1. Candidate jars from Maven classpath dumps -------------------------
# Scope: -Full considers every precomputed module dump; otherwise only selected
# modules are considered. Explicit -Modules keeps the broad test-scope behavior.
# The default module set may use filters to avoid dragging the whole Keycloak
# reactor into the universal classpath for one ServiceLoader provider.
$candidateJars = New-Object System.Collections.Generic.List[string]
$seenCandidateJars = New-Object 'System.Collections.Generic.HashSet[string]' ([System.StringComparer]::OrdinalIgnoreCase)

$script:DefaultModuleDependencyClosures = @(
  # Universal classpath includes db-edb/target/classes and its ServiceLoader
  # provider. Loading that provider requires test-framework/test-containers and
  # org.testcontainers:testcontainers-jdbc even for tests that do not request
  # EnterpriseDB explicitly.
  [pscustomobject]@{
    Module = "test-framework/db-edb"
    IncludeScope = "runtime"
    IncludeGroupIds = @(
      "org.testcontainers",
      "com.github.docker-java",
      "org.rnorth",
      "org.apache.commons",
      "org.jetbrains"
    )
    IncludeArtifactIds = @()
  },
  [pscustomobject]@{
    Module = "test-framework/junit5-config"
    IncludeScope = "runtime"
    IncludeGroupIds = @("org.infinispan")
    IncludeArtifactIds = @()
  },
  # Universal classpath includes ui/target/classes and its ServiceLoader
  # provider. Loading that provider links Selenium WebDriver suppliers before a
  # test class asks for a browser explicitly, so keep the default closure
  # limited to Selenium and its direct driver/runtime support libraries.
  [pscustomobject]@{
    Module = "test-framework/ui"
    IncludeScope = "runtime"
    IncludeGroupIds = @(
      "org.seleniumhq.selenium",
      "org.htmlunit",
      "io.opentelemetry",
      "org.apache.commons",
      "commons-logging",
      "commons-codec",
      "commons-exec",
      "com.google.auto.service",
      "org.jspecify"
    )
    IncludeArtifactIds = @()
  },
  [pscustomobject]@{
    Module = "test-framework/core"
    IncludeScope = "runtime"
    IncludeGroupIds = @()
    IncludeArtifactIds = @("quarkus-bootstrap-app-model", "quarkus-bootstrap-core", "quarkus-bootstrap-maven-resolver")
  },
  # quarkus-bootstrap-maven-resolver links Maven Resolver, Sisu, Plexus, and
  # Maven model APIs when Keycloak resolves its Quarkus module path during
  # server startup. Keep this separate from the exact Quarkus artifact filter:
  # Maven dependency plugin group and artifact filters are intersected.
  [pscustomobject]@{
    Module = "test-framework/core"
    IncludeScope = "runtime"
    IncludeGroupIds = @(
      "org.apache.maven",
      "org.apache.maven.resolver",
      "org.apache.maven.wagon",
      "org.codehaus.plexus",
      "org.eclipse.sisu",
      "io.smallrye.beanbag",
      "com.google.inject",
      "aopalliance",
      "javax.inject",
      "commons-cli",
      "commons-io",
      "org.slf4j"
    )
    IncludeArtifactIds = @()
  }
)

$cpFiles = @()
if ($Full) {
  $cpFiles = Get-ChildItem -Path $KeycloakRoot -Recurse -Filter "cratonvm-full-cp.txt" -File -ErrorAction SilentlyContinue
  Write-Host "[-Full] scanning ALL $($cpFiles.Count) module classpath dumps under $KeycloakRoot"
}
else {
  $moduleScopes = @()
  if ($Modules.Count -gt 0) {
    $moduleScopes = @($Modules | ForEach-Object {
      [pscustomobject]@{ Module = $_; IncludeScope = "test"; IncludeGroupIds = @(); IncludeArtifactIds = @() }
    })
  } else {
    $moduleScopes = @($script:DefaultModuleDependencyClosures)
    Write-Host "Using default universal dependency module(s): $((@($moduleScopes) | ForEach-Object { $_.Module }) -join ', ')"
  }

  foreach ($scope in $moduleScopes) {
    $f = Get-ModuleClasspathFile -Module $scope.Module -IncludeScope $scope.IncludeScope -IncludeGroupIds @($scope.IncludeGroupIds) -IncludeArtifactIds @($scope.IncludeArtifactIds)
    if ($f) { $cpFiles += $f }
  }
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
    if ((Test-Path $candidate) -and -not (Test-StaleClassOutputEntry $candidate)) {
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
$removedExisting = New-Object System.Collections.Generic.List[string]
if (Test-Path $OutFile) {
  $raw = (Get-Content -Path $OutFile -Raw)
  if (-not [string]::IsNullOrWhiteSpace($raw)) {
    foreach ($part in ($raw -split '[;\r\n]+' | Where-Object { $_.Trim().Length -gt 0 })) {
      $norm = Normalize-Path $part
      if (Test-StaleClassOutputEntry $norm) {
        $removedExisting.Add($norm) | Out-Null
        continue
      }
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

if ($toAdd.Count -eq 0 -and $removedExisting.Count -eq 0) {
  Write-Host "No new classpath entries found; $OutFile already covers everything in scope ($($existing.Count) existing entries)."
  exit 0
}

if ($removedExisting.Count -gt 0) {
  Write-Host "Found $($removedExisting.Count) stale existing class output entries to remove from ${OutFile}:"
  foreach ($r in $removedExisting) { Write-Host "  - $r" }
}

if ($toAdd.Count -gt 0) {
  Write-Host "Found $($toAdd.Count) candidate entries not currently in $OutFile ($($existing.Count) existing entries after pruning):"
  foreach ($a in $toAdd) { Write-Host "  + $a" }
}

if (-not $Apply) {
  Write-Host ""
  Write-Host "Dry run only (no changes written). Re-run with -Apply to apply these classpath changes to $OutFile."
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
