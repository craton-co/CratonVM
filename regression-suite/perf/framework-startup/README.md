# Framework startup gate

Measures launch-to-readiness in fresh processes, alternating baseline/candidate
order. Both binaries run with `--fast-boot`. The gate requires a 2x median
improvement for **both** applications and exits 1 if either misses it.
Each run must exit successfully and match the fixture's deterministic `CHECK`
and `READY` lines from HotSpot. Readiness precedes shutdown; shutdown must still
complete successfully. The gate compares stdout and saves stderr in a separate
`*.log.stderr` file so VM diagnostics cannot alter application assertions. A
printed framework startup banner alone is insufficient.

The Spring fixture starts a non-web `SpringApplication`, verifies a configured
singleton, and closes its context. The Quarkus fixture is an augmented fast-jar
with ArC CDI, configuration injection and singleton identity checks. These are
small application fixtures, not the whole framework suites or HTTP-server tests.
The Quarkus fixture uses the local checkout's `999-SNAPSHOT` artifacts; its
version must match those artifacts.

Build once outside the timed runs (PowerShell, from the CVM repository root):

```powershell
$jdkPath = 'C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot'
$appsPath = 'C:/craton/CVM/apps'
$fixturePath = (Resolve-Path regression-suite/perf/framework-startup).Path.Replace('\', '/')
$outputPath = (New-Item -ItemType Directory -Force scratch/framework-startup).FullName.Replace('\', '/')
$springCp = (Get-ChildItem "$appsPath/spring-boot/cratonvm-suite/lib/*.jar" |
    ForEach-Object { $_.FullName.Replace('\', '/') }) -join ';'
& "$jdkPath/bin/javac.exe" -cp $springCp -d "$outputPath/classes" "$fixturePath/spring/Startup.java"
@('-Dprobe.value=configured', '-Dfile.encoding=UTF-8', '-cp',
    "`"$outputPath/classes;$springCp`"", 'probe.Startup') | Set-Content "$outputPath/spring.args"

$env:JAVA_HOME = $jdkPath
& "$appsPath/quarkus/mvnw.cmd" "-Dmaven.repo.local=$appsPath/quarkus-suite-runner/.m2-ext-20260908" `
    -f "$fixturePath/quarkus/pom.xml" package -DskipTests '-Dquarkus.native.builder-image=unused'
@('-Dprobe.value=configured', '-Dfile.encoding=UTF-8', '-jar',
    "`"$fixturePath/quarkus/target/quarkus-app/quarkus-run.jar`"") | Set-Content "$outputPath/quarkus.args"

python "$fixturePath/test_benchmark.py"
python "$fixturePath/benchmark.py" --baseline C:/path/to/baseline/cratonvm.exe `
    --candidate C:/path/to/candidate/cratonvm.exe --java-home $jdkPath `
    --spring-args "$outputPath/spring.args" --quarkus-args "$outputPath/quarkus.args" `
    --output "$outputPath/comparison" --runs 5
```

Use forward slashes in Java argument files: backslashes are escape characters.
The output directory must be new. Logs and raw samples remain there, including
completed samples if a subsequent run fails. Run on a quiet host without a build
or profiler competing with the measured processes. These are fresh-process,
warm-filesystem-cache measurements; this tool does not flush the operating
system's disk cache. Check any runtime change in `--real-jdk` against HotSpot too;
this performance gate does not replace that correctness check.
