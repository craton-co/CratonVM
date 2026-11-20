use super::*;
use crate::ir::{Graph, IrType, MemKind, NodeId, Op, NO_NODE};

fn graph() -> (Graph, NodeId) {
    let mut g = Graph::empty(0);
    g.exit = 0;
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let join = g.add(Op::Merge, IrType::Control, vec![start, start], None);
    (g, join)
}

#[test]
fn a_phi_of_zero_and_one_fits_a_boolean_slot() {
    let (mut g, join) = graph();
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let phi = g.add(Op::Phi, IrType::Int, vec![join, one, zero], None);
    assert!(narrow_array_value_fits(&g, phi, MemKind::Byte, Some(4)));
    // A 2 among the inputs does not fit a boolean.
    let two = g.add(Op::Const(2), IrType::Int, vec![], None);
    let bad = g.add(Op::Phi, IrType::Int, vec![join, one, two], None);
    assert!(!narrow_array_value_fits(&g, bad, MemKind::Byte, Some(4)));
}

#[test]
fn a_nested_or_cyclic_phi_fits_only_if_every_leaf_does() {
    let (mut g, join) = graph();
    let c100 = g.add(Op::Const(100), IrType::Int, vec![], None);
    let c_neg = g.add(Op::Const(-100), IrType::Int, vec![], None);
    let inner = g.add(Op::Phi, IrType::Int, vec![join, c100, c_neg], None);
    // `outer = φ(inner, outer)`: a loop-carried φ re-feeding itself.
    let outer = g.add(Op::Phi, IrType::Int, vec![join, inner, NO_NODE], None);
    assert!(g.set_input(outer, 2, outer));
    assert!(narrow_array_value_fits(&g, outer, MemKind::Byte, Some(8)));
    assert!(
        !narrow_array_value_fits(&g, outer, MemKind::Char, Some(5)),
        "-100 is not a char"
    );
    // An undefined edge refuses.
    let undef = g.add(Op::Phi, IrType::Int, vec![join, c100, NO_NODE], None);
    assert!(!narrow_array_value_fits(&g, undef, MemKind::Byte, Some(8)));
}

#[test]
fn the_explicit_narrowing_ops_fit_their_own_kinds() {
    let (mut g, _) = graph();
    let x = g.add(Op::Param(0), IrType::Int, vec![], None);
    let b = g.add(Op::I2B, IrType::Int, vec![x], None);
    let c = g.add(Op::I2C, IrType::Int, vec![x], None);
    assert!(narrow_array_value_fits(&g, b, MemKind::Byte, Some(8)));
    assert!(narrow_array_value_fits(&g, b, MemKind::Short, Some(9)));
    assert!(!narrow_array_value_fits(&g, b, MemKind::Char, Some(5)));
    assert!(narrow_array_value_fits(&g, c, MemKind::Char, Some(5)));
    assert!(!narrow_array_value_fits(&g, c, MemKind::Short, Some(9)));
}

