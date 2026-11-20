#[allow(unused_imports)]
use super::*;

/// Rewrite the loop `plan` describes into the in-place shape (see the section
/// comment). Every check ran in [`plan_loop_block_coarsening`]; nothing here
/// refuses. `bails` counts the early exits (loop tests and identity tests).
pub(in crate::ir_optimize) fn apply_loop_block_coarsening_in_place(
    graph: &mut Graph,
    plan: &LoopBlockPlan,
) -> LoopBlockApplied {
    let k = LOOP_LOCK_COARSEN_FACTOR;
    let (read_op, read_pc, read_inputs) = {
        let r = &graph.nodes[plan.read as usize];
        let inputs: Vec<NodeId> = r.inputs.iter().copied().collect();
        (r.op.clone(), r.bytecode_pc, inputs)
    };
    // Round 17 wave 2 (lane monitor2): a trusted `static final` monitor is
    // copy 0's read for the whole group.
    let rereads = plan.rereads_per_copy();
    // j27 wave 4 (lane MONROWS, MON17W2-2): copy 0 is the original body, so
    // its read proves the base for the copies that follow it.
    monitor_rows::note_field_monitor_base_proof(
        graph,
        plan.monitor == BlockMonitor::Field,
        plan.read,
    );
    let (x_pc, x_tok) = {
        let x = &graph.nodes[plan.exit as usize];
        (x.bytecode_pc, x.inputs[1])
    };
    let (cond_op, cond_ty, cond_inputs) = {
        let c = &graph.nodes[plan.cond as usize];
        let inputs: Vec<NodeId> = c.inputs.iter().copied().collect();
        (c.op.clone(), c.ty, inputs)
    };
    let mut frames = 0usize;
    let mut bails = 0usize;
    // Copy 0 is the original body: what leaves it is what the header φs' back
    // edges carry, its end is the back edge's control, its last memory effect
    // the exit's token.
    let mut cur: FxHashMap<NodeId, NodeId> = plan
        .carried
        .iter()
        .copied()
        .filter(|&p| p != plan.mem_phi)
        .map(|p| (p, graph.nodes[p as usize].inputs[plan.back_slot]))
        .collect();
    let mut start = plan.back_ctrl;
    let mut window_tok = x_tok;
    // Copy 0's frame at the block's `monitorexit` is the builder's own.
    let mut last_exit_frame: Option<u32> = x_pc
        .and_then(|pc| plan.frames.get(&pc))
        .and_then(|&si| u32::try_from(si).ok());
    for _ in 1..k {
        // The loop test of the iteration this copy runs.
        let iv_j = cur.get(&plan.iv_phi).copied().unwrap_or(plan.iv_phi);
        let ins: Vec<NodeId> = cond_inputs
            .iter()
            .map(|&i| if i == plan.iv_phi { iv_j } else { i })
            .collect();
        let cond_j = graph.add(cond_op.clone(), cond_ty, ins, None);
        // No bytecode pc: no profile describes this branch, and no OSR entry
        // may land on either arm.
        let test = graph.add(Op::If, IrType::Control, vec![start, cond_j], None);
        let p0 = graph.add(Op::Proj(0), IrType::Control, vec![test], None);
        let p1 = graph.add(Op::Proj(1), IrType::Control, vec![test], None);
        let (go, out) = if plan.body_proj == 0 {
            (p0, p1)
        } else {
            (p1, p0)
        };
        loop_block_exit_to_header(graph, plan, out, window_tok, x_pc, last_exit_frame, &cur);
        bails += 1;
        let (entry, entry_tok) = if rereads {
            // The read's own inputs past control and memory (a getfield's
            // base and offset) are invariant.
            let mut ins = read_inputs.clone();
            ins[0] = go;
            ins[1] = window_tok;
            let lj = graph.add(read_op.clone(), IrType::Ref, ins, read_pc);
            let same = graph.add(Op::Cmp(CmpOp::Eq), IrType::Int, vec![lj, plan.read], None);
            let idt = graph.add(Op::If, IrType::Control, vec![go, same], None);
            let ok = graph.add(Op::Proj(0), IrType::Control, vec![idt], None);
            let other = graph.add(Op::Proj(1), IrType::Control, vec![idt], None);
            loop_block_exit_to_header(graph, plan, other, lj, x_pc, last_exit_frame, &cur);
            bails += 1;
            (ok, lj)
        } else {
            (go, window_tok)
        };
        // The copy: the body renamed onto this iteration's values. The monitor
        // is copy 0's own node, which every copy keeps.
        let mut subst: FxHashMap<NodeId, NodeId> = FxHashMap::default();
        subst.insert(plan.body_entry, entry);
        subst.insert(plan.enter, entry_tok);
        for (&p, &v) in &cur {
            subst.insert(p, v);
        }
        loop_block_clone_all(graph, &plan.clone_order, &mut subst);
        let mut at: FxHashMap<usize, u32> = FxHashMap::default();
        // Every leaving clone, and every other clone whose pc has a frame
        // (round 17 wave 2, lane monitor2).
        for &v in plan.leaving.iter().chain(plan.quiet.iter()) {
            let Some(pc) = graph.nodes[v as usize].bytecode_pc else {
                continue;
            };
            let (Some(&si), Some(&clone)) = (plan.frames.get(&pc), subst.get(&v)) else {
                continue;
            };
            let snap = match at.get(&pc) {
                Some(&s) => s,
                None => {
                    let s = loop_block_copy_frame(graph, si, &subst);
                    at.insert(pc, s);
                    frames += 1;
                    s
                }
            };
            loop_block_stamp(graph, clone, snap);
        }
        if let Some(pc) = x_pc {
            if let Some(&si) = plan.frames.get(&pc) {
                let snap = match at.get(&pc) {
                    Some(&s) => s,
                    None => {
                        frames += 1;
                        loop_block_copy_frame(graph, si, &subst)
                    }
                };
                last_exit_frame = Some(snap);
            }
        }
        window_tok = subst.get(&x_tok).copied().unwrap_or(x_tok);
        start = subst.get(&plan.back_ctrl).copied().unwrap_or(start);
        let next: Vec<(NodeId, NodeId)> = cur
            .keys()
            .map(|&p| {
                let back = graph.nodes[p as usize].inputs[plan.back_slot];
                (p, subst.get(&back).copied().unwrap_or(back))
            })
            .collect();
        cur.extend(next);
    }
    // The group's end: one release, then the header's back edge, each φ taking
    // its value after the last copy. The original exit has no user left.
    let exit = graph.add(
        Op::MonitorExit,
        IrType::Memory,
        vec![start, window_tok, plan.read],
        x_pc,
    );
    if let Some(snap) = last_exit_frame {
        loop_block_stamp(graph, exit, snap);
    }
    graph.set_input(plan.header, plan.back_slot - 1, start);
    for &p in &plan.carried {
        let v = if p == plan.mem_phi {
            exit
        } else {
            cur.get(&p).copied().unwrap_or(p)
        };
        graph.set_input(p, plan.back_slot, v);
    }
    graph.kill(plan.exit);
    LoopBlockApplied {
        enter: plan.enter,
        exit,
        frames,
        bails,
    }
}

// ── Nested-lock elision: `enter(o) .. enter(o) .. exit(o) .. exit(o)` ─────
//
// Round 11 wave 14 (lane fib; `r11w9-mid-nested-lock-elision-needs-a-partial-
// relock-20260924.md`, design in `r11w13-fib-nested-lock-elision-split-monitor-
// entry-patch-20260924.md`). HotSpot's `EliminateNestedLocks`: the inner
// `monitorenter(o)` / `monitorexit(o)` of `synchronized (o) { .. synchronized
// (o) { .. } .. }` are deleted.
//
// # Why it is sound under the JMM
//
// While the outer region holds `o`, no other thread can acquire it. The inner
// acquire therefore synchronizes-with no release by another thread, and the
// inner release is observed by nobody before the outer one. Deleting the pair
// removes no happens-before edge between threads, for ANY `o`, escaping or not.
// `wait`/`notify` in the inner region behave the same at hold count 1 as at 2
// (a wait releases every level and restores them all), and `Thread.holdsLock`
// answers the same.
//
// # How a frame inside the inner region is described
//
// The builder's snapshots still name `o` twice there. The pass sets the bit of
// each entry whose region it deleted in `SafepointSnapshot::monitor_elided_mask`,
// and `ir_lower::resolve_monitors` publishes `{o, held, relock: false}` then
// `{o, elided, relock: true}`. The consumers that resume honour `relock` per
// ENTRY (`deopt_resume::build_deopt_frame_inner`,
// `exception_dispatch::materialize_and_relock_precise_frame`), so the thread
// ends up holding `o` exactly as often as the interpreter frame will release
// it. Every consumer that cannot re-acquire refuses a `relock` entry and fails
// CLOSED: the install verifier (it refuses two entries on one object until the
// split patch lands), a reason-9 rethrow pad (`ir_lower::rethrow_frame_refusal`
// refuses the compile -- which also keeps the split away from the reason-9
// drain of an `ACC_SYNCHRONIZED` method, which maps locals without relocking),
// the OSR entry contract and the OSR exit admission (`osr_entry.rs`), and the
// in-place OSR transfers. Frames in this tier are flat (a splice resumes at the
// caller's `invoke`), so no caller scope carries the split.
//
// # Which regions are nested
//
// Structured locking (the builder refuses anything else) means the monitor
// stack a snapshot records at the TOP of a `monitorenter` is the stack before
// it, and at the top of a `monitorexit` the stack with the released region
// innermost. So a bytecode `monitorenter` on reference node `o` is nested
// exactly when its own bci's snapshot already holds `o` (the same NODE), and a
// `monitorexit` exactly when its snapshot's innermost entry is `o` and a deeper
// entry is too. An object's nested regions are deleted all together or not at
// all, and an entry is marked by the same rule (an earlier entry of the same
// snapshot names the same node), so the marks and the deletions agree:
//
// * a region's enter, its exits and every snapshot inside it see the stack
//   prefix pushed on its path; a join adopts its first predecessor's stack,
//   which the builder checked names the same objects up to trivial φs;
// * so an object that any snapshot or monitor op names through a DIFFERENT
//   node stripping to the same value is refused whole -- its entries could be
//   exact on one path and not on another;
// * a caller-held synchronized CALL's pair (`enter; Call; exit`, all on the
//   invoke's pc, never on the monitor stack and never in a frame) is left
//   alone: `ir_lower` serves that call only with the pair in place; so is a
//   synchronized splice's pair (`Graph::sync_splice_windows`, round 14 wave
//   3), for the same reason, recognised by node since it has no `Call`;
// * an op without a pc, without a snapshot at its bci, or whose snapshots
//   disagree, an entry deeper than 63, and a pair that cannot be spliced out
//   of the memory chain each refuse the object.
//
// Default on since round 11 wave 17 (`CRATONVM_JIT_IR_NESTED_LOCK_ELIM=0` is
// the kill switch). The install verifier admits the held-then-re-take split
// since wave 15 (lane `ir`), an IR OSR entry refuses to land inside an elided
// region since wave 16, and the wave-16 and wave-17 flag matrices read the
// switched-on arm identical to the default (probe `R11FibNestedLock` deopts
// inside the elided region).

/// `id` through φs that merge only one value (and themselves): the builder's
/// `IrBuilder::strip_trivial_phis`, which is what decides that two monitor
/// stack entries at a join name one object.
pub(in crate::ir_optimize) fn strip_trivial_monitor_phis(graph: &Graph, id: NodeId) -> NodeId {
    let mut cur = id;
    for _ in 0..32 {
        let Some(node) = graph.node_opt(cur) else {
            return cur;
        };
        if node.op != Op::Phi || node.ty == IrType::Memory {
            return cur;
        }
        let mut only: Option<NodeId> = None;
        for v in node.phi_value_inputs() {
            let Some(v) = v else {
                return cur;
            };
            if v == cur {
                continue;
            }
            match only {
                None => only = Some(v),
                Some(o) if o == v => {}
                Some(_) => return cur,
            }
        }
        match only {
            Some(next) => cur = next,
            None => return cur,
        }
    }
    cur
}

/// One object's nested regions, as [`elide_nested_monitors`] collects them.
#[derive(Default)]
pub(in crate::ir_optimize) struct NestedLockPlan {
    /// The `MonitorEnter` / `MonitorExit` nodes of its nested regions.
    pub(in crate::ir_optimize) ops: Vec<NodeId>,
    /// How many of `ops` are enters.
    pub(in crate::ir_optimize) enters: usize,
    /// Something about the object refuses the whole plan.
    pub(in crate::ir_optimize) refused: bool,
}

/// Can monitor op `op` be spliced out of the memory chain: a well-formed op
/// with a live incoming token, no frame state naming it, and every user naming
/// it only as a memory token?
pub(in crate::ir_optimize) fn nested_monitor_op_spliceable(graph: &Graph, op: NodeId) -> bool {
    let Some(n) = graph.node_opt(op) else {
        return false;
    };
    if !matches!(n.op, Op::MonitorEnter | Op::MonitorExit) || n.inputs.len() != 3 {
        return false;
    }
    let Some(token) = n.input_opt(1) else {
        return false;
    };
    if !graph.node_opt(token).is_some_and(|t| t.op != Op::Dead) {
        return false;
    }
    if !graph.safepoint_users_of(op).is_empty() {
        return false;
    }
    graph
        .users_of(op)
        .into_iter()
        .all(|u| match graph.node_opt(u) {
            None => true,
            Some(un) if un.op == Op::Dead => true,
            Some(un) => un
                .inputs
                .iter()
                .enumerate()
                .all(|(i, &inp)| inp != op || is_memory_token_slot(un, i)),
        })
}

/// Delete every nested (re-entrant) monitor region, per object, and mark the
/// snapshot entries that named it. Returns the number of regions (enters)
/// deleted. See the section comment above for the soundness argument and for
/// every refusal.
pub(crate) fn elide_nested_monitors(graph: &mut Graph) -> usize {
    if graph.safepoints.is_empty() {
        return 0;
    }
    let mut monitor_ops: Vec<NodeId> = Vec::new();
    let mut call_pcs: FxHashSet<usize> = FxHashSet::default();
    // Round 14 wave 3 (lane sync; page
    // `r14w2-syncsplice-nested-lock-elision-unstacked-monitor-ops-patch-FIXED-20260929.md`):
    // a synchronized splice's pair is on no frame state, so its bci's snapshot
    // says nothing about it (inside a held region its enter would read as
    // nested and its exit would not). `ir_lower` serves it only with both
    // halves in place, so it is skipped by node, as the caller-held CALL's
    // pair is by pc below.
    //
    // Round 15 wave 1 (lane sync, SS8-2): so is a self-locking body's method
    // monitor (`Graph::method_self_lock`). Its enter sits under a bci-0
    // snapshot that already names it (it would read as nested and be
    // deleted, leaving every exit to release a lock never taken), and
    // `ir_lower` requires every one of its ops in place.
    let unstacked: FxHashSet<NodeId> = graph
        .sync_splice_windows
        .iter()
        .flat_map(|w| [w.enter, w.exit])
        .chain(
            graph
                .method_self_lock
                .iter()
                .flat_map(|lock| std::iter::once(lock.enter).chain(lock.exits.iter().copied())),
        )
        .filter(|&n| n != NO_NODE)
        .collect();
    for (id, n) in graph.nodes.iter().enumerate() {
        match n.op {
            Op::MonitorEnter | Op::MonitorExit => {
                if n.inputs.len() != 3 {
                    return 0;
                }
                if unstacked.contains(&(id as NodeId)) {
                    continue;
                }
                monitor_ops.push(id as NodeId);
            }
            Op::Call { .. } => {
                if let Some(pc) = n.bytecode_pc {
                    call_pcs.insert(pc);
                }
            }
            _ => {}
        }
    }
    if monitor_ops.len() < 2 {
        return 0;
    }
    let mut by_bci: FxHashMap<usize, Vec<usize>> = FxHashMap::default();
    for (si, sp) in graph.safepoints.iter().enumerate() {
        by_bci.entry(sp.bci).or_default().push(si);
    }

    // Classify every bytecode monitor op by its own bci's snapshots.
    let mut plans: FxHashMap<NodeId, NestedLockPlan> = FxHashMap::default();
    for &id in &monitor_ops {
        let Some(n) = graph.node_opt(id) else {
            continue;
        };
        let is_enter = n.op == Op::MonitorEnter;
        let Some(obj) = n.input_opt(2) else {
            return 0;
        };
        let Some(pc) = n.bytecode_pc else {
            plans.entry(obj).or_default().refused = true;
            continue;
        };
        if call_pcs.contains(&pc) {
            continue;
        }
        let mut verdict: Option<bool> = None;
        let mut agree = true;
        for &si in by_bci.get(&pc).map(Vec::as_slice).unwrap_or(&[]) {
            let Some(sp) = graph.safepoints.get(si) else {
                agree = false;
                break;
            };
            let nested = if is_enter {
                sp.monitors.contains(&obj)
            } else {
                match sp.monitors.split_last() {
                    Some((&top, below)) if top == obj => below.contains(&obj),
                    _ => {
                        agree = false;
                        break;
                    }
                }
            };
            if let Some(v) = verdict {
                if v != nested {
                    agree = false;
                    break;
                }
            } else {
                verdict = Some(nested);
            }
        }
        let plan = plans.entry(obj).or_default();
        match (agree, verdict) {
            (true, Some(true)) => {
                plan.ops.push(id);
                if is_enter {
                    plan.enters += 1;
                }
            }
            (true, Some(false)) => {}
            // No snapshot at the bci, or snapshots that disagree.
            _ => plan.refused = true,
        }
    }
    if plans.values().all(|p| p.refused || p.enters == 0) {
        return 0;
    }

    // Refuse an object any frame or monitor op names through a second node
    // that strips to the same value.
    let mut first_by_root: FxHashMap<NodeId, NodeId> = FxHashMap::default();
    let mut aliased_roots: FxHashSet<NodeId> = FxHashSet::default();
    let named = graph
        .safepoints
        .iter()
        .flat_map(|sp| sp.monitors.iter().copied())
        .chain(
            monitor_ops
                .iter()
                .filter_map(|&id| graph.node_opt(id).and_then(|n| n.input_opt(2))),
        );
    for node in named {
        if node == NO_NODE {
            continue;
        }
        let root = strip_trivial_monitor_phis(graph, node);
        let first = *first_by_root.entry(root).or_insert(node);
        if first != node {
            aliased_roots.insert(root);
        }
    }

    let mut objects: Vec<NodeId> = plans
        .iter()
        .filter(|(_, p)| !p.refused && p.enters > 0)
        .map(|(&o, _)| o)
        .collect();
    objects.sort_unstable();
    let mut regions = 0usize;
    for obj in objects {
        if aliased_roots.contains(&strip_trivial_monitor_phis(graph, obj)) {
            continue;
        }
        let Some(plan) = plans.get(&obj) else {
            continue;
        };
        if !plan
            .ops
            .iter()
            .all(|&op| nested_monitor_op_spliceable(graph, op))
        {
            continue;
        }
        // The entries to mark, computed before anything is deleted so a
        // too-deep entry refuses the object with the graph untouched.
        let mut marks: Vec<(usize, u64)> = Vec::new();
        let mut too_deep = false;
        for (si, sp) in graph.safepoints.iter().enumerate() {
            let mut seen = false;
            let mut bits = 0u64;
            for (k, &m) in sp.monitors.iter().enumerate() {
                if m != obj {
                    continue;
                }
                if seen {
                    if k >= 64 {
                        too_deep = true;
                        break;
                    }
                    bits |= 1u64 << k;
                }
                seen = true;
            }
            if too_deep {
                break;
            }
            if bits != 0 {
                marks.push((si, bits));
            }
        }
        if too_deep {
            continue;
        }
        // Splice each op out of the memory chain, reading its token afresh:
        // an earlier splice may have rewired it to that op's own token.
        // Every op was checked above, and a splice only ever hands a user a
        // live token, so none of them is skipped.
        for &op in &plan.ops {
            let Some(token) = graph.node_opt(op).and_then(|n| n.input_opt(1)) else {
                continue;
            };
            graph.replace_all_uses(op, token);
            graph.kill(op);
        }
        for (si, bits) in marks {
            // Not through `graph.safepoints`: a mutable borrow of that field
            // bumps the edge epoch (round 11 wave 18), and the mask is not a
            // node reference the def-use lists need to hear about.
            let _ = graph.or_safepoint_monitor_elided_mask(si, bits);
        }
        regions += plan.enters;
    }
    if regions > 0 && false {
        eprintln!("[cratonvm-jitc] ir nested-lock elision: deleted {regions} inner region(s)");
    }
    regions
}

