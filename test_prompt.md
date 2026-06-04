# Task: run the full cross-VM test/measurement suite and report results

Run every test/measurement performed in the cross-VM comparison session, collect the
numbers, and produce consolidated comparison tables. This is a **measurement + reporting**
task — do not change VM source. Run on Windows (Git-bash). When done, print the tables and
stop.

## VMs under test
- **CratonVM-CPU**: `C:/craton/CratonVM/target/release/cratonvm.exe --java-home "C:/Program Files/Java/jdk-25"`
- **CratonVM-GPU**: `C:/craton/CratonVM/target-gpu/release/cratonvm.exe --gpu --java-home "C:/Program Files/Java/jdk-25"` (RTX 2060)
- **HotSpot**: `C:/Program Files/Java/jdk-25/bin/java.exe`
- **TornadoVM**: `C:/craton/tornadovm/jdk-25.0.3/bin/java.exe` (+ `@C:/craton/tornadovm/tornadovm-4.0.1-jdk25-ptx/tornado-argfile` for GPU paths; plain CPU Java needs no argfile)
- Maven: `C:/tools/apache-maven-3.9.15/bin/mvn.cmd`

## Prerequisites / gotchas
1. **Build both binaries** (a running `cratonvm.exe` locks the exe — `taskkill //F //IM cratonvm.exe` first; verify the binary mtime advanced after each build):
   - CPU: `cargo build --release -p cratonvm-cli --bin cratonvm`
   - GPU: `cargo build --release -p cratonvm-cli --bin cratonvm --features gpu-driver --target-dir target-gpu`
   - `native-builtins` is huge; if rustc dies with exit `0xffffffff`, set `RUST_MIN_STACK=536870912` and/or build to an isolated `--target-dir` (a concurrent agent may be holding cargo locks / relinking with stale rlibs — verify with a unique target dir).
2. **MSYS classpath mangling**: when passing a `;`-separated Windows `-cp` to a native `.exe` from Git-bash, export `MSYS2_ARG_CONV_EXCL='*'` and `MSYS_NO_PATHCONV=1` first, or the junit jar gets dropped (`bc-suite-3way.sh` already does this).
3. **Kill stray `cratonvm.exe` after each test run** (a hung run locks the binary for the next build).
4. Use **absolute** classpaths for Commons Math (a relative cp from the wrong cwd fails on every VM).

## Test 1 — numeric micro-benchmarks (4 modes): wall ms + checksum correctness
`BenchSuite` in `bench/` — one process per (benchmark, variant). Benchmarks:
`arith1500M fib44 sieve250k matrix600 bintrees18 vadd2_28`. Each prints
`RESULT name=<n> ms=<t> checksum=<c>`.
```
# all 4 modes (CPU/GPU/HotSpot/TornadoVM):
TIMEOUT=360 HEAP=8g bash test-infra/bench-modes-compare.sh
# or via the master harness, CPU-only: VARIANTS="cratonvm-cpu hotspot tornadovm"
```
Report a table: rows = benchmarks, cols = VMs, cells = wall ms. **Verify every checksum is
identical across a row** (correctness). Known: `bintrees18` crashes/timeouts on CratonVM
(JIT safepoint-spill); GPU ≈ CPU here (none of these kernels are offload-eligible).

## Test 2 — Bouncy Castle core suites (3 VMs)
```
bash test-infra/bc-suite-3way.sh        # asn1, math-ec, math-raw, math, crypto*, pqc, util*
```
Report rc / state (OK/FAIL/TIMEOUT) / wall / heap per suite per VM. Notes: `crypto-prng`
passes; `util-encoders` passes when the cp is correct (MSYS guard on); `crypto-regression`
needs ≥4g (OOMs at 1g on HotSpot too); `math-ec`/`pqc` are slow on CratonVM (BC JIT ban →
interpreter). Distinguish genuine fails from harness artifacts.

