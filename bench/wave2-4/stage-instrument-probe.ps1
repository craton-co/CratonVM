# bench/wave2-4/stage-instrument-probe.ps1
# WP2.4-D - Windows PowerShell 5.1 stager for the hand-rolled
# `-javaagent:` regression test. Mirrors stage-instrument-probe.sh.
#
# Builds:
#   staged-instrument/classes/{Target,RetransformAgent,Main}.class
#   staged-instrument/agent.jar with Premain-Class: RetransformAgent

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$Here       = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot   = Resolve-Path (Join-Path $Here '..\..')
$StagedDir  = Join-Path $Here 'staged-instrument'
$ClassesDir = Join-Path $StagedDir 'classes'
$CompileLog = Join-Path $StagedDir 'compile.log'
$MainFile   = Join-Path $StagedDir 'main-class.txt'
$AgentJar   = Join-Path $StagedDir 'agent.jar'
$SrcDir     = Join-Path $RepoRoot 'apps\instrument_probe'
$ManifestSrc= Join-Path $SrcDir 'META-INF\MANIFEST.MF'

New-Item -ItemType Directory -Force -Path $ClassesDir | Out-Null
Set-Content -Path $CompileLog -Value '' -Encoding utf8
if (Test-Path $AgentJar)             { Remove-Item $AgentJar -Force }
$skipFlag = Join-Path $StagedDir 'skipped.flag'
if (Test-Path $skipFlag) { Remove-Item $skipFlag -Force }

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

# Locate javac + jar.
$Javac = $null
$Jar   = $null
if ($env:JAVA_HOME) {
    $cj = Join-Path $env:JAVA_HOME 'bin\javac.exe'; if (Test-Path $cj) { $Javac = $cj }
    $cr = Join-Path $env:JAVA_HOME 'bin\jar.exe';   if (Test-Path $cr) { $Jar   = $cr }
}
if (-not $Javac) {
    $cmd = Get-Command javac -ErrorAction SilentlyContinue
    if ($cmd) { $Javac = $cmd.Source }
}
if (-not $Jar) {
    $cmd = Get-Command jar -ErrorAction SilentlyContinue
    if ($cmd) { $Jar = $cmd.Source }
}
foreach ($root in @('C:\Program Files\Java\jdk-25', 'C:\Program Files\Java\jdk-21')) {
    if (-not $Javac) {
        $c = Join-Path $root 'bin\javac.exe'; if (Test-Path $c) { $Javac = $c }
    }
    if (-not $Jar) {
        $c = Join-Path $root 'bin\jar.exe';   if (Test-Path $c) { $Jar   = $c }
    }
}
if (-not $Javac) { Write-Error "stage-instrument-probe: javac not found"; exit 10 }
if (-not $Jar)   { Write-Error "stage-instrument-probe: jar not found (need JDK, not JRE)"; exit 10 }
Add-Content -Path $CompileLog -Value "stage-instrument-probe: javac=$Javac" -Encoding utf8
Add-Content -Path $CompileLog -Value "stage-instrument-probe: jar=$Jar"     -Encoding utf8

# Compile.
$Sources = @(
    (Join-Path $SrcDir 'Target.java'),
    (Join-Path $SrcDir 'RetransformAgent.java'),
    (Join-Path $SrcDir 'Main.java')
)
Add-Content -Path $CompileLog -Value "stage-instrument-probe: compiling $($Sources.Count) sources" -Encoding utf8
$rc = Invoke-Native -Exe $Javac -ArgList (@('--release','21','-d',$ClassesDir) + $Sources) -LogPath $CompileLog
if ($rc -ne 0) {
    Write-Error "stage-instrument-probe: javac failed (rc=$rc); see $CompileLog"
    exit 12
}

if (-not (Test-Path $ManifestSrc)) {
    Write-Error "stage-instrument-probe: missing manifest at $ManifestSrc"
    exit 12
}

# Build agent jar. The two .class files we need to bundle are the
# top-level RetransformAgent + the Patcher inner class.
Add-Content -Path $CompileLog -Value "stage-instrument-probe: building agent jar at $AgentJar" -Encoding utf8
$jarArgs = @(
    'cfm', $AgentJar, $ManifestSrc,
    '-C', $ClassesDir, 'RetransformAgent.class',
    '-C', $ClassesDir, 'RetransformAgent$Patcher.class'
)
$rc2 = Invoke-Native -Exe $Jar -ArgList $jarArgs -LogPath $CompileLog
if ($rc2 -ne 0) {
    Write-Error "stage-instrument-probe: jar packaging failed (rc=$rc2); see $CompileLog"
    exit 12
}

# Sanity: list jar contents.
Add-Content -Path $CompileLog -Value "stage-instrument-probe: agent jar contents:" -Encoding utf8
$null = Invoke-Native -Exe $Jar -ArgList @('tf', $AgentJar) -LogPath $CompileLog

Set-Content -Path $MainFile -Value 'Main' -Encoding utf8
Add-Content -Path $CompileLog -Value "stage-instrument-probe: main class = Main" -Encoding utf8
Add-Content -Path $CompileLog -Value "stage-instrument-probe: staged classes at $ClassesDir" -Encoding utf8
Write-Output "stage-instrument-probe: OK (agent=$AgentJar, classes=$ClassesDir)"
exit 0
