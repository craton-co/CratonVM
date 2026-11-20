// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;
use crate::deopt::DespecRegistry;
use crate::ir::SafepointSnapshot;

#[test]
fn w7_trap_free_preheader_hoists_charat_guards_and_folds_coder_branch() {
    let mut g = Graph::empty(3);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], Some(0));
    let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let coder = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let val = g.add(Op::Param(1), IrType::Ref, vec![start], None);
    let n = g.add(Op::Param(2), IrType::Int, vec![start], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let mask63 = g.add(Op::Const(63), IrType::Int, vec![], None);
    let null = g.add(Op::Const(0), IrType::Ref, vec![], None);

    g.safepoints
        .push(SafepointSnapshot::new(10, vec![], vec![], vec![]));
    let hdr_sp = (g.safepoints.len() - 1) as u32;

    let region = g.add(Op::Region, IrType::Control, vec![c0, NO_NODE], Some(10));
    g.set_node_frame_snapshot(region, hdr_sp);
    let i_phi = g.add(Op::Phi, IrType::Int, vec![region, zero, NO_NODE], Some(10));
    let m_phi = g.add(Op::Phi, IrType::Memory, vec![region, m0, NO_NODE], Some(10));
    let cond = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![i_phi, n], Some(10));
    let if_loop = g.add(Op::If, IrType::Control, vec![region, cond], Some(10));
    let body = g.add(Op::Proj(0), IrType::Control, vec![if_loop], Some(13));
    let exit = g.add(Op::Proj(1), IrType::Control, vec![if_loop], Some(30));

    let cmp_coder = g.add(Op::Cmp(CmpOp::Eq), IrType::Int, vec![coder, zero], Some(15));
    let g_coder = g.add(
        Op::Guard { bci: 15 },
        IrType::Void,
        vec![body, cmp_coder],
        Some(15),
    );
    let cmp_nn = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![val, null], Some(16));
    let g_nn = g.add(
        Op::Guard { bci: 16 },
        IrType::Void,
        vec![body, cmp_nn],
        Some(16),
    );
    let idx_mask = g.add(Op::And, IrType::Int, vec![i_phi, mask63], Some(18));
    let alen = g.add(
        Op::ArrayLength,
        IrType::Int,
        vec![body, m_phi, val],
        Some(19),
    );
    let shifted = g.add(Op::Shr, IrType::Int, vec![alen, coder], Some(20));
    let cmp_bnd = g.add(
        Op::Cmp(CmpOp::Lt),
        IrType::Int,
        vec![idx_mask, shifted],
        Some(21),
    );
    let g_bnd = g.add(
        Op::Guard { bci: 21 },
        IrType::Void,
        vec![body, cmp_bnd],
        Some(21),
    );
    let if_coder = g.add(Op::If, IrType::Control, vec![body, cmp_coder], Some(22));
    let latin1 = g.add(Op::Proj(0), IrType::Control, vec![if_coder], Some(23));
    let utf16 = g.add(Op::Proj(1), IrType::Control, vec![if_coder], Some(25));
    let back = g.add(Op::Merge, IrType::Control, vec![latin1, utf16], Some(28));
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let next_i = g.add(Op::Add, IrType::Int, vec![i_phi, one], Some(28));
    g.set_input(region, 1, back);
    g.set_input(i_phi, 2, next_i);
    g.set_input(m_phi, 2, m_phi);
    let _ = (g_coder, g_nn, g_bnd);
    g.exit = g.add(Op::Return, IrType::Void, vec![exit], Some(30));

    let despec = DespecRegistry::default();
    let census = plant_trap_free_preheaders(&mut g, &despec, "T.f()V", false);
    assert!(
        census.invariant >= 2,
        "expected coder and null guards hoisted"
    );
    assert_eq!(
        census.masked, 1,
        "expected masked charAt bounds guard hoisted"
    );
    assert!(census.folded >= 1, "expected If(coder == 0) folded");

    // Pre-header (`c0`) now owns the hoisted guards, and no Guard remains in body.
    assert!(
        !g.nodes
            .iter()
            .any(|nd| { matches!(nd.op, Op::Guard { .. }) && nd.input_opt(0) == Some(body) }),
        "body should be trap-free after hoisting to preheader"
    );
}

