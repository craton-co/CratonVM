use super::*;

/// A Locale row of one VM is listed for it and not for another; the list
/// is read-only (the row is still there afterwards).
#[test]
fn gci_i30_rp_weak_row_owners_lists_this_vm_s_locale_rows_only() {
    const VM: usize = 0x3030_0001;
    const OTHER: usize = 0x3030_0002;
    const OWNER: usize = 0x7a30_0040;
    // SAFETY: a key only; nothing below dereferences it.
    let owner = unsafe { ObjectRef::from_raw(OWNER as *mut u8) };
    locale_data_set(VM, owner, "tr", "TR", "");
    let mut mine = Vec::new();
    gc_weak_row_owners(VM, &mut mine);
    let mut theirs = Vec::new();
    gc_weak_row_owners(OTHER, &mut theirs);
    let still_there = !locale_data_get(owner).0.is_empty();
    forget_vm_locale_state(VM);
    assert!(mine.contains(&OWNER), "{mine:x?}");
    assert!(!theirs.contains(&OWNER), "{theirs:x?}");
    assert!(still_there, "listing owners must not drop rows");
}
