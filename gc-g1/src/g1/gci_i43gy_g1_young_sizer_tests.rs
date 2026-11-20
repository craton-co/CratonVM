use super::*;
use cratonvm_gc_core::gen_young_sizing::YoungGenSizing;

const MIB: usize = 1024 * 1024;

fn req(
    xmn: Option<usize>,
    new_size: Option<usize>,
    max_new_size: Option<usize>,
    new_ratio: Option<usize>,
) -> YoungGenSizing {
    YoungGenSizing {
        xmn,
        new_size,
        max_new_size,
        new_ratio,
    }
}

/// 32 regions of 1 MiB: default young bounds 1..=19.
fn collector() -> G1Collector {
    G1Collector::new(G1CollectorConfig {
        heap_size: 32 * MIB,
        initial_heap_size: 32 * MIB,
        region_size: MIB,
        gc_worker_threads: 1,
        max_gc_pause_ms: 200,
        ..G1CollectorConfig::default()
    })
}

#[test]
fn gci_i43gy_sizer_kinds_follow_g1_young_gen_sizer() {
    let none: Vec<String> = Vec::new();
    // -Xmn: NewSize = MaxNewSize, a fixed young length.
    let (s, w) = g1_young_sizer_from_flags(&req(Some(32 * MIB), None, None, None), MIB);
    assert_eq!(s, G1YoungSizer::MaxAndNewSize { min: 32, max: 32 });
    assert_eq!(w, none);
    assert!(!s.adaptive());
    assert!(s.ceiling_is_operators());
    assert_eq!(s.bounds(256), (32, 32));
    // -Xmn wins over a typed NewSize / MaxNewSize.
    let (s, _) =
        g1_young_sizer_from_flags(&req(Some(8 * MIB), Some(2 * MIB), Some(64 * MIB), None), MIB);
    assert_eq!(s, G1YoungSizer::MaxAndNewSize { min: 8, max: 8 });
    // NewSize alone: the minimum; the default maximum (60 %), adaptive.
    let (s, w) = g1_young_sizer_from_flags(&req(None, Some(16 * MIB), None, None), MIB);
    assert_eq!(s, G1YoungSizer::NewSizeOnly { min: 16 });
    assert_eq!(w, none);
    assert!(s.adaptive());
    assert!(!s.ceiling_is_operators());
    assert_eq!(s.bounds(256), (16, 153));
    assert_eq!(s.bounds(20), (16, 16), "the maximum never below the minimum");
    // MaxNewSize alone: the maximum; the default minimum (5 %), adaptive.
    let (s, _) = g1_young_sizer_from_flags(&req(None, None, Some(32 * MIB), None), MIB);
    assert_eq!(s, G1YoungSizer::MaxNewSizeOnly { max: 32 });
    assert!(s.adaptive());
    assert!(s.ceiling_is_operators());
    assert_eq!(s.bounds(256), (12, 32));
    assert_eq!(s.bounds(2048), (32, 32), "the minimum never above the maximum");
    // Both, different: adaptive between them.
    let (s, _) =
        g1_young_sizer_from_flags(&req(None, Some(16 * MIB), Some(64 * MIB), None), MIB);
    assert_eq!(s, G1YoungSizer::MaxAndNewSize { min: 16, max: 64 });
    assert!(s.adaptive());
    assert_eq!(s.bounds(256), (16, 64));
    // NewSize above MaxNewSize: HotSpot's warning, the maximum raised.
    let (s, w) =
        g1_young_sizer_from_flags(&req(None, Some(64 * MIB), Some(32 * MIB), None), MIB);
    assert_eq!(s, G1YoungSizer::MaxAndNewSize { min: 64, max: 64 });
    assert_eq!(
        w,
        vec!["NewSize (65536k) is greater than the MaxNewSize (32768k). A new max generation size of 65536k will be used."
            .to_string()]
    );
    // NewRatio alone: regions / (NewRatio + 1), fixed.
    let (s, w) = g1_young_sizer_from_flags(&req(None, None, None, Some(3)), MIB);
    assert_eq!(s, G1YoungSizer::NewRatio { ratio: 3 });
    assert_eq!(w, none);
    assert!(!s.adaptive());
    assert!(s.ceiling_is_operators());
    assert_eq!(s.bounds(256), (64, 64));
    assert_eq!(s.bounds(2), (1, 1), "floor of one region");
    // NewRatio with a size: overridden, with HotSpot's warning.
    let (s, w) =
        g1_young_sizer_from_flags(&req(None, Some(16 * MIB), None, Some(3)), MIB);
    assert_eq!(s, G1YoungSizer::NewSizeOnly { min: 16 });
    assert_eq!(
        w,
        vec!["-XX:NewSize and -XX:MaxNewSize override -XX:NewRatio".to_string()]
    );
    let (s, w) = g1_young_sizer_from_flags(&req(Some(32 * MIB), None, None, Some(2)), MIB);
    assert_eq!(s, G1YoungSizer::MaxAndNewSize { min: 32, max: 32 });
    assert_eq!(w.len(), 1);
    // Sizes below a region are one region; larger regions divide.
    let (s, _) = g1_young_sizer_from_flags(&req(Some(512 * 1024), None, None, None), MIB);
    assert_eq!(s, G1YoungSizer::MaxAndNewSize { min: 1, max: 1 });
    let (s, _) = g1_young_sizer_from_flags(&req(Some(64 * MIB), None, None, None), 4 * MIB);
    assert_eq!(s, G1YoungSizer::MaxAndNewSize { min: 16, max: 16 });
    // Nothing typed: the pre-i43 bounds, byte for byte.
    let (s, w) = g1_young_sizer_from_flags(&YoungGenSizing::default(), MIB);
    assert_eq!(s, G1YoungSizer::Defaults);
    assert_eq!(w, none);
    assert_eq!(s.bounds(32), (1, 19));
    assert_eq!(s.bounds(512), (25, 307));
    assert_eq!(s.bounds(1), (1, 1));
    assert!(s.adaptive());
    assert!(!s.ceiling_is_operators());
}

