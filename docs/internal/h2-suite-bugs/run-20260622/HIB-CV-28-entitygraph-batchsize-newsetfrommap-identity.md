# HIB-CV-28 — Entity-graph `@BatchSize` collection batching breaks (`Collections.newSetFromMap` drops `IdentityHashMap` semantics)

**Run:** full Hibernate ORM suite, 2026-06-22/23
**Test:** `org.hibernate.orm.test.entitygraph.EntityGraphBatchSizeTest` (both tests)
**Severity:** Medium — real behavioral divergence; deterministic under `--nojit`; HotSpot PASS
**Status:** ROOT-CAUSED + FIXED (native `Collections.newSetFromMap` now honors `IdentityHashMap` backing)

---

## Symptom

Both tests fail with a bare `AssertionError` from `assertSelectCount`. The tests
assert the number of SQL `SELECT`s for batched vs non-batched associations on a
graph that pins per-attribute `@BatchSize`:

```
GraphBatchBatchedAuthor  expected 1 (batched, bs=3)
GraphBatchSingleAuthor   expected 3 (one-by-one, bs=1)
GraphBatchBook_batchedTags expected 1 (batched, bs=3)   <-- CratonVM issued 3
GraphBatchBook_singleTags  expected 3 (one-by-one, bs=1)
```

Captured counts (instrumented `assertSelectCount`):

| association | kind | bs | HotSpot | CratonVM (before fix) |
|---|---|---|---|---|
| batchedAuthor | `@ManyToOne` | 3 | 1 `IN (?,?,?)` | **1 `IN (?,?,?)`** ✓ |
| singleAuthor | `@ManyToOne` | 1 | 3 `=?` | 3 `=?` ✓ |
| **batchedTags** | `@ElementCollection` | 3 | **1 `IN (?,?,?)`** | **3 `=?`** ✗ |
| singleTags | `@ElementCollection` | 1 | 3 `=?` | 3 `=?` ✓ |

Only the **batched element collection** diverged. Entity batch-fetch (the
`@ManyToOne`) batched correctly; the collection batch-fetch did not.

## Root cause

Localized by instrumenting Hibernate and diffing the two VMs:

`AbstractCollectionPersister.determineLoaderToUse` chooses a batch loader vs a
single-key loader from the active `LoadQueryInfluencers`:

```
HotSpot   batchedTags: isAffectedByInfluencers=true  effectiveBatchSize=3 -> createCollectionLoader (batch, IN (?,?,?))
CratonVM  batchedTags: isAffectedByInfluencers=false effectiveBatchSize=-1 -> getCollectionLoader (static single-key, =?)
```

On HotSpot the loader is resolved at `endLoading`, *inside* the
`withFetchOptions` scope, so the graph's `batchSize=3` is visible. On CratonVM
the same resolution fired **inline, per row, outside the scope**. The call
stacks pinpointed why:

```
HotSpot:  forceInitialization  <- endLoading  <- LoadQueryInfluencers.withFetchOptions:334   (scope ACTIVE -> bs=3, batched)
CratonVM: forceInitialization  <- PersistentSet.hashCode(PersistentSet.java:430)              (scope INACTIVE -> bs=-1, single)
                               <- AbstractNonJoinCollectionInitializer.addNonLazyCollection:197
```

`AbstractNonJoinCollectionInitializer` defers eager non-lazy collections to
`endLoading` for batched initialization, collecting them in:

```java
nonLazyCollections = newSetFromMap( new IdentityHashMap<>() );  // identity set
nonLazyCollections.add( collection );                           // line 197
```

`PersistentSet.hashCode()` **force-initializes the collection** (its hash depends
on its elements). An `IdentityHashMap` must use `System.identityHashCode`, so
`add` must NOT call `collection.hashCode()`. On CratonVM it *did* — initializing
each collection inline, per row, with the static single-key loader, before the
deferred batch ever ran.

### The VM-level defect (minimal reproducer)

```java
Set<K> s = Collections.newSetFromMap(new IdentityHashMap<>());  // K.hashCode/equals have side effects
s.add(a); s.add(b); s.add(c); s.add(a);
// HotSpot : size=3  a.hashCode-calls=0  a.equals-calls=0   (identity)
// CratonVM: size=3  a.hashCode-calls=2  a.equals-calls=2   (VALUE hash -> BUG)
//
// IdentityHashMap.put directly: hashCode-calls=0 on BOTH  (IdentityHashMap itself is correct)
```

