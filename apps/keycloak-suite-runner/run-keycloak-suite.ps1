<#
.SYNOPSIS
  Keycloak JUnit suite runner for CratonVM and HotSpot.

.DESCRIPTION
  Runs the compiled Keycloak test classes one process per class through the
  existing apps\keycloak\kc-runner KcRunner JUnit Platform harness. The runner
  records per-class wall time, persists stdout/stderr logs, can select classes
  by category (passed vs all others), and can run the four category/JIT
  combinations concurrently.

  The "passed" category is defined by a reference result file: classes with
  status/state PASS are passed, every other discovered class is in "others".
  Existing apps\keycloak\kcfull-results-rerun\results.tsv is used as the first
  local reference when present.
#>
[CmdletBinding()]
param(
  [ValidateSet('passed','others','failed','all')] [string]$Category = 'all',
  [ValidateSet('on','off')]                       [string]$Jit = 'on',
  [ValidateSet('craton','hotspot')]               [string]$Vm = 'craton',

  [int]$Start = 1,
  [int]$Count = 0,
  [int]$Parallel = 1,
  [int]$TimeoutSec = 600,

  [string]$RunName = '',
  [string]$ModeName = '',
  [string]$KeycloakRoot = '',
  [string]$WorkDir = '',
  [string]$RefCsv = '',
  [string]$ClassList = '',
  [string]$Exe = '',
  [string]$JdkHome = '',
  [string]$MaxHeap = '2g',
  [string[]]$CratonArgs = @(),

  [switch]$RefreshLists,
  [switch]$RefreshClasspaths,
  [switch]$UniversalClasspath,
  [switch]$ListOnly,
  [switch]$AllModes
)

$ErrorActionPreference = 'Stop'

function Write-Info([string]$Message) {
  Write-Host "[keycloak-suite] $Message"
}

function Die([string]$Message) {
  Write-Error "[keycloak-suite] $Message"
  exit 1
}

function Get-RepoRoot {
  try {
    $root = & git -C $PSScriptRoot rev-parse --show-toplevel 2>$null
    if ($LASTEXITCODE -eq 0 -and $root) { return $root.Trim() }
  } catch {}
  return (Split-Path (Split-Path $PSScriptRoot -Parent) -Parent)
}

function Get-PowerShellExe {
  if ($PSHOME) {
    $candidate = Join-Path $PSHOME 'pwsh.exe'
    if (Test-Path $candidate) { return $candidate }
    $candidate = Join-Path $PSHOME 'powershell.exe'
    if (Test-Path $candidate) { return $candidate }
  }
  return 'powershell.exe'
}

function ConvertTo-SafeName([string]$Value) {
  return ($Value -replace '[^A-Za-z0-9_.-]', '_')
}

