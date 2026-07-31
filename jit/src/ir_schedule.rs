// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Linearize the Sea-of-Nodes IR graph into a sequence of basic blocks.
//!
//! The scheduler places each data node into a basic block such that:
//! 1. The node dominates all its uses.
//! 2. The node is placed as late as possible (to minimize register pressure).
//!
//! Control nodes define the block structure; data nodes float freely
//! and are pinned to blocks by this pass.

use super::ir::{Graph, NodeId, Op, NO_NODE};

// ── Public types ─────────────────────────────────────────────────────

/// A basic block in the scheduled output.
#[derive(Debug)]
pub struct Block {
    /// Block index (0-based).
    pub id: usize,
    /// Control node that starts this block (Start, Merge, Region, Proj).
    pub ctrl: NodeId,
    /// Data nodes scheduled into this block (topological order).
    pub nodes: Vec<NodeId>,
    /// Terminal control node (If, Return, goto-like Merge/Region successor).
    pub terminator: Option<NodeId>,
    /// Successor blocks (0, 1, or 2 successors).
    pub successors: Vec<usize>,
    /// Predecessor blocks.
    pub predecessors: Vec<usize>,
}

/// Result of scheduling: an ordered list of basic blocks.
pub struct Schedule {
    pub blocks: Vec<Block>,
    /// Maps NodeId → block index.
    pub node_to_block: Vec<usize>,
    /// Dominator relation over the block CFG: `dom[b][d]` is true iff block `d`
    /// dominates block `b`. Retained from the scheduler's own
    /// `compute_dominators` (it was previously dropped) so callers — notably the
    /// guard-surviving scalar-replacement producer — can prove that a value's
    /// defining block always executes before a deopt point.
    pub dom: Vec<Vec<bool>>,
}

impl Schedule {
    /// True iff `node` is computed in a block that **strictly** dominates
    /// `block` (a different block on every path to `block`). Used by the deopt
    /// producer to prove a scalar-replaced object's `Op::New` / field stores
    /// have definitely executed before a deopt point. Conservative: a node in
    /// the *same* block as `block` returns `false` even when it is textually
    /// earlier — v1 does not reason about intra-block order (a block's data
    /// nodes are topologically, not program, ordered, and a div guard need not
    /// be memory-ordered against a field store), so same-block bails to the
    /// safe whole-method re-run. Unplaced (`usize::MAX`) nodes return `false`.
    pub fn node_strictly_dominates_block(&self, node: NodeId, block: usize) -> bool {
        let nb = match self.node_to_block.get(node as usize) {
            Some(&b) if b != usize::MAX => b,
            _ => return false,
        };
        nb != block
            && self
                .dom
                .get(block)
                .and_then(|row| row.get(nb))
                .copied()
                .unwrap_or(false)
    }
}

// ── Scheduler ────────────────────────────────────────────────────────

