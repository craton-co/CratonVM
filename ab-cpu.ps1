# A/B two cratonvm binaries by USER CPU TIME, interleaved A-B-B-A.
#
# Wall clock cannot resolve a small delta on a shared box (other VMs and cargo
# jobs run concurrently here); user CPU time per process can. `.Handle` is
# touched before `WaitForExit` because a `Start-Process -PassThru` object whose
# handle was never materialised reports `$null` for ExitCode AND for
# TotalProcessorTime — every run would silently score as zero.
param(
  [Parameter(Mandatory=$true)][string]$A,
  [Parameter(Mandatory=$true)][string]$B,
  [Parameter(Mandatory=$true)][string[]]$VmArgs,
  [int]$Reps = 3
)

function Run-One([string]$exe) {
  # `-ArgumentList` splits on whitespace, so an unquoted `--java-home
  # "C:\Program Files\..."` reaches the VM as `C:\Program` and every run fails
  # argument parsing with rc=1 in ~40 ms — a fast, uniform, entirely fake
  # result. Quote any element that contains a space.
  $quoted = $VmArgs | ForEach-Object { if ($_ -match '\s') { '"' + $_ + '"' } else { $_ } }
  $p = Start-Process -FilePath $exe -ArgumentList $quoted -PassThru -NoNewWindow `
        -RedirectStandardOutput "$env:TEMP\ab-cpu-out.txt" -RedirectStandardError "$env:TEMP\ab-cpu-err.txt"
  $null = $p.Handle
  $p.WaitForExit()
  [pscustomobject]@{
    Cpu  = [math]::Round($p.TotalProcessorTime.TotalSeconds, 2)
    User = [math]::Round($p.UserProcessorTime.TotalSeconds, 2)
    Wall = [math]::Round(($p.ExitTime - $p.StartTime).TotalSeconds, 2)
    Exit = $p.ExitCode
    Out  = (Get-Content "$env:TEMP\ab-cpu-out.txt" -Tail 1)
  }
}

$ra = @(); $rb = @()
for ($i = 1; $i -le $Reps; $i++) {
  # A-B-B-A within each rep, so a monotone drift in host load cancels instead
  # of landing on one arm (see feedback_abba_interleave_still_clusters_one_arm:
  # the ordering alone is not enough, which is why every rep is reported).
  $x = Run-One $A; $ra += $x; Write-Output ("rep{0} A cpu={1}s user={2}s wall={3}s rc={4} | {5}" -f $i,$x.Cpu,$x.User,$x.Wall,$x.Exit,$x.Out)
  $x = Run-One $B; $rb += $x; Write-Output ("rep{0} B cpu={1}s user={2}s wall={3}s rc={4} | {5}" -f $i,$x.Cpu,$x.User,$x.Wall,$x.Exit,$x.Out)
  $x = Run-One $B; $rb += $x; Write-Output ("rep{0} B cpu={1}s user={2}s wall={3}s rc={4} | {5}" -f $i,$x.Cpu,$x.User,$x.Wall,$x.Exit,$x.Out)
  $x = Run-One $A; $ra += $x; Write-Output ("rep{0} A cpu={1}s user={2}s wall={3}s rc={4} | {5}" -f $i,$x.Cpu,$x.User,$x.Wall,$x.Exit,$x.Out)
}

# A run that failed argument parsing is fast, uniform, and meaningless — refuse
# to print a ratio over one.
if (($ra + $rb | Where-Object { $_.Exit -ne 0 }).Count -gt 0) {
  Write-Output "REFUSING to summarise: at least one run exited non-zero (see $env:TEMP\ab-cpu-err.txt)"
  exit 2
}

$ma = ($ra | Measure-Object -Property Cpu -Average -Minimum).Average
$mb = ($rb | Measure-Object -Property Cpu -Average -Minimum).Average
$na = ($ra | Measure-Object -Property Cpu -Minimum).Minimum
$nb = ($rb | Measure-Object -Property Cpu -Minimum).Minimum
Write-Output ("--- A mean_cpu={0:N2}s min={1:N2}s  n={2}" -f $ma,$na,$ra.Count)
Write-Output ("--- B mean_cpu={0:N2}s min={1:N2}s  n={2}" -f $mb,$nb,$rb.Count)
Write-Output ("--- B/A mean={0:N3}  min={1:N3}" -f ($mb/$ma),($nb/$na))
