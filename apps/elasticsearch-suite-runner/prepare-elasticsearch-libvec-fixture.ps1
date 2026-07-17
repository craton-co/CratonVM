<#
.SYNOPSIS
  Rebuilds the Linux x64 Elasticsearch libvec fixture from the supplied
  Elasticsearch checkout and installs it into that same checkout.

.DESCRIPTION
  The suite must never receive libvec from a different checkout or a cached
  artifact package. This script builds the library from
  libs/simdvec/native in the selected Elasticsearch root, verifies the
  documented bulk8 ABI sentinels in the result, then atomically replaces
  lib/platform/linux-x64/libvec.so.
#>
[CmdletBinding()]
param(
  [Parameter(Mandatory = $true)] [string]$ElasticsearchRoot,
  [switch]$RebuildToolchain
)

$ErrorActionPreference = 'Stop'
# PowerShell 7 can turn stderr from an expected non-zero native probe (such as
# `docker image inspect` for a missing local image) into a terminating error.
# Native command status is handled explicitly below instead.
$PSNativeCommandUseErrorActionPreference = $false

function Write-Info([string]$Message) {
  Write-Host "[elasticsearch-libvec-fixture] $Message"
}

function Die([string]$Message) {
  throw "[elasticsearch-libvec-fixture] $Message"
}

function Invoke-Docker([string[]]$Arguments) {
  & docker @Arguments
  if ($LASTEXITCODE -ne 0) {
    Die "docker $($Arguments -join ' ') failed with exit code $LASTEXITCODE"
  }
}

$root = [System.IO.Path]::GetFullPath($ElasticsearchRoot)
if (-not (Test-Path -LiteralPath $root -PathType Container)) {
  Die "Elasticsearch root not found: $root"
}

$nativeRoot = Join-Path $root 'libs/simdvec/native'
$dockerfile = Join-Path $nativeRoot 'Dockerfile.cross-toolchain'
if (-not (Test-Path -LiteralPath $dockerfile -PathType Leaf)) {
  Die "Elasticsearch native vec sources are missing: $dockerfile"
}

if (-not (Get-Command docker -ErrorAction SilentlyContinue)) {
  Die "Docker is required to build the Linux x64 fixture reproducibly."
}

$image = 'cratonvm-es-libvec-toolchain-20260717'
$savedErrorActionPreference = $ErrorActionPreference
try {
  $ErrorActionPreference = 'Continue'
  & docker image inspect $image *> $null
  $imagePresent = $LASTEXITCODE -eq 0
} finally {
  $ErrorActionPreference = $savedErrorActionPreference
}
if ($RebuildToolchain -or -not $imagePresent) {
  Write-Info "building local native toolchain image=$image"
  Invoke-Docker @('build', '--pull', '-f', $dockerfile, '-t', $image, $nativeRoot)
}

$containerRoot = '/workspace'
$sourceRelative = 'build/libs/vec/shared/amd64/libvec.so'
Write-Info "building Linux x64 libvec from checkout=$root"
Invoke-Docker @('run', '--rm', '-v', "$nativeRoot`:$containerRoot", '-w', $containerRoot, $image, 'make', $sourceRelative)

$source = Join-Path $nativeRoot ($sourceRelative -replace '/', '\\')
if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
  Die "native build completed without producing $source"
}

$nmCommand = @'
symbols=$(nm -D --defined-only build/libs/vec/shared/amd64/libvec.so | awk '$3 ~ /^vec_/ { count++ } END { print count+0 }')
printf 'vec_symbols=%s\n' "$symbols"
test "$symbols" -ge 155
nm -D --defined-only build/libs/vec/shared/amd64/libvec.so | grep -Fq vec_cosi8_bulk8
nm -D --defined-only build/libs/vec/shared/amd64/libvec.so | grep -Fq vec_doti8_bulk8
nm -D --defined-only build/libs/vec/shared/amd64/libvec.so | grep -Fq vec_sqri8_bulk8
'@
Write-Info 'validating native exports'
Invoke-Docker @('run', '--rm', '-v', "$nativeRoot`:$containerRoot", '-w', $containerRoot, $image, 'sh', '-lc', $nmCommand)

$targetDir = Join-Path $root 'lib/platform/linux-x64'
$target = Join-Path $targetDir 'libvec.so'
New-Item -ItemType Directory -Force -Path $targetDir | Out-Null
$temporary = Join-Path $targetDir 'libvec.so.new'
Copy-Item -LiteralPath $source -Destination $temporary -Force
Move-Item -LiteralPath $temporary -Destination $target -Force

$hash = (Get-FileHash -LiteralPath $target -Algorithm SHA256).Hash.ToLowerInvariant()
Write-Info "READY path=$target sha256=$hash"
