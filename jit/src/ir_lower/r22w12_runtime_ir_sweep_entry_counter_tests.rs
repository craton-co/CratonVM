#[test]
fn r22w12_the_ir_entry_counter_is_gated_on_the_armed_sweeper_and_kept_by_the_body() {
    let src = concat!(
        include_str!("../ir_lower/mod.rs"),
        "\n",
        include_str!("../ir_lower/virtual_object_info.rs"),
        "\n",
        include_str!("../ir_lower/mir_plan_and_init.rs"),
        "\n",
        include_str!("../ir_lower/vector_preloop.rs"),
        "\n",
        include_str!("../ir_lower/value_phi_copies.rs"),
        "\n",
        include_str!("../ir_lower/field_proof_entry_is_rechecked_osr.rs"),
        "\n",
        include_str!("../ir_lower/int_expr_bounds.rs"),
        "\n",
        include_str!("../ir_lower/character_to_chars_prefix.rs"),
        "\n",
        include_str!("../ir_lower/getfield_legacy_read.rs"),
        "\n",
        include_str!("../ir_lower/post_call_exit_site_for.rs"),
        "\n",
        include_str!("../ir_lower/entry_fold_node.rs"),
        "\n",
        include_str!("../ir_lower/call_loaded_ic_entry_releasing.rs"),
        "\n",
        include_str!("../ir_lower/fallthrough_poll_spills.rs"),
        "\n",
        include_str!("../ir_lower/ir_ic_cascade_admits_kind.rs"),
        "\n",
        include_str!("../ir_lower/implicit_kind_matches_catch.rs"),
        "\n",
        include_str!("../ir_lower/scoped_raw_tail.rs"),
        "\n",
        include_str!("../ir_lower/frame_needs.rs"),
        "\n",
        include_str!("../ir_lower/back_edge_dead_ref_homes.rs"),
        "\n",
        include_str!("../ir_lower/block_lazy_clear_survivors.rs"),
        "\n",
        include_str!("../ir_lower/promoted_pending_mark.rs"),
        "\n",
        include_str!("../ir_lower/site_value_live_at.rs"),
        "\n",
        include_str!("../ir_lower/plan_register_residency_with.rs"),
        "\n",
        include_str!("../ir_lower/inner_attempt.rs"),
        "\n",
        include_str!("../ir_lower/g1_aastore_screen.rs"),
        "\n",
        include_str!("../ir_lower/scoped_session_confined_rows.rs")
    );
    let body = src
        .split("fn emit_sweep_entry_counter(")
        .nth(1)
        .expect("the emitter");
    let body = &body[..body.find("\n    }\n").expect("end")];
    let gate = body
        .find("crate::code_cache_sweep_enabled()")
        .expect("the sweeper gate");
    let emit = body
        .find("self.buf.emit(&[0x49, 0xBA])")
        .expect("MOV R10, imm64");
    let add = body
        .find("[0x49, 0x83, 0x42, disp, 0x01]")
        .expect("ADD qword [R10+0], 1");
    assert!(gate < emit && emit < add);
    let lower = src
        .split("fn lower_inner_attempt(")
        .nth(1)
        .expect("the lowering");
    let pad = lower
        .find("lowerer.buf.emit(&crate::ENTRY_PATCH_PAD);")
        .expect("the pad");
    let counter = lower
        .find("lowerer.emit_sweep_entry_counter()")
        .expect("the counter");
    let fast = lower
        .find("lowerer.emit_entry_fast_return();")
        .expect("the fast return");
    let kept = lower
        .find("cm._jit_entry_counter = sweep_entry_counter;")
        .expect("kept");
    assert!(pad < counter && counter < fast && fast < kept);
}
