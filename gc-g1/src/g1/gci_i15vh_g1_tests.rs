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

/// Only `Old` regions are in the ranges (neighbours coalesce), the ranges
/// are skipped when not asked for, and the trust bit is the last
/// cleanup's: false before any cycle, true after a clean one.
#[test]
fn gci_i15vh_g1_view_lists_old_regions_and_the_last_cleanups_trust() {
    let gc = collector();
    let (trusted, ranges) = gc.parked_mirror_keyed_view(true);
    assert!(!trusted, "no cycle has run");
    assert!(ranges.is_empty(), "a fresh arena has no Old region: {ranges:?}");
    gc.with_regions_mut(|regions| {
        regions[2].set_region_type(RegionType::Old);
        regions[3].set_region_type(RegionType::Old);
        regions[5].set_region_type(RegionType::Old);
    });
    let (b2, b3, b5, e3, e5) = {
        let regions = gc.regions.read();
        (
            regions[2].data.addr(),
            regions[3].data.addr(),
            regions[5].data.addr(),
            regions[3].data.addr() + regions[3].data.len(),
            regions[5].data.addr() + regions[5].data.len(),
        )
    };
    let (_, ranges) = gc.parked_mirror_keyed_view(true);
    let inside = |a: usize| ranges.iter().any(|&(lo, hi)| lo <= a && a < hi);
    assert!(inside(b2) && inside(b3) && inside(b5) && inside(e3 - 8) && inside(e5 - 8));
    let b4 = {
        let regions = gc.regions.read();
        regions[4].data.addr()
    };
    assert!(!inside(b4), "an eden / free region is not old: {ranges:?}");
    if b3 == b2 + 1024 * 1024 {
        assert!(ranges.len() <= 2, "adjacent Old regions coalesce: {ranges:?}");
    }
    let (_, none) = gc.parked_mirror_keyed_view(false);
    assert!(none.is_empty(), "ranges only when asked");

    // A clean cycle: the cleanup trusted its closure.
    gc.start_concurrent_mark(&stw());
    gc.remark(&stw(), &[]);
    while !gc.concurrent_mark_step(usize::MAX) {}
    gc.cleanup(&stw());
    assert!(gc.parked_mirror_keyed_view(false).0, "a clean cycle is trusted");
    // The next cycle's start withdraws the trust until its own cleanup.
    gc.start_concurrent_mark(&stw());
    assert!(!gc.parked_mirror_keyed_view(false).0, "an open cycle is not trusted yet");
}
