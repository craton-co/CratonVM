// SPDX-License-Identifier: Apache-2.0
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

/// Why the range pass declined a bounds check. Ordered from "nothing to work
/// with" towards "one conjunct short", so the census reads as a work list: the
/// codes near the bottom are the ones a modest extension could recover.
const REFUSAL_NONE: u8 = 0;
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
    let range_on = range_enabled();
    let max_stride = induction_max_stride();
    // `a[h & (a.length - 1)]` (round 12 wave 3; see `masked_below_length`).
    // Asked up front because its `m >= 0` half needs the lattice, which is
    // otherwise computed only when an `idx < length` fact exists.
    let masked_any = range_on
        && mask_index_enabled()
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
                (block < schedule.blocks.len()).then_some((base, block))
            })
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
    let const_mask_on = range_on && const_mask_enabled();
    // JIT round 21 wave 4 (lane bce, BCE21-2): `a[i - c]`, see
    // `negative_offset_index`. Opt-in.
    let const_offset_on = range_on && const_offset_predicate_enabled();

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
    let node_ranges =
        if range_on && (!ranges.is_empty() || !offsets.is_empty() || masked_any || clamp_any) {
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
    let room_on = range_on && range_edges_enabled() && room_offset_enabled();
    let mono_facts = if range_on
        && range_edges_enabled()
        && ((monotone_enabled() && (!ranges.is_empty() || !offsets.is_empty())) || room_on)
    {
        monotone_facts(graph, schedule)
    } else {
        MonotoneFacts::default()
    };
    let mono_phis = if range_on
        && range_edges_enabled()
        && monotone_enabled()
        && (!ranges.is_empty() || !offsets.is_empty() || !mono_facts.room.is_empty())
    {
        monotone_non_negative_phis(graph, schedule, &mono_facts, &non_negative_at)
    } else {
        FxHashSet::default()
    };
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

        for &id in &block.nodes {
            let Some(node) = graph.nodes.get(id as usize) else {
                continue;
            };
            // Set BEFORE the anchor's own facts are recorded: its check is the
            // one that fails into the handler.
            if let Some(&f) = edge_fences.get(&id) {
                fence = Some(narrow_fence(schedule, fence, f));
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
            if let Some(x) = guard_non_null_operand(graph, node) {
                local_deref.insert(x);
                record_block_fact(&mut deref_blocks, &mut deref_after, x, b, fence);
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
                    let mut proved = false;
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
                        if let Some((i, j)) = offset_index(graph, idx) {
                            proved =
                                offsets
                                    .get(&i)
                                    .map_or(&[][..], Vec::as_slice)
                                    .iter()
                                    .any(|f| {
                                        f.base == base
                                            && j <= f.margin
                                            && dominates_reflexive(schedule, b, f.true_block)
                                            && proven_non_negative(
                                                graph,
                                                schedule,
                                                node_ranges.as_ref(),
                                                &non_negative_at,
                                                &mono_phis,
                                                i,
                                                f.true_block,
                                                b,
                                                max_stride,
                                            )
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
                            );
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
                            proved = const_mask_in_range(graph, schedule, &const_below, base, m, b);
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
    out
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
    }
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
    base: NodeId,
    m: i64,
    block: usize,
) -> bool {
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

/// `CRATONVM_JIT_IR_BCE_CONST_MASK` -- default ON (nio20 wave 6, lane L4);
/// `0|false|off|no` drops the constant-mask arm ([`const_mask_in_range`]).
/// Read once per [`analyze`].
fn const_mask_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_BCE_CONST_MASK")
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

/// The value of an `int` constant node, if `id` is one.
fn int_const(graph: &Graph, id: NodeId) -> Option<i64> {
    let n = graph.nodes.get(id as usize)?;
    match n.op {
        Op::Const(c) if n.ty == crate::ir::IrType::Int => Some(c),
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

/// `CRATONVM_JIT_IR_BCE_CLAMP_INDEX` — **default OFF** (opt-in; JIT round 19
/// wave 4, lane irops3, IROPS2-7 of `docs/internal/jit-proposals/jit-r19-irops2-proposals-RETIRED-20261008.md`): the
/// bounds check of a clamped index (`clamp_index_needs`) is dropped when the
/// array is long enough. Read once per [`analyze`]; no `static`
/// (`process_global_statics_ratchet`).
fn clamp_index_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_BCE_CLAMP_INDEX")
}

/// `CRATONVM_JIT_IR_BCE_CLAMP_PREDICATE` -- **default OFF** (JIT round 19
/// wave 5, lane osr5, IROPS3-1 of `jit-r19-irops3-proposals.md`): with
/// `CRATONVM_JIT_IR_BCE_CLAMP_INDEX`, a loop whose body indexes an invariant
/// array by a clamp needing `a.length >= 1` gets a loop-entry predicate
/// `Guard(0 < a.length)` (`ir_optimize::plant_clamp_length_predicates`), and
/// the clamp arm reads it, so the FIRST clamped access loses its check too.
/// No `static` (`process_global_statics_ratchet`).
pub(crate) fn clamp_predicate_enabled() -> bool {
    clamp_index_enabled()
        && cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_BCE_CLAMP_PREDICATE")
}

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

/// `CRATONVM_JIT_IR_BCE_MASK_INDEX` — default ON (round 12 wave 3);
/// `0|false|off|no` drops [`masked_below_length`]'s arm. Read once per
/// [`analyze`].
fn mask_index_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_BCE_MASK_INDEX")
}

/// The value compare `cond`, evaluated to `holds`, proves `>= 0`: `v >= c`
/// (or `v > c - 1`) with `c` a non-negative `int` constant, in either operand
/// order.
fn non_negative_by(graph: &Graph, cond: NodeId, holds: bool) -> Option<NodeId> {
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
    (lo >= 0).then_some(v)
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
    let step = match next_node.inputs.as_slice() {
        &[a, b] if a == phi_id && b != phi_id => b,
        &[a, b] if b == phi_id && a != phi_id => a,
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
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_BCE_MONOTONE")
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
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_BCE_GUARD_BELOW")
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
    (int_typed(graph, v) && int_typed(graph, x)).then_some((v, x))
}

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
    (int_typed(graph, *x) && int_typed(graph, *v)).then_some((*v, *x, k))
}

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

/// `CRATONVM_JIT_IR_BCE_ROOM_OFFSET` (nio21 wave 4, lane L5) -- **default
/// ON**; `0|false|off|no` drops the room-offset arm ([`room_offset_in_range`]).
/// Read once per [`analyze`].
fn room_offset_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_BCE_ROOM_OFFSET")
}

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
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_BCE_VAR_OFFSET")
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
        Op::Const(c) if n.ty == crate::ir::IrType::Int => LenKey::Const(c),
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
                || mono_phis.contains(&v)
        }
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
    let max_stride = max_stride.clamp(1, MAX_PROVEN_STRIDE);
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
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_CONST_OFFSET_PREDICATE")
}

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
    if cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_BCE_STRIDE3") {
        MAX_PROVEN_STRIDE
    } else {
        1
    }
}

