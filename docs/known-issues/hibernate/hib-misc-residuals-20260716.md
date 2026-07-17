# Misc non-passed residuals — 2026-07-16 full-suite rerun

The remaining 13 non-passed classes (of 20 total) not covered by the
[120-second timeout cluster](../../internal/hib-120s-junit-timeout-cluster-20260716.md).
Source: full 4548-class rerun, real-JDK, JIT-on, `dev@2f02e939d`,
`TIMEOUT=1200`, local Windows host.

## `DefaultCatalogAndSchemaTest` — GC-corruption family CLOSED; BigInteger AIOOBE/InvalidMappingException residual CONFIRMED real, reproducibility wildly environment-sensitive (near-unreproducible in some sessions, a reliable ~50% full-harness hit rate in others, same day), root cause STILL OPEN

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

## Update 2026-07-17 (follow-up session): fast, minimal, Hibernate-free repro found (85-90% failure rate); JIT-only, GC-amplified, precisely localized to `divideMagnitude`'s `shift > 0` normalization path — root mechanism narrowed further but the exact faulty instruction NOT pinned; no fix landed this session

Picked up this doc's own "next step" (disassemble the JIT-compiled `divideKnuth`/
`divide`/`divideMagnitude` and compare against interpreter semantics). Built from
`dev@3e74dd5a`, worktree `wt-hib-bigint-divideknuth-20260717`, branch
`fix/hib-bigint-divideknuth-jit-20260717`; re-verified at the end of the session
against `dev@8ef4d59b` (merged in mid-session, includes an unrelated
`conservative_roots.rs` scan-cost fix, `f377eb69` — confirmed **not** related to
this bug, repro behavior identical before/after that merge).

