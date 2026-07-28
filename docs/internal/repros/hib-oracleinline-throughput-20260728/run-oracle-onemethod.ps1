# Single-method A/B for the OracleInlineMutationStrategyIdTest throughput residual.
# testDeleteFromPerson still carries the full @BeforeEach 2200-entity fixture, so
# it is representative, but one method costs ~4 min instead of ~19.
param(
	[string]$Exe = "C:\craton\CratonVM-hib-osa-window-20260728\target\release\cratonvm-osawin-20260728.exe",
	[string]$OutDir = "C:\craton\CratonVM-hib-osa-window-20260728",
	[string[]]$Only = @("craton-base", "craton-quiet", "craton-h2jit", "craton-graphq")
)

$jdk = "C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot"
$list = "oracle-inline-one.txt"
$timeout = "-Djunit.jupiter.execution.timeout.default=3600s"
$quiet = "-Dlog4j2.configurationFile=log4j2-quiet.properties"

function Run-Case($tag, $extraArgs, $envName, $envValue) {
	if ($Only -notcontains $tag) { return }
	if ($envName) { Set-Item -Path "env:$envName" -Value $envValue }
	$log = Join-Path $OutDir "one-$tag.log"
	$err = Join-Path $OutDir "one-$tag.err.log"
	$argv = @("--java-home", $jdk, "--Xmx", "1500m", "@common.args", $timeout) `
		+ $extraArgs + @("CratonRunnerTimed", $list, "0")
	$sw = [Diagnostics.Stopwatch]::StartNew()
	& $Exe @argv 1>$log 2>$err
	$sw.Stop()
	if ($envName) { Remove-Item -Path "env:$envName" -ErrorAction SilentlyContinue }
	$m = ((Get-Content $log | Select-String "@@METHOD") -join " ") -replace '.*IdTest#\S+ ', ''
	Write-Host ("{0,-16} wall={1,7:N1}s   {2}" -f $tag, $sw.Elapsed.TotalSeconds, $m)
}

$nosql = @("-Dhibernate.show_sql=false", "-Dhibernate.format_sql=false")

$graph = "-Dhibernate.flush.queue.type=graph"

Run-Case "craton-base"   @()                                        $null $null
Run-Case "craton-quiet"  @($quiet)                                  $null $null
Run-Case "craton-h2jit"  @()   "CRATONVM_JIT_ALLOW_PACKAGES" "org/h2/"
Run-Case "craton-graphq" @($graph)                                  $null $null
# Hibernate's own test hibernate.properties sets show_sql=true and
# format_sql=true, so every one of the fixture's 4400 statements is
# pretty-printed to stdout. This case removes that.
Run-Case "craton-nosql"  $nosql                                     $null $null
# All four levers at once: does the method then fit a 120 s budget?
Run-Case "craton-alllev" ($nosql + @($quiet, $graph)) "CRATONVM_JIT_ALLOW_PACKAGES" "org/h2/"
