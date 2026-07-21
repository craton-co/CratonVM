# H2 stored-procedure tests — in-process `javac` compile of the `CREATE ALIAS` body is catastrophically slow under CratonVM (hangs/near-timeouts, not a deadlock)

**Status:** FIXED on `dev` (verified 2026-07-21).
This document preserves the historical failure evidence from the stale
`7aed580f`-based binary.

**Resolution:** `829c7bcf4` (`perf(interpreter): wire the missing invokestatic
inline cache + drop per-call descriptor allocation`, 2026-07-20) is an
ancestor of current `dev` and removes the repeated real-JDK dispatch work
that amplified the `javac` classpath walk into timeout-scale latency.

**Tests (4, all sharing the identical mechanism):**
- `org.hibernate.orm.test.sql.storedproc.ResultMappingTest` — HANG (rc=124, 300s harness cap)
- `org.hibernate.orm.test.sql.storedproc.StoredProcedureTest` — HANG (rc=124, 300s harness cap)
- `org.hibernate.orm.test.sql.storedproc.StoredProcedureResultSetMappingTest` — FAIL, `testPartialResults()` timed out after 120s (JUnit's own per-test timeout, fired live inside the process; class finished at `ms=139424`)
- `org.hibernate.orm.test.jpa.procedure.StoredProcedureResultSetMappingTest` — FAIL, `setup(EntityManagerFactoryScope)` timed out after 120s (class finished at `ms=167422`)

**Found:** 2026-07-21, `apps/hib-suite-runner` "others" category rerun (50
known-historically-failing classes, 8 shards, JIT on, real JDK) against a
binary built from `C:\craton\CratonVM-hib-local-0712` (branch
`test/hib-local-0712`, merged with `origin/dev` HEAD `7aed580f0`), using a
freshly-regenerated `common.args` classpath (not a classpath artifact of that
regeneration). Run dir:
`apps/hib-suite-runner/runs/run-20260721-152142-others/on-real/`
(`shard-0/raw.log`, `shard-1/raw.log`, `shard-2/raw.log`, `shard-3/raw.log`).

## Symptom

All four classes use H2's `CREATE ALIAS <name> AS $$ ... $$` mechanism to
register a small Java method (using `org.h2.tools.SimpleResultSet`) as a
callable stored procedure — see
`hibernate-core/src/test/java/org/hibernate/orm/test/sql/storedproc/H2ProcTesting.java`
(`findOneUser`/`findUsers`/`findUserRange`, used by `ResultMappingTest` and
`StoredProcedureTest`), and the inline `ProcedureDefinition.sqlCreateStrings`
in both `StoredProcedureResultSetMappingTest` variants (`allEmployeeNames`).
H2 compiles this Java source **in-process** at `CREATE ALIAS` execution time
via `org.h2.util.SourceCompiler`, using the *real* JDK's
`javax.tools.JavaCompiler` (`ToolProvider.getSystemJavaCompiler()`), not a
synthetic/stubbed compiler.

In the two `HANG` classes, the process never gets past the very first
`CREATE ALIAS` statement (raw log stops mid-way through printing the SQL,
then the harness's `timeout 300` wrapper kills it, `rc=124`, no `@@RESULT`
ever printed). In the two `FAIL` classes, the class *does* eventually finish
— but at 139s/167s wall-clock, comfortably past JUnit's 120s
`junit.jupiter.execution.timeout.default` — so JUnit reports a
`TimeoutException` even though the operation wasn't infinite, just far too
slow.

## Root cause: real in-process `javac` compiling against the full ~240-jar test classpath is far too slow under CratonVM

Reproduced solo (isolated, single class, no other classes queued in the same
process):

```bash
cd C:/craton/CratonVM/apps/hib-suite-runner
echo "org.hibernate.orm.test.sql.storedproc.StoredProcedureTest" > /tmp/single.txt
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 CRATONVM_DBG_EXIT=1 timeout 200 \
  "C:/craton/CratonVM-hib-local-0712/target/release/cratonvm.exe" \
  --java-home "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot" --Xmx 1500m \
  @common.args -Dcraton.batch=1 CratonRunner /tmp/single.txt 0
```

Result: `rc=124`, hung mid-way through the **first** `CREATE ALIAS
findOneUser` statement (stdout stops immediately after the SQL text is
printed, before the second alias `findUsers` is ever attempted).
`CRATONVM_DBG_EXIT=1` confirms `System.exit` is **never** called during the
hang (ruling out an early alternate hypothesis that a stray `System.exit(0)`
was involved — the `[cratonvm] System.exit(0) called` lines that appear
*after* this test's block in `shard-0/raw.log` belong to the harness's
`cat "$tmp" >> "$RAW"` buffering of the **next** class's process, not this
one; `apps/hib-suite-runner/run-hib.sh`'s `run_shard()` redirects each
child's stdout to a temp file that is only appended to `raw.log` after the
child exits, while stderr is appended live — so blocks can visually
interleave in the merged log even though each class runs in its own fresh
process, `-Dcraton.batch=1`).

Re-running with the VM's own stack-dump watchdog (present on this exact
binary, `vm-cli/src/main.rs`, `--stack-dump-on-timeout`) confirms this is
**not a deadlock** — the main thread is making continuous, genuine forward
progress deep inside the real JDK compiler, not blocked/parked:

