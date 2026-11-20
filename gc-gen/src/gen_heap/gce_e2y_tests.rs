use super::*;

/// The line names every counter the gate reads, and on a fresh heap with
/// the fixed age they read "nothing promoted, threshold 2, never adapted".
#[test]
fn e2y_the_gen_alloc_line_carries_the_tenuring_gate_counters() {
    // gcg g1/y: pinned, so the fixed-age assertions below always run.
    let heap = GenerationalHeap::with_sizes(64 * 1024, 1024 * 1024).with_fixed_promotion_age();
    let line = heap.alloc_path_census_line();
    for key in [
        "gen_objects_promoted=0 ",
        "gen_bytes_promoted=0 ",
        "gen_tenuring_threshold=",
        "gen_tenuring_adapted=",
        "gen_tenuring_raised=",
        "gen_tenuring_lowered=",
    ] {
        assert!(line.contains(key), "{key:?} missing from {line}");
    }
    assert!(!heap.tenuring.engaged(adaptive_tenuring_flag()));
    assert!(line.contains("gen_tenuring_threshold=2 "), "{line}");
    // gci i4/yg appended the `gen_refill_exhausted_*` keys after it.
    assert!(line.contains("gen_tenuring_lowered=0 "), "{line}");
}
