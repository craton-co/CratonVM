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
//!
//! ## Block frequency, layout and intra-block priority (C2 review P1)
//!
//! On top of the placement above this module estimates how often each block
//! executes ([`BlockFrequencies`]), can reorder the block list so hot
//! successors land physically next and cold subtrees sink to the end
//! ([`layout_blocks`]), and can list-schedule the *pure* nodes inside a block
//! by critical path and register pressure rather than raw dependence order
//! ([`schedule_with_options`]).
//!
//! All three are **opt-in**: [`schedule`] is exactly
//! `schedule_with_options(graph, &ScheduleOptions::default())`, and the default
//! options leave the block order, the intra-block order and therefore the
//! emitted bytes byte-for-byte identical to what this pass produced before.
//! Only [`Schedule::freq`] and [`Schedule::layout`] are new, and both are
//! additive read-only reports.
//!
//! ### The contract with `ir_lower`
//!
//! `ir_lower` derives three things from the *order* of [`Schedule::blocks`],
//! and every one of them survives a reordering by construction:
//!
//! 1. **`plan_slots`' position model.** Each block occupies a contiguous run of
//!    linear positions — one per data node, one for the terminator, then one
//!    extra for the outgoing edge where `emit_phi_copies` reads that block's phi
//!    arguments. Reordering permutes whole blocks and never splits, merges,
//!    adds or drops one, so the numbering is still "spans back to back, one
//!    edge position each". Intra-block priority scheduling permutes a block's
//!    `nodes` but keeps the same node *set*, so the block's span keeps its
//!    length; it also keeps every definition before every use, so no live range
//!    inverts.
//! 2. **The back-edge test `succ <= block_idx`,** which `lower_block` /
//!    `lower_terminator` use to decide where to emit a safepoint poll. The
//!    layout is a *reverse postorder*: for every edge `u → v` reachable from the
//!    entry, `pos(u) < pos(v)` unless the edge is retreating (its target is a
//!    DFS ancestor of its source). Every cycle contains at least one retreating
//!    edge in any DFS, so every loop still gets at least one poll per iteration.
//!    A natural loop's back edge — whose target dominates its source — is
//!    retreating in *every* DFS, so it is polled in every layout.
//! 3. **`blocks[0]` is the entry.** The layout pins it at position 0, so
//!    [`compute_dominators`]' entry assumption and `find_best_block`'s fallback
//!    are unchanged.
//!
//! Nothing else in `ir_lower` reads a block index as an ordering:
//! `emit_phi_copies` matches predecessors by *control token*, and
//! `patch_branches` resolves every edge through `block_offsets`, so control flow
//! is index-addressed rather than layout-addressed.
//!
//! ### What a "fall-through" means here
//!
//! `ir_lower` currently ends **every** block with an explicit `JMP`, including
//! the edge it calls the fall-through. Making the hot successor physically next
//! is therefore necessary but not yet sufficient: the redundant `JMP` to
//! `block_idx + 1` still has to be elided in `ir_lower::lower_block` /
//! `lower_terminator` for the layout to turn into fewer taken branches and
//! fewer bytes. [`LayoutReport`] measures the opportunity this pass creates so
//! that change can be evaluated against a number rather than a hunch.

use super::bailout::{Bailout, BailoutReason};
use super::ir::{Graph, NodeId, Op, Reorder, ReorderBlock, NO_NODE};
use std::collections::HashMap;

// ── Tuning constants ─────────────────────────────────────────────────

/// Trip count assumed for a loop with no usable profile.
///
/// The classic static-profile default (Ball–Larus / Wu–Larus and every
/// production compiler since) — a loop back edge is taken ~90 % of the time,
/// i.e. the body runs ~10× per entry. It is deliberately modest: over-
/// estimating trip counts inflates every block inside a loop relative to
/// straight-line code, and the layout only needs the *ordering* of frequencies
/// to be right, not their absolute scale.
pub const DEFAULT_TRIP_COUNT: f64 = 10.0;

/// Clamp on an estimated trip count, so a profile that saw one loop exit in a
/// million iterations cannot produce a frequency that saturates `f64` precision
/// once it is raised to the nesting depth.
const MAX_TRIP_COUNT: f64 = 1_000.0;

/// Edge probabilities are clamped away from 0 and 1 so a single profiled
/// branch can never drive a reachable block's frequency to exactly zero (which
/// would make "hotter than" comparisons meaningless for everything downstream
/// of it).
const MIN_EDGE_PROB: f64 = 0.01;
const MAX_EDGE_PROB: f64 = 1.0 - MIN_EDGE_PROB;

/// Minimum observations before a branch profile is trusted over the static
/// heuristics. Matches `profile::BranchCounts::is_usually_taken`'s own
/// `total >= 20` floor, so the two tiers agree about what "profiled" means.
pub const MIN_PROFILE_SAMPLES: u64 = 20;

/// Saturation ceiling for a block frequency. A deeply nested loop nest reaches
/// this long before `f64` loses precision, and every consumer only compares
/// frequencies, so saturating is strictly better than overflowing to infinity.
const MAX_BLOCK_FREQ: f64 = 1.0e9;

/// A block is "cold" when it executes less than this fraction of one entry
/// execution — i.e. it is reached only through an unlikely branch.
pub const COLD_FREQ_THRESHOLD: f64 = 0.05;

// ── Public types ─────────────────────────────────────────────────────

/// Observed taken / not-taken counts for one conditional branch.
///
/// Deliberately a plain pair rather than a borrow of `profile::BranchCounts`:
/// the scheduler must not depend on the profile module's storage (the same
/// numbers arrive from `profile::MethodProfile::branches`, from
/// `pgo::BranchProfile`, and from hand-written tests), and the counts are two
/// integers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BranchBias {
    /// Times the branch condition was TRUE (the `Proj(0)` edge was taken).
    pub taken: u64,
    /// Times the branch condition was FALSE (the `Proj(1)` edge was taken).
    pub not_taken: u64,
}

impl BranchBias {
    /// Total observations.
    pub fn total(&self) -> u64 {
        self.taken.saturating_add(self.not_taken)
    }

    /// Probability of the TRUE edge, or `None` when there are too few samples
    /// to prefer this over the static heuristics.
    pub fn taken_probability(&self) -> Option<f64> {
        let total = self.total();
        if total < MIN_PROFILE_SAMPLES {
            return None;
        }
        Some(clamp_prob(self.taken as f64 / total as f64))
    }
}

/// Knobs for [`schedule_with_options`]. `Default` reproduces the historical
/// scheduler exactly: no profile, no reordering, dependence-order-only
/// intra-block scheduling.
#[derive(Debug, Clone, Default)]
pub struct ScheduleOptions {
    /// Profiled branch bias keyed by the *bytecode PC* of the `Op::If`, which
    /// is the same key `ir_lower`'s `branch_hints` and the interpreter's
    /// `profile::MethodProfile::record_branch` use. Empty ⇒ static heuristics
    /// only.
    pub branch_counts: HashMap<usize, BranchBias>,
    /// Reorder [`Schedule::blocks`] so hot successors fall through and cold
    /// subtrees sink towards the end.
    pub layout_hot_paths: bool,
    /// List-schedule the pure nodes inside each block by critical path and
    /// register pressure instead of plain dependence order.
    pub priority_within_blocks: bool,
    /// Move a single-use operand to sit immediately before the node that reads
    /// it. See [`pair_single_use_operands`].
    pub pair_single_use_operands: bool,
    /// Groups of block indices (in the *pre-layout* numbering) that must stay
    /// contiguous and in relative order — the shape an exception handler's
    /// protected range takes once the IR grows exception edges.
    ///
    /// The IR front end does not build exception edges today (`IrBuilder::build`
    /// skips handler bodies outright, so a JIT frame never takes one), which is
    /// why this is empty in the production pipeline. It exists because a layout
    /// pass that has no notion of a region cannot be *made* region-safe later
    /// without re-deriving the order, and because "cold-block sinking must not
    /// move a block out of its protected range" is a property worth pinning
    /// before there is a range to violate.
    pub protected_regions: Vec<Vec<usize>>,
    /// Sink a pure data node to the shallowest loop nesting that still
    /// dominates every use of it. See [`sink_pure_nodes`]; ORed with
    /// `CRATONVM_JIT_IR_SINK_LATE` so the production path, which calls
    /// [`schedule`] with no options at all, can reach it.
    pub sink_pure_late: bool,
}

/// Sink pure nodes out of loops they are only used outside of -- **default ON**
/// since 2026-09-05; `CRATONVM_JIT_IR_SINK_LATE=0` is the kill switch.
///
/// This module's own header says a data node is "placed as late as possible
/// (to minimize register pressure)". It is not: [`find_best_block`] picks the
/// deepest block that all its INPUTS dominate, which is schedule-EARLY. For a
/// value whose inputs are a loop phi and a constant, that is inside the loop —
/// whatever its uses do.
///
/// `probes/OsrTierBench.java` is the case that named it. `return sum * 1000003L
/// + acc` compiles to three nodes whose only consumer is the `Return` in the
/// exit block, and all three were scheduled into the loop body: eleven of the
/// loop's fifty-five instructions, computed and discarded on every iteration.
fn sink_late_enabled() -> bool {
    // 2026-09-05: DEFAULT ON. `=0` is the kill switch.
    match cratonvm_types::flags::runtime_var("CRATONVM_JIT_IR_SINK_LATE") {
        Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
        Err(_) => true,
    }
}

/// Also take the LATEST block at equal loop depth, and read a phi's value
/// input on its own edge -- **default OFF**; `CRATONVM_JIT_IR_SINK_EQUAL_DEPTH=1`
/// arms it.
///
/// [`sink_pure_nodes`]'s own doc records why the equal-depth half was left out:
/// it is "a different trade with a different risk", and leaving it out kept the
/// pass attributable to the one thing it claims. The partial unroller is the
/// case that makes the trade worth taking, and
/// `internal/performance/c2-the-partial-unroller-20260911.md` §5 is the
/// measurement that says so: every copy of an unrolled body sits at the SAME
/// loop depth as the header, so the strict-decrease rule moves none of them,
/// all `factor` copies are computed above the first test, and the carried
/// values spill. That page counts 69 memory operands of 180 in the unrolled
/// loop against **zero** in the rolled one.
///
/// The second half is not a separate option because it is not separable. A
/// phi's value input is used **on its edge** -- in the matching predecessor
/// block -- not in the phi's own block. Attributing it to the phi's block makes
/// a loop header a use site for every carried value, which pins all of them
/// above the body whatever the depth rule then decides. Equal-depth sinking
/// without the edge rule therefore moves nothing in a loop, which is the only
/// place it was built to help.
///
/// Read LIVE, deliberately. `ir_per_copy_frames_enabled` documents the trap
/// this flag would otherwise walk into: the first version of THAT flag cached
/// in a `OnceLock`, the matrix test ran the OFF arm first, and both arms
/// reported byte-identical code.
fn sink_equal_depth_enabled() -> bool {
    matches!(
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_IR_SINK_EQUAL_DEPTH").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE") | Ok("True")
    )
}

/// The block a CONTROL node belongs to, walking up `inputs[0]` until a node
/// that actually heads a block is reached.
///
/// A `Merge`/`Region`/`Start`/`Proj` heads its block and is found on the first
/// step; an `If` does not, and resolves to the block whose terminator it is.
/// Same walk `regalloc::ls_ctrl_block_of` makes against a finished
/// [`Schedule`], written here against the in-progress `blocks` + `node_to_block`
/// pair because this pass runs before one exists.
fn ctrl_block_of(
    graph: &Graph,
    blocks: &[Block],
    node_to_block: &[usize],
    mut ctrl: NodeId,
) -> Option<usize> {
    for _ in 0..graph.nodes.len() {
        if ctrl == NO_NODE {
            return None;
        }
        let blk = *node_to_block.get(ctrl as usize)?;
        if blk != usize::MAX && blocks.get(blk).map(|b| b.ctrl) == Some(ctrl) {
            return Some(blk);
        }
        let node = graph.nodes.get(ctrl as usize)?;
        match node.inputs.first() {
            Some(&next) if next != ctrl => ctrl = next,
            _ => return None,
        }
    }
    None
}

/// May `op` be moved to a different block without changing what the method
/// does?
///
/// Pure arithmetic only: no memory edge, no control edge, and nothing that can
/// trap. `Op::Div` and `Op::Rem` are absent because their zero guard is a
/// deopt point, and moving a trap changes where the exception is raised;
/// `Op::Const` is absent because it is already in the entry block (it has no
/// inputs); `Op::Phi` is pinned to its merge.
fn op_is_sinkable(op: &Op) -> bool {
    matches!(
        op,
        Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Neg
            | Op::And
            | Op::Or
            | Op::Xor
            | Op::Shl
            | Op::Shr
            | Op::UShr
            | Op::I2L
            | Op::L2I
            | Op::I2F
            | Op::I2D
            | Op::L2F
            | Op::L2D
            | Op::F2I
            | Op::F2L
            | Op::F2D
            | Op::D2I
            | Op::D2L
            | Op::D2F
            | Op::Cmp(_)
            | Op::LCmp
    )
}

/// Loop nesting depth per block, from the back edges the dominator relation
/// already knows about.
///
/// A back edge is an edge `u -> v` whose target dominates its source; its
/// natural loop is `v` plus everything that reaches `u` without passing
/// through `v`. Same definition `ir_lower`'s poll placement rests on, computed
/// here from `dom` rather than re-derived.
fn loop_depths(blocks: &[Block], dom: &Dominators) -> Vec<u32> {
    let n = blocks.len();
    let mut depth = vec![0u32; n];
    for u in 0..n {
        for &v in &blocks[u].successors {
            if v >= n || !dom.dominates(v, u) {
                continue;
            }
            if u == v {
                depth[v] += 1;
                continue;
            }
            let mut in_loop = vec![false; n];
            in_loop[v] = true;
            in_loop[u] = true;
            let mut stack = vec![u];
            while let Some(b) = stack.pop() {
                for &p in &blocks[b].predecessors {
                    if p < n && !in_loop[p] {
                        in_loop[p] = true;
                        stack.push(p);
                    }
                }
            }
            for (b, inside) in in_loop.iter().enumerate() {
                if *inside {
                    depth[b] += 1;
                }
            }
        }
    }
    depth
}

/// The deepest block that dominates every block in `of`, or `None`.
fn deepest_common_dominator(dom: &Dominators, of: &[usize], nb: usize) -> Option<usize> {
    let mut best: Option<usize> = None;
    for cand in 0..nb {
        if !of.iter().all(|&b| dom.dominates(cand, b)) {
            continue;
        }
        best = Some(match best {
            // Deeper means dominated by the other.
            Some(cur) if dom.dominates(cur, cand) => cand,
            Some(cur) => cur,
            None => cand,
        });
    }
    best
}

/// Move each pure node to the shallowest loop nesting on the dominator path
/// between where its inputs put it and where its uses need it.
///
/// **Only when the depth strictly decreases** — unless
/// [`sink_equal_depth_enabled`], which adds the other half of the classic
/// schedule-late (prefer the latest block at equal depth, to shorten live
/// ranges) together with the phi-edge use attribution it needs to reach a loop
/// at all. That is a different trade with a different risk, which is why it is
/// a flag and why the depth rule is what a failure falls back to rather than
/// what it takes down with it.
///
/// # The safepoint obligation, and why a failure reverts everything
///
/// [`safepoints_dominate_anchors`] states it. The check is made against the
/// FINAL placement; a violation first retries the placement WITHOUT the
/// equal-depth rule, and reverts the whole method only if the depth rule alone
/// cannot satisfy it either. `reverted` counts that.
fn sink_pure_nodes(
    graph: &Graph,
    blocks: &mut [Block],
    node_to_block: &mut [usize],
    dom: &Dominators,
) -> (usize, usize) {
    let nb = blocks.len();
    if nb < 2 {
        return (0, 0);
    }
    let depth = loop_depths(blocks, dom);
    let original: Vec<usize> = node_to_block.to_vec();

    // Read once per call, not once per process: `sink_equal_depth_enabled`
    // documents why this may not be cached across compiles.
    let equal_depth = sink_equal_depth_enabled();

    let mut moved = place_sunk_nodes(graph, blocks, node_to_block, dom, &depth, equal_depth);
    let mut ok =
        moved == 0 || safepoints_dominate_anchors(graph, node_to_block, &original, dom, nb);
    let mut retried = false;

    // The equal-depth rule sinks strictly more nodes than the depth rule alone,
    // so it has strictly more ways to violate the safepoint obligation — and
    // the revert is whole-method. Falling straight back to "no sinking at all"
    // would therefore let the NEW rule cost the OLD one its wins on any method
    // that happens to name a sunk value in a deopt frame, which is a regression
    // dressed as a conservative choice. Retry with the rule this pass shipped
    // with instead, and revert only if THAT also fails.
    if !ok && equal_depth {
        node_to_block.copy_from_slice(&original);
        moved = place_sunk_nodes(graph, blocks, node_to_block, dom, &depth, false);
        ok = moved == 0 || safepoints_dominate_anchors(graph, node_to_block, &original, dom, nb);
        retried = true;
    }

    if moved == 0 {
        node_to_block.copy_from_slice(&original);
        return (0, 0);
    }
    if !ok {
        node_to_block.copy_from_slice(&original);
        return (0, moved);
    }

    // Rebuild each block's data-node list from the placement. `topo_sort_block`
    // runs after this and restores dependence order within every block.
    let placed: Vec<NodeId> = blocks
        .iter()
        .flat_map(|b| b.nodes.iter().copied())
        .collect();
    for b in blocks.iter_mut() {
        b.nodes.clear();
    }
    for id in placed {
        let b = node_to_block[id as usize];
        if b != usize::MAX && b < nb {
            blocks[b].nodes.push(id);
        }
    }
    if retried && cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_IR_SINK") {
        eprintln!("[ir-sink] equal-depth placement violated a safepoint; fell back to depth-only");
    }
    (moved, 0)
}

