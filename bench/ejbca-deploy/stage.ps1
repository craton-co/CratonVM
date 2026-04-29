# bench/ejbca-deploy/stage.ps1
# WP8.11 — PowerShell port of stage.sh.

$ErrorActionPreference = 'Stop'
$Here = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot = (Resolve-Path (Join-Path $Here '..\..')).Path
$WildflyFixture = Join-Path $RepoRoot 'bench\wildfly-boot'
$StagedDir = Join-Path $Here 'staged'
$EjbcaStaged = Join-Path $StagedDir 'ejbca'
$WildflyStaged = Join-Path $StagedDir 'wildfly'
$StageLog = Join-Path $StagedDir 'stage.log'
$ServerConfigMarker = Join-Path $StagedDir 'server-config.txt'

New-Item -ItemType Directory -Force -Path $StagedDir, $EjbcaStaged | Out-Null
'' | Set-Content -Path $StageLog -Encoding utf8

function Log([string]$msg) {
    $line = "stage-ejbca-deploy: $msg"
    Write-Host $line
    Add-Content -Path $StageLog -Value $line -Encoding utf8
}

$WildflyStagePs1 = Join-Path $WildflyFixture 'stage.ps1'
if (-not (Test-Path $WildflyStagePs1)) {
    Log "ERROR WP8.10 prerequisite missing: $WildflyStagePs1 not found"
    exit 11
}
Log "delegating WildFly stage to $WildflyStagePs1"
& powershell -NoProfile -ExecutionPolicy Bypass -File $WildflyStagePs1 *>> $StageLog
if ($LASTEXITCODE -ne 0) {
    Log "ERROR bench/wildfly-boot/stage.ps1 failed (rc=$LASTEXITCODE)"
    exit 11
}

# Discover WildFly home.
$WildflyHome = $null
$candidates = @(
    (Join-Path $WildflyFixture 'staged\wildfly'),
    (Join-Path $WildflyFixture 'staged\wildfly-32.0.1.Final')
)
$candidates += (Get-ChildItem -Path (Join-Path $WildflyFixture 'staged') -Directory -Filter 'wildfly-*' -ErrorAction SilentlyContinue |
    Select-Object -ExpandProperty FullName)
foreach ($c in $candidates) {
    if ($c -and (Test-Path $c) -and (Test-Path (Join-Path $c 'jboss-modules.jar'))) {
        $WildflyHome = $c; break
    }
}
if (-not $WildflyHome) {
    Log "ERROR WildFly home not found under $WildflyFixture\staged\"
    exit 11
}
Log "using WILDFLY_HOME=$WildflyHome"

if (Test-Path $WildflyStaged) { Remove-Item -Recurse -Force $WildflyStaged }
cmd /c mklink /J "$WildflyStaged" "$WildflyHome" *>> $StageLog
if ($LASTEXITCODE -ne 0) {
    Log "junction failed; copying tree (slow)"
    Copy-Item -Recurse -Force $WildflyHome $WildflyStaged
}

