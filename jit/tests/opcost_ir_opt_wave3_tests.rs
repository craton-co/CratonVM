// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! External integration tests for wave-3 OpCost IR/optimizer fixes:
//! - `Op::IndyRootRef` array length constant folding
//! - `Op::InstanceOf` on loaded final class strengthened to `Op::ExactClassIs`
//!   and dominated `CheckCast` dropped across `Op::Merge`
//! - `throwCatch` bounded non-negative IV compare fold (`i >= 0` -> `1`) +
//!   post-prune `Guard(ClassInitTest)` hoisting
//! - `getfield` pointer-chasing `Op::Load` deopt unrolling (`4x`)

use cratonvm_jit::ir::{
    with_guard_token, CmpOp, ConstRefMeta, Graph, IrType, MemKind, NodeId, Op, SafepointSnapshot,
    NO_NODE,
};
use cratonvm_jit::ir_optimize::optimize;

#[test]
fn indy_root_ref_array_length_folds_to_const_in_optimize() {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], Some(0));
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], Some(0));
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], Some(0));
    let slot_addr = 0xBEEF_4000;
    let arr = g.add(
        Op::IndyRootRef { slot_addr },
        IrType::Ref,
        vec![ctrl],
        Some(1),
    );
    g.const_ref_meta.insert(
        slot_addr,
        ConstRefMeta {
            non_null: true,
            exact_class_id: Some(88),
            prim_array_tag: None,
            array_length: Some(12),
            static_field: Some((99, 1)),
        },
    );
    let len = g.add(Op::ArrayLength, IrType::Int, vec![ctrl, mem, arr], Some(2));
    let ret = g.add(Op::Return, IrType::Void, vec![ctrl, len], Some(3));
    g.exit = ret;

    optimize(&mut g);
    let ret_val = g.nodes[ret as usize].inputs[1];
    assert_eq!(g.nodes[ret_val as usize].op, Op::Const(12));
}

#[test]
fn final_instanceof_strengthens_and_drops_checkcast_across_merge() {
    let (name_ptr, name_len) = cratonvm_jit::intern_typecheck_target_with_finality(
        "com/example/Wave3FinalP",
        Some(0x6001_0001),
        true,
    );
    let mut g = Graph::empty(1);
    let start = g.add(Op::Start, IrType::Control, vec![], Some(0));
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], Some(0));
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], Some(0));
    let obj = g.add(Op::Param(0), IrType::Ref, vec![start], Some(0));
    let io = g.add(
        Op::InstanceOf {
            name_ptr: name_ptr as usize,
            name_len,
            seen_cell: 0,
            header_only: false,
        },
        IrType::Int,
        vec![ctrl, mem, obj],
        Some(1),
    );
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let cmp = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![io, zero], Some(2));
    let iff = g.add(Op::If, IrType::Control, vec![ctrl, cmp], Some(2));
    let hit = g.add(Op::Proj(0), IrType::Control, vec![iff], Some(2));
    let miss = g.add(Op::Proj(1), IrType::Control, vec![iff], Some(2));
    let merge_hit = g.add(Op::Merge, IrType::Control, vec![hit], Some(3));
    let cast = g.add(
        Op::CheckCast {
            name_ptr: name_ptr as usize,
            name_len,
        },
        IrType::Ref,
        vec![merge_hit, mem, obj],
        Some(4),
    );
    let join = g.add(Op::Merge, IrType::Control, vec![merge_hit, miss], Some(5));
    let r_phi = g.add(Op::Phi, IrType::Ref, vec![join, cast, obj], Some(5));
    g.exit = g.add(Op::Return, IrType::Void, vec![join, r_phi], Some(6));

    optimize(&mut g);
    assert!(
        g.nodes.iter().any(|n| matches!(
            n.op,
            Op::ExactClassIs {
                class_id: 0x6001_0001
            }
        )),
        "loaded final class InstanceOf should strengthen to ExactClassIs"
    );
    assert!(
        !g.nodes
            .iter()
            .any(|n| matches!(n.op, Op::InstanceOf { .. })),
        "original Op::InstanceOf must be replaced"
    );
    assert!(
        !g.nodes.iter().any(|n| matches!(n.op, Op::CheckCast { .. })),
        "dominated CheckCast across Merge must be eliminated"
    );
}

