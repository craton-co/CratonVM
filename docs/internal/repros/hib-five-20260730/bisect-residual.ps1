# Attribute the residual moving-young throughput cost on ONE binary.
#
# Context: the relocation-scoped admission gates removed the optimizing-tier
# handicap, but `type.temporal` still exceeds the 300 s cap on default flags
# while passing under CRATONVM_NO_MOVING_YOUNG=1. Shadow push/reload is already
# eliminated as the cause. These are the two remaining candidates.
#
# RUN ONLY ON A QUIET HOST. Check first:
#   Get-CimInstance Win32_Process -Filter "Name='cratonvm.exe'" |
#     Where-Object { $_.ExecutablePath -notlike '*hibfive-takeover*' }
# Host noise has already manufactured two false results in this investigation
# (a 420->602 s "regression" and a 188->269 s one); at 45-82% background load
# these lanes are indistinguishable.
param(
    [string]$Class = 'org.hibernate.orm.test.type.temporal.OffsetDateTimeTest',
    [int]$TimeoutSec = 900
)
$ErrorActionPreference = 'Stop'
$runner = Join-Path $PSScriptRoot 'run-hib.ps1'

# Lane D is the target: if it matches lane E, the residual is fully explained by
# the two levers. If only one of B/C moves, that one is the whole cost.
$lanes = @(
    @{ N = 'A-default';        E = @{} },
    @{ N = 'B-no-scratch';     E = @{ CRATONVM_JIT_MY_SCRATCH_FLUSH = '0' } },
    @{ N = 'C-no-selfcall';    E = @{ CRATONVM_JIT_MY_SELFCALL_PROOF = '0' } },
    @{ N = 'D-neither';        E = @{ CRATONVM_JIT_MY_SCRATCH_FLUSH = '0'; CRATONVM_JIT_MY_SELFCALL_PROOF = '0' } },
    @{ N = 'E-no-moving';      E = @{ CRATONVM_NO_MOVING_YOUNG = '1' } }
)
$keys = 'CRATONVM_JIT_MY_SCRATCH_FLUSH', 'CRATONVM_JIT_MY_SELFCALL_PROOF', 'CRATONVM_NO_MOVING_YOUNG'

foreach ($l in $lanes) {
    foreach ($k in $keys) { Remove-Item "Env:$k" -ErrorAction SilentlyContinue }
    $label = "bisect-$($l.N)"
    & $runner -Classes @($Class) -Label $label -Runtime craton -TimeoutSec $TimeoutSec -EnvVars $l.E | Out-Null
    foreach ($k in $keys) { Remove-Item "Env:$k" -ErrorAction SilentlyContinue }
    $row = Import-Csv "C:\craton\CratonVM-hibfive-takeover-20260730\.hibtake\runs\$label\_summary.csv" |
        Select-Object -First 1
    "{0,-16} {1,-8} {2}s" -f $l.N, $row.Status, $row.WallSec | Write-Output
}
