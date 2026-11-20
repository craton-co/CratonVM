use super::*;

struct NoMonitors;
impl MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

fn stw() -> cratonvm_gc_core::collector::StopTheWorldToken {
    // SAFETY: these tests run the heap single-threaded; no mutator runs.
    unsafe { cratonvm_gc_core::collector::StopTheWorldToken::new() }
}

/// 16 MiB, fully committed, 1 MiB regions, no percentage cap on the old
/// CSet, a 1 ms pause goal and a copy price of exactly 1 ns per byte. On
/// the legacy arms of `CRATONVM_G1_MIXED_HOTSPOT_CHOICE` and
/// `CRATONVM_G1_EAGER_RECLAIM_DURING_MARK` (both default ON since
/// 2026-10-17); a test that needs a switch arm sets it itself.
fn collector() -> G1Collector {
    let mut gc = G1Collector::new(G1CollectorConfig {
        heap_size: 16 * 1024 * 1024,
        initial_heap_size: 16 * 1024 * 1024,
        region_size: 1024 * 1024,
        max_gc_pause_ms: 1,
        old_cset_region_threshold_percent: 100,
        mixed_gc_live_threshold_percent: 85,
        heap_waste_percent: 0,
        gc_worker_threads: 1,
        ..G1CollectorConfig::default()
    });
    gc.evac_ns_per_byte.store(1, Ordering::Relaxed);
    gc.evac_cost_samples.store(1, Ordering::Relaxed);
    gc.mixed_hotspot_choice = false;
    gc.eager_reclaim_during_mark = false;
    gc
}

/// An Old region with mark data: half a region allocated, `live` of it
/// marked live.
fn stage_old(gc: &G1Collector, idx: usize, live: usize) {
    let rs = gc.config.region_size;
    gc.with_regions_mut(|regions| {
        let r = &mut regions[idx];
        r.set_region_type(RegionType::Old);
        r.set_cursor(rs / 2);
        r.live_bytes = live;
        r.gc_efficiency = live as f64 / rs as f64;
    });
}

/// Three equal candidates at 400 KiB live (409 600 ns each); budget
/// 1 000 000 - 100 000 (fix-up EMA) = 900 000 ns.
fn stage_three(gc: &G1Collector) {
    for idx in [3, 5, 7] {
        stage_old(gc, idx, 400 * 1024);
    }
    gc.fixup_ns_ema.store(100_000, Ordering::Relaxed);
    gc.fixup_regions_ema.store(1, Ordering::Relaxed);
}

#[test]
fn gci_i11gm_the_test_hook_and_the_pauses_choose_through_one_selector() {
    let gc = collector();
    stage_three(&gc);
    let regions = gc.regions.read();
    let choice = gc.choose_old_cset(&regions, &RegionSet::new(), OldCsetSource::Mixed, false);
    drop(regions);
    // 409 600 + 409 600 fits 900 000; the third does not.
    assert_eq!(choice.picked, vec![3, 5]);
    assert!(choice.budget_capped);
    assert!(!choice.fixup_charged);
    assert_eq!(choice.available, 3);
    assert_eq!(gc.select_old_regions_for_mixed_gc_within(false), choice.picked);
}

#[test]
fn gci_i11gm_the_cost_model_arm_charges_the_marginal_fixup_on_the_production_path() {
    let gc = collector();
    stage_three(&gc);
    let regions = gc.regions.read();
    // 409 600 + 100 000 per candidate (one to-space region, no rset
    // source): the second (1 019 200) no longer fits 900 000.
    let choice = gc.choose_old_cset(&regions, &RegionSet::new(), OldCsetSource::Mixed, true);
    drop(regions);
    assert_eq!(choice.picked, vec![3]);
    assert!(choice.fixup_charged);
    gc.note_old_cset_census(&choice, 0);
    assert_eq!(gc.mixed_fixup_charged_pauses.load(Ordering::Relaxed), 1);
    assert_eq!(gc.mixed_cset_capped_budget.load(Ordering::Relaxed), 1);
    assert_eq!(gc.mixed_old_regions_selected.load(Ordering::Relaxed), 1);
}

#[test]
fn gci_i11gm_the_exclusion_set_and_the_plan_and_full_compaction_arms() {
    let gc = collector();
    stage_three(&gc);
    let regions = gc.regions.read();
    let mut exclude = RegionSet::new();
    exclude.insert(3);
    let excluded = gc.choose_old_cset(&regions, &exclude, OldCsetSource::Mixed, false);
    assert_eq!(excluded.picked, vec![5, 7]);
    let plan = gc.choose_old_cset(&regions, &RegionSet::new(), OldCsetSource::Plan, false);
    assert!(plan.picked.is_empty());
    assert_eq!(plan.available, 0);
    let full = gc.choose_old_cset(
        &regions,
        &RegionSet::new(),
        OldCsetSource::FullCompaction { young_bytes: 0 },
        true,
    );
    drop(regions);
    // Uncapped and unbudgeted: every region `full_compaction_candidates`
    // admits, and no fix-up price read.
    assert!(!full.budget_capped);
    assert!(!full.fixup_charged);
    assert_eq!(full.max_old, usize::MAX);
    assert_eq!(full.picked.len(), full.available);
    // The census counts a compaction plan's sources as selected.
    gc.note_old_cset_census(&plan, 4);
    assert_eq!(gc.mixed_old_regions_selected.load(Ordering::Relaxed), 4);
}

