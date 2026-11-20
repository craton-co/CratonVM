// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT round 20 wave 18 (lane xSplitIf, LOOPOPT20-1): split-if for a branch
//! on a φ of constants (`CRATONVM_JIT_IR_SPLIT_IF_CONST_PHI`, default ON,
//! `=0` is the kill switch).
//!
//! # Why
//!
//! `boolean hasRemaining() { return position < limit; }` is
//! `if_icmpge L; iconst_1; goto E; L: iconst_0; E: ireturn`. Spliced into
//! `while (buf.hasRemaining())` (or `while (it.hasNext())`), the caller's
//! `ifeq` branches on the join `E`'s value φ `phi(E; 1, 0)`. Both arms of the
//! callee's own test then reach a back edge through `E`, so the loop body has
//! no `If` edge stating `position < limit`: `mask_predicate::header_stay_edge`
//! answers `None` (the mask and monotone predicates refuse), `ir_check_elim`'s
//! edge facts and the range lattice miss the bound, and the unroller's
//! `region -> if -> {back, exit}` shape fails
//! (`docs/feature-designs/deopt-osr.md`).
//! HotSpot's split-if routes each predecessor of `E` straight to the successor
//! its constant selects.
//!
//! # What
//!
//! For an `If` at merge `M` whose condition is `P` or `Cmp(cc, P, k)` /
//! `Cmp(cc, k, P)` (`k` an `int` constant), `P` an `int` φ at `M` whose every
//! value is an `int` constant: each predecessor `c_j` of `M` is routed to the
//! projection `P = k_j` selects -- the projection's users are rewired to
//! `c_j` (or to a new, pc-less `Merge` of the predecessors that select it) --
//! and `M`, `P`, the compare, the `If` and both projections are killed.
//!
//! # When it refuses (the graph is left as it was)
//!
//! * the compare has any user but the `If`, `P` any user but the condition,
//!   or `M` any user but `P` and the `If` (another φ -- a memory φ included --
//!   or a node anchored at the join would need a copy per side);
//! * a predecessor of `M` has a user other than `M` (an arm that computes or
//!   anchors something; also keeps `ir_schedule`'s in-block id order out of
//!   the question), or appears twice;
//! * one side would receive no predecessor (the branch is decided; that is
//!   [`super::prune_constant_ifs`]'s job once the cleanup folds the φ);
//! * `M` is the target of a retreating control edge (a loop header, reducible
//!   or not), or the graph has no usable `Start` to tell;
//! * a safepoint snapshot naming `P` or the compare can still be consulted
//!   ([`super::pure_body_frames_unconsultable`]: a surviving node carries its
//!   bci, directly or through the splice resume map, claims it by
//!   `frame_snapshot`, or it is a post-call snapshot). The caller's flat
//!   snapshot at the `ifeq` bci names `P` on its stack, and so does the chain
//!   snapshot at the callee's `ireturn`; once the compare, the `If` and its
//!   projections (at the `ifeq` bci) and `M` and `P` (at the join's bci) are
//!   gone, nothing reaches them.
//!
//! # Why it is sound
//!
//! Control that reaches `M` through `c_j` reaches it with `P = k_j`, so the
//! `If` takes the projection `k_j` selects on every such execution: the
//! rewire only removes the join and the test, and every node after the
//! projection sees the same values (nothing at `M` but `P`, and `P` is read
//! by the condition alone) and the same memory token (no memory φ at `M`).
//! The loop structure is unchanged: `M` is no header, every header keeps its
//! inputs (a projection that was a back edge or a loop entry is replaced by
//! one control node, a predecessor or the new `Merge`), so each loop keeps one
//! header and its back edges. The frames that named `P` are unconsultable (the
//! check above); their slots are set to `NO_NODE` here, and the flat ones'
//! bcis join `UNROLL_STRANDED_BCIS`, the net `ir_lower` checks against the
//! bcis its emission transfers at (a disagreement refuses the compile). The
//! new `Merge` carries no bytecode pc, so it never sets the lowerer's carried
//! `cur_bci` to the killed join's bci.
//!
//! # Through a memory φ, with non-empty arms (JIT round 25 wave 2; default ON since round 26, `=0` kills)
//!
//! Under `CRATONVM_JIT_IR_SPLIT_IF_MEMORY_PHI` two refusals above are lifted.
//! The case is a spliced `return a && b` callee with a store on one return
//! path (`Pattern$BmpCharProperty.match`'s `matcher.hitEnd = true; return
//! false`): its return join carries a memory φ besides the boolean, and the
//! storing arm anchors the store.
//!
//! * A MEMORY φ `Q` at `M` is split with the join: each side gets `Q`'s value
//!   on its one predecessor, or a new memory φ at that side's new `Merge`.
//!   Every use of `Q` must lie on one side -- a node anchored at control one
//!   projection dominates, or a φ slot whose edge one projection dominates --
//!   and is pointed at that side's value. A use neither side dominates (after
//!   the two sides join again) refuses. Memory is in no frame state, so no
//!   frame changes.
//! * A predecessor with users besides `M` (an arm that anchors or computes
//!   something; never another control successor) is routed through a new
//!   single-input `Merge` of its own side, so what the arm anchors stays in
//!   the arm's block and the successor's nodes stay in theirs.
//!
//! The rest of the soundness argument is unchanged: control reaching `M`
//! through `c_j` takes the projection `k_j` selects, and every node past the
//! projection now reads, for `Q`, the value `Q` had on the edge it came in by.
//!
//! # A partly constant φ, a condition chain (JIT round 26, RB25-2 / RB25-4, default ON)
//!
//! Under `CRATONVM_JIT_IR_SPLIT_IF_PARTIAL_PHI`:
//!
//! * The condition may be a CHAIN over `P` of up to three compares and `And`s,
//!   each with an `int` constant operand and each the only user of the link
//!   below it: `If(Cmp(Eq, And(P, 1), 0))` is the caller's `ifeq` on a
//!   spliced `boolean` callee's masked `ireturn`. A constant is decided by
//!   evaluating the chain on it, exactly as the lowering would (compares on
//!   the low 32 bits, masks in `int`).
//! * A φ value that is NOT a constant no longer refuses: its predecessor
//!   `c_j` gets its own pc-less copy `If_j(c_j, chain[P := v_j])`, whose
//!   `Proj(0)` / `Proj(1)` join the true / false side. This is jump threading
//!   for the constant edges (they skip the materialised `0`/`1`, the φ copy
//!   and the test) and, for an edge whose value is a compare, a branch on that
//!   compare where the value is made (no `setcc`, no edge copy). A split is
//!   made only when one edge is constant or one copy's value is a compare.
//! * No copy is made when a projection enters a loop header: a pc-less branch
//!   there would put its back-edge poll at whatever bci was lowered before it.
//!
//! Soundness: on an edge `c_j` the `If` at `M` would have tested `chain(v_j)`;
//! `If_j` tests exactly that, at the end of `c_j`'s block, and leads to the
//! same side. The copies are pure (`Cmp`, `And`, `If`, `Proj`), carry no pc
//! and cannot deopt, so no frame is consulted at them; the frames naming `P`
//! or a link of the chain are held to the unconsultable rule as before.
//!
//! # Through a value φ (JIT round 26, RB25-3, default ON)
//!
//! Under `CRATONVM_JIT_IR_SPLIT_IF_VALUE_PHI` (with the memory-φ extension), a
//! VALUE φ `R` at `M` besides `P` is split like a memory φ: each side gets
//! `R`'s value on its one edge, or a new φ at its new `Merge`. Every use of
//! `R` must lie on one side -- anchored on control one projection dominates,
//! or a φ slot whose edge one projection dominates; a floating pure user has
//! no side and refuses. `R` is killed, so a frame naming it must be
//! unconsultable too (it joins the named set of the frame rule).

