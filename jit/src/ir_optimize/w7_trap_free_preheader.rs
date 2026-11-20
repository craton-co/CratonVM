// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Wave 7b (lane ir-opt-inline): a LATE trap-free pre-header for a loop that
//! only reads (`CRATONVM_JIT_IR_TRAP_FREE_PREHEADER`, default ON).
//!
//! # Why
//!
//! `for (i = 0; i < n; i++) x += s.charAt(i & 63)` over an invariant String
//! `s` keeps, after every earlier loop pass, `Guard(coder == 0)`,
//! `Guard(value != null)`, `ArrayLength(value)` and `Guard((i & 63) <
//! value.length)` inside the loop, in both modes: the real-JDK splice of
//! `String.charAt` -> `StringLatin1.charAt` -> `Preconditions.checkIndex`, and
//! the `--compatible` decode. The length predicates (`plant_length_predicates`)
//! run right after the loop rounds, BEFORE the late passes (the class
//! predicate, the field forward, the `charAt` read hoist) make `s.coder` and
//! `s.value` pre-header values, so those guards became invariant only after
//! every planter had looked; and no planter takes a constant-masked bounds
//! GUARD (`mask_predicate` plants for an `a[x & m]` ACCESS, not for the
//! `checkIndex` guard in front of it). Each trapping node left in the body is a
//! deopt point that pins its frame's values (the dead UTF-16 index math
//! `((i & 63) << 1) + 1` among them), and keeps the body off the main/post
//! unroller's pure route (`unroll_wide_pure`, `strip_mine`).
//!
//! # What
//!
//! For each single-entry loop whose body only reads (no store, call,
//! allocation, monitor or volatile access), on a guard the loop runs every
//! iteration (its control dominates every back edge):
//!
//! * `Guard(a op b)`, `a` and `b` loop-invariant: one `Guard{header}(a op b)`
//!   in the pre-header ([`crate::deopt::PredicateKind::Length`], the length
//!   predicates' kind for the same move);
//! * then `Guard(x & m < len)` (or `len > x & m`), `m` a constant in
//!   `0..=i32::MAX`, `len` the `ArrayLength` of an invariant array `a`
//!   (possibly `len >> s` with `s` zero or proven zero by a pre-header
//!   `Guard(s == 0)`, the UTF-16-aware `value.length >> coder`): the
//!   `mask_predicate` guard `Guard{header}(m < a.length)` in the pre-header
//!   ([`crate::deopt::PredicateKind::Mask`]), which `ir_check_elim` and
//!   `mask_predicate::mask_guard_proves_access` already read;
//!
//! then, from the facts the pre-header guards state, an in-body `If` or
//! `Guard` on the same compare folds (`If(coder == 0)`: the UTF-16 arm is
//! pruned by `prune_constant_ifs`), and an in-body `ArrayLength(a)` of an array
//! the pre-header already reads becomes that read.
//!
//! # Why it is sound
//!
//! Every guard planted here deopts to the LOOP ENTRY with the loop-entry frame
//! (`loop_entry_frame`: the header snapshot with each header φ at its entry
//! value), exactly the discipline of `install_range_predicates`: nothing of the
//! loop has run, the interpreter runs the loop itself and raises what the
//! bytecode raises where it raises it. A guard's condition is computed from
//! values defined before the loop (an invariant compare) or is a stronger
//! statement than every per-iteration one (`x & m <= m < a.length`), so a
//! passing pre-header guard implies every in-body one, and a failing one costs
//! a deopt, never a wrong result. A header whose predicate keeps failing is
//! withdrawn by the de-spec registry (`predicate_withdrawn` for the planter's
//! kind), after which the in-body guards stay. A shift proven zero is proven by
//! a guard ALREADY in the pre-header when the bounds guard is decided. The
//! pre-header dominates the single-entry body, so a fact it proves holds at
//! every in-body node; an in-body guard is only ever folded to TRUE (a guard
//! folded to false would be a site trap). An array's length never changes, so
//! an in-body length read of an array the pre-header read is the same value.
//!
//! Not on the optimizing OSR route or an OSR-door graph unless every entry of
//! the loop passes its pre-header (`loop_entered_only_through_its_pre_header`,
//! the late invariant hoist's rule).

use super::{
    back_edges_dominated_by, build_users, is_loop_invariant, loop_body_with,
    loop_entered_only_through_its_pre_header, loop_entry_frame, loop_entry_memory, loop_headers,
    orphaned_predicate_frames, spliced_loop_entered_only_through_its_pre_header,
    tag_predicate_guards, FxHashSet,
};
use crate::ir::{CmpOp, Graph, IrType, Node, NodeId, Op, SafepointSnapshot, NO_NODE};

/// `CRATONVM_JIT_IR_TRAP_FREE_PREHEADER` (wave 7b, lane ir-opt-inline; perf,
/// **default ON**, `=0` kills). Read once per optimize; no `static`.
pub(super) fn trap_free_preheader_enabled() -> bool {
    true
}

/// At most this many guards one loop's pre-header takes from its body.
const MAX_HOISTS_PER_LOOP: usize = 24;

/// What one run of [`plant_trap_free_preheaders`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct PreheaderCensus {
    /// In-body invariant guards moved to a pre-header.
    pub(super) invariant: usize,
    /// In-body constant-masked bounds guards answered by a pre-header mask
    /// guard (planted here or already there).
    pub(super) masked: usize,
    /// In-body `If` / `Guard` conditions folded from a pre-header fact.
    pub(super) folded: usize,
    /// In-body `ArrayLength`s replaced by the pre-header's read.
    pub(super) lengths: usize,
}

