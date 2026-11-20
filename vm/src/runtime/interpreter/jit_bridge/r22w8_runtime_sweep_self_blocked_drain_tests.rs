#[test]
fn r22w8_the_sweep_driver_drains_inside_its_own_blocked_window() {
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
    );
    let body = src
        .split("fn maybe_sweep_cold_jit_bodies(")
        .nth(1)
        .expect("the driver");
    let body = &body[..body.find("\n}\n").expect("end")];
    let sweep = body
        .find(".sweep_cold_bodies(progress)")
        .expect("the sweep");
    let gate = body
        .find("current_thread_jit_chain_is_readable()")
        .expect("the readable-stack guard");
    let enter = body
        .find("note_blocking_transition_enter()")
        .expect("the window");
    let drain = body.find("reclaim_retired_jit_code()").expect("the drain");
    let leave = body
        .find("note_blocking_transition_leave()")
        .expect("the close");
    let pressure = body
        .rfind("note_code_cache_pressure(")
        .expect("the re-report");
    assert!(sweep < gate && gate < enter && enter < drain && drain < leave && leave < pressure);
}
