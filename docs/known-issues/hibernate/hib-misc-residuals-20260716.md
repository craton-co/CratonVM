# Misc non-passed residuals — 2026-07-16 full-suite rerun

The remaining 13 non-passed classes (of 20 total) not covered by the
[120-second timeout cluster](hib-120s-junit-timeout-cluster-20260716.md).
Source: full 4548-class rerun, real-JDK, JIT-on, `dev@2f02e939d`,
`TIMEOUT=1200`, local Windows host.

## `DefaultCatalogAndSchemaTest` — CLOSED (corruption); real, distinct `ArrayIndexOutOfBoundsException`/`InvalidMappingException` bug now exposed, NEW and OPEN (2026-07-16)

`org.hibernate.orm.test.boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest`

**Status: the GC-corruption/crash family tracked in this section is CLOSED.**
Fixed jointly by two independent, concurrent 2026-07-16 sessions whose work
landed together on `dev`:

1. This session's `fix/hib-reflection-gc-sweep-20260716` (merged at
   `a0c60214`): swept `native-builtins/src/generics.rs`, which had **never**
   used the `pin_native_root`/`read_native_pin` idiom anywhere — every
   `Type`/`TypeVariable`/`WildcardType`/`ParameterizedType` builder held a
   freshly-allocated object or array in a Rust local across further
   allocating calls (`create_string`, nested `typesig_to_real_type`/
   `typearg_to_real_type` recursion, `new_type_array`) before it became
   reachable from a Java root. This is the "enum/type-var builders" residual
   gap flagged (but never swept) in
   `docs/internal/fixed-suite-bugs/jit-junit-discovery-reflection-corruption.md`
   after `db047d38` (see below) closed the sibling
   `collect_public_fields`/`methods`, `getParameterAnnotations`, and
   `create_annotation_proxy` gaps. Fixed: `type_sig_to_java`
   (ParameterizedType/TypeVariable/GenericArrayType arms), `type_arg_to_java`
   (Extends/Super/Unbounded WildcardType arms), `type_param_to_java`,
   `typesig_to_real_type` (ParameterizedTypeImpl), and
   `typearg_to_real_type`/`real_wildcard_type` (WildcardTypeImpl); plus 4
   `lang_class.rs` callers that filled a `TypeVariable[]`/`Type[]` array via
   these builders without pinning the destination array across the loop
   (`native_class_get_type_parameters`, `native_class_get_generic_interfaces`,
   `native_method_get_generic_param_types`,
   `native_method_get_type_parameters`), and the
   `AnnotationElementValue::Enum` arm of `annotation_element_to_java_typed`
   (`class_mirror` held across `create_string` + `Enum.valueOf` invoke).
2. A concurrent session's `fix/wildfly-cce0079-close-20260716` (root-cause
   writeup: `docs/internal/fixed-suite-bugs/wildfly-cce0079-young-start-set-truncation-FIXED.md`):
   fixed the actual GC-level bug this whole family's corruption cascade rode
   on — `gc/src/gen_heap.rs`'s moving-young-GC `young_object_starts`
   pre-forwarding walk assumed a contiguous bump-allocated young space and
   `break`'d out silently the first time it hit a legitimate non-object gap
   (a free-list block, reserved TLAB tail, or GAP-filler sentinel), leaving
   **every young object above that break point excluded from the
   forwardable set for that entire collection** — for every root, including
   `native_pin_roots` pins. This session's own live tracing
   (`CRATONVM_DBG_STALE_RECV=1` + `CRATONVM_DBG_GC_STRESS=65536`) caught this
   exact mechanism red-handed on `DefaultCatalogAndSchemaTest`: `GC: young
   object-start walk stopped at an implausible extent` firing at offsets as
   small as ~500 bytes–4MB into a ~585MB young generation, followed
   immediately by a broad `Stale pointer detected in invokevirtual receiver`
   cascade touching dozens of unrelated live objects in one collection
   (`org/hibernate/mapping/PersistentClass`, JUnit's `ThrowableCollector`,
   `java/util/List`/`Map`/`Optional`, `java/lang/Class`, etc.) — this is why
   the earlier "one more unrooted reflection site" hypothesis (below) kept
   finding a *different* victim each session: the reflection-heavy scan was
   just an efficient way to reach the next forced young GC, not the site of
   the actual defect.