#[test]
fn gci_i43gy_sizer_encoding_round_trips() {
    for s in [
        G1YoungSizer::Defaults,
        G1YoungSizer::NewSizeOnly { min: 7 },
        G1YoungSizer::MaxNewSizeOnly { max: 9 },
        G1YoungSizer::MaxAndNewSize { min: 3, max: 11 },
        G1YoungSizer::NewRatio { ratio: 2 },
    ] {
        let (k, a, b) = s.encode();
        assert_eq!(G1YoungSizer::decode(k, a, b), s);
    }
    assert_eq!(G1YoungSizer::decode(200, 5, 5), G1YoungSizer::Defaults);
}

#[test]
fn gci_i43gy_collector_takes_the_sizer_and_the_kill_switch_keeps_the_defaults() {
    {
        let gc = collector();
        assert_eq!(gc.young_region_bounds(), (1, 19));
        assert_eq!(gc.young_target_regions(), 19);
        // A default request changes nothing.
        assert!(gc.set_young_gen_sizing(&YoungGenSizing::default()).is_empty());
        assert_eq!(gc.young_sizer(), G1YoungSizer::Defaults);
        assert_eq!(gc.young_region_bounds(), (1, 19));
        assert!(!gc.young_ceiling_binding());
        // -Xmn8m: a fixed eight-region young generation.
        let w = gc.set_young_gen_sizing(&req(Some(8 * MIB), None, None, None));
        assert!(w.is_empty());
        assert_eq!(gc.young_region_bounds(), (8, 8));
        assert_eq!(gc.young_target_regions(), 8, "the target restarts at the new ceiling");
        assert!(gc.young_ceiling_binding());
        assert!(!gc.young_sizing_adaptive());
        assert_eq!(gc.young_length_bounds_bytes(), (8 * MIB, 8 * MIB));
        assert_eq!(
            gc.tenuring_desired_bytes.load(Ordering::Relaxed),
            gc.survivor_target_bytes() as u64,
            "GC(0)'s desired survivor size follows the new target"
        );
    }
    {
        let gc = collector();
        let w = gc.set_young_gen_sizing(&req(None, Some(4 * MIB), None, Some(1)));
        assert_eq!(w, vec!["-XX:NewSize and -XX:MaxNewSize override -XX:NewRatio".to_string()]);
        assert_eq!(gc.young_region_bounds(), (4, 19));
        assert!(!gc.young_ceiling_binding(), "NewSize alone keeps the default ceiling");        }    }