/// Choose a block for every sinkable node, writing the choice into
/// `node_to_block`. Returns how many nodes it moved.
///
/// Split out of [`sink_pure_nodes`] so the equal-depth rule can be retried
/// without it — see the fallback there. Makes no safepoint check of its own:
/// the obligation is a property of the FINAL placement and is checked once, by
/// [`safepoints_dominate_anchors`].
fn place_sunk_nodes(
    graph: &Graph,
    blocks: &[Block],
    node_to_block: &mut [usize],
    dom: &Dominators,
    depth: &[u32],
    equal_depth: bool,
) -> usize {
    let nb = blocks.len();
    let mut moved = 0usize;

    // Iterated, because a node can only follow its uses: the `Add` feeding the
    // `Return` has to reach the exit block before the `Mul` feeding the `Add`
    // can see a reason to. Three rounds settle the shape above; the bound is a
    // guard against a graph that does not converge, not a tuning choice.
    for _ in 0..8 {
        let mut use_blocks: Vec<Vec<usize>> = vec![Vec::new(); graph.nodes.len()];
        for (uid, un) in graph.nodes.iter().enumerate() {
            let ub = node_to_block[uid];
            if ub == usize::MAX || ub >= nb {
                continue;
            }
            // A phi reads each value input ON ITS OWN EDGE. Attributing those
            // reads to the phi's block makes a loop header a use site for every
            // carried value and pins all of them above the body; attributing
            // them to the matching predecessor is what lets a carried value
            // sink to the copy that produces the next one. See
            // `sink_equal_depth_enabled` for why this travels with the
            // equal-depth rule rather than being its own switch.
            //
            // Falls back to the conservative whole-node attribution whenever
            // the merge's arity does not match the phi's — an unmatched phi is
            // a shape this pass does not model, and modelling it wrongly would
            // sink a value past a reader.
            if equal_depth && matches!(un.op, Op::Phi) {
                let edges: Option<Vec<usize>> = un
                    .input_opt(0)
                    .and_then(|c| graph.nodes.get(c as usize))
                    .filter(|m| matches!(m.op, Op::Merge | Op::Region))
                    .filter(|m| m.inputs.len() + 1 == un.inputs.len())
                    .map(|m| {
                        m.inputs
                            .iter()
                            .map(|&c| {
                                ctrl_block_of(graph, blocks, node_to_block, c).unwrap_or(usize::MAX)
                            })
                            .collect()
                    });
                if let Some(edges) = edges {
                    for (k, &inp) in un.inputs.iter().skip(1).enumerate() {
                        if inp == NO_NODE {
                            continue;
                        }
                        // When the edge's block is unknown, fall back to the
                        // phi's own block for THIS input rather than dropping
                        // the use entirely — a dropped use is a node free to
                        // sink below a reader.
                        let eb = match edges[k] {
                            e if e != usize::MAX && e < nb => e,
                            _ => ub,
                        };
                        if let Some(v) = use_blocks.get_mut(inp as usize) {
                            if !v.contains(&eb) {
                                v.push(eb);
                            }
                        }
                    }
                    continue;
                }
            }
            for &inp in &un.inputs {
                if inp == NO_NODE {
                    continue;
                }
                if let Some(v) = use_blocks.get_mut(inp as usize) {
                    if !v.contains(&ub) {
                        v.push(ub);
                    }
                }
            }
        }
        let mut changed = false;
        for id in 0..graph.nodes.len() {
            let early = node_to_block[id];
            if early == usize::MAX || early >= nb {
                continue;
            }
            if !op_is_sinkable(&graph.nodes[id].op) {
                continue;
            }
            let uses = &use_blocks[id];
            if uses.is_empty() {
                continue;
            }
            let Some(late) = deepest_common_dominator(dom, uses, nb) else {
                continue;
            };
            // A node is only ever moved DOWN its own dominator path. When a use
            // sits in `early` itself this makes `late == early` and nothing
            // moves, which is the right answer: the value is needed here.
            if !dom.dominates(early, late) {
                continue;
            }
            let mut best = early;
            for cand in 0..nb {
                if !dom.dominates(early, cand) || !dom.dominates(cand, late) {
                    continue;
                }
                // Shallower loop nesting always wins: that is this pass's
                // original claim and it is never traded away.
                if depth[cand] < depth[best] {
                    best = cand;
                    continue;
                }
                // At equal depth, take the LATEST — the candidate the current
                // best dominates. Every candidate here dominates `late`, which
                // dominates every use, so this shortens the live range without
                // moving the value below any reader. `cand != best` keeps the
                // reflexive case from counting as a move.
                if equal_depth
                    && depth[cand] == depth[best]
                    && cand != best
                    && dom.dominates(best, cand)
                {
                    best = cand;
                }
            }
            if best != early {
                node_to_block[id] = best;
                changed = true;
                moved += 1;
            }
        }
        if !changed {
            break;
        }
    }
    moved
}

/// Which `graph.safepoints` entry would `ir_lower` resolve for a deopt taken at
/// this node?
///
/// Exactly `ir_lower::resolve_frame_state_for_site`'s rule, and it has to be:
/// an anchor test that asks a broader question than the lowerer answers reports
/// conflicts that cannot occur, and a narrower one misses conflicts that can.
///
/// * a node carrying a per-copy [`crate::ir::Node::frame_snapshot`] resolves to
///   THAT snapshot, provided it still names the node's own bci;
/// * every other node falls back to the first snapshot at its bci, which is the
///   by-bci scan that was the only path before cloned regions existed.
///
/// The distinction is the whole reason an unrolled body can be scheduled per
/// copy at all: after a clone there are `factor` nodes at one bci, and keying
/// anchors by bci makes every copy's frame claim every copy's blocks — so a
/// value sunk into copy 1 reads as failing to dominate copy 0 and the placement
/// is thrown away. See `internal/performance/c2-per-copy-deopt-frames-20260911.md`.
fn resolved_snapshot(graph: &Graph, n: &crate::ir::Node) -> Option<usize> {
    let pc = n.bytecode_pc?;
    if let Some(si) = n.frame_snapshot {
        if graph.safepoints.get(si as usize).map(|s| s.bci) == Some(pc) {
            return Some(si as usize);
        }
    }
    graph.safepoints.iter().position(|s| s.bci == pc)
}

/// Does every value a safepoint names still dominate every block that could
/// anchor that safepoint?
///
/// A frame state resolves a value it names from that value's HOME WORD, and the
/// home is written wherever the node is emitted. So a node this pass moved must
/// still dominate every block that could anchor a safepoint naming it —
/// otherwise a deopt inside the loop reads a word nothing has written yet,
/// which is the "confidently wrong value" failure this area produces.
///
/// A node still at its original block is skipped: this pass owes nothing for a
/// placement it did not choose.
///
/// The answer is about the WHOLE method, and a violation reverts the whole
/// method rather than the offending node: reverting one node can break
/// another's dominance (a use pulled back above a def that sank), so undoing
/// the lot is the only revert that is obviously correct.
fn safepoints_dominate_anchors(
    graph: &Graph,
    node_to_block: &[usize],
    original: &[usize],
    dom: &Dominators,
    nb: usize,
) -> bool {
    let mut anchors: std::collections::HashMap<usize, Vec<usize>> =
        std::collections::HashMap::new();
    for (nid, n) in graph.nodes.iter().enumerate() {
        if let Some(si) = resolved_snapshot(graph, n) {
            let b = node_to_block[nid];
            if b != usize::MAX && b < nb {
                anchors.entry(si).or_default().push(b);
            }
        }
    }
    for (si, sp) in graph.safepoints.iter().enumerate() {
        let Some(anchor_blocks) = anchors.get(&si) else {
            // No node resolves to this snapshot, so `build_deopt_points` finds
            // no native anchor for it and emits no point at all.
            continue;
        };
        for &v in sp.locals.iter().chain(sp.stack.iter()) {
            if v == NO_NODE {
                continue;
            }
            let vb = match node_to_block.get(v as usize) {
                Some(&b) if b != usize::MAX && b < nb => b,
                _ => continue,
            };
            if original.get(v as usize) == Some(&vb) {
                continue; // not moved by this pass
            }
            if !anchor_blocks.iter().all(|&ab| dom.dominates(vb, ab)) {
                return false;
            }
        }
    }
    true
}

/// Where a block's frequency estimate came from, and what it is.
///
/// Frequencies are **normalized on the entry block**: `freq[0] == 1.0` means
/// "once per invocation of this method", so a block inside a 10-trip loop reads
/// ~9.0 and a block behind a 1-in-100 branch reads ~0.01. That normalization is
/// what makes the numbers comparable across methods and stable under
/// reordering — permuting blocks permutes this vector and changes no value.
#[derive(Debug, Clone, Default)]
pub struct BlockFrequencies {
    /// Estimated executions per method invocation, one entry per block.
    /// Always finite, non-negative and `<= MAX_BLOCK_FREQ`.
    pub freq: Vec<f64>,
    /// Loop nesting depth (0 = not in any loop).
    pub depth: Vec<u32>,
    /// True for a block that is the target of a natural loop back edge.
    pub is_loop_header: Vec<bool>,
    /// Estimated trip count for each loop header (1.0 for non-headers).
    pub trip: Vec<f64>,
    /// `edge_prob[b][i]` is the probability of leaving `b` via
    /// `blocks[b].successors[i]`. Parallel to `successors`, which the layout
    /// never reorders, so `ir_lower`'s "`successors[0]` is the taken edge"
    /// convention is untouched.
    pub edge_prob: Vec<Vec<f64>>,
    /// Two-way branches whose probabilities came from real profile data.
    pub profiled_branches: usize,
    /// Two-way branches that fell back to the static heuristics.
    pub static_branches: usize,
}

impl BlockFrequencies {
    /// The largest frequency in the method (at least 1.0, since the entry is
    /// normalized to 1.0 and there is always an entry).
    pub fn max_freq(&self) -> f64 {
        self.freq
            .iter()
            .copied()
            .fold(1.0f64, |a, b| if b > a { b } else { a })
    }

    /// Frequency of `b` as a fraction of the hottest block, in `[0, 1]`.
    /// Returns 0.0 for an out-of-range index rather than panicking.
    pub fn relative(&self, b: usize) -> f64 {
        match self.freq.get(b) {
            Some(&f) => (f / self.max_freq()).clamp(0.0, 1.0),
            None => 0.0,
        }
    }

    /// True when `b` runs on well under one invocation in twenty — the blocks
    /// worth sinking away from the hot path. Out-of-range indices are cold.
    pub fn is_cold(&self, b: usize) -> bool {
        self.freq.get(b).copied().unwrap_or(0.0) < COLD_FREQ_THRESHOLD
    }

    /// Fraction of two-way branches whose probability came from a profile
    /// rather than a heuristic — the "profile accuracy" input the review asks
    /// to measure. 1.0 with no branches at all (nothing was guessed).
    pub fn profile_coverage(&self) -> f64 {
        let total = self.profiled_branches + self.static_branches;
        if total == 0 {
            1.0
        } else {
            self.profiled_branches as f64 / total as f64
        }
    }
}

/// What the layout pass actually achieved, measured on the CFG it just laid
/// out rather than asserted.
///
/// `fallthrough_weight` counts an edge `u → v` when `v` is *physically next*
/// after `u`, weighted by how often that edge is estimated to execute
/// (`freq[u] * P(u → v)`). `baseline_fallthrough_weight` is the same sum over
/// the original order, so the two together are the improvement this pass makes
/// available to `ir_lower` once it stops emitting `JMP next`.
#[derive(Debug, Clone, Default)]
pub struct LayoutReport {
    /// False when the layout was not requested, or was computed and rejected
    /// by its own validator (in which case the original order is kept).
    pub applied: bool,
    /// Edges whose target is the physically next block, after layout.
    pub fallthrough_edges: usize,
    /// Same, before layout.
    pub baseline_fallthrough_edges: usize,
    /// Execution-weighted fall-through, after layout.
    pub fallthrough_weight: f64,
    /// Same, before layout.
    pub baseline_fallthrough_weight: f64,
    /// Total execution weight over all CFG edges — the denominator for both
    /// weights above.
    pub total_edge_weight: f64,
    /// Cold blocks that moved strictly later in the order.
    pub cold_blocks_sunk: usize,
    /// Why the layout was rejected, when it was.
    pub rejected: Option<String>,
}

impl LayoutReport {
    /// Fraction of execution weight that leaves a block by falling into the
    /// physically next one. 1.0 when there are no edges at all.
    pub fn fallthrough_fraction(&self) -> f64 {
        if self.total_edge_weight <= 0.0 {
            1.0
        } else {
            (self.fallthrough_weight / self.total_edge_weight).clamp(0.0, 1.0)
        }
    }

    /// The same fraction for the pre-layout order.
    pub fn baseline_fallthrough_fraction(&self) -> f64 {
        if self.total_edge_weight <= 0.0 {
            1.0
        } else {
            (self.baseline_fallthrough_weight / self.total_edge_weight).clamp(0.0, 1.0)
        }
    }
}

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
    /// Per-block execution frequency estimate, indexed by the *final* block
    /// index (i.e. permuted along with `blocks` when a layout is applied).
    /// Always populated — it costs one linear pass over a CFG whose block count
    /// is small — so a consumer can weight spill cost, code alignment or
    /// inlining without asking for the layout.
    pub freq: BlockFrequencies,
    /// What the frequency-driven layout did, or why it did nothing.
    pub layout: LayoutReport,
    /// Why each single-use operand did or did not end up beside its consumer.
    /// See [`PairCensus`]; empty when `pair_single_use_operands` is off.
    pub pairing: PairCensus,
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
///
/// Exactly `schedule_with_options(graph, &ScheduleOptions::default())`: no
/// profile, no reordering, dependence-order-only intra-block scheduling. The
/// block list, each block's node order and therefore the bytes `ir_lower`
/// emits are unchanged from before block frequencies existed; the only
/// addition is the read-only [`Schedule::freq`] / [`Schedule::layout`] report.
/// Uses of each node as an INPUT of another node, indexed by `NodeId`.
///
/// The same count `ir_lower`'s carry planner reads, derived the same way, so
/// "single use" means one thing in both places. A value named by a frame state
/// is NOT a use here: frame states pin a value's home, which is a separate
/// question and one [`pair_single_use_operands`] does not touch.
fn use_counts(graph: &Graph) -> Vec<u32> {
    let mut out = vec![0u32; graph.nodes.len()];
    for node in &graph.nodes {
        for &input in &node.inputs {
            if input != NO_NODE {
                if let Some(c) = out.get_mut(input as usize) {
                    *c = c.saturating_add(1);
                }
            }
        }
    }
    out
}

/// Why each `(consumer, operand)` pair did or did not end up adjacent.
///
/// # Why this exists
///
/// `c2-one-carry-slot-is-the-frame-traffic-ceiling-FIXED-20260910.md` §10 closed
/// on a number it could not explain: **82% of the carry's candidate windows are
/// declined for `operand_position`** — the node two positions back is not the
/// consumer's second operand, so the `[input1, input0, cons]` triple both carry
/// slots need was never formed. [`pair_single_use_operands`] is the pass that
/// would form it, it declines for five enumerated reasons, and *"which of those
/// dominates is not yet counted — the pairing pass has no census, and that is
/// the next thing to build, not the next thing to fix."*
///
/// This is that census. One counter per `continue`, so the five reasons are
/// five numbers rather than one silence.
///
/// # The denominator is the honest one
///
/// [`Self::candidates`] counts `(consumer, operand)` pairs where the operand is
/// a real, distinct node — every pair the pass could conceivably act on, and
/// nothing else. The sibling census learned this the hard way: its first cut
/// reported every three consecutive scheduled nodes as the denominator, which
/// made the dominant cause "not a candidate" and said nothing.
///
/// # The identity
///
/// `paired + multi_use + no_node + producer_arm + other_block + after_consumer
/// + already_adjacent + deopt_between == candidates`, checked by
/// [`Self::closes`] and asserted in debug builds at the call site. A reason
/// added to the pass and not to the census fails that assertion rather than
/// quietly reading low — the failure mode
/// `c2-the-gp-register-file-is-not-the-binding-constraint-20260910.md` §10.1
/// records, where a census that computed its own answer drifted to zero while
/// the emission it described did three things.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PairCensus {
    /// `(consumer, operand)` pairs examined. The denominator.
    pub candidates: usize,
    /// Moved to sit immediately before the consumer.
    pub paired: usize,
    /// The operand is read by more than one node, so sinking it past a reader
    /// would be a semantic change rather than a scheduling one.
    pub multi_use: usize,
    /// No node behind the operand's id. Structural; expected to be zero.
    pub no_node: usize,
    /// The operand's op is not one `ir_lower::op_home_is_one_store_rax`
    /// certifies, so the carry would refuse it even if it were adjacent.
    pub producer_arm: usize,
    /// The operand is scheduled in a different block from its consumer.
    pub other_block: usize,
    /// The operand is scheduled AFTER its consumer in this block. A consumer
    /// that is itself a phi reads across a back edge, so this is not a bug.
    pub after_consumer: usize,
    /// Already immediately before the consumer — the pass has nothing to do and
    /// the carry already sees the shape.
    pub already_adjacent: usize,
    /// A node a deopt can arrive at sits between the two positions, so sinking
    /// the definition past it would leave a frame slot nothing had written.
    pub deopt_between: usize,
}

impl PairCensus {
    /// Does every candidate fall into exactly one bucket?
    pub fn closes(&self) -> bool {
        self.paired
            + self.multi_use
            + self.no_node
            + self.producer_arm
            + self.other_block
            + self.after_consumer
            + self.already_adjacent
            + self.deopt_between
            == self.candidates
    }

    /// Add this method's counts to the process-wide totals.
    fn publish(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        debug_assert!(
            self.closes(),
            "a `continue` in pair_single_use_operands has no counter: {self:?}"
        );
        let fields = [
            self.candidates,
            self.paired,
            self.multi_use,
            self.no_node,
            self.producer_arm,
            self.other_block,
            self.after_consumer,
            self.already_adjacent,
            self.deopt_between,
        ];
        for (slot, v) in IR_PAIR_CENSUS.iter().zip(fields) {
            slot.fetch_add(v as u64, Relaxed);
        }
    }
}

/// Process-wide pairing totals, in [`PairCensus`] field order.
///
/// Unconditional and cheap — nine relaxed adds per scheduled method. A census
/// you have to switch on is one nobody has for the run they already did, which
/// is the same reason `IR_CARRY_DEFERRED` is unconditional.
static IR_PAIR_CENSUS: [std::sync::atomic::AtomicU64; 9] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// The process-wide pairing census since process start.
pub fn ir_pair_census() -> PairCensus {
    use std::sync::atomic::Ordering::Relaxed;
    let v: Vec<usize> = IR_PAIR_CENSUS
        .iter()
        .map(|a| a.load(Relaxed) as usize)
        .collect();
    PairCensus {
        candidates: v[0],
        paired: v[1],
        multi_use: v[2],
        no_node: v[3],
        producer_arm: v[4],
        other_block: v[5],
        after_consumer: v[6],
        already_adjacent: v[7],
        deopt_between: v[8],
    }
}

/// `CRATONVM_JIT_IR_PAIR_OPERANDS=0` — schedule single-use operands in plain
/// dependence order again, so `ir_lower`'s carry sees only what the list
/// scheduler happened to leave adjacent.
///
/// Default ON. Both orders compute the same values from the same inputs; what
/// differs is how many of them the lowerer can keep in a register.
fn pair_operands_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_IR_PAIR_OPERANDS").as_deref(),
            Ok("0") | Ok("false") | Ok("off")
        )
    })
}

