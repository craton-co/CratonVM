# 120-second JUnit timeout cluster — 7 classes, one suspected systemic cause

| | |
|---|---|
| **Status** | MOSTLY CLOSED (2026-07-17). 4 of 7 original classes were a mis-attributed reflection/GC-corruption bug, now fixed (see below). `LiteralRenderingTest` is effectively resolved (now ~1.6x HotSpot). `InsertOrderingRCATest` is a confirmed, profiled generic architectural gap (~9.7x HotSpot) -- not independently fixable without broader interpreter/JIT throughput work; not a discrete bug. `BatchTest` is now CONFIRMED FIXED — a clean solo run with a 1200s timeout completed in 153.9s with zero failures and zero corruption warnings (see below); it was never a throughput-cluster member, just slow to get a clean measurement window on this heavily-contended host. |
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
| ~~`org.hibernate.orm.test.batch.BatchTest`~~ | ~~`testBatchInsertUpdate`~~ | — | **FIXED — see below. Confirmed clean pass (2026-07-17), 153863ms solo, zero corruption warnings; never actually a throughput-cluster member.** |
| ~~`org.hibernate.orm.test.batchfetch.DynamicBatchFetchTest`~~ | ~~`testMultiLoad`~~ | — | **FIXED — see below. Was the reflection/GC-corruption family, not a throughput issue.** |
| ~~`org.hibernate.orm.test.function.json.JsonArrayUnnestTest`~~ | ~~`testUnnest`~~ | — | **FIXED — see below. Was the reflection/GC-corruption family, not a throughput issue.** |
| ~~`org.hibernate.orm.test.id.uuid.rfc9562.UUidV6V7GeneratorTest`~~ | ~~`testMonotonicityUuid6`~~ | — | **FIXED — re-attributed, see below. Not a throughput/timeout member of this cluster.** |
| `org.hibernate.orm.test.insertordering.InsertOrderingRCATest` | `testBatching` | 165898 | **Re-investigated 2026-07-17 -- profiled, confirmed generic architectural gap, not independently fixable. See dedicated section below.** |
| ~~`org.hibernate.orm.test.bootstrap.scanning.ScannerTest`~~ | ~~`testCustomScanner`~~ | — | **FIXED — see below. Was the reflection/GC-corruption family (confirmed *infinite livelock* on a prior baseline, not just a timeout), not a throughput issue.** |
| ~~`org.hibernate.orm.test.sql.exec.SmokeTests`~~ | ~~`testQueryConcurrency`~~ | — | **FIXED — see below. Was (at least partly) the reflection/GC-corruption family, not purely a throughput issue. Combined-run residual (2026-07-17) also now confirmed fixed.** |
| `org.hibernate.orm.test.type.contributor.LiteralRenderingTest` | `testIdVersionFunctions` | 347729 | **Re-investigated 2026-07-17 -- now within the documented generic-gap range (~1.6x HotSpot); effectively resolved by cumulative fixes landed since this baseline. See dedicated section below.** |

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

## RESOLVED (2026-07-16, separate session): `ScannerTest`, `JsonArrayUnnestTest`, `DynamicBatchFetchTest` were never throughput/timeout members of this cluster — same GC-corruption family as `UUidV6V7GeneratorTest`/`DefaultCatalogAndSchemaTest`, now FIXED; `SmokeTests` fixed standalone, load-dependent residual open

Follow-up to a baseline-measurement pass on this same host
(`dev@dcb24161`) that found several of this cluster's classes don't
actually reproduce as "slow but correct" at all — they crash or livelock
with the same reflection/GC-corruption signature as
`DefaultCatalogAndSchemaTest`/`UUidV6V7GeneratorTest` above:
`ScannerTest#testCustomScanner` was a **confirmed infinite livelock**
(identical warning repeating forever at the same microsecond timestamp, no
forward progress); `BatchTest#testBatchInsertUpdate` and
`JsonArrayUnnestTest#testUnnest` crashed with a `NullPointerException`
inside JUnit Platform Launcher's `OutcomeDelayingEngineExecutionListener`
after `gen_heap::get_field: out-of-bounds field read dropped` /
`Stale pointer detected in invokevirtual receiver` warnings;
`DynamicBatchFetchTest#testMultiLoad` and `SmokeTests#testQueryConcurrency`
were suspected same-family but not fully confirmed.

**Verified FIXED this session** against the `dev` tip built from
`fix/hib-reflection-gc-sweep-20260716` (this session's `generics.rs` GC-safety
sweep, merged with a concurrent session's `gen_heap.rs` young-object-start-walk
fix — see
[hib-misc-residuals-20260716.md](hib-misc-residuals-20260716.md)'s
`DefaultCatalogAndSchemaTest` entry for the full root-cause writeup, which
applies identically here):