/// **Default ON** since 2026-09-06. `CRATONVM_JIT_IR_BCE_RANGE=0` keeps the
/// dominating-redundancy pass and drops only the range proof, so a bisect can
/// separate the two; `CRATONVM_JIT_IR_CHECK_ELIM=0` drops both.
fn read_range_flag() -> bool {
    !matches!(
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_IR_BCE_RANGE").as_deref(),
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
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_IR_RANGE_EDGES").as_deref(),
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
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_IR_CHECK_ELIM").as_deref(),
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
/// How many promotion cells [`ELIDED`] carries after its three own.
const PROMOTE_CENSUS_CELLS: usize = 5;
/// The census line's names, in cell order.
pub const PROMOTE_CENSUS_NAMES: [&str; PROMOTE_CENSUS_CELLS] = [
    "predicted_refusals",
    "retries",
    "resident_operands",
    "chain_pads",
    "join_phis",
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
mod tests {
    use super::*;
    use crate::ir::{IrType, NO_NODE};
    use crate::ir_schedule;

    /// MX16-3: a `multianewarray`'s outer array is a fresh, never-null
    /// reference, like the other allocations.
    #[test]
    fn a_multianewarray_is_definitely_non_null() {
        assert!(definitely_non_null(&Op::MultiNewArray { site: 7 }));
        assert!(definitely_non_null(&Op::NewArray {
            element_type: 10,
            component_class_id: 0
        }));
        assert!(!definitely_non_null(&Op::Param(0)));
    }

    fn empty_graph() -> Graph {
        Graph::empty(0)
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
    ///
    /// # This test used to assert nothing, most of the time
    ///
    /// It reached the gate through `flags::with_thread_overrides`, which is
    /// THREAD-scoped, while `enabled()`'s cache is a process-wide `OnceLock`.
    /// Two consequences, and the second is the one that bit:
    ///
    ///  * whenever another test had already initialised the lock, `enabled()`
    ///    still answered `true` inside the override, and the body returned
    ///    early with no assertion at all -- which under a parallel run is the
    ///    common case, so the kill switch was effectively untested;
    ///  * whenever THIS test won the race instead, it memoized **false** for
    ///    the whole binary and every test asserting a check IS elided failed.
    ///    Six did, together, on 2026-09-07.
    ///
    /// `CheckElimForce` is thread-local, so there is no race to win or lose:
    /// the assertion below now runs on every execution, and no other test can
    /// see the setting.
    #[test]
    fn the_kill_switch_elides_nothing() {
        let _off = CheckElimForce::off();
        assert!(
            !enabled(),
            "the force must actually reach the gate, or everything below is \
             asserting the DEFAULT-ON behaviour under a kill-switch name"
        );
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
    }

    /// The force is THREAD-local and leaves the process-wide cache alone.
    ///
    /// This is the regression test for the defect itself rather than for the
    /// pass: it proves that switching the gate off for one test cannot switch
    /// it off for the binary. Without it, a future edit that "simplifies"
    /// `CheckElimForce` back into `with_thread_overrides` reintroduces the
    /// original failure and nothing here notices -- the six tests it breaks
    /// only fail when the interleaving happens to put the kill-switch test
    /// first, which is why it took a loaded box to surface.
    #[test]
    fn the_kill_switch_force_does_not_escape_its_own_thread() {
        {
            let _off = CheckElimForce::off();
            assert!(!enabled(), "the force must apply on this thread");
        }
        // Dropped: this thread sees the real gate again.
        assert!(enabled(), "the force leaked past its guard on this thread");
        // And it was never visible on another thread, whichever order the two
        // ran in.
        let elsewhere = std::thread::spawn(enabled).join().expect("probe thread");
        assert!(
            elsewhere,
            "the force was visible on another thread: the gate's cache has been \
             poisoned for the whole binary, which is the 2026-09-07 defect"
        );
    }

    /// A `with_thread_overrides` caller cannot poison the gate for the binary.
    ///
    /// This is the test for the CLASS rather than for the one caller that was
    /// fixed. `the_kill_switch_elides_nothing` no longer reaches the gate that
    /// way, but nothing stops the next test from doing so, and the failure it
    /// would reintroduce is invisible in a normal run -- it needs the
    /// interleaving to put that test first, which took a loaded box to produce
    /// on 2026-09-07.
    ///
    /// The guarantee is structural, not conventional: `enabled`'s `OnceLock` is
    /// compiled out of test builds, so a thread-scoped override yields a
    /// thread-scoped answer and the process keeps its own.
    ///
    /// It reads `false` inside the override on THIS thread and `true` on
    /// another thread at the same time -- which a memoized gate cannot do,
    /// whichever of the two won.
    #[test]
    fn a_thread_override_of_the_gate_stays_on_its_own_thread() {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_CHECK_ELIM", Some("0"))],
            || {
                assert!(
                    !enabled(),
                    "the override must be visible on the thread that set it"
                );
                let elsewhere = std::thread::spawn(enabled).join().expect("probe thread");
                assert!(
                    elsewhere,
                    "a THREAD-scoped override was visible on another thread: the \
                     gate has memoized it process-wide, which is the 2026-09-07 \
                     defect -- six tests asserting a check IS elided then fail \
                     together, but only when the interleaving puts this one first"
                );
            },
        );
        assert!(
            enabled(),
            "and the gate is unchanged once the override is dropped"
        );
    }

    /// The range proof has its own switch, and its own force.
    ///
    /// `CRATONVM_JIT_IR_BCE_RANGE=0` keeps the dominating-redundancy pass and
    /// drops only the range proof, so the two are separately switchable in
    /// production; a test that could not tell them apart could not test that
    /// split. The counted loop below is elided by the RANGE proof alone -- with
    /// it off, nothing dominates the access and the check must stay.
    #[test]
    fn the_range_switch_drops_only_the_range_proof() {
        let _off = RangeForce::off();
        assert!(!range_enabled(), "the force must reach the range gate");
        assert!(
            enabled(),
            "and must leave the dominating-redundancy pass alone -- that is \
             what makes this a separate switch"
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
        let (g, sched, st) = counted_loop_with_header(Op::Merge, CmpOp::Lt, 0, 1, true, true, true);
        let e = analyze(&g, &sched);
        assert!(
            e.bounds_elided(st) && e.bounds_range_proved(st),
            "a loop header is an Op::Merge in every graph the IR builder \
             produces; matching only Op::Region makes this pass inert on real \
             code while its hand-built tests stay green",
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
        let (g, sched, st) = counted_loop_with_header(Op::Merge, CmpOp::Ge, 1, 1, true, true, true);
        let e = analyze(&g, &sched);
        assert!(
            e.bounds_elided(st) && e.bounds_range_proved(st),
            "not (i >= a.length) is i < a.length, and that is the edge the loop \
             body actually hangs off in javac output",
        );
    }

    /// A stride of four is REFUSED, and not because four is unusual.
    ///
    /// The non-negativity proof runs: the guard gives `i <= len - 1`, so after
    /// the increment `i <= len - 1 + stride`. `len` can be `i32::MAX - 2`, so
    /// any stride above three overflows there, and a wrapped `i` is negative --
    /// which passes the SIGNED `i < len` test and then indexes behind the
    /// object with the check removed. (Until round 12 wave 3 this test used a
    /// stride of two, against a cap of `i32::MAX`; see `MAX_PROVEN_STRIDE`.)
    #[test]
    fn a_stride_the_proof_cannot_close_keeps_its_check() {
        let (g, sched, st) = counted_loop(CmpOp::Lt, 4, true, true, true);
        let e = analyze(&g, &sched);
        assert!(
            !e.bounds_elided(st),
            "a stride above three can carry the induction variable past len and \
             into overflow; the check stays",
        );
        // A negative step from 0 is not an up-count: `i` goes negative at once.
        let (g, sched, st) = counted_loop(CmpOp::Lt, -1, true, true, true);
        assert!(!analyze(&g, &sched).bounds_elided(st));
    }

    /// Round 12 wave 3 (W2-1): strides two and three close under the VM's
    /// array-length cap, and `CRATONVM_JIT_IR_BCE_STRIDE3=0` restores unit
    /// stride.
    #[test]
    fn strides_two_and_three_close_under_the_array_length_cap() {
        assert_eq!(MAX_PROVEN_STRIDE, 3);
        assert_eq!(VM_MAX_ARRAY_LENGTH + 3 - 1, i64::from(i32::MAX));
        for s in [2, 3] {
            let (g, sched, st) = counted_loop(CmpOp::Lt, s, true, true, true);
            let e = analyze(&g, &sched);
            assert!(
                e.bounds_elided(st) && e.bounds_range_proved(st),
                "stride {s}"
            );
            // The back-edge rule still binds at every stride.
            let (g, sched, st) = counted_loop(CmpOp::Lt, s, false, true, true);
            assert!(!analyze(&g, &sched).bounds_range_proved(st), "stride {s}");
        }
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_BCE_STRIDE3", Some("0"))],
            || {
                let (g, sched, st) = counted_loop(CmpOp::Lt, 2, true, true, true);
                assert!(!analyze(&g, &sched).bounds_elided(st));
            },
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

    /// Build `if (idx < a.length) a[idx] = 1;` where `idx` is a parameter,
    /// optionally wrapped in one narrowing conversion, and return
    /// `(graph, schedule, the ArrayStore)`.
    ///
    /// Straight-line, no loop: the upper bound is a plain dominating branch, so
    /// the ONLY question left is `idx >= 0` — which is the question the SSA
    /// value-range lattice answers and the three hand-written shape arms do not.
    fn guarded_access_with(narrow: Option<Op>) -> (Graph, Schedule, NodeId) {
        let mut g = empty_graph();
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let arr = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let raw = g.add(Op::Param(1), IrType::Int, vec![start], None);
        let idx = match narrow {
            Some(op) => g.add(op, IrType::Int, vec![raw], None),
            None => raw,
        };
        let len = g.add(Op::ArrayLength, IrType::Int, vec![c0, m0, arr], None);
        let cmp = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![idx, len], None);
        let iff = g.add(Op::If, IrType::Control, vec![c0, cmp], None);
        let t = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
        let _f = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let st = g.add(
            Op::ArrayStore(MemKind::Int),
            IrType::Void,
            vec![t, m0, arr, idx, one],
            Some(0),
        );
        g.exit = g.add(Op::Return, IrType::Void, vec![t, idx], Some(1));
        let sched = ir_schedule::schedule(&g);
        (g, sched, st)
    }

    /// `range_analysis::analyze_graph` — a sound SSA interval lattice with
    /// JVMS-correct transfer functions, fully tested — had no call site outside
    /// its own `mod tests`, so this tier's `idx >= 0` question was answered by
    /// three hand-written shapes and nothing else.
    ///
    /// `(char) p` is `Op::I2C`, whose range is `[0, 65535]`. It is not a
    /// constant, not an array length and not a phi, so every shape arm refuses
    /// it — and the check on `a[(char) p]` under a dominating `(char) p <
    /// a.length` stayed, forever, for an index that cannot be negative.
    #[test]
    fn a_char_narrowed_index_is_proven_non_negative_by_the_range_lattice() {
        let (g, sched, st) = guarded_access_with(Some(Op::I2C));
        let e = analyze(&g, &sched);
        assert!(
            e.bounds_elided(st),
            "(char) p is in [0, 65535], and the branch bounds it above",
        );
        assert!(
            e.bounds_range_proved(st),
            "and the RANGE pass is what proved it — there is no earlier access \
             for the redundancy pass to work from",
        );
    }

    /// The twin that differs only in the conversion: `(byte) p` is
    /// `[-128, 127]`, which admits a negative index, and a negative index
    /// PASSES the signed `idx < a.length` test. The check must stay.
    ///
    /// Without this, the test above would pass just as well against a range
    /// arm that answered `true` for everything.
    #[test]
    fn a_byte_narrowed_index_still_keeps_its_check() {
        let (g, sched, st) = guarded_access_with(Some(Op::I2B));
        let e = analyze(&g, &sched);
        assert!(
            !e.bounds_elided(st),
            "(byte) p can be -1, which satisfies idx < a.length and indexes \
             behind the array",
        );
    }

    /// And the un-narrowed parameter, to pin that the lattice is what changed
    /// the answer rather than the fixture shape.
    #[test]
    fn an_unconstrained_index_keeps_its_check() {
        let (g, sched, st) = guarded_access_with(None);
        let e = analyze(&g, &sched);
        assert!(!e.bounds_elided(st), "nothing bounds p below");
    }

    /// `if (i >= 0) { if (i < a.length) a[i] = 1; }` -- B3's shape, built with
    /// the SIGN test on the outer branch so the only thing that can prove
    /// `i >= 0` is the edge narrowing.
    ///
    /// `sign_op` picks the outer comparison, and `taken` picks which of its two
    /// edges the inner branch hangs off, so one fixture covers the true edge,
    /// the false edge (where the fact is the NEGATION) and the inverted
    /// spelling.
    fn nested_guarded_access_vs(
        sign_op: CmpOp,
        bound: i64,
        hang_off_true_edge: bool,
    ) -> (Graph, Schedule, NodeId) {
        let mut g = empty_graph();
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let arr = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let idx = g.add(Op::Param(1), IrType::Int, vec![start], None);

        let k = g.add(Op::Const(bound), IrType::Int, vec![], None);
        let sign = g.add(Op::Cmp(sign_op), IrType::Int, vec![idx, k], None);
        let outer = g.add(Op::If, IrType::Control, vec![c0, sign], None);
        let ot = g.add(Op::Proj(0), IrType::Control, vec![outer], None);
        let of = g.add(Op::Proj(1), IrType::Control, vec![outer], None);
        let inner_ctrl = if hang_off_true_edge { ot } else { of };

        let len = g.add(
            Op::ArrayLength,
            IrType::Int,
            vec![inner_ctrl, m0, arr],
            None,
        );
        let cmp = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![idx, len], None);
        let iff = g.add(Op::If, IrType::Control, vec![inner_ctrl, cmp], None);
        let t = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
        let _f = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let st = g.add(
            Op::ArrayStore(MemKind::Int),
            IrType::Void,
            vec![t, m0, arr, idx, one],
            Some(0),
        );
        g.exit = g.add(Op::Return, IrType::Void, vec![t, idx], Some(1));
        let sched = ir_schedule::schedule(&g);
        (g, sched, st)
    }

    /// The common case: the outer test compares against zero.
    fn nested_guarded_access(
        sign_op: CmpOp,
        hang_off_true_edge: bool,
    ) -> (Graph, Schedule, NodeId) {
        nested_guarded_access_vs(sign_op, 0, hang_off_true_edge)
    }

    /// **B3.** The shape `NOTES-opts.md` named as the thing the path-INSENSITIVE
    /// evaluator "proves nothing about".
    ///
    /// `i` is a bare `Param`: no constant, no array length, no unit-stride phi,
    /// and `analyze_graph`'s whole-method answer for it is top. The ONLY thing
    /// that makes `a[i]` provably in bounds is that reaching it required
    /// `i >= 0` to be true, which is a fact about an `If` EDGE.
    /// `an_unconstrained_index_keeps_its_check` is the same fixture without the
    /// outer guard and still refuses, which is what stops this from passing for
    /// a reason other than the one it names.
    #[test]
    fn an_index_guarded_non_negative_is_proven_by_edge_narrowing() {
        let (g, sched, st) = nested_guarded_access(CmpOp::Ge, true);
        let e = analyze(&g, &sched);
        assert!(
            e.bounds_elided(st),
            "i >= 0 on the way in, i < a.length at the access: both halves proven",
        );
        assert!(
            e.bounds_range_proved(st),
            "and by the RANGE pass -- there is no earlier access here for the \
             redundancy pass to work from",
        );
    }

    /// The same fact reached through the FALSE edge of `i < 0`.
    #[test]
    fn a_fact_can_be_read_off_a_false_edge() {
        let (g, sched, st) = nested_guarded_access(CmpOp::Lt, false);
        let e = analyze(&g, &sched);
        assert!(e.bounds_elided(st), "!(i < 0) is i >= 0, which is enough");
    }

    /// The arm that tells `CmpOp::negate` from `CmpOp::swapped` -- which the
    /// zero-bound fixtures CANNOT. Against `0`, `Lt.negate() == Ge` and
    /// `Lt.swapped() == Gt` both prove non-negative, so either would elide and
    /// a test built on them passes with the two confused. This was that test
    /// for one sitting: it was written claiming to separate them, and an A/B
    /// that replaced `negate` with `swapped` did not move it.
    ///
    /// Against `-1` they separate. The false edge of `i < -1` proves
    /// `i >= -1`, which still admits `-1`, so the check must STAY. The mirror
    /// image would read `i > -1` -- i.e. `i >= 0` -- and delete a check that a
    /// value of `-1` needs: `-1` passes the signed `i < a.length` test and
    /// indexes behind the array.
    #[test]
    fn the_false_edge_takes_the_negation_and_not_the_mirror_image() {
        let (g, sched, st) = nested_guarded_access_vs(CmpOp::Lt, -1, false);
        let e = analyze(&g, &sched);
        assert!(
            !e.bounds_elided(st),
            "!(i < -1) is i >= -1, and i == -1 indexes behind the array",
        );
    }

    /// And the arm that must NOT elide: hanging the access off the TRUE edge of
    /// `i < 0` puts it where `i` is provably negative.
    ///
    /// A negative index passes the signed `i < a.length` test and indexes
    /// behind the array, which is the out-of-bounds heap write the range
    /// module's header opens by naming. If edge narrowing ever reports a fact
    /// for the wrong edge, this is the test that fails.
    #[test]
    fn an_index_guarded_negative_keeps_its_check() {
        let (g, sched, st) = nested_guarded_access(CmpOp::Lt, true);
        let e = analyze(&g, &sched);
        assert!(
            !e.bounds_elided(st),
            "on the true edge of `i < 0` the index is negative and the check is \
             the only thing standing between it and the heap",
        );
    }

    /// The kill switch really kills it.
    ///
    /// `CRATONVM_JIT_IR_RANGE_EDGES=0` has to restore the pre-B3 answer on the
    /// very fixture B3 added, or it is not a way back. This also anti-vacuates
    /// `an_index_guarded_non_negative_is_proven_by_edge_narrowing`: the two
    /// differ only in the flag, so they cannot both be passing for a reason
    /// unrelated to the narrowing.
    #[test]
    fn the_edge_narrowing_switch_restores_the_old_answer() {
        let (g, sched, st) = nested_guarded_access(CmpOp::Ge, true);
        let elided = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_RANGE_EDGES", Some("0"))],
            || analyze(&g, &sched).bounds_elided(st),
        );
        assert!(
            !elided,
            "with the narrowing off, `i` is a bare Param again and nothing \
             bounds it below",
        );
    }

    /// The walk accumulates facts as it goes and queries them with
    /// `dominates`, so a dominating block must be processed first. Block
    /// indices are POST-layout and frequency-driven layout permutes them, so
    /// index order does not guarantee that; this order does.
    ///
    /// Asserted as the helper's contract rather than end-to-end: producing a
    /// schedule whose layout actually inverts a dominator pair needs profile
    /// frequencies a unit fixture cannot pin, and a behavioural test that
    /// happened to get the identity permutation would prove nothing.
    #[test]
    fn the_block_walk_visits_every_dominator_before_what_it_dominates() {
        for (_g, sched) in [
            {
                let (g, s, _) = counted_loop(CmpOp::Lt, 1, true, true, true);
                (g, s)
            },
            {
                let (g, s, _) = counted_loop(CmpOp::Lt, 1, false, true, true);
                (g, s)
            },
        ] {
            let order = dominator_order(&sched);
            assert_eq!(
                order.len(),
                sched.blocks.len(),
                "every block must be visited exactly once"
            );
            let mut seen = vec![false; sched.blocks.len()];
            for &b in &order {
                assert!(!seen[b], "block {b} visited twice");
                seen[b] = true;
            }
            let position: Vec<usize> = {
                let mut p = vec![0usize; order.len()];
                for (i, &b) in order.iter().enumerate() {
                    p[b] = i;
                }
                p
            };
            for b in 0..sched.blocks.len() {
                for d in 0..sched.blocks.len() {
                    if d != b && sched.dom.dominates(d, b) {
                        assert!(
                            position[d] < position[b],
                            "block {d} dominates {b} but is walked after it, so \
                             its facts are not yet available when {b} is processed"
                        );
                    }
                }
            }
        }
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

    /// A field `Load` proves nothing about its base's nullness (r9).
    ///
    /// When this test was written, `ir_lower`'s helper-less inline getfield
    /// answered `0` for a null receiver and CONTINUED, so a `Load` of the
    /// interned `null` literal was followed by the next node running with that
    /// base still null; with the `Load` recording a dereference fact, the
    /// `ArrayLoad` of the same literal below had its null test elided and read
    /// `[0 + HEADER + idx*4]`. Since round 9 wave 2 (iropt X3) that inline
    /// `Load` deopts (`NullCheck`) on a null receiver instead -- but this pass
    /// cannot see which lowering a `Load` gets (a helper-backed one, a spliced
    /// one, one whose check a later change relaxes), so it still must not
    /// treat the `Load` as proof, and the test still pins that.
    #[test]
    fn a_field_load_does_not_prove_its_base_non_null() {
        let mut g = empty_graph();
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let null = g.add(Op::Const(0), IrType::Ref, vec![], None);
        let field = g.add(Op::Const(0), IrType::Int, vec![], None);
        let idx = g.add(Op::Param(0), IrType::Int, vec![start], None);
        let f = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![ctrl, mem, null, field],
            Some(0),
        );
        let a = g.add(
            Op::ArrayLoad(MemKind::Int),
            IrType::Int,
            vec![ctrl, f, null, idx],
            Some(1),
        );
        let sum = g.add(Op::Add, IrType::Int, vec![f, a], Some(2));
        g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, sum], Some(3));

        let sched = ir_schedule::schedule(&g);
        let e = analyze(&g, &sched);
        assert!(
            !e.null_elided(a),
            "a field read of the base is not a null test the array access can lean on"
        );
    }

    /// An access in a block the entry cannot reach keeps BOTH of its checks.
    ///
    /// `Dominators::dominates(d, b)` answers `true` for every `d` when `b` is
    /// unreachable -- its documented contract, matching the dense fixpoint it
    /// replaced. Read as a permission that is fail-OPEN: without the
    /// reachability test in [`dominates`], the second `ArrayLoad` below sits in
    /// a block with no predecessors and had its null test AND its bounds test
    /// elided on the strength of the first one, which does not dominate it at
    /// all. `ir_lower` emits every block in `schedule.blocks`, so the
    /// guard-free bytes are really emitted; only "no branch targets that block"
    /// kept it off the wrong-code list.
    #[test]
    fn unreachable_block_proves_nothing() {
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
        // A block head with no predecessors: the entry cannot reach it.
        let orphan = g.add(Op::Region, IrType::Control, vec![], None);
        let l2 = g.add(
            Op::ArrayLoad(MemKind::Int),
            IrType::Int,
            vec![orphan, mem, arr, idx],
            Some(1),
        );
        g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, l1], Some(2));

        let sched = ir_schedule::schedule(&g);
        let lb = sched.node_to_block[l2 as usize];
        assert!(
            lb < sched.blocks.len() && !sched.dom.is_reachable(lb),
            "the second access must land in the unreachable block for this test to \
             mean anything (block {lb} of {})",
            sched.blocks.len(),
        );
        let e = analyze(&g, &sched);
        assert!(
            !e.null_elided(l2),
            "nothing dominates an unreachable block, so no dereference proves its base",
        );
        assert!(
            !e.bounds_elided(l2),
            "nothing dominates an unreachable block, so no earlier check proves this pair",
        );
    }
}

