# HIB-CV-35 — CratonVM-only Hibernate correctness divergences (long-tail)

## Validated scorecard (clean binary `cvh35.exe`, worktree `fix/hib-cv-35` @ dev)

| Test | Inventory | After fixes | Disposition |
|---|---|---|---|
| util.SerializationHelperTest | CNFE (1/2) | **2/2 PASS** ✅ | FIXED — getResource parent delegation |
| stateless.StatelessSessionPersistentContextTest | fail (0/1) | **1/1 PASS** ✅ | FIXED — emptyIterator singleton |
| proxy.ProxyClassReuseTest | CNFE (2/3) | 2/3 | CNFE root FIXED; residual `testNoReuse` = classloader **isolation** (HIB-CV-24) |
| bootstrap.scanning.JarVisitorTest | CRASH | **9/9 PASS** ✅ | Was the `[HIB32]` flood (watchdog) — not a real bug |
| sorted.set.SortComparatorTest | fail (0/1) | 0/1 | OPEN — persist-side dedup (1-of-2 inserted); core collections OK |
| sorted.set.SortNaturalTest | fail (0/1) | 0/1 | OPEN — same as above |
| mapping.type.format.XmlFormatterTest | UOE (10/12) | 10/12 | OPEN — `byte[][]`→CollectionJavaType; `isArray` is correct |
| util.PropertiesHelperTest | UOE (1/2) | **2/2 PASS** ✅ | FIXED — Properties live entrySet (STATIC view) |
| stats.ExplicitQueryStatsMaxSizeTest | 0≠1000 (1/2) | 1/2 | OPEN — BoundedConcurrentHashMap LRU eviction |
| mapping.fetch.depth.NoDepthTests | .par URL (2/4) | 2/4 | OPEN — ShrinkWrap in-memory archive URL |
| onetoone.nopojo.DynamicMapOneToOneTest | LOADERR | HANG | OPEN — `--nojit` hbm.xml-mapping bootstrap hang (HIB-CV-23 class) |

**Fixes landed on branch `fix/hib-cv-35`** (build with `CARGO_PROFILE_RELEASE_LTO=false
CARGO_PROFILE_RELEASE_CODEGEN_UNITS=256` to avoid OOM under IDE/agent contention):
1. **getResource / getResourceAsStream parent delegation** (`native-builtins/src/classloader.rs`) — §3.
2. **`Collections.emptyIterator()` singleton** (`native-builtins/src/phases_early.rs` + `native-collections/src/lib.rs`) — §4.

---

**Status:** PARTIALLY FIXED (2 tests fixed + 1 CNFE root + 1 flood-debunked; 6 deep ones documented)
**Date:** 2026-06-23
**Baseline:** dev (inventory generated on f8cdd52b; retest current dev)
**Mode:** all reproduce under `--nojit`; all PASS on HotSpot
**Inventory:** [`INVENTORY-cvonly-real-fails.tsv`](INVENTORY-cvonly-real-fails.tsv)
**Harness:** from `apps/hibernate-orm/.cratonvm-suite`,
`<cv> --nojit @common.args -Dcraton.trace=1 CratonRunner <listfile> 0`

This document triages the still-failing long-tail of CratonVM-only correctness
divergences in the Hibernate ORM test suite, grouped by root cause. Each entry
records the symptom, the reproduction, the root cause (located in VM source),
the supporting evidence, and a recommended fix with a confidence level.

---

## 0. Environment note — leftover debug flood + concurrent-agent contention

Two environment issues were found and must be recorded:

### 0a. Unconditional `[HIB32]` getfield diagnostic (working-tree regression)

The working tree carried **uncommitted** debug instrumentation in
`vm/src/runtime/interpreter.rs` (the `Getfield` handler, ~line 11231):

```rust
// HIB-CV-32 DIAGNOSTIC: detect a corrupt `value` ...
{
    let raw: [u64; 2] = unsafe { std::mem::transmute_copy(&value) };
    if (raw[0] as u32) > 6 { eprintln!("[HIB32] CORRUPT getfield value ..."); ... }
}
```

