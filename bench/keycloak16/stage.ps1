# bench/keycloak16/stage.ps1
# WP8.7 — stage the keycloak16 forcing-function fixture (Windows PowerShell 5.1).
#
# Network requirement: NONE today (placeholder fixture is in-tree).
# Real-distribution slot (not yet wired): if $env:KEYCLOAK16_HOME points at an
# unpacked distribution, prefer it. Tarball URL TODO; <5min on CI.
#
# Idempotent — safe to re-run.
# Exit codes: 0 ok, 10 no javac, 11 no sources, 12 compile failure.

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$Here       = Split-Path -Parent $MyInvocation.MyCommand.Path
$StagedDir  = Join-Path $Here 'staged'
$ClassesDir = Join-Path $StagedDir 'classes'
$CompileLog = Join-Path $StagedDir 'compile.log'
$MainFile   = Join-Path $StagedDir 'main-class.txt'
$SrcDir     = Join-Path $Here 'fixture'

New-Item -ItemType Directory -Force -Path $ClassesDir | Out-Null
Set-Content -Path $CompileLog -Value '' -Encoding utf8

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

$Javac = $null
if ($env:JAVA_HOME) {
    $candidate = Join-Path $env:JAVA_HOME 'bin\javac.exe'
    if (Test-Path $candidate) { $Javac = $candidate }
}
if (-not $Javac) {
    $cmd = Get-Command javac -ErrorAction SilentlyContinue
    if ($cmd) { $Javac = $cmd.Source }
}
if (-not $Javac) { Write-Error "stage-keycloak16: javac not found"; exit 10 }
Add-Content -Path $CompileLog -Value "stage-keycloak16: using javac at $Javac" -Encoding utf8

$SrcFile = Join-Path $SrcDir 'Main.java'
if (-not (Test-Path $SrcFile)) {
    Write-Error "stage-keycloak16: $SrcFile missing"
    exit 11
}

Add-Content -Path $CompileLog -Value "stage-keycloak16: compiling fixture/Main.java" -Encoding utf8
$rc = Invoke-Javac -Javac $Javac -ArgList @('--release','21','-d',$ClassesDir,$SrcFile) -LogPath $CompileLog
if ($rc -ne 0) {
    Write-Error "stage-keycloak16: javac failed (rc=$rc); see $CompileLog"
    exit 12
}

Set-Content -Path $MainFile -Value 'Main' -Encoding utf8
Add-Content -Path $CompileLog -Value "stage-keycloak16: main class = Main" -Encoding utf8
Add-Content -Path $CompileLog -Value "stage-keycloak16: staged classes at $ClassesDir" -Encoding utf8
Write-Output "stage-keycloak16: OK"
exit 0