**The isolated-loop repro from the previous entries (a raw
`new BigInteger(1, digest).toString(35)` loop, 200,000 iterations, zero failures)
was misleading — it never actually needed Hibernate, just the right operand
shape.** A tighter repro,
`/data/hib-baseline-runner-20260716/bigint-divideknuth-repros-20260717/SmallDividendRepro.java`,
loops `BigInteger.divideAndRemainder(35^12)` (a fixed ~62-bit, 2-int-word
divisor — exactly `NamingHelper.hashedName`'s `longRadix[35]`) against **small**
random dividends (67-70 bits, only slightly bigger than the divisor, so
`MutableBigInteger.divideMagnitude`'s main Knuth D2-D7 loop runs **zero**
iterations and execution goes straight to the post-loop special-cased
final-digit block), checking `q*b+r == a && 0<=r<b` directly instead of relying
on the `smallToString` array-overflow side effect. This reproduces **85-90% of
the time**, in well under a second, no Hibernate/JUnit/classloading involved:
```
@@RESULT bad=17686 of 20000
```
The real production call (`NamingHelper.hashedName` via
`bigint-divideknuth-repros-20260717/HashedNameProbe.java`, unchanged from the
prior session) was re-run against this same finding and throws the **identical,
exact** original stack trace deterministically at the same trial number across 3
repeat runs:
```
trial=771 ... FAILED: java.lang.ArrayIndexOutOfBoundsException: Index 2 out of bounds for length 2
	at java.math.BigInteger.smallToString(BigInteger.java:4170)
	at java.math.BigInteger.toString(BigInteger.java:4223)
	at java.math.BigInteger.toString(BigInteger.java:4118)
	at org.hibernate.boot.model.naming.NamingHelper.hashedName(NamingHelper.java:143)
```
confirming the minimal repro and the production bug are the same defect.

**Bisection chain (all against the same binary, `--nojit` vs default JIT-on
unless noted):**
1. **JIT-only, not a race, not compile-timing-dependent:** `--nojit` is 0/20000
   clean every time. Lowering `CRATONVM_TIER_C1_THRESHOLD` makes the failure
   onset track the threshold precisely (e.g. threshold=5 → onset around
   invocation ~240 instead of ~1500) — this is a **deterministic compile-time
   codegen defect**, not a background-compiler-thread race: a
   `FixedValRepro.java` variant that calls `divideAndRemainder` on the exact
   same fixed operands thousands of times in a row shows a **hard state
   transition** — correct on every call before the relevant methods finish
   compiling (`normalize`/`toBigInteger`/`divideKnuth`/`compare`/`divide`, all
   enqueued together at the same invocation count, matching the previous
   entry's tier-enqueue finding), then **consistently, identically wrong on
   every call after**, not intermittent/flickering.
2. **Localized to the `shift > 0` normalization path specifically.**
   `ShiftZeroRepro.java` uses a divisor with its top bit forced set (so
   `Integer.numberOfLeadingZeros(divisor.value[0]) == 0`, i.e. `shift == 0`),
   which skips `divideMagnitude`'s entire D1 divisor/dividend
   normalize-by-`shift` step and the D8 `rem.rightShift(shift)`
   "unnormalize" step (both gated `if (shift > 0)`): **0/20000 failures.**
   Reverting to the `shift == 2` divisor (`35^12`, i.e. the real
   `NamingHelper` divisor) reintroduces the bug at the same ~88% rate. This
   is the single most useful isolating fact found this session.
3. **Not the main Knuth D2-D7 loop, not `mulsub`, not
   `unsignedLongCompare`.** `SmallDividendRepro`'s small-dividend shape
   proves the main loop doesn't even need to run (`limit-1 == 0`) for the bug
   to fire at full rate. `mulsub([I[IIII)I` (1026 bytes compiled) and
   `unsignedLongCompare(JJ)Z` (214 bytes) were each fully manually traced
   instruction-by-instruction against their real-JDK source
   (`jdk25/lib/src.zip`) and are byte-for-byte semantically correct, including
   the unusual "three-way-compare-then-threshold" codegen idiom this JIT
   uses for every `>`/`<` comparison (`setg`/`setl`/`sub`/`test` instead of a
   direct `setg`) — correct here because both compared operands are
   pre-masked to `[0, 0xFFFFFFFF]`, so signed vs. unsigned compare coincide.
4. **Not which D1 sub-branch fires.** `BranchIsolateRepro.java` forces
   either the `Integer.numberOfLeadingZeros(dividend.value[0]) >= shift`
   branch (`this.primitiveLeftShift(shift, remarr, 1)`, a shared compiled
   method) or the `else` branch (a hand-inlined shift-with-carry loop
   directly in `divideMagnitude`'s own bytecode) — **both fail at ~85-90%**,
   meaning the defect is in something common to both paths: most likely the
   *divisor's* `div.primitiveLeftShift(shift, divisor, 0)` call (unconditional
   whenever `shift > 0`, identical in both branches) or the D8
   `rem.rightShift(shift)`/`rem.normalize()` step (also unconditional in both
   branches). The full compiled body of the (non-unrolled) shift-with-carry
   loop was manually traced instruction-by-instruction against source and is
   also correct.
5. **Not loop unrolling.** `CRATONVM_DISABLE_UNROLL=1` removes a confirmed,
   real extra unrolled copy of the shift-loop body (verified via disassembly:
   3 `shl`+2 `shr` instructions in the loop region drop to the source-correct
   2 `shl`+1 `shr` once disabled) but the failure rate is **unchanged**
   (17709/20000) — ruling out unroll-boundary/trip-count handling as the
   cause.
6. **GC-amplified, and this looks like the real mechanism, but is NOT fully
   confirmed.** `CRATONVM_DBG_GC_STRESS=65536` turns the ~88%
   "wrong-silent-result" rate into an outright, immediate, deterministic
   `ArrayIndexOutOfBoundsException` crash (the exact production exception) on
   the very first stressed run. `--nojit` + the same GC stress stays 0/3000
   clean — ruling out a pure GC bug independent of JIT. `CRATONVM_DBG_VERIFY_OOP_MAPS=1`
   under GC stress fires repeated `[VERIFY-OOP-MAPS] unmapped in-band oop`
   warnings for JIT-compiled code at the frame addresses matching
   `divideMagnitude` (and one caller), at very deep `[rbp-N]` offsets (up to
   `-0x3ce0`) consistent with the single-pass backend's own internal
   scratch/spill slots (used to evaluate nested sub-expressions like
   `(b<<shift)|(c>>>n2)`) rather than the method's 28 named bytecode locals
   (`javap` confirms `divideMagnitude` has `locals=28`, well under the 64-bit
   `local_oop_masks` tracking width, so it is *not* a mask-width overflow).
   **However**, this does not fully explain the mechanism: `moving_young`
   (the moving Cheney young-gen collector) is **off by default**
   (`CRATONVM_MOVING_YOUNG` unset), so plain "object relocated, stale pointer"
   can't be the whole story under default settings, and — more importantly —
   `CRATONVM_NO_PRECISE_JIT_MAPS=1` (forcing pure conservative frame
   scanning, no precise maps at all) does **not** change the failure rate
   either (17704/20000, same as with precise maps on). So either (a) the
   conservative fallback scan also fails to cover these deep scratch-slot
   addresses for a method this large/complex (a genuinely deeper root-tracking
   gap than "just add missing precise-map entries" would fix), or (b) the
   `VERIFY-OOP-MAPS` warnings are the diagnostic's own documented false-positive
   case ("NB band may include nested-JIT-callee slots") and the GC-stress
   sensitivity is actually a heap-layout/adjacency effect amplifying a
   genuine **out-of-bounds write** bug (e.g. a one-element overflow past
   `remarr`'s `intLen+2`-sized allocation, or past the 2-element local
   `divisor` array) rather than a stale-read bug — under GC stress, allocation
   adjacency changes what ends up next to the overflowing array, changing
   whether the overflow corrupts something visible. **This distinction was
   not resolved this session.**

**Not fixed this session.** The investigation is narrowed further than any
prior entry (a Hibernate-free, sub-second, 85-90%-reliable repro; JIT-only;
localized to the D1/D8 `shift > 0` normalization code in `divideMagnitude`;
`mulsub`/`unsignedLongCompare`/loop-unrolling/branch-choice all individually
ruled out) but the exact faulty instruction or the precise
stale-read-vs-out-of-bounds-write mechanism was not pinned down, despite an
extensive full manual disassembly trace (`CRATONVM_DBG_JIT_DISASM`) of every
individually-compiled method on the call path. Given this touches shared JIT
codegen/GC-root-tracking infrastructure used far beyond `BigInteger`, landing a
speculative fix without pinning the exact defect was judged too risky —
per this doc's own standing guidance, a well-documented non-fix beats a risky
guess here.

**Next step for a follow-up session**, roughly in order of expected
cost/payoff:
1. Use `SmallDividendRepro`/`ShiftZeroRepro`/`BranchIsolateRepro` (all left at
   `/data/hib-baseline-runner-20260716/bigint-divideknuth-repros-20260717/`) —
   they reproduce in under a second, no Hibernate needed, and don't need
   re-deriving.
2. Resolve the "which mechanism" question directly: instrument
   `divideMagnitude`'s local `divisor`/`rem`/`remarr` arrays with a
   `System.identityHashCode`-based before/after check bracketing the
   `div.primitiveLeftShift(shift, divisor, 0)` call and the final
   `rem.rightShift(shift)` call (a small reflection-based probe in package
   `java.math` can call these package-private methods directly without
   `setAccessible`) to see whether the array's **address changes** (moving
   GC relocated it — but `moving_young` is off by default, so this would
   itself be a new finding) or the array's **contents become wrong without
   changing identity** (favors an out-of-bounds write from a neighboring
   compiled expression, or a genuine arithmetic bug this session's manual
   trace missed).
3. If (2) points at a stale/relocated reference: extend
   `emit_oop_map_for_safepoint`'s Stage 1/2 tracking
   (`jit/src/x64.rs`, ~line 9981) to also cover the single-pass backend's own
   scratch/spill slots (the `[rbp-0x88]`/`[rbp-0x118]`-style temps used to
   evaluate nested sub-expressions) when they hold live reference values
   across a GC-capable call — not just operand-stack slots and the 28 named
   bytecode locals.
4. If (2) points at an out-of-bounds write: audit the exact bounds-check
   emitted for the `remarr[intLen+1] = c << shift;` tail write and the
   `divisor[dlen]`-sized array's fill in `primitiveLeftShift`'s inlined tail
   store, for a one-element-too-generous bound.

Probe sources (all Hibernate-free except `HashedNameProbe.java`) left at
`/data/hib-baseline-runner-20260716/bigint-divideknuth-repros-20260717/`:
`MTBigIntDivRepro.java`, `FixedValRepro.java`, `SmallDividendRepro.java`,
`ShiftZeroRepro.java`, `BranchIsolateRepro.java`, `CopyOfRangeMicro.java`
(isolated `Arrays.copyOfRange` check, clean/not the cause),
`HashedNameProbe.java` (real production call, reused from the prior session).

## Update 2026-07-17 (diagnostic-tooling session): new `CRATONVM_DBG_AIOOBE3` probe decisively rules out the moved/stale-pointer hypothesis from the entry above (mechanism is data corruption, not GC-root tracking); re-localizes the leading suspect to the **`mulsub` "final-digit" call site** in `divideMagnitude`, which the prior entry's mulsub exoneration did not cover — still not fixed

Picked up this doc's own "next step" #2 (resolve stale-pointer-vs-out-of-bounds-write)
using a new, durable diagnostic instead of the suggested reflection-based
identity-hash probe: added `CRATONVM_DBG_AIOOBE3` (env-gated, behavior-neutral
when unset; landed this session, `jit_throw_aioobe`'s x64 stub now also passes
the array pointer -- `RAX`, already live and unused at that point -- as a 3rd
argument) which dumps the raw `ObjectHeader` (`class_id`, `kind`,
`element_type`, `array_length`, `num_slots`, `gc_age`, **`forwarding_ptr`**) at
the exact pointer the JIT's bounds check compared against, at the moment
`jit_throw_aioobe` fires. This directly answers "did GC move this object out
from under a stale JIT-held pointer" without needing a live debugger or a
reflection probe.

**Repro used:** the prior entry's `SmallDividendRepro.java` (still present at
`/data/hib-baseline-runner-20260716/bigint-divideknuth-repros-20260717/`,
unchanged, still reproduces at ~88% on `dev@08808a57` -- confirmed not fixed by
anything landed between `dev@3e74dd5a` and `dev@08808a57`) under
`CRATONVM_DBG_GC_STRESS=65536` (turns the silent-wrong-result failure into an
immediate, deterministic `ArrayIndexOutOfBoundsException`, per the prior
entry's finding #6) plus `CRATONVM_DBG_AIOOBE3=1`. Fires reliably on the first
stressed run:

```
[AIOOBE3-DIAG] jit-reported index=2 length=2 array_ptr=0x20040408cd0
header: class_id=0 kind=1 elem_ty=10 ident_hash=403 array_length_field=2
num_slots=2 gc_age=1 gc_flags=0 forwarding_ptr=0x0
```

`kind=1` = `ObjectKind::Array`, `elem_ty=10` = `ArrayElementType::Int` (per
`types/src/heap_types.rs`). **This is decisive:**

1. **`forwarding_ptr=0x0`** -- the object was never evacuated/relocated. Combined
   with the prior entry's own observation that `CRATONVM_MOVING_YOUNG` is off by
   default, this closes out hypothesis (a) from the prior entry's finding #6
   outright: this is **not** a moved-object/stale-root-tracking bug. No G1
   evacuation-pointer-staleness fix is needed here.
2. **The header is fully self-consistent** (`array_length_field=2` matches
   `num_slots=2`, plausible `gc_age`/`ident_hash`, no garbage bit patterns) --
   this is a real, validly-allocated `int[2]` object, not heap corruption
   smearing garbage across an unrelated address. The JIT's bounds check itself
   (RAX->header->length compare->RCX index) is doing exactly what it's supposed
   to do; the bug is upstream of it, in *which* index or *which* array
   reference reached that check.
3. This confirms the prior entry's hypothesis (b): the mechanism is a genuine
   **wrong value** -- either an out-of-bounds *index* computed one too high, or
   a live reference that's pointing at the wrong (but validly-allocated)
   `int[2]` object -- not a stale/moved pointer. Per the same MD5-digest-shaped
   input distribution reasoning used throughout this doc's history, a real
   `int[2]` with this exact shape can only plausibly be `divisor` (or the `a`
   parameter it's passed as) inside `MutableBigInteger.divideMagnitude`/
   `mulsub` -- `final int dlen = div.intLen;` is genuinely `2` for the
   production divisor (`longRadix[35]` ~= 61.7 bits) and for
   `SmallDividendRepro`'s fixed `35^12` divisor alike.

**Re-examined the prior entry's finding #3 ("not mulsub") against
`SmallDividendRepro`'s own stated shape and found a gap in it.** Finding #3
correctly shows the main Knuth `for (j=0; j<limit-1; j++)` loop runs zero
iterations for `SmallDividendRepro` (`limit-1 == 0`, small dividend). But
`divideMagnitude`'s real JDK source (`jdk25/lib/src.zip`, confirmed by direct
read) calls `mulsub` a **second, unconditional time outside that loop**, for
the final digit:

```java
// D4 Multiply and subtract  (this is OUTSIDE the `for (j=0; j<limit-1; ...)` loop)
rem.value[limit - 1 + rem.offset] = 0;
if (needRemainder)
    borrow = mulsub(rem.value, divisor, qhat, dlen, limit - 1 + rem.offset);
```

This call is reached exactly once per `divideMagnitude` invocation whenever
`qhat != 0` (the overwhelmingly common case), **independent of how many times
the main loop ran** -- so `SmallDividendRepro` (limit=1, main loop skipped)
still calls `mulsub` exactly once, through this "final-digit" call site.
Finding #3's manual trace verified `mulsub`'s *own* compiled bytecode-to-asm
translation is internally correct for whatever `len`/`a`/`offset` it is
*given* (confirmed independently this session -- full disassembly of
`mulsub([I[IIII)I`, `CRATONVM_DBG_JIT_DISASM=mulsub`, shows a structurally
sound `for (j=len-1; j>=0; j--)` decrement loop with the loop bound (`r13`,
loaded once from the `len` argument register `r8` at entry and never
reloaded) driving both the loop init and the `a[j]` bounds check
consistently). That trace did **not**, and could not by itself, verify that
the *value* arriving in `r8`/`len` at the call site is actually `dlen` -- i.e.
it rules out a bug **inside** `mulsub`, not a bug in **what `divideMagnitude`
passes to it**. Given `mulsub` is a private instance method invoked via
`invokespecial` with `this`+5 params = exactly 6 Java-level arguments, filling
*every* SysV integer argument register (`ARG_REGS = [RDI,RSI,RDX,RCX,R8,R9]`,
`jit/src/x64.rs`) with none left over for stack-arg fallback -- a boundary
condition ("call site needs exactly all 6 registers, no more, no fewer") that
is inherently less exercised/tested than calls with 1-4 args, is a strictly
narrower and more plausible target than a bug shared by every array access in
the method.

**Not fixed this session** -- did not pin the exact faulty instruction (the
divideMagnitude-side call-argument marshaling for the final-digit `mulsub`
call specifically, as opposed to the loop's repeated calls to the same
callee, was not yet isolated by disassembly; `divideMagnitude`'s compiled body
is 24KB and the two call sites are not textually adjacent). Declined to
speculatively patch `emit_stack_arg_setup`/the invokespecial 6-arg direct-call
path (`jit/src/x64.rs` ~line 24505-24525) without first confirming which of
`len` (R8) vs the `divisor` reference itself (RDX, arg index 2) arrives wrong
-- per this doc's own standing guidance, both are equally consistent with the
`AIOOBE3` evidence above, and a wrong fix to shared invoke-dispatch codegen
used far beyond `BigInteger` would be worse than no fix.

**Next step for a follow-up session, in order of expected cost/payoff:**
1. Reuse `SmallDividendRepro.java` + `CRATONVM_DBG_GC_STRESS=65536` +
   `CRATONVM_DBG_AIOOBE3=1` (this session's addition, already on `dev`) for an
   instant, reliable repro/diagnostic loop -- no Hibernate, sub-second, fires
   on the first run.
2. Isolate the two `mulsub` call sites from each other directly: temporarily
   force `SmallDividendRepro`'s divisor/dividend so that `limit >= 2` (main
   loop runs >=1 iteration, exercising the *loop's* `mulsub` call site) vs.
   `limit == 1` (main loop never runs, only the *final-digit* call site is
   exercised, as today) and compare failure rates. If only one shape fails,
   that pins which of the two (textually distinct, separately-codegen'd)
   `mulsub(...)` call sites in `divideMagnitude`'s compiled body is at fault,
   without needing to read the full 24KB disassembly.
3. Once the offending call site is isolated, get its `CRATONVM_DBG_JIT_DISASM`
   output specifically (grep the compiled `divideMagnitude` body around that
   call site's argument-register loads -- `ARG_REGS[4]`=R8=`len`,
   `ARG_REGS[2]`=RDX=`a`/`divisor` per `emit_stack_arg_setup`,
   `jit/src/x64.rs` ~line 11130) and check whether the value loaded into R8
   before the `CALL` genuinely traces back to `dlen`, or whether it's been
   aliased with something else live at that specific program point (e.g. the
   `qhat`/`qrem`/`borrow` locals computed just above it in source, which *are*
   reused/redefined repeatedly right before this call).
4. If the call-argument marshaling turns out clean, fall back to the prior
   entry's step 4 (audit `primitiveLeftShift`'s inlined tail store /
   `remarr[intLen+1]` write) -- note this session found the 3-argument
   `primitiveLeftShift(int, int[], int)` overload that actually fills
   `divisor` (`div.primitiveLeftShift(shift, divisor, 0)`) does **not** appear
   in `CRATONVM_DBG_JIT_DISASM=primitiveLeftShift` output for this repro (only
   the 1-arg `primitiveLeftShift(I)V` overload compiles) -- i.e. it stays
   **interpreted**, which combined with `--nojit` being clean makes this
   overload a low-probability suspect on its own, but does not rule out
   `divisor`'s *header/reference* being corrupted by something else before
   `mulsub` reads it.

## Update 2026-07-17 (follow-up session, concurrent with the `CRATONVM_DBG_AIOOBE3` session above): could NOT reproduce despite extensive re-verification — including with that session's own frozen crashing binary, run 80+ times — extremely fragile, process/environment-sensitive; still OPEN, no fix landed, no closure claimed

Picked up this doc's own "next step" from the `fast, minimal, Hibernate-free
repro found` entry (resolve stale-reference-vs-OOB-write via array-identity
instrumentation, then fix `emit_inline_tlab_new`/JIT scratch-slot GC-root
tracking). Ran concurrently with the `CRATONVM_DBG_AIOOBE3` diagnostic
session above on the same shared host (worktree
`wt-hib-bigint-divideknuth-20260717`, branch
`fix/hib-bigint-divideknuth-jit-20260717`) — the two sessions' work is
reconciled here rather than presented as sequential.

**Before instrumenting anything, re-ran the prior entry's own fast repros as a
sanity baseline on `dev@08808a57` (the tip at session start) — and got a
surprising negative result: none of them reproduced.** `SmallDividendRepro`:
0 failures across 570,000+ combined trials (20,000×6 default runs, 50,000
under `CRATONVM_DBG_GC_STRESS=4096`/`65536`, 500,000 in one run, individual
reruns with `CRATONVM_MOVING_YOUNG=1`, `CRATONVM_DISABLE_UNROLL=1`,
`CRATONVM_TIER_C1_THRESHOLD=5`, `CRATONVM_NO_PRECISE_JIT_MAPS=1`,
`CRATONVM_COMPACT_REF_FIELDS=0`, and an artificial 128-process/120+-load-average
host-contention condition). `FixedValRepro`/`ShiftZeroRepro`/
`BranchIsolateRepro`/`HashedNameProbe` (2000 real `NamingHelper.hashedName`
trials): all 0 failures. Verified this wasn't explained by any code landing
on `dev` since the prior entry's final verification point (`dev@8ef4d59b`):
only two non-merge commits existed in that range (`76514940`, unrelated
`Files.walkFileTree` fix, and `6599b898`, a real but unrelated
`emit_inline_tlab_new` stale-compact-object-size fix for `NEW` object
allocation, not `int[]` array allocation) — reverting `6599b898` in a scratch
A/B build made no difference, and rebuilding `dev@8ef4d59b` **verbatim** in a
clean (`env -i`) environment still did not reproduce (0 failures across the
same battery of tests). A full real-Hibernate whole-class run via the actual
harness driver (`CratonRunner`, `DiscoverySelectors.selectClass` — the same
invocation a prior entry in this section could never get to complete under
host contention) completed cleanly this session:
```
@@RESULT 0 org.hibernate.orm.test.boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest found=132 started=132 ok=132 failed=0 aborted=0 skipped=0 ms=597185
```
132/132, matching the recorded HotSpot baseline exactly, with zero
`ArrayIndexOutOfBoundsException` occurrences. The 2-method interleaved
`MultiMethodRunner` repro (`entityPersister` + `createSchema_fromSessionFactory`,
which a prior entry explicitly reported as still failing) was also 24/24
clean this session.

**This was heading toward a "closed, does not reproduce" conclusion until
`git fetch` picked up the concurrent `CRATONVM_DBG_AIOOBE3` session's push
(commits `642fa3a5`/`87d14299`), which directly contradicts it: that session
reports the identical `SmallDividendRepro` + `CRATONVM_DBG_GC_STRESS=65536`
recipe "fires reliably on the first stressed run" on the identical
`dev@08808a57`, with decisive diagnostic evidence (a raw `ObjectHeader` dump
at the JIT bounds-check failure showing a fully self-consistent, non-relocated
`int[2]` — `forwarding_ptr=0x0` — proving this is genuine data corruption,
not a stale/moved pointer).** That finding is trusted over this session's own
non-reproduction: it is artifact-based (an actual captured header dump from a
live failure), not merely an absence of failure.

**Went further to reconcile the two rather than just noting the conflict.**
Merged the concurrent session's commits into this session's worktree,
rebuilt, and reran the identical `SmallDividendRepro` +
`CRATONVM_DBG_GC_STRESS=65536` + `CRATONVM_DBG_AIOOBE3=1` recipe against
**three different binaries**: this session's own merged-tip build (0/3000,
×3), a from-scratch `CARGO_PROFILE_RELEASE_LTO=off` build matching a sibling
concurrent session's build flags (0/3000, ×3), and — most decisively — **the
literal frozen binary the `CRATONVM_DBG_AIOOBE3` session itself used**
(`/data/data/frozen-hib-biginteger-aioobe-20260717/cratonvm-biginteger-devtip-20260717`,
byte-for-byte the exact executable that produced their `AIOOBE3-DIAG` dump):
**80 consecutive process invocations (20 + 60), 0 failures, 0 `AIOOBE3-DIAG`
lines fired.** Same binary, same repro source, same flags, same host — no
crash. This rules out a build-configuration explanation (LTO on/off) and
confirms the divergence is not "which commit" or "which build flags" but
something about live process/runtime state at invocation time (heap/stack
addresses under ASLR — confirmed enabled, `randomize_va_space=2` — ambient
memory pressure, scheduling, or some other per-process-launch variable this
session did not identify).

**Conclusion: NOT closing this item.** The concurrent session's artifact-based
evidence (an actual `ObjectHeader` dump from a live crash) is real and takes
priority over this session's inability to reproduce it. What this session
adds is a data point about just how fragile the trigger condition is: even
the literal crashing binary, invoked the same way, on the same host, did not
crash 80/80 times for this session's process launches. This is consistent
with (and updates) the "heisenbug" framing from an earlier entry in this
section (the "scaling-investigation session" finding that the bug required
multi-method interleaving and oscillated pass/fail for identical inputs
across different process runs) — the trigger condition is apparently
sensitive to something below the level of "which code" or "which repro,"
down to specific runtime/address-space conditions at process-launch time.
This session's clean 132/132 whole-class run and clean 80/80 frozen-binary
runs should **not** be read as evidence the bug is fixed or even rare in
practice — only that this session's particular process launches did not hit
the trigger window.

**Handing off to the concurrent session's own next-step plan** (already
documented in the `CRATONVM_DBG_AIOOBE3` entry immediately above: isolate
`divideMagnitude`'s two `mulsub` call sites — the main-loop one vs. the
unconditional final-digit one outside the loop — from each other by forcing
`limit >= 2` vs. `limit == 1`, then get `CRATONVM_DBG_JIT_DISASM` output
around the final-digit call site's `R8`/`RDX` argument-register loads to
check whether the value reaching `mulsub` for `len`/`divisor` is genuinely
`dlen`/the divisor reference, or aliased with something else live at that
program point). Given this session could not get the bug to fire at all, it
was not able to make further progress on that specific plan and defers to
whichever session next gets a live reproduction — check for further updates
from the `fix/hib-biginteger-smalltostring-aioobe-20260717`-lineage work
(worktrees `wt-hib-mulsub-oldtip-20260717`/`wt-hib-mulsub-argmarshal-20260717`
were both active on the shared host during this session, likely the same
investigation continuing) before restarting from scratch.

No code change made this session (nothing reproduced to validate a fix
against, and this session's own would-be "closed" conclusion was itself
superseded by better evidence before being finalized).

## Update 2026-07-17 (follow-up session, `wt-hib-mulsub-argmarshal-20260717`/`wt-hib-mulsub-oldtip-20260717`): picked up the isolation plan directly; STILL cannot reproduce (620,000+ combined trials, 3× clean full-class 132/132 runs) — but static analysis of the suspected `mulsub` register-marshaling path finds the specific clobber mechanism previously hypothesized does NOT apply to this call shape. Not closing (per this doc's own standing conclusion above); root cause remains genuinely unresolved.

Picked up this doc's own concrete next step from the `CRATONVM_DBG_AIOOBE3`
entry: isolate `divideMagnitude`'s two `mulsub` call sites (main-loop vs. the
"final-digit" one) via `limit>=2` vs. `limit==1`, then inspect the
final-digit call site's real register loads via `CRATONVM_DBG_JIT_DISASM`.

**Build/verification setup.** Fresh isolated worktree
(`wt-hib-mulsub-argmarshal-20260717`, branch
`fix/hib-mulsub-argmarshal-20260717`, `dev@67db1afb` at start,
`CARGO_PROFILE_RELEASE_LTO=off`), plus a second from-scratch build pinned
to `dev@08808a57` (`wt-hib-mulsub-oldtip-20260717`) — the exact commit the
`CRATONVM_DBG_AIOOBE3` entry above reports firing "reliably on the first
stressed run."

**Step 1 (limit isolation) could not be completed as planned — nothing
reproduced to isolate.** Added a `BigDividendRepro.java` (~140-bit dividend
against the same fixed `35^12` divisor, forcing `limit>=3` so the main D2-D7
loop's own `mulsub` call site runs repeatedly) alongside the existing
`SmallDividendRepro.java` (`limit==1`, final-digit call site only). Both
variants, against **both** binaries (`dev@08808a57` and `dev@67db1afb`),
under `CRATONVM_DBG_GC_STRESS` at 65536/16384/4096/2097152-byte thresholds,
default settings, and `CRATONVM_TIER_C1_THRESHOLD=50` (to force `mulsub`/
`divideMagnitude` to compile early — confirmed via
`CRATONVM_DBG_JIT_DISASM=divideMagnitude,mulsub` that both methods really do
get JIT-compiled in these runs, 23607/1029 bytes respectively, matching the
`fast, minimal, Hibernate-free repro` entry's own byte counts): **0 failures
across 620,000+ combined `SmallDividendRepro`/`BigDividendRepro` trials**
(20 back-to-back 5000-trial runs alone accounted for 100,000 of these, all
clean). `HashedNameProbe` (the real `NamingHelper.hashedName` call, 2000
trials against the real Hibernate classpath): also clean on both binaries.
**Three separate full real-harness `CratonRunner`/`selectClass` whole-class
runs** (the exact harness invocation, `-Dcraton.trace=1`), all against the
`dev@67db1afb` binary: `found=132 started=132 ok=132 failed=0 ms=700951`,
`ms=722262`, `ms=642977` — three clean 132/132 runs in a row, zero
`ArrayIndexOutOfBoundsException` anywhere in any of the three logs. This
independently reproduces (and extends, via the `BigDividendRepro`
limit-isolation attempt and a third-tip cross-check) the immediately-preceding
entry's own non-reproduction finding — consistent with that entry's
"extremely fragile, process/environment-sensitive" framing, not a
contradiction of it.

**Step 2 substitute: since no live failure was available to disassemble, did
the next-best thing — a static code-path audit of the specific register-clobber
mechanism this doc's `CRATONVM_DBG_AIOOBE3` entry flagged as the leading
suspect** (`emit_stack_arg_setup` in `jit/src/x64.rs`, the loop that walks
`arg_slots[0..reg_arg_count]` and writes each into `ARG_REGS[i+ctx_offset]`
in ascending order). Two corrections/refinements to the prior entry's framing:

1. **The two `mulsub` call sites in `divideMagnitude` are `invokevirtual`,
   not `invokespecial`**, per direct `javap --system <jdk25> -c -p
   java.math.MutableBigInteger` output (bytecode offsets 719 and 1084,
   `invokevirtual #289 // Method mulsub:([I[IIII)I`). This doesn't change
   the argument-count analysis (CratonVM's JIT still resolves `mulsub` as a
   private, effectively-non-polymorphic method and takes the same
   direct-call fast path used for `invokespecial`), but the doc's framing of
   "invoked via `invokespecial`" should be read as CratonVM's internal
   dispatch classification, not the literal bytecode opcode.
2. **The specific `SCRATCH_REGS`/`ARG_REGS` aliasing clobber this doc's
   `CRATONVM_DBG_AIOOBE3` entry flagged as the boundary condition to check
   (R8/R9 double as both the last two `ARG_REGS` slots and the only two
   `SCRATCH_REGS`, so overwriting one before reading the other as a call
   argument's source could silently swap/corrupt the last two arguments)
   does **not appear to be reachable for this call's actual bytecode shape**,
   on direct code reading:
   - `push_from_rax` (`jit/src/x64.rs`) — the path that materializes the
     result of an arithmetic sub-expression like the final-digit call's
     `limit - 1 + rem.offset` argument — **always spills to a `Frame` slot**,
     never a `Scratch` register; the doc comment there explicitly records
     that scratch-register caching for this path "was tested but showed
     regressions" and was reverted. So a computed-expression argument (the
     one most likely, on general principle, to still be sitting in a
     register right before the call) is not actually a `Scratch`-sourced
     value in this JIT's current implementation.
   - The only two places in `jit/src/x64.rs` that push a `StackSlot::Scratch`
     value onto the simulated operand stack at all are both inside `dup`/
     `dup2` handling (`emit_dup_top_slot` and one `dup2`-form site) — i.e. a
     value only becomes `Scratch` by being a duplicated copy of an
     already-scratch top-of-stack. `divideMagnitude`'s two `mulsub` call
     sites (per the `javap` bytecode: `aload_0; aload N; getfield value;
     aload N; iload N; iload N; iload N; getfield offset; iadd; invokevirtual
     mulsub`) don't contain a `dup`/`dup2` in the argument-pushing sequence,
     so none of the 6 arguments should ever arrive as a `Scratch` slot for
     this specific call shape.
   - Net: the "last-two-of-six-registers-alias-the-only-two-scratch-regs"
     clobber this doc flagged as the next thing to check is real code (and
     could plausibly bite *some* 6-argument call site that does go through
     `dup`), but does not look reachable for `divideMagnitude`'s `mulsub`
     calls specifically. **This is a negative result from static reading
     only** — without a live failing capture to disassemble (see Step 1),
     it was not possible to empirically confirm what `arg_slots[4]`/
     `arg_slots[5]` actually are at the real call site, only to audit the
     general code paths that could produce a `Scratch` slot there. A future
     session that does get a live capture should still verify this directly
     via `CRATONVM_DBG_JIT_DISASM` rather than trust this static conclusion
     alone.
3. Also confirmed, as a byproduct of getting `mulsub`/`divideMagnitude` to
   actually cross the (now-1500-invocation-default) JIT compile threshold
   reliably: `CRATONVM_DBG_TIER_ENQUEUE`'s "enqueue" log line does **not**
   fire for either method even when they demonstrably do get JIT-compiled
   (confirmed via `CRATONVM_DBG_JIT_DISASM` showing real compiled bodies at
   the expected byte sizes) — worth a note for any future session using that
   diagnostic to gate on compilation state: it does not cover every compiled
   method (plausibly an inlining or logging-coverage gap in `tiered.rs`
   unrelated to this bug), so its absence should not be read as "never
   compiled."

**Regression check.** `cargo test --release -p cratonvm-jit --lib`:
906/906 pass (the `cargo test` default target set separately fails to
*compile* 6 pre-existing integration-test files over a `JitRuntimeHelpers`
struct-literal/arg-count mismatch unrelated to this session's — or any
recent — change; not investigated further, flagged here only so a future
session doesn't mistake it for a regression from this entry).

**Not closing this item.** This session's own non-reproduction (even
extending the immediately-preceding entry's already-extensive battery with
a third dev tip, a `limit>=2` variant, and 3 full clean end-to-end harness
runs) does not outweigh the `CRATONVM_DBG_AIOOBE3` entry's artifact-based
evidence (an actual `ObjectHeader` dump captured from a real, live crash) —
per this doc's own standing guidance and the immediately-preceding entry's
explicit reasoning, absence of failure is not evidence of a fix. The
register-marshaling hypothesis is now better-understood (specific enough to
mostly rule out on static grounds, for this exact call site) but not
replaced with a confirmed alternative.

**Next step for a follow-up session**, given two sessions now (this one and
the immediately-preceding one) have burned significant time on
process-at-a-time reproduction attempts without success:
1. Stop trying single bounded runs. Given the trigger is "below
   process-launch granularity" fragile (per the preceding entry), the next
   session should set up a genuinely long-running (many-hours,
   background/unattended) loop of the frozen crashing binary +
   `SmallDividendRepro` + `CRATONVM_DBG_GC_STRESS=65536` +
   `CRATONVM_DBG_AIOOBE3=1`, with the process configured to core-dump on the
   `AIOOBE3-DIAG` firing (or, simpler, just redirect stdout/stderr to a file
   and grep it periodically), and let it run far longer (hundreds to
   thousands of process launches, or one very long-lived process doing
   millions of trials with periodic forced GCs) than any session so far has
   budgeted — both this session and the preceding one gave up after tens to
   hundreds of thousands of trials, which may simply not be enough given how
   rare the trigger apparently is.
2. If/when a live capture is obtained, verify this entry's static
   `Scratch`/`push_from_rax` analysis directly against the real compiled
   `divideMagnitude` body for that specific process (does `arg_slots[4]`/
   `arg_slots[5]` at the final-digit `mulsub` call site actually resolve to
   `Frame`, as this entry's static reading predicts, or something else?)
   before spending time on any speculative fix.
3. If the register-marshaling theory is ruled out by (2), fall back to the
   `fast, minimal, Hibernate-free repro` entry's own step 4 (audit the
   `remarr[intLen+1]`/`primitiveLeftShift` inlined tail-store bounds) as the
   next most promising concrete lead — not investigated by either this
   session or the preceding one.

No code change made this session (nothing reproduced to validate a fix
against; per this doc's own standing guidance, a wrong fix to shared
invoke-dispatch codegen used far beyond `BigInteger` would be worse than no
fix, and this session could not even get to "which of the two hypotheses in
finding #6 is right," let alone confirm a specific defective instruction).


## Update 2026-07-17 (post-`f377eb69` GC-conservative-scan-fix session): the hypothesis that the `LockTest` conservative-roots GC-scan fix (`f377eb69`) also fixed this BigInteger AIOOBE is **REFUTED on git-ancestry grounds**; ~930k fresh trials + 5 clean end-to-end runs still 0 occurrences, but that is non-reproduction (consistent with the prior two sessions), not a fix. Still OPEN.

A session was dispatched specifically to test the hypothesis that `f377eb69`
(`fix(jit): incremental unregistered-JIT-frame scan when recursion deepens`,
branch `fix/hib-jit-tiering-heuristic-20260717`, the `LockTest`/GC
conservative-scan fix documented in the `LockTest` section below) had
*incidentally* fixed — or shared a mechanism with — this BigInteger AIOOBE,
on the grounds that both live in the same GC-root-scanning-near-JIT-frames
subsystem (`vm/src/jit/conservative_roots.rs`).

**The hypothesis is refuted by the commit graph, before any trial was run:**

- `f377eb69` is dated **2026-07-17 04:17:45 UTC**.
- It is a **strict ancestor of `08808a57`** (05:14 UTC) — the exact tip on
  which the `CRATONVM_DBG_AIOOBE3` diagnostic session (entry above) captured
  the **one decisive live crash** and reported the repro firing "at ~88% on
  `dev@08808a57`."
- It is likewise a strict ancestor of **`67db1afb`** (05:33 UTC), the
  620,000-trial non-reproduction tip.
- The exact incremental-band code `f377eb69` introduced (the
  `search_lo < verified_lo` "recursing deeper" branch in
  `scan_active_jit_frames`) is verified **present in the trees of both
  `08808a57` and `67db1afb`**, and at the current `origin/dev` tip
  (`git show <tip>:vm/src/jit/conservative_roots.rs | grep 'search_lo < verified_lo'`
  → present in all three). It was not reverted; a later perf commit
  (`b7a1ed84`) touched the same file but did not remove it.

Therefore `f377eb69` was **already live in the binary that produced the one
confirmed live crash**. A fix that predates the last confirmed reproduction
of a bug cannot be what fixed it. This is corroborated independently by the
`CRATONVM_DBG_AIOOBE3` entry's own wording — it states the repro was
"confirmed not fixed by anything landed between `dev@3e74dd5a` and
`dev@08808a57`," a commit range that **contains** `f377eb69`. The subsequent
non-reproduction across 620k+ trials happened on binaries that also already
contained `f377eb69` — same as the crash binary — so the fix explains
neither the crash nor the later dormancy. It is orthogonal to this bug.

The broader "maybe a *different* correctness bug lurks in the same
conservative-scan subsystem" framing is also disfavored by pre-existing
evidence, not just the specific-commit refutation: the `CRATONVM_DBG_AIOOBE3`
entry's captured `ObjectHeader` dump showed `forwarding_ptr=0x0` on a
fully self-consistent, non-relocated `int[2]` — i.e. the object was never
moved and no GC-root/stale-pointer tracking was involved. That points at
genuine value/index data corruption in the divide path, away from (not
toward) any missed-root-during-scan mechanism. `f377eb69`'s change is
purely to the *detection* scan's extent (it narrows how much stack is
re-scanned; it never changes what gets marked once a frame is found), which
cannot introduce or remove a data-corruption bug of this shape.

**Empirical work this session (for completeness, against a fresh binary that
includes `f377eb69` and ~30 later `dev` commits):**

- Built a fresh `origin/dev` release binary in an isolated worktree
  (`/data/data/wt-hib-biginteger-postgcfix-20260717`, `dev@394f9c93`,
  `CARGO_PROFILE_RELEASE_LTO=off`), `f377eb69` confirmed present; frozen at
  `/data/data/frozen-postgcfix-20260717-cratonvm`, md5
  `a29c51eb3bf76b760b7acabe8dd1333e`.
- **Stress sweep, ~930,000 combined trials, 0 failures / 0 `AIOOBE3-DIAG` /
  0 crashes:** `SmallDividendRepro` (the sub-second, Hibernate-free repro
  the `CRATONVM_DBG_AIOOBE3` entry reported firing "reliably on the first
  stressed run") under `CRATONVM_DBG_GC_STRESS` at 65536 / 16384 / 4096,
  `CRATONVM_DBG_AIOOBE3=1`, with and without `CRATONVM_TIER_C1_THRESHOLD=50`
  to force early compilation. Split as: 540,000 trials (108×5000) against
  the frozen confirmed-crash binary `cratonvm-biginteger-devtip-20260717`
  **and** the pre-`f377eb69` control `cratonvm-biginteger-base-20260717`
  (built 04:16, one minute before `f377eb69` — an explicit A/B: neither the
  pre-fix nor the post-fix frozen binary reproduced), plus 390,000 trials
  (78×5000) against the fresh post-fix build. Zero `@@BAD` wrong-quotient
  results and zero AIOOBE across all of it.
- **5 whole-class end-to-end runs** via the real harness (`CratonRunner`,
  `DiscoverySelectors.selectClass`, `CRATONVM_DBG_AIOOBE3=1`) of
  `DefaultCatalogAndSchemaTest` — the multi-`@Test`-method-in-one-process
  condition the "scaling-investigation session" identified as the ~47-52%
  whole-class trigger — 3 on the frozen post-fix `devtip` binary
  (ms=649230/678809/680405) + 2 on the fresh build (ms=641071/673979):
  **every run `found=132 started=132 ok=132 failed=0 aborted=0`**, matching
  the recorded HotSpot baseline exactly. Zero
  `ArrayIndexOutOfBoundsException` / `smallToString` / `AIOOBE3-DIAG` in any
  run.
- `cargo test --release -p cratonvm-jit --lib` on the documented tip:
  **906/906 pass** (no regression; matches the `f377eb69` fix session's own
  report for this target).

**Conclusion: NOT closing.** The `f377eb69` hypothesis is refuted; this
session's ~930k-trial + end-to-end non-reproduction is fully consistent with
the immediately-preceding two sessions' non-reproduction (this is now the
**third consecutive session** unable to reproduce, cumulative >1.5M trials
across them) and, by this doc's own standing conclusion, absence of failure
is not evidence of a fix while the `CRATONVM_DBG_AIOOBE3` artifact-based live
capture stands. The one meaningful advance this session adds is **eliminating
`f377eb69`/the conservative-roots-scan subsystem as the explanation**, so a
future session should not re-test that angle. The concrete next step is
unchanged from the two entries above: a genuinely long-running (many-hours,
unattended) loop of the frozen crashing binary +
`SmallDividendRepro`/`CRATONVM_DBG_GC_STRESS=65536`/`CRATONVM_DBG_AIOOBE3=1`
to obtain a *second* live capture, then isolate `divideMagnitude`'s two
`mulsub` call sites (`limit>=2` vs `limit==1`) and disassemble the
final-digit call site's `R8`/`RDX` argument-register loads against a real
failing process. No code change made this session (nothing reproduced to
validate a fix against; the only candidate mechanism was refuted, not
replaced).

## Update 2026-07-17 (independent same-day session, run concurrently with/immediately after the "post-`f377eb69`" session above): the "0/6 clean" empirical finding above does NOT replicate -- this session got 7/7 full end-to-end runs FAILING consistently at ~50-55%, with fresh `CRATONVM_DBG_AIOOBE3` captures; hypothesis still REFUTED (agrees with the entry above), but flagging an unresolved same-day reproducibility discrepancy

**Setup, independent of the entry immediately above.** Fresh worktree
`wt-hib-biginteger-postgcfix-verify-20260717`, pinned to `origin/dev` at
`fd7a241d` (fetched via `git show origin/dev:...` to confirm content, not a
stale local checkout), confirmed via `git merge-base --is-ancestor f377eb69
HEAD` before building. `CARGO_PROFILE_RELEASE_LTO=off cargo build --release
-p cratonvm-cli`, frozen to
`/data/data/frozen-hib-biginteger-postgcfix-verify-20260717/cratonvm-postgcfix-verify-20260717`
(md5 `81946d8b512c93b937e56bbd0857176a`). Verified only 8 non-doc commits
separate this tip from the current `origin/dev` tip
(`7f18c4f1`/`7a8222eb`/`6882531a`/`ce78b92f`/`394f9c93`/`21ac1849`
[`ArrayList$ListItr` no-op-stub fix]/`9b39da87`/`c7868c30` [AIO
completion-dispatcher deadlock fix]) -- none touch JIT codegen, GC,
`conservative_roots.rs`, or `BigInteger`/`generics.rs`, so this binary and
the immediately-preceding entry's "fresh" binary should be code-equivalent
for this bug's purposes. The shared Hibernate fixture
(`/data/data/apps/hibernate-orm-harness/hib-libs/test-classes`) was also
confirmed unchanged since the `HARNESS-REBUILD-20260717.md` rebuild (no
files newer than that doc, `common.args` md5 unchanged) -- ruling out a
different-fixture-build explanation for what follows.

**Isolated micro-repro: 840,000 trials, 0 failures, 0 `AIOOBE3-DIAG` hits.**
`SmallDividendRepro`/`BigDividendRepro` (5000 trials/launch x 100 process
launches, `CRATONVM_DBG_GC_STRESS` in {65536, 4096, 16384, 262144, 2097152})
plus `HashedNameProbe` (real `NamingHelper.hashedName` call, 2000 trials x 20
launches, `CRATONVM_DBG_GC_STRESS=65536`) against this binary: uniformly
clean. This part fully agrees with the immediately-preceding entry and every
prior session back to the `wt-hib-mulsub-*` entries -- the isolated repro
tooling has now gone 0-for-well-over-1,000,000 combined trials across at
least four independent sessions and should probably be deprioritized in
favor of the finding below.

**Full end-to-end harness (`CratonRunner`/`DiscoverySelectors.selectClass`,
default settings, no stress env vars): 7/7 runs FAILED, consistently, at
50-55%.** This directly contradicts the immediately-preceding entry's "5
clean end-to-end runs" on what should be equivalent code:

```
run 1 (plain default):                 found=132 started=132 ok=64 failed=68 ms=543996
run 2 (plain default):                 found=132 started=132 ok=64 failed=68 ms=527804
run 3 (plain default):                 found=132 started=132 ok=66 failed=66 ms=514271
run 4 (plain default):                 found=132 started=132 ok=60 failed=72 ms=467151
run 5 (plain default):                 found=132 started=132 ok=60 failed=72 ms=437629
run 6 (CRATONVM_DBG_AIOOBE3=1 only):   found=132 started=132 ok=66 failed=66 ms=531132
run 7 (CRATONVM_NO_PRECISE_JIT_MAPS=1
       + CRATONVM_DBG_AIOOBE3=1):      found=132 started=132 ok=66 failed=66 ms=516149
```

Every single failure in a sampled run (`grep`, run 1, 68/68) is the
identical, exact documented signature:
`java.lang.ArrayIndexOutOfBoundsException: Index 2 out of bounds for length 2`
(no `InvalidMappingException` variety seen this session -- may be a rarer
secondary symptom, or may correlate with something this session's runs
didn't hit). This is **not flaky pass/fail noise** -- the failure count is
tightly banded (66-72 of 132, i.e. every run independently lands within a
~9% window of ~51% failure rate) across 7 independent process launches, some
minutes apart, under materially different env-var configurations (plain,
`AIOOBE3`-instrumented, precise-maps-disabled) -- indicating a deterministic
per-test-method trigger condition under this session's environment, not a
rare/lucky hit.

**Fresh `CRATONVM_DBG_AIOOBE3` captures (run 6) reproduce the exact same
diagnostic signature as the original decisive capture from two sessions
ago**, at scale (dozens of hits across the run, all identical):

```
[AIOOBE3-DIAG] jit-reported index=2 length=2 array_ptr=0x20042648808
header: class_id=0 kind=1 elem_ty=10 ident_hash=6547 array_length_field=2
num_slots=2 gc_age=1 gc_flags=0 forwarding_ptr=0x0
```

`forwarding_ptr=0x0`, internally self-consistent `int[2]` header -- same as
before. This reconfirms (does not newly establish) that this is genuine data
corruption / wrong-reference-or-index, not object relocation. **New this
session:** run 7 (`CRATONVM_NO_PRECISE_JIT_MAPS=1`, forcing every GC root
lookup through the conservative scanner instead of precise oop maps)
produced an **identical** failure count (66/132) to run 6's precise-maps-on
baseline (66/132). If either the precise-map path or the conservative-scan
path (the `f377eb69` subject) were the actual defect, forcing full-time
reliance on the *other* one should have shifted the failure rate one way or
the other. It didn't move at all. This is a second, independent piece of
evidence (on top of the git-ancestry argument in the entry above) against
*any* GC-root-tracking explanation, precise or conservative -- and further
supports the `divideMagnitude`/`mulsub` arithmetic-or-argument-marshaling
codegen hypothesis from the "fast, minimal, Hibernate-free repro" entry over
any GC-root theory.

**This session did not attempt a fix.** Per this doc's own standing
guidance and given the exact faulty instruction still isn't pinned (only
now ruled further away from GC-root theories), landing a speculative patch
to shared JIT/GC infrastructure would be irresponsible.

**Unresolved discrepancy, flagged honestly rather than silently
overwritten:** this session's reproduction is about as strong as evidence
gets for this doc's history (7/7 runs, tight failure-rate band, matching
diagnostic signature, ruled out two GC-root theories) -- yet the
*immediately preceding, same-day* session, working from code that appears
equivalent and the same shared fixture, reported the opposite (6/6 clean,
940k clean isolated trials). Both sessions' methodology looks sound on
inspection; neither obviously made a setup mistake. Given this doc's own
long-standing "heisenbug, extremely fragile, process/environment-sensitive"
framing (previously observed as *rare, hard-to-trigger* crashes against a
*normally-clean* baseline), this is the first time the *opposite* polarity
has been observed -- a session where the bug looks like a **reliable,
majority-of-runs** failure instead of a rare one. Whether this reflects
genuine moment-to-moment host/environment sensitivity (extreme, but
consistent with this doc's history), some subtle undetected divergence
between the two sessions' otherwise-parallel setups, or something else
entirely was not resolved this session.

**Practical recommendation for the next session, superseding the isolated
tools' priority:** the full end-to-end harness invocation is, *right now, in
this session's environment*, a **reliable ~9-minute, ~50%-hit-rate repro** --
categorically better than the isolated micro-repro tools, which have never
once reproduced across 4 sessions and >1.5M combined trials. If this
reliability holds for a follow-up session too, that session should pivot
straight to `CRATONVM_DBG_JIT_DISASM=divideMagnitude,mulsub` against a live
full-harness run (not the isolated repro) to finally attempt pinning the
exact faulty instruction the "fast, minimal, Hibernate-free repro" and
`CRATONVM_DBG_AIOOBE3` entries above narrowed to but could never get a live
capture to confirm against. If it does *not* reproduce reliably for that
next session either, that itself is useful evidence for the
environment-sensitivity explanation over a setup-divergence explanation.

**This item remains OPEN.** The hypothesis under test this round (BigInteger
AIOOBE fixed by/related to `f377eb69`) is REFUTED -- agreeing with the entry
immediately above, now via three independent lines of evidence (git
ancestry, this session's high-volume live reproduction on the "fixed" tip,
and the precise-maps-on/off invariance). Not moving to
`docs/internal/fixed-suite-bugs/` -- this is not a resolution, and per this
update, if anything the bug is *more* clearly alive and reproducible right
now than the entry immediately above suggested. The rest of this doc's items
are independently closed (see their own sections) but this one blocks
declaring the whole `hib-misc-residuals-20260716.md` doc closed.


## Update 2026-07-17 (live-gdb-capture session): mulsub argument-marshaling hypothesis DEFINITIVELY REFUTED via live register-state correlation; crash re-localized with high confidence to `divideMagnitude`'s own D1-normalize array-allocation-vs-tail-write `intLen` consistency, not any callee. Still not fixed — root cause narrowed to a specific, mechanically-verifiable next check, but the exact clobbered register/slot not pinned before session time ran out.

Picked up this doc's own "next step" (with a ~50% hit rate on the real workload,
attach `gdb` to a live process and catch the crash in real time). Host was
notably idle this session (load average 0.02-2, vs. the 8-215 contention
ranges every prior session on this item reported) — the first genuinely quiet
window this investigation has had.

**Own repro rate, confirmed independently, twice, at default settings (no
`CRATONVM_DBG_GC_STRESS`, no env overrides):** fresh binary built from
`origin/dev@39f89987` (worktree `wt-hib-biginteger-rootcause-20260717`,
`CARGO_PROFILE_RELEASE_LTO=off`, frozen + md5-verified at
`/data/data/frozen-hib-biginteger-rootcause-20260717/cratonvm-biginteger-rootcause-20260717`,
md5 `39052b5f2dda78e610c079f8d0ecb85a`). Two consecutive full-harness runs
(`CratonRunner`/`DiscoverySelectors.selectClass`, exactly the harness's own
invocation):
```
run1: found=132 started=132 ok=64 failed=68 aborted=0 skipped=0 ms=456646
run2: found=132 started=132 ok=60 failed=72 aborted=0 skipped=0 ms=431340  (CRATONVM_DBG_AIOOBE3=1 -Dcraton.trace=1)
```
51.5% and 54.5% failure rates — squarely in the "reliable ~50%" band the
immediately-preceding same-day session reported, **not** the "0/6 clean"
band a different immediately-preceding session reported the same day. This
session's environment reproduces the bug reliably and did not need to
resolve that standing discrepancy further — it simply confirms which
polarity holds *right now*, on a quiet host, consistent with the doc's
existing "practical recommendation" to trust the reliable-repro polarity
going forward.

Both runs' `CRATONVM_DBG_AIOOBE3` captures (run2, 72 hits) show the **exact
same** `array_ptr`/`ident_hash` for every single one of the 72 failures in
that run (`array_ptr=0x200427cf640`/`ident_hash=6547`, repeated verbatim 72
times) — a new observation this doc hadn't explicitly called out before.
Combined with the "hard state transition" finding two entries above (correct
before the relevant methods finish compiling, then consistently wrong on
every subsequent call), this is consistent with one specific compiled
artifact, once installed, deterministically mis-computing the same failure
against whichever object happens to be live at that moment — not a
per-call/per-object heisenbug once the bad compile is in place.

### Live gdb capture: `jit_throw_aioobe` breakpoint against a live full-harness run

Added a `gdb -batch` wrapper (`break
cratonvm_vm::jit::helpers::jit_throw_aioobe`, dump registers +
`frame 6`/`bt`/instruction window on every hit, then `continue`) around the
literal harness invocation (`gdb -batch -x <script> --args <cratonvm>
@common.args CratonRunner <testlist> 0`) — no isolated repro tool, no stress
flags, just the real harness under a debugger. This fired within ~4-6
minutes of wall clock every time, multiple independent launches, always
landing on the **same fixed native return address** within one process
(confirmed identical across 8+ consecutive hits in one run) — i.e. the same
JIT-compiled call site, deterministically, for that process's lifetime.

**Captured at the moment of every throw (representative sample, byte-for-byte
identical across 8 consecutive hits in one run and reproduced again in two
further independent gdb launches):**
```
index(rdi)=2 length(rsi)=2 array_ptr(rdx)=0x20042646880
r12=-261202831 r13=1 r14=-261202831 r15=2
rax=93824999136800 rbx=2 rcx=2 r8=0 r9=2 r10=2
```
Frame 6 (the JIT-compiled caller of `jit_throw_aioobe`) always resolves to
the same address within one launch (e.g. `0x00007fffece76362` /
`0x00007fffece78362` / `0x00007fffece74362` across different launches —
same relative offset each time modulo ASLR base, confirmed by the
byte-identical disassembly window at each). The instructions immediately
preceding the call, and the instructions immediately after the return
address, are decisive:
```
   ...
   mov    %rax,%rdx        ; array_ptr
   mov    %rcx,%rdi        ; index
   mov    %r10,%rsi        ; length
   movabs $<jit_throw_aioobe addr>,%rax
   call   *%rax
=> mov    -0xc0(%rbp),%rbx     <-- epilogue-style restore of RBX
   mov    -0xc8(%rbp),%r12     <-- ... and R12
```
and the ~40 instructions before the call show a `shl %cl,%eax` / `shr
%cl,%eax` / `or %eax,%ecx` sequence storing into an array with the standard
bounds-check idiom (`mov r10d,[rax+0Ch]; cmp ecx,r10d; jae -> fail-stub`) —
**not** the `imul` multiply-accumulate shape `mulsub`'s compiled body uses.

**This is decisive and, cross-checked carefully against static disassembly of
all three candidate methods, refutes the mulsub hypothesis this doc's last
four entries were built on:**

1. **Not `mulsub`.** `mulsub`'s own compiled prologue (independently
   re-verified this session via a fresh `CRATONVM_DBG_JIT_DISASM=mulsub`
   capture, full manual trace) saves/restores exactly `r12,r13,r14,r15` at
   fixed offsets `[rbp-0xC8]/[rbp-0xD0]/[rbp-0xD8]/[rbp-0xE0]` — **no `rbx`
   at all**, and the offsets don't match the live capture's
   `rbx@[rbp-0xC0]`/`r12@[rbp-0xC8]` pair. `mulsub`'s inner loop is a
   multiply-accumulate (`imul`), not a shift (`shl`/`shr`). Separately: at
   `mulsub`'s two `a[j]`/`q[offset]` bounds checks, the register holding the
   about-to-fail index (`rcx`, loaded from `r14`=j or `r15`=offset
   respectively, immediately before the compare) would have to equal
   whichever of `r14`/`r15` it was sourced from at the moment of failure —
   but the live capture's `rcx=2` matches **neither** `r14=-261202831` nor is
   there a consistent story for `r15=2` feeding `mulsub`'s own `j`/`offset`
   roles once mulsub's *other* register reuse (`r13`→`product`,
   `r12`→`difference`, per this session's full re-trace of `mulsub`'s
   compiled body) is accounted for. Net: **`mulsub`'s compiled body, called
   with whatever it is actually called with, is internally correct** — this
   session traced every instruction of its 1029-byte compiled body against
   the real JDK25 source and found no defect, extending (not just repeating)
   the "not mulsub itself" conclusion three prior entries already reached
   from the argument-marshaling angle — this entry adds the missing "and the
   live crash doesn't even land in mulsub's frame shape at all" confirmation
   those entries never had a live capture to make.
