// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Null-check and bounds-check elimination for the OPTIMIZING tier.
//!
//! # Why this module exists
//!
//! The single-pass backend has both of these and has had them for a long time:
//! `x64/bce.rs` is speculative bounds-check elimination fed by `scev` and
//! `loop_analysis` (it uses only `range_analysis`'s `Range` TYPE, in its
//! default-off `range_safe_pcs`), and `x64/null_check_elim.rs` plus the `this`
//! seed remove receiver null tests. Neither was reachable from the IR tier, so
//! before this module `ir_lower::emit_array_null_bounds_guards` emitted,
//! unconditionally, at every `ArrayLoad` and every `ArrayStore`:
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
//! …plus one that is not dominance: **a value range.** The `idx >= 0` half of
//! the range proof consults `range_analysis::analyze_graph`, the SSA interval
//! lattice, which until 2026-09-17 had no call site in the crate at all — so
//! this tier had no value-range analysis and answered `idx >= 0` from three
//! hand-written shapes. It is an extra arm, not a replacement. Since B3
//! (2026-09-17) it is the PATH-SENSITIVE form (`analyze_graph_pathwise`, which
//! narrows values along the `If` edges that reach a block), but it still
//! refuses to recompute a phi, so the unit-stride rule remains the one that
//! proves an induction variable, and it cannot see `(char) p`.
//!
//! The range proof — a dominating `idx < a.length` for the SAME array plus
//! `idx >= 0` — removes the FIRST check too (this paragraph used to say that
//! needed an SSA-level SCEV; the unit-stride rule does it for `i = 0; i <
//! a.length; i++`). Since round 11 wave 6 a bound that is not this array's own
//! length also counts when it is PROVEN at most that length: `b = new int[n]`
//! bounds `i < n` for `b`, and a dominating `n <= a.length` (an `If` edge or a
//! `Guard`) bounds it for `a` (see `upper_bounds`). Since wave 8 a non-strict
//! `i <= x` counts when `x` is STRICTLY below the length, and so does a
//! decreasing induction variable whose start is (`a.length - 1`, or a fact
//! such as the pre-header guard `ir_optimize`'s range predication plants).
//! What remains out of reach is an unrelated bound with no such fact and an
//! `i != a.length` exit.
//!
//! Non-null facts come from dominating dereferences, allocations, the receiver,
//! and (round 11) a dominating `Guard(Cmp(Ne, x, null))`, which deopts on null
//! just as an emitted dereference check does.
//!
//! # Why dominance and not "earlier in the same block"
//!
//! Both, actually -- and the same-block case is the one that needs care.
//! `Schedule::dom` (a `Dominators`) is a relation over BLOCKS, so it answers nothing about two
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
//!    the block vector. `schedule_with_options` recomputes the relation after a
//!    permutation for exactly this reason ("the dominator relation is a
//!    property of the CFG, not of its numbering") -- if it ever stopped, every
//!    dominance answer here would be read from the wrong row.
//! 2. **`ir_lower` emits a block by walking `Block::nodes` in order.** That is
//!    what makes the same-block accumulation program order rather than an
//!    arbitrary topological one. Pinned by
//!    `the_block_node_order_is_the_emission_order`.
//! 3. **Every op in [`deref_base_input`] takes its base at `inputs[2]`, and
//!    none of them CONTINUES with a null base.** `ArrayLoad`/`ArrayStore`/
//!    `ArrayLength` emit an explicit null test that deopts; the monitor
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

mod root_ref_arrays;

/// Why the range pass declined a bounds check. Ordered from "nothing to work
/// with" towards "one conjunct short", so the census reads as a work list: the
/// codes near the bottom are the ones a modest extension could recover.
const REFUSAL_NONE: u8 = 0;
/// The `CRATONVM_DBG_BCE_REFUSALS` line's placeholder for "no induction".
// Removed unused `NO_NODE_DBG`.
/// The index is never compared against ANY array length in this graph. No
/// range pass can help; the access is not in a length-guarded region.
const REFUSAL_NO_LENGTH_TEST: u8 = 1;
/// The index IS length-tested, but only against a different array. Recovering
/// these needs an aliasing or equal-length fact, which this tier does not have.
const REFUSAL_OTHER_ARRAY: u8 = 2;
/// Right index, right array, but the guard does not dominate the access --
/// the test is on a path that does not reach here.
const REFUSAL_NOT_DOMINATING: u8 = 3;
/// Everything holds except `idx >= 0`, and the index is a PHI: a real loop
/// whose induction shape the unit-stride or back-edge rule rejected. This is
/// the bucket that relaxing those rules would recover.
const REFUSAL_IV_SHAPE: u8 = 4;
/// Everything holds except `idx >= 0`, and the index is not a phi at all --
/// a computed index needing a value-range fact this pass does not compute.
const REFUSAL_NOT_NON_NEGATIVE: u8 = 5;

/// Human name for a `REFUSAL_*` code.
fn refusal_name(code: u8) -> &'static str {
    match code {
        REFUSAL_NO_LENGTH_TEST => "no-length-test",
        REFUSAL_OTHER_ARRAY => "other-array",
        REFUSAL_NOT_DOMINATING => "guard-not-dominating",
        REFUSAL_IV_SHAPE => "iv-shape-rejected",
        REFUSAL_NOT_NON_NEGATIVE => "index-not-non-negative",
        _ => "none",
    }
}

/// Per-node verdicts, indexed by `NodeId`.
#[derive(Default)]
pub struct CheckElision {
    /// `true` for a node whose array/receiver base is provably non-null.
    null_check_unneeded: Vec<bool>,
    /// `true` for a node whose `(base, index)` bounds check is provably
    /// redundant.
    bounds_check_unneeded: Vec<bool>,
    /// Why the range pass could NOT prove this node, as a `REFUSAL_*` code.
    /// Recorded per node and counted at EMISSION for the same reason
    /// `bounds_by_range` is: `analyze` also runs for compiles the acceptance
    /// gate later discards.
    ///
    /// It exists because "extend the range pass" is not one decision but four,
    /// and the four have very different costs. Guessing which conjunct blocks
    /// the remaining checks is exactly the "measure the component before
    /// building the optimisation" mistake.
    bounds_refusal: Vec<u8>,
    /// `true` for a node the RANGE pass proved, as opposed to the
    /// dominating-redundancy pass. Per-node rather than a running total
    /// because the process census is taken at EMISSION: `analyze` also runs
    /// for compiles whose body the acceptance gate later discards, and those
    /// must not show up as work that reached the workload.
    bounds_by_range: Vec<bool>,
    /// JIT round 19 wave 5 (lane osr5, IROPS3-2): `true` for a node the
    /// clamp arm (`clamp_index_needs`) proved, a subset of `bounds_by_range`;
    /// counted at emission like it ([`note_clamp_proved`]).
    bounds_by_clamp: Vec<bool>,
    /// JIT round 21 (lane charat, STRINTR21-5,
    /// `CRATONVM_JIT_IR_GUARD_FACT_ELIDE`): `true` for an `Op::Guard` whose
    /// condition a dominating fact already proves ([`guard_proven`]); the
    /// lowering emits nothing for it.
    guard_check_unneeded: Vec<bool>,
}

impl CheckElision {
    /// IROPS3-2: was the bounds check at `node` proven by the clamp arm?
    #[inline]
    pub fn bounds_clamp_proved(&self, node: NodeId) -> bool {
        self.bounds_by_clamp
            .get(node as usize)
            .copied()
            .unwrap_or(false)
    }

    /// STRINTR21-5: is the `Op::Guard` `node` proven by a dominating fact, so
    /// that it can never fire?
    #[inline]
    pub fn guard_elided(&self, node: NodeId) -> bool {
        self.guard_check_unneeded
            .get(node as usize)
            .copied()
            .unwrap_or(false)
    }

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

    /// Why the range pass declined `node`, as a `REFUSAL_*` code. Only
    /// meaningful when [`Self::bounds_elided`] is false.
    #[inline]
    pub fn bounds_refusal(&self, node: NodeId) -> u8 {
        self.bounds_refusal
            .get(node as usize)
            .copied()
            .unwrap_or(REFUSAL_NONE)
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
        // `Op::Load` / `Op::Store` (field accesses) are deliberately NOT here
        // (r9). Coupling 3 in the module notes needs every listed op to stop on
        // a null base, and `ir_lower`'s helper-less inline `Op::Load` arm does
        // not: it answers `0` for a null receiver (`XOR EAX, EAX`) and carries
        // on. And the fact bought nothing: the only consumers of
        // `null_elided` are `ArrayLoad`/`ArrayStore`, and a verified method
        // never uses one SSA value as both a field receiver and an array --
        // except the interned `null` literal, `Op::Const(0)`, which is exactly
        // the value the inline arm continues past. `Foo f = null; f.x;
        // int[] a = null; a[0];` elided the second null check on that path.
        _ => None,
    }
}

/// The reference a `Guard(Cmp(Ne, x, null))` proves non-null for every node
/// after it, or `None` for any other node.
///
/// A guard deopts when its condition is zero, and `x != null` is zero exactly
/// when `x` is null. The null literal is the `Ref`-typed `Op::Const(0)` the
/// builder interns for `aconst_null`. A compare of null with null proves
/// nothing and answers `None`.
fn guard_non_null_operand(graph: &Graph, node: &crate::ir::Node) -> Option<NodeId> {
    if !matches!(node.op, Op::Guard { .. }) {
        return None;
    }
    let cond = graph.nodes.get(*node.inputs.get(1)? as usize)?;
    if cond.op != Op::Cmp(CmpOp::Ne) || cond.inputs.len() != 2 {
        return None;
    }
    let is_null = |n: NodeId| {
        graph
            .nodes
            .get(n as usize)
            .is_some_and(|c| c.op == Op::Const(0) && c.ty == crate::ir::IrType::Ref)
    };
    let (a, b) = (cond.inputs[0], cond.inputs[1]);
    match (is_null(a), is_null(b)) {
        (false, true) => Some(a),
        (true, false) => Some(b),
        _ => None,
    }
}

/// Operand proven non-null once `node` (an `x != null` or `ExactClassIs(x)`
/// guard) passes. Separate from [`guard_non_null_operand`] because a passing
/// null check makes a later `x != null` redundant, NOT a later `ExactClassIs`.
fn guard_proves_non_null_operand(graph: &Graph, node: &crate::ir::Node) -> Option<NodeId> {
    if let Some(x) = guard_non_null_operand(graph, node) {
        return Some(x);
    }
    if !matches!(node.op, Op::Guard { .. }) {
        return None;
    }
    let cond = graph.nodes.get(*node.inputs.get(1)? as usize)?;
    if matches!(cond.op, Op::ExactClassIs { .. }) && cond.inputs.len() == 1 {
        return Some(cond.inputs[0]);
    }
    None
}

/// The `(base, index)` pair a bounds check is against, if this op has one.
fn bounds_pair(node_op: &Op, inputs: &[NodeId]) -> Option<(NodeId, NodeId)> {
    match node_op {
        Op::ArrayLoad(_) | Op::ArrayStore(_) => Some((*inputs.get(2)?, *inputs.get(3)?)),
        _ => None,
    }
}

/// Does this node's result carry a value that cannot be null?
pub(crate) fn definitely_non_null(op: &Op) -> bool {
    matches!(
        op,
        // A successful allocation returns a non-null reference; a failed one
        // returns the deopt sentinel and never reaches a use.
        Op::New { .. }
            | Op::NewArray { .. }
            // MX16-3 (round 16 matrix): the outer array of a `multianewarray`.
            // Its helper answers a fresh array or `0`, and `0` takes the
            // exception edge exactly as `NewArray`'s does, so no use sees null.
            | Op::MultiNewArray { .. }
            // nio21 w15 L3: a deferred `new`'s helper answers a fresh object
            // or `0`, and `0` takes the exception edge, as `New`'s does.
            | Op::NewCp { .. }
            // The two constant-pool materializers either produce an object or
            // stash a pending exception and return the sentinel.
            | Op::ConstString { .. }
            | Op::ConstClass { .. }
    )
}

