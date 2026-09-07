// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Null-check and bounds-check elimination for the OPTIMIZING tier.
//!
//! # Why this module exists
//!
//! The single-pass backend has both of these and has had them for a long time:
//! `x64/bce.rs` is 3,819 lines of speculative bounds-check elimination fed by
//! `range_analysis` and `scev`, and `x64/null_check_elim.rs` plus the `this`
//! seed remove receiver null tests. Neither is reachable from the IR tier --
//! `range_analysis` is referenced by `x64/bce.rs` and nothing else, and
//! `null_check_elim` by `x64/*` and nothing else -- so
//! `ir_lower::emit_array_null_bounds_guards` emitted, unconditionally, at every
//! `ArrayLoad` and every `ArrayStore`:
//!
//! ```text
//! TEST RAX, RAX            ; the null check
//! JZ   <deopt>
//! MOV  R10D, [RAX + len]   ; the length load
//! CMP  ECX, R10D
//! JAE  <deopt>             ; the bounds check
//! ```
//!
//! Every element, every iteration, with no elision, no proof and no hoisting of
//! the length load. That is the tier that is supposed to be the OPTIMIZING one.
//!
//! # What this pass proves, and what it deliberately does not
//!
//! Two facts, both by DOMINANCE over the scheduled block CFG, which is the
//! cheapest sound form and needs no new analysis infrastructure:
//!
//! * **Non-null.** A value is non-null at `n` if something that dominates `n`
//!   already dereferenced it, or if it is the result of an allocation. If the
//!   value were null, the dominating dereference would have deopted and `n`
//!   would not be running.
//! * **In bounds.** A bounds check on the SSA pair `(base, index)` is redundant
//!   if a dominating node checked the SAME pair. In SSA neither operand can
//!   have been redefined in between, and a Java array's length is fixed for its
//!   lifetime, so the earlier check having passed is a proof that this one will.
//!
//! What it does NOT do is the interesting half of `x64/bce.rs`: proving
//! `0 <= i < a.length` for an induction variable from the loop's own trip
//! count, which removes the check on the FIRST iteration too. That wants an
//! SSA-level SCEV, which does not exist yet (`scev` is a bytecode analysis).
//! This pass removes the second and subsequent checks on the same pair, which
//! on `a[i] = a[i] + 1` and on any loop whose body touches an element more than
//! once is most of them.
//!
//! # Why dominance and not "earlier in the same block"
//!
//! Both, actually -- and the same-block case is the one that needs care.
//! `Schedule::dom` is a relation over BLOCKS, so it answers nothing about two
//! nodes in one block. Within a block the pass walks `Block::nodes` in order
//! and accumulates facts as it goes, which is sound because `ir_lower` emits a
//! block's nodes in exactly that order. That coupling is asserted rather than
//! assumed: see `the_block_node_order_is_the_emission_order`.
//!
//! # The three couplings this pass rests on, all checked
//!
//! Each is a property of code that lives elsewhere, so each is written down
//! here rather than remembered:
//!
//! 1. **`Schedule::dom` is indexed by POST-layout block numbers.** The pass
//!    queries it with post-layout indices, and frequency-driven layout permutes
//!    the block vector. `schedule_with_options` recomputes the matrix after a
//!    permutation for exactly this reason ("the dominator relation is a
//!    property of the CFG, not of its numbering") -- if it ever stopped, every
//!    dominance answer here would be read from the wrong row.
//! 2. **`ir_lower` emits a block by walking `Block::nodes` in order.** That is
//!    what makes the same-block accumulation program order rather than an
//!    arbitrary topological one. Pinned by
//!    `the_block_node_order_is_the_emission_order`.
//! 3. **Every op in [`deref_base_input`] takes its base at `inputs[2]`, and
//!    none of them CONTINUES with a null base.** `ArrayLoad`/`ArrayStore`/
//!    `ArrayLength` emit an explicit null test that deopts; `Load`/`Store`
//!    null-check the receiver or fault into the signal handler; the monitor
//!    ops compare the helper's `i64::MIN` sentinel and jump to the exception
//!    epilogue, so "a null receiver takes the NPE path" as their own comment
//!    says. If any of them ever returned normally on null, the fact this pass
//!    records past it would be false and the elision would be a wrong-code bug
//!    rather than a missed check.
//!
//! # Fail-closed
//!
//! Every unknown answers "check needed". An unplaced node (`usize::MAX` in
//! `node_to_block`), a node this pass has no arm for, a base that is a `Phi` --
//! all of them keep their check. The cost of a wrong `true` here is a null
//! dereference or an out-of-bounds read in compiled code; the cost of a wrong
//! `false` is two instructions.

use crate::ir::{CmpOp, Graph, MemKind, NodeId, Op};
use crate::ir_schedule::Schedule;
use rustc_hash::{FxHashMap, FxHashSet};

/// Per-node verdicts, indexed by `NodeId`.
#[derive(Default)]
pub struct CheckElision {
    /// `true` for a node whose array/receiver base is provably non-null.
    null_check_unneeded: Vec<bool>,
    /// `true` for a node whose `(base, index)` bounds check is provably
    /// redundant.
    bounds_check_unneeded: Vec<bool>,
    /// `true` for a node the RANGE pass proved, as opposed to the
    /// dominating-redundancy pass. Per-node rather than a running total
    /// because the process census is taken at EMISSION: `analyze` also runs
    /// for compiles whose body the acceptance gate later discards, and those
    /// must not show up as work that reached the workload.
    bounds_by_range: Vec<bool>,
}

