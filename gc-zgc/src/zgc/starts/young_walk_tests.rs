use super::*;

/// The young sweep's walk must not visit the off-grid spill set, because
/// `ZgcRealHeap::collect_garbage` sweeps that set **separately**, on one
/// thread, straight after whichever sweep arm ran
/// (`registered.for_each_spilled_base(sweep_floor, ..)`).
///
/// What breaks if this fails: `sweep_one` runs twice on every spilled base.
/// The first visit zeroes the header, pushes `(off, size)` onto the shard's
/// span list and removes the base from the registry; the second re-sizes
/// the corpse the first one made and pushes a **second span over the same
/// bytes**, after which `Arena::add_free_block` holds one address twice and
/// the allocator hands it to two objects. The spill set is empty on every
/// healthy run, which is why this needs a test rather than a workload.
#[test]
fn a_young_walk_leaves_the_spill_set_to_its_caller() {
    const BASE: usize = 0x20_0000;
    const SPAN: usize = 64 * 1024;
    const FLOOR: usize = BASE + 32 * 1024;

    let starts = ZObjectStarts::with_bitmap(BASE, SPAN, true);
    starts.insert(BASE + 8); // on the grid, below the floor
    starts.insert(FLOOR); // on the grid, at the floor
    starts.insert(FLOOR + 64); // on the grid, above the floor
    starts.insert(BASE + 12); // MISALIGNED: `locate` refuses, so it spills
    assert!(
        starts.has_spill(),
        "the fixture must actually produce a spill, or this test proves \
         nothing about the spill set",
    );

    let snap = starts.snapshot();

    // (1) No below-floor page accepted: exactly the two bases at or above
    //     the floor, and NOT the spilled one.
    let mut seen: Vec<usize> = Vec::new();
    snap.for_each_young_base(FLOOR, |_off| false, |b| seen.push(b));
    assert_eq!(
        seen,
        vec![FLOOR, FLOOR + 64],
        "a young walk must visit the nursery and nothing else; a spilled \
         base here is the double-sweep this test exists for",
    );

    // (2) Every below-floor page accepted: the grid base below the floor
    //     joins, ASCENDING across the join — and the spilled base still
    //     does not, because it belongs to the caller either way.
    let mut seen: Vec<usize> = Vec::new();
    snap.for_each_young_base(FLOOR, |_off| true, |b| seen.push(b));
    assert_eq!(seen, vec![BASE + 8, FLOOR, FLOOR + 64]);
    assert!(
        seen.windows(2).all(|w| w[0] < w[1]),
        "the sweep's run-merging needs ascending order across the join",
    );

    // (3) And the spilled base is reachable, through the accessor that owns
    //     it. It is not lost, it is someone else's.
    let mut spilled: Vec<usize> = Vec::new();
    snap.for_each_spilled_base(0, |b| spilled.push(b));
    assert_eq!(spilled, vec![BASE + 12]);
}
