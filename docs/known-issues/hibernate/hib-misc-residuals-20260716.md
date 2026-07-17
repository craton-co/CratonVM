# Misc non-passed residuals — 2026-07-16 full-suite rerun

The remaining 13 non-passed classes (of 20 total) not covered by the
[120-second timeout cluster](hib-120s-junit-timeout-cluster-20260716.md).
Source: full 4548-class rerun, real-JDK, JIT-on, `dev@2f02e939d`,
`TIMEOUT=1200`, local Windows host.

## `DefaultCatalogAndSchemaTest` — OPEN: real, deterministic reflection/GC corruption in JAXB model-building (re-investigated 2026-07-16)

`org.hibernate.orm.test.boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest`

**Status:** OPEN — confirmed NOT a one-off flake, NOT the old getfield
regression, and NOT the HIB-CV-33 GC-sweep corruptor. Re-investigated
against the frozen `dev@dcb24161` baseline on the shared Azure Linux host
(15+ solo reruns across `--nojit`, JIT-on, and
`CRATONVM_DBG_FORCE_MOVING=1`): the class fails **100% deterministically**
(15/15, host load ranging 8–70 across runs, so not load-dependent either) —
but the failure shape is not the originally-reported `HANG (rc=124)`. It
completes quickly (40–90s) with `rc=0` and a **silent zero-test discovery
failure**:
```
@@RESULT 0 ...DefaultCatalogAndSchemaTest found=0 started=0 ok=0 failed=0 aborted=0 skipped=0 ms=... loaderror=java.lang.NullPointerException
```
The underlying exception varies by exact harness bootstrap class used
(`Cannot invoke "EngineExecutionListener.executionStarted(...)" because
"this.delegate" is null` via the `CratonRunner` harness; `AbstractMethodError:
TestEngine.getId()...has no Code attribute` via a minimal standalone
`Launcher.execute()` probe) — both are **the same underlying cause seen
through different victims**: `CRATONVM_DBG_NOCODE=1`/`CRATONVM_DBG_STALE_RECV=1`
show the AME's receiver reads back as a zeroed header
(`recv_cid=0 recv_class=java/lang/Object`), and the interpreter logs
`Stale pointer detected in invokevirtual receiver (ptr=..., all-zero
header)` immediately before it. A genuine `rc=124` timeout was also
observed once under extreme host contention (load average 60+), so the
originally-reported HANG can still occur too — it is a secondary/rarer
presentation of the same underlying corruption, not a separate bug.

**A one genuine hang WAS reproduced** during this investigation (rc=124,
60s timeout, host load 60.76 at the time) — corroborating the original
doc's `HANG (rc=124)` report as a real (if less common) presentation of
this same defect under heavy contention, not an unrelated environmental
fluke.

**Ruled out:**
- The already-fixed JIT guarded-inline-getfield regression (`93b33576`) —
  reproduces identically with `--nojit`, so JIT is not involved.
- The HIB-CV-33/HIB-CV-22 non-moving-young-sweep GC corruptor — reproduces
  identically under `CRATONVM_DBG_FORCE_MOVING=1`, which forces the moving
  collector that fix relies on.
- Simple GC/heap pressure — reproduces identically with `--Xmx 2048m`.
- Not systemic to the harness or binary generally — two control classes
  (`LockTest`, `JarVisitorTest`) run against the exact same binary/load show
  **zero** occurrences of this signature.

**Root cause (partially fixed this session, 2026-07-16):** `CRATONVM_DBG_STALE_RECV=1`
traces every occurrence into
`org.glassfish.jaxb.runtime.v2.model.impl.ClassInfoImpl.findGetterSetterProperties`
— JAXB's reflection-heavy getter/setter/annotation scan over this test's
many HBM-XML-mapped entity classes (this class alone exercises legacy
`<hibernate-mappings/>` XML mapping in addition to annotations, per the
`HHH90000028` deprecation warnings in its own log, unlike the two control
classes). This is the same "unrooted native `ObjectRef` accumulated across
an allocating call" family as several already-fixed sibling bugs in
`native-builtins/src/lang_class.rs` (see
`docs/internal/fixed-suite-bugs/jit-junit-discovery-reflection-corruption.md`'s
"residual gap" list, and
`docs/internal/fixed-suite-bugs/wildfly-standalone-managed-server-boot-fails-under-surefire-fork.md`'s
6-site long-tail). Found and fixed **3 more, previously-unswept sites**
(commit `db047d38`, merged to `dev` at `6178c36f`):
1. `collect_public_fields`/`collect_public_methods` (backing
   `Class.getFields()`/`getMethods()`) pushed freshly-created Field/Method
   mirror `ObjectRef`s into a plain, unrooted `Vec` during the
   class-hierarchy walk — a GC triggered by the Nth
   `create_field_object`/`create_method_object` call could reclaim the
   first N-1 already-created mirrors. Fixed by collecting only
   `FieldMetadata`/`MethodMetadata` during the walk and materializing the
   array in one `build_mirror_array` pass (which pins the destination
   array across every allocating call).
