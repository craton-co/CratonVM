use super::*;

fn table(entries: &[(usize, u64)]) -> [u64; AGE_TABLE_LEN] {
    let mut t = [0u64; AGE_TABLE_LEN];
    for &(age, bytes) in entries {
        t[age] = bytes;
    }
    t
}

/// HotSpot's rule, case by case: the first age whose CUMULATIVE bytes
/// exceed the target; the ceiling when nothing does; never below 1.
#[test]
fn the_threshold_is_hotspots_cumulative_rule() {
    // Nothing survives: the ceiling.
    assert_eq!(compute_tenuring_threshold(&table(&[]), 1000, 15), 15);
    // Everything fits: the ceiling.
    assert_eq!(
        compute_tenuring_threshold(&table(&[(1, 100), (2, 100), (3, 100)]), 1000, 15),
        15
    );
    // Age 1 alone overflows: 1, not 0.
    assert_eq!(
        compute_tenuring_threshold(&table(&[(1, 5000)]), 1000, 15),
        1
    );
    // Cumulative crosses at age 3 (400 + 400 + 400 > 1000).
    assert_eq!(
        compute_tenuring_threshold(&table(&[(1, 400), (2, 400), (3, 400)]), 1000, 15),
        3
    );
    // Exactly AT the target does not cross (HotSpot's `>`).
    assert_eq!(
        compute_tenuring_threshold(&table(&[(1, 500), (2, 500)]), 1000, 15),
        15
    );
    // The ceiling caps a late crossing.
    assert_eq!(
        compute_tenuring_threshold(&table(&[(1, 400), (2, 400), (3, 400)]), 1000, 2),
        2
    );
    // `MaxTenuringThreshold=0` is "always tenure".
    assert_eq!(compute_tenuring_threshold(&table(&[(1, 1)]), 1000, 0), 0);
    // Bucket 0 is ignored (never written by a collection).
    assert_eq!(
        compute_tenuring_threshold(&table(&[(0, u64::MAX)]), 0, 15),
        15
    );
}

/// A saturated table cannot overflow the running sum or the cast.
#[test]
fn a_saturated_age_table_is_total() {
    let t = [u64::MAX; AGE_TABLE_LEN];
    assert_eq!(compute_tenuring_threshold(&t, u64::MAX - 1, 15), 1);
    assert_eq!(compute_tenuring_threshold(&t, u64::MAX, 15), 15);
}

/// The target is one eighth of the trigger, times the ratio.
#[test]
fn the_desired_survivor_size_follows_the_young_trigger() {
    let mib = 1024 * 1024;
    assert_eq!(desired_survivor_bytes(32 * mib, 50), 2 * mib as u64);
    assert_eq!(desired_survivor_bytes(32 * mib, 100), 4 * mib as u64);
    assert_eq!(desired_survivor_bytes(32 * mib, 0), 0);
    // Halving the trigger (the pause-goal loop) halves the target.
    assert_eq!(desired_survivor_bytes(16 * mib, 50), mib as u64);
}

/// gcf f2/y: the target is HotSpot's, in WORDS: `floor(floor(eden / 8 /
/// 8) * ratio / 100)` words, reported in bytes. A byte-granular target
/// differs on an eden that is not a multiple of 64 bytes, and a table
/// between the two values gets a different threshold.
#[test]
fn f2y_the_desired_survivor_size_is_rounded_to_hotspot_words() {
    // eden 1000 B: survivor 125 B = 15 words; 15 * 50 / 100 = 7 words.
    assert_eq!(desired_survivor_bytes(1000, 50), 56);
    // (The byte-granular formula gave 125 * 50 / 100 = 62.)
    // A 60-byte survivor table is above HotSpot's 7-word target: T = 1.
    let desired = desired_survivor_bytes(1000, 50);
    assert_eq!(
        compute_tenuring_threshold(&table(&[(1, 60)]), desired, 15),
        1
    );
    // 7 words exactly is not above it.
    assert_eq!(
        compute_tenuring_threshold(&table(&[(1, 56)]), desired, 15),
        15
    );
    // Word-multiple edens are unchanged by the rounding.
    let mib = 1024 * 1024;
    assert_eq!(desired_survivor_bytes(64 * mib, 50), 4 * mib as u64);
    assert_eq!(desired_survivor_bytes(0, 50), 0);
}


