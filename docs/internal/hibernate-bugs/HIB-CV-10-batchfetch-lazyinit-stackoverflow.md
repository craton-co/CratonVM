# HIB-CV-10 — `StackOverflowError` in batch-fetch lazy-collection initialization (non-termination)

**Severity:** High — fails ≥7 `batch`/`batchfetch` test classes.
**Status:** ✅ FIXED (worktree `fix/hibernate-full-suite`, `native-collections/src/lib.rs` `native_lhm_entry_set`). Root cause: CratonVM's LinkedHashMap `entrySet()` built its view-backing via `make_view_set_of`→`native_map_put`, which hashes each `SimpleEntry` by its Java `hashCode` (= key.hashCode ^ value.hashCode), calling the VALUE's hashCode — for `LinkedHashMap<…,PersistentCollection>` that ran `PersistentSet.hashCode`→`read`→lazy-init during `BatchFetchQueue.collectBatchLoadableCollectionKeys`, recursing → StackOverflow. Fixed by building the entrySet view-backing keyed by entry IDENTITY hash (like `native_map_entry_set`). Verified `MapEntryHashProbe` value.hashCode=false; NestedLazyManyToOneTest 3/3 + 4 more batch-fetch classes pass.
**Mode:** Interpreter (JIT-off census) — not a JIT bug.
**HotSpot:** not affected (the same recursion terminates after one initialization).

## Symptom

```
java.lang.StackOverflowError
```

Affected (sample): `BatchFetchRefreshTest`, `NestedLazyManyToOneTest`,
`BatchAndUserTypeIdCollectionTest`, `CompositeIdAndElementCollectionBatchingTest`,
`SetAndBagCollectionTest`, `CompositeIdWithOneFieldAndElementCollectionBatchingTest`,
`BatchAndClassIdCollectionTest`.

## Root cause (mechanism)

The faulting thread cycles through this repeating frame group (~per the stack dump):

```
org.hibernate.collection.spi.PersistentSet.hashCode
  -> java.util.AbstractMap$SimpleEntry.hashCode
  -> AbstractPersistentCollection.lambda$initialize$0
  -> AbstractPersistentCollection.initialize / read / withTemporarySessionIfNeeded
  -> SessionImpl.initializeCollection
  -> DefaultInitializeCollectionEventListener.onInitializeCollection
  -> AbstractCollectionBatchLoader.load / resolveKeysToInitialize
  -> AbstractCollectionPersister.initialize
  -> BatchFetchQueue.collectBatchLoadableCollectionKeys
  -> PersistentSet.hashCode            (cycle)
```

Computing an uninitialized `PersistentSet`'s `hashCode()` triggers `read()` → lazy
`initialize()`; during batch fetching, `BatchFetchQueue.collectBatchLoadableCollectionKeys`
computes `hashCode()` of other collections, re-entering. On HotSpot this **terminates** after the
first initialization (the collection's `initialized`/`initializing` state short-circuits the
re-entry, and the batch queue skips a collection already being initialized). On CratonVM it does
**not** terminate — the cycle repeats until the stack is exhausted — so a re-entry guard
(`AbstractPersistentCollection.initialized` / `initializing` boolean, or the batch-queue's
collection dedup) is not being honoured.

This is a CratonVM interpreter correctness bug, **not** a stack-size issue: CratonVM runs a fixed
128 MB main-thread stack and the cycle exhausts it via genuine non-termination (HotSpot's much
smaller default stack handles the terminating version fine).

## Suspected area / next step

A boolean field on `AbstractPersistentCollection` (`initialized` / `initializing`) is likely read
as stale/false on re-entry (interpreter field load/store, or a `withTemporarySessionIfNeeded`
flag not persisting), OR `BatchFetchQueue`'s collection-key set dedup fails (collection
`hashCode`/`equals` identity). Instrument `AbstractPersistentCollection.initialize`'s early-return
guard and `BatchFetchQueue` dedup to find which guard a second entry slips past.