2. `native_method_get_parameter_annotations` left its outer
   `Annotation[][]` array unpinned across a loop whose body
   (`build_annotation_array_for`) allocates before `set_array_element` ran.
3. `create_annotation_proxy` left the freshly-allocated proxy object
   itself unpinned across many allocating calls (string/class-mirror/method
   lookups) between allocation and the point it becomes reachable from a
   Java root.

**Verified real but insufficient:** `CRATONVM_DBG_STALE_RECV=1` on the
fixed binary shows the first corruption in `findGetterSetterProperties`
moved from bytecode offset `pc=170` to `pc=64` in an earlier build then to
receiving a **different** local variable later in the same method
(`java/util/Map.keySet()` instead of `ClassInfoImpl.nav()`) — i.e. the fix
demonstrably reduced/deferred the corruption, confirming these are real
bugs, but **at least one more unrooted site remains** in the same method's
continued reflection/annotation scanning. `DefaultCatalogAndSchemaTest`
itself still reproduces the `found=0`/`loaderror=NullPointerException`
failure 100% of the time even on the merged-and-rebuilt `dev` tip.

**Next step for a follow-up session:** re-run
`CRATONVM_DBG_STALE_RECV=1` against current `dev` on this exact class,
find the (now later, still-unidentified) call site inside
`ClassInfoImpl.findGetterSetterProperties`'s continued execution after the
property-Map lookup, and audit it for the same pin/re-read pattern. Given
the "different receiver each time" progression, a systematic sweep of
remaining `lang_class.rs` natives that allocate-then-use an `ObjectRef`
without `pin_native_root` (matching the audit already done for the
sibling constructor helpers) is likely higher-leverage than continuing to
chase individual call sites one at a time.

## `JarVisitorTest` — RESOLVED: confirmed harness-artifact + underlying non-issue (2026-07-16)

`org.hibernate.orm.test.bootstrap.scanning.JarVisitorTest`

**Status:** ✅ CLOSED, not a CratonVM bug. Re-verified via 10 solo reruns
(5x `--nojit` + 5x JIT-on, `timeout 120`) against the frozen `dev@dcb24161`
baseline: **10/10 runs completed cleanly**, `rc=0`, elapsed 887ms–1913ms
(never 0ms), each producing a deterministic, well-formed
`@@FAIL ... AssertionError: Unable to setup packaging test : could not
interpret url`. This confirms the original `rc=0/ms=0` "CRASH" row was
indeed a harness/logging artifact (as suspected) — a genuinely healthy run
never produces that shape. However, the test does not "pass cleanly"
either: it fails deterministically for a reason unrelated to CratonVM.
Root-caused to `PackagingTestCase`'s static initializer, which requires its
own classloader-resource path to contain `target`/`bin`/`out/test`
(Gradle/Maven/IntelliJ build-output conventions) to locate a build dir for
ShrinkWrap fixtures; the `hib-suite-runner` harness's classpath
(`hib-libs/test-classes`) contains none of those substrings. Verified this
reproduces identically under real HotSpot JDK 25 given the same classpath
layout (a standalone probe against the real `java` binary shows
`contains target/bin/out-of-test` all false) — i.e. this is a harness
classpath-configuration limitation, not a CratonVM defect, and no CratonVM
code change applies. Also verified (running `JarVisitorTest` + `ScannerTest`
together in one process) that CratonVM correctly produces
`NoClassDefFoundError` on `ScannerTest`'s subsequent load of the
already-failed `PackagingTestCase` class (369ms, no hang) — ruling out a
class-init-failure-mishandling explanation for the separate `ScannerTest`
120s-timeout entry (tracked in the timeout cluster doc, unaffected, still
open with its own cause). Full writeup + evidence:
[hib-jarvisitortest-packagingtestcase-classpath-layout-NOT-A-BUG.md](../../internal/hib-jarvisitortest-packagingtestcase-classpath-layout-NOT-A-BUG.md).

