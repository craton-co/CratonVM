# Hibernate ORM "misc-correctness" failure tail (misc16) — triage sweep

| | |
|---|---|
| **Scope** | 16 Hibernate ORM test classes that FAIL on CratonVM (real-JDK mode, JIT on) but PASS on HotSpot, and are NOT in the bytecode-enhancement / lazytoone / slowness-timeout / temporal-GC clusters. |
| **Baseline** | dev `1f05e792` → re-verified on dev `2da9a00d`; binary `cvmisc.exe` (worktree `CratonVM-misc16`, branch `fix/hib-misc-correctness`). Harness `../../../../apps/hib-suite-runner`, `common.args`, `maxParallelForks=1`, real JDK 25. |
| **List** | `../../../../apps/hib-suite-runner/misc16.txt` |
| **Date** | 2026-06-30 |

**Verification (merged binary `2da9a00d`→dev merge `a5847887`, fixes `2ba1347b`+`d906ce97`):**
`ExplicitQueryStatsMaxSizeTest` PASS 2/2 and `DatabaseTimeZoneMultiTenancyTest`
PASS 1/1 (both were failing). `XmlFormatterTest` PASS (dev). No regressions in
the rest of misc16; `LhmEvictProbe` + non-overriding-LHM-subclass +
`LinkedHashSet` regression probes match HotSpot. `StandardFunctionTests`
completed this run (40/44, 4 failed — matches the HotSpot slowness/shared-failure
pattern, see §15–16).

**Retry (2026-07-01, non-GC/JIT pass):** focused unit coverage for the native-registry
slowness mitigations still passes on current `dev`:
`cargo test -p cratonvm-native-api method_descriptor_prefilter -- --nocapture`
(2/2) and
`cargo test -p cratonvm-native-api hash_single_scan_preserves_legacy_keys -- --nocapture`
(1/1). Added and verified the intrinsic-table signature prefilter:
`cargo test -p cratonvm-native-builtins intrinsics::tests -- --nocapture`
(24/24, including `signature_prefilter_is_conservative`). The full Hibernate
`FunctionTests` / `StandardFunctionTests` rerun was not
available from the separate worktree because `../../../../apps` is gitignored there; a broader
`cratonvm-vm` test build also failed before execution with MSVC linker disk-space
errors. Keep §15–16 open until the app-level timing rerun is completed.

**Recheck (2026-07-02, refreshed dev `8aa12046`):** the fixes that landed on
`dev` did not close the remaining function sweep items: the refreshed dev
binary `cvmisc16-dev8aa12046-20260702.exe` still reports `FunctionTests` HANG
and `StandardFunctionTests` HANG at the 300 s harness cap. On this branch,
`StandardFunctionTests` now completes **PASS 44/44** in 296706 ms with binary
`cvmisc16-cmpfast-20260702.exe`; the old `Function.compare`/tagless-comparator
failure is gone. `FunctionTests` remains live-but-too-slow at 300 s. A watchdog
stack dump shows the late hot path in unshaded ANTLR
`ParserATNSimulator.closureCheckingStopState` / `closure_` under
`PredictionContext.mergeSingletons` during
`testAggregateIndexElementWithPath`, matching the existing H4 cold HQL/ANTLR
prediction-throughput cluster rather than a new comparator or classpath
correctness bug.

Run the sweep:
```
cd C:/craton/CratonVM/apps/hib-suite-runner
CV=C:/craton/CratonVM-misc16/target/release/cvmisc.exe TIMEOUT=300 bash rerun.sh misc16.txt misc16
```
Single class, full stacks (the harness gates `printStackTrace` on `-Dcraton.trace=1`):
```
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 $CV --java-home "C:/Program Files/Java/jdk-25" --Xmx 1500m \
  @common.args -Dcraton.trace=1 -Dcraton.batch=1 CratonRunner "<windows-path-listfile>" 0
```

---

## Summary table

**2026-07-02 summary override:** row 15 remains OPEN and is now routed to the
existing H4 ANTLR cold-prediction/deep-recursion throughput cluster. Row 16 is
FIXED after the comparator lambda/function dispatch fix now present on current
`dev`; the focused branch run passes `StandardFunctionTests` 44/44.