`IdentityHashMap` is fine; **`Collections.newSetFromMap` is the culprit.**
CratonVM force-dispatches `java/util/Collections.newSetFromMap(Map)Set`
(`vm/src/vm/vm_exec.rs`, the big force-native `||` chain) to a synthetic native
(`native-builtins/src/lib.rs`, registered for `java/util/Collections`) that
**ignored the backing map** (`let _map = ...`) and returned a plain value-hash
`HashSet`. That override was added for Spring Boot's
`newSetFromMap(new WeakHashMap<>())` shutdown hook and Felix's
`newSetFromMap(new ConcurrentHashMap())` — both value-hash maps, so the synthetic
set happened to behave correctly there. For an `IdentityHashMap` backing it
silently switched identity semantics to value semantics, and the value hash on a
`PersistentSet` has the side effect of initializing it.

## Fix (two coordinated parts)

**1. `native-builtins/src/lib.rs` — the `Collections.newSetFromMap` native.**
When the backing map is a `java/util/IdentityHashMap`, build the **real**
`java/util/Collections$SetFromMap` wrapping the passed map (via
`new_object_initialized`), so `add`/`contains` route through `IdentityHashMap`
(reference identity), exactly like HotSpot. All other backings (Hash/Weak/
Concurrent — value-hash) keep the existing synthetic `HashSet`, so the Spring/
Felix workarounds are untouched.
- Trap fixed along the way: the original native read the backing map from
  `args.get(1)`, but for this *static* method the argument is at `args[0]`
  (`argc=1`). The original code never used `_map`, so the off-by-one was inert
  until this fix needed the value. The native now picks the first `Object`
  operand positionally.

**2. `native-collections/src/lib.rs` — the `Collections$SetFromMap` view natives.**
`newSetFromMap` previously *always* returned a synthetic `HashSet`, so a real
`SetFromMap` was never created and these natives were dead. Part 1 resurrects
them for the `IdentityHashMap` case, and they mis-read a real `IdentityHashMap`
(its backing is a flat alternating key/value table, not CratonVM's synthetic
`buckets/size/capacity` HashMap) — `iterator()`/`toArray()` returned garbage
(`byte[]`, nulls) → `ClassCastException: SessionImpl cannot be cast to
PersistentCollection` in Hibernate's `endLoading` loop. Fixed by routing
`iterator`/`toArray`/`contains` through the **real backing map** via
`invoke_virtual` (`m.keySet().iterator()`, `m.keySet().toArray()`,
`m.containsKey(k)`) instead of the synthetic-layout `native_map_*` helpers.
Only the (new) `IdentityHashMap`-backed `SetFromMap` path is affected.

## Verification

- Minimal `IdHashProbe`: CratonVM `newSetFromMap(IdentityHashMap)` → hashCode/
  equals calls **0** (was 2), `IdentityHashMap.put` unchanged.
- `SfmProbe` / `SfmReg`: `SetFromMap` over IdentityHashMap iterates `[AA,BB,CC]`,
  `contains`/`toArray`/`new ArrayList<>(set)` all correct == HotSpot; Hash/Weak/
  Concurrent backings keep the synthetic `HashSet` path (size/contains/list all
  correct, unchanged).
- `EntityGraphBatchSizeTest`: **OK 2/2** (was 0/2); `batchedTags` now one
  `IN (?,?,?)` query; counts 1/3/1/3 == HotSpot. Reproduced + verified `--nojit`.

Build note: verified with a binary built with per-package `opt-level=0`
(`--config`, deps cached) because the shared build host was being thrashed by
concurrent sessions killing `cargo`/`rustc`; the fix is source-level and
collector/JIT-independent (interpreter `--nojit`).

## Notes / traps

- `IdentityHashMap` running real JDK bytecode is correct on its own — do not
  "fix" `IdentityHashMap`; the bug was the `newSetFromMap` synthetic override.
- General trap: a synthetic native that fabricates a collection and ignores a
  constructor argument loses the argument's semantics. `newSetFromMap` must
  honor the backing map type.
