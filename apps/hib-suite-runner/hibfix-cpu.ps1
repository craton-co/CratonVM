# hibfix-cpu.ps1 <outfile> <exe> <args...>  -> prints wall_ms and cpu (user+kernel) ms
param([string]$Out, [string]$Exe, [Parameter(ValueFromRemainingArguments=$true)][string[]]$Rest)
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $Exe
$psi.Arguments = (($Rest | ForEach-Object { '"' + ($_ -replace '"','\"') + '"' }) -join ' ')
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.UseShellExecute = $false
$sw = [System.Diagnostics.Stopwatch]::StartNew()
$p = [System.Diagnostics.Process]::Start($psi)
$so = $p.StandardOutput.ReadToEndAsync()
$se = $p.StandardError.ReadToEndAsync()
$p.WaitForExit()
$sw.Stop()
$user = $p.UserProcessorTime.TotalMilliseconds
$kern = $p.PrivilegedProcessorTime.TotalMilliseconds
Set-Content -Path $Out -Encoding utf8 -Value ($so.Result + "`n" + $se.Result)
"wall_ms={0:N0} cpu_user_ms={1:N0} cpu_kernel_ms={2:N0} rc={3}" -f $sw.ElapsedMilliseconds, $user, $kern, $p.ExitCode
