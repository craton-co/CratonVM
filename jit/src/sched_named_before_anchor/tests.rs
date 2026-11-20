use super::*;
use crate::ir::{CmpOp, IrType, SafepointSnapshot};

fn bare() -> Graph {
    let mut g = Graph::empty(0);
    g.entry = NO_NODE;
    g
}

/// The defect's shape: a guard at pc 5 whose snapshot names a compare
/// created after it (a higher id, no data user). The compare must come
/// first; with nothing named the order is untouched.
#[test]
fn named_compare_moves_ahead_of_its_guard() {
    let mut g = bare();
    let a = g.add(Op::Param(0), IrType::Int, vec![], None);
    let b = g.add(Op::Param(1), IrType::Int, vec![], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let guard = g.add(Op::Guard { bci: 5 }, IrType::Void, vec![NO_NODE, zero], Some(5));
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let late = g.add(Op::Add, IrType::Int, vec![b, one], None);
    let cmp = g.add(Op::Cmp(CmpOp::Gt), IrType::Int, vec![a, late], Some(2));
    g.safepoints.push(SafepointSnapshot::new(5, vec![a, b], vec![cmp], vec![]));
    let by_bci = snapshots_by_bci(&g);
    let mut nodes = vec![a, b, zero, guard, one, late, cmp];
    let moved = hoist_named_before_anchors(&g, &mut nodes, &by_bci);
    let at = |n: NodeId| nodes.iter().position(|&x| x == n).unwrap();
    assert_eq!(moved, 3, "cmp and its two in-block inputs after the guard");
    assert!(at(cmp) < at(guard), "{nodes:?}");
    assert!(at(late) < at(cmp) && at(one) < at(late), "inputs stay before users: {nodes:?}");
    assert_eq!(nodes.len(), 7);

    let mut g2 = bare();
    let z = g2.add(Op::Const(0), IrType::Int, vec![], None);
    let gd = g2.add(Op::Guard { bci: 5 }, IrType::Void, vec![NO_NODE, z], Some(5));
    let c = g2.add(Op::Const(7), IrType::Int, vec![], None);
    let mut plain = vec![z, gd, c];
    assert_eq!(hoist_named_before_anchors(&g2, &mut plain, &snapshots_by_bci(&g2)), 0);
    assert_eq!(plain, vec![z, gd, c], "an unnamed value keeps its place");
}

/// A named value computed from an effect after the anchor cannot move.
#[test]
fn named_value_over_an_effect_stays() {
    let mut g = bare();
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let guard = g.add(Op::Guard { bci: 5 }, IrType::Void, vec![NO_NODE, zero], Some(5));
    let other = g.add(Op::Guard { bci: 9 }, IrType::Void, vec![NO_NODE, zero], Some(9));
    let v = g.add(Op::Neg, IrType::Int, vec![other], None);
    g.safepoints.push(SafepointSnapshot::new(5, vec![v], vec![], vec![]));
    let mut nodes = vec![zero, guard, other, v];
    assert_eq!(hoist_named_before_anchors(&g, &mut nodes, &snapshots_by_bci(&g)), 0);
    assert_eq!(nodes, vec![zero, guard, other, v]);
}

/// A named value computed from the anchor's own result (a post-call frame
/// naming the call's value) is never moved ahead of the anchor.
#[test]
fn value_of_the_anchor_itself_stays() {
    let mut g = bare();
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let anchor = g.add(Op::Guard { bci: 5 }, IrType::Void, vec![NO_NODE, zero], Some(5));
    let v = g.add(Op::Neg, IrType::Int, vec![anchor], None);
    g.safepoints.push(SafepointSnapshot::new(5, vec![], vec![v], vec![]));
    let mut nodes = vec![zero, anchor, v];
    assert_eq!(hoist_named_before_anchors(&g, &mut nodes, &snapshots_by_bci(&g)), 0);
    assert_eq!(nodes, vec![zero, anchor, v]);
}
