use super::*;

/// Both paths that make the C call capture the callee-saved spill in
/// their own frame BEFORE the state is published, and keep it live until
/// AFTER the leave: a pause that claims the thread scans it there.
#[test]
fn both_downcall_paths_spill_before_the_entry_and_keep_it_past_the_leave() {
    let src = concat!(include_str!("../panama/mod.rs"), "\n", include_str!("../panama/alloc_segment_carrier.rs"), "\n", include_str!("../panama/register_pe_memory_segment_on.rs"), "\n", include_str!("../panama/pe_of_array_alias.rs"), "\n", include_str!("../panama/pe_swap_width.rs")).replace("\r\n", "\n");
    // JIT round 17 wave 1 (lane ffmseg): the memo path's body moved into
    // `pe_downcall_invoke_memo_with` (the row switch passed in).
    for sig in [
        "\npub(in crate::panama) fn pe_downcall_invoke_memo_with(",
        "\npub(in crate::panama) fn pe_downcall_invoke_general(",
    ] {
        let start = src.find(sig).unwrap_or_else(|| panic!("{sig} not found"));
        let rest = &src[start + 1..];
        let body = &rest[..rest.find("\n}\n").unwrap_or(rest.len())];
        let at = |needle: &str| {
            body.find(needle)
                .unwrap_or_else(|| panic!("{needle} missing in {sig}"))
        };
        let spill = at("Some(plf::CalleeSavedSpill::capture())");
        let enter = at("downcall_enter_gc_safe(ctx, &ctx_guard)");
        let call = at("libffi::raw::ffi_call(");
        let leave = at("ctx_guard.leave_region(ctx)");
        let keep = at("spill.keep_alive()");
        assert!(spill < enter, "{sig}: spill before the entry");
        assert!(enter < call && call < leave, "{sig}: entry, call, leave");
        assert!(leave < keep, "{sig}: the spill outlives the leave");
    }
}

/// JIT round 17 wave 5 (lane ffm4; FFM417-1): the two direct rows keep the
/// same order under `CRATONVM_JIT_FFM_DOWNCALL_DIRECT_LAZY`, and both
/// in-native readers default ON (the flip of
/// `CRATONVM_FFM_DOWNCALL_IN_NATIVE_LAZY`).
#[test]
fn direct_rows_spill_before_the_lazy_entry_and_the_lazy_state_defaults_on() {
    let src = concat!(include_str!("../panama/mod.rs"), "\n", include_str!("../panama/alloc_segment_carrier.rs"), "\n", include_str!("../panama/register_pe_memory_segment_on.rs"), "\n", include_str!("../panama/pe_of_array_alias.rs"), "\n", include_str!("../panama/pe_swap_width.rs")).replace("\r\n", "\n");
    for sig in [
        "\npub unsafe fn jit_downcall_direct_invoke(",
        "\npub unsafe fn jit_downcall_direct_invoke_regs(",
        "\npub unsafe fn jit_downcall_direct_segment_invoke(",
    ] {
        let start = src.find(sig).unwrap_or_else(|| panic!("{sig} not found"));
        let rest = &src[start + 1..];
        let body = &rest[..rest.find("\n}\n").unwrap_or(rest.len())];
        let at = |needle: &str| {
            body.find(needle)
                .unwrap_or_else(|| panic!("{needle} missing in {sig}"))
        };
        let spill = at("CalleeSavedSpill::capture())");
        let enter = at("guard.enter_region_lazy(ctx, None)");
        let call = at("direct_stub_call(");
        let leave = at("direct_row_leave_lazy(ctx, &guard)");
        let keep = at("spill.keep_alive()");
        let drop_guard = at("drop(guard)");
        assert!(spill < enter, "{sig}: spill before the entry");
        assert!(enter < call && call < leave, "{sig}: entry, call, leave");
        assert!(
            leave < keep && keep < drop_guard,
            "{sig}: the spill outlives the leave"
        );
    }
    assert!(ffm_downcall_in_native_lazy());
}

/// JIT round 20 wave 14 (lane ffm3c; `CRATONVM_FFM_LAZY_DIRECT_LEAN`): the
/// latched verdict answers exactly what the three switches answer, so the
/// lean and the unlatched readers can never disagree on a row.
#[test]
fn the_latched_direct_row_verdict_agrees_with_the_switches() {
    let lazy = direct_row_goes_lazy_by_switches();
    assert_eq!(direct_row_goes_lazy(), lazy);
    assert_eq!(
        direct_row_admits_non_critical(),
        !(ffm_downcall_gc_safe() || ffm_downcall_in_native_lazy()) || lazy
    );
    let verdict = direct_row_lazy_verdict();
    assert_eq!(
        verdict & DIRECT_LAZY_VERDICT_LEAN != 0,
        ffm_lazy_direct_lean()
    );
    assert!(ffm_lazy_direct_lean());
}

/// FFS16-1: with no manager in any VM the verdict is "none", with or
/// without the mirror; with one installed in this VM it is not.
#[test]
fn the_no_security_manager_verdict_matches_the_slot() {
    let _guard = crate::security_manager::security_state_test_lock();
    let ctx = crate::test_utils::MockNativeContext::new();
    ctx.set_vm_identity(0x5f5_1601);
    let _ = crate::security_manager::set_security_manager_for_test(&ctx, None);
    assert!(downcall_no_security_manager(&ctx));
    // SAFETY: never dereferenced.
    let sm = unsafe { ObjectRef::from_raw(0x6000 as *mut u8) };
    let _ = crate::security_manager::set_security_manager_for_test(&ctx, Some(sm));
    assert!(!downcall_no_security_manager(&ctx));
    let _ = crate::security_manager::set_security_manager_for_test(&ctx, None);
    assert!(downcall_no_security_manager(&ctx));
}
