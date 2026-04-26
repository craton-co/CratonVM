//! Memory management: garbage collector, object model, allocation.
//!
//! This crate contains:
//! - [`Heap`] -- semi-space heap for Java objects and arrays
//! - [`GenerationalHeap`] -- generational heap with young/old gen and card table
//! - [`ObjectHeader`] -- header stored at the start of every heap allocation
//! - [`ObjectKind`] -- distinguishes objects from arrays
//! - [`ArrayElementType`] -- component type for Java arrays
//! - [`arena::Arena`] -- linear bump-pointer arena used by the semi-space GC
//! - [`collector::GarbageCollector`] -- trait abstracting heap implementations
//! - [`collector::MonitorCleanup`] -- trait for GC-time monitor remapping
//! - [`card_table::CardTable`] -- card table for old->young reference tracking
//! - [`old_gen::OldGen`] -- free-list allocator for old generation

pub mod arena;
pub mod card_table;
pub mod class_unloading;
pub mod collector;
pub mod gc_quiescence;
pub mod compact_header;
pub mod compressed_oops;
pub mod concurrent_mark;
pub mod g1;
pub mod gc;
pub mod gen_heap;
pub mod heap;
pub mod mark_bitmap;
pub mod metaspace;
pub mod numa;
pub mod old_gen;
pub mod reference;
pub mod region;
pub mod satb;
pub mod tlab;
pub mod vm_heap;
pub mod zgc;

pub use collector::{GarbageCollector, MonitorCleanup};
pub use gc::{install_gc_finish_hook, install_gc_start_hook, JvmtiGcHook};
pub use vm_heap::{GcBackend, VmHeap};
pub use compact_header::{CompactAllocator, CompactHeader, CompactHeaderSavingsReport, HashCodeTable, HeaderView, LegacyHeaderFields, LockState, NarrowKlassTable};
pub use compressed_oops::{CompressedOop, CompressedOops, CompressedOopsMode, NarrowKlass};
pub use concurrent_mark::{ConcurrentGcPhase, ConcurrentGcState, ConcurrentMarker};
pub use g1::{G1Collector, G1CollectionType, G1CollectorConfig};
pub use gen_heap::{GenerationalHeap, HeapStats, HeapStatsSnapshot};
pub use heap::{ArrayElementType, Heap, ObjectHeader, ObjectKind};
pub use mark_bitmap::MarkBitmap;
pub use region::{RegionHeap, RegionType, RememberedSet};
pub use reference::{ReferenceEntry, ReferenceProcessor, ReferenceProcessingResult, ReferenceProcessingStats, ReferenceQueue, ReferenceType};
pub use satb::{SatbBuffer, SatbQueue};
pub use tlab::Tlab;