// ── Round 11 (lane iropt) ────────────────────────────────────────────

#[cfg(test)]
mod r11_iropt_tests {
    use super::*;
    use crate::ir::{IrType, NO_NODE};
    use crate::ir_schedule;

    /// `Guard(arr != null); arr[i]` — the guard deopts on a null `arr`, so the
    /// access behind it needs no null check of its own. The bounds check is
    /// untouched: a guard proves nothing about the index.
    #[test]
    fn a_non_null_guard_is_a_null_check_fact() {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let arr = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let idx = g.add(Op::Param(1), IrType::Int, vec![start], None);
        let null = g.add(Op::Const(0), IrType::Ref, vec![], None);
        let cond = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![arr, null], Some(0));
        let guard = g.add(
            Op::Guard { bci: 0 },
            IrType::Void,
            vec![ctrl, cond],
            Some(0),
        );
        let load = g.add(
            Op::ArrayLoad(MemKind::Int),
            IrType::Int,
            vec![ctrl, mem, arr, idx],
            Some(1),
        );
        g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, load], Some(2));

        assert_eq!(
            guard_non_null_operand(&g, &g.nodes[guard as usize]),
            Some(arr)
        );
        assert_eq!(guard_non_null_operand(&g, &g.nodes[load as usize]), None);

        let sched = ir_schedule::schedule(&g);
        let e = analyze(&g, &sched);
        assert!(
            e.null_elided(load),
            "the guard already rejected a null `arr`"
        );
        assert!(!e.bounds_elided(load));
    }

    /// `Guard(x == null)` continues only when `x` IS null, and a compare of
    /// null with null proves nothing.
    #[test]
    fn only_a_not_equal_null_guard_proves_non_null() {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let x = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let null = g.add(Op::Const(0), IrType::Ref, vec![], None);
        let eq = g.add(Op::Cmp(CmpOp::Eq), IrType::Int, vec![x, null], None);
        let g_eq = g.add(Op::Guard { bci: 0 }, IrType::Void, vec![ctrl, eq], None);
        let nn = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![null, null], None);
        let g_nn = g.add(Op::Guard { bci: 0 }, IrType::Void, vec![ctrl, nn], None);
        let rev = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![null, x], None);
        let g_rev = g.add(Op::Guard { bci: 0 }, IrType::Void, vec![ctrl, rev], None);
        assert_eq!(guard_non_null_operand(&g, &g.nodes[g_eq as usize]), None);
        assert_eq!(guard_non_null_operand(&g, &g.nodes[g_nn as usize]), None);
        assert_eq!(
            guard_non_null_operand(&g, &g.nodes[g_rev as usize]),
            Some(x)
        );
    }
}

