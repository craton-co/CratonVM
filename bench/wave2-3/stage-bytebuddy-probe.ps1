# bench/wave2-3/stage-bytebuddy-probe.ps1
# WP2.3-D - Windows PowerShell 5.1 stager for the ByteBuddy acceptance
# probes. Mirrors stage-bytebuddy-probe.sh.
#
# Behaviour:
#   * Locate byte-buddy-X.Y.Z.jar from $env:BYTEBUDDY_JAR or local
#     maven / vendor locations. If absent, stage only the synthetic
#     probe and write a 'skipped.flag'.
#
# Exit codes: 0 ok or skipped, 10 javac missing, 12 compile failure.

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$Here       = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot   = Resolve-Path (Join-Path $Here '..\..')
$StagedDir  = Join-Path $Here 'staged-bytebuddy'
$ClassesDir = Join-Path $StagedDir 'classes'
$CompileLog = Join-Path $StagedDir 'compile.log'
$MainFile   = Join-Path $StagedDir 'main-class.txt'
$JarPathFile= Join-Path $StagedDir 'jar-path.txt'
$SkipFlag   = Join-Path $StagedDir 'skipped.flag'
$SrcDir     = Join-Path $RepoRoot 'apps\bytebuddy_probe'

New-Item -ItemType Directory -Force -Path $ClassesDir | Out-Null
Set-Content -Path $CompileLog -Value '' -Encoding utf8
if (Test-Path $SkipFlag) { Remove-Item $SkipFlag -Force }

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
    Write-Error "stage-bytebuddy-probe: javac not found"
    exit 10
}
Add-Content -Path $CompileLog -Value "stage-bytebuddy-probe: using javac at $Javac" -Encoding utf8

# Locate byte-buddy core jar.
$JarPath = $null
$candidates = @()
if ($env:BYTEBUDDY_JAR -and (Test-Path $env:BYTEBUDDY_JAR)) { $candidates += $env:BYTEBUDDY_JAR }

$globRoots = @()
if ($env:USERPROFILE) {
    $globRoots += (Join-Path $env:USERPROFILE '.m2\repository\net\bytebuddy\byte-buddy')
}
$globRoots += 'C:\Users\Victor\.m2\repository\net\bytebuddy\byte-buddy'
$globRoots += 'C:\craton\ejbca-ce\lib\hibernate'

foreach ($root in $globRoots) {
    if (-not (Test-Path $root)) { continue }
    $found = Get-ChildItem -Path $root -Recurse -ErrorAction SilentlyContinue -Filter 'byte-buddy-*.jar' |
             Where-Object {
                 $_.Name -match '^byte-buddy-[0-9]' -and
                 $_.Name -notmatch 'agent' -and
                 $_.Name -notmatch 'android' -and
                 $_.Name -notmatch 'sources' -and
                 $_.Name -notmatch 'javadoc'
             } |
             Sort-Object -Property Name -Descending |
             Select-Object -First 1
    if ($found) { $candidates += $found.FullName }
}
foreach ($kc in @('C:\craton\keycloak-16.1.1', 'C:\craton\keycloak-26.2.4')) {
    if (-not (Test-Path $kc)) { continue }
    $found = Get-ChildItem -Path $kc -Recurse -ErrorAction SilentlyContinue -Filter 'byte-buddy-*.jar' |
             Where-Object {
                 $_.Name -match '^byte-buddy-[0-9]' -and
                 $_.Name -notmatch 'agent' -and
                 $_.Name -notmatch 'android'
             } |
             Sort-Object -Property Name -Descending |
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
    # Maven Central fallback — download byte-buddy core into staged-bytebuddy\cache\.
    $CacheDir = Join-Path $StagedDir 'cache'
    $BbVer = if ($env:BYTEBUDDY_VERSION) { $env:BYTEBUDDY_VERSION } else { '1.14.19' }
    $CachedJar = Join-Path $CacheDir ("byte-buddy-{0}.jar" -f $BbVer)
    if (Test-Path $CachedJar) {
        Add-Content -Path $CompileLog -Value "stage-bytebuddy-probe: using cached byte-buddy at $CachedJar" -Encoding utf8
        $JarPath = $CachedJar
    } elseif ($env:NO_NET -eq '1') {
        Add-Content -Path $CompileLog -Value "stage-bytebuddy-probe: NO_NET=1; skipping Maven Central fallback" -Encoding utf8
    } else {
        New-Item -ItemType Directory -Force -Path $CacheDir | Out-Null
        $McBase = if ($env:MAVEN_CENTRAL_BASE) { $env:MAVEN_CENTRAL_BASE } else { 'https://repo1.maven.org/maven2' }
        $Url = "$McBase/net/bytebuddy/byte-buddy/$BbVer/byte-buddy-$BbVer.jar"
        Add-Content -Path $CompileLog -Value "stage-bytebuddy-probe: fetching $Url" -Encoding utf8
        try {
            Invoke-WebRequest -Uri $Url -OutFile $CachedJar -UseBasicParsing -TimeoutSec 60 -ErrorAction Stop
            $JarPath = $CachedJar
            Add-Content -Path $CompileLog -Value "stage-bytebuddy-probe: downloaded byte-buddy to $CachedJar" -Encoding utf8
        } catch {
            Add-Content -Path $CompileLog -Value "stage-bytebuddy-probe: Maven Central download failed: $($_.Exception.Message)" -Encoding utf8
            if (Test-Path $CachedJar) { Remove-Item $CachedJar -Force }
        }
    }
}

