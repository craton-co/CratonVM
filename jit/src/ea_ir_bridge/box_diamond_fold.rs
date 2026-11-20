// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Recognize and fold spliced `--real-jdk` `Box.valueOf(x)` diamonds
//! (`Phi(cache_element, New(Box) + Store(value = x))`) and standalone
//! `Box.valueOf(x)` calls whose only remaining consumers are `hashCode`,
//! unboxes, or safepoint snapshots.

use super::*;

pub(super) struct SplicedBoxDiamond {
    pub phi: ir::NodeId,
    pub uop: ir::UnboxOp,
    pub value: ir::NodeId,
    pub box_class_id: u32,
    pub store: ir::NodeId,
    pub store_offset: ir::NodeId,
    pub new_obj: ir::NodeId,
    pub cache_load: ir::NodeId,
    pub cache_static: Option<ir::NodeId>,
    pub merge: ir::NodeId,
}

fn is_derived_from_value(g: &ir::Graph, node: ir::NodeId, value: ir::NodeId, depth: u8) -> bool {
    if node == value {
        return true;
    }
    if depth == 0 {
        return false;
    }
    let Some(n) = g.node_opt(node) else {
        return false;
    };
    match n.op {
        ir::Op::L2I | ir::Op::I2L | ir::Op::Neg => n
            .inputs
            .first()
            .copied()
            .is_some_and(|i| is_derived_from_value(g, i, value, depth - 1)),
        ir::Op::Add | ir::Op::Sub | ir::Op::Cmp(_) => n
            .inputs
            .iter()
            .any(|&i| is_derived_from_value(g, i, value, depth - 1)),
        _ => false,
    }
}

fn cache_static_of_array_load(
    g: &ir::Graph,
    cache_load: ir::NodeId,
) -> Option<(ir::NodeId, ir::NodeId)> {
    let n = g.node_opt(cache_load)?;
    if !matches!(n.op, ir::Op::ArrayLoad(ir::MemKind::Ref)) || n.inputs.len() < 4 {
        return None;
    }
    let arr = n.inputs[2];
    let idx = n.inputs[3];
    let static_arr = g.node_opt(arr).and_then(|an| match an.op {
        ir::Op::LoadStatic {
            type_tag: b'[' | b'L',
            ..
        }
        | ir::Op::IndyRootRef { .. } => Some(arr),
        _ => None,
    })?;
    Some((static_arr, idx))
}

/// Identify a spliced JDK `Integer.valueOf(x)` / `Long.valueOf(x)` diamond `Phi`:
/// `Phi(Ref, [merge, cache_elem, new_box])` where one arm is `Op::New { num_fields: 1 }`
/// initialized by a single `Op::Store(Int|Long, new_box, x)` and the other arm is
/// `Op::ArrayLoad(Ref)` from a static cache array indexed by an expression of `x`.
pub(super) fn detect_spliced_box_diamond(
    g: &ir::Graph,
    phi_id: ir::NodeId,
) -> Option<SplicedBoxDiamond> {
    let phi = g.node_opt(phi_id)?;
    if phi.op != ir::Op::Phi || phi.ty != ir::IrType::Ref || phi.inputs.len() != 3 {
        return None;
    }
    let merge = phi.inputs[0];
    if !g.node_opt(merge).is_some_and(|m| m.op == ir::Op::Merge) {
        return None;
    }
    let (a, b) = (phi.inputs[1], phi.inputs[2]);
    let (new_obj, cache_load) = match (g.node_opt(a).map(|n| &n.op), g.node_opt(b).map(|n| &n.op)) {
        (Some(ir::Op::New { .. }), Some(ir::Op::ArrayLoad(ir::MemKind::Ref))) => (a, b),
        (Some(ir::Op::ArrayLoad(ir::MemKind::Ref)), Some(ir::Op::New { .. })) => (b, a),
        _ => return None,
    };
    let (box_class_id, num_fields) = match g.node_opt(new_obj)?.op {
        ir::Op::New {
            class_id,
            num_fields: 1,
        } => (class_id, 1u16),
        _ => return None,
    };
    let _ = num_fields;
    if g.finalizable_new_classes.contains(&box_class_id) {
        return None;
    }

    // Inspect all value uses of `new_obj`: must be only `phi`, one `Op::Store(Int|Long)`,
    // optional `KeepAlive` fences, and optional `Op::Load` reading the same field.
    let mut found_store: Option<(ir::NodeId, ir::UnboxOp, ir::NodeId, ir::NodeId)> = None;
    for (uid, u) in g.nodes.iter().enumerate() {
        if u.op == ir::Op::Dead {
            continue;
        }
        let uid = uid as ir::NodeId;
        for (slot, &inp) in u.inputs.iter().enumerate() {
            if inp != new_obj || ea_is_memory_token_slot(u, slot) {
                continue;
            }
            if uid == phi_id {
                continue;
            }
            match u.op {
                ir::Op::KeepAlive if slot == 1 => {}
                ir::Op::Store(mk) if slot == 2 && u.inputs.len() >= 5 => {
                    let uop = match mk {
                        ir::MemKind::Int => ir::UnboxOp::IntValue,
                        ir::MemKind::Long => ir::UnboxOp::LongValue,
                        _ => return None,
                    };
                    if found_store.is_some() {
                        return None;
                    }
                    let val = u.inputs[4];
                    let off = u.inputs[3];
                    if g.type_of(val) != Some(uop.result_type()) {
                        return None;
                    }
                    found_store = Some((uid, uop, val, off));
                }
                _ => return None,
            }
        }
    }
    let (store, uop, value, store_offset) = found_store?;
    let expected_cid = match uop {
        ir::UnboxOp::IntValue => g.box_class_ids.0,
        ir::UnboxOp::LongValue => g.box_class_ids.1,
        _ => None,
    };
    if expected_cid.filter(|&cid| cid != 0) != Some(box_class_id) {
        return None;
    }
    if !g
        .node_opt(value)
        .is_some_and(|v| v.op != ir::Op::Dead && g.type_of(value) == Some(uop.result_type()))
    {
        return None;
    }
    let (cache_static, idx) = cache_static_of_array_load(g, cache_load)?;
    if !is_derived_from_value(g, idx, value, 4) {
        return None;
    }
    // Verify a dominating `If` above `cache_load`'s control compares `value`.
    if !has_dominating_value_compare(g, cache_load, value) {
        return None;
    }
    Some(SplicedBoxDiamond {
        phi: phi_id,
        uop,
        value,
        box_class_id,
        store,
        store_offset,
        new_obj,
        cache_load,
        cache_static: Some(cache_static),
        merge,
    })
}

