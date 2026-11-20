use super::*;
use crate::ir::{
    Graph, IrType, MemKind, NodeId, Op, SafepointSnapshot, SpliceScopeChain, SpliceScopeFrame,
};

fn chain(bci: usize, local: NodeId, caller_local: NodeId) -> SafepointSnapshot {
    let mut sp = SafepointSnapshot::new(bci, vec![local], vec![caller_local], Vec::new());
    sp.splice_scope = Some(std::sync::Arc::new(SpliceScopeChain {
        scopes: vec![
            SpliceScopeFrame {
                class_id: 0,
                method_key: "P.f:()V".to_string(),
                bci: 2,
                code_len: 5,
                max_locals: 1,
                locals_len: 0,
                stack_len: 0,
            },
            SpliceScopeFrame {
                class_id: 0,
                method_key: String::new(),
                bci: 1,
                code_len: 0,
                max_locals: 0,
                locals_len: 1,
                stack_len: 0,
            },
        ],        }));
    sp    }

/// A graph with a trapping load at combined pc 12 of the body spliced for
/// the `invoke` at bci 1, a flat frame at 1, and two chain frames at 12:
/// one naming a live value and one naming an object escape analysis
/// removed. Returns `(graph, alloc)`.
fn graph() -> (Graph, NodeId) {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let p = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![ctrl, mem, p, zero],
        Some(12),
    );
    g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, load], Some(4));
    let alloc = g.add(Op::Const(3), IrType::Ref, vec![], None);
    g.push_safepoint(SafepointSnapshot::new(1, vec![p], vec![p], Vec::new()));
    g.push_safepoint(chain(12, p, p));
    g.push_safepoint(chain(12, alloc, p));
    (g, alloc)
}

/// An unnamed chain frame never pins an allocation (the lowerer falls back
/// to the flat frame for any chain naming a removed object); the flat
/// frame still does.
#[test]
fn an_unnamed_chain_snapshot_does_not_pin_what_it_names() {
    let (g, alloc) = graph();
    assert!(!ea_snapshot_names(&g, alloc), "named only by a chain frame");
    let p = g.safepoints[0].locals[0];
    assert!(
        ea_snapshot_names(&g, p),
        "the flat frame at the invoke names it"
    );
}

/// After escape analysis removed the object, the post-EA prune drops the
/// chain frame naming it and keeps the live one and the flat fall-back.
#[test]
fn the_post_ea_prune_drops_a_chain_frame_naming_a_removed_object() {
    let (mut g, alloc) = graph();
    g.kill(alloc);
    let dropped = ir_prune_unconsumable_snapshots(&mut g, &[(10, 15, 1)]);
    assert_eq!(dropped, 1);
    assert_eq!(g.safepoints.len(), 2);
    assert!(!g.safepoints[0].is_splice_state());
    assert_eq!(g.safepoints[0].bci, 1);
    assert!(g.safepoints[1].is_splice_state());
    assert_eq!(g.safepoints[1].bci, 12);
}
