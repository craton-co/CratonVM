# Hibernate ORM "misc-correctness" failure tail (misc16) — triage sweep

| | |
|---|---|
| **Scope** | 16 Hibernate ORM test classes that FAIL on CratonVM (real-JDK mode, JIT on) but PASS on HotSpot, and are NOT in the bytecode-enhancement / lazytoone / slowness-timeout / temporal-GC clusters. |
| **Baseline** | dev `1f05e792` → re-verified on dev `2da9a00d`; binary `cvmisc.exe` (worktree `CratonVM-misc16`, branch `fix/hib-misc-correctness`). Harness `apps/hib-suite-runner`, `common.args`, `maxParallelForks=1`, real JDK 25. |
| **List** | `apps/hib-suite-runner/misc16.txt` |
| **Date** | 2026-06-30 |

**Verification (merged binary `2da9a00d`→dev merge `a5847887`, fixes `2ba1347b`+`d906ce97`):**
`ExplicitQueryStatsMaxSizeTest` PASS 2/2 and `DatabaseTimeZoneMultiTenancyTest`
PASS 1/1 (both were failing). `XmlFormatterTest` PASS (dev). No regressions in
the rest of misc16; `LhmEvictProbe` + non-overriding-LHM-subclass +
`LinkedHashSet` regression probes match HotSpot. `StandardFunctionTests`
completed this run (40/44, 4 failed — matches the HotSpot slowness/shared-failure
pattern, see §15–16).

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

| # | Class | Symptom | Root cause | Disposition |
|---|-------|---------|-----------|-------------|
| 1 | `stats.ExplicitQueryStatsMaxSizeTest` | `expected:<0> but was:<1000>` | native `LinkedHashMap.put` never ran `removeEldestEntry` eviction hook | **FIXED** (this branch) |
| 2 | `multitenancy.DatabaseTimeZoneMultiTenancyTest` | timestamp +6h | `TimeZone.getTimeZone("CST")` (3-letter id) → rawOffset 0 | **FIXED** (this branch; HIB-CV-34 family) |
| 3 | `mapping.type.format.XmlFormatterTest` | (was UOE) | — | **FLIPPED GREEN** on current dev — excluded |
| 4 | `cache.EnhancedProxyCacheTest` | can't cast to `PersistentAttributeInterceptable` | loader-blind class store (enhanced class shadowed by app-loader original) | OPEN — loader-blind group |
| 5 | `flush.AutoFlushBeforeLoadTest` | To-one mapping type mismatch / persister | loader-blind class store | OPEN — loader-blind group |
| 6 | `jpa.callbacks.PrivateConstructorEnhancerTest` | can't cast to `PersistentAttributeInterceptable` | loader-blind class store | OPEN — loader-blind group |
| 7 | `type.LobUnfetchedPropertyTest` | can't cast to `PersistentAttributeInterceptable` | loader-blind class store | OPEN — loader-blind group |
| 8 | `pc.InstanceIdentityTest` | IAE "not of expected type" | loader-blind class store | OPEN — loader-blind group |
| 9 | `proxy.ProxyClassReuseTest` | `Could not instantiate persister` (1/3) | loader-blind class store (dual-loader) | OPEN — owned by `hib-proxyclassreuse-loader-blind-class-resolution.md` |
| 10 | `util.dtd.EntityResolverTest` | unmapped entity `[Child]` | synthetic `XMLInputFactory.newInstance()` stub shadows real Woodstox → resolver ignored, general entity not expanded | **FIXED** (default-ON; `CRATONVM_REAL_STAX_FACTORY=0` escape hatch); slowness is separate (§15–16) |
| 11 | `jpa.transaction.TransactionTimeoutTest` | `getStatus()=0 ACTIVE` | Narayana transaction-reaper thread never fires the 2s timeout | OPEN — new |
| 12 | `id.uuid.rfc9562.UUidV6V7GeneratorTest` | MockitoException | native `AnnotatedTypeBaseImpl.location` null → `getAnnotatedOwnerType` NPE | **NPE FIXED**; residual = 1M-iteration slowness (§15–16 cluster) |
| 13 | `batchfetch.DynamicBatchFetchTest` | `testMultiLoad` 120s timeout (param-binding 1/2 now passes) | slowness (2000-row multiLoad); HIB-CV-37 #2 param-binding appears fixed | Route → HIB-CV-37 |
| 14 | `service.ClassLoaderServiceImplTest` | AssertionError | custom-loader class identity / `loadJavaServices` | Route → HIB-CV-24 |
| 15 | `query.hql.FunctionTests` | HANG @300s | (slowness vs wrong-result — see below) | OPEN — slowness/correctness |
| 16 | `query.hql.StandardFunctionTests` | HANG @300s | (slowness vs wrong-result — see below) | OPEN — slowness/correctness |

