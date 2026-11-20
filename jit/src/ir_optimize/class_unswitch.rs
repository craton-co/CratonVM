// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Loop versioning on an invariant class test for a bimorphic receiver
//! (`CRATONVM_JIT_IR_FFM_CLASS_UNSWITCH`).
//!
//! HotSpot C2 unswitches loops on invariant type tests. When an invariant receiver
//! is tested against a class in the loop body (as in bimorphic FFM element access),
//! this pass versions the loop into two mono loops behind a pre-header class test:
//! `If(ExactClassIs(r, C))`.
//! Loop A executes under `ExactClassIs(r, C) == 1`.
//! Loop B executes under `ExactClassIs(r, C) == 0`.
//! Subsequent `prune_constant_ifs` strips the unreached branch in each loop,
//! converting both into single-block counted loops that the vectorizer can vectorize.

use super::{
    build_users, collapse_trivial_merges, install_copy_frames, is_loop_invariant, loop_body_with,
    loop_headers, prune_constant_ifs, scalar_cleanup, FxHashMap, FxHashSet,
};
use crate::ir::{Graph, IrType, NodeId, Op, SafepointSlotKind, UnboxOp, NO_NODE};
use std::collections::BTreeSet;

/// At most this many nodes in the loop body to admit unswitching.
const MAX_UNSWITCH_BODY_NODES: usize = 256;

/// `CRATONVM_JIT_IR_FFM_CLASS_UNSWITCH` (t1 wave 8, lane FFM27, FFM27-2):
/// loop versioning on an invariant class test for a bimorphic receiver.
fn ffm_class_unswitch_enabled() -> bool {
    true
}

/// Unswitch every candidate bimorphic loop in `graph`. Returns how many were unswitched.
pub(super) fn unswitch_bimorphic_class_loops(graph: &mut Graph) -> usize {
    if graph.ffm_bimorphic_tests.is_empty()
        || !ffm_class_unswitch_enabled()
        || !super::const_if_prune_enabled()
    {
        return 0;
    }
    collapse_trivial_merges(graph);
    let mut total_unswitched = 0;
    while !graph.ffm_bimorphic_tests.is_empty() {
        let loops = loop_headers(graph);
        if loops.is_empty() {
            break;
        }
        let users = build_users(graph);
        let mut progress = false;
        for (region, entry_pred, back_ctrls) in loops {
            if unswitch_one_loop(graph, &users, region, entry_pred, &back_ctrls) {
                prune_constant_ifs(graph);
                collapse_trivial_merges(graph);
                scalar_cleanup(graph);
                total_unswitched += 1;
                progress = true;
                break;
            }
        }
        if !progress {
            break;
        }
    }
    total_unswitched
}

