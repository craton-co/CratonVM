// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 1 (lane TYPECHK): text pins and switch tests for the referent
//! changes of page
//! `weakref-get-and-refersto-read-a-compact-referent-through-jit-getfield-FIXED-20261029.md`.
//! An emitted referent read needs a whole lowering with a wired SATB gate, so
//! the shape is pinned on the source, as `jr21_tl3_referent_gate_tests` does.

/// `CRATONVM_JIT_IR_REFERENT_SPLICE_COMPACT_ROW` is default ON, `=0` kills,
/// and an unwired table names no row.
/// The proven-receiver form (no SATB pre-gate test after its read) is never
/// taken by a handed-out referent read.
#[test]
fn j27_a_referent_read_never_takes_the_proven_receiver_form() {
    let src = concat!(
        include_str!("ir_lower/mod.rs"),
        "\n",
        include_str!("ir_lower/virtual_object_info.rs"),
        "\n",
        include_str!("ir_lower/mir_plan_and_init.rs"),
        "\n",
        include_str!("ir_lower/vector_preloop.rs"),
        "\n",
        include_str!("ir_lower/value_phi_copies.rs"),
        "\n",
        include_str!("ir_lower/field_proof_entry_is_rechecked_osr.rs"),
        "\n",
        include_str!("ir_lower/int_expr_bounds.rs"),
        "\n",
        include_str!("ir_lower/character_to_chars_prefix.rs"),
        "\n",
        include_str!("ir_lower/getfield_legacy_read.rs"),
        "\n",
        include_str!("ir_lower/post_call_exit_site_for.rs"),
        "\n",
        include_str!("ir_lower/entry_fold_node.rs"),
        "\n",
        include_str!("ir_lower/call_loaded_ic_entry_releasing.rs"),
        "\n",
        include_str!("ir_lower/fallthrough_poll_spills.rs"),
        "\n",
        include_str!("ir_lower/ir_ic_cascade_admits_kind.rs"),
        "\n",
        include_str!("ir_lower/implicit_kind_matches_catch.rs"),
        "\n",
        include_str!("ir_lower/scoped_raw_tail.rs"),
        "\n",
        include_str!("ir_lower/frame_needs.rs"),
        "\n",
        include_str!("ir_lower/back_edge_dead_ref_homes.rs"),
        "\n",
        include_str!("ir_lower/block_lazy_clear_survivors.rs"),
        "\n",
        include_str!("ir_lower/promoted_pending_mark.rs"),
        "\n",
        include_str!("ir_lower/site_value_live_at.rs"),
        "\n",
        include_str!("ir_lower/plan_register_residency_with.rs"),
        "\n",
        include_str!("ir_lower/inner_attempt.rs"),
        "\n",
        include_str!("ir_lower/g1_aastore_screen.rs"),
        "\n",
        include_str!("ir_lower/scoped_session_confined_rows.rs")
    )
    .replace("\r\n", "\n");
    let start = src
        .find("fn emit_inline_compact_getfield(")
        .expect("the inline getfield arm");
    let body = &src[start..];
    let body = &body[..body.find("\n    \x7d\n").expect("its end")];
    let gate = body
        .find("if self.proven_fields_on && !referent_gate")
        .expect("the proven form excludes referent reads");
    let proven = body
        .find("self.emit_proven_receiver_getfield(")
        .expect("the proven form");
    assert!(gate < proven);
}

/// The builder gives a spliced referent read (`referent_load_pcs`, slot 0,
/// a reference) the spliced row, and the compile door sets it.
#[test]
fn j27_the_spliced_referent_read_carries_the_row() {
    let ir = concat!(
        include_str!("ir/mod.rs"),
        "\n",
        include_str!("ir/node_id.rs"),
        "\n",
        include_str!("ir/safepoints.rs"),
        "\n",
        include_str!("ir/loop_phis.rs"),
        "\n",
        include_str!("ir/set_osr_requested_header.rs"),
        "\n",
        include_str!("ir/splice_op_cannot_raise.rs"),
        "\n",
        include_str!("ir/narrow_int_return.rs"),
        "\n",
        include_str!("ir/is_jdk_throwable_init_candidate.rs"),
        "\n",
        include_str!("ir/attach_sb_virtual_builders.rs"),
        "\n",
        include_str!("ir/field_load_memo_mode.rs"),
        "\n",
        include_str!("ir/ir_splice_store_fence_forward_branch.rs"),
        "\n",
        include_str!("ir/ir_trap_census.rs")
    )
    .replace("\r\n", "\n");
    assert!(ir.contains("self.note_spliced_referent_row(load);"));
    let lib = include_str!("lib_parts/ir_tier_attempt.rs").replace("\r\n", "\n");
    assert!(lib.contains(
        "builder.set_spliced_referent_row(ir::spliced_referent_row_from_table(direct_helpers));"
    ));
}
