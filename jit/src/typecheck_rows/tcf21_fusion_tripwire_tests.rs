use super::*;

fn lower_source() -> String {
    concat!(include_str!("../ir_lower/mod.rs"), "\n", include_str!("../ir_lower/virtual_object_info.rs"), "\n", include_str!("../ir_lower/mir_plan_and_init.rs"), "\n", include_str!("../ir_lower/vector_preloop.rs"), "\n", include_str!("../ir_lower/value_phi_copies.rs"), "\n", include_str!("../ir_lower/field_proof_entry_is_rechecked_osr.rs"), "\n", include_str!("../ir_lower/int_expr_bounds.rs"), "\n", include_str!("../ir_lower/character_to_chars_prefix.rs"), "\n", include_str!("../ir_lower/getfield_legacy_read.rs"), "\n", include_str!("../ir_lower/post_call_exit_site_for.rs"), "\n", include_str!("../ir_lower/entry_fold_node.rs"), "\n", include_str!("../ir_lower/call_loaded_ic_entry_releasing.rs"), "\n", include_str!("../ir_lower/fallthrough_poll_spills.rs"), "\n", include_str!("../ir_lower/ir_ic_cascade_admits_kind.rs"), "\n", include_str!("../ir_lower/implicit_kind_matches_catch.rs"), "\n", include_str!("../ir_lower/scoped_raw_tail.rs"), "\n", include_str!("../ir_lower/frame_needs.rs"), "\n", include_str!("../ir_lower/back_edge_dead_ref_homes.rs"), "\n", include_str!("../ir_lower/block_lazy_clear_survivors.rs"), "\n", include_str!("../ir_lower/promoted_pending_mark.rs"), "\n", include_str!("../ir_lower/site_value_live_at.rs"), "\n", include_str!("../ir_lower/plan_register_residency_with.rs"), "\n", include_str!("../ir_lower/inner_attempt.rs"), "\n", include_str!("../ir_lower/g1_aastore_screen.rs"), "\n", include_str!("../ir_lower/scoped_session_confined_rows.rs")).replace("\r\n", "\n")
}



#[test]
fn tcf21_the_fusion_admission_keeps_its_screens() {
    let src = lower_source();
    let start = src
        .find("    pub(in crate::ir_lower) fn typecheck_branch_fusion_for(")
        .expect("the admission");
    let body = &src[start..];
    let body = &body[..body.find("\n    }\n").expect("its end")];
    for needle in [
        "typecheck_branch_fusion_enabled()",
        "self.fused_branch_on",
        "self.mir_mode != MirMode::Off",
        "self.ref_residency_on",
        "self.ls_caller_saved_on",
        "self.fence_pending",
        "!self.ls_block_start.is_empty()",
        "self.typecheck_branch_pending.is_some()",
        "self.dead_home_clears.contains_key(&id)",
        "is_memory_token_slot",
        "block.nodes.len() != pos + 2",
        "self.use_count.get(cond as usize).copied() != Some(1)",
        "self.lazy_ref_sites.contains_key(&cond)",
        "Op::Cmp(CmpOp::Ne) => true",
        "Op::Cmp(CmpOp::Eq) => false",
        "block.successors[0] == block.successors[1]",
        "block.successors.iter().any(|&s| s <= block_idx)",
    ] {
        assert!(
            body.contains(needle),
            "typecheck_branch_fusion_for lost `{needle}`: re-argue the fusion's soundness \
             (docs/feature-designs/jit-ir-optimizations.md) before dropping it"
        );
    }
}

#[test]
fn tcf21_every_two_edge_if_layout_resolves_the_parked_answers() {
    let src = lower_source();
    assert_eq!(
        src.matches("tc_edges = Some(").count(),
        3,
        "the three two-successor layouts of Op::If (no phi copies, one copy edge, general)"
    );
    assert_eq!(
        src.matches("self.typecheck_branch_fusion_for(id)").count(),
        1
    );
    assert_eq!(
        src.matches("self.typecheck_branch_pending.take()").count(),
        2,
        "the If arm resolves it; lower_block refuses a leftover"
    );
    assert_eq!(
        src.matches("a fused instanceof's answers reached no If edge")
            .count(),
        1
    );
    assert_eq!(
        src.matches("self.emit_typecheck_row_first_arm_from_word(")
            .count(),
        2
    );
}

#[test]
fn tcf21_the_shared_word_pass_verifier_arity_and_builder_stay_wired() {
    let opt = concat!(include_str!("../ir_optimize/mod.rs"), "\n", include_str!("../ir_optimize/optimize_context.rs"), "\n", include_str!("../ir_optimize/apply_loop_block_coarsening_in_place.rs"), "\n", include_str!("../ir_optimize/redundant_loads_pass.rs"), "\n", include_str!("../ir_optimize/plant_null_base_load_predicates.rs"), "\n", include_str!("../ir_optimize/unroll_pure_body_enabled.rs")).replace("\r\n", "\n");
    assert!(opt.contains("typecheck_class_word::share_typecheck_class_words(graph);"));
    let verify = include_str!("../ir_verify.rs").replace("\r\n", "\n");
    assert!(verify.contains("Op::InstanceOf { .. } | Op::CheckCast { .. } => (3, 4),"));
    let lower = lower_source();
    assert!(lower.contains("typecheck_row_first_from_word_bytes(rows, cid, kind, latch)"));
    assert!(
        !lower.contains("arm.bytes[2..]"),
        "the builder owns the two dropped bytes"
    );
}
