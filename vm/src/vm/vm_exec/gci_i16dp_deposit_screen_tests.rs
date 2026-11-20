use super::deposit_keep_heap_addrs_from;
use crate::types::ObjectRef;

fn r(a: usize) -> ObjectRef {
    // SAFETY: an address only; never dereferenced.
    unsafe { ObjectRef::from_raw(a as *mut u8) }
}

/// The old screen, kept here as the oracle: `split_off` and re-push.
fn split_off_screen(v: &mut Vec<ObjectRef>, from: usize, keep: impl Fn(usize) -> bool) {
    let added = v.split_off(from);
    for o in added {
        if keep(o.as_ptr() as usize) {
            v.push(o);
        }
    }
}

/// gci i16/dp: the in-place screen leaves exactly what `split_off` and
/// re-push left -- prefix untouched, survivors in order, duplicates kept
/// -- for every split point and several keep rules, and allocates no new
/// buffer (the capacity is the vector's own).
#[test]
fn gci_i16dp_in_place_screen_matches_split_off() {
    let base: Vec<ObjectRef> = [0x1000, 0x2008, 0x3000, 0x2008, 0x4010, 0x5000, 0x6008, 0x7000]
        .iter()
        .map(|&a| r(a))
        .collect();
    let rules: [fn(usize) -> bool; 4] = [
        |_| true,
        |_| false,
        |a| a & 0xf == 0,
        |a| (0x3000..0x6000).contains(&a),
    ];
    for from in 0..=base.len() {
        for rule in rules {
            let mut expected = base.clone();
            split_off_screen(&mut expected, from, rule);
            let mut got = base.clone();
            let cap = got.capacity();
            let ptr = got.as_ptr();
            deposit_keep_heap_addrs_from(&mut got, from, rule);
            assert_eq!(got, expected, "from {from}");
            assert_eq!(got.capacity(), cap, "no reallocation");
            assert_eq!(got.as_ptr(), ptr, "the same buffer");
        }
    }
    let mut empty: Vec<ObjectRef> = Vec::new();
    deposit_keep_heap_addrs_from(&mut empty, 0, |_| true);
    assert!(empty.is_empty());
}
