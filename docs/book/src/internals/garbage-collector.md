# The Garbage Collector

CratonVM's memory management subsystem is implemented in the `cratonvm-gc` crate. This chapter details how the garbage collectors operate internally; for user-facing operation (sizing, collector selection, tuning flags), see [Memory & Garbage Collection](../user-guide/memory-and-gc.md).

## Collectors

| Collector | Selection | Status |
|-----------|-----------|--------|
| **ZGC** (`ZgcRealHeap`, `gc/src/zgc.rs`) | Default / `-XX:+UseZGC` | **The default collector.** Real and wired end to end — `GcAlgorithm::Zgc` → `GcBackend::Zgc` → `VmHeap::Zgc`. Features a dual-ended arena layout with stop-the-world compaction by default (`CRATONVM_ZGC_RELOCATE=0` provides the override). Supports generational collection (`CRATONVM_ZGC_GENERATIONAL=1`) tracking object ages with card barriers, and concurrent marking (`CRATONVM_ZGC_CONC_START=60`) where a coordinator traces the strong reference closure concurrently with mutator execution. Thread-local allocation buffers (TLAB) are active by default across both interpreter and JIT paths. |
| **Generational** | `-XX:+UseGenerationalGC` / `-XX:-UseZGC` | Production generational collector. Young and old generations with write barriers and a card table. Young generation performs moving Cheney copying or selective promotion; moving evacuation fails closed to non-moving collection if root coverage proof is incomplete. |
| **G1** (Region-Based) | `-XX:+UseG1GC` | Experimental region-based collector dividing the heap into discrete regions with incremental evacuation. |

### The ZGC Engine

The `zgc` Cargo feature is enabled by default in CratonVM builds. Key architectural aspects of the implementation include:

- **Arena Compaction and Relocation**: During collection, live objects are slid within the arena to eliminate external fragmentation. When relocation completes, the collector produces a `PointerMap` used by JIT stack maps, monitor tables, JNI registries, and native side tables to remap moved references.
- **Thread-Local Allocation Buffers (TLAB)**: `VmHeap::refill_tlab` integrates with thread-local allocation contexts for both interpreter object allocations and JIT inline bump allocation. Thread-local chunks eliminate global lock contention on the allocation fast path.
- **Concurrent Marking**: When concurrent marking is configured (`CRATONVM_ZGC_CONC_START`), a background `ZMarkCoordinator` traverses the object graph concurrently while mutators execute, utilizing the VM's SATB (Snapshot-At-The-Beginning) pre-write barrier to track concurrently mutated references.
- **Generational Partitioning**: With `CRATONVM_ZGC_GENERATIONAL=1`, objects track age in their headers. Young collection sweeps only up to the nursery boundary, relying on card marks on old-generation objects to capture inter-generational roots.

## Generational Collector Design

Objects are allocated initially in the **young generation** (nursery). Because the majority of objects become unreachable shortly after allocation, the young generation is collected frequently and cheaply. Objects surviving repeated collection passes are **promoted** to the **tenured old generation**.

Key components ensuring correctness:

- **Write Barrier and Card Table**: Stores of references from old-generation objects to young-generation objects are intercepted by write barriers that mark corresponding entries in a card table. During young collection, only dirty cards are scanned for roots, avoiding whole-heap traversal.
- **Root Scanning**: The collection begins from defined roots: active thread stacks (interpreter frames and JIT compiled frames), static class fields, JNI global and local handles, pinned references, and registered VM internal side tables.
- **Precise Stack Maps**: For compiled native code, precise JIT stack maps record exact reference registers and stack locations at safepoints, ensuring accurate liveness identification without conservative guessing.

## Object Layout and Memory Layout

Object representation consists of a header followed by packed fields:

```text
[ ObjectHeader ] [ field0 ] [ field1 ] ...
```

The standard object header is 32 bytes (accommodating class pointer, identity hash, locking/monitor state, and GC metadata). Instance fields are laid out with natural alignment:
- 1, 2, 4, and 8-byte primitives take their natural widths.
- Reference fields occupy 8 bytes by default (or 4 bytes when compressed oops are enabled).
- Arrays store element length in the header followed by tightly-packed elements. The JIT addresses elements directly with scaled-index addressing (SIB).

### Key Modules in `cratonvm-gc`

| Module | Core Responsibility |
|--------|---------------------|
| `heap.rs` | Object and array layout, basic allocation structures. |
| `gen_heap.rs` | Generational heap orchestration (young and old generation coordination). |
| `gc.rs` | Cheney copying evacuation engine. |
| `collector.rs` | Stop-the-world orchestration and phase sequencing. |
| `card_table.rs` | Card marking and dirty-card tracking. |
| `arena.rs` | Bump-pointer allocation arenas. |
| `roots.rs` | Multi-provider root scanning and reference remapping. |
| `old_gen.rs` | Tenured generation allocation and sweep management. |
| `zgc.rs` | ZGC implementation, compaction slide, concurrent marking, and TLAB integration. |

## Native Roots and Object Pinning

Native code, JNI references, and embedders hold object pointers that must be respected during garbage collection. The root registry manages provider callbacks that enumerate these external references.

Additionally, CratonVM supports a **pin set**: an object pinned for native interop or active GPU offload (such as an array mapped directly into a CUDA or HIP kernel) is marked unmovable. Moving collection cycles verify a per-cycle coverage proof: if any subsystem or root provider cannot guarantee complete remapping safety, the cycle safely defaults to non-moving collection.

## Stop-the-World Safepoints

Garbage collection pauses bring mutator threads to synchronized safepoint states. Safepoints inspect thread states, freeze execution, scan interpreter and JIT stacks, and orchestrate tracing and compaction. Once references are remapped, threads resume concurrently.

## Virtual Address Space and Commit Policies

Heap arenas use virtual address reservations that commit pages on demand (`gc::reservation::HeapStore`). The ergonomic maximum heap reservation sizes up to ¼ of physical RAM (capped at 32 GiB by default), ensuring extensive headroom without consuming unneeded physical memory.
