// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Memory management: garbage collector, object model, allocation.
//!
//! The core GC and heap implementations live in the `cratonvm-gc` crate.
//! This module re-exports them and adds VM-specific functionality:
//! - [`roots`] — GC root scanning (depends on VM-internal types)
//! - [`gc::update_all_roots`] — post-GC root remapping (depends on VM-internal types)
//! - [`native_roots`] — uniform registry letting native side-tables register
//!   their held `ObjectRef`s as GC roots (scan + post-move remap) without
//!   editing `roots`/`gc` directly
//! - [`addr_keyed`] — post-collection re-key + sweep for address-keyed
//!   side-tables that must NOT root their keys (caches, as opposed to the
//!   root sources in `native_roots`)
//! - [`reclaim_guard`] — the flag-free "this receiver points into memory the
//!   collector already reclaimed" verdict, shared by the failure paths that
//!   see an all-zero `ClassId(0)` header

// Re-export submodules from the gc crate so that existing
// `use crate::memory::{arena, heap, ...}` paths continue to work.
pub use cratonvm_gc::arena;
pub use cratonvm_gc::card_table;
pub use cratonvm_gc::collector;
pub use cratonvm_gc::gen_heap;
pub use cratonvm_gc::heap;
pub use cratonvm_gc::old_gen;
pub use cratonvm_gc::vm_heap;

// VM-local modules that depend on VM-internal types.
pub mod addr_keyed;
pub mod gc;
pub mod native_roots;
pub mod reclaim_guard;
pub use reclaim_guard::corrupt_cell_exit_summary;
pub mod roots;
pub mod smuggled_longs;

// Top-level re-exports for convenience.
pub use cratonvm_gc::collector::GarbageCollector;
pub use cratonvm_gc::gen_heap::GenerationalHeap;
pub use cratonvm_gc::heap::{ArrayElementType, Heap, ObjectHeader, ObjectKind};
pub use cratonvm_gc::vm_heap::{GcBackend, VmHeap};
pub use cratonvm_gc::MonitorCleanup;
