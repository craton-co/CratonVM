# Doc 23 regression harness.
#
# Runs the org.apache.tomcat.util.{buf,collections,http} and
# org.apache.catalina.util test classes under two CratonVM binaries and
# reports any class whose verdict differs. These are the classes doc 23's
# change set touches the code paths of; the point is that nothing else in them
# moves.
#
#   pwsh probes\run-doc23-regression.ps1 -BaseExe <a.exe> -FixExe <b.exe>
[CmdletBinding()]
param(
  [Parameter(Mandatory=$true)][string]$BaseExe,
  [Parameter(Mandatory=$true)][string]$FixExe,
  [string]$JdkHome    = 'C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot',
  [string]$CpFile     = 'C:\craton\CratonVM\apps\tomcat\.suite\cp.txt',
  [string]$ListFile   = 'C:\craton\CratonVM\apps\tomcat\.suite\all-tests.txt',
  [string]$Filter     = 'org\.apache\.tomcat\.util\.(buf|collections|http)\.|org\.apache\.catalina\.util\.',
  [int]   $TimeoutSec = 300,
  [string]$MaxHeap    = '2g',
  [string]$OutDir     = ''
)

$ErrorActionPreference = 'Stop'
$cp      = (Get-Content $CpFile -Raw).Trim()
$classes = Get-Content $ListFile | Where-Object { $_ -match $Filter }
if (-not $OutDir) { $OutDir = Join-Path $env:TEMP ("doc23-regression-" + (Get-Random)) }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

Write-Host "[doc23] $($classes.Count) classes, logs in $OutDir"

function Invoke-Class([string]$exe, [string]$cls, [string]$tag) {
  $log = Join-Path $OutDir "$tag.$cls.log"
  # `Start-Process -ArgumentList` joins with spaces and quotes nothing, so any
  # argument containing one (the JDK home, the classpath) must be quoted here
  # or it arrives as several arguments.
  $quoted = @('--java-home', """$JdkHome""", "-Xmx$MaxHeap", '-cp', """$cp""",
              'org.junit.runner.JUnitCore', $cls)
  $p = Start-Process -FilePath $exe -PassThru -NoNewWindow -RedirectStandardOutput $log `
        -RedirectStandardError "$log.err" -ArgumentList $quoted
  if (-not $p.WaitForExit($TimeoutSec * 1000)) {
    try { $p.Kill() } catch {}
    return 'TIMEOUT'
  }
  # The timed overload can return before the exit code is populated; the
  # argument-less wait settles it (and flushes the redirected streams).
  $p.WaitForExit()
  $text = (Get-Content $log -Raw -ErrorAction SilentlyContinue)
  if ($null -eq $text) { $text = '' }
  if ($text -match '(?m)^OK \(\d+ test')          { return 'PASS' }
  if ($text -match '(?m)^Tests run: .*Failures')  { return 'FAIL' }
  if ($p.ExitCode -ne 0)                          { return "EXIT$($p.ExitCode)" }
  return 'UNKNOWN'
}

$rows = @()
foreach ($cls in $classes) {
  $b = Invoke-Class $BaseExe $cls 'base'
  $f = Invoke-Class $FixExe  $cls 'fix'
  $flag = if ($b -eq $f) { '   ' } else { '<<<' }
  Write-Host ("{0,-8} {1,-8} {2} {3}" -f $b, $f, $flag, $cls)
  $rows += [pscustomobject]@{ Class = $cls; Base = $b; Fix = $f; Same = ($b -eq $f) }
}

# `@()` matters: a single differing row is not a collection in PS 5.1, so
# `$diff.Count` would be empty and the summary would silently claim 0 differ.
$diff = @($rows | Where-Object { -not $_.Same })
Write-Host ""
Write-Host ("[doc23] {0}/{1} classes identical; {2} differ" -f ($rows.Count - $diff.Count), $rows.Count, $diff.Count)
if ($diff) { $diff | Format-Table -AutoSize }
$rows | Export-Csv -NoTypeInformation -Path (Join-Path $OutDir 'summary.csv')
Write-Host "[doc23] summary: $(Join-Path $OutDir 'summary.csv')"