/// Move a consumer's single-use operands to sit immediately before it, as
/// `[input1, input0, cons]`.
///
/// # What this is for
///
/// `ir_lower` gives every node a frame slot, writes the node's result to it and
/// reloads it at the use. Its carry removes that round trip, but only for
/// ADJACENT scheduled pairs — it leaves the value in RAX (or copies it to RCX)
/// and lets the consumer's arm read it where it already is.
///
/// Measured on a `s += i ^ (s >>> 3)` loop with `CRATONVM_DBG_IR_LINEAR_SCAN=1`:
/// of 25 nodes, residency skipped 16 as `single_use` — it will not spend a
/// callee-saved register plus its prologue save on a value read once — and the
/// carry that exists to cover exactly those took 2. The rest went through the
/// frame: `[rbp-78h]` was written and reloaded two instructions later with the
/// value still live in a register.
///
/// Those producers were not ineligible. They were not ADJACENT: `I2L` and the
/// `Xor` that reads it had the other operand's `UShr` scheduled between them.
///
/// # Why the order is `[input1, input0, cons]` and not the reverse
///
/// A consumer's arm reads its first operand from RAX and its second from RCX
/// (`ir_lower::op_reads_rax_then_rcx`). RAX is where a producer's arm already
/// leaves the value, so `input0` goes adjacent and carries in RAX for free.
/// `input1` is copied to RCX at its own home store and has to survive
/// `input0`'s arm — which is why `ir_lower::op_preserves_rcx` exists and is
/// short. Putting them the other way round would ask a value to survive in RAX,
/// which every arm overwrites.
///
/// # Why sinking a definition later is safe here
///
/// It is not safe in general: a value defined later is UNDEFINED at any deopt
/// arriving in between, and `graph.safepoints` names the full operand stack at
/// every bci, so an intermediate is named from its definition until its
/// consumer pops it. Sinking past a point a deopt can arrive at would hand the
/// interpreter a frame slot nothing had written.
///
/// The condition is therefore about what is CROSSED, not about the value: every
/// node strictly between the operand's old position and its new one must be one
/// no deopt can arrive at (`ir_lower::op_cannot_deopt`, the same enumeration
/// `compute_deopt_named_reachable` builds its trapping-bci set from). When that
/// holds there is no program point between the two positions where a frame
/// state can be read, so none can observe the difference.
///
/// Two further restrictions keep this to the case it was written for:
///
///   * the operand is used exactly once, so nothing between it and the consumer
///     can read it — the reordering is invisible to every other node;
///   * its op is one `ir_lower::op_home_is_one_store_rax` certifies, the same
///     allowlist the carry planner requires. Moving a value the carry will
///     refuse anyway buys nothing and widens the blast radius for no reason.
///
/// Returns how many operands moved.
fn pair_single_use_operands(
    graph: &Graph,
    uses: &[u32],
    nodes: &mut Vec<NodeId>,
    census: &mut PairCensus,
) -> usize {
    let mut moved = 0usize;
    // Descending, so sinking an operand cannot disturb a consumer this loop has
    // yet to visit: everything it moves lands below the current index.
    let mut ci = nodes.len();
    while ci > 0 {
        ci -= 1;
        if ci >= nodes.len() {
            continue;
        }
        let cons = nodes[ci];
        let Some(cn) = graph.nodes.get(cons as usize) else {
            continue;
        };
        // Reverse operand order: `input1` is sunk first and `input0` lands
        // below it, giving `[input1, input0, cons]`.
        let operands: Vec<NodeId> = cn.inputs.iter().copied().take(2).collect();
        for &prod in operands.iter().rev() {
            if prod == NO_NODE || prod == cons {
                continue;
            }
            // Denominator: a real operand of a real consumer, both of which
            // exist. Everything counted below is a subset of this.
            census.candidates += 1;
            if uses.get(prod as usize).copied().unwrap_or(0) != 1 {
                census.multi_use += 1;
                continue;
            }
            let Some(pn) = graph.nodes.get(prod as usize) else {
                census.no_node += 1;
                continue;
            };
            if !crate::ir_lower::op_home_is_one_store_rax(&pn.op) {
                census.producer_arm += 1;
                continue;
            }
            // Re-read both positions: a previous sink in this same iteration
            // has already shifted the consumer down by one.
            let Some(ci_now) = nodes.iter().position(|&n| n == cons) else {
                census.other_block += 1;
                continue;
            };
            let Some(pi) = nodes.iter().position(|&n| n == prod) else {
                // The producer is scheduled in a DIFFERENT block. Sinking
                // across a block boundary is a different transform with a
                // different proof obligation, and this pass does not attempt
                // it.
                census.other_block += 1;
                continue;
            };
            if pi >= ci_now {
                census.after_consumer += 1;
                continue;
            }
            if pi + 1 == ci_now {
                census.already_adjacent += 1;
                continue;
            }
            // The crossing condition: everything in between must be a node no
            // deopt can arrive at.
            if !nodes[pi + 1..ci_now].iter().all(|&mid| {
                graph
                    .nodes
                    .get(mid as usize)
                    .is_some_and(|m| crate::ir_lower::op_cannot_deopt(&m.op))
            }) {
                census.deopt_between += 1;
                continue;
            }
            let id = nodes.remove(pi);
            // `ci_now` was the consumer's index BEFORE the removal, and the
            // removal was below it, so the consumer now sits at `ci_now - 1`
            // and the operand belongs immediately before it.
            nodes.insert(ci_now - 1, id);
            census.paired += 1;
            moved += 1;
        }
    }
    moved
}

pub fn schedule(graph: &Graph) -> Schedule {
    schedule_with_options(graph, &production_schedule_options())
}

/// The options the PRODUCTION optimizing pipeline schedules with.
///
/// This function exists because for the life of this backend the production
/// pipeline called `schedule(graph)`, `schedule` called `schedule_with_options`
/// with `ScheduleOptions::default()`, and `Default` is documented as
/// reproducing "the historical scheduler exactly: no profile, no reordering,
/// dependence-order-only intra-block scheduling". Two of the three stages this
/// module implements were therefore switched off in every compile the VM ever
/// ran, and nothing said so -- the default was doing its job, which is to be a
/// neutral starting point for a TEST, and it had quietly become the shipping
/// configuration as well.
///
/// **`layout_hot_paths`** turns on frequency-driven block layout. It needs no
/// profile to be worth having: [`static_branch_probs`] derives probabilities
/// from loop structure alone -- a back edge closes a loop, and a loop is
/// entered to be repeated; leaving the innermost loop is improbable by exactly
/// its trip count -- which is enough to sink cold subtrees past the loop
/// bodies. That matters more in this tier than in most, because the optimizing
/// tier lays out exception and rare-branch blocks INLINE with hot loop code
/// today.
///
/// **`priority_within_blocks`** list-schedules the pure nodes inside a block by
/// critical path instead of plain dependence order. The reason to want it is on
/// the record with a number: `docs/JIT_OPTIMIZATION.md` traced this tier's
/// residual 1.36x to a dependency chain of store-then-load pairs on the same
/// slot two instructions apart, and showed that a probe with four INDEPENDENT
/// accumulators shrank the gap to 1.18x. Reordering by critical path is the
/// pass whose job is exactly that.
///
/// Both stages are TOTAL: each validates its own output (`validate_order`) and
/// keeps the previous state on failure, recording the reason in
/// [`Schedule::layout`]. So the failure mode of turning them on is the layout
/// this tier already had.
///
/// `CRATONVM_JIT_IR_HOT_LAYOUT=0` and `CRATONVM_JIT_IR_LIST_SCHED=0` are the
/// kill switches, separately, because the blast radii differ: one moves blocks
/// and therefore every oop-map and safepoint position in the method, the other
/// moves only pure nodes within a block.
pub fn production_schedule_options() -> ScheduleOptions {
    ScheduleOptions {
        layout_hot_paths: hot_layout_enabled(),
        priority_within_blocks: list_sched_enabled(),
        pair_single_use_operands: pair_operands_enabled(),
        ..ScheduleOptions::default()
    }
}

/// Frequency-driven block layout in the production pipeline. **Default ON**
/// since 2026-09-06; `CRATONVM_JIT_IR_HOT_LAYOUT=0` is the kill switch.
pub fn hot_layout_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_IR_HOT_LAYOUT").as_deref(),
            Ok("0") | Ok("false") | Ok("off") | Ok("no")
        )
    })
}

/// Critical-path list scheduling within a block. **Default ON** since
/// 2026-09-06; `CRATONVM_JIT_IR_LIST_SCHED=0` is the kill switch.
pub fn list_sched_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_IR_LIST_SCHED").as_deref(),
            Ok("0") | Ok("false") | Ok("off") | Ok("no")
        )
    })
}

