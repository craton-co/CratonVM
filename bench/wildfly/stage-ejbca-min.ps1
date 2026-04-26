# bench/wildfly/stage-ejbca-min.ps1
# WP0.5 — stage the minimum EJBCA cesecore-common DirectRunner subset that was
# exercised on 2026-04-24 (session 93). Windows PowerShell 5.1 compatible.
# See docs/wildfly-ejbca-roadmap.md WP0.5 and memory/finding_println_regression.md.
#
# Behaviour mirrors stage-ejbca-min.sh:
#   * If C:\craton\ejbca-test-run\ is present, stage AccessMatchType +
#     DirectRunner from there.
#   * Otherwise fall back to apps\ejbca_min_fixture\src\Main.java placeholder.
#
# Exit codes: 0 ok, 10 no javac, 11 no sources, 12 compile failure.
#
# Usage: powershell -ExecutionPolicy Bypass -File bench\wildfly\stage-ejbca-min.ps1

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$Here      = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot  = Resolve-Path (Join-Path $Here '..\..')
$StagedDir = Join-Path $Here 'staged'
$ClassesDir = Join-Path $StagedDir 'classes'
$CompileLog = Join-Path $StagedDir 'compile.log'
$MainFile  = Join-Path $StagedDir 'main-class.txt'

New-Item -ItemType Directory -Force -Path $ClassesDir | Out-Null
Set-Content -Path $CompileLog -Value '' -Encoding utf8

# Wrapper that captures native stderr without PowerShell 5.1 NativeCommandError.
# Uses $psi.Arguments (always available on PS 5.1) with explicit quoting of
# any arg that contains whitespace or classpath separators.
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
    Write-Error "stage-ejbca-min: javac not found (JAVA_HOME unset and javac not on PATH)"
    exit 10
}
Add-Content -Path $CompileLog -Value "stage-ejbca-min: using javac at $Javac" -Encoding utf8

# Decide source set.
$EjbcaDir = if ($env:EJBCA_TEST_RUN_DIR) { $env:EJBCA_TEST_RUN_DIR } else { 'C:\craton\ejbca-test-run' }
$Mode = ''
if (Test-Path (Join-Path $EjbcaDir 'src\org\cesecore\authorization\user\AccessMatchType.java')) {
    $Mode = 'real'
} elseif (Test-Path (Join-Path $RepoRoot 'apps\ejbca_min_fixture\src\Main.java')) {
    $Mode = 'placeholder'
} else {
    Write-Error "stage-ejbca-min: no EJBCA source tree at $EjbcaDir and no placeholder fixture"
    exit 11
}
Add-Content -Path $CompileLog -Value "stage-ejbca-min: mode=$Mode" -Encoding utf8

$MainClass = ''
if ($Mode -eq 'real') {
    $SrcDir = Join-Path $EjbcaDir 'src'
    $JunitJar    = Join-Path $EjbcaDir 'junit.jar'
    $HamcrestJar = Join-Path $EjbcaDir 'hamcrest.jar'
    $Cp = ($JunitJar, $HamcrestJar) -join ';'
    $Sources = @((Join-Path $SrcDir 'org\cesecore\authorization\user\AccessMatchType.java'))
    if (Test-Path (Join-Path $SrcDir 'DirectRunner.java')) {
        $Sources += (Join-Path $SrcDir 'DirectRunner.java')
        $MainClass = 'DirectRunner'
    } elseif (Test-Path (Join-Path $SrcDir 'Runner.java')) {
        $Sources += (Join-Path $SrcDir 'Runner.java')
        $MainClass = 'Runner'
    } else {
        $MainClass = 'org.cesecore.authorization.user.AccessMatchType'
    }
    $Args = @('--release','21','-cp',$Cp,'-d',$ClassesDir) + $Sources
    Add-Content -Path $CompileLog -Value "stage-ejbca-min: compiling $($Sources.Count) sources with cp=$Cp" -Encoding utf8
    $rc = Invoke-Javac -Javac $Javac -ArgList $Args -LogPath $CompileLog
    if ($rc -ne 0) {
        Write-Error "stage-ejbca-min: javac failed (rc=$rc); see $CompileLog"
        exit 12
    }
    Copy-Item $JunitJar    (Join-Path $StagedDir 'junit.jar')    -Force -ErrorAction SilentlyContinue
    Copy-Item $HamcrestJar (Join-Path $StagedDir 'hamcrest.jar') -Force -ErrorAction SilentlyContinue
} else {
    $SrcFile = Join-Path $RepoRoot 'apps\ejbca_min_fixture\src\Main.java'
    Add-Content -Path $CompileLog -Value "stage-ejbca-min: compiling placeholder $SrcFile" -Encoding utf8
    $rc = Invoke-Javac -Javac $Javac -ArgList @('--release','21','-d',$ClassesDir,$SrcFile) -LogPath $CompileLog
    if ($rc -ne 0) {
        Write-Error "stage-ejbca-min: javac failed (rc=$rc); see $CompileLog"
        exit 12
    }
    $MainClass = 'Main'
}

Set-Content -Path $MainFile -Value $MainClass -Encoding utf8
Add-Content -Path $CompileLog -Value "stage-ejbca-min: main class = $MainClass" -Encoding utf8
Add-Content -Path $CompileLog -Value "stage-ejbca-min: staged classes at $ClassesDir" -Encoding utf8
Write-Output "stage-ejbca-min: OK (main=$MainClass, classes=$ClassesDir)"
exit 0
