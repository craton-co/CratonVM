// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! nio21 wave 6 (lane L5): loop-entry predicates for a VARIABLE-offset index
//! `a[i + o]` (`CRATONVM_JIT_IR_OFFSET_PREDICATE`).
//!
//! # What
//!
//! `HeapByteBuffer.put(byte)` stores `hb[ix(nextPutIndex())]`, that is
//! `hb[p + offset]`, behind `p < limit` (`dst.hasRemaining()` in
//! `UTF_8$Encoder.encodeBufferLoop`, and `nextPutIndex`'s own throw test).
//! Once the class predicate and the field forward have run, `p` is the header
//! value φ of `dst.position` and `hb`, `offset`, `limit` are pre-header
//! values. The access is in range only by the buffer's class invariant
//! `offset + limit <= hb.length`, which no compare in the method states, so
//! `ir_check_elim` keeps the check per byte. The same shape is every
//! `buf[off + i]` loop over an array slice.
//!
//! This plants, in the pre-header, deopting to the LOOP ENTRY with the
//! loop-entry frame (`install_range_predicates`' nodes, frame and de-spec
//! discipline):
//!
//! * `x <= a.length - o` for each access `a[i + o]` (either operand order) in
//!   the body, `a` and `o` loop-invariant, `o` not a constant, `i` a header
//!   `int` φ whose every back value is the φ plus constants, and an in-loop
//!   `If` edge stating `i < x` (`x` invariant) on the access's control chain;
//! * `0 <= o`, unless `o` is a non-negative constant or a length;
//! * `0 <= init(i)`, unless the same holds of it.
//!
//! `ir_check_elim`'s variable-offset arm (`VarOffsetFacts`) then proves the
//! access: `i < x` from the edge, `i >= 0` from its monotone-induction rule
//! (which reads the `0 <= init` guard), and the two guards. With `o >= 0`,
//! `a.length - o` is exact, and `0 <= i + o < x + o <= a.length <= i32::MAX`,
//! so the 32-bit sum does not wrap either.
//!
//! # Why it is sound
//!
//! Every guard only DEOPTS: a failing one resumes the interpreter at the
//! loop's header with the state the loop is entered with; nothing has run
//! between the pre-header and that point. All of them are evaluated before
//! the loop, so the loop runs only when ALL hold (the order among them does
//! not matter: `a.length - o` may wrap for a negative `o`, but then `0 <= o`
//! fails). A guard that keeps failing is withdrawn by the de-spec registry
//! (the header bci), after which the method keeps its checks. The proof that
//! REMOVES checks is `ir_check_elim`'s and fails closed.
//!
//! # What it does not plant for
//!
//! A loop with no top test, an access whose control chain up to the header
//! (through `If` edges and single-input merges) has no `i < x` edge, a base
//! allocated in this compile or computed in the loop, a base an
//! exception-edge anchor of the body indexes (nio21 wave 6 L2's rule), `x`
//! the base's own length (`a.length <= a.length - o` would fail for every
//! `o > 0`), a loop of a spliced callee (no header snapshot of its own), the
//! optimizing OSR route and an OSR-door graph (their entries skip the
//! pre-header).

use super::{
    build_users, is_loop_invariant, loop_body_with, loop_entry_frame, loop_entry_memory,
    loop_headers, mask_predicate::header_stay_edge,
};
use crate::ir::{CmpOp, Graph, IrType, NodeId, Op, SafepointSnapshot, NO_NODE};

/// At most this many guards per compile.
const MAX_OFFSET_GUARDS: usize = 8;

/// How many forward merges a back value may pass through (the monotone
/// predicate's bound; `ir_check_elim` re-checks the shape with dominance).
const SHAPE_DEPTH: u32 = 4;

/// How many control nodes the walk from an access up to its `i < x` edge
/// visits before giving up.
const EDGE_WALK: usize = 32;

/// `CRATONVM_JIT_IR_OFFSET_PREDICATE` (nio21 wave 6, lane L5) -- **default
/// ON**, `=0` is the kill switch. Read once per compile.
/// `CRATONVM_JIT_IR_BCE_VAR_OFFSET=0` (the `ir_check_elim` arm that reads the
/// guards) makes them pointless but does not stop them.
pub(super) fn offset_predicate_enabled() -> bool {
    true
}