/// The VM files a length only for an initialized trusted final holding a
/// non-null array. Keep the fact on its SSA load; elements remain mutable.
fn trusted_static_arrays(graph: &Graph) -> FxHashMap<NodeId, i32> {
    root_ref_arrays::trusted_static_arrays_with_root_refs(graph)
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
        bounds_by_clamp: vec![false; n],
        bounds_refusal: vec![REFUSAL_NONE; n],
        guard_check_unneeded: vec![false; n],
    };
    if !enabled() {
        return out;
    }
    // STRINTR21-5 (lane charat): the guard facts. `a < b` known on an `If`
    // edge (by block, reflexive) and past a `Guard(a < b)` (block-level, like
    // the dereference facts), and the edge facts again keyed by `a` for the
    // unit-stride arm of `guard_proven`'s `x >= 0` shape.
    let guard_facts_on = guard_fact_elide_enabled();
    let lt_edges: FxHashMap<(NodeId, NodeId), Vec<usize>> = if guard_facts_on {
        strict_less_edge_facts(graph, schedule)
    } else {
        FxHashMap::default()
    };
    let mut lt_edges_by_lhs: FxHashMap<NodeId, Vec<usize>> = FxHashMap::default();
    for (&(a, _), blocks) in &lt_edges {
        lt_edges_by_lhs
            .entry(a)
            .or_default()
            .extend(blocks.iter().copied());
    }
    let mut lt_blocks: FxHashMap<(NodeId, NodeId), Vec<usize>> = FxHashMap::default();
    let mut lt_after: FxHashMap<(NodeId, NodeId), Vec<usize>> = FxHashMap::default();
    let mut nonneg_blocks: FxHashMap<NodeId, Vec<usize>> = FxHashMap::default();
    let mut nonneg_after: FxHashMap<NodeId, Vec<usize>> = FxHashMap::default();
    let mut le_const_blocks: FxHashMap<NodeId, Vec<(usize, i64)>> = FxHashMap::default();
    let mut le_const_after: FxHashMap<NodeId, Vec<(usize, i64)>> = FxHashMap::default();

    // Fact 1 (block-level): for each value, the set of blocks in which it is
    // known dereferenced. A later node is covered when ANY of those blocks
    // dominates the later node's block.
    let mut deref_blocks: FxHashMap<NodeId, Vec<usize>> = FxHashMap::default();
    // Fact 2 (block-level): the same for a checked `(base, index)` pair.
    let mut checked_blocks: FxHashMap<(NodeId, NodeId), Vec<usize>> = FxHashMap::default();
    // nio21 wave 6 (lane L2): the same two facts when they were created at or
    // after an exception-edge anchor in their block (see
    // `exception_edge_fences`). Keyed by the anchor's normal-continuation
    // block `c` and read REFLEXIVELY: such a fact holds exactly where `c`
    // dominates, never in a handler region the anchor's `If(CallThrew)` true
    // edges reach, where the check FAILED (or, for a node after the anchor,
    // never ran).
    let mut deref_after: FxHashMap<NodeId, Vec<usize>> = FxHashMap::default();
    let mut checked_after: FxHashMap<(NodeId, NodeId), Vec<usize>> = FxHashMap::default();
    let edge_fences = exception_edge_fences(graph, schedule);

    // Values that are non-null by construction, and the receiver.
    //
    // The receiver is seeded for the same reason the single-pass tier seeds it
    // (`CRATONVM_JIT_THIS_NONNULL`): the JVM guarantees a non-null receiver at
    // the call site, so `this` is non-null on entry and stays so -- it is a
    // parameter and SSA gives it no other definition.
    let trusted_arrays = trusted_static_arrays(graph);
    let mut non_null: FxHashSet<NodeId> = FxHashSet::default();
    for (id, node) in graph.nodes.iter().enumerate() {
        if definitely_non_null(&node.op)
            || trusted_arrays.contains_key(&(id as NodeId))
            || root_ref_arrays::is_non_null_root_or_static(graph, node)
        {
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
    let range_on = range_enabled();
    let max_stride = induction_max_stride();
    // `a[h & (a.length - 1)]` (round 12 wave 3; see `masked_below_length`).
    // Asked up front because its `m >= 0` half needs the lattice, which is
    // otherwise computed only when an `idx < length` fact exists.
    let masked_any = range_on
        && graph.nodes.iter().any(|n| {
            bounds_pair(&n.op, &n.inputs)
                .is_some_and(|(base, idx)| masked_below_length(graph, idx, base).is_some())
        });
    // JIT round 19 wave 4 (lane irops3, IROPS2-7): `a[clamp(i)]`, see
    // `clamp_index_needs`. Asked up front for the reason `masked_any` is.
    let clamp_any = range_on
        && clamp_index_enabled()
        && graph.nodes.iter().any(|n| {
            bounds_pair(&n.op, &n.inputs).is_some_and(|(_, idx)| is_clamp_root(graph, idx))
        });
    let RangeFacts {
        bounds: ranges,
        non_negative_at,
        offsets,
        const_below,
        at_most,
        alloc_by_len,
        ne_start_phis,
    } = if range_on {
        upper_bounds(graph, schedule)
    } else {
        RangeFacts {
            bounds: Vec::new(),
            non_negative_at: FxHashMap::default(),
            offsets: FxHashMap::default(),
            const_below: FxHashMap::default(),
            at_most: FxHashMap::default(),
            alloc_by_len: FxHashMap::default(),
            ne_start_phis: FxHashSet::default(),
        }
    };
    // The clamp arm's `base.length >= L` sources (IROPS2-7): every
    // `idx < base.length` fact (with `idx >= 0` at its own block it proves
    // `base.length >= 1`), and every `ArrayLength` node of a base (read
    // through the path-sensitive lattice at the access). Empty unless a clamp
    // index exists.
    let clamp_bounds: Vec<(NodeId, NodeId, usize)> = if clamp_any {
        ranges
            .iter()
            .map(|ub| (ub.base, ub.idx, ub.true_block))
            .collect()
    } else {
        Vec::new()
    };
    // JIT round 19 wave 5 (lane osr5, IROPS3-1): a guard `c < base.length`
    // (`c >= 0`) or `c <= base.length` (`c >= 1`) -- the loop-entry predicate
    // `ir_optimize::plant_clamp_length_predicates` plants -- proves
    // `base.length >= 1` in every block it strictly dominates. `(base,
    // block)`; empty unless `CRATONVM_JIT_IR_BCE_CLAMP_PREDICATE`.
    let clamp_len_guards: Vec<(NodeId, usize)> = if clamp_any && clamp_predicate_enabled() {
        graph
            .nodes
            .iter()
            .enumerate()
            .filter_map(|(id, n)| {
                if !matches!(n.op, Op::Guard { .. }) {
                    return None;
                }
                let base = length_positive_by(graph, *n.inputs.get(1)?)?;
                let block = schedule.node_to_block.get(id).copied()?;
                (block < schedule.blocks.len()).then_some((base, block))            })
            .collect()
    } else {
        Vec::new()
    };
    let mut lengths_of: FxHashMap<NodeId, Vec<NodeId>> = FxHashMap::default();
    if clamp_any {
        for (id, n) in graph.nodes.iter().enumerate() {
            if n.op == Op::ArrayLength {
                if let Some(&base) = n.inputs.get(2) {
                    lengths_of.entry(base).or_default().push(id as NodeId);
                }
            }
        }
    }
    // Blocks in which an array access of a base has been REACHED (its bounds
    // check emitted or proven), which proves `base.length >= 1` in every block
    // they dominate: an index `0 <= j < base.length` existed. Kept only for
    // the clamp arm.
    let mut accessed_blocks: FxHashMap<NodeId, Vec<usize>> = FxHashMap::default();
    // nio20 wave 6 (lane L4): `a[p & m]`, `m` a non-negative constant, under
    // `m < a.length` (see `const_mask_in_range`). Needs no lattice.
    let const_mask_on = range_on;
    // JIT round 21 wave 4 (lane bce, BCE21-2): `a[i - c]`, see
    // `negative_offset_index`. Opt-in.
    let const_offset_on = range_on && const_offset_predicate_enabled();
    // JIT r22 wave 2 (lane loops, LOOPS22-4 / BCE21-4): `a[(int) i]` with `i`
    // a `long` induction under `i < (long) a.length`. Opt-in; the facts are
    // collected only when such an access exists.
    let long_bounds: FxHashMap<NodeId, Vec<UpperBound>> = if range_on
        && long_induction_bce_enabled()
        && graph.nodes.iter().any(|n| {
            bounds_pair(&n.op, &n.inputs).is_some_and(|(_, idx)| l2i_operand(graph, idx).is_some())
        }) {
        long_upper_bounds(graph, schedule)
    } else {
        FxHashMap::default()
    };

    // The SSA value-range lattice, over the whole graph, once.
    //
    // `range_analysis::analyze_graph` has existed — with a sound lattice, JVMS-
    // correct transfer functions and its own tests — with no call site outside
    // its own `mod tests`. `x64/bce.rs` imports the `Range` TYPE for its
    // bytecode-level state and never the graph evaluator, so the optimizing
    // tier had no value-range analysis at all: `proven_non_negative` was three
    // hand-written arms (`Const >= 0`, `ArrayLength`, unit-stride phi).
    //
    // It is consulted as an EXTRA arm rather than as a replacement. The
    // evaluator is path-INSENSITIVE — `transfer` has no arm narrowing a value
    // on an `If` edge — so it proves nothing about the guard-dominated shape
    // the unit-stride rule exists for, and dropping that rule in favour of this
    // one would be a straight loss. What it adds is every index the three arms
    // have no shape for: `i & 0xff`, `i >>> 1`, an `I2C`, a `Load(Char)`, a
    // `Const` reached through arithmetic — all already modelled and tested
    // there.
    //
    // Fail-closed twice over: `analyze_graph` resets every entry to top when
    // its fixpoint does not settle inside `MAX_GRAPH_ROUNDS`, and the query
    // below uses `definitely_non_negative`, which answers `false` at bottom
    // instead of vacuously `true`.
    //
    // B3, 2026-09-17: this is now the PATH-SENSITIVE form. The comment above
    // said the evaluator "proves nothing about the guard-dominated shape the
    // unit-stride rule exists for" -- that was true of `analyze_graph` and is
    // no longer true of `analyze_graph_pathwise`, which narrows a value along
    // the `If` edges that must have been taken to reach a block. The unit-
    // stride rule is still NOT dropped: it proves things about a phi, which is
    // the one op the refinement deliberately refuses to recompute (see
    // `range_analysis::refine`).
    //
    // `CRATONVM_JIT_IR_RANGE_EDGES=0` selects a fact-free overlay, which is
    // exactly the pre-B3 behaviour through the same query path.
    //
    // Round 11: computed once, and only when it can be read. The default arm
    // used to run `analyze_graph` here AND again inside
    // `analyze_graph_pathwise`, discarding the first; and both ran for every
    // method, although `node_ranges` is read only inside the `ranges` loop
    // below — a method with no `idx < a.length` fact paid the whole fixpoint
    // for nothing.
    // JIT r22 wave 2 (lane loops, nio21 L5W3-1, opt-in): a `Guard(idx <
    // a.length)` (a branch the never-taken pruning turned into a guard, or a
    // planted predicate) bounds `idx` below `a.length` for the nodes after it
    // -- later in its own block, and in every block its block STRICTLY
    // dominates. Asked up front for the reason `masked_any` is.
    let guard_upper_any = range_on
        && guard_upper_bounds_enabled()
        && graph.nodes.iter().any(|n| {
            matches!(n.op, Op::Guard { .. })
                && n.inputs
                    .get(1)
                    .is_some_and(|&c| strictly_below_length(graph, c, true).is_some())
        });
    let mut guard_upper_blocks: FxHashMap<(NodeId, NodeId), Vec<usize>> = FxHashMap::default();
    let trusted_array_access = range_on && graph.nodes.iter().any(|n| {
        bounds_pair(&n.op, &n.inputs).is_some_and(|(base, _)| trusted_arrays.contains_key(&base))
    });
    let mask_guard_any = guard_facts_on && range_on
        && graph.nodes.iter().any(|n| mask_zero_guard_operand(graph, n).is_some());
    let node_ranges = if range_on
        && (!ranges.is_empty() || !offsets.is_empty() || masked_any || clamp_any
            || guard_upper_any || trusted_array_access || mask_guard_any)
    {
        Some(if range_edges_enabled() {
            crate::range_analysis::analyze_graph_pathwise(graph, schedule)
        } else {
            crate::range_analysis::PathRanges::flow_insensitive(
                crate::range_analysis::analyze_graph(graph),
            )
        })
    } else {
        None
    };
    // nio21 wave 3 (lane L5): the header φs proven `>= 0` by the monotone
    // induction rule (`monotone_non_negative_phis`): a non-counted loop's
    // index advanced by `1..k` in several arms, `UTF_8$Decoder.decodeArrayLoop`'s
    // `sp` and `dp`. Edge facts, so `CRATONVM_JIT_IR_RANGE_EDGES=0` withdraws
    // them too; computed only when some `idx < length` fact could use them.
    //
    // nio21 wave 4 (lane L5): the `v < x` / `x - v >= k` facts the rule reads
    // are collected once and shared with the room-offset arm below
    // (`room_offset_in_range`), which needs them even where no `idx < length`
    // fact exists (an access hanging only off a room test).
    let room_on = range_on && range_edges_enabled();
    let mono_facts = if range_on
        && range_edges_enabled()
        && ((monotone_enabled() && (!ranges.is_empty() || !offsets.is_empty())) || room_on)
    {
        monotone_facts(graph, schedule)
    } else {
        MonotoneFacts::default()
    };
    let mut mono_phis = if range_on
        && range_edges_enabled()
        && monotone_enabled()
        && (!ranges.is_empty() || !offsets.is_empty() || !mono_facts.room.is_empty())
    {
        monotone_non_negative_phis(graph, schedule, &mono_facts, &non_negative_at)
    } else {
        FxHashSet::default()
    };
    // t1 wave 6 (lane B6, `CRATONVM_JIT_IR_BCE_NE_EXIT_VAR_START`): the `!=`
    // loops' φs with a guarded non-constant start, `>= 0` wherever they are
    // defined (`ne_exit_var_start_bound`) -- the property every reader of
    // `mono_phis` asks. Not gated by the monotone switch: a different rule.
    mono_phis.extend(ne_start_phis);
    let room_on = room_on && !mono_facts.room.is_empty();
    // nio21 wave 6 (lane L5): `a[i + o]`, `o` a non-constant value, under an
    // `i < x` edge and `x <= a.length - o`, `o >= 0` facts (the offset
    // predicate's pre-header guards; see `VarOffsetFacts`). `None` unless some
    // `x <= a.length - o` fact exists. An edge-fact arm, so
    // `CRATONVM_JIT_IR_RANGE_EDGES=0` drops it too.
    let var_offset = if range_on && range_edges_enabled() && var_offset_enabled() {
        VarOffsetFacts::collect(graph, schedule, &non_negative_at)
    } else {
        None
    };
    // The `idx < length` facts by index, so each bounds check looks at its own
    // index's facts instead of filtering all of them (O(accesses × facts)).
    // Pushed in `ranges` order, so each list is visited in the order the
    // filter visited it.
    let mut ranges_by_idx: FxHashMap<NodeId, Vec<UpperBound>> = FxHashMap::default();
    for ub in ranges {
        ranges_by_idx.entry(ub.idx).or_default().push(ub);
    }
    // j27 wave 2 (lane TIER1PROP, `CRATONVM_JIT_IR_MONOTONE_NEG_OFFSET`,
    // opt-in): `a[p - c]` under a monotone `p` whose entry values are floored
    // `>= c` (see `monotone_phi_at_least`). An edge-fact arm, so
    // `CRATONVM_JIT_IR_RANGE_EDGES=0` drops it too.
    let neg_floor_on = range_on
        && range_edges_enabled()
        && monotone_neg_offset_enabled()
        && !ranges_by_idx.is_empty();
    let guard_floors = if neg_floor_on {
        guard_floor_facts(graph, schedule)
    } else {
        MonotoneFloors::default()
    };

    // One pass over the blocks in DOMINATOR ORDER — every block after each of
    // its dominators.
    //
    // This used to walk `schedule.blocks` in index order, which is neither RPO
    // nor dominator order: block indices are POST-layout, and frequency-driven
    // layout deliberately permutes them. The facts below accumulate as the walk
    // proceeds and are queried with `dominates(schedule, b, d)`, so a
    // dominating block that happens to carry a higher index had not yet
    // contributed its fact when `b` was processed. The query stayed sound —
    // this was only ever a missed elision — but it made the result depend on
    // how hot a method's blocks measured, so the same method could elide
    // different checks on different runs.
    for b in dominator_order(schedule) {
        let block = &schedule.blocks[b];
        // Facts established EARLIER IN THIS BLOCK. Kept separate from the
        // block-level maps so a fact does not appear to dominate itself.
        let mut local_deref: FxHashSet<NodeId> = FxHashSet::default();
        let mut local_checked: FxHashSet<(NodeId, NodeId)> = FxHashSet::default();
        // nio21 wave 6 L2: `None` until an exception-edge anchor is met in this
        // block; from it on, where this block's block-level facts go
        // (`Some(Some(c))`: under `c`, reflexively; `Some(None)`: nowhere).
        let mut fence: Option<Option<usize>> = None;
        let mut local_accessed: FxHashSet<NodeId> = FxHashSet::default();
        // L5W3-1: `(base, idx)` stated `idx < base.length` by a guard passed
        // EARLIER IN THIS BLOCK.
        let mut local_guard_upper: FxHashSet<(NodeId, NodeId)> = FxHashSet::default();
        // STRINTR21-5: `a < b` facts established earlier in this block.
        let mut local_lt: FxHashSet<(NodeId, NodeId)> = FxHashSet::default();
        let mut local_nonneg: FxHashSet<NodeId> = FxHashSet::default();
        let mut local_le_const: FxHashMap<NodeId, i64> = FxHashMap::default();

        for &id in &block.nodes {
            let Some(node) = graph.nodes.get(id as usize) else {
                continue;
            };
            // Set BEFORE the anchor's own facts are recorded: its check is the
            // one that fails into the handler.
            if let Some(&f) = edge_fences.get(&id) {
                fence = Some(narrow_fence(schedule, fence, f));
            }

            // STRINTR21-5 (lane charat): a guard whose condition a dominating
            // fact proves can never fire. Asked BEFORE this guard's own facts
            // are recorded below, so it cannot prove itself; an exception-edge
            // anchor keeps its guard (its failure is a handler's entry).
            if guard_facts_on
                && matches!(node.op, Op::Guard { .. })
                && !edge_fences.contains_key(&id)
            {
                let facts = GuardFacts {
                    non_null: &non_null,
                    local_deref: &local_deref,
                    deref_blocks: &deref_blocks,
                    deref_after: &deref_after,
                    local_lt: &local_lt,
                    lt_blocks: &lt_blocks,
                    lt_after: &lt_after,
                    lt_edges: &lt_edges,
                    lt_edges_by_lhs: &lt_edges_by_lhs,
                    ranges: node_ranges.as_ref(),
                    non_negative_at: &non_negative_at,
                    mono_phis: &mono_phis,
                    local_checked: &local_checked,
                    checked_blocks: &checked_blocks,
                    checked_after: &checked_after,
                };
                if guard_proven(graph, schedule, node, b, &facts).is_some() {
                    out.guard_check_unneeded[id as usize] = true;
                }
            }
            if guard_facts_on {
                if let Some(pair) = guard_strictly_less(graph, node) {
                    local_lt.insert(pair);
                    record_block_fact(&mut lt_blocks, &mut lt_after, pair, b, fence);
                }
                if let Some(v) = matches!(node.op, Op::Guard { .. })
                    .then(|| node.inputs.get(1).copied())
                    .flatten()
                    .and_then(|c| non_negative_by(graph, c, true))
                {
                    local_nonneg.insert(v);
                    record_block_fact(&mut nonneg_blocks, &mut nonneg_after, v, b, fence);
                }
                if let Some((v, k)) = guard_const_le_bound(graph, node) {
                    local_le_const
                        .entry(v)
                        .and_modify(|prev| *prev = (*prev).min(k))
                        .or_insert(k);
                    match fence {
                        None => le_const_blocks.entry(v).or_default().push((b, k)),
                        Some(Some(c)) => le_const_after.entry(v).or_default().push((c, k)),
                        Some(None) => {}
                    }
                }
            }

            if let Some(base_idx) = deref_base_input(&node.op) {
                let Some(&base) = node.inputs.get(base_idx) else {
                    continue;
                };
                let known = non_null.contains(&base)
                    || local_deref.contains(&base)
                    || block_fact_holds(schedule, &deref_blocks, &deref_after, &base, b);
                if known {
                    out.null_check_unneeded[id as usize] = true;
                }
                // Whether or not the check was elided, reaching PAST this node
                // proves the base was non-null: an elided check means it was
                // already proven, and an emitted one deopts when it is not.
                // (With an exception edge it enters a handler instead; that
                // path skips the rest of this block, and `fence` keeps the
                // block-level fact out of the handler region.)
                local_deref.insert(base);
                // Once per block: the walk visits one block at a time, so a
                // repeat is always the last entry, and pushing it again only
                // lengthened every later dominance scan (M accesses to one
                // array in one block made the pass O(M²)).
                record_block_fact(&mut deref_blocks, &mut deref_after, base, b, fence);
            }

            // A `Guard(Cmp(Ne, x, null))` deopts on a null `x`, so reaching
            // past it proves `x` non-null exactly as an emitted dereference
            // check does (round 11; filed by lane irlower as
            // `r11-irlower-guard-nonnull-is-not-a-check-elim-fact`). The String
            // expansion's `value` guard, the unbox receiver guards and every
            // pruned `ifnull`/`ifnonnull` plant this shape, and the element
            // accesses behind them re-tested the same reference.
            if let Some(x) = guard_proves_non_null_operand(graph, node) {
                local_deref.insert(x);
                record_block_fact(&mut deref_blocks, &mut deref_after, x, b, fence);
            }
            // L5W3-1: a passed `Guard(idx < base.length)` (it deopts unless
            // its condition holds). The block-level fact is kept only for a
            // guard before any exception-edge anchor of its block (a fenced
            // fact fails closed, as the clamp arm's access facts do).
            if guard_upper_any && matches!(node.op, Op::Guard { .. }) {
                if let Some((x, base)) = node
                    .inputs
                    .get(1)
                    .and_then(|&c| strictly_below_length(graph, c, true))
                {
                    local_guard_upper.insert((base, x));
                    if fence.is_none() {
                        let bs = guard_upper_blocks.entry((base, x)).or_default();
                        if bs.last() != Some(&b) {
                            bs.push(b);
                        }
                    }
                }
            }

            if let Some(pair) = bounds_pair(&node.op, &node.inputs) {
                let known = local_checked.contains(&pair)
                    || block_fact_holds(schedule, &checked_blocks, &checked_after, &pair, b);
                if known {
                    out.bounds_check_unneeded[id as usize] = true;
                } else {
                    // Not redundant. Try to prove it dead outright: a
                    // dominating `idx < base.length` for the SAME array, plus
                    // `idx >= 0`. Both halves, always -- see the range notes.
                    let (base, idx) = pair;
                    // Staged rather than one `any()`, so the FIRST failing
                    // conjunct is recorded. The stages run from "nothing to
                    // work with" to "one step short", which makes the census
                    // read as a work list rather than a total.
                    // With the range pass off nothing was attempted, and the
                    // census must not report "no length test" as a reason
                    // (round 11).
                    let mut reason = if range_on {
                        REFUSAL_NO_LENGTH_TEST
                    } else {
                        REFUSAL_NONE
                    };
                    let known_len = trusted_arrays.get(&base).copied().or_else(|| {
                        graph.nodes.get(base as usize).and_then(|n| {
                            matches!(n.op, Op::NewArray { .. })
                                .then(|| n.inputs.get(2).copied().and_then(|c| int_const(graph, c)))
                                .flatten()
                                .filter(|&len| len >= 0)
                                .map(|len| len as i32)
                        })
                    });
                    let mut proved = known_len.is_some_and(|length| {
                        let nonneg = local_nonneg.contains(&idx)
                            || block_fact_holds(schedule, &nonneg_blocks, &nonneg_after, &idx, b)
                            || proven_non_negative(
                                graph,
                                schedule,
                                node_ranges.as_ref(),
                                &non_negative_at,
                                &mono_phis,
                                idx,
                                b,
                                b,
                                max_stride,
                            );
                        let lim = i64::from(length);
                        nonneg
                            && (node_ranges
                                .as_ref()
                                .is_some_and(|r| r.at(idx, b).hi().is_some_and(|hi| hi < lim))
                                || constant_guard_upper_bound(
                                    graph, schedule, idx, b, &local_lt, &lt_blocks, &lt_after,
                                )
                                .is_some_and(|bound| bound <= lim)
                                || le_const_below(
                                    schedule,
                                    &local_le_const,
                                    &le_const_blocks,
                                    &le_const_after,
                                    idx,
                                    b,
                                    lim,
                                ))
                    });
                    for ub in ranges_by_idx.get(&idx).map_or(&[][..], Vec::as_slice) {
                        if reason == REFUSAL_NO_LENGTH_TEST {
                            reason = REFUSAL_OTHER_ARRAY;
                        }
                        if ub.base != base {
                            continue;
                        }
                        if reason == REFUSAL_OTHER_ARRAY {
                            reason = REFUSAL_NOT_DOMINATING;
                        }
                        if !dominates_reflexive(schedule, b, ub.true_block) {
                            continue;
                        }
                        if !proven_non_negative(
                            graph,
                            schedule,
                            node_ranges.as_ref(),
                            &non_negative_at,
                            &mono_phis,
                            idx,
                            ub.true_block,
                            b,
                            max_stride,
                        ) {
                            // Split by whether the index is a phi: a rejected
                            // INDUCTION VARIABLE means the stride or back-edge
                            // rule refused a real loop, which relaxing them
                            // could recover. Anything else is a different
                            // problem entirely.
                            reason = if matches!(
                                graph.nodes.get(idx as usize).map(|n| &n.op),
                                Some(Op::Phi)
                            ) {
                                REFUSAL_IV_SHAPE
                            } else {
                                REFUSAL_NOT_NON_NEGATIVE
                            };
                            continue;
                        }
                        proved = true;
                        break;
                    }
                    // `a[i + j]` under `i < a.length - k`, `0 <= j <= k`
                    // (wave 9; see `OffsetBound`). Tried only after every
                    // fact on the index itself, so the census keeps the
                    // reason those gave when this does not prove it either.
                    if !proved {
                        // JIT r23 w2c close: and `a[i]` itself (`j == 0`) under the
                        // same fact, for the margin-stride induction below (a
                        // main loop's first copy).
                        let offset_or_self = offset_index(graph, idx).or_else(|| {
                            (margin_stride_enabled() && offsets.contains_key(&idx)).then_some((idx, 0))
                        });
                        if let Some((i, j)) = offset_or_self {
                            proved =
                                offsets
                                    .get(&i)
                                    .map_or(&[][..], Vec::as_slice)
                                    .iter()
                                    .any(|f| {
                                        f.base == base
                                            && j <= f.margin
                                            && dominates_reflexive(schedule, b, f.true_block)
                                            && (proven_non_negative(
                                                graph,
                                                schedule,
                                                node_ranges.as_ref(),
                                                &non_negative_at,
                                                &mono_phis,
                                                i,
                                                f.true_block,
                                                b,
                                                max_stride,
                                            ) || is_margin_stride_induction(
                                                graph,
                                                schedule,
                                                i,
                                                f.true_block,
                                                f.margin,
                                            ))
                                    });
                        }
                    }
                    // `a[i - c]`, `c >= 1`, under a dominating `i < a.length`
                    // fact for the SAME array, `i` an increasing induction
                    // from a constant `>= c` (JIT round 21 wave 4, lane bce,
                    // BCE21-2; see `negative_offset_index`).
                    if !proved && const_offset_on {
                        if let Some((i, c)) = negative_offset_index(graph, idx) {
                            proved = ranges_by_idx
                                .get(&i)
                                .map_or(&[][..], Vec::as_slice)
                                .iter()
                                .any(|ub| {
                                    ub.base == base
                                        && dominates_reflexive(schedule, b, ub.true_block)
                                        && unit_stride_induction_floor(
                                            graph,
                                            schedule,
                                            i,
                                            ub.true_block,
                                            max_stride,
                                        )
                                        .is_some_and(|floor| floor >= c)
                                });
                        }
                    }
                    // The same access under a MONOTONE `i` whose every entry
                    // value is floored `>= c` (j27 wave 2, lane TIER1PROP,
                    // BCE21-2's non-constant start; see `monotone_phi_at_least`).
                    if !proved && neg_floor_on {
                        if let Some((i, c)) = negative_offset_index(graph, idx) {
                            proved = ranges_by_idx
                                .get(&i)
                                .map_or(&[][..], Vec::as_slice)
                                .iter()
                                .any(|ub| {
                                    ub.base == base
                                        && dominates_reflexive(schedule, b, ub.true_block)
                                })
                                && monotone_phi_at_least(
                                    graph,
                                    schedule,
                                    &mono_facts,
                                    &non_negative_at,
                                    &guard_floors,
                                    i,
                                    c,
                                );
                        }
                    }
                    // `a[(int) i]`, `i` a `long` increasing induction from a
                    // constant `>= 0` under a dominating `i < (long) a.length`
                    // for the SAME array (LOOPS22-4; see `long_upper_bounds`).
                    if !proved && !long_bounds.is_empty() {
                        if let Some(i) = l2i_operand(graph, idx) {
                            proved = long_bounds
                                .get(&i)
                                .map_or(&[][..], Vec::as_slice)
                                .iter()
                                .any(|ub| {
                                    ub.base == base
                                        && dominates_reflexive(schedule, b, ub.true_block)
                                        && graph.nodes.get(i as usize).is_some_and(|phi| {
                                            phi.op == Op::Phi
                                                && is_unit_stride_induction(
                                                    graph,
                                                    schedule,
                                                    i,
                                                    phi,
                                                    ub.true_block,
                                                    max_stride,
                                                )
                                        })
                                });
                        }
                    }
                    // `a[idx]` behind a passed `Guard(idx < a.length)` (nio21
                    // L5W3-1, `CRATONVM_JIT_IR_BCE_GUARD_UPPER`): the guard's
                    // block is the fact block `proven_non_negative`'s
                    // unit-stride rule asks every back edge to pass.
                    if !proved && guard_upper_any {
                        let key = (base, idx);
                        let nonneg = |g: usize| {
                            proven_non_negative(
                                graph,
                                schedule,
                                node_ranges.as_ref(),
                                &non_negative_at,
                                &mono_phis,
                                idx,
                                g,
                                b,
                                max_stride,
                            )
                        };
                        proved = (local_guard_upper.contains(&key) && nonneg(b))
                            || guard_upper_blocks.get(&key).is_some_and(|bs| {
                                bs.iter().any(|&g| dominates(schedule, b, g) && nonneg(g))
                            });
                    }
                    // `a[i + j]` (or `a[i]`, `j == 0`) under a dominating room
                    // test `x - i >= k`, `j <= k - 1`, with `x <= a.length`
                    // and `x >= 0` known at the access (nio21 wave 4, lane
                    // L5; see `room_offset_in_range`): the decode loop's
                    // `sa[sp + 1..3]` and `da[dp + 1]`.
                    if !proved && room_on {
                        let (i, j) = offset_index(graph, idx).unwrap_or((idx, 0));
                        proved = room_offset_in_range(
                            graph,
                            schedule,
                            &RoomInputs {
                                facts: &mono_facts,
                                at_most: &at_most,
                                alloc_by_len: &alloc_by_len,
                                non_negative_at: &non_negative_at,
                                ranges: node_ranges.as_ref(),
                                mono_phis: &mono_phis,
                                max_stride,
                            },
                            base,
                            i,
                            j,
                            b,
                        );
                    }
                    // `a[i + o]` under `i < x`, `x <= a.length - o`, `o >= 0`
                    // (nio21 wave 6, lane L5; see `VarOffsetFacts`): the
                    // buffer store `hb[p + offset]`.
                    if !proved {
                        if let Some(vo) = var_offset.as_ref() {
                            proved = vo.proves(
                                graph,
                                schedule,
                                node_ranges.as_ref(),
                                &non_negative_at,
                                max_stride,
                                base,
                                idx,
                                b,
                                id,
                            );
                        }
                    }
                    // `a[p & (a.length - k)]`, `k >= 1`, where the mask is
                    // proven `>= 0` at the access (round 12 wave 3).
                    if !proved && masked_any {
                        if let Some(m) = masked_below_length(graph, idx, base) {
                            proved = proven_non_negative(
                                graph,
                                schedule,
                                node_ranges.as_ref(),
                                &non_negative_at,
                                &mono_phis,
                                m,
                                b,
                                b,
                                max_stride,
                            ) || local_nonneg.contains(&m)
                                || block_fact_holds(schedule, &nonneg_blocks, &nonneg_after, &m, b);
                        }
                    }
                    // `a[max(0, min(i, a.length - 1))]` and its mirror
                    // (JIT round 19 wave 4, lane irops3, IROPS2-7): the
                    // index is in range whenever `a.length >= need`.
                    let mut clamp_proved = false;
                    if !proved && clamp_any {
                        let nonneg = |v: NodeId| {
                            proven_non_negative_without_induction(
                                graph,
                                schedule,
                                node_ranges.as_ref(),
                                &non_negative_at,
                                v,
                                b,
                            )
                        };
                        if let Some(need) = clamp_index_needs(graph, idx, base, &nonneg) {
                            proved = need <= 0
                                || (need == 1
                                    && (local_accessed.contains(&base)
                                        || accessed_blocks.get(&base).is_some_and(|bs| {
                                            bs.iter().any(|&d| dominates(schedule, b, d))
                                        })
                                        || clamp_bounds.iter().any(|&(fb, fi, fblock)| {
                                            fb == base
                                                && dominates_reflexive(schedule, b, fblock)
                                                && proven_non_negative(
                                                    graph,
                                                    schedule,
                                                    node_ranges.as_ref(),
                                                    &non_negative_at,
                                                    &mono_phis,
                                                    fi,
                                                    fblock,
                                                    fblock,
                                                    max_stride,
                                                )
                                        })))
                                || node_ranges.as_ref().is_some_and(|r| {
                                    lengths_of.get(&base).is_some_and(|ls| {
                                        ls.iter()
                                            .any(|&l| r.at(l, b).lo().is_some_and(|lo| lo >= need))
                                    })
                                })
                                // IROPS3-1 (lane osr5): the loop-entry
                                // `a.length > 0` predicate.
                                || (need == 1
                                    && clamp_len_guards
                                        .iter()
                                        .any(|&(gb, gblock)| gb == base && dominates(schedule, b, gblock)));
                            clamp_proved = proved;
                        }
                    }
                    // `a[p & m]`, `m` a constant in `0..=i32::MAX`, where `m <
                    // a.length` is known at the access (nio20 wave 6, lane L4):
                    // the index is in `[0, m]` by construction, so no `>= 0`
                    // half is owed.
                    if !proved && const_mask_on {
                        if let Some(m) = const_mask_of(graph, idx) {
                            proved = const_mask_in_range(graph, schedule, &const_below, &trusted_arrays, base, m, b);
                        }
                    }
                    if proved {
                        out.bounds_check_unneeded[id as usize] = true;
                        out.bounds_by_range[id as usize] = true;
                        out.bounds_by_clamp[id as usize] = clamp_proved;
                    } else {
                        out.bounds_refusal[id as usize] = reason;
                    }
                }
                local_checked.insert(pair);
                record_block_fact(&mut checked_blocks, &mut checked_after, pair, b, fence);
                if clamp_any {
                    local_accessed.insert(pair.0);
                    // nio21 wave 6 L2 x JIT round 19: past an exception-edge
                    // anchor (`fence` set) the block-level access fact is not
                    // recorded at all -- its reader asks plain dominance only,
                    // so a fenced fact fails closed (one fewer elision).
                    if fence.is_none() {
                        let bs = accessed_blocks.entry(pair.0).or_default();
                        if bs.last() != Some(&b) {
                            bs.push(b);
                        }
                    }
                }
            }
        }
    }
    mark_strip_mine_proven(graph, &mut out);
    out
}

/// j27 wave 4 (lane STRIPOSR, `CRATONVM_JIT_IR_STRIP_MINE_ARRAYS`): the
/// accesses of a strip-mined inner loop this analysis proved on the graph
/// before the strip mine ([`Graph::strip_mine_proven_accesses`]). The inner
/// test `i < slim` hides the main test `i < n - d` the offset rule reads, as
/// C2's strip mining runs after its range-check elimination; the proof was
/// taken before, and every run of the node now is a run of it then, with the
/// same index and array (`ir_optimize::strip_mine`'s "Why it is exact").
/// Only reached with the analysis on (`enabled()`): `=0` keeps every check.
fn mark_strip_mine_proven(graph: &Graph, out: &mut CheckElision) {
    for &id in &graph.strip_mine_proven_accesses {
        let Some(n) = graph.node_opt(id) else { continue };
        if let Some(slot) = out.null_check_unneeded.get_mut(id as usize) {
            *slot = true;
        }
        if matches!(n.op, Op::ArrayLoad(_)) {
            if let Some(slot) = out.bounds_check_unneeded.get_mut(id as usize) {
                *slot = true;
            }
        }
    }
}

// ── Guards a dominating fact proves (JIT round 21, lane charat) ───────
//
// STRINTR21-5 (`jr21-strintr2-charat-loop-per-char-node-cost-FIXED-20261101.md`):
// the IR `String` expansion guards its receiver, its `value` and its index at
// EVERY site, so `for (k = 0; k < s.length(); k++) s.charAt(k)` ran the
// receiver and `value` null guards twice per char and the `k >= 0` /
// `k < count` pair once more, behind a loop test that had just compared `k`
// with the same `count`. HotSpot's range-check elimination answers all of it
// with the loop's own exit test. Four shapes, each proven by the dominance
// argument the dereference and bounds facts above rest on (a guard deopts
// when its condition is zero, so past a dominating proof the condition cannot
// be zero; in SSA neither operand is redefined in between):
//
// 0. `Guard(x != null)` with `x` already known non-null: the dereference
//    facts, exactly as an array's null check is elided;
// 1. `Guard(a < b)` (or `b > a`) under a dominating `a < b`: an `If` edge
//    that proves it (`for`'s test, on its body edge) or an earlier guard of
//    the same pair;
// 2. `Guard(x >= 0)` (or `0 <= x`) with `x` proven non-negative: the arms
//    `proven_non_negative_without_unit_stride` reads, or a unit-stride
//    induction `x = phi(c >= 0, x + 1)` whose every back edge passes an `If`
//    edge proving `x < y` (any `y`: then `x + 1` cannot wrap).
//
// 3. `Guard((x & mask) == 0)` for a nonnegative int whose interval and earlier
//    constant-bound guards keep it below the mask's lowest set bit.
//
// Fail closed: any other condition, an unreachable block, a guard that is an
// exception-edge anchor, keeps its guard. `CRATONVM_JIT_IR_GUARD_FACT_ELIDE=0`
// emits every guard again.

/// `CRATONVM_JIT_IR_GUARD_FACT_ELIDE` (JIT round 21, lane charat, STRINTR21-5)
/// -- **default ON**, `=0` emits every guard. Read once per [`analyze`],
/// uncached (no `static`; a thread override in a test is honoured).
pub(crate) fn guard_fact_elide_enabled() -> bool {
    true
}

/// The facts [`guard_proven`] reads, borrowed from [`analyze`]'s walk.
struct GuardFacts<'f> {
    non_null: &'f FxHashSet<NodeId>,
    local_deref: &'f FxHashSet<NodeId>,
    deref_blocks: &'f FxHashMap<NodeId, Vec<usize>>,
    deref_after: &'f FxHashMap<NodeId, Vec<usize>>,
    local_lt: &'f FxHashSet<(NodeId, NodeId)>,
    lt_blocks: &'f FxHashMap<(NodeId, NodeId), Vec<usize>>,
    lt_after: &'f FxHashMap<(NodeId, NodeId), Vec<usize>>,
    lt_edges: &'f FxHashMap<(NodeId, NodeId), Vec<usize>>,
    lt_edges_by_lhs: &'f FxHashMap<NodeId, Vec<usize>>,
    ranges: Option<&'f crate::range_analysis::PathRanges>,
    non_negative_at: &'f FxHashMap<NodeId, Vec<usize>>,
    mono_phis: &'f FxHashSet<NodeId>,
    /// `(base, idx)` pairs whose bounds check a dominating access passed.
    local_checked: &'f FxHashSet<(NodeId, NodeId)>,
    checked_blocks: &'f FxHashMap<(NodeId, NodeId), Vec<usize>>,
    checked_after: &'f FxHashMap<(NodeId, NodeId), Vec<usize>>,
}

