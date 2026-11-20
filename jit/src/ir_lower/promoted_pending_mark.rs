#[allow(unused_imports)]
use super::*;

/// OSR2-3 (lane osr5): the deopt-point count before one node's lowering
/// ([`Lowerer::begin_promoted_pending`] / [`Lowerer::end_promoted_pending`]).
pub(in crate::ir_lower) struct PromotedPendingMark {
    pub(in crate::ir_lower) boxes: usize,
    pub(in crate::ir_lower) rethrows: usize,
    /// Wave 7 (lane osr7, OSR5-1): the node sits in a spliced body and was
    /// given the pad under `..._PENDING_CHAIN`, so every point it publishes
    /// must carry a caller chain.
    pub(in crate::ir_lower) spliced: bool,
}

/// JIT round 19 wave 5 (lane osr5, OSR2-3,
/// `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PENDING_DEOPT`): which in-loop stores
/// are silent, and each promoted cell's value right before every node of its
/// loop's body, along the FINAL schedule.
///
/// # Why the schedule, and why it is exact
///
/// A guard carries no memory edge: its order against a store is positional
/// inside a block, and the scheduler keeps every impure node (a guard, a
/// store) in its relative order (`ir_schedule`'s memory-order rule 3: a
/// safepoint never swaps with a memory-ordering node). The kept stores are
/// still in the graph, so the order the lowerer emits IS the program order of
/// the cell's writes against every deopt point. Walking each body block's
/// nodes in that order, a silent store sets its cell's current value to the
/// value it stores; the header block starts from the cell's header value (the
/// exit write-back's `v`, a phi the loop carries); a later block starts from
/// its predecessors' common value. Predecessors that disagree give `None` (the
/// promotion refuses a body memory phi, so this does not arise from its own
/// graphs); a deopt reached with `None` refuses the compile.
pub(crate) struct PromotedPending {
    /// The silent in-loop stores.
    pub(crate) silent: std::collections::HashSet<NodeId>,
    /// For each node of a promoted loop's body: `(exit write-back store,
    /// value before this node)` per cell of that loop.
    pub(crate) at_node: HashMap<NodeId, Vec<(NodeId, Option<NodeId>)>>,
    /// Every block of every such loop's body.
    pub(crate) body_blocks: Vec<usize>,
    /// OSR5-3 (wave 7, lane osr7): how many (join, cell) values came from a
    /// join's value phi (`CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PENDING_BRANCH`).
    pub(crate) join_phis: usize,
}

/// OSR5-3 (JIT round 19 wave 7, lane osr7,
/// `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PENDING_BRANCH`): cell `k`'s value at
/// the entry of join block `b` when its predecessors disagree -- the live
/// value phi of `b`'s merge whose input on every merge edge is exactly the
/// value cell `k` holds at the end of that edge's predecessor block (`out`).
/// The promotion's memory walk builds one such phi per body memory phi it
/// crosses (`loop_field_forward::Walk::phi_value`), so for a store under a
/// branch the back edge's value is this phi. `None` (the cell stays unknown,
/// and a deopt reached with it unknown refuses the compile) when `b` is not
/// a merge, an edge's predecessor is not resolved yet or holds an unknown
/// value, a predecessor of the block is reached by no merge edge, or no phi
/// matches.
pub(in crate::ir_lower) fn pending_join_phi(
    graph: &Graph,
    schedule: &Schedule,
    b: usize,
    k: usize,
    out: &HashMap<usize, Vec<Option<NodeId>>>,
) -> Option<NodeId> {
    let ctrl = schedule.blocks.get(b)?.ctrl;
    let region = graph.nodes.get(ctrl as usize)?;
    if !matches!(region.op, Op::Merge | Op::Region) {
        return None;
    }
    let mut edge_blocks: Vec<usize> = Vec::with_capacity(region.inputs.len());
    let mut want: Vec<NodeId> = Vec::with_capacity(region.inputs.len());
    for &c in &region.inputs {
        let pb = ctrl_block_of(graph, schedule, c)?;
        want.push((*out.get(&pb)?.get(k)?)?);
        edge_blocks.push(pb);
    }
    if schedule.blocks[b]
        .predecessors
        .iter()
        .any(|p| !edge_blocks.contains(p))
    {
        return None;
    }
    graph
        .nodes
        .iter()
        .enumerate()
        .find(|(_, n)| {
            n.op == Op::Phi
                && matches!(n.ty, IrType::Int | IrType::Long)
                && n.inputs.first() == Some(&ctrl)
                && n.inputs.len() == want.len() + 1
                && n.inputs[1..] == want[..]
        })
        // Cast: an arena index is a `NodeId`.
        .map(|(id, _)| id as NodeId)
}

/// Build [`PromotedPending`]; `Ok(None)` when the graph keeps no silent store.
/// `Err` names why the plan is not sound for this schedule (the compile is
/// then refused at the first node of such a loop).
pub(crate) fn plan_promoted_pending(
    graph: &Graph,
    schedule: &Schedule,
) -> Result<Option<PromotedPending>, &'static str> {
    use std::collections::HashSet;
    if graph.promoted_sunk_stores.is_empty() {
        return Ok(None);
    }
    let mut silent: HashSet<NodeId> = HashSet::new();
    let mut at_node: HashMap<NodeId, Vec<(NodeId, Option<NodeId>)>> = HashMap::new();
    let mut regions: Vec<NodeId> = Vec::new();
    let mut body_blocks: Vec<usize> = Vec::new();
    let mut join_phis = 0usize;
    for &(exit, _) in &graph.promoted_sunk_stores {
        let region = graph
            .promoted_cell_writebacks
            .iter()
            .find(|&&(_, s)| s == exit)
            .map(|&(r, _)| r)
            .ok_or("an exit store with no header")?;
        if !regions.contains(&region) {
            regions.push(region);
        }
    }
    let block_of = |v: NodeId| {
        schedule
            .node_to_block
            .get(v as usize)
            .copied()
            .filter(|&b| b != usize::MAX)
    };
    for region in regions {
        // (exit store, cell, its silent stores). JIT round 21 (lane promote,
        // TYPECHECK21-9): a static cell too (`pending_cell_of`).
        let mut cells: Vec<(NodeId, PendingCell, Vec<NodeId>)> = Vec::new();
        for &(r, exit) in &graph.promoted_cell_writebacks {
            if r != region {
                continue;
            }
            let shape = pending_cell_of(graph, exit).ok_or("an exit store lost its shape")?;
            let exit_op = graph.nodes.get(exit as usize).map(|n| n.op.clone());
            let sunk: Vec<NodeId> = graph
                .promoted_sunk_stores
                .iter()
                .filter(|&&(e, _)| e == exit)
                .map(|&(_, st)| st)
                .collect();
            for &st in &sunk {
                let n = graph
                    .nodes
                    .get(st as usize)
                    .filter(|n| n.op != Op::Dead)
                    .ok_or("a silent store was deleted")?;
                // A static cell's store names the cell in its op alone
                // (class, field, tag, volatility), which the op comparison
                // checks.
                let same_cell = match shape.base {
                    Some(base) => {
                        n.inputs.len() >= 5
                            && n.input_opt(2) == Some(base)
                            && n.input_opt(3)
                                .and_then(|i| graph.nodes.get(i as usize))
                                .is_some_and(|i| i.op == Op::Const(shape.field_index))
                    }
                    None => n.inputs.len() >= 3,
                };
                if Some(n.op.clone()) != exit_op || !same_cell {
                    return Err("a silent store is not its cell's");
                }
                silent.insert(st);
            }
            cells.push((exit, shape, sunk));
        }
        let header = schedule
            .blocks
            .iter()
            .position(|b| b.ctrl == region)
            .ok_or("no header block")?;
        let dom = &schedule.dom;
        let mut body: HashSet<usize> = HashSet::new();
        body.insert(header);
        let mut work: Vec<usize> = schedule.blocks[header]
            .predecessors
            .iter()
            .copied()
            .filter(|&p| dom.is_reachable(p) && dom.dominates(header, p))
            .collect();
        if work.is_empty() {
            return Err("no back edge");
        }
        while let Some(b) = work.pop() {
            if body.insert(b) {
                work.extend(schedule.blocks[b].predecessors.iter().copied());
            }
        }
        if cells.iter().any(|(_, _, sunk)| {
            sunk.iter()
                .any(|&st| !block_of(st).is_some_and(|b| body.contains(&b)))
        }) {
            return Err("a silent store outside its loop");
        }
        let entry: Vec<Option<NodeId>> = cells.iter().map(|c| Some(c.1.value)).collect();
        let mut order: Vec<usize> = body.iter().copied().collect();
        order.sort_unstable();
        let mut out: HashMap<usize, Vec<Option<NodeId>>> = HashMap::new();
        for _ in 0..=order.len() {
            let mut progressed = false;
            for &b in &order {
                if out.contains_key(&b) {
                    continue;
                }
                let state_in = if b == header {
                    entry.clone()
                } else {
                    let mut acc: Option<Vec<Option<NodeId>>> = None;
                    let mut ready = true;
                    let mut disagree = false;
                    for &p in &schedule.blocks[b].predecessors {
                        if !body.contains(&p) {
                            return Err("a side entry into the loop");
                        }
                        let Some(o) = out.get(&p) else {
                            ready = false;
                            break;
                        };
                        acc = Some(match acc {
                            None => o.clone(),
                            Some(a) => a
                                .iter()
                                .zip(o)
                                .map(|(x, y)| {
                                    if x == y {
                                        *x
                                    } else {
                                        disagree = true;
                                        None
                                    }
                                })
                                .collect(),
                        });
                    }
                    if !ready {
                        continue;
                    }
                    let mut merged = acc.ok_or("a body block with no predecessor")?;
                    // OSR5-3 (wave 7, lane osr7, `..._PENDING_BRANCH`): a cell
                    // the predecessors disagree on is the join's value phi
                    // whose inputs are exactly their values, edge by edge.
                    if disagree && pending_join_values_enabled() {
                        for (k, slot) in merged.iter_mut().enumerate() {
                            if slot.is_none() {
                                *slot = pending_join_phi(graph, schedule, b, k, &out);
                                if slot.is_some() {
                                    join_phis += 1;
                                }
                            }
                        }
                    }
                    merged
                };
                let mut state = state_in;
                for &n in &schedule.blocks[b].nodes {
                    if let Some(k) = cells.iter().position(|c| c.2.contains(&n)) {
                        state[k] = graph
                            .nodes
                            .get(n as usize)
                            .and_then(|x| x.input_opt(cells[k].1.value_slot));
                        continue;
                    }
                    at_node.insert(
                        n,
                        cells.iter().zip(&state).map(|(c, v)| (c.0, *v)).collect(),
                    );
                }
                out.insert(b, state);
                progressed = true;
            }
            if out.len() == order.len() || !progressed {
                break;
            }
        }
        if out.len() != order.len() {
            return Err("an irreducible loop body");
        }
        body_blocks.extend(order);
    }
    Ok(Some(PromotedPending {
        silent,
        at_node,
        body_blocks,
        join_phis,
    }))
}

