#![cfg_attr(not(test), deny(unsafe_code))]
#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(clippy::all)]
#![allow(deprecated)]
#![deny(duplicate_macro_attributes)]

pub use cratonvm_gc_core::{
    a2dbg, arena, autobox, blocked_access_debug, card_table, collector, compressed_oops,
    evac_pool, external_roots, gc, gc_flags, gc_quiescence, gen_young_sizing, gpu_critical, heap,
    heap_bitmap, heap_geometry, heap_reservation, jit_bounds, mark_bitmap, numa, reference, region,
    reservation, root_write_audit, safepoint, satb, shadow_stack, skip_bits, stale_objref_debug,
    thread_cpu, tlab, young_mark, zero_forensics,
};
#[cfg(feature = "lilliput-prototype")]
pub use cratonvm_gc_core::compact_header;

pub mod gc_metrics {
    pub use cratonvm_gc_core::gc_metrics::*;
    pub use crate::vm_heap::decision_report::collector_decision_report;
}

pub use cratonvm_gc_core::{
    class_layout, compact_ref_fields_enabled, flush_thread_satb_buffer, gc_metrics_report,
    install_class_info_hook, install_class_info_in_domain_hook, install_gc_finish_hook,
    install_gc_start_hook, is_compact_object, loader_pin, object_instance_size,
    register_class_layout, resolve_class_info, resolve_class_info_in, satb_thread_local_log,
    set_compact_ref_fields_enabled, ArrayElementType, ClassInfoHook, ClassInfoInDomainHook,
    CollectorDecision, CompactLayout, CompressedOop, CompressedOops, CompressedOopsMode,
    ConcurrentGcPhase, ConcurrentGcState, GarbageCollector, GcMetricsRaw, GcMetricsReport, Heap,
    JvmtiGcHook, MarkBitmap, MonitorCleanup, ObjectHeader, ObjectKind, ReferenceEntry,
    ReferenceProcessingResult, ReferenceProcessingStats, ReferenceProcessor, ReferenceType,
    RegionType, RememberedSet, SatbQueue, StopTheWorldToken, Tlab,
};
#[cfg(feature = "lilliput-prototype")]
pub use cratonvm_gc_core::{
    CompactAllocator, CompactHeader, CompactHeaderSavingsReport, HashCodeTable, HeaderView,
    LegacyHeaderFields, LockState, NarrowKlass, NarrowKlassTable,
};

pub use cratonvm_gc_gen::{
    concurrent_mark, gen_evac, gen_heap, old_gen, serial_pool_accounting, clear_jit_read_bounds,
    clear_jit_ref_store_plan, install_jit_satb_pre_gate_cell, jit_g1_barrier_addr,
    jit_read_bounds_addr, jit_ref_store_armed_markers, jit_ref_store_gate_addrs,
    jit_ref_store_post_skip_mask, jit_region_bounds_addr, jit_satb_pre_gate_addr,
    publish_jit_read_bounds, publish_jit_ref_store_plan, publish_jit_ref_store_plan_masked,
    set_jit_ref_store_post_active, set_jit_ref_store_pre_active, ConcurrentMarker,
    GenerationalHeap, HeapStats, HeapStatsSnapshot, JitG1BarrierTable, JitReadBoundsTable,
    JitRefStoreGates, JitRegionBoundsTable, JIT_G1_BARRIER, JIT_READ_BOUNDS, JIT_REF_STORE_GATES,
    JIT_REGION_BOUNDS, JIT_YOUNG_FLOOR_AGE_ZERO,
};

pub use cratonvm_gc_g1::{
    g1, g1_cards, g1_concurrent, g1_object_address_census, ConcurrentMarkController,
    ConcurrentMarkState, G1CollectionType, G1Collector, G1CollectorConfig,
};

#[cfg(feature = "zgc")]
pub use cratonvm_gc_zgc::{zgc, zgc_concurrent};

pub use gc_metrics::collector_decision_report;
pub mod vm_heap;
pub use vm_heap::{GcBackend, VmHeap};

///
/// With the token threaded through, the call compiles:
///
/// ```
/// use cratonvm_gc::Heap;
/// use cratonvm_gc::collector::{MonitorCleanup, StopTheWorldToken};
/// use std::collections::HashMap;
///
/// struct NoMonitors;
/// impl MonitorCleanup for NoMonitors {
///     fn remap_after_gc(&self, _: &cratonvm_types::PointerMap) {}
/// }
///
/// let heap = Heap::new();
/// let mut roots = Vec::new();
/// // SAFETY: this doctest has no other mutator threads.
/// let stw = unsafe { StopTheWorldToken::new() };
/// let _ = heap.collect_garbage(&stw, &mut roots, &NoMonitors);
/// ```
///
/// Safe construction of STW proof must not compile:
///
/// ```compile_fail
/// use cratonvm_gc::collector::StopTheWorldToken;
///
/// let _stw = StopTheWorldToken::new();
/// ```
#[cfg(doctest)]
#[allow(dead_code)]
struct _StopTheWorldTokenCompileFailDocs;

