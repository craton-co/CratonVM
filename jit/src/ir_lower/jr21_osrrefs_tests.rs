use super::*;

/// JIT round 21 wave 17 (lane osrrefs, OD16-2): the reference half of the
/// dataflow live-in excuse, rule by rule.
#[test]
fn jr21_osrrefs_a_dead_unnamed_reference_owes_no_osr_seed() {
    let mut g = Graph::empty(8);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let merge = g.add(Op::Merge, IrType::Control, vec![start], Some(4));
    let mem_phi = g.add(Op::Phi, IrType::Memory, vec![merge, start], Some(4));
    let long_phi = g.add(Op::Phi, IrType::Long, vec![merge, start], Some(4));
    let ref_phi = g.add(Op::Phi, IrType::Ref, vec![merge, start], Some(4));
    let ref_value = g.add(Op::Add, IrType::Ref, vec![], Some(9));
    let f = |id: NodeId, own: bool, live_in: fn() -> Option<bool>| {
        Lowerer::osr_entry_ref_needs_no_seed_by(&g.nodes[id as usize], own, live_in)
    };
    let dead: fn() -> Option<bool> = || Some(false);
    let live: fn() -> Option<bool> = || Some(true);
    let unknown: fn() -> Option<bool> = || None;
    assert!(f(ref_phi, false, dead), "an outer loop's dead reference φ");
    assert!(f(ref_value, false, dead), "a dead reference value");
    assert!(
        !f(ref_phi, false, live),
        "a live reference is seeded, not nulled"
    );
    assert!(
        !f(ref_phi, false, unknown),
        "an unconverged model is not a verdict of death"
    );
    assert!(
        !f(ref_phi, true, dead),
        "a φ of the entry block is written by the edge copies the stub skips"
    );
    assert!(
        !f(long_phi, false, dead),
        "primitives are the other switch's"
    );
    assert!(!f(mem_phi, false, dead), "a memory φ is the token rule's");
    // The primitive rule still refuses a reference on its own.
    assert!(!Lowerer::osr_entry_value_needs_no_seed_by(
        &g.nodes[ref_phi as usize],
        false,
        dead,
        false,
        true,
    ));
}

/// Text pin (a stub needs a whole lowering): the nulls are written after
/// the recomputation and before the live seeds, and every excused
/// reference reaches the stub through `OsrRemat::null_refs`.
#[test]
fn jr21_osrrefs_the_stub_nulls_before_the_live_seeds() {
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
    )
    .replace("\r\n", "\n");
    let src = src.as_str();
    // `\x7d`: no brace literal in a test (the brace-counting ratchets).
    let remat = src
        .find("self.emit_osr_remat_insn(insn);\n            \x7d\n\n            // ── Null")
        .expect("the recomputation loop of the ordinary stub");
    let nulls = src
        .find("self.emit_osr_null_ref(id);")
        .expect("the null loop");
    let live = src
        .find("// ── Seed the live locals, last")
        .expect("the live seeds");
    assert!(remat < nulls && nulls < live);
    assert!(src.contains("remat.null_refs = null_refs;"));
}