fn has_dominating_value_compare(g: &ir::Graph, start: ir::NodeId, value: ir::NodeId) -> bool {
    let mut cur = g.node_opt(start).and_then(|n| n.inputs.first().copied());
    for _ in 0..16 {
        let Some(cid) = cur else {
            break;
        };
        let Some(cn) = g.node_opt(cid) else {
            break;
        };
        match cn.op {
            ir::Op::Proj(_) => {
                if let Some(&iff) = cn.inputs.first() {
                    if let Some(in_node) = g.node_opt(iff) {
                        if in_node.op == ir::Op::If {
                            if let Some(&cond) = in_node.inputs.get(1) {
                                if is_derived_from_value(g, cond, value, 3) {
                                    return true;
                                }
                            }
                            cur = in_node.inputs.first().copied();
                            continue;
                        }
                    }
                }
                cur = cn.inputs.first().copied();
            }
            ir::Op::Guard { .. } | ir::Op::Merge => {
                cur = cn.inputs.first().copied();
            }
            _ => break,
        }
    }
    false
}

/// True if `n` can seed `ir_fold_box_phi_webs`: either a `Ref` `Phi`, or a pure `Box.valueOf` call.
pub(super) fn is_box_web_seed(g: &ir::Graph, n: &ir::Node) -> bool {
    if n.ty != ir::IrType::Ref {
        return false;
    }
    match n.op {
        ir::Op::Phi => true,
        ir::Op::Call { info_ptr } => {
            ir_call_box_value_of(n, info_ptr).is_some() && !ir_call_guards_a_first_call(g, info_ptr)
        }
        _ => false,
    }
}