/// OSR2-3: the values a promoted loop's deopt pads read, which therefore keep
/// a home of their own for the whole method (`plan_slots` pins them beside
/// the scalar-replacement field values) and are deopt-named for the home-drop
/// rule (`compute_deopt_named_reachable`): every silent store's value, and
/// each cell's header value, entry value and base. Empty for every graph
/// without silent stores.
pub(in crate::ir_lower) fn promoted_pending_pin_values(graph: &Graph) -> Vec<NodeId> {
    let mut out = Vec::new();
    for &(exit, st) in &graph.promoted_sunk_stores {
        // JIT round 21 (lane promote, TYPECHECK21-9): a static cell's store
        // holds its value in slot 2 and has no base.
        let Some(cell) = pending_cell_of(graph, exit) else {
            continue;
        };
        if let Some(v) = graph
            .nodes
            .get(st as usize)
            .filter(|n| n.op != Op::Dead)
            .and_then(|n| n.input_opt(cell.value_slot))
        {
            out.push(v);
        }
        out.extend([cell.value, cell.init]);
        out.extend(cell.base);
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// [`promoted_pending_pin_values`] as `plan_slots` pins them (a home word no
/// other value shares, so a home that IS written stays readable at every
/// pad). Under `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PENDING_BRANCH` (OSR5-3,
/// lane osr7) also every value the pending plan names at some node -- which
/// adds the joins' value phis; empty addition when the plan is refuted (that
/// compile is refused at its first promoted node anyway).
pub(crate) fn promoted_pending_slot_pins(graph: &Graph, schedule: &Schedule) -> Vec<NodeId> {
    let mut out = promoted_pending_pin_values(graph);
    if out.is_empty() || !pending_join_values_enabled() {
        return out;
    }
    if let Ok(Some(plan)) = plan_promoted_pending(graph, schedule) {
        for states in plan.at_node.values() {
            out.extend(states.iter().filter_map(|&(_, v)| v));
        }
        out.sort_unstable();
        out.dedup();
    }
    out
}

/// The pending values `compute_deopt_named_reachable` marks deopt-named, which
/// keeps their home words written unless they own their register for the
/// whole method. [`promoted_pending_slot_pins`] -- or, under
/// `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PENDING_RESIDENT` (OSR6-1, lane osr7),
/// only each cell's entry read and base, written once before the loop: the
/// per-iteration values are then described by their resident register at a
/// pad (`Lowerer::pending_resident_reg`), or by a home that was kept for
/// another reason, or the compile is refused.
pub(crate) fn promoted_pending_deopt_named(graph: &Graph, schedule: &Schedule) -> Vec<NodeId> {
    if !ir_loop_field_promote_pending_resident_enabled() {
        return promoted_pending_slot_pins(graph, schedule);
    }
    let mut out = Vec::new();
    for &(exit, _) in &graph.promoted_sunk_stores {
        // TYPECHECK21-9 (lane promote): a static cell has no base.
        if let Some(cell) = pending_cell_of(graph, exit) {
            out.push(cell.init);
            out.extend(cell.base);
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PENDING_LIVE=1` (JIT round 19
/// proposal wave 8, lane osr8, OSR7-1). **default on**; inert without
/// `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PENDING_RESIDENT` (and so without
/// `..._PENDING_DEOPT`).
///
/// Under `..._RESIDENT` a pad describes a pending value by its register only
/// when the allocator's live range strictly contains the point
/// ([`Lowerer::pending_resident_reg`]). A value whose last real read comes
/// before the point -- the cell's header value at a guard after the cell's
/// last read and before its store (`R19Osr7Pending.guardAfterUse`) -- fails
/// that, and then needs a kept home or refuses the compile. Under this switch
/// every value the pending plan names at a node that can deopt is LIVE there,
/// to one position past the point: a read at the point is added to its
/// liveness, the backward propagation done for it (every block on a path from
/// its definition to the point is live-in, every predecessor of one live-out)
/// and the value's range widened to that hull, in the slot plan and in the
/// allocator's model alike ([`pending_deopt_live_widening`]). The allocation
/// is then verified against the wider ranges, so no other value holds the
/// register anywhere on those paths.
pub(crate) fn ir_loop_field_promote_pending_live_enabled() -> bool {
    true
}

/// OSR7-1: one pending value's extra liveness ([`pending_deopt_live_widening`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingLiveRow {
    /// The pending value.
    pub(crate) value: NodeId,
    /// The lowest position the extra reads make it live at.
    pub(crate) lo: usize,
    /// The highest such position (one past a point, at least).
    pub(crate) hi: usize,
    /// Blocks the extra reads make it live on entry to.
    pub(crate) live_in: Vec<usize>,
}

/// OSR7-1: the rows, over the block spans they were computed in (the
/// position numbering `regalloc::liveness_core` uses: each scheduled node,
/// the terminator, one outgoing-edge position per block).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PendingLiveWidening {
    pub(crate) span: Vec<(usize, usize)>,
    pub(crate) rows: Vec<PendingLiveRow>,
}

/// OSR7-1 (JIT round 19 wave 8, lane osr8,
/// [`ir_loop_field_promote_pending_live_enabled`]): for every node of a
/// promoted loop that can deopt, and every `int`/`long` value the pending
/// plan names there that `plan_slots` pins (`promoted_pending_slot_pins`, a
/// home word nobody else shares, so a wider range costs that home nothing),
/// the liveness a read at the point and at the next position would add. Empty
/// when the switch (or `..._RESIDENT`) is off, the graph keeps no silent
/// store, or the plan is refuted (that compile is refused anyway).
///
/// A value is skipped (keeps today's range: the pad then needs a kept home or
/// the compile is refused, as before) when its definition is not found, or
/// sits after the point in the point's own block, or the backward walk from
/// the point never meets the defining block (a value that does not dominate
/// the point, which the plan never names).
pub(crate) fn pending_deopt_live_widening(
    graph: &Graph,
    schedule: &Schedule,
) -> PendingLiveWidening {
    use std::collections::HashSet;
    let mut out = PendingLiveWidening::default();
    if graph.promoted_sunk_stores.is_empty()
        || !ir_loop_field_promote_pending_resident_enabled()
        || !ir_loop_field_promote_pending_live_enabled()
    {
        return out;
    }
    let Ok(Some(plan)) = plan_promoted_pending(graph, schedule) else {
        return out;
    };
    let pins: HashSet<NodeId> = promoted_pending_slot_pins(graph, schedule)
        .into_iter()
        .collect();
    let n = graph.nodes.len();
    let nb = schedule.blocks.len();
    let mut pos_of: Vec<Option<usize>> = vec![None; n];
    let mut span: Vec<(usize, usize)> = Vec::with_capacity(nb);
    let mut seq = 0usize;
    for block in &schedule.blocks {
        let start = seq;
        for &nid in &block.nodes {
            if let Some(cell) = pos_of.get_mut(nid as usize) {
                *cell = Some(seq);
            }
            seq += 1;
        }
        if let Some(term) = block.terminator {
            if let Some(cell) = pos_of.get_mut(term as usize) {
                *cell = Some(seq);
            }
            seq += 1;
        }
        span.push((start, seq));
        seq += 1;
    }
    let block_of = |v: NodeId| {
        schedule
            .node_to_block
            .get(v as usize)
            .copied()
            .filter(|&b| b < nb)
    };
    // `(block, position)` a value is defined at: a φ at its merge block's
    // first position, anything else where the schedule put it.
    let def_at = |v: NodeId| -> Option<(usize, usize)> {
        let node = graph.nodes.get(v as usize)?;
        if node.op == Op::Phi {
            let c = node.input_opt(0)?;
            let b = schedule.blocks.iter().position(|b| b.ctrl == c)?;
            Some((b, span[b].0))
        } else {
            Some((block_of(v)?, pos_of.get(v as usize).copied().flatten()?))
        }
    };
    let mut points: Vec<(NodeId, &Vec<(NodeId, Option<NodeId>)>)> =
        plan.at_node.iter().map(|(&k, v)| (k, v)).collect();
    points.sort_unstable_by_key(|&(k, _)| k);
    let mut rows: Vec<PendingLiveRow> = Vec::new();
    for (at, states) in points {
        let Some(node) = graph.nodes.get(at as usize) else {
            continue;
        };
        if node_cannot_deopt(graph, node) {
            continue;
        }
        let (Some(p), Some(pb)) = (pos_of.get(at as usize).copied().flatten(), block_of(at)) else {
            continue;
        };
        // One past the point: the next node, the terminator or the edge
        // position, all inside the point's own block.
        let past = (p + 1).min(span[pb].1);
        for &(_, v) in states {
            let Some(v) = v else {
                continue;
            };
            if !pins.contains(&v) {
                continue;
            }
            let Some(vn) = graph.nodes.get(v as usize) else {
                continue;
            };
            if !matches!(vn.ty, IrType::Int | IrType::Long) || matches!(vn.op, Op::Const(_)) {
                continue;
            }
            let Some((db, dp)) = def_at(v) else {
                continue;
            };
            let mut lo = p.min(dp);
            let mut hi = past;
            let mut live_in: Vec<usize> = Vec::new();
            if db == pb {
                // Defined in the point's own block: before it (a φ at the
                // block's top counts), or not a value the plan may name here.
                let before = if vn.op == Op::Phi { dp <= p } else { dp < p };
                if !before {
                    continue;
                }
            } else {
                let mut seen = vec![false; nb];
                let mut work = vec![pb];
                let mut met_def = false;
                while let Some(b) = work.pop() {
                    if seen[b] {
                        continue;
                    }
                    seen[b] = true;
                    live_in.push(b);
                    lo = lo.min(span[b].0);
                    for &q in &schedule.blocks[b].predecessors {
                        if q >= nb {
                            continue;
                        }
                        // Live out of every predecessor of a live-in block.
                        hi = hi.max(span[q].1);
                        lo = lo.min(span[q].1);
                        if q == db {
                            met_def = true;
                        } else {
                            work.push(q);
                        }
                    }
                }
                if !met_def {
                    continue;
                }
            }
            live_in.sort_unstable();
            match rows.iter_mut().find(|r| r.value == v) {
                Some(row) => {
                    row.lo = row.lo.min(lo);
                    row.hi = row.hi.max(hi);
                    row.live_in.extend(live_in);
                    row.live_in.sort_unstable();
                    row.live_in.dedup();
                }
                None => rows.push(PendingLiveRow {
                    value: v,
                    lo,
                    hi,
                    live_in,
                }),
            }
        }
    }
    out.span = span;
    out.rows = rows;
    out
}

/// OSR7-1: widen the slot plan's ranges by [`pending_deopt_live_widening`]'s
/// rows (pinned values only, so no colouring changes). Returns how many
/// ranges grew.
pub(in crate::ir_lower) fn widen_slot_plan_for_pending(
    plan: &mut SlotPlan,
    widening: &PendingLiveWidening,
) -> usize {
    let mut grew = 0usize;
    for row in &widening.rows {
        if let Some(Some(r)) = plan.range.get_mut(row.value as usize) {
            let (lo, hi) = (r.lo.min(row.lo), r.hi.max(row.hi));
            if (lo, hi) != (r.lo, r.hi) {
                r.lo = lo;
                r.hi = hi;
                grew += 1;
            }
        }
    }
    grew
}

/// OSR7-1: widen the allocator's liveness model by the same rows: each
/// value's range to the hull, and its live-in bit in every block the extra
/// reads make it live on entry to. Nothing when the model did not converge
/// (every range is the whole method) or numbers its blocks differently.
pub(crate) fn widen_live_model_for_pending(
    live: &mut crate::regalloc::LiveModel,
    widening: &PendingLiveWidening,
) -> usize {
    if widening.rows.is_empty() || !live.converged || live.span != widening.span {
        return 0;
    }
    let mut grew = 0usize;
    for row in &widening.rows {
        let v = row.value as usize;
        if let Some(Some(r)) = live.range.get_mut(v) {
            let (lo, hi) = (r.lo.min(row.lo), r.hi.max(row.hi));
            if (lo, hi) != (r.lo, r.hi) {
                r.lo = lo;
                r.hi = hi;
                grew += 1;
            }
        } else {
            continue;
        }
        for &b in &row.live_in {
            if let Some(word) = live
                .block_live_in
                .get_mut(b)
                .and_then(|bits| bits.get_mut(v / 64))
            {
                *word |= 1u64 << (v % 64);
            }
        }
    }
    grew
}

/// `MOV dst, src` for one write-back operand (64-bit).
pub(in crate::ir_lower) fn emit_cell_writeback_load(
    buf: &mut ExecutableBuffer,
    dst: u8,
    src: CellWriteBackSrc,
) {
    let r = u8::from(dst >= 8);
    match src {
        CellWriteBackSrc::Frame(off) => {
            // MOV r64, [RBP + disp32]: REX.W(+R) 8B /r, mod=10 rm=101.
            buf.emit(&[0x48 | (r << 2), 0x8B, 0x80 | ((dst & 7) << 3) | 5]);
            buf.emit(&off.to_le_bytes());
        }
        CellWriteBackSrc::Reg(s) => {
            // MOV r/m64(dst), r64(src): REX.W(+R src)(+B dst) 89 /r.
            buf.emit(&[
                0x48 | (u8::from(s >= 8) << 2) | r,
                0x89,
                0xC0 | ((s & 7) << 3) | (dst & 7),
            ]);
        }
        CellWriteBackSrc::Imm(v) => {
            // MOV r64, imm64: REX.W(+B) B8+r.
            buf.emit(&[0x48 | r, 0xB8 | (dst & 7)]);
            buf.emit(&v.to_le_bytes());
        }
    }
}

/// Emit one write-back (OSR2-1): `RAX = value; R11 = init; CMP; JE skip;
/// arg2 = RAX; arg0 = [RBP + base_off]; arg1 = field_index; CALL helper;
/// skip:`. The compare is 32-bit for an `int` cell (the helper stores the low
/// half, as the loop's own store did) and 64-bit for a `long` one. Clobbers
/// RAX, R11, the flags and whatever the helper's ABI lets it clobber; reads
/// no register but the operands'. Stack alignment and the Win64 home space
/// are the poll slow path's own, whose `CALL` ran at the same `RSP`.
pub(in crate::ir_lower) fn emit_cell_writeback(buf: &mut ExecutableBuffer, wb: &CellWriteBack) {
    emit_cell_writeback_load(buf, RAX, wb.value);
    emit_cell_writeback_load(buf, R11, wb.init);
    if wb.long {
        buf.emit(&[0x4C, 0x39, 0xD8]); // CMP RAX, R11
    } else {
        buf.emit(&[0x44, 0x39, 0xD8]); // CMP EAX, R11D
    }
    buf.emit(&[0x0F, 0x84]); // JE skip
    let skip = buf.pos();
    buf.emit(&[0, 0, 0, 0]);
    if let Some(sc) = wb.static_cell {
        // JIT round 20 wave 11 (lane pLoop, OSR4-2): a static cell.
        emit_static_cell_store(buf, &sc);
    } else {
        emit_cell_writeback_load(buf, CALL_ARG_REGS[2], CellWriteBackSrc::Reg(RAX));
        emit_cell_writeback_load(buf, CALL_ARG_REGS[0], CellWriteBackSrc::Frame(wb.base_off));
        emit_cell_writeback_load(buf, CALL_ARG_REGS[1], CellWriteBackSrc::Imm(wb.field_index));
        // Cast: the helper's address bits as the immediate.
        emit_cell_writeback_load(buf, RAX, CellWriteBackSrc::Imm(wb.helper as i64));
        buf.emit(&[0xFF, 0xD0]); // CALL RAX
    }

    // Cast: buffer offsets fit a rel32 (an overflowed buffer bails the compile).
    let rel = buf.pos() as i32 - (skip as i32 + 4);
    // Tolerated on an overflowed buffer, as `Lowerer::patch_or_bail`.
    let _ = buf.try_patch_i32(skip, rel);
}

/// t1 wave 8 (lane PROMOTE, PROMOTE21-2): is every write-back of a deopt
/// pad a promoted STATIC cell's? Such a pad is inline stores through RAX /
/// RCX / R11 ([`emit_static_cell_store`]) with no helper call, so it cannot
/// disturb an XMM register a frame names. `false` for no pad or an empty one.
pub(in crate::ir_lower) fn pad_writebacks_all_static(wbs: Option<&[CellWriteBack]>) -> bool {
    wbs.is_some_and(|w| !w.is_empty() && w.iter().all(|wb| wb.static_cell.is_some()))
}

/// JIT round 20 wave 11 (lane pLoop, OSR4-2): store RAX (the cell's new
/// value) into a promoted static cell, the words
/// `Lowerer::emit_inline_putstatic` writes for the same field: word 0 =
/// `tag | payload32 << 32` and word 1 = 0 for an `int`-category value (tag
/// `Value::Int` = 0), word 0 = `tag` (`Value::Long` = 1) and word 1 = the
/// payload for a `long`. The statics block is re-read through its base cell,
/// so a republished block is followed. Clobbers R11, RCX and the flags --
/// all free at a poll pad, whose field arm calls a helper that may clobber
/// every caller-saved register; reads no register but RAX.
pub(in crate::ir_lower) fn emit_static_cell_store(
    buf: &mut ExecutableBuffer,
    sc: &StaticCellWriteBack,
) {
    // MOV R11, imm64(&base_cell) ; MOV R11, [R11]
    // Cast: the cell's address bits as the immediate.
    emit_cell_writeback_load(buf, R11, CellWriteBackSrc::Imm(sc.base_cell as i64));
    buf.emit(&[0x4D, 0x8B, 0x1B]); // MOV R11, [R11]
    if sc.long {
        // MOV qword [R11 + disp32], imm32 (REX.W+B C7 /0, mod 10, rm 011).
        buf.emit(&[0x49, 0xC7, 0x83]);
        buf.emit(&sc.cell_off.to_le_bytes());
        buf.emit(&1u32.to_le_bytes()); // `Value::Long`
                                       // MOV qword [R11 + disp32], RAX (REX.W+B 89 /r, reg RAX).
        buf.emit(&[0x49, 0x89, 0x83]);
        buf.emit(&sc.hi_off.to_le_bytes());
    } else {
        buf.emit(&[0x89, 0xC1]); // MOV ECX, EAX (zero-extends)
        buf.emit(&[0x48, 0xC1, 0xE1, 0x20]); // SHL RCX, 32 (tag `Value::Int` = 0)
                                             // MOV qword [R11 + disp32], RCX (REX.W+B 89 /r, reg RCX).
        buf.emit(&[0x49, 0x89, 0x8B]);
        buf.emit(&sc.cell_off.to_le_bytes());
        buf.emit(&[0x31, 0xC9]); // XOR ECX, ECX
        buf.emit(&[0x49, 0x89, 0x8B]); // MOV qword [R11 + disp32], RCX
        buf.emit(&sc.hi_off.to_le_bytes());
    }
}

/// t1 wave 8 (lane PROMOTE): does `v` name an XMM register as a home, itself
/// or through a scalar-replaced object's fields (the visit
/// [`frame_named_xmms`] makes, for one value)?
pub(in crate::ir_lower) fn frame_value_names_xmm(v: &FrameValue, depth: usize) -> bool {
    match v {
        FrameValue::XmmFloat(_) | FrameValue::XmmDouble(_) => true,
        FrameValue::VirtualObject(state) if depth < 64 => state
            .field_values
            .iter()
            .any(|f| frame_value_names_xmm(f, depth + 1)),
        _ => false,
    }
}

/// OSR5-4 (lane osr6): every XMM register `fs` names as a value's home,
/// sorted and deduplicated -- its locals, operand stack and monitor objects,
/// the fields of every scalar-replaced object it describes, across the whole
/// caller chain. What a write-back pad's helper call must not disturb.
pub(in crate::ir_lower) fn frame_named_xmms(fs: &FrameState) -> Vec<u8> {
    fn visit(v: &FrameValue, out: &mut Vec<u8>, depth: usize) {
        match v {
            FrameValue::XmmFloat(x) | FrameValue::XmmDouble(x) => out.push(*x),
            FrameValue::VirtualObject(state) if depth < 64 => {
                for f in &state.field_values {
                    visit(f, out, depth + 1);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    let mut scope = Some(fs);
    let mut seen = 0usize;
    while let Some(f) = scope {
        let monitor_objects = f.monitors.iter().map(|m| &m.object);
        for v in f.locals.iter().chain(f.stack.iter()).chain(monitor_objects) {
            visit(v, &mut out, 0);
        }
        seen += 1;
        if seen >= 64 {
            break;
        }
        scope = f.caller.as_deref();
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// The bytes [`emit_pad_xmm_save`] moves `RSP` by for `n` registers: the
/// 32-byte Win64 home space the pad's helper calls own at the new `RSP`, then
/// one 8-byte slot per register, rounded to 16 so the calls stay aligned.
pub(in crate::ir_lower) fn pad_xmm_save_bytes(n: usize) -> i32 {
    // Cast: at most 16 registers, so a few hundred bytes.
    32 + ((n * 8 + 15) & !15) as i32
}

/// `MOVSD [RSP + disp32], xmm` (`store`) or `MOVSD xmm, [RSP + disp32]`.
pub(in crate::ir_lower) fn emit_movsd_rsp(
    buf: &mut ExecutableBuffer,
    xmm: u8,
    disp: i32,
    store: bool,
) {
    buf.emit_byte(0xF2);
    if xmm >= 8 {
        buf.emit_byte(0x44); // REX.R
    }
    // 0F 11 /r (store) or 0F 10 /r (load); ModRM mod=10 reg=xmm rm=100 (SIB),
    // SIB base=RSP no index.
    buf.emit(&[
        0x0F,
        if store { 0x11 } else { 0x10 },
        0x84 | ((xmm & 7) << 3),
        0x24,
    ]);
    buf.emit(&disp.to_le_bytes());
}

/// OSR5-4 (lane osr6, `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PAD_XMM_SAVE`):
/// before a pad's write-backs, `SUB RSP, k; MOVSD [RSP + 32 + 8i], xmm_i` for
/// each register in `xmms` (nothing at all for none). The pad runs at an
/// `RSP` where the body's helper calls run, so moving it by a multiple of 16
/// keeps every write-back's `CALL` aligned, and the saves sit above the new
/// home space, where no callee writes. Paired with [`emit_pad_xmm_restore`].
pub(in crate::ir_lower) fn emit_pad_xmm_save(buf: &mut ExecutableBuffer, xmms: &[u8]) {
    if xmms.is_empty() {
        return;
    }
    buf.emit(&[0x48, 0x81, 0xEC]); // SUB RSP, imm32
    buf.emit(&pad_xmm_save_bytes(xmms.len()).to_le_bytes());
    for (i, &x) in xmms.iter().enumerate() {
        // Cast: i < 16.
        emit_movsd_rsp(buf, x, 32 + 8 * i as i32, true);
    }
}

/// The restore half of [`emit_pad_xmm_save`]: reload each register's low 64
/// bits (all a frame's `XmmFloat` / `XmmDouble` names; the pad never returns
/// into the body, so the upper halves are dead) and give `RSP` back.
pub(in crate::ir_lower) fn emit_pad_xmm_restore(buf: &mut ExecutableBuffer, xmms: &[u8]) {
    if xmms.is_empty() {
        return;
    }
    for (i, &x) in xmms.iter().enumerate() {
        // Cast: i < 16.
        emit_movsd_rsp(buf, x, 32 + 8 * i as i32, false);
    }
    buf.emit(&[0x48, 0x81, 0xC4]); // ADD RSP, imm32
    buf.emit(&pad_xmm_save_bytes(xmms.len()).to_le_bytes());
}

/// `CRATONVM_JIT_IR_GP_WIDE=1` — widen the GP file to the seven registers Win64
/// makes callee-saved (RBX, R12–R15 **and RSI/RDI**) instead of the historical
/// five. **Default OFF, and off because it was MEASURED, not because it is
/// unsoaked.**
///
/// It engages, and engaging is not the same as paying. On
/// `probes/FieldLoop.java` (`sum`, one accumulator, `probe.reps=25000`), one
/// binary, arms interleaved BAAB/ABBA with a second copy of the OFF arm as the
/// control:
///
/// ```text
/// resident 5 -> 6   splits 5 -> 2   scan_spills 1 -> 0   scan_reloads 2 -> 0
/// A (off)  909 ms | C (control) 903 ms | B (on) 948 ms
/// noise floor 0.7%   effect +4.6%   => ON is SLOWER, above the floor
/// ```
///
/// Better residency, worse code, and the write-through value model is why:
/// `lower_data_node` stores every value to its home word whether or not it also
/// got a register, so promoting a value ADDS its publish move and REMOVES no
/// store. A wider file buys more publishes, not fewer stores. That is the same
/// conclusion `docs/jit/linear-scan-wiring.md` reaches from the other end
/// ("it buys loads, not stores"), reached here by widening the file until the
/// cost showed.
///
/// The numbers above were taken while the frame reserved all seven slots in
/// BOTH arms, so they isolate residency rather than frame size. That is no
/// longer how it is wired: [`ir_saved_gpr_bytes`] sizes the save area from
/// [`ir_gp_file`], so the reservation now follows the handout and the OFF arm
/// is byte-identical to the pre-widening tree. Re-running this A/B therefore
/// compares seven-slot frames against five-slot ones and can only look BETTER
/// for the OFF arm than the 4.6% above — which is why the number is recorded
/// as the one taken under the older wiring rather than silently re-attributed.
///
/// Worth re-running if home-slot elimination ever lands: the register file is
/// not the binding constraint while the frame stays authoritative, and this
/// flag is the ready-made instrument for asking again once it does not.
pub(in crate::ir_lower) fn ir_gp_wide_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        matches!(
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
            Ok("1") | Ok("true") | Ok("on") | Ok("yes")
        )
    })
}

/// The GP registers THIS compile may allocate over.
///
/// [`IR_LOWER_LS_GPRS`] is the widest file the platform's ABI offers; this is
/// the subset THIS compile may hand out, and they differ only when the kill
/// switch is off. The reservation follows the handout — [`ir_saved_gpr_bytes`]
/// and `Lowerer::saved_gpr_regs` both size from here, not from the constant —
/// so with the switch off the frame is byte-identical to the pre-widening tree
/// rather than paying 16 bytes for two registers nothing can name.
///
/// Safe only because this is a `OnceLock`: every reader resolves the same
/// answer for the life of the process, so the frame layout cannot drift from
/// the handout between `Lowerer::new` and an emission site. That is exactly
/// the hazard `saved_xmm_bytes`' own comment warns about, and the `OnceLock`
/// is what discharges it — a per-compile or thread-local re-read here would
/// reintroduce it.
pub(in crate::ir_lower) fn ir_gp_file() -> &'static [u8] {
    if ir_gp_wide_enabled() {
        IR_LOWER_LS_GPRS
    } else {
        &IR_LOWER_LS_GPRS[..crate::regalloc::xmm_roles::IR_GP_LINEAR_SCAN_NARROW]
    }
}

/// `CRATONVM_JIT_IR_LS_CALLER_SAVED` (JIT round 25, item 1b of
/// `docs/feature-designs/regex-jit-contract.md`,
/// "Round 24"): a value whose whole live range lies inside one block and
/// crosses only nodes whose lowering writes no caller-saved GPR takes one of
/// [`IR_LS_CALLER_SAVED_GPRS`] (R8-R11) instead of competing for the
/// callee-saved file, which then serves the values that cross calls.
/// **Default ON since j27 2026-10-28**, `=0` kills; read per compile (nothing
/// the frame layout depends on reads it).
///
/// Why `CRATONVM_JIT_IR_GP_WIDE` could not answer the question: on System V --
/// the host the regex rows are counted on -- it is a silent no-op
/// (`IR_GP_LINEAR_SCAN` is already the whole callee-saved set and its narrow
/// prefix is five), so the round-24 "GP_WIDE within noise" arm ran byte-identical
/// code. On Win64 it adds RSI/RDI, which are callee-saved too and paid for by a
/// prologue save and a restore at every exit.
///
/// The rule, the clobber audit and what is left:
/// [`plan_caller_saved_residency`], [`ir_op_caller_saved_transparent`] and
/// `docs/feature-designs/jit-ir-optimizations.md`.
pub(in crate::ir_lower) fn ir_ls_caller_saved_enabled() -> bool {
    // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
    true
}

/// JIT round 25 review (R5 of
/// `docs/feature-designs/jit-ir-optimizations.md`):
/// may this compile hand out the caller-saved file at all? The switch, and no
/// level-2 machine list (`MirMode::Verify` / `MirMode::Emit`,
/// `CRATONVM_JIT_IR_ISEL_VERIFY` / `CRATONVM_JIT_IR_ISEL_EMIT`). The
/// transparency audit ([`ir_op_caller_saved_transparent`]) covers the
/// per-opcode arms only; an emitted MIR tile replaces an arm and was never
/// audited for R8-R11, and the verifier compares the tiles against an
/// allocation that never had a caller-saved register. Read once per compile.
pub(in crate::ir_lower) fn ir_ls_caller_saved_admitted() -> bool {
    caller_saved_admitted_with_mir(
        ir_ls_caller_saved_enabled(),
        isel_emit_enabled() || isel_verify_enabled(),
    )
}

/// [`ir_ls_caller_saved_admitted`] as a predicate. Pure.
pub(in crate::ir_lower) fn caller_saved_admitted_with_mir(switch_on: bool, mir_on: bool) -> bool {
    switch_on && !mir_on
}

/// Does `node`'s lowering leave every register of [`IR_LS_CALLER_SAVED_GPRS`]
/// untouched, on every path that returns into the body? The one predicate both
/// [`plan_caller_saved_residency`] (which values may hold one) and
/// `Lowerer::lower_block` (where residency in one ends) ask, so the two cannot
/// disagree.
///
/// Audited arm by arm (round 25, item 1b):
///
/// * `Const`: `MOV qword [home], imm32`, or `MOV RAX, imm64` + `store_rax`.
/// * integer `Add` / `Sub` / `Mul` / `Neg` / `And` / `Or` / `Xor` / `Shl` /
///   `Shr` / `UShr` / `I2L` / `L2I`: the accumulator in RAX, the second
///   operand in RCX, folded as an immediate or read in place from its
///   register; `LEA` into RAX or into the result's OWN register; then
///   `store_rax`, which may publish into the result's own register and install
///   an RAX / RCX carry.
/// * `Cmp`: `CMP RAX, RCX; SETcc AL; MOVZX EAX, AL` + `store_rax`, or nothing
///   when fused (its operands are then read by the consumer).
/// * `LCmp`: as `Cmp`, plus `SETL DL; MOVZX EDX, DL` -- RDX, not in the set.
/// * `Guard`: a compare (`emit_fused_int_compare` or `TEST EAX, EAX`) and a
///   `Jcc` to a pad that never returns into the body (a deopt, an implicit
///   throw, which is jumped over, or a forced chain trip, whose counter is
///   `PUSHFQ; PUSH RAX ... POP RAX; POPFQ`).
/// * `Phi`: nothing, or the generic re-publish into its own register, which is
///   never a caller-saved one (a phi is not a block-local value).
/// * `Param`: RAX, its home and its own register (a parameter is never given a
///   caller-saved one); a reference parameter's prologue word is zeroed with a
///   plain `MOV qword [rbp - off], 0`.
/// * `Start` / `Proj` / `Merge` / `Region` / `Dead` / `KeepAlive` /
///   `OsrMemory`: nothing at all.
///
/// The helpers these reach (`emit_add_lea*`, `emit_alu_acc_*`, `emit_imul_*`,
/// `emit_shift_imm`, the RCX twins, `emit_fused_int_compare`, `gp_load_value`,
/// `publish_def_at_store`, the generic publish) write RAX, RCX or the result's
/// own register; the int poisoning stress mode's `PUSH R11 ... POP R11`
/// restores R11. Between two nodes `lower_block` emits only frame-slot zero
/// stores, a `LOCK ADD [RSP]` fence and the split plan's transitions (callee-
/// saved registers only).
///
/// Wave 2 (round 25) adds, audited the same way:
///
/// * integer `Div` / `Rem`: RAX, RCX (the divisor, or `emit_div_by_const`'s
///   copy of the dividend), RDX (`CQO` / `IDIV`, the magic constant), a
///   zero-divisor `Jcc` to a deopt pad (`emit_deopt_unless`) or the inline /
///   cold placeholder (`XOR EAX, EAX`, jumping back to the post-`IDIV`
///   offset), and the `MIN / -1` fixup (`NEG` / `XOR` of EAX). The FP `Rem`
///   is a `CALL` and stays out.
/// * FP `Add` / `Sub` / `Mul` / `Div` (`fp_load_value` into XMM0/XMM1,
///   `fp_binop`, `fp_store_value`), FP `Neg` (RAX), `ConstF` (RAX), the
///   conversions `I2F` / `I2D` / `L2F` / `L2D` / `F2D` / `D2F` (RAX, XMM0) and
///   `F2I` / `F2L` / `D2I` / `D2L` (`CVTT*` into RAX plus
///   `emit_fp_to_int_fixup`: RAX, XMM0, XMM1, short jumps), `FCmp` (RAX, RDX,
///   XMM0/XMM1).
///
/// Left out, as unaudited rather than known bad: `ScalarIntrinsic` (RDX and
/// helper calls). Out for cause: everything that loads, stores, allocates,
/// calls, polls or takes a field / epoch guard (R10/R11 scratch, argument
/// registers, and calls that destroy all four).
pub(in crate::ir_lower) fn ir_op_caller_saved_transparent(node: &Node) -> bool {
    match node.op {
        Op::Const(_) | Op::Phi | Op::Guard { .. } | Op::Cmp(_) | Op::LCmp | Op::Param(_) => true,
        Op::Rem => matches!(node.ty, IrType::Int | IrType::Long),
        Op::Add | Op::Sub | Op::Mul | Op::Neg | Op::Div => matches!(
            node.ty,
            IrType::Int | IrType::Long | IrType::Float | IrType::Double
        ),
        Op::ConstF(_)
        | Op::FCmp { .. }
        | Op::I2F
        | Op::I2D
        | Op::L2F
        | Op::L2D
        | Op::F2I
        | Op::F2L
        | Op::D2I
        | Op::D2L
        | Op::F2D
        | Op::D2F => true,
        Op::Start
        | Op::Proj(_)
        | Op::Merge
        | Op::Region
        | Op::Dead
        | Op::KeepAlive
        | Op::OsrMemory => true,
        Op::And | Op::Or | Op::Xor | Op::Shl | Op::Shr | Op::UShr | Op::I2L | Op::L2I => {
            matches!(node.ty, IrType::Int | IrType::Long)
        }
        _ => false,
    }
}

/// Per linear position: may a caller-saved register hold a value THROUGH this
/// position? `true` for a node position whose every scheduled node is
/// [`ir_op_caller_saved_transparent`], and for the terminator position of an
/// `Op::If` on a block with no back edge (the arm reads its condition first and
/// no poll precedes it; `Lowerer::caller_saved_survive_terminator` is the same
/// test). `false` for everything else, every block's edge position included,
/// so a run of `true` positions never leaves its block.
///
/// Positions are numbered the way `lower_block` emits them (`ls_node_positions`
/// and `ls_block_start`): block `b`'s nodes at `span[b].0 + i`, its terminator
/// right after, then the edge. `plan_register_residency` refuses the method
/// before this runs unless that numbering's total agrees with the model's.
pub(in crate::ir_lower) fn caller_saved_transparent_positions(
    graph: &Graph,
    schedule: &Schedule,
    live: &crate::regalloc::LiveModel,
) -> Vec<bool> {
    let mut ok = vec![false; live.total_positions];
    for (b, block) in schedule.blocks.iter().enumerate() {
        let Some(&(start, _)) = live.span.get(b) else {
            continue;
        };
        for (offset, &id) in block.nodes.iter().enumerate() {
            let transparent = graph
                .nodes
                .get(id as usize)
                .is_some_and(ir_op_caller_saved_transparent);
            if let Some(cell) = ok.get_mut(start + offset) {
                *cell = transparent;
            }
        }
        if let Some(term) = block.terminator {
            let is_if = matches!(graph.nodes.get(term as usize).map(|n| &n.op), Some(Op::If));
            let back_edge = block.successors.iter().any(|&s| s <= b);
            if let Some(cell) = ok.get_mut(start + block.nodes.len()) {
                *cell = is_if && !back_edge;
            }
        }
    }
    ok
}

/// What the caller-saved planner decided. Built by the pre-scan half
/// ([`plan_caller_saved_residency`]), extended by the post-scan half
/// ([`plan_caller_saved_splits`]) and handed to the `Lowerer` through
/// `RegResidency::caller_saved_republish`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::ir_lower) struct CallerSavedPlan {
    /// The planner ran (the switch is on and the position model matches
    /// `lower_block`'s numbering).
    pub(in crate::ir_lower) active: bool,
    /// `(value, register)` for the WHOLE-RANGE values, in assignment order.
    /// These are pinned out of the scan.
    pub(in crate::ir_lower) assigned: Vec<(usize, u8)>,
    /// Whole-range values that met every condition; `assigned` plus the ones
    /// the file had no room for.
    pub(in crate::ir_lower) candidates: usize,
    /// `(after, value)`: once node `after` is lowered, reload `value` from its
    /// home into its caller-saved register
    /// (`Lowerer::republish_caller_saved_after`). `after == value` for an
    /// opaque definition; otherwise the opaque node a split run follows.
    pub(in crate::ir_lower) republish: Vec<(NodeId, NodeId)>,
    /// Per register of [`IR_LS_CALLER_SAVED_GPRS`]: the position intervals
    /// handed out so far, endpoint-inclusive. Sized at the first query.
    pub(in crate::ir_lower) held: Vec<Vec<(usize, usize)>>,
    /// Post-scan: values given a register for some runs, how many runs, and
    /// how many values had a qualifying run but found no register.
    pub(in crate::ir_lower) split_values: usize,
    pub(in crate::ir_lower) split_runs: usize,
    pub(in crate::ir_lower) split_no_room: usize,
}

impl CallerSavedPlan {
    /// The first register free over EVERY interval of `want`
    /// (endpoint-inclusive), if any.
    pub(in crate::ir_lower) fn free_register_for(
        &mut self,
        want: &[(usize, usize)],
    ) -> Option<usize> {
        if self.held.len() != IR_LS_CALLER_SAVED_GPRS.len() {
            self.held = vec![Vec::new(); IR_LS_CALLER_SAVED_GPRS.len()];
        }
        self.held.iter().position(|ranges| {
            want.iter()
                .all(|&(lo, hi)| ranges.iter().all(|&(l, h)| hi < l || h < lo))
        })
    }

    /// Hand register `k` to `want`'s intervals.
    pub(in crate::ir_lower) fn hold(&mut self, k: usize, want: &[(usize, usize)]) {
        if let Some(ranges) = self.held.get_mut(k) {
            ranges.extend_from_slice(want);
        }
    }
}

/// The per-position facts both caller-saved passes read.
pub(in crate::ir_lower) struct CallerSavedPositions {
    /// [`caller_saved_transparent_positions`].
    pub(in crate::ir_lower) transparent: Vec<bool>,
    /// The data node at a node position (`span[b].0 + i`); `None` at
    /// terminator and edge positions.
    pub(in crate::ir_lower) node_at: Vec<Option<NodeId>>,
    /// The latest NON-transparent position at or before each position.
    pub(in crate::ir_lower) prev_opaque: Vec<Option<usize>>,
}

pub(in crate::ir_lower) fn caller_saved_positions(
    graph: &Graph,
    schedule: &Schedule,
    live: &crate::regalloc::LiveModel,
) -> CallerSavedPositions {
    let transparent = caller_saved_transparent_positions(graph, schedule, live);
    let mut node_at: Vec<Option<NodeId>> = vec![None; transparent.len()];
    for (b, block) in schedule.blocks.iter().enumerate() {
        let Some(&(start, _)) = live.span.get(b) else {
            continue;
        };
        for (offset, &id) in block.nodes.iter().enumerate() {
            if let Some(cell) = node_at.get_mut(start + offset) {
                *cell = Some(id);
            }
        }
    }
    let mut prev_opaque: Vec<Option<usize>> = vec![None; transparent.len()];
    let mut last: Option<usize> = None;
    for (p, &t) in transparent.iter().enumerate() {
        if !t {
            last = Some(p);
        }
        prev_opaque[p] = last;
    }
    CallerSavedPositions {
        transparent,
        node_at,
        prev_opaque,
    }
}

/// Static data-input counts per node (every edge, as the residency pass's own
/// `use_count`).
pub(in crate::ir_lower) fn caller_saved_use_counts(graph: &Graph) -> Vec<u32> {
    let mut use_count = vec![0u32; graph.nodes.len()];
    for node in &graph.nodes {
        for &input in &node.inputs {
            if input != NO_NODE {
                if let Some(c) = use_count.get_mut(input as usize) {
                    *c = c.saturating_add(1);
                }
            }
        }
    }
    use_count
}

/// `(def, lo, hi)` for a value either caller-saved pass may consider, or
/// `None`: an `int` / `long` value -- not a constant (an immediate to every
/// reader), a phi (published on edges) or a parameter (the prologue fills
/// parameter registers from the ABI registers, R8/R9 among them) -- read at
/// least twice, with a home word, not a scalar-replacement field value, placed
/// by the schedule. `lo..=hi` is the union of `plan_slots`' and the
/// allocator's ranges and the definition's own position.
pub(in crate::ir_lower) fn caller_saved_value_span(
    graph: &Graph,
    schedule: &Schedule,
    plan: &SlotPlan,
    live: &crate::regalloc::LiveModel,
    use_count: &[u32],
    id: usize,
) -> Option<(usize, usize, usize)> {
    let node = graph.nodes.get(id)?;
    if !matches!(node.ty, IrType::Int | IrType::Long)
        || matches!(node.op, Op::Const(_) | Op::Phi | Op::Param(_))
    {
        return None;
    }
    let uses = use_count.get(id).copied().unwrap_or(0);
    if (uses < 2 && !r25_w7a_reg_alloc::direct_single_use_candidate(graph, schedule, id, uses))
        || !live.wants_loc.get(id).copied().unwrap_or(false)
        || plan.node_color.get(id).copied().flatten().is_none()
        || plan.sr_field_values.contains(&(id as NodeId))
    {
        return None;
    }
    let def = live.pos_of.get(id).copied().flatten()?;
    let a = plan.range.get(id).copied().flatten()?;
    let b = live.range.get(id).copied().flatten()?;
    let lo = a.lo.min(b.lo).min(def);
    let hi = a.hi.max(b.hi);
    (hi > lo).then_some((def, lo, hi))
}

/// The caller-saved half of the GP residency plan, before the scan (JIT round
/// 25, item 1b, [`ir_ls_caller_saved_enabled`]): the WHOLE-RANGE values.
///
/// # Which values
///
/// Those [`caller_saved_value_span`] admits, not pinned, every position of
/// whose live range except the definition's own is transparent
/// ([`caller_saved_transparent_positions`]). So the range lies inside one
/// block, every node it spans after the definition writes none of R8-R11, and
/// its last use is a transparent node or the block's poll-free `If`.
///
/// The definition itself may be OPAQUE (wave 2): a field or array read, a
/// call, a static read. Its arm may publish the register and then clobber it,
/// so the value is also listed in `republish` under its own id, and
/// `Lowerer::republish_caller_saved_after` reloads it from the home once the
/// whole node is emitted -- after its last clobber on every route that returns
/// into the body.
///
/// # Which register
///
/// Greedy interval colouring, heaviest first (`LiveModel::weight`, the
/// loop-weighted use count, then node id): the first register whose intervals
/// do not overlap this one's range, endpoint-inclusive as everywhere in this
/// file. A candidate that finds none is left to the scan exactly as before.
///
/// # How it composes with the scan
///
/// The caller pins every ASSIGNED value out of `allocate_linear_scan`, so the
/// callee-saved file is spent only on values that need it, and installs the
/// assignment after every other pass. Nothing else hands out R8-R11, so no
/// second-opinion pass is needed.
///
/// # What the emission owes it
///
/// `Lowerer::invalidate_caller_saved_residency` at every block boundary and
/// before every non-transparent node (a fused compare reads its operands at the
/// branch, possibly after a call: it then reloads the home word), and
/// `Lowerer::install_caller_saved_residency`, which keeps those home words
/// written. Soundness rests on those two and on the transparency audit alone:
/// a planning mistake here costs reads (an eviction, a home read), never a
/// wrong value.
pub(in crate::ir_lower) fn plan_caller_saved_residency(
    graph: &Graph,
    schedule: &Schedule,
    plan: &SlotPlan,
    live: &crate::regalloc::LiveModel,
) -> CallerSavedPlan {
    let n = graph.nodes.len();
    let pos = caller_saved_positions(graph, schedule, live);
    let use_count = caller_saved_use_counts(graph);
    // `(weight, id, lo, hi, opaque definition)` for every value that qualifies.
    let mut cands: Vec<(u64, usize, usize, usize, bool)> = Vec::new();
    for id in 0..n {
        if live.pinned.get(id).copied().unwrap_or(true) {
            continue;
        }
        let Some((def, lo, hi)) =
            caller_saved_value_span(graph, schedule, plan, live, &use_count, id)
        else {
            continue;
        };
        if hi >= pos.transparent.len() {
            continue;
        }
        if !(lo..=hi).all(|p| p == def || pos.transparent[p]) {
            continue;
        }
        cands.push((
            live.weight.get(id).copied().unwrap_or(0),
            id,
            lo,
            hi,
            !pos.transparent[def],
        ));
    }
    cands.sort_unstable_by(|x, y| y.0.cmp(&x.0).then(x.1.cmp(&y.1)));
    let mut out = CallerSavedPlan {
        active: true,
        candidates: cands.len(),
        ..CallerSavedPlan::default()
    };
    for &(_, id, lo, hi, opaque_def) in &cands {
        if let Some(k) = out.free_register_for(&[(lo, hi)]) {
            out.hold(k, &[(lo, hi)]);
            out.assigned.push((id, IR_LS_CALLER_SAVED_GPRS[k]));
            if opaque_def {
                out.republish.push((id as NodeId, id as NodeId));
            }
        }
    }
    out
}

/// `CRATONVM_JIT_IR_LS_CALLER_SAVED_SPLITS` (round 25 wave 2, item 1b's
/// "split at calls"; requires `CRATONVM_JIT_IR_LS_CALLER_SAVED`): a value that
/// got no GP register at all keeps a caller-saved one for the RUNS of its
/// range that read it at least twice between two opaque nodes, reloaded from
/// the home after the opaque node that opens each run.
/// Default ON since j27 2026-10-28, `=0` kills; read per compile.
pub(in crate::ir_lower) fn ir_ls_caller_saved_splits_enabled() -> bool {
    // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
    true
}

/// The caller-saved half of the plan AFTER the scan and every other pass
/// (round 25 wave 2, [`ir_ls_caller_saved_splits_enabled`]): split runs.
///
/// For a value [`caller_saved_value_span`] admits that ended up in NO
/// register (the scan split or refused it, no other pass took it), each read
/// at a transparent position is attributed to its ANCHOR -- the latest opaque
/// node before it in the value's range, or the definition when there is none.
/// Reads with one anchor form a run: between the anchor and the run's last
/// read nothing writes R8-R11. A run of two or more reads (counted per input
/// edge) is worth one home load after its anchor; a run of one is not, and a
/// read at an opaque node (a call argument, a field base) goes to the home in
/// any case, since the register is invalidated before such a node. A read whose
/// latest opaque position is a terminator or an edge (a run that would start at
/// a block top) has no node to reload after and is left out.
///
/// A value gets ONE register for all its kept runs (`gp_reg_of` is static);
/// the intervals it holds are each run's `[anchor, last read]` plus its
/// definition point, where every definition arm publishes into its assigned
/// register. Heaviest first, as the whole-range pass, which ran before the
/// scan and has first call on the registers.
///
/// Returns the number of values given a register.
#[allow(clippy::too_many_arguments)]
pub(in crate::ir_lower) fn plan_caller_saved_splits(
    graph: &Graph,
    schedule: &Schedule,
    plan: &SlotPlan,
    live: &crate::regalloc::LiveModel,
    gp_reg_of: &mut [Option<u8>],
    reg_of: &[Option<u8>],
    split_managed: &[bool],
    cs: &mut CallerSavedPlan,
) -> usize {
    let n = graph.nodes.len();
    let pos = caller_saved_positions(graph, schedule, live);
    let use_count = caller_saved_use_counts(graph);
    // `(weight, id, intervals, anchors to reload after, runs)`.
    let mut wants: Vec<(u64, usize, Vec<(usize, usize)>, Vec<NodeId>, usize)> = Vec::new();
    for id in 0..n {
        if gp_reg_of.get(id).copied().flatten().is_some()
            || reg_of.get(id).copied().flatten().is_some()
            || split_managed.get(id).copied().unwrap_or(true)
        {
            continue;
        }
        let Some((def, _lo, hi)) =
            caller_saved_value_span(graph, schedule, plan, live, &use_count, id)
        else {
            continue;
        };
        if hi >= pos.transparent.len() {
            continue;
        }
        let Some(uses) = live.uses.get(id) else {
            continue;
        };
        // `(anchor position, anchor node, last read position, reads)`.
        let mut runs: Vec<(usize, NodeId, usize, usize)> = Vec::new();
        for &u in uses {
            if u <= def || u > hi || !pos.transparent[u] {
                continue;
            }
            let anchor_pos = match pos.prev_opaque[u - 1] {
                Some(q) if q > def => q,
                _ => def,
            };
            let anchor = if anchor_pos == def {
                id as NodeId
            } else {
                match pos.node_at[anchor_pos] {
                    Some(y) => y,
                    None => continue,
                }
            };
            let reads = match pos.node_at[u] {
                Some(r) => graph.nodes.get(r as usize).map_or(1, |rn| {
                    rn.inputs
                        .iter()
                        .filter(|&&i| i == id as NodeId)
                        .count()
                        .max(1)
                }),
                None => 1,
            };
            match runs.last_mut() {
                Some(last) if last.0 == anchor_pos => {
                    last.2 = u;
                    last.3 += reads;
                }
                _ => runs.push((anchor_pos, anchor, u, reads)),
            }
        }
        runs.retain(|r| r.3 >= 2);
        if runs.is_empty() {
            continue;
        }
        let mut intervals: Vec<(usize, usize)> = runs.iter().map(|r| (r.0, r.2)).collect();
        if runs[0].0 != def {
            intervals.push((def, def));
        }
        let anchors: Vec<NodeId> = runs
            .iter()
            .filter(|r| r.0 != def || !pos.transparent[def])
            .map(|r| r.1)
            .collect();
        wants.push((
            live.weight.get(id).copied().unwrap_or(0),
            id,
            intervals,
            anchors,
            runs.len(),
        ));
    }
    wants.sort_unstable_by(|x, y| y.0.cmp(&x.0).then(x.1.cmp(&y.1)));
    let mut taken = 0usize;
    for (_, id, intervals, anchors, runs) in wants {
        let Some(k) = cs.free_register_for(&intervals) else {
            cs.split_no_room += 1;
            continue;
        };
        cs.hold(k, &intervals);
        if let Some(cell) = gp_reg_of.get_mut(id) {
            *cell = Some(IR_LS_CALLER_SAVED_GPRS[k]);
        }
        for a in anchors {
            cs.republish.push((a, id as NodeId));
        }
        cs.split_values += 1;
        cs.split_runs += runs;
        taken += 1;
    }
    taken
}

/// The `disp32` a RIP-relative operand needs to reach `target` from an
/// instruction that STARTS at `here` and encodes to `len` bytes, or `None` when
/// the target is outside ±2GB.
///
/// x86-64 measures a RIP-relative displacement from the end of the WHOLE
/// instruction — past every immediate — so `len` is the full encoded length,
/// not the offset of the displacement field. Getting that wrong does not
/// fault and does not fail a smoke test: the operand silently names a word `k`
/// bytes from the intended one, and for a GUARD that means comparing an
/// unrelated global against a baked epoch. Hence one function, two callers and
/// `a_rip_displacement_resolves_to_its_target`.
pub(in crate::ir_lower) fn rip_disp32(here: usize, len: usize, target: usize) -> Option<i32> {
    let next_pc = here.wrapping_add(len);
    // Widening: usize -> i128 (no truncation, for the range check).
    let delta: i128 = (target as i128) - (next_pc as i128);
    if delta < i32::MIN as i128 || delta > i32::MAX as i128 {
        return None;
    }
    Some(delta as i32) // Cast: range-checked immediately above.
}

/// zs s3/bt: the compare half of the fused reference-store gate
/// (`Lowerer::emit_zgc_fused_ref_store_gate`) for an instruction starting at
/// `here`: `CMP word [rip+disp32], 0` (8 bytes) when `word` is in RIP reach,
/// else `MOV R11, imm64; CMP word [R11], 0` (15 bytes). ZF=1 exactly when
/// both gate bytes read zero.
pub(in crate::ir_lower) fn zgc_fused_gate_cmp_bytes(here: usize, word: usize) -> Vec<u8> {
    // 66 83 3D <disp32> 00
    const RIP_LEN: usize = 8;
    if let Some(disp) = rip_disp32(here, RIP_LEN, word) {
        let mut b = vec![0x66, 0x83, 0x3D];
        b.extend_from_slice(&disp.to_le_bytes());
        b.push(0x00);
        return b;
    }
    let mut b = vec![0x49, 0xBB]; // MOV R11, imm64
    b.extend_from_slice(&(word as u64).to_le_bytes()); // Cast: an address
    b.extend_from_slice(&[0x66, 0x41, 0x83, 0x3B, 0x00]); // CMP word [R11], 0
    b
}

/// gcj j3/rt3 -- `CRATONVM_JIT_IR_SATB_GATE_ONCE` (default OFF, opt-in; `=1`
/// arms it; proposal
/// `gcj-j2jt2-proposal-generational-satb-gate-in-one-instruction-once-per-store`):
/// the optimizing tier's gated reference `putfield` / `aastore` on the
/// GENERATIONAL mask plan tests its SATB pre gate once, at the top
/// ([`Lowerer::emit_ir_satb_pre_gate`]), in one RIP-relative instruction when
/// the gate byte is in reach; the inline card arm's second test "right before
/// the store" is not emitted.
///
/// # Why the second test owes nothing
///
/// * Nothing between the first test and the store is a GC point: every unsure
///   edge of the card arm jumps to the helper BEFORE the store (the helper
///   runs its own SATB), and the card PRE check, the clean-card inline mark
///   and the shape tests call nothing. So a thread that ARRIVES at the
///   initial-mark pause cannot be between the two tests
///   (`gcj3rt3_the_satb_gate_once_store_window_calls_nothing` pins the bytes).
/// * A thread the BUG-03 take-over FREEZES between them is covered by the
///   generational initial mark's frozen-peer scan
///   (`ConcurrentMarker::initial_mark_with_frozen`): the receiver is in RAX,
///   an exact base, for the whole window, so the pause greys the field's
///   snapshot value before the frozen store can overwrite it. That argument
///   does not depend on where in the window the peer froze, so it covers the
///   stretch the second test guarded and the stretch after it, which the
///   second test never covered.
///
/// Refused under `CRATONVM_GEN_CONC_NO_FROZEN_EAGER_SCAN=1` (the bisection
/// lever that turns that scan off), so the lever keeps its meaning. G1 and
/// ZGC never reach it ([`Lowerer::ir_satb_gate_once_here`]). Read once per
/// store site at compile time.
pub(crate) fn ir_satb_gate_once_enabled() -> bool {
    false && !cratonvm_types::flags().gc.gen_conc_no_frozen_eager_scan
}

/// Does a fresh receiver's reference `putfield` skip the SATB gate? Not when
/// the store is a colored one (`emit_gated_ir_ref_putfield`'s comment).
pub(crate) fn ir_fresh_store_skips_gate(fresh_receiver: bool, colored_store: bool) -> bool {
    fresh_receiver && !colored_store
}

/// zs s5/bt2: `CMP BYTE [rip+disp32], 0` (`80 3D d32 00`, 7 bytes) for an
/// instruction starting at `here` -- the ZGC putstatic reference store's
/// SATB pre-gate test in one instruction -- or `None` when `gate` is out of
/// ±2GB reach (the caller keeps `MOV R11, imm64; CMP BYTE [R11], 0`).
pub(in crate::ir_lower) fn zgc_pre_byte_cmp_rip_bytes(here: usize, gate: usize) -> Option<Vec<u8>> {
    let disp = rip_disp32(here, 7, gate)?;
    let mut b = vec![0x80, 0x3D];
    b.extend_from_slice(&disp.to_le_bytes());
    b.push(0x00);
    Some(b)
}

/// gcj j2/jt2 — `CRATONVM_JIT_IR_FRESH_STORE_NO_BARRIER` (WARNING gcj-j2-residuals-flip-20261025:
/// default ON since 2026-10-25, `=0` kills; F rows SAME and NEUTRAL; `gcj-j1jt-proposal-stores-into-a-fresh-allocation-skip-the-post-barrier`,
/// the slice that needs no collector change): on the GENERATIONAL mask plan
/// (a published post-skip mask and a generational card view), the gated
/// reference `putfield` into a FRESH receiver ([`zgc_fresh_receiver_stores`]'s
/// window rule: an `Op::New` of the same block, nothing but pure arithmetic
/// and first stores into that object in between) emits no SATB pre gate.
///
/// # Why the pre gate owes nothing
///
/// [`zgc_fresh_receiver_stores`]' SATB argument, which does not depend on the
/// collector: the field's old value is the allocation's null (conditions 3 and
/// 4 of the window rule: no store in the window wrote it, and the object is
/// unpublished), so the pre barrier would log nothing whatever the mark state,
/// including a mark that opened during the allocation's slow path or while a
/// BUG-03 take-over held this thread inside the window. (The generational
/// initial mark's frozen-peer scan, `ConcurrentMarker::initial_mark_with_frozen`,
/// covers gated stores whose old value is NOT null; this one has none.)
///
/// # What is kept, and why the whole of HotSpot's `ReduceInitialCardMarks` is not
///
/// The young-receiver test stays: it is register-only here (the flags byte is
/// in CL for the store's shape test anyway), and it is what keeps a fresh
/// object the allocation returned OLD -- the young-full spill into the old
/// generation (`GenerationalHeap::try_alloc_object_old`), or a promotion by a
/// collection inside the allocation's slow path -- on the barriered path.
/// Dropping it as well needs a deferred card mark on every old-returning JIT
/// allocation AND a take-over completion that covers a fresh object held only
/// in a stack slot (the post-only completion reads registers); see the
/// proposal page's STATUS. Read once per compile.
pub(crate) fn gen_fresh_store_no_pre_gate_for(
    helpers: &cratonvm_jit_api::JitRuntimeHelpers,
) -> bool {
    crate::x64::ref_store_post_skip_mask_of(helpers).is_some()
        && crate::x64::gen_card_view_of(helpers).is_some()
}

/// zs s5/bt2 (P1 of `zgc-zs3bt-proposal-fresh-receiver-elision-and-loop-held-store-gate-20261014.md`):
/// the reference `Op::Store`s whose receiver is FRESH, i.e. whose barrier is
/// provably a no-op, so the ZGC gated `putfield` may store with no gate at
/// all (`CRATONVM_JIT_ZGC_FRESH_RECEIVER_STORE`, opt-in).
///
/// A store qualifies when, in ONE block's schedule order:
///
/// 1. its receiver is an `Op::New` scheduled earlier in the same block and not
///    scalar-replaced (`virtual_news`);
/// 2. everything scheduled between the `New` and the store is either pure
///    arithmetic / a constant (no call, no stub, no memory, no safepoint), or
///    an `Op::Store` INTO THAT SAME OBJECT whose value is not the object
///    itself (anything else closes every open window: a call, a load, a
///    store into another object, a poll, an array allocation). Another
///    non-virtual `Op::New` does NOT close a window: it opens its own beside
///    it (j27 wave 5, lane BARHUNT: this list used to say "an allocation",
///    which the scan has never done). That is sound for the reasons below --
///    the old field is still null whatever the nested allocation's slow path
///    collected, and a collection there that promotes the receiver cards it
///    (`age_survivor`; the generational arm keeps its young-receiver test);
/// 3. the object is not the VALUE of any store in that window (it has not
///    escaped);
/// 4. no earlier store in the window wrote the same field (constant offset).
///
/// # Why the barrier owes nothing (SATB first)
///
/// * **SATB pre barrier.** It logs the OLD value. A fresh object's fields are
///   zero (null) when its allocation returns, conditions 3 and 4 say neither
///   this body nor another thread (the object is unpublished) wrote this
///   field since, so the old value IS null and the pre barrier would log
///   nothing. That holds whatever the mark state is, including a mark that
///   opened inside the allocation's own slow path after the object existed
///   (its snapshot then holds the object with all-null fields). A store value
///   can materialize a scalar-replaced object and allocate inside the window;
///   the proof requires the null old field, not a constant mark state.
/// * **Post barrier (generational ZGC only; the floor shape).** An allocation
///   while materializing a store value may collect and promote the receiver.
///   Survivor ageing cards the whole promoted object (`age_survivor` ->
///   `card_object`); that card outlives this store. Without promotion the
///   receiver remains age 0 and the age-floor gate would skip the barrier.
///
/// The caller applies the result only where the fused gate applies (ZGC's
/// age-floor plan); G1 never sees it. (gcj j2/jt2: the generational mask plan
/// applies it too, under its own opt-in switch and for the SATB pre gate only:
/// [`gen_fresh_store_no_pre_gate_for`].)
pub(crate) fn zgc_fresh_receiver_stores(
    graph: &Graph,
    schedule: &Schedule,
    is_virtual: impl Fn(NodeId) -> bool,
) -> std::collections::HashSet<NodeId> {
    zgc_fresh_receiver_scan(graph, schedule, is_virtual, |_| {})
}

/// zt t9/ca (Q1 of `zgc-zs5bt2-proposal-store-gate-next-steps-20261015.md`):
/// [`zgc_fresh_receiver_stores`] for a compile under `CRATONVM_DBG_JITC`,
/// which also prints one `[cratonvm-jitc] zgc fresh-receiver window closed by
/// <Op> @pc=N` line per node that closed an open window -- the census that
/// names which op to admit next if the elision turns out too narrow. The set
/// is the plain function's, line or no line.
pub(crate) fn zgc_fresh_receiver_stores_logged(
    graph: &Graph,
    schedule: &Schedule,
    is_virtual: impl Fn(NodeId) -> bool,
) -> std::collections::HashSet<NodeId> {
    if !crate::x64::dbg_jitc_enabled() {
        return zgc_fresh_receiver_stores(graph, schedule, is_virtual);
    }
    zgc_fresh_receiver_scan(graph, schedule, is_virtual, |id| {
        let node = graph.nodes.get(id as usize);
        let op = node.map_or_else(|| "<missing>".to_string(), |n| format!("{:?}", n.op));
        // The variant name alone: `Debug` of a struct variant carries its fields.
        let name = op
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '<' || c == '>'))
            .next()
            .unwrap_or("");
        let pc = node.and_then(|n| n.bytecode_pc);
        eprintln!("[cratonvm-jitc] zgc fresh-receiver window closed by {name} @pc={pc:?} n{id}");
    })
}

/// Pure graph-only exact class bound of `value` when `value` is `new C`, loaded
/// from `new C[n]`, or a known exact-class constant root ref (`IndyRootRef` /
/// `LoadStatic` in `const_ref_meta`). Used by `aastore` covariance discharge
/// and fresh-receiver `aastore` admission.
pub(in crate::ir_lower) fn aastore_value_class_bound_in(
    graph: &Graph,
    value: NodeId,
) -> Option<u32> {
    let v = graph.nodes.get(value as usize)?;
    match &v.op {
        Op::New { class_id, .. } => Some(*class_id),
        Op::ArrayLoad(MemKind::Ref) => {
            let array = *v.inputs.get(2)?;
            match &graph.nodes.get(array as usize)?.op {
                Op::NewArray {
                    element_type: 0,
                    component_class_id,
                } => Some(*component_class_id),
                _ => None,
            }
        }
        Op::IndyRootRef { slot_addr } => graph
            .const_ref_meta
            .get(slot_addr)
            .and_then(|m| m.exact_class_id),
        Op::LoadStatic {
            class_id,
            field_index,
            ..
        } => graph.const_ref_meta.values().find_map(|m| {
            (m.static_field == Some((*class_id, *field_index)))
                .then_some(m.exact_class_id)
                .flatten()
        }),
        _ => None,
    }
}

fn is_exact_object_array_in(
    graph: &Graph,
    array: NodeId,
    obj_arr_cid: Option<u32>,
    obj_cid: Option<u32>,
    depth: u8,
    seen: &mut Vec<NodeId>,
    found_witness: &mut bool,
) -> bool {
    if seen.contains(&array) {
        return true;
    }
    let Some(node) = graph.nodes.get(array as usize) else {
        return false;
    };
    match &node.op {
        Op::NewArray {
            element_type: 0,
            component_class_id,
        } if *component_class_id == 0 || obj_cid.is_some_and(|cid| *component_class_id == cid) => {
            *found_witness = true;
            true
        }
        Op::IndyRootRef { slot_addr } => {
            let hit = obj_arr_cid.is_some_and(|cid| {
                graph
                    .const_ref_meta
                    .get(slot_addr)
                    .and_then(|m| m.exact_class_id)
                    == Some(cid)
            });
            *found_witness |= hit;
            hit
        }
        Op::LoadStatic {
            class_id,
            field_index,
            ..
        } => {
            let hit = obj_arr_cid.is_some_and(|cid| {
                graph.const_ref_meta.values().any(|m| {
                    m.static_field == Some((*class_id, *field_index))
                        && m.exact_class_id == Some(cid)
                })
            });
            *found_witness |= hit;
            hit
        }
        Op::Phi if depth < 4 => {
            seen.push(array);
            let mut has_input = false;
            for &inp in node.inputs.iter().skip(1) {
                if inp == array {
                    continue;
                }
                has_input = true;
                if !is_exact_object_array_in(
                    graph,
                    inp,
                    obj_arr_cid,
                    obj_cid,
                    depth + 1,
                    seen,
                    found_witness,
                ) {
                    return false;
                }
            }
            has_input
        }
        _ => false,
    }
}

/// Whether `aastore` into `array` of `value` is provably covariance-safe from
/// the IR graph alone (null value, same-array element, exact `Object[]`, or
/// exact component class of `new C[n]`), so no `jit_aastore_type_check` helper
/// call is needed.
pub(in crate::ir_lower) fn aastore_covariance_known_safe(
    graph: &Graph,
    array: NodeId,
    value: NodeId,
) -> bool {
    let value_is_null = graph
        .nodes
        .get(value as usize)
        .is_some_and(|n| matches!(n.op, Op::Const(0)));
    if value_is_null {
        return true;
    }
    let value_from_same_array = graph.nodes.get(value as usize).is_some_and(|v| {
        matches!(v.op, Op::ArrayLoad(MemKind::Ref)) && v.inputs.get(2).copied() == Some(array)
    });
    if value_from_same_array {
        return true;
    }
    let (obj_arr_cid, obj_cid) = match crate::ir_collection_layouts_current() {
        Some(l) => (
            (l.object_array_class_id != 0).then_some(l.object_array_class_id),
            (l.object_class_id != 0).then_some(l.object_class_id),
        ),
        None => (None, None),
    };
    let mut seen = Vec::new();
    let mut found_witness = false;
    if is_exact_object_array_in(
        graph,
        array,
        obj_arr_cid,
        obj_cid,
        0,
        &mut seen,
        &mut found_witness,
    ) && found_witness
    {
        return true;
    }
    match graph.nodes.get(array as usize).map(|n| &n.op) {
        Some(Op::NewArray {
            element_type: 0,
            component_class_id,
        }) => aastore_value_class_bound_in(graph, value)
            .is_some_and(|bound| bound == *component_class_id),
        _ => false,
    }
}

/// The scan behind [`zgc_fresh_receiver_stores`]: the same set, and
/// `on_close(id)` for every node that closed at least one open window (a
/// missing node, a store into another object, a store with a non-constant
/// offset/index, a store of an open object as a value, a type-checked
/// `aastore`, or any op that is neither pure nor a window-preserving store).
pub(in crate::ir_lower) fn zgc_fresh_receiver_scan(
    graph: &Graph,
    schedule: &Schedule,
    is_virtual: impl Fn(NodeId) -> bool,
    mut on_close: impl FnMut(NodeId),
) -> std::collections::HashSet<NodeId> {
    use std::collections::HashSet;
    let pure = |op: &Op| {
        matches!(
            op,
            Op::Const(_)
                | Op::ConstF(_)
                | Op::Param(_)
                | Op::Add
                | Op::Sub
                | Op::Mul
                | Op::Neg
                | Op::And
                | Op::Or
                | Op::Xor
                | Op::Shl
                | Op::Shr
                | Op::UShr
                | Op::I2L
                | Op::L2I
                | Op::I2F
                | Op::I2D
                | Op::L2F
                | Op::L2D
                | Op::F2I
                | Op::F2L
                | Op::F2D
                | Op::D2I
                | Op::D2L
                | Op::D2F
                | Op::Cmp(_)
                | Op::LCmp
                | Op::FCmp { .. }
                | Op::ScalarIntrinsic(_)
        )
    };
    let mut fresh: HashSet<NodeId> = HashSet::new();
    for block in &schedule.blocks {
        // Open windows: the `New` / `NewArray` and the field/element indices stored so far.
        let mut open: HashMap<NodeId, HashSet<i64>> = HashMap::new();
        for &id in &block.nodes {
            let Some(n) = graph.nodes.get(id as usize) else {
                if !open.is_empty() {
                    on_close(id);
                }
                open.clear();
                continue;
            };
            let mut record_store =
                |base: NodeId, key_node: NodeId, value: NodeId, is_ref: bool, needs_call: bool| {
                    let escaped = open.remove(&value).is_some();
                    if !open.contains_key(&base) || needs_call {
                        if escaped || !open.is_empty() {
                            on_close(id);
                        }
                        open.clear();
                        return;
                    }
                    let Some(Op::Const(slot_key)) =
                        graph.nodes.get(key_node as usize).map(|m| &m.op)
                    else {
                        on_close(id);
                        open.remove(&base);
                        return;
                    };
                    if escaped {
                        on_close(id);
                    }
                    let Some(written) = open.get_mut(&base) else {
                        return;
                    };
                    if is_ref && !written.contains(slot_key) {
                        fresh.insert(id);
                    }
                    written.insert(*slot_key);
                };
            match &n.op {
                Op::New { .. } if n.ty == IrType::Ref && !is_virtual(id) => {
                    open.insert(id, HashSet::new());
                }
                Op::NewArray { element_type, .. } if n.ty == IrType::Ref && !is_virtual(id) => {
                    // An array allocation closes any prior open windows (tests pin this),
                    // then opens its own window if it is a reference array.
                    if !open.is_empty() {
                        on_close(id);
                    }
                    open.clear();
                    if *element_type == 0 {
                        open.insert(id, HashSet::new());
                    }
                }
                Op::Store(kind) if n.inputs.len() >= 5 => {
                    record_store(
                        n.inputs[2],
                        n.inputs[3],
                        n.inputs[4],
                        matches!(kind, MemKind::Ref),
                        false,
                    );
                }
                Op::ArrayStore(kind) if n.inputs.len() >= 5 => {
                    let is_ref = matches!(kind, MemKind::Ref);
                    let needs_call =
                        is_ref && !aastore_covariance_known_safe(graph, n.inputs[2], n.inputs[4]);
                    record_store(n.inputs[2], n.inputs[3], n.inputs[4], is_ref, needs_call);
                }
                op if pure(op) => {}
                Op::Call { .. } | Op::Proj(_) | Op::Load(_) | Op::ArrayLoad(_) => {
                    // Non-escaping calls/reads cannot observe or mutate fields of an
                    // unpublished `Op::New`, so its prior fields remain provably null.
                    let escaped: Vec<NodeId> = n
                        .inputs
                        .iter()
                        .copied()
                        .filter(|inp| open.contains_key(inp))
                        .collect();
                    if !escaped.is_empty() {
                        on_close(id);
                        for k in escaped {
                            open.remove(&k);
                        }
                    }
                }
                _ => {
                    if !open.is_empty() {
                        on_close(id);
                    }
                    open.clear();
                }
            }
        }
    }
    fresh
}

/// `CRATONVM_JIT_IR_EPOCH_GUARD_RIP=0` — emit the layout-replacement epoch
/// guard as `MOV R11, imm64` + `CMP dword [R11], imm32` again,
/// instead of the single RIP-relative `CMP dword [rip+disp32], imm32`.
/// **Default ON.**
///
/// Semantically identical — same counter, same comparison, same aligned 32-bit
/// load — so this is an ENCODING switch, not a behaviour one. It exists
/// because a shape that is one instruction shorter still has to be shown to
/// be faster rather than assumed, and both arms have to come from one binary
/// for that to be measurable on a host with this one's drift.
///
/// The off arm is also what runs when the counter is out of ±2GB RIP reach, so
/// setting this exercises the fallback path deliberately rather than waiting
/// for an address-space layout that produces it.
pub(crate) fn ir_epoch_guard_rip_enabled() -> bool {
    // j27 MEMOSLOT: a `MemoSlot`, so a test override reaches it.
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    crate::memo_flag::memo_bool(&ON, || {
        !matches!(
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
            Ok("0") | Ok("false") | Ok("off") | Ok("no")
        )
    })
}

/// The GP registers [`Lowerer::emit_prologue`] MAY save — the widest file this
/// platform offers. What a given compile actually saves is
/// [`ir_gp_file`], which the kill switch narrows; this constant is the
/// ABI fact and that function is the policy.
///
/// All of [`IR_LOWER_LS_GPRS`]: every one is callee-saved on the target ABI,
/// which is why they are the file. Like the XMM list this IS
/// platform-conditional — RSI/RDI join it on Win64 — and like the XMM list only
/// the registers the residency plan actually used are saved, so a wider file
/// that goes unused emits no extra instruction.
pub(in crate::ir_lower) const IR_LOWER_SAVED_GPRS: &[u8] =
    crate::regalloc::xmm_roles::IR_GP_PROLOGUE_SAVED;

/// Publish a resident value into its register AT ITS DEFINITION, out of RAX,
/// instead of reloading the home word this backend just wrote -- **default
/// ON** since 2026-09-05; `CRATONVM_JIT_IR_PUBLISH_AT_DEF=0` is the kill switch.
///
/// The generic publish at the end of `lower_data_node` is a LOAD of the word
/// the arm above it stored, and its own comment has said since it landed that
/// "a per-arm register-to-register publish would be cheaper still". Doing it
/// per arm meant editing fifty call sites; doing it in `store_rax` means
/// recognising, at the one place that writes a home word from RAX, that the
/// offset being written is the current definition's own home -- at which point
/// RAX provably holds that value and the copy is free of any question about
/// where it came from.
///
/// It is also the precondition for dropping the store: with the publish reading
/// the home, a home that is never written cannot be published from.
pub(in crate::ir_lower) fn ir_publish_at_def_enabled() -> bool {
    // 2026-09-05: DEFAULT ON. `=0` is the kill switch.
    true
}

pub(in crate::ir_lower) fn ir_carry_single_use_enabled() -> bool {
    #[cfg(test)]
    {
        if let Some(forced) = CARRY_FORCE.with(|c| c.get()) {
            return forced;
        }
    }
    // OFF under a MIR mode, and not only so the byte-equality lane can compare.
    // In `MirMode::Emit` a tiled node is emitted by the SELECTOR and its arm
    // never runs, so a mix would be genuinely broken rather than merely
    // different: an arm could start a carry that its tiled consumer never
    // reads, or fold an immediate the tiler then re-materialises. Residency is
    // switched off under a MIR mode for the same class of reason.
    if isel_emit_enabled() || isel_verify_enabled() {
        return false;
    }
    // 2026-09-05: DEFAULT ON. `=0` is the kill switch.
    true
}

/// `CRATONVM_JIT_IR_CARRY_2ND=0` — take only the adjacent carry, the shape that
/// predates `ir_schedule::pair_single_use_operands`.
///
/// Default ON. This is the arm whose soundness rests on [`op_preserves_rcx`]
/// rather than on adjacency, and a wrong entry there produces a plausible wrong
/// integer rather than a fault — so it is the first flag to try when an
/// optimizing-tier result is wrong rather than absent.
pub(crate) fn ir_carry_second_operand_enabled() -> bool {
    // j27 MEMOSLOT: a `MemoSlot`, so a test override reaches it.
    static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    crate::memo_flag::memo_bool(&G, || {
        !matches!(
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
            Ok("0") | Ok("false") | Ok("off")
        )
    })
}

/// `CRATONVM_JIT_IR_CARRY_RCX_FOLDED=0` — restrict a deferred carry's crossing
/// to the ops [`op_preserves_rcx`] names, ignoring the folded-operand widening
/// [`Lowerer::node_preserves_rcx`] adds.
///
/// Default ON since 2026-09-10. `op_preserves_rcx` is two ops, and its own doc
/// gives the reason: *every* binary arm loads its second operand with
/// `gp_load_value(RCX, node.inputs[1])`. That is true of the arm, and false of
/// the arm on the path it takes when that operand is a CONSTANT — every one of
/// them opens `if !self.emit_alu_acc_imm(node.inputs[1], ..)` (or the `imul` /
/// shift form), and the folded branch never reaches the RCX read at all. The
/// three fold helpers share one predicate, `alu_imm32(id).is_some()`, so the
/// planner can ask the arm's own question rather than a weaker one.
///
/// That is a NODE-level fact, not an op-level one, which is why it cannot live
/// in `op_preserves_rcx`. `x + 1`, `x & 0xFF`, `x >>> 3` and `x * 31` are the
/// commonest producers in Java arithmetic, and before this every one of them
/// disqualified whatever was scheduled behind it.
pub(crate) fn ir_carry_rcx_folded_enabled() -> bool {
    // j27 MEMOSLOT: a `MemoSlot`, so a test override reaches it.
    static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    crate::memo_flag::memo_bool(&G, || {
        !matches!(
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
            Ok("0") | Ok("false") | Ok("off")
        )
    })
}

/// `CRATONVM_JIT_IR_ADD_LEA=0` — lower `x + k` and `x - k` through the
/// accumulator, the shape that predates 2026-09-10.
///
/// Default ON. `LEA` is the only three-operand integer instruction on x86-64,
/// and an increment whose source and destination are both register-resident is
/// exactly the case that needs one: without it the value makes a round trip
/// through RAX it has no other reason to make.
///
/// The kill switch and the A/B. See [`Lowerer::emit_add_lea`] for what it
/// replaces and for the obligations a DEFINING arm has that a fused compare
/// does not.
pub(crate) fn ir_add_lea_enabled() -> bool {
    // j27 MEMOSLOT: a `MemoSlot`, so a test override reaches it.
    static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    crate::memo_flag::memo_bool(&G, || {
        !matches!(
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
            Ok("0") | Ok("false") | Ok("off")
        )
    })
}

/// The ops whose arms reach RCX ONLY through the register form of their second
/// operand, so that folding that operand into an immediate leaves RCX
/// untouched.
///
/// Membership is a claim about arm SHAPE and nothing else: one
/// `gp_load_value(RAX, node.inputs[0])`, then a single
/// `if !self.emit_<fold>(node.inputs[1], ..) { self.gp_load_value(RCX,
/// node.inputs[1]); .. }`, then `store_rax`.
/// `every_folded_arm_reaches_rcx_only_in_its_register_form` checks that, and
/// checks the fold helpers' shared predicate too — the tie between "the
/// planner thinks this folds" and "the arm took the folded branch".
///
/// The FP-typed forms of these ops take an entirely different path
/// (`fp_load_value` into XMM0/XMM1), so `node_preserves_rcx` restricts to
/// `Int`/`Long`.
pub(in crate::ir_lower) const RCX_FREE_WHEN_FOLDED: &[&str] = &[
    "Add", "Sub", "Mul", "And", "Or", "Xor", "Shl", "Shr", "UShr",
];

/// Deferred carries, process-wide: `(candidate windows, taken, declined because
/// the arm in between can write RCX, of which that arm folds its own second
/// operand)`.
///
/// A CANDIDATE is a consumer already taking its first operand in RAX, which is
/// the only honest denominator: the shape the second slot exists for.
///
/// A per-compile census answers "did it fire on THIS method", which is the
/// question a kernel asks. Retiring
/// `c2-one-carry-slot-is-the-frame-traffic-ceiling-FIXED-20260910.md` needed
/// the other one — how often the shape occurs across a probe set — and that
/// is a total over every method a run compiles, not a line per method to be
/// summed by eye.
pub(in crate::ir_lower) static IR_CARRY_DEFERRED: [std::sync::atomic::AtomicU64; 4] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// `(candidates, taken, mid_writes_rcx, of_which_foldable)` since process start.
pub fn ir_carry_deferred_census() -> (u64, u64, u64, u64) {
    use std::sync::atomic::Ordering::Relaxed;
    (
        IR_CARRY_DEFERRED[0].load(Relaxed),
        IR_CARRY_DEFERRED[1].load(Relaxed),
        IR_CARRY_DEFERRED[2].load(Relaxed),
        IR_CARRY_DEFERRED[3].load(Relaxed),
    )
}

pub(in crate::ir_lower) fn note_deferred_census(
    candidates: usize,
    taken: usize,
    mid_rcx: usize,
    foldable: usize,
) {
    use std::sync::atomic::Ordering::Relaxed;
    IR_CARRY_DEFERRED[0].fetch_add(candidates as u64, Relaxed);
    IR_CARRY_DEFERRED[1].fetch_add(taken as u64, Relaxed);
    IR_CARRY_DEFERRED[2].fetch_add(mid_rcx as u64, Relaxed);
    IR_CARRY_DEFERRED[3].fetch_add(foldable as u64, Relaxed);
    #[cfg(test)]
    IR_CENSUS_THIS_THREAD.with(|c| {
        let mut v = c.get();
        v[0] += candidates as u64;
        v[1] += taken as u64;
        v[2] += mid_rcx as u64;
        v[3] += foldable as u64;
        c.set(v);
    });
}

/// Classes of the `ir-home-keep` census: why a definition home store was
/// emitted (`Lowerer::home_keep_class`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ir_lower) enum HomeKeep {
    Ref,
    Fp,
    Phi,
    ResidentHeld,
    ResidentReach,
    ResidentOther,
    SpilledMultiUse,
    SingleHeld,
    SingleReach,
    SingleAdjacent,
    SingleDistant,
    Other,
}

pub(in crate::ir_lower) const HOME_KEEP_CLASSES: usize = 12;

pub(in crate::ir_lower) const HOME_KEEP_NAMES: [&str; HOME_KEEP_CLASSES] = [
    "ref",
    "fp",
    "phi",
    "res_held",
    "res_reach",
    "res_other",
    "multi_use",
    "one_held",
    "one_reach",
    "one_adjacent",
    "one_distant",
    "other",
];

/// `CRATONVM_JIT_IR_BRANCH_LAYOUT_POLARITY=0` — choose a fused branch's
/// fall-through edge from the PGO hint alone, the shape that predates
/// 2026-09-11.
///
/// Default ON. See the comment at its one read site in `Op::If` for why the
/// hint alone was not enough: it is empty without `CRATONVM_TIER_PGO`, and the
/// fallback it left behind — "the `true` edge is the near one" — is inverted
/// for every javac counted loop.
///
/// Off restores the previous bytes exactly.
///
/// It is a TIE-BREAK, not an override: a branch with a profile hint keeps the
/// polarity that hint asks for, on or off. See the read site.
pub(crate) fn ir_branch_layout_polarity_enabled() -> bool {
    // j27 MEMOSLOT: a `MemoSlot`, so a test override reaches it (and replaces
    // the test-only `BRANCH_LAYOUT_POLARITY_FORCE` thread-local).
    static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    crate::memo_flag::memo_bool(&G, || {
        !matches!(
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
            Ok("0") | Ok("false") | Ok("off")
        )
    })
}

/// Fused branches whose fall-through edge the LAYOUT chose and the hint would
/// not have.
///
/// The engagement number for [`ir_branch_layout_polarity_enabled`], and the
/// only one that means anything: a flag whose census is zero on a workload
/// changed nothing there, whatever the emitted size says. Reported beside
/// `ir_fallthroughs_elided` under `CRATONVM_DBG=jitc`, because the two measure
/// the two halves of one decision — this one picks which edge should fall
/// through, that one elides the `JMP` when it does.
pub(in crate::ir_lower) static BRANCH_POLARITY_FROM_LAYOUT: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// How many fused branches took their fall-through edge from the block layout
/// rather than from the profile hint, since process start.
pub fn ir_branch_polarity_from_layout() -> u64 {
    BRANCH_POLARITY_FROM_LAYOUT.load(std::sync::atomic::Ordering::Relaxed)
}

/// `CRATONVM_JIT_IR_PHI_COPY_DIRECT=0` — stage every phi edge copy through RAX
/// and publish the phi's register from there, the shape that predates
/// 2026-09-11.
///
/// Default ON. A phi copy whose destination has a register reads straight into
/// it, which removes the `mov <phi reg>, rax` that followed every one — one
/// instruction per loop-carried value per iteration. See
/// [`Lowerer::emit_copy_op`] for why the two emissions are the same program.
///
/// Off restores the previous bytes exactly: RAX stages, the home store comes
/// from RAX, and the publish is a separate move.
///
/// j27 MEMOSLOT: a `MemoSlot`, so a test holds both arms in one process
/// through `with_thread_overrides` (the test-only `PHI_COPY_DIRECT_FORCE`
/// thread-local it replaces is gone).
pub(crate) fn ir_phi_copy_direct_enabled() -> bool {
    static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    crate::memo_flag::memo_bool(&G, || {
        !matches!(
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
            Ok("0") | Ok("false") | Ok("off")
        )
    })
}

/// `CRATONVM_JIT_IR_CMP_IN_PLACE=0` — load both operands of a fused compare into
/// RAX and RCX before comparing them, the shape that predates 2026-09-10.
///
/// Default ON. A fused compare is the one arm that can read its operands
/// wherever they already are without any further obligation: it produces no
/// value, writes no home, publishes no register and leaves only flags. When
/// both operands are register-resident the two `MOV`s ahead of it are pure
/// overhead, and on a counted loop they are two of the seventeen instructions
/// in the body.
pub(crate) fn ir_cmp_in_place_enabled() -> bool {
    // j27 MEMOSLOT: a `MemoSlot`, so a test override reaches it.
    static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    crate::memo_flag::memo_bool(&G, || {
        !matches!(
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
            Ok("0") | Ok("false") | Ok("off")
        )
    })
}

/// Fold a constant second operand into the ALU instruction instead of
/// materialising it into RCX first -- **default ON** since 2026-09-05;
/// `CRATONVM_JIT_IR_ALU_IMM=0` is the kill switch.
///
/// Every binary arithmetic arm in this file reads its second operand through
/// `gp_load_value(RCX, ..)` and then works register-to-register. When that
/// operand is a constant, `ir_const_imm` turns the read into `mov ecx, imm` --
/// so the emitted loop carries a whole instruction per constant operand that
/// x86 has an addressing form for:
///
/// ```text
/// mov ecx,1Fh  /  imul eax,ecx        becomes   imul eax,eax,1Fh
/// mov ecx,1    /  add eax,ecx         becomes   add eax,1
/// mov ecx,0FFh /  and rax,rcx         becomes   and rax,0FFh
/// ```
///
/// Unlike the rest of the work around it, this is not specific to a loop
/// shape: it is every `x + 1`, `x & 0xFF` and `x * 31` in every compiled
/// method. On `OsrTierBench.kernel` it is five of the loop's forty-four
/// instructions.
///
/// Only the SECOND operand is folded, never the first, even for the
/// commutative ops. `gp_load_value(RAX, node.inputs[0])` being unconditional
/// is what `op_reads_rax_then_rcx` and the carry's RAX contract rest on, and
/// buying a few more folds is not worth making that conditional.
pub(in crate::ir_lower) fn ir_alu_imm_enabled() -> bool {
    #[cfg(test)]
    {
        if let Some(forced) = ALU_IMM_FORCE.with(|c| c.get()) {
            return forced;
        }
    }
    // OFF under a MIR mode, and not only so the byte-equality lane can compare.
    // In `MirMode::Emit` a tiled node is emitted by the SELECTOR and its arm
    // never runs, so a mix would be genuinely broken rather than merely
    // different: the tiler would re-materialise an immediate this folded.
    // Residency is switched off under a MIR mode for the same class of reason.
    if isel_emit_enabled() || isel_verify_enabled() {
        return false;
    }
    // 2026-09-05: DEFAULT ON. `=0` is the kill switch.
    true
}

/// Let the register file be AUTHORITATIVE for a promoted `Int`/`Long`, instead
/// of a write-through read cache over the frame — **default ON** since
/// 2026-09-09; `CRATONVM_JIT_IR_REG_AUTHORITATIVE=0` restores the write-through
/// shape, so both arms stay timeable from one binary.
///
/// This is the switch on [`Lowerer::compute_deopt_named_reachable`]. Off, a
/// value keeps its home store unless it owns its register for the whole method
/// or the whole method cannot deopt. On, it also keeps it only when a deopt
/// that can actually happen names it — which is the difference between "a
/// handful of values in a trap-free kernel" and "ordinary methods".
///
/// # Why it is on
///
/// It landed off, because the prediction it rests on is the same one
/// `CRATONVM_JIT_IR_DROP_UNREACHABLE_HOMES` rests on and that had been off
/// since it landed: the net under a wrong prediction is a refused compile,
/// which is safe, but the population it newly applies to had not been soaked.
/// It has been now, and the soak is what flipped it rather than the argument:
///
/// * `jit-flag-soak.sh` over the 39 deterministic workloads in
///   `vm/tests/resources/cratonvm`, under Generational, G1 and ZGC —
///   0 divergent in all three.
/// * checksum parity against Temurin JDK 25 on 14 workloads across 7 feature
///   arms — all matching.
/// * `C2Probe loop 20000`, interleaved and order-flipped, 12 rounds: **−3.7 %
///   median, faster in 12 of 12 paired rounds**.
///
/// **It does not touch reference values or the GC**: `Ref` is refused by both
/// droppability predicates exactly as before. Reference residency is a separate
/// switch and a separate answer — see [`ir_ref_residency_enabled`], which is
/// off because it does not pay, not because it is unsafe.
pub fn ir_register_authoritative_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
            Ok("0") | Ok("false") | Ok("off") | Ok("no")
        )
    })
}

