use super::*;
use crate::ir::{Graph, IrType, MemKind, Op};

/// `o = new O(); o.x = 5; int v = o.x; Reference.reachabilityFence(o);
/// return v;` with the fence built as the builder builds it
/// (`KeepAlive [ctrl, o]`). `finalizer_free` says whether the compile
/// resolved class 7 with no finalizer. Returns the graph and
/// `[o, fence, return, five]`.
fn fenced_fresh_object(finalizer_free: bool) -> (Graph, [ir::NodeId; 4]) {
    let mut g = Graph::empty(16);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let o = g.add(
        Op::New {
            class_id: 7,
            num_fields: 1,
        },
        IrType::Ref,
        vec![ctrl, mem],
        Some(0),
    );
    let off0 = g.add(Op::Const(0), IrType::Int, vec![], None);
    let five = g.add(Op::Const(5), IrType::Int, vec![], None);
    let st = g.add(
        Op::Store(MemKind::Int),
        IrType::Memory,
        vec![ctrl, mem, o, off0, five],
        Some(4),
    );
    let ld = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![ctrl, st, o, off0],
        Some(8),
    );
    let fence = g.add(Op::KeepAlive, IrType::Void, vec![ctrl, o], Some(12));
    let ret = g.add(Op::Return, IrType::Int, vec![ctrl, ld], Some(16));
    g.entry = start;
    g.exit = ret;
    if finalizer_free {
        g.finalizer_free_new_classes.insert(7);
    }
    (g, [o, fence, ret, five])
}

/// The fence of a finalizer-free fresh object is a non-escaping use: the
/// object is scalar-replaced, the load answers the stored value, and the
/// fence dies with the allocation (it would otherwise name `Op::Dead`).
#[test]
fn a_fenced_finalizer_free_allocation_is_scalar_replaced_with_its_fence() {
    let (mut g, [o, fence, ret, five]) = fenced_fresh_object(true);
    let (ea, id_map) = escape_analysis_from_ir(&g);
    assert_eq!(
        ea.nodes[id_map[fence as usize]].op,
        escape_analysis::Op::KeepAlive
    );
    assert_eq!(
        ea.nodes[id_map[fence as usize]].inputs,
        vec![id_map[o as usize]]
    );
    let result = escape_analysis::analyze_escapes(&ea);
    assert!(
        result
            .scalar_replaceable
            .iter()
            .any(|i| i.alloc_node == id_map[o as usize]),
        "{:?}",
        result.scalar_refusals
    );
    apply_ea_to_ir(&mut g, &id_map, &result);
    assert_eq!(g.nodes[o as usize].op, Op::Dead);
    assert_eq!(g.nodes[fence as usize].op, Op::Dead);
    assert_eq!(g.nodes[ret as usize].inputs.get(1).copied(), Some(five));
}

/// A class the compile did not resolve as finalizer-free (it has one, or
/// nobody said): the fence stays the escaping call it always was, so the
/// object keeps its allocation and the fence keeps naming it.
#[test]
fn a_fence_of_a_possibly_finalizable_allocation_still_escapes_it() {
    let (mut g, [o, fence, _, _]) = fenced_fresh_object(false);
    let (ea, id_map) = escape_analysis_from_ir(&g);
    assert_eq!(
        ea.nodes[id_map[fence as usize]].op,
        escape_analysis::Op::Call
    );
    let result = escape_analysis::analyze_escapes(&ea);
    assert!(!result
        .scalar_replaceable
        .iter()
        .any(|i| i.alloc_node == id_map[o as usize]));
    apply_ea_to_ir(&mut g, &id_map, &result);
    assert!(matches!(g.nodes[o as usize].op, Op::New { .. }));
    assert_eq!(g.nodes[fence as usize].op, Op::KeepAlive);
    assert_eq!(g.nodes[fence as usize].inputs.get(1).copied(), Some(o));
}