impl PreheaderCensus {
    /// Did the run change the graph?
    pub(super) fn changed(&self) -> bool {
        self.invariant + self.masked + self.folded + self.lengths > 0
    }

    fn add(&mut self, other: PreheaderCensus) {
        self.invariant += other.invariant;
        self.masked += other.masked;
        self.folded += other.folded;
        self.lengths += other.lengths;
    }
}

/// Does `body` only read: no node that writes memory, calls, allocates, locks
/// or is a volatile access? Guards, loads, element and length reads, the
/// `--compatible` `charAt` decode and fences are reads.
pub(super) fn body_only_reads(graph: &Graph, body: &FxHashSet<NodeId>) -> bool {
    body.iter().all(|&id| {
        let Some(n) = graph.node_opt(id) else {
            return true;
        };
        if n.is_volatile_access() {
            return false;
        }
        match &n.op {
            Op::Call { .. }
            | Op::IndyBridge { .. }
            | Op::Store(_)
            | Op::ArrayStore(_)
            | Op::StoreStatic { .. }
            | Op::MonitorEnter
            | Op::MonitorExit
            | Op::New { .. }
            | Op::NewArray { .. }
            | Op::NewCp { .. }
            | Op::MultiNewArray { .. } => false,
            Op::Phi
            | Op::Dead
            | Op::OsrMemory
            | Op::Guard { .. }
            | Op::Load(_)
            | Op::ArrayLoad(_)
            | Op::ArrayLength
            | Op::StrCharDecode
            | Op::KeepAlive
            | Op::ClassInitTest { .. } => true,
            op => {
                op.is_control()
                    || (!crate::ir::memory_shape_of(op).may_write()
                        && !super::licm_hoist_barrier(op))
            }
        }
    })
}

/// The two inputs of a `Cmp`, with its operator.
fn cmp_parts(n: &Node) -> Option<(CmpOp, NodeId, NodeId)> {
    match (&n.op, n.inputs.as_slice()) {
        (Op::Cmp(op), &[a, b]) => Some((*op, a, b)),
        _ => None,
    }
}

/// Is `id` the `int` constant 0?
fn is_int_zero(graph: &Graph, id: NodeId) -> bool {
    graph
        .node_opt(id)
        .is_some_and(|n| n.ty == IrType::Int && n.op == Op::Const(0))
}

/// The array `len` is the length of: `ArrayLength(a)`, or `ArrayLength(a) >>
/// s` / `>>> s` with `s` the constant 0 or a node in `zero` (proven 0 at the
/// loop entry).
fn length_base(graph: &Graph, len: NodeId, zero: &FxHashSet<NodeId>) -> Option<NodeId> {
    let n = graph.node_opt(len)?;
    match &n.op {
        Op::ArrayLength => n.input_opt(2),
        Op::Shr | Op::UShr => {
            let s = n.input_opt(1)?;
            if !zero.contains(&s) && !is_int_zero(graph, s) {
                return None;
            }
            let inner = graph.node_opt(n.input_opt(0)?)?;
            (inner.op == Op::ArrayLength)
                .then(|| inner.input_opt(2))
                .flatten()
        }
        _ => None,
    }
}

/// The length read under `len` (itself, or the shifted one).
fn length_read(graph: &Graph, len: NodeId) -> Option<&Node> {
    let n = graph.node_opt(len)?;
    match &n.op {
        Op::ArrayLength => Some(n),
        Op::Shr | Op::UShr => graph
            .node_opt(n.input_opt(0)?)
            .filter(|inner| inner.op == Op::ArrayLength),
        _ => None,
    }
}

/// `(m, len)` when `cond` is `x & m < len` or `len > x & m`
/// (`ir_check_elim::const_mask_of`).
fn masked_bound(graph: &Graph, cond: &Node) -> Option<(i64, NodeId)> {
    let (idx, len) = match cmp_parts(cond)? {
        (CmpOp::Lt, idx, len) => (idx, len),
        (CmpOp::Gt, len, idx) => (idx, len),
        _ => return None,
    };
    let m = crate::ir_check_elim::const_mask_of(graph, idx)?;
    (0..=i64::from(i32::MAX)).contains(&m).then_some((m, len))
}

/// The value `v == 0` / `0 == v` proves zero.
fn proves_zero(graph: &Graph, cond: &Node) -> Option<NodeId> {
    let (CmpOp::Eq, a, b) = cmp_parts(cond)? else {
        return None;
    };
    match (is_int_zero(graph, a), is_int_zero(graph, b)) {
        (false, true) => Some(a),
        (true, false) => Some(b),
        _ => None,
    }
}

/// The live guards anchored on `entry_pred` with their condition nodes.
fn entry_guards(graph: &Graph, entry_pred: NodeId) -> Vec<(NodeId, &Node)> {
    graph
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| matches!(n.op, Op::Guard { .. }) && n.input_opt(0) == Some(entry_pred))
        .filter_map(|(id, n)| Some((id as NodeId, graph.node_opt(n.input_opt(1)?)?)))
        .collect()
}

/// The values the pre-header guards prove zero.
fn entry_zeros(graph: &Graph, entry_pred: NodeId) -> FxHashSet<NodeId> {
    entry_guards(graph, entry_pred)
        .into_iter()
        .filter_map(|(_, c)| proves_zero(graph, c))
        .collect()
}

