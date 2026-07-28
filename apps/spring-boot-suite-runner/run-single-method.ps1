param(
  [Parameter(Mandatory=$true)][string]$Module,
  [Parameter(Mandatory=$true)][string]$ClassName,
  [Parameter(Mandatory=$true)][string]$Method,
  [string]$SpringBootRoot = 'C:\craton\CratonVM\apps\spring-boot',
  [string]$Exe = 'C:\craton\CratonVM-pem-clientauth-decrypterror-20260720\target\release\cratonvm-pem-clientauth-decrypterror.exe',
  [string]$JdkHome = 'C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot',
  [string]$MaxHeap = '2g',
  [int]$StackDumpTimeoutSec = 0,
  [switch]$NoJit,
  [string[]]$CratonArgs = @(),
  [hashtable]$ExtraEnv = @{}
)

$ErrorActionPreference = 'Stop'
$SbRunnerDir = Join-Path $SpringBootRoot 'sb-runner'
$modulePath = $Module -replace '/', '\'
$moduleRoot = Join-Path $SpringBootRoot $modulePath
$cpFile = Join-Path $moduleRoot 'build\cratonvm-test-cp.txt'
if (-not (Test-Path $cpFile)) { throw "missing classpath file: $cpFile" }

function Add-UniqueEntry($list, $seen, $entry) {
  if (-not $entry) { return }
  $full = [System.IO.Path]::GetFullPath($entry)
  $key = $full.ToLowerInvariant()
  if (-not $seen.ContainsKey($key)) { $list.Add($full); $seen[$key] = $true }
}

function Find-GradleCacheJar([string]$Group, [string]$Artifact) {
  $root = Join-Path $env:USERPROFILE '.gradle\caches\modules-2\files-2.1'
  $artifactDir = Join-Path $root "$Group\$Artifact"
  if (-not (Test-Path $artifactDir)) { return $null }
  return Get-ChildItem -Path $artifactDir -Recurse -File -Filter "$Artifact-*.jar" -ErrorAction SilentlyContinue |
    Where-Object { $_.Name -notmatch '(-sources|-javadoc)\.jar$' } |
    Sort-Object FullName -Descending | Select-Object -First 1
}

$entries = New-Object System.Collections.Generic.List[string]
$seen = @{}
Add-UniqueEntry $entries $seen $SbRunnerDir
$separator = [string][System.IO.Path]::PathSeparator
foreach ($e in ((Get-Content -Path $cpFile -Raw).Trim() -split [regex]::Escape($separator))) {
  if ($e) { Add-UniqueEntry $entries $seen $e }
}
$infra = @(
  @('org.junit.platform', 'junit-platform-commons'),
  @('org.junit.platform', 'junit-platform-engine'),
  @('org.junit.platform', 'junit-platform-launcher'),
  @('org.junit.jupiter', 'junit-jupiter-api'),
  @('org.junit.jupiter', 'junit-jupiter-engine'),
  @('org.junit.vintage', 'junit-vintage-engine'),
  @('org.opentest4j', 'opentest4j'),
  @('org.apiguardian', 'apiguardian-api'),
  @('junit', 'junit')
)
foreach ($pair in $infra) {
  $jar = Find-GradleCacheJar -Group $pair[0] -Artifact $pair[1]
  if ($jar) { Add-UniqueEntry $entries $seen $jar.FullName }
}

$cp = ($entries -join $separator)
$exePath = [System.IO.Path]::GetFullPath($Exe)
if (-not (Test-Path $exePath)) { throw "CratonVM exe not found: $exePath" }

$env:CRATONVM_REAL = 'net-sockets,aqs'
$env:CRATONVM_THREADS = '-default-watchdog'
$env:CRATONVM_JIT = 'rootsnap-cache'
foreach ($k in $ExtraEnv.Keys) { Set-Item -Path "env:$k" -Value $ExtraEnv[$k] }

$args = @('--java-home', $JdkHome, '--Xmx', $MaxHeap, '--stack-dump-on-timeout', $StackDumpTimeoutSec)
if ($NoJit) { $args += '--nojit' }
if ($CratonArgs.Count -gt 0) { $args += $CratonArgs }
$args += @('-Dfile.encoding=UTF-8', '-Djava.awt.headless=true', '-cp', $cp, 'SbRunnerMethod', $ClassName, $Method)

Push-Location $moduleRoot
try {
  & $exePath @args
  Write-Output "EXITCODE=$LASTEXITCODE"
} finally {
  Pop-Location
}