// ── Round 11 wave 6 (lane mid) ───────────────────────────────────────

#[cfg(test)]
mod r11w6_mid_tests {
    use super::*;
    use crate::ir::{IrType, NO_NODE};
    use crate::ir_schedule;

    /// What runs in front of the loop.
    enum Prelude {
        Nothing,
        /// `if (n > a.length) ..`, with the loop on its false edge (where
        /// `n <= a.length`) or, for the negative twin, on its true edge.
        IfEdge {
            loop_on_true_edge: bool,
        },
        /// `Guard(n <op> a.length)` in the entry block.
        Guard(CmpOp),
    }

    /// The array the body stores into, and the loop bound that goes with it.
    enum Target {
        /// `a[i]`, loop bound `n`.
        Param,
        /// `b = new int[n]; b[i]`, loop bound `n`.
        NewOfBound,
        /// `b = new int[m]; b[i]`, loop bound `n`.
        NewOfOther,
        /// `b = new int[a.length]; b[i]`, loop bound a SECOND `a.length` read.
        NewOfLength,
        /// `b = new int[8]; b[i]`, loop bound a second `8` node.
        NewOfConst,
        /// `a[i]`, loop bound `Math.min(n, a.length)`.
        ParamUnderMin,
        /// `a[i]`, loop bound `Math.max(n, a.length)`: proves nothing.
        ParamUnderMax,
    }

    /// `[prelude] for (int i = 0; i < bound; i++) t[i] = 1;`, top-tested with
    /// the body on the true edge (which is also the back edge). Returns
    /// `(graph, schedule, the ArrayStore)`.
    fn loop_over(prelude: Prelude, target: Target) -> (Graph, Schedule, NodeId) {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let arr = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let n = g.add(Op::Param(1), IrType::Int, vec![start], None);
        let m = g.add(Op::Param(2), IrType::Int, vec![start], None);
        let entry = match prelude {
            Prelude::Nothing => c0,
            Prelude::IfEdge { loop_on_true_edge } => {
                let len = g.add(Op::ArrayLength, IrType::Int, vec![c0, m0, arr], None);
                let gt = g.add(Op::Cmp(CmpOp::Gt), IrType::Int, vec![n, len], None);
                let pif = g.add(Op::If, IrType::Control, vec![c0, gt], None);
                let t = g.add(Op::Proj(0), IrType::Control, vec![pif], None);
                let f = g.add(Op::Proj(1), IrType::Control, vec![pif], None);
                if loop_on_true_edge {
                    t
                } else {
                    f
                }
            }
            Prelude::Guard(op) => {
                let len = g.add(Op::ArrayLength, IrType::Int, vec![c0, m0, arr], None);
                let cmp = g.add(Op::Cmp(op), IrType::Int, vec![n, len], None);
                let _ = g.add(Op::Guard { bci: 0 }, IrType::Void, vec![c0, cmp], Some(0));
                c0
            }
        };
        let new_array = |g: &mut Graph, count: NodeId| {
            g.add(
                Op::NewArray {
                    element_type: 10,
                    component_class_id: 0,
                },
                IrType::Ref,
                vec![entry, m0, count],
                None,
            )
        };
        let target_arr = match target {
            Target::Param | Target::ParamUnderMin | Target::ParamUnderMax => arr,
            Target::NewOfBound => new_array(&mut g, n),
            Target::NewOfOther => new_array(&mut g, m),
            Target::NewOfLength => {
                let len = g.add(Op::ArrayLength, IrType::Int, vec![entry, m0, arr], None);
                new_array(&mut g, len)
            }
            Target::NewOfConst => {
                let eight = g.add(Op::Const(8), IrType::Int, vec![], None);
                new_array(&mut g, eight)
            }
        };
        let region = g.add(Op::Merge, IrType::Control, vec![entry], None);
        let bound = match target {
            Target::NewOfLength => g.add(Op::ArrayLength, IrType::Int, vec![region, m0, arr], None),
            Target::NewOfConst => g.add(Op::Const(8), IrType::Int, vec![], None),
            Target::ParamUnderMin | Target::ParamUnderMax => {
                let len = g.add(Op::ArrayLength, IrType::Int, vec![region, m0, arr], None);
                let op = if matches!(target, Target::ParamUnderMin) {
                    crate::ir::ScalarOp::MinI
                } else {
                    crate::ir::ScalarOp::MaxI
                };
                g.add(Op::ScalarIntrinsic(op), IrType::Int, vec![n, len], None)
            }
            _ => n,
        };
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let iv = g.add(Op::Phi, IrType::Int, vec![region, zero], None);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let next = g.add(Op::Add, IrType::Int, vec![iv, one], None);
        g.nodes[iv as usize].inputs.push(next);
        let cmp = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![iv, bound], None);
        let iff = g.add(Op::If, IrType::Control, vec![region, cmp], None);
        let body = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
        let exit = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
        g.nodes[region as usize].inputs.push(body);
        let st = g.add(
            Op::ArrayStore(MemKind::Int),
            IrType::Void,
            vec![body, m0, target_arr, iv, one],
            Some(0),
        );
        g.exit = g.add(Op::Return, IrType::Void, vec![exit, iv], Some(1));
        let sched = ir_schedule::schedule(&g);
        (g, sched, st)
    }

    fn range_proved(prelude: Prelude, target: Target) -> bool {
        let (g, sched, st) = loop_over(prelude, target);
        let e = analyze(&g, &sched);
        e.bounds_elided(st) && e.bounds_range_proved(st)
    }

    /// `if (n > a.length) ..; for (i = 0; i < n; i++) a[i] = 1;` -- the loop
    /// runs where `n <= a.length`, so `i < n` is `i < a.length`.
    #[test]
    fn a_length_precheck_bounds_a_loop_over_n() {
        assert!(range_proved(
            Prelude::IfEdge {
                loop_on_true_edge: false
            },
            Target::Param
        ));
    }

    /// The same loop on the OTHER edge, where `n > a.length`: `a[n - 1]` is
    /// out of bounds and the check must stay.
    #[test]
    fn the_wrong_edge_of_the_precheck_proves_nothing() {
        assert!(!range_proved(
            Prelude::IfEdge {
                loop_on_true_edge: true
            },
            Target::Param
        ));
    }

    /// Without a precheck, `i < n` says nothing about `a`: the check stays,
    /// and the census still reads "no length test".
    #[test]
    fn a_loop_over_n_without_a_fact_keeps_its_check() {
        let (g, sched, st) = loop_over(Prelude::Nothing, Target::Param);
        let e = analyze(&g, &sched);
        assert!(!e.bounds_elided(st));
        assert_eq!(e.bounds_refusal(st), REFUSAL_NO_LENGTH_TEST);
    }

    /// A guard that deopts unless `n <= a.length` (or `n < a.length`) proves
    /// the same as the precheck edge.
    #[test]
    fn a_guard_n_at_most_length_bounds_a_loop_over_n() {
        for op in [CmpOp::Le, CmpOp::Lt] {
            assert!(range_proved(Prelude::Guard(op), Target::Param), "{op:?}");
        }
    }

    /// `Guard(n >= a.length)` bounds `n` from BELOW, and `Eq`/`Ne` order
    /// nothing: nothing proven.
    #[test]
    fn a_guard_the_other_way_proves_nothing() {
        for op in [CmpOp::Ge, CmpOp::Gt, CmpOp::Eq, CmpOp::Ne] {
            assert!(!range_proved(Prelude::Guard(op), Target::Param), "{op:?}");
        }
    }

    /// `b = new int[n]; for (i < n) b[i]` -- `b.length == n` for `b`'s life.
    #[test]
    fn an_array_allocated_with_the_bound_is_in_bounds() {
        assert!(range_proved(Prelude::Nothing, Target::NewOfBound));
    }

    /// `b = new int[a.length]; for (i < a.length) b[i]` with two separate
    /// length reads of `a`: the same array, so the same length.
    #[test]
    fn an_array_allocated_with_another_arrays_length_is_in_bounds() {
        assert!(range_proved(Prelude::Nothing, Target::NewOfLength));
    }

    /// Two `8` constants are one length.
    #[test]
    fn an_array_allocated_with_the_constant_bound_is_in_bounds() {
        assert!(range_proved(Prelude::Nothing, Target::NewOfConst));
    }

    /// `for (i < Math.min(n, a.length)) a[i]`: the minimum is at most the
    /// length. `Math.max` is not.
    #[test]
    fn a_bound_under_math_min_of_the_length_is_in_bounds() {
        assert!(range_proved(Prelude::Nothing, Target::ParamUnderMin));
        assert!(!range_proved(Prelude::Nothing, Target::ParamUnderMax));
    }

    /// `b = new int[m]; for (i < n) b[i]`: a different count proves nothing.
    #[test]
    fn an_array_allocated_with_a_different_count_keeps_its_check() {
        assert!(!range_proved(Prelude::Nothing, Target::NewOfOther));
    }

    /// `CRATONVM_JIT_IR_BCE_RANGE=0` drops these facts with the rest of the
    /// range proof.
    #[test]
    fn the_range_switch_drops_the_new_facts() {
        let _off = RangeForce::off();
        assert!(!range_proved(
            Prelude::IfEdge {
                loop_on_true_edge: false
            },
            Target::Param
        ));
        assert!(!range_proved(Prelude::Nothing, Target::NewOfBound));
    }
}

// ── Round 11 wave 8 (lane mid) ───────────────────────────────────────

#[cfg(test)]
mod r11w8_mid_tests {
    use super::*;
    use crate::ir::{IrType, NO_NODE};
    use crate::ir_schedule;

    /// The induction variable's start.
    #[derive(Clone, Copy)]
    enum Start {
        Zero,
        N,
        /// `Sub(a.length, k)`.
        LengthSub(i64),
        /// `Add(a.length, k)`.
        LengthAdd(i64),
        Length,
    }

    /// A limit the test compares the induction variable against.
    #[derive(Clone, Copy)]
    enum Limit {
        N,
        /// `Sub(a.length, k)`.
        LengthSub(i64),
    }

    /// The test that keeps the loop running, on the `If`'s TRUE edge.
    #[derive(Clone, Copy)]
    enum Stay {
        Below(Limit),
        AtMost(Limit),
        AtLeast(i64),
    }

