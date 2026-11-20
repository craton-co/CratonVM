use super::*;
use crate::ir::{IrType, MemKind};
use crate::ir_schedule;

#[derive(Clone, Copy)]
enum Fact {
    None,
    /// `if (c OP a.length)`, the load on the true edge.
    Edge(CmpOp, i64),
    /// `Guard(c OP a.length)` in the entry block, the load one branch
    /// further down (a block the guard's strictly dominates).
    Guard(CmpOp, i64),
    /// The load reads a `new int[c]` instead of the parameter.
    Fresh(i64),
}

fn proved(fact: Fact, mask: i64) -> bool {
    let mut g = Graph::empty(0);
    let s = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = s;
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![s], None);
    let m0 = g.add(Op::Proj(1), IrType::Memory, vec![s], None);
    let param = g.add(Op::Param(0), IrType::Ref, vec![s], None);
    let h = g.add(Op::Param(1), IrType::Int, vec![s], None);
    let sel = g.add(Op::Param(2), IrType::Int, vec![s], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let mut arr = param;
    let body = match fact {
        Fact::None => c0,
        Fact::Edge(op, c) => {
            let len = g.add(Op::ArrayLength, IrType::Int, vec![c0, m0, param], None);
            let cc = g.add(Op::Const(c), IrType::Int, vec![], None);
            let cmp = g.add(Op::Cmp(op), IrType::Int, vec![cc, len], None);
            let iff = g.add(Op::If, IrType::Control, vec![c0, cmp], None);
            let t = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
            let _f = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
            t
        }
        Fact::Guard(op, c) => {
            let len = g.add(Op::ArrayLength, IrType::Int, vec![c0, m0, param], None);
            let cc = g.add(Op::Const(c), IrType::Int, vec![], None);
            let cmp = g.add(Op::Cmp(op), IrType::Int, vec![cc, len], None);
            let _ = g.add(Op::Guard { bci: 0 }, IrType::Void, vec![c0, cmp], Some(0));
            let ne = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![sel, zero], None);
            let iff = g.add(Op::If, IrType::Control, vec![c0, ne], None);
            let t = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
            let _f = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
            t
        }
        Fact::Fresh(c) => {
            let cc = g.add(Op::Const(c), IrType::Int, vec![], None);
            arr = g.add(
                Op::NewArray {
                    element_type: 10,
                    component_class_id: 0,
                },
                IrType::Ref,
                vec![c0, m0, cc],
                None,
            );
            c0
        }
    };
    let mk = g.add(Op::Const(mask), IrType::Int, vec![], None);
    let idx = g.add(Op::And, IrType::Int, vec![h, mk], None);
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

#[test]
fn const_mask_of_reads_either_operand_and_only_non_negative_masks() {
    let mut g = Graph::empty(0);
    let s = g.add(Op::Start, IrType::Control, vec![], None);
    let p = g.add(Op::Param(0), IrType::Int, vec![s], None);
    let seven = g.add(Op::Const(7), IrType::Int, vec![], None);
    let neg = g.add(Op::Const(-1), IrType::Int, vec![], None);
    let a = g.add(Op::And, IrType::Int, vec![p, seven], None);
    let b = g.add(Op::And, IrType::Int, vec![seven, p], None);
    let c = g.add(Op::And, IrType::Int, vec![p, neg], None);
    let d = g.add(Op::Or, IrType::Int, vec![p, seven], None);
    assert_eq!(const_mask_of(&g, a), Some(7));
    assert_eq!(const_mask_of(&g, b), Some(7));
    assert_eq!(const_mask_of(&g, c), None, "a negative mask bounds nothing");
    assert_eq!(const_mask_of(&g, d), None);
}

/// The mask predicate's guard `7 < a.length` proves `a[h & 7]` (and a
/// smaller mask) one block down.
#[test]
fn the_mask_predicate_guard_proves_the_masked_access() {
    assert!(proved(Fact::Guard(CmpOp::Lt, 7), 7));
    assert!(proved(Fact::Guard(CmpOp::Lt, 7), 3));
    assert!(
        !proved(Fact::Guard(CmpOp::Lt, 7), 8),
        "a wider mask is not covered"
    );
    // `8 <= a.length` is `7 < a.length`.
    assert!(proved(Fact::Guard(CmpOp::Le, 8), 7));
    assert!(!proved(Fact::Guard(CmpOp::Le, 7), 7));
}

#[test]
fn an_if_edge_on_a_constant_proves_the_masked_access() {
    assert!(proved(Fact::Edge(CmpOp::Lt, 15), 15));
    assert!(proved(Fact::Edge(CmpOp::Le, 16), 15));
    assert!(!proved(Fact::Edge(CmpOp::Le, 15), 15));
    assert!(!proved(Fact::None, 0));
}

#[test]
fn a_fresh_array_of_constant_length_proves_a_smaller_mask() {
    assert!(proved(Fact::Fresh(8), 7));
    assert!(!proved(Fact::Fresh(7), 7));
}