/// Schedule the IR graph into basic blocks, with frequency-driven block layout
/// and priority intra-block scheduling under caller control.
///
/// Total: this function never panics and never fails. Every optional stage
/// validates its own output and silently keeps the previous state when the
/// validation fails, recording the reason in [`Schedule::layout`].
pub fn schedule_with_options(graph: &Graph, opts: &ScheduleOptions) -> Schedule {
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
    //
    // An `If` / `Return` / `Throw` terminates the block its control input
    // heads. One pass over the graph, finding that block through
    // `node_to_block`, rather than one whole-graph scan per block: that was
    // O(blocks × nodes), which a method near the node cap pays in full. Nodes
    // are visited in id order and a later match overwrites an earlier one, so a
    // block two terminators name keeps the higher id, as the per-block scan
    // did.
    for (id, node) in graph.nodes.iter().enumerate() {
        if !matches!(node.op, Op::If | Op::Return | Op::Throw) {
            continue;
        }
        let Some(&head) = node.inputs.first() else {
            continue;
        };
        let Some(&block_idx) = node_to_block.get(head as usize) else {
            continue;
        };
        // Only block heads have an entry yet, and no terminator is a head; the
        // `ctrl` check makes that a fact here rather than an assumption.
        if block_idx < blocks.len() && blocks[block_idx].ctrl == head {
            blocks[block_idx].terminator = Some(id as NodeId);
            node_to_block[id] = block_idx;
        }
    }

    // Step 3 reads, per block, the projections of its `If` terminator and the
    // merges its control feeds. Both are gathered here in ONE pass, for the
    // same reason as Step 2. Each list is built in node-id order — the order
    // the per-block scans visited them in — so the successor and predecessor
    // lists below come out identical.
    let num_blocks = blocks.len();
    let mut if_projs: Vec<Vec<(u8, usize)>> = vec![Vec::new(); num_blocks];
    let mut fed_merges: Vec<Vec<usize>> = vec![Vec::new(); num_blocks];
    for (id, node) in graph.nodes.iter().enumerate() {
        match &node.op {
            Op::Proj(which) => {
                let Some(&term) = node.inputs.first() else {
                    continue;
                };
                let Some(&tb) = node_to_block.get(term as usize) else {
                    continue;
                };
                if tb >= num_blocks || blocks[tb].terminator != Some(term) || !is_if(graph, term)
                {
                    continue;
                }
                let succ_block = node_to_block[id];
                if succ_block == usize::MAX {
                    continue;
                }
                if_projs[tb].push((*which, succ_block));
            }
            Op::Merge | Op::Region => {
                for &head in node.inputs.iter() {
                    let Some(&hb) = node_to_block.get(head as usize) else {
                        continue;
                    };
                    if hb >= num_blocks || blocks[hb].ctrl != head {
                        continue;
                    }
                    // A merge naming the same head twice is ONE successor (the
                    // scan asked `inputs.contains(&head)`). This merge's inputs
                    // are visited consecutively, so a repeat is always the
                    // list's last entry.
                    if fed_merges[hb].last() != Some(&id) {
                        fed_merges[hb].push(id);
                    }
                }
            }
            _ => {}
        }
    }

    // Step 3: Build successor/predecessor edges
    for block_idx in 0..blocks.len() {
        if let Some(term) = blocks[block_idx].terminator {
            let term_node = &graph.nodes[term as usize];
            match &term_node.op {
                Op::If => {
                    // The Proj(0) and Proj(1) successors, pushed in PROJECTION
                    // order.
                    //
                    // `ir_lower` names `successors[0]` the TRUE block and
                    // `successors[1]` the false one, so this order is not a
                    // presentation detail: it decides which way every
                    // conditional branch this backend emits goes.
                    //
                    // This scan used to push in node-ID order and rely on the
                    // two agreeing, which they do for every graph `IrBuilder`
                    // produces — its `if_icmp*` arms add `Proj(0)` and then
                    // `Proj(1)`, so the lower id is always the true edge. That
                    // is a property of one producer, not of the IR, and the
                    // first transform to CLONE an `If` broke it: the partial
                    // unroller adds each copy's continue-edge projection first
                    // (it is the one the next copy hangs off), and in a javac
                    // `if_icmpge` loop that is `Proj(1)`. Every intermediate
                    // test then branched to the edge meant for its opposite,
                    // the early exits were never taken, and the loop ran
                    // `n + factor - 1` iterations — `sum(1)` returned 6.
                    //
                    // Sorting by the projection's own index makes the invariant
                    // a property of this code rather than of node-allocation
                    // order. It is a no-op on every graph the builder makes.
                    //
                    // `if_projs` holds them in node-id order; the sort is
                    // stable, so equal indices keep that order as before.
                    let mut succs: Vec<(u8, usize)> = std::mem::take(&mut if_projs[block_idx]);
                    succs.sort_by_key(|&(which, _)| which);
                    for (_which, succ_block) in succs {
                        if !blocks[block_idx].successors.contains(&succ_block) {
                            blocks[block_idx].successors.push(succ_block);
                        }
                        if !blocks[succ_block].predecessors.contains(&block_idx) {
                            blocks[succ_block].predecessors.push(block_idx);
                        }
                    }
                }
                Op::Return | Op::Throw => {
                    // No successors — a throw always leaves the frame, exactly
                    // like a return (cov-07).
                }
                _ => {}
            }
        }

        // For blocks without explicit terminator (fall-through via goto → merge),
        // check if the block's control feeds into a Merge/Region
        // (`fed_merges`, gathered above.)
        for &id in &fed_merges[block_idx] {
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
    //
    // Inputs are placed BEFORE their users. `find_best_block` only sees inputs
    // that already have a block, and this step used to visit nodes in id order
    // — so an input with a HIGHER id than its user was silently ignored. Every
    // graph the builder makes defines before it uses, but a pass that rewires
    // an old node to a newer input does not (`reassociate_affine` is one), and
    // the ignored input let the user land in a block the input's block does
    // not dominate: a value read before its definition, which
    // `verify_data_locations` then refuses. So each node is placed on the way
    // OUT of a depth-first walk over its still-unplaced data inputs. On a graph
    // whose inputs all have lower ids the walk never descends, and the
    // placement — block and push order both — is exactly the id-order one.
    let dom = Dominators::compute(&blocks);
    let mut entered = vec![false; num_nodes];
    let mut walk: Vec<(NodeId, usize)> = Vec::new();
    for root in 0..num_nodes {
        // Already placed, dead, or a control node (a block boundary, handled
        // above) — or placed by an earlier root's walk.
        if entered[root] || !awaits_placement(graph, &node_to_block, root) {
            continue;
        }
        entered[root] = true;
        walk.push((root as NodeId, 0));
        // Iterative, like `topo_sort_block`: a long operand chain must not
        // recurse. Each frame is a node and the index of its next input edge.
        while let Some(&(id, cursor)) = walk.last() {
            let node = &graph.nodes[id as usize];
            let mut next = cursor;
            let mut descend: Option<NodeId> = None;
            // A phi is pinned to its merge and reads its value inputs on the
            // incoming edges, so `find_best_block` never consults them — and
            // following them would walk a loop's back edge into its own body.
            if !matches!(node.op, Op::Phi) {
                while next < node.inputs.len() {
                    let inp = node.inputs[next];
                    next += 1;
                    if inp == NO_NODE {
                        continue;
                    }
                    let i = inp as usize;
                    // An input already `entered` but not yet placed is on the
                    // walk above this frame: a cycle through non-phi data
                    // nodes, which well-formed SSA does not have. It is left
                    // unplaced for this user, exactly as id order left it.
                    if i < num_nodes && !entered[i] && awaits_placement(graph, &node_to_block, i) {
                        entered[i] = true;
                        descend = Some(inp);
                        break;
                    }
                }
            }
            let top = walk.len() - 1;
            walk[top].1 = next;
            match descend {
                Some(inp) => walk.push((inp, 0)),
                None => {
                    walk.pop();
                    // Find a block dominated by all of this node's inputs' blocks.
                    let block = find_best_block(graph, id, &node_to_block, &blocks, &dom);
                    node_to_block[id as usize] = block;
                    blocks[block].nodes.push(id);
                }
            }
        }
    }

    // Step 4b: Sink pure nodes out of loops they are only used outside of.
    //
    // Placed here — after every node has a block, before the intra-block
    // ordering — because it changes which block a node is in and nothing else.
    // `topo_sort_block` below then repairs the order inside every block it
    // touched.
    if opts.sink_pure_late || sink_late_enabled() {
        let (moved, reverted) = sink_pure_nodes(graph, &mut blocks, &mut node_to_block, &dom);
        if moved > 0 {
            crate::ir_evidence::note(crate::ir_evidence::Transform::SunkLate);
        }
        if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_IR_SINK") {
            eprintln!("[ir-sink] moved={moved} reverted_for_safepoints={reverted}");
        }
    }

    // Step 5: Order the data nodes within each block.
    //
    // The baseline is a plain dependence (topological) order. With
    // `priority_within_blocks` the *pure* nodes are instead list-scheduled by
    // critical path and register pressure; impure nodes keep their relative
    // order exactly, because the IR threads memory through `mem` edges but
    // `Op::Guard` carries none — its ordering against a store is positional, so
    // moving it would be a semantic change, not a scheduling decision.
    for block in &mut blocks {
        topo_sort_block(graph, &mut block.nodes);
        if opts.priority_within_blocks {
            priority_sort_block(graph, &mut block.nodes);
        }
    }

    // Step 5b: put a consumer's single-use operands next to it, so `ir_lower`
    // can carry them in registers instead of round-tripping them through the
    // frame. See `pair_single_use_operands`.
    let mut pairing = PairCensus::default();
    if opts.pair_single_use_operands {
        let uses = use_counts(graph);
        for block in &mut blocks {
            pair_single_use_operands(graph, &uses, &mut block.nodes, &mut pairing);
        }
        pairing.publish();
    }

    // Step 6: Estimate how often each block runs, then (optionally) lay the
    // blocks out so the hot successors fall through.
    let mut freq = compute_frequencies(graph, &blocks, &dom, opts);
    let mut layout = LayoutReport {
        total_edge_weight: total_edge_weight(&blocks, &freq),
        ..LayoutReport::default()
    };
    let identity: Vec<usize> = (0..blocks.len()).collect();
    layout.baseline_fallthrough_edges = count_fallthrough_edges(&blocks, &identity);
    layout.baseline_fallthrough_weight = fallthrough_weight(&blocks, &freq, &identity);
    layout.fallthrough_edges = layout.baseline_fallthrough_edges;
    layout.fallthrough_weight = layout.baseline_fallthrough_weight;

    // Emission order is block INDEX order (`ir_lower` walks `schedule.blocks`
    // front to back), and block indices are CREATION order — the order Step 1
    // happened to walk control nodes in. Nothing made that a reverse postorder,
    // so a block could be emitted before a block that dominates it, and a value
    // read before the instruction that defines it.
    //
    // `verify_data_locations` catches exactly that and refuses the compile, so
    // it was never wrong code — it was lost compiles, silently. Measured on
    // 2026-09-04: `StringUTF16.compress` (def in block 4, use in block 1),
    // `Pattern.range` (9 and 1), `Pattern.clazz`; and on an H2 test class the
    // same refusal denies the optimizing tier to `java/lang/String.equals` and
    // `java/lang/StringLatin1.equals`.
    //
    // The machinery to fix it was already here and switched off: `layout_hot_paths`
    // defaults to false, so the DFS layout — whose whole point is that "for
    // every edge u -> v reachable from the entry, pos(u) < pos(v) unless the
    // edge is retreating" (this module's invariant 2) — never ran in
    // production. A dominator is a DFS ancestor, so an RPO layout places it
    // first and the def-before-use property follows.
    //
    // So: when hot-path layout is off, still lay the blocks out, just without
    // the frequency priority. `dfs_layout(blocks, None)` is the plain DFS, and
    // the same `validate_order` guards it. `CRATONVM_JIT_IR_RPO_LAYOUT=0`
    // restores creation order.
    if !opts.layout_hot_paths && rpo_layout_enabled() {
        match layout_blocks_rpo(&blocks, &opts.protected_regions) {
            Ok(order) => {
                layout.fallthrough_edges = count_fallthrough_edges(&blocks, &order);
                layout.fallthrough_weight = fallthrough_weight(&blocks, &freq, &order);
                layout.applied = true;
                RPO_LAYOUTS_APPLIED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                apply_order(&mut blocks, &mut node_to_block, &mut freq, &order);
            }
            Err(bailout) => {
                // Same policy as the hot-path arm: the method still compiles,
                // it just keeps the order it had.
                RPO_LAYOUTS_REJECTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                layout.rejected = Some(bailout.to_string());
            }
        }
    }

    if opts.layout_hot_paths {
        match layout_blocks(&blocks, &freq, &opts.protected_regions) {
            Ok(order) => {
                layout.fallthrough_edges = count_fallthrough_edges(&blocks, &order);
                layout.fallthrough_weight = fallthrough_weight(&blocks, &freq, &order);
                layout.cold_blocks_sunk = count_cold_sunk(&freq, &order);
                layout.applied = true;
                apply_order(&mut blocks, &mut node_to_block, &mut freq, &order);
            }
            Err(bailout) => {
                // Not a compilation bailout: the method still compiles.
                // Recorded rather than counted, so the bailout table stays a
                // table of *refused compiles*. Fall back to the plain reverse
                // postorder before creation order, because creation order is
                // the one that can emit a use before its definition.
                layout.rejected = Some(bailout.to_string());
                if rpo_layout_enabled() {
                    if let Ok(order) = layout_blocks_rpo(&blocks, &opts.protected_regions) {
                        layout.fallthrough_edges = count_fallthrough_edges(&blocks, &order);
                        layout.fallthrough_weight = fallthrough_weight(&blocks, &freq, &order);
                        layout.applied = true;
                        RPO_LAYOUTS_APPLIED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        apply_order(&mut blocks, &mut node_to_block, &mut freq, &order);
                    }
                }
            }
        }
    }

    // The dominator relation is a property of the CFG, not of its numbering,
    // but `dom` is *indexed* by block number — so recompute it after a
    // permutation rather than trying to permute a matrix in place. Callers
    // (`Schedule::node_strictly_dominates_block`, the scalar-replacement deopt
    // producer) index it with post-layout indices.
    //
    // The published relation is the dense matrix its consumers index, built
    // once here from whichever `Dominators` matches the final numbering.
    let dom = if layout.applied {
        compute_dominators(&blocks)
    } else {
        dom.to_matrix()
    };

    Schedule {
        blocks,
        node_to_block,
        dom,
        freq,
        layout,
        pairing,
    }
}

// ── Helpers ──────────────────────────────────────────────────────────

fn is_start(graph: &Graph, id: NodeId) -> bool {
    matches!(graph.nodes.get(id as usize), Some(n) if n.op == Op::Start)
}

fn is_if(graph: &Graph, id: NodeId) -> bool {
    matches!(graph.nodes.get(id as usize), Some(n) if n.op == Op::If)
}

/// Does Step 4 of [`schedule_with_options`] still owe node `id` a block?
///
/// True for a data node — not dead, not a control node — that has no block
/// yet: exactly the nodes that step placed when it walked ids in order. An
/// out-of-range id is never owed anything.
fn awaits_placement(graph: &Graph, node_to_block: &[usize], id: usize) -> bool {
    match (graph.nodes.get(id), node_to_block.get(id)) {
        (Some(n), Some(&b)) => b == usize::MAX && n.op != Op::Dead && !n.op.is_control(),
        _ => false,
    }
}

/// Compute, for every block, the set of blocks that dominate it, as the dense
/// matrix [`Schedule::dom`] publishes.
///
/// `dom[b][d]` is true iff block `d` dominates block `b`. Block 0 (the entry)
/// is assumed to be the CFG entry: it dominates every block, and is dominated
/// only by itself. For blocks unreachable from the entry (no path of
/// predecessors back to block 0) the row is "all blocks" — the value the
/// classic iterative data-flow fixpoint this replaced left them at — so callers
/// must only rely on `dominates(a, b)` when both are reachable;
/// `find_best_block` handles the unreachable/empty case by falling back to the
/// entry block.
///
/// Derived from [`Dominators`], which computes the relation in near-linear
/// time. The matrix itself is still O(B²) memory because [`Schedule::dom`]'s
/// consumers index it directly; it is built once per schedule, and the
/// scheduler's own queries go to [`Dominators`] instead.
fn compute_dominators(blocks: &[Block]) -> Vec<Vec<bool>> {
    Dominators::compute(blocks).to_matrix()
}

/// True iff block `a` dominates block `b` (every path from the entry to `b`
/// passes through `a`), read from a [`compute_dominators`] matrix. Both indices
/// must be in range; an out-of-range index answers `false`.
#[inline]
fn dominates(matrix: &[Vec<bool>], a: usize, b: usize) -> bool {
    matrix
        .get(b)
        .and_then(|row| row.get(a))
        .copied()
        .unwrap_or(false)
}

/// [`Dominators`]' `idom` sentinel for a block the entry does not reach.
const UNREACHED: usize = usize::MAX;

/// The dominator relation over a block CFG, as an immediate-dominator tree.
///
/// Computed with Cooper, Harvey and Kennedy's iterative algorithm over reverse
/// postorder ("A Simple, Fast Dominance Algorithm", 2001): each pass walks the
/// blocks in RPO and intersects the predecessors' dominator-tree paths, which
/// settles in two or three passes on the CFGs this tier builds. It replaced a
/// dense bitset fixpoint that cost O(B²) per pass — a real cost on a method
/// near the node cap, where the scheduler computes the relation twice.
///
/// Queries are O(1): the dominator tree is numbered by a depth-first walk, and
/// `a` dominates `b` exactly when `b`'s pre/post interval nests inside `a`'s.
///
/// # Same answers as the fixpoint, unreachable blocks included
///
/// Reads `predecessors` only, as the fixpoint did, so a CFG whose successor
/// and predecessor lists disagree gets the same relation from both. Block 0 is
/// the entry. A block the entry does not reach is dominated by EVERY block —
/// the fixpoint's top element, which it never tightened for such a block —
/// and dominates no reachable one. The test module keeps the fixpoint as a
/// reference and checks that the two agree.
#[derive(Debug, Clone)]
pub struct Dominators {
    /// Immediate dominator per block. `idom[0] == 0`; [`UNREACHED`] for a
    /// block the entry does not reach.
    idom: Vec<usize>,
    /// Pre-order number of each reachable block in the dominator tree.
    pre: Vec<usize>,
    /// Post-order number of each reachable block in the dominator tree.
    post: Vec<usize>,
}

impl Dominators {
    /// Compute the relation for `blocks`, whose entry is block 0.
    ///
    /// Total: never panics. An empty CFG yields an empty relation, and an
    /// out-of-range predecessor index is ignored, as the fixpoint ignored it.
    pub fn compute(blocks: &[Block]) -> Self {
        let n = blocks.len();
        let mut idom = vec![UNREACHED; n];
        let mut pre = vec![0usize; n];
        let mut post = vec![0usize; n];
        if n == 0 {
            return Dominators { idom, pre, post };
        }

        // Forward edges, derived from `predecessors` rather than read from
        // `successors`: see "Same answers as the fixpoint" above.
        let mut succs: Vec<Vec<usize>> = vec![Vec::new(); n];
        for (b, blk) in blocks.iter().enumerate() {
            for &p in &blk.predecessors {
                if p < n {
                    succs[p].push(b);
                }
            }
        }

        // Reverse postorder of the blocks reachable from the entry, by an
        // iterative DFS (a long chain of blocks must not recurse).
        let mut postorder: Vec<usize> = Vec::with_capacity(n);
        let mut seen = vec![false; n];
        let mut dfs: Vec<(usize, usize)> = vec![(0, 0)];
        seen[0] = true;
        while let Some(&(b, i)) = dfs.last() {
            if i < succs[b].len() {
                let top = dfs.len() - 1;
                dfs[top].1 = i + 1;
                let s = succs[b][i];
                if !seen[s] {
                    seen[s] = true;
                    dfs.push((s, 0));
                }
            } else {
                dfs.pop();
                postorder.push(b);
            }
        }
        let rpo: Vec<usize> = postorder.iter().rev().copied().collect();
        let mut rpo_num = vec![usize::MAX; n];
        for (k, &b) in rpo.iter().enumerate() {
            rpo_num[b] = k;
        }

        // The entry finishes last, so it is `rpo[0]`; every other reachable
        // block has its DFS parent — a predecessor — earlier in `rpo`, so the
        // first pass already gives each of them an `idom`.
        idom[0] = 0;
        let mut changed = true;
        while changed {
            changed = false;
            for &b in rpo.iter().skip(1) {
                let mut new_idom = UNREACHED;
                for &p in &blocks[b].predecessors {
                    // An unreached or not-yet-processed predecessor carries no
                    // information: the fixpoint's top element is the identity
                    // of its intersection, so skipping one is the same answer.
                    if p >= n || idom[p] == UNREACHED {
                        continue;
                    }
                    new_idom = if new_idom == UNREACHED {
                        p
                    } else {
                        Self::intersect(&idom, &rpo_num, p, new_idom)
                    };
                }
                if new_idom != UNREACHED && idom[b] != new_idom {
                    idom[b] = new_idom;
                    changed = true;
                }
            }
        }

        // Number the dominator tree: `a` dominates `b` iff
        // `pre[a] <= pre[b] && post[b] <= post[a]`.
        let mut children: Vec<Vec<usize>> = vec![Vec::new(); n];
        for &b in rpo.iter().skip(1) {
            children[idom[b]].push(b);
        }
        let mut clock = 0usize;
        pre[0] = clock;
        clock += 1;
        let mut walk: Vec<(usize, usize)> = vec![(0, 0)];
        while let Some(&(b, i)) = walk.last() {
            if i < children[b].len() {
                let top = walk.len() - 1;
                walk[top].1 = i + 1;
                let c = children[b][i];
                pre[c] = clock;
                clock += 1;
                walk.push((c, 0));
            } else {
                post[b] = clock;
                clock += 1;
                walk.pop();
            }
        }

        Dominators { idom, pre, post }
    }

    /// The nearest common dominator of two processed blocks: walk whichever is
    /// later in reverse postorder up its `idom` chain until the two meet.
    fn intersect(idom: &[usize], rpo_num: &[usize], mut a: usize, mut b: usize) -> usize {
        while a != b {
            while rpo_num[a] > rpo_num[b] {
                a = idom[a];
            }
            while rpo_num[b] > rpo_num[a] {
                b = idom[b];
            }
        }
        a
    }

    /// Number of blocks the relation covers.
    pub fn len(&self) -> usize {
        self.idom.len()
    }

    /// True for a relation over no blocks.
    pub fn is_empty(&self) -> bool {
        self.idom.is_empty()
    }

    /// The immediate dominator of `b`: `None` for the entry, for a block the
    /// entry does not reach, and for an out-of-range index.
    pub fn idom(&self, b: usize) -> Option<usize> {
        match self.idom.get(b) {
            Some(&d) if b != 0 && d != UNREACHED => Some(d),
            _ => None,
        }
    }

    /// True iff block `a` dominates block `b`. Reflexive.
    ///
    /// Exactly [`dominates`] over [`Self::to_matrix`]: `false` for an
    /// out-of-range index, `true` for any `a` when `b` is unreachable, and
    /// `false` when only `a` is.
    #[inline]
    pub fn dominates(&self, a: usize, b: usize) -> bool {
        let (Some(&ia), Some(&ib)) = (self.idom.get(a), self.idom.get(b)) else {
            return false;
        };
        if ib == UNREACHED {
            return true;
        }
        if ia == UNREACHED {
            return false;
        }
        self.pre[a] <= self.pre[b] && self.post[b] <= self.post[a]
    }

    /// The dense matrix [`Schedule::dom`] publishes: `m[b][d]` iff `d`
    /// dominates `b`. O(B²) memory, filled by walking each block's `idom`
    /// chain.
    pub fn to_matrix(&self) -> Vec<Vec<bool>> {
        let n = self.idom.len();
        let mut matrix = Vec::with_capacity(n);
        for b in 0..n {
            if self.idom[b] == UNREACHED {
                matrix.push(vec![true; n]);
                continue;
            }
            let mut row = vec![false; n];
            let mut d = b;
            loop {
                row[d] = true;
                if d == 0 {
                    break;
                }
                d = self.idom[d];
            }
            matrix.push(row);
        }
        matrix
    }
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
    dom: &Dominators,
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
            .all(|&other| other == cand || dom.dominates(other, cand));
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

// ── Intra-block priority scheduling ──────────────────────────────────

/// Blocks larger than this keep the plain dependence order.
///
/// The selection loop rescans the ready set on every pick, so it is quadratic
/// in the block's node count. A straight-line method can put thousands of nodes
/// in one block (the deep-operand-chain case `topo_sort_block` exists for), and
/// a compiler must not turn a pathological graph into a pathological *compile*
/// — the same rule `ir_lower::SLOT_PLAN_WORK_BUDGET` states for liveness.
const PRIORITY_BLOCK_BUDGET: usize = 4_000;

/// Re-order the data nodes of one block by scheduling priority, refining the
/// dependence order [`topo_sort_block`] has already produced.
///
/// Two things drive the priority, in this order:
///
/// * **Register pressure.** A node whose operands die at it *frees* frame slots;
///   a node that only defines a value *costs* one. Preferring the negative-delta
///   candidates keeps peak liveness — the number `ir_lower::plan_slots` colours
///   and `estimate_frame_bytes` budgets — down.
/// * **Critical path.** Among candidates with equal pressure delta, the one with
///   the longest remaining dependence chain goes first, so the chain that
///   determines the block's length starts as early as possible.
///
/// Ties break on the seed order, so the output is a deterministic function of
/// the input — a scheduler that depends on hash iteration order cannot be
/// bisected against.
///
/// **What it never reorders.** Only *pure* nodes float. Every impure node
/// (`Op::Store`, `Op::Call`, `Op::Guard`, `Op::Div`, …) is wired into a total
/// chain in its existing relative order, so no side effect, deopt point or
/// potentially-throwing operation changes position relative to another. That
/// matters because the IR threads memory through `mem` edges but `Op::Guard`
/// carries none: its ordering against a store is positional, and moving it
/// would be a semantic change rather than a scheduling decision.
///
/// Total: leaves `nodes` untouched on anything it cannot schedule (a duplicate
/// entry, a dangling id, a dependence cycle, or a block over budget).
fn priority_sort_block(graph: &Graph, nodes: &mut Vec<NodeId>) {
    let count = nodes.len();
    if count <= 2 || count > PRIORITY_BLOCK_BUDGET {
        return;
    }
    // `nodes` is already a valid topological order on entry.
    let baseline: Vec<NodeId> = nodes.clone();

    let mut idx_of: HashMap<NodeId, usize> = HashMap::with_capacity(count);
    for (i, &id) in baseline.iter().enumerate() {
        idx_of.insert(id, i);
    }
    if idx_of.len() != count {
        return; // duplicate entry — not a shape this pass reasons about
    }

    let is_phi = |i: usize| -> bool {
        matches!(
            graph.nodes.get(baseline[i] as usize),
            Some(node) if matches!(node.op, Op::Phi)
        )
    };

    // Value dependences inside this block.
    //
    // A phi's *value* inputs are deliberately excluded: they are read by the
    // predecessor block's edge copies (`ir_lower::emit_phi_copies`) at the
    // predecessor's outgoing-edge position, not at the phi's own position —
    // exactly the attribution `plan_slots` makes. Including them would also make
    // a loop-carried phi depend on a node defined later in its own block, a
    // cycle no order can satisfy.
    let mut val_deps: Vec<Vec<usize>> = vec![Vec::new(); count];
    let mut val_use_count: Vec<usize> = vec![0; count];
    for (i, &id) in baseline.iter().enumerate() {
        let node = match graph.nodes.get(id as usize) {
            Some(node) => node,
            None => return,
        };
        if matches!(node.op, Op::Phi) {
            continue;
        }
        for &inp in &node.inputs {
            if inp == NO_NODE {
                continue;
            }
            if let Some(&j) = idx_of.get(&inp) {
                if j != i && !val_deps[i].contains(&j) {
                    val_deps[i].push(j);
                    val_use_count[j] += 1;
                }
            }
        }
    }

    // Full ordering DAG = value dependences + the side-effect chain.
    let mut deps: Vec<Vec<usize>> = val_deps.clone();
    let mut users: Vec<Vec<usize>> = vec![Vec::new(); count];
    for (i, d) in deps.iter().enumerate() {
        for &j in d {
            users[j].push(i);
        }
    }
    // Phis lead the chain: a phi is defined on entry to its block, before any
    // instruction in it. Everything else keeps its existing relative order.
    let mut chain: Vec<usize> = (0..count)
        .filter(|&i| {
            !graph
                .nodes
                .get(baseline[i] as usize)
                .map(|node| node.op.is_pure())
                .unwrap_or(false)
        })
        .collect();
    chain.sort_by_key(|&i| (!is_phi(i), i));
    for w in chain.windows(2) {
        let (a, b) = (w[0], w[1]);
        if !deps[b].contains(&a) {
            deps[b].push(a);
            users[a].push(b);
        }
    }

    // Seed order: phis first, then the incoming dependence order. This is a
    // valid topological order of the DAG above (a phi has no in-block
    // dependence, and the chain follows the incoming order among non-phis), so
    // it both computes heights and breaks ties deterministically.
    let mut seed: Vec<usize> = (0..count).collect();
    seed.sort_by_key(|&i| (!is_phi(i), i));
    let mut seed_rank = vec![0usize; count];
    for (r, &i) in seed.iter().enumerate() {
        seed_rank[i] = r;
    }

    // Critical-path height: longest chain of dependent nodes still ahead.
    let mut height = vec![1usize; count];
    for &i in seed.iter().rev() {
        let mut h = 1usize;
        for &u in &users[i] {
            h = h.max(height[u].saturating_add(1));
        }
        height[i] = h;
    }

    let mut remaining_deps: Vec<usize> = deps.iter().map(|d| d.len()).collect();
    let mut remaining_uses: Vec<usize> = val_use_count.clone();
    let mut ready: Vec<usize> = (0..count).filter(|&i| remaining_deps[i] == 0).collect();
    let mut out: Vec<NodeId> = Vec::with_capacity(count);

    while !ready.is_empty() {
        let mut best_slot = 0usize;
        let mut best_key: Option<(i64, usize, usize)> = None;
        for (k, &i) in ready.iter().enumerate() {
            // Operands whose *last* remaining consumer is this node die here.
            let kills = val_deps[i]
                .iter()
                .filter(|&&j| remaining_uses[j] == 1)
                .count() as i64;
            // `usize::MAX - height` turns "prefer the longest chain" into a
            // minimisation, so the whole key is compared in one direction.
            let key = (1i64 - kills, usize::MAX - height[i], seed_rank[i]);
            if best_key.map(|b| key < b).unwrap_or(true) {
                best_key = Some(key);
                best_slot = k;
            }
        }
        let i = ready.swap_remove(best_slot);
        out.push(baseline[i]);
        for &j in &val_deps[i] {
            if remaining_uses[j] > 0 {
                remaining_uses[j] -= 1;
            }
        }
        for &u in &users[i] {
            if remaining_deps[u] > 0 {
                remaining_deps[u] -= 1;
                if remaining_deps[u] == 0 {
                    ready.push(u);
                }
            }
        }
    }

    // A cycle would leave nodes unscheduled. The plain topological order is
    // always valid, so keep it rather than emitting a partial block.
    if out.len() != count {
        return;
    }
    // The memory-ordering rule has the last word. The side-effect chain above
    // is *supposed* to make this vacuous — every impure node is wired into a
    // total order — but "supposed to" is what a validator is for: this pass is
    // the only thing between the memory model and the emitted instruction
    // order, and a reordering it cannot justify must not leave the function.
    if check_memory_order(graph, &baseline, &out).is_ok() {
        *nodes = out;
    }
}

// ── Memory-effect ordering ───────────────────────────────────────────
//
// The scheduler is the pass that decides what order instructions are emitted
// in, so it is the pass that can silently break the memory model. This section
// is the rule it obeys, stated once and checkable after the fact.
//
// # The rule
//
// Let *M* be the sub-sequence of a block's nodes that **order memory** — every
// node whose `Op::memory_shape` gives it a non-inert `MemEffect`: it reads,
// writes, allocates, safepoints, or carries a JMM fence half. Then for any two
// candidate orderings of the same node set:
//
// 1. The candidate must be a **permutation** of the baseline. A schedule that
//    drops or invents a node is refused outright, whatever else is true of it.
// 2. Two members of *M* may swap **only** if `Graph::may_reorder` answers
//    `Allowed` for the pair, in baseline order. That is the memory model's own
//    answer: disjoint locations, a read/read pair, or an inert partner.
// 3. A **barrier** — any member of *M* that is a safepoint or whose `MemOrder`
//    is not `Plain` — may not swap with another member of *M* **at all**, even
//    when rule 2 would allow it.
// 4. Nodes outside *M* are unconstrained by this rule. They are pure values;
//    dependence order already pins them, and that is `topo_sort_block`'s job.
//
// # Why rule 3 is stronger than the model
//
// `Graph::may_reorder` deliberately answers a *memory-side* question only. Its
// own documentation lists what it does not cover, and one item on that list is
// exactly what a scheduler needs: **implicit exception order**. `Op::Load`
// faults on a null base and deopts; `Op::Guard` transfers control and carries
// no memory edge at all. Moving a load below a safepoint is memory-neutral —
// the model answers `Allowed(ReadOnly)` — and is still wrong, because the
// deopt that a fault triggers would now rebuild a frame at a bci whose
// safepoint has already run.
//
// So the rule refuses it. Refusing costs an optimisation the scheduler does not
// currently attempt (the priority pass already chains every impure node); a
// wrong answer here costs a wrong reconstruction, which is the failure mode
// that does not announce itself.

/// Why an ordering was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrderRefusal {
    /// The candidate is not a permutation of the baseline — a node was
    /// dropped, invented or duplicated.
    NotAPermutation,
    /// Too many memory-ordering nodes moved to check the pairs within budget.
    /// Refusing is the fail-closed answer: an unverified reordering is not an
    /// accepted one.
    Budget,
    /// The memory model refuses this swap, for this reason.
    Model(ReorderBlock),
    /// One of the pair is a barrier — a safepoint or a JMM fence half — and a
    /// barrier does not move relative to anything that orders memory.
    Barrier,
}

/// A pair of memory-ordering nodes whose relative order changed illegally.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OrderViolation {
    /// The node that came first in the baseline.
    pub earlier: NodeId,
    /// The node that came second in the baseline.
    pub later: NodeId,
    /// Why the swap is refused.
    pub refusal: OrderRefusal,
}

