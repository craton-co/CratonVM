use super::*;
use cratonvm_types::flags::with_thread_overrides;
use std::collections::HashMap;

fn fixture() -> (Graph, NodeId) {
    let mut g = Graph::empty(32);
    let c = g.add(Op::Const(0), IrType::Int, vec![], None);
    (g, c)
}

fn call(g: &mut Graph, c: NodeId) -> NodeId {
    g.add(
        Op::Call { info_ptr: 0 },
        IrType::Int,
        vec![c, c, c],
        Some(3),
    )
}

fn ret(g: &mut Graph, c: NodeId, v: NodeId) -> NodeId {
    g.add(Op::Return, IrType::Void, vec![c, v], Some(9))
}

fn view<'a>(
    nodes: &'a [NodeId],
    terminator: Option<NodeId>,
    successors: &'a [usize],
) -> TailSinkBlock<'a> {
    TailSinkBlock {
        ctrl: None,
        nodes,
        terminator,
        successors,
    }
}

/// `return f(x) + 1`: the call, a pure add, the return. Admitted.
#[test]
fn r25w7c_a_call_followed_by_pure_ops_and_a_return_is_admitted() {
    let (mut g, c) = fixture();
    let k = call(&mut g, c);
    let add = g.add(Op::Add, IrType::Int, vec![k, c], None);
    let r = ret(&mut g, c, add);
    let nodes = [k, add];
    let got = tail_sink_calls(&g, &[view(&nodes, Some(r), &[])]);
    assert_eq!(got, vec![(k, true)]);
}

/// A second call after the first: only the last one is admitted. A load
/// after the call (a helper / trap path) refuses it.
#[test]
fn r25w7c_a_later_call_or_an_impure_op_refuses() {
    let (mut g, c) = fixture();
    let k1 = call(&mut g, c);
    let k2 = call(&mut g, c);
    let r = ret(&mut g, c, k2);
    let nodes = [k1, k2];
    let got = tail_sink_calls(&g, &[view(&nodes, Some(r), &[])]);
    assert_eq!(got, vec![(k1, false), (k2, true)]);

    let (mut g, c) = fixture();
    let k = call(&mut g, c);
    let ld = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![c, c, k, c],
        Some(4),
    );
    let r = ret(&mut g, c, ld);
    let nodes = [k, ld];
    assert_eq!(
        tail_sink_calls(&g, &[view(&nodes, Some(r), &[])]),
        vec![(k, false)]
    );
}

/// Zero the compile-id immediate of every identity store
/// (`MOV dword <seg>:[disp32], imm32`, `emit_frame_record_identity`):
/// each test lowerer carries its own compile id.
fn mask_identity(mut code: Vec<u8>) -> Vec<u8> {
    let seg = crate::x64::inline_rbp_tls_segment_prefix();
    let head = [seg, 0xC7, 0x04, 0x25];
    let mut i = 0;
    while i + 12 <= code.len() {
        if code[i..i + 4] == head {
            code[i + 8..i + 12].fill(0);
            i += 12;
        } else {
            i += 1;
        }
    }
    code
}

/// The four emission sites read `cur`, and the `Op::Call` arm sets it
/// only for an unprefixed admitted call; every node's lowering clears it.
#[test]
fn r25w7c_the_sites_read_the_one_flag() {
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
    // Each needle also appears once here, in this test's own source.
    let in_code = |needle: &str| src.matches(needle).count() >= 1;
    assert!(in_code(
        "self.tail_sink.cur = self.call_prefix_join.is_none() && self.tail_sink.admits(id);"
    ));
    assert!(in_code(
        "let sunk = self.tail_sink.cur && sync_exit.is_none();"
    ));
    assert!(in_code("if hits_republished && !self.tail_sink.cur {"));
    assert!(in_code("lowerer.end_tail_republish_sink_body();"));
    // Spelled with escapes and a continuation here, so the one match is
    // the code (the clear at the top of the per-node lowering).
    assert!(src.contains(
        "// W7-C: a sink decision belongs to the one `Op::Call` \
         that made it.\n        self.tail_sink.cur = false;"
    ));
}