$EjbcaVersion = if ($env:EJBCA_VERSION) { $env:EJBCA_VERSION } else { '8.3.2' }
$EjbcaZip = Join-Path $StagedDir ("ejbca_ce_{0}.zip" -f $EjbcaVersion.Replace('.', '_'))
$UrlCandidates = @(
    "https://github.com/Keyfactor/ejbca-ce/releases/download/v$EjbcaVersion/ejbca_ce_$($EjbcaVersion.Replace('.','_')).zip",
    "https://sourceforge.net/projects/ejbca/files/ejbca6/$EjbcaVersion/ejbca_ce_$($EjbcaVersion.Replace('.','_')).zip/download"
)
$LocalCache = if ($env:EJBCA_DIST_CACHE) { $env:EJBCA_DIST_CACHE } `
              else { "C:\craton\ejbca-ce\dist\ejbca_ce_$($EjbcaVersion.Replace('.','_')).zip" }

if (Test-Path $LocalCache) {
    Log "using local EJBCA cache: $LocalCache"
    Copy-Item -Force $LocalCache $EjbcaZip
} elseif (Test-Path $EjbcaZip) {
    Log "EJBCA zip already present (skip download)"
} else {
    $fetched = $null
    foreach ($url in $UrlCandidates) {
        Log "trying $url"
        try {
            Invoke-WebRequest -Uri $url -OutFile $EjbcaZip -UseBasicParsing -TimeoutSec 60 -ErrorAction Stop
            $fetched = $url; break
        } catch {
            Log "  failed ($($_.Exception.Message))"
            if (Test-Path $EjbcaZip) { Remove-Item $EjbcaZip }
        }
    }
    if (-not $fetched) {
        Log "ERROR could not download EJBCA from any candidate URL"
        Log "  set `$env:EJBCA_DIST_CACHE to the path of a pre-downloaded zip"
        exit 12
    }
    Log "downloaded from $fetched"
}

Log "extracting $EjbcaZip"
if (Test-Path $EjbcaStaged) { Remove-Item -Recurse -Force $EjbcaStaged }
New-Item -ItemType Directory -Force -Path $EjbcaStaged | Out-Null
try {
    Expand-Archive -Path $EjbcaZip -DestinationPath $EjbcaStaged -Force
} catch {
    Log "ERROR Expand-Archive failed: $($_.Exception.Message)"
    exit 13
}

$EjbcaTop = $EjbcaStaged
if (-not (Test-Path (Join-Path $EjbcaStaged 'dist'))) {
    $inner = Get-ChildItem -Path $EjbcaStaged -Directory | Where-Object {
        Test-Path (Join-Path $_.FullName 'dist')
    } | Select-Object -First 1
    if ($inner) { $EjbcaTop = $inner.FullName }
    else {
        Log "ERROR no dist/ inside the EJBCA zip"
        exit 13
    }
}

$EarSrc = Join-Path $EjbcaTop 'dist\ejbca.ear'
if (-not (Test-Path $EarSrc)) {
    $EarSrc = (Get-ChildItem -Path $EjbcaTop -Recurse -Filter 'ejbca.ear' -File -ErrorAction SilentlyContinue |
               Select-Object -First 1).FullName
}
if (-not $EarSrc -or -not (Test-Path $EarSrc)) {
    Log "ERROR no ejbca.ear in dist (8.x may need 'ant deployear' — out of scope)"
    exit 12
}
$DeployDir = Join-Path $WildflyStaged 'standalone\deployments'
New-Item -ItemType Directory -Force -Path $DeployDir | Out-Null
Copy-Item -Force $EarSrc (Join-Path $DeployDir 'ejbca.ear')
'' | Set-Content -Path (Join-Path $DeployDir 'ejbca.ear.dodeploy') -Encoding utf8
Log "staged ejbca.ear -> $DeployDir"

$ServerConfig = 'standalone-ejbca.xml'
$SeSrc = $null
foreach ($c in @(
    (Join-Path $EjbcaTop 'dist\wildfly\standalone-ejbca.xml'),
    (Join-Path $EjbcaTop 'conf\standalone-ejbca.xml'),
    (Join-Path $EjbcaTop 'dist\standalone-ejbca.xml')
)) {
    if (Test-Path $c) { $SeSrc = $c; break }
}
$ConfigDir = Join-Path $WildflyStaged 'standalone\configuration'
if ($SeSrc) {
    Copy-Item -Force $SeSrc (Join-Path $ConfigDir 'standalone-ejbca.xml')
    Log "staged standalone-ejbca.xml from $SeSrc"
} else {
    Log "WARN standalone-ejbca.xml not in dist; falling back to standalone.xml"
    $ServerConfig = 'standalone.xml'
}
Set-Content -Path $ServerConfigMarker -Value $ServerConfig -Encoding utf8
Log "server-config = $ServerConfig"

Log "OK staged at $StagedDir"
exit 0