/// Delete BOTH halves of every synchronized splice window
/// (`Graph::sync_splice_windows`) that sits inside a region this frame holds
/// on the same object -- `synchronized (o) { o.leaf(); }` with `leaf` a
/// synchronized splice -- and drop the window from the list. Returns the
/// number of windows deleted.
///
/// The JMM argument is [`elide_nested_monitors`]': while the region holds `o`
/// no other thread can acquire it, so the window's recursive acquire and
/// release synchronize with nobody, and every access in the window stays
/// inside the region, whose own pair is untouched. `Thread.holdsLock` answers
/// the same, and the window holds no call (no `wait`/`notify`).
///
/// Unlike a bytecode region, a window is on no frame state, so nothing is
/// marked: a frame inside the region already names `o` once, which is the
/// hold that remains. "Inside a held region" is read from the snapshots at the
/// window's own bci (the caller's `invoke`, whose snapshot is taken at the top
/// of the instruction with the caller's monitor stack): EVERY one must name
/// the window's object (through trivial φs), and there must be one. A window
/// is deleted only whole, when both halves are live, name one object at one
/// bci, and can be spliced out of the memory chain
/// ([`nested_monitor_op_spliceable`]); `ir_lower` never sees a half of it.
///
/// Round 14 wave 5 (lane sync5, SS8-3): a window whose object is the
/// compiling synchronized method's receiver (`Graph::method_monitor_param`,
/// through trivial φs of the finished graph) is held for the whole body by
/// whoever entered it, with no snapshot to say so; it is deleted by the same
/// rules without the snapshot test.
///
/// Round 14 wave 7 (lane sync7, RV6-1): likewise a `static synchronized`
/// window whose `static_class_id` is the class whose mirror the compiling
/// `static synchronized` method holds (`Graph::method_monitor_class`): the
/// window locks the mirror of the class declaring its callee (JVMS 2.11.10),
/// which is that same class, so its pair is recursive on a hold the whole body
/// runs under.
pub(crate) fn elide_region_nested_sync_windows(graph: &mut Graph) -> usize {
    let method_monitor = graph
        .method_monitor_node()
        .map(|m| strip_trivial_monitor_phis(graph, m));
    let method_monitor_class = graph.method_monitor_class.filter(|&c| c != 0);
    if graph.sync_splice_windows.is_empty()
        || (graph.safepoints.is_empty()
            && method_monitor.is_none()
            && method_monitor_class.is_none())
    {
        return 0;
    }
    let mut by_bci: FxHashMap<usize, Vec<usize>> = FxHashMap::default();
    for (si, sp) in graph.safepoints.iter().enumerate() {
        by_bci.entry(sp.bci).or_default().push(si);
    }
    let windows = graph.sync_splice_windows.clone();
    let mut deleted: Vec<NodeId> = Vec::new();
    for w in windows {
        if w.enter == NO_NODE || w.exit == NO_NODE {
            continue;
        }
        let (Some(enter), Some(exit)) = (graph.node_opt(w.enter), graph.node_opt(w.exit)) else {
            continue;
        };
        if enter.op != Op::MonitorEnter || exit.op != Op::MonitorExit {
            continue;
        }
        let (Some(obj), Some(exit_obj), Some(pc)) =
            (enter.input_opt(2), exit.input_opt(2), enter.bytecode_pc)
        else {
            continue;
        };
        if exit.bytecode_pc != Some(pc) {
            continue;
        }
        let root = strip_trivial_monitor_phis(graph, obj);
        if strip_trivial_monitor_phis(graph, exit_obj) != root {
            continue;
        }
        let snapshots = by_bci.get(&pc).map(Vec::as_slice).unwrap_or(&[]);
        let in_method = method_monitor == Some(root);
        let in_static_method =
            !in_method && w.static_class_id != 0 && method_monitor_class == Some(w.static_class_id);
        let held = in_method
            || in_static_method
            || (!snapshots.is_empty()
                && snapshots.iter().all(|&si| {
                    graph.safepoints.get(si).is_some_and(|sp| {
                        sp.monitors
                            .iter()
                            .any(|&m| m != NO_NODE && strip_trivial_monitor_phis(graph, m) == root)
                    })
                }));
        if !held
            || !nested_monitor_op_spliceable(graph, w.enter)
            || !nested_monitor_op_spliceable(graph, w.exit)
        {
            continue;
        }
        // The enter first; the exit's token is read afresh, since it may
        // have been the enter.
        for op in [w.enter, w.exit] {
            let Some(token) = graph.node_opt(op).and_then(|n| n.input_opt(1)) else {
                continue;
            };
            graph.replace_all_uses(op, token);
            graph.kill(op);
        }
        deleted.push(w.enter);
        crate::ir::note_sync_splice_census(if in_method {
            crate::ir::SYNC_CENSUS_WINDOW_ELIDED_IN_METHOD
        } else if in_static_method {
            crate::ir::SYNC_CENSUS_WINDOW_ELIDED_IN_STATIC_METHOD
        } else {
            crate::ir::SYNC_CENSUS_WINDOW_ELIDED
        });
    }
    if deleted.is_empty() {
        return 0;
    }
    graph
        .sync_splice_windows
        .retain(|w| !deleted.contains(&w.enter));
    if false {
        eprintln!(
            "[cratonvm-jitc] ir nested-lock elision: deleted {} synchronized splice window(s) \
             inside a held region",
            deleted.len()
        );
    }
    deleted.len()
}

/// Round 14 wave 6 (lane sync6, proposal S5-2 of `jit-r14-sync5-proposals-RETIRED-20260930.md`):
/// replace every `Thread.holdsLock(o)` CALL whose `o` strips, through trivial
/// φs of the FINISHED graph, to the compiling synchronized method's receiver
/// (`Graph::method_monitor_param`) by the constant `1`. Returns how many were
/// replaced. Run under `ir::ir_holds_lock_method_fold_enabled`.
///
/// Sound for the reason the builder's in-method fold is (SS8-3): every
/// execution of this graph runs with that monitor held by the executing
/// thread, and the graph cannot release it (a `wait` gives it back only inside
/// its own call, and re-takes it before returning). What the builder could not
/// do is read a loop-header φ whose back edges were still pending
/// (`IrBuilder::holds_lock_same_object`); here every φ is complete, so a φ
/// whose value inputs are all the receiver (or itself) IS the receiver on
/// every iteration. `Thread.holdsLock` is not a synchronization action, so no
/// ordering is lost: the call leaves the memory chain (each user naming it as
/// a token takes the call's own incoming token), and every value use --
/// snapshot slots included -- reads the constant.
///
/// A call is kept when its row is not the `holdsLock` row, its shape is not
/// `[ctrl, mem, arg]`, or its incoming token is missing or dead.
pub(crate) fn fold_method_monitor_holds_lock(graph: &mut Graph) -> usize {
    let Some(receiver) = graph
        .method_monitor_node()
        .map(|m| strip_trivial_monitor_phis(graph, m))
    else {
        return 0;
    };
    let candidates: Vec<NodeId> = graph
        .nodes
        .iter()
        .enumerate()
        .filter_map(|(id, n)| match n.op {
            Op::Call { info_ptr }
                if n.inputs.len() == 3 && crate::ir::is_holds_lock_info_row(info_ptr) =>
            {
                Some(id as NodeId)
            }
            _ => None,
        })
        .collect();
    let mut folded = 0usize;
    for call in candidates {
        let Some(n) = graph.node_opt(call) else {
            continue;
        };
        let (Some(token), Some(arg)) = (n.input_opt(1), n.input_opt(2)) else {
            continue;
        };
        if !graph.node_opt(token).is_some_and(|t| t.op != Op::Dead)
            || strip_trivial_monitor_phis(graph, arg) != receiver
        {
            continue;
        }
        let mut token_slots: Vec<(NodeId, usize)> = Vec::new();
        for u in graph.users_of(call) {
            let Some(un) = graph.node_opt(u) else {
                continue;
            };
            for (i, &inp) in un.inputs.iter().enumerate() {
                if inp == call && is_memory_token_slot(un, i) {
                    token_slots.push((u, i));
                }
            }
        }
        let one = graph.add(Op::Const(1), IrType::Int, vec![], None);
        graph.replace_all_uses(call, one);
        for (u, i) in token_slots {
            graph.set_input(u, i, token);
        }
        graph.kill(call);
        folded += 1;
        crate::ir::note_sync_splice_census(crate::ir::SYNC_CENSUS_HOLDSLOCK_FINISHED_GRAPH);
    }
    if folded > 0 && false {
        eprintln!(
            "[cratonvm-jitc] ir holdsLock fold on the finished graph: {folded} call(s) on the \
             synchronized method's receiver"
        );
    }
    folded
}

/// Collapse a strongly connected set of value phis that carries one outside
/// value (`CRATONVM_JIT_IR_PHI_SCC_COLLAPSE`, default ON).
///
/// The trivial-phi rule in [`try_simplify`] sees a phi whose inputs are itself
/// and one other node. A local that no path reassigns, threaded through the
/// merges of a nested loop, is not that shape: each merge's phi takes the
/// PREVIOUS merge's phi (which takes this one back along the loop edge), so
/// every phi in the cycle has two distinct non-self inputs and none collapses,
/// though together they only ever carry one value. Each such local costs a
/// frame-to-frame copy on every edge of every iteration.
///
/// Tarjan over the phi-to-phi operand edges; a component whose operands, outside
/// itself, are exactly one node `v` (Braun et al., redundant-phi elimination)
/// is `v` everywhere. Memory and control phis are left alone, an undefined
/// operand or a type disagreement refuses the component.
pub(in crate::ir_optimize) fn collapse_phi_cycles(graph: &mut Graph) -> bool {
    let n = graph.nodes.len();
    let is_phi = |g: &Graph, i: usize| {
        g.nodes[i].op == Op::Phi
            && !matches!(
                g.nodes[i].ty,
                IrType::Memory | IrType::Control | IrType::Void
            )
    };
    let succ = |g: &Graph, i: usize| -> Vec<usize> {
        g.nodes[i]
            .inputs
            .iter()
            .skip(1)
            .filter(|&&v| v != NO_NODE && (v as usize) < n && is_phi(g, v as usize))
            .map(|&v| v as usize)
            .collect()
    };
    let mut index = vec![usize::MAX; n];
    let mut low = vec![0usize; n];
    let mut on_stack = vec![false; n];
    let mut comp = vec![usize::MAX; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut comps: Vec<Vec<usize>> = Vec::new();
    let mut counter = 0usize;
    for root in 0..n {
        if !is_phi(graph, root) || index[root] != usize::MAX {
            continue;
        }
        let mut work: Vec<(usize, Vec<usize>, usize)> = Vec::new();
        index[root] = counter;
        low[root] = counter;
        counter += 1;
        stack.push(root);
        on_stack[root] = true;
        work.push((root, succ(graph, root), 0));
        while let Some((v, nexts, pos)) = work.last_mut() {
            let v = *v;
            if *pos < nexts.len() {
                let w = nexts[*pos];
                *pos += 1;
                if index[w] == usize::MAX {
                    index[w] = counter;
                    low[w] = counter;
                    counter += 1;
                    stack.push(w);
                    on_stack[w] = true;
                    let s = succ(graph, w);
                    work.push((w, s, 0));
                } else if on_stack[w] {
                    low[v] = low[v].min(index[w]);
                }
            } else {
                work.pop();
                if let Some((parent, _, _)) = work.last() {
                    let parent = *parent;
                    low[parent] = low[parent].min(low[v]);
                }
                if low[v] == index[v] {
                    let mut members = Vec::new();
                    while let Some(w) = stack.pop() {
                        on_stack[w] = false;
                        comp[w] = comps.len();
                        members.push(w);
                        if w == v {
                            break;
                        }
                    }
                    comps.push(members);
                }
            }
        }
    }
    let mut changed = false;
    for (cid, members) in comps.iter().enumerate() {
        let mut outside = NO_NODE;
        let mut ok = true;
        'scan: for &p in members {
            if graph.nodes[p].op != Op::Phi {
                ok = false;
                break;
            }
            for &v in graph.nodes[p].inputs.iter().skip(1) {
                if v == NO_NODE {
                    ok = false;
                    break 'scan;
                }
                let vu = v as usize;
                if vu < n && comp[vu] == cid && graph.nodes[vu].op == Op::Phi {
                    continue;
                }
                if matches!(graph.nodes[vu].op, Op::Dead) {
                    continue;
                }
                if outside == NO_NODE {
                    outside = v;
                } else if outside != v {
                    ok = false;
                    break 'scan;
                }
            }
        }
        if !ok || outside == NO_NODE {
            continue;
        }
        let ty = graph.nodes[outside as usize].ty;
        if members.iter().any(|&p| graph.nodes[p].ty != ty) {
            continue;
        }
        for &p in members {
            graph.replace_all_uses(p as NodeId, outside);
            graph.kill(p as NodeId);
        }
        changed = true;
    }
    changed
}

/// The scalar clean-up fixpoint: fold, simplify, GVN, then the three
/// eliminators, until a round changes nothing.
///
/// Extracted so the pre-unroll and post-unroll copies cannot drift — they were
/// written out twice and the convergence rule had to be fixed in both.
///
/// # The convergence test, and what it used to miss
///
/// The loop tested `graph.live_count() == before` alone. That is the right
/// signal for a pass that KILLS nodes — `gvn`, `eliminate_redundant_loads`,
/// `eliminate_dead_stores`, `eliminate_dead_nodes` and the killing half of
/// `algebraic_simplify` all lower the count when they fire — and the wrong one
/// for `fold_constants`, which rewrites a node's `op` to `Op::Const` in place
/// and kills nothing. A round whose only effect was folding therefore read as
/// "converged" and broke the loop, even though the new constants had not yet
/// been offered to `algebraic_simplify` (`x * 1`), to `gvn` (two sites folding
/// to the same value), or to the load and store eliminators.
///
/// In practice the miss was usually masked — folding normally orphans its
/// operands and the following DCE reclaims them, moving the count after all —
/// but "usually masked" is not a convergence criterion. The rule is explicit:
/// **a pass that can change the graph without changing `live_count` must report
/// it by returning `bool`**, and that answer is OR-ed into `changed` here.
/// `fold_constants` is one such pass; `eliminate_redundant_loads` is the other,
/// and its `bool` was being DISCARDED at this call site, so a round whose only
/// effect was re-pointing a load at an earlier one also read as converged.
/// Both are idempotent, so an honest `true` cannot spin the loop.
///
/// JIT round 21 lane stress (FUZZ21-2, `CRATONVM_DBG_IR_VERIFY_EACH_PASS`):
/// almost every pass ends with this cleanup, so it is where the per-pass
/// verifier runs ([`verify_after_pass`]), naming the call site -- the pass
/// that just ran -- through `#[track_caller]`.
#[track_caller]
pub(in crate::ir_optimize) fn scalar_cleanup(graph: &mut Graph) {
    let site = std::panic::Location::caller();
    // j27 (lane FRAMES): the frame copies this cleanup orphans (it folded or
    // merged every node naming one) are collected before the verifier looks;
    // see `orphan_frames`. A copy unnamed before the cleanup is not touched.
    let named_before = orphan_frames::drop_orphaned_frame_copies_enabled()
        .then(|| orphan_frames::snapshot_named_mask(graph));
    scalar_cleanup_rounds(graph);
    if let Some(named_before) = named_before {
        orphan_frames::drop_orphaned_frame_copies(graph, &named_before, None);
    }
    verify_after_pass(graph, site);
}

/// JIT round 21 lane stress (FUZZ21-2): under
/// `CRATONVM_DBG_IR_VERIFY_EACH_PASS`, the post-optimize verifier lanes
/// (structure, types, frame states -- no consultable snapshot slot names a
/// `Dead` node, every deopt site has a frame -- and the memory chain) on the
/// graph a pass and its cleanup just left, one `[ir-verify-each-pass]` line per
/// finding naming the pass's call site. Report only: the compile goes on, and
/// the pipeline's own post-optimize hook still decides whether it is refused.
/// The first site that reports is the pass that broke the graph; every later
/// one may only be repeating it. Off, one flag read per cleanup.
pub(in crate::ir_optimize) fn verify_after_pass(
    graph: &Graph,
    site: &'static std::panic::Location<'static>,
) {
    if !crate::ir_verify::verify_each_pass_enabled() {
        return;
    }
    if let Err(bailout) = crate::ir_verify::verify_graph_between_passes(graph) {
        eprintln!(
            "[ir-verify-each-pass] {} after the pass at {}:{}: {bailout}",
            graph.scope_method_key,
            site.file(),
            site.line()
        );
    }
}

/// The fixed point [`scalar_cleanup`] runs.
pub(in crate::ir_optimize) fn scalar_cleanup_rounds(graph: &mut Graph) {
    for _ in 0..8 {
        let before = graph.live_count();
        let mut changed = fold_constants(graph);
        changed |= algebraic_simplify(graph);
        changed |= collapse_phi_cycles(graph);
        changed |= decide_branch_implied_guards(graph);
        changed |= fuse_lcmp_compares(graph);
        changed |= eliminate_trivial_guards(graph);
        gvn(graph);
        changed |= eliminate_redundant_loads(graph);
        eliminate_dead_stores(graph);
        changed |= eliminate_initial_zero_stores(graph);
        changed |= eliminate_overwritten_static_stores(graph);
        eliminate_dead_nodes(graph);
        if !changed && graph.live_count() == before {
            break;
        }
    }
}

/// SCEV-driven loop-invariant code motion, plus the cleanup that follows it.
///
/// Default-**ON**; `CRATONVM_JIT_LICM=0` turns it off (`licm_enabled` is
/// `map_or(true, ..)`). It is why the single-pass `aaload`/FP hoists are not on
/// the veto list in `x64/single_pass_only.rs`. Runs after the fixed-point
/// cleanup so it sees already-folded / GVN'd invariant expressions, then a
/// lightweight cleanup re-runs GVN + DCE to dedup any anchor edges it rewrote.
///
/// Extracted from [`optimize`] so the pass can be called from *either* side of
/// the unroller without its body being written twice — see
/// [`licm_before_unroll_enabled`].
///
/// Returns whether the pass made PROGRESS — i.e. whether a later round could
/// see something new. A compact-form recognition still gets the trailing
/// cleanup (that is what dedups duplicate compact reads) but is not progress,
/// because the same recognition happens on every visit. See [`LicmOutcome`].
pub(in crate::ir_optimize) fn run_licm_pass(graph: &mut Graph) -> bool {
    if !licm_enabled() {
        return false;
    }
    let outcome = licm_outcome(graph);
    if outcome.any_work() {
        crate::ir_evidence::note(crate::ir_evidence::Transform::Licm);
        gvn(graph);
        // The hoist lands every copy of an invariant read on ONE control anchor
        // and ONE memory state, which is the zero-length case of the chain walk
        // in `eliminate_redundant_loads`. Without this call the pre-header holds
        // four complete read sequences where one would do — the shape §7 of
        // `c2-the-loop-body-is-mostly-code-it-never-runs-20260911.md` filed.
        // That pass is off by default, so the zero-length case alone runs
        // here unless it is on (round 12 wave 7).
        eliminate_hoisted_duplicate_loads(graph);
        eliminate_dead_nodes(graph);
    }
    outcome.moved
}

/// Is `base` non-null on entry to the method, so that a load of it hoisted
/// above the loop test cannot invent a `NullPointerException`?
///
/// Two sources, both already believed elsewhere in this crate: the receiver
/// parameter (`Graph::receiver_param` is a parameter INDEX, so the node is
/// whichever `Op::Param` carries it — reading it as a node id would seed an
/// arbitrary node non-null), and the ops `ir_check_elim::definitely_non_null`
/// answers for. Anything else is `false`; this is a permission, not an
/// analysis.
///
/// Until 2026-09-17 the body implemented only the first of the two sources the
/// doc names. Nothing was unsound — the one caller happened to `||` in
/// `definitely_non_null` itself — but a predicate whose doc and body disagree
/// is a trap for the next caller, and this function grew a second one (the
/// restricted read hoist) in the same commit that closed the gap.
pub(in crate::ir_optimize) fn base_non_null_on_entry(graph: &Graph, base: NodeId) -> bool {
    if base == NO_NODE || base as usize >= graph.nodes.len() {
        return false;
    }
    let op = &graph.nodes[base as usize].op;
    if crate::ir_check_elim::definitely_non_null(op) {
        return true;
    }
    // `CRATONVM_JIT_IR_STATIC_FINAL_EXACT_CLASS`: an OSR header's φ of a
    // write-once local, whose walked input is the `new` and whose arm input
    // the arm's exact-class guard tests.
    if *op == Op::Phi
        && crate::ir::ir_static_final_exact_class_enabled()
        && phi_non_null_on_every_edge(graph, base, &mut FxHashSet::default(), 0)
    {
        return true;
    }
    // And a trusted `static final` the VM filed non-null.
    if let Op::LoadStatic {
        class_id,
        field_index,
        type_tag: b'L',
        is_volatile: false,
    } = *op
    {
        if graph
            .static_final_non_null
            .contains(&(class_id, field_index))
        {
            return true;
        }
    }
    if let Op::IndyRootRef { slot_addr } = *op {
        if graph
            .const_ref_meta
            .get(&slot_addr)
            .is_some_and(|m| m.non_null)
        {
            return true;
        }
    }
    match graph.receiver_param {
        Some(recv) => matches!(op, Op::Param(p) if *p == recv),
        None => false,
    }
}

/// Is `v` non-null on every path that reaches it? A Phi qualifies when every
/// incoming value is itself, definitely non-null, another proven Phi, or
/// guarded by `ExactClassIs` on a control node that dominates that incoming
/// edge. A null value fails `ExactClassIs`, so its path deopts before use.
pub(crate) fn phi_non_null_on_every_edge(
    graph: &Graph,
    v: NodeId,
    seen: &mut FxHashSet<NodeId>,
    depth: usize,
) -> bool {
    let Some(node) = graph.node_opt(v) else {
        return false;
    };
    // The guard scan below is linear in the graph: bounded for the very large
    // ones, which keep the answer they had.
    if node.op != Op::Phi || depth > 4 || graph.nodes.len() > 20_000 || !seen.insert(v) {
        return false;
    }
    let Some(region) = node.input_opt(0).and_then(|r| graph.node_opt(r)) else {
        return false;
    };
    let preds: Vec<NodeId> = region.inputs.iter().copied().collect();
    let data: Vec<NodeId> = node.inputs.iter().skip(1).copied().collect();
    if preds.len() != data.len() || data.is_empty() || data.iter().all(|&input| input == v) {
        return false;
    }
    data.iter().zip(&preds).all(|(&input, &pred)| {
        if input == v {
            return true;
        }
        let Some(n) = graph.node_opt(input) else {
            return false;
        };
        if crate::ir_check_elim::definitely_non_null(&n.op)
            || phi_non_null_on_every_edge(graph, input, seen, depth + 1)
        {
            return true;
        }
        graph.nodes.iter().any(|g| {
            matches!(g.op, Op::Guard { .. })
                && g.input_opt(1)
                    .and_then(|c| graph.node_opt(c))
                    .is_some_and(|c| {
                        matches!(c.op, Op::ExactClassIs { class_id } if class_id != 0)
                            && c.input_opt(0) == Some(input)
                    })
                && g.input_opt(0)
                    .is_some_and(|gc| control_dominates(graph, gc, pred))
        })
    })
}

