#[test]
fn the_osr_offer_keeps_a_barred_finalizable_constructor_interpreted() {
    let src = concat!(
        include_str!("../interpreter/mod.rs"),
        "\n",
        include_str!("../interpreter/cached_invoke_target.rs"),
        "\n",
        include_str!("../interpreter/execute.rs"),
        "\n",
        include_str!("../interpreter/resume_continuation.rs"),
        "\n",
        include_str!("../interpreter/execute_frame_from_index_loop.rs"),
        "\n",
        include_str!("../interpreter/resolve_array_class_loader_aware.rs"),
        "\n",
        include_str!("../interpreter/park_for_debugger_under.rs"),
        "\n",
        include_str!("../interpreter/double_arith_apply.rs")
    )
    .replace("\r\n", "\n");
    let body = src
        .split("\npub(in crate::runtime::interpreter) fn try_osr_offer(")
        .nth(1)
        .and_then(|rest| rest.split("\n\u{7d}\n").next())
        .expect("try_osr_offer");
    let agent = body
        .find("if jvmti_requires_interpreter_for_method(")
        .expect("the agent screen");
    let bar = body
        .find("let osr_ctor_barred = finalizer_bars_compiling(")
        .expect("the finalizer screen");
    let budget = body
        .find(".osr_attempts_at(entry_pc)")
        .expect("the method-wide budget");
    assert!(agent < bar && bar < budget);
    let arm = &body[bar..budget];
    let retire = arm
        .find(".set_osr_attempts(entry_pc, crate::runtime::frame::OSR_MAX_ATTEMPTS);")
        .expect("the header is retired for the activation");
    let skip = arm.find("return OsrBackoffOutcome::Skip;").expect("the refusal");
    assert!(retire < skip);
}