/// `(a, b)` when compare `cond`, evaluated to `holds`, states `a < b`.
fn strictly_less(graph: &Graph, cond: NodeId, holds: bool) -> Option<(NodeId, NodeId)> {
    let c = graph.nodes.get(cond as usize)?;
    let Op::Cmp(op) = c.op else {
        return None;
    };
    let [a, b] = c.inputs.as_slice() else {
        return None;
    };
    let op = if holds { op } else { op.negate() };
    match op {
        CmpOp::Lt => Some((*a, *b)),
        CmpOp::Gt => Some((*b, *a)),
        _ => None,
    }
}

/// The `a < b` an `Op::Guard` `node` establishes past itself, or `None`.
fn guard_strictly_less(graph: &Graph, node: &crate::ir::Node) -> Option<(NodeId, NodeId)> {
    if !matches!(node.op, Op::Guard { .. }) {
        return None;
    }
    strictly_less(graph, *node.inputs.get(1)?, true)
}

/// Every `a < b` an `If` edge proves, with the blocks it holds in (the edge's
/// own block, read reflexively). The edge-to-block mapping `upper_bounds`
/// uses: a block whose control is the `Proj` of an `If` is entered only on
/// that edge.
fn strict_less_edge_facts(
    graph: &Graph,
    schedule: &Schedule,
) -> FxHashMap<(NodeId, NodeId), Vec<usize>> {
    let mut out: FxHashMap<(NodeId, NodeId), Vec<usize>> = FxHashMap::default();
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
        if !matches!(graph.nodes.get(owner as usize).map(|n| &n.op), Some(Op::If)) {
            continue;
        }
        let Some(&cond) = graph.nodes[owner as usize].inputs.get(1) else {
            continue;
        };
        if let Some(pair) = strictly_less(graph, cond, which == 0) {
            out.entry(pair).or_default().push(b);
        }
    }
    out
}

fn guard_const_le_bound(graph: &Graph, node: &crate::ir::Node) -> Option<(NodeId, i64)> {
    if !matches!(node.op, Op::Guard { .. }) {
        return None;
    }
    let c = graph.nodes.get(*node.inputs.get(1)? as usize)?;
    let Op::Cmp(op) = c.op else {
        return None;
    };
    let [a, b] = c.inputs.as_slice() else {
        return None;
    };
    match op {
        CmpOp::Le => int_const(graph, *b).map(|k| (*a, k)),
        CmpOp::Ge => int_const(graph, *a).map(|k| (*b, k)),
        _ => None,
    }
}

fn le_const_below(
    schedule: &Schedule,
    local: &FxHashMap<NodeId, i64>,
    plain: &FxHashMap<NodeId, Vec<(usize, i64)>>,
    after: &FxHashMap<NodeId, Vec<(usize, i64)>>,
    value: NodeId,
    block: usize,
    lim: i64,
) -> bool {
    local.get(&value).is_some_and(|&k| k < lim)
        || plain.get(&value).is_some_and(|v| {
            v.iter()
                .any(|&(f, k)| k < lim && dominates(schedule, block, f))
        })
        || after.get(&value).is_some_and(|v| {
            v.iter()
                .any(|&(f, k)| k < lim && dominates_reflexive(schedule, block, f))
        })
}

/// A positive constant exclusive bound established by a passed guard.
/// Same-block order and exceptional continuations use the existing fact maps.
fn constant_guard_upper_bound(
    graph: &Graph,
    schedule: &Schedule,
    value: NodeId,
    block: usize,
    local: &FxHashSet<(NodeId, NodeId)>,
    plain: &FxHashMap<(NodeId, NodeId), Vec<usize>>,
    after: &FxHashMap<(NodeId, NodeId), Vec<usize>>,
) -> Option<i64> {
    local.iter().chain(plain.keys()).chain(after.keys()).filter_map(|&(left, limit)| {
        if left != value { return None; }
        let bound = int_const(graph, limit)?;
        (bound > 0 && (local.contains(&(left, limit))
            || block_fact_holds(schedule, plain, after, &(left, limit), block)))
            .then_some(bound)
    }).min()
}

/// `(value, mask)` for an int guard `(value & constant_mask) == 0`.
fn mask_zero_guard_operand(graph: &Graph, node: &crate::ir::Node) -> Option<(NodeId, u32)> {
    if !matches!(node.op, Op::Guard { .. }) { return None; }
    let cmp = graph.nodes.get(*node.inputs.get(1)? as usize)?;
    if cmp.op != Op::Cmp(CmpOp::Eq) { return None; }
    let [left, right] = cmp.inputs.as_slice() else { return None; };
    let bits = if int_const(graph, *right) == Some(0) { *left }
        else if int_const(graph, *left) == Some(0) { *right }
        else { return None; };
    let and = graph.nodes.get(bits as usize)?;
    if and.op != Op::And || and.ty != crate::ir::IrType::Int { return None; }
    let [left, right] = and.inputs.as_slice() else { return None; };
    let (value, mask) = match (int_const(graph, *left), int_const(graph, *right)) {
        (_, Some(mask)) => (*left, mask),
        (Some(mask), _) => (*right, mask),
        _ => return None,
    };
    if graph.nodes.get(value as usize)?.ty != crate::ir::IrType::Int { return None; }
    // int_const supplies the declared 32-bit pattern, including negative masks.
    let mask = mask as u32;
    (mask != 0).then_some((value, mask))
}

fn mask_zero_guard_proven(
    graph: &Graph,
    schedule: &Schedule,
    node: &crate::ir::Node,
    block: usize,
    facts: &GuardFacts<'_>,
) -> bool {
    let Some((value, mask)) = mask_zero_guard_operand(graph, node) else { return false; };
    let Some(ranges) = facts.ranges else { return false; };
    let range = ranges.at(value, block);
    if !range.definitely_non_negative() { return false; }
    // A nonnegative value below the mask's lowest set bit has no overlapping bits.
    // mask is nonzero, so the shift is at most 31 and fits the i64 proof domain.
    let limit = 1_i64 << mask.trailing_zeros();
    range.hi().is_some_and(|hi| hi < limit)
        || constant_guard_upper_bound(graph, schedule, value, block,
            facts.local_lt, facts.lt_blocks, facts.lt_after)
            .is_some_and(|bound| bound <= limit)
}

/// Which of the four shapes (see the section notes) proves the guard `node`
/// in block `b`, or `None` when it must stay.
fn guard_proven(
    graph: &Graph,
    schedule: &Schedule,
    node: &crate::ir::Node,
    b: usize,
    f: &GuardFacts<'_>,
) -> Option<usize> {
    if !schedule.dom.is_reachable(b) {
        return None;
    }
    // 0. `x != null`.
    if let Some(x) = guard_non_null_operand(graph, node) {
        let known = f.non_null.contains(&x)
            || f.local_deref.contains(&x)
            || block_fact_holds(schedule, f.deref_blocks, f.deref_after, &x, b);
        return known.then_some(0);
    }
    // 3. A prior integer bound leaves no bits that overlap this constant mask.
    if mask_zero_guard_proven(graph, schedule, node, b, f) { return Some(3); }
    let cond = *node.inputs.get(1)?;
    // 1. `a < b`.
    if let Some(pair) = strictly_less(graph, cond, true) {
        let known = f.local_lt.contains(&pair)
            || block_fact_holds(schedule, f.lt_blocks, f.lt_after, &pair, b)
            || f.lt_edges
                .get(&pair)
                .is_some_and(|bs| bs.iter().any(|&e| dominates_reflexive(schedule, b, e)));
        return known.then_some(1);
    }
    // 2. `x >= 0`.
    let c = graph.nodes.get(cond as usize)?;
    let x = match (&c.op, c.inputs.as_slice()) {
        (Op::Cmp(CmpOp::Ge), [x, z]) if int_const(graph, *z) == Some(0) => *x,
        (Op::Cmp(CmpOp::Le), [z, x]) if int_const(graph, *z) == Some(0) => *x,
        _ => return None,
    };
    let by_facts = proven_non_negative_without_unit_stride(
        graph,
        schedule,
        f.ranges,
        f.non_negative_at,
        f.mono_phis,
        x,
        b,
    );
    // An access indexed by `x` passed its (unsigned) bounds check: `0 <= x`.
    let by_access = || {
        f.local_checked.iter().any(|&(_, i)| i == x)
            || f.checked_blocks.keys().chain(f.checked_after.keys()).any(|&(a, i)| {
                i == x && block_fact_holds(schedule, f.checked_blocks, f.checked_after, &(a, i), b)
            })
    };
    if by_facts || by_access() {
        return Some(2);
    }
    let by_induction = || {
        let Some(xn) = graph.nodes.get(x as usize) else {
            return false;
        };
        xn.op == Op::Phi
            && f.lt_edges_by_lhs.get(&x).is_some_and(|bs| {
                bs.iter().any(|&e| {
                    dominates_reflexive(schedule, b, e)
                        && is_unit_stride_induction(graph, schedule, x, xn, e, 1)
                })
            })
    };
    (by_facts || by_induction()).then_some(2)
}

// ── Exception edges fence the dominance facts (nio21 wave 6, lane L2) ──
//
// Both dominance facts above rest on "a FAILED check leaves the method": a
// node that dominates `n` dereferenced (or bounds-checked) the value, and had
// it failed, `n` would not be running. The exception edges break that premise
// on purpose. An `If(Op::CallThrew)` chain anchored on a node `t` (an implicit
// edge's `ArrayLoad` / `ArrayStore` / zero-divisor `Guard`, or a call-site
// edge's `Call`) gives `t`'s failure a CFG edge into a handler, and `t`'s
// block dominates that handler. So a fact created by `t` -- or by any node
// after `t` in its block, which the pad jumps over -- must not hold there:
//
// ```java
// try { x = a[i]; } catch (NullPointerException e) { n = a.length; }  // a IS null
// ```
//
// The rule: such a fact holds at block `b` iff the normal continuation `c` of
// `t`'s chain (the block the LAST false edge heads) dominates `b`,
// reflexively. Every path from `t`'s block to `b` then runs through the chain's
// false edges, i.e. through `t` passing. "Not dominated by a `Proj(0)`" would
// NOT be enough: a handler that two sites share is a merge that neither
// site's `Proj(0)` block dominates, yet `t`'s block still does.
//
// Facts created BEFORE the anchor in its block stay plain: those nodes ran on
// the failing path too.

/// Per exception-edge anchor (input 1 of an `Op::CallThrew`), the block its
/// chain's normal continuation heads, or `None` when the schedule does not
/// show one cleanly -- and then the anchor's block keeps no block-level fact
/// past it (fail closed). Empty for a graph without `CallThrew`, which is
/// every graph unless one of the default-OFF edge switches built one.
fn exception_edge_fences(graph: &Graph, schedule: &Schedule) -> FxHashMap<NodeId, Option<usize>> {
    let mut out: FxHashMap<NodeId, Option<usize>> = FxHashMap::default();
    if !graph
        .nodes
        .iter()
        .any(|n| matches!(n.op, Op::CallThrew { .. }))
    {
        return out;
    }
    // `If` by condition, and the `Proj(1)` of each `If` (a second one marks
    // the `If` malformed: `NO_NODE`).
    let mut ifs_by_cond: FxHashMap<NodeId, Vec<NodeId>> = FxHashMap::default();
    let mut proj1_of: FxHashMap<NodeId, NodeId> = FxHashMap::default();
    for (id, n) in graph.nodes.iter().enumerate() {
        // Cast: node ids index `graph.nodes`, which the builder bounds.
        let id = id as NodeId;
        match n.op {
            Op::If => {
                if let Some(&c) = n.inputs.get(1) {
                    ifs_by_cond.entry(c).or_default().push(id);
                }
            }
            Op::Proj(1) => {
                if let Some(&p) = n.inputs.first() {
                    proj1_of
                        .entry(p)
                        .and_modify(|x| *x = crate::ir::NO_NODE)
                        .or_insert(id);
                }
            }
            _ => {}
        }
    }
    for (id, n) in graph.nodes.iter().enumerate() {
        if !matches!(n.op, Op::CallThrew { .. }) {
            continue;
        }
        let Some(&anchor) = n.inputs.get(1) else {
            continue;
        };
        // Cast: as above.
        let flag = id as NodeId;
        let ifs = ifs_by_cond.get(&flag).map_or(&[][..], Vec::as_slice);
        for &iff in ifs {
            let cont = proj1_of
                .get(&iff)
                .copied()
                .and_then(|p| normal_continuation_block(schedule, iff, p));
            let prev = out.get(&anchor).copied();
            let next = match prev {
                None => cont,
                Some(prev) => narrow_fence(schedule, Some(prev), cont),
            };
            out.insert(anchor, next);
        }
        // A flag with no `If` routes nothing: no fence from it (another flag
        // of the same anchor may still set one).
    }
    out
}

/// The block `proj` (the `Proj(1)` of `iff`) heads, when it is a block of its
/// own whose ONLY predecessor is `iff`'s block -- so dominance from it means
/// "through this false edge". `None` for any other shape.
fn normal_continuation_block(schedule: &Schedule, iff: NodeId, proj: NodeId) -> Option<usize> {
    if proj == crate::ir::NO_NODE {
        return None;
    }
    let ib = block_of(schedule, iff)?;
    let c = block_of(schedule, proj)?;
    let blk = schedule.blocks.get(c)?;
    (c != ib && blk.ctrl == proj && blk.predecessors.len() == 1 && blk.predecessors[0] == ib)
        .then_some(c)
}

/// Combine the fence a block already has with one more anchor's: the deeper of
/// the two continuation blocks when one dominates the other (a chain's false
/// edges, or two anchors in one block), else `None` (fail closed).
fn narrow_fence(
    schedule: &Schedule,
    have: Option<Option<usize>>,
    add: Option<usize>,
) -> Option<usize> {
    match (have, add) {
        (None, add) => add,
        (Some(None), _) | (_, None) => None,
        (Some(Some(x)), Some(y)) => {
            if dominates_reflexive(schedule, y, x) {
                Some(y)
            } else if dominates_reflexive(schedule, x, y) {
                Some(x)
            } else {
                None
            }
        }
    }
}

/// Record a block-level fact `key` created in block `b`, under `fence` (see
/// [`analyze`]): plain (`None`), past an exception edge (`Some(Some(c))`), or
/// not at all (`Some(None)`).
fn record_block_fact<K: Copy + Eq + std::hash::Hash>(
    plain: &mut FxHashMap<K, Vec<usize>>,
    after: &mut FxHashMap<K, Vec<usize>>,
    key: K,
    b: usize,
    fence: Option<Option<usize>>,
) {
    let (map, at) = match fence {
        None => (plain, b),
        Some(Some(c)) => (after, c),
        Some(None) => return,
    };
    let bs = map.entry(key).or_default();
    if bs.last() != Some(&at) {
        bs.push(at);
    }
}

