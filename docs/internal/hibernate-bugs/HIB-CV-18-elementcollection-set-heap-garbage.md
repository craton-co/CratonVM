# HIB-CV-18 — `@ElementCollection` `Set<String>` materializes heap garbage instead of the stored values

**Severity:** High (data corruption). Confirmed class:
`org.hibernate.orm.test.annotations.collectionelement.ordered.ElementCollectionSortingTest` — `found=2 ok=1 failed=1`.

**Status:** ✅ FIXED — `dev` (commit `692320c7`), `native-collections/src/lib.rs`.
**Mode:** Interpreter (JIT-off census).
**HotSpot:** not affected.

## Symptom

`testSortingEmbeddableCollectionOfPrimitives` persists a `Person` whose
`@ElementCollection @OrderBy Set<String>` holds `{antoniak, lantoniak}`, clears
the session, reloads, and `new ArrayList<>(person.getNickNames())` comes back as
**three unrelated VM singletons**:

```
[org.hibernate.internal.SessionFactoryImpl@…,
 <a SessionFactory UUID string>,
 org.hibernate.boot.registry.internal.BootstrapServiceRegistryImpl@…]
```

## Root cause (NOT GC corruption — a deterministic slot misread)

The garbage is the **same VM singletons on every run**, which ruled out random
heap/GC corruption and pointed at a fixed slot misread. Reproduced with a
minimal standalone entity (`.cratonvm-suite/MyEcRepro.java`); the diagnostic was
decisive:

- `size()` = 2, **iterator** → `[antoniak, lantoniak]`, the backing
  `LinkedHashSet` / its `LinkedHashMap.keySet()` all **correct**.
- Only `new ArrayList<>(persistentSet)` returned the garbage; a direct
  `persistentSet.toArray()` was correct.

`new ArrayList<>(coll)` → `native_al_init_from_collection` →
`collect_collection_elements`, whose generic ArrayList-layout probe reads the
receiver's `elementData`/`size` slots **by index on any object**. A Hibernate
`org.hibernate.collection.spi.PersistentSet` is a real bytecode class; those
indices land on unrelated fields, one of which is a ref-array whose head
elements are VM singletons → the observed garbage. The collection's own
`iterator()`/`toArray()` (real bytecode) were correct all along.

## Fix

Mirror the existing class-name special-cases in `collect_collection_elements`
(ILHC / EnumSet / ArrayDeque / PriorityQueue): route any
`org/hibernate/collection/` instance to its own `size()`/`toArray()` instead of
the blind slot probes. Also hardened `native_al_init_from_collection` and
`native_hs_to_array`(+typed) to be GC-safe (count → allocate → re-read), since
`alloc_ref_array` can move the just-collected elements while the raw `Vec<Value>`
refs aren't GC roots (the documented `tm_materialize_deser_array` pattern).

## Verification (vs HotSpot)

Minimal repro `MyEcRepro` (reload `@ElementCollection @OrderBy Set<String>`):
`new ArrayList<>(set) = [antoniak, lantoniak]` → **PASS** (was VM-singleton
garbage). Backing set, iterator, and `toArray()` all agree with HotSpot.
