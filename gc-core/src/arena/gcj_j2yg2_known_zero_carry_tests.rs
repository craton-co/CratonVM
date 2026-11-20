use super::*;

const K: usize = 1024;

/// 1 MiB bumped, holes at 4K+8K and 64K+4K listed and claimed.
fn swept() -> Arena {
    let mut a = Arena::new(4 << 20);
    let _ = a.alloc(1 << 20, 8).expect("bump");
    a.add_free_block(4 * K, 8 * K);
    a.add_free_block(64 * K, 4 * K);
    a.set_known_zero_free_spans(vec![(4 * K, 8 * K), (64 * K, 4 * K)]);
    a
}

#[test]
fn gcj_j2yg2_union_sorts_merges_touching_spans_and_drops_empty_ones() {
    let u = union_known_zero_spans(
        vec![(64, 16), (0, 32), (200, 0)],
        vec![(32, 8), (100, 50), (120, 10)],
    );
    assert_eq!(u, vec![(0, 40), (64, 16), (100, 50)]);
    assert!(union_known_zero_spans(Vec::new(), Vec::new()).is_empty());
}

/// Only the claimed bytes that are still on the free list are carried:
/// a block partly handed out carries its free part, an unclaimed free
/// block carries nothing.
#[test]
fn gcj_j2yg2_known_zero_within_is_the_set_on_the_free_list() {
    let a = swept();
    assert_eq!(
        a.known_zero_within(&a.free_blocks_sorted()),
        vec![(4 * K, 8 * K), (64 * K, 4 * K)]
    );
    // The first hole's head went out (only its tail is free), and an
    // unclaimed block is free too.
    let free = [(6 * K, 6 * K), (32 * K, 4 * K), (64 * K, 4 * K)];
    assert_eq!(a.known_zero_within(&free), vec![(6 * K, 6 * K), (64 * K, 4 * K)]);
    assert!(a.known_zero_within(&[]).is_empty());
    assert!(Arena::new(1 << 20).known_zero_within(&free).is_empty(), "no set");
}

/// The keeping door: a free-listed tail joins the set (merged with a
/// span it touches), a retraction only clips the set at the new cursor.
/// The plain door still forgets (`e1y_doors_that_relist_handed_out_bytes_forget_the_set`).
#[test]
fn gcj_j2yg2_the_keeping_tail_door_keeps_the_set() {
    let mut a = swept();
    assert_eq!(
        a.return_unused_tail_keeping_known_zero(128 * K, 16 * K, 0),
        TailReturn::FreeListed
    );
    assert_eq!(a.known_zero_bytes(), 12 * K + 16 * K);
    assert!(a.hand_out_is_known_zero(128 * K, 16 * K), "the tail is claimed");
    assert!(a.hand_out_is_known_zero(4 * K, 8 * K), "the old spans stay");
    // A tail touching a claimed span merges with it.
    assert_eq!(
        a.return_unused_tail_keeping_known_zero(68 * K, 4 * K, 0),
        TailReturn::FreeListed
    );
    assert!(a.hand_out_is_known_zero(64 * K, 8 * K), "one span 64K..72K");

    // A retraction below every span leaves the set as it was.
    let cursor = a.used_low_for_compaction();
    let before = a.known_zero_bytes();
    assert_eq!(
        a.return_unused_tail_keeping_known_zero(cursor - 8 * K, 8 * K, 0),
        TailReturn::Retracted
    );
    assert_eq!(a.known_zero_bytes(), before);

    // A span that reaches the cursor is clipped at the new one.
    let mut b = Arena::new(4 << 20);
    let _ = b.alloc(1 << 20, 8).expect("bump");
    b.set_known_zero_free_spans(vec![(4 * K, 8 * K), ((1 << 20) - 16 * K, 16 * K)]);
    assert_eq!(
        b.return_unused_tail_keeping_known_zero((1 << 20) - 4 * K, 4 * K, 0),
        TailReturn::Retracted
    );
    assert_eq!(b.known_zero_bytes(), 8 * K + 12 * K);
    assert!(!b.hand_out_is_known_zero((1 << 20) - 4 * K, 8), "above the new cursor");
}
