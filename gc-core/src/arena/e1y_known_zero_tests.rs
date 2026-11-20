use super::*;

/// A sweep-shaped arena: 1 MiB bumped, two holes listed and claimed zero.
fn swept() -> Arena {
    let mut a = Arena::new(4 << 20);
    let _ = a.alloc(1 << 20, 8).expect("bump");
    a.add_free_block(4096, 8192);
    a.add_free_block(65536, 4096);
    a.set_known_zero_free_spans(vec![(4096, 8192), (65536, 4096)]);
    a
}

#[test]
fn e1y_a_hand_out_is_known_zero_only_wholly_inside_one_span() {
    let a = swept();
    assert_eq!(a.known_zero_bytes(), 12288);
    assert!(a.hand_out_is_known_zero(4096, 8192), "the whole span");
    assert!(a.hand_out_is_known_zero(4096 + 64, 128), "inside");
    assert!(a.hand_out_is_known_zero(65536, 4096));
    assert!(
        !a.hand_out_is_known_zero(4096 + 8192 - 8, 16),
        "straddles the end"
    );
    assert!(
        !a.hand_out_is_known_zero(4088, 16),
        "starts before the span"
    );
    assert!(!a.hand_out_is_known_zero(32768, 64), "between spans");
    assert!(
        !a.hand_out_is_known_zero(4096, 0),
        "an empty hand-out claims nothing"
    );
    assert!(!a.hand_out_is_known_zero(usize::MAX - 4, 16), "no overflow");
    assert!(
        !Arena::new(1 << 20).hand_out_is_known_zero(0, 8),
        "no set installed"
    );
}

#[test]
fn e1y_doors_that_relist_handed_out_bytes_forget_the_set() {
    let mut a = swept();
    a.add_free_block(262144, 64);
    assert_eq!(a.known_zero_bytes(), 0, "add_free_block");

    let mut a = swept();
    a.clear_free_list();
    assert_eq!(a.known_zero_bytes(), 0, "clear_free_list");

    let mut a = swept();
    let _ = a.rebuild_low_free_list(&[(4096, 8192)]);
    assert_eq!(a.known_zero_bytes(), 0, "rebuild_low_free_list");

    let mut a = swept();
    a.reset();
    assert_eq!(a.known_zero_bytes(), 0, "reset");

    let mut a = swept();
    let _ = a.reset_deferring_zero();
    assert_eq!(a.known_zero_bytes(), 0, "reset_deferring_zero");

    let mut a = swept();
    let _ = a.return_unused_tail(131072, 4096, 0);
    assert_eq!(a.known_zero_bytes(), 0, "return_unused_tail");

    let mut a = swept();
    let _ = a.retract_cursor_to(512 << 10);
    assert_eq!(a.known_zero_bytes(), 0, "retract_cursor_to");
}

#[test]
fn zt3gn_splice_keeps_blocks_outside_windows_and_drops_the_rest() {
    let mut a = Arena::new(4 << 20);
    let _ = a.alloc(1 << 20, 8).expect("bump");
    // before the window, inside it, straddling the floor, above the floor
    a.add_free_block(4096, 4096);
    a.add_free_block(16384, 4096);
    a.add_free_block(24576, 8192);
    a.add_free_block(40960, 4096);
    let before = a.free_list_bytes();
    let (removed, in_win) =
        a.splice_low_free_list(&[(16384, 8192)], 1 << 19, 28672, &[(16384, 8192), (28672, 65536)]);
    // dropped: the 4096 inside the window, the 4096 above the floor; the
    // 8192 block is clipped to 4096 (loss 4096).
    assert_eq!((removed, in_win), (4096 + 4096 + 4096, 4096));
    let mut got = a.low_blocks_sorted();
    got.sort();
    assert_eq!(
        got,
        vec![(4096, 4096), (16384, 8192), (24576, 4096), (28672, 65536)]
    );
    assert_eq!(a.free_list_bytes(), before - removed + 8192 + 65536);
}

#[test]
fn e1y_a_merge_and_a_split_keep_the_set() {
    let mut a = Arena::new(4 << 20);
    let _ = a.alloc(1 << 20, 8).expect("bump");
    // Two ADJACENT holes, so the coalescer has something to merge.
    a.add_free_block(8192, 4096);
    a.add_free_block(12288, 4096);
    a.set_known_zero_free_spans(vec![(8192, 8192)]);
    let _ = a.coalesce_free_list();
    assert_eq!(
        a.known_zero_bytes(),
        8192,
        "coalesce re-lists only listed bytes"
    );
    // A free-list hand-out splits the block; the remnant stays listed and
    // the claim about the bytes stays as it was.
    let before = a.used_low_for_compaction();
    let p = a.alloc(1024, 8).expect("a free-list hand-out");
    let off = p as usize - a.base_ptr() as usize;
    assert!(off < before, "served from the free list, below the cursor");
    assert!(a.hand_out_is_known_zero(off, 1024));
    assert_eq!(a.known_zero_bytes(), 8192);
}