/// The live pre-header mask guard on `entry_pred` covering `arr` up to at
/// least `m`, its `ArrayLength` anchored there too (the `mask_predicate`
/// shape).
fn entry_mask_guard(graph: &Graph, entry_pred: NodeId, arr: NodeId, m: i64) -> Option<NodeId> {
    entry_guards(graph, entry_pred)
        .into_iter()
        .find_map(|(id, c)| {
            let (CmpOp::Lt, k, len) = cmp_parts(c)? else {
                return None;
            };
            let kn = graph.node_opt(k)?;
            let Op::Const(kv) = kn.op else {
                return None;
            };
            let ln = graph.node_opt(len)?;
            (kn.ty == IrType::Int
                && kv >= m
                && ln.op == Op::ArrayLength
                && ln.input_opt(0) == Some(entry_pred)
                && ln.input_opt(2) == Some(arr))
            .then_some(id)
        })
}

/// The pre-header `ArrayLength(arr)` anchored on `entry_pred`, if any.
fn entry_length_of(graph: &Graph, entry_pred: NodeId, arr: NodeId) -> Option<NodeId> {
    graph.nodes.iter().enumerate().find_map(|(id, n)| {
        (n.op == Op::ArrayLength
            && n.input_opt(0) == Some(entry_pred)
            && n.input_opt(2) == Some(arr))
        .then_some(id as NodeId)
    })
}

/// The `Cmp` facts the live guards anchored on `entry_pred` state:
/// `(op, a, b)` holds at every node of the loop the pre-header enters.
fn entry_facts(graph: &Graph, entry_pred: NodeId) -> Vec<(CmpOp, NodeId, NodeId)> {
    entry_guards(graph, entry_pred)
        .into_iter()
        .filter_map(|(_, c)| cmp_parts(c))
        .collect()
}

/// Does a fact decide `cond` (`Some(true)` / `Some(false)`)?
fn decided_by(facts: &[(CmpOp, NodeId, NodeId)], cond: &Node) -> Option<bool> {
    let (cop, ca, cb) = cmp_parts(cond)?;
    facts.iter().find_map(|&(op, a, b)| {
        let same =
            (ca, cb) == (a, b) || (matches!(op, CmpOp::Eq | CmpOp::Ne) && (ca, cb) == (b, a));
        if !same {
            None
        } else if cop == op {
            Some(true)
        } else if cop == op.negate() {
            Some(false)
        } else {
            None
        }
    })
}

/// Plant the pre-header the module notes describe in every loop that takes
/// one. `despec` / `method_key` withdraw a header per planter kind;
/// `osr_route` is `OptimizeContext::osr_route`.
pub(super) fn plant_trap_free_preheaders(
    graph: &mut Graph,
    despec: &crate::deopt::DespecRegistry,
    method_key: &str,
    osr_route: bool,
) -> PreheaderCensus {
    let mut census = PreheaderCensus::default();
    if !trap_free_preheader_enabled() || graph.safepoints.is_empty() {
        return census;
    }
    let mut left_frames: Vec<u32> = Vec::new();
    for (region, entry_pred, back_ctrls) in loop_headers(graph) {
        if entry_pred == NO_NODE
            || !matches!(
                graph.node_opt(region).map(|n| &n.op),
                Some(Op::Region | Op::Merge)
            )
        {
            continue;
        }
        let site = LoopSite {
            region,
            entry_pred,
            back_ctrls: &back_ctrls,
        };
        let is_osr = osr_route || graph.osr_compile;
        census.add(plant_one(
            graph,
            despec,
            method_key,
            is_osr,
            site,
            &mut left_frames,
        ));
    }
    if !left_frames.is_empty() {
        let orphans = orphaned_predicate_frames(graph, &left_frames);
        if !orphans.is_empty() {
            let mut keep = vec![true; graph.safepoints.len()];
            for o in orphans {
                if let Some(k) = keep.get_mut(o) {
                    *k = false;
                }
            }
            let _ = graph.retain_safepoints(&keep);
        }
    }
    census
}

/// One loop, as `loop_headers` names it.
#[derive(Clone, Copy)]
struct LoopSite<'a> {
    region: NodeId,
    entry_pred: NodeId,
    back_ctrls: &'a [NodeId],
}

/// The loop-entry snapshot this loop's planted nodes name, pushed on first
/// use.
fn entry_snapshot(graph: &mut Graph, si: &mut Option<u32>, frame: &SafepointSnapshot) -> u32 {
    if let Some(idx) = *si {
        return idx;
    }
    graph.push_safepoint(frame.clone());
    // Cast: a snapshot index fits the node field's `u32`, as every planter's.
    let idx = (graph.safepoints.len() - 1) as u32;
    *si = Some(idx);
    idx
}

/// Kill `old` (an in-body guard or length read) in favour of `new`, keeping
/// its frame for the orphan sweep.
fn retire_to(graph: &mut Graph, old: NodeId, new: NodeId, left_frames: &mut Vec<u32>) {
    if let Some(fs) = graph.node_opt(old).and_then(|n| n.frame_snapshot) {
        left_frames.push(fs);
    }
    graph.replace_all_uses(old, new);
    graph.kill(old);
}

/// Is `id` a value defined before the loop of `region` / `body`?
fn invariant_in(graph: &Graph, id: NodeId, region: NodeId, body: &FxHashSet<NodeId>) -> bool {
    !body.contains(&id) && is_loop_invariant(graph, id, region, body)
}