/// gcf f2/y: `add_to_age_table` is `AgeTable::add` at the new age,
/// clamped to the last bucket.
#[test]
fn f2y_add_to_age_table_counts_at_the_new_age_and_clamps() {
    let mut t = [0u64; AGE_TABLE_LEN];
    add_to_age_table(&mut t, 1, 24);
    add_to_age_table(&mut t, 1, 40);
    add_to_age_table(&mut t, 15, 8);
    add_to_age_table(&mut t, 200, 8);
    assert_eq!(t[1], 64);
    assert_eq!(t[15], 16);
    assert_eq!(t.iter().sum::<u64>(), 80);
}

/// Default state: not engaged, historical promotion age; the env lever or
/// any `-XX:` tenuring value engages it; the unit conversion is `T + 1`.
#[test]
fn engagement_and_the_unit_conversion() {
    let s = TenuringState::new();
    assert!(!s.engaged(false));
    assert_eq!(s.promotion_age(false, 3), 3, "default: the fixed age");
    assert!(s.engaged(true));
    assert_eq!(
        s.promotion_age(true, 3),
        DEFAULT_MAX_TENURING_THRESHOLD + 1,
        "env lever alone: HotSpot's initial threshold (= Max)"
    );

    s.configure(&TenuringConfig {
        max_tenuring_threshold: Some(6),
        ..TenuringConfig::default()
    });
    assert!(
        s.engaged(false),
        "a typed -XX value engages adaptive tenuring"
    );
    assert_eq!(s.threshold(), 6, "initial defaults to the max");
    assert_eq!(s.promotion_age(false, 3), 7);

    s.configure(&TenuringConfig {
        max_tenuring_threshold: Some(4),
        initial_tenuring_threshold: Some(9),
        ..TenuringConfig::default()
    });
    assert_eq!(s.threshold(), 4, "initial is clamped to the max");

    s.configure(&TenuringConfig {
        print_tenuring_distribution: true,
        ..TenuringConfig::default()
    });
    assert!(!s.engaged(false), "the print flag alone does not engage");
    assert!(s.print_requested());
}

/// gcg g1/y: a heap pinned to the fixed age reads the variable as `=0`;
/// a typed `-XX:` value still engages adaptive tenuring on it.
#[test]
fn g1y_a_pinned_heap_ignores_the_variable_but_not_a_typed_flag() {
    let s = TenuringState::new();
    assert!(
        s.engaged(true),
        "positive control: the variable engages an unpinned heap"
    );
    s.pin_fixed();
    assert!(!s.engaged(true), "pinned: the variable reads as =0");
    assert_eq!(s.promotion_age(true, 3), 3, "pinned: the fixed age");
    s.configure(&TenuringConfig {
        max_tenuring_threshold: Some(6),
        ..TenuringConfig::default()
    });
    assert!(s.engaged(true), "a typed -XX value engages a pinned heap");
    assert_eq!(s.promotion_age(true, 3), 7);
}

/// `adapt` stores the new threshold, reports it, and counts the move.
#[test]
fn adapt_moves_the_threshold_both_ways_and_counts_it() {
    let s = TenuringState::new();
    s.configure(&TenuringConfig {
        max_tenuring_threshold: Some(15),
        ..TenuringConfig::default()
    });
    let mib = 1024 * 1024;
    // 32 MiB trigger -> 2 MiB target. 3 MiB at age 1 -> threshold 1.
    let d = s.adapt(&table(&[(1, 3 * mib)]), 32 * mib as usize);
    assert_eq!((d.previous, d.next, d.max), (15, 1, 15));
    assert_eq!(d.desired_bytes, 2 * mib);
    assert_eq!(s.promotion_age(false, 3), 2);
    // Survivors fall to a trickle -> back to the ceiling.
    let d = s.adapt(&table(&[(1, 1024), (2, 1024)]), 32 * mib as usize);
    assert_eq!((d.previous, d.next), (1, 15));
    // Unchanged.
    let _ = s.adapt(&table(&[]), 32 * mib as usize);
    assert_eq!(s.census(), [3, 1, 1, 1]);
}