**Verification (this session, `dev`-tip binary = `a0c60214` merged with the
`cce0079` GC fix, built and tested against `/data/hibsrc-baseline-20260716`):**
before this merge, the class deterministically failed at test-*discovery*
with `found=0`/`ClassCastException: java.lang.Object cannot be cast to
org.junit.platform.engine.TestExecutionResult$Status` (confirmed
reproducing on `a0c60214` alone, i.e. this session's `generics.rs` fix by
itself was real but insufficient — matching this doc's own "verified real
but insufficient" history below). After merging in the `cce0079` GC fix and
rebuilding: **zero** `Stale pointer detected` occurrences, and the class now
correctly discovers and runs its full `found=132` — up from `found=0`:
```
@@RESULT 0 ...DefaultCatalogAndSchemaTest found=132 started=132 ok=26 failed=106 aborted=0 skipped=0 ms=218539
```
Reproduced twice (a third run was interrupted mid-flight when the shared
Hibernate fixture's `target/` build output was wiped by unrelated host
activity — not a regression, just lost test infrastructure; see the
`NEW, OPEN` item below for what a follow-up session needs to rebuild before
continuing).

**NEW, OPEN (2026-07-16): 106/132 real `ArrayIndexOutOfBoundsException`/`InvalidMappingException` failures, unrelated to GC corruption.**
Now that the class can actually run instead of crashing at discovery, it
exposes a genuine, different correctness bug:
```
java.lang.ArrayIndexOutOfBoundsException: Index 2 out of bounds for length 2
org.hibernate.boot.InvalidMappingException: Could not parse mapping document: null (INPUT_STREAM)
```
Not investigated further this session (out of scope/time — the task this
session was scoped to was the GC-corruption family, now closed). Given this
class exercises legacy `<hibernate-mappings/>` HBM-XML in addition to
annotations (per its own `HHH90000028` deprecation warnings), the
`InvalidMappingException`/`INPUT_STREAM` shape is a plausible lead: an HBM
mapping resource failing to resolve/open via some loader path, cascading
into the array-index failures downstream. **Next step:** rebuild the
Hibernate fixture (`cd /data/hibsrc-baseline-20260716 && GRADLE_USER_HOME=/data/gradle-home-baseline-20260716
./gradlew hibernate-core:testClasses` — this session's attempt hit an
unrelated toolchain error, `Toolchain installation
'/usr/lib/jvm/java-21-openjdk-amd64' does not provide the required
capabilities: [JAVA_COMPILER]`, needs a working JDK toolchain pointed at
first), then get a `-Dcraton.trace=true` stack trace for one of the 106
failures to find the actual throw site.

<details>
<summary>Original investigation history (superseded — kept for context)</summary>

**Ruled out (original investigation):**
- The already-fixed JIT guarded-inline-getfield regression (`93b33576`) —
  reproduces identically with `--nojit`, so JIT is not involved.
- The HIB-CV-33/HIB-CV-22 non-moving-young-sweep GC corruptor — reproduces
  identically under `CRATONVM_DBG_FORCE_MOVING=1`, which forces the moving
  collector that fix relies on.
- Simple GC/heap pressure — reproduces identically with `--Xmx 2048m`.
- Not systemic to the harness or binary generally — two control classes
  (`LockTest`, `JarVisitorTest`) run against the exact same binary/load show
  **zero** occurrences of this signature.

**Root cause (partially fixed pre-session, 2026-07-16):** `CRATONVM_DBG_STALE_RECV=1`
traces every occurrence into
`org.glassfish.jaxb.runtime.v2.model.impl.ClassInfoImpl.findGetterSetterProperties`
— JAXB's reflection-heavy getter/setter/annotation scan over this test's
many HBM-XML-mapped entity classes. Found and fixed **3 more,
previously-unswept sites** (commit `db047d38`, merged to `dev` at
`6178c36f`):
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

**Verified real but insufficient (pre-session finding, since superseded):**
`CRATONVM_DBG_STALE_RECV=1` on the fixed binary showed the first corruption
in `findGetterSetterProperties` moving to a different local variable each
time a site got fixed — this doc originally concluded "at least one more
unrooted site remains" and recommended a systematic `lang_class.rs` sweep.
That sweep (this session's `generics.rs` fix) was real and necessary but,
per the verification above, **not sufficient on its own** — the GC-level
`young_object_starts` walk bug was the deeper root cause the "different
victim every time" pattern was actually pointing at.

</details>

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
Azure Linux host), then the **livelock blocking it was root-caused and
FIXED the same day** (follow-up session, same date). Both this class and
`LocalDateTimeTest` now complete; see the new residual noted below before
assuming this section's "matches HotSpot" framing still fully applies.

**Livelock: FIXED.** Full writeup:
[hib-aqs-threadpoolexecutor-relocation-livelock-FIXED.md](../../internal/fixed-suite-bugs/hib-aqs-threadpoolexecutor-relocation-livelock-FIXED.md).
Root cause was **not** a GC root-scanning gap in the usual sense: the
`Executors.newSingleThreadExecutor()`/`newFixedThreadPool`/
`newCachedThreadPool` native factory shims
(`native-builtins/src/phases_early.rs`) correctly construct the real
`ThreadPoolExecutor` via `invoke_special` and correctly pin/re-read the
object across that construction's own nested allocations — but then
discarded the (possibly GC-relocated) result on return, so the outer
factory closure kept returning its own pre-construction, now-stale Rust
`ObjectRef`. That stale address has no Java frame slot for the GC's root-
remap machinery to fix up (it exists only as a native value until the
interpreter stores the returned `Value` into a bytecode local), so once the
GC's semispace flip reused the stale from-space memory, the first
`invokevirtual` on the "constructed" executor read an all-zero header —
manifesting as the sustained `ConditionNode` stale-pointer livelock under
this pair of classes' unusually heavy per-iteration `ExecutorService`
churn (`Timezones.withDefaultTimeZone()`, called on every one of the
class's ~608/162 parameterized iterations). Fixed by having
`initialize_real_thread_pool_executor` return the current (possibly
relocated) object instead of discarding it, and updating its four factory
call sites to use that returned value.

This is a **different** bug from the GC forwarding-walk-truncation family
also fixed 2026-07-16
(`docs/internal/springboot/basiccontroller-stale-pointer-invokevirtual-aqs-conditionnode-crash-FIXED.md`) —
that family drops objects that are never forwarded at all during a moving
collection; this bug's object *was* correctly forwarded, but the native
caller never learned the new address. Both produce the identical
`Stale pointer detected ... ConditionNode` log signature, which is why they
looked like the same bug at first glance.

**Verification (fixed binary, `--nojit`, real-JDK, solo runs):**
`ZonedDateTimeTest` `found=608 started=608 ok=341 failed=63 aborted=204
skipped=0` (2/2 clean full runs, byte-identical; a 3rd run was cut short by
unrelated severe host memory exhaustion from other concurrent sessions on
this shared box, no stale-pointer signature in the partial log).
`LocalDateTimeTest` `found=162 started=162 ok=90 failed=0 aborted=72
skipped=0` (3/3 clean, byte-identical). Broader regression check
(`OptimizerConcurrencyUnitTest`, `Executors.newFixedThreadPool(10)`, real
concurrent multi-tenant ID generation) showed zero stale-pointer warnings.

**New residual surfaced by the fix — OPEN, needs its own investigation:**
now that `ZonedDateTimeTest` can complete, it shows 63 genuine
`AssertionFailedError`s HotSpot does not have for the identical classpath
(`found=608 ok=341 failed=63 aborted=204` for CratonVM vs. HotSpot's
`found=608 ok=404 failed=0 aborted=204`, confirmed via
`/home/victor/jdk25/bin/java` direct run, 29.2s, zero exceptions in the
full raw log). All 63 failures are timezone-offset value mismatches, e.g.
`Values written by Hibernate ORM should match the original value ... ==>
expected: <2017-11-06 09:19:01.0> but was: <2017-11-06 01:19:01.0>` — a
consistent 8-hour skew matching `Timezones.ZONE_UTC_MINUS_8`. This is a
previously-invisible defect (the livelock pre-empted ever seeing it), not
investigated further this session since it's unrelated to the GC/native
construction bug above. `LocalDateTimeTest`'s `failed=0` suggests the
discrepancy is specific to zone-*offset* handling (`ZoneOffset`/`UTC-8`
literal parsing or storage), not a general Hibernate/H2 timestamp bug —
worth a dedicated follow-up starting from `Timezones.toTimeZone`/
`ZONE_UTC_MINUS_8` and whichever `java.time`/H2 conversion path handles
fixed-offset (non-region) zones differently from `LocalDateTimeTest`'s
zone-less values.