impl CheckElision {
    /// Is the null check at `node` provably unnecessary?
    #[inline]
    pub fn null_elided(&self, node: NodeId) -> bool {
        self.null_check_unneeded
            .get(node as usize)
            .copied()
            .unwrap_or(false)
    }

    /// Is the bounds check at `node` provably unnecessary?
    #[inline]
    pub fn bounds_elided(&self, node: NodeId) -> bool {
        self.bounds_check_unneeded
            .get(node as usize)
            .copied()
            .unwrap_or(false)
    }

    /// Was the bounds check at `node` proven by the RANGE pass rather than by
    /// dominating redundancy? Only meaningful when [`Self::bounds_elided`] is
    /// also true.
    #[inline]
    pub fn bounds_range_proved(&self, node: NodeId) -> bool {
        self.bounds_by_range
            .get(node as usize)
            .copied()
            .unwrap_or(false)
    }

    /// `(null_elided, bounds_elided)` totals, for the census.
    pub fn counts(&self) -> (usize, usize) {
        (
            self.null_check_unneeded.iter().filter(|b| **b).count(),
            self.bounds_check_unneeded.iter().filter(|b| **b).count(),
        )
    }
}

/// Which input of `op` is the base being dereferenced, if any.
///
/// An allowlist, so an op this module has never heard of contributes no fact
/// rather than the wrong one. The index is into `Node::inputs`.
fn deref_base_input(op: &Op) -> Option<usize> {
    match op {
        // [ctrl, mem, array, index, ...]
        Op::ArrayLoad(_) | Op::ArrayStore(_) => Some(2),
        // [ctrl, mem, obj]
        Op::ArrayLength | Op::MonitorEnter | Op::MonitorExit => Some(2),
        // [ctrl, mem, base, ...] -- a field access.
        Op::Load(_) | Op::Store(_) => Some(2),
        _ => None,
    }
}

/// The `(base, index)` pair a bounds check is against, if this op has one.
fn bounds_pair(node_op: &Op, inputs: &[NodeId]) -> Option<(NodeId, NodeId)> {
    match node_op {
        Op::ArrayLoad(_) | Op::ArrayStore(_) => {
            Some((*inputs.get(2)?, *inputs.get(3)?))
        }
        _ => None,
    }
}

/// Does this node's result carry a value that cannot be null?
fn definitely_non_null(op: &Op) -> bool {
    matches!(
        op,
        // A successful allocation returns a non-null reference; a failed one
        // returns the deopt sentinel and never reaches a use.
        Op::New { .. }
            | Op::NewArray { .. }
            // The two constant-pool materializers either produce an object or
            // stash a pending exception and return the sentinel.
            | Op::ConstString { .. }
            | Op::ConstClass { .. }
    )
}

/// Run the analysis over a scheduled graph.
///
/// Linear in nodes times blocks in the worst case (the dominance query is an
/// array index), and the block count in this tier is small.
pub fn analyze(graph: &Graph, schedule: &Schedule) -> CheckElision {
    let n = graph.nodes.len();
    let mut out = CheckElision {
        null_check_unneeded: vec![false; n],
        bounds_check_unneeded: vec![false; n],
        bounds_by_range: vec![false; n],
    };
    if !enabled() {
        return out;
    }

    // Fact 1 (block-level): for each value, the set of blocks in which it is
    // known dereferenced. A later node is covered when ANY of those blocks
    // dominates the later node's block.
    let mut deref_blocks: FxHashMap<NodeId, Vec<usize>> = FxHashMap::default();
    // Fact 2 (block-level): the same for a checked `(base, index)` pair.
    let mut checked_blocks: FxHashMap<(NodeId, NodeId), Vec<usize>> = FxHashMap::default();

    // Values that are non-null by construction, and the receiver.
    //
    // The receiver is seeded for the same reason the single-pass tier seeds it
    // (`CRATONVM_JIT_THIS_NONNULL`): the JVM guarantees a non-null receiver at
    // the call site, so `this` is non-null on entry and stays so -- it is a
    // parameter and SSA gives it no other definition.
    let mut non_null: FxHashSet<NodeId> = FxHashSet::default();
    for (id, node) in graph.nodes.iter().enumerate() {
        if definitely_non_null(&node.op) {
            non_null.insert(id as NodeId);
        }
    }
    // `receiver_param` is a PARAMETER INDEX, not a `NodeId` -- the node is
    // whichever `Op::Param` carries that index. Reading it as a node id would
    // seed an arbitrary node non-null, which is a wrong-code bug rather than a
    // missed optimization, so the lookup is explicit.
    if let Some(recv_idx) = graph.receiver_param {
        for (id, node) in graph.nodes.iter().enumerate() {
            if matches!(node.op, Op::Param(p) if p == recv_idx) {
                non_null.insert(id as NodeId);
            }
        }
    }

    // The range facts, computed once over the whole graph. Independent of the
    // block walk below: a bounds check is proven by a DOMINATING branch, so
    // the order the walk reaches blocks in does not matter.
    //
    // Empty when the range pass alone is switched off, which leaves the
    // dominating-redundancy pass running. The two get separate switches
    // because they remove DIFFERENT checks -- redundancy only ever a second
    // access, range only ever a first -- and one switch covering both cannot
    // attribute a regression to either.
    let ranges = if range_enabled() {
        upper_bounds(graph, schedule)
    } else {
        Vec::new()
    };

    // One pass over the blocks in index order. Within a block, facts
    // accumulated by earlier nodes apply to later ones directly; across blocks
    // they apply through `dom`.
    for (b, block) in schedule.blocks.iter().enumerate() {
        // Facts established EARLIER IN THIS BLOCK. Kept separate from the
        // block-level maps so a fact does not appear to dominate itself.
        let mut local_deref: FxHashSet<NodeId> = FxHashSet::default();
        let mut local_checked: FxHashSet<(NodeId, NodeId)> = FxHashSet::default();

        for &id in &block.nodes {
            let Some(node) = graph.nodes.get(id as usize) else {
                continue;
            };

            if let Some(base_idx) = deref_base_input(&node.op) {
                let Some(&base) = node.inputs.get(base_idx) else {
                    continue;
                };
                let known = non_null.contains(&base)
                    || local_deref.contains(&base)
                    || deref_blocks
                        .get(&base)
                        .is_some_and(|bs| bs.iter().any(|&d| dominates(schedule, b, d)));
                if known {
                    out.null_check_unneeded[id as usize] = true;
                }
                // Whether or not the check was elided, reaching PAST this node
                // proves the base was non-null: an elided check means it was
                // already proven, and an emitted one deopts when it is not.
                local_deref.insert(base);
                deref_blocks.entry(base).or_default().push(b);
            }

            if let Some(pair) = bounds_pair(&node.op, &node.inputs) {
                let known = local_checked.contains(&pair)
                    || checked_blocks
                        .get(&pair)
                        .is_some_and(|bs| bs.iter().any(|&d| dominates(schedule, b, d)));
                if known {
                    out.bounds_check_unneeded[id as usize] = true;
                } else {
                    // Not redundant. Try to prove it dead outright: a
                    // dominating `idx < base.length` for the SAME array, plus
                    // `idx >= 0`. Both halves, always -- see the range notes.
                    let (base, idx) = pair;
                    let proved = ranges.iter().any(|ub| {
                        ub.idx == idx
                            && ub.base == base
                            && dominates_reflexive(schedule, b, ub.true_block)
                            && proven_non_negative(graph, schedule, idx, ub.true_block)
                    });
                    if proved {
                        out.bounds_check_unneeded[id as usize] = true;
                        out.bounds_by_range[id as usize] = true;
                    }
                }
                local_checked.insert(pair);
                checked_blocks.entry(pair).or_default().push(b);
            }
        }
    }
    out
}