#[test]
fn w7_two_pass_hoists_final_java_coder_then_masked_bounds_and_verifies() {
    let mut g = Graph::empty(2);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], Some(0));
    let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let s = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let n = g.add(Op::Param(1), IrType::Int, vec![start], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let mask63 = g.add(Op::Const(63), IrType::Int, vec![], None);
    let off_coder = g.add(Op::Const(16), IrType::Long, vec![], None);
    let off_val = g.add(Op::Const(8), IrType::Long, vec![], None);

    g.safepoints
        .push(SafepointSnapshot::new(10, vec![], vec![], vec![]));
    let hdr_sp = (g.safepoints.len() - 1) as u32;

    let region = g.add(Op::Region, IrType::Control, vec![c0, NO_NODE], Some(10));
    g.set_node_frame_snapshot(region, hdr_sp);
    let i_phi = g.add(Op::Phi, IrType::Int, vec![region, zero, NO_NODE], Some(10));
    let m_phi = g.add(Op::Phi, IrType::Memory, vec![region, m0, NO_NODE], Some(10));
    let cond = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![i_phi, n], Some(10));
    let if_loop = g.add(Op::If, IrType::Control, vec![region, cond], Some(10));
    let body = g.add(Op::Proj(0), IrType::Control, vec![if_loop], Some(13));
    let exit = g.add(Op::Proj(1), IrType::Control, vec![if_loop], Some(30));

    let coder = g.add(
        Op::Load(crate::ir::MemKind::Byte),
        IrType::Int,
        vec![c0, m0, s, off_coder],
        Some(5),
    );
    let val = g.add(
        Op::Load(crate::ir::MemKind::Ref),
        IrType::Ref,
        vec![c0, m0, s, off_val],
        Some(6),
    );
    g.final_java_field_reads.insert(coder);
    g.final_java_field_reads.insert(val);

    let cmp_coder = g.add(Op::Cmp(CmpOp::Eq), IrType::Int, vec![coder, zero], Some(17));
    let if_coder = g.add(Op::If, IrType::Control, vec![body, cmp_coder], Some(18));
    let latin1 = g.add(Op::Proj(0), IrType::Control, vec![if_coder], Some(19));
    let utf16 = g.add(Op::Proj(1), IrType::Control, vec![if_coder], Some(25));

    // Only the true arm (latin1) has the null check + masked bounds guard.
    let null = g.add(Op::Const(0), IrType::Ref, vec![], None);
    let cmp_nn = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![val, null], Some(20));
    let _g_nn = g.add(
        Op::Guard { bci: 20 },
        IrType::Void,
        vec![latin1, cmp_nn],
        Some(20),
    );
    let idx_mask = g.add(Op::And, IrType::Int, vec![i_phi, mask63], Some(20));
    let alen = g.add(
        Op::ArrayLength,
        IrType::Int,
        vec![latin1, m_phi, val],
        Some(21),
    );
    let cmp_bnd = g.add(
        Op::Cmp(CmpOp::Lt),
        IrType::Int,
        vec![idx_mask, alen],
        Some(22),
    );
    let _g_bnd = g.add(
        Op::Guard { bci: 22 },
        IrType::Void,
        vec![latin1, cmp_bnd],
        Some(22),
    );

    let back = g.add(Op::Merge, IrType::Control, vec![latin1, utf16], Some(28));
    let next_i = g.add(Op::Add, IrType::Int, vec![i_phi, one], Some(28));
    g.set_input(region, 1, back);
    g.set_input(i_phi, 2, next_i);
    g.set_input(m_phi, 2, m_phi);
    g.exit = g.add(Op::Return, IrType::Void, vec![exit], Some(30));

    let despec = DespecRegistry::default();
    let c1 = plant_trap_free_preheaders(&mut g, &despec, "T.f()V", false);
    let c2 = plant_trap_free_preheaders(&mut g, &despec, "T.f()V", false);
    assert!(
        c1.invariant >= 1,
        "pass 1 should hoist If(coder == 0): {c1:?}"
    );
    assert_eq!(
        c2.masked, 1,
        "pass 2 should hoist masked bounds guard: {c2:?}"
    );
    crate::ir_verify::verify_graph_at_phase(&g, "w7_two_pass")
        .expect("IR should verify after two passes");
}

#[test]
fn w7_does_not_hoist_nullable_or_type_narrowed_final_field_load() {
    let mut g = Graph::empty(64);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    g.entry = c0;
    let s = g.add(Op::Param(1), IrType::Ref, vec![start], None);
    let n = g.add(Op::Param(2), IrType::Int, vec![start], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let off_val = g.add(Op::Const(16), IrType::Int, vec![], None);

    g.safepoints
        .push(SafepointSnapshot::new(10, vec![], vec![], vec![]));
    let hdr_sp = (g.safepoints.len() - 1) as u32;

    let region = g.add(Op::Region, IrType::Control, vec![c0, NO_NODE], Some(10));
    g.set_node_frame_snapshot(region, hdr_sp);
    let i_phi = g.add(Op::Phi, IrType::Int, vec![region, zero, NO_NODE], Some(10));
    let m_phi = g.add(Op::Phi, IrType::Memory, vec![region, m0, NO_NODE], Some(10));
    let cond = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![i_phi, n], Some(10));
    let if_loop = g.add(Op::If, IrType::Control, vec![region, cond], Some(10));
    let body = g.add(Op::Proj(0), IrType::Control, vec![if_loop], Some(13));
    let exit = g.add(Op::Proj(1), IrType::Control, vec![if_loop], Some(30));

    let is_str = g.add(Op::ExactClassIs { class_id: 6 }, IrType::Int, vec![s], Some(14));
    let _g_str = g.add(Op::Guard { bci: 14 }, IrType::Void, vec![body, is_str], Some(14));
    let val = g.add(
        Op::Load(crate::ir::MemKind::Ref),
        IrType::Ref,
        vec![body, m_phi, s, off_val],
        Some(16),
    );
    g.final_java_field_reads.insert(val);
    let _len = g.add(Op::ArrayLength, IrType::Int, vec![body, val, val], Some(18));

    let next_i = g.add(Op::Add, IrType::Int, vec![i_phi, one], Some(20));
    let back = g.add(Op::Merge, IrType::Control, vec![body], Some(20));
    g.set_input(region, 1, back);
    g.set_input(i_phi, 2, next_i);
    g.set_input(m_phi, 2, m_phi);
    g.exit = g.add(Op::Return, IrType::Void, vec![exit], Some(30));

    let users = build_users(&g);
    let body_set = loop_body_with(&g, &users, region, c0, &[back]);
    let hoisted = hoist_invariant_final_array_loads(
        &mut g,
        region,
        c0,
        Some(m0),
        &users,
        &body_set,
    );
    assert_eq!(hoisted, 0, "must not hoist nullable/type-narrowed load");
}