/// A direct self-recursive call (`invoke_kind == 4`) outside every protected
/// range is not a trap for frame-state purposes (round 11 wave 13, lane fib).
///
/// It is a refinement of the same "which deopts can actually happen" question
/// [`ir_register_authoritative_enabled`] turns on, so it rides that switch
/// rather than minting a new variable (a new `CRATONVM_*` name needs a
/// `types/src/flag_groups.rs` row): `CRATONVM_JIT_IR_REG_AUTHORITATIVE=0`
/// restores the old rule along with the rest of the write-through shape, and
/// `CRATONVM_JIT_IR_SELFREC_DIRECT=0` removes the route altogether. Under the
/// old rule every such call pinned the values its snapshot names (the
/// argument, the locals, the pending results) to home words.
///
/// The argument is at [`Lowerer::self_call_reads_no_frame_state`]; the net
/// under it is `build_deopt_points`' latch. Also admits the route's result to
/// the dropped-home allowlist ([`Lowerer::home_is_one_store_rax`]).
pub(in crate::ir_lower) fn ir_self_call_states_unread_enabled() -> bool {
    ir_register_authoritative_enabled()
}

/// Build a `DeoptimizationPoint` only at a bci this body can actually transfer
/// from — **default OFF**, opt in with
/// `CRATONVM_JIT_IR_DEOPT_POINTS_AT_TRAPS=1`.
///
/// `ir_drop_unreachable_homes_enabled` above does this for the ALL-or-nothing
/// case: a body that emitted no trap at all skips every point. This is the same
/// argument at per-point granularity, and it applies to the ordinary body —
/// the one with a handful of guards and forty-odd frame states describing
/// bytecode boundaries between them.
///
/// Measured with it on, against off, on the same binary: see
/// `[c2] deopt points` in the exit census.
pub fn ir_deopt_points_at_traps_enabled() -> bool {
    // t1 wave 3 (lane Y): read per call, no process `OnceLock` latch.
    matches!(
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
        Ok("1") | Ok("true") | Ok("on") | Ok("yes")
    )
}