/// Does block `d` dominate block `b`? Reflexive, so a block dominates itself --
/// which is why the same-block case is handled by `local_*` sets rather than by
/// this query: without that split, the FIRST dereference in a block would prove
/// itself.
#[inline]
fn dominates(schedule: &Schedule, b: usize, d: usize) -> bool {
    if b == d {
        // Same block: `local_deref` / `local_checked` own this case, in program
        // order. Answering `true` here would elide the first check in the
        // block on the strength of a later one.
        return false;
    }
    schedule
        .dom
        .get(b)
        .and_then(|row| row.get(d))
        .copied()
        .unwrap_or(false)
}

// ── Range-based bounds-check elimination ─────────────────────────────
//
// The dominance pass above removes the SECOND check on a pair; it can never
// remove the FIRST, which is why `bounds_elided=8` sat against
// `bounds_emitted=174` on H2. The overwhelming majority of Java array traffic
// is `for (int i = 0; i < a.length; i++) … a[i] …`, where NO check is needed
// at all — and that is what this recognises.
//
// A check at `(base, idx)` is dead when BOTH halves are proven:
//
//   * `idx <  base.length` — a dominating `If` on `idx < ArrayLength(base)`,
//     taken on its true edge. Pure dominance, sound on its own.
//   * `idx >= 0`           — a constant, another array length, or a counted
//     induction variable (below).
//
// BOTH are required because the source-level test is SIGNED. A negative `idx`
// satisfies `idx < len` and would index behind the object; eliding on the
// upper bound alone is a memory-safety bug, not a missed corner case.
//
// # Why the stride must be exactly 1
//
// For an induction phi `i = [region, init, i + s]` the non-negativity argument
// is: `init >= 0`, `s > 0`, so `i` only increases and never wraps. The "never
// wraps" half is the load-bearing one and it is NOT free.
//
// If the increment is guarded by `i < len`, then before it `i <= len - 1`, so
// after it `i <= len - 1 + s`. An array length can be as large as `i32::MAX`,
// so `len - 1 + s` overflows for any `s > 1` at the top of the range. A wrapped
// `i` is negative, a negative `i` PASSES the signed `i < len` test, and the
// loop then indexes with it — with the check eliminated. So the stride bound is
// not a heuristic here, it is the proof.
//
// With `s == 1`: `i <= len - 1` before, `i <= len <= i32::MAX` after. No
// overflow, for any array length the JVM can produce. That is the whole reason
// this is restricted to unit stride rather than to "a small constant" — a
// bound like `s <= 1024` would still be unsound against a near-`i32::MAX`
// array, and unit stride is the shape essentially every Java array loop has.
//
// # Why the guard must dominate the BACK EDGE
//
// It is not enough that the test dominates the ACCESS. `for (int i = 0; ; i++)`
// with an inner `if (i < a.length) a[i] = …` also has a test dominating the
// access, but nothing stops `i` running to `i32::MAX` and wrapping — after
// which the access's own test passes on a negative index. Requiring the test to
// dominate every back edge into the loop header is what rules that out: then no
// iteration happens without `i < len` having held, so `i` never gets past `len`
// in the first place.

