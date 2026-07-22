# WildFly testsuite on CratonVM — crash collection

Running the **WildFly 41.0.0.Beta1 testsuite** (`apps/wildfly/testsuite`, ~1,600
test classes) under **CratonVM** to collect as many distinct VM crash reports as
possible. Methodology mirrors the kafka-clients sweep: **one CratonVM JVM per test
class** (via the `KRun` programmatic JUnit-Platform launcher) so a crash/hang in one
class cannot abort the others. Never stops on failure — every class is accounted for.

## Harness (worktree `C:\craton\CratonVM-wildfly`, branch `test/wildfly-suite`)
- `wildfly-suite/KRun.java` — per-class launcher; prints one flushed
  `RESULT <class> found=.. succ=.. fail=.. status=..` line per class, plus
  `FAILCAUSE`/`LOADERR` detail. Engine-agnostic (vintage = JUnit4, jupiter = JUnit5).
- `wildfly-suite/run-wildfly.sh` — driver. `GRAN`=class, batched per module with
  crash-recovery (a crashed batch JVM → remaining classes re-run individually).
  Boots **JDK 25** (`--java-home`) because CratonVM on a boot JDK <19 silently
  no-ops `new Thread(runnable)` → false hangs. External `timeout` is the sole hang
  detector (`CRATONVM_DISABLE_DEFAULT_WATCHDOG=1`). Resumable (skips classes already
  in `results.tsv`).
- `wildfly-suite/harness-cp.txt` — fixed junit platform/vintage/jupiter classpath.
- `wildfly-suite/gen-cp.sh` — emits per-module `target/cratonvm-testcp.txt` deps.
- `wildfly-suite/results/` — `results.tsv`, `crashes.log`, `failcauses.log`,
  `progress.log`, `summary.txt`.

## Status classes
`OK` (all pass) · `FAIL` (ran, ≥1 test/container failure) · `LOADERR` (class load /
discovery threw) · `EMPTY` (no tests found) · `ABEND` (JVM died with no RESULT —
**CratonVM crash**) · `TIMEOUT` (external timeout — **hang**) · `POST-RESULT` (result
emitted but JVM then crashed).

The **ABEND / TIMEOUT / POST-RESULT** classes are the crash reports of interest;
each distinct signature is written up as `bug-NN-*.md` here.

## Live status
See [STATUS.md](STATUS.md) for current exact numbers (updated as the run proceeds).

## Live-container reruns
See [live-container-smoke.md](live-container-smoke.md): with a live managed WildFly server
the smoke group goes from all-FAIL (no container) to **111/111 pass** on HotSpot. Running the
Arquillian **client under CratonVM** is currently blocked by two CratonVM gaps (Surefire
java-CLI args; null `Enumeration` in the client) — both documented there.