/// Let a value lose its home word when the only things naming it are frame
/// states NO DEOPT CAN REACH -- default ON.
///
/// # The problem
///
/// `graph.safepoints` records the FULL OPERAND STACK at every bci, so an
/// intermediate is "deopt-named" from its definition until its consumer pops
/// it. Both the `deopt` cause of [`Lowerer::census_home_blocks`] and the single-use
/// carry (`still_deopt_named`) refuse on that. Yet `OsrTierBench.kernel`
/// reports `sentinel_free=true` — the body emits no deopt stub and no
/// call-exception stub, so it cannot transfer to the interpreter from anywhere
/// inside itself. Thirty-three frame states, not one of them reachable, and
/// they are what pins every intermediate to memory.
///
/// # Why this does not remove the fail-closed net
///
/// The obvious implementation — stop building the unreachable points — would
/// make the trap classification LOAD-BEARING: misclassify one and a deopt
/// reconstructs a confidently wrong value. This does the opposite. It
/// **predicts** trap-freedom from the graph to decide what to drop, and then
/// **verifies it against the emission that actually happened**: `lower_inner`
/// computes `osr_sentinel_free` from `deopt_stub_patches` and
/// `call_exc_patches` before `build_deopt_points` runs, and the points are
/// skipped only when that says the body really did emit no trap.
///
/// If the prediction was wrong, the points ARE built, `frame_value_for` meets a
/// dropped home with no register describing it at a bci the body can transfer
/// from, latches `UnallocatedValue`, and `lower_inner`'s post-points latch
/// check refuses the compile (that second check is round 9 wave 2's; before
/// it the latch was read only before the points were built, so this sentence
/// described a net that was not there). Nothing is taken away; a case is added in which
/// the net is provably not needed.
pub fn ir_drop_unreachable_homes_enabled() -> bool {
    true
}