```bash
timeout 90 "C:/craton/CratonVM-hib-local-0712/target/release/cratonvm.exe" \
  --java-home "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot" --Xmx 1500m \
  --stack-dump-on-timeout 45 @common.args -Dcraton.batch=1 CratonRunner /tmp/single.txt 0
```

The watchdog re-dumps periodically once armed; across dozens of dumps the
main thread's frame count oscillates 131⟷132 (real, ongoing recursive
descent through the compiler — not a static/frozen deadlock, and not
unbounded stack growth either), while the two background threads
(`Hibernate Connection Pool Validation Thread`,
`junit-jupiter-timeout-watcher`) sit correctly parked on
`AbstractQueuedSynchronizer$ConditionObject.awaitNanos` — all three threads
report `blocked=false`. The full call chain (top of stack down):

```
CratonRunner.main
 → org.junit.platform.launcher...Launcher.execute
  → ... (JUnit5 launcher machinery)
   → org.hibernate.tool.schema.spi.SchemaManagementToolCoordinator.process
    → org.hibernate.tool.schema.internal.SchemaCreatorImpl.{doCreation,createAuxiliaryObjectsAfterTables}
     → org.hibernate.tool.schema.internal.Helper.applySqlString
      → org.hibernate.tool.schema.internal.exec.GenerationTargetToDatabase.accept
       → org.h2.jdbc.JdbcStatement.execute / executeInternal
        → org.h2.command.Command.executeUpdate → CommandContainer.update
         → org.h2.command.ddl.CreateFunctionAlias.update
          → org.h2.schema.FunctionAlias.{newInstanceFromSource,init,load,loadFromSource}
           → org.h2.util.SourceCompiler.{getMethod,getClass,javaxToolsJavac}
            → com.sun.tools.javac.api.JavacTaskImpl.{call,doCall,invocationHelper}
             → com.sun.tools.javac.main.JavaCompiler.{compile,enterTrees}
              → com.sun.tools.javac.comp.Enter.{main,complete}
               → com.sun.tools.javac.code.Symbol(ClassSymbol).complete
                → com.sun.tools.javac.comp.TypeEnter.{complete,Phase.completeEnvs,doCompleteEnvs}
                 → com.sun.tools.javac.comp.TypeEnter$ImportsPhase.{runPhase,resolveImports,checkClassPackageClash}
                  → com.sun.tools.javac.code.ClassFinder.{complete,fillIn,scanUserPaths,list}
                   → com.sun.tools.javac.file.JavacFileManager.list
                    → JavacFileManager$ArchiveContainer.{list,visitFile}   <-- bottom of dump, walking one classpath jar's zip entries
```