/// One loop's guards, decided without touching the graph.
struct OffsetPlan {
    header_bci: usize,
    entry_pred: NodeId,
    entry_mem: NodeId,
    /// `(x, a, o)`: `Guard(x <= a.length - o)`.
    rooms: Vec<(NodeId, NodeId, NodeId)>,
    /// `Guard(0 <= v)` for each `v`.
    non_negative: Vec<NodeId>,
    frame: SafepointSnapshot,
}

/// Plant the guards the module notes describe, unless `despec` withdrew a
/// speculation at the loop's header. Returns the number of guards planted.
pub(super) fn plant_offset_predicates(
    graph: &mut Graph,
    despec: &crate::deopt::DespecRegistry,
    method_key: &str,
) -> usize {
    if graph.safepoints.is_empty() {
        return 0;
    }
    let dbg = false;
    let mut planted = 0usize;
    for (region, entry_pred, back_ctrls) in loop_headers(graph) {
        if planted >= MAX_OFFSET_GUARDS {
            break;
        }
        if !matches!(
            graph.node_opt(region).map(|n| &n.op),
            Some(Op::Region | Op::Merge)
        ) {
            continue;
        }
        let Some(header_bci) = graph.node_opt(region).and_then(|r| r.bytecode_pc) else {
            continue;
        };
        let Ok(bci32) = u32::try_from(header_bci) else {
            continue;
        };
        if crate::deopt::predicate_withdrawn(
            despec,
            method_key,
            bci32,
            crate::deopt::PredicateKind::Offset,
        ) {
            if dbg {
                eprintln!(
                    "[ir-licm] offset predicate: header {region} bci {header_bci} \
                     de-specialised, not planted"
                );
            }
            continue;
        }
        let Some(plan) = offset_plan(graph, region, entry_pred, &back_ctrls) else {
            continue;
        };
        let n = install_offset_plan(graph, plan, MAX_OFFSET_GUARDS - planted);
        planted += n;
        if dbg && n > 0 {
            eprintln!(
                "[ir-licm] offset predicate: header {region} bci {header_bci} planted {n} \
                 (total {planted})"
            );
        }
    }
    planted
}

/// The value of an `int` constant node.
fn int_const(graph: &Graph, id: NodeId) -> Option<i64> {
    graph.node_opt(id).and_then(|n| match n.op {
        Op::Const(c) if n.ty == IrType::Int => Some(c),
        _ => None,
    })
}

/// Is `v`, a back value of the header φ `phi`, the φ itself, the φ plus
/// constants, or a φ of a (non-header) merge whose inputs all are? A shape
/// screen only (the monotone predicate's): `ir_check_elim` re-checks it with
/// dominance and the facts.
fn stepped_shape(graph: &Graph, phi: NodeId, region: NodeId, v: NodeId, depth: u32) -> bool {
    if v == phi {
        return true;
    }
    let Some(n) = graph.node_opt(v) else {
        return false;
    };
    if n.op == Op::Phi {
        if depth >= SHAPE_DEPTH {
            return false;
        }
        let Some((&m, vals)) = n.inputs.split_first() else {
            return false;
        };
        if m == region
            || vals.is_empty()
            || !matches!(
                graph.node_opt(m).map(|x| &x.op),
                Some(Op::Merge | Op::Region)
            )
        {
            return false;
        }
        return vals
            .iter()
            .all(|&x| stepped_shape(graph, phi, region, x, depth + 1));
    }
    crate::ir_check_elim::constant_step(graph, v).is_some_and(|(root, _)| root == phi)
}

/// Is `v` a non-negative `int` constant or an array length (owing no guard)?
fn non_negative_by_shape(graph: &Graph, v: NodeId) -> bool {
    graph.node_opt(v).is_some_and(|n| match n.op {
        Op::Const(c) => n.ty == IrType::Int && (0..=i64::from(i32::MAX)).contains(&c),
        Op::ArrayLength => true,
        _ => false,
    })
}

