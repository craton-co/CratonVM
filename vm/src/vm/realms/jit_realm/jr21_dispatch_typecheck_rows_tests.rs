use super::SupersDisplays;
use cratonvm_jit::typecheck_rows::{
    iface_slot_word, ROWS_CAP, ROW_BYTES, ROW_WORDS, STATUS_DEPTH_SHIFT, STATUS_HIERARCHY_ONLY,
    STATUS_MASK, STATUS_PUBLISHED,
};

fn row(addr: usize, id: u32) -> [u32; ROW_WORDS] {
    let mut out = [0u32; ROW_WORDS];
    // Cast: a row index below `ROWS_CAP`.
    let p = (addr + id as usize * ROW_BYTES) as *const u32;
    for (i, w) in out.iter_mut().enumerate() {
        // SAFETY: the rows `d` owns are alive in the caller; `id <
        // ROWS_CAP` and `i < ROW_WORDS` keep the read inside the buffer.
        *w = unsafe { p.add(i).read_volatile() };
    }
    out
}

#[test]
fn rows_are_backfilled_then_filled_and_never_move() {
    let d = SupersDisplays::new();
    if !d.enabled() {
        return; // displays switched off in this environment
    }
    d.publish_with_verdict(7, &[1, 7], &[40], true);
    let addr = d.typecheck_rows_addr();
    assert_ne!(addr, 0);
    assert_eq!(addr % ROW_BYTES, 0);
    let r7 = row(addr, 7);
    assert_eq!(r7[0] & STATUS_MASK, STATUS_HIERARCHY_ONLY, "backfilled");
    // JIT round 21, lane tcmiss: the class's own depth rides in byte 1.
    assert_eq!(r7[0] >> STATUS_DEPTH_SHIFT, 1);
    assert_eq!(r7[1], 7);
    assert_eq!(r7[iface_slot_word(40)], 40);
    d.publish_with_verdict(9, &[1, 7, 9], &[41], false);
    assert_eq!(d.typecheck_rows_addr(), addr, "the rows do not move");
    let r9 = row(addr, 9);
    assert_eq!(r9[0] & STATUS_MASK, STATUS_PUBLISHED);
    assert_eq!(r9[0] >> STATUS_DEPTH_SHIFT, 2);
    assert_eq!(&r9[1..3], &[7, 9]);
    assert_eq!(r9[iface_slot_word(41)], 41);
    // A class past the rows gets a display and no row.
    d.publish_with_verdict(ROWS_CAP + 3, &[1, ROWS_CAP + 3], &[], true);
    assert_eq!(d.depth_of(ROWS_CAP + 3), Some(1));
    assert_eq!(row(addr, 8), [0u32; ROW_WORDS], "untouched row");
    // A second publication of a class changes nothing (first one wins).
    d.publish_with_verdict(7, &[1, 9, 7], &[], false);
    assert_eq!(row(addr, 7), r7, "a row is written once");
}