/// Does this loop's body execute at least once, every time the loop is
/// reached?
///
/// The pre-header hoist is speculative for exactly one reason: the pre-header
/// runs when the body does not. When the body always runs, that reason is gone
/// and the hoist may take a base nothing else vouches for — every fault the
/// hoisted read can raise, the first iteration was going to raise anyway.
///
/// One question answers it: does [`analyze_counted_loop`] prove this
/// single-back-edge loop takes at least one trip? Two of its four answers say
/// yes — a constant trip count of at least one, and `TripOverCap`, which the
/// unroller returns only after simulating nine iterations that all continued.
/// The second is the one that matters: the full unroller is default-ON and
/// takes constant-trip loops of 8 or fewer apart before LICM sees them, so the
/// population this permission actually serves is the constant-trip loop too
/// big to fully unroll — real, but not the shape the tiering inversion was
/// measured on
/// (`probes/FieldLoop.java` counts to a `-D` property, so its bound is not
/// constant and this answers `false` for it). A loop bounded by a parameter
/// cannot be proven non-empty here at all; doing that needs the loop's entry
/// test hoisted with the load, which is control-flow surgery and a different
/// change.
///
/// **Default ON** since round 11 wave 5; `CRATONVM_JIT_IR_LICM_HOIST_COUNTED=0`
/// is the kill switch, and `CRATONVM_JIT_LICM=0` still turns all of LICM off.
/// It was default OFF because it widens a permission, and a widening that
/// fires by default has to earn it on evidence. The evidence (orchestrator,
/// wave-4 binary, `=1` against `=0`): identical output on
/// `R11MidCountedHoistSkipsStore` (`bad 0`), `R11MidHoistNpeReentry`,
/// `R11IroptHoistNpeFrame` (mismatch 0), `R11IroptNestedLicm` and
/// `R11MidEaVictimChain`; the full R11 probe battery in the default and
/// C2-always arms and the 12 legacy probes all match HotSpot with it on;
/// single-run timings `R11IroptNestedLicm` afterCall 47 -> 40 ms and inner
/// 15 -> 8 ms, CratonBenchC2 1152 -> 951 ms. The soundness argument is the
/// wave-3/4 audit on `r11-iropt-licm-preheader-deopt-reads-unwritten-phi-slots`
/// (the hoisted read gets the first-iteration frame, and anything the first
/// iteration runs before it refuses the hoist; `install_preheader_deopt_frame`).
pub(in crate::ir_optimize) fn loop_body_definitely_runs(
    graph: &Graph,
    region: NodeId,
    back_ctrls: &[NodeId],
) -> bool {
    if !licm_hoist_counted_enabled() {
        return false;
    }
    // More than one back edge and `analyze_counted_loop`'s single-`If` model
    // does not describe this loop; it would be answering about a different
    // shape than the one being hoisted out of.
    let [back] = back_ctrls else {
        return false;
    };
    match analyze_counted_loop(graph, region, *back) {
        Ok(c) => c.trip >= 1,
        // `TripOverCap` is the interesting half, and missing it made this
        // predicate answer `false` for every loop it was written for. The
        // refusal is the UNROLLER's — `trip > UNROLL_MAX_TRIP`, which is 8 —
        // and it is only reached after the simulation has already stepped the
        // induction variable past the exit test nine times. "Too big to unroll"
        // is therefore a proof that the body runs, and a stronger one than the
        // `Ok` arm's.
        Err(NotCounted::TripOverCap(_)) => true,
        // `RuntimeBound` is a counted loop whose bound is a runtime value, and
        // `Shape` is a loop this analysis does not model. Neither says anything
        // about whether the body runs.
        Err(NotCounted::RuntimeBound(_)) | Err(NotCounted::Shape) => false,
    }
}

/// `CRATONVM_JIT_IR_LICM_HOIST_COUNTED` — default ON (round 11 wave 5);
/// `0|false|off|no` turns it off. Read live, like `licm_mem_edge_enabled`, so
/// an in-process A/B can exercise both arms.
pub(in crate::ir_optimize) fn licm_hoist_counted_enabled() -> bool {
    !matches!(
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
        Ok("0") | Ok("false") | Ok("off") | Ok("no")
    )
}

/// A hoisted invariant load has its MEMORY edge moved to the loop-entry memory
/// state as well as its control edge — which is what actually gets it scheduled
/// outside the loop. **Default ON** since 2026-09-11;
/// `CRATONVM_JIT_IR_LICM_MEM_EDGE=0` is the kill switch.
///
/// Without it the hoist is cosmetic: `ir_schedule::find_best_block` puts a data
/// node in the deepest block dominated by all of its input blocks, so a load
/// still naming the header's memory phi is scheduled back into the loop LICM
/// just moved it out of. See the hoist site in [`licm`], and
/// `docs/feature-designs/jit-ir-optimizations.md`.
///
/// Flipped ON against the bar this tree uses for a default: on
/// `probes/FieldLoop.java` `sum` the optimizing tier goes from 1.12x SLOWER
/// than the single-pass tier to **0.68x**, measured interleaved with a
/// same-config control; CratonBench's seven checksums are byte-identical in
/// every arm; and the whole `cratonvm-jit` suite — 2,373 unit tests and the 145
/// `ir_vs_singlepass` differential cases — passes gate-ON exactly as gate-OFF.
///
/// Read LIVE on every call rather than cached in a `OnceLock`, for the reason
/// `ir_per_copy_frames_enabled` spells out: a cached read makes one arm of an
/// in-process A/B untestable, because whichever arm runs first fixes the answer
/// for the whole process.
pub(in crate::ir_optimize) fn licm_mem_edge_enabled() -> bool {
    !matches!(
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
        Ok("0") | Ok("false") | Ok("off") | Ok("no")
    )
}

/// `CRATONVM_JIT_IR_UNROLL_PROMOTED_LOOPS` (JIT round 21, lane loopopt,
/// TYPECHECK21-6; perf, default ON): after a loop promotion (field, static or
/// fresh-object cells) left a register-only loop and
/// `loop_retire::retire_accumulator_loops` has already retired any constant-step
/// linear accumulator loops (preserving `static-counter-long` retirement),
/// offer the graph to the unroller once more when the loop rounds unrolled
/// nothing (see `optimize_passes`). Read per optimize.
pub(in crate::ir_optimize) fn unroll_promoted_loops_enabled() -> bool {
    true
}

/// `true` when `CRATONVM_JIT_IR_LICM_BEFORE_UNROLL` is set: LICM runs *before*
/// the unroller instead of after it, so the unroller sees invariant loads
/// already re-anchored to the pre-header rather than pinned to the header.
///
/// Default OFF — see the pass-order comment in [`optimize`] for what the
/// order costs and why flipping it is a measurement rather than a cleanup.
///
/// Read LIVE on every call rather than cached in a `OnceLock`, for the reason
/// `ir_per_copy_frames_enabled` spells out: a cached read makes the ON arm of
/// an in-process A/B untestable, because whichever arm runs first fixes the
/// answer for the whole process and both arms then emit identical code.
pub(in crate::ir_optimize) fn licm_before_unroll_enabled() -> bool {
    matches!(
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
        Ok("1") | Ok("true") | Ok("on") | Ok("yes")
    )
}

/// `true` (default) when the SCEV-driven loop-invariant code-motion pass runs.
/// Flipped default-ON after the increment-2 LICM pass soaked clean (bt10/14/16/18
/// + loop-heavy bench differential gate-ON ≡ gate-OFF == HotSpot, like the
/// `IR_CALL`/`IR_LONG`/`IR_FP` flips). `CRATONVM_JIT_LICM=0` is the opt-out that
/// restores the pre-flip (no-LICM) behaviour as the safety net.
///
/// # The opt-out used to ignore `false` / `off` / `no` (fixed 2026-09-16)
///
/// This gate was `map_or(true, |v| v != "0")`, so only the literal `0` turned
/// it off: `CRATONVM_JIT_LICM=false` left LICM running. It is now
/// [`cratonvm_types::flags::runtime_flag_default_on`], the crate's named
/// default-ON parser, which accepts `0|false|off|no` (trimmed,
/// case-insensitive) like every other opt-out in this backend. Nothing that
/// was off before becomes on — the change only widens the off-set.
///
/// Read once per LICM pass, no `static` (j27 LATCHES): a per-VM or per-thread
/// override reaches it.
pub fn licm_enabled() -> bool {
    true
}

/// `true` when `CRATONVM_JIT_REASSOC` is set (cached). Enables the affine
/// strength-reduction pass and the matching IR-path gate relaxation.
/// Kept latched (j27 LATCHES): the single-pass walk reads it once per `iload`.
pub fn reassoc_enabled() -> bool {
    use std::sync::OnceLock;
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| false)
}

/// `true` (default) when pure, call-free *branchy* integer methods may take the
/// IR pipeline. The φ/branch SIGSEGV (missing SETcc ModRM byte) and the
/// loop-carried-phi miscompile that originally gated these methods are fixed,
/// so the IR path now compiles if/else and loops correctly. `CRATONVM_NO_IR_BRANCHY`
/// is the emergency opt-out that restores the single-pass-only routing for the
/// branchy shape (e.g. to bisect a suspected IR-path regression).
pub fn ir_branchy_enabled() -> bool {
    !false
}

// ── Affine strength reduction (reassociation) ────────────────────────
//
// An integer value is "affine in `root`" when it equals `k*root + c` for
// compile-time constants `k`, `c` (with `root == None` meaning a pure
// constant `c`). The pass computes this form for every integer Add/Sub/Mul/Neg
// node in one forward pass (SSA data inputs precede their users in id order;
// loop-carried φ back-edges read as opaque, which is conservative), then
// rewrites each chain node into the canonical `k*root + c`. Dead chain nodes
// are removed by the following DCE pass; duplicate const/Mul/Add nodes are
// merged by GVN.
//
// Correctness: Java `int`/`long` arithmetic is two's-complement and wraps mod
// 2^n, where +, -, * are associative, commutative, and distributive, so the
// reassociation is exact. Floating point (non-associative) is never touched.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::ir_optimize) struct Affine {
    /// Root value this expression is affine in; `None` => pure constant.
    ///
    /// Deliberately an `Option<NodeId>` and *not* the [`NO_NODE`] sentinel.
    /// `NO_NODE` already carries one meaning — "this graph edge is absent" —
    /// and reusing the same bit pattern here for a second, value-domain
    /// meaning ("this expression has no root; it is a pure constant") made one
    /// `u32::MAX` stand for two incompatible things. A missed test then does
    /// not read as "absent", it reads as node `u32::MAX`, which is exactly the
    /// `nodes[u32::MAX]` panic `ir.rs`'s `NO_NODE` deprecation note describes.
    /// With an `Option` the compiler forces the case split at every read.
    pub(in crate::ir_optimize) root: Option<NodeId>,
    pub(in crate::ir_optimize) k: i64,
    pub(in crate::ir_optimize) c: i64,
}

#[inline]
pub(in crate::ir_optimize) fn w_mul(is_int: bool, a: i64, b: i64) -> i64 {
    if is_int {
        ((a as i32).wrapping_mul(b as i32)) as i64
    } else {
        a.wrapping_mul(b)
    }
}
#[inline]
pub(in crate::ir_optimize) fn w_add(is_int: bool, a: i64, b: i64) -> i64 {
    if is_int {
        ((a as i32).wrapping_add(b as i32)) as i64
    } else {
        a.wrapping_add(b)
    }
}
#[inline]
pub(in crate::ir_optimize) fn w_sub(is_int: bool, a: i64, b: i64) -> i64 {
    if is_int {
        ((a as i32).wrapping_sub(b as i32)) as i64
    } else {
        a.wrapping_sub(b)
    }
}
#[inline]
pub(in crate::ir_optimize) fn w_neg(is_int: bool, a: i64) -> i64 {
    if is_int {
        ((a as i32).wrapping_neg()) as i64
    } else {
        a.wrapping_neg()
    }
}

/// `Some(true)` for `int`, `Some(false)` for `long`, `None` otherwise.
pub(in crate::ir_optimize) fn int_width(ty: IrType) -> Option<bool> {
    match ty {
        IrType::Int => Some(true),
        IrType::Long => Some(false),
        _ => None,
    }
}

pub(in crate::ir_optimize) fn is_int_arith(op: &Op) -> bool {
    matches!(op, Op::Add | Op::Sub | Op::Mul | Op::Neg)
}

/// Combine the affine forms of a binary op's two operands.
pub(in crate::ir_optimize) fn combine_affine(
    op: &Op,
    is_int: bool,
    a: Affine,
    b: Affine,
    opaque: Affine,
) -> Affine {
    match op {
        Op::Add => {
            if a.root.is_none() {
                Affine {
                    root: b.root,
                    k: b.k,
                    c: w_add(is_int, b.c, a.c),
                }
            } else if b.root.is_none() {
                Affine {
                    root: a.root,
                    k: a.k,
                    c: w_add(is_int, a.c, b.c),
                }
            } else if a.root == b.root {
                Affine {
                    root: a.root,
                    k: w_add(is_int, a.k, b.k),
                    c: w_add(is_int, a.c, b.c),
                }
            } else {
                opaque
            }
        }
        Op::Sub => {
            if b.root.is_none() {
                Affine {
                    root: a.root,
                    k: a.k,
                    c: w_sub(is_int, a.c, b.c),
                }
            } else if a.root.is_none() {
                Affine {
                    root: b.root,
                    k: w_neg(is_int, b.k),
                    c: w_sub(is_int, a.c, b.c),
                }
            } else if a.root == b.root {
                Affine {
                    root: a.root,
                    k: w_sub(is_int, a.k, b.k),
                    c: w_sub(is_int, a.c, b.c),
                }
            } else {
                opaque
            }
        }
        Op::Mul => {
            if a.root.is_none() {
                if b.root.is_none() {
                    Affine {
                        root: None,
                        k: 0,
                        c: w_mul(is_int, a.c, b.c),
                    }
                } else {
                    Affine {
                        root: b.root,
                        k: w_mul(is_int, b.k, a.c),
                        c: w_mul(is_int, b.c, a.c),
                    }
                }
            } else if b.root.is_none() {
                Affine {
                    root: a.root,
                    k: w_mul(is_int, a.k, b.c),
                    c: w_mul(is_int, a.c, b.c),
                }
            } else {
                opaque
            }
        }
        _ => opaque,
    }
}

/// `(value, type)` → the lowest-id `Op::Const` node of that value and type.
/// It replaces a `get_or_add_const` that scanned the whole arena on every call,
/// and mirrors `IrBuilder::interned_const` in `ir.rs`.
///
/// # Why a hit can be trusted
///
/// * **New nodes**, including a constant added through `graph` directly, are
///   indexed on the next call. Everything from `upto` to the end of the arena
///   is scanned once, and `or_insert` keeps the LOWEST id per key, which is
///   what the scan's first match returned.
/// * **A killed or rewritten node** fails the re-check on a hit. The full scan
///   [`const_node_by_scan`] then decides and repairs the entry.
/// * **A shorter arena** than has been indexed is rebuilt from scratch rather
///   than trusted.
///
/// What it cannot see is an OLDER node turned INTO a constant after it was
/// indexed. So one is made per pass invocation and never kept across passes:
/// constant folding rewrites ops in place, but the two passes that use this
/// only add nodes, redirect uses and kill.
pub(in crate::ir_optimize) struct ConstIntern {
    pub(in crate::ir_optimize) map: FxHashMap<(i64, IrType), NodeId>,
    /// How far into `graph.nodes` `map` has indexed.
    pub(in crate::ir_optimize) upto: usize,
}

impl ConstIntern {
    pub(in crate::ir_optimize) fn new() -> Self {
        ConstIntern {
            map: FxHashMap::default(),
            upto: 0,
        }
    }

    /// The lowest-id live `Op::Const(val)` typed `ty`, or `None`. Always the
    /// same answer as [`const_node_by_scan`].
    pub(in crate::ir_optimize) fn find(
        &mut self,
        graph: &Graph,
        val: i64,
        ty: IrType,
    ) -> Option<NodeId> {
        if graph.nodes.len() < self.upto {
            self.map.clear();
            self.upto = 0;
        }
        for id in self.upto..graph.nodes.len() {
            let node = &graph.nodes[id];
            if let Op::Const(v) = node.op {
                self.map.entry((v, node.ty)).or_insert(id as NodeId);
            }
        }
        self.upto = graph.nodes.len();

        let id = *self.map.get(&(val, ty))?;
        if matches!(graph.nodes.get(id as usize), Some(n) if n.op == Op::Const(val) && n.ty == ty) {
            return Some(id);
        }
        match const_node_by_scan(graph, val, ty) {
            Some(found) => {
                self.map.insert((val, ty), found);
                Some(found)
            }
            None => {
                self.map.remove(&(val, ty));
                None
            }
        }
    }

    /// Get an existing `Const(val)` node of type `ty`, or create one.
    pub(in crate::ir_optimize) fn get_or_add(
        &mut self,
        graph: &mut Graph,
        val: i64,
        ty: IrType,
    ) -> NodeId {
        // t1 wave 2 (lane Y): look up the sign-extended value `Graph::add`
        // would store, or an unsigned `int` spelling never finds its node.
        // Cast: an `int` constant is its low 32 bits.
        let val = if ty == IrType::Int {
            i64::from(val as i32)
        } else {
            val
        };
        if let Some(id) = self.find(graph, val, ty) {
            return id;
        }
        graph.add(Op::Const(val), ty, vec![], None)
    }
}

/// The lowest-id `Op::Const(val)` node typed `ty`, by scanning the arena. This
/// is what [`ConstIntern`] answers without the scan, and its fallback when a
/// map entry has gone stale.
pub(in crate::ir_optimize) fn const_node_by_scan(
    graph: &Graph,
    val: i64,
    ty: IrType,
) -> Option<NodeId> {
    graph
        .nodes
        .iter()
        .position(|n| n.op == Op::Const(val) && n.ty == ty)
        .map(|i| i as NodeId)
}

/// Materialize `k*root + c` as IR nodes, reusing `root` directly when possible.
///
/// `root` is `None` for a pure constant (`k == 0`, value `c`). A non-zero `k`
/// with no root is not a representable value, so that combination yields
/// `None` and the caller leaves the node alone — rather than fabricating an
/// edge out of a sentinel, which is how a `Mul` with a `u32::MAX` input used to
/// become reachable.
pub(in crate::ir_optimize) fn build_affine(
    graph: &mut Graph,
    consts: &mut ConstIntern,
    root: Option<NodeId>,
    k: i64,
    c: i64,
    ty: IrType,
) -> Option<NodeId> {
    if k == 0 {
        return Some(consts.get_or_add(graph, c, ty));
    }
    let root = root?;
    let base = if k == 1 {
        root
    } else {
        let kc = consts.get_or_add(graph, k, ty);
        graph.add(Op::Mul, ty, vec![root, kc], None)
    };
    Some(if c == 0 {
        base
    } else {
        let cc = consts.get_or_add(graph, c, ty);
        graph.add(Op::Add, ty, vec![base, cc], None)
    })
}

/// Returns how many rewritten nodes collapsed a chain deeper than the
/// two-node `k*root + c` they became.
pub(in crate::ir_optimize) fn reassociate_affine(graph: &mut Graph) -> usize {
    let len = graph.nodes.len();
    let mut aff: Vec<Option<Affine>> = vec![None; len];
    // `true` for nodes we will rewrite: an integer arith node that is affine in
    // a single real root AND has an operand that is itself an affine arith node
    // over the same root (i.e. a genuine chain to collapse). Decided in the
    // forward pass from *original* inputs so later input-rewrites cannot
    // perturb the decision.
    let mut materialize: Vec<bool> = vec![false; len];
    // Round 17: the longest chain of integer arith nodes over the same root
    // ending at each node. `k*root + c` is at most 2 deep, so only a node
    // deeper than that is a collapse worth reporting as evidence.
    let mut depth: Vec<u32> = vec![0; len];

    for id in 0..len {
        let node = &graph.nodes[id];
        if node.op == Op::Dead {
            continue;
        }
        let opaque = Affine {
            root: Some(id as NodeId),
            k: 1,
            c: 0,
        };
        let is_int = match int_width(node.ty) {
            Some(w) => w,
            None => {
                aff[id] = Some(opaque);
                continue;
            }
        };
        // Read an operand's already-computed affine form; only inputs with a
        // strictly-lower id are available (φ back-edges read as None → opaque).
        let geta = |slot: usize| -> Option<Affine> {
            node.inputs.get(slot).and_then(|&i| {
                if (i as usize) < id {
                    aff[i as usize]
                } else {
                    None
                }
            })
        };
        let res = match &node.op {
            Op::Const(v) => Affine {
                root: None,
                k: 0,
                c: if is_int { (*v as i32) as i64 } else { *v },
            },
            Op::Neg => match geta(0) {
                Some(a) => Affine {
                    root: a.root,
                    k: w_neg(is_int, a.k),
                    c: w_neg(is_int, a.c),
                },
                None => opaque,
            },
            Op::Add | Op::Sub | Op::Mul => match (geta(0), geta(1)) {
                (Some(a), Some(b)) => combine_affine(&node.op, is_int, a, b, opaque),
                _ => opaque,
            },
            _ => opaque,
        };
        // Decide materialization while original inputs are intact. A form with
        // no root (`None` — a pure constant) has nothing to strength-reduce,
        // and a form rooted at the node itself (`Some(id)` — opaque) has
        // nothing to collapse; `filter` removes both without either case
        // hiding behind a sentinel comparison.
        if let Some(root) = res.root.filter(|&r| r != id as NodeId) {
            if is_int_arith(&node.op) {
                let has_arith_operand = node.inputs.iter().any(|&j| {
                    (j as usize) < id
                        && aff[j as usize].is_some_and(|fj| fj.root == Some(root))
                        && is_int_arith(&graph.nodes[j as usize].op)
                });
                materialize[id] = has_arith_operand;
                depth[id] = 1 + node
                    .inputs
                    .iter()
                    .filter(|&&j| {
                        (j as usize) < id && aff[j as usize].is_some_and(|fj| fj.root == Some(root))
                    })
                    .map(|&j| depth[j as usize])
                    .max()
                    .unwrap_or(0);
            }
        }
        aff[id] = Some(res);
    }

    let mut consts = ConstIntern::new();
    let mut rewritten = 0usize;
    for id in 0..len {
        if !materialize[id] {
            continue;
        }
        let ty = graph.nodes[id].ty;
        let a = match aff[id] {
            Some(a) => a,
            None => continue,
        };
        // `None` here means the form was not materializable (a non-zero `k`
        // with no root). `materialize[id]` is only set for a form with a real
        // root, so this is unreachable today; leaving the node untouched is
        // the safe answer if that ever stops holding.
        let mat = match build_affine(graph, &mut consts, a.root, a.k, a.c, ty) {
            Some(m) => m,
            None => continue,
        };
        if mat != id as NodeId {
            graph.replace_all_uses(id as NodeId, mat);
            graph.kill(id as NodeId);
            if depth[id] > 2 {
                rewritten += 1;
            }
        }
    }
    rewritten
}

