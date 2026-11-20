// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;
use crate::ir::{with_guard_token, with_licensing_guard, MemKind, SafepointSnapshot};

#[test]
fn arraylist_and_charat_reuse_prehoisted_array_length_and_elide_bounds() {
    let mut g = Graph::empty(32);
    let start = g.add(Op::Start, IrType::Void, vec![], None);
    g.entry = start;
    let ctrl0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let list = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let n_param = g.add(Op::Param(1), IrType::Int, vec![start], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let mask15 = g.add(Op::Const(15), IrType::Int, vec![], None);
    let ed_f = g.add(Op::Const(0), IrType::Int, vec![], None);
    let null_ref = g.add(Op::Const(0), IrType::Ref, vec![], None);
    g.push_safepoint(SafepointSnapshot::new(
        10,
        vec![list, n_param, zero, zero],
        vec![],
        vec![],
    ));
    let pre_recv_ne = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![list, null_ref], None);
    let pre_recv_g = g.add(
        Op::Guard { bci: 4 },
        IrType::Void,
        vec![ctrl0, pre_recv_ne],
        Some(4),
    );
    g.set_node_frame_snapshot(pre_recv_g, 0);
    let pre_ed = g.add(
        Op::Load(MemKind::Ref),
        IrType::Ref,
        with_licensing_guard(vec![ctrl0, mem0, list, ed_f], pre_recv_g),
        Some(4),
    );
    let pre_al = g.add(
        Op::ArrayLength,
        IrType::Int,
        vec![ctrl0, pre_ed, pre_ed],
        Some(4),
    );

    let hdr = g.add(Op::Region, IrType::Control, vec![ctrl0, ctrl0], Some(10));
    let phi_i = g.add(Op::Phi, IrType::Int, vec![hdr, zero, zero], Some(10));
    let cond = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![phi_i, n_param], None);
    let iff = g.add(Op::If, IrType::Control, vec![hdr, cond], Some(10));
    let body = g.add(Op::Proj(0), IrType::Control, vec![iff], Some(12));
    let exit = g.add(Op::Proj(1), IrType::Control, vec![iff], Some(20));

    let idx = g.add(Op::And, IrType::Int, vec![phi_i, mask15], None);
    let lt_len = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![idx, pre_al], None);
    let bg = g.add(
        Op::Guard { bci: 14 },
        IrType::Void,
        vec![body, lt_len],
        Some(14),
    );
    g.set_node_frame_snapshot(bg, 0);
    let elem = g.add(
        Op::ArrayLoad(MemKind::Ref),
        IrType::Ref,
        with_licensing_guard(vec![body, pre_al, pre_ed, idx], bg),
        Some(14),
    );
    let next_i = g.add(Op::Add, IrType::Int, vec![phi_i, one], None);
    g.set_input(hdr, 1, body);
    g.set_input(phi_i, 2, next_i);
    g.exit = g.add(Op::Return, IrType::Void, vec![exit, elem], Some(20));

    let mut left = Vec::new();
    let mut replaced = 0;
    let planted = hoist_invariant_arraylist_get(
        &mut g,
        hdr,
        ctrl0,
        &[body],
        Some(pre_al),
        10,
        0,
        false,
        &mut left,
        &mut replaced,
    );
    assert!(planted >= 2);
    let al_count = g.nodes.iter().filter(|n| n.op == Op::ArrayLength).count();
    assert_eq!(al_count, 1, "must reuse pre-hoisted ArrayLength");
    assert_eq!(
        g.nodes[bg as usize].op,
        Op::Dead,
        "in-loop bounds guard must be killed"
    );
}