i.e. **CratonVM is genuinely running the real JDK's `javac` front-end
in-process** (this is expected — H2's `SourceCompiler.javaxToolsJavac` really
does call `ToolProvider.getSystemJavaCompiler()`), and that compiler is stuck
resolving the single import (`import org.h2.tools.SimpleResultSet;`) by
scanning the **entire test classpath** — `apps/hib-suite-runner/common.args`
is a `-cp` argfile with **~241 jars** (`GUIDE.md` §1) — jar by jar, entry by
entry (`ArchiveContainer.visitFile`), plus a `com.sun.tools.javac.comp.Modules
.setupAllModules`/`.enter` pass (JDK 9+ module-system bootstrap that `javac`
always performs once per compilation). On real HotSpot this same operation —
compiling ~15 lines of Java against a large `-cp` — completes in well under a
second (this exact 4-class set was confirmed passing quickly, `ok=4/4/1/1
failed=0`, in the 2026-07-06 HIB-CV-27 regression-verification run, see
below); under CratonVM's interpreter/dispatch path it does not finish within
a 90-200s solo wall-clock budget. This is consistent with — and a new
concrete instance of — the general "hot native-call/dispatch overhead"
performance-regression family already documented elsewhere in this codebase
(e.g. `reference_hashmap_native_call_dispatch_overhead_20260711`,
`reference_junit5_execution_machinery_dispatch_overhead`,
`reference_hot_op_helperization_trap`): a code path whose cost scales with
iteration count (here, ~241 jars × N zip entries each, scanned per compiled
`ALIAS`) multiplies CratonVM's higher per-call overhead into an overall
100×+ slowdown, tipping a sub-second HotSpot operation into a multi-minute
CratonVM one.

## Not the previously-fixed HIB-CV-27 bug — and not simple host-load noise either