/// An `idx < ArrayLength(base)` fact and the block it holds in.
struct UpperBound {
    idx: NodeId,
    base: NodeId,
    /// Block entered on the edge where `idx < base.length` holds -- the
    /// branch's TRUE edge for `Lt`/`Gt`, its FALSE edge for `Ge`/`Le`.
    true_block: usize,
}

/// `dom[b][d]`, but reflexive — a block dominates itself.
///
/// The redundancy pass deliberately answers `false` for `b == d` so a check
/// cannot prove itself; the range pass needs the opposite, because an access
/// sitting in the very block a branch guards IS covered by it (the test runs in
/// the predecessor's terminator, before any node of the true block).
#[inline]
fn dominates_reflexive(schedule: &Schedule, b: usize, d: usize) -> bool {
    b == d
        || schedule
            .dom
            .get(b)
            .and_then(|row| row.get(d))
            .copied()
            .unwrap_or(false)
}

/// Collect every branch that proves `idx < ArrayLength(base)` on one of its
/// edges.
///
/// BOTH edges are examined, and that is not thoroughness -- it is the
/// difference between working and not. `IrBuilder` keeps the bytecode's own
/// branch polarity: `if_icmpge` becomes `Cmp(Ge)` with `Proj(0)` going to the
/// BRANCH TARGET, so for the top-tested loop javac emits as
/// `if_icmpge exit` the body hangs off the FALSE edge and the comparison reads
/// `Ge`, not `Lt`. Matching only `Lt`-on-true finds the bottom-tested shape
/// and silently misses the other one.
///
/// Per edge, the facts that give `idx < len`:
///
/// | edge  | comparison        | why |
/// |-------|-------------------|-----|
/// | true  | `Lt[idx, len]`    | directly |
/// | true  | `Gt[len, idx]`    | the same fact written backwards |
/// | false | `Ge[idx, len]`    | negation of `idx >= len` |
/// | false | `Le[len, idx]`    | negation of `len <= idx` |
///
/// `Eq`/`Ne` prove nothing about ordering, and a non-strict bound on the
/// TAKEN side (`Le[idx, len]`) still admits `idx == len`.
fn upper_bounds(graph: &Graph, schedule: &Schedule) -> Vec<UpperBound> {
    // Which block does each edge of each `If` open? `Block::ctrl` is the
    // control node starting the block, and for a branch successor that is the
    // `Proj`. When an edge lands straight on a `Merge` instead (an empty arm),
    // no block is found and that edge contributes no fact.
    let mut edge_block: FxHashMap<(NodeId, u8), usize> = FxHashMap::default();
    for (b, block) in schedule.blocks.iter().enumerate() {
        let Some(ctrl) = graph.nodes.get(block.ctrl as usize) else {
            continue;
        };
        let Op::Proj(which) = ctrl.op else { continue };
        if which > 1 {
            continue;
        }
        let Some(&owner) = ctrl.inputs.first() else {
            continue;
        };
        if matches!(graph.nodes.get(owner as usize).map(|n| &n.op), Some(Op::If)) {
            edge_block.insert((owner, which), b);
        }
    }

    let mut out = Vec::new();
    for (id, node) in graph.nodes.iter().enumerate() {
        if !matches!(node.op, Op::If) {
            continue;
        }
        let Some(&cond_id) = node.inputs.get(1) else {
            continue;
        };
        let Some(cond) = graph.nodes.get(cond_id as usize) else {
            continue;
        };
        // `Op::Cmp` is `[a, b]` with no control input. See the table above for
        // which (edge, comparison) pairs prove `idx < len`; a non-strict bound
        // on the taken side would leave `idx == len`, which is precisely the
        // off-by-one the check exists to catch.
        let (idx, len_id, edge) = match (&cond.op, cond.inputs.as_slice()) {
            (Op::Cmp(CmpOp::Lt), [a, b]) => (*a, *b, 0u8),
            (Op::Cmp(CmpOp::Gt), [a, b]) => (*b, *a, 0u8),
            (Op::Cmp(CmpOp::Ge), [a, b]) => (*a, *b, 1u8),
            (Op::Cmp(CmpOp::Le), [a, b]) => (*b, *a, 1u8),
            _ => continue,
        };
        let Some(len) = graph.nodes.get(len_id as usize) else {
            continue;
        };
        if !matches!(len.op, Op::ArrayLength) {
            continue;
        }
        // `Op::ArrayLength` is `[ctrl, mem, array_ref]`.
        let Some(&base) = len.inputs.get(2) else {
            continue;
        };
        let Some(&true_block) = edge_block.get(&(id as NodeId, edge)) else {
            continue;
        };
        out.push(UpperBound {
            idx,
            base,
            true_block,
        });
    }
    out
}

/// Is `v` provably `>= 0` at every point the guard at `guard_block` covers?
///
/// `guard_block` is the true edge of the branch that established the upper
/// bound: an induction variable's non-negativity is proven THROUGH that same
/// branch (see the module notes above), so the two halves cannot be decided
/// independently.
fn proven_non_negative(
    graph: &Graph,
    schedule: &Schedule,
    v: NodeId,
    guard_block: usize,
) -> bool {
    let Some(node) = graph.nodes.get(v as usize) else {
        return false;
    };
    match &node.op {
        Op::Const(c) => *c >= 0,
        // A JVM array length is non-negative by construction: `newarray` with a
        // negative count throws before the length can ever be read.
        Op::ArrayLength => true,
        Op::Phi => is_unit_stride_induction(graph, schedule, v, node, guard_block),
        _ => false,
    }
}

