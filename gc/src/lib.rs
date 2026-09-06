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
// A RATCHET, not a style preference. `duplicate_macro_attributes` fired on
// `tlab.rs` and was read past, and what it was reporting was that a test had
// stopped being a test: an insertion between `#[test]` and its `fn` moved the
// attribute onto the item below, so `retire_charges_the_thread_for_...` was
// registered TWICE and `install_tail_filler_always_consumes_the_tlab` was
// registered NOT AT ALL. The suite count went up, nothing failed, and a
// heap-corruption guard silently stopped running for as long as it took someone
// to compare `--list` against the source.
//
// Denying it makes the next occurrence a build failure. It is the right lint for
// this: a duplicated `#[test]` has no legitimate use, and the duplicate is the
// *observable* half of a slip whose other half -- the orphaned function -- is
// invisible in test output. Worth adding workspace-wide; scoped here because
// this is where it happened and where the largest test module lives.
#![deny(duplicate_macro_attributes)]

/// The GC slice of the process-wide typed configuration.
///
/// Every `CRATONVM_*` flag this crate reads is a field on
/// [`cratonvm_types::GcFlags`], parsed once at first use. Before the typed
/// config existed each of these was an independent `cratonvm_types::flags::runtime_var_os` call
/// wrapped in its own `OnceLock`; see `flag-census.md` for the
/// inventory and `cratonvm_types::flags` for the latching rules.
#[inline]
pub(crate) fn gc_flags() -> &'static cratonvm_types::GcFlags {
    &cratonvm_types::flags().gc
}

pub mod a2dbg;
pub mod arena;
// The ONE implementation of "a non-reference value was stored into a slot the
// class declares as a reference" (W7-84-primitive-in-reference-store.md).
//
// The STORE and READ primitives -- `box_for_reference_slot`,
// `unbox_reference_slot`, `needs_reference_box` -- are still `pub(crate)`, and
// for exactly the original reason: the module exists so `gen_heap`, `zgc`,
// `g1` and `heap` cannot drift apart again, and exporting the boxing primitive
// would invite a fifth caller with a fifth opinion. Nothing outside this crate
// can reach them, and `gc/tests/primitive_in_reference_slot.rs`'s ratchet still
// pins that exactly four files call them.
//
// The MODULE became `pub` on 2026-09-01 for three items that are not stores at
// all -- `expect_primitive_into_reference`, `ExpectedPrimitiveIntoReference`
// and `expected_primitive_into_reference_count`. They exist so ONE known
// producer, `vm/src/vm/vm_object.rs`'s class-mirror populator, can declare its
// two deliberate `ClassId`-over-`java.lang.Class.cachedConstructor` writes
// EXPECTED, instead of the W7-84 guard spending its entire default-run
// rate-limit budget reporting the VM to itself (~33 stores, ~12 stderr WARN
// lines, on a hello-world -- so a genuine third-party store arrived after the
// limiter was already spent). A `pub use` re-export here would have worked
// equally well; `pub mod` was chosen because the module doc is where a reader
// has to end up anyway to learn what the scope means, and a re-export hides
// that path.
pub mod autobox;
pub mod blocked_access_debug;
pub mod card_table;
pub mod class_unloading;
pub mod collector;
pub mod compact_header;
pub mod compressed_oops;
pub mod concurrent_mark;
pub mod evac_pool;
pub mod external_roots;
pub mod g1;
pub mod g1_cards;
pub mod g1_concurrent;
pub mod heap_bitmap;
pub mod heap_geometry;
pub mod heap_reservation;
pub mod gc;
/// Card / remembered-set cost counters and the per-cycle collector-decision
/// record (see [`gc_metrics::gc_metrics_report`] and
/// [`gc_metrics::collector_decision_report`]).
pub mod gc_metrics;
pub mod gc_quiescence;
/// Parallel evacuation for the generational young (Cheney) copy phase — the
/// copy-then-CAS forwarding protocol, per-worker to-space buffers, and the
/// work-sharing closure. Driven only by [`gen_heap`]; the census counters are
/// public so a run can say whether the parallel path engaged.
pub mod gen_evac;
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
/// Heap backing store: reserve address space, commit it in granules, and give
/// it back. See the module docs for why the `alloc_zeroed` block it replaces
/// charged the whole of `-Xmx` at startup on Windows and never returned a byte.
pub mod reservation;
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
// The Z Garbage Collector backend, gated behind the `zgc` feature.
//
// The "1884-LOC stub with no in-workspace consumer" this comment used to
// claim has been false on both counts since the real collector landed:
// `vm_heap.rs` does `use crate::zgc::ZgcRealHeap` and carries `VmHeap::Zgc`
// arms behind the same cfg, and `cratonvm-vm` forwards the feature so
// `-XX:+UseZGC` really selects it. Two things of different maturity sit
// behind the one flag — a REAL memory-backed mark-sweep collector
// (`ZgcRealHeap`) and a metadata-only SIMULATION of OpenJDK's colored-pointer
// model (`ZgcCollector` / `ColoredPointer` / `LoadBarrier`). See the
// `[features]` comment in `gc/Cargo.toml` for the full split.
//
// Default-OFF is a pass-rate-parity decision, not a "nothing uses it" one.
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
pub use gc_metrics::{
    collector_decision_report, gc_metrics_report, CollectorDecision, GcMetricsRaw, GcMetricsReport,
};
pub use gen_heap::{
    clear_jit_read_bounds, clear_jit_ref_store_plan, jit_g1_barrier_addr, jit_read_bounds_addr,
    jit_ref_store_armed_markers, jit_ref_store_gate_addrs, jit_ref_store_post_skip_mask,
    jit_region_bounds_addr, publish_jit_read_bounds, publish_jit_ref_store_plan,
    publish_jit_ref_store_plan_masked, set_jit_ref_store_post_active, set_jit_ref_store_pre_active,
    GenerationalHeap, HeapStats, HeapStatsSnapshot, JitG1BarrierTable, JitReadBoundsTable,
    JitRefStoreGates, JitRegionBoundsTable, JIT_G1_BARRIER, JIT_READ_BOUNDS, JIT_REF_STORE_GATES,
    JIT_REGION_BOUNDS, JIT_YOUNG_FLOOR_AGE_ZERO,
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
