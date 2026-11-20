use super::*;
use crate::ir::{IrType, NO_NODE};
use crate::ir_schedule;

/// `Guard(arr != null); arr[i]` — the guard deopts on a null `arr`, so the
/// access behind it needs no null check of its own. The bounds check is
/// untouched: a guard proves nothing about the index.
#[test]
fn a_non_null_guard_is_a_null_check_fact() {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let arr = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let idx = g.add(Op::Param(1), IrType::Int, vec![start], None);
    let null = g.add(Op::Const(0), IrType::Ref, vec![], None);
    let cond = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![arr, null], Some(0));
    let guard = g.add(
        Op::Guard { bci: 0 },
        IrType::Void,
        vec![ctrl, cond],
        Some(0),
    );
    let load = g.add(
        Op::ArrayLoad(MemKind::Int),
        IrType::Int,
        vec![ctrl, mem, arr, idx],
        Some(1),
    );
    g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, load], Some(2));

    assert_eq!(
        guard_non_null_operand(&g, &g.nodes[guard as usize]),
        Some(arr)
    );
    assert_eq!(guard_non_null_operand(&g, &g.nodes[load as usize]), None);

    let sched = ir_schedule::schedule(&g);
    let e = analyze(&g, &sched);
    assert!(
        e.null_elided(load),
        "the guard already rejected a null `arr`"
    );
    assert!(!e.bounds_elided(load));
}

/// `Guard(x == null)` continues only when `x` IS null, and a compare of
/// null with null proves nothing.
#[test]
fn only_a_not_equal_null_guard_proves_non_null() {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let x = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let null = g.add(Op::Const(0), IrType::Ref, vec![], None);
    let eq = g.add(Op::Cmp(CmpOp::Eq), IrType::Int, vec![x, null], None);
    let g_eq = g.add(Op::Guard { bci: 0 }, IrType::Void, vec![ctrl, eq], None);
    let nn = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![null, null], None);
    let g_nn = g.add(Op::Guard { bci: 0 }, IrType::Void, vec![ctrl, nn], None);
    let rev = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![null, x], None);
    let g_rev = g.add(Op::Guard { bci: 0 }, IrType::Void, vec![ctrl, rev], None);
    assert_eq!(guard_non_null_operand(&g, &g.nodes[g_eq as usize]), None);
    assert_eq!(guard_non_null_operand(&g, &g.nodes[g_nn as usize]), None);
    assert_eq!(
        guard_non_null_operand(&g, &g.nodes[g_rev as usize]),
        Some(x)
    );
}
