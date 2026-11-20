use super::*;
use crate::ir::{IrType, NO_NODE};
use crate::ir_schedule;

/// `for (i = 0; i <test> a.length - k; i++) a[i + j] = 1;` (`<` or, with
/// `non_strict`, `<=`), the body on the true edge. Returns whether the
/// store's bounds check is proven away by the range pass.
fn offset_proved(k: i64, non_strict: bool, j: i64) -> bool {
    let mut g = Graph::empty(0);
    let s = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = s;
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![s], None);
    let m0 = g.add(Op::Proj(1), IrType::Memory, vec![s], None);
    let arr = g.add(Op::Param(0), IrType::Ref, vec![s], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let region = g.add(Op::Merge, IrType::Control, vec![c0], None);
    let iv = g.add(Op::Phi, IrType::Int, vec![region, zero], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let next = g.add(Op::Add, IrType::Int, vec![iv, one], None);
    g.nodes[iv as usize].inputs.push(next);
    let len = g.add(Op::ArrayLength, IrType::Int, vec![c0, m0, arr], None);
    let kc = g.add(Op::Const(k), IrType::Int, vec![], None);
    let x = g.add(Op::Sub, IrType::Int, vec![len, kc], None);
    let op = if non_strict { CmpOp::Le } else { CmpOp::Lt };
    let cmp = g.add(Op::Cmp(op), IrType::Int, vec![iv, x], None);
    let iff = g.add(Op::If, IrType::Control, vec![region, cmp], None);
    let body = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
    let exit = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
    g.nodes[region as usize].inputs.push(body);
    let jc = g.add(Op::Const(j), IrType::Int, vec![], None);
    let idx = g.add(Op::Add, IrType::Int, vec![iv, jc], None);
    let val = g.add(Op::Const(1), IrType::Int, vec![], None);
    let st = g.add(
        Op::ArrayStore(MemKind::Int),
        IrType::Void,
        vec![body, m0, arr, idx, val],
        Some(0),
    );
    g.exit = g.add(Op::Return, IrType::Void, vec![exit, iv], Some(1));
    let sched = ir_schedule::schedule(&g);
    let e = analyze(&g, &sched);
    e.bounds_elided(st) && e.bounds_range_proved(st)
}

/// `a[i + 1]` under `i < a.length - 1`, and every `j <= k` under
/// `i < a.length - k`.
#[test]
fn an_offset_within_the_margin_is_in_bounds() {
    assert!(offset_proved(1, false, 1));
    assert!(offset_proved(1, false, 0));
    assert!(offset_proved(3, false, 2));
    assert!(offset_proved(3, false, 3));
}

/// `j > k` can reach `a[a.length]`: `i = len - k - 1` gives
/// `i + k + 1 = len`.
#[test]
fn an_offset_past_the_margin_keeps_its_check() {
    assert!(!offset_proved(1, false, 2));
    assert!(!offset_proved(3, false, 4));
}

/// The non-strict test gives one less: `i <= a.length - k` covers
/// `j <= k - 1`.
#[test]
fn a_non_strict_test_covers_one_offset_less() {
    assert!(offset_proved(2, true, 1));
    assert!(!offset_proved(2, true, 2));
    assert!(!offset_proved(1, true, 1));
}

/// A zero margin (`i < a.length - 0`) proves `i` itself, and nothing past.
#[test]
fn a_zero_margin_proves_no_offset() {
    assert!(!offset_proved(0, false, 1));
}
