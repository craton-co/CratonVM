# bench/wave2-4/stage-jacoco-probe.ps1
# WP2.4-D - Windows PowerShell 5.1 stager for JaCoCo coverage probe.
# Mirrors stage-jacoco-probe.sh.

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$Here       = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot   = Resolve-Path (Join-Path $Here '..\..')
$StagedDir  = Join-Path $Here 'staged-jacoco'
$ClassesDir = Join-Path $StagedDir 'classes'
$CompileLog = Join-Path $StagedDir 'compile.log'
$MainFile   = Join-Path $StagedDir 'main-class.txt'
$JarPathFile= Join-Path $StagedDir 'jar-path.txt'
$SkipFlag   = Join-Path $StagedDir 'skipped.flag'
$SrcDir     = Join-Path $RepoRoot 'apps\jacoco_probe'

New-Item -ItemType Directory -Force -Path $ClassesDir | Out-Null
Set-Content -Path $CompileLog -Value '' -Encoding utf8
if (Test-Path $SkipFlag) { Remove-Item $SkipFlag -Force }

function Invoke-Native {
    param([string]$Exe, [string[]]$ArgList, [string]$LogPath)
    $quoted = foreach ($a in $ArgList) {
        if ($a -match '[\s;]') { '"' + $a.Replace('"','\"') + '"' } else { $a }
    }
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $Exe
    $psi.Arguments = ($quoted -join ' ')
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError  = $true
    $psi.UseShellExecute = $false
    $p = [System.Diagnostics.Process]::Start($psi)
    $stdout = $p.StandardOutput.ReadToEnd()
    $stderr = $p.StandardError.ReadToEnd()
    $p.WaitForExit()
    Add-Content -Path $LogPath -Value $stdout -Encoding utf8
    Add-Content -Path $LogPath -Value $stderr -Encoding utf8
    return $p.ExitCode
}

# Locate javac.
$Javac = $null
if ($env:JAVA_HOME) {
    $c = Join-Path $env:JAVA_HOME 'bin\javac.exe'; if (Test-Path $c) { $Javac = $c }
}
if (-not $Javac) {
    $cmd = Get-Command javac -ErrorAction SilentlyContinue
    if ($cmd) { $Javac = $cmd.Source }
}
foreach ($root in @('C:\Program Files\Java\jdk-25', 'C:\Program Files\Java\jdk-21')) {
    if (-not $Javac) {
        $c = Join-Path $root 'bin\javac.exe'; if (Test-Path $c) { $Javac = $c }
    }
}
if (-not $Javac) { Write-Error "stage-jacoco-probe: javac not found"; exit 10 }
Add-Content -Path $CompileLog -Value "stage-jacoco-probe: using javac at $Javac" -Encoding utf8

# Locate jacocoagent.jar.
$JarPath = $null
$candidates = @()
if ($env:JACOCO_AGENT_JAR -and (Test-Path $env:JACOCO_AGENT_JAR)) { $candidates += $env:JACOCO_AGENT_JAR }
$candidates += 'C:\craton\ejbca-ce\lib\coverage\jacocoagent.jar'

# Search local maven for org.jacoco.agent runtime jar.
foreach ($base in @($env:USERPROFILE, 'C:\Users\Victor')) {
    if (-not $base) { continue }
    $root = Join-Path $base '.m2\repository\org\jacoco\org.jacoco.agent'
    if (Test-Path $root) {
        $found = Get-ChildItem -Path $root -Recurse -ErrorAction SilentlyContinue -Filter '*.jar' |
                 Where-Object { $_.Name -match 'runtime' -and $_.Name -notmatch 'sources' -and $_.Name -notmatch 'javadoc' } |
                 Select-Object -First 1
        if ($found) { $candidates += $found.FullName }
    }
}

foreach ($cand in $candidates) {
    if (Test-Path $cand) { $JarPath = $cand; break }
}

if (-not $JarPath) {
    Add-Content -Path $CompileLog -Value "stage-jacoco-probe: SKIP jacocoagent.jar not found" -Encoding utf8
    Add-Content -Path $CompileLog -Value "  searched: \$env:JACOCO_AGENT_JAR, C:\craton\ejbca-ce\lib\coverage, ~/.m2/repository/org/jacoco" -Encoding utf8
    Set-Content -Path $SkipFlag -Value 'no-jacoco-agent' -Encoding utf8
    Write-Output "stage-jacoco-probe: SKIP jacoco agent jar not found"
}

# Compile probes (no jar needed at compile time).
$Sources = @((Join-Path $SrcDir 'Target.java'), (Join-Path $SrcDir 'Main.java'))
Add-Content -Path $CompileLog -Value "stage-jacoco-probe: compiling $($Sources.Count) sources" -Encoding utf8
$rc = Invoke-Native -Exe $Javac -ArgList (@('--release','21','-d',$ClassesDir) + $Sources) -LogPath $CompileLog
if ($rc -ne 0) {
    Write-Error "stage-jacoco-probe: javac failed (rc=$rc); see $CompileLog"
    exit 12
}

if ($JarPath) {
    Add-Content -Path $CompileLog -Value "stage-jacoco-probe: located jacoco agent at $JarPath" -Encoding utf8
    Set-Content -Path $JarPathFile -Value $JarPath -Encoding utf8
    Copy-Item $JarPath (Join-Path $StagedDir 'jacocoagent.jar') -Force
}

Set-Content -Path $MainFile -Value 'Main' -Encoding utf8
Add-Content -Path $CompileLog -Value "stage-jacoco-probe: main class = Main" -Encoding utf8
Add-Content -Path $CompileLog -Value "stage-jacoco-probe: staged classes at $ClassesDir" -Encoding utf8
Write-Output "stage-jacoco-probe: OK (classes=$ClassesDir, jar=$JarPath)"
exit 0