fn plant_one(
    graph: &mut Graph,
    despec: &crate::deopt::DespecRegistry,
    method_key: &str,
    is_osr: bool,
    site: LoopSite<'_>,
    left_frames: &mut Vec<u32>,
) -> PreheaderCensus {
    use crate::deopt::{predicate_withdrawn, PredicateKind};
    let LoopSite {
        region,
        entry_pred,
        back_ctrls,
    } = site;
    let mut census = PreheaderCensus::default();
    let users = build_users(graph);
    let mut body = loop_body_with(graph, &users, region, entry_pred, back_ctrls);
    if is_osr
        && !loop_entered_only_through_its_pre_header(graph, region, entry_pred, &body)
        && !spliced_loop_entered_only_through_its_pre_header(graph, region, &body)
    {
        return census;
    }
    if !body_only_reads(graph, &body) {
        return census;
    }
    let Some(header_bci) = graph.node_opt(region).and_then(|r| r.bytecode_pc) else {
        return census;
    };
    let Ok(bci32) = u32::try_from(header_bci) else {
        return census;
    };
    let invariant_ok = !predicate_withdrawn(despec, method_key, bci32, PredicateKind::Length);
    let mask_ok = !predicate_withdrawn(despec, method_key, bci32, PredicateKind::Mask);
    if !invariant_ok && !mask_ok {
        return census;
    }
    let Some(entry_slot) = graph
        .node_opt(region)
        .and_then(|r| r.inputs.iter().position(|&p| p == entry_pred))
        .map(|p| p + 1)
    else {
        return census;
    };
    let Some((hb, frame)) = loop_entry_frame(graph, region, entry_slot, &body) else {
        return census;
    };
    let entry_mem = loop_entry_memory(graph, region, entry_pred);
    if !is_osr && mask_ok {
        if hoist_invariant_final_array_loads(
            graph,
            region,
            entry_pred,
            entry_mem,
            &users,
            &body,
        ) > 0
        {
            body = loop_body_with(graph, &users, region, entry_pred, back_ctrls);
        }
    }
    let mut si: Option<u32> = None;
    let mut hoisted = 0usize;
    // JIT round 20 wave 5 (`R20FuzzLicm1`): a division's zero guard stays at
    // its division's bci unless the guard token orders the division after it.
    let token_edges = crate::ir::guard_token_edges_enabled();
    let division_bcis: FxHashSet<usize> = graph
        .nodes
        .iter()
        .filter(|n| matches!(n.op, Op::Div | Op::Rem))
        .filter_map(|n| n.bytecode_pc)
        .collect();
    // nio21 wave 6 L2: no pre-header length test of an array an
    // exception-edge anchor of the body indexes.
    let anchors = crate::ir::exception_edge_anchors(graph);
    let anchor_arrays =
        crate::ir::exception_edge_anchor_arrays(graph, &anchors, &|id: NodeId| body.contains(&id));

    // Which guards run every iteration, decided before the first edit: the
    // dominance answer reads a users map that must match the arena.
    let mut every_iteration: Vec<NodeId> = body
        .iter()
        .copied()
        .filter(|&id| {
            let Some(g) = graph.node_opt(id) else {
                return false;
            };
            let Op::Guard { bci } = g.op else {
                return false;
            };
            // Exactly `[ctrl, cond]`: a guard carrying another guard's token
            // is ordered after it, and stays.
            let &[gctrl, _] = g.inputs.as_slice() else {
                return false;
            };
            !(division_bcis.contains(&bci) && !token_edges)
                && (gctrl == region || back_edges_dominated_by(graph, &users, gctrl, back_ctrls))
        })
        .collect();
    every_iteration.sort_unstable();
    let live_cond = |graph: &Graph, gid: NodeId| -> Option<NodeId> {
        graph
            .node_opt(gid)
            .filter(|g| matches!(g.op, Op::Guard { .. }))
            .and_then(|g| g.input_opt(1))
    };

    // Phase 1: invariant compares.
    if invariant_ok {
        for &gid in &every_iteration {
            if hoisted >= MAX_HOISTS_PER_LOOP {
                break;
            }
            let Some((op, a, b)) = live_cond(graph, gid)
                .and_then(|c| graph.node_opt(c))
                .and_then(cmp_parts)
            else {
                continue;
            };
            let constant = |id: NodeId| {
                graph
                    .node_opt(id)
                    .is_some_and(|n| matches!(n.op, Op::Const(_)))
            };
            if !invariant_in(graph, a, region, &body)
                || !invariant_in(graph, b, region, &body)
                || (constant(a) && constant(b))
            {
                continue;
            }
            let idx = entry_snapshot(graph, &mut si, &frame);
            let before = graph.nodes.len();
            let cmp = graph.add(Op::Cmp(op), IrType::Int, vec![a, b], None);
            let ng = graph.add(
                Op::Guard { bci: hb },
                IrType::Void,
                vec![entry_pred, cmp],
                Some(hb),
            );
            if !graph.set_node_frame_snapshot(ng, idx) {
                graph.kill(ng);
                graph.kill(cmp);
                continue;
            }
            tag_predicate_guards(graph, before, PredicateKind::Length);
            retire_to(graph, gid, ng, left_frames);
            census.invariant += 1;
            hoisted += 1;
        }
    }

    // Phase 2: constant-masked bounds, a shift proven zero only by a guard
    // that is in the pre-header now.
    if mask_ok {
        let zero = entry_zeros(graph, entry_pred);
        for &gid in &every_iteration {
            if hoisted >= MAX_HOISTS_PER_LOOP {
                break;
            }
            let Some(c) = live_cond(graph, gid).and_then(|c| graph.node_opt(c)) else {
                continue;
            };
            let Some((m, len)) = masked_bound(graph, c) else {
                continue;
            };
            let Some(arr) = length_base(graph, len, &zero) else {
                continue;
            };
            let admitted = graph.node_opt(arr).is_some_and(|an| {
                an.ty == IrType::Ref
                    && !matches!(an.op, Op::NewArray { .. } | Op::MultiNewArray { .. })
            }) && invariant_in(graph, arr, region, &body)
                && !anchor_arrays.contains(&arr);
            if !admitted {
                continue;
            }
            // The memory the in-body length read reads when it is outside the
            // body: the fallback for a loop with no memory φ.
            let read_mem = length_read(graph, len)
                .and_then(|n| n.input_opt(1))
                .filter(|m| !body.contains(m));
            let ng = match entry_mask_guard(graph, entry_pred, arr, m) {
                Some(existing) => existing,
                None => {
                    let Some(mem) = entry_mem.or(read_mem) else {
                        continue;
                    };
                    let idx = entry_snapshot(graph, &mut si, &frame);
                    let before = graph.nodes.len();
                    let pre_len = graph.add(
                        Op::ArrayLength,
                        IrType::Int,
                        vec![entry_pred, mem, arr],
                        Some(hb),
                    );
                    let k = graph.add(Op::Const(m), IrType::Int, vec![], None);
                    let cmp =
                        graph.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![k, pre_len], Some(hb));
                    let ng = graph.add(
                        Op::Guard { bci: hb },
                        IrType::Void,
                        vec![entry_pred, cmp],
                        Some(hb),
                    );
                    if !(graph.set_node_frame_snapshot(pre_len, idx)
                        && graph.set_node_frame_snapshot(ng, idx))
                    {
                        graph.kill(ng);
                        graph.kill(cmp);
                        graph.kill(pre_len);
                        continue;
                    }
                    tag_predicate_guards(graph, before, PredicateKind::Mask);
                    ng
                }
            };
            retire_to(graph, gid, ng, left_frames);
            census.masked += 1;
            hoisted += 1;
        }
    }

    if invariant_ok {
        let if_hoists = hoist_invariant_exit_ifs(
            graph, region, entry_pred, entry_mem, hb, &frame, &mut si, &users, back_ctrls, &body,
        );
        census.invariant += if_hoists;
    }
    census.folded = fold_body_tests(graph, entry_pred, &body);
    census.lengths = reuse_entry_lengths(graph, region, entry_pred, &body, left_frames);
    census
}

