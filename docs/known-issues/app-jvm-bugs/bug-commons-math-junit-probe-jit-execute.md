# Commons Math — JUnit5 probe JIT failure at execute (CM-2)

## Status
**OPEN** on `target/release/cratonvm.exe` (2026-06-05 apps suite).

Distinct from [CM-1 ConsoleLauncher / full reactor](bug-commons-math-full-reactor.md) (`BasicFileAttributes.isDirectory()` during package scan).

## Severity
**MEDIUM** — programmatic JUnit5 launch discovers tests but cannot execute them reliably.

## App / suite
- **Probe:** `bench/JUnitProbe.java` (uses `LauncherFactory`, not picocli)
- **Target:** `org.apache.commons.math4.transform.TransformUtilsTest`
- **Harness:** `test-infra/run-all-apps-suites.sh`
- **Log:** `test-infra/suite-results/apps-all-20260605-170945/commons-math-junit-probe-cratonvm.log`

## Symptom

Discovery **succeeds**:

```
DISCOVER_FOUND=4
  TransformUtilsTest
    - testSampleWrongBounds
    - testSample
    - testSampleNegativeNumberOfPoints
    - testSampleNullNumberOfPoints
```

Execution **fails** at `JUnitProbe.main` line 22 (`launcher.execute`):

```
Exception in thread "main" java/lang/InternalError:
  JIT dispatch into org/junit/platform/launcher/core/EngineExecutionOrchestrator.execute(
    Lorg/junit/platform/launcher/core/LauncherDiscoveryResult;
    Lorg/junit/platform/engine/EngineExecutionListener;)V failed:
  linkage error: no class def found: org/apache/commons/math4/transform/TransformUtilsTest
```

- **rc:** 1 · **wall:** 4.5 s

## HotSpot behavior

Not measured in apps suite (CratonVM failed first). Earlier same-day run on partial classpath: discovery found tests; HotSpot had `initializationError` on incomplete deps. With full deps, HotSpot executes all 4 tests.

## CratonVM behavior

- **Discover phase:** vintage engine loads `TransformUtilsTest` from classpath — class is visible.
- **Execute phase:** JIT-compiled dispatch into `EngineExecutionOrchestrator.execute` reports **linkage error: no class def found** for the same class.

Suggests **classloader / JIT linkage mismatch** between discovery and execution class paths, or JIT compiling a call site against a stale or wrong loader view.

### Flakiness note

An earlier run the same day (~12 s wall) reported `DISCOVER_FOUND=4 EXEC_FOUND=4 SUCCEEDED=4 FAILED=0` with the same probe and similar classpath. Treat as **intermittent JIT/linkage** until root-caused.

## Reproduce

```bash
# compile probe (once)
STD=".bench-cache/junit-platform-console-standalone-1.10.2.jar"
CM="apps/_test-suites/commons-math"
CP="$STD;$CM/commons-math-transform/target/{classes,test-classes};$CM/commons-math-core/target/classes;…m2 deps…"
javac -cp "$CP" -d bench bench/JUnitProbe.java

cratonvm.exe --java-home "<jdk-25>" --Xmx 2g -cp "bench;$CP" JUnitProbe
```

Or: `bash test-infra/run-junit-probes.sh`

## Fix direction

1. JIT dispatch / linkage for interface calls on JUnit Platform launcher when test classes were loaded by a different loader during discovery.
2. Compare interpreter-only run (`--nojit` / JIT bisect off) — if execute passes interpreted, bug is JIT-specific.
3. Verify `TransformUtilsTest` remains defined in the same loader at execute entry (classloader probe at `EngineExecutionOrchestrator.execute` prologue).

## Related

- [bug-commons-math-full-reactor.md](bug-commons-math-full-reactor.md) (CM-1 — ConsoleLauncher vintage scan)
- `test-infra/run-junit-probes.sh`
- [apps/CRATONVM_CRASHES.md](../../apps/CRATONVM_CRASHES.md)
