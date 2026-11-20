use super::*;

fn body<'a>(src: &'a str, start: &str) -> &'a str {
    let at = src.find(start).unwrap_or_else(|| panic!("{start}"));
    let rest = &src[at..];
    &rest[..rest.find("\n\u{7d}\n").expect("end")]
}

fn pos(hay: &str, needle: &str) -> usize {
    hay.find(needle).unwrap_or_else(|| panic!("{needle}"))
}

#[test]
fn gci_i48_rf48_only_a_g1_full_collection_door_keeps_by_default() {
    let on = || true;
    let unread = || -> bool { panic!("the switch is read last") };
    assert!(g1_full_gc_keeps_undiscovered_rule(
        true,
        GcDoor::SystemGc,
        &on
    ));
    assert!(g1_full_gc_keeps_undiscovered_rule(
        true,
        GcDoor::JvmtiForceGc,
        &on
    ));
    assert!(!g1_full_gc_keeps_undiscovered_rule(
        true,
        GcDoor::SystemGc,
        &|| false
    ));
    assert!(!g1_full_gc_keeps_undiscovered_rule(
        false,
        GcDoor::SystemGc,
        &unread
    ));
    for door in [
        GcDoor::AllocationThreshold,
        GcDoor::AllocationFailure,
        GcDoor::G1CompactionPause,
        GcDoor::G1HumongousAllocation,
    ] {
        assert!(
            !g1_full_gc_keeps_undiscovered_rule(true, door, &unread),
            "{door:?}"
        );
    }
}

/// Full-collection preservation is on independently of the optional
/// all-collector STW probe and legacy carry switch.

/// The door engages the probe through the rule, carries after its round
/// (and after its own G-2 carry), the remark stages after its carry read
/// and before the pre-prune, and the compaction pause is bracketed by the
/// arm and the forget.
#[test]
fn gci_i48_rf48_the_steps_are_wired_in_order() {
    let src = GC_AND_ALLOC_SRC.replace("\r\n", "\n");
    let door = body(&src, "fn run_collection_pause(");
    let probe = pos(door, "stw_probe_engaged(shared, door)");
    let round = pos(door, "    process_references_after_gc(\n");
    let g2 = pos(
        door,
        "g1_carry_door_queued_finalizables(shared, door, &dead_finalizers);",
    );
    let carry = pos(
        door,
        "g1_carry_door_undiscovered_refs(shared, door, &result.pointer_map);",
    );
    assert!(
        probe < round && round < g2 && g2 < carry,
        "{probe} {round} {g2} {carry}"
    );
    let rows = body(&src, "fn remark_reference_rows(");
    let read = pos(
        rows,
        "ref_proc.carried_undiscovered_for(shared.mem.heap.collection_count",
    );
    let stage = pos(rows, "ref_proc.stage_full_gc_undiscovered(&refs,");
    let prune = pos(
        rows,
        "ref_proc.remove_collected_reference_objects(&reference_is_live)",
    );
    assert!(read < stage && stage < prune, "{read} {stage} {prune}");
    let pause = body(&src, "fn g1_full_compaction_pause(");
    let arm = pos(pause, "g1_full_gc_arm_undiscovered(shared);");
    let run = pos(pause, "maybe_gc_forced_collected_on(");
    let forget = pos(pause, "g1_full_gc_forget_undiscovered(shared);");
    assert!(arm < run && run < forget, "{arm} {run} {forget}");
    let carry_fn = body(&src, "fn g1_carry_door_undiscovered_refs(");
    assert!(carry_fn.contains("g1_full_gc_keeps_undiscovered(shared, door)"));
    assert!(
        !carry_fn.contains("g1_force_full_cycle_is_stw"),
        "concurrent requests carry too"
    );
    let full = body(&src, "fn force_full_collection(");
    let compaction = pos(full, "if !user_concurrent {");
    let forget = pos(full, "g1_full_gc_forget_undiscovered(shared);");
    assert!(
        compaction < forget,
        "a concurrent request must drop its carry without compaction"
    );
}

