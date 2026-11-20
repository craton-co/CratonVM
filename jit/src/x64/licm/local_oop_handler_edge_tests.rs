use super::compute_local_oop_masks_windowed;

// 0: aload_0   1: astore_1   2: aconst_null   3: athrow
// 4: astore_2  (handler for [0, 4))   5: return
const CODE: [u8; 6] = [0x2a, 0x4c, 0x01, 0xbf, 0x4d, 0xb1];

#[test]
fn a_handler_is_reached_through_its_protected_range() {
    let ranges = [(0usize, 4usize, 4usize)];
    let (masks, reached, windows) =
        compute_local_oop_masks_windowed(&CODE, CODE.len(), 3, 0b1, &ranges);
    assert_eq!(windows, 1);
    assert!(
        reached[4],
        "the handler is reached through the exception edge"
    );
    assert!(reached[5], "and the code after it");
    // Local 0 (the reference parameter) is an oop at every protected pc;
    // local 1 is an oop only AFTER pc 1's store, so the pc-0 and pc-1
    // exception edges (in & out) drop it.
    assert_eq!(
        masks[4], 0b001,
        "the handler keeps only what every throw point proves"
    );
    // `astore_2` of the caught exception makes local 2 an oop at pc 5.
    assert_eq!(masks[5], 0b101);
}

#[test]
fn without_ranges_the_handler_stays_unreached() {
    let (_masks, reached, _) = compute_local_oop_masks_windowed(&CODE, CODE.len(), 3, 0b1, &[]);
    assert!(
        !reached[4],
        "the historical graph: `athrow` has no successor"
    );
}