/// Fold the in-body `If` / `Guard` tests the pre-header's facts decide; a
/// guard only to TRUE. Returns how many conditions were folded.
fn fold_body_tests(graph: &mut Graph, entry_pred: NodeId, body: &FxHashSet<NodeId>) -> usize {
    let facts = entry_facts(graph, entry_pred);
    if facts.is_empty() {
        return 0;
    }
    let mut tests: Vec<NodeId> = body
        .iter()
        .copied()
        .filter(|&id| {
            graph
                .node_opt(id)
                .is_some_and(|n| matches!(n.op, Op::If | Op::Guard { .. }) && n.inputs.len() >= 2)
        })
        .collect();
    tests.sort_unstable();
    let mut folded = 0usize;
    let mut one: Option<NodeId> = None;
    let mut nil: Option<NodeId> = None;
    for id in tests {
        let Some(n) = graph.node_opt(id) else {
            continue;
        };
        let is_guard = matches!(n.op, Op::Guard { .. });
        let ctrl = n.inputs[0];
        let Some(holds) = graph
            .node_opt(n.inputs[1])
            .and_then(|c| decided_by(&facts, c))
        else {
            continue;
        };
        if is_guard && !holds {
            continue;
        }
        let k = if holds {
            *one.get_or_insert_with(|| graph.add(Op::Const(1), IrType::Int, vec![], None))
        } else {
            *nil.get_or_insert_with(|| graph.add(Op::Const(0), IrType::Int, vec![], None))
        };
        if graph.set_input(id, 1, k) {
            folded += 1;
            if !is_guard {
                collapse_folded_if(graph, id, ctrl, holds);
            }
        }
    }
    folded
}