    /// `[Guard(n <pre> a.length);] for (i = start; <stay>; i += step)
    /// a[i] = 1;`, the body on the true edge (also the back edge). Returns
    /// whether the store's bounds check is proven away by the range pass.
    fn proved(pre: Option<CmpOp>, start: Start, stay: Stay, step: i64) -> bool {
        let mut g = Graph::empty(0);
        let s = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = s;
        let c0 = g.add(Op::Proj(0), IrType::Control, vec![s], None);
        let m0 = g.add(Op::Proj(1), IrType::Memory, vec![s], None);
        let arr = g.add(Op::Param(0), IrType::Ref, vec![s], None);
        let n = g.add(Op::Param(1), IrType::Int, vec![s], None);
        if let Some(op) = pre {
            let len = g.add(Op::ArrayLength, IrType::Int, vec![c0, m0, arr], None);
            let cmp = g.add(Op::Cmp(op), IrType::Int, vec![n, len], None);
            let _ = g.add(Op::Guard { bci: 0 }, IrType::Void, vec![c0, cmp], Some(0));
        }
        let length_minus = |g: &mut Graph, k: i64, sub: bool| {
            let len = g.add(Op::ArrayLength, IrType::Int, vec![c0, m0, arr], None);
            let kc = g.add(Op::Const(k), IrType::Int, vec![], None);
            let op = if sub { Op::Sub } else { Op::Add };
            g.add(op, IrType::Int, vec![len, kc], None)
        };
        let init = match start {
            Start::Zero => g.add(Op::Const(0), IrType::Int, vec![], None),
            Start::N => n,
            Start::LengthSub(k) => length_minus(&mut g, k, true),
            Start::LengthAdd(k) => length_minus(&mut g, k, false),
            Start::Length => g.add(Op::ArrayLength, IrType::Int, vec![c0, m0, arr], None),
        };
        let limit = |g: &mut Graph, l: Limit| match l {
            Limit::N => n,
            Limit::LengthSub(k) => length_minus(g, k, true),
        };
        let region = g.add(Op::Merge, IrType::Control, vec![c0], None);
        let iv = g.add(Op::Phi, IrType::Int, vec![region, init], None);
        let stride = g.add(Op::Const(step), IrType::Int, vec![], None);
        let next = g.add(Op::Add, IrType::Int, vec![iv, stride], None);
        g.nodes[iv as usize].inputs.push(next);
        let cmp = match stay {
            Stay::Below(l) => {
                let x = limit(&mut g, l);
                g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![iv, x], None)
            }
            Stay::AtMost(l) => {
                let x = limit(&mut g, l);
                g.add(Op::Cmp(CmpOp::Le), IrType::Int, vec![iv, x], None)
            }
            Stay::AtLeast(c) => {
                let x = g.add(Op::Const(c), IrType::Int, vec![], None);
                g.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![iv, x], None)
            }
        };
        let iff = g.add(Op::If, IrType::Control, vec![region, cmp], None);
        let body = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
        let exit = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
        g.nodes[region as usize].inputs.push(body);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let st = g.add(
            Op::ArrayStore(MemKind::Int),
            IrType::Void,
            vec![body, m0, arr, iv, one],
            Some(0),
        );
        g.exit = g.add(Op::Return, IrType::Void, vec![exit, iv], Some(1));
        let sched = ir_schedule::schedule(&g);
        let e = analyze(&g, &sched);
        e.bounds_elided(st) && e.bounds_range_proved(st)
    }

    /// `for (i = 0; i <= n; i++) a[i]` is in bounds only when `n < a.length`:
    /// the non-strict `n <= a.length` leaves `a[a.length]`.
    #[test]
    fn an_inclusive_loop_needs_a_strict_fact() {
        let up_to_n = Stay::AtMost(Limit::N);
        assert!(proved(Some(CmpOp::Lt), Start::Zero, up_to_n, 1));
        assert!(!proved(Some(CmpOp::Le), Start::Zero, up_to_n, 1));
        assert!(!proved(None, Start::Zero, up_to_n, 1));
    }

    /// `i <= a.length - 1` and `i < a.length - 1` need no fact at all; a
    /// subtraction of zero is not below the length.
    #[test]
    fn a_limit_of_length_minus_k_is_below_the_length() {
        assert!(proved(
            None,
            Start::Zero,
            Stay::AtMost(Limit::LengthSub(1)),
            1
        ));
        assert!(proved(
            None,
            Start::Zero,
            Stay::Below(Limit::LengthSub(1)),
            1
        ));
        assert!(!proved(
            None,
            Start::Zero,
            Stay::AtMost(Limit::LengthSub(0)),
            1
        ));
    }

    /// `for (i = a.length - 1; i >= 0; i--) a[i]`, the same from
    /// `a.length + (-1)`, and from a start the pre-header guard puts below the
    /// length, by any negative step.
    #[test]
    fn a_down_count_from_below_the_length_is_in_bounds() {
        let to_zero = Stay::AtLeast(0);
        assert!(proved(None, Start::LengthSub(1), to_zero, -1));
        assert!(proved(None, Start::LengthAdd(-1), to_zero, -1));
        assert!(proved(Some(CmpOp::Lt), Start::N, to_zero, -1));
        assert!(proved(Some(CmpOp::Lt), Start::N, to_zero, -2));
    }

    /// The down-count negatives: a start AT the length, a non-strict fact on
    /// the start, no fact, a test that admits `-1`, and a step that is not
    /// negative. Each one can index out of range.
    #[test]
    fn a_down_count_that_can_leave_the_array_keeps_its_check() {
        let to_zero = Stay::AtLeast(0);
        assert!(!proved(None, Start::Length, to_zero, -1));
        assert!(!proved(Some(CmpOp::Le), Start::N, to_zero, -1));
        assert!(!proved(None, Start::N, to_zero, -1));
        assert!(!proved(None, Start::LengthSub(1), Stay::AtLeast(-1), -1));
        assert!(!proved(None, Start::LengthSub(1), to_zero, 1));
        assert!(!proved(None, Start::LengthSub(1), to_zero, 0));
    }

    /// `CRATONVM_JIT_IR_BCE_RANGE=0` drops the wave-8 facts with the rest.
    #[test]
    fn the_range_switch_drops_the_wave8_facts() {
        let _off = RangeForce::off();
        assert!(!proved(
            Some(CmpOp::Lt),
            Start::Zero,
            Stay::AtMost(Limit::N),
            1
        ));
        assert!(!proved(None, Start::LengthSub(1), Stay::AtLeast(0), -1));
    }

    /// Wave 9 (mid proposal W8-1): the down count's `i >= 0` half is the loop
    /// test's own edge fact (`non_negative_at`). With default flags the down
    /// counts are proved and the negatives keep their checks. The arm is an
    /// edge fact, so `CRATONVM_JIT_IR_RANGE_EDGES=0` withdraws it with every
    /// other one (that switch must restore the pre-narrowing answer), and it
    /// does not stand alone without the SSA range lattice either: the upper
    /// half of a down count's proof comes from the lattice.
    #[test]
    fn a_down_count_is_proved_by_its_loop_test_edge() {
        let to_zero = Stay::AtLeast(0);
        assert!(proved(None, Start::LengthSub(1), to_zero, -1));
        assert!(proved(Some(CmpOp::Lt), Start::N, to_zero, -2));
        assert!(!proved(None, Start::LengthSub(1), Stay::AtLeast(-1), -1));
        assert!(!proved(None, Start::Length, to_zero, -1));
        assert!(!proved(Some(CmpOp::Le), Start::N, to_zero, -1));
    }
}

// ── Round 11 wave 9 (lane mid): adjacent offsets ─────────────────────

#[cfg(test)]
mod r11w9_mid_tests {
    use super::*;
    use crate::ir::{IrType, NO_NODE};
    use crate::ir_schedule;

    /// `for (i = 0; i <test> a.length - k; i++) a[i + j] = 1;` (`<` or, with
    /// `non_strict`, `<=`), the body on the true edge. Returns whether the
    /// store's bounds check is proven away by the range pass.
    fn offset_proved(k: i64, non_strict: bool, j: i64) -> bool {
        let mut g = Graph::empty(0);
        let s = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = s;
        let c0 = g.add(Op::Proj(0), IrType::Control, vec![s], None);
        let m0 = g.add(Op::Proj(1), IrType::Memory, vec![s], None);
        let arr = g.add(Op::Param(0), IrType::Ref, vec![s], None);
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let region = g.add(Op::Merge, IrType::Control, vec![c0], None);
        let iv = g.add(Op::Phi, IrType::Int, vec![region, zero], None);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let next = g.add(Op::Add, IrType::Int, vec![iv, one], None);
        g.nodes[iv as usize].inputs.push(next);
        let len = g.add(Op::ArrayLength, IrType::Int, vec![c0, m0, arr], None);
        let kc = g.add(Op::Const(k), IrType::Int, vec![], None);
        let x = g.add(Op::Sub, IrType::Int, vec![len, kc], None);
        let op = if non_strict { CmpOp::Le } else { CmpOp::Lt };
        let cmp = g.add(Op::Cmp(op), IrType::Int, vec![iv, x], None);
        let iff = g.add(Op::If, IrType::Control, vec![region, cmp], None);
        let body = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
        let exit = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
        g.nodes[region as usize].inputs.push(body);
        let jc = g.add(Op::Const(j), IrType::Int, vec![], None);
        let idx = g.add(Op::Add, IrType::Int, vec![iv, jc], None);
        let val = g.add(Op::Const(1), IrType::Int, vec![], None);
        let st = g.add(
            Op::ArrayStore(MemKind::Int),
            IrType::Void,
            vec![body, m0, arr, idx, val],
            Some(0),
        );
        g.exit = g.add(Op::Return, IrType::Void, vec![exit, iv], Some(1));
        let sched = ir_schedule::schedule(&g);
        let e = analyze(&g, &sched);
        e.bounds_elided(st) && e.bounds_range_proved(st)
    }

    /// `a[i + 1]` under `i < a.length - 1`, and every `j <= k` under
    /// `i < a.length - k`.
    #[test]
    fn an_offset_within_the_margin_is_in_bounds() {
        assert!(offset_proved(1, false, 1));
        assert!(offset_proved(1, false, 0));
        assert!(offset_proved(3, false, 2));
        assert!(offset_proved(3, false, 3));
    }

    /// `j > k` can reach `a[a.length]`: `i = len - k - 1` gives
    /// `i + k + 1 = len`.
    #[test]
    fn an_offset_past_the_margin_keeps_its_check() {
        assert!(!offset_proved(1, false, 2));
        assert!(!offset_proved(3, false, 4));
    }

    /// The non-strict test gives one less: `i <= a.length - k` covers
    /// `j <= k - 1`.
    #[test]
    fn a_non_strict_test_covers_one_offset_less() {
        assert!(offset_proved(2, true, 1));
        assert!(!offset_proved(2, true, 2));
        assert!(!offset_proved(1, true, 1));
    }

    /// A zero margin (`i < a.length - 0`) proves `i` itself, and nothing past.
    #[test]
    fn a_zero_margin_proves_no_offset() {
        assert!(!offset_proved(0, false, 1));
    }
}

