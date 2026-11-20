// ---------------------------------------------------------------------------
// Allocation sinking to the one escaping edge (round 16, lane alloc)
// ---------------------------------------------------------------------------
//
// `for (..) { P p = new P(i, i + 1); if ((i & 1023) == 0) keep = p; s += p.x + p.y; }`
// allocates on every iteration although the object escapes (into the
// loop-carried `keep` phi) on one iteration in 1024. Escape analysis sees the
// phi and answers `GlobalEscape` (the phi reaches a `keep != null` compare and
// a load after the loop), so nothing is replaced, and every iteration pays the
// bump, the header, two stores and two loads -- HotSpot scalar-replaces all
// but the kept ones.
//
// This pass is the narrow, provable half of partial escape analysis: when the
// ONLY use of a `new` other than stores into it and loads from it is one input
// of a value phi, and that input arrives over one edge of the `If` that ends
// the allocation's own block, it
//
//   1. forwards every field load to the value the allocation's own store put
//      there (the object has no other writer: every use is listed);
//   2. deletes the allocation and its stores from the block;
//   3. re-creates them -- a fresh `Op::New` with the same class, bci and
//      stores -- on the escaping edge, and hands THAT object to the phi;
//   4. merges the edge's new memory state into the join with a memory phi.
//
// Identity: the only reference to the object that survives the block is the
// phi input, and it is now the materialized copy, so no program point can
// observe two different objects for one Java object. The materialized object
// holds exactly what the original held at that point: every store ran in the
// allocation's block, which ends at the `If`.
//
// Deopt: the only frames that can name the deleted node are snapshots taken
// between the `new` and the end of its uses. A snapshot that a deopt or an
// exception exit can actually consult (a transfer bci: every node that can
// deopt, every loop header, every claimed or post-call snapshot) and that
// names the allocation refuses the whole rewrite; the rest are snapshots no
// transfer can read (forward joins, the spliced `<init>`'s invoke frame), and
// their slots for the deleted node are cleared to `NO_NODE`, the dead-slot
// marker snapshot liveness already writes. The materialized `new` keeps the
// original's bci, so its OOM exit resumes at the `new` with the frame the
// original used -- re-executing `new` and the block, which wrote nothing
// observable (JVMS 6.3 lets a `VirtualMachineError` surface at any point).
//
// Everything else refuses: a second phi use, any other use (a call, a
// compare, a monitor, a store OF the object), a volatile access, a sub-int
// field kind, a load of a field no store wrote, a load not ordered after the
// store it would forward, an existing memory phi at the join, a join with any
// other predecessor, a node pinned to either edge, an unattributable deopt.

/// `CRATONVM_JIT_IR_ALLOC_SINK_TO_EDGE=1` — run
/// [`ir_sink_allocations_to_escaping_edge`] before escape analysis. Default ON (flipped; `=0` kills); read per compile.
pub(crate) fn ir_alloc_sink_to_edge_enabled() -> bool {
    true
}

/// One planned sink: everything [`apply_edge_sink`] needs, all of it decided
/// before the graph is touched.
struct EdgeSinkPlan {
    alloc: ir::NodeId,
    /// The escaping edge (`Proj` of the block-ending `If`).
    edge: ir::NodeId,
    /// The join the edge flows into.
    join: ir::NodeId,
    /// The value phi at `join` and the input slot that carries the object.
    phi: ir::NodeId,
    phi_slot: usize,
    /// The allocation's stores, in id order.
    stores: Vec<ir::NodeId>,
    /// `(load, forwarded value)`.
    loads: Vec<(ir::NodeId, ir::NodeId)>,
    /// The memory state flowing out of the allocation's block (with the
    /// stores removed), which every memory use after the join consumed.
    mem_out: ir::NodeId,
    /// `(node, slot)` memory uses after the join that now read the merge.
    after_join: Vec<(ir::NodeId, usize)>,
    /// `(node, slot, token)` other memory uses naming a removed store or load.
    rewire: Vec<(ir::NodeId, usize, ir::NodeId)>,
    /// `(snapshot, kind, slot)` frame slots naming the allocation.
    snapshot_slots: Vec<(usize, ir::SafepointSlotKind, usize)>,
}

