use super::*;

/// The amortized prune walks only once the table has doubled since the
/// last walk, never below the floor, and always once it crosses the
/// ceiling.
#[test]
fn relocations_prune_due_is_amortized_floored_and_capped() {
    assert!(!relocations_prune_due(10, 0), "below the floor: skip");
    assert!(relocations_prune_due(ZGC_RELOCATIONS_PRUNE_MIN, 0));
    assert!(!relocations_prune_due(300_000, 400_000), "not yet doubled");
    assert!(relocations_prune_due(400_000, 400_000));
    assert!(
        relocations_prune_due(ZGC_RELOCATIONS_CEILING + 1, 6_000_000),
        "past the ceiling the walk is always due, so the clear can bind"
    );
    assert!(!relocations_prune_due(ZGC_RELOCATIONS_CEILING, 6_000_000));
}

/// zt t1/rl: the high slide's sorted pin test answers what the linear
/// scan it replaced answered, at every boundary.
#[test]
fn zt1_rl_the_sorted_pin_test_matches_the_linear_scan() {
    let pins = vec![64usize, 128, 136, 512];
    let linear = |from: usize, size: usize, inc: bool| {
        pins.iter()
            .any(|p| *p >= from && (*p < from + size || (inc && *p == from + size)))
    };
    for from in (0..600usize).step_by(8) {
        for size in [8usize, 16, 64, 72] {
            for inc in [false, true] {
                assert_eq!(
                    sorted_pin_in_extent(&pins, from, size, inc),
                    linear(from, size, inc),
                    "from={from} size={size} inc={inc}"
                );
            }
        }
    }
    assert!(!sorted_pin_in_extent(&[], 0, 8, true));
}

/// zt t1/rl: the Windows pause-only uncommitter refuses on Windows under
/// the switch only.
#[test]
fn zt1_rl_the_pause_only_uncommitter_refuses_only_on_windows_under_the_switch() {
    assert!(zgc_uncommitter_refused_on_this_platform(true, true));
    assert!(!zgc_uncommitter_refused_on_this_platform(true, false));
    assert!(!zgc_uncommitter_refused_on_this_platform(false, true));
    assert!(!zgc_uncommitter_refused_on_this_platform(false, false));
}

/// zt t1/rl: the scaled ceiling only lowers the absolute one, has a
/// floor, and the due test binds at whichever ceiling it is given.
#[test]
fn zt1_rl_the_scaled_relocations_ceiling_only_lowers_the_bound() {
    const MIB: usize = 1024 * 1024;
    assert_eq!(
        zgc_relocations_ceiling_for(256 * MIB, false),
        ZGC_RELOCATIONS_CEILING
    );

    assert_eq!(
        zgc_relocations_ceiling_for(8 * 1024 * MIB, true),
        ZGC_RELOCATIONS_CEILING
    );
    assert!(relocations_prune_due_with(MIB + 1, 6_000_000, MIB));
    assert!(!relocations_prune_due_with(MIB, 6_000_000, MIB));
    assert_eq!(
        relocations_prune_due(ZGC_RELOCATIONS_CEILING + 1, 6_000_000),
        relocations_prune_due_with(
            ZGC_RELOCATIONS_CEILING + 1,
            6_000_000,
            ZGC_RELOCATIONS_CEILING
        )
    );
}


/// The chain rule: a hop is followed only into a LATER slide's entry.
#[test]
fn forward_chain_end_follows_only_later_slides() {
    let mut m: FxHashMap<usize, (usize, u32)> = FxHashMap::default();
    // ONE slide: Y moved B(0x200) -> C(0x100), X moved A(0x300) -> B.
    m.insert(0x200, (0x100, 7));
    m.insert(0x300, (0x200, 7));
    assert_eq!(forward_chain_end(&m, m[&0x300], true), 0x200, "X is at B");
    assert_eq!(
        forward_chain_end(&m, m[&0x300], false),
        0x100,
        "the old walk handed back Y"
    );
    // TWO slides: X moved A2 -> B2 in slide 8, then B2 -> C2 in slide 9.
    m.insert(0x900, (0x800, 8));
    m.insert(0x800, (0x700, 9));
    assert_eq!(forward_chain_end(&m, m[&0x900], true), 0x700);
    // Epoch wrap: slide u32::MAX then slide 0 is still "later".
    let mut w: FxHashMap<usize, (usize, u32)> = FxHashMap::default();
    w.insert(0x50, (0x40, u32::MAX));
    w.insert(0x40, (0x30, 0));
    assert_eq!(forward_chain_end(&w, w[&0x50], true), 0x30);
}