| # | Class | Symptom | Root cause | Disposition |
|---|-------|---------|-----------|-------------|
| 1 | `stats.ExplicitQueryStatsMaxSizeTest` | `expected:<0> but was:<1000>` | native `LinkedHashMap.put` never ran `removeEldestEntry` eviction hook | **FIXED** (this branch) |
| 2 | `multitenancy.DatabaseTimeZoneMultiTenancyTest` | timestamp +6h | `TimeZone.getTimeZone("CST")` (3-letter id) → rawOffset 0 | **FIXED** (this branch; HIB-CV-34 family) |
| 3 | `mapping.type.format.XmlFormatterTest` | (was UOE) | — | **FLIPPED GREEN** on current dev — excluded |
| 4 | `cache.EnhancedProxyCacheTest` | can't cast to `PersistentAttributeInterceptable` | `allocate_loader_id` aliased user namespace 2 onto Application + loader-blind reflection/lambda dispatch | **FIXED** (gated; §4–9) |
| 5 | `flush.AutoFlushBeforeLoadTest` | To-one mapping type mismatch / persister | same | **FIXED** (gated; §4–9) |
| 6 | `jpa.callbacks.PrivateConstructorEnhancerTest` | can't cast to `PersistentAttributeInterceptable` | same | **FIXED** 5/5 (gated; §4–9) |
| 7 | `type.LobUnfetchedPropertyTest` | can't cast to `PersistentAttributeInterceptable` | same | **FIXED** (gated; §4–9) |
| 8 | `pc.InstanceIdentityTest` | IAE "not of expected type" | same | **FIXED** (gated; §4–9) |
| 9 | `proxy.ProxyClassReuseTest` | now `assertSame(getClassLoader(), cl1)` (was `already defined by app loader`), 2/3 | dual-isolated-loader `getClassLoader` attribution | ADVANCED (gated; proxies now distinct via keystone) — still owned by `hib-proxyclassreuse-loader-blind-class-resolution.md` |
| 10 | `util.dtd.EntityResolverTest` | unmapped entity `[Child]` | synthetic `XMLInputFactory.newInstance()` stub shadows real Woodstox → resolver ignored, general entity not expanded | **FIXED** (default-ON; `CRATONVM_REAL_STAX_FACTORY=0` escape hatch); slowness is separate (§15–16) |
| 11 | `jpa.transaction.TransactionTimeoutTest` | `getStatus()=0 ACTIVE` | `ConcurrentHashMap.entrySet()` returned a dead snapshot, so Narayana's transaction reaper could not remove live entries | **FIXED** (dev; see internal CHM entrySet write-through doc) |
| 12 | `id.uuid.rfc9562.UUidV6V7GeneratorTest` | MockitoException | native `AnnotatedTypeBaseImpl.location` null → `getAnnotatedOwnerType` NPE | **NPE FIXED**; residual = 1M-iteration slowness (§15–16 cluster) |
| 13 | `batchfetch.DynamicBatchFetchTest` | `testMultiLoad` 120s timeout (param-binding 1/2 now passes) | slowness (2000-row multiLoad); old param-binding failure appears fixed | Route → slowness cluster (§15-16) |
| 14 | `service.ClassLoaderServiceImplTest` | AssertionError (2/2) | (a) real-mode `loadClass` skipped `findLoadedClass` (override-first copy lost); (b) `ServiceLoader` read a `file:` descriptor URL as a classpath resource | **FIXED** (this branch; HIB-CV-24 family) |
| 15 | `query.hql.FunctionTests` | HANG @300s (really ~49× slow) | CPU-bound in interpreter `NativeMethodRegistry::find` (per-invoke native-shadow check, invoke-cache miss); NOT logging | OPEN — slowness (profiled) |
| 16 | `query.hql.StandardFunctionTests` | HANG @300s (really slow) | same slowness cluster as §15 | OPEN — slowness |

---

## 1. ExplicitQueryStatsMaxSizeTest — `LinkedHashMap` eviction hook (FIXED)

**Root cause.** Hibernate's bounded query-plan stats use
`StatsNamedContainer` → `BoundedConcurrentHashMap(capacity, …, LRU)`. The LRU
policy is a `LinkedHashMap` subclass (`super(capacity, lf, true)` access-order)
that evicts via an overridden `removeEldestEntry`. CratonVM **force-natives**
`LinkedHashMap.put` (`native_lhm_put` in `../../../../native-collections/src/lib.rs`), and
that native inserted the node + bumped `size` but **never replicated
`LinkedHashMap.afterNodeInsertion(true)`** — so `removeEldestEntry` was never
consulted and the map grew without bound. Entry `"1"` is never evicted →
`getExecutionTotalTime()` returns 1000 instead of 0.

Isolated with `LhmEvictProbe.java` (a `LinkedHashMap` subclass with
`removeEldestEntry` capping at 8):

```
HotSpot:  size after 200 puts = 8    contains(1) = false
CratonVM: size after 200 puts = 200  contains(1) = true   (insertion-order LHM: 50 not 8)
```

**Fix** (`../../../../native-collections/src/lib.rs`, end of `native_lhm_put`): after a new
node insert, when the receiver's runtime class is not exactly
`java/util/LinkedHashMap` (base + `LinkedHashSet` backing never override the
hook), invoke `removeEldestEntry(Ljava/util/Map$Entry;)Z` virtually on the
eldest insertion-order entry (the head); if it returns true, remove the eldest
by key. The removal is idempotent — Hibernate's `LRU.removeEldestEntry` already
removes the entry reentrantly (`segment.remove → eviction.onEntryRemove →
this.remove`), so the subsequent native remove is a no-op there; for a plain LRU
cache it performs the eviction.

---

## 2. DatabaseTimeZoneMultiTenancyTest — 3-letter zone id rawOffset (FIXED)

