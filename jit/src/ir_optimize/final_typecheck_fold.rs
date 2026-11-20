// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Final-class `InstanceOf` strengthening (`Op::ExactClassIs`) and bounded
//! non-negative induction-variable compare folding.

use crate::ir::{exception_edge_anchors, is_memory_token_slot, CmpOp, Graph, IrType, NodeId, Op};
use crate::ir_schedule::schedule;
use crate::range_analysis::GraphRanges;
use rustc_hash::FxHashSet;

/// Strengthen `Op::InstanceOf` against a loaded `final`, non-array class
/// (`checkcast_exact_target`) into `Op::ExactClassIs { class_id }`.
///
/// Both return `0` on `null` and `1` iff the object's class word equals
/// `class_id`. Any memory-token users of the replaced `InstanceOf` are wired
/// to its incoming memory token (`inputs[1]`).
pub(in crate::ir_optimize) fn strengthen_final_instanceof(graph: &mut Graph) -> usize {
    let anchors: FxHashSet<NodeId> = exception_edge_anchors(graph).into_iter().collect();
    let mut targets: Vec<(NodeId, NodeId, NodeId, u32)> = Vec::new();
    for (id, node) in graph.nodes.iter().enumerate() {
        let nid = id as NodeId;
        if anchors.contains(&nid) || node.ty != IrType::Int {
            continue;
        }
        let Op::InstanceOf {
            name_ptr, name_len, ..
        } = node.op
        else {
            continue;
        };
        let (Some(&mem_in), Some(&obj)) = (node.inputs.get(1), node.inputs.get(2)) else {
            continue;
        };
        if let Some(class_id) = super::plant_null_base_load_predicates::checkcast_exact_target(name_ptr, name_len) {
            targets.push((nid, mem_in, obj, class_id));
        }
    }
    if targets.is_empty() {
        return 0;
    }
    for &(nid, mem_in, obj, class_id) in &targets {
        let mem_users: Vec<(NodeId, usize)> = graph
            .nodes
            .iter()
            .enumerate()
            .flat_map(|(u, un)| {
                (0..un.inputs.len())
                    .filter(move |&k| un.inputs[k] == nid && is_memory_token_slot(un, k))
                    .map(move |k| (u as NodeId, k))
            })
            .collect();
        for (u, k) in mem_users {
            graph.set_input(u, k, mem_in);
        }
        graph.clear_inputs(nid);
        graph.nodes[nid as usize].op = Op::ExactClassIs { class_id };
        graph.push_input(nid, obj);
    }
    targets.len()
}

fn int_const(graph: &Graph, id: NodeId) -> Option<i64> {
    let n = graph.node_opt(id)?;
    let Op::Const(c) = n.op else { return None };
    (n.ty == IrType::Int).then_some(c)
}

fn cmp_proves_lt_any_int(graph: &Graph, cond: NodeId, edge: usize, phi: NodeId) -> bool {
    let Some(c) = graph.node_opt(cond) else {
        return false;
    };
    let Op::Cmp(mut op) = c.op else {
        return false;
    };
    if c.ty != IrType::Int || c.inputs.len() != 2 {
        return false;
    }
    let (a, b) = (c.inputs[0], c.inputs[1]);
    if edge == 1 {
        op = op.negate();
    } else if edge != 0 {
        return false;
    }
    let (Some(an), Some(bn)) = (graph.node_opt(a), graph.node_opt(b)) else {
        return false;
    };
    if an.ty != IrType::Int || bn.ty != IrType::Int {
        return false;
    }
    // `phi < rhs` (or `rhs > phi`) where `rhs` is a signed 32-bit int guarantees
    // `phi <= i32::MAX - 1`, so `phi + 1` cannot overflow signed 32-bit int.
    matches!(
        (op, a == phi, b == phi),
        (CmpOp::Lt, true, _) | (CmpOp::Gt, _, true)
    )
}

