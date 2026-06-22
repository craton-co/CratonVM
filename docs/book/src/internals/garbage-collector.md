# The Garbage Collector

CratonVM's memory management lives in the `cratonvm-gc` crate. This chapter
describes how the collectors work; for operating the heap (sizing, selecting a
collector, diagnosing pauses) see [Memory & Garbage
Collection](../user-guide/memory-and-gc.md).

## Collectors

| Collector | Selection | Status |
|-----------|-----------|--------|
| **Generational** | default / `-XX:+UseGenerationalGC` | The default. Young/old generations with write barriers and a card table. The young generation uses a Cheney copying collector; the default young path is a non-moving sweep with selective promotion. |
| **G1** (region-based) | `-XX:+UseG1GC` | Experimental. The generational collector remains the safety net during its maturation. |
| `zgc` | feature-gated stub | Not a selectable production collector — a metadata simulation plus a real stop-the-world mark-sweep heap that isn't wired into the backend dispatch. |

## Generational design

Objects are allocated in a **young generation**. Most objects die young, so the
young generation is collected frequently and cheaply; objects that survive long
enough are **promoted** to the **old generation**, which is collected less
often.

Two cross-cutting mechanisms keep generational collection correct:

- **Write barrier + card table.** A reference stored from an old-generation
  object to a young-generation object must be tracked, or the young collection
  would miss a live root. CratonVM records such cross-generational stores by
  marking a *card* (a small region of the old generation) so the young collector
  can scan only the dirty cards instead of the whole old generation.
- **Root scanning.** A collection starts from the roots — thread stacks
  (interpreter frames and JIT frames), static fields, JNI references, and pinned
  objects — and traces reachable objects. The Structure-of-Arrays frame layout
  lets the collector identify reference slots from their type tags, and [precise
  JIT stack maps](jit.md#precise-stack-maps-and-gc-safety) do the same for
  compiled frames.

## Object layout

Objects carry a header followed by their fields:

```text
[ ObjectHeader ] [ field0 ] [ field1 ] ...
```

Arrays use **compact element sizes** — 1, 2, 4, or 8 bytes per element depending
on the component type (so `byte[]` is one byte per element, `int[]` four,
`long[]`/`double[]`/`Object[]` eight). The JIT addresses elements directly with
SIB scaling.

Key modules:

| Module | Responsibility |
|--------|----------------|
| `heap.rs` | Object/array layout and allocation. |
| `gen_heap.rs` | The generational heap (young + old). |
| `gc.rs` | The Cheney copying algorithm. |
| `collector.rs` | Collection coordination and stop-the-world orchestration. |
| `card_table.rs` | Card marking for cross-generational references. |
| `arena.rs` | Bump-pointer arenas. |
| `roots.rs` | Root scanning and pointer remapping. |
| `old_gen.rs` | Old-generation management. |

## Native roots and JNI pinning

Native code and embedders hold object references the collector must treat as
roots. The GC maintains a **native-root registry** plus a **JNI pin set**: an
object pinned for a critical section (for example, an array handed to native
code, or — with the GPU feature — an array a GPU kernel is reading) is kept alive
and not moved for the duration.

## Stop-the-world safepoints

Collection happens at a stop-the-world safepoint: threads are brought to a known
point, roots are scanned, and live objects are traced (and, for moving
collection, evacuated and references remapped). The safepoint machinery is part
of the VM's threading subsystem — see [Threading &
Concurrency](threading.md).

## Eager arena commit

The generational heap **eagerly commits** its arenas at startup (it zero-fills
the backing memory). This is why the ergonomic default heap is capped — an
uncapped quarter-of-RAM heap would charge that much committed memory per process.
See [Memory & Garbage Collection](../user-guide/memory-and-gc.md).
