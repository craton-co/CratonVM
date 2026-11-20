use super::*;
use crate::ir::IrType;

/// `Const(0xFFFF_FFFF)` typed `Int` is `-1` (the 32-bit compare reads the
/// low half), so `v >= 0xFFFF_FFFF` states `v >= -1` and proves nothing
/// about `v`'s sign. Before the fix `non_negative_by` read the raw `i64`
/// and answered `Some(v)`.
#[test]
fn an_unsigned_spelled_minus_one_bound_proves_no_sign() {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let v = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let m1 = g.add(Op::Const(0xFFFF_FFFF), IrType::Int, vec![], None);
    let ge = g.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![v, m1], None);
    assert_eq!(int_const(&g, m1), Some(-1));
    assert_eq!(non_negative_by(&g, ge, true), None);
    // The sign-extended spelling of the same `int` shares its key.
    let m1s = g.add(Op::Const(-1), IrType::Int, vec![], None);
    assert!(len_key(&g, m1) == len_key(&g, m1s));
    // A genuine non-negative bound still proves the sign.
    let z = g.add(Op::Const(0), IrType::Int, vec![], None);
    let ge0 = g.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![v, z], None);
    assert_eq!(non_negative_by(&g, ge0, true), Some(v));
}

/// The unsigned spelling of a negative `int` start, `Const(0xFFFF_FFFF)`
/// (`-1`; `Graph::add` stores it sign-extended), is not a non-negative
/// start, so `for (i = -1; i < a.length; i++)` keeps its check.
#[test]
fn an_unsigned_spelled_negative_start_is_not_a_unit_stride_induction() {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let arr = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let region = g.add(Op::Merge, IrType::Control, vec![c0], None);
    let init = g.add(Op::Const(0xFFFF_FFFF), IrType::Int, vec![], None);
    let iv = g.add(Op::Phi, IrType::Int, vec![region, init], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let next = g.add(Op::Add, IrType::Int, vec![iv, one], None);
    g.nodes[iv as usize].inputs.push(next);
    let len = g.add(Op::ArrayLength, IrType::Int, vec![region, m0, arr], None);
    let lt = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![iv, len], None);
    let iff = g.add(Op::If, IrType::Control, vec![region, lt], None);
    let body = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
    let exit = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
    g.nodes[region as usize].inputs.push(body);
    let st = g.add(
        Op::ArrayStore(crate::ir::MemKind::Int),
        IrType::Void,
        vec![body, m0, arr, iv, one],
        Some(0),
    );
    g.exit = g.add(Op::Return, IrType::Void, vec![exit, one], Some(1));
    let sched = crate::ir_schedule::schedule(&g);
    let e = analyze(&g, &sched);
    assert!(!e.bounds_range_proved(st));
}