2. **Not the 3-arg `primitiveLeftShift(I[II)V`.** Also fully re-verified this
   session (fresh `CRATONVM_DBG_JIT_DISASM` capture, complete instruction
   trace against real JDK25 source): its compiled body is a pure
   frame-slot/spill implementation with **no `r12`-`r15`/`rbx` callee-saved
   register use at all** (confirmed: no `mov [rbp-N],r12`-style prologue
   save anywhere in its 1593-byte body) — structurally incompatible with the
   live capture's `rbx`/`r12` epilogue-restore pattern. Its own tail-write
   bounds check (`result[resFrom + m]`, `m = intLen - 1` computed and stored
   to a dedicated frame slot, never re-derived) is **provably safe for any
   consistent `intLen`**: with `resFrom=1` (the only call site,
   `this.primitiveLeftShift(shift, remarr, 1)`), the tail index is always
   exactly `intLen`, the last valid slot of a length-`(intLen+1)` array. This
   method cannot overflow on its own; a defect here would require the same
   kind of external-inconsistency argument as below, but the register-shape
   evidence points away from this method entirely.
3. **Points at `divideMagnitude` itself.** The live capture's `rbx`+`r12`
   (and by extension `r13`-`r15`) callee-saved restore pattern matches a
   *large*, register-pressured method that caches many locals across an
   extended body — exactly `divideMagnitude`'s own profile (23607-byte
   compiled body, confirmed this session to use `rbx` as a spilled/cached
   local via its own safepoint-preservation blocks, `grep rbx` on the
   32-line-frame-save pattern that recurs ~10 times through the body). The
   preceding `shl`/`shr`/`or`-into-array-store shape matches
   `divideMagnitude`'s own **hand-inlined** D1-normalize shift-with-carry
   loop (source, `MutableBigInteger.java:1576-1586`, the `shift > 0 &&
   numberOfLeadingZeros(value[offset]) < shift` branch — this doc's
   `fast, minimal, Hibernate-free repro` entry's finding #2 already
   established the bug requires `shift > 0`, and finding #4 already flagged
   "the else branch (a hand-inlined shift-with-carry loop directly in
   `divideMagnitude`'s own bytecode)" as one of two candidates for the
   unconditional `div.primitiveLeftShift(shift, divisor, 0)`/rem-shift step
   — this session's live evidence points specifically at *this* candidate,
   not the other).

