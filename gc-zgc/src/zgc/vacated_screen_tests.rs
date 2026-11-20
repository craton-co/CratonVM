use super::{
    block_overlapping_spans, merge_ascending_spans, relocate_cost_gate_declines,
    ZGC_FRAG_GAUGE_MIN_FREE_PERMILLE, ZGC_RELOCATE_COST_GATE_RATIO,
};

/// The screen `relocate_stw` ran until wave 6: merge the stayed and slid
/// extents, take a prefix maximum of the ends, and per span ask whether
/// anything starting below `hi` ends above `lo`. Kept verbatim as the
/// reference.
fn old_screen(
    stayed: Vec<(usize, usize)>,
    slid: Vec<(usize, usize)>,
    spans: &[(usize, usize)],
) -> Vec<bool> {
    let live_after = merge_ascending_spans(stayed, slid);
    let mut max_end: Vec<usize> = Vec::with_capacity(live_after.len());
    let mut running = 0usize;
    for &(_, e) in &live_after {
        running = running.max(e);
        max_end.push(running);
    }
    spans
        .iter()
        .map(|&(lo, hi)| {
            let idx = live_after.partition_point(|(st, _)| *st < hi);
            idx > 0 && max_end[idx - 1] > lo
        })
        .collect()
}

fn new_screen(extents: &[(usize, usize)], spans: &[(usize, usize)]) -> Vec<bool> {
    let mut cands: Vec<(usize, usize, bool)> =
        spans.iter().map(|&(lo, hi)| (lo, hi, false)).collect();
    for &(now, end) in extents {
        block_overlapping_spans(&mut cands, now, end);
    }
    cands.iter().map(|c| c.2).collect()
}

/// Every span answer equals the old materialised screen's, over extents
/// that end before, touch, straddle, sit inside, cover several spans, and
/// arrive in any order -- including the fail-closed "unsizable" extent that
/// runs to the cursor.
#[test]
fn the_streaming_screen_answers_what_the_prefix_max_screen_answered() {
    // Four 0x100-byte page spans with a gap between the 2nd and 3rd.
    let spans = vec![
        (0x1000, 0x1100),
        (0x1100, 0x1200),
        (0x1400, 0x1500),
        (0x1500, 0x1600),
    ];
    let cases: Vec<(Vec<(usize, usize)>, Vec<(usize, usize)>)> = vec![
        // Nothing live above the spans' floor.
        (vec![(0x100, 0x200), (0x800, 0x1000)], vec![(0x200, 0x240)]),
        // An extent ending exactly at a span's `lo` does not block it.
        (vec![(0xf00, 0x1000)], vec![]),
        // A straddler from below into span 0.
        (vec![(0xf80, 0x1008)], vec![]),
        // Wholly inside span 1, and one inside the gap (blocks nothing).
        (vec![(0x1180, 0x11c0), (0x1280, 0x1300)], vec![]),
        // One extent covering spans 2 and 3 (the unsizable, fail-closed shape).
        (vec![(0x14f0, 0x2000)], vec![]),
        // Starting exactly at a span's `hi` does not block it; at its `lo` does.
        (vec![(0x1200, 0x1240), (0x1500, 0x1508)], vec![]),
        // Slid extents below, stayed ones above; out-of-order input too.
        (
            vec![(0x1580, 0x15a0), (0x1010, 0x1020)],
            vec![(0x800, 0x900), (0x900, 0x980)],
        ),
        (vec![], vec![]),
    ];
    for (stayed, slid) in cases {
        let want = old_screen(stayed.clone(), slid.clone(), &spans);
        let all: Vec<(usize, usize)> = stayed.iter().chain(slid.iter()).copied().collect();
        assert_eq!(
            new_screen(&all, &spans),
            want,
            "stayed={stayed:x?} slid={slid:x?}"
        );
    }
    // And the answers are the intended ones, not merely equal: the
    // straddler blocks span 0 only; the covering extent blocks 2 and 3.
    assert_eq!(
        new_screen(&[(0xf80, 0x1008)], &spans),
        vec![true, false, false, false]
    );
    assert_eq!(
        new_screen(&[(0x14f0, 0x2000)], &spans),
        vec![false, false, true, true]
    );
    assert_eq!(new_screen(&[(0xf00, 0x1000)], &spans), vec![false; 4]);
}

/// The gate declines only on a roomy heap whose selected garbage is under
/// `1/RATIO` of the live bytes, and never on an empty live set.
#[test]
fn the_cost_gate_declines_only_a_cheap_reclaim_on_a_roomy_heap() {
    let roomy = ZGC_FRAG_GAUGE_MIN_FREE_PERMILLE;
    let tight = ZGC_FRAG_GAUGE_MIN_FREE_PERMILLE - 1;
    let live = 320 * 1024 * 1024;
    let small = live / ZGC_RELOCATE_COST_GATE_RATIO - 1;
    let enough = live / ZGC_RELOCATE_COST_GATE_RATIO;
    assert!(relocate_cost_gate_declines(small, live, roomy));
    assert!(relocate_cost_gate_declines(0, live, 1000));
    assert!(!relocate_cost_gate_declines(enough, live, roomy));
    assert!(!relocate_cost_gate_declines(small, live, tight));
    assert!(!relocate_cost_gate_declines(0, 0, 1000));
    // No overflow on absurd inputs.
    assert!(!relocate_cost_gate_declines(usize::MAX, usize::MAX, 1000));
}