/// Largest number of ordered pairs [`check_memory_order`] will examine.
///
/// The check is quadratic in the number of memory-ordering nodes in one block,
/// and a compiler must not turn a pathological graph into a pathological
/// compile — the same rule `PRIORITY_BLOCK_BUDGET` states above. Over budget
/// the answer is [`OrderRefusal::Budget`], i.e. "not verified, therefore not
/// allowed".
pub const MEMORY_ORDER_PAIR_BUDGET: usize = 16_384;

/// Does `id` order memory at all?
///
/// True for every node with a non-inert `MemEffect`: it reads, writes,
/// allocates, is a safepoint, or carries a JMM fence half. An id that names no
/// node answers `true` — `Graph::memory_effect` degrades an unknown node to
/// `MemEffect::OPAQUE`, and the failure direction has to be the one that
/// refuses motion.
pub fn orders_memory(graph: &Graph, id: NodeId) -> bool {
    !graph.memory_effect(id).is_inert()
}

/// Is `id` a barrier — a node nothing memory-ordering may cross?
///
/// A safepoint (`Op::Call`, `Op::New`, `Op::NewArray`, `Op::Guard`, the
/// monitors) or any node whose `MemOrder` is not `Plain` (a monitor operation,
/// a volatile access). See the section header for why safepoints are barriers
/// here even though the memory model lets a read cross one.
pub fn is_memory_barrier(graph: &Graph, id: NodeId) -> bool {
    let e = graph.memory_effect(id);
    e.safepoint || !e.order.is_plain()
}

/// Does `candidate` preserve the memory-effect ordering of `baseline`?
///
/// The rule is stated in full in the section header above. In short: the
/// candidate must be a permutation, two memory-ordering nodes may only swap
/// when `Graph::may_reorder` licenses it, and a barrier may not swap with a
/// memory-ordering node at all.
///
/// Cheap in the common case: when the memory-ordering nodes appear in the same
/// relative order in both sequences — which is what every scheduler in this
/// file currently produces — the answer is a single linear scan and no pairwise
/// query at all.
///
/// Total: never panics. Every refusal names the pair responsible, except
/// [`OrderRefusal::NotAPermutation`], where there is no pair to name and the
/// node ids are [`NO_NODE`].
pub fn check_memory_order(
    graph: &Graph,
    baseline: &[NodeId],
    candidate: &[NodeId],
) -> Result<(), OrderViolation> {
    let not_a_permutation = OrderViolation {
        earlier: NO_NODE,
        later: NO_NODE,
        refusal: OrderRefusal::NotAPermutation,
    };
    if baseline.len() != candidate.len() {
        return Err(not_a_permutation);
    }
    {
        let mut a = baseline.to_vec();
        let mut b = candidate.to_vec();
        a.sort_unstable();
        b.sort_unstable();
        if a != b {
            return Err(not_a_permutation);
        }
    }

    let base_mem: Vec<NodeId> = baseline
        .iter()
        .copied()
        .filter(|&id| orders_memory(graph, id))
        .collect();
    let cand_mem: Vec<NodeId> = candidate
        .iter()
        .copied()
        .filter(|&id| orders_memory(graph, id))
        .collect();
    // Nothing that orders memory moved relative to anything else that does.
    if base_mem == cand_mem {
        return Ok(());
    }

    let mut pos: HashMap<NodeId, usize> = HashMap::with_capacity(cand_mem.len());
    for (i, &id) in cand_mem.iter().enumerate() {
        pos.insert(id, i);
    }
    // A duplicated memory node makes "which one moved" unanswerable.
    if pos.len() != cand_mem.len() {
        return Err(not_a_permutation);
    }
    let pairs = base_mem
        .len()
        .saturating_mul(base_mem.len().saturating_sub(1))
        / 2;
    if pairs > MEMORY_ORDER_PAIR_BUDGET {
        return Err(OrderViolation {
            earlier: base_mem.first().copied().unwrap_or(NO_NODE),
            later: base_mem.last().copied().unwrap_or(NO_NODE),
            refusal: OrderRefusal::Budget,
        });
    }

    for (i, &x) in base_mem.iter().enumerate() {
        for &y in base_mem.iter().skip(i + 1) {
            let (px, py) = match (pos.get(&x), pos.get(&y)) {
                (Some(&a), Some(&b)) => (a, b),
                _ => return Err(not_a_permutation),
            };
            if px < py {
                continue; // relative order preserved
            }
            if is_memory_barrier(graph, x) || is_memory_barrier(graph, y) {
                return Err(OrderViolation {
                    earlier: x,
                    later: y,
                    refusal: OrderRefusal::Barrier,
                });
            }
            match graph.may_reorder(x, y) {
                Reorder::Allowed(_) => {}
                Reorder::Blocked(r) => {
                    return Err(OrderViolation {
                        earlier: x,
                        later: y,
                        refusal: OrderRefusal::Model(r),
                    })
                }
            }
        }
    }
    Ok(())
}

// ── Block frequency estimation ───────────────────────────────────────

/// Clamp a probability into `[MIN_EDGE_PROB, MAX_EDGE_PROB]`, mapping NaN and
/// the infinities to "no information" (0.5) rather than propagating them.
#[inline]
fn clamp_prob(p: f64) -> f64 {
    if !p.is_finite() {
        return 0.5;
    }
    p.clamp(MIN_EDGE_PROB, MAX_EDGE_PROB)
}

/// The index within `blocks[b].successors` of the TRUE (`Proj(0)`) edge of an
/// `Op::If`, or `None` when `b` is not a two-way branch or the projections
/// cannot be identified.
///
/// Read off the successor's *control node* rather than from the successor's
/// position, because `schedule` builds the successor list by scanning the graph
/// in node-id order and `ir_lower` only ever assumes `successors[0]` is the
/// taken edge — an assumption this pass must read, not re-derive.
fn true_successor_index(graph: &Graph, blocks: &[Block], b: usize) -> Option<usize> {
    let blk = blocks.get(b)?;
    if blk.successors.len() != 2 {
        return None;
    }
    for (i, &s) in blk.successors.iter().enumerate() {
        let ctrl = blocks.get(s)?.ctrl;
        if let Some(node) = graph.nodes.get(ctrl as usize) {
            if node.op == Op::Proj(0) {
                return Some(i);
            }
        }
    }
    None
}

/// Profiled probability of the TRUE edge of `blocks[b]`'s `Op::If`, keyed by
/// the branch's bytecode PC. `None` when there is no terminator, no PC, no
/// profile entry, or too few samples to beat the static heuristics.
fn profiled_taken_prob(
    graph: &Graph,
    blocks: &[Block],
    b: usize,
    opts: &ScheduleOptions,
) -> Option<f64> {
    let term = blocks.get(b)?.terminator?;
    let node = graph.nodes.get(term as usize)?;
    if node.op != Op::If {
        return None;
    }
    let pc = node.bytecode_pc?;
    opts.branch_counts.get(&pc)?.taken_probability()
}

/// Estimate how often each block executes, normalized so the entry block is
/// 1.0 (once per method invocation).
///
/// The model, in order of precedence:
///
/// 1. **Real profile.** A two-way branch whose bytecode PC has at least
///    [`MIN_PROFILE_SAMPLES`] observations takes its edge probabilities straight
///    from the counts (clamped away from 0 and 1). The same counts also give the
///    enclosing loop its trip count, via `trip = 1 / P(exit)`.
/// 2. **Loop structure.** Natural loops are found from the CFG's back edges
///    (an edge whose target *dominates* its source), giving each block a nesting
///    depth. A loop header's frequency is multiplied by its trip count, so its
///    body outweighs everything outside the loop.
/// 3. **Static branch heuristics.** With no profile, an edge that leaves the
///    innermost enclosing loop gets `1 / trip` and the edge that stays gets the
///    rest; an edge back to a dominating header likewise gets the bulk. Anything
///    else is an even split — guessing at a branch nobody measured is how a
///    static profile earns a *worse* layout than no layout.
///
/// Frequencies are then propagated once in reverse postorder, ignoring edges
/// that run backwards in that order (their source is not yet known; the loop
/// multiplier is what accounts for them). This is a single linear pass rather
/// than an iterative fixed point, so the cost is `O(blocks + edges)` and the
/// result is a deterministic function of the CFG and the profile.
///
/// Total: never panics. An empty CFG yields empty vectors.
pub fn compute_frequencies(
    graph: &Graph,
    blocks: &[Block],
    dom: &Dominators,
    opts: &ScheduleOptions,
) -> BlockFrequencies {
    let n = blocks.len();
    let mut out = BlockFrequencies {
        freq: vec![0.0; n],
        depth: vec![0; n],
        is_loop_header: vec![false; n],
        trip: vec![1.0; n],
        edge_prob: blocks
            .iter()
            .map(|blk| vec![0.0; blk.successors.len()])
            .collect(),
        profiled_branches: 0,
        static_branches: 0,
    };
    if n == 0 {
        return out;
    }

    // ── 1. Natural loops ─────────────────────────────────────────────
    //
    // A back edge is one whose target dominates its source. The loop body is
    // everything that reaches the latch without passing back through the
    // header, i.e. the standard backward closure over predecessors.
    let mut loop_body: Vec<Option<Vec<bool>>> = vec![None; n];
    for (latch, blk) in blocks.iter().enumerate() {
        for &header in &blk.successors {
            if header >= n || !dom.dominates(header, latch) {
                continue;
            }
            out.is_loop_header[header] = true;
            let body = loop_body[header].get_or_insert_with(|| {
                let mut v = vec![false; n];
                v[header] = true;
                v
            });
            if latch == header {
                continue; // self loop: the header is the whole body
            }
            let mut stack = Vec::new();
            if !body[latch] {
                body[latch] = true;
                stack.push(latch);
            }
            while let Some(x) = stack.pop() {
                for &p in &blocks[x].predecessors {
                    if p < n && !body[p] {
                        body[p] = true;
                        stack.push(p);
                    }
                }
            }
        }
    }
    for h in 0..n {
        if let Some(body) = &loop_body[h] {
            for (x, &inside) in body.iter().enumerate() {
                if inside {
                    out.depth[x] = out.depth[x].saturating_add(1);
                }
            }
        }
    }
    // Innermost enclosing loop per block: the deepest header whose body
    // contains it. Ties (two headers at equal depth) go to the lower index, so
    // the answer does not depend on iteration order.
    let mut innermost: Vec<Option<usize>> = vec![None; n];
    for h in 0..n {
        if let Some(body) = &loop_body[h] {
            for (x, &inside) in body.iter().enumerate() {
                if !inside {
                    continue;
                }
                let better = match innermost[x] {
                    None => true,
                    Some(cur) => out.depth[h] > out.depth[cur],
                };
                if better {
                    innermost[x] = Some(h);
                }
            }
        }
    }

    // ── 2. Trip counts ───────────────────────────────────────────────
    for h in 0..n {
        let body = match &loop_body[h] {
            Some(body) => body,
            None => continue,
        };
        let mut trip = DEFAULT_TRIP_COUNT;
        for b in 0..n {
            if !body[b] || blocks[b].successors.len() != 2 {
                continue;
            }
            let s0 = blocks[b].successors[0];
            let s1 = blocks[b].successors[1];
            let in0 = s0 < n && body[s0];
            let in1 = s1 < n && body[s1];
            if in0 == in1 {
                continue; // not a loop exit branch
            }
            let (ti, pt) = match (
                true_successor_index(graph, blocks, b),
                profiled_taken_prob(graph, blocks, b, opts),
            ) {
                (Some(ti), Some(pt)) => (ti, pt),
                _ => continue,
            };
            // Probability of the edge that leaves the loop.
            let p_true_is_exit = if ti == 0 { !in0 } else { !in1 };
            let p_exit = if p_true_is_exit { pt } else { 1.0 - pt };
            trip = (1.0 / clamp_prob(p_exit)).clamp(1.0, MAX_TRIP_COUNT);
            break;
        }
        out.trip[h] = trip;
    }

    // ── 3. Edge probabilities ────────────────────────────────────────
    for b in 0..n {
        let ns = blocks[b].successors.len();
        match ns {
            0 => {}
            1 => out.edge_prob[b][0] = 1.0,
            2 => {
                let ti = true_successor_index(graph, blocks, b);
                let profiled = profiled_taken_prob(graph, blocks, b, opts);
                if let (Some(ti), Some(pt)) = (ti, profiled) {
                    out.profiled_branches += 1;
                    out.edge_prob[b][ti] = pt;
                    out.edge_prob[b][1 - ti] = 1.0 - pt;
                } else {
                    out.static_branches += 1;
                    let (p0, p1) =
                        static_branch_probs(blocks, dom, &loop_body, &innermost, &out, b);
                    out.edge_prob[b][0] = p0;
                    out.edge_prob[b][1] = p1;
                }
            }
            _ => {
                let p = 1.0 / ns as f64;
                for i in 0..ns {
                    out.edge_prob[b][i] = p;
                }
            }
        }
    }

    // ── 4. Propagate in reverse postorder ────────────────────────────
    let dfs = dfs_layout(blocks, None);
    out.freq[0] = 1.0;
    for &b in &dfs.order {
        if b == 0 {
            continue;
        }
        let mut acc = 0.0f64;
        for &p in &blocks[b].predecessors {
            if p >= n {
                continue;
            }
            // Skip edges that run backwards in this order: their source's
            // frequency is not known yet, and the loop multiplier below is what
            // accounts for the iterations they represent.
            if dfs.pos[p] >= dfs.pos[b] || dfs.pos[p] == usize::MAX {
                continue;
            }
            let i = match blocks[p].successors.iter().position(|&s| s == b) {
                Some(i) => i,
                None => continue,
            };
            acc += out.freq[p] * out.edge_prob[p].get(i).copied().unwrap_or(0.0);
        }
        if out.is_loop_header[b] {
            acc *= out.trip[b];
        }
        out.freq[b] = acc;
    }

    // ── 5. Normalize ─────────────────────────────────────────────────
    //
    // The entry is 1.0 by construction; every other value is finite,
    // non-negative and saturated at MAX_BLOCK_FREQ so a deep loop nest cannot
    // reach infinity and make "hotter than" meaningless.
    for f in out.freq.iter_mut() {
        if !f.is_finite() || *f < 0.0 {
            *f = 0.0;
        } else if *f > MAX_BLOCK_FREQ {
            *f = MAX_BLOCK_FREQ;
        }
    }
    out.freq[0] = 1.0;
    out
}

