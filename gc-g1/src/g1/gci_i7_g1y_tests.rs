use super::*;

/// `--verbose:gc` alone no longer selects the whole-heap CSet verifier;
/// debug builds and the opt-in still do.
#[test]
fn gci_i7_g1y_verbose_gc_alone_does_not_select_the_whole_heap_verifier() {
    // (debug_build, gc_log_enabled, on_verbose_gc) -> unbounded
    assert!(!verify_unbounded_for(false, true, false), "--verbose:gc, switch off");
    assert!(verify_unbounded_for(false, true, true), "--verbose:gc, switch on");
    assert!(!verify_unbounded_for(false, false, true), "switch on without --verbose:gc");
    assert!(!verify_unbounded_for(false, false, false), "release, quiet");
    assert!(verify_unbounded_for(true, false, false), "debug build");
    assert!(verify_unbounded_for(true, true, false), "debug build with --verbose:gc");
}

/// The young-target arm, net of the pinned-out regions: the vi5g-g1f J
/// state (target at the floor 25, ~22 young regions pinned, young 26)
/// fires without the netting and does not with it.
#[test]
fn gci_i7_g1y_young_target_arm_nets_out_pinned_regions() {
    let (target, max) = (25, 307);
    assert!(young_target_arm_fires(26, target, max, 0), "the arm before i7");
    assert!(!young_target_arm_fires(26, target, max, 22), "22 of 26 pinned");
    assert!(!young_target_arm_fires(46, target, max, 22), "24 collectable");
    assert!(young_target_arm_fires(47, target, max, 22), "25 collectable");
    assert!(!young_target_arm_fires(3, target, max, 22), "saturates, never wraps");
    assert!(
        !young_target_arm_fires(400, max, max, 0),
        "a target at its ceiling is never a trigger"
    );
    assert!(!young_target_arm_fires(400, max + 1, max, 0));
}

/// Young and mixed records publish `jni_pinned_out + jit_pinned_out`; a
/// full record publishes 0 (gci i10/dl: it empties young).
#[test]
fn gci_i7_g1y_young_and_mixed_records_publish_the_pinned_kept_count() {
    let gc = G1Collector::new(G1CollectorConfig {
        heap_size: 8 * 1024 * 1024,
        initial_heap_size: 8 * 1024 * 1024,
        region_size: 1024 * 1024,
        gc_worker_threads: 1,
        ..G1CollectorConfig::default()
    });
    let stats = GcStats {
        objects_copied: 1,
        bytes_copied: 64,
        bytes_freed: 4096,
    };
    assert_eq!(gc.young_pinned_kept_regions(), 0);
    let pinned = G1PausePhases {
        jni_pinned_out: 2,
        jit_pinned_out: 3,
        ..G1PausePhases::default()
    };
    gc.record_collection_with_phases(G1CollectionType::YoungOnly, 10, &stats, pinned);
    assert_eq!(gc.young_pinned_kept_regions(), 5);
    gc.record_collection_with_phases(
        G1CollectionType::Full,
        10,
        &stats,
        G1PausePhases::default(),
    );
    assert_eq!(gc.young_pinned_kept_regions(), 0, "a full record empties young (gci i10/dl)");
    gc.record_collection_with_phases(G1CollectionType::YoungOnly, 10, &stats, pinned);
    assert_eq!(gc.young_pinned_kept_regions(), 5);
    gc.record_collection_with_phases(
        G1CollectionType::Mixed,
        10,
        &stats,
        G1PausePhases::default(),
    );
    assert_eq!(gc.young_pinned_kept_regions(), 0);
}
