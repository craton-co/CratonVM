# `ConcurrentHashMap` allocated its whole segment table in the constructor — FIXED 2026-09-11

**Status: FIXED.** Found 2026-09-11 while carrying
`docs/internal/fixed-bugs/jdk-collection-classes-are-padded-to-a-synthetic-stub-floor-FIXED-20260911.md`'s
treatment to the rest of the collections. It was the worst ratio in that survey
by a factor of four, and it carried a wrong-field-value defect with it.

**Verified on:** Windows 11, JDK 25 Temurin `25.0.3+9`, branch
`claude/collections-speed-20260911` off `dev@f2a60701b`.

## 1. The measurement

`probes/CollectionShapeCause.java`, retained heap per EMPTY instance, 20 000
instances, Generational:

```text
                          HotSpot   before   after
  ConcurrentHashMap          64.1    600.0   104.0
  ConcurrentHashMap + 4      272.3    792.0   792.0
```

The FILLED row is the control, and it is why the empty-row win is a removal
rather than a move: if the constructor's allocation had merely been deferred to
somewhere else on the insert path, `+4` would have moved with it.

600 B was `Object[4]` of segments, four three-slot segment objects and four
`Object[4]` bucket arrays — every one of them for a map that may never be
written. HotSpot allocates NOTHING: `table` is null until the first `put`, and
the constructor only sets `sizeCtl`.

## 2. It was also writing an `Object[]` into a `Set` field

`CHM_FIELD_SEGMENTS` is absolute slot 0, and on a real `ConcurrentHashMap` slot
0 is `AbstractMap.keySet`, declared `Ljava/util/Set;`. Every map this VM built
handed the image a `Set` field holding an `Object[]`:

```text
                             HotSpot   CratonVM before   CratonVM after
  AbstractMap.keySet           null    Object[] len=4    null
```

That is the same shape `publish_map_table` records fixing on the `HashMap` side
("it does not fault today only because the natives shadow every reader"). A
fresh `ConcurrentHashMap` is now field-for-field identical to HotSpot's, all
twelve.

## 3. The fix

`native_chm_init_default` no longer calls `chm_init_segments`.
`chm_segment_for_mut` already installed the segments on first insert, with the
GC pinning that path needs, and every inserting entry point goes through it —
`put`, `putIfAbsent`, `remove`, `replace`, `merge`, `compute`,
`computeIfAbsent`. The read-only paths already treat a segment-less receiver as
an empty map (`chm_all_segments` returns an empty `Vec`), which it is.

`sizeCtl` is still recorded: a real default map's first table is
`DEFAULT_CAPACITY = 16` and `chm_reorder_by_virtual_bucket` needs that number
whether or not a table exists yet.

**One number had to be reconciled first.** The lazy path built
`CHM_DEFAULT_SEGMENTS` (16) segments and the constructor built
`CHM_DEFAULT_INIT_SEGMENTS` (4). That disagreement cost nothing while the lazy
arm served only receivers no constructor had run on — and would have quadrupled
every default map's first table the moment it served them all. The lazy path now
builds 4, which is also the first time `chm_total_capacity` (4 x 4 = 16) and
`chm_initial_table` (16 for an unrecorded receiver) agree; both feed
`chm_reorder_by_virtual_bucket`.

**`computeIfAbsent` is the case worth checking and it is safe.**
`chm_compute_if_absent_absent_path` looks the segment up with the READ-ONLY
`chm_segment_for` and returns `null` without storing when it finds none — so a
path that reached it on a segment-less map would report success and store
nothing. It cannot: `native_chm_compute_if_absent` resolves through
`chm_segment_for_mut` first, and so do `compute` and `merge`. The probe asserts
each door separately for that reason.

## 4. What is NOT changed

`native_chm_init_capacity` and `native_chm_init_full` still allocate eagerly.
Their shape is `(num_segments, cap_per_segment)` computed from arguments, and
only the default form's shape is recoverable from `sizeCtl` alone — the 3-arg
form's segment count comes from `concurrencyLevel`, which is recorded nowhere.
Deferring those needs somewhere to put the pending shape, which is its own
change. `java.util.Properties` is the notable consumer (its backing map is a
`new ConcurrentHashMap<>(initialCapacity)`), and it is 544 B against HotSpot's
120.3 — see the successor page.

## 5. How it was validated

* **`probes/CollectionSlotFloor.java`** grew an `emptyConcurrentReads` section
  for exactly the state this makes reachable: thirteen read-only assertions on a
  never-written map, then EACH inserting door on its own fresh map (`put`,
  `putIfAbsent`, `computeIfAbsent`, `merge`, `compute`, `putAll`), then growth
  past the first table from a map that started with none. PASS on HotSpot and on
  CratonVM in real-JDK mode.
* **Synthetic-JDK** (`--features synthetic-jdk`, its own binary): the same eight
  pre-existing mismatches and the same 127 `field index OOB` warnings as the
  established baseline — verdict-neutral, which is the criterion.
* **`regression-suite/run.sh`:** 93 passed, 0 failed of 93.

## 6. Repro

```bash
javac -d out probes/CollectionShapeCause.java probes/CollectionSlotFloor.java
java -Xmx3g --add-opens java.base/java.util=ALL-UNNAMED \
     --add-opens java.base/java.util.concurrent=ALL-UNNAMED -cp out CollectionShapeCause
cratonvm -XX:+UseGenerationalGC --Xmx 3g -c out CollectionShapeCause
cratonvm --Xmx 2g -c out CollectionSlotFloor
```
