use super::{i_plus_b_due, i_plus_b_nominates, I_PLUS_B_RETRY_BACKEDGES};

/// HotSpot's numbers: 100 calls of a 20-iteration loop are enough
/// (`100 + 2000`), 99 are not, and no loop is enough below 100 calls.
#[test]
fn the_predicate_is_hotspots_tier3_back_edge_half() {
    assert!(i_plus_b_due(100, 1_900));
    assert!(i_plus_b_due(100, 2_000));
    assert!(!i_plus_b_due(99, 1_000_000));
    assert!(!i_plus_b_due(150, 1_849));
    assert!(i_plus_b_due(u32::MAX, u32::MAX), "saturates");
}

/// One nomination at the crossing, then one per retry stride.
#[test]
fn it_nominates_at_the_crossing_and_on_the_retry_stride_only() {
    // Short loops: the predicate turns on at the 100th call.
    assert!(i_plus_b_nominates(99, 1_980, 100, 2_000));
    assert!(!i_plus_b_nominates(100, 2_000, 101, 2_020));
    // Long loops: `b` alone is past the bar long before `i` reaches 100;
    // the crossing is the 100th call, not the back edges.
    assert!(i_plus_b_nominates(99, 50_000, 100, 50_500));
    // The retry stride.
    let s = I_PLUS_B_RETRY_BACKEDGES;
    assert!(i_plus_b_nominates(200, 3 * s - 1, 201, 3 * s + 19));
    assert!(!i_plus_b_nominates(200, 3 * s, 201, 3 * s + 20));
    // Never below the bar.
    assert!(!i_plus_b_nominates(10, 0, 11, 20));
}
