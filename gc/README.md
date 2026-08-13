# cratonvm-gc

Garbage collector and memory management for CratonVM.

Part of the [CratonVM](https://github.com/craton-co/cratonvm) Java Virtual
Machine implemented from scratch in Rust.

## Scope

Provides the heap, allocator, and collectors. Includes a generational
heap (young + old gen with a card table for old→young write barriers),
a G1-style region collector, a `ZgcRealHeap` backend, thread-local
allocation buffers (TLABs), SATB concurrent marking, weak / soft /
phantom reference processing, compact and compressed object headers
(narrow klass / narrow oop support), and class unloading. The
`GarbageCollector` trait abstracts the backend so the VM can swap
collectors at startup.

`ZgcRealHeap` is a real, memory-backed collector and `-XX:+UseZGC`
genuinely selects it (`GcAlgorithm::Zgc` → `GcBackend::Zgc` →
`VmHeap::Zgc`) — but it is compiled in only behind the default-off `zgc`
feature, so a stock build does not contain it and `-XX:+UseZGC` there
warns and falls back to Generational. It is also not production ZGC: a
stop-the-world, non-moving, whole-heap mark-sweep, with no colored
pointers, load barriers, concurrency, or compaction. The colored-pointer
code above it in `src/zgc.rs` is a metadata-only simulation with no
production consumer.

## Non-goals

- No interpreter or JIT integration logic — the VM crate owns root
  scanning and safepoint coordination; this crate exposes the hooks.
- No bytecode-level object layout decisions; field offsets are computed
  by `cratonvm-classloading`.
- Not a general-purpose allocator. Every allocation knows its
  `ObjectKind` and `ClassId`.

## Usage

```rust
use cratonvm_gc::{ArrayElementType, GcBackend, VmHeap};
use cratonvm_types::{ClassId, Value};

let heap = VmHeap::new(GcBackend::Generational, 64 * 1024 * 1024);
let obj = heap.alloc_object(ClassId::new(1), 2);
let ints = heap.alloc_array(ClassId::new(2), ArrayElementType::Int, 16);
heap.set_field(obj, 0, Value::Int(42));
heap.set_array_element(ints, 0, Value::Int(7)).unwrap();
// Safepoints call heap.collect_garbage(&stw, roots, monitors).
```

## Status

Pre-1.0. API stability is best-effort. Tied to the
[CratonVM](https://github.com/craton-co/cratonvm) workspace version.

## License

Apache-2.0. See `LICENSE` and `NOTICE` at the workspace root.

Copyright 2024-2026 Craton Software Company.