### Why this specific site can overflow: a source-level `intLen`-consistency argument

Read the real JDK25 `divideMagnitude` source in full this session
(`jdk25/lib/src.zip`). The D1-normalize block has two shift-branches, both
gated on `Integer.numberOfLeadingZeros(value[offset]) >= shift` (`value`/
`offset`/`intLen` here are `divideMagnitude`'s own receiver — the
*dividend*, not the divisor `div`):
```java
if (numberOfLeadingZeros(value[offset]) >= shift) {          // branch A
    int[] remarr = new int[intLen + 1];
    ...
    this.primitiveLeftShift(shift, remarr, 1);                // tail index = intLen (safe, see above)
} else {                                                       // branch B
    int[] remarr = new int[intLen + 2];
    ...
    for (int i=1; i < intLen+1; i++, rFrom++) {
        ...
        remarr[i] = (b << shift) | (c >>> n2);
    }
    remarr[intLen+1] = c << shift;                              // tail index = intLen+1 (safe, for the SAME intLen)
}
```
**Both branches are mathematically overflow-proof for any single, consistent
value of `intLen`** — branch A's tail index (`intLen`) is always the last
valid slot of a length-`(intLen+1)` array; branch B's tail index
(`intLen+1`) is always the last valid slot of a length-`(intLen+2)` array.
The *only* way either branch produces a genuine out-of-bounds write is if
the JIT-compiled code uses **two different values** for what should be one
`this.intLen` read — one for the array-allocation size, a different
(larger) one for the tail-write index — despite nothing in the source
mutating `intLen` in between. Working the observed `index=2, length=2`
backward against branch B's formulas: `length=intLen_alloc+2=2 ⇒
intLen_alloc=0` is impossible (a zero-length dividend never reaches this
code — `smallToString`'s own `while (tmp.signum != 0)` guard rules it out).
Against branch A via a `this.primitiveLeftShift(shift, remarr, 1)` call:
`length=intLen_alloc+1=2 ⇒ intLen_alloc=1`, and the *callee's own*
internally-correct tail index would then be exactly `intLen=1` (not 2) —
also does not reproduce a length-2/index-2 crash on a single consistent
value. **Every self-consistent scenario is safe; only a genuine
allocation-time-vs-later-use inconsistency in `this.intLen`'s cached value
reproduces this exact `index=2,length=2` signature.** This is a strong,
source-derived argument that the defect is a JIT register/spill-slot
**re-read inconsistency** for the dividend's `intLen` field within
`divideMagnitude`'s own compiled body (most plausibly: `intLen` gets cached
in a register or spill slot early in the branch for the allocation, and the
D1-normalize loop's own locals — `i`, `b`, `c`, `rFrom` — alias or clobber
that same slot before the post-loop tail statement re-reads it, given this
is the same general "aggressive same-slot register reuse within one method"
pattern this session independently re-confirmed is real and commonplace in
this JIT's output while tracing `mulsub`'s own compiled body (there, `r12`/
`r13`/`r14`/`r15` each get reused for 2-3 different logical values across
one method invocation — safe there only because each reuse strictly
postdates that register's prior value's last use; the hypothesis here is
that `divideMagnitude`'s D1-normalize section has one such reuse that is
**not** safe).

**Not fixed this session.** The exact clobbering instruction/slot was not
pinned before time ran out — this session's own attempts to get a
byte-exact correlation between the live crash's return address and a
statically-captured `CRATONVM_DBG_JIT_DISASM=divideMagnitude` dump did not
converge: `divideMagnitude` is large and register-pressured enough that its
compiled layout appears to vary between separate compilations (different
frame offsets observed between a `SmallDividendRepro`-driven compile and the
live full-harness compile), so a static dump from one process cannot be
byte-matched against a live capture from a different process — the
correlation has to happen **within the same live process**. Per this doc's
own standing guidance, declined to guess at or speculatively patch anything
in `divideMagnitude`'s D1-normalize codegen without that confirmation —
this is exactly the kind of shared, complex codegen path where a wrong fix
would be worse than no fix.

**Next step for a follow-up session — now a narrow, mechanical task, not an
open-ended search:**
1. Reuse this session's exact recipe (`gdb -batch`, `break
   cratonvm_vm::jit::helpers::jit_throw_aioobe`, full harness, default
   settings, no stress flags — fires in 4-6 minutes on an idle host, faster
   on a loaded one per the immediately-preceding entry's ~9-11 minute
   figures) to get a fresh hit.
2. **In the same live process** (do not `quit` after the first hit —
   `continue` is already wired in this session's script), before killing
   gdb, run `x/300i <jit_return_pc>-2200` (or similar; `divideMagnitude`'s
   compiled body is large, the D1-normalize section is within the first
   ~2KB per this session's `SmallDividendRepro`-driven static capture) and
   manually walk backward from the tail-write bounds check to find: (a) the
   nearest preceding `new int[]`/array-allocation call sequence (the
   `578FA0FE2540h`-style helper address pattern this session identified, or
   its equivalent in that process — resolve via `x/i` on the loaded
   immediate, not by assuming the address is stable across launches), and
   (b) which register/frame-slot feeds the SIZE argument to that
   allocation vs. which register/frame-slot feeds the tail-write's index —
   if they are the same frame slot (`[rbp-N]`, reloaded fresh both times),
   the bug is not a stale-register-cache issue and this hypothesis is
   refuted; if the allocation reads a frame slot but the tail-write reads a
   *register* that was last written by the loop body (not by a fresh
   reload of that same frame slot), that pins the defect precisely.
3. Once pinned, the fix is almost certainly local to whichever code path in
   `jit/src/lib.rs`/`jit/src/x64.rs` decides when a cached-local register
   value can be reused across a loop body vs. when it must be reloaded from
   the frame slot (an invalidation-scope bug, not a new mechanism) — but per
   this doc's standing guidance, get the live confirmation from step 2
   before writing that fix.
4. If step 2 instead shows the two reads *do* consistently use the same
   slot (refuting this entry's hypothesis), the next candidate per the
   source-level argument above is that `intLen` itself is being *read* from
   the wrong object (e.g., `div.intLen` vs `this.intLen` swapped at one of
   the two call sites, a receiver-confusion bug rather than a register-cache
   bug) — check the receiver (`rsi`/`rdi` depending on ctx) feeding each of
   the two `intLen`-reading getfield sites, not just the value.

Probe/diagnostic artifacts from this session: gdb scripts and captured logs
at `/data/data/tmp/gdb_aioobe_capture*.gdb` / `*.log` on the shared host
(6 iterations, `capture6` is the cleanest/most complete — full register
dump + frame-6 resolution + disassembly window in one pass), plus the
full manually-annotated `mulsub`/`primitiveLeftShift(I[II)V`/
`divideMagnitude` static disassembly captures at `/data/data/tmp/disasm3.log`,
`/data/data/tmp/mulsub_full.txt`, `/data/data/tmp/pls3arg.txt`,
`/data/data/tmp/divideMagnitude_full.txt` (all against
`dev@39f89987`/`CRATONVM_TIER_C1_THRESHOLD=50`/`SmallDividendRepro`, kept for
reuse though note the frame-offset caveat above before assuming byte-exact
correlation with a fresh live capture).

**This item remains OPEN.** No code change made this session. The mulsub
argument-marshaling hypothesis this doc's last four entries were built
around is now REFUTED with live-capture-grade evidence (not just
non-reproduction) for the first time in this investigation's history, and
the search space is narrowed from "somewhere in the JIT/GC interaction
across three candidate methods" to "a specific, mechanically-checkable
register/slot-reuse question within one method's D1-normalize section,
confirmable by one more live gdb session on a quiet host." The rest of this
doc's items are independently closed; this one still blocks declaring
`hib-misc-residuals-20260716.md` fully closed.

## Update 2026-07-17 (same-process double-capture session): still could NOT reproduce despite an exhaustive battery (0/23 harness-level executions, ~200k+ isolated trials) on a fresh, verified `dev` tip — but static source-reading RULES OUT the two register-caching mechanisms the immediately-prior entry's next-steps assumed, and corrects that entry's own "rbx/r12 restore" evidence to a routine method epilogue, not a diagnostic signal

Picked up this doc's own next step (the same-process double-capture: catch
`divideMagnitude`'s array-allocation point and the later AIOOBE failure
point from the SAME live compiled instance, in the SAME process, rather
than correlating a live crash against a separately-compiled static dump).

**Setup.** Fresh isolated worktree (`wt-hib-biginteger-finalfix-20260717`,
`CARGO_TARGET_DIR=cv-target-hib-biginteger-finalfix-20260717`,
`CARGO_PROFILE_RELEASE_LTO=off`) built from `origin/dev@08c50579` (current
tip, re-fetched and confirmed both before and after this session — no
concurrent work landed on this item). Frozen + md5-verified at
`/data/data/frozen-hib-biginteger-finalfix-20260717/cratonvm-biginteger-finalfix-20260717`.
Host was idle at session start (load 0.02-0.4) and stayed calm/moderate
throughout (peaks around 3.0 from this session's own parallel attempts, never
externally contended).

**Same-process double-capture technique used (the actual improvement over
the prior entry's single-shot register dump):** rather than relying on a
separately-compiled static `CRATONVM_DBG_JIT_DISASM` dump from a different
process (the prior entry's own stated blocker — `divideMagnitude`'s compiled
layout varies between separate compiles), this session ran the real harness
invocation with **both** `CRATONVM_DBG_JIT_DISASM=divideMagnitude,mulsub`
**and** a `gdb -batch` wrapper breaking on
`cratonvm_vm::jit::helpers::jit_throw_aioobe` **in the same process
launch**. Since `CRATONVM_DBG_JIT_DISASM` prints its dump (with the
compiled method's real `entry=0x...` address and per-instruction hex
offsets) to the process's own stdout *before* any later crash in that same
process, and the live gdb capture's `frame 6` return address comes from
that identical compiled instance, subtracting `entry` from the live return
PC gives an exact, byte-correct offset into the SAME dump — no cross-process
correlation assumption required. (Script:
`/data/data/tmp/gdb_aioobe_samecapture.gdb`, left on the shared host;
adds `handle SIGUSR2 nostop noprint pass`/`handle SIGUSR1 nostop noprint
pass` to the prior sessions' scripts — CratonVM uses `SIGUSR2` for its
internal STW/JIT-takeover signaling, and `gdb -batch` without that handler
stops on the first one and never resumes, which silently killed this
session's very first attempt before the fix.)

**Could not obtain a single live hit this session, despite a much larger and
more varied battery than any single prior session:**
- 8 full-class `CratonRunner`/`selectClass` end-to-end harness runs (the
  exact invocation the doc's harness uses) under the gdb wrapper with JIT
  disasm enabled — 3 sequential/aborted (1 lost to the `SIGUSR2` issue
  above before the fix), then 3 in parallel, then 3 more single sequential
  runs. **8/8 completed `found=132 ok=132 failed=0`** (`ms` range
  589022-697688), zero `AIOOBE HIT` breakpoint fires.
- 15 fast 2-method-interleaved `MultiMethodRunner` runs (`entityPersister` +
  `createSchema_fromSessionFactory:org.hibernate.testing.orm.junit.DomainModelScope`,
  24 executions/run, ~117-140s each — the exact combination the
  `longRadix`/`digitsPerLong`-fix entry reported "still reproduces on the
  fixed binary"). **15/15 clean**, `found=24 ok=24 failed=0` every time,
  zero AIOOBE.
- `SmallDividendRepro` (the sub-second, Hibernate-free repro reported
  "85-90% failure rate" / "fires reliably on the first stressed run"
  several entries ago) at `CRATONVM_DBG_GC_STRESS` ∈ {4096, 16384, 65536,
  2097152}, 20,000 trials each (80,000 total), plus `CRATONVM_TIER_C1_THRESHOLD=5`
  with and without stress (20,000 + 100,000 more trials): **0/220,000
  `@@BAD`, 0 `AIOOBE3-DIAG`, 0 `ArrayIndexOutOfBoundsException`.**
- `FixedValRepro` (the "hard state transition" determinism probe), 4×3000
  iterations: **0/12,000 bad.**

Total this session: **0/23 real harness-level executions** and **well over
230,000 isolated-repro trials**, all clean, on a binary built from the
current tip with no code differences from the binary the immediately-prior
(live-gdb-capture) session used. Per this doc's own long-standing "3+
sessions have gotten 0/N clean while others got a reliable ~50%, same code,
same day" pattern, this is consistent with — not a refutation of — the bug
still being real; it is simply this session's data point on the "clean"
side of the doc's own documented bimodal split. (One operational note for
future sessions: running **9** parallel `cratonvm`+`gdb` processes at once
OOM-killed 3 of them — each live Hibernate-harness process under this
recipe uses 6-8GB RSS; **3 concurrent is a safe ceiling** on this host's
32GB, not the 6-9 this session initially tried.)

**No live capture means no register/slot correlation was obtained this
session either** — the same-process double-capture technique above is
ready and confirmed mechanically sound (verified end-to-end against the
non-crashing runs: the JIT disasm dump for `divideMagnitude` prints
correctly, `entry=`/offsets parse as expected, the gdb breakpoint fires
correctly on non-AIOOBE `SIGUSR2`/`SIGUSR1` STW signals without stopping
the run), it simply never got a real hit to apply it to.

**However, static source-reading this session materially narrows (and in
one case corrects) the mechanism hypothesized by the immediately-prior
entries, using the exact bytecode ground truth and the JIT's own register/
spill-slot allocation code — this is real, load-bearing progress even
without a live capture:**

1. **The graph-coloring JVM-local register allocator
   (`jit/src/regalloc.rs`) is OFF by default for `divideMagnitude`,
   contradicting the framing several recent entries built on.**
   `jit/src/x64.rs`'s `callee_saved_gpr_local_homes_enabled()` (~line 2680)
   defaults to `false` (opt-in only via
   `CRATONVM_JIT_ENABLE_CALLEE_SAVED_GPR_LOCALS=1`); the only other path to
   register-mapped locals, `kernel_reg_locals_enabled()`'s "pure kernel"
   fast path (~line 2696), is structurally gated to methods with **no
   invokes, no field ops, no allocation** — `divideMagnitude` has all
   three (10+ invokes, `getfield`/`putfield` on `intLen`/`value`/`offset`,
   `newarray`), so it can never qualify. Net: `local_assignments` (per
   `jit/src/x64.rs:8386`) is `vec![None; ...]` for this method under
   default settings — **every JVM local, including `this` (local 0), is
   frame-slot-only**, never register-resident. This rules out "a JVM
   bytecode local's register home gets reused for a different local" as
   the mechanism, which is what the prior two entries' "next step" #2/#3
   were implicitly set up to check.
2. **`getfield` never caches across bytecode instructions.** Read the
   `0xb4` (`getfield`) codegen arm (`jit/src/x64.rs:20501` onward, both the
   compact-layout and legacy-layout paths): every occurrence always issues
   a fresh `MOV` from the receiver's field cell — there is no
   redundant-load-elimination/CSE pass for field reads anywhere in this
   JIT (confirmed by grep: no `field_cache`/`cached_getfield`/
   `redundant_load`-shaped code exists in `jit/src/x64.rs`).
3. **Got the exact bytecode ground truth via `javap -c -p` against the
   real `jdk25` `MutableBigInteger.class`** (not just the previously-quoted
   JDK source, the actual compiled bytecode `divideMagnitude` executes):
   branch B (the `shift > 0`, `else` D1-normalize path this doc's history
   already localized to) contains **four textually distinct
   `aload_0; getfield #21 (intLen)` sites** — bytecode pc 106 (`remarr`
   allocation size, `intLen+2`), pc 129 (`rem.intLen` bookkeeping,
   unrelated to the crash), pc 164 (the loop condition `i < intLen+1`,
   **re-evaluated fresh every iteration** — not hoisted), and pc 213 (the
   tail-write index `intLen+1`). javac itself does not cache `intLen` into
   a synthetic local anywhere in this method either. Combined with finding
   2, there is no code path — JIT-level or javac-level — by which pc106's
   and pc213's reads of `this.intLen` could observe different cached
   values while agreeing on the same live object: each is an independent,
   freshly-issued memory load of the same immutable-in-this-scope field.