/// Fold `Op::Load(Int|Long)` (e.g. spliced `Integer.hashCode()` / `Integer.intValue()`)
/// and `Op::Unbox` whose receiver is a spliced `Box.valueOf(x)` diamond `Phi` to `x`.
pub(super) fn fold_spliced_box_field_loads_and_unboxes(g: &mut ir::Graph) -> usize {
    let mut targets: Vec<(ir::NodeId, ir::NodeId)> = Vec::new();
    for (idx, n) in g.nodes.iter().enumerate() {
        let id = idx as ir::NodeId;
        match n.op {
            ir::Op::Load(mk)
                if matches!(mk, ir::MemKind::Int | ir::MemKind::Long) && n.inputs.len() >= 4 =>
            {
                let base = n.inputs[2];
                let Some(d) = detect_spliced_box_diamond(g, base) else {
                    continue;
                };
                if n.ty != d.uop.result_type() {
                    continue;
                }
                let offset_ok = n.inputs[3] == d.store_offset
                    || matches!(
                        (
                            g.node_opt(n.inputs[3]).map(|o| &o.op),
                            g.node_opt(d.store_offset).map(|o| &o.op)
                        ),
                        (Some(ir::Op::Const(a)), Some(ir::Op::Const(b))) if a == b
                    );
                let decl_ok = match (
                    g.field_decl_class.get(&id),
                    g.field_decl_class.get(&d.store),
                ) {
                    (Some(&a), Some(&b)) => a == b && a == d.box_class_id,
                    _ => true,
                };
                if offset_ok && decl_ok {
                    targets.push((id, d.value));
                }
            }
            ir::Op::Unbox { op, .. } if n.inputs.len() == 3 => {
                let base = n.inputs[2];
                let Some(d) = detect_spliced_box_diamond(g, base) else {
                    continue;
                };
                if op == d.uop && n.ty == d.uop.result_type() {
                    targets.push((id, d.value));
                }
            }
            _ => {}
        }
    }
    let mut folded = 0usize;
    for (node_id, value) in targets {
        let Some(token) = g.node_opt(node_id).and_then(|n| n.input_opt(1)) else {
            continue;
        };
        if !g.node_opt(value).is_some_and(|v| v.op != ir::Op::Dead) {
            continue;
        }
        ir_retire_folded_node(g, node_id, token, Some(value));
        folded += 1;
    }
    // Fold `Cmp(Ne/Eq, d.phi, 0)` null checks on non-null box diamonds and retire
    // any guard consuming the resulting `Const(1)` when no live node depends on it.
    let mut cmp_folds: Vec<(ir::NodeId, i64)> = Vec::new();
    let is_zero_const = |g: &ir::Graph, nid: ir::NodeId| {
        g.node_opt(nid).is_some_and(|z| {
            z.op == ir::Op::Const(0) && matches!(z.ty, ir::IrType::Ref | ir::IrType::Int)
        })
    };
    for (idx, n) in g.nodes.iter().enumerate() {
        let id = idx as ir::NodeId;
        let (ir::Op::Cmp(cc @ (ir::CmpOp::Ne | ir::CmpOp::Eq)), [a, b]) =
            (&n.op, n.inputs.as_slice())
        else {
            continue;
        };
        let target = if is_zero_const(g, *b) {
            *a
        } else if is_zero_const(g, *a) {
            *b
        } else {
            continue;
        };
        if detect_spliced_box_diamond(g, target).is_some() {
            let val = i64::from(*cc == ir::CmpOp::Ne);
            cmp_folds.push((id, val));
        }
    }
    for (cmp_id, val) in cmp_folds {
        let c = g.add(ir::Op::Const(val), ir::IrType::Int, vec![], None);
        g.replace_all_uses(cmp_id, c);
        g.kill(cmp_id);
        folded += 1;
    }
    let true_guards: Vec<ir::NodeId> = g
        .nodes
        .iter()
        .enumerate()
        .filter_map(|(idx, n)| {
            let ir::Op::Guard { .. } = n.op else {
                return None;
            };
            let cond = n.inputs.get(1).copied()?;
            let ok = g
                .node_opt(cond)
                .is_some_and(|cn| matches!(cn.op, ir::Op::Const(v) if v != 0));
            let id = idx as ir::NodeId;
            (ok && has_no_external_value_users(g, id, &[])).then_some(id)
        })
        .collect();
    for gid in true_guards {
        g.kill(gid);
        folded += 1;
    }
    folded
}

/// True if `node` has no live value users (skipping memory-token edges and `ignored`).
fn has_no_external_value_users(g: &ir::Graph, node: ir::NodeId, ignored: &[ir::NodeId]) -> bool {
    for (uid, u) in g.nodes.iter().enumerate() {
        if u.op == ir::Op::Dead {
            continue;
        }
        let uid = uid as ir::NodeId;
        if ignored.contains(&uid) {
            continue;
        }
        for (slot, &inp) in u.inputs.iter().enumerate() {
            if inp != node || ea_is_memory_token_slot(u, slot) {
                continue;
            }
            if matches!(u.op, ir::Op::KeepAlive) && slot == 1 {
                continue;
            }
            return false;
        }
    }
    true
}