/// gci i45/sc: `-XX:SurvivorRatio` divides the survivor capacity the
/// target is taken from (HotSpot Serial: `eden / SurvivorRatio`); the
/// default ratio is the old constant, byte for byte, and a ratio of 0
/// (refused by the launcher) reads as 1 rather than dividing by zero.
#[test]
fn gci_i45_sc_the_survivor_target_reads_the_heaps_survivor_ratio() {
    let mib: usize = 1024 * 1024;
    for eden in [0, 1000, 32 * mib, 71_630_848, 59_703_296] {
        for tsr in [0u8, 50, 90, 100] {
            assert_eq!(
                desired_survivor_bytes_at(eden, tsr, SURVIVOR_RATIO),
                desired_survivor_bytes(eden, tsr)
            );
            assert_eq!(
                desired_survivor_bytes_space_aligned_at(eden, tsr, SURVIVOR_RATIO),
                desired_survivor_bytes_space_aligned(eden, tsr)
            );
        }
    }
    // SurvivorRatio=4: a 32 MiB eden has an 8 MiB survivor, target 4 MiB.
    assert_eq!(desired_survivor_bytes_at(32 * mib, 50, 4), 4 * mib as u64);
    // HotSpot Serial at -Xmx256m -XX:SurvivorRatio=4: young
    // align_down(256m/3, 64K) = 89456640, survivor align_down(young/6, 64K)
    // = 14876672, eden 59703296; the aligned target is half the survivor.
    assert_eq!(
        desired_survivor_bytes_space_aligned_at(59_703_296, 50, 4),
        14_876_672 / 2
    );
    assert_eq!(desired_survivor_bytes_at(1000, 50, 0), desired_survivor_bytes_at(1000, 50, 1));

    let s = TenuringState::new();
    assert_eq!(s.survivor_ratio(), SURVIVOR_RATIO);
    s.configure(&TenuringConfig {
        max_tenuring_threshold: Some(15),
        ..TenuringConfig::default()
    });
    let d8 = s.adapt(&table(&[]), 32 * mib);
    assert_eq!(d8.desired_bytes, 2 * mib as u64);
    s.set_survivor_ratio(4);
    let d4 = s.adapt(&table(&[]), 32 * mib);
    assert_eq!(d4.desired_bytes, 4 * mib as u64);
    assert_eq!(s.fixed_decision(32 * mib, 1).desired_bytes, 4 * mib as u64);
    s.set_survivor_ratio(0);
    assert_eq!(s.survivor_ratio(), 1, "never a zero divisor");
}

/// The distribution print carries HotSpot's wording, skips empty ages and
/// keeps a running total.
#[test]
fn the_distribution_lines_read_like_hotspots() {
    let d = TenuringDecision {
        desired_bytes: 2048,
        previous: 15,
        next: 2,
        max: 15,
        adaptive: true,
    };
    let lines = distribution_lines(&d, &table(&[(1, 1500), (3, 700)]));
    assert_eq!(lines.len(), 3);
    assert!(lines[0].starts_with(
        "[GC] tenuring: Desired survivor size 2048 bytes, new threshold 2 (max threshold 15)"
    ));
    assert!(lines[0].contains("mode=adaptive"));
    assert_eq!(
        lines[1],
        "[GC] tenuring: - age   1:       1500 bytes,       1500 total"
    );
    assert_eq!(
        lines[2],
        "[GC] tenuring: - age   3:        700 bytes,       2200 total"
    );
}

/// HotSpot's ranges for the value flags.
#[test]
fn the_value_flags_take_hotspots_ranges() {
    assert_eq!(
        TenuringConfig::parse_value("-XX:MaxTenuringThreshold", "15", 15),
        Ok(15)
    );
    assert_eq!(
        TenuringConfig::parse_value("-XX:MaxTenuringThreshold", " 0 ", 15),
        Ok(0)
    );
    assert!(TenuringConfig::parse_value("-XX:MaxTenuringThreshold", "16", 15).is_err());
    assert!(TenuringConfig::parse_value("-XX:TargetSurvivorRatio", "abc", 100).is_err());
    assert_eq!(
        TenuringConfig::parse_value("-XX:TargetSurvivorRatio", "100", 100),
        Ok(100)
    );
    let c = TenuringConfig {
        target_survivor_ratio: Some(90),
        print_tenuring_distribution: true,
        ..TenuringConfig::default()
    };
    assert!(c.requests_adaptive());
    assert!(!c.is_default());
    assert_eq!(
        c.given_flag_names(),
        vec!["-XX:TargetSurvivorRatio", "-XX:+PrintTenuringDistribution"]
    );
    assert!(TenuringConfig::default().is_default());
}

