#[allow(unused_imports)]
use super::*;

/// JIT round 20 wave 21 (lane pollroot; page
/// `jr20-back-edge-poll-publishes-dead-reference-homes-20261013.md`): the
/// `Ref` values whose home nothing reads at the back-edge poll of
/// `back_edge_block` before the value is written again, as `(node, colour)`.
/// [`Lowerer::emit_back_edge_dead_home_clears`] zeroes their homes before the
/// poll's flag test, so the poll's map, the frame block and the band scan stop
/// rooting what the program no longer holds -- HotSpot's oop map at a
/// back-edge poll is precise, and a stale strong root there changes when a
/// weak / soft / phantom referent is cleared and when an
/// `OutOfMemoryError` is thrown.
///
/// `general == false` is [`referent_loads_dead_at_back_edge`]: the candidates
/// are the spliced `Reference.referent` reads (`Graph::referent_load_pcs`) and
/// the `Ref` value phis that merge one. `general == true`
/// (`CRATONVM_JIT_IR_BACK_EDGE_DEAD_HOME_CLEAR`) adds every `Ref` result of a
/// field / static / array-element load, a call, an allocation and a
/// `checkcast` -- ops whose own lowering writes the home at the node's
/// scheduled position (`alloc_slot` at emission) -- and every `Ref` value phi
/// whose edge copies all run before the poll (no phi of a block this latch
/// branches to: a header's or an exit's copy runs after the poll). Constants,
/// OSR locals, caught exceptions and the other `Ref` producers are left
/// alone. Every rule below is the referent rule of round 20 wave 20,
/// unchanged, plus the exit rule; the answer is empty (zero nothing) whenever
/// a fact cannot be established.
///
/// A candidate `V` (a phi's block is its merge's) qualifies only when:
///
/// * **the poll's position is known** ([`back_edge_poll_pos`]);
/// * **this is a back edge of a loop whose header dominates `V`**: every
///   successor at or before the block dominates it (a reducible back edge),
///   and dominates `V`'s block. A path from the poll that re-enters a header
///   runs `V` again before any point `V` dominates (first-arrival argument:
///   entry -> header avoids `V`'s block, so a suffix that also avoided `V`
///   would make a path to a reader of `V` that never ran `V`), so no node and
///   no frame state that names `V` (a deopt, a rethrow pad, a later mode
///   exit) reads the zeroed word first. What that leaves is the reads AT the
///   latch: a header phi's incoming edge (its copy, a header mode exit), and
///   the terminator, which lowers after the poll together with any compare it
///   fuses -- `V` feeds none of them (it may feed a value phi only when that
///   phi is itself a candidate, whose copies all run before the poll);
/// * **the exit rule** (wave 21): a latch whose branch may also leave the loop
///   (a successor after it) takes the path out without re-running anything.
///   An exit inside the natural loop of this back edge refuses the whole
///   answer; otherwise a `V` defined inside that loop must have no use
///   outside it (a memory phi's token edge excepted: it reads no home) and no
///   snapshot naming it at all. A `V` outside that loop (dominated by the
///   header, e.g. code after an inner loop in an outer body) needs no such
///   check: every path to the latch through its header avoids `V`'s block;
/// * **no other live value shares the word**: no other node of `V`'s colour
///   has a colouring range covering the poll's position. `V`'s own range is
///   not asked: by default a memory-token use counts as a use, so a load's
///   range runs to the header memory phi's edge although no code reads the
///   home there;
/// * `V` is no pinned promoted / scalar-replaced value
///   (`SlotPlan::sr_field_values`, written back by deopt pads), and no value
///   whose colouring range chain snapshots widened.
///
/// JIT round 20 wave 21 follow-up (lane pollroot; page
/// `jr20-back-edge-poll-keeps-dead-header-phi-and-exit-latch-homes-FIXED-20261013.md`),
/// general mode only:
///
/// * **a header phi `P` of this latch's loop is a candidate too.** Its home is
///   written by this latch's edge copy, which runs AFTER the poll
///   (`emit_phi_copies` follows the poll on both latch shapes), so at the poll
///   it still holds the value the loop has just finished with. Nothing reads
///   it before that copy when the rules above hold for `P`: the copy writes
///   `P` and reads only its source; no other value phi may read `P` (a phi of
///   a latch successor is never trusted, so a header swap `a <- b, b <- a` or
///   `P <- P` keeps both homes); the header mode exit and the promoted-cell
///   write-backs name `P`'s EDGE source, never `P`; the terminator is checked
///   as for any value. A promoted header's phis are left alone. The lowerer
///   additionally skips a header phi that owns a register or is split-managed
///   ([`Lowerer::emit_back_edge_dead_home_clears`]): a coalesced back-edge
///   source shares the phi's register, so only a home-only phi is zeroed;
/// * **the exit rule's snapshot half is precise** when `exit_reach` is given:
///   instead of "no snapshot names `V` at all", `V`'s colour must be absent
///   from the keep union of every own pc of the code the exits reach before
///   re-entering the loop (`exit_reach(pc)`: the plain bytecode-reachability
///   union the allocation sites use, `DeadRefSlotClear::plain_reach_kept`,
///   liveness-narrowed locals, splices and per-copy snapshots included), and
///   no snapshot a node of that code claims may name `V`. Any pc the union
///   cannot answer for (or none at all) falls back to the coarse rule.
pub(in crate::ir_lower) fn back_edge_dead_ref_homes(
    graph: &Graph,
    schedule: &Schedule,
    plan: &SlotPlan,
    back_edge_block: usize,
    general: bool,
) -> Vec<(usize, u32)> {
    back_edge_dead_ref_homes_with(graph, schedule, plan, back_edge_block, general, None)
}

