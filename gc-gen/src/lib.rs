#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(clippy::all)]
#![allow(deprecated)]
#![deny(duplicate_macro_attributes)]

pub use cratonvm_gc_core::gen_young_sizing;

pub mod concurrent_mark;
pub mod gen_evac;
pub mod gen_heap;
pub mod old_gen;
pub mod serial_pool_accounting;

pub use concurrent_mark::ConcurrentMarker;
pub use gen_heap::{
    clear_jit_read_bounds, clear_jit_ref_store_plan, install_jit_satb_pre_gate_cell,
    jit_g1_barrier_addr, jit_read_bounds_addr, jit_ref_store_armed_markers,
    jit_ref_store_gate_addrs, jit_ref_store_post_skip_mask, jit_region_bounds_addr,
    jit_satb_pre_gate_addr, publish_jit_read_bounds, publish_jit_ref_store_plan,
    publish_jit_ref_store_plan_masked, set_jit_ref_store_post_active,
    set_jit_ref_store_pre_active, GenerationalHeap, HeapStats, HeapStatsSnapshot,
    JitG1BarrierTable, JitReadBoundsTable, JitRefStoreGates, JitRegionBoundsTable, JIT_G1_BARRIER,
    JIT_READ_BOUNDS, JIT_REF_STORE_GATES, JIT_REGION_BOUNDS, JIT_YOUNG_FLOOR_AGE_ZERO,
};