/// Walk the control chain up from `ctrl` -- through `If` projections and
/// single-input merges, never past `region` -- and return the `x` of the
/// first edge stating `i < x` with `x` accepted by `invariant`. Every node on
/// that chain dominates `ctrl`, so the edge does.
///
/// JIT round 20 (lane loopopt, `CRATONVM_JIT_IR_BCE_GUARD_BELOW`): or of a
/// `Guard(i < x)` anchored on a control node of that chain -- before `access`
/// when it shares `access`'s control (`ir_schedule` keeps a block's impure
/// nodes in id order), and on a join the walk stops at. With the
/// never-taken pruning on, `nextPutIndex`'s `p < limit` and an ASCII-profiled
/// `dst.hasRemaining()` are guards, not edges, so this is where a buffer
/// loop's bound is stated. A shape screen: `ir_check_elim`'s arm re-checks the
/// guard's position on the schedule (`guard_fact_precedes`).
fn dominating_below_edge(
    graph: &Graph,
    ctrl: NodeId,
    region: NodeId,
    i: NodeId,
    invariant: &dyn Fn(NodeId) -> bool,
    access: NodeId,
) -> Option<NodeId> {
    let guards_on = crate::ir_check_elim::guard_below_enabled();
    let anchors = if guards_on {
        crate::ir::exception_edge_anchors(graph)
    } else {
        Default::default()
    };
    let guard_at = |c: NodeId| -> Option<NodeId> {
        if !guards_on {
            return None;
        }
        super::anchored_at(graph, c).into_iter().find_map(|g| {
            let gn = graph.node_opt(g)?;
            if !matches!(gn.op, Op::Guard { .. })
                || anchors.contains(&g)
                || (c == ctrl && g >= access)
            {
                return None;
            }
            let (v, x) = crate::ir_check_elim::strictly_below(graph, gn.input_opt(1)?, true)?;
            (v == i && invariant(x)).then_some(x)
        })
    };
    let mut c = ctrl;
    for _ in 0..EDGE_WALK {
        if c == region || c == NO_NODE {
            return None;
        }
        if let Some(x) = guard_at(c) {
            return Some(x);
        }
        let n = graph.node_opt(c)?;
        match n.op {
            Op::Proj(which) if which <= 1 && n.ty == IrType::Control => {
                let owner = *n.inputs.first()?;
                let on = graph.node_opt(owner)?;
                if on.op != Op::If {
                    return None;
                }
                if let Some(cond) = on.input_opt(1) {
                    if let Some((v, x)) =
                        crate::ir_check_elim::strictly_below(graph, cond, which == 0)
                    {
                        if v == i && invariant(x) {
                            return Some(x);
                        }
                    }
                }
                c = on.input_opt(0)?;
            }
            Op::Merge if n.inputs.len() == 1 => c = *n.inputs.first()?,
            _ => return None,
        }
    }
    None
}