/// gcd d2/g: a young value is exempt from the pin set only when every
/// occurrence in the root list is a precise one the scan counted, it is
/// not vetoed, and it is young.
#[test]
fn gcd_d2g_only_values_named_by_precise_tables_alone_are_exempt() {
    let at = |a: usize| {
        // SAFETY: never dereferenced; only the address is compared.
        unsafe { cratonvm_types::ObjectRef::from_raw(a as *mut u8) }
    };
    let (only, twice, shared, vetoed, old) = (
        0x10_0000usize,
        0x10_0040usize,
        0x10_0080usize,
        0x10_00c0usize,
        0x90_0000usize,
    );
    // `twice`: two static slots name it (both counted); `shared`: one
    // static and one other root (a conservative word, say).
    let roots = vec![
        at(only),
        at(twice),
        at(twice),
        at(shared),
        at(shared),
        at(vetoed),
        at(old),
    ];
    let precise: rustc_hash::FxHashMap<usize, u32> =
        [(only, 1), (twice, 2), (shared, 1), (vetoed, 1), (old, 1)]
            .into_iter()
            .collect();
    let exempt = precise_only_root_values(&roots, &precise, |a| a < 0x80_0000, |a| a == vetoed);
    let mut got: Vec<usize> = exempt.into_iter().collect();
    got.sort_unstable();
    assert_eq!(got, vec![only, twice]);

    // A count the list no longer matches (fewer occurrences) exempts
    // nothing for that address.
    let short = vec![at(twice)];
    assert!(
        precise_only_root_values(&short, &precise, |_| true, |_| false)
            .iter()
            .all(|&a| a != twice)
    );
}

/// gcd d2/g: the published table is per thread and taken once.
#[test]
fn gcd_d2g_precise_root_values_are_taken_once() {
    clear_precise_root_values();
    assert!(take_precise_root_values().is_none());
    publish_precise_root_values([(0x10_0000usize, 1u32)].into_iter().collect());
    let taken = take_precise_root_values().expect("published");
    assert_eq!(taken.get(&0x10_0000), Some(&1));
    assert!(take_precise_root_values().is_none(), "take-once");
    publish_precise_root_values([(0x10_0000usize, 1u32)].into_iter().collect());
    clear_precise_root_values();
    assert!(take_precise_root_values().is_none(), "the scan's clear");
}

/// gci i13/cm item 6: the "no young sweep follows" scope licenses skipping
/// the precise-root table only inside the scope AND under the opt-in; it
/// nests, and is restored on exit and on an unwind.
#[test]
fn gci_i13cm_no_young_sweep_follows_only_inside_the_scope_and_under_the_switch() {
    let on = [(CONC_PAUSE_NO_PRECISE_ROOT_TABLE_FLAG, Some("1"))];
    let off = [(CONC_PAUSE_NO_PRECISE_ROOT_TABLE_FLAG, None)];
    cratonvm_types::flags::with_thread_overrides(&on, || {
        assert!(!no_young_sweep_follows(), "outside the scope");
        let inside = with_no_young_sweep_following(|| {
            let nested = with_no_young_sweep_following(no_young_sweep_follows);
            (no_young_sweep_follows(), nested)
        });
        assert_eq!(inside, (true, true));
        assert!(!no_young_sweep_follows(), "restored on exit");
        let unwound = std::panic::catch_unwind(|| {
            with_no_young_sweep_following(|| -> bool { panic!("unwind out of the scope") })
        });
        assert!(unwound.is_err());
        assert!(!no_young_sweep_follows(), "restored on an unwind");
    });
    cratonvm_types::flags::with_thread_overrides(&off, || {
        assert!(
            !with_no_young_sweep_following(no_young_sweep_follows),
            "off (the default): the scan counts as before"
        );
    });
}