/// Sink every allocation [`plan_edge_sink`] admits; returns how many. See the
/// section comment above. Bounded: one plan per round, at most eight rounds.
pub(crate) fn ir_sink_allocations_to_escaping_edge(
    graph: &mut ir::Graph,
    spliced_ranges: &[(usize, usize, usize)],
) -> usize {
    let mut sunk = 0usize;
    for _ in 0..8 {
        let users = edge_sink_users(graph);
        // The cheap screen first: a `new` used only by field accesses on it
        // and by one value phi. Only then is the graph scheduled for the
        // dominance questions the plan asks.
        let candidate = graph.nodes.iter().enumerate().any(|(id, n)| {
            matches!(n.op, ir::Op::New { .. })
                && users.get(id).is_some_and(|us| {
                    let mut phis = 0usize;
                    let shape_ok = us.iter().all(|&(u, slot)| {
                        graph.node_opt(u).is_some_and(|un| match un.op {
                            ir::Op::Store(_) | ir::Op::Load(_) => slot == 2,
                            ir::Op::Phi => {
                                phis += 1;
                                un.ty == ir::IrType::Ref
                            }
                            _ => false,
                        })
                    });
                    shape_ok && phis == 1
                })
        });
        if !candidate {
            break;
        }
        let sched = crate::ir_schedule::schedule(graph);
        let plan = (0..graph.nodes.len())
            .find_map(|id| plan_edge_sink(graph, &sched, &users, id as ir::NodeId, spliced_ranges));
        let Some(plan) = plan else {
            break;
        };
        apply_edge_sink(graph, plan);
        sunk += 1;
    }
    sunk
}

/// `users[v]` = every `(user, input slot)` naming `v`, over live nodes.
fn edge_sink_users(graph: &ir::Graph) -> Vec<Vec<(ir::NodeId, usize)>> {
    let mut users: Vec<Vec<(ir::NodeId, usize)>> = vec![Vec::new(); graph.nodes.len()];
    for (id, n) in graph.nodes.iter().enumerate() {
        if n.op == ir::Op::Dead {
            continue;
        }
        for (slot, &inp) in n.inputs.iter().enumerate() {
            if let Some(list) = users.get_mut(inp as usize) {
                list.push((id as ir::NodeId, slot));
            }
        }
    }
    users
}

/// The constant field index at input 3 of a field access, if any.
fn edge_sink_field_index(graph: &ir::Graph, access: &ir::Node) -> Option<i64> {
    let off = *access.inputs.get(3)?;
    match graph.node_opt(off)?.op {
        ir::Op::Const(v) if v >= 0 => Some(v),
        _ => None,
    }
}

/// The full-width kinds a forwarded load may read: a sub-int field's stored
/// value need not be the value its load returns.
fn edge_sink_kind_ok(kind: ir::MemKind) -> bool {
    matches!(
        kind,
        ir::MemKind::Int
            | ir::MemKind::Long
            | ir::MemKind::Float
            | ir::MemKind::Double
            | ir::MemKind::Ref
    )
}

/// Is `id` one of the nodes the plan removes (the allocation, a store into
/// it, a load from it)?
fn edge_sink_removed(
    id: ir::NodeId,
    alloc: ir::NodeId,
    stores: &std::collections::HashSet<ir::NodeId>,
    loads: &std::collections::HashSet<ir::NodeId>,
) -> bool {
    id == alloc || stores.contains(&id) || loads.contains(&id)
}

/// The block a memory TOKEN is defined in.
///
/// Not just `node_to_block`: the scheduler places only data nodes and block
/// heads, and a memory projection (`Proj(1)` of `Start`, a call's memory
/// `Proj`) is a control-family op it never places -- its slot stays
/// `usize::MAX`. Such a token is defined where its producer is: `Start`'s in
/// the entry block (the block headed by `Start` or one of its projections),
/// anything else's in its producer's block. Round 16 lane alloc, wave c: the
/// planner used to ask `node_to_block` alone and refused every graph whose
/// memory chain starts at the entry -- every graph, since a loop's memory phi
/// names the entry token on its entry edge.
fn edge_sink_def_block(
    graph: &ir::Graph,
    sched: &crate::ir_schedule::Schedule,
    token: ir::NodeId,
) -> Option<usize> {
    let placed = |id: ir::NodeId| -> Option<usize> {
        let b = *sched.node_to_block.get(id as usize)?;
        (b < sched.blocks.len()).then_some(b)
    };
    if let Some(b) = placed(token) {
        return Some(b);
    }
    let n = graph.node_opt(token)?;
    if !matches!(n.op, ir::Op::Proj(_)) {
        return None;
    }
    let producer = *n.inputs.first()?;
    if graph.node_opt(producer)?.op == ir::Op::Start {
        if let Some(b) = placed(producer) {
            return Some(b);
        }
        return sched.blocks.iter().position(|blk| {
            blk.ctrl == producer
                || graph.node_opt(blk.ctrl).is_some_and(|c| {
                    matches!(c.op, ir::Op::Proj(_)) && c.inputs.first() == Some(&producer)
                })
        });
    }
    placed(producer)
}