use super::{
    build_users, control_not_dominated_by, pure_body_frames_unconsultable, retreating_edge_targets,
    FxHashMap, FxHashSet, UNROLL_STRANDED_BCIS,
};
use crate::ir::{CmpOp, Graph, IrType, NodeId, Op, NO_NODE};

/// `CRATONVM_JIT_IR_SPLIT_IF_CONST_PHI` (JIT round 20 wave 18, lane xSplitIf,
/// LOOPOPT20-1) -- **default ON**, `=0` is the kill switch. Read once per run
/// of the pass.
pub(super) fn split_if_const_phi_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_SPLIT_IF_MEMORY_PHI` (JIT round 25 wave 2; default ON since round 26, `=0` kills): the
/// split-if also through memory φs at the join and with non-empty arms (see
/// the module notes). Read once per plan.
pub(super) fn split_if_memory_phi_enabled() -> bool {
    // R26-FLIP (default ON, `=0` kills).
    true
}

/// What one run of [`split_const_phi_ifs`] did, for the census line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct SplitIfCensus {
    /// `If`s whose condition reads a constant φ at their own merge.
    pub candidates: usize,
    /// Split.
    pub split: usize,
    /// Refused because a frame naming the φ or the compare stays reachable.
    pub refused_frames: usize,
    /// Of `split`, those that needed `CRATONVM_JIT_IR_SPLIT_IF_MEMORY_PHI`
    /// (a memory φ at the join or a non-empty arm).
    pub through_memory_phis: usize,
    /// Of `split`, those with a predecessor that got its own copy of the `If`
    /// (`CRATONVM_JIT_IR_SPLIT_IF_PARTIAL_PHI`).
    pub partial: usize,
    /// Of `split`, those through a value φ (`CRATONVM_JIT_IR_SPLIT_IF_VALUE_PHI`).
    pub through_value_phis: usize,
    /// Shape refusals by the source line of the refusing check.
    pub shape_refusals: std::collections::BTreeMap<u32, usize>,
}

/// One incoming edge of a side's new join: predecessor `j` of `merge`,
/// routed whole (`Direct`, its constant decides the side) or through one
/// projection of its own copy of the `If` (`Copy`, under
/// `CRATONVM_JIT_IR_SPLIT_IF_PARTIAL_PHI`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Edge {
    Direct(usize),
    Copy(usize),
}

impl Edge {
    fn pred_idx(self) -> usize {
        match self {
            Edge::Direct(j) | Edge::Copy(j) => j,
        }
    }
}

/// The rewrite of one `If`, decided before any mutation.
struct SplitIfPlan {
    merge: NodeId,
    phi: NodeId,
    /// The condition's nodes above the φ, outermost (the `If`'s condition)
    /// first: empty for `If(P)`, `[Cmp(cc, P, k)]` for the original shape, and
    /// under `CRATONVM_JIT_IR_SPLIT_IF_PARTIAL_PHI` a chain of compares and
    /// masks with `int` constants (`If(Cmp(Eq, And(P, 1), 0))`).
    chain: Vec<NodeId>,
    iff: NodeId,
    /// `Proj(0)` (true) and `Proj(1)` (false).
    projs: [NodeId; 2],
    /// The predecessors of `merge`, in their order there.
    preds: Vec<NodeId>,
    /// The φ's value on each predecessor.
    values: Vec<NodeId>,
    /// The edges each side's new join receives, in order.
    edges: [Vec<Edge>; 2],
    /// The predecessors that get their own copy of the `If`.
    copies: Vec<usize>,
    /// A side with a predecessor that has users besides `merge`: it gets a
    /// new `Merge` even for one predecessor.
    busy: [bool; 2],
    /// The memory φs at `merge` (`CRATONVM_JIT_IR_SPLIT_IF_MEMORY_PHI`) and
    /// the value φs besides `phi` (`CRATONVM_JIT_IR_SPLIT_IF_VALUE_PHI`).
    side_phis: Vec<NodeId>,
    /// Each use of one of `side_phis` (or of `phi`, with `phi_side`):
    /// `(user, input slot, where it reads, the φ)`.
    phi_slots: Vec<(NodeId, usize, Loc, NodeId)>,
    /// Each rejoin a slot reads at, with the side of each of its input edges.
    rejoin_sides: Vec<(NodeId, Vec<usize>)>,
    /// How many of `side_phis` are value φs.
    value_phis: usize,
    /// The condition's φ has users besides the chain (`CRATONVM_JIT_IR_SPLIT_IF_VALUE_PHI`):
    /// it is split per side like a value φ, its slots in `phi_slots`.
    phi_side: bool,
    /// The flat snapshot bcis left naming a killed value (`UNROLL_STRANDED_BCIS`).
    stranded: Vec<usize>,
}

/// Where a use of a φ at the split join reads: on one side, or at a merge
/// past the split where the two sides join again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Loc {
    Side(usize),
    Rejoin(NodeId),
}

/// Merges past the split considered as rejoins, at most.
const REJOIN_CANDIDATES: usize = 4;
/// How deep [`plan_split`]'s floating-user walk follows pure nodes.
const DATA_DEPTH: u32 = 4;

/// Why [`plan_split`] declined.
enum Refusal {
    /// The source line of the refusing check (the `CRATONVM_DBG_LICM`
    /// census names it).
    Shape(u32),
    Frames,
}

fn cmp_holds(cc: CmpOp, a: i32, b: i32) -> bool {
    match cc {
        CmpOp::Eq => a == b,
        CmpOp::Ne => a != b,
        CmpOp::Lt => a < b,
        CmpOp::Le => a <= b,
        CmpOp::Gt => a > b,
        CmpOp::Ge => a >= b,
    }
}

fn int_const(graph: &Graph, id: NodeId) -> Option<i64> {
    let n = graph.node_opt(id)?;
    match n.op {
        Op::Const(v) if n.ty == IrType::Int => Some(v),
        _ => None,
    }
}

/// `CRATONVM_JIT_IR_SPLIT_IF_PARTIAL_PHI` (RB25-2 / RB25-4; default ON, `=0` kills): the
/// split-if also when only SOME of the φ's values are constants, and through
/// a condition chain of compares and masks. A predecessor whose value is not
/// a constant gets its own copy of the `If`, on the condition rebuilt over
/// its value (see the module notes).
pub(super) fn split_if_partial_phi_enabled() -> bool {
    // R26-FLIP (default ON, `=0` kills).
    true
}

/// `CRATONVM_JIT_IR_SPLIT_IF_VALUE_PHI` (RB25-3; default ON, `=0` kills): the split-if also
/// through value φs at the join, each split per side like a memory φ.
pub(super) fn split_if_value_phi_enabled() -> bool {
    // R26-FLIP (default ON, `=0` kills).
    true
}

/// How deep a condition chain may be under `CRATONVM_JIT_IR_SPLIT_IF_PARTIAL_PHI`.
const CHAIN_DEPTH: usize = 3;

/// The other operand of the binary `int` node `n`, one of whose two inputs
/// is `x`, when that other operand is an `int` constant: `(k, x is the left
/// operand)`.
fn const_operand(graph: &Graph, n: &crate::ir::Node, x: NodeId) -> Option<(i64, bool)> {
    if n.inputs.len() != 2 {
        return None;
    }
    let (a, b) = (n.input_opt(0)?, n.input_opt(1)?);
    if a == x && b != x {
        Some((int_const(graph, b)?, true))
    } else if b == x && a != x {
        Some((int_const(graph, a)?, false))
    } else {
        None
    }
}

/// The condition chain of `If` `iff`: `(merge, phi, chain)`, the chain
/// outermost first. Without `deep` only the original shapes: `If(P)` and
/// `If(Cmp(cc, P, k))` / `If(Cmp(cc, k, P))`. With it, up to
/// [`CHAIN_DEPTH`] compares and `And`s, each with an `int` constant operand.
fn condition_chain(
    graph: &Graph,
    iff: NodeId,
    deep: bool,
) -> Option<(NodeId, NodeId, Vec<NodeId>)> {
    let n = graph.node_opt(iff)?;
    if n.op != Op::If || n.inputs.len() != 2 {
        return None;
    }
    let merge = n.input_opt(0)?;
    let cond = n.input_opt(1)?;
    if graph.node_opt(merge)?.op != Op::Merge {
        return None;
    }
    let is_phi_here = |id: NodeId| {
        graph.node_opt(id).is_some_and(|p| {
            p.op == Op::Phi && p.ty == IrType::Int && p.input_opt(0) == Some(merge)
        })
    };
    let mut chain: Vec<NodeId> = Vec::new();
    let mut cur = cond;
    let depth = if deep { CHAIN_DEPTH } else { 1 };
    loop {
        if is_phi_here(cur) {
            return Some((merge, cur, chain));
        }
        if chain.len() >= depth {
            return None;
        }
        let cn = graph.node_opt(cur)?;
        if cn.ty != IrType::Int || cn.inputs.len() != 2 {
            return None;
        }
        match cn.op {
            Op::Cmp(_) => {}
            Op::And if deep => {}
            _ => return None,
        }
        // The non-constant operand is the next link.
        let (a, b) = (cn.input_opt(0)?, cn.input_opt(1)?);
        let next = match (int_const(graph, a), int_const(graph, b)) {
            (None, Some(_)) => a,
            (Some(_), None) => b,
            _ => return None,
        };
        chain.push(cur);
        cur = next;
    }
}

/// Evaluate `chain` (outermost first) on the constant `v` at its bottom, as
/// the lowering would: compares on the low 32 bits, masks in `int`.
fn eval_chain(graph: &Graph, chain: &[NodeId], phi: NodeId, v: i64) -> Option<i64> {
    let mut x = v;
    let mut below = phi;
    for &c in chain.iter().rev() {
        let n = graph.node_opt(c)?;
        let (k, left) = const_operand(graph, n, below)?;
        x = match n.op {
            Op::Cmp(cc) => {
                let (a, b) = if left {
                    (x as i32, k as i32)
                } else {
                    (k as i32, x as i32)
                };
                i64::from(cmp_holds(cc, a, b))
            }
            Op::And => i64::from((x as i32) & (k as i32)),
            _ => return None,
        };
        below = c;    }
    Some(x)}

/// The projection index (`0` true, `1` false) the φ value `v` selects.
fn side_of_value(graph: &Graph, chain: &[NodeId], phi: NodeId, v: i64) -> Option<usize> {
    let truth = (eval_chain(graph, chain, phi, v)? as i32) != 0;
    Some(if truth { 0 } else { 1 })
}

/// The condition shape of `If` `iff` for the candidate scan.
fn condition_shape(graph: &Graph, iff: NodeId) -> Option<(NodeId, NodeId, Vec<NodeId>)> {
    condition_chain(graph, iff, split_if_partial_phi_enabled())
}

/// RB25-4 (`CRATONVM_JIT_IR_SPLIT_IF_PARTIAL_PHI`): an `If` at a `Merge`
/// whose condition is NOT made at the join -- no φ there feeds it, and none
/// of its inputs (followed through pure nodes) is a φ or a node anchored at
/// the join -- so it has the same value on every predecessor, and a
/// predecessor whose dominating edges decide it can go straight to its side.
/// `Some(merge)` for such an `If`.
fn phi_less_shape(graph: &Graph, iff: NodeId) -> Option<NodeId> {
    let n = graph.node_opt(iff)?;
    if n.op != Op::If || n.inputs.len() != 2 {
        return None;
    }
    let merge = n.input_opt(0)?;
    let cond = n.input_opt(1)?;
    let mn = graph.node_opt(merge)?;
    if mn.op != Op::Merge || mn.inputs.len() < 2 {
        return None;
    }
    let cn = graph.node_opt(cond)?;
    if cn.ty != IrType::Int
        || matches!(
            cn.op,
            Op::Const(_)
                | Op::OsrEntryFlag { .. }
                | Op::CatchEntryFlag { .. }
                | Op::CallThrew { .. }
        )
    {
        return None;
    }
    // Not made at the join: a bounded walk of its pure inputs.
    let mut work = vec![(cond, 0u32)];
    let mut seen: FxHashSet<NodeId> = FxHashSet::default();
    while let Some((x, d)) = work.pop() {
        if !seen.insert(x) {
            continue;
        }
        let xn = graph.node_opt(x)?;
        if xn.op == Op::Phi && xn.input_opt(0) == Some(merge) {
            return None;
        }
        if xn.inputs.first() == Some(&merge) {
            return None;
        }
        let pure = matches!(
            xn.op,
            Op::Cmp(_)
                | Op::Add
                | Op::Sub
                | Op::And
                | Op::Or
                | Op::Xor
                | Op::Shl
                | Op::Shr
                | Op::UShr
        );
        if pure {
            if d >= 6 {
                return None;
            }
            for &i in xn.inputs.iter() {
                work.push((i, d + 1));
            }
        }
    }
    Some(merge)
}

/// The live users of `id`, each as often as it names `id`.
fn live_users(graph: &Graph, users: &[Vec<NodeId>], id: NodeId) -> Vec<NodeId> {
    users
        .get(id as usize)
        .map(|u| {
            u.iter()
                .copied()
                .filter(|&x| graph.node_opt(x).is_some_and(|n| n.op != Op::Dead))
                .collect()
        })
        .unwrap_or_default()
}

/// Decide the split of `iff` (see the module notes), or refuse.
fn plan_split(
    graph: &Graph,
    users: &[Vec<NodeId>],
    retreating: &FxHashSet<NodeId>,
    iff: NodeId,
) -> Result<SplitIfPlan, Refusal> {
    let partial = split_if_partial_phi_enabled();
    // The φ-less form (RB25-4) has `phi == NO_NODE` and an empty chain.
    let (merge, phi, chain) = match condition_shape(graph, iff) {
        Some(shape) => shape,
        None if partial => (
            phi_less_shape(graph, iff).ok_or(Refusal::Shape(line!()))?,
            NO_NODE,
            Vec::new(),
        ),
        None => return Err(Refusal::Shape(line!())),
    };
    let phi_less = phi == NO_NODE;
    let mn = graph.node_opt(merge).ok_or(Refusal::Shape(line!()))?;
    let preds: Vec<NodeId> = mn.inputs.iter().copied().collect();
    if preds.len() < 2 || preds.iter().any(|&c| c == NO_NODE) {
        return Err(Refusal::Shape(line!()));
    }
    // Each value: a constant decides its side; anything else needs a copy of
    // the `If` (`CRATONVM_JIT_IR_SPLIT_IF_PARTIAL_PHI`). Without a φ, every
    // predecessor carries the condition itself, decided where its dominating
    // edges decide it (`bool_branch::implied_by_dominating_edge`).
    let mut values: Vec<NodeId> = Vec::with_capacity(preds.len());
    let mut decided: Vec<Option<usize>> = Vec::with_capacity(preds.len());
    if phi_less {
        let cond = graph
            .node_opt(iff)
            .and_then(|n| n.input_opt(1))
            .ok_or(Refusal::Shape(line!()))?;
        for &c in &preds {
            values.push(cond);
            decided.push(
                super::bool_branch::implied_by_dominating_edge(graph, c, cond).map(|t| {
                    if t {
                        0
                    } else {
                        1
                    }
                }),
            );
        }
        if decided.iter().all(Option::is_none) {
            return Err(Refusal::Shape(line!()));
        }
    } else {
        let pn = graph.node_opt(phi).ok_or(Refusal::Shape(line!()))?;
        if pn.inputs.len() != preds.len() + 1 {
            return Err(Refusal::Shape(line!()));
        }
        for v in pn.phi_value_inputs() {
            let v = v.ok_or(Refusal::Shape(line!()))?;
            values.push(v);
            match int_const(graph, v) {
                Some(k) => decided.push(Some(
                    side_of_value(graph, &chain, phi, k).ok_or(Refusal::Shape(line!()))?,
                )),
                None if partial => decided.push(None),
                None => return Err(Refusal::Shape(line!())),
            }
        }
    }
    if retreating.contains(&merge) {
        return Err(Refusal::Shape(line!()));
    }
    // A copy is worth its `If` only when something decides or fuses: at
    // least one edge is constant, or a copy's value is a compare its own
    // `If` then branches on.
    let copies: Vec<usize> = (0..preds.len()).filter(|&j| decided[j].is_none()).collect();
    if !copies.is_empty() && !phi_less {
        let any_const = decided.iter().any(Option::is_some);
        let any_cmp = copies.iter().any(|&j| {
            graph
                .node_opt(values[j])
                .is_some_and(|n| matches!(n.op, Op::Cmp(_)) && n.ty == IrType::Int)
        });
        if !any_const && !any_cmp {
            return Err(Refusal::Shape(line!()));
        }
    }

    // Each link of the chain feeds the next alone (the outermost the `If`),
    // and the φ feeds the chain's bottom (or the `If`) alone.
    let mut above = iff;
    // (No chain without a φ: the condition stays, read by the copies.)
    for &c in &chain {
        if live_users(graph, users, c) != [above] {
            return Err(Refusal::Shape(line!()));
        }
        above = c;
    }
    // Under `CRATONVM_JIT_IR_SPLIT_IF_VALUE_PHI` the φ may have other users
    // too (`boolean r = ..; if (r) ..; return r;`): it is then split per side
    // like any value φ at the join, below.
    let phi_side = if phi_less {
        false
    } else {
        let mut phi_users = live_users(graph, users, phi);
        let at = phi_users
            .iter()
            .position(|&u| u == above)
            .ok_or(Refusal::Shape(line!()))?;
        phi_users.remove(at);
        !phi_users.is_empty()
    };
    if phi_side && !(split_if_memory_phi_enabled() && split_if_value_phi_enabled()) {
        return Err(Refusal::Shape(line!()));
    }
    // Nothing at the join but the φ and the `If` -- and, under
    // `CRATONVM_JIT_IR_SPLIT_IF_MEMORY_PHI`, memory φs; under
    // `CRATONVM_JIT_IR_SPLIT_IF_VALUE_PHI` as well, value φs.
    let extended = split_if_memory_phi_enabled();
    let value_split = extended && split_if_value_phi_enabled();
    let mut mem_phis: Vec<NodeId> = Vec::new();
    let mut val_phis: Vec<NodeId> = Vec::new();
    let mut mu = live_users(graph, users, merge);
    mu.sort_unstable();
    let mut want = if phi_less { vec![iff] } else { vec![phi, iff] };
    want.sort_unstable();
    if mu != want {
        if !extended {
            return Err(Refusal::Shape(line!()));
        }
        let mut rest = mu.clone();
        for &x in &want {
            let pos = rest
                .iter()
                .position(|&u| u == x)
                .ok_or(Refusal::Shape(line!()))?;
            rest.remove(pos);
        }
        for u in rest {
            let un = graph.node_opt(u).ok_or(Refusal::Shape(line!()))?;
            if un.op != Op::Phi
                || un.input_opt(0) != Some(merge)
                || un.inputs.len() != preds.len() + 1
                || un.phi_value_inputs().any(|v| v.is_none())
                || mem_phis.contains(&u)
                || val_phis.contains(&u)
            {
                return Err(Refusal::Shape(line!()));
            }
            if un.ty == IrType::Memory {
                mem_phis.push(u);
            } else if value_split
                && matches!(
                    un.ty,
                    IrType::Int | IrType::Long | IrType::Ref | IrType::Float | IrType::Double
                )
            {
                val_phis.push(u);
            } else {
                return Err(Refusal::Shape(line!()));
            }
        }
    }
    // Exactly the two control projections.
    let mut projs = [NO_NODE; 2];
    let iu = live_users(graph, users, iff);
    if iu.len() != 2 {
        return Err(Refusal::Shape(line!()));
    }
    for u in iu {
        let un = graph.node_opt(u).ok_or(Refusal::Shape(line!()))?;
        match un.op {
            Op::Proj(w) if un.ty == IrType::Control && w <= 1 && projs[w as usize] == NO_NODE => {
                projs[w as usize] = u;
            }
            _ => return Err(Refusal::Shape(line!())),
        }
    }
    if projs.contains(&NO_NODE) {
        return Err(Refusal::Shape(line!()));
    }
    // A copy of the `If` carries no bytecode pc (a pc-less node never sets
    // the lowerer's carried bci); a pc-less branch into a loop header would
    // put its back-edge poll at whatever bci came before it. So no copy when
    // a projection enters a loop header.
    if !copies.is_empty() {
        for &p in &projs {
            if live_users(graph, users, p)
                .iter()
                .any(|u| retreating.contains(u))
            {
                return Err(Refusal::Shape(line!()));
            }
        }
    }
    // Empty arms: each predecessor is a control node naming the join once and
    // used by nothing else (under `CRATONVM_JIT_IR_SPLIT_IF_MEMORY_PHI`, or
    // anchoring data nodes, which then keep a block of their own).
    let mut seen: FxHashSet<NodeId> = FxHashSet::default();
    let mut edges: [Vec<Edge>; 2] = [Vec::new(), Vec::new()];
    let mut busy = [false; 2];
    for (j, &c) in preds.iter().enumerate() {
        if !seen.insert(c) || c == iff || projs.contains(&c) {
            return Err(Refusal::Shape(line!()));
        }
        let cn = graph.node_opt(c).ok_or(Refusal::Shape(line!()))?;
        if cn.ty != IrType::Control || !matches!(cn.op, Op::Proj(_) | Op::Merge | Op::Region) {
            return Err(Refusal::Shape(line!()));
        }
        let cu = live_users(graph, users, c);
        let arm_busy = cu != [merge];
        if arm_busy {
            // A non-empty arm (`CRATONVM_JIT_IR_SPLIT_IF_MEMORY_PHI`): it names
            // the join once, has no other control successor, and is no arm
            // the lowering enters by a jump.
            if !extended || cu.iter().filter(|&&u| u == merge).count() != 1 {
                return Err(Refusal::Shape(line!()));
            }
            for &u in &cu {
                if u == merge {
                    continue;
                }
                let un = graph.node_opt(u).ok_or(Refusal::Shape(line!()))?;
                if un.ty == IrType::Control || un.op.is_control() {
                    return Err(Refusal::Shape(line!()));
                }
            }
        }
        // An arm the lowering enters by a jump past its `If`: a busy one would
        // move what it anchors, and a copy would hang a second `If` under the
        // arm's entry.
        if arm_busy || decided[j].is_none() {
            if let Op::Proj(_) = cn.op {
                let flag = cn
                    .input_opt(0)
                    .and_then(|i| graph.node_opt(i))
                    .filter(|i| i.op == Op::If)
                    .and_then(|i| i.input_opt(1))
                    .and_then(|f| graph.node_opt(f));
                if flag.is_some_and(|f| {
                    matches!(
                        f.op,
                        Op::OsrEntryFlag { .. } | Op::CatchEntryFlag { .. } | Op::CallThrew { .. }
                    )
                }) {
                    return Err(Refusal::Shape(line!()));
                }
            }
        }
        match decided[j] {
            Some(side) => {
                if arm_busy {
                    busy[side] = true;
                }
                edges[side].push(Edge::Direct(j));
            }
            None => {
                edges[0].push(Edge::Copy(j));
                edges[1].push(Edge::Copy(j));
            }
        }
    }
    if edges.iter().any(Vec::is_empty) {
        return Err(Refusal::Shape(line!()));
    }

    // Each use of a φ at the join, by the side that dominates it.
    let mut side_phis: Vec<NodeId> = mem_phis.clone();
    side_phis.extend(val_phis.iter().copied());
    let mut phi_slots: Vec<(NodeId, usize, Loc, NodeId)> = Vec::new();
    let mut rejoin_sides: Vec<(NodeId, Vec<usize>)> = Vec::new();
    if !side_phis.is_empty() || phi_side {
        let reach = control_not_dominated_by(graph, users, NO_NODE);
        let not_dominated = [
            control_not_dominated_by(graph, users, projs[0]),
            control_not_dominated_by(graph, users, projs[1]),
        ];
        let side_of = |ctrl: NodeId| -> Option<usize> {
            if ctrl == NO_NODE || !reach.contains(&ctrl) {
                return None;
            }
            match (
                not_dominated[0].contains(&ctrl),
                not_dominated[1].contains(&ctrl),
            ) {
                (false, true) => Some(0),
                (true, false) => Some(1),
                _ => None,
            }
        };
        // JIT round 26: a merge past the split whose every input edge lies on
        // one side (the two sides joining again, as `if (r) ..; return r;`
        // does): a use there reads a new φ of the sides' values.
        let mut rejoins: Vec<(NodeId, FxHashSet<NodeId>)> = Vec::new();
        if split_if_value_phi_enabled() {
            for (id, n) in graph.nodes.iter().enumerate() {
                let id = id as NodeId;
                if n.op != Op::Merge || id == merge || n.inputs.len() < 2 || !reach.contains(&id) {
                    continue;
                }
                let sides: Vec<Option<usize>> = n.inputs.iter().map(|&c| side_of(c)).collect();
                if sides.iter().any(Option::is_none)
                    || !sides.contains(&Some(0))
                    || !sides.contains(&Some(1))
                {
                    continue;
                }
                rejoins.push((id, control_not_dominated_by(graph, users, id)));
                if rejoins.len() >= REJOIN_CANDIDATES {
                    break;
                }
            }
        }
        let loc_of = |ctrl: NodeId| -> Option<Loc> {
            if let Some(side) = side_of(ctrl) {
                return Some(Loc::Side(side));
            }
            if ctrl == NO_NODE || !reach.contains(&ctrl) {
                return None;
            }
            rejoins
                .iter()
                .find(|(_, not_dom)| !not_dom.contains(&ctrl))
                .map(|&(j, _)| Loc::Rejoin(j))
        };
        // Where a FLOATING pure node is used: the one location every use of
        // it lies at (recursively through other floating nodes), or `None`.
        // A frame naming it would be a use at a point this cannot see.
        fn data_loc(
            graph: &Graph,
            users: &[Vec<NodeId>],
            x: NodeId,
            depth: u32,
            loc_of: &dyn Fn(NodeId) -> Option<Loc>,
        ) -> Option<Loc> {
            if depth == 0 {
                return None;
            }
            let named = graph.safepoints.iter().any(|sp| {
                sp.locals
                    .iter()
                    .chain(sp.stack.iter())
                    .chain(sp.monitors.iter())
                    .any(|&v| v == x)
            });
            if named {
                return None;
            }
            let mut out: Option<Loc> = None;
            let mut xu = live_users(graph, users, x);
            xu.sort_unstable();
            xu.dedup();
            if xu.is_empty() {
                return None;
            }
            for u in xu {
                let un = graph.node_opt(u)?;
                let mut here: Vec<Loc> = Vec::new();
                if un.op == Op::Phi {
                    let jn = graph.node_opt(un.input_opt(0)?)?;
                    for (k, &v) in un.inputs.iter().enumerate() {
                        if k > 0 && v == x {
                            here.push(loc_of(*jn.inputs.get(k - 1)?)?);
                        }
                    }
                } else if let Some(a) = un.input_opt(0).filter(|&a| {
                    a != x && graph.node_opt(a).is_some_and(|n| n.ty == IrType::Control)
                }) {
                    here.push(loc_of(a)?);
                } else if !un.op.is_control() && un.ty != IrType::Memory {
                    here.push(data_loc(graph, users, u, depth - 1, loc_of)?);
                } else {
                    return None;
                }
                for l in here {
                    match out {
                        None => out = Some(l),
                        Some(o) if o == l => {}
                        Some(_) => return None,
                    }
                }
            }
            out
        }
        let mut split_phis = side_phis.clone();
        if phi_side {
            split_phis.push(phi);
        }
        for &q in &split_phis {
            let mut qu = live_users(graph, users, q);
            qu.sort_unstable();
            qu.dedup();
            for u in qu {
                // The condition's own read of the φ goes with the chain.
                if q == phi && u == above {
                    continue;
                }
                let un = graph.node_opt(u).ok_or(Refusal::Shape(line!()))?;
                // A φ reads `q` on an edge; an anchored node where it is
                // anchored; a floating pure node (a value φ's only) where all
                // its uses are (`data_loc`).
                let is_value = q == phi || val_phis.contains(&q);
                let anchor = if un.op == Op::Phi {
                    None
                } else {
                    let a = un.input_opt(0).ok_or(Refusal::Shape(line!()))?;
                    if graph.node_opt(a).ok_or(Refusal::Shape(line!()))?.ty == IrType::Control {
                        Some(a)
                    } else if is_value && !un.op.is_control() && un.ty != IrType::Memory {
                        None
                    } else {
                        return Err(Refusal::Shape(line!()));
                    }
                };
                let floating = un.op != Op::Phi && anchor.is_none();
                let floating_loc = if floating {
                    let l = data_loc(graph, users, u, DATA_DEPTH, &loc_of);
                    if l.is_none() && super::loop_pass_report_enabled() {
                        eprintln!(
                            "[ir] split-if n{iff}: phi n{q}'s floating user n{u} has no one location ({})",
                            graph.scope_method_key
                        );
                    }
                    Some(l.ok_or(Refusal::Shape(line!()))?)
                } else {
                    None
                };
                for (k, &x) in un.inputs.iter().enumerate() {
                    if x != q {
                        continue;
                    }
                    if k == 0 && !floating {
                        return Err(Refusal::Shape(line!()));
                    }
                    let loc = match (floating_loc, un.op == Op::Phi) {
                        (Some(l), _) => l,
                        (None, true) => {
                            let jn = un.input_opt(0).ok_or(Refusal::Shape(line!()))?;
                            let edge = graph
                                .node_opt(jn)
                                .and_then(|jnode| jnode.inputs.get(k - 1).copied())
                                .ok_or(Refusal::Shape(line!()))?;
                            let l = loc_of(edge);
                            if l.is_none() && super::loop_pass_report_enabled() {
                                eprintln!(
                                    "[ir] split-if n{iff}: phi n{q}'s phi user n{u} reads on edge n{edge} with no location ({})",
                                    graph.scope_method_key
                                );
                            }
                            l.ok_or(Refusal::Shape(line!()))?
                        }
                        (None, false) => loc_of(anchor.ok_or(Refusal::Shape(line!()))?)
                            .ok_or(Refusal::Shape(line!()))?,
                    };
                    phi_slots.push((u, k, loc, q));
                }
            }
        }
        // The side of every input edge of each rejoin a slot reads at.
        for &(_, _, loc, _) in &phi_slots {
            if let Loc::Rejoin(j) = loc {
                if rejoin_sides.iter().any(|(r, _)| *r == j) {
                    continue;
                }
                let jn = graph.node_opt(j).ok_or(Refusal::Shape(line!()))?;
                let mut sides = Vec::with_capacity(jn.inputs.len());
                for &c in jn.inputs.iter() {
                    sides.push(side_of(c).ok_or(Refusal::Shape(line!()))?);
                }
                rejoin_sides.push((j, sides));
            }
        }
    }

    // Frames: nothing may still consult a snapshot naming a killed value.
    let mut killed: FxHashSet<NodeId> = [merge, iff, projs[0], projs[1]].into_iter().collect();
    if !phi_less {
        killed.insert(phi);
    }
    killed.extend(side_phis.iter().copied());
    killed.extend(chain.iter().copied());
    let mut named: FxHashSet<NodeId> = FxHashSet::default();
    if !phi_less {
        named.insert(phi);
    }
    named.extend(chain.iter().copied());
    // A value φ is killed and no one node stands for it on both sides, so a
    // frame naming it (a later local) must be unconsultable too.
    named.extend(val_phis.iter().copied());
    let none: FxHashSet<NodeId> = FxHashSet::default();
    if !pure_body_frames_unconsultable(graph, &killed, &named, &none) {
        return Err(Refusal::Frames);
    }
    let stranded: Vec<usize> = graph
        .safepoints
        .iter()
        .filter(|sp| {
            !sp.is_splice_state()
                && sp
                    .locals
                    .iter()
                    .chain(sp.stack.iter())
                    .chain(sp.monitors.iter())
                    .any(|&s| s != NO_NODE && named.contains(&s))
        })
        .map(|sp| sp.bci)
        .collect();
    Ok(SplitIfPlan {
        merge,
        phi,
        chain,
        iff,
        projs,
        preds,
        values,
        edges,
        copies,
        busy,
        value_phis: val_phis.len(),
        phi_side,
        side_phis,
        phi_slots,
        rejoin_sides,
        stranded,
    })
}

/// Rebuild `chain` (outermost first) over `v` in place of `phi`: every link
/// is a new pc-less node with the same op and constant. Returns the new
/// outermost node (the copy's condition), or `v` for an empty chain.
fn copy_chain(graph: &mut Graph, chain: &[NodeId], phi: NodeId, v: NodeId) -> NodeId {
    let mut below_old = phi;
    let mut below_new = v;
    for &c in chain.iter().rev() {
        let Some(n) = graph.node_opt(c) else {
            return below_new;
        };
        let (op, ty) = (n.op.clone(), n.ty);
        let inputs: Vec<NodeId> = n
            .inputs
            .iter()
            .map(|&x| if x == below_old { below_new } else { x })
            .collect();
        let id = graph.add(op, ty, inputs, None);
        below_old = c;
        below_new = id;
    }
    below_new
}

/// Apply a [`plan_split`] answer. Every check was made there.
fn apply_split(graph: &mut Graph, plan: SplitIfPlan) {
    // The copies: `If_j(c_j, chain[v_j])` and its two projections.
    let mut copy_projs: FxHashMap<usize, [NodeId; 2]> = FxHashMap::default();
    for &j in &plan.copies {
        let (c, v) = (plan.preds[j], plan.values[j]);
        let cond = copy_chain(graph, &plan.chain, plan.phi, v);
        let iff = graph.add(Op::If, IrType::Control, vec![c, cond], None);
        let t = graph.add(Op::Proj(0), IrType::Control, vec![iff], None);
        let f = graph.add(Op::Proj(1), IrType::Control, vec![iff], None);
        copy_projs.insert(j, [t, f]);
    }
    let edge_ctrl = |e: Edge, s: usize| -> NodeId {
        match e {
            Edge::Direct(j) => plan.preds[j],
            Edge::Copy(j) => copy_projs.get(&j).map(|p| p[s]).unwrap_or(NO_NODE),
        }
    };
    let mut targets = [NO_NODE; 2];
    for (s, target) in targets.iter_mut().enumerate() {
        let ins: Vec<NodeId> = plan.edges[s].iter().map(|&e| edge_ctrl(e, s)).collect();
        *target = match (ins.as_slice(), plan.edges[s].as_slice()) {
            ([only], [Edge::Direct(_)]) if !plan.busy[s] => *only,
            ([only], [Edge::Copy(_)]) => *only,
            _ => graph.add(Op::Merge, IrType::Control, ins, None),
        };
    }
    // Each φ's value on each side: its one edge's value, or a new φ at the
    // side's new `Merge` over that side's edges, in the same order.
    let mut side_values: Vec<(NodeId, [NodeId; 2])> = Vec::with_capacity(plan.side_phis.len() + 1);
    let mut split_phis = plan.side_phis.clone();
    if plan.phi_side {
        split_phis.push(plan.phi);
    }
    for &q in &split_phis {
        let Some(qn) = graph.node_opt(q) else {
            continue;
        };
        let ty = qn.ty;
        let ins: Vec<NodeId> = qn.inputs.iter().copied().collect();
        let mut vals = [NO_NODE; 2];
        for (s, val) in vals.iter_mut().enumerate() {
            let idx: Vec<usize> = plan.edges[s].iter().map(|e| e.pred_idx()).collect();
            *val = match idx.as_slice() {
                [j] => ins.get(j + 1).copied().unwrap_or(NO_NODE),
                _ => {
                    let mut inputs = Vec::with_capacity(idx.len() + 1);
                    inputs.push(targets[s]);
                    inputs.extend(
                        idx.iter()
                            .map(|&j| ins.get(j + 1).copied().unwrap_or(NO_NODE)),
                    );
                    graph.add(Op::Phi, ty, inputs, None)
                }
            };
        }
        side_values.push((q, vals));
    }
    // A rejoin's φ of each split φ, made once per (rejoin, φ).
    let mut rejoin_values: Vec<((NodeId, NodeId), NodeId)> = Vec::new();
    for &(user, slot, loc, q) in &plan.phi_slots {
        let Some((_, vals)) = side_values.iter().find(|(m, _)| *m == q).copied() else {
            continue;
        };
        let value = match loc {
            Loc::Side(side) => vals[side],
            Loc::Rejoin(j) => match rejoin_values.iter().find(|(k, _)| *k == (j, q)) {
                Some(&(_, v)) => v,
                None => {
                    let Some((_, sides)) = plan.rejoin_sides.iter().find(|(r, _)| *r == j) else {
                        continue;
                    };
                    let ty = graph.node_opt(q).map(|n| n.ty).unwrap_or(IrType::Int);
                    let mut inputs = Vec::with_capacity(sides.len() + 1);
                    inputs.push(j);
                    inputs.extend(sides.iter().map(|&s| vals[s]));
                    let v = graph.add(Op::Phi, ty, inputs, None);
                    rejoin_values.push(((j, q), v));
                    v
                }
            },
        };
        graph.set_input(user, slot, value);
    }
    for (proj, target) in plan.projs.iter().zip(targets.iter()) {
        graph.replace_all_uses(*proj, *target);
    }
    graph.kill(plan.projs[0]);
    graph.kill(plan.projs[1]);
    graph.kill(plan.iff);
    for &c in &plan.chain {
        graph.kill(c);
    }
    // The only names left are snapshot slots, none of them reachable.
    if plan.phi != NO_NODE {
        graph.replace_all_uses(plan.phi, NO_NODE);
        graph.kill(plan.phi);
    }
    // Every use was pointed at a side's value above; a value φ's snapshot
    // slots are unreachable (`plan_split`'s frame check).
    for (i, &q) in plan.side_phis.iter().enumerate() {
        if i >= plan.side_phis.len() - plan.value_phis {
            graph.replace_all_uses(q, NO_NODE);
        }
        graph.kill(q);
    }
    graph.kill(plan.merge);
    if !plan.stranded.is_empty() {
        UNROLL_STRANDED_BCIS.with(|c| c.borrow_mut().flat.extend(plan.stranded));
    }
}

/// Split every `If` on a constant φ the module notes admit. See there.
pub(super) fn split_const_phi_ifs(graph: &mut Graph) -> SplitIfCensus {
    let mut census = SplitIfCensus::default();
    if !split_if_const_phi_enabled() {
        return census;
    }
    let candidates: Vec<NodeId> = {
        let g: &Graph = graph;
        (0..g.nodes.len() as NodeId)
            .filter(|&id| {
                condition_shape(g, id).is_some()
                    || (split_if_partial_phi_enabled() && phi_less_shape(g, id).is_some())
            })
            .collect()
    };
    census.candidates = candidates.len();
    if candidates.is_empty() {
        return census;
    }
    let mut state: Option<(Vec<Vec<NodeId>>, FxHashSet<NodeId>)> = None;
    for iff in candidates {
        if state.is_none() {
            let users = build_users(graph);
            let Some(retreating) = retreating_edge_targets(graph, &users) else {
                return census;
            };
            state = Some((users, retreating));
        }
        let Some((users, retreating)) = state.as_ref() else {
            return census;
        };
        match plan_split(graph, users, retreating, iff) {
            Ok(plan) => {
                if plan.side_phis.len() > plan.value_phis || plan.busy.contains(&true) {
                    census.through_memory_phis += 1;
                }
                if !plan.copies.is_empty() {
                    census.partial += 1;
                }
                if plan.value_phis > 0 || plan.phi_side {
                    census.through_value_phis += 1;
                }
                apply_split(graph, plan);
                census.split += 1;
                state = None;
            }
            Err(Refusal::Frames) => census.refused_frames += 1,
            Err(Refusal::Shape(line)) => *census.shape_refusals.entry(line).or_default() += 1,
        }
    }
    census
}


