use super::*;

/// The record owns the names its `info` borrows, the row points at the
/// record, and the emitter's decode finds the helper again.
#[test]
fn the_row_names_the_record_and_the_record_names_the_site() {
    let site = VarHandleJudgedSite::new(
        0x1000,
        0x2000,
        "java/lang/invoke/VarHandle",
        "set",
        "(LHolder;I)V",
        3,
        b'V',
        0,
        7,
        0,
    );
    assert_eq!(site.info.class_name, "java/lang/invoke/VarHandle");
    assert_eq!(site.info.method_name, "set");
    assert_eq!(site.info.descriptor, "(LHolder;I)V");
    assert_eq!(site.info.num_jit_args, 3);
    assert_eq!(site.info.declaring_class_id, 7);
    let row = site.direct_call(2, b'V');
    assert_eq!(row.guard_class_id, JIT_VARHANDLE_JUDGED_GUARD);
    assert!(row.needs_context);
    assert_eq!(row.entry, &*site as *const VarHandleJudgedSite as usize);
    assert_eq!(
        row.entry & VARHANDLE_JUDGED_SITE_TAG,
        0,
        "the tag bit is free"
    );
    assert_eq!(
        // SAFETY: `row.entry` is `site`, alive for the whole test.
        unsafe { varhandle_judged_site_helper(row.entry) },
        Some(0x2000)
    );
    // SAFETY: zero and a misaligned word are refused before any read.
    assert_eq!(unsafe { varhandle_judged_site_helper(0) }, None);
    // SAFETY: as above.
    assert_eq!(unsafe { varhandle_judged_site_helper(row.entry + 1) }, None);
}

/// The marker is outside every other guard word's meaning.
#[test]
fn the_judged_marker_is_no_other_guard() {
    assert_eq!(
        sync_direct_instance_guard_class(JIT_VARHANDLE_JUDGED_GUARD),
        None
    );
    for other in [
        JIT_CYCLE_EDGE_CELL_GUARD,
        JIT_RETIRE_CELL_GUARD,
        JIT_SELF_CALL_GUARD,
        JIT_SYNC_DIRECT_GUARD,
    ] {
        assert_ne!(JIT_VARHANDLE_JUDGED_GUARD, other);
    }
}
