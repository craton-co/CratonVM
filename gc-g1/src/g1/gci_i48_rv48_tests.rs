/// `SharedEvac::tlab_alloc` answers a refused Survivor claim as the cap
/// only on the shared-PLAB arm (the arm whose `claim_pool_slot` counts
/// claims against it); the per-worker arm's pool exhaustion keeps the
/// shared-destination scan. Needles split with `concat!` so this test's
/// own text is not another occurrence.
#[test]
fn gci_i48_rv48_the_cap_answer_is_the_shared_plab_arms_only() {
    let src = concat!(include_str!("../g1/mod.rs"), "\n", include_str!("../g1/value_from_unaligned_ptr.rs"), "\n", include_str!("../g1/config.rs"), "\n", include_str!("../g1/needs_gc_recount_due.rs"), "\n", include_str!("../g1/collector.rs"), "\n", include_str!("../g1/set_resolved_compact_primitive_field.rs"), "\n", include_str!("../g1/futile_young_escalation_count.rs"), "\n", include_str!("../g1/count_unretired_forwards.rs"), "\n", include_str!("../g1/collect_outgoing_cross_region_edges.rs"), "\n", include_str!("../g1/mark_root_grid_place.rs"), "\n", include_str!("../g1/record_collection_with_phases_within.rs"), "\n", include_str!("../g1/bump_satb_pre_barrier_epoch.rs"), "\n", include_str!("../g1/evac_copy_fits_a_region.rs"), "\n", include_str!("../g1/old_plab_bytes_for.rs")).replace("\r\n", "\n");
    let guarded = concat!(
        "&& tlab.dest_type == RegionType::Survivor\n",
        "                && self.survivor_plab\n",
        "                && self.collector.",
        "survivor_cap_reached()\n",
    );
    assert_eq!(src.matches(guarded).count(), 1, "the guarded early return");
    let unguarded = concat!(
        "&& tlab.dest_type == RegionType::Survivor\n",
        "                && self.collector.",
        "survivor_cap_reached()",
    );
    assert_eq!(src.matches(unguarded).count(), 0, "no unguarded copy left");
}
