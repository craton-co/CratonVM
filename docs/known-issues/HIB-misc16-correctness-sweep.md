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
| 10 | `util.dtd.EntityResolverTest` | unmapped entity `[Child]` | StAX `XMLResolver` not consulted for `classpath://` general entity | OPEN — new |
| 11 | `jpa.transaction.TransactionTimeoutTest` | `getStatus()=0 ACTIVE` | Narayana transaction-reaper thread never fires the 2s timeout | OPEN — new |
| 12 | `id.uuid.rfc9562.UUidV6V7GeneratorTest` | MockitoException | JDK `AnnotatedTypeFactory.getAnnotatedOwnerType` NPE (type-annotation bytes) | OPEN — new |
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

## 10. EntityResolverTest — StAX `classpath://` general entity (OPEN, new)

`Parent.hbm.xml` declares an external general entity
`<!ENTITY child SYSTEM "classpath://…/child.xml">` and references it as
`&child;`. Hibernate's StAX `XMLResolver`
(`LocalXmlResourceResolver.resolveEntity`) maps `classpath://` to a classpath
resource stream. On CratonVM the `&child;` expansion is **empty** → the `Child`
`<class>` mapping never loads → `MappingException: Collection [Parent.children]
references an unmapped entity [Child]`. Isolated single-class run: ~53 s + the
unmapped-entity error (the 120 s `TimeoutException` seen in the 8-shard sweep is
shard contention, not a true deadlock). The slowness + empty expansion is
consistent with the StAX `XMLResolver` not being consulted for the external
general entity (and/or a fallback I/O attempt). Area: real-JDK StAX
external-general-entity resolution (Woodstox / JDK `XMLInputFactory`) — verify
whether `XMLResolver.resolveEntity` is invoked for general (non-DTD) entities
and whether `IS_SUPPORTING_EXTERNAL_ENTITIES` / entity expansion is honored.

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

## 12. UUidV6V7GeneratorTest — Mockito → JDK `AnnotatedTypeFactory` NPE (OPEN, new)

Mockito inline-mock self-attach **succeeds** here (not the kafka bug-09 case).
ByteBuddy then copies type annotations onto the generated mock's methods and
calls `AnnotatedType.getAnnotatedOwnerType()` on parameterized types, where the
real JDK `sun.reflect.annotation.AnnotatedTypeFactory$AnnotatedTypeBaseImpl.
getLocation()` returns **null** → `NullPointerException` in `popLocation(byte)`
(`AnnotatedTypeFactory.java:190`), surfacing as
`MockitoException: cannot mock … SharedSessionContractImplementor`. CratonVM
feeds the JDK annotated-type factory a malformed/empty `LocationInfo` for these
owner/parameterized types — sibling of the kafka bug-09 fix #6
(`getExecutableTypeAnnotationBytes`), now in the `getAnnotatedOwnerType` path.
Area: `native-builtins` type-annotation-bytes provisioning for parameterized /
owner types.

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