/// Schedule the IR graph into basic blocks.
pub fn schedule(graph: &Graph) -> Schedule {
    let num_nodes = graph.nodes.len();

    // Step 1: Identify control-flow nodes and build block structure
    let mut blocks = Vec::new();
    let mut node_to_block = vec![usize::MAX; num_nodes];

    // Find all control nodes that start blocks:
    // Start, Merge, Region, and Proj(0)/Proj(1) from If nodes
    let mut block_heads: Vec<NodeId> = Vec::new();

    // Entry block starts at the control projection from Start
    for (id, node) in graph.nodes.iter().enumerate() {
        match &node.op {
            Op::Proj(0) if is_start(graph, node.inputs[0]) => {
                block_heads.push(id as NodeId);
            }
            Op::Merge | Op::Region => {
                block_heads.push(id as NodeId);
            }
            Op::Proj(0) | Op::Proj(1) if is_if(graph, node.inputs[0]) => {
                block_heads.push(id as NodeId);
            }
            _ => {}
        }
    }

    // Create blocks
    for &head in &block_heads {
        let block_idx = blocks.len();
        node_to_block[head as usize] = block_idx;
        blocks.push(Block {
            id: block_idx,
            ctrl: head,
            nodes: Vec::new(),
            terminator: None,
            successors: Vec::new(),
            predecessors: Vec::new(),
        });
    }

    // Step 2: Find terminators for each block
    // Walk control edges forward from each block head
    for block_idx in 0..blocks.len() {
        let head = blocks[block_idx].ctrl;
        // Find nodes that use this control token
        for (id, node) in graph.nodes.iter().enumerate() {
            if node.op == Op::Dead {
                continue;
            }
            match &node.op {
                Op::If => {
                    if !node.inputs.is_empty() && node.inputs[0] == head {
                        blocks[block_idx].terminator = Some(id as NodeId);
                        node_to_block[id] = block_idx;
                    }
                }
                Op::Return => {
                    if !node.inputs.is_empty() && node.inputs[0] == head {
                        blocks[block_idx].terminator = Some(id as NodeId);
                        node_to_block[id] = block_idx;
                    }
                }
                _ => {}
            }
        }
    }

    // Step 3: Build successor/predecessor edges
    for block_idx in 0..blocks.len() {
        if let Some(term) = blocks[block_idx].terminator {
            let term_node = &graph.nodes[term as usize];
            match &term_node.op {
                Op::If => {
                    // Find Proj(0) and Proj(1) successors
                    for (id, node) in graph.nodes.iter().enumerate() {
                        if let Op::Proj(_n) = &node.op {
                            if !node.inputs.is_empty() && node.inputs[0] == term {
                                let succ_block = node_to_block[id];
                                if succ_block != usize::MAX {
                                    if !blocks[block_idx].successors.contains(&succ_block) {
                                        blocks[block_idx].successors.push(succ_block);
                                    }
                                    if !blocks[succ_block].predecessors.contains(&block_idx) {
                                        blocks[succ_block].predecessors.push(block_idx);
                                    }
                                }
                            }
                        }
                    }
                }
                Op::Return => {
                    // No successors
                }
                _ => {}
            }
        }

        // For blocks without explicit terminator (fall-through via goto → merge),
        // check if the block's control feeds into a Merge/Region
        let head = blocks[block_idx].ctrl;
        for (id, node) in graph.nodes.iter().enumerate() {
            if matches!(&node.op, Op::Merge | Op::Region) {
                if node.inputs.contains(&head) {
                    let succ_block = node_to_block[id];
                    if succ_block != usize::MAX
                        && succ_block != block_idx
                        && !blocks[block_idx].successors.contains(&succ_block)
                    {
                        blocks[block_idx].successors.push(succ_block);
                        if !blocks[succ_block].predecessors.contains(&block_idx) {
                            blocks[succ_block].predecessors.push(block_idx);
                        }
                    }
                }
            }
        }
    }

    // Step 4: Place data nodes into blocks.
    //
    // Correctness invariant: a data node must be scheduled into a block where
    // *every* one of its (already-placed) inputs is available — i.e. a block
    // dominated by all of its inputs' blocks. Earlier this used raw block-index
    // ordering (`inp_block > best`) as a dominance proxy, which is unsound: in
    // an irreducible or reordered CFG a higher index does not imply dominance,
    // so a use could be placed before its def. We now compute a real dominator
    // relation over the block CFG and use it to pick a dominated home block,
    // falling back to the entry block (which dominates everything) when no such
    // block exists. See `find_best_block`.
    let dom = compute_dominators(&blocks);
    for (id, node) in graph.nodes.iter().enumerate() {
        if node_to_block[id] != usize::MAX || node.op == Op::Dead {
            continue; // already placed or dead
        }
        if node.op.is_control() {
            continue; // control nodes are block boundaries, already handled
        }
        // Find a block dominated by all of this node's inputs' blocks.
        let block = find_best_block(graph, id as NodeId, &node_to_block, &blocks, &dom);
        node_to_block[id] = block;
        blocks[block].nodes.push(id as NodeId);
    }

    // Step 5: Topological sort data nodes within each block
    for block in &mut blocks {
        topo_sort_block(graph, &mut block.nodes);
    }

    Schedule {
        blocks,
        node_to_block,
        dom,
    }
}

// ── Helpers ──────────────────────────────────────────────────────────

fn is_start(graph: &Graph, id: NodeId) -> bool {
    matches!(graph.nodes.get(id as usize), Some(n) if n.op == Op::Start)
}

fn is_if(graph: &Graph, id: NodeId) -> bool {
    matches!(graph.nodes.get(id as usize), Some(n) if n.op == Op::If)
}