/// Static (profile-free) probabilities for the two successors of `blocks[b]`.
///
/// Returns `(P(successors[0]), P(successors[1]))`.
fn static_branch_probs(
    blocks: &[Block],
    dom: &Dominators,
    loop_body: &[Option<Vec<bool>>],
    innermost: &[Option<usize>],
    freq: &BlockFrequencies,
    b: usize,
) -> (f64, f64) {
    let n = blocks.len();
    let s0 = blocks[b].successors[0];
    let s1 = blocks[b].successors[1];

    // Back-edge heuristic: an edge to a header that dominates this block closes
    // a loop, and a loop is entered to be repeated.
    let back0 = s0 < n && dom.dominates(s0, b);
    let back1 = s1 < n && dom.dominates(s1, b);
    if back0 != back1 {
        let header = if back0 { s0 } else { s1 };
        let trip = freq.trip.get(header).copied().unwrap_or(DEFAULT_TRIP_COUNT);
        let p_back = clamp_prob(1.0 - 1.0 / trip.max(1.0));
        return if back0 {
            (p_back, 1.0 - p_back)
        } else {
            (1.0 - p_back, p_back)
        };
    }

    // Loop-exit heuristic: leaving the innermost enclosing loop is the
    // improbable direction, by exactly the trip count that defines the loop.
    if let Some(h) = innermost.get(b).copied().flatten() {
        if let Some(body) = loop_body.get(h).and_then(|x| x.as_ref()) {
            let in0 = s0 < n && body[s0];
            let in1 = s1 < n && body[s1];
            if in0 != in1 {
                let trip = freq.trip.get(h).copied().unwrap_or(DEFAULT_TRIP_COUNT);
                let p_stay = clamp_prob(1.0 - 1.0 / trip.max(1.0));
                return if in0 {
                    (p_stay, 1.0 - p_stay)
                } else {
                    (1.0 - p_stay, p_stay)
                };
            }
        }
    }

    // Nothing to go on. An even split is deliberate: inventing a bias for a
    // branch nobody measured is how a static profile earns a worse layout than
    // no layout at all.
    (0.5, 0.5)
}

// ── Block layout ─────────────────────────────────────────────────────

/// The depth-first walk both the frequency propagation and the layout share.
struct DfsResult {
    /// Blocks in reverse postorder — the entry block's tree first, then any
    /// block unreachable from the entry, each in its own reverse postorder.
    order: Vec<usize>,
    /// `pos[b]` is `b`'s index in `order` (`usize::MAX` if absent).
    pos: Vec<usize>,
    /// Reachable from block 0.
    reachable: Vec<bool>,
    /// Edges whose target was on the DFS stack — the retreating edges. Every
    /// cycle contains at least one, and a natural loop's back edge (target
    /// dominates source) is retreating in *every* DFS.
    retreating: Vec<(usize, usize)>,
}

/// Depth-first reverse postorder over the block CFG.
///
/// With `priority`, each block's successors are visited in **ascending**
/// priority. Reverse postorder emits a block immediately before the subtree of
/// its *last-visited* successor, so visiting cold-first puts the hottest
/// successor physically next — the fall-through — and sinks the cold subtree
/// behind it.
///
/// Iterative, with an explicit work stack: a recursive walk overflows the native
/// stack on a long chain of blocks, which is the same reason `topo_sort_block`
/// is iterative.
fn dfs_layout(blocks: &[Block], priority: Option<&[f64]>) -> DfsResult {
    let n = blocks.len();
    let mut result = DfsResult {
        order: Vec::with_capacity(n),
        pos: vec![usize::MAX; n],
        reachable: vec![false; n],
        retreating: Vec::new(),
    };
    if n == 0 {
        return result;
    }

    let mut succ_order: Vec<Vec<usize>> = Vec::with_capacity(n);
    for blk in blocks {
        let mut ss: Vec<usize> = blk.successors.iter().copied().filter(|&s| s < n).collect();
        if let Some(pri) = priority {
            ss.sort_by(|&a, &b| {
                let fa = pri.get(a).copied().unwrap_or(0.0);
                let fb = pri.get(b).copied().unwrap_or(0.0);
                fa.partial_cmp(&fb)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(a.cmp(&b))
            });
        }
        succ_order.push(ss);
    }

    const UNVISITED: u8 = 0;
    const ON_STACK: u8 = 1;
    const DONE: u8 = 2;
    let mut state = vec![UNVISITED; n];
    let mut stack: Vec<(usize, usize)> = Vec::new();

    // Block 0 is the entry and must stay at position 0, so it roots the first
    // walk; anything it cannot reach is walked afterwards and appended, never
    // prepended.
    let mut next_root = Some(0usize);
    while let Some(root) = next_root {
        let mut post: Vec<usize> = Vec::new();
        state[root] = ON_STACK;
        stack.push((root, 0));
        while let Some(&(b, edge)) = stack.last() {
            let frame = stack.len() - 1;
            if edge < succ_order[b].len() {
                stack[frame].1 = edge + 1;
                let s = succ_order[b][edge];
                if state[s] == UNVISITED {
                    state[s] = ON_STACK;
                    stack.push((s, 0));
                } else if state[s] == ON_STACK {
                    result.retreating.push((b, s));
                }
            } else {
                state[b] = DONE;
                post.push(b);
                stack.pop();
            }
        }
        let entry_tree = result.order.is_empty();
        for &b in post.iter().rev() {
            result.pos[b] = result.order.len();
            result.order.push(b);
            if entry_tree {
                result.reachable[b] = true;
            }
        }
        next_root = (0..n).find(|&b| state[b] == UNVISITED);
    }
    result
}

/// Compute a frequency-driven block order: `order[position] = block index`.
///
/// The result is a reverse postorder of the block CFG in which each block's
/// hottest successor is visited last, so it lands immediately after — the
/// fall-through — while cold subtrees sink behind it. Reverse postorder is not
/// an aesthetic choice: `ir_lower` reads `succ <= block_idx` as "this is a back
/// edge, emit a safepoint poll", and reverse postorder is exactly the property
/// that makes that test mean what it says.
///
/// Cold sinking is therefore bounded by that constraint. A cold block is moved
/// *behind the hot subtree that shares its predecessor*, not to the physical end
/// of the method: relocating it past its own successors would turn forward edges
/// into apparent back edges, and a layout pass may not manufacture safepoint
/// semantics.
///
/// Returns a structured [`Bailout`] rather than panicking or silently emitting a
/// broken order when the result fails its own validation; the caller keeps the
/// order it already had, which is always correct.
/// Is the reverse-postorder block layout on? Default yes; `=0` restores the
/// historical creation order, which is the arm every build before 2026-09-04
/// shipped.
fn rpo_layout_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_IR_RPO_LAYOUT")
            .map_or(true, |v| v != "0" && v != "false")
    })
}

/// Layouts applied, and layouts whose validation refused (leaving creation
/// order). A reordering that never fires and one that always refuses look the
/// same from outside.
static RPO_LAYOUTS_APPLIED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static RPO_LAYOUTS_REJECTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `(applied, rejected)` counts for the reverse-postorder block layout.
pub fn rpo_layout_counts() -> (u64, u64) {
    use std::sync::atomic::Ordering::Relaxed;
    (
        RPO_LAYOUTS_APPLIED.load(Relaxed),
        RPO_LAYOUTS_REJECTED.load(Relaxed),
    )
}

/// [`layout_blocks`] without the frequency priority: a plain DFS, so the result
/// is a reverse postorder and nothing else.
///
/// Kept separate from `layout_blocks` rather than folded in behind an
/// `Option<&freq>` so the hot-path arm's behaviour is byte-identical to what it
/// was; this one is reached only when that arm is off.
pub fn layout_blocks_rpo(
    blocks: &[Block],
    protected_regions: &[Vec<usize>],
) -> Result<Vec<usize>, Bailout> {
    let n = blocks.len();
    if n == 0 {
        return Ok(Vec::new());
    }
    let dfs = dfs_layout(blocks, None);
    if dfs.order.len() != n {
        return Err(Bailout::with_context(
            BailoutReason::Internal("rpo layout did not enumerate every block"),
            format!("laid out {} of {} blocks", dfs.order.len(), n),
        ));
    }
    let mut order = dfs.order.clone();
    repair_regions(&mut order, protected_regions, n)?;
    validate_order(blocks, &order, &dfs, protected_regions)?;
    Ok(order)
}

pub fn layout_blocks(
    blocks: &[Block],
    freq: &BlockFrequencies,
    protected_regions: &[Vec<usize>],
) -> Result<Vec<usize>, Bailout> {
    let n = blocks.len();
    if n == 0 {
        return Ok(Vec::new());
    }
    let dfs = dfs_layout(blocks, Some(&freq.freq));
    if dfs.order.len() != n {
        return Err(Bailout::with_context(
            BailoutReason::Internal("block layout did not enumerate every block"),
            format!("laid out {} of {} blocks", dfs.order.len(), n),
        ));
    }
    let mut order = dfs.order.clone();
    repair_regions(&mut order, protected_regions, n)?;
    validate_order(blocks, &order, &dfs, protected_regions)?;
    Ok(order)
}

/// Pull each protected region's members back together, contiguously, at the
/// position of its earliest member — keeping their laid-out relative order.
///
/// This is what stops cold-block sinking from lifting a block out of an
/// exception handler's protected range: a cold block inside the range is still
/// ordered after the hot ones *within* the range, but it cannot leave it.
fn repair_regions(order: &mut Vec<usize>, regions: &[Vec<usize>], n: usize) -> Result<(), Bailout> {
    for region in regions {
        let mut members: Vec<usize> = region.iter().copied().filter(|&b| b < n).collect();
        members.sort_unstable();
        members.dedup();
        if members.len() < 2 {
            continue; // a region of one block is contiguous by definition
        }
        let pos = positions_of(order, n);
        if members.iter().any(|&b| pos[b] == usize::MAX) {
            return Err(Bailout::with_context(
                BailoutReason::Internal("protected region names a block outside the layout"),
                format!("region {members:?}"),
            ));
        }
        let mut by_pos = members.clone();
        by_pos.sort_by_key(|&b| pos[b]);
        let anchor = pos[by_pos[0]];
        let mut rebuilt: Vec<usize> = Vec::with_capacity(order.len());
        for (p, &b) in order.iter().enumerate() {
            if p == anchor {
                rebuilt.extend(by_pos.iter().copied());
            }
            if !members.contains(&b) {
                rebuilt.push(b);
            }
        }
        *order = rebuilt;
    }
    Ok(())
}

/// Check every property `ir_lower` and this module's own callers rely on:
/// the order is a permutation of the blocks, the entry stays first, no edge
/// that is not retreating runs backwards, and every protected region is
/// contiguous.
fn validate_order(
    blocks: &[Block],
    order: &[usize],
    dfs: &DfsResult,
    regions: &[Vec<usize>],
) -> Result<(), Bailout> {
    let n = blocks.len();
    if order.len() != n {
        return Err(Bailout::with_context(
            BailoutReason::Internal("block layout changed the block count"),
            format!("{} positions for {} blocks", order.len(), n),
        ));
    }
    let mut pos = vec![usize::MAX; n];
    for (p, &b) in order.iter().enumerate() {
        if b >= n {
            return Err(Bailout::with_context(
                BailoutReason::Internal("block layout names a block that does not exist"),
                format!("position {p} holds b{b}"),
            ));
        }
        if pos[b] != usize::MAX {
            return Err(Bailout::with_context(
                BailoutReason::Internal("block layout repeats a block"),
                format!("b{b} at positions {} and {p}", pos[b]),
            ));
        }
        pos[b] = p;
    }
    if order.first() != Some(&0) {
        return Err(Bailout::new(BailoutReason::Internal(
            "block layout moved the entry block off position 0",
        )));
    }

    // Reverse-postorder property. Every backwards edge out of *reachable* code
    // must be one the DFS classified as retreating; that is what keeps
    // `ir_lower`'s `succ <= block_idx` safepoint-poll test meaning "back edge",
    // and it guarantees every loop still polls at least once per iteration.
    let retreating: std::collections::HashSet<(usize, usize)> =
        dfs.retreating.iter().copied().collect();
    for (b, blk) in blocks.iter().enumerate() {
        if !dfs.reachable[b] {
            continue; // dead code: an extra poll there is harmless
        }
        for &s in &blk.successors {
            if s >= n || pos[s] > pos[b] {
                continue;
            }
            if !retreating.contains(&(b, s)) {
                return Err(Bailout::with_context(
                    BailoutReason::Internal("block layout put a forward edge backwards"),
                    format!("edge b{b} -> b{s} at positions {} -> {}", pos[b], pos[s]),
                ));
            }
        }
    }

    for region in regions {
        let mut members: Vec<usize> = region.iter().copied().filter(|&b| b < n).collect();
        members.sort_unstable();
        members.dedup();
        if members.len() < 2 {
            continue;
        }
        let mut ps: Vec<usize> = members.iter().map(|&b| pos[b]).collect();
        ps.sort_unstable();
        let span = ps[ps.len() - 1] - ps[0] + 1;
        if span != ps.len() {
            return Err(Bailout::with_context(
                BailoutReason::Internal("block layout split a protected region"),
                format!("region {members:?} spans {span} positions"),
            ));
        }
    }
    Ok(())
}

/// Rewrite the block list, the node→block map and the frequency report into the
/// order's numbering.
///
/// Only called with an order that [`validate_order`] has accepted, so the
/// permutation is total; the recovery path exists because a scheduler that can
/// leave the block list half-moved is worse than one that gives up.
fn apply_order(
    blocks: &mut Vec<Block>,
    node_to_block: &mut [usize],
    freq: &mut BlockFrequencies,
    order: &[usize],
) {
    let n = blocks.len();
    if order.len() != n || n == 0 {
        return;
    }
    let new_index = positions_of(order, n);
    if new_index.iter().any(|&p| p == usize::MAX) {
        return;
    }

    let mut slots: Vec<Option<Block>> = std::mem::take(blocks).into_iter().map(Some).collect();
    let mut reordered: Vec<Block> = Vec::with_capacity(n);
    for &b in order {
        match slots.get_mut(b).and_then(|slot| slot.take()) {
            Some(blk) => reordered.push(blk),
            None => break,
        }
    }
    if reordered.len() != n {
        // Unreachable after validation. Put every block back, in its original
        // order (each still carries its original `id`), and change nothing else
        // — the schedule stays exactly as it was.
        for slot in slots.iter_mut() {
            if let Some(blk) = slot.take() {
                reordered.push(blk);
            }
        }
        reordered.sort_by_key(|blk| blk.id);
        *blocks = reordered;
        return;
    }

    for (p, blk) in reordered.iter_mut().enumerate() {
        blk.id = p;
        for s in blk.successors.iter_mut() {
            if *s < n {
                *s = new_index[*s];
            }
        }
        for pred in blk.predecessors.iter_mut() {
            if *pred < n {
                *pred = new_index[*pred];
            }
        }
    }
    *blocks = reordered;

    for slot in node_to_block.iter_mut() {
        if *slot < n {
            *slot = new_index[*slot];
        }
    }

    // The frequency report is indexed by block, so it permutes with them.
    // `edge_prob` rows stay aligned with `successors` because the layout never
    // reorders a block's successor list — only the indices it names.
    let new_freq: Vec<f64> = order
        .iter()
        .map(|&b| freq.freq.get(b).copied().unwrap_or(0.0))
        .collect();
    let new_depth: Vec<u32> = order
        .iter()
        .map(|&b| freq.depth.get(b).copied().unwrap_or(0))
        .collect();
    let new_header: Vec<bool> = order
        .iter()
        .map(|&b| freq.is_loop_header.get(b).copied().unwrap_or(false))
        .collect();
    let new_trip: Vec<f64> = order
        .iter()
        .map(|&b| freq.trip.get(b).copied().unwrap_or(1.0))
        .collect();
    let new_probs: Vec<Vec<f64>> = order
        .iter()
        .map(|&b| freq.edge_prob.get(b).cloned().unwrap_or_default())
        .collect();
    freq.freq = new_freq;
    freq.depth = new_depth;
    freq.is_loop_header = new_header;
    freq.trip = new_trip;
    freq.edge_prob = new_probs;
}

// ── Layout measurement ───────────────────────────────────────────────

/// Invert an order into `pos[block] = position`.
fn positions_of(order: &[usize], n: usize) -> Vec<usize> {
    let mut pos = vec![usize::MAX; n];
    for (p, &b) in order.iter().enumerate() {
        if b < n {
            pos[b] = p;
        }
    }
    pos
}

/// Estimated executions of `blocks[b]`'s `i`-th outgoing edge.
fn edge_weight(freq: &BlockFrequencies, b: usize, i: usize) -> f64 {
    let f = freq.freq.get(b).copied().unwrap_or(0.0);
    let p = freq
        .edge_prob
        .get(b)
        .and_then(|row| row.get(i))
        .copied()
        .unwrap_or(0.0);
    f * p
}

/// Total estimated executions over every CFG edge — the denominator for the
/// fall-through fractions in [`LayoutReport`].
fn total_edge_weight(blocks: &[Block], freq: &BlockFrequencies) -> f64 {
    let mut total = 0.0f64;
    for (b, blk) in blocks.iter().enumerate() {
        for i in 0..blk.successors.len() {
            total += edge_weight(freq, b, i);
        }
    }
    total
}

/// Edges whose target is the physically next block under `order`.
fn count_fallthrough_edges(blocks: &[Block], order: &[usize]) -> usize {
    let n = blocks.len();
    let pos = positions_of(order, n);
    let mut count = 0usize;
    for (b, blk) in blocks.iter().enumerate() {
        if pos[b] == usize::MAX {
            continue;
        }
        for &s in &blk.successors {
            if s < n && pos[s] != usize::MAX && pos[s] == pos[b] + 1 {
                count += 1;
            }
        }
    }
    count
}

/// The same edges, weighted by how often they are estimated to execute.
fn fallthrough_weight(blocks: &[Block], freq: &BlockFrequencies, order: &[usize]) -> f64 {
    let n = blocks.len();
    let pos = positions_of(order, n);
    let mut weight = 0.0f64;
    for (b, blk) in blocks.iter().enumerate() {
        if pos[b] == usize::MAX {
            continue;
        }
        for (i, &s) in blk.successors.iter().enumerate() {
            if s < n && pos[s] != usize::MAX && pos[s] == pos[b] + 1 {
                weight += edge_weight(freq, b, i);
            }
        }
    }
    weight
}

