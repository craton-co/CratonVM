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

`ZgcRealHeap` is a real, memory-backed collector, `-XX:+UseZGC`
genuinely selects it (`GcAlgorithm::Zgc` → `GcBackend::Zgc` →
`VmHeap::Zgc`), and **it is the default**: the `zgc` feature has been
default-ON since 2026-08-10 and `VmConfig`'s default `gc_algorithm` is
`Zgc`. A `--no-default-features` build falls back to Generational, and
`-XX:+UseGenerationalGC` is the escape hatch in every build.

The colored-pointer code is no longer a simulation. `src/zgc/vaddr.rs`
mints colored words, `src/zgc/barrier.rs::z_load` is the load barrier,
`src/zgc_concurrent.rs` runs concurrent marking (`CRATONVM_ZGC_CONC_START`),
`src/zgc/relocate.rs` compacts (kill switch `CRATONVM_ZGC_RELOCATE=0`),
and `src/zgc/generation.rs` implements the opt-in generational mode
(`CRATONVM_ZGC_GENERATIONAL=1`).

What is *not* yet production ZGC: the **JIT-side load barrier is
unwired**. `vm/src/jit/helpers.rs` and `heap.rs::ref_element_word_implausible`
still reach raw reference words and panic as a deliberate tripwire when a
colored one arrives. See
`docs/feature-designs/zgc-jit-load-barrier.md`.

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