## `LockTest` — real timing-sensitive assertion failure (root mechanism isolated 2026-07-16, still OPEN)

`org.hibernate.orm.test.jpa.lock.LockTest`

```
org.opentest4j.AssertionFailedError: execution exceeded timeout of 5000 ms by 2180 ms
```

**Status:** OPEN, but the mechanism is now well isolated (2026-07-16,
against the frozen `dev@dcb24161` baseline on the shared Azure Linux host).
Reproduced solo repeatedly: the single failing method is always
`testFindWithPessimisticWriteLockTimeoutException` (`LockTest.java:127`).
The overshoot is **not stable** — 2180ms (original, quiet local Windows
host) vs 13.5s/19.4s/22.4s/25.8s across 4 solo reruns on this heavily
shared/contended Azure host (`uptime` load average 9–15 on 16 cores from
~10 other concurrent sessions) — overshoot tracks host contention, so raw
overshoot magnitude is not a reliable metric on this host, only pass/fail.

**HotSpot comparison.** Whole-class solo run on real HotSpot
(`/home/victor/jdk25/bin/java`, same classpath/props as `common.args`):
6093ms wall, **15/15 started tests pass**, no timeout. CratonVM whole-class
solo run: 29–41s wall, 14/15 pass, this one method always fails. That's a
~5–7x class-level ratio, consistent with the 120s-cluster's suspected
systemic gap — but a targeted apples-to-apples check (a custom
`SingleMethodRunner` using `DiscoverySelectors.selectMethod`, isolating just
this one method in a cold JVM so both sides pay the same one-time JPA/EMF +
schema-bootstrap cost) tells a different story:
- HotSpot, isolated: test body ≈5.0s (itself borderline — fails by 9ms
  when cold/isolated, but comfortably passes inside the warm full-class
  run). The `assertTimeout(5s)` wrapper covers the *entire* nested
  transaction workflow including EMF bootstrap, not just the lock wait, so
  it's inherently tight even on HotSpot when cold.
- CratonVM JIT-on, isolated: test body ≈30.8s (**~6.2x** HotSpot).
- CratonVM `--nojit`, isolated: test body ≈8.2s (**~1.6x** HotSpot) —
  much closer to parity.

**Root cause narrowed: JIT compilation-time tax, not GC pauses or raw
interpreted throughput.** Two independent bisections on the *whole-class*
solo run confirm this precisely:
1. `--nojit` (interpreter only): **15/15 pass**, 12.7s total — faster
   *and* correct.
2. JIT left nominally on, but tiered-compilation thresholds raised so high
   compilation never triggers during this short run
   (`CRATONVM_TIER_C1_THRESHOLD=100000 CRATONVM_TIER_C2_THRESHOLD=1000000`):
   **15/15 pass**, 11.8s total — the fastest of all CratonVM configurations
   tried.

Both bisections converge: the failure only happens when CratonVM's JIT
actually *compiles* something mid-run. This looks like a "JIT warmup tax
exceeds payback" problem specific to short-lived, one-shot JVM processes
(one Hibernate test class per process): the default tiered thresholds
(`c1_threshold=200`, from `jit/src/tiered.rs`) are eager enough that H2/
Hibernate-internal hot methods cross the C1 threshold during this test
class's run, and CratonVM's compilation itself (not the compiled code
running) costs enough wall-clock/CPU to blow the tight 5s budget — likely
worse on this host because compiler-thread work competes with the main
thread for cores under the observed heavy contention.

This is a **distinct mechanism** from the 120s-cluster's working hypothesis
(steady-state JIT dispatch / GC pause / native-call throughput during
*already-compiled* execution) — this is compilation-*latency*, paid once,
in a short-lived process. Attempted corroboration against 2 of the 7
120s-cluster classes with `--nojit` (`ScannerTest`, `SmokeTests`) was
inconclusive: both hit unrelated harness/environment errors solo
(`ScannerTest`: `could not interpret url` packaging setup issue, same as
the now-closed `JarVisitorTest` classpath limitation; `SmokeTests`: NPE in
`EngineExecutionListener` — an unrelated harness-listener wiring problem
under `--nojit`), not a clean pass/fail signal either way, so the
same-root-cause question versus the 120s cluster remains open.