/// The phase clock prints nothing when off and names each phase when on.
#[test]
fn slide_phase_clock_is_silent_off_and_names_phases_on() {
    let mut off = ZSlidePhaseClock::new(false);
    off.mark("logical");
    assert!(off.line(1, 0, true).is_none());
    let mut on = ZSlidePhaseClock::new(true);
    on.mark("logical");
    on.mark("slide");
    let line = on.line(10, 3, false).expect("on");
    assert!(line.starts_with("[zgc-relocate-phases] live=10 moved=3 fused=false"));
    assert!(line.contains(" logical_us=") && line.contains(" slide_us="));
    assert!(line.contains(" total_us="));
}

/// An UNARMED fail-stop lets a panic unwind (the pre-move phases).
#[test]
fn an_unarmed_slide_fail_stop_lets_a_panic_unwind() {
    let r = std::panic::catch_unwind(|| {
        let _g = ZSlideFailStop::default();
        panic!("before the first memmove");
    });
    assert!(r.is_err());
}

/// The source filter answers exactly: false only off every source page.
#[test]
fn source_pages_filter_is_exact_and_stands_down_off_arena() {
    let page = ZgcRealHeap::Z_LOGICAL_PAGE_BYTES;
    let lo = 0x1000_0000usize;
    let hi = lo + 8 * page;
    let f = ZSourcePages::new(&[(lo + 3 * page + 64, lo)], lo, hi, page);
    assert!(f.may_be_source(lo + 3 * page));
    assert!(!f.may_be_source(lo + 2 * page));
    assert!(!f.may_be_source(hi + 8));
    let g = ZSourcePages::new(&[(hi + 64, lo)], lo, hi, page);
    assert!(
        g.may_be_source(lo),
        "an off-arena source stands the filter down"
    );
}

/// The fused fixture: sparse low pages (every eighth object survives) and
/// a high end of large objects interleaved with dead ones (freed), wired
/// into one reference graph that crosses both ends. Returns the heap, the
/// live set (ascending) and the low and high survivors.
fn fused_fixture() -> (ZgcRealHeap, Vec<usize>, Vec<ObjectRef>, Vec<ObjectRef>) {
    const PAGE: usize = ZgcRealHeap::Z_LOGICAL_PAGE_BYTES;
    const FIELDS: usize = 500;
    const BIG: usize = 12_000;
    let heap = ZgcRealHeap::with_capacity(64 * 1024 * 1024);
    heap.set_tlab_enabled(false);
    let per_page = PAGE / (HEADER_SIZE + FIELDS * SLOT_SIZE);
    let mut low: Vec<ObjectRef> = Vec::new();
    for _page in 0..4 {
        for i in 0..per_page {
            let o = heap.alloc_object(ClassId::new(1), FIELDS);
            if i % 8 == 0 {
                low.push(o);
            }
        }
    }
    let (base, capacity) = {
        let a = heap.arena.lock();
        (a.base_ptr() as usize, a.capacity())
    };
    let mut top = base + capacity;
    let mut dead: Vec<(usize, usize)> = Vec::new();
    let mut high: Vec<ObjectRef> = Vec::new();
    for _ in 0..8 {
        let d = heap.alloc_object(ClassId::new(1), BIG).as_ptr() as usize;
        dead.push((d, top));
        let o = heap.alloc_object(ClassId::new(1), BIG);
        top = o.as_ptr() as usize;
        high.push(o);
    }
    {
        let mut arena = heap.arena.lock();
        for (lo, hi) in &dead {
            arena.add_free_block(lo - base, hi - lo);
        }
        arena.coalesce_high();
    }
    for (i, o) in low.iter().enumerate() {
        heap.set_field(*o, 0, Value::Object(Some(low[(i + 1) % low.len()])));
        heap.set_field(*o, 1, Value::Object(Some(high[i % high.len()])));
        heap.set_field(*o, 2, Value::Long(i as i64));
    }
    for (j, o) in high.iter().enumerate() {
        heap.set_field(*o, 0, Value::Object(Some(low[(j * 3) % low.len()])));
        heap.set_field(*o, 1, Value::Object(Some(high[(j + 1) % high.len()])));
    }
    let mut live: Vec<usize> = low
        .iter()
        .chain(high.iter())
        .map(|r| r.as_ptr() as usize)
        .collect();
    live.sort_unstable();
    (heap, live, low, high)
}