/// Fold dead/snapshot-only spliced JDK `Integer.valueOf(x)` / `Long.valueOf(x)`
/// diamonds in `ir_fold_box_phi_webs` with `AUTO_BOX_{INTEGER,LONG}_ATYPE` deopt recipes,
/// retiring the `Phi`, `New`, `Store`, `ArrayLoad`, and collapsing the empty CFG diamond.
pub(super) fn fold_spliced_box_diamonds_in_ea(
    ir_graph: &mut ir::Graph,
    sr_map: &mut Option<ir_lower::ScalarReplacementMap>,
    pinned: &mut std::collections::HashSet<ir::NodeId>,
    entry_ctrl: ir::NodeId,
) -> usize {
    let mut diamonds: Vec<SplicedBoxDiamond> = Vec::new();
    for idx in 0..ir_graph.nodes.len() {
        let id = idx as ir::NodeId;
        if let Some(d) = detect_spliced_box_diamond(ir_graph, id) {
            diamonds.push(d);
        }
    }
    let claimed_chains = super::ea_claimed_chain_snapshots(ir_graph);
    let consulted_pcs = super::ea_chain_pcs_consulted(ir_graph);
    let chain_virtuals = crate::deopt::chain_virtuals_enabled();
    let mut folded = 0usize;
    for d in diamonds {
        let members = [d.phi, d.new_obj, d.cache_load];
        if !has_no_external_value_users(ir_graph, d.phi, &members)
            || !has_no_external_value_users(ir_graph, d.new_obj, &[d.phi, d.store])
            || !has_no_external_value_users(ir_graph, d.cache_load, &[d.phi])
        {
            continue;
        }
        if !ir_graph
            .node_opt(d.value)
            .is_some_and(|v| v.op != ir::Op::Dead)
        {
            continue;
        }
        // Check snapshots: disallow Monitor / preserve_allocations, and only block on
        // splice_state chains that are actively consulted when chain virtuals are off.
        let mut snap_ok = true;
        for &m in &members {
            for s in ir_graph.safepoint_users_of(m) {
                let Some(sp) = ir_graph.safepoints.get(s.snapshot as usize) else {
                    snap_ok = false;
                    break;
                };
                if matches!(s.kind, ir::SafepointSlotKind::Monitor) || sp.preserve_allocations {
                    snap_ok = false;
                    break;
                }
                if sp.is_splice_state() {
                    let is_claimed = claimed_chains.contains(&s.snapshot);
                    let is_consulted = consulted_pcs.contains(&sp.bci)
                        || ir_graph
                            .nodes
                            .iter()
                            .any(|n| n.op != ir::Op::Dead && n.frame_snapshot == Some(s.snapshot));
                    if is_claimed && is_consulted && !chain_virtuals {
                        snap_ok = false;
                        break;
                    }
                }
            }
            if !snap_ok {
                break;
            }
        }
        if !snap_ok {
            continue;
        }
        let atype = match d.uop {
            ir::UnboxOp::IntValue => crate::deopt::AUTO_BOX_INTEGER_ATYPE,
            _ => crate::deopt::AUTO_BOX_LONG_ATYPE,
        };
        let has_snap = members
            .iter()
            .any(|&m| !ir_graph.safepoint_users_of(m).is_empty());
        if has_snap {
            let key = ir_graph.add(ir::Op::Dead, ir::IrType::Void, vec![], None);
            sr_map
                .get_or_insert_with(ir_lower::ScalarReplacementMap::default)
                .objects
                .insert(
                    key,
                    ir_lower::VirtualObjectInfo {
                        class_id: 0,
                        array_element_type: Some(atype),
                        num_fields: 1,
                        field_values: vec![Some(d.value)],
                        new_ctrl: entry_ctrl,
                        store_ctrls: Vec::new(),
                        store_nodes: Vec::new(),
                        store_fields: Vec::new(),
                        loop_phis: None,
                        non_null_cond: None,
                    },
                );
            pinned.insert(d.value);
            for &m in &members {
                for s in ir_graph.safepoint_users_of(m) {
                    ir_graph.set_safepoint_slot(s.snapshot as usize, s.kind, s.slot as usize, key);
                }
            }
        }
        // Retire KeepAlives on `new_obj`, `store`, `new_obj`, `cache_load`, `cache_static` (if unused), and `phi`.
        let mut fences: Vec<ir::NodeId> = Vec::new();
        for (uid, u) in ir_graph.nodes.iter().enumerate() {
            if u.op == ir::Op::KeepAlive && u.inputs.get(1).copied() == Some(d.new_obj) {
                fences.push(uid as ir::NodeId);
            }
        }
        for f in fences {
            if let Some(tok) = ir_graph.node_opt(f).and_then(|n| n.input_opt(0)) {
                ir_retire_folded_node(ir_graph, f, tok, None);
            }
        }
        if let Some(tok) = ir_graph.node_opt(d.store).and_then(|n| n.input_opt(1)) {
            ir_retire_folded_node(ir_graph, d.store, tok, None);
        }
        if let Some(tok) = ir_graph.node_opt(d.new_obj).and_then(|n| n.input_opt(1)) {
            ir_retire_folded_node(ir_graph, d.new_obj, tok, None);
        }
        if let Some(tok) = ir_graph.node_opt(d.cache_load).and_then(|n| n.input_opt(1)) {
            ir_retire_folded_node(ir_graph, d.cache_load, tok, None);
        }
        if let Some(cs) = d.cache_static {
            if has_no_external_value_users(ir_graph, cs, &[]) {
                if let Some(tok) = ir_graph.node_opt(cs).and_then(|n| n.input_opt(1)) {
                    ir_retire_folded_node(ir_graph, cs, tok, None);
                } else if ir_graph
                    .node_opt(cs)
                    .is_some_and(|n| matches!(n.op, ir::Op::IndyRootRef { .. }))
                {
                    ir_graph.kill(cs);
                }
            }
        }
        ir_graph.kill(d.phi);
        try_collapse_empty_box_diamond(ir_graph, d.merge);
        folded += 1;
    }
    folded
}