4. **The always-on-by-default `ALL_SPILL_GPRS` safepoint register spill
   (`emit_pre_safepoint_spill`, `jit/src/x64.rs:9447`) is write-only and
   therefore cannot itself be the corrupting mechanism.** Confirmed
   `precise_maps` (default-on) implies `safepoint_reg_spill_all` via
   `precise_implies_reg_spill` (`jit/src/x64.rs:8275-8277`), so by default
   this method's compiled body DOES blind-spill all 14 `ALL_SPILL_GPRS`
   into a reserved frame region before every GC-capable call (matching the
   "recurring ~10 times" pattern a prior entry observed) — but grepping the
   whole crate for `reg_spill_base` shows it is **written in exactly one
   place and never read back anywhere**. This matches its own design
   comment ("fully conservative... can only over-retain, never corrupt")
   and rules it out as a source of a wrong VALUE reaching later code.
5. **Correction: the "rbx/r12 restore right after the `jit_throw_aioobe`
   call" observation from the live-gdb-capture entry is the method's
   routine epilogue, not evidence about which local was clobbered.**
   `jit_throw_aioobe` (`vm/src/jit/helpers.rs:3997`) returns normally
   (the `i64::MIN` deopt sentinel) rather than unwinding — its own
   doc-comment states the JIT-compiled caller "immediately runs the method
   epilogue" after the call returns. A standard epilogue unconditionally
   restores every callee-saved register the method's prologue saved,
   regardless of whether that register held anything related to
   `intLen`/the D1-normalize loop specifically — so the prior entry's
   inference ("matches a large, register-pressured method that caches many
   locals... exactly `divideMagnitude`'s own profile") is not wrong that
   the method uses those registers *somewhere*, but the instructions
   observed immediately after the call are generic epilogue boilerplate,
   not a targeted restore of the specific value that overflowed. This
   should not be leaned on for register/slot attribution in a future
   session; the earlier decisive evidence in that same entry (the
   pre-call `shl`/`shr`/`or`-into-array-store shape matching the
   D1-normalize hand-inlined loop, as opposed to `mulsub`'s `imul`
   multiply-accumulate shape) is unaffected by this correction and still
   stands as the strongest localization to `divideMagnitude`'s own
   compiled body.

**Net effect on the search space:** with (1) and (2) ruling out both
register-local-caching and getfield-caching as possible mechanisms, and
(4)/(5) removing two pieces of evidence that had been (mis)read as
supporting a local-caching theory, the remaining plausible mechanisms for
"two reads of the same immutable field observe different values within one
compiled invocation" are narrower than any prior entry stated:
  - **(i) An arithmetic/index-computation bug in the `iastore` codegen
    itself**, independent of `intLen` consistency — e.g. a bad
    sign-extension or off-by-one in how `intLen + 1` gets materialized into
    the index register at the bounds check, present on every single-consistent-read
    execution but rare enough to explain the observed low overall trigger
    rate combined with the "hard state transition once JIT-compiled" shape
    from an earlier entry.
  - **(ii) An operand-stack max-depth / frame-region-sizing bug.** JVM
    locals get fixed, unique, non-overlapping frame offsets
    (`local_offset(idx) = (idx+1)*8`, `jit/src/x64.rs:8752`) — not
    investigated this session is whether the OPERAND-STACK spill region
    (sized from `max_stack`, `jit/src/x64.rs` prologue-size computation
    around line 8360) is computed correctly for this specific method's
    actual maximum simulated-stack depth; an under-count here would be the
    one way a *transient* expression value (like the `intLen+2` about to be
    passed to `newarray`, or `intLen+1` about to feed an `iastore` index)
    could alias a JVM local's frame slot without either "local caching" or
    "getfield caching" being involved.
Neither (i) nor (ii) is confirmed — both are concrete, narrower,
next-session-actionable leads than the register/getfield-caching framing
this session ruled out.

**Not fixed this session — declined to guess.** Per this doc's own
standing guidance and given (1)-(5) above rule out the specific mechanisms
the last several entries were narrowing toward, without a live capture to
confirm (i) or (ii) there is nothing safe to patch; a wrong fix to shared
`iastore`/bounds-check or operand-stack-sizing codegen used by every array
store in the JIT would be far worse than no fix.

**Next step for a follow-up session:**
1. Reuse `/data/data/tmp/gdb_aioobe_samecapture.gdb` (this session's
   same-process double-capture script, mechanically verified working —
   remember the `SIGUSR2`/`SIGUSR1` `handle ... nostop noprint pass` lines
   are required or the run silently dies on the first internal STW signal)
   with `CRATONVM_DBG_JIT_DISASM=divideMagnitude,mulsub` set, against the
   plain full-harness invocation (no stress flags — every recipe this
   session tried with or without stress was equally unproductive, so
   there's no evidence stress flags help and the doc's own history shows
   the plain full-harness run has been the more reliable trigger in
   several other sessions). Budget for **multiple session-lengths**, not
   one sitting — this session's 0/23 result is itself informative but, per
   the doc's own standing conclusion, not evidence of a fix.
2. When (not if, per the doc's history) a hit lands: parse the
   `CRATONVM_DBG_JIT_DISASM` dump printed earlier in the SAME process's
   own stdout for `divideMagnitude`'s `entry=` address, subtract it from
   the live `frame 6` return PC to get the exact byte offset, and locate
   that offset in the dump. Confirm points 4-5 above hold for that live
   instance (no `reg_spill_base` reload appears; the post-call
   instructions really are the generic epilogue), then walk backward to
   the `iastore` bounds-check and the `newarray` call immediately preceding
   it in program order, and read off which frame slot/register feeds the
   SIZE argument vs. which feeds the INDEX — per finding (2), both should
   be independent fresh loads, so pin down whether they actually agree
   (pointing at hypothesis (i), an arithmetic bug) or disagree despite both
   reading `[rbp-samesameoffset]` (which would only be explained by
   something OTHER than a register/local cache — e.g. hypothesis (ii), a
   slot the operand stack and the locals both believe they own).
3. If a live capture is obtained and (i)/(ii) both come up clean, fall back
   to the `fast, minimal, Hibernate-free repro` entry's own step 4 (audit
   `primitiveLeftShift`'s inlined tail-store bounds specifically for the
   3-argument overload, which that entry noted stays interpreted under the
   `SmallDividendRepro` shape and was not re-checked under a real
   `Hibernate` multi-method compile — this session did not check whether
   it compiles under the full-harness shape either).

**This item remains OPEN.** No code change made this session. The rest of
this doc's items are independently closed; this one still blocks declaring
`hib-misc-residuals-20260716.md` fully closed.

## Update 2026-07-17 (lead1/lead2 static-audit + concurrent-fix cross-reference session): both assigned leads RULED OUT via exhaustive static audit; found a HIGH-CONFIDENCE candidate root cause/fix already landed on `dev` by a concurrent, differently-scoped session (`dbba7c93`/`b34e09cd`, "select exact oop map at active safepoint") whose mechanism matches every piece of this saga's evidence — NOT empirically confirmed via a live before/after reproduction this session (bimodal luck again), so NOT closing outright, but this is the strongest lead the whole investigation has produced

Picked up this doc's own two flagged leads from the entry above: (i) an
`iastore` index-computation arithmetic bug, and (ii) an operand-stack
max-depth/frame-sizing bug. Setup: fresh `origin/dev@fcefa8ca` build,
worktree `wt-hib-biginteger-lead3-20260717`, `CARGO_PROFILE_RELEASE_LTO=off`,
frozen + md5-verified at
`/data/data/frozen-hib-biginteger-lead3-20260717/cratonvm-biginteger-lead3-20260717`
(md5 `c4362967433ce9a308c9695cdcaa6390`).

**Lead 1 (`iastore` index arithmetic) — RULED OUT.** Read the actual
`iastore`/bounds-check codegen (`jit/src/x64.rs`): the array length is
loaded fresh from the object header (`MOV R10D, [RAX+ARRAY_LENGTH_OFFSET]`)
at the bounds-check site itself, never cached from an earlier point — the
JIT's bounds check is provably comparing the CURRENT header's length against
whatever index reached it. `iastore`'s three-value pop sequence
(`val_slot`/`index_slot`/`array_slot` popped in that order from the
simulated LIFO stack) matches JVMS `iastore` semantics exactly. Extended
the prior "same-process double-capture" entry's argument-marshaling audit
(which only covered `mulsub`'s 6-arg call) to `primitiveLeftShift(I[II)V`'s
4-arg invokevirtual call sites in `divideMagnitude` (`emit_stack_arg_setup`,
`jit/src/x64.rs:11130`): with `has_ctx` consuming one `ARG_REGS` slot, all
4 Java args (receiver+3 params) fit entirely within the remaining register
slots — `stack_arg_count == 0`, no stack-arg spillover, no
`SCRATCH_REGS`/`ARG_REGS` aliasing risk (the R8/R9 double-duty hazard the
`CRATONVM_DBG_AIOOBE3` entry flagged for `mulsub`'s 6-arg shape structurally
cannot arise for a 4-arg call). No arithmetic or index-computation defect
found in this path.

**Lead 2 (operand-stack/frame sizing) — RULED OUT, plus one genuinely new
angle closed.** Confirmed `divideMagnitude`'s real `max_stack=7` (from
`javap -v`, matching the class file's own verified value) flows through
`jit/src/lib.rs`'s `set_pending_verified_max_stack` into `x64.rs`'s
`max(verified_max_stack, estimated_max_stack) + max_invoke_args +
inline_stack_reserve` — a deliberately over-provisioned reservation scheme
(the `inline_stack_reserve` term's own doc comment records it was added
after a prior real bug of exactly this shape, "observed as a
`ClassCastException`... when a clobbered slot fed an enum-typed field", i.e.
this exact failure family already has one fixed precedent in this
codebase). **New check, not covered by any prior session on this bug:**
verified `primitiveLeftShift(I[II)V`'s real bytecode length (90 bytes, from
the `javap` dump) exceeds `MAX_INLINE_BYTECODE_SIZE=35`
(`jit/src/lib.rs:2474`), so it can **never** be inlined into
`divideMagnitude` — this closes out an inlined-frame-slot-collision
hypothesis (the callee's own locals aliasing the caller's spill region via
the `inline_sites`/`callee_local_base` mechanism) before it could even be
seriously proposed. No frame-sizing defect found.

**Clarifying, non-mechanism-changing finding:** got the real JDK25
`BigInteger.smallToString` bytecode via `javap -c -p -v` for the first time
in this investigation's history (prior sessions worked from the JDK25
*source*, never the compiled `Code`/`LineNumberTable`). The reported crash
frame `at java.math.BigInteger.smallToString(BigInteger.java:4170)` is,
per the real `LineNumberTable`, the bytecode range **containing the
`a.divide(b, q)` call itself** (`aload 10; aload 11; aload 9; invokevirtual
divide; astore 12`) — not a direct array access. `smallToString`'s own
`digitGroups` array is a `long[]` (`newarray long`, JDK25 refactored this
method to use `long[]`+`StringBuilder` instead of the older `String[]`
shape), so it cannot be the `elem_ty=10` (`ArrayElementType::Int`) array
`CRATONVM_DBG_AIOOBE3` captures — there is no `int[]` anywhere in
`smallToString`'s own bytecode. This confirms (does not newly establish)
that the crash's Java-level stack trace is a routine consequence of the
exception propagating up through `MutableBigInteger.divide`/`divideKnuth`/
`divideMagnitude` frames to `smallToString`'s call site line, fully
consistent with every prior session's localization to `divideMagnitude`'s
own compiled body.