/// Round 12 wave 3 (lane iropt2): the masked hashed-table index and the
/// stride-3 rule's cap.
#[cfg(test)]
mod r12w3_iropt2_mask_index_tests {
    use super::*;
    use crate::ir::{IrType, MemKind};
    use crate::ir_schedule;

    /// `if (a.length CMP c) return a[h & (L.length - k)]`, `L` being `a` or
    /// another array. Returns whether the load's check is range-proven away.
    fn mask_proved(guard: Option<(CmpOp, i64)>, k: i64, same_array: bool) -> bool {
        let mut g = Graph::empty(0);
        let s = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = s;
        let c0 = g.add(Op::Proj(0), IrType::Control, vec![s], None);
        let m0 = g.add(Op::Proj(1), IrType::Memory, vec![s], None);
        let arr = g.add(Op::Param(0), IrType::Ref, vec![s], None);
        let other = g.add(Op::Param(1), IrType::Ref, vec![s], None);
        let h = g.add(Op::Param(2), IrType::Int, vec![s], None);
        let len = g.add(Op::ArrayLength, IrType::Int, vec![c0, m0, arr], None);
        let body = match guard {
            Some((op, c)) => {
                let cc = g.add(Op::Const(c), IrType::Int, vec![], None);
                let cmp = g.add(Op::Cmp(op), IrType::Int, vec![len, cc], None);
                let iff = g.add(Op::If, IrType::Control, vec![c0, cmp], None);
                let t = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
                let _f = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
                t
            }
            None => c0,
        };
        let mask_len = if same_array {
            len
        } else {
            g.add(Op::ArrayLength, IrType::Int, vec![c0, m0, other], None)
        };
        let kc = g.add(Op::Const(k), IrType::Int, vec![], None);
        let m = g.add(Op::Sub, IrType::Int, vec![mask_len, kc], None);
        let idx = g.add(Op::And, IrType::Int, vec![h, m], None);
        let ld = g.add(
            Op::ArrayLoad(MemKind::Int),
            IrType::Int,
            vec![body, m0, arr, idx],
            Some(0),
        );
        g.exit = g.add(Op::Return, IrType::Void, vec![body, ld], Some(1));
        let sched = ir_schedule::schedule(&g);
        let e = analyze(&g, &sched);
        e.bounds_elided(ld) && e.bounds_range_proved(ld)
    }

    /// `HashMap.getNode`: `(n = tab.length) > 0 && tab[(n - 1) & hash]`.
    #[test]
    fn a_power_of_two_table_index_under_a_non_empty_test_is_in_bounds() {
        assert!(mask_proved(Some((CmpOp::Gt, 0)), 1, true));
        assert!(mask_proved(Some((CmpOp::Ge, 1)), 1, true));
        assert!(
            mask_proved(Some((CmpOp::Gt, 4)), 5, true),
            "any k >= 1 is below"
        );
        // ...but `len - 5 >= 0` needs `len >= 5`, not `len > 0`.
        assert!(!mask_proved(Some((CmpOp::Gt, 0)), 5, true));
    }

    /// The twins that must keep the check: an empty table makes the mask -1
    /// (no non-empty test), `len >= 0` says nothing, the mask of ANOTHER
    /// array's length, and `k == 0` (`h & len` reaches `len`).
    #[test]
    fn a_mask_the_proof_cannot_close_keeps_its_check() {
        assert!(!mask_proved(None, 1, true));
        assert!(!mask_proved(Some((CmpOp::Ge, 0)), 1, true));
        assert!(!mask_proved(Some((CmpOp::Gt, 0)), 1, false));
        assert!(!mask_proved(Some((CmpOp::Gt, 0)), 0, true));
    }

    #[test]
    fn the_mask_arm_has_a_kill_switch() {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_BCE_MASK_INDEX", Some("0"))],
            || assert!(!mask_proved(Some((CmpOp::Gt, 0)), 1, true)),
        );
    }
}

/// JIT round 19 wave 4 (lane irops3, IROPS2-7): the clamp-index arm.
#[cfg(test)]
mod r19w4_irops3_clamp_index_tests {
    use super::*;
    use crate::ir::{IrType, MemKind, ScalarOp};
    use crate::ir_schedule;

    /// What proves (or not) `a.length >= 1` before the access.
    #[derive(Clone, Copy, Debug)]
    enum Pre {
        Nothing,
        /// `a[j] = 1` in a block dominating the access.
        EarlierAccess,
        /// `if (a.length > 0)`, the access on its true edge.
        LengthPositive,
        /// `if ((char) j < a.length)`, the access on its true edge.
        IndexBelowLength,
        /// IROPS3-1 (lane osr5): `Guard(0 < a.length)` in a dominating block
        /// (the loop-entry clamp predicate's shape).
        LengthGuard,
        /// `Guard(0 <= a.length)`: proves nothing.
        WeakLengthGuard,
    }

    /// The index expression.
    #[derive(Clone, Copy, Debug)]
    enum Shape {
        /// `max(0, min(i, a.length - 1))`.
        MaxOfMin,
        /// `min(max(i, 0), a.length - 1)`.
        MinOfMax,
        /// `max(0, min(i, a.length))`: the non-strict bound proves nothing.
        NonStrict,
        /// `max(0, min(i, b.length - 1))`: another array's length.
        OtherArray,
        /// `min(i, a.length - 1)` with `i` unconstrained: may be negative.
        MinOnly,
    }

    fn min_of(g: &mut Graph, a: NodeId, b: NodeId) -> NodeId {
        g.add(
            Op::ScalarIntrinsic(ScalarOp::MinI),
            IrType::Int,
            vec![a, b],
            None,
        )
    }

    fn max_of(g: &mut Graph, a: NodeId, b: NodeId) -> NodeId {
        g.add(
            Op::ScalarIntrinsic(ScalarOp::MaxI),
            IrType::Int,
            vec![a, b],
            None,
        )
    }