**Root cause.** The test reads back a `created_on` timestamp on a tenant whose
session `jdbcTimeZone` is `"CST"` and asserts `12:00`, but got `18:00` (+6h).
HIB-CV-34 (`../../../../native-builtins/src/lib.rs` `tz_standard_offset_seconds`) wired
standard UTC offsets for IANA zone ids, but the deprecated **three-letter
abbreviations** (`CST`/`EST`/`PST`/`MST`/`HST`) were absent → fell back to
rawOffset 0, shifting the per-session JDBC timestamp by the full zone offset.
Real JDK 25: `TimeZone.getTimeZone("CST").getRawOffset() == -21600000` (−6h).

**Fix:** add `EST −5h, CST −6h, MST −7h, PST −8h, HST −10h` to
`tz_standard_offset_seconds`. (Standard, non-DST offsets — faithful to real
JDK's fixed rawOffset for these ids; the failing timestamp `2018-11-23` is
standard time.) Same family as the FIXED HIB-CV-34 doc.

---

## 4–9. Loader-blind class store — bytecode-enhancement / persister group (FIXED, gated)

Five `@BytecodeEnhanced` classes (`EnhancedProxyCacheTest`,
`AutoFlushBeforeLoadTest`, `PrivateConstructorEnhancerTest` 5/5,
`LobUnfetchedPropertyTest`, `InstanceIdentityTest`) now **PASS** with the gate
`CRATONVM_LOADER_AWARE_RESOLUTION=1` (0→5). `ProxyClassReuseTest` is the
separate dual-isolated-loader variant (still owned by
`hib-proxyclassreuse-loader-blind-class-resolution.md`; see below).

Each test runs through a child `EnhancingClassLoader` (parent = app loader) that
ByteBuddy-enhances the entity and `defineClass`es it so it implements
`PersistentAttributeInterceptable` / `ManagedEntity` / `SelfDirtinessTracker`.
Enhancement itself was never the problem — the leak was a chain of loader-blind
resolution sites, each unmasked as the previous was fixed. Diagnosed with
env-gated traces (`CRATONVM_IAE_TRACE`) on a single class at a time.

### KEYSTONE (ungated correctness fix) — `allocate_loader_id` namespace-id collision

`NativeContextImpl::allocate_loader_id()` (`../../../../vm/src/vm/vm_exec.rs`) started its
counter at **1**, handing the first two user-defined loaders namespace ids 1 and
2. But the `ClassLoaderId` i32 encoding (`loader_id_of_class` /
`define_class_full`) reserves **0=Bootstrap, 1=Extension, 2=Application**, so
`UserDefined(2)` *aliases* Application. The `EnhancingClassLoader` got namespace
**2**, so its enhanced entity was stored as `UserDefined(2)` ≡ Application —
indistinguishable from the un-enhanced global copy. Downstream loader-faithful
checks that assume user ids are `>= 3` (`inherit_lookup_loader`'s `raw < 3`
guard) then re-homed the ByteBuddy instantiator (`X$HibernateInstantiator`) into
the Application namespace, so its `new Country` resolved the *un-enhanced* copy →
`can't be cast to PersistentAttributeInterceptable`. **Fix:** start the counter
at **3** so every allocated namespace is a genuine `UserDefined` id. Ungated —
this is a strict encoding-collision correctness fix (a user namespace must never
alias a built-in category); validated by the classloading/native-builtins unit
suites and by gate-off tests still producing the original failure modes.

### Three further loader-faithful gaps (all gated on `CRATONVM_LOADER_AWARE_RESOLUTION`)

- **(a) Reflective `Method`/`Field`/`Constructor` type resolution**
  (`../../../../native-builtins/src/lang_class.rs`) materialised return/parameter/field
  descriptor classes through the flat global store, so an enhanced entity's
  `getContinent()` reported the *un-enhanced* `Continent` while the to-one target
  (via `Class.forName` through the enhancing loader) was enhanced —
  `ToOneAttributeMapping`'s `declaredType.isAssignableFrom(targetType)` failed
  (`mapped with targetEntity=X, but declared as X`). **Fix:**
  `descriptor_to_class_mirror_via_loader` resolves `L`-form types through the
  **declaring class's own loader namespace** (exact `(loader,name)` probe, no
  Java `loadClass` → GC-safe mid-reflection-build).

- **(b) Lambda impl-method dispatch** (invokedynamic;
  `../../../../vm/src/runtime/interpreter.rs` `try_lambda_dispatch` + `../../../../vm/src/vm/vm_exec.rs`)
  dispatched the impl by its owner **name** → global copy, so a
  `session -> { new Entity(); }` lambda ran the *un-enhanced* enclosing-class
  body. **Fix:** `lambda_impl_dispatch_override` resolves the impl owner through
  the invokedynamic **caller's** loader (`lambda_proxy_hosts`) for
  `InvokeStatic`/`InvokeSpecial`/`NewInvokeSpecial`; the
  `InvokeVirtual`/`InvokeInterface` branches dispatch on the **receiver's exact
  class_id** when it diverges from the by-name copy (guarded to real,
  same-named, non-lambda-proxy receivers — mirrors the ordinary
  `invoke_virtual` divergence override).