// ── Multiply-by-constant strength reduction (round 17) ───────────────
//
// `r16-codegen-loop-and-straight-line-anatomy-FIXED-20261005.md`, `bigA`: once its
// three chains sat in registers, the body was bound by the `a` chain alone,
// `a = a * 31 + k` 48 times -- `IMUL` (3 cycles) then `ADD` (1) per step, ~4
// cycles a link, against HotSpot's ~2. C2 rewrites a multiply by `2^n - 1`
// into a shift and a subtract (`MulINode::Ideal`), and the following `+ k`
// then folds into the subtrahend: `(x << n) - (x - k)`, where the shift and
// `x - k` (one `LEA`) run in parallel and the `SUB` joins them.
//
// Exact under two's-complement wrap: `x * (2^n - 1) == (x << n) - x` and
// `(p - x) + k == p - (x - k)` hold mod `2^32` / `2^64` for every `x`, and the
// shift count `n` (`2..=30` for `int`, `2..=62` for `long`) is below the width,
// so Java's shift masking does not touch it.

/// `CRATONVM_JIT_IR_MUL_STRENGTH_REDUCE` (round 17): run
/// [`strength_reduce_mul`]. Read per compile.
pub(in crate::ir_optimize) fn mul_strength_reduce_enabled() -> bool {
    false
}

/// `Some(n)` when `c` (at `ty`'s width) is `2^n - 1` with `n` in the shift
/// range that keeps the identity exact.
pub(in crate::ir_optimize) fn mul_shift_form(c: i64, ty: IrType) -> Option<i64> {
    let (c, max_n) = match ty {
        // Cast: an `int` constant is its low 32 bits.
        IrType::Int => (i64::from(c as i32), 30),
        IrType::Long => (c, 62),
        _ => return None,
    };
    (2..=max_n).find(|&n| c == (1i64 << n) - 1)
}

/// Rewrite `Add(Mul(x, 2^n - 1), k)` as `Sub(Shl(x, n), Add(x, -k))`.
/// Returns how many were rewritten.
///
/// Only that shape, and only when the multiply has no other reader. A
/// multiply a snapshot names is taken only in a graph no node of which can
/// deopt, under the unroller's unreachable-frames net
/// (`UNROLL_USED_UNREACHABLE_FRAMES`, which makes `lower_inner` refuse the
/// compile if emission builds a deopt point after all): the snapshot slot is
/// cleared (`NO_NODE`), as nothing can read it. `bigA` names every `a * 31`
/// on the operand stack of some bci and has no deopt point. Measured on the
/// round-17 binary: the bare
/// `x * (2^n +- 1)` rewrite adds a `MOV` and a second ALU op where `IMUL`
/// was one instruction, which costs a throughput-bound loop (`intLoop`'s
/// `i * 3`, `nocall`'s `i * 7`) more than the cycle of latency it saves; and
/// a multiply a frame state names stays alive beside its rewrite, so both
/// forms are computed (`bigA` 53 -> 68 ns). The multiply-add on a dependence
/// chain is the case that pays: four cycles of latency become two.
pub(in crate::ir_optimize) fn strength_reduce_mul(graph: &mut Graph) -> usize {
    let frames_unreachable = unroll_over_unreachable_frames()
        && crate::ir_lower::ir_drop_unreachable_homes_enabled()
        && crate::ir_lower::graph_cannot_deopt(graph);
    let mut relied_on_unreachable = false;
    let named: FxHashSet<NodeId> = graph
        .safepoints
        .iter()
        .flat_map(|sp| {
            sp.locals
                .iter()
                .chain(sp.stack.iter())
                .chain(sp.monitors.iter())
        })
        .copied()
        .filter(|&v| v != NO_NODE)
        .collect();
    let users = build_users(graph);
    let mut consts = ConstIntern::new();
    let mut rewritten = 0usize;
    let len = graph.nodes.len();
    for id in 0..len {
        let (ty, inputs, pc) = {
            let n = &graph.nodes[id];
            if n.op != Op::Add || !matches!(n.ty, IrType::Int | IrType::Long) {
                continue;
            }
            (n.ty, n.inputs.clone(), n.bytecode_pc)
        };
        let &[a, b] = inputs.as_slice() else {
            continue;
        };
        let (mul, k) = match (const_value(&graph.nodes, a), const_value(&graph.nodes, b)) {
            (None, Some(k)) => (a, k),
            (Some(k), None) => (b, k),
            _ => continue,
        };
        let Some(mn) = graph.node_opt(mul) else {
            continue;
        };
        if mn.op != Op::Mul
            || mn.ty != ty
            || users.get(mul as usize).map_or(0, Vec::len) != 1
            || (named.contains(&mul) && !frames_unreachable)
        {
            continue;
        }
        let &[ma, mb] = mn.inputs.as_slice() else {
            continue;
        };
        let (x, c) = match (const_value(&graph.nodes, ma), const_value(&graph.nodes, mb)) {
            (None, Some(c)) => (ma, c),
            (Some(c), None) => (mb, c),
            _ => continue,
        };
        let Some(n) = mul_shift_form(c, ty) else {
            continue;
        };
        let neg_k = match ty {
            // Cast: an `int` constant is its low 32 bits.
            IrType::Int => i64::from((k as i32).wrapping_neg()),
            _ => k.wrapping_neg(),
        };
        // The shift count is an `int` at both widths (`ishl` / `lshl`).
        let nc = consts.get_or_add(graph, n, IrType::Int);
        let kc = consts.get_or_add(graph, neg_k, ty);
        let shl = graph.add(Op::Shl, ty, vec![x, nc], pc);
        let inner = graph.add(Op::Add, ty, vec![x, kc], pc);
        let new = graph.add(Op::Sub, ty, vec![shl, inner], pc);
        graph.replace_all_uses(id as NodeId, new);
        graph.kill(id as NodeId);
        if named.contains(&mul) {
            // Its only readers left are snapshot slots no deopt can consult.
            graph.replace_all_uses(mul, NO_NODE);
            relied_on_unreachable = true;
        }
        graph.kill(mul);
        rewritten += 1;
    }
    if relied_on_unreachable {
        UNROLL_USED_UNREACHABLE_FRAMES.with(|c| c.set(true));
    }
    rewritten
}

// ── Constant Folding ─────────────────────────────────────────────────

/// Evaluate nodes whose inputs are all constants.
/// Returns whether it rewrote anything.
///
/// The return value is load-bearing, not decoration: this pass rewrites a node
/// IN PLACE (`op` becomes `Op::Const`) and kills nothing, so it does not move
/// `Graph::live_count`. [`optimize`]'s fixpoint used to test exactly that count
/// and therefore read a fold-only round as "converged" — see the note on the
/// loop. Idempotent by construction, so reporting `true` cannot spin: once a
/// node is `Op::Const`, `try_fold` no longer matches it.
pub(in crate::ir_optimize) fn fold_constants(graph: &mut Graph) -> bool {
    let len = graph.nodes.len();
    let mut changed = false;
    for id in 0..len {
        if graph.nodes[id].op == Op::Dead {
            continue;
        }
        if let Some(folded) = try_fold(&graph.nodes, id as NodeId) {
            // Rewritten in place: same id, type and bytecode pc. The edges go
            // through `clear_inputs`, not `inputs.clear()`, so the maintained
            // use lists stay current; the raw clear bumped the edge epoch and
            // sent the next `replace_all_uses` / `users_of` down the
            // full-rebuild path once per fold round.
            graph.nodes[id].op = Op::Const(folded);
            graph.clear_inputs(id as NodeId);
            changed = true;
        } else if let Some(bits) = try_fold_scalar_fp(&graph.nodes, id as NodeId) {
            // JIT round 19 wave 2 (lane irops2, IROPS19-3), opt-in: the same
            // in-place rewrite, to an FP constant.
            graph.nodes[id].op = Op::ConstF(bits);
            graph.clear_inputs(id as NodeId);
            changed = true;
        }
    }
    changed
}

/// OSR2-2: `ExactClassIs { id }(New { c })` is `c == id`, and
/// `ClassWordOf(New { c })` is `c`'s 32-bit class word (`c as i32`, the word
/// `Cmp(Eq, ClassWordOf(r), Const(id as i32))` compares; the ordinary `Cmp`
/// fold then answers the chain's test). A `new C` is a non-null plain object
/// whose header names exactly `C` for its whole life, which is all either op
/// reads. Neither was folded before: a splice on a just-allocated receiver
/// kept a dead test, the class word kept the allocation's header read alive,
/// and escape analysis saw the test as an `EaOp::Other` use of the
/// allocation. `None` for anything else, an id-0 allocation, or with the
/// switch off.
pub(in crate::ir_optimize) fn try_fold_fresh_class_test(
    nodes: &[Node],
    node: &Node,
) -> Option<i64> {
    if !matches!(node.op, Op::ExactClassIs { .. } | Op::ClassWordOf) {
        return None;
    }
    let input = nodes.get(*node.inputs.first()? as usize)?;
    let Op::New {
        class_id: allocated,
        ..
    } = input.op
    else {
        return None;
    };
    if allocated == 0 {
        return None;
    }
    match node.op {
        Op::ExactClassIs { class_id } => Some(i64::from(class_id == allocated)),
        // Cast: the id's bits, read as the `int` the class word is.
        Op::ClassWordOf => Some(i64::from(allocated as i32)),
        _ => None,
    }
}

pub(in crate::ir_optimize) fn try_fold(nodes: &[Node], id: NodeId) -> Option<i64> {
    let node = &nodes[id as usize];
    // Integer folding only, and the guard is the same one `try_simplify`
    // carries a paragraph about.
    //
    // `Op::Add`/`Sub`/`Mul`/`Div`/`Rem`/`Neg` are SHARED between integer and
    // floating-point arithmetic — `ir.rs` builds an `Op::Div` typed
    // `IrType::Float`. Without this line a `Float`-typed node takes the `is_int
    // == false` branch of every arm below and is folded with `i64` arithmetic
    // ON THE IEEE-754 BIT PATTERN: `a.wrapping_div(b)` over two encodings,
    // producing a number that is not the quotient of anything.
    //
    // It was unreachable rather than wrong, because `const_value` only matches
    // `Op::Const` and an FP constant is `Op::ConstF`. That is a property of a
    // different function, one `Simplified::Zero(IrType::Float)` away from
    // becoming a silent FP miscompile, and it is not what this function's arms
    // are written against.
    if int_width(node.ty).is_none() {
        return None;
    }
    // JIT round 19 wave 4 (lane osr4, OSR2-2), opt-in: a class test of a
    // fresh allocation is a constant.
    if let Some(v) = try_fold_fresh_class_test(nodes, node) {
        return Some(v);
    }
    let is_int = node.ty == IrType::Int;
    match &node.op {
        Op::Add
        | Op::Sub
        | Op::Mul
        | Op::Div
        | Op::Rem
        | Op::And
        | Op::Or
        | Op::Xor
        | Op::Shl
        | Op::Shr
        | Op::UShr => {
            let a = const_value(nodes, node.inputs[0])?;
            let b = const_value(nodes, node.inputs[1])?;
            let raw = match &node.op {
                Op::Add => {
                    if is_int {
                        ((a as i32).wrapping_add(b as i32)) as i64
                    } else {
                        a.wrapping_add(b)
                    }
                }
                Op::Sub => {
                    if is_int {
                        ((a as i32).wrapping_sub(b as i32)) as i64
                    } else {
                        a.wrapping_sub(b)
                    }
                }
                Op::Mul => {
                    if is_int {
                        ((a as i32).wrapping_mul(b as i32)) as i64
                    } else {
                        a.wrapping_mul(b)
                    }
                }
                Op::Div => {
                    if is_int {
                        let bi = b as i32;
                        if bi == 0 {
                            return None;
                        }
                        let ai = a as i32;
                        // Java semantics: INT_MIN / -1 == INT_MIN (no exception).
                        // i32::wrapping_div handles this correctly.
                        if ai == i32::MIN && bi == -1 {
                            i32::MIN as i64
                        } else {
                            ai.wrapping_div(bi) as i64
                        }
                    } else {
                        if b == 0 {
                            return None;
                        }
                        // Java semantics: LONG_MIN / -1 == LONG_MIN (no exception).
                        if a == i64::MIN && b == -1 {
                            i64::MIN
                        } else {
                            a.wrapping_div(b)
                        }
                    }
                }
                Op::Rem => {
                    if is_int {
                        let bi = b as i32;
                        if bi == 0 {
                            return None;
                        }
                        let ai = a as i32;
                        // Java semantics: INT_MIN % -1 == 0 (no exception).
                        if ai == i32::MIN && bi == -1 {
                            0
                        } else {
                            ai.wrapping_rem(bi) as i64
                        }
                    } else {
                        if b == 0 {
                            return None;
                        }
                        // Java semantics: LONG_MIN % -1 == 0 (no exception).
                        if a == i64::MIN && b == -1 {
                            0
                        } else {
                            a.wrapping_rem(b)
                        }
                    }
                }
                Op::And => a & b,
                Op::Or => a | b,
                Op::Xor => a ^ b,
                Op::Shl => {
                    if is_int {
                        // Java: only low 5 bits of shift count for int.
                        ((a as i32).wrapping_shl((b as u32) & 0x1f)) as i64
                    } else {
                        // Java: only low 6 bits of shift count for long.
                        a.wrapping_shl((b as u32) & 0x3f)
                    }
                }
                Op::Shr => {
                    if is_int {
                        ((a as i32).wrapping_shr((b as u32) & 0x1f)) as i64
                    } else {
                        a.wrapping_shr((b as u32) & 0x3f)
                    }
                }
                Op::UShr => {
                    if is_int {
                        ((a as u32).wrapping_shr((b as u32) & 0x1f)) as i64
                    } else {
                        (a as u64).wrapping_shr((b as u32) & 0x3f) as i64
                    }
                }
                _ => unreachable!(),
            };
            // Defensive truncation: for i32 ops, ensure the high 32 bits are
            // a sign-extension of the low 32 bits. (Most paths above already
            // satisfy this, but truncating once at the end makes the
            // invariant robust to future edits.)
            let result = if is_int { (raw as i32) as i64 } else { raw };
            Some(result)
        }
        Op::Neg => {
            let a = const_value(nodes, node.inputs[0])?;
            let raw = if is_int {
                ((a as i32).wrapping_neg()) as i64
            } else {
                a.wrapping_neg()
            };
            let result = if is_int { (raw as i32) as i64 } else { raw };
            Some(result)
        }
        // Integer conversions and `lcmp` (r9). Pure, total, and each one's JLS
        // rule is a Rust cast: `i2l` sign-extends, `l2i` keeps the low 32 bits,
        // `i2b`/`i2s` truncate and sign-extend, `i2c` truncates and
        // ZERO-extends. They were not folded at all, so `(long) 7`, `(byte) c`
        // on a folded `c`, and an `lcmp` of two folded longs each survived as
        // an instruction and hid a constant from every pass downstream
        // (`fold_constants` of their users, the counted-loop recogniser, the
        // trivial-guard sweep).
        //
        // The result width is the NODE's type, which `int_width` above has
        // already restricted to `Int`/`Long`; a malformed node typed otherwise
        // never reaches here.
        Op::I2L | Op::L2I | Op::I2B | Op::I2C | Op::I2S => {
            let a = const_value(nodes, *node.inputs.first()?)?;
            let raw = match &node.op {
                Op::I2L => (a as i32) as i64,
                Op::L2I => (a as i32) as i64,
                Op::I2B => (a as i8) as i64,
                Op::I2C => (a as u16) as i64,
                Op::I2S => (a as i16) as i64,
                _ => return None,
            };
            Some(if is_int { (raw as i32) as i64 } else { raw })
        }
        // `lcmp`: sign(a - b) as -1/0/1, compared as `long`s.
        Op::LCmp => {
            let a = const_value(nodes, *node.inputs.first()?)?;
            let b = const_value(nodes, *node.inputs.get(1)?)?;
            Some(match a.cmp(&b) {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            })
        }
        Op::Cmp(cc) => {
            let a = const_value(nodes, node.inputs[0])?;
            let b = const_value(nodes, node.inputs[1])?;
            // Compare in the proper width so that two i32-typed constants
            // whose i64 sign-extensions agree compare identically. The width
            // is the OPERANDS' — a `Cmp` result is always `Int`, so keying on
            // `node.ty` would compare two `long` constants as truncated ints.
            let operands_long = matches!(nodes[node.inputs[0] as usize].ty, IrType::Long)
                || matches!(nodes[node.inputs[1] as usize].ty, IrType::Long);
            let result = if !operands_long {
                let ai = a as i32;
                let bi = b as i32;
                match cc {
                    CmpOp::Eq => ai == bi,
                    CmpOp::Ne => ai != bi,
                    CmpOp::Lt => ai < bi,
                    CmpOp::Le => ai <= bi,
                    CmpOp::Gt => ai > bi,
                    CmpOp::Ge => ai >= bi,
                }
            } else {
                match cc {
                    CmpOp::Eq => a == b,
                    CmpOp::Ne => a != b,
                    CmpOp::Lt => a < b,
                    CmpOp::Le => a <= b,
                    CmpOp::Gt => a > b,
                    CmpOp::Ge => a >= b,
                }
            };
            Some(if result { 1 } else { 0 })
        }
        // JIT round 19 wave 2 (lane irops2, IROPS19-3), opt-in.
        Op::ScalarIntrinsic(sop) if crate::ir::ir_scalar_range_fold_enabled() => {
            fold_scalar_intrinsic_int(nodes, node, *sop)
        }
        _ => None,
    }
}

/// JIT round 19 wave 2 (lane irops2, IROPS19-3): the value of an
/// integer-valued scalar intrinsic whose operands are all constants, in the
/// node's width (an `int` result sign-extended, the [`try_fold`] convention).
/// Each arm is the JDK method's specification in Rust's wrapping arithmetic:
/// `Math.abs(MIN_VALUE)` is `MIN_VALUE`, a rotate distance is masked to 5 or
/// 6 bits, `numberOfLeading/TrailingZeros(0)` is the width, the raw FP bit
/// moves read an `Op::ConstF`'s bits unchanged. `None` for a family with no
/// rule here or a non-constant operand.
pub(in crate::ir_optimize) fn fold_scalar_intrinsic_int(
    nodes: &[Node],
    node: &Node,
    sop: crate::ir::ScalarOp,
) -> Option<i64> {
    use crate::ir::ScalarOp as S;
    let arg = |k: usize| node.inputs.get(k).and_then(|&i| const_value(nodes, i));
    let fp_bits = |k: usize| match node.inputs.get(k).map(|&i| &nodes[i as usize].op) {
        Some(Op::ConstF(bits)) => Some(*bits),
        _ => None,
    };
    let int = |v: i32| Some(i64::from(v));
    match sop {
        S::MinI => int((arg(0)? as i32).min(arg(1)? as i32)),
        S::MaxI => int((arg(0)? as i32).max(arg(1)? as i32)),
        S::MinL => Some(arg(0)?.min(arg(1)?)),
        S::MaxL => Some(arg(0)?.max(arg(1)?)),
        S::AbsI => int((arg(0)? as i32).wrapping_abs()),
        S::AbsL => Some(arg(0)?.wrapping_abs()),
        S::CompareI => int((arg(0)? as i32).cmp(&(arg(1)? as i32)) as i32),
        S::CompareL => int(arg(0)?.cmp(&arg(1)?) as i32),
        // Cast: a count of at most 64 bits always fits an `int`.
        S::NlzI => int((arg(0)? as i32 as u32).leading_zeros() as i32),
        S::NlzL => int((arg(0)? as u64).leading_zeros() as i32),
        S::NtzI => int((arg(0)? as i32 as u32).trailing_zeros() as i32),
        S::NtzL => int((arg(0)? as u64).trailing_zeros() as i32),
        S::BitCountI => int((arg(0)? as i32 as u32).count_ones() as i32),
        S::BitCountL => int((arg(0)? as u64).count_ones() as i32),
        S::ReverseBytesI => int((arg(0)? as i32).swap_bytes()),
        S::ReverseBytesL => Some(arg(0)?.swap_bytes()),
        S::LowestOneBitI => {
            let x = arg(0)? as i32;
            int(x & x.wrapping_neg())
        }
        S::LowestOneBitL => {
            let x = arg(0)?;
            Some(x & x.wrapping_neg())
        }
        // `x & (MIN_VALUE >>> numberOfLeadingZeros(x))`, the JDK body; the
        // shift by the full width (x == 0) answers 0.
        S::HighestOneBitI => {
            let x = arg(0)? as i32 as u32;
            let top = 0x8000_0000u32.checked_shr(x.leading_zeros()).unwrap_or(0);
            int((x & top) as i32)
        }
        S::HighestOneBitL => {
            let x = arg(0)? as u64;
            let top = (1u64 << 63).checked_shr(x.leading_zeros()).unwrap_or(0);
            Some((x & top) as i64)
        }
        // Cast: the distance's low bits only, which the hardware masks too.
        S::RotateLeftI => int((arg(0)? as i32).rotate_left((arg(1)? as u32) & 31)),
        S::RotateRightI => int((arg(0)? as i32).rotate_right((arg(1)? as u32) & 31)),
        S::RotateLeftL => Some(arg(0)?.rotate_left((arg(1)? as u32) & 63)),
        S::RotateRightL => Some(arg(0)?.rotate_right((arg(1)? as u32) & 63)),
        S::MultiplyHighL => Some(((i128::from(arg(0)?) * i128::from(arg(1)?)) >> 64) as i64),
        S::UnsignedMultiplyHighL => {
            let (a, b) = (arg(0)? as u64, arg(1)? as u64);
            Some(((u128::from(a) * u128::from(b)) >> 64) as i64)
        }
        // Cast: a raw reinterpretation, by definition.
        S::DoubleToRawLongBits => Some(fp_bits(0)? as i64),
        S::FloatToRawIntBits => int(fp_bits(0)? as u32 as i32),
        // JIT round 20 wave 11 (lane pIR): the JDK bodies of the code-point
        // tests (signed `>=` for `charCount`, `>>> 16` for the predicates).
        S::CharCount => int(if (arg(0)? as i32) >= 0x1_0000 { 2 } else { 1 }),
        S::IsBmpCodePoint => int(i32::from((arg(0)? as i32 as u32) >> 16 == 0)),
        S::IsValidCodePoint => int(i32::from((arg(0)? as i32 as u32) >> 16 < 0x11)),
        S::IsIsoControl => {
            let cp = arg(0)? as i32 as u32;
            int(i32::from(cp <= 0x1F || cp.wrapping_sub(0x7F) <= 0x20))
        }
        _ => None,
    }
}

