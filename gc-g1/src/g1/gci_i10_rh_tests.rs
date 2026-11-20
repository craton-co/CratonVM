use super::*;

/// On the legacy arm: `CRATONVM_G1_MIXED_HOTSPOT_CHOICE` is default ON
/// since 2026-10-17, and each test turns the HotSpot arm on itself.
fn collector() -> G1Collector {
    let mut gc = G1Collector::new(G1CollectorConfig {
        heap_size: 16 * 1024 * 1024,
        initial_heap_size: 16 * 1024 * 1024,
        region_size: 1024 * 1024,
        ..G1CollectorConfig::default()
    });
    gc.mixed_hotspot_choice = false;
    gc
}

/// An Old region that held half a region at cleanup, 40 % of the region marked
/// live (so 10 % garbage), and then took `promoted` bytes of promotions.
fn stage(gc: &G1Collector, idx: usize, promoted: usize) {
    let rs = gc.config.region_size;
    gc.with_regions_mut(|regions| {
        let r = &mut regions[idx];
        r.set_region_type(RegionType::Old);
        r.set_cursor(rs / 2);
        r.live_bytes = rs / 2 - rs / 10;
        r.gc_efficiency = r.live_bytes as f64 / rs as f64;
        r.cursor_at_cleanup = rs / 2;
        r.set_cursor(rs / 2 + promoted);
    });
}

#[test]
fn gci_i10rh_promoted_bytes_are_live_on_the_hotspot_arm_only() {
    let mut gc = collector();
    let rs = gc.config.region_size;
    stage(&gc, 3, rs / 2);
    // Legacy: 40 % live, a candidate. HotSpot: 90 % live, above the 85 % threshold.
    assert_eq!(gc.select_old_regions_for_mixed_gc_within(false), vec![3]);
    gc.mixed_hotspot_choice = true;
    assert!(gc.select_old_regions_for_mixed_gc_within(false).is_empty());
}

#[test]
fn gci_i10rh_the_hotspot_minimum_is_taken_over_a_spent_budget() {
    let mut gc = collector();
    gc.config.max_gc_pause_ms = 0; // a zero budget
    stage(&gc, 3, 0);
    stage(&gc, 5, 0);
    // Legacy: max_old = 16 * 10 / 100 = 1.
    assert_eq!(gc.select_old_regions_for_mixed_gc_within(false).len(), 1);
    // HotSpot: max_old = ceil(1.6) = 2, and a minimum of 2 ignores the budget.
    gc.mixed_hotspot_choice = true;
    gc.mixed_min_old.store(2, Ordering::Relaxed);
    assert_eq!(gc.select_old_regions_for_mixed_gc_within(false), vec![3, 5]);
}

#[test]
fn gci_i10rh_region_accessors_fall_back_without_mark_data() {
    let gc = collector();
    let rs = gc.config.region_size;
    stage(&gc, 3, rs / 2);
    gc.with_regions_mut(|regions| {
        let r = &mut regions[3];
        assert_eq!(r.mixed_live(false), rs / 2 - rs / 10);
        assert_eq!(r.mixed_live(true), rs - rs / 10);
        assert_eq!(r.mixed_reclaimable(false), rs / 2 + rs / 10);
        assert_eq!(r.mixed_reclaimable(true), rs / 10);
        r.cursor_at_cleanup = 0; // no mark-data stamp: the switch arm reads legacy
        assert_eq!(r.mixed_live(true), r.live_bytes);
        assert_eq!(r.mixed_evac_cost_ns(2, true), 2 * r.live_bytes as u64);
    });
}
