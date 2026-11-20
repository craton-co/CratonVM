// `pub(crate)` in `ea_ir_bridge`; see the file preamble for why the
// crate root's glob cannot carry it.
use crate::ea_ir_bridge::narrow_array_value_fits;

use crate::ir::{Graph, IrType, MemKind, NodeId, Op};

fn graph() -> Graph {
    let mut g = Graph::empty(0);
    g.exit = 0;
    g
}

fn konst(g: &mut Graph, v: i64) -> NodeId {
    g.add(Op::Const(v), IrType::Int, vec![], None)
}

#[test]
fn a_constant_must_be_in_the_slot_range() {
    let mut g = graph();
    let c300 = konst(&mut g, 300);
    let c44 = konst(&mut g, 44);
    let c_neg = konst(&mut g, -1);
    assert!(
        !narrow_array_value_fits(&g, c300, MemKind::Byte, Some(8)),
        "300 does not fit a byte"
    );
    assert!(narrow_array_value_fits(&g, c44, MemKind::Byte, Some(8)));
    assert!(
        !narrow_array_value_fits(&g, c_neg, MemKind::Char, Some(5)),
        "char is unsigned"
    );
    assert!(narrow_array_value_fits(&g, c300, MemKind::Short, Some(9)));
    // Wide element kinds store the whole value.
    assert!(narrow_array_value_fits(&g, c300, MemKind::Int, Some(10)));
}

#[test]
fn the_builders_own_narrowing_shapes_fit() {
    let mut g = graph();
    let x = g.add(Op::Param(0), IrType::Int, vec![], None);
    let k24 = konst(&mut g, 24);
    let shl = g.add(Op::Shl, IrType::Int, vec![x, k24], None);
    let i2b = g.add(Op::Shr, IrType::Int, vec![shl, k24], None);
    assert!(narrow_array_value_fits(&g, i2b, MemKind::Byte, Some(8)));
    assert!(!narrow_array_value_fits(&g, i2b, MemKind::Char, Some(5)));
    let mask = konst(&mut g, 0xFFFF);
    let i2c = g.add(Op::And, IrType::Int, vec![x, mask], None);
    assert!(narrow_array_value_fits(&g, i2c, MemKind::Char, Some(5)));
    // An unnarrowed parameter fits no narrow slot.
    assert!(!narrow_array_value_fits(&g, x, MemKind::Short, Some(9)));
}

#[test]
fn a_boolean_slot_takes_only_zero_or_one() {
    let mut g = graph();
    let one = konst(&mut g, 1);
    let two = konst(&mut g, 2);
    assert!(narrow_array_value_fits(&g, one, MemKind::Byte, Some(4)));
    assert!(
        !narrow_array_value_fits(&g, two, MemKind::Byte, Some(4)),
        "boolean[] stores value & 1"
    );
}
