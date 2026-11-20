use super::AddrMemo;
use std::rc::Rc;

#[test]
fn two_objects_of_one_set_both_hit() {
    let mut memo: AddrMemo<u32> = AddrMemo::new(64);
    let a = 0x1000usize;
    let set_a = memo.set_index(a);
    // Find a second address that maps to the same set.
    let mut b = a + 8;
    while memo.set_index(b) != set_a {
        b += 8;
    }
    memo.insert(1, 7, a, Rc::new(10));
    memo.insert(1, 7, b, Rc::new(20));
    assert_eq!(memo.get(1, 7, a).as_deref().copied(), Some(10));
    assert_eq!(memo.get(1, 7, b).as_deref().copied(), Some(20));
    assert_eq!(memo.get(1, 7, a).as_deref().copied(), Some(10));
    // vm == 0 always refuses; different epoch misses.
    assert!(memo.get(0, 7, a).is_none());
    assert!(memo.get(1, 8, a).is_none());
}
