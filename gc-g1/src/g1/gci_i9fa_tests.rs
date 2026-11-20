use super::*;

/// The totals skip an uninstrumented record, count a drain pass without
/// summing it, and recompute `other` from the pause total.
#[test]
fn gci_i9fa_phase_totals_sum_and_skip_drain() {
    let t = G1PhaseTotals::default();
    t.note(&G1PausePhases::default(), 999);
    let p = G1PausePhases {
        roots_us: 10,
        rset_us: 200,
        closure_us: 300,
        fixup_us: 400,
        free_us: 20,
        verify_us: 30,
        fixup_regions: 11,
        fixup_bytes: 1 << 20,
        jit_pinned_out: 9,
        cset_regions: 20,
        ..G1PausePhases::default()
    };
    t.note(&p, 1_000);
    t.note(&p, 1_000);
    t.note(
        &G1PausePhases {
            drain_pass: true,
            ..p
        },
        5_000,
    );
    let line = t.line();
    assert!(
        line.starts_with("[GC] g1 pause-phases: pauses=2 drain_passes=1 pause_us=2000 "),
        "{line}"
    );
    assert!(line.contains(" fixup_us=800 "), "{line}");
    assert!(line.contains(" other_us=80 "), "{line}");
    assert!(line.contains(" fixup_regions=22 "), "{line}");
    assert!(line.contains(" jit_pinned_out=18 "), "{line}");
    assert!(line.contains(" cset_regions=40 "), "{line}");
}

/// The Phase-4 skip arms only with its switch on the card-screen premise,
/// and takes only an unchanged, non-empty, non-JNI-pinned ordinary region.
#[test]
fn gci_i9fa_fixup_skip_gates() {
    assert!(fixup_skip_clean_pinned_armed(true, true, true, false));
    assert!(!fixup_skip_clean_pinned_armed(false, true, true, false), "switch off");
    assert!(!fixup_skip_clean_pinned_armed(true, false, true, false), "card rset off");
    assert!(!fixup_skip_clean_pinned_armed(true, true, false, false), "screen off");
    assert!(!fixup_skip_clean_pinned_armed(true, true, true, true), "card clean on");

    use RegionType as T;
    assert!(fixup_skip_region_eligible(T::Eden, T::Eden, 4096, 4096, false));
    assert!(fixup_skip_region_eligible(T::Old, T::Old, 4096, 4096, false));
    assert!(fixup_skip_region_eligible(T::Survivor, T::Survivor, 64, 64, false));
    assert!(!fixup_skip_region_eligible(T::Eden, T::Eden, 4096, 4096, true), "JNI pin");
    assert!(!fixup_skip_region_eligible(T::Eden, T::Eden, 0, 0, false), "empty");
    assert!(!fixup_skip_region_eligible(T::Old, T::Old, 8192, 4096, false), "written");
    assert!(!fixup_skip_region_eligible(T::Old, T::Eden, 4096, 4096, false), "retyped");
    assert!(
        !fixup_skip_region_eligible(T::HumongousStart, T::HumongousStart, 4096, 4096, false),
        "humongous"
    );
    assert!(!fixup_skip_region_eligible(T::Free, T::Free, 4096, 4096, false), "free");
}

/// Each pinned region lands in exactly one `*_only` bucket or none, the
/// young split follows the region type, and an unknown index is skipped.
#[test]
fn gci_i9fa_pin_attribution_buckets() {
    let pinned: RegionSet = [1usize, 2, 3, 4, 5, 9].into_iter().collect();
    let own: RegionSet = [1usize, 4].into_iter().collect();
    let others: RegionSet = [2usize, 4, 5].into_iter().collect();
    let tlab: RegionSet = [5usize].into_iter().collect();
    let young_of = |i: usize| match i {
        9 => None,
        1 => Some((false, 100)),
        _ => Some((true, 1_000)),
    };
    let row = attribute_pin_regions(&pinned, young_of, &own, &others, &tlab);
    assert_eq!(row.regions, 5);
    assert_eq!(row.young, 4);
    assert_eq!(row.bytes, 4_100);
    assert_eq!((row.own, row.others, row.tlab), (2, 3, 1));
    assert_eq!(row.own_only, 1, "region 1");
    assert_eq!((row.others_only, row.others_only_young), (1, 1), "region 2");
    assert_eq!(row.nonobj_only, 1, "region 3");
    let sum = G1PinAttribution::default();
    assert!(sum.line().is_none(), "no line before an attributed pause");
    sum.add(&row);
    sum.add(&row);
    let line = sum.line().expect("a line after one");
    assert!(
        line.starts_with("[GC] g1 pin-attribution: pauses=2 pinned_regions=10 pinned_young=8 "),
        "{line}"
    );
    assert!(
        line.contains(" others_only=2 others_only_young=2 nonobj_only=2 "),
        "{line}"
    );
}

/// gci i24/jp: `by_xt` counts the pinned regions this cycle's cross-thread
/// pins claim (a claim outside the pin set counts nothing) and sums into
/// the line after `xt_words`.
#[test]
fn gci_i24jp_pin_attribution_counts_by_xt() {
    let pinned: RegionSet = [1usize, 2, 3].into_iter().collect();
    let xt: RegionSet = [2usize, 3, 7].into_iter().collect();
    assert_eq!(pinned_regions_claimed_by(&pinned, &xt), 2);
    assert_eq!(pinned_regions_claimed_by(&pinned, &RegionSet::new()), 0);
    let sum = G1PinAttribution::default();
    let row = G1PinAttributionRow { regions: 3, xt: 2, xt_words: 5, ..Default::default() };
    sum.add(&row);
    sum.add(&row);
    let line = sum.line().expect("a line after one");
    assert!(line.contains(" xt_words=10 by_xt=4 ("), "{line}");
}