if (-not $JarPath) {
    Add-Content -Path $CompileLog -Value "stage-bytebuddy-probe: SKIP byte-buddy jar not found" -Encoding utf8
    Add-Content -Path $CompileLog -Value "  attempted: Maven Central (set NO_NET=1 to skip; \$env:MAVEN_CENTRAL_BASE to override mirror)" -Encoding utf8
    Add-Content -Path $CompileLog -Value "  set BYTEBUDDY_JAR=/path/to/byte-buddy-X.Y.Z.jar to enable real-DSL probe" -Encoding utf8
    Set-Content -Path $SkipFlag -Value 'no-bytebuddy-jar' -Encoding utf8
    Write-Output "stage-bytebuddy-probe: SKIP byte-buddy jar not found (set BYTEBUDDY_JAR to enable real-DSL probe)"
}

# Compile synthetic.
$SourcesBase = @(
    (Join-Path $SrcDir 'Greeter.java'),
    (Join-Path $SrcDir 'HiGreeter.java'),
    (Join-Path $SrcDir 'ByteBuddyProbeSynth.java')
)
Add-Content -Path $CompileLog -Value "stage-bytebuddy-probe: compiling synthetic probe ($($SourcesBase.Count) sources)" -Encoding utf8
$rc = Invoke-Javac -Javac $Javac -ArgList (@('--release','21','-d',$ClassesDir) + $SourcesBase) -LogPath $CompileLog
if ($rc -ne 0) {
    Write-Error "stage-bytebuddy-probe: javac (synthetic) failed (rc=$rc); see $CompileLog"
    exit 12
}

$PayloadDir = Join-Path $StagedDir 'payload'
New-Item -ItemType Directory -Force -Path $PayloadDir | Out-Null
$srcPayload = Join-Path $SrcDir 'payload\HiGreeter.class'
if (Test-Path $srcPayload) {
    Copy-Item $srcPayload (Join-Path $PayloadDir 'HiGreeter.class') -Force
}

if ($JarPath) {
    Add-Content -Path $CompileLog -Value "stage-bytebuddy-probe: located byte-buddy jar at $JarPath" -Encoding utf8
    Set-Content -Path $JarPathFile -Value $JarPath -Encoding utf8
    Copy-Item $JarPath (Join-Path $StagedDir 'byte-buddy.jar') -Force

    Add-Content -Path $CompileLog -Value "stage-bytebuddy-probe: compiling ByteBuddyProbe with jar on cp" -Encoding utf8
    $rc2 = Invoke-Javac -Javac $Javac -ArgList @('--release','21','-cp',$JarPath,'-d',$ClassesDir,(Join-Path $SrcDir 'ByteBuddyProbe.java')) -LogPath $CompileLog
    if ($rc2 -ne 0) {
        Write-Error "stage-bytebuddy-probe: javac (ByteBuddyProbe) failed (rc=$rc2); see $CompileLog"
        exit 12
    }
    Set-Content -Path $MainFile -Value 'ByteBuddyProbe' -Encoding utf8
} else {
    Add-Content -Path $CompileLog -Value "stage-bytebuddy-probe: jar absent - main probe = synthetic ByteBuddyProbeSynth" -Encoding utf8
    Set-Content -Path $MainFile -Value 'ByteBuddyProbeSynth' -Encoding utf8
}

$mc = (Get-Content -Path $MainFile -Raw).Trim()
Add-Content -Path $CompileLog -Value "stage-bytebuddy-probe: main class = $mc" -Encoding utf8
Add-Content -Path $CompileLog -Value "stage-bytebuddy-probe: staged classes at $ClassesDir" -Encoding utf8
Write-Output "stage-bytebuddy-probe: OK (main=$mc, classes=$ClassesDir)"
exit 0
