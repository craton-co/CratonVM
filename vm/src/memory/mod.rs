//! Memory management: garbage collector, object model, allocation.
//!
//! The core GC and heap implementations live in the `rustjvm-gc` crate.
//! This module re-exports them and adds VM-specific functionality:
//! - [`roots`] — GC root scanning (depends on VM-internal types)
//! - [`gc::update_all_roots`] — post-GC root remapping (depends on VM-internal types)

// Re-export submodules from the gc crate so that existing
// `use crate::memory::{arena, heap, ...}` paths continue to work.
pub use rustjvm_gc::arena;
pub use rustjvm_gc::card_table;
pub use rustjvm_gc::collector;
pub use rustjvm_gc::gen_heap;
pub use rustjvm_gc::heap;
pub use rustjvm_gc::old_gen;
pub use rustjvm_gc::vm_heap;

// VM-local modules that depend on VM-internal types.
pub mod gc;
pub mod roots;

// Top-level re-exports for convenience.
pub use rustjvm_gc::collector::GarbageCollector;
pub use rustjvm_gc::gen_heap::GenerationalHeap;
pub use rustjvm_gc::vm_heap::{GcBackend, VmHeap};
pub use rustjvm_gc::heap::{ArrayElementType, Heap, ObjectHeader, ObjectKind};
pub use rustjvm_gc::MonitorCleanup;
