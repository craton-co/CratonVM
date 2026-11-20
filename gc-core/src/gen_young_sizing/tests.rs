use super::*;

const M: usize = 1 << 20;
const G: usize = 1 << 30;

const K: usize = 1 << 10;

fn serial(max: usize, initial: usize, req: YoungGenSizing) -> SerialYoungErgonomics {
    hotspot_serial_young_ergonomics(&SerialYoungFlags::new(max, initial, 6 * M, &req, None))
}

/// gc-known-issues retire: HotSpot 25's Serial `[gc,ergo]` texts at
/// `-Xmx256m` (Ubuntu 25.0.4.1 and Microsoft 25.0.3, 2026-10-25).
#[test]
fn serial_young_ergonomics_answer_as_hotspot() {
    let size = |n: Option<usize>, m: Option<usize>| YoungGenSizing {
        new_size: n,
        max_new_size: m,
        ..Default::default()
    };
    let t = 1usize << 40;
    let new_size_line = |n: usize, i: usize, r: usize| {
        format!(
            "NewSize ({n}k) is equal to or greater than initial heap size ({i}k).  A new \
             NewSize of {r}k will be used to accomodate an old generation."
        )
    };
    let max_line = |n: usize| {
        format!(
            "MaxNewSize ({n}k) is equal to or greater than the entire heap (262144k).  A \
             new max generation size of 262080k will be used."
        )
    };
    let inconsistency = "Inconsistency between generation sizes and heap size, resizing \
                         the generations to fit the heap.";
    assert_eq!(
        serial(256 * M, 256 * M, size(Some(t), None)).warnings,
        vec![new_size_line(1073741824, 262144, 262080)]
    );
    assert_eq!(
        serial(256 * M, 256 * M, size(None, Some(t))).warnings,
        vec![max_line(1073741824)]
    );
    let xmn = YoungGenSizing {
        xmn: Some(t),
        ..Default::default()
    };
    assert_eq!(
        serial(256 * M, 256 * M, xmn).warnings,
        vec![
            new_size_line(1073741824, 262144, 262080),
            max_line(1073741824)
        ]
    );
    assert_eq!(
        serial(256 * M, 16 * M, size(Some(32 * M), None)).warnings,
        vec![new_size_line(32768, 16384, 16320)]
    );
    assert_eq!(
        serial(256 * M, 16 * M, size(None, Some(300 * M))).warnings,
        vec![max_line(307200), inconsistency.to_string()]
    );
    assert!(serial(256 * M, 256 * M, size(Some(64 * M), Some(32 * M)))
        .warnings
        .is_empty());
    // `NewSize=0`: the smallest young generation, eden one alignment.
    let zero = serial(256 * M, 256 * M, size(Some(0), None));
    assert!(zero.warnings.is_empty());
    assert_eq!((zero.initial_young, zero.initial_eden), (192 * K, 64 * K));
    // Microsoft 25.0.3 on a 16 GiB host (initial 254M): HotSpot resizes the
    // young generation to 87359488 bytes.
    let windows = serial(256 * M, 266_338_304, size(Some(0), None));
    assert_eq!(windows.warnings, vec![inconsistency.to_string()]);
    assert_eq!(windows.initial_young, 87_359_488);
    // Defaults: no line.
    assert!(serial(256 * M, 256 * M, YoungGenSizing::default())
        .warnings
        .is_empty());
    assert_eq!(
        hotspot_gc_before_init_lines(192 * K)[1],
        "GC triggered before VM initialization completed. Try increasing NewSize, \
         current value 192K."
    );
    assert_eq!(HOTSPOT_DEFAULT_NEW_SIZE / K, 1331);
}

#[test]
fn an_explicit_zero_size_plans_nothing() {
    let zero = YoungGenSizing {
        new_size: Some(0),
        max_new_size: Some(0),
        ..Default::default()
    };
    assert_eq!(plan_young_gen(G, &zero), None);    }

