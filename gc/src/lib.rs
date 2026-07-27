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
//! For the backend architectures (Generational vs G1 vs ZGC), the VM↔GC
//! protocol, correctness status, tuning knobs and the diagnostic
//! switches, see [`docs/GC.md`](https://github.com/craton-co/cratonvm/blob/main/docs/GC.md)
//! in the workspace root.

// Pre-existing clippy lints in this low-level GC crate that are style/judgment
// calls rather than defects: many-argument internal collector entry points,
// raw-pointer header accessors, index-by-loop over region/descriptor tables,
// and complex collector types. Allowed crate-wide to keep `clippy -D warnings`
// green without churning audited GC hot paths. (`uninit_vec`: the buffers are
// fully written by the GC before any read — flagged for a future precise audit.)
#![allow(
    clippy::too_many_arguments,
    clippy::type_complexity,
    clippy::needless_range_loop,
    clippy::ptr_arg,
    clippy::not_unsafe_ptr_arg_deref,
    clippy::large_enum_variant,
    clippy::field_reassign_with_default,
    clippy::uninit_vec,
    clippy::no_effect,
    clippy::if_same_then_else,
    clippy::match_like_matches_macro,
    clippy::explicit_auto_deref,
    clippy::doc_lazy_continuation
)]

/// The GC slice of the process-wide typed configuration.
///
/// Every `CRATONVM_*` flag this crate reads is a field on
/// [`cratonvm_types::GcFlags`], parsed once at first use. Before the typed
/// config existed each of these was an independent `cratonvm_types::flags::runtime_var_os` call
/// wrapped in its own `OnceLock`; see `docs/internal/flag-census.md` for the
/// inventory and `cratonvm_types::flags` for the latching rules.
#[inline]
pub(crate) fn gc_flags() -> &'static cratonvm_types::GcFlags {
    &cratonvm_types::flags().gc
}

pub mod a2dbg;
pub mod arena;
pub mod blocked_access_debug;
pub mod card_table;
pub mod class_unloading;
pub mod collector;
pub mod compact_header;
pub mod compressed_oops;
pub mod concurrent_mark;
pub mod external_roots;
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
pub mod stale_objref_debug;
pub mod tlab;
pub mod vm_heap;
// Parallel young-generation marking (mark bitmap + scoped worker
// drain). Crate-internal: only `gen_heap` drives it.
mod young_mark;
pub mod zero_forensics;
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

// Compact reference-field layout registry lives in `cratonvm-types` (shared by
// classloading + gc). Re-export the GC/heap-facing API for convenience.
pub use cratonvm_types::{
    class_layout, compact_ref_fields_enabled, is_compact_object, object_body_size,
    register_class_layout, set_compact_ref_fields_enabled, CompactLayout,
};
// HIB-CV-24: class-loader liveness pinning registry (shared by gen_heap marker
// + native-builtins side-table). See `cratonvm_types::loader_pin`.
pub use collector::{GarbageCollector, MonitorCleanup, StopTheWorldToken};
pub use compact_header::{
    CompactAllocator, CompactHeader, CompactHeaderSavingsReport, HashCodeTable, HeaderView,
    LegacyHeaderFields, LockState, NarrowKlassTable,
};
pub use compressed_oops::{CompressedOop, CompressedOops, CompressedOopsMode, NarrowKlass};
pub use concurrent_mark::{ConcurrentGcPhase, ConcurrentGcState, ConcurrentMarker};
pub use cratonvm_types::loader_pin;
pub use g1::{G1CollectionType, G1Collector, G1CollectorConfig};
pub use g1_concurrent::{ConcurrentMarkController, ConcurrentMarkState};
pub use gc::{
    install_class_info_hook, install_gc_finish_hook, install_gc_start_hook, resolve_class_info,
    ClassInfoHook, JvmtiGcHook,
};
pub use gen_heap::{
    jit_region_bounds_addr, GenerationalHeap, HeapStats, HeapStatsSnapshot, JitRegionBoundsTable,
    JIT_REGION_BOUNDS,
};
pub use heap::{ArrayElementType, Heap, ObjectHeader, ObjectKind};
pub use mark_bitmap::MarkBitmap;
pub use reference::{
    ReferenceEntry, ReferenceProcessingResult, ReferenceProcessingStats, ReferenceProcessor,
    ReferenceQueue, ReferenceType,
};
// G1CORE-11: `RegionHeap` is intentionally NOT re-exported — it is an
// unused prototype with known-unsound conservative slot rewriting (see the
// `#[deprecated]` note on the type). The production region-based collector
// is `G1Collector` (`g1.rs`), reached through `VmHeap::G1`.
pub use region::{RegionType, RememberedSet};
pub use satb::{flush_thread_satb_buffer, satb_thread_local_log, SatbBuffer, SatbQueue};
pub use tlab::Tlab;
pub use vm_heap::{GcBackend, VmHeap};