/// Does a block-level fact `key` hold in block `b`? A plain fact by strict
/// dominance ([`dominates`]); a fenced one by reflexive dominance of its
/// continuation block, failing closed for an unreachable `b`.
fn block_fact_holds<K: Eq + std::hash::Hash>(
    schedule: &Schedule,
    plain: &FxHashMap<K, Vec<usize>>,
    after: &FxHashMap<K, Vec<usize>>,
    key: &K,
    b: usize,
) -> bool {
    plain
        .get(key)
        .is_some_and(|bs| bs.iter().any(|&d| dominates(schedule, b, d)))
        || (schedule.dom.is_reachable(b)
            && after
                .get(key)
                .is_some_and(|cs| cs.iter().any(|&c| dominates_reflexive(schedule, b, c))))
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
    // `Dominators::dominates` answers `true` for EVERY `d` when `b` is
    // unreachable -- that is its documented contract (the fixpoint's top
    // element, never tightened for a block the entry cannot reach). Read
    // through this predicate it becomes a fail-OPEN answer in a memory-safety
    // pass: any deref of the same base anywhere in the method would license
    // eliding the null check at an access in an unreachable block, and any
    // `idx < len` branch anywhere would license dropping its bounds check.
    // `ir_lower` emits every block in `schedule.blocks`, reachable or not, so
    // the guard-free bytes are really emitted -- today only on code no branch
    // targets, which is why this has never been observed, but the module
    // header's rule is "every unknown answers check needed" and "b is
    // unreachable" is an unknown. See `unreachable_block_proves_nothing`.
    if !schedule.dom.is_reachable(b) {
        return false;
    }
    schedule.dom.dominates(d, b)
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
// Round 12 wave 3 (iropt proposal W2-1): "an array length can be as large as
// `i32::MAX`" is not true of this VM. Its cap is `i32::MAX - 2`
// (`VM_MAX_ARRAY_LENGTH`), so `i <= len - 1 <= i32::MAX - 3` and `i + s` is
// exact for `s <= 3` (`MAX_PROVEN_STRIDE`): pairwise and triple loops keep no
// check. A bound like `s <= 1024` is still unsound, for the reason above.
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

/// Does block `d` dominate block `b`? Unlike [`dominates`], a block dominates
/// itself here.
///
/// The redundancy pass deliberately answers `false` for `b == d` so a check
/// cannot prove itself; the range pass needs the opposite, because an access
/// sitting in the very block a branch guards IS covered by it (the test runs in
/// the predecessor's terminator, before any node of the true block).
///
/// Fails closed for an unreachable `b`, for the reason [`dominates`] states at
/// length: `Dominators::dominates` answers `true` there for every `d`, which as
/// a bounds-check permission is "no guard at all proves this access".
#[inline]
fn dominates_reflexive(schedule: &Schedule, b: usize, d: usize) -> bool {
    if b == d {
        return true;
    }
    schedule.dom.is_reachable(b) && schedule.dom.dominates(d, b)
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
///
/// # A bound that is not a length (round 11 wave 6)
///
/// `idx < x` also bounds `idx` below `base.length` when `x <= base.length` is
/// known wherever the `idx < x` edge holds. Three sources:
///
/// * **A minimum.** `x = Math.min(y, base.length)` (`ScalarOp::MinI`) is at
///   most each operand, by definition.
/// * **An allocation.** `b = new T[x]` has `b.length == x` for its whole
///   lifetime (a negative `x` throws before `b` exists), so `idx < x` gives
///   `idx < b.length` for `b` itself. `for (i = 0; i < n; i++) r[i] = ..` over
///   `r = new int[n]`, and the copy loop over `new int[a.length]`.
/// * **A dominating fact** `x <= base.length`: an `If` edge on which it holds
///   (read with the same negation rule as the table above), or a `Guard`
///   whose condition states it (a guard deopts unless its condition holds).
///   An edge fact holds from the start of its block, so it covers a
///   `true_block` it dominates reflexively; a guard's fact holds only after
///   the guard, so its block must STRICTLY dominate `true_block`.
///
/// Both halves are SSA facts about the same values: `x`, `base` and `idx`
/// cannot be redefined between a dominating fact and a use it covers without
/// the fact's block running again. `x` is compared through [`LenKey`], so the
/// same array's length read twice, or the same `int` constant interned twice,
/// is one value. `idx >= 0` is still proven separately in [`analyze`], and the
/// unit-stride argument is unchanged: `idx < x` with `x` an `int` keeps
/// `idx + 1 <= i32::MAX`.
///
/// # Non-strict tests and decreasing loops (round 11 wave 8)
///
/// Three more sources, each needing a STRICT `x < base.length` (see
/// [`strictly_below_length`] and [`below_own_length`]):
///
/// * **`idx <= x`** on an edge, with `x < base.length`: `idx <= x < len`.
///   A non-strict `x <= len` proves nothing here -- `idx == x == len` is the
///   off-by-one the check exists for. The unit-stride argument still holds:
///   `idx <= len - 1`, so `idx + 1 <= len <= i32::MAX`.
/// * **`idx < x`** with `x = base.length - k`, `k >= 1`: `x < len`.
/// * **A decreasing induction φ** (see [`decreasing_induction`]) whose start
///   value is below `base.length`: every value the φ takes is at most its
///   start, so it is below the length everywhere the header dominates. Its
///   `idx >= 0` half is the `If`-edge fact in [`RangeFacts::non_negative_at`]
///   (round 11 wave 9, see [`proven_non_negative`]), with the path-sensitive
///   range at the access as the fallback.
///
/// Also returns the blocks where a value is known `>= 0` from an `If` edge
/// (`non_negative_by`), keyed by the value, and the [`OffsetBound`]s keyed by
/// the bounded value (round 11 wave 9, mid proposal W8-2).
fn upper_bounds(graph: &Graph, schedule: &Schedule) -> RangeFacts {
    // Which block does each edge of each `If` open? `Block::ctrl` is the
    // control node starting the block, and for a branch successor that is the
    // `Proj`. When an edge lands straight on a `Merge` instead (an empty arm),
    // no block is found and that edge contributes no fact.
    let mut edge_block: FxHashMap<(NodeId, u16), usize> = FxHashMap::default();
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

    // The allocation and dominating-fact sources of `x <= base.length` (see
    // the doc above), keyed by `LenKey` of `x`. The minimum is read in place
    // below.
    let mut alloc_by_len: FxHashMap<LenKey, Vec<NodeId>> = FxHashMap::default();
    let mut at_most: FxHashMap<LenKey, Vec<AtMostLength>> = FxHashMap::default();
    // JIT round 20 wave 3 (lane loopopt): a guard past an exception-edge
    // anchor in its block states no block-level fact (`guard_after_anchor`).
    let fence_anchors = crate::ir::exception_edge_anchors(graph);
    // The strict `x < base.length` facts, and the blocks where a value is
    // known `>= 0` from an `If` edge (wave 8; see the doc above).
    let mut below: FxHashMap<LenKey, Vec<AtMostLength>> = FxHashMap::default();
    let mut non_negative_at: FxHashMap<NodeId, Vec<usize>> = FxHashMap::default();
    // nio20 wave 6 (lane L4): the same facts with a constant `x`, keyed by
    // the array, for the constant-mask arm.
    let mut const_below: FxHashMap<NodeId, Vec<ConstBelow>> = FxHashMap::default();
    // t1 wave 6 (lane B6, BCE21-5 residual): `v >= c`, `c >= 0`, after a
    // `Guard` (not past an exception-edge anchor), keyed by `v` -- the `0 <=
    // s` pre-header guard of a `!=` loop with a non-constant start
    // ([`ne_exit_var_start_bound`]). Collected only under its switch.
    let ne_var_start = bce_ne_exit_var_start_enabled();
    let mut guard_non_negative: FxHashMap<NodeId, Vec<usize>> = FxHashMap::default();
    // The header φs [`ne_exit_var_start_bound`] admits: `>= 0` wherever they
    // are defined (see its doc).
    let mut ne_start_phis: FxHashSet<NodeId> = FxHashSet::default();
    for (id, node) in graph.nodes.iter().enumerate() {
        match node.op {
            // `Op::NewArray` is `[ctrl, mem, length]`.
            Op::NewArray { .. } => {
                if let Some(&count) = node.inputs.get(2) {
                    alloc_by_len
                        .entry(len_key(graph, count))
                        .or_default()
                        .push(id as NodeId);
                }
            }
            Op::If => {
                let Some(&cond) = node.inputs.get(1) else {
                    continue;
                };
                for edge in [0u16, 1u16] {
                    let Some(&block) = edge_block.get(&(id as NodeId, edge)) else {
                        continue;
                    };
                    if let Some((x, base)) = at_most_length(graph, cond, edge == 0) {
                        at_most
                            .entry(len_key(graph, x))
                            .or_default()
                            .push(AtMostLength {
                                base,
                                block,
                                after_block_start: false,
                            });
                        if let Some(f) = const_below_of(graph, x, false, base, block, false) {
                            const_below.entry(base).or_default().push(f);
                        }
                    }
                    if let Some((x, base)) = strictly_below_length(graph, cond, edge == 0) {
                        below
                            .entry(len_key(graph, x))
                            .or_default()
                            .push(AtMostLength {
                                base,
                                block,
                                after_block_start: false,
                            });
                        if let Some(f) = const_below_of(graph, x, true, base, block, false) {
                            const_below.entry(base).or_default().push(f);
                        }
                    }
                    // An `If`-edge fact, so it honours the edge kill switch:
                    // `CRATONVM_JIT_IR_RANGE_EDGES=0` has to restore the
                    // pre-narrowing answer (`the_edge_narrowing_switch_restores_the_old_answer`).
                    if range_edges_enabled() {
                        if let Some(v) = non_negative_by(graph, cond, edge == 0) {
                            non_negative_at.entry(v).or_default().push(block);
                        }
                    }
                }
            }
            Op::Guard { .. } => {
                let Some(&cond) = node.inputs.get(1) else {
                    continue;
                };
                let Some(&block) = schedule.node_to_block.get(id) else {
                    continue;
                };
                if block >= schedule.blocks.len() {
                    continue;
                }
                if guard_after_anchor(schedule, &fence_anchors, block, id as NodeId) {
                    continue;
                }
                if ne_var_start {
                    if let Some(v) = non_negative_by(graph, cond, true) {
                        guard_non_negative.entry(v).or_default().push(block);
                    }
                }
                if let Some((x, base)) = at_most_length(graph, cond, true) {
                    at_most
                        .entry(len_key(graph, x))
                        .or_default()
                        .push(AtMostLength {
                            base,
                            block,
                            after_block_start: true,
                        });
                    if let Some(f) = const_below_of(graph, x, false, base, block, true) {
                        const_below.entry(base).or_default().push(f);
                    }
                }
                if let Some((x, base)) = strictly_below_length(graph, cond, true) {
                    below
                        .entry(len_key(graph, x))
                        .or_default()
                        .push(AtMostLength {
                            base,
                            block,
                            after_block_start: true,
                        });
                    if let Some(f) = const_below_of(graph, x, true, base, block, true) {
                        const_below.entry(base).or_default().push(f);
                    }
                }
            }
            _ => {}
        }
    }
    // Wave-8 facts, appended after every older one so the older readings keep
    // their order in `ranges_by_idx` (the refusal census is order-sensitive).
    let mut extra: Vec<UpperBound> = Vec::new();
    // Wave-9 facts: `idx < base.length - margin`, for `a[idx + j]`.
    let mut offsets: FxHashMap<NodeId, Vec<OffsetBound>> = FxHashMap::default();

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
        // The NON-STRICT reading of the same compare, on the other edge from
        // the strict one: `idx <= x`, which bounds `idx` below `base.length`
        // only when `x` is STRICTLY below it.
        let non_strict = match (&cond.op, cond.inputs.as_slice()) {
            (Op::Cmp(CmpOp::Le), [a, b]) => Some((*a, *b, 0u16)),
            (Op::Cmp(CmpOp::Ge), [a, b]) => Some((*b, *a, 0u16)),
            (Op::Cmp(CmpOp::Gt), [a, b]) => Some((*a, *b, 1u16)),
            (Op::Cmp(CmpOp::Lt), [a, b]) => Some((*b, *a, 1u16)),
            _ => None,
        };
        if let Some((idx, x, edge)) = non_strict {
            if let Some(&true_block) = edge_block.get(&(id as NodeId, edge)) {
                // `idx <= len - k`, `k >= 1`: `idx + (k - 1) < len`.
                if let Some((base, k)) = length_margin(graph, x) {
                    offsets.entry(idx).or_default().push(OffsetBound {
                        base,
                        true_block,
                        margin: k - 1,
                    });
                }
                let mut bases: Vec<NodeId> = Vec::new();
                if let Some(base) = below_own_length(graph, x) {
                    bases.push(base);
                }
                for f in below.get(&len_key(graph, x)).map_or(&[][..], Vec::as_slice) {
                    if fact_holds_at(schedule, f, true_block) && !bases.contains(&f.base) {
                        bases.push(f.base);
                    }
                }
                for base in bases {
                    extra.push(UpperBound {
                        idx,
                        base,
                        true_block,
                    });
                }
            }
        }
        // `Op::Cmp` is `[a, b]` with no control input. See the table above for
        // which (edge, comparison) pairs prove `idx < len`; a non-strict bound
        // on the taken side would leave `idx == len`, which is precisely the
        // off-by-one the check exists to catch.
        // BCE21-5 (r23 wave 2, lane L8): `i != a.length` on its "differ"
        // edge, for an `i` that starts at 0 and steps by exactly 1 through
        // that edge -- see [`ne_exit_bound`].
        {
            let ne = match (&cond.op, cond.inputs.as_slice()) {
                (Op::Cmp(CmpOp::Ne), [a, b]) => Some((*a, *b, 0u16)),
                (Op::Cmp(CmpOp::Eq), [a, b]) => Some((*a, *b, 1u16)),
                _ => None,
            };
            if let Some((a, b, edge)) = ne {
                if let Some(&true_block) = edge_block.get(&(id as NodeId, edge)) {
                    for (idx, len_id) in [(a, b), (b, a)] {
                        if let Some(base) =
                            ne_exit_bound(graph, schedule, &at_most, idx, len_id, true_block)
                        {
                            extra.push(UpperBound {
                                idx,
                                base,
                                true_block,
                            });
                        } else if ne_var_start {
                            // t1 wave 6 (lane B6): a non-constant start under
                            // the two pre-header guards.
                            if let Some(base) = ne_exit_var_start_bound(
                                graph,
                                schedule,
                                &NeStartFacts {
                                    at_most: &at_most,
                                    non_negative_at: &non_negative_at,
                                    guard_non_negative: &guard_non_negative,
                                },
                                idx,
                                len_id,
                                true_block,
                            ) {
                                extra.push(UpperBound {
                                    idx,
                                    base,
                                    true_block,
                                });
                                ne_start_phis.insert(idx);
                            }
                        }
                    }
                }
            }
        }
        let (idx, len_id, edge) = match (&cond.op, cond.inputs.as_slice()) {
            (Op::Cmp(CmpOp::Lt), [a, b]) => (*a, *b, 0u16),
            (Op::Cmp(CmpOp::Gt), [a, b]) => (*b, *a, 0u16),
            (Op::Cmp(CmpOp::Ge), [a, b]) => (*a, *b, 1u16),
            (Op::Cmp(CmpOp::Le), [a, b]) => (*b, *a, 1u16),
            _ => continue,
        };
        let Some(len) = graph.nodes.get(len_id as usize) else {
            continue;
        };
        let Some(&true_block) = edge_block.get(&(id as NodeId, edge)) else {
            continue;
        };
        // `Op::ArrayLength` is `[ctrl, mem, array_ref]`.
        let direct = if matches!(len.op, Op::ArrayLength) {
            len.inputs.get(2).copied()
        } else {
            None
        };
        if let Some(base) = direct {
            out.push(UpperBound {
                idx,
                base,
                true_block,
            });
        }
        // `i < Math.min(x, base.length)`: the minimum is at most each operand,
        // wherever it is defined.
        if matches!(len.op, Op::ScalarIntrinsic(crate::ir::ScalarOp::MinI)) {
            for &operand in len.inputs.iter() {
                let base = graph
                    .nodes
                    .get(operand as usize)
                    .filter(|o| o.op == Op::ArrayLength)
                    .and_then(|o| o.inputs.get(2).copied());
                if let Some(base) = base {
                    out.push(UpperBound {
                        idx,
                        base,
                        true_block,
                    });
                }
            }
        }
        let key = len_key(graph, len_id);
        for &base in alloc_by_len.get(&key).map_or(&[][..], Vec::as_slice) {
            if Some(base) != direct {
                out.push(UpperBound {
                    idx,
                    base,
                    true_block,
                });
            }
        }
        for f in at_most.get(&key).map_or(&[][..], Vec::as_slice) {
            let holds_here = if f.after_block_start {
                dominates(schedule, true_block, f.block)
            } else {
                dominates_reflexive(schedule, true_block, f.block)
            };
            if holds_here && Some(f.base) != direct {
                out.push(UpperBound {
                    idx,
                    base: f.base,
                    true_block,
                });
            }
        }
        // `idx < a.length - k`, `k >= 1` (wave 8).
        if let Some(base) = below_own_length(graph, len_id) {
            extra.push(UpperBound {
                idx,
                base,
                true_block,
            });
        }
        // The same fact with its margin kept (wave 9): `idx + k < len`.
        if let Some((base, k)) = length_margin(graph, len_id) {
            offsets.entry(idx).or_default().push(OffsetBound {
                base,
                true_block,
                margin: k,
            });
        }
    }

    // Decreasing induction φs whose start is below a length (wave 8).
    for (id, node) in graph.nodes.iter().enumerate() {
        if node.op != Op::Phi || node.ty != crate::ir::IrType::Int {
            continue;
        }
        let Some((init, header_block)) =
            decreasing_induction(graph, schedule, id as NodeId, node, &non_negative_at)
        else {
            continue;
        };
        let mut bases: Vec<NodeId> = Vec::new();
        if let Some(base) = below_own_length(graph, init) {
            bases.push(base);
        }
        for f in below
            .get(&len_key(graph, init))
            .map_or(&[][..], Vec::as_slice)
        {
            if fact_holds_at(schedule, f, header_block) && !bases.contains(&f.base) {
                bases.push(f.base);
            }
        }
        for base in bases {
            extra.push(UpperBound {
                idx: id as NodeId,
                base,
                true_block: header_block,
            });
        }
    }
    out.extend(extra);
    RangeFacts {
        bounds: out,
        non_negative_at,
        offsets,
        const_below,
        at_most,
        alloc_by_len,
        ne_start_phis,
    }
}



/// BCE21-5: `Some(base)` when `idx != len` holding at `true_block` proves
/// `idx < base.length` there: `len` is `ArrayLength(base)` of an array defined
/// in a block that strictly dominates the loop header (so every trip reads
/// one length), and `idx` is a header φ entering as the constant `0` and
/// stepping by exactly `+1` on back edges that all pass `true_block`
/// ([`is_unit_stride_induction`] with stride 1).
///
/// Why: by induction over the trips, `0 <= idx <= len` at the header. It holds
/// on entry (`0 <= len`), and an increment happens only after `idx != len`,
/// i.e. (with `idx <= len`) `idx < len`, so `idx + 1 <= len` -- no wrap, since
/// `len <= VM_MAX_ARRAY_LENGTH`. At `true_block` `idx != len` and `idx <= len`
/// give `idx < len`. A stride above 1 could step over `len`, so it is not
/// admitted.
///
/// A constant start `c >= 1` (the r23 L8 close, L8-5,
/// `CRATONVM_JIT_IR_BCE_NE_EXIT_START`): admitted when a `c <= base.length`
/// fact holds at the loop header -- the pre-header guard
/// `ir_optimize::range_predicate_plan` plants for exactly this shape, or a
/// dominating test. The induction's base case is then that fact; without it
/// `for (i = 5; i != a.length; i++)` over a 3-element array walks off the end,
/// which is why the check stays when no fact holds.
fn ne_exit_bound(
    graph: &Graph,
    schedule: &Schedule,
    at_most: &FxHashMap<LenKey, Vec<AtMostLength>>,
    idx: NodeId,
    len_id: NodeId,
    true_block: usize,
) -> Option<NodeId> {
    let len = graph.nodes.get(len_id as usize)?;
    if len.op != Op::ArrayLength {
        return None;
    }
    // `Op::ArrayLength` is `[ctrl, mem, array_ref]`.
    let base = *len.inputs.get(2)?;
    let phi = graph.nodes.get(idx as usize)?;
    if phi.op != Op::Phi || phi.ty != crate::ir::IrType::Int {
        return None;
    }
    let &[region, init, _] = phi.inputs.as_slice() else {
        return None;
    };
    let start = int_const(graph, init)?;
    if !(0..=i64::from(i32::MAX)).contains(&start)
        || (start != 0 && !bce_ne_exit_start_enabled())
        || !is_unit_stride_induction(graph, schedule, idx, phi, true_block, 1)
    {
        return None;
    }
    let header_block = *schedule.node_to_block.get(region as usize)?;
    let base_block = *schedule.node_to_block.get(base as usize)?;
    if base_block >= schedule.blocks.len() || !dominates(schedule, header_block, base_block) {
        return None;
    }
    // A non-zero start needs `start <= base.length` on entry (L8-5).
    if start != 0
        && !at_most.get(&LenKey::Const(start)).is_some_and(|fs| {
            fs.iter()
                .any(|f| f.base == base && fact_holds_at(schedule, f, header_block))
        })
    {
        return None;
    }
    Some(base)
}

/// `CRATONVM_JIT_IR_BCE_NE_EXIT_START` -- default ON (`=0` kills; the r23 L8
/// close, L8-5): [`ne_exit_bound`] admits a constant start `c >= 1` under a
/// `c <= base.length` fact, and `ir_optimize::range_predicate_plan` plants
/// that fact as a pre-header guard. Read per compile; no `static`.
pub(crate) fn bce_ne_exit_start_enabled() -> bool {
    true
}

// ── `!=` exit with a non-constant start (t1 wave 6, lane B6, BCE21-5) ──
//
// `for (int i = s; i != a.length; i++) .. a[i] ..` with `s` a value the
// compiler cannot see (a parameter, a field read, `off + k`).
//
// # The rule
//
// `a[i]` is in range at block `b` when `i != a.length` (an `Ne` true edge or
// an `Eq` false edge, either operand order) opens a block `f` dominating `b`,
// and
//
// * `i` is an `int` header φ `φ(header; s, i + 1)`: the back value is exactly
//   `Add(i, Const(1))` (the constant typed `int` and spelled `1`), and EVERY
//   back edge into the header leaves a block `f` dominates
//   ([`unit_step_from_any_start`]);
// * `a` is defined in a block that strictly dominates the header (one length
//   for every trip);
// * `s <= a.length` holds on entry to the header: an [`AtMostLength`] fact
//   for `a` keyed by `s` (the pre-header guard
//   `ir_optimize::range_predicate_plan` plants, a dominating `If` edge, or a
//   dominating guard), or `s` IS `a.length`; and
// * `0 <= s` holds on entry to the header: `s` is a length, or an `If`-edge
//   fact `s >= c`, `c >= 0`, opens a block dominating the header, or a
//   `Guard` stating it sits in a block STRICTLY dominating the header (a
//   guard's fact holds only after the guard) -- the second pre-header guard
//   `0 <= s` the same planter adds.
//
// A constant start is the constant arms' ([`ne_exit_bound`]); this arm takes
// only a non-constant `s`.
//
// # Why it is sound
//
// Claim: at the header, `s <= i <= a.length` on every trip. Write `n =
// a.length` (one value: `a` is fixed for the loop, a Java array's length never
// changes, and `n <= VM_MAX_ARRAY_LENGTH = i32::MAX - 2`).
//
// * Base: the first value of `i` is `s`, and `s <= n` holds on entry (the
//   fact's block dominates the header, so every entry passed it; `s` and `n`
//   are SSA values, so the fact is about the very values the loop sees -- the
//   fact's compare reads `s`, so its block is dominated by `s`'s definition,
//   and no path from that definition to the header avoids the fact block).
// * Step (back-edge dominance): a back edge leaves a block dominated by `f`,
//   so on the iteration that takes it the test `i != n` was evaluated on THIS
//   iteration's `i` and held. With the hypothesis `i <= n` that is `i < n`.
// * Stride exactly 1, no overflow: the next value is `i + 1 <= n <= i32::MAX -
//   2`, so the 32-bit `Add` is exact; and `i + 1 > i >= s`. A stride of 2 (or
//   any `k > 1`) could step from `n - 1` to `n + 1` and over the exit, after
//   which nothing bounds `i` until it wraps through `i32::MIN` -- refused.
//   The step is read as the node `Const(1)` typed `int`; `Graph::add_inputs`
//   stores every `int` constant sign-extended (`ir_verify::check_int_const_width`),
//   so that spelling is exactly the `int` value 1 and an unsigned or
//   high-bits spelling of 1 (`0x1_0000_0001`) arrives as `Const(1)` too --
//   correctly, since the lowering's 32-bit `add` adds 1 either way.
//
// So at `f` (and every block it dominates), `i != n` and `i <= n` give `i <
// n`, and `i >= s >= 0` gives the lower half. The φ is `>= 0` everywhere it is
// defined, which is why it joins `mono_phis` (whose readers ask exactly that).
//
// A negative `s`, or `s > n`, fails one of the two pre-header guards: each is
// a deopt-only `Guard` resuming at the loop header with the loop-entry frame
// (`ir_optimize::install_range_predicates` and the sign guard beside it), so
// the interpreter runs the loop from its first test and throws the
// `ArrayIndexOutOfBoundsException` (`Index -1 out of bounds for length 3`,
// `Index 4 out of bounds for length 3`) itself, where the bytecode throws it.
// A header that keeps failing is de-specialised and gets no guards, and then
// this arm finds no facts and the checks stay. Without either fact the arm
// refuses: `for (i = -1; i != a.length; i++) a[i]` faults on its first trip,
// and `for (i = 5; i != 3; i++)` walks up to `i32::MAX` and wraps.
//
// `CRATONVM_JIT_IR_BCE_NE_EXIT_VAR_START` (default ON since j27 2026-10-28) gates the arm and the
// planter's two guards; `CRATONVM_JIT_IR_BCE_NE_EXIT=0` kills both.

/// `CRATONVM_JIT_IR_BCE_NE_EXIT_VAR_START` -- **default ON since j27 2026-10-28**
/// (t1 wave 6, lane B6, BCE21-5 residual; perf, `=0` kills): [`ne_exit_var_start_bound`]
/// and the two pre-header guards `ir_optimize::range_predicate_plan` plants
/// for it. Needs `CRATONVM_JIT_IR_BCE_NE_EXIT` (default ON). Read per compile;
/// no `static`.
pub(crate) fn bce_ne_exit_var_start_enabled() -> bool {
    true
}

/// The facts [`ne_exit_var_start_bound`] reads, from [`upper_bounds`].
struct NeStartFacts<'a> {
    /// Every `x <= base.length` fact, keyed by `LenKey` of `x`.
    at_most: &'a FxHashMap<LenKey, Vec<AtMostLength>>,
    /// `If`-edge `v >= c`, `c >= 0` blocks, keyed by `v`.
    non_negative_at: &'a FxHashMap<NodeId, Vec<usize>>,
    /// `Guard` `v >= c`, `c >= 0` blocks (fact after the guard), keyed by `v`.
    guard_non_negative: &'a FxHashMap<NodeId, Vec<usize>>,
}

/// t1 wave 6 (lane B6): `Some(base)` when `idx != len` holding at
/// `true_block` proves `0 <= idx < base.length` there for an `idx` entering
/// the loop as a NON-constant `s` (see the section notes above for the rule
/// and its soundness argument).
fn ne_exit_var_start_bound(
    graph: &Graph,
    schedule: &Schedule,
    facts: &NeStartFacts<'_>,
    idx: NodeId,
    len_id: NodeId,
    true_block: usize,
) -> Option<NodeId> {
    let len = graph.nodes.get(len_id as usize)?;
    if len.op != Op::ArrayLength {
        return None;
    }
    // `Op::ArrayLength` is `[ctrl, mem, array_ref]`.
    let base = *len.inputs.get(2)?;
    let phi = graph.nodes.get(idx as usize)?;
    if phi.op != Op::Phi || phi.ty != crate::ir::IrType::Int {
        return None;
    }
    let (header_block, init) = unit_step_from_any_start(graph, schedule, idx, phi, true_block)?;
    let init_node = graph.nodes.get(init as usize)?;
    // A constant start is [`ne_exit_bound`]'s, under its own switches.
    if matches!(init_node.op, Op::Const(_)) || init_node.ty != crate::ir::IrType::Int {
        return None;
    }
    let base_block = *schedule.node_to_block.get(base as usize)?;
    if base_block >= schedule.blocks.len() || !dominates(schedule, header_block, base_block) {
        return None;
    }
    // `s <= base.length` on entry to the header.
    let key = len_key(graph, init);
    let upper = key == LenKey::LengthOf(base)
        || facts.at_most.get(&key).is_some_and(|fs| {
            fs.iter()
                .any(|f| f.base == base && fact_holds_at(schedule, f, header_block))
        });
    // `0 <= s` on entry to the header. An `If` edge holds from its block's
    // start; a guard only after itself, so its block must STRICTLY dominate.
    let lower = init_node.op == Op::ArrayLength
        || facts.non_negative_at.get(&init).is_some_and(|bs| {
            bs.iter()
                .any(|&f| dominates_reflexive(schedule, header_block, f))
        })
        || facts
            .guard_non_negative
            .get(&init)
            .is_some_and(|bs| bs.iter().any(|&f| dominates(schedule, header_block, f)));
    (upper && lower).then_some(base)
}

/// t1 wave 6 (lane B6): `(header block, entry value)` when `phi_id` is the
/// `int` φ `φ(header; init, phi + 1)` of a loop header with exactly one entry
/// edge and one back edge, the step spelled `Const(1)` typed `int`, and every
/// back edge into the header leaving a block `guard_block` dominates. Unlike
/// [`is_unit_stride_induction`] the entry value may be anything; which input
/// is the entry is read off the CFG (as [`decreasing_induction`] does), not
/// off the input order.
fn unit_step_from_any_start(
    graph: &Graph,
    schedule: &Schedule,
    phi_id: NodeId,
    phi: &crate::ir::Node,
    guard_block: usize,
) -> Option<(usize, NodeId)> {
    let &[region_id, v1, v2] = phi.inputs.as_slice() else {
        return None;
    };
    let region = graph.nodes.get(region_id as usize)?;
    if !matches!(region.op, Op::Merge | Op::Region) {
        return None;
    }
    let &[c1, c2] = region.inputs.as_slice() else {
        return None;
    };
    let header_block = block_of(schedule, region_id)?;
    if !schedule.dom.is_reachable(header_block) {
        return None;
    }
    let is_back = |ctrl: NodeId| -> Option<bool> {
        let blk = block_of(schedule, ctrl)?;
        Some(dominates_reflexive(schedule, blk, header_block))
    };
    let (init, next) = match (is_back(c1)?, is_back(c2)?) {
        (false, true) => (v1, v2),
        (true, false) => (v2, v1),
        _ => return None,
    };
    let next_node = graph.nodes.get(next as usize)?;
    if next_node.op != Op::Add || next_node.ty != crate::ir::IrType::Int {
        return None;
    }
    let is_one = |n: NodeId| {
        graph
            .nodes
            .get(n as usize)
            .is_some_and(|x| x.op == Op::Const(1) && x.ty == crate::ir::IrType::Int)
    };
    let unit = match next_node.inputs.as_slice() {
        &[a, b] => (a == phi_id && is_one(b)) || (b == phi_id && is_one(a)),
        _ => false,
    };
    if !unit {
        return None;
    }
    let header = schedule.blocks.get(header_block)?;
    let mut saw_back_edge = false;
    for &p in &header.predecessors {
        if !dominates_reflexive(schedule, p, header_block) {
            continue;
        }
        saw_back_edge = true;
        if !dominates_reflexive(schedule, p, guard_block) {
            return None;
        }
    }
    saw_back_edge.then_some((header_block, init))
}

/// What [`upper_bounds`] proves, for [`analyze`].
struct RangeFacts {
    /// Every `idx < base.length` fact.
    bounds: Vec<UpperBound>,
    /// The blocks where a value is `>= 0` by an `If` edge.
    non_negative_at: FxHashMap<NodeId, Vec<usize>>,
    /// `idx < base.length - margin` facts, keyed by `idx`.
    offsets: FxHashMap<NodeId, Vec<OffsetBound>>,
    /// nio20 wave 6 (lane L4): `c < base.length` facts with `c` a
    /// non-negative `int` constant, keyed by `base`, for the constant-mask
    /// arm ([`const_mask_in_range`]).
    const_below: FxHashMap<NodeId, Vec<ConstBelow>>,
    /// nio21 wave 4 (lane L5): every `x <= base.length` fact, keyed by
    /// `LenKey` of `x`, for the room-offset arm ([`room_offset_in_range`]).
    at_most: FxHashMap<LenKey, Vec<AtMostLength>>,
    /// nio21 wave 4 (lane L5): the arrays allocated with a count, keyed by
    /// `LenKey` of the count (`b.length == x`), for the same arm.
    alloc_by_len: FxHashMap<LenKey, Vec<NodeId>>,
    /// t1 wave 6 (lane B6): the header φs [`ne_exit_var_start_bound`]
    /// admitted, each `>= 0` wherever it is defined.
    ne_start_phis: FxHashSet<NodeId>,
}

/// `c < base.length` on entry to (an `If` edge) or after the node in (a
/// `Guard`) `at.block`, with `c` a non-negative `int` constant (nio20 wave 6,
/// lane L4). Read for an index `p & m` with `0 <= m <= c`.
struct ConstBelow {
    c: i64,
    at: AtMostLength,
}

/// `m` when `idx` is the `int` `p & m` (either operand order) with `m` an
/// `int` constant in `0..=i32::MAX` (nio20 wave 6, lane L4). Such an index is
/// in `[0, m]` whatever `p` is: `m`'s sign bit is clear, so the result's is,
/// and every bit the result has is a bit of `m`, so (both non-negative) it is
/// at most `m`. `ir_optimize`'s mask predicate plants `m < a.length` for it.
pub(crate) fn const_mask_of(graph: &Graph, idx: NodeId) -> Option<i64> {
    let n = graph.nodes.get(idx as usize)?;
    if n.op != Op::And || n.ty != crate::ir::IrType::Int {
        return None;
    }
    let [a, b] = n.inputs.as_slice() else {
        return None;
    };
    let in_range = |c: i64| (0..=i64::from(i32::MAX)).contains(&c);
    match (int_const(graph, *a), int_const(graph, *b)) {
        (_, Some(m)) if in_range(m) => Some(m),
        (Some(m), _) if in_range(m) => Some(m),
        _ => None,
    }
}

/// Is an index `p & m` ([`const_mask_of`]) of `base` in range at `block`?
/// Yes when `m < base.length` is known there: `base` a fresh array of a
/// constant length above `m`, or a dominating [`ConstBelow`] fact with `c >=
/// m` for `base` itself (an `If` edge from the start of its block, a `Guard`
/// -- the mask predicate's -- only strictly after its block).
fn const_mask_in_range(
    graph: &Graph,
    schedule: &Schedule,
    facts: &FxHashMap<NodeId, Vec<ConstBelow>>,
    trusted_arrays: &FxHashMap<NodeId, i32>,
    base: NodeId,
    m: i64,
    block: usize,
) -> bool {
    if trusted_arrays.get(&base).is_some_and(|&len| m < i64::from(len)) {
        return true;
    }
    let fresh_len = graph
        .nodes
        .get(base as usize)
        .filter(|b| matches!(b.op, Op::NewArray { .. }))
        .and_then(|b| b.inputs.get(2).copied())
        .and_then(|len| int_const(graph, len));
    if fresh_len.is_some_and(|n| m < n) {
        return true;
    }
    facts.get(&base).is_some_and(|fs| {
        fs.iter()
            .any(|f| f.c >= m && f.at.base == base && fact_holds_at(schedule, &f.at, block))
    })
}



/// The [`ConstBelow`] reading of a `x < base.length` (`strict`) or `x <=
/// base.length` fact whose `x` is an `int` constant: `c = x` or `x - 1`,
/// kept only when `c >= 0` (a negative `c` bounds no mask).
fn const_below_of(
    graph: &Graph,
    x: NodeId,
    strict: bool,
    base: NodeId,
    block: usize,
    after_block_start: bool,
) -> Option<ConstBelow> {
    let v = int_const(graph, x)?;
    let c = if strict { v } else { v.checked_sub(1)? };
    if !(0..=i64::from(i32::MAX)).contains(&c) {
        return None;
    }
    Some(ConstBelow {
        c,
        at: AtMostLength {
            base,
            block,
            after_block_start,
        },
    })
}

/// `idx < base.length - margin` on entry to `true_block` (round 11 wave 9,
/// mid proposal W8-2). An access at `idx + j` with `0 <= j <= margin` is then
/// below the length: `idx + j <= idx + margin < len`. The 32-bit sum cannot
/// wrap, because it is below `len <= i32::MAX`, and with `idx >= 0` (proven
/// separately, as for any index) it is non-negative.
struct OffsetBound {
    base: NodeId,
    true_block: usize,
    margin: i64,
}