This block was **unconditional** and ran on every `getfield`. Its
`(raw[0] as u32) > 6` heuristic false-positives on ordinary tagged `Value`s, so
it floods stderr (millions of lines) on normal JUnit/log4j init. The flood slows
execution enough to trip the 120 s stack-dump watchdog → tests that are actually
*correct-but-slow* abort and look like crashes/hangs.

**Fix applied** (main worktree): gated behind a new cached env flag
`CRATONVM_DIAG_HIB32` (default off), matching the adjacent debug gates:
- `vm/src/runtime/env_cache.rs`: `cached_is_set!(diag_hib32, "CRATONVM_DIAG_HIB32");`
- `vm/src/runtime/interpreter.rs`: `if crate::runtime::env_cache::diag_hib32() { ... }`

NB: this flood is **not present on committed dev** (the diagnostic was uncommitted
WIP), so it does not affect the inventory baseline — but any build off this dirty
tree is unusable without the gate.

### 0b. Concurrent agent in the same worktree

A second agent (`cratonvm-rerun0623`) was running the same rerun in the same
working tree, editing the same 14 WIP `.rs` files and (with IntelliJ's background
cargo) contending for the build lock, the global `~/.cargo/.package-cache` lock,
and memory — cross-killing builds. Validation builds were performed in an
isolated worktree (`fix/hib-cv-35` at dev) with uniquely-named binaries to
sidestep this; where a clean-binary rerun was not yet possible, findings below are
marked accordingly. Reproductions captured **before** this contention (and even
through the flood) are noted as CONFIRMED.

---

## 1. Sorted-set/-map ordering — collection hydration dedups distinct elements

> **✅ FIXED 2026-06-24 (commit 9ac7ef1d, branch `fix/hib-sortnatural-cascade`).
> The root cause below was WRONG.** It is NOT an `invokeinterface
> Comparable.compareTo` mis-dispatch and NOT ByteBuddy proxy / SF-build
> corruption. The real cause: the native `TreeSet`/`TreeMap` natural-order
> comparator (`native-collections::natural_compare`) probed **field 0** of each
> element as a primitive — so an entity with `@Id @GeneratedValue long id`
> (id==0 on fresh instances) compared "equal" and dedup'd at `add()` time,
> before any persist. The `long` vs boxed `Long` id type is what made the
> `@SessionFactory` repro fail while the `MetadataSources` repro passed. Fix:
> gate the fast path on `unbox_wrapper`. See
> [`hib-sortnatural-persistentsortedset-cascade-drop.md`](../hib-sortnatural-persistentsortedset-cascade-drop.md).
> All four `sorted.{set,map}.{SortNatural,SortComparator}Test` now pass.

**Tests:** `org.hibernate.orm.test.sorted.set.SortComparatorTest`,
`org.hibernate.orm.test.sorted.set.SortNaturalTest`
(sibling `sorted.map.*` share the cause)

**Symptom (CONFIRMED, reproduced):**
`assertThat(owner.cats.size()).isEqualTo(2)` → `expected: 2 but was: 1`.
After persisting two distinct children and reloading, the `TreeSet`/`TreeMap`
collection holds **1** element instead of 2.

**Not the core library:** a standalone probe (`TreeSet<String>` natural +
`String.CASE_INSENSITIVE_ORDER`, and `TreeMap` keyed by distinct strings
`"A"`/`"B"`) is **byte-identical to HotSpot** (size 2, correct first/last,
`"A".compareTo("B") = -1`, `CIO.compare("B","a") = 1`). So `TreeSet`/`TreeMap`
ordering and `String.compareTo`/`CASE_INSENSITIVE_ORDER` are correct.

**ROOT CAUSE (precisely localized): `invokeinterface Comparable.compareTo`
mis-dispatches to a 0-returning target for an `@Entity` class, *after Hibernate's
`SessionFactory` build-time processing of that class* — so the user's own
`TreeSet<Cat>` dedups two distinct cats at the SOURCE (before any persist).**

A self-contained JUnit reproduction (`@DomainModel`+`@SessionFactory`, entity `Cat
implements Comparable<Cat>`) shows, inside the test body (SF already built):
```
DIAG GenCmp(non-entity, identical structure) size=2   <- OK
DIAG Cat(ENTITY)                              size=1   <- BUG   (k1.compareTo(k2)=1 — direct call CORRECT)
DIAG RawCmp(raw Comparable, no bridge)        size=2   <- OK
DIAG String / Integer                         size=2   <- OK
```
`new TreeSet<Cat>(); add(B); add(A)` → **size 1**: the second `add` returns
`false`. Real JDK `TreeMap.put` runs (TreeSet/TreeMap are NOT native on CratonVM),
so its `((Comparable)key).compareTo(t.key)` — an `invokeinterface
Comparable.compareTo(Object)` — returns **0** for two distinct `Cat`s.