**Not fixed this session.** Raising the global tiered-compilation
thresholds (or otherwise making compilation less eager / fully
asynchronous so it never stalls the invoking thread) is a plausible fix,
but it's a cross-cutting JIT policy change with a large blast radius
(other, longer-running benchmarks in the suite may rely on the current
eagerness for their own throughput) — not something to change blind in
this session without broader regression testing across the perf/bench
suite. Leaving OPEN for a session that can run that wider validation.
Next step for that session: instrument `jit/src/tiered.rs` compile
decisions (method key + tier + wall-clock cost) during a solo `LockTest`
run to identify exactly which method(s) cross `c1_threshold` and confirm
the compile-time cost directly, then evaluate a scoped fix (e.g.
short-process detection, always-async compilation, or a higher default
`c1_threshold`) against the full benchmark suite before changing defaults.

**Update 2026-07-16 (follow-up session, branch
`fix/jit-compile-time-tax-20260716`, merged to dev): partial mitigation
landed, full resolution still OPEN.** Did the next step this doc called
for: added a `CRATONVM_DBG_TIER_ENQUEUE` diagnostic to
`jit/src/tiered.rs`'s `should_compile_inner` (prints method key + tier +
invocation count + process-elapsed-ms on every compile-task enqueue) and
used it to confirm the mechanism directly instead of by wall-clock
inference alone. Raised `CompilationPolicy`'s defaults from
`c1_threshold=200/c2_threshold=5000` to `c1_threshold=1500/c2_threshold=20000`
(`osr_threshold` untouched — a hot loop that has already run
`osr_threshold` back-edges is self-evidently still running, unlike a
one-shot bootstrap method counted by plain invocation count; both
remain `CRATONVM_TIER_C1_THRESHOLD`/`CRATONVM_TIER_C2_THRESHOLD`
env-overridable as before).

This is a real, validated, safe mitigation — but empirically **not a
full fix**:

1. **No steady-state throughput regression.** A same-binary A/B
   (`CRATONVM_TIER_C1_THRESHOLD=200 CRATONVM_TIER_C2_THRESHOLD=5000` env
   override, i.e. the old defaults, vs the new unset-env defaults) on a
   standalone `fib(32)` recursive workload measured 581ms vs 591ms —
   within noise. `vm/benches/vm_benchmarks.rs`'s `jit_hot_loop_dispatch`,
   `specjvm_compiler_throughput`, `interpreter_fibonacci`, and
   `shootout_binary_trees` all report normal, healthy numbers after the
   change. This tracks analytically too: `jit_hot_loop_dispatch`
   pre-warms (crossing whichever threshold is set) *outside* its timed
   measurement window, and genuinely hot/long-running workloads blow
   past even the raised bar quickly relative to their total lifetime.

2. **The raised threshold delays and reduces compile volume for this
   workload but does not suppress it, and does not reliably fix
   `LockTest`'s timeout.** The new diagnostic shows a solo `LockTest` run
   (post-merge binary, moderate host load ~10-30) still triggered **341
   compile-task enqueues across 128 distinct methods** —
   `java/lang/reflect/Modifier.isStatic`/`.isPrivate`/`.isFinal`/`.isPublic`,
   several `org/junit/platform/commons/util/ReflectionUtils`/`Preconditions`
   methods, and numerous `org/h2/...` internals — all legitimately called
   thousands of times by JUnit5's reflection-heavy test discovery and
   H2/Hibernate's JDBC round-trips even within one short test-class
   process. Raising the bar 7.5x (200 → 1500) delays when these cross the
   line but does not stop them from eventually doing so. The sibling
   bisection's extreme `CRATONVM_TIER_C1_THRESHOLD=100000` (500x the
   original) was what fully suppressed compilation and produced a clean
   pass — a threshold that high risks becoming indistinguishable from
   disabling the JIT outright for small/medium workloads, which is not
   something to default to without much broader validation than this
   session could perform.

   Five solo `LockTest` reruns with the new defaults (JIT nominally on,
   no env overrides): **all 5 failed** the internal 5000ms timeout, with
   overshoot varying 21232-27885ms across 4 runs taken at very heavy host
   contention (`uptime` load average 184-215, 175+ concurrent users) and
   7387ms on a 6th, later run at milder contention (load average ~10-30,
   post-merge-rebuild). A same-window real-HotSpot solo run passed
   cleanly both times it was tried (5706ms flat, 15/15, zero timeout),
   confirming the residual gap is CratonVM-specific and not purely host
   noise. A `--nojit` run on the *same* fix binary, same host, passed
   cleanly (9631ms, 15/15) — proving interpreter-only execution
   comfortably fits the budget and confirming JIT-related overhead
   (compilation and/or the ongoing per-invocation tiered-profiling
   bookkeeping that runs even for methods that never end up compiling)
   is still the dominant remaining cost, just not fully eliminated by
   this threshold raise alone.