/// Kill switch (2026-10-07; item 1 of
/// `docs/feature-designs/deopt-osr.md`):
/// a `getstatic` / `putstatic` of a class not proven initialized at compile
/// time gets a post-call exit site after its helper, which may run
/// `<clinit>` (`Lowerer::maybe_emit_post_static_helper_exit_site`), as the
/// single-pass tier's helper does (`x64::op_invoke::POST_CLINIT_HELPER_EXIT_SITES`).
/// `false` emits no such site, byte for byte as before.
pub(in crate::ir_lower) const IR_POST_STATIC_HELPER_EXITS: bool = true;

/// [`IR_POST_STATIC_HELPER_EXITS`], for the lowering.
pub(in crate::ir_lower) fn ir_post_static_helper_exits_enabled() -> bool {
    IR_POST_STATIC_HELPER_EXITS
}

/// Can lowering `op` emit a deopt stub or a call-exception stub — i.e. can a
/// frame ever transfer to the interpreter from it?
///
/// An ALLOWLIST of ops that cannot, so an op this file has never heard of
/// counts as trapping and the method is treated as reachable. `Op::Div` and
/// `Op::Rem` are absent because their zero-divisor guard is a deopt point;
/// every memory, call, allocation, cast and monitor op is absent for the
/// reasons its own arm gives.
///
/// The claim is checked against the emission rather than trusted: see
/// [`ir_drop_unreachable_homes_enabled`].
pub(crate) fn op_cannot_deopt(op: &Op) -> bool {
    matches!(
        op,
        Op::Start
            | Op::Proj(_)
            | Op::Param(_)
            | Op::Const(_)
            | Op::ConstF(_)
            | Op::Phi
            | Op::Merge
            | Op::Region
            | Op::If
            // A multi-way branch reads a register, compares, and jumps. Even
            // its jump table is read-only data in this method's own code
            // buffer — no call, no allocation, no safepoint.
            | Op::Switch { .. }
            | Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Neg
            | Op::And
            | Op::Or
            | Op::Xor
            | Op::Shl
            | Op::Shr
            | Op::UShr
            | Op::I2L
            | Op::L2I
            | Op::I2F
            | Op::I2D
            | Op::L2F
            | Op::L2D
            | Op::F2I
            | Op::F2L
            | Op::F2D
            | Op::D2I
            | Op::D2L
            | Op::D2F
            | Op::Cmp(_)
            | Op::LCmp
            | Op::FCmp { .. }
            // A return cannot trap. It was missing from the first cut, which
            // is why `OsrTierBench.kernel` -- pure arithmetic and a `Return` --
            // reported `graph_trap_free=false` and the optimization declined
            // itself. The allowlist failing safe is the intended direction;
            // this is the cost of that direction, paid once.
            | Op::Return
            // Pure arithmetic with no control edge, no memory edge and no
            // trapping form -- see `Op::ScalarIntrinsic`. `Math.min`,
            // `Math.max`, `Math.abs` and the two `compare`s cannot throw, which
            // is the whole reason they are expressible as arithmetic here.
            | Op::ScalarIntrinsic(_)
            // Round 11 wave 14 (lane osrmerge): a zero, a load from the
            // interpreter's locals array and a token -- no call, no stub.
            | Op::OsrEntryFlag { .. }
            | Op::OsrLocal(_)
            | Op::OsrMemory
            // JIT round 19 wave 3 (lane eager3): one byte load and a compare.
            | Op::ClassInitTest { .. }
            | Op::CurrentThreadMirror { .. }
            | Op::VthreadBoundaryTest { .. }
            // JIT round 20 wave 11 (lane pIndy): one root word load.
            | Op::IndyRootRef { .. }
            // JIT round 19 wave 5 (lane handler5, SPLICE4-1): a zero, and a
            // frame-word load and clear -- no call, no stub.
            | Op::CatchEntryFlag { .. }
            | Op::CaughtException
            // nio20 L2: the reachability-fence keep-alive emits no code.
            | Op::KeepAlive
            // nio21 wave 3 L2: a zero and a frame-word read -- no call, no
            // stub (the call they follow is the node that calls). Its
            // `CaughtException` is listed with `CatchEntryFlag` above.
            | Op::CallThrew { .. }
            // Round 11 wave 13: a header test with no call and no deopt stub
            // (its arm answers `0` for a null or an array). It may be scheduled
            // inside a caller-held monitor window, which this list admits.
            | Op::ExactClassIs { .. }
            // JIT round 19 (lane osr19): the same header read, answering the
            // class word (`0` for a null or an array). No call, no stub.
            | Op::ClassWordOf
            // JIT round 21 (lane charat, STRINTR21-5): a `TEST`, a branch and
            // one load -- no check, no call, no stub (its guards deopt).
            | Op::StrCharDecode
            | Op::Dead
    )
}

/// `CRATONVM_JIT_IR_VOLATILE_FENCE_SINK` (JIT round 23 close, the zki
/// `volatile-store` row of `r21-lean-allocation-rows-*`; perf, default ON,
/// `=0` kill): a volatile store's StoreLoad fence stands before the next node
/// that needs it rather than right after the store
/// (`Lowerer::ensure_fence_sink_plan`). Read per compile; no `static`.
pub(crate) fn ir_volatile_fence_sink_enabled() -> bool {
    true
}

/// May an owed volatile-store fence stay owed across `op`?
///
/// Only where `op` can neither read under a volatile load's ordering nor
/// leave this frame for code that could: [`op_cannot_deopt`]'s allowlist
/// (arithmetic, constants, branches, header reads -- no call, no stub), minus
/// `Op::Return`, plus the two static accesses the sink exists for. A plain
/// `getstatic` and any `putstatic` read and write their statics block inline;
/// their helper route is a CALL, and the lowering pays an owed fence on that
/// route before it (`emit_owed_fence_before_helper_call`). A volatile
/// `getstatic` is exactly what the fence orders against, so it is not
/// transparent. Everything else -- field loads with their null stubs, guards,
/// calls, allocations, monitors, atomics -- settles the fence first.
pub(crate) fn fence_sink_transparent(op: &Op) -> bool {
    match op {
        Op::Return => false,
        Op::LoadStatic { is_volatile, .. } => !is_volatile,
        Op::StoreStatic { .. } => true,
        _ => op_cannot_deopt(op),
    }
}

/// Can NOTHING in this graph transfer to the interpreter?
///
/// The graph-side half of the `graph_trap_free` prediction, lifted out of
/// `lower_inner` so `ir_optimize` can ask the same question BEFORE lowering --
/// which is where the unroller needs it. `op_cannot_deopt` is an allowlist, so
/// an op nobody has classified counts as trapping and the answer is `false`.
///
/// This is a PREDICTION and is treated as one everywhere it is used: the
/// consumers that act on it are backed by a check against the emission that
/// actually happened, and a disagreement refuses the compile rather than
/// producing a body whose frame states are wrong.
pub fn graph_cannot_deopt(graph: &Graph) -> bool {
    graph
        .nodes
        .iter()
        .all(|n| n.op == Op::Dead || node_cannot_deopt(graph, n))
}

/// [`op_cannot_deopt`] plus the graph-aware clauses the op alone cannot
/// answer:
///
/// * an integer division by a non-zero constant (round 9 wave 2, iropt X2) --
///   see [`int_division_cannot_trap`];
/// * a field store into a receiver that cannot be null (round 9 wave 4,
///   irlower4, NOTES-w3-irmid3 request L3) -- see [`store_cannot_deopt`].
///
/// `pub(crate)` because `ir_optimize::consultable_snapshots_with` asks the same
/// question before lowering: a bci this answers `true` for at every node there
/// is not a transfer site, so its snapshot is not consultable. The emission is
/// what makes that true, so the two MUST read one predicate -- which is why the
/// store clause is a function the `Op::Store` arm calls too.
pub(crate) fn node_cannot_deopt(graph: &Graph, n: &Node) -> bool {
    node_cannot_deopt_with(graph, n, |m| masked_array_load_cannot_deopt(graph, m))
}

/// JIT round 23 wave 9 (lane W9-L7, `CRATONVM_JIT_IR_ANCHOR_TRAP_FREE_SKIP`):
/// can lowering `n` transfer to the interpreter -- a deopt stub, a
/// call-exception exit, a helper that may run `<clinit>` -- so that a frame
/// state at its bci must describe the program point where `n` runs?
///
/// `false` exactly for [`node_cannot_deopt`]'s nodes (pure arithmetic,
/// compares, conversions, constants, phis, a division by a non-zero
/// constant, a store into a fresh object, a mask-proven element read) and a
/// NON-volatile `getstatic` this compile reads directly
/// (`ir_optimize::is_direct_static_read`: two `MOV`s through the class's baked
/// statics cell, no call, no `<clinit>`, no exit). Everything else -- a field
/// read with its null test, an array access, a division by a variable, a
/// guard, a call, an allocation, a cast, an `Unbox` -- can, and still anchors.
///
/// Read by the scheduler's anchor model (`ir_schedule`'s
/// `safepoints_dominate_anchors` / `unsound_sunk_nodes`) and by
/// [`Lowerer::note_bci_anchor`], so the two agree about which blocks a
/// by-bci snapshot is anchored in. The lowering checks the static-read half
/// against its own route (`load_static_reads_directly`) and refuses the
/// compile on a disagreement.
pub(crate) fn can_deopt(graph: &Graph, n: &Node) -> bool {
    if node_cannot_deopt(graph, n) {
        return false;
    }
    !crate::ir_optimize::is_direct_static_read(graph, n)
}

/// `CRATONVM_JIT_IR_ANCHOR_TRAP_FREE_SKIP` (JIT round 23 wave 9, lane W9-L7;
/// perf, **default ON** since 2026-11-09 (`=0` kills it; opt-in until then, n23-w9c A/B: neutral -- SegMono heapget
/// 183/140/155 ns/op on against 206/143/151 off, heapput 155/125/166 against
/// 209/121/130, W2c nocast 2.20/1.78/2.39 against 3.31/1.50/1.80 ns/elem on a
/// noisy host; battery SAME both ways): a node that cannot deopt ([`can_deopt`])
/// anchors no by-bci frame state -- neither in the scheduler's soundness model
/// nor in `bci_native`. Hoisted or invariant trap-free nodes keep their in-loop
/// pcs (a pre-header `getstatic LE_INT` at pc 14, the invariant `length - 4` at
/// pc 19 in `R23W4SegMono.heapGet`), and as anchors they pinned the in-loop
/// snapshots of those bcis to the pre-header, so a value those snapshots name
/// (`31 * h`, `(long) k`) could never leave the loop header. No deopt can be
/// taken at such a node, so no point built there could ever be consulted.
/// Read per compile / per node; no `static`.
pub(crate) fn ir_anchor_trap_free_skip_enabled() -> bool {
    true
}

/// [`node_cannot_deopt`] with the masked-load clause supplied by the caller
/// (nio21 wave 7, lane L5): the free function asks
/// [`masked_array_load_cannot_deopt`] per node, the lowering its per-compile
/// cache (`Lowerer::node_cannot_deopt_here`). One list of clauses for both.
pub(crate) fn guard_const_true_cannot_trap(graph: &Graph, node: &Node) -> bool {
    graph.catch_arm_sites.is_empty()
        && matches!(node.op, Op::Guard { .. })
        && node
            .inputs
            .get(1)
            .and_then(|&cid| graph.nodes.get(cid as usize))
            .is_some_and(|cn| {
                cn.ty == IrType::Int && matches!(cn.op, Op::Const(v) if (v as i32) != 0)
            })
}

pub(crate) fn node_cannot_deopt_with(
    graph: &Graph,
    n: &Node,
    masked_load: impl Fn(&Node) -> bool,
) -> bool {
    op_cannot_deopt(&n.op)
        || guard_const_true_cannot_trap(graph, n)
        || int_division_cannot_trap(graph, n)
        || store_cannot_deopt(graph, n)
        || masked_load(n)
}

/// nio21 wave 5 (lane L5; `r20-masked-index-loop-keeps-its-load-and-checks`
/// step 2): an `Op::ArrayLoad` `a[p & m]` whose null AND bounds checks a
/// loop-entry `Guard(c < a.length)`, `c >= m`, proves by graph shape
/// (`ir_optimize::mask_guard_proves_access`), under
/// `CRATONVM_JIT_IR_MASK_GUARD_ELIDE` (default ON since nio21 wave 7). The `ArrayLoad` arm emits
/// its checks only through [`Lowerer::emit_array_null_bounds_guards_for`],
/// which ORs exactly this predicate into BOTH elisions, and nothing else in
/// either of its routes (GPR or XMM) can transfer: the element load is one
/// `MOV` / `MOVSS` / `MOVSD` (plus the narrow-oop decode for a reference),
/// with no call and no stub. So the claim is exact, by the store clause's
/// pattern: one predicate, read by the emission and by this prediction.
/// `ArrayStore` is not claimed (its reference form calls `jit_aastore`).
pub(crate) fn masked_array_load_cannot_deopt(graph: &Graph, n: &Node) -> bool {
    matches!(n.op, Op::ArrayLoad(_))
        && crate::ir_optimize::mask_guard_elide_enabled()
        && crate::ir_optimize::mask_guard_proves_access(graph, n)
}

/// Does lowering this `Op::Store` emit no deopt point at all?
///
/// True exactly when BOTH hold, which are the two things the `Op::Store` arm
/// can deopt on:
///
/// * the field-offset input is an `Op::Const` (anything else takes the arm's
///   unconditional deopt), and
/// * the receiver is definitely non-null: a fresh allocation/materialiser,
///   or a Phi whose incoming values are all proven non-null (a guarded OSR
///   value is safe because a failed exact-class guard deopts before use).
///   Failed allocations and resolutions also leave through their own checks.
///   premise `ir_check_elim` already relies on (default on) to drop the null
///   check of an array access on a fresh array. The arm then skips its
///   `TEST RAX,RAX; deopt` receiver check
///   ([`Lowerer::emit_store_receiver_null_check`]).
///
/// Nothing else in any of the four routes transfers: the inline int store
/// writes through the receiver; `jit_putfield_int`/`_long`/`_float`/`_double`
/// return `()` and nothing after them is checked; the gated reference store's
/// bails all land on `jit_putfield_object`, which also returns `()` and is
/// followed by no sentinel compare. (Those helpers are CALLS -- which is why
/// `op_cannot_deopt` still does not list `Op::Store`: it is also read as "can
/// a collector run here" by the residency invalidation, and that answer stays
/// "yes".)
///
/// Why it matters: the snapshot at a `putfield`'s own bci names the receiver
/// on its operand stack. While every store could deopt, that snapshot was
/// consultable for every allocation that was ever stored to, which kept
/// `ir_optimize::eliminate_write_only_stores` inert under snapshot liveness
/// (`ir-snapshots-at-every-bci-pin-values-and-now-stores-20260918.md`).
pub(crate) fn store_cannot_deopt(graph: &Graph, n: &Node) -> bool {
    // The lowered layout only: `[ctrl, mem, base, offset, value]`, which is
    // what the `Op::Store` arm reads. (The optimizer tolerates shorter test
    // layouts in which slot 3 is the VALUE; they are never lowered, and
    // answering for them would misread a constant value as an offset.)
    if !matches!(n.op, Op::Store(_)) || n.inputs.len() < 5 {
        return false;
    }
    let (Some(&base), Some(&offset)) = (n.inputs.get(2), n.inputs.get(3)) else {
        return false;
    };
    let offset_is_const = graph
        .nodes
        .get(offset as usize)
        .is_some_and(|o| matches!(o.op, Op::Const(_)));
    offset_is_const && store_receiver_cannot_be_null(graph, base)
}

/// Is `base` (an `Op::Store`'s receiver input) a node that is never null at a
/// use? The one predicate the `Op::Store` arm's null-check elision and
/// [`store_cannot_deopt`] share.
pub(in crate::ir_lower) fn store_receiver_cannot_be_null(graph: &Graph, base: NodeId) -> bool {
    graph.nodes.get(base as usize).is_some_and(|b| {
        crate::ir_check_elim::definitely_non_null(&b.op)
            || (b.op == Op::Phi
                && crate::ir_optimize::phi_non_null_on_every_edge(
                    graph,
                    base,
                    &mut rustc_hash::FxHashSet::default(),
                    0,
                ))
    })
}

/// `CRATONVM_JIT_IR_SCOPED_BYTE_ARRAY_BASE_WIDEN` (JIT round 20, lane memory;
/// page `r20-heapbb-twelve-x-residual-is-guards-not-calls-CLOSED-20261012.md`;
/// **default OFF** since round 20 wave 2, opt-in `=1`; off keeps the nio21
/// wave 7 fact exactly -- the host A/B showed no gain: a standalone
/// `HeapByteBuffer.getInt` body is reached through a call that costs far more
/// than the screen it saves): the `ByteBuffer.hb`
/// fact of `CRATONVM_JIT_IR_SCOPED_BYTE_ARRAY_BASE` also covers the compiling
/// method's OWN `hb` reads (`CompileRequest::cp_field_byte_array_resolver`,
/// fed in `lib.rs` `ir_tier_attempt`) and a φ of such reads
/// ([`scoped_base_is_byte_array`]). Read per compile.
pub(crate) fn ir_scoped_byte_array_base_widen_enabled() -> bool {
    false
}

/// JIT round 20 (lane memory): is a scoped access's `base` a `byte[]` or null
/// by [`Graph::byte_array_nodes`] -- the node itself, or a φ (a loop header's
/// or a join's) whose every value input is, recursively, such a node or the
/// null literal? A φ of "a `byte[]` or null" values is one too; a pass that
/// merges or carries an `hb` read (a loop-carried forward, a join of two
/// spliced bodies) used to drop the fact, because the set names only the
/// loads themselves. Bounded at sixteen nodes; anything else answers `false`
/// (the base keeps the primitive-array screen, as before). A null base takes
/// the off-heap arm either way, so a null input changes nothing.
pub(in crate::ir_lower) fn scoped_base_is_byte_array(graph: &Graph, base: NodeId) -> bool {
    if graph.byte_array_nodes.contains(&base) {
        return true;
    }
    let mut work: Vec<NodeId> = vec![base];
    let mut seen: Vec<NodeId> = Vec::new();
    while let Some(id) = work.pop() {
        if seen.contains(&id) {
            continue;
        }
        if seen.len() >= 16 {
            return false;
        }
        seen.push(id);
        if graph.byte_array_nodes.contains(&id) {
            continue;
        }
        let Some(node) = graph.nodes.get(id as usize) else {
            return false;
        };
        if crate::ir::is_null_const(node) {
            continue;
        }
        if !matches!(node.op, Op::Phi) || node.inputs.len() < 2 {
            return false;
        }
        // `[merge, val_0, val_1, ..]`.
        work.extend(node.inputs[1..].iter().copied());
    }
    true
}

/// Round nio20 wave 3 (lane L4): the lowest one-dimensional PRIMITIVE array
/// tag byte (`[Z`, `ArrayElementType::Boolean`), the bias of
/// [`Lowerer::lower_scoped_raw`]'s one-compare array screen.
pub(in crate::ir_lower) const SCOPED_PRIM_ARRAY_TAG_LO: u8 = (cratonvm_types::ObjectKind::Array
    as u8)
    | ((cratonvm_types::ArrayElementType::Boolean as u8) << 2);

/// The bits `tag - SCOPED_PRIM_ARRAY_TAG_LO` may have for a primitive array:
/// `(elem - Boolean) << 2` for `elem` in `Boolean ..= Long`, i.e. `0x1C`.
pub(in crate::ir_lower) const SCOPED_PRIM_ARRAY_TAG_SPAN: u32 =
    ((cratonvm_types::ArrayElementType::Long as u32)
        - (cratonvm_types::ArrayElementType::Boolean as u32))
        << 2;

/// The screened heap arm's element-size read: `MOVZX ECX, BYTE [RDX +
/// KIND_TAGS]; SHR ECX, 2; AND ECX, 3` -- log2 of the element size of a
/// base already proven a one-dimensional primitive array.
pub(in crate::ir_lower) fn scoped_screened_shift_bytes() -> [u8; 10] {
    const KIND_TAGS: u8 = cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8;
    [
        0x0F, 0xB6, 0x4A, KIND_TAGS, 0xC1, 0xE9, 0x02, 0x83, 0xE1, 0x03,
    ]
}

// The screen's premises, checked where the layout is declared: the primitive
// element types are exactly the contiguous `Boolean ..= Long` (4..=11), the
// span is a run of whole multiples of 4 (`0b11100`), the kind sits in bits
// 0..1, and an array's bits 6..7 are reserved zero.
const _: () = {
    use cratonvm_types::ArrayElementType as E;
    assert!(E::Boolean as u8 == 4 && E::Long as u8 == 11);
    assert!(E::Char as u8 == 5 && E::Float as u8 == 6 && E::Double as u8 == 7);
    assert!(E::Byte as u8 == 8 && E::Short as u8 == 9 && E::Int as u8 == 10);
    assert!(E::Reference as u8 == 0);
    assert!(SCOPED_PRIM_ARRAY_TAG_SPAN == 0x1C);
    assert!(cratonvm_types::KIND_TAG_BYTE_MASK == 0x3);
    assert!(
        (cratonvm_types::MARK_RESERVED_MASK >> cratonvm_types::MARK_QUARTET_SHIFT) == 0xC0,
        "an array's reserved tag bits are the byte's bits 6..7"
    );
};

/// The divisor of an integer `Op::Div`/`Op::Rem` when it is a compile-time
/// `Op::Const`, normalised to the division's width (`ty`): an `Int` division
/// reads only the low 32 bits of the constant, so `(c as i32)` is what the
/// `IDIV ECX` sees. `None` for anything else, and for FP types.
pub(in crate::ir_lower) fn const_int_divisor(
    graph: &Graph,
    ty: IrType,
    divisor: NodeId,
) -> Option<i64> {
    let c = match graph.nodes.get(divisor as usize).map(|n| &n.op) {
        Some(Op::Const(c)) => *c,
        _ => return None,
    };
    match ty {
        // Cast: an `int` division reads the low 32 bits; sign-extend them back.
        IrType::Int => Some(i64::from(c as i32)),
        IrType::Long => Some(c),
        _ => None,
    }
}

/// The signed-division magic number for the divisor `a` (`3 <= a < 2^(width-1)`,
/// not a power of two) at `width` 32 or 64: Hacker's Delight figure 10-1
/// (`magic`), in exact `u128` arithmetic. Returns `(m, s)` with `m < 2^width`
/// such that for every `n` of that width
/// `n / a == floor(n * m / 2^(width + s)) + (n < 0 ? 1 : 0)`.
///
/// For `width == 64` the caller multiplies by `m` as a SIGNED 64-bit value; when
/// `m >= 2^63` that value is `m - 2^64`, and the true product is recovered by
/// adding `n` to the high half (HD 10-4). For `width == 32`, `m < 2^32` fits a
/// 64-bit multiplier as is.
///
/// Note HD's loop test is `delta = a - r2` and continues while
/// `q1 < delta || (q1 == delta && r1 == 0)`. `x64::arith::magic_signed_div*`
/// use the same test since round 11 wave 3; before that they exited the loop
/// one step early and compiled `(1 << 30) / 1073741825` and
/// `(1L << 62) / (2^62 + 1)` to 1 instead of 0.
pub(in crate::ir_lower) fn signed_div_magic(a: u64, width: u32) -> (u64, u32) {
    debug_assert!(width == 32 || width == 64);
    debug_assert!(a >= 3 && !a.is_power_of_two());
    let ad = u128::from(a);
    let two = 1u128 << (width - 1);
    let anc = two - 1 - two % ad;
    let mut p = width - 1;
    let mut q1 = two / anc;
    let mut r1 = two - q1 * anc;
    let mut q2 = two / ad;
    let mut r2 = two - q2 * ad;
    loop {
        p += 1;
        q1 *= 2;
        r1 *= 2;
        if r1 >= anc {
            q1 += 1;
            r1 -= anc;
        }
        q2 *= 2;
        r2 *= 2;
        if r2 >= ad {
            q2 += 1;
            r2 -= ad;
        }
        let delta = ad - r2;
        if !(q1 < delta || (q1 == delta && r1 == 0)) {
            break;
        }
        // HD bounds p by 2 * width; this is a belt against a loop that
        // should not happen, never the exit on a valid input.
        if p >= 2 * width {
            break;
        }
    }
    // Cast: q2 + 1 < 2^width by HD's theorem (checked by
    // `r9w3_signed_div_magic_matches_brute_force`).
    ((q2 + 1) as u64, p - width)
}

