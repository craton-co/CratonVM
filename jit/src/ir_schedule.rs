// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Linearize the Sea-of-Nodes IR graph into a sequence of basic blocks.
//!
//! The scheduler places each data node into a basic block such that:
//! 1. The node dominates all its uses.
//! 2. The node is placed in the deepest block all its inputs dominate
//!    (schedule-EARLY, [`find_best_block`]); pure nodes are then moved down to
//!    a shallower loop nesting on their dominator path when their uses allow
//!    it ([`sink_pure_nodes`], default on). There is no general
//!    schedule-late; round 11 corrected this line, which claimed one.
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
//! `ScheduleOptions::default()` turns all three off, but [`schedule`] — the
//! production entry point — uses [`production_schedule_options`], which turns
//! the layout, the priority pass and operand pairing ON (each with its own
//! kill switch). Even with the options off, the block list is still laid out
//! in reverse postorder unless `CRATONVM_JIT_IR_RPO_LAYOUT=0`. (Round 11: this
//! paragraph used to say all three were opt-in and that [`schedule`] used the
//! defaults, which stopped being true on 2026-09-06.)
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
//!    [`Dominators`]' entry assumption and `find_best_block`'s fallback
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
use super::ir::{Graph, IrType, NodeId, Op, Reorder, ReorderBlock, NO_NODE};
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
/// numbers arrive from `profile::MethodProfile::branches` and from
/// hand-written tests), and the counts are two integers.
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
    /// Values a deopt reads although no `graph.safepoints` entry names them:
    /// the field values of a scalar-replaced object
    /// (`ir_lower::scalar_replacement_field_values`), which the lowerer reads
    /// from their homes to materialize the object. After EA kills the store,
    /// such a value often has no user at all, so neither rule that keeps a
    /// frame-named value ahead of a deopt applied to it: list scheduling put
    /// it after the guard, and the lowerer then refused the object
    /// (`r11w6-lower-sr-recipe-values-scheduled-after-the-deopt`). Listed
    /// here, a value is ordered like a frame-named one inside its block and
    /// is never moved to another block by the late sink. Empty by default.
    pub extra_frame_named: Vec<NodeId>,
}

/// Sink pure nodes out of loops they are only used outside of -- **default ON**
/// since 2026-09-05; `CRATONVM_JIT_IR_SINK_LATE=0` is the kill switch.
///
/// This module's own header said (until round 11) a data node is "placed as
/// late as possible (to minimize register pressure)". It is not: [`find_best_block`] picks the
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
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_SINK_LATE")
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

/// `CRATONVM_JIT_IR_SINK_PHI_EDGE=1`: attribute a φ's value read to the
/// predecessor edge it arrives on (see `place_sunk_nodes`) WITHOUT the
/// equal-depth tie rule, so the edge attribution can be A/B'd alone
/// (irback proposal W11-2). The equal-depth rule turns it on as well.
/// Read live, for the reason [`sink_equal_depth_enabled`] gives.
fn sink_phi_edge_enabled() -> bool {
    matches!(
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_IR_SINK_PHI_EDGE").as_deref(),
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
/// does, **judged from the opcode alone**?
///
/// Pure arithmetic only: no memory edge, no control edge, and nothing that can
/// trap. `Op::Const` is absent because it is already in the entry block (it has
/// no inputs); `Op::Phi` is pinned to its merge.
///
/// `Op::Div` and `Op::Rem` are absent **here** because the integer forms trap,
/// and moving a trap changes where the exception is raised. That is an opcode
/// answer to a question the opcode cannot fully decide, which is why
/// [`node_is_sinkable`] exists: whether a division traps is a property of the
/// node's TYPE, not of its opcode. See there.
///
/// `Op::FCmp` and the three narrowing conversions `I2B`/`I2C`/`I2S` used to be
/// missing from this list with no reason recorded anywhere. They are in
/// `Op::is_pure()`, they are total functions of two (resp. one) value inputs
/// with no control and no memory edge, and `ir_verify::expected_arity` gives
/// them fixed arity — the same argument that puts `Op::LCmp` and `Op::L2I`
/// here. `FCmp`'s NaN polarity rides in the op itself (`nan_greater`), so
/// moving one cannot change which answer it gives.
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
            | Op::I2B
            | Op::I2C
            | Op::I2S
            | Op::Cmp(_)
            | Op::LCmp
            | Op::FCmp { .. }
    )
}

/// May the node at `id` be moved to a different block?
///
/// [`op_is_sinkable`] plus the one case the opcode cannot answer on its own.
///
/// # Floating-point division does not trap, and it is the expensive one
///
/// The sink pass exists to move work out of a loop that does not need it, so
/// the nodes it most wants to move are the slow ones — and the list it had
/// excluded every node with a latency above three cycles. `Op::Div` was the
/// biggest omission, and it was excluded for a reason that holds for exactly
/// half of its instances:
///
/// * an INTEGER `idiv`/`ldiv`/`irem`/`lrem` raises `ArithmeticException` on a
///   zero divisor, and `i64::MIN / -1` overflows `idiv` into `#DE` as well.
///   Both are deopt points anchored to a bci, so moving the division moves
///   where the program throws. Still refused.
/// * a FLOATING-POINT `fdiv`/`ddiv` (`Op::Div` with `IrType::Float` or
///   `IrType::Double`) raises **nothing**. JVMS §6.5 (`fdiv`, `ddiv`) defines
///   division by zero as ±infinity and 0/0 as NaN, and the JVM's FP
///   environment has every IEEE-754 exception masked, so `divss`/`divsd`
///   cannot fault. It is a total function of two value inputs with no control
///   edge and no memory edge — the same shape as `Op::Mul`, at roughly four
///   times the latency (see [`op_latency`]).
///
/// `Op::Rem` on `Float`/`Double` is deliberately NOT included even though it
/// also cannot throw: `frem`/`drem` have no single SSE instruction and lower
/// to a `jit_frem`/`jit_drem` helper CALL (see the 0x72/0x73 arms of the IR
/// builder). A call is a safepoint, and this pass does not move safepoints.
///
/// Note that `Op::Div` is *already* a floating data node — `add_data` gives it
/// two value inputs and no control edge, so `find_best_block` places it in the
/// deepest block its operands dominate. Sinking it is therefore the same class
/// of motion the placement step already performs, not a new freedom; and the
/// integer form's trap is carried by a SEPARATE `Op::Guard` node with a real
/// `[ctrl, cond]` edge that this pass never touches.
fn node_is_sinkable(graph: &Graph, id: usize) -> bool {
    let Some(node) = graph.nodes.get(id) else {
        return false;
    };
    if op_is_sinkable(&node.op) {
        return true;
    }
    matches!(node.op, Op::Div) && matches!(node.ty, IrType::Float | IrType::Double)
}

/// Loop nesting depth per block, from the back edges the dominator relation
/// already knows about.
///
/// A back edge is an edge `u -> v` whose target dominates its source; its
/// natural loop is `v` plus everything that reaches `u` without passing
/// through `v`. Same definition `ir_lower`'s poll placement rests on, computed
/// here from `dom` rather than re-derived.
///
/// # The one copy (round 11 wave 3)
///
/// `regalloc::ls_loop_depths` used to be a second, hand-maintained copy of
/// this — same back-edge test, same predecessor walk — kept because this
/// function was private to a file the register allocator's lane could not
/// widen, and it lagged a round behind once (see below). It now calls this
/// function (hence `pub(crate)`), so the two cannot disagree.
///
/// # One loop per HEADER, not per back edge (round 9)
///
/// This used to add one level per back EDGE. A loop with two latches — a
/// `while` whose `continue` jumps straight back to the header, next to the
/// normal back edge — is one loop, and its body came out two levels deep (and
/// the blocks only one latch reaches, one level). The frequency model beside it
/// (`compute_frequencies`) already unions the latches per header, so the two
/// disagreed about the same CFG. The latches are now unioned per header here
/// too, and an edge out of a block the entry does NOT reach is never a back
/// edge: `Dominators::dominates` answers `true` for any dominator of an
/// unreachable block, so every edge from dead code into live code used to read
/// as a loop and deepen the live block it lands on.
///
/// `regalloc::ls_loop_depths` made the same change a round later — the lag
/// that ended with it delegating here.
pub(crate) fn loop_depths(blocks: &[Block], dom: &Dominators) -> Vec<u32> {
    loop_nest(blocks, dom).0
}

/// [`loop_depths`] plus the innermost enclosing loop HEADER of every block
/// (`None` outside every loop).
///
/// The depth alone is a nesting count, not a loop identity: two sibling loops
/// are both depth 1. The equal-depth sink rule needs the identity, or it moves
/// a value out of one loop into a sibling loop whose header the first one
/// exits into (`docs/known-issues/jit/ir-sink-equal-depth-can-cross-into-a-sibling-loop-20260918.md`).
///
/// The innermost header of a block is the header, among those whose natural
/// loop contains it, with the greatest depth. Natural loops unioned per header
/// are nested or disjoint on a reducible CFG, so that header's loop is the
/// smallest one containing the block. On an irreducible CFG two such headers
/// can tie; the lower index wins so the answer does not depend on iteration
/// order (the same rule `compute_frequencies` uses for its own `innermost`).
fn loop_nest(blocks: &[Block], dom: &Dominators) -> (Vec<u32>, Vec<Option<usize>>) {
    let n = blocks.len();
    let mut depth = vec![0u32; n];
    // Members of each header's loop, recorded so the innermost header can be
    // chosen once every depth is final.
    let mut members: Vec<(usize, Vec<usize>)> = Vec::new();
    let mut latches: Vec<Vec<usize>> = vec![Vec::new(); n];
    for u in 0..n {
        if !dom.is_reachable(u) {
            continue;
        }
        for &v in &blocks[u].successors {
            if v >= n || !dom.dominates(v, u) {
                continue;
            }
            if !latches[v].contains(&u) {
                latches[v].push(u);
            }
        }
    }
    let mut in_loop = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    for v in 0..n {
        if latches[v].is_empty() {
            continue;
        }
        in_loop.iter_mut().for_each(|x| *x = false);
        in_loop[v] = true;
        stack.clear();
        for &u in &latches[v] {
            // A self loop (`u == v`) is already marked: its body is the header.
            if !in_loop[u] {
                in_loop[u] = true;
                stack.push(u);
            }
        }
        while let Some(b) = stack.pop() {
            for &p in &blocks[b].predecessors {
                if p < n && !in_loop[p] {
                    in_loop[p] = true;
                    stack.push(p);
                }
            }
        }
        let mut body: Vec<usize> = Vec::new();
        for (b, inside) in in_loop.iter().enumerate() {
            if *inside {
                depth[b] = depth[b].saturating_add(1);
                body.push(b);
            }
        }
        members.push((v, body));
    }
    let mut innermost: Vec<Option<usize>> = vec![None; n];
    // `members` is in increasing header order, so a strict `>` keeps the
    // lower-indexed header on a tie.
    for (h, body) in &members {
        for &b in body {
            let better = match innermost[b] {
                None => true,
                Some(cur) => depth[*h] > depth[cur],
            };
            if better {
                innermost[b] = Some(*h);
            }
        }
    }
    (depth, innermost)
}

/// Do `a` and `b` sit in the same innermost loop (or both outside every loop)?
/// An index past the end of `innermost` answers only for another such index,
/// which keeps a mis-sized table from licensing a move.
fn same_innermost_loop(innermost: &[Option<usize>], a: usize, b: usize) -> bool {
    innermost.get(a) == innermost.get(b)
}

/// The deepest block that dominates every block in `of`, or `None`.
///
/// The same answer as [`deepest_common_dominator_by_scan`], which tests every
/// block, but found by walking the dominator tree:
///
/// * a member past the end is dominated by no block, so the answer is `None`;
/// * an unreachable member is dominated by EVERY block, so it constrains
///   nothing, and the answer is the nearest common dominator of the reachable
///   members, found by climbing `idom` chains;
/// * when every member is unreachable, every block qualifies, and the scan's
///   "deeper wins" rule settles on the highest-numbered unreachable block
///   (pinned by
///   `sink_common_dominator_treats_an_unreachable_use_as_dominated_by_every_block`).
///
/// An empty `of`, or a relation whose size is not `nb`, is answered by the scan
/// itself. The sink pass never asks either question.
fn deepest_common_dominator(dom: &Dominators, of: &[usize], nb: usize) -> Option<usize> {
    if of.is_empty() || nb != dom.len() {
        return deepest_common_dominator_by_scan(dom, of, nb);
    }
    let mut common: Option<usize> = None;
    for &b in of {
        if b >= nb {
            return None;
        }
        if !dom.is_reachable(b) {
            continue;
        }
        common = Some(match common {
            None => b,
            Some(mut x) => {
                // The entry dominates every reachable block, so this stops at
                // block 0 at the latest.
                while !dom.dominates(x, b) {
                    match dom.idom(x) {
                        Some(up) => x = up,
                        None => return deepest_common_dominator_by_scan(dom, of, nb),
                    }
                }
                x
            }
        });
    }
    match common {
        Some(x) => Some(x),
        None => dom.last_unreached(),
    }
}

/// Where the sink pass puts a node whose inputs put it in `early` and whose uses
/// need it no lower than `late`. `early` must dominate `late`.
///
/// Among the blocks on the dominator path between the two, the shallowest loop
/// nesting wins, and among equally shallow blocks the one NEAREST `early`
/// (highest in the dominator tree). With `equal_depth` on, a tie then goes to
/// the later block, the one the current best dominates. The same answer as
/// [`choose_sink_block_by_scan`], which tests every block for "on the path",
/// but it visits only the path: the `idom` chain from `late` up to `early`,
/// visited from `early` down. `path` is scratch space, reused across calls.
///
/// # Ties break by dominance, not by block number (round 11 wave 12)
///
/// The path used to be visited in block-number order. Block numbers are
/// creation order, and `IrBuilder::build` pre-creates every jump target's
/// merge in pc order before it creates any `Proj`, so a join below the loop
/// exit usually has the LOWER number. Among equally shallow candidates the
/// join therefore beat the exit that dominates it, and where a value landed
/// depended on node-allocation order rather than on the CFG. That tie-break is
/// what put a φ's edge input into the φ's own block
/// (`r11w10-orch-schedule-places-phi-edge-input-off-its-edge`). Visiting the
/// chain from `early` down makes "the first strictly shallower block" the one
/// nearest `early` (irback proposal W11-3).
///
/// When `late` is unreachable, every block dominates it, so the candidates are
/// not a path. That case, and a relation whose size is not `nb`, go to the
/// scan.
#[allow(clippy::too_many_arguments)]
fn choose_sink_block(
    dom: &Dominators,
    depth: &[u32],
    innermost: &[Option<usize>],
    early: usize,
    late: usize,
    equal_depth: bool,
    nb: usize,
    path: &mut Vec<usize>,
) -> usize {
    if nb != dom.len() || !dom.is_reachable(late) {
        return choose_sink_block_by_scan(dom, depth, innermost, early, late, equal_depth, nb);
    }
    path.clear();
    let mut x = late;
    loop {
        path.push(x);
        if x == early {
            break;
        }
        match dom.idom(x) {
            Some(up) => x = up,
            // The chain reached the entry without meeting `early`, so `early`
            // does not dominate `late`. The caller rules that out; let the
            // scan answer rather than guess.
            None => {
                return choose_sink_block_by_scan(
                    dom,
                    depth,
                    innermost,
                    early,
                    late,
                    equal_depth,
                    nb,
                )
            }
        }
    }
    // Collected from `late` up; every entry dominates the one before it, so
    // reversed it is in dominator-tree preorder, `early` first.
    path.reverse();
    let mut best = early;
    for &cand in path.iter() {
        if depth[cand] < depth[best] {
            best = cand;
            continue;
        }
        if equal_depth
            && depth[cand] == depth[best]
            && cand != best
            && same_innermost_loop(innermost, best, cand)
            && dom.dominates(best, cand)
        {
            best = cand;
        }
    }
    best
}

/// [`choose_sink_block`] by testing every block. This is the placement loop
/// the sink pass ran before it walked the dominator tree. It is kept as the
/// answer for the cases the walk hands back, and as the reference the tests
/// hold the walk to.
///
/// Candidates are visited in dominator-tree preorder (then block number, for
/// the unreachable blocks, which have no preorder), not in block-number order:
/// see "Ties break by dominance" on [`choose_sink_block`]. For a reachable
/// `late` the candidates are exactly the `idom` chain, so the order is the
/// walk's.
fn choose_sink_block_by_scan(
    dom: &Dominators,
    depth: &[u32],
    innermost: &[Option<usize>],
    early: usize,
    late: usize,
    equal_depth: bool,
    nb: usize,
) -> usize {
    let mut candidates: Vec<usize> = (0..nb)
        .filter(|&cand| dom.dominates(early, cand) && dom.dominates(cand, late))
        .collect();
    candidates.sort_by_key(|&b| (dom.preorder(b).unwrap_or(usize::MAX), b));
    let mut best = early;
    for cand in candidates {
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
        //
        // Equal DEPTH is not the same LOOP: a sibling loop the current one
        // exits straight into is also at this depth, and moving the value
        // there runs it once per iteration of an unrelated loop. The
        // candidate must share `best`'s innermost header.
        if equal_depth
            && depth[cand] == depth[best]
            && cand != best
            && same_innermost_loop(innermost, best, cand)
            && dom.dominates(best, cand)
        {
            best = cand;
        }
    }
    best
}

