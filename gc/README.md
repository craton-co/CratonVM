# cratonvm-gc

Garbage collector and memory management for CratonVM.

Part of the [CratonVM](https://github.com/craton-co/cratonvm) Java Virtual
Machine implemented from scratch in Rust.

## Scope

Provides the heap, allocator, and collectors. Includes a generational
heap (young + old gen with a card table for old→young write barriers),
a G1-style region collector, an optional ZGC backend gated behind the
`zgc` feature, thread-local allocation buffers (TLABs), SATB concurrent
marking, weak / soft / phantom reference processing, compact and
compressed object headers (narrow klass / narrow oop support), and
class unloading. The `GarbageCollector` trait abstracts the backend so
the VM can swap collectors at startup.

## Non-goals

- No interpreter or JIT integration logic — the VM crate owns root
  scanning and safepoint coordination; this crate exposes the hooks.
- No bytecode-level object layout decisions; field offsets are computed
  by `cratonvm-classloading`.
- Not a general-purpose allocator. Every allocation knows its
  `ObjectKind` and `ClassId`.

## Usage

```rust
use cratonvm_gc::{VmHeap, GcBackend};

let heap = VmHeap::new(GcBackend::Generational, 64 * 1024 * 1024);
// VM threads call heap.allocate_object / heap.allocate_array;
// safepoints trigger heap.collect via the GarbageCollector trait.
```

## Status

Pre-1.0. API stability is best-effort. Tied to the
[CratonVM](https://github.com/craton-co/cratonvm) workspace version.

## License

Apache-2.0. See `LICENSE` and `NOTICE` at the workspace root.

Copyright 2024-2026 Craton Software Company.
