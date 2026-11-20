use super::*;
use crate::ir::{IrType, MemKind};
use crate::ir_schedule;

/// `if (a.length CMP c) return a[h & (L.length - k)]`, `L` being `a` or
/// another array. Returns whether the load's check is range-proven away.
fn mask_proved(guard: Option<(CmpOp, i64)>, k: i64, same_array: bool) -> bool {
    let mut g = Graph::empty(0);
    let s = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = s;
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![s], None);
    let m0 = g.add(Op::Proj(1), IrType::Memory, vec![s], None);
    let arr = g.add(Op::Param(0), IrType::Ref, vec![s], None);
    let other = g.add(Op::Param(1), IrType::Ref, vec![s], None);
    let h = g.add(Op::Param(2), IrType::Int, vec![s], None);
    let len = g.add(Op::ArrayLength, IrType::Int, vec![c0, m0, arr], None);
    let body = match guard {
        Some((op, c)) => {
            let cc = g.add(Op::Const(c), IrType::Int, vec![], None);
            let cmp = g.add(Op::Cmp(op), IrType::Int, vec![len, cc], None);
            let iff = g.add(Op::If, IrType::Control, vec![c0, cmp], None);
            let t = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
            let _f = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
            t
        }
        None => c0,
    };
    let mask_len = if same_array {
        len
    } else {
        g.add(Op::ArrayLength, IrType::Int, vec![c0, m0, other], None)
    };
    let kc = g.add(Op::Const(k), IrType::Int, vec![], None);
    let m = g.add(Op::Sub, IrType::Int, vec![mask_len, kc], None);
    let idx = g.add(Op::And, IrType::Int, vec![h, m], None);
    let ld = g.add(
        Op::ArrayLoad(MemKind::Int),
        IrType::Int,
        vec![body, m0, arr, idx],
        Some(0),
    );
    g.exit = g.add(Op::Return, IrType::Void, vec![body, ld], Some(1));
    let sched = ir_schedule::schedule(&g);
    let e = analyze(&g, &sched);
    e.bounds_elided(ld) && e.bounds_range_proved(ld)
}

/// `HashMap.getNode`: `(n = tab.length) > 0 && tab[(n - 1) & hash]`.
#[test]
fn a_power_of_two_table_index_under_a_non_empty_test_is_in_bounds() {
    assert!(mask_proved(Some((CmpOp::Gt, 0)), 1, true));
    assert!(mask_proved(Some((CmpOp::Ge, 1)), 1, true));
    assert!(
        mask_proved(Some((CmpOp::Gt, 4)), 5, true),
        "any k >= 1 is below"
    );
    // ...but `len - 5 >= 0` needs `len >= 5`, not `len > 0`.
    assert!(!mask_proved(Some((CmpOp::Gt, 0)), 5, true));
}

/// The twins that must keep the check: an empty table makes the mask -1
/// (no non-empty test), `len >= 0` says nothing, the mask of ANOTHER
/// array's length, and `k == 0` (`h & len` reaches `len`).
#[test]
fn a_mask_the_proof_cannot_close_keeps_its_check() {
    assert!(!mask_proved(None, 1, true));
    assert!(!mask_proved(Some((CmpOp::Ge, 0)), 1, true));
    assert!(!mask_proved(Some((CmpOp::Gt, 0)), 1, false));
    assert!(!mask_proved(Some((CmpOp::Gt, 0)), 0, true));
}

