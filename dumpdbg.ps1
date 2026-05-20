param([int]$WaitSec = 14)
Start-Sleep -Seconds $WaitSec
$proc = Get-Process cratonvm -ErrorAction SilentlyContinue | Sort-Object StartTime | Select-Object -Last 1
if (-not $proc) { Write-Output "no cratonvm process"; exit 1 }
$dumpPath = 'C:\craton\CratonVM\hangdbg.dmp'
if (Test-Path $dumpPath) { Remove-Item $dumpPath -Force }
$fs = [System.IO.File]::Create($dumpPath)
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public class DbgD {
    [DllImport("dbghelp.dll")]
    public static extern bool MiniDumpWriteDump(IntPtr hProcess, uint pid, Microsoft.Win32.SafeHandles.SafeFileHandle hFile, int dumpType, IntPtr exParam, IntPtr userParam, IntPtr callback);
}
'@
$ok = [DbgD]::MiniDumpWriteDump($proc.Handle, [uint32]$proc.Id, $fs.SafeFileHandle, 2, [IntPtr]::Zero, [IntPtr]::Zero, [IntPtr]::Zero)
$fs.Close()
Write-Output "ok=$ok size=$((Get-Item $dumpPath).Length) pid=$($proc.Id) cpu=$($proc.CPU)"
