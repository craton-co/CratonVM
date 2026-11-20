//! Round 12 wave 2, lane iropt: the shared users map `loop_body_with` reads
//! (LICM builds it once per visit, the range-predicate pass once per plant,
//! the unroller once per transform).
use super::*;

/// `Start -> pre -> region <-> (If -> back)`, `If -> exit -> Return`, with a
/// load anchored at the header. Returns `(graph, region, pre, back, load)`.
fn one_loop() -> (Graph, NodeId, NodeId, NodeId, NodeId) {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Void, vec![], None);
    g.entry = start;
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let pre = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let region = g.add(Op::Region, IrType::Control, vec![pre, NO_NODE], None);
    let base = g.add(Op::Param(0), IrType::Ref, vec![], None);
    let off = g.add(Op::Const(0), IrType::Int, vec![], None);
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![region, mem, base, off],
        None,
    );
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let cond = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![load, zero], None);
    let iff = g.add(Op::If, IrType::Control, vec![region, cond], None);
    let back = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
    let exit = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
    let ret = g.add(Op::Return, IrType::Void, vec![exit], None);
    g.exit = ret;
    assert!(g.set_input(region, 1, back));
    (g, region, pre, back, load)
}

/// LICM re-anchors a load between two headers of one visit. The map was
/// built before that, and the body read through it is still exact: the
/// map is only consulted for control adjacency, and a load is not control.
#[test]
fn a_users_map_built_before_a_hoist_still_gives_the_same_loop_body() {
    let (mut g, region, pre, back, load) = one_loop();
    let users = build_users(&g);
    let before = loop_body_with(&g, &users, region, pre, &[back]);
    assert!(
        before.contains(&load),
        "the header-anchored load is in the body"
    );
    assert_eq!(before, loop_body(&g, region, pre, &[back]));

    // What the hoist does to it: control (and memory) to the pre-header.
    assert!(g.set_input(load, 0, pre));
    let stale = loop_body_with(&g, &users, region, pre, &[back]);
    let fresh = loop_body(&g, region, pre, &[back]);
    assert_eq!(stale, fresh);
    assert!(!fresh.contains(&load), "a hoisted load has left the body");
}

/// A map that predates a node is not trusted: here the node is a control
/// pass-through on the back edge, which a stale map would never reach, so
/// the latch would fall out of the body.
#[test]
fn a_users_map_shorter_than_the_arena_is_rebuilt_not_trusted() {
    let (mut g, region, pre, back, _) = one_loop();
    let users = build_users(&g);
    let latch = g.add(Op::Merge, IrType::Control, vec![back], None);
    assert!(g.set_input(region, 1, latch));
    let body = loop_body_with(&g, &users, region, pre, &[latch]);
    assert!(body.contains(&latch) && body.contains(&back));
    assert_eq!(body, loop_body(&g, region, pre, &[latch]));
}