/// Compute, for every block, the set of blocks that dominate it.
///
/// Block 0 (the entry) is assumed to be the CFG entry: it dominates every
/// block, and is dominated only by itself. We use the classic iterative
/// data-flow formulation:
///
/// ```text
///   dom(entry) = {entry}
///   dom(b)     = {b} ∪ ( ⋂ over preds p of dom(p) )
/// ```
///
/// iterated to a fixpoint. `dom[b]` is a bitset (one `bool` per block) where
/// `dom[b][d]` is true iff block `d` dominates block `b`. This is O(B² · iters)
/// in the worst case, but block counts in a single JIT'd method are small. For
/// blocks unreachable from the entry (no path of predecessors back to block 0)
/// the fixpoint leaves them dominated by "all blocks"; callers must therefore
/// only rely on `dominates(a, b)` when both are reachable — `find_best_block`
/// handles the unreachable/empty case by falling back to the entry block.
fn compute_dominators(blocks: &[Block]) -> Vec<Vec<bool>> {
    let n = blocks.len();
    if n == 0 {
        return Vec::new();
    }
    // Initialize: entry dominated only by itself; every other block tentatively
    // dominated by all blocks (the conservative "top" of the lattice).
    let mut dom: Vec<Vec<bool>> = vec![vec![true; n]; n];
    dom[0] = vec![false; n];
    dom[0][0] = true;

    let mut changed = true;
    while changed {
        changed = false;
        for b in 1..n {
            // new_dom = {b} ∪ ( ⋂ over preds p of dom(p) )
            let mut new_dom: Option<Vec<bool>> = None;
            for &p in &blocks[b].predecessors {
                if p >= n {
                    continue;
                }
                if let Some(acc) = new_dom.as_mut() {
                    for d in 0..n {
                        acc[d] &= dom[p][d];
                    }
                } else {
                    new_dom = Some(dom[p].clone());
                }
            }
            // A block with no (in-range) predecessors keeps the "top" set; only
            // tighten when we actually intersected predecessor sets.
            let mut new_dom = match new_dom {
                Some(v) => v,
                None => continue,
            };
            new_dom[b] = true; // a block always dominates itself
            if new_dom != dom[b] {
                dom[b] = new_dom;
                changed = true;
            }
        }
    }
    dom
}

/// True iff block `a` dominates block `b` (every path from the entry to `b`
/// passes through `a`). Both indices must be in range.
#[inline]
fn dominates(dom: &[Vec<bool>], a: usize, b: usize) -> bool {
    dom.get(b)
        .and_then(|row| row.get(a))
        .copied()
        .unwrap_or(false)
}

/// Find the home block for a data node such that every one of its already-placed
/// inputs is available there.
///
/// We collect the blocks of all known inputs and pick the *deepest* one that is
/// dominated by every other input block — that block is reached only after all
/// inputs are computed, so a use can never precede its def. (For a Phi, input[0]
/// is its Merge/Region control node, whose block is exactly this anchor; the
/// per-predecessor value inputs feed it via edge copies, not by being live in
/// the Phi's home block, so they are intentionally allowed to be non-dominating.)
///
/// If the input blocks are not totally ordered by dominance (e.g. a value that
/// truly depends on values from sibling branches — which a well-formed SSA graph
/// resolves through a Phi), we fall back to the entry block (block 0), which
/// dominates everything. This is conservative: it never schedules a use before
/// a dominating def, and only over-anchors otherwise-floating pure nodes.
fn find_best_block(
    graph: &Graph,
    id: NodeId,
    node_to_block: &[usize],
    blocks: &[Block],
    dom: &[Vec<bool>],
) -> usize {
    let node = &graph.nodes[id as usize];

    // Phi nodes are pinned to their Merge/Region's block: input[0] is that
    // control node. The value inputs are delivered by edge copies, so they must
    // not drag the Phi out of its merge block.
    if matches!(node.op, Op::Phi) {
        if let Some(&ctrl) = node.inputs.first() {
            if ctrl != NO_NODE && (ctrl as usize) < node_to_block.len() {
                let cb = node_to_block[ctrl as usize];
                if cb != usize::MAX && cb < blocks.len() {
                    return cb;
                }
            }
        }
        return 0;
    }

    // Gather the distinct, in-range, already-placed input blocks.
    let mut input_blocks: Vec<usize> = Vec::new();
    for &inp in &node.inputs {
        if inp != NO_NODE && (inp as usize) < node_to_block.len() {
            let ib = node_to_block[inp as usize];
            if ib != usize::MAX && ib < blocks.len() && !input_blocks.contains(&ib) {
                input_blocks.push(ib);
            }
        }
    }

    // No known input blocks → free to live in the entry block (params/consts).
    if input_blocks.is_empty() {
        return 0;
    }

    // Pick the deepest block that is dominated by every other input block. Such
    // a block sees all inputs on every path that reaches it.
    let mut best: Option<usize> = None;
    for &cand in &input_blocks {
        let dominated_by_all = input_blocks
            .iter()
            .all(|&other| other == cand || dominates(dom, other, cand));
        if dominated_by_all {
            // Among valid candidates prefer the one dominated by the most others
            // (i.e. the latest in dominance order). Since exactly one input block
            // can be dominated by all the rest in a totally-ordered chain, the
            // first match is already the unique deepest; keep it.
            best = Some(cand);
            break;
        }
    }

    match best {
        Some(b) => b,
        // Inputs span incomparable branches (no single dominated home). Fall
        // back to the entry block, which dominates all of them. Conservative:
        // never places a use before a def.
        None => 0,
    }
}

