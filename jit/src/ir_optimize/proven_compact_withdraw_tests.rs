// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use super::optimize_context::proven_compact_withdrawn_pcs;
use crate::ir::{Graph, IrType, MemKind, Op};

#[test]
fn withdraws_null_check_and_implicit_null_trap_pcs() {
    let mut g = Graph::empty(16);
    let start = g.add(Op::Start, IrType::Void, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let obj = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let f0 = g.add(Op::Const(0), IrType::Int, vec![], None);
    let f1 = g.add(Op::Const(1), IrType::Int, vec![], None);
    let l1 = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![ctrl, mem, obj, f0],
        Some(7),
    );
    let l2 = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![ctrl, l1, obj, f1],
        Some(12),
    );
    g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, l2], Some(15));

    let despec = crate::deopt::DespecRegistry::new();
    let key = "test/NullTrap.f:(Ltest/Obj;)I";
    let null_id = crate::deopt::speculation_id(7, crate::deopt::DeoptReason::NullCheck);
    despec.insert(key, 7, null_id);
    let pcs = proven_compact_withdrawn_pcs(&g, &despec, key);
    assert!(pcs.contains(&7));
    assert!(pcs.contains(&12), "same base site should also withdraw");

    let despec2 = crate::deopt::DespecRegistry::new();
    despec2.note_implicit_null_trap(key, 12, 1);
    let pcs2 = proven_compact_withdrawn_pcs(&g, &despec2, key);
    assert!(pcs2.contains(&12));
}
