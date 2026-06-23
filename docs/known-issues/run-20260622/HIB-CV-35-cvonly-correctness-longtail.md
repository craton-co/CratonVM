# HIB-CV-35 — CratonVM-only Hibernate correctness divergences (long-tail)

**Status:** OPEN (triage + root-cause complete; fixes specified, partial)
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

**Root cause (hydration):** the dedup occurs while Hibernate hydrates the
`PersistentSortedSet`/`PersistentSortedMap` from the result set — two distinct
rows collapse because their ordering key compares equal **at insert time**. The
sorted-MAP variant is keyed by distinct *String* values (`"A"`,`"B"`) yet also
collapses to size 1, which rules out entity-identity/first-level-cache collision
and points at the **per-row key/element value being read as the same value for
both rows** during collection load (a stale field read or row-cursor reuse in the
collection initializer path), or the elements being added to the sorted backing
*before* their ordering field is populated (two-phase load) such that both
compare as `null==null → 0`.

**Next step to confirm:** run with Hibernate SQL logging (`org.hibernate.SQL` at
DEBUG) on a clean binary to determine whether (a) only 1 row is inserted at
persist, or (b) 2 rows are returned at load but collapse on add. The map-keyed-by-
String evidence strongly favors (b) — a hydration-time field/row read returning
the same value twice.

**Confidence:** root cause class HIGH (hydration, not core collections);
exact mechanism MEDIUM pending SQL trace.

---

## 2. `UnsupportedOperationException` where HotSpot succeeds

### 2a. `util.PropertiesHelperTest` — `Properties.entrySet()` is not a live view

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

VM code: `native_class_is_array` (`native-builtins/src/lang_class.rs:2152`) returns
`mirror_class_name(this).starts_with('[')`. If the mirror for a 2-D array
(`byte[][]`) is not named `"[[B"` (e.g. created with a wrong/strict name), `isArray`
returns false and the array branch is skipped → `CollectionJavaType`.

**Next step to confirm:** probe (ready: `ArrProbe.java`) printing
`byte[][].class.isArray()`, `.getComponentType()`, `.getTypeName()` vs HotSpot.
If `isArray(byte[][])` is `false` or the name is wrong, the fix is in 2-D array
mirror naming in `lang_class.rs`.

**Confidence:** root cause HIGH (resolution hinges on array reflection); exact
defect MEDIUM pending the probe on a clean binary.

---

## 3. Custom `ClassLoader.getResourceAsStream` returns null for classpath resources

**Tests:** `org.hibernate.orm.test.util.SerializationHelperTest`
(CNFE `...util.SerializableThing`),
`org.hibernate.orm.test.proxy.ProxyClassReuseTest`
(CNFE `...ProxyClassReuseTest$ProxyGetter`)

**Symptom (CONFIRMED, reproduced):** `ClassNotFoundException`. Both tests use a
custom `ClassLoader` subclass that loads a class by reading its bytes via
`this.getResourceAsStream(name.replace('.','/') + ".class")` and `defineClass`;
the `getResourceAsStream` returns **null** for a `.class` resource that exists on
the application classpath, so the loader throws `"<name> not found"`.

**Root cause:** the resource lookup path for a custom (app-defined)
`ClassLoader` subclass does not locate classpath resources. In the JDK,
`ClassLoader.getResourceAsStream` → `getResource` delegates to the parent and
ultimately the application/system loader's `findResource`, which scans the
classpath. On CratonVM this returns null for these custom loaders. This is the
**resource** facet of the classloader-isolation work tracked in
[HIB-CV-24](HIB-CV-24-classloader-isolation-delegation.md) / SBR-14, and is
distinct from `loadClass`/`findBootstrapClass` (the WIP `classloader.rs`
`cl_bootstrap_scoped` work addresses class loading, not resource lookup).

**Recommended fix:** ensure `ClassLoader.getResource`/`getResourceAsStream`
default delegation reaches the app/system classpath scan for custom subclasses
that don't override `findResource` — i.e. the synthetic/native
`getResourceAsStream` must fall through to the classpath resource resolver
(the same one backing the system loader), not just the bootstrap/module set.

**Confidence:** root cause HIGH (shared, reproduced on both tests); fix location
in the classloader resource natives.

---

## 4. `Collections` empty-singleton identity (`==`) divergence

**Test:** `org.hibernate.orm.test.stateless.StatelessSessionPersistentContextTest`

**Symptom (inventory):** `"StatelessSession: PersistenceContext has not been
cleared" expected: <true> but was: <false>`.

**Root cause:** the assertions use **reference identity** against JDK singletons:
```java
persistenceContextInternal.managedEntitiesIterator() == Collections.emptyIterator()
persistenceContextInternal.getCollectionsByKey()     == Collections.EMPTY_MAP
```
Hibernate returns the JDK singletons (`Collections.emptyIterator()` /
`Collections.EMPTY_MAP`) when the context is empty. The `==` holds on HotSpot
because those are process-wide singletons. If CratonVM's
`Collections.emptyIterator()` (or `emptyMap()`) does not return the *same*
singleton instance each call — or returns a freshly-synthesized object — the
identity check fails. This is the object-identity/synthesis theme also seen in
SBR-07..13.

**Next step to confirm:** probe (ready in `ArrProbe.java`):
`Collections.emptyIterator() == Collections.emptyIterator()` and
`Collections.EMPTY_MAP == Collections.emptyMap()` vs HotSpot.

**Recommended fix:** ensure `Collections.emptyIterator()`/`emptyList()`/
`emptyMap()`/`emptySet()` return the canonical static singletons
(`Collections.EMPTY_*` / `EmptyIterator.EMPTY_ITERATOR`) rather than new
instances, so reference identity is preserved.

**Confidence:** root cause HIGH (identity-on-singleton is explicit in the test);
exact VM defect MEDIUM pending probe.

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

## Crash variants — re-evaluation

The inventory's "crash"/"LOADERR" entries
(`bootstrap.scanning.JarVisitorTest` CRASH, `dynamicmap`/`onetoone.nopojo`
`DynamicMapOneToOneTest` LOADERR) must be re-checked on a **clean** (de-flooded)
binary: several apparent crashes/hangs in initial reruns were the `[HIB32]` flood
tripping the watchdog (§0a), not genuine faults. `DynamicMapOneToOneTest` uses an
`hbm.xml` (dynamic-map, no-POJO) mapping and may also intersect the `--nojit`
XML-mapping path (HIB-CV-23). Re-run pending a contention-free build.

---

## Recommended fix order (by confidence × value)

1. **§0a HIB32 gate** — DONE (working-tree regression; unblocks all runs).
2. **§3 custom-loader `getResourceAsStream`** — HIGH, shared by 2 tests, classloader theme.
3. **§2b array-`Class` reflection** (`isArray`/2-D mirror name) — HIGH, core reflection, likely small.
4. **§4 `Collections` empty singletons** — HIGH if probe confirms non-singleton.
5. **§2a Properties live entrySet** — HIGH cause, contained but touches shared `native-collections`.
6. **§1 sorted hydration** — needs SQL trace to localize.
7. **§5 bounded LRU**, **§6 `.par` URL** — defer.