/// Find `(CmpNode, constant 0/1)` pairs decided because an `Int` loop-induction
/// `Op::Phi` starts `>= 0` and every back edge adds `1` under a dominating `phi < bound`
/// check (preventing `i32::MAX` wrap-around).
pub(in crate::ir_optimize) fn bounded_nonneg_induction_compares(
    graph: &Graph,
    ranges: &GraphRanges,
) -> Vec<(NodeId, i64)> {
    let has_phi_cmp = graph.nodes.iter().any(|n| {
        matches!(n.op, Op::Cmp(CmpOp::Ge | CmpOp::Gt | CmpOp::Lt | CmpOp::Le))
            && n.ty == IrType::Int
            && n.inputs.len() == 2
            && n.inputs.iter().any(|&x| {
                graph
                    .node_opt(x)
                    .is_some_and(|xn| xn.op == Op::Phi && xn.ty == IrType::Int)
            })
    });
    if !has_phi_cmp {
        return Vec::new();
    }

    let sched = schedule(graph);
    let headers = super::loop_headers(graph);
    let mut nonneg_phis: FxHashSet<NodeId> = FxHashSet::default();

    for (region, entry_pred, back_ctrls) in headers {
        let Some(rn) = graph.node_opt(region) else {
            continue;
        };
        let Some(&region_blk) = sched.node_to_block.get(region as usize) else {
            continue;
        };
        if !sched.dom.is_reachable(region_blk) {
            continue;
        }
        let Some(entry_slot) = rn.inputs.iter().position(|&c| c == entry_pred) else {
            continue;
        };
        if back_ctrls.is_empty() {
            continue;
        }
        // Check each phi attached to `region`.
        for (pid, pnode) in graph.nodes.iter().enumerate() {
            let phi = pid as NodeId;
            if pnode.op != Op::Phi
                || pnode.ty != IrType::Int
                || pnode.input_opt(0) != Some(region)
                || pnode.inputs.len() != rn.inputs.len() + 1
            {
                continue;
            }
            let Some(init) = pnode.input_opt(1 + entry_slot) else {
                continue;
            };
            let init_nonneg = ranges.of(init).lo().is_some_and(|lo| lo >= 0)
                || int_const(graph, init).is_some_and(|c| (0..=i64::from(i32::MAX)).contains(&c));
            if !init_nonneg {
                continue;
            }
            // Collect dominating upper-bound (`phi < rhs`) edge blocks for `phi`.
            let mut guard_blocks: Vec<usize> = Vec::new();
            for (nid, node) in graph.nodes.iter().enumerate() {
                let Op::Proj(edge) = node.op else {
                    continue;
                };
                if edge > 1 {
                    continue;
                }
                let Some(iff_id) = node.inputs.first().copied() else {
                    continue;
                };
                let Some(iff) = graph.node_opt(iff_id).filter(|i| i.op == Op::If) else {
                    continue;
                };
                let Some(cond) = iff.inputs.get(1).copied() else {
                    continue;
                };
                if cmp_proves_lt_any_int(graph, cond, edge as usize, phi) {
                    if let Some(&blk) = sched.node_to_block.get(nid) {
                        if sched.dom.is_reachable(blk) && sched.dom.dominates(region_blk, blk) {
                            guard_blocks.push(blk);
                        }
                    }
                }
            }
            if guard_blocks.is_empty() {
                continue;
            }
            let mut all_backs_ok = true;
            for (slot, &back_ctrl) in rn.inputs.iter().enumerate() {
                if slot == entry_slot {
                    continue;
                }
                let Some(back_val) = pnode.input_opt(1 + slot) else {
                    all_backs_ok = false;
                    break;
                };
                if back_val == phi {
                    continue;
                }
                let Some(bnode) = graph.node_opt(back_val) else {
                    all_backs_ok = false;
                    break;
                };
                let step_is_one = matches!(bnode.op, Op::Add)
                    && bnode.ty == IrType::Int
                    && bnode.inputs.len() == 2
                    && ((bnode.inputs[0] == phi && int_const(graph, bnode.inputs[1]) == Some(1))
                        || (bnode.inputs[1] == phi
                            && int_const(graph, bnode.inputs[0]) == Some(1)));
                if !step_is_one {
                    all_backs_ok = false;
                    break;
                }
                let Some(&back_blk) = sched.node_to_block.get(back_ctrl as usize) else {
                    all_backs_ok = false;
                    break;
                };
                if !sched.dom.is_reachable(back_blk)
                    || !guard_blocks
                        .iter()
                        .any(|&gb| sched.dom.dominates(gb, back_blk))
                {
                    all_backs_ok = false;
                    break;
                }
            }
            if all_backs_ok {
                nonneg_phis.insert(phi);
            }
        }
    }

    if nonneg_phis.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::new();
    for (id, node) in graph.nodes.iter().enumerate() {
        let Op::Cmp(cc) = node.op else {
            continue;
        };
        if node.ty != IrType::Int || node.inputs.len() != 2 {
            continue;
        }
        let (a, b) = (node.inputs[0], node.inputs[1]);
        let decided = if nonneg_phis.contains(&a) {
            int_const(graph, b).and_then(|c| match cc {
                CmpOp::Ge if c <= 0 => Some(1),
                CmpOp::Gt if c < 0 => Some(1),
                CmpOp::Lt if c <= 0 => Some(0),
                CmpOp::Le if c < 0 => Some(0),
                _ => None,
            })
        } else if nonneg_phis.contains(&b) {
            int_const(graph, a).and_then(|c| match cc {
                CmpOp::Le if c <= 0 => Some(1),
                CmpOp::Lt if c < 0 => Some(1),
                CmpOp::Gt if c <= 0 => Some(0),
                CmpOp::Ge if c < 0 => Some(0),
                _ => None,
            })
        } else {
            None
        };
        if let Some(v) = decided {
            out.push((id as NodeId, v));
        }
    }
    out
}

