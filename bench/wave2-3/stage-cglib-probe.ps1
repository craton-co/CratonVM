# bench/wave2-3/stage-cglib-probe.ps1
# WP2.3-D - Windows PowerShell 5.1 stager for the CGLIB acceptance probes.
# Mirrors stage-cglib-probe.sh.
#
# Behaviour:
#   * Locate cglib-nodep-X.Y.jar from $env:CGLIB_JAR or well-known
#     local maven / vendor locations. If absent, stage only the
#     synthetic probe and write a 'skipped.flag' under staged-cglib/.
#   * javac --release 21 the probes into staged-cglib\classes.
#
# Exit codes: 0 ok or skipped, 10 javac missing, 12 compile failure.
#
# Usage: powershell -ExecutionPolicy Bypass -File bench\wave2-3\stage-cglib-probe.ps1

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$Here       = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot   = Resolve-Path (Join-Path $Here '..\..')
$StagedDir  = Join-Path $Here 'staged-cglib'
$ClassesDir = Join-Path $StagedDir 'classes'
$CompileLog = Join-Path $StagedDir 'compile.log'
$MainFile   = Join-Path $StagedDir 'main-class.txt'
$JarPathFile= Join-Path $StagedDir 'jar-path.txt'
$SkipFlag   = Join-Path $StagedDir 'skipped.flag'
$SrcDir     = Join-Path $RepoRoot 'apps\cglib_probe'

New-Item -ItemType Directory -Force -Path $ClassesDir | Out-Null
Set-Content -Path $CompileLog -Value '' -Encoding utf8
if (Test-Path $SkipFlag) { Remove-Item $SkipFlag -Force }

# Wrapper around native javac that sidesteps the PowerShell 5.1 NativeCommandError
# behaviour with stderr.
function Invoke-Javac {
    param([string]$Javac, [string[]]$ArgList, [string]$LogPath)
    $quoted = foreach ($a in $ArgList) {
        if ($a -match '[\s;]') { '"' + $a.Replace('"','\"') + '"' } else { $a }
    }
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $Javac
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
    $candidate = Join-Path $env:JAVA_HOME 'bin\javac.exe'
    if (Test-Path $candidate) { $Javac = $candidate }
}
if (-not $Javac) {
    $cmd = Get-Command javac -ErrorAction SilentlyContinue
    if ($cmd) { $Javac = $cmd.Source }
}
if (-not $Javac) {
    Write-Error "stage-cglib-probe: javac not found"
    exit 10
}
Add-Content -Path $CompileLog -Value "stage-cglib-probe: using javac at $Javac" -Encoding utf8

# Locate cglib jar.
$JarPath = $null
$candidates = @()
if ($env:CGLIB_JAR -and (Test-Path $env:CGLIB_JAR)) { $candidates += $env:CGLIB_JAR }

# Build glob roots - return only existing paths.
$globRoots = @()
if ($env:USERPROFILE) {
    $globRoots += (Join-Path $env:USERPROFILE '.m2\repository\cglib\cglib-nodep')
    $globRoots += (Join-Path $env:USERPROFILE '.m2\repository\cglib\cglib')
}
$globRoots += "$env:USERPROFILE\.m2\repository\cglib\cglib-nodep"
$globRoots += "$env:USERPROFILE\.m2\repository\cglib\cglib"
$globRoots += 'C:\craton\ejbca-ce\lib'

foreach ($root in $globRoots) {
    if (-not (Test-Path $root)) { continue }
    $found = Get-ChildItem -Path $root -Recurse -ErrorAction SilentlyContinue -Filter 'cglib*.jar' |
             Where-Object { $_.Name -notmatch 'sources' -and $_.Name -notmatch 'javadoc' } |
             Select-Object -First 1
    if ($found) { $candidates += $found.FullName }
}
# Keycloak nested layout.
foreach ($kc in @('C:\craton\keycloak-16.1.1', 'C:\craton\keycloak-26.2.4')) {
    if (-not (Test-Path $kc)) { continue }
    $found = Get-ChildItem -Path $kc -Recurse -ErrorAction SilentlyContinue -Filter 'cglib*.jar' |
             Where-Object { $_.Name -notmatch 'sources' -and $_.Name -notmatch 'javadoc' } |
             Select-Object -First 1
    if ($found) { $candidates += $found.FullName }
}

foreach ($cand in $candidates) {
    if (Test-Path $cand) {
        $JarPath = $cand
        break
    }
}

