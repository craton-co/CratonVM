fn production() -> String {
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
    whole
}

fn production_fn(sig: &str) -> String {
    let src = production();
    let start = src.find(sig);
    assert!(start.is_some(), "`{sig}` not found");
    let body = &src[start.unwrap_or(0)..];
    // `\u{7d}` is `}` (see `i43_l2_compile_inputs_tests::production_fn`).
    body[..body.find("\n\u{7d}\n").map_or(body.len(), |e| e + 3)].to_string()
}

fn flat(s: &str) -> String {
    s.chars().filter(|c| !c.is_ascii_whitespace()).collect()
}

/// The cell is planned only behind the opt-in switch, only for a
/// compile-ORDER refusal of an `invokestatic`, only through the admission
/// helper, and the site's `JitInvokeInfo` (the cell's dispatch fallback)
/// is still pushed after it; the artifact owns the cell.
#[test]
fn the_osr_door_plans_a_late_cell_only_behind_its_gates() {
    let osr = flat(&production_fn("fn compile_osr_body("));
    let candidate = osr.find(
        "osr_late_cell_candidate=matches!(reason,cratonvm_jit::DirectBindRefusal::CalleeNotYetCompiled);",
    );
    let gate =
        osr.find("ifosr_late_cell_candidate&&site_invoke_kind==3&&code.get(ipc)==Some(&0xb8)");
    let switch = osr.find("&&cratonvm_jit::jit_osr_late_static_cell_enabled()");
    let admit = osr.find("osr_late_static_cell_target(shared,class_id,");
    let row = osr.find("guard_class_id:cratonvm_jit::JIT_CYCLE_EDGE_CELL_GUARD,");
    let owned = osr.find("osr_retire_cells.push(cell);");
    assert!(
        candidate.is_some() && candidate < gate,
        "candidate {candidate:?} gate {gate:?}"
    );
    assert!(gate < switch && switch < admit && admit < row && row < owned);
    let info_after = owned.and_then(|o| osr[o..].find("push_osr_invoke_info("));
    assert!(
        info_after.is_some(),
        "the dispatch fallback's info follows the cell"
    );
    assert!(osr.contains("cm._jit_cycle_cells.extend(osr_retire_cells);"));
}

/// The admission is the method-entry doors' plus the OSR door's own
/// refusal, a callee the site's class declares itself, and the cycle
/// verdict.
#[test]
fn the_late_cell_admission_asks_every_gate() {
    let f = flat(&production_fn("fn osr_late_static_cell_target("));
    assert!(f.contains("letadmitted=cycle_edge_target_admitted(shared,caller_class_id,"));
    assert!(f.contains("if!admitted||osr_callee_bars_direct_call("));
    assert!(f.contains("ifdeclaring_id!=callee_cid{returnNone;"));
    assert!(f.contains("direct_call_requires_dispatch_of("));
    assert!(f.contains("(!in_cycle).then_some(callee_cid)"));
}

/// t1 wave 3, lane X: the optimizing OSR body's cells are registered on
/// the default path (`CRATONVM_JIT_IR_OSR_CELL_REGISTER`, default ON),
/// before the body is remembered (so before anything can enter it).
#[test]
fn t1x_the_ir_osr_body_registers_its_cells_by_default() {
    let build = flat(&production_fn("fn build_osr_optimizing_artifact("));
    let reg = build.find(
        "ifcratonvm_jit::jit_ir_osr_cell_registration_enabled()\
         {shared.jit.jit_cache.register_unpublished_body_late_cells(&artifact);",
    );
    let remember = build.find("remember_osr_optimizing_artifact(");
    assert!(
        reg.is_some() && reg < remember,
        "reg {reg:?} remember {remember:?}"
    );
    assert!(!build.contains("ifcratonvm_jit::jit_osr_late_static_cell_enabled(){shared.jit"));
}

/// t1 wave 3, lane X: the single-pass OSR door marks every raw static
/// self-call, vouches for it in the request, and stages the self-call
/// service's info on its own channel (never in `invoke_info`).
#[test]
fn t1x_the_osr_door_marks_its_static_self_calls() {
    let osr = flat(&production_fn("fn compile_osr_body("));
    let arm = osr.find(
        "ifinvoke_kind==3&&is_recursive_call{direct_calls2.push((pc,\
         cratonvm_jit::static_self_call_marker(param_count,",
    );
    assert!(arm.is_some(), "the self-call arm registers its marker");
    let staged = arm.and_then(|a| osr[a..].find("&mutstaged,"));
    let cont = arm.and_then(|a| osr[a..].find("continue;"));
    assert!(
        staged.is_some() && staged < cont,
        "the info is staged before the arm leaves"
    );
    assert!(osr
        .contains("static_self_calls_registered:cratonvm_jit::static_self_call_marker_enabled(),"));
    assert!(osr.contains("self_call_invoke_info:osr_self_call_invoke_info,"));
    assert!(osr.contains("ifosr_self_call_invoke_info==0"));
}

/// The eager first-call door stages the same info for its raw static
/// self-calls (page `t1-X-eager-door-self-call-has-no-callee-deopt-service`).
#[test]
fn the_eager_door_stages_its_self_call_service_info() {
    let interp = flat(
        &concat!(
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
        .replace("\r\n", "\n"),
    );
    let arm = interp.find("direct_calls_early.push((pc,cratonvm_jit::static_self_call_marker(");
    assert!(
        arm.is_some(),
        "the eager self-call arm registers its marker"
    );
    let staged = arm.and_then(|a| interp[a..].find("eager_self_call_invoke_info=&*infoas*const"));
    let cont = arm.and_then(|a| interp[a..].find("continue;"));
    assert!(
        staged.is_some() && staged < cont,
        "the info is staged before the arm leaves"
    );
    assert!(interp.contains("self_call_invoke_info:eager_self_call_invoke_info,"));
}