**Not HIB-CV-27.** `docs/internal/hib-linux-fail-bucket-triage-20260703.md`
documents these exact 4 classes previously failing with a hard *compile
error* (`package org.h2.tools does not exist`), root-caused to
`java.io.File.<clinit>` never populating `file.separator`/`path.separator`
(CratonVM's `System.getProperties()` singleton has a null backing `map`),
which broke `javac`'s classpath-string decoding. That fix is confirmed
**still present and working** here — every repro in this investigation logs
`Post-clinit fixup: File fs/separator/pathSeparator populated (5/5)`
successfully before `CREATE ALIAS` ever executes, and the 2026-07-06
verification run recorded all 4 classes passing cleanly and quickly
(`sql.storedproc.ResultMappingTest ok=4 failed=0`,
`sql.storedproc.StoredProcedureResultSetMappingTest ok=1 failed=0`,
`sql.storedproc.StoredProcedureTest ok=4 failed=0`,
`jpa.procedure.StoredProcedureResultSetMappingTest ok=1 failed=0`). Something
between 2026-07-06 and now turns a working-but-apparently-borderline-slow
compile into one that actually trips timeouts — this doc does not
pin down whether that's a genuine new interpreter/dispatch regression in the
interim (a large number of commits landed across many branches in this
window) or whether the operation was always this slow and simply didn't
(yet) lose the race against `common.args`'s classpath size /
concurrent-host load at verification time. Either way, the *mechanism* is
now root-caused, which the prior docs did not do.

**Not simple host-load noise, either — though the shared host doesn't help.**
Two prior docs
(`docs/internal/fixed-suite-bugs/hib-generic-timeout-hang-longtail-resolved-20260715.md`,
`docs/internal/hib-linux-fail-bucket-triage-20260703.md`) explicitly flag
`sql.storedproc.{ResultMappingTest,StoredProcedureTest}` as having shown
inconsistent PASS/FAIL/HANG/CRASH results across reruns on this same
heavily-shared, multi-tenant host, and conclude (without finding a root
cause) that the whole 61-class "longtail" bucket is "likely a mix of real
slowness and this run's heavy host contention." This investigation's solo
repro **was not on a fully idle host** either — `Get-CimInstance
Win32_Process` during the repro showed ~9 other concurrent `cratonvm.exe`
processes from other sessions (including a full second 8-shard
`hib-suite-runner` sweep that started mid-repro) — so host noise was still a
factor and this doc cannot claim a clean-room quiet-host measurement. What
it *can* claim: the operation is genuinely CPU-bound (confirmed via two
successive `Get-Process`/`CPU` samples 15s apart, +8.6s of CPU time
consumed — not flat/parked) and spends that CPU time recursing through real
`javac`'s classpath-scanning machinery, a cost that scales with
`common.args`'s ~241-jar classpath — not waiting on a lock or a
timing-sensitive race. That reconciles the prior "noise" conclusion with a
concrete mechanism: the compile is *inherently* slow enough on CratonVM to
sit right at the edge of the 120s/300s timeout thresholds, so *any*
additional host contention (which this box has in abundance — see
`reference_shared_host_multitenant_confound`) is enough to tip an individual
run over one threshold or another, explaining the previously-observed
non-determinism without requiring the host to be the root cause.

## Why the fourth class's failure mode differs slightly

`sql.storedproc.StoredProcedureResultSetMappingTest`'s raw log
(`shard-1/raw.log:993-1066`) shows the `CREATE ALIAS allEmployeeNames`
compile complete in normal time this run, and the subsequent `{call
allEmployeeNames()}` JDBC call also execute and extract all 3 result rows
successfully — the stall happens *after* that, before the test method
returns (class finishes at `ms=139424`, i.e. ~19s past the 120s JUnit
per-test deadline that had already fired and been reported). This is
consistent with either (a) the same classpath-scan-cost mechanism landing at
a different point in this run due to host-load timing (this class's
`ProcedureDefinition` triggers its own separate `CREATE ALIAS` compile at
schema-export time, same as the others, just apparently faster this run), or
(b) a related-but-distinct slow path in Hibernate's
`ProcedureCall`/`ResultSetOutput`/`@ConstructorResult` reflective row-mapping
after the raw JDBC extraction. This doc does not distinguish between (a) and
(b) conclusively — flagged as a follow-up rather than over-claimed.

## Suggested fix direction (not applied — investigation only)

Not a single-line fix candidate — this is a systemic interpreter/dispatch
overhead issue in a code path (real `javac`'s classpath scanner) that
CratonVM has no reason to special-case, unlike the JDK-internal natives
usually patched in this codebase. Possible directions for follow-up:
- Profile `JavacFileManager$ArchiveContainer.list`/`visitFile` and
  `ClassFinder.scanUserPaths` specifically under CratonVM to find which
  native call(s) in that hot loop carry disproportionate per-call overhead
  (zip/jar entry iteration, `Path`/`NIO` file-visitor dispatch, and
  `HashMap`/`HashSet` operations inside `javac` are all plausible candidates
  given precedent elsewhere in this codebase).
- Consider whether `common.args`'s ~241-jar classpath is itself avoidable
  for this specific harness (e.g., a slimmer classpath for the in-process
  `javac` invocation specifically) — but this is a harness workaround, not a
  CratonVM fix, and wouldn't address the underlying dispatch-overhead defect
  for real-world applications that also do in-process compilation against
  large classpaths (Groovy, JSP, H2, Janino, etc. all use this pattern).

## Reproduction

```bash
cd C:/craton/CratonVM/apps/hib-suite-runner
echo "org.hibernate.orm.test.sql.storedproc.StoredProcedureTest" > /tmp/single.txt
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 timeout 200 \
  "C:/craton/CratonVM-hib-local-0712/target/release/cratonvm.exe" \
  --java-home "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot" --Xmx 1500m \
  @common.args -Dcraton.batch=1 CratonRunner /tmp/single.txt 0
# rc=124, hangs mid-first-CREATE-ALIAS every time in this investigation's runs
```

For a stack dump instead of just a kill, use `--stack-dump-on-timeout <secs>`
in place of `CRATONVM_DISABLE_DEFAULT_WATCHDOG=1` (see above).

## Final verification (2026-07-21)

Azure host `20.83.144.174`, clean current-`dev` release build, real JDK 25,
and the harness's full ~241-jar `common.args` classpath; every class used a
fresh process and retained its normal JUnit timeout settings.

| Mode | Class index | Result | Wall time |
|---|---:|---|---:|
| JIT | 0 | `ResultMappingTest`: 4/4 passed | 58.92s |
| JIT | 1 | `StoredProcedureTest`: 4/4 passed | 61.01s |
| JIT | 2 | SQL result-set mapping: 1/1 passed | 34.70s |
| JIT | 3 | JPA result-set mapping: 1/1 passed | 22.55s |
| `--nojit` | 0 | `ResultMappingTest`: 4/4 passed | 44.63s |
| `--nojit` | 1 | `StoredProcedureTest`: 4/4 passed | 46.83s |
| `--nojit` | 2 | SQL result-set mapping: 1/1 passed | 60.83s |
| `--nojit` | 3 | JPA result-set mapping: 1/1 passed | 19.12s |

All eight processes exited `0`; no class reached its former 120s JUnit or
300s harness timeout.
