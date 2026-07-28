# Repeat pass for the three decisive OracleInline configurations, interleaved so
# host drift cannot favour one of them (see the tomcat A/B lesson: sequential
# blocks lie). Each case is a fresh process running one test method.
param(
	[string]$Exe = "C:\craton\CratonVM-hib-osa-window-20260728\target\release\cratonvm-osawin-20260728.exe",
	[string]$OutDir = "C:\craton\CratonVM-hib-osa-window-20260728",
	[int]$Rounds = 2
)

$jdk = "C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot"
$list = "oracle-inline-one.txt"
$timeout = "-Djunit.jupiter.execution.timeout.default=3600s"
$quiet = "-Dlog4j2.configurationFile=log4j2-quiet.properties"
$graph = "-Dhibernate.flush.queue.type=graph"
$nosql = @("-Dhibernate.show_sql=false", "-Dhibernate.format_sql=false")

$cases = @(
	@{ tag = "base";   args = @();                              env = $null },
	@{ tag = "graph";  args = @($graph);                        env = $null },
	@{ tag = "alllev"; args = ($nosql + @($quiet, $graph));     env = "org/h2/" }
)

for ($r = 1; $r -le $Rounds; $r++) {
	foreach ($c in $cases) {
		if ($c.env) { $env:CRATONVM_JIT_ALLOW_PACKAGES = $c.env }
		$log = Join-Path $OutDir "rep$r-$($c.tag).log"
		$argv = @("--java-home", $jdk, "--Xmx", "1500m", "@common.args", $timeout) `
			+ $c.args + @("CratonRunnerTimed", $list, "0")
		& $Exe @argv 1>$log 2>$null
		if ($c.env) { Remove-Item env:CRATONVM_JIT_ALLOW_PACKAGES -ErrorAction SilentlyContinue }
		$ms = (Get-Content $log | Select-String "@@METHOD") -replace '.*ms=(\d+).*', '$1'
		Write-Host ("round {0}  {1,-8} ms={2}" -f $r, $c.tag, $ms)
	}
}