/// If `merge` is an empty 2-arm diamond (every node whose `ctrl` is inside the diamond
/// is dead or a pure `If`/`Proj`/`Merge` with no external users), rewire `merge` and any
/// uniform memory `Phi` at `merge` directly to the diamond's entry `ctrl` and `mem`.
fn try_collapse_empty_box_diamond(g: &mut ir::Graph, merge: ir::NodeId) {
    let Some(mnode) = g.node_opt(merge) else {
        return;
    };
    if mnode.op != ir::Op::Merge || mnode.inputs.len() != 2 {
        return;
    }
    let arms = [mnode.inputs[0], mnode.inputs[1]];
    // Collect candidate dominating `Op::If`s along `arms[0]`.
    let mut candidates = Vec::new();
    let mut cur = Some(arms[0]);
    for _ in 0..8 {
        let Some(c) = cur else { break };
        let Some(n) = g.node_opt(c) else { break };
        if n.op == ir::Op::If {
            candidates.push(c);
        }
        cur = match n.op {
            ir::Op::Proj(_) | ir::Op::If | ir::Op::Merge => n.inputs.first().copied(),
            _ => None,
        };
    }

    // Find the nearest candidate `entry_if` that encloses all control paths from both `arms`.
    let mut chosen: Option<(ir::NodeId, ir::NodeId, Vec<ir::NodeId>)> = None;
    for entry_if in candidates {
        let Some(entry_ctrl) = g.node_opt(entry_if).and_then(|n| n.inputs.first().copied()) else {
            continue;
        };
        let mut inner_ctrls: Vec<ir::NodeId> = Vec::new();
        let mut work = vec![arms[0], arms[1]];
        let mut valid = true;
        while let Some(id) = work.pop() {
            if inner_ctrls.contains(&id) {
                continue;
            }
            if inner_ctrls.len() >= 24 {
                valid = false;
                break;
            }
            let Some(n) = g.node_opt(id) else {
                valid = false;
                break;
            };
            inner_ctrls.push(id);
            if id == entry_if {
                continue;
            }
            match n.op {
                ir::Op::Proj(_) | ir::Op::If => {
                    let Some(&prev) = n.inputs.first() else {
                        valid = false;
                        break;
                    };
                    work.push(prev);
                }
                ir::Op::Merge if !n.inputs.is_empty() => {
                    work.extend_from_slice(&n.inputs);
                }
                _ => {
                    valid = false;
                    break;
                }
            }
        }
        if valid && inner_ctrls.contains(&entry_if) {
            chosen = Some((entry_if, entry_ctrl, inner_ctrls));
            break;
        }
    }

    let Some((_entry_if, entry_ctrl, inner_ctrls)) = chosen else {
        return;
    };

    let mut mem_phis: Vec<(ir::NodeId, ir::NodeId)> = Vec::new();
    for (i, n) in g.nodes.iter().enumerate() {
        if n.op == ir::Op::Phi && n.inputs.first().copied() == Some(merge) && n.inputs.len() == 3 {
            if n.ty != ir::IrType::Memory || n.inputs[1] != n.inputs[2] {
                return;
            }
            mem_phis.push((i as ir::NodeId, n.inputs[1]));
        }
    }

    for (uid, u) in g.nodes.iter().enumerate() {
        if u.op == ir::Op::Dead {
            continue;
        }
        let uid = uid as ir::NodeId;
        if uid == merge || inner_ctrls.contains(&uid) || mem_phis.iter().any(|&(mp, _)| mp == uid) {
            continue;
        }
        if u.inputs.iter().any(|inp| inner_ctrls.contains(inp)) {
            return;
        }
    }

    for (mp, mem_val) in mem_phis {
        g.replace_all_uses(mp, mem_val);
        g.kill(mp);
    }
    g.replace_all_uses(merge, entry_ctrl);
    g.kill(merge);
    for c in inner_ctrls {
        g.kill(c);
    }
}