/// [`back_edge_dead_ref_homes`] with the exit rule's keep union (see there).
/// `exit_reach` is consulted in general mode only.
pub(in crate::ir_lower) fn back_edge_dead_ref_homes_with(
    graph: &Graph,
    schedule: &Schedule,
    plan: &SlotPlan,
    back_edge_block: usize,
    general: bool,
    exit_reach: Option<&mut dyn FnMut(usize) -> Option<Vec<u64>>>,
) -> Vec<(usize, u32)> {
    let mut out = Vec::new();
    if !general && graph.referent_load_pcs.is_empty() {
        return out;
    }
    let Some(block) = schedule.blocks.get(back_edge_block) else {
        return out;
    };
    let Some(poll_pos) = back_edge_poll_pos(schedule, plan, back_edge_block) else {
        return out;
    };
    let headers: Vec<usize> = block
        .successors
        .iter()
        .copied()
        .filter(|&s| s <= back_edge_block)
        .collect();
    if headers.is_empty()
        || !headers
            .iter()
            .all(|&h| schedule.dom.dominates(h, back_edge_block))
    {
        return out;
    }
    let nb = schedule.blocks.len();
    let n = graph.nodes.len();
    // The successors this latch may leave the loop for (laid out after it).
    let exits: Vec<usize> = block
        .successors
        .iter()
        .copied()
        .filter(|s| !headers.contains(s))
        .collect();
    // The natural loop of this back edge: the headers, and every block that
    // reaches the latch without passing one.
    let mut in_loop = vec![false; nb];
    for &h in &headers {
        if let Some(cell) = in_loop.get_mut(h) {
            *cell = true;
        }
    }
    let mut work = vec![back_edge_block];
    while let Some(b) = work.pop() {
        if in_loop.get(b).copied() != Some(false) {
            continue;
        }
        in_loop[b] = true;
        work.extend(schedule.blocks[b].predecessors.iter().copied());
    }
    if exits
        .iter()
        .any(|&x| in_loop.get(x).copied().unwrap_or(true))
    {
        return out;
    }
    let ranges = plan.colouring_ranges();
    // A node without a range is taken to be live everywhere.
    let covers = |id: usize| -> bool {
        match ranges.get(id).copied().flatten() {
            Some(r) => r.lo <= poll_pos && poll_pos <= r.hi,
            None => true,
        }
    };
    // The block a value is defined in: a phi's merge block, else its own.
    let block_of_merge: HashMap<NodeId, usize> = schedule
        .blocks
        .iter()
        .enumerate()
        .map(|(b, blk)| (blk.ctrl, b))
        .collect();
    let block_of = |id: usize| -> Option<usize> {
        let node = graph.nodes.get(id)?;
        let b = if matches!(node.op, Op::Phi) {
            let merge = *node.inputs.as_slice().first()?;
            *block_of_merge.get(&merge)?
        } else {
            *schedule.node_to_block.get(id)?
        };
        (b < nb).then_some(b)
    };
    // A MEMORY phi's incoming edge is not a read: the builder threads every
    // load into the memory chain, so the header's memory phi always names a
    // load, and no copy or frame state reads that edge.
    let value_phi = |node: &Node| matches!(node.op, Op::Phi) && node.ty != IrType::Memory;
    // A phi of a block this latch branches to is written by a copy that runs
    // AFTER the poll (a header's, an exit's): never a candidate, and never a
    // reader whose copy may be trusted to run before it.
    let copied_after_poll = |id: usize| -> bool {
        match block_of(id) {
            Some(b) => block.successors.contains(&b),
            None => true,
        }
    };
    // A phi of one of this latch's headers (follow-up): written by this
    // latch's own edge copy after the poll. Not a promoted header's.
    let header_phi = |id: usize| -> bool {
        block_of(id).is_some_and(|b| {
            headers.contains(&b)
                && !graph
                    .nodes
                    .get(schedule.blocks[b].ctrl as usize)
                    .and_then(|h| h.bytecode_pc)
                    .is_some_and(|pc| graph.promoted_cell_header_bcis.contains(&pc))
        })
    };
    let referent_read = |node: &Node| {
        matches!(node.op, Op::Load(_))
            && node
                .bytecode_pc
                .is_some_and(|pc| graph.referent_load_pcs.contains(&pc))
    };
    // The candidates.
    let mut cand = vec![false; n];
    for (id, node) in graph.nodes.iter().enumerate() {
        if node.ty != IrType::Ref {
            continue;
        }
        cand[id] = referent_read(node)
            || (general
                && match node.op {
                    Op::Load(_)
                    | Op::LoadStatic { .. }
                    | Op::ArrayLoad(_)
                    | Op::Call { .. }
                    | Op::New { .. }
                    | Op::NewArray { .. }
                    | Op::MultiNewArray { .. }
                    | Op::NewCp { .. }
                    | Op::CheckCast { .. } => true,
                    Op::Phi => !copied_after_poll(id) || header_phi(id),
                    _ => false,
                });
    }
    if !cand.contains(&true) {
        return out;
    }
    // Referent mode: every `Ref` value phi that merges a candidate (a guarded
    // splice's join of its inline read with the miss arm's call),
    // transitively. A no-op in general mode, which seeded them all.
    loop {
        let mut grew = false;
        for (id, node) in graph.nodes.iter().enumerate() {
            if cand[id] || !value_phi(node) || node.ty != IrType::Ref || copied_after_poll(id) {
                continue;
            }
            if node
                .inputs
                .as_slice()
                .iter()
                .skip(1)
                .any(|&v| cand.get(v as usize).copied().unwrap_or(false))
            {
                cand[id] = true;
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    // Read by a value phi that is not itself a candidate -- a header phi's
    // latch edge (its copy at this block's end, a header mode exit), an exit
    // phi's, or anything else this walk did not prove.
    let mut phi_read = vec![false; n];
    // The terminator lowers after the poll, and may lower a compare it fuses
    // (`emit_fused_int_compare`) there too: neither may read the value.
    let mut term_read = vec![false; n];
    // The exit rule's two facts, only asked with an exit.
    let mut used_outside = vec![false; n];
    let mut named = vec![false; n];
    // The precise snapshot half (follow-up): the exit code's keep union, and
    // the values the snapshots its nodes claim name.
    let mut exit_kept: Option<Vec<u64>> = None;
    let mut named_in_r = vec![false; n];
    for (p, node) in graph.nodes.iter().enumerate() {
        if value_phi(node) && !(cand[p] && !copied_after_poll(p)) {
            for &v in node.inputs.as_slice().iter().skip(1) {
                if let Some(cell) = phi_read.get_mut(v as usize) {
                    *cell = true;
                }
            }
        }
    }
    if let Some(term) = block.terminator {
        if let Some(t) = graph.nodes.get(term as usize) {
            for &x in t.inputs.as_slice() {
                if let Some(cell) = term_read.get_mut(x as usize) {
                    *cell = true;
                }
                if let Some(xn) = graph.nodes.get(x as usize) {
                    for &y in xn.inputs.as_slice() {
                        if let Some(cell) = term_read.get_mut(y as usize) {
                            *cell = true;
                        }
                    }
                }
            }
        }
    }
    if !exits.is_empty() {
        for (u, node) in graph.nodes.iter().enumerate() {
            if matches!(node.op, Op::Phi) && node.ty == IrType::Memory {
                continue;
            }
            let (inputs, user_block) = if matches!(node.op, Op::Phi) {
                (node.inputs.as_slice().get(1..).unwrap_or(&[]), block_of(u))
            } else {
                (
                    node.inputs.as_slice(),
                    schedule.node_to_block.get(u).copied().filter(|&b| b < nb),
                )
            };
            // An unscheduled user counts as outside: nothing proves where it runs.
            let outside = match user_block {
                Some(b) => !in_loop[b],
                None => !matches!(node.op, Op::Dead),
            };
            if !outside {
                continue;
            }
            for &v in inputs {
                if let Some(cell) = used_outside.get_mut(v as usize) {
                    *cell = true;
                }
            }
        }
        for sp in &graph.safepoints {
            for v in sp
                .locals
                .iter()
                .chain(sp.stack.iter())
                .chain(sp.monitors.iter())
                .copied()
                .chain(sp.sb_virtual_nodes())
            {
                if v == NO_NODE {
                    continue;
                }
                if let Some(cell) = named.get_mut(v as usize) {
                    *cell = true;
                }
            }
        }
        if let (true, Some(reach)) = (general, exit_reach) {
            exit_kept = exit_keep_union(graph, schedule, &exits, &in_loop, reach, &mut named_in_r);
        }
    }
    let mut sr_pinned = vec![false; n];
    for &v in &plan.sr_field_values {
        if let Some(cell) = sr_pinned.get_mut(v as usize) {
            *cell = true;
        }
    }
    // How many values of each colour have a range over the poll.
    let colours = plan
        .node_color
        .iter()
        .flatten()
        .map(|&c| c as usize + 1)
        .max()
        .unwrap_or(0);
    let mut colour_live = vec![0usize; colours];
    for (m, c) in plan.node_color.iter().enumerate() {
        if let Some(c) = *c {
            if covers(m) {
                colour_live[c as usize] += 1;
            }
        }
    }
    for id in 0..n {
        if !cand[id] {
            continue;
        }
        let Some(colour) = plan.node_color.get(id).copied().flatten() else {
            continue;
        };
        let Some(home_block) = block_of(id) else {
            continue;
        };
        if !headers
            .iter()
            .all(|&h| schedule.dom.dominates(h, home_block))
        {
            continue;
        }
        if sr_pinned[id] || phi_read[id] || term_read[id] {
            continue;
        }
        if !exits.is_empty() && in_loop[home_block] {
            let named_on_exit = match exit_kept.as_ref() {
                Some(kept) => named_in_r[id] || bits_contains(kept, colour as usize),
                None => named[id],
            };
            if used_outside[id] || named_on_exit {
                continue;
            }
        }
        // A value only chain snapshots name has its colouring range widened
        // to every guard that may publish the chain: keep it out, whatever
        // the dominance argument says.
        let widened = plan
            .colour_range
            .get(id)
            .copied()
            .flatten()
            .is_some_and(|w| Some(w) != plan.range.get(id).copied().flatten());
        if widened {
            continue;
        }
        // The candidate itself is judged by the dominance argument above, not
        // by its range (see the doc comment). Every OTHER value of its colour
        // must have no range over the poll.
        let others_live = colour_live[colour as usize] - usize::from(covers(id));
        if others_live > 0 {
            continue;
        }
        out.push((id, colour));
    }
    out
}

/// JIT round 20 wave 21 follow-up (lane pollroot): the keep union of the code
/// a latch's `exits` reach before re-entering its natural loop (`in_loop`):
/// `reach(pc)` over every pc a node, terminator or block head of that code
/// carries, ORed; and, in `named_in_r`, every value a snapshot claimed by one
/// of those nodes (`Node::frame_snapshot`) names, un-narrowed. `None` (use
/// the coarse rule) when that code carries no pc, `reach` cannot answer for
/// one, or a claimed snapshot is missing.
pub(in crate::ir_lower) fn exit_keep_union(
    graph: &Graph,
    schedule: &Schedule,
    exits: &[usize],
    in_loop: &[bool],
    reach: &mut dyn FnMut(usize) -> Option<Vec<u64>>,
    named_in_r: &mut [bool],
) -> Option<Vec<u64>> {
    let nb = schedule.blocks.len();
    let mut in_r = vec![false; nb];
    let mut work: Vec<usize> = exits.to_vec();
    while let Some(b) = work.pop() {
        if b >= nb || in_r[b] || in_loop.get(b).copied().unwrap_or(true) {
            continue;
        }
        in_r[b] = true;
        work.extend(schedule.blocks[b].successors.iter().copied());
    }
    let mut pcs: Vec<usize> = Vec::new();
    let mut claimed: Vec<u32> = Vec::new();
    for (b, blk) in schedule.blocks.iter().enumerate() {
        if !in_r[b] {
            continue;
        }
        let ids = std::iter::once(blk.ctrl)
            .chain(blk.nodes.iter().copied())
            .chain(blk.terminator);
        for nid in ids {
            let Some(node) = graph.nodes.get(nid as usize) else {
                continue;
            };
            if let Some(pc) = node.bytecode_pc {
                pcs.push(pc);
            }
            if let Some(si) = node.frame_snapshot {
                claimed.push(si);
            }
        }
    }
    if pcs.is_empty() {
        return None;
    }
    pcs.sort_unstable();
    pcs.dedup();
    let mut kept: Vec<u64> = Vec::new();
    for pc in pcs {
        let bits = reach(pc)?;
        if kept.len() < bits.len() {
            kept.resize(bits.len(), 0);
        }
        bits_or(&mut kept, &bits);
    }
    claimed.sort_unstable();
    claimed.dedup();
    for si in claimed {
        let sp = graph.safepoints.get(si as usize)?;
        for &v in sp
            .locals
            .iter()
            .chain(sp.stack.iter())
            .chain(sp.monitors.iter())
        {
            if v == NO_NODE {
                continue;
            }
            if let Some(cell) = named_in_r.get_mut(v as usize) {
                *cell = true;
            }
        }
    }
    Some(kept)
}

/// JIT round 20 wave 21 (lane pollroot): an own-method bci for the
/// back-edge poll of `latch` from which every bytecode the frame can run
/// after the poll is reachable: the latch's terminator pc, else its last node
/// with a pc, else (an empty taken-edge block) its single predecessor's
/// terminator pc -- the branch. `None` otherwise (clear nothing).
pub(in crate::ir_lower) fn back_edge_poll_bci(graph: &Graph, schedule: &Schedule, latch: usize) -> Option<usize> {
    let block = schedule.blocks.get(latch)?;
    let pc_of = |id: NodeId| graph.nodes.get(id as usize).and_then(|n| n.bytecode_pc);
    if let Some(pc) = block.terminator.and_then(pc_of) {
        return Some(pc);
    }
    if let Some(pc) = block.nodes.iter().rev().find_map(|&id| pc_of(id)) {
        return Some(pc);
    }
    match block.predecessors.as_slice() {
        [pred] => schedule
            .blocks
            .get(*pred)
            .and_then(|b| b.terminator)
            .and_then(pc_of),
        _ => None,
    }
}

/// JIT round 20 wave 21 (lane pollroot): `outer[c]` = every value coloured
/// `c` is a `Ref` defined OUTSIDE the loop(s) of `headers` -- in a block some
/// header does not dominate (an outer-loop temporary or phi, a value from
/// before the loop) -- and none is a phi with a register or a split-managed
/// value (`excluded[id]`, the lowerer's residency facts). The colours the
/// dominance rule ([`back_edge_dead_ref_homes`]) cannot reach, and the only
/// ones the poll's liveness clear zeroes.
pub(in crate::ir_lower) fn outer_ref_colours(
    graph: &Graph,
    schedule: &Schedule,
    plan: &SlotPlan,
    headers: &[usize],
    excluded: &dyn Fn(usize) -> bool,
) -> Vec<bool> {
    let nb = schedule.blocks.len();
    let colours = plan
        .node_color
        .iter()
        .flatten()
        .map(|&c| c as usize + 1)
        .max()
        .unwrap_or(0);
    let block_of_merge: HashMap<NodeId, usize> = schedule
        .blocks
        .iter()
        .enumerate()
        .map(|(b, blk)| (blk.ctrl, b))
        .collect();
    let mut outer = vec![true; colours];
    for (id, c) in plan.node_color.iter().enumerate() {
        let Some(c) = *c else {
            continue;
        };
        let Some(node) = graph.nodes.get(id) else {
            outer[c as usize] = false;
            continue;
        };
        let block = if matches!(node.op, Op::Phi) {
            node.inputs
                .as_slice()
                .first()
                .and_then(|m| block_of_merge.get(m).copied())
        } else {
            schedule.node_to_block.get(id).copied().filter(|&b| b < nb)
        };
        let is_outer = node.ty == IrType::Ref
            && !excluded(id)
            && block.is_some_and(|b| !headers.iter().all(|&h| schedule.dom.dominates(h, b)));
        if !is_outer {
            outer[c as usize] = false;
        }
    }
    outer
}

/// Work budget for the liveness fixed point, in `nodes × blocks` units.
///
/// Past this the analysis is skipped and every value keeps a dedicated slot —
/// still an improvement on the old reservation (only nodes that actually
/// allocate get a slot, rather than every arena node), and the frame bound then
/// declines the method exactly as it does today. A compiler must not turn a
/// pathological graph into a pathological *compile*.
pub(in crate::ir_lower) const SLOT_PLAN_WORK_BUDGET: usize = 8_000_000;

/// Hard cap on liveness fixed-point iterations. The analysis converges in
/// loop-depth + 2 passes over a reducible CFG walked in reverse block order;
/// exceeding this means the block order is not what we think it is, and the
/// answer is discarded (everything pinned) rather than truncated — a truncated
/// may-analysis under-approximates liveness, which would alias live values.
pub(in crate::ir_lower) const SLOT_PLAN_MAX_ITERATIONS: usize = 256;

/// Resolve the block that produces control token `ctrl` by walking up control
/// inputs until a node that heads some block is reached.
///
/// Free-function form of [`Lowerer::block_of_ctrl`], which delegates here, so
/// [`plan_slots`] attributes a phi's value inputs to exactly the predecessor
/// block `emit_phi_copies` will copy them from. Every index is checked: the
/// planner runs before `verify_data_locations`, on a graph nothing has vetted.
pub(crate) fn ctrl_block_of(graph: &Graph, schedule: &Schedule, mut ctrl: NodeId) -> Option<usize> {
    for _ in 0..graph.nodes.len() {
        if ctrl == NO_NODE {
            return None;
        }
        let blk = *schedule.node_to_block.get(ctrl as usize)?;
        if blk != usize::MAX && schedule.blocks.get(blk).map(|b| b.ctrl) == Some(ctrl) {
            return Some(blk);
        }
        // Step up the control chain (first input is the control edge for
        // Proj/If/Merge-derived nodes).
        let node = graph.nodes.get(ctrl as usize)?;
        match node.inputs.first() {
            Some(&next) if next != ctrl => ctrl = next,
            _ => return None,
        }
    }
    None
}

/// Set bit `i`. Out-of-range indices are ignored rather than panicking: the
/// planner runs on a graph nothing has verified yet.
#[inline]
pub(in crate::ir_lower) fn bits_insert(bits: &mut [u64], i: usize) {
    if let Some(word) = bits.get_mut(i / 64) {
        *word |= 1u64 << (i % 64);
    }
}

/// Is bit `i` set?
#[inline]
pub(in crate::ir_lower) fn bits_contains(bits: &[u64], i: usize) -> bool {
    bits.get(i / 64)
        .is_some_and(|w| w & (1u64 << (i % 64)) != 0)
}

/// Visit every set bit, in increasing order.
#[inline]
pub(in crate::ir_lower) fn bits_for_each(bits: &[u64], mut f: impl FnMut(usize)) {
    for (w, &word) in bits.iter().enumerate() {
        let mut rest = word;
        while rest != 0 {
            let b = rest.trailing_zeros() as usize;
            f(w * 64 + b);
            rest &= rest - 1;
        }
    }
}

/// The inputs `node` names ONLY in a memory-token slot
/// (`ir::is_memory_token_slot`), as `(id, slot, exempt(id))`: every id in a
/// token slot that no other (value) slot of the same node names, once each.
/// Empty for a φ, whose lowering emits nothing (`emit_phi_copies` owns it).
/// Interpreter round i2 wave 64, lane L2: the set
/// [`Lowerer::token_read_check`] refuses reads of.
pub(in crate::ir_lower) fn token_only_inputs_in(
    node: &Node,
    exempt: impl Fn(NodeId) -> bool,
) -> Vec<(NodeId, usize, bool)> {
    let mut out: Vec<(NodeId, usize, bool)> = Vec::new();
    if matches!(node.op, Op::Phi) {
        return out;
    }
    for (slot, &inp) in node.inputs.iter().enumerate() {
        if inp == NO_NODE || !crate::ir::is_memory_token_slot(node, slot) {
            continue;
        }
        let also_a_value = node
            .inputs
            .iter()
            .enumerate()
            .any(|(s, &v)| v == inp && s != slot && !crate::ir::is_memory_token_slot(node, s));
        if also_a_value || out.iter().any(|e| e.0 == inp) {
            continue;
        }
        out.push((inp, slot, exempt(inp)));
    }
    out
}

/// [`token_only_inputs_in`] over every node of one lowering scope: an id some
/// node of the scope names in a memory-token slot, unless some node of the
/// scope names it in a value slot (then the scope legitimately reads it).
/// Once each, first slot wins. JIT round 19 wave 10 (lane tok10, i64-L2): a
/// terminator's scope is the terminator plus the compare fused into it.
pub(in crate::ir_lower) fn token_only_inputs_across(
    nodes: &[&Node],
    exempt: impl Fn(NodeId) -> bool,
) -> Vec<(NodeId, usize, bool)> {
    let mut out: Vec<(NodeId, usize, bool)> = Vec::new();
    for node in nodes {
        for entry in token_only_inputs_in(node, &exempt) {
            if out.iter().any(|e| e.0 == entry.0) {
                continue;
            }
            let read_as_value = nodes.iter().any(|n| {
                !matches!(n.op, Op::Phi)
                    && n.inputs
                        .iter()
                        .enumerate()
                        .any(|(s, &v)| v == entry.0 && !crate::ir::is_memory_token_slot(n, s))
            });
            if !read_as_value {
                out.push(entry);
            }
        }
    }
    out
}

/// Is a token-only input of the scope being lowered exempt from the token-read
/// guard? By identity (wave 64 / wave 10): its home is `Pinned`, or a
/// safepoint snapshot names it. JIT round 19 proposal wave 11, lane lv11
/// (TOK10-3, `CRATONVM_JIT_IR_LOWER_TOKEN_READ_GUARD_POINT_RANGE`): with
/// `reader_pos` given, an identity-exempt id stays exempt only while its home's
/// colouring range covers that position (endpoint-inclusive). Past the end of
/// the range a trimmed (r17 / slot-plan token) home may already hold another
/// value -- or, for a φ, the next iteration's -- so a deopt description read
/// there is stale. No range or no position: the identity answer (nothing
/// contradicts it).
pub(in crate::ir_lower) fn token_exempt_at(
    class: Option<SlotClass>,
    named: bool,
    range: Option<LiveRange>,
    reader_pos: Option<usize>,
) -> bool {
    let by_identity = matches!(class, Some(SlotClass::Pinned)) || named;
    if !by_identity {
        return false;
    }
    match (range, reader_pos) {
        (Some(r), Some(p)) => r.lo <= p && p <= r.hi,
        _ => true,
    }
}

/// LV11-4 (JIT round 19 proposal wave 12, lane lv12): may a deopt point at
/// position `pos` read value `id`'s home? Yes for a value with no home, no
/// range, or a `Pinned` home (never shared, so nothing else can have
/// overwritten it); otherwise only inside its colouring range
/// (endpoint-inclusive). With the default plans every snapshot-named value is
/// pinned, so this refuses nothing; it is the check a home trim must pass.
pub(in crate::ir_lower) fn point_home_covers(plan: &SlotPlan, id: usize, pos: usize) -> bool {
    if plan.node_color.get(id).copied().flatten().is_none() {
        return true;
    }
    if matches!(
        plan.class.get(id).copied().flatten(),
        Some(SlotClass::Pinned) | None
    ) {
        return true;
    }
    match plan.colouring_ranges().get(id).copied().flatten() {
        Some(r) => r.lo <= pos && pos <= r.hi,
        None => true,
    }
}

/// `CRATONVM_JIT_IR_DEOPT_POINT_RANGE_CHECK=1` (default on; JIT round 19 proposal
/// wave 12, lane lv12, LV11-4 of `jit-r19-lv11-proposals.md`): the TOK10-3
/// point rule for the by-bci points [`Lowerer::build_deopt_points`] resolves
/// outside every token scope, at points a resume can arrive at. Compile-time
/// only; read once per lowering (no `static`).
pub(in crate::ir_lower) fn ir_deopt_point_range_check_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_LOWER_TOKEN_READ_GUARD_POINT_RANGE=1` (default on; JIT round
/// 19 proposal wave 11, lane lv11, TOK10-3 of `jit-r19-tok10-proposals-RETIRED-20261009.md`):
/// the token-read guard's identity exemption also asks that the id's home
/// range cover the reader's position ([`token_exempt_at`]). Only meaningful
/// while the guard is on. With the default (untrimmed) plans every pinned or
/// snapshot-named home already covers each point that reads it, so the rule
/// refuses nothing more there; it is the check behind "a trim is sound" for
/// the r17 / slot-plan token trims. Read once per lowering (no `static`).
pub(in crate::ir_lower) fn ir_lower_token_read_guard_point_range_enabled() -> bool {
    true
}

/// The lowering scope that installed the token-read guard's set (JIT round 19
/// wave 10, lane tok10, i64-L2). The discriminant indexes the per-scope
/// counters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ir_lower) enum TokenScope {
    /// A data node's own arm (`lower_data_node_tracked`), since wave 64.
    Data = 0,
    /// A block terminator and the compare fused into it (`lower_terminator`).
    Term = 1,
    /// A level-2 MIR tile's encoder (`lower_data_node_through_mir`).
    Mir = 2,
}

impl TokenScope {
    pub(in crate::ir_lower) const ALL: [TokenScope; 3] = [TokenScope::Data, TokenScope::Term, TokenScope::Mir];

    pub(in crate::ir_lower) fn name(self) -> &'static str {
        match self {
            TokenScope::Data => "data",
            TokenScope::Term => "term",
            TokenScope::Mir => "mir",
        }
    }
}

/// What [`Lowerer::token_scope_enter`] hands [`Lowerer::token_scope_exit`].
pub(in crate::ir_lower) struct TokenScopeSaved {
    pub(in crate::ir_lower) set: Vec<(NodeId, usize, bool)>,
    pub(in crate::ir_lower) scope: TokenScope,
    pub(in crate::ir_lower) reader: NodeId,
}

/// Compute the liveness-based frame-slot colouring for one scheduled graph.
///
/// Total: never panics and never fails. A graph it cannot analyse (over the
/// work budget, or a fixed point that will not converge) falls back to a
/// dedicated slot per allocating value, which is what the lowerer did before
/// this existed. The result is *checked* by [`verify_slot_colouring`], not
/// trusted — the property it establishes (two values live at one point never
/// share a word) is the one whose violation is silent wrong code.
pub(in crate::ir_lower) fn plan_slots(
    graph: &Graph,
    schedule: &Schedule,
    sr_map: Option<&ScalarReplacementMap>,
) -> SlotPlan {
    plan_slots_with_token_edges(
        graph,
        schedule,
        sr_map,
        crate::regalloc::slots_token_edges_not_uses_enabled(),
    )
}

/// [`plan_slots`] with the token-edge decision passed in rather than read
/// from `CRATONVM_JIT_IR_SLOTS_TOKEN_EDGES_NOT_USES` (interpreter round i2
/// wave 64, lane L2; split out so the wave-63 token-trim census can build
/// both frame plans in one compile without a flag override).
///
/// `token_edges_not_uses` true: an input in a memory-TOKEN slot
/// (`ir::is_memory_token_slot`) is not a read of its producer's home, and
/// neither is a memory φ's incoming edge (`gather_phi_copies` copies value φs
/// only). The same two rules, spelled the same way, as
/// `regalloc::build_live_model_with_token_edges`, so the two position models
/// keep agreeing range for range when both are on. Which values want a slot,
/// and which are pinned, does not change.
///
/// Interpreter round i2 wave 65 (lane L2): reads
/// `CRATONVM_JIT_IR_SHARED_LIVENESS_CORE` (the analysis runs through
/// `regalloc::liveness_core`, the same function `build_live_model` uses) and
/// `CRATONVM_JIT_IR_SLOTS_PHI_EDGE_RANGES` (the core also applies the φ edge
/// rule, so the φ ranges agree with the allocator's; implies the core).
pub(in crate::ir_lower) fn plan_slots_with_token_edges(
    graph: &Graph,
    schedule: &Schedule,
    sr_map: Option<&ScalarReplacementMap>,
    token_edges_not_uses: bool,
) -> SlotPlan {
    let liveness = slot_liveness_choice();
    plan_slots_with(graph, schedule, sr_map, token_edges_not_uses, liveness)
}

/// The [`SlotLiveness`] [`plan_slots`] runs under the current switches
/// (`CRATONVM_JIT_IR_SHARED_LIVENESS_CORE`,
/// `CRATONVM_JIT_IR_SLOTS_PHI_EDGE_RANGES`,
/// `CRATONVM_JIT_IR_PHI_EDGE_INTERFERE`). Interpreter round i2 wave 66, lane
/// L2: factored out of [`plan_slots_with_token_edges`], unchanged, so
/// [`plan_slots_one_pass`] asks the same question.
pub(in crate::ir_lower) fn slot_liveness_choice() -> SlotLiveness {
    let phi_ranges = crate::regalloc::slots_phi_edge_ranges_enabled();
    if phi_ranges || crate::regalloc::shared_liveness_core_enabled() {
        SlotLiveness::Core {
            phi_edge_interfere: phi_ranges && crate::regalloc::phi_edge_interfere_enabled(),
        }
    } else {
        SlotLiveness::Legacy
    }
}

// The single pass below runs under the live model's budget and cap and feeds
// the slot plan with it; that is the slot plan's own answer only while the
// two pairs of constants are equal.
const _: () = assert!(
    SLOT_PLAN_WORK_BUDGET == crate::regalloc::LIVE_MODEL_WORK_BUDGET
        && SLOT_PLAN_MAX_ITERATIONS == crate::regalloc::LIVE_MODEL_MAX_ITERATIONS
);

/// The compile's single `regalloc::liveness_core` answer, kept from the slot
/// plan to the allocator's live model (interpreter round i2 wave 66, lane L2,
/// `CRATONVM_JIT_IR_ONE_LIVENESS_PASS`;
/// `i65-L2-proposal-one-liveness-pass-per-compile`).
pub(in crate::ir_lower) struct OneLivenessPass {
    /// `regalloc::live_model_wants_loc`'s answer, which the core ran over.
    pub(in crate::ir_lower) wants_loc: Vec<bool>,
    /// Computed with the live model's rules, its budget, and the read list.
    pub(in crate::ir_lower) core: crate::regalloc::LivenessCore,
}

impl OneLivenessPass {
    /// The live model `regalloc::build_live_model(graph, schedule)` builds
    /// under the same switches, without rerunning the fixed point.
    pub(in crate::ir_lower) fn into_live_model(self, graph: &Graph, schedule: &Schedule) -> crate::regalloc::LiveModel {
        crate::regalloc::live_model_from_core(graph, schedule, self.wants_loc, self.core)
    }
}

/// Why a compile kept two liveness passes under
/// `CRATONVM_JIT_IR_ONE_LIVENESS_PASS`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ir_lower) enum OneLivenessPassDecline {
    /// The switch is off.
    Off,
    /// The slot plan's token or φ rule is not the live model's.
    RulesDiffer,
    /// `slot_plan_wants` and `regalloc::live_model_wants_loc` disagree (kept
    /// in lockstep by `the_two_value_defining_enumerations_agree`; checked
    /// here per compile because sharing across a disagreement would be wrong
    /// rather than slow).
    ValueSetsDiffer,
}

impl OneLivenessPassDecline {
    pub(in crate::ir_lower) fn label(self) -> &'static str {
        match self {
            OneLivenessPassDecline::Off => "off",
            OneLivenessPassDecline::RulesDiffer => "rules-differ",
            OneLivenessPassDecline::ValueSetsDiffer => "value-sets-differ",
        }
    }
}

/// The rules one shared pass runs under, or `None` when the slot plan
/// (`slot_token`, `slot_liveness`) and the live model (`model`) would ask
/// different questions. The legacy slot liveness has no φ rule.
pub(in crate::ir_lower) fn one_liveness_pass_rules(
    slot_token: bool,
    slot_liveness: SlotLiveness,
    model: crate::regalloc::LivenessRules,
) -> Option<crate::regalloc::LivenessRules> {
    let slot_phi = match slot_liveness {
        SlotLiveness::Legacy => false,
        SlotLiveness::Core { phi_edge_interfere } => phi_edge_interfere,
    };
    if slot_token == model.token_edges_not_uses && slot_phi == model.phi_edge_interfere {
        Some(crate::regalloc::LivenessRules {
            collect_reads: true,
            ..model
        })
    } else {
        None
    }
}

/// [`plan_slots`], plus the liveness answer for the live model when
/// `CRATONVM_JIT_IR_ONE_LIVENESS_PASS` is on and the two consumers ask the
/// same question (interpreter round i2 wave 66, lane L2). Off, or declined,
/// the plan is exactly [`plan_slots`]' and the caller builds its own model.
pub(in crate::ir_lower) fn plan_slots_one_pass(
    graph: &Graph,
    schedule: &Schedule,
    sr_map: Option<&ScalarReplacementMap>,
) -> (SlotPlan, Result<OneLivenessPass, OneLivenessPassDecline>) {
    plan_slots_one_pass_scratch(graph, schedule, sr_map, None)
}

/// [`plan_slots_one_pass`] with an optional per-compile
/// `regalloc::LivenessScratch` (JIT round 19 wave 10, lane lv10,
/// `CRATONVM_JIT_IR_LIVENESS_SHARED_SCRATCH`): the switch-off plan's legacy
/// liveness copy runs through it. `None` is [`plan_slots_one_pass`] exactly.
pub(in crate::ir_lower) fn plan_slots_one_pass_scratch(
    graph: &Graph,
    schedule: &Schedule,
    sr_map: Option<&ScalarReplacementMap>,
    scratch: Option<&mut crate::regalloc::LivenessScratch>,
) -> (SlotPlan, Result<OneLivenessPass, OneLivenessPassDecline>) {
    if !crate::regalloc::one_liveness_pass_enabled() {
        let plan = match scratch {
            None => plan_slots(graph, schedule, sr_map),
            Some(scratch) => plan_slots_with_scratch(
                graph,
                schedule,
                sr_map,
                crate::regalloc::slots_token_edges_not_uses_enabled(),
                slot_liveness_choice(),
                Some(scratch),
            ),
        };
        return (plan, Err(OneLivenessPassDecline::Off));
    }
    plan_slots_one_pass_with(
        graph,
        schedule,
        sr_map,
        crate::regalloc::slots_token_edges_not_uses_enabled(),
        slot_liveness_choice(),
        crate::regalloc::live_model_rules(),
    )
}

/// [`plan_slots_one_pass`] with the switch readings passed in.
pub(in crate::ir_lower) fn plan_slots_one_pass_with(
    graph: &Graph,
    schedule: &Schedule,
    sr_map: Option<&ScalarReplacementMap>,
    slot_token: bool,
    slot_liveness: SlotLiveness,
    model_rules: crate::regalloc::LivenessRules,
) -> (SlotPlan, Result<OneLivenessPass, OneLivenessPassDecline>) {
    let rules = match one_liveness_pass_rules(slot_token, slot_liveness, model_rules) {
        Some(rules) => rules,
        None => {
            return (
                plan_slots_with(graph, schedule, sr_map, slot_token, slot_liveness),
                Err(OneLivenessPassDecline::RulesDiffer),
            )
        }
    };
    let wants_slot = slot_plan_wants(graph, schedule);
    let wants_loc = crate::regalloc::live_model_wants_loc(graph, schedule);
    if wants_slot != wants_loc {
        return (
            plan_slots_with(graph, schedule, sr_map, slot_token, slot_liveness),
            Err(OneLivenessPassDecline::ValueSetsDiffer),
        );
    }
    let core = crate::regalloc::liveness_core(
        graph,
        schedule,
        &wants_loc,
        rules,
        crate::regalloc::LIVE_MODEL_WORK_BUDGET,
        crate::regalloc::LIVE_MODEL_MAX_ITERATIONS,
    );
    let plan = plan_slots_from_core(graph, schedule, sr_map, &wants_slot, core.slot_view());
    (plan, Ok(OneLivenessPass { wants_loc, core }))
}

/// Slot and register planning share selected operands. Different token/phi
/// policies retain separate analyses of that same value flow.
pub(in crate::ir_lower) fn plan_slots_with_value_flow(
    graph: &Graph, schedule: &Schedule, sr_map: Option<&ScalarReplacementMap>,
    flow: &crate::regalloc::ValueFlow,
) -> (SlotPlan, Result<OneLivenessPass, OneLivenessPassDecline>) {
    let mut wants_slot = slot_plan_wants(graph, schedule);
    let mut wants_loc = crate::regalloc::live_model_wants_loc(graph, schedule);
    flow.apply_wants(&mut wants_slot);
    flow.apply_wants(&mut wants_loc);
    let model_rules = crate::regalloc::live_model_rules();
    let mut rules = crate::regalloc::LivenessRules {
        token_edges_not_uses: crate::regalloc::slots_token_edges_not_uses_enabled(),
        phi_edge_interfere: match slot_liveness_choice() {
            SlotLiveness::Legacy => false,
            SlotLiveness::Core { phi_edge_interfere } => phi_edge_interfere,
        },
        collect_reads: false,
    };
    let same_values = wants_slot == wants_loc;
    let shared = same_values && rules.token_edges_not_uses == model_rules.token_edges_not_uses
        && rules.phi_edge_interfere == model_rules.phi_edge_interfere;
    rules.collect_reads = shared;
    let core = crate::regalloc::liveness_core_with_value_flow(graph, schedule, &wants_slot,
        rules, SLOT_PLAN_WORK_BUDGET, SLOT_PLAN_MAX_ITERATIONS, flow);
    if shared {
        let plan = plan_slots_from_core(graph, schedule, sr_map, &wants_slot, core.slot_view());
        (plan, Ok(OneLivenessPass { wants_loc, core }))
    } else {
        let plan = plan_slots_from_core(graph, schedule, sr_map, &wants_slot, core);
        (plan, Err(if same_values { OneLivenessPassDecline::RulesDiffer }
            else { OneLivenessPassDecline::ValueSetsDiffer }))
    }
}

/// Which liveness computation [`plan_slots_with`] runs (interpreter round i2
/// wave 65, lane L2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ir_lower) enum SlotLiveness {
    /// [`plan_slots_liveness_legacy`]: this file's own copy, no φ edge rule.
    Legacy,
    /// `regalloc::liveness_core`, with the φ edge rule as given.
    Core { phi_edge_interfere: bool },
}

/// Which values receive a frame slot.
///
/// Exactly the set `alloc_slot` is called for: every phi (reserved by
/// `prealloc_phi_slots` whether scheduled or not) plus every scheduled node
/// whose lowering allocates. `op_defines_result_slot` is the same predicate
/// `verify_data_locations` uses, so the two cannot drift.
pub(in crate::ir_lower) fn slot_plan_wants(graph: &Graph, schedule: &Schedule) -> Vec<bool> {
    let n = graph.nodes.len();
    let mut wants_slot = vec![false; n];
    for (id, node) in graph.nodes.iter().enumerate() {
        if matches!(node.op, Op::Phi) {
            wants_slot[id] = true;
        }
    }
    for block in &schedule.blocks {
        for &nid in &block.nodes {
            if let Some(node) = graph.nodes.get(nid as usize) {
                if op_defines_result_slot(&node.op) {
                    wants_slot[nid as usize] = true;
                }
            }
        }
    }
    wants_slot
}

/// [`plan_slots_with_token_edges`] with the liveness choice passed in.
pub(in crate::ir_lower) fn plan_slots_with(
    graph: &Graph,
    schedule: &Schedule,
    sr_map: Option<&ScalarReplacementMap>,
    token_edges_not_uses: bool,
    liveness: SlotLiveness,
) -> SlotPlan {
    plan_slots_with_scratch(
        graph,
        schedule,
        sr_map,
        token_edges_not_uses,
        liveness,
        None,
    )
}

/// [`plan_slots_with`] with an optional per-compile
/// `regalloc::LivenessScratch` (JIT round 19 wave 10, lane lv10,
/// `CRATONVM_JIT_IR_LIVENESS_SHARED_SCRATCH`), which only the legacy
/// liveness copy uses. `None` is [`plan_slots_with`] exactly.
pub(in crate::ir_lower) fn plan_slots_with_scratch(
    graph: &Graph,
    schedule: &Schedule,
    sr_map: Option<&ScalarReplacementMap>,
    token_edges_not_uses: bool,
    liveness: SlotLiveness,
    scratch: Option<&mut crate::regalloc::LivenessScratch>,
) -> SlotPlan {
    // ── 1. Which values receive a frame slot ─────────────────────────
    let wants_slot = slot_plan_wants(graph, schedule);

    // ── 2 and 4. Positions, uses, and the liveness fixed point ───────
    //
    // Interpreter round i2 wave 65 (lane L2): either this file's own copy
    // (`plan_slots_liveness_legacy`) or the one `regalloc::build_live_model`
    // runs too (`regalloc::liveness_core`). Same answer for the same rules.
    let core = match liveness {
        SlotLiveness::Legacy => match scratch {
            None => plan_slots_liveness_legacy(graph, schedule, &wants_slot, token_edges_not_uses),
            Some(scratch) => plan_slots_liveness_legacy_impl(
                graph,
                schedule,
                &wants_slot,
                token_edges_not_uses,
                SLOT_PLAN_MAX_ITERATIONS,
                crate::regalloc::legacy_liveness_fixed_point(schedule.blocks.len()),
                None,
                Some(scratch),
            ),
        },
        SlotLiveness::Core { phi_edge_interfere } => crate::regalloc::liveness_core(
            graph,
            schedule,
            &wants_slot,
            crate::regalloc::LivenessRules {
                token_edges_not_uses,
                phi_edge_interfere,
                collect_reads: false,
            },
            SLOT_PLAN_WORK_BUDGET,
            SLOT_PLAN_MAX_ITERATIONS,
        ),
    };
    plan_slots_from_core(graph, schedule, sr_map, &wants_slot, core)
}

/// May `node`'s value donate a colour but never receive a recycled one?
///
/// cov-01: `Op::ConstString` / `Op::ConstClass` produce a `Ref` from a helper
/// call, exactly as `Op::Call` can, and their arms publish the safepoint map
/// before the call for the same reason. Give them the same "may donate a
/// colour, may never receive a recycled one" treatment; a recycled colour here
/// would be a slot the map already named. (JIT round 25 wave 3: one predicate
/// for `plan_slots_from_core` and [`recolour_slot_plan`]; j27 wave 2: and
/// `regalloc::ls_color_homes`.)
pub(crate) fn colour_is_fresh_only(node: &Node) -> bool {
    node.ty == IrType::Ref
        && matches!(
            node.op,
            Op::Call { .. }
                // Round 16 (lane indy): a bridged indy's `Ref` result is a
                // helper-call result mapped the same way.
                | Op::IndyBridge { .. }
                // Round 16 (lane matrix): so is a `multianewarray`'s.
                | Op::MultiNewArray { .. }
                // nio21 w15 L3: and a deferred `new`'s.
                | Op::NewCp { .. }
                | Op::ConstString { .. }
                | Op::ConstClass { .. }
                | Op::LoadStatic { .. }
        )
}

/// Steps 3 onward of [`plan_slots_with`]: pins, ranges, colouring, over a
/// liveness answer already computed for `wants_slot` (interpreter round i2
/// wave 66, lane L2; split out so `plan_slots_one_pass` can hand it the
/// compile's single `regalloc::liveness_core` answer). Reads `pos_of`,
/// `span`, `total_positions`, `lo`, `hi`, `set_only_by_phi_rule` and
/// `converged`; never the bit sets or the read list.
pub(in crate::ir_lower) fn plan_slots_from_core(
    graph: &Graph,
    schedule: &Schedule,
    sr_map: Option<&ScalarReplacementMap>,
    wants_slot: &[bool],
    core: crate::regalloc::LivenessCore,
) -> SlotPlan {
    let n = graph.nodes.len();
    let crate::regalloc::LivenessCore {
        pos_of,
        span,
        total_positions,
        lo,
        hi,
        set_only_by_phi_rule,
        converged: coloured,
        ..
    } = core;

    // ── 3. Classes that never share ──────────────────────────────────
    let mut pinned = vec![false; n];
    let mut fresh_only = vec![false; n];
    for (id, node) in graph.nodes.iter().enumerate() {
        if !wants_slot[id] {
            continue;
        }
        if matches!(node.op, Op::Phi) {
            pinned[id] = true;
        }
        if colour_is_fresh_only(node) {
            fresh_only[id] = true;
        }
    }
    // Round 14 wave 1 (lane chain): a CHAIN snapshot's locals and stack are
    // not pinned here; `chain_read_colour_ranges` below keeps each such value's
    // word from its definition to every guard that can publish the chain. See
    // `ir_chain_snapshot_range_pins_enabled`.
    let chain_range_mode = ir_chain_snapshot_range_pins_enabled()
        && graph.safepoints.iter().any(|sp| sp.is_guard_splice_state());
    let mut chain_named = vec![false; if chain_range_mode { n } else { 0 }];
    for sp in &graph.safepoints {
        if chain_range_mode && sp.is_guard_splice_state() {
            for &v in sp.locals.iter().chain(sp.stack.iter()) {
                if v != NO_NODE {
                    if let Some(named) = chain_named.get_mut(v as usize) {
                        *named = true;
                    }
                }
            }
            // Monitors stay pinned: the locked object is the compiling
            // method's, and a flat frame names it too.
            for &v in &sp.monitors {
                if v != NO_NODE {
                    if let Some(slot) = pinned.get_mut(v as usize) {
                        *slot = true;
                    }
                }
            }
            continue;
        }
        // A monitor entry pins its reference's home exactly as a local does:
        // the deopt reads the locked object from that word at any recorded bci.
        for &v in sp
            .locals
            .iter()
            .chain(sp.stack.iter())
            .chain(sp.monitors.iter())
        {
            if v != NO_NODE {
                if let Some(slot) = pinned.get_mut(v as usize) {
                    *slot = true;
                }
            }
        }
    }
    // The walk is `scalar_replacement_field_values`, which
    // `regalloc::LiveModel::pin_scalar_replacement_fields` also calls, so the
    // two pin sets are one set by construction (round 9 wave 4 --
    // `regalloc-live-model-misses-scalar-replacement-pins`).
    let sr_field_values = hidden_frame_values(graph, schedule, sr_map);
    // JIT round 19 wave 5 (lane osr5, OSR2-3): the values a promoted loop's
    // deopt pads write back are read from their homes like these are.
    // Wave 7 (lane osr7, OSR5-3): plus the joins' value phis under the branch
    // switch (`promoted_pending_slot_pins`).
    for &v in &sr_field_values {
        if let Some(slot) = pinned.get_mut(v as usize) {
            *slot = true;
        }
    }

    // ── 5b. Finalise the ranges ──────────────────────────────────────
    let whole_method = LiveRange {
        lo: 0,
        hi: total_positions.saturating_sub(1),
    };
    let mut range: Vec<Option<LiveRange>> = vec![None; n];
    // gen r5w6/oomjit10: read before the pinning below rewrites `lo`'s
    // meaning into "whole method". `lo == usize::MAX` after a CONVERGED
    // analysis = no use, no edge copy and no live block names the value.
    // Wave 65: the φ edge rule (`SlotLiveness::Core`) can set a φ's `lo` from
    // its incoming edges alone; that is not a read, so the question is asked
    // of the pre-rule `lo` (`set_only_by_phi_rule`, all false without it).
    let phi_code_unused: Vec<bool> = (0..n)
        .map(|id| {
            coloured
                && wants_slot[id]
                && (lo[id] == usize::MAX || set_only_by_phi_rule.get(id).copied().unwrap_or(false))
                && matches!(graph.nodes[id].op, Op::Phi)
        })
        .collect();
    for id in 0..n {
        if !wants_slot[id] {
            continue;
        }
        if !coloured {
            pinned[id] = true;
        }
        range[id] = Some(if lo[id] == usize::MAX || !coloured {
            // Never placed and never live anywhere (an unscheduled phi, or the
            // analysis was skipped): assume the whole method and pin it.
            whole_method
        } else {
            LiveRange {
                lo: lo[id],
                hi: hi[id].max(lo[id]),
            }
        });
    }

    // Peak simultaneous liveness, by sweeping the interval endpoints. This is
    // the floor a perfect colouring would reach; `slots` sits above it by the
    // pinned classes plus whatever the contiguous-interval approximation costs.
    let mut delta = vec![0i64; total_positions + 2];
    for r in range.iter().flatten() {
        if r.lo < delta.len() && r.hi + 1 < delta.len() {
            delta[r.lo] += 1;
            delta[r.hi + 1] -= 1;
        }
    }
    let mut running = 0i64;
    let mut peak_live = 0i64;
    for d in &delta {
        running += d;
        peak_live = peak_live.max(running);
    }

    // ── 5c. Chain-snapshot values (round 14 wave 1, lane chain) ──────
    //
    // A value only chain snapshots name gets its COLOURING range widened to
    // cover every guard that can publish such a chain, instead of a pin. The
    // published `range` stays the value range, so every reader of it
    // (residency's second opinions, the dead-home clears) sees exactly what it
    // saw before; `verify_slot_colouring` checks the wider one. Anything the
    // analysis cannot settle is pinned, which is the pre-round-14 plan.
    let mut colour_range: Vec<Option<LiveRange>> = Vec::new();
    let mut chain_ranged = 0usize;
    if chain_range_mode {
        let candidate: Vec<bool> = (0..n)
            .map(|id| chain_named[id] && wants_slot[id] && !pinned[id])
            .collect();
        let widened = if coloured {
            chain_read_colour_ranges(graph, schedule, &pos_of, &span, &candidate, &range)
        } else {
            None
        };
        match widened {
            Some(widened) => {
                chain_ranged = candidate.iter().filter(|&&c| c).count();
                colour_range = widened;
            }
            None => {
                for (id, &c) in candidate.iter().enumerate() {
                    if c {
                        pinned[id] = true;
                    }
                }
            }
        }
    }
    let colouring_range: &[Option<LiveRange>] = if colour_range.is_empty() {
        &range[..]
    } else {
        &colour_range[..]
    };

    // ── 6. Colouring ─────────────────────────────────────────────────
    //
    // Pinned values first, in node-id order — the order `prealloc_phi_slots`
    // walks — so a phi web's slots stay where they have always been and the
    // frame layout of a phi-only change is unperturbed.
    let mut node_color: Vec<Option<u32>> = vec![None; n];
    let mut class: Vec<Option<SlotClass>> = vec![None; n];
    // A colour index cannot exceed the node count, and `NodeId` is a `u32`, so
    // the counter provably fits.
    let mut next_color: u32 = 0;
    for id in 0..n {
        if wants_slot[id] && pinned[id] {
            node_color[id] = Some(next_color);
            class[id] = Some(SlotClass::Pinned);
            next_color = next_color.saturating_add(1);
        }
    }

    let mut order: Vec<usize> = (0..n).filter(|&id| wants_slot[id] && !pinned[id]).collect();
    order.sort_by_key(|&id| (colouring_range[id].map_or(0, |r| r.lo), id));
    // (hi, colour, is_ref), min-heap on `hi` so the earliest-expiring colour
    // is released first.
    let mut active: BinaryHeap<Reverse<(usize, u32, bool)>> = BinaryHeap::new();
    let mut free_ref: Vec<u32> = Vec::new();
    let mut free_prim: Vec<u32> = Vec::new();
    for id in order {
        let r = match colouring_range[id] {
            Some(r) => r,
            None => continue,
        };
        while let Some(&Reverse((active_hi, active_color, active_is_ref))) = active.peek() {
            if active_hi >= r.lo {
                break;
            }
            active.pop();
            if active_is_ref {
                free_ref.push(active_color);
            } else {
                free_prim.push(active_color);
            }
        }
        let is_ref = graph.nodes[id].ty == IrType::Ref;
        let recycled = if fresh_only[id] {
            None
        } else if is_ref {
            free_ref.pop()
        } else {
            free_prim.pop()
        };
        let color = match recycled {
            Some(color) => color,
            None => {
                let color = next_color;
                next_color = next_color.saturating_add(1);
                color
            }
        };
        node_color[id] = Some(color);
        class[id] = Some(if is_ref {
            SlotClass::Ref
        } else {
            SlotClass::Prim
        });
        active.push(Reverse((r.hi, color, is_ref)));
    }

    SlotPlan {
        node_color,
        range,
        class,
        slots: next_color as usize,
        peak_live: peak_live.max(0) as usize,
        sr_field_values,
        pos_of,
        phi_code_unused,
        colour_range,
        chain_ranged,
        snapshot_ranged: None,
    }
}

/// The legacy (pre-wave-65) liveness half of [`plan_slots_with`]: steps 2 and
/// 4 exactly as they were spelled here before `regalloc::liveness_core`
/// existed, returned in the core's shape. No φ edge rule and no read list.
/// Kept until `CRATONVM_JIT_IR_SHARED_LIVENESS_CORE` has been confirmed
/// byte-identical on a host.
pub(in crate::ir_lower) fn plan_slots_liveness_legacy(
    graph: &Graph,
    schedule: &Schedule,
    wants_slot: &[bool],
    token_edges_not_uses: bool,
) -> crate::regalloc::LivenessCore {
    plan_slots_liveness_legacy_with(
        graph,
        schedule,
        wants_slot,
        token_edges_not_uses,
        SLOT_PLAN_MAX_ITERATIONS,
        crate::regalloc::legacy_liveness_fixed_point(schedule.blocks.len()),
        None,
    )
}

/// [`plan_slots_liveness_legacy`] with the iteration cap and the fixed point
/// explicit, and the fixed point's raw result in `trace` when asked for
/// (interpreter round i2 wave 70, lane L2,
/// `CRATONVM_JIT_IR_LEGACY_LIVENESS_WORKLIST`). The round-robin arm is the
/// legacy loop, unchanged.
pub(in crate::ir_lower) fn plan_slots_liveness_legacy_with(
    graph: &Graph,
    schedule: &Schedule,
    wants_slot: &[bool],
    token_edges_not_uses: bool,
    max_iterations: usize,
    fixed_point: crate::regalloc::LivenessFixedPoint,
    trace: Option<&mut crate::regalloc::LsLegacyRun>,
) -> crate::regalloc::LivenessCore {
    plan_slots_liveness_legacy_impl(
        graph,
        schedule,
        wants_slot,
        token_edges_not_uses,
        max_iterations,
        fixed_point,
        trace,
        None,
    )
}

/// The slot plan's own position walk (step 2 of
/// [`plan_slots_liveness_legacy`], unchanged; split out in JIT round 19 wave
/// 10, lane lv10, so the scratch path can take
/// `regalloc::LivenessScratch::positions` instead).
pub(in crate::ir_lower) fn slot_plan_positions_legacy(
    n: usize,
    schedule: &Schedule,
) -> (Vec<Option<usize>>, Vec<(usize, usize)>, usize) {
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
        // One position past the block's last instruction: the outgoing edge,
        // where `emit_phi_copies` reads this block's phi arguments.
        let end = seq;
        seq += 1;
        span.push((start, end));
    }
    (pos_of, span, seq)
}

/// [`plan_slots_liveness_legacy_with`] with an optional per-compile
/// `regalloc::LivenessScratch` (JIT round 19 wave 10, lane lv10,
/// `CRATONVM_JIT_IR_LIVENESS_SHARED_SCRATCH`). `None` is the legacy copy
/// exactly.
#[allow(clippy::too_many_arguments)]
pub(in crate::ir_lower) fn plan_slots_liveness_legacy_impl(
    graph: &Graph,
    schedule: &Schedule,
    wants_slot: &[bool],
    token_edges_not_uses: bool,
    max_iterations: usize,
    fixed_point: crate::regalloc::LivenessFixedPoint,
    trace: Option<&mut crate::regalloc::LsLegacyRun>,
    mut scratch: Option<&mut crate::regalloc::LivenessScratch>,
) -> crate::regalloc::LivenessCore {
    let n = graph.nodes.len();
    let nb = schedule.blocks.len();

    // ── 2. Linear emission positions and block spans ─────────────────
    let (pos_of, span, total_positions) = match scratch.as_deref_mut() {
        Some(s) => {
            let m = s.positions(graph, schedule);
            (m.pos_of.clone(), m.span.clone(), m.total_positions)
        }
        None => slot_plan_positions_legacy(n, schedule),
    };

    // ── 4. Uses, definitions and the liveness fixed point ────────────
    let mut lo = vec![usize::MAX; n];
    let mut hi = vec![0usize; n];
    // Past the work budget nothing below runs — including the `nodes × blocks`
    // bit-set allocation, which is the expensive part — and every value keeps a
    // dedicated slot.
    let mut coloured = n.saturating_mul(nb) <= SLOT_PLAN_WORK_BUDGET;
    let words = n.div_ceil(64).max(1);
    // Wave 65: handed back in the core's shape (empty when the budget skipped
    // the analysis).
    let mut kept_live_in: Vec<u64> = Vec::new();
    let mut kept_live_out: Vec<u64> = Vec::new();
    if coloured {
        // JIT round 19 wave 10 (lane lv10): pooled when a scratch is present.
        let mut def_bits = crate::regalloc::ls_zeroed(&mut scratch, words * nb);
        let mut use_bits = crate::regalloc::ls_zeroed(&mut scratch, words * nb);
        let mut phi_out_bits = crate::regalloc::ls_zeroed(&mut scratch, words * nb);

        // Direct (non-phi) uses and definitions, in block order so that "used
        // before defined in this block" — the upward-exposed set the dataflow
        // needs — falls out of a single pass.
        let mut local_def = crate::regalloc::ls_zeroed(&mut scratch, words);
        for b in 0..nb {
            let base = b * words;
            local_def.fill(0);
            let block = &schedule.blocks[b];
            for u in block.nodes.iter().copied().chain(block.terminator) {
                let ui = u as usize;
                let node = match graph.nodes.get(ui) {
                    Some(node) => node,
                    None => continue,
                };
                let upos = pos_of.get(ui).copied().flatten().unwrap_or(span[b].0);
                // A phi's value inputs are NOT read here — the predecessor's
                // edge copy reads them, handled below; `inputs[0]` is control.
                if !matches!(node.op, Op::Phi) {
                    for (slot, &inp) in node.inputs.iter().enumerate() {
                        let ii = inp as usize;
                        if inp == NO_NODE || ii >= n || !wants_slot[ii] {
                            continue;
                        }
                        // Interpreter round i2 wave 64 (lane L2): an ordering
                        // edge, not a read of the home word (see
                        // `plan_slots_with_token_edges`).
                        if token_edges_not_uses && crate::ir::is_memory_token_slot(node, slot) {
                            continue;
                        }
                        lo[ii] = lo[ii].min(upos);
                        hi[ii] = hi[ii].max(upos);
                        if !bits_contains(&local_def, ii) {
                            bits_insert(&mut use_bits[base..base + words], ii);
                        }
                    }
                }
                if wants_slot[ui] {
                    bits_insert(&mut local_def, ui);
                    bits_insert(&mut def_bits[base..base + words], ui);
                    lo[ui] = lo[ui].min(upos);
                    hi[ui] = hi[ui].max(upos);
                }
            }
        }

        // Phi edge copies. Grouping the phis by their merge control once turns
        // `emit_phi_copies`' per-edge whole-graph scan into a single pass.
        // JIT round 19 wave 10 (lane lv10): one edge's reads as a closure, so
        // the walk below and the scratch's prebuilt table (the same edges in
        // the same order) share it.
        let mut on_edge = |pred: usize, k: usize, phis: &[NodeId]| {
            let at = span[pred].1;
            for &pid in phis {
                let v = match graph
                    .nodes
                    .get(pid as usize)
                    .and_then(|p| p.inputs.get(k + 1))
                {
                    Some(&v) if v != NO_NODE => v,
                    _ => continue,
                };
                let vi = v as usize;
                if vi >= n || !wants_slot[vi] {
                    continue;
                }
                // Interpreter round i2 wave 64 (lane L2): a memory φ gets
                // no edge copy, so its source's home is not read here.
                if token_edges_not_uses
                    && graph
                        .nodes
                        .get(pid as usize)
                        .is_some_and(|p| p.ty == IrType::Memory)
                {
                    continue;
                }
                lo[vi] = lo[vi].min(at);
                hi[vi] = hi[vi].max(at);
                bits_insert(&mut phi_out_bits[pred * words..(pred + 1) * words], vi);
            }
        };
        match scratch.as_deref_mut() {
            Some(s) => {
                let edges = s.take_phi_edges(graph, schedule);
                for e in &edges.edges {
                    on_edge(e.pred, e.k, edges.phis_of(e));
                }
                s.put_phi_edges(edges);
            }
            None => {
                let mut phis_of: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
                for (id, node) in graph.nodes.iter().enumerate() {
                    if !matches!(node.op, Op::Phi) {
                        continue;
                    }
                    if let Some(&merge) = node.inputs.first() {
                        if merge != NO_NODE {
                            phis_of.entry(merge).or_default().push(id as NodeId);
                        }
                    }
                }
                for succ in 0..nb {
                    let merge_ctrl = schedule.blocks[succ].ctrl;
                    let merge_node = match graph.nodes.get(merge_ctrl as usize) {
                        Some(m) if matches!(m.op, Op::Merge | Op::Region) => m,
                        _ => continue,
                    };
                    let phis = match phis_of.get(&merge_ctrl) {
                        Some(phis) => phis,
                        None => continue,
                    };
                    // merge.inputs[k] is the control token for phi value k (= input k+1).
                    for (k, &ctrl_in) in merge_node.inputs.iter().enumerate() {
                        let pred = match ctrl_block_of(graph, schedule, ctrl_in) {
                            Some(pred) if pred < nb => pred,
                            _ => continue,
                        };
                        on_edge(pred, k, phis);
                    }
                }
            }
        }

        let mut live_in = crate::regalloc::ls_zeroed(&mut scratch, words * nb);
        let mut live_out = crate::regalloc::ls_zeroed(&mut scratch, words * nb);
        let mut visits = 0usize;
        let converged = match fixed_point {
            crate::regalloc::LivenessFixedPoint::RoundRobin => {
                let mut scratch = vec![0u64; words];
                let mut converged = false;
                for _ in 0..max_iterations {
                    let mut changed = false;
                    visits += nb;
                    // Reverse block order approximates reverse postorder, which
                    // is the fast direction for a backward analysis.
                    for b in (0..nb).rev() {
                        let base = b * words;
                        scratch.copy_from_slice(&phi_out_bits[base..base + words]);
                        for &s in &schedule.blocks[b].successors {
                            if s >= nb {
                                continue;
                            }
                            for w in 0..words {
                                scratch[w] |= live_in[s * words + w];
                            }
                        }
                        for w in 0..words {
                            let merged_out = live_out[base + w] | scratch[w];
                            if merged_out != live_out[base + w] {
                                live_out[base + w] = merged_out;
                                changed = true;
                            }
                            let entering = use_bits[base + w] | (merged_out & !def_bits[base + w]);
                            let merged_in = live_in[base + w] | entering;
                            if merged_in != live_in[base + w] {
                                live_in[base + w] = merged_in;
                                changed = true;
                            }
                        }
                    }
                    if !changed {
                        converged = true;
                        break;
                    }
                }
                converged
            }
            // Interpreter round i2 wave 70, lane L2: the same answer, pass for
            // pass, visiting only dirty blocks (wave 71: the predecessor
            // lists per `fixed_point`).
            crate::regalloc::LivenessFixedPoint::Worklist
            | crate::regalloc::LivenessFixedPoint::WorklistSchedulePreds
            | crate::regalloc::LivenessFixedPoint::WorklistPostorder { .. }
            | crate::regalloc::LivenessFixedPoint::RoundRobinDriver => {
                crate::regalloc::ls_fixed_point_on_schedule(
                    schedule.blocks.as_slice(),
                    fixed_point,
                    crate::regalloc::LsFixedPointSets {
                        nb,
                        words,
                        phi_out: &phi_out_bits,
                        uses: &use_bits,
                        defs: &def_bits,
                    },
                    &mut live_in,
                    &mut live_out,
                    max_iterations,
                    &mut visits,
                )
            }
        };
        if let Some(trace) = trace {
            *trace = crate::regalloc::LsLegacyRun {
                ran: true,
                converged,
                live_in: live_in.clone(),
                live_out: live_out.clone(),
                visits,
            };
        }
        coloured = converged;

        // ── 5a. Widen each range to cover every block it is live in ──
        //
        // This is where loops are handled: a value used inside a loop is
        // live-out of every block on the back edge's path, so its range covers
        // the whole loop without the colouring ever knowing what a loop is.
        if coloured {
            for b in 0..nb {
                let base = b * words;
                let (start, end) = span[b];
                bits_for_each(&live_in[base..base + words], |v| {
                    if v < n && wants_slot[v] {
                        lo[v] = lo[v].min(start);
                    }
                });
                bits_for_each(&live_out[base..base + words], |v| {
                    if v < n && wants_slot[v] {
                        hi[v] = hi[v].max(end);
                    }
                });
            }
        }
        // JIT round 19 wave 10 (lane lv10): back to the pool, if any (the
        // live sets leave with the answer).
        for buf in [def_bits, use_bits, phi_out_bits, local_def] {
            crate::regalloc::ls_give(&mut scratch, buf);
        }
        kept_live_in = live_in;
        kept_live_out = live_out;
    }

    crate::regalloc::LivenessCore {
        pos_of,
        span,
        total_positions,
        lo,
        hi,
        set_only_by_phi_rule: vec![false; n],
        converged: coloured,
        words,
        live_in: kept_live_in,
        live_out: kept_live_out,
        reads: Vec::new(),
    }
}

/// The token-trim census's slot-plan column (interpreter round i2 wave 64,
/// lane L2): both frame plans for one graph, compared. Diagnostic only; costs
/// two extra [`plan_slots_with_token_edges`] passes, run only while
/// `CRATONVM_DBG_IR_LS_TOKEN_TRIM_CENSUS` is on.
pub(in crate::ir_lower) fn token_trim_slot_column(
    graph: &Graph,
    schedule: &Schedule,
    sr_map: Option<&ScalarReplacementMap>,
) -> crate::regalloc::TokenTrimSlotColumn {
    let off = plan_slots_with_token_edges(graph, schedule, sr_map, false);
    let on = plan_slots_with_token_edges(graph, schedule, sr_map, true);
    let homes_trimmed = off
        .range
        .iter()
        .zip(on.range.iter())
        .filter(|(a, b)| match (a, b) {
            (Some(a), Some(b)) => b.hi.saturating_sub(b.lo) < a.hi.saturating_sub(a.lo),
            _ => false,
        })
        .count();
    crate::regalloc::TokenTrimSlotColumn {
        words_off: off.slots,
        words_on: on.slots,
        ref_words_off: off.slots.saturating_sub(off.prim_colors().len()),
        ref_words_on: on.slots.saturating_sub(on.prim_colors().len()),
        homes_trimmed,
    }
}

/// Every value a LOOP-CARRIED object's recipe may name
/// ([`VirtualObjectInfo::loop_phis`]): its value φs and every value its
/// eliminated stores wrote. A deopt reads one of them when it describes the
/// object, but no snapshot names them, so without this they read as "named by
/// no deopt" and may lose their home word. Treating them as named keeps the
/// homes (it can only keep more). Empty unless a recipe has `loop_phis`.
pub(in crate::ir_lower) fn loop_carried_recipe_values(
    sr: Option<&ScalarReplacementMap>,
) -> impl Iterator<Item = NodeId> + '_ {
    sr.into_iter()
        .flat_map(|m| m.objects.values())
        .filter(|info| info.loop_phis.is_some())
        .flat_map(|info| {
            info.loop_phis
                .iter()
                .flatten()
                .map(|&(_, phi)| phi)
                .chain(
                    info.store_fields
                        .iter()
                        .flatten()
                        .filter_map(|&(_, value)| value),
                )
                .chain(info.field_values.iter().flatten().copied())
        })
        .filter(|&v| v != NO_NODE)
}

/// Every scalar-replacement field value in `sr` -- `sr.objects[..].field_values`
/// with `None` and `NO_NODE` skipped. The ONE walk both pin sets are built
/// from: `plan_slots`' third `SlotClass::Pinned` population, and
/// `regalloc::LiveModel::pin_scalar_replacement_fields`. May repeat an id (two
/// objects can hold one value); both consumers are idempotent.
///
/// Also every value an eliminated store wrote (`store_fields`), since round 11
/// wave 6: a deopt between two stores of a fresh object describes each field
/// by its last RUN store (`Lowerer::field_values_after_run_stores`), which can
/// be an earlier value than the field's final one, so it needs a home too.
pub(crate) fn scalar_replacement_field_values(
    sr: &ScalarReplacementMap,
) -> impl Iterator<Item = NodeId> + '_ {
    sr.objects
        .values()
        .flat_map(|info| {
            info.field_values
                .iter()
                .flatten()
                .copied()
                .chain(
                    info.store_fields
                        .iter()
                        .flatten()
                        .filter_map(|&(_, value)| value),
                )
                .chain(info.loop_phis.iter().flatten().map(|&(_, phi)| phi))
        })
        .filter(|&v| v != NO_NODE)
}

pub(in crate::ir_lower) fn hidden_frame_values(graph: &Graph, schedule: &Schedule, sr: Option<&ScalarReplacementMap>) -> Vec<NodeId> {
    sr.into_iter().flat_map(scalar_replacement_field_values)
        .chain(promoted_pending_slot_pins(graph, schedule)).collect()
}

/// `CRATONVM_JIT_IR_CHAIN_SNAPSHOT_RANGE_PINS` -- **default ON** (round 14
/// wave 1, lane chain; round 14 section of
/// `r13w11-chain5-chain-arm-hashmap-kernel-slower-than-default-FIXED-20261001.md`),
/// `0` pins every value a chain snapshot names, as before. Inert on a graph
/// with no chain snapshot, i.e. everywhere outside
/// `CRATONVM_JIT_IR_SPLICE_FRAME_STATES=1`.
///
/// A chain snapshot is recorded at every instruction of a spliced body and
/// names the callee's locals and stack plus every caller scope's. Pinning all
/// of them (`SlotClass::Pinned`: a word nothing else ever takes) gave each
/// spliced temporary its own frame word for the whole method, where the
/// default arm (no snapshot inside a splice) let them share. A chain arm
/// `HashMap.putVal` body came out 2.5x the default's code (76 847 bytes
/// against 30 035). The likely mechanism (confirm with
/// `CRATONVM_DBG_IR_RELOC=1`): past `FRAME_BLOCK_MAX_HOMES` reference homes a
/// body publishes its roots per CALL (push and copy back every home at every
/// GC point) instead of once per activation.
///
/// Sound because a chain snapshot has exactly one reader: the box of a guard
/// lowered at the snapshot's own raw pc (`Lowerer::splice_chain_for_site`,
/// from `emit_deopt_unless` and the `Op::Guard` arm; `build_deopt_points`
/// skips every chain snapshot). [`chain_read_colour_ranges`] makes every
/// scheduled node that could be such a guard -- any node at that pc, an
/// `Op::Guard` whose `bci` is that pc, a node naming the snapshot -- a use of
/// every value the snapshot names, through the same backward dataflow the
/// colouring uses, so a word is never handed to another value while a guard
/// that could publish it is still ahead. Read per compile.
pub(in crate::ir_lower) fn ir_chain_snapshot_range_pins_enabled() -> bool {
    true
}

/// Round 14 wave 1 (lane chain): the colouring ranges of [`plan_slots`] with
/// every `candidate` value (named by a chain snapshot and by nothing that pins
/// it) widened over the chain snapshots' readers. See
/// [`ir_chain_snapshot_range_pins_enabled`] for which nodes read one. Every
/// other entry is `range`'s. `None` when the dataflow does not settle; the
/// caller then pins the candidates.
pub(in crate::ir_lower) fn chain_read_colour_ranges(
    graph: &Graph,
    schedule: &Schedule,
    pos_of: &[Option<usize>],
    span: &[(usize, usize)],
    candidate: &[bool],
    range: &[Option<LiveRange>],
) -> Option<Vec<Option<LiveRange>>> {
    chain_read_colour_ranges_with(
        graph,
        schedule,
        pos_of,
        span,
        candidate,
        range,
        SLOT_PLAN_MAX_ITERATIONS,
        crate::regalloc::legacy_liveness_fixed_point(schedule.blocks.len()),
        None,
    )
}

/// [`chain_read_colour_ranges`] with the iteration cap and the fixed point
/// explicit, and the fixed point's raw result in `trace` when asked for
/// (interpreter round i2 wave 70, lane L2,
/// `CRATONVM_JIT_IR_LEGACY_LIVENESS_WORKLIST`). The round-robin arm is the
/// legacy loop, unchanged; the worklist arm passes an all-zero φ-out set,
/// which is this copy's transfer (`scratch.fill(0)`).
#[allow(clippy::too_many_arguments)]
pub(in crate::ir_lower) fn chain_read_colour_ranges_with(
    graph: &Graph,
    schedule: &Schedule,
    pos_of: &[Option<usize>],
    span: &[(usize, usize)],
    candidate: &[bool],
    range: &[Option<LiveRange>],
    max_iterations: usize,
    fixed_point: crate::regalloc::LivenessFixedPoint,
    trace: Option<&mut crate::regalloc::LsLegacyRun>,
) -> Option<Vec<Option<LiveRange>>> {
    let n = graph.nodes.len();
    let nb = schedule.blocks.len();
    if span.len() != nb || candidate.len() != n || range.len() != n {
        return None;
    }
    let mut out: Vec<Option<LiveRange>> = range.to_vec();
    // Compact bit index per candidate value.
    let mut index: Vec<usize> = vec![usize::MAX; n];
    let mut values: Vec<usize> = Vec::new();
    for (id, &c) in candidate.iter().enumerate() {
        if c {
            index[id] = values.len();
            values.push(id);
        }
    }
    if values.is_empty() {
        return Some(out);
    }
    let mut by_pc: HashMap<usize, Vec<usize>> = HashMap::new();
    for (si, sp) in graph.safepoints.iter().enumerate() {
        if sp.is_guard_splice_state() {
            by_pc.entry(sp.bci).or_default().push(si);
        }
    }
    let words = values.len().div_ceil(64).max(1);
    let mut use_bits = vec![0u64; words * nb];
    let mut def_bits = vec![0u64; words * nb];
    let mut lo: Vec<usize> = values
        .iter()
        .map(|&v| range[v].map_or(usize::MAX, |r| r.lo))
        .collect();
    let mut hi: Vec<usize> = values
        .iter()
        .map(|&v| range[v].map_or(0, |r| r.hi))
        .collect();
    let mut local_def = vec![0u64; words];
    let mut sites: Vec<usize> = Vec::new();
    // JIT round 21 wave 1 (lane keepset), once per colouring.
    let latch_exit_reads = true;
    for (b, block) in schedule.blocks.iter().enumerate() {
        let base = b * words;
        local_def.fill(0);
        for u in block.nodes.iter().copied().chain(block.terminator) {
            let ui = u as usize;
            let Some(node) = graph.nodes.get(ui) else {
                continue;
            };
            if let (false, Some(upos)) = (node.op == Op::Dead, pos_of.get(ui).copied().flatten()) {
                // Every chain snapshot a deopt planted while lowering `u` could
                // publish: at its own pc, at an `Op::Guard`'s `bci`, or the
                // one it names. A superset of `splice_chain_snapshot_for_site`.
                sites.clear();
                if let Some(at) = node.bytecode_pc.and_then(|pc| by_pc.get(&pc)) {
                    sites.extend_from_slice(at);
                }
                if let Op::Guard { bci } = node.op {
                    if let Some(at) = by_pc.get(&bci) {
                        sites.extend_from_slice(at);
                    }
                }
                if let Some(si) = node.frame_snapshot {
                    let si = si as usize;
                    if graph
                        .safepoints
                        .get(si)
                        .is_some_and(|s| s.is_splice_state())
                    {
                        sites.push(si);
                    }
                }
                for &si in &sites {
                    let Some(sp) = graph.safepoints.get(si) else {
                        continue;
                    };
                    for &v in sp.locals.iter().chain(sp.stack.iter()) {
                        let vi = v as usize;
                        if v == NO_NODE || vi == ui {
                            continue;
                        }
                        let k = match index.get(vi) {
                            Some(&k) if k != usize::MAX => k,
                            _ => continue,
                        };
                        lo[k] = lo[k].min(upos);
                        hi[k] = hi[k].max(upos);
                        if !bits_contains(&local_def, k) {
                            bits_insert(&mut use_bits[base..base + words], k);
                        }
                    }
                }
            }
            if let Some(&k) = index.get(ui) {
                if k != usize::MAX {
                    bits_insert(&mut local_def, k);
                    bits_insert(&mut def_bits[base..base + words], k);
                }
            }
        }
        // JIT round 21 wave 1 (lane keepset): the spliced back-edge mode exit
        // of this latch reads the header's chain snapshot at the latch's edge
        // position (`spliced_latch_exit_reads`; see the section above
        // `ir_spliced_latch_exit_reads_enabled`). Byte-identical whenever the
        // header has a φ at its pc (already a reader of every value defined
        // before the loop); a φ-less spliced loop gets the read it lacked.
        if latch_exit_reads {
            if let Some((_, read)) = spliced_latch_exit_reads(graph, schedule, b, &by_pc, None) {
                let at = span[b].1;
                for v in read {
                    let k = match index.get(v as usize) {
                        Some(&k) if k != usize::MAX => k,
                        _ => continue,
                    };
                    lo[k] = lo[k].min(at);
                    hi[k] = hi[k].max(at);
                    if !bits_contains(&local_def, k) {
                        bits_insert(&mut use_bits[base..base + words], k);
                    }
                }
            }
        }
    }
    // The backward fixed point of `plan_slots`' step 4, over these values and
    // these reads only (their ordinary uses are already in `range`).
    let mut live_in = vec![0u64; words * nb];
    let mut live_out = vec![0u64; words * nb];
    let mut visits = 0usize;
    let converged = match fixed_point {
        crate::regalloc::LivenessFixedPoint::RoundRobin => {
            let mut scratch = vec![0u64; words];
            let mut converged = false;
            for _ in 0..max_iterations {
                let mut changed = false;
                visits += nb;
                for b in (0..nb).rev() {
                    let base = b * words;
                    scratch.fill(0);
                    for &s in &schedule.blocks[b].successors {
                        if s >= nb {
                            continue;
                        }
                        for w in 0..words {
                            scratch[w] |= live_in[s * words + w];
                        }
                    }
                    for w in 0..words {
                        let merged_out = live_out[base + w] | scratch[w];
                        if merged_out != live_out[base + w] {
                            live_out[base + w] = merged_out;
                            changed = true;
                        }
                        let entering = use_bits[base + w] | (merged_out & !def_bits[base + w]);
                        let merged_in = live_in[base + w] | entering;
                        if merged_in != live_in[base + w] {
                            live_in[base + w] = merged_in;
                            changed = true;
                        }
                    }
                }
                if !changed {
                    converged = true;
                    break;
                }
            }
            converged
        }
        // Interpreter round i2 wave 70, lane L2: the same answer, pass for
        // pass, visiting only dirty blocks. No φ-out term here: zeros.
        // Wave 71: the predecessor lists per `fixed_point`.
        crate::regalloc::LivenessFixedPoint::Worklist
        | crate::regalloc::LivenessFixedPoint::WorklistSchedulePreds
        | crate::regalloc::LivenessFixedPoint::WorklistPostorder { .. }
        | crate::regalloc::LivenessFixedPoint::RoundRobinDriver => {
            let no_phi_out = vec![0u64; words * nb];
            crate::regalloc::ls_fixed_point_on_schedule(
                schedule.blocks.as_slice(),
                fixed_point,
                crate::regalloc::LsFixedPointSets {
                    nb,
                    words,
                    phi_out: &no_phi_out,
                    uses: &use_bits,
                    defs: &def_bits,
                },
                &mut live_in,
                &mut live_out,
                max_iterations,
                &mut visits,
            )
        }
    };
    if let Some(trace) = trace {
        *trace = crate::regalloc::LsLegacyRun {
            ran: true,
            converged,
            live_in: live_in.clone(),
            live_out: live_out.clone(),
            visits,
        };
    }
    if !converged {
        return None;
    }
    for (b, &(start, end)) in span.iter().enumerate() {
        let base = b * words;
        bits_for_each(&live_in[base..base + words], |k| {
            if let Some(l) = lo.get_mut(k) {
                *l = (*l).min(start);
            }
        });
        bits_for_each(&live_out[base..base + words], |k| {
            if let Some(h) = hi.get_mut(k) {
                *h = (*h).max(end);
            }
        });
    }
    for (k, &v) in values.iter().enumerate() {
        if lo[k] == usize::MAX {
            continue;
        }
        out[v] = Some(LiveRange {
            lo: lo[k],
            hi: hi[k].max(lo[k]),
        });
    }
    Some(out)
}

// ── Dead reference homes at a call (gen r5w2/oomjit6) ────────────────
//
// An IR frame keeps every reference in a home word (see the relocation
// contract above `emit_sp_id_store`), and nothing ever clears one. A value the
// program is done with therefore stays in its frame word until another value
// of the same colour overwrites it -- and a value any deopt snapshot names is
// `SlotClass::Pinned`, a colour NOTHING else ever takes, so its word keeps the
// value for the rest of the activation. The collector's band scan is
// conservative over the whole spill band below the watermark, so each such
// word is a GC root for as long as the frame is live.
//
// That is the shape `GenR4W6JitOomRootProbe` measures on every case since the
// optimizing tier took over OSR bodies with exception exits (JIT r11 wave 11,
// `CRATONVM_JIT_OSR_OPTIMIZING_EXC_EXITS`): each `case*` method is entered
// once and OSR'd in its warm-up loop, so its tail -- `stagedData = blocks(16);
// ref = new WeakReference<>(stagedData); verdict(.., check(ref, true))` -- runs
// in an OPTIMIZING OSR frame. The list is dead at the `check` call (HotSpot's
// oop map there does not name it), but its `getstatic` value and the
// `blocks(16)` result are both on the operand stack at some bci, hence pinned,
// hence still in two frame words of a frame that is live across the whole
// check. The single-pass OSR body it replaced dropped such words through its
// spill cursor (`live_frame_hi`) and register-oop mask; the IR tier publishes
// neither claim (`reg_oop_mask: None`, and its `live_frame_hi` is a
// monotone watermark).
//
// # What is cleared, and why that is sound
//
// At a GC point emitted while lowering an `Op::Call` (gen r5w5/oomjit9: inside
// a splice too, at the bci of its outermost enclosing own invoke), every
// colour that can hold a reference is zeroed unless it is KEPT. A colour is
// kept when
//
//   * a value of that colour has a live range covering the call's position
//     (its uses in compiled code -- the same over-approximation the colouring
//     trusts not to alias two live values), or
//   * a value of that colour is named by a snapshot whose bci is reachable
//     from the call's bci over the method's normal bytecode edges (the call's
//     own bci included). Every deopt, rethrow pad, OSR exit and mode exit this
//     activation can still take after the call consumes a snapshot at such a
//     bci: an IR body contains no handler code (a throw leaves the body), and
//     `Op::Call` is control-pinned, so nothing that executes after it belongs
//     to a bytecode before it.
//
// A colour that is neither holds no value compiled code or a frame-state
// reader will read again before a later definition writes it. Zeroing it can
// only turn a word the scan would have rooted into a null. Phis are never
// cleared (their slot is written by edge copies at positions outside the
// phi's own range -- gen r5w6/oomjit10: except under the default precise keep
// set, `ir_precise_keep_set_enabled`, which records those copies), nor is a
// scalar-replaced object's field value (it is read
// by a materialisation this analysis does not place; gen r5w5/oomjit9 keeps
// those colours everywhere instead of refusing the whole method, as it did).
//
// Default ON since gcf vf5 (`CRATONVM_JIT_IR_CLEAR_DEAD_REF_SLOTS=0` turns it
// off): it adds a store per dead reference word per call site to compiled
// code. vf5 measured R10SelfRecCatch at +2 % median (5 interleaved pairs,
// inside the host's floor), and it fixed `oome-throwable-kept` 6/6. Read per compile, no static
// (`process_global_statics_ratchet`).





/// `CRATONVM_JIT_IR_INLINE_BYTE_ARRAYCOPY` (JIT r23 wave 12, lane L6; perf;
/// its own key `=1` / `=0`, unset it follows the row-bound splice below, which
/// is default ON: neutral alone on the n23-w12a A/B, `W8L6SplitParts` cached
/// / full / comma / list within noise of off): the `System.arraycopy` prefix copies a
/// `byte[]` / `boolean[]` range between two different arrays inline
/// (`emit_ir_byte_arraycopy_arm`) instead of calling the bound helper. Read
/// per compile.
pub(crate) fn ir_inline_byte_arraycopy_enabled() -> bool {
    // JIT r25 wave 13 (W13-2): unset, the inline copy follows
    // `CRATONVM_JIT_IR_SPLICE_ARRAYCOPY_ROW_BOUND`. c30: the splice that switch
    // admits LOST 22-27 % alone (split / groups / split-ws / cached) and won
    // 3-6 % with this copy; the two are one arm.
    if None::<std::ffi::OsString>.is_none() {
        return crate::r25_w11_2_arraycopy_row::enabled();
    }
    false
}

/// The copy loops of `emit_ir_byte_arraycopy_arm`. Entry: R10 = source byte
/// address, RCX = destination byte address, R11 = byte count >= 0. Exit: the
/// last byte copied. Clobbers RAX, RCX, R10, R11 and the flags. Eight bytes
/// at a time, then single bytes; forward (the arm admits two different arrays
/// only, which never overlap).
///
/// ```text
///  0: SUB R11, 8          ; 49 83 EB 08
///  4: JB  tail (26)       ; 72 14
///  6: MOV RAX, [R10]      ; 49 8B 02       top8
///  9: MOV [RCX], RAX      ; 48 89 01
/// 12: ADD R10, 8          ; 49 83 C2 08
/// 16: ADD RCX, 8          ; 48 83 C1 08
/// 20: SUB R11, 8          ; 49 83 EB 08
/// 24: JAE top8 (6)        ; 73 EC
/// 26: ADD R11, 8          ; 49 83 C3 08    tail: R11 = bytes left (0..7)
/// 30: JZ  done (48)       ; 74 10
/// 32: MOV AL, [R10]       ; 41 8A 02       top1
/// 35: MOV [RCX], AL       ; 88 01
/// 37: INC R10             ; 49 FF C2
/// 40: INC RCX             ; 48 FF C1
/// 43: DEC R11             ; 49 FF CB
/// 46: JNZ top1 (32)       ; 75 F0
/// 48:                                      done
/// ```
pub(in crate::ir_lower) const BYTE_ARRAYCOPY_LOOPS: [u8; 48] = [
    0x49, 0x83, 0xEB, 0x08, // SUB R11, 8
    0x72, 0x14, // JB tail
    0x49, 0x8B, 0x02, // MOV RAX, [R10]
    0x48, 0x89, 0x01, // MOV [RCX], RAX
    0x49, 0x83, 0xC2, 0x08, // ADD R10, 8
    0x48, 0x83, 0xC1, 0x08, // ADD RCX, 8
    0x49, 0x83, 0xEB, 0x08, // SUB R11, 8
    0x73, 0xEC, // JAE top8
    0x49, 0x83, 0xC3, 0x08, // ADD R11, 8
    0x74, 0x10, // JZ done
    0x41, 0x8A, 0x02, // MOV AL, [R10]
    0x88, 0x01, // MOV [RCX], AL
    0x49, 0xFF, 0xC2, // INC R10
    0x48, 0xFF, 0xC1, // INC RCX
    0x49, 0xFF, 0xCB, // DEC R11
    0x75, 0xF0, // JNZ top1
];


/// `CRATONVM_JIT_IR_INLINE_REF_ARRAYCOPY_CHECKED` (JIT r23 wave 11, lane L6;
/// perf, **default ON** since n23-w11a, `=0` kill; win on W8L6SplitParts
/// `list` 424/267/350 vs 550/536/618 ns/op, `cached` and `full` also lower):
/// the inline reference arraycopy prefix also copies
/// a range whose destination component differs from the source's, proving
/// each element by an exact class-id compare
/// (`emit_ir_ref_arraycopy_checked_arm`). The first unproven element declines
/// to the bound helper. Read per compile.
pub(crate) fn ir_inline_ref_arraycopy_checked_enabled() -> bool {
    true
}

/// The checked arm's `JZ rel8` over a non-null element's proof:
/// `TEST BYTE [RAX+disp8], imm8` (4) + `JNZ rel32` (6) + `CMP EDX, [RAX]` (2) +
/// `JNE rel32` (6).
pub(in crate::ir_lower) const CHECKED_COPY_NULL_SKIP: u8 = 4 + 6 + 2 + 6;



/// `CRATONVM_JIT_IR_INLINE_REFLECT_NEW_ARRAY` (JIT r23 wave 8, lane L8; perf,
/// default ON, `=0` restores the bound `jit_reflect_new_array_direct` call):
/// the optimizing tier's inline `Array.newArray(Class, int)` prefix
/// ([`Lowerer::try_emit_ir_reflect_new_array_prefix`]). Read per site at
/// compile time; no static.
pub(in crate::ir_lower) fn ir_inline_reflect_new_array_enabled() -> bool {
    true
}

/// Is `class.method descriptor` (invoke kind `kind`) the one site
/// [`Lowerer::try_emit_ir_reflect_new_array_prefix`] answers --
/// `invokestatic java/lang/reflect/Array.newArray(Class, int)`, the native
/// behind `Array.newInstance`, `Arrays.copyOf(T[], int)` and
/// `Collection.toArray(T[])`?
pub(in crate::ir_lower) fn reflect_new_array_prefix_site(class: &str, method: &str, descriptor: &str, kind: u8) -> bool {
    kind == 3
        && class == "java/lang/reflect/Array"
        && method == "newArray"
        && descriptor == "(Ljava/lang/Class;I)Ljava/lang/Object;"
}





// ── Round 16 wave 7 (lane frames): what the fresh-frame clear may leave out ──
//
// `docs/feature-designs/jit-ir-optimizations.md`
// has the whole argument; the short form, per switch:
//
// * `_SKIP_UNROOTED`: the frame-deopt `SavedRegisters` XMM half (16 words,
//   `FrameLayout::deopt_xmm_lo..hi`). The default band scan
//   (`conservative_roots::scan_one_frame_filtered`) never makes a word there a
//   root (`roots = !xmm_image && ..`), never captures a G1 pin for it (only a
//   would-be root owes one), and publishes an object-start word there MOVABLE
//   (`publish_validated_band_word`), never unrewritable; the half is outside
//   the verifiable band (`callee_saved_lo` half-line). Its one reader,
//   `ir_deopt_entry` via `deopt::try_resolve_value`'s `XmmFloat` /
//   `XmmDouble` arms, runs only after this body's deopt stub stored all
//   sixteen words (`emit_deopt_stubs`, the `emit_movsd_frame_from_xmm` loop).
// * `_SKIP_WRITTEN`: the words the prologue itself stores before anything can
//   stop the thread cooperatively -- the callee-saved XMM and GPR saves it
//   makes, the context word and every incoming parameter word. Every
//   safepoint, call and deopt of the body comes after those stores, so every
//   cooperative scan reads exactly what it read with the clear.
//
// Neither is the reference-free ("lean") frame the round-16 handoff asked
// about: such a frame's band is still scanned conservatively, every word of
// it, and so it needs the clear exactly as much as any other (see the page).

/// `CRATONVM_JIT_IR_ZERO_FRESH_FRAMES_SKIP_UNROOTED` -- the fresh-frame clear
/// (SSE / AVX form) leaves the deopt XMM image out (see the section above).
/// Lane frames, round 16 wave 7; default ON since wave 8 (R16Bench, Linux:
/// with the other two, bintrees 98 -> 90, poly 1487 -> 1360 ms), `=0` is the
/// kill switch. Read per compile; no static.
pub(in crate::ir_lower) fn ir_zero_fresh_frames_skip_unrooted_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_ZERO_FRESH_FRAMES_SKIP_WRITTEN` -- the fresh-frame clear
/// (SSE / AVX form) leaves out the words the prologue (or the OSR entry stub)
/// writes before its first possible stop (see the section above). Lane
/// frames, round 16 wave 7; default ON since wave 8, `=0` is the kill switch.
/// Read per compile; no static.
pub(in crate::ir_lower) fn ir_zero_fresh_frames_skip_written_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_PRIMITIVE_COLOUR_CLAIM` -- JIT round 25, item 3 of the
/// round-24 redesign (`docs/feature-designs/regex-jit-contract.md`),
/// default ON since R25-FLIP, `=0` kills. An optimizing-tier body publishes its `Prim` colours' offsets as a
/// per-body claim (`CompiledMethod::ir_prim_colour_claim`), the collector's
/// band scan honours it at every poll -- also where no safepoint id is
/// current, which is what kept the whole frame in the clear -- and the
/// fresh-frame clear (prologue and OSR entry stubs) leaves those words out
/// (`Lowerer::fresh_frame_clear_skip_with`). Under the switch the colouring
/// also numbers every `Prim` colour before every rootable one
/// (`SlotPlan::group_prim_colours_first`), so the words left out form one
/// run. Read once per compile ([`Lowerer::new`]; the grouping reads it once
/// more, before the plan is verified -- either answer alone is sound); the
/// collector trusts only the published claim. No static.
pub(in crate::ir_lower) fn ir_primitive_colour_claim_enabled() -> bool {
    // R25-FLIP: default ON; `=0` is the kill switch.
    true
}

// ── JIT round 25 wave 2: smaller optimizing-tier frames, smaller clears ──
//
// Page: `docs/feature-designs/real-frame-deopt.md`. An
// optimizing-tier frame is, from `rbp` down: the Java-local words, the context
// word, five bookkeeping words, one word per colour (`SlotPlan::slots`), the
// callee-saved save bands, the 256-byte deopt `SavedRegisters` image (GPR half
// deepest), the argument staging, the 16-byte stack-argument reserve and the
// 32-byte ABI shadow (`Lowerer::new`). The fresh-frame clear zeroed all of it
// but the deopt XMM half, the prologue's own writes and (item 3) the `Prim`
// colours. Three switches (opt-in when written, default ON since j27
// 2026-10-28) take more out, each with its own argument:
//
// * `CRATONVM_JIT_IR_PINNED_PRIM_COLOUR_CLAIM` (with item 3's claim): the
//   `Pinned` colours of non-reference values ([`claimable_pinned_nodes`]) join
//   the per-body claim. Every value a safepoint snapshot names is pinned
//   (`plan_slots_from_core`, step 3), so on a guard-heavy body most of the
//   colour band is pinned, and item 3 claimed none of it.
// * `CRATONVM_JIT_IR_DEOPT_IMAGE_CLAIM` (with item 3's claim): the deopt GPR
//   image joins the per-body claim. Its only writer is the body's shared deopt
//   stub and its only reader `ir_deopt_entry`, inside one call that reaches no
//   GC point (`conservative_roots.rs`, the gcg g1/f section, whose default-ON
//   never-roots claim already drops the block at every scan); so at every
//   point the band scan can see the frame the block is residue or a finished
//   deopt's leftovers, and zeroing it per call (128 bytes) buys nothing.
// * `CRATONVM_JIT_IR_FRAME_PARAM_LOCALS_ONLY`: the frame reserves Java-local
//   words for the parameters only ([`ir_frame_locals`]). This tier never
//   writes or reads a local word past the parameters (every local is an SSA
//   value with a coloured home; `alias_param_home`), so those words were
//   reserved, zeroed and scanned for nothing.

/// `CRATONVM_JIT_IR_PINNED_PRIM_COLOUR_CLAIM` -- JIT round 25 wave 2,
/// default ON since j27 2026-10-28 (`=0` kills), and inert unless `CRATONVM_JIT_IR_PRIMITIVE_COLOUR_CLAIM` is on too:
/// the `Pinned` colours [`claimable_pinned_nodes`] proves never hold a
/// reference join the per-body claim (`CompiledMethod::ir_prim_colour_claim`),
/// leave the fresh-frame clear, and are numbered with the `Prim` colours at the
/// shallow end of the band (`SlotPlan::group_claimed_colours_first`). Read per
/// compile (the grouping and [`Lowerer::new`] each read it once; either answer
/// alone is sound, the grouping only orders). No static.
pub(in crate::ir_lower) fn ir_pinned_prim_colour_claim_enabled() -> bool {
    // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
    true
}

/// `CRATONVM_JIT_IR_DEOPT_IMAGE_CLAIM` -- JIT round 25 wave 2, default ON
/// since j27 2026-10-28 (`=0` kills), and inert unless `CRATONVM_JIT_IR_PRIMITIVE_COLOUR_CLAIM` is on too: the words
/// of the deopt GPR image join the per-body claim and leave the fresh-frame
/// clear (see the section above). Read once per compile in [`Lowerer::new`].
/// No static.
pub(in crate::ir_lower) fn ir_deopt_image_claim_enabled() -> bool {
    // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
    true
}

/// `CRATONVM_JIT_IR_FRAME_PARAM_LOCALS_ONLY` -- JIT round 25 wave 2, default
/// ON (default ON since j27 2026-10-28), `=0` kills:
/// reserve Java-local words for the parameters only ([`ir_frame_locals`]).
/// Read once per compile, in `lower_inner_attempt`, which sizes the frame
/// bound and builds the lowerer from the same answer. No static.
pub(in crate::ir_lower) fn ir_frame_param_locals_only_enabled() -> bool {
    // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
    true
}

/// The Java-local words an optimizing-tier frame reserves for a method of
/// `num_params` incoming arguments and `num_locals` (`max_locals`) locals:
/// `num_locals` as always, or, with `param_only`, just enough for every
/// parameter word -- `num_params`, or one past the highest `Op::Param` index
/// the graph holds if that is more (a hand-built graph may name one), and never
/// more than `num_locals`.
///
/// Why the words past the parameters can go: this tier stores into a
/// Java-local word only in the prologue (`store_abi_reg` to `local_offset(i)`,
/// `i < num_params`) and reads one only as a parameter's home
/// (`alias_param_home`, `frame_value_for`'s `Op::Param` arm, the monitor
/// home); every other local is an SSA value homed in a colour. The words past
/// the parameters were only zeroed (prologue, OSR entry stubs) and scanned.
/// Every offset below them (context, bookkeeping, colours, ...) is derived from
/// this count in `Lowerer::new`, and the published `java_locals_hi` from the
/// same `locals_size`, so the collector's view moves with the frame.
pub(in crate::ir_lower) fn ir_frame_locals(graph: &Graph, num_params: usize, num_locals: usize, param_only: bool) -> usize {
    if !param_only {
        return num_locals;
    }
    let highest_param = graph
        .nodes
        .iter()
        .filter_map(|n| match n.op {
            Op::Param(idx) => Some(usize::from(idx) + 1),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    num_params.max(highest_param).min(num_locals)
}

/// Colour `c`'s frame offset, `[rbp - (first_spill + 8c)]` (the arithmetic of
/// `planned_slot_off`), for every colour of `colours`, ascending and distinct;
/// an offset past `i16` is dropped rather than truncated, as for
/// [`Lowerer::prim_slot_offsets`].
pub(in crate::ir_lower) fn colour_frame_offsets(colours: &[u32], first_spill: i32) -> Vec<i16> {
    let mut offs: Vec<i16> = colours
        .iter()
        .filter_map(|&c| {
            i32::try_from(u64::from(c).saturating_mul(8))
                .ok()
                .and_then(|d| first_spill.checked_add(d))
                .and_then(|o| i16::try_from(o).ok())
        })
        .collect();
    offs.sort_unstable();
    offs.dedup();
    offs
}

/// JIT round 25 wave 2 (`CRATONVM_JIT_IR_PINNED_PRIM_COLOUR_CLAIM`): per node,
/// whether its `Pinned` colour provably never holds a reference of this
/// activation, so the per-body claim may name it.
///
/// A `Pinned` colour has exactly one occupant (`verify_slot_colouring`), and is
/// written only by that node's home store, its OSR seed, a zeroing clear and --
/// for a phi -- the edge copies of the phi's inputs. So the colour holds only
/// values of that node, and a node qualifies when:
///
/// * its type is `int`, `long`, `float` or `double` (never `Ref`, never a
///   token), or it is a `Memory` phi, which no copy, seed or snapshot writes;
/// * it is no monitor object (named in any snapshot's `monitors`, or an input
///   of a `monitorenter` / `monitorexit`): a monitor is re-spelt as a
///   reference by the deopt metadata (`monitor_object_value`), and hand-built
///   graphs type `this` as `int`;
/// * for a phi, every typed value input is a primitive and no input is a phi
///   that fails this rule (a fixed point): a phi whose inputs do not join
///   takes `PHI_TYPE_FALLBACK` (`int`) while an edge copy still stores the
///   `Ref` input into its home, so the declared type of a phi alone is not
///   proof. A phi with no typed value input at all is left out too.
///
/// Anything else answers `false`, which keeps the word cleared and scanned as
/// before. This is the item-3 argument (a `Prim` colour never holds a
/// reference) extended to the one-occupant colours, with the phi caveat that
/// item 3 never met (it never claimed a phi: every phi is pinned).
pub(in crate::ir_lower) fn claimable_pinned_nodes(graph: &Graph, plan: &SlotPlan) -> Vec<bool> {
    let n = graph.nodes.len();
    let mut out = vec![false; n];
    let prim_ty =
        |t: IrType| matches!(t, IrType::Int | IrType::Long | IrType::Float | IrType::Double);
    let mut monitor = vec![false; n];
    let monitor_operands = graph
        .safepoints
        .iter()
        .flat_map(|sp| sp.monitors.iter().copied())
        .chain(
            graph
                .nodes
                .iter()
                .filter(|node| matches!(node.op, Op::MonitorEnter | Op::MonitorExit))
                .flat_map(|node| node.inputs.iter().copied()),
        );
    for v in monitor_operands {
        if v != NO_NODE {
            if let Some(m) = monitor.get_mut(v as usize) {
                *m = true;
            }
        }
    }
    // Phis that may hold a reference whatever their declared type, as a
    // monotone fixed point (each pass marks at least one more or stops).
    let mut tainted = vec![false; n];
    for _ in 0..=n {
        let mut changed = false;
        for (id, node) in graph.nodes.iter().enumerate() {
            if node.op != Op::Phi || tainted[id] {
                continue;
            }
            let mut typed = false;
            let mut bad = !prim_ty(node.ty);
            if !bad {
                for &inp in node.inputs.iter().skip(1) {
                    let Some(src) = graph.nodes.get(inp as usize) else {
                        continue;
                    };
                    if src.op == Op::Dead || src.ty == IrType::Void {
                        continue;
                    }
                    if !prim_ty(src.ty) || tainted.get(inp as usize).copied().unwrap_or(true) {
                        bad = true;
                        break;
                    }
                    typed = true;
                }
            }
            if bad || !typed {
                tainted[id] = true;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    for (id, node) in graph.nodes.iter().enumerate() {
        // A memory phi has a colour (every phi wants one) but no machine value:
        // `phis_by_merge` drops it from the edge copies, no snapshot or OSR
        // seed names it, so nothing but the clear ever writes its home.
        let memory_phi = node.op == Op::Phi && node.ty == IrType::Memory;
        if plan.class.get(id).copied().flatten() != Some(SlotClass::Pinned)
            || node.op == Op::Dead
            || !(prim_ty(node.ty) || memory_phi)
            || monitor[id]
            || (node.op == Op::Phi && !memory_phi && tainted[id])
        {
            continue;
        }
        out[id] = true;
    }
    out
}

// ── JIT round 25 wave 3: snapshot-named values coloured by range ─────
//
// Page: `docs/feature-designs/real-frame-deopt.md`, "Wave 3".
// `plan_slots_from_core` (step 3) pins every value a safepoint snapshot names:
// a `Pinned` colour has one occupant for the whole method, so on a guard- and
// call-heavy body most of the colour band shares nothing (`Greedy.match`:
// about 67 colours). Pinning stood in for "every reader of the snapshot finds
// the value in its home", for readers the colouring never modelled.
//
// Under `CRATONVM_JIT_IR_SNAPSHOT_RANGE_COLOURS` a non-reference value pinned
// for that reason alone is coloured like any other `Prim` value instead, over
// a colouring range that the dataflow widens with every position the plan
// treats as reading it through a snapshot ([`apply_snapshot_range_colours`]):
//
// * every scheduled node or terminator reads the snapshots at its own pc, at
//   that pc's resume bci (a site inside a splice resumes at its invoke), at the
//   bci an `Op::Guard` names (raw and resumed), the snapshot it names
//   (`frame_snapshot`), and the snapshots at the next recorded bci after its
//   pc (a post-site exit resumes at the successor's state);
// * every block's outgoing edge -- and its terminator -- reads the snapshots at
//   each successor's bci (a back-edge poll's mode exit takes the header's
//   state there);
// * every block start reads the snapshots at its control node's bci, and an
//   opaque OSR arm's start the header's (an OSR entry stub seeds and describes
//   the value there).
//
// References stay pinned (the GC-side claims and dead-home clears reason about
// `Ref` colours separately), and so do phis, parameters, constants, monitor
// objects and scalar-replacement field values.
//
// The rules are a model, so the lowerer CHECKS them: every frame-state read of
// a ranged value by its home ([`Lowerer::snapshot_ranged_home_ok`], from
// `frame_value_for`) must happen at a position the plan recorded for that
// value, and every OSR seed of one at a bci recorded for it. Anything else --
// an unmodelled reader, a pad built after the body -- refuses the attempt, and
// the ladder lowers once more with the ranging off (the keep-everything
// retry, `keep_deopt_image`). A miss costs a lowering, never a value.

/// `CRATONVM_JIT_IR_SNAPSHOT_RANGE_COLOURS` -- JIT round 25 wave 3, default
/// ON (default ON since j27 2026-10-28), `=0` kills:
/// colour snapshot-named non-reference values by range instead of pinning
/// them (see the section above). Read once per lowering attempt in
/// `lower_inner_attempt`; the keep-everything retry never ranges. No static.
pub(in crate::ir_lower) fn ir_snapshot_range_colours_enabled() -> bool {
    // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
    true
}

/// The helper-argument area at the deep end of an optimizing-tier frame: the
/// 32-byte ABI shadow plus the 16-byte stack-argument reserve
/// (`Lowerer::new`'s `shadow` and `stack_arg_reserve`; the collector's
/// `IR_HELPER_ARG_AREA_BYTES`).
pub(in crate::ir_lower) const IR_HELPER_AREA_BYTES: i32 = 48;

/// `CRATONVM_JIT_IR_HELPER_AREA_CLAIM` -- JIT round 25 wave 3, default ON
/// since j27 2026-10-28 (`=0` kills), and inert unless `CRATONVM_JIT_IR_PRIMITIVE_COLOUR_CLAIM` is on too, and on
/// Win64: the helper-argument area ([`IR_HELPER_AREA_BYTES`]) joins the
/// per-body claim and leaves the fresh-frame clear.
///
/// Why no word of it ever holds a reference of this activation on System V:
/// the area is `[RSP, RSP + 48)` at every call this body makes, and
///
/// * a System V callee owns no home space in its caller's frame (only Win64
///   has the 32-byte shadow), and its frame starts below the return address
///   the `CALL` pushes below `RSP`;
/// * the only in-frame stack-argument stores this file emits are Win64's
///   (`jit_invoke_virtual_mic`'s fifth and sixth arguments, `#[cfg(target_os =
///   "windows")]`); every other stack-argument store (`emit_ic_wide_marshal`,
///   the post-call exit's verdict tail, the pad write-back spills) is to a
///   block the code first reserves BELOW `RSP` (`SUB RSP`) and releases after;
/// * a PUSH, a signal frame and a callee's frame are all below `RSP`.
///
/// So the words hold only what an earlier, deeper frame left there, and the
/// per-body claim (never root an object start there) is exact. Read once per
/// compile in [`Lowerer::new`]. No static.
pub(in crate::ir_lower) fn ir_helper_area_claim_enabled() -> bool {
    // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
    true
}

/// The slot plan's block spans, `(first position, outgoing-edge position)`,
/// re-derived from the numbering (each scheduled node, the terminator, one
/// outgoing-edge position per block) and checked against `pos_of` node by
/// node, as [`back_edge_poll_pos`] checks it. `None` when the two disagree.
pub(in crate::ir_lower) fn slot_plan_spans(schedule: &Schedule, pos_of: &[Option<usize>]) -> Option<Vec<(usize, usize)>> {
    let mut seq = 0usize;
    let mut spans = Vec::with_capacity(schedule.blocks.len());
    for blk in &schedule.blocks {
        let start = seq;
        for (k, &nid) in blk.nodes.iter().enumerate() {
            if pos_of.get(nid as usize).copied().flatten() != Some(seq + k) {
                return None;
            }
        }
        seq += blk.nodes.len();
        if let Some(term) = blk.terminator {
            if pos_of.get(term as usize).copied().flatten() != Some(seq) {
                return None;
            }
            seq += 1;
        }
        spans.push((start, seq));
        seq += 1;
    }
    Some(spans)
}

/// Step 6 of `plan_slots_from_core` again, over a new pinned set and new
/// colouring ranges: pinned values first, one colour each, in node-id order;
/// then every other value that has a colour now, by range start, recycling
/// the earliest-expiring colour of its class (`Ref` or `Prim`), a fresh-only
/// value never receiving a recycled one. Rewrites `node_color`, `class` and
/// `slots`; `range` and every per-node list stay.
pub(in crate::ir_lower) fn recolour_slot_plan(
    graph: &Graph,
    plan: &mut SlotPlan,
    pinned: &[bool],
    colouring: &[Option<LiveRange>],
) {
    let n = graph.nodes.len();
    let wants: Vec<bool> = (0..n)
        .map(|id| plan.node_color.get(id).copied().flatten().is_some())
        .collect();
    let mut node_color: Vec<Option<u32>> = vec![None; n];
    let mut class: Vec<Option<SlotClass>> = vec![None; n];
    let mut next_color: u32 = 0;
    for id in 0..n {
        if wants[id] && pinned.get(id).copied().unwrap_or(false) {
            node_color[id] = Some(next_color);
            class[id] = Some(SlotClass::Pinned);
            next_color = next_color.saturating_add(1);
        }
    }
    let mut order: Vec<usize> = (0..n)
        .filter(|&id| wants[id] && !pinned.get(id).copied().unwrap_or(false))
        .collect();
    order.sort_by_key(|&id| (colouring.get(id).copied().flatten().map_or(0, |r| r.lo), id));
    let mut active: BinaryHeap<Reverse<(usize, u32, bool)>> = BinaryHeap::new();
    let mut free_ref: Vec<u32> = Vec::new();
    let mut free_prim: Vec<u32> = Vec::new();
    for id in order {
        let Some(r) = colouring.get(id).copied().flatten() else {
            // No range: a colour of its own, as a pinned value has.
            node_color[id] = Some(next_color);
            class[id] = Some(SlotClass::Pinned);
            next_color = next_color.saturating_add(1);
            continue;
        };
        while let Some(&Reverse((active_hi, active_color, active_is_ref))) = active.peek() {
            if active_hi >= r.lo {
                break;
            }
            active.pop();
            if active_is_ref {
                free_ref.push(active_color);
            } else {
                free_prim.push(active_color);
            }
        }
        let node = &graph.nodes[id];
        let is_ref = node.ty == IrType::Ref;
        let recycled = if colour_is_fresh_only(node) {
            None
        } else if is_ref {
            free_ref.pop()
        } else {
            free_prim.pop()
        };
        let color = match recycled {
            Some(color) => color,
            None => {
                let color = next_color;
                next_color = next_color.saturating_add(1);
                color
            }
        };
        node_color[id] = Some(color);
        class[id] = Some(if is_ref {
            SlotClass::Ref
        } else {
            SlotClass::Prim
        });
        active.push(Reverse((r.hi, color, is_ref)));
    }
    plan.node_color = node_color;
    plan.class = class;
    plan.slots = next_color as usize;
}

/// JIT round 25 wave 3 (`CRATONVM_JIT_IR_SNAPSHOT_RANGE_COLOURS`): colour the
/// snapshot-named non-reference values of `plan` by range (see the section
/// above). Returns whether it changed the plan; on `false` the plan is
/// untouched. `spliced_ranges` is the compile's `(start, end, invoke)` table,
/// for the resume bci of a site inside a splice.
///
/// Candidates: a value with a colour, `Pinned`, scheduled, of type `int`,
/// `long`, `float` or `double`, named in a snapshot's locals or stack, and not
/// a phi, a parameter, a constant, a monitor object or a scalar-replacement
/// field value (`SlotPlan::sr_field_values`) -- the other reasons a value is
/// pinned, plus the constants an OSR stub re-materialises. A
/// snapshot read at a position before the value's own definition in the same
/// block is not a read (the value has no location there; `frame_value_for`
/// refuses such a description on its own).
pub(in crate::ir_lower) fn apply_snapshot_range_colours(
    graph: &Graph,
    schedule: &Schedule,
    spliced_ranges: &[(usize, usize, usize)],
    plan: &mut SlotPlan,
) -> bool {
    apply_snapshot_range_colours_with(
        graph,
        schedule,
        spliced_ranges,
        plan,
        SnapshotRangeSelect {
            all_prims: true,
            splice_local: false,
            all_refs: false,
        },
    )
}

/// Which snapshot-named `Pinned` values [`apply_snapshot_range_colours_with`]
/// colours by range. JIT round 25 wave 8 (W8-6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ir_lower) struct SnapshotRangeSelect {
    /// Every candidate `int` / `long` / `float` / `double` value, wherever it
    /// is defined (`CRATONVM_JIT_IR_SNAPSHOT_RANGE_COLOURS`, wave 3).
    pub(in crate::ir_lower) all_prims: bool,
    /// The candidate values of either class -- references included -- that
    /// are local to the compile's splices ([`splice_local_values`];
    /// `CRATONVM_JIT_IR_SPLICE_SLOT_SHARE`, wave 8).
    pub(in crate::ir_lower) splice_local: bool,
    /// Every candidate reference, wherever it is defined: the root's own
    /// snapshot-named references too (`CRATONVM_JIT_IR_SPLICE_SLOT_SHARE_ALL_REFS`
    /// under `CRATONVM_JIT_IR_SPLICE_SLOT_SHARE`, wave 9).
    pub(in crate::ir_lower) all_refs: bool,
}

/// `CRATONVM_JIT_IR_SPLICE_SLOT_SHARE` -- JIT round 25 wave 8 (W8-6),
/// default ON since j27 2026-10-28, `=0` kills: the snapshot-named values local to the compile's splices
/// ([`splice_local_values`]), references included, are coloured by range
/// instead of pinned, so disjoint splices share frame words
/// ([`apply_snapshot_range_colours_with`]). Read once per lowering attempt in
/// `lower_inner_attempt`; the keep-everything retry never ranges. No static.
pub(in crate::ir_lower) fn ir_splice_slot_share_enabled() -> bool {
    // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
    true
}

/// `CRATONVM_JIT_IR_SPLICE_SLOT_SHARE_ALL_REFS` -- JIT round 25 wave 9
/// (W9-4), default ON since j27 2026-10-28 (`=0` kills), and inert without `CRATONVM_JIT_IR_SPLICE_SLOT_SHARE`:
/// every snapshot-named reference is coloured by range, not only the
/// splice-local ones -- the root's own (`this`, the `Matcher`, its arrays,
/// the operand-stack temporaries every root bytecode names) included. Wave 9
/// measured why the splice-local set alone does not engage under
/// `CRATONVM_JIT_IR_NESTED_CHAIN_SPLICE`: a spliced body records CHAIN
/// snapshots (`CRATONVM_JIT_IR_SPLICE_FRAME_STATES`, default ON), and the
/// values only chain snapshots name are already coloured by range
/// (`CRATONVM_JIT_IR_CHAIN_SNAPSHOT_RANGE_PINS`, default ON, round 14), so
/// what stays `Pinned` is the root's. Read once per lowering attempt
/// ([`splice_slot_share_select`]). No static.
pub(in crate::ir_lower) fn ir_splice_slot_share_all_refs_enabled() -> bool {
    // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
    true
}

/// The candidate set this lowering attempt ranges, from the three switches:
/// `CRATONVM_JIT_IR_SNAPSHOT_RANGE_COLOURS` (every primitive),
/// `CRATONVM_JIT_IR_SPLICE_SLOT_SHARE` (the splice-local values, only when
/// the compile spliced something: `has_splices`) and, under it,
/// `CRATONVM_JIT_IR_SPLICE_SLOT_SHARE_ALL_REFS` (every reference). Reads the
/// switches; no static.
pub(in crate::ir_lower) fn splice_slot_share_select(has_splices: bool) -> SnapshotRangeSelect {
    let share = ir_splice_slot_share_enabled();
    SnapshotRangeSelect {
        all_prims: ir_snapshot_range_colours_enabled(),
        splice_local: share && has_splices,
        all_refs: share && ir_splice_slot_share_all_refs_enabled(),
    }
}

/// Why [`apply_snapshot_range_colours_why`] left the plan as it was (JIT
/// round 25 wave 9, W9-4: the `outcome=` of the `[ir-splice-slot-share]`
/// census).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ir_lower) enum SnapshotRangeSkip {
    /// No candidate class selected.
    Off,
    /// The plan's per-node tables do not match the graph.
    Shape,
    /// The plan's numbering does not re-derive from the schedule.
    Spans,
    /// No `Pinned` value of a selected class qualified.
    NoCandidate,
    /// The read dataflow did not converge within the plan's cap.
    NotConverged,
    /// A candidate ended without any position (no definition, no read).
    NoRange,
}

impl SnapshotRangeSkip {
    /// The census word.
    pub(in crate::ir_lower) fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Shape => "plan-shape",
            Self::Spans => "spans",
            Self::NoCandidate => "no-candidate",
            Self::NotConverged => "not-converged",
            Self::NoRange => "no-range",
        }
    }
}

/// JIT round 25 wave 9 (W9-4): where a plan's `Pinned` colours come from, by
/// origin (a node whose pc lies in a spliced range is `splice`, every other
/// `root`) and by type, plus what the ranging could take: the census half of
/// the `[ir-splice-slot-share]` line. Computed on the plan BEFORE ranging.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::ir_lower) struct PinnedOrigins {
    pub(in crate::ir_lower) root_ref: usize,
    pub(in crate::ir_lower) root_prim: usize,
    pub(in crate::ir_lower) splice_ref: usize,
    pub(in crate::ir_lower) splice_prim: usize,
    /// Pinned phis (never ranged).
    pub(in crate::ir_lower) phi: usize,
    /// Pinned parameters and constants (never ranged).
    pub(in crate::ir_lower) param_const: usize,
    /// Splice-local values ([`splice_local_values`]) still `Pinned`, and how
    /// many of them are references: what `CRATONVM_JIT_IR_SPLICE_SLOT_SHARE`
    /// alone can range.
    pub(in crate::ir_lower) splice_local: usize,
    pub(in crate::ir_lower) splice_local_ref: usize,
    /// Values the round-14 chain rule coloured by range instead of pinning.
    pub(in crate::ir_lower) chain_ranged: usize,
}

/// Count [`PinnedOrigins`] for `plan` (before any snapshot ranging). Pure.
pub(in crate::ir_lower) fn pinned_origins(
    graph: &Graph,
    plan: &SlotPlan,
    spliced_ranges: &[(usize, usize, usize)],
) -> PinnedOrigins {
    let in_splice = |pc: usize| {
        spliced_ranges
            .iter()
            .any(|&(start, end, _)| pc >= start && pc < end)
    };
    let local = splice_local_values(graph, spliced_ranges);
    let mut out = PinnedOrigins {
        chain_ranged: plan.chain_ranged,
        ..PinnedOrigins::default()
    };
    for (id, node) in graph.nodes.iter().enumerate() {
        if plan.class.get(id).copied().flatten() != Some(SlotClass::Pinned)
            || plan.node_color.get(id).copied().flatten().is_none()
        {
            continue;
        }
        if matches!(node.op, Op::Phi) {
            out.phi += 1;
            continue;
        }
        if matches!(node.op, Op::Param(_) | Op::Const(_) | Op::ConstF(_)) {
            out.param_const += 1;
            continue;
        }
        let is_ref = node.ty == IrType::Ref;
        let spliced = node.bytecode_pc.is_some_and(in_splice);
        match (spliced, is_ref) {
            (true, true) => out.splice_ref += 1,
            (true, false) => out.splice_prim += 1,
            (false, true) => out.root_ref += 1,
            (false, false) => out.root_prim += 1,
        }
        if local.get(id).copied().unwrap_or(false) {
            out.splice_local += 1;
            if is_ref {
                out.splice_local_ref += 1;
            }
        }
    }
    out
}

/// JIT round 25 wave 8 (W8-6, `CRATONVM_JIT_IR_SPLICE_SLOT_SHARE`): the values
/// that belong to a spliced body and to nothing outside the compile's splices.
/// `out[id]` holds when
///
/// * node `id` carries a bytecode pc inside some `(start, end, invoke)` range
///   of `spliced_ranges` (it was built from a spliced callee's bytecode), and
/// * at least one safepoint snapshot names it in its locals or stack, and
///   every snapshot that names it there (or in its monitors) sits at a bci
///   inside some spliced range -- no frame state of the compiling method's own
///   bytecode, and no snapshot outside every splice (a per-copy snapshot), ever
///   describes it.
///
/// Such a value is what the splice's own frame states keep alive: a callee
/// local, a callee operand-stack entry. Two of them from sibling splices on
/// different branches, or from sequential splices whose values are dead at the
/// boundary, have disjoint read sets, so they can share a frame word once their
/// colouring ranges are the reads rather than the whole method. Pure.
pub(in crate::ir_lower) fn splice_local_values(graph: &Graph, spliced_ranges: &[(usize, usize, usize)]) -> Vec<bool> {
    let n = graph.nodes.len();
    if spliced_ranges.is_empty() {
        return vec![false; n];
    }
    let in_splice = |pc: usize| {
        spliced_ranges
            .iter()
            .any(|&(start, end, _)| pc >= start && pc < end)
    };
    let mut out: Vec<bool> = graph
        .nodes
        .iter()
        .map(|node| node.bytecode_pc.is_some_and(in_splice))
        .collect();
    let mut named = vec![false; n];
    for sp in &graph.safepoints {
        let inside = in_splice(sp.bci);
        for &v in sp
            .locals
            .iter()
            .chain(sp.stack.iter())
            .chain(sp.monitors.iter())
        {
            let vi = v as usize;
            if v == NO_NODE || vi >= n {
                continue;
            }
            named[vi] = true;
            if !inside {
                out[vi] = false;
            }
        }
    }
    for (o, &nm) in out.iter_mut().zip(named.iter()) {
        *o &= nm;
    }
    out
}

/// JIT round 25 wave 8 (W8-6): [`apply_snapshot_range_colours`] over the
/// candidate set `select` names. With `select.splice_local`, a splice-local
/// value ([`splice_local_values`]) is ranged whatever its class: a reference
/// becomes an ordinary `Ref` value, so it shares its frame word only with
/// other `Ref` values (`recolour_slot_plan` keeps the two free lists apart,
/// and `verify_slot_colouring` re-derives that), and only where the colouring
/// ranges -- every modelled frame-state read included -- are disjoint. The
/// reader model and its enforcement (`Lowerer::snapshot_ranged_home_ok`, the
/// OSR seed check) are the wave-3 ones, and type-agnostic. See the section
/// "Wave 8 (W8-6)" of `docs/feature-designs/real-frame-deopt.md`
/// for why the reference consumers (maps, frame block, dead-home clears,
/// precise liveness) stay sound with shared snapshot-named `Ref` colours.
pub(in crate::ir_lower) fn apply_snapshot_range_colours_with(
    graph: &Graph,
    schedule: &Schedule,
    spliced_ranges: &[(usize, usize, usize)],
    plan: &mut SlotPlan,
    select: SnapshotRangeSelect,
) -> bool {
    apply_snapshot_range_colours_why(graph, schedule, spliced_ranges, plan, select).is_ok()
}

/// [`apply_snapshot_range_colours_with`], saying why it changed nothing (JIT
/// round 25 wave 9, W9-4). `Ok` exactly when the plan changed; on `Err` the
/// plan is untouched.
pub(in crate::ir_lower) fn apply_snapshot_range_colours_why(
    graph: &Graph,
    schedule: &Schedule,
    spliced_ranges: &[(usize, usize, usize)],
    plan: &mut SlotPlan,
    select: SnapshotRangeSelect,
) -> Result<(), SnapshotRangeSkip> {
    let n = graph.nodes.len();
    if plan.node_color.len() != n || plan.class.len() != n || plan.pos_of.len() != n {
        return Err(SnapshotRangeSkip::Shape);
    }
    if !select.all_prims && !select.splice_local && !select.all_refs {
        return Err(SnapshotRangeSkip::Off);
    }
    let splice_local = if select.splice_local {
        splice_local_values(graph, spliced_ranges)
    } else {
        vec![false; n]
    };
    let Some(span) = slot_plan_spans(schedule, &plan.pos_of) else {
        return Err(SnapshotRangeSkip::Spans);
    };
    let nb = schedule.blocks.len();
    let resume = |pc: usize| {
        spliced_ranges
            .iter()
            .find(|&&(start, end, _)| pc >= start && pc < end)
            .map_or(pc, |&(_, _, invoke)| invoke)
    };
    let mut by_bci: HashMap<usize, Vec<usize>> = HashMap::new();
    for (si, sp) in graph.safepoints.iter().enumerate() {
        by_bci.entry(sp.bci).or_default().push(si);
    }
    let mut bcis: Vec<usize> = by_bci.keys().copied().collect();
    bcis.sort_unstable();
    let prim_ty =
        |t: IrType| matches!(t, IrType::Int | IrType::Long | IrType::Float | IrType::Double);
    let mut keep_pinned = vec![false; n];
    for &v in &plan.sr_field_values {
        if let Some(k) = keep_pinned.get_mut(v as usize) {
            *k = true;
        }
    }
    let monitor_operands = graph
        .safepoints
        .iter()
        .flat_map(|sp| sp.monitors.iter().copied())
        .chain(
            graph
                .nodes
                .iter()
                .filter(|node| matches!(node.op, Op::MonitorEnter | Op::MonitorExit))
                .flat_map(|node| node.inputs.iter().copied()),
        );
    for v in monitor_operands {
        if let Some(k) = keep_pinned.get_mut(v as usize) {
            *k = true;
        }
    }
    let mut named = vec![false; n];
    for sp in graph.safepoints.iter() {
        for &v in sp.locals.iter().chain(sp.stack.iter()) {
            if let Some(c) = named.get_mut(v as usize) {
                *c = true;
            }
        }
    }
    let mut index = vec![usize::MAX; n];
    let mut values: Vec<usize> = Vec::new();
    for (id, node) in graph.nodes.iter().enumerate() {
        if !named[id]
            || keep_pinned[id]
            || plan.class[id] != Some(SlotClass::Pinned)
            || plan.node_color[id].is_none()
            || plan.pos_of[id].is_none()
            // A constant stays pinned: a frame state describes it as an
            // immediate anyway, and an OSR entry stub re-materialises its home
            // outside any modelled position.
            || matches!(node.op, Op::Phi | Op::Param(_) | Op::Dead | Op::Const(_) | Op::ConstF(_))
            // Wave 8 (W8-6): a splice-local value of either class; every
            // other candidate is a primitive under the wave-3 switch. A
            // reference result a call publishes stays fresh-only in the
            // recolouring (`colour_is_fresh_only`): it may donate its word
            // once it dies, never receive a recycled one.
            // Wave 9 (W9-4): every reference under `..._ALL_REFS`.
            || !((select.all_prims && prim_ty(node.ty))
                || (select.all_refs && node.ty == IrType::Ref)
                || (splice_local[id] && (prim_ty(node.ty) || node.ty == IrType::Ref)))
        {
            continue;
        }
        index[id] = values.len();
        values.push(id);
    }
    if values.is_empty() {
        return Err(SnapshotRangeSkip::NoCandidate);
    }
    let block_of = |v: usize| schedule.node_to_block.get(v).copied().unwrap_or(usize::MAX);
    let words = values.len().div_ceil(64).max(1);
    let mut use_bits = vec![0u64; words * nb];
    let mut def_bits = vec![0u64; words * nb];
    let mut reads: Vec<Vec<usize>> = vec![Vec::new(); values.len()];
    let mut entry_bcis: Vec<Vec<usize>> = vec![Vec::new(); values.len()];
    let mut local_def = vec![0u64; words];
    let mut sites: Vec<usize> = Vec::new();
    let no_sites: &[usize] = &[];
    let at_bci = |bci: usize| by_bci.get(&bci).map_or(no_sites, Vec::as_slice);
    for (b, block) in schedule.blocks.iter().enumerate() {
        let base = b * words;
        local_def.fill(0);
        let (start, edge) = span[b];
        // The block start: an OSR entry stub at the control node's bci (an
        // opaque arm: at its header's) seeds and describes values there.
        let mut entry_at: Vec<usize> = Vec::new();
        if let Some(ctrl) = graph.nodes.get(block.ctrl as usize) {
            if let Some(pc) = ctrl.bytecode_pc {
                entry_at.push(pc);
            }
            if matches!(ctrl.op, Op::Proj(_)) {
                let flag = ctrl
                    .inputs
                    .first()
                    .and_then(|&iff| graph.nodes.get(iff as usize))
                    .filter(|iff| iff.op == Op::If)
                    .and_then(|iff| iff.inputs.get(1))
                    .and_then(|&cond| graph.nodes.get(cond as usize));
                if let Some(Op::OsrEntryFlag { header_bci }) = flag.map(|f| &f.op) {
                    entry_at.push(*header_bci);
                }
            }
        }
        for &bci in &entry_at {
            for &si in at_bci(bci) {
                let Some(sp) = graph.safepoints.get(si) else {
                    continue;
                };
                for &v in sp.locals.iter().chain(sp.stack.iter()) {
                    let Some(&k) = index.get(v as usize).filter(|&&k| k != usize::MAX) else {
                        continue;
                    };
                    reads[k].push(start);
                    entry_bcis[k].push(bci);
                    bits_insert(&mut use_bits[base..base + words], k);
                }
            }
        }
        for u in block.nodes.iter().copied().chain(block.terminator) {
            let ui = u as usize;
            let Some(node) = graph.nodes.get(ui) else {
                continue;
            };
            let Some(upos) = plan.pos_of.get(ui).copied().flatten() else {
                continue;
            };
            if node.op != Op::Dead {
                sites.clear();
                if let Some(pc) = node.bytecode_pc {
                    sites.extend_from_slice(at_bci(pc));
                    sites.extend_from_slice(at_bci(resume(pc)));
                    let next = bcis.partition_point(|&x| x <= pc);
                    if let Some(&nx) = bcis.get(next) {
                        sites.extend_from_slice(at_bci(nx));
                    }
                }
                if let Op::Guard { bci } = node.op {
                    sites.extend_from_slice(at_bci(bci));
                    sites.extend_from_slice(at_bci(resume(bci)));
                }
                if let Some(si) = node.frame_snapshot {
                    sites.push(si as usize);
                }
                for &si in &sites {
                    let Some(sp) = graph.safepoints.get(si) else {
                        continue;
                    };
                    for &v in sp.locals.iter().chain(sp.stack.iter()) {
                        let vi = v as usize;
                        let Some(&k) = index.get(vi).filter(|&&k| k != usize::MAX) else {
                            continue;
                        };
                        // Not yet defined in this block: no location here.
                        if vi == ui
                            || (block_of(vi) == b
                                && plan.pos_of[vi].is_some_and(|vp| vp > upos))
                        {
                            continue;
                        }
                        reads[k].push(upos);
                        if !bits_contains(&local_def, k) {
                            bits_insert(&mut use_bits[base..base + words], k);
                        }
                    }
                }
                // Ordinary uses: already in the value's range, recorded so a
                // frame state built at its reader passes the check.
                for &v in node.inputs.iter() {
                    if let Some(&k) = index.get(v as usize).filter(|&&k| k != usize::MAX) {
                        reads[k].push(upos);
                    }
                }
            }
            if let Some(&k) = index.get(ui).filter(|&&k| k != usize::MAX) {
                bits_insert(&mut local_def, k);
                bits_insert(&mut def_bits[base..base + words], k);
                reads[k].push(upos);
            }
        }
        // The outgoing edge (and the terminator): each successor's state.
        let term_pos = block
            .terminator
            .and_then(|t| plan.pos_of.get(t as usize).copied().flatten());
        for &s in &block.successors {
            let Some(succ) = schedule.blocks.get(s) else {
                continue;
            };
            let Some(pc) = graph
                .nodes
                .get(succ.ctrl as usize)
                .and_then(|c| c.bytecode_pc)
            else {
                continue;
            };
            for &si in at_bci(pc) {
                let Some(sp) = graph.safepoints.get(si) else {
                    continue;
                };
                for &v in sp.locals.iter().chain(sp.stack.iter()) {
                    let Some(&k) = index.get(v as usize).filter(|&&k| k != usize::MAX) else {
                        continue;
                    };
                    reads[k].push(edge);
                    if let Some(tp) = term_pos {
                        reads[k].push(tp);
                    }
                    if !bits_contains(&local_def, k) {
                        bits_insert(&mut use_bits[base..base + words], k);
                    }
                }
            }
        }
    }
    // The backward fixed point over these reads (their ordinary uses are in
    // the plan's ranges already), as `chain_read_colour_ranges_with` runs it.
    let mut live_in = vec![0u64; words * nb];
    let mut live_out = vec![0u64; words * nb];
    let mut scratch = vec![0u64; words];
    let mut converged = false;
    for _ in 0..SLOT_PLAN_MAX_ITERATIONS {
        let mut changed = false;
        for b in (0..nb).rev() {
            let base = b * words;
            scratch.fill(0);
            for &s in &schedule.blocks[b].successors {
                if s >= nb {
                    continue;
                }
                for w in 0..words {
                    scratch[w] |= live_in[s * words + w];
                }
            }
            for w in 0..words {
                let merged_out = live_out[base + w] | scratch[w];
                if merged_out != live_out[base + w] {
                    live_out[base + w] = merged_out;
                    changed = true;
                }
                let entering = use_bits[base + w] | (merged_out & !def_bits[base + w]);
                let merged_in = live_in[base + w] | entering;
                if merged_in != live_in[base + w] {
                    live_in[base + w] = merged_in;
                    changed = true;
                }
            }
        }
        if !changed {
            converged = true;
            break;
        }
    }
    if !converged {
        return Err(SnapshotRangeSkip::NotConverged);
    }
    let mut colouring: Vec<Option<LiveRange>> = plan.colouring_ranges().to_vec();
    let mut lo: Vec<usize> = values
        .iter()
        .map(|&v| colouring[v].map_or(usize::MAX, |r| r.lo))
        .collect();
    let mut hi: Vec<usize> = values
        .iter()
        .map(|&v| colouring[v].map_or(0, |r| r.hi))
        .collect();
    for (k, rs) in reads.iter().enumerate() {
        for &p in rs {
            lo[k] = lo[k].min(p);
            hi[k] = hi[k].max(p);
        }
    }
    for (b, &(start, end)) in span.iter().enumerate() {
        let base = b * words;
        bits_for_each(&live_in[base..base + words], |k| {
            if let Some(l) = lo.get_mut(k) {
                *l = (*l).min(start);
            }
        });
        bits_for_each(&live_out[base..base + words], |k| {
            if let Some(h) = hi.get_mut(k) {
                *h = (*h).max(end);
            }
        });
    }
    for (k, &v) in values.iter().enumerate() {
        if lo[k] == usize::MAX {
            return Err(SnapshotRangeSkip::NoRange);
        }
        colouring[v] = Some(LiveRange {
            lo: lo[k],
            hi: hi[k].max(lo[k]),
        });
    }
    let slots_before = plan.slots;
    let pinned: Vec<bool> = (0..n)
        .map(|id| plan.class[id] == Some(SlotClass::Pinned) && index[id] == usize::MAX)
        .collect();
    recolour_slot_plan(graph, plan, &pinned, &colouring);
    plan.colour_range = colouring;
    let mut read_map: HashMap<NodeId, Vec<usize>> = HashMap::new();
    let mut entry_map: HashMap<NodeId, Vec<usize>> = HashMap::new();
    for (k, &v) in values.iter().enumerate() {
        // Cast: a node index, far below `u32::MAX` (it came from one).
        let id = v as NodeId;
        let mut rs = std::mem::take(&mut reads[k]);
        rs.sort_unstable();
        rs.dedup();
        read_map.insert(id, rs);
        let mut es = std::mem::take(&mut entry_bcis[k]);
        es.sort_unstable();
        es.dedup();
        entry_map.insert(id, es);
    }
    plan.snapshot_ranged = Some(Box::new(SnapshotRanged {
        reads: read_map,
        entry_bcis: entry_map,
        span,
        slots_before,
    }));
    Ok(())
}

/// How many bytes a fresh-frame clear of `frame_size` bytes writes under
/// `skip` (indexed by word `w` = `[rbp - 8(w + 1)]`; empty: the whole frame).
/// The `CRATONVM_DBG_JITC` arithmetic of the round-25 primitive-colour claim.
pub(in crate::ir_lower) fn fresh_clear_written_bytes(frame_size: i32, skip: &[bool]) -> u32 {
    if frame_size <= 0 || frame_size % 8 != 0 {
        return 0;
    }
    // Cast: a positive frame size over 8 (at most 32 KiB / 8).
    let words = (frame_size / 8) as usize;
    let kept = (0..words)
        .filter(|&w| !skip.get(w).copied().unwrap_or(false))
        .count();
    // Cast: at most 4096 words.
    (kept as u32).saturating_mul(8)
}

/// `CRATONVM_JIT_IR_ZERO_FRESH_FRAMES_AVX` -- the fresh-frame clear's wide
/// stores are 32-byte `VMOVDQU ymm5` (VEX throughout, `VZEROUPPER` after the
/// last one) instead of 16-byte `MOVDQU xmm5`, on a CPU and OS with AVX
/// (`is_x86_feature_detected!`, which caches its own answer: no static here).
/// The same words are zeroed. Lane frames, round 16 wave 7; default ON since
/// wave 8 (alone it measured no gain, bintrees 98 -> 101 ms; with the two
/// skips it is part of the measured 98 -> 90), `=0` is the kill switch. Read
/// per compile.
pub(in crate::ir_lower) fn ir_zero_fresh_frames_avx_enabled() -> bool {
    true
        && fresh_frame_clear_avx_supported()
}

/// Can this process execute the VEX-encoded clear?
pub(in crate::ir_lower) fn fresh_frame_clear_avx_supported() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        std::is_x86_feature_detected!("avx")
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}



/// The frame-deopt `SavedRegisters` XMM half of a frame whose `gpr[0]` sits at
/// `[rbp - deopt_regs_base]`, as the half-open `[lo, hi)` range of `[rbp - off]`
/// word offsets, or `None` when no image was reserved. `xmm[n]` is at
/// `deopt_regs_base - 128 - 8n`, `n` in `0..16`. ONE definition for the
/// published `FrameLayout::deopt_xmm_lo/hi` and the clear's skip, so the clear
/// can never leave out a word the collector does not also treat as the image.
pub(in crate::ir_lower) fn deopt_xmm_image_range(deopt_regs_base: i32) -> Option<(i32, i32)> {
    (deopt_regs_base != 0).then_some((deopt_regs_base - 248, deopt_regs_base - 120))
}

/// Frames up to this many bytes are cleared with straight-line 16-byte stores
/// (at most 16 of them); larger ones loop 64 bytes per iteration
/// (`Lowerer::emit_zero_frame_sse`).
pub(in crate::ir_lower) const ZERO_FRAME_SSE_STRAIGHT_BYTES: i32 = 256;

// ── The store-free form: precise frame liveness (gen r5w3/live7) ─────
//
// `CRATONVM_JIT_PRECISE_FRAME_LIVENESS=1` spends the SAME dead-colour rule
// without a store in compiled code. At every GC point of an armed call site
// the lowerer
//
//   * leaves the dead homes out of the safepoint map and the per-call shadow
//     push, so the precise channels (`scan_active_oop_map_at_rbp`, the
//     shadow-stack root pass, the relocation walk) no longer name them -- the
//     frame block, which would publish every home for the whole activation,
//     is off under the flag (`plan_frame_block`);
//   * records `(covered, live)` for the safepoint id in
//     `CompiledMethod::frame_liveness` ([`crate::FrameLivenessTable`]):
//     covered = every clearable colour's home plus the staging region, live =
//     covered minus the homes dead there and the staging words this call does
//     not write (`Lowerer::precise_liveness_claim`);
//
// and the collector's band scan skips a covered word the record leaves out
// (`vm/src/jit/conservative_roots.rs`, `is_dead_by_frame_liveness`, which
// spends whatever table a body carries -- so this per-compile read is the one
// gate, and a body compiled without the flag claims nothing). The words stay
// as they are: a dead word keeps whatever it held,
// which the soundness argument above says no compiled code, no deopt and no
// frame-state reader reads before a later definition writes it. That is also
// why a relocation need not rewrite it.
//
// What it does NOT cover, said so a PASS/FAIL is read correctly: `Prim`
// colours, phi colours, parameter homes (gci i3/ir: under the opt-in
// `CRATONVM_JIT_IR_REF_PARAM_HOME_ZERO` a reference parameter's prologue word
// is zeroed after its copy instead, `ref_param_home_zero_candidates`), the
// bookkeeping words, the register images and the single-pass tier. Its oracle is
// `CRATONVM_DBG_VERIFY_REG_OOP_MAPS=1` (every word the claim drops is
// re-checked for reachability, and a dropped word the active map still names
// is reported as a contradiction).
//
// Read per compile, no static (`process_global_statics_ratchet`).



// ── Dead-home clears, on by default (gen r5w5/oomjit9) ───────────────
//
// The two forms above are opt-in, and neither reached the probes' holder: the
// census on `d8a690353` (`GenR5W3OsrHolderProbe`, `GenR5W2OsrDeadSlotProbe`)
// names a dropped list held by an OSR frame's own `region=operand-spill`
// words, reached twice -- through the frame block's indirect shadow-stack
// entries (`jit-shadow-stack`, section 11's root snapshot) and through the
// band scan below the monotone watermark (section 14). Both read the WORD, so
// a word that holds null stops both. What this pass zeroes, at the first byte
// of every armed `Op::Call` (own or spliced):
//
//   * every reference home `DeadRefSlotClear::dead_colours_at` calls dead
//     there AND that may hold a value written since it was last zeroed on
//     some path to the call (a forward union over the block graph: a colour
//     turns stale at each definition of a value of its colour and clean at
//     each armed call where it is dead). A dead word that is provably still
//     zero is not stored again, so a hot loop pays one store per value that
//     died since its previous call, not one per dead colour per call;
//   * every argument-staging word an earlier call may have left a reference
//     in (stale at each call that stages a `Ref` argument there; clean at
//     each armed call). The whole region, not only the words past this
//     call's arguments: a direct call to a compiled callee stages nothing on
//     its hot path (`CRATONVM_JIT_IR_COLD_ARG_STAGE`), so the previous call's
//     arguments would otherwise sit in the words this call calls its own for
//     the whole callee -- `GenR4W6JitOomRootProbe`'s `catch-inline-receiver`,
//     where the `WeakReference` constructor staged the `Box` and the checking
//     callee ran on top of it. Every writer of the region is a call lowering
//     whose readers run inside the same node, after it wrote.
//
// Sound for the reason the opt-in store form is: a dead colour holds nothing
// compiled code, a deopt, a rethrow pad or a mode exit reads before a later
// definition writes it, and the keep set now also covers the splices' frame
// states and anything the schedule placed after the call. Only zero is ever
// written, so the worst a wrong "may be stale" answer can do is leave a dead
// word unzeroed -- the old behaviour -- never clear a live one.
//
// `CRATONVM_JIT_IR_DEAD_HOME_CLEARS=0` is the kill switch. Read per compile, no
// static (`process_global_statics_ratchet`). Default ON as a retention fix
// (dropped data kept reachable by compiled frames), with the price to measure
// named in `docs/feature-designs/jit-ir-optimizations.md`.





// gci i1/fr (2026-10-11) -- `gengc-r5w5-oomjit9-ir-dead-homes-residuals` item 1:
// allocation GC points are armed sites (opt-in).
//
// Until now only an `Op::Call` armed the dead-reference facts
// (`dead_ref_clear_site_for`), so a collection reached from an inline
// allocation's slow path (a TLAB refill, or the `OutOfMemoryError` a compiled
// allocation throws) saw the frame's homes as its last CALL left them: a value
// that died between that call and the allocation stayed a root -- in the band,
// in the map's `frame_slot_offsets`, and in the shadow push -- for the
// collection the allocation triggered.
//
// `Op::New` / `Op::NewArray` are control-pinned exactly like a call
// (`ir_schedule::pinned_anchor`: slot 0 is the control anchor, and LICM never
// re-anchors an allocation), so their bci is their program point -- the premise
// of the bytecode-reachability half of the keep set. The schedule half is the
// same backward liveness, recording an after-set at the allocation step too
// (`snapshot_colours_live_after_each_call` / `snapshot_colours_after_each_call`
// through [`dead_ref_clear_arms`]); a step's set is the colours live AT it,
// i.e. its own reads and everything after it minus its own result, which is
// not written until the allocation returns. Every consumer of an armed site
// is unchanged: the planned first-byte clears (`plan_dead_home_clears`: an
// allocation takes no inline prefix, so it always cleans), the store-free
// claim at its safepoint map (`precise_liveness_claim`: an allocation stages
// no reference argument, so the staging words are an earlier call's), and the
// opt-in dense stores.
//
// Price: stores at the first byte of an allocation whose dead homes MAY be
// stale there (the stale-set union zeroes a home once per definition, not per
// iteration), on the allocation's fast path. Hence opt-in, with a cost row.
// `=1` arms allocations; unset is the call-only rule byte for byte.



/// Does a node with op `op` arm the dead-reference facts (a GC point whose
/// keep set the planner computes)? An `Op::Call` always; an `Op::New` /
/// `Op::NewArray` when `allocs` ([`ir_alloc_site_clears_enabled`]). Pure.
pub(in crate::ir_lower) fn dead_ref_clear_arms(op: &Op, allocs: bool) -> bool {
    matches!(op, Op::Call { .. }) || (allocs && matches!(op, Op::New { .. } | Op::NewArray { .. }))
}

// gci i4/ir (2026-10-12) -- `gci-i3ir-unarmed-optimizing-gc-points-keep-dead-homes`
// item 1: the other GC-point ops are armed sites too (opt-in).
//
// Besides a call and an allocation, `ir_op_is_gc_point` names ops whose slow
// path reaches the runtime and can stop for a collection: `ConstString` /
// `ConstClass` / `NewCp` (resolution, allocation), `LoadStatic` /
// `StoreStatic` (class initialisation runs Java code), `CheckCast` /
// `InstanceOf` (resolution), `MonitorEnter` / `MonitorExit` (a contended
// monitor parks), `MultiNewArray` (allocates). At their maps nothing was
// claimed or cleared, so `emit_safepoint_map` named every defined `Ref` home
// there, dead or alive, and a value that died after the last armed site on the
// path stayed a precise root of that collection.
//
// The rule arms such a node only when it is CONTROL-PINNED (slot 0 a control
// node, the `ir_schedule::pinned_anchor` shape) and carries its own bci: then
// its bci is its program point, the premise the bytecode-reachability half of
// the keep set rests on, exactly as for a call and an allocation. A floating
// `CheckCast` / `InstanceOf` / `ConstString` / `ConstClass` (built without an
// anchor) stays unarmed. `IndyBridge` and `LambdaIntToDouble` stay unarmed:
// they call Java with arguments whose staging this file does not model per op.
// The armed op's precise claim names NO staging word dead
// (`dead_ref_clear_site_args = usize::MAX` for these ops), so whatever a slow
// path stages stays covered and live, as at an unarmed site.
//
// Every consumer of an armed site is unchanged: the planned first-byte clears
// (`plan_dead_home_clears`; these ops take no inline prefix, so they always
// clean), the store-free claim at their maps, the opt-in dense stores. The
// keep-set builders record an after-set at the same steps.
//
// Price: stores at the first byte of such an op (a `getstatic` of an
// initialised class, a `checkcast` that passes) for each home that MAY be stale
// there, on the fast path. Hence opt-in with a cost row. `=1` arms them; unset
// is the call-and-allocation rule byte for byte. Read once per compile.
//
// Not covered: the safepoint POLLS (method entry, loop back edges). A poll is
// not a node, so the keep-set builders have no step for it; arming it needs a
// synthetic step per poll (design on the page).





/// Is `node` one of the non-call GC-point ops the section above arms: the op
/// is listed, the node carries a bci, and its slot 0 is a control node (the
/// pinned shape)? Pure.
pub(in crate::ir_lower) fn other_gc_point_is_pinned_site(graph: &Graph, node: &Node) -> bool {
    if !matches!(
        node.op,
        Op::ConstString { .. }
            | Op::ConstClass { .. }
            | Op::LoadStatic { .. }
            | Op::StoreStatic { .. }
            | Op::CheckCast { .. }
            | Op::InstanceOf { .. }
            | Op::MonitorEnter
            | Op::MonitorExit
            | Op::MultiNewArray { .. }
            | Op::NewCp { .. }
    ) {
        return false;
    }
    if node.bytecode_pc.is_none() || crate::ir_verify::control_input_indices(node) != (0..1) {
        return false;
    }
    node.inputs
        .first()
        .and_then(|&c| graph.nodes.get(c as usize))
        .is_some_and(|c| c.ty == IrType::Control)
}

/// The armed-site rule with the graph in hand: [`dead_ref_clear_arms`], plus,
/// when `others` ([`ir_other_gc_point_clears_enabled`]), a pinned non-call
/// GC-point op ([`other_gc_point_is_pinned_site`]). Pure.
pub(in crate::ir_lower) fn dead_ref_clear_arms_node(graph: &Graph, node: &Node, allocs: bool, others: bool) -> bool {
    dead_ref_clear_arms(&node.op, allocs) || (others && other_gc_point_is_pinned_site(graph, node))
}

// gci i5/ir (2026-10-12) -- item 2 of
// `gci-i3ir-unarmed-optimizing-gc-points-keep-dead-homes-FIXED-20261013`: the loop
// BACK-EDGE safepoint polls arm the dead-home facts too (opt-in,
// `CRATONVM_JIT_IR_POLL_GC_POINT_CLEARS=1`).
//
// A poll is not a node, so it gets a synthetic site id
// ([`poll_site_key`]: `graph.nodes.len() + block`, which names no node) and
// both keep-set builders record an after-set under it:
//
//   * the poll of a block WITH a terminator (`lower_terminator`'s conditional
//     back edge) runs BEFORE the terminator's compare, so its step sits before
//     the terminator's step and its position is the terminator's own
//     (`pos_of[term]`): every value the compare reads is still covered there;
//   * the poll of a block WITHOUT one (a `goto`, `lower_block`'s fall-through
//     arm) runs after every node and before the edge's phi copies, so its step
//     is the block's last and its position is the block's edge position (one
//     past its last instruction: where `emit_phi_copies` reads, and where every
//     live-out value's range ends -- `plan_slots`' step 5a).
//
// The bci the reachability half reads is the loop HEADER's (the `Merge`'s own
// pc): after the poll the thread is at the header (taken edge, or the mode
// exit, which resumes there) or on the branch's other edge, which the header
// reaches too, so every snapshot a later reader can name is reachable from it.
// The poll's step also READS the header's snapshots (the mode exit's state is
// the header's, φ inputs replaced by this edge's, and those inputs' ranges end
// at the edge position). Only a true back edge is armed: the header must be a
// `Merge` / `Region` with a bci that dominates the block. The method-entry
// poll is never armed (nothing is defined there).
//
// At an armed poll the slow path's map spends the store-free claim and the
// dense stores exactly as an armed call's does, with no staging word claimed
// dead (`dead_ref_clear_site_args = usize::MAX`), and the dense stores are
// emitted even where the planned first-byte clears are complete
// (`Lowerer::poll_site_dense`): no planned clear runs before a poll, so its
// dead homes would otherwise keep their last value in the word. The fast path
// (flag clear) is byte for byte unchanged; off, every poll is unarmed as
// before. Read once per compile.

/// `CRATONVM_JIT_IR_POLL_GC_POINT_CLEARS=1`: see the section above. Default
/// OFF.
pub(in crate::ir_lower) fn ir_poll_gc_point_clears_enabled() -> bool {
    false
}

/// The synthetic site id of the back-edge poll at the end of `block`
/// (`graph.nodes.len() + block`): no node has that id, so it can key the
/// keep-set maps beside the real armed nodes. `None` when it does not fit.
pub(in crate::ir_lower) fn poll_site_key(graph: &Graph, block: usize) -> Option<NodeId> {
    graph
        .nodes
        .len()
        .checked_add(block)
        .and_then(|k| NodeId::try_from(k).ok())
}

/// The loop header a back-edge poll at the end of `block` jumps to and the
/// header's bytecode pc, or `None` when that poll is not armed: no successor
/// at or before `block` (no back edge, so no poll), a header that is not a
/// `Merge` / `Region` with a bci, or one that does not dominate `block` (not
/// a loop). The first such successor, as `lower_block` / `lower_terminator`
/// decide to poll. Pure.
pub(in crate::ir_lower) fn back_edge_poll_header(
    graph: &Graph,
    schedule: &Schedule,
    block: usize,
) -> Option<(usize, usize)> {
    let blk = schedule.blocks.get(block)?;
    if !schedule.dom.is_reachable(block) {
        return None;
    }
    // gci i5/ir (t5a): EVERY successor laid out at or before `block`, not only
    // the first: the hot-path layout can put a loop's EXIT block before the
    // loop, which makes a forward edge look backward (`s <= block`); that
    // edge's target is no dominating header and arms nothing, while a true
    // back edge of the same block still does.
    //
    // A header the optimizer built without a bci (the partial unroller's main
    // loop, `ir_optimize` ~15022) takes the bci of the latch's own last
    // instruction instead: it lies inside the bytecode loop, from which the
    // whole loop and everything after it is reachable, and such a header has
    // no mode exit (that needs the header's bci) whose state could be read.
    let latch_pc = || {
        blk.terminator
            .into_iter()
            .chain(blk.nodes.iter().rev().copied())
            .find_map(|id| graph.nodes.get(id as usize).and_then(|n| n.bytecode_pc))
    };
    blk.successors.iter().copied().filter(|&s| s <= block).find_map(|header| {
        let ctrl = schedule.blocks.get(header)?.ctrl;
        let node = graph.nodes.get(ctrl as usize)?;
        if !matches!(node.op, Op::Merge | Op::Region) || !schedule.dom.dominates(header, block) {
            return None;
        }
        Some((header, node.bytecode_pc.or_else(latch_pc)?))
    })
}

// ── The spliced back-edge mode exit is a frame-state reader (r21 keepset) ──
//
// JIT round 21 wave 1, lane keepset (residual 3 of
// `docs/feature-designs/jit-ir-optimizations.md`).
// The unconditional back-edge poll of a loop INSIDE a splice may leave
// through a caller chain (`Lowerer::spliced_back_edge_mode_exit_state`,
// `CRATONVM_JIT_IR_SPLICED_HEADER_POLL_EXITS` /
// `CRATONVM_JIT_IR_ENTRY_SPLICED_HEADER_POLL_EXITS`, both default ON). That
// exit reads the chain snapshot recorded at the header's pc, AT THE LATCH,
// with each of the header's φs replaced by its input on this edge, every
// value from its home word (`frame_value_for`, which never asks whether the
// word was recycled or zeroed). Neither liveness that guards those words
// placed that read:
//
// * the keep set ([`snapshot_colours_live_after_each_call`]): the only steps
//   at the header's pc are its φs, which read no snapshot at a spliced pc
//   (gci i47w/ks47, `CRATONVM_JIT_IR_KEEP_SET_SPLICED_EXITLESS_STEPS`) but
//   still mark the pc read, so the header snapshot also left the
//   bytecode-reachability union; the latch's poll step read nothing. A value
//   only that snapshot names -- a callee local the loop reads only through a
//   computation LICM hoisted to the pre-header, e.g. `s += b.v` -- was dead
//   at the poll, `outer_dead_home_offsets` zeroed its home in the poll's slow
//   path, and the exit then resumed the callee with `b == null` (a
//   `NullPointerException` HotSpot never throws), or the in-loop calls'
//   maps dropped it;
// * the colouring ([`chain_read_colour_ranges`]): it counts nodes at the
//   snapshot's pc. A header φ is one, so a value defined before the loop is
//   live round the whole loop whenever the header has a φ (the answer below
//   is then byte-identical); a φ-less spliced loop had no reader and could
//   hand the word to another value.
//
// The fix: both make the latch a reader of exactly what the exit reads
// ([`spliced_latch_exit_reads`]) -- the keep set at the latch's poll step
// (its after set is what the poll's dead-home clears and every earlier GC
// point of the loop consult), the colouring at the latch's edge position.
// `CRATONVM_JIT_IR_SPLICED_LATCH_EXIT_READS=0` restores both byte for byte.
// Read once per keep-set build / colouring, no static
// (`process_global_statics_ratchet`). Default ON as a correctness fix.





/// JIT round 21 wave 3 (lane keepset; rev-ir finding 2 of
/// `docs/feature-designs/jit-ir-optimizations.md`): the
/// structural half of `Lowerer::back_edge_mode_exit_state` -- may the poll on
/// the edge `pred_block -> header_block` take a mode exit at all? `pred` ends
/// in no terminator and has that one successor, the header block is headed by
/// a `Merge` / `Region` with a bci, and it dominates a reachable `pred`.
/// `Ok((header_ctrl, header_bci))`, or the refusal name the census prints.
/// The ONE copy of these tests: the lowering refuses through it and
/// [`spliced_latch_exit_reads`] admits through it, so the keep set reads the
/// header snapshot at exactly the latches whose exit can read it (a superset
/// after the lowering's non-structural refusals, which only remove exits).
/// Pure.
pub(in crate::ir_lower) fn back_edge_exit_shape(
    graph: &Graph,
    schedule: &Schedule,
    pred_block: usize,
    header_block: usize,
) -> Result<(NodeId, usize), &'static str> {
    let pred = schedule.blocks.get(pred_block).ok_or("no-pred-block")?;
    if pred.terminator.is_some() {
        return Err("pred-ends-in-a-branch");
    }
    if pred.successors.len() != 1 || pred.successors.first() != Some(&header_block) {
        return Err("pred-not-a-plain-edge");
    }
    let header_ctrl = schedule.blocks.get(header_block).ok_or("no-header-block")?.ctrl;
    let header = graph.nodes.get(header_ctrl as usize).ok_or("no-header-node")?;
    if !matches!(header.op, Op::Merge | Op::Region) {
        return Err("header-not-a-merge");
    }
    // A true back edge: the header dominates the edge's source.
    let dom = &schedule.dom;
    if !dom.is_reachable(pred_block) || !dom.dominates(header_block, pred_block) {
        return Err("not-a-dominated-back-edge");
    }
    let header_bci = header.bytecode_pc.ok_or("header-without-bci")?;
    Ok((header_ctrl, header_bci))
}

/// JIT round 21 wave 1 (lane keepset): what the spliced back-edge mode exit
/// of `latch` can read, or `None` when `latch` can have no such exit. The
/// shape tests are `Lowerer::back_edge_mode_exit_state`'s: no terminator, a
/// single successor laid out at or before it that dominates it and is
/// headed by a `Merge` / `Region` with a pc at which `chain_by_pc` (every
/// guard-chain snapshot, by pc) lists a snapshot. Returns those snapshot
/// indices (all of them: the exit takes the first, a superset is sound) and
/// the values they name -- locals, stack and monitors -- with each φ of the
/// header replaced by its input on this edge (the exit reads the edge input,
/// never the φ's home, which the edge copy after the poll rewrites). A φ
/// whose edge is not found is listed as itself. May repeat a value. Pure.
///
/// `outer_local_dead(bci, slot)`, when given, leaves out a local of the
/// COMPILING method (the chain's outermost scope, parked in its `invoke` at
/// `bci`) that bytecode liveness calls dead there: the exit then resumes the
/// interpreter with whatever the (possibly zeroed) word holds for a local it
/// never reads, as `sp_bits` narrows an own-pc snapshot's locals. Only the
/// keep set narrows (zeroing such a word is harmless); the colouring passes
/// `None`, so no other value is ever handed the word.
pub(in crate::ir_lower) fn spliced_latch_exit_reads(
    graph: &Graph,
    schedule: &Schedule,
    latch: usize,
    chain_by_pc: &HashMap<usize, Vec<usize>>,
    outer_local_dead: Option<&dyn Fn(usize, usize) -> bool>,
) -> Option<(Vec<usize>, Vec<NodeId>)> {
    if chain_by_pc.is_empty() {
        return None;
    }
    // The poll gets a mode exit only where `lower_block` passes it a header:
    // no terminator and a first successor at or before the latch. The rest is
    // the lowering's own shape test, shared (`back_edge_exit_shape`).
    let header = *schedule.blocks.get(latch)?.successors.first()?;
    if header > latch {
        return None;
    }
    let (header_ctrl, header_pc) = back_edge_exit_shape(graph, schedule, latch, header).ok()?;
    let merge = graph.nodes.get(header_ctrl as usize)?;
    let sis = chain_by_pc.get(&header_pc).filter(|s| !s.is_empty())?;
    // `merge.inputs[k]` is the control of value `k`, a φ's input `k + 1`.
    let edge = merge
        .inputs
        .iter()
        .position(|&c| ctrl_block_of(graph, schedule, c) == Some(latch));
    let mut values: Vec<NodeId> = Vec::new();
    for &si in sis {
        let Some(sp) = graph.safepoints.get(si) else {
            continue;
        };
        // Where the compiling method's locals sit in `sp.stack`
        // (`ir::SpliceScopeChain`'s layout, as the exit computes it: scope
        // 0's operand stack, then each caller's locals and operand stack,
        // innermost caller first): `(start, end, invoke bci)`.
        let outer_locals = outer_local_dead.and_then(|_| {
            let chain = sp.splice_scope.as_deref()?;
            let n = chain.scopes.len();
            if n < 2 {
                return None;
            }
            let mut at = usize::try_from(chain.scopes[0].stack_len).ok()?;
            for s in &chain.scopes[1..n - 1] {
                at = at
                    .checked_add(usize::try_from(s.locals_len).ok()?)?
                    .checked_add(usize::try_from(s.stack_len).ok()?)?;
            }
            let outer = chain.scopes.last()?;
            let end = at.checked_add(usize::try_from(outer.locals_len).ok()?)?;
            Some((at, end, usize::try_from(outer.bci).ok()?))
        });
        let stack_from = sp.locals.len();
        for (i, &v) in sp
            .locals
            .iter()
            .chain(sp.stack.iter())
            .chain(sp.monitors.iter())
            .enumerate()
        {
            if v == NO_NODE {
                continue;
            }
            if let (Some(dead), Some((start, end, bci))) = (outer_local_dead, outer_locals) {
                let in_stack = i.checked_sub(stack_from).filter(|&s| s < sp.stack.len());
                if let Some(s) = in_stack.filter(|&s| start <= s && s < end) {
                    if dead(bci, s - start) {
                        continue;
                    }
                }
            }
            let header_phi = graph
                .nodes
                .get(v as usize)
                .filter(|n| matches!(n.op, Op::Phi) && n.inputs.first() == Some(&header_ctrl));
            match (header_phi, edge) {
                (Some(phi), Some(k)) => {
                    if let Some(&src) = phi.inputs.get(k + 1) {
                        if src != NO_NODE {
                            values.push(src);
                        }
                    }
                }
                _ => values.push(v),
            }
        }
    }
    Some((sis.clone(), values))
}

/// Each block's EDGE position in the slot plan's linear numbering (one past
/// its last node or terminator; `slot_plan_positions_legacy`'s `span[b].1`),
/// re-derived from the schedule and checked against `pos_of` node by node.
/// `None` -- arm no poll -- when any scheduled node's recorded position
/// disagrees with the re-derivation. Pure.
pub(in crate::ir_lower) fn block_edge_positions(schedule: &Schedule, pos_of: &[Option<usize>]) -> Option<Vec<usize>> {
    let mut out = Vec::with_capacity(schedule.blocks.len());
    let mut seq = 0usize;
    for block in &schedule.blocks {
        for id in block.nodes.iter().copied().chain(block.terminator) {
            match pos_of.get(id as usize).copied().flatten() {
                Some(p) if p == seq => {}
                _ => return None,
            }
            seq += 1;
        }
        out.push(seq);
        seq += 1;
    }
    Some(out)
}

/// gci i4/ir: the index of the block the method ENTERS. `Op::Start` is a
/// control node the scheduler never places (`ir_schedule::build_block_cfg`
/// heads the entry block with Start's control projection `Proj(0)`, and
/// `awaits_placement` skips control nodes), so `node_to_block[graph.entry]`
/// is `usize::MAX`; the entry block is the one headed by `Proj(0)` of
/// `graph.entry`. A placed `graph.entry` (a hand-built schedule) is taken as
/// it is. `None` when neither exists.
pub(in crate::ir_lower) fn scheduled_entry_block(graph: &Graph, schedule: &Schedule) -> Option<usize> {
    let nb = schedule.blocks.len();
    if let Some(&b) = schedule.node_to_block.get(graph.entry as usize) {
        if b < nb {
            return Some(b);
        }
    }
    schedule.blocks.iter().position(|blk| {
        graph.nodes.get(blk.ctrl as usize).is_some_and(|n| {
            matches!(n.op, Op::Proj(0)) && n.inputs.first().copied() == Some(graph.entry)
        })
    })
}

// gci i3/ir (2026-10-12) -- `gci-i2sp-ir-callee-map-names-its-nulled-parameter-home`
// and item 5(a) of `gengc-r5w5-oomjit9-ir-dead-homes-residuals`: a reference
// PARAMETER's prologue word stops being a second, never-cleared home (opt-in).
//
// The prologue stores every incoming argument to `[rbp - (idx+1)*8]`, and
// `Op::Param`'s arm copies a reference parameter from there into its own
// coloured home. Every safepoint map republishes the prologue word
// (`ref_param_homes`, first in `emit_safepoint_map`), the band scan reads it
// (`region=java-local`), and nothing ever clears it: the dead-home clears and
// the liveness claim act on colours only. So a parameter the method is done
// with -- `a = null; ... cleared(ref)` in `Gcd1ArgPinSpecialProbe`'s
// `dropVirtual` / `dropIface` -- keeps its object through every collection of
// the activation (`holder#1 ... off=16 region=java-local tier=ir in_map=true`),
// even where its colour was cleared.
//
// After the copy, the prologue word has no reader that the colour does not
// also serve:
//
//   * frame states: `frame_value_for` describes a parameter by its prologue
//     word only while its node is not yet emitted (`defined_nodes`), and by its
//     colour from then on. With the node in the entry block, block 0, and that
//     block entered from nowhere else, every point described before the copy
//     runs before it (blocks are emitted in index order, and the entry poll
//     precedes them); an OSR entry never runs block 0 and zeroes the locals
//     band itself (`emit_osr_entry_stubs`);
//   * `monitor_object_home`: the prologue word, likewise, only for a node not
//     yet emitted;
//   * the self-locking body's receiver (`self_lock_obj_home`) and the VM's
//     caller-held receiver read (`held_monitors.rs`, `Param(0)`'s word): left
//     out -- parameter 0 of an INSTANCE method is never a candidate. gci
//     i4/ir: a STATIC method's parameter 0 is (`Graph::receiver_param` and
//     `method_monitor_param` both `None`): a static self-lock locks the class
//     mirror, and `held_monitors.rs` reads a static body's mirror, never a
//     frame word (the static-direct case of `Gcd1ArgPinSpecialProbe`,
//     `dropStatic off=8`);
//   * the plan used to look the entry block up as `node_to_block[graph.entry]`,
//     which is `usize::MAX` on every real schedule (`Op::Start` is a control
//     node the scheduler never places), so it admitted nothing (vi3-ir A
//     VOID); gci i4/ir finds it by its head ([`scheduled_entry_block`]);
//   * a second `Op::Param` of the same index would copy the word again: such a
//     graph has no candidate at that index;
//   * the debugger: the facts this rides on (`DeadRefClearBytecode`) are
//     withheld while one may read locals, and the lowerer then plans nothing.
//
// The copy is then the parameter's only frame word, and it is a colour, so the
// default dead-home clears zero it where it is dead and the store-free claim
// covers it. Price: one store per admitted reference parameter per
// activation. A map still names the zeroed word, which reads as null.
//
// Default ON since the gci orchestrator (2026-10-13); `=0` is the old emission
// byte for byte. Read once per compile.



// gci i5/ir (2026-10-12) -- the cost of the switch above (vi4-ir D: median
// 1.026 on BinT, whose `check(Node n)` pays the store on every activation,
// leaves included). `CRATONVM_JIT_IR_REF_PARAM_HOME_ZERO_LAZY=1` (opt-in, only
// with the switch above) moves the zeroing from right after the copy to the
// planned first-byte clears of the parameter's OWN colour
// (`plan_dead_home_clears`): the prologue word is zeroed exactly where the
// planner zeroes the copy, i.e. at the first armed site on each path that
// finds the parameter dead. An activation that reaches no such site (a leaf,
// or a path on which the parameter stays live) pays nothing, and the word is
// then useless to zero anyway: the copy itself holds the parameter there.
//
// Why it is sound: the parameter's colour is defined once, by the copy in the
// entry block (a candidate is scheduled there and its colour must have no
// other occupant), so every planned clear of it lies after the copy on every
// path; that is checked per site (same block: a later position; another
// block: dominated by the copy's), and a candidate with a planned clear that
// fails the check keeps the eager store. After the clear the prologue word
// reads null, as the eager store left it, and every reader after the copy
// reads the copy (`frame_value_for` prefers the node's slot once emitted).
// Only when the planned clears are complete (`dead_home_clears_complete`):
// otherwise the dense per-site stores clear the copy at sites the plan does
// not list, and the eager store stays.
//
// Coverage it gives up: a GC point that is not a planned site -- a back-edge
// poll under `CRATONVM_JIT_IR_POLL_GC_POINT_CLEARS`, whose dense stores clear
// the copy but not the prologue word -- keeps the parameter through the word
// until the next planned clear. Read once per compile.



// JIT round 25 item 2 (2026-10-24) --
// `docs/feature-designs/regex-jit-contract.md`,
// "Round 24", redesign item 2: every
// call publishes every defined reference home, and in `Pattern$GroupHead.match`
// three of its seven words are copies of the same parameters.
//
// What a call publishes beyond what is live after it, and why only one class
// of it can go:
//
//   * a colour dead AT the call is already left out (the precise-liveness
//     claim, `precise_liveness_claim`) and zero (the dead-home clears);
//   * a colour whose last use IS the call (an argument nothing reads after
//     the return) cannot go: the call's own cold paths read it after the GC
//     point -- the callee-deopt service re-stages every argument from its
//     home (`runtime_lowering::emit_callee_deopt_cold`), a local throw pad
//     resumes a handler -- and a word left unpublished but non-zero is stale
//     after a move, which the next unarmed map (poll, monitor, helper op)
//     would then publish as a root;
//   * a reference parameter's PROLOGUE word: once the parameter is copied to
//     its own colour, the lazy zeroing (`ir_ref_param_home_zero_lazy_enabled`)
//     leaves the word holding the same reference until the colour dies, and
//     every map pushes and copies back both words. The prologue word has no
//     reader the copy does not also serve (the section above
//     `ir_ref_param_home_zero_enabled`), so it can be zeroed at any point
//     after the copy.
//
// `CRATONVM_JIT_IR_SHADOW_LIVE_ACROSS_CALL` (default ON since R25-FLIP, `=0` kills): at an armed GC point
// of a scheduled node that the copy precedes on every path
// (`param_copy_precedes_site`), the word is zeroed (`MOV qword [rbp - home],
// 0`, one store in place of a push word and a copy-back word) and left out of
// that safepoint's map and push (`Lowerer::shadow_live_param_home_drops`).
// The word then holds zero or the parameter's current value everywhere: a
// later unarmed map still names and pushes it, and the copy-back keeps it
// current, exactly as before. Retention can only improve (the word stops
// rooting the parameter from that site on; the copy roots it where it is
// live). Not touched: the receiver of an instance method (not a candidate),
// a frame-block body, polls, and every non-armed map.
//
// `CRATONVM_DBG_JITC=1` prints, per armed map, `[ir-shadow-live] method=..
// sp=<id> site=n<node> published_before=<n> published_now=<m> dropped=[..]
// zeroed=[..]`. Read once per compile.



// JIT round 25 item 2, proposal 1 -- the receiver. Parameter 0 of an instance
// method is no `ref_param_home_zero_candidates` member, because two readers use
// its prologue word `[rbp - 8]` after the copy: the self-locking body's
// exceptional exits release its monitor from it (`self_lock_obj_home`), and
// `held_monitors.rs` names a caller-held (wrapped, `requires_wrapped_entry`)
// body's receiver from it under `CRATONVM_JMX_CALLER_HELD_IR_RECEIVER_HOME`,
// requiring every active map to name it. Both exist only for an
// `ACC_SYNCHRONIZED` method: every publication of one stamps
// `requires_wrapped_entry` or self-locks. Every other reader of the word
// (`frame_value_for`, `monitor_object_home` for a `synchronized (this)` block)
// reads it only before the node is emitted, as for any other parameter.
//
// So with `CRATONVM_JIT_IR_SHADOW_LIVE_RECEIVER` (default ON since R25-FLIP, only with
// `CRATONVM_JIT_IR_SHADOW_LIVE_ACROSS_CALL`), an instance method the builder
// stamped `Graph::method_synchronized == Some(false)`, and that does not
// self-lock, gives the drop rule its receiver too. Only the drop rule: the
// eager / lazy zeroing keep their admission. `None` (a hand-built graph) admits
// nothing. Read once per compile.



// JIT round 25 item 2, proposal 3 -- a frame block under precise liveness.
// Since gen r5w3/live7 `plan_frame_block` refused the block whenever
// `CRATONVM_JIT_PRECISE_FRAME_LIVENESS` was on: a block publishes every home
// BY ADDRESS for the whole activation, so a home the claim proves dead at a
// call would stay a precise root through it. That was written for the
// store-free claim. Since JIT round 20 wave 17 (lane gcDead) the claim is made
// only with the converged dead-home plan, because a dropped home must be ZERO
// at the collection (a later unarmed map republishes it), and the planned
// first-byte clears (or, without a complete plan, the dense per-site stores of
// `emit_dead_ref_slot_clears`, which in a block body also clear above the live
// cursor) make it so. A block entry that names a zero word roots nothing, so
// the retention claim holds through the block: the dead homes the claim leaves
// out of the map are exactly as unrooted as in the per-call form. The block's
// own prologue contract (every block word zeroed, the entry-block refinement
// off: `zero_ref_value_slots(false)`) makes the stale-set union's "entry is
// clean" premise hold for those words regardless of the fresh-frame clear.
//
// Gain: no per-call push and no copy-back at all -- one by-address publication
// per activation (two instructions per home, behind the shared subroutine) and
// a `CMP` / `JNE` per GC point; the maps still drop the dead homes and the
// liveness table is unchanged. Bodies over the home cap keep the per-call form.
//
// `CRATONVM_JIT_IR_SHADOW_FRAME_BLOCK_PRECISE` (default ON since R25-FLIP, `=0` kills; needs
// `CRATONVM_JIT_IR_SHADOW_FRAME_BLOCK`, default on). Read once per compile.



// JIT round 25 item 2, wave 3 -- the frame block's words, cleared lazily.
//
// A frame block names its words BY ADDRESS from its first publication to the
// return, so each must hold a reference or zero at every GC point this
// activation reaches; that is why the prologue clears them. A word that is
// WRITTEN, on every path, before every GC point needs no clear: the first
// collection that can see it finds this activation's own reference there. Once
// written a block word stays a reference or zero (only its colour's `Ref`
// occupants, their copy-backs and the dead-home clears write it, and the
// collector rewrites it through the block).
//
// `CRATONVM_JIT_IR_SHADOW_FRAME_BLOCK_LAZY_CLEAR=1` (opt-in; only in a body that
// publishes a frame block, only from the prologue, never under a MIR mode or on
// the keep-everything retry) leaves such words out of the fresh-frame clear and
// out of `zero_ref_value_slots` (`Lowerer::plan_block_lazy_clear`):
//
//   * a candidate is a block word no phi lives in, written by a `Ref` node
//     whose own arm stores its home (not home-dropped, scheduled, used);
//   * a must-written dataflow over the schedule's CFG from block 0 (the entry,
//     entered from nowhere else) keeps a candidate only if it is written
//     before every GC point -- every scheduled node `op_cannot_deopt` does not
//     clear (before that node's own write), every terminator that may call,
//     and every back-edge poll ([`block_lazy_clear_survivors`]);
//   * the method-entry poll, which runs before any of the body, zeroes the
//     words on its slow path, the only way into its safepoint;
//   * a transfer that leaves the CFG -- a throw pad into a handler, a catch
//     probe -- starts at a node that is itself a GC point, where the words were
//     already written; an OSR entry stub clears the whole frame itself.
//
// The plan rests on the writes being emitted: `store_rax` records each own
// home write while a plan exists, and `verify_block_lazy_clear_writes` refuses
// the attempt if a counted write is missing (the ladder lowers once more
// without the plan). `CRATONVM_DBG_JITC=1` prints `[ir-block-lazy-clear]` per
// body. Read once per compile.

/// `CRATONVM_JIT_IR_SHADOW_FRAME_BLOCK_LAZY_CLEAR=0`: see the section above.
pub(in crate::ir_lower) fn ir_shadow_frame_block_lazy_clear_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_SHADOW_FRAME_BLOCK_LAZY_CLEAR")
}