/// `(base, k)` when `x` is `base.length - k` with `k >= 1` — the same shapes
/// as [`below_own_length`], with the distance kept. Neither form wraps: a
/// length is `>= 0`, `k <= i32::MAX` for `Sub`, and an `Add` of a negative
/// `int` constant is at least `i32::MIN`.
fn length_margin(graph: &Graph, x: NodeId) -> Option<(NodeId, i64)> {
    let n = graph.nodes.get(x as usize)?;
    if n.ty != crate::ir::IrType::Int {
        return None;
    }
    let length_of = |id: NodeId| {
        graph
            .nodes
            .get(id as usize)
            .filter(|l| l.op == Op::ArrayLength)
            .and_then(|l| l.inputs.get(2).copied())
    };
    let positive = |c: i64| (1..=i64::from(i32::MAX)).contains(&c);
    let negative = |c: i64| (i64::from(i32::MIN)..=-1).contains(&c);
    match (&n.op, n.inputs.as_slice()) {
        (Op::Sub, [l, r]) => {
            let k = int_const(graph, *r).filter(|&k| positive(k))?;
            Some((length_of(*l)?, k))
        }
        (Op::Add, [l, r]) => {
            if let Some(c) = int_const(graph, *r).filter(|&c| negative(c)) {
                Some((length_of(*l)?, -c))
            } else {
                let c = int_const(graph, *l).filter(|&c| negative(c))?;
                Some((length_of(*r)?, -c))
            }
        }
        _ => None,
    }
}

/// `(i, j)` when `idx` is the `int` sum `i + j` with `j` a constant in
/// `0..=i32::MAX`, in either operand order.
fn offset_index(graph: &Graph, idx: NodeId) -> Option<(NodeId, i64)> {
    let n = graph.nodes.get(idx as usize)?;
    if n.op != Op::Add || n.ty != crate::ir::IrType::Int {
        return None;
    }
    let small = |c: i64| (0..=i64::from(i32::MAX)).contains(&c);
    match n.inputs.as_slice() {
        [a, b] => match (int_const(graph, *a), int_const(graph, *b)) {
            (None, Some(j)) if small(j) => Some((*a, j)),
            (Some(j), None) if small(j) => Some((*b, j)),
            _ => None,
        },
        _ => None,
    }
}

/// Whether the fact `f` holds on entry to `block`: from the start of its own
/// block for an `If` edge fact, only after the node for a `Guard` fact.
fn fact_holds_at(schedule: &Schedule, f: &AtMostLength, block: usize) -> bool {
    if f.after_block_start {
        dominates(schedule, block, f.block)
    } else {
        dominates_reflexive(schedule, block, f.block)
    }
}

/// The value of an `int` constant node, if `id` is one, read at `int` width.
///
/// t1 jitbce (2026-10-25): an `int` `Op::Const` may hold its unsigned 32-bit
/// pattern (`ir_optimize`'s `an_int_minus_one_is_read_at_int_width` pins that
/// `Const(0xFFFF_FFFF)` typed `Int` is `-1`), while the 32-bit compare the
/// lowering emits reads only the low half. Returning the raw `i64` let a
/// `v >= 0xFFFF_FFFF` reading (really `v >= -1`) prove `v >= 0` in
/// [`non_negative_by`]; every reader here reasons about the `int` value, so the
/// constant is sign-extended from its low 32 bits.
fn int_const(graph: &Graph, id: NodeId) -> Option<i64> {
    let n = graph.nodes.get(id as usize)?;
    match n.op {
        // Cast: an `int` constant is its low 32 bits.
        Op::Const(c) if n.ty == crate::ir::IrType::Int => Some(i64::from(c as i32)),
        _ => None,
    }
}

/// `(x, base)` when compare `cond`, evaluated to `holds`, states the STRICT
/// `x < base.length`. The sibling of [`at_most_length`] for the readings a
/// non-strict bound or a decreasing start needs.
fn strictly_below_length(graph: &Graph, cond: NodeId, holds: bool) -> Option<(NodeId, NodeId)> {
    let c = graph.nodes.get(cond as usize)?;
    let Op::Cmp(op) = c.op else {
        return None;
    };
    let [a, b] = c.inputs.as_slice() else {
        return None;
    };
    let op = if holds { op } else { op.negate() };
    let (x, len) = match op {
        CmpOp::Lt => (*a, *b),
        CmpOp::Gt => (*b, *a),
        _ => return None,
    };
    let len_node = graph.nodes.get(len as usize)?;
    if len_node.op != Op::ArrayLength {
        return None;
    }
    let base = *len_node.inputs.get(2)?;
    Some((x, base))
}

/// `base` when `x` is `base.length - k` (`Sub` by `1..=i32::MAX`, or `Add` of
/// `i32::MIN..=-1`), which is STRICTLY below `base.length` wherever it is
/// defined: a length is `>= 0`, so the subtraction cannot wrap.
pub(crate) fn below_own_length(graph: &Graph, x: NodeId) -> Option<NodeId> {
    let n = graph.nodes.get(x as usize)?;
    if n.ty != crate::ir::IrType::Int {
        return None;
    }
    let length_of = |id: NodeId| {
        graph
            .nodes
            .get(id as usize)
            .filter(|l| l.op == Op::ArrayLength)
            .and_then(|l| l.inputs.get(2).copied())
    };
    let positive = |c: i64| (1..=i64::from(i32::MAX)).contains(&c);
    let negative = |c: i64| (i64::from(i32::MIN)..=-1).contains(&c);
    match (&n.op, n.inputs.as_slice()) {
        (Op::Sub, [l, r]) if int_const(graph, *r).is_some_and(positive) => length_of(*l),
        (Op::Add, [l, r]) if int_const(graph, *r).is_some_and(negative) => length_of(*l),
        (Op::Add, [l, r]) if int_const(graph, *l).is_some_and(negative) => length_of(*r),
        _ => None,
    }
}

/// `m` when `idx` is `p & m` (either operand order) and `m` is `base.length -
/// k`, `k >= 1` ([`below_own_length`]) -- the hashed-table index
/// `tab[(n - 1) & hash]` of `HashMap.getNode` and every power-of-two table.
///
/// Round 12 wave 3 (lane iropt2). The access is in range once `m >= 0` holds
/// at it, and the caller proves that separately (`n > 0` on the edge the
/// access hangs off, read through the path-sensitive lattice):
///
/// * `m >= 0` has its sign bit clear, so `p & m` does too: `idx >= 0`;
/// * every bit set in `p & m` is set in `m`, so `idx <= m` (both are
///   non-negative, so the unsigned order is the signed one);
/// * `m = base.length - k <= base.length - 1`, without wrapping, because a
///   length is `>= 0` and `k` is at most `i32::MAX`.
///
/// Without `m >= 0` nothing follows: an empty table gives `m == -1` and `idx ==
/// p`. `CRATONVM_JIT_IR_BCE_MASK_INDEX=0` turns the arm off.
fn masked_below_length(graph: &Graph, idx: NodeId, base: NodeId) -> Option<NodeId> {
    let n = graph.nodes.get(idx as usize)?;
    if n.op != Op::And || n.ty != crate::ir::IrType::Int {
        return None;
    }
    let [a, b] = n.inputs.as_slice() else {
        return None;
    };
    [*a, *b]
        .into_iter()
        .find(|&m| below_own_length(graph, m) == Some(base))
}

/// `CRATONVM_JIT_IR_BCE_CLAMP_INDEX` — **default on** (default on; JIT round 19
/// wave 4, lane irops3, IROPS2-7 of `docs/feature-designs/jit-ir-optimizations.md`): the
/// bounds check of a clamped index (`clamp_index_needs`) is dropped when the
/// array is long enough. Read once per [`analyze`]; no `static`
/// (`process_global_statics_ratchet`).
fn clamp_index_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_BCE_CLAMP_PREDICATE` -- **default on** (JIT round 19
/// wave 5, lane osr5, IROPS3-1 of `jit-r19-irops3-proposals.md`): with
/// `CRATONVM_JIT_IR_BCE_CLAMP_INDEX`, a loop whose body indexes an invariant
/// array by a clamp needing `a.length >= 1` gets a loop-entry predicate
/// `Guard(0 < a.length)` (`ir_optimize::plant_clamp_length_predicates`), and
/// the clamp arm reads it, so the FIRST clamped access loses its check too.
/// No `static` (`process_global_statics_ratchet`).
pub(crate) fn clamp_predicate_enabled() -> bool {
    clamp_index_enabled()
        && false}

/// IROPS3-1: is `a[idx]` a clamped access whose check needs exactly
/// `a.length >= 1` (both clamp idioms), with no operand proof beyond the
/// shape itself? What the loop-entry predicate is planted for.
pub(crate) fn clamp_needs_a_non_empty_array(graph: &Graph, idx: NodeId, base: NodeId) -> bool {
    clamp_index_needs(graph, idx, base, &|_| false) == Some(1)
}

/// IROPS3-1: the array whose length `cond` proves `>= 1` when non-zero:
/// `Cmp(Lt)[c, len]` with `c >= 0`, `Cmp(Le)[c, len]` with `c >= 1`, or the
/// mirrored `Cmp(Gt)[len, c]` / `Cmp(Ge)[len, c]`, `len` an `ArrayLength`.
fn length_positive_by(graph: &Graph, cond: NodeId) -> Option<NodeId> {
    let c = graph.nodes.get(cond as usize)?;
    let (k, len, strict) = match (&c.op, c.inputs.as_slice()) {
        (Op::Cmp(CmpOp::Lt), [k, l]) => (*k, *l, true),
        (Op::Cmp(CmpOp::Le), [k, l]) => (*k, *l, false),
        (Op::Cmp(CmpOp::Gt), [l, k]) => (*k, *l, true),
        (Op::Cmp(CmpOp::Ge), [l, k]) => (*k, *l, false),
        _ => return None,
    };
    let Op::Const(kv) = graph.nodes.get(k as usize)?.op else {
        return None;
    };
    // Cast: the compare reads the low 32 bits of an `int` constant.
    let kv = i64::from(kv as i32);
    if !(if strict { kv >= 0 } else { kv >= 1 }) {
        return None;
    }
    let l = graph.nodes.get(len as usize)?;
    (l.op == Op::ArrayLength).then(|| l.inputs.get(2).copied())?
}

/// Is `idx` an `int` `Math.min` / `Math.max` -- the root of a clamp?
fn is_clamp_root(graph: &Graph, idx: NodeId) -> bool {
    graph.nodes.get(idx as usize).is_some_and(|n| {
        n.ty == crate::ir::IrType::Int
            && matches!(
                n.op,
                Op::ScalarIntrinsic(crate::ir::ScalarOp::MinI | crate::ir::ScalarOp::MaxI)
            )
    })
}

/// JIT round 19 wave 4 (lane irops3, IROPS2-7): the smallest `L` such that
/// `0 <= idx < base.length` holds whenever `base.length >= L`, for an index
/// built of `int` `Math.min` / `Math.max` over constants, `base.length - k`
/// and values `nonneg` proves `>= 0` -- the clamp idioms
/// `max(0, min(i, a.length - 1))` and `min(max(i, 0), a.length - 1)` (both
/// `L = 1`). `None` when the shape proves nothing.
///
/// Two recursive halves, each answering "the least `L` under which this half
/// holds", combined by the obvious rules -- `min(x, y)` is below a bound when
/// EITHER operand is and non-negative when BOTH are; `max` the other way
/// round:
///
/// * `upper(v)`: `v < base.length`. `base.length - k` (`k >= 1`) always
///   (`below_own_length`, `L = 0`); a constant `c >= 0` when the length is at
///   least `c + 1`; a negative constant always.
/// * `lower(v)`: `v >= 0`. A constant `c >= 0` and any array length always;
///   `base.length - k` when the length is at least `k` (`length_margin`, same
///   base only); anything `nonneg` proves always.
///
/// The book (IROPS2-7) read the clamp's range `[0, last.hi]` as dropping the
/// whole check. It does not, alone: for an EMPTY array both idioms produce an
/// out-of-range index (`0`, resp. `-1`) and the check must throw. So the arm
/// returns the length it needs, and [`analyze`] drops the check only with a
/// fact `base.length >= L` at the access (a dominating access of the same
/// array, a dominating `j < base.length` with `j >= 0`, or the path-sensitive
/// range of the length).
///
/// Every value here is one SSA value, so `idx` is the same number the
/// bounds check would have compared. Depth-bounded: deeper shapes refuse.
fn clamp_index_needs(
    graph: &Graph,
    idx: NodeId,
    base: NodeId,
    nonneg: &dyn Fn(NodeId) -> bool,
) -> Option<i64> {
    if !is_clamp_root(graph, idx) {
        return None;
    }
    let up = clamp_upper(graph, idx, base, 0)?;
    let lo = clamp_lower(graph, idx, base, nonneg, 0)?;
    Some(up.max(lo))
}

/// The bound on [`clamp_index_needs`]' recursion: a clamp is two levels.
const CLAMP_MAX_DEPTH: usize = 4;

/// The `int` `MinI`/`MaxI` operands of `v`, with which one it is.
fn clamp_minmax(graph: &Graph, v: NodeId) -> Option<(bool, NodeId, NodeId)> {
    let n = graph.nodes.get(v as usize)?;
    if n.ty != crate::ir::IrType::Int {
        return None;
    }
    let is_min = match n.op {
        Op::ScalarIntrinsic(crate::ir::ScalarOp::MinI) => true,
        Op::ScalarIntrinsic(crate::ir::ScalarOp::MaxI) => false,
        _ => return None,
    };
    match n.inputs.as_slice() {
        [a, b] => Some((is_min, *a, *b)),
        _ => None,
    }
}

/// `upper(v)` of [`clamp_index_needs`].
fn clamp_upper(graph: &Graph, v: NodeId, base: NodeId, depth: usize) -> Option<i64> {
    if depth > CLAMP_MAX_DEPTH {
        return None;
    }
    if below_own_length(graph, v) == Some(base) {
        return Some(0);
    }
    if let Some(c) = int_const(graph, v) {
        // Cast-free: an `int` constant, so `c + 1` cannot overflow `i64`.
        return Some(if c < 0 { 0 } else { c + 1 });
    }
    let (is_min, a, b) = clamp_minmax(graph, v)?;
    let ua = clamp_upper(graph, a, base, depth + 1);
    let ub = clamp_upper(graph, b, base, depth + 1);
    if is_min {
        match (ua, ub) {
            (Some(x), Some(y)) => Some(x.min(y)),
            (x, y) => x.or(y),
        }
    } else {
        Some(ua?.max(ub?))
    }
}

/// `lower(v)` of [`clamp_index_needs`].
fn clamp_lower(
    graph: &Graph,
    v: NodeId,
    base: NodeId,
    nonneg: &dyn Fn(NodeId) -> bool,
    depth: usize,
) -> Option<i64> {
    if depth > CLAMP_MAX_DEPTH {
        return None;
    }
    if let Some(c) = int_const(graph, v) {
        return (0..=i64::from(i32::MAX)).contains(&c).then_some(0);
    }
    if graph
        .nodes
        .get(v as usize)
        .is_some_and(|n| n.op == Op::ArrayLength)
    {
        return Some(0);
    }
    if let Some((b, k)) = length_margin(graph, v) {
        if b == base {
            return Some(k);
        }
    }
    if let Some((is_min, a, b)) = clamp_minmax(graph, v) {
        let la = clamp_lower(graph, a, base, nonneg, depth + 1);
        let lb = clamp_lower(graph, b, base, nonneg, depth + 1);
        let r = if is_min {
            la.zip(lb).map(|(x, y)| x.max(y))
        } else {
            match (la, lb) {
                (Some(x), Some(y)) => Some(x.min(y)),
                (x, y) => x.or(y),
            }
        };
        if r.is_some() {
            return r;
        }
    }
    nonneg(v).then_some(0)
}

/// [`proven_non_negative`] without its induction-variable shape arm, for a
/// value with no bound of its own to prove non-negativity THROUGH (the clamp
/// arm's operands): a constant, an array length, an `If`-edge fact
/// dominating the access, or the path-sensitive lattice at the access. The
/// phi arm is left out because its argument needs the block holding the
/// loop's own `i < x` test, which a clamp operand does not have.
fn proven_non_negative_without_induction(
    graph: &Graph,
    schedule: &Schedule,
    ranges: Option<&crate::range_analysis::PathRanges>,
    non_negative_at: &FxHashMap<NodeId, Vec<usize>>,
    v: NodeId,
    access_block: usize,
) -> bool {
    let Some(node) = graph.nodes.get(v as usize) else {
        return false;
    };
    let by_shape = match &node.op {
        Op::Const(c) => node.ty == crate::ir::IrType::Int && (0..=i64::from(i32::MAX)).contains(c),
        Op::ArrayLength => true,
        _ => false,
    };
    by_shape
        || non_negative_at.get(&v).is_some_and(|blocks| {
            blocks
                .iter()
                .any(|&f| dominates_reflexive(schedule, access_block, f))
        })
        || ranges.is_some_and(|r| r.at(v, access_block).definitely_non_negative())
}



/// The value compare `cond`, evaluated to `holds`, proves `>= 0`: `v >= c`
/// (or `v > c - 1`) with `c` a non-negative `int` constant, in either operand
/// order.
fn non_negative_by(graph: &Graph, cond: NodeId, holds: bool) -> Option<NodeId> {
    lower_bound_by(graph, cond, holds).and_then(|(v, lo)| (lo >= 0).then_some(v))
}

/// `(v, lo)` when the value compare `cond`, evaluated to `holds`, states `v >=
/// lo` (or `v > lo - 1`) with `lo` read off an `int` constant at `int` width,
/// in either operand order. [`non_negative_by`] is its `lo >= 0` reading; the
/// j27 floor arm ([`monotone_phi_at_least`]) reads any `lo`.
fn lower_bound_by(graph: &Graph, cond: NodeId, holds: bool) -> Option<(NodeId, i64)> {
    let c = graph.nodes.get(cond as usize)?;
    let Op::Cmp(op) = c.op else {
        return None;
    };
    let [a, b] = c.inputs.as_slice() else {
        return None;
    };
    let op = if holds { op } else { op.negate() };
    let (v, lo) = match op {
        CmpOp::Ge => (*a, int_const(graph, *b)?),
        CmpOp::Gt => (*a, int_const(graph, *b)?.checked_add(1)?),
        CmpOp::Le => (*b, int_const(graph, *a)?),
        CmpOp::Lt => (*b, int_const(graph, *a)?.checked_add(1)?),
        CmpOp::Eq | CmpOp::Ne => return None,
    };
    Some((v, lo))
}

/// `φ(header; init, φ + k)` with `k` a negative `int` constant, where every
/// back edge into the header is dominated by an `If` edge proving `φ >= 0`.
/// Returns `(init, header block)`.
///
/// Every value such a φ takes is at most `init`. The step runs from a value
/// proven `>= 0` on that iteration (the φ is one SSA value from the header to
/// the back edge), so `φ + k >= k >= i32::MIN` and it cannot wrap: each value
/// is strictly below the previous one. The start is `init` as it was when the
/// loop was last entered, and `init` (defined outside the loop, since it
/// flows in on the entry edge) cannot be redefined without leaving the loop
/// and entering it again through that edge. So `φ <= init` everywhere the
/// header dominates.
fn decreasing_induction(
    graph: &Graph,
    schedule: &Schedule,
    phi_id: NodeId,
    phi: &crate::ir::Node,
    non_negative_at: &FxHashMap<NodeId, Vec<usize>>,
) -> Option<(NodeId, usize)> {
    let &[region_id, v1, v2] = phi.inputs.as_slice() else {
        return None;
    };
    let region = graph.nodes.get(region_id as usize)?;
    if !matches!(region.op, Op::Merge | Op::Region) {
        return None;
    }
    let &[c1, c2] = region.inputs.as_slice() else {
        return None;
    };
    let header_block = *schedule.node_to_block.get(region_id as usize)?;
    let header = schedule.blocks.get(header_block)?;
    // Which input is the back edge: its control's block is inside the loop.
    let is_back = |ctrl: NodeId| -> Option<bool> {
        let blk = *schedule.node_to_block.get(ctrl as usize)?;
        if blk >= schedule.blocks.len() {
            return None;
        }
        Some(dominates_reflexive(schedule, blk, header_block))
    };
    let (init, next) = match (is_back(c1)?, is_back(c2)?) {
        (false, true) => (v1, v2),
        (true, false) => (v2, v1),
        _ => return None,
    };
    let next_node = graph.nodes.get(next as usize)?;
    if next_node.op != Op::Add || next_node.ty != crate::ir::IrType::Int {
        return None;
    }
    let step = match *next_node.inputs.as_slice() {
        [a, b] if a == phi_id && b != phi_id => b,
        [a, b] if b == phi_id && a != phi_id => a,
        _ => return None,
    };
    if !int_const(graph, step).is_some_and(|k| (i64::from(i32::MIN)..=-1).contains(&k)) {
        return None;
    }
    let blocks = non_negative_at.get(&phi_id)?;
    let mut saw_back_edge = false;
    for &p in &header.predecessors {
        if !dominates_reflexive(schedule, p, header_block) {
            continue;
        }
        saw_back_edge = true;
        if !blocks.iter().any(|&n| dominates_reflexive(schedule, p, n)) {
            return None;
        }
    }
    saw_back_edge.then_some((init, header_block))
}

// ── Monotone inductions (nio21 wave 3, lane L5) ──────────────────────
//
// `UTF_8$Decoder.decodeArrayLoop` (JDK 25) is a NON-counted loop:
//
// ```java
// while (sp < sl) {
//     int b1 = sa[sp];
//     if (b1 >= 0) { if (dp >= dl) return xflow(..); da[dp++] = (char) b1; sp++; }
//     else if (..) { if (sl - sp < 2 || dp >= dl) return xflow(..); .. sp += 2; }
//     else if (..) { int srcRemaining = sl - sp; if (srcRemaining < 3 || dp >= dl) .. sp += 3; }
//     else if (..) { int srcRemaining = sl - sp; if (srcRemaining < 4 || dl - dp < 2) .. sp += 4; }
//     ..
// }
// ```
//
// `sp` and `dp` start from computed values and advance by 1..4 in several
// arms, so neither is the `[region, const, φ + s]` φ `is_unit_stride_induction`
// reads, and every `sa[sp]` / `da[dp]` kept both checks. HotSpot predicates
// the loop instead; the two halves here are `ir_optimize::monotone_predicate`
// (the pre-header guards `0 <= init`, `0 <= x`, `x <= a.length`) and this
// rule, which proves the `idx >= 0` half for such a φ.
//
// # The rule
//
// A header φ `p = φ(header; e.., b..)` is `>= 0` wherever it is defined when
//
// * every ENTRY value `e` (an input whose control is not inside the loop) is
//   `>= 0` on the edge that brings it: a non-negative `int` constant, an array
//   length, or a dominating `If`-edge or `Guard` fact `e >= c`, `c >= 0`; and
// * every BACK value `b` is `p` itself, `p + c` (a chain of `int` `Add`s of
//   constants in `0..=MONOTONE_MAX_STEP`, total `c >= 1`), or a φ of a FORWARD
//   merge (none of whose inputs comes from a block it dominates) whose every
//   input is again such a value along its own edge (at most
//   `MONOTONE_MERGE_DEPTH` merges deep), where each `p + c` flows along an
//   edge dominated by a fact that keeps it from wrapping:
//   * (a) `p < x` for any `int` `x`, and `c == 1`: `p + 1 <= x <= i32::MAX`;
//   * (b) `x - p >= k` (the 32-bit `Sub(x, p)` compared), `k >= c`, with `x`
//     known `>= 0` on that edge (a constant, a length, an edge or guard fact,
//     or a dominating `p < x`, which with `p >= 0` gives `x >= 1`).
//
// # Why it is sound
//
// By induction over the iterations. The first value of `p` is an entry value,
// `>= 0` by the first condition. Assume `p >= 0` on the current iteration
// (every use of `p` inside the loop sees the current iteration's value). A
// fact read from a block that dominates the back edge's block was evaluated
// on THIS iteration: the fact's `If` reads `p`, so it lies inside the loop,
// and a block inside the loop that dominates another one is passed on every
// iteration's path to it (a path from the header to that block avoiding the
// fact block, prefixed with the loop's first entry, would avoid it from the
// method entry). Then in (a) `p + c = p + 1 <= x <= i32::MAX`, and in (b)
// `x >= 0` and `p >= 0` make `x - p` exact (it lies in `[-i32::MAX,
// i32::MAX]`), so `p + c <= p + k <= x <= i32::MAX`. Either way the sum does
// not wrap and is `>= 0`, and so is every partial sum of the `Add` chain (each
// constant is `>= 0`). A forward merge's φ takes one of its inputs, each
// proven on its own edge. So the next value of `p` is `>= 0`.
//
// The rule says nothing about the UPPER half: `idx < base.length` is still the
// dominating `idx < x` fact plus `x <= base.length` that `upper_bounds` reads
// (for `decodeArrayLoop`, the pre-header guards). Fails closed on everything
// it does not recognise: an unscheduled control, an unreachable header, a
// back value of any other shape, a backward merge.
//
// `CRATONVM_JIT_IR_BCE_MONOTONE=0` drops the rule.

/// How many forward merges a back value may pass through.
const MONOTONE_MERGE_DEPTH: u32 = 4;

/// The largest single constant a monotone step may add (the proof needs only
/// `>= 0`; the bound keeps the running total far from `i64` overflow).
const MONOTONE_MAX_STEP: i64 = 1 << 16;

/// `CRATONVM_JIT_IR_BCE_MONOTONE` (nio21 wave 3, lane L5) -- **default ON**;
/// `0|false|off|no` drops the monotone-induction arm of the `idx >= 0` proof.
/// Read once per [`analyze`].
fn monotone_enabled() -> bool {
    true
}

/// The block control node `ctrl` was placed in, or `None` when the schedule
/// placed it nowhere.
fn block_of(schedule: &Schedule, ctrl: NodeId) -> Option<usize> {
    let b = *schedule.node_to_block.get(ctrl as usize)?;
    (b < schedule.blocks.len()).then_some(b)
}

