# The Garbage Collector

CratonVM's memory management lives in the `cratonvm-gc` crate. This chapter
describes how the collectors work; for operating the heap (sizing, selecting a
collector, diagnosing pauses) see [Memory & Garbage
Collection](../user-guide/memory-and-gc.md).

## Collectors

| Collector | Selection | Status |
|-----------|-----------|--------|
| **Generational** | default / `-XX:+UseGenerationalGC` | The default. Young/old generations with write barriers and a card table. The default young path is non-moving mark/sweep with selective promotion; moving evacuation is opt-in and fail-closed on incomplete root coverage. |
| **G1** (region-based) | `-XX:+UseG1GC` | Experimental. The generational collector remains the safety net during its maturation. |
| **ZGC** (`ZgcRealHeap`, `gc/src/zgc.rs:1396`) | `-XX:+UseZGC`, in a build with the default-off `zgc` Cargo feature | Real and wired end to end — `GcAlgorithm::Zgc` → `GcBackend::Zgc` → `VmHeap::Zgc` — but **not** a real ZGC: one `Arena`, a non-moving whole-heap stop-the-world mark-sweep, no TLABs. Absent from a stock build. The colored-pointer / `ZPage` code alongside it in the same file (`ZgcCollector`, `ColoredPointer`, `LoadBarrier`, `GenerationalZgc`) is a metadata-only simulation with no production consumer. |

> **On the `zgc` row.** `gc/src/vm_heap.rs` does `use crate::zgc::ZgcRealHeap`
> and carries `VmHeap::Zgc` arms behind the same cfg, and `cratonvm-vm` forwards
> the feature, so `-XX:+UseZGC` really selects it. The feature is
> **default-off** — a stock build compiles only two backends, and
> `cargo build --release -p cratonvm-cli --features zgc` is what produces a
> ZGC-capable launcher. It stays default-off for pass-rate parity, not for want
> of a consumer: on the 1975-class Spring Boot suite, same binary with only the
> collector toggled, ZGC measures 1860 PASS / 49 HANG / 22 FAIL against the
> default collector's 1902 / 18 / 11
> ([record](../../../internal/fixed-suite-bugs/springboot/zgc-real-fullsuite-regression-RETIRED-20260807.md)). The path to
> a genuinely concurrent, generational, compacting ZGC is
> [`docs/feature-designs/zgc-production-implementation-plan.md`](../../../feature-designs/zgc-production-implementation-plan.md).

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
  (interpreter frames and JIT frames), static fields, JNI references, pinned
  objects, and registered VM/native side tables — and traces reachable
  objects. Compact interpreter slots retain reference-kind information, while
  [JIT stack maps](jit.md#precise-stack-maps-and-gc-safety) and the conservative
  frame safety net cover compiled frames.

## Object layout

Objects carry a header followed by their fields:

```text
[ ObjectHeader ] [ field0 ] [ field1 ] ...
```

The compatibility header is currently 32 bytes. Instance fields use
class-computed, alignment-aware compact offsets: primitive fields occupy their
natural 1/2/4/8-byte widths and reference fields occupy eight bytes by default.
With compressed oops explicitly enabled, reference fields/elements narrow to
four bytes. The interpreter's `Value` type is a boundary representation, not
the physical object-field layout.

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
roots. VM/native side tables register paired scan/remap callbacks through the
root registry, and the collector consumes those providers without importing a
Java-library overlay. The GC also maintains a **JNI pin set**: an object pinned
for a critical section (for example, an array handed to native code, or — with
the GPU feature — an array a GPU kernel is reading) is kept alive and not moved
for the duration.

Moving collection requires a per-cycle coverage proof. If any JIT/native/root
owner cannot prove safe discovery and remapping, the cycle diverts to the
non-moving path rather than relocating with incomplete information.

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