#[test]
fn no_flag_means_no_plan() {
    assert!(YoungGenSizing::default().is_default());
    assert_eq!(plan_young_gen(G, &YoungGenSizing::default()), None);
}

#[test]
fn xmn_is_the_young_pair() {
    let req = YoungGenSizing {
        xmn: Some(256 * M),
        ..Default::default()
    };
    let p = plan_young_gen(G, &req).expect("a flag was given");
    assert_eq!(p.young_semi, 128 * M);
    assert_eq!(p.young_pair(), 256 * M);
    assert_eq!(p.old_size, G - 256 * M);
    assert_eq!(p.source, "-Xmn");
    assert!(p.adjustments.is_empty(), "{:?}", p.adjustments);
}

#[test]
fn new_ratio_divides_the_heap() {
    // NewRatio=2: young = 1/3 of the heap.
    let req = YoungGenSizing {
        new_ratio: Some(2),
        ..Default::default()
    };
    let p = plan_young_gen(3 * G, &req).expect("plan");
    assert_eq!(p.young_pair(), G);
    assert_eq!(p.old_size, 2 * G);
    // NewRatio=1 is this collector's default split.
    let one = YoungGenSizing {
        new_ratio: Some(1),
        ..Default::default()
    };
    let p1 = plan_young_gen(G, &one).expect("plan");
    assert_eq!(p1.young_semi, default_young_semi(G));
}

#[test]
fn xmn_wins_over_the_other_three_and_says_so() {
    let req = YoungGenSizing {
        xmn: Some(64 * M),
        new_size: Some(512 * M),
        max_new_size: Some(512 * M),
        new_ratio: Some(1),
    };
    let p = plan_young_gen(G, &req).expect("plan");
    assert_eq!(p.young_pair(), 64 * M);
    assert_eq!(p.adjustments.len(), 2, "{:?}", p.adjustments);
}

#[test]
fn new_size_is_a_floor_and_max_new_size_a_ceiling() {
    // Default base is 512 MiB of a 1 GiB heap.
    let cap = YoungGenSizing {
        max_new_size: Some(128 * M),
        ..Default::default()
    };
    assert_eq!(plan_young_gen(G, &cap).expect("plan").young_pair(), 128 * M);
    let above = YoungGenSizing {
        max_new_size: Some(900 * M),
        ..Default::default()
    };
    assert_eq!(
        plan_young_gen(G, &above).expect("plan").young_pair(),
        512 * M
    );
    let floor = YoungGenSizing {
        new_size: Some(768 * M),
        ..Default::default()
    };
    assert_eq!(
        plan_young_gen(G, &floor).expect("plan").young_pair(),
        768 * M
    );
    let below = YoungGenSizing {
        new_size: Some(64 * M),
        ..Default::default()
    };
    assert_eq!(
        plan_young_gen(G, &below).expect("plan").young_pair(),
        512 * M
    );
    // Both, and with a ratio base.
    let both = YoungGenSizing {
        new_size: Some(100 * M),
        max_new_size: Some(200 * M),
        new_ratio: Some(3),
        ..Default::default()
    };
    assert_eq!(
        plan_young_gen(G, &both).expect("plan").young_pair(),
        200 * M
    );
}

#[test]
fn a_new_size_above_max_new_size_raises_the_ceiling() {
    let req = YoungGenSizing {
        new_size: Some(300 * M),
        max_new_size: Some(100 * M),
        ..Default::default()
    };
    let p = plan_young_gen(G, &req).expect("plan");
    assert_eq!(p.young_pair(), 300 * M);
    assert_eq!(p.adjustments.len(), 1, "{:?}", p.adjustments);
}