/// g1ep6: the young target the survivor policy divides is HotSpot's
/// (`calculate_young_target_length`): the desired length, but at most the
/// young regions already allocated plus the Free regions outside the
/// reserve; never below what is allocated. On `G1epKeepRounds 10 -Xmx1g`
/// the desired 411-614 regions capped every pause at 47-80 Survivor
/// regions that the next pause copied again; HotSpot's 43 -> 7 per round
/// follow the Free pool down.
#[test]
fn g1ep6_the_young_target_is_bounded_by_free_regions_outside_the_reserve() {
    // 1024 regions: the reserve is 102.
    assert_eq!(g1_young_target_within_free(400, 60, 900, 1024), 400, "room: the desired");
    assert_eq!(g1_young_target_within_free(400, 165, 220, 1024), 165 + 118, "free bounds it");
    assert_eq!(g1_young_target_within_free(400, 77, 96, 1024), 77, "inside the reserve");
    assert_eq!(g1_young_target_within_free(400, 450, 500, 1024), 450, "already past it");
    assert_eq!(g1_young_target_within_free(0, 0, 0, 1024), 0);
}

/// Lane lf's hunk H-gy1: `-XX:SurvivorRatio` sizes the maximum survivor
/// regions, `ceil(young target / SurvivorRatio)`, and through them the
/// desired survivor size; `None` or the kill switch keeps 8.
#[test]
fn gci_i43gy_survivor_ratio_sizes_g1s_max_survivor_regions() {
    assert_eq!(g1_max_survivor_regions_at(32, 4), 8);
    assert_eq!(g1_max_survivor_regions_at(33, 4), 9);
    assert_eq!(g1_max_survivor_regions_at(32, 0), 32, "0 reads as 1");
    assert_eq!(g1_max_survivor_regions_at(64, 8), g1_max_survivor_regions(64));
    assert_eq!(
        g1_desired_survivor_bytes_with(19, MIB, 50, 8),
        g1_desired_survivor_bytes_at(19, MIB, 50)
    );
    {
        let gc = collector();
        assert_eq!(gc.survivor_ratio(), 8);
        gc.set_survivor_ratio(None);
        assert_eq!(gc.survivor_ratio(), 8);
        gc.set_survivor_ratio(Some(4));
        assert_eq!(gc.survivor_ratio(), 4);
        // target 19: ceil(19 / 4) = 5 regions, half of them desired.
        assert_eq!(gc.survivor_target_bytes(), 5 * MIB / 2);
        assert_eq!(
            gc.tenuring_desired_bytes.load(Ordering::Relaxed),
            (5 * MIB / 2) as u64
        );
    }
}

#[test]
fn gci_i43gy_binding_arm_fires_at_the_ceiling_after_one_eden_region() {
    assert!(binding_young_arm_fires(8, 8, 0));
    assert!(!binding_young_arm_fires(7, 8, 0));
    assert!(!binding_young_arm_fires(8, 8, 8), "survivors at the target: one Eden region first");
    assert!(binding_young_arm_fires(9, 8, 8));
    assert!(!binding_young_arm_fires(5, 3, usize::MAX), "the floor saturates, never wraps");
    // The default ceiling stays inert (the i14yt / i20ys rule).
    assert!(!young_target_arm_fires(8, 8, 8, 0));
}

// The wiring: with `-Xmn8m` eight Eden regions request a pause while the
// Free pool is comfortable (24 of 32 Free); without the sizer the same
// heap sits under the default ceiling (19) and does not.
