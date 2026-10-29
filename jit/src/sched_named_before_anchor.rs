//! j27 wave 5 (lane STRPIN): a pure value a deopt frame names is scheduled
//! before the deopt point that publishes the frame, inside one block.
//!
//! # The defect
//!
//! `ir_schedule` places data nodes block by block in id order and then sorts
//! each block topologically. A pure node with no data user inside the block
//! keeps its id position. A pass that REPLACES a value by a newly created node
//! (the boolean phi fold, `ir_optimize::bool_branch`, which turns javac's
//! `iconst_1 / iconst_0` join into a negated compare) leaves the new node with
//! a high id, and when its only remaining reader is a frame state, nothing
//! pulls it ahead of the guard that reads that frame. The lowerer then reaches
//! the guard before the value has a home, describes the slot as `Unsupported`,
//! and a REQUIRED splice chain (`ir_lower` `splice_chain_for_site`) latches
//! "a spliced guard's required caller chain cannot be published": the whole
//! optimizing compile is lost. `Buffer.limit(I)` / `position(I)` spliced into
//! a caller hit it on every compile (`(newLimit > capacity | newLimit < 0)` is
//! the folded boolean on the operand stack at the cold throw arm's `ifeq`), so
//! the NIO setter loops of the CharsetDecoder page ran the single-pass body.
//!
//! # The rule
//!
//! For each anchor (a node that can deopt) in a block, every pure non-phi
//! value that a snapshot the anchor may publish names, and that sits after the
//! anchor in the same block, moves to just before it -- together with its
//! in-block inputs that also sit after the anchor. A value whose in-block
//! input after the anchor is not pure is left alone (moving it would cross an
//! effect). Moving a pure node EARLIER is always legal for its users (they stay
//! after it) and for every frame (its home is written sooner), which is the
//! rule `priority_sort_block` already relies on.
//!
//! `CRATONVM_JIT_IR_SCHED_NAMED_BEFORE_ANCHOR`, default ON, `=0` restores the
//! old order.

use std::collections::HashMap;

use crate::ir::{Graph, NodeId, Op, NO_NODE};

/// `CRATONVM_JIT_IR_SCHED_NAMED_BEFORE_ANCHOR` (j27 wave 5, lane STRPIN;
/// default ON, `=0` kills). Read once per schedule.
pub(crate) fn enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_SCHED_NAMED_BEFORE_ANCHOR")
}

/// Snapshot indices per bci, built once per schedule.
pub(crate) fn snapshots_by_bci(graph: &Graph) -> HashMap<usize, Vec<usize>> {
    let mut out: HashMap<usize, Vec<usize>> = HashMap::new();
    for (si, sp) in graph.safepoints.iter().enumerate() {
        out.entry(sp.bci).or_default().push(si);
    }
    out
}

/// The values the frames `anchor` may publish name: every snapshot at its
/// bytecode pc (flat or chain; over-approximating only orders more values
/// earlier) and its own per-copy snapshot.
fn named_by_anchor(
    graph: &Graph,
    anchor: NodeId,
    by_bci: &HashMap<usize, Vec<usize>>,
) -> Vec<NodeId> {
    let Some(n) = graph.nodes.get(anchor as usize) else {
        return Vec::new();
    };
    let mut sis: Vec<usize> = n
        .bytecode_pc
        .and_then(|pc| by_bci.get(&pc))
        .cloned()
        .unwrap_or_default();
    if let Some(si) = n.frame_snapshot {
        sis.push(si as usize);
    }
    let mut out = Vec::new();
    for si in sis {
        let Some(sp) = graph.safepoints.get(si) else {
            continue;
        };
        out.extend(sp.locals.iter().chain(sp.stack.iter()).copied().filter(|&v| v != NO_NODE));
    }
    out
}

fn is_movable(graph: &Graph, id: NodeId) -> bool {
    graph
        .nodes
        .get(id as usize)
        .is_some_and(|n| n.op.is_pure() && !matches!(n.op, Op::Phi))
}

/// The nodes to move ahead of position `p` so that `v` (at a position after
/// `p`) is computed before it: `v` and its in-block inputs after `p`, or
/// `None` when one of them is not movable.
fn closure_after(
    graph: &Graph,
    nodes: &[NodeId],
    pos: &HashMap<NodeId, usize>,
    p: usize,
    v: NodeId,
) -> Option<Vec<NodeId>> {
    let mut out: Vec<NodeId> = Vec::new();
    let mut work = vec![v];
    while let Some(id) = work.pop() {
        if out.contains(&id) {
            continue;
        }
        if !is_movable(graph, id) {
            return None;
        }
        out.push(id);
        for &inp in &graph.nodes[id as usize].inputs {
            match pos.get(&inp) {
                // A value computed from the anchor itself (a post-call frame
                // naming the call's result) is defined by it: never ahead.
                Some(&q) if q == p => return None,
                Some(&q) if q > p => work.push(inp),
                _ => {}
            }
        }
    }
    out.sort_by_key(|id| pos.get(id).copied().unwrap_or(usize::MAX));
    debug_assert!(out.iter().all(|id| nodes.contains(id)));
    Some(out)
}

/// Move the named values of each anchor of `nodes` (one block, already in a
/// valid topological order) ahead of it. Returns how many nodes moved.
pub(crate) fn hoist_named_before_anchors(
    graph: &Graph,
    nodes: &mut Vec<NodeId>,
    by_bci: &HashMap<usize, Vec<usize>>,
) -> usize {
    let mut moved = 0usize;
    let mut pos = positions(nodes);
    let mut i = 0usize;
    while i < nodes.len() {
        let anchor = nodes[i];
        let is_anchor = graph.nodes.get(anchor as usize).is_some_and(|n| {
            n.op != Op::Dead && !n.op.is_pure() && crate::ir_lower::can_deopt(graph, n)
        });
        if is_anchor {
            let n = hoist_for_anchor(graph, nodes, &pos, i, by_bci);
            if n > 0 {
                moved += n;
                i += n;
                pos = positions(nodes);
            }
        }
        i += 1;
    }
    moved
}

fn positions(nodes: &[NodeId]) -> HashMap<NodeId, usize> {
    nodes.iter().enumerate().map(|(k, &id)| (id, k)).collect()
}

/// [`hoist_named_before_anchors`] for the anchor at position `p`: the number
/// of nodes inserted before it (the anchor moves right by as many).
fn hoist_for_anchor(
    graph: &Graph,
    nodes: &mut Vec<NodeId>,
    pos: &HashMap<NodeId, usize>,
    p: usize,
    by_bci: &HashMap<usize, Vec<usize>>,
) -> usize {
    let named = named_by_anchor(graph, nodes[p], by_bci);
    if named.is_empty() {
        return 0;
    }
    let mut take: Vec<NodeId> = Vec::new();
    for v in named {
        if !pos.get(&v).is_some_and(|&q| q > p) || take.contains(&v) {
            continue;
        }
        if let Some(c) = closure_after(graph, nodes, pos, p, v) {
            for id in c {
                if !take.contains(&id) {
                    take.push(id);
                }
            }
        }
    }
    if take.is_empty() {
        return 0;
    }
    take.sort_by_key(|id| pos[id]);
    nodes.retain(|id| !take.contains(id));
    let n = take.len();
    nodes.splice(p..p, take);
    n
}

#[cfg(test)]
mod tests {
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
}