// JIT round 25 item 2, wave 4 -- reference homes without definition stores
// (the first slice of the HotSpot shape: `docs/feature-designs/native-handle-discipline.md`, "Wave 4").
//
// Under `CRATONVM_JIT_IR_REF_RESIDENCY` a reference in a callee-saved register
// is a write-through cache: its home is stored at the definition, and the copy
// is dropped after every node that may call, because a moving collection
// rewrites the home and not the register. `CRATONVM_JIT_IR_LAZY_REF_HOMES=1`
// (opt-in; needs ref residency, publish-at-definition and register phi copies;
// never under a MIR mode or on the keep-everything retry) drops the store for a
// field-load result `v` (`Lowerer::plan_lazy_ref_homes`) when:
//
//   * `v` is in a callee-saved register of the plan's file, not split-managed,
//     not a coalesced phi source, and no reachable deopt names it (its frame
//     states would name the home);
//   * every position a collection may run at inside `v`'s colouring range
//     `(lo, hi]` is a HOOKED node: a field load (`Op::Load`) or a no-call
//     primitive array op, at a bci no handler of this frame covers and with no
//     implicit-throw pad. Calls, allocations, stores, polls, calling
//     terminators and every other non-allowlisted node block it.
//
// A hooked node keeps `v` in its register across its fast path. Each of its
// helper calls (`emit_getfield_checked_helper_call`, the plain `getfield` arm,
// the outlined getfield tail) is bracketed: `v` is stored to its home first --
// so the frame block, or the band scan, holds it for the helper's collection --
// and reloaded after (`lazy_ref_hook_enter` / `_exit`). After the node, the
// node-end invalidation is undone for `v` (`Lowerer::lazy_ref_after_node`),
// only when the node's code holds no call outside a hooked span (a
// conservative byte scan, [`code_has_unhooked_call`]); else the attempt is
// refused naming `v`, and the ladder lowers once more without it.
//
// Every other reader of the home refuses rather than reads it: `v` is
// home-dropped, so `slot_of` and `frame_value_for` latch a bailout naming it
// (`note_elide_refusal` then excludes it from the next attempt). The home word
// itself only ever holds `v` (spilled) or what its colour held before (a
// reference or zero), so a map or a frame block that names it at a GC point
// outside `v`'s range names a valid word; the lazy clear never counts `v`'s
// definition as a write (it is home-dropped).
//
// `CRATONVM_DBG_JITC=1` prints `[ir-lazy-ref] method=.. ref_def_stores_emitted=N
// ref_def_stores_removed=M hooked_nodes=K spill_stores=S blocked_by_calls_only=C
// blocked_other=O` per body: the last two count the otherwise-eligible values a
// real call alone, or anything else, keeps out (wave 5: why real calls are not
// taken is in the residuals page). Read once per compile.