/// Walk back through single-input or same-arm diamond `Op::Merge` nodes from `ctrl`
/// to recover a unique dominating `(IfNode, ProjWhich)` branch edge, if any.
pub(in crate::ir_optimize) fn dominated_branch_proj(
    graph: &Graph,
    mut ctrl: NodeId,
) -> Option<(NodeId, u32)> {
    for _ in 0..8 {
        let n = graph.node_opt(ctrl)?;
        match n.op {
            Op::Proj(which) => return n.inputs.first().map(|&iff| (iff, u32::from(which))),
            Op::Merge if n.inputs.len() == 1 => ctrl = n.inputs[0],
            _ => break,
        }
    }
    let mut work = vec![ctrl];
    let mut visited = FxHashSet::default();
    let mut target: Option<(NodeId, u32)> = None;
    let mut steps = 0usize;
    while let Some(c) = work.pop() {
        if !visited.insert(c) {
            continue;
        }
        steps += 1;
        if steps > 32 {
            return None;
        }
        let n = graph.node_opt(c)?;
        match n.op {
            Op::Proj(which) => {
                let &iff = n.inputs.first()?;
                let w = u32::from(which);
                match target {
                    None => target = Some((iff, w)),
                    Some(prev) if prev == (iff, w) => {}
                    Some(_) => return None,
                }
            }
            Op::Merge => {
                if n.inputs.is_empty() {
                    return None;
                }
                work.extend_from_slice(&n.inputs);
            }
            _ => return None,
        }
    }
    target
}

/// Fold `Op::Cmp(CmpOp::Ne, x, 0)` (and `0, x`) to `x` when `x` already
/// produces `{0, 1}` (`ExactClassIs`, `InstanceOf`, or `Cmp`), keeping
/// memory tokens and safepoint snapshots sound.
fn fold_boolean_ne_zero(graph: &mut Graph) -> usize {
    let mut folded = 0usize;
    loop {
        let mut round = 0usize;
        for id in 0..graph.nodes.len() {
            let nid = id as NodeId;
            let Some(node) = graph.node_opt(nid) else {
                continue;
            };
            if node.op != Op::Cmp(CmpOp::Ne) || node.ty != IrType::Int || node.inputs.len() != 2 {
                continue;
            }
            let (a, b) = (node.inputs[0], node.inputs[1]);
            let x = match (int_const(graph, a), int_const(graph, b)) {
                (None, Some(0)) => a,
                (Some(0), None) => b,
                _ => continue,
            };
            if x == nid {
                continue;
            }
            let Some(xn) = graph.node_opt(x) else {
                continue;
            };
            if xn.ty != IrType::Int
                || !matches!(
                    xn.op,
                    Op::ExactClassIs { .. } | Op::InstanceOf { .. } | Op::Cmp(_)
                )
            {
                continue;
            }
            graph.replace_all_uses(nid, x);
            graph.kill(nid);
            round += 1;
        }
        folded += round;
        if round == 0 {
            break;
        }
    }
    folded
}