fn unswitch_one_loop(
    graph: &mut Graph,
    users: &[Vec<NodeId>],
    region: NodeId,
    entry_pred: NodeId,
    back_ctrls: &[NodeId],
) -> bool {
    let dbg = false;

    if back_ctrls.len() != 1 || entry_pred == NO_NODE {
        if dbg { eprintln!("[unswitch-dbg] r{} rejected: back_ctrls.len={} entry={}", region, back_ctrls.len(), entry_pred); }
        return false;
    }
    let back_ctrl = back_ctrls[0];
    let Some(rnode) = graph.node_opt(region) else {
        return false;
    };
    if rnode.inputs.len() != 2 {
        if dbg { eprintln!("[unswitch-dbg] r{} rejected: rnode.inputs.len={}", region, rnode.inputs.len()); }
        return false;
    }
    let entry_idx = match rnode.inputs.iter().position(|&c| c == entry_pred) {
        Some(idx) => idx,
        None => return false,
    };
    let back_idx = match rnode.inputs.iter().position(|&c| c == back_ctrl) {
        Some(idx) => idx,
        None => return false,
    };

    // Carried phis of the loop header
    let mut carried = Vec::new();
    for &u in &users[region as usize] {
        if let Some(un) = graph.node_opt(u) {
            if un.op == Op::Phi && un.inputs.first() == Some(&region) {
                if un.inputs.len() != 3 {
                    if dbg { eprintln!("[unswitch-dbg] r{} rejected: phi inputs.len={}", region, un.inputs.len()); }
                    return false;
                }
                carried.push(u);
            }
        }
    }
    let carried_set: FxHashSet<NodeId> = carried.iter().copied().collect();

    let body = loop_body_with(graph, users, region, entry_pred, back_ctrls);
    if body.len() > MAX_UNSWITCH_BODY_NODES || body.len() < 3 {
        if dbg { eprintln!("[unswitch-dbg] r{} rejected: body.len={}", region, body.len()); }
        return false;
    }

    // Find the loop exit If and its projections
    let ctrl_users: Vec<NodeId> = users[region as usize]
        .iter()
        .copied()
        .filter(|&u| graph.node_opt(u).is_some_and(|n| n.op.is_control()))
        .collect();
    if ctrl_users.len() != 1 {
        if dbg {
            eprintln!("[unswitch-dbg] r{} rejected: ctrl_users.len={} ({:?})", region, ctrl_users.len(), ctrl_users);
            for &cu in &ctrl_users {
                eprintln!("[unswitch-dbg]   ctrl_user n{}: {:?}", cu, graph.node_opt(cu).map(|n| &n.op));
            }
        }
        return false;
    }
    let exit_if = ctrl_users[0];
    if graph.node_opt(exit_if).map(|n| &n.op) != Some(&Op::If) {
        if dbg { eprintln!("[unswitch-dbg] r{} rejected: exit_if not If", region); }
        return false;
    }
    let projs: Vec<NodeId> = users[exit_if as usize].clone();
    let [p0, p1] = *projs.as_slice() else {
        if dbg { eprintln!("[unswitch-dbg] r{} rejected: projs.len={}", region, projs.len()); }
        return false;
    };
    let exit_ctrl = match (body.contains(&p0), body.contains(&p1)) {
        (true, false) => p1,
        (false, true) => p0,
        _ => {
            if dbg { eprintln!("[unswitch-dbg] r{} rejected: stay/exit not partitioned in body", region); }
            return false;
        }
    };
    let exit_proj_idx = match graph.nodes[exit_ctrl as usize].op {
        Op::Proj(idx) => idx,
        _ => return false,
    };
    // A single exit is required for the exit merge and its live-out values.
    if body.iter().any(|&id| {
        graph.nodes[id as usize].op.is_control()
            && users[id as usize].iter().any(|&u| {
                graph.nodes[u as usize].op.is_control() && u != exit_ctrl && !body.contains(&u)
            })
    }) {
        return false;
    }

    let Some((r, class_a, test_ifs)) = invariant_tests(graph, region, &body) else {
        if dbg { eprintln!("[unswitch-dbg] r{} rejected: no candidate invariant class test found", region); }
        return false;
    };
    if dbg { eprintln!("[unswitch-dbg] r{} SUCCESS: candidate found! receiver n{}, class {}, test_ifs: {:?}", region, r, class_a, test_ifs); }

    // The clone set: body nodes (excluding header and carried phis),
    // inner phis of merges inside the body, and floating pure nodes computed from them.
    let mut clone: FxHashSet<NodeId> = body
        .iter()
        .copied()
        .filter(|&n| n != region && !carried_set.contains(&n))
        .collect();

    for (id, n) in graph.nodes.iter().enumerate() {
        let id = id as NodeId;
        if n.op == Op::Phi
            && !carried_set.contains(&id)
            && n.input_opt(0).is_some_and(|c| c != region && body.contains(&c))
        {
            clone.insert(id);
        }
    }

    let cloned_bcis: FxHashSet<usize> = clone
        .iter()
        .copied()
        .chain([region, exit_ctrl])
        .filter_map(|id| graph.nodes[id as usize].bytecode_pc)
        .collect();
    let mut plan = Vec::new();
    let mut frame_bcis = FxHashSet::default();
    for (si, sp) in graph.safepoints.iter().enumerate() {
        if cloned_bcis.contains(&sp.bci) {
            // This transform operates on flat, single-frame bytecode sites.
            if sp.is_splice_state() || !frame_bcis.insert(sp.bci) {
                return false;
            }
            plan.push((si as u32, sp.clone()));
        }
    }
    if clone.iter().chain([&region, &exit_ctrl]).any(|&id| {
        graph.nodes[id as usize].frame_snapshot.is_some()
    }) {
        return false;
    }

    // Floating values used by the loop or its frames need copies. Exit-only
    // calculations stay outside and take their inputs from the exit phis.
    let mut work: Vec<NodeId> = clone
        .iter()
        .chain(carried.iter())
        .flat_map(|&id| graph.nodes[id as usize].inputs.iter().copied())
        .collect();
    for (_, sp) in &plan {
        work.extend(sp.locals.iter().chain(&sp.stack).chain(&sp.monitors).copied());
    }
    while let Some(id) = work.pop() {
        if let Some(n) = graph.node_opt(id) {
            if n.op.is_pure()
                && !clone.contains(&id)
                && !is_loop_invariant(graph, id, region, &body)
            {
                clone.insert(id);
                work.extend(n.inputs.iter().copied());
            }
        }
    }

    if clone.len() > MAX_UNSWITCH_BODY_NODES {
        return false;
    }

    let Some(order) = clone_order(graph, users, &clone) else {
        return false;
    };

    // Preheader If(ExactClassIs(r, class_a))
    let cond = graph.add(Op::ExactClassIs { class_id: class_a }, IrType::Int, vec![r], None);
    let if_pre = graph.add(Op::If, IrType::Control, vec![entry_pred, cond], None);
    let into_a = graph.add(Op::Proj(0), IrType::Control, vec![if_pre], None);
    let into_b = graph.add(Op::Proj(1), IrType::Control, vec![if_pre], None);

    // Rewire Loop B (original loop) entry
    graph.set_input(region, entry_idx, into_b);

    // Clone Loop A
    let header_pc = graph.nodes[region as usize].bytecode_pc;
    let header_a = graph.add(Op::Merge, IrType::Control, vec![into_a], header_pc);
    let mut subst: FxHashMap<NodeId, NodeId> = FxHashMap::default();
    subst.insert(region, header_a);

    let mut phis_a: Vec<NodeId> = Vec::new();
    for &p in &carried {
        let ty = graph.nodes[p as usize].ty;
        let init = graph.nodes[p as usize].inputs[1 + entry_idx];
        let pc = graph.nodes[p as usize].bytecode_pc;
        let phi_a = graph.add(Op::Phi, ty, vec![header_a, init], pc);
        subst.insert(p, phi_a);
        phis_a.push(phi_a);
    }

    let mut iter_clones = vec![header_a];
    for &node in &order {
        let op = graph.nodes[node as usize].op.clone();
        let ty = graph.nodes[node as usize].ty;
        let pc = graph.nodes[node as usize].bytecode_pc;
        let volatile = graph.nodes[node as usize].volatile_access;
        let inputs: Vec<NodeId> = graph.nodes[node as usize]
            .inputs
            .iter()
            .map(|&i| subst.get(&i).copied().unwrap_or(i))
            .collect();
        let clone_node = graph.add(op, ty, inputs, pc);
        if volatile {
            graph.set_volatile_access(clone_node);
        }
        if graph.byte_array_nodes.contains(&node) {
            graph.byte_array_nodes.insert(clone_node);
        }
        if graph.ffm_proven_heap_accesses.contains(&node) {
            graph.ffm_proven_heap_accesses.insert(clone_node);
        }
        if let Some(&field) = graph.ffm_scoped_length_fields.get(&node) {
            graph.ffm_scoped_length_fields.insert(clone_node, field);
        }
        subst.insert(node, clone_node);
        iter_clones.push(clone_node);
    }

    // Connect back edges of Loop A
    let back_ctrl_a = subst.get(&back_ctrl).copied().unwrap_or(back_ctrl);
    graph.push_input(header_a, back_ctrl_a);
    for (&p, &phi_a) in carried.iter().zip(phis_a.iter()) {
        let back_val = graph.nodes[p as usize].inputs[1 + back_idx];
        let back_val_a = subst.get(&back_val).copied().unwrap_or(back_val);
        graph.push_input(phi_a, back_val_a);
    }

    // Exit merge
    let exit_if_a = subst.get(&exit_if).copied().unwrap_or(exit_if);
    let exit_ctrl_a = graph.add(
        Op::Proj(exit_proj_idx),
        IrType::Control,
        vec![exit_if_a],
        graph.nodes[exit_ctrl as usize].bytecode_pc,
    );
    let exit_ctrl_b = exit_ctrl;
    let exit_merge = graph.add(Op::Merge, IrType::Control, vec![exit_ctrl_a, exit_ctrl_b], None);
    iter_clones.push(exit_ctrl_a);
    install_copy_frames(graph, &plan, &subst, &iter_clones, false);

    // Closure to determine if a node is part of the loops
    let in_loop = |u: NodeId| clone.contains(&u) || carried_set.contains(&u) || u == region;

    // Rewire control users of exit_ctrl outside the loop
    for &u in &users[exit_ctrl as usize] {
        if !in_loop(u) {
            let u_inputs = graph.nodes[u as usize].inputs.clone();
            for (idx, &inp) in u_inputs.iter().enumerate() {
                if inp == exit_ctrl {
                    graph.set_input(u, idx, exit_merge);
                }
            }
        }
    }

    let outside_bcis: FxHashSet<_> = graph.nodes[..users.len()]
        .iter()
        .enumerate()
        .filter(|&(id, n)| n.op != Op::Dead && !in_loop(id as NodeId) && id as NodeId != exit_ctrl)
        .filter_map(|(_, n)| n.bytecode_pc)
        .collect();
    let mut escaping_slots: FxHashMap<NodeId, Vec<(usize, SafepointSlotKind, usize)>> = FxHashMap::default();
    for (si, sp) in graph.safepoints.iter().enumerate() {
        if !cloned_bcis.contains(&sp.bci) && outside_bcis.contains(&sp.bci) {
            for (kind, values) in [
                (SafepointSlotKind::Local, &sp.locals),
                (SafepointSlotKind::Stack, &sp.stack),
                (SafepointSlotKind::Monitor, &sp.monitors),
            ] {
                for (slot, &v) in values.iter().enumerate() {
                    if subst.contains_key(&v) {
                        escaping_slots.entry(v).or_default().push((si, kind, slot));
                    }
                }
            }
        }
    }
    for &v in carried.iter().chain(&order) {
        let ext_users: Vec<NodeId> = users[v as usize]
            .iter()
            .copied()
            .filter(|&u| !in_loop(u))
            .collect();
        if ext_users.is_empty() && !escaping_slots.contains_key(&v) {
            continue;
        }
        let ty = graph.nodes[v as usize].ty;
        if ty == IrType::Control {
            continue;
        }
        let v_a = subst.get(&v).copied().unwrap_or(v);
        let v_b = v;
        let exit_phi = graph.add(Op::Phi, ty, vec![exit_merge, v_a, v_b], None);
        for u in ext_users {
            let u_inputs = graph.nodes[u as usize].inputs.clone();
            for (idx, &inp) in u_inputs.iter().enumerate() {
                if inp == v {
                    graph.set_input(u, idx, exit_phi);
                }
            }
        }
        if let Some(slots) = escaping_slots.remove(&v) {
            for (si, kind, slot) in slots {
                graph.set_safepoint_slot(si, kind, slot, exit_phi);
            }
        }
    }

    // Fold class tests:
    // In Loop A (class_a): test of class_a -> 1
    // In Loop B (!class_a): test of class_a -> 0
    let one = graph.add(Op::Const(1), IrType::Int, vec![], None);
    let zero = graph.add(Op::Const(0), IrType::Int, vec![], None);

    for &t in &test_ifs {
        let t_a = subst.get(&t).copied().unwrap_or(t);
        graph.set_input(t_a, 1, one);
        graph.set_input(t, 1, zero);
        graph.ffm_bimorphic_tests.remove(&t);
        file_arm_reads(graph, users, t, header_a, Some(&subst));
        file_arm_reads(graph, users, t, region, None);
    }

    true
}