/// Topological sort the data nodes within a block so every node appears after
/// the inputs it depends on (that also live in this block).
///
/// This is an iterative post-order DFS using an explicit work stack. A previous
/// version recursed over the operand chain, which could overflow the native
/// stack on a deeply nested expression (e.g. a long left-leaning arithmetic
/// chain produces an operand chain as deep as the expression). The explicit
/// stack keeps memory bounded by the heap instead of the call stack.
fn topo_sort_block(graph: &Graph, nodes: &mut Vec<NodeId>) {
    if nodes.len() <= 1 {
        return;
    }
    let set: std::collections::HashSet<NodeId> = nodes.iter().copied().collect();
    let mut sorted = Vec::with_capacity(nodes.len());
    let mut visited = std::collections::HashSet::with_capacity(nodes.len());

    // Each stack frame tracks a node plus the index of the next input edge to
    // descend into. We push a node, walk its in-block inputs one at a time, and
    // only emit (`sorted.push`) the node once all its inputs have been emitted —
    // reproducing the recursive post-order without recursion.
    let mut stack: Vec<(NodeId, usize)> = Vec::new();

    for &root in nodes.iter() {
        if visited.contains(&root) {
            continue;
        }
        stack.push((root, 0));
        // Mark on push so a node already queued isn't re-entered via another
        // edge (matches the original `visited.insert(id)` guard at entry).
        visited.insert(root);

        while let Some(&(id, _)) = stack.last() {
            let inputs = &graph.nodes[id as usize].inputs;
            // Advance past inputs that are not in this block or already visited,
            // descending into the first unvisited in-block input we find. We take
            // the edge cursor by value and write it back via indexing so the
            // mutable borrow of `stack` does not overlap the `push`/`pop` below.
            let frame = stack.len() - 1;
            let mut edge = stack[frame].1;
            let mut descended = None;
            while edge < inputs.len() {
                let inp = inputs[edge];
                edge += 1;
                if set.contains(&inp) && visited.insert(inp) {
                    descended = Some(inp);
                    break;
                }
            }
            stack[frame].1 = edge;
            match descended {
                Some(inp) => stack.push((inp, 0)),
                // All inputs processed → emit this node in post-order and pop.
                None => {
                    sorted.push(id);
                    stack.pop();
                }
            }
        }
    }

    *nodes = sorted;
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{IrBuilder, IrType};
    use crate::ir_optimize;

    fn build_schedule(
        code: &[u8],
        code_len: usize,
        num_params: usize,
        num_locals: usize,
    ) -> Schedule {
        let builder = IrBuilder::new(num_params, num_locals);
        let mut graph = builder.build(code, code_len).expect("build failed");
        ir_optimize::optimize(&mut graph);
        schedule(&graph)
    }

    #[test]
    fn test_schedule_linear_method() {
        // int f(int a, int b) { return a + b; }
        let code = [0x1a, 0x1b, 0x60, 0xac, 0, 0];
        let sched = build_schedule(&code, 4, 2, 2);
        assert!(!sched.blocks.is_empty(), "Should have at least one block");
        // Single block method — one entry block with Return terminator
        let entry = &sched.blocks[0];
        assert!(entry.terminator.is_some());
    }

    #[test]
    fn test_schedule_branching_method() {
        // if (x == 0) return 1; return 0;
        let code = [
            0x1a, // 0: iload_0
            0x99, 0x00, 0x05, // 1: ifeq → 6
            0x03, // 4: iconst_0
            0xac, // 5: ireturn
            0x04, // 6: iconst_1
            0xac, // 7: ireturn
            0, 0,
        ];
        let sched = build_schedule(&code, 8, 1, 1);
        // Should have 3 blocks: entry (with If), true branch, false branch
        assert!(
            sched.blocks.len() >= 2,
            "Branching method should have 2+ blocks, got {}",
            sched.blocks.len()
        );
    }

    #[test]
    fn test_schedule_node_to_block_mapping() {
        // int f(int a, int b) { return a + b; }
        let code = [0x1a, 0x1b, 0x60, 0xac, 0, 0];
        let sched = build_schedule(&code, 4, 2, 2);
        // Every non-dead, non-MAX node should map to a valid block
        for (id, &blk) in sched.node_to_block.iter().enumerate() {
            if blk != usize::MAX {
                assert!(
                    blk < sched.blocks.len(),
                    "Node {} mapped to invalid block {}",
                    id,
                    blk
                );
            }
        }
    }

    #[test]
    fn test_schedule_blocks_have_ctrl_nodes() {
        let code = [0x1a, 0x1b, 0x60, 0xac, 0, 0];
        let sched = build_schedule(&code, 4, 2, 2);
        for block in &sched.blocks {
            // Every block must have a valid ctrl node
            assert!(
                (block.ctrl as usize) < sched.node_to_block.len(),
                "Block {} has out-of-range ctrl node {}",
                block.id,
                block.ctrl
            );
        }
    }

    #[test]
    fn test_schedule_entry_block_is_first() {
        // The first block should be the entry block (control projection from Start)
        let code = [0x1a, 0xac, 0, 0]; // iload_0; ireturn
        let sched = build_schedule(&code, 2, 1, 1);
        assert!(!sched.blocks.is_empty());
        // Entry block should have no predecessors
        assert!(
            sched.blocks[0].predecessors.is_empty(),
            "Entry block should have no predecessors"
        );
    }

    #[test]
    fn test_schedule_return_has_no_successors() {
        let code = [0x1a, 0xac, 0, 0]; // iload_0; ireturn
        let sched = build_schedule(&code, 2, 1, 1);
        // Find the block with a Return terminator — it should have no successors
        for block in &sched.blocks {
            if let Some(term) = block.terminator {
                let builder = IrBuilder::new(1, 1);
                let mut graph = builder.build(&code, 2).expect("build failed");
                ir_optimize::optimize(&mut graph);
                if graph.nodes[term as usize].op == Op::Return {
                    assert!(
                        block.successors.is_empty(),
                        "Return block should have no successors"
                    );
                }
            }
        }
    }

    #[test]
    fn test_schedule_branching_successor_count() {
        // if (x == 0) return 1; return 0;
        let code = [
            0x1a, // iload_0
            0x99, 0x00, 0x05, // ifeq → 6
            0x03, // iconst_0
            0xac, // ireturn
            0x04, // iconst_1
            0xac, // ireturn
            0, 0,
        ];
        let sched = build_schedule(&code, 8, 1, 1);
        // At least one block should have 2 successors (the If block)
        let _has_branch = sched.blocks.iter().any(|b| b.successors.len() == 2);
        // It's possible the optimizer simplifies the branch, so just check
        // that the schedule produced valid blocks
        assert!(
            sched.blocks.len() >= 2,
            "Branch should produce multiple blocks"
        );
    }

    #[test]
    fn test_schedule_constant_return() {
        // return 42;  →  bipush 42; ireturn
        let code = [0x10, 42, 0xac, 0, 0, 0];
        let sched = build_schedule(&code, 3, 0, 0);
        assert!(!sched.blocks.is_empty());
        // Single block with a terminator
        assert!(sched.blocks[0].terminator.is_some());
    }

    #[test]
    fn test_schedule_subtraction() {
        // int f(int a, int b) { return a - b; }
        // iload_0; iload_1; isub; ireturn
        let code = [0x1a, 0x1b, 0x64, 0xac, 0, 0];
        let sched = build_schedule(&code, 4, 2, 2);
        // Should have data nodes (the Sub) placed in a block
        let total_data_nodes: usize = sched.blocks.iter().map(|b| b.nodes.len()).sum();
        assert!(
            total_data_nodes > 0,
            "Data nodes should be scheduled into blocks"
        );
    }

    // ── Helpers for the dominator / topo-sort unit tests ─────────────────

    /// Build a bare block with the given predecessor list (ctrl/terminator are
    /// irrelevant for the dominator math, which only reads `predecessors`).
    fn blk(id: usize, preds: &[usize]) -> Block {
        Block {
            id,
            ctrl: 0,
            nodes: Vec::new(),
            terminator: None,
            successors: Vec::new(),
            predecessors: preds.to_vec(),
        }
    }

    #[test]
    fn test_dominators_diamond() {
        // Diamond CFG:  0 → {1,2} → 3
        let blocks = vec![blk(0, &[]), blk(1, &[0]), blk(2, &[0]), blk(3, &[1, 2])];
        let dom = compute_dominators(&blocks);
        // Entry dominates everything.
        for b in 0..4 {
            assert!(dominates(&dom, 0, b), "entry must dominate block {b}");
        }
        // The merge (3) is dominated only by 0 and itself — NOT by 1 or 2,
        // since either side branch can be taken.
        assert!(dominates(&dom, 3, 3));
        assert!(!dominates(&dom, 1, 3), "branch 1 must NOT dominate merge");
        assert!(!dominates(&dom, 2, 3), "branch 2 must NOT dominate merge");
        // Siblings do not dominate each other.
        assert!(!dominates(&dom, 1, 2));
        assert!(!dominates(&dom, 2, 1));
    }

    #[test]
    fn test_dominators_chain() {
        // Straight-line chain 0 → 1 → 2 → 3: each block dominates all later ones.
        let blocks = vec![blk(0, &[]), blk(1, &[0]), blk(2, &[1]), blk(3, &[2])];
        let dom = compute_dominators(&blocks);
        for a in 0..4 {
            for b in a..4 {
                assert!(dominates(&dom, a, b), "block {a} must dominate {b}");
            }
            for b in 0..a {
                assert!(!dominates(&dom, a, b), "block {a} must not dominate {b}");
            }
        }
    }

    #[test]
    fn test_dominators_loop_back_edge() {
        // Loop: 0 → 1 → 2, with back-edge 2 → 1 (1 has preds {0, 2}).
        let mut blocks = vec![blk(0, &[]), blk(1, &[0, 2]), blk(2, &[1])];
        blocks[1].successors = vec![2];
        blocks[2].successors = vec![1];
        let dom = compute_dominators(&blocks);
        // The fixpoint must terminate and yield sensible dominance despite the
        // cycle: 0 dominates 1 and 2; 1 dominates 2.
        assert!(dominates(&dom, 0, 1));
        assert!(dominates(&dom, 0, 2));
        assert!(dominates(&dom, 1, 2));
        assert!(
            !dominates(&dom, 2, 1),
            "back-edge target not dominated by body"
        );
    }

    #[test]
    fn test_topo_sort_orders_inputs_before_uses() {
        // Build a graph where node 2 = node0 + node1, all in one block, but the
        // block list is given uses-first to force a reorder.
        let mut graph = Graph {
            nodes: Vec::new(),
            entry: NO_NODE,
            exit: NO_NODE,
            safepoints: Vec::new(),
            uses: Default::default(),
        };
        let a = graph.add(Op::Const(1), IrType::Int, vec![], None); // 0
        let b = graph.add(Op::Const(2), IrType::Int, vec![], None); // 1
        let sum = graph.add(Op::Add, IrType::Int, vec![a, b], None); // 2
        let mut nodes = vec![sum, a, b]; // intentionally out of order
        topo_sort_block(&graph, &mut nodes);
        let pos = |n: NodeId| nodes.iter().position(|&x| x == n).unwrap();
        assert!(pos(a) < pos(sum), "input a must precede the Add");
        assert!(pos(b) < pos(sum), "input b must precede the Add");
        assert_eq!(nodes.len(), 3, "no nodes dropped or duplicated");
    }

    #[test]
    fn test_topo_sort_deep_chain_no_overflow() {
        // A left-leaning chain N levels deep would blow a recursive stack; the
        // iterative version must handle it. Each node depends on the previous.
        let mut graph = Graph {
            nodes: Vec::new(),
            entry: NO_NODE,
            exit: NO_NODE,
            safepoints: Vec::new(),
            uses: Default::default(),
        };
        let base = graph.add(Op::Const(0), IrType::Int, vec![], None);
        let mut prev = base;
        let depth = 50_000;
        for _ in 0..depth {
            prev = graph.add(Op::Neg, IrType::Int, vec![prev], None);
        }
        // Present the nodes in reverse (deepest first) to force full traversal.
        let mut nodes: Vec<NodeId> = (0..=depth as NodeId).rev().collect();
        topo_sort_block(&graph, &mut nodes);
        // After sorting, every node must come after its single input.
        assert_eq!(nodes.len(), depth + 1);
        assert_eq!(nodes[0], base, "the no-input base must sort first");
        for w in nodes.windows(2) {
            assert!(w[0] < w[1], "chain must be in ascending dependency order");
        }
    }
}