/// Collapse a folded `If(ctrl, Const(holds))` when its dead projection only
/// reaches `Merge` nodes without `Phi`s, exposing the surviving arm's nodes
/// directly under `ctrl` for subsequent preheader passes.
fn collapse_folded_if(graph: &mut Graph, if_id: NodeId, ctrl: NodeId, holds: bool) {
    let taken_idx = u16::from(!holds);
    let dead_idx = 1 - taken_idx;
    let mut taken_proj = None;
    let mut dead_proj = None;
    for u in graph.users_of(if_id) {
        match graph.node_opt(u).map(|n| &n.op) {
            Some(Op::Proj(w)) if *w == taken_idx => taken_proj = Some(u),
            Some(Op::Proj(w)) if *w == dead_idx => dead_proj = Some(u),
            _ => {}
        }
    }
    if let Some(dp) = dead_proj {
        let dp_users = graph.users_of(dp);
        for &m in &dp_users {
            let Some(mn) = graph.node_opt(m) else {
                return;
            };
            if mn.op != Op::Merge
                || graph
                    .users_of(m)
                    .into_iter()
                    .any(|u| graph.node_opt(u).is_some_and(|un| un.op == Op::Phi))
            {
                return;
            }
        }
        for m in dp_users {
            let Some(mn) = graph.node_opt(m) else {
                continue;
            };
            let inputs_copy = mn.inputs.clone();
            let kept: Vec<NodeId> = inputs_copy.iter().copied().filter(|&x| x != dp).collect();
            if kept.len() == 1 {
                let single = if Some(kept[0]) == taken_proj {
                    ctrl
                } else {
                    kept[0]
                };
                let mut m_users: Vec<(NodeId, usize)> = Vec::new();
                for u in graph.users_of(m) {
                    for i in 0..graph.nodes[u as usize].inputs.len() {
                        if graph.nodes[u as usize].inputs[i] == m {
                            m_users.push((u, i));
                        }
                    }
                }
                for (u, idx) in m_users {
                    graph.set_input(u, idx, single);
                }
            } else {
                for (idx, &inp) in inputs_copy.iter().enumerate() {
                    if inp == dp {
                        graph.set_input(m, idx, NO_NODE);
                    }
                }
            }
        }
    }
    if let Some(tp) = taken_proj {
        let mut tp_users: Vec<(NodeId, usize)> = Vec::new();
        for u in graph.users_of(tp) {
            for i in 0..graph.nodes[u as usize].inputs.len() {
                if graph.nodes[u as usize].inputs[i] == tp {
                    tp_users.push((u, i));
                }
            }
        }
        for (u, idx) in tp_users {
            graph.set_input(u, idx, ctrl);
        }
    }
}

/// Redirect memory-token users of `old` to `in_mem`, then retire `old` to `new`.
fn rewire_memory_and_retire(
    graph: &mut Graph,
    old: NodeId,
    in_mem: NodeId,
    new: NodeId,
    left_frames: &mut Vec<u32>,
) {
    let mut edits: Vec<(NodeId, usize)> = Vec::new();
    for u in graph.users_of(old) {
        let Some(un) = graph.node_opt(u) else {
            continue;
        };
        for (slot, &x) in un.inputs.as_slice().iter().enumerate() {
            if x == old && crate::ir::is_memory_token_slot(un, slot) {
                edits.push((u, slot));
            }
        }
    }
    for (u, slot) in edits {
        let _ = graph.set_input(u, slot, in_mem);
    }
    retire_to(graph, old, new, left_frames);
}

/// Replace each in-body `ArrayLength(a)` of an invariant array the pre-header
/// reads by that read: memory-token users take the read's own incoming token,
/// value users the pre-header read. Returns how many were replaced.
fn reuse_entry_lengths(
    graph: &mut Graph,
    region: NodeId,
    entry_pred: NodeId,
    body: &FxHashSet<NodeId>,
    left_frames: &mut Vec<u32>,
) -> usize {
    let mut lengths: Vec<NodeId> = body
        .iter()
        .copied()
        .filter(|&id| graph.node_opt(id).is_some_and(|n| n.op == Op::ArrayLength))
        .collect();
    lengths.sort_unstable();
    let mut replaced = 0usize;
    for al in lengths {
        let Some((in_mem, arr)) = graph
            .node_opt(al)
            .filter(|n| n.op == Op::ArrayLength)
            .and_then(|n| Some((n.input_opt(1)?, n.input_opt(2)?)))
        else {
            continue;
        };
        if !invariant_in(graph, arr, region, body) {
            continue;
        }
        let Some(pre) = entry_length_of(graph, entry_pred, arr) else {
            continue;
        };
        rewire_memory_and_retire(graph, al, in_mem, pre, left_frames);
        replaced += 1;
    }
    replaced
}

