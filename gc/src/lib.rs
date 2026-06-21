// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

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
//!
//! # Tuning
//!
//! For choosing a backend (Generational vs G1 vs ZGC), sizing the heap,
//! interpreting JFR GC events, and diagnosing common pause / allocation
//! symptoms, see [`docs/gc-tuning.md`](https://github.com/craton-co/cratonvm/blob/main/docs/gc-tuning.md)
//! in the workspace root.

pub mod arena;
pub mod card_table;
pub mod class_unloading;
pub mod collector;
pub mod compact_header;
pub mod compressed_oops;
pub mod concurrent_mark;
pub mod g1;
pub mod g1_concurrent;
pub mod gc;
pub mod gc_quiescence;
pub mod gen_heap;
pub mod heap;
pub mod mark_bitmap;
pub mod metaspace;
pub mod numa;
pub mod old_gen;
/// JNI critical-section object pin set (see [`pinned`]).
pub mod pinned;
pub mod reference;
pub mod region;
#[cfg(feature = "gpu-offload")]
pub mod safepoint;
pub mod satb;
pub mod shadow_stack;
pub mod tlab;
pub mod vm_heap;
// Round-7 cross-cutting Fix 4: ZGC stub (1884 LOC) gated behind the `zgc`
// feature. No in-workspace consumer references `zgc::*` today, so paying
// the compile-time + binary-size cost on every build is pure waste. Flip
// the feature on once a real consumer lands.
#[cfg(feature = "zgc")]
pub mod zgc;
// Task #55: ZGC concurrent-mark controller — mirrors the G1
// `ConcurrentMarkController` shape so the two converge once ZGC moves off
// the page-storage simulation. Gated under the same `zgc` feature.
#[cfg(feature = "zgc")]
pub mod zgc_concurrent;

pub use collector::{GarbageCollector, MonitorCleanup, StopTheWorldToken};
pub use compact_header::{
    CompactAllocator, CompactHeader, CompactHeaderSavingsReport, HashCodeTable, HeaderView,
    LegacyHeaderFields, LockState, NarrowKlassTable,
};
pub use compressed_oops::{CompressedOop, CompressedOops, CompressedOopsMode, NarrowKlass};
pub use concurrent_mark::{ConcurrentGcPhase, ConcurrentGcState, ConcurrentMarker};
pub use g1::{G1CollectionType, G1Collector, G1CollectorConfig};
pub use g1_concurrent::{ConcurrentMarkController, ConcurrentMarkState};
pub use gc::{
    install_class_info_hook, install_gc_finish_hook, install_gc_start_hook, resolve_class_info,
    ClassInfoHook, JvmtiGcHook,
};
pub use gen_heap::{GenerationalHeap, HeapStats, HeapStatsSnapshot};
pub use heap::{ArrayElementType, Heap, ObjectHeader, ObjectKind};
pub use mark_bitmap::MarkBitmap;
pub use reference::{
    ReferenceEntry, ReferenceProcessingResult, ReferenceProcessingStats, ReferenceProcessor,
    ReferenceQueue, ReferenceType,
};
pub use region::{RegionHeap, RegionType, RememberedSet};
pub use satb::{flush_thread_satb_buffer, satb_thread_local_log, SatbBuffer, SatbQueue};
pub use tlab::Tlab;
pub use vm_heap::{GcBackend, VmHeap};