/// What one arm of the slide produced, in arena OFFSETS so two heaps
/// compare: (moved, reclaimed, sorted moves, the graph as
/// `(holder, field, target)` after the slide).
type ArmResult = (
    usize,
    usize,
    Vec<(usize, usize)>,
    Vec<(usize, usize, usize)>,
);

fn run_arm(fused: bool) -> ArmResult {
    FUSED_SLIDE_SCREEN_OVERRIDE.with(|c| c.set(Some(fused)));
    let (heap, live, low, high) = fused_fixture();
    let base = heap.arena.lock().base_ptr() as usize;
    let (moved, reclaimed, map) = heap.relocate_stw_for_test(&live);
    FUSED_SLIDE_SCREEN_OVERRIDE.with(|c| c.set(None));
    let now = |r: &ObjectRef| {
        let a = r.as_ptr() as usize;
        map.get(&a).copied().unwrap_or(a)
    };
    let mut moves: Vec<(usize, usize)> =
        map.iter().map(|(f, t)| (f - base, t - base)).collect();
    moves.sort_unstable();
    let target_of = |h: ObjectRef, field: usize| -> usize {
        match heap.get_field(h, field) {
            Value::Object(Some(t)) => t.as_ptr() as usize,
            other => panic!("fused={fused}: field {field} lost: {other:?}"),
        }
    };
    let mut graph = Vec::new();
    for (i, o) in low.iter().enumerate() {
        let h = unsafe { ObjectRef::from_raw(now(o) as *mut u8) };
        let t0 = target_of(h, 0);
        let t1 = target_of(h, 1);
        // Correctness, not only equivalence: each field names where its
        // target is NOW, at either end.
        assert_eq!(
            t0,
            now(&low[(i + 1) % low.len()]),
            "fused={fused}: low {i} f0"
        );
        assert_eq!(
            t1,
            now(&high[i % high.len()]),
            "fused={fused}: low {i} f1 (high)"
        );
        assert_eq!(
            heap.get_field(h, 2),
            Value::Long(i as i64),
            "payload survives"
        );
        graph.push((now(o) - base, 0, t0 - base));
        graph.push((now(o) - base, 1, t1 - base));
    }
    for (j, o) in high.iter().enumerate() {
        let h = unsafe { ObjectRef::from_raw(now(o) as *mut u8) };
        let t0 = target_of(h, 0);
        let t1 = target_of(h, 1);
        assert_eq!(
            t0,
            now(&low[(j * 3) % low.len()]),
            "fused={fused}: high {j} f0 (low)"
        );
        assert_eq!(
            t1,
            now(&high[(j + 1) % high.len()]),
            "fused={fused}: high {j} f1"
        );
        graph.push((now(o) - base, 0, t0 - base));
        graph.push((now(o) - base, 1, t1 - base));
    }
    let high_moved = high
        .iter()
        .filter(|o| now(o) != o.as_ptr() as usize)
        .count();
    let low_moved = low.iter().filter(|o| now(o) != o.as_ptr() as usize).count();
    assert!(high_moved > 0, "fused={fused}: the high end must compact");
    assert!(low_moved > 0, "fused={fused}: the low end must slide");
    (moved, reclaimed, moves, graph)
}

/// THE EQUIVALENCE TEST for `CRATONVM_ZGC_FUSED_SLIDE_SCREEN`: the fused
/// arm (screen inside the rewrite, high end compacted before the low cursor
/// drops) and the separate-pass arm move the same objects to the same
/// places, reclaim the same bytes and leave the same reference graph, over
/// a fixture that moves objects at BOTH ends with references crossing
/// between them.
#[test]
fn the_fused_screen_arm_equals_the_separate_pass_arm() {
    let fused = run_arm(true);
    let separate = run_arm(false);
    assert!(fused.0 > 0, "the fixture must move something");
    assert_eq!(fused.0, separate.0, "moved");
    assert_eq!(fused.1, separate.1, "reclaimed");
    assert_eq!(fused.2, separate.2, "the same moves");
    assert_eq!(fused.3, separate.3, "the same reference graph");
}