fn plan_edge_sink(
    graph: &ir::Graph,
    sched: &crate::ir_schedule::Schedule,
    users: &[Vec<(ir::NodeId, usize)>],
    alloc: ir::NodeId,
    spliced_ranges: &[(usize, usize, usize)],
) -> Option<EdgeSinkPlan> {
    let n = graph.node_opt(alloc)?;
    if !matches!(n.op, ir::Op::New { .. }) || n.inputs.len() != 2 || n.frame_snapshot.is_some() {
        return None;
    }
    // JIT r20 lane rev12 (hFin's screen, completed): sinking deletes the
    // allocation on its non-escaping path, which is scalar replacement there;
    // a finalizable or `java.lang.ref` class is never removed, as
    // `find_scalar_replacements` already refuses it.
    if let ir::Op::New { class_id, .. } = &n.op {
        if graph.finalizable_new_classes.contains(class_id) {
            return None;
        }
    }
    let alloc_pc = n.bytecode_pc?;
    let ctrl = n.inputs[0];

    // ── every use of the allocation, classified ─────────────────────────
    let mut stores: Vec<ir::NodeId> = Vec::new();
    let mut loads: Vec<ir::NodeId> = Vec::new();
    let mut phi_use: Option<(ir::NodeId, usize)> = None;
    for &(u, slot) in users.get(alloc as usize)? {
        let un = graph.node_opt(u)?;
        match un.op {
            ir::Op::Store(_) if slot == 2 => stores.push(u),
            ir::Op::Load(_) if slot == 2 => loads.push(u),
            ir::Op::Phi if un.ty == ir::IrType::Ref && slot >= 1 && phi_use.is_none() => {
                phi_use = Some((u, slot))
            }
            _ => return None,
        }
    }
    let (phi, phi_slot) = phi_use?;
    stores.sort_unstable();
    stores.dedup();
    loads.sort_unstable();
    loads.dedup();

    // ── the stores: in the allocation's block, one per field ────────────
    let mut field_store: HashMap<i64, (ir::NodeId, ir::MemKind, ir::NodeId)> = HashMap::new();
    for &s in &stores {
        let sn = graph.node_opt(s)?;
        let ir::Op::Store(kind) = sn.op else {
            return None;
        };
        if sn.volatile_access
            || sn.inputs.len() < 5
            || sn.inputs[0] != ctrl
            || !edge_sink_kind_ok(kind)
        {
            return None;
        }
        let field = edge_sink_field_index(graph, sn)?;
        if field_store.insert(field, (s, kind, sn.inputs[4])).is_some() {
            return None;
        }
    }
    let store_set: std::collections::HashSet<ir::NodeId> = stores.iter().copied().collect();
    let load_set: std::collections::HashSet<ir::NodeId> = loads.iter().copied().collect();

    // ── the loads: each forwards the value its field's store wrote ──────
    let mut forwarded: Vec<(ir::NodeId, ir::NodeId)> = Vec::new();
    for &l in &loads {
        let ln = graph.node_opt(l)?;
        let ir::Op::Load(kind) = ln.op else {
            return None;
        };
        if ln.volatile_access || ln.inputs.len() < 4 {
            return None;
        }
        let field = edge_sink_field_index(graph, ln)?;
        let &(store, store_kind, value) = field_store.get(&field)?;
        if store_kind != kind {
            return None;
        }
        // Ordered after that store: the memory chain back from the load runs
        // through other forwarded loads, then through this object's stores,
        // and meets `store` there.
        let mut t = ln.inputs[1];
        while load_set.contains(&t) {
            t = *graph.node_opt(t)?.inputs.get(1)?;
        }
        let mut seen = false;
        while store_set.contains(&t) {
            if t == store {
                seen = true;
                break;
            }
            t = *graph.node_opt(t)?.inputs.get(1)?;
        }
        if !seen {
            return None;
        }
        forwarded.push((l, value));
    }

    // ── the escaping edge: one projection of the If ending this block ───
    let pn = graph.node_opt(phi)?;
    let join = *pn.inputs.first()?;
    let jn = graph.node_opt(join)?;
    if !matches!(jn.op, ir::Op::Merge | ir::Op::Region) || jn.inputs.len() != 2 {
        return None;
    }
    if jn.inputs[0] == jn.inputs[1] {
        return None;
    }
    let edge = *jn.inputs.get(phi_slot - 1)?;
    let iff = {
        let en = graph.node_opt(edge)?;
        if !matches!(en.op, ir::Op::Proj(_)) {
            return None;
        }
        *en.inputs.first()?
    };
    let ifn = graph.node_opt(iff)?;
    if ifn.op != ir::Op::If || ifn.inputs.first() != Some(&ctrl) {
        return None;
    }
    for &pred in jn.inputs.iter() {
        let pr = graph.node_opt(pred)?;
        if !matches!(pr.op, ir::Op::Proj(_)) || pr.inputs.first() != Some(&iff) {
            return None;
        }
        // Nothing but the join is pinned to either edge.
        if users.get(pred as usize)?.iter().any(|&(u, _)| u != join) {
            return None;
        }
    }
    // No memory phi at the join yet (the edges carry no memory effect).
    let join_has_mem_phi = users.get(join as usize)?.iter().any(|&(u, _)| {
        graph
            .node_opt(u)
            .is_some_and(|un| un.op == ir::Op::Phi && un.ty == ir::IrType::Memory)
    });
    if join_has_mem_phi {
        return None;
    }

    // ── memory: who reads the state flowing through the join ────────────
    let block_of = |id: ir::NodeId| -> Option<usize> {
        let b = *sched.node_to_block.get(id as usize)?;
        (b < sched.blocks.len()).then_some(b)
    };
    let join_block = block_of(join)?;
    let resolve = |mut t: ir::NodeId| -> Option<ir::NodeId> {
        while store_set.contains(&t) || load_set.contains(&t) {
            t = *graph.node_opt(t)?.inputs.get(1)?;
        }
        Some(t)
    };
    let mut mem_out: Option<ir::NodeId> = None;
    let mut after_join: Vec<(ir::NodeId, usize)> = Vec::new();
    let mut rewire: Vec<(ir::NodeId, usize, ir::NodeId)> = Vec::new();
    for (uid, un) in graph.nodes.iter().enumerate() {
        let uid = uid as ir::NodeId;
        if un.op == ir::Op::Dead || edge_sink_removed(uid, alloc, &store_set, &load_set) {
            continue;
        }
        for (slot, &raw) in un.inputs.iter().enumerate() {
            if raw == ir::NO_NODE || !ir::is_memory_token_slot(un, slot) {
                continue;
            }
            let token = resolve(raw)?;
            let use_block = if un.op == ir::Op::Phi {
                let merge = graph.node_opt(*un.inputs.first()?)?;
                block_of(*merge.inputs.get(slot - 1)?)?
            } else {
                block_of(uid)?
            };
            let def_block = edge_sink_def_block(graph, sched, token)?;
            if sched.dom.dominates(join_block, use_block)
                && !sched.dom.dominates(join_block, def_block)
            {
                match mem_out {
                    None => mem_out = Some(token),
                    Some(m) if m == token => {}
                    Some(_) => return None,
                }
                after_join.push((uid, slot));
            } else if token != raw {
                rewire.push((uid, slot, token));
            }
        }
    }
    let mem_out = mem_out?;

    // ── deopt: no transfer may read a frame naming the allocation ───────
    let resume = |pc: usize| -> usize {
        spliced_ranges
            .iter()
            .find(|&&(start, end, _)| pc >= start && pc < end)
            .map_or(pc, |&(_, _, invoke)| invoke)
    };
    let mut transfer: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut claimed: std::collections::HashSet<u32> = std::collections::HashSet::new();
    // The materialized `new` traps at the original's bci.
    transfer.insert(alloc_pc);
    transfer.insert(resume(alloc_pc));
    for (vid, v) in graph.nodes.iter().enumerate() {
        let vid = vid as ir::NodeId;
        if v.op == ir::Op::Dead || edge_sink_removed(vid, alloc, &store_set, &load_set) {
            continue;
        }
        if let Some(si) = v.frame_snapshot {
            claimed.insert(si);
        }
        if matches!(v.op, ir::Op::Merge | ir::Op::Region) {
            // A loop header is where OSR enters and where a poll may trap.
            let Some(b) = block_of(vid) else {
                continue;
            };
            let header = sched.blocks[b]
                .predecessors
                .iter()
                .any(|&p| sched.dom.dominates(b, p));
            if header {
                let pc = v.bytecode_pc?;
                transfer.insert(pc);
                transfer.insert(resume(pc));
            }
            continue;
        }
        if ir_lower::op_cannot_deopt(&v.op) {
            continue;
        }
        let pc = v.bytecode_pc?;
        transfer.insert(pc);
        transfer.insert(resume(pc));
        if let ir::Op::Guard { bci } = v.op {
            transfer.insert(bci);
            transfer.insert(resume(bci));
        }
    }
    let mut snapshot_slots: Vec<(usize, ir::SafepointSlotKind, usize)> = Vec::new();
    for (si, sp) in graph.safepoints.iter().enumerate() {
        let mut named = false;
        for (k, &v) in sp.locals.iter().enumerate() {
            if v == alloc {
                named = true;
                snapshot_slots.push((si, ir::SafepointSlotKind::Local, k));
            }
        }
        for (k, &v) in sp.stack.iter().enumerate() {
            if v == alloc {
                named = true;
                snapshot_slots.push((si, ir::SafepointSlotKind::Stack, k));
            }
        }
        if sp.monitors.contains(&alloc) {
            return None;
        }
        if named
            && (transfer.contains(&sp.bci)
                || claimed.contains(&(si as u32))
                || sp.post_call_of != ir::NO_NODE)
        {
            return None;
        }
    }

    Some(EdgeSinkPlan {
        alloc,
        edge,
        join,
        phi,
        phi_slot,
        stores,
        loads: forwarded,
        mem_out,
        after_join,
        rewire,
        snapshot_slots,
    })
}