/// Is `n` an integer `Op::Div`/`Op::Rem` whose divisor is a non-zero
/// constant? Such a division cannot trap: `Lowerer::emit_div_zero_guard`
/// emits nothing for it (no `TEST`, no `DivByZero` point), and the `MIN / -1`
/// case never raises either (it is materialised inline). This is the
/// PREDICTION half; the emission half is those two guards, and
/// `build_deopt_points` refuses to trust the prediction unless the body also
/// emitted no deopt stub.
///
/// `pub(crate)` since round 9 wave 11 (irl11): `ir_optimize::unroll` clones
/// such a division into an unrolled body, on this same prediction.
pub(crate) fn int_division_cannot_trap(graph: &Graph, n: &Node) -> bool {
    matches!(n.op, Op::Div | Op::Rem)
        && n.inputs.len() >= 2
        && const_int_divisor(graph, n.ty, n.inputs[1]).is_some_and(|c| c != 0)
}

/// Does lowering `op` read its first input into RAX, and its second (if it has
/// one) into RCX, before emitting anything else?
///
/// The consumer half of the carry contract, and — like
/// [`op_home_is_one_store_rax`] — an audit of the arms rather than a guess.
/// Every op here has the same shape: `alloc_slot`, `gp_load_value(RAX,
/// inputs[0])`, `gp_load_value(RCX, inputs[1])`, the ALU, `store_rax`. That is
/// what makes both carry registers safe:
///
/// * RAX, because NOTHING is emitted between the producer's last instruction
///   and the consumer's first read — checked, not argued: the read refuses
///   unless `buf.pos()` is still where the producer left it;
/// * RCX, because the only thing emitted before the second read is the first
///   read, and a `gp_load_value` into RAX writes RAX and nothing else.
///
/// `Op::Cmp`, `Op::LCmp` and `Op::FCmp` are absent: a comparison may be fused
/// into its branch and then emits nothing at all. `Op::Load`, `Op::ArrayLoad`
/// and `Op::Call` are absent because they read their operands in another order
/// or through another path.
pub(in crate::ir_lower) fn op_reads_rax_then_rcx(op: &Op) -> bool {
    matches!(
        op,
        Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Div
            | Op::Rem
            | Op::Neg
            | Op::And
            | Op::Or
            | Op::Xor
            | Op::Shl
            | Op::Shr
            | Op::UShr
            | Op::I2L
            | Op::L2I
    )
}

/// May the optimizing tier lower `aastore` through the `jit_aastore` helper?
/// **Default ON** since 2026-09-06; `CRATONVM_JIT_IR_AASTORE=0` restores the
/// fail-closed refusal, which refuses the whole method.
pub fn ir_aastore_enabled() -> bool {
    // j27 MEMOSLOT: a `MemoSlot`, so a test override reaches it.
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    crate::memo_flag::memo_bool(&ON, || {
        !matches!(
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
            Ok("0") | Ok("false") | Ok("off") | Ok("no")
        )
    })
}

/// gcf f1/p: `cratonvm_gc::card_table::JitCardView::TRACE_IR_DIRTY_OFFSET` and
/// `TRACE_IR_CLEAN_OFFSET` — the card view words the optimizing tier's PRE
/// card check counts into under `CRATONVM_DBG_IR_REF_STORE_TRACE=1`
/// (`Lowerer::emit_ir_card_pre_check`). Disp8s off the view in R11; pinned by
/// `gcf_f1p_ir_card_trace_offsets_match_the_view`.
pub(in crate::ir_lower) const IR_CARD_TRACE_DIRTY: u8 = 56;
pub(in crate::ir_lower) const IR_CARD_TRACE_CLEAN: u8 = 64;

/// gcf f2/p: the card-store barrier's view words and constants, this tier's
/// copies of `x64::objects`'s (`JitCardView::SCAN_BOUND_OFFSET`,
/// `SUMMARY_NEG_OFFSET`, `FLAG_SUMMARY`, `FLAGS_OFFSET`, and
/// `log2(SUMMARY_GROUP)`); pinned by
/// `gcf_f2p_ir_card_store_offsets_match_the_view`.
pub(in crate::ir_lower) const IR_CARD_VIEW_SCAN_BOUND: i32 = 72;
pub(in crate::ir_lower) const IR_CARD_VIEW_SUMMARY_NEG: i32 = 80;
pub(in crate::ir_lower) const IR_CARD_VIEW_FLAGS: usize = 32;
pub(in crate::ir_lower) const IR_CARD_VIEW_FLAG_SUMMARY: usize = 2;
pub(in crate::ir_lower) const IR_CARD_SUMMARY_SHIFT: u8 = 6;

/// gcf f2/p — `CRATONVM_JIT_CARD_STORE_BARRIER` (default ON since gcg g6/jm) on this tier:
/// the PRE card check's CLEAN edge stores the summary and card bytes inline
/// (`Lowerer::emit_ir_card_store_inline`) instead of taking the helper. The
/// same switch the single-pass tier reads (`x64::objects`'s
/// `jit_card_store_barrier_enabled`), read per compiled site.
pub(in crate::ir_lower) fn ir_card_store_barrier_enabled() -> bool {
    // gcg g6/jm: default ON (vg5: forms 21 ok on both tiers, `[rset-verify]
    // missing=0`, conc / hashmap / G1 / ZGC SAME; the clean-card rows within
    // 2x of the single-pass tier, `p1b_cs_sparse` 2.4x faster). `=0` kills it.
    true
}

/// gcf f2/p: does the view at `view` (validated by `x64::gen_card_view_of`,
/// whose result `ir_card_view` holds) carry a summary map?
pub(in crate::ir_lower) fn ir_card_view_has_summary(view: usize) -> bool {
    if view == 0 || (view & 7) != 0 {
        return false;
    }
    // SAFETY: `view` is a live `JitCardView` whose magic `gen_card_view_of`
    // checked; word 4 (`flags`) is written once at construction.
    let flags = unsafe { std::ptr::read((view + IR_CARD_VIEW_FLAGS) as *const usize) };
    (flags & IR_CARD_VIEW_FLAG_SUMMARY) != 0
}

/// gcf f2/p: how [`Lowerer::emit_ir_card_store_inline`] recomputes the
/// address whose card it marks into R10: the receiver in RAX (a field store,
/// or an array's header card), or the ELEMENT slot `[RAX + RCX*8 + disp8]` of
/// an element-precise `aastore` (RCX = the zero-extended index).
#[derive(Clone, Copy)]
pub(in crate::ir_lower) enum IrCardMark {
    Rax,
    Element(u8),
}

/// May a gated `aastore` into an old array check the generational card
/// inline instead of calling `jit_aastore` (round 13 wave 2, lane irhash,
/// W7-2; `Lowerer::emit_ir_aastore_card_arm`)?
///
/// Rides the single-pass tier's switch, `CRATONVM_JIT_INLINE_CARD_MARK`
/// (default ON since the gcg triage, tg1), so
/// one A/B switches both tiers;
/// `CRATONVM_JIT_IR_INLINE_CARD_CHECK=0` (default ON) keeps this tier on the
/// helper while the single-pass tier checks inline. Read per compiled site,
/// live, so a test can drive it with `flags::with_thread_overrides`.
pub(in crate::ir_lower) fn ir_inline_card_check_enabled() -> bool {
    cratonvm_types::flags().jit.inline_card_mark
}

/// The `[receiver + disp32]` displacements of a gated reference `putfield`'s
/// LEGACY 16-byte field cell (gce e1/f): the long header's slot count, the
/// cell's tag word and its 64-bit payload. Computed once per site by
/// `Lowerer::emit_gated_ir_ref_putfield` and shared by its two inline stores.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ir_lower) struct IrLegacyRefCell {
    pub(in crate::ir_lower) num_slots_disp: i32,
    pub(in crate::ir_lower) tag_disp: i32,
    pub(in crate::ir_lower) payload_disp: i32,
}