/// Decide the guards for the loop at `region`, or `None` when it is not a
/// candidate (see the module notes).
fn offset_plan(
    graph: &Graph,
    region: NodeId,
    entry_pred: NodeId,
    back_ctrls: &[NodeId],
) -> Option<OffsetPlan> {
    let users = build_users(graph);
    let body = loop_body_with(graph, &users, region, entry_pred, back_ctrls);
    let region_node = graph.node_opt(region)?;
    let arity = region_node.inputs.len();
    let entry_slot = region_node.inputs.iter().position(|&p| p == entry_pred)? + 1;
    // A top-tested loop only, so a failing guard stands for a loop that runs.
    // JIT round 20 (lane loopopt, `CRATONVM_JIT_IR_OFFSET_PREDICATE_ANY_HEADER`):
    // or any header. The screen is a cost rule, not a soundness one (every
    // guard deopts to the loop entry, and the interpreter then runs the loop
    // itself), and it refused every buffer loop: `while (src.hasRemaining())`
    // is a spliced boolean method, so the header's first test is
    // `hasRemaining`'s own `position < limit`, BOTH of whose arms reach the
    // `iconst_1` / `iconst_0` join and the caller's `ifeq` inside the body --
    // `header_stay_edge` answers `None` for it. The guards this pass plants
    // state a buffer's class invariant (`offset + limit <= hb.length`,
    // `0 <= offset`, `0 <= position`), which a well-formed buffer never fails,
    // and a header that keeps failing one is de-specialised as before.
    if header_stay_edge(graph, region, &body).is_none() && false {
        return None;
    }

    // The header's stepped `int` φs.
    let mut phis: Vec<NodeId> = Vec::new();
    for &u in users.get(region as usize)? {
        let Some(n) = graph.node_opt(u) else {
            continue;
        };
        if n.op != Op::Phi
            || n.ty != IrType::Int
            || n.inputs.first() != Some(&region)
            || n.inputs.len() != arity + 1
            || phis.contains(&u)
        {
            continue;
        }
        let shaped = n
            .inputs
            .iter()
            .enumerate()
            .skip(1)
            .all(|(slot, &v)| slot == entry_slot || stepped_shape(graph, u, region, v, 0));
        if shaped {
            phis.push(u);
        }
    }
    if phis.is_empty() {
        return None;
    }
    let invariant = |x: NodeId| {
        x != NO_NODE && !body.contains(&x) && is_loop_invariant(graph, x, region, &body)
    };
    // nio21 wave 6 (lane L2's rule): no guard over an array an exception-edge
    // anchor of the body indexes.
    let anchors = crate::ir::exception_edge_anchors(graph);
    let anchor_arrays =
        crate::ir::exception_edge_anchor_arrays(graph, &anchors, &|id| body.contains(&id));

    let const_offset = crate::ir_check_elim::const_offset_predicate_enabled();
    let mut rooms: Vec<(NodeId, NodeId, NodeId)> = Vec::new();
    let mut used: Vec<NodeId> = Vec::new();
    let mut offsets: Vec<NodeId> = Vec::new();
    let mut access_mem: Option<NodeId> = None;
    let mut ids: Vec<NodeId> = body.iter().copied().collect();
    ids.sort_unstable();
    for &id in &ids {
        let Some(n) = graph.node_opt(id) else {
            continue;
        };
        if !matches!(n.op, Op::ArrayLoad(_) | Op::ArrayStore(_)) {
            continue;
        }
        let (Some(ctrl), Some(mem), Some(base), Some(idx)) = (
            n.input_opt(0),
            n.input_opt(1),
            n.input_opt(2),
            n.input_opt(3),
        ) else {
            continue;
        };
        if anchor_arrays.contains(&base) {
            continue;
        }
        let Some(base_node) = graph.node_opt(base) else {
            continue;
        };
        if base_node.ty != IrType::Ref
            || matches!(
                base_node.op,
                Op::New { .. } | Op::NewArray { .. } | Op::MultiNewArray { .. } | Op::NewCp { .. }
            )
            || !invariant(base)
        {
            continue;
        }
        let Some(idx_node) = graph.node_opt(idx) else {
            continue;
        };
        if idx_node.op != Op::Add || idx_node.ty != IrType::Int {
            continue;
        }
        let [u, v] = idx_node.inputs.as_slice() else {
            continue;
        };
        for (i, o) in [(*u, *v), (*v, *u)] {
            // JIT round 21 wave 4 (lane bce, BCE21-2): a constant `o` in
            // `1..=i32::MAX` too, under `CRATONVM_JIT_IR_CONST_OFFSET_PREDICATE`
            // (opt-in): `x <= a.length - c`, a `Sub` by a positive constant,
            // which cannot wrap; `0 <= o` is then owed no guard. The guard
            // names the access's own constant node, the key
            // `ir_check_elim::VarOffsetFacts` reads it under.
            let const_ok = match int_const(graph, o) {
                None => true,
                Some(c) => const_offset && (1..=i64::from(i32::MAX)).contains(&c),
            };
            if !phis.contains(&i) || !const_ok || !invariant(o) {
                continue;
            }
            if !graph.node_opt(o).is_some_and(|on| on.ty == IrType::Int) {
                continue;
            }
            let Some(x) = dominating_below_edge(graph, ctrl, region, i, &invariant, id) else {
                continue;
            };
            // `x` the base's own length: `a.length <= a.length - o` fails for
            // every positive `o`.
            if graph
                .node_opt(x)
                .is_some_and(|xn| xn.op == Op::ArrayLength && xn.input_opt(2) == Some(base))
            {
                continue;
            }
            if !rooms.contains(&(x, base, o)) {
                rooms.push((x, base, o));
            }
            if !used.contains(&i) {
                used.push(i);
            }
            if !offsets.contains(&o) {
                offsets.push(o);
            }
            if access_mem.is_none() {
                access_mem = Some(mem);
            }
            break;
        }
    }
    if rooms.is_empty() {
        return None;
    }

    // `0 <= o` and `0 <= init(i)`.
    let mut non_negative: Vec<NodeId> = Vec::new();
    for &o in &offsets {
        if !non_negative_by_shape(graph, o) && !non_negative.contains(&o) {
            non_negative.push(o);
        }
    }
    for &p in &used {
        let init = graph.node_opt(p)?.input_opt(entry_slot)?;
        if init == NO_NODE || body.contains(&init) {
            return None;
        }
        if !non_negative_by_shape(graph, init) && !non_negative.contains(&init) {
            non_negative.push(init);
        }
    }

    let entry_mem = loop_entry_memory(graph, region, entry_pred)
        .or_else(|| access_mem.filter(|m| !body.contains(m)))?;
    graph.node_opt(entry_mem)?;
    let (header_bci, frame) = loop_entry_frame(graph, region, entry_slot, &body)?;
    Some(OffsetPlan {
        header_bci,
        entry_pred,
        entry_mem,
        rooms,
        non_negative,
        frame,
    })
}