- **(c) Lambda-arg `checkcast`** (`checkcast_lambda_instantiated_args` /
  `lambda_arg_provably_not_instance`) rejected an enhanced entity as
  `X cannot be cast to X` against the name-resolved (un-enhanced) copy — the
  `ImmutableEntity::getName` method-reference receiver coercion. **Fix:** gated
  name-equality carve-out: two same-**named** cross-loader copies are the same
  logical type, so the cast succeeds.

### Results & residual

- Gate-on: the 5 `@BytecodeEnhanced` classes PASS (0→5).
- Gate-off: byte-identical — the same *original* failure modes
  (`can't be cast to PersistentAttributeInterceptable` / `not of expected type`),
  no crash.
- `ProxyClassReuseTest.testNoReuse`: **advanced** — the keystone made the two
  isolated proxies distinct (`assertNotSame` now passes; was
  `IncompatibleClassChangeError: already defined by application loader`). It now
  fails only its residual `assertSame(proxyClass1.getClassLoader(), cl1)`
  (`getClassLoader()` reports the app loader) — still the separately-owned
  dual-isolated-loader `getClassLoader`-attribution case
  (`hib-proxyclassreuse-loader-blind-class-resolution.md`).

**Why the existing gate misses it (fix direction).** The gated
`resolve_class_loader_aware` (`../../../../vm/src/runtime/interpreter.rs`) only covers
bytecode `CONSTANT_Class` sites (`ldc`/`new`/`checkcast`/`instanceof`/owner
constants). The entity `Class` here arrives via the `@DomainModel(annotatedClasses
= {…})` **annotation element value** and reflective `Class` lookups, which still
go through the flat global store. Loader-faithful resolution must extend to
annotation-element `Class` values (decoded in `../../../../reader/src/attribute.rs`,
surfaced via `../../../../native-builtins/src/lang_class.rs`) and to the layer-2/3
namespace attribution in `../../../../native-builtins/src/lang_system.rs` (`defineClass`) /
`classloader.rs` for a child loader whose parent is the application loader.
Core class-store change, app-gauntlet blast radius — keep behind the gate.

---

## 10. EntityResolverTest — StAX `classpath://` general entity (FIXED, default-ON; escape hatch `CRATONVM_REAL_STAX_FACTORY=0`)

`Parent.hbm.xml` declares an external general entity
`<!ENTITY child SYSTEM "classpath://…/child.xml">` and references it as
`&child;`. Hibernate's StAX `XMLResolver`
(`LocalXmlResourceResolver.resolveEntity`) maps `classpath://` to a classpath
resource stream. On CratonVM the `&child;` expansion was **empty** → the `Child`
`<class>` mapping never loaded → `MappingException: Collection [Parent.children]
references an unmapped entity [Child]`.

**Root cause (confirmed by probe).** CratonVM **force-natives**
`XMLInputFactory.newInstance()`/`newFactory()` to a *synthetic* factory — an
instance of the **abstract** `javax/xml/stream/XMLInputFactory` itself
(`../../../../native-builtins/src/xml_stax.rs` `native_factory_new_instance`) — whose
`setXMLResolver` is a no-op (`native_factory_set_property`) and whose
quick-xml-backed cursor reader never expands external **general** entities. This
synthetic factory **shadows the real Woodstox** (`com.ctc.wstx.stax.WstxInputFactory`,
`woodstox-core` + `stax2-api` on the classpath) that HotSpot's `FactoryFinder`
selects. Probe (`StaxProbe`/`StaxProbe2`, `../../../../apps/hib-suite-runner`) on the same
JDK 25 classpath:

| Path | HotSpot | CratonVM (pre-fix) |
|------|---------|--------------------|
| `XMLInputFactory.newInstance()` | `WstxInputFactory` → entity spliced | **synthetic `XMLInputFactory`** → `&child;` literal/empty |
| `ServiceLoader.load(XMLInputFactory.class)` | `WstxInputFactory` | `WstxInputFactory` *(works!)* |
| direct `new WstxInputFactory()` | resolver called, entity spliced | **resolver called, entity spliced** *(works!)* |
| `newFactory(id, cl)` (FactoryFinder) | Woodstox, entity spliced | **Woodstox, entity spliced** *(works!)* |
| direct JDK `XMLInputFactoryImpl` | (module-blocked) | NPE `fEntityManager` null — **why the synthetic stub exists** |

i.e. the only broken link was `newInstance()` returning the synthetic stub
instead of the real provider; Woodstox itself runs correctly as bytecode on
CratonVM, and `ServiceLoader`/`FactoryFinder` already resolve it.