/// JIT round 19 wave 2 (lane irops2, IROPS19-3): the bits of an FP-valued
/// scalar intrinsic whose operand is a constant, for the families whose
/// answer is EXACTLY specified: `abs` (a sign-bit clear, NaN payloads
/// included -- the lowering's `ANDP`), `sqrt` (IEEE 754 correctly rounded),
/// `floor`/`ceil`/`rint` (exact roundings), and the raw bit moves. A NaN
/// operand or a NaN result is NOT folded for the arithmetic families, so
/// which NaN pattern comes back stays the hardware's (`SQRTSD`/`ROUNDSD`)
/// rather than Rust's. A `float` is its 32-bit pattern zero-extended, as
/// `Op::ConstF` holds it.
pub(in crate::ir_optimize) fn try_fold_scalar_fp(nodes: &[Node], id: NodeId) -> Option<u64> {
    use crate::ir::ScalarOp as S;
    let node = &nodes[id as usize];
    let Op::ScalarIntrinsic(sop) = node.op else {
        return None;
    };
    if !matches!(node.ty, IrType::Float | IrType::Double)
        || !crate::ir::ir_scalar_range_fold_enabled()
    {
        return None;
    }
    let input = *node.inputs.first()?;
    let op = &nodes[input as usize].op;
    let fp = match op {
        Op::ConstF(bits) => Some(*bits),
        _ => None,
    };
    let arith = |f: fn(f64) -> f64| -> Option<u64> {
        let x = f64::from_bits(fp?);
        let r = f(x);
        (!x.is_nan() && !r.is_nan()).then(|| r.to_bits())
    };
    match sop {
        S::AbsD => Some(fp? & 0x7FFF_FFFF_FFFF_FFFF),
        S::AbsF => Some(u64::from(fp? as u32 & 0x7FFF_FFFF)),
        S::SqrtD => arith(f64::sqrt),
        S::FloorD => arith(f64::floor),
        S::CeilD => arith(f64::ceil),
        S::RintD => arith(f64::round_ties_even),
        S::LongBitsToDouble => match op {
            // Cast: a raw reinterpretation, by definition.
            Op::Const(v) => Some(*v as u64),
            _ => None,
        },
        S::IntBitsToFloat => match op {
            // Cast: the `int`'s 32-bit pattern, zero-extended.
            Op::Const(v) => Some(u64::from(*v as i32 as u32)),
            _ => None,
        },
        _ => None,
    }
}

pub(in crate::ir_optimize) fn const_value(nodes: &[Node], id: NodeId) -> Option<i64> {
    if let Op::Const(v) = nodes[id as usize].op {
        Some(v)
    } else {
        None
    }
}

// ── Algebraic Simplification ─────────────────────────────────────────

/// The bits an `int` node can have set, when a local fact bounds them: a
/// constant mask (`x & m`), a `char` element load, a constant.
pub(in crate::ir_optimize) fn int_possible_bits(nodes: &[Node], id: NodeId) -> Option<u32> {
    let node = nodes.get(id as usize)?;
    if node.ty != IrType::Int {
        return None;
    }
    match &node.op {
        Op::Const(c) => Some(*c as u32),
        Op::ArrayLoad(MemKind::Char) => Some(0xFFFF),
        Op::And => node
            .inputs
            .iter()
            .take(2)
            .filter_map(|&i| const_value(nodes, i))
            .map(|m| m as u32)
            .next(),
        _ => None,
    }
}

/// `Some(w)` when the `int` node is its own low `w` bits sign-extended: a
/// `byte` (8) or `short` (16) element load, or a `(x << k) >> k` (`32 - k`).
pub(in crate::ir_optimize) fn int_sign_extended_from(nodes: &[Node], id: NodeId) -> Option<u32> {
    let node = nodes.get(id as usize)?;
    if node.ty != IrType::Int {
        return None;
    }
    match &node.op {
        Op::ArrayLoad(MemKind::Byte) => Some(8),
        Op::ArrayLoad(MemKind::Short) => Some(16),
        Op::Shr => {
            let k = const_value(nodes, *node.inputs.get(1)?)? & 0x1f;
            let inner = nodes.get(*node.inputs.first()? as usize)?;
            let same = matches!(inner.op, Op::Shl)
                && inner.inputs.len() >= 2
                && const_value(nodes, inner.inputs[1]).map(|c| c & 0x1f) == Some(k);
            (same && k > 0).then(|| 32 - k as u32)
        }
        _ => None,
    }
}

/// What an algebraic identity rewrites a node to.
pub(in crate::ir_optimize) enum Simplified {
    /// An existing node.
    Node(NodeId),
    /// The integer zero of the rewritten node's own type.
    Zero(IrType),
    /// An integer constant of the given type (r9: the self-compare identities,
    /// whose answer may be `1` and whose type is the compare's `Int`).
    Const(i64, IrType),
    /// A NEW `Op::Neg` of this operand, typed and pc'd like the rewritten node
    /// (round 12 wave 3, W2-6: `x / -1`, `x * -1`, `0 - x`). Integer only; see
    /// [`try_scalar_identity`].
    NegOf(NodeId),
}

/// `CRATONVM_JIT_IR_SCALAR_IDENTITIES` — default ON (round 12 wave 3, W2-6);
/// `0|false|off|no` turns [`try_scalar_identity`] off. Read once per
/// [`algebraic_simplify`] call.
pub(in crate::ir_optimize) fn scalar_identities_enabled() -> bool {
    true
}

/// Simplify expressions using algebraic identities.
pub(in crate::ir_optimize) fn algebraic_simplify(graph: &mut Graph) -> bool {
    let len = graph.nodes.len();
    let mut consts = ConstIntern::new();
    let mut changed = false;
    let identities = scalar_identities_enabled();
    for id in 0..len {
        if graph.nodes[id].op == Op::Dead {
            continue;
        }
        // `x == null` / `x != null` on a base that cannot be null is asked
        // first, because it needs the `Graph` (the receiver index) and
        // `try_simplify` sees only the arena.
        let simplified = match null_compare_answer(graph, id as NodeId) {
            Some(v) => Some(Simplified::Const(v, IrType::Int)),
            None => try_simplify(&graph.nodes, id as NodeId).or_else(|| {
                if identities {
                    try_scalar_identity(&graph.nodes, id as NodeId)
                } else {
                    None
                }
            }),
        };
        let replacement = match simplified {
            Some(Simplified::Node(n)) => n,
            // Materialised with the node's own type, never "the first
            // `Const(0)` in the arena": that may be the `Ref`-typed null
            // literal or a zero of the other integer width, and every
            // consumer (lowering width, oop maps, deopt frame values, φ joins)
            // reads `node.ty`.
            Some(Simplified::Zero(ty)) => consts.get_or_add(graph, 0, ty),
            Some(Simplified::Const(v, ty)) => consts.get_or_add(graph, v, ty),
            // A fresh node, not an in-place op change: a `Div` carries a
            // trailing guard token and is not `is_pure`, a `Neg` is neither.
            // The divide-by-zero guard the `Div` named is a DCE root on its
            // own; its `-1 != 0` condition folds and `eliminate_trivial_guards`
            // removes it on that evidence.
            Some(Simplified::NegOf(x)) => {
                let (ty, pc) = (graph.nodes[id].ty, graph.nodes[id].bytecode_pc);
                graph.add(Op::Neg, ty, vec![x], pc)
            }
            None => continue,
        };
        graph.replace_all_uses(id as NodeId, replacement);
        graph.kill(id as NodeId);
        changed = true;
    }
    changed
}

pub(in crate::ir_optimize) fn try_simplify(nodes: &[Node], id: NodeId) -> Option<Simplified> {
    let node = &nodes[id as usize];
    let inputs = &node.inputs;
    match &node.op {
        // Every identity below is an INTEGER identity. Floating-point
        // arithmetic reuses these ops typed `Float`/`Double`, and none of them
        // holds there: `x - x` and `x * 0` are NaN for NaN or ±Infinity, and
        // `x + 0` loses the sign of -0.0. (`is_const_val` only matches an
        // integer `Const`, which an FP operand never is, but the `x - x` and
        // `x ^ x` arms look at no constant at all — `d - d == 0.0` folded to
        // true for NaN.)
        Op::Add | Op::Sub | Op::Mul | Op::And | Op::Or | Op::Xor | Op::Shl | Op::Shr | Op::UShr
            if int_width(node.ty).is_none() =>
        {
            None
        }
        // x + 0 → x
        Op::Add => {
            if is_const_val(nodes, inputs[1], 0) {
                return Some(Simplified::Node(inputs[0]));
            }
            if is_const_val(nodes, inputs[0], 0) {
                return Some(Simplified::Node(inputs[1]));
            }
            None
        }
        // x - 0 → x, x - x → 0
        Op::Sub => {
            if is_const_val(nodes, inputs[1], 0) {
                return Some(Simplified::Node(inputs[0]));
            }
            if inputs[0] == inputs[1] {
                return Some(Simplified::Zero(node.ty));
            }
            None
        }
        // x * 1 → x, x * 0 → 0
        Op::Mul => {
            if is_const_val(nodes, inputs[1], 1) {
                return Some(Simplified::Node(inputs[0]));
            }
            if is_const_val(nodes, inputs[0], 1) {
                return Some(Simplified::Node(inputs[1]));
            }
            if is_const_val(nodes, inputs[1], 0) || is_const_val(nodes, inputs[0], 0) {
                return Some(Simplified::Zero(node.ty));
            }
            None
        }
        // x & 0 → 0, x & -1 → x
        Op::And => {
            if is_const_val(nodes, inputs[1], 0) || is_const_val(nodes, inputs[0], 0) {
                return Some(Simplified::Zero(node.ty));
            }
            if is_const_val(nodes, inputs[1], -1) {
                return Some(Simplified::Node(inputs[0]));
            }
            if is_const_val(nodes, inputs[0], -1) {
                return Some(Simplified::Node(inputs[1]));
            }
            // x & x → x (r9).
            if inputs[0] == inputs[1] {
                return Some(Simplified::Node(inputs[0]));
            }
            // `CRATONVM_JIT_IR_NARROW_MASK_FOLD`: `x & m` → `x` when `m` keeps
            // every bit `x` can have -- the `(char)` of a `char` load, or the
            // second `& 0xFFFF` of a `(char)(b & 0xFF)` passed through a
            // spliced `char` parameter and return.
            if node.ty == IrType::Int {
                for (x, m) in [(inputs[0], inputs[1]), (inputs[1], inputs[0])] {
                    if let (Some(m), Some(bits)) =
                        (const_value(nodes, m), int_possible_bits(nodes, x))
                    {
                        if bits & !(m as u32) == 0 {
                            return Some(Simplified::Node(x));
                        }
                    }
                }
            }
            None
        }
        // x | 0 → x, x | x → x
        Op::Or => {
            if is_const_val(nodes, inputs[1], 0) {
                return Some(Simplified::Node(inputs[0]));
            }
            if is_const_val(nodes, inputs[0], 0) {
                return Some(Simplified::Node(inputs[1]));
            }
            if inputs[0] == inputs[1] {
                return Some(Simplified::Node(inputs[0]));
            }
            None
        }
        // x ^ 0 → x, x ^ x → 0
        Op::Xor => {
            if is_const_val(nodes, inputs[1], 0) {
                return Some(Simplified::Node(inputs[0]));
            }
            if is_const_val(nodes, inputs[0], 0) {
                return Some(Simplified::Node(inputs[1]));
            }
            if inputs[0] == inputs[1] {
                return Some(Simplified::Zero(node.ty));
            }
            None
        }
        // x << 0 → x, x >> 0 → x, x >>> 0 → x — and, since r9, every count
        // the JLS masks TO zero: an `int` shift uses only the low five bits of
        // its count and a `long` shift the low six (JLS 15.19), so `x << 32` on
        // an `int` and `x >>> 64` on a `long` are both `x`. `0 << n`, `0 >> n`
        // and `0 >>> n` are `0` for every `n`.
        Op::Shl | Op::Shr | Op::UShr => {
            // `CRATONVM_JIT_IR_NARROW_MASK_FOLD`: `(x << k) >> k` → `x` (the
            // `(byte)` / `(short)` narrowing) when `x` is already
            // sign-extended from `32 - k` bits or fewer.
            if matches!(node.op, Op::Shr) && node.ty == IrType::Int {
                if let (Some(k), Some(inner)) = (
                    const_value(nodes, inputs[1]).map(|c| c & 0x1f),
                    nodes.get(inputs[0] as usize),
                ) {
                    if matches!(inner.op, Op::Shl)
                        && inner.ty == IrType::Int
                        && inner.inputs.len() >= 2
                        && const_value(nodes, inner.inputs[1]).map(|c| c & 0x1f) == Some(k)
                        && k > 0
                    {
                        let x = inner.inputs[0];
                        if int_sign_extended_from(nodes, x).is_some_and(|w| w <= 32 - k as u32) {
                            return Some(Simplified::Node(x));
                        }
                    }
                }
            }
            let mask: i64 = if node.ty == IrType::Int { 0x1f } else { 0x3f };
            if const_value(nodes, inputs[1]).is_some_and(|c| (c & mask) == 0) {
                return Some(Simplified::Node(inputs[0]));
            }
            if is_const_val(nodes, inputs[0], 0) {
                return Some(Simplified::Zero(node.ty));
            }
            None
        }
        // x / 1 → x, x % 1 → 0, x % -1 → 0 (r9).
        //
        // Integer only: `Op::Div`/`Op::Rem` are shared with FP, where `x % 1.0`
        // is the fractional part, not zero. `x % -1` is `0` for every `x`,
        // `MIN_VALUE` included (JLS 15.17.3), which is the one case a lowering
        // to `idiv` would otherwise trap on. `x / -1` is NOT here: it is
        // `-x` (wrapping at `MIN_VALUE`), which needs a new node.
        //
        // The optional trailing guard token (`Op::guard_shape`) sits at slot
        // 2 and is never read: the divisor is slot 1 in both arities. The
        // divide-by-zero guard beside the node stays — its condition folds to
        // a constant and `eliminate_trivial_guards` removes it on its own
        // evidence, not on this rewrite's.
        Op::Div | Op::Rem if int_width(node.ty).is_some() && inputs.len() >= 2 => {
            let is_int = node.ty == IrType::Int;
            let divisor =
                const_value(nodes, inputs[1]).map(|c| if is_int { (c as i32) as i64 } else { c });
            match (&node.op, divisor) {
                (Op::Div, Some(1)) => Some(Simplified::Node(inputs[0])),
                (Op::Rem, Some(1)) | (Op::Rem, Some(-1)) => Some(Simplified::Zero(node.ty)),
                _ => None,
            }
        }
        // x == x, x <= x, x >= x → 1; x != x, x < x, x > x → 0 (r9).
        //
        // `Op::Cmp` is the INTEGER / reference compare — floating point goes
        // through `Op::FCmp`, where `x == x` is false for NaN — and the operand
        // type is checked anyway, so a future FP use of `Cmp` cannot inherit
        // an identity that does not hold for it.
        Op::Cmp(cc) if inputs.len() == 2 && inputs[0] == inputs[1] => {
            if !matches!(
                nodes[inputs[0] as usize].ty,
                IrType::Int | IrType::Long | IrType::Ref
            ) {
                return None;
            }
            let v = match cc {
                CmpOp::Eq | CmpOp::Le | CmpOp::Ge => 1,
                CmpOp::Ne | CmpOp::Lt | CmpOp::Gt => 0,
            };
            Some(Simplified::Const(v, IrType::Int))
        }
        // lcmp(x, x) → 0 (r9). `long` operands only by construction.
        Op::LCmp if inputs.len() == 2 && inputs[0] == inputs[1] => Some(Simplified::Zero(node.ty)),
        // Trivial-phi elimination (standard SSA simplification, Braun et al.):
        // a phi whose value inputs (slots 1..; slot 0 is the control anchor) are
        // all either the phi itself (a back-edge that re-feeds the same value —
        // i.e. a loop-carried local that is never reassigned) or a single other
        // value `v` collapses to `v`: its value cannot differ by predecessor.
        // This removes the redundant loop-header phis the IR builder inserts for
        // *invariant* locals (e.g. the receiver `b` in `for (..) acc += b.f`),
        // exposing the real `Param`/alloc base to LICM, GVN, and the alias
        // oracle — without it LICM's load hoisting is inert on production IR
        // (the base is always a region-anchored phi → judged loop-variant).
        // A phi with two distinct non-self value inputs (an induction phi
        // `φ(0, i+1)`, an accumulator, a real merge, a memory phi advanced by an
        // in-loop op) has `single` set twice → returns None, left untouched.
        Op::Phi => {
            let mut single = NO_NODE;
            for &v in inputs.iter().skip(1) {
                if v == id {
                    continue; // self-reference (unchanged back-edge)
                }
                // An undefined edge is not "the same value": folding
                // `φ(undef, v)` to `v` hands every reader on the undefined
                // path (a frame state, at least) a value that path never
                // computed (round 11 wave 7, patch page
                // `r11w7-ircore-ir-optimize-trivial-phi-undef-patch`, from
                // `r11w6-orch-post-schedule-phi-check-refuses-hashmap`).
                if v == NO_NODE {
                    return None;
                }
                if matches!(nodes[v as usize].op, Op::Dead) {
                    continue; // a stale dead input observes nothing
                }
                if single == NO_NODE {
                    single = v;
                } else if single != v {
                    return None; // genuine merge of distinct values
                }
            }
            if single != NO_NODE && single != id {
                Some(Simplified::Node(single))
            } else {
                None
            }
        }
        _ => None,
    }
}

pub(in crate::ir_optimize) fn is_const_val(nodes: &[Node], id: NodeId, val: i64) -> bool {
    matches!(nodes[id as usize].op, Op::Const(v) if v == val)
}

/// The small integer identities of round 12 wave 3 (iropt proposal W2-6),
/// asked only when [`try_simplify`] has no answer and
/// [`scalar_identities_enabled`].
///
/// Each is exact in two's-complement `int`/`long` arithmetic (JLS 15.15.4,
/// 15.17.1, 15.18.2, 5.1.3), including at `MIN_VALUE`:
///
/// * `-(-x) → x`: negation is `~x + 1` modulo 2^n, an involution; `-MIN ==
///   MIN`, so `-(-MIN) == MIN` too.
/// * `(int)(long) x → x` for an `int` `x`: `i2l` sign-extends, `l2i` keeps
///   the low 32 bits, which are `x`'s.
/// * `x / -1 → -x`: JLS 15.17.2 defines `MIN / -1` as `MIN` (overflow, no
///   exception), which is exactly `-MIN`; for every other `x` the quotient is
///   `-x` exactly. The divisor is a nonzero constant, so the division could
///   never raise either.
/// * `x * -1 → -x` and `-1 * x → -x`: multiplication is modulo 2^n, and
///   `x * (2^n - 1) ≡ -x`.
/// * `0 - x → -x`: the definition of negation (JLS 15.15.4: `-x` equals
///   `(~x) + 1`, which is `0 - x` modulo 2^n).
///
/// Floating point is refused: `0.0 - 0.0` is `+0.0` while `-(0.0)` is `-0.0`.
/// Every operand and constant must carry the node's own integer type, so a
/// malformed mixed-width node is left alone rather than rewritten.
pub(in crate::ir_optimize) fn try_scalar_identity(
    nodes: &[Node],
    id: NodeId,
) -> Option<Simplified> {
    let node = nodes.get(id as usize)?;
    let is_int = int_width(node.ty)?;
    let typed = |n: NodeId, ty: IrType| nodes.get(n as usize).is_some_and(|m| m.ty == ty);
    // A constant of the node's own type, compared at that width.
    let konst = |k: usize| {
        let m = nodes.get(*node.inputs.get(k)? as usize)?;
        match m.op {
            Op::Const(c) if m.ty == node.ty => Some(if is_int { (c as i32) as i64 } else { c }),
            _ => None,
        }
    };
    let operand = |k: usize| node.inputs.get(k).copied().filter(|&n| typed(n, node.ty));
    match node.op {
        Op::Neg => {
            let inner = nodes.get(*node.inputs.first()? as usize)?;
            if inner.op != Op::Neg || inner.ty != node.ty {
                return None;
            }
            let x = *inner.inputs.first()?;
            typed(x, node.ty).then_some(Simplified::Node(x))
        }
        Op::L2I if node.ty == IrType::Int => {
            let inner = nodes.get(*node.inputs.first()? as usize)?;
            if inner.op != Op::I2L || inner.ty != IrType::Long {
                return None;
            }
            let x = *inner.inputs.first()?;
            typed(x, IrType::Int).then_some(Simplified::Node(x))
        }
        // Slot 2, when present, is the divide-by-zero guard token; it is not
        // an operand.
        Op::Div if node.inputs.len() >= 2 && konst(1) == Some(-1) => {
            operand(0).map(Simplified::NegOf)
        }
        Op::Mul if node.inputs.len() == 2 => {
            if konst(1) == Some(-1) {
                operand(0).map(Simplified::NegOf)
            } else if konst(0) == Some(-1) {
                operand(1).map(Simplified::NegOf)
            } else {
                None
            }
        }
        Op::Sub if node.inputs.len() == 2 && konst(0) == Some(0) => {
            operand(1).map(Simplified::NegOf)
        }
        _ => None,
    }
}

/// `x == null` → `0` and `x != null` → `1`, for an `x` that cannot be null
/// (r9).
///
/// "Cannot be null" is [`base_non_null_on_entry`] — the receiver parameter,
/// or an op `ir_check_elim::definitely_non_null` answers for (a fresh
/// allocation, a constant-pool string or class mirror) — which is the one
/// non-null authority this crate already stakes wrong-code on in two places
/// (LICM's hoist permission and the null-check eliminator's seed). The null
/// literal is the `Ref`-typed `Op::Const(0)` `IrBuilder::aconst_null` interns.
///
/// Its value is the guard it unlocks. The builder's receiver check and the
/// String expansion's `recv != null` are `Guard(Cmp(Ne)[recv, null])`; with the
/// compare folded, [`eliminate_trivial_guards`] removes the guard, and with it
/// the hard LICM barrier the guard was.
pub(in crate::ir_optimize) fn null_compare_answer(graph: &Graph, id: NodeId) -> Option<i64> {
    let node = graph.node_opt(id)?;
    let Op::Cmp(cc) = node.op else {
        return None;
    };
    if node.inputs.len() != 2 {
        return None;
    }
    let is_null = |x: NodeId| matches!(graph.node_opt(x), Some(n) if n.op == Op::Const(0) && n.ty == IrType::Ref);
    let (a, b) = (node.inputs[0], node.inputs[1]);
    let other = if is_null(b) {
        a
    } else if is_null(a) {
        b
    } else {
        return None;
    };
    if !base_non_null_on_entry(graph, other) {
        return None;
    }
    match cc {
        CmpOp::Eq => Some(0),
        CmpOp::Ne => Some(1),
        _ => None,
    }
}

