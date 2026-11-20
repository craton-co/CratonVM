use super::*;

#[test]
fn writes_are_recorded_once_tracking_is_on_and_removal_forgets() {
    enable_dirty_tracking();
    let mut rows: OverlayRows<u32> = OverlayRows::new();
    rows.insert(1, 10);
    *rows.entry(2).or_default() += 1;
    if let Some(v) = rows.get_mut(&1) {
        *v += 1;
    }
    assert!(rows.is_dirty(1) && rows.is_dirty(2));
    let _ = rows.get(&1);
    let taken = rows.take_dirty();
    assert_eq!(taken.len(), 2);
    assert!(!rows.is_dirty(1), "taking clears the record");
    let _ = rows.get(&1);
    assert!(!rows.is_dirty(1), "a read is not a write");
    rows.insert(3, 0);
    rows.remove(&3);
    assert!(!rows.is_dirty(3), "a removed row leaves the record");
    rows.restore_dirty([1, 99]);
    assert!(rows.is_dirty(1));
    assert!(
        !rows.is_dirty(99),
        "only keys that still have a row come back"
    );
    for (_, v) in rows.gc_iter_mut() {
        *v += 1;
    }
    rows.take_dirty();
    rows.gc_retain(|_, _| true);
    assert!(
        !rows.is_dirty(1),
        "the collector's own writes are not recorded"
    );
}
