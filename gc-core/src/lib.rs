#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(clippy::all)]
#![allow(deprecated)]
#![deny(duplicate_macro_attributes)]

pub mod a2dbg;
pub mod arena;
pub mod autobox;
pub mod blocked_access_debug;
pub mod card_table;
pub mod collector;
#[cfg(any(test, doc, feature = "lilliput-prototype"))]
pub mod compact_header;
pub mod compressed_oops;
pub mod concurrent_mark;
pub mod evac_pool;
pub mod external_roots;
pub mod gc;
pub mod gc_metrics;
pub mod gc_quiescence;
pub mod gen_young_sizing;
pub mod gpu_critical;
pub mod heap;
pub mod heap_bitmap;
pub mod heap_geometry;
pub mod heap_reservation;
pub mod jit_bounds;
pub mod mark_bitmap;
pub mod numa;
pub mod reference;
pub mod region;
pub mod reservation;
pub mod root_write_audit;
pub mod safepoint;
pub mod satb;
pub mod shadow_stack;
pub mod skip_bits;
pub mod stale_objref_debug;
pub mod thread_cpu;
pub mod tlab;
pub mod young_mark;
pub mod zero_forensics;

pub use cratonvm_types::{
    class_layout, compact_ref_fields_enabled, is_compact_object, object_instance_size,
    register_class_layout, set_compact_ref_fields_enabled, CompactLayout,
};
pub use cratonvm_types::loader_pin;
pub use card_table::CardTable;
pub use collector::{GarbageCollector, MonitorCleanup, StopTheWorldToken};
#[cfg(any(test, doc, feature = "lilliput-prototype"))]
pub use compact_header::{
    CompactAllocator, CompactHeader, CompactHeaderSavingsReport, HashCodeTable, HeaderView,
    LegacyHeaderFields, LockState, NarrowKlassTable,
};
#[cfg(any(test, doc, feature = "lilliput-prototype"))]
pub use compressed_oops::NarrowKlass;
pub use compressed_oops::{CompressedOop, CompressedOops, CompressedOopsMode};
pub use concurrent_mark::{ConcurrentGcPhase, ConcurrentGcState};
pub use gc::{
    install_class_info_hook, install_class_info_in_domain_hook, install_gc_finish_hook,
    install_gc_start_hook, resolve_class_info, resolve_class_info_in, ClassInfoHook,
    ClassInfoInDomainHook, JvmtiGcHook,
};
pub use gc_metrics::{
    gc_metrics_report, CollectorDecision, GcMetricsRaw, GcMetricsReport,
};
pub use jit_bounds::{
    jit_region_bounds_addr,
    clear_jit_read_bounds, clear_jit_ref_store_plan, install_jit_satb_pre_gate_cell,
    jit_g1_barrier_addr, jit_read_bounds_addr, jit_ref_store_armed_markers,
    jit_ref_store_gate_addrs, jit_ref_store_post_skip_mask, jit_satb_pre_gate_addr, publish_jit_read_bounds, publish_jit_ref_store_plan,
    publish_jit_ref_store_plan_masked, set_jit_ref_store_post_active,
    set_jit_ref_store_pre_active, JitG1BarrierTable, JitReadBoundsTable, JitRefStoreGates,
    JitRegionBoundsTable, JIT_G1_BARRIER, JIT_READ_BOUNDS, JIT_REF_STORE_GATES, JIT_YOUNG_FLOOR_AGE_ZERO,
};
pub use heap::{ArrayElementType, Heap, ObjectHeader, ObjectKind};
pub use mark_bitmap::MarkBitmap;
pub use numa::{NumaNode, NumaTopology};
pub use reference::{
    ReferenceEntry, ReferenceProcessingResult, ReferenceProcessingStats, ReferenceProcessor,
    ReferenceType,
};
pub use region::{RegionType, RememberedSet};
pub use satb::{flush_thread_satb_buffer, satb_thread_local_log, SatbQueue};
pub use tlab::Tlab;

#[inline]
pub fn gc_flags() -> &'static cratonvm_types::GcFlags {
    &cratonvm_types::flags().gc
}
pub mod skip_class_filter;