/// `CRATONVM_JIT_IR_LCMP_COMPARE_FUSE` (**default ON**; `0` keeps the
/// three-way value): `Cmp(op, LCmp(a, b), 0)` becomes `Cmp(op, a, b)`, and
/// `Cmp(op, 0, LCmp(a, b))` becomes `Cmp(op.swapped(), a, b)` -- `lcmp` is
/// the exact sign of `a - b`, so the two state the same fact for every `op`,
/// and the lowering's `Cmp` already compares `long`s at 64 bits. javac's
/// `if ((mask & bit) != 0)` on a `long` is `land; lconst_0; lcmp; ifeq`, and
/// without this every test materialised `-1/0/1` with two `SETcc`s and a
/// `SUB` before comparing it to zero (`HttpUtil.validateAsciiStringToken`'s
/// token mask, twice per character). Returns whether it rewrote anything.
pub(in crate::ir_optimize) fn fuse_lcmp_compares(graph: &mut Graph) -> bool {
    let lcmp_of = |graph: &Graph, v: NodeId| -> Option<(NodeId, NodeId)> {
        graph
            .node_opt(v)
            .filter(|m| m.op == Op::LCmp && m.inputs.len() == 2)
            .map(|m| (m.inputs[0], m.inputs[1]))
    };
    // A compare that only feeds branches is left to the lowering, which fuses
    // `lcmp` + `Cmp` + `If` into one `CMP; Jcc`; a 64-bit `Cmp` under an `If`
    // is not fused the same way and would materialise its `SETcc` again.
    let mut non_branch_user = vec![false; graph.nodes.len()];
    for u in graph.nodes.iter() {
        if u.op == Op::Dead || u.op == Op::If {
            continue;
        }
        for &i in u.inputs.iter() {
            if i != NO_NODE && (i as usize) < non_branch_user.len() {
                non_branch_user[i as usize] = true;
            }
        }
    }
    let mut edits: Vec<(usize, CmpOp, NodeId, NodeId)> = Vec::new();
    for (id, n) in graph.nodes.iter().enumerate() {
        let Op::Cmp(op) = n.op else {
            continue;
        };
        if n.inputs.len() != 2 || !non_branch_user[id] {
            continue;
        }
        let (x, y) = (n.inputs[0], n.inputs[1]);
        if is_const_val(&graph.nodes, y, 0) {
            if let Some((a, b)) = lcmp_of(graph, x) {
                edits.push((id, op, a, b));
            }
        } else if is_const_val(&graph.nodes, x, 0) {
            if let Some((a, b)) = lcmp_of(graph, y) {
                edits.push((id, op.swapped(), a, b));
            }
        }
    }
    let changed = !edits.is_empty();
    for (id, op, a, b) in edits {
        graph.nodes[id].op = Op::Cmp(op);
        graph.set_inputs(id as NodeId, vec![a, b]);
    }
    changed
}

/// A `Guard(Cmp(op, a, b))` that only a single-predecessor control chain
/// reaches from an edge of an `If` deciding the same fact -- `If(Cmp(op2, a,
/// b))`'s `Proj(0)` with `op2 == op`, its `Proj(1)` with `op2.negate() ==
/// op`, or either with the operands swapped -- can never fire: its operands
/// are the same SSA values the branch compared, and every path to the guard
/// took that edge. Its condition becomes `Const(1)` and
/// [`eliminate_trivial_guards`] removes it. The case that named it: a spliced
/// `AsciiString.byteAt`'s `index >= length` throw arm, a trap guard right
/// under the `index < seq.length()` loop test of the caller that spliced it
/// (`HttpHeaderValidationUtilTest`'s value loop, one per character).
/// Returns whether it rewrote a condition.
///
/// # A zero-divisor guard is never branch-decided (interpreter round i5, wave 105)
///
/// The fact a dominating branch proves holds only BELOW that branch, so a
/// guard decided by it may be removed only if everything it licenses stays
/// below it too. An integer `Op::Div` / `Op::Rem` does not: it is a floating
/// `[a, b]` data node (its guard token edge is off by default, and
/// [`eliminate_trivial_guards`] strips it anyway), and the scheduler may place
/// it above the branch that tested its divisor. What keeps such a hoisted copy
/// from trapping is the guard ITSELF: `ir_lower::Lowerer::emit_div_zero_guard`
/// keys the division's "zero → placeholder" shape off an `Op::Guard` at the
/// division's pc (`guard_bcis`). With that guard decided away the lowerer
/// falls back to a deopt-if-zero at the bci (flat code: the H2
/// `MemoryEstimator` deopt on a path the interpreter never took) or, for a
/// spliced body, which has no flat snapshot, to a bare `IDIV` -- `#DE` on the
/// path the branch excluded. That was
/// `docs/feature-designs/jit-ir-optimizations.md`:
/// `L2W98JitArithCorners.hashOf` splices `idiv(a, b)` under `b != 0` and
/// `idivZ(a, b)` under `b == 0`; the former's guard was decided, its `Div`
/// floated (or GVN-merged with the latter's), and the bare `IDIV` ran with
/// `b == 0`. A guard whose bci is the pc of an integer division that can trap
/// is therefore skipped here, the rule the length-predicate hoist already
/// keeps for the same reason (JIT round 20 wave 5, `division_bcis` in the
/// LICM predicate pass); the range-decided route
/// (the range-decided compares, `range_decides`) stays, because a range fact holds on every
/// execution, not only below one edge.
pub(in crate::ir_optimize) fn decide_branch_implied_guards(graph: &mut Graph) -> bool {
    let div_guard_pcs: FxHashSet<usize> = graph
        .nodes
        .iter()
        .filter(|n| {
            matches!(n.op, Op::Div | Op::Rem)
                && matches!(n.ty, IrType::Int | IrType::Long)
                && !crate::ir_lower::int_division_cannot_trap(graph, n)
        })
        .filter_map(|n| n.bytecode_pc)
        .collect();
    let mut decided: Vec<NodeId> = Vec::new();
    for (id, n) in graph.nodes.iter().enumerate() {
        if !matches!(n.op, Op::Guard { .. }) || n.inputs.len() != 2 {
            continue;
        }
        if let Op::Guard { bci } = n.op {
            if div_guard_pcs.contains(&bci) {
                continue;
            }
        }
        let (ctrl, cond) = (n.inputs[0], n.inputs[1]);
        let Some(c) = graph.node_opt(cond) else {
            continue;
        };
        let Op::Cmp(op) = c.op else {
            continue;
        };
        if c.inputs.len() != 2 {
            continue;
        }
        let (a, b) = (c.inputs[0], c.inputs[1]);
        // `phi >= k` (or `k <= phi`) of a +1 induction variable that starts
        // at or above `k` and steps only under its loop test.
        let iv_bound = match (
            op,
            const_value(&graph.nodes, b),
            const_value(&graph.nodes, a),
        ) {
            (CmpOp::Ge, Some(k), _) => Some((a, k)),
            (CmpOp::Gt, Some(k), _) => k.checked_add(1).map(|k| (a, k)),
            (CmpOp::Le, _, Some(k)) => Some((b, k)),
            (CmpOp::Lt, _, Some(k)) => k.checked_add(1).map(|k| (b, k)),
            _ => None,
        };
        if iv_bound.is_some_and(|(v, k)| induction_at_least(graph, v, k)) {
            decided.push(id as NodeId);
            continue;
        }
        let mut cur = ctrl;
        for _ in 0..LOAD_CSE_CHAIN_BUDGET {
            let Some(cn) = graph.node_opt(cur) else {
                break;
            };
            match cn.op {
                Op::Proj(k) => {
                    let Some(iff_id) = cn.input_opt(0) else {
                        break;
                    };
                    let Some(iff) = graph.node_opt(iff_id) else {
                        break;
                    };
                    if iff.op != Op::If {
                        break;
                    }
                    if let Some(c2) = iff.input_opt(1).and_then(|x| graph.node_opt(x)) {
                        if let Op::Cmp(op2) = c2.op {
                            if c2.inputs.len() == 2 && k <= 1 {
                                let held = if k == 0 { op2 } else { op2.negate() };
                                let (x, y) = (c2.inputs[0], c2.inputs[1]);
                                if (x == a && y == b && held == op)
                                    || (x == b && y == a && held.swapped() == op)
                                {
                                    decided.push(id as NodeId);
                                    break;
                                }
                            }
                        }
                    }
                    match iff.input_opt(0) {
                        Some(p) => cur = p,
                        None => break,
                    }
                }
                Op::Merge | Op::Region if cn.inputs.len() == 1 => cur = cn.inputs.as_slice()[0],
                _ => break,
            }
        }
    }
    if decided.is_empty() {
        return false;
    }
    let one = graph.add(Op::Const(1), IrType::Int, vec![], None);
    for g in decided {
        let Some(node) = graph.node_opt(g) else {
            continue;
        };
        let ctrl = node.inputs[0];
        let mut inputs: Vec<NodeId> = node.inputs.iter().copied().collect();
        inputs[0] = ctrl;
        inputs[1] = one;
        graph.set_inputs(g, inputs);
    }
    true
}

/// Is the `int` loop φ `v` at least `k` on every iteration? It is when its
/// region has one entry and one back edge, the entry value is a constant
/// `>= k`, the back-edge value is `v + 1`, and every path from the header to
/// the back edge takes an edge of the header's own test that states `v < L`
/// for some `L` (so `v + 1` cannot wrap: `v < L <= MAX`). The step is then
/// monotone from the entry value and never overflows.
pub(in crate::ir_optimize) fn induction_at_least(graph: &Graph, v: NodeId, k: i64) -> bool {
    let Some(phi) = graph.node_opt(v) else {
        return false;
    };
    if phi.op != Op::Phi || phi.ty != IrType::Int || phi.inputs.len() != 3 {
        return false;
    }
    let region_id = phi.inputs[0];
    let Some(region) = graph.node_opt(region_id) else {
        return false;
    };
    if !matches!(region.op, Op::Merge | Op::Region) || region.inputs.len() != 2 {
        return false;
    }
    let (entry, back) = (phi.inputs[1], phi.inputs[2]);
    if !const_value(&graph.nodes, entry).is_some_and(|e| (e as i32 as i64) >= k) {
        return false;
    }
    let steps_by_one = graph.node_opt(back).is_some_and(|n| {
        n.op == Op::Add
            && n.ty == IrType::Int
            && n.inputs.len() == 2
            && ((n.inputs[0] == v && const_value(&graph.nodes, n.inputs[1]) == Some(1))
                || (n.inputs[1] == v && const_value(&graph.nodes, n.inputs[0]) == Some(1)))
    });
    if !steps_by_one {
        return false;
    }
    // The header's test: an `If` straight on the region, deciding `v < L` on
    // one of its edges (`continue_edge`).
    let mut continue_edge: Option<NodeId> = None;
    for u in graph.users_of(region_id) {
        let Some(iff) = graph.node_opt(u) else {
            continue;
        };
        if iff.op != Op::If || iff.input_opt(0) != Some(region_id) {
            continue;
        }
        let Some(c) = iff.input_opt(1).and_then(|x| graph.node_opt(x)) else {
            continue;
        };
        let Op::Cmp(op) = c.op else {
            continue;
        };
        if c.inputs.len() != 2 || c.inputs[0] != v {
            continue;
        }
        // `Proj(0)` holds `op`, `Proj(1)` its negation.
        let want = if op == CmpOp::Lt {
            0
        } else if op.negate() == CmpOp::Lt {
            1
        } else {
            continue;
        };
        for p in graph.users_of(u) {
            if graph.node_opt(p).is_some_and(|n| n.op == Op::Proj(want)) {
                continue_edge = Some(p);
            }
        }
    }
    let Some(cont) = continue_edge else {
        return false;
    };
    // Every control path back from the back edge meets `cont` before the
    // header (or the method entry).
    let mut seen: FxHashSet<NodeId> = FxHashSet::default();
    let mut work = vec![region.inputs[1]];
    while let Some(n) = work.pop() {
        if n == cont || !seen.insert(n) {
            continue;
        }
        if n == region_id || seen.len() > 4096 {
            return false;
        }
        let Some(node) = graph.node_opt(n) else {
            return false;
        };
        match node.op {
            Op::Start => return false,
            Op::Merge | Op::Region => work.extend(node.inputs.iter().copied()),
            _ => match node.input_opt(0) {
                Some(p) => work.push(p),
                None => return false,
            },
        }
    }
    true
}

pub(in crate::ir_optimize) fn eliminate_trivial_guards(graph: &mut Graph) -> bool {
    let mut changed = false;
    for id in 0..graph.nodes.len() {
        let gid = id as NodeId;
        let never_fires = {
            let node = &graph.nodes[id];
            matches!(node.op, Op::Guard { .. })
                && node.inputs.len() == 2
                && matches!(
                    graph.node_opt(node.inputs[1]),
                    Some(c) if c.ty == IrType::Int && matches!(c.op, Op::Const(v) if (v as i32) != 0)
                )
        };
        if !never_fires {
            continue;
        }
        let users = graph.users_of(gid);
        let strippable = users.iter().all(|&u| match graph.node_opt(u) {
            Some(n) if n.op == Op::Dead => true,
            Some(n) => crate::ir::guard_token_slot(n).is_some_and(|slot| {
                n.inputs
                    .iter()
                    .enumerate()
                    .all(|(i, &x)| (x == gid) == (i == slot))
            }),
            None => false,
        });
        if !strippable {
            continue;
        }
        for u in users {
            let kept: Vec<NodeId> = match graph.node_opt(u) {
                Some(n) if n.op != Op::Dead => match crate::ir::guard_token_slot(n) {
                    Some(slot) => n.inputs.iter().take(slot).copied().collect(),
                    None => continue,
                },
                _ => continue,
            };
            graph.set_inputs(u, kept);
        }
        graph.kill(gid);
        changed = true;
    }
    changed
}

// ── Range-decided compares (round 12 wave 3, iropt proposal W2-4) ─────
//
// `try_fold` decides a `Cmp` only when both operands are constants. The value-
// range lattice (`range_analysis::analyze_graph`) decides more:
// `(x & 0x7f) < 0x80`, `(char) c >= 0`, `(b & 0xff) <= 255`, `x >>> 28 < 16`.
// Such a compare is rewritten to the constant it always yields; a guard on it
// then goes through `eliminate_trivial_guards` (a divide-by-zero guard on
// `n | 1`, a `recv`-free bound guard), and a value use folds on.
//
// # Why it is sound
//
// `analyze_graph` is a post-fixpoint of monotone transfer functions over the
// whole method: every value a node can take on ANY execution lies in its range
// (it resets every entry to top when the fixpoint does not settle, and
// `converged()` is checked). If `ra.hi < rb.lo` then `a < b` on every
// execution, so the compare has one value and replacing it with that constant
// changes no execution. Each decision reads only the ranges of the pre-rewrite
// graph, and a rewrite replaces a node by the value it always had, so it cannot
// invalidate another decision. Refused: a bottom operand (a range that admits
// no value decides nothing a consumer may trust), operands of different widths,
// and anything not `Int`/`Long` (`Ref` compares; FP goes through `FCmp`).
// `Cmp` is always the SIGNED compare (`CmpOp` has no unsigned member).
//
// It trusts the lattice exactly as far as `ir_check_elim` already does to drop
// bounds checks, including at an OSR entry: the optimizing OSR door enters no
// body holding a speculation guard (see the range-predication notes), so every
// value reaching a header φ is one the graph models.
//
// An `If` on the folded constant is pruned by [`prune_constant_ifs`] (round 12
// wave 6): the dead arm is cut out of the graph. One the pass refuses (a loop
// in or at the edge of the dead arm, ...) keeps both successors, and `ir_lower`
// emits it as one unconditional edge (`Lowerer::constant_if_live_block`,
// `CRATONVM_JIT_IR_CONST_IF_FOLD`). Page
// `r12w3-iropt2-constant-if-is-still-a-branch-FIXED-20260926.md`.

/// The `(cc, a, b)` of an integer compare the range lattice may decide: both
/// operands the same integer type, not both constants (`try_fold`'s).
pub(in crate::ir_optimize) fn range_cmp_candidate(
    graph: &Graph,
    node: &Node,
) -> Option<(CmpOp, NodeId, NodeId)> {
    let Op::Cmp(cc) = node.op else {
        return None;
    };
    if node.ty != IrType::Int {
        return None;
    }
    let &[a, b] = node.inputs.as_slice() else {
        return None;
    };
    let (an, bn) = (graph.node_opt(a)?, graph.node_opt(b)?);
    if an.ty != bn.ty || int_width(an.ty).is_none() {
        return None;
    }
    if matches!(an.op, Op::Const(_)) && matches!(bn.op, Op::Const(_)) {
        return None;
    }
    Some((cc, a, b))
}

/// The value `a cc b` has for every `a` in `ra` and `b` in `rb`, or `None`.
pub(in crate::ir_optimize) fn range_decides(
    cc: CmpOp,
    ra: crate::range_analysis::Range,
    rb: crate::range_analysis::Range,
) -> Option<i64> {
    if ra.width() != rb.width() {
        return None;
    }
    let (alo, ahi, blo, bhi) = (ra.lo()?, ra.hi()?, rb.lo()?, rb.hi()?);
    let truth = match cc {
        CmpOp::Lt if ahi < blo => true,
        CmpOp::Lt if alo >= bhi => false,
        CmpOp::Le if ahi <= blo => true,
        CmpOp::Le if alo > bhi => false,
        CmpOp::Gt if alo > bhi => true,
        CmpOp::Gt if ahi <= blo => false,
        CmpOp::Ge if alo >= bhi => true,
        CmpOp::Ge if ahi < blo => false,
        CmpOp::Eq | CmpOp::Ne if ahi < blo || bhi < alo => cc == CmpOp::Ne,
        CmpOp::Eq | CmpOp::Ne if alo == ahi && blo == bhi && alo == blo => cc == CmpOp::Eq,
        _ => return None,
    };
    Some(i64::from(truth))
}

/// Rewrite every integer `Cmp` the whole-method range lattice decides to its
/// constant. Returns whether anything changed. See the section notes.
pub(in crate::ir_optimize) fn fold_range_decided_compares(graph: &mut Graph) -> bool {
    if !graph
        .nodes
        .iter()
        .any(|n| range_cmp_candidate(graph, n).is_some())
    {
        return false;
    }
    let ranges = crate::range_analysis::analyze_graph(graph);
    if !ranges.converged() {
        return false;
    }
    let mut decided: Vec<(NodeId, i64)> = graph
        .nodes
        .iter()
        .enumerate()
        .filter_map(|(id, n)| {
            let (cc, a, b) = range_cmp_candidate(graph, n)?;
            let v = range_decides(cc, ranges.of(a), ranges.of(b))?;
            Some((id as NodeId, v))
        })
        .collect();
    for (id, v) in super::final_typecheck_fold::bounded_nonneg_induction_compares(graph, &ranges) {
        if !decided.iter().any(|&(d, _)| d == id) {
            decided.push((id, v));
        }
    }
    if decided.is_empty() {
        return false;
    }
    let mut consts = ConstIntern::new();
    for (id, v) in decided {
        let c = consts.get_or_add(graph, v, IrType::Int);
        graph.replace_all_uses(id, c);
        graph.kill(id);
    }
    true
}

// ── Constant `If` pruning (round 12 wave 6, lane iropt5) ──────────────
//
// The optimizer half of page
// `r12w3-iropt2-constant-if-is-still-a-branch-FIXED-20260926.md` (proposal W4-3), on
// the plan the wave-5 section of that page corrected: `eliminate_dead_nodes`
// roots every effectful node by op, not by control reachability, so rewiring
// the live projection alone reclaims nothing from an arm that holds an
// access. The pass therefore cuts the arm itself.
//
// For an `If` whose condition is an `Int` constant (Proj(0) is the true edge,
// the low 32 bits decide, as in `ir_lower`'s `constant_if_live_block`):
//
// 1. `A` = the control structure forward-reachable from `Start` without
//    entering the dead projection; `D` = what is reachable from the dead
//    projection and not in `A`. `D` is exactly the control only the dead edge
//    reaches.
// 2. `K` = `D`, plus every node anchored in `D` (its input 0 is a `D` node:
//    accesses, calls, guards, allocations, returns, throws, the φs of `D`'s
//    merges), plus, transitively, every value node that uses a `K` node.
// 3. A `Merge` in `A` with inputs from `D` (the join after the arm) loses
//    those inputs, and every φ on it loses the same positions (value AND
//    memory φs: the arm's stores feed the join's memory φ). A φ left with one
//    value is replaced by it.
// 4. The live projection's users move to the `If`'s control input; the `If`,
//    both projections and all of `K` are killed. A `Return` in `K` that was
//    `graph.exit` hands `exit` to the last surviving `Return`.
//
// # Why it is sound
//
// The constant is the value the condition has on every execution of this
// compiled code (it is folded from the lattice or from constants), so the dead
// projection is never taken, and no execution reaches a `D` node: every path
// from `Start` to one crosses the dead edge (that is `D`'s definition). A node
// anchored in `D` never executes. A value built only from such nodes is never
// computed on a live path, and SSA dominance means no live node can read one
// except through a φ at a join, whose input for the dead edge is exactly what
// step 3 removes. The pass does not rely on that argument: a `K` value read by
// a node anchored in `A`, or by a φ through a live edge, REFUSES the prune.
//
// Deopt state: a snapshot whose bci lies in the dead arm is unreachable, and
// the `eliminate_dead_nodes` the caller runs next normalises its slots that
// named killed nodes to `NO_NODE` (its stale-slot step). A snapshot at a live
// bci names only live values, the join's φ rather than the arm's value.
//
// # What it refuses (the `If` keeps both successors; lowering still folds it)
//
// * a `Region` (a loop header) in `D` or with an input from `D`: cutting a
//   loop's back edge or entry changes what the loop passes and the OSR entry
//   stubs see, and needs the φ collapse and header re-derivation the page
//   lists as its own step;
// * an `If` with anything but its two projections as users, or a missing one;
// * a `D` that reaches an `A` node other than a `Merge` (impossible for a
//   well-formed graph; refused rather than assumed);
// * a φ on a join whose arity disagrees with its merge's, or that step 3 would
//   leave naming itself;
// * killing the only `Return`s (the arm was the method's only normal exit).
//
// Kill switch: `CRATONVM_JIT_IR_CONST_IF_PRUNE=0` (default ON).