**Conclusion: landed the threshold raise as a genuine, low-risk,
validated partial mitigation** (real CPU waste eliminated for every
method that no longer crosses the lower 200-invocation bar at all; zero
observed regression to steady-state/long-running throughput), **but
`LockTest` and `CriteriaBuilderNonStandardFunctionsTest` remain OPEN** —
neither passes reliably under default settings after this change. A
full fix needs either a much more aggressive threshold (with the
throughput risk above validated away across a broader benchmark set) or
a fundamentally different heuristic — e.g. weighing a method's
*remaining* expected call volume rather than just its cumulative count,
or making the per-invocation tiered-bookkeeping itself cheaper/lock-free
— both bigger undertakings than this follow-up session's time budget
allowed. Left OPEN for a future session with more time and, ideally, a
quieter host: this session's host load swung from 6.9 to 215 over the
course of testing, which by itself makes wall-clock pass/fail signal for
a 5-second-budget test hard to trust in isolation — though the
`--nojit`-vs-JIT-on same-binary, same-host comparison above is the most
trustworthy signal gathered this round, and it points at JIT-on overhead
(compile activity and/or bookkeeping), not host noise, as the residual
cause.

## `CriteriaBuilderNonStandardFunctionsTest` — RESOLVED: original symptom stale, residual is JIT compile-time tax (2026-07-16)

`org.hibernate.orm.test.query.criteria.CriteriaBuilderNonStandardFunctionsTest`

```
org.hibernate.exception.ConstraintViolationException: could not execute batch
[Unique index or primary key violation: "PUBLIC.CONSTRAINT_35E3F7 PRIMARY KEY ON ...
```

**Status:** investigated 2026-07-16 against the frozen `dev@dcb24161` baseline
(shared Azure Linux host). **Not test-order dependent** — reproduces solo, in
complete isolation, on the very first attempt and every attempt thereafter
(13+ solo reruns). The class's `@BeforeEach` persists 5 `EntityOfBasics` rows
with **explicit, manually-assigned ids (1-5)** — there is no `@GeneratedValue`
id generator anywhere in this test, so the doc's original "real
id-generation double-issue bug" hypothesis is ruled out categorically
regardless of any other finding below; a collision could only ever come from
a duplicate/leftover row at those exact fixed ids.

**The originally-captured `ConstraintViolationException`/PRIMARY KEY symptom
did not reproduce even once** across 13 solo reruns on this baseline
(default heap, `--Xmx 96m`, JIT-on, `--nojit`, high-JIT-threshold — see
below). `dev@dcb24161` already includes the same-day
[`1c4aaa06` "close stream ArrayList GC pressure corruption"](../../internal/fixed-suite-bugs/stream-arraylist-gc-pressure-heap-corruption-FIXED.md)
fix, merged just before this investigation. That fix closed a family of bugs
where GC-pressure-triggered corruption of `ArrayList`-backed collections
(stale/duplicated conservative roots, missed old-to-young remembered-set
entries) produced spurious duplicate elements — exactly the shape that would
turn one `persist()` into two INSERTs of the same row inside one JDBC batch,
i.e. a duplicate-PK batch failure. This is circumstantial (no before/after
A-B on the exact pre-fix binary was possible this session — no such binary
was available), but is the most plausible explanation for why the original
symptom is now unreproducible: it was very likely the same bug family,
already fixed.

