# Generational GC Architecture — Moving Young Gen and Concurrent Old Gen

**Status:** Shipped (active under `CRATONVM_GC=generational`). Moving young generation
Cheney copy is default on; concurrent old-generation mark and sweep is supported.

## What it does today

CratonVM's Generational Garbage Collector (`gc/src/gen_heap.rs`, `gc/src/card_table.rs`,
`gc/src/gc_quiescence.rs`) optimizes memory management based on the weak generational hypothesis:
most objects die young.

### 1. Heap Partitioning: Young and Old Generations

The heap is segregated into two primary spaces:
- **Young Generation:**
  - **Eden Space:** Fresh allocations occur in Eden using fast bump-pointer thread-local
    allocation buffers (TLABs).
  - **Survivor Spaces (From-Space and To-Space):** Semi-space Cheney copy spaces used to retain
    objects surviving initial minor collections.
- **Old (Tenured) Generation:**
  - Houses long-lived objects promoted from the young generation after exceeding the tenuring
    threshold.
  - Managed via mark-sweep and compacting free-list arenas.

### 2. Minor Collection: Cheney Semi-Space Evacuation

When Eden fills, a minor (young-generation) collection triggers:

1. **Root Identification:**
   - Active thread execution stacks and registers.
   - JNI handles and VM global roots.
   - **Remembered Sets (Card Table):** Dirty cards in the card table identify old-generation
     objects holding references into young-generation objects, avoiding an expensive scan
     of the entire old generation.
2. **Evacuation:**
   - Live young objects reachable from roots are copied into the `To` survivor space.
   - Objects that have survived multiple young cycles (reaching `tenuring_threshold`) are
     promoted directly into the old generation.
3. **Space Flip:**
   - Eden and `From` survivor space are cleared simultaneously.
   - The roles of `From` and `To` survivor spaces are swapped.
4. **Pinned Allocation Protection:**
   - Pinned objects (e.g., active JNI critical sections or buffer locks) are tracked via
     pinned young lists and preserved without moving, preventing pointer invalidation.

### 3. Card Table and Remembered Set Mechanism

To decouple young-generation collection from old-generation size:
- **Card Granularity:** The heap is partitioned into 512-byte card granules (`CARD_SHIFT = 9`).
- **Post-Write Barrier:** Any reference store in the old generation marks the corresponding
  card table byte as `DIRTY_CARD`.
- **Card Scanning:** Minor GC scans only dirty cards to find cross-generation pointers. Once
  scanned, cards are reset to clean unless they continue to hold cross-generation references.

### 4. Major Collection: Concurrent Old Generation Mark and Sweep

When old generation occupancy crosses the collection threshold:
- **Concurrent Mark:** Traverses live old-generation objects concurrently with mutator threads
  using SATB write barriers to track concurrent pointer modifications.
- **STW Sweep / Compaction:** Reclaims dead old-generation memory and consolidates fragmented
  free lists.

### 5. Dynamic Sizing and Quiescence

- **Share Sizer:** Dynamically tunes the ratio of Eden to Survivor to Old generation memory
  based on allocation throughput, promotion volume, and pause duration targets.
- **Heap Limits:** Respects `-Xms` (initial heap size), `-Xmx` (maximum heap size), and
  GC overhead limits, raising `OutOfMemoryError` if GC overhead exceeds configured thresholds.
- **Quiescence Protocol:** Thread quiescence ensures all mutator threads halt safely at
  safepoint polls before heap restructuring phases begin.

## Flags and Configuration

| Flag | Default | Description |
|---|---|---|
| `CRATONVM_GC` | `generational` | Enables the generational collector |
| `CRATONVM_MOVING_YOUNG_GEN` | `1` | Enables moving young generation evacuation |
| `CRATONVM_CONC_MARK` | `1` | Enables concurrent marking for old-gen collections |
