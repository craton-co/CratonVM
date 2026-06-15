# Per-class H2 suite sweep for one VM. Runs each org.h2.test class via RunOne in
# its own process + own working dir (isolates crashes, hs_err logs and ./data).
# Classifies PASS/FAIL/SKIP/CRASH/HANG and writes a results CSV + per-class logs.
#
#   VM = craton | hotspot
#   Parallel = max concurrent worker processes
#   TimeoutSec = per-class wall cap (a class exceeding it => HANG, killed)
param(
    [ValidateSet('craton','hotspot')][string]$VM = 'craton',
    [int]$Parallel = 4,
    [int]$TimeoutSec = 200,
    [string]$Tag = 'run1',
    [ValidateSet('mem','disk','net')][string]$Config = 'mem',
    [string]$ClassesFile = 'C:\craton\CratonVM-h2suite\h2sweep\classes.txt',
    [string]$Only = ''   # optional: comma-separated substrings; run only matching classes
)
$ErrorActionPreference = 'Stop'
$H2 = 'C:\craton\CratonVM\apps\h2database\h2'
$cp = Get-Content "$H2\cp_abs.txt"
$jdk = 'C:\Program Files\Java\jdk-25'
$root = "C:\craton\CratonVM-h2suite\h2sweep\$VM-$Tag"
New-Item -ItemType Directory -Force -Path $root | Out-Null
$csv = "$root\results.csv"
'class,status,rc,sec,sig' | Out-File $csv -Encoding utf8

# Resolve launcher
if ($VM -eq 'craton') {
    $src = 'C:\craton\CratonVM-h2suite\target\release\cratonvm.exe'
    $exe = "$root\cratonvm-h2sweep-$Tag.exe"
    Copy-Item $src $exe -Force
    # Let our per-class timeout govern hangs, not the internal 120s stack-dump watchdog.
    $env:CRATONVM_DISABLE_DEFAULT_WATCHDOG = '1'
    # Quote args containing spaces so Start-Process -ArgumentList does not split them.
    $jdkQ = '"' + $jdk + '"'
    $cpQ  = '"' + $cp  + '"'
    $baseArgs = @('--java-home', $jdkQ, '-Xmx1g', '-cp', $cpQ, 'org.h2.test.RunOne')
} else {
    $exe = "$jdk\bin\java.exe"
    $baseArgs = @('-Xmx1g', '-cp', $cp, 'org.h2.test.RunOne')
}

$classes = Get-Content $ClassesFile | Where-Object { $_ -and -not $_.StartsWith('#') }
if ($Only) {
    $pats = $Only.Split(',')
    $classes = $classes | Where-Object { $c = $_; ($pats | Where-Object { $c -like "*$_*" }).Count -gt 0 }
}
$total = $classes.Count
Write-Output "VM=$VM tag=$Tag classes=$total parallel=$Parallel timeout=${TimeoutSec}s"

$crashRe = 'EXCEPTION_ACCESS_VIOLATION|fatal error|runtime error:|not implemented:|panicked|inconsistent header|stack dump|Object\(Some\(|SIGSEGV|STATUS_|internal error|thread .* has overflowed its stack|Unsupported|unreachable'
$queue = [System.Collections.Queue]::new()
$classes | ForEach-Object { $queue.Enqueue($_) }
$active = @()
$done = 0
$counts = @{ PASS=0; FAIL=0; SKIP=0; CRASH=0; HANG=0 }

while ($queue.Count -gt 0 -or $active.Count -gt 0) {
    # fill
    while ($active.Count -lt $Parallel -and $queue.Count -gt 0) {
        $cls = $queue.Dequeue()
        $short = ($cls -split '\.')[-1]
        $wd = "$root\wd\$short"
        New-Item -ItemType Directory -Force -Path $wd | Out-Null
        $out = "$root\$short.out.log"
        $err = "$root\$short.err.log"
        $a = $baseArgs + @($cls, $Config)
        $p = Start-Process -FilePath $exe -ArgumentList $a -WorkingDirectory $wd `
             -NoNewWindow -PassThru -RedirectStandardOutput $out -RedirectStandardError $err
        $active += [pscustomobject]@{ cls=$cls; short=$short; proc=$p; t0=(Get-Date); out=$out; err=$err; wd=$wd }
    }
    Start-Sleep -Milliseconds 400
    $still = @()
    foreach ($w in $active) {
        $sec = [int]((Get-Date) - $w.t0).TotalSeconds
        if ($w.proc.HasExited) {
            $rc = $w.proc.ExitCode
            $outTxt = (Get-Content $w.out -Raw -ErrorAction SilentlyContinue)
            $errTxt = (Get-Content $w.err -Raw -ErrorAction SilentlyContinue)
            $hasResult = $outTxt -match 'RUNONE_RESULT'
            $sig = ''
            $status = ''
            if ($hasResult) {
                if ($outTxt -match 'failed=true') { $status = 'FAIL' }
                elseif ($outTxt -match 'code=2') { $status = 'SKIP' }
                else { $status = 'PASS' }
            } else {
                $status = 'CRASH'
                $m = [regex]::Match("$errTxt`n$outTxt", $crashRe)
                if ($m.Success) {
                    $line = (("$errTxt`n$outTxt" -split "`n") | Where-Object { $_ -match $crashRe } | Select-Object -First 1)
                    $sig = ($line -replace '[\r\n]',' ').Trim()
                    if ($sig.Length -gt 200) { $sig = $sig.Substring(0,200) }
                } else {
                    $sig = "no-result rc=$rc"
                }
            }
            $hs = Get-ChildItem "$($w.wd)\hs_err_pid*.log" -ErrorAction SilentlyContinue | Select-Object -First 1
            if ($hs) { if ($status -ne 'CRASH') { $status='CRASH' }; if (-not $sig) { $sig = "hs_err:$($hs.Name)" } }
            $counts[$status]++
            $done++
            $sigEsc = '"' + ($sig -replace '"','''') + '"'
            "$($w.cls),$status,$rc,$sec,$sigEsc" | Out-File $csv -Append -Encoding utf8
            $col = @{PASS='OK';FAIL='FAIL';SKIP='skip';CRASH='CRASH';HANG='HANG'}[$status]
            Write-Output ("[{0,3}/{1}] {2,-6} {3,5}s {4} {5}" -f $done,$total,$col,$sec,$w.short,$sig)
        }
        elseif ($sec -ge $TimeoutSec) {
            try { $w.proc.Kill(); $w.proc.WaitForExit(3000) } catch {}
            # kill stray children sharing the unique exe name (craton)
            $status = 'HANG'; $sig = "timeout ${TimeoutSec}s"
            $counts[$status]++; $done++
            "$($w.cls),$status,-1,$sec,`"$sig`"" | Out-File $csv -Append -Encoding utf8
            Write-Output ("[{0,3}/{1}] {2,-6} {3,5}s {4} {5}" -f $done,$total,'HANG',$sec,$w.short,$sig)
        }
        else { $still += $w }
    }
    $active = $still
}

Write-Output "=== $VM-$Tag SUMMARY ==="
$counts.GetEnumerator() | Sort-Object Name | ForEach-Object { Write-Output ("{0,-6}: {1}" -f $_.Key,$_.Value) }
Write-Output "CSV: $csv"
