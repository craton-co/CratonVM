# Runs one probe class under several VM configurations, sequentially, with the
# environment reset between them (Set-Item Env: persists for the whole process,
# so a shared session would leak config N into config N+1).
param(
    [string[]]$Classes = @('org.hibernate.orm.test.type.temporal.OffsetDateTimeTest'),
    [int]$TimeoutSec = 900
)
$ErrorActionPreference = 'Stop'
$runner = 'C:\craton\CratonVM-hibfive-takeover-20260730\.hibtake\run-hib.ps1'

$configs = @(
    @{ Label = 'cfg-default';        Env = @{} },
    @{ Label = 'cfg-nomoving';       Env = @{ CRATONVM_NO_MOVING_YOUNG = '1' } },
    @{ Label = 'cfg-nomoving-tierup'; Env = @{ CRATONVM_NO_MOVING_YOUNG = '1'; CRATONVM_JIT_VIRTUAL_TIERUP = '1' } }
)
$allKeys = 'CRATONVM_NO_MOVING_YOUNG', 'CRATONVM_JIT_VIRTUAL_TIERUP'

foreach ($cfg in $configs) {
    foreach ($k in $allKeys) { Remove-Item -Path "Env:$k" -ErrorAction SilentlyContinue }
    Write-Output "########## $($cfg.Label) ##########"
    & $runner -Classes $Classes -Label $cfg.Label -Runtime craton -TimeoutSec $TimeoutSec -EnvVars $cfg.Env
    foreach ($k in $allKeys) { Remove-Item -Path "Env:$k" -ErrorAction SilentlyContinue }
}
