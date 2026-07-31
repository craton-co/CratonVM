<#
  ab-arm.ps1 - run ONE arm of the full Tomcat suite A/B.

  Arm A = defaults (OSR dead-local entry admitted; putfield-ctor JIT ban lifted)
  Arm B = both relaxations restored to their historical behaviour

  Both arms run CONCURRENTLY as separate processes so they see the same host
  load; the same binary serves both, so nothing but the two knobs differs.
  ASCII-only.
#>
param(
  [Parameter(Mandatory=$true)][ValidateSet('A','B')][string]$Arm,
  [string]$Exe = 'C:\craton\CratonVM-doc30-osr-20260731\cratonvm-doc30-slotfix-v2.exe',
  [int]$Parallel = 4,
  [int]$TimeoutSec = 300
)
$ErrorActionPreference = 'Continue'
if ($Arm -eq 'B') {
  $env:CRATONVM_JIT_OSR_DEAD_LOCALS = '0'
  $env:CRATONVM_JIT_PUTFIELD_INIT   = '0'
} else {
  Remove-Item Env:CRATONVM_JIT_OSR_DEAD_LOCALS -ErrorAction SilentlyContinue
  Remove-Item Env:CRATONVM_JIT_PUTFIELD_INIT   -ErrorAction SilentlyContinue
}
& 'C:\craton\CratonVM\apps\tomcat-suite-runner\run-tomcat-suite.ps1' `
    -Category all -Jit on -Jdk real -Vm craton `
    -Exe $Exe -Parallel $Parallel -TimeoutSec $TimeoutSec `
    -RunName ("doc30-arm$Arm")