#[test]
fn the_old_generation_is_never_emptied() {
    for req in [
        YoungGenSizing {
            xmn: Some(G),
            ..Default::default()
        },
        YoungGenSizing {
            xmn: Some(4 * G),
            ..Default::default()
        },
        YoungGenSizing {
            new_ratio: Some(0),
            ..Default::default()
        },
        YoungGenSizing {
            new_size: Some(usize::MAX),
            ..Default::default()
        },
    ] {
        let p = plan_young_gen(G, &req).expect("plan");
        assert!(p.old_size >= MIN_OLD_GEN_BYTES, "{req:?} -> {p:?}");
        assert_eq!(p.old_size + p.young_pair(), G, "{req:?}");
        assert_eq!(p.young_semi % 8, 0);
        assert_eq!(
            p.adjustments.len(),
            1,
            "exactly one clamp note: {:?}",
            p.adjustments
        );
    }
}

#[test]
fn a_tiny_young_request_is_raised_to_the_minimum() {
    let req = YoungGenSizing {
        xmn: Some(8),
        ..Default::default()
    };
    let p = plan_young_gen(G, &req).expect("plan");
    assert_eq!(p.young_semi, YOUNG_SEMI_MIN);
    assert_eq!(p.adjustments.len(), 1);
}

#[test]
fn a_tiny_heap_still_plans_a_coherent_geometry() {
    for total in [0usize, 4096, 64 * 1024, 3 * M] {
        let req = YoungGenSizing {
            xmn: Some(usize::MAX),
            ..Default::default()
        };
        let p = plan_young_gen(total, &req).expect("plan");
        assert!(p.young_semi >= YOUNG_SEMI_MIN);
        assert!(p.old_size > 0);
        assert_eq!(p.old_size + p.young_pair(), total.max(MIN_TOTAL_BYTES));
    }
}

#[test]
fn the_flag_names_follow_what_was_given() {
    let req = YoungGenSizing {
        xmn: Some(1),
        new_ratio: Some(2),
        ..Default::default()
    };
    assert_eq!(req.given_flag_names(), vec!["-Xmn", "-XX:NewRatio"]);
    assert!(YoungGenSizing::default().given_flag_names().is_empty());
}

#[test]
fn the_summary_names_the_source_and_the_split() {
    let req = YoungGenSizing {
        xmn: Some(256 * M),
        ..Default::default()
    };
    let s = plan_young_gen(G, &req).expect("plan").summary();
    assert!(s.contains("256 MiB (-Xmn; 2 x 128 MiB semi-spaces)"), "{s}");
    assert!(s.contains("old generation 768 MiB"), "{s}");
}

/// gcj j3/yg3: the eden + survivor geometry inside the pair. `-Xmn128m`
/// with HotSpot's defaults is DefNew's 12.75 MiB survivors and
/// 102.5 MiB eden; the commit never exceeds the budget; the trigger the
/// pair can then reach (eden) is HotSpot's, not 95 % of a 64 MiB semi.
#[test]
fn gcj_j3yg3_the_serial_pair_geometry_is_defnews_inside_the_budget() {
    const A: usize = 64 * 1024;
    let g = plan_serial_pair_geometry(128 * M, 8, A).expect("fits");
    assert_eq!(g.survivor, 13_369_344, "align_down(128 MiB / 10, 64 KiB)");
    assert_eq!(g.eden, 128 * M - 2 * 13_369_344);
    assert_eq!(g.semi_reserve, g.eden + g.survivor);
    assert_eq!(g.max_young_commit(), 128 * M, "the commit is the budget");
    assert!(g.eden > (64 * M) * 95 / 100, "above today's pair trigger");
    for (y, sr) in [(64 * M, 8), (256 * M, 1), (32 * M, 6), (G, 8), (3 * M + 5, 8)] {
        let g = plan_serial_pair_geometry(y, sr, A).expect("fits");
        assert_eq!(g.eden + 2 * g.survivor, g.young_budget, "{y} {sr}");
        assert!(g.young_budget <= y && g.survivor % A == 0, "{y} {sr}");
        assert_eq!(g.max_young_commit(), g.young_budget);
    }
    assert_eq!(plan_serial_pair_geometry(2 * A, 8, A), None, "no room for eden");
    assert_eq!(plan_serial_pair_geometry(128 * M, 8, 3), None, "not a power of two");
}
