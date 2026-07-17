<#
.SYNOPSIS
  Elasticsearch JUnit suite runner for CratonVM and HotSpot.

.DESCRIPTION
  Runs compiled Elasticsearch test classes one process per class through
  org.junit.runner.JUnitCore. The runner uses per-module
  build\craton-testcp.txt files produced in the Elasticsearch fixture, records
  per-class wall time, persists stdout/stderr logs, supports passed/others
  slicing from a reference result file, and can run the four category/JIT
  combinations concurrently.
#>
[CmdletBinding()]
param(
  [ValidateSet('passed','others','failed','all')] [string]$Category = 'all',
  [ValidateSet('on','off')]                       [string]$Jit = 'on',
  [ValidateSet('craton','hotspot')]               [string]$Vm = 'craton',

  [int]$Start = 1,
  [int]$Count = 0,
  [int]$Parallel = 1,
  [int]$TimeoutSec = 120,

  [string]$RunName = '',
  [string]$ModeName = '',
  [string]$ElasticsearchRoot = '',
  [string]$WorkDir = '',
  [string]$RefCsv = '',
  [string]$Exe = '',
  [string]$JdkHome = '',
  [string]$MaxHeap = '2g',
  [string]$Seed = 'B17AC9D3E1F2A0C4',
  [string[]]$CratonArgs = @(),

  [switch]$SkipNativeFixtureCheck,
  [switch]$RefreshLists,
  [switch]$ListOnly,
  [switch]$AllModes
)

$ErrorActionPreference = 'Stop'

function Write-Info([string]$Message) {
  Write-Host "[elasticsearch-suite] $Message"
}

function Die([string]$Message) {
  Write-Error "[elasticsearch-suite] $Message"
  exit 1
}

function Get-LinuxX64DynamicSymbols([string]$Library) {
  $nm = Get-Command nm -ErrorAction SilentlyContinue
  if ($nm) {
    $exports = @(& $nm.Source '-D' '--defined-only' $Library 2>$null)
    if ($LASTEXITCODE -ne 0 -or $exports.Count -eq 0) {
      Die "Unable to read dynamic symbols from native fixture: $Library"
    }
    return $exports
  }

  # The preparer supplies this image. Its nm fallback keeps the standard
  # Windows PowerShell runner able to validate the Linux fixture it launches
  # through a Linux target environment.
  $docker = Get-Command docker -ErrorAction SilentlyContinue
  if (-not $docker) {
    Die "Cannot validate Linux libvec fixture because neither 'nm' nor Docker is available. Run this runner on the Linux target, or prepare the fixture with prepare-elasticsearch-libvec-fixture.ps1."
  }

  $libraryDir = Split-Path -Parent $Library
  $libraryName = Split-Path -Leaf $Library
  $image = 'cratonvm-es-libvec-toolchain-20260717'
  $savedErrorActionPreference = $ErrorActionPreference
  try {
    $ErrorActionPreference = 'Continue'
    $exports = @(& $docker.Source 'run' '--rm' '-v' "$libraryDir`:/fixture:ro" $image 'nm' '-D' '--defined-only' "/fixture/$libraryName" 2>$null)
    $exitCode = $LASTEXITCODE
  } finally {
    $ErrorActionPreference = $savedErrorActionPreference
  }
  if ($exitCode -ne 0 -or $exports.Count -eq 0) {
    Die "Unable to read dynamic symbols from native fixture: $Library. Install GNU binutils or run prepare-elasticsearch-libvec-fixture.ps1 to create the local validation image."
  }
  return $exports
}