Decisively ruled out:
- Core collections: `TreeSet`/`TreeMap`/`IdentityHashMap`/`HashSet` of two distinct
  `Cat`s → size 2 **standalone** (== HotSpot). Array `Class` reflection too.
- The synthetic-bridge `compareTo(Object)`: a *non-entity* `GenCmp` with the
  identical `Comparable<T>`+bridge shape works (size 2). A raw `Comparable`
  (no bridge) works.
- Megamorphic call-site pollution: warming `TreeMap.put`'s `compareTo` call site
  with 5 Comparable types × 2000 iters does NOT reproduce it standalone.
- Reflection on the class (`getMethods`/`getMethod("compareTo",Object)`/bridge
  invoke) does NOT corrupt dispatch standalone.
- GC: big heap (`-Xmx512m`) and `CRATONVM_FORCE_MOVING=1` do NOT help (so NOT the
  HIB-CV-33 load-sensitive corruptor).
- Direct `((Comparable)cat).compareTo(other)` from app code returns the correct
  value even in the failing context — only the *internal* `TreeMap.put` dispatch
  is wrong.

So **building the `SessionFactory` over the `Comparable` entity corrupts that
class's method-table / itable entry for `compareTo`** (only that class — sibling
non-entity Comparables in the same run are fine). Manual `MetadataSources`
bootstrap does NOT trigger it; the `@DomainModel`/`@SessionFactory` extension path
does — prime suspect is the ByteBuddy proxy/instantiator generation for the entity
(Hibernate makes `Cat$HibernateProxy` subclasses; cf. ProxyClassReuseTest), which
may rewrite/relocate the entity's `compareTo` itable slot on CratonVM.

**Next step:** bisect the SF-build steps for an entity (BytecodeProvider proxy
generation vs instantiator vs JavaType registration); instrument the interpreter's
`invokeinterface` resolution for `Comparable.compareTo` on the entity class to see
which target it picks after each step. Repro: `SortDiagTest` in the suite scratch
(self-contained).

**Confidence:** root cause HIGH and precisely reproduced; exact SF-build corruptor
MEDIUM (bisection pending). Deep interpreter/ByteBuddy interaction — deferred.

---

## 2. `UnsupportedOperationException` where HotSpot succeeds

### 2a. `util.PropertiesHelperTest` — `Properties.entrySet()` is not a live view — ✅ FIXED

