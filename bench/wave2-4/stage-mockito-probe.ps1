# bench/wave2-4/stage-mockito-probe.ps1
# WP2.4-D - Windows PowerShell 5.1 stager for the Mockito MockMaker
# acceptance probe. Mirrors stage-mockito-probe.sh.

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$Here       = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot   = Resolve-Path (Join-Path $Here '..\..')
$StagedDir  = Join-Path $Here 'staged-mockito'
$ClassesDir = Join-Path $StagedDir 'classes'
$CompileLog = Join-Path $StagedDir 'compile.log'
$MainFile   = Join-Path $StagedDir 'main-class.txt'
$JarsTxt    = Join-Path $StagedDir 'jars.txt'
$SkipFlag   = Join-Path $StagedDir 'skipped.flag'
$SrcDir     = Join-Path $RepoRoot 'apps\mockito_probe'

New-Item -ItemType Directory -Force -Path $ClassesDir | Out-Null
Set-Content -Path $CompileLog -Value '' -Encoding utf8
Set-Content -Path $JarsTxt    -Value '' -Encoding utf8
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
if (-not $Javac) { Write-Error "stage-mockito-probe: javac not found"; exit 10 }
Add-Content -Path $CompileLog -Value "stage-mockito-probe: using javac at $Javac" -Encoding utf8

# Find first matching jar under given roots, excluding sources/javadoc.
function Find-FirstJar {
    param([string[]]$Roots, [string]$ArtifactName)
    foreach ($root in $Roots) {
        if (-not $root -or -not (Test-Path $root)) { continue }
        $found = Get-ChildItem -Path $root -Recurse -ErrorAction SilentlyContinue -Filter "$ArtifactName-*.jar" |
                 Where-Object { $_.Name -notmatch 'sources' -and $_.Name -notmatch 'javadoc' -and $_.Name -match "^$ArtifactName-[0-9]" } |
                 Select-Object -First 1
        if ($found) { return $found.FullName }
    }
    return $null
}

$m2Roots = @()
if ($env:USERPROFILE) { $m2Roots += (Join-Path $env:USERPROFILE '.m2\repository') }
$m2Roots += 'C:\Users\Victor\.m2\repository'

$MockitoJar = $env:MOCKITO_JAR
if (-not $MockitoJar -or -not (Test-Path $MockitoJar)) {
    $roots = $m2Roots | ForEach-Object { Join-Path $_ 'org\mockito\mockito-core' }
    $MockitoJar = Find-FirstJar -Roots $roots -ArtifactName 'mockito-core'
}

$BbJar = $env:BYTEBUDDY_JAR
if (-not $BbJar -or -not (Test-Path $BbJar)) {
    $roots = $m2Roots | ForEach-Object { Join-Path $_ 'net\bytebuddy\byte-buddy' }
    $BbJar = Find-FirstJar -Roots $roots -ArtifactName 'byte-buddy'
}

$BbAgJar = $env:BYTEBUDDY_AGENT_JAR
if (-not $BbAgJar -or -not (Test-Path $BbAgJar)) {
    $roots = $m2Roots | ForEach-Object { Join-Path $_ 'net\bytebuddy\byte-buddy-agent' }
    $BbAgJar = Find-FirstJar -Roots $roots -ArtifactName 'byte-buddy-agent'
}

$ObjJar = $env:OBJENESIS_JAR
if (-not $ObjJar -or -not (Test-Path $ObjJar)) {
    $roots = $m2Roots | ForEach-Object { Join-Path $_ 'org\objenesis\objenesis' }
    $ObjJar = Find-FirstJar -Roots $roots -ArtifactName 'objenesis'
}

# Maven Central fallback — download any missing jar into staged-mockito\cache\.
$CacheDir = Join-Path $StagedDir 'cache'
$McBase   = if ($env:MAVEN_CENTRAL_BASE) { $env:MAVEN_CENTRAL_BASE } else { 'https://repo1.maven.org/maven2' }
function Fetch-Mc {
    param([string]$Label, [string]$GroupPath, [string]$Artifact, [string]$Version, [string]$BaseName, [string]$LogPath, [string]$CacheDir, [string]$McBase)
    $cached = Join-Path $CacheDir ("{0}.jar" -f $BaseName)
    if (Test-Path $cached) {
        Add-Content -Path $LogPath -Value "stage-mockito-probe: using cached $Label at $cached" -Encoding utf8
        return $cached
    }
    if ($env:NO_NET -eq '1') { return $null }
    New-Item -ItemType Directory -Force -Path $CacheDir | Out-Null
    $url = "$McBase/$GroupPath/$Artifact/$Version/$BaseName.jar"
    Add-Content -Path $LogPath -Value "stage-mockito-probe: fetching $url" -Encoding utf8
    try {
        Invoke-WebRequest -Uri $url -OutFile $cached -UseBasicParsing -TimeoutSec 60 -ErrorAction Stop
        return $cached
    } catch {
        Add-Content -Path $LogPath -Value "stage-mockito-probe: download failed for $Label : $($_.Exception.Message)" -Encoding utf8
        if (Test-Path $cached) { Remove-Item $cached -Force }
        return $null
    }
}

