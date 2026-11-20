// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT round 22 wave 1 (lane loops, LOOPS22-1 of
//! `docs/feature-designs/jit-ir-optimizations.md`; HOIST13-2's
//! residual): retire a counted loop whose every carried value is linear
//! (`CRATONVM_JIT_IR_LOOP_RETIRE_ACCUMULATORS`, opt-in).
//!
//! # Why
//!
//! The cell promotions (`loop_field_forward`: static, field, fresh, element)
//! leave `for (i = 0; i < n; i++) COUNTER++;` as a loop whose body is two
//! register adds: `R20PLoopStaticCells` static-counter rows at ~0.47 ns per
//! iteration, HotSpot 25 at 0.0 -- C2 replaces an induction-linear value by its
//! exit value (`PhaseIdealLoop::do_maximally_unroll` / the empty-loop removal
//! after IV replacement) and deletes the loop that computes nothing else.
//!
//! # What
//!
//! A loop `region -> If(cond) -> { back, exit }` (the unroller's single-block
//! shape) whose
//!
//! * test is `iv < bound` with `iv += 1` per trip, or `iv > bound` with
//!   `iv -= 1` (signed `int`, strict: the loop always terminates, after
//!   `max(0, bound - iv0)` resp. `max(0, iv0 - bound)` trips), `bound` loop
//!   invariant;
//! * every other header φ is invariant (its back value is itself or its entry
//!   value -- the memory φ of a loop that no longer touches memory) or LINEAR:
//!   an `int` / `long` `p + x` / `x + p` / `p - x` with `x` invariant;
//! * body is nothing else: no node is anchored at the header, the test or the
//!   back edge but the φs, the test and its two projections, and the per-trip
//!   values (the test's compare and each linear φ's add) have no user outside
//!   the loop,
//!
//! is replaced by its exit values, computed in the pre-header and joined to
//! the exit's users:
//!
//! * `trip = max(0L, (long) bound - (long) iv0)` (resp. `iv0 - bound`), exact;
//! * `iv_exit = max(iv0, bound)` (resp. `min`);
//! * `p_exit = p0 + trip * x` (resp. `- trip * x`), `long` arithmetic for a
//!   `long` cell and the `int` product of `(int) trip` for an `int` one --
//!   both wrap exactly like `trip` repeated additions do (Java `int` / `long`
//!   arithmetic is modulo 2^32 / 2^64, and so is `p0 + trip * x`).
//!
//! # Why the frames stay right
//!
//! Nothing in the retired loop can deopt (adds and a compare), so the only
//! readers of its snapshots are the surviving nodes. After the rewrite the
//! header's flat snapshot names the EXIT values: exactly what it named at the
//! loop's exit before, so a post-loop node at the header's bci (a promotion's
//! write-back, which names that frame) sees what it always saw. A pre-loop
//! node never read the header's φs (they are not defined there); one that
//! carries a bci of the loop must name its own frame
//! (`super::pc_reads_no_flat_frame`: the loop-entry predicates, a direct
//! `getstatic`; or the field forward's pre-header read with its own pre-header
//! frame). Every other flat snapshot inside the loop's pc range that names a
//! loop value is unreachable afterwards and joins `UNROLL_STRANDED_BCIS`, the
//! net `ir_lower` checks its by-bci transfers against (a disagreement refuses
//! the compile). No surviving pc-less node may deopt (it would inherit a
//! carried `cur_bci`). A spliced loop (unless `CRATONVM_JIT_IR_LOOP_RETIRE_SPLICED`,
//! whose in-loop chain snapshots join the channel's chain half, checked against
//! the pcs the emission publishes a chain at), a post-call chain snapshot
//! holding a loop value, or a frame outside the loop holding a per-trip value
//! refuses.
//!
//! Not on the optimizing OSR route nor an OSR-door compile: an ordinary OSR
//! entry stub jumps to a loop header, and this pass deletes one. In a
//! method-entry body the header's own OSR stub is simply not emitted (the
//! loop is gone), the cost the promotions already pay.

use super::loop_field_forward::ForwardDespec;
use super::{
    build_users, collapse_trivial_merges, const_i64, control_not_dominated_by, loop_headers,
    loop_pass_report_enabled, pc_reads_no_flat_frame, FxHashMap, FxHashSet, StrandedFrames,
    UNROLL_STRANDED_BCIS,
};
use crate::ir::{CmpOp, Graph, IrType, Node, NodeId, Op, SafepointSnapshot, ScalarOp, NO_NODE};

/// `CRATONVM_JIT_IR_LOOP_RETIRE_ACCUMULATORS` (JIT round 22 wave 1, lane
/// loops, LOOPS22-1): **default ON** since the r22 A/B (`=0` off; perf). Read once per optimize
/// (no `static`).
pub(super) fn loop_retire_enabled() -> bool {
    // R22-AB-FLIP: default ON since the JIT r22 opt-in A/B (docs/feature-designs/jit-ir-optimizations.md); `=0` is the kill switch.
    true
}