#[test]
fn throw_catch_loop_folds_iv_nonneg_branch_and_hoists_class_init_guard() {
    let mut g = Graph::empty(1);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let _mem0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let n = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let hundred = g.add(Op::Const(100), IrType::Int, vec![], None);
    let nz = g.add(
        Op::Cmp(CmpOp::Ne),
        IrType::Int,
        vec![hundred, zero],
        Some(1),
    );
    let div_g = g.add(Op::Guard { bci: 1 }, IrType::Void, vec![entry, nz], Some(1));
    let bound = g.add(
        Op::Div,
        IrType::Int,
        with_guard_token(vec![n, hundred], div_g),
        Some(1),
    );

    let header = g.add(Op::Region, IrType::Control, vec![entry, NO_NODE], Some(4));
    let i_phi = g.add(Op::Phi, IrType::Int, vec![header, zero, NO_NODE], Some(4));
    let s_phi = g.add(Op::Phi, IrType::Int, vec![header, zero, NO_NODE], Some(4));

    // `if (i >= 0)` at loop header (executes before loop exit check) with
    // `Guard(ClassInitTest)` on true arm.
    let cmp_ge0 = g.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![i_phi, zero], Some(7));
    let if_ge0 = g.add(Op::If, IrType::Control, vec![header, cmp_ge0], Some(7));
    let t_arm = g.add(Op::Proj(0), IrType::Control, vec![if_ge0], None);
    let f_arm = g.add(Op::Proj(1), IrType::Control, vec![if_ge0], None);

    let ev_test = g.add(
        Op::ClassInitTest {
            state_addr: 0xDEAD_7000,
            initialized: 0,
        },
        IrType::Int,
        vec![t_arm],
        Some(8),
    );
    let ev_guard = g.add(
        Op::Guard { bci: 8 },
        IrType::Void,
        vec![t_arm, ev_test],
        Some(8),
    );
    let s_throw = g.add(Op::Add, IrType::Int, vec![s_phi, one], Some(9));
    let arm_merge = g.add(Op::Merge, IrType::Control, vec![t_arm, f_arm], Some(10));
    let s_next = g.add(
        Op::Phi,
        IrType::Int,
        vec![arm_merge, s_throw, s_phi],
        Some(10),
    );
    let cmp_loop = g.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![i_phi, bound], Some(5));
    let if_loop = g.add(Op::If, IrType::Control, vec![arm_merge, cmp_loop], Some(5));
    let exit_ctrl = g.add(Op::Proj(0), IrType::Control, vec![if_loop], None);
    let body_ctrl = g.add(Op::Proj(1), IrType::Control, vec![if_loop], None);
    let i_next = g.add(Op::Add, IrType::Int, vec![i_phi, one], Some(11));

    g.set_input(header, 1, body_ctrl);
    g.set_input(i_phi, 2, i_next);
    g.set_input(s_phi, 2, s_next);
    g.exit = g.add(Op::Return, IrType::Void, vec![exit_ctrl, s_phi], Some(12));

    g.push_safepoint(SafepointSnapshot::new(
        4,
        vec![n, i_phi, s_phi],
        vec![],
        vec![],
    ));
    let ev_si = (g.safepoints.len() - 1) as u32;
    g.set_node_frame_snapshot(header, ev_si);
    g.set_node_frame_snapshot(ev_guard, ev_si);

    optimize(&mut g);
    let has_in_body_ev_guard = g.nodes.iter().any(|n| {
        n.op != Op::Dead
            && matches!(
                n.op,
                Op::ClassInitTest {
                    state_addr: 0xDEAD_7000,
                    ..
                }
            )
            && n.inputs.first().copied() != Some(entry)
    });
    assert!(
        !has_in_body_ev_guard,
        "after i >= 0 folds & prunes, ClassInitTest event guard must hoist to preheader"
    );
}

#[test]
fn pointer_chasing_getfield_loop_unrolls_via_deopt_reads() {
    let mut g = Graph::empty(2);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let head = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let n = g.add(Op::Param(1), IrType::Int, vec![start], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let off_x = g.add(Op::Const(16), IrType::Int, vec![], None);
    let off_next = g.add(Op::Const(24), IrType::Int, vec![], None);

    let header = g.add(Op::Region, IrType::Control, vec![entry, NO_NODE], Some(4));
    let i_phi = g.add(Op::Phi, IrType::Int, vec![header, zero, NO_NODE], Some(4));
    let s_phi = g.add(Op::Phi, IrType::Int, vec![header, zero, NO_NODE], Some(4));
    let p_phi = g.add(Op::Phi, IrType::Ref, vec![header, head, NO_NODE], Some(4));
    let mem_phi = g.add(
        Op::Phi,
        IrType::Memory,
        vec![header, mem0, NO_NODE],
        Some(4),
    );
    let cmp = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![i_phi, n], Some(5));
    let iff = g.add(Op::If, IrType::Control, vec![header, cmp], Some(5));
    let body = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
    let exit = g.add(Op::Proj(1), IrType::Control, vec![iff], None);

    let ld_x = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![body, mem_phi, p_phi, off_x],
        Some(7),
    );
    let ld_next = g.add(
        Op::Load(MemKind::Ref),
        IrType::Ref,
        vec![body, ld_x, p_phi, off_next],
        Some(8),
    );
    let s_next = g.add(Op::Add, IrType::Int, vec![s_phi, ld_x], Some(9));
    let i_next = g.add(Op::Add, IrType::Int, vec![i_phi, one], Some(10));
    g.set_input(header, 1, body);
    g.set_input(i_phi, 2, i_next);
    g.set_input(s_phi, 2, s_next);
    g.set_input(p_phi, 2, ld_next);
    g.set_input(mem_phi, 2, ld_next);
    g.exit = g.add(Op::Return, IrType::Void, vec![exit, s_phi], Some(11));

    g.push_safepoint(SafepointSnapshot::new(
        4,
        vec![head, n, i_phi, s_phi, p_phi],
        vec![],
        vec![],
    ));
    g.push_safepoint(SafepointSnapshot::new(
        7,
        vec![head, n, i_phi, s_phi, p_phi],
        vec![],
        vec![],
    ));
    g.push_safepoint(SafepointSnapshot::new(
        8,
        vec![head, n, i_phi, s_phi, p_phi],
        vec![],
        vec![],
    ));
    let _ = with_guard_token;

    optimize(&mut g);
    let live_loads = g
        .nodes
        .iter()
        .filter(|n| n.op != Op::Dead && matches!(n.op, Op::Load(_)))
        .count();
    assert!(
        live_loads > 2,
        "pointer-chasing Op::Load loop should main/post unroll (live loads: {live_loads})"
    );
}