if (-not $MockitoJar) {
    $v = if ($env:MOCKITO_VERSION) { $env:MOCKITO_VERSION } else { '5.13.0' }
    $got = Fetch-Mc -Label 'mockito-core' -GroupPath 'org/mockito' -Artifact 'mockito-core' -Version $v -BaseName "mockito-core-$v" -LogPath $CompileLog -CacheDir $CacheDir -McBase $McBase
    if ($got) { $MockitoJar = $got }
}
if (-not $BbJar) {
    $v = if ($env:BYTEBUDDY_VERSION) { $env:BYTEBUDDY_VERSION } else { '1.14.19' }
    $got = Fetch-Mc -Label 'byte-buddy' -GroupPath 'net/bytebuddy' -Artifact 'byte-buddy' -Version $v -BaseName "byte-buddy-$v" -LogPath $CompileLog -CacheDir $CacheDir -McBase $McBase
    if ($got) { $BbJar = $got }
}
if (-not $BbAgJar) {
    $v = if ($env:BYTEBUDDY_VERSION) { $env:BYTEBUDDY_VERSION } else { '1.14.19' }
    $got = Fetch-Mc -Label 'byte-buddy-agent' -GroupPath 'net/bytebuddy' -Artifact 'byte-buddy-agent' -Version $v -BaseName "byte-buddy-agent-$v" -LogPath $CompileLog -CacheDir $CacheDir -McBase $McBase
    if ($got) { $BbAgJar = $got }
}
if (-not $ObjJar) {
    $v = if ($env:OBJENESIS_VERSION) { $env:OBJENESIS_VERSION } else { '3.4' }
    $got = Fetch-Mc -Label 'objenesis' -GroupPath 'org/objenesis' -Artifact 'objenesis' -Version $v -BaseName "objenesis-$v" -LogPath $CompileLog -CacheDir $CacheDir -McBase $McBase
    if ($got) { $ObjJar = $got }
}

$missing = @()
if (-not $MockitoJar) { $missing += 'mockito-core' }
if (-not $BbJar)      { $missing += 'byte-buddy' }
if (-not $BbAgJar)    { $missing += 'byte-buddy-agent' }
if (-not $ObjJar)     { $missing += 'objenesis' }

if ($missing.Count -gt 0) {
    Add-Content -Path $CompileLog -Value ("stage-mockito-probe: SKIP missing jars: " + ($missing -join ', ')) -Encoding utf8
    Add-Content -Path $CompileLog -Value "  searched: ~/.m2/repository, C:\Users\Victor\.m2\repository" -Encoding utf8
    Add-Content -Path $CompileLog -Value "  attempted: Maven Central (set NO_NET=1 to skip; \$env:MAVEN_CENTRAL_BASE to override mirror)" -Encoding utf8
    Set-Content -Path $SkipFlag -Value ('missing: ' + ($missing -join ',')) -Encoding utf8
    # Compile the interface even when skipping so re-runs of the run script
    # don't hit a missing main-class.
    $rc = Invoke-Native -Exe $Javac -ArgList @('--release','21','-d',$ClassesDir,(Join-Path $SrcDir 'SomeInterface.java')) -LogPath $CompileLog
    if ($rc -ne 0) {
        Write-Error "stage-mockito-probe: javac (SomeInterface) failed (rc=$rc); see $CompileLog"
        exit 12
    }
    Set-Content -Path $MainFile -Value 'Main' -Encoding utf8
    Write-Output ("stage-mockito-probe: SKIP missing jars: " + ($missing -join ', '))
    exit 0
}

Copy-Item $MockitoJar (Join-Path $StagedDir 'mockito-core.jar')      -Force
Copy-Item $BbJar      (Join-Path $StagedDir 'byte-buddy.jar')        -Force
Copy-Item $BbAgJar    (Join-Path $StagedDir 'byte-buddy-agent.jar')  -Force
Copy-Item $ObjJar     (Join-Path $StagedDir 'objenesis.jar')         -Force

@(
    "mockito-core=$MockitoJar"
    "byte-buddy=$BbJar"
    "byte-buddy-agent=$BbAgJar"
    "objenesis=$ObjJar"
) | Set-Content -Path $JarsTxt -Encoding utf8

Add-Content -Path $CompileLog -Value "stage-mockito-probe: located jars" -Encoding utf8
Get-Content -Path $JarsTxt | ForEach-Object { Add-Content -Path $CompileLog -Value "  $_" -Encoding utf8 }

$cp = @(
    (Join-Path $StagedDir 'mockito-core.jar')
    (Join-Path $StagedDir 'byte-buddy.jar')
    (Join-Path $StagedDir 'byte-buddy-agent.jar')
    (Join-Path $StagedDir 'objenesis.jar')
) -join ';'

$Sources = @(
    (Join-Path $SrcDir 'SomeInterface.java')
    (Join-Path $SrcDir 'Main.java')
)
Add-Content -Path $CompileLog -Value "stage-mockito-probe: compiling $($Sources.Count) sources with mockito on cp" -Encoding utf8
$rc2 = Invoke-Native -Exe $Javac -ArgList (@('--release','21','-cp',$cp,'-d',$ClassesDir) + $Sources) -LogPath $CompileLog
if ($rc2 -ne 0) {
    Write-Error "stage-mockito-probe: javac failed (rc=$rc2); see $CompileLog"
    exit 12
}

Set-Content -Path $MainFile -Value 'Main' -Encoding utf8
Add-Content -Path $CompileLog -Value "stage-mockito-probe: main class = Main" -Encoding utf8
Add-Content -Path $CompileLog -Value "stage-mockito-probe: staged classes at $ClassesDir" -Encoding utf8
Write-Output "stage-mockito-probe: OK (classes=$ClassesDir)"
exit 0