#[test]
fn gci_i11gm_a_compaction_pause_spends_the_mixed_count_only_on_the_default_arm() {
    let mut gc = collector();
    gc.marking_complete.store(true, Ordering::Relaxed);
    gc.mixed_gc_remaining.store(2, Ordering::Relaxed);
    gc.spend_mixed_count(true);
    assert_eq!(gc.mixed_gc_remaining.load(Ordering::Relaxed), 1, "default arm: spent");
    assert_eq!(gc.mixed_count_compaction_pauses.load(Ordering::Relaxed), 1);

    gc.mixed_count_skips_compaction = true;
    gc.spend_mixed_count(true);
    assert_eq!(gc.mixed_gc_remaining.load(Ordering::Relaxed), 1, "switch arm: kept");
    assert!(gc.marking_complete.load(Ordering::Relaxed));
    assert_eq!(gc.mixed_count_compaction_pauses.load(Ordering::Relaxed), 2);

    gc.spend_mixed_count(false);
    assert_eq!(gc.mixed_gc_remaining.load(Ordering::Relaxed), 0);
    assert!(!gc.marking_complete.load(Ordering::Relaxed), "the last mixed pause closes the phase");

    // Outside a mixed phase nothing is spent and nothing is counted.
    gc.spend_mixed_count(true);
    assert_eq!(gc.mixed_gc_remaining.load(Ordering::Relaxed), 0);
    assert_eq!(gc.mixed_count_compaction_pauses.load(Ordering::Relaxed), 2);
}

#[test]
fn gci_i11gm_gray_entries_inside_a_freed_span_are_dropped_and_no_others() {
    let gc = collector();
    gc.mark_worklist.lock().extend([0x1000, 0x2000, gray_entry(0x2800, 3), 0x3000]);
    if let Some(d) = gc.mark_deques.first() {
        d.lock().extend([0x2400, 0x5000]);
    }
    let dropped = gc.drop_gray_entries_in_ranges(&[(0x2000, 0x3000)]);
    let in_deques = usize::from(!gc.mark_deques.is_empty());
    assert_eq!(dropped, 2 + in_deques);
    assert_eq!(*gc.mark_worklist.lock(), vec![0x1000, 0x3000]);
    if let Some(d) = gc.mark_deques.first() {
        assert_eq!(*d.lock(), vec![0x5000]);
    }
    assert_eq!(gc.drop_gray_entries_in_ranges(&[]), 0);
}

/// Finding 6: a dead PRIMITIVE humongous array goes at the first pause of
/// an open mark cycle on the switch arm, and stays (the pre-existing
/// `an_open_mark_cycle_suppresses_eager_humongous_reclaim`) on the default
/// arm.
#[test]
fn gci_i11gm_an_open_mark_cycle_reclaims_a_dead_primitive_span_on_the_switch_arm_only() {
    for (switch, kept) in [(false, 1usize), (true, 0usize)] {
        let mut gc = collector();
        gc.eager_reclaim_during_mark = switch;
        let _dead = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 100_000);
        assert_eq!(gc.count_regions(RegionType::HumongousStart), 1);
        gc.start_concurrent_mark(&stw());
        let _churn = gc.alloc_object(ClassId::new(1), 1);
        let mut roots: Vec<ObjectRef> = vec![];
        gc.young_collection(&mut roots, &NoMonitors);
        assert_eq!(
            gc.count_regions(RegionType::HumongousStart),
            kept,
            "switch={switch}"
        );
        assert_eq!(
            gc.eager_mark_spans.load(Ordering::Relaxed),
            (1 - kept) as u64,
            "switch={switch}"
        );
    }
}

/// Finding 6: a gray entry naming the dead span goes with it.
#[test]
fn gci_i11gm_a_gray_entry_naming_a_reclaimed_span_is_dropped() {
    let mut gc = collector();
    gc.eager_reclaim_during_mark = true;
    let dead = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 100_000);
    let addr = dead.as_ptr() as usize;
    gc.start_concurrent_mark(&stw());
    gc.mark_worklist.lock().push(addr);
    let _churn = gc.alloc_object(ClassId::new(1), 1);
    let mut roots: Vec<ObjectRef> = vec![];
    gc.young_collection(&mut roots, &NoMonitors);
    assert_eq!(gc.count_regions(RegionType::HumongousStart), 0);
    assert!(!gc.gray_set_contains(addr), "the gray entry must go with its span");
    assert_eq!(gc.eager_mark_gray_dropped.load(Ordering::Relaxed), 1);
}

/// Finding 6: only a primitive array -- a dead REFERENCE-array span is
/// still kept while the cycle is open, on the switch arm too.
#[test]
fn gci_i11gm_a_reference_array_span_is_not_reclaimed_during_marking() {
    let mut gc = collector();
    gc.eager_reclaim_during_mark = true;
    let _dead = gc.alloc_array(ClassId::new(0), ArrayElementType::Reference, 100_000);
    assert_eq!(gc.count_regions(RegionType::HumongousStart), 1);
    gc.start_concurrent_mark(&stw());
    let _churn = gc.alloc_object(ClassId::new(1), 1);
    let mut roots: Vec<ObjectRef> = vec![];
    gc.young_collection(&mut roots, &NoMonitors);
    assert_eq!(gc.count_regions(RegionType::HumongousStart), 1);
    assert_eq!(gc.eager_mark_spans.load(Ordering::Relaxed), 0);
}