function Assert-LinuxX64VectorFixture([string]$Root) {
  # JdkVectorLibrary eagerly links its complete native table during class
  # initialization. A missing or old libvec therefore makes nearly every ES
  # test fail before it reaches test code. Keep this check before class-list
  # selection so a fixture error can never be recorded as suite failures.
  $library = Join-Path $Root 'lib/platform/linux-x64/libvec.so'
  if (-not (Test-Path -LiteralPath $library -PathType Leaf)) {
    Die "Elasticsearch native fixture is missing: $library. Rebuild it from this checkout with apps/elasticsearch-suite-runner/prepare-elasticsearch-libvec-fixture.ps1 -ElasticsearchRoot '$Root'."
  }

  $exports = @(Get-LinuxX64DynamicSymbols $library)

  $symbols = @($exports | ForEach-Object {
    $parts = $_ -split '\s+'
    if ($parts.Count -ge 3) { $parts[$parts.Count - 1] }
  } | Where-Object { $_ })
  $vectorSymbols = @($symbols | Where-Object { $_ -like 'vec_*' })

  # The 2026-07 fixture ABI has 155 vec_* exports. The old 145-export
  # artifact lacks the bulk8 family; these sentinels make the diagnostic
  # explicit even if a future tooling change changes the count.
  $required = @('vec_caps', 'vec_cosi8_bulk8', 'vec_doti8_bulk8', 'vec_sqri8_bulk8')
  $missing = @($required | Where-Object { $symbols -notcontains $_ })
  if ($vectorSymbols.Count -lt 155 -or $missing.Count -gt 0) {
    $missingText = if ($missing.Count) { $missing -join ', ' } else { 'none' }
    Die "Elasticsearch native fixture is stale or incompatible: $library exports $($vectorSymbols.Count) vec_* symbols (need at least 155); missing required symbols: $missingText. Rebuild it from this checkout with apps/elasticsearch-suite-runner/prepare-elasticsearch-libvec-fixture.ps1 -ElasticsearchRoot '$Root'."
  }

  $hash = (Get-FileHash -LiteralPath $library -Algorithm SHA256).Hash.ToLowerInvariant()
  Write-Info "validated libvec path=$library vec_symbols=$($vectorSymbols.Count) sha256=$hash"
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

function Get-LogBaseName([string]$Module, [string]$Class) {
  $safe = ConvertTo-SafeName "$Module.$Class"
  if ($safe.Length -le 80) { return $safe }

  $sha = [System.Security.Cryptography.SHA256]::Create()
  try {
    $bytes = [System.Text.Encoding]::UTF8.GetBytes("$Module`t$Class")
    $hashBytes = $sha.ComputeHash($bytes)
    $hash = ([System.BitConverter]::ToString($hashBytes) -replace '-', '').Substring(0, 12).ToLowerInvariant()
  } finally {
    $sha.Dispose()
  }
  return "$($safe.Substring(0, 64)).$hash"
}

function ConvertTo-InvariantString([double]$Value) {
  return $Value.ToString('F3', [System.Globalization.CultureInfo]::InvariantCulture)
}

function Add-ContentWithRetry([string]$Path, [string]$Value) {
  for ($attempt = 1; $attempt -le 50; $attempt++) {
    try {
      Add-Content -LiteralPath $Path -Value $Value -Encoding ascii -ErrorAction Stop
      return
    } catch [System.IO.IOException] {
      if ($attempt -eq 50) { throw }
      Start-Sleep -Milliseconds ([Math]::Min(1000, 40 * $attempt))
    }
  }
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

  # These paths are also used by `-AllModes` for the child PowerShell
  # consoles.  Create their parents here instead of relying on a caller's
  # sibling-directory side effect.
  foreach ($path in @($StdoutPath, $StderrPath)) {
    $parent = Split-Path -Parent $path
    if ($parent) { New-Item -ItemType Directory -Force -Path $parent | Out-Null }
  }

  $psi = [System.Diagnostics.ProcessStartInfo]::new()
  $psi.FileName = $FilePath
  $psi.WorkingDirectory = $WorkingDirectory
  $psi.UseShellExecute = $false
  $psi.CreateNoWindow = $true
  $psi.RedirectStandardOutput = $true
  $psi.RedirectStandardError = $true
  Set-ProcessArguments -StartInfo $psi -Arguments $Arguments

  [void]$psi.EnvironmentVariables.Set_Item('CRATONVM_DISABLE_DEFAULT_WATCHDOG', '1')

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
  foreach ($path in @($Record.stdoutPath, $Record.stderrPath)) {
    $parent = Split-Path -Parent $path
    if ($parent) { New-Item -ItemType Directory -Force -Path $parent | Out-Null }
  }
  [System.IO.File]::WriteAllText($Record.stdoutPath, $stdout, [System.Text.Encoding]::UTF8)
  [System.IO.File]::WriteAllText($Record.stderrPath, $stderr, [System.Text.Encoding]::UTF8)
  $exitCode = $Record.proc.ExitCode
  try { $Record.proc.Dispose() } catch {}
  return $exitCode
}

function ConvertTo-ModulePath([string]$Root, [string]$Path) {
  $rootFull = [System.IO.Path]::GetFullPath($Root).TrimEnd('\','/')
  $pathFull = [System.IO.Path]::GetFullPath($Path).TrimEnd('\','/')
  $prefix = $rootFull + [System.IO.Path]::DirectorySeparatorChar
  if ($pathFull.StartsWith($prefix, [System.StringComparison]::OrdinalIgnoreCase)) {
    return ($pathFull.Substring($prefix.Length) -replace '\\','/')
  }
  return ((Split-Path $pathFull -Leaf) -replace '\\','/')
}

function ConvertTo-RepoRelativeModule([string]$Root, [string]$CpFile) {
  $buildDir = Split-Path $CpFile -Parent
  $moduleDir = Split-Path $buildDir -Parent
  return ConvertTo-ModulePath $Root $moduleDir
}

function Import-ResultRows([string]$Path) {
  if (-not (Test-Path $Path)) { return @() }
  $first = Get-Content -Path $Path -TotalCount 1
  if (-not $first) { return @() }
  $delimiter = ','
  if ($first -like "*`t*") { $delimiter = "`t" }

  $cols = $first.Split($delimiter).Count
  $hasHeader = ($first -match '(^|,|\t)(module|class|status|verdict)(,|\t|$)')
  if ($hasHeader) {
    return @(Import-Csv -Path $Path -Delimiter $delimiter)
  }

  if ($cols -ge 7) {
    return @(Import-Csv -Path $Path -Delimiter $delimiter -Header module,class,hs_rc,hs_status,cv_rc,cv_status,verdict)
  }
  return @()
}

function Resolve-ReferenceFile {
  if ($RefCsv -and (Test-Path $RefCsv)) { return $RefCsv }
  $candidates = @(
    (Join-Path $script:WorkRoot 'reference.tsv'),
    (Join-Path $script:WorkRoot 'reference.csv'),
    (Join-Path $script:ElasticsearchDir 'cratonvm-suite\results.jit.all.tsv'),
    (Join-Path $script:ElasticsearchDir 'cratonvm-suite\results.jit-on.tsv'),
    (Join-Path $script:ElasticsearchDir 'cratonvm-suite\results.nojit.all.tsv'),
    (Join-Path $script:ElasticsearchDir 'cratonvm-suite\results.nojit.tsv')
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
  foreach ($name in @('status','state','verdict','cv_status')) {
    if ($Row.PSObject.Properties.Name -contains $name) {
      $value = ([string]$Row.$name).Trim().ToUpperInvariant()
      if ($value) { return $value }
    }
  }
  return ''
}

function Test-ReferenceVmRow($Row) {
  if (-not ($Row.PSObject.Properties.Name -contains 'vm')) { return $true }
  $vm = ([string]$Row.vm).ToLowerInvariant()
  return ($vm -eq '' -or $vm -eq 'craton' -or $vm -eq 'cratonvm')
}

function Write-ClassList([string]$Path, [object[]]$Rows) {
  "module`tclass" | Set-Content -Path $Path -Encoding ascii
  foreach ($row in $Rows) {
    Add-ContentWithRetry -Path $Path -Value "$($row.module)`t$($row.class)"
  }
}

function Build-ClassLists {
  New-Item -ItemType Directory -Force -Path $script:WorkRoot | Out-Null
  $allPath = Join-Path $script:WorkRoot 'all-tests.tsv'
  $passedPath = Join-Path $script:WorkRoot 'passed.tsv'
  $othersPath = Join-Path $script:WorkRoot 'others.tsv'

  $records = New-Object System.Collections.Generic.List[object]
  $legacyList = Join-Path $script:ElasticsearchDir 'cratonvm-suite\all-classes.tsv'
  if (Test-Path $legacyList) {
    foreach ($line in (Get-Content -Path $legacyList)) {
      if (-not $line.Trim()) { continue }
      if ($line -match '^module`tclass$') { continue }
      $parts = $line -split "`t"
      if ($parts.Count -lt 2) { continue }
      $records.Add([pscustomobject]@{ module = $parts[0]; class = $parts[1] })
    }
  } else {
    $cpFiles = Get-ChildItem -Path $script:ElasticsearchDir -Recurse -File -Filter 'craton-testcp.txt' -ErrorAction SilentlyContinue |
      Sort-Object FullName
    foreach ($cpFile in $cpFiles) {
      $module = ConvertTo-RepoRelativeModule $script:ElasticsearchDir $cpFile.FullName
      $moduleDir = Join-Path $script:ElasticsearchDir ($module -replace '/', [System.IO.Path]::DirectorySeparatorChar)
      $testDirs = @(
        (Join-Path $moduleDir 'build\classes\java\test'),
        (Join-Path $moduleDir 'build\classes\java\internalClusterTest'),
        (Join-Path $moduleDir 'build\classes\java\yamlRestTest')
      ) | Where-Object { Test-Path $_ }
      foreach ($testDir in $testDirs) {
        $classes = Get-ChildItem -Path $testDir -Recurse -File -Filter '*.class' -ErrorAction SilentlyContinue |
          Where-Object { $_.Name -notlike '*$*' -and ($_.Name -like '*Tests.class' -or $_.Name -like '*IT.class' -or $_.Name -like '*Test.class') } |
          Sort-Object FullName
        foreach ($classFile in $classes) {
          $rel = $classFile.FullName.Substring($testDir.Length + 1)
          $fqcn = ($rel -replace '\.class$', '') -replace '[\\/]', '.'
          $records.Add([pscustomobject]@{ module = $module; class = $fqcn })
        }
      }
    }
  }

  $unique = @($records | Sort-Object module, class -Unique)
  Write-ClassList $allPath $unique

  $reference = Resolve-ReferenceFile
  if ($reference) {
    $passedKeys = @{}
    foreach ($row in (Import-ResultRows $reference)) {
      if (-not (Test-ReferenceVmRow $row)) { continue }
      $key = Get-ClassKey $row
      if (-not $key) { continue }
      $status = Get-RowStatus $row
      if ($status -eq 'PASS' -or $status -eq 'OK') { $passedKeys[$key] = $true }
    }

    $passed = New-Object System.Collections.Generic.List[object]
    $others = New-Object System.Collections.Generic.List[object]
    foreach ($record in $unique) {
      $key = "$($record.module)`t$($record.class)"
      if ($passedKeys.ContainsKey($key)) {
        $passed.Add($record)
      } else {
        $others.Add($record)
      }
    }
    Write-ClassList $passedPath $passed
    Write-ClassList $othersPath $others
    Write-Info "class lists refreshed: all=$($unique.Count), passed=$($passed.Count), others=$($others.Count), reference=$reference"
  } else {
    Write-Info "class list refreshed: all=$($unique.Count); no reference found for passed/others split"
  }
}

function Get-SelectedClasses {
  $allPath = Join-Path $script:WorkRoot 'all-tests.tsv'
  $passedPath = Join-Path $script:WorkRoot 'passed.tsv'
  $othersPath = Join-Path $script:WorkRoot 'others.tsv'

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
  $from = [Math]::Max(1, $Start) - 1
  if ($from -ge $list.Count) { return @() }
  $end = $list.Count
  if ($Count -gt 0) { $end = [Math]::Min($list.Count, $from + $Count) }
  return @($list[$from..($end - 1)])
}

function Resolve-CratonExe {
  if ($Exe) {
    if (-not (Test-Path $Exe)) { Die "CratonVM executable not found: $Exe" }
    return [System.IO.Path]::GetFullPath($Exe)
  }
  if ($env:CV_BIN -and (Test-Path $env:CV_BIN)) {
    return [System.IO.Path]::GetFullPath($env:CV_BIN)
  }
  if ($env:CRATONVM_ELASTICSEARCH_EXE -and (Test-Path $env:CRATONVM_ELASTICSEARCH_EXE)) {
    return [System.IO.Path]::GetFullPath($env:CRATONVM_ELASTICSEARCH_EXE)
  }

  $unique = Join-Path $script:RepoRoot 'target\release\cratonvm-elasticsearch-suite.exe'
  if (Test-Path $unique) { return [System.IO.Path]::GetFullPath($unique) }

  $generic = Join-Path $script:RepoRoot 'target\release\cratonvm.exe'
  if (Test-Path $generic) {
    New-Item -ItemType Directory -Force -Path (Split-Path $unique) | Out-Null
    Copy-Item -Path $generic -Destination $unique -Force
    Write-Info "copied generic cratonvm.exe to unique runner binary: $unique"
    return [System.IO.Path]::GetFullPath($unique)
  }

  Die "CratonVM executable not found. Pass -Exe or set CV_BIN/CRATONVM_ELASTICSEARCH_EXE."
}

function Resolve-Jdk {
  if ($JdkHome) { return $JdkHome }
  if ($env:JAVA_HOME) { return $env:JAVA_HOME }
  return 'C:\Program Files\Java\jdk-25'
}

function Get-Classpath([string]$Module) {
  $modulePath = $Module -replace '/', [System.IO.Path]::DirectorySeparatorChar
  $cpFile = Join-Path (Join-Path $script:ElasticsearchDir $modulePath) 'build\craton-testcp.txt'
  if (-not (Test-Path $cpFile)) { return '' }
  $entries = @(Get-Content -Path $cpFile | ForEach-Object { $_.Trim() } | Where-Object { $_ })
  if ($entries.Count -eq 0) { return '' }
  if ($entries.Count -eq 1) { return $entries[0] }
  return ($entries -join [System.IO.Path]::PathSeparator)
}

function Get-EsJavaArgs([bool]$HotSpot) {
  $esHome = [System.IO.Path]::GetFullPath($script:ElasticsearchDir)
  $args = @(
    "-Dtests.seed=$Seed",
    "-Des.path.home=$esHome",
    '-Djava.awt.headless=true',
    '-Djna.nosys=true',
    '-Dtests.logger.level=WARN',
    '-Dio.netty.noUnsafe=true',
    '-Dtests.testfeatures.enabled=true',
    '-Dtests.security.manager=false',
    '-Dtests.asserts=false',
    '-Dtests.timeoutSuite=580000!',
    '--add-opens=java.base/java.util=ALL-UNNAMED',
    '--add-opens=java.base/java.lang=ALL-UNNAMED',
    '--add-opens=java.base/java.security.cert=ALL-UNNAMED',
    '--add-opens=java.base/java.nio.channels=ALL-UNNAMED',
    '--add-opens=java.base/java.nio=ALL-UNNAMED',
    '--add-opens=java.base/java.net=ALL-UNNAMED',
    '--add-opens=java.base/javax.net.ssl=ALL-UNNAMED',
    '--add-opens=java.base/java.nio.file=ALL-UNNAMED',
    '--add-opens=java.base/java.time=ALL-UNNAMED',
    '--add-opens=java.management/java.lang.management=ALL-UNNAMED',
    '--add-opens=java.base/jdk.internal.misc=ALL-UNNAMED',
    '--enable-native-access=ALL-UNNAMED',
    '--add-modules=jdk.incubator.vector'
  )
  if ($HotSpot) {
    return @("-Xmx$MaxHeap") + $args
  }
  return $args
}

function New-ProcessRecord {
  param(
    [object]$ClassRow,
    [string]$ModeOut,
    [string]$ExePath,
    [string]$JavaExe,
    [string]$JdkPath,
    [bool]$NoJit
  )

  $module = [string]$ClassRow.module
  $class = [string]$ClassRow.class
  $cp = Get-Classpath $module
  if (-not $cp) {
    return [pscustomobject]@{
      noCp = $true
      module = $module
      class = $class
    }
  }

  $safe = Get-LogBaseName -Module $module -Class $class
  $logDir = Join-Path $ModeOut 'logs'
  $outFile = Join-Path $logDir "$safe.out.log"
  $errFile = Join-Path $logDir "$safe.err.log"
  # Windows PowerShell 5.1/.NET Framework still hits MAX_PATH for a normal
  # class name when callers use a long worktree, work directory, or run name.
  # Keep the documented per-mode layout when it fits; otherwise use a compact
  # deterministic subdirectory under the work root.  Include ModeOut in the
  # hash so repeated runs retain separate logs instead of overwriting them.
  if ($env:OS -eq 'Windows_NT' -and $outFile.Length -ge 240) {
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try {
      $key = [System.Text.Encoding]::UTF8.GetBytes("$ModeOut`t$module`t$class")
      $hash = ([System.BitConverter]::ToString($sha.ComputeHash($key)) -replace '-', '').Substring(0, 16).ToLowerInvariant()
    } finally {
      $sha.Dispose()
    }
    $logDir = Join-Path (Join-Path $script:WorkRoot 'logs') $hash
    $outFile = Join-Path $logDir "$safe.out.log"
    $errFile = Join-Path $logDir "$safe.err.log"
  }
  New-Item -ItemType Directory -Force -Path $logDir | Out-Null

  if ($Vm -eq 'hotspot') {
    $file = $JavaExe
    $args = Get-EsJavaArgs $true
    if ($NoJit) { $args += '-Xint' }
    $args += @('-cp', $cp, 'org.junit.runner.JUnitCore', $class)
  } else {
    $file = $ExePath
    $args = @('--java-home', $JdkPath, '--stack-dump-on-timeout', '0', '--Xmx', $MaxHeap)
    if ($NoJit) { $args += '--nojit' }
    if ($CratonArgs.Count -gt 0) { $args += $CratonArgs }
    $args += Get-EsJavaArgs $false
    $args += @('-cp', $cp, 'org.junit.runner.JUnitCore', $class)
  }

  $psi = [System.Diagnostics.ProcessStartInfo]::new()
  $psi.FileName = $file
  $psi.WorkingDirectory = $script:ElasticsearchDir
  $psi.UseShellExecute = $false
  $psi.CreateNoWindow = $true
  $psi.RedirectStandardOutput = $true
  $psi.RedirectStandardError = $true
  Set-ProcessArguments -StartInfo $psi -Arguments $args
  [void]$psi.EnvironmentVariables.Set_Item('CRATONVM_DISABLE_DEFAULT_WATCHDOG', '1')

  $proc = [System.Diagnostics.Process]::new()
  $proc.StartInfo = $psi
  [void]$proc.Start()

  return [pscustomobject]@{
    noCp = $false
    proc = $proc
    stdoutTask = $proc.StandardOutput.ReadToEndAsync()
    stderrTask = $proc.StandardError.ReadToEndAsync()
    module = $module
    class = $class
    start = Get-Date
    outFile = $outFile
    errFile = $errFile
  }
}

function Get-RunClassification {
  param(
    [string]$Combined,
    [object]$ExitCode,
    [bool]$TimedOut
  )

  $status = 'NOSUMMARY'
  $tests = 0
  $failed = 0

  $crashPattern = '(?i)EXCEPTION_ACCESS_VIOLATION|STATUS_ACCESS_VIOLATION|SIGSEGV|fatal runtime error|panicked at|thread ''.*'' panicked|internal error:|not yet implemented|illegal instruction|STATUS_STACK_BUFFER|caught fatal signal|cratonvm panic|entered unreachable'
  if ($TimedOut -or [string]$ExitCode -eq 'TIMEOUT') {
    $status = 'HANG'
  } elseif ($Combined -match $crashPattern) {
    $status = 'CRASH'
  }

  $ok = [regex]::Match($Combined, 'OK \((\d+) tests?\)')
  if ($ok.Success) {
    $tests = [int]$ok.Groups[1].Value
  } else {
    $run = [regex]::Match($Combined, 'Tests run:\s*(\d+),\s*Failures:\s*(\d+)')
    if ($run.Success) {
      $tests = [int]$run.Groups[1].Value
      $failed = [int]$run.Groups[2].Value
    }
  }

  if ($status -eq 'NOSUMMARY') {
    if ($ExitCode -eq 0) {
      $status = 'PASS'
    } elseif ($ExitCode -eq 1) {
      $status = 'FAIL'
    } elseif ($tests -gt 0) {
      $status = 'FAIL'
    } else {
      $status = 'CRASH'
    }
  }

  return [pscustomobject]@{ status = $status; tests = $tests; failed = $failed }
}

function Complete-ProcessRecord {
  param(
    [object]$Record,
    [string]$ResultPath,
    [object]$ExitCode,
    [double]$Seconds,
    [bool]$TimedOut
  )

  if ($Record.noCp) {
    $line = @(
      $script:ResultIndex,
      $Record.module,
      $Record.class,
      $Vm,
      $(if ($Jit -eq 'off') { 'off' } else { 'on' }),
      'NOCP',
      'NOCP',
      (ConvertTo-InvariantString 0),
      0,
      0,
      '',
      '',
      'missing module build\craton-testcp.txt'
    ) -join "`t"
    Add-ContentWithRetry -Path $ResultPath -Value $line
    $script:ResultIndex++
    Write-Host ("  [{0}] {1,-9} {2,7:N1}s {3}" -f $script:EffectiveModeName, 'NOCP', 0, $Record.class)
    return
  }

  try { $Record.stdoutTask.Wait(5000) | Out-Null } catch {}
  try { $Record.stderrTask.Wait(5000) | Out-Null } catch {}
  $stdout = ''
  $stderr = ''
  try { $stdout = $Record.stdoutTask.Result } catch {}
  try { $stderr = $Record.stderrTask.Result } catch {}
  foreach ($path in @($Record.outFile, $Record.errFile)) {
    $parent = Split-Path -Parent $path
    if ($parent) { New-Item -ItemType Directory -Force -Path $parent | Out-Null }
  }
  [System.IO.File]::WriteAllText($Record.outFile, $stdout, [System.Text.Encoding]::UTF8)
  [System.IO.File]::WriteAllText($Record.errFile, $stderr, [System.Text.Encoding]::UTF8)

  $combined = "$stdout`n$stderr"
  $classInfo = Get-RunClassification -Combined $combined -ExitCode $ExitCode -TimedOut $TimedOut

  $note = ''
  $noteMatch = [regex]::Match($combined, '(?im)^(?!\s+at\s)(.*(?:Exception|Error|Caused by|AssertionError|panicked|not implemented|NoClassDef|NoSuchMethod|AbstractMethod|EXCEPTION_ACCESS).*)$')
  if ($noteMatch.Success) {
    $note = (($noteMatch.Groups[1].Value -replace "`t", ' ') -replace "`r|`n", ' ')
    if ($note.Length -gt 180) { $note = $note.Substring(0, 180) }
  }
  if ($classInfo.status -eq 'PASS') { $note = '' }

  $line = @(
    $script:ResultIndex,
    $Record.module,
    $Record.class,
    $Vm,
    $(if ($Jit -eq 'off') { 'off' } else { 'on' }),
    $ExitCode,
    $classInfo.status,
    (ConvertTo-InvariantString $Seconds),
    $classInfo.tests,
    $classInfo.failed,
    $Record.outFile,
    $Record.errFile,
    $note
  ) -join "`t"
  Add-ContentWithRetry -Path $ResultPath -Value $line
  $script:ResultIndex++

  Write-Host ("  [{0}] {1,-9} {2,7:N1}s {3}" -f $script:EffectiveModeName, $classInfo.status, $Seconds, $Record.class)
}

function Wait-OneRunning {
  param(
    [System.Collections.ArrayList]$Running,
    [string]$Results
  )
  while ($true) {
    for ($i = 0; $i -lt $Running.Count; $i++) {
      $record = $Running[$i]
      if ($record.noCp) {
        Complete-ProcessRecord -Record $record -ResultPath $Results -ExitCode 'NOCP' -Seconds 0 -TimedOut $false
        $Running.RemoveAt($i)
        return
      }

      $elapsed = ((Get-Date) - $record.start).TotalSeconds
      $timedOut = $false
      if (-not $record.proc.HasExited -and $elapsed -ge $TimeoutSec) {
        $timedOut = $true
        try { $record.proc.Kill($true) } catch { try { $record.proc.Kill() } catch {} }
      }
      if ($record.proc.HasExited -or $timedOut) {
        if (-not $timedOut) {
          $exit = $record.proc.ExitCode
        } else {
          $exit = 'TIMEOUT'
        }
        if (-not $timedOut) {
          $elapsed = ((Get-Date) - $record.start).TotalSeconds
        }
        Complete-ProcessRecord -Record $record -ResultPath $Results -ExitCode $exit -Seconds $elapsed -TimedOut $timedOut
        try { $record.proc.Dispose() } catch {}
        $Running.RemoveAt($i)
        return
      }
    }
    Start-Sleep -Milliseconds 200
  }
}

function Invoke-Mode {
  param([object[]]$Classes)

  $jdk = Resolve-Jdk
  # `$IsWindows` is only defined by PowerShell 6+.  This runner's documented
  # invocation uses Windows PowerShell 5.1 (`powershell.exe`), where that
  # unset variable silently selected the Unix `bin/java` path and prevented
  # every local run before a JVM could start.
  $javaName = if ($env:OS -eq 'Windows_NT') { 'bin\java.exe' } else { 'bin/java' }
  $java = Join-Path $jdk $javaName
  if (-not (Test-Path $java)) { Die "HotSpot java not found: $java" }
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
    "index`tmodule`tclass`tvm`tjit`trc`tstatus`tseconds`ttests`tfailed`tstdoutLog`tstderrLog`tnote" | Set-Content -Path $results -Encoding ascii
  }

  $done = @{}
  foreach ($row in (Import-Csv -Path $results -Delimiter "`t")) {
    if ($row.module -and $row.class) { $done["$($row.module)`t$($row.class)"] = $true }
  }
  $todo = @($Classes | Where-Object { -not $done.ContainsKey("$($_.module)`t$($_.class)") })

  Write-Info "mode=$mode vm=$Vm jit=$Jit category=$Category selected=$($Classes.Count) todo=$($todo.Count) parallel=$Parallel timeout=${TimeoutSec}s"
  if ($Vm -eq 'craton') { Write-Info "craton exe=$craton" }
  Write-Info "logs/results=$modeOut"

  $script:ResultIndex = @((Import-Csv -Path $results -Delimiter "`t")).Count + 1
  $running = [System.Collections.ArrayList]::new()
  $started = Get-Date

  foreach ($class in $todo) {
    while ($running.Count -ge [Math]::Max(1, $Parallel)) {
      Wait-OneRunning -Running $running -Results $results
    }
    $record = New-ProcessRecord -ClassRow $class -ModeOut $modeOut -ExePath $craton -JavaExe $java -JdkPath $jdk -NoJit:($Jit -eq 'off')
    [void]$running.Add($record)
  }

  while ($running.Count -gt 0) {
    Wait-OneRunning -Running $running -Results $results
  }

  $wall = ((Get-Date) - $started).TotalSeconds
  Write-Summary -ResultsPath $results -ModeOut $modeOut -WallSeconds $wall

  if ($Vm -eq 'hotspot') {
    $baselineDir = Join-Path $script:WorkRoot 'baseline'
    New-Item -ItemType Directory -Force -Path $baselineDir | Out-Null
    Copy-Item -Path $results -Destination (Join-Path $baselineDir "hotspot-baseline-$run.tsv") -Force
    Copy-Item -Path $results -Destination (Join-Path $baselineDir 'hotspot-baseline-latest.tsv') -Force
    Copy-Item -Path (Join-Path $modeOut 'summary.md') -Destination (Join-Path $baselineDir "hotspot-baseline-$run.md") -Force
    Copy-Item -Path (Join-Path $modeOut 'summary.md') -Destination (Join-Path $baselineDir 'hotspot-baseline-latest.md') -Force
  }
}

function Write-Summary {
  param(
    [string]$ResultsPath,
    [string]$ModeOut,
    [double]$WallSeconds
  )
  $rows = @(Import-Csv -Path $ResultsPath -Delimiter "`t")
  $summary = Join-Path $ModeOut 'summary.md'
  $counts = $rows | Group-Object status | Sort-Object Name
  $sumSeconds = 0.0
  foreach ($row in $rows) { $sumSeconds += ConvertFrom-InvariantString $row.seconds }

  $lines = New-Object System.Collections.Generic.List[string]
  $lines.Add('# Elasticsearch suite result')
  $lines.Add('')
  $lines.Add("- run: $RunName")
  $lines.Add("- mode: $script:EffectiveModeName")
  $lines.Add("- vm: $Vm")
  $lines.Add("- jit: $Jit")
  $lines.Add("- category: $Category")
  $lines.Add("- classes recorded: $($rows.Count)")
  $lines.Add("- wall seconds: $(ConvertTo-InvariantString $WallSeconds)")
  $lines.Add("- summed class seconds: $(ConvertTo-InvariantString $sumSeconds)")
  $lines.Add("- elasticsearch root: $script:ElasticsearchDir")
  if ($Vm -eq 'craton') { $lines.Add("- craton exe: $(Resolve-CratonExe)") }
  $lines.Add('')
  $lines.Add('## Status Counts')
  if ($counts.Count -eq 0) {
    $lines.Add('- none')
  } else {
    foreach ($count in $counts) { $lines.Add("- $($count.Name): $($count.Count)") }
  }
  $lines.Add('')
  $lines.Add('## Files')
  $lines.Add("- results: $ResultsPath")
  $lines.Add("- logs: $(Join-Path $ModeOut 'logs')")
  [System.IO.File]::WriteAllLines($summary, $lines, [System.Text.Encoding]::UTF8)
  Write-Info "DONE mode=$script:EffectiveModeName wall=$(ConvertTo-InvariantString $WallSeconds)s summary=$summary"
}

function Invoke-AllModes {
  $run = $RunName
  if (-not $run) { $run = Get-Date -Format 'yyyyMMdd-HHmmss' }
  $ps = Get-PowerShellExe
  $modes = @(
    @{ category='passed'; jit='on';  name='passed-jit' },
    @{ category='passed'; jit='off'; name='passed-nojit' },
    @{ category='others'; jit='on';  name='others-jit' },
    @{ category='others'; jit='off'; name='others-nojit' }
  )
  $children = @()
  foreach ($m in $modes) {
    $consoleDir = Join-Path (Join-Path (Join-Path $script:WorkRoot 'results') $run) $m.name
    New-Item -ItemType Directory -Force -Path $consoleDir | Out-Null
    $stdout = Join-Path (Split-Path $consoleDir -Parent) "$($m.name).console.out.log"
    $stderr = Join-Path (Split-Path $consoleDir -Parent) "$($m.name).console.err.log"
    $args = @(
      '-NoProfile','-ExecutionPolicy','Bypass',
      '-File', $PSCommandPath,
      '-Category', $m.category,
      '-Jit', $m.jit,
      '-Vm', $Vm,
      '-Start', [string]$Start,
      '-Count', [string]$Count,
      '-Parallel', [string]$Parallel,
      '-TimeoutSec', [string]$TimeoutSec,
      '-RunName', $run,
      '-ModeName', $m.name,
      '-ElasticsearchRoot', $script:ElasticsearchDir,
      '-WorkDir', $script:WorkRoot,
      '-MaxHeap', $MaxHeap,
      '-Seed', $Seed
    )
    if ($RefCsv) { $args += @('-RefCsv', $RefCsv) }
    if ($Exe) { $args += @('-Exe', $Exe) }
    if ($JdkHome) { $args += @('-JdkHome', $JdkHome) }
    foreach ($arg in $CratonArgs) { $args += @('-CratonArgs', $arg) }
    $children += Start-RedirectedProcess -FilePath $ps -Arguments $args -WorkingDirectory $script:RepoRoot -StdoutPath $stdout -StderrPath $stderr
    Write-Info "started mode=$($m.name)"
  }
  $failed = 0
  foreach ($child in $children) {
    $rc = Complete-RedirectedProcess $child
    if ($rc -ne 0) { $failed++ }
  }
  if ($failed -gt 0) { Die "$failed AllModes child process(es) failed" }
}

$script:RepoRoot = Get-RepoRoot
if (-not $ElasticsearchRoot) {
  $repoCandidate = Join-Path $script:RepoRoot 'apps\elasticsearch'
  if (Test-Path $repoCandidate) {
    $ElasticsearchRoot = $repoCandidate
  } elseif (Test-Path 'C:\craton\CratonVM\apps\elasticsearch') {
    $ElasticsearchRoot = 'C:\craton\CratonVM\apps\elasticsearch'
  } else {
    $ElasticsearchRoot = $repoCandidate
  }
}
$script:ElasticsearchDir = [System.IO.Path]::GetFullPath($ElasticsearchRoot)
if (-not (Test-Path $script:ElasticsearchDir)) { Die "Elasticsearch root not found: $script:ElasticsearchDir" }

if ($SkipNativeFixtureCheck) {
  Write-Info 'SKIPPING native libvec fixture check by explicit request'
} else {
  Assert-LinuxX64VectorFixture $script:ElasticsearchDir
}

if (-not $WorkDir) { $WorkDir = Join-Path $PSScriptRoot '.suite' }
$script:WorkRoot = [System.IO.Path]::GetFullPath($WorkDir)
New-Item -ItemType Directory -Force -Path $script:WorkRoot | Out-Null

if ($Parallel -lt 1) { $Parallel = 1 }
if ($TimeoutSec -lt 1) { $TimeoutSec = 1 }

if ($AllModes) {
  if ($RefreshLists -or -not (Test-Path (Join-Path $script:WorkRoot 'all-tests.tsv'))) {
    Build-ClassLists
  }
  Invoke-AllModes
  exit 0
}

$classes = @(Get-SelectedClasses)
Write-Info "selected $($classes.Count) classes (category=$Category start=$Start count=$Count)"

if ($ListOnly) {
  $i = 0
  foreach ($class in $classes) {
    $i++
    "{0}`t{1}`t{2}" -f $i, $class.module, $class.class
  }
  exit 0
}

Invoke-Mode -Classes $classes