fn invariant_tests(
    graph: &Graph,
    region: NodeId,
    body: &FxHashSet<NodeId>,
) -> Option<(NodeId, u32, Vec<NodeId>)> {
    let mut tests = graph.ffm_bimorphic_tests.iter().copied().filter(|id| body.contains(id));
    let first = tests.next()?;
    let predicate = |id| {
        let n = graph.node_opt(id)?;
        if n.op != Op::If {
            return None;
        }
        super::class_predicate::exact_class_test(graph, n.input_opt(1)?)
    };
    let (recv, class_id) = predicate(first)?;
    if !is_loop_invariant(graph, recv, region, body) {
        return None;
    }
    let mut matched = vec![first];
    for test in tests {
        if predicate(test) != Some((recv, class_id)) {
            return None;
        }
        matched.push(test);
    }
    matched.sort_unstable();
    Some((recv, class_id, matched))
}

fn clone_order(
    graph: &Graph,
    users: &[Vec<NodeId>],
    clone: &FxHashSet<NodeId>,
) -> Option<Vec<NodeId>> {
    let mut indegree: FxHashMap<_, _> = clone
        .iter()
        .map(|&id| (id, graph.nodes[id as usize].inputs.iter().filter(|i| clone.contains(i)).count()))
        .collect();
    let mut ready: BTreeSet<_> = indegree.iter().filter_map(|(&id, &n)| (n == 0).then_some(id)).collect();
    let mut order = Vec::with_capacity(clone.len());
    while let Some(id) = ready.pop_first() {
        order.push(id);
        // The users map has one entry per input slot, including repeated inputs.
        for user in &users[id as usize] {
            if let Some(n) = indegree.get_mut(user) {
                *n -= 1;
                if *n == 0 {
                    ready.insert(*user);
                }
            }
        }
    }
    (order.len() == clone.len()).then_some(order)
}