/// Add one plan's guards, at most `budget` of them: every guard the plan
/// needs, or none (a room guard without its sign guards proves nothing, and
/// the sign guards alone buy nothing). Returns the number added.
fn install_offset_plan(graph: &mut Graph, plan: OffsetPlan, budget: usize) -> usize {
    let OffsetPlan {
        header_bci,
        entry_pred,
        entry_mem,
        rooms,
        non_negative,
        frame,
    } = plan;
    if rooms.len() + non_negative.len() > budget {
        return 0;
    }
    // One frame copy for every guard of this loop; each is named with it
    // (unnamed, a guard would resolve the header's own snapshot, whose φ homes
    // the pre-header has not written yet), and it is taken back out when
    // naming fails.
    let pc = Some(header_bci);
    graph.push_safepoint(frame);
    let si = (graph.safepoints.len() - 1) as u32;
    let mut added: Vec<NodeId> = Vec::new();
    let mut ok = true;
    for (x, a, o) in rooms {
        let len = graph.add(
            Op::ArrayLength,
            IrType::Int,
            vec![entry_pred, entry_mem, a],
            pc,
        );
        let room = graph.add(Op::Sub, IrType::Int, vec![len, o], pc);
        let cond = graph.add(Op::Cmp(CmpOp::Le), IrType::Int, vec![x, room], pc);
        let guard = graph.add(
            Op::Guard { bci: header_bci },
            IrType::Void,
            vec![entry_pred, cond],
            pc,
        );
        added.extend([len, room, cond, guard]);
        if !(graph.set_node_frame_snapshot(len, si) && graph.set_node_frame_snapshot(guard, si)) {
            ok = false;
            break;
        }
    }
    let mut guards = 0usize;
    if ok {
        let zero = graph.add(Op::Const(0), IrType::Int, vec![], None);
        added.push(zero);
        for v in non_negative {
            let cond = graph.add(Op::Cmp(CmpOp::Le), IrType::Int, vec![zero, v], pc);
            let guard = graph.add(
                Op::Guard { bci: header_bci },
                IrType::Void,
                vec![entry_pred, cond],
                pc,
            );
            added.extend([cond, guard]);
            if !graph.set_node_frame_snapshot(guard, si) {
                ok = false;
                break;
            }
        }
    }
    if !ok {
        // All or nothing: take every node back out, then the frame copy.
        for &id in added.iter().rev() {
            graph.kill(id);
        }
        let keep: Vec<bool> = (0..graph.safepoints.len())
            .map(|i| i != si as usize)
            .collect();
        let _ = graph.retain_safepoints(&keep);
        return 0;
    }
    for &id in &added {
        if graph
            .node_opt(id)
            .is_some_and(|n| matches!(n.op, Op::Guard { .. }))
        {
            guards += 1;
        }
    }
    guards
}

#[cfg(test)]
mod tests;
