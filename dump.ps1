param([int]$WaitSec = 14)
Start-Sleep -Seconds $WaitSec
$proc = Get-Process rustjvm -ErrorAction SilentlyContinue | Sort-Object StartTime | Select-Object -Last 1
if (-not $proc) { Write-Output "no rustjvm process"; exit 1 }
$dumpPath = 'C:\craton\CratonVM\hang.dmp'
if (Test-Path $dumpPath) { Remove-Item $dumpPath -Force }
$fs = [System.IO.File]::Create($dumpPath)
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public class Dbg {
    [DllImport("dbghelp.dll")]
    public static extern bool MiniDumpWriteDump(IntPtr hProcess, uint pid, Microsoft.Win32.SafeHandles.SafeFileHandle hFile, int dumpType, IntPtr exParam, IntPtr userParam, IntPtr callback);
}
'@
# dumpType 0 = MiniDumpNormal (thread stacks only — small, enough for stackwalk)
$ok = [Dbg]::MiniDumpWriteDump($proc.Handle, [uint32]$proc.Id, $fs.SafeFileHandle, 0, [IntPtr]::Zero, [IntPtr]::Zero, [IntPtr]::Zero)
$fs.Close()
Write-Output "dump ok=$ok size=$((Get-Item $dumpPath).Length) pid=$($proc.Id) cpu=$($proc.CPU)"