function ConvertTo-SafeFileStem([string]$Value) {
  $safe = ConvertTo-SafeName $Value
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

function Get-Sha256Hex([string]$Value) {
  $sha = [System.Security.Cryptography.SHA256]::Create()
  try {
    $bytes = [System.Text.Encoding]::UTF8.GetBytes($Value)
    return (([System.BitConverter]::ToString($sha.ComputeHash($bytes))) -replace '-', '').ToLowerInvariant()
  } finally {
    $sha.Dispose()
  }
}

function ConvertTo-InvariantString([double]$Value) {
  return $Value.ToString('F3', [System.Globalization.CultureInfo]::InvariantCulture)
}

function ConvertFrom-InvariantString([string]$Value) {
  $parsed = 0.0
  if ([double]::TryParse($Value, [System.Globalization.NumberStyles]::Float, [System.Globalization.CultureInfo]::InvariantCulture, [ref]$parsed)) {
    return $parsed
  }
  return 0.0
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
  if ($argListProp) {
    foreach ($arg in $Arguments) { [void]$StartInfo.ArgumentList.Add($arg) }
  } else {
    $StartInfo.Arguments = (($Arguments | ForEach-Object { Quote-WindowsArgument $_ }) -join ' ')
  }
}

function Start-RedirectedProcess {
  param(
    [string]$FilePath,
    [string[]]$Arguments,
    [string]$WorkingDirectory,
    [string]$StdoutPath,
    [string]$StderrPath
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
  return [pscustomobject]@{
    proc = $proc
    stdoutTask = $proc.StandardOutput.ReadToEndAsync()
    stderrTask = $proc.StandardError.ReadToEndAsync()
    stdoutPath = $StdoutPath
    stderrPath = $StderrPath
  }
}

function Complete-RedirectedProcess([object]$Record) {
  $Record.proc.WaitForExit()
  try { $Record.stdoutTask.Wait(5000) | Out-Null } catch {}
  try { $Record.stderrTask.Wait(5000) | Out-Null } catch {}
  $stdout = ''
  $stderr = ''
  try { $stdout = $Record.stdoutTask.Result } catch {}
  try { $stderr = $Record.stderrTask.Result } catch {}
  [System.IO.File]::WriteAllText($Record.stdoutPath, $stdout, [System.Text.Encoding]::UTF8)
  [System.IO.File]::WriteAllText($Record.stderrPath, $stderr, [System.Text.Encoding]::UTF8)
  $exitCode = $Record.proc.ExitCode
  try { $Record.proc.Dispose() } catch {}
  return $exitCode
}

function ConvertTo-RepoRelativeModule([string]$Root, [string]$TestClassesDir) {
  $rootFull = [System.IO.Path]::GetFullPath($Root).TrimEnd('\','/')
  $dirFull = [System.IO.Path]::GetFullPath($TestClassesDir).TrimEnd('\','/')
  $prefix = $rootFull + [System.IO.Path]::DirectorySeparatorChar
  if ($dirFull.StartsWith($prefix, [System.StringComparison]::OrdinalIgnoreCase)) {
    $rel = $dirFull.Substring($prefix.Length)
  } else {
    $rel = Split-Path $dirFull -Leaf
  }
  $suffix = [System.IO.Path]::Combine('target', 'test-classes')
  if ($rel.EndsWith($suffix, [System.StringComparison]::OrdinalIgnoreCase)) {
    $rel = $rel.Substring(0, $rel.Length - $suffix.Length).TrimEnd('\','/')
  }
  return ($rel -replace '\\','/')
}

function Resolve-MavenExe {
  $sh = Join-Path $script:KeycloakDir 'mvnw'
  $cmd = Join-Path $script:KeycloakDir 'mvnw.cmd'
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

function Split-ClasspathEntries([string]$Classpath) {
  if (-not $Classpath) { return @() }
  $separator = [string][System.IO.Path]::PathSeparator
  return @($Classpath -split [regex]::Escape($separator) | Where-Object { $_ })
}

function Add-UniqueClasspathEntry {
  param(
    [System.Collections.Generic.List[string]]$Entries,
    [hashtable]$Seen,
    [string]$Entry
  )

  if (-not $Entry) { return }
  try {
    $full = [System.IO.Path]::GetFullPath($Entry)
    $key = $full.ToLowerInvariant()
    $value = $full
  } catch {
    $key = $Entry.ToLowerInvariant()
    $value = $Entry
  }
  if (-not $Seen.ContainsKey($key)) {
    $Entries.Add($value)
    $Seen[$key] = $true
  }
}

function Add-InferredJUnitRuntimeEntries {
  param(
    [System.Collections.Generic.List[string]]$Entries,
    [hashtable]$Seen
  )

  $snapshot = @($Entries)
  foreach ($entry in $snapshot) {
    if ($entry -match 'junit-platform-engine[\\/](?<version>[^\\/]+)[\\/]junit-platform-engine-[^\\/]+\.jar$') {
      $version = $Matches.version
      $artifactDir = Split-Path (Split-Path $entry -Parent) -Parent
      $groupDir = Split-Path $artifactDir -Parent
      $launcher = Join-Path (Join-Path (Join-Path $groupDir 'junit-platform-launcher') $version) "junit-platform-launcher-$version.jar"
      if (Test-Path $launcher) {
        Add-UniqueClasspathEntry -Entries $Entries -Seen $Seen -Entry $launcher
      }
    }
    if ($entry -match 'junit-jupiter-api[\\/](?<version>[^\\/]+)[\\/]junit-jupiter-api-[^\\/]+\.jar$') {
      $version = $Matches.version
      $artifactDir = Split-Path (Split-Path $entry -Parent) -Parent
      $groupDir = Split-Path $artifactDir -Parent
      $engine = Join-Path (Join-Path (Join-Path $groupDir 'junit-jupiter-engine') $version) "junit-jupiter-engine-$version.jar"
      if (Test-Path $engine) {
        Add-UniqueClasspathEntry -Entries $Entries -Seen $Seen -Entry $engine
      }
    }
  }
}

function Get-M2RepoRoot {
  if ($script:M2RepoRootCache) { return $script:M2RepoRootCache }
  $candidates = @()
  if ($env:KCRUNNER_M2_REPO) { $candidates += $env:KCRUNNER_M2_REPO }
  if ($env:HOME) { $candidates += (Join-Path $env:HOME '.m2/repository') }
  if ($env:USERPROFILE) { $candidates += (Join-Path $env:USERPROFILE '.m2/repository') }
  foreach ($c in $candidates) {
    if (Test-Path $c) { $script:M2RepoRootCache = $c; return $c }
  }
  $script:M2RepoRootCache = ''
  return ''
}

function Add-JUnitPlatformInfraEntries {
  # KcRunner (kc-runner/KcRunner.java) always drives tests through the JUnit
  # Platform Launcher so it can run BOTH vintage (JUnit4) and jupiter (JUnit5)
  # classes uniformly. Maven's dependency:build-classpath only reflects what a
  # module's own pom declares, and plain-JUnit4 modules never declare the
  # launcher/vintage-engine (Surefire bundles its own internally), so KcRunner
  # would NoClassDefFoundError on org.junit.platform.launcher.core.* before
  # running a single test. Make the launcher + both engines universally
  # available regardless of what the module under test happens to depend on.
  param(
    [System.Collections.Generic.List[string]]$Entries,
    [hashtable]$Seen
  )

  $repo = Get-M2RepoRoot
  if (-not $repo) { return }

  $artifacts = @(
    @('org/junit/platform/junit-platform-commons', 'junit-platform-commons'),
    @('org/junit/platform/junit-platform-engine', 'junit-platform-engine'),
    @('org/junit/platform/junit-platform-launcher', 'junit-platform-launcher'),
    @('org/junit/jupiter/junit-jupiter-api', 'junit-jupiter-api'),
    @('org/junit/jupiter/junit-jupiter-engine', 'junit-jupiter-engine'),
    @('org/junit/vintage/junit-vintage-engine', 'junit-vintage-engine'),
    @('org/opentest4j/opentest4j', 'opentest4j'),
    @('org/apiguardian/apiguardian-api', 'apiguardian-api'),
    # junit-vintage-engine's own TestEngine impl reaches into classic JUnit4
    # runtime classes (org.junit.runner.Version, Description, Runner, ...) —
    # without the real junit:junit jar this surfaces as "class not found:
    # junit/runner/Version" (25 CRASH classes in the 2026-07-04 full-suite run:
    # scim/core, ssf/core, ssf/transmitter, test-framework/*, tests/webauthn,
    # tests/clustering — every module whose own pom has no JUnit4 dependency).
    @('junit/junit', 'junit')
  )
  foreach ($pair in $artifacts) {
    $artifactDir = Join-Path $repo $pair[0]
    if (-not (Test-Path $artifactDir)) { continue }
    $jar = Get-ChildItem -Path $artifactDir -Recurse -File -Filter "$($pair[1])-*.jar" -ErrorAction SilentlyContinue |
      Where-Object { $_.Name -notmatch '(-sources|-javadoc)\.jar$' } |
      Sort-Object FullName -Descending |
      Select-Object -First 1
    if ($jar) { Add-UniqueClasspathEntry -Entries $Entries -Seen $Seen -Entry $jar.FullName }
  }
}

function ConvertTo-ManifestClasspathUrl([string]$Entry) {
  $full = [System.IO.Path]::GetFullPath($Entry)
  if ((Test-Path $full -PathType Container) -and -not ($full.EndsWith('\') -or $full.EndsWith('/'))) {
    $full += [System.IO.Path]::DirectorySeparatorChar
  }
  return ([System.Uri]::new($full, [System.UriKind]::Absolute)).AbsoluteUri
}

function Split-ManifestLine([string]$Line) {
  $max = 70
  $out = New-Object System.Collections.Generic.List[string]
  if ($Line.Length -le $max) {
    $out.Add($Line)
    return @($out)
  }

  $out.Add($Line.Substring(0, $max))
  $offset = $max
  while ($offset -lt $Line.Length) {
    $take = [Math]::Min($max - 1, $Line.Length - $offset)
    $out.Add(' ' + $Line.Substring($offset, $take))
    $offset += $take
  }
  return @($out)
}

function New-PathingJar {
  param(
    [string]$Module,
    [string[]]$Entries
  )

  $pathingDir = Join-Path $script:WorkRoot 'pathing-jars'
  New-Item -ItemType Directory -Force -Path $pathingDir | Out-Null
  $signature = 'pathing-jar-manifest-url-v2' + "`n" + (($Entries | ForEach-Object { [System.IO.Path]::GetFullPath($_) }) -join "`n")
  $hash = (Get-Sha256Hex $signature).Substring(0, 16)
  $stem = ConvertTo-SafeFileStem $(if ($Module) { $Module } else { 'universal' })
  $jarPath = Join-Path $pathingDir "$stem-$hash.jar"
  if (Test-Path $jarPath) { return [System.IO.Path]::GetFullPath($jarPath) }

  Add-Type -AssemblyName System.IO.Compression | Out-Null
  Add-Type -AssemblyName System.IO.Compression.FileSystem | Out-Null
  $urls = @($Entries | ForEach-Object { ConvertTo-ManifestClasspathUrl $_ })
  $lines = New-Object System.Collections.Generic.List[string]
  $lines.Add('Manifest-Version: 1.0')
  $lines.Add('Main-Class: KcRunner')
  foreach ($line in (Split-ManifestLine ('Class-Path: ' + ($urls -join ' ')))) {
    $lines.Add($line)
  }
  $lines.Add('')

  if (Test-Path $jarPath) { Remove-Item -LiteralPath $jarPath -Force }
  $zip = [System.IO.Compression.ZipFile]::Open($jarPath, [System.IO.Compression.ZipArchiveMode]::Create)
  try {
    $entry = $zip.CreateEntry('META-INF/MANIFEST.MF')
    $stream = $entry.Open()
    try {
      $writer = [System.IO.StreamWriter]::new($stream, [System.Text.Encoding]::ASCII)
      try {
        $writer.NewLine = "`r`n"
        foreach ($line in $lines) { $writer.WriteLine($line) }
      } finally {
        $writer.Dispose()
      }
    } finally {
      $stream.Dispose()
    }
  } finally {
    $zip.Dispose()
  }
  return [System.IO.Path]::GetFullPath($jarPath)
}

function Import-ResultRows([string]$Path) {
  if (-not (Test-Path $Path)) { return @() }
  $first = Get-Content -Path $Path -TotalCount 1
  $delimiter = ','
  if ($first -like "*`t*") { $delimiter = "`t" }
  return @(Import-Csv -Path $Path -Delimiter $delimiter)
}

function Import-ClassRows([string]$Path) {
  if (-not (Test-Path $Path)) { Die "class list not found: $Path" }
  $rows = @(Import-ResultRows $Path)
  foreach ($row in $rows) {
    if (-not ($row.PSObject.Properties.Name -contains 'module') -or -not ($row.PSObject.Properties.Name -contains 'class')) {
      Die "class list must contain module and class columns: $Path"
    }
  }
  return $rows
}

function Select-ClassRange([object[]]$List) {
  $list = @($List)
  $from = [Math]::Max(1, $Start) - 1
  if ($from -ge $list.Count) { return @() }
  $end = $list.Count
  if ($Count -gt 0) { $end = [Math]::Min($list.Count, $from + $Count) }
  return @($list[$from..($end - 1)])
}

function Resolve-ReferenceFile {
  if ($RefCsv -and (Test-Path $RefCsv)) { return $RefCsv }
  $candidates = @(
    (Join-Path $script:WorkRoot 'reference.tsv'),
    (Join-Path $script:WorkRoot 'reference.csv'),
    (Join-Path $script:KeycloakDir 'kcfull-results-rerun\results.tsv'),
    (Join-Path $script:KeycloakDir 'kcfull-results\results.tsv')
  )
  foreach ($candidate in $candidates) {
    if (Test-Path $candidate) { return $candidate }
  }
  return ''
}

function Get-ClassKey($Row) {
  $module = ''
  $class = ''
  if ($Row.PSObject.Properties.Name -contains 'module') { $module = [string]$Row.module }
  if ($Row.PSObject.Properties.Name -contains 'class') { $class = [string]$Row.class }
  if (-not $class) { return '' }
  return "$module`t$class"
}

function Get-RowStatus($Row) {
  if ($Row.PSObject.Properties.Name -contains 'status') { return ([string]$Row.status).ToUpperInvariant() }
  if ($Row.PSObject.Properties.Name -contains 'state') { return ([string]$Row.state).ToUpperInvariant() }
  return ''
}

function Test-ReferenceVmRow($Row) {
  if (-not ($Row.PSObject.Properties.Name -contains 'vm')) { return $true }
  $vm = ([string]$Row.vm).ToLowerInvariant()
  return ($vm -eq '' -or $vm -eq 'craton' -or $vm -eq 'cratonvm')
}

function Build-ClassLists {
  New-Item -ItemType Directory -Force -Path $script:WorkRoot | Out-Null
  $allPath = Join-Path $script:WorkRoot 'all-tests.tsv'
  $passedPath = Join-Path $script:WorkRoot 'passed.tsv'
  $othersPath = Join-Path $script:WorkRoot 'others.tsv'
  $testClassPatterns = @('*Test.class', '*Tests.class', '*IT.class', '*ITCase.class')

  $records = New-Object System.Collections.Generic.List[object]
  $dirs = Get-ChildItem -Path $script:KeycloakDir -Recurse -Directory -Filter 'test-classes' -ErrorAction SilentlyContinue |
    Where-Object { $_.FullName -match '[\\/]target[\\/]test-classes$' } |
    Sort-Object FullName

  foreach ($dir in $dirs) {
    $module = ConvertTo-RepoRelativeModule $script:KeycloakDir $dir.FullName
    $classes = @(
      foreach ($pattern in $testClassPatterns) {
        Get-ChildItem -Path $dir.FullName -Recurse -File -Filter $pattern -ErrorAction SilentlyContinue
      }
    ) |
      Where-Object { $_.Name -notlike '*$*' } |
      Sort-Object FullName -Unique

    foreach ($classFile in $classes) {
      $rel = $classFile.FullName.Substring($dir.FullName.Length + 1)
      $fqcn = ($rel -replace '\.class$', '') -replace '[\\/]', '.'
      $records.Add([pscustomobject]@{ module = $module; class = $fqcn })
    }
  }

  $unique = $records |
    Sort-Object module, class -Unique

  "module`tclass" | Set-Content -Path $allPath -Encoding ascii
  foreach ($record in $unique) {
    "$($record.module)`t$($record.class)" | Add-Content -Path $allPath -Encoding ascii
  }

  $reference = Resolve-ReferenceFile
  if ($reference) {
    $passedKeys = @{}
    foreach ($row in (Import-ResultRows $reference)) {
      if (-not (Test-ReferenceVmRow $row)) { continue }
      $key = Get-ClassKey $row
      if (-not $key) { continue }
      if ((Get-RowStatus $row) -eq 'PASS') { $passedKeys[$key] = $true }
    }

    "module`tclass" | Set-Content -Path $passedPath -Encoding ascii
    "module`tclass" | Set-Content -Path $othersPath -Encoding ascii
    foreach ($record in $unique) {
      $key = "$($record.module)`t$($record.class)"
      if ($passedKeys.ContainsKey($key)) {
        "$($record.module)`t$($record.class)" | Add-Content -Path $passedPath -Encoding ascii
      } else {
        "$($record.module)`t$($record.class)" | Add-Content -Path $othersPath -Encoding ascii
      }
    }
    Write-Info "class lists refreshed: all=$($unique.Count), passed=$((Import-Csv $passedPath -Delimiter "`t").Count), others=$((Import-Csv $othersPath -Delimiter "`t").Count), reference=$reference"
  } else {
    Write-Info "class list refreshed: all=$($unique.Count); no reference found for passed/others split"
  }
}

function Get-SelectedClasses {
  $allPath = Join-Path $script:WorkRoot 'all-tests.tsv'
  $passedPath = Join-Path $script:WorkRoot 'passed.tsv'
  $othersPath = Join-Path $script:WorkRoot 'others.tsv'

  if ($ClassList) {
    return Select-ClassRange (Import-ClassRows $ClassList)
  }

  if ($RefreshLists -or -not (Test-Path $allPath)) {
    Build-ClassLists
  }

  $resolvedCategory = $Category
  if ($resolvedCategory -eq 'failed') { $resolvedCategory = 'others' }

  $source = $allPath
  if ($resolvedCategory -eq 'passed') { $source = $passedPath }
  if ($resolvedCategory -eq 'others') { $source = $othersPath }

  if (-not (Test-Path $source)) {
    $reference = Resolve-ReferenceFile
    if (-not $reference) {
      Die "category '$Category' needs a reference result file. Pass -RefCsv, copy a prior run to $script:WorkRoot\reference.tsv, or run -Category all first."
    }
    Build-ClassLists
  }
  if (-not (Test-Path $source)) { Die "class list not found: $source" }

  $list = @(Import-Csv -Path $source -Delimiter "`t")
  return Select-ClassRange $list
}

function Get-UniversalClasspathEntries {
  param([string]$Module = '')

  $runnerDir = Join-Path $script:KeycloakDir 'kc-runner'
  $cpFile = Join-Path $script:KeycloakDir 'kc-universal-cp.txt'
  if (-not (Test-Path $runnerDir)) { Die "missing KcRunner directory: $runnerDir" }
  if (-not (Test-Path (Join-Path $runnerDir 'KcRunner.class'))) { Die "missing KcRunner.class in $runnerDir" }
  if (-not (Test-Path $cpFile)) { Die "missing universal classpath file: $cpFile" }

  $cp = (Get-Content -Path $cpFile -Raw).Trim()
  if (-not $cp) { Die "empty classpath file: $cpFile" }

  $entries = New-Object System.Collections.Generic.List[string]
  $seen = @{}
  if ($Module) {
    $modulePath = $Module -replace '/', '\'
    $moduleRoot = Join-Path $script:KeycloakDir $modulePath
    if (Test-Path $moduleRoot) {
      Add-UniqueClasspathEntry -Entries $entries -Seen $seen -Entry (Join-Path $moduleRoot 'target\classes')
      Add-UniqueClasspathEntry -Entries $entries -Seen $seen -Entry (Join-Path $moduleRoot 'target\test-classes')
    }
  }
  foreach ($entry in (@($runnerDir) + (Split-ClasspathEntries $cp))) {
    Add-UniqueClasspathEntry -Entries $entries -Seen $seen -Entry $entry
  }
  Add-InferredJUnitRuntimeEntries -Entries $entries -Seen $seen
  Add-JUnitPlatformInfraEntries -Entries $entries -Seen $seen
  return @($entries)
}

function Get-ModuleClasspathEntries {
  param([string]$Module)

  if (-not $Module -or $UniversalClasspath) {
    return Get-UniversalClasspathEntries -Module $Module
  }

  if (-not $script:ModuleClasspathCache) { $script:ModuleClasspathCache = @{} }
  if ($script:ModuleClasspathCache.ContainsKey($Module)) {
    return @($script:ModuleClasspathCache[$Module])
  }

  $modulePath = $Module -replace '/', '\'
  $moduleRoot = Join-Path $script:KeycloakDir $modulePath
  $pom = Join-Path $moduleRoot 'pom.xml'
  if (-not (Test-Path $pom)) {
    Write-Info "module has no pom.xml, using universal classpath: $Module"
    $entries = @(Get-UniversalClasspathEntries)
    $script:ModuleClasspathCache[$Module] = $entries
    return $entries
  }

  $cacheDir = Join-Path $script:WorkRoot 'classpaths'
  New-Item -ItemType Directory -Force -Path $cacheDir | Out-Null
  $safe = ConvertTo-SafeFileStem $Module
  $cpFile = Join-Path $cacheDir "$safe.test.cp.txt"
  $needsRefresh = $RefreshClasspaths -or -not (Test-Path $cpFile) -or -not ((Get-Content -Path $cpFile -Raw -ErrorAction SilentlyContinue).Trim())
  if ($needsRefresh) {
    $mvn = Resolve-MavenExe
    $outLog = Join-Path $cacheDir "$safe.maven.out.log"
    $errLog = Join-Path $cacheDir "$safe.maven.err.log"
    Write-Info "building Maven test classpath for $Module"
    $record = Start-RedirectedProcess -FilePath $mvn -Arguments @(
      '-pl', $Module,
      '-DincludeScope=test',
      "-Dmdep.outputFile=$cpFile",
      '-DskipTests',
      'dependency:build-classpath'
    ) -WorkingDirectory $script:KeycloakDir -StdoutPath $outLog -StderrPath $errLog
    $exit = Complete-RedirectedProcess $record
    if ($exit -ne 0) {
      $outText = (Get-Content -Path $outLog -Raw -ErrorAction SilentlyContinue)
      $errText = (Get-Content -Path $errLog -Raw -ErrorAction SilentlyContinue)
      if (($outText + $errText) -match 'Could not find the selected project in the reactor') {
        $moduleOutLog = Join-Path $cacheDir "$safe.module.maven.out.log"
        $moduleErrLog = Join-Path $cacheDir "$safe.module.maven.err.log"
        Write-Info "module is not selectable from root reactor, building Maven test classpath from module directory: $Module"
        $record = Start-RedirectedProcess -FilePath $mvn -Arguments @(
          '-DincludeScope=test',
          "-Dmdep.outputFile=$cpFile",
          '-DskipTests',
          'dependency:build-classpath'
        ) -WorkingDirectory $moduleRoot -StdoutPath $moduleOutLog -StderrPath $moduleErrLog
        $exit = Complete-RedirectedProcess $record
        if ($exit -ne 0) {
          Die "Maven dependency:build-classpath failed for $Module from module directory (exit $exit). Logs: $moduleOutLog $moduleErrLog"
        }
      } else {
        Die "Maven dependency:build-classpath failed for $Module (exit $exit). Logs: $outLog $errLog"
      }
    }
  }

  $runnerDir = Join-Path $script:KeycloakDir 'kc-runner'
  if (-not (Test-Path (Join-Path $runnerDir 'KcRunner.class'))) { Die "missing KcRunner.class in $runnerDir" }

  $entries = New-Object System.Collections.Generic.List[string]
  $seen = @{}
  Add-UniqueClasspathEntry -Entries $entries -Seen $seen -Entry (Join-Path $moduleRoot 'target\classes')
  Add-UniqueClasspathEntry -Entries $entries -Seen $seen -Entry (Join-Path $moduleRoot 'target\test-classes')
  Add-UniqueClasspathEntry -Entries $entries -Seen $seen -Entry $runnerDir
  foreach ($entry in (Split-ClasspathEntries ((Get-Content -Path $cpFile -Raw).Trim()))) {
    Add-UniqueClasspathEntry -Entries $entries -Seen $seen -Entry $entry
  }
  Add-InferredJUnitRuntimeEntries -Entries $entries -Seen $seen
  Add-JUnitPlatformInfraEntries -Entries $entries -Seen $seen

  $result = @($entries)
  Write-Info "module classpath $Module entries=$($result.Count)"
  $script:ModuleClasspathCache[$Module] = $result
  return $result
}

function Get-LaunchSpec {
  param([object]$ClassRow)

  $module = ''
  if ($ClassRow -and ($ClassRow.PSObject.Properties.Name -contains 'module')) {
    $module = [string]$ClassRow.module
  }
  $entries = @(Get-ModuleClasspathEntries -Module $module)
  $separator = [string][System.IO.Path]::PathSeparator
  $cp = ($entries -join $separator)
  $usePathingJar = ([System.Environment]::OSVersion.Platform -eq [System.PlatformID]::Win32NT) -and ($cp.Length -gt 24000)
  if ($usePathingJar) {
    $jar = New-PathingJar -Module $module -Entries $entries
    return [pscustomobject]@{ kind = 'jar'; value = $jar; entries = $entries.Count; length = $cp.Length }
  }
  return [pscustomobject]@{ kind = 'cp'; value = $cp; entries = $entries.Count; length = $cp.Length }
}

function Resolve-CratonExe {
  if ($Exe) {
    if (-not (Test-Path $Exe)) { Die "CratonVM executable not found: $Exe" }
    return [System.IO.Path]::GetFullPath($Exe)
  }
  if ($env:CV_BIN -and (Test-Path $env:CV_BIN)) {
    return [System.IO.Path]::GetFullPath($env:CV_BIN)
  }
  if ($env:CRATONVM_KEYCLOAK_EXE -and (Test-Path $env:CRATONVM_KEYCLOAK_EXE)) {
    return [System.IO.Path]::GetFullPath($env:CRATONVM_KEYCLOAK_EXE)
  }

  $unique = Join-Path $script:RepoRoot 'target\release\cratonvm-keycloak-suite.exe'
  if (Test-Path $unique) { return [System.IO.Path]::GetFullPath($unique) }

  $generic = Join-Path $script:RepoRoot 'target\release\cratonvm.exe'
  if (Test-Path $generic) {
    New-Item -ItemType Directory -Force -Path (Split-Path $unique) | Out-Null
    Copy-Item -Path $generic -Destination $unique -Force
    Write-Info "copied generic cratonvm.exe to unique runner binary: $unique"
    return [System.IO.Path]::GetFullPath($unique)
  }

  Die "CratonVM executable not found. Pass -Exe or set CV_BIN/CRATONVM_KEYCLOAK_EXE."
}

function Resolve-Jdk {
  if ($JdkHome) { return $JdkHome }
  if ($env:JAVA_HOME) { return $env:JAVA_HOME }
  return 'C:\Program Files\Java\jdk-25'
}

function Get-ArquillianBootstrapProperties([string]$Module) {
  if (-not $Module.StartsWith('testsuite/integration-arquillian/tests/', [System.StringComparison]::OrdinalIgnoreCase)) {
    return @()
  }

  # Arquillian does not discover this descriptor from the regular test
  # classpath. Maven Surefire supplies it explicitly via -Darquillian.xml.
  # The direct KcRunner path must preserve that contract or Arquillian builds
  # an empty ContainerRegistry and every class fails before its first test.
  $modulePath = $Module -replace '/', [System.IO.Path]::DirectorySeparatorChar
  $moduleRoot = Join-Path $script:KeycloakDir $modulePath
  $descriptor = Join-Path $moduleRoot 'target/dependency/arquillian.xml'
  if (-not (Test-Path $descriptor)) {
    $baseRoot = Join-Path $script:KeycloakDir ('testsuite{0}integration-arquillian{0}tests{0}base' -f [System.IO.Path]::DirectorySeparatorChar)
    $baseDescriptor = Join-Path $baseRoot 'target/dependency/arquillian.xml'
    if (Test-Path $baseDescriptor) { $descriptor = $baseDescriptor }
  }
  if (-not (Test-Path $descriptor)) {
    Die "Arquillian descriptor missing for $Module. Build its Maven test resources first (for example: mvn -pl $Module -am -DskipTests test-compile)."
  }

  $properties = New-Object System.Collections.Generic.List[string]
  $properties.Add("-Darquillian.xml=$([System.IO.Path]::GetFullPath($descriptor))")

  # The Arquillian descriptor is only half of the Surefire contract. Its
  # placeholders and enabled-container expressions are resolved from the
  # module's effective Surefire systemPropertyVariables. Materialize that
  # configuration once per module and cache it with the other runner state.
  $cacheDir = Join-Path $script:WorkRoot 'arquillian-bootstrap'
  New-Item -ItemType Directory -Force -Path $cacheDir | Out-Null
  $safe = ConvertTo-SafeFileStem $Module
  $effectivePom = Join-Path $cacheDir "$safe.effective-pom.xml"
  if ($RefreshClasspaths -or -not (Test-Path $effectivePom)) {
    $mvn = Resolve-MavenExe
    $outLog = Join-Path $cacheDir "$safe.effective-pom.out.log"
    $errLog = Join-Path $cacheDir "$safe.effective-pom.err.log"
    Write-Info "building effective Maven POM for Arquillian bootstrap: $Module"
    $record = Start-RedirectedProcess -FilePath $mvn -Arguments @(
      '-pl', $Module,
      '-DskipTests',
      "-Doutput=$effectivePom",
      'help:effective-pom'
    ) -WorkingDirectory $script:KeycloakDir -StdoutPath $outLog -StderrPath $errLog
    $exit = Complete-RedirectedProcess $record
    # Some leaf modules (notably the legacy SSSD tests) are only selected
    # through their aggregator. Maven can still produce their effective POM
    # when run from the module itself, so retain that direct-module fallback.
    if ($exit -ne 0 -and (Test-Path $moduleRoot)) {
      Write-Info "retrying effective Maven POM from Arquillian module root: $Module"
      $record = Start-RedirectedProcess -FilePath $mvn -Arguments @(
        '-DskipTests',
        "-Doutput=$effectivePom",
        'help:effective-pom'
      ) -WorkingDirectory $moduleRoot -StdoutPath $outLog -StderrPath $errLog
      $exit = Complete-RedirectedProcess $record
    }
    if ($exit -ne 0) {
      Die "Maven help:effective-pom failed for Arquillian module $Module (exit $exit). Logs: $outLog $errLog"
    }
  }

  [xml]$pom = Get-Content -Path $effectivePom -Raw
  $nodes = @($pom.SelectNodes("//*[local-name()='plugin'][*[local-name()='artifactId' and text()='maven-surefire-plugin']]/*[local-name()='configuration']/*[local-name()='systemPropertyVariables']"))
  if ($nodes.Count -eq 0) {
    Die "effective Maven POM has no Surefire systemPropertyVariables for Arquillian module ${Module}: $effectivePom"
  }
  $seen = @{ 'arquillian.xml' = $true }
  foreach ($node in $nodes) {
    foreach ($entry in $node.ChildNodes) {
      if ($entry.NodeType -ne [System.Xml.XmlNodeType]::Element) { continue }
      $name = $entry.LocalName
      $value = $entry.InnerText.Trim()
      if (-not $name -or -not $value -or $value -match '\$\{') { continue }
      if ($seen.ContainsKey($name)) { continue }
      $properties.Add("-D$name=$value")
      $seen[$name] = $true
    }
  }

  return $properties.ToArray()
}

function Get-ModuleSystemProperties([string]$Module) {
  $arquillianProperties = @(Get-ArquillianBootstrapProperties -Module $Module)
  if ($arquillianProperties.Count -gt 0) {
    return $arquillianProperties
  }
  # testsuite/model classes derive from KeycloakModelTest, whose static
  # initializer requires keycloak.model.parameters to name at least one
  # org.keycloak.testsuite.model.parameters.* class or it crashes with a
  # NullPointerException on KeycloakSession.realms() while publishing
  # PostMigrationEvent (no provider/DB is wired up without it). Upstream
  # testsuite/model/pom.xml only ever runs this module under one of its
  # <profiles> (e.g. -Pjpa+infinispan), each of which sets this property plus
  # the keycloak.connectionsJpa.default.* JDBC properties via Surefire's
  # <systemPropertyVariables>. We invoke KcRunner directly instead of through
  # Surefire, so none of that is injected automatically - reproduce the
  # jpa+infinispan profile's values here.
  if ($Module -eq 'testsuite/model') {
    return @(
      '-Dkeycloak.model.parameters=Infinispan,Jpa',
      '-Djava.util.logging.manager=org.jboss.logmanager.LogManager',
      '-Dkeycloak.connectionsJpa.default.driver=org.h2.Driver',
      '-Dkeycloak.connectionsJpa.default.database=keycloak',
      '-Dkeycloak.connectionsJpa.default.user=sa',
      '-Dkeycloak.connectionsJpa.default.password=',
      '-Dkeycloak.connectionsJpa.default.url=jdbc:h2:mem:test;DB_CLOSE_DELAY=-1'
    )
  }
  return @()
}

function New-ProcessRecord {
  param(
    [object]$ClassRow,
    [string]$ModeOut,
    [object]$LaunchSpec,
    [string]$ExePath,
    [string]$JavaExe,
    [string]$JdkPath,
    [bool]$NoJit,
    [int]$Retries = 0
  )

  $module = [string]$ClassRow.module
  $class = [string]$ClassRow.class
  $workingDirectory = $script:KeycloakDir
  if ($module) {
    $modulePath = $module -replace '/', [System.IO.Path]::DirectorySeparatorChar
    $moduleRoot = Join-Path $script:KeycloakDir $modulePath
    if (Test-Path $moduleRoot) { $workingDirectory = $moduleRoot }
  }
  $safe = ConvertTo-SafeFileStem "$module.$class"
  $logDir = Join-Path $ModeOut 'logs'
  New-Item -ItemType Directory -Force -Path $logDir | Out-Null
  $outFile = Join-Path $logDir "$safe.out.log"
  $errFile = Join-Path $logDir "$safe.err.log"
  $moduleProps = @(Get-ModuleSystemProperties -Module $module)

  if ($Vm -eq 'hotspot') {
    $file = $JavaExe
    $args = @("-Xmx$MaxHeap", '-Dfile.encoding=UTF-8', '-Djava.awt.headless=true')
    if ($moduleProps.Count -gt 0) { $args += $moduleProps }
    if ($NoJit) { $args += '-Xint' }
    if ($LaunchSpec.kind -eq 'jar') {
      $args += @('-jar', $LaunchSpec.value, $class)
    } else {
      $args += @('-cp', $LaunchSpec.value, 'KcRunner', $class)
    }
  } else {
    $file = $ExePath
    $args = @('--java-home', $JdkPath, '--stack-dump-on-timeout', '0', '--Xmx', $MaxHeap)
    if ($NoJit) { $args += '--nojit' }
    if ($CratonArgs.Count -gt 0) { $args += $CratonArgs }
    $args += @('-Dfile.encoding=UTF-8', '-Djava.awt.headless=true')
    if ($moduleProps.Count -gt 0) { $args += $moduleProps }
    if ($LaunchSpec.kind -eq 'jar') {
      $args += @('--jar', $LaunchSpec.value, $class)
    } else {
      $args += @('-cp', $LaunchSpec.value, 'KcRunner', $class)
    }
  }

  $psi = [System.Diagnostics.ProcessStartInfo]::new()
  $psi.FileName = $file
  $psi.WorkingDirectory = $workingDirectory
  $psi.UseShellExecute = $false
  $psi.CreateNoWindow = $true
  $psi.RedirectStandardOutput = $true
  $psi.RedirectStandardError = $true
  Set-ProcessArguments -StartInfo $psi -Arguments $args

  $proc = [System.Diagnostics.Process]::new()
  $proc.StartInfo = $psi
  [void]$proc.Start()
  $stdoutTask = $proc.StandardOutput.ReadToEndAsync()
  $stderrTask = $proc.StandardError.ReadToEndAsync()

  return [pscustomobject]@{
    proc = $proc
    stdoutTask = $stdoutTask
    stderrTask = $stderrTask
    module = $module
    class = $class
    start = Get-Date
    outFile = $outFile
    errFile = $errFile
    retries = $Retries
  }
}

function Read-ProcessOutputs([object]$Record) {
  try { $Record.stdoutTask.Wait(5000) | Out-Null } catch {}
  try { $Record.stderrTask.Wait(5000) | Out-Null } catch {}
  $stdout = ''
  $stderr = ''
  try { $stdout = $Record.stdoutTask.Result } catch {}
  try { $stderr = $Record.stderrTask.Result } catch {}
  return [pscustomobject]@{ stdout = $stdout; stderr = $stderr }
}

function Test-SilentAbnormalExit([object]$ExitCode, [string]$Stdout, [string]$Stderr) {
  # A CratonVM process that exits on its own (not killed by our -TimeoutSec
  # watchdog, which is recorded as the literal string 'TIMEOUT' rather than
  # a numeric code) but leaves BOTH stdout and stderr completely empty is
  # not explainable by anything inside CratonVM itself: the top-level
  # main-vm Ok/Err handler, the visibility-first panic hook, the Windows
  # vectored hardware-fault handler, and System.exit/Runtime.exit all print
  # at least one diagnostic line before the process terminates (see
  # keycloak-empty-stderr-process-exits.md). A zero-output
  # abnormal exit is therefore the signature of something OUTSIDE the
  # process (OS/AV/resource-pressure termination) killing it before any of
  # that code could run.
  if ([string]$ExitCode -eq 'TIMEOUT') { return $false }
  if ([int64]$ExitCode -eq 0) { return $false }
  return ($Stdout.Trim().Length -eq 0 -and $Stderr.Trim().Length -eq 0)
}

function Complete-ProcessRecord {
  param(
    [object]$Record,
    [string]$ResultPath,
    [object]$ExitCode,
    [double]$Seconds,
    [string]$Stdout,
    [string]$Stderr
  )

  $stdout = $Stdout
  $stderr = $Stderr
  [System.IO.File]::WriteAllText($Record.outFile, $stdout, [System.Text.Encoding]::UTF8)
  [System.IO.File]::WriteAllText($Record.errFile, $stderr, [System.Text.Encoding]::UTF8)

  $combined = "$stdout`n$stderr"
  $status = 'NOSUMMARY'
  $tests = 0
  $failed = 0
  $aborted = 0
  $skipped = 0
  $containersFailed = 0

  if ([string]$ExitCode -eq 'TIMEOUT') {
    $status = 'HANG'
  } elseif (Test-SilentAbnormalExit -ExitCode $ExitCode -Stdout $stdout -Stderr $stderr) {
    $status = 'SILENTEXIT'
  } elseif ($combined -match '(?i)EXCEPTION_ACCESS_VIOLATION|SIGSEGV|fatal runtime error|panicked at|stack smashing|illegal instruction|STATUS_ACCESS|STATUS_STACK_BUFFER|caught fatal signal|cratonvm panic|internal error: entered unreachable') {
    $status = 'CRASH'
  }

  $m = [regex]::Match($combined, 'KCRUNNER_RESULT tests=(\d+) failed=(\d+) aborted=(\d+) skipped=(\d+) containersFailed=(\d+)')
  if ($m.Success) {
    $tests = [int]$m.Groups[1].Value
    $failed = [int]$m.Groups[2].Value
    $aborted = [int]$m.Groups[3].Value
    $skipped = [int]$m.Groups[4].Value
    $containersFailed = [int]$m.Groups[5].Value
    if ($status -ne 'CRASH' -and $status -ne 'HANG') {
      if ($combined -match 'KCRUNNER_LOAD_FAIL') {
        $status = 'LOADFAIL'
      } elseif ($failed -gt 0 -or $containersFailed -gt 0) {
        $status = 'FAIL'
      } elseif ($tests -eq 0) {
        $status = 'EMPTY'
      } elseif ($aborted -ge $tests) {
        $status = 'SKIP'
      } elseif ($aborted -gt 0) {
        $status = 'PARTIAL'
      } else {
        $status = 'PASS'
      }
    }
  } elseif ($status -eq 'NOSUMMARY') {
    if ($combined -match '(?m)^OK \(') {
      $status = 'PASS'
    } elseif ($combined -match 'FAILURES!!!|Tests run:\s+\d+,\s+Failures:') {
      $status = 'FAIL'
    } elseif ($ExitCode -ne 0) {
      $status = 'CRASH'
    }
  }

  $note = ''
  if ($status -eq 'SILENTEXIT') {
    $note = "no stdout/stderr captured (rc=$ExitCode)"
    if ($Record.retries -gt 0) { $note += "; unchanged after $($Record.retries) retry(ies)" }
    $note += '; not a known CratonVM-internal exit path -- see keycloak-empty-stderr-process-exits.md'
  } else {
    $noteMatch = [regex]::Match($combined, '(?im)^(?!\s+at\s)(.*(?:Exception|Error|Caused by|KCRUNNER_LOAD_FAIL|panicked|not implemented|NoClassDef|NoSuchMethod|AbstractMethod|AssertionError).*)$')
    if ($noteMatch.Success) {
      $note = (($noteMatch.Groups[1].Value -replace "`t", ' ') -replace "`r|`n", ' ')
    }
  }
  if ($note.Length -gt 180) { $note = $note.Substring(0, 180) }
  if ($status -eq 'SKIP') {
    $note = "all $tests test(s) aborted by a JUnit assumption"
  } elseif ($status -eq 'PASS' -or $status -eq 'EMPTY') {
    $note = ''
  }

  $line = @(
    $script:ResultIndex,
    $Record.module,
    $Record.class,
    $Vm,
    $(if ($Jit -eq 'off') { 'off' } else { 'on' }),
    $ExitCode,
    $status,
    (ConvertTo-InvariantString $Seconds),
    $tests,
    $failed,
    $aborted,
    $skipped,
    $containersFailed,
    $Record.outFile,
    $Record.errFile,
    $note
  ) -join "`t"
  Add-Content -Path $ResultPath -Value $line -Encoding ascii
  $script:ResultIndex++

  Write-Host ("  [{0}] {1,-9} {2,7:N1}s {3}" -f $script:EffectiveModeName, $status, $Seconds, $Record.class)
}

function Invoke-Mode {
  param([object[]]$Classes)

  $jdk = Resolve-Jdk
  $javaBin = Join-Path $jdk 'bin'
  $java = @((Join-Path $javaBin 'java.exe'), (Join-Path $javaBin 'java')) | Where-Object { Test-Path $_ } | Select-Object -First 1
  if (-not $java) { Die "HotSpot java executable not found under: $javaBin" }
  $craton = ''
  if ($Vm -eq 'craton') { $craton = Resolve-CratonExe }

  $run = $RunName
  if (-not $run) { $run = Get-Date -Format 'yyyyMMdd-HHmmss' }

  $mode = $ModeName
  if (-not $mode) {
    if ($Vm -eq 'hotspot') {
      $mode = if ($Jit -eq 'off') { 'hotspot-xint' } else { 'hotspot-jit' }
    } else {
      $mode = "$Category-$(if ($Jit -eq 'off') { 'nojit' } else { 'jit' })"
    }
  }
  $script:EffectiveModeName = $mode

  $modeOut = Join-Path (Join-Path (Join-Path $script:WorkRoot 'results') $run) $mode
  New-Item -ItemType Directory -Force -Path $modeOut | Out-Null
  $results = Join-Path $modeOut 'results.tsv'
  if (-not (Test-Path $results)) {
    "index`tmodule`tclass`tvm`tjit`trc`tstatus`tseconds`ttests`tfailed`taborted`tskipped`tcontainersFailed`tstdoutLog`tstderrLog`tnote" |
      Set-Content -Path $results -Encoding ascii
  }

  $done = @{}
  foreach ($row in (Import-Csv -Path $results -Delimiter "`t")) {
    if ($row.class) { $done["$($row.module)`t$($row.class)"] = $true }
  }
  $todo = @($Classes | Where-Object { -not $done.ContainsKey("$($_.module)`t$($_.class)") })
  $script:ResultIndex = (Import-Csv -Path $results -Delimiter "`t").Count + 1

  Write-Info "mode=$mode vm=$Vm jit=$Jit category=$Category selected=$($Classes.Count) todo=$($todo.Count) parallel=$Parallel timeout=${TimeoutSec}s"
  if ($Vm -eq 'craton') { Write-Info "craton exe=$craton" }
  Write-Info "logs/results=$modeOut"

  $modeStarted = Get-Date
  $script:RunningRecords = New-Object System.Collections.Generic.List[object]

  function Drain-Running {
    $still = New-Object System.Collections.Generic.List[object]
    foreach ($record in $script:RunningRecords) {
      $elapsed = ((Get-Date) - $record.start).TotalSeconds
      if ($record.proc.HasExited) {
        $code = $record.proc.ExitCode
        $outputs = Read-ProcessOutputs $record
        if ((Test-SilentAbnormalExit -ExitCode $code -Stdout $outputs.stdout -Stderr $outputs.stderr) -and $record.retries -lt 1) {
          # No CratonVM-internal path exits silently (see Test-SilentAbnormalExit) --
          # this looks like an external/transient kill (OS, AV, resource pressure).
          # Retry once, transparently, before recording anything: most occurrences
          # of this signature do not reproduce on a second attempt.
          Write-Info ("  [retry] {0} rc={1} with no stdout/stderr -- retrying once" -f $record.class, $code)
          try { $record.proc.Dispose() } catch {}
          $retryRow = [pscustomobject]@{ module = $record.module; class = $record.class }
          $retryLaunch = Get-LaunchSpec -ClassRow $retryRow
          $still.Add((New-ProcessRecord -ClassRow $retryRow -ModeOut $modeOut -LaunchSpec $retryLaunch -ExePath $craton -JavaExe $java -JdkPath $jdk -NoJit:($Jit -eq 'off') -Retries ($record.retries + 1)))
        } else {
          Complete-ProcessRecord -Record $record -ResultPath $results -ExitCode $code -Seconds ([Math]::Round($elapsed, 3)) -Stdout $outputs.stdout -Stderr $outputs.stderr
          try { $record.proc.Dispose() } catch {}
        }
      } elseif ($elapsed -ge $TimeoutSec) {
        try { $record.proc.Kill($true) } catch { try { $record.proc.Kill() } catch {} }
        try { $record.proc.WaitForExit(5000) | Out-Null } catch {}
        $outputs = Read-ProcessOutputs $record
        Complete-ProcessRecord -Record $record -ResultPath $results -ExitCode 'TIMEOUT' -Seconds ([Math]::Round($elapsed, 3)) -Stdout $outputs.stdout -Stderr $outputs.stderr
        try { $record.proc.Dispose() } catch {}
      } else {
        $still.Add($record)
      }
    }
    $script:RunningRecords = $still
  }

  foreach ($classRow in $todo) {
    while ($script:RunningRecords.Count -ge $Parallel) {
      Drain-Running
      if ($script:RunningRecords.Count -ge $Parallel) { Start-Sleep -Milliseconds 200 }
    }
    $launch = Get-LaunchSpec -ClassRow $classRow
    $script:RunningRecords.Add((New-ProcessRecord -ClassRow $classRow -ModeOut $modeOut -LaunchSpec $launch -ExePath $craton -JavaExe $java -JdkPath $jdk -NoJit:($Jit -eq 'off')))
  }
  while ($script:RunningRecords.Count -gt 0) {
    Drain-Running
    if ($script:RunningRecords.Count -gt 0) { Start-Sleep -Milliseconds 200 }
  }
  Remove-Variable -Name RunningRecords -Scope Script -ErrorAction SilentlyContinue

  $elapsed = [Math]::Round(((Get-Date) - $modeStarted).TotalSeconds, 3)
  $rows = @(Import-Csv -Path $results -Delimiter "`t")
  $summaryPath = Join-Path $modeOut 'summary.md'
  $counts = $rows | Group-Object status | Sort-Object Name
  $classSeconds = 0.0
  foreach ($row in $rows) {
    $classSeconds += ConvertFrom-InvariantString ([string]$row.seconds)
  }

  $summary = New-Object System.Collections.Generic.List[string]
  $summary.Add("# Keycloak suite result")
  $summary.Add("")
  $summary.Add("- run: $run")
  $summary.Add("- mode: $mode")
  $summary.Add("- vm: $Vm")
  $summary.Add("- jit: $Jit")
  $summary.Add("- category: $Category")
  $summary.Add("- classes recorded: $($rows.Count)")
  $summary.Add("- wall seconds: $(ConvertTo-InvariantString $elapsed)")
  $summary.Add("- summed class seconds: $(ConvertTo-InvariantString $classSeconds)")
  $summary.Add("- keycloak root: $script:KeycloakDir")
  if ($Vm -eq 'craton') { $summary.Add("- craton exe: $craton") }
  $summary.Add("")
  $summary.Add("## Status Counts")
  foreach ($group in $counts) {
    $summary.Add("- $($group.Name): $($group.Count)")
  }
  $summary.Add("")
  $summary.Add("## Files")
  $summary.Add("- results: $results")
  $summary.Add("- logs: $(Join-Path $modeOut 'logs')")
  $summary | Set-Content -Path $summaryPath -Encoding ascii

  if ($Vm -eq 'hotspot') {
    $baselineDir = Join-Path $script:WorkRoot 'baseline'
    New-Item -ItemType Directory -Force -Path $baselineDir | Out-Null
    $baselineBase = "hotspot-baseline-$run"
    Copy-Item -Path $results -Destination (Join-Path $baselineDir "$baselineBase.tsv") -Force
    Copy-Item -Path $summaryPath -Destination (Join-Path $baselineDir "$baselineBase.md") -Force
    Copy-Item -Path $results -Destination (Join-Path $baselineDir 'hotspot-baseline-latest.tsv') -Force
    Copy-Item -Path $summaryPath -Destination (Join-Path $baselineDir 'hotspot-baseline-latest.md') -Force
    Write-Info "baseline copied to $baselineDir\$baselineBase.tsv"
  }

  Write-Info "DONE mode=$mode wall=${elapsed}s summary=$summaryPath"
}

function Invoke-AllModes {
  $reference = Resolve-ReferenceFile
  if (-not $reference) {
    Die "-AllModes needs a reference result file for passed/others. Pass -RefCsv or create $script:WorkRoot\reference.tsv."
  }
  if ($RefreshLists -or -not (Test-Path (Join-Path $script:WorkRoot 'passed.tsv')) -or -not (Test-Path (Join-Path $script:WorkRoot 'others.tsv'))) {
    Build-ClassLists
  }

  $run = $RunName
  if (-not $run) { $run = Get-Date -Format 'yyyyMMdd-HHmmss' }
  $self = $PSCommandPath
  $psExe = Get-PowerShellExe
  $modes = @(
    @{ category = 'passed'; jit = 'on';  name = 'passed-jit' },
    @{ category = 'passed'; jit = 'off'; name = 'passed-nojit' },
    @{ category = 'others'; jit = 'on';  name = 'others-jit' },
    @{ category = 'others'; jit = 'off'; name = 'others-nojit' }
  )

  $runRoot = Join-Path (Join-Path $script:WorkRoot 'results') $run
  New-Item -ItemType Directory -Force -Path $runRoot | Out-Null
  $children = @()
  foreach ($mode in $modes) {
    $log = Join-Path $runRoot "$($mode.name).console.log"
    $err = Join-Path $runRoot "$($mode.name).console.err.log"
    $args = @(
      '-NoProfile',
      '-ExecutionPolicy', 'Bypass',
      '-File', $self,
      '-Category', $mode.category,
      '-Jit', $mode.jit,
      '-Vm', 'craton',
      '-Start', $Start,
      '-Count', $Count,
      '-Parallel', $Parallel,
      '-TimeoutSec', $TimeoutSec,
      '-RunName', $run,
      '-ModeName', $mode.name,
      '-KeycloakRoot', $script:KeycloakDir,
      '-WorkDir', $script:WorkRoot,
      '-JdkHome', (Resolve-Jdk),
      '-MaxHeap', $MaxHeap
    )
    if ($Exe) { $args += @('-Exe', $Exe) }
    if ($RefCsv) { $args += @('-RefCsv', $RefCsv) }
    if ($ClassList) { $args += @('-ClassList', $ClassList) }
    if ($RefreshClasspaths) { $args += '-RefreshClasspaths' }
    if ($UniversalClasspath) { $args += '-UniversalClasspath' }
    if ($CratonArgs.Count -gt 0) {
      foreach ($extra in $CratonArgs) { $args += @('-CratonArgs', $extra) }
    }
    Write-Info "launching $($mode.name) in background"
    $children += Start-RedirectedProcess -FilePath $psExe -Arguments $args -WorkingDirectory $script:RepoRoot -StdoutPath $log -StderrPath $err
  }

  $failedChildren = 0
  foreach ($child in $children) {
    $exitCode = Complete-RedirectedProcess $child
    if ($exitCode -ne 0) { $failedChildren++ }
  }
  if ($failedChildren -gt 0) {
    Die "$failedChildren all-mode child process(es) failed; inspect $runRoot\*.console*.log"
  }
  Write-Info "ALL MODES DONE. Results under $runRoot"
}

$script:RepoRoot = Get-RepoRoot
if (-not $KeycloakRoot) {
  $appsRoot = Split-Path $PSScriptRoot -Parent
  $KeycloakRoot = Join-Path $appsRoot 'keycloak'
}
$script:KeycloakDir = [System.IO.Path]::GetFullPath($KeycloakRoot)
if (-not (Test-Path $script:KeycloakDir)) { Die "Keycloak root not found: $script:KeycloakDir" }

if (-not $WorkDir) { $WorkDir = Join-Path $PSScriptRoot '.suite' }
$script:WorkRoot = [System.IO.Path]::GetFullPath($WorkDir)
New-Item -ItemType Directory -Force -Path $script:WorkRoot | Out-Null

if ($Start -lt 1) { Die "-Start is 1-based and must be >= 1" }
if ($Count -lt 0) { Die "-Count must be >= 0" }
if ($Parallel -lt 1) { Die "-Parallel must be >= 1" }
if ($TimeoutSec -lt 1) { Die "-TimeoutSec must be >= 1" }

if ($AllModes) {
  if ($Vm -ne 'craton') { Die "-AllModes is CratonVM-only" }
  Invoke-AllModes
  return
}

$classes = @(Get-SelectedClasses)
Write-Info "selected $($classes.Count) classes (category=$Category start=$Start count=$(if ($Count -gt 0) { $Count } else { 'all' }))"
if ($ListOnly) {
  $i = $Start
  foreach ($class in $classes) {
    "{0}`t{1}`t{2}" -f $i, $class.module, $class.class
    $i++
  }
  return
}
if ($classes.Count -eq 0) { Die "no classes selected" }

Invoke-Mode -Classes $classes
