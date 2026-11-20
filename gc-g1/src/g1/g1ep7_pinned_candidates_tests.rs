use super::*;

fn collector() -> G1Collector {
    G1Collector::new(G1CollectorConfig {
        heap_size: 16 * 1024 * 1024,
        initial_heap_size: 16 * 1024 * 1024,
        region_size: 1024 * 1024,
        ..G1CollectorConfig::default()
    })
}

/// An Old region half full at cleanup with 10 % of the region garbage: a
/// mixed candidate on either arm.
fn stage(gc: &G1Collector, idx: usize) {
    let rs = gc.config.region_size;
    gc.with_regions_mut(|regions| {
        let r = &mut regions[idx];
        r.set_region_type(RegionType::Old);
        r.set_cursor(rs / 2);
        r.live_bytes = rs / 2 - rs / 10;
        r.gc_efficiency = r.live_bytes as f64 / rs as f64;
        r.cursor_at_cleanup = rs / 2;
    });
}

#[test]
fn g1ep7_a_mixed_phase_whose_only_candidates_are_pinned_ends() {
    let gc = collector();
    stage(&gc, 3);
    gc.marking_complete.store(true, Ordering::Relaxed);
    gc.mixed_gc_remaining.store(5, Ordering::Relaxed);
    let regions = gc.regions.read();
    let mut pinned = RegionSet::new();
    pinned.insert(3);
    let blocked = gc.choose_old_cset(&regions, &pinned, OldCsetSource::Mixed, false);
    let open = gc.choose_old_cset(&regions, &RegionSet::new(), OldCsetSource::Mixed, false);
    drop(regions);
    assert_eq!(open.picked, vec![3]);
    assert!(!gc.end_mixed_phase_if_only_pinned_left(&open), "a pause that takes a candidate goes on");
    assert!(gc.needs_mixed_gc());
    assert!(blocked.picked.is_empty());
    assert!(gc.end_mixed_phase_if_only_pinned_left(&blocked), "nothing takeable is left");
    assert!(!gc.needs_mixed_gc());
    assert_eq!(gc.mixed_gc_remaining.load(Ordering::Relaxed), 0);
}

#[test]
fn g1ep7_an_empty_old_half_with_nothing_excluded_leaves_the_count_alone() {
    let gc = collector();
    gc.marking_complete.store(true, Ordering::Relaxed);
    gc.mixed_gc_remaining.store(5, Ordering::Relaxed);
    let regions = gc.regions.read();
    let none = gc.choose_old_cset(&regions, &RegionSet::new(), OldCsetSource::Mixed, false);
    drop(regions);
    assert!(!gc.end_mixed_phase_if_only_pinned_left(&none));
    assert_eq!(gc.mixed_gc_remaining.load(Ordering::Relaxed), 5);
}