/// Hoist loop-invariant `If` exits (such as `local_throw_dynamic_edge`'s
/// `InstanceOf` + `ClassInitTest` event guards) in a read-only loop.
#[allow(clippy::too_many_arguments)]
fn hoist_invariant_exit_ifs(
    graph: &mut Graph,
    region: NodeId,
    entry_pred: NodeId,
    entry_mem: Option<NodeId>,
    hb: usize,
    frame: &SafepointSnapshot,
    si: &mut Option<u32>,
    users: &[Vec<NodeId>],
    back_ctrls: &[NodeId],
    body: &FxHashSet<NodeId>,
) -> usize {
    let mut ifs: Vec<NodeId> = body
        .iter()
        .copied()
        .filter(|&id| {
            graph.node_opt(id).is_some_and(|n| {
                n.op == Op::If
                    && n.inputs.len() >= 2
                    && (n.inputs[0] == region
                        || back_edges_dominated_by(graph, users, n.inputs[0], back_ctrls)
                        || is_pure_class_init_cond(graph, n.inputs[1], 0))
            })
        })
        .collect();
    ifs.sort_unstable();
    let mut hoisted = 0usize;
    for iff in ifs {
        let Some(cond) = graph.node_opt(iff).and_then(|n| n.input_opt(1)) else {
            continue;
        };
        let (mut p0, mut p1) = (None, None);
        for &u in &users[iff as usize] {
            match graph.node_opt(u).map(|n| &n.op) {
                Some(Op::Proj(0)) => p0 = Some(u),
                Some(Op::Proj(1)) => p1 = Some(u),
                _ => {}
            }
        }
        let (Some(p0), Some(p1)) = (p0, p1) else {
            continue;
        };
        let (keep_true, spec_final) = match (body.contains(&p0), body.contains(&p1)) {
            (true, false) => (true, false),
            (false, true) => (false, false),
            (true, true) => {
                let Some((op @ (CmpOp::Eq | CmpOp::Ne), a, b)) =
                    graph.node_opt(cond).and_then(cmp_parts)
                else {
                    continue;
                };
                let final_var = if is_int_zero(graph, b) {
                    Some(a)
                } else if is_int_zero(graph, a) {
                    Some(b)
                } else {
                    None
                };
                let Some(v) = final_var.filter(|&v| {
                    invariant_in(graph, v, region, body)
                        && graph.final_java_field_reads.contains(&v)
                }) else {
                    continue;
                };
                let _ = v;
                (op == CmpOp::Eq, true)
            }
            _ => continue,
        };
        let mut order: Vec<NodeId> = Vec::new();
        let mut visited: FxHashSet<NodeId> = FxHashSet::default();
        if !collect_hoistable_cond(graph, cond, region, body, &mut visited, &mut order) {
            if !spec_final {
                if let Some((m, len)) = graph
                    .node_opt(cond)
                    .and_then(|cn| masked_exit_bound(graph, cn, keep_true))
                {
                    if let Some(arr) = graph
                        .node_opt(len)
                        .filter(|ln| ln.op == Op::ArrayLength)
                        .and_then(|ln| ln.input_opt(2))
                        .filter(|&arr| invariant_in(graph, arr, region, body))
                    {
                        let pre_len = entry_length_of(graph, entry_pred, arr).or_else(|| {
                            let mem = entry_mem?;
                            let pre_len = graph.add(
                                Op::ArrayLength,
                                IrType::Int,
                                vec![entry_pred, mem, arr],
                                Some(hb),
                            );
                            let idx = entry_snapshot(graph, si, frame);
                            if !graph.set_node_frame_snapshot(pre_len, idx) {
                                graph.kill(pre_len);
                                return None;
                            }
                            Some(pre_len)
                        });
                        if let Some(pre_len) = pre_len {
                            let idx = entry_snapshot(graph, si, frame);
                            let before = graph.nodes.len();
                            let k_m = graph.add(Op::Const(m), IrType::Int, vec![], None);
                            let cmp = graph.add(
                                Op::Cmp(CmpOp::Lt),
                                IrType::Int,
                                vec![k_m, pre_len],
                                Some(hb),
                            );
                            let ng = graph.add(
                                Op::Guard { bci: hb },
                                IrType::Void,
                                vec![entry_pred, cmp],
                                Some(hb),
                            );
                            if graph.set_node_frame_snapshot(ng, idx) {
                                tag_predicate_guards(
                                    graph,
                                    before,
                                    crate::deopt::PredicateKind::Mask,
                                );
                                let k = graph.add(
                                    Op::Const(if keep_true { 1 } else { 0 }),
                                    IrType::Int,
                                    vec![],
                                    None,
                                );
                                if graph.set_input(iff, 1, k) {
                                    hoisted += 1;
                                    if let Some(ctrl) =
                                        graph.node_opt(iff).and_then(|n| n.input_opt(0))
                                    {
                                        collapse_folded_if(graph, iff, ctrl, keep_true);
                                    }
                                }
                            } else {
                                graph.kill(ng);
                                graph.kill(cmp);
                                graph.kill(k_m);
                            }
                        }
                    }
                }
            }
            continue;
        }
        let needs_mem = order.iter().any(|&id| {
            graph
                .node_opt(id)
                .is_some_and(|n| matches!(n.op, Op::InstanceOf { .. }))
        });
        let mem = if needs_mem {
            let Some(m) = entry_mem else {
                continue;
            };
            Some(m)
        } else {
            None
        };
        let idx = entry_snapshot(graph, si, frame);
        let mut cloned: std::collections::HashMap<NodeId, NodeId> =
            std::collections::HashMap::new();
        let mut ok = true;
        for id in order {
            let Some(n) = graph.node_opt(id).cloned() else {
                ok = false;
                break;
            };
            let map_in = |x: NodeId| cloned.get(&x).copied().unwrap_or(x);
            let (new_op, new_inputs, snap) = match &n.op {
                Op::ClassInitTest {
                    state_addr,
                    initialized,
                } => (
                    Op::ClassInitTest {
                        state_addr: *state_addr,
                        initialized: *initialized,
                    },
                    vec![entry_pred],
                    false,
                ),
                Op::InstanceOf {
                    name_ptr,
                    name_len,
                    seen_cell,
                    header_only,
                } => (
                    Op::InstanceOf {
                        name_ptr: *name_ptr,
                        name_len: *name_len,
                        seen_cell: *seen_cell,
                        header_only: *header_only,
                    },
                    vec![entry_pred, mem.unwrap(), map_in(n.inputs[2])],
                    true,
                ),
                op @ (Op::And | Op::Or | Op::Xor | Op::Cmp(_) | Op::Const(_)) => (
                    op.clone(),
                    n.inputs.iter().map(|&x| map_in(x)).collect(),
                    false,
                ),
                _ => {
                    ok = false;
                    break;
                }
            };
            let new_id = graph.add(new_op, n.ty, new_inputs, snap.then_some(hb));
            if snap && !graph.set_node_frame_snapshot(new_id, idx) {
                graph.kill(new_id);
                ok = false;
                break;
            }
            cloned.insert(id, new_id);
        }
        if !ok {
            continue;
        }
        let pre_cond = cloned.get(&cond).copied().unwrap_or(cond);
        let guard_cond = if keep_true {
            pre_cond
        } else {
            let zero = graph.add(Op::Const(0), IrType::Int, vec![], None);
            graph.add(Op::Cmp(CmpOp::Eq), IrType::Int, vec![pre_cond, zero], None)
        };
        let before = graph.nodes.len();
        let ng = graph.add(
            Op::Guard { bci: hb },
            IrType::Void,
            vec![entry_pred, guard_cond],
            Some(hb),
        );
        if !graph.set_node_frame_snapshot(ng, idx) {
            graph.kill(ng);
            continue;
        }
        if spec_final {
            tag_predicate_guards(graph, before, crate::deopt::PredicateKind::Length);
        }
        let k = graph.add(
            Op::Const(if keep_true { 1 } else { 0 }),
            IrType::Int,
            vec![],
            None,
        );
        if graph.set_input(iff, 1, k) {
            hoisted += 1;
            if let Some(ctrl) = graph.node_opt(iff).and_then(|n| n.input_opt(0)) {
                collapse_folded_if(graph, iff, ctrl, keep_true);
            }
        }
    }
    hoisted
}

