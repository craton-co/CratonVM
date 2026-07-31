# Interleaved A/B of the shadow-stack EMISSION variable on ONE binary.
#   A = default            (emission off: relocation-scoped implication)
#   B = CRATONVM_SHADOW_STACK=1 (emission on: approximates the pre-change codegen)
# Interleaved A,B,A,B so a drifting host perturbs both lanes equally — a
# back-to-back lane comparison on this box is worthless otherwise.
param(
    [string]$Class = 'org.hibernate.orm.test.hql.ASTParserLoadingTest',
    [int]$Rounds = 2,
    [int]$TimeoutSec = 900
)
$ErrorActionPreference = 'Stop'
$runner = 'C:\craton\CratonVM-hibfive-takeover-20260730\.hibtake\run-hib.ps1'
$key = 'CRATONVM_SHADOW_STACK'

$rows = @()
for ($r = 1; $r -le $Rounds; $r++) {
    foreach ($lane in @('A-emission-off', 'B-emission-on')) {
        Remove-Item -Path "Env:$key" -ErrorAction SilentlyContinue
        $envs = if ($lane -eq 'B-emission-on') { @{ $key = '1' } } else { @{} }
        $label = "ab-$lane-r$r"
        & $runner -Classes @($Class) -Label $label -Runtime craton -TimeoutSec $TimeoutSec -EnvVars $envs | Out-Null
        Remove-Item -Path "Env:$key" -ErrorAction SilentlyContinue
        $csv = "C:\craton\CratonVM-hibfive-takeover-20260730\.hibtake\runs\$label\_summary.csv"
        $row = Import-Csv $csv | Select-Object -First 1
        $rows += [pscustomobject]@{ Round = $r; Lane = $lane; Status = $row.Status; WallSec = $row.WallSec }
        "round $r  $lane : $($row.Status) $($row.WallSec)s" | Write-Output
    }
}
Write-Output ""
$rows | Group-Object Lane | ForEach-Object {
    $vals = $_.Group | ForEach-Object { [double]($_.WallSec -replace ',', '.') }
    "{0}: runs={1} median={2}s" -f $_.Name, $vals.Count, [math]::Round((($vals | Sort-Object)[[int]([math]::Floor($vals.Count/2))]), 1)
}
