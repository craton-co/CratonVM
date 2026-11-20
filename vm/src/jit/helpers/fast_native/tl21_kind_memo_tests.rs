use super::{TlKindMemo, TL_KIND_MEMO_SLOTS, TL_SET_MEMO_TAG};

#[test]
fn tl21_a_rotation_of_thirty_two_classes_stays_resident() {
    let mut memo = TlKindMemo::new();
    assert_eq!(memo.lookup(7, 1), None, "the first ask stamps the epoch");
    for cid in 1..=32u32 {
        memo.insert(7, cid, (cid % 3) as u8);
        memo.insert(7, cid | TL_SET_MEMO_TAG, (cid % 2) as u8);
    }
    for round in 0..4 {
        for cid in 1..=32u32 {
            assert_eq!(
                memo.lookup(7, cid),
                Some((cid % 3) as u8),
                "get {cid} r{round}"
            );
            assert_eq!(
                memo.lookup(7, cid | TL_SET_MEMO_TAG),
                Some((cid % 2) as u8),
                "set {cid} r{round}"
            );
        }
    }
}

#[test]
fn tl21_a_colliding_class_evicts_only_its_slot_and_a_new_epoch_empties_all() {
    let mut memo = TlKindMemo::new();
    assert_eq!(memo.lookup(1, 5), None);
    // Class ids a whole table apart (half the slots, the tag shifting one
    // bit) share a slot.
    let a = 5u32;
    let b = a + (TL_KIND_MEMO_SLOTS / 2) as u32;
    assert_eq!(TlKindMemo::slot(a), TlKindMemo::slot(b));
    assert_ne!(TlKindMemo::slot(a), TlKindMemo::slot(a | TL_SET_MEMO_TAG));
    memo.insert(1, a, 1);
    memo.insert(1, 6, 2);
    memo.insert(1, b, 0);
    assert_eq!(memo.lookup(1, a), None, "evicted by its collider");
    assert_eq!(memo.lookup(1, b), Some(0));
    assert_eq!(memo.lookup(1, 6), Some(2), "an unrelated slot is untouched");
    // Another epoch: emptied, and an insert under the OLD epoch is dropped.
    assert_eq!(memo.lookup(2, 6), None);
    memo.insert(1, 6, 9);
    assert_eq!(memo.lookup(2, 6), None);
    memo.insert(2, 6, 1);
    assert_eq!(memo.lookup(2, 6), Some(1));
}

#[test]
fn tl21_key_zero_is_never_stored_or_served() {
    let mut memo = TlKindMemo::new();
    assert_eq!(memo.lookup(3, 0), None);
    memo.insert(3, 0, 2);
    assert_eq!(memo.lookup(3, 0), None);
}