/// `CRATONVM_JIT_IR_BOX_PHI_WEB_FOLD` (EA21-2; perf, default ON (flipped; `=0` kills)). Read once
/// per compile.
pub(crate) fn ir_box_phi_web_fold_enabled() -> bool {
    // R22-AB-FLIP: default ON since the JIT r22 opt-in A/B (docs/feature-designs/jit-ir-optimizations.md); `=0` is the kill switch.
    true
}

/// The most members one box web may have.
const MAX_BOX_WEB_MEMBERS: usize = 64;

/// One box web [`ir_fold_box_phi_webs`] retires.
struct BoxWeb {
    kind: ir::UnboxOp,
    /// Members in discovery order (calls and φs).
    members: Vec<ir::NodeId>,
    /// `(unbox, member it reads)`.
    readers: Vec<(ir::NodeId, ir::NodeId)>,
}

/// The box web containing reference φ `seed`, or `None` when the closure
/// leaves the shape (see the section comment). Marks every node it walks in
/// `visited`.
fn ir_box_web_from(
    g: &ir::Graph,
    users: &[Vec<(ir::NodeId, usize)>],
    seed: ir::NodeId,
    visited: &mut [bool],
) -> Option<BoxWeb> {
    let mut members: Vec<ir::NodeId> = Vec::new();
    let mut in_web: std::collections::HashSet<ir::NodeId> = std::collections::HashSet::new();
    let mut readers: Vec<(ir::NodeId, ir::NodeId)> = Vec::new();
    let mut kind: Option<ir::UnboxOp> = None;
    let mut work = vec![seed];
    while let Some(m) = work.pop() {
        if !in_web.insert(m) {
            continue;
        }
        if let Some(v) = visited.get_mut(m as usize) {
            *v = true;
        }
        if in_web.len() > MAX_BOX_WEB_MEMBERS {
            return None;
        }
        let node = g.node_opt(m)?;
        match &node.op {
            ir::Op::Phi if node.ty == ir::IrType::Ref && node.inputs.len() >= 2 => {
                for &inp in node.inputs.iter().skip(1) {
                    if inp == ir::NO_NODE {
                        return None;
                    }
                    work.push(inp);
                }
            }
            ir::Op::Call { info_ptr } => {
                let uop = ir_call_box_value_of(node, *info_ptr)?;
                if ir_call_guards_a_first_call(g, *info_ptr)
                    || node.input_opt(1).is_none()
                    || node.input_opt(2).and_then(|a| g.type_of(a)) != Some(uop.result_type())
                    || kind.is_some_and(|k| k != uop)
                {
                    return None;
                }
                kind = Some(uop);
            }
            _ => return None,
        }
        members.push(m);
        for &(u, slot) in users.get(m as usize).map(Vec::as_slice).unwrap_or(&[]) {
            let Some(un) = g.node_opt(u) else {
                continue;
            };
            if un.op == ir::Op::Dead || ea_is_memory_token_slot(un, slot) {
                continue;
            }
            match &un.op {
                ir::Op::Phi if un.ty == ir::IrType::Ref => work.push(u),
                // An ordering edge (`Op::CallThrew`); `ir_retire_box_call`
                // answers it "never threw".
                ir::Op::CallThrew { .. } if slot == 1 => {}
                ir::Op::Unbox { op, .. }
                    if slot == 2 && un.inputs.len() == 3 && un.input_opt(1).is_some() =>
                {
                    if kind.is_some_and(|k| k != *op) || un.ty != op.result_type() {
                        return None;
                    }
                    kind = Some(*op);
                    readers.push((u, m));
                }
                _ => return None,
            }
        }
    }
    let kind = kind?;
    if !matches!(kind, ir::UnboxOp::IntValue | ir::UnboxOp::LongValue) {
        return None;
    }
    // A web of φs alone carries no box this fold can name.
    if !members
        .iter()
        .any(|&m| matches!(g.node_opt(m).map(|n| &n.op), Some(ir::Op::Call { .. })))
    {
        return None;
    }
    Some(BoxWeb {
        kind,
        members,
        readers,
    })
}