/// `CRATONVM_JIT_IR_LOOP_RETIRE_GUARDED_BOUNDS` (JIT round 22 wave 2, lane
/// loops, LOOPS22-3): **default ON** since the round-24 close (perf, `=0`
/// kills; inert without [`loop_retire_enabled`]; `R22LoopsWave2` inclusive-*
/// and not-equal-* 480-650 -> 7-14 ns, both modes, facts equal HotSpot's). Also retire `i <= n` / `i >= n` behind a
/// pre-header `Guard(n != MAX_VALUE / MIN_VALUE)` with the loop-entry frame,
/// and `i != n` (always finite: the index wraps until it meets `n`, a trip
/// count of `(n - i0) mod 2^32`). Read per candidate loop (no `static`).
pub(super) fn guarded_bounds_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_LOOP_RETIRE_SPLICED` (JIT round 22 wave 2, lane loops,
/// LOOPS22-2): **default ON** since the round-24 close (perf, `=0` kills;
/// inert without [`loop_retire_enabled`]; `R22LoopsWave2 spliced` 300 -> 14
/// ns jdk-only, 290 -> 14 compatible, HotSpot 41, facts equal, once a
/// spliced callee's trailing chain frame joins the net). Also retire a loop of a SPLICED callee, whose
/// frames are chain snapshots: the header's chain snapshot, like the flat one,
/// names the exit values afterwards (what a post-loop node at the header pc --
/// a spliced promotion's write-back -- always read), and every other chain
/// snapshot holding a loop value must be unreachable by the same per-node
/// rule. The lowering's net behind that prediction is the chain half of
/// `UNROLL_STRANDED_BCIS` (`StrandedFrames::chain`, the combined pcs of those
/// snapshots), checked against the pcs at which the emission published a
/// chain (`ir_lower`'s `chain_transfer_pcs`). Read per candidate loop (no
/// `static`).
pub(super) fn spliced_retire_enabled() -> bool {
    true
}

/// How the loop's test bounds its induction variable (`iv <cont> bound`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum TestForm {
    /// `<` with stride 1, `>` with stride -1.
    Strict,
    /// `<=` with stride 1, `>=` with stride -1 (LOOPS22-3, guarded).
    Inclusive,
    /// `!=`, either stride (LOOPS22-3).
    NotEqual,
}

/// One header φ of a candidate loop.
#[derive(Clone, Copy)]
enum Carried {
    /// Its back value is itself or its entry value: `entry` on every trip.
    Invariant { entry: NodeId },
    /// `p + step` (`neg == false`) or `p - step` per trip, `step` invariant;
    /// `next` is that add / sub.
    Linear {
        entry: NodeId,
        step: NodeId,
        neg: bool,
        next: NodeId,
    },
    /// `p +/-= iv`, `p +/-= iv +/- inv`, `p +/-= I2L(iv)`, or `p +/-= I2L(iv +/- inv)`
    /// on a unit-stride strict `int` counted loop.
    AffineIv {
        entry: NodeId,
        next: NodeId,
        cand_iv: NodeId,
        sign: i64,
        offset: Option<(NodeId, i64)>,
        widen_i2l: bool,
        i2l_inner_offset: Option<i64>,
        extra_gone: [NodeId; 2],
    },
}

/// Runs dead-node elimination, closed-form loop retirement, and strip-mining
/// after `ir_fold_box_phi_webs` scalarizes boxed loop accumulators.
pub(crate) fn post_box_fold_cleanup(graph: &mut Graph, replaced: &mut usize) {
    super::eliminate_dead_nodes(graph);
    if !graph.osr_compile && loop_retire_enabled() {
        let retired = retire_accumulator_loops(graph, None);
        if retired > 0 {
            *replaced += retired;
            super::eliminate_dead_nodes(graph);
        }
    }
    if super::strip_mine::strip_mine_enabled() {
        let iter = super::strip_mine::strip_mine_iter();
        let mined = super::strip_mine::strip_mine_loops(graph, iter);
        if mined > 0 {
            *replaced += mined;
            super::eliminate_dead_nodes(graph);
        }
    }
}

fn match_iv_plus_inv(
    graph: &Graph,
    variant: &FxHashSet<NodeId>,
    carried_set: &FxHashSet<NodeId>,
    p: NodeId,
    node_id: NodeId,
    expected_ty: IrType,
) -> Option<(NodeId, Option<(NodeId, i64)>)> {
    if carried_set.contains(&node_id)
        && node_id != p
        && graph.node_opt(node_id).map(|n| n.ty) == Some(expected_ty)
    {
        return Some((node_id, None));
    }
    let n = graph.node_opt(node_id)?;
    if n.ty != expected_ty || n.inputs.len() != 2 {
        return None;
    }
    let (a, b) = (n.inputs[0], n.inputs[1]);
    let is_iv = |x: NodeId| {
        carried_set.contains(&x) && x != p && graph.node_opt(x).map(|xn| xn.ty) == Some(expected_ty)
    };
    let is_inv = |x: NodeId| {
        x != NO_NODE
            && !variant.contains(&x)
            && graph.node_opt(x).map(|xn| xn.ty) == Some(expected_ty)
    };
    match n.op {
        Op::Add if is_iv(a) && is_inv(b) => Some((a, Some((b, 1)))),
        Op::Add if is_iv(b) && is_inv(a) => Some((b, Some((a, 1)))),
        Op::Sub if is_iv(a) && is_inv(b) => Some((a, Some((b, -1)))),
        _ => None,
    }
}

/// True when `iv + c` (32-bit `int` arithmetic) cannot wrap under `(stride, cont)`
/// starting at `iv_init`.
fn i2l_offset_no_wrap(graph: &Graph, iv_init: NodeId, stride: i64, cont: CmpOp, c: i64) -> bool {
    if c == 0 {
        return true;
    }
    match (stride, cont) {
        (1, CmpOp::Lt) => {
            c == 1
                || (c <= 1
                    && const_i64(graph, iv_init)
                        .is_some_and(|init_v| init_v + c >= i64::from(i32::MIN)))
        }
        (-1, CmpOp::Gt) => {
            c == -1
                || (c >= -1
                    && const_i64(graph, iv_init)
                        .is_some_and(|init_v| init_v + c <= i64::from(i32::MAX)))
        }
        _ => false,
    }
}

fn match_affine_iv(
    graph: &Graph,
    variant: &FxHashSet<NodeId>,
    carried_set: &FxHashSet<NodeId>,
    p: NodeId,
    ty: IrType,
    entry: NodeId,
    back: NodeId,
    term: NodeId,
    sign: i64,
) -> Option<Carried> {
    if ty == IrType::Int {
        let (cand_iv, offset) =
            match_iv_plus_inv(graph, variant, carried_set, p, term, IrType::Int)?;
        let extra = if offset.is_some() { term } else { NO_NODE };
        return Some(Carried::AffineIv {
            entry,
            next: back,
            cand_iv,
            sign,
            offset,
            widen_i2l: false,
            i2l_inner_offset: None,
            extra_gone: [extra, NO_NODE],
        });
    }
    if ty != IrType::Long {
        return None;
    }
    let tn = graph.node_opt(term)?;
    if tn.op == Op::I2L && tn.inputs.len() == 1 {
        let inner = tn.inputs[0];
        let is_iv = |id: NodeId| {
            carried_set.contains(&id)
                && id != p
                && graph.node_opt(id).map(|n| n.ty) == Some(IrType::Int)
        };
        if is_iv(inner) {
            return Some(Carried::AffineIv {
                entry,
                next: back,
                cand_iv: inner,
                sign,
                offset: None,
                widen_i2l: true,
                i2l_inner_offset: None,
                extra_gone: [term, NO_NODE],
            });
        }
        // Admit `I2L(iv +/- Const(c))` when the loop's induction test
        // guarantees 32-bit `iv + c` cannot wrap (validated once `iv`/`cont`
        // are known in `retire_loop`).
        let in_node = graph.node_opt(inner)?;
        if in_node.ty == IrType::Int && in_node.inputs.len() == 2 {
            let (a, b) = (in_node.inputs[0], in_node.inputs[1]);
            let int_const = |id: NodeId| {
                let cn = graph.node_opt(id)?;
                (cn.ty == IrType::Int)
                    .then(|| match cn.op {
                        Op::Const(v) => Some(i64::from(v as i32)),
                        _ => None,
                    })
                    .flatten()
            };
            let matched = match in_node.op {
                Op::Add => int_const(b)
                    .filter(|_| is_iv(a))
                    .map(|cv| (a, b, 1i64, cv))
                    .or_else(|| int_const(a).filter(|_| is_iv(b)).map(|cv| (b, a, 1i64, cv))),
                Op::Sub => int_const(b)
                    .filter(|_| is_iv(a))
                    .map(|cv| (a, b, -1i64, -cv)),
                _ => None,
            };
            if let Some((cand_iv, kc, off_sign, c_eff)) = matched {
                return Some(Carried::AffineIv {
                    entry,
                    next: back,
                    cand_iv,
                    sign,
                    offset: Some((kc, off_sign)),
                    widen_i2l: true,
                    i2l_inner_offset: Some(c_eff),
                    extra_gone: [term, inner],
                });
            }
        }
        return None;
    }
    if tn.ty == IrType::Long && tn.inputs.len() == 2 && matches!(tn.op, Op::Add | Op::Sub) {
        let (a, b) = (tn.inputs[0], tn.inputs[1]);
        let unwrap_i2l = |id: NodeId| {
            let n = graph.node_opt(id)?;
            if n.op == Op::I2L && n.inputs.len() == 1 {
                let iv_cand = n.inputs[0];
                if carried_set.contains(&iv_cand)
                    && iv_cand != p
                    && graph.node_opt(iv_cand).map(|x| x.ty) == Some(IrType::Int)
                {
                    return Some(iv_cand);
                }
            }
            None
        };
        let is_inv_l = |id: NodeId| {
            id != NO_NODE
                && !variant.contains(&id)
                && graph.node_opt(id).map(|x| x.ty) == Some(IrType::Long)
        };
        let (cand_iv, i2l_node, inv_l, off_sign) = match tn.op {
            Op::Add => {
                if let Some(c) = unwrap_i2l(a).filter(|_| is_inv_l(b)) {
                    (c, a, b, 1)
                } else if let Some(c) = unwrap_i2l(b).filter(|_| is_inv_l(a)) {
                    (c, b, a, 1)
                } else {
                    return None;
                }
            }
            Op::Sub => {
                let c = unwrap_i2l(a).filter(|_| is_inv_l(b))?;
                (c, a, b, -1)
            }
            _ => return None,
        };
        return Some(Carried::AffineIv {
            entry,
            next: back,
            cand_iv,
            sign,
            offset: Some((inv_l, off_sign)),
            widen_i2l: true,
            i2l_inner_offset: None,
            extra_gone: [term, i2l_node],
        });
    }
    None
}

/// Retire every loop of `graph` this module's section describes. Returns how
/// many were retired.
pub(super) fn retire_accumulator_loops(graph: &mut Graph, despec: ForwardDespec<'_>) -> usize {
    collapse_trivial_merges(graph);
    let mut retired = 0usize;
    let mut tried: FxHashSet<NodeId> = FxHashSet::default();
    // A retirement rewires the loop's exit to its entry, which may change the
    // next loop's shape: the loop list is taken again after each one.
    loop {
        let loops = loop_headers(graph);
        let Some((region, entry_pred, back_ctrls)) =
            loops.into_iter().find(|l| !tried.contains(&l.0))
        else {
            break;
        };
        tried.insert(region);
        let header_bci = graph.node_opt(region).and_then(|n| n.bytecode_pc);
        match retire_loop(graph, region, entry_pred, &back_ctrls, despec) {
            Ok(n_values) => {
                retired += 1;
                if loop_pass_report_enabled() || crate::x64::dbg_jitc_enabled() {
                    eprintln!(
                        "[ir] loop retire: header bci {header_bci:?}: {n_values} carried value(s) \
                         replaced by their exit values, loop deleted"
                    );
                }
            }
            Err(why) => {
                if loop_pass_report_enabled() {
                    eprintln!("[ir] loop retire: header bci {header_bci:?} refused -- {why}");
                }
            }
        }
    }
    retired
}

/// Does `sp` name any node of `set` in a local, stack or monitor slot?
fn snapshot_names(sp: &SafepointSnapshot, set: &FxHashSet<NodeId>) -> bool {
    sp.locals
        .iter()
        .chain(sp.stack.iter())
        .chain(sp.monitors.iter())
        .any(|&s| s != NO_NODE && set.contains(&s))
}

/// Does `n` name its own snapshot, recorded at `pc`, by `frame_snapshot`?
fn names_own_frame_at(graph: &Graph, n: &Node, pc: usize) -> bool {
    n.frame_snapshot
        .and_then(|si| graph.safepoints.get(si as usize))
        .is_some_and(|sp| sp.bci == pc && !sp.is_splice_state())
}

/// LOOPS22-2: does `n` name its own CHAIN snapshot, recorded at `pc`?
fn names_own_chain_frame_at(graph: &Graph, n: &Node, pc: usize) -> bool {
    n.frame_snapshot
        .and_then(|si| graph.safepoints.get(si as usize))
        .is_some_and(|sp| sp.bci == pc && sp.is_splice_state() && !sp.post_call_only)
}

/// The candidate test and the rewrite for one loop. `Ok(values)` once the
/// loop is gone; `Err(why)` leaves the graph exactly as it was.
fn retire_loop(
    graph: &mut Graph,
    region: NodeId,
    entry_pred: NodeId,
    back_ctrls: &[NodeId],
    despec: ForwardDespec<'_>,
) -> Result<usize, &'static str> {
    let [back_ctrl] = back_ctrls else {
        return Err("not exactly one back edge");
    };
    let back_ctrl = *back_ctrl;
    let header_bci = graph
        .node_opt(region)
        .and_then(|n| n.bytecode_pc)
        .ok_or("a header with no bytecode pc")?;
    let rin: Vec<NodeId> = graph.nodes[region as usize]
        .inputs
        .iter()
        .copied()
        .collect();
    if rin.len() != 2 {
        return Err("not a two-input header");
    }
    let (entry_slot, back_slot) = match (
        rin.iter().position(|&p| p == entry_pred),
        rin.iter().position(|&p| p == back_ctrl),
    ) {
        (Some(e), Some(b)) if e != b => (1 + e, 1 + b),
        _ => return Err("the header's inputs are not its entry and its back edge"),
    };
    let users = build_users(graph);

    // ── The single-block shape: region -> If -> { back, exit } ──
    let mut if_node = NO_NODE;
    let mut carried: Vec<NodeId> = Vec::new();
    for &u in &users[region as usize] {
        let n = &graph.nodes[u as usize];
        let anchored_here = n.inputs.first().copied() == Some(region);
        match n.op {
            Op::Phi if anchored_here => {
                if !carried.contains(&u) {
                    carried.push(u);
                }
            }
            Op::If if anchored_here && (if_node == NO_NODE || if_node == u) => if_node = u,
            _ => return Err("a node other than a phi or the test hangs on the header"),
        }
    }
    if if_node == NO_NODE {
        return Err("no test at the header");
    }
    let back_is_true = {
        let b = &graph.nodes[back_ctrl as usize];
        if b.inputs.first().copied() != Some(if_node) {
            return Err("the back edge is not a projection of the header's test");
        }
        match b.op {
            Op::Proj(0) => true,
            Op::Proj(1) => false,
            _ => return Err("the back edge is not a projection of the header's test"),
        }
    };
    let exit_which: u16 = if back_is_true { 1 } else { 0 };
    let mut exit_ctrl = NO_NODE;
    for &u in &users[if_node as usize] {
        if u == back_ctrl {
            continue;
        }
        match graph.nodes[u as usize].op {
            Op::Proj(k) if k == exit_which && (exit_ctrl == NO_NODE || exit_ctrl == u) => {
                exit_ctrl = u
            }
            _ => return Err("the test has a user other than its two projections"),
        }
    }
    if exit_ctrl == NO_NODE {
        return Err("no exit projection");
    }
    if users[back_ctrl as usize].iter().any(|&u| u != region) {
        if loop_pass_report_enabled() {
            let others: Vec<String> = users[back_ctrl as usize]
                .iter()
                .filter(|&&u| u != region)
                .map(|&u| format!("n{u} {:?}", graph.nodes[u as usize].op))
                .collect();
            eprintln!(
                "[ir] loop retire: region {region}: on the back edge n{back_ctrl}: {others:?}"
            );
        }
        return Err("a node hangs on the back edge");
    }
    let cond = graph.nodes[if_node as usize]
        .input_opt(1)
        .ok_or("a test with no condition")?;
    let (cmp_op, ca, cb) = match graph.node_opt(cond) {
        Some(n) if n.inputs.len() == 2 => match n.op {
            Op::Cmp(o) => (o, n.inputs[0], n.inputs[1]),
            _ => return Err("the test is not a compare"),
        },
        _ => return Err("the test is not a compare"),
    };

    // ── What varies: data reachable from a header φ (not through another φ,
    // not through control) ──
    let carried_set: FxHashSet<NodeId> = carried.iter().copied().collect();
    let mut variant: FxHashSet<NodeId> = carried_set.clone();
    let mut work: Vec<NodeId> = carried.clone();
    while let Some(c) = work.pop() {
        for &u in &users[c as usize] {
            let op = &graph.nodes[u as usize].op;
            if op.is_control() || (*op == Op::Phi && !carried_set.contains(&u)) {
                continue;
            }
            if variant.insert(u) {
                work.push(u);
            }
        }
    }

    // ── Each header φ: invariant or linear ──
    let mut kinds: FxHashMap<NodeId, Carried> = FxHashMap::default();
    for &p in &carried {
        let n = &graph.nodes[p as usize];
        if n.inputs.len() != 3 {
            return Err("a header phi with other than two values");
        }
        let (entry, back) = (n.inputs[entry_slot], n.inputs[back_slot]);
        if entry == NO_NODE || back == NO_NODE {
            return Err("an undefined phi input");
        }
        if variant.contains(&entry) {
            return Err("a phi entry value that varies in the loop");
        }
        if back == p || back == entry {
            kinds.insert(p, Carried::Invariant { entry });
            continue;
        }
        let ty = n.ty;
        if !matches!(ty, IrType::Int | IrType::Long) {
            return Err("a carried value that is neither int nor long");
        }
        let b = graph.node_opt(back).ok_or("a dangling back value")?;
        if b.ty != ty || b.inputs.len() != 2 {
            return Err("a carried value that is not linear");
        }
        let (l, r) = (b.inputs[0], b.inputs[1]);
        let (step, neg) = match b.op {
            Op::Add if l == p && r != p => (r, false),
            Op::Add if r == p && l != p => (l, false),
            Op::Sub if l == p && r != p => (r, true),
            _ => return Err("a carried value that is not linear"),
        };
        if step == NO_NODE {
            return Err("a step that varies in the loop");
        }
        if variant.contains(&step) {
            let sign = if neg { -1 } else { 1 };
            let Some(affine) = match_affine_iv(
                graph,
                &variant,
                &carried_set,
                p,
                ty,
                entry,
                back,
                step,
                sign,
            ) else {
                return Err("a step that varies in the loop");
            };
            kinds.insert(p, affine);
            continue;
        }
        if graph.node_opt(step).map(|s| s.ty) != Some(ty) {
            return Err("a step of another type");
        }
        kinds.insert(
            p,
            Carried::Linear {
                entry,
                step,
                neg,
                next: back,
            },
        );
    }

    // ── The induction variable and its bound ──
    let (iv, bound, iv_left) = match (carried_set.contains(&ca), carried_set.contains(&cb)) {
        (true, false) => (ca, cb, true),
        (false, true) => (cb, ca, false),
        _ => return Err("the test does not compare one header phi with an invariant"),
    };
    let (iv_init, stride) = match kinds.get(&iv).copied() {
        Some(Carried::Linear {
            entry, step, neg, ..
        }) if graph.nodes[iv as usize].ty == IrType::Int => {
            let c = const_i64(graph, step).ok_or("the induction step is not a constant")?;
            let s = if neg {
                c.checked_neg().ok_or("the induction step is not 1 or -1")?
            } else {
                c
            };
            if s != 1 && s != -1 {
                return Err("the induction step is not 1 or -1");
            }
            (entry, s)
        }
        _ => return Err("the tested phi is not an int induction variable"),
    };
    if variant.contains(&bound) || graph.node_opt(bound).map(|n| n.ty) != Some(IrType::Int) {
        return Err("the bound varies in the loop or is not an int");
    }
    // The CONTINUE condition, stated as `iv <op> bound`.
    let mut cont = if back_is_true {
        cmp_op
    } else {
        cmp_op.negate()
    };
    if !iv_left {
        cont = cont.swapped();
    }
    // JIT r22 wave 2 (LOOPS22-3, `CRATONVM_JIT_IR_LOOP_RETIRE_GUARDED_BOUNDS`):
    // an inclusive bound (`i <= n` / `i >= n`) behind a pre-header guard that
    // the bound is not the width's extreme, and a `!=` test, which always
    // terminates (the index wraps until it meets the bound).
    let guarded_bounds = guarded_bounds_enabled();
    let form = match (stride, cont) {
        (1, CmpOp::Lt) | (-1, CmpOp::Gt) => TestForm::Strict,
        (1, CmpOp::Le) | (-1, CmpOp::Ge) if guarded_bounds => TestForm::Inclusive,
        (_, CmpOp::Ne) if guarded_bounds => TestForm::NotEqual,
        _ => return Err("the exit test is not a strict bound in the stride's direction"),
    };
    for k in kinds.values() {
        if let Carried::AffineIv {
            cand_iv,
            i2l_inner_offset,
            ..
        } = *k
        {
            if cand_iv != iv || stride != 1 || form != TestForm::Strict {
                return Err("an affine induction accumulator on a non-unit strict loop");
            }
            if let Some(c) = i2l_inner_offset {
                if !i2l_offset_no_wrap(graph, iv_init, stride, cont, c) {
                    return Err("an I2L(iv + c) offset may wrap 32-bit int");
                }
            }
        }
    }
    // The inclusive form's guard, `bound != MAX_VALUE` (`!= MIN_VALUE` for a
    // down-count). Not needed for a constant bound off the extreme; refused at
    // a header the de-spec registry withdrew (the guard would trap again).
    let extreme: i64 = if stride == 1 {
        i64::from(i32::MAX)
    } else {
        i64::from(i32::MIN)
    };
    let needs_guard = form == TestForm::Inclusive && const_i64(graph, bound).is_none();
    if form == TestForm::Inclusive && const_i64(graph, bound) == Some(extreme) {
        return Err("an inclusive bound at the width's extreme never exits");
    }
    if needs_guard
        && despec.is_some_and(|(reg, key)| {
            u32::try_from(header_bci).is_ok_and(|bci| reg.contains(key, bci))
        })
    {
        return Err("a de-specialised header (the bound guard trapped)");
    }
    // JIT r22 wave 3 (`r22-review-w3-throws-on-loops-FIXED-20261020.md` item 1): the
    // bound guard must run before everything the loop's exit carried, and it
    // cannot be ordered by an edge (a `Guard` has no memory output; impure
    // nodes in one block keep their id order, and the guard is the newest
    // node). The rewrite moves every node hung on the exit projection into the
    // pre-header block, so with a guard the exit may carry only what has no
    // effect to misorder: pure values, the `Return`, a join (`Merge` /
    // `Region` of two or more inputs: a block of its own, after the
    // pre-header's). An `If` is refused too: a later fold of its branch would
    // move its arm's effects into the pre-header block.
    if needs_guard {
        let ordered_after = |u: NodeId| {
            let n = &graph.nodes[u as usize];
            match n.op {
                Op::Return => true,
                Op::Merge | Op::Region => n.inputs.len() >= 2,
                _ => !n.op.is_control() && n.op.is_pure(),
            }
        };
        if !users[exit_ctrl as usize].iter().all(|&u| ordered_after(u)) {
            return Err("the loop's exit carries an effect the bound guard must precede");
        }
        // Item 2: a spliced loop's guard would trap under a combined-buffer pc,
        // which is not the (method, bci) the de-spec registry charges, so the
        // refusal above could never match and every recompile would plant it
        // again. The guarded form stays flat-only.
        if crate::ir::splice_resume_map_this_build()
            .is_some_and(|map| map.resume_bci(header_bci) != Some(header_bci))
        {
            return Err("a bound guard inside a splice (no de-spec key for it)");
        }
    }

    // ── The per-trip values: the compare and each linear φ's add, used by
    // nothing outside the loop ──
    let mut gone: FxHashSet<NodeId> = FxHashSet::default();
    gone.insert(cond);
    for k in kinds.values() {
        match *k {
            Carried::Linear { next, .. } => {
                gone.insert(next);
            }
            Carried::AffineIv {
                next, extra_gone, ..
            } => {
                gone.insert(next);
                for g in extra_gone {
                    if g != NO_NODE {
                        gone.insert(g);
                    }
                }
            }
            Carried::Invariant { .. } => {}
        }
    }
    for &d in &gone {
        for &u in &users[d as usize] {
            if !(gone.contains(&u) || carried_set.contains(&u) || u == if_node) {
                return Err("a per-trip value is used outside the loop");
            }
        }
    }
    let mut killed: FxHashSet<NodeId> = gone.clone();
    killed.extend(carried.iter().copied());
    killed.extend([region, if_node, back_ctrl, exit_ctrl]);

    let stranded = frames_after_retire(
        graph,
        &users,
        &killed,
        &gone,
        &carried_set,
        header_bci,
        exit_ctrl,
        spliced_retire_enabled(),
    )?;

    // ── Commit. First the inclusive form's guard (LOOPS22-3), which can still
    // refuse: `Guard{bci: header}(bound != extreme)` with the pre-header frame
    // (the header's snapshot at its loop-entry values), so a failing guard
    // resumes the interpreter at the header and the loop runs there. Its frame
    // names no loop value (`preheader_snapshot` maps every header phi to its
    // entry input), and a guard naming its own frame at its pc is what
    // `frames_after_retire` lets a pre-loop node be. ──
    if needs_guard {
        let fwd = entry_slot - 1;
        let si =
            super::loop_field_forward::preheader_snapshot(graph, region, fwd, header_bci, &killed)
                .ok_or("no pre-header frame for the bound guard")?;
        let lim = graph.add(Op::Const(extreme), IrType::Int, vec![], None);
        let ne = graph.add(
            Op::Cmp(CmpOp::Ne),
            IrType::Int,
            vec![bound, lim],
            Some(header_bci),
        );
        let guard = graph.add(
            Op::Guard { bci: header_bci },
            IrType::Void,
            vec![entry_pred, ne],
            Some(header_bci),
        );
        if !graph.set_node_frame_snapshot(guard, si) {
            // Unreachable: the guard's pc is the snapshot's bci.
            graph.kill(guard);
            graph.kill(ne);
            return Err("a bound guard without its frame");
        }
    }

    // ── The exit values, in the pre-header (pc-less, pure) ──
    // `bound_eff` is the strict bound the loop's last trip stops at: `n` for
    // `<` / `>` and `!=`, `n + 1` / `n - 1` for the guarded inclusive form
    // (exact: the guard, or the constant, keeps `n` off the extreme).
    let bound_eff = match form {
        TestForm::Inclusive => {
            let one = graph.add(Op::Const(1), IrType::Int, vec![], None);
            let op = if stride == 1 { Op::Add } else { Op::Sub };
            graph.add(op, IrType::Int, vec![bound, one], None)
        }
        TestForm::Strict | TestForm::NotEqual => bound,
    };
    let bound_l = graph.add(Op::I2L, IrType::Long, vec![bound_eff], None);
    let init_l = graph.add(Op::I2L, IrType::Long, vec![iv_init], None);
    let diff = if stride == 1 {
        graph.add(Op::Sub, IrType::Long, vec![bound_l, init_l], None)
    } else {
        graph.add(Op::Sub, IrType::Long, vec![init_l, bound_l], None)
    };
    let trip_l = if form == TestForm::NotEqual {
        // `!=`: the index steps until it EQUALS the bound, wrapping through the
        // width if it starts on the far side, so the trip count is the
        // distance modulo 2^32: the low 32 bits of the exact difference.
        let low = graph.add(Op::Const(0xFFFF_FFFF), IrType::Long, vec![], None);
        graph.add(Op::And, IrType::Long, vec![diff, low], None)
    } else {
        let zero_l = graph.add(Op::Const(0), IrType::Long, vec![], None);
        graph.add(
            Op::ScalarIntrinsic(ScalarOp::MaxL),
            IrType::Long,
            vec![diff, zero_l],
            None,
        )
    };
    let mut trip_i = NO_NODE;
    let mut tri_node = NO_NODE;
    let mut finals: Vec<(NodeId, NodeId)> = Vec::with_capacity(carried.len());
    for &p in &carried {
        let Some(kind) = kinds.get(&p).copied() else {
            continue;
        };
        let fin = match kind {
            Carried::Invariant { entry } => entry,
            // `!=` stops exactly at the bound (`i0 == n` is the zero trip).
            Carried::Linear { .. } if p == iv && form == TestForm::NotEqual => bound,
            Carried::Linear { .. } if p == iv => {
                let sop = if stride == 1 {
                    ScalarOp::MaxI
                } else {
                    ScalarOp::MinI
                };
                graph.add(
                    Op::ScalarIntrinsic(sop),
                    IrType::Int,
                    vec![iv_init, bound_eff],
                    None,
                )
            }
            Carried::Linear {
                entry, step, neg, ..
            } => {
                let ty = graph.nodes[p as usize].ty;
                let trips = if ty == IrType::Long {
                    trip_l
                } else {
                    if trip_i == NO_NODE {
                        trip_i = graph.add(Op::L2I, IrType::Int, vec![trip_l], None);
                    }
                    trip_i
                };
                let prod = graph.add(Op::Mul, ty, vec![trips, step], None);
                let op = if neg { Op::Sub } else { Op::Add };
                graph.add(op, ty, vec![entry, prod], None)
            }
            Carried::AffineIv {
                entry,
                sign,
                offset,
                widen_i2l,
                ..
            } => {
                let ty = graph.nodes[p as usize].ty;
                if tri_node == NO_NODE {
                    let one_l = graph.add(Op::Const(1), IrType::Long, vec![], None);
                    let trip_m1 = graph.add(Op::Sub, IrType::Long, vec![trip_l, one_l], None);
                    let prod_t = graph.add(Op::Mul, IrType::Long, vec![trip_l, trip_m1], None);
                    let sh1 = graph.add(Op::Const(1), IrType::Int, vec![], None);
                    tri_node = graph.add(Op::UShr, IrType::Long, vec![prod_t, sh1], None);
                }
                let base_iv = match offset {
                    Some((inv, off_sign)) => {
                        let inv_l = if graph.nodes[inv as usize].ty == IrType::Long {
                            inv
                        } else if let Some(cv) = const_i64(graph, inv) {
                            graph.add(Op::Const(cv), IrType::Long, vec![], None)
                        } else {
                            graph.add(Op::I2L, IrType::Long, vec![inv], None)
                        };
                        let op = if off_sign >= 0 { Op::Add } else { Op::Sub };
                        graph.add(op, IrType::Long, vec![init_l, inv_l], None)
                    }
                    None => init_l,
                };
                let base_term = graph.add(Op::Mul, IrType::Long, vec![trip_l, base_iv], None);
                let delta_l = graph.add(Op::Add, IrType::Long, vec![base_term, tri_node], None);
                let delta = if ty == IrType::Int && !widen_i2l {
                    graph.add(Op::L2I, IrType::Int, vec![delta_l], None)
                } else {
                    delta_l
                };
                let op = if sign >= 0 { Op::Add } else { Op::Sub };
                graph.add(op, ty, vec![entry, delta], None)
            }
        };
        finals.push((p, fin));
    }
    // Every use of a φ -- post-loop nodes and frames (the header's own
    // included, see the section notes) -- now reads its exit value.
    for &(p, fin) in &finals {
        if fin != p {
            graph.replace_all_uses(p, fin);
        }
    }
    graph.replace_all_uses(exit_ctrl, entry_pred);
    graph.kill(region);
    graph.kill(if_node);
    graph.kill(back_ctrl);
    graph.kill(exit_ctrl);
    for &p in &carried {
        graph.kill(p);
    }
    let mut gone_sorted: Vec<NodeId> = gone.iter().copied().collect();
    gone_sorted.sort_unstable();
    for d in gone_sorted {
        graph.kill(d);
    }
    // A promotion's poll write-back record (`Graph::promoted_cell_writebacks`)
    // keeps naming the dead header: the lowering looks it up by a live
    // header's node only (no header, no poll), and an OSR2-3 silent store,
    // whose record does need it, is an in-loop store this census refused.
    UNROLL_STRANDED_BCIS.with(|c| {
        let mut c = c.borrow_mut();
        c.flat.extend(stranded.flat);
        c.chain.extend(stranded.chain);
    });
    Ok(finals.len())
}

/// The frame obligations of retiring the loop (see the module notes). `Ok`
/// carries the flat bcis and (LOOPS22-2) the chain pcs the rewrite strands
/// (for `UNROLL_STRANDED_BCIS`).
fn frames_after_retire(
    graph: &Graph,
    users: &[Vec<NodeId>],
    killed: &FxHashSet<NodeId>,
    gone: &FxHashSet<NodeId>,
    carried: &FxHashSet<NodeId>,
    header_bci: usize,
    exit_ctrl: NodeId,
    spliced: bool,
) -> Result<StrandedFrames, &'static str> {
    for &g in gone {
        match graph.node_opt(g) {
            Some(n) if crate::ir_lower::node_cannot_deopt(graph, n) => {}
            _ => return Err("a per-trip value can deopt"),
        }
    }
    let splices = crate::ir::splice_resume_map_this_build();
    // The loop's pc range: every pc a retired node carries.
    let mut lo = usize::MAX;
    let mut hi = 0usize;
    for &k in killed {
        if let Some(pc) = graph.node_opt(k).and_then(|n| n.bytecode_pc) {
            if let Some(map) = splices.as_ref() {
                if !spliced && map.resume_bci(pc) != Some(pc) {
                    return Err("a spliced loop");
                }
            }
            lo = lo.min(pc);
            hi = hi.max(pc);
        }
    }
    let in_range = |pc: usize| lo <= pc && pc <= hi;
    // Control nodes the loop's exit does not dominate: everything before it
    // and the loop itself. The exit projection is not among them: what hangs
    // on it (a promotion's write-back diamond) runs after the loop, and is
    // re-anchored on the loop's entry, after the exit values, by the rewrite.
    let not_after = control_not_dominated_by(graph, users, exit_ctrl);
    let after = |c: NodeId| {
        graph
            .node_opt(c)
            .is_some_and(|n| n.op != Op::Dead && n.op.is_control())
            && !not_after.contains(&c)
    };
    // JIT round 24: every pc a surviving node carries (its own, a guard's
    // and their spliced resumes): where a transfer could resume afterwards.
    let mut surviving_pcs: FxHashSet<usize> = FxHashSet::default();
    for (id, n) in graph.nodes.iter().enumerate() {
        // Cast: arena index to node id.
        let id = id as NodeId;
        if n.op == Op::Dead || killed.contains(&id) {
            continue;
        }
        if n.bytecode_pc.is_none() && !crate::ir_lower::node_cannot_deopt(graph, n) {
            return Err("a pc-less node that can deopt");
        }
        surviving_pcs.extend(n.bytecode_pc);
        if let Op::Guard { bci } = n.op {
            surviving_pcs.insert(bci);
        }
        let post_loop = if n.op.is_control() {
            after(id)
        } else {
            n.inputs.first().copied().is_some_and(&after)
        };
        if let Some(sp) = n
            .frame_snapshot
            .and_then(|si| graph.safepoints.get(si as usize))
        {
            if snapshot_names(sp, gone) {
                return Err("a node names a frame holding a per-trip value");
            }
            if snapshot_names(sp, carried) && !(post_loop && sp.bci == header_bci) {
                return Err("a node outside the loop's exit names a frame of the loop");
            }
        }
        let mut pcs: [Option<usize>; 2] = [n.bytecode_pc, None];
        if let Op::Guard { bci } = n.op {
            pcs[1] = Some(bci);
        }
        for pc in pcs.into_iter().flatten() {
            let resumed = match splices.as_ref() {
                Some(map) => map.resume_bci(pc).ok_or("an unresolvable spliced pc")?,
                None => pc,
            };
            surviving_pcs.insert(resumed);
            for at in [pc, resumed] {
                if !in_range(at) || pc_reads_no_flat_frame(graph, n) {
                    continue;
                }
                if at == header_bci && post_loop {
                    continue;
                }
                // The field forward's pre-header read: its deopts resolve
                // through its own pre-header frame.
                if at == header_bci
                    && matches!(n.op, Op::Load(_))
                    && names_own_frame_at(graph, n, at)
                {
                    continue;
                }
                // LOOPS22-2: a spliced loop's pre-header guard, length and
                // read name their own CHAIN frame at the header pc.
                if spliced
                    && at == header_bci
                    && matches!(n.op, Op::Guard { .. } | Op::ArrayLength | Op::Load(_))
                    && names_own_chain_frame_at(graph, n, at)
                {
                    continue;
                }
                return Err("a surviving node reaches the loop's frames");
            }
        }
    }
    let mut stranded = StrandedFrames::default();
    for sp in graph.safepoints.iter() {
        let names_gone = snapshot_names(sp, gone);
        let names_carried = snapshot_names(sp, carried);
        if !names_gone && !names_carried {
            continue;
        }
        if sp.is_splice_state() && (!spliced || sp.post_call_of != NO_NODE) {
            return Err("a chain frame holds a loop value");
        }
        // JIT round 24 (the LOOPS22-3 close): a flat frame past the retired
        // nodes' pcs that no surviving node resumes at -- javac's trailing
        // `goto` of a top-tested loop holds `i + 1` and every updated
        // accumulator, and carries no node -- is unreachable afterwards like
        // the in-range ones, and joins the same net (`ir_lower` refuses a
        // by-bci transfer at it). A post-loop pc a node carries (a
        // bottom-tested loop's exit, whose frame holds those values as the
        // exit's) still refuses.
        //
        // A spliced loop's trailing CHAIN frame (the callee's own `goto`, at
        // a combined pc past the loop's) joins the chain half the same way:
        // `R22LoopsWave2 spliced` was refused on it ("a frame outside the
        // loop holds a per-trip value", the frame at the pc after `addUp`'s
        // loop holding `i + 1` and `c + k`). A chain frame a post-call value
        // holds was refused above.
        let tail_frame = (!sp.is_splice_state() || spliced)
            && sp.bci > header_bci
            && !surviving_pcs.contains(&sp.bci);
        if names_gone && !in_range(sp.bci) && tail_frame {
            let set = if sp.is_splice_state() {
                &mut stranded.chain
            } else {
                &mut stranded.flat
            };
            if !set.contains(&sp.bci) {
                set.push(sp.bci);
            }
            continue;
        }
        if names_gone && (!in_range(sp.bci) || sp.bci == header_bci) {
            if loop_pass_report_enabled() {
                let held: Vec<NodeId> = sp
                    .locals
                    .iter()
                    .chain(sp.stack.iter())
                    .copied()
                    .filter(|s| gone.contains(s))
                    .collect();
                eprintln!(
                    "[ir] loop retire: header bci {header_bci}: the frame at bci {} (loop pcs \
                     {lo}..={hi}, splice {}, post-call {}) holds per-trip value(s) {held:?}",
                    sp.bci,
                    sp.is_splice_state(),
                    sp.post_call_only,
                );
            }
            return Err("a frame outside the loop holds a per-trip value");
        }
        if !in_range(sp.bci) || sp.bci == header_bci {
            continue;
        }
        // LOOPS22-2: a chain snapshot's net is the chain half of the channel,
        // keyed by the combined pc its publishing site is lowered at
        // (`ir_lower` refuses a compile that publishes one of these).
        let set = if sp.is_splice_state() {
            &mut stranded.chain
        } else {
            &mut stranded.flat
        };
        if !set.contains(&sp.bci) {
            set.push(sp.bci);
        }
    }
    Ok(stranded)
}

#[cfg(test)]
#[path = "loop_retire_tests.rs"]
mod tests;
