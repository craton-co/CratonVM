param(
    [string]$Path = "lcov.info",
    [double]$LineThreshold = 85.0,
    [switch]$AllowMissing
)

$ErrorActionPreference = "Stop"

if (-not (Test-Path -LiteralPath $Path)) {
    $message = "LCOV report not found: $Path"
    if ($AllowMissing) {
        Write-Warning $message
        exit 0
    }
    Write-Error $message
    exit 2
}

$found = 0
$hit = 0

foreach ($line in Get-Content -LiteralPath $Path) {
    if ($line -match '^LF:(\d+)$') {
        $found += [int]$Matches[1]
    } elseif ($line -match '^LH:(\d+)$') {
        $hit += [int]$Matches[1]
    }
}

if ($found -le 0) {
    Write-Error "LCOV report contains no line counters: $Path"
    exit 2
}

$coverage = ($hit / $found) * 100.0
$display = "{0:N2}" -f $coverage
$required = "{0:N2}" -f $LineThreshold

Write-Host "Line coverage: $display% ($hit / $found), required: $required%"

if ($coverage + 0.000001 -lt $LineThreshold) {
    Write-Error "Line coverage is below the required threshold."
    exit 1
}

exit 0
