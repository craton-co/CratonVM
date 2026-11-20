use super::*;

/// Round 12 wave 5 (lane ffm2): the call site ORs the kind into the layout
/// reference; every kind code must fit under the mask and split back out
/// with the layout address intact.
#[test]
fn the_layout_and_the_kind_split_back_apart() {
    let layout: i64 = 0x7f00_1234_5678;
    assert_eq!(layout & FFM_KIND_MASK, 0, "a heap reference is 8-aligned");
    for kind in [
        FFM_KIND_BYTE,
        FFM_KIND_SHORT,
        FFM_KIND_CHAR,
        FFM_KIND_INT,
        FFM_KIND_LONG,
        FFM_KIND_FLOAT,
        FFM_KIND_DOUBLE,
    ] {
        assert!(
            kind & !FFM_KIND_MASK == 0,
            "kind {kind} does not fit the mask"
        );
        assert_eq!(ffm_split_layout_kind(layout | kind), (layout as u64, kind));
    }
    // A null layout (the site passed `null`) still splits, and declines.
    assert_eq!(ffm_split_layout_kind(FFM_KIND_INT), (0, FFM_KIND_INT));
    assert_eq!(ffm_layout_align_mask(0), None);
}

/// A layout the native never read in host order has no verdict, so the
/// fast path declines (the native then swaps or throws).
#[test]
fn a_layout_without_a_verdict_is_declined() {
    assert_eq!(ffm_layout_align_mask(0x7f00_1234_5678), None);
}
