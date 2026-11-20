use super::*;

fn obj(addr: usize) -> ObjectRef {
    // Keys only: never dereferenced.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn scan(rows: &MemoRows<u32>, addr: usize) -> Option<usize> {
    rows.iter().position(|(l, _)| l.as_ptr() as usize == addr)
}

fn agree(rows: &mut MemoRows<u32>, generation: u64, addrs: &[usize]) {
    for &a in addrs {
        let want = scan(rows, a);
        assert_eq!(rows.position_of(generation, a, |id| Some(*id)), want, "addr {a:#x}");
    }
}

#[test]
fn zt2nd_the_memo_answers_what_the_scan_answers() {
    let mut rows = MemoRows::<u32>::new();
    let addrs = [0x1000, 0x2000, 0x3000, 0x4000, 0x5000];
    agree(&mut rows, 0, &addrs);
    rows.push((obj(0x1000), 3));
    rows.push((obj(0x2000), 4));
    agree(&mut rows, 0, &addrs);
    // Appended tail, and a duplicate address: the FIRST row wins.
    rows.push((obj(0x3000), 5));
    rows.push((obj(0x1000), 6));
    agree(&mut rows, 0, &addrs);
    assert_eq!(rows.position_of(0, 0x1000, |id| Some(*id)), Some(0));
    // A value changed in place keeps its position.
    rows[1].1 = 9;
    agree(&mut rows, 0, &addrs);
    // A shorter `retain` (a test helper's) rebuilds.
    rows.retain(|(_, id)| *id != 3);
    agree(&mut rows, 0, &addrs);
    // The GC pass remaps keys in place (same length) AFTER bumping the
    // generation: the memo rebuilds.
    for (l, _) in rows.iter_mut() {
        *l = obj(l.as_ptr() as usize + 0x10_0000);
    }
    let moved: Vec<usize> = addrs.iter().map(|a| a + 0x10_0000).collect();
    agree(&mut rows, 1, &moved);
    agree(&mut rows, 1, &addrs);
    // A clear, then the same length again in the same generation.
    rows.clear_rows();
    rows.push((obj(0x4000), 7));
    rows.push((obj(0x5000), 8));
    rows.push((obj(0x2000), 10));
    agree(&mut rows, 1, &addrs);
}

#[test]
fn zt2nd_the_id_memo_finds_the_first_row_of_an_id() {
    let mut rows = MemoRows::<u32>::new();
    rows.push((obj(0x1000), 3));
    rows.push((obj(0x2000), 4));
    rows.push((obj(0x3000), 4));
    rows.sync(0, |id| Some(*id));
    assert_eq!(rows.memo.by_key.get(&4).copied(), Some(1));
    assert!(row_has_id(&rows, 1, 4));
    assert!(!row_has_id(&rows, 0, 4));
    assert!(row_has_loader(&rows, 2, 0x3000));
    assert!(!row_has_loader(&rows, 5, 0x3000));
}