    /// `[pre] a[<shape>] = 1`. Returns whether the store's check is
    /// range-proven away.
    fn clamp_proved(pre: Pre, shape: Shape) -> bool {
        let mut g = Graph::empty(0);
        let s = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = s;
        let c0 = g.add(Op::Proj(0), IrType::Control, vec![s], None);
        let m0 = g.add(Op::Proj(1), IrType::Memory, vec![s], None);
        let arr = g.add(Op::Param(0), IrType::Ref, vec![s], None);
        let i = g.add(Op::Param(1), IrType::Int, vec![s], None);
        let j = g.add(Op::Param(2), IrType::Int, vec![s], None);
        let other = g.add(Op::Param(3), IrType::Ref, vec![s], None);
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let len = g.add(Op::ArrayLength, IrType::Int, vec![c0, m0, arr], None);
        let branch = |g: &mut Graph, cmp: NodeId| {
            let iff = g.add(Op::If, IrType::Control, vec![c0, cmp], None);
            let t = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
            let _f = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
            t
        };
        let body = match pre {
            Pre::Nothing => c0,
            Pre::EarlierAccess => {
                let _first = g.add(
                    Op::ArrayStore(MemKind::Int),
                    IrType::Void,
                    vec![c0, m0, arr, j, one],
                    Some(0),
                );
                let cmp = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![i, zero], None);
                branch(&mut g, cmp)
            }
            Pre::LengthPositive => {
                let cmp = g.add(Op::Cmp(CmpOp::Gt), IrType::Int, vec![len, zero], None);
                branch(&mut g, cmp)
            }
            Pre::IndexBelowLength => {
                let jj = g.add(Op::I2C, IrType::Int, vec![j], None);
                let cmp = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![jj, len], None);
                branch(&mut g, cmp)
            }
            Pre::LengthGuard | Pre::WeakLengthGuard => {
                let op = if matches!(pre, Pre::LengthGuard) {
                    CmpOp::Lt
                } else {
                    CmpOp::Le
                };
                let pos = g.add(Op::Cmp(op), IrType::Int, vec![zero, len], None);
                let _guard = g.add(Op::Guard { bci: 0 }, IrType::Void, vec![c0, pos], Some(0));
                let cmp = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![i, zero], None);
                branch(&mut g, cmp)
            }
        };
        let len2 = g.add(Op::ArrayLength, IrType::Int, vec![body, m0, arr], None);
        let olen = g.add(Op::ArrayLength, IrType::Int, vec![body, m0, other], None);
        let last = g.add(Op::Sub, IrType::Int, vec![len2, one], None);
        let olast = g.add(Op::Sub, IrType::Int, vec![olen, one], None);
        let idx = match shape {
            Shape::MaxOfMin => {
                let m = min_of(&mut g, i, last);
                max_of(&mut g, zero, m)
            }
            Shape::MinOfMax => {
                let m = max_of(&mut g, i, zero);
                min_of(&mut g, m, last)
            }
            Shape::NonStrict => {
                let m = min_of(&mut g, i, len2);
                max_of(&mut g, zero, m)
            }
            Shape::OtherArray => {
                let m = min_of(&mut g, i, olast);
                max_of(&mut g, zero, m)
            }
            Shape::MinOnly => min_of(&mut g, i, last),
        };
        let st = g.add(
            Op::ArrayStore(MemKind::Int),
            IrType::Void,
            vec![body, m0, arr, idx, one],
            Some(1),
        );
        g.exit = g.add(Op::Return, IrType::Void, vec![body, i], Some(2));
        let sched = ir_schedule::schedule(&g);
        let e = analyze(&g, &sched);
        e.bounds_elided(st) && e.bounds_range_proved(st)
    }

    const ON: (&str, Option<&str>) = ("CRATONVM_JIT_IR_BCE_CLAMP_INDEX", Some("1"));

    /// Both clamp idioms are in range on an array known non-empty, by each of
    /// the three sources of `a.length >= 1`.
    #[test]
    fn a_clamped_index_into_a_non_empty_array_needs_no_check() {
        cratonvm_types::flags::with_thread_overrides(&[ON], || {
            for pre in [
                Pre::EarlierAccess,
                Pre::LengthPositive,
                Pre::IndexBelowLength,
            ] {
                for shape in [Shape::MaxOfMin, Shape::MinOfMax] {
                    assert!(clamp_proved(pre, shape), "{pre:?} {shape:?}");
                }
            }
        });
    }

    /// The case the book's reading missed: on an EMPTY array both idioms
    /// produce an out-of-range index (`0`, resp. `-1`), and the check is what
    /// throws. With no `a.length >= 1` fact the check stays.
    #[test]
    fn a_clamped_index_into_a_possibly_empty_array_keeps_its_check() {
        cratonvm_types::flags::with_thread_overrides(&[ON], || {
            for shape in [Shape::MaxOfMin, Shape::MinOfMax] {
                assert!(!clamp_proved(Pre::Nothing, shape), "{shape:?}");
            }
        });
    }

    /// The shapes that prove nothing whatever the length: a non-strict upper
    /// bound, another array's length, and a minimum with no lower clamp.
    #[test]
    fn a_shape_the_clamp_cannot_close_keeps_its_check() {
        cratonvm_types::flags::with_thread_overrides(&[ON], || {
            for shape in [Shape::NonStrict, Shape::OtherArray, Shape::MinOnly] {
                assert!(!clamp_proved(Pre::LengthPositive, shape), "{shape:?}");
                assert!(!clamp_proved(Pre::EarlierAccess, shape), "{shape:?}");
            }
        });
    }

    /// Default OFF: without the switch nothing changes.
    #[test]
    fn the_clamp_arm_is_opt_in() {
        for pre in [
            Pre::EarlierAccess,
            Pre::LengthPositive,
            Pre::IndexBelowLength,
        ] {
            for shape in [Shape::MaxOfMin, Shape::MinOfMax] {
                assert!(!clamp_proved(pre, shape), "{pre:?} {shape:?}");
            }
        }
    }

    const PREDICATE: (&str, Option<&str>) = ("CRATONVM_JIT_IR_BCE_CLAMP_PREDICATE", Some("1"));

    /// IROPS3-1 (lane osr5): the loop-entry `Guard(0 < a.length)` proves
    /// `a.length >= 1` in the blocks it dominates, under its own switch only;
    /// `Guard(0 <= a.length)` proves nothing.
    #[test]
    fn r19_osr5_a_dominating_length_guard_closes_the_first_clamped_access() {
        cratonvm_types::flags::with_thread_overrides(&[ON, PREDICATE], || {
            for shape in [Shape::MaxOfMin, Shape::MinOfMax] {
                assert!(clamp_proved(Pre::LengthGuard, shape), "{shape:?}");
                assert!(!clamp_proved(Pre::WeakLengthGuard, shape), "{shape:?}");
            }
            for shape in [Shape::NonStrict, Shape::OtherArray, Shape::MinOnly] {
                assert!(!clamp_proved(Pre::LengthGuard, shape), "{shape:?}");
            }
        });
        cratonvm_types::flags::with_thread_overrides(&[ON], || {
            for shape in [Shape::MaxOfMin, Shape::MinOfMax] {
                assert!(
                    !clamp_proved(Pre::LengthGuard, shape),
                    "switch off: {shape:?}"
                );
            }
        });
    }

    /// IROPS3-2: a clamp proof is marked as one (the emission census's
    /// source); another range proof is not.
    #[test]
    fn r19_osr5_clamp_proofs_are_told_apart() {
        cratonvm_types::flags::with_thread_overrides(&[ON], || {
            let mut g = Graph::empty(0);
            let s = g.add(Op::Start, IrType::Control, vec![], None);
            g.entry = s;
            let c0 = g.add(Op::Proj(0), IrType::Control, vec![s], None);
            let m0 = g.add(Op::Proj(1), IrType::Memory, vec![s], None);
            let arr = g.add(Op::Param(0), IrType::Ref, vec![s], None);
            let i = g.add(Op::Param(1), IrType::Int, vec![s], None);
            let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
            let one = g.add(Op::Const(1), IrType::Int, vec![], None);
            let len = g.add(Op::ArrayLength, IrType::Int, vec![c0, m0, arr], None);
            let cmp = g.add(Op::Cmp(CmpOp::Gt), IrType::Int, vec![len, zero], None);
            let iff = g.add(Op::If, IrType::Control, vec![c0, cmp], None);
            let t = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
            let _f = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
            let len2 = g.add(Op::ArrayLength, IrType::Int, vec![t, m0, arr], None);
            let last = g.add(Op::Sub, IrType::Int, vec![len2, one], None);
            let m = min_of(&mut g, i, last);
            let idx = max_of(&mut g, zero, m);
            let st = g.add(
                Op::ArrayStore(MemKind::Int),
                IrType::Void,
                vec![t, m0, arr, idx, one],
                Some(1),
            );
            g.exit = g.add(Op::Return, IrType::Void, vec![t, i], Some(2));
            let sched = ir_schedule::schedule(&g);
            let e = analyze(&g, &sched);
            assert!(e.bounds_elided(st) && e.bounds_range_proved(st));
            assert!(e.bounds_clamp_proved(st));
        });
    }

    /// The needs function itself, on shapes with a constant other than 0.
    #[test]
    fn the_needed_length_of_each_part() {
        let mut g = Graph::empty(0);
        let s = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = s;
        let c0 = g.add(Op::Proj(0), IrType::Control, vec![s], None);
        let m0 = g.add(Op::Proj(1), IrType::Memory, vec![s], None);
        let arr = g.add(Op::Param(0), IrType::Ref, vec![s], None);
        let i = g.add(Op::Param(1), IrType::Int, vec![s], None);
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let three = g.add(Op::Const(3), IrType::Int, vec![], None);
        let two = g.add(Op::Const(2), IrType::Int, vec![], None);
        let len = g.add(Op::ArrayLength, IrType::Int, vec![c0, m0, arr], None);
        let last2 = g.add(Op::Sub, IrType::Int, vec![len, two], None);
        let never = |_: NodeId| false;
        // `max(3, min(i, len - 2))`: below the length once `len >= 4`.
        let mn = min_of(&mut g, i, last2);
        let mx = max_of(&mut g, three, mn);
        assert_eq!(clamp_index_needs(&g, mx, arr, &never), Some(4));
        // `min(max(i, 3), len - 2)`: `len - 2 >= 0` needs `len >= 2`.
        let mx2 = max_of(&mut g, i, three);
        let mn2 = min_of(&mut g, mx2, last2);
        assert_eq!(clamp_index_needs(&g, mn2, arr, &never), Some(2));
        // `max(0, i)` has no upper half.
        let mx3 = max_of(&mut g, zero, i);
        assert_eq!(clamp_index_needs(&g, mx3, arr, &never), None);
        // A non-clamp root is not this arm's.
        assert_eq!(clamp_index_needs(&g, last2, arr, &never), None);
    }
}

/// nio20 wave 6 (lane L4): `a[p & m]` with a constant mask, under `c CMP
/// a.length` (an `If` edge, or the mask predicate's `Guard`), or over a fresh
/// array of constant length.
#[cfg(test)]
mod nio20w6_const_mask_tests {
    use super::*;
    use crate::ir::{IrType, MemKind};
    use crate::ir_schedule;

    #[derive(Clone, Copy)]
    enum Fact {
        None,
        /// `if (c OP a.length)`, the load on the true edge.
        Edge(CmpOp, i64),
        /// `Guard(c OP a.length)` in the entry block, the load one branch
        /// further down (a block the guard's strictly dominates).
        Guard(CmpOp, i64),
        /// The load reads a `new int[c]` instead of the parameter.
        Fresh(i64),
    }

    fn proved(fact: Fact, mask: i64) -> bool {
        let mut g = Graph::empty(0);
        let s = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = s;
        let c0 = g.add(Op::Proj(0), IrType::Control, vec![s], None);
        let m0 = g.add(Op::Proj(1), IrType::Memory, vec![s], None);
        let param = g.add(Op::Param(0), IrType::Ref, vec![s], None);
        let h = g.add(Op::Param(1), IrType::Int, vec![s], None);
        let sel = g.add(Op::Param(2), IrType::Int, vec![s], None);
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let mut arr = param;
        let body = match fact {
            Fact::None => c0,
            Fact::Edge(op, c) => {
                let len = g.add(Op::ArrayLength, IrType::Int, vec![c0, m0, param], None);
                let cc = g.add(Op::Const(c), IrType::Int, vec![], None);
                let cmp = g.add(Op::Cmp(op), IrType::Int, vec![cc, len], None);
                let iff = g.add(Op::If, IrType::Control, vec![c0, cmp], None);
                let t = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
                let _f = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
                t
            }
            Fact::Guard(op, c) => {
                let len = g.add(Op::ArrayLength, IrType::Int, vec![c0, m0, param], None);
                let cc = g.add(Op::Const(c), IrType::Int, vec![], None);
                let cmp = g.add(Op::Cmp(op), IrType::Int, vec![cc, len], None);
                let _ = g.add(Op::Guard { bci: 0 }, IrType::Void, vec![c0, cmp], Some(0));
                let ne = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![sel, zero], None);
                let iff = g.add(Op::If, IrType::Control, vec![c0, ne], None);
                let t = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
                let _f = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
                t
            }
            Fact::Fresh(c) => {
                let cc = g.add(Op::Const(c), IrType::Int, vec![], None);
                arr = g.add(
                    Op::NewArray {
                        element_type: 10,
                        component_class_id: 0,
                    },
                    IrType::Ref,
                    vec![c0, m0, cc],
                    None,
                );
                c0
            }
        };
        let mk = g.add(Op::Const(mask), IrType::Int, vec![], None);
        let idx = g.add(Op::And, IrType::Int, vec![h, mk], None);
        let ld = g.add(
            Op::ArrayLoad(MemKind::Int),
            IrType::Int,
            vec![body, m0, arr, idx],
            Some(0),
        );
        g.exit = g.add(Op::Return, IrType::Void, vec![body, ld], Some(1));
        let sched = ir_schedule::schedule(&g);
        let e = analyze(&g, &sched);
        e.bounds_elided(ld) && e.bounds_range_proved(ld)
    }

    #[test]
    fn const_mask_of_reads_either_operand_and_only_non_negative_masks() {
        let mut g = Graph::empty(0);
        let s = g.add(Op::Start, IrType::Control, vec![], None);
        let p = g.add(Op::Param(0), IrType::Int, vec![s], None);
        let seven = g.add(Op::Const(7), IrType::Int, vec![], None);
        let neg = g.add(Op::Const(-1), IrType::Int, vec![], None);
        let a = g.add(Op::And, IrType::Int, vec![p, seven], None);
        let b = g.add(Op::And, IrType::Int, vec![seven, p], None);
        let c = g.add(Op::And, IrType::Int, vec![p, neg], None);
        let d = g.add(Op::Or, IrType::Int, vec![p, seven], None);
        assert_eq!(const_mask_of(&g, a), Some(7));
        assert_eq!(const_mask_of(&g, b), Some(7));
        assert_eq!(const_mask_of(&g, c), None, "a negative mask bounds nothing");
        assert_eq!(const_mask_of(&g, d), None);
    }

    /// The mask predicate's guard `7 < a.length` proves `a[h & 7]` (and a
    /// smaller mask) one block down.
    #[test]
    fn the_mask_predicate_guard_proves_the_masked_access() {
        assert!(proved(Fact::Guard(CmpOp::Lt, 7), 7));
        assert!(proved(Fact::Guard(CmpOp::Lt, 7), 3));
        assert!(
            !proved(Fact::Guard(CmpOp::Lt, 7), 8),
            "a wider mask is not covered"
        );
        // `8 <= a.length` is `7 < a.length`.
        assert!(proved(Fact::Guard(CmpOp::Le, 8), 7));
        assert!(!proved(Fact::Guard(CmpOp::Le, 7), 7));
    }

    #[test]
    fn an_if_edge_on_a_constant_proves_the_masked_access() {
        assert!(proved(Fact::Edge(CmpOp::Lt, 15), 15));
        assert!(proved(Fact::Edge(CmpOp::Le, 16), 15));
        assert!(!proved(Fact::Edge(CmpOp::Le, 15), 15));
        assert!(!proved(Fact::None, 0));
    }

    #[test]
    fn a_fresh_array_of_constant_length_proves_a_smaller_mask() {
        assert!(proved(Fact::Fresh(8), 7));
        assert!(!proved(Fact::Fresh(7), 7));
    }

    #[test]
    fn the_const_mask_arm_has_a_kill_switch() {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_BCE_CONST_MASK", Some("0"))],
            || assert!(!proved(Fact::Guard(CmpOp::Lt, 7), 7)),
        );
    }
}