/// May `web`'s members be described at every snapshot that names them? No
/// monitor slot, no chain snapshot, and no snapshot naming two members that
/// may be one object (their φ-input closures, themselves included, meet).
fn ir_box_web_snapshots_admit(g: &ir::Graph, web: &BoxWeb) -> bool {
    let member: std::collections::HashSet<ir::NodeId> = web.members.iter().copied().collect();
    let reach = |m: ir::NodeId| -> std::collections::HashSet<ir::NodeId> {
        let mut out = std::collections::HashSet::new();
        let mut work = vec![m];
        while let Some(x) = work.pop() {
            if !member.contains(&x) || !out.insert(x) {
                continue;
            }
            if let Some(n) = g.node_opt(x) {
                if n.op == ir::Op::Phi {
                    work.extend(n.inputs.iter().skip(1).copied());
                }
            }
        }
        out
    };
    let mut named: HashMap<u32, Vec<ir::NodeId>> = HashMap::new();
    for &m in &web.members {
        for s in g.safepoint_users_of(m) {
            if matches!(s.kind, ir::SafepointSlotKind::Monitor) {
                return false;
            }
            if g.safepoints.get(s.snapshot as usize).is_none_or(|sp| {
                (sp.is_splice_state() && !crate::deopt::chain_virtuals_enabled())
                    || sp.preserve_allocations
            }) {
                return false;
            }
            let list = named.entry(s.snapshot).or_default();
            if !list.contains(&m) {
                list.push(m);
            }
        }
    }
    for list in named.values() {
        if list.len() < 2 {
            continue;
        }
        let sets: Vec<std::collections::HashSet<ir::NodeId>> =
            list.iter().map(|&m| reach(m)).collect();
        for i in 0..sets.len() {
            for j in (i + 1)..sets.len() {
                if !sets[i].is_disjoint(&sets[j]) {
                    return false;
                }
            }
        }
    }
    true
}