/// `[region, const_init, phi + 1]`, with the increment guarded by `guard_block`.
fn is_unit_stride_induction(
    graph: &Graph,
    schedule: &Schedule,
    phi_id: NodeId,
    phi: &crate::ir::Node,
    guard_block: usize,
) -> bool {
    // `Op::Phi` is `[merge, val_0, val_1, …]`. A loop phi has exactly two
    // values: the entry value and the back-edge value.
    let (region_id, init_id, back_id) = match phi.inputs.as_slice() {
        [r, i, b] => (*r, *i, *b),
        _ => return false,
    };
    // BOTH ops, and `Op::Merge` is the one that matters. `IrBuilder` creates
    // every merge point through `ensure_merge`, which builds an `Op::Merge`;
    // `activate_loop_header` then fills in its inputs and phis but never
    // rewrites the op. So a loop header from real bytecode is a `Merge`, and
    // `Op::Region` appears only in hand-built graphs -- including this
    // module's own tests, which is exactly how a version of this pass that
    // accepted only `Region` passed every unit test while doing nothing
    // whatsoever to a compiled method.
    //
    // Accepting both costs no soundness: what makes this a loop is the
    // BACK EDGE found in the CFG below, not the op. A forward `Merge` has no
    // predecessor it dominates, so `saw_back_edge` stays false and it is
    // refused.
    if !matches!(
        graph.nodes.get(region_id as usize).map(|n| &n.op),
        Some(Op::Merge) | Some(Op::Region)
    ) {
        return false;
    }
    // The entry value must itself be non-negative. Only the CONSTANT case is
    // accepted: anything else would recurse, and a phi whose entry value is
    // another phi is not a shape this pass claims to understand.
    let init_ok = matches!(
        graph.nodes.get(init_id as usize).map(|n| &n.op),
        Some(Op::Const(c)) if *c >= 0
    );
    if !init_ok {
        return false;
    }
    // The back-edge value must be exactly `phi + 1`. See the module notes for
    // why the stride is pinned to one rather than bounded by a constant.
    let Some(back) = graph.nodes.get(back_id as usize) else {
        return false;
    };
    if !matches!(back.op, Op::Add) {
        return false;
    }
    let is_one = |n: NodeId| {
        matches!(
            graph.nodes.get(n as usize).map(|x| &x.op),
            Some(Op::Const(1))
        )
    };
    let unit_step = match back.inputs.as_slice() {
        [a, b] => (*a == phi_id && is_one(*b)) || (*b == phi_id && is_one(*a)),
        _ => false,
    };
    if !unit_step {
        return false;
    }

    // Every back edge into the header must pass through the guard. Without
    // this the loop can iterate — and increment — on a path where the test
    // never ran, which is how an induction variable reaches `i32::MAX` and
    // wraps negative.
    let Some(&header_block) = schedule.node_to_block.get(region_id as usize) else {
        return false;
    };
    let Some(header) = schedule.blocks.get(header_block) else {
        return false;
    };
    let mut saw_back_edge = false;
    for &p in &header.predecessors {
        // A back edge comes from inside the loop, which is exactly the set of
        // blocks the header dominates.
        if !dominates_reflexive(schedule, p, header_block) {
            continue;
        }
        saw_back_edge = true;
        if !dominates_reflexive(schedule, p, guard_block) {
            return false;
        }
    }
    // Fail closed: a "loop" with no back edge is not a loop, and a phi that
    // references itself without one is a graph this pass should not reason
    // about.
    saw_back_edge
}

/// **Default ON** since 2026-09-06. `CRATONVM_JIT_IR_BCE_RANGE=0` keeps the
/// dominating-redundancy pass and drops only the range proof, so a bisect can
/// separate the two; `CRATONVM_JIT_IR_CHECK_ELIM=0` drops both.
pub fn range_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_IR_BCE_RANGE").as_deref(),
            Ok("0") | Ok("false") | Ok("off") | Ok("no")
        )
    })
}

/// **Default ON** since 2026-09-06. `CRATONVM_JIT_IR_CHECK_ELIM=0` restores the
/// unconditional guards at every array site, which is every build before this
/// module existed.
pub fn enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_IR_CHECK_ELIM").as_deref(),
            Ok("0") | Ok("false") | Ok("off") | Ok("no")
        )
    })
}

