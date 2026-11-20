// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;
use crate::deopt::DespecRegistry;
use crate::ir::{MemKind, SafepointSnapshot};
use crate::ir_check_elim;
use crate::ir_schedule::schedule;

fn seq_charat_row() -> usize {
    let info: &'static crate::JitInvokeInfo = Box::leak(Box::new(crate::JitInvokeInfo {
        class_name: "java/lang/CharSequence",
        method_name: "charAt",
        descriptor: "(I)C",
        num_jit_args: 2,
        return_type: b'C',
        invoke_kind: 2,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    info as *const crate::JitInvokeInfo as usize
}

#[test]
fn hoist_invariant_string_charat_raw_elides_null_and_bounds_checks() {
    let mut g = Graph::empty(1);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let s = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let n = g.add(Op::Param(1), IrType::Int, vec![start], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let mask15 = g.add(Op::Const(15), IrType::Int, vec![], None);
    let coder_off = g.add(Op::Const(1), IrType::Int, vec![], None);
    let value_off = g.add(Op::Const(0), IrType::Int, vec![], None);

    let region = g.add(Op::Region, IrType::Control, vec![entry, NO_NODE], Some(4));
    let i_phi = g.add(Op::Phi, IrType::Int, vec![region, zero, NO_NODE], Some(4));
    let acc_phi = g.add(Op::Phi, IrType::Int, vec![region, zero, NO_NODE], Some(4));
    let cmp = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![i_phi, n], Some(5));
    let iff = g.add(Op::If, IrType::Control, vec![region, cmp], Some(5));
    let body = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
    let exit = g.add(Op::Proj(1), IrType::Control, vec![iff], None);

    let coder = g.add(
        Op::Load(MemKind::Byte),
        IrType::Int,
        vec![body, mem0, s, coder_off],
        Some(6),
    );
    let value = g.add(
        Op::Load(MemKind::Ref),
        IrType::Ref,
        vec![body, coder, s, value_off],
        Some(7),
    );
    let idx = g.add(Op::And, IrType::Int, vec![i_phi, mask15], Some(8));
    let decode = g.add(
        Op::StrCharDecode,
        IrType::Int,
        vec![body, value, value, idx, coder],
        Some(9),
    );
    let acc_next = g.add(Op::Add, IrType::Int, vec![acc_phi, decode], Some(10));
    let i_next = g.add(Op::Add, IrType::Int, vec![i_phi, one], Some(11));
    g.set_input(region, 1, body);
    g.set_input(i_phi, 2, i_next);
    g.set_input(acc_phi, 2, acc_next);
    g.exit = g.add(Op::Return, IrType::Void, vec![exit, acc_phi], Some(12));
    g.push_safepoint(SafepointSnapshot::new(
        4,
        vec![s, n, i_phi, acc_phi],
        vec![],
        vec![],
    ));

    let mut left = Vec::new();
    let mut replaced = 0usize;
    let guards = hoist_invariant_string_charat(
        &mut g,
        region,
        entry,
        &[body],
        Some(mem0),
        4,
        0,
        false,
        &mut left,
        &mut replaced,
    );
    assert!(
        guards > 0,
        "must hoist StrCharDecode metadata and mask guard"
    );
    let raw = g
        .nodes
        .iter()
        .enumerate()
        .find_map(|(id, nd)| (nd.op == Op::ArrayLoad(MemKind::Byte)).then_some(id as NodeId))
        .expect("Latin1 ArrayLoad(Byte) created");
    let sched = schedule(&g);
    let checks = ir_check_elim::analyze(&g, &sched);
    assert!(
        checks.null_elided(raw),
        "hoisted String value ArrayLength must elide raw null check"
    );
    assert!(
        checks.bounds_elided(raw),
        "hoisted mask guard (15 < byte_len) must elide raw bounds check"
    );
}

#[test]
fn hoist_seq_charat_latin1_uses_byte_len_so_raw_elides_null_and_bounds() {
    let mut g = Graph::empty(1);
    g.string_layout = Some(crate::StringFieldLayout::new(0, Some(1), 2, 7));
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let s = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let n = g.add(Op::Param(1), IrType::Int, vec![start], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);

    let is_str = g.add(Op::ExactClassIs { class_id: 7 }, IrType::Int, vec![s], None);
    g.add(
        Op::Guard { bci: 4 },
        IrType::Void,
        vec![entry, is_str],
        Some(4),
    );
    g.seq_charat_trap_headers.insert(4);

    let region = g.add(Op::Region, IrType::Control, vec![entry, NO_NODE], Some(4));
    let i_phi = g.add(Op::Phi, IrType::Int, vec![region, zero, NO_NODE], Some(4));
    let acc_phi = g.add(Op::Phi, IrType::Int, vec![region, zero, NO_NODE], Some(4));
    let mem_phi = g.add(
        Op::Phi,
        IrType::Memory,
        vec![region, mem0, NO_NODE],
        Some(4),
    );
    let cmp = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![i_phi, n], Some(5));
    let iff = g.add(Op::If, IrType::Control, vec![region, cmp], Some(5));
    let body = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
    let exit = g.add(Op::Proj(1), IrType::Control, vec![iff], None);

    let call = g.add(
        Op::Call {
            info_ptr: seq_charat_row(),
        },
        IrType::Int,
        vec![body, mem_phi, s, i_phi],
        Some(9),
    );
    let acc_next = g.add(Op::Add, IrType::Int, vec![acc_phi, call], Some(10));
    let i_next = g.add(Op::Add, IrType::Int, vec![i_phi, one], Some(11));
    g.set_input(region, 1, body);
    g.set_input(i_phi, 2, i_next);
    g.set_input(acc_phi, 2, acc_next);
    g.set_input(mem_phi, 2, call);
    g.exit = g.add(Op::Return, IrType::Void, vec![exit, acc_phi], Some(12));

    g.push_safepoint(SafepointSnapshot::new(
        4,
        vec![s, n, i_phi, acc_phi],
        vec![],
        vec![],
    ));
    g.push_safepoint(SafepointSnapshot::new(
        9,
        vec![s, n, i_phi, acc_phi],
        vec![s, i_phi],
        vec![],
    ));

    let despec = DespecRegistry::new();
    let users = build_users(&g);
    let loop_body = loop_body_with(&g, &users, region, entry, &[body]);
    let layout = g.string_layout.expect("layout");
    let hoisted = hoist_one(
        &mut g,
        &despec,
        "T.f:(Ljava/lang/CharSequence;I)I",
        layout,
        region,
        entry,
        4,
        &loop_body,
        call,
        true,
    );
    assert!(hoisted, "latin1 hoist_one must succeed");
    let raw = g
        .nodes
        .iter()
        .enumerate()
        .find_map(|(id, nd)| (nd.op == Op::ArrayLoad(MemKind::Byte)).then_some(id as NodeId))
        .expect("Latin1 ArrayLoad(Byte) created");
    let sched = schedule(&g);
    let checks = ir_check_elim::analyze(&g, &sched);
    assert!(
        checks.null_elided(raw),
        "latin1 hoisted ArrayLength must elide raw null check"
    );
    assert!(
        checks.bounds_elided(raw),
        "latin1 Guard(idx < byte_len) + unit-stride IV must elide raw bounds check"
    );
}