/// References admitted to the GP register file, and cached copies dropped
/// because a collector could have run since the copy was made.
///
/// Read as a pair, and read `dropped == 0` with suspicion: a workload that
/// promoted references but never invalidated one either has no calls at all or
/// has an invalidation that is not wired, and only the counter distinguishes
/// them.
pub(in crate::ir_lower) static REF_RESIDENCY_ADMITTED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(in crate::ir_lower) static REF_RESIDENCY_DROPPED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// `(references given a register, cached copies invalidated)`, process-wide.
pub fn ir_ref_residency_census() -> (u64, u64) {
    (
        REF_RESIDENCY_ADMITTED.load(std::sync::atomic::Ordering::Relaxed),
        REF_RESIDENCY_DROPPED.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// May a REFERENCE be register-resident in this backend? **default off**;
/// `CRATONVM_JIT_IR_REF_RESIDENCY=1` turns it on.
///
/// # What was actually blocking this
///
/// The tree's standing answer — `docs/feature-designs/two-backends-decision.md`
/// step 1, and this file's own bank match — was "`OopMapEntry` has no register
/// bank, so a collector could neither see nor relocate a reference in a
/// register". That is a correct statement about a backend whose register file
/// is AUTHORITATIVE. This one's is not: it is a write-through read cache over a
/// frame slot, the slot is written at every definition, and `emit_safepoint_map`
/// publishes that slot. The collector's view was never incomplete. What was
/// wrong was only the *cached copy*, and only after a collection — a moving
/// collector rewrites the slot and cannot rewrite the copy.
///
/// So the obligation is not "tell the collector about the register". It is
/// "never read a copy the collector may have invalidated", and
/// [`Lowerer::invalidate_ref_residency`] discharges it directly. This is the
/// same shape as the R4 lesson: the blocker was a whole-mechanism proxy
/// standing in for a per-point question.
///
/// # Why the allocator's own rule is not the safety net
///
/// `allocate_linear_scan` refuses a reference whose live range covers a
/// `MachineModel::safepoint`, and this flag lifts that refusal
/// (`refs_may_cross_safepoints`). It has to: that model is built from
/// `ir_op_is_safepoint`, a model of *oop-map publication* sites, and this
/// backend also emits plain helper calls at ops that are not on that list —
/// `Op::Load` reaching `jit_getfield` is one, `Op::Store` reaching
/// `jit_putfield_object` another. Leaning on it would have been leaning on an
/// under-approximation. The invalidation instead asks an ALLOWLIST
/// (`op_cannot_deopt`): an op nobody has classified invalidates, so an op added
/// to `Op` later is safe by default. A false invalidation costs a reference its
/// register between two instructions; a false omission is a stale oop, which is
/// heap corruption, so the polarity is the whole design.
///
/// Default OFF until the GC soak suites have run against it — the failure it
/// risks is silent and the flag is how both arms are timed from one binary.
pub(in crate::ir_lower) fn ir_ref_residency_enabled() -> bool {
    true
}

/// May a promoted reference's live range CROSS a safepoint? **Default ON**
/// (within `ir-ref-residency`, which is itself off); `-ir-ref-residency-cross-safepoint`
/// restricts promotion to ranges with no safepoint in them at all.
///
/// The two are worth separating because they cost different things, and the
/// difference is not a detail — it is the whole result. Crossing admits the
/// shapes that matter (a loop-carried node pointer, an array base held over a
/// call) but the allocator then RESERVES a register for the whole range while
/// `invalidate_ref_residency` makes the copy unreadable for most of it, so the
/// promotion can cost a register without buying a single avoided reload.
/// Restricting to safepoint-free ranges gives the register back wherever the
/// copy would have been dead anyway.
///
/// Which one wins is a question about the workload's shape and is answered by
/// measurement; see `docs/feature-designs/two-backends-decision.md`.
pub(in crate::ir_lower) fn ir_ref_residency_cross_safepoint_enabled() -> bool {
    !matches!(
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
        Ok("0") | Ok("false") | Ok("off") | Ok("no")
    )
}

/// `CRATONVM_JIT_IR_LS_SPLITS=1` — take the values the linear scan SPLIT, and
/// follow them from register to register and back to memory instead of
/// refusing them. **Default OFF.**
///
/// # What it is off
///
/// Exactly the emission this file produced before it existed. A value the scan
/// split lands in `plan_register_residency`'s `_` arm, is counted under
/// `split_or_spilled=` and gets no register at all — a home store and a reload
/// at every use, which is what it had before the allocator existed. Nothing
/// else about the plan moves, so the shipping binary is byte-identical until
/// somebody measures this.
///
/// # What it is on
///
/// The residency cache stops being a property of a whole live range and becomes
/// a property of a POSITION. `alloc.location_at(node, pos)` is the allocator's
/// answer to "where is this value here", and this flag is what makes the
/// emission agree with it: `reg_of` / `gp_reg_of` are re-pointed at each
/// transition, so `resident_xmm` / `resident_gpr` — through which every read
/// already goes — answer for the position the emission has reached rather than
/// for the value's whole life.
///
/// That turns on the consumers of five things the allocator has been computing
/// and this file has been discarding: `Allocation::events`, `SpillKind::Remat`
/// and `regalloc::remat_of`, `reg_moves`, the eviction cost model
/// (`ls_pick_victim`, `SPILL_DISTANCE_SCALE`, `ls_carry_relief`) and the
/// `splits` counter itself. See `NOTES-regalloc.md` §1 and
/// `docs/jit/linear-scan-regalloc.md`.
///
/// # What it is NOT
///
/// It is not a waiver of anything. A managed value keeps its home word
/// (`set_residency` withholds droppability from the whole population), is never
/// named in a deopt frame (`set_residency` poisons the exclusive-ownership
/// count for every register it visits), and every `Ref` copy it publishes lands
/// inside `lower_block`'s per-node invalidation audit — the refusals in
/// `ls_split_transitions` exist to keep that last one true, and a value whose
/// placement cannot be proved gets no register rather than a guess.
pub(in crate::ir_lower) fn ir_ls_splits_enabled() -> bool {
    matches!(
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
        Ok("1") | Ok("true") | Ok("on") | Ok("yes")
    )
}

/// `CRATONVM_JIT_IR_LS_SPLITS_CARRIED_FIRST=1` (default on; JIT round 19 proposal
/// wave 11, lane lv11; the LV10-6 / i64-L2 answer): a GP split value that
/// visits a register the loop-carried reservation would hand out yields it
/// (is demoted to its home) -- see the "carried values first" pass in
/// [`plan_register_residency_with`]. Only meaningful under
/// [`ir_ls_splits_enabled`] with the gate open
/// (`CRATONVM_JIT_IR_SLOTS_PHI_EDGE_RANGES`); it only ever removes split
/// values. Read uncached like the split switch.
pub(in crate::ir_lower) fn ir_ls_splits_carried_first_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_LS_SPLITS_CARRIED_PRICED=1` (default on; JIT round 19
/// proposal wave 12, lane lv12, LV11-2): the priced form of
/// [`ir_ls_splits_carried_first_enabled`]. A GP split value that visits a
/// register the carried reservation wants yields only when its
/// [`split_register_benefit`] does not exceed the summed loop weight of the
/// carried values assigned those registers in the dry run. Runs the same dry
/// run; under `CARRIED_FIRST` as well, the price still decides.
pub(in crate::ir_lower) fn ir_ls_splits_carried_priced_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_LS_SPLITS_REFUSE_NO_GAIN=1` (default on; JIT round 19 proposal
/// wave 12, lane lv12, LV11-3): the split arm refuses a placement whose
/// [`split_register_benefit`] is not positive -- e.g. "defined in memory,
/// loaded one position later, read once", which a memory operand does for
/// free.
pub(in crate::ir_lower) fn ir_ls_splits_refuse_no_gain_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_LS_PREPIN_REFS_UNDER_SPLITS=1` (default on; JIT round 19
/// proposal wave 12, lane lv12, LV11-1): [`ls_certain_refusals`] lists a `Ref`
/// as a certain refusal under `CRATONVM_JIT_IR_LS_SPLITS` too (while
/// reference residency is off), since `ls_bank_for` refuses it in the split
/// arm as well. Only matters with `ir_ls_prepin_refused_enabled`.
pub(in crate::ir_lower) fn ir_ls_prepin_refs_under_splits_enabled() -> bool {
    true
}

/// What a split value's registers buy (JIT round 19 proposal wave 12, lane
/// lv12; LV11-2 / LV11-3): the loop-weighted reads of `id` served while it
/// sits in a register (a use position inside a segment that holds one), minus
/// the loop-weighted `Load` transitions that fill those registers. A register
/// read saves one memory access over the memory operand the unsplit lowering
/// uses, and a `Load` costs one, so zero or less means the split saves
/// nothing. Weights are `regalloc`'s per-position loop weight
/// (`LOOP_WEIGHT_PER_DEPTH^depth`, capped).
pub(in crate::ir_lower) fn split_register_benefit(
    live: &crate::regalloc::LiveModel,
    id: usize,
    segs: &[crate::regalloc::Segment],
    transitions: &[(usize, LsTransition)],
) -> i128 {
    use crate::regalloc::{LOOP_WEIGHT_PER_DEPTH, MAX_LOOP_DEPTH_WEIGHT};
    let pos_weight = |p: usize| -> i128 {
        let b = live.block_of_pos.get(p).copied().unwrap_or(0);
        let depth = live.loop_depth.get(b).copied().unwrap_or(0);
        i128::from(
            LOOP_WEIGHT_PER_DEPTH
                .checked_pow(depth)
                .unwrap_or(MAX_LOOP_DEPTH_WEIGHT)
                .min(MAX_LOOP_DEPTH_WEIGHT),
        )
    };
    let mut benefit: i128 = 0;
    for &u in live.uses.get(id).map(Vec::as_slice).unwrap_or(&[]) {
        if segs
            .iter()
            .any(|seg| seg.reg.is_some() && seg.range.lo <= u && u <= seg.range.hi)
        {
            benefit += pos_weight(u);
        }
    }
    for &(pos, t) in transitions {
        // A `RematAdd` is priced as the load it may degrade to, so the
        // remat switch never changes which values are admitted.
        if matches!(t, LsTransition::Load { .. } | LsTransition::RematAdd { .. }) {
            benefit -= pos_weight(pos);
        }
    }
    benefit
}

/// Block-exit edges that became a real fall-through because the layout already
/// placed the successor next, and those that still needed a `JMP rel32`.
///
/// Read as a pair. `elided` alone cannot distinguish a layout that is working
/// from a method whose blocks happen to be in source order anyway; the ratio is
/// what says whether `ir_schedule`'s frequency-driven layout is buying
/// anything, which until the elision existed it provably was not.
pub(in crate::ir_lower) static FALLTHROUGHS_ELIDED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(in crate::ir_lower) static BLOCK_JMPS_EMITTED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Inline field-read sites that were the FIRST in their block.
pub(in crate::ir_lower) static IR_FIELD_SITE_FIRST: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// Inline field-read sites that were NOT — each one repeats a layout-epoch
/// guard and a per-object compactness test the site before it already ran.
pub(in crate::ir_lower) static IR_FIELD_SITE_LATER: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// `(first_in_block, later_in_block)` over every inline field read this process
/// emitted. The closing identity: the two sum to the number of inline
/// `getfield` sites emitted, which is the `getfield` total minus the declines
/// `metrics::IR_GETFIELD_DECLINE_NAMES` itemises.
pub fn ir_field_site_census() -> (u64, u64) {
    (
        IR_FIELD_SITE_FIRST.load(std::sync::atomic::Ordering::Relaxed),
        IR_FIELD_SITE_LATER.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// Safepoint polls emitted with their slow path AFTER the body.
pub(in crate::ir_lower) static IR_POLLS_OUTLINED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// Safepoint polls emitted with their slow path inline, the historical shape.
pub(in crate::ir_lower) static IR_POLLS_INLINE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// `(outlined, inline)` — every poll this process emitted, split by shape.
///
/// The two are exhaustive by construction: [`Lowerer::emit_safepoint_poll`]
/// returns early before either counter when polls are switched off entirely,
/// and otherwise takes exactly one of the two arms. So `outlined + inline` is
/// the number of polls emitted, and a poll that counted neither is a third arm
/// someone added without a counter.
///
/// Counted rather than asserted for the same reason
/// [`ir_fallthrough_census`] is: a switch that silently does nothing looks
/// exactly like a switch whose effect is invisible, and only a count tells
/// them apart. Process-global and monotone — read a DELTA around the compile
/// under test, never an absolute.
pub fn ir_poll_census() -> (u64, u64) {
    (
        IR_POLLS_OUTLINED.load(std::sync::atomic::Ordering::Relaxed),
        IR_POLLS_INLINE.load(std::sync::atomic::Ordering::Relaxed),
    )
}

thread_local! {
    /// Per-thread mirror of the two counters above, bumped at the same sites.
    ///
    /// The process-global pair is what the reporting wants; it is the wrong
    /// instrument for a TEST, and the difference is a flake rather than a
    /// theory. A test sets its outline switch through
    /// `with_thread_overrides`, so only the testing thread's arm is known
    /// -- but every other thread in a parallel `cargo test` is compiling
    /// fixtures that emit INLINE polls into the same global counter. A delta
    /// read around one compile therefore measures this thread's outlined polls
    /// and the whole suite's inline ones.
    ///
    /// `an_outlined_safepoint_poll_stops_when_the_inline_one_does_and_not_otherwise`
    /// (deleted with its switch in round 14 wave 2) already excused this on its first assertion ("the census is
    /// PROCESS-global and the suite runs in parallel, so the INLINE count picks
    /// up whatever else is compiling on another thread") and then asserted
    /// `out_census.1 == 0` anyway -- which failed **two runs in three** on an
    /// unmodified tree, measured 2026-09-12.
    pub(in crate::ir_lower) static IR_POLLS_LOCAL: std::cell::Cell<(u64, u64)> = const {
        std::cell::Cell::new((0, 0))
    };
}

/// This THREAD's `(outlined, inline)` poll counts. See [`IR_POLLS_LOCAL`].
pub fn ir_poll_census_local() -> (u64, u64) {
    IR_POLLS_LOCAL.with(std::cell::Cell::get)
}

/// Why [`Lowerer::back_edge_mode_exit_state`] gave an unconditional back-edge
/// poll no mode exit. Interpreter round i1 wave 17, lane L1
/// (`i15-L2-ir-poll-mode-exit-coverage-has-no-census`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ir_lower) enum ModeExitRefusal {
    /// Not an OSR-door compile, or the exits are switched off: nothing was
    /// sought, and nothing is counted.
    NotSought,
    /// Not a back edge into a merge header of this method: a spliced header,
    /// a monitor at the header, a missing bci.
    Shape,
    /// No safepoint snapshot survives at the header's bci.
    NoHeaderSnapshot,
    /// A header φ with no input, or two inputs, on this edge.
    HeaderPhiInput,
    /// A φ of another merge that does not dominate the header (the opaque
    /// OSR merge's, a stale snapshot's).
    ForeignPhi,
    /// A value not yet emitted in layout order, or whose block does not
    /// dominate the header.
    NotCurrent,
    /// A register-described value in a frame with no deopt register image.
    NoRegisterImage,
    /// A value with no location at the poll: a dropped home not resident in a
    /// register there.
    Unlocatable,
    /// `osr_exit::guard_exit_point_resumable` refused the frame.
    NotResumable,
}

/// Under `CRATONVM_DBG_JITC=1`, one line per unconditional back-edge poll
/// whose mode exit [`Lowerer::back_edge_mode_exit_state`] refused as
/// [`ModeExitRefusal::Shape`], naming which structural test refused it:
/// `[ir-mode-exit-refused] shape:<why> <osr-door|method-entry>` (interpreter
/// round i2 wave 56, lane L2). Rank with
/// `grep ir-mode-exit-refused | sort | uniq -c`. Compile time only; nothing
/// counted in a static (`process_global_statics_ratchet`).
#[cold]
#[inline(never)]
pub(in crate::ir_lower) fn note_ir_mode_exit_shape(
    why: &'static str,
    mode: IrPollModeExits,
    method_key: &str,
) {
    if false {
        // Wave 57 (lane L2): the compiling method too, which the i56-L2
        // proposal asked for to tell the spliced loops apart.
        eprintln!(
            "[ir-mode-exit-refused] shape:{why} {} method={method_key}",
            if mode.is_method_entry() {
                "method-entry"
            } else {
                "osr-door"
            }
        );
    }
}

/// Under `CRATONVM_DBG_JITC=1`, one line per back-edge poll whose mode exit
/// was refused for a NON-structural cause (`cause` indexes
/// `ModeExitRefusal::CAUSE_NAMES`, never 0 = `shape`, which
/// [`note_ir_mode_exit_shape`] names):
/// `[ir-mode-exit-refused] cause:<name> <method-entry|osr-door> method=<key>`.
/// Interpreter round i2 wave 61, lane L2 (shape 2 (b) of the i41-L1 page:
/// the census's unattributed `not_current` refusals). Compile time only.
#[cold]
#[inline(never)]
pub(in crate::ir_lower) fn note_ir_mode_exit_cause(
    cause: usize,
    mode: IrPollModeExits,
    method_key: &str,
) {
    if false {
        let name = ModeExitRefusal::CAUSE_NAMES
            .get(cause)
            .copied()
            .unwrap_or("?");
        eprintln!(
            "[ir-mode-exit-refused] cause:{name} {} method={method_key}",
            if mode.is_method_entry() {
                "method-entry"
            } else {
                "osr-door"
            }
        );
    }
}

/// Under `CRATONVM_DBG_JITC=1`, one line per spliced loop header whose
/// back-edge poll was given a caller-chain mode exit
/// ([`Lowerer::spliced_back_edge_mode_exit_state`]):
/// `[ir-mode-exit-given] spliced-header <osr-door|method-entry> scope=<key>@<bci>
/// caller=<key>@<bci> depth=<n>`. The positive control of interpreter round
/// i2 wave 57's spliced-header poll exits (lane L2). Compile time only.
#[cold]
#[inline(never)]
pub(in crate::ir_lower) fn note_ir_spliced_header_exit_given(
    fs: &FrameState,
    mode: IrPollModeExits,
) {
    let mut depth = 0usize;
    let mut scope = Some(fs);
    while let Some(f) = scope {
        depth += 1;
        scope = f.caller.as_deref();
    }
    let caller = fs
        .caller
        .as_deref()
        .map_or_else(String::new, |c| format!("{}@{}", c.method_key, c.bci));
    eprintln!(
        "[ir-mode-exit-given] spliced-header {} scope={}@{} caller={caller} depth={depth}",
        if mode.is_method_entry() {
            "method-entry"
        } else {
            "osr-door"
        },
        fs.method_key,
        fs.bci,
    );
}

impl ModeExitRefusal {
    /// The counted causes, in [`Self::cause_index`] order.
    pub(in crate::ir_lower) const CAUSE_NAMES: [&'static str; 8] = [
        "shape",
        "no_header_snapshot",
        "header_phi_input",
        "foreign_phi",
        "not_current",
        "no_register_image",
        "unlocatable",
        "not_resumable",
    ];

    /// The `[ir-mode-exit-refused]` name of a non-structural refusal of a
    /// spliced loop's chain exit (interpreter round i2 wave 57, lane L2);
    /// `None` for `Shape` (named where it was decided) and `NotSought`.
    pub(in crate::ir_lower) fn spliced_header_name(self) -> Option<&'static str> {
        Some(match self {
            Self::NotSought | Self::Shape => return None,
            Self::NoHeaderSnapshot => "spliced-header-no-chain-snapshot",
            Self::HeaderPhiInput => "spliced-header-phi-input",
            Self::ForeignPhi => "spliced-header-foreign-phi",
            Self::NotCurrent => "spliced-header-not-current",
            Self::NoRegisterImage => "spliced-header-no-register-image",
            Self::Unlocatable => "spliced-header-unlocatable",
            Self::NotResumable => "spliced-header-not-resumable",
        })
    }

    /// The cause's index into [`Self::CAUSE_NAMES`]; `None` for `NotSought`.
    pub(in crate::ir_lower) fn cause_index(self) -> Option<usize> {
        Some(match self {
            Self::NotSought => return None,
            Self::Shape => 0,
            Self::NoHeaderSnapshot => 1,
            Self::HeaderPhiInput => 2,
            Self::ForeignPhi => 3,
            Self::NotCurrent => 4,
            Self::NoRegisterImage => 5,
            Self::Unlocatable => 6,
            Self::NotResumable => 7,
        })
    }
}

/// Per tier (row [`CENSUS_OSR_DOOR`] or [`CENSUS_METHOD_ENTRY`]), `[given,
/// refused by cause...]` over every unconditional back-edge poll of an
/// optimizing compile of that tier this process lowered (slot 0: the poll got
/// a mode exit; slot `1 + i`: refused for
/// [`ModeExitRefusal::CAUSE_NAMES`]`[i]`). See
/// [`Lowerer::note_mode_exit_outcome`]. One row until interpreter round i1
/// wave 18 (lane L2) added the method-entry tier.
pub(in crate::ir_lower) static IR_POLL_MODE_EXIT_CENSUS: [[std::sync::atomic::AtomicU64; 9]; 2] =
    [const { [const { std::sync::atomic::AtomicU64::new(0) }; 9] }; 2];

/// [`IR_POLL_MODE_EXIT_CENSUS`]'s row for the optimizing OSR door's compiles.
pub(in crate::ir_lower) const CENSUS_OSR_DOOR: usize = 0;
/// [`IR_POLL_MODE_EXIT_CENSUS`]'s row for method-entry compiles
/// ([`IrPollModeExits::MethodEntry`]).
pub(in crate::ir_lower) const CENSUS_METHOD_ENTRY: usize = 1;

/// One row of [`IR_POLL_MODE_EXIT_CENSUS`], as `(given, [(cause, count)])`.
pub(in crate::ir_lower) fn ir_poll_mode_exit_census_row(
    row: usize,
) -> (u64, Vec<(&'static str, u64)>) {
    let load =
        |i: usize| IR_POLL_MODE_EXIT_CENSUS[row][i].load(std::sync::atomic::Ordering::Relaxed);
    let refused = ModeExitRefusal::CAUSE_NAMES
        .iter()
        .enumerate()
        .map(|(i, &name)| (name, load(i + 1)))
        .collect();
    (load(0), refused)
}

/// The process-wide optimizing-OSR poll mode-exit census: how many
/// unconditional back-edge polls of OSR-door compiles got an exit to the
/// interpreter, and the refused ones as `(cause, count)` in
/// [`ModeExitRefusal::CAUSE_NAMES`] order. The coverage number an agent cares
/// about is `given / (given + refused)`. Process-global and monotone, counted
/// at compile time. Interpreter round i1 wave 17, lane L1.
pub fn ir_poll_mode_exit_census() -> (u64, Vec<(&'static str, u64)>) {
    ir_poll_mode_exit_census_row(CENSUS_OSR_DOOR)
}

/// [`ir_poll_mode_exit_census`] for the unconditional back-edge polls of
/// optimizing METHOD-ENTRY compiles (interpreter round i1 wave 18, lane L2).
/// A compile [`ir_entry_mode_exits_admitted`] refused seeks nothing and
/// counts nothing.
pub fn ir_entry_poll_mode_exit_census() -> (u64, Vec<(&'static str, u64)>) {
    ir_poll_mode_exit_census_row(CENSUS_METHOD_ENTRY)
}

/// Which back-edge polls of an optimizing compile may leave the body for the
/// interpreter, and on which bit of `HelperFnSafepointSlowPath`'s verdict
/// (`Lowerer::back_edge_mode_exit_state`, `Lowerer::emit_poll_mode_exit`).
/// Chosen by the IR door in `lib.rs` (`ir_tier`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IrPollModeExits {
    /// No poll reads the verdict: the codegen before interpreter round i1
    /// wave 15.
    Off,
    /// The optimizing OSR door's compile (`CompileRequest::osr_mode_exit_polls`;
    /// wave 15, lane L2). The body runs inside the OSR'd interpreter frame's
    /// activation and leaves on `SAFEPOINT_VERDICT_INNERMOST_FRAME`, the
    /// question its planless transfer sink asks of that frame.
    OsrDoor,
    /// A method-entry compile whose exit every stash sink resumes
    /// ([`ir_entry_mode_exits_admitted`]; wave 18, lane L2). The poll names
    /// its body and leaves on `SAFEPOINT_VERDICT_POLLING_BODY`, which the VM
    /// answers about that body's method; the sinks take the exit uncharged
    /// (`jvmti_events::exit_left_for_the_interpreter`).
    MethodEntry,
    /// [`Self::MethodEntry`] for the one shape a caller-held sync-direct site
    /// binds: an `ACC_SYNCHRONIZED` method whose graph can neither trap nor
    /// call ([`ir_entry_poll_mode_exits`]; interpreter round i2 wave 53, lane
    /// L2, under `CRATONVM_JIT_IR_SYNC_LEAF_NAMED_POLL_EXITS`). Its polls
    /// leave on `SAFEPOINT_VERDICT_NAMED_BODY`, which the VM sets only for a
    /// body the poll named AND no compiled caller bound
    /// (`CompiledMethod::is_bound_by_a_caller_held_site`); it seeks an exit
    /// only with a compile id (nothing could name the body without one) and
    /// puts no post-call exit (the graph has no call). Every other rule is
    /// [`Self::MethodEntry`]'s.
    MethodEntryNamedOnly,
}

impl IrPollModeExits {
    /// The verdict bit this mode's polls leave on, `None` for [`Self::Off`].
    /// Every bit is below `0x100`, so the `TEST AL, imm8` form takes them.
    pub(in crate::ir_lower) fn verdict_bit(self) -> Option<u8> {
        let bit = match self {
            Self::Off => return None,
            Self::OsrDoor => crate::SAFEPOINT_VERDICT_INNERMOST_FRAME,
            Self::MethodEntry => crate::SAFEPOINT_VERDICT_POLLING_BODY,
            Self::MethodEntryNamedOnly => crate::SAFEPOINT_VERDICT_NAMED_BODY,
        };
        u8::try_from(bit).ok()
    }

    /// A method-entry mode ([`Self::MethodEntry`] or
    /// [`Self::MethodEntryNamedOnly`]): the stash sinks rebuild the exit's
    /// frame as a new one, under the `IR_ENTRY_EXITS_*` lock rules.
    pub(in crate::ir_lower) fn is_method_entry(self) -> bool {
        matches!(self, Self::MethodEntry | Self::MethodEntryNamedOnly)
    }
}

/// The compiling method's own bytecode, handed to the lowerer so a back-edge
/// poll's mode exit can call a local DEAD at its loop header by the
/// handler-aware bytecode liveness (`regalloc::live_locals_per_pc_all`) and
/// describe it as `FrameValue::Undefined`, as the single-pass backend's
/// deopt snapshot does (`x64::deopt_stubs::build_frame_state_at`), instead of
/// refusing the exit for a value the compiled code no longer holds.
/// Interpreter round i1 wave 18, lane L2.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PollExitBytecode<'a> {
    /// The method's own code (not a splice's combined buffer); may be padded.
    pub(crate) code: &'a [u8],
    /// Its real length.
    pub(crate) code_len: usize,
    /// Parameter slots, `this` included (the liveness entry mask).
    pub(crate) num_params: usize,
    /// `max_locals`: how many 64-slot windows the liveness covers.
    pub(crate) num_locals: usize,
    /// `(start_pc, end_pc, handler_pc)` per exception-table row, so a local
    /// only a handler reads is live.
    pub(crate) handlers: &'a [(usize, usize, usize)],
}

/// gen r5w6/oomjit10: the compiling method's own bytecode, handed to the
/// dead-reference rule ([`DeadRefSlotClear`]): the decoded code for its
/// reachability walk, and what `regalloc::live_locals_per_pc_all` needs to
/// narrow every snapshot's locals to the ones bytecode liveness keeps
/// (exception edges included, so a local only a handler reads stays live).
/// Only `lib.rs`'s IR door builds one.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DeadRefClearBytecode<'a> {
    /// The method's own code, unpadded (its length is the code length).
    pub(crate) code: &'a [u8],
    /// The method descriptor, available before the compiled method is labeled.
    pub(crate) method_descriptor: &'a str,
    /// ACC_SYNCHRONIZED methods hold a monitor even without monitor bytecodes.
    pub(crate) method_synchronized: bool,
    /// Parameter slots, `this` included (the liveness entry mask).
    pub(crate) num_params: usize,
    /// `max_locals`.
    pub(crate) num_locals: usize,
    /// `(start_pc, end_pc, handler_pc)` per exception-table row.
    pub(crate) handlers: &'a [(usize, usize, usize)],
    /// t1 ARGFIX item 2 ([`ir_keep_set_splice_local_liveness_enabled`]): the
    /// bodies this compile spliced, one per method key, for narrowing a chain
    /// snapshot's scope locals by each scope's own liveness. Empty when
    /// nothing was spliced (and in every lowering but `lib.rs`'s IR door).
    pub(crate) spliced_bodies: &'a [SplicedBodyCode],
}

/// t1 ARGFIX item 2: one spliced method's interpreter bytecode, for
/// [`splice_snapshot_dead_slots`]. Built by `lib.rs` from the inline tables:
/// the UNMASKED body (`IrInlineTables::original_body_codes` when the planner
/// masked its handlers, else the relocated bytes), its exception rows
/// rebased to the body, and its parameter / local counts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SplicedBodyCode {
    /// `"class/Name.method:descriptor"`, as [`crate::ir::SpliceScopeFrame::method_key`].
    pub(crate) method_key: String,
    pub(crate) code: Vec<u8>,
    /// Parameter slots, receiver included (the liveness entry mask).
    pub(crate) num_params: usize,
    /// `max_locals`.
    pub(crate) num_locals: usize,
    /// `(start_pc, end_pc, handler_pc)`, body-relative.
    pub(crate) handlers: Vec<(usize, usize, usize)>,
}

/// t1 ARGFIX item 2 (`t1-ARGFIX-ir-owned-args-never-reach-osr-or-spliced-callers`):
/// `CRATONVM_JIT_IR_KEEP_SET_SPLICE_LOCAL_LIVENESS`, default **ON** since j27
/// wave 1 (lane FRAMES, 2026-10-28; `=0` kills; opt-in `=1` before). Under
/// the precise keep set, the locals of every scope of a chain snapshot (a
/// frame state recorded inside a splice) are narrowed by THAT scope's own
/// bytecode liveness, exception edges included, as an own-instruction
/// snapshot's are by the compiling method's: scope 0 at the instruction the
/// snapshot was taken at, every caller scope at the `invoke` it is parked in
/// (an `invoke` touches no local, so live-in there is live-out). Without it a
/// spliced body keeps every local of every scope named for its whole body --
/// a 52 MiB argument the callee dropped after `a.length` stays rooted across
/// the callee's loop. HotSpot prunes an inlined callee's locals by the
/// callee's own `MethodLiveness` and pops the caller's arguments at the
/// invoke. A deopt there resumes the scope with whatever the (possibly
/// zeroed) home holds for a local the interpreter never reads, exactly the
/// own-instruction rule. Read once per keep-set build; no static.
///
/// j27 review (why the flip is sound): the bodies are the bytes a deopt
/// resumes (`spliced_body_codes`: the unmasked/unstripped copy whenever the
/// resolver changed the spliced bytes, which `jit_bridge` records for every
/// masked or stripped body; invokes and returns stay verbatim in the combined
/// buffer, so a nested call is an `invoke` to the liveness, not a jump), with
/// the admitted handler rows; a scope pc that is no instruction start is not
/// `covered` and stays live (`local_dead_at`); a key whose `max_locals` or
/// bytes disagree is not answered; a truncated fixed point widens to all-live;
/// and the whole keep set is withheld under a debugger (the door passes no
/// bytecode), as for the own-instruction rule.
pub(crate) fn ir_keep_set_splice_local_liveness_enabled() -> bool {
    true
}

/// t1 ARGFIX item 2: one spliced body's own bytecode local liveness, exception
/// edges included -- the table [`splice_snapshot_dead_slots`] reads per scope.
/// Shared by the keep set and the owned-argument window (j27 ARGFIX2).
pub(in crate::ir_lower) fn spliced_body_liveness(
    b: &SplicedBodyCode,
) -> (Vec<u64>, Vec<bool>, usize) {
    crate::regalloc::live_locals_per_pc_all(
        &b.code,
        b.code.len(),
        b.num_params,
        &[],
        &b.handlers,
        b.num_locals,
    )
}

/// t1 ARGFIX item 2: which slots of chain snapshot `sp` are scope locals that
/// their scope's liveness calls dead: `(dead locals, dead stack entries)`,
/// parallel to `sp.locals` / `sp.stack`. `None` for a snapshot that is not a
/// chain, a malformed chain ([`crate::ir::SpliceScopeChain::split`] refuses
/// it), or when no scope can be answered -- the caller then narrows nothing.
/// A scope is answered only when its liveness is known: `bodies` by method
/// key for a spliced scope (and only when its `max_locals` agrees), `root`
/// for the compiling method (the last scope, empty key). Operand-stack
/// entries and monitors are never dead here.
pub(in crate::ir_lower) fn splice_snapshot_dead_slots(
    sp: &crate::ir::SafepointSnapshot,
    bodies: &HashMap<&str, (&(Vec<u64>, Vec<bool>, usize), usize)>,
    root: Option<(&(Vec<u64>, Vec<bool>, usize), usize)>,
) -> Option<(Vec<bool>, Vec<bool>)> {
    let chain = sp.splice_scope.as_deref()?;
    let local_ix: Vec<usize> = (0..sp.locals.len()).collect();
    let stack_ix: Vec<usize> = (0..sp.stack.len()).collect();
    let scopes = chain.split(&local_ix, &stack_ix)?;
    let mut dead_locals = vec![false; sp.locals.len()];
    let mut dead_stack = vec![false; sp.stack.len()];
    let mut any = false;
    for (i, (frame, (scope_locals, _))) in chain.scopes.iter().zip(scopes.iter()).enumerate() {
        let answer = if frame.method_key.is_empty() {
            root
        } else {
            bodies
                .get(frame.method_key.as_str())
                .copied()
                .filter(|&(_, n)| u32::try_from(n).ok() == Some(frame.max_locals))
        };
        let Some((live, num_locals)) = answer else {
            continue;
        };
        let Ok(bci) = usize::try_from(frame.bci) else {
            continue;
        };
        for (slot, &ix) in scope_locals.iter().enumerate() {
            if local_dead_at(live, num_locals, bci, slot) {
                any = true;
                if i == 0 {
                    dead_locals[ix] = true;
                } else {
                    dead_stack[ix] = true;
                }
            }
        }
    }
    any.then_some((dead_locals, dead_stack))
}

/// Is JVM local `slot` dead at `bci` by `live`, the `(rows, covered, words)`
/// of `regalloc::live_locals_per_pc_all` over a method of `num_locals` locals?
/// `false` for a `bci` no block covers (its row is a default, not an answer)
/// and for a slot past `num_locals`. Interpreter round i1 wave 18, lane L2.
pub(crate) fn local_dead_at(
    live: &(Vec<u64>, Vec<bool>, usize),
    num_locals: usize,
    bci: usize,
    slot: usize,
) -> bool {
    let (rows, covered, words) = live;
    if slot >= num_locals || !covered.get(bci).copied().unwrap_or(false) {
        return false;
    }
    let word = bci
        .checked_mul(*words)
        .and_then(|at| at.checked_add(slot / 64))
        .and_then(|at| rows.get(at));
    word.is_some_and(|word| word & (1u64 << (slot % 64)) == 0)
}

/// JVM local slots containing reference parameters, including this when the
/// descriptor's parameter slots leave room for the receiver. None means the
/// method label or slot count was not structurally consistent, so callers must
/// conservatively retain service copies.
pub(crate) fn reference_parameter_slots(descriptor: &str, num_params: usize) -> Option<Vec<usize>> {
    let tags: Vec<u8> = crate::DescriptorParamIter::new(descriptor).collect();
    let descriptor_slots = tags.iter().try_fold(0usize, |n, tag| {
        n.checked_add(if matches!(tag, b'J' | b'D') { 2 } else { 1 })
    })?;
    let receiver_slots = num_params.checked_sub(descriptor_slots)?;
    if receiver_slots > 1 {
        return None;
    }
    let has_receiver = receiver_slots == 1;
    let mut slot = usize::from(has_receiver);
    let mut references = Vec::new();
    if has_receiver {
        references.push(0);
    }
    for tag in tags {
        if matches!(tag, b'L' | b'[') {
            references.push(slot);
        }
        slot = slot.checked_add(if matches!(tag, b'J' | b'D') { 2 } else { 1 })?;
    }
    (slot == num_params).then_some(references)
}

/// Kill switch for [`IrPollModeExits::MethodEntry`], like the single-pass
/// `x64::safepoint::METHOD_ENTRY_MODE_EXITS_ENABLED`: `false` restores the
/// wave-17 codegen of every optimizing method-entry body exactly.
/// Interpreter round i1 wave 18, lane L2.
pub(in crate::ir_lower) const IR_METHOD_ENTRY_MODE_EXITS_ENABLED: bool = true;

/// May the back-edge polls of an optimizing METHOD-ENTRY body of this method
/// leave for the interpreter (interpreter round i1 wave 18, lane L2;
/// `i9-L5-jvmti-frames-already-compiled-finish-compiled`, IR item (c))?
///
/// Such an exit is resumed as a new interpreter frame by whichever stash sink
/// the body's sentinel reaches: the interpreter door's
/// (`jit_bridge::resume_or_despeculate_stash`, and the first-call door in
/// `interpreter.rs`) or a compiled caller's callee service
/// (`helpers::try_resume_trapped_callee`). An exit a sink could NOT resume
/// would re-run the method from entry and replay what the body committed, so
/// this admits exactly the methods every one of them resumes whatever the
/// artifact's `can_deopt_resume`:
///
/// * `deopt_sink_resume_enabled()` and neither `ACC_SYNCHRONIZED` nor a
///   `monitorenter` / `monitorexit` in the bytecode — the VM's
///   `sink_precise_resume_allowed`, the door's arm for an optimizing body
///   (whose `can_deopt_resume` is set only for monitor bodies and scalar
///   replacement), with the same `bytecode_holds_monitor` walk;
/// * not `ACC_SYNCHRONIZED` is also `try_resume_trapped_callee`'s refusal,
///   and keeps a synchronized body free of trap points, which the VM's
///   `caller_held_body_is_closed` / `wrapped_body_attempt_never_reruns` read.
///   (Wave 18's reasons; wave 22 below admits a synchronized method, and says
///   why neither holds for it.)
///
/// The frame itself must still pass `osr_exit::guard_exit_point_resumable`
/// (`Lowerer::back_edge_mode_exit_state`). `code_len` bounds the walk; the
/// buffer may carry padding after it.
///
/// Interpreter round i1 wave 19, lane L2: a method with a `monitorenter` /
/// `monitorexit` (not `ACC_SYNCHRONIZED`) is admitted too, on the sinks'
/// OTHER arm: `can_deopt_resume && deopt_real_enabled()` at the interpreter
/// door, `can_deopt_resume` at the first-call door, and the callee service,
/// which refuses only `ACC_SYNCHRONIZED`. The lowering sets `can_deopt_resume`
/// for such a body whenever it gave a poll an exit (`lower_inner_attempt`,
/// the monitor-block vouch), and an exit is given only at a header whose
/// snapshot holds no monitor, so the resumed frame owns no lock. Kill switch:
/// `IR_MONITOR_BLOCK_MODE_EXITS_ENABLED`. Wave 21 (lane L2): a header inside
/// the block gets an exit too, its frame naming the held locks
/// (`relock == false`), which the rebuilt frame keeps and its own
/// `monitorexit` releases (`IR_ENTRY_EXITS_HOLDING_A_LOCK_ENABLED`).
///
/// Interpreter round i1 wave 22, lane L2
/// (`i18-L2-ir-entry-exits-refused-for-monitor-methods`, remains (a)): an
/// `ACC_SYNCHRONIZED` method is admitted too, on the same `can_deopt_resume`
/// arm (`deopt_real_enabled()`). The two reasons it was refused do not hold
/// for a body that has a poll exit:
///
/// * no dispatch helper CALLs a synchronized body raw — every raw door refuses
///   a `requires_wrapped_entry` artifact (`try_jit_compile_callee`,
///   `jit_invoke_dispatch`'s cache arm, cycle cells) — so its stash never
///   reaches `try_resume_trapped_callee`. It reaches the VM doors that hold
///   the method monitor around the body (`execute_jit_call`,
///   `execute_jit_call_decoded`, `execute_wrapped_body_door_locked`, and
///   `execute()`'s first-call door), which resume it on `can_deopt_resume`
///   and hand the monitor to the resumed frame
///   (`hand_monitor_to_resumed_frame`) or hold it until that frame returns.
///   The IR door (`lib.rs`) sets `can_deopt_resume` for every synchronized
///   body with a box, which a poll exit is;
/// * the one raw CALL of a synchronized body is a compiled caller's
///   caller-held sync-direct site, which binds only a body with no trap point
///   at all (`jit_bridge::caller_held_body_is_closed`), so it never binds a
///   body with an exit. To keep that binding for the bodies it serves today,
///   [`ir_entry_poll_mode_exits`] gives no exit to a synchronized body whose
///   graph cannot trap or call ([`graph_cannot_deopt`]): that is the only
///   shape the caller-held site can bind, and the only one that loses the
///   exit. Kill switch: `IR_SYNCHRONIZED_METHOD_MODE_EXITS_ENABLED`.
pub fn ir_entry_mode_exits_admitted(code: &[u8], code_len: usize, is_synchronized: bool) -> bool {
    if !IR_METHOD_ENTRY_MODE_EXITS_ENABLED || code_len > code.len() {
        return false;
    }
    if is_synchronized {
        return IR_SYNCHRONIZED_METHOD_MODE_EXITS_ENABLED && crate::deopt_real_enabled();
    }
    if crate::bytecode_holds_monitor(code, code_len) {
        IR_MONITOR_BLOCK_MODE_EXITS_ENABLED && crate::deopt_real_enabled()
    } else {
        crate::deopt_sink_resume_enabled()
    }
}

/// Kill switch for the `ACC_SYNCHRONIZED` half of
/// [`ir_entry_mode_exits_admitted`] (interpreter round i1 wave 22, lane L2):
/// `false` restores the wave-21 admission, which refused every synchronized
/// method, so its optimizing method-entry body kept verdict-blind polls.
pub(in crate::ir_lower) const IR_SYNCHRONIZED_METHOD_MODE_EXITS_ENABLED: bool = true;

/// Does any scope of `fs` name a scalar-replaced object (`VirtualObject` /
/// `VirtualObjectRef`) in a local or an operand? Census only (the
/// `ir-exit-cost-census` line, nio21 wave 4 L3).
pub(in crate::ir_lower) fn frame_state_names_a_virtual_object(fs: &FrameState) -> bool {
    let mut scope = Some(fs);
    while let Some(s) = scope {
        if s.locals.iter().chain(s.stack.iter()).any(|v| {
            matches!(
                v,
                FrameValue::VirtualObject(_) | FrameValue::VirtualObjectRef(_)
            )
        }) {
            return true;
        }
        scope = s.caller.as_deref();
    }
    false
}

/// Does `values` (a snapshot's locals or operands) read `NO_NODE` in a slot
/// that is not `dead`, other than the upper half of a `long` / `double` the
/// slot before names (`wide`)? The rule of
/// `Lowerer::state_lost_a_value_to_the_optimizer`.
pub(in crate::ir_lower) fn snapshot_values_lost(
    values: &[NodeId],
    dead: &dyn Fn(usize) -> bool,
    wide: &dyn Fn(NodeId) -> bool,
) -> bool {
    values
        .iter()
        .enumerate()
        .any(|(i, &v)| v == NO_NODE && !dead(i) && !(i > 0 && wide(values[i - 1])))
}

/// The back-edge poll mode of an optimizing compile that is NOT the OSR
/// door's (interpreter round i1 wave 22, lane L2): [`IrPollModeExits::MethodEntry`]
/// when [`ir_entry_mode_exits_admitted`] admits the method, except for an
/// `ACC_SYNCHRONIZED` method whose graph can neither trap nor call
/// ([`graph_cannot_deopt`]), else [`IrPollModeExits::Off`].
///
/// Why the exception: a compiled caller CALLs a synchronized body directly,
/// holding its monitor itself, only when that body has no trap point at all
/// (`jit_bridge::caller_held_body_is_closed`, which reads
/// `_deopt_point_boxes`, where a poll exit lives), because that site's
/// exceptional edge treats every sentinel as the callee's exception and has
/// no way to hand the monitor to a resumed frame. A body that can trap or
/// call is refused there anyway (a guard is a trap point, a call breaks
/// `wrapped_body_attempt_never_reruns`), so its exit costs nothing; a
/// trap-free, call-free body is the one that site binds, and an exit would
/// send its callers back to the dispatch helper. The prediction errs one way
/// only: a graph that could trap but emitted no trap gets an exit and loses
/// the binding (slower, never wrong), and a graph predicted trap-free keeps a
/// verdict-blind poll, as every synchronized body did before wave 22.
///
/// `lib.rs` (`ir_tier`) calls this for every compile without
/// `CompileRequest::osr_mode_exit_polls`.
///
/// Interpreter round i2 wave 53, lane L2 (the wave-25 design of
/// `docs/feature-designs/jit-ir-optimizations.md`;
/// shape 2 (c) of
/// `docs/feature-designs/jit-ir-optimizations.md`):
/// under [`ir_sync_leaf_named_poll_exits_enabled`] the exception's shape gets
/// [`IrPollModeExits::MethodEntryNamedOnly`] instead of no exit. Its polls
/// leave only on a bit the VM never sets for a body a caller-held site bound
/// (`SAFEPOINT_VERDICT_NAMED_BODY`), and the binding marks a body before the
/// caller that `CALL`s it can run, so the site still binds it
/// (`jit_bridge::caller_held_body_is_closed`, `named_poll_exits_only`) and
/// never sees it leave; a body no caller bound (entered from the interpreter
/// or a door) leaves like every other synchronized method-entry body.
pub(crate) fn ir_entry_poll_mode_exits(
    code: &[u8],
    code_len: usize,
    is_synchronized: bool,
    graph: &Graph,
) -> IrPollModeExits {
    if !ir_entry_mode_exits_admitted(code, code_len, is_synchronized) {
        return IrPollModeExits::Off;
    }
    if is_synchronized && graph_cannot_deopt(graph) {
        return if ir_sync_leaf_named_poll_exits_enabled() {
            IrPollModeExits::MethodEntryNamedOnly
        } else {
            IrPollModeExits::Off
        };
    }
    IrPollModeExits::MethodEntry
}

/// `CRATONVM_JIT_IR_SYNC_LEAF_NAMED_POLL_EXITS` -- interpreter round i2 wave
/// 53, lane L2: an optimizing method-entry body of an `ACC_SYNCHRONIZED`
/// method whose graph can neither trap nor call gets back-edge poll exits on
/// `SAFEPOINT_VERDICT_NAMED_BODY` ([`IrPollModeExits::MethodEntryNamedOnly`])
/// instead of verdict-blind polls. Off restores the wave-22 exception exactly
/// (no exit, no box, the body byte-identical). Read once per compile; not
/// cached in a static (`process_global_statics_ratchet`). **Default ON**
/// since the wave-53 lane's last commit (revert that commit to turn it off by
/// default); `=0` is the kill switch.
pub(crate) fn ir_sync_leaf_named_poll_exits_enabled() -> bool {
    true
}

/// JIT round 21 lane stress: the forced-arrival period a value of
/// `CRATONVM_DBG_JIT_DEOPT_A_LOT` / `CRATONVM_DBG_GC_AT_BACK_EDGE` asks for,
/// and its optional seed: `<N>` or `<N>:<seed>` (decimal). An off word (`0`,
/// `false`, `off`, `no`, empty) is `None`; any other on word (`1` excepted,
/// which is a period of one) is [`POLL_STRESS_DEFAULT_PERIOD`] with no seed,
/// so `=on` / `=true` mean "the default rate".
pub(crate) fn parse_poll_stress(value: &str) -> Option<(u32, Option<u32>)> {
    let v = value.trim();
    if v.is_empty() || cratonvm_types::flags::value_is_off(v) {
        return None;
    }
    let (n, seed) = match v.split_once(':') {
        Some((n, seed)) => (n.trim(), seed.trim().parse::<u32>().ok()),
        None => (v, None),
    };
    match n.parse::<u32>() {
        Ok(0) => None,
        Ok(period) => Some((period, seed)),
        Err(_) => Some((POLL_STRESS_DEFAULT_PERIOD, None)),
    }
}

/// Is the poll-stress switch `name` (`CRATONVM_DBG_JIT_DEOPT_A_LOT` or
/// `CRATONVM_DBG_GC_AT_BACK_EDGE`) set to an on value ([`parse_poll_stress`])?
/// For the VM's safepoint slow path, which acts on a forced arrival only for
/// the switches that are set. Not cached (no static); the slow path asks it
/// only on an arrival that found the pause flag clear.
pub fn dbg_poll_stress_switch_on(name: &str) -> bool {
    cratonvm_types::flags::runtime_var(name)
        .ok()
        .and_then(|v| parse_poll_stress(&v))
        .is_some()
}

/// The forced-arrival period of a stress switch set to a non-numeric on word.
/// A prime, so sites with different trip counts drift through each other's
/// phases.
pub(crate) const POLL_STRESS_DEFAULT_PERIOD: u32 = 997;

/// JIT round 21 lane stress (FUZZ21-1 / FUZZ21-7): `(period, seed)` of the
/// back-edge poll stress, `(0, None)` off. Both switches share the one forced
/// arrival per site (the VM acts on each switch that is set), so its period is
/// the smaller of the two that are set, and the seed the first one given.
/// Read once per lowering, never on a hot path; not cached in a static
/// (`process_global_statics_ratchet`).
pub(crate) fn dbg_poll_stress() -> (u32, Option<u32>) {
    let read = |name: &str| {
        cratonvm_types::flags::runtime_var(name)
            .ok()
            .and_then(|v| parse_poll_stress(&v))
    };
    match (
        read("CRATONVM_DBG_JIT_DEOPT_A_LOT"),
        read("CRATONVM_DBG_GC_AT_BACK_EDGE"),
    ) {
        (None, None) => (0, None),
        (Some(a), None) | (None, Some(a)) => a,
        (Some((pa, sa)), Some((pb, sb))) => (pa.min(pb), sa.or(sb)),
    }
}

/// The first countdown of the `site`-th stressed poll of a body: the period
/// itself without a seed (every site's first forced arrival is its Nth), or a
/// seeded value in `1..=period` that differs from site to site, so the sites
/// of one loop nest do not all fire on the same iteration.
pub(crate) fn poll_stress_initial_countdown(period: u32, seed: Option<u32>, site: usize) -> u32 {
    let period = period.max(1);
    match seed {
        None => period,
        Some(seed) => {
            // Cast: a site index; only its low bits matter to the mix.
            let mixed = seed.wrapping_add((site as u32).wrapping_mul(0x9E37_79B9));
            1 + mixed % period
        }
    }
}

/// JIT round 21 lane stress (FUZZ21-3): `CRATONVM_DBG_DEAD_HOME_POISON`.
/// Read once per lowering (and by the deopt detector); not cached in a static.
pub(crate) fn dbg_dead_home_poison_enabled() -> bool {
    false
}

/// [`ir_artifact_hides_an_assigned_local`], asking the INNER scopes of a caller
/// chain as well when `spliced` gives the combined buffer the build walked and
/// its body table (round 13 wave 13, lane jitfix;
/// `r13w12-snapliv-debugger-discard-check-ignores-chain-inner-scopes`).
///
/// Since round 13 wave 3 (`CRATONVM_JIT_IR_SPLICE_FRAME_STATES`) an IR point
/// can carry a chain whose inner frames are spliced callees'. Each is asked
/// exactly as the outermost one, against its own method's bytecode
/// (`combined[base .. base + code_len]`, no handlers: a spliced body has none,
/// the inline resolver refuses an exception table) at its own `bci`, with its
/// argument slots as the entry mask. A scope whose body the table does not
/// name, or whose range is past the buffer, answers `true` (fails closed, as a
/// short `code` does). `None` asks the outermost scope only, as before.
pub(crate) fn ir_artifact_hides_an_assigned_local_scoped(
    cm: &CompiledMethod,
    bytecode: PollExitBytecode<'_>,
    spliced: Option<(&[u8], &crate::ir::IrInlineFrameSites)>,
) -> bool {
    if bytecode.code_len > bytecode.code.len() {
        return true;
    }
    let mut assigned: Option<(Vec<u64>, Vec<bool>, usize)> = None;
    // Per spliced body `base`: its table and `max_locals`.
    let mut callee_assigned: HashMap<usize, ((Vec<u64>, Vec<bool>, usize), usize)> = HashMap::new();
    let points = cm
        .deopt_points
        .iter()
        .chain(cm._deopt_point_boxes.iter().map(|point| &**point));
    for point in points {
        let mut frame = &point.frame_state;
        while let Some(caller) = frame.caller.as_deref() {
            // `frame` is an inner scope: a spliced callee's.
            if let Some((combined, bodies)) = spliced {
                if frame
                    .locals
                    .iter()
                    .any(|value| matches!(value, FrameValue::Undefined))
                {
                    let Some((base, len, arg_slots, max_locals)) =
                        bodies.body_for_key(&frame.method_key)
                    else {
                        return true;
                    };
                    // From `base` to the buffer's end, so a decoder that looks
                    // past the body's last instruction reads the buffer's own
                    // padding; `len` bounds the analysis.
                    let Some(body) = combined.get(base..).filter(|b| b.len() >= len) else {
                        return true;
                    };
                    let (table, width) = callee_assigned.entry(base).or_insert_with(|| {
                        (
                            crate::regalloc::assigned_locals_per_pc_all(
                                body,
                                len,
                                0,
                                arg_slots,
                                &[],
                                max_locals,
                            ),
                            max_locals,
                        )
                    });
                    if frame_hides_an_assigned_local(table, *width, frame) {
                        return true;
                    }
                }
            }
            frame = caller;
        }
        if !frame
            .locals
            .iter()
            .any(|value| matches!(value, FrameValue::Undefined))
        {
            continue;
        }
        let table = assigned.get_or_insert_with(|| {
            crate::regalloc::assigned_locals_per_pc_all(
                bytecode.code,
                bytecode.code_len,
                bytecode.num_params,
                &[],
                bytecode.handlers,
                bytecode.num_locals,
            )
        });
        if frame_hides_an_assigned_local(table, bytecode.num_locals, frame) {
            return true;
        }
    }
    false
}

/// [`ir_artifact_hides_an_assigned_local`] for one frame: is some local that
/// `assigned` (the `(rows, covered, words)` of
/// `regalloc::assigned_locals_per_pc_all`) calls definitely assigned at
/// `frame.bci` published `Undefined`, other than the upper half of a two-slot
/// value? `false` at a bci no reachable block covers (no answer there).
pub(in crate::ir_lower) fn frame_hides_an_assigned_local(
    assigned: &(Vec<u64>, Vec<bool>, usize),
    num_locals: usize,
    frame: &FrameState,
) -> bool {
    let (rows, covered, words) = assigned;
    let bci = frame.bci as usize;
    if !covered.get(bci).copied().unwrap_or(false) {
        return false;
    }
    let mut upper_half = false;
    for (slot, value) in frame.locals.iter().enumerate().take(num_locals) {
        let skip = upper_half;
        upper_half = matches!(
            value,
            FrameValue::Long(_)
                | FrameValue::Double(_)
                | FrameValue::RegisterLong(_)
                | FrameValue::XmmDouble(_)
                | FrameValue::StackSlotLong(_)
                | FrameValue::StackSlotDouble(_)
        );
        if skip || !matches!(value, FrameValue::Undefined) {
            continue;
        }
        let word = bci
            .checked_mul(*words)
            .and_then(|at| at.checked_add(slot / 64))
            .and_then(|at| rows.get(at))
            .copied()
            .unwrap_or(0);
        if word & (1u64 << (slot % 64)) != 0 {
            return true;
        }
    }
    false
}

/// Kill switch for the monitor-block half of [`ir_entry_mode_exits_admitted`]
/// (interpreter round i1 wave 19, lane L2): `false` restores the wave-18
/// admission, which refused every method with monitor bytecode.
pub(in crate::ir_lower) const IR_MONITOR_BLOCK_MODE_EXITS_ENABLED: bool = true;

/// Kill switch for a METHOD-ENTRY back-edge exit at a loop header inside a
/// `synchronized` block (interpreter round i1 wave 21, lane L2;
/// `Lowerer::back_edge_mode_exit_state`): `false` restores the wave-19 rule,
/// under which such a header keeps a verdict-blind poll.
pub(in crate::ir_lower) const IR_ENTRY_EXITS_HOLDING_A_LOCK_ENABLED: bool = true;

/// Kill switch for an ELIDED lock level (`MonitorInfo::relock`) in a
/// METHOD-ENTRY back-edge exit's frame (interpreter round i1 wave 23, lane L2;
/// `Lowerer::back_edge_mode_exit_state`): `false` restores the wave-21 rule,
/// under which such a header keeps a verdict-blind poll.
pub(in crate::ir_lower) const IR_ENTRY_EXITS_WITH_AN_ELIDED_LOCK_ENABLED: bool = true;

/// `CRATONVM_JIT_IR_ENTRY_POLL_OUTLINE` (**default ON**; `0` restores the
/// inline `JZ .clear` shape): outline the METHOD-ENTRY poll's slow path (emit
/// it after the body and invert the test so the fast path falls through).
/// Back-edge polls outline under their own switch since round 17,
/// [`ir_back_edge_poll_outline_enabled`] (the all-polls switch,
/// `CRATONVM_JIT_IR_POLL_OUTLINE`, was deleted in round 14 wave 2).
/// Round 13 wave 1, lane callcost. Read live, so an in-process A/B sees both
/// arms.
pub(in crate::ir_lower) fn ir_entry_poll_outline_enabled() -> bool {
    true
}

/// Round 17 (CG16P-1 of `jit-r16-codegen-proposals-DONE-20261005.md`):
/// `CRATONVM_JIT_IR_BACK_EDGE_POLL_OUTLINE` outlines a loop BACK-EDGE poll's
/// slow path as [`ir_entry_poll_outline_enabled`] does the entry poll's: the
/// test becomes `JNZ <after the body>`, so the flag-clear path of every
/// iteration falls through instead of taking a `JZ` over ~50 bytes. The
/// outlined block reloads the edge's FP values (the gap that made round 13's
/// all-polls switch slower on FP loops) and keeps the mode exit. **Default
/// ON** since round 17 (`0` restores the inline `JZ` skip): `R16CodegenLoops`
/// intLoop 0.92 -> 0.69 ns with the constant-home drop, nocall and bigA a few
/// percent. Read per poll.
pub(in crate::ir_lower) fn ir_back_edge_poll_outline_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_HOME_RELOAD_FORWARD` (**default on**; `1` opts in): a
/// frame load of a word the instruction right before it stored from
/// RAX (`MOV [rbp - off], RAX; MOV r, [rbp - off]`, the def store of one node
/// and the first read of the next) becomes `MOV r, RAX`, or a `NOP` for RAX,
/// once the body is complete and nothing branches to the load
/// ([`Lowerer::apply_home_reload_forwards`]). The store stays; what goes is
/// the store-to-load forwarding latency on the value's dependency chain,
/// paid at nearly every node of a spilled loop. Off: measured no gain on
/// `HttpHeaderValidationUtilTest`'s loops (1423 loads rewritten, steady
/// ns/iter unchanged -- the store-to-load pairs were already cheap, the CPU
/// forwarding a `[rbp - off]` word), and it moves bytes the MIR byte-equality
/// tests pin. Read per compile.
///
/// `CRATONVM_JIT_IR_POLL_SLOW_PATH_DEAD_HOME_CLEAR` (**default ON**; `0`
/// restores the stores before the test): a back-edge poll zeroes the dead
/// reference homes of `Lowerer::emit_back_edge_dead_home_clears` in its slow
/// path, right before the map that would publish them, instead of on every
/// iteration. The collector reads a frame only at a GC point, and the poll's
/// slow path is this point's only way in. Read per poll.
pub(in crate::ir_lower) fn ir_home_reload_forward_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_ELIDE_UNNAMED_HOMES` (JIT round 25 item 1a; perf,
/// **default ON** since round 26, R26-FLIP; `=0` kills it): no definition-time home store for an `int` /
/// `long` value that no reachable deopt names and whose one reader is lowered
/// right behind it ([`Lowerer::plan_tail_carries`]), and the materialised
/// boolean of a compare fused into its `If` when only UNREACHABLE frame states
/// name it (`fused_cmp_unnamed`). Where `CRATONVM_JIT_IR_HOME_RELOAD_FORWARD`
/// turned the reload into a `NOP` / `MOV` and kept the store (so it could not
/// move an instruction count), this removes both. Every misprediction refuses
/// the attempt and the ladder lowers once more with every home kept. Read per
/// compile; `CRATONVM_DBG_JITC` prints `ir-home-stores:` per lowering.
pub(in crate::ir_lower) fn ir_elide_unnamed_homes_enabled() -> bool {
    // OFF under a MIR mode, for the reason `ir_carry_single_use_enabled` is:
    // the byte-equality lane compares a per-opcode arm against the tile
    // encoder, which knows nothing of an elided store.
    if isel_emit_enabled() || isel_verify_enabled() {
        return false;
    }
    // R26-FLIP (default ON, `=0` kills).
    true
}

/// `CRATONVM_JIT_IR_ELIDE_HOMES_SPILL_AT_INSPECTION` (JIT round 25 item 1a,
/// wave 3; perf, **default ON** since round 26, `=0` kills it; acts only together with
/// `CRATONVM_JIT_IR_ELIDE_UNNAMED_HOMES`): a register-resident value that a
/// frame state names at a call, allocation, monitor, helper or back-edge poll
/// no longer keeps its definition-time home store. Its register is stored
/// into its home right before each such point instead, once per dominating
/// path (`Lowerer::emit_inspection_spills`). Every frame reader keeps reading
/// homes there, so the VM is unchanged. Read per compile; `CRATONVM_DBG_JITC`'s
/// `ir-home-stores:` line counts `spills=` and `spills_deduped=`.
pub(in crate::ir_lower) fn ir_elide_homes_spill_at_inspection_enabled() -> bool {
    // R26-FLIP (default ON, `=0` kills).
    true
}

/// `CRATONVM_JIT_IR_SITE_NAMED_LOOP_VALUES` (JIT round 25 wave 6, lane W6-B;
/// perf, **default ON since j27 2026-10-28**, `=0` kills, and only together with
/// `CRATONVM_JIT_IR_ELIDE_UNNAMED_HOMES` and
/// `CRATONVM_JIT_IR_ELIDE_HOMES_SPILL_AT_INSPECTION`): the site-named homes of
/// item 1a reach the values that live across a loop.
///
/// * A loop header is no longer a hard block. A value a header's frame state
///   names may be site-named when the allocator's liveness has it live into
///   that header, or it is a phi of that header (its edge copies write its
///   register on every way in). The OSR entry already seeds a dropped home's
///   register instead of its word (`Lowerer::emit_osr_seed`).
/// * An `int` / `long` loop PHI may be site-named; its edge copies then write
///   only its register (`emit_copy_op`'s `drop_home`, the path
///   `phi_home_droppable` already takes).
/// * The inspection spills a BACK-EDGE poll owes move into the poll's slow
///   path (`Lowerer::poll_slow_spills`, inline or outlined), the only way a
///   frame can be parked and read there. The flag-clear path of every
///   iteration stores nothing, and the plan's spill-cost rule stops counting
///   back-edge blocks.
///
/// Read once per lowering attempt; `CRATONVM_DBG_JITC` prints the
/// `ir-home-why` census line per body with the switch on or off.
pub(in crate::ir_lower) fn ir_site_named_loop_values_enabled() -> bool {
    // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
    true
}

// JIT round 25 wave 6 (lane W6-B): why a definition (or phi edge) home store
// was emitted, for the `CRATONVM_DBG_JITC` `ir-home-why` census
// (`Lowerer::plan_home_keep_census`). One code per node, the first reason the
// item-1a planning would refuse the value for.
pub(in crate::ir_lower) const HK_REF: u8 = 0;
pub(in crate::ir_lower) const HK_FP: u8 = 1;
pub(in crate::ir_lower) const HK_NO_REG: u8 = 2;
pub(in crate::ir_lower) const HK_REG_FILTER: u8 = 3;
pub(in crate::ir_lower) const HK_OP: u8 = 4;
pub(in crate::ir_lower) const HK_ENTRY: u8 = 5;
pub(in crate::ir_lower) const HK_HEADER: u8 = 6;
pub(in crate::ir_lower) const HK_INSPECT: u8 = 7;
pub(in crate::ir_lower) const HK_NOT_LIVE: u8 = 8;
pub(in crate::ir_lower) const HK_RECIPE: u8 = 9;
pub(in crate::ir_lower) const HK_EXCLUDED: u8 = 10;
pub(in crate::ir_lower) const HK_PHI: u8 = 11;
pub(in crate::ir_lower) const HK_UNPLANNED: u8 = 12;
pub(in crate::ir_lower) const HK_OTHER: u8 = 13;
/// The number of census codes.
pub(in crate::ir_lower) const HK_N: usize = 14;
/// "No blocking reason recorded" in `site_named_blocked`'s reason vector.
pub(in crate::ir_lower) const HK_NONE: u8 = 255;
/// The census line's field names, indexed by code.
pub(in crate::ir_lower) const HK_NAMES: [&str; HK_N] = [
    "ref",
    "fp",
    "no_reg",
    "reg_filter",
    "op",
    "entry",
    "header",
    "inspect",
    "not_live",
    "recipe",
    "excluded",
    "phi",
    "unplanned",
    "other",
];

/// Record `code` as `v`'s blocking reason in `why` (when one is kept and none
/// was recorded yet) and mark `v` blocked in `out`. The marking half of
/// `Lowerer::site_named_blocked`.
pub(in crate::ir_lower) fn mark_site_blocked(
    out: &mut [bool],
    why: &mut Option<&mut Vec<u8>>,
    v: NodeId,
    code: u8,
) {
    if let Some(cell) = out.get_mut(v as usize) {
        *cell = true;
    }
    if let Some(w) = why.as_deref_mut() {
        if let Some(cell) = w.get_mut(v as usize) {
            if *cell == HK_NONE {
                *cell = code;
            }
        }
    }
}

/// At most this many per-value retries of one lowering (JIT round 25 item 1a,
/// wave 2): each excludes the values the previous attempt's elisions were
/// refused for; past it the ladder lowers with every home kept.
pub(in crate::ir_lower) const ELIDE_VALUE_RETRIES_MAX: usize = 3;

/// The per-value retry state of the compile lowering on this thread: the
/// values an attempt refused an elision of ([`Lowerer::note_elide_refusal`]),
/// and the values the compile's later attempts must not elide.
#[derive(Default)]
pub(in crate::ir_lower) struct ElideRetrySets {
    pub(in crate::ir_lower) refused: Vec<NodeId>,
    pub(in crate::ir_lower) excluded: Vec<NodeId>,
}

thread_local! {
    /// [`ElideRetrySets`] of this thread's compile. Per thread and per
    /// compile, scoped by [`ElideRetryScope`]: compile scratch, not
    /// compatibility state. (One static for both sets: JIT r25 wave 8, W8-2.)
    pub(in crate::ir_lower) static ELIDE_RETRY: std::cell::RefCell<ElideRetrySets> = const {
        std::cell::RefCell::new(ElideRetrySets {
            refused: Vec::new(),
            excluded: Vec::new(),
        })
    };
}

/// The per-value retry scope of one compile's lowering ladder: starts with
/// nothing refused or excluded, and restores whatever an enclosing compile on
/// the same thread had on drop.
pub(in crate::ir_lower) struct ElideRetryScope {
    pub(in crate::ir_lower) saved: ElideRetrySets,
}

impl ElideRetryScope {
    pub(in crate::ir_lower) fn enter() -> ElideRetryScope {
        ElideRetryScope {
            saved: ELIDE_RETRY.with(|r| std::mem::take(&mut *r.borrow_mut())),
        }
    }

    /// Move the refused values into the exclusion set. `false` when nothing
    /// new was refused, so another attempt would refuse the same way.
    pub(in crate::ir_lower) fn exclude_refused(&self) -> bool {
        ELIDE_RETRY.with(|r| {
            let mut sets = r.borrow_mut();
            let refused = std::mem::take(&mut sets.refused);
            let before = sets.excluded.len();
            for id in refused {
                if !sets.excluded.contains(&id) {
                    sets.excluded.push(id);
                }
            }
            sets.excluded.len() > before
        })
    }

    pub(in crate::ir_lower) fn excluded_len(&self) -> usize {
        ELIDE_RETRY.with(|r| r.borrow().excluded.len())
    }
}

impl Drop for ElideRetryScope {
    fn drop(&mut self) {
        let saved = std::mem::take(&mut self.saved);
        ELIDE_RETRY.with(|r| *r.borrow_mut() = saved);
    }
}

/// Record `node` as refused by this thread's lowering attempt.
pub(in crate::ir_lower) fn note_elide_refused_value(node: NodeId) {
    ELIDE_RETRY.with(|r| r.borrow_mut().refused.push(node));
}

/// JIT round 25 item 1a (wave 2): the allocator's liveness facts
/// [`Lowerer::site_live_at`] reads, copied out of the `LiveModel` the
/// residency plan was made from (which moves on) for the values the plan made
/// resident. Positions are the model's linear emission positions.
pub(in crate::ir_lower) struct SiteLive {
    pub(in crate::ir_lower) pos_of: Vec<Option<usize>>,
    pub(in crate::ir_lower) span: Vec<(usize, usize)>,
    pub(in crate::ir_lower) block_of_pos: Vec<usize>,
    /// Per resident `int` / `long` value: its sorted read positions and,
    /// per block, whether it is live on entry.
    pub(in crate::ir_lower) facts: HashMap<NodeId, (Vec<usize>, Vec<bool>)>,
}