// ── Arm-entry trap hoist (the r23 L8 close, L8-7) ──────────────────────────
//
// A spliced exit the builder plants as an ALWAYS-failing guard --
// `Guard{bci}[p, Const(0)]`, the unloaded-typecheck / unloaded-call / spliced
// athrow exits -- ends its arm at run time, but the builder walks on: the
// rest of the arm (a call, the field reloads after it) is built, and its
// edge into the join makes every value forwarded across the branch a φ.
// `HeapByteBuffer.getInt(int)`'s `session()` is the shape: `segment != null`
// reaches the cast `(AbstractMemorySegmentImpl) segment` of a class not yet
// loaded, so its arm is `Guard(0); Call sessionImpl(); reload hb, offset,
// limit, segment, ...`, and `absGet`'s loop then carried six such φs through
// frame homes, reloaded the `ByteBuffer` statics after the join every
// iteration, and kept the loop static hoist out (a call in the body): items
// (1)-(3) of the L8-7 RIP sample.
//
// For an `If` whose projection `p` is the control of such a guard `g`, and
// nothing anchored at `p` was built before `g` (a lower node id: the builder
// appends in walk order), the arm does nothing observable before `g`
// deopts. So `g` moves to the `If`'s own block, guarding the OTHER edge's
// condition (`k` when `p` is the false edge, `!k` otherwise), and the `If`'s
// condition becomes the constant that takes the other edge; the constant-`If`
// prune (`prune_constant_ifs`, which runs right after) then cuts the arm, its
// join inputs and φ inputs.
//
// # Why it is sound
//
// * Same answers on the live edge: `g` passes exactly when the `If` would
//   have taken the other edge, and that edge's code is untouched.
// * Same deopt on the dead edge: before, the arm ran nothing anchored ahead
//   of `g` (no lower-id node is anchored at `p`), so reaching `g` and
//   deopting there was the arm's only observable act; now the deopt happens
//   at the branch with the same guard -- its `bci`, its trap reason (the
//   per-pc class-loading / unstable-if tables), its chain or by-bci frame,
//   none of which depend on its block. Every value that frame names was
//   built before `g` and so is not anchored in the arm; a frame naming an
//   allocation is refused (virtual objects are described per block).
// * Program order in the `If`'s block. Its effects run in node-id order
//   (`ir_schedule::priority_sort_block`'s side-effect chain), and after the
//   prune that block also holds the LIVE edge's nodes. So `g` must come after
//   every effect anchored at the `If`'s control and before every effect
//   anchored on the live edge (its projection and single-input `Merge`
//   wrappers). The builder appends in walk order, which gives the first
//   whenever no later pass anchored an effect there, and the second only when
//   it built the trap arm before the live one (a fallthrough trap arm, the
//   `HeapByteBuffer` shape). Both are checked and refused otherwise. In
//   `MetaClassImpl.chooseMethodInternal` (Groovy 5) the trap arm is the jump
//   target: `if (o instanceof ParameterTypes) { ((ParameterTypes) o)...` was
//   built first, so its checkcast and call ran ahead of the hoisted guard and
//   threw `ClassCastException` on a `FastArray` (quarkus
//   RunOnVirtualThreadDisabledTest; `docs/feature-designs/jit-ir-optimizations.md`).
//   A guard that moves onto another hoist's live edge is ordered after that
//   hoist's guard, or dropped.
//
// Kill switch: `CRATONVM_JIT_IR_ARM_TRAP_HOIST=0` (default ON; it was default OFF from
// 2026-10-25 until the program-order refusals above fixed the Groovy miscompile).

/// One hoist [`hoist_arm_entry_traps`] admitted.
pub(in crate::ir_optimize) struct ArmTrapPlan {
    /// The always-failing guard.
    pub(in crate::ir_optimize) gid: NodeId,
    /// The branch it moves to, and that branch's control.
    pub(in crate::ir_optimize) if_id: NodeId,
    pub(in crate::ir_optimize) if_ctrl: NodeId,
    /// The edge the guard sits on (`Proj(w)`), and the branch's condition.
    pub(in crate::ir_optimize) w: u16,
    pub(in crate::ir_optimize) k: NodeId,
    /// The other edge's projection and its single-input `Merge` wrappers:
    /// the prune merges what is anchored there into the branch's block.
    pub(in crate::ir_optimize) live_chain: Vec<NodeId>,
}

/// Hoist every always-failing guard that opens an `If` arm to the branch, and
/// make the branch constant (the section notes above). Returns how many were
/// hoisted; the caller runs [`prune_constant_ifs`] next.
pub(in crate::ir_optimize) fn hoist_arm_entry_traps(graph: &mut Graph) -> usize {
    if !const_if_prune_enabled() {
        return 0;
    }
    let users = build_users(graph);
    let users_of = |id: NodeId| users.get(id as usize).map(Vec::as_slice).unwrap_or(&[]);
    let is_zero = |id: NodeId| {
        graph
            .node_opt(id)
            .is_some_and(|n| n.op == Op::Const(0) && n.ty == IrType::Int)
    };
    let mut plans: Vec<ArmTrapPlan> = Vec::new();
    for (gid, g) in graph.nodes.iter().enumerate() {
        // Cast: an arena index is a `NodeId`.
        let gid = gid as NodeId;
        let Op::Guard { bci } = g.op else {
            continue;
        };
        let (Some(p), Some(c)) = (g.input_opt(0), g.input_opt(1)) else {
            continue;
        };
        if g.inputs.len() != 2 || !is_zero(c) {
            continue;
        }
        let dbg = false;
        let refuse = |why: &str| {
            if dbg {
                eprintln!(
                    "[ir] arm-entry trap n{gid} (bci {bci}) not hoisted: {why} in {}",
                    graph.dbg_label
                );
            }
        };
        // The builder wraps a branch projection in single-input `Merge`s:
        // step through them to the projection.
        let mut chain: Vec<NodeId> = vec![p];
        let mut proj = p;
        while let Some(n) = graph.node_opt(proj) {
            if n.op == Op::Merge && n.inputs.len() == 1 && chain.len() < 4 {
                proj = n.inputs[0];
                chain.push(proj);
            } else {
                break;
            }
        }
        let Some(pn) = graph.node_opt(proj) else {
            continue;
        };
        let Op::Proj(w) = pn.op else {
            refuse("its control is not a branch projection");
            continue;
        };
        if pn.ty != IrType::Control || w > 1 {
            continue;
        }
        let Some(if_id) = pn.input_opt(0) else {
            continue;
        };
        let Some(ifn) = graph.node_opt(if_id) else {
            continue;
        };
        if ifn.op != Op::If || ifn.inputs.len() != 2 {
            continue;
        }
        let Some(k) = ifn.input_opt(1) else {
            continue;
        };
        // The condition must be a 0/1 compare (the guard reads "non-zero
        // passes"; the negation below needs the compare's own operands).
        if !graph
            .node_opt(k)
            .is_some_and(|n| matches!(n.op, Op::Cmp(_)) && n.inputs.len() == 2)
        {
            refuse("the branch condition is not a compare");
            continue;
        }
        // Nothing of the arm before the guard.
        let early = chain.iter().any(|&c| {
            users_of(c).iter().any(|&u| {
                u != gid
                    && u < gid
                    && graph.node_opt(u).is_some_and(|n| {
                        n.op != Op::Dead && n.input_opt(0) == Some(c) && !n.op.is_control()
                    })
            })
        });
        if early {
            refuse("the arm runs a node before it");
            continue;
        }
        // A frame at the guard's bci naming an allocation stays where it is.
        let names_alloc = graph.safepoints.iter().filter(|s| s.bci == bci).any(|s| {
            s.locals
                .iter()
                .chain(s.stack.iter())
                .chain(s.monitors.iter())
                .any(|&v| {
                    graph.node_opt(v).is_some_and(|n| {
                        matches!(
                            n.op,
                            Op::New { .. } | Op::NewArray { .. } | Op::MultiNewArray { .. }
                        )
                    })
                })
        });
        if names_alloc || plans.iter().any(|p| p.if_id == if_id) {
            refuse("its frame names an allocation, or the branch has another");
            continue;
        }
        // Program order in the If's block is node-id order among its
        // effects (`ir_schedule::priority_sort_block`'s side-effect chain).
        // The prune moves the LIVE edge's nodes into that block, so `g`
        // must precede every effect anchored on the live edge, and must not
        // precede an effect already anchored at the If's control (a pass
        // may have placed one there after the build): `g` would deopt with
        // the arm's frame before it ran. The live edge's single-input
        // `Merge` wrappers count as the edge.
        let Some(if_ctrl) = ifn.input_opt(0) else {
            continue;
        };
        let live_proj = users_of(if_id).iter().copied().find(|&u| {
            graph
                .node_opt(u)
                .is_some_and(|n| n.op == Op::Proj(1 - w) && n.ty == IrType::Control)
        });
        let Some(live_proj) = live_proj else {
            refuse("the branch has no live projection");
            continue;
        };
        let mut live_chain: Vec<NodeId> = vec![live_proj];
        while live_chain.len() < 4 {
            let last = *live_chain.last().unwrap_or(&live_proj);
            let next = users_of(last).iter().copied().find(|&u| {
                graph
                    .node_opt(u)
                    .is_some_and(|n| n.op == Op::Merge && n.inputs.len() == 1)
            });
            match next {
                Some(m) => live_chain.push(m),
                None => break,
            }
        }
        let effect_at = |c: NodeId, u: NodeId| {
            graph.node_opt(u).is_some_and(|n| {
                n.op != Op::Dead
                    && n.op != Op::Phi
                    && !n.op.is_control()
                    && !n.op.is_pure()
                    && n.input_opt(0) == Some(c)
            })
        };
        let live_first = live_chain
            .iter()
            .any(|&c| users_of(c).iter().any(|&u| u < gid && effect_at(c, u)));
        let late_at_branch = users_of(if_ctrl)
            .iter()
            .any(|&u| u > gid && effect_at(if_ctrl, u));
        if live_first || late_at_branch {
            refuse("an effect the branch block would run is ordered on the wrong side of it");
            continue;
        }
        plans.push(ArmTrapPlan {
            gid,
            if_id,
            w,
            k,
            if_ctrl,
            live_chain,
        });
    }
    // A planned guard that moves into another plan's merged block (its If
    // stands on that plan's live edge) runs there in id order too: it must
    // come after that plan's guard, or it deopts at a bci the other branch
    // never let control reach.
    loop {
        let late = (0..plans.len()).find(|&q| {
            plans.iter().any(|p| {
                p.gid != plans[q].gid
                    && plans[q].gid < p.gid
                    && p.live_chain.contains(&plans[q].if_ctrl)
            })
        });
        match late {
            Some(q) => {
                plans.remove(q);
            }
            None => break,
        }
    }
    let mut n = 0usize;
    for ArmTrapPlan {
        gid,
        if_id,
        w,
        k,
        if_ctrl,
        ..
    } in plans
    {
        let pc = graph.nodes[gid as usize].bytecode_pc;
        if let Some(bci) = pc {
            if !graph.hoisted_arm_trap_pcs.contains(&bci) {
                graph.hoisted_arm_trap_pcs.push(bci);
            }
        }
        // The guard keeps the edge the arm does NOT take.
        let keep = if w == 1 {
            k
        } else {
            let kn = &graph.nodes[k as usize];
            let Op::Cmp(cc) = &kn.op else {
                continue;
            };
            let (cc, a, b) = (*cc, kn.inputs[0], kn.inputs[1]);
            graph.add(Op::Cmp(cc.negate()), IrType::Int, vec![a, b], pc)
        };
        let g = &mut graph.nodes[gid as usize];
        g.inputs[0] = if_ctrl;
        g.inputs[1] = keep;
        let live = graph.add(Op::Const(i64::from(w == 1)), IrType::Int, Vec::new(), None);
        graph.nodes[if_id as usize].inputs[1] = live;
        n += 1;
        if false {
            eprintln!(
                "[ir] arm-entry trap hoisted: guard n{gid} (bci {pc:?}) to If n{if_id} in {}",
                graph.dbg_label
            );
        }
    }
    n
}

/// Kill switch for [`prune_constant_ifs`]. **Default ON**. Read once per
/// compile.
pub(in crate::ir_optimize) fn const_if_prune_enabled() -> bool {
    true
}

/// What [`prune_constant_if`] does to the graph, decided before any mutation.
pub(in crate::ir_optimize) struct ConstIfPrune {
    pub(in crate::ir_optimize) if_id: NodeId,
    pub(in crate::ir_optimize) if_ctrl: NodeId,
    pub(in crate::ir_optimize) live_proj: NodeId,
    /// Every node to kill (`K` in the section notes), `D` included.
    pub(in crate::ir_optimize) kill: FxHashSet<NodeId>,
    /// Each join after the dead arm, with the input positions it keeps.
    pub(in crate::ir_optimize) joins: Vec<(NodeId, Vec<usize>)>,
    /// The `Return` that becomes `graph.exit`, when the old one is killed.
    pub(in crate::ir_optimize) new_exit: Option<NodeId>,
}

/// Prune the dead arm of every `If` on an `Int` constant that
/// [`plan_constant_if_prune`] admits. Returns how many were pruned. See the
/// section notes.
pub(in crate::ir_optimize) fn prune_constant_ifs(graph: &mut Graph) -> usize {
    if !const_if_prune_enabled() {
        return 0;
    }
    let candidates: Vec<NodeId> = graph
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| {
            n.op == Op::If
                && n.input_opt(1)
                    .and_then(|c| graph.node_opt(c))
                    .is_some_and(|c| matches!(c.op, Op::Const(_)) && c.ty == IrType::Int)
        })
        .map(|(id, _)| id as NodeId)
        .collect();
    let mut pruned = 0usize;
    for if_id in candidates {
        // An earlier prune may have killed this `If` with its arm.
        if let Some(plan) = plan_constant_if_prune(graph, if_id) {
            apply_constant_if_prune(graph, plan);
            pruned += 1;
        }
    }
    pruned
}

/// Decide the prune of the constant `If` `if_id` without touching the graph,
/// or `None` when it is refused (see the section notes).
pub(in crate::ir_optimize) fn plan_constant_if_prune(
    graph: &Graph,
    if_id: NodeId,
) -> Option<ConstIfPrune> {
    let if_node = graph.node_opt(if_id)?;
    if if_node.op != Op::If {
        return None;
    }
    let if_ctrl = if_node.input_opt(0)?;
    let cond = graph.node_opt(if_node.input_opt(1)?)?;
    let Op::Const(v) = cond.op else {
        return None;
    };
    if cond.ty != IrType::Int {
        return None;
    }
    let live_which: u16 = if (v as i32) != 0 { 0 } else { 1 };
    let users = build_users(graph);
    fn users_at(users: &[Vec<NodeId>], id: NodeId) -> &[NodeId] {
        users.get(id as usize).map(Vec::as_slice).unwrap_or(&[])
    }
    let node_users = |id: NodeId| users_at(&users, id);
    let is_live = |id: NodeId| graph.node_opt(id).is_some_and(|n| n.op != Op::Dead);

    // The `If`'s users must be exactly its two control projections.
    let mut live_proj: Option<NodeId> = None;
    let mut dead_proj: Option<NodeId> = None;
    for &u in node_users(if_id) {
        if !is_live(u) {
            continue;
        }
        let un = graph.node_opt(u)?;
        match un.op {
            Op::Proj(w) if un.ty == IrType::Control && w == live_which && live_proj.is_none() => {
                live_proj = Some(u);
            }
            Op::Proj(w)
                if un.ty == IrType::Control && w == 1 - live_which && dead_proj.is_none() =>
            {
                dead_proj = Some(u);
            }
            _ => return None,
        }
    }
    let (live_proj, dead_proj) = (live_proj?, dead_proj?);

    // `A`: forward control reachable from `Start`, never entering `dead_proj`.
    if !is_live(graph.entry) {
        return None;
    }
    let mut reach: FxHashSet<NodeId> = FxHashSet::default();
    reach.insert(graph.entry);
    let mut work: Vec<NodeId> = vec![graph.entry];
    while let Some(c) = work.pop() {
        for &u in node_users(c) {
            if u != dead_proj
                && graph.node_opt(u).is_some_and(is_forward_control)
                && reach.insert(u)
            {
                work.push(u);
            }
        }
    }
    // Pruning unreachable code is `eliminate_dead_nodes`' job, not this one's.
    if !reach.contains(&if_id) {
        return None;
    }

    // `D`: reachable from `dead_proj`, not in `A`. Whatever `D` reaches in `A`
    // must be a join (`Merge`); anything else is refused.
    let mut dead_ctrl: FxHashSet<NodeId> = FxHashSet::default();
    dead_ctrl.insert(dead_proj);
    let mut joins: FxHashSet<NodeId> = FxHashSet::default();
    let mut work: Vec<NodeId> = vec![dead_proj];
    while let Some(c) = work.pop() {
        for &u in node_users(c) {
            let Some(un) = graph.node_opt(u) else {
                continue;
            };
            if !is_forward_control(un) {
                continue;
            }
            if reach.contains(&u) {
                if un.op != Op::Merge {
                    return None;
                }
                joins.insert(u);
            } else if dead_ctrl.insert(u) {
                work.push(u);
            }
        }
    }
    if dead_ctrl
        .iter()
        .any(|&c| graph.node_opt(c).is_some_and(|n| n.op == Op::Region))
    {
        return None;
    }

    // Each join keeps the positions whose control is not in `D`, and every φ
    // on it must line up with it.
    let mut join_plan: Vec<(NodeId, Vec<usize>)> = Vec::with_capacity(joins.len());
    let mut join_list: Vec<NodeId> = joins.iter().copied().collect();
    join_list.sort_unstable();
    for &m in &join_list {
        let mn = graph.node_opt(m)?;
        let kept: Vec<usize> = mn
            .inputs
            .iter()
            .enumerate()
            .filter(|&(_, &c)| !dead_ctrl.contains(&c))
            .map(|(j, _)| j)
            .collect();
        if kept.is_empty() {
            return None;
        }
        for &p in node_users(m) {
            let Some(pn) = graph.node_opt(p) else {
                continue;
            };
            if pn.op != Op::Phi || pn.input_opt(0) != Some(m) {
                continue;
            }
            if pn.inputs.len() != mn.inputs.len() + 1 {
                return None;
            }
            if kept.len() == 1 {
                let only = kept.first().and_then(|&j| pn.inputs.get(j + 1).copied());
                if only.is_none() || only == Some(p) || only == Some(NO_NODE) {
                    return None;
                }
            }
        }
        join_plan.push((m, kept));
    }

    // `K`: `D`, what is anchored in it, and every value computed from those.
    let is_control_node = |id: NodeId| graph.node_opt(id).is_some_and(|n| n.ty == IrType::Control);
    let mut kill: FxHashSet<NodeId> = dead_ctrl.clone();
    let mut work: Vec<NodeId> = dead_ctrl.iter().copied().collect();
    while let Some(x) = work.pop() {
        for &u in node_users(x) {
            if kill.contains(&u) || joins.contains(&u) || !is_live(u) {
                continue;
            }
            let un = graph.node_opt(u)?;
            if is_forward_control(un) {
                // A control user outside `D` and not a join was refused above.
                return None;
            }
            let anchor = un.input_opt(0).filter(|&a| is_control_node(a));
            if un.op == Op::Phi {
                let m = un.input_opt(0)?;
                if dead_ctrl.contains(&m) {
                    kill.insert(u);
                    work.push(u);
                    continue;
                }
                // A φ on a join may read a `K` value only through a dead edge.
                let (_, kept) = join_plan.iter().find(|(j, _)| *j == m)?;
                let reads_through_live_edge = un
                    .inputs
                    .iter()
                    .enumerate()
                    .skip(1)
                    .any(|(k, &val)| val == x && kept.contains(&(k - 1)));
                if reads_through_live_edge {
                    return None;
                }
                continue;
            }
            match anchor {
                Some(a) if dead_ctrl.contains(&a) => {
                    kill.insert(u);
                    work.push(u);
                    // A dead zero-divisor guard takes its division with it: the
                    // `Div`/`Rem` floats and reads only live operands, so
                    // nothing above reaches it, and without its guard the
                    // lowerer deopts on a zero divisor wherever the scheduler
                    // put it -- re-running the dead arm's division in the
                    // interpreter (`R21FuzzGen85`'s `x / (i0 - p)` under an
                    // `o.f0 < o.f0` it never took threw into its handler).
                    if matches!(un.op, Op::Guard { .. }) {
                        for d in divisions_licensed_by(graph, &users, u) {
                            if is_live(d) && kill.insert(d) {
                                work.push(d);
                            }
                        }
                    }
                }
                // Anchored in live control and reading a dead value.
                Some(_) => return None,
                // A floating value computed from a dead one.
                None => {
                    kill.insert(u);
                    work.push(u);
                }
            }
        }
    }
    if kill.contains(&if_id) || kill.contains(&live_proj) || kill.contains(&if_ctrl) {
        return None;
    }

    // `exit`: moved to the last surviving `Return` if the arm owned it.
    let new_exit = if kill.contains(&graph.exit) {
        let survivor = graph
            .nodes
            .iter()
            .enumerate()
            .rev()
            .map(|(id, n)| (id as NodeId, n))
            .find(|&(id, n)| n.op == Op::Return && !kill.contains(&id))
            .map(|(id, _)| id)?;
        Some(survivor)
    } else {
        None
    };

    Some(ConstIfPrune {
        if_id,
        if_ctrl,
        live_proj,
        kill,
        joins: join_plan,
        new_exit,
    })
}

