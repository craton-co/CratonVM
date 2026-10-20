param(
    [switch]$Execute,
    [string]$Until,
    [switch]$SkipPackage
)

$ErrorActionPreference = "Stop"

$publishOrder = @(
    "cratonvm-types",
    "cratonvm-reader",
    "cratonvm-native-api",
    "cratonvm-jit-api",
    "cratonvm-jit",
    "cratonvm-gc",
    "cratonvm-native-collections",
    "cratonvm-native-io",
    "cratonvm-classloading",
    "cratonvm-native-builtins",
    "cratonvm-jfr",
    "cratonvm-vm",
    "cratonvm-cli",
    "libcratonvm",
    "cratonvm-embed",
    "cratonvm-difftest"
)

$withheld = @(
    "cratonvm-native-awt",
    "cratonvm-jit-cuda",
    "cratonvm-gpu",
    "cratonvm-cuda-bridge",
    "cratonvm-fuzz"
)

if ($Until) {
    $stopIndex = [array]::IndexOf($publishOrder, $Until)
    if ($stopIndex -lt 0) {
        throw "Unknown -Until crate '$Until'. Expected one of: $($publishOrder -join ', ')"
    }
    $publishOrder = $publishOrder[0..$stopIndex]
}

Write-Host "CratonVM crates.io dry-run checklist"
Write-Host "Publish order: $($publishOrder -join ' -> ')"
Write-Host "Withheld packages: $($withheld -join ', ')"
Write-Host "Metadata gate: Craton Software Company authors, Apache-2.0 license, README, and repository/homepage/documentation links."
Write-Host "Dependency gate: default features for publishable crates must not pull withheld packages."
Write-Host ""

function Invoke-Step {
    param(
        [string]$Crate,
        [string[]]$CargoArgs
    )

    $display = "cargo $($CargoArgs -join ' ')"
    if (-not $Execute) {
        Write-Host "[preview][$Crate] $display"
        return
    }

    Write-Host "[run][$Crate] $display"
    & cargo @CargoArgs
    if ($LASTEXITCODE -ne 0) {
        throw "Command failed for $Crate with exit code $LASTEXITCODE`: $display"
    }
}

foreach ($crate in $publishOrder) {
    Invoke-Step -Crate $crate -CargoArgs @("package", "-p", $crate, "--list")
    if (-not $SkipPackage) {
        Invoke-Step -Crate $crate -CargoArgs @("package", "-p", $crate)
    }
    Invoke-Step -Crate $crate -CargoArgs @("publish", "-p", $crate, "--dry-run")
    Write-Host ""
}

if (-not $Execute) {
    Write-Host "Preview only. Re-run with -Execute after inspecting docs/RELEASE_READINESS.md."
}