fn cond_used_elsewhere_or_in_snapshot(graph: &Graph, iff: NodeId, cond: NodeId) -> bool {
    let other_node_uses = graph.nodes.iter().enumerate().any(|(uid, u)| {
        if u.op == Op::Dead {
            return false;
        }
        let uid = uid as NodeId;
        if uid == iff {
            u.inputs
                .iter()
                .enumerate()
                .any(|(slot, &inp)| slot != 1 && inp == cond)
        } else {
            u.inputs.contains(&cond)
        }
    });
    if other_node_uses {
        return true;
    }
    graph.safepoints.iter().any(|sp| {
        sp.locals.contains(&cond) || sp.stack.contains(&cond) || sp.monitors.contains(&cond)
    })
}

/// Rewrite `If(ctrl, Cmp(Eq, ExactClassIs, 0))` (and `0, ExactClassIs`) to
/// `If(ctrl, ExactClassIs)` by swapping its `Proj(0)` <-> `Proj(1)` edges
/// when `cond` has no other live node or safepoint snapshot uses and `If` has
/// clean `[Proj(0), Proj(1)]` control projections.
fn rewrite_negated_exact_class_ifs(graph: &mut Graph) -> usize {
    let mut rewritten = 0usize;
    for id in 0..graph.nodes.len() {
        let iff = id as NodeId;
        let Some(iff_node) = graph.node_opt(iff) else {
            continue;
        };
        if iff_node.op != Op::If || iff_node.inputs.len() < 2 {
            continue;
        }
        let cond = iff_node.inputs[1];
        let Some(cond_node) = graph.node_opt(cond) else {
            continue;
        };
        if cond_node.op != Op::Cmp(CmpOp::Eq)
            || cond_node.ty != IrType::Int
            || cond_node.inputs.len() != 2
        {
            continue;
        }
        let (a, b) = (cond_node.inputs[0], cond_node.inputs[1]);
        let x = match (int_const(graph, a), int_const(graph, b)) {
            (None, Some(0)) => a,
            (Some(0), None) => b,
            _ => continue,
        };
        let Some(xn) = graph.node_opt(x) else {
            continue;
        };
        if xn.ty != IrType::Int || !matches!(xn.op, Op::ExactClassIs { .. }) {
            continue;
        }
        if cond_used_elsewhere_or_in_snapshot(graph, iff, cond) {
            continue;
        }

        let mut p0: Option<NodeId> = None;
        let mut p1: Option<NodeId> = None;
        let mut unexpected = false;
        for (uid, u) in graph.nodes.iter().enumerate() {
            if u.op == Op::Dead || !u.inputs.contains(&iff) {
                continue;
            }
            match u.op {
                Op::Proj(0)
                    if u.ty == IrType::Control && u.inputs.as_slice() == [iff] && p0.is_none() =>
                {
                    p0 = Some(uid as NodeId);
                }
                Op::Proj(1)
                    if u.ty == IrType::Control && u.inputs.as_slice() == [iff] && p1.is_none() =>
                {
                    p1 = Some(uid as NodeId);
                }
                _ => {
                    unexpected = true;
                    break;
                }
            }
        }
        if unexpected {
            continue;
        }
        let (Some(p0), Some(p1)) = (p0, p1) else {
            continue;
        };

        graph.set_input(iff, 1, x);
        graph.nodes[p0 as usize].op = Op::Proj(1);
        graph.nodes[p1 as usize].op = Op::Proj(0);
        graph.kill(cond);
        rewritten += 1;
    }
    rewritten
}