/// gcd d5/r: the sealed take hands the table only to a sweep whose root
/// list is at least as long as the scan that published it returned, and
/// only once that scan finished (sealed). Every refusal still consumes
/// the table.
#[test]
fn gcd_d5r_the_sealed_take_refuses_a_table_that_does_not_describe_the_list() {
    let counts = || -> rustc_hash::FxHashMap<usize, u32> {
        [(0x10_0000usize, 1u32)].into_iter().collect()
    };
    clear_precise_root_values();
    // Nothing published: nothing taken, nothing sealed.
    seal_precise_root_values(4);
    assert!(take_sealed_precise_root_values(4).is_none());
    // Sealed at 4: a list of 4 or more takes it (the gatherer appends).
    publish_precise_root_values(counts());
    seal_precise_root_values(4);
    assert!(take_sealed_precise_root_values(6).is_some());
    assert!(take_sealed_precise_root_values(6).is_none(), "take-once");
    // Unsealed (the scan never finished): refused, and consumed.
    publish_precise_root_values(counts());
    assert!(take_sealed_precise_root_values(6).is_none());
    assert!(
        take_precise_root_values().is_none(),
        "the refusal consumed it"
    );
    // Sealed longer than the list: refused, and consumed.
    publish_precise_root_values(counts());
    seal_precise_root_values(8);
    assert!(take_sealed_precise_root_values(3).is_none());
    assert!(
        take_precise_root_values().is_none(),
        "the refusal consumed it"
    );
    // A later publish starts unsealed again.
    publish_precise_root_values(counts());
    seal_precise_root_values(2);
    publish_precise_root_values(counts());
    assert!(
        take_sealed_precise_root_values(9).is_none(),
        "the re-publish unsealed it"
    );
    clear_precise_root_values();
}

/// gci i6/pn (conc10): a value a published conservative JIT pin names is
/// never exempt, even when the pin never entered the root list (the count
/// rule alone would exempt it), and the veto is counted. A value no pin
/// names is exempt as before; the band veto still applies.
#[test]
fn gci_i6pn_a_conservatively_pinned_value_is_never_precise_exempt() {
    let at = |a: usize| {
        // SAFETY: never dereferenced; only the address is compared.
        unsafe { cratonvm_types::ObjectRef::from_raw(a as *mut u8) }
    };
    let (only, spilled, banded) = (0x10_0000usize, 0x10_0040usize, 0x10_0080usize);
    // Every occurrence in the list is a counted precise one, so the count
    // rule alone would exempt all three.
    let roots = vec![at(only), at(spilled), at(banded)];
    let precise: rustc_hash::FxHashMap<usize, u32> =
        [(only, 1), (spilled, 1), (banded, 1)].into_iter().collect();
    let alone = precise_only_root_values(&roots, &precise, |_| true, |_| false);
    assert_eq!(alone.len(), 3, "the count rule alone exempts all three");
    // `spilled` is named by a published spill-region pin that is not in
    // the list; `banded` by an unrewritable band word.
    let conservative: rustc_hash::FxHashSet<usize> = [spilled].into_iter().collect();
    let before = PRECISE_ROOT_CENSUS[4].load(Ordering::Relaxed);
    let exempt = precise_only_root_values_vetoing_conservative(
        &roots,
        &precise,
        |_| true,
        |a| a == banded,
        &conservative,
    );
    let mut got: Vec<usize> = exempt.into_iter().collect();
    got.sort_unstable();
    assert_eq!(got, vec![only]);
    assert!(
        PRECISE_ROOT_CENSUS[4].load(Ordering::Relaxed) > before,
        "the pin veto is counted (prp_conservative_vetoed)"
    );
    // No pin, no band word: identical to the count rule.
    let none = rustc_hash::FxHashSet::default();
    let same =
        precise_only_root_values_vetoing_conservative(&roots, &precise, |_| true, |_| false, &none);
    assert_eq!(same.len(), 3);
}

/// gci i6/pn (conc10): the sweep's exemption record is per thread, taken
/// once, and labels an address by the exemption that moved it.
#[test]
fn gci_i6pn_young_root_exemptions_are_taken_once_and_label_their_cause() {
    let _ = take_young_root_exemptions();
    assert!(take_young_root_exemptions().is_none());
    assert_eq!(young_root_exemption_label(None, 0x10_0000), "unrecorded");
    record_young_root_exemptions(YoungRootExemptions {
        prp: [0x10_0000usize].into_iter().collect(),
        movable: [0x10_0040usize].into_iter().collect(),
    });
    let taken = take_young_root_exemptions().expect("recorded");
    assert!(take_young_root_exemptions().is_none(), "take-once");
    assert_eq!(young_root_exemption_label(Some(&taken), 0x10_0000), "prp");
    assert_eq!(young_root_exemption_label(Some(&taken), 0x10_0040), "movable");
    assert_eq!(
        young_root_exemption_label(Some(&taken), 0x10_0080),
        "pinned-or-not-a-root"
    );
}
