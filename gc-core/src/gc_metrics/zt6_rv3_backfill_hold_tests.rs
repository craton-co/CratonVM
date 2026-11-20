//! zt t6/rv3 (`zgc-zs10rv7-*` item 5): the drain's busy-lock hold memo.
use super::*;

#[test]
fn zt6rv3_the_hold_stands_only_at_its_own_stamp() {
    let b = BackendGcBeans::with_order(BackendBeanShape::Zgc, true);
    assert!(!b.metaspace_backfill_held_at(3, 7), "no hold recorded");
    b.set_metaspace_backfill_hold(Some((3, 7)));
    assert!(b.metaspace_backfill_held_at(3, 7));
    assert!(!b.metaspace_backfill_held_at(4, 7), "a class was defined");
    assert!(!b.metaspace_backfill_held_at(3, 8), "a collection ended");
    b.set_metaspace_backfill_hold(None);
    assert!(!b.metaspace_backfill_held_at(3, 7), "cleared");
    assert_eq!(b.metaspace_backfill_retry_passes(), 0);
    assert_eq!(b.note_metaspace_backfill_retry_pass(), 1);
    assert_eq!(b.metaspace_backfill_retry_passes(), 1);
}
