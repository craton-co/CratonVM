use super::*;

fn stw() -> cratonvm_gc_core::collector::StopTheWorldToken {
    // SAFETY: these tests run the heap single-threaded; no mutator runs.
    unsafe { cratonvm_gc_core::collector::StopTheWorldToken::new() }
}

fn collector() -> G1Collector {
    G1Collector::new(G1CollectorConfig {
        heap_size: 8 * 1024 * 1024,
        initial_heap_size: 8 * 1024 * 1024,
        region_size: 1024 * 1024,
        gc_worker_threads: 1,
        ..G1CollectorConfig::default()
    })
}

/// Idle: every non-`Old` region (here: the whole fresh arena). An open
/// mark (a concurrent start has run), a pending mixed phase or an armed
/// full compaction: refused, so no pause that reads old roots from the
/// slice is ever offered the split.
#[test]
fn gci_i13vg_g1_offers_the_split_only_to_a_plain_young_pause() {
    let gc = collector();
    let ranges = gc.parked_old_only_young_ranges().expect("idle, young-only");
    assert_eq!(ranges.len(), 1, "a fresh arena coalesces: {ranges:?}");
    assert_eq!(ranges[0].1 - ranges[0].0, 8 * 1024 * 1024);
    // An Old region drops out of the young set.
    gc.with_regions_mut(|regions| regions[3].set_region_type(RegionType::Old));
    let ranges = gc.parked_old_only_young_ranges().expect("still idle");
    let old_base = gc.regions.read()[3].data.addr();
    assert!(!ranges.iter().any(|&(lo, hi)| lo <= old_base && old_base < hi), "{ranges:?}");

    // A pending mixed phase.
    gc.marking_complete.store(true, Ordering::Release);
    gc.mixed_gc_remaining.store(1, Ordering::Relaxed);
    assert!(gc.parked_old_only_young_ranges().is_none(), "mixed");
    gc.marking_complete.store(false, Ordering::Release);
    gc.mixed_gc_remaining.store(0, Ordering::Relaxed);

    // An armed full compaction.
    gc.full_compaction_armed.store(true, Ordering::Release);
    assert!(gc.parked_old_only_young_ranges().is_none(), "full compaction");
    gc.full_compaction_armed.store(false, Ordering::Release);

    // An open concurrent mark (the initial mark has run).
    gc.start_concurrent_mark(&stw());
    assert!(gc.parked_old_only_young_ranges().is_none(), "open mark");
}