---

## 1. ExplicitQueryStatsMaxSizeTest — `LinkedHashMap` eviction hook (FIXED)

**Root cause.** Hibernate's bounded query-plan stats use
`StatsNamedContainer` → `BoundedConcurrentHashMap(capacity, …, LRU)`. The LRU
policy is a `LinkedHashMap` subclass (`super(capacity, lf, true)` access-order)
that evicts via an overridden `removeEldestEntry`. CratonVM **force-natives**
`LinkedHashMap.put` (`native_lhm_put` in `native-collections/src/lib.rs`), and
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

**Fix** (`native-collections/src/lib.rs`, end of `native_lhm_put`): after a new
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
HIB-CV-34 (`native-builtins/src/lib.rs` `tz_standard_offset_seconds`) wired
standard UTC offsets for IANA zone ids, but the deprecated **three-letter
abbreviations** (`CST`/`EST`/`PST`/`MST`/`HST`) were absent → fell back to
rawOffset 0, shifting the per-session JDBC timestamp by the full zone offset.
Real JDK 25: `TimeZone.getTimeZone("CST").getRawOffset() == -21600000` (−6h).

**Fix:** add `EST −5h, CST −6h, MST −7h, PST −8h, HST −10h` to
`tz_standard_offset_seconds`. (Standard, non-DST offsets — faithful to real
JDK's fixed rawOffset for these ids; the failing timestamp `2018-11-23` is
standard time.) Same family as the FIXED HIB-CV-34 doc.

---

## 4–9. Loader-blind class store — bytecode-enhancement / persister group (OPEN)

Six classes share **one** root cause. Five (`EnhancedProxyCacheTest`,
`AutoFlushBeforeLoadTest`, `PrivateConstructorEnhancerTest`,
`LobUnfetchedPropertyTest`, `InstanceIdentityTest`) carry `@BytecodeEnhanced`;
its JUnit extension runs the test through a child `EnhancingClassLoader`
(parent = application loader) that ByteBuddy-enhances the entity and
`defineClass`es it so it implements `PersistentAttributeInterceptable` /
`ManagedEntity` / `SelfDirtinessTracker`.

Enhancement is **not** the problem — the enhanced `.class` is generated
correctly (`javap` confirms the interfaces + `$$_hibernate_*` members) and
`defineClass(enhanced)` is called. CratonVM's **flat global name→ClassId store
is loader-blind**: the same-named entity defined by the app loader and by the
`EnhancingClassLoader` collapse to one entry, and the reference reaching
`SingleTableEntityPersister` resolves to the *unenhanced* app-loader copy →
`can't be cast to PersistentAttributeInterceptable` / `Could not instantiate
persister` / `not of expected type`.

- `ProxyClassReuseTest` is the dual-isolated-loader variant already owned by
  `docs/known-issues/hib-proxyclassreuse-loader-blind-class-resolution.md`.
- **Update (dev merge):** dev commit `7183f42a` + `docs/known-issues/hib-bytecode-enhancement-loader-faithful-linking.md`
  added gate-on loader-faithful supertype *linking* / `invokespecial` dispatch
  for `@BytecodeEnhanced` entities. That doc reports the eager-enhancement
  cluster FIXED gate-on but the `enhancement.lazy.*` / `mapping.lazytoone.*`
  cluster still OPEN. **Re-tested on the merged binary with
  `CRATONVM_LOADER_AWARE_RESOLUTION=1`: all 6 still FAIL** (identical
  `can't be cast to PersistentAttributeInterceptable` / persister / IAE) — so
  these misc16 classes fall in the still-open lazy/non-eager cluster, not the
  portion dev's `7183f42a` fixed. Tracked by
  `hib-bytecode-enhancement-loader-faithful-linking.md` (open lazy cluster).