/// Strengthen final-class `CheckCast` nodes whose result is dereferenced
/// unconditionally on `ctrl` (or whose input is non-null) into a call-free
/// deopting `Guard(ExactClassIs { class_id })` backed by `DespecRegistry`.
pub(in crate::ir_optimize) fn strengthen_final_checkcast_to_guard(
    graph: &mut Graph,
    despec: &crate::deopt::DespecRegistry,
    method_key: &str,
) -> usize {
    let anchors: FxHashSet<NodeId> = exception_edge_anchors(graph).into_iter().collect();
    let mut converted = 0usize;
    let node_len = graph.nodes.len();
    for id in 0..node_len {
        let nid = id as NodeId;
        if anchors.contains(&nid) {
            continue;
        }
        let Some(node) = graph.node_opt(nid) else {
            continue;
        };
        let Op::CheckCast { name_ptr, name_len } = node.op else {
            continue;
        };
        if node.ty != IrType::Ref {
            continue;
        }
        let (Some(&ctrl), Some(&mem_in), Some(&obj)) =
            (node.inputs.first(), node.inputs.get(1), node.inputs.get(2))
        else {
            continue;
        };
        let Some(class_id) = super::plant_null_base_load_predicates::checkcast_exact_target(name_ptr, name_len) else {
            continue;
        };
        let Some(pc) = node.bytecode_pc else {
            continue;
        };
        let Ok(bci32) = u32::try_from(pc) else {
            continue;
        };
        let spec_id = crate::deopt::speculation_id(bci32, crate::deopt::DeoptReason::UncommonTrap);
        if despec.contains_speculation(method_key, bci32, spec_id) {
            continue;
        }
        let snap_idx = node
            .frame_snapshot
            .filter(|&fs| {
                graph
                    .safepoints
                    .get(fs as usize)
                    .is_some_and(|sp| sp.bci == pc && !sp.is_splice_state())
            })
            .or_else(|| {
                graph
                    .safepoints
                    .iter()
                    .position(|sp| sp.bci == pc && !sp.is_splice_state())
                    .and_then(|i| u32::try_from(i).ok())
            });
        let Some(snap_idx) = snap_idx else {
            continue;
        };
        let obj_nn = graph
            .node_opt(obj)
            .is_some_and(|on| crate::ir_check_elim::definitely_non_null(&on.op));
        let deref_on_ctrl = graph.nodes.iter().any(|u| {
            u.op != Op::Dead
                && matches!(u.op, Op::Unbox { .. } | Op::Load(_) | Op::Store(_))
                && u.input_opt(0) == Some(ctrl)
                && u.input_opt(2) == Some(nid)
        });
        if !(obj_nn || deref_on_ctrl) {
            continue;
        }

        let mem_users: Vec<(NodeId, usize)> = graph
            .nodes
            .iter()
            .enumerate()
            .flat_map(|(u, un)| {
                (0..un.inputs.len())
                    .filter(move |&k| un.inputs[k] == nid && is_memory_token_slot(un, k))
                    .map(move |k| (u as NodeId, k))
            })
            .collect();
        for (u, k) in mem_users {
            graph.set_input(u, k, mem_in);
        }
        graph.replace_all_uses(nid, obj);
        let exact = graph.add(
            Op::ExactClassIs { class_id },
            IrType::Int,
            vec![obj],
            Some(pc),
        );
        let guard = graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl, exact],
            Some(pc),
        );
        graph.set_node_frame_snapshot(guard, snap_idx);
        graph.kill(nid);
        converted += 1;
    }
    converted
}

/// Strengthen final-class `InstanceOf` nodes and normalize `{0, 1}` boolean
/// compares / negated `ExactClassIs` branches.
pub(in crate::ir_optimize) fn final_typecheck_fold(graph: &mut Graph) -> usize {
    let mut changed = strengthen_final_instanceof(graph);
    changed += fold_boolean_ne_zero(graph);
    changed += rewrite_negated_exact_class_ifs(graph);
    changed
}