/// Is `id` an `int`-typed node?
fn int_typed(graph: &Graph, id: NodeId) -> bool {
    graph
        .nodes
        .get(id as usize)
        .is_some_and(|n| n.ty == crate::ir::IrType::Int)
}

/// The edge and guard facts the monotone rule reads, keyed by the value they
/// bound. Every block is one where the fact holds on entry (an `If` edge) or
/// after the node (a `Guard`), and in every block it dominates.
#[derive(Default)]
struct MonotoneFacts {
    /// `v < x`: `v -> [(x, block)]`.
    below: FxHashMap<NodeId, Vec<(NodeId, usize)>>,
    /// `x - v >= k`, `k >= 1`: `v -> [(x, k, block)]`.
    room: FxHashMap<NodeId, Vec<(NodeId, i64, usize)>>,
    /// `v >= c`, `c >= 0`, after a `Guard`: `v -> [block]`.
    guard_non_negative: FxHashMap<NodeId, Vec<usize>>,
    /// JIT round 20 (lane loopopt, `CRATONVM_JIT_IR_BCE_GUARD_BELOW`): `v <
    /// x` after a `Guard` that is not an exception-edge anchor: `v -> [(x,
    /// block, guard)]`. Kept apart from `below`, whose readers take an `If`
    /// edge's fact from its block's START: a guard's fact holds only after
    /// the guard. Read by the monotone rule's step (a), where it is asked
    /// when control LEAVES a block (every node of a dominating block,
    /// reflexively, has run), and by the variable-offset arm, which asks it
    /// strictly after the guard ([`guard_fact_precedes`]).
    guard_below: FxHashMap<NodeId, Vec<(NodeId, usize, NodeId)>>,
}

/// `CRATONVM_JIT_IR_BCE_GUARD_BELOW` (JIT round 20, lane loopopt) --
/// **default ON**; `0|false|off|no` drops the guard-sourced `v < x` facts
/// ([`MonotoneFacts::guard_below`]). On, a trap-form `Guard(p < limit)` (the
/// buffer accessors' `nextPutIndex` / `nextGetIndex` throw test, and every
/// branch the never-taken pruning turned into a guard) bounds the monotone
/// rule's step and the variable-offset arm's index as an `If` edge does. Read
/// once per [`monotone_facts`].
pub(crate) fn guard_below_enabled() -> bool {
    true
}

/// JIT round 20 (lane loopopt, review): is `guard` scheduled in `block` after
/// an exception-edge anchor (or is one)? Its fact then does not hold on the
/// anchor's handler path, although its block dominates the handler
/// (`exception_edge_fences`), so a block-level guard fact must not be recorded.
/// Empty `anchors` (every graph built without the default-OFF edge switches)
/// answers `false` at once.
fn guard_after_anchor(
    schedule: &Schedule,
    anchors: &std::collections::HashSet<NodeId>,
    block: usize,
    guard: NodeId,
) -> bool {
    if anchors.is_empty() {
        return false;
    }
    if anchors.contains(&guard) {
        return true;
    }
    let Some(b) = schedule.blocks.get(block) else {
        return true;
    };
    for &n in &b.nodes {
        if n == guard {
            return false;
        }
        if anchors.contains(&n) {
            return true;
        }
    }
    // Not found in its own block: fail closed.
    true
}

/// JIT round 20 (lane loopopt): does the fact of `guard`, scheduled in block
/// `gb`, hold at `access` in block `b`? Strictly after the guard: `gb`
/// strictly dominates `b`, or both are in one block and the guard is placed
/// before the access there (a block runs its nodes in their scheduled order).
fn guard_fact_precedes(
    schedule: &Schedule,
    gb: usize,
    guard: NodeId,
    b: usize,
    access: NodeId,
) -> bool {
    if gb != b {
        return dominates(schedule, b, gb);
    }
    let Some(block) = schedule.blocks.get(b) else {
        return false;
    };
    let pos = |id: NodeId| block.nodes.iter().position(|&n| n == id);
    matches!((pos(guard), pos(access)), (Some(g), Some(a)) if g < a)
}

/// `(v, x)` when compare `cond`, evaluated to `holds`, states the signed
/// `v < x` of two `int`s. A false compare states its negation.
pub(crate) fn strictly_below(graph: &Graph, cond: NodeId, holds: bool) -> Option<(NodeId, NodeId)> {
    let c = graph.nodes.get(cond as usize)?;
    let Op::Cmp(op) = c.op else {
        return None;
    };
    let [a, b] = c.inputs.as_slice() else {
        return None;
    };
    let op = if holds { op } else { op.negate() };
    let (v, x) = match op {
        CmpOp::Lt => (*a, *b),
        CmpOp::Gt => (*b, *a),
        _ => return None,
    };
    (int_typed(graph, v) && int_typed(graph, x)).then_some((v, x))}

/// `(v, x, k)` when compare `cond`, evaluated to `holds`, states `x - v >= k`
/// with `k` in `1..=i32::MAX`, the compared value being the `int` node
/// `Sub(x, v)` (`srcRemaining < 4`, `dl - dp < 2` on their false edges).
pub(crate) fn room_at_least(
    graph: &Graph,
    cond: NodeId,
    holds: bool,
) -> Option<(NodeId, NodeId, i64)> {
    let c = graph.nodes.get(cond as usize)?;
    let Op::Cmp(op) = c.op else {
        return None;
    };
    let [a, b] = c.inputs.as_slice() else {
        return None;
    };
    let op = if holds { op } else { op.negate() };
    let (s, k) = match op {
        CmpOp::Ge => (*a, int_const(graph, *b)?),
        CmpOp::Gt => (*a, int_const(graph, *b)?.checked_add(1)?),
        CmpOp::Le => (*b, int_const(graph, *a)?),
        CmpOp::Lt => (*b, int_const(graph, *a)?.checked_add(1)?),
        CmpOp::Eq | CmpOp::Ne => return None,
    };
    if !(1..=i64::from(i32::MAX)).contains(&k) {
        return None;
    }
    let sub = graph.nodes.get(s as usize)?;
    if sub.op != Op::Sub || sub.ty != crate::ir::IrType::Int {
        return None;
    }
    let [x, v] = sub.inputs.as_slice() else {
        return None;
    };
    (int_typed(graph, *x) && int_typed(graph, *v)).then_some((*v, *x, k))}

/// Collect the [`MonotoneFacts`] of the scheduled graph.
fn monotone_facts(graph: &Graph, schedule: &Schedule) -> MonotoneFacts {
    // The block each `If` edge opens (`upper_bounds`' map): an edge landing
    // straight on a merge has none and contributes no fact.
    let mut edge_block: FxHashMap<(NodeId, u16), usize> = FxHashMap::default();
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
    let mut facts = MonotoneFacts::default();
    // JIT round 20 (lane loopopt): an exception-edge anchor's failure enters a
    // handler instead of leaving, so its guard states no fact (the rule
    // `VarOffsetFacts::collect` already applies to its room guards).
    let guard_below_on = guard_below_enabled();
    let anchors = crate::ir::exception_edge_anchors(graph);
    for (id, node) in graph.nodes.iter().enumerate() {
        match node.op {
            Op::If => {
                let Some(&cond) = node.inputs.get(1) else {
                    continue;
                };
                for edge in [0u16, 1u16] {
                    let Some(&block) = edge_block.get(&(id as NodeId, edge)) else {
                        continue;
                    };
                    if let Some((v, x)) = strictly_below(graph, cond, edge == 0) {
                        facts.below.entry(v).or_default().push((x, block));
                    }
                    if let Some((v, x, k)) = room_at_least(graph, cond, edge == 0) {
                        facts.room.entry(v).or_default().push((x, k, block));
                    }
                }
            }
            Op::Guard { .. } => {
                let Some(&cond) = node.inputs.get(1) else {
                    continue;
                };
                let Some(block) = block_of(schedule, id as NodeId) else {
                    continue;
                };
                // JIT round 20 (lane loopopt, review): a guard placed after an
                // exception-edge anchor in its block is skipped by the
                // anchor's handler path, which its block still dominates.
                if guard_after_anchor(schedule, &anchors, block, id as NodeId) {
                    continue;
                }
                if let Some(v) = non_negative_by(graph, cond, true) {
                    facts.guard_non_negative.entry(v).or_default().push(block);
                }
                if guard_below_on && !anchors.contains(&(id as NodeId)) {
                    if let Some((v, x)) = strictly_below(graph, cond, true) {
                        facts
                            .guard_below
                            .entry(v)
                            .or_default()
                            .push((x, block, id as NodeId));
                    }
                }
            }
            _ => {}
        }
    }
    facts
}

/// `(root, c)` when `v` is `root + c`: a chain of at most four `int` `Add`s,
/// each of a constant in `0..=MONOTONE_MAX_STEP`, totalling `c >= 1`.
pub(crate) fn constant_step(graph: &Graph, v: NodeId) -> Option<(NodeId, i64)> {
    let small = |k: i64| (0..=MONOTONE_MAX_STEP).contains(&k);
    let mut cur = v;
    let mut total: i64 = 0;
    for _ in 0..4 {
        let Some(n) = graph.nodes.get(cur as usize) else {
            break;
        };
        if n.op != Op::Add || n.ty != crate::ir::IrType::Int {
            break;
        }
        let [a, b] = n.inputs.as_slice() else {
            break;
        };
        match (int_const(graph, *a), int_const(graph, *b)) {
            (None, Some(k)) if small(k) => {
                total += k;
                cur = *a;
            }
            (Some(k), None) if small(k) => {
                total += k;
                cur = *b;
            }
            _ => break,
        }
    }
    (cur != v && total >= 1).then_some((cur, total))
}

/// Is `v` known `>= 0` when control leaves block `blk`? `phi`, when given, is
/// a φ already assumed `>= 0` (the induction hypothesis), so a dominating
/// `phi < v` proves `v >= 1`.
fn value_non_negative_leaving(
    graph: &Graph,
    schedule: &Schedule,
    facts: &MonotoneFacts,
    non_negative_at: &FxHashMap<NodeId, Vec<usize>>,
    v: NodeId,
    blk: usize,
    phi: Option<NodeId>,
) -> bool {
    let Some(n) = graph.nodes.get(v as usize) else {
        return false;
    };
    match n.op {
        Op::Const(c) if n.ty == crate::ir::IrType::Int => {
            return (0..=i64::from(i32::MAX)).contains(&c);
        }
        Op::ArrayLength => return true,
        _ => {}
    }
    // An `If`-edge fact holds from its block's start, a guard's after the
    // guard; control leaves `blk` after every node in it, so a fact block
    // dominating `blk` reflexively covers the edge either way.
    let held = |blocks: Option<&Vec<usize>>| {
        blocks.is_some_and(|bs| bs.iter().any(|&f| dominates_reflexive(schedule, blk, f)))
    };
    if held(non_negative_at.get(&v)) || held(facts.guard_non_negative.get(&v)) {
        return true;
    }
    phi.is_some_and(|p| {
        facts.below.get(&p).is_some_and(|fs| {
            fs.iter()
                .any(|&(x, f)| x == v && dominates_reflexive(schedule, blk, f))
        })
    })
}

/// Is back value `v`, flowing out of block `blk` into a merge, a step of the
/// header φ `phi_id` the module notes' rule accepts?
#[allow(clippy::too_many_arguments)]
fn monotone_step_safe(
    graph: &Graph,
    schedule: &Schedule,
    facts: &MonotoneFacts,
    non_negative_at: &FxHashMap<NodeId, Vec<usize>>,
    phi_id: NodeId,
    header_id: NodeId,
    v: NodeId,
    blk: usize,
    depth: u32,
) -> bool {
    if v == phi_id {
        return true;
    }
    let Some(node) = graph.nodes.get(v as usize) else {
        return false;
    };
    if node.op == Op::Phi {
        if depth >= MONOTONE_MERGE_DEPTH {
            return false;
        }
        let Some((&m, vals)) = node.inputs.split_first() else {
            return false;
        };
        if m == header_id {
            return false;
        }
        let Some(merge) = graph.nodes.get(m as usize) else {
            return false;
        };
        if !matches!(merge.op, Op::Merge | Op::Region)
            || vals.is_empty()
            || merge.inputs.len() != vals.len()
        {
            return false;
        }
        let Some(mblk) = block_of(schedule, m) else {
            return false;
        };
        for (&c, &x) in merge.inputs.iter().zip(vals) {
            let Some(cb) = block_of(schedule, c) else {
                return false;
            };
            // A FORWARD merge only: an input from a block the merge
            // dominates is another loop's back edge.
            if dominates_reflexive(schedule, cb, mblk) {
                return false;
            }
            if !monotone_step_safe(
                graph,
                schedule,
                facts,
                non_negative_at,
                phi_id,
                header_id,
                x,
                cb,
                depth + 1,
            ) {
                return false;
            }
        }
        return true;
    }
    let Some((root, c)) = constant_step(graph, v) else {
        return false;
    };
    if root != phi_id {
        return false;
    }
    // (a) `p < x`, one step. JIT round 20 (lane loopopt): or a guard's `p <
    // x` (`guard_below`) -- asked when control LEAVES `blk`, after every node
    // of a block dominating it reflexively, the guard included.
    if c == 1
        && (facts.below.get(&phi_id).is_some_and(|fs| {
            fs.iter()
                .any(|&(_, f)| dominates_reflexive(schedule, blk, f))
        }) || facts.guard_below.get(&phi_id).is_some_and(|fs| {
            fs.iter()
                .any(|&(_, f, _)| dominates_reflexive(schedule, blk, f))
        }))
    {
        return true;
    }
    // (b) `x - p >= k`, `k >= c`, `x >= 0`.
    facts.room.get(&phi_id).is_some_and(|fs| {
        fs.iter().any(|&(x, k, f)| {
            k >= c
                && dominates_reflexive(schedule, blk, f)
                && value_non_negative_leaving(
                    graph,
                    schedule,
                    facts,
                    non_negative_at,
                    x,
                    blk,
                    Some(phi_id),
                )
        })
    })
}

/// Does the header φ `phi_id` satisfy the module notes' monotone rule?
fn monotone_induction_non_negative(
    graph: &Graph,
    schedule: &Schedule,
    facts: &MonotoneFacts,
    non_negative_at: &FxHashMap<NodeId, Vec<usize>>,
    phi_id: NodeId,
    phi: &crate::ir::Node,
) -> bool {
    let Some((&region_id, values)) = phi.inputs.split_first() else {
        return false;
    };
    let Some(region) = graph.nodes.get(region_id as usize) else {
        return false;
    };
    if !matches!(region.op, Op::Merge | Op::Region)
        || values.len() < 2
        || region.inputs.len() != values.len()
    {
        return false;
    }
    let Some(header_block) = block_of(schedule, region_id) else {
        return false;
    };
    if !schedule.dom.is_reachable(header_block) {
        return false;
    }
    let (mut saw_entry, mut saw_back) = (false, false);
    for (&ctrl, &v) in region.inputs.iter().zip(values) {
        let Some(blk) = block_of(schedule, ctrl) else {
            return false;
        };
        if dominates_reflexive(schedule, blk, header_block) {
            saw_back = true;
            if !monotone_step_safe(
                graph,
                schedule,
                facts,
                non_negative_at,
                phi_id,
                region_id,
                v,
                blk,
                0,
            ) {
                return false;
            }
        } else {
            saw_entry = true;
            if !value_non_negative_leaving(graph, schedule, facts, non_negative_at, v, blk, None) {
                return false;
            }
        }
    }
    saw_entry && saw_back
}

/// Every `int` φ the monotone rule proves `>= 0` (see the module notes).
/// Only a φ some `v < x` or `x - v >= k` fact names is examined: without one
/// no step of it can be proven.
fn monotone_non_negative_phis(
    graph: &Graph,
    schedule: &Schedule,
    facts: &MonotoneFacts,
    non_negative_at: &FxHashMap<NodeId, Vec<usize>>,
) -> FxHashSet<NodeId> {
    let mut out: FxHashSet<NodeId> = FxHashSet::default();
    for (id, node) in graph.nodes.iter().enumerate() {
        let id = id as NodeId;
        if node.op != Op::Phi || node.ty != crate::ir::IrType::Int {
            continue;
        }
        if !facts.below.contains_key(&id)
            && !facts.room.contains_key(&id)
            && !facts.guard_below.contains_key(&id)
        {
            continue;
        }
        if monotone_induction_non_negative(graph, schedule, facts, non_negative_at, id, node) {
            out.insert(id);
        }
    }
    out
}

// ── Room offsets (nio21 wave 4, lane L5) ─────────────────────────────
//
// The decode loop's multi-byte arms read `sa[sp + 1]`, `sa[sp + 2]`,
// `sa[sp + 3]` behind `sl - sp < k` (false edge), and its 4-byte arm stores
// `da[dp]`, `da[dp + 1]` behind `dl - dp < 2`. The wave-9 offset rule
// (`OffsetBound`) needs the LENGTH in the compare (`i < a.length - k`); here
// the compare is `x - i >= k` with `x <= a.length` known separately (the
// monotone predicate's pre-header guards `sl <= sa.length`, `dl <=
// da.length`).
//
// # The rule
//
// An access `a[i + j]`, `0 <= j` (or `a[i]`, `j == 0`), is in range at block
// `b` when, for one room fact `x - i >= k` (the 32-bit `Sub(x, i)` compared,
// [`room_at_least`]) whose edge block dominates `b`:
//
// * `j <= k - 1`;
// * `x <= a.length` holds at `b`: `x` IS `a.length`, `a` was allocated with
//   count `x`, `x` is `Math.min(.., a.length)`, or an `If`-edge / `Guard` fact
//   [`AtMostLength`] for `a` holds at `b`;
// * `i >= 0` at `b` ([`proven_non_negative`], which includes the monotone
//   rule);
// * `x >= 0` at `b`: a non-negative constant, a length, an `If`-edge fact, a
//   `Guard` fact in a block STRICTLY dominating `b` (a guard in `b` itself may
//   follow the access), a dominating `i < x` edge (with `i >= 0`, `x >= 1`),
//   or the path-sensitive range at `b`.
//
// # Why it is sound
//
// Every fact is about SSA values and holds at `b` (each fact block dominates
// `b`, or the fact is a property of the value). With `x` and `i` both in
// `[0, i32::MAX]` the true difference `x - i` lies in `[-i32::MAX, i32::MAX]`,
// so the 32-bit `Sub` the compare read is exact and `i + k <= x`. Then `i + j
// <= i + k - 1 < x <= a.length`, the 32-bit `Add` cannot wrap (the true sum is
// below `x <= i32::MAX`) and is `>= 0` (both terms are). Without `x >= 0` it
// does not follow: `x = i32::MIN`, `i = 1` wraps `x - i` to `i32::MAX`.
//
// `CRATONVM_JIT_IR_BCE_ROOM_OFFSET=0` drops the arm; it is an edge-fact arm,
// so `CRATONVM_JIT_IR_RANGE_EDGES=0` drops it too.



/// The per-[`analyze`] facts [`room_offset_in_range`] reads.
struct RoomInputs<'a> {
    facts: &'a MonotoneFacts,
    at_most: &'a FxHashMap<LenKey, Vec<AtMostLength>>,
    alloc_by_len: &'a FxHashMap<LenKey, Vec<NodeId>>,
    non_negative_at: &'a FxHashMap<NodeId, Vec<usize>>,
    ranges: Option<&'a crate::range_analysis::PathRanges>,
    mono_phis: &'a FxHashSet<NodeId>,
    // JIT round 20 (lane loopopt): no longer read -- the room arm does not
    // take the unit-stride shape (`proven_non_negative_without_unit_stride`).
    #[allow(dead_code)]
    max_stride: i64,
}

/// Is `x <= base.length` known at block `b`? (See the room notes.)
fn at_most_length_of_at(
    graph: &Graph,
    schedule: &Schedule,
    inp: &RoomInputs<'_>,
    x: NodeId,
    base: NodeId,
    b: usize,
) -> bool {
    let key = len_key(graph, x);
    if key == LenKey::LengthOf(base) {
        return true;
    }
    if inp
        .alloc_by_len
        .get(&key)
        .is_some_and(|bs| bs.contains(&base))
    {
        return true;
    }
    let min_of_length = graph.nodes.get(x as usize).is_some_and(|n| {
        matches!(n.op, Op::ScalarIntrinsic(crate::ir::ScalarOp::MinI))
            && n.inputs
                .iter()
                .any(|&o| len_key(graph, o) == LenKey::LengthOf(base))
    });
    if min_of_length {
        return true;
    }
    inp.at_most.get(&key).is_some_and(|fs| {
        fs.iter()
            .any(|f| f.base == base && fact_holds_at(schedule, f, b))
    })
}

/// Is the room bound `x` known `>= 0` at block `b`, given `i >= 0` there?
/// (See the room notes.)
fn room_bound_non_negative_at(
    graph: &Graph,
    schedule: &Schedule,
    inp: &RoomInputs<'_>,
    x: NodeId,
    i: NodeId,
    b: usize,
) -> bool {
    let Some(n) = graph.nodes.get(x as usize) else {
        return false;
    };
    match n.op {
        Op::Const(c) if n.ty == crate::ir::IrType::Int => {
            return (0..=i64::from(i32::MAX)).contains(&c);
        }
        Op::ArrayLength => return true,
        _ => {}
    }
    if inp
        .non_negative_at
        .get(&x)
        .is_some_and(|bs| bs.iter().any(|&f| dominates_reflexive(schedule, b, f)))
    {
        return true;
    }
    // A guard holds only after itself: its block must STRICTLY dominate `b`.
    if inp
        .facts
        .guard_non_negative
        .get(&x)
        .is_some_and(|bs| bs.iter().any(|&f| dominates(schedule, b, f)))
    {
        return true;
    }
    // `i < x` with `i >= 0` (the caller's) gives `x >= 1`.
    if inp.facts.below.get(&i).is_some_and(|fs| {
        fs.iter()
            .any(|&(bx, f)| bx == x && dominates_reflexive(schedule, b, f))
    }) {
        return true;
    }
    inp.ranges
        .is_some_and(|r| r.at(x, b).definitely_non_negative())
}

/// Is `base[i + j]` (`j >= 0`) in range at block `b` by the room rule (see
/// the room notes)?
fn room_offset_in_range(
    graph: &Graph,
    schedule: &Schedule,
    inp: &RoomInputs<'_>,
    base: NodeId,
    i: NodeId,
    j: i64,
    b: usize,
) -> bool {
    if j < 0 || !int_typed(graph, i) {
        return false;
    }
    let Some(rooms) = inp.facts.room.get(&i) else {
        return false;
    };
    rooms.iter().any(|&(x, k, f)| {
        j < k
            && dominates_reflexive(schedule, b, f)
            && at_most_length_of_at(graph, schedule, inp, x, base, b)
            // JIT round 20 (lane loopopt): NOT `proven_non_negative`, whose
            // unit-stride shape arm reads `f` as an `i < x` fact with `x` at
            // most a length there. A room fact `x - i >= k` bounds nothing at
            // `f` (the 32-bit `Sub` wraps: `x = MIN`, `i = MAX` gives 1), and
            // `x <= a.length`, `x >= 0` are only known at `b`, so `i` may run
            // to `i32::MAX` and wrap negative while every back edge passes `f`.
            // The monotone rule (whose room step asks `x >= 0`) still applies.
            && proven_non_negative_without_unit_stride(
                graph,
                schedule,
                inp.ranges,
                inp.non_negative_at,
                inp.mono_phis,
                i,
                b,
            )
            && room_bound_non_negative_at(graph, schedule, inp, x, i, b)
    })
}

// ── Variable offsets (nio21 wave 6, lane L5) ─────────────────────────
//
// `HeapByteBuffer.put(byte)` stores `hb[p + offset]` behind `p < limit`, and
// the access is in range only by the buffer's invariant `offset + limit <=
// hb.length`. `ir_optimize`'s offset predicate plants it, in the loop's
// pre-header, as `Guard(limit <= hb.length - offset)` plus `Guard(0 <=
// offset)` and `Guard(0 <= p_init)`.
//
// # The rule
//
// An access `a[i + o]` (either operand order) is in range at block `b` when,
// for some `x`:
//
// * `x <= a.length - o` holds at `b`: an `If` edge (from its block's start)
//   or a `Guard` (strictly after its block) stating `x <= Sub(L, o)` or `x <
//   Sub(L, o)`, `L` a length of `a` itself and `o` the SAME SSA value;
// * an `If` edge `i < x` dominates `b` (reflexively);
// * `i >= 0` at `b` ([`proven_non_negative`], with the monotone rule, whose
//   entry half reads the `0 <= init` guard);
// * `o >= 0` at `b`: a non-negative constant, a length, an `If`-edge fact, a
//   `Guard` fact strictly dominating `b`, or the path-sensitive range at `b`.
//
// # Why it is sound
//
// Every fact is about SSA values and holds at `b`. With `o >= 0` and `L >= 0`
// the 32-bit `L - o` is exact, so `x + o <= L`. Then `i + o < x + o <= L <=
// i32::MAX`: the 32-bit sum does not wrap, and with both terms `>= 0` it is
// `>= 0`. Without `o >= 0` it does not follow (`L - o` may wrap).
//
// `CRATONVM_JIT_IR_BCE_VAR_OFFSET=0` drops the arm; it is an edge-fact arm,
// so `CRATONVM_JIT_IR_RANGE_EDGES=0` drops it too.

/// `CRATONVM_JIT_IR_BCE_VAR_OFFSET` (nio21 wave 6, lane L5) -- **default
/// ON**; `0|false|off|no` drops the variable-offset arm ([`VarOffsetFacts`]).
/// Read once per [`analyze`].
fn var_offset_enabled() -> bool {
    true
}

/// `(x, base, o)` when compare `cond`, evaluated to `holds`, states `x <=
/// base.length - o` (or the stricter `x < base.length - o`): the compared
/// value is the `int` node `Sub(L, o)` with `L` an `ArrayLength` of `base`. A
/// false compare states its NEGATION.
fn at_most_length_minus(
    graph: &Graph,
    cond: NodeId,
    holds: bool,
) -> Option<(NodeId, NodeId, NodeId)> {
    let c = graph.nodes.get(cond as usize)?;
    let Op::Cmp(op) = c.op else {
        return None;
    };
    let [a, b] = c.inputs.as_slice() else {
        return None;
    };
    let op = if holds { op } else { op.negate() };
    let (x, s) = match op {
        CmpOp::Le | CmpOp::Lt => (*a, *b),
        CmpOp::Ge | CmpOp::Gt => (*b, *a),
        CmpOp::Eq | CmpOp::Ne => return None,
    };
    let sub = graph.nodes.get(s as usize)?;
    if sub.op != Op::Sub || sub.ty != crate::ir::IrType::Int {
        return None;
    }
    let [l, o] = sub.inputs.as_slice() else {
        return None;
    };
    let len = graph.nodes.get(*l as usize)?;
    if len.op != Op::ArrayLength {
        return None;
    }
    let base = *len.inputs.get(2)?;
    (int_typed(graph, x) && int_typed(graph, *o)).then_some((x, base, *o))
}