## Bugs
| # | Title | Sev | Status |
|---|-------|-----|--------|
| [09](bug-09-surefire-commandreader-await-started.md) | CratonVM's Surefire fork shim left `ForkedBooter.commandReader` live after `setupBooter`, making JUnit4Provider wait for a master command stream that the single-class runner does not provide. | High | **FIXED** - no-JIT and JIT one-class WildFly slices now exit; residual failures are managed-server startup timeouts from `-Dtimeout.factor=1` |
| [10](bug-10-surefire-junit4-description-linkage.md) | Surefire's JUnit4 reflector probes the missing JUnit 4.13 `Description.createSuiteDescription(String)` method and expects to fall back to the annotation-varargs overload. CratonVM now bridges the helper directly. | Med | **FIXED** - old `NoSuchMethodError`/`JUnit4Reflector` linkage signature absent in JIT and no-JIT verification |
| [11](bug-11-file-delete-null-receiver-fallback.md) | The interpreter's `java/io/File` null-receiver compatibility fallback returned `false` for mutators such as `delete()`, so `((File) null).delete()` skipped the required NPE and changed WildFly teardown accounting. | Med | **FIXED** - minimized repro and WildFly `DeployAllServerGroupsTestCase` now match HotSpot's NPE/error shape in JIT and no-JIT |
| [12](bug-12-msc-stabilitymonitor-awaitstability-null-controller.md) | `StabilityMonitor.awaitStability` missed the MSC native cleanup and populated failed/problem sets with null service controllers, causing `ServiceController.provides()` NPE in WildFly domain smoke tests. | High | **FIXED** - focused rerun advanced past the null-controller failure |
| [13](bug-13-surefire-event-channel-buffered-output-interleaving.md) | Surefire fork event frames were corrupted by native PrintStream/BufferedOutputStream output paths, producing dumpstream warnings, zero-test runs, and fork crashes. | High | **FIXED** - focused rerun reports 2 JUnit methods and no new dumpstream |
| [14](bug-14-jboss-modules-multi-entry-modulepath.md) | JBoss Modules bridge kept only the first entry from WildFly domain launchers' multi-entry `-mp`, hiding `org.jboss.as.process-controller` and dependency modules behind per-test `added-modules` roots. | High | **FIXED** - three-entry `-mp` process-controller help repro resolves and exits rc=0 with `mpmulti2` |
| [01](bug-01-stream-foreachordered-abstractmethoderror.md) | `Stream.forEachOrdered` → `AbstractMethodError` "no Code attribute" (CratonVM-only; 27+ classes) | High | **FIXED** — repro'd + verified |
| [02](bug-02-zipfile-entries-null.md) | `ZipFile.entries()`/getName/size return null/0 (CratonVM-only; NPEs ShrinkWrap's package scanner — blocked the live-container client) | High | **FIXED** — repro'd + verified |
| [03](bug-03-regex-perf-deployment-build.md) | Char-by-char loops ~400–1000× slower (Gap C). **(A)** native-bridged String/CharSequence accessors in compiled code — **FIXED** (call-site intrinsics + CharSequence guard; static charAt loop 18 000→195 ms, ~90×). **(B)** instance methods never invocation-tier-up (only static do) so `Pattern$*.match` never compiled — **FIXED, default-OFF** (`CRATONVM_JIT_VIRTUAL_TIERUP`; InstBench 21 613→285 ms ~76×). **(C)** that exposed a JIT→JIT call-boundary miscompile in `Matcher.search` — root-caused (bisect) + **skip-listed** so regex is correct under the flag (replaceAll ~0.5–0.7 ms, was minutes-to-never). Remaining: general JIT→JIT codegen fix + full-suite re-test to flip (B) default-ON | Med | **(A)+(C) FIXED, (B) FIXED default-OFF**; flag-on unblocks ShrinkWrap |
| [04](bug-04-surefire-jvm-dup-xmx.md) | CratonVM as Surefire `-Djvm=` fork (Gap A) — managed-container startup unblocked layer by layer: dup-`-Xmx`, ForkedBooter+JUnit+Arquillian, `Path.toAbsolutePath()` mangle (validateWildFlyDir), `ServerSocket/DatagramSocket.setReuseAddress` NPE, `ProcessBuilder.start` quoted-path (os err 123) — **all 5 FIXED**. Client now runs to the deployment-archive build → only [Gap C/bug-03](bug-03-regex-perf-deployment-build.md) (perf) remains | Med | **5 layers FIXED**; only Gap C perf left |
| [05](bug-05-jit-exception-handler-this-null.md) | JIT-compiled instance method's catch block sees `this`/params as null/uninitialized — `route_jit_exception_through_method` rebuilt the handler frame with `NO_ARGS`. A real, generic production JIT bug (repro's at default threshold), B-gated. Distinct from the WildFly cluster (those die in discovery=bug-06 before reaching this execution path). | High | **FIXED** — standalone repro (`TCRepro` 49500→0) + 6-scenario HotSpot diff + B-off/JIT-off differential |
| [07](bug-07-annotationproxy-dispatch-fastpath.md) | Every method call on a synthetic `AnnotationProxy` paid the full `invoke_or_native` resolution-miss cascade before the bottom "last resort" annotation rescue — pathologically slow for annotation-heavy reflection (JUnit/Arquillian/Spring scanning; ClusteredJPA2LCTestCase timed out under NSME logging). Added an early fast-path in `invoke_or_native` + `jit_invoke_virtual_mic` (cached cid). | Med (perf) | **FIXED** — AnnotationProxy NSME probes 303→0, correctness == HotSpot, no regression |
| [06](bug-06-jit-junit-discovery-reflection-corruption.md) | Reflection mirror arrays (`Field[]`/`Method[]`/`Class[]`/annotation arrays) not GC-rooted while built — a mid-build young GC reclaims/moves the array (held only in a Rust local) → stale ref resolves to a reused bare `java/lang/Object`. **This is the actual WildFly cluster**: `annotationType must not be null`×5, `AbstractMethodError getId/getParent/hasGenericInformation`×3, `ClassCast Object→Status`×1. Not JIT-specific (repros JIT-off under GC stress); B-on exposes it via GC pressure. | High | **FIXED** — `build_mirror_array` pins the array across the fill (lang_class.rs); standalone GC-stress repros + WildFly cluster == HotSpot |