/// [`deepest_common_dominator`] by testing every block, as it was computed
/// before the dominator-tree walk. Kept as the answer for the cases the walk
/// hands back, and as the reference the tests hold the walk to.
fn deepest_common_dominator_by_scan(dom: &Dominators, of: &[usize], nb: usize) -> Option<usize> {
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
/// # The two obligations, and why a failure reverts everything
///
/// [`safepoints_dominate_anchors`] states the safepoint one and
/// [`sunk_reads_dominated`] the read one; [`placement_is_sound`] is both. The
/// check is made against the FINAL placement; a violation first retries the
/// placement WITHOUT the equal-depth rule, and reverts the whole method only if
/// the depth rule alone cannot satisfy it either. `reverted` counts that.
fn sink_pure_nodes(
    graph: &Graph,
    blocks: &mut [Block],
    node_to_block: &mut [usize],
    dom: &Dominators,
    keep: &[bool],
) -> (usize, usize) {
    let nb = blocks.len();
    if nb < 2 {
        return (0, 0);
    }
    let (depth, innermost) = loop_nest(blocks, dom);
    let original: Vec<usize> = node_to_block.to_vec();

    // Read once per call, not once per process: `sink_equal_depth_enabled`
    // documents why this may not be cached across compiles.
    let equal_depth = sink_equal_depth_enabled();
    let phi_edge = equal_depth || sink_phi_edge_enabled();

    let mut moved = place_sunk_nodes(
        graph,
        blocks,
        node_to_block,
        dom,
        &depth,
        &innermost,
        equal_depth,
        phi_edge,
        keep,
    );
    let mut ok = moved == 0 || placement_is_sound(graph, node_to_block, &original, dom, nb);
    let mut retried = false;

    // The equal-depth rule sinks strictly more nodes than the depth rule alone,
    // so it has strictly more ways to violate the safepoint obligation — and
    // the revert is whole-method. Falling straight back to "no sinking at all"
    // would therefore let the NEW rule cost the OLD one its wins on any method
    // that happens to name a sunk value in a deopt frame, which is a regression
    // dressed as a conservative choice. Retry with the rule this pass shipped
    // with instead, and revert only if THAT also fails.
    if !ok && phi_edge {
        node_to_block.copy_from_slice(&original);
        moved = place_sunk_nodes(
            graph,
            blocks,
            node_to_block,
            dom,
            &depth,
            &innermost,
            false,
            false,
            keep,
        );
        ok = moved == 0 || placement_is_sound(graph, node_to_block, &original, dom, nb);
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
    //
    // A SUNK node goes to the FRONT of its new block (after any phis leading
    // it), not to wherever the flattened block-index order happens to put it.
    // `safepoints_dominate_anchors` accepted the move because the new block
    // dominates every anchor block -- reflexively, when the anchor is in that
    // same block -- and that argument is only true if the value is computed
    // before the anchor INSIDE the block too. Appending in flattened order put
    // it after the block's own nodes whenever its old block had a higher index
    // than its new one (block indices are creation order, not dominance
    // order), and `topo_sort_block` only pulls a node earlier for a user, never
    // for a frame state: a deopt at a call in that block then read the value's
    // home before anything wrote it. At the front it runs before every anchor
    // in the block, exactly as it ran before them in the block it left. Its
    // inputs are all in blocks strictly dominating this one (or are other sunk
    // nodes, which `topo_sort_block` orders among themselves), so the front is
    // always a legal position.
    let placed: Vec<NodeId> = blocks
        .iter()
        .flat_map(|b| b.nodes.iter().copied())
        .collect();
    let mut own: Vec<Vec<NodeId>> = vec![Vec::new(); nb];
    let mut sunk: Vec<Vec<NodeId>> = vec![Vec::new(); nb];
    for id in placed {
        let b = node_to_block[id as usize];
        if b != usize::MAX && b < nb {
            if original.get(id as usize) == Some(&b) {
                own[b].push(id);
            } else {
                sunk[b].push(id);
            }
        }
    }
    for (b, blk) in blocks.iter_mut().enumerate() {
        let mine = std::mem::take(&mut own[b]);
        let incoming = std::mem::take(&mut sunk[b]);
        let lead = mine
            .iter()
            .take_while(|&&id| {
                graph
                    .nodes
                    .get(id as usize)
                    .is_some_and(|n| matches!(n.op, Op::Phi))
            })
            .count();
        let mut rebuilt: Vec<NodeId> = Vec::with_capacity(mine.len() + incoming.len());
        rebuilt.extend_from_slice(&mine[..lead]);
        rebuilt.extend(incoming);
        rebuilt.extend_from_slice(&mine[lead..]);
        blk.nodes = rebuilt;
    }
    if retried && cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_IR_SINK") {
        eprintln!(
            "[ir-sink] equal-depth placement broke a safepoint or a read; fell back to depth-only"
        );
    }
    (moved, 0)
}

/// Choose a block for every sinkable node, writing the choice into
/// `node_to_block`. Returns how many nodes it moved.
///
/// Split out of [`sink_pure_nodes`] so the equal-depth rule can be retried
/// without it — see the fallback there. Makes no check of its own: both
/// obligations are properties of the FINAL placement and are checked once, by
/// [`placement_is_sound`].
///
/// A node `keep` marks (see [`ScheduleOptions::extra_frame_named`]) stays where
/// its inputs put it.
#[allow(clippy::too_many_arguments)]
fn place_sunk_nodes(
    graph: &Graph,
    blocks: &[Block],
    node_to_block: &mut [usize],
    dom: &Dominators,
    depth: &[u32],
    innermost: &[Option<usize>],
    equal_depth: bool,
    phi_edge: bool,
    keep: &[bool],
) -> usize {
    let nb = blocks.len();
    let mut moved = 0usize;
    // Scratch for `choose_sink_block`, reused across nodes and rounds.
    let mut path: Vec<usize> = Vec::new();

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
            // Where a phi's value input is read when its edge block is not
            // known (or not modelled): the phi's IMMEDIATE DOMINATOR, never
            // the phi's own block. The value is copied on the incoming edge,
            // BEFORE that block runs, so the φ's block is not a legal home for
            // it (round 11 wave 11, `r11w10-orch-schedule-places-phi-edge-
            // input-off-its-edge`). Every strict dominator of a block
            // dominates each of its predecessors, so `idom` is the deepest
            // legal stand-in for "some predecessor".
            //
            // Attributing the read to the φ's own block made that block `late`,
            // and `choose_sink_block` then took it whenever it was the lowest-
            // numbered shallower block on the path — which it is whenever the
            // builder's pre-created merge (pc order, before any `Proj`) outranks
            // the exit block. `return held == 0 ? s : -1 - held;` after a loop
            // sank `-1 - held` out of the header into the ternary's join, and
            // the join's φ then read it on the edge from the arm that was
            // supposed to compute it. The post-schedule φ check refused the
            // compile; before round 11 wave 2 added that check, the edge copy
            // read a home nothing had written.
            //
            // `None` (the entry, or an unreachable block) keeps `ub`: the entry
            // has no predecessor to read on, and every block dominates an
            // unreachable one.
            let phi_edge_fallback = if matches!(un.op, Op::Phi) {
                dom.idom(ub).unwrap_or(ub)
            } else {
                ub
            };
            // A phi reads each value input ON ITS OWN EDGE. Attributing those
            // reads to the phi's block makes a loop header a use site for every
            // carried value and pins all of them above the body; attributing
            // them to the matching predecessor is what lets a carried value
            // sink to the copy that produces the next one. See
            // `sink_equal_depth_enabled` for why this travels with the
            // equal-depth rule; `sink_phi_edge_enabled` turns it on alone.
            //
            // Falls back to the conservative whole-node attribution whenever
            // the merge's arity does not match the phi's — an unmatched phi is
            // a shape this pass does not model, and modelling it wrongly would
            // sink a value past a reader.
            if phi_edge && matches!(un.op, Op::Phi) {
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
                        // phi's immediate dominator for THIS input rather than
                        // dropping the use entirely — a dropped use is a node
                        // free to sink below a reader. (Not the phi's own
                        // block: see `phi_edge_fallback`.)
                        let eb = match edges[k] {
                            e if e != usize::MAX && e < nb => e,
                            _ => phi_edge_fallback,
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
            for (slot, &inp) in un.inputs.iter().enumerate() {
                if inp == NO_NODE {
                    continue;
                }
                // Slot 0 of a phi is its merge (a control node, never sunk);
                // every other slot is a value read on an edge.
                let at = if slot == 0 { ub } else { phi_edge_fallback };
                if let Some(v) = use_blocks.get_mut(inp as usize) {
                    if !v.contains(&at) {
                        v.push(at);
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
            if !node_is_sinkable(graph, id) || keep.get(id).copied().unwrap_or(false) {
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
            // Only the dominator path from `early` down to `late` is a
            // candidate; `choose_sink_block` walks that path instead of
            // testing every block.
            let best = choose_sink_block(
                dom,
                depth,
                innermost,
                early,
                late,
                equal_depth,
                nb,
                &mut path,
            );
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
///
/// `first_at_bci` is the by-bci fallback precomputed once per call of
/// [`safepoints_dominate_anchors`] (first snapshot index per bci). It replaced a
/// `position` scan of `graph.safepoints` per NODE — O(nodes x snapshots), and
/// the builder records a snapshot at nearly every bci.
fn resolved_snapshot(
    graph: &Graph,
    n: &crate::ir::Node,
    first_at_bci: &HashMap<usize, usize>,
    splices: Option<&crate::ir::IrSpliceResumeMap>,
) -> Option<usize> {
    let pc = n.bytecode_pc?;
    if let Some(si) = n.frame_snapshot {
        // The node's own frame, at its pc or at the bci that pc resumes at out
        // of a spliced body (`Graph::set_node_frame_snapshot`, round 11 wave 5).
        // The by-bci fallback below stays on the raw pc.
        let at = graph.safepoints.get(si as usize).map(|s| s.bci);
        if at == Some(pc) || (at.is_some() && at == splices.and_then(|m| m.resume_bci(pc))) {
            return Some(si as usize);
        }
    }
    first_at_bci.get(&pc).copied()
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
    let mut first_at_bci: HashMap<usize, usize> = HashMap::with_capacity(graph.safepoints.len());
    for (si, sp) in graph.safepoints.iter().enumerate() {
        first_at_bci.entry(sp.bci).or_insert(si);
    }
    let mut anchors: std::collections::HashMap<usize, Vec<usize>> =
        std::collections::HashMap::new();
    let splices = crate::ir::splice_resume_map_this_build();
    for (nid, n) in graph.nodes.iter().enumerate() {
        if let Some(si) = resolved_snapshot(graph, n, &first_at_bci, splices.as_ref()) {
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
        for &v in sp
            .locals
            .iter()
            .chain(sp.stack.iter())
            .chain(sp.monitors.iter())
        {
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

/// Both obligations a sink placement owes: [`safepoints_dominate_anchors`]
/// and [`sunk_reads_dominated`]. The cheaper safepoint half runs first.
fn placement_is_sound(
    graph: &Graph,
    node_to_block: &[usize],
    original: &[usize],
    dom: &Dominators,
    nb: usize,
) -> bool {
    safepoints_dominate_anchors(graph, node_to_block, original, dom, nb)
        && sunk_reads_dominated(graph, node_to_block, original, dom, nb)
}

/// Does every node this pass MOVED still dominate each of its readers?
///
/// Round 11 wave 12 (irback proposal W11-1). The pass derives `late` from its
/// own model of where each read happens, and nothing re-checked the placement
/// against the reads themselves: when that model was wrong (a φ's edge read
/// counted in the φ's own block, `r11w10-orch-schedule-places-phi-edge-input-
/// off-its-edge`), the misplaced read surfaced only at the release
/// post-schedule refusal ([`Schedule::misplaced_reads`]), which costs the
/// method its optimizing compile. Checked here, the same defect costs one
/// optimization: the fallback in [`sink_pure_nodes`] retries depth-only and
/// then reverts.
///
/// The read sites are [`Schedule::misplaced_reads`]'s, so a placement this
/// accepts is one that check accepts too, as far as moved values go:
///
/// * a φ reads value input `k + 1` on the edge from its merge's control input
///   `k`, in that input's block. When the merge has no such input, or it has no
///   block, the read is taken at the φ block's immediate dominator, the stand-in
///   [`place_sunk_nodes`] itself uses;
/// * every other placed node reads each input in its own block. Intra-block
///   order is `topo_sort_block`'s job, as it is for the moves themselves.
///
/// A reader or a definition with no block (`usize::MAX`) is skipped, and so is
/// a definition still at its original block: this pass owes nothing for a
/// placement it did not choose.
fn sunk_reads_dominated(
    graph: &Graph,
    node_to_block: &[usize],
    original: &[usize],
    dom: &Dominators,
    nb: usize,
) -> bool {
    let block_of = |id: NodeId| -> Option<usize> {
        if id == NO_NODE {
            return None;
        }
        match node_to_block.get(id as usize) {
            Some(&b) if b < nb => Some(b),
            _ => None,
        }
    };
    // The new block of a node this pass moved; `None` for everything else.
    let moved_to = |id: NodeId| -> Option<usize> {
        let b = block_of(id)?;
        (original.get(id as usize) != Some(&b)).then_some(b)
    };
    for (uid, user) in graph.nodes.iter().enumerate() {
        let Some(ub) = node_to_block.get(uid).copied().filter(|&b| b < nb) else {
            continue;
        };
        if matches!(user.op, Op::Phi) {
            let merge = user
                .inputs
                .first()
                .and_then(|&m| graph.nodes.get(m as usize));
            let fallback = dom.idom(ub).unwrap_or(ub);
            for (k, &v) in user.inputs.iter().skip(1).enumerate() {
                let Some(vb) = moved_to(v) else {
                    continue;
                };
                let at = merge
                    .and_then(|m| m.inputs.get(k).copied())
                    .and_then(block_of)
                    .unwrap_or(fallback);
                if !dom.dominates(vb, at) {
                    return false;
                }
            }
        } else {
            for &inp in user.inputs.iter() {
                let Some(vb) = moved_to(inp) else {
                    continue;
                };
                if !dom.dominates(vb, ub) {
                    return false;
                }
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
    /// Successor blocks: 0, 1 or 2, or one per projection of a `Switch`.
    pub successors: Vec<usize>,
    /// Predecessor blocks.
    pub predecessors: Vec<usize>,
}

/// Result of scheduling: an ordered list of basic blocks.
pub struct Schedule {
    pub blocks: Vec<Block>,
    /// Maps NodeId → block index.
    pub node_to_block: Vec<usize>,
    /// Dominator relation over the block CFG, indexed by the FINAL block
    /// numbers: `dom.dominates(d, b)` is true iff block `d` dominates block
    /// `b`. Kept so callers can prove that a value's defining block always runs
    /// before a deopt point: the guard-surviving scalar-replacement producer
    /// does, and so does `ir_check_elim`.
    ///
    /// It used to be published as a dense `Vec<Vec<bool>>` matrix, O(B²)
    /// memory, built only because `ir_check_elim` indexed it. Every reader now
    /// asks [`Dominators::dominates`], which answers the same in O(1).
    pub dom: Dominators,
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
    /// Scheduled nodes that read an input whose definition does not dominate
    /// the read, in block order — SSA's one structural invariant, checked on
    /// the finished schedule.
    ///
    /// Three halves (round 11 wave 2 added the last two):
    ///
    /// * a **data node** reads every input in its own block, so each input's
    ///   block must dominate it;
    /// * a block's **terminator** (`If`, `Switch`, `Return`, `Throw`) the same
    ///   way. It is in no `Block::nodes` list, so a `Return` in a join reading a
    ///   load from one arm was invisible here — and to
    ///   `ir_lower::verify_data_locations`, which compares EMISSION order and
    ///   passes whenever that arm happens to be laid out first;
    /// * a **φ** reads value input `k + 1` on the edge from its merge's
    ///   control input `k`, so that input's block must dominate the
    ///   predecessor block the edge leaves — not the merge.
    ///
    /// Round 11 wave 4 adds a fourth, the release form of the verifier's
    /// side-effect rule (`dominance_violations`' `Input` finding for a
    /// `Store`/`ArrayStore`/`Call`/`MonitorEnter`/`MonitorExit`):
    ///
    /// * a node with a **side effect** anchored on a control token that heads
    ///   a block must BE in that block. `find_best_block` places it in the
    ///   deepest of its input blocks, so it leaves its anchor only when some
    ///   input is defined strictly below the anchor — the node then runs on
    ///   only some of the paths through its anchor, and every input it reads
    ///   dominates its new block, so the three halves above are all satisfied.
    ///   (No later step moves such a node: the Step 4b sink moves pure nodes
    ///   only, and a layout permutes node and anchor together.)
    ///
    ///   Round 11 wave 10 extended the debug rule to every pinned op
    ///   ([`pinned_anchor`]); wave 11 released this half for every pinned op
    ///   too ([`anchor_rule_is_released`], the switch that staged it).
    ///
    /// An input with no block (`usize::MAX`: a control token that heads no
    /// block, `Start`'s projections, an unplaced node) is skipped;
    /// `ir_lower::verify_data_locations` names the unplaced-value case.
    ///
    /// Empty for every graph the scheduler can place legally. A non-empty
    /// answer is either the shape `find_best_block` falls back on (a node whose
    /// inputs live in incomparable blocks) or a graph that reads a value on a
    /// path that never computed it, and it names the node so a refusal can say
    /// which pass broke the graph instead of reporting a generic def-after-use.
    /// `lib.rs::try_compile_inner` refuses the compile on the first entry
    /// (`"post-schedule"` bailout; kill switch `CRATONVM_JIT_VERIFY_IR=0`).
    /// What it does NOT check is the frame-state half of
    /// [`dominance_violations`] — see that function.
    ///
    /// The node ids of `Schedule::misplaced_reads`, in the same order.
    pub fn misplaced_data_nodes(&self, graph: &Graph) -> Vec<NodeId> {
        self.misplaced_reads(graph).iter().map(|r| r.node).collect()
    }

    /// [`Schedule::misplaced_data_nodes`] with the READ that is wrong: one
    /// entry per offending node (its first offending input, in the order the
    /// halves are checked), in block order.
    ///
    /// Round 11 wave 8 (irback proposal W7-2): the refusal used to name the
    /// node alone, and triaging `r11w6-orch-post-schedule-phi-check-refuses-hashmap`
    /// needed an IR dump and a hand walk to learn that φ 116's input was n65
    /// on the edge from n92. [`MisplacedRead`] carries the slot, the
    /// definition and, for a φ, the predecessor edge, so a refusal can print
    /// all of it on one line.
    pub(crate) fn misplaced_reads(&self, graph: &Graph) -> Vec<MisplacedRead> {
        let block_of = |id: NodeId| -> Option<usize> {
            if id == NO_NODE {
                return None;
            }
            match self.node_to_block.get(id as usize) {
                Some(&b) if b != usize::MAX => Some(b),
                _ => None,
            }
        };
        let mut out = Vec::new();
        for (b, block) in self.blocks.iter().enumerate() {
            for &id in block.nodes.iter().chain(block.terminator.iter()) {
                let Some(node) = graph.nodes.get(id as usize) else {
                    continue;
                };
                let bad: Option<MisplacedRead> = if matches!(node.op, Op::Phi) {
                    // `[merge, v_0, …]` against `merge = [c_0, …]`, paired by
                    // position exactly as `ir_lower::emit_phi_copies` pairs them.
                    let merge = node
                        .inputs
                        .first()
                        .and_then(|&m| graph.nodes.get(m as usize));
                    match merge {
                        Some(merge) => {
                            node.inputs.iter().skip(1).enumerate().find_map(|(k, &v)| {
                                let c = merge.inputs.get(k).copied()?;
                                let (vb, pb) = (block_of(v)?, block_of(c)?);
                                (!self.dom.dominates(vb, pb)).then_some(MisplacedRead {
                                    node: id,
                                    slot: k + 1,
                                    def: v,
                                    edge: Some(c),
                                })
                            })
                        }
                        None => None,
                    }
                } else {
                    // The side-effect half: moved off the block its control
                    // anchor heads. [`pinned_anchor`] is the anchor
                    // `dominance_violations` uses, applied to every op
                    // [`anchor_rule_is_released`] answers for (all of them
                    // since wave 11). Reported as slot 0, "reads" its anchor.
                    let sunk_off_its_anchor = pinned_anchor(graph, node)
                        .filter(|_| anchor_rule_is_released(&node.op))
                        .filter(|&c| block_of(c).is_some_and(|anchor| anchor != b));
                    match sunk_off_its_anchor {
                        Some(c) => Some(MisplacedRead {
                            node: id,
                            slot: 0,
                            def: c,
                            edge: None,
                        }),
                        None => node.inputs.iter().enumerate().find_map(|(i, &inp)| {
                            let ib = block_of(inp)?;
                            (!self.dom.dominates(ib, b)).then_some(MisplacedRead {
                                node: id,
                                slot: i,
                                def: inp,
                                edge: None,
                            })
                        }),
                    }
                };
                if let Some(read) = bad {
                    out.push(read);
                }
            }
        }
        out
    }

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
        nb != block && self.dom.dominates(nb, block)
    }
}

/// One read [`Schedule::misplaced_reads`] refuses: `node.inputs[slot] == def`,
/// and `def`'s block does not dominate where the read happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MisplacedRead {
    /// The reading node (a data node, a terminator, a side-effecting node or
    /// a φ).
    pub node: NodeId,
    /// Which input of `node` is read. `0` with `def` a control token is the
    /// side-effect half: the node was scheduled off the block `def` heads.
    pub slot: usize,
    /// `node.inputs[slot]`.
    pub def: NodeId,
    /// For a φ, the merge's control input `slot - 1` — the edge whose copy
    /// reads `def`, and whose block `def` must dominate. `None` otherwise.
    pub edge: Option<NodeId>,
}

// ── Def-use dominance over the graph (round 11 wave 3) ──────────────

/// One definition-use pair that no schedule can honour. Produced by
/// [`dominance_violations`]; `ir_verify`'s dominance lane formats them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DominanceViolation {
    /// `user.inputs[slot] == def`, and `def`'s earliest block does not
    /// dominate the block `user` is fixed in: a terminator's own block, or the
    /// control anchor of a node with a side effect.
    Input {
        user: NodeId,
        slot: usize,
        def: NodeId,
    },
    /// φ `phi`'s value input `phi.inputs[slot] == def` does not dominate the
    /// predecessor block its edge (`merge.inputs[slot - 1]`) leaves.
    PhiEdge {
        phi: NodeId,
        slot: usize,
        def: NodeId,
    },
    /// `user` reads `a` and `b`, whose earliest blocks are incomparable: no
    /// block is dominated by both, so no placement of `user` is legal.
    Incomparable { user: NodeId, a: NodeId, b: NodeId },
    /// `graph.safepoints[snapshot]` names `def`, which does not dominate the
    /// block of `site` — a pinned node that deopts into that snapshot, so the
    /// deopt would rebuild a slot from a home nothing has written yet.
    FrameState {
        site: NodeId,
        snapshot: usize,
        def: NodeId,
    },
}

/// `id`'s block in an early placement, when it has one.
fn placed_block(early: &[usize], nb: usize, id: NodeId) -> Option<usize> {
    if id == NO_NODE {
        return None;
    }
    match early.get(id as usize) {
        Some(&b) if b < nb => Some(b),
        _ => None,
    }
}

/// The control token `node` is PINNED to: its `inputs[0]`, for every op whose
/// control slot is `inputs[0]` alone in [`crate::ir_verify::control_input_indices`]
/// (the one table of which ops have one), when that input really is a control
/// token. `None` for everything that floats, for the control nodes themselves
/// (`Proj`, and a one-predecessor `Merge`/`Region`, whose range is also
/// `0..1`), and for the terminators, which have their own half in
/// [`dominance_violations`] and [`Schedule::misplaced_reads`]. The
/// `IrType::Control` test keeps the compact hand-built forms (`Store` as
/// `[base, value]`, `Load` as `[base]`) exempt.
///
/// `find_best_block` places a pinned node in the deepest of its input blocks,
/// and the anchor is one of them, so the node leaves its anchor exactly when
/// some other input is defined strictly below the anchor — and then runs on
/// only some of the paths through it. For a `Guard`, a `New`, a `CheckCast`
/// or an `Unbox` that changes WHETHER the program traps or allocates on a
/// path. Until round 11 wave 10
/// (`r11w9-ircore-anchor-rule-covers-five-of-the-pinned-ops`) the rule named
/// only `Store`/`ArrayStore`/`Call`/`MonitorEnter`/`MonitorExit`, and every
/// other op in the table was silently re-scheduled onto some paths.
fn pinned_anchor(graph: &Graph, node: &crate::ir::Node) -> Option<NodeId> {
    if matches!(
        node.op,
        Op::If | Op::Switch { .. } | Op::Return | Op::Throw | Op::Proj(_) | Op::Merge | Op::Region
    ) {
        return None;
    }
    if crate::ir_verify::control_input_indices(node) != (0..1) {
        return None;
    }
    let &c = node.inputs.first()?;
    graph
        .nodes
        .get(c as usize)
        .is_some_and(|cn| cn.ty == IrType::Control)
        .then_some(c)
}

/// The ops whose [`pinned_anchor`] the RELEASE check
/// ([`Schedule::misplaced_reads`], the `"post-schedule"` refusal in `lib.rs`)
/// holds them to. The debug lane (`dominance_violations`, read by
/// `ir_verify::check_dominance`) holds every pinned op to it.
///
/// Released for EVERY pinned op since round 11 wave 11: the release check and
/// the debug lane now hold the same rule. Staged in wave 10 (five ops only)
/// against a pass that re-points an anchor upward while an operand stays
/// below. The passes that write a pinned node's slot 0 were read for that
/// shape and none can produce it (the argument is on
/// `r11w9-ircore-anchor-rule-covers-five-of-the-pinned-ops`, wave 11):
/// LICM's three re-anchor arms and the range-predicate planter move a node to
/// the pre-header only when every value operand is loop-invariant, and an
/// invariant operand of a node inside the loop is defined in a block that
/// strictly dominates the header — so it dominates the pre-header too;
/// `collapse_trivial_merges` hands a one-input merge's nodes to the block
/// that is that merge's only predecessor. Kept as a function (not inlined to
/// `true`) so a regression can re-stage one op without touching the callers;
/// the orchestrator's census is the check on this argument.
fn anchor_rule_is_released(_op: &Op) -> bool {
    true
}

/// The schedule-EARLY block of data node `id` — [`find_best_block`]'s answer,
/// computed the same way — or `usize::MAX` when the inputs' blocks are not
/// totally ordered by dominance, which is reported into `out` instead of being
/// papered over with block 0 the way the scheduler must.
fn early_block_of(
    graph: &Graph,
    id: NodeId,
    early: &[usize],
    dom: &Dominators,
    nb: usize,
    out: &mut Vec<DominanceViolation>,
) -> usize {
    let Some(node) = graph.nodes.get(id as usize) else {
        return usize::MAX;
    };
    if matches!(node.op, Op::Phi) {
        return node
            .inputs
            .first()
            .and_then(|&m| placed_block(early, nb, m))
            .unwrap_or(usize::MAX);
    }
    // A node pinned to a control token does not float: its block is its
    // anchor's, and an input that does not dominate that anchor is
    // `dominance_violations`' `Input` finding. Scanning its inputs here as
    // well would report the same defect a second time, as an `Incomparable`
    // between the anchor and the input.
    if let Some(c) = pinned_anchor(graph, node) {
        if let Some(b) = placed_block(early, nb, c) {
            return b;
        }
    }
    // The inputs' blocks must form one chain in the dominator tree: two blocks
    // that both dominate a third are comparable, so an incomparable pair is a
    // proof that no block is dominated by all of them. The scheduler's own
    // walk (round 12 wave 2: shared, so the two cannot drift).
    match deepest_input_block(&node.inputs, |inp| placed_block(early, nb, inp), dom) {
        Ok(deepest) => deepest.map_or(0, |(b, _)| b),
        Err((a, b)) => {
            out.push(DominanceViolation::Incomparable { user: id, a, b });
            usize::MAX
        }
    }
}

/// Every definition-use pair of `graph` that no schedule can honour — SSA's
/// one structural invariant, stated over the GRAPH rather than over a finished
/// [`Schedule`] (round 11 wave 3,
/// `r11-irback-no-def-use-dominance-check-anywhere-FIXED-20260924.md`).
///
/// It builds the scheduler's own block CFG ([`build_block_cfg`]) and places
/// every data node schedule-early exactly as [`find_best_block`] does, then
/// asks what that placement cannot fix by moving a node:
///
/// * a data node whose inputs live in incomparable blocks
///   ([`DominanceViolation::Incomparable`]) — `find_best_block` puts it in block
///   0, above both definitions;
/// * a terminator (`If`/`Switch`/`Return`/`Throw`) reading a value whose block
///   does not dominate the terminator's block — the terminator cannot move, and
///   `ir_lower::verify_data_locations` compares emission order, which passes
///   whenever the defining arm happens to be laid out first;
/// * a node pinned to a control anchor ([`pinned_anchor`]: every op
///   `ir_verify::control_input_indices` gives a control slot at input 0 —
///   the side-effecting ones, and since round 11 wave 10 also `Guard`, `New`,
///   `NewArray`, `CheckCast`, `InstanceOf`, `Unbox`, `LoadStatic`, the
///   constants, `Load`, `ArrayLoad` and the full `ArrayLength`) reading a
///   value whose block does not dominate that anchor — the scheduler would
///   sink the node into the input's block, onto only some of the paths
///   through its anchor;
/// * a φ value input that does not dominate the predecessor its edge leaves.
///
/// With `frame_states`, also: a value a safepoint snapshot names must dominate
/// every PINNED node (a live control anchor) that can deopt into that snapshot.
/// Pure deopting nodes (`Div`/`Rem`) are not anchors here: their early block
/// is the deepest of their inputs, which for a division by a loop-invariant
/// value is above the loop whose φs the snapshot names — a finding about where
/// the scheduler MAY put the node, not about the graph.
///
/// Silent on every shape it cannot map to a block: a control input that heads
/// no block (`Start` itself in a hand-built fixture), `NO_NODE`, a removed
/// node, an unreachable block (which [`Dominators::dominates`] reports as
/// dominated by everything). Allocation: the CFG, the dominator tree and one
/// word per node; one pass over the nodes after the placement.
pub(crate) fn dominance_violations(graph: &Graph, frame_states: bool) -> Vec<DominanceViolation> {
    let (blocks, mut early) = build_block_cfg(graph);
    let mut out = Vec::new();
    let nb = blocks.len();
    if nb == 0 {
        return out;
    }
    let dom = Dominators::compute(&blocks);
    let n = graph.nodes.len();

    // Early placement, inputs before users: the walk of `schedule_with_options`
    // Step 4, with `early_block_of` in place of `find_best_block`.
    let mut entered = vec![false; n];
    let mut walk: Vec<(NodeId, usize)> = Vec::new();
    for root in 0..n {
        if entered.get(root).copied().unwrap_or(true) || !awaits_placement(graph, &early, root) {
            continue;
        }
        if let Some(e) = entered.get_mut(root) {
            *e = true;
        }
        walk.push((root as NodeId, 0));
        while let Some(&(id, cursor)) = walk.last() {
            let Some(node) = graph.nodes.get(id as usize) else {
                walk.pop();
                continue;
            };
            let mut next = cursor;
            let mut descend: Option<NodeId> = None;
            if !matches!(node.op, Op::Phi) {
                while let Some(&inp) = node.inputs.get(next) {
                    next += 1;
                    if inp == NO_NODE {
                        continue;
                    }
                    let i = inp as usize;
                    if i < n && !entered[i] && awaits_placement(graph, &early, i) {
                        entered[i] = true;
                        descend = Some(inp);
                        break;
                    }
                }
            }
            if let Some(top) = walk.last_mut() {
                top.1 = next;
            }
            match descend {
                Some(inp) => walk.push((inp, 0)),
                None => {
                    walk.pop();
                    let b = early_block_of(graph, id, &early, &dom, nb, &mut out);
                    if let Some(slot) = early.get_mut(id as usize) {
                        *slot = b;
                    }
                }
            }
        }
    }

    // A node's fixed block when it cannot float: `Some(anchor)` for a node
    // pinned (see [`pinned_anchor`]) to a control token that heads a block.
    let pinned_block = |node: &crate::ir::Node| -> Option<usize> {
        placed_block(&early, nb, pinned_anchor(graph, node)?)
    };

    for (idx, node) in graph.nodes.iter().enumerate() {
        let id = idx as NodeId;
        match node.op {
            Op::Dead => {}
            Op::Phi => {
                let Some(merge) = node
                    .inputs
                    .first()
                    .and_then(|&m| graph.nodes.get(m as usize))
                else {
                    continue;
                };
                if !matches!(merge.op, Op::Merge | Op::Region) {
                    continue;
                }
                for (slot, &def) in node.inputs.iter().enumerate().skip(1) {
                    let Some(db) = placed_block(&early, nb, def) else {
                        continue;
                    };
                    let Some(pb) = merge
                        .inputs
                        .get(slot - 1)
                        .and_then(|&c| placed_block(&early, nb, c))
                    else {
                        continue;
                    };
                    if !dom.dominates(db, pb) {
                        out.push(DominanceViolation::PhiEdge { phi: id, slot, def });
                    }
                }
            }
            Op::If | Op::Switch { .. } | Op::Return | Op::Throw => {
                let Some(b) = placed_block(&early, nb, id) else {
                    continue;
                };
                // Only the terminator Step 2 kept; a second one on the same
                // head is `ir_verify::check_control_anchors`' finding.
                if blocks.get(b).and_then(|blk| blk.terminator) != Some(id) {
                    continue;
                }
                for (slot, &def) in node.inputs.iter().enumerate().skip(1) {
                    if let Some(db) = placed_block(&early, nb, def) {
                        if !dom.dominates(db, b) {
                            out.push(DominanceViolation::Input {
                                user: id,
                                slot,
                                def,
                            });
                        }
                    }
                }
            }
            _ => {
                let Some(anchor) = pinned_block(node) else {
                    continue;
                };
                for (slot, &def) in node.inputs.iter().enumerate().skip(1) {
                    if let Some(db) = placed_block(&early, nb, def) {
                        if !dom.dominates(db, anchor) {
                            out.push(DominanceViolation::Input {
                                user: id,
                                slot,
                                def,
                            });
                        }
                    }
                }
            }
        }
    }

    if frame_states && !graph.safepoints.is_empty() {
        let mut first_at_bci: HashMap<usize, usize> =
            HashMap::with_capacity(graph.safepoints.len());
        for (si, sp) in graph.safepoints.iter().enumerate() {
            first_at_bci.entry(sp.bci).or_insert(si);
        }
        let max_bci = graph.safepoints.iter().map(|s| s.bci).max().unwrap_or(0);
        let splices = crate::ir::splice_resume_map_this_build();
        for (idx, node) in graph.nodes.iter().enumerate() {
            if node.op == Op::Dead || crate::ir_lower::op_cannot_deopt(&node.op) {
                continue;
            }
            // Pinned: a control slot naming a control token that heads a
            // block. `ir_verify::control_input_indices` is the table of which
            // ops have one.
            let pinned = !crate::ir_verify::control_input_indices(node).is_empty()
                && node
                    .inputs
                    .first()
                    .and_then(|&c| graph.nodes.get(c as usize))
                    .is_some_and(|c| c.ty == IrType::Control);
            if !pinned {
                continue;
            }
            let id = idx as NodeId;
            let Some(site_block) = placed_block(&early, nb, id) else {
                continue;
            };
            // The resume rule of `ir_verify::check_deopt_coverage`, which is
            // `ir_lower`'s: a guard deopts at its own bci, everything else at
            // its pc, translated out of a spliced body when a map is in hand.
            let pc = match node.op {
                Op::Guard { bci } => Some(bci),
                _ => node.bytecode_pc,
            };
            let Some(pc) = pc else {
                continue;
            };
            let resume = match &splices {
                Some(map) => match map.resume_bci(pc) {
                    Some(r) => r,
                    None => continue,
                },
                None if pc > max_bci => continue,
                None => pc,
            };
            let own = node
                .frame_snapshot
                .map(|si| si as usize)
                .filter(|&si| graph.safepoints.get(si).is_some_and(|s| s.bci == resume));
            let Some(si) = own.or_else(|| first_at_bci.get(&resume).copied()) else {
                // No snapshot at all: `check_deopt_coverage`'s finding.
                continue;
            };
            // Round 13 wave 3 (lane framestate): a node inside a spliced body
            // may ALSO resume through the chain snapshot at its own pc
            // (`ir::SpliceScopeChain`), which names the callee's values and
            // every caller frame; its values must dominate the site too.
            let is_chain_at_pc = |ci: usize| {
                graph
                    .safepoints
                    .get(ci)
                    .is_some_and(|s| s.is_splice_state() && s.bci == pc)
            };
            let chain = node
                .frame_snapshot
                .map(|ci| ci as usize)
                .filter(|&ci| is_chain_at_pc(ci))
                .or_else(|| first_at_bci.get(&pc).copied().filter(|&ci| is_chain_at_pc(ci)))
                .filter(|&ci| ci != si);
            for si in std::iter::once(si).chain(chain) {
                let Some(sp) = graph.safepoints.get(si) else {
                    continue;
                };
                for &def in sp
                    .locals
                    .iter()
                    .chain(sp.stack.iter())
                    .chain(sp.monitors.iter())
                {
                    if let Some(db) = placed_block(&early, nb, def) {
                        if !dom.dominates(db, site_block) {
                            out.push(DominanceViolation::FrameState {
                                site: id,
                                snapshot: si,
                                def,
                            });
                        }
                    }
                }
            }
        }
    }
    out
}

// ── Scheduler ────────────────────────────────────────────────────────

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
/// + already_adjacent + deopt_between + memory_order == candidates`, checked by
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
    /// [`check_memory_order`] refused the permutation: sinking the operand
    /// would move a memory-ordering node past another one in a way the memory
    /// model does not allow.
    ///
    /// See the memory-order section on [`pair_single_use_operands`]. This pass
    /// used to be the ONE reordering in the scheduler that
    /// `check_memory_order` never saw — it runs after the single validation
    /// call — and its own screen (`op_cannot_deopt`) answers a different
    /// question, one that admits `Op::Phi`, `Op::Return` and `Op::Dead`, all of
    /// which carry `MemEffect::OPAQUE`.
    pub memory_order: usize,
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
            + self.memory_order
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
            self.memory_order,
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
static IR_PAIR_CENSUS: [std::sync::atomic::AtomicU64; 10] = [
    std::sync::atomic::AtomicU64::new(0),
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
        memory_order: v[9],
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
/// # Why the memory-order check is here and not implicit
///
/// This is step 5b, and it runs AFTER the one `check_memory_order` call in
/// `schedule_with_options` — which validates step 5's `topo_sort_block` /
/// `priority_sort_block` and nothing else. So for the life of this pass it was
/// the only reordering in the scheduler that the memory-order validator never
/// saw.
///
/// The screen it did have is the deopt one above, and a deopt question is not
/// a memory-order question. The two are coupled only by an accident that lives
/// in a DIFFERENT FILE: the producers this pass moves are
/// `ir_lower::op_home_is_one_store_rax`, which includes `Op::ArrayLoad`,
/// `Op::ArrayLength`, `Op::Div` and `Op::Rem` — memory-reading and trapping ops
/// — and sinking one is safe only if everything crossed is memory-inert.
/// `ir_lower::op_cannot_deopt` does not answer that question, and it is not
/// even accidentally equivalent to it: `Op::Phi` (memory-typed), `Op::Return`
/// and `Op::Dead` are all on that allowlist and all carry `MemEffect::OPAQUE`
/// (`ir::effect_of_node`). Crossing one of those moves a read past a node that
/// the model says writes anything — exactly the defect `check_memory_order`
/// exists to catch, and which its own tests pin for the *other* pass.
///
/// So every move is now validated with the same validator, over the window the
/// move permutes: `nodes[pi..ci]` before, and the same nodes with the producer
/// rotated to the end after. Nodes outside that window do not change their
/// relative order with anything, so the window is the whole question. In the
/// overwhelmingly common case the window is pure arithmetic, the check finds no
/// memory-ordering pair at all and costs a scan; refusals are counted as
/// [`PairCensus::memory_order`].
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
            // The consumer never moves: a sink removes the producer from below
            // it (shifting it down one) and re-inserts the producer at
            // `ci_now - 1` (shifting it back). So `ci` is still its index, and
            // the whole-block rescan this used to make per operand was an
            // O(block) search whose answer was known. The check stays as a
            // cheap guard on that argument.
            let ci_now = if nodes.get(ci) == Some(&cons) {
                ci
            } else {
                match nodes.iter().position(|&n| n == cons) {
                    Some(p) => p,
                    None => {
                        census.other_block += 1;
                        continue;
                    }
                }
            };
            // Searched backwards from the consumer: a single-use operand is
            // almost always close above it, so this is O(distance) instead of
            // O(block). A block's list is duplicate-free (`topo_sort_block`
            // emits each node once), so the nearest match is the only one.
            let Some(pi) = nodes[..ci_now].iter().rposition(|&n| n == prod) else {
                if nodes[ci_now + 1..].contains(&prod) {
                    census.after_consumer += 1;
                } else {
                    // The producer is scheduled in a DIFFERENT block. Sinking
                    // across a block boundary is a different transform with a
                    // different proof obligation, and this pass does not
                    // attempt it.
                    census.other_block += 1;
                }
                continue;
            };
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
            // The memory model has the last word, exactly as it does for step
            // 5 (`priority_sort_block`'s tail). The move permutes precisely
            // `nodes[pi..ci_now]` — the producer leaves the front of that
            // window and re-enters at its end — and every node outside the
            // window keeps its relative order with everything, so validating
            // the window validates the move. See the memory-order section
            // above for why this is not redundant with `deopt_between`.
            let order_is_legal = {
                let window_before: &[NodeId] = &nodes[pi..ci_now];
                let mut window_after: Vec<NodeId> = window_before.to_vec();
                let rotated = window_after.remove(0);
                window_after.push(rotated);
                check_memory_order(graph, window_before, &window_after).is_ok()
            };
            if !order_is_legal {
                census.memory_order += 1;
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

/// Schedule the IR graph into basic blocks with the PRODUCTION options.
///
/// `schedule_with_options(graph, &production_schedule_options())` — not the
/// all-off `ScheduleOptions::default()`, which this doc used to claim (the doc
/// had also drifted onto `use_counts` above). See
/// [`production_schedule_options`] for what is on and the kill switches.
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

/// Steps 1-3 of [`schedule_with_options`]: the block CFG, with no data node
/// placed yet.
///
/// Returns the blocks (heads, terminators, successor and predecessor edges; every
/// `nodes` list empty) and `node_to_block`, in which exactly the block heads and
/// the terminators have a block and every other entry is `usize::MAX`.
///
/// A function of its own (round 11 wave 3) so that `ir_verify`'s dominance lane
/// ([`dominance_violations`]) reasons over the CFG the scheduler will actually
/// build rather than over a second copy of "what heads a block" — the verifier
/// and this module disagreeing about exactly that was the subject of
/// `r11-irback-merge-inputs-and-control-anchors-unchecked-FIXED-20260924.md`.
fn build_block_cfg(graph: &Graph) -> (Vec<Block>, Vec<usize>) {
    let num_nodes = graph.nodes.len();

    // Step 1: Identify control-flow nodes and build block structure
    let mut blocks = Vec::new();
    let mut node_to_block = vec![usize::MAX; num_nodes];

    // Find all control nodes that start blocks:
    // Start, Merge, Region, Proj(0)/Proj(1) from If nodes, and EVERY Proj of a
    // Switch (a multi-way branch has one edge per case plus the default, and
    // each of them opens a block exactly as an `If` edge does).
    let mut block_heads: Vec<NodeId> = Vec::new();

    // Entry block starts at the control projection from Start
    for (id, node) in graph.nodes.iter().enumerate() {
        match &node.op {
            // `first()`, not `[0]`: a `Proj` with no producer edge is an
            // arity finding for `ir_verify`, not a panic on a compile thread
            // (the switch arm below already reads it this way).
            Op::Proj(0) if node.inputs.first().is_some_and(|&p| is_start(graph, p)) => {
                block_heads.push(id as NodeId);
            }
            Op::Merge | Op::Region => {
                block_heads.push(id as NodeId);
            }
            Op::Proj(0) | Op::Proj(1) if node.inputs.first().is_some_and(|&p| is_if(graph, p)) => {
                block_heads.push(id as NodeId);
            }
            // Unindexed on purpose: a `Switch`'s projection indices run
            // `0 ..= high - low + 1`, so there is no small set to enumerate,
            // and an index out of that range is `ir_verify::check_projections`'
            // business rather than this scan's.
            //
            // `inputs.first()` rather than `inputs[0]`: this is the first arm
            // that can be reached by a `Proj` with ANY index, so it is the
            // first that could meet one with no producer edge at all, and this
            // file is inside the no-panic compile path.
            Op::Proj(_) if node.inputs.first().is_some_and(|&p| is_switch(graph, p)) => {
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
        if !matches!(node.op, Op::If | Op::Switch { .. } | Op::Return | Op::Throw) {
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
    let mut if_projs: Vec<Vec<(u16, usize)>> = vec![Vec::new(); num_blocks];
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
                if tb >= num_blocks
                    || blocks[tb].terminator != Some(term)
                    || !(is_if(graph, term) || is_switch(graph, term))
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
    //
    // `edge_from[s] == b` <=> `s` is already a successor of `b` (round 11
    // wave 9, compile time). Block `b`'s successors, and `b`'s entries in
    // other blocks' predecessor lists, are all added in iteration `b` and
    // always together, so this one stamp answers both `contains` scans the
    // loop used to make — O(successors) per edge on a `Switch` (up to
    // `ir::SWITCH_MAX_PROJECTIONS` of them) and O(predecessors) per edge into
    // its join. Same lists, same order.
    let mut edge_from: Vec<usize> = vec![usize::MAX; num_blocks];
    for block_idx in 0..blocks.len() {
        if let Some(term) = blocks[block_idx].terminator {
            let term_node = &graph.nodes[term as usize];
            match &term_node.op {
                // `Op::Switch` shares this arm: its edges are projections of
                // its own node, pushed in projection order, exactly like an
                // `If`'s two. The only difference is how many there are, and
                // nothing below counts them.
                //
                // What a switch does NOT share is the "successors[0] is the
                // taken edge" reading. `ir_lower`'s `Op::Switch` arm resolves
                // each case's block from the graph — the `Proj(k)` users of the
                // switch, through `node_to_block` — rather than from a position
                // in this list, because the dedup below would silently shift
                // every later index if two edges ever did land on one block.
                Op::If | Op::Switch { .. } => {
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
                    let mut succs: Vec<(u16, usize)> = std::mem::take(&mut if_projs[block_idx]);
                    succs.sort_by_key(|&(which, _)| which);
                    for (_which, succ_block) in succs {
                        if edge_from[succ_block] != block_idx {
                            edge_from[succ_block] = block_idx;
                            blocks[block_idx].successors.push(succ_block);
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
        //
        // A SELF edge is kept (round 11). A loop header that names itself (a
        // `Merge` in every graph `IrBuilder` makes — `ensure_merge` never
        // builds an `Op::Region` — or a `Region` in a hand-built one) is a
        // loop whose body built no control node at all: `while (true) {
        // a[i] = i; i++; }` (it exits only by an exception) leaves
        // `self.ctrl == header` at the `goto`, and `patch_loop_backedge`
        // appends that token to the header. The same shape appears once
        // `ir_optimize::collapse_trivial_merges` forwards a single-input merge
        // (e.g. the join behind a branch `prune_always_taken_branch` turned
        // into a guard) that was the region's back-edge input. This used to
        // skip `succ_block == block_idx`, which left the loop block with no
        // terminator AND no successor: `ir_lower::lower_block` then emitted
        // no jump at all, so the loop ran once and fell into whatever block
        // the layout put next. Every consumer already models a self loop
        // (`loop_nest`, `compute_frequencies`, `regalloc::ls_loop_depths`,
        // `validate_order`'s retreating set, `emit_phi_copies`' by-token edge
        // match); only this edge builder refused to produce one.
        for &id in &fed_merges[block_idx] {
            let succ_block = node_to_block[id];
            if succ_block < num_blocks && edge_from[succ_block] != block_idx {
                edge_from[succ_block] = block_idx;
                blocks[block_idx].successors.push(succ_block);
                blocks[succ_block].predecessors.push(block_idx);
            }
        }
    }

    (blocks, node_to_block)
}

/// Schedule the IR graph into basic blocks, with frequency-driven block layout
/// and priority intra-block scheduling under caller control.
///
/// Total: this function never panics and never fails. Every optional stage
/// validates its own output and silently keeps the previous state when the
/// validation fails, recording the reason in [`Schedule::layout`].
pub fn schedule_with_options(graph: &Graph, opts: &ScheduleOptions) -> Schedule {
    let num_nodes = graph.nodes.len();

    // Steps 1-3: block heads, terminators, and the CFG edges between them.
    let (mut blocks, mut node_to_block) = build_block_cfg(graph);

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
        // Empty unless the caller listed deopt-read values the frames do not
        // name; an empty slice keeps every node sinkable.
        let mut keep: Vec<bool> = Vec::new();
        if !opts.extra_frame_named.is_empty() {
            keep = vec![false; num_nodes];
            for &v in &opts.extra_frame_named {
                if let Some(k) = keep.get_mut(v as usize) {
                    *k = true;
                }
            }
        }
        let (moved, reverted) =
            sink_pure_nodes(graph, &mut blocks, &mut node_to_block, &dom, &keep);
        if moved > 0 {
            crate::ir_evidence::note(crate::ir_evidence::Transform::SunkLate);
        }
        if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_IR_SINK") {
            // `reverted` counts a read-check revert too since round 11 wave 12
            // (`placement_is_sound`); the key names both.
            eprintln!("[ir-sink] moved={moved} reverted_for_safepoints_or_reads={reverted}");
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
    let frame_named = if opts.priority_within_blocks {
        let mut named = frame_named_values(graph);
        for &v in &opts.extra_frame_named {
            if let Some(slot) = named.get_mut(v as usize) {
                *slot = true;
            }
        }
        named
    } else {
        Vec::new()
    };
    for block in &mut blocks {
        topo_sort_block(graph, &mut block.nodes);
        if opts.priority_within_blocks {
            priority_sort_block(graph, &mut block.nodes, &frame_named);
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
    // but `dom` answers by block number, so recompute it after a permutation
    // rather than renumber it in place. Callers query it with post-layout
    // indices: `Schedule::node_strictly_dominates_block`, the
    // scalar-replacement deopt producer, and `ir_check_elim`.
    let dom = if layout.applied {
        Dominators::compute(&blocks)
    } else {
        dom
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

/// Is `id` the multi-way branch terminator?
///
/// Separate from [`is_if`] rather than folded into it: the two are asked
/// together where "does this node own control edges through projections" is
/// the question, and apart where the two-successor reading of `successors`
/// matters (`true_successor_index`, `static_branch_probs`). Folding them would
/// have silently handed a 12-way switch to code that indexes `successors[1]`.
fn is_switch(graph: &Graph, id: NodeId) -> bool {
    matches!(graph.nodes.get(id as usize), Some(n) if matches!(n.op, Op::Switch { .. }))
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

// The test-only dense-matrix helpers (`compute_dominators`, `dominates`,
// `Dominators::to_matrix`) live in the test module at the end of this file
// (round 11 wave 3): `jit/tests/panic_free_compile_ratchet.rs` stops scanning a
// file at its first test-configuration attribute, and theirs sat here, above
// two thousand lines of production scheduler code the ratchet therefore never
// read.

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
    /// The highest-numbered block the entry does not reach, if any. The sink
    /// pass's answer when every use it has to cover is unreachable; see
    /// `deepest_common_dominator`.
    last_unreached: Option<usize>,
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
            return Dominators {
                idom,
                pre,
                post,
                last_unreached: None,
            };
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

        let last_unreached = (0..n).rev().find(|&b| idom[b] == UNREACHED);
        Dominators {
            idom,
            pre,
            post,
            last_unreached,
        }
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

    /// True for a block the entry reaches; `false` for an unreachable block and
    /// for an out-of-range index.
    pub fn is_reachable(&self, b: usize) -> bool {
        matches!(self.idom.get(b), Some(&d) if d != UNREACHED)
    }

    /// `b`'s number in a preorder walk of the dominator tree: a dominator
    /// always numbers below every block it dominates. `None` for an
    /// unreachable block (which is in no tree) and an out-of-range index.
    fn preorder(&self, b: usize) -> Option<usize> {
        if self.is_reachable(b) {
            self.pre.get(b).copied()
        } else {
            None
        }
    }

    /// The highest-numbered block the entry does not reach, or `None` when
    /// every block is reachable.
    pub fn last_unreached(&self) -> Option<usize> {
        self.last_unreached
    }

    /// True iff block `a` dominates block `b`. Reflexive.
    ///
    /// The same answers the dense matrix this replaced gave: `false` for an
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

    // The deepest already-placed input block, dominated by every other one:
    // it sees all inputs on every path that reaches it. No known input block
    // → free to live in the entry block (params/consts). Round 12 wave 2
    // (lane irback, proposal R12-5): the one-pass walk the verifier's
    // `early_block_of` shares, in place of an all-pairs scan over a `Vec`
    // allocated per node; `deepest_input_block` says why the answer is the
    // same one.
    let nb = blocks.len();
    match deepest_input_block(&node.inputs, |inp| placed_block(node_to_block, nb, inp), dom) {
        Ok(Some((b, _))) => b,
        Ok(None) => 0,
        // Inputs span incomparable branches: no block is dominated by all of
        // them, so NO placement is legal. Well-formed SSA never has this (a
        // value merging two arms reads them through a phi); reaching it means
        // an upstream pass built a broken graph.
        //
        // This is NOT a conservative placement. The entry block runs BEFORE
        // the inputs' blocks, so a node here reads inputs nothing has computed
        // yet -- a use before its def. It is kept deliberately because it is
        // the one choice the emission-order check is guaranteed to catch:
        // `blocks[0]` is emitted first in every layout (invariant 3 above) and
        // no input of this node lives in block 0 (block 0 dominates every
        // reachable block, so it would have been comparable), so
        // `ir_lower::verify_data_locations` always sees a def after its use and
        // refuses the compile. Placing it in the deepest input block instead,
        // as a "total" answer, can pass that index-order check -- the other
        // input's arm may well have the lower index -- and emit a read of a
        // value that arm's path never wrote.
        //
        // The failure is counted ([`ir_incomparable_input_placements`]) and
        // attributable ([`Schedule::misplaced_data_nodes`] names the node), so
        // a lost compile is not silent any more.
        Err(_) => {
            IR_INCOMPARABLE_INPUT_PLACEMENTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            0
        }
    }
}

/// Schedule-early's one question: the DEEPEST block among `inputs`' placed
/// blocks, which every other one dominates, with the input that lives there.
/// Shared by [`find_best_block`] (the scheduler) and [`early_block_of`] (the
/// verifier's dominance lane), so the lane checks the placement the scheduler
/// makes rather than a second code's idea of it (round 12 wave 2, lane irback,
/// proposal R12-5).
///
/// `Ok(None)` when no input is placed. `Err((a, b))` names two inputs whose
/// blocks are incomparable in the dominator tree, which proves that no block is
/// dominated by all of them.
///
/// One pass and no allocation, where `find_best_block` used to collect the
/// distinct blocks into a `Vec` and test every candidate against every other.
/// Same answer, in two cases:
///
/// * **Every block reachable.** Dominance is then a partial order. The running
///   answer is dominated by every input seen so far (it moves only to a block
///   the old answer dominates, and dominance is transitive), so at the end it
///   is dominated by all of them — the one block the all-pairs scan accepts,
///   unique in a chain. When no such block exists, some input is incomparable
///   with the running answer, and the walk reports it.
/// * **Some block unreachable.** [`Dominators::dominates`] reports an
///   unreachable block as dominated by every block, so the all-pairs scan
///   accepted the FIRST input (in input order) whose block is unreachable,
///   whatever else the inputs were. Answered first, the same way.
fn deepest_input_block(
    inputs: &[NodeId],
    block_of: impl Fn(NodeId) -> Option<usize>,
    dom: &Dominators,
) -> Result<Option<(usize, NodeId)>, (NodeId, NodeId)> {
    let unreached = inputs.iter().find_map(|&inp| {
        block_of(inp)
            .filter(|&b| dom.idom.get(b) == Some(&UNREACHED))
            .map(|b| (b, inp))
    });
    if unreached.is_some() {
        return Ok(unreached);
    }
    let mut deepest: Option<(usize, NodeId)> = None;
    for &inp in inputs {
        let Some(ib) = block_of(inp) else {
            continue;
        };
        match deepest {
            None => deepest = Some((ib, inp)),
            Some((db, dn)) => {
                if ib == db || dom.dominates(ib, db) {
                    continue;
                }
                if dom.dominates(db, ib) {
                    deepest = Some((ib, inp));
                } else {
                    return Err((dn, inp));
                }
            }
        }
    }
    Ok(deepest)
}

/// How many data nodes [`find_best_block`] could not place legally because
/// their inputs sit in blocks no single block is dominated by, process-wide.
/// Every one of them is a broken graph from an upstream pass and costs the
/// method its optimizing-tier compile (`ir_lower::verify_data_locations`
/// refuses it). Should read zero forever.
static IR_INCOMPARABLE_INPUT_PLACEMENTS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Read [`IR_INCOMPARABLE_INPUT_PLACEMENTS`].
pub fn ir_incomparable_input_placements() -> u64 {
    IR_INCOMPARABLE_INPUT_PLACEMENTS.load(std::sync::atomic::Ordering::Relaxed)
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
            // A phi's value inputs are read on its incoming EDGES
            // (`ir_lower::emit_phi_copies`), not at the phi, so they are not
            // dependences here — the attribution `priority_sort_block`,
            // `regalloc::build_live_model` and `verify_data_locations` all make.
            // Descending into them put a loop-carried value ahead of the phi it
            // is computed from: a single-block loop body `[phi, next = phi + 1]`
            // came out `[next, phi]`, a use before its def in every position
            // model that does not special-case phis.
            let inputs: &[NodeId] = match graph.nodes.get(id as usize) {
                Some(n) if !matches!(n.op, Op::Phi) => &n.inputs[..],
                _ => &[],
            };
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

/// Why [`priority_sort_block`] did or did not reorder a block.
///
/// The same instrument [`PairCensus`] is, for the same reason: that pass had
/// five silent `continue`s and the note recording it said the census was "the
/// next thing to build, not the next thing to fix". This one has six silent
/// `return`s, and two of them are claims about the pass's own model rather
/// than about a block:
///
/// * [`Self::cycle`] — the ordering DAG left a node unscheduled. That is
///   supposed to be impossible: `deps` is value dependences plus a side-effect
///   chain laid down in the incoming order, and the incoming order is already
///   a valid topological order, so the DAG has a topological order by
///   construction. A non-zero count here means the construction argument is
///   wrong, which is a correctness question and not a tuning one.
/// * [`Self::memory_order`] — [`check_memory_order`] refused the permutation.
///   The section header says the side-effect chain is *supposed* to make that
///   validator vacuous, because every impure node is wired into a total order
///   and only pure nodes float. A non-zero count means the chain is not total
///   after all.
///
/// Both read zero today. The point of counting them is that "reads zero" and
/// "nobody is looking" are different states, and the second is what
/// `NOTES-misc.md` §12 is really about: the scheduler's dependence model is a
/// total chain standing in for anti- and output-dependences, and the case for
/// replacing it with real edges has to start from a measurement of what the
/// stand-in is currently costing and whether it is even holding.
///
/// # The identity
///
/// `too_small + over_budget + duplicate_node + missing_node + cycle +
/// memory_order + unchanged + reordered == blocks`, checked by
/// [`Self::closes`]. `blocks` is bumped on ENTRY to the pass and the buckets
/// at each exit ([`note_priority_block`] says why the two are separate calls),
/// so a `return` added to the pass and not to the census leaves the identity
/// open rather than quietly reading low.
///
/// Unlike [`PairCensus`] there is no per-method publication site to
/// `debug_assert` it at — the counters are global and written one block at a
/// time — so the identity is asserted on constructed values by
/// `the_priority_census_identity_catches_an_uncounted_exit` and read off the
/// global table by hand. A live snapshot can catch a thread mid-block, between
/// the entry bump and the exit bump, so `closes()` is deliberately NOT
/// asserted against the process-wide totals.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PriorityCensus {
    /// Blocks offered to the pass. The denominator.
    pub blocks: usize,
    /// Two nodes or fewer: no ordering choice exists.
    pub too_small: usize,
    /// Above [`PRIORITY_BLOCK_BUDGET`]; the selection loop is quadratic.
    pub over_budget: usize,
    /// The block listed one node twice. Structural; expected to be zero.
    pub duplicate_node: usize,
    /// A node id in the block has no node behind it. Structural; expected to
    /// be zero.
    pub missing_node: usize,
    /// The ordering DAG left a node unscheduled — see the type doc. **A
    /// non-zero count is a defect in the DAG construction, not a refusal.**
    pub cycle: usize,
    /// [`check_memory_order`] refused the permutation — see the type doc. **A
    /// non-zero count means the side-effect chain is not total.**
    pub memory_order: usize,
    /// The pass ran and produced the order it was given.
    pub unchanged: usize,
    /// The pass ran and moved at least one node.
    pub reordered: usize,
}

impl PriorityCensus {
    /// Does every block fall into exactly one bucket?
    pub fn closes(&self) -> bool {
        self.too_small
            + self.over_budget
            + self.duplicate_node
            + self.missing_node
            + self.cycle
            + self.memory_order
            + self.unchanged
            + self.reordered
            == self.blocks
    }

    /// Are both model-invariant buckets empty?
    ///
    /// The question worth asking of a whole run: did the ordering DAG always
    /// have a topological order, and was the memory-order validator always
    /// vacuous? Those are the two properties the pass's comments assert and
    /// nothing checked.
    pub fn model_held(&self) -> bool {
        self.cycle == 0 && self.memory_order == 0
    }
}

/// Process-wide priority-scheduling totals, in [`PriorityCensus`] field order.
///
/// Unconditional and cheap — one relaxed add per block, on a path that already
/// allocates several vectors per block. A census you have to switch on is one
/// nobody has for the run they already did.
static IR_PRIORITY_CENSUS: [std::sync::atomic::AtomicU64; 9] = [
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

/// Which [`PriorityCensus`] bucket a block landed in, as an index into
/// [`IR_PRIORITY_CENSUS`]. Slot 0 is the denominator and is always bumped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PriorityOutcome {
    TooSmall = 1,
    OverBudget = 2,
    DuplicateNode = 3,
    MissingNode = 4,
    Cycle = 5,
    MemoryOrder = 6,
    Unchanged = 7,
    Reordered = 8,
}

/// Count one block OFFERED to the pass — the denominator, bumped on entry.
///
/// Deliberately a separate call from [`note_priority_outcome`], and this is
/// the whole of why [`PriorityCensus::closes`] can catch anything. Bumping the
/// denominator and the bucket together would make the identity hold by
/// construction: an exit that forgot to record would simply not be counted at
/// all, on either side, and the census would close while under-reporting. With
/// the denominator taken on entry, a `return` that records nothing leaves
/// `blocks` ahead of the bucket sum and `closes()` answers `false`.
fn note_priority_block() {
    use std::sync::atomic::Ordering::Relaxed;
    IR_PRIORITY_CENSUS[0].fetch_add(1, Relaxed);
}

/// Record one block's disposition. Every exit from [`priority_sort_block`]
/// goes through this; see [`note_priority_block`] for why the denominator
/// does not.
fn note_priority_outcome(outcome: PriorityOutcome) {
    use std::sync::atomic::Ordering::Relaxed;
    IR_PRIORITY_CENSUS[outcome as usize].fetch_add(1, Relaxed);
}

/// The process-wide priority-scheduling census since process start.
pub fn ir_priority_census() -> PriorityCensus {
    use std::sync::atomic::Ordering::Relaxed;
    let v: Vec<usize> = IR_PRIORITY_CENSUS
        .iter()
        .map(|a| a.load(Relaxed) as usize)
        .collect();
    PriorityCensus {
        blocks: v[0],
        too_small: v[1],
        over_budget: v[2],
        duplicate_node: v[3],
        missing_node: v[4],
        cycle: v[5],
        memory_order: v[6],
        unchanged: v[7],
        reordered: v[8],
    }
}

/// Blocks larger than this keep the plain dependence order.
///
/// The selection loop rescans the ready set on every pick, so it is quadratic
/// in the block's node count. A straight-line method can put thousands of nodes
/// in one block (the deep-operand-chain case `topo_sort_block` exists for), and
/// a compiler must not turn a pathological graph into a pathological *compile*
/// — the same rule `ir_lower::SLOT_PLAN_WORK_BUDGET` states for liveness.
const PRIORITY_BLOCK_BUDGET: usize = 4_000;

/// Result latency of one node, in cycles, for the critical-path term of
/// [`priority_sort_block`].
///
/// # What this is and what it is not
///
/// It is a *relative* cost used to rank two ready nodes against each other. It
/// is **not** a machine model: there is no issue width here, no port
/// assignment, no reservation station, and no memory hierarchy — a scheduler
/// that claimed those would have to be re-derived per microarchitecture and
/// re-validated per release, and this pass only ever needs to answer "which of
/// these two should go first". The numbers below are the result latencies of
/// the x86-64 instruction each op lowers to on a Skylake-class core (Agner Fog
/// tables 2024; the equivalent Zen 3/4 numbers differ by at most two cycles
/// for every entry, and never in a way that reorders the buckets). AArch64
/// Neoverse latencies land in the same order — the relative ranking, which is
/// all this is used for, is stable across both.
///
/// The previous model was `1` for every node. That is fine for a chain of
/// `add`s and wrong by an order of magnitude the moment a division or a load
/// is in the block, which is exactly when intra-block scheduling has something
/// to win.
///
/// # Why an unknown op gets a small number and not a large one
///
/// Anything not named below — every impure node, so `Op::Call`, `Op::Store`,
/// the allocations, `Op::Guard` — is already wired into a total side-effect
/// chain by `priority_sort_block` and cannot move relative to another impure
/// node whatever this says. Its latency only affects the height of the PURE
/// nodes feeding it. Answering "1" there keeps the heights of pure chains
/// comparable with each other instead of letting one call inflate every chain
/// that happens to end at it; a large default would make every operand of a
/// call outrank every other node in the block, which is a priority inversion
/// dressed up as a cost model.
fn op_latency(op: &Op, ty: IrType) -> usize {
    let fp = matches!(ty, IrType::Float | IrType::Double);
    match op {
        // `divsd` 13-14, `divss` 11; 32-bit `idiv` 26, 64-bit `idiv` 36-95.
        // The integer numbers are a range because the divider is
        // data-dependent; 26 is the low end, so this under-states rather than
        // over-states a cost the scheduler would otherwise chase.
        Op::Div | Op::Rem if fp => 14,
        Op::Div | Op::Rem => 26,
        // `imul r64, r64` is 3; `mulsd`/`mulss` are 4.
        Op::Mul if fp => 4,
        Op::Mul => 3,
        // `addsd`/`subsd` 4. The integer forms are the 1-cycle default below.
        Op::Add | Op::Sub | Op::Neg if fp => 4,
        // An L1 hit. A miss is 12 (L2) to ~200 (DRAM) and is not predictable
        // here; using the hit latency keeps a load ahead of arithmetic without
        // letting it dominate the whole block.
        Op::Load(_) | Op::ArrayLoad(_) | Op::LoadStatic { .. } => 5,
        // `cvtsi2sd` / `cvttsd2si` and friends cross the integer/SSE domain.
        Op::I2F | Op::I2D | Op::L2F | Op::L2D | Op::F2I | Op::F2L | Op::D2I | Op::D2L => 6,
        // `cvtss2sd` / `cvtsd2ss` stay inside the SSE domain.
        Op::F2D | Op::D2F => 4,
        // A three-way compare is a compare plus two conditional materialisations
        // (`ucomisd` + `setcc`/`cmov`), not one instruction.
        Op::LCmp | Op::FCmp { .. } => 3,
        // Everything else: a single-cycle ALU op, or an impure node whose
        // position is fixed by the side-effect chain anyway. See the header.
        _ => 1,
    }
}

/// Re-order the data nodes of one block by scheduling priority, refining the
/// dependence order [`topo_sort_block`] has already produced.
///
/// Two things drive the priority, in this order:
///
/// * **Register pressure.** A node whose operands die at it *frees* frame slots;
///   a node that only defines a value *costs* one. Preferring the negative-delta
///   candidates keeps peak liveness — the number `ir_lower::plan_slots` colours
///   and `estimate_frame_bytes` budgets — down.
/// * **Critical path.** Among candidates with equal pressure delta, the one
///   whose remaining dependence chain is longest *in cycles* goes first, so the
///   chain that determines the block's length starts as early as possible.
///   Chain length is summed from [`op_latency`] rather than counted in nodes:
///   an `fdiv` is fourteen cycles and an `and` is one, and a model that calls
///   them equal ranks a three-`and` chain above a `divsd`.
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
/// entry, a dangling id, a dependence cycle, or a block over budget). Every
/// one of those exits is counted — see [`PriorityCensus`].
///
/// # The dependence model, stated plainly
///
/// `NOTES-misc.md` §12 records that "the scheduler has no dependence graph" is
/// half true, and it is worth having the accurate half written where the code
/// is. What this pass builds IS a real per-block ordering DAG: `val_deps` are
/// true (read-after-write) dependences on the block's own definitions,
/// `users` is the forward edge set, `height` is a latency-weighted critical
/// path and `ready` is a worklist. What it does not build:
///
/// * **Anti-dependences (write-after-read) and output dependences
///   (write-after-write) are not represented, and are not needed, because the
///   total side-effect chain subsumes them.** Every impure node is ordered
///   against every other impure node, so the pairs those edge kinds would
///   constrain cannot move relative to each other at all. The chain is a
///   conservative STAND-IN for the two edge kinds, not an absence of them —
///   and it is why this pass can be sound without the IR representing either.
///
///   Replacing the chain with real edges is the change §12 describes, and it
///   is the one a partial implementation turns into a wrong-code bug: an
///   anti-dependence the edge builder misses is two memory operations the
///   scheduler is then free to swap, with `check_memory_order` as the only
///   thing left between that and the emitted order. It is not attempted here.
///   [`PriorityCensus::memory_order`] and [`PriorityCensus::cycle`] are what
///   would justify attempting it: they measure whether the stand-in is
///   holding, which nothing did before.
/// * **Anything across blocks.** Placement is "the deepest block dominated by
///   all inputs" plus the late sink. There is no global list scheduling, no
///   software pipelining and no cross-block motion of an impure node.
/// * **A machine model.** [`op_latency`] is a relative ranking and says so. A
///   real one is an issue-width / port / reservation-station table that has to
///   be re-validated per microarchitecture, and it should not be started until
///   something measures that the relative ranking is what is limiting.
///
/// `named` is [`frame_named_values`] for this graph (see the round-9 section
/// below).
fn priority_sort_block(graph: &Graph, nodes: &mut Vec<NodeId>, named: &[bool]) {
    priority_sort_block_inner(graph, nodes, named);
}

/// `named[id]` is true when some frame state in `graph.safepoints` names node
/// `id` — as a local, an operand-stack entry or a monitor.
///
/// Computed once per method by `schedule_with_options`, which adds
/// [`ScheduleOptions::extra_frame_named`], and handed to every block's
/// [`priority_sort_block`].
fn frame_named_values(graph: &Graph) -> Vec<bool> {
    let mut named = vec![false; graph.nodes.len()];
    for sp in &graph.safepoints {
        for &v in sp
            .locals
            .iter()
            .chain(sp.stack.iter())
            .chain(sp.monitors.iter())
        {
            if v == NO_NODE {
                continue;
            }
            if let Some(slot) = named.get_mut(v as usize) {
                *slot = true;
            }
        }
    }
    named
}

/// The body of [`priority_sort_block`].
///
/// # A named value is not delayed past a deopt point (round 9)
///
/// The pass floats pure nodes and chains impure ones, and the chain is what it
/// relied on for "no side effect, deopt point or potentially-throwing operation
/// changes position relative to another". That protects the ANCHORS from each
/// other. It does not protect a VALUE a frame at one of those anchors names: a
/// pure `t = a + b` ahead of a call that deopts with `t` in its frame was free
/// to be scheduled after the call whenever the call headed the longer chain —
/// which, since an impure node's height includes every impure node after it,
/// it usually does. A deopt at the call then rebuilt the interpreter frame from
/// `t`'s home word before anything had written it. `pair_single_use_operands`
/// refuses exactly this crossing (`PairCensus::deopt_between`); this pass had
/// no such rule.
///
/// So every pure node a frame names gets an edge to the next node after it (in
/// the incoming order) that can deopt. Every such node is impure and therefore
/// in the side-effect chain, so one edge orders it before every later anchor
/// too. The edges point forward in the incoming order, which is a topological
/// order of the DAG, so they cannot create a cycle. A pure node may still move
/// EARLIER past an anchor — computing a value sooner writes its home sooner,
/// which no frame can object to.
fn priority_sort_block_inner(graph: &Graph, nodes: &mut Vec<NodeId>, named: &[bool]) {
    // The denominator, taken before any exit can be reached.
    note_priority_block();
    let count = nodes.len();
    if count <= 2 {
        note_priority_outcome(PriorityOutcome::TooSmall);
        return;
    }
    if count > PRIORITY_BLOCK_BUDGET {
        note_priority_outcome(PriorityOutcome::OverBudget);
        return;
    }
    // `nodes` is already a valid topological order on entry.
    let baseline: Vec<NodeId> = nodes.clone();

    let mut idx_of: HashMap<NodeId, usize> = HashMap::with_capacity(count);
    for (i, &id) in baseline.iter().enumerate() {
        idx_of.insert(id, i);
    }
    if idx_of.len() != count {
        // Duplicate entry — not a shape this pass reasons about.
        note_priority_outcome(PriorityOutcome::DuplicateNode);
        return;
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
            None => {
                note_priority_outcome(PriorityOutcome::MissingNode);
                return;
            }
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
    // Frame-named pure values stay ahead of the next deopt point. See the doc
    // comment on this function.
    let mut pending: Vec<usize> = Vec::new();
    for i in 0..count {
        let Some(node) = graph.nodes.get(baseline[i] as usize) else {
            continue;
        };
        if node.op.is_pure() {
            if named.get(baseline[i] as usize).copied().unwrap_or(false) {
                pending.push(i);
            }
            continue;
        }
        if !crate::ir_lower::op_cannot_deopt(&node.op) {
            for &p in &pending {
                if !deps[i].contains(&p) {
                    deps[i].push(p);
                    users[p].push(i);
                }
            }
            pending.clear();
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

    // Critical-path height: the longest chain of dependent nodes still ahead,
    // measured in CYCLES rather than in nodes.
    //
    // This used to add one per edge, which is a latency model that says an
    // `fdiv` and an `and` take the same time. On the two shapes the priority
    // scheduler exists for that is not a rounding error: a 14-cycle `divsd` at
    // the head of a two-node chain loses to a 1-cycle `xor` at the head of a
    // three-node one, and the machine then waits for the division it could have
    // started first. `op_latency` states each node's cost and where the number
    // comes from; a leaf's height is its own latency, not 1.
    let mut height = vec![0usize; count];
    for &i in seed.iter().rev() {
        let own = graph
            .nodes
            .get(baseline[i] as usize)
            .map_or(1, |node| op_latency(&node.op, node.ty));
        let mut h = 0usize;
        for &u in &users[i] {
            h = h.max(height[u]);
        }
        height[i] = h.saturating_add(own);
    }

    let mut remaining_deps: Vec<usize> = deps.iter().map(|d| d.len()).collect();
    let mut remaining_uses: Vec<usize> = val_use_count.clone();
    let mut out: Vec<NodeId> = Vec::with_capacity(count);

    // The pick: the ready node with the smallest key
    // `(1 - kills, usize::MAX - height, seed_rank)`, where `kills` counts the
    // operands whose *last* remaining consumer is this node (they die here),
    // and `usize::MAX - height` turns "prefer the longest chain" into a
    // minimisation, so the whole key is compared in one direction.
    // `seed_rank` is unique, so the pick is a total order.
    //
    // Round 11 wave 10 (irback proposal W9-4): the ready set used to be
    // re-scored in full at every step, a scan of each ready node's operands,
    // O(count × ready × deps) on blocks of up to `PRIORITY_BLOCK_BUDGET`
    // nodes. `kills` is now kept per node. An operand's remaining-use count
    // only falls, and it reaches 1 exactly once; at that step its one
    // unscheduled consumer gains a kill. A key therefore only ever
    // decreases, so the ready set is a min-heap that takes a fresh entry
    // whenever a ready node's key drops. An entry whose `kills` is no longer
    // current is stale and skipped. Same picks, same order.
    let mut val_users: Vec<Vec<usize>> = vec![Vec::new(); count];
    for (i, d) in val_deps.iter().enumerate() {
        for &j in d {
            val_users[j].push(i);
        }
    }
    let mut kills: Vec<i64> = val_deps
        .iter()
        .map(|d| d.iter().filter(|&&j| val_use_count[j] == 1).count() as i64)
        .collect();
    let key_of = |i: usize, kills: &[i64]| -> (i64, usize, usize, usize) {
        (1i64 - kills[i], usize::MAX - height[i], seed_rank[i], i)
    };
    let mut scheduled = vec![false; count];
    let mut ready: std::collections::BinaryHeap<std::cmp::Reverse<(i64, usize, usize, usize)>> = (0
        ..count)
        .filter(|&i| remaining_deps[i] == 0)
        .map(|i| std::cmp::Reverse(key_of(i, &kills)))
        .collect();

    while let Some(std::cmp::Reverse((k, _, _, i))) = ready.pop() {
        if scheduled[i] || k != 1i64 - kills[i] {
            continue;
        }
        scheduled[i] = true;
        out.push(baseline[i]);
        for &j in &val_deps[i] {
            if remaining_uses[j] > 0 {
                remaining_uses[j] -= 1;
                if remaining_uses[j] == 1 {
                    if let Some(&u) = val_users[j].iter().find(|&&u| !scheduled[u]) {
                        kills[u] += 1;
                        if remaining_deps[u] == 0 {
                            ready.push(std::cmp::Reverse(key_of(u, &kills)));
                        }
                    }
                }
            }
        }
        for &u in &users[i] {
            if remaining_deps[u] > 0 {
                remaining_deps[u] -= 1;
                if remaining_deps[u] == 0 {
                    ready.push(std::cmp::Reverse(key_of(u, &kills)));
                }
            }
        }
    }

    // A cycle would leave nodes unscheduled. The plain topological order is
    // always valid, so keep it rather than emitting a partial block.
    //
    // Counted, and counted as a DEFECT rather than a refusal: `deps` is the
    // value dependences plus a side-effect chain laid down in the incoming
    // order, and the incoming order is already a valid topological order, so
    // the DAG has a topological order by construction. A non-zero
    // `PriorityCensus::cycle` says that argument is wrong somewhere, which is
    // not something to discover from a benchmark that got slower.
    if out.len() != count {
        note_priority_outcome(PriorityOutcome::Cycle);
        return;
    }
    // The memory-ordering rule has the last word. The side-effect chain above
    // is *supposed* to make this vacuous — every impure node is wired into a
    // total order — but "supposed to" is what a validator is for: this pass is
    // the only thing between the memory model and the emitted instruction
    // order, and a reordering it cannot justify must not leave the function.
    if check_memory_order(graph, &baseline, &out).is_err() {
        // Also counted as a defect, not a refusal, and for the same kind of
        // reason. This validator is *supposed* to be vacuous for this pass —
        // only pure nodes float, every impure node is in the total chain — so
        // a non-zero `PriorityCensus::memory_order` says the chain is not
        // total. That is the question `NOTES-misc.md` §12 is actually about:
        // the chain is standing in for the anti- and output-dependences the IR
        // does not represent, and any case for replacing it with real edges
        // has to begin by knowing whether the stand-in is holding.
        note_priority_outcome(PriorityOutcome::MemoryOrder);
        return;
    }
    note_priority_outcome(if out == baseline {
        PriorityOutcome::Unchanged
    } else {
        PriorityOutcome::Reordered
    });
    *nodes = out;
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
        // `dominates(header, latch)` is vacuously true for an unreachable
        // latch, which would make every edge from dead code into live code a
        // "back edge" — and multiply the live block it lands on by a trip
        // count. Only a reachable latch closes a loop.
        if !dom.is_reachable(latch) {
            continue;
        }
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
    //
    // Each pull needs the index of the edge `p -> b` in `p`'s successors. A
    // `position` scan is free for an `If`, but a `Switch` with S cases paid it
    // once per case block: O(S²), and S reaches `ir::SWITCH_MAX_PROJECTIONS`.
    // A block wider than `SCAN_WIDTH` answers from a map instead, built only
    // when such a block exists; `or_insert` keeps the FIRST index, which is
    // what `position` answers, so the frequencies are bit-identical (round 11
    // wave 10, irback proposal W9-3).
    const SCAN_WIDTH: usize = 8;
    let mut wide_edge: HashMap<(usize, usize), usize> = HashMap::new();
    for (p, blk) in blocks.iter().enumerate() {
        if blk.successors.len() > SCAN_WIDTH {
            for (i, &s) in blk.successors.iter().enumerate() {
                wide_edge.entry((p, s)).or_insert(i);
            }
        }
    }
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
            let succs = &blocks[p].successors;
            let edge = if succs.len() > SCAN_WIDTH {
                wide_edge.get(&(p, b)).copied()
            } else {
                succs.iter().position(|&s| s == b)
            };
            let Some(i) = edge else {
                continue;
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
    // Blocks below `scan` are all visited, so the search for the next
    // unvisited root resumes there instead of restarting at 0 — the restart
    // made a method with many unreachable blocks quadratic in the block count.
    let mut scan = 0usize;
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
        while scan < n && state[scan] != UNVISITED {
            scan += 1;
        }
        next_root = (scan < n).then_some(scan);
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

    impl Dominators {
        /// The relation as a dense matrix: `m[b][d]` iff `d` dominates `b`.
        /// O(B²) memory, filled by walking each block's `idom` chain. It is
        /// what `Schedule::dom` published before the schedule kept the
        /// [`Dominators`] itself, and these tests compare it against the dense
        /// fixpoint reference.
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

    /// For every block, the set of blocks that dominate it, as a dense matrix:
    /// `dom[b][d]` iff block `d` dominates block `b`. A block the entry does
    /// not reach gets the row "all blocks" — the value the classic iterative
    /// fixpoint this replaced left it at.
    fn compute_dominators(blocks: &[Block]) -> Vec<Vec<bool>> {
        Dominators::compute(blocks).to_matrix()
    }

    /// True iff block `a` dominates block `b`, read from a
    /// `compute_dominators` matrix; `false` for an out-of-range index.
    fn dominates(matrix: &[Vec<bool>], a: usize, b: usize) -> bool {
        matrix
            .get(b)
            .and_then(|row| row.get(a))
            .copied()
            .unwrap_or(false)
    }

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

    /// Round 11 wave 12 (irback W11-2): the φ-edge switch is read live too.
    #[test]
    fn phi_edge_flag_is_read_live() {
        let read = |v: Option<&str>| {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_JIT_IR_SINK_PHI_EDGE", v)],
                sink_phi_edge_enabled,
            )
        };
        assert!(read(Some("1")));
        assert!(!read(Some("0")));
        assert!(!read(None), "default is OFF");
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
        let mut graph = bare_graph();
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
        let mut graph = bare_graph();
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

    /// `Graph::empty` with no entry either: the one place this module's tests
    /// build a graph, so a change to `Graph`'s fields is a change here alone
    /// (`r11-irback-use-list-invariant-enforced-by-convention-only`, step 2).
    fn bare_graph() -> Graph {
        let mut g = Graph::empty(0);
        g.entry = NO_NODE;
        g
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

    /// Build the three-node window the pairing pass acts on: a producer the
    /// carry certifies, one node in between, and the single consumer.
    ///
    /// Returns `(nodes, census, producer, consumer)` after one direct run of
    /// the pass, so a test can assert on both the order and on WHY the pass
    /// decided as it did.
    fn run_pairing_over(f: &mut MemGraph, middle: Id) -> (Vec<Id>, PairCensus, Id, Id) {
        // `ArrayLength` is the shortest producer that is BOTH in
        // `op_home_is_one_store_rax` (so the pass will consider moving it) and
        // memory-ordering (a `LengthRead`), which is the combination the
        // memory-order check exists for.
        let prod = f.graph.add(
            Op::ArrayLength,
            IrType::Int,
            vec![f.ctrl, f.mem, f.base],
            None,
        );
        // One input, so exactly one `(consumer, operand)` candidate.
        let cons = f.graph.add(Op::Neg, IrType::Int, vec![prod], None);
        let mut nodes = vec![prod, middle, cons];
        let uses = use_counts(&f.graph);
        let mut census = PairCensus::default();
        pair_single_use_operands(&f.graph, &uses, &mut nodes, &mut census);
        (nodes, census, prod, cons)
    }

    /// The pass still does its job when what it crosses is genuinely pure.
    /// The memory-order check is a screen, not a brake.
    #[test]
    fn pairing_sinks_a_producer_past_a_pure_node() {
        let mut f = mem_graph();
        let konst = f.graph.add(Op::Const(9), IrType::Int, vec![], None);
        let (nodes, census, prod, cons) = run_pairing_over(&mut f, konst);

        assert_eq!(census.paired, 1, "the pure window must still pair");
        assert_eq!(census.memory_order, 0);
        assert_eq!(
            nodes,
            vec![konst, prod, cons],
            "the producer sank past the pure node to sit beside its consumer"
        );
        assert!(census.closes(), "every candidate landed in a bucket");
    }

    /// The defect the step-5b memory-order check exists to catch.
    ///
    /// A memory-typed φ carries `MemEffect::OPAQUE` — it reads and writes
    /// anything — and `ir_lower::op_cannot_deopt` accepts `Op::Phi`, because a
    /// deopt cannot arrive AT a φ. So the pass's only screen says "cross it"
    /// while the memory model says "you just moved a read past an opaque
    /// write". That is not a hypothetical intersection of the two allowlists:
    /// `Op::Phi`, `Op::Return` and `Op::Dead` are all in `op_cannot_deopt` and
    /// all carry `OPAQUE`.
    ///
    /// Before the check, step 5b was the one reordering in the scheduler
    /// `check_memory_order` never saw — it runs after the single validation
    /// call — so this move happened silently.
    #[test]
    fn pairing_refuses_to_sink_a_memory_read_past_an_opaque_node() {
        let mut f = mem_graph();
        // A memory-typed φ: `effect_of_node` gives it `MemEffect::OPAQUE`,
        // and `op_cannot_deopt` accepts `Op::Phi`.
        let phi = f.graph.add(Op::Phi, IrType::Memory, vec![f.mem], None);
        let (nodes, census, prod, cons) = run_pairing_over(&mut f, phi);

        assert_eq!(
            census.memory_order, 1,
            "the memory model must be the one to refuse this, not the deopt screen"
        );
        assert_eq!(
            census.deopt_between, 0,
            "the deopt screen ALLOWS crossing a phi — that is the whole point"
        );
        assert_eq!(census.paired, 0);
        assert_eq!(
            nodes,
            vec![prod, phi, cons],
            "nothing moved: the order is unchanged"
        );
        assert!(census.closes(), "every candidate landed in a bucket");
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

    /// The latency table must rank the buckets it exists to distinguish.
    ///
    /// Absolute cycle counts are a microarchitecture's business and this pass
    /// only ever compares two of them, so what is pinned here is the ORDER —
    /// which is the part that has to stay true across Skylake, Zen and
    /// Neoverse. A table that got the order wrong would still "work": the
    /// scheduler would simply prefer the cheaper node, silently, on every
    /// method.
    #[test]
    fn the_latency_table_ranks_a_divide_above_a_multiply_above_an_add() {
        let int_add = op_latency(&Op::Add, IrType::Int);
        let int_mul = op_latency(&Op::Mul, IrType::Int);
        let int_div = op_latency(&Op::Div, IrType::Int);
        assert!(int_add < int_mul && int_mul < int_div, "integer ladder");

        let fp_add = op_latency(&Op::Add, IrType::Double);
        let fp_mul = op_latency(&Op::Mul, IrType::Double);
        let fp_div = op_latency(&Op::Div, IrType::Double);
        assert_eq!(fp_add, fp_mul, "addsd and mulsd are both four cycles");
        assert!(fp_mul < fp_div, "divsd is several times mulsd");

        // The same opcode costs different amounts at different types: an
        // integer add is one cycle and `addsd` is four. A table keyed on the
        // opcode alone cannot say that, which is why `op_latency` takes a type.
        assert!(int_add < fp_add, "the type has to reach the table");

        // A load is slower than arithmetic but must not be treated as a
        // cache miss — the header says why an L1 hit is the number to use.
        let load = op_latency(&Op::Load(crate::ir::MemKind::Int), IrType::Int);
        assert!(load > int_add && load < int_div);

        // An unnamed op answers the small default, not a large one. See the
        // header: an impure node cannot move anyway, and a large default would
        // make every operand of a call outrank everything else in the block.
        assert_eq!(op_latency(&Op::Return, IrType::Void), 1);
    }

    /// The critical path is measured in cycles, so the slower of two equally
    /// ready nodes goes first.
    ///
    /// `int f(int a, int b) { return (a + b) ^ (a * b); }`. Once both
    /// parameters are emitted the add and the multiply are both ready, neither
    /// kills an operand (each parameter still has the other consumer alive), so
    /// the pressure term ties and the critical-path term decides. Both feed the
    /// same `Xor`, so under the old count-the-nodes model their heights were
    /// equal too and the tie fell through to seed order — which is bytecode
    /// order, and the add is written first. The multiply is three cycles and
    /// the add is one, so a cycle-counting model has to put the multiply first.
    ///
    /// The priority-OFF arm is the anti-vacuity guard: it pins that the plain
    /// dependence order really does emit the add first, so the ON arm is
    /// asserting a change and not an accident of node numbering.
    #[test]
    fn the_priority_scheduler_starts_the_slower_of_two_ready_nodes_first() {
        // 0: iload_0  1: iload_1  2: iadd
        // 3: iload_0  4: iload_1  5: imul
        // 6: ixor     7: ireturn
        let code = [0x1au8, 0x1b, 0x60, 0x1a, 0x1b, 0x68, 0x82, 0xac, 0, 0];
        let builder = IrBuilder::new(2, 2);
        let mut graph = builder.build(&code, 8).expect("build failed");
        ir_optimize::optimize(&mut graph);

        let add = graph
            .nodes
            .iter()
            .position(|n| n.op == Op::Add)
            .expect("the method computes a + b") as NodeId;
        let mul = graph
            .nodes
            .iter()
            .position(|n| n.op == Op::Mul)
            .expect("the method computes a * b") as NodeId;

        let order_in = |sched: &Schedule| -> (usize, usize) {
            let nodes = sched
                .blocks
                .iter()
                .flat_map(|b| b.nodes.iter().copied())
                .collect::<Vec<_>>();
            let at = |id: NodeId| nodes.iter().position(|&x| x == id).expect("node scheduled");
            (at(add), at(mul))
        };

        let (add_off, mul_off) =
            order_in(&schedule_with_options(&graph, &ScheduleOptions::default()));
        assert!(
            add_off < mul_off,
            "the plain dependence order emits the add first — without that this \
             test cannot tell a reordering from the status quo"
        );

        let (add_on, mul_on) = order_in(&schedule_with_options(
            &graph,
            &ScheduleOptions {
                priority_within_blocks: true,
                ..ScheduleOptions::default()
            },
        ));
        assert!(
            mul_on < add_on,
            "the three-cycle multiply must start before the one-cycle add"
        );
    }

    /// Every block offered to the priority scheduler lands in exactly one
    /// census bucket, and neither model-invariant bucket is ever hit.
    ///
    /// Two claims, and the second is the one worth having. `cycle` says the
    /// ordering DAG had no topological order, which cannot happen if the
    /// construction argument holds (value dependences plus a side-effect chain
    /// laid down in an order that is already topological). `memory_order` says
    /// `check_memory_order` refused the permutation, which cannot happen if the
    /// chain really is total over the impure nodes. Both are asserted against
    /// the process-wide counters rather than a reset snapshot, precisely
    /// because "zero across every test this binary ran" is the stronger
    /// statement and is immune to another test running concurrently.
    #[test]
    fn the_priority_census_closes_and_its_model_invariants_hold() {
        // 0: iload_0  1: iload_1  2: iadd
        // 3: iload_0  4: iload_1  5: imul
        // 6: ixor     7: ireturn
        let code = [0x1au8, 0x1b, 0x60, 0x1a, 0x1b, 0x68, 0x82, 0xac, 0, 0];
        let builder = IrBuilder::new(2, 2);
        let mut graph = builder.build(&code, 8).expect("build failed");
        ir_optimize::optimize(&mut graph);

        let before = ir_priority_census().blocks;
        let _ = schedule_with_options(
            &graph,
            &ScheduleOptions {
                priority_within_blocks: true,
                ..ScheduleOptions::default()
            },
        );
        let after = ir_priority_census();
        assert!(
            after.blocks > before,
            "the denominator must advance, or the census is not wired to the pass"
        );
        assert_eq!(
            after.cycle, 0,
            "the ordering DAG is built from a valid topological order, so it \
             always has one — a cycle here is a defect in the construction"
        );
        assert_eq!(
            after.memory_order, 0,
            "only pure nodes float and every impure node is in the total \
             side-effect chain, so the memory-order validator must be vacuous \
             for this pass — a refusal here means the chain is not total"
        );
        assert!(
            after.model_held(),
            "the two model invariants are also asked as one question"
        );
    }

    /// The census identity, on values rather than on the global table.
    ///
    /// `closes()` is what makes a `return` added to `priority_sort_block` and
    /// not to the census fail loudly instead of quietly reading low — the
    /// failure mode `PairCensus` documents from a census that drifted to zero
    /// while the emission it described did three things.
    #[test]
    fn the_priority_census_identity_catches_an_uncounted_exit() {
        let complete = PriorityCensus {
            blocks: 7,
            too_small: 2,
            over_budget: 1,
            duplicate_node: 0,
            missing_node: 0,
            cycle: 0,
            memory_order: 0,
            unchanged: 1,
            reordered: 3,
        };
        assert!(complete.closes());
        assert!(complete.model_held());

        // One block that left through an exit nobody counted.
        let leaky = PriorityCensus {
            blocks: 8,
            ..complete
        };
        assert!(!leaky.closes());

        // And the model invariants are a separate question from the identity:
        // a census can close perfectly while reporting a broken chain.
        let closed_but_broken = PriorityCensus {
            blocks: 7,
            memory_order: 1,
            reordered: 2,
            ..complete
        };
        assert!(closed_but_broken.closes());
        assert!(!closed_but_broken.model_held());
    }

    /// Whether a division may sink is a property of its TYPE, not its opcode.
    ///
    /// `idiv` raises `ArithmeticException` on a zero divisor and so is pinned;
    /// `ddiv` raises nothing at all (JVMS §6.5: division by zero is ±infinity)
    /// and is the single most expensive pure node the IR has. Asserted by
    /// retyping ONE node of a real graph, because that isolates the type as the
    /// only thing that changed — a second fixture built from `ddiv` bytecode
    /// would also differ in its operands, its uses and its node ids.
    #[test]
    fn a_floating_point_division_may_sink_and_an_integer_one_may_not() {
        // int f(int a, int b) { return a / b; }
        let code = [0x1au8, 0x1b, 0x6c, 0xac, 0, 0];
        let builder = IrBuilder::new(2, 2);
        let mut graph = builder.build(&code, 4).expect("build failed");
        let div = graph
            .nodes
            .iter()
            .position(|n| n.op == Op::Div)
            .expect("the idiv builds an Op::Div");

        assert_eq!(graph.nodes[div].ty, IrType::Int);
        assert!(
            !node_is_sinkable(&graph, div),
            "an integer division traps, so moving it moves where the program throws"
        );

        graph.nodes[div].ty = IrType::Double;
        assert!(
            node_is_sinkable(&graph, div),
            "ddiv cannot raise, so it is as movable as a multiply and far more \
             worth moving"
        );

        // `frem`/`drem` also cannot raise, and are still refused: they lower to
        // a `jit_frem`/`jit_drem` helper CALL, and a call is a safepoint.
        graph.nodes[div].op = Op::Rem;
        assert!(
            !node_is_sinkable(&graph, div),
            "an FP remainder is a helper call, and this pass does not move safepoints"
        );
    }

    /// The three narrowing conversions and the FP compare belong on the
    /// sinkable list, and were missing from it with no reason recorded.
    ///
    /// Each is in `Op::is_pure()`, each is a total function of its value inputs
    /// with no control and no memory edge, and `Op::LCmp` — its structural twin
    /// — was already there. An omission with no argument behind it is how a
    /// pass quietly stops applying to a whole family of code.
    #[test]
    fn the_pure_narrowing_conversions_and_the_fp_compare_are_sinkable() {
        for op in [Op::I2B, Op::I2C, Op::I2S] {
            assert!(op_is_sinkable(&op), "{op:?} is pure and has fixed arity");
            assert!(op.is_pure(), "and this list must not drift from is_pure");
        }
        let fcmp = Op::FCmp {
            double: true,
            nan_greater: false,
        };
        assert!(op_is_sinkable(&fcmp));
        assert!(
            op_is_sinkable(&Op::LCmp),
            "the twin that was already listed"
        );
        // The refusals that must survive: a store, a call-shaped node and a phi
        // are not sinkable however pure their neighbours are.
        assert!(!op_is_sinkable(&Op::Phi));
        assert!(!op_is_sinkable(&Op::ArrayLength));
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
        assert_dominators_match_reference("diamond", &cfg(4, &[(0, 1), (0, 2), (1, 3), (2, 3)]));
        // 0 -> 1 (outer header) -> 2 (inner header) -> 3 (inner latch) -> 2,
        // 2 -> 4 (outer latch) -> 1, 1 -> 5 (exit).
        assert_dominators_match_reference(
            "nested loops",
            &cfg(6, &[(0, 1), (1, 2), (2, 3), (3, 2), (2, 4), (4, 1), (1, 5)]),
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
        assert_eq!(
            d.idom(3),
            Some(0),
            "a join's idom is the branch, not an arm"
        );
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
        assert!(
            !dom.dominates(3, 1),
            "an unreachable block dominates no reachable one"
        );

        // Every use unreachable: the last unreachable block.
        assert_eq!(deepest_common_dominator(&dom, &[3], 5), Some(4));
        assert_eq!(deepest_common_dominator(&dom, &[4, 3], 5), Some(4));
        // A reachable use decides on its own.
        assert_eq!(deepest_common_dominator(&dom, &[2, 3], 5), Some(2));
        assert_eq!(deepest_common_dominator(&dom, &[1, 2, 4], 5), Some(1));
        // No block dominates an out-of-range use.
        assert_eq!(deepest_common_dominator(&dom, &[2, 9], 5), None);
    }

    /// The sink pass's dominator-tree walks give the same answers as the block
    /// scans they replaced, on several hundred generated CFGs. Unreachable
    /// blocks, self loops and irreducible cycles are all included. Placement
    /// is checked under both the real loop depths and random depths, so ties
    /// break the same way too.
    #[test]
    fn sink_placement_walks_match_the_block_scans_on_generated_cfgs() {
        let mut state: u64 = 0x5DEE_CE66_D1CE_4E5B;
        let mut next = |bound: usize| -> usize {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((state >> 33) as usize) % bound.max(1)
        };
        let mut path: Vec<usize> = Vec::new();
        for case in 0..300 {
            let n = 1 + next(20);
            let m = next(3 * n + 1);
            let edges: Vec<(usize, usize)> = (0..m).map(|_| (next(n), next(n))).collect();
            let blocks = cfg(n, &edges);
            let dom = Dominators::compute(&blocks);

            for _ in 0..24 {
                let k = 1 + next(4);
                // `next(n + 1)` can name one block past the end.
                let of: Vec<usize> = (0..k).map(|_| next(n + 1)).collect();
                assert_eq!(
                    deepest_common_dominator(&dom, &of, n),
                    deepest_common_dominator_by_scan(&dom, &of, n),
                    "case {case}: common dominator of {of:?}"
                );
            }

            let (loop_depth, innermost) = loop_nest(&blocks, &dom);
            assert_eq!(loop_depth, loop_depths(&blocks, &dom));
            let random_depth: Vec<u32> = (0..n).map(|_| next(3) as u32).collect();
            for depth in [&loop_depth, &random_depth] {
                for early in 0..n {
                    for late in 0..n {
                        if !dom.dominates(early, late) {
                            continue;
                        }
                        for equal_depth in [false, true] {
                            assert_eq!(
                                choose_sink_block(
                                    &dom,
                                    depth,
                                    &innermost,
                                    early,
                                    late,
                                    equal_depth,
                                    n,
                                    &mut path
                                ),
                                choose_sink_block_by_scan(
                                    &dom,
                                    depth,
                                    &innermost,
                                    early,
                                    late,
                                    equal_depth,
                                    n
                                ),
                                "case {case}: early {early}, late {late}, equal_depth {equal_depth}"
                            );
                        }
                    }
                }
            }
        }
    }

    /// The all-pairs scan `find_best_block` made before round 12 wave 2:
    /// `Some(Some(b))` placed in `b`, `Some(None)` no placed input, `None`
    /// no block dominated by all of them.
    fn best_block_by_all_pairs_scan(dom: &Dominators, of: &[Option<usize>]) -> Option<Option<usize>> {
        let mut input_blocks: Vec<usize> = Vec::new();
        for b in of.iter().flatten() {
            if !input_blocks.contains(b) {
                input_blocks.push(*b);
            }
        }
        if input_blocks.is_empty() {
            return Some(None);
        }
        input_blocks
            .iter()
            .copied()
            .find(|&cand| {
                input_blocks
                    .iter()
                    .all(|&other| other == cand || dom.dominates(other, cand))
            })
            .map(Some)
    }

    /// Round 12 wave 2 (lane irback, proposal R12-5): the one-pass
    /// `deepest_input_block` the scheduler and the verifier's dominance lane
    /// now share places every node where the all-pairs scan did, on several
    /// hundred generated CFGs with unreachable blocks, cycles and unplaced
    /// inputs, and an incomparability it reports is a real one.
    #[test]
    fn r12w2_deepest_input_block_matches_the_all_pairs_scan_on_generated_cfgs() {
        let mut state: u64 = 0x1B87_3593_4C0F_FEE5;
        let mut next = |bound: usize| -> usize {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((state >> 33) as usize) % bound.max(1)
        };
        for case in 0..300 {
            let n = 1 + next(20);
            let m = next(3 * n + 1);
            let edges: Vec<(usize, usize)> = (0..m).map(|_| (next(n), next(n))).collect();
            let blocks = cfg(n, &edges);
            let dom = Dominators::compute(&blocks);
            for _ in 0..24 {
                let k = next(6);
                // `None` is an input with no block yet (or `NO_NODE`).
                let of: Vec<Option<usize>> = (0..k)
                    .map(|_| if next(5) == 0 { None } else { Some(next(n)) })
                    .collect();
                let inputs: Vec<NodeId> = (0..k as NodeId).collect();
                let block_of = |i: NodeId| of.get(i as usize).copied().flatten();
                let got = deepest_input_block(&inputs, block_of, &dom);
                match (best_block_by_all_pairs_scan(&dom, &of), got) {
                    (Some(want), Ok(have)) => {
                        assert_eq!(have.map(|(b, _)| b), want, "case {case}: inputs {of:?}");
                        if let Some((b, i)) = have {
                            assert_eq!(block_of(i), Some(b), "case {case}: the named input");
                        }
                    }
                    (None, Err((a, b))) => {
                        let (ba, bb) = (block_of(a), block_of(b));
                        let (Some(ba), Some(bb)) = (ba, bb) else {
                            panic!("case {case}: {of:?} names an unplaced input");
                        };
                        assert!(
                            !dom.dominates(ba, bb) && !dom.dominates(bb, ba),
                            "case {case}: {of:?} reports comparable blocks {ba} and {bb}"
                        );
                    }
                    (want, got) => panic!("case {case}: {of:?}: scan {want:?}, walk {got:?}"),
                }
            }
        }
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
            sched.dom.dominates(input_block, user_block),
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

    // ── round 9 (irsched lane) ───────────────────────────────────────────

    /// `t = a + b; <guard>; <guard>; <guard>` with `t` named by a frame state.
    ///
    /// The guards form the side-effect chain, so the first one heads the
    /// longest chain in the block and the list scheduler picks it before the
    /// one-node `t`. That is fine for an unnamed value and wrong for a named
    /// one: a deopt at the first guard rebuilds the frame from `t`'s home word
    /// before `t` has written it.
    #[test]
    fn a_frame_named_value_is_not_list_scheduled_past_a_deopt_point() {
        let build = |named: bool| -> Vec<NodeId> {
            let mut graph = bare_graph();
            let start = graph.add(Op::Start, IrType::Control, vec![], None);
            let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
            let p0 = graph.add(Op::Param(0), IrType::Int, vec![], None);
            let p1 = graph.add(Op::Param(1), IrType::Int, vec![], None);
            let t = graph.add(Op::Add, IrType::Int, vec![p0, p1], Some(2));
            let g1 = graph.add(Op::Guard { bci: 3 }, IrType::Void, vec![ctrl, p0], Some(3));
            let g2 = graph.add(Op::Guard { bci: 4 }, IrType::Void, vec![ctrl, p1], Some(4));
            let g3 = graph.add(Op::Guard { bci: 5 }, IrType::Void, vec![ctrl, p0], Some(5));
            if named {
                graph.safepoints.push(crate::ir::SafepointSnapshot::new(
                    3,
                    vec![t],
                    Vec::new(),
                    Vec::new(),
                ));
            }
            let mut nodes = vec![t, g1, g2, g3];
            let frame_named = frame_named_values(&graph);
            priority_sort_block(&graph, &mut nodes, &frame_named);
            nodes
        };
        let pos = |nodes: &[NodeId], id: usize| nodes.iter().position(|&n| n as usize == id);
        // Node ids: 4 = t, 5 = the first guard.
        let free = build(false);
        assert!(
            pos(&free, 4) > pos(&free, 5),
            "twin: an unnamed value is free to follow the chain head, got {free:?}"
        );
        let pinned = build(true);
        assert!(
            pos(&pinned, 4) < pos(&pinned, 5),
            "a named value must stay ahead of the next deopt point, got {pinned:?}"
        );
    }

    /// Round 11 wave 10 (irback proposal W9-4): a kill gained MID-block moves
    /// its node ahead of an older, equally tall candidate — the step the
    /// incremental `kills` must not miss.
    ///
    /// `x` feeds `u1` and `u2`; `u2` also reads `y`, so it kills `y` and goes
    /// first. That leaves `u1` as `x`'s last consumer (a kill), which beats
    /// `z` although `z` has the lower seed rank. `w` (a 3-cycle `imul`) heads
    /// the longest chain and goes before everything. Hand-evaluated against
    /// the full re-score this replaced: `w, x, y, u2, u1, z`.
    #[test]
    fn r11w10_a_kill_gained_mid_block_is_seen_by_the_list_scheduler() {
        let mut graph = bare_graph();
        let p0 = graph.add(Op::Param(0), IrType::Int, vec![], None);
        let p1 = graph.add(Op::Param(1), IrType::Int, vec![], None);
        let x = graph.add(Op::Add, IrType::Int, vec![p0, p1], Some(1));
        let y = graph.add(Op::Sub, IrType::Int, vec![p0, p1], Some(2));
        let z = graph.add(Op::Add, IrType::Int, vec![p0, p0], Some(3));
        let u1 = graph.add(Op::Add, IrType::Int, vec![x, p0], Some(4));
        let u2 = graph.add(Op::Add, IrType::Int, vec![x, y], Some(5));
        let w = graph.add(Op::Mul, IrType::Int, vec![p0, p1], Some(6));
        let mut nodes = vec![x, y, z, u1, u2, w];
        let frame_named = frame_named_values(&graph);
        priority_sort_block(&graph, &mut nodes, &frame_named);
        assert_eq!(nodes, vec![w, x, y, u2, u1, z]);
    }

    /// A `while` loop with a `continue` has two latches and is still ONE loop.
    ///
    /// ```text
    ///   0 -> 1 (header) -> 2 -> 3 -> 1   (normal back edge)
    ///                      2 -> 1        (the continue)
    ///   1 -> 4 (exit)
    /// ```
    #[test]
    fn a_loop_with_two_latches_is_one_level_deep() {
        let blocks = cfg(5, &[(0, 1), (1, 2), (2, 3), (3, 1), (2, 1), (1, 4)]);
        let dom = Dominators::compute(&blocks);
        assert_eq!(loop_depths(&blocks, &dom), vec![0, 1, 1, 1, 0]);
    }

    /// A data node whose inputs live in sibling arms of a diamond has no legal
    /// home. The scheduler keeps the entry-block fallback (the one placement
    /// the emission-order check always catches), counts it, and names it.
    ///
    /// `bad` reads the two arm projections directly -- the stand-in for any
    /// value computed in one arm each -- with no phi at the merge.
    #[test]
    fn a_node_with_inputs_in_incomparable_blocks_is_counted_and_named() {
        let mut g = bare_graph();
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let p = g.add(Op::Param(0), IrType::Int, vec![], None);
        let if_node = g.add(Op::If, IrType::Control, vec![entry, p], Some(0));
        let t = g.add(Op::Proj(0), IrType::Control, vec![if_node], Some(0));
        let f = g.add(Op::Proj(1), IrType::Control, vec![if_node], Some(0));
        let merge = g.add(Op::Merge, IrType::Control, vec![t, f], Some(1));
        let good = g.add(Op::Add, IrType::Int, vec![p, p], Some(1));
        let bad = g.add(Op::Add, IrType::Int, vec![t, f], Some(1));
        let sum = g.add(Op::Add, IrType::Int, vec![good, bad], Some(1));
        let ret = g.add(Op::Return, IrType::Void, vec![merge, sum], Some(2));
        g.entry = start;
        g.exit = ret;

        let before = ir_incomparable_input_placements();
        let sched = schedule(&g);
        assert!(
            ir_incomparable_input_placements() > before,
            "the illegal placement must be counted"
        );
        let tb = sched.node_to_block[t as usize];
        let fb = sched.node_to_block[f as usize];
        assert!(!sched.dom.dominates(tb, fb) && !sched.dom.dominates(fb, tb));
        let misplaced = sched.misplaced_data_nodes(&g);
        assert!(
            misplaced.contains(&bad),
            "the schedule must name the node: {misplaced:?}"
        );
        assert!(
            !misplaced.contains(&good),
            "twin: a node with a legal home is not named: {misplaced:?}"
        );
    }

    /// The equal-depth sink rule stays inside one loop: a sibling loop the
    /// first one exits straight into is at the same depth but is a different
    /// loop (`ir-sink-equal-depth-can-cross-into-a-sibling-loop-20260918.md`).
    ///
    /// ```text
    ///   0 -> 1 (L1 header) <-> 2
    ///        1 -> 3 (L2 header) <-> 4
    ///             3 -> 5
    /// ```
    #[test]
    fn equal_depth_sinking_does_not_cross_into_a_sibling_loop() {
        let blocks = cfg(6, &[(0, 1), (1, 2), (2, 1), (1, 3), (3, 4), (4, 3), (3, 5)]);
        let dom = Dominators::compute(&blocks);
        let (depth, innermost) = loop_nest(&blocks, &dom);
        assert_eq!(depth, vec![0, 1, 1, 1, 1, 0]);
        assert_eq!(
            innermost,
            vec![None, Some(1), Some(1), Some(3), Some(3), None]
        );
        let mut path = Vec::new();
        // A value computed in L1's header whose only reader is in L2's body.
        let got = choose_sink_block(&dom, &depth, &innermost, 1, 4, true, 6, &mut path);
        assert_eq!(
            got, 1,
            "the value must stay in L1, not run once per L2 iteration"
        );
        assert_eq!(
            choose_sink_block_by_scan(&dom, &depth, &innermost, 1, 4, true, 6),
            1
        );

        // Twin: inside ONE loop the rule still moves the value down to its
        // reader. 0 -> 1 (header) -> 2 -> 3 -> 1, 1 -> 4.
        let blocks = cfg(5, &[(0, 1), (1, 2), (2, 3), (3, 1), (1, 4)]);
        let dom = Dominators::compute(&blocks);
        let (depth, innermost) = loop_nest(&blocks, &dom);
        assert_eq!(innermost, vec![None, Some(1), Some(1), Some(1), None]);
        let got = choose_sink_block(&dom, &depth, &innermost, 1, 3, true, 5, &mut path);
        assert_eq!(
            got, 3,
            "twin: equal-depth sinking within one loop still applies"
        );
    }

    /// A nested loop's blocks name the INNER header; the outer loop's own
    /// blocks name the outer one.
    #[test]
    fn the_innermost_header_of_a_nested_loop_block_is_the_inner_header() {
        // 0 -> 1 (outer) -> 2 (inner) <-> 3, 2 -> 4 -> 1, 1 -> 5.
        let blocks = cfg(6, &[(0, 1), (1, 2), (2, 3), (3, 2), (2, 4), (4, 1), (1, 5)]);
        let dom = Dominators::compute(&blocks);
        let (depth, innermost) = loop_nest(&blocks, &dom);
        assert_eq!(depth, vec![0, 1, 2, 2, 1, 0]);
        assert_eq!(
            innermost,
            vec![None, Some(1), Some(2), Some(2), Some(1), None]
        );
    }

    /// An edge out of a block the entry never reaches is not a back edge,
    /// although `dominates` answers `true` for every dominator of dead code.
    #[test]
    fn an_edge_from_dead_code_does_not_make_a_loop() {
        // 0 -> 1 -> 2 is live; 3 is unreachable and jumps into 1.
        let blocks = cfg(4, &[(0, 1), (1, 2), (3, 1)]);
        let dom = Dominators::compute(&blocks);
        assert!(!dom.is_reachable(3));
        assert_eq!(loop_depths(&blocks, &dom), vec![0, 0, 0, 0]);
        let freq = compute_frequencies(&bare_graph(), &blocks, &dom, &ScheduleOptions::default());
        assert!(!freq.is_loop_header[1], "block 1 is not a loop header");
        assert!(
            freq.freq[1] <= 1.0,
            "a live block reached once must not be multiplied by a trip count: {}",
            freq.freq[1]
        );
    }

    /// Round 11 wave 10 (irback proposal W9-3): a `Switch`-wide block answers
    /// its edge indices from the map, with `position`'s first-index answer. 13
    /// edges out of block 0 (above `SCAN_WIDTH`), two of them into block 1.
    #[test]
    fn r11w10_a_wide_switch_propagates_every_case_edge() {
        let mut edges: Vec<(usize, usize)> = (1..=12).map(|k| (0, k)).collect();
        edges.push((0, 1));
        edges.extend((1..=12).map(|k| (k, 13)));
        let blocks = cfg(14, &edges);
        assert_eq!(blocks[0].successors.len(), 13);
        let dom = Dominators::compute(&blocks);
        let freq = compute_frequencies(&bare_graph(), &blocks, &dom, &ScheduleOptions::default());
        let p = 1.0 / 13.0;
        assert_eq!(freq.edge_prob[0], vec![p; 13]);
        for k in 2..=12 {
            assert_eq!(freq.freq[k], p, "case block {k}");
        }
        // Both predecessor entries of block 1 read edge 0, as `position` did.
        assert_eq!(freq.freq[1], p + p);
        assert!(freq.freq[13] > 0.0);
    }

    // ── Round 11 (lane irback) ───────────────────────────────────────────

    fn r11_empty_graph() -> Graph {
        bare_graph()
    }

    /// A loop whose body built no control node: `Merge(entry, Merge)` (the
    /// builder makes every loop header an `Op::Merge`).
    ///
    /// `int f(int k) { if (k < 0) return -1; int i = 0; while (true) { i += k; } }`
    /// builds exactly this — the `goto` back to the header carries the
    /// region's own token. Returns `(graph, region, phi, next)`.
    fn r11_self_loop_graph() -> (Graph, NodeId, NodeId, NodeId) {
        let mut g = r11_empty_graph();
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let k = g.add(Op::Param(0), IrType::Int, vec![start], None);
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let cond = g.add(Op::Cmp(crate::ir::CmpOp::Ge), IrType::Int, vec![k, zero], None);
        let if_node = g.add(Op::If, IrType::Control, vec![ctrl, cond], None);
        let t = g.add(Op::Proj(0), IrType::Control, vec![if_node], None);
        let f = g.add(Op::Proj(1), IrType::Control, vec![if_node], None);
        let minus_one = g.add(Op::Const(-1), IrType::Int, vec![], None);
        let ret = g.add(Op::Return, IrType::Void, vec![f, minus_one], None);
        let region = g.add(Op::Merge, IrType::Control, vec![t], None);
        assert!(g.push_input(region, region));
        let phi = g.add(Op::Phi, IrType::Int, vec![region, zero, NO_NODE], None);
        let next = g.add(Op::Add, IrType::Int, vec![phi, k], None);
        assert!(g.set_input(phi, 2, next));
        g.entry = start;
        g.exit = ret;
        (g, region, phi, next)
    }

    /// A self-looping region keeps its self edge. Before round 11 Step 3
    /// skipped `succ_block == block_idx`, so the loop block had neither a
    /// terminator nor a successor and `ir_lower::lower_block` emitted no jump:
    /// the loop ran once and fell into the next block in layout order.
    #[test]
    fn r11_a_self_looping_region_keeps_its_back_edge() {
        let (g, region, _, _) = r11_self_loop_graph();
        let sched = schedule(&g);
        let rb = sched
            .blocks
            .iter()
            .position(|b| b.ctrl == region)
            .expect("the region heads a block");
        assert!(
            sched.blocks[rb].successors.contains(&rb),
            "the loop's back edge was dropped: successors {:?}",
            sched.blocks[rb].successors
        );
        assert!(sched.blocks[rb].predecessors.contains(&rb));
        // The general invariant `lower_block` relies on: a block with no
        // terminator is a goto and must name where it goes.
        for (b, blk) in sched.blocks.iter().enumerate() {
            if blk.terminator.is_none() {
                assert!(
                    !blk.successors.is_empty(),
                    "block {b} has neither a terminator nor a successor"
                );
            }
        }
        // And the frequency model sees a loop there.
        assert!(sched.freq.is_loop_header[rb]);
    }

    /// `topo_sort_block` does not treat a phi's value inputs as dependences.
    /// It used to descend into them, so `[phi, next = phi + k]` in one block
    /// came out `[next, phi]`.
    #[test]
    fn r11_topo_sort_keeps_a_phi_ahead_of_its_loop_carried_input() {
        let (g, _, phi, next) = r11_self_loop_graph();
        let mut nodes = vec![phi, next];
        topo_sort_block(&g, &mut nodes);
        assert_eq!(nodes, vec![phi, next]);
        // Presented the other way round, the real dependence (`next` reads
        // `phi`) still wins.
        let mut nodes = vec![next, phi];
        topo_sort_block(&g, &mut nodes);
        assert_eq!(nodes, vec![phi, next]);
    }

    // ── Round 11 wave 2 (lane ircore) ────────────────────────────────────

    /// A diamond on `p != 0` with one pinned load per arm:
    /// `(graph, merge, load_in_true_arm, load_in_false_arm)`.
    fn r11w2_diamond_with_arm_loads() -> (Graph, NodeId, NodeId, NodeId) {
        let mut g = r11_empty_graph();
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let base = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let p = g.add(Op::Param(1), IrType::Int, vec![start], None);
        let off = g.add(Op::Const(16), IrType::Int, vec![], None);
        let if_node = g.add(Op::If, IrType::Control, vec![entry, p], Some(0));
        let t = g.add(Op::Proj(0), IrType::Control, vec![if_node], Some(0));
        let f = g.add(Op::Proj(1), IrType::Control, vec![if_node], Some(0));
        let lt = g.add(
            Op::Load(crate::ir::MemKind::Int),
            IrType::Int,
            vec![t, mem, base, off],
            Some(3),
        );
        let lf = g.add(
            Op::Load(crate::ir::MemKind::Int),
            IrType::Int,
            vec![f, mem, base, off],
            Some(6),
        );
        let merge = g.add(Op::Merge, IrType::Control, vec![t, f], Some(9));
        g.entry = start;
        (g, merge, lt, lf)
    }

    /// The finished schedule of a well-formed graph names nothing — including
    /// a φ whose loop-carried input is defined in the loop block itself.
    #[test]
    fn r11w2_a_well_formed_schedule_has_no_misplaced_nodes() {
        let (mut g, merge, lt, lf) = r11w2_diamond_with_arm_loads();
        let phi = g.add(Op::Phi, IrType::Int, vec![merge, lt, lf], Some(9));
        let ret = g.add(Op::Return, IrType::Void, vec![merge, phi], Some(9));
        g.exit = ret;
        let sched = schedule(&g);
        assert_eq!(sched.misplaced_data_nodes(&g), Vec::<NodeId>::new());

        let (g, _, _, _) = r11_self_loop_graph();
        let sched = schedule(&g);
        assert_eq!(sched.misplaced_data_nodes(&g), Vec::<NodeId>::new());
    }

    /// A TERMINATOR reading a value from one arm of a diamond it is joined
    /// below. `verify_data_locations` compares emission order and passes when
    /// that arm is laid out first; the dominance check names the `Return`.
    #[test]
    fn r11w2_a_terminator_reading_a_sibling_arm_value_is_named() {
        let (mut g, merge, lt, _lf) = r11w2_diamond_with_arm_loads();
        let ret = g.add(Op::Return, IrType::Void, vec![merge, lt], Some(9));
        g.exit = ret;
        let sched = schedule(&g);
        let misplaced = sched.misplaced_data_nodes(&g);
        assert!(misplaced.contains(&ret), "{misplaced:?}");
        assert!(!misplaced.contains(&lt), "{misplaced:?}");
    }

    /// A φ whose inputs are swapped relative to its merge's predecessors reads
    /// each arm's value on the OTHER arm's edge; the φ half names it.
    #[test]
    fn r11w2_a_phi_input_that_does_not_dominate_its_edge_is_named() {
        let (mut g, merge, lt, lf) = r11w2_diamond_with_arm_loads();
        // merge = [t, f], so input 1 travels the `t` edge and must be `lt`.
        let phi = g.add(Op::Phi, IrType::Int, vec![merge, lf, lt], Some(9));
        let ret = g.add(Op::Return, IrType::Void, vec![merge, phi], Some(9));
        g.exit = ret;
        let sched = schedule(&g);
        let misplaced = sched.misplaced_data_nodes(&g);
        assert!(misplaced.contains(&phi), "{misplaced:?}");
        assert!(!misplaced.contains(&ret), "{misplaced:?}");
    }

    /// Round 11 wave 4: the release form's side-effect half. A store anchored
    /// at the ENTRY that stores the true arm's load is placed by
    /// `find_best_block` in the true arm — every input dominates it there, so
    /// the three older halves pass — and would run on one path only. The same
    /// store anchored at the true arm, where the load is, is well formed.
    #[test]
    fn r11w4_a_side_effect_sunk_below_its_anchor_is_named() {
        let (mut g, merge, lt, lf) = r11w2_diamond_with_arm_loads();
        let t = g.nodes[lt as usize].inputs[0];
        let mem = g.nodes[lt as usize].inputs[1];
        let base = g.nodes[lt as usize].inputs[2];
        let off = g.nodes[lt as usize].inputs[3];
        let if_node = g.nodes[t as usize].inputs[0];
        let entry = g.nodes[if_node as usize].inputs[0];
        let sunk = g.add(
            Op::Store(crate::ir::MemKind::Int),
            IrType::Memory,
            vec![entry, mem, base, off, lt],
            Some(1),
        );
        let phi = g.add(Op::Phi, IrType::Int, vec![merge, lt, lf], Some(9));
        let ret = g.add(Op::Return, IrType::Void, vec![merge, phi], Some(9));
        g.exit = ret;
        let sched = schedule(&g);
        assert_ne!(
            sched.node_to_block[sunk as usize], sched.node_to_block[entry as usize],
            "the fixture relies on the scheduler sinking the store"
        );
        assert_eq!(sched.misplaced_data_nodes(&g), vec![sunk]);

        let (mut g, merge, lt, lf) = r11w2_diamond_with_arm_loads();
        let t = g.nodes[lt as usize].inputs[0];
        let mem = g.nodes[lt as usize].inputs[1];
        let base = g.nodes[lt as usize].inputs[2];
        let off = g.nodes[lt as usize].inputs[3];
        g.add(
            Op::Store(crate::ir::MemKind::Int),
            IrType::Memory,
            vec![t, mem, base, off, lt],
            Some(4),
        );
        let phi = g.add(Op::Phi, IrType::Int, vec![merge, lt, lf], Some(9));
        let ret = g.add(Op::Return, IrType::Void, vec![merge, phi], Some(9));
        g.exit = ret;
        let sched = schedule(&g);
        assert_eq!(sched.misplaced_data_nodes(&g), Vec::<NodeId>::new());
    }

    /// Round 11 wave 7 (`r11w6-orch-post-schedule-phi-check-refuses-hashmap`):
    /// the φ half resolves each merge input to the PREDECESSOR block its
    /// control token heads, as `ir_lower::emit_phi_copies` places the copy.
    /// `HashMap.getNode`'s refused graph, once `ir_optimize` had folded
    /// `φ(undef, first.key)` to `first.key`, was this: a φ at a join BELOW an
    /// earlier diamond reading a value one arm of that diamond defines. The
    /// check must keep naming it; the same join reading the diamond's own φ is
    /// well formed.
    #[test]
    fn r11w7_a_phi_reading_an_earlier_arm_value_past_its_join_is_named() {
        let build = |through_the_join: bool| {
            let (mut g, merge, lt, lf) = r11w2_diamond_with_arm_loads();
            let t = g.nodes[lt as usize].inputs[0];
            let if_node = g.nodes[t as usize].inputs[0];
            let p = g.nodes[if_node as usize].inputs[1];
            let joined = g.add(Op::Phi, IrType::Int, vec![merge, lt, lf], Some(9));
            let if2 = g.add(Op::If, IrType::Control, vec![merge, p], Some(10));
            let a = g.add(Op::Proj(0), IrType::Control, vec![if2], Some(10));
            let b = g.add(Op::Proj(1), IrType::Control, vec![if2], Some(10));
            let m2 = g.add(Op::Merge, IrType::Control, vec![a, b], Some(12));
            let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
            let on_a = if through_the_join { joined } else { lt };
            let phi = g.add(Op::Phi, IrType::Int, vec![m2, on_a, zero], Some(12));
            let ret = g.add(Op::Return, IrType::Void, vec![m2, phi], Some(12));
            g.exit = ret;
            (g, phi, lt)
        };

        let (g, phi, lt) = build(false);
        let misplaced = schedule(&g).misplaced_data_nodes(&g);
        assert!(misplaced.contains(&phi), "{misplaced:?}");
        assert!(
            dominance_violations(&g, false).contains(&DominanceViolation::PhiEdge {
                phi,
                slot: 1,
                def: lt
            }),
            "the graph-level lane names the same edge"
        );

        let (g, _, _) = build(true);
        assert_eq!(schedule(&g).misplaced_data_nodes(&g), Vec::<NodeId>::new());
        assert_eq!(dominance_violations(&g, false), no_findings());
    }

    /// Round 11 wave 8 (irback proposal W7-2): `misplaced_reads` names the
    /// READ — slot, definition and, for a φ, the edge — for each of the three
    /// shapes the node-only answer used to leave to an IR dump.
    #[test]
    fn r11w8_misplaced_reads_name_the_slot_the_def_and_the_edge() {
        // φ half: merge = [t, f], so slot 1 travels the `t` edge and reads
        // `lf`, which only the `f` arm computes.
        let (mut g, merge, lt, lf) = r11w2_diamond_with_arm_loads();
        let t = g.nodes[lt as usize].inputs[0];
        let phi = g.add(Op::Phi, IrType::Int, vec![merge, lf, lt], Some(9));
        let ret = g.add(Op::Return, IrType::Void, vec![merge, phi], Some(9));
        g.exit = ret;
        let sched = schedule(&g);
        let reads = sched.misplaced_reads(&g);
        assert!(
            reads.contains(&MisplacedRead {
                node: phi,
                slot: 1,
                def: lf,
                edge: Some(t),
            }),
            "{reads:?}"
        );
        assert_eq!(
            reads.iter().map(|r| r.node).collect::<Vec<_>>(),
            sched.misplaced_data_nodes(&g),
            "the node-only answer is the same list"
        );

        // Terminator half: the `Return` below the join reads the `t` arm's load.
        let (mut g, merge, lt, _lf) = r11w2_diamond_with_arm_loads();
        let ret = g.add(Op::Return, IrType::Void, vec![merge, lt], Some(9));
        g.exit = ret;
        let reads = schedule(&g).misplaced_reads(&g);
        assert!(
            reads.contains(&MisplacedRead {
                node: ret,
                slot: 1,
                def: lt,
                edge: None,
            }),
            "{reads:?}"
        );

        // Side-effect half: a store anchored at the entry, sunk into the arm.
        let (mut g, merge, lt, lf) = r11w2_diamond_with_arm_loads();
        let t = g.nodes[lt as usize].inputs[0];
        let mem = g.nodes[lt as usize].inputs[1];
        let base = g.nodes[lt as usize].inputs[2];
        let off = g.nodes[lt as usize].inputs[3];
        let if_node = g.nodes[t as usize].inputs[0];
        let entry = g.nodes[if_node as usize].inputs[0];
        let sunk = g.add(
            Op::Store(crate::ir::MemKind::Int),
            IrType::Memory,
            vec![entry, mem, base, off, lt],
            Some(1),
        );
        let phi = g.add(Op::Phi, IrType::Int, vec![merge, lt, lf], Some(9));
        let ret = g.add(Op::Return, IrType::Void, vec![merge, phi], Some(9));
        g.exit = ret;
        assert_eq!(
            schedule(&g).misplaced_reads(&g),
            vec![MisplacedRead {
                node: sunk,
                slot: 0,
                def: entry,
                edge: None,
            }]
        );
    }

    /// The bytecode that produced it: a local set on one arm before a join,
    /// then reassigned in a loop and never read. The builder now retires the
    /// φs that merge the unset slot (`IrBuilder::retire_untypeable_phis`), so
    /// nothing is left for `ir_optimize` to fold into a misplaced read — built
    /// or optimized, release form or graph lane.
    #[test]
    fn r11w7_getnode_shape_builds_and_optimizes_with_no_misplaced_phi() {
        // static int f(int[] a, int c) { int k;
        //   if (c > 0) k = a.length; c--; while (c > 0) { k = c; c--; } return 0; }
        let code: [u8; 24] = [
            0x1b, 0x9e, 0x00, 0x06, 0x2a, 0xbe, 0x3d, 0x84, 0x01, 0xff, 0x1b, 0x9e, 0x00, 0x0b,
            0x1b, 0x3d, 0x84, 0x01, 0xff, 0xa7, 0xff, 0xf7, 0x03, 0xac,
        ];
        let mut b = IrBuilder::new(2, 3);
        b.set_param_types(&[IrType::Ref, IrType::Int]);
        let Some(mut graph) = b.build(&code, 24) else {
            panic!("the getNode shape must build");
        };
        assert_eq!(dominance_violations(&graph, true), no_findings(), "built");
        assert_eq!(
            schedule(&graph).misplaced_data_nodes(&graph),
            Vec::<NodeId>::new(),
            "built, release form"
        );
        ir_optimize::optimize(&mut graph);
        assert_eq!(
            dominance_violations(&graph, false),
            no_findings(),
            "optimized"
        );
        assert_eq!(
            schedule_with_options(&graph, &production_schedule_options())
                .misplaced_data_nodes(&graph),
            Vec::<NodeId>::new(),
            "optimized, release form"
        );
    }

    /// Round 11 wave 7, the scheduler half of
    /// `r11w6-lower-sr-recipe-values-scheduled-after-the-deopt`: a user-less
    /// pure value no frame names (an eliminated store's value, which a
    /// scalar-replaced object's materialization reads) follows the guard chain
    /// under list scheduling, unless the caller lists it in
    /// `extra_frame_named`.
    #[test]
    fn r11w7_an_extra_named_recipe_value_is_scheduled_ahead_of_the_deopt() {
        let run = |listed: bool| -> (Vec<NodeId>, NodeId, NodeId) {
            let mut g = r11_empty_graph();
            let start = g.add(Op::Start, IrType::Control, vec![], None);
            let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
            let p0 = g.add(Op::Param(0), IrType::Int, vec![], None);
            let p1 = g.add(Op::Param(1), IrType::Int, vec![], None);
            let t = g.add(Op::Add, IrType::Int, vec![p0, p1], Some(2));
            let g1 = g.add(Op::Guard { bci: 3 }, IrType::Void, vec![ctrl, p0], Some(3));
            g.add(Op::Guard { bci: 4 }, IrType::Void, vec![ctrl, p1], Some(4));
            g.add(Op::Guard { bci: 5 }, IrType::Void, vec![ctrl, p0], Some(5));
            let ret = g.add(Op::Return, IrType::Void, vec![ctrl, p0], Some(6));
            g.entry = start;
            g.exit = ret;
            let opts = ScheduleOptions {
                priority_within_blocks: true,
                extra_frame_named: if listed { vec![t] } else { Vec::new() },
                ..ScheduleOptions::default()
            };
            let sched = schedule_with_options(&g, &opts);
            let b = sched.node_to_block[g1 as usize];
            assert_eq!(
                sched.node_to_block[t as usize], b,
                "the fixture puts the value in the guards' block"
            );
            (sched.blocks[b].nodes.clone(), t, g1)
        };
        let pos = |nodes: &[NodeId], id: NodeId| nodes.iter().position(|&n| n == id);
        let (free, t, g1) = run(false);
        assert!(
            pos(&free, t) > pos(&free, g1),
            "twin: an unlisted user-less value follows the chain head, got {free:?}"
        );
        let (pinned, t, g1) = run(true);
        assert!(
            pos(&pinned, t) < pos(&pinned, g1),
            "a listed value stays ahead of the first deopt point, got {pinned:?}"
        );
    }

    // ── Round 11 wave 3 (lane ircore): graph-level dominance ─────────────

    fn no_findings() -> Vec<DominanceViolation> {
        Vec::new()
    }

    /// The two well-formed fixtures above — a diamond joined by a φ, and a
    /// loop whose body built no control node — have no finding, snapshot half
    /// included.
    #[test]
    fn r11w3_a_well_formed_graph_has_no_dominance_violations() {
        let (mut g, merge, lt, lf) = r11w2_diamond_with_arm_loads();
        let phi = g.add(Op::Phi, IrType::Int, vec![merge, lt, lf], Some(9));
        let ret = g.add(Op::Return, IrType::Void, vec![merge, phi], Some(9));
        g.exit = ret;
        assert_eq!(dominance_violations(&g, true), no_findings());

        let (g, _, _, _) = r11_self_loop_graph();
        assert_eq!(dominance_violations(&g, true), no_findings());
    }

    /// The four shapes the scheduler cannot repair by moving a node, each on
    /// the diamond: a terminator, a φ edge, a node with a side effect, and a
    /// pure node whose inputs are in sibling arms.
    #[test]
    fn r11w3_each_unschedulable_read_is_named() {
        // A `Return` in the join reading the true arm's load.
        let (mut g, merge, lt, _lf) = r11w2_diamond_with_arm_loads();
        let ret = g.add(Op::Return, IrType::Void, vec![merge, lt], Some(9));
        g.exit = ret;
        assert_eq!(
            dominance_violations(&g, false),
            vec![DominanceViolation::Input {
                user: ret,
                slot: 1,
                def: lt
            }]
        );

        // A φ whose inputs are swapped against its merge's predecessors.
        let (mut g, merge, lt, lf) = r11w2_diamond_with_arm_loads();
        let phi = g.add(Op::Phi, IrType::Int, vec![merge, lf, lt], Some(9));
        let ret = g.add(Op::Return, IrType::Void, vec![merge, phi], Some(9));
        g.exit = ret;
        let found = dominance_violations(&g, false);
        assert!(
            found.contains(&DominanceViolation::PhiEdge {
                phi,
                slot: 1,
                def: lf
            }) && found.contains(&DominanceViolation::PhiEdge {
                phi,
                slot: 2,
                def: lt
            }),
            "{found:?}"
        );

        // A store anchored at the join, storing the true arm's load: the
        // scheduler would sink it into the true arm and skip it on the other.
        let (mut g, merge, lt, lf) = r11w2_diamond_with_arm_loads();
        let phi = g.add(Op::Phi, IrType::Int, vec![merge, lt, lf], Some(9));
        let base = g.nodes[lt as usize].inputs[2];
        let off = g.nodes[lt as usize].inputs[3];
        let mem = g.nodes[lt as usize].inputs[1];
        let st = g.add(
            Op::Store(crate::ir::MemKind::Int),
            IrType::Memory,
            vec![merge, mem, base, off, lt],
            Some(9),
        );
        let ret = g.add(Op::Return, IrType::Void, vec![merge, phi], Some(10));
        g.exit = ret;
        assert_eq!(
            dominance_violations(&g, false),
            vec![DominanceViolation::Input {
                user: st,
                slot: 4,
                def: lt
            }]
        );

        // A pure node over both arms' loads, with no φ.
        let (mut g, merge, lt, lf) = r11w2_diamond_with_arm_loads();
        let sum = g.add(Op::Add, IrType::Int, vec![lt, lf], Some(9));
        let ret = g.add(Op::Return, IrType::Void, vec![merge, sum], Some(9));
        g.exit = ret;
        let found = dominance_violations(&g, false);
        assert!(
            matches!(
                found.as_slice(),
                [DominanceViolation::Incomparable { user, .. }] if *user == sum
            ),
            "{found:?}"
        );
    }

    /// A pinned node that deopts into a snapshot naming a value from a sibling
    /// arm: named only by the snapshot half.
    #[test]
    fn r11w3_a_frame_value_that_does_not_dominate_its_deopt_site_is_named() {
        // Clears this thread's splice resume map, so pcs resolve as given.
        let _scope = crate::ir::IrBuildResultsScope::enter();
        let (mut g, merge, lt, lf) = r11w2_diamond_with_arm_loads();
        let phi = g.add(Op::Phi, IrType::Int, vec![merge, lt, lf], Some(9));
        let ret = g.add(Op::Return, IrType::Void, vec![merge, phi], Some(9));
        g.exit = ret;
        // `lt` (bci 3) deopts into the frame at bci 3, which names `lf` — a
        // value the false arm computes.
        g.push_safepoint(crate::ir::SafepointSnapshot::new(
            3,
            vec![lf],
            vec![],
            Vec::new(),
        ));
        assert_eq!(dominance_violations(&g, false), no_findings());
        assert_eq!(
            dominance_violations(&g, true),
            vec![DominanceViolation::FrameState {
                site: lt,
                snapshot: 0,
                def: lf
            }]
        );
    }

    /// Graphs the bytecode front end builds have no finding, before and after
    /// `ir_optimize::optimize`: a join inside a rotated loop, an ECJ loop
    /// inside a javac one, javac's nested counted loops, a do-while whose
    /// header is the method entry, and a lock held across a loop. The
    /// verifier's dominance lane runs by default in every debug build, so a
    /// finding here would be a false positive on legal bytecode. (The snapshot
    /// half, which runs only on request, is asserted on the built graphs.)
    #[test]
    fn r11w3_builder_graphs_have_no_dominance_violations() {
        // int f(int n) { int s = 0; for (int i = 0; i < n; i++)
        //   { if ((i & 1) == 0) s += i; else s -= 1; } return s; }  (rotated)
        let rotated_if_else: &[u8] = &[
            0x03, 0x3c, 0x03, 0x3d, 0xa7, 0x00, 0x16, 0x1c, 0x04, 0x7e, 0x9a, 0x00, 0x0a, 0x1b,
            0x1c, 0x60, 0x3c, 0xa7, 0x00, 0x06, 0x84, 0x01, 0xff, 0x84, 0x02, 0x01, 0x1c, 0x1a,
            0xa1, 0xff, 0xeb, 0x1b, 0xac, 0, 0,
        ];
        // An ECJ-shaped inner loop inside a javac outer loop.
        let rotated_inside_javac: &[u8] = &[
            0x03, 0x3c, 0x03, 0x3d, 0x1c, 0x1a, 0xa2, 0x00, 0x1a, 0x03, 0x3e, 0xa7, 0x00, 0x0a,
            0x1b, 0x1d, 0x60, 0x3c, 0x84, 0x03, 0x01, 0x1d, 0x1c, 0xa1, 0xff, 0xf7, 0x84, 0x02,
            0x01, 0xa7, 0xff, 0xe7, 0x1b, 0xac, 0, 0,
        ];
        // javac's nested counted loops.
        let javac_nested: &[u8] = &[
            0x03, 0x3c, 0x03, 0x3d, 0x1c, 0x1a, 0xa2, 0x00, 0x19, 0x03, 0x3e, 0x1d, 0x1a, 0xa2,
            0x00, 0x0c, 0x84, 0x01, 0x01, 0x84, 0x03, 0x01, 0xa7, 0xff, 0xf5, 0x84, 0x02, 0x01,
            0xa7, 0xff, 0xe8, 0x1b, 0xac, 0, 0,
        ];
        // int f(int n) { do { n = n - 3; } while (n > 0); return n; }
        let do_while_at_entry: &[u8] = &[
            0x1a, 0x06, 0x64, 0x3b, 0x1a, 0x9d, 0xff, 0xfb, 0x1a, 0xac, 0, 0,
        ];
        // synchronized (o) { while (k != 0) {} }
        let lock_across_loop: &[u8] = &[
            0x2a, 0x59, 0x4c, 0xc2, 0x1c, 0x99, 0x00, 0x06, 0xa7, 0xff, 0xfc, 0x2b, 0xc3, 0xb1, 0,
            0,
        ];
        let fixtures: [(&str, &[u8], usize, usize, usize); 5] = [
            ("rotated_if_else", rotated_if_else, 33, 1, 3),
            ("rotated_inside_javac", rotated_inside_javac, 34, 1, 4),
            ("javac_nested", javac_nested, 33, 1, 4),
            ("do_while_at_entry", do_while_at_entry, 10, 1, 1),
            ("lock_across_loop", lock_across_loop, 14, 3, 3),
        ];
        for (name, code, len, params, locals) in fixtures {
            let Some(mut graph) = IrBuilder::new(params, locals).build(code, len) else {
                panic!("{name}: the fixture must build");
            };
            assert_eq!(
                dominance_violations(&graph, true),
                no_findings(),
                "{name}: built"
            );
            // The release form (`lib.rs` refuses on its first entry) must be
            // as silent as the debug lane on legal bytecode (round 11 wave 4).
            assert_eq!(
                schedule(&graph).misplaced_data_nodes(&graph),
                Vec::<NodeId>::new(),
                "{name}: built, release form"
            );
            ir_optimize::optimize(&mut graph);
            assert_eq!(
                dominance_violations(&graph, false),
                no_findings(),
                "{name}: optimized"
            );
            assert_eq!(
                schedule_with_options(&graph, &production_schedule_options())
                    .misplaced_data_nodes(&graph),
                Vec::<NodeId>::new(),
                "{name}: optimized, release form"
            );
        }
    }

    // ── Round 11 wave 10 (lane ircore): every pinned op keeps its anchor ──

    /// `r11w9-ircore-anchor-rule-covers-five-of-the-pinned-ops`: a `Guard`, a
    /// `NewArray` and a `Load` anchored at the ENTRY whose last operand is a
    /// value only the true arm computes. `find_best_block` sinks each into the
    /// true arm — a guard or an allocation on one path only — and every input
    /// dominates it there. The debug lane names the operand that does not
    /// dominate the anchor, and since wave 11 the release form names the node
    /// (`anchor_rule_is_released`).
    #[test]
    fn r11w10_every_pinned_op_sunk_below_its_anchor_is_named() {
        for which in ["guard", "new_array", "load"] {
            let (mut g, merge, lt, lf) = r11w2_diamond_with_arm_loads();
            let t = g.nodes[lt as usize].inputs[0];
            let mem = g.nodes[lt as usize].inputs[1];
            let base = g.nodes[lt as usize].inputs[2];
            let if_node = g.nodes[t as usize].inputs[0];
            let entry = g.nodes[if_node as usize].inputs[0];
            let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
            let c = g.add(
                Op::Cmp(crate::ir::CmpOp::Ge),
                IrType::Int,
                vec![lt, zero],
                Some(4),
            );
            let (pinned, slot) = match which {
                "guard" => (
                    g.add(Op::Guard { bci: 1 }, IrType::Void, vec![entry, c], Some(1)),
                    1,
                ),
                "new_array" => (
                    g.add(
                        Op::NewArray {
                            element_type: 10,
                            component_class_id: 0,
                        },
                        IrType::Ref,
                        vec![entry, mem, c],
                        Some(1),
                    ),
                    2,
                ),
                _ => (
                    g.add(
                        Op::Load(crate::ir::MemKind::Int),
                        IrType::Int,
                        vec![entry, mem, base, c],
                        Some(1),
                    ),
                    3,
                ),
            };
            let phi = g.add(Op::Phi, IrType::Int, vec![merge, lt, lf], Some(9));
            let ret = g.add(Op::Return, IrType::Void, vec![merge, phi], Some(9));
            g.exit = ret;
            assert_eq!(
                pinned_anchor(&g, &g.nodes[pinned as usize]),
                Some(entry),
                "{which}"
            );
            let sched = schedule(&g);
            assert_ne!(
                sched.node_to_block[pinned as usize], sched.node_to_block[entry as usize],
                "{which}: the fixture relies on the scheduler sinking the node"
            );
            assert_eq!(
                dominance_violations(&g, false),
                vec![DominanceViolation::Input {
                    user: pinned,
                    slot,
                    def: c
                }],
                "{which}"
            );
            // Released in wave 11: the release form names it too.
            assert_eq!(
                sched.misplaced_data_nodes(&g),
                vec![pinned],
                "{which}: release form"
            );

            // Twin: the same node anchored at the true arm, where its operand
            // is computed, is well formed.
            assert!(g.set_input(pinned, 0, t));
            assert_eq!(
                dominance_violations(&g, false),
                no_findings(),
                "{which}: twin"
            );
        }
    }

    /// What [`pinned_anchor`] does NOT pin: the control nodes themselves (a
    /// one-predecessor `Merge` has the same `0..1` control range), the
    /// terminators (their own half), a floating data node, and the compact
    /// hand-built forms whose slot 0 is not a control token.
    #[test]
    fn r11w10_pinned_anchor_leaves_control_terminators_and_compact_forms_alone() {
        let (mut g, merge, lt, _lf) = r11w2_diamond_with_arm_loads();
        let t = g.nodes[lt as usize].inputs[0];
        let base = g.nodes[lt as usize].inputs[2];
        let if_node = g.nodes[t as usize].inputs[0];
        let one_pred = g.add(Op::Merge, IrType::Control, vec![t], Some(9));
        let ret = g.add(Op::Return, IrType::Void, vec![merge, lt], Some(9));
        let add = g.add(Op::Add, IrType::Int, vec![lt, lt], Some(9));
        let compact = g.add(
            Op::Load(crate::ir::MemKind::Int),
            IrType::Int,
            vec![base],
            Some(9),
        );
        for id in [t, if_node, one_pred, ret, add, compact] {
            assert_eq!(pinned_anchor(&g, &g.nodes[id as usize]), None, "n{id}");
        }
        assert_eq!(pinned_anchor(&g, &g.nodes[lt as usize]), Some(t));
    }

    /// The builder's own graph for an array-summing loop — a pinned
    /// `ArrayLength` in the header, a pinned `ArrayLoad` in the body — has no
    /// finding under the widened rule, as built and after `optimize` (LICM
    /// re-points the length's anchor at the pre-header).
    #[test]
    fn r11w10_an_array_loop_has_no_pinned_op_below_its_anchor() {
        // static int f(int[] a, int n) {
        //   int s = 0; for (int i = 0; i < a.length; i++) s += a[i]; return s; }
        let code: [u8; 24] = [
            0x03, 0x3d, 0x03, 0x3e, // 0: iconst_0; istore_2; iconst_0; istore_3
            0x1d, 0x2a, 0xbe, // 4: iload_3; aload_0; arraylength
            0xa2, 0x00, 0x0f, // 7: if_icmpge 22
            0x1c, 0x2a, 0x1d, 0x2e, // 10: iload_2; aload_0; iload_3; iaload
            0x60, 0x3d, // 14: iadd; istore_2
            0x84, 0x03, 0x01, // 16: iinc 3, 1
            0xa7, 0xff, 0xf1, // 19: goto 4
            0x1c, 0xac, // 22: iload_2; ireturn
        ];
        let mut b = IrBuilder::new(2, 4);
        b.set_param_types(&[IrType::Ref, IrType::Int]);
        let Some(mut graph) = b.build(&code, 24) else {
            panic!("the array loop must build");
        };
        assert!(
            graph
                .nodes
                .iter()
                .any(|n| matches!(n.op, Op::ArrayLoad(_)) && pinned_anchor(&graph, n).is_some()),
            "the fixture must hold a pinned array load"
        );
        assert_eq!(dominance_violations(&graph, false), no_findings(), "built");
        ir_optimize::optimize(&mut graph);
        assert_eq!(
            dominance_violations(&graph, false),
            no_findings(),
            "optimized"
        );
        assert_eq!(
            schedule_with_options(&graph, &production_schedule_options())
                .misplaced_data_nodes(&graph),
            Vec::<NodeId>::new(),
            "optimized, release form"
        );
    }

    // ── Round 11 wave 11 (lane ircore) ───────────────────────────────────

    /// `r11w10-orch-schedule-places-phi-edge-input-off-its-edge`, reduced:
    /// `int f(int n) { int held = 0; while (held < n) held++;
    ///   return held == 0 ? 0 : -1 - held; }`. Returns
    /// `(graph, join, loop_exit, sub)`.
    ///
    /// The join is created BEFORE the loop's projections, as
    /// `IrBuilder::build` pre-creates every merge in pc order before it walks;
    /// so the join heads a lower-numbered block than the loop exit that is its
    /// immediate dominator. That numbering is what made the sink pick the join.
    fn r11w11_ternary_after_loop_graph() -> (Graph, NodeId, NodeId, NodeId) {
        let mut g = r11_empty_graph();
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let join = g.add(Op::Merge, IrType::Control, vec![], Some(20));
        let header = g.add(Op::Merge, IrType::Control, vec![entry], Some(2));
        let n = g.add(Op::Param(0), IrType::Int, vec![], None);
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let minus_one = g.add(Op::Const(-1), IrType::Int, vec![], None);
        let held = g.add(Op::Phi, IrType::Int, vec![header, zero, NO_NODE], Some(2));
        let lt = g.add(
            Op::Cmp(crate::ir::CmpOp::Lt),
            IrType::Int,
            vec![held, n],
            Some(4),
        );
        let if1 = g.add(Op::If, IrType::Control, vec![header, lt], Some(4));
        let body = g.add(Op::Proj(0), IrType::Control, vec![if1], Some(4));
        let exit = g.add(Op::Proj(1), IrType::Control, vec![if1], Some(4));
        let next = g.add(Op::Add, IrType::Int, vec![held, one], Some(7));
        assert!(g.push_input(header, body));
        assert!(g.set_input(held, 2, next));
        let ne = g.add(
            Op::Cmp(crate::ir::CmpOp::Ne),
            IrType::Int,
            vec![held, zero],
            Some(12),
        );
        let if2 = g.add(Op::If, IrType::Control, vec![exit, ne], Some(12));
        let nonzero = g.add(Op::Proj(0), IrType::Control, vec![if2], Some(12));
        let is_zero = g.add(Op::Proj(1), IrType::Control, vec![if2], Some(12));
        // `-1 - held`: early block is the loop header (it reads the header
        // φ), and its only reader is the join's φ, on the `nonzero` edge.
        let sub = g.add(Op::Sub, IrType::Int, vec![minus_one, held], Some(18));
        assert!(g.push_input(join, is_zero));
        assert!(g.push_input(join, nonzero));
        let phi = g.add(Op::Phi, IrType::Int, vec![join, zero, sub], Some(20));
        let ret = g.add(Op::Return, IrType::Void, vec![join, phi], Some(20));
        g.entry = start;
        g.exit = ret;
        (g, join, exit, sub)
    }

    /// The graph is well formed (the graph-level lane names nothing: the φ's
    /// input is defined in the loop header, which dominates its edge), and
    /// the scheduler must keep it so. The sink pass used to count the φ's read
    /// in the φ's OWN block, took the join as the value's `late` block and —
    /// the join being the lowest-numbered shallower block on the dominator
    /// path — moved `-1 - held` into it, below the edge that reads it. It must
    /// still leave the loop, one block higher: the loop exit.
    #[test]
    fn r11w11_a_value_only_a_join_phi_reads_does_not_sink_into_the_join() {
        let (g, join, exit, sub) = r11w11_ternary_after_loop_graph();
        assert_eq!(dominance_violations(&g, false), no_findings(), "graph");
        let sched = schedule(&g);
        assert_eq!(
            sched.misplaced_reads(&g),
            Vec::<MisplacedRead>::new(),
            "post-schedule"
        );
        let at = |id: NodeId| sched.node_to_block[id as usize];
        assert_ne!(at(sub), at(join), "sunk into the φ's own block");
        assert_eq!(
            at(sub),
            at(exit),
            "the value should still sink out of the loop, to the loop exit"
        );
    }

    /// The same shape as bytecode, built and optimized — the tail of
    /// `R11SynccallDirectCallerLoop.mutator`, the one census hit of wave 10:
    /// `static int f(int n) { int s = 0, held = 0;
    ///   for (int i = 0; i < n; i++) { s += i; held += i & 1; }
    ///   return held == 0 ? s : -1 - held; }`
    #[test]
    fn r11w11_the_mutator_tail_schedules_with_no_misplaced_phi_input() {
        let code: [u8; 39] = [
            0x03, 0x3c, 0x03, 0x3d, 0x03, 0x3e, // 0: s = 0; held = 0; i = 0
            0x1d, 0x1a, 0xa2, 0x00, 0x13, // 6: iload_3; iload_0; if_icmpge 27
            0x1b, 0x1d, 0x60, 0x3c, // 11: s += i
            0x1c, 0x1d, 0x04, 0x7e, 0x60, 0x3d, // 15: held += i & 1
            0x84, 0x03, 0x01, // 21: iinc 3, 1
            0xa7, 0xff, 0xee, // 24: goto 6
            0x1c, 0x9a, 0x00, 0x07, // 27: iload_2; ifne 35
            0x1b, 0xa7, 0x00, 0x06, // 31: iload_1; goto 38
            0x02, 0x1c, 0x64, // 35: iconst_m1; iload_2; isub
            0xac, // 38: ireturn
        ];
        let Some(mut graph) = IrBuilder::new(1, 4).build(&code, 39) else {
            panic!("the mutator tail must build");
        };
        ir_optimize::optimize(&mut graph);
        assert_eq!(
            dominance_violations(&graph, false),
            no_findings(),
            "optimized"
        );
        assert_eq!(
            schedule_with_options(&graph, &production_schedule_options()).misplaced_reads(&graph),
            Vec::<MisplacedRead>::new(),
            "optimized, release form"
        );
    }

    // ── Round 11 wave 12 (lane ircore) ───────────────────────────────────

    /// irback proposal W11-1: the sink pass's own read check refuses the
    /// placement wave 11 fixed at its source. `-1 - held`, moved from the loop
    /// header into the join, sits below the edge its only reader (the join's
    /// φ) reads it on; moved to the loop exit, it dominates that edge.
    #[test]
    fn r11w12_the_read_check_refuses_a_value_sunk_below_its_phi_edge() {
        let (g, join, exit, sub) = r11w11_ternary_after_loop_graph();
        let sched = schedule(&g);
        let nb = sched.blocks.len();
        let at = |id: NodeId| sched.node_to_block[id as usize];
        let held = g.nodes[sub as usize].inputs[1];
        let header = at(held);
        assert_ne!(header, at(exit), "precondition: the φ heads the loop");

        // As if the pass had found `sub` in the header and moved it.
        let mut original = sched.node_to_block.clone();
        original[sub as usize] = header;

        let mut into_join = original.clone();
        into_join[sub as usize] = at(join);
        assert!(
            !sunk_reads_dominated(&g, &into_join, &original, &sched.dom, nb),
            "the join does not dominate the `nonzero` edge the φ reads on"
        );

        let mut into_exit = original.clone();
        into_exit[sub as usize] = at(exit);
        assert!(
            sunk_reads_dominated(&g, &into_exit, &original, &sched.dom, nb),
            "twin: the loop exit dominates both arms"
        );
        assert!(
            sunk_reads_dominated(&g, &original, &original, &sched.dom, nb),
            "twin: nothing moved, nothing owed"
        );
    }

    /// W11-1, the non-φ half: a value moved below a reader in its own block
    /// (the loop test's compare, below the `If` that reads it) is refused.
    #[test]
    fn r11w12_the_read_check_refuses_a_value_sunk_below_a_plain_reader() {
        let (g, _join, exit, _sub) = r11w11_ternary_after_loop_graph();
        let sched = schedule(&g);
        let nb = sched.blocks.len();
        let lt = g
            .nodes
            .iter()
            .position(|n| n.op == Op::Cmp(crate::ir::CmpOp::Lt))
            .expect("the loop test");
        let original = sched.node_to_block.clone();
        assert_ne!(
            original[lt],
            usize::MAX,
            "precondition: the compare is placed"
        );
        assert!(sunk_reads_dominated(
            &g, &original, &original, &sched.dom, nb
        ));

        let mut below = original.clone();
        below[lt] = sched.node_to_block[exit as usize];
        assert!(
            !sunk_reads_dominated(&g, &below, &original, &sched.dom, nb),
            "the loop exit does not dominate the header's `If`"
        );
    }

    /// irback proposal W11-3: equally shallow candidates are taken nearest
    /// `early`, not lowest-numbered. Block 3 is a join created before the loop
    /// exit (block 4) that dominates it, as `IrBuilder::build` pre-creates
    /// merges:
    ///
    /// ```text
    ///   0 -> 1 (header) <-> 2,  1 -> 4 (exit) -> 3 (join)
    /// ```
    #[test]
    fn r11w12_sink_ties_break_by_dominance_not_by_block_number() {
        let blocks = cfg(5, &[(0, 1), (1, 2), (2, 1), (1, 4), (4, 3)]);
        let dom = Dominators::compute(&blocks);
        let (depth, innermost) = loop_nest(&blocks, &dom);
        assert_eq!(depth, vec![0, 1, 1, 0, 0]);
        let mut path = Vec::new();

        // Depth-only: the first shallower block below the header is the exit.
        // Block order would have taken the join, which the exit dominates.
        assert_eq!(
            choose_sink_block(&dom, &depth, &innermost, 1, 3, false, 5, &mut path),
            4
        );
        assert_eq!(
            choose_sink_block_by_scan(&dom, &depth, &innermost, 1, 3, false, 5),
            4
        );
        // Equal-depth still walks on down to the reader's block.
        assert_eq!(
            choose_sink_block(&dom, &depth, &innermost, 1, 3, true, 5, &mut path),
            3
        );
        assert_eq!(
            choose_sink_block_by_scan(&dom, &depth, &innermost, 1, 3, true, 5),
            3
        );
    }
}