/// The facts the variable-offset arm reads, collected once per [`analyze`]
/// (see the section notes).
struct VarOffsetFacts {
    /// `x <= base.length - o`: `(base, o) -> [(x, block, after_block_start)]`.
    rooms: FxHashMap<(NodeId, NodeId), Vec<(NodeId, usize, bool)>>,
    /// The `i < x` edges and `v >= 0` guards, as the monotone rule reads them.
    mono: MonotoneFacts,
    /// The φs the monotone rule proves `>= 0` from `mono`.
    mono_phis: FxHashSet<NodeId>,
}

impl VarOffsetFacts {
    /// `None` when the graph states no `x <= base.length - o` fact (the arm
    /// then costs one scan of the nodes).
    fn collect(
        graph: &Graph,
        schedule: &Schedule,
        non_negative_at: &FxHashMap<NodeId, Vec<usize>>,
    ) -> Option<Self> {
        let mut edge_block: FxHashMap<(NodeId, u16), usize> = FxHashMap::default();
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
        let mut rooms: FxHashMap<(NodeId, NodeId), Vec<(NodeId, usize, bool)>> =
            FxHashMap::default();
        let anchors = crate::ir::exception_edge_anchors(graph);
        for (id, node) in graph.nodes.iter().enumerate() {
            let Some(&cond) = node.inputs.get(1) else {
                continue;
            };
            match node.op {
                Op::If => {
                    for edge in [0u16, 1u16] {
                        let Some(&block) = edge_block.get(&(id as NodeId, edge)) else {
                            continue;
                        };
                        if let Some((x, base, o)) = at_most_length_minus(graph, cond, edge == 0) {
                            rooms.entry((base, o)).or_default().push((x, block, false));
                        }
                    }
                }
                Op::Guard { .. } => {
                    // An exception-edge anchor's failure enters a handler
                    // instead of leaving (nio21 wave 6 L2): no fact of its.
                    if anchors.contains(&(id as NodeId)) {
                        continue;
                    }
                    let Some(block) = block_of(schedule, id as NodeId) else {
                        continue;
                    };
                    // JIT round 20 (lane loopopt, review): nor past one.
                    if guard_after_anchor(schedule, &anchors, block, id as NodeId) {
                        continue;
                    }
                    if let Some((x, base, o)) = at_most_length_minus(graph, cond, true) {
                        rooms.entry((base, o)).or_default().push((x, block, true));
                    }
                }
                _ => {}
            }
        }
        if rooms.is_empty() {
            return None;
        }
        let mono = monotone_facts(graph, schedule);
        let mono_phis = if monotone_enabled() {
            monotone_non_negative_phis(graph, schedule, &mono, non_negative_at)
        } else {
            FxHashSet::default()
        };
        Some(VarOffsetFacts {
            rooms,
            mono,
            mono_phis,
        })
    }

    /// Is the offset `o` known `>= 0` at block `b`? (See the section notes.)
    fn offset_non_negative_at(
        &self,
        graph: &Graph,
        schedule: &Schedule,
        ranges: Option<&crate::range_analysis::PathRanges>,
        non_negative_at: &FxHashMap<NodeId, Vec<usize>>,
        o: NodeId,
        b: usize,
    ) -> bool {
        let Some(n) = graph.nodes.get(o as usize) else {
            return false;
        };
        match n.op {
            Op::Const(c) if n.ty == crate::ir::IrType::Int => {
                return (0..=i64::from(i32::MAX)).contains(&c);
            }
            Op::ArrayLength => return true,
            _ => {}
        }
        if non_negative_at
            .get(&o)
            .is_some_and(|bs| bs.iter().any(|&f| dominates_reflexive(schedule, b, f)))
        {
            return true;
        }
        // A guard holds only after itself: its block must STRICTLY dominate `b`.
        if self
            .mono
            .guard_non_negative
            .get(&o)
            .is_some_and(|bs| bs.iter().any(|&f| dominates(schedule, b, f)))
        {
            return true;
        }
        ranges.is_some_and(|r| r.at(o, b).definitely_non_negative())
    }

    /// Is `base[idx]`, `idx` the `int` sum `i + o`, in range at block `b` by
    /// the variable-offset rule (see the section notes)?
    #[allow(clippy::too_many_arguments)]
    fn proves(
        &self,
        graph: &Graph,
        schedule: &Schedule,
        ranges: Option<&crate::range_analysis::PathRanges>,
        non_negative_at: &FxHashMap<NodeId, Vec<usize>>,
        // JIT round 20 (lane loopopt): unused -- the arm proves stride 1 only
        // (see below); kept so the call site reads like its neighbours.
        _max_stride: i64,
        base: NodeId,
        idx: NodeId,
        b: usize,
        access: NodeId,
    ) -> bool {
        let Some(n) = graph.nodes.get(idx as usize) else {
            return false;
        };
        if n.op != Op::Add || n.ty != crate::ir::IrType::Int {
            return false;
        }
        let [u, v] = n.inputs.as_slice() else {
            return false;
        };
        for (i, o) in [(*u, *v), (*v, *u)] {
            if !int_typed(graph, i) {
                continue;
            }
            let Some(rooms) = self.rooms.get(&(base, o)) else {
                continue;
            };
            // `i < x` from an `If` edge (from its block's start) or, JIT
            // round 20 (lane loopopt), from a guard strictly before the access
            // (`guard_fact_precedes`): `(x, the fact's block)`. The block is
            // also the one `proven_non_negative`'s unit-stride shape asks every
            // back edge to pass, which a guard's block answers as an edge's
            // does (control leaves a back-edge block after the guard).
            let mut below: Vec<(NodeId, usize)> = self
                .mono
                .below
                .get(&i)
                .map(|fs| {
                    fs.iter()
                        .copied()
                        .filter(|&(_, f)| dominates_reflexive(schedule, b, f))
                        .collect()
                })
                .unwrap_or_default();
            if let Some(gs) = self.mono.guard_below.get(&i) {
                below.extend(
                    gs.iter()
                        .filter(|&&(_, gb, g)| guard_fact_precedes(schedule, gb, g, b, access))
                        .map(|&(x, gb, _)| (x, gb)),
                );
            }
            if below.is_empty() {
                continue;
            }
            for &(x, rb, after) in rooms {
                let room_holds = if after {
                    dominates(schedule, b, rb)
                } else {
                    dominates_reflexive(schedule, b, rb)
                };
                if !room_holds {
                    continue;
                }
                let key = len_key(graph, x);
                for &(bx, f) in &below {
                    // JIT round 20 (lane loopopt): stride 1 only. `x <= a.length -
                    // o` is known at `b`, not at the `i < x` fact block `f`, so
                    // on an iteration that never reaches `b` `x` may be
                    // `i32::MAX` and a stride-3 step wraps; `i < x` alone keeps
                    // a stride-1 step from wrapping whatever `x` is.
                    if len_key(graph, bx) == key
                        && proven_non_negative(
                            graph,
                            schedule,
                            ranges,
                            non_negative_at,
                            &self.mono_phis,
                            i,
                            f,
                            b,
                            1,
                        )
                        && self.offset_non_negative_at(
                            graph,
                            schedule,
                            ranges,
                            non_negative_at,
                            o,
                            b,
                        )
                    {
                        return true;
                    }
                }
            }
        }
        false
    }
}

/// A key under which two `int` values are EQUAL wherever both are defined:
/// the same SSA node, the length of the same array (a Java array's length
/// never changes), or the same `int` constant. See [`upper_bounds`].
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum LenKey {
    Node(NodeId),
    LengthOf(NodeId),
    Const(i64),
}

fn len_key(graph: &Graph, id: NodeId) -> LenKey {
    let Some(n) = graph.nodes.get(id as usize) else {
        return LenKey::Node(id);
    };
    match n.op {
        Op::ArrayLength => match n.inputs.get(2) {
            Some(&base) => LenKey::LengthOf(base),
            None => LenKey::Node(id),
        },
        // Cast: an `int` constant is its low 32 bits (see [`int_const`]), so
        // two spellings of one `int` share a key.
        Op::Const(c) if n.ty == crate::ir::IrType::Int => LenKey::Const(i64::from(c as i32)),
        _ => LenKey::Node(id),
    }
}

/// A `x <= base.length` fact that holds in `block` and every block it
/// dominates -- from the block's start for an `If` edge, only after the node
/// for a `Guard` (`after_block_start`).
struct AtMostLength {
    base: NodeId,
    block: usize,
    after_block_start: bool,
}

/// `(x, base)` when compare `cond` states `x <= base.length`, given that it
/// evaluated to `holds`. `Lt` implies `Le`, and a false compare states its
/// NEGATION (`CmpOp::negate`, not the mirror image).
fn at_most_length(graph: &Graph, cond: NodeId, holds: bool) -> Option<(NodeId, NodeId)> {
    let c = graph.nodes.get(cond as usize)?;
    let Op::Cmp(op) = c.op else {
        return None;
    };
    let [a, b] = c.inputs.as_slice() else {
        return None;
    };
    let op = if holds { op } else { op.negate() };
    let (x, len) = match op {
        CmpOp::Le | CmpOp::Lt => (*a, *b),
        CmpOp::Ge | CmpOp::Gt => (*b, *a),
        CmpOp::Eq | CmpOp::Ne => return None,
    };
    let len_node = graph.nodes.get(len as usize)?;
    if len_node.op != Op::ArrayLength {
        return None;
    }
    let base = *len_node.inputs.get(2)?;
    Some((x, base))
}

/// Is `v` provably `>= 0` at every point the guard at `guard_block` covers?
///
/// `guard_block` is the true edge of the branch that established the upper
/// bound: an induction variable's non-negativity is proven THROUGH that same
/// branch (see the module notes above), so the two halves cannot be decided
/// independently.
///
/// `ranges` is the SSA value-range lattice when it is switched on, consulted
/// as a fourth arm: it reaches shapes the three hand-written ones have no case
/// for, and they reach the guard-dominated induction variable it cannot see.
///
/// `non_negative_at` is the fifth arm (round 11 wave 9, mid proposal W8-1):
/// an `If` edge proving `v >= c`, `c >= 0` (`non_negative_by`), whose block
/// dominates the access. It holds there for the reason every edge fact in
/// this module does: the edge block is entered only through that edge, `v` is
/// one SSA value, and a use dominated by the edge block sees the value the
/// edge tested (the test reads `v`, so `v`'s definition dominates it, and no
/// path from that definition to the access avoids the edge). It is what the
/// down count's `i >= 0` loop test gives its accesses. Reading it by shape
/// means the elision no longer needs the SSA range lattice
/// (`CRATONVM_JIT_IR_BCE_RANGE=0` keeps it). It is still an edge fact, so
/// `CRATONVM_JIT_IR_RANGE_EDGES=0` withdraws it with every other one.
///
/// `max_stride` is [`induction_max_stride`], read once per [`analyze`].
#[allow(clippy::too_many_arguments)]
fn proven_non_negative(
    graph: &Graph,
    schedule: &Schedule,
    ranges: Option<&crate::range_analysis::PathRanges>,
    non_negative_at: &FxHashMap<NodeId, Vec<usize>>,
    mono_phis: &FxHashSet<NodeId>,
    v: NodeId,
    guard_block: usize,
    access_block: usize,
    max_stride: i64,
) -> bool {
    let Some(node) = graph.nodes.get(v as usize) else {
        return false;
    };
    let by_shape = match &node.op {
        // An `int` index constant: a value past `i32::MAX` would truncate to a
        // negative 32-bit index at run time, which is how the lattice reads it
        // too (round 11). No folder produces one; the bound keeps it that way.
        Op::Const(c) => (0..=i64::from(i32::MAX)).contains(c),
        // A JVM array length is non-negative by construction: `newarray` with a
        // negative count throws before the length can ever be read.
        Op::ArrayLength => true,
        // nio21 wave 3 (lane L5): or a monotone induction, proven `>= 0`
        // everywhere it is defined (`monotone_non_negative_phis`).
        Op::Phi => {
            is_unit_stride_induction(graph, schedule, v, node, guard_block, max_stride)
                || mono_phis.contains(&v)        }
        _ => false,
    };
    if by_shape {
        return true;
    }
    if non_negative_at.get(&v).is_some_and(|blocks| {
        blocks
            .iter()
            .any(|&f| dominates_reflexive(schedule, access_block, f))
    }) {
        return true;
    }
    // `definitely_non_negative` is now the ONLY non-negativity predicate
    // `Range` offers, and this call site is why. The vacuous twin it used to
    // have answered `true` at bottom, where "every value is >= 0" holds because
    // there are no values; reading that as permission to drop a bounds check
    // makes the elision rest on the analysis being right that the value cannot
    // occur, which is not a claim a memory-safety proof may depend on.
    // Asked at the ACCESS block, not at `guard_block`. The access is dominated
    // by the guard, so it sees a superset of the guard's facts -- every `If`
    // edge on the way from the guard down to the access narrows too. Asking at
    // `guard_block` would be sound and would simply prove less; `guard_block`
    // stays because the SHAPE arm above reasons about the guard, not about the
    // access.
    ranges.is_some_and(|r| r.at(v, access_block).definitely_non_negative())
}

/// JIT round 20 (lane loopopt): [`proven_non_negative`] without the
/// unit-stride shape arm, for a caller whose fact block is not an `i < x`
/// fact with `x` bounded by a length there (the room-offset arm). Constants,
/// lengths, the monotone φs, dominating `v >= c` edges and the range lattice.
#[allow(clippy::too_many_arguments)]
fn proven_non_negative_without_unit_stride(
    graph: &Graph,
    schedule: &Schedule,
    ranges: Option<&crate::range_analysis::PathRanges>,
    non_negative_at: &FxHashMap<NodeId, Vec<usize>>,
    mono_phis: &FxHashSet<NodeId>,
    v: NodeId,
    access_block: usize,
) -> bool {
    let Some(node) = graph.nodes.get(v as usize) else {
        return false;
    };
    let by_shape = match &node.op {
        Op::Const(c) => (0..=i64::from(i32::MAX)).contains(c),
        Op::ArrayLength => true,
        Op::Phi => mono_phis.contains(&v),
        _ => false,
    };
    if by_shape {
        return true;
    }
    if non_negative_at.get(&v).is_some_and(|blocks| {
        blocks
            .iter()
            .any(|&f| dominates_reflexive(schedule, access_block, f))
    }) {
        return true;
    }
    ranges.is_some_and(|r| r.at(v, access_block).definitely_non_negative())
}

/// The blocks of `schedule`, ordered so that every block comes after each of
/// its dominators.
///
/// Sorted by dominator-tree DEPTH — the number of other blocks that dominate a
/// block — with the post-layout index breaking ties. If `d` dominates `b` then
/// everything dominating `d` also dominates `b` and `d` does too, so
/// `depth(d) < depth(b)` strictly, and `d` is emitted first. Any order with
/// that property is enough for the accumulate-then-query walk in [`analyze`].
///
/// A full dominator-tree preorder would do as well and is cheaper. This comment
/// used to say that needs "an `idom` the `Dominators` type does not expose";
/// that stopped being true when `Dominators::idom` was made public, and
/// `range_analysis::dom_preorder` builds exactly such a walk from it. Left as a
/// sort here rather than switched in the same change as B3, because the two
/// orders are not identical -- this one is stable under ties by block index and
/// the walk's output feeds a census -- and a reordering wants its own before/
/// after rather than riding along with an analysis change.
///
/// The depth is read off the `idom` chain with memoisation, O(B) (round 11
/// wave 2; it counted dominators by asking `dominates` of every pair, O(B²)).
/// The count it reproduces is `Dominators::dominates`' own: for a reachable
/// block, its strict ancestors in the idom tree; for an unreachable block,
/// every other block (the relation's documented top), so those sort last as
/// before.
fn dominator_order(schedule: &Schedule) -> Vec<usize> {
    let n = schedule.blocks.len();
    let mut order: Vec<usize> = (0..n).collect();
    const UNKNOWN: usize = usize::MAX;
    let mut depth: Vec<usize> = vec![UNKNOWN; n];
    let mut chain: Vec<usize> = Vec::new();
    for b in 0..n {
        if !schedule.dom.is_reachable(b) {
            depth[b] = n.saturating_sub(1);
            continue;
        }
        chain.clear();
        let mut cur = b;
        let base = loop {
            if depth[cur] != UNKNOWN {
                break depth[cur];
            }
            match schedule.dom.idom(cur) {
                // `chain.len() <= n` bounds a malformed cyclic idom array.
                Some(p) if p != cur && p < n && chain.len() <= n => {
                    chain.push(cur);
                    cur = p;
                }
                _ => {
                    depth[cur] = 0;
                    break 0;
                }
            }
        };
        let mut d = base;
        for &c in chain.iter().rev() {
            d += 1;
            depth[c] = d;
        }
    }
    order.sort_by_key(|&b| (depth[b], b));
    order
}

/// `[region, const_init, phi + s]`, with the increment guarded by
/// `guard_block` and `1 <= s <= max_stride` (1 unless
/// [`induction_max_stride`] widens it; see [`MAX_PROVEN_STRIDE`]).
fn is_unit_stride_induction(
    graph: &Graph,
    schedule: &Schedule,
    phi_id: NodeId,
    phi: &crate::ir::Node,
    guard_block: usize,
    max_stride: i64,
) -> bool {
    is_unit_stride_induction_capped(graph, schedule, phi_id, phi, guard_block, max_stride, MAX_PROVEN_STRIDE)
}

/// JIT r23 w2c close (`CRATONVM_JIT_IR_BCE_MARGIN_STRIDE`): [`is_unit_stride_induction`]
/// under a guard `phi < a.length - margin`, where the step may reach
/// `MAX_PROVEN_STRIDE + margin` -- a main/post unroll's main loop (`i' < n -
/// d`, step `factor * s`). The no-wrap argument of the module notes, with the
/// margin: at the guard `phi < a.length - margin <= VM_MAX_ARRAY_LENGTH -
/// margin`, so `phi + step <= VM_MAX_ARRAY_LENGTH - margin - 1 +
/// MAX_PROVEN_STRIDE + margin = i32::MAX`.
fn is_margin_stride_induction(
    graph: &Graph,
    schedule: &Schedule,
    phi_id: NodeId,
    guard_block: usize,
    margin: i64,
) -> bool {
    if margin <= 0 || !margin_stride_enabled() {
        return false;
    }
    let Some(phi) = graph.nodes.get(phi_id as usize).filter(|n| n.op == Op::Phi) else {
        return false;
    };
    let cap = MAX_PROVEN_STRIDE.saturating_add(margin).min(i64::from(i32::MAX));
    is_unit_stride_induction_capped(graph, schedule, phi_id, phi, guard_block, cap, cap)
}

/// `CRATONVM_JIT_IR_BCE_MARGIN_STRIDE` (JIT r23 w2c close; perf, **default
/// ON**, `=0` kill): [`is_margin_stride_induction`] in the offset rule. Read
/// per [`analyze`] call site.
fn margin_stride_enabled() -> bool {
    true
}