## Test 3 — Apache Commons Math (3 VMs)
- **HotSpot & TornadoVM**: full reactor, genuine, by running Maven UNDER each VM's own
  `JAVA_HOME` (not `-Djvm`, which surefire ignores):
  ```
  cd apps/_test-suites/commons-math
  JAVA_HOME="C:/Program Files/Java/jdk-25"               <mvn> -o test -Dmaven.test.failure.ignore=true -Drat.skip=true -Dcheckstyle.skip=true -Dspotbugs.skip=true -Dpmd.skip=true -Denforcer.skip=true -Dmaven.javadoc.skip=true -Danimal.sniffer.skip=true
  JAVA_HOME="C:/craton/tornadovm/jdk-25.0.3"             <mvn> -o test ... (same flags)
  ```
  Report `Tests run / Failures / Errors / Skipped` from the reactor summary (~3204 tests).
- **CratonVM**: the picocli ConsoleLauncher path is separately tracked; use the
  **programmatic `LauncherFactory`** probe (authoritative). Build the abs cp (single
  jupiter-api) and run `JUnitProbe` / `EngineProbe` (in `bench/`, see Test 5). Report
  DISCOVER/EXEC FOUND/SUCCEEDED/FAILED, compared to HotSpot.

## Test 4 — extras (4 modes)
```
bash test-infra/extras-modes-compare.sh     # junit-help, dacapo-avrora
```
Both are **measurement artifacts, not VM signals** — report as such: junit `--help` prints
`Usage: junit` on HotSpot/Tornado (rc=0); DaCapo avrora "validation FAILED" on HotSpot too
(JDK-25 stderr-digest artifact: expected digest = SHA-1 of the empty string).

## Test 5 — JUnit5 discovery probes (CratonVM vs HotSpot)
Programmatic launch bypassing picocli — the authoritative discovery test. Probes compile
against the standalone jar into `bench/`:
- `JUnitProbe` → `DISCOVER_FOUND` / `EXEC_FOUND SUCCEEDED FAILED`
- `EngineProbe` → `ServiceLoader<TestEngine>` ids
- `AnnProbe3` → class modifiers (expect `0x1`, not `0x21`) + `findAnnotatedMethods`
```
STD=.bench-cache/junit-platform-console-standalone-1.10.2.jar
ABS=<standalone jar>;apps/_test-suites/commons-math/commons-math-{transform,core}/target/{classes,test-classes};<commons-numbers/rng/math3 jars from ~/.m2>
"<jdk>/bin/javac.exe" -cp "$ABS" -d bench /tmp/{JUnitProbe,EngineProbe,AnnProbe3}.java
<cratonvm> -cp "bench;$ABS" JUnitProbe   # compare to: "<jdk>/bin/java.exe" -cp "bench;$ABS" JUnitProbe
```
Report FOUND counts side by side (target: CratonVM == HotSpot == 4 for `TransformUtilsTest`).

## One-shot option
The master harness runs Test 1 (bench) + Test 2 (bc) + Test 3 (commons-math, hotspot/tornado
genuine) + Test 4 (extras) and renders pivot tables:
```
SECTIONS="bench bc commons-math extras render" bash test-infra/run-vm-comparison.sh
# CPU-only bench/extras: VARIANTS="cratonvm-cpu hotspot tornadovm"
```
Raw TSVs land under `test-infra/suite-results/cmp-*.tsv`; the script prints aligned
comparison tables at the end.

## Deliverable
Print: (1) micro-benchmark table (wall ms + a checksum-consistency column), (2) BC suite
state table, (3) Commons Math tests/fail/skip per VM, (4) extras with artifact annotations,
(5) JUnit5 discovery FOUND counts. Note any run-to-run variance and any VM that crashed/
timed out, then stop.

Memory refs: `reference_cross_vm_comparison_harness`, `reference_junit5_console_launcher`,
`reference_bc_suite_harness_artifacts`.
