use super::*;

fn obj(addr: usize) -> ObjectRef {
    // SAFETY: a key only, never dereferenced.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

/// Three rows, and a lookup at generation 1 (the reconcile's bump has
/// happened, its rewrite has not).
fn rows_seen_in_the_window() -> MemoRows<u32> {
    let mut rows = MemoRows::<u32>::new();
    rows.push((obj(0x1000), 1));
    rows.push((obj(0x2000), 2));
    rows.push((obj(0x3000), 3));
    assert_eq!(rows.position_of(1, 0x1000, |id| Some(*id)), Some(0));
    rows
}

/// The remap the reconcile applies: the FIRST row's loader moved, the
/// last one did not (same length, same last address).
fn remap(row: &mut (ObjectRef, u32)) -> bool {
    if row.0.as_ptr() as usize == 0x1000 {
        row.0 = obj(0x9000);
    }
    true
}

#[test]
fn zt3x2_a_remap_after_a_lookup_in_the_window_is_found() {
    let mut rows = rows_seen_in_the_window();
    rows.retain_rows_mut(remap);
    assert_eq!(rows.position_of(1, 0x9000, |id| Some(*id)), Some(0));
    assert_eq!(rows.position_of(1, 0x1000, |id| Some(*id)), None);
}

/// The positive control: the same remap through the plain `Vec` (the t2
/// shape of the reconcile) leaves the memo answering the old address and
/// missing the new one, which a scan finds.
#[test]
fn zt3x2_control_a_plain_retain_leaves_the_memo_stale() {
    let mut rows = rows_seen_in_the_window();
    rows.retain_mut(remap);
    assert_eq!(rows.iter().position(|(l, _)| l.as_ptr() as usize == 0x9000), Some(0));
    assert_eq!(rows.position_of(1, 0x9000, |id| Some(*id)), None, "the stale miss");
}