**Fix** (`../../../../native-builtins/src/xml_stax.rs`, **default-ON**; `CRATONVM_REAL_STAX_FACTORY=0`
is the escape hatch). `native_factory_new_instance` first resolves a real,
concrete third-party provider via `ServiceLoader.load(XMLInputFactory.class, TCCL)`
(the same lookup `FactoryFinder` uses, minus its broken JDK fallback) and returns
it; the synthetic stub remains the fallback when **no** provider is registered
(the JDK's own `XMLInputFactoryImpl` is unusable here — `fEntityManager` null — so
WildFly's `XMLInputFactoryUtil.create()` boot path is unaffected when no provider
is present). Native dispatch is keyed on the abstract base class, so a concrete
`WstxInputFactory` instance runs its own real `setXMLResolver`/`createXMLEventReader`
bytecode (verified: the natives do **not** re-intercept the subclass). By default
`EntityResolverTest` **PASS 1/1** (`ok=1 failed=0`); with `=0` it reverts to the
synthetic stub (still FAIL) — confirming the escape hatch. Default-ON is the
"real-Java-default" direction and only diverges from the old synthetic behavior
when a third-party factory is present (i.e. exactly HotSpot's choice).

**Slowness is a *separate*, pre-existing issue — NOT the StAX path.** The
hypothesized DTD-fetch fallback is **disproven**: isolated wall time is ~56 s in
**both** the FAIL (gate-off, synthetic) and PASS (gate-on, Woodstox) runs
(`ms=56534` vs `ms=57388`). Both consult the resolver for the DTD (mapped
locally, no network), so the time is Hibernate bootstrap + verbose FINEST/FINE
logging (the §15–16 cluster), independent of entity resolution. The 120 s
`TimeoutException` in the 8-shard sweep is shard contention, not a deadlock.
Route the slowness to the Hibernate slowness cluster; the **correctness** item is
fixed.

**StAX gauntlet (validated the flip — now default-ON).** Ran a 13-class
Hibernate XML-binding subset (all hit the StAX binder; Woodstox on classpath so
the gate fires) gate-OFF vs gate-ON, plus a no-provider fallback probe:

| Scenario | Result |
|----------|--------|
| Hibernate XML subset, gate **OFF** | PASS 11, FAIL 1 (EntityResolverTest), NOTESTS 1 |
| Hibernate XML subset, gate **ON** | PASS 12, FAIL 0, NOTESTS 1 — **EntityResolverTest FAIL→PASS, all 11 others unchanged, zero regressions** |
| No-provider classpath, gate ON vs OFF | identical: both return the synthetic stub, parse OK, no crash |

Subset: `OrmXmlEnumTypeTest, OrmXmlIndexTest, OrmXmlGeneratedTest,
PreParsedOrmXmlTest, HbmXmlComponentVisitorTest, XMLMappingDisabledTest,
OrmXmlParseTest, ImmutableEntityXmlMappingTest, MappingClassMoreThanOnceTest,
XmlMappingTests (NOTESTS both ways), UserTypeTest,
ForeignKeysCreationForXMLMappingTest, EntityResolverTest`.

Coverage notes: the Spring StAX-SAX bridge tests (`SC-stax-xml-family.md`) and
Keycloak/WildFly boot were **not** run live — their classpaths carry **no**
third-party StAX provider (verified), so the gate is a *no-op* there (synthetic
stub retained), and the no-provider probe confirms that path is crash-free and
byte-identical to the synthetic stub. The flip is regression-free on all
exercised paths and only changes behavior when a real provider is present (=
HotSpot's own selection). **Flipped to default-ON** (escape hatch
`CRATONVM_REAL_STAX_FACTORY=0`); a full app-gauntlet remains advisable to cover
any classpath bundling a *non-Woodstox* provider (Aalto etc.).

---

## 11. TransactionTimeoutTest - JTA transaction-reaper entry removal (FIXED on dev)

This row was stale. The timeout daemon and Narayana scheduling path were not the
root cause. The real failure was CratonVM's native `ConcurrentHashMap.entrySet()`
returning a dead snapshot instead of a live write-through view, so Narayana's
`TransactionReaper` bookkeeping could not remove active timeout entries through
the entry-set iterator path.

The fix lives in `../../../../native-collections/src/lib.rs`: `ConcurrentHashMap.entrySet()`
now returns a live synthetic view, and `HashSet.remove` delegates entry-set
removal back to the source map. The detailed fixed write-up is
`docs/internal/hibernate-bugs/hibernate-jta-narayana-reaper-chm-entryset-writethrough.md`.

---

## 12. UUidV6V7GeneratorTest — Mockito → JDK `AnnotatedTypeFactory` NPE (NPE FIXED; residual hang)

Mockito inline-mock self-attach **succeeds** here (not the kafka bug-09 case).
ByteBuddy then copies type annotations onto the generated mock's methods and
calls `AnnotatedType.getAnnotatedOwnerType()`, where the real JDK
`AnnotatedTypeBaseImpl.getLocation()` returned **null** → `NullPointerException`
in `popLocation(byte)` (`AnnotatedTypeFactory.java:190`), surfacing as
`MockitoException: cannot mock … SharedSessionContractImplementor`.

**Root cause (FIXED).** CratonVM *natively constructs* `AnnotatedTypeBaseImpl`
(`make_annotated_type` / `make_annotated_type_with_anns`, `../../../../native-builtins/src/lang_class.rs`)
but set only the `type` and `annotations` fields — leaving `location` **null**.
The non-overridden JDK `getAnnotatedOwnerType()` does `getLocation().popLocation((byte)1)`
→ NPE. **Fix** (`annotated_type_fill_bookkeeping`): seed
`location = TypeAnnotation$LocationInfo.BASE_LOCATION` (depth 0 → `popLocation`
returns null → the owner is rebuilt with BASE_LOCATION by real-JDK code) and
`allOnSameTargetTypeAnnotations = empty[]`. Isolated with `AtypeProbe.java`
(no Mockito): pre-fix throws the exact NPE on `getAnnotatedOwnerType()` for a
nested class and `Map.Entry`; post-fix returns the owner type, matching HotSpot.
Branch `fix/annotatedtype-location` (commit `3ee5ffaf`, off dev `29acdee7`).

**Residual = slowness cluster (not a distinct bug).** With the NPE gone, the
`mock(SharedSessionContractImplementor.class)` call **succeeds** and the test
body runs. A watchdog Java thread-dump shows the hot frame is
`org.assertj.core.api.StringAssert.<init>` inside `testMonotonicity` — the test
does **`ITERATIONS = 1_000_000`** UUID generations followed by 2 × 1 000 000
`assertThat(...)` comparisons (`assertThat(uuid.toString()).isGreaterThan(...)`).
On HotSpot this JITs to a tight loop and finishes well under the 120 s per-test
JUnit timeout; on CratonVM the interpreter/early-JIT runs the 3 M-operation loop
too slowly to finish in 120 s. JUnit's `SameThreadTimeoutInvocation` cannot
preempt the running loop, so the default stack-dump watchdog aborts the process
first (the earlier "MockitoException" was the *real* VM bug; this residual is the
same slowness family as §15–16 `FunctionTests`). **Verdict:** the AnnotatedType
fix removes the actual VM defect; the remaining failure is CratonVM loop
throughput on a 1 M-iteration microbenchmark-style test — route to the slowness
cluster, not a new correctness bug.

---

## 13. DynamicBatchFetchTest — route → slowness cluster

`found=2 ok=1 failed=1`: the previously-failing `testDynamicBatchFetch`
`JdbcParameterBindingsImpl` IdentityHashMap param-binding miss now **passes**;
the remaining failure is `testMultiLoad` exceeding the 120 s per-test timeout
(2000-row insert + `byMultipleIds` multiLoad — slowness, not wrong-result).
Keep it with the Hibernate slowness work unless a fresh correctness failure
appears.

## 14. ClassLoaderServiceImplTest — FIXED (two real-JDK-mode loader bugs)

Both tests `AssertionError`; both root-caused and **FIXED** (this branch). Each
was a distinct, independent real-JDK-mode defect, isolated with standalone
probes (`ClProbe`/`SlProbe`, custom `ClassLoader` subclass — no Hibernate stack).
Real-JDK-mode note: `ClassLoader.loadClass`/`findLoadedClass`/`defineClass1` run
through `../../../../native-builtins/src/classloader_real.rs` (+ `service_loader.rs`), **not**
the synthetic-mode `classloader.rs` natives — the fixes had to land there.

**14a. `testSystemClassLoaderNotOverriding` (HHH-7084) — `loadClass` skipped
`findLoadedClass`.** `TestClassLoader.overrideClass(Entity.class)` calls
`defineClass("jakarta.persistence.Entity", bytes…)` to define its OWN copy, then
`loadClass(name)` must return THAT copy (JVMS §5.3.2 step 1: `c =
findLoadedClass(name)` before any parent delegation). Probe delta on the unfixed
binary: `findLoadedClass(name)` correctly returned the overridden class, but
`loadClass(name)` returned the **app-loader original** — so `assertThat(
anotherClass).isNotSameAs(testClass)` failed at line 49. Root cause:
`cl_real_load_class_base` (real-mode base delegation) went straight to the
flat-global `ctx.load_class` without first consulting the loader's own-defined
class. **Fix:** for a user-defined loader, call
`find_loaded_class_for_loader(this, name)` (the exact, no-global-fallback logic
the `findLoadedClass` native already uses) FIRST and return its result; `None`
falls through to the existing delegation, so built-in loaders and the null-parent
`findClass`-deferral path (`AggregatedClassLoader`) are unchanged.

**14b. `testStoppableClassLoaderService` (HHH-8363) — `ServiceLoader` read a
`file:` descriptor URL as a classpath resource.** `TestClassLoader` overrides
`findResources` to return a forced `file:` URL for the `TypeContributor` service
descriptor; `loadJavaServices` must find exactly 1 provider. Probe delta:
`discover_providers` (`service_loader.rs`) DID obtain the URL via
`loader.findResources`, but then extracted its path and looked it up with
`find_all_resource_bytes` — a **classpath-relative** lookup — so an absolute
`file:/C:/…/META-INF/services/<spi>` path missed and the provider list came back
empty (`hasSize(1)` got 0 at line 91). **Fix:** when the descriptor URL is a
plain `file:` URL (no `!/` jar separator), read the file directly from the
filesystem (`std::fs::read` on the percent-decoded path); the classpath lookup
remains the fallback for `jar:`/`classpath:` URLs.

**Verification (branch binary):** `service.ClassLoaderServiceImplTest` **2/2
PASS** (was 0/2). No regressions: sibling
`bootstrap.registry.classloading.ClassLoaderServiceImplTest` 7/7 (unchanged),
`proxy.ProxyClassReuseTest` 2 ok/1 fail (unchanged — its `testNoReuse` is the
still-OPEN dual-isolated-loader item, `hib-proxyclassreuse-loader-blind-class-resolution.md`),
a normal-delegation probe (custom loader → app/bootstrap parent, stable identity,
CNFE on miss) matches HotSpot exactly, and `cratonvm-classloading` (527) +
`cratonvm-native-builtins` (2711) unit tests green. HIB-CV-24 loader-isolation
family.

## 15–16. FunctionTests / StandardFunctionTests — slowness cluster, NOT a correctness bug

`FunctionTests` was run to completion under a 1200 s cap: CratonVM finishes in
**1169 s** (rc=0, `found=123 ok=97 failed=20`) vs HotSpot **24 s**
(`ok=99 failed=18`) — **~49× slower**. The 300 s "HANG" (rc=124) was purely a
timeout artifact of a slow-but-live process; no per-test `TimeoutException`, no
deadlock, steady forward progress to `@@DONE`. (The process is CPU-bound in the
interpreter — see the profile below; logging volume is high but not the CPU
cost.)

Crucially, the CratonVM failures **map onto the same failing tests as HotSpot**
(20 vs 18; identical queries/assertions — `sinh`, `theDuration`,
`maxindex/indices`, `cast(boolean as String)`, etc.). These are pre-existing
harness/dialect failures present on HotSpot too — **not** CratonVM HQL-function
wrong-results. The only delta is exception flavor (CratonVM surfaces
`ArrayIndexOutOfBoundsException` where HotSpot surfaces `IndexOutOfBoundsException`).

**Disposition:** slowness-cluster (the ~49× slowdown is the real issue), not a
distinct misc-correctness bug — arguably mis-classified into this tail.
`StandardFunctionTests` should re-run on the merged binary to confirm the same
pattern (HotSpot baseline: 29 s, `found=44 ok=41 failed=3`).

### 2026-07-02 recheck after refreshed dev

Refreshed `dev` at `8aa12046` still timed out both `FunctionTests` and
`StandardFunctionTests` at the 300 s harness cap. The branch binary
`cvmisc16-cmpfast-20260702.exe` fixes the `StandardFunctionTests` side: the
class now completes `found=44 started=44 ok=44 failed=0 aborted=0 skipped=0`
in 296706 ms. The bug was the comparator native treating a tagless lambda
`java.util.function.Function` from `Comparator.comparing` as though it were a
`Comparator` and dispatching `compare`; the fix identifies Function and
primitive key-extractor SAMs directly and compares their extracted keys.

`FunctionTests` is still not closed. With the comparator fixed it advances into
the known H4 path: the watchdog stack for
`testAggregateIndexElementWithPath` is in
`ParserATNSimulator.closureCheckingStopState` / `closure_`, called through
`ATNConfigSet.add` and `PredictionContext.mergeSingletons`. This is the same
cold HQL/ANTLR prediction-throughput and deep-recursion problem documented in
`../spring/springrepos-extension-hang-jit-throughput-and-deep-recursion.md`.
It is not a new wrong-result or tagless-comparator correctness failure.

After rebasing onto current local `dev` at `c4eb9dfa`, the comparator native
fix is already present in `dev` and no longer appears as a delta in this branch.
The remaining branch delta records the recheck and keeps only the miss-path
throughput reductions that helped the focused Hibernate rerun reach the
resolved `StandardFunctionTests` path.

### Profile (cdb sampling, `release-with-debug` binary + PDB)

Sampled the live `FunctionTests` executor thread (`main-vm`) with `cdb -pv`
(24 top-frame + 10 deep-frame samples). The result is unusually concentrated:

- **CPU-bound**, not I/O/logging-bound: the thread shows ~1:1 user-CPU-to-wall
  time; **0 of 34 samples** landed in any logging / `format` / `write` / `fmt`
  frame. The earlier "heavy TRACE/FINEST logging dominates" guess is **wrong** —
  `-Dhibernate.show_sql`/`format_sql` add stdout volume but negligible CPU.
- **24/24 top frames** and **10/10 deep chains** are the *same* stack:
  `cratonvm_native_api::registry::NativeMethodRegistry::find`
  (× the two `hash_pass` FNV passes) ← `interpreter::execute_invokevirtual_vtable_fast`
  ← `execute_frame` ← `execute` ← `vm_exec::invoke_on_class_shared_inner`
  ← `invoke_shared` ← `interpreter::try_lambda_dispatch`.

**Root cause.** `execute_invokevirtual_vtable_fast` is the **invoke-cache MISS
path** (per its own Step-1 comment). Before reading the vtable it runs a
native-shadow check — `native_methods.find(receiver, m, d)` **and** a
superclass-chain walk calling `find(parent, m, d)` for every ancestor
(`interpreter.rs` ~24185–24250, the "FJP fix") — to catch methods natively
shadowed on a supertype. `NativeMethodRegistry::find` recomputes
`native_method_hash` = **two full FNV byte-passes over
`class · method · descriptor`** (long Hibernate names/descriptors) on **every
call**. For this HQL/lambda workload the per-call-site `invoke_cache` misses
repeatedly (lambda / reflective-getter dispatch through `try_lambda_dispatch`),
so `vtable_fast` — and its double string-hash × parent-chain-depth — re-runs on
nearly every `invokevirtual`. That single check is ~100 % of on-CPU time.

**Also ruled out:** per-test SessionFactory rebuild (FunctionTests uses
*class-level* `@DomainModel(GAMBIT)`/`@SessionFactory` + `@BeforeAll` → built
once); JIT wrong-results (the hot bodies run in the **interpreter**
`execute_frame`, i.e. they are not getting JIT-compiled — a second lever).

**Fix levers (highest → lowest leverage; all hot-path, need broad regression soak):**
1. **Memoize the native-shadow verdict** per `(receiver_class_id, method, desc)`
   (or per call-site) so `find()` + the parent-chain walk run once, not per
   invoke. Kills the dominant cost directly.
2. **Fix the `invoke_cache` miss** on the `try_lambda_dispatch` /
   reflective-invoke path so `vtable_fast` stops re-running for warm sites.
3. **Cheaper `native_method_hash`** — single pass, or precompute/intern the
   (class,method,desc) hash on the resolved-method record instead of re-hashing
   raw `&str` each call.
4. **JIT-compile the interpreted lambda/reflection-invoked bodies** (they'd
   inline the call and skip `vtable_fast`+`find` entirely); investigate why they
   stay interpreted (invocation counting on these dispatch paths).

The profile is the deliverable here; the fix is a hot-path interpreter change
(every `invokevirtual`, gauntlet-wide blast radius) and should be prototyped +
soaked separately, not landed blind.

### 2026-07-01 partial mitigation

Implemented lever 1 for the vtable-fast path: each `JvmThread` now keeps a
bounded native-shadow verdict cache keyed by
`(receiver_class_id, method_name_hash, descriptor_hash)`. Repeated
invoke-cache misses for the same receiver/method descriptor can skip both the
direct `NativeMethodRegistry::find` probe and the superclass walk after the
first verdict. The cache is bypassed whenever class redefinition is active, so
Mockito/JVMTI-woven bytecode keeps the existing native-shadow suppression
semantics.

This should remove the profiled hot `NativeMethodRegistry::find` cost from warm
miss sites, but the full Hibernate `FunctionTests` / `StandardFunctionTests`
rerun has not been completed on this branch. Keep this cluster open until that
end-to-end verification confirms the slowdown is gone and no other lever is
needed.

### 2026-07-01 follow-up: native-registry hash hot-path hardening

Implemented lever 3 as a general registry-side mitigation: `native_method_hash`
now updates both 64-bit FNV-style accumulators during one scan over
`class.method.descriptor` instead of walking the same long Hibernate
class/method/descriptor strings twice. The key format is intentionally
unchanged, and a focused regression compares representative hashes against the
legacy independent two-pass calculation.

This reduces the cost of every `NativeMethodRegistry::find` call, including
paths not covered by the vtable-fast verdict cache above. It still does not
claim to close the Hibernate `FunctionTests` / `StandardFunctionTests` slowness
cluster until those external tests are rerun.

### 2026-07-01 follow-up: native-shadow signature prefilter

Implemented another general hot-path mitigation: `NativeMethodRegistry` now
exposes a conservative O(1) membership check for `(method_name, descriptor)`.
When a clean descriptor has no native registration on any class, the
interpreter can skip class-qualified native probes and superclass walks
entirely.

This prefilter is used in `execute_invokevirtual_vtable_fast`, the intrinsic
native-override guard, and `populate_virtual_invoke_cache`. Positive or
descriptor-quirk cases still run the existing class-sensitive lookup logic, so
native override priority and descriptor compatibility behavior are unchanged.
The full Hibernate `FunctionTests` / `StandardFunctionTests` rerun is still
outstanding, so keep this slowness cluster open. Focused prefilter and hash-key
regressions were re-run green on 2026-07-01; app-level timing remains the missing
acceptance signal.

### 2026-07-01 follow-up: intrinsic signature prefilter

Implemented the same cheap-prefilter shape for interpreter intrinsics. The
vtable-fast path and virtual invoke-cache population now first ask whether the
`(method_name, descriptor)` pair can possibly be intrinsic before walking the
class hierarchy to find the resolved declaring class. A `false` result skips the
`class_manager` read + `find_method_recursive` intrinsic guard entirely; a
`true` result still performs the existing declaring-class lookup and exact
`intrinsics::lookup`, so overridden subclass behavior and JVMTI redefine
suppression remain unchanged.

This should reduce another warm-miss cost for Hibernate/framework call sites
whose signatures are not in the tiny intrinsic table. It is still a mitigation,
not closure: the full Hibernate `FunctionTests` / `StandardFunctionTests`
timing rerun remains outstanding.