**What reproduces instead, consistently:** `TimeoutException:
prepareData(org.hibernate.testing.orm.junit.SessionFactoryScope) timed out
after 120 seconds`, with the *identical* found/ok/failed/skipped shape as the
original entry (20 found / 18 started / **17 ok / 1 failed** / 2 skipped) —
i.e. this looks like the same underlying event the original run captured,
just manifesting as a timeout instead of an exception because it took even
longer on this host. Full stack trace (`-Dcraton.trace=1`) shows this is
JUnit5's `SameThreadTimeoutInvocation` — **not a preemptive/async timeout**;
it measures wall-clock and only reports `TimeoutException` after the
underlying call actually returns/throws, discarding whatever the real
underlying outcome was if it also exceeded 120s. So a run that would have
reported `ConstraintViolationException` at, say, 140s instead reports
`TimeoutException` and hides the real exception — one plausible unification
of both symptoms under a single "prepareData is occasionally very slow"
root mechanism.

**HotSpot comparison** (`/home/victor/jdk25/bin/java`, identical classpath/
props via `common.args`): **5/5 clean runs**, 6.8-8.1s each, run back-to-back
under the *exact same* crushing host contention as the CratonVM runs below
(`uptime` load average 28-53 on 16 cores throughout this investigation, from
~50+ other concurrent sessions on this shared box). CratonVM JIT-on: 100% of
default-config solo reruns either barely passed (~123-127s total) or hit the
120s `TimeoutException` (~150-165s total) — i.e. CratonVM is *at minimum*
~15x slower than HotSpot for this class even on a "passing" run, before any
timeout is even considered, on this host.

**Live gdb capture during an actual stall** (poll-and-pounce technique per
[wildfly-gc-barrier-boot-hang-and-harness-fixes.md](../../internal/fixed-suite-bugs/wildfly-gc-barrier-boot-hang-and-harness-fixes.md):
background the run, poll the log for a >12s output-idle gap, `sudo gdb -p
<pid> -ex 'thread apply all bt'` the instant it's detected). Result: **no
deadlock** — only one thread (`main-vm`) was doing anything; the other three
(`Hibernate Conne`, `junit-jupiter-t`, and the joining `main` thread) were
parked/idle as expected. `main-vm` was genuinely CPU-bound, live inside
`native_al_itr_next -> al_state -> al_slots_for -> is_subclass_of` (an
ArrayList iterator's native `next()`, resolving whether the receiver is a
`java.util.Vector` for field-slot purposes), which allocates and grows a
fresh, uncached `FxHashSet` on **every single call** (`native-collections/src/lib.rs`
`al_slots_for`, `classloading/src/class.rs`'s `is_subclass_of`/
`is_subclass_of_inner`). This is a real, narrow inefficiency worth a look —
every ArrayList/Vector-layout native access pays a full class-hierarchy walk
with a fresh hashmap allocation instead of a per-`ClassId` cached answer —
but it was not proven to be *the* dominant cost below, only *a* genuine
CPU-bound hot path caught live during a stall.

**Root cause, confirmed via bisection (same methodology as this file's
`LockTest` entry, found earlier the same day): JIT compilation-time tax, not
a data-corruption bug, not GC pauses, not raw interpreted throughput.**
- `--nojit` (interpreter only): **3/3 clean runs**, 18/18 ok, 29.5-30.7s
  each — no timeout, ever, despite host load climbing to 42-48 during these
  runs.
- JIT nominally on, but tiered-compilation thresholds raised so compilation
  never triggers during this short run
  (`CRATONVM_TIER_C1_THRESHOLD=100000 CRATONVM_TIER_C2_THRESHOLD=1000000`):
  **2/2 clean runs**, 18/18 ok, 31.6-32.9s each — load average 48-53 during
  these runs (the heaviest contention seen all session), still clean.
- Default JIT-on config: 0/6 clean in the runs immediately preceding this
  bisection (barely-passing-slow or `TimeoutException`), at *lower*
  observed load averages (22-42) than the bisection runs that passed
  cleanly.

Both bisections converge on the same conclusion the `LockTest` entry reached
independently: CratonVM's compilation itself (not the JIT-compiled code
running afterward) costs enough wall-clock/CPU under this host's contention
to blow a short-lived process's time budget, and disabling or deferring
compilation removes the failure entirely. This is the **same mechanism**,
not a separate bug — see that entry above for the shared root-cause status
(OPEN at the JIT-policy level: raising `c1_threshold`/making compilation
async is a plausible fix but a cross-cutting change needing broader
benchmark-suite validation, deliberately not changed blind this session).

