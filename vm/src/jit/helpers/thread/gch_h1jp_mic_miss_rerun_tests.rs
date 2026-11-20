use super::{mic_miss_rerun_reads_the_callers_block, ObjectRef, Value};

/// gch h1/jp (fr review item 1): the caller's block is read only while it
/// names the same objects as the re-encoding.
#[test]
fn gch_h1jp_the_mic_miss_rerun_reads_the_callers_block_only_when_it_is_current() {
    // SAFETY: never dereferenced; only its address is compared.
    let obj = unsafe { ObjectRef::from_raw(0x1000usize as *mut u8) };
    let full = [
        Value::Object(Some(obj)),
        Value::Int(-1),
        Value::Object(None),
    ];
    let raw = [0x1000_i64, -1, 0];
    // The caller kept an int's upper bits dirty: still the same objects.
    let caller = [0x1000_i64, 0xffff_ffff, 0];
    assert!(mic_miss_rerun_reads_the_callers_block(&raw, &caller, &full));
    // The probe moved the receiver: the caller's word is pre-move.
    let moved = [0x2000_i64, -1, 0];
    assert!(!mic_miss_rerun_reads_the_callers_block(
        &moved, &caller, &full
    ));
    // A null that is not null in the caller's block.
    let stray = [0x1000_i64, -1, 0x3000];
    assert!(!mic_miss_rerun_reads_the_callers_block(&raw, &stray, &full));
    // Shapes that do not line up are never read across.
    assert!(!mic_miss_rerun_reads_the_callers_block(
        &raw,
        &caller[..2],
        &full
    ));
    assert!(!mic_miss_rerun_reads_the_callers_block(
        &raw[..2],
        &caller[..2],
        &full
    ));
}

/// gch h1/jp (fr review item 2): both compiled tiers root the argument
/// block a dispatch helper is handed, so the declined-stash and frameless
/// re-runs that read it after the callee read current words. Single-pass:
/// every reference staged into the helper's buffer is pushed to
/// `pending_staged_arg_oops`, which the call's safepoint map names (precise,
/// remapped in place). Optimizing tier: the staging region is carved out of
/// the frame (`frame_size - shadow - stack_arg_reserve` down), inside the
/// `[rbp - frame_size, rbp)` band every compiled-frame scan reads, and no
/// liveness claim drops a word the current call staged -- a conservative
/// root, whose object is pinned, not moved. A source pin, so an edit that
/// moves either block out of its root set fails here first.
#[test]
fn gch_h1jp_both_tiers_root_the_dispatch_staging_block() {
    let single = concat!(include_str!("../../../../../jit/src/x64/op_invoke/mod.rs"), "\n", include_str!("../../../../../jit/src/x64/op_invoke/sp_tailcall_keeps_vm_helper_frame.rs"), "\n", include_str!("../../../../../jit/src/x64/op_invoke/sp_byte_array_view_body.rs"), "\n", include_str!("../../../../../jit/src/x64/op_invoke/walk_invoke_instance.rs"), "\n", include_str!("../../../../../jit/src/x64/op_invoke/arraylist_access_prefix.rs"), "\n", include_str!("../../../../../jit/src/x64/op_invoke/indy_site_is_bridged.rs"), "\n", include_str!("../../../../../jit/src/x64/op_invoke/sb_to_string_latin1_body.rs")).replace("\r\n", "\n");
    assert!(
        single.contains("if arg_oops[i] {\n                                self.pending_staged_arg_oops.push(buf_offset);"),
        "single-pass: the helper's args buffer names its references in the map"
    );
    let ir = concat!(include_str!("../../../../../jit/src/ir_lower/mod.rs"), "\n", include_str!("../../../../../jit/src/ir_lower/virtual_object_info.rs"), "\n", include_str!("../../../../../jit/src/ir_lower/mir_plan_and_init.rs"), "\n", include_str!("../../../../../jit/src/ir_lower/vector_preloop.rs"), "\n", include_str!("../../../../../jit/src/ir_lower/value_phi_copies.rs"), "\n", include_str!("../../../../../jit/src/ir_lower/field_proof_entry_is_rechecked_osr.rs"), "\n", include_str!("../../../../../jit/src/ir_lower/int_expr_bounds.rs"), "\n", include_str!("../../../../../jit/src/ir_lower/character_to_chars_prefix.rs"), "\n", include_str!("../../../../../jit/src/ir_lower/getfield_legacy_read.rs"), "\n", include_str!("../../../../../jit/src/ir_lower/post_call_exit_site_for.rs"), "\n", include_str!("../../../../../jit/src/ir_lower/entry_fold_node.rs"), "\n", include_str!("../../../../../jit/src/ir_lower/call_loaded_ic_entry_releasing.rs"), "\n", include_str!("../../../../../jit/src/ir_lower/fallthrough_poll_spills.rs"), "\n", include_str!("../../../../../jit/src/ir_lower/ir_ic_cascade_admits_kind.rs"), "\n", include_str!("../../../../../jit/src/ir_lower/implicit_kind_matches_catch.rs"), "\n", include_str!("../../../../../jit/src/ir_lower/scoped_raw_tail.rs"), "\n", include_str!("../../../../../jit/src/ir_lower/frame_needs.rs"), "\n", include_str!("../../../../../jit/src/ir_lower/back_edge_dead_ref_homes.rs"), "\n", include_str!("../../../../../jit/src/ir_lower/block_lazy_clear_survivors.rs"), "\n", include_str!("../../../../../jit/src/ir_lower/promoted_pending_mark.rs"), "\n", include_str!("../../../../../jit/src/ir_lower/site_value_live_at.rs"), "\n", include_str!("../../../../../jit/src/ir_lower/plan_register_residency_with.rs"), "\n", include_str!("../../../../../jit/src/ir_lower/inner_attempt.rs"), "\n", include_str!("../../../../../jit/src/ir_lower/g1_aastore_screen.rs"), "\n", include_str!("../../../../../jit/src/ir_lower/scoped_session_confined_rows.rs")).replace("\r\n", "\n");
    assert!(
        ir.contains("let args_stage_top_off = frame_size - shadow - stack_arg_reserve;"),
        "optimizing tier: the staging region lies inside the frame"
    );
    assert!(
        ir.contains("The words this call does stage stay covered-and-live"),
        "optimizing tier: the liveness claim keeps the current call's staged words"
    );
    let scan = concat!(include_str!("../../conservative_roots/mod.rs"), "\n", include_str!("../../conservative_roots/jit_frame_chain_entry.rs"), "\n", include_str!("../../conservative_roots/gc_outgoing_arg_roots_enabled.rs"), "\n", include_str!("../../conservative_roots/refresh_moving_young_coverage_for.rs"), "\n", include_str!("../../conservative_roots/warn_cross_thread_jit_gap.rs"), "\n", include_str!("../../conservative_roots/scan_one_frame.rs")).replace("\r\n", "\n");
    assert!(
        scan.contains("bands.push(rbp - frame_size, rbp);"),
        "the compiled-frame scan reads the whole frame band"
    );
}