fn apply_edge_sink(graph: &mut ir::Graph, plan: EdgeSinkPlan) {
    let (new_op, alloc_pc) = {
        let n = &graph.nodes[plan.alloc as usize];
        (n.op.clone(), n.bytecode_pc)
    };
    // The materialized allocation and its stores, on the escaping edge.
    let fresh = graph.add(
        new_op,
        ir::IrType::Ref,
        vec![plan.edge, plan.mem_out],
        alloc_pc,
    );
    let mut mem = plan.mem_out;
    for &s in &plan.stores {
        let (op, ty, pc, mut inputs) = {
            let sn = &graph.nodes[s as usize];
            (sn.op.clone(), sn.ty, sn.bytecode_pc, sn.inputs.to_vec())
        };
        inputs[0] = plan.edge;
        inputs[1] = mem;
        inputs[2] = fresh;
        mem = graph.add(op, ty, inputs, pc);
    }
    // The join's memory: the edge's stores on the escaping side, the block's
    // own state on the other.
    let (join_inputs, join_pc) = {
        let jn = &graph.nodes[plan.join as usize];
        (jn.inputs.to_vec(), jn.bytecode_pc)
    };
    let mut phi_inputs = vec![plan.join];
    for &pred in &join_inputs {
        phi_inputs.push(if pred == plan.edge { mem } else { plan.mem_out });
    }
    let mem_phi = graph.add(ir::Op::Phi, ir::IrType::Memory, phi_inputs, join_pc);
    for &(u, slot) in &plan.after_join {
        graph.set_input(u, slot, mem_phi);
    }
    for &(u, slot, token) in &plan.rewire {
        graph.set_input(u, slot, token);
    }
    // The phi now receives the materialized object.
    graph.set_input(plan.phi, plan.phi_slot, fresh);
    // Frames no transfer reads: the deleted node is gone from them.
    for &(si, kind, k) in &plan.snapshot_slots {
        graph.set_safepoint_slot(si, kind, k, ir::NO_NODE);
    }
    // Loads read the stored values (operands and any snapshot naming them).
    for &(l, value) in &plan.loads {
        graph.replace_all_uses(l, value);
    }
    for &(l, _) in &plan.loads {
        graph.kill(l);
    }
    for &s in &plan.stores {
        graph.kill(s);
    }
    graph.kill(plan.alloc);
    if ir_stage_reporting() {
        eprintln!(
            "[ir] alloc-sink-to-edge: n{} -> n{fresh} on edge n{} (join n{}, {} store(s), {} load(s) forwarded)",
            plan.alloc,
            plan.edge,
            plan.join,
            plan.stores.len(),
            plan.loads.len(),
        );
    }
}