/// Fold every box web (see the section comment). The recipes go into
/// `sr_map` (created if `None`), and each recipe's primitive into `pinned`
/// (the escape-analysis rounds' `descriptor_pinned`, so no later round retires
/// a value a recipe reads). Returns how many webs folded. Needs precise resume
/// (`scalar_deopt_descriptor_available`): without a recipe a snapshot naming a
/// box could not be described.
pub(crate) fn ir_fold_box_phi_webs(
    ir_graph: &mut ir::Graph,
    sr_map: &mut Option<ir_lower::ScalarReplacementMap>,
    pinned: &mut std::collections::HashSet<ir::NodeId>,
) -> usize {
    if !ir_box_phi_web_fold_enabled()
        || escape_analysis::ir_scalar_replacement_disabled()
        || !scalar_deopt_descriptor_available()
        || (ir_graph.osr_compile && !crate::deopt::osr_guard_exit_virtuals_enabled())
    {
        return 0;
    }
    // The entry block's control: every recipe's allocation point, which
    // strictly dominates every other block (`frame_value_for_object`).
    let entry = ir_graph.entry;
    let Some(entry_ctrl) = ir_graph.nodes.iter().enumerate().find_map(|(i, n)| {
        (matches!(n.op, ir::Op::Proj(0))
            && n.ty == ir::IrType::Control
            && n.inputs.first() == Some(&entry))
        .then_some(i as ir::NodeId)
    }) else {
        return 0;
    };
    let count = ir_graph.nodes.len();
    let mut users: Vec<Vec<(ir::NodeId, usize)>> = vec![Vec::new(); count];
    for (uid, n) in ir_graph.nodes.iter().enumerate() {
        if n.op == ir::Op::Dead {
            continue;
        }
        for (slot, &inp) in n.inputs.iter().enumerate() {
            if let Some(list) = users.get_mut(inp as usize) {
                list.push((uid as ir::NodeId, slot));
            }
        }
    }
    let mut visited = vec![false; count];
    let mut webs: Vec<BoxWeb> = Vec::new();
    for seed in 0..count {
        if visited[seed] {
            continue;
        }
        let n = &ir_graph.nodes[seed];
        if !is_box_web_seed(ir_graph, n) {
            continue;
        }
        if let Some(web) = ir_box_web_from(ir_graph, &users, seed as ir::NodeId, &mut visited) {
            if ir_box_web_snapshots_admit(ir_graph, &web) {
                webs.push(web);
            }
        }
    }
    let mut folded = 0usize;
    for web in webs {
        let prim_ty = web.kind.result_type();
        let atype = match web.kind {
            ir::UnboxOp::IntValue => crate::deopt::AUTO_BOX_INTEGER_ATYPE,
            _ => crate::deopt::AUTO_BOX_LONG_ATYPE,
        };
        // Each member's primitive: a call's argument, a φ's new primitive φ.
        let mut prim: HashMap<ir::NodeId, ir::NodeId> = HashMap::new();
        let mut phis: Vec<(ir::NodeId, ir::NodeId)> = Vec::new();
        for &m in &web.members {
            let n = &ir_graph.nodes[m as usize];
            if matches!(n.op, ir::Op::Call { .. }) {
                if let Some(arg) = n.input_opt(2) {
                    prim.insert(m, arg);
                }
            } else {
                let inputs: Vec<ir::NodeId> = n.inputs.iter().copied().collect();
                let pc = n.bytecode_pc;
                let p = ir_graph.add(ir::Op::Phi, prim_ty, inputs, pc);
                prim.insert(m, p);
                phis.push((m, p));
            }
        }
        for &(m, p) in &phis {
            let len = ir_graph.nodes[m as usize].inputs.len();
            for slot in 1..len {
                let inp = ir_graph.nodes[m as usize].inputs[slot];
                if let Some(&v) = prim.get(&inp) {
                    ir_graph.set_input(p, slot, v);
                }
            }
        }
        // Recipes for the members a snapshot or another object's recipe names
        // (a varargs `Object[]` escape analysis removed holds the box), and
        // the slots and recipe fields re-pointed.
        for &m in &web.members {
            let slots = ir_graph.safepoint_users_of(m);
            let in_recipes = sr_map.as_ref().is_some_and(|sr| {
                sr.objects.values().any(|o| {
                    o.field_values.contains(&Some(m))
                        || o.store_fields.iter().flatten().any(|&(_, v)| v == Some(m))
                })
            });
            if slots.is_empty() && !in_recipes {
                continue;
            }
            let Some(&value) = prim.get(&m) else {
                continue;
            };
            let key = ir_graph.add(ir::Op::Dead, ir::IrType::Void, vec![], None);
            let is_phi = phis.iter().any(|&(pm, _)| pm == m);
            sr_map
                .get_or_insert_with(ir_lower::ScalarReplacementMap::default)
                .objects
                .insert(
                    key,
                    ir_lower::VirtualObjectInfo {
                        class_id: 0,
                        array_element_type: Some(atype),
                        num_fields: 1,
                        field_values: vec![Some(value)],
                        new_ctrl: entry_ctrl,
                        store_ctrls: Vec::new(),
                        store_nodes: Vec::new(),
                        store_fields: Vec::new(),
                        loop_phis: is_phi.then(|| vec![(0, value)]),
                        non_null_cond: None,
                    },
                );
            pinned.insert(value);
            for s in slots {
                ir_graph.set_safepoint_slot(s.snapshot as usize, s.kind, s.slot as usize, key);
            }
            if in_recipes {
                if let Some(sr) = sr_map.as_mut() {
                    for (&owner, o) in sr.objects.iter_mut() {
                        if owner == key {
                            continue;
                        }
                        for fv in o.field_values.iter_mut().filter(|fv| **fv == Some(m)) {
                            *fv = Some(key);
                        }
                        for (_, v) in o
                            .store_fields
                            .iter_mut()
                            .flatten()
                            .filter(|(_, v)| *v == Some(m))
                        {
                            *v = Some(key);
                        }
                    }
                }
            }
        }
        // Unboxes read their member's primitive; then the calls leave the
        // memory chain and the box φs die.
        for &(u, m) in &web.readers {
            let Some(&value) = prim.get(&m) else {
                continue;
            };
            let Some(token) = ir_graph.node_opt(u).and_then(|n| n.input_opt(1)) else {
                continue;
            };
            ir_retire_folded_node(ir_graph, u, token, Some(value));
        }
        for &m in &web.members {
            if matches!(ir_graph.nodes[m as usize].op, ir::Op::Call { .. }) {
                ir_retire_box_call(ir_graph, m);
            }
        }
        for &(m, _) in &phis {
            ir_graph.kill(m);
        }
        folded += 1;
    }
    folded += fold_spliced_box_field_loads_and_unboxes(ir_graph);
    folded += fold_spliced_box_diamonds_in_ea(ir_graph, sr_map, pinned, entry_ctrl);
    if folded > 0 && ir_stage_reporting() {
        eprintln!("[ir] box phi-web fold: {folded} boxed accumulator web(s) unboxed");
    }
    folded
}

#[cfg(test)]
#[path = "box_diamond_fold_tests.rs"]
mod box_diamond_fold_tests;
