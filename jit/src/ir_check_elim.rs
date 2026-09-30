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
    let RangeFacts {
        bounds: ranges,
        non_negative_at,
        offsets,
    } = if range_on {
        upper_bounds(graph, schedule)
    } else {
        RangeFacts {
            bounds: Vec::new(),
            non_negative_at: FxHashMap::default(),
            offsets: FxHashMap::default(),
        }
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
    let node_ranges = if range_on && (!ranges.is_empty() || !offsets.is_empty() || masked_any) {
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
                // Once per block: the walk visits one block at a time, so a
                // repeat is always the last entry, and pushing it again only
                // lengthened every later dominance scan (M accesses to one
                // array in one block made the pass O(M²)).
                let bs = deref_blocks.entry(base).or_default();
                if bs.last() != Some(&b) {
                    bs.push(b);
                }
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
                let bs = deref_blocks.entry(x).or_default();
                if bs.last() != Some(&b) {
                    bs.push(b);
                }
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
                                                i,
                                                f.true_block,
                                                b,
                                                max_stride,
                                            )
                                    });
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
                                m,
                                b,
                                b,
                                max_stride,
                            );
                        }
                    }
                    if proved {
                        out.bounds_check_unneeded[id as usize] = true;
                        out.bounds_by_range[id as usize] = true;
                    } else {
                        out.bounds_refusal[id as usize] = reason;
                    }
                }
                local_checked.insert(pair);
                let bs = checked_blocks.entry(pair).or_default();
                if bs.last() != Some(&b) {
                    bs.push(b);
                }
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
    // The strict `x < base.length` facts, and the blocks where a value is
    // known `>= 0` from an `If` edge (wave 8; see the doc above).
    let mut below: FxHashMap<LenKey, Vec<AtMostLength>> = FxHashMap::default();
    let mut non_negative_at: FxHashMap<NodeId, Vec<usize>> = FxHashMap::default();
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
                if let Some((x, base)) = at_most_length(graph, cond, true) {
                    at_most
                        .entry(len_key(graph, x))
                        .or_default()
                        .push(AtMostLength {
                            base,
                            block,
                            after_block_start: true,
                        });
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
        Op::Phi => is_unit_stride_induction(graph, schedule, v, node, guard_block, max_stride),
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
static ELIDED: [std::sync::atomic::AtomicU64; 2] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];
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

/// Record one guard-pair decision. `which` is 0 for the null check, 1 for the
/// bounds check.
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
            assert!(e.bounds_elided(st) && e.bounds_range_proved(st), "stride {s}");
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
        let guard = g.add(Op::Guard { bci: 0 }, IrType::Void, vec![ctrl, cond], Some(0));
        let load = g.add(
            Op::ArrayLoad(MemKind::Int),
            IrType::Int,
            vec![ctrl, mem, arr, idx],
            Some(1),
        );
        g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, load], Some(2));

        assert_eq!(guard_non_null_operand(&g, &g.nodes[guard as usize]), Some(arr));
        assert_eq!(guard_non_null_operand(&g, &g.nodes[load as usize]), None);

        let sched = ir_schedule::schedule(&g);
        let e = analyze(&g, &sched);
        assert!(e.null_elided(load), "the guard already rejected a null `arr`");
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
        assert!(mask_proved(Some((CmpOp::Gt, 4)), 5, true), "any k >= 1 is below");
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