**Reclassifying:** this is not a distinct wrong-behavior/id-generation bug.
Moving out of "genuine wrong-behavior symptom" — it belongs with
[the 120s-timeout cluster](hib-120s-junit-timeout-cluster-20260716.md) (same
`TimeoutException(...)` shape, same "one test absorbs a one-time
SessionFactory-bootstrap cost that occasionally exceeds 120s" shape) and
with this file's own `LockTest` entry (same JIT-compile-tax root mechanism,
confirmed via the identical bisection). No code change made this session —
the underlying JIT-policy fix is intentionally left to the session handling
that broader, already-tracked investigation. The one concrete, narrow lead
worth a follow-up look: `al_slots_for`'s per-call, uncached
`is_subclass(cid, vector_id)` check in `native-collections/src/lib.rs`
(caught live via gdb mid-stall) — a small per-`ClassId` cache there is a
plausible, low-risk contribution to closing part of the general throughput
gap, independent of the JIT-tax question.

**Update 2026-07-16 (follow-up session): same partial-mitigation-not-full-fix
outcome as the `LockTest` entry above.** The `fix/jit-compile-time-tax-20260716`
threshold raise (`c1_threshold` 200 -> 1500, `c2_threshold` 5000 -> 20000,
see that entry for the full validation writeup) was tested against this
class too: a solo run on the pre-merge fix binary still hit the 120s
`TimeoutException` (`ms=148340`, 17/18 ok, 1 failed — same shape as
before), with the new `CRATONVM_DBG_TIER_ENQUEUE` diagnostic showing zero
compile-task enqueues *in that specific run* — but the `LockTest` entry's
later post-merge run showed the "zero enqueues" reading is not a reliable
sign of true suppression by itself (it can also mean the process was so
starved of CPU under extreme host contention that no method reached even
the raised 1500-invocation bar; a calmer run on the same binary reached
1500+ for 128 distinct methods). Given that ambiguity, this class's result
should be read the same way as `LockTest`'s: the threshold raise is a real,
validated, safe mitigation with no steady-state throughput regression, but
it does **not** reliably fix this class's timeout either. Remains OPEN,
tracked jointly with `LockTest` at the JIT-policy level.

## Already-expected ABORTED entries (matches HotSpot, not a defect)

Per [hib-bytecode-enhancement-loader-faithful-linking.md](../../internal/fixed-suite-bugs/hib-bytecode-enhancement-loader-faithful-linking-FIXED.md)
and the 2026-07-11 audit, these are expected `@CustomEnhancementContext`/
dialect-gated partial skips:

- `org.hibernate.orm.test.bytecode.enhancement.basic.InheritedTest`
- `org.hibernate.orm.test.bytecode.enhancement.basic.MappedSuperclassTest`
- `org.hibernate.orm.test.type.temporal.InstantTests`
- `org.hibernate.orm.test.type.temporal.LocalDateTimeTest`
- `org.hibernate.orm.test.type.temporal.OffsetDateTimeTest`
- `org.hibernate.orm.test.type.temporal.OffsetTimeTest`
- `org.hibernate.orm.test.manytomanyassociationclass.surrogateid.generated.ManyToManyAssociationClassGeneratedIdTest`
  (2026-07-16, confirmed) — 6 found / 3 ok / 3 aborted / 0 skipped. Root
  cause found via a custom `TestExecutionListener` (`AbortTraceRunner`,
  mirrors `CratonRunner`'s launcher setup but also captures
  `TestExecutionResult.getThrowable()` for ABORTED results, since
  `SummaryGeneratingListener.getFailures()` only covers FAILED). The 3
  aborted methods (`testRemoveAndAddEqualElement`,
  `testRemoveAndAddEqualCollection`, `testRemoveAndAddEqualElementNonKeyModified`
  — the three overridden in this subclass) all call
  `skipForGraphQueue(scope)`, which does
  `assumeFalse(getConfiguredQueueType() == QueueType.GRAPH, ...)`. Hibernate
  ORM 8's `QueueType.fromSetting(null)` defaults to `GRAPH`
  (`hibernate.flush.queue.type` is unset by the harness), so this assumption
  is expected to fail and skip these 3 legacy-ordering-specific methods by
  design, independent of the JVM. Confirmed by running the identical class
  through a standalone JUnit5 launcher directly on
  `/home/victor/jdk25/bin/java` (real HotSpot, same classpath as
  `common.args`): also 6 found / 3 ok / 3 aborted, same 3 methods, byte-for-byte
  identical abort message on both VMs:
  `org.opentest4j.TestAbortedException: Assumption failed: Legacy
  insert-before-delete ordering is not expected with the graph action queue`.
  Not a CratonVM defect — same shape as the other entries in this list, just
  gated by a Hibernate-internal default rather than `@CustomEnhancementContext`
  or a dialect check. No fix needed.

