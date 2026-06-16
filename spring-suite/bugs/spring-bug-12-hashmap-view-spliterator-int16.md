# spring-bug-12: HashMap keySet/entrySet/values `.spliterator()` → `expected object reference, got int(16)`

| | |
|---|---|
| **Category** | VM-CORRECTNESS / interpreter (synthetic collection layout) |
| **Module** | native-collections (map views) |
| **HotSpot JDK 25** | OK |
| **CratonVM** | INTERNAL ERROR (interpreter, even `--nojit`) |
| **Status** | **FIXED** (worktree `fix/spring-bug-10-11`) — validated |
| **Found via** | spring-bug-11 investigation (the JDK method `HashMap$KeySpliterator.tryAdvance` is ALSO miscompiled by JIT dup_x1, but here it fails in the INTERPRETER for a different reason) |

## Symptom
`map.keySet().spliterator()` (and `entrySet()`/`values()`) on a `java.util.HashMap`
throws, even with JIT disabled:
```
internal error: expected object reference, got int(16)
  ctx="arraylength … (in java/util/HashMap$KeySpliterator.tryAdvance …)"
```
`int(16)` = the HashMap's capacity. Repro: `spring-suite/probe/KSplProbe.java` (`--nojit`).
Plain iteration (`for (k : map.keySet())`), `get`, `size`, `Set.of(...).spliterator()`,
and a *plain* `new HashSet().spliterator()` all work — only the **map-view** spliterator failed.

## Root cause
CratonVM models `HashMap` natively with a synthetic `(buckets@0, size@1, capacity@2)` field
layout. `map.keySet()` (`native_map_key_set`) returns a synthetic `HashSet` whose backing was a
`cratonvm/util/MapViewBacking` with that **synthetic** layout. But `HashSet.spliterator()` runs
real JDK bytecode: it builds `new HashMap.KeySpliterator<>(this.map, …)` and reads
`getfield map.table` / `map.modCount` / `map.size` **directly by their real-HashMap field slots**.
The real `table` slot is **2** — which on the synthetic backing held the `Int(capacity)=16`. So
`getfield table` returned `16` and `arraylength` got an int. (The native `HashSet.spliterator`
override `p59_hashset_spliterator` never fires — `HashSet.spliterator` has real bytecode, which
wins, so the synthetic spliterator path is dead.)

This is the same family the S111r7/r11/r13 fixes already addressed for `System.getenv()`,
`Properties`, and `Set.of(...)` (all switched their synthetic backing to the real HashMap layout) —
it just had never been applied to the **keySet/entrySet view** backing, which is also *mutable*
(live-view resync + write-through removal), so it can't be a simple immutable real-layout copy.

## Fix (`native-collections/src/lib.rs`)
Dual-storage, matching the convention `map_resize` already maintains (slot 0 buckets **mirrored**
into the real `table` slot, capacity Int only written when `table` isn't that slot):
- **`alloc_view_backing`** — allocate the view backing as a real `java/util/HashMap` (16 slots: real
  fields `table`/`size`/`modCount`/`threshold`/`loadFactor`/`entrySet` at their resolved slots, the
  bucket array ALSO at synthetic slot 0, the source-map + view-kind markers at slots 14/15). Falls
  back to the legacy synthetic `MapViewBacking` only if the real HashMap class isn't resolvable
  (early bootstrap, where the JDK spliterator path isn't reachable anyway).
- **`resync_view_set`** — the live-view rebuild now mirrors the fresh bucket array into the real
  `table` slot and skips the `Int(capacity)` write when `table` IS slot 2 (else it re-corrupted the
  table → bug returned on the first `size()`/`iterator()`/`contains()` read). Capacity derived from
  the bucket-array length via `map_state` (slot 2 may be an array, not an Int).

`map_state`/`set_map_size`/`map_alloc_node`/the remove natives were already layout-aware (table-slot
fallback, name-resolved `size`, real `HashMap$Node` slot order), so they needed no change.

## Validation (all match HotSpot, `--nojit`)
`probe/`: `KSplProbe` (original repro), `HMDiag`, `CollViewTest` (11 checks: spliterator/stream/
entrySet/values + write-through), `ViewResync` (9 checks: **resync-then-spliterate, live-view
mutate, toArray, setValue write-through** — the cases the first cut regressed), `FocusedReg`
(12 direct view ops: putAll, computeIfAbsent, merge, keySet/entrySet `removeIf`, `values().remove`,
copy-ctor, contains, toArray). LinkedHashMap views also work.

## Out of scope (separate, pre-existing — NOT regressed)
- `TreeMap` (`TreeSet`/`ts_` side-table backing) and `ConcurrentHashMap` (`alloc_synthetic("HashSet",1)`,
  no view markers) keySet/entrySet `.spliterator()` still fail — different backings, not touched by
  this fix (`probe/OtherMaps.java`).
- `Collectors.toMap(...)` / stream collectors throw `intValue on null` — a separate Stream/Collectors
  defect (collection uses `put`, never keySet views); `probe/MapsRegression.java` line 15.
- `HashMap$KeySpliterator.tryAdvance` under **JIT** still SIGSEGVs — that is the dup_x1 OSR miscompile
  of spring-bug-11 (independent of this interpreter fix).
