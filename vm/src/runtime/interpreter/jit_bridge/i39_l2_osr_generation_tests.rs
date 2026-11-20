use super::stamps_straddle_a_redefinition;

/// A class never redefined: any two stamps name its one generation.
#[test]
fn a_class_never_redefined_has_one_generation() {
    assert!(!stamps_straddle_a_redefinition(0, None));
    assert!(!stamps_straddle_a_redefinition(7, None));
}

/// The older side at or after the class's last redefinition: both read
/// its current pool. Before it: one side reads the replaced pool, bytes
/// equal or not.
#[test]
fn the_older_side_must_not_predate_the_last_redefinition() {
    assert!(
        !stamps_straddle_a_redefinition(12, Some(12)),
        "moved onto it"
    );
    assert!(
        !stamps_straddle_a_redefinition(13, Some(12)),
        "built after it"
    );
    assert!(
        stamps_straddle_a_redefinition(0, Some(12)),
        "an obsolete activation"
    );
    assert!(
        stamps_straddle_a_redefinition(11, Some(12)),
        "built while the swap was under way"
    );
}

/// The entry asks the rule on the body it is about to enter, after every
/// route produced it and before the body runs (a text pin: `try_osr`
/// needs a live frame, a loaded class and a backend).
#[test]
fn try_osr_asks_the_generation_rule_before_entering() {
    let src = concat!(
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
    let production = src.as_str();
    let body = &production[production
        .find("\npub(in crate::runtime::interpreter) fn try_osr(")
        .expect("try_osr exists")..];
    let body = &body[..body.find("\n\u{7d}\n").map_or(body.len(), |e| e + 3)];
    let rule = body
        .find("osr_body_generation_differs_from_frame(shared, &thread.frames[frame_idx], &compiled)")
        .expect("try_osr asks the generation rule");
    let chosen = body
        .find("let compiled = match ir_osr {")
        .expect("the body is chosen");
    let enter = body
        .find("set_jit_thread(thread)")
        .expect("the body is entered");
    assert!(chosen < rule && rule < enter);
}