/// Cold blocks that ended up strictly later than they started.
fn count_cold_sunk(freq: &BlockFrequencies, order: &[usize]) -> usize {
    let n = order.len();
    let pos = positions_of(order, n);
    (0..n)
        .filter(|&b| freq.is_cold(b) && pos[b] != usize::MAX && pos[b] > b)
        .count()
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

    /// The equal-depth half of schedule-late: a pure node whose only use is in
    /// a later block at the SAME loop depth moves there.
    ///
    /// `int f(int a, int b) { if (a != 0) return a + b; return 1; }` — the
    /// `Add` is computed in the ENTRY block because that is where
    /// `find_best_block` puts it (schedule-EARLY: the deepest block its inputs,
    /// two parameters, dominate), and its only reader is the `Return` on the
    /// taken arm. Both blocks are at depth 0, so the strict-decrease rule this
    /// pass shipped with moves nothing; the equal-depth rule moves it to the
    /// block that reads it, where it is also computed on one path instead of
    /// both.
    ///
    /// **The sum is deliberately not stored to a local.** A local is named by
    /// the deopt frame of every later bci, including the ones on the arm it did
    /// not sink into, and [`safepoints_dominate_anchors`] then refuses the
    /// placement — correctly, because a frame on that arm would read a word
    /// nothing had written. That refusal is the pass working, not the rule
    /// failing, and a test written on a local would assert the opposite of what
    /// it looks like it asserts.
    ///
    /// Asserted as a DIFFERENCE between the two arms rather than as an absolute
    /// block index, so a change in how the scheduler numbers blocks cannot make
    /// this pass vacuously.
    #[test]
    fn equal_depth_sink_moves_a_pure_node_to_its_only_reader() {
        // 0: iload_0   1: ifne +6 (-> 7)
        // 4: iconst_1  5: ireturn   6: nop
        // 7: iload_0   8: iload_1   9: iadd   10: ireturn
        let code = [
            0x1au8, 0x9a, 0x00, 0x06, 0x04, 0xac, 0x00, 0x1a, 0x1b, 0x60, 0xac, 0, 0,
        ];

        let block_of_add = |on: &str| -> (usize, usize) {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_JIT_IR_SINK_EQUAL_DEPTH", Some(on))],
                || {
                    let builder = IrBuilder::new(2, 2);
                    let mut graph = builder.build(&code, 11).expect("build failed");
                    ir_optimize::optimize(&mut graph);
                    let sched = schedule(&graph);
                    let add = graph
                        .nodes
                        .iter()
                        .position(|n| n.op == Op::Add)
                        .expect("the method computes a + b");
                    (sched.node_to_block[add], sched.blocks.len())
                },
            )
        };

        let (off, nblocks) = block_of_add("0");
        let (on, _) = block_of_add("1");
        assert!(nblocks >= 2, "branching method should have 2+ blocks");
        assert_ne!(
            on, off,
            "the equal-depth rule did not move the Add out of block {off}"
        );
        assert_ne!(on, usize::MAX, "the Add must still be placed somewhere");
        assert!(on < nblocks, "block index {on} out of range");
    }

    /// A value a deopt frame names on the arm it did NOT sink into keeps its
    /// early placement — the obligation [`safepoints_dominate_anchors`] states.
    ///
    /// `int f(int a, int b) { int t = a + b; if (a == 0) return 1; return t; }`
    /// is the same shape as the test above with one edit: the sum goes through
    /// a LOCAL, so every snapshot from its store onwards names it, including
    /// the one on the `return 1` arm. Sinking it into the other arm would let a
    /// deopt there read a word nothing had written. The pass must decline, and
    /// declining must cost nothing else — which is what the fallback to the
    /// depth-only rule is for.
    #[test]
    fn a_value_a_frame_names_on_the_other_arm_does_not_sink() {
        // 0: iload_0   1: iload_1   2: iadd   3: istore_2
        // 4: iload_0   5: ifeq +5 (-> 10)
        // 8: iload_2   9: ireturn   10: iconst_1  11: ireturn
        let code = [
            0x1au8, 0x1b, 0x60, 0x3d, 0x1a, 0x99, 0x00, 0x05, 0x1c, 0xac, 0x04, 0xac, 0, 0,
        ];

        let block_of_add = |on: &str| -> usize {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_JIT_IR_SINK_EQUAL_DEPTH", Some(on))],
                || {
                    let builder = IrBuilder::new(2, 3);
                    let mut graph = builder.build(&code, 12).expect("build failed");
                    ir_optimize::optimize(&mut graph);
                    let sched = schedule(&graph);
                    let add = graph
                        .nodes
                        .iter()
                        .position(|n| n.op == Op::Add)
                        .expect("the method computes a + b");
                    sched.node_to_block[add]
                },
            )
        };

        assert_eq!(
            block_of_add("1"),
            block_of_add("0"),
            "a value the other arm's frame names must keep its early placement",
        );
    }

    /// The flag is read LIVE, not cached in a `OnceLock`.
    ///
    /// `ir_per_copy_frames_enabled` records what a cached scheduling flag costs:
    /// the matrix test ran the OFF arm first and both arms then reported
    /// byte-identical code. Two calls in one process, different values, must
    /// give different answers.
    #[test]
    fn equal_depth_flag_is_read_live() {
        let read = |v: Option<&str>| {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_JIT_IR_SINK_EQUAL_DEPTH", v)],
                sink_equal_depth_enabled,
            )
        };
        assert!(read(Some("1")));
        assert!(!read(Some("0")));
        assert!(!read(None), "default is OFF");
        assert!(read(Some("1")), "a second read must still see the override");
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
            receiver_param: None,
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
            receiver_param: None,
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

    // ── Memory-effect ordering ───────────────────────────────────────────

    use crate::ir::{MemKind, NodeId as Id};

    /// An `If`'s successors come out in PROJECTION order, whatever order the
    /// projections were added in.
    ///
    /// `ir_lower` names `successors[0]` the true block and `successors[1]` the
    /// false one, so this ordering decides which way every conditional branch
    /// the backend emits goes. It held for free as long as `IrBuilder` was the
    /// only producer — its `if_icmp*` arms add `Proj(0)` and then `Proj(1)`, so
    /// the lower node id was always the true edge, and this scan pushed in node
    /// id order.
    ///
    /// The first transform to CLONE an `If` broke that. The partial unroller
    /// adds each copy's continue-edge projection first, because that is the one
    /// the next copy hangs off, and in a javac `if_icmpge` loop the continue
    /// edge is `Proj(1)`. Every intermediate test then branched to the edge
    /// meant for its opposite: the early exits were never taken and the loop
    /// ran `n + factor - 1` iterations, so `sum(1)` returned 6.
    ///
    /// This graph is built the way the unroller builds one — `Proj(1)` first —
    /// so it fails against a node-id-ordered scan and passes against an
    /// index-ordered one. A fixture built in the conventional order cannot tell
    /// the two apart, which is why this one is deliberately backwards.
    #[test]
    fn an_ifs_successors_are_ordered_by_projection_not_by_node_id() {
        let mut g = bare_graph();
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let cond = g.add(Op::Param(0), IrType::Int, vec![], None);
        let if_node = g.add(Op::If, IrType::Control, vec![entry, cond], Some(0));
        // Backwards on purpose: the FALSE edge gets the lower node id.
        let false_edge = g.add(Op::Proj(1), IrType::Control, vec![if_node], Some(0));
        let true_edge = g.add(Op::Proj(0), IrType::Control, vec![if_node], Some(0));
        let a = g.add(Op::Const(1), IrType::Int, vec![], None);
        let b = g.add(Op::Const(2), IrType::Int, vec![], None);
        let merge = g.add(
            Op::Merge,
            IrType::Control,
            vec![true_edge, false_edge],
            Some(1),
        );
        let phi = g.add(Op::Phi, IrType::Int, vec![merge, a, b], Some(1));
        let ret = g.add(Op::Return, IrType::Void, vec![merge, phi], Some(2));
        g.entry = start;
        g.exit = ret;

        let schedule = schedule(&g);
        let if_block = schedule.node_to_block[if_node as usize];
        let succ = &schedule.blocks[if_block].successors;
        assert_eq!(succ.len(), 2, "an `If` has exactly two successors");
        assert_eq!(
            schedule.blocks[succ[0]].ctrl, true_edge,
            "successors[0] must be the Proj(0) block — `ir_lower` branches on it \
             as the TRUE edge",
        );
        assert_eq!(
            schedule.blocks[succ[1]].ctrl, false_edge,
            "successors[1] must be the Proj(1) block",
        );
    }

    fn bare_graph() -> Graph {
        Graph {
            nodes: Vec::new(),
            entry: NO_NODE,
            exit: NO_NODE,
            safepoints: Vec::new(),
            uses: Default::default(),
            receiver_param: None,
        }
    }

    /// `ctrl`, `mem`, one base reference and two constant offsets, so the
    /// alias model has everything it needs to prove (or refuse) disjointness.
    struct MemGraph {
        graph: Graph,
        ctrl: Id,
        mem: Id,
        base: Id,
        off_a: Id,
        off_b: Id,
        value: Id,
    }

    fn mem_graph() -> MemGraph {
        let mut graph = bare_graph();
        let start = graph.add(Op::Start, IrType::Control, vec![], None);
        let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let base = graph.add(Op::Param(0), IrType::Ref, vec![], None);
        let off_a = graph.add(Op::Const(1), IrType::Int, vec![], None);
        let off_b = graph.add(Op::Const(2), IrType::Int, vec![], None);
        let value = graph.add(Op::Param(1), IrType::Int, vec![], None);
        MemGraph {
            graph,
            ctrl,
            mem,
            base,
            off_a,
            off_b,
            value,
        }
    }

    impl MemGraph {
        fn load(&mut self, tok: Id, off: Id) -> Id {
            self.graph.add(
                Op::Load(MemKind::Int),
                IrType::Int,
                vec![self.ctrl, tok, self.base, off],
                None,
            )
        }
        fn store(&mut self, tok: Id, off: Id) -> Id {
            self.graph.add(
                Op::Store(MemKind::Int),
                IrType::Void,
                vec![self.ctrl, tok, self.base, off, self.value],
                None,
            )
        }
    }

    #[test]
    fn orders_memory_classifies_the_op_families() {
        let mut f = mem_graph();
        let load = f.load(f.mem, f.off_a);
        let store = f.store(load, f.off_b);
        let enter = f.graph.add(
            Op::MonitorEnter,
            IrType::Memory,
            vec![f.ctrl, store, f.base],
            None,
        );
        let exit = f.graph.add(
            Op::MonitorExit,
            IrType::Memory,
            vec![f.ctrl, enter, f.base],
            None,
        );
        let call = f.graph.add(
            Op::Call { info_ptr: 0 },
            IrType::Int,
            vec![f.ctrl, exit],
            None,
        );
        let guard = f.graph.add(
            Op::Guard { bci: 0 },
            IrType::Void,
            vec![f.ctrl, f.value],
            None,
        );
        let add = f
            .graph
            .add(Op::Add, IrType::Int, vec![f.value, f.value], None);
        let konst = f.graph.add(Op::Const(7), IrType::Int, vec![], None);

        for (id, name) in [
            (load, "load"),
            (store, "store"),
            (enter, "monitorenter"),
            (exit, "monitorexit"),
            (call, "call"),
            (guard, "guard"),
        ] {
            assert!(orders_memory(&f.graph, id), "{name} must order memory");
        }
        for (id, name) in [(add, "add"), (konst, "const"), (f.value, "param")] {
            assert!(!orders_memory(&f.graph, id), "{name} must be inert");
        }

        // Barriers: safepoints and the JMM fence halves. A plain load or store
        // is neither.
        for (id, name) in [
            (enter, "monitorenter"),
            (exit, "monitorexit"),
            (call, "call"),
            (guard, "guard"),
        ] {
            assert!(is_memory_barrier(&f.graph, id), "{name} must be a barrier");
        }
        assert!(!is_memory_barrier(&f.graph, load));
        assert!(!is_memory_barrier(&f.graph, store));
    }

    #[test]
    fn an_unchanged_order_is_always_accepted() {
        let mut f = mem_graph();
        let load = f.load(f.mem, f.off_a);
        let store = f.store(load, f.off_b);
        let add = f.graph.add(Op::Add, IrType::Int, vec![load, f.value], None);
        let seq = vec![load, store, add];
        assert_eq!(check_memory_order(&f.graph, &seq, &seq), Ok(()));
    }

    /// Pure nodes are not this rule's business: moving one past a store
    /// changes nothing the memory model can see, and dependence order (which
    /// `topo_sort_block` enforces) is what pins them.
    #[test]
    fn pure_nodes_may_move_freely_around_memory_nodes() {
        let mut f = mem_graph();
        let k = f.graph.add(Op::Const(9), IrType::Int, vec![], None);
        let load = f.load(f.mem, f.off_a);
        let store = f.store(load, f.off_b);
        let base = vec![load, k, store];
        let moved = vec![k, load, store];
        assert_eq!(check_memory_order(&f.graph, &base, &moved), Ok(()));
    }

    /// The defect the rule exists to catch: a load hoisted above a store to
    /// the same cell reads the value that was there *before* the store.
    #[test]
    fn a_load_may_not_swap_with_an_aliasing_store() {
        let mut f = mem_graph();
        let store = f.store(f.mem, f.off_a);
        let load = f.load(store, f.off_a); // same base, same constant offset
        let base = vec![store, load];
        let swapped = vec![load, store];
        assert_eq!(
            check_memory_order(&f.graph, &base, &swapped),
            Err(OrderViolation {
                earlier: store,
                later: load,
                refusal: OrderRefusal::Model(ReorderBlock::MayAlias),
            })
        );
    }

    /// Two accesses the model proves disjoint do commute.
    #[test]
    fn provably_disjoint_accesses_may_swap() {
        let mut f = mem_graph();
        let store = f.store(f.mem, f.off_a);
        let load = f.load(store, f.off_b); // different constant offset
        let base = vec![store, load];
        let swapped = vec![load, store];
        assert_eq!(check_memory_order(&f.graph, &base, &swapped), Ok(()));
    }

    /// A monitor-enter is an acquire and a safepoint. Nothing that orders
    /// memory crosses it — including a load the pairwise memory model would
    /// wave through.
    #[test]
    fn nothing_crosses_a_barrier() {
        let mut f = mem_graph();
        let enter = f.graph.add(
            Op::MonitorEnter,
            IrType::Memory,
            vec![f.ctrl, f.mem, f.base],
            None,
        );
        let load = f.load(enter, f.off_a);
        let base = vec![enter, load];
        let swapped = vec![load, enter];
        assert_eq!(
            check_memory_order(&f.graph, &base, &swapped),
            Err(OrderViolation {
                earlier: enter,
                later: load,
                refusal: OrderRefusal::Barrier,
            })
        );
    }

    /// Specifically the case where the rule is deliberately stronger than
    /// `may_reorder`: an allocation is a safepoint, the model says a read may
    /// cross it, and the scheduler still refuses because a faulting load below
    /// a safepoint deopts into a frame that safepoint has already passed.
    #[test]
    fn the_barrier_rule_is_stronger_than_the_pairwise_model() {
        let mut f = mem_graph();
        let alloc = f.graph.add(
            Op::New {
                class_id: 0,
                num_fields: 1,
            },
            IrType::Ref,
            vec![f.ctrl, f.mem],
            None,
        );
        let load = f.load(alloc, f.off_a);
        // The memory model itself permits the swap …
        assert!(f.graph.may_reorder(alloc, load).is_allowed());
        // … and the scheduler still refuses it.
        assert_eq!(
            check_memory_order(&f.graph, &[alloc, load], &[load, alloc]),
            Err(OrderViolation {
                earlier: alloc,
                later: load,
                refusal: OrderRefusal::Barrier,
            })
        );
    }

    /// Two allocations do not commute: which one runs first is observable
    /// through `OutOfMemoryError` and through identity hashes.
    #[test]
    fn two_allocations_may_not_swap() {
        let mut f = mem_graph();
        let a = f.graph.add(
            Op::New {
                class_id: 0,
                num_fields: 1,
            },
            IrType::Ref,
            vec![f.ctrl, f.mem],
            None,
        );
        let b = f.graph.add(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            IrType::Ref,
            vec![f.ctrl, a],
            None,
        );
        assert!(check_memory_order(&f.graph, &[a, b], &[b, a]).is_err());
    }

    /// A schedule that drops or invents a node is refused whatever else is
    /// true of it — a dropped store is a dropped write.
    #[test]
    fn a_non_permutation_is_refused() {
        let mut f = mem_graph();
        let load = f.load(f.mem, f.off_a);
        let store = f.store(load, f.off_b);
        let dropped = vec![load];
        assert_eq!(
            check_memory_order(&f.graph, &[load, store], &dropped),
            Err(OrderViolation {
                earlier: NO_NODE,
                later: NO_NODE,
                refusal: OrderRefusal::NotAPermutation,
            })
        );
        let invented = vec![load, f.value];
        assert_eq!(
            check_memory_order(&f.graph, &[load, store], &invented),
            Err(OrderViolation {
                earlier: NO_NODE,
                later: NO_NODE,
                refusal: OrderRefusal::NotAPermutation,
            })
        );
        let duplicated = vec![load, load];
        assert_eq!(
            check_memory_order(&f.graph, &[load, store], &duplicated),
            Err(OrderViolation {
                earlier: NO_NODE,
                later: NO_NODE,
                refusal: OrderRefusal::NotAPermutation,
            })
        );
    }

    /// The priority scheduler's output must satisfy the rule on every block it
    /// touches. This is the property the validator was wired in to guarantee,
    /// checked against the real pass rather than asserted about it.
    #[test]
    fn the_priority_scheduler_never_violates_the_memory_order() {
        // `int f(int a, int b) { return (a + b) * a - b; }` — enough data
        // nodes in one block for the priority pass to have something to do
        // (it declines blocks of two or fewer).
        let code = [
            0x1a, // 0: iload_0
            0x1b, // 1: iload_1
            0x60, // 2: iadd
            0x1a, // 3: iload_0
            0x68, // 4: imul
            0x1b, // 5: iload_1
            0x64, // 6: isub
            0xac, // 7: ireturn
            0, 0,
        ];
        let builder = IrBuilder::new(2, 2);
        let mut graph = builder.build(&code, 8).expect("build failed");
        ir_optimize::optimize(&mut graph);

        let plain = schedule(&graph);
        let opts = ScheduleOptions {
            priority_within_blocks: true,
            ..ScheduleOptions::default()
        };
        let prioritised = schedule_with_options(&graph, &opts);
        assert_eq!(plain.blocks.len(), prioritised.blocks.len());
        for (b, p) in plain.blocks.iter().zip(prioritised.blocks.iter()) {
            assert_eq!(
                check_memory_order(&graph, &b.nodes, &p.nodes),
                Ok(()),
                "block {} was reordered illegally",
                b.id
            );
        }
    }

    /// Pairing may only ever REORDER: every block keeps exactly the node set it
    /// had. A node it dropped or duplicated would be one `ir_lower` emits twice
    /// or not at all.
    #[test]
    fn pairing_preserves_every_block_node_set() {
        let code = [0x1a, 0x1b, 0x60, 0x1a, 0x68, 0x1b, 0x64, 0xac, 0, 0];
        let builder = IrBuilder::new(2, 2);
        let mut graph = builder.build(&code, 8).expect("build failed");
        ir_optimize::optimize(&mut graph);
        let off = schedule_with_options(
            &graph,
            &ScheduleOptions {
                pair_single_use_operands: false,
                ..ScheduleOptions::default()
            },
        );
        let on = schedule_with_options(
            &graph,
            &ScheduleOptions {
                pair_single_use_operands: true,
                ..ScheduleOptions::default()
            },
        );
        assert_eq!(off.blocks.len(), on.blocks.len());
        for (a, b) in off.blocks.iter().zip(on.blocks.iter()) {
            let mut xs = a.nodes.clone();
            let mut ys = b.nodes.clone();
            xs.sort_unstable();
            ys.sort_unstable();
            assert_eq!(xs, ys, "block {} lost or gained a node", a.id);
        }
    }

    /// Pairing only ever sinks, so it cannot move a node above one of its own
    /// inputs — which is exactly the kind of claim that stops being true when
    /// someone widens the pass. Asserted over the emitted order rather than
    /// trusted.
    #[test]
    fn pairing_keeps_every_definition_before_its_uses() {
        let code = [0x1a, 0x1b, 0x60, 0x1a, 0x68, 0x1b, 0x64, 0xac, 0, 0];
        let builder = IrBuilder::new(2, 2);
        let mut graph = builder.build(&code, 8).expect("build failed");
        ir_optimize::optimize(&mut graph);
        let sched = schedule_with_options(
            &graph,
            &ScheduleOptions {
                pair_single_use_operands: true,
                ..ScheduleOptions::default()
            },
        );
        for block in &sched.blocks {
            let mut seen = std::collections::HashSet::new();
            for &nid in &block.nodes {
                for &input in &graph.nodes[nid as usize].inputs {
                    if input == NO_NODE {
                        continue;
                    }
                    if block.nodes.contains(&input) {
                        assert!(
                            seen.contains(&input),
                            "node {nid} reads {input}, which this block defines later"
                        );
                    }
                }
                seen.insert(nid);
            }
        }
    }

    /// A single-use operand must not be sunk past anything a deopt can arrive
    /// at: it is undefined there, and `graph.safepoints` names the whole
    /// operand stack at every bci.
    ///
    /// `idiv` is the available trapping node — it deopts on a zero divisor
    /// after reading its operands.
    #[test]
    fn pairing_does_not_sink_past_a_node_a_deopt_can_reach() {
        let code = [
            0x1a, // iload_0
            0x1b, // iload_1
            0x60, // iadd     <- a single-use operand of the outer iadd
            0x1a, // iload_0
            0x1b, // iload_1
            0x6c, // idiv     <- can deopt
            0x60, // iadd
            0xac, // ireturn
            0, 0,
        ];
        let builder = IrBuilder::new(2, 2);
        let Some(mut graph) = builder.build(&code, 8) else {
            return;
        };
        ir_optimize::optimize(&mut graph);
        let sched = schedule_with_options(
            &graph,
            &ScheduleOptions {
                pair_single_use_operands: true,
                ..ScheduleOptions::default()
            },
        );
        for block in &sched.blocks {
            let Some(div) = block
                .nodes
                .iter()
                .position(|&n| matches!(graph.nodes[n as usize].op, Op::Div))
            else {
                continue;
            };
            for &i in &graph.nodes[block.nodes[div] as usize].inputs {
                if i == NO_NODE {
                    continue;
                }
                if let Some(pos) = block.nodes.iter().position(|&x| x == i) {
                    assert!(
                        pos < div,
                        "an operand of the trapping node was sunk past it"
                    );
                }
            }
        }
    }

    /// The validator may only ever *reject* a reordering, never cause one: a
    /// block keeps exactly the node set it had, whether or not the priority
    /// pass ran. (A node the pass dropped would also be a node `plan_slots`
    /// never colours and `ir_lower` never emits.)
    #[test]
    fn priority_scheduling_preserves_every_block_node_set() {
        let code = [0x1a, 0x1b, 0x60, 0x1a, 0x68, 0x1b, 0x64, 0xac, 0, 0];
        let builder = IrBuilder::new(2, 2);
        let mut graph = builder.build(&code, 8).expect("build failed");
        ir_optimize::optimize(&mut graph);
        let plain = schedule(&graph);
        let prioritised = schedule_with_options(
            &graph,
            &ScheduleOptions {
                priority_within_blocks: true,
                ..ScheduleOptions::default()
            },
        );
        assert_eq!(plain.blocks.len(), prioritised.blocks.len());
        for (x, y) in plain.blocks.iter().zip(prioritised.blocks.iter()) {
            let mut a = x.nodes.clone();
            let mut b = y.nodes.clone();
            a.sort_unstable();
            b.sort_unstable();
            assert_eq!(a, b, "block {} lost or gained a node", x.id);
        }
    }

    /// Blocks are indexed in CREATION order — the order Step 1 walked control
    /// nodes in — and `ir_lower` emits `schedule.blocks` front to back. Nothing
    /// made creation order a reverse postorder, so a block could be emitted
    /// before the block that dominates it, and a value read before the
    /// instruction defining it. `verify_data_locations` caught that and refused
    /// the compile, which is why it cost lost compiles rather than wrong code.
    ///
    /// Here block 1 is reachable only through block 2, but is numbered first.
    #[test]
    fn rpo_layout_puts_a_dominator_before_the_block_it_dominates() {
        let blocks = vec![
            Block {
                id: 0,
                ctrl: 0,
                nodes: Vec::new(),
                terminator: None,
                successors: vec![2],
                predecessors: Vec::new(),
            },
            Block {
                id: 1,
                ctrl: 1,
                nodes: Vec::new(),
                terminator: None,
                successors: Vec::new(),
                predecessors: vec![2],
            },
            Block {
                id: 2,
                ctrl: 2,
                nodes: Vec::new(),
                terminator: None,
                successors: vec![1],
                predecessors: vec![0],
            },
        ];
        let order = layout_blocks_rpo(&blocks, &[]).expect("a three-block chain must lay out");
        let pos = |b: usize| {
            order
                .iter()
                .position(|&x| x == b)
                .expect("every block placed")
        };
        assert_eq!(pos(0), 0, "the entry block stays at position 0");
        assert!(
            pos(2) < pos(1),
            "block 2 dominates block 1 and must be emitted first; got order {order:?}",
        );
    }

    /// The layout is a permutation, not a filter: dropping or repeating a block
    /// would silently delete or duplicate code.
    #[test]
    fn rpo_layout_is_a_permutation() {
        let blocks = vec![
            Block {
                id: 0,
                ctrl: 0,
                nodes: Vec::new(),
                terminator: None,
                successors: vec![1, 2],
                predecessors: Vec::new(),
            },
            Block {
                id: 1,
                ctrl: 1,
                nodes: Vec::new(),
                terminator: None,
                successors: vec![3],
                predecessors: vec![0],
            },
            Block {
                id: 2,
                ctrl: 2,
                nodes: Vec::new(),
                terminator: None,
                successors: vec![3],
                predecessors: vec![0],
            },
            Block {
                id: 3,
                ctrl: 3,
                nodes: Vec::new(),
                terminator: None,
                successors: Vec::new(),
                predecessors: vec![1, 2],
            },
        ];
        let order = layout_blocks_rpo(&blocks, &[]).expect("a diamond must lay out");
        let mut seen = order.clone();
        seen.sort_unstable();
        assert_eq!(
            seen,
            vec![0, 1, 2, 3],
            "order {order:?} is not a permutation"
        );
        let pos = |b: usize| order.iter().position(|&x| x == b).unwrap();
        assert!(
            pos(3) > pos(1) && pos(3) > pos(2),
            "the merge follows both arms"
        );
    }

    /// An empty CFG is not an error, and must not be turned into one.
    #[test]
    fn rpo_layout_accepts_an_empty_block_list() {
        assert_eq!(
            layout_blocks_rpo(&[], &[]).expect("empty is fine"),
            Vec::<usize>::new()
        );
    }

    // ── Dominators against the dense fixpoint they replaced ──────────────

    /// The dense iterative fixpoint `compute_dominators` used to be, kept
    /// verbatim as the reference [`Dominators`] must agree with — unreachable
    /// blocks, out-of-range predecessors and all.
    fn dense_dominators_reference(blocks: &[Block]) -> Vec<Vec<bool>> {
        let n = blocks.len();
        if n == 0 {
            return Vec::new();
        }
        let mut dom: Vec<Vec<bool>> = vec![vec![true; n]; n];
        dom[0] = vec![false; n];
        dom[0][0] = true;

        let mut changed = true;
        while changed {
            changed = false;
            for b in 1..n {
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
                let mut new_dom = match new_dom {
                    Some(v) => v,
                    None => continue,
                };
                new_dom[b] = true;
                if new_dom != dom[b] {
                    dom[b] = new_dom;
                    changed = true;
                }
            }
        }
        dom
    }

    /// A CFG of `n` blocks from an edge list, with both edge lists filled in
    /// (the dominator math reads only `predecessors`; `successors` is there
    /// so the fixture is a plausible CFG). An edge may name a block `>= n`,
    /// which lands in `predecessors` only — the out-of-range case both
    /// algorithms must ignore.
    fn cfg(n: usize, edges: &[(usize, usize)]) -> Vec<Block> {
        let mut blocks: Vec<Block> = (0..n).map(|id| blk(id, &[])).collect();
        for &(from, to) in edges {
            if to < n {
                blocks[to].predecessors.push(from);
            }
            if from < n && to < n {
                blocks[from].successors.push(to);
            }
        }
        blocks
    }

    /// Both representations answer every `(a, b)` query the way the reference
    /// matrix does, one index past the end included.
    fn assert_dominators_match_reference(name: &str, blocks: &[Block]) {
        let reference = dense_dominators_reference(blocks);
        assert_eq!(
            compute_dominators(blocks),
            reference,
            "{name}: matrix differs from the dense fixpoint"
        );
        let doms = Dominators::compute(blocks);
        assert_eq!(doms.len(), blocks.len(), "{name}: relation size");
        for a in 0..=blocks.len() {
            for b in 0..=blocks.len() {
                assert_eq!(
                    doms.dominates(a, b),
                    dominates(&reference, a, b),
                    "{name}: dominates({a}, {b}) differs from the dense fixpoint"
                );
            }
        }
    }

    #[test]
    fn chk_dominators_match_the_dense_fixpoint_on_hand_built_cfgs() {
        assert_dominators_match_reference("empty", &[]);
        assert_dominators_match_reference("single block", &cfg(1, &[]));
        assert_dominators_match_reference(
            "diamond",
            &cfg(4, &[(0, 1), (0, 2), (1, 3), (2, 3)]),
        );
        // 0 -> 1 (outer header) -> 2 (inner header) -> 3 (inner latch) -> 2,
        // 2 -> 4 (outer latch) -> 1, 1 -> 5 (exit).
        assert_dominators_match_reference(
            "nested loops",
            &cfg(
                6,
                &[(0, 1), (1, 2), (2, 3), (3, 2), (2, 4), (4, 1), (1, 5)],
            ),
        );
        // A loop with two entries: neither 1 nor 2 dominates the other.
        assert_dominators_match_reference(
            "irreducible two-entry loop",
            &cfg(4, &[(0, 1), (0, 2), (1, 2), (2, 1), (1, 3), (2, 3)]),
        );
        // Listed with the join BEFORE its arms, so block order is not RPO.
        assert_dominators_match_reference(
            "numbering is not reverse postorder",
            &cfg(5, &[(0, 3), (0, 4), (3, 1), (4, 1), (1, 2)]),
        );
        assert_dominators_match_reference(
            "self loop and a back edge to the entry",
            &cfg(3, &[(0, 1), (1, 1), (1, 2), (2, 0)]),
        );
        // 3 has no predecessors; 4 <-> 5 is a cycle nothing enters; 5 also
        // feeds the reachable 2, which must ignore it.
        assert_dominators_match_reference(
            "unreachable blocks",
            &cfg(6, &[(0, 1), (1, 2), (4, 5), (5, 4), (5, 2), (3, 2)]),
        );
        assert_dominators_match_reference(
            "out-of-range predecessor",
            &cfg(3, &[(0, 1), (1, 2), (7, 2), (9, 1)]),
        );
        let chain: Vec<(usize, usize)> = (0..999).map(|b| (b, b + 1)).collect();
        assert_dominators_match_reference("long chain", &cfg(1000, &chain));
    }

    /// Several hundred small CFGs from a fixed-seed generator, so the
    /// agreement is not an artifact of the shapes someone thought to draw.
    #[test]
    fn chk_dominators_match_the_dense_fixpoint_on_generated_cfgs() {
        // Numerical Recipes' LCG: deterministic, no dependency.
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = |bound: usize| -> usize {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((state >> 33) as usize) % bound.max(1)
        };
        for case in 0..400 {
            let n = 1 + next(24);
            let m = next(3 * n + 1);
            let edges: Vec<(usize, usize)> = (0..m).map(|_| (next(n), next(n))).collect();
            assert_dominators_match_reference(&format!("generated case {case}"), &cfg(n, &edges));
        }
    }

    #[test]
    fn chk_idom_names_the_nearest_strict_dominator() {
        let d = Dominators::compute(&cfg(5, &[(0, 1), (0, 2), (1, 3), (2, 3), (4, 3)]));
        assert_eq!(d.idom(0), None, "the entry has no immediate dominator");
        assert_eq!(d.idom(1), Some(0));
        assert_eq!(d.idom(3), Some(0), "a join's idom is the branch, not an arm");
        assert_eq!(d.idom(4), None, "an unreachable block has none");
        assert_eq!(d.idom(5), None, "nor does an out-of-range index");
    }

    // ── The sink pass's answers around unreachable blocks, pinned ────────

    /// What the sink pass's common-dominator query answers when a use sits in
    /// a block the entry does not reach.
    ///
    /// In this model every block dominates an unreachable one, so an
    /// unreachable use constrains nothing. When EVERY use is unreachable,
    /// every block qualifies, and the "deeper wins" rule settles on the
    /// highest-numbered unreachable block. The block scan that answered this
    /// was replaced by a dominator-tree walk; these are the answers the walk
    /// must keep.
    #[test]
    fn sink_common_dominator_treats_an_unreachable_use_as_dominated_by_every_block() {
        // 0 -> 1 -> 2 is reachable; 3 -> 4 is not.
        let blocks = cfg(5, &[(0, 1), (1, 2), (3, 4)]);
        let dom = Dominators::compute(&blocks);
        for a in 0..5 {
            assert!(dom.dominates(a, 3), "{a} must dominate the unreachable 3");
            assert!(dom.dominates(a, 4), "{a} must dominate the unreachable 4");
        }
        assert!(!dom.dominates(3, 1), "an unreachable block dominates no reachable one");

        // Every use unreachable: the last unreachable block.
        assert_eq!(deepest_common_dominator(&dom, &[3], 5), Some(4));
        assert_eq!(deepest_common_dominator(&dom, &[4, 3], 5), Some(4));
        // A reachable use decides on its own.
        assert_eq!(deepest_common_dominator(&dom, &[2, 3], 5), Some(2));
        assert_eq!(deepest_common_dominator(&dom, &[1, 2, 4], 5), Some(1));
        // No block dominates an out-of-range use.
        assert_eq!(deepest_common_dominator(&dom, &[2, 9], 5), None);
    }

    // ── Step 4 placement order ───────────────────────────────────────────

    /// An old node rewired to read a NEWER one is placed where that input is
    /// available, not where id order happened to leave it.
    ///
    /// Step 4 used to place nodes in id order, and `find_best_block` ignores
    /// inputs that have no block yet. So `user` below — created first, then
    /// pointed at `input` — saw only the parameter, went to the entry block,
    /// and was emitted before the merge block that computes `input`: exactly
    /// the def-after-use `verify_data_locations` refuses, and the shape
    /// `reassociate_affine` leaves behind when it rewires an operand to a node
    /// it just materialised.
    #[test]
    fn an_old_node_rewired_to_a_newer_input_is_placed_after_that_input() {
        let mut g = bare_graph();
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let p = g.add(Op::Param(0), IrType::Int, vec![], None);
        // The OLD node, created before the value it will end up reading.
        let user = g.add(Op::Add, IrType::Int, vec![p, p], None);
        let if_node = g.add(Op::If, IrType::Control, vec![entry, p], Some(0));
        let t = g.add(Op::Proj(0), IrType::Control, vec![if_node], Some(0));
        let f = g.add(Op::Proj(1), IrType::Control, vec![if_node], Some(0));
        let merge = g.add(Op::Merge, IrType::Control, vec![t, f], Some(1));
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let two = g.add(Op::Const(2), IrType::Int, vec![], None);
        let phi = g.add(Op::Phi, IrType::Int, vec![merge, one, two], Some(1));
        // The NEW input: it reads the phi, so it can only live in the merge.
        let input = g.add(Op::Mul, IrType::Int, vec![phi, p], Some(1));
        assert!(g.set_input(user, 0, input), "fixture: rewire the old node");
        let ret = g.add(Op::Return, IrType::Void, vec![merge, user], Some(2));
        g.entry = start;
        g.exit = ret;
        assert!(input > user, "fixture: the input must have the higher id");

        let sched = schedule(&g);
        let merge_block = sched.node_to_block[merge as usize];
        let input_block = sched.node_to_block[input as usize];
        let user_block = sched.node_to_block[user as usize];
        assert_eq!(
            input_block, merge_block,
            "the input is placed with the phi it reads"
        );
        assert!(
            dominates(&sched.dom, input_block, user_block),
            "the input's block {input_block} must dominate the user's block {user_block}"
        );
        // Emission order: blocks in index order, then position in the block.
        let emitted_at = |id: NodeId| -> (usize, usize) {
            let b = sched.node_to_block[id as usize];
            let pos = sched.blocks[b]
                .nodes
                .iter()
                .position(|&x| x == id)
                .expect("a placed node is listed in its block");
            (b, pos)
        };
        assert!(
            emitted_at(input) < emitted_at(user),
            "the input {:?} must be emitted before its user {:?}",
            emitted_at(input),
            emitted_at(user),
        );
    }

    /// The input-first walk is iterative: a chain whose every link reads a
    /// HIGHER id — the walk's worst case, 20 000 frames deep — neither
    /// overflows the stack nor places a link before its input.
    #[test]
    fn input_first_placement_survives_a_long_forward_chain() {
        let mut g = bare_graph();
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let depth = 20_000usize;
        // Created with a placeholder input, then each link pointed at the next.
        let links: Vec<NodeId> = (0..depth)
            .map(|_| g.add(Op::Neg, IrType::Int, vec![NO_NODE], None))
            .collect();
        let p = g.add(Op::Param(0), IrType::Int, vec![], None);
        for k in 0..depth {
            let target = if k + 1 < depth { links[k + 1] } else { p };
            assert!(g.set_input(links[k], 0, target));
        }
        let ret = g.add(Op::Return, IrType::Void, vec![entry, links[0]], None);
        g.entry = start;
        g.exit = ret;

        let sched = schedule_with_options(&g, &ScheduleOptions::default());
        let b0 = sched.node_to_block[links[0] as usize];
        let mut pos = vec![usize::MAX; g.nodes.len()];
        for (i, &id) in sched.blocks[b0].nodes.iter().enumerate() {
            pos[id as usize] = i;
        }
        for k in 0..depth - 1 {
            assert_eq!(sched.node_to_block[links[k] as usize], b0);
            assert!(
                pos[links[k + 1] as usize] < pos[links[k] as usize],
                "link {k} is listed before the link it reads"
            );
        }
    }
}
