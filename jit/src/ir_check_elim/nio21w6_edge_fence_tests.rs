use super::*;
use crate::ir::IrType;
use crate::ir_schedule;

struct Site {
    /// The anchor's normal continuation (`Proj(1)` of its last `If`).
    normal: NodeId,
    /// The true edge's control (`Proj(0)`).
    caught: NodeId,
}

/// `t = CallThrew { site, 0 } [ctrl, anchor]`, `If(t)`, as the builder
/// plants it (`IrBuilder::plant_implicit_throw_edges`).
fn plant(g: &mut Graph, ctrl: NodeId, anchor: NodeId, site: u32) -> Site {
    let t = g.add(
        Op::CallThrew { site, index: 0 },
        IrType::Int,
        vec![ctrl, anchor],
        Some(0),
    );
    let iff = g.add(Op::If, IrType::Control, vec![ctrl, t], Some(0));
    let caught = g.add(Op::Proj(0), IrType::Control, vec![iff], Some(0));
    let normal = g.add(Op::Proj(1), IrType::Control, vec![iff], Some(0));
    let _exc = g.add(Op::CaughtException, IrType::Ref, vec![caught], None);
    Site { normal, caught }
}

struct Fixture {
    g: Graph,
    /// The access inside the `try`, the anchor.
    anchor: NodeId,
    /// `a.length` in the handler.
    handler_len: NodeId,
    /// `a[i]` in the handler.
    handler_load: NodeId,
    /// `a[i]` after the `try`, on the normal path.
    after_load: NodeId,
}

/// `try { x = a[i]; } catch (RuntimeException e) { a.length; a[i]; }
/// a[i];`, with `pre` an `a.length` read BEFORE the anchor in its block.
fn single_site(pre: bool) -> Fixture {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let arr = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let i = g.add(Op::Param(1), IrType::Int, vec![start], None);
    if pre {
        let _ = g.add(Op::ArrayLength, IrType::Int, vec![c0, m0, arr], Some(0));
    }
    let anchor = g.add(
        Op::ArrayLoad(MemKind::Int),
        IrType::Int,
        vec![c0, m0, arr, i],
        Some(1),
    );
    let s = plant(&mut g, c0, anchor, 0);
    let handler_len = g.add(
        Op::ArrayLength,
        IrType::Int,
        vec![s.caught, m0, arr],
        Some(5),
    );
    let handler_load = g.add(
        Op::ArrayLoad(MemKind::Int),
        IrType::Int,
        vec![s.caught, m0, arr, i],
        Some(6),
    );
    let _hret = g.add(
        Op::Return,
        IrType::Void,
        vec![s.caught, handler_load],
        Some(7),
    );
    let after_load = g.add(
        Op::ArrayLoad(MemKind::Int),
        IrType::Int,
        vec![s.normal, m0, arr, i],
        Some(3),
    );
    g.exit = g.add(
        Op::Return,
        IrType::Void,
        vec![s.normal, after_load],
        Some(4),
    );
    Fixture {
        g,
        anchor,
        handler_len,
        handler_load,
        after_load,
    }
}

/// The page's first snippet: in the handler `a` IS null (or `i` out of
/// range), so neither check there may lean on the anchor's.
#[test]
fn a_handler_keeps_the_checks_its_trapping_access_failed() {
    let f = single_site(false);
    let sched = ir_schedule::schedule(&f.g);
    let e = analyze(&f.g, &sched);
    assert!(!e.null_elided(f.anchor) && !e.bounds_elided(f.anchor));
    assert!(
        !e.null_elided(f.handler_len),
        "a.length in an NPE handler of a[i] must keep its null check"
    );
    // Its null check MAY go: the handler's own `a.length` (bci 5) ran
    // first in the same block and dereferenced `a` -- a fact from INSIDE
    // the handler, not the anchor's. Its bounds check may not: nothing in
    // the handler checked `(a, i)`.
    assert!(
        !e.bounds_elided(f.handler_load),
        "a[i] in an AIOOBE handler of a[i] must keep its bounds check"
    );
}

/// The fence costs the passing path nothing: past the chain's false edge
/// the anchor passed, and both facts hold.
#[test]
fn the_normal_continuation_keeps_the_anchors_facts() {
    let f = single_site(false);
    let sched = ir_schedule::schedule(&f.g);
    let e = analyze(&f.g, &sched);
    assert!(
        e.null_elided(f.after_load),
        "a was dereferenced by the passing anchor"
    );
    assert!(
        e.bounds_elided(f.after_load),
        "(a, i) was checked by the passing anchor"
    );
}

/// A dereference BEFORE the anchor ran on the failing path too.
#[test]
fn a_fact_from_before_the_anchor_still_reaches_the_handler() {
    let f = single_site(true);
    let sched = ir_schedule::schedule(&f.g);
    let e = analyze(&f.g, &sched);
    assert!(
        e.null_elided(f.handler_len),
        "a.length before the try proved a non-null"
    );
    assert!(
        !e.bounds_elided(f.handler_load),
        "but nothing before the try checked (a, i)"
    );
}

/// Two sites share one handler, a merge neither site's true-edge block
/// dominates -- the case "drop the facts under a `Proj(0)`" misses: the
/// first anchor's block still dominates the handler.
#[test]
fn a_handler_shared_by_two_sites_keeps_its_checks() {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let a = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let b = g.add(Op::Param(1), IrType::Ref, vec![start], None);
    let i = g.add(Op::Param(2), IrType::Int, vec![start], None);
    let first = g.add(
        Op::ArrayLoad(MemKind::Int),
        IrType::Int,
        vec![c0, m0, a, i],
        Some(1),
    );
    let s1 = plant(&mut g, c0, first, 0);
    let second = g.add(
        Op::ArrayLoad(MemKind::Int),
        IrType::Int,
        vec![s1.normal, m0, b, i],
        Some(2),
    );
    let s2 = plant(&mut g, s1.normal, second, 1);
    let handler = g.add(
        Op::Merge,
        IrType::Control,
        vec![s1.caught, s2.caught],
        Some(9),
    );
    let len = g.add(Op::ArrayLength, IrType::Int, vec![handler, m0, a], Some(9));
    let _hret = g.add(Op::Return, IrType::Void, vec![handler, len], Some(10));
    g.exit = g.add(Op::Return, IrType::Void, vec![s2.normal, second], Some(3));
    let sched = ir_schedule::schedule(&g);
    let e = analyze(&g, &sched);
    assert!(
        !e.null_elided(len),
        "the first site's failure enters the shared handler with a null"
    );
}

/// No `CallThrew`, no fence: the plain dominance answers are unchanged.
#[test]
fn a_graph_without_edges_has_no_fences() {
    let mut f = single_site(false);
    let sched = ir_schedule::schedule(&f.g);
    let fences = exception_edge_fences(&f.g, &sched);
    assert_eq!(fences.len(), 1);
    assert!(
        fences[&f.anchor].is_some(),
        "the builder's shape has a clean continuation"
    );
    for n in &mut f.g.nodes {
        if matches!(n.op, Op::CallThrew { .. }) {
            n.op = Op::Const(0);
        }
    }
    assert!(exception_edge_fences(&f.g, &sched).is_empty());
}