/// I-17: G1's three mark-cycle entry points require the same witness.
///
/// `start_concurrent_mark`, `remark` and `cleanup` are all STW phases —
/// `cleanup` reclassifies and frees regions — and until 2026-08-13 they were
/// the only such entry points on the collector taking no [`StopTheWorldToken`],
/// i.e. the one invariant in the G1 audit's §7 table enforced by nothing but a
/// comment. These doctests keep it a signature: a future refactor that drops
/// the parameter to "simplify" the call sites turns the suite red instead of
/// silently un-enforcing the phase.
///
/// ```compile_fail
/// use cratonvm_gc::{G1Collector, G1CollectorConfig};
///
/// let gc = G1Collector::new(G1CollectorConfig::default());
/// // Missing &StopTheWorldToken — should fail to compile.
/// gc.cleanup();
/// ```
///
/// ```compile_fail
/// use cratonvm_gc::{G1Collector, G1CollectorConfig};
///
/// let gc = G1Collector::new(G1CollectorConfig::default());
/// // Missing &StopTheWorldToken — should fail to compile.
/// gc.start_concurrent_mark();
/// ```
///
/// ```compile_fail
/// use cratonvm_gc::{G1Collector, G1CollectorConfig};
///
/// let gc = G1Collector::new(G1CollectorConfig::default());
/// // Missing &StopTheWorldToken — should fail to compile.
/// gc.remark(&[]);
/// ```
///
/// With the token threaded through, the cycle compiles:
///
/// ```
/// use cratonvm_gc::{G1Collector, G1CollectorConfig, StopTheWorldToken};
///
/// let gc = G1Collector::new(G1CollectorConfig::default());
/// // SAFETY: this doctest has no other mutator threads.
/// let stw = unsafe { StopTheWorldToken::new() };
/// gc.start_concurrent_mark(&stw);
/// gc.remark(&stw, &[]);
/// gc.cleanup(&stw);
/// ```
#[cfg(doctest)]
#[allow(dead_code)]
struct _G1MarkCycleStwCompileFailDocs;

/// The SATB final drain is an STW-only operation, and the compiler must say so.
///
/// `SatbQueue::deactivate_and_drain` has no happens-before edge against a
/// mutator that observed the gate ACTIVE before the `ACTIVE -> DRAINING` CAS
/// and then blocked on a shard lock: that writer never re-reads the gate, so
/// its entry can land after the drain and be missing from the snapshot the
/// sweep acts on. The `in_flight` quiescence wait added 2026-09-20 makes that
/// writer *visible* to the drain on a bounded budget, which narrows the window
/// to a writer suspended between its increment and its push — it does not
/// remove it. What makes the shipped code sound is still the remark pause,
/// i.e. a property of the CALLER, and until 2026-09-21 that property was
/// written down nowhere on a `pub fn`. The `&StopTheWorldToken` below is that
/// property, moved from a comment into the signature. See
/// `docs/feature-designs/jit-barrier-lowering.md`.
///
/// ```compile_fail
/// use cratonvm_gc::SatbQueue;
///
/// let q = SatbQueue::new();
/// q.activate();
/// // Missing &StopTheWorldToken — should fail to compile.
/// let _snapshot = q.deactivate_and_drain();
/// ```
///
/// A caller that is NOT at a safepoint has `deactivate_and_discard`, which
/// runs the same protocol and returns nothing — the entries it collects are
/// not a snapshot:
///
/// ```
/// use cratonvm_gc::SatbQueue;
///
/// let q = SatbQueue::new();
/// q.activate();
/// q.deactivate_and_discard();
/// assert!(!q.is_active());
/// ```
///
/// With the token, the snapshot-producing form compiles:
///
/// ```
/// use cratonvm_gc::{SatbQueue, StopTheWorldToken};
///
/// let q = SatbQueue::new();
/// q.activate();
/// // SAFETY: this doctest has no other mutator threads.
/// let stw = unsafe { StopTheWorldToken::new() };
/// let snapshot = q.deactivate_and_drain(&stw);
/// assert!(snapshot.is_empty());
/// ```
#[cfg(doctest)]
#[allow(dead_code)]
struct _SatbDeactivateStwCompileFailDocs;