| Class | Result | Notes |
|---|---|---|
| `ScannerTest` | **PASS** `found=2 started=2 ok=2 failed=0` (ms=264299) | Was a confirmed infinite livelock; now completes cleanly. Tested against this session's `generics.rs` fix alone (pre-merge with the `gen_heap.rs` fix). |
| `JsonArrayUnnestTest` | **PASS** `found=5 started=5 ok=5 failed=0` (ms=412184) | All 5 tests in the class pass, including `testUnnest`. Tested against the `generics.rs` fix alone. |
| `DynamicBatchFetchTest` | **PASS** `found=2 started=2 ok=2 failed=0` (ms=783098) | Both tests pass, including `testMultiLoad`. Tested against the `generics.rs` fix alone. |
| `SmokeTests` | **FIXED — confirmed both standalone and combined** `found=17 started=16 ok=16 failed=0 aborted=0 skipped=1` (ms=190461 standalone) | All 16 started tests pass (including `testQueryConcurrency`), 1 expected skip. Previously: when run back-to-back with `JsonArrayUnnestTest` + `DynamicBatchFetchTest` in one JVM process (same list, same run), this class crashed at discovery with `AbstractMethodError` and the same broad `Stale pointer detected` cascade (touching `java/util/Optional`, `org/junit/platform/launcher/LauncherSession`, `java/util/List`) as `DefaultCatalogAndSchemaTest` pre-`gen_heap.rs`-fix — that run predated the `gen_heap.rs` merge. **Re-verified post-merge 2026-07-17**: `JsonArrayUnnestTest` + `DynamicBatchFetchTest` + `SmokeTests` run back-to-back in one `CratonRunner` process (same order) all pass cleanly (`found=5 ok=5`, `found=2 ok=2`, `found=17 started=16 ok=16 skipped=1` respectively) with **zero** `Stale pointer detected`/`AbstractMethodError` occurrences anywhere in the combined log — the `gen_heap.rs` young-object-start-walk fix does fully close this residual. |
| `BatchTest` | **FIXED — confirmed 2026-07-17** | A follow-up session got the clean, definitive result this row asked for: solo run, `timeout 1200`, `dev`-tip binary (`3dcf81e5` + this session's own unrelated BigInteger fix, worktree `wt-hib-verify-20260717`) — `@@RESULT 0 ...BatchTest found=4 started=4 ok=4 failed=0 aborted=0 skipped=0 ms=153863`. Zero stale-pointer/corruption warnings in the log. 153.9s is well inside Hibernate's 120s-per-test internal timeout (each of the 4 individual `@Test` methods stays under it even though the class total exceeds it) and comfortably inside the 1200s solo wrapper — this class was never a genuine member of this cluster's "correct but too slow" throughput bucket; it just needed a clean measurement window free of the extreme host contention (load average 10-230) that blocked every prior attempt at getting a `@@RESULT` at all. |

Given 3 of these classes (`ScannerTest`, `JsonArrayUnnestTest`,
`DynamicBatchFetchTest`) are unambiguously fixed and no longer belong in
this cluster's table at all (removed above), `SmokeTests`' previously
unverified batch-load residual is now also confirmed fixed (2026-07-17: a
follow-up session ran `JsonArrayUnnestTest` + `DynamicBatchFetchTest` +
`SmokeTests` back-to-back in one `CratonRunner` process, same order as the
original failing observation, post-`gen_heap.rs`-fix — all three passed
cleanly: `JsonArrayUnnestTest found=5 ok=5`, `DynamicBatchFetchTest
found=2 ok=2`, `SmokeTests found=17 started=16 ok=16 skipped=1`, zero
`Stale pointer`/`AbstractMethodError` occurrences anywhere in the combined
log), and `BatchTest` is also now confirmed fixed (see above), this
cluster's original "7 classes, one systemic throughput cause" framing
needs revision further: at least 5 of the original 7 were never a
throughput issue — they were mis-attributed instances of the same
reflection/GC-corruption family documented in
`hib-misc-residuals-20260716.md`. Only `InsertOrderingRCATest` and
`LiteralRenderingTest` remain plausible members of an actual throughput
cluster (and `LiteralRenderingTest` is itself now within the documented
generic-gap range, effectively resolved — see below); `SmokeTests`' former
load-only residual is fully closed, no longer open in any form.

## Follow-up (2026-07-17, throughput-profiling session): `InsertOrderingRCATest` and `LiteralRenderingTest` profiled -- one is generic gap (unfixable here), one is effectively resolved

Session scope: the task doc handed off three suspected-generic-gap cases --
`InsertOrderingRCATest#testBatching` and `LiteralRenderingTest#testIdVersionFunctions`
from this cluster, plus `LockTest` (tracked in
[hib-misc-residuals-20260716.md](hib-misc-residuals-20260716.md), see that
file's update below). Built fresh at `dev@33df5d3c` (worktree
`wt-hib-throughput-profile-20260716`; this doc-update commit itself is from a
second worktree, `wt-hib-throughput-docs-20260717`, at the later tip
`dev@3cb39d87`, after the first worktree's git registration was lost to
concurrent host activity -- see the environment note below).

**Environment note for future sessions reusing `/data/hibsrc-baseline-20260716`
/ `/data/hib-baseline-runner-20260716`:** at session start, the shared fixture's
`hibernate-core/target/` and `hibernate-testing/target/` (plus every other
module's `target/`) were **empty** -- wiped by unrelated host disk-pressure
cleanup, and `~/jdk25` was a **dangling symlink** (its target,
`/data/data/jdk25-real`, was gone too). Recovered by: downloading a fresh
Temurin 25.0.3+9 JDK to `/data/jdk25-real-20260717` and repointing `~/jdk25`
at it, then rebuilding the fixture with
`JAVA_HOME=~/jdk25 GRADLE_USER_HOME=/data/gradle-home-baseline-20260716
./gradlew :hibernate-core:testClasses :hibernate-testing:jar
:hibernate-community-dialects:jar :hibernate-scan-jandex:jar :hibernate-ant:jar
:hibernate-reveng:jar` (gradle's build cache made this a ~3-minute job, not a
full rebuild). **Symptom if this recurs:** `Class.forName` on any Hibernate
test class throws a bare `java.lang.ClassNotFoundException` with no message
and no cause even though the `.class` file demonstrably exists and reads
correctly -- the real failure is a missing *dependency* class (here,
`hibernate-testing`'s `EntityManagerFactoryBasedFunctionalTest`/
`ServiceRegistryProducer`, because only the `jar` artifact, not the `classes/`
dir, is on `common.linux.args`'s classpath, and `:hibernate-core:testClasses`
alone doesn't force-build sibling-module jars). This masking is itself a
minor CratonVM diagnostics gap (real-JDK-mode `ClassLoader.loadClass`'s
`load_class_visible_to` in `native-builtins/src/classloader_real.rs` discards
the true underlying error and always reports a bare CNFE) -- not fixed this
session (out of scope for a throughput task), flagged separately.

### `InsertOrderingRCATest#testBatching` -- generic gap, confirmed, not independently fixable

**Repro, current dev tip:** 70095ms / 71987ms (two runs) vs HotSpot's 7350ms
(from the original baseline table) = **~9.7x**. This is already a large
improvement over both previously-recorded numbers for this class (366877ms /
49.9x in the task handoff table, 165898ms in this doc's own 2026-07-16 table)
-- attributable to the cumulative reflection/GC-safety and JIT-policy fixes
other sessions landed on `dev` this same day, not to anything done this
session. The remaining ~9.7x gap is still above the documented ~2-5x
allocation-heavy-tight-loop ceiling, so it warranted profiling rather than
an assumed pass.

**Profiling (three checks, each a clean bisection):**
1. **`--verbose:gc`: zero GC events for the entire 65-82s run.** The test's
   object graph (`DefaultTemplatesVault.getDefaultRCATemplates()`, a fixed
   set of ~20 RCA templates with nested causes/expressions, ~500 total
   persisted rows) is too small to trigger a single young collection. Rules
   out GC pause overhead entirely -- this is not a memory-traffic-bound
   workload despite superficially resembling one.
2. **`--nojit` is *slower*, not faster** (82140ms vs 70-72s with JIT on).
   This is the opposite signal from `LockTest`/
   `CriteriaBuilderNonStandardFunctionsTest` (where `--nojit` fixes the
   timeout and is faster) -- it rules out JIT compile-time tax as the
   dominant mechanism here. JIT is net-beneficial on this workload; the
   gap is not "JIT overhead exceeds payback," it's raw execution cost.
3. **`CRATONVM_DBG_TIER_ENQUEUE`: 2260 distinct methods enqueued, all at
   tier=C1, zero at C2**, spread across the *entire* 65-second run (first
   enqueue at 267ms, last at 65685ms) rather than clustered at startup.
   Each method crosses the c1_threshold (1500 invocations) only once, late,
   from being called a modest, steady number of times across many different
   code paths -- not from a tight hot loop. No method ever accumulates the
   20000 invocations needed for C2. Combined with check 2, this means most
   of the run's CPU time is spent in either the interpreter or
   once-compiled-C1 code across a very *wide* set of methods, never in
   fully-optimized C2 code.

**Workload shape, from the JDBC trace log:** 184 distinct
`Created JDBC batch` / `Executing JDBC batch` events (i.e. 184 separate
`PreparedStatement` shapes) for ~500 total inserted rows -- averaging under 3
rows per batch before Hibernate's insert-ordering switches to a different
entity type. Each distinct statement shape drives H2's SQL parser/planner
through code paths that are largely new/cold relative to the last one, so
this workload is **method-diversity-bound**, not **iteration-count-bound**:
it is close to a worst case for a JIT-reliant VM, because JIT amortizes its
own overhead over repeated execution of the *same* compiled method, and this
test deliberately maximizes the number of distinct entity/table types seen
per transaction (that's the entire point of an insert-*ordering* test).

**Verdict: generic architectural gap, not an isolated fixable bug.** All
three profiling angles (GC, JIT-tax bisection, compile-enqueue distribution)
point away from a discrete defect and toward CratonVM's per-bytecode/
per-call execution cost across a wide, code-diverse workload -- exactly the
documented interpreter/JIT throughput gap
([`bt-throughput-levers-handoff.md`](../../internal/feature-designs/bt-throughput-levers-handoff.md)),
just manifesting worse than the 2-5x ceiling measured on tight allocation
loops because this workload's code-path diversity prevents the JIT from
amortizing compilation the way a hot loop does. Closing this gap would mean
improving CratonVM's baseline interpreter/C1 dispatch throughput broadly --
out of scope for this session and not a targeted fix. No code change made;
no regression risk.

### `LiteralRenderingTest#testIdVersionFunctions` -- effectively resolved, within generic-gap range

**Repro, current dev tip:** 12940ms vs HotSpot's 8018ms = **~1.6x**. Down
from 21.2x (169769ms, task handoff table) and 347729ms (this doc's own
2026-07-16 table). 1.6x is comfortably inside the documented ~2-5x
allocation-heavy-tight-loop range and close to parity -- this class no
longer represents a throughput problem worth further investigation. Like
`InsertOrderingRCATest`, the improvement is attributable to cumulative
fixes landed on `dev` by other sessions this same day (reflection/GC-safety
sweep, JIT threshold raise), not to anything done this session. No
profiling beyond the repro was needed given the result is already at
target; no code change made.

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

## Update 2026-07-17: `LockTest`'s "JIT compile-time tax" was actually a GC conservative-scan cost — real fix landed, worth re-checking this cluster's remaining classes against it

The `LockTest`/`CriteriaBuilderNonStandardFunctionsTest` "JIT compile-time
tax" mechanism referenced throughout this doc has been root-caused precisely
and **fixed** — see the `LockTest` entry in
[hib-misc-residuals-20260716.md](hib-misc-residuals-20260716.md) for the
full writeup. Short version: the actual cost was never compilation itself
(the background compiler thread measured ~0 CPU); it was
`vm/src/jit/conservative_roots.rs`'s `scan_active_jit_frames` conservative
native-stack scan, which — once *any* method anywhere in the process had
successfully published a JIT-compiled body — re-scanned the ENTIRE live
native call stack on every single per-native-call GC root snapshot for a
process whose interpreter recursion depth kept growing (exactly the shape
of Hibernate/JUnit5/H2's deeply nested call chains). Fixed on `dev` at
`f377eb69`: the scan's existing "verified clean" memo now also covers the
case of recursing *deeper* than the last check (previously only recursing
shallower was cheap), turning an O(current total stack depth) rescan into
an O(incremental depth since last check) one. `LockTest`: 0/5 → 5/5 clean
(dev tip `f377eb69`, ~7-9s each, was failing its 5s internal timeout by
12-19s every run). Validated against `vm/benches/vm_benchmarks.rs` with no
regression on any benchmark that exercises real interpreter/JIT/GC code.

This is directly relevant to this cluster's own working hypothesis (a
single systemic JIT/GC/native-call throughput gap): `scan_active_jit_frames`
runs on *every* object-returning native call, not just in short one-shot
test processes, so any of this cluster's still-open classes
(`InsertOrderingRCATest`, `BatchTest`, and re-checks of the "resolved"
`LiteralRenderingTest`) that publish at least one JIT compile and also have
growing/varying interpreter recursion depth could be paying the same
pre-fix cost. `InsertOrderingRCATest` was previously profiled (this doc's
2026-07-17 entry above) with `CRATONVM_DBG_TIER_ENQUEUE` showing 2260
compile enqueues at C1 — worth re-profiling with `CRATONVM_DBG_ROOTSNAP`
against the post-`f377eb69` binary to see whether any of its remaining
~9.7x-vs-HotSpot gap was this same mechanism rather than the "method-
diversity-bound, can't amortize JIT compilation" architectural-gap verdict
that session reached (that verdict was reached via a `--nojit`-is-slower
bisection, which rules out *net* JIT-tax dominance but not a smaller
`scan_active_jit_frames` contribution underneath it). Not re-checked this
session (out of scope/time for the session that landed the fix) — flagged
here for whichever session next touches this cluster.