/// Null checks and bounds checks elided in the optimizing tier, process-wide.
///
/// Both counters always printed together with the EMITTED counts by the census
/// caller: an elision count on its own cannot distinguish "the pass works" from
/// "this workload compiles no array accesses in this tier", and that
/// distinction has cost this file wrong conclusions before.
static ELIDED: [std::sync::atomic::AtomicU64; 2] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];
static EMITTED: [std::sync::atomic::AtomicU64; 2] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// Record one guard-pair decision. `which` is 0 for the null check, 1 for the
/// bounds check.
/// Bounds checks the RANGE pass proved, counted at emission.
static RANGE_PROVED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Count one range-proved elision. Called from the emitter, alongside
/// [`note_check`], so a discarded compile contributes nothing.
pub fn note_range_proved() {
    RANGE_PROVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// How many elided bounds checks the range pass proved, of `census().2`.
pub fn range_census() -> u64 {
    RANGE_PROVED.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn note_check(which: usize, elided: bool) {
    let table = if elided { &ELIDED } else { &EMITTED };
    table[which].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if elided {
        crate::ir_evidence::note(crate::ir_evidence::Transform::GuardElided);
    }
}

/// `(null_elided, null_emitted, bounds_elided, bounds_emitted)`.
pub fn census() -> (u64, u64, u64, u64) {
    use std::sync::atomic::Ordering::Relaxed;
    (
        ELIDED[0].load(Relaxed),
        EMITTED[0].load(Relaxed),
        ELIDED[1].load(Relaxed),
        EMITTED[1].load(Relaxed),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{IrType, NO_NODE};
    use crate::ir_schedule;

    fn empty_graph() -> Graph {
        Graph {
            nodes: Vec::new(),
            entry: 0,
            exit: NO_NODE,
            safepoints: Vec::new(),
            uses: Default::default(),
            receiver_param: None,
        }
    }

    /// `a[i] + a[i]` -- one array, one index, two loads in one block. The
    /// SECOND load's checks are both redundant; the first load's are not.
    ///
    /// This is the shape the pass exists for and the one a dominance-only
    /// implementation would miss entirely, because both nodes are in the same
    /// block and `dom` is reflexive.
    #[test]
    fn the_second_access_to_the_same_element_needs_no_checks() {
        let mut g = empty_graph();
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let arr = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let idx = g.add(Op::Param(1), IrType::Int, vec![start], None);
        let l1 = g.add(
            Op::ArrayLoad(MemKind::Int),
            IrType::Int,
            vec![ctrl, mem, arr, idx],
            Some(0),
        );
        let l2 = g.add(
            Op::ArrayLoad(MemKind::Int),
            IrType::Int,
            vec![ctrl, mem, arr, idx],
            Some(1),
        );
        let sum = g.add(Op::Add, IrType::Int, vec![l1, l2], Some(2));
        g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, sum], Some(3));

        let sched = ir_schedule::schedule(&g);
        let e = analyze(&g, &sched);
        assert!(
            !e.null_elided(l1) && !e.bounds_elided(l1),
            "the FIRST access proves the facts; it cannot use them",
        );
        assert!(
            e.null_elided(l2),
            "the second access's base was dereferenced by the first",
        );
        assert!(
            e.bounds_elided(l2),
            "the second access checks the same (base, index) SSA pair",
        );
    }

    /// A DIFFERENT index against the same array keeps its bounds check and
    /// loses only the null check. Getting this wrong is an out-of-bounds read
    /// in compiled code, so it gets its own test rather than riding on the one
    /// above.
    #[test]
    fn a_different_index_keeps_its_bounds_check() {
        let mut g = empty_graph();
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let arr = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let i = g.add(Op::Param(1), IrType::Int, vec![start], None);
        let j = g.add(Op::Param(2), IrType::Int, vec![start], None);
        let l1 = g.add(
            Op::ArrayLoad(MemKind::Int),
            IrType::Int,
            vec![ctrl, mem, arr, i],
            Some(0),
        );
        let l2 = g.add(
            Op::ArrayLoad(MemKind::Int),
            IrType::Int,
            vec![ctrl, mem, arr, j],
            Some(1),
        );
        let sum = g.add(Op::Add, IrType::Int, vec![l1, l2], Some(2));
        g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, sum], Some(3));

        let sched = ir_schedule::schedule(&g);
        let e = analyze(&g, &sched);
        assert!(
            e.null_elided(l2),
            "the array is the same value and was already dereferenced",
        );
        assert!(
            !e.bounds_elided(l2),
            "a DIFFERENT index is a different bounds question",
        );
    }

    /// A freshly allocated array is non-null by construction, so even its
    /// first access needs no null check -- but it still needs its bounds check,
    /// because nothing here knows the length.
    #[test]
    fn a_fresh_allocation_needs_no_null_check_and_still_needs_bounds() {
        let mut g = empty_graph();
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let len = g.add(Op::Const(4), IrType::Int, vec![], None);
        let arr = g.add(
            Op::NewArray {
                element_type: 10,
                component_class_id: 0,
            },
            IrType::Ref,
            vec![ctrl, mem, len],
            Some(0),
        );
        let idx = g.add(Op::Const(0), IrType::Int, vec![], None);
        let l = g.add(
            Op::ArrayLoad(MemKind::Int),
            IrType::Int,
            vec![ctrl, arr, arr, idx],
            Some(1),
        );
        g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, l], Some(2));

        let sched = ir_schedule::schedule(&g);
        let e = analyze(&g, &sched);
        assert!(e.null_elided(l), "an allocation result cannot be null");
        assert!(
            !e.bounds_elided(l),
            "nothing here proves the index is inside the length",
        );
    }

    /// The kill switch must produce the pre-change answer at every site, or the
    /// A/B compares two things.
    #[test]
    fn the_kill_switch_elides_nothing() {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_CHECK_ELIM", Some("0"))],
            || {
                if enabled() {
                    // The gate is a process-wide `OnceLock`; another test won
                    // the race to initialise it. Assert nothing rather than
                    // assert something false.
                    return;
                }
                let mut g = empty_graph();
                let start = g.add(Op::Start, IrType::Control, vec![], None);
                g.entry = start;
                let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
                let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
                let arr = g.add(Op::Param(0), IrType::Ref, vec![start], None);
                let idx = g.add(Op::Param(1), IrType::Int, vec![start], None);
                let l1 = g.add(
                    Op::ArrayLoad(MemKind::Int),
                    IrType::Int,
                    vec![ctrl, mem, arr, idx],
                    Some(0),
                );
                let l2 = g.add(
                    Op::ArrayLoad(MemKind::Int),
                    IrType::Int,
                    vec![ctrl, mem, arr, idx],
                    Some(1),
                );
                let sum = g.add(Op::Add, IrType::Int, vec![l1, l2], Some(2));
                g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, sum], Some(3));
                let sched = ir_schedule::schedule(&g);
                let e = analyze(&g, &sched);
                assert!(!e.null_elided(l2) && !e.bounds_elided(l2));
            },
        );
    }

    /// The pass reads `Block::nodes` in order and treats that as PROGRAM order
    /// within the block. `ir_lower` emits a block by iterating the same vector,
    /// so the two agree -- but only for as long as nobody changes one of them.
    ///
    /// Asserting it on the source is the cheapest way to keep the coupling
    /// visible: a behavioural test cannot distinguish "the orders agree" from
    /// "this graph happens not to care".

    /// Build `for (int i = 0; i CMP a.length; i += stride) a[i] = 1;` and
    /// return `(graph, schedule, the ArrayStore)`.
    ///
    /// `guard_back_edge` chooses between the two loop shapes the range proof
    /// distinguishes. `true` is the ordinary `for` loop, where the bounds test
    /// IS the loop test and nothing re-enters the header without passing it.
    /// `false` wraps the test in an inner `if` under an unconditional loop --
    /// `for (int i = 0; ; i++) if (c) if (i < a.length) a[i] = 1;` -- where the
    /// test still dominates the ACCESS but the increment runs on a path that
    /// never took it.
    fn counted_loop(
        cmp_op: CmpOp,
        stride: i64,
        guard_back_edge: bool,
        bound_is_same_array: bool,
        init_is_const: bool,
    ) -> (Graph, crate::ir_schedule::Schedule, NodeId) {
        counted_loop_with_header(
            Op::Region,
            cmp_op,
            0,
            stride,
            guard_back_edge,
            bound_is_same_array,
            init_is_const,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn counted_loop_with_header(
        header: Op,
        cmp_op: CmpOp,
        // Which `Proj` of the bounds branch opens the loop BODY. `0` is the
        // bottom-tested `if_icmplt body` shape; `1` is the top-tested
        // `if_icmpge exit` shape, where the body is the fall-through.
        body_edge: u8,
        stride: i64,
        guard_back_edge: bool,
        bound_is_same_array: bool,
        init_is_const: bool,
    ) -> (Graph, crate::ir_schedule::Schedule, NodeId) {
        let mut g = empty_graph();
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let arr = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let other = g.add(Op::Param(1), IrType::Ref, vec![start], None);

        // Header, back edge patched in once the body exists.
        let region = g.add(header, IrType::Control, vec![c0], None);
        let init = if init_is_const {
            g.add(Op::Const(0), IrType::Int, vec![], None)
        } else {
            g.add(Op::Param(2), IrType::Int, vec![start], None)
        };
        let iv = g.add(Op::Phi, IrType::Int, vec![region, init], None);
        let step = g.add(Op::Const(stride), IrType::Int, vec![], None);
        let iv_next = g.add(Op::Add, IrType::Int, vec![iv, step], None);
        g.nodes[iv as usize].inputs.push(iv_next);

        // The array whose length bounds the loop -- the one being indexed, or
        // a different one.
        let bound_arr = if bound_is_same_array { arr } else { other };
        // `Op::ArrayLength` is [ctrl, mem, array_ref].
        let len = g.add(
            Op::ArrayLength,
            IrType::Int,
            vec![region, m0, bound_arr],
            None,
        );

        let (body_ctrl, back_ctrl) = if guard_back_edge {
            // region -> If(i < len) -> true = body, and the body IS the back
            // edge. Nothing reaches the header without the test.
            let cmp = g.add(Op::Cmp(cmp_op), IrType::Int, vec![iv, len], None);
            let iff = g.add(Op::If, IrType::Control, vec![region, cmp], None);
            let t = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
            let f = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
            let body = if body_edge == 0 { t } else { f };
            (body, body)
        } else {
            // region -> If(c) -> true -> If(i < len) -> true = body.
            // The BACK EDGE is the outer false edge, which the bounds test
            // does not dominate.
            let c = g.add(Op::Param(3), IrType::Int, vec![start], None);
            let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
            let outer_cmp = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![c, zero], None);
            let outer = g.add(Op::If, IrType::Control, vec![region, outer_cmp], None);
            let ot = g.add(Op::Proj(0), IrType::Control, vec![outer], None);
            let of = g.add(Op::Proj(1), IrType::Control, vec![outer], None);
            let cmp = g.add(Op::Cmp(cmp_op), IrType::Int, vec![iv, len], None);
            let iff = g.add(Op::If, IrType::Control, vec![ot, cmp], None);
            let t = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
            let _tf = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
            (t, of)
        };
        g.nodes[region as usize].inputs.push(back_ctrl);

        // `a[i] = 1` -- a STORE, so nothing can treat it as dead.
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let st = g.add(
            Op::ArrayStore(MemKind::Int),
            IrType::Void,
            vec![body_ctrl, m0, arr, iv, one],
            Some(0),
        );
        g.exit = g.add(Op::Return, IrType::Void, vec![body_ctrl, iv], Some(1));

        let sched = ir_schedule::schedule(&g);
        (g, sched, st)
    }

    /// The shape this pass exists for: `for (int i = 0; i < a.length; i++)`.
    /// No check is needed at all, and no earlier access proves it -- which is
    /// exactly what the dominating-redundancy pass could never do.
    #[test]
    fn the_classic_counted_loop_needs_no_bounds_check() {
        let (g, sched, st) = counted_loop(CmpOp::Lt, 1, true, true, true);
        let e = analyze(&g, &sched);
        assert!(
            e.bounds_elided(st),
            "i is a unit-stride induction variable from 0 and the loop test is \
             i < a.length for the SAME array, so 0 <= i < a.length holds",
        );
        assert!(
            e.bounds_range_proved(st),
            "and it was the RANGE pass that proved it, not redundancy -- there \
             is no earlier access to be redundant with",
        );
    }

    /// The SAME loop with the header op `IrBuilder` actually produces.
    ///
    /// `ensure_merge` builds every merge point -- loop headers included -- as
    /// `Op::Merge`; `activate_loop_header` fills in its inputs and never
    /// rewrites the op, so `Op::Region` never appears in a graph built from
    /// bytecode. A version of this pass that matched only `Op::Region` passed
    /// the test above and every other test in this module, and would have
    /// eliminated exactly zero bounds checks in a compiled method. This is the
    /// test that fails in that case.
    #[test]
    fn the_header_op_the_builder_actually_emits_is_recognised() {
        let (g, sched, st) =
            counted_loop_with_header(Op::Merge, CmpOp::Lt, 0, 1, true, true, true);
        let e = analyze(&g, &sched);
        assert!(
            e.bounds_elided(st) && e.bounds_range_proved(st),
            "a loop header is an Op::Merge in every graph the IR builder              produces; matching only Op::Region makes this pass inert on real              code while its hand-built tests stay green",
        );
    }

    /// The top-tested shape, where the useful fact is on the FALSE edge.
    ///
    /// javac compiles a `while`/`for` head as `if_icmpge exit`: branch OUT when
    /// the index has reached the length, fall through into the body. The
    /// builder keeps that polarity, so the comparison is `Ge` and the body is
    /// `Proj(1)`. A pass that looked only for `Lt` on `Proj(0)` would find
    /// nothing here while every other test in this module stayed green.
    #[test]
    fn the_bound_is_read_off_the_false_edge_of_a_negated_test() {
        let (g, sched, st) =
            counted_loop_with_header(Op::Merge, CmpOp::Ge, 1, 1, true, true, true);
        let e = analyze(&g, &sched);
        assert!(
            e.bounds_elided(st) && e.bounds_range_proved(st),
            "not (i >= a.length) is i < a.length, and that is the edge the loop              body actually hangs off in javac output",
        );
    }

    /// A stride of two is REFUSED, and not because two is unusual.
    ///
    /// The non-negativity proof runs: the guard gives `i <= len - 1`, so after
    /// the increment `i <= len - 1 + stride`. `len` can be `i32::MAX`, so any
    /// stride above one overflows there, and a wrapped `i` is negative -- which
    /// passes the SIGNED `i < len` test and then indexes behind the object with
    /// the check removed. Unit stride is the largest step for which the proof
    /// closes without knowing `len`.
    #[test]
    fn a_stride_the_proof_cannot_close_keeps_its_check() {
        let (g, sched, st) = counted_loop(CmpOp::Lt, 2, true, true, true);
        let e = analyze(&g, &sched);
        assert!(
            !e.bounds_elided(st),
            "a non-unit stride can carry the induction variable past len and \
             into overflow; the check stays",
        );
    }

    /// The test dominating the ACCESS is not enough: the increment must be
    /// dominated too. `for (int i = 0; ; i++) if (c) if (i < a.length) a[i]=1;`
    /// has a bounds test on every access and still lets `i` run to `i32::MAX`
    /// and wrap, after which that same test passes on a negative index.
    #[test]
    fn a_back_edge_that_skips_the_test_keeps_its_check() {
        let (g, sched, st) = counted_loop(CmpOp::Lt, 1, false, true, true);
        let e = analyze(&g, &sched);
        assert!(
            !e.bounds_elided(st),
            "the increment is reachable without the bounds test having held, \
             so nothing bounds the induction variable",
        );
    }

    /// `i < b.length` says nothing about `a[i]`. The pass matches the array
    /// SSA node, so two arrays that happen to be the same length are still two
    /// arrays.
    #[test]
    fn a_bound_taken_from_a_different_array_proves_nothing() {
        let (g, sched, st) = counted_loop(CmpOp::Lt, 1, true, false, true);
        let e = analyze(&g, &sched);
        assert!(
            !e.bounds_elided(st),
            "the loop is bounded by another array's length",
        );
    }

    /// `i <= a.length` leaves `i == a.length` reachable, which is the exact
    /// off-by-one the check exists to catch.
    #[test]
    fn a_non_strict_bound_is_not_a_bound() {
        let (g, sched, st) = counted_loop(CmpOp::Le, 1, true, true, true);
        let e = analyze(&g, &sched);
        assert!(!e.bounds_elided(st), "i <= len still admits i == len");
    }

    /// A parameter start could be negative, and a negative index PASSES the
    /// signed loop test. The upper bound alone is never enough.
    #[test]
    fn an_induction_variable_from_an_unknown_start_keeps_its_check() {
        let (g, sched, st) = counted_loop(CmpOp::Lt, 1, true, true, false);
        let e = analyze(&g, &sched);
        assert!(
            !e.bounds_elided(st),
            "nothing proves the start is non-negative, and i < len does not",
        );
    }

    #[test]
    fn the_block_node_order_is_the_emission_order() {
        let src = include_str!("ir_lower.rs");
        assert!(
            src.contains("for &node_id in &block.nodes")
                || src.contains("for &id in &block.nodes")
                || src.contains("block.nodes.iter()"),
            "ir_lower must still emit a block by walking `block.nodes` in order; \
             this pass's same-block reasoning depends on it",
        );
    }
}