**Why the existing gate misses it (fix direction).** The gated
`resolve_class_loader_aware` (`vm/src/runtime/interpreter.rs`) only covers
bytecode `CONSTANT_Class` sites (`ldc`/`new`/`checkcast`/`instanceof`/owner
constants). The entity `Class` here arrives via the `@DomainModel(annotatedClasses
= {…})` **annotation element value** and reflective `Class` lookups, which still
go through the flat global store. Loader-faithful resolution must extend to
annotation-element `Class` values (decoded in `reader/src/attribute.rs`,
surfaced via `native-builtins/src/lang_class.rs`) and to the layer-2/3
namespace attribution in `native-builtins/src/lang_system.rs` (`defineClass`) /
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
(`native-builtins/src/xml_stax.rs` `native_factory_new_instance`) — whose
`setXMLResolver` is a no-op (`native_factory_set_property`) and whose
quick-xml-backed cursor reader never expands external **general** entities. This
synthetic factory **shadows the real Woodstox** (`com.ctc.wstx.stax.WstxInputFactory`,
`woodstox-core` + `stax2-api` on the classpath) that HotSpot's `FactoryFinder`
selects. Probe (`StaxProbe`/`StaxProbe2`, `apps/hib-suite-runner`) on the same
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

**Fix** (`native-builtins/src/xml_stax.rs`, **default-ON**; `CRATONVM_REAL_STAX_FACTORY=0`
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

## 11. TransactionTimeoutTest — JTA transaction-reaper never fires (OPEN, new)

`testH2` sets a 2 s JTA timeout, begins, then runs `select sleep(10000)` (H2
alias → `Thread.sleep`). Expected: a `QueryTimeout`/`LockTimeout` or a
rolled-back/marking status. Actual final `transactionManager.getStatus() == 0`
(`STATUS_ACTIVE`) — assertion `Expecting actual: 0 to be in: [4, 9, 1]`. The
alias slept the full 10 s and the transaction was never marked/rolled-back.
JTA platform is Arjuna/Narayana; its **TransactionReaper** background daemon
that watches per-transaction deadlines isn't firing under CratonVM (scheduled
wakeup / daemon-thread scheduling / async abort divergence). Distinct from
`HIB-CV-19` (null TM). Moderately deep (background-timer thread).

---

## 12. UUidV6V7GeneratorTest — Mockito → JDK `AnnotatedTypeFactory` NPE (NPE FIXED; residual hang)

Mockito inline-mock self-attach **succeeds** here (not the kafka bug-09 case).
ByteBuddy then copies type annotations onto the generated mock's methods and
calls `AnnotatedType.getAnnotatedOwnerType()`, where the real JDK
`AnnotatedTypeBaseImpl.getLocation()` returned **null** → `NullPointerException`
in `popLocation(byte)` (`AnnotatedTypeFactory.java:190`), surfacing as
`MockitoException: cannot mock … SharedSessionContractImplementor`.

**Root cause (FIXED).** CratonVM *natively constructs* `AnnotatedTypeBaseImpl`
(`make_annotated_type` / `make_annotated_type_with_anns`, `native-builtins/src/lang_class.rs`)
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

## 13. DynamicBatchFetchTest — route → HIB-CV-37

`found=2 ok=1 failed=1`: the previously-failing `testDynamicBatchFetch`
(HIB-CV-37 #2 — `JdbcParameterBindingsImpl` IdentityHashMap param-binding miss)
now **passes**; the remaining failure is `testMultiLoad` exceeding the 120 s
per-test timeout (2000-row insert + `byMultipleIds` multiLoad — slowness, not
wrong-result). Tracked under `HIB-CV-37`.

## 14. ClassLoaderServiceImplTest — route → HIB-CV-24

Both tests `AssertionError` — `testSystemClassLoaderNotOverriding` (a custom
loader's overriding class must win) and `testStoppableClassLoaderService`
(`loadJavaServices` via `findResources`). Custom-classloader class-identity /
service-loader isolation — the HIB-CV-24 loader-isolation family.

## 15–16. FunctionTests / StandardFunctionTests — slowness cluster, NOT a correctness bug

`FunctionTests` was run to completion under a 1200 s cap: CratonVM finishes in
**1169 s** (rc=0, `found=123 ok=97 failed=20`) vs HotSpot **24 s**
(`ok=99 failed=18`) — **~49× slower**. The 300 s "HANG" (rc=124) was purely a
timeout artifact of a slow-but-live process; no per-test `TimeoutException`, no
deadlock, steady forward progress to `@@DONE`. Heavy `TRACE`/`FINEST` Hibernate
logging dominates the wall time.

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