/// `CRATONVM_JIT_IR_LAZY_REF_HOMES=0`: see the section above.
pub(in crate::ir_lower) fn ir_lazy_ref_homes_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_LAZY_REF_HOMES")
}

/// How a node inside a lazy reference's range is treated (the section above).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ir_lower) enum LazyRefNodeClass {
    /// `op_cannot_deopt`: no call, no trap, no collection.
    Transparent,
    /// A field load (helper calls hooked) or a primitive array op (no call;
    /// checked by the scan).
    Hookable,
    /// Anything else may collect without a hook.
    Blocking,
}

/// [`LazyRefNodeClass`] of `op`. Pure.
pub(in crate::ir_lower) fn lazy_ref_node_class(op: &Op) -> LazyRefNodeClass {
    if op_cannot_deopt(op) {
        return LazyRefNodeClass::Transparent;
    }
    match op {
        Op::Load(_) | Op::ArrayLength => LazyRefNodeClass::Hookable,
        Op::ArrayLoad(k) | Op::ArrayStore(k) if !matches!(k, MemKind::Ref) => {
            LazyRefNodeClass::Hookable
        }
        _ => LazyRefNodeClass::Blocking,
    }
}

/// Does `code` (emitted at buffer offset `base`) hold a `CALL` through a
/// register or memory (`FF /2`: `FF` then a ModRM whose reg field is 2, any
/// REX before it) at an offset outside every `[start, end)` of `hooked`?
/// Conservative in the safe direction: bytes that merely look like one count.
/// A `CALL rel32` (`E8`) is not looked for: none of the hooked emitters writes
/// one (the source audit test below pins that). Pure.
pub(in crate::ir_lower) fn code_has_unhooked_call(code: &[u8], base: usize, hooked: &[(usize, usize)]) -> bool {
    code.windows(2).enumerate().any(|(i, w)| {
        w[0] == 0xFF && (w[1] >> 3) & 7 == 2 && {
            let at = base + i;
            !hooked.iter().any(|&(s, e)| at >= s && at < e)
        }
    })
}

/// One event of a block, in schedule order, for [`block_lazy_clear_survivors`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ir_lower) enum LazyClearStep {
    /// A point a collection may run at.
    Gc,
    /// The candidate with this index is written.
    Write(usize),
}

/// A schedule block as [`block_lazy_clear_survivors`] reads it.
#[derive(Clone, Debug, Default)]
pub(in crate::ir_lower) struct LazyClearBlock {
    pub(in crate::ir_lower) preds: Vec<usize>,
    pub(in crate::ir_lower) steps: Vec<LazyClearStep>,
    /// A GC point after every step (a back-edge poll, a calling terminator).
    pub(in crate::ir_lower) gc_at_end: bool,
}

