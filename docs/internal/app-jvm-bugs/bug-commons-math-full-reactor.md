# Apache Commons Math — full reactor / ConsoleLauncher failure guide

## Status
**OPEN** (2026-06-05). Programmatic JUnit5 discovery (`JUnitProbe`) works; Maven
full reactor and `ConsoleLauncher` package scan do not.

## Severity
**HIGH** — blocks running the ~3204-test Commons Math reactor under CratonVM.

## App / suite
- **Tree:** `apps/_test-suites/commons-math/` (Apache Commons Math 4.x multi-module Maven reactor)
- **HotSpot path:** `JAVA_HOME=<jdk-25> mvn -o test` from module root (~3204 tests, BUILD SUCCESS)
- **CratonVM paths tried:**
  1. Maven under CratonVM — **not possible** (CratonVM is not a Maven `JAVA_HOME`)
  2. `org.junit.platform.console.ConsoleLauncher execute --select-package org.apache.commons.math4.transform` — **CRASH**
  3. Programmatic `LauncherFactory` via `bench/JUnitProbe` — **PASS** (4/4 on `TransformUtilsTest`)

## HotSpot behavior (reference)

```
Tests run: 3204, Failures: 0, Errors: 0, Skipped: 30, Flakes: 4
BUILD SUCCESS
wall ~132 s
```

Run with Maven driving the JVM (not `-Djvm=` — Surefire ignores that):

```bash
cd apps/_test-suites/commons-math
JAVA_HOME="C:/Program Files/Java/jdk-25" mvn -o test \
  -Dmaven.test.failure.ignore=true \
  -Drat.skip=true -Dcheckstyle.skip=true -Dspotbugs.skip=true \
  -Dpmd.skip=true -Denforcer.skip=true -Dmaven.javadoc.skip=true \
  -Danimal.sniffer.skip=true
```

## CratonVM behavior — ConsoleLauncher (package scan)

**Command:**

```bash
cratonvm.exe --java-home "<jdk-25>" --Xmx 2g \
  -cp "<junit-standalone.jar>;<commons-math-transform/classes+test-classes>;<commons-math-core/classes>" \
  org.junit.platform.console.ConsoleLauncher execute \
  --select-package org.apache.commons.math4.transform \
  --details=summary --disable-banner
```

**Result:** rc=-1 / `System.exit(-1)` after ~150 s. No tests executed.

**Log:** `test-infra/suite-results/cm-console-crash.log`

### Root exception chain

```
org.junit.platform.commons.JUnitException: TestEngine with ID 'junit-vintage' failed to discover tests
Caused by: org.junit.platform.commons.JUnitException: PackageSelector [packageName = 'org.apache.commons.math4.transform'] resolution failed
Caused by: java.lang.AbstractMethodError: method java/nio/file/attribute/BasicFileAttributes.isDirectory()Z has no Code attribute
    at java.nio.file.Files.walkFileTree(Files.java:2536)
    at org.junit.platform.commons.util.ClasspathScanner.findClassesForPath(ClasspathScanner.java:125)
    at org.junit.vintage.engine.discovery.VintageDiscoverer.discover(...)
```

Vintage engine classpath scanning walks directories via `Files.walkFileTree`. The
walk invokes `BasicFileAttributes.isDirectory()` on CratonVM. That interface
method is dispatched without a `Code` attribute (JDK interface default / native
bridge gap), so discovery aborts before any test class loads.

After the exception, picocli prints `--help` and calls `System.exit(-1)`.

### What this is NOT

Older harness notes mentioned `NoSuchMethodError: BufferedWriter.write([BII)V`.
That was a separate picocli/help-banner path. The **2026-06-05 fresh-build**
failure on `--select-package` is **`BasicFileAttributes.isDirectory()` —
AbstractMethodError**, reproducible with the log above.

## CratonVM behavior — programmatic probe (works)

Bypasses vintage package scan; selects one class directly:

```bash
javac -cp "<junit-standalone.jar>;..." -d bench bench/JUnitProbe.java
cratonvm.exe -cp "bench;<same cp>" JUnitProbe org.apache.commons.math4.transform.TransformUtilsTest
```

**Result (2026-06-05):**

```
DISCOVER_FOUND=4
EXEC_FOUND=4 SUCCEEDED=4 FAILED=0
```

HotSpot on the same partial classpath reports `DISCOVER_FOUND=1` with
`initializationError` (missing Maven dependency jars for full fixture init) —
so CratonVM is **ahead** on this narrow probe, not behind.

## Workarounds

| Goal | Approach |
|------|----------|
| Run one test class | `JUnitProbe` / `LauncherFactory` + `selectClass` |
| Run one module | Enumerate test classes, one `selectClass` per class (no `--select-package`) |
| Full 3204-test reactor | Blocked until `BasicFileAttributes` NIO bridge fixed |
| Compare to HotSpot | Run Maven under HotSpot only; compare per-class probes on CratonVM |

## Fix direction

1. **`java.nio.file.attribute.BasicFileAttributes`** — ensure interface methods
   (`isDirectory()`, `isRegularFile()`, etc.) dispatch correctly when called on
   CratonVM's `BasicFileAttributes` implementation during `Files.walkFileTree`.
2. Optionally exclude vintage engine: `--exclude-engine=junit-vintage` may allow
   Jupiter-only discovery if no vintage tests are required (Commons Math 4 is
   Jupiter-first; verify before relying on this).
3. Do **not** use ConsoleLauncher `--help` as a health signal on CratonVM until
   picocli output path is stable.

## Related files

- Harness: `test-infra/run-vm-comparison.sh` (`run_commons_math`)
- Probe script: `test-infra/run-junit-probes.sh`
- App README: `apps/README.md` (Commons Math section)
- Comparison TSV: `test-infra/suite-results/cmp-commons-math-20260605-163424.tsv`