### Reproduction battery this session — clean on both a pre-fix and a post-fix binary (see below), consistent with this doc's own documented bimodal/heisenbug pattern

Neither lead's static ruling-out produced a concrete defect, so pursued
live reproduction per this doc's standing plan. **Pre-fix binary**
(`cratonvm-biginteger-lead3-20260717`, `dev@fcefa8ca`): 6 clean full-harness
`CratonRunner`/`selectClass` runs (`found=132 ok=132 failed=0`, both
`gdb`-wrapped-with-`CRATONVM_DBG_JIT_DISASM` and plain, `ms` range
707061-1065418) + ~194,000 clean `SmallDividendRepro` trials across
`CRATONVM_DBG_GC_STRESS` ∈ {4096, 65536} and `CRATONVM_TIER_C1_THRESHOLD=50`.
2 additional full-harness launches (one plain, one
`CRATONVM_DBG_GC_STRESS=65536`) were lost mid-run with no `@@RESULT` and no
crash/exception in the log — likely host memory contention (this session's
host load swung 7-94 and `free -m` briefly dropped under 500MB available
during the busiest window; `dmesg` had no OOM entries in its retained
scrollback either way) rather than anything attributable to the code;
treated as inconclusive, not clean-or-crashed data points.

### The actual finding this session adds: a concurrently-landed, differently-motivated fix on `dev` whose mechanism is an exact match for this bug's entire evidence trail

A routine `git fetch origin dev` partway through this session picked up
`dbba7c93`/`b34e09cd` (`fix(jit,gc): select exact oop map at active
safepoint`, merged `f2021f99`..`0bd8f8be` range), landed by a **different,
concurrently-running session** whose own scope (per its commit message and
diff) was never stated as being about this BigInteger bug at all. Reading
its full diff (`jit/src/lib.rs`, `vm/src/jit/conservative_roots.rs`) finds
a real, previously-unidentified-by-any-prior-session-on-this-item defect
in the precise GC root scanner, and the mechanism is a striking match:

