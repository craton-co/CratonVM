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
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_LOOP_RETIRE_ACCUMULATORS")
}

/// `CRATONVM_JIT_IR_LOOP_RETIRE_GUARDED_BOUNDS` (JIT round 22 wave 2, lane
/// loops, LOOPS22-3): **default ON** since the round-24 close (perf, `=0`
/// kills; inert without [`loop_retire_enabled`]; `R22LoopsWave2` inclusive-*
/// and not-equal-* 480-650 -> 7-14 ns, both modes, facts equal HotSpot's). Also retire `i <= n` / `i >= n` behind a
/// pre-header `Guard(n != MAX_VALUE / MIN_VALUE)` with the loop-entry frame,
/// and `i != n` (always finite: the index wraps until it meets `n`, a trip
/// count of `(n - i0) mod 2^32`). Read per candidate loop (no `static`).
pub(super) fn guarded_bounds_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_LOOP_RETIRE_GUARDED_BOUNDS")
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
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_LOOP_RETIRE_SPLICED")
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
    let rin: Vec<NodeId> = graph.nodes[region as usize].inputs.iter().copied().collect();
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
        if step == NO_NODE || variant.contains(&step) {
            return Err("a step that varies in the loop");
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
    let mut cont = if back_is_true { cmp_op } else { cmp_op.negate() };
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
        if let Carried::Linear { next, .. } = *k {
            gone.insert(next);
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
        let si = super::loop_field_forward::preheader_snapshot(
            graph, region, fwd, header_bci, &killed,
        )
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
mod tests {
    use super::*;

    /// The nodes of [`counted`] a test reads back.
    struct Loop {
        g: Graph,
        entry: NodeId,
        header: NodeId,
        iv: NodeId,
        acc: NodeId,
        test: NodeId,
        ret: NodeId,
    }

    /// `int c = 0; for (int i = 0; i < n; i++) c += k; return c;` by hand:
    /// a `Merge` header at pc 2, `If(Cmp(cc, i, n))` at pc 5 whose `Proj(0)` is
    /// the back edge, the two adds at pc 8, `Return(exit, c)` at pc 12 and the
    /// header's flat snapshot naming `n`, `k`, `i`, `c`. `acc_next` builds the
    /// accumulator's back value from `(acc, iv, k)`.
    fn counted(
        cc: CmpOp,
        acc_next: impl FnOnce(&mut Graph, NodeId, NodeId, NodeId) -> NodeId,
    ) -> Loop {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let n = g.add(Op::Param(0), IrType::Int, vec![start], None);
        let k = g.add(Op::Param(1), IrType::Int, vec![start], None);
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let header = g.add(Op::Merge, IrType::Control, vec![entry, NO_NODE], Some(2));
        let iv = g.add(Op::Phi, IrType::Int, vec![header, zero, NO_NODE], Some(2));
        let acc = g.add(Op::Phi, IrType::Int, vec![header, zero, NO_NODE], Some(2));
        let test = g.add(Op::Cmp(cc), IrType::Int, vec![iv, n], Some(5));
        let iff = g.add(Op::If, IrType::Control, vec![header, test], Some(5));
        let back = g.add(Op::Proj(0), IrType::Control, vec![iff], Some(5));
        let exit = g.add(Op::Proj(1), IrType::Control, vec![iff], Some(5));
        let next = g.add(Op::Add, IrType::Int, vec![iv, one], Some(8));
        let acc_back = acc_next(&mut g, acc, iv, k);
        assert!(g.set_input(header, 1, back));
        assert!(g.set_input(iv, 2, next));
        assert!(g.set_input(acc, 2, acc_back));
        let ret = g.add(Op::Return, IrType::Void, vec![exit, acc], Some(12));
        g.exit = ret;
        g.push_safepoint(SafepointSnapshot::new(2, vec![n, k, iv, acc], vec![], vec![]));
        Loop {
            g,
            entry,
            header,
            iv,
            acc,
            test,
            ret,
        }
    }

    fn add_k(g: &mut Graph, acc: NodeId, _iv: NodeId, k: NodeId) -> NodeId {
        g.add(Op::Add, IrType::Int, vec![acc, k], Some(8))
    }

    #[test]
    fn r22_loops_an_accumulator_loop_is_replaced_by_its_exit_values() {
        let _ = super::super::take_unroll_stranded_bcis();
        let mut l = counted(CmpOp::Lt, add_k);
        assert_eq!(retire_accumulator_loops(&mut l.g, None), 1);
        for dead in [l.header, l.iv, l.acc, l.test] {
            assert_eq!(l.g.nodes[dead as usize].op, Op::Dead, "n{dead}");
        }
        let ret = &l.g.nodes[l.ret as usize];
        assert_eq!(ret.inputs[0], l.entry, "the return hangs on the loop's entry");
        // `c0 + (int) max(0L, (long) n - (long) 0) * k`
        let fin = &l.g.nodes[ret.inputs[1] as usize];
        assert_eq!(fin.op, Op::Add);
        let prod = &l.g.nodes[fin.inputs[1] as usize];
        assert_eq!(prod.op, Op::Mul);
        let trips = &l.g.nodes[prod.inputs[0] as usize];
        assert_eq!(trips.op, Op::L2I);
        assert_eq!(
            l.g.nodes[trips.inputs[0] as usize].op,
            Op::ScalarIntrinsic(ScalarOp::MaxL)
        );
        // The header frame names the exit values, never a dead node.
        let sp = &l.g.safepoints[0];
        assert!(sp.locals.iter().all(|&v| l.g.nodes[v as usize].op != Op::Dead));
        assert_eq!(sp.locals[3], ret.inputs[1]);
        assert_eq!(
            l.g.nodes[sp.locals[2] as usize].op,
            Op::ScalarIntrinsic(ScalarOp::MaxI)
        );
    }

    /// JIT round 24: javac's top-tested loop ends in a `goto` past every
    /// retired node's pc, and its frame holds `i + 1` and `c + k`. No node
    /// carries that pc, so the loop is retired and the pc joins the stranded
    /// net; a node at that pc (a post-loop transfer could resume there) still
    /// refuses.
    #[test]
    fn r24_a_frame_at_the_trailing_goto_is_stranded_not_refused() {
        let _ = super::super::take_unroll_stranded_bcis();
        let mut l = counted(CmpOp::Lt, add_k);
        let (iv_next, acc_next) = (
            l.g.nodes[l.iv as usize].inputs[2],
            l.g.nodes[l.acc as usize].inputs[2],
        );
        let (n, k) = (l.g.safepoints[0].locals[0], l.g.safepoints[0].locals[1]);
        l.g.push_safepoint(SafepointSnapshot::new(
            10,
            vec![n, k, iv_next, acc_next],
            vec![],
            vec![],
        ));
        assert_eq!(retire_accumulator_loops(&mut l.g, None), 1);
        assert!(super::super::take_unroll_stranded_bcis().contains(&10));
        // The same frame with a surviving node at its pc: refused.
        let mut l = counted(CmpOp::Lt, add_k);
        let (iv_next, acc_next) = (
            l.g.nodes[l.iv as usize].inputs[2],
            l.g.nodes[l.acc as usize].inputs[2],
        );
        let (n, k) = (l.g.safepoints[0].locals[0], l.g.safepoints[0].locals[1]);
        l.g.push_safepoint(SafepointSnapshot::new(
            12,
            vec![n, k, iv_next, acc_next],
            vec![],
            vec![],
        ));
        assert_eq!(
            retire_accumulator_loops(&mut l.g, None),
            0,
            "the return carries pc 12"
        );
    }

    #[test]
    fn r22_loops_a_non_linear_accumulator_keeps_its_loop() {
        let mut l = counted(CmpOp::Lt, |g, acc, iv, _k| {
            let x = g.add(Op::Xor, IrType::Int, vec![iv, acc], Some(8));
            g.add(Op::Add, IrType::Int, vec![acc, x], Some(8))
        });
        assert_eq!(retire_accumulator_loops(&mut l.g, None), 0);
        assert_eq!(l.g.nodes[l.header as usize].op, Op::Merge);
        assert_eq!(l.g.nodes[l.acc as usize].op, Op::Phi);
    }

    /// `i <= n` never ends for `n == Integer.MAX_VALUE`: not retired without
    /// the guarded-bounds switch (default ON since the round-24 close; with it
    /// the loop is retired behind its guard, below).
    #[test]
    fn r22_loops_an_inclusive_bound_keeps_its_loop() {
        let mut l = counted(CmpOp::Le, add_k);
        let retired = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_LOOP_RETIRE_GUARDED_BOUNDS", Some("0"))],
            || retire_accumulator_loops(&mut l.g, None),
        );
        assert_eq!(retired, 0);
        assert_eq!(l.g.nodes[l.header as usize].op, Op::Merge);
    }

    /// A loop whose test runs the wrong way for its stride (`i > n` with
    /// `i++`) is no counted loop of this shape.
    #[test]
    fn r22_loops_a_test_against_the_stride_keeps_its_loop() {
        let mut l = counted(CmpOp::Gt, add_k);
        assert_eq!(retire_accumulator_loops(&mut l.g, None), 0);
        assert_eq!(l.g.nodes[l.iv as usize].op, Op::Phi);
    }

    const GUARDED: &str = "CRATONVM_JIT_IR_LOOP_RETIRE_GUARDED_BOUNDS";

    /// LOOPS22-3: `i <= n` is retired behind `Guard(n != MAX_VALUE)`, whose
    /// frame is the header's at its loop-entry values (`i = 0`, `c = 0`).
    #[test]
    fn r22_loops_an_inclusive_bound_is_retired_behind_a_guard() {
        let mut l = counted(CmpOp::Le, add_k);
        let retired = cratonvm_types::flags::with_thread_overrides(&[(GUARDED, Some("1"))], || {
            retire_accumulator_loops(&mut l.g, None)
        });
        assert_eq!(retired, 1);
        assert_eq!(l.g.nodes[l.header as usize].op, Op::Dead);
        let guard = l
            .g
            .nodes
            .iter()
            .position(|n| matches!(n.op, Op::Guard { bci: 2 }))
            .expect("a bound guard");
        let g = &l.g.nodes[guard];
        let cond = &l.g.nodes[g.inputs[1] as usize];
        assert_eq!(cond.op, Op::Cmp(CmpOp::Ne));
        assert_eq!(l.g.nodes[cond.inputs[1] as usize].op, Op::Const(i64::from(i32::MAX)));
        let sp = &l.g.safepoints[g.frame_snapshot.expect("its own frame") as usize];
        assert_eq!(sp.bci, 2);
        assert!(sp.locals[2..].iter().all(|&v| l.g.nodes[v as usize].op == Op::Const(0)));
    }

    /// LOOPS22-3: `i != n` is retired with no guard; its trip count is the
    /// distance modulo 2^32 and the index ends at `n`.
    #[test]
    fn r22_loops_a_not_equal_test_is_retired_without_a_guard() {
        let mut l = counted(CmpOp::Ne, add_k);
        let retired = cratonvm_types::flags::with_thread_overrides(&[(GUARDED, Some("1"))], || {
            retire_accumulator_loops(&mut l.g, None)
        });
        assert_eq!(retired, 1);
        assert!(!l.g.nodes.iter().any(|n| matches!(n.op, Op::Guard { .. })));
        let sp = &l.g.safepoints[0];
        // The index's exit value is the bound itself (`n`, local 0).
        assert_eq!(sp.locals[2], sp.locals[0]);
        let fin = &l.g.nodes[l.g.nodes[l.ret as usize].inputs[1] as usize];
        let prod = &l.g.nodes[fin.inputs[1] as usize];
        let trips = &l.g.nodes[prod.inputs[0] as usize];
        assert_eq!(l.g.nodes[trips.inputs[0] as usize].op, Op::And);
    }

    /// Wave 3 (`r22-review-w3-throws-on-loops-FIXED-20261020.md` item 1): a guarded
    /// inclusive loop whose exit carries a store is refused -- the store would
    /// be moved into the pre-header block ahead of the bound guard -- while the
    /// strict form of the same loop (no guard) is still retired.
    #[test]
    fn r22_loops_a_guarded_loop_whose_exit_stores_is_refused() {
        for (cc, want) in [(CmpOp::Le, 0usize), (CmpOp::Lt, 1usize)] {
            let mut l = counted(cc, add_k);
            let exit = l.g.nodes[l.ret as usize].inputs[0];
            let start = l.g.entry;
            let mem = l.g.add(Op::Proj(1), IrType::Memory, vec![start], None);
            let arr = l.g.add(Op::Param(2), IrType::Ref, vec![start], None);
            let zero = l.g.add(Op::Const(0), IrType::Int, vec![], None);
            l.g.add(
                Op::ArrayStore(crate::ir::MemKind::Int),
                IrType::Void,
                vec![exit, mem, arr, zero, l.acc],
                Some(11),
            );
            let retired = cratonvm_types::flags::with_thread_overrides(&[(GUARDED, Some("1"))], || {
                retire_accumulator_loops(&mut l.g, None)
            });
            assert_eq!(retired, want, "{cc:?}");
            assert!(!l.g.nodes.iter().any(|n| matches!(n.op, Op::Guard { .. })), "{cc:?}");
        }
    }

    /// Without the wave-2 switch both keep their loops (the wave-1 answer).
    #[test]
    fn r22_loops_the_guarded_forms_need_their_switch() {
        for cc in [CmpOp::Le, CmpOp::Ne] {
            let mut l = counted(cc, add_k);
            let retired =
                cratonvm_types::flags::with_thread_overrides(&[(GUARDED, Some("0"))], || {
                    retire_accumulator_loops(&mut l.g, None)
                });
            assert_eq!(retired, 0, "{cc:?}");
        }
    }

    /// LOOPS22-2's net: an in-loop CHAIN snapshot naming a carried value is
    /// refused without `CRATONVM_JIT_IR_LOOP_RETIRE_SPLICED`; with it, the
    /// loop is retired and the snapshot's combined pc joins the channel's
    /// chain half (the flat half stays empty), which `ir_lower` checks against
    /// the pcs it publishes a chain at.
    #[test]
    fn r22_loops_a_spliced_chain_frame_joins_the_chain_net() {
        const SPLICED: &str = "CRATONVM_JIT_IR_LOOP_RETIRE_SPLICED";
        for (on, want) in [(Some("0"), 0usize), (Some("1"), 1usize)] {
            let _ = super::super::take_unroll_stranded_frames();
            let mut l = counted(CmpOp::Lt, add_k);
            let mut sp = SafepointSnapshot::new(8, vec![l.iv, l.acc], vec![], vec![]);
            sp.splice_scope = Some(Box::new(crate::ir::SpliceScopeChain { scopes: Vec::new() }));
            l.g.push_safepoint(sp);
            let retired = cratonvm_types::flags::with_thread_overrides(&[(SPLICED, on)], || {
                retire_accumulator_loops(&mut l.g, None)
            });
            assert_eq!(retired, want, "{on:?}");
            let stranded = super::super::take_unroll_stranded_frames();
            if want == 1 {
                assert_eq!(stranded.chain, vec![8]);
                assert!(stranded.flat.is_empty());
            } else {
                assert_eq!(stranded, StrandedFrames::default());
            }
        }
    }

    /// JIT round 24 close (LOOPS22-2 on `R22LoopsWave2 spliced`): a spliced
    /// callee's trailing `goto` CHAIN frame, past the loop's pcs and carried
    /// by no node, holds `i + 1` and `c + k`. With the switch it joins the
    /// chain half like the in-range ones (it was refused, "a frame outside the
    /// loop holds a per-trip value"); at a pc a node carries it still refuses.
    #[test]
    fn r24_a_spliced_trailing_chain_frame_is_stranded() {
        const SPLICED: &str = "CRATONVM_JIT_IR_LOOP_RETIRE_SPLICED";
        for (pc, want) in [(10usize, 1usize), (12, 0)] {
            let _ = super::super::take_unroll_stranded_frames();
            let mut l = counted(CmpOp::Lt, add_k);
            let (iv_next, acc_next) = (
                l.g.nodes[l.iv as usize].inputs[2],
                l.g.nodes[l.acc as usize].inputs[2],
            );
            let mut sp = SafepointSnapshot::new(pc, vec![iv_next, acc_next], vec![], vec![]);
            sp.splice_scope = Some(Box::new(crate::ir::SpliceScopeChain { scopes: Vec::new() }));
            l.g.push_safepoint(sp);
            let retired =
                cratonvm_types::flags::with_thread_overrides(&[(SPLICED, Some("1"))], || {
                    retire_accumulator_loops(&mut l.g, None)
                });
            assert_eq!(retired, want, "pc {pc}");
            let stranded = super::super::take_unroll_stranded_frames();
            if want == 1 {
                assert_eq!(stranded.chain, vec![10]);
                assert!(stranded.flat.is_empty());
            }
        }
    }
}
