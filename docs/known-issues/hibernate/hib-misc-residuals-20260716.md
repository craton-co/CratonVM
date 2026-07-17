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


## Update 2026-07-17 (this session): AIOOBE/InvalidMappingException NOT independently reproduced across 60/132 real executions; NEW severe whole-class-discovery performance cliff found, blocking full confirmation

Picked up this doc's own "Next step" from the entry above (root-cause the
`ArrayIndexOutOfBoundsException`/`InvalidMappingException` 106/132 failures
now that the GC-corruption family is closed). Built a fresh binary from
`dev` (worktree `wt-hib-defaultcatalog-hbmxml-20260717`, branch
`fix/hib-defaultcatalog-hbmxml-20260717`, merged through `732241c8` — i.e.
several commits later than the `a0c60214`-based baseline that produced the
`found=132 ok=26 failed=106` result quoted above).

**HotSpot baseline (this session, same classpath/props via `common.args`):**
whole-class solo run, `132 found / 132 started / 132 ok / 0 failed`, `34.1s`
flat. Confirms real HotSpot has no legitimate skips/failures here — any
CratonVM failure is CratonVM-specific.

**Whole-class run on CratonVM: could not obtain a completed result this
session**, on an extremely contended shared host (`uptime` load average
swinging 8 → 64 over the session; `free -m` down to <500MB available at one
point; `dmesg` shows the host OOM-killing unrelated processes —
`systemd`/`(sd-pam)`/`rustc` from other concurrent sessions, and once one of
this session's own probe processes — throughout). Three separate whole-class
attempts (`DiscoverySelectors.selectClass`, exactly what `CratonRunner`/the
harness uses), JIT-on ×2 and `--nojit` ×1, **never produced a single
`@@RESULT` line** — not even for the first of 132 tests — within 6-11
minutes each (one bounded run was left to 600s and still hadn't produced a
result when this update was written). This is *slower*, not hung: live
`gdb -p <pid> -ex 'thread apply all bt'` snapshots taken several times
during these stalls each landed in a **different**, legitimate code path —
GC conservative-root scanning of JIT frames
(`gc::gen_heap::is_object_address` via `scan_active_jit_frames`),
`Throwable`-style full-stack-trace capture
(`vm::runtime::stackwalker::capture_full_trace` walking `LazyAttribute`/line
tables), and `HashMap.computeIfAbsent`-driven string hashing
(`native_map_compute_if_absent` → `map_hash_key` → `read_string`) — ruling
out a deadlock/livelock. Two of these whole-class attempts' processes were
independently confirmed still alive and CPU-bound (98-100%) via `ps`
immediately before being killed to free the host; RSS grew from ~1GB to
5-8GB over each attempt's lifetime without ever finishing test #0.

**To get a faster, more surgical signal, bypassed the whole-class
`@ParameterizedClass` discovery** with two standalone Java probes compiled
against the harness classpath and run through the real `cratonvm` binary
(source kept at
`/data/data/tmp/MiniProbe.java`/`/data/data/tmp/MethodProbeRunner.java` on
the shared host; not committed, since they're throwaway diagnostic
harnesses, not product code):

- `MiniProbe.java` — calls `MetadataSources.buildMetadata()` →
  `SessionFactoryBuilder.build()` → `SchemaExport.doExecution(CREATE, …)`
  directly (no JUnit5 launcher at all), for 2 of the fixture's `xmlMapping`
  variants (`null` and `implicit-global-catalog-and-schema.orm.xml`),
  reproducing exactly the `addInputStream`/`addAnnotatedClasses` fixture
  from `DefaultCatalogAndSchemaTest.produceModel()`. **Zero exceptions**,
  both variants: metadata build 14.7s/3.9s (CratonVM) vs 2.0s/0.13s
  (HotSpot); SessionFactory build 7.1s/1.4s vs 1.0s/0.08s; DDL-create export
  129ms/112ms vs 21ms/3ms — a genuine but unremarkable ~7-20x CratonVM/
  HotSpot ratio, consistent with this project's known general interpreter
  overhead, not a correctness gap.
- `MethodProbeRunner.java` — uses
  `DiscoverySelectors.selectMethod(className, methodName, paramTypes)`
  instead of `selectClass`, so the *real* JUnit5 launcher + Jupiter engine +
  `@ParameterizedClass`/`@MethodSource("options")` + Hibernate's
  `ServiceRegistryFunctionalTesting`/`DomainModelFunctionalTesting`/
  `SessionFactoryFunctionalTesting` extensions still run exactly as they do
  in a whole-class run — just scoped to **one** `@Test` method (still all
  12 `Options` combos for that method).

  Ran 5 of the class's 11 `@Test` methods this way — `entityPersister`
  (127.7s), `createSchema_fromSessionFactory` (117.5s),
  `updateSchema_fromSessionFactory` (140.3s), `tableGenerator` (64.7s),
  `sequenceGenerator` (60.7s) — **60 of the 132 total parameterized test
  executions, 60/60 passing** (`found=12 started=12 ok=12 failed=0` each),
  matching HotSpot's per-method 12/12 pass shape exactly. This covers DDL
  create-script generation via both the `SchemaManagementToolCoordinator`
  path and (indirectly, via `MiniProbe`) the `SchemaExport` path,
  entity-persister catalog/schema-qualifier resolution across every entity
  variant in the fixture (annotations, `orm.xml`, `hbm.xml`, joined/
  table-per-class inheritance, custom-SQL, identity/table/sequence/
  increment/enhanced-sequence generators), and DDL update-script generation
  against live `DatabaseMetaData`. **Zero
  `ArrayIndexOutOfBoundsException`/`InvalidMappingException` anywhere**
  across these 60 real, varied executions. (A 6th method,
  `createSchema_fromMetadata`, was mid-run — already past 6 minutes,
  further into its run than the other five typically needed to finish —
  when the host ran out of memory entirely; `dmesg` confirms `Out of
  memory: Killed process … (cratonvm-hbmxml)` for this probe's PID at the
  same timestamp several unrelated processes on the host were also
  OOM-killed. Inconclusive — not attributable to this class or this fix,
  just lost to host contention.)

**Assessment:** given (a) the sibling session's `found=132 ok=26 failed=106`
capture was against an earlier `dev` tip than this session's binary, (b)
several additional GC/reflection correctness fixes have landed on `dev` in
the interim (this doc's own `Update` sections above/below this one catalog
some of them), and (c) 60 of the 132 real parameterized executions —
spanning DDL generation, entity-persister reflection, and three different
ID-generator strategies — all pass cleanly on the current tip with zero
occurrences of the reported exception, **the AIOOBE/InvalidMappingException
bug most likely no longer reproduces on current `dev`**, probably fixed
incidentally by later, unrelated correctness work rather than by anything
landed this session. This is **not proven** with a full clean 132/132 run —
the whole-class run itself could not complete this session, for the
separate reason below, compounded by extreme host contention. Leaving this
sub-item **OPEN but downgraded**: a follow-up session (ideally on a quiet
host) should get one clean whole-class `found=132 … failed=0` run to close
it formally, or, if it still reproduces, get a `-Dcraton.trace=1` stack
trace for it — the `MethodProbeRunner.java` recipe above will get there
much faster than a whole-class run (a fresh `git worktree`, one
`cargo build --release -p cratonvm-cli`, then loop `MethodProbeRunner
org.hibernate.orm.test.boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest
<method> [paramType]` over the remaining 6 untested methods:
`updateSchema_fromSessionFactory` done; still untested:
`dropSchema_fromSessionFactory`, `createSchema_fromMetadata`,
`dropSchema_fromMetadata`, `incrementGenerator`, `enhancedTableGenerator`,
`enhancedSequenceGenerator`).

**NEW finding this session: a severe, reproducible performance cliff
specific to whole-class (`DiscoverySelectors.selectClass`) discovery, most
likely the real reason full harness runs of this class have historically
hung / timed out / exited silently with `rc=0` and no `@@RESULT`
(previously misread as a hang or a harness artifact).** The arithmetic does
not close: summing the five confirmed-clean per-method
`MethodProbeRunner` runs above gives ~510s of CratonVM time for 60/132
executions (~8.5s/execution average) — linear extrapolation to all 132
suggests the whole class should complete in well under 20 minutes even on
this contended host. Instead, three separate whole-class attempts never
produced even one `@@RESULT` in up to 11 minutes (one left running 600s
unresolved as of writing). The qualitative difference between "one
`Launcher.execute()` covering 12 descriptors" (`selectMethod`, fast) and
"one `Launcher.execute()` covering 132 descriptors" (`selectClass`, the
harness's actual invocation, never seen to finish even test #0) points at
JUnit5's `@ParameterizedClass` discovery/execution-tree bookkeeping scaling
far worse than linearly with total descriptor count specifically on
CratonVM. Two candidate contributing native hot paths were observed live
but **not confirmed as the dominant cost** (no profiler available on this
host this session):
- A recurring `cratonvm::gc::guard` WARN, seen in every whole-class stall
  this session: `gen_heap::get_field: out-of-bounds field read dropped
  (caller used slot index past receiver's layout — … typically a
  speculative collection-layout probe dispatched on a non-matching receiver
  type) … class_name=org/junit/jupiter/engine/execution/
  InterceptingExecutableInvoker … real_field_count=Some(0)` — some native
  fast path is speculatively probing JUnit5's reflective method-invocation
  wrapper as if it were a collection, on every reflective test/lifecycle
  invocation. The guard fails safe (no correctness impact, the read is
  dropped rather than corrupting anything) but the wasted speculative
  attempt itself is pure overhead paid on every single reflective
  invocation JUnit5 makes — and a whole-class run makes ~11x more of them
  than a single-method run.
- The already-documented, still-open, uncached
  `al_slots_for`/`is_subclass_of` per-call `FxHashSet` allocation flagged in
  this doc's `CriteriaBuilderNonStandardFunctionsTest` entry below (caught
  live via gdb mid-stall in *that* investigation) is structurally the same
  kind of "cheap-looking speculative check that isn't actually cheap"
  waste, and worth checking whether it's the same code path.

Next step for a follow-up session (ideally on a quiet host, so wall-clock
numbers are trustworthy): profile a `selectClass`-based whole-class run
directly (e.g. `perf record`/sampling, or `CRATONVM_DBG_TIER_ENQUEUE`-style
targeted counters) to find the actual dominant cost, and/or run
`MethodProbeRunner`-style probes selecting 2, 4, 6, 8, 11 methods together
in one `Launcher.execute()` call (rather than 1 method 11 times) to
empirically find the scaling exponent before attempting a fix — if it's
genuinely superlinear in descriptor count, the fix likely belongs in a
CratonVM-side cache/collection used by the JUnit5 reflection/invocation
path (candidates above), not in Hibernate or in this test.

No code change made this session — did not pin down a specific line to fix
for either the (likely-already-resolved) AIOOBE or the (newly-found, real,
but not yet root-caused to a specific line) performance cliff, and declined
to fabricate a speculative fix for either. Probe sources left at
`/data/data/tmp/MiniProbe.java` and `/data/data/tmp/MethodProbeRunner.java`
on the shared host for the next session to reuse directly.

## Update 2026-07-17 (scaling-investigation session): the "severe whole-class-discovery performance cliff" above is REFUTED — host-noise artifact, not a CratonVM defect. Growth curve is linear. But a real, previously-missed correctness bug was found and root-cause-narrowed in the process.

Picked up this doc's own "next step" from the entry immediately above: empirically
measure whether execution time scales linearly or superlinearly with the number of
`@Test` methods run together in one process, to determine whether the "whole-class
run never produces even one `@@RESULT` in 6-11 minutes" symptom is a real CratonVM
scaling defect or host noise. Reused the prior session's
`/data/data/tmp/MethodProbeRunner.java` recipe and added a new
`/data/data/tmp/MultiMethodProbeRunner.java` (JUnit5 Platform Launcher,
`DiscoverySelectors.selectMethod(...)` — one selector per requested `@Test` method,
all in a single `LauncherDiscoveryRequest`/`Launcher.execute()` call, with a
`TestExecutionListener` emitting a `@@PROGRESS n=... sinceStart_ms=... id=...` line
on every individual test start/finish) against the same
`wt-hib-defaultcatalog-hbmxml-20260717` binary (`dev@f0a74645`) used by the prior
session, on the same shared host (load average 9-36 this session — calmer than the
previous session's 8-64, but still real contention, not quiet).

**Growth curve (from a single 132-execution run, all 11 `@Test` methods requested
together — cumulative `sinceStart_ms` at each checkpoint):**

| n (test #) | cumulative ms | ms/test so far | interval ms/test (prev 10) |
|---|---|---|---|
| 10 | 107,146 | 10.7k | 10.7k |
| 20 | 175,250 | 8.8k | 6.8k |
| 30 | 216,659 | 7.2k | 4.1k |
| 40 | 250,508 | 6.3k | 3.4k |
| 50 | 280,318 | 5.6k | 3.0k |
| 60 | 329,946 | 5.5k | 5.0k |
| 70 | 363,294 | 5.2k | 3.3k |
| 80 | 413,958 | 5.2k | 5.1k |
| 90 | 457,755 | 5.1k | 4.4k |
| 100 | 513,245 | 5.1k | 5.5k |
| 110 | 547,454 | 5.0k | 3.4k |
| 120 | 611,256 | 5.1k | 6.4k |
| 130 | 656,745 | 5.1k | 4.5k |
| 132 | 668,923 (final `@@MMRESULT`) | 5.07k | — |

Per-test cost **falls** from 10.7k ms/test (n=1-10, includes one-time JVM/JUnit
warmup) to ~5k ms/test by n=40 and then stays flat (±30% noise band, consistent
with host contention) all the way to n=132 — the opposite of a cliff. There is no
inflection point, no monotonic growth, no point where forward progress stops. This
directly falsifies the "superlinear/quadratic descriptor-count scaling" hypothesis
from the prior entry.

**Decisive check: the *actual* real-harness `selectClass`-based invocation
(`CratonRunner`, exactly what the harness uses) was re-run standalone against this
same class, same binary, same host, this session** — the earlier session's own
recipe, just re-tried when host load happened to be lower (9-16 vs 8-64):
```
@@RESULT 0 ...DefaultCatalogAndSchemaTest found=132 started=132 ok=70 failed=62 aborted=0 skipped=0 ms=579598
```
**It completed in 579.6s (9.66 minutes)** — well inside the prior session's own
"~20 minutes" linear-extrapolation estimate, and *faster* than this session's
`selectMethod`-list run (668.9s) covering the identical 132 executions. `discover()`
alone (no execution) for the same `selectClass` request was also separately timed:
**696ms** — ruling out a discovery-phase bottleneck as well.

**Conclusion: the "severe whole-class-discovery performance cliff" is CLOSED —
refuted, not a CratonVM defect.** The prior session's 3 attempts that "never
produced even one `@@RESULT` in 6-11 minutes" happened on a much more extremely
contended host (load 8-64, active OOM-kills of unrelated processes, `free -m` under
500MB at one point, per that session's own notes) — this session's clean,
`selectClass`-based, real-harness-driver reproduction on a calmer host completed
the exact same class in under 10 minutes with no anomaly. No CratonVM-side
cache/collection scaling fix is needed. This resolves the prior entry's open
"Next step" (profile `selectClass` to find the scaling exponent) — there is no
scaling exponent to find; growth is linear.

**However: a real, substantial, previously-missed correctness bug was found in
the process, which is very likely the true, complete explanation for this doc's own
"AIOOBE/InvalidMappingException NOT independently reproduced across 60/132"
conclusion above being wrong.**

Both the `selectClass` harness run and the `selectMethod`-list run above show the
exact same signature, at a strikingly consistent rate (62/132 and 69/132
respectively, ~47-52%):
```
java.lang.ArrayIndexOutOfBoundsException: Index 2 out of bounds for length 2
	at java.math.BigInteger.smallToString(BigInteger.java:4170)
	at java.math.BigInteger.toString(BigInteger.java:4223)
	at java.math.BigInteger.toString(BigInteger.java:4118)
	at org.hibernate.boot.model.naming.NamingHelper.hashedName(NamingHelper.java:143)
	at org.hibernate.boot.model.naming.NamingHelper.generateHashedFkName(...)
	... (or generateHashedConstraintName)
	at org.hibernate.boot.model.naming.ImplicitNamingStrategyJpaCompliantImpl...
	at org.hibernate.boot.internal.InFlightMetadataCollectorImpl.secondPassCompileForeignKeys(...)
	at org.hibernate.boot.model.process.spi.MetadataBuildingProcess.build(...)
	at org.hibernate.orm.test.boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest.produceModel(DefaultCatalogAndSchemaTest.java:291)
```
This is `NamingHelper.hashedName`'s `new BigInteger(1, md5Digest).toString(35)` call
(base-35 encoding of a 16-byte MD5 digest, used to generate implicit FK/unique-key
constraint names) throwing inside real-JDK `BigInteger`'s own `smallToString`
digit-group loop (`digitGroups[numGroups++] = r2.longValue();`, JDK25
`BigInteger.java:4170` per `jdk25/lib/src.zip`) — i.e. a genuine CratonVM bug in
`java.math.BigInteger`/`MutableBigInteger` execution, **not** a Hibernate or
mapping bug, and unrelated to the GC-corruption family this section previously
tracked.

**Why the prior session's "0/60 failures across 5 individually-scoped methods"
finding missed this:** that session always scoped `selectMethod` to exactly *one*
`@Test` method at a time (looping the same single method through all 12 parameter
combos). This session's finding is that the bug requires **multiple different
`@Test` methods running together, interleaved, in the same process** — a minimal
2-method repro (`tableGenerator` + `sequenceGenerator`, interleaved per parameter
combo via `MultiMethodProbeRunner`, 24 total executions) reproduces it reliably
(3/24 AIOOBE, onset around the 22nd-26th execution in every attempt), while either
method alone (12 executions, previously verified in the prior session) never does.
The failure pattern across a full 132-execution run is **not** a one-time
corruption-then-stuck-broken-forever shape — it **oscillates**: a run of the same
two methods showed SUCCESS for executions 1-21, FAILED for 22-24; the full
132-execution run showed FAILED 26-36, SUCCESS 37-42, FAILED 43-54, SUCCESS 55+,
etc. The same (method, parameter-combo) pair can pass in one process and fail in
another, ruling out a purely input-dependent (deterministic on the MD5 digest
bytes) explanation.

**Root cause narrowed but not fully pinned — JIT-tiering-related, not GC-corruption,
not a simple deterministic algorithm bug:**
- **`--nojit` bisection on the minimal 2-method repro: 24/24 pass (zero failures)
  vs JIT-on 21/24 (3 AIOOBE failures)**, otherwise identical — strong evidence the
  bug requires the JIT to be involved (either a JIT-compiled miscompilation of
  `BigInteger.smallToString`/`MutableBigInteger.divide`'s bytecode, or a
  tier-up-timing-sensitive interaction).
- **However, an isolated, Hibernate-free repro does NOT reproduce it**: a
  standalone program (`/data/data/tmp/BigIntRepro.java`) looping
  `new BigInteger(1, md5(input)).toString(35)` 200,000 times over varied inputs
  (single-threaded, JIT-on, default binary) produced **zero** failures. This rules
  out "any sufficiently long-running JIT-compiled call site of this exact method
  eventually miscompiles" as the mechanism — the bug needs something about
  Hibernate's broader concurrent allocation/GC/class-loading context that a tight
  isolated loop doesn't reproduce, which combined with the `--nojit` result and the
  oscillating (not input-deterministic) failure pattern is most consistent with a
  **JIT-compilation-timing/GC-interaction bug** (a live-compiled version of
  `smallToString`/`divide`/`MutableBigInteger` internals being installed or read at
  a bad moment relative to a concurrent background-compiler or GC event) rather
  than a static miscompilation of one method in isolation.
- A live `CRATONVM_DBG_GC_STRESS=65536` / `CRATONVM_DBG_STALE_RECV=1` attempt
  (the technique that closed this exact test class's earlier GC-corruption family)
  was tried this session but did not complete within a reasonable bounded window —
  the stress level made even the first test take minutes; not pursued further given
  session time budget. A follow-up session should retry with a lower stress
  divisor (e.g. `CRATONVM_DBG_GC_STRESS=2097152` / 2MB, tried but not completed
  this session either — needs its own dedicated time budget) or a `perf`/sampling
  profile of the JIT compiler thread during the minimal 2-method repro to catch
  the actual bad compile/install event.
- Checked `native-builtins/src/biginteger_intrinsics.rs` (the `T19_H13` native
  overrides for `BigInteger`'s `@IntrinsicCandidate` methods `implSquareToLen`,
  `mulAdd`, `addOne`, `primitiveLeftShift`/`shiftLeftImplWorker`,
  `primitiveRightShift`/`shiftRightImplWorker`) as a candidate culprit, since these
  are exactly the kind of hand-written Rust replacement that has caused subtle
  bugs elsewhere in this codebase. Per JDK25's actual source
  (`jdk25/lib/src.zip:java.base/java/math/BigInteger.java`), these specific methods
  are used by `BigInteger.square()`/`shiftLeft()`/`shiftRight()`, **not** by
  `smallToString()` or `MutableBigInteger.divide()` (the actual call path in the
  crash) — so this module is very unlikely to be the direct culprit, though
  `longRadix[35]`'s lazy static initialization (used by `smallToString`) may
  itself be computed via `pow()`/`square()` and could theoretically be a shared
  suspect; not confirmed either way this session.

**Next step for a follow-up session:** get a `-Dcraton.trace=true` capture across
several failures from the `MultiMethodProbeRunner tableGenerator sequenceGenerator`
2-method repro (fast, ~2 min, reliable ~3/24 failure rate) plus a JIT compile-event
trace (whatever this codebase's equivalent of `-XX:+PrintCompilation`/
`CRATONVM_DBG_TIER_ENQUEUE` is) correlated against the exact executions that fail,
to catch which JIT tier/compile event coincides with a failure vs a
same-method-same-combo success elsewhere in the same run. Do **not** speculatively
patch `biginteger_intrinsics.rs` without first confirming it's actually on the hot
call path — it very likely is not, per the JDK source cross-check above.

No code change made this session for the BigInteger bug (root cause narrowed, not
pinned to a specific faulty line — declined to guess). Probe sources left at
`/data/data/tmp/MultiMethodProbeRunner.java`, `/data/data/tmp/DiscoveryOnlyProbe.java`,
and `/data/data/tmp/BigIntRepro.java` on the shared host for reuse.

## Update 2026-07-17 (follow-up session): `longRadix`/`digitsPerLong` clinit gap FOUND and FIXED (real, distinct bug) — but does NOT resolve this AIOOBE; JIT-tier-enqueue evidence narrows the suspect list to `MutableBigInteger.divide`/`divideKnuth`/`normalize`/`compare`, still OPEN

Picked up this doc's own "next step" (get `CRATONVM_DBG_TIER_ENQUEUE` correlated
against failures) plus independently investigated the `longRadix`/`digitsPerLong`
static-init angle the previous entry flagged but didn't confirm either way.

**A real, distinct, now-FIXED bug was found first, before the JIT angle.**
`vm/src/vm/vm_util.rs`'s `post_clinit_fixup` for `java/math/BigInteger` already
force-populates `ZERO`/`ONE`/`TWO`/`NEGATIVE_ONE`/`TEN` because real-JDK
`BigInteger.<clinit>` is documented (in that same function, comment predates this
session) to not reliably complete under CratonVM. That fixup never touched
`digitsPerLong`/`longRadix` — the 37-element radix-conversion tables
`smallToString` indexes (`java.base/java/math/BigInteger.java`, extracted from
this build's own `jdk25/lib/src.zip`: both are plain array-literal statics
populated via `valueOf(0x...)` calls inside the same `<clinit>`, **not** lazily
computed via `pow()`/`square()` as the previous entry speculated — direct source
inspection settles that open question). Extended the existing fixup to also
force-populate both tables with the real JDK's own constants, using the same
`make_or_patch_bi`/`set_static_by_name` idiom already established for the five
named constants. Landed as `f725589c` on `dev` (commit message has the full
before/after detail).

**This fix is real and necessary in general, but it is NOT what's causing
`DefaultCatalogAndSchemaTest`'s residual AIOOBE — confirmed, not assumed:**
- A reflection dump taken *immediately after* the AIOOBE fires (patched into a
  `MultiMethodRunner` harness added this session) shows `longRadix.length=37` and
  `longRadix[35]=3379220508056640625` (the correct JDK value) at the moment of
  failure, on the *same run* that just threw. The table is not corrupted, wrong,
  or truncated when the crash happens — directly answering the previous entry's
  open question ("could `longRadix`'s static-init be a shared suspect?") with a
  concrete no.
- Three separate isolated probes — a raw `BigInteger.valueOf(...).toString(35)`
  loop (up to 129-bit magnitudes), a `new BigInteger(1, digest).toString(35)` loop
  matching Hibernate's exact construction (50 random 128-bit digests, then 3000
  more in a single long-running process to force JIT tier-up), and 2000 calls
  through the *real* `org.hibernate.boot.model.naming.NamingHelper.hashedName()`
  — all pass 100% clean against the fixed binary. The fix is correct and
  sufficient for every reproduction narrower than the full multi-method Hibernate
  scenario.
- The minimal 2-method repro this doc's previous entry established
  (`entityPersister` + `createSchema_fromSessionFactory` interleaved across the
  12 `@ParameterizedClass` options, via a `MultiMethodRunner` harness added this
  session, similar in spirit to the previous entry's `MultiMethodProbeRunner`)
  **still reproduces on the fixed binary**, byte-for-byte identical symptom
  (`ArrayIndexOutOfBoundsException: Index 2 out of bounds for length 2` at
  `BigInteger.smallToString`). This is the same conclusion the previous entry
  already reached from a different angle (its 200,000-iteration standalone loop
  also found zero failures) — two independent investigations, two different
  probe styles, same result: the static radix tables are not the mechanism.

**New, more specific lead for the actual (still open) bug.** Re-ran the minimal
2-method repro under `CRATONVM_DBG_TIER_ENQUEUE=1`: `java/math/MutableBigInteger`'s
`divide(...)`/`divide(...,Z)`/`divideKnuth(...)`/`normalize()`/`compare(...)`/
`toBigInteger(I)` **all get enqueued for C1 compilation together, at the same
invocation count (1524) and same instant (~72-94s into the run)**, shortly before
the failure fires later in the run. This is the exact call chain `smallToString`'s
digit-group loop drives (`MutableBigInteger.divide` → `divideKnuth` for the
Knuth Algorithm-D long division that peels off each base-35 digit group). A
`--nojit` rerun of the identical 2-method repro (same fixed binary) passes 24/24
clean, confirming — as the previous entry's own `--nojit` bisection already
showed on the unfixed binary — that JIT involvement is necessary for the failure
to manifest, and now additionally pointing at `MutableBigInteger`'s Knuth-division
family specifically as the tier-up event that immediately precedes it, rather than
`smallToString`/`BigInteger.toString` themselves (which are comparatively trivial
wrappers around the division loop).

**Still not pinned to a specific line or confirmed as a genuine JIT codegen bug
vs. a GC/compile-timing interaction** — consistent with the previous entry's own
assessment that this needs either a JIT-compiler-thread trace during the exact
failing compile/install event, or line-level disassembly of the compiled
`divideKnuth`/`divide` to compare against the interpreter's semantics. Not
pursued further this session given the time already spent reconciling the
`longRadix` angle and this task's primary scope (BatchTest/SmokeTests
re-verification, budgeted as this session's other two items). **Next step for a
follow-up session:** disassemble the JIT-compiled `MutableBigInteger.divideKnuth`
(this codebase's JIT disassembly diagnostic, e.g. `CRATONVM_DBG_JIT_DISASM`) for
the specific method/tier combination logged above, and compare against the
interpreter's array-bounds/loop-trip-count computation for the same inputs —
`divideKnuth`'s array-index/loop-bound arithmetic (a complex multi-word Knuth
Algorithm D implementation, `MutableBigInteger.java`) is the most promising
concrete place to look for a register-allocation or loop-bound miscompilation,
per this session's tier-enqueue correlation.

**Verification of this session's own fix:** `cargo test --release -p cratonvm-vm
--lib vm_util` — 39/39 pass, no regressions. Build clean, no new warnings beyond
pre-existing ones. Reproduction harnesses (`MethodRunner.java`,
`MultiMethodRunner.java`, `ResourceLoopProbe.java`, `BigIntRadixProbe.java`,
`BigIntCtorProbe.java`/`BigIntCtorProbe2.java`, `HashedNameProbe.java`) left in
`/data/hib-baseline-runner-20260716/` on the shared host for reuse.

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

## `LockTest` — FIXED 2026-07-17 (root cause was a GC conservative-scan cost, not JIT compilation itself)

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

## Update (2026-07-17, throughput-profiling session): `LockTest` reconfirmed on a much later dev tip -- same mechanism, still OPEN, no new fix attempted

Re-ran this class's known bisection (as part of a session tasked with
`InsertOrderingRCATest`/`LiteralRenderingTest`/`LockTest` throughput
profiling -- see the other two classes' writeup in
[hib-120s-junit-timeout-cluster-20260716.md](hib-120s-junit-timeout-cluster-20260716.md))
against `dev@33df5d3c`, many commits ahead of this section's original
`dcb24161`/later-session tips. Result: **identical mechanism, still
reproduces.**

- Default (JIT on): whole-class solo run `found=23 started=15 ok=14
  failed=1 aborted=0 skipped=8 ms=36861`, the sole failure being
  `testFindWithPessimisticWriteLockTimeoutException` --
  `AssertionFailedError: execution exceeded timeout of 5000 ms by 18055 ms`
  (host load ~7-8 at the time, so this is a real, not purely
  contention-driven, overshoot -- consistent with the "JIT-on overhead, not
  host noise" conclusion already reached below).
- `--nojit`: whole-class solo run `found=23 started=15 ok=15 failed=0
  ms=9770` -- clean pass, and ~3.8x *faster* than the JIT-on run. Matches
  this section's original bisection exactly (JIT-on is both slower and
  incorrect for this short-lived process; JIT-off is both faster and
  correct).
- Confirmed the `c1_threshold=1500`/`c2_threshold=20000` mitigation from
  `fix/jit-compile-time-tax-20260716` is still the default on this tip
  (`jit/src/tiered.rs`) -- so the residual gap this section already
  documented (mitigation reduces but does not suppress compile volume for
  reflection/JDBC-heavy short processes) is confirmed still the live state,
  not something that regressed or improved incidentally since the last
  update.

**No new fix attempted this session** -- this reconfirmation used the same
`--nojit` A/B this section already ran, and the "next step" this section
already calls for (a fundamentally different tiering heuristic, or a much
more aggressive threshold validated across the broader benchmark suite) is
unchanged and still a bigger undertaking than a profiling-focused session's
time budget allows. Filed here purely as evidence that the mechanism is
stable across a large `dev` delta, so a future session picking this up can
trust the existing root-cause writeup below without re-deriving it.

## Update 2026-07-17 (session tasked with fixing the JIT compile-time-tax problem): FIXED — the real mechanism was never compilation cost, it was a GC conservative-scan cost gated on "any method has compiled"

**Landed on `dev` at `f377eb69`** (branch `fix/hib-jit-tiering-heuristic-20260717`),
built and validated from `dev@3dcf81e5` then rebased twice to keep up with a
very active `dev` (final tip includes ~30 unrelated commits from concurrent
sessions; none touched the changed file). **This entry supersedes the
"JIT compile-time tax" framing** both this section and
[hib-120s-junit-timeout-cluster-20260716.md](hib-120s-junit-timeout-cluster-20260716.md)
used for `LockTest`/`CriteriaBuilderNonStandardFunctionsTest` since
2026-07-16 — that framing was a reasonable inference from wall-clock
bisection (`--nojit` / raised-threshold both "fixed" it) but the *mechanism*
it implied (compilation itself burns CPU/wall-clock) turns out to be wrong.
The real mechanism, found this session via direct profiling instead of
inference:

**Root cause.** `jit/src/tiered.rs`'s background compiler is genuinely
async and lock-free on the mutator's hot path (confirmed: `/proc/<pid>/task/*/stat`
CPU-tick sampling during a full JIT-on `LockTest` run showed the
`cratonvm-jit-compiler` thread accumulating **~0 measured CPU ticks** despite
340 compile-task enqueues, while `main-vm` alone accounted for essentially
100% of wall-clock CPU — ruling out "compiling is slow" and "compiler thread
steals cores from the mutator" as the mechanism). A `perf record`/`perf
report` capture of the same run instead found **`cratonvm_gc::gen_heap::
GenerationalHeap::is_object_address` (30.6%) + `cratonvm_vm::runtime::
interpreter::update_root_snapshot` (26.0%) + `VmHeap::is_object_address`
(9.2%) — ~66% of all CPU** — dominating, versus a combined <10% for the same
symbols on a `--nojit` run of the identical workload.

`CRATONVM_DBG_ROOTSNAP=1` pinned this precisely: `update_root_snapshot`'s
per-call cost climbed from **~13.4us at 200k calls to ~63.8us at 400k calls**
(same call count, `avg_frames` growing 49.9→79.3 in lockstep with the
workload's naturally deepening interpreter recursion) on a default JIT-on
run, while the *identical* workload under `--nojit` — or under JIT nominally
on but with `CRATONVM_TIER_C1_THRESHOLD`/`_C2_THRESHOLD` raised so high no
compile ever completes — stayed flat at **~1.8-2.0us for the entire run**,
byte-for-byte matching each other. That last comparison is the key: it
proves the cost is gated on **at least one method having successfully
*published* a compiled body** (`cratonvm_jit::jit_code_range_count() > 0`),
not on compilation *activity* — a class whose enqueued compiles all bail
(skip-list, transient failure, etc.) never pays this cost at all no matter
how many tasks get enqueued and retried (this is exactly why
`CriteriaBuilderNonStandardFunctionsTest` no longer reproduced the failure
even before this fix — see that entry below).

Tracing into `vm/src/jit/conservative_roots.rs`'s `scan_active_jit_frames`
(called from `update_root_snapshot` on every object-returning native call —
see that function's own doc comments) found the exact mechanism: once
`jit_code_range_count() > 0`, an "A5 fix" safety-net block conservatively
scans the thread's native (Rust) call stack word-by-word for a stray return
address into JIT-compiled code that the precise `JitEntryGuard` chain might
have missed. A `UNREG_JIT_VERIFIED_LO` thread-local memoizes how much of
`[search_lo, stack_high)` was already scanned clean, but **only helped when
the current stack pointer was at or above (shallower than) the last verified
point** — for a workload whose interpreter recursion depth keeps *growing*
over the run's lifetime (deeply nested Hibernate/JUnit5/H2 call chains are
exactly this shape), the memoized boundary was invalidated on almost every
call, forcing a full linear rescan of the **entire currently-live native
stack** (up to the 8 MiB cap in `native_stack_has_jit_frame`) on nearly
every single root snapshot for the rest of the process's life, once any one
method had compiled.

**The fix** (`vm/src/jit/conservative_roots.rs`, in the `scan_active_jit_frames`
block guarded by `!moving_young_enabled() && code_ranges > 0`): when
`code_ranges` is unchanged since the last verification and the new
`search_lo` is strictly *deeper* than the memoized `verified_lo`, only the
new incremental band `[search_lo, verified_lo)` needs scanning — the
once-verified `[verified_lo, stack_high)` band is provably still clean by
the *same* invariant the existing memo already relies on ("nothing above our
current stack pointer can change while we are nested below it"), which is
symmetric with respect to which direction `search_lo` moved. On a clean
incremental scan the verified boundary extends down to the new `search_lo`,
exactly as the pre-existing shallower-or-equal case already did. Falls back
to the original full-range scan whenever this can't be proven safe (first
check, a shallower `search_lo`, or a new compile since the last check). The
"found something" branch's marking scope (`scan_one_frame(search_lo, high,
...)`) and GC-quiescence flag are byte-for-byte unchanged — only the
*detection* scan is narrowed, never what gets conservatively marked once a
frame is actually found.

**Validation.**
- `LockTest`: **5/5 clean passes** post-fix (dev tip `f377eb69`), consistently
  7.0-9.3s total (vs. 0/5 pre-fix on the same tip — 4/5 failed the internal
  5000ms timeout by 12.8-18.7s, the 5th didn't even finish inside a 90s
  wrapper). `CRATONVM_DBG_ROOTSNAP` on the fixed binary stays flat at
  ~1.06-1.94us/call for the entire run — as cheap as (or cheaper than)
  `--nojit`, not just "less bad."
- `CriteriaBuilderNonStandardFunctionsTest`: 5/5 clean both before and after
  this fix on this dev tip (see its own entry below for why) — unaffected
  either way by this specific class's workload, confirmed not regressed by
  the fix.
- `vm/src/jit/conservative_roots.rs`'s own 21 unit tests: 21/21 pass, both
  pre- and post-rebase.
- `jit::` module test sweep (125 tests): 118 passed either way; the same 7
  failures (`jit::skip_list::tests::*`) reproduce byte-for-byte identically
  on the unfixed binary too (confirmed via an explicit `git stash` A/B) —
  pre-existing, unrelated to this fix (different file, JIT-eligibility
  policy, not GC root scanning).
- `vm/benches/vm_benchmarks.rs`: no regression on any benchmark that
  actually exercises the interpreter/JIT/GC paths my fix touches
  (`jit_hot_loop_dispatch` -14.1%, `specjvm_compiler_throughput` -6.2%,
  `dacapo_avrora_100k_loop` -7.0%, `interpreter_fibonacci/{10,30,40}`
  -22.5%/-13.3%/-10.7%, `shootout_binary_trees/{8,12}` -7.7%/-20.2% — all
  "improved" per Criterion, though most of that delta is plausibly this
  heavily-shared host settling down between runs rather than a genuine
  effect of the fix on these particular short/tight-loop benchmarks, which
  mostly don't run long enough to publish a JIT body inside the timed
  window). Two benchmarks (`gc_write_barrier_lower_bound_touch_loop`,
  `monitor_enter_exit_lower_bound_touch_loop`) showed a noisy, inconsistent
  "regression" (+8-42% across two separate reruns) — traced to source and
  confirmed these are explicitly-documented **placeholder** benchmarks
  (`// Placeholder lower-bound benchmark` in `vm/benches/vm_benchmarks.rs`)
  that call `black_box` on two pointers in a bare loop and touch *no*
  interpreter, GC, or JIT code at all (verified by reading
  `bench_gc_write_barrier`/`bench_monitor_enter_exit`'s source directly) —
  structurally impossible for this fix to affect; a same-code-vs-itself
  control rerun of the identical unfixed binary against its own saved
  baseline showed comparable-magnitude noise (-2.5%/-7.4%) in the *opposite*
  direction, confirming this host's nanosecond-scale measurement noise on a
  ~250-300ns loop, not a real regression.
- The full 4548-class Hibernate suite was **not** rerun this session (out of
  time budget for a single-fix session) — a future session should fold this
  fix into the next full-suite pass this repo's other sessions periodically
  run.

**Why this was missed by the earlier "JIT compile-time tax" sessions:**
both prior sessions' bisections (`--nojit` fixes it; raising the threshold
so nothing compiles fixes it) are *consistent* with either "compilation
itself is the cost" or "the mere existence of one published compile flips on
an expensive per-call GC scan" — both hypotheses predict the exact same
bisection outcomes, since both require at least one method to actually
compile. Distinguishing them needed the direct per-thread CPU-tick sampling
and `perf record` profile this session ran, which neither prior session had
time/tooling to do. The `CRATONVM_DBG_TIER_ENQUEUE` diagnostic those
sessions added was necessary but not sufficient — it shows *enqueues*, not
*publishes*, and (as this session's `CriteriaBuilderNonStandardFunctionsTest`
finding below shows) a class can enqueue hundreds of compiles that all fail
to publish and never pay this cost at all.

## `CriteriaBuilderNonStandardFunctionsTest` — RESOLVED: original symptom stale, JIT-tax residual now FIXED too (2026-07-17)

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

**Update 2026-07-17 (session that fixed `LockTest`'s JIT-tax mechanism,
see that entry above for the full root-cause writeup): this class is now
also RESOLVED, and the "OPEN" status above should no longer be trusted.**

First, an important correction: **this class already passed reliably
(5/5 clean, ~20-26s each) on `dev@3dcf81e5` — the base this session started
from, *before* any code change.** `CRATONVM_DBG_TIER_ENQUEUE` showed 1695
compile-task enqueues in a representative run, yet `CRATONVM_DBG_ROOTSNAP`
stayed flat/cheap (~1.85-2.5us/call) for the entire run — meaning none of
those 1695 enqueued tasks ever actually *published* a compiled body for this
specific workload (all bailed via the skip-list or a transient failure), so
`jit_code_range_count()` stayed 0 the whole run and the expensive
`scan_active_jit_frames` path (see the `LockTest` entry above) never
activated at all. This is presumably an incidental improvement from the
cumulative reflection/GC-safety and JIT-policy fixes other sessions landed
on `dev` between the 2026-07-16 baseline this doc's history was written
against and `3dcf81e5` — not something traced to a single commit this
session, and not something this session's own fix should get credit for.

With this session's `scan_active_jit_frames` incremental-scan fix
(`dev@f377eb69`) also applied: still **5/5 clean**, and modestly faster
(12.4-16.9s vs. 13.0-26.0s pre-fix across the two sets of 5 reruns) —
consistent with the fix being a pure win whenever it *does* activate (a
different run of this same class, on a different day/host-load window,
could plausibly publish at least one compile and hit the pre-fix pathology;
this fix removes that risk going forward regardless of which specific
methods happen to compile).

**Reclassifying: no longer tracked as OPEN.** Both the original "real
constraint violation" hypothesis (ruled out in the 2026-07-16 entry above)
and the "JIT compile-time tax" residual (this update) are closed. If this
class regresses again in a future full-suite run, re-open referencing this
entry and the `LockTest` entry's root-cause writeup rather than re-deriving
the JIT-tax bisection from scratch.

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

## Update 2026-07-17 (follow-up session): `ZoneId.systemDefault()` host-timezone-leak FIXED — 63→20/608, new narrower residual identified

**Root cause found and fixed.** The 63 failures noted above were **not**
"specific to `UTC-8`" or a general "zone-offset handling"/H2-conversion
bug as speculated — they were a single, systemic native bug:
`java.time.ZoneId.systemDefault()`, in CratonVM's real-JDK-mode boot path,
was permanently intercepted to return a hardcoded synthetic UTC
`ZoneOffset` (`native-builtins/src/lib.rs::register_essential_natives`,
originally landed to fix an unrelated log4j boot-time NPE), **completely
ignoring every `TimeZone.setDefault(...)` call the running program made**.
`Timezones.withDefaultTimeZone()` (used by every `@Test` method in this
class and `LocalDateTimeTest`) calls `TimeZone.setDefault(...)` then the
test's own expected-value computation reads it back via
`ZoneId.systemDefault()` directly — which always resolved to UTC
regardless of what was set, producing an N-hour skew matching whichever
zone the parameterized test happened to configure (`UTC-8`, `Europe/Paris`,
`Pacific/Auckland` — not just `UTC-8`; the original doc's framing that it
was "specific to `UTC-8`" was based on an incomplete sample of the 63
failures' `@@FAIL` lines, which don't carry parameter values — a
`DisplayNameRunner` JUnit5 launcher variant, capturing the live
`TestPlan`'s `TestIdentifier` display names on `executionFinished`, was
needed to see the actual per-parameter breakdown).

Full root-cause + fix writeup:
[hib-zoneddatetime-systemdefault-host-timezone-leak-FIXED.md](../../internal/fixed-suite-bugs/hib-zoneddatetime-systemdefault-host-timezone-leak-FIXED.md).
Fix commit: `81c66806` (branch `fix/hib-zoneddatetime-offset-skew-20260716`,
merged to `dev`).

**Verification:** `ZonedDateTimeTest` `found=608 started=608 ok=384
failed=20 aborted=204 skipped=0` — stable across 4 independent solo
reruns (pre- and post-merge onto `dev`), down from `failed=63`.
`LocalDateTimeTest` (`found=162 ok=90 failed=0 aborted=72`) and
`InstantTests` (`found=204 ok=112 failed=0 aborted=92`): no regression.

**New residual — OPEN, distinct bug, not fixed by the above:** the
remaining 20/608 failures are **100% isolated** to one narrow parameter
cluster: `env=Europe/Paris` combined with dates at the 1904-12-31/1905-01-01
boundary (the test's own data comments call this out: "Also test dates
around 1905-01-01, because the code behaves differently before and after
1905"). Real pre-1911 `Europe/Paris` used Local Mean Time, UTC+00:09:21
(9 minutes 21 seconds), not a flat-hour offset — real HotSpot's tzdb
correctly applies this historical fractional offset; CratonVM instead
produces a rounded flat-hour value:

```
expected: <1905-01-01T01:09:20+00:09:21[Europe/Paris]> but was: <1905-01-01T00:18:41+00:09:21[Europe/Paris]>
expected: <1905-01-01 01:09:21.0> but was: <1905-01-01 02:00:00.0>
```

Notably the `+00:09:21` offset **is** present and correct in the
`ZonedDateTime` zone/offset portion itself (confirming `ZoneId.of("Europe/Paris")`
and the general zone-rules machinery, and this session's
`ZoneId.systemDefault()` fix, correctly resolve the *zone identity* even
for this historical case) — the bug is narrower, in the actual
instant/local-time arithmetic applied for dates before the 1911
standardization. This is the exact same underlying date range flagged as
producing an unexplained `InternalError: CloneNotSupportedException` in
this doc's original (pre-livelock-fix) investigation: with the
`ZoneId.systemDefault()` fix now letting the class run far enough to
reach these parameters reliably, that exception reproduces intermittently
(1 of 4 reruns hit it on 4 of the 20 failing methods; the other 3 reruns
produced plain `AssertionFailedError`/`AssertionError` for the exact same
parameters instead) — timing/GC-dependent, consistent with a
stale-object or clone-support gap rather than a deterministic value bug,
though the flat-hour-rounding `AssertionFailedError` shape is the
dominant/majority presentation. Not investigated further this session
(distinct subsystem — historical tzdb rule precision / possible
`Object.clone()` gap for a real JDK date class — from the
`ZoneId.systemDefault()` fix above). **Next step for a follow-up
session:** reproduce a bare (non-Hibernate) `ZonedDateTime.of(1905, 1, 1,
0, 0, 0, 0, ZoneId.of("Europe/Paris")).withZoneSameInstant(ZoneId.of("Europe/Paris"))`-style
micro-repro to confirm whether the `+00:09:21` LMT rule is applied
correctly by bare `java.time` arithmetic outside Hibernate (narrowing
core `java.time`/tzdb vs. Hibernate/JDBC layer, per this doc's original
investigation template), then chase the intermittent
`CloneNotSupportedException` with `CRATONVM_DBG_STALE_OBJREF`/a targeted
`Object.clone()` native-support audit if the micro-repro confirms a
CratonVM-side (not test-data) defect.

## Update 2026-07-17 (follow-up session): Europe/Paris pre-1911 LMT precision — FIXED, `ZonedDateTimeTest` now 608/608 matching HotSpot

**Status: CLOSED.** The 20/608 residual above is fixed. Full root-cause +
fix writeup:
[hib-paris-lmt-precision-FIXED.md](../../internal/fixed-suite-bugs/hib-paris-lmt-precision-FIXED.md).
Fix commit: `31544c27` (branch `fix/hib-paris-lmt-precision-20260717`,
merged to `dev` at `03d7e98f`).

**Root cause, in short:** not a `java.time`/tzdata bug at all — a
standalone HotSpot-comparison micro-repro proved CratonVM's
`ZonedDateTime`/`ZoneId`/`ZoneRules` arithmetic for the pre-1911 LMT case
is already byte-for-byte correct. The bug was in the **separate, legacy**
`java.util.TimeZone`/`Calendar`/`SimpleTimeZone` API family that
`java.sql.Timestamp`'s deprecated constructor and
`GregorianCalendar.computeTime()`/`.computeFields()` use for the actual
JDBC round-trip: CratonVM's real-JDK-mode `TimeZone` is synthesized
(`alloc_synth_timezone` in `native-builtins/src/lib.rs`, a workaround for
an unrelated `ZoneInfoFile`/`tzdb.dat` bootstrap gap) as a real
`java.util.SimpleTimeZone` with the zone's *modern* standard offset plus
an annual DST rule — structurally incapable of expressing a one-time
historical cutover like Paris's 1911-03-11 LMT→WET switch, since
`SimpleTimeZone.getOffsets(long, int[])` (the method
`GregorianCalendar` actually calls) only ever consults the fixed
`rawOffset` field, with no date parameter.

**Fix:** a small explicit `historical_lmt_offset(zone_id)` table plus a
native override of `SimpleTimeZone.getOffsets(long, int[])`, returning the
historical LMT offset for instants strictly before a zone's cutover and
deferring to the real bytecode (`invoke_virtual_bytecode_only`) otherwise.
Deliberately limited to `Europe/Paris` — a `GeneralityRepro.java`
HotSpot-comparison probe showed real HotSpot's own legacy Calendar path
(not just `java.time`) correctly resolves Paris's LMT, but does **not**
resolve the analogous `Europe/Amsterdam`/`Europe/Oslo` cutovers (their
compiled legacy zoneinfo data apparently omits the pre-1892/1893 LMT rule
even though `java.time`'s `ZoneRules` has it) — adding those would have
made CratonVM diverge from real HotSpot instead of matching it. See the
fix commit/writeup for why "real IANA tzdata has a cutover" doesn't imply
"HotSpot's legacy Calendar path resolves it".

**Verification:** `ZonedDateTimeTest` `found=608 started=608 ok=404
failed=0 aborted=204 skipped=0` — matches real HotSpot's exact shape
(`ok=404 failed=0 aborted=204`), stable across 5 independent solo reruns
(pre-merge and post-merge onto `dev`, `--nojit`). `LocalDateTimeTest`
(`found=162 ok=90 failed=0 aborted=72`) and `InstantTests` (`found=204
ok=112 failed=0 aborted=92`): no regression, both re-verified against the
merged-`dev` tip. The intermittent `CloneNotSupportedException` flagged in
the update above did not reproduce in any of the 5 post-fix reruns
(previously ~1-in-4) — plausibly a side effect of the same
offset-miscomputation retry path, though not separately root-caused; flag
if it resurfaces.

A separate, unrelated, out-of-scope bug was found incidentally while
probing generality: CratonVM's synthetic `SimpleTimeZone` construction
retroactively applies the *modern* EU DST rule to 19th-century dates that
predate real DST adoption in that region (e.g. `Europe/Amsterdam` at
1892-04-01 returns +2:00 on CratonVM vs. real HotSpot's +1:00 — DST wasn't
introduced there until 1916). Not investigated further (not blocking any
known test), flagged for a future session.
