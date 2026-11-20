    use super::*;
    use std::collections::HashMap;
    // =======================================================================
    // G1 correctness audit (g1-audit.md)
    // =======================================================================

    use cratonvm_gc_core::gc_metrics::{g1_cycle_kind, g1_degraded, last_g1_cycle};
    // ---------------------------------------------------------------
    // F-05 - the card table
    // ---------------------------------------------------------------

    use crate::g1_cards::G1_CARD_BYTES;

mod remap_after_gc;
#[allow(unused_imports)]
pub use remap_after_gc::*;
#[allow(unused_imports)]
use remap_after_gc::*;
mod a_humongous_object_awaiting;
#[allow(unused_imports)]
pub use a_humongous_object_awaiting::*;
#[allow(unused_imports)]
use a_humongous_object_awaiting::*;
mod collection_and_region_tests;
#[allow(unused_imports)]
pub use collection_and_region_tests::*;
#[allow(unused_imports)]
use collection_and_region_tests::*;
mod parallel_self_forward_clears;
#[allow(unused_imports)]
pub use parallel_self_forward_clears::*;
#[allow(unused_imports)]
use parallel_self_forward_clears::*;
mod rset_source_filter_needs_a_trusted;
#[allow(unused_imports)]
pub use rset_source_filter_needs_a_trusted::*;
#[allow(unused_imports)]
use rset_source_filter_needs_a_trusted::*;

pub fn many_region_config() -> G1CollectorConfig {
    G1CollectorConfig {
        heap_size: 4 * 1024 * 1024,
        region_size: 8 * 1024,
        promotion_age: 0,
        ..small_config()
    }
}

pub fn free_region_count(gc: &G1Collector) -> usize {
    gc.regions
        .read()
        .iter()
        .filter(|r| r.region_type() == RegionType::Free)
        .count()
}

pub fn consume_regions(gc: &G1Collector, count: usize) {
    let full = gc.config.region_size;
    let mut regions = gc.regions.write();
    for r in regions.iter_mut().rev().take(count) {
        r.set_region_type(RegionType::Eden);
        r.set_cursor(full);
    }
    gc.publish_region_census(&regions);
}

pub fn parallel_config(workers: usize, region_count: usize) -> G1CollectorConfig {
    let region_size = 1024 * 1024;
    G1CollectorConfig {
        heap_size: region_size * region_count,
        // F-16: fully committed, for the reason `small_config` states.
        initial_heap_size: region_size * region_count,
        region_size,
        max_gc_pause_ms: 200,
        ihop_percent: 45,
        promotion_age: 3,
        gc_worker_threads: workers,
        string_dedup_enabled: false,
        mixed_gc_count_target: 8,
        old_cset_region_threshold_percent: 10,
        mixed_gc_live_threshold_percent: 85,
        heap_waste_percent: 0,
    }
}