/// The integer divisions whose zero-divisor guard is `guard`: the `Div`/`Rem`
/// `IrBuilder::add_div_zero_guard` anchored it beside, i.e. at the guard's bci
/// and dividing by the value the guard's `Cmp(Ne)[divisor, 0]` tests. With the
/// guard-token edge off (`ir::guard_token_edges_enabled`, the default) this is
/// the only link between the two -- the lowerer makes the same match by bci
/// (`ir_lower::Lowerer::emit_div_zero_guard`). A `Div` is not `is_pure`, so GVN
/// never merges one division into another site's.
pub(in crate::ir_optimize) fn divisions_licensed_by(
    graph: &Graph,
    users: &[Vec<NodeId>],
    guard: NodeId,
) -> Vec<NodeId> {
    let Some(g) = graph.node_opt(guard) else {
        return Vec::new();
    };
    let Op::Guard { bci } = g.op else {
        return Vec::new();
    };
    let Some(cond) = g.input_opt(1).and_then(|c| graph.node_opt(c)) else {
        return Vec::new();
    };
    if cond.op != Op::Cmp(CmpOp::Ne) {
        return Vec::new();
    }
    let (Some(divisor), Some(zero)) = (cond.input_opt(0), cond.input_opt(1)) else {
        return Vec::new();
    };
    if !graph
        .node_opt(zero)
        .is_some_and(|z| matches!(z.op, Op::Const(0)))
    {
        return Vec::new();
    }
    users
        .get(divisor as usize)
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .copied()
        .filter(|&d| {
            graph.node_opt(d).is_some_and(|n| {
                matches!(n.op, Op::Div | Op::Rem)
                    && matches!(n.ty, IrType::Int | IrType::Long)
                    && n.bytecode_pc == Some(bci)
                    && n.input_opt(1) == Some(divisor)
            })
        })
        .collect()
}

/// Apply a [`plan_constant_if_prune`] answer. Every check was made there.
pub(in crate::ir_optimize) fn apply_constant_if_prune(graph: &mut Graph, plan: ConstIfPrune) {
    for (m, kept) in &plan.joins {
        let phis: Vec<NodeId> = graph
            .users_of(*m)
            .into_iter()
            .filter(|&p| {
                graph
                    .node_opt(p)
                    .is_some_and(|n| n.op == Op::Phi && n.input_opt(0) == Some(*m))
            })
            .collect();
        for p in phis {
            let Some(pn) = graph.node_opt(p) else {
                continue;
            };
            let values: Vec<NodeId> = kept
                .iter()
                .filter_map(|&j| pn.inputs.get(j + 1).copied())
                .collect();
            if let [only] = values.as_slice() {
                let only = *only;
                graph.replace_all_uses(p, only);
                graph.kill(p);
            } else {
                let mut inputs = Vec::with_capacity(values.len() + 1);
                inputs.push(*m);
                inputs.extend(values);
                graph.set_inputs(p, inputs);
            }
        }
        let Some(mn) = graph.node_opt(*m) else {
            continue;
        };
        let inputs: Vec<NodeId> = kept
            .iter()
            .filter_map(|&j| mn.inputs.get(j).copied())
            .collect();
        graph.set_inputs(*m, inputs);
    }
    let keep_proj_as_merge = collapse_would_invert_effects(graph, plan.if_ctrl, plan.live_proj);
    if keep_proj_as_merge {
        if let Some(proj_node) = graph.nodes.get_mut(plan.live_proj as usize) {
            proj_node.op = Op::Merge;
        }
        graph.set_inputs(plan.live_proj, vec![plan.if_ctrl]);
    } else {
        graph.replace_all_uses(plan.live_proj, plan.if_ctrl);
    }
    let mut dead: Vec<NodeId> = plan.kill.into_iter().collect();
    dead.sort_unstable();
    for id in dead {
        graph.kill(id);
    }
    if !keep_proj_as_merge {
        graph.kill(plan.live_proj);
    }
    graph.kill(plan.if_id);
    if let Some(exit) = plan.new_exit {
        graph.exit = exit;
    }
}

// ── Global Value Numbering (GVN) ─────────────────────────────────────

/// Deduplicate nodes that compute the same value.
///
/// Uses a pre-computed hash of (op, ty, inputs) as the map key to avoid
/// cloning the inputs Vec on every lookup.
pub(in crate::ir_optimize) fn gvn(graph: &mut Graph) {
    // Hot-path: use FxHashMap (rustc_hash) instead of the std SipHash-backed
    // HashMap. The keys are 64-bit value-number hashes (no adversarial input),
    // so the faster FxHasher gives ≈2x lookup throughput with the same
    // collision behavior in practice.
    // A bucket is `(first, rest)` rather than a single id, so a hash collision
    // costs one extra comparison instead of the rest of the pass.
    //
    // It used to be `FxHashMap<u64, NodeId>`: on a collision — the entry is
    // present, the full identity comparison fails — the code did nothing at
    // all. It neither merged nor replaced the entry, so every LATER node with
    // that hash was compared against the same stale occupant and no true
    // duplicate behind it was ever found again. With `FxHasher` over 64-bit
    // keys that is rare, and "rare and silent" is exactly the kind of
    // correctness-of-the-optimizer bug that never gets noticed.
    //
    // The overflow `Vec` allocates only when a collision actually happens.
    // True duplicates never grow a bucket: the first occurrence is stored and
    // every later one is merged into it, so a bucket reaches two entries only
    // when two DIFFERENT values hash alike.
    let mut value_map: FxHashMap<u64, (NodeId, Vec<NodeId>)> = FxHashMap::default();
    let len = graph.nodes.len();
    for id in 0..len {
        let id = id as NodeId;
        let node = &graph.nodes[id as usize];
        if node.op == Op::Dead || !node.op.is_pure() {
            continue;
        }
        // A commutative op is keyed on its operands in canonical order (r9),
        // so `a + b` and `b + a` are one value. See [`gvn_canonical_pair`].
        let canon = gvn_canonical_pair(node);
        let key: &[NodeId] = match &canon {
            Some(pair) => &pair[..],
            None => node.inputs.as_slice(),
        };
        let hash = gvn_hash(&node.op, node.ty, node.frame_snapshot, key);
        // Verify a bucket occupant is actually the same node (hash collision
        // check).
        //
        // `frame_snapshot` is part of the identity, not decoration: two nodes
        // computing the same value in different COPIES of an unrolled body are
        // different program points, and merging them would give one copy's
        // code the other copy's deopt frame. `None` on both is every node of
        // every graph that did not unroll, so this costs nothing there.
        let mut found: Option<NodeId> = None;
        let mut bucket_exists = false;
        if let Some(bucket) = value_map.get(&hash) {
            bucket_exists = true;
            for cand in std::iter::once(bucket.0).chain(bucket.1.iter().copied()) {
                if cand == id {
                    continue;
                }
                let c = &graph.nodes[cand as usize];
                let same_inputs = match (&canon, gvn_canonical_pair(c)) {
                    (Some(x), Some(y)) => *x == y,
                    (None, None) => c.inputs == node.inputs,
                    _ => false,
                };
                if c.op == node.op
                    && c.ty == node.ty
                    && c.frame_snapshot == node.frame_snapshot
                    && same_inputs
                {
                    found = Some(cand);
                    break;
                }
            }
        }
        match found {
            Some(existing) => {
                graph.replace_all_uses(id, existing);
                graph.kill(id);
            }
            // A real collision: keep this node in the bucket too, so the next
            // node that hashes here can still be deduped against it.
            None if bucket_exists => {
                if let Some(b) = value_map.get_mut(&hash) {
                    b.1.push(id);
                }
            }
            None => {
                value_map.insert(hash, (id, Vec::new()));
            }
        }
    }
}

/// A commutative binary node's two operands in ascending id order, or `None`
/// for any other node (r9).
///
/// Without it GVN compared operand lists positionally, so `a + b` and `b + a`
/// — which the builder produces whenever source order differs, and which
/// `reassociate_affine` and the unroller's cloning produce on their own — were
/// two values, two registers and two instructions.
///
/// Integer `Add`/`Mul`/`And`/`Or`/`Xor` only: the FP forms of `Add`/`Mul`
/// commute in value but NOT in bits when both operands are NaN (x86 returns
/// the first operand's payload, and `doubleToRawLongBits` can see it).
/// `Cmp(Eq)` / `Cmp(Ne)` are symmetric at every operand type, NaN included.
pub(in crate::ir_optimize) fn gvn_canonical_pair(node: &Node) -> Option<[NodeId; 2]> {
    let commutes = match node.op {
        Op::Add | Op::Mul | Op::And | Op::Or | Op::Xor => int_width(node.ty).is_some(),
        Op::Cmp(CmpOp::Eq) | Op::Cmp(CmpOp::Ne) => true,
        _ => false,
    };
    if !commutes || node.inputs.len() != 2 {
        return None;
    }
    let (a, b) = (node.inputs[0], node.inputs[1]);
    Some(if a <= b { [a, b] } else { [b, a] })
}

/// Compute a hash of (op, ty, inputs) for GVN without cloning the inputs Vec.
///
/// Uses `FxHasher` rather than the std default (SipHash) — GVN runs on every
/// JIT compile and the values are non-adversarial node identifiers, so the
/// 2-3x faster Fx hash is a strict win.
pub(in crate::ir_optimize) fn gvn_hash(
    op: &Op,
    ty: IrType,
    frame_snapshot: Option<u32>,
    inputs: &[NodeId],
) -> u64 {
    let mut hasher = FxHasher::default();
    op.hash(&mut hasher);
    ty.hash(&mut hasher);
    frame_snapshot.hash(&mut hasher);
    inputs.hash(&mut hasher);
    hasher.finish()
}

// ── Redundant-load elimination ─────────────────────────────────
//
// Two reads of the same cell out of the same heap compute the same value, and
// until 2026-09-11 nothing in this file said so.
//
// [`gvn`] cannot: it skips every node that is not `Op::is_pure()`, and no
// memory op is pure. That is the right rule for `gvn` — a load's value depends
// on the heap, which its `(op, ty, inputs)` key does not describe — but it
// meant the IR tier had no redundant-load elimination at all.
//
// Making `gvn` load-aware would still not have fired, for a second reason that
// is easy to miss: the builder advances the memory token on every READ
// (`ir.rs`, `self.mem = load`, for the WAR ordering a later store needs). Four
// `getfield`s in a row are therefore a CHAIN — `L1(mem=M)`, `L2(mem=L1)`,
// `L3(mem=L2)`, `L4(mem=L3)` — and no two of them share a memory input, so
// input-equality would reject every pair. `probes/FieldLoop.java` `sumWide` is
// exactly this shape, and its loop body ran four complete field-read sequences
// per iteration for one field that nothing writes
// (`docs/feature-designs/jit-ir-optimizations.md`
// §7).
//
// So this pass keys on everything EXCEPT the memory token, and then asks the
// question the token was hiding: is `B`'s memory state reachable from `A`'s
// through nodes that cannot have written the cell `B` reads? Only
// `MemAccess::{FieldRead, ArrayRead, LengthRead}` — reads, which advance the
// token and write nothing — may appear on that path. One store, one call, one
// allocation, one memory φ stops the walk and the pair is left alone.
//
// SOUNDNESS, the four things that have to hold:
//
//   * **Same address.** Every input except the token is compared for
//     node-identity, so the base, the field-index `Const` and (for an
//     `ArrayLoad`) the index are literally the same nodes. The field identity
//     of an `Op::Load` lives in an input (`Const(field_index)`), not in a side
//     table, so input equality IS address equality.
//   * **Same heap.** The chain walk above. `from == to` — the two loads
//     already naming one memory state, which is what LICM's memory-edge hoist
//     leaves behind — is the zero-length case and passes trivially.
//   * **Same place.** `A` and `B` must share a control anchor, and so must
//     every node on the chain between them. That makes the whole argument a
//     within-block one and removes any need for a dominance query: `A` and `B`
//     execute exactly when each other do. Dropping this would admit `if (c) x
//     = o.f; else y = o.f;`, where neither load dominates the other and
//     merging them speculates a read (and its null check) onto a path that
//     need not take it — the same class of bug as the pre-header hoist fixed
//     in §3a of the page above.
//   * **Same program point.** `frame_snapshot` is part of the key, for the
//     reason [`gvn`] states: two copies of an unrolled body are different
//     program points, and merging across them gives one copy's code the
//     other's deopt frame.
//
// AND THE CHAIN IS REPAIRED, NOT BROKEN. `B` sits in the memory chain, so
// `replace_all_uses(B, A)` alone would be wrong in the quiet direction: a
// store that named `B` as its token would be re-pointed at `A`, losing its
// ordering against every read BETWEEN them — reads that are still there. The
// two kinds of edge are separated: a *token* user of `B` is spliced to `B`'s
// own incoming token (the chain closes over the hole, exactly as
// `kill_store_splicing_memory_chain` does for a store), and only then do the
// remaining *value* users, and the safepoint slots, follow the value to `A`.

/// True when this op reads memory and writes none — the only thing allowed to
/// sit between two loads this pass merges.
///
/// Reads the one table (`ir::Op::memory_shape`) rather than listing ops, so a
/// new memory op is classified by the same authority `memory_token_slot` and
/// `effect_of_node` read. An op with no shape at all (a φ, a `Return`, any pure
/// node) answers `false`: it is not on a memory chain, and reaching one means
/// the walk has left the territory this pass reasons about.
pub(in crate::ir_optimize) fn is_pure_memory_read(op: &Op) -> bool {
    matches!(
        op.memory_shape().map(|s| s.access),
        Some(crate::ir::MemAccess::FieldRead)
            | Some(crate::ir::MemAccess::ArrayRead)
            | Some(crate::ir::MemAccess::LengthRead)
    )
}

/// How many links of memory chain the walk will follow before giving up.
///
/// A bound, not a tuning knob: the chain between two redundant reads is
/// normally 0-4 links (the `sumWide` case is 1-3), and an unbounded walk on a
/// graph whose chain has been left cyclic by a rewrite elsewhere would hang the
/// compiler rather than miscompile. 64 is far past any real distance.
pub(in crate::ir_optimize) const LOAD_CSE_CHAIN_BUDGET: usize = 64;

/// Is memory state `to` reachable backwards from memory state `from` through
/// nothing that can have written the cell `reads`?
///
/// `from == to` is the zero-length case and is `true`.
///
/// # What may sit on the path
///
/// By default only a pure read — `MemAccess::{FieldRead, ArrayRead,
/// LengthRead}`, which advances the memory token and writes nothing. That is
/// the whole rule, and it is why one `Op::Store` stops the merge however
/// obviously it writes somewhere else.
///
/// `CRATONVM_JIT_IR_LOAD_CSE_ALIAS=1` replaces it with the question the alias
/// model already answers: does this node WRITE anything that may alias the cell
/// being read? `ir::effect_of_node` classifies the node and `Graph::may_alias`
/// decides, so `this.a; this.b = x; this.a` merges — two different field
/// indices are two different cells whatever the bases are, because two
/// different objects have disjoint storage and one object cannot have one cell
/// at two indices.
///
/// The read rule is not deleted but subsumed: a read's `writes` class is
/// `AliasClass::None`, which aliases nothing.
///
/// Three things still stop the walk even when nothing aliases, and each is a
/// separate claim:
///
/// * a **safepoint**, because a relocating collector may run there. `MemEffect`
///   says a load may cross one freely, and that is about MOVING a load; this
///   pass DELETES one and hands an older value to a later program point, which
///   is a different claim and not one to make on the strength of a comment
///   about the other.
/// * an **allocation**, for the reason `MemEffect::allocates` gives: which
///   allocation runs first is observable, and it is also a safepoint.
/// * any **ordering stronger than `MemOrder::Plain`**. A volatile access is a
///   fence; re-using a value read before one is exactly what a fence forbids.
pub(in crate::ir_optimize) fn memory_chain_reaches(
    graph: &Graph,
    from: NodeId,
    to: NodeId,
    ctrl: NodeId,
    reads: crate::ir::AliasClass,
    cross_block: bool,
) -> bool {
    // Across blocks (`load_cse_dominance_enabled`) only a pure read may sit on
    // the path, whatever block it is anchored in: a read writes nothing, so its
    // placement cannot matter. The alias-aware widening stays within a block.
    let alias_aware = load_cse_alias_enabled() && !cross_block;
    let mut cur = from;
    for _ in 0..LOAD_CSE_CHAIN_BUDGET {
        if cur == to {
            return true;
        }
        let Some(node) = graph.node_opt(cur) else {
            return false;
        };
        if cross_block {
            if !is_pure_memory_read(&node.op) {
                return false;
            }
        } else if node.input_opt(0) != Some(ctrl) {
            return false;
        }
        // A VOLATILE access on the path is an acquire (a read) or a full fence
        // (a write): re-using a value read before it is exactly what the
        // fence forbids. Checked before the op-level test, which cannot see
        // the mark and would wave a volatile READ through as a pure one
        // (round 9 wave 6).
        if node.is_volatile_access() {
            return false;
        }
        if !is_pure_memory_read(&node.op) {
            if !alias_aware {
                return false;
            }
            // `Graph::memory_effect`, not the bare `effect_of_node`: the
            // graph-aware form folds an offset operand that names an
            // `Op::Const` into `AccessOffset::Const`, and a pair of unequal
            // constants is the ONLY thing that ever proves two accesses to the
            // same base disjoint. With the unfolded form every field index
            // stays `Dynamic` and nothing is ever provably distinct — which is
            // exactly how the first version of this passed its negative test
            // and failed its positive one.
            let eff = graph.memory_effect(cur);
            // Round nio20 wave 4 (lane L1, W3L4-1): a safepoint that is only
            // a deopt (the scoped fold's inline checks) is not the "relocating
            // collector may run here" case above. Either the deopt is taken
            // and no later compiled code runs, or it is not and nothing ran:
            // no call, no poll, no allocation. Its write is still judged by
            // the alias test below (raw memory, never a field).
            if (eff.safepoint && !graph.safepoint_is_deopt_only(cur))
                || eff.allocates
                || eff.order != crate::ir::MemOrder::Plain
                || graph.may_alias(eff.writes, reads)
            {
                return false;
            }
        }
        let Some(slot) = crate::ir::memory_token_slot(node) else {
            return false;
        };
        let Some(next) = node.input_opt(slot) else {
            return false;
        };
        cur = next;
    }
    false
}

/// The GVN key of a memory read, with the memory token left out.
///
/// Hashes `(op, ty, frame_snapshot, inputs-except-the-token)`. The token slot
/// is the one edge this pass reasons about separately; everything else is
/// identity-compared by [`redundant_load_matches`] after the hash hit, exactly
/// as `gvn` does.
pub(in crate::ir_optimize) fn load_cse_hash(node: &Node, token_slot: usize) -> u64 {
    let mut hasher = FxHasher::default();
    node.op.hash(&mut hasher);
    node.ty.hash(&mut hasher);
    node.frame_snapshot.hash(&mut hasher);
    for (i, input) in node.inputs.as_slice().iter().enumerate() {
        // Slot 0 is the control anchor, which the caller relates on its own
        // terms (same block, or a dominating one): leaving it in the key would
        // put two reads in different blocks in different buckets.
        if i == token_slot || i == 0 {
            continue;
        }
        input.hash(&mut hasher);
    }
    hasher.finish()
}

/// Do these two reads address the same cell at the same program point? (The
/// hash-collision check, and the real predicate.)
pub(in crate::ir_optimize) fn redundant_load_matches(
    a: &Node,
    b: &Node,
    token_slot: usize,
) -> bool {
    a.op == b.op
        && a.ty == b.ty
        && a.frame_snapshot == b.frame_snapshot
        && a.inputs.len() == b.inputs.len()
        && a.inputs
            .as_slice()
            .iter()
            .zip(b.inputs.as_slice().iter())
            .enumerate()
            .all(|(i, (x, y))| i == token_slot || i == 0 || x == y)
}

/// Eliminate reads whose value a read already performed in the same block
/// holds.
///
/// Returns `true` when it removed at least one. See the section comment above
/// for the soundness argument; `CRATONVM_JIT_IR_LOAD_CSE` is the switch.
pub(in crate::ir_optimize) fn eliminate_redundant_loads(graph: &mut Graph) -> bool {
    if !load_cse_enabled() {
        return false;
    }
    redundant_loads_pass(graph, false)
}

/// Round 12 wave 7 (lane iropt6, the optimizing tier's half of X2): after a
/// LICM round, merge a read into an earlier read of the same address anchored
/// on the SAME control node, when only plain reads lie between their memory
/// states -- even with `CRATONVM_JIT_IR_LOAD_CSE` off.
///
/// [`run_licm_pass`] always meant to do this: its comment says the hoist lands
/// every copy of an invariant read on one anchor and one memory state, "the
/// zero-length case" of [`eliminate_redundant_loads`], and calls that pass --
/// which returns at once unless `CRATONVM_JIT_IR_LOAD_CSE=1`. So on the
/// default path `for (i = 0; i < data.length; i++) s += data[i]` hoisted two
/// reads of `this.data`, one for the loop test and one for the element, and
/// bounds-check elimination, which relates an access to a length by NODE
/// (`ir_check_elim::LenKey::LengthOf`), could not see that the tested length
/// is the accessed array's: the element kept its check. (The two hoisted
/// reads need not share a token: the builder advances the token on every
/// read, so the element read's token is whichever read preceded it, and LICM
/// keeps a token that is already invariant.)
///
/// The full pass's most conservative form, and nothing else: one block (no
/// dominance), and the chain walk's pure-read rule even when
/// `CRATONVM_JIT_IR_LOAD_CSE_ALIAS=1` is set. So no store, call, allocation,
/// monitor, volatile access or safepoint lies between the two reads on the
/// memory chain, and neither can run without the other. Merging two plain
/// reads of one field into one is a legal execution under the JMM (the second
/// may always see what the first saw); volatile reads are never merged, and a
/// graph holding a token-less memory op is refused, both exactly as in the full
/// pass. Note that the merge is what makes the bounds proof sound: two
/// SEPARATE reads of `this.data` may see two different arrays under a race,
/// and relating them without merging would elide a check the second one needs.
pub(in crate::ir_optimize) fn eliminate_hoisted_duplicate_loads(graph: &mut Graph) -> bool {
    if load_cse_enabled() {
        return eliminate_redundant_loads(graph);
    }
    redundant_loads_pass(graph, true)
}
