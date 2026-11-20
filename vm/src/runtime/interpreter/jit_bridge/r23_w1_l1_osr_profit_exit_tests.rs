fn production_fn(sig: &str) -> String {
    let whole = concat!(
        include_str!("../jit_bridge/mod.rs"),
        "\n",
        include_str!("../jit_bridge/jit_verdicts.rs"),
        "\n",
        include_str!("../jit_bridge/osr_continuation.rs"),
        "\n",
        include_str!("../jit_bridge/compile_osr_body.rs"),
        "\n",
        include_str!("../jit_bridge/osr_exception_exit.rs"),
        "\n",
        include_str!("../jit_bridge/try_osr.rs"),
        "\n",
        include_str!("../jit_bridge/native_shadow_arms.rs"),
        "\n",
        include_str!("../jit_bridge/compile_optimizing_artifact.rs"),
        "\n",
        include_str!("../jit_bridge/never_run_static_exit_pcs.rs"),
        "\n",
        include_str!("../jit_bridge/try_jit_compile_callee_slow.rs"),
        "\n",
        include_str!("../jit_bridge/byte_array_field_declarer_admitted.rs"),
        "\n",
        include_str!("../jit_bridge/folded_handle_member_row.rs"),
        "\n",
        include_str!("../jit_bridge/callee_never_taken_rows.rs"),
        "\n",
        include_str!("../jit_bridge/mint_ldc_slots_at_offer.rs")
    )
    .replace("\r\n", "\n");
    let src = &whole;
    let start = src.find(sig);
    assert!(start.is_some(), "`{sig}` not found");
    let start = start.unwrap_or(0);
    let body = &src[start..];
    let flat: String = body[..body.find("\n\u{7d}\n").map_or(body.len(), |e| e + 3)]
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .collect();
    flat
}

/// The build arms after its fan-out remembered the body under every
/// header; the arming asks the door's own cache question at every header
/// before it forces anything.
#[test]
fn r23_l1_the_optimizing_build_arms_after_it_remembers() {
    let build = production_fn("fn build_osr_optimizing_artifact(");
    let fan_out = build.rfind("remember_osr_optimizing_artifact(");
    let arm = build.find("arm_single_pass_osr_profit_exit(");
    assert!(fan_out.is_some() && arm.is_some());
    assert!(fan_out < arm, "armed once every header is remembered");
    let arming = production_fn("fn arm_single_pass_osr_profit_exit(");
    let enabled = arming.find("osr_profit_exit_enabled()");
    let every = arming.find("osr_profit_exit_headers(code)");
    let asked = arming.find("!osr_optimizing_cached_peek(shared,class_id,names,key,pc,code)");
    // Review item 3: the arming's question has no side effects.
    let peek = production_fn("fn osr_optimizing_cached_peek(");
    assert!(!peek.contains("late_bind_osr_optimizing_stale_taken("));
    assert!(!peek.contains(".remove("));
    assert!(!peek.contains(".store("));
    assert!(!peek.contains("get_mut("));
    assert!(peek.contains("entry.poly_decline_unasked"));
    let force = arming.find(".arm_osr_profit_exit(&single_pass)");
    assert!(enabled.is_some() && every.is_some() && asked.is_some() && force.is_some());
    assert!(enabled < every && every < asked && asked < force);
    assert!(arming.contains("single_pass.used_ir_backend"));
}

/// An armed body is never re-entered: not from the re-entry memo, not
/// reused by the single-pass door; its `OsrExit` is a committed exit.
#[test]
fn r23_l1_the_door_never_reenters_an_armed_body_and_commits_its_exit() {
    let door = production_fn("fn try_osr(");
    assert!(door.contains("letmemo_hit=memo_hit.filter(|hit|{!hit.osr_profit_exit_armed()"));
    assert!(door.contains(
        "letprofit_exit=!charged&&!jvmti_left&&osr_profit_exit_left(shared,&compiled,cause);"
    ));
    assert!(door.contains("||profit_exit||"));
    let reuse = production_fn("fn compile_osr_artifact(");
    assert!(reuse.contains("letosr_reused=!despec_stale&&!profit_armed&&"));
    let left = production_fn("fn osr_profit_exit_left(");
    assert!(left.contains("DeoptReason::OsrExit"));
    assert!(left.contains("osr_profit_exit_may_leave(compiled)"));
}
