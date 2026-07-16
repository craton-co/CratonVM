# 120-second JUnit timeout cluster — 7 classes, one suspected systemic cause

| | |
|---|---|
| **Status** | OPEN — new finding, 2026-07-16 full-suite rerun. Not individually root-caused yet. |
| **Area** | Suspected: JIT/interpreter throughput, GC pause behavior, or native-call dispatch overhead under real-JDK+JIT-on mode. |
| **Severity** | Medium — no crashes or wrong results, but a real perf/timing gap wide enough to blow through Hibernate's own generous internal timeouts. |

## Symptom

Seven unrelated Hibernate test classes, spanning batching, JSON functions,
ID generation, insert ordering, boot scanning, SQL execution, and type
contribution, all fail with the identical shape:

```
java.util.concurrent.TimeoutException: <testMethod>(...) timed out after 120 seconds
```

This is Hibernate's own `@Timeout(120)`-style internal JUnit guard on
individual test methods — not the harness's outer `TIMEOUT=1200` (all of
these classes complete well under the harness timeout; the *test method
itself* reports having exceeded a 120-second internal watchdog).

## Affected classes (2026-07-16 rerun, real-JDK, JIT-on, `dev@2f02e939d`)

| Class | Method | Elapsed (ms) | Notes |
|---|---|---|---|
| `org.hibernate.orm.test.batch.BatchTest` | `testBatchInsertUpdate` | 320518 | |
| `org.hibernate.orm.test.batchfetch.DynamicBatchFetchTest` | `testMultiLoad` | 270408 | |
| `org.hibernate.orm.test.function.json.JsonArrayUnnestTest` | `testUnnest` | 523811 | |
| ~~`org.hibernate.orm.test.id.uuid.rfc9562.UUidV6V7GeneratorTest`~~ | ~~`testMonotonicityUuid6`~~ | — | **FIXED — re-attributed, see below. Not a throughput/timeout member of this cluster.** |
| `org.hibernate.orm.test.insertordering.InsertOrderingRCATest` | `testBatching` | 165898 | |
| `org.hibernate.orm.test.bootstrap.scanning.ScannerTest` | `testCustomScanner` | 134963 | |
| `org.hibernate.orm.test.sql.exec.SmokeTests` | `testQueryConcurrency` | 193477 | Contradicts the 2026-07-14 "all sql.exec.* classes pass" note in the archived assertion-longtail doc — this specific concurrency-flavored test within `SmokeTests` still times out; the other 16 tests in the class pass. |
| `org.hibernate.orm.test.type.contributor.LiteralRenderingTest` | `testIdVersionFunctions` | 347729 | |

(`org.hibernate.orm.test.jpa.lock.LockTest` has a related but distinct
symptom — `AssertionFailedError: execution exceeded timeout of 5000ms by
2180ms` on a much tighter 5-second internal timeout — tracked separately in
[hib-misc-residuals-20260716.md](hib-misc-residuals-20260716.md) since it may
be a different, more timing-sensitive class of issue.)

## Working hypothesis

