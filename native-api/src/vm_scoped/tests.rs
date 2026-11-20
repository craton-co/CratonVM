use super::*;

/// The property the whole type exists for: one VM cannot read or clobber
/// another's row.
#[test]
fn rows_are_isolated_per_vm() {
    static T: VmScoped<Vec<u32>> = VmScoped::new();
    T.with(1, |row| row.push(10));
    T.with(2, |row| row.push(20));
    assert_eq!(T.peek(1, |r| r.clone()), Some(vec![10]));
    assert_eq!(T.peek(2, |r| r.clone()), Some(vec![20]));
    assert_eq!(T.peek(3, |r| r.clone()), None, "peek must not create a row");
    assert!(!T.has_row(3));
}

/// Teardown drops only the named VM — the shape `release_vm_native_state`
/// relies on when one VM is disposed of while another is still running.
#[test]
fn forget_drops_one_row_only() {
    static T: VmScoped<Vec<u32>> = VmScoped::new();
    T.with(7, |row| row.push(1));
    T.with(8, |row| row.push(2));
    T.forget(7);
    assert!(!T.has_row(7));
    assert_eq!(T.peek(8, |r| r.clone()), Some(vec![2]));
    T.forget(7);
}

/// gci i12/ul F5: `try_with` runs when the lock is free and declines,
/// without blocking, while it is held (here by this very thread).
#[test]
fn gci_i12_ul_try_with_declines_a_held_lock() {
    static T: VmScoped<Vec<u32>> = VmScoped::new();
    assert_eq!(T.try_with(1, |row| row.push(5)), Some(()));
    let inner = T.with(1, |row| {
        row.push(6);
        T.try_with(1, |r| r.len())
    });
    assert_eq!(inner, None, "a held table lock must decline, not block");
    assert_eq!(T.peek(1, |r| r.clone()), Some(vec![5, 6]));
}