#[test]
fn string_builder_and_hashmap_hoist_invariants_out_of_loop() {
    // StringBuilder loop
    let mut g = Graph::empty(32);
    let start = g.add(Op::Start, IrType::Void, vec![], None);
    g.entry = start;
    let ctrl0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let sb = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let n_param = g.add(Op::Param(1), IrType::Int, vec![start], None);
    let ch = g.add(Op::Param(2), IrType::Int, vec![start], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let null_ref = g.add(Op::Const(0), IrType::Ref, vec![], None);
    let f_val = g.add(Op::Const(0), IrType::Int, vec![], None);
    let f_cnt = g.add(Op::Const(1), IrType::Int, vec![], None);
    let f_cod = g.add(Op::Const(2), IrType::Int, vec![], None);
    g.push_safepoint(SafepointSnapshot::new(
        10,
        vec![sb, n_param, ch, zero],
        vec![],
        vec![],
    ));

    let hdr = g.add(Op::Region, IrType::Control, vec![ctrl0, ctrl0], Some(10));
    let phi_i = g.add(Op::Phi, IrType::Int, vec![hdr, zero, zero], Some(10));
    let phi_mem = g.add(Op::Phi, IrType::Memory, vec![hdr, mem0, mem0], Some(10));
    let cond = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![phi_i, n_param], None);
    let iff = g.add(Op::If, IrType::Control, vec![hdr, cond], Some(10));
    let body = g.add(Op::Proj(0), IrType::Control, vec![iff], Some(12));
    let exit = g.add(Op::Proj(1), IrType::Control, vec![iff], Some(20));

    let sb_ne = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![sb, null_ref], None);
    let sb_ng = g.add(
        Op::Guard { bci: 14 },
        IrType::Void,
        vec![body, sb_ne],
        Some(14),
    );
    g.set_node_frame_snapshot(sb_ng, 0);
    let sb_ex = g.add(
        Op::ExactClassIs { class_id: 24 },
        IrType::Int,
        vec![sb],
        None,
    );
    let sb_eg = g.add(
        Op::Guard { bci: 14 },
        IrType::Void,
        vec![body, sb_ex],
        Some(14),
    );
    g.set_node_frame_snapshot(sb_eg, 0);
    let coder = g.add(
        Op::Load(MemKind::Byte),
        IrType::Int,
        with_licensing_guard(vec![body, phi_mem, sb, f_cod], sb_eg),
        Some(14),
    );
    let c_eq0 = g.add(Op::Cmp(CmpOp::Eq), IrType::Int, vec![coder, zero], None);
    let cg = g.add(
        Op::Guard { bci: 14 },
        IrType::Void,
        vec![body, c_eq0],
        Some(14),
    );
    g.set_node_frame_snapshot(cg, 0);
    let val = g.add(
        Op::Load(MemKind::Ref),
        IrType::Ref,
        with_licensing_guard(vec![body, coder, sb, f_val], sb_eg),
        Some(14),
    );
    let v_ne = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![val, null_ref], None);
    let vg = g.add(
        Op::Guard { bci: 14 },
        IrType::Void,
        vec![body, v_ne],
        Some(14),
    );
    g.set_node_frame_snapshot(vg, 0);
    let cnt = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![body, val, sb, f_cnt],
        Some(14),
    );
    let cap = g.add(Op::ArrayLength, IrType::Int, vec![body, cnt, val], Some(14));
    let ast = g.add(
        Op::ArrayStore(MemKind::Byte),
        IrType::Memory,
        vec![body, cap, val, cnt, ch],
        Some(14),
    );
    let next_cnt = g.add(Op::Add, IrType::Int, vec![cnt, one], None);
    let cst = g.add(
        Op::Store(MemKind::Int),
        IrType::Memory,
        vec![body, ast, sb, f_cnt, next_cnt],
        Some(14),
    );
    let next_i = g.add(Op::Add, IrType::Int, vec![phi_i, one], None);
    g.set_input(hdr, 1, body);
    g.set_input(phi_i, 2, next_i);
    g.set_input(phi_mem, 2, cst);
    g.exit = g.add(Op::Return, IrType::Void, vec![exit], Some(20));

    let mut left = Vec::new();
    let mut replaced = 0;
    let planted = hoist_invariant_string_charat(
        &mut g,
        hdr,
        ctrl0,
        &[body],
        Some(mem0),
        10,
        0,
        false,
        &mut left,
        &mut replaced,
    );
    assert!(planted >= 3);
    assert!(g
        .nodes
        .iter()
        .any(|n| n.op == Op::ArrayLength && n.input_opt(0) == Some(ctrl0)));
    assert_eq!(g.nodes[sb_ng as usize].op, Op::Dead);
    assert_eq!(g.nodes[sb_eg as usize].op, Op::Dead);
    assert_eq!(g.nodes[coder as usize].op, Op::Dead);
    assert_eq!(g.nodes[val as usize].op, Op::Dead);
    assert_eq!(g.nodes[cap as usize].op, Op::Dead);

    // HashMap loop
    let mut hg = Graph::empty(32);
    let h_start = hg.add(Op::Start, IrType::Void, vec![], None);
    hg.entry = h_start;
    let h_ctrl0 = hg.add(Op::Proj(0), IrType::Control, vec![h_start], None);
    let h_mem0 = hg.add(Op::Proj(1), IrType::Memory, vec![h_start], None);
    let map = hg.add(Op::Param(0), IrType::Ref, vec![h_start], None);
    let hn = hg.add(Op::Param(1), IrType::Int, vec![h_start], None);
    let h_zero = hg.add(Op::Const(0), IrType::Int, vec![], None);
    let h_one = hg.add(Op::Const(1), IrType::Int, vec![], None);
    let h_null = hg.add(Op::Const(0), IrType::Ref, vec![], None);
    let f_tab = hg.add(Op::Const(0), IrType::Int, vec![], None);
    hg.push_safepoint(SafepointSnapshot::new(
        10,
        vec![map, hn, h_zero],
        vec![],
        vec![],
    ));

    let h_hdr = hg.add(
        Op::Region,
        IrType::Control,
        vec![h_ctrl0, h_ctrl0],
        Some(10),
    );
    let h_i = hg.add(Op::Phi, IrType::Int, vec![h_hdr, h_zero, h_zero], Some(10));
    let h_mem = hg.add(
        Op::Phi,
        IrType::Memory,
        vec![h_hdr, h_mem0, h_mem0],
        Some(10),
    );
    let h_cond = hg.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![h_i, hn], None);
    let h_iff = hg.add(Op::If, IrType::Control, vec![h_hdr, h_cond], Some(10));
    let h_body = hg.add(Op::Proj(0), IrType::Control, vec![h_iff], Some(12));
    let h_exit = hg.add(Op::Proj(1), IrType::Control, vec![h_iff], Some(20));

    let map_ne = hg.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![map, h_null], None);
    let map_ng = hg.add(
        Op::Guard { bci: 14 },
        IrType::Void,
        vec![h_body, map_ne],
        Some(14),
    );
    hg.set_node_frame_snapshot(map_ng, 0);
    let map_ex = hg.add(
        Op::ExactClassIs { class_id: 21 },
        IrType::Int,
        vec![map],
        None,
    );
    let map_eg = hg.add(
        Op::Guard { bci: 14 },
        IrType::Void,
        vec![h_body, map_ex],
        Some(14),
    );
    hg.set_node_frame_snapshot(map_eg, 0);
    let tab = hg.add(
        Op::Load(MemKind::Ref),
        IrType::Ref,
        with_licensing_guard(vec![h_body, h_mem, map, f_tab], map_eg),
        Some(14),
    );
    let t_ne = hg.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![tab, h_null], None);
    let tg = hg.add(
        Op::Guard { bci: 14 },
        IrType::Void,
        vec![h_body, t_ne],
        Some(14),
    );
    hg.set_node_frame_snapshot(tg, 0);
    let tlen = hg.add(
        Op::ArrayLength,
        IrType::Int,
        vec![h_body, tab, tab],
        Some(14),
    );
    let mask = hg.add(Op::Sub, IrType::Int, vec![tlen, h_one], None);
    let m_ge0 = hg.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![mask, h_zero], None);
    let mg = hg.add(
        Op::Guard { bci: 14 },
        IrType::Void,
        vec![h_body, m_ge0],
        Some(14),
    );
    hg.set_node_frame_snapshot(mg, 0);
    let landm = hg.add(Op::And, IrType::Int, vec![tlen, mask], None);
    let pow2 = hg.add(Op::Cmp(CmpOp::Eq), IrType::Int, vec![landm, h_zero], None);
    let pg = hg.add(
        Op::Guard { bci: 14 },
        IrType::Void,
        vec![h_body, pow2],
        Some(14),
    );
    hg.set_node_frame_snapshot(pg, 0);
    let idx = hg.add(Op::And, IrType::Int, vec![mask, h_i], None);
    let node = hg.add(
        Op::ArrayLoad(MemKind::Ref),
        IrType::Ref,
        with_guard_token(vec![h_body, tlen, tab, idx], mg),
        Some(14),
    );
    let h_next_i = hg.add(Op::Add, IrType::Int, vec![h_i, h_one], None);
    hg.set_input(h_hdr, 1, h_body);
    hg.set_input(h_i, 2, h_next_i);
    hg.set_input(h_mem, 2, node);
    hg.exit = hg.add(Op::Return, IrType::Void, vec![h_exit, node], Some(20));

    let mut h_left = Vec::new();
    let mut h_rep = 0;
    let h_planted = hoist_invariant_string_charat(
        &mut hg,
        h_hdr,
        h_ctrl0,
        &[h_body],
        Some(h_mem0),
        10,
        0,
        false,
        &mut h_left,
        &mut h_rep,
    );
    assert!(h_planted >= 5);
    assert_eq!(hg.nodes[map_ng as usize].op, Op::Dead);
    assert_eq!(hg.nodes[map_eg as usize].op, Op::Dead);
    assert_eq!(hg.nodes[tab as usize].op, Op::Dead);
    assert_eq!(hg.nodes[tlen as usize].op, Op::Dead);
    assert_eq!(hg.nodes[mask as usize].op, Op::Dead);
    assert!(hg
        .nodes
        .iter()
        .any(|n| n.op == Op::ArrayLength && n.input_opt(0) == Some(h_ctrl0)));
}