All 7 failures are "test completes correctly but too slowly," not wrong
results — `ok` count is one less than `found` in every case, with the single
failed test being exactly the one that tripped its internal timeout. The
shared shape (a generous-but-finite internal timeout, tripped only under
CratonVM) suggests a single systemic throughput gap versus HotSpot — most
likely in JIT-compiled hot-path dispatch, GC pause frequency/duration, or
native-call overhead for JDBC/H2 round-trips — rather than 7 independent
bugs in unrelated Hibernate subsystems. `testQueryConcurrency` in particular
is the kind of test name that stresses concurrent/tight-loop execution,
consistent with a throughput explanation over a correctness one. (An eighth
class, `UUidV6V7GeneratorTest`, was originally filed here on the same
"tight-loop" reasoning but has since been re-investigated and removed —
see the dedicated section below. It turned out to be a real GC-corruption
correctness bug wearing a timeout costume, now fixed, unrelated to this
cluster's throughput hypothesis.)

## Not yet done

- HotSpot timing comparison on the same 7 methods (same harness, same
  `TIMEOUT`, `HOTSPOT=1`) to quantify the actual CratonVM/HotSpot elapsed-time
  ratio and confirm this is CratonVM-specific rather than environmental
  (shared host contention was ruled out for prior clusters this session, but
  this is a fresh local-host run and hasn't been checked against a quiet
  baseline).
- Per-class profiling (`CRATONVM_DBG_JIT_DISASM`, GC pause counters) to
  narrow "systemic slowness" down to a specific subsystem.

## RESOLVED (2026-07-16, separate session): `UUidV6V7GeneratorTest` was never a throughput/timeout member of this cluster — real GC-corruption bug, now FIXED

`org.hibernate.orm.test.id.uuid.rfc9562.UUidV6V7GeneratorTest#testMonotonicityUuid6`

**Status: FIXED.** Removed from the table above. A baseline-measurement pass
on this same host (`dev@dcb24161`) found that, on a fresh build, this class
does **not** actually hit the 120s internal JUnit timeout at all — it fails
in ~47s with two genuine errors:

```
AbstractMethodError: method java/lang/instrument/Instrumentation.isModifiableClass(Ljava/lang/Class;)Z has no Code attribute
```

plus an `ArrayIndexOutOfBoundsException` later in the same run. The original
622550ms/120s-timeout row (still shown historically in this doc's git
history) came from an older dev tip; the AbstractMethodError/AIOOBE shape is
what a fresh build at `dcb24161` actually produces, not a timeout.

**Investigation.** This test's setup calls `Mockito.mock(SharedSessionContractImplementor.class)`,
which drives Mockito's inline mock maker through ByteBuddy's self-attach
`Instrumentation` path (see §12 of
[HIB-misc16-correctness-sweep.md](../../internal/HIB-misc16-correctness-sweep.md)
for the original, now-superseded NPE fix in this exact call chain). Deep
tracing into `vm/src/runtime/instrument.rs`'s native registrations and the
invoke-cache/vtable-fast dispatch paths in `vm/src/runtime/interpreter.rs`
(a plausible-looking but ultimately wrong hypothesis — the interface-dispatch
caching added 2026-07-15 for `execute_invokevirtual_cached`/
`execute_invokevirtual_vtable_fast` was suspected of mis-resolving
`invokeinterface Instrumentation.isModifiableClass` to the interface's own
abstract declaration, but every code path found there does properly guard on
the receiver's actual runtime class) turned out to be a dead end: rebuilding
fresh at the then-current `origin/dev` tip (`bb2adc87`, worktree
`wt-hib-uuid-instrumentation-20260716`) reproduced **no failure at all** —
`testMonotonicityUuid6` and `testMonotonicityUuid7` both pass cleanly
(`found=2 started=2 ok=2 failed=0`, verified twice: `ms=167052` with
`-Dcraton.trace=true`, `ms=341255` without — timing variance tracks this
heavily shared host's load, not flakiness in the fix).

**Root cause: already fixed by commit `db047d38`**, landed a few commits
ahead of the `dcb24161` baseline (found by an unrelated `DefaultCatalogAndSchemaTest`
investigation session, see
[hib-misc-residuals-20260716.md](hib-misc-residuals-20260716.md)). That
commit closed unrooted-`ObjectRef` GC-safety gaps in
`native-builtins/src/lang_class.rs`'s `collect_public_fields`/
`collect_public_methods` (`Class.getFields()`/`getMethods()`),
`native_method_get_parameter_annotations`, and `create_annotation_proxy` —
exactly the reflection surface ByteBuddy's mock-subclass generator hammers
while introspecting `SharedSessionContractImplementor` (a large interface).
A GC triggered mid-walk could relocate/reclaim an already-minted
`Field`/`Method`/`Annotation` mirror, corrupting a later-dereferenced
pointer — plausibly explaining both the `AbstractMethodError` (a corrupted
receiver can present as anything, including "looks like it only has the
interface's abstract declaration") and the `ArrayIndexOutOfBoundsException`
(a corrupted array/index downstream of the same unrooted walk) as two
manifestations of one memory-safety bug rather than two independent issues.
This is the same defect family `db047d38`'s own message documents fixing
for `DefaultCatalogAndSchemaTest`'s JAXB/HBM-XML reflection path — this
class was simply a second, independent trigger of the identical gap via a
different reflection-heavy code path (Mockito/ByteBuddy instead of JAXB).

**No VM code change made by this session** — the fix was already on `dev`
before this worktree was even created; this session's contribution is the
diagnosis (ruling out the `Instrumentation`-dispatch hypothesis, confirming
the GC-corruption attribution, and this re-classification) plus this doc
update. Build/verification SHA: `bb2adc87` (built and tested; no commits of
this session's own needed pushing beyond this doc). `runtime::instrument`'s
16 Rust unit tests were re-run for completeness (unrelated to the actual
fix, but the area this session initially suspected) and pass 16/16.

**Residual (not blocking, noted per this doc's own throughput theme):**
167s–341s CratonVM wall time for the whole class (both methods) is still
roughly 35–70x HotSpot's ~4.8s ballpark for the same class. Each individual
test method stays under Hibernate's 120s per-test internal timeout so this
does not reproduce as a failure, but if a future session tightens that
margin (e.g. via other perf work making individual iterations slower, or a
slower host), it could resurface as a *bona fide* timeout — in which case
it would genuinely belong back in this cluster's throughput bucket. Not
pursued further this session per the task's explicit scope (throughput is
a separate, non-blocking concern once correctness is confirmed).

## Related finding (2026-07-16): CriteriaBuilderNonStandardFunctionsTest joins this shape, root cause narrowed to JIT compile-time tax

Investigated as a separately-filed "real constraint violation" residual
(see [hib-misc-residuals-20260716.md](hib-misc-residuals-20260716.md)'s
CriteriaBuilderNonStandardFunctionsTest entry for the full writeup). Same
symptom shape as this cluster (prepareData(...) TimeoutException after
120s, ok = found - 1). Bisected via --nojit (3/3 clean) and via
JIT-nominally-on-but-thresholds-raised-so-compilation-never-triggers (2/2
clean) versus default JIT-on (0/6 clean immediately prior) -- both
bisections converge on JIT compilation-time tax in a short-lived process,
the same mechanism this file's sibling LockTest entry (in
hib-misc-residuals-20260716.md) found independently the same day. Live gdb
capture during one stall also caught a genuine, narrow, uncached
per-call class-hierarchy-walk allocation (native-collections/src/lib.rs's
al_slots_for -> classloading/src/class.rs's is_subclass_of) as a
contributing CPU-bound hot path, though not proven to be the dominant cost.
Worth folding into whichever session picks up this cluster's "per-class
profiling" next step -- the JIT-compile-tax bisection methodology (disable
JIT vs raise compile thresholds vs default) is a fast, cheap first cut that
could be run against the other 7 classes here before deeper profiling.

## Follow-up (2026-07-16, later session): threshold-raise fix landed on dev, checked against this cluster -- inconclusive, do not assume it helps

`fix/jit-compile-time-tax-20260716` (merged to dev) raised
`jit/src/tiered.rs`'s `CompilationPolicy` defaults from
`c1_threshold=200/c2_threshold=5000` to `c1_threshold=1500/c2_threshold=20000`
-- see the `LockTest`/`CriteriaBuilderNonStandardFunctionsTest` entries in
[hib-misc-residuals-20260716.md](hib-misc-residuals-20260716.md) for the
full validation writeup. Short version: it's a real, safe, validated
mitigation (no steady-state throughput regression on a `fib(32)` A/B or the
`vm/benches/vm_benchmarks.rs` suite) but it did **not** reliably fix either
of those two classes -- a solo `LockTest` run on the fixed binary still
triggered 341 compile-task enqueues across 128 distinct methods (JUnit5
reflection-discovery + H2 internals called thousands of times even within
one short process), and `CriteriaBuilderNonStandardFunctionsTest` still hit
its 120s `TimeoutException`.

A spot-check of one class from this cluster was attempted --
`org.hibernate.orm.test.batch.BatchTest` -- but was **inconclusive**: the
solo run was launched under a 200s `timeout` wrapper that turned out to be
shorter than this class's own previously-recorded ~320s
(`testBatchInsertUpdate` = 320518ms in the table above), so the process was
killed before producing any `@@RESULT`/`@@FAIL` line. Re-running with a
longer wrapper (400s+) was not done this round due to time constraints, so
this cluster's classes remain **unchecked** against the threshold-raise fix.
`ScannerTest` was not re-attempted either (the `hib-misc-residuals` doc's
`--nojit` corroboration attempt against it already hit the same
`could not interpret url` packaging/classpath harness limitation tracked in
that doc's `JarVisitorTest` entry, unrelated to the JIT).

Given the `LockTest`/`CriteriaBuilderNonStandardFunctionsTest` result above
(fix does not suppress compilation for reflection-heavy workloads, just
delays it), there is no reason to assume the threshold raise resolves any
of these 7 classes either -- if anything the evidence points the other way
(these classes' methods almost certainly also cross 1500 invocations well
within their multi-hundred-second runtimes). Leaving this cluster's
individual classes unchecked and OPEN rather than claiming a benefit that
wasn't demonstrated. Next session picking this up should rerun the
JIT-compile-tax bisection (`--nojit` / raised-threshold / default) against
2-3 of these classes with a `timeout` wrapper generously longer than each
class's own previously-recorded elapsed time, using the new
`CRATONVM_DBG_TIER_ENQUEUE` diagnostic (added by the fix, in
`jit/src/tiered.rs`) to confirm compile activity directly rather than
inferring it from pass/fail alone.