/// The pre-collection pass takes the armed set under the processor lock,
/// notes it undiscovered and drops its rows from both null lists, before
/// the null loop writes any slot.
#[test]
fn gci_i48_rf48_the_pre_collection_pass_spares_the_armed_rows() {
    let interp = concat!(
        include_str!("../../interpreter/mod.rs"),
        "\n",
        include_str!("../../interpreter/cached_invoke_target.rs"),
        "\n",
        include_str!("../../interpreter/execute.rs"),
        "\n",
        include_str!("../../interpreter/resume_continuation.rs"),
        "\n",
        include_str!("../../interpreter/execute_frame_from_index_loop.rs"),
        "\n",
        include_str!("../../interpreter/resolve_array_class_loader_aware.rs"),
        "\n",
        include_str!("../../interpreter/park_for_debugger_under.rs"),
        "\n",
        include_str!("../../interpreter/double_arith_apply.rs")
    )
    .replace("\r\n", "\n");
    let pre = body(&interp, "\npub fn weakref_null_referents_pre_gc(");
    let stamp = pos(
        pre,
        "let full_gc_stamp = shared.mem.heap.collection_count();",
    );
    let lock = pos(pre, "let mut rp = shared.mem.ref_processor.lock();");
    let take = pos(pre, "rp.take_armed_full_gc_undiscovered(full_gc_stamp);");
    let note = pos(pre, "rp.note_undiscovered_references(&held);");
    let weak = pos(
        pre,
        "weak_phantom.retain(|&(r, _, _)| held.binary_search(&r).is_err());",
    );
    let soft = pos(pre, ".filter(|&(r, _)| held.binary_search(&r).is_err())");
    let null_loop = pos(
        pre,
        "for (row_index, (ref_obj_addr, row_referent, stamp)) in",
    );
    assert!(stamp < lock && lock < take && take < note && note < weak);
    assert!(weak < soft && soft < null_loop);
}

/// zt t7/cc (`CRATONVM_ZGC_GEN_YOUNG_SKIP_REFERENCES`): the young-cycle
/// arm of the pre-collection pass is decided before the processor lock,
/// takes no clear-all-soft request and condemns no soft reference under
/// it, and skips only the null write itself -- the examined / handled
/// bookkeeping (`stamp_referent_class`) still runs, so an application-
/// cleared row is retired rather than enqueued. The helper only READS a
/// pending request, and declines while one is pending.
#[test]
fn zt7cc_the_young_cycle_arm_skips_only_the_condemn_and_the_null_write() {
    let interp = concat!(
        include_str!("../../interpreter/mod.rs"),
        "\n",
        include_str!("../../interpreter/cached_invoke_target.rs"),
        "\n",
        include_str!("../../interpreter/execute.rs"),
        "\n",
        include_str!("../../interpreter/resume_continuation.rs"),
        "\n",
        include_str!("../../interpreter/execute_frame_from_index_loop.rs"),
        "\n",
        include_str!("../../interpreter/resolve_array_class_loader_aware.rs"),
        "\n",
        include_str!("../../interpreter/park_for_debugger_under.rs"),
        "\n",
        include_str!("../../interpreter/double_arith_apply.rs")
    )
    .replace("\r\n", "\n");
    let pre = body(&interp, "\npub fn weakref_null_referents_pre_gc(");
    let opt_out = pos(pre, "if !weakref_clear_enabled() || no_refproc() \u{7b}");
    let arm = pos(
        pre,
        "let keep_referents = zgc_young_cycle_keeps_references(shared);",
    );
    let lock = pos(pre, "let mut rp = shared.mem.ref_processor.lock();");
    assert!(opt_out < arm && arm < lock);
    let take_gate = pos(
        pre,
        "let clear_all = if keep_referents \u{7b}\n            false", // literal-continuation-ok: source witness
    );
    let take = pos(pre, "rp.take_clear_all_soft_request()");
    let condemn_gate = pos(pre, "\u{7d} else if keep_referents \u{7b}");
    let condemn = pos(pre, "rp.condemn_idle_soft_refs(soft_free_mb, now_ms())");
    assert!(take_gate < take && condemn_gate < condemn);
    let null_gate = pos(pre, "if !keep_referents \u{7b}");
    let null_write = pos(
        pre,
        ".set_field_suppress_satb(ref_obj, 0, Value::Object(None));",
    );
    let handled = pos(pre, "rp.stamp_referent_class(ref_obj_addr, class_id);");
    assert!(null_gate < null_write && null_write < handled);
    let helper = body(&interp, "fn zgc_young_cycle_keeps_references(");
    assert!(helper.contains("clear_all_soft_requested()"));
    assert!(!helper.contains("take_clear_all_soft_request"));
    assert!(helper.contains("zgc_young_skip_references_on()"));
}