fn hoist_invariant_final_array_loads(
    graph: &mut Graph,
    region: NodeId,
    entry_pred: NodeId,
    entry_mem: Option<NodeId>,
    users: &[Vec<NodeId>],
    body: &FxHashSet<NodeId>,
) -> usize {
    let Some(mem) = entry_mem else {
        return 0;
    };
    let mut loads: Vec<NodeId> = body
        .iter()
        .copied()
        .filter(|&id| {
            let Some(n) = graph.node_opt(id) else {
                return false;
            };
            matches!(n.op, Op::Load(crate::ir::MemKind::Ref))
                && graph.final_java_field_reads.contains(&id)
                && n.inputs.len() >= 4
                && invariant_in(graph, n.inputs[2], region, body)
                && invariant_in(graph, n.inputs[3], region, body)
                && super::late_base_non_null(graph, n.inputs[2], entry_pred)
                && !body.iter().any(|&bid| {
                    graph.node_opt(bid).is_some_and(|bn| {
                        matches!(
                            bn.op,
                            Op::ExactClassIs { .. }
                                | Op::InstanceOf { .. }
                                | Op::CheckCast { .. }
                        ) && bn.inputs.contains(&n.inputs[2])
                    })
                })
                && users[id as usize].iter().any(|&u| {
                    body.contains(&u)
                        && graph.node_opt(u).is_some_and(|un| un.op == Op::ArrayLength)
                })
        })
        .collect();
    loads.sort_unstable();
    let mut hoisted = 0usize;
    for id in loads {
        let Some(n) = graph.node_opt(id).cloned() else {
            continue;
        };
        let pre = graph.add(
            n.op,
            n.ty,
            vec![entry_pred, mem, n.inputs[2], n.inputs[3]],
            n.bytecode_pc,
        );
        if graph.final_java_field_reads.contains(&id) {
            graph.final_java_field_reads.insert(pre);
        }
        graph.replace_all_uses(id, pre);
        graph.kill(id);
        hoisted += 1;
    }
    hoisted
}

fn masked_exit_bound(graph: &Graph, cond: &Node, keep_true: bool) -> Option<(i64, NodeId)> {
    let (op, a, b) = cmp_parts(cond)?;
    let (idx, len) = match (keep_true, op) {
        (true, CmpOp::Lt) => (a, b),
        (true, CmpOp::Gt) => (b, a),
        (false, CmpOp::Ge) => (a, b),
        (false, CmpOp::Le) => (b, a),
        _ => return None,
    };
    let m = crate::ir_check_elim::const_mask_of(graph, idx)?;
    (0..=i64::from(i32::MAX)).contains(&m).then_some((m, len))
}

fn is_pure_class_init_cond(graph: &Graph, id: NodeId, depth: usize) -> bool {
    if depth > 16 {
        return false;
    }
    let Some(n) = graph.node_opt(id) else {
        return false;
    };
    match &n.op {
        Op::ClassInitTest { .. } => true,
        Op::And | Op::Or | Op::Xor | Op::Cmp(_) => {
            !n.inputs.is_empty()
                && n.inputs
                    .iter()
                    .all(|&d| is_pure_class_init_cond(graph, d, depth + 1) || graph.node_opt(d).is_some_and(|dn| matches!(dn.op, Op::Const(_))))
        }
        _ => false,
    }
}

fn collect_hoistable_cond(
    graph: &Graph,
    id: NodeId,
    region: NodeId,
    body: &FxHashSet<NodeId>,
    visited: &mut FxHashSet<NodeId>,
    order: &mut Vec<NodeId>,
) -> bool {
    if invariant_in(graph, id, region, body) || !visited.insert(id) {
        return true;
    }
    if visited.len() > 16 {
        return false;
    }
    let Some(n) = graph.node_opt(id) else {
        return false;
    };
    let deps: &[NodeId] = match &n.op {
        Op::ClassInitTest { .. } | Op::Const(_) => &[],
        Op::InstanceOf { .. } if n.inputs.len() >= 3 => &n.inputs[2..3],
        Op::And | Op::Or | Op::Xor | Op::Cmp(_) => &n.inputs,
        _ => return false,
    };
    for &d in deps {
        if !collect_hoistable_cond(graph, d, region, body, visited, order) {
            return false;
        }
    }
    order.push(id);
    true
}

#[cfg(test)]
#[path = "w7_trap_free_preheader_tests.rs"]
mod tests;