**The bug the fix closes:** `scan_one_frame_precise`'s old implementation,
when a GC-capable safepoint fires, identified only ONE "boundary" compiled
method (`info`/`cm`) and scanned the **union of every oop map that method
has** (its own comment: "Enumerate EVERY oop map the method has... this is
conservative-within-the-map... false negatives impossible given the
union-of-all-maps"). That reasoning is correct **only** for a safepoint
inside that one method's own frame — it has **no logic at all** for walking
up the native call stack to also scan a JIT **caller's** frame when the
active safepoint is inside a nested JIT-to-JIT callee. The fix's own commit
message states this plainly: "the old implementation scanned the union of
every map in only the boundary method; besides retaining dead oops, it
**omitted maps belonging to nested JIT callers entirely**."

**Why this applies exactly to `divideMagnitude`'s calls into
`primitiveLeftShift`/`mulsub`.** Both are private methods CratonVM's JIT
resolves via the direct-call fast path (confirmed by this doc's own prior
"mulsub argument-marshaling" entries), which — verified this session,
`jit/src/x64.rs:24542` — emits `self.emit_pre_safepoint_spill()`
**immediately before** `self.emit_call_absolute(callee_entry)`.
`emit_pre_safepoint_spill` is exactly the function (`jit/src/x64.rs:~9500`)
that writes the caller's current bytecode PC into `divideMagnitude`'s own
reserved `sp_id_slot_off` frame slot — i.e. `divideMagnitude`'s frame
**does** get a correctly-populated safepoint-id at each of these two call
sites, but the OLD scanner never consulted it for anything other than the
single "boundary" frame the walker happened to identify. If a GC (including
this codebase's background/concurrent STW takeover, which per this doc's
own `gc/`-family memory can interrupt a running thread mid-instruction via
signal, not only at an allocating bytecode) strikes while native execution
is suspended **inside** `primitiveLeftShift`'s or `mulsub`'s own compiled
frame (a nested JIT callee, itself allocation-free but still a full
safepoint per `invokevirtual`'s "every dispatch is a safepoint" contract),
the old scanner would identify the *innermost* frame as the boundary and
never walk up one level to scan `divideMagnitude`'s own `divisor`/
`remarr`/`rem`/`this` (dividend) references — a genuine missed root.

**Why this precisely reproduces every symptom this 8-session investigation
recorded**, in one mechanism, for the first time:
- **"Self-consistent, validly-allocated `int[2]`, `forwarding_ptr=0x0`"**
  (the `CRATONVM_DBG_AIOOBE3` entry's decisive capture) — exactly the
  signature of reading through a dangling reference into memory that was
  freed (because nothing marked it live) and **reused** for a new,
  unrelated, but validly-shaped allocation, not a relocated/stale pointer.
  This is the one hypothesis that entry's own reasoning left open
  ("favors an out-of-bounds write... or a genuine arithmetic bug") without
  identifying a missed-root explanation as a third option — because no
  prior session had read this specific scanner code path.
- **"GC-amplified"** (`CRATONVM_DBG_GC_STRESS` turning a silent
  wrong-result into a deterministic crash) — a missed-root bug is, by
  construction, only observable when a GC actually runs at the exact
  vulnerable window; raising GC frequency directly raises the odds of
  landing in that window.
- **"Hard state transition, correct before the relevant methods finish
  compiling, then consistently wrong after"** (an earlier entry's
  `FixedValRepro` finding) — this scanner-selection bug is dormant under
  `--nojit` (no compiled frames to mis-scan) and, once `divideMagnitude`/
  `primitiveLeftShift`/`mulsub` are JIT-compiled with precise maps active,
  every subsequent GC that happens to strike mid-callee is vulnerable —
  matching the observed determinism-after-compile shape.
- **"Not `mulsub`'s or `primitiveLeftShift`'s own compiled body"** (three
  separate sessions' full manual disassembly traces, all clean) — correct:
  the defect is not in either callee's codegen at all, it is in the GC's
  own root-scanning of the **caller's** frame while a callee is active.
- **Not a stale-register-cache or getfield-caching issue** (the
  "same-process double-capture" entry's findings 1-2) — correct and
  unaffected by this finding; those really were clean, the defect lives one
  layer below JIT codegen, in the GC integration around it.

**This fix was not authored by this session** — it landed from a
concurrently-running, differently-scoped investigation
(oop-map-selection-at-safepoint bug), and its commit message makes no
reference to `BigInteger`, `divideMagnitude`, or this doc. This session is,
as far as the git history and this doc's own record show, the first to
connect the two.

**Verification performed this session (inconclusive on causation, clean on
regression):** built `origin/dev@0bd8f8be` (includes both `dbba7c93` and
`b34e09cd`) in worktree `wt-hib-biginteger-oopmapfix-20260717`, frozen +
md5-verified at
`/data/data/frozen-hib-biginteger-oopmapfix-20260717/cratonvm-oopmapfix-20260717`
(md5 `e6416207046352100f95fbfafd9b34ae`). 2 clean full-harness
`CratonRunner` runs (`found=132 ok=132 failed=0`, `ms=784499`/`758275`,
zero `AIOOBE3-DIAG`) + 48,000 clean isolated `SmallDividendRepro` trials
(2 more full-harness launches lost mid-run with no result, same
inconclusive host-contention pattern as the pre-fix binary above — **not**
attributable to this fix; see the "host contention" note above). `cargo
test --release -p cratonvm-jit --lib`: **906/906 pass**, no regression from
either landed commit.

**Why this is NOT being called a confirmed fix, despite the strength of the
mechanistic match:** this session's own reproduction attempts landed on
the "clean" polarity on **both** the pre-fix and the post-fix binary — a
real `0/N` on the pre-fix binary specifically means this session never
caught a live "before" crash to A/B against the post-fix binary's clean
runs. Per this doc's own long-standing, repeatedly-reconfirmed conclusion,
absence of failure on the pre-fix binary is not evidence the bug is rare or
fixed — it is simply this session's data point on the "clean" side of the
doc's own documented bimodal split (the same split multiple earlier
entries hit on both sides, sometimes on the very same day). A
mechanistically airtight explanation that also happens to match a
non-reproduction is not the same as an empirically demonstrated fix.

**Recommendation for the next session — now a narrow, mechanical
confirmation task, not an open-ended search:**
1. Reuse the two frozen binaries already on the shared host — pre-fix
   `/data/data/frozen-hib-biginteger-lead3-20260717/cratonvm-biginteger-lead3-20260717`
   (`dev@fcefa8ca`, confirmed predates `dbba7c93`) and post-fix
   `/data/data/frozen-hib-biginteger-oopmapfix-20260717/cratonvm-oopmapfix-20260717`
   (`dev@0bd8f8be`, confirmed includes it) — no rebuild needed.
2. Get ONE live "before" crash on the pre-fix binary using whichever
   recipe this doc's history shows working on a given session's host
   conditions (plain full-harness `CratonRunner` runs have been the more
   reliable trigger in several entries; the `gdb`-wrapped
   `jit_throw_aioobe` breakpoint recipe at
   `/data/data/tmp/gdb_aioobe_samecapture.gdb`/`gdb_lead3_samecapture.gdb`
   remains available and mechanically verified working). If host
   contention is the blocker (as it partly was this session), retry on a
   quieter window or budget for a long unattended loop per the
   "post-`f377eb69`" entries' own advice.
3. Immediately re-run the **identical** recipe against the post-fix binary.
   If the pre-fix binary crashes and the post-fix binary stays clean across
   a comparable trial count, this closes the item for real — move a full
   consolidated writeup (covering the entire mulsub-refutation →
   divideMagnitude-relocalization → missed-root-at-nested-safepoint arc)
   to `docs/internal/fixed-suite-bugs/`, and re-check whether this also
   closes the whole `hib-misc-residuals-20260716.md` doc (it is, per every
   entry in this section, the last blocking item).
4. If the pre-fix binary also stays clean (i.e. this specific session's
   inability to reproduce turns out not to be bad luck but something about
   current host/build conditions generally suppressing the trigger), that
   itself would be worth a dedicated investigation into what changed
   between the sessions that reliably got the ~50% hit rate and now.

No code change made this session (the relevant fix was found already-landed
on `dev`, not authored here) — this is a docs-only update.

## Update 2026-07-17 (A/B confirmation session): the `dbba7c93`/`b34e09cd` oop-map-safepoint fix is **REFUTED** as the fix for this bug — direct before/after comparison on two frozen, md5-verified binaries shows statistically indistinguishable ~50% failure rates on both sides. Still OPEN; root cause remains genuinely unresolved.

Picked up the immediately-prior entry's own recommendation: do a direct,
mechanical A/B using the two frozen binaries it left on the shared host —
no rebuild needed. Verified both binaries' provenance first (worktree
reflog + commit-date cross-check, since these are shared worktrees that can
move under a session): `frozen-hib-biginteger-lead3-20260717/
cratonvm-biginteger-lead3-20260717` (md5 `c4362967433ce9a308c9695cdcaa6390`)
was built from `dev@fcefa8ca` (checked out 10:56 UTC), confirmed **not** an
ancestor of `dbba7c93` (committed 11:19 UTC, i.e. genuinely pre-fix).
`frozen-hib-biginteger-oopmapfix-20260717/cratonvm-oopmapfix-20260717` (md5
`e6416207046352100f95fbfafd9b34ae`) was built from `dev@0bd8f8be` (checked
out 12:09 UTC), confirmed to include both `dbba7c93` and `b34e09cd`
(post-fix). Both md5s matched the prior session's own recorded values
exactly — binaries unmodified.

**Pre-fix binary: got a live crash on the very first attempt.** Ran the
real harness driver (`CratonRunner`/`DiscoverySelectors.selectClass`,
exactly what the suite uses) against `DefaultCatalogAndSchemaTest`,
`CRATONVM_DBG_AIOOBE3=1 -Dcraton.trace=true`, no other stress flags:
```
@@RESULT 0 ...DefaultCatalogAndSchemaTest found=132 started=132 ok=66 failed=66 aborted=0 skipped=0 ms=543442
```
All 66 failures were the exact production signature
(`java.lang.ArrayIndexOutOfBoundsException: Index 2 out of bounds for
length 2` at `BigInteger.smallToString`/`NamingHelper.hashedName`), each
preceded by an `[AIOOBE3-DIAG]` capture matching this saga's established
signature exactly (`array_length_field=2 num_slots=2 forwarding_ptr=0x0`).
A second, independent clean run gave the identical shape:
`found=132 ok=66 failed=66`. (A third parallel attempt hit a genuine,
severe host-memory crunch this session ran into — see the "host contention"
note below — and was OOM-killed mid-run, logged 62 more real
`[AIOOBE3-DIAG]` firings before termination but never reached `@@RESULT`;
not counted as one of the two clean data points above, but consistent with
them.)

**Post-fix binary: immediately re-ran the identical recipe. It crashes at
the same rate.**
```
@@RESULT 0 ...DefaultCatalogAndSchemaTest found=132 started=132 ok=64 failed=68 aborted=0 skipped=0 ms=554731
@@RESULT 0 ...DefaultCatalogAndSchemaTest found=132 started=132 ok=64 failed=68 aborted=0 skipped=0 ms=514891
```
Two independent, clean, uncontended, complete `selectClass` runs, both
`found=132 ok=64 failed=68` — same count both times. Every one of the 68
failures per run carries the byte-for-byte identical stack trace as the
pre-fix binary and the original production bug:
```
java.lang.ArrayIndexOutOfBoundsException: Index 2 out of bounds for length 2
	at java.math.BigInteger.smallToString(BigInteger.java:4170)
	at java.math.BigInteger.toString(BigInteger.java:4223)
	at java.math.BigInteger.toString(BigInteger.java:4118)
	at org.hibernate.boot.model.naming.NamingHelper.hashedName(NamingHelper.java:143)
	at org.hibernate.boot.model.naming.NamingHelper.generateHashedConstraintName(NamingHelper.java:104)
	...
```
preceded by the identical `[AIOOBE3-DIAG]` header capture shape
(`array_length_field=2 num_slots=2 gc_age=1 gc_flags=0
forwarding_ptr=0x0`) as every pre-fix capture. (Two further post-fix
attempts run in parallel earlier in the session, under the same host-memory
crunch noted below, also independently fired 20+ and 45+ real
`[AIOOBE3-DIAG]` occurrences respectively before being OOM-killed short of
`@@RESULT` — additional, independent confirmation beyond the two clean
runs.)

**Conclusion: pre-fix 66/132 (50.0%) and 66/132 (50.0%) vs. post-fix
68/132 (51.5%) and 68/132 (51.5%, identical rerun) is not a meaningful
difference — well within this bug's own long-documented run-to-run
variance, and on the same side of "still crashes" both times.** The
`dbba7c93`/`b34e09cd` "select exact oop map at active safepoint" fix,
despite the previous session's mechanistically well-reasoned case for it
(missed-root-at-nested-JIT-callee-safepoint explaining every piece of this
saga's evidence), **does not measurably change this bug's reproduction
rate at all.** This is a clean refutation, not another inconclusive
non-reproduction data point — both binaries were driven with the exact
same recipe, back-to-back, on the same host, and both crashed reliably.

**What this means for the mechanism:** either (a) the previous session's
match between the oop-map bug's mechanism and this bug's evidence trail is
a coincidence — both bugs can independently produce a "validly-allocated,
non-forwarded, wrong-shape array" signature, since that is simply what any
missed-root-adjacent *or* pure-arithmetic/codegen corruption of this
specific `int[2]` looks like from the object-header level — or (b) the
`dbba7c93`/`b34e09cd` fix is real and necessary in general (906/906
`cratonvm-jit` tests still pass, per the prior session) but does not cover
the specific nested-call shape `divideMagnitude`'s calls into
`primitiveLeftShift`/`mulsub` actually hit, for a reason not yet
identified. This session did not have time to distinguish between these
after the A/B result came back negative; either way, **the search for this
bug's true root cause must continue** — the oop-map-safepoint angle should
be considered exhausted as a candidate unless new evidence specifically
reopens it.

**Host-contention note (methodological, not a finding about the bug):**
this session's host was, for roughly a 20-minute window while running 4
harness processes plus another concurrent session's `rustc -C lto=fat`
build in parallel, driven into genuine OOM-kill territory —
`journalctl -k` confirms the kernel OOM-killer fired repeatedly (killing,
among others, one of this session's own harness processes, another
session's `dbus-daemon`, and another session's `java` process) during a
window where `free -m` showed as little as 270 MB free / 809 MB available
out of 32 GB total. This did **not** invalidate any of the four `@@RESULT`
captures above (all four completed after the session backed off to
strictly one process at a time), but it did truncate three additional
attempts (one pre-fix, two post-fix) before they reached `@@RESULT` — those
are reported above only as supporting `[AIOOBE3-DIAG]` evidence, not as
clean pass/fail data points, consistent with this doc's own established
practice of not counting host-contention casualties as findings about the
code.

**Recommendation for the next session:** do not re-attempt the
`dbba7c93`/`b34e09cd` angle — it is now empirically closed, negatively.
Every other angle this 9-session investigation has tried (`iastore` index
arithmetic, operand-stack/frame sizing, `mulsub`/`primitiveLeftShift`
codegen down to the instruction level, register allocation, getfield
caching, safepoint spill, GC conservative-root scanning, the precise
oop-map scanner) has also been ruled out with comparable rigor (see this
section's full history above). The one thread not yet fully chased to a
concrete faulty line: this session's own re-confirmation that
`[AIOOBE3-DIAG]`'s header dump is always self-consistent
(`array_length_field=2` matching the reported `length=2`,
`forwarding_ptr=0x0`) across every capture in this investigation's history,
on both pre- and post-oop-map-fix binaries — meaning the array header
itself is never corrupted or stale; only the *index* (always reported as
`2`, i.e. one past the valid end) is ever wrong. A follow-up session should
pivot from "what corrupts the array" (repeatedly ruled out) to "why is the
index specifically always `length`, never some other out-of-range value" —
that specific, narrow pattern (off-by-one at the array's own boundary,
every single time, across dozens of independent captures) has not been
explicitly interrogated by any prior session and may be the more tractable
next thread to pull.

**Not moving this item to `docs/internal/fixed-suite-bugs/` and not
closing this doc.** This is the opposite of this doc's hoped-for outcome
this session: a strong, mechanistically-plausible lead was tested directly
and empirically refuted, not confirmed. `DefaultCatalogAndSchemaTest`'s
BigInteger AIOOBE remains the last open item in this document, still
unresolved after 10 sessions.

No code change made this session (the fix under test was already landed on
`dev` by a different session; this session's own findings are refutational,
not a new fix candidate) — this is a docs-only update. `git fetch origin
dev` immediately before this edit confirmed no other session has touched
`divideMagnitude`, `scan_one_frame_precise`, or this doc's
`DefaultCatalogAndSchemaTest` section since the entry above was written
(two unrelated commits landed in the interim: `2c20a877`, JIT
putstatic/new/invokestatic class-init fix, and `a4d8d2f0`, an unrelated
doc-link-path retarget after the 120s-timeout-cluster doc's archival — both
confirmed not to touch this bug's code paths or this section's content).

## Update 2026-07-17 (index-always-equals-length + recompilation-transition session): the recompilation-transition hypothesis is REFUTED via direct empirical testing (two independent mechanisms both confirmed dormant for this method family); speculative BCE also ruled out for the specific loops involved via bytecode-pattern analysis; no live crash captured this session (2 attempts, both clean) — still OPEN, no fix landed

Picked up this doc's own standing next step (get a live capture and pin the exact
faulty instruction) plus a new angle: the observation, constant across every
`CRATONVM_DBG_AIOOBE3` capture in this saga's history, that the JIT-reported
`index` is **always exactly equal to** `length` (`index=2 length=2`, never any
other out-of-range value) — never previously interrogated on its own — combined
with a fresh hypothesis that CratonVM's tiered JIT recompiling a method (C1 →
C2, or an "eager first compile" superseded later) mid-run could leave a caller
executing against a stale/inconsistent view of a callee's bounds.

**Setup.** Fresh worktree `wt-hib-biginteger-idxlen-20260717`,
`CARGO_TARGET_DIR=cv-target-hib-biginteger-idxlen-20260717`,
`CARGO_PROFILE_RELEASE_LTO=off`, built from `origin/dev@38192937` (current tip
at session start and end — confirmed via `git fetch` immediately before writing
this entry, no concurrent work landed). Frozen + md5-verified at
`/data/data/frozen-hib-biginteger-idxlen-20260717/cratonvm-idxlen-20260717`
(md5 `3b76f3e5a241ce230d3b1d44e316840a`).

### Angle 1 — speculative bounds-check elimination (`analyze_bounds_elimination`, `jit/src/x64.rs`): RULED OUT for the specific loops on this bug's call path

Never previously examined by any entry in this doc's history. Got the real
JDK25 `MutableBigInteger.divideMagnitude`/`mulsub` bytecode via
`javap -c -p -v --system=<jdk25> java.math.MutableBigInteger` (full dump kept
at `/data/data/tmp/mbi_javap.txt` on the shared host) and read
`x64.rs`'s BCE implementation (`find_induction_variable`, `analyze_loop_bound`,
`find_safe_array_accesses`, `find_speculative_array_accesses`,
`SpeculativeBCEGuard`) end to end.

**Finding: both loops on this bug's established call path recompute their
upper bound from a fresh `getfield` every iteration, not from a local variable
— a shape `analyze_loop_bound` structurally does not recognize.** The D1-normalize
branch-B loop (`for (i=1; i<intLen+1; i++)`, the loop this doc's own prior
entries most strongly localized the defect to) compiles to
`iload i; aload_0; getfield intLen; iconst_1; iadd; if_icmpge exit` — the
comparison's second operand is a 4-instruction expression, not a bare
`iload`/`bipush` immediately following the induction-variable load.
`analyze_loop_bound`'s pattern matcher (`jit/src/x64.rs` ~line 6334) requires
`iload iv; iload/bipush/sipush bound; if_icmp*` with the bound load
*immediately* following the IV load — `aload_0` (the `getfield` receiver) does
not match any of the recognized bound-load opcodes, so the matcher's `bound_local`
resolves to `None` and the function falls through without returning a
`LoopBoundsInfo` for this loop. The same is true of the main Knuth D2-D7 loop
(`for (j=0; j<limit-1; j++)`, bytecode `iload j; iload limit; iconst_1; isub;
if_icmpge`) — the trailing `iconst_1; isub` (computing `limit-1` fresh every
iteration, exactly as javac emits it, no loop-invariant-code-motion) means the
instruction right after the bound's `iload` is `iconst_1`, not a comparator
opcode, so this loop is rejected on the same structural grounds. **Neither
loop can ever receive static or speculative BCE elision from this JIT** — the
whole `analyze_bounds_elimination` pass (both its static
`find_safe_array_accesses` path, which additionally requires
`find_bound_arraylength_provenance` — moot here since there's no `bound_local`
at all — and its speculative `SpeculativeBCEGuard` path) is a dead end for this
bug specifically, confirmed by direct bytecode-pattern reading rather than by
disassembly or non-reproduction.

### Angle 2 — PGO-driven loop-unroll-factor divergence between a first (unprofiled) and later (profiled) compile: real code path exists, but subsumed by this doc's own prior finding

`jit/src/x64.rs` ~line 26570 selects the loop-unroll factor two different ways:
a static body-size heuristic (used when no profile is available — 1 extra copy
for a 20-50 byte body) vs. a PGO-driven factor from `LoopTripProfile::
suggests_unroll_factor` (`jit/src/profile.rs`, 4x unroll for hot loops with
average trip count ≤8, extending eligibility to larger loop bodies too). The
D1-normalize loop's ~46-byte body would get 1 extra copy from the static path
but 3 extra copies from the PGO path — genuinely different unrolled code for
the *same* loop depending on whether profile data was available at compile
time, which is exactly a "first compile vs. later recompile sees different
inputs" shape. However, this doc's own `fast, minimal, Hibernate-free repro`
entry already tested `CRATONVM_DISABLE_UNROLL=1` (which short-circuits *both*
the static and PGO paths to zero extra copies) against the default (which, per
that entry's own disassembly capture, happened to be running the *static*
1-extra-copy path) and found **no change** in the ~88% failure rate. That
result already rules out "unrolling of any factor is necessary for this bug"
in general, so the PGO-vs-static distinction — while a real, previously
unexamined divergent-codegen mechanism — is not a new candidate; recorded here
only so a future session doesn't have to re-derive why it's not worth chasing
further.

### Angle 3 — the recompilation-transition hypothesis (this session's primary assigned lead): REFUTED via two independent, directly-tested mechanisms

CratonVM has a genuine, well-engineered, default-on background recompilation
mechanism (`jit/src/tiered.rs`'s `request_c2_upgrade`, the "C1→C2 supersede":
after any qualifying method's C1 body publishes, a Low-priority C2 recompile —
through the **completely different** optimizing IR pipeline, `jit/src/ir.rs`/
`ir_lower.rs`/`ir_optimize.rs`/`ir_schedule.rs`, not just a re-run of the same
single-pass backend — is automatically enqueued). `mulsub` structurally
qualifies for this upgrade: it is call-free and allocation-free (confirmed via
`javap`: pure `iload`/`lload`/`iaload`/`iastore`/arithmetic, zero
`invoke*`/`new`/`anewarray`), and `c2_upgrade_would_engage`'s admission gate
(`jit/src/lib.rs` ~line 2509) passes it via the `ir_emit_long && fp_free`
clause since `CRATONVM_JIT_IR_LONG` is default-ON. **This is exactly the kind
of previously-unexamined mechanism the task's hypothesis called for**: every
prior session's exhaustive manual disassembly of `mulsub` (three-plus separate
full instruction-by-instruction traces across this doc's history) explicitly
discussed `x64.rs`-specific single-pass-backend constructs (`ARG_REGS`,
`SCRATCH_REGS`, `emit_stack_arg_setup`) with no session ever checking whether
`mulsub`'s *actual* compiled body was produced by that backend at all, versus
the entirely separate, never-scrutinized IR-optimizing pipeline.

**Directly tested, twice, and refuted both times:**

1. **The C1→C2 async supersede never fires for this method family in
   practice.** Ran `SmallDividendRepro` (this doc's own fast, reliable,
   Hibernate-free repro) for up to 3,000,000 iterations / 90 seconds under
   `CRATONVM_DBG_JITC=1` (verbose compile-lifecycle logging — `tiered-enqueue`,
   `upgrade-OK`, `full-compile` are each distinct, unambiguous log lines) —
   **zero `tiered-enqueue` and zero `upgrade-OK` events in the entire run**,
   across 3 separate invocations (20 `full-compile` events total, covering
   `divideMagnitude`, both `primitiveLeftShift` overloads, and `mulsub`, every
   one of them via the `full-compile` label only). The async tiered
   background-worker pipeline (`ensure_background_compiler`/`compiler_loop`)
   is real, present, and default-on in this codebase, but for this exact
   `SmallDividendRepro`-shaped, divide-heavy workload it never actually
   dispatches a single task — every compile happens through the separate,
   synchronous "eager direct-call callee compile" path instead (see below),
   which pre-empts the tiered manager before it ever gets a chance to enqueue
   anything (`compiled.or_else(...)` in `vm/src/runtime/interpreter.rs` only
   runs the tiered-enqueue branch when the eager path returned `None` first).
2. **Even the eager path's own `optimize=true` (C2/IR) request does not
   change `mulsub`'s compiled output at all.** Both of the two mutator-side
   "eager compile" call sites (`vm/src/runtime/interpreter.rs` ~lines 4906 and
   28424, labelled "optimize = C2 / optimizing IR pipeline" and "Eager
   direct-call callee compile — optimized (C2) tier" respectively) pass
   `optimize: true` unconditionally on `mulsub`'s first and only compile —
   meaning `mulsub` should, per `try_compile`'s own documented contract
   ("`true` ... runs the optimizing IR pipeline"), never see the single-pass
   backend at all. Directly tested this by forcing `CRATONVM_JIT_IR_LONG=0`
   (mulsub's *only* qualifying admission clause into `ir_compatible`, since it
   is long/category-2-heavy) against the default (`=1`, i.e. the IR pipeline
   admission is open) and diffing the two `CRATONVM_DBG_JIT_DISASM=mulsub`
   captures: **byte-for-byte identical compiled body both ways** (same
   `len=1029`, same instruction sequence at every offset; the only diff lines
   are ASLR-shifted absolute addresses baked into `movabs`/near-jump
   immediates, not code shape). This proves the IR-optimizing pipeline is
   **not actually the backend producing `mulsub`'s real compiled artifact**
   despite structurally qualifying per the static admission scan — something
   in the IR *builder* itself (not the coarse `ir_compatible` gate) evidently
   still bails on `mulsub`'s specific bytecode shape and falls back to the
   single-pass `x64::compile` backend every time, silently and consistently.
   This is a real, minor, latent inefficiency (a call/alloc-free hot numeric
   kernel that should be IR-eligible per the gate never actually takes the
   optimizing path) but not a correctness concern for this bug, and it also
   **positively confirms** (rather than merely failing to refute) every prior
   session's implicit assumption that `mulsub`'s manually-traced disassembly
   was examining the real single-pass-backend artifact all along.

**Conclusion: the recompilation-transition hypothesis is REFUTED for this
specific method family, via two independent, directly-executed tests rather
than by absence-of-crash alone.** There is no live recompilation event for
`divideMagnitude`/`mulsub`/`primitiveLeftShift` to race against under this
workload shape: each compiles exactly once (via the eager, synchronous,
mutator-blocking path), the async C1→C2 background supersede never engages for
them, and even the theoretically-open IR-pipeline door for `mulsub` produces
identical output to the closed-door case. A caller's compiled understanding of
these callees' bounds cannot go stale mid-run because there is only ever one
compiled version of each, for the lifetime of the process, under default
settings and under every env-var combination tried this session.

### Live-capture attempts this session

1. **Full harness, `CratonRunner`/`selectClass`, plain settings +
   `CRATONVM_DBG_AIOOBE3=1` + `CRATONVM_DBG_TIER_ENQUEUE=1`, wrapped in
   `gdb -batch` breaking on `jit_throw_aioobe`** (reusing
   `/data/data/tmp/gdb_aioobe_samecapture.gdb`'s technique, adapted to
   `/data/data/tmp/gdb_idxlen_capture.gdb`): ran ~11.75 minutes (moderate host
   contention, load average 3.7-4.7, one other concurrent session's `cargo
   test` run sharing the host) — **clean, `found=132 started=132 ok=132
   failed=0 ms=706073`, zero AIOOBE hits, zero tier-enqueue events for any of
   `divideMagnitude`/`mulsub`/`primitiveLeftShift`** (confirming this doc's
   own prior finding that `CRATONVM_DBG_TIER_ENQUEUE` doesn't cover these
   methods regardless of outcome).
2. **`SmallDividendRepro` under `CRATONVM_DBG_GC_STRESS=65536` +
   `CRATONVM_DBG_AIOOBE3=1`** (the recipe multiple prior entries reported
   firing "reliably on the first stressed run" at up to 88%): 20,000 trials,
   all 4 divide-family methods disassembled — **clean, `bad=0 of 20000`, zero
   `AIOOBE3-DIAG` hits.**

Both consistent with — not contradicting — this doc's own long-established
bimodal/heisenbug framing (several prior sessions also got 0/N clean on both
the full-harness and the GC-stress isolated repro, on different days and
different hosts, with no code difference).

### Net assessment

No live crash captured this session. No fix landed — per this doc's own
standing guidance, declined to speculatively patch anything given no failing
run to validate a fix against, and this session's own primary lead
(recompilation-transition) came back refuted rather than confirmed. What this
session adds beyond the doc's existing history:
- Speculative BCE (`jit/src/x64.rs`) is now **positively ruled out** (not just
  unmentioned) for the two loops this bug's call path actually goes through,
  via direct bytecode-pattern analysis against real JDK25 `javap` output —
  the bound-detection pattern matcher structurally cannot fire for either
  loop's `getfield`-based, freshly-recomputed-every-iteration bound.
- The recompilation-transition hypothesis — explicitly the primary new lead
  for this session — is **refuted with concrete, repeatable, directly-executed
  evidence** (zero tiered-enqueue/upgrade events across 3 runs and 90+ seconds
  of dedicated divide-heavy execution; byte-identical `mulsub` output with
  `CRATONVM_JIT_IR_LONG` on vs. off), not by absence of a crash. This closes
  off an entire class of future hypotheses ("maybe it's a stale compiled
  version of a callee") that no prior session had a concrete way to rule out.
- A genuinely new, minor, separately-worth-flagging observation: `mulsub`
  structurally qualifies for CratonVM's IR-optimizing-pipeline admission gate
  (`c2_upgrade_would_engage`/`ir_compatible`) but never actually compiles
  through it in practice — worth a future session's brief look (not on this
  bug's critical path) since a hot, call-free, allocation-free numeric kernel
  failing to reach the optimizing tier is a missed-performance opportunity,
  independent of this correctness bug.

**Next step for a follow-up session**, given the recompilation-transition and
speculative-BCE angles are now both closed: return to this doc's own
long-standing "same-process double-capture" plan (`gdb_aioobe_samecapture.gdb`
+ `CRATONVM_DBG_JIT_DISASM=divideMagnitude,mulsub` in the same process launch,
no stress flags — the plain full-harness invocation has the best track record
across this doc's history) and budget for **multiple session-lengths of
attempts**, since this session's own two attempts (one full-harness, one
GC-stress) both landed on the "clean" side of this doc's documented bimodal
split, matching several prior sessions' experience on a given day. When (per
this doc's own history, not "if") a live hit lands, the concrete mechanical
next step is unchanged from several entries above: parse the `entry=` address
from the same process's own `CRATONVM_DBG_JIT_DISASM` stdout, subtract from
the live `frame 6` return PC, locate the offset in the dump, and walk backward
from the tail-write bounds check to the nearest preceding `newarray`/
allocation to compare the SIZE-argument's source (frame slot or register)
against the INDEX-argument's source at the point of failure — the one
concrete, mechanically-checkable question this doc's history has repeatedly
identified as the actual remaining unknown, still unconfirmed by any session
to date for lack of a correlatable live capture.

Artifacts this session: fresh binary + worktree at
`wt-hib-biginteger-idxlen-20260717` /
`frozen-hib-biginteger-idxlen-20260717/cratonvm-idxlen-20260717`
(`dev@38192937`, md5 `3b76f3e5a241ce230d3b1d44e316840a`); `javap` dump at
`/data/data/tmp/mbi_javap.txt`; gdb script/log at
`/data/data/tmp/gdb_idxlen_capture.gdb`/`.gdblog`; full-harness stdout at
`/data/data/tmp/idxlen_run1_stdout.log`; isolated-repro logs and the
`mulsub` IR-on/IR-off diff at `/data/data/tmp/idxlen-repro-20260717/`.

**This item remains OPEN.** The rest of this doc's items are independently
closed; this one still blocks declaring `hib-misc-residuals-20260716.md`
fully closed.


## Update 2026-07-17/18 (concurrency-investigation session): confirmed real multi-thread execution reaches the vulnerable window; found and flagged a genuinely new, distinct CountDownLatch/AQS hang under thread churn (not fixed by the same-day `cce6e1c6` GC-barrier fix); could NOT connect concurrency to the AIOOBE's silent-corruption signature despite direct targeted testing; 5 consecutive clean full-harness runs this session (0/132 AIOOBE across every attempt, spanning both sides of `cce6e1c6`) — still OPEN, no fix landed for this specific bug

This session picked up a dimension no prior session on this item had
examined: whether CratonVM's own multi-thread execution or its JIT/GC
runtime's own thread-coordination machinery (not just single-threaded
GC-root-scanning of one thread's JIT frames) is implicated, following the
explicit precedent that a real, now-fixed concurrency bug
(`Executors.new*ThreadPool*`'s GC-relocation-stale-return-value defect,
`62be72a0`, `docs/internal/fixed-suite-bugs/hib-aqs-threadpoolexecutor-relocation-livelock-FIXED.md`)
already exists in this exact codebase area.

**Confirmed real concurrent execution reaches the vulnerable window.** Live
`ps -T`/`/proc/<tid>/comm` inspection of a real `CratonRunner`/`selectClass`
run of `DefaultCatalogAndSchemaTest` (default settings) shows five real OS
threads alive simultaneously during metadata building — the exact phase
that calls `NamingHelper.hashedName`: the launcher thread, `main-vm` (the
interpreter thread that runs the JUnit5 launcher and Hibernate boot code),
`cratonvm-jit-co[mpiler]` (the process-wide background tiered-compilation
thread — started idempotently on the first interpreter invocation hook per
`jit/src/tiered.rs::ensure_background_compiler`, present even when
Java-level execution is single-threaded), `Hibernate Conne[ction Pool
Validation Thread]` (spawned by
`org.hibernate.engine.jdbc.connections.internal.PoolState.startIfNeeded()`
via `Executors.newSingleThreadScheduledExecutor` — confirmed via direct
source read of `PoolState.java:44-48`; this thread is created fresh once
per `SessionFactory` build, i.e. up to 132 times per class run, so this is
genuine repeated thread CHURN throughout the run, not just a single
persistent extra thread), and `junit-jupiter-t[imeout-watcher]` (JUnit5's
`TimeoutExtension` — confirmed via `javap` on the real
`junit-jupiter-engine-6.0.3.jar` that even `Timeout.ThreadMode.SAME_THREAD`,
the mode this harness's global `-Djunit.jupiter.execution.timeout.default=120s`
resolves to absent parallel-execution config, still runs a
`ScheduledExecutorService`-backed watchdog thread).

**However, `NamingHelper.hashedName` itself is not directly invoked by
multiple Java threads concurrently** — JUnit test execution is
single-threaded absent explicit parallel-execution config (none found in
this harness: no `junit-platform.properties`, no
`junit.jupiter.execution.parallel.enabled` in `common.args`, confirmed via
jar/classpath inspection). The concurrency is in the surrounding VM/runtime
machinery (background JIT compiler, connection-pool thread, timeout-watcher
thread churn), not in two threads racing to call
`smallToString`/`divideMagnitude` at the same instant.

**Found and cleanly isolated a new, highly-reproducible CratonVM
concurrency bug** while probing this angle with purpose-built synthetic
repros (`MultiThreadDivRepro.java`, `CdlSpawnRepro.java`,
`MultiSpawnRepro.java`, `MinimalThreadRepro.java`, `ThreadChurnHashRepro.java`
— all kept at `/data/data/tmp/idxlen-repro-20260717/` on the shared host):
4 threads each doing 50,000 trivial allocations then
`CountDownLatch.countDown()` (no `Thread.join()` anywhere), released via a
shared start latch, under `CRATONVM_DBG_GC_STRESS=65536` (`CdlSpawnRepro.java`)
**hangs reliably on the very first round** — `CountDownLatch.await(30,
SECONDS)` times out with `getCount()==1` (one thread's countdown is never
observed), on both a pre- and a **post**-`cce6e1c6` build (see below).
Cross-checked plain concurrent execution (4 threads dividing
`BigInteger`s concurrently, no churn, no stress: clean, 0/80,000) and plain
thread churn without a `CountDownLatch` (`MultiSpawnRepro.java`, `Thread.join()`
instead: clean, sub-second per round even under the same stress level) —
isolating the defect specifically to the `CountDownLatch`/monitor-wait path
under concurrent thread activity, not to concurrency or thread churn alone.

**This independently corroborates — via a completely different, from-first-principles
repro built without knowledge of it — a separate, concurrently-running
session's same-day discovery of "a genuine livelock in the GC barrier's
`expected`/`arrived` accounting under rapid thread churn"** in the identical
subsystem (`vm/src/threading/gc_barrier.rs`), documented in
`docs/known-issues/CRATONVM-SPRING-GENUINE-BUGLIST.md` (§5.8 follow-up #3,
commit `6e5f4582`, reproduced there at only ~1/70 via a `ThreadGroup`/ordinary
`Thread` churn harness). That session's follow-up (§5.8 follow-up #5, commit
**`cce6e1c6`**, landed mid-way through this session) root-caused and fixed
one concrete instance of the mechanism: the *contended* branch of
thread-termination's "notify waiting `Thread.join()`ers" step called
`GcBarrier::enter_blocked()` without first calling `deposit_root_snapshot()`,
so a terminating thread parked in `Monitor::block_enter()` while a `join()`er
held its monitor was silently counted in a GC pause's `expected` quota with
no way to ever arrive.

**This session rebuilt from `cce6e1c6` (frozen + md5-verified at
`/data/data/frozen-hib-biginteger-concurrency-v2-20260717/cratonvm-concurrency-v2-20260717`,
md5 `155b9ac81cc6ee89f95bd544c1c8a3fe`) and re-ran `CdlSpawnRepro` against it
— it still hangs, identically.** This is expected, not a refutation of
`cce6e1c6`: `CdlSpawnRepro`'s threads never call `Thread.join()`, so they
cannot hit the exact contended-monitor branch that fix targeted. A fresh
`CRATONVM_DBG_STW_CENSUS=1` capture on the post-fix build additionally shows
the GC barrier itself is **not** stuck this time — many consecutive STW
generations complete successfully (`arrived==expected` cycling cleanly
through 2502→2507+ during the hang) while the repro's `CountDownLatch`
still never reaches zero. This means `CdlSpawnRepro`'s hang is likely a
**different bug entirely** from the GC-barrier accounting family — most
likely a genuine lost-wakeup/lost-notification defect in the
`CountDownLatch`/monitor-wait implementation itself (`native_cdl_await`/
`native_cdl_count_down`, `vm/src/vm/vm_exec.rs`'s `monitor_wait`,
`vm/src/threading/monitor.rs`), closer in spirit to the
`Executors.new*ThreadPool*` precedent bug's family than to the GC-barrier
livelock family. **Not investigated to a fix this session** — flagged as a
standalone follow-up (spawned task, self-contained repro instructions
included) rather than chased further here, since it is a distinct
concurrency bug from the AIOOBE this doc tracks and the connection to it
(next paragraph) came back negative.

**Could NOT connect any of the above — the CDL hang, thread churn, or plain
concurrent BigInteger execution — to the AIOOBE's silent-corruption
signature, despite direct, targeted testing.** `ThreadChurnHashRepro.java`
inlines `NamingHelper.hashedName`'s exact MD5-pad-hash +
`BigInteger(1, digest).toString(35)` algorithm byte-for-byte (no Hibernate
classpath dependency needed) and was run for 2000 rounds × 3 threads × 50
calls/thread (300,000 real `hashedName`-shaped calls, genuine thread
create/exit churn every round) against the doc's own previously-confirmed-crashing
frozen binary (`cratonvm-idxlen-20260717`, `dev@38192937`) —
**deliberately without `CRATONVM_DBG_GC_STRESS`**, since that flag reliably
triggers the (unrelated) CDL hang above and would mask any AIOOBE signal.
**Result: 0/300,000, no hang, no exception, ~46s wall time.** The
non-churning, purely-concurrent 4-thread divide test (no stress) was
likewise clean (0/80,000, sub-second).

**Full end-to-end harness reproduction status this session: 0/5, the
cleanest run of runs this saga has recorded to date.** Five independent,
complete `CratonRunner`/`DiscoverySelectors.selectClass` runs, no
concurrency-specific env vars beyond one with `CRATONVM_DBG_AIOOBE3=1`
enabled (to guarantee a capture if it fired): the original
`cratonvm-idxlen-20260717` frozen binary (`dev@38192937`) — `found=132
ok=132 failed=0`; three runs of a freshly built, md5-verified
`dev@6e5f4582` binary (`cratonvm-concurrency-20260717`, before `cce6e1c6`
landed) — `found=132 ok=132 failed=0` × 3 (one with
`CRATONVM_DBG_AIOOBE3=1 -Dcraton.trace=true`, zero `AIOOBE3-DIAG` captures);
one run of the post-`cce6e1c6` rebuild (`cratonvm-concurrency-v2-20260717`,
`dev@cce6e1c6`) — `found=132 ok=132 failed=0`. Zero
`ArrayIndexOutOfBoundsException` anywhere, across all five. Per this doc's
own long-standing, repeatedly-reconfirmed "absence of failure is not
evidence of a fix" conclusion (established across at least four prior
sessions that each independently hit both the "reliable ~50%" and "0/N
clean" polarities on ostensibly-equivalent code, sometimes the same day),
**this is not read as evidence the AIOOBE is fixed or has become rare** —
only as this session's own data point on the doc's documented "clean" side
of its bimodal reproduction pattern. Notably, `cce6e1c6`'s own tripwire
diagnostic (an always-on cross-check in `stw_take_over_and_wait` comparing
the barrier's legacy `blocked_count()` against the registry's
`in_blocked_region` census whenever the takeover loop stalls 64+ rounds) did
not fire in any of this session's runs either, for what that is worth.

**Verification (regression only — no code change landed for the AIOOBE
this session):** `cargo test --release -p cratonvm-jit --lib`: 906/906
pass. `cargo test --release -p cratonvm-gc --lib`: 880/880 pass. `cargo test
--release -p cratonvm-vm --lib`: 2201 passed / 16 failed — the exact,
previously-documented pre-existing debug-build-only `lock_order` +
already-broken `jit::skip_list` baseline this doc's history has repeatedly
confirmed are environmental, not regressions (matches every prior session's
count for this target).

**Assessment.** The concurrency angle this investigation was explicitly
tasked with examining is real and was pursued with positive engineering
rigor for the first time in this saga (a working, independently-reproduced,
now cross-referenced OTHER concurrency bug in the identical broad subsystem,
rather than the purely negative "didn't find anything" pattern every
single-threaded session before it produced) — but it does **not** appear to
be the mechanism behind this specific AIOOBE: the one hypothesis that would
connect them (thread churn interacting with an in-flight
`BigInteger`/`divideMagnitude` computation) was tested directly, at volume,
with the exact production algorithm, and came back clean both before and
after a real, unrelated fix landed in the same subsystem. Combined with
this session's uncharacteristically clean 5/5 full-harness reproduction
attempts, the balance of evidence this session gathered leans (without
proof, per this doc's own standing caution) toward "not concurrency, and
possibly incidentally rarer or fixed by some recent change" — but per this
doc's ten-session history of that exact impression being wrong on both
sides more than once, **this item is NOT being closed or moved to
`docs/internal/fixed-suite-bugs/`**. The concurrency hypothesis specifically
should be considered **explored and not supported by direct evidence** (not
"ruled out with certainty," since a bimodal/heisenbug this fragile resists
certainty either way) — a future session should not need to re-derive the
threading-model facts established here (no JUnit parallel execution, real
but indirect thread churn via Hibernate's connection-pool validation thread,
background JIT compiler thread always present) but should look elsewhere
for the AIOOBE's mechanism, or spend a dedicated session attempting the
now-larger battery of full-harness reproduction (this session's 0/5 joins
several prior sessions' both-polarity results; the doc's own established
"practical recommendation" — pivot to `CRATONVM_DBG_JIT_DISASM` +
same-process `gdb` double-capture the moment a live hit lands — remains the
most concrete unclaimed next step whenever that happens).

**New standalone finding spun off as a follow-up task** (not part of this
item, tracked separately): the `CountDownLatch`/monitor-wait hang under
thread churn + heavy GC pressure (`CdlSpawnRepro.java`, reproduces on
round 1 of 1, dramatically more reliable than the GC-barrier livelock's
documented ~1/70) is a genuine, distinct, still-open CratonVM concurrency
bug, confirmed NOT fixed by `cce6e1c6`. Self-contained repro and analysis
left for whoever picks it up next.

**UPDATE (2026-07-17, follow-up session):** root-caused and FIXED -- NOT
the same defect class as the GC-barrier livelock (`cce6e1c6`), despite
both being CountDownLatch-shaped and GC-pressure-sensitive. The real bug:
`native-builtins/src/lib.rs`'s `populate_real_thread_holder` (the native
override backing `new Thread(Runnable, String)`) pins the freshly
allocated `Thread$FieldHolder` object across a nested native-builtin
`FieldHolder.<init>` invoke, but never re-read the pin before using it to
build that invoke's `args` -- only after. A GC landing in the unguarded
window (routinely hit on the FIRST-ever `new Thread(...)` in a process,
whose `ensure_class_initialized("java/lang/Thread$FieldHolder")` call
loads that class for the first time) left the constructor writing
`task`/`group`/etc. into an abandoned, structurally-still-valid from-space
copy, so `Thread.holder.task` read back as a genuine zero forever and that
worker's `Runnable.run()` (and therefore its own `countDown()`) was never
invoked -- explaining the "3 of 4 threads count down, one silently never
does" symptom. Fixed by re-reading the pin immediately before use, twice
(matching the "re-read right before every use" discipline the sibling
`CRATONVM-SPRING-GENUINE-BUGLIST.md` doc keeps having to reapply). 20/20 +
10/10 clean stress runs post-fix; full writeup at
`docs/known-issues/CRATONVM-SPRING-GENUINE-BUGLIST.md` section 5.8
follow-up #7 (the authoritative writeup lives there, not here -- this is
only a cross-reference since the bug was found as a side-effect of this
doc's own BigInteger/AIOOBE investigation). The BigInteger AIOOBE item
above remains separately OPEN and is NOT connected to this fix -- do not
conflate the two.

No code change landed for the AIOOBE this session. `git fetch origin dev`
immediately before this edit confirms tip `cce6e1c6`; no other session has
touched this doc's `DefaultCatalogAndSchemaTest` section since the entry
above.

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
[hib-120s-junit-timeout-cluster-20260716.md](../../internal/hib-120s-junit-timeout-cluster-20260716.md))
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
[hib-120s-junit-timeout-cluster-20260716.md](../../internal/hib-120s-junit-timeout-cluster-20260716.md)
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
[the 120s-timeout cluster](../../internal/hib-120s-junit-timeout-cluster-20260716.md) (same
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