One exception: `org.hibernate.orm.test.type.temporal.ZonedDateTimeTest` —
investigated 2026-07-16 against the frozen `dev@dcb24161` baseline (shared
Azure Linux host). **Not yet confirmed same-mechanism; keep OPEN, and a
separate, more severe defect surfaced during the attempt.**

**HotSpot comparison** (`/home/victor/jdk25/bin/java`, identical classpath/
props via `common.args`): solo run completes cleanly in 29.2s — 608 found /
404 ok / 204 aborted / 0 failed, the identical shape reported for CratonVM.
A full-text scan of the entire raw output (all WARN/INFO Hibernate logging
included, via `-Dcraton.trace=1`) contains **zero** occurrences of
"exception" (case-insensitive) anywhere. HotSpot's 204 aborted tests here
are 100% silent assumption-based skips, exactly like the other
`@CustomEnhancementContext`/dialect-gated entries in this list — there is no
HotSpot-side message of any kind to compare against.

**CratonVM comparison: could not obtain a completed run on this host.** 4
independent solo attempts — JIT-on, JIT-on with `RUST_LOG=error`, `--nojit`,
and `nice -n 19` — all deterministically hit a severe livelock instead of
completing: tens of millions of repeated `Stale pointer detected in
invokevirtual receiver (ptr=..., all-zero header) — falling back to CP class
java/util/concurrent/locks/AbstractQueuedSynchronizer$ConditionNode`
warnings (`vm/src/runtime/interpreter.rs`), against **the same object
address sustained across checks taken minutes apart** — ruling out ordinary
slow-but-progressing execution across the class's 608 parameterized
iterations, which would churn through many different addresses. None of the
4 attempts reached a single `@@RESULT` within bounded timeouts up to 300s
(30-40M+ log lines emitted, no forward progress). A 5th attempt pinned to a
single core (`taskset -c 0`) avoided the livelock but hit a different,
unrelated harness bug instead (NPE: "Cannot invoke
`EngineExecutionListener.getClass()` because `listener` is null" during
JUnit class discovery/loading, found=0).

This reproduces identically on the sibling `LocalDateTimeTest` (also in this
same "already-expected" list, sharing the same `AbstractJavaTimeTypeTests`
base): same livelock signature, same non-terminating spam, no `@@RESULT`.
Both classes' shared `Timezones.withDefaultTimeZone()` helper
(`hibernate-core/src/test/java/.../type/temporal/Timezones.java`) creates a
brand-new `Executors.newSingleThreadExecutor()` + submits a `Future` on
**every one of the class's ~608 iterations**, which is far heavier
AQS/`ConditionNode`/thread-pool churn per run than any other class
currently tracked in this doc — the likely reason this specific livelock
only manifests for this pair of classes.

**Conclusion so far:** since HotSpot's abort mechanism for this class is
provably silent, the original hedge ("same expected-skip mechanism
surfacing a different message") cannot be literally correct — there is no
HotSpot message to be "the same" as. Whatever produced the original
`InternalError: CloneNotSupportedException` capture in CratonVM must be a
CratonVM-only artifact, not shared HotSpot behavior; it is **not** confirmed
to belong in this "matches HotSpot, not a defect" section, and
`ZonedDateTimeTest` should be treated as OPEN, not closed, until it can
actually be re-verified. Root-causing the original signature itself was not
possible this session because CratonVM never got far enough to reproduce it
(the livelock above pre-empts it entirely on this host).

**Separately flagged:** the livelock itself (AQS `ConditionNode`
stale-pointer detection never resolving under heavy `ExecutorService`/
`Future` churn) is a distinct, newly-discovered, clearly-reproducible defect
in its own right — 4/4 reproduction rate, unrelated to JIT-vs-interpreter
choice — that blocks any solo verification of this whole temporal-test pair
on a contended host, and needs its own dedicated investigation (in the
spirit of this project's existing "stale-objref"/root-coverage-gap bug
family) with the `CRATONVM_DBG_SWEEP_ZERO`/`CRATONVM_DBG_STALE_RECV` probes
already built into `interpreter.rs` for this exact scenario, ideally on a
quiet host to get an uncontaminated signal.