fn file_arm_reads(
    graph: &mut Graph,
    users: &[Vec<NodeId>],
    test: NodeId,
    header: NodeId,
    copy: Option<&FxHashMap<NodeId, NodeId>>,
) {
    let arm = if copy.is_some() { 0 } else { 1 };
    let rename = |id| copy.and_then(|map| map.get(&id).copied()).unwrap_or(id);
    for &proj in &users[test as usize] {
        if graph.nodes[proj as usize].op != Op::Proj(arm) {
            continue;
        }
        for &site in &users[proj as usize] {
            let n = &graph.nodes[site as usize];
            if n.input_opt(0) != Some(proj) || !graph.ffm_scoped_length_fields.contains_key(&site) {
                continue;
            }
            let [_, mut mem, recv, scope, base, ..] = *n.inputs.as_slice() else {
                continue;
            };
            let native = graph.node_opt(base).is_some_and(crate::ir::is_null_const);
            while let Some(n) = graph.node_opt(mem) {
                let Op::Unbox { op, .. } = n.op else {
                    break;
                };
                if !super::ffm_field_hoist::is_segment_field_read(op)
                    || n.input_opt(0) != Some(proj)
                    || n.input_opt(2) != Some(recv)
                {
                    break;
                }
                let read = rename(mem);
                graph.ffm_final_field_reads.insert(read);
                graph.ffm_versioned_field_headers.insert(read, header);
                if mem == scope {
                    if native {
                        graph.ffm_native_scope_reads.insert(read);
                    } else {
                        graph.ffm_heap_scope_reads.insert(read);
                    }
                }
                mem = graph.nodes[mem as usize].inputs[1];
            }
        }
    }
}

#[cfg(test)]
mod tests;