if (-not $JarPath) {
    # Maven Central fallback — download cglib-nodep into staged-cglib\cache\.
    $CacheDir = Join-Path $StagedDir 'cache'
    $CglibVer = if ($env:CGLIB_VERSION) { $env:CGLIB_VERSION } else { '3.3.0' }
    $CachedJar = Join-Path $CacheDir ("cglib-nodep-{0}.jar" -f $CglibVer)
    if (Test-Path $CachedJar) {
        Add-Content -Path $CompileLog -Value "stage-cglib-probe: using cached cglib at $CachedJar" -Encoding utf8
        $JarPath = $CachedJar
    } elseif ($env:NO_NET -eq '1') {
        Add-Content -Path $CompileLog -Value "stage-cglib-probe: NO_NET=1; skipping Maven Central fallback" -Encoding utf8
    } else {
        New-Item -ItemType Directory -Force -Path $CacheDir | Out-Null
        $McBase = if ($env:MAVEN_CENTRAL_BASE) { $env:MAVEN_CENTRAL_BASE } else { 'https://repo1.maven.org/maven2' }
        $Url = "$McBase/cglib/cglib-nodep/$CglibVer/cglib-nodep-$CglibVer.jar"
        Add-Content -Path $CompileLog -Value "stage-cglib-probe: fetching $Url" -Encoding utf8
        try {
            Invoke-WebRequest -Uri $Url -OutFile $CachedJar -UseBasicParsing -TimeoutSec 60 -ErrorAction Stop
            $JarPath = $CachedJar
            Add-Content -Path $CompileLog -Value "stage-cglib-probe: downloaded cglib to $CachedJar" -Encoding utf8
        } catch {
            Add-Content -Path $CompileLog -Value "stage-cglib-probe: Maven Central download failed: $($_.Exception.Message)" -Encoding utf8
            if (Test-Path $CachedJar) { Remove-Item $CachedJar -Force }
        }
    }
}

if (-not $JarPath) {
    Add-Content -Path $CompileLog -Value "stage-cglib-probe: SKIP cglib jar not found" -Encoding utf8
    Add-Content -Path $CompileLog -Value "  searched: \$env:CGLIB_JAR, $env:USERPROFILE\.m2, %USERPROFILE%\.m2, C:\craton\ejbca-ce\lib, C:\craton\keycloak-*" -Encoding utf8
    Add-Content -Path $CompileLog -Value "  attempted: Maven Central (set NO_NET=1 to skip; \$env:MAVEN_CENTRAL_BASE to override mirror)" -Encoding utf8
    Set-Content -Path $SkipFlag -Value 'no-cglib-jar' -Encoding utf8
    Write-Output "stage-cglib-probe: SKIP cglib jar not found (set CGLIB_JAR to enable real-DSL probe)"
}

# Compile synthetic family.
$SourcesBase = @(
    (Join-Path $SrcDir 'Target.java'),
    (Join-Path $SrcDir 'EnhancedTarget.java'),
    (Join-Path $SrcDir 'Step1.java'),
    (Join-Path $SrcDir 'Step2.java'),
    (Join-Path $SrcDir 'CglibProbe.java')
)
Add-Content -Path $CompileLog -Value "stage-cglib-probe: compiling synthetic probes ($($SourcesBase.Count) sources)" -Encoding utf8
$rc = Invoke-Javac -Javac $Javac -ArgList (@('--release','21','-d',$ClassesDir) + $SourcesBase) -LogPath $CompileLog
if ($rc -ne 0) {
    Write-Error "stage-cglib-probe: javac (synthetic) failed (rc=$rc); see $CompileLog"
    exit 12
}

# Stage payload directory.
$PayloadDir = Join-Path $StagedDir 'payload'
New-Item -ItemType Directory -Force -Path $PayloadDir | Out-Null
$srcPayload = Join-Path $SrcDir 'payload\EnhancedTarget.class'
if (Test-Path $srcPayload) {
    Copy-Item $srcPayload (Join-Path $PayloadDir 'EnhancedTarget.class') -Force
}

if ($JarPath) {
    Add-Content -Path $CompileLog -Value "stage-cglib-probe: located cglib jar at $JarPath" -Encoding utf8
    Set-Content -Path $JarPathFile -Value $JarPath -Encoding utf8
    Copy-Item $JarPath (Join-Path $StagedDir 'cglib.jar') -Force

    Add-Content -Path $CompileLog -Value "stage-cglib-probe: compiling CglibProbe2 with jar on cp" -Encoding utf8
    $rc2 = Invoke-Javac -Javac $Javac -ArgList @('--release','21','-cp',$JarPath,'-d',$ClassesDir,(Join-Path $SrcDir 'CglibProbe2.java')) -LogPath $CompileLog
    if ($rc2 -ne 0) {
        Write-Error "stage-cglib-probe: javac (CglibProbe2) failed (rc=$rc2); see $CompileLog"
        exit 12
    }
    Set-Content -Path $MainFile -Value 'CglibProbe2' -Encoding utf8
} else {
    Add-Content -Path $CompileLog -Value "stage-cglib-probe: jar absent - main probe = synthetic CglibProbe" -Encoding utf8
    Set-Content -Path $MainFile -Value 'CglibProbe' -Encoding utf8
}

$mc = (Get-Content -Path $MainFile -Raw).Trim()
Add-Content -Path $CompileLog -Value "stage-cglib-probe: main class = $mc" -Encoding utf8
Add-Content -Path $CompileLog -Value "stage-cglib-probe: staged classes at $ClassesDir" -Encoding utf8
Write-Output "stage-cglib-probe: OK (main=$mc, classes=$ClassesDir)"
exit 0