#[cfg(test)]
mod nio21w3_monotone_tests {
    //! The monotone-induction rule (nio21 wave 3, lane L5) on the shape of
    //! `UTF_8$Decoder.decodeArrayLoop`: a non-counted `while (sp < sl)` whose
    //! index advances by one in one arm and by `step` in another.
    use super::*;
    use crate::ir::IrType;
    use crate::ir_schedule;

    #[derive(Clone, Copy)]
    struct Shape {
        /// `Guard(sl <= sa.length)` in the pre-header (the upper half).
        length_guard: bool,
        /// `Guard(0 <= init)` in the pre-header.
        init_guard: bool,
        /// The multi-byte arm's `x - sp < k` bail test, if any.
        room: Option<i64>,
        /// The arm's `x` is a second bound `dl` instead of `sl`.
        room_on_other: bool,
        /// `Guard(0 <= dl)` in the pre-header.
        other_guard: bool,
        /// The multi-byte arm's step.
        step: i64,
        /// nio21 wave 4: an `sa[sp + j]` load on the multi arm past its room
        /// test (the decode loop's `sa[sp + 1..3]`).
        offset: Option<i64>,
    }

    const BASE: Shape = Shape {
        length_guard: true,
        init_guard: true,
        room: Some(2),
        room_on_other: false,
        other_guard: false,
        step: 2,
        offset: None,
    };

    /// ```java
    /// while (sp < sl) {
    ///     int b = sa[sp];
    ///     if (b >= 0) { sp++; }
    ///     else { if (x - sp < k) return; sp += step; }
    /// }
    /// ```
    /// Returns `(graph, schedule, the sa[sp] load, the sa[sp + j] load)`.
    fn decode_like(s: Shape) -> (Graph, Schedule, NodeId, Option<NodeId>) {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let sa = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let init = g.add(Op::Param(1), IrType::Int, vec![start], None);
        let sl = g.add(Op::Param(2), IrType::Int, vec![start], None);
        let dl = g.add(Op::Param(3), IrType::Int, vec![start], None);
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        if s.length_guard {
            let len = g.add(Op::ArrayLength, IrType::Int, vec![c0, m0, sa], None);
            let le = g.add(Op::Cmp(CmpOp::Le), IrType::Int, vec![sl, len], None);
            g.add(Op::Guard { bci: 0 }, IrType::Void, vec![c0, le], Some(0));
        }
        if s.init_guard {
            let le = g.add(Op::Cmp(CmpOp::Le), IrType::Int, vec![zero, init], None);
            g.add(Op::Guard { bci: 0 }, IrType::Void, vec![c0, le], Some(0));
        }
        if s.other_guard {
            let le = g.add(Op::Cmp(CmpOp::Le), IrType::Int, vec![zero, dl], None);
            g.add(Op::Guard { bci: 0 }, IrType::Void, vec![c0, le], Some(0));
        }
        let region = g.add(Op::Merge, IrType::Control, vec![c0], None);
        let sp = g.add(Op::Phi, IrType::Int, vec![region, init], None);
        // `if (sp >= sl) <exit>`, the builder's polarity: Proj(0) is the
        // branch target, the exit.
        let ge = g.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![sp, sl], None);
        let head = g.add(Op::If, IrType::Control, vec![region, ge], None);
        let exit = g.add(Op::Proj(0), IrType::Control, vec![head], None);
        let stay = g.add(Op::Proj(1), IrType::Control, vec![head], None);
        let load = g.add(
            Op::ArrayLoad(MemKind::Byte),
            IrType::Int,
            vec![stay, m0, sa, sp],
            Some(1),
        );
        let neg = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![load, zero], None);
        let arm = g.add(Op::If, IrType::Control, vec![stay, neg], None);
        let multi = g.add(Op::Proj(0), IrType::Control, vec![arm], None);
        let ascii = g.add(Op::Proj(1), IrType::Control, vec![arm], None);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let sp1 = g.add(Op::Add, IrType::Int, vec![sp, one], None);
        let step = g.add(Op::Const(s.step), IrType::Int, vec![], None);
        let spk = g.add(Op::Add, IrType::Int, vec![sp, step], None);
        let (multi_ok, bail) = match s.room {
            Some(k) => {
                let kk = g.add(Op::Const(k), IrType::Int, vec![], None);
                let x = if s.room_on_other { dl } else { sl };
                let rem = g.add(Op::Sub, IrType::Int, vec![x, sp], None);
                let short = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![rem, kk], None);
                let t = g.add(Op::If, IrType::Control, vec![multi, short], None);
                let bail = g.add(Op::Proj(0), IrType::Control, vec![t], None);
                let ok = g.add(Op::Proj(1), IrType::Control, vec![t], None);
                (ok, Some(bail))
            }
            None => (multi, None),
        };
        let off_load = s.offset.map(|j| {
            let jj = g.add(Op::Const(j), IrType::Int, vec![], None);
            let at = g.add(Op::Add, IrType::Int, vec![sp, jj], None);
            g.add(
                Op::ArrayLoad(MemKind::Byte),
                IrType::Int,
                vec![multi_ok, m0, sa, at],
                Some(3),
            )
        });
        let latch = g.add(Op::Merge, IrType::Control, vec![ascii, multi_ok], None);
        let next = g.add(Op::Phi, IrType::Int, vec![latch, sp1, spk], None);
        g.nodes[region as usize].inputs.push(latch);
        g.nodes[sp as usize].inputs.push(next);
        let out = match bail {
            Some(b) => g.add(Op::Merge, IrType::Control, vec![exit, b], None),
            None => exit,
        };
        g.exit = g.add(Op::Return, IrType::Void, vec![out], Some(2));
        let sched = ir_schedule::schedule(&g);
        (g, sched, load, off_load)
    }

    fn proved(s: Shape) -> bool {
        let (g, sched, load, _) = decode_like(s);
        let e = analyze(&g, &sched);
        e.bounds_elided(load) && e.bounds_range_proved(load)
    }

    /// nio21 wave 4: is the `sa[sp + j]` load proven?
    fn offset_proved(s: Shape) -> bool {
        let (g, sched, _, off) = decode_like(s);
        let off = off.expect("the shape has an offset load");
        let e = analyze(&g, &sched);
        e.bounds_elided(off) && e.bounds_range_proved(off)
    }

    #[test]
    fn nio21w4_an_offset_inside_the_room_test_is_proven() {
        // `sl - sp < 2` false: `sa[sp + 1]`.
        assert!(offset_proved(Shape {
            offset: Some(1),
            ..BASE
        }));
        // `sl - sp < 4` false: `sa[sp + 3]`.
        assert!(offset_proved(Shape {
            room: Some(4),
            step: 4,
            offset: Some(3),
            ..BASE
        }));
    }

    #[test]
    fn nio21w4_an_offset_at_the_room_keeps_its_check() {
        // `sl - sp >= 2` admits `sp + 2 == sl`.
        assert!(!offset_proved(Shape {
            offset: Some(2),
            ..BASE
        }));
    }

    #[test]
    fn nio21w4_an_offset_needs_the_length_guard() {
        assert!(!offset_proved(Shape {
            offset: Some(1),
            length_guard: false,
            ..BASE
        }));
    }

    #[test]
    fn nio21w4_an_offset_under_another_bound_needs_it_non_negative() {
        // `dl - sp >= 2` with no `0 <= dl` guard and no `dl <= sa.length`:
        // the length guard is on `sl`, so even `0 <= dl` does not prove it.
        let other = Shape {
            offset: Some(1),
            room_on_other: true,
            ..BASE
        };
        assert!(!offset_proved(other));
        assert!(!offset_proved(Shape {
            other_guard: true,
            ..other
        }));
    }

    #[test]
    fn nio21w4_the_room_offset_arm_has_a_kill_switch() {
        let s = Shape {
            offset: Some(1),
            ..BASE
        };
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_BCE_ROOM_OFFSET", Some("0"))],
            || assert!(!offset_proved(s)),
        );
        // The `sa[sp]` proof does not depend on it.
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_BCE_ROOM_OFFSET", Some("0"))],
            || assert!(proved(s)),
        );
    }

    #[test]
    fn a_decode_loop_index_is_proven_under_its_entry_guards() {
        assert!(proved(BASE));
    }

    #[test]
    fn a_wider_step_under_a_wide_enough_room_test_is_proven() {
        assert!(proved(Shape {
            room: Some(4),
            step: 4,
            ..BASE
        }));
    }

    #[test]
    fn an_unknown_start_keeps_the_check() {
        assert!(!proved(Shape {
            init_guard: false,
            ..BASE
        }));
    }

    #[test]
    fn a_multi_step_with_no_room_test_keeps_the_check() {
        assert!(!proved(Shape { room: None, ..BASE }));
    }

    #[test]
    fn a_step_wider_than_the_room_test_keeps_the_check() {
        assert!(!proved(Shape { step: 3, ..BASE }));
    }

    #[test]
    fn no_length_guard_keeps_the_check() {
        assert!(!proved(Shape {
            length_guard: false,
            ..BASE
        }));
    }

    #[test]
    fn a_room_test_on_another_bound_needs_that_bound_non_negative() {
        let other = Shape {
            room_on_other: true,
            ..BASE
        };
        assert!(!proved(other));
        assert!(proved(Shape {
            other_guard: true,
            ..other
        }));
    }

    #[test]
    fn the_monotone_arm_has_a_kill_switch() {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_BCE_MONOTONE", Some("0"))],
            || assert!(!proved(BASE)),
        );
    }

    #[test]
    fn constant_step_reads_a_chain_of_non_negative_constants() {
        let mut g = Graph::empty(0);
        let p = g.add(Op::Param(0), IrType::Int, vec![], None);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let minus = g.add(Op::Const(-1), IrType::Int, vec![], None);
        let a = g.add(Op::Add, IrType::Int, vec![p, one], None);
        let b = g.add(Op::Add, IrType::Int, vec![one, a], None);
        let c = g.add(Op::Add, IrType::Int, vec![p, minus], None);
        assert_eq!(constant_step(&g, b), Some((p, 2)));
        assert_eq!(constant_step(&g, c), None);
        assert_eq!(constant_step(&g, p), None);
    }
}

/// nio21 wave 6, lane L2: the dominance facts stop at an exception edge
/// (`exception_edge_fences`).
#[cfg(test)]
mod nio21w6_edge_fence_tests {
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
}
