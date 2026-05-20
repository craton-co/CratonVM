$proc = Get-Process -Id 26164 -ErrorAction SilentlyContinue
if (-not $proc) { Write-Output "process 26164 gone"; exit 1 }
$dumpPath = 'C:\craton\CratonVM\hangfull.dmp'
if (Test-Path $dumpPath) { Remove-Item $dumpPath -Force }
$fs = [System.IO.File]::Create($dumpPath)
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public class Dbg2 {
    [DllImport("dbghelp.dll")]
    public static extern bool MiniDumpWriteDump(IntPtr hProcess, uint pid, Microsoft.Win32.SafeHandles.SafeFileHandle hFile, int dumpType, IntPtr exParam, IntPtr userParam, IntPtr callback);
}
'@
# dumpType 2 = MiniDumpWithFullMemory (real stack data -> proper CFI unwinding)
$ok = [Dbg2]::MiniDumpWriteDump($proc.Handle, [uint32]$proc.Id, $fs.SafeFileHandle, 2, [IntPtr]::Zero, [IntPtr]::Zero, [IntPtr]::Zero)
$fs.Close()
Write-Output "fulldump ok=$ok size=$((Get-Item $dumpPath).Length) pid=$($proc.Id) cpu=$($proc.CPU)"