/// [`is_unit_stride_induction`] with the step's cap explicit.
fn is_unit_stride_induction_capped(
    graph: &Graph,
    schedule: &Schedule,
    phi_id: NodeId,
    phi: &crate::ir::Node,
    guard_block: usize,
    max_stride: i64,
    cap: i64,
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
        Some(Op::Const(c)) if (0..=i64::from(i32::MAX)).contains(c)
    );
    if !init_ok {
        return false;
    }
    // The back-edge value must be exactly `phi + s`, `1 <= s <= max_stride`.
    // See the module notes ("Why the stride must be exactly 1", and the round
    // 12 wave 3 addendum) for why the step is bounded by the VM's array-length
    // cap and by nothing else.
    let Some(back) = graph.nodes.get(back_id as usize) else {
        return false;
    };
    if !matches!(back.op, Op::Add) {
        return false;
    }
    let max_stride = max_stride.clamp(1, cap);
    let is_one = |n: NodeId| {
        matches!(
            graph.nodes.get(n as usize).map(|x| &x.op),
            Some(Op::Const(s)) if (1..=max_stride).contains(s)
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

// ── Negative constant offsets (JIT round 21 wave 4, lane bce, BCE21-2) ──
//
// `c[i] = a[i - 1] + a[i] + a[i + 1]`: the `a[i + 1]` half is the offset
// predicate's (`ir_optimize::offset_predicate`, a constant `o` under this
// switch, read by `VarOffsetFacts`); this is the `a[i - c]` half.
//
// # The rule
//
// `a[i - c]` (`Sub(i, c)` or `Add(i, -c)`, `c` in `1..=i32::MAX`) is in range
// at block `b` when an `i < a.length` fact for the SAME array (any
// `UpperBound` on `i`: the length itself, `new T[x]`, a dominating `x <=
// a.length` -- the range predicate's guard, which `range_predicate_plan`
// plants for such an access under this switch) holds at a block `f`
// dominating `b`, and `i` is an increasing induction
// (`is_unit_stride_induction` with `f`) whose constant entry value is `>= c`.
//
// # Why it is sound
//
// `is_unit_stride_induction` with `f` proves that every increment runs from a
// value `< a.length <= VM_MAX_ARRAY_LENGTH` by at most `MAX_PROVEN_STRIDE`, so
// it never wraps: `i` never decreases and is at least its entry value `k >= c`
// wherever it is defined. At `b`: `c <= i < a.length`, so the 32-bit `i - c` is
// exact and lies in `[0, a.length - 1]`.
//
// `CRATONVM_JIT_IR_CONST_OFFSET_PREDICATE` (opt-in) gates the arm and both
// planters.

/// `CRATONVM_JIT_IR_CONST_OFFSET_PREDICATE` -- **default OFF** (JIT round 21
/// wave 4, lane bce, BCE21-2): the `a[i - c]` arm of [`analyze`], the range
/// predicate's guard for such an access (`ir_optimize::range_predicate_plan`)
/// and the offset predicate's constant-offset room guard
/// (`ir_optimize::offset_predicate`). Read once per [`analyze`] / per plan; no
/// `static`.
pub(crate) fn const_offset_predicate_enabled() -> bool {
    false}

/// BCE21-2: `(i, c)` when `idx` is the `int` `i - c` (`Sub(i, Const(c))`) or
/// `i + (-c)` (`Add` of `Const(-c)`, either operand order), with `c` in
/// `1..=i32::MAX` and `i` not itself a constant.
pub(crate) fn negative_offset_index(graph: &Graph, idx: NodeId) -> Option<(NodeId, i64)> {
    let n = graph.nodes.get(idx as usize)?;
    if n.ty != crate::ir::IrType::Int {
        return None;
    }
    let positive = |c: i64| (1..=i64::from(i32::MAX)).contains(&c);
    match (&n.op, n.inputs.as_slice()) {
        (Op::Sub, [l, r]) => {
            let c = int_const(graph, *r)?;
            (positive(c) && int_const(graph, *l).is_none()).then_some((*l, c))
        }
        (Op::Add, [l, r]) => match (int_const(graph, *l), int_const(graph, *r)) {
            (None, Some(k)) if k.checked_neg().is_some_and(positive) => Some((*l, -k)),
            (Some(k), None) if k.checked_neg().is_some_and(positive) => Some((*r, -k)),
            _ => None,
        },
        _ => None,
    }
}

/// BCE21-2: the constant entry value of the φ `phi_id` when
/// [`is_unit_stride_induction`] accepts it with `guard_block` -- every value
/// the φ takes is then at least that entry value (see the section notes).
fn unit_stride_induction_floor(
    graph: &Graph,
    schedule: &Schedule,
    phi_id: NodeId,
    guard_block: usize,
    max_stride: i64,
) -> Option<i64> {
    let phi = graph.nodes.get(phi_id as usize)?;
    if phi.op != Op::Phi
        || !is_unit_stride_induction(graph, schedule, phi_id, phi, guard_block, max_stride)
    {
        return None;
    }
    // `is_unit_stride_induction` read the φ as `[region, init, next]` and
    // required `init` to be a constant in `0..=i32::MAX`.
    let init = *phi.inputs.get(1)?;
    match graph.nodes.get(init as usize)?.op {
        Op::Const(c) => Some(c),
        _ => None,
    }
}

// ── `a[p - c]` under a monotone index (j27 wave 2, lane TIER1PROP: BCE21-2's
// non-constant-start half) ──
//
// `for (i = from; i < to; i++) .. a[i - 1] ..`, and the decode-style loops
// whose index advances by `1..k` in several arms: the BCE21-2 arm above proves
// `a[i - c]` only for a unit-stride `i` from a CONSTANT start `>= c`. The
// book's other half ("otherwise needs `Guard(c <= init)`") is this arm. It
// reads the monotone predicate's pre-header guards
// (`ir_optimize::monotone_predicate`, which plants `x <= a.length` and, under
// the same switch, `c <= init`; module `neg_offset` there).
//
// # The rule
//
// `a[p - c]`, `c` in `1..=i32::MAX` ([`negative_offset_index`]), is in range at
// block `b` when
//
// * an [`UpperBound`] `p < a.length` for the SAME array holds at `b` (the
//   `If`-edge and at-most facts every other arm reads), and
// * `p` is a header φ the monotone rule accepts
//   ([`monotone_induction_non_negative`]) and EVERY entry value of `p` is at
//   least `c` when control leaves its entry block: an `int` constant `>= c`,
//   or a `Guard(v >= lo)`, `lo >= c`, in a block dominating it reflexively
//   and not past an exception-edge anchor ([`MonotoneFloors`]).
//
// # Why it is sound
//
// The monotone rule proves every back value of `p` is `p` plus a constant
// `>= 0` that cannot wrap (`monotone_step_safe`), so `p` never decreases and
// every value it takes is at least its least entry value, which is `>= c`. At
// `b`: `1 <= c <= p < a.length <= i32::MAX`, so the 32-bit `p - c` is exact
// and lies in `[0, a.length - 1 - c]`. A failing floor guard deopts to the
// loop entry with the entry frame, where the interpreter runs the loop and
// raises the AIOOBE (with its own text) where the bytecode raises it.

/// `CRATONVM_JIT_IR_MONOTONE_NEG_OFFSET` -- **default OFF** (j27 wave 2, lane
/// TIER1PROP; opt-in because it was written without a build): the `a[p - c]`
/// arm of [`analyze`] above and its planter half in
/// `ir_optimize::monotone_predicate`. Read once per [`analyze`] / per plan; no
/// `static`.
pub(crate) fn monotone_neg_offset_enabled() -> bool {
    false}

/// `v -> [(lo, block)]`: the `int` `v >= lo` holds after a `Guard` in `block`
/// that is not past an exception-edge anchor there.
type MonotoneFloors = FxHashMap<NodeId, Vec<(i64, usize)>>;

/// Collect the [`MonotoneFloors`] of the scheduled graph.
fn guard_floor_facts(graph: &Graph, schedule: &Schedule) -> MonotoneFloors {
    let anchors = crate::ir::exception_edge_anchors(graph);
    let mut out = MonotoneFloors::default();
    for (id, node) in graph.nodes.iter().enumerate() {
        if !matches!(node.op, Op::Guard { .. }) {
            continue;
        }
        let (Some(&cond), Some(block)) = (node.inputs.get(1), block_of(schedule, id as NodeId))
        else {
            continue;
        };
        if guard_after_anchor(schedule, &anchors, block, id as NodeId) {
            continue;
        }
        if let Some((v, lo)) = lower_bound_by(graph, cond, true) {
            if int_typed(graph, v) {
                out.entry(v).or_default().push((lo, block));
            }
        }
    }
    out
}

/// Is `v` known `>= c` when control leaves block `blk`?
fn value_at_least_leaving(
    graph: &Graph,
    schedule: &Schedule,
    floors: &MonotoneFloors,
    v: NodeId,
    blk: usize,
    c: i64,
) -> bool {
    if int_const(graph, v).is_some_and(|k| k >= c) {
        return true;
    }
    floors.get(&v).is_some_and(|fs| {
        fs.iter()
            .any(|&(lo, f)| lo >= c && dominates_reflexive(schedule, blk, f))
    })
}

/// Is the header φ `phi_id` at least `c` wherever it is defined (the section
/// notes' rule)?
fn monotone_phi_at_least(
    graph: &Graph,
    schedule: &Schedule,
    facts: &MonotoneFacts,
    non_negative_at: &FxHashMap<NodeId, Vec<usize>>,
    floors: &MonotoneFloors,
    phi_id: NodeId,
    c: i64,
) -> bool {
    let Some(phi) = graph.nodes.get(phi_id as usize) else {
        return false;
    };
    if phi.op != Op::Phi
        || !int_typed(graph, phi_id)
        || !monotone_induction_non_negative(graph, schedule, facts, non_negative_at, phi_id, phi)
    {
        return false;
    }
    // `monotone_induction_non_negative` checked the shape: `[region, v..]`,
    // one value per region input, at least one entry and one back edge.
    let Some((&region_id, values)) = phi.inputs.split_first() else {
        return false;
    };
    let (Some(region), Some(header_block)) = (
        graph.nodes.get(region_id as usize),
        block_of(schedule, region_id),
    ) else {
        return false;
    };
    region.inputs.iter().zip(values).all(|(&ctrl, &v)| {
        block_of(schedule, ctrl).is_some_and(|blk| {
            // A back edge (the header dominates its block) steps up from the
            // φ; only an entry value needs the floor.
            dominates_reflexive(schedule, blk, header_block)
                || value_at_least_leaving(graph, schedule, floors, v, blk, c)
        })
    })
}

// ── `long` induction variables (JIT round 22 wave 2, lane loops, LOOPS22-4 /
// BCE21-4) ──
//
// `for (long i = 0; i < a.length; i++) a[(int) i]`: javac's test is
// `lload i; aload a; arraylength; i2l; lcmp; ifge exit`, so the IR states
// `Cmp(Ge, LCmp(i, I2L(a.length)), 0)` and the index is `L2I(i)`. Neither the
// `int` fact collector nor the unit-stride rule saw it, so the check stayed.
//
// # The rule
//
// `a[L2I(i)]` is in range at block `b` when an `If` edge stating
// `i < (long) a.length` for the SAME array (read through the `lcmp`: `LCmp(x,
// y) cc 0` is exactly `x cc y`, and `0 cc LCmp(x, y)` is `y cc x`, for every
// signed `cc`) opens a block `f` dominating `b`, and `i` is a `long` phi
// [`is_unit_stride_induction`] accepts with `f`: a constant entry value in
// `0..=i32::MAX`, a constant step `1..=MAX_PROVEN_STRIDE`, every back edge
// through `f`.
//
// # Why it is sound
//
// Every increment starts from `i < a.length <= VM_MAX_ARRAY_LENGTH` and a
// `long` cannot wrap there, so `i` never decreases and is at least its entry
// value `>= 0` wherever it is defined. At `b`: `0 <= i < a.length <=
// i32::MAX`, so `L2I(i)` is `i` exactly and lies in `[0, a.length - 1]`.

/// `CRATONVM_JIT_IR_BCE_LONG_INDUCTION` -- **default ON** since 2026-10-26 (t1, `=0` kills; JIT round 22 wave
/// 2, lane loops, LOOPS22-4 / BCE21-4; perf): the `a[(int) i]` arm of
/// [`analyze`] for a `long` induction variable. Read once per [`analyze`]; no
/// `static`.
pub(crate) fn long_induction_bce_enabled() -> bool {
    // WARNING t1-user-flip-20261026: default ON, `=0` kills.
    true
}

/// `CRATONVM_JIT_IR_BCE_GUARD_UPPER` -- **default OFF** (JIT round 22 wave 2,
/// lane loops, nio21 L5W3-1; perf, opt-in): a `Guard(idx < a.length)` bounds
/// `idx` for the accesses after it (later in its block, or in a block it
/// strictly dominates), with `idx >= 0` proven as for an `If` edge
/// ([`proven_non_negative`], the guard's block as the fact block). A guard's
/// fact holds only AFTER the guard, which is why it is not an `UpperBound`
/// (whose block-start fact covers its whole block). Read once per
/// [`analyze`]; no `static`.
pub(crate) fn guard_upper_bounds_enabled() -> bool {
    true
}

/// LOOPS22-4: the `long` value `i` when `idx` is the `int` `L2I(i)`.
fn l2i_operand(graph: &Graph, idx: NodeId) -> Option<NodeId> {
    let n = graph.nodes.get(idx as usize)?;
    if n.op != Op::L2I || n.ty != crate::ir::IrType::Int {
        return None;
    }
    let i = *n.inputs.first()?;
    (graph.nodes.get(i as usize)?.ty == crate::ir::IrType::Long).then_some(i)
}

/// LOOPS22-4: every `i < (long) base.length` fact an `If` edge states through
/// an `lcmp` zero test, keyed by `i` (see the section notes; the edge table is
/// [`upper_bounds`]'s).
fn long_upper_bounds(graph: &Graph, schedule: &Schedule) -> FxHashMap<NodeId, Vec<UpperBound>> {
    let mut edge_block: FxHashMap<(NodeId, u16), usize> = FxHashMap::default();
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
    let lcmp = |v: NodeId| -> Option<(NodeId, NodeId)> {
        let n = graph.nodes.get(v as usize)?;
        (n.op == Op::LCmp && n.inputs.len() == 2).then(|| (n.inputs[0], n.inputs[1]))
    };
    let array_of_long_length = |v: NodeId| -> Option<NodeId> {
        let widened = graph.nodes.get(v as usize)?;
        if widened.op != Op::I2L {
            return None;
        }
        let len = graph.nodes.get(*widened.inputs.first()? as usize)?;
        if len.op != Op::ArrayLength {
            return None;
        }
        // `Op::ArrayLength` is `[ctrl, mem, array_ref]`.
        len.inputs.get(2).copied()
    };
    let mut out: FxHashMap<NodeId, Vec<UpperBound>> = FxHashMap::default();
    for (id, node) in graph.nodes.iter().enumerate() {
        if node.op != Op::If {
            continue;
        }
        let Some(cond) = node.inputs.get(1).and_then(|&c| graph.nodes.get(c as usize)) else {
            continue;
        };
        let (op, x, y) = match (&cond.op, cond.inputs.as_slice()) {
            (Op::Cmp(o), [x, y]) => (*o, *x, *y),
            _ => continue,
        };
        // `LCmp(a, b) cc 0` is `a cc b`; `0 cc LCmp(a, b)` is `b cc a`.
        let pair = if int_const(graph, y) == Some(0) {
            lcmp(x)
        } else if int_const(graph, x) == Some(0) {
            lcmp(y).map(|(p, q)| (q, p))
        } else {
            None
        };
        let Some((a, b)) = pair else {
            continue;
        };
        let (idx, len_id, edge) = match op {
            CmpOp::Lt => (a, b, 0u16),
            CmpOp::Gt => (b, a, 0u16),
            CmpOp::Ge => (a, b, 1u16),
            CmpOp::Le => (b, a, 1u16),
            CmpOp::Eq | CmpOp::Ne => continue,
        };
        let Some(base) = array_of_long_length(len_id) else {
            continue;
        };
        // Cast: arena index to node id.
        let Some(&true_block) = edge_block.get(&(id as NodeId, edge)) else {
            continue;
        };
        out.entry(idx).or_default().push(UpperBound {
            idx,
            base,
            true_block,
        });
    }
    out
}

/// The largest array length this VM hands out: HotSpot's
/// `arrayOopDesc::max_array_length`, `Integer.MAX_VALUE - 2`, which is
/// `vm/src/runtime/interpreter/gc_and_alloc.rs::MAX_JAVA_ARRAY_LENGTH`. Every
/// Java-visible allocation front end refuses a longer array with
/// `OutOfMemoryError: Requested array size exceeds VM limit`: the interpreter's
/// `newarray`/`anewarray` and `multianewarray`, the JIT's `newarray` helpers,
/// and the native allocators (`vm_exec.rs` `new_array` / `new_ref_array`) that
/// `Array.newInstance`, `Arrays.copyOf` and the rest reach. Two paths still
/// reach the heap without it, JNI `New<Type>Array` and the raw `VmHeap`
/// allocators (whose own cap is `i32::MAX`); page
/// `r12w3-iropt2-array-length-cap-at-the-heap-patch-FIXED-20260926.md` closes them.
/// Until it lands, `CRATONVM_JIT_IR_BCE_STRIDE3=0` restores unit stride.
const VM_MAX_ARRAY_LENGTH: i64 = i32::MAX as i64 - 2;

/// The largest induction step whose increment cannot wrap under the loop's
/// own bound (round 12 wave 3, iropt proposal W2-1).
///
/// With `i < x` holding before every increment and `x <= a.length <=
/// VM_MAX_ARRAY_LENGTH`: `i <= VM_MAX_ARRAY_LENGTH - 1`, so `i + s <=
/// VM_MAX_ARRAY_LENGTH - 1 + s`, which is at most `i32::MAX` exactly when `s <=
/// i32::MAX - VM_MAX_ARRAY_LENGTH + 1`, i.e. `s <= 3`. Every `x` the upper-bound
/// facts carry is at most the indexed array's length (the length itself, a
/// `min` with it, `new T[x]`'s own length, a dominating `x <= a.length` or `x <
/// a.length`, `a.length - k`), so the bound holds for all of them.
const MAX_PROVEN_STRIDE: i64 = i32::MAX as i64 - VM_MAX_ARRAY_LENGTH + 1;

/// `CRATONVM_JIT_IR_BCE_STRIDE3` — default ON (round 12 wave 3, W2-1);
/// `0|false|off|no` pins the range proof's induction step back to exactly 1.
/// Read once per [`analyze`], live; `ir_optimize::range_predicate_plan` asks
/// it too, so a predicate is planted only for a step this pass will accept.
pub(crate) fn induction_max_stride() -> i64 {
    {
        MAX_PROVEN_STRIDE
    }
}

/// **Default ON** since 2026-09-06. `CRATONVM_JIT_IR_BCE_RANGE=0` keeps the
/// dominating-redundancy pass and drops only the range proof, so a bisect can
/// separate the two; `CRATONVM_JIT_IR_CHECK_ELIM=0` drops both.
fn read_range_flag() -> bool {
    !matches!(
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
        Ok("0") | Ok("false") | Ok("off") | Ok("no")
    )
}

fn range_enabled() -> bool {
    // Test-only force, then an UNCACHED read -- see `enabled` for why the
    // process-wide memo may not exist in a test binary.
    #[cfg(test)]
    {
        if let Some(forced) = range_forced() {
            return forced;
        }
        return read_range_flag();
    }
    #[cfg(not(test))]
    {
        use std::sync::OnceLock;
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(read_range_flag)
    }
}

/// **Default ON** since 2026-09-17 (B3). `CRATONVM_JIT_IR_RANGE_EDGES=0` keeps
/// the SSA value-range proof but strips its path sensitivity, which is exactly
/// the behaviour between 2026-09-06 and B3: `analyze_graph`'s whole-method
/// answer, with no narrowing along `If` edges.
///
/// It is a third switch rather than a re-use of `CRATONVM_JIT_IR_BCE_RANGE`
/// because the two remove different checks and a bisect that cannot separate
/// them cannot attribute a regression to either -- the same argument the range
/// flag's own doc makes against folding it into `CRATONVM_JIT_IR_CHECK_ELIM`.
fn read_range_edges_flag() -> bool {
    !matches!(
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
        Ok("0") | Ok("false") | Ok("off") | Ok("no")
    )
}

fn range_edges_enabled() -> bool {
    // Uncached under `cfg(test)` for the reason `enabled` documents at length:
    // `flags::runtime_var` honours THREAD-scoped overrides, and a process-wide
    // memo lets whichever test thread arrives first decide for all of them.
    #[cfg(test)]
    {
        return read_range_edges_flag();
    }
    #[cfg(not(test))]
    {
        use std::sync::OnceLock;
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(read_range_edges_flag)
    }
}

/// **Default ON** since 2026-09-06. `CRATONVM_JIT_IR_CHECK_ELIM=0` restores the
/// unconditional guards at every array site, which is every build before this
/// module existed.
fn read_check_elim_flag() -> bool {
    !matches!(
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
        Ok("0") | Ok("false") | Ok("off") | Ok("no")
    )
}

fn enabled() -> bool {
    // # There is no process-wide memo in a TEST binary, deliberately
    //
    // A `OnceLock` here is right in production -- the flag cannot change and
    // this is on the compile path -- and wrong under `cfg(test)`, because
    // `flags::runtime_var` honours `flags::with_thread_overrides`, which is
    // THREAD-scoped. Memoizing a thread-scoped answer in a process-wide cell
    // means whichever test thread calls this first decides the answer for every
    // other test in the binary. When that thread was inside an override asking
    // for `0`, the pass was switched off for the whole run and every test
    // asserting that a check IS elided failed at once -- six of them, together,
    // on 2026-09-07. See `CHECK_ELIM_FORCE` for the reproduction.
    //
    // So the cache is compiled out of test builds rather than merely avoided by
    // convention. That closes the whole class: a future test reaching this gate
    // through `with_thread_overrides` now gets the correct per-thread answer
    // instead of poisoning its neighbours, and needs no note in this file to
    // stay correct. `CheckElimForce` remains the preferred spelling because it
    // says what it does at the call site.
    #[cfg(test)]
    {
        if let Some(forced) = check_elim_forced() {
            return forced;
        }
        return read_check_elim_flag();
    }
    #[cfg(not(test))]
    {
        use std::sync::OnceLock;
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(read_check_elim_flag)
    }
}

#[cfg(test)]
thread_local! {
    /// Test-only override for [`enabled`], so a unit test never depends on the
    /// process environment, on whether the flag has been declared yet, or on
    /// which test won a race.
    ///
    /// # The defect this exists to make impossible
    ///
    /// `the_kill_switch_elides_nothing` used to reach the gate through
    /// `flags::with_thread_overrides(&[("CRATONVM_JIT_IR_CHECK_ELIM", Some("0"))])`.
    /// That override is THREAD-scoped; the `OnceLock` in [`enabled`] is
    /// PROCESS-wide. So when that test happened to be the first caller of
    /// `enabled()` in the binary, it memoized **false** for every other test in
    /// the run, and every test asserting that a check IS elided failed at once.
    ///
    /// Observed 2026-09-07: six of them, together, in a lib-suite run that took
    /// 7.37 s against the usual 0.13 s because a release build and a JVM
    /// workload shared the box -- load changed the interleaving, and that
    /// changed who won the race. Reproduced deterministically by giving the
    /// override a name that sorts first and running `--test-threads=1`:
    /// `enabled()` after the override read `false`, and the same six tests
    /// failed. `a_different_index_keeps_its_bounds_check`,
    /// `a_fresh_allocation_needs_no_null_check_and_still_needs_bounds`,
    /// `the_bound_is_read_off_the_false_edge_of_a_negated_test`,
    /// `the_classic_counted_loop_needs_no_bounds_check`,
    /// `the_header_op_the_builder_actually_emits_is_recognised`,
    /// `the_second_access_to_the_same_element_needs_no_checks`.
    ///
    /// Thread-local, so parallel tests cannot see each other's setting and no
    /// race exists to win. This is the same shape `ir_lower::ls_forced` and
    /// `lib.rs::box_unbox_forced` use, and for the same reason.
    ///
    /// The memo itself is also compiled out of test builds now (see
    /// [`enabled`]), so this cell is the PREFERRED spelling rather than the
    /// only safe one -- it says at the call site what is being forced, where a
    /// `with_thread_overrides` call says only which environment variable.
    static CHECK_ELIM_FORCE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };

    /// The [`range_enabled`] twin. Separate cell, because the two flags are
    /// separately switchable in production (`CRATONVM_JIT_IR_BCE_RANGE=0` keeps
    /// the dominating-redundancy pass and drops only the range proof) and a
    /// test that could not tell them apart could not test that split.
    static RANGE_FORCE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn check_elim_forced() -> Option<bool> {
    CHECK_ELIM_FORCE.with(|c| c.get())
}

#[cfg(test)]
fn range_forced() -> Option<bool> {
    RANGE_FORCE.with(|c| c.get())
}

/// Test-only RAII override of [`enabled`] on this thread.
#[cfg(test)]
struct CheckElimForce;

#[cfg(test)]
impl CheckElimForce {
    /// Force the pass OFF for the duration -- the kill-switch arm.
    fn off() -> CheckElimForce {
        CHECK_ELIM_FORCE.with(|c| c.set(Some(false)));
        CheckElimForce
    }
}

#[cfg(test)]
impl Drop for CheckElimForce {
    fn drop(&mut self) {
        CHECK_ELIM_FORCE.with(|c| c.set(None));
    }
}

/// Test-only RAII override of [`range_enabled`] on this thread.
#[cfg(test)]
struct RangeForce;

#[cfg(test)]
impl RangeForce {
    /// Force the range proof OFF for the duration, leaving the
    /// dominating-redundancy pass on -- the production `=0` shape.
    fn off() -> RangeForce {
        RANGE_FORCE.with(|c| c.set(Some(false)));
        RangeForce
    }
}

#[cfg(test)]
impl Drop for RangeForce {
    fn drop(&mut self) {
        RANGE_FORCE.with(|c| c.set(None));
    }
}

/// Null checks and bounds checks elided in the optimizing tier, process-wide.
///
/// Both counters always printed together with the EMITTED counts by the census
/// caller: an elision count on its own cannot distinguish "the pass works" from
/// "this workload compiles no array accesses in this tier", and that
/// distinction has cost this file wrong conclusions before.
/// Index 2 (JIT round 19 wave 5, lane osr5, IROPS3-2): bounds checks the
/// CLAMP arm proved, a subset of [`range_census`] -- a cell of this table, not
/// a new `static` (`process_global_statics_ratchet`). [`note_check`] never
/// indexes it.
/// Indexes 3.. (JIT round 19 wave 7, lane osr7, OSR4-4 / OSR6-2): the loop
/// field promotion's deopt-pad census ([`note_promote_census`],
/// [`promote_census`]), cells of this table for the same reason.
static ELIDED: [std::sync::atomic::AtomicU64; 3 + PROMOTE_CENSUS_CELLS] = [
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

/// OSR6-2 (lane osr7): promotions of a loop holding a deopt point that the
/// predict switch refused at promotion time
/// (`CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PREDICT`, layer 1).
pub const PROMOTE_CENSUS_PREDICTED: usize = 0;
/// OSR6-2: IR-door rebuilds without deopt-pad promotions after a
/// `promoted field cells` lowering refusal (the predict switch's layer 2).
pub const PROMOTE_CENSUS_RETRIED: usize = 1;
/// OSR6-1: pending write-back operands a pad reads from a resident register
/// (`CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PENDING_RESIDENT`).
pub const PROMOTE_CENSUS_RESIDENT_OPERANDS: usize = 2;
/// OSR5-1: chain points inside a splice given a write-back pad
/// (`CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PENDING_CHAIN`).
pub const PROMOTE_CENSUS_CHAIN_PADS: usize = 3;
/// OSR5-3: joins whose cell value the pending plan took from a value phi
/// (`CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PENDING_BRANCH`).
pub const PROMOTE_CENSUS_JOIN_PHIS: usize = 4;
/// PROMOTE21-5 (JIT round 21 lane promote residual): static / fresh-object
/// loops promoted with deopt pads (`CRATONVM_JIT_IR_LOOP_PROMOTE_DEOPT_PADS`).
pub const PROMOTE_CENSUS_STATIC_FRESH_PAD_LOOPS: usize = 5;
/// PROMOTE21-5: static / fresh-object loops the IR door's retry without
/// deopt-pad promotions refused at a pad-admitted point.
pub const PROMOTE_CENSUS_STATIC_FRESH_WITHHELD: usize = 6;
/// How many promotion cells [`ELIDED`] carries after its three own.
const PROMOTE_CENSUS_CELLS: usize = 7;
/// The census line's names, in cell order.
pub const PROMOTE_CENSUS_NAMES: [&str; PROMOTE_CENSUS_CELLS] = [
    "predicted_refusals",
    "retries",
    "resident_operands",
    "chain_pads",
    "join_phis",
    "static_fresh_pad_loops",
    "static_fresh_withheld",
];

/// OSR4-4 / OSR6-2 (lane osr7): add `n` to promotion census cell `cell`
/// (a `PROMOTE_CENSUS_*` index). Counted at COMPILE time, so a compile later
/// discarded still counts: the cells say how often a shape was met, not how
/// much installed code it produced. Out-of-range cells are ignored.
pub fn note_promote_census(cell: usize, n: u64) {
    if cell < PROMOTE_CENSUS_CELLS {
        ELIDED[3 + cell].fetch_add(n, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The promotion census, in `PROMOTE_CENSUS_*` order.
pub fn promote_census() -> [u64; PROMOTE_CENSUS_CELLS] {
    std::array::from_fn(|i| ELIDED[3 + i].load(std::sync::atomic::Ordering::Relaxed))
}
static EMITTED: [std::sync::atomic::AtomicU64; 2] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// Range-pass refusals by reason, counted at emission. Index is the
/// `REFUSAL_*` code.
static REFUSALS: [std::sync::atomic::AtomicU64; 6] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// Count one range-pass refusal, by reason. Called from the emitter so a
/// discarded compile contributes nothing.
pub fn note_range_refusal(code: u8) {
    if let Some(c) = REFUSALS.get(code as usize) {
        c.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// `(reason, count)` for every non-zero refusal reason, worst first.
pub fn refusal_census() -> Vec<(&'static str, u64)> {
    let mut v: Vec<(&'static str, u64)> = (1u8..6)
        .map(|c| {
            (
                refusal_name(c),
                REFUSALS[c as usize].load(std::sync::atomic::Ordering::Relaxed),
            )
        })
        .filter(|(_, n)| *n > 0)
        .collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    v
}

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

/// IROPS3-2 (lane osr5): count one clamp-proved elision (also counted by
/// [`note_range_proved`]). Called from the emitter.
pub fn note_clamp_proved() {
    ELIDED[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// IROPS3-2: how many of [`range_census`]'s elisions the clamp arm proved.
pub fn clamp_census() -> u64 {
    ELIDED[2].load(std::sync::atomic::Ordering::Relaxed)
}

/// Record one guard-pair decision. `which` is 0 for the null check, 1 for the
/// bounds check.
pub fn note_check(which: usize, elided: bool) {
    let table: &[std::sync::atomic::AtomicU64] = if elided { &ELIDED[..2] } else { &EMITTED };
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
mod tests;

// ── Round 11 (lane iropt) ────────────────────────────────────────────

#[cfg(test)]
mod r11_iropt_tests;

// ── Round 11 wave 6 (lane mid) ───────────────────────────────────────

#[cfg(test)]
mod r11w6_mid_tests;

// ── Round 11 wave 8 (lane mid) ───────────────────────────────────────

#[cfg(test)]
mod r11w8_mid_tests;

// ── Round 11 wave 9 (lane mid): adjacent offsets ─────────────────────

#[cfg(test)]
mod r11w9_mid_tests;



/// Round 12 wave 3 (lane iropt2): the masked hashed-table index and the
/// stride-3 rule's cap.
#[cfg(test)]
mod r12w3_iropt2_mask_index_tests;



/// nio20 wave 6 (lane L4): `a[p & m]` with a constant mask, under `c CMP
/// a.length` (an `If` edge, or the mask predicate's `Guard`), or over a fresh
/// array of constant length.
#[cfg(test)]
mod nio20w6_const_mask_tests;

#[cfg(test)]
mod nio21w3_monotone_tests;

/// nio21 wave 6, lane L2: the dominance facts stop at an exception edge
/// (`exception_edge_fences`).
#[cfg(test)]
mod nio21w6_edge_fence_tests;

// JIT round 22 wave 2 (lane loops, LOOPS22-4): the `long` induction arm.


// t1 jitbce (2026-10-25): `int` constants are read at `int` width.
#[cfg(test)]
mod t1_jitbce_int_width_tests;