> **✅ FIXED 2026-06-29 (branch `fix/hib-cv-35-properties`). PropertiesHelperTest
> 1/2 → 2/2.** `Properties.entrySet()` now returns a **live STATIC entrySet view**
> backed by the Properties object: each element is a 3-field
> `java/util/Map$Entry` (key, value, sourceMap=this) so `entry.setValue(v)`
> writes through to the side-table (`native_entry_set_value` dispatches the
> backing `put` virtually for a Hashtable/Properties source — new
> `is_hashtable_receiver`), and the set's `iterator().remove()` / `remove(entry)`
> / `clear()` propagate to the side-table via the view backing's virtual
> `remove`/`clear`. A new view-kind `VIEW_KIND_ENTRYSET_STATIC` materialises the
> view **once** and never resyncs from the source — a normal resyncing entrySet
> view would walk `Properties.entrySet()` on every `iterator()`/`size()` and
> recurse (the side-table is not a natively-readable HashMap bucket array).
> Keying write-through on the view backing (not the entries' source field) keeps
> a later `new HashSet<>(props.entrySet())` copy correctly detached on `remove`.
> Verified: 2 standalone repros 9/9 + 12/12 == HotSpot (incl. copy-detachment,
> set.remove boolean, contains, clear), 2710 native unit tests, SerializationHelper
> / StatelessSession still pass.

**Symptom (CONFIRMED, reproduced):**
`UnsupportedOperationException` at
`java.util.AbstractMap$SimpleImmutableEntry.setValue` ←
`ConfigurationHelper.resolvePlaceHolders` (`ConfigurationHelper.java:350`).

**Root cause:** CratonVM's synthetic `Properties` keeps entries in a Rust
side-table (`native-builtins/src/properties_sidetable.rs`) because `new
Properties()` leaves the JDK-internal `map` CHM field null. Its
`native_properties_entry_set` returns a `HashSet` of detached
`AbstractMap$SimpleImmutableEntry` objects. `resolvePlaceHolders` iterates the
entry set and calls `entry.setValue(resolved)` (and, for `"${}"`,
`iterator.remove()`), expecting a **live** entrySet view (JDK `Hashtable`/
`Properties` semantics). `SimpleImmutableEntry.setValue` throws by contract; even
a mutable `SimpleEntry` would be a detached copy, so neither `setValue` nor
`iterator.remove()` would write back to the side-table, and the later
`getProperty`/`getInt` reads would see stale/unresolved values.

Probe (this VM vs HotSpot):
```
Properties.entrySet() entry class = AbstractMap$SimpleImmutableEntry  (HotSpot: ConcurrentHashMap$MapEntry)
  setValue -> THREW UnsupportedOperationException                     (HotSpot: OK, writes through)
Hashtable.entrySet()/HashMap.entrySet() setValue -> OK on both
```

**Recommended fix:** make `Properties.entrySet()` return a **live** view whose
`setValue` and `iterator.remove()` write through to the side-table. The
side-table mutators already exist (`put_kv`, `remove_kv` in
`properties_sidetable.rs`). Implementation options:
1. Return live `java/util/Map$Entry` (3-field: key, value, sourceMap=Properties)
   entries and teach the shared `native_entry_set_value`
   (`native-collections`) to route write-through for a `Properties` source via
   `invoke_virtual "put"` (→ `native_properties_put` → side-table) instead of the
   direct `native_map_put` (which targets the synthetic HashMap buckets the
   side-table does not read). Plus a Properties-aware live `iterator.remove()`.
2. Longer-term/cleaner: initialize the real CHM `map` on `new Properties()` and
   route real JDK `Properties` bytecode, retiring the side-table — large blast
   radius (Spring/Kafka/Keycloak depend on the side-table today), so gate it.

**Confidence:** root cause HIGH; fix (1) is contained but requires a small,
carefully-scoped change to a shared `native-collections` function.

### 2b. `mapping.type.format.XmlFormatterTest` — array `JavaType` mis-resolved to `CollectionJavaType`

**Symptom (CONFIRMED, reproduced):**
`UnsupportedOperationException` at
`CollectionJavaType.wrap` (`CollectionJavaType.java:91`) ←
`Jackson{,3}XmlFormatMapper.fromString` ← `testByteArray` (`byte[][]`, `byte[]`).

**Root cause:** `CollectionJavaType.wrap`/`unwrap` throw by design — they are
*never* meant to be reached for an array. `JavaTypeRegistry.getDescriptor(
value.getClass())` for `byte[][]` must resolve to an `ArrayJavaType`. The
resolver (`JavaTypeRegistry.resolveDescriptor(Type)`) only takes the array branch
when `javaClass.isArray()` is true; otherwise it builds a basic/collection
descriptor whose `wrap` throws. The divergence therefore hinges on multi-dimensional
array **`Class` reflection** — `byte[][].class.isArray()` / `getComponentType()` /
`getTypeName()`.

**Array reflection RULED OUT (probe == HotSpot):**
```
byte[]   isArray=true comp=byte      typeName=byte[]   name=[B
byte[][] isArray=true comp=class [B  typeName=byte[][] name=[[B
String[] isArray=true comp=String    typeName=java.lang.String[]
int[][]  isArray=true comp=class [I  typeName=int[][]
```
All identical to HotSpot. So `JavaTypeRegistry.resolveDescriptor`'s
`javaClass.isArray()` branch *should* fire — the mis-resolution is **elsewhere**:
likely a stale/pre-seeded entry in `descriptorsByTypeName` keyed by `"byte[][]"`
(or a divergent `createArrayTypeDescriptor`/`findDescriptor` for the `byte[]`
element), so `getDescriptor(byte[][])` returns a cached `CollectionJavaType`
without running the array creator. Only `testByteArray` (the `byte[][]` cases)
fails; the other 10 subtests pass.

**Next step:** dump `getDescriptor(byte[][].class).getClass()` and inspect
whether `descriptorsByTypeName` already holds `"byte[][]"`→CollectionJavaType
before the array path runs.

**Confidence:** NOT array reflection (proven). Resolution/cache path — MEDIUM.
Deep, niche (Jackson-XML byte-array round-trip) — deferred.

---

## 3. Custom `ClassLoader.getResource(AsStream)` skipped parent delegation — ✅ FIXED

**Tests:** `org.hibernate.orm.test.util.SerializationHelperTest`
(CNFE `...util.SerializableThing`),
`org.hibernate.orm.test.proxy.ProxyClassReuseTest`
(CNFE `...ProxyClassReuseTest$ProxyGetter`)

**Symptom:** `ClassNotFoundException`. Both tests use a custom `ClassLoader`
subclass that loads a class by reading its bytes via
`this.getResourceAsStream(name.replace('.','/') + ".class")` and `defineClass`;
`getResourceAsStream` returned **null** for a `.class` on the app classpath.

**Root cause (isolated by probe):** `cl_get_resource`
(`native-builtins/src/classloader.rs`), for a non-builtin (user) loader, called
`findResource()` **directly**, skipping the JDK `ClassLoader.getResource`
parent-delegation step. A custom loader that overrides only `loadClass` inherits
the default `findResource` (returns null) → every parent-served resource reported
null. Probe (custom loader, parent = system):
```
sys.getResource           = file:/.../SerializableThing.class   (worked)
custom.getResource        = null                                (BUG; HotSpot: the same URL)
custom.getResourceAsStream= false                               (BUG; HotSpot: true)
```

**Fix (landed):** in the user-loader branch of `cl_get_resource`, delegate to
`parent.getResource(name)` FIRST (the JDK contract), then fall back to this
loader's `findResource`. `cl_get_resource_as_stream` mirrors JDK
(`getResource(name).openStream()`) for user loaders so it inherits the same
delegation. **Validated:** `custom.getResource(AsStream)` now resolve; **
SerializationHelperTest 2/2 PASS**.

**Residual:** `ProxyClassReuseTest.testNoReuse` no longer throws CNFE but now
fails a *different* assertion — two `IsolatingClassLoader`s produce the **same**
proxy class (`assertNotSame` fails). That is genuine classloader **isolation**
(each isolated loader should `defineClass` its own copy), tracked under
[HIB-CV-24](HIB-CV-24-classloader-isolation-delegation.md) — out of scope here.

---

## 4. `Collections.emptyIterator()` not a singleton — ✅ FIXED

**Test:** `org.hibernate.orm.test.stateless.StatelessSessionPersistentContextTest`

**Symptom (inventory):** `"StatelessSession: PersistenceContext has not been
cleared" expected: <true> but was: <false>`.

**Root cause (isolated by probe):** the cleared-context assertions use
**reference identity** against JDK singletons:
```java
managedEntitiesIterator() == Collections.emptyIterator()   // the failing one
getCollectionsByKey()      == Collections.EMPTY_MAP         // (EMPTY_MAP was already OK)
```
Probe: `Collections.emptyIterator() == Collections.emptyIterator()` → **false** on
CratonVM (true on HotSpot); `EMPTY_MAP`/`emptyMap()` were already stable. The
`emptyIterator` native (TWO registrations: `phases_early.rs:551` — the active
one — and `native-collections`'s `native_empty_iterator`) allocated a **fresh**
`Collections$EmptyIterator` per call.

**Fix (landed):** return the process-wide singleton stored in the (GC-rooted)
`Collections$EmptyIterator.EMPTY_ITERATOR` static field, populating it lazily on
first use — the same mechanism `Collections.EMPTY_MAP` already uses
(`collections_empty_singleton`). Applied to BOTH registrations
(`ensure_class_initialized` first so the class id resolves cold-start).
**Validated: StatelessSessionPersistentContextTest 1/1 PASS.**

Note: array `Class` reflection (`isArray`/`getComponentType`/`getName`/
`getTypeName` for `byte[]`/`byte[][]`/`String[]`/`int[][]`) is **byte-identical to
HotSpot** — see §2b; that ruled out the original array-reflection hypothesis.

---

## 5. `stats.ExplicitQueryStatsMaxSizeTest` — bounded query-stats cache doesn't evict

**Symptom (inventory):** `expected: <0> but was: <1000>`. With
`QUERY_STATISTICS_MAX_SIZE=100`, after recording 210 distinct queries the early
entry `"1"` should have been evicted (its `getExecutionTotalTime()` → 0 for a
fresh entry); on CratonVM it still reports `1000` (the original value), i.e. the
bounded LRU **did not evict**.

**Root cause:** Hibernate's query-statistics map is a bounded LRU
(`org.hibernate.internal.util.collections.BoundedConcurrentHashMap`). Eviction is
not occurring on CratonVM — either the eviction policy's accounting (LRU/LIRS
recency lists, segment sizing) relies on a behavior that diverges, or a
concurrency primitive the bounded map depends on behaves differently.

**Next step:** isolate with a standalone `BoundedConcurrentHashMap` probe
(put > maxSize entries; assert size stays ≤ maxSize and the eldest is gone).

**Confidence:** MEDIUM (data-structure behavior; needs isolation probe).

---

## 6. Runtime-built `.par` archive URL / archive scanning

**Tests:** `mapping.fetch.depth.NoDepthTests`
(`Could not create URL for archive: fetch-depth.par`),
`bootstrap.scanning.JarVisitorTest`

**Symptom (inventory):** `NoDepthTests` JPA variants
(`testWithMaxJpa`/`testNoMaxJpa`) build an in-memory ShrinkWrap `JavaArchive`
("fetch-depth.par") + `ShrinkWrapClassLoader` and call
`Persistence.createEntityManagerFactory("fetch-depth", ...)`; Hibernate's archive
scanning fails to construct a URL for the ShrinkWrap virtual archive. The non-JPA
variants (`testWithMax`/`testNoMax`) do not use ShrinkWrap and should pass.
`JarVisitorTest` builds `.par`/`.ear` archives at runtime and scans them via the
`jar:`/nested-`jar:` URL protocols.

**Root cause (hypothesis):** CratonVM's URL/stream-handler + archive-descriptor
path does not support (a) the ShrinkWrap in-memory archive URL scheme, and/or
(b) runtime-built jar/ear `jar:...!/...` nested-archive URL resolution that
Hibernate's `StandardArchiveDescriptorFactory` requires. Related to the URL
stream-handler/factory work in HIB-CV-15 and the `classpath:`/URL-factory fixes.

**Confidence:** LOW–MEDIUM; niche (in-memory archive URL handling). Recommend
deferring behind the higher-value fixes above.

---

## Crash variants — re-evaluated on the clean binary

- **`bootstrap.scanning.JarVisitorTest`** — inventory "CRASH" was the `[HIB32]`
  flood tripping the 120 s watchdog (§0a). On the clean binary it is **9/9 PASS**.
  No real bug.
- **`onetoone.nopojo.DynamicMapOneToOneTest`** (inventory "LOADERR") — on the
  clean binary it **HANGS** (no `@@RESULT`, watchdog/timeout) during
  SessionFactory bootstrap of its `hbm.xml` (dynamic-map, no-POJO) mapping, after
  JAXB context init. This is the `--nojit` XML-mapping-processing hang class
  ([HIB-CV-23](HIB-CV-23-nojit-hang-orm-xml-mapping-processing.md)), not a
  load-error/abort. Deep — deferred.

---

## Recommended fix order (by confidence × value)

1. **§0a HIB32 gate** — DONE (working-tree regression; unblocks all runs).
2. **§3 custom-loader `getResourceAsStream`** — HIGH, shared by 2 tests, classloader theme.
3. **§2b array-`Class` reflection** (`isArray`/2-D mirror name) — HIGH, core reflection, likely small.
4. **§4 `Collections` empty singletons** — HIGH if probe confirms non-singleton.
5. **§2a Properties live entrySet** — HIGH cause, contained but touches shared `native-collections`.
6. **§1 sorted hydration** — needs SQL trace to localize.
7. **§5 bounded LRU**, **§6 `.par` URL** — defer.
