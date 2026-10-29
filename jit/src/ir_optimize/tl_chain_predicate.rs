//! TLGET21-3 (`CRATONVM_JIT_IR_THREAD_LOCAL_LOOP_PREDICATE`, perf, **default
//! ON**, `=0` kill; page
//! `jr21-tl3-threadlocal-get-expansion-still-8x-hotspot-FIXED-20261014.md`): a
//! trap-form `ThreadLocal.get` expansion inside a loop that writes nothing
//! moves, whole, to the loop's pre-header.
//!
//! # What
//!
//! `for (...) s += TL.get()[0] + i;` builds, per iteration, the whole
//! `ThreadLocalMap.getEntry` hit path: the receiver's class test, the
//! current thread, `threadLocals`, `table`, its length, the hash, up to three
//! probed entries with their class and referent tests, and `value`. In the
//! trap form (TL321-1) every miss is a `Guard` at the invoke, so the chain is
//! straight reads and guards; but LICM moves no guard, and every read sits
//! behind one, so all of it stayed in the loop (~12 ns a get against
//! HotSpot's ~1.5, whose C2 predicates the same chain out of its loop).
//!
//! The builder records each trap-form get ([`crate::ir::TlChainRecord`]).
//! For a loop that contains the chain on every iteration, writes no memory
//! and makes no call, and whose receiver is defined outside it, this pass
//! re-anchors the chain on the loop's entry edge: the chain's first control
//! and memory become the pre-header's, the header's entry edge becomes the
//! chain's hit, and the body continues from where the chain began. Every
//! guard of the chain becomes a loop-entry predicate (`bci` = the header's,
//! the loop-entry frame), and so do its array reads (which name that frame).
//!
//! # Why it is sound
//!
//! * The chain reads `recv.threadLocalHashCode` (final), the current thread
//!   (constant for a loop that makes no call: nothing can unmount it),
//!   `Thread.threadLocals`, `ThreadLocalMap.table`, its elements, their
//!   referents and `Entry.value`. Only Java code of this thread writes the
//!   middle four (`ThreadLocalMap` is thread-confined), and only through a
//!   call or a store; the body has neither (`loop_writes_memory`). A referent
//!   equal to `recv` cannot be cleared while `recv` -- defined outside the
//!   loop -- is live. So every iteration's chain reads what the pre-header
//!   chain read, and passes or fails exactly as it does.
//! * A failing guard deopts to the loop's entry with the loop-entry frame
//!   (every header phi at its entry value; for a spliced loop the inlined
//!   chain at the header, every caller at its invoke): nothing of the loop has
//!   run, and the chain wrote nothing, so the interpreter runs the loop
//!   itself and its `get()` runs the JDK body (`initialValue`, the probe
//!   chain, expunge). The chain was on every iteration's path
//!   (`anchored_on_every_iteration`), so a loop that would not have called
//!   `get()` at all is a zero-trip loop, which the deopt also runs exactly.
//! * Never loops: the guards are charged under this planter's predicate id
//!   at the header (`PredicateKind::ThreadLocal`; a spliced loop's at its
//!   call site under the chain id), and past the per-bci limit the next
//!   compile leaves the chain in the loop (its own trap-form rules then
//!   apply).
//! * An OSR entry at the header would skip the chain whose value the body
//!   reads: the guards sit at the header's bci in a dominating block, which
//!   `ir_lower`'s `CRATONVM_JIT_IR_OSR_PREDICATE_REFUSAL` refuses an entry
//!   past, and an OSR graph is served only for a loop every entry reaches
//!   through its pre-header (the null-base predicate's rule).

use super::{
    anchored_on_every_iteration, build_users, chain_predicate_guard_frame, header_is_spliced,
    is_loop_invariant, loop_body_with, loop_entered_only_through_its_pre_header,
    loop_entry_chain_frame, loop_entry_frame, loop_entry_memory, loop_headers, loop_writes_memory,
    preheader_traps_block_entry_predicate, spliced_loop_entered_only_through_its_pre_header,
};
use crate::ir::{Graph, NodeId, Op, TlChainRecord, NO_NODE};
use rustc_hash::FxHashSet;

/// `CRATONVM_JIT_IR_THREAD_LOCAL_LOOP_PREDICATE` (TLGET21-3) -- **default
/// ON**, `=0` is the kill switch. Read per compile; no `static`.
pub(super) fn tl_loop_predicate_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_THREAD_LOCAL_LOOP_PREDICATE")
}

/// The chain's live nodes inside `body`, or `None` when the record no longer
/// describes a self-contained chain there (a later pass merged, moved or
/// copied part of it). See [`plant_tl_chain_predicates`].
fn chain_nodes(
    graph: &Graph,
    c: &TlChainRecord,
    region: NodeId,
    body: &FxHashSet<NodeId>,
    recv_moves: bool,
) -> Option<FxHashSet<NodeId>> {
    if c.first >= c.end || c.end as usize > graph.nodes.len() {
        return None;
    }
    // A `get`'s value read, or a `set`'s store (which stays in the loop).
    let is_set = c.store != NO_NODE;
    let last = if is_set { c.store } else { c.value };
    let v = graph.node_opt(last)?;
    let shape_ok = if is_set {
        v.op == Op::Store(crate::ir::MemKind::Ref) && last + 1 == c.end
    } else {
        matches!(v.op, Op::Load(_))
    };
    if !shape_ok || v.bytecode_pc != Some(c.pc) || !body.contains(&last) {
        return None;
    }
    // Every live node the expansion added, the floating ones (compares,
    // class tests, the index arithmetic) included: `body` holds only the
    // loop's pinned nodes. Not a constant, which other code may share and
    // which needs no move.
    let set: FxHashSet<NodeId> = (c.first..c.end)
        .filter(|&id| {
            id != c.store
                && graph
                    .node_opt(id)
                    .is_some_and(|n| !matches!(n.op, Op::Dead | Op::Const(_)))
        })
        .collect();
    // The hit leaves at a control of the chain (the probe walk's join), or,
    // with no walk, at the entry control itself (every test a guard on it).
    let hit_ok =
        (set.contains(&c.hit_ctrl) && body.contains(&c.hit_ctrl)) || c.hit_ctrl == c.entry_ctrl;
    if (!is_set && !set.contains(&c.value))
        || !hit_ok
        || !body.contains(&c.entry_ctrl)
        || set.contains(&c.entry_ctrl)
    {
        return None;
    }
    // Every pinned node of the chain is in this loop (none was copied out
    // or left behind by an earlier pass).
    for &id in &set {
        let n = graph.node_opt(id)?;
        let pinned = n.op.is_control()
            || matches!(
                n.op,
                Op::Guard { .. } | Op::Load(_) | Op::ArrayLoad(_) | Op::ArrayLength
            )
            || matches!(n.op, Op::CurrentThreadMirror { .. });
        if pinned && !body.contains(&id) {
            return None;
        }
    }
    // Inputs: the chain's own nodes, its entry control and memory, or values
    // defined outside the loop that do not vary in it (the receiver, the
    // constants, anything LICM already took out).
    for &id in &set {
        let n = graph.node_opt(id)?;
        if let Op::Guard { bci } = n.op {
            if bci != c.pc {
                return None;
            }
        }
        for &i in n.inputs.iter() {
            if i == NO_NODE
                || set.contains(&i)
                || i == c.entry_ctrl
                || i == c.entry_mem
                || (recv_moves && i == c.recv)
            {
                continue;
            }
            if body.contains(&i) || !is_loop_invariant(graph, i, region, body) {
                return None;
            }
        }
    }
    Some(set)
}

/// Is `recv` a read of a `static final` reference field the class
/// initialised (`Graph::static_final_non_null`): one value for the whole
/// loop, never null, a read that cannot trap or write? Such a receiver
/// (`static final ThreadLocal<..> TL`; `TL.get()` in the loop) moves to the
/// pre-header with the chain.
fn static_final_receiver(graph: &Graph, recv: NodeId) -> bool {
    graph.node_opt(recv).is_some_and(|n| {
        matches!(
            n.op,
            Op::LoadStatic { class_id, field_index, type_tag: b'L', is_volatile: false }
                if graph.static_final_non_null.contains(&(class_id, field_index))
        ) && n.inputs.len() == 2
    })
}

/// Is `id` a non-volatile read of a `static final` reference the VM filed
/// for a class it had initialised: an object (`Graph::static_final_non_null`)
/// or an array (`Graph::trusted_static_array_lengths`, filed only for a field
/// whose class `resolve_static_base` answers, so initialised)? Such a read
/// runs no `<clinit>` and writes nothing.
fn filed_static_final_read(graph: &Graph, id: NodeId) -> bool {
    graph.node_opt(id).is_some_and(|n| match n.op {
        Op::LoadStatic {
            class_id,
            field_index,
            type_tag: b'L',
            is_volatile: false,
        } => graph
            .static_final_non_null
            .contains(&(class_id, field_index)),
        Op::LoadStatic {
            class_id,
            field_index,
            type_tag: b'[',
            is_volatile: false,
        } => graph
            .trusted_static_array_lengths
            .iter()
            .any(|&(key, _)| key == (class_id, field_index)),
        _ => false,
    })
}

/// Does anything outside the chain use a chain node other than the value
/// and the hit control (a `set`'s store aside, which reads the found entry
/// and the chain's memory)? A frame snapshot naming a chain temporary counts
/// too: the move keeps only the value's uses (and the hit's) as they were.
fn chain_leaks(graph: &Graph, c: &TlChainRecord, set: &FxHashSet<NodeId>) -> bool {
    for (u, n) in graph.nodes.iter().enumerate() {
        // Cast: arena index to node id.
        let u = u as NodeId;
        if set.contains(&u) || n.op == Op::Dead || (c.store != NO_NODE && u == c.store) {
            continue;
        }
        for &i in n.inputs.iter() {
            if set.contains(&i) && i != c.value && i != c.hit_ctrl {
                return true;
            }
        }
    }
    graph.safepoints.iter().any(|sp| {
        sp.locals
            .iter()
            .chain(sp.stack.iter())
            .chain(sp.monitors.iter())
            .any(|&v| v != c.value && set.contains(&v))
    })
}

/// Move every recorded `ThreadLocal.get` chain it may (module notes) to its
/// loop's pre-header, unless `despec` withdrew the predicate at that loop.
/// Returns how many chains moved.
pub(super) fn plant_tl_chain_predicates(
    graph: &mut Graph,
    despec: &crate::deopt::DespecRegistry,
    method_key: &str,
    osr_route: bool,
) -> usize {
    if graph.tl_chains.is_empty() || graph.safepoints.is_empty() {
        return 0;
    }
    let dbg = super::loop_pass_report_enabled();
    let osr_graph = osr_route || graph.osr_compile;
    let chains = graph.tl_chains.clone();
    if dbg {
        eprintln!(
            "[ir-licm] ThreadLocal loop predicate: {} recorded chain(s) in {method_key}",
            chains.len()
        );
    }
    let mut moved = 0usize;
    // One chain per loop and round: the headers (and bodies) change with
    // every move, so the next chain of the same loop waits for the next
    // `loop_headers` walk.
    let mut done: Vec<usize> = Vec::new();
    loop {
        let mut progressed = false;
        for (ci, c) in chains.iter().enumerate() {
            if done.contains(&ci) {
                continue;
            }
            match plant_one(graph, c, despec, method_key, osr_graph, dbg) {
                Some(true) => {
                    done.push(ci);
                    moved += 1;
                    progressed = true;
                    break;
                }
                Some(false) => continue,
                None => done.push(ci),
            }
        }
        if !progressed {
            break;
        }
    }
    moved
}

/// One chain: `Some(true)` moved, `None` never will this compile,
/// `Some(false)` not in any loop it may leave.
fn plant_one(
    graph: &mut Graph,
    c: &TlChainRecord,
    despec: &crate::deopt::DespecRegistry,
    method_key: &str,
    osr_graph: bool,
    dbg: bool,
) -> Option<bool> {
    let refuse = |why: &str| {
        if dbg {
            eprintln!(
                "[ir-licm] ThreadLocal loop predicate: chain at pc {} (value n{}) stays -- {why}",
                c.pc, c.value
            );
        }
    };
    let users = build_users(graph);
    // The innermost loop whose body holds the chain's entry.
    let mut best: Option<(NodeId, NodeId, FxHashSet<NodeId>)> = None;
    for (region, entry_pred, back_ctrls) in loop_headers(graph) {
        if entry_pred == NO_NODE
            || !matches!(
                graph.node_opt(region).map(|n| &n.op),
                Some(Op::Region | Op::Merge)
            )
        {
            continue;
        }
        let body = loop_body_with(graph, &users, region, entry_pred, &back_ctrls);
        if !body.contains(&c.entry_ctrl) || body.contains(&entry_pred) {
            continue;
        }
        if best.as_ref().is_none_or(|(_, _, b)| body.len() < b.len()) {
            best = Some((region, entry_pred, body));
        }
    }
    let Some((region, entry_pred, body)) = best else {
        refuse("no loop holds its entry");
        return None;
    };
    // A `static final` receiver read inside the loop moves with the chain.
    let recv_moves = body.contains(&c.recv) && static_final_receiver(graph, c.recv);
    let Some(set) = chain_nodes(graph, c, region, &body, recv_moves) else {
        refuse("the record no longer names a self-contained chain");
        return None;
    };
    if !recv_moves && (body.contains(&c.recv) || !is_loop_invariant(graph, c.recv, region, &body)) {
        refuse("the receiver varies in the loop");
        return Some(false);
    }
    // The receiver's own read writes nothing (a direct static read of an
    // initialised class), and a `set`'s own store writes `Entry.value` alone,
    // which no read of the lookup reads (the thread's `threadLocals`, the
    // map's `table`, its elements, their referents, the key's hash: other
    // fields of exactly-tested classes, and array slots); the rest of the
    // body is asked as it is.
    let writes = {
        let mut rest = body.clone();
        if recv_moves {
            rest.remove(&c.recv);
        }
        if c.store != NO_NODE {
            rest.remove(&c.store);
        }
        // Nor does any other read of a `static final` the VM filed non-null
        // (`ThreadLocal.set(VALUE)`'s value, an array-typed one included, which
        // the builder leaves on the memory chain): its class is initialised
        // and the field never changes, so the read reaches no `<clinit>`.
        rest.retain(|&id| !filed_static_final_read(graph, id));
        if loop_writes_memory(graph, &rest) {
            // The first writer, for the refusal line.
            Some(
                rest.iter()
                    .copied()
                    .find(|&id| {
                        let one: FxHashSet<NodeId> = std::iter::once(id).collect();
                        loop_writes_memory(graph, &one)
                    })
                    .unwrap_or(NO_NODE),
            )
        } else {
            None
        }
    };
    if let Some(w) = writes {
        refuse(&format!(
            "the loop writes memory or calls (n{w} {:?} at pc {:?})",
            graph.node_opt(w).map(|n| &n.op),
            graph.node_opt(w).and_then(|n| n.bytecode_pc),
        ));
        return None;
    }
    if !anchored_on_every_iteration(graph, c.entry_ctrl, region) {
        refuse("not on every iteration's path");
        return None;
    }
    if chain_leaks(graph, c, &set) {
        refuse("a chain temporary is used outside it");
        return None;
    }
    let spliced = header_is_spliced(graph, region);
    let entered = if spliced {
        spliced_loop_entered_only_through_its_pre_header(graph, region, &body)
    } else {
        loop_entered_only_through_its_pre_header(graph, region, entry_pred, &body)
    };
    if osr_graph && !entered {
        refuse("an OSR entry may bypass the pre-header");
        return None;
    }
    let Some(header_bci) = graph.node_opt(region).and_then(|r| r.bytecode_pc) else {
        return None;
    };
    if preheader_traps_block_entry_predicate(graph, entry_pred, header_bci) {
        refuse("the pre-header may trap");
        return None;
    }
    let Some(entry_slot) = graph
        .node_opt(region)
        .and_then(|r| r.inputs.iter().position(|&p| p == entry_pred))
    else {
        return None;
    };
    let reused = if spliced {
        chain_predicate_guard_frame(graph, entry_pred, header_bci)
    } else {
        None
    };
    let frame = match reused {
        Some(si) => graph
            .safepoints
            .get(si as usize)
            .map(|f| (header_bci, f.clone())),
        None if spliced => loop_entry_chain_frame(graph, region, entry_slot + 1, &body),
        None => loop_entry_frame(graph, region, entry_slot + 1, &body),
    };
    let Some((header_bci, frame)) = frame else {
        refuse("no loop-entry frame");
        return None;
    };
    let Ok(bci32) = u32::try_from(header_bci) else {
        return None;
    };
    let kind = crate::deopt::PredicateKind::ThreadLocal;
    let withdrawn = match frame.splice_scope.as_deref() {
        Some(chain) => match (chain.scopes.first(), chain.scopes.last()) {
            (Some(inner), Some(outer)) => crate::deopt::chain_predicate_withdrawn(
                despec,
                method_key,
                outer.bci,
                &inner.method_key,
                inner.bci,
                kind,
            ),
            _ => true,
        },
        None => crate::deopt::predicate_withdrawn(despec, method_key, bci32, kind),
    };
    if withdrawn {
        refuse("de-specialised at the loop header");
        return None;
    }
    // The memory the pre-header ends with. With no memory phi at the header
    // memory is invariant, and the chain's own entry memory is already it.
    let pre_mem = match loop_entry_memory(graph, region, entry_pred) {
        Some(m) => m,
        None if !body.contains(&c.entry_mem) => c.entry_mem,
        None => {
            refuse("no loop-entry memory");
            return None;
        }
    };

    // ── The move. Nothing above changed the graph. ──
    let si = match reused {
        Some(si) => si,
        None => {
            graph.push_safepoint(frame);
            // Cast: a snapshot index; `push_safepoint` just grew the list.
            (graph.safepoints.len() - 1) as u32
        }
    };
    // 1. The body continues from where the chain began: every outside use
    // of the hit control takes the chain's entry control (unless they are the
    // same node: a chain with no probe walk hangs on its entry control
    // alone), every outside MEMORY use of a chain node (the value read's
    // users, a `set`'s store) takes the chain's entry memory. A `set`'s store
    // stays where it was, on the moved found entry.
    let walked = c.hit_ctrl != c.entry_ctrl;
    let chain = &set;
    let outside_users: Vec<(NodeId, usize, NodeId)> = graph
        .nodes
        .iter()
        .enumerate()
        .filter(|(u, n)| {
            // Cast: arena index to node id.
            !set.contains(&(*u as NodeId)) && n.op != Op::Dead
        })
        .flat_map(|(u, n)| {
            (0..n.inputs.len()).filter_map(move |k| {
                let i = n.inputs[k];
                // Cast: arena index to node id.
                let u = u as NodeId;
                if walked && i == c.hit_ctrl {
                    Some((u, k, c.entry_ctrl))
                } else if chain.contains(&i) && crate::ir::is_memory_token_slot(n, k) {
                    Some((u, k, c.entry_mem))
                } else {
                    None
                }
            })
        })
        .collect();
    // 2. The chain starts on the loop's entry edge.
    let inside: Vec<(NodeId, usize, NodeId)> = set
        .iter()
        .flat_map(|&id| {
            let n = &graph.nodes[id as usize];
            (0..n.inputs.len()).filter_map(move |k| {
                let i = n.inputs[k];
                if i == c.entry_ctrl {
                    Some((id, k, entry_pred))
                } else if i == c.entry_mem {
                    Some((id, k, pre_mem))
                } else {
                    None
                }
            })
        })
        .collect();
    for (u, k, to) in outside_users.into_iter().chain(inside) {
        graph.set_input(u, k, to);
    }
    // The receiver read, ahead of the chain on the entry edge.
    if recv_moves {
        graph.set_input(c.recv, 0, entry_pred);
        graph.set_input(c.recv, 1, pre_mem);
    }
    // 3. The header is entered from the chain's hit (with no walk the chain
    // now hangs on the pre-header's own control, which still enters it).
    if walked {
        graph.set_input(region, entry_slot, c.hit_ctrl);
    }
    // 4. Every guard and array read of the chain is a loop-entry predicate
    // point: the header's bci, the loop-entry frame.
    let mut ids: Vec<NodeId> = set.iter().copied().collect();
    ids.sort_unstable();
    let mut guards = 0usize;
    for id in ids {
        let Some(n) = graph.node_opt_mut(id) else {
            continue;
        };
        match n.op {
            Op::Guard { .. } => {
                n.op = Op::Guard { bci: header_bci };
                n.bytecode_pc = Some(header_bci);
                guards += 1;
                if spliced {
                    graph.chain_predicate_guards.push((id, kind as u8));
                } else if crate::deopt::predicate_own_speculation_enabled() {
                    graph.predicate_guard_kinds.push((id, kind as u8));
                }
            }
            Op::ArrayLoad(_) | Op::ArrayLength => {
                n.bytecode_pc = Some(header_bci);
            }
            _ => continue,
        }
        let _ = graph.set_node_frame_snapshot(id, si);
    }
    if dbg {
        eprintln!(
            "[ir-licm] ThreadLocal loop predicate: chain at pc {} moved to the pre-header of \
             header n{region} (bci {header_bci}{}): {} nodes, {guards} guards, {} n{}",
            c.pc,
            if spliced { ", spliced" } else { "" },
            set.len(),
            if c.store != NO_NODE {
                "store kept"
            } else {
                "value"
            },
            if c.store != NO_NODE { c.store } else { c.value },
        );
    }
    Some(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deopt::DespecRegistry;
    use crate::ir::{CmpOp, IrType, MemKind, SafepointSnapshot};

    const KEY: &str = "T.f:(I)I";

    /// `for (i = 0; i < n; i++) s += TL.get() == null ? 1 : 0;` with the
    /// trap-form chain built as the builder builds it (class guard, thread,
    /// `threadLocals` null guard, `table`, length, hash, one probe with an
    /// `If` on the referent and a second probe whose miss is a guard, the
    /// join, `value`), `TL` a parameter. With `store`, the body also writes
    /// a field. Returns `(graph, region, entry pred, value)`.
    fn tl_loop(store: bool) -> (Graph, NodeId, NodeId, NodeId) {
        tl_loop_with(store, false)
    }

    /// [`tl_loop`]; with `set`, the chain ends in `e.value = TL` (a `set`'s
    /// store) instead of the value read, and the returned node is the store.
    fn tl_loop_with(store: bool, set: bool) -> (Graph, NodeId, NodeId, NodeId) {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let tl = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let n = g.add(Op::Param(1), IrType::Int, vec![start], None);
        let region = g.add(Op::Merge, IrType::Control, vec![c0], Some(10));
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let iv = g.add(Op::Phi, IrType::Int, vec![region, zero], Some(10));
        let s = g.add(Op::Phi, IrType::Int, vec![region, zero], Some(10));
        let mem = g.add(Op::Phi, IrType::Memory, vec![region, m0], Some(10));
        let cond = g.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![iv, n], Some(12));
        let iff = g.add(Op::If, IrType::Control, vec![region, cond], Some(12));
        let exit = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
        let stay = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
        // The chain at the invoke pc 16.
        let pc = 16;
        let first = g.nodes.len() as NodeId;
        let null = g.add(Op::Const(0), IrType::Ref, vec![], None);
        let guard = |g: &mut Graph, ctrl: NodeId, cond: NodeId| {
            g.add(
                Op::Guard { bci: pc },
                IrType::Void,
                vec![ctrl, cond],
                Some(pc),
            );
        };
        let cls = g.add(
            Op::ExactClassIs { class_id: 40 },
            IrType::Int,
            vec![tl],
            Some(pc),
        );
        guard(&mut g, stay, cls);
        let t = g.add(
            Op::CurrentThreadMirror { offset: 64 },
            IrType::Ref,
            vec![stay],
            Some(pc),
        );
        let t_ok = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![t, null], Some(pc));
        guard(&mut g, stay, t_ok);
        let i5 = g.add(Op::Const(5), IrType::Int, vec![], None);
        let map = g.add(
            Op::Load(MemKind::Ref),
            IrType::Ref,
            vec![stay, mem, t, i5],
            Some(pc),
        );
        let map_ok = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![map, null], Some(pc));
        guard(&mut g, stay, map_ok);
        let tab = g.add(
            Op::Load(MemKind::Ref),
            IrType::Ref,
            vec![stay, map, map, one],
            Some(pc),
        );
        let len = g.add(Op::ArrayLength, IrType::Int, vec![stay, tab, tab], Some(pc));
        let len_ok = g.add(Op::Cmp(CmpOp::Gt), IrType::Int, vec![len, zero], Some(pc));
        guard(&mut g, stay, len_ok);
        let h = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![stay, tab, tl, zero],
            Some(pc),
        );
        let mask = g.add(Op::Sub, IrType::Int, vec![len, one], Some(pc));
        let i0 = g.add(Op::And, IrType::Int, vec![h, mask], Some(pc));
        let e0 = g.add(
            Op::ArrayLoad(MemKind::Ref),
            IrType::Ref,
            vec![stay, h, tab, i0],
            Some(pc),
        );
        let e0_ok = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![e0, null], Some(pc));
        guard(&mut g, stay, e0_ok);
        let r0 = g.add(
            Op::Load(MemKind::Ref),
            IrType::Ref,
            vec![stay, e0, e0, zero],
            Some(pc),
        );
        let same0 = g.add(Op::Cmp(CmpOp::Eq), IrType::Int, vec![r0, tl], Some(pc));
        let probe = g.add(Op::If, IrType::Control, vec![stay, same0], Some(pc));
        let found = g.add(Op::Proj(0), IrType::Control, vec![probe], Some(pc));
        let other = g.add(Op::Proj(1), IrType::Control, vec![probe], Some(pc));
        let r0_live = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![r0, null], Some(pc));
        guard(&mut g, other, r0_live);
        let i1 = g.add(Op::And, IrType::Int, vec![i0, mask], Some(pc));
        let e1 = g.add(
            Op::ArrayLoad(MemKind::Ref),
            IrType::Ref,
            vec![other, r0, tab, i1],
            Some(pc),
        );
        let e1_ok = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![e1, null], Some(pc));
        guard(&mut g, other, e1_ok);
        let r1 = g.add(
            Op::Load(MemKind::Ref),
            IrType::Ref,
            vec![other, e1, e1, zero],
            Some(pc),
        );
        let same1 = g.add(Op::Cmp(CmpOp::Eq), IrType::Int, vec![r1, tl], Some(pc));
        guard(&mut g, other, same1);
        let join = g.add(Op::Merge, IrType::Control, vec![found, other], Some(pc));
        let mphi = g.add(Op::Phi, IrType::Memory, vec![join, r0, r1], Some(pc));
        let e = g.add(Op::Phi, IrType::Ref, vec![join, e0, e1], Some(pc));
        let value = if set {
            g.add(
                Op::Store(MemKind::Ref),
                IrType::Memory,
                vec![join, mphi, e, i5, tl],
                Some(pc),
            )
        } else {
            g.add(
                Op::Load(MemKind::Ref),
                IrType::Ref,
                vec![join, mphi, e, i5],
                Some(pc),
            )
        };
        let end = g.nodes.len() as NodeId;
        g.tl_chains.push(TlChainRecord {
            recv: tl,
            entry_ctrl: stay,
            entry_mem: mem,
            hit_ctrl: join,
            value: if set { NO_NODE } else { value },
            store: if set { value } else { NO_NODE },
            first,
            end,
            pc,
        });
        let isnull = if set {
            one
        } else {
            g.add(Op::Cmp(CmpOp::Eq), IrType::Int, vec![value, null], Some(19))
        };
        let sum = g.add(Op::Add, IrType::Int, vec![s, isnull], Some(20));
        let mut back_mem = value;
        if store {
            back_mem = g.add(
                Op::Store(MemKind::Int),
                IrType::Memory,
                vec![join, value, tl, zero, sum],
                Some(21),
            );
        }
        let next = g.add(Op::Add, IrType::Int, vec![iv, one], Some(22));
        g.nodes[iv as usize].inputs.push(next);
        g.nodes[s as usize].inputs.push(sum);
        g.nodes[mem as usize].inputs.push(back_mem);
        g.nodes[region as usize].inputs.push(join);
        let ret = g.add(Op::Return, IrType::Int, vec![exit, s], None);
        g.exit = ret;
        g.safepoints.push(SafepointSnapshot::new(
            10,
            vec![tl, n, iv, s],
            vec![],
            vec![],
        ));
        g.safepoints.push(SafepointSnapshot::new(
            16,
            vec![tl, n, iv, s],
            vec![tl],
            vec![],
        ));
        let after = if set { Vec::new() } else { vec![value] };
        g.safepoints.push(SafepointSnapshot::new(
            19,
            vec![tl, n, iv, s],
            after,
            vec![],
        ));
        (g, region, c0, value)
    }

    fn plant(g: &mut Graph, despec: &DespecRegistry) -> usize {
        cratonvm_types::flags::with_thread_overrides(&[], || {
            plant_tl_chain_predicates(g, despec, KEY, false)
        })
    }

    /// The chain moves: the header is entered from the chain's join, the
    /// chain starts on the old entry edge, every guard is a header predicate
    /// with the loop-entry frame (`i` and `s` at `0`), the array reads name
    /// it too, and the body's memory no longer runs through the value read.
    #[test]
    fn tlget21_3_the_chain_moves_to_the_pre_header() {
        let (mut g, region, c0, value) = tl_loop(false);
        let rec = g.tl_chains[0].clone();
        assert_eq!(plant(&mut g, &DespecRegistry::new()), 1);
        assert!(g.nodes[region as usize].inputs.contains(&rec.hit_ctrl));
        assert!(!g.nodes[region as usize].inputs.contains(&c0));
        let guards: Vec<NodeId> = (rec.first..rec.end)
            .filter(|&id| matches!(g.nodes[id as usize].op, Op::Guard { .. }))
            .collect();
        assert_eq!(guards.len(), 8);
        for &gd in &guards {
            let n = &g.nodes[gd as usize];
            assert_eq!(n.op, Op::Guard { bci: 10 });
            let si = n.frame_snapshot.expect("a named frame");
            let sp = &g.safepoints[si as usize];
            assert_eq!(sp.bci, 10);
            assert!(!sp.locals.iter().any(|&v| g.nodes[v as usize].op == Op::Phi));
            if crate::deopt::predicate_own_speculation_enabled() {
                assert!(g
                    .predicate_guard_kinds
                    .contains(&(gd, crate::deopt::PredicateKind::ThreadLocal as u8)));
            }
        }
        for id in rec.first..rec.end {
            let n = &g.nodes[id as usize];
            if matches!(n.op, Op::ArrayLoad(_) | Op::ArrayLength) {
                assert_eq!(n.bytecode_pc, Some(10));
                assert!(n.frame_snapshot.is_some());
            }
            assert!(
                !n.inputs.contains(&rec.entry_ctrl),
                "n{id} still on the body control"
            );
            assert!(
                !n.inputs.contains(&rec.entry_mem),
                "n{id} still on the body memory"
            );
        }
        // The chain's first nodes hang on the old entry edge.
        assert!(g
            .nodes
            .iter()
            .any(|n| matches!(n.op, Op::Guard { .. }) && n.inputs[0] == c0));
        // The back edge's memory was the value read (a token use): it is the
        // loop's own memory phi now; the value's VALUE use is unchanged.
        let mem_phi = &g.nodes[rec.entry_mem as usize];
        assert_eq!(mem_phi.inputs.get(2), Some(&rec.entry_mem));
        assert!(g
            .users_of(value)
            .iter()
            .any(|&u| g.nodes[u as usize].op == Op::Cmp(CmpOp::Eq)
                && g.nodes[u as usize].bytecode_pc == Some(19)));
    }

    /// A body that writes memory keeps the chain; so does a withdrawn
    /// predicate; `=0` turns the planter off.
    #[test]
    fn tlget21_3_a_writing_body_a_withdrawal_or_the_switch_keeps_the_chain() {
        let (mut g, _, _, _) = tl_loop(true);
        assert_eq!(plant(&mut g, &DespecRegistry::new()), 0);
        let (mut g, _, _, _) = tl_loop(false);
        let despec = DespecRegistry::new();
        despec.insert(
            KEY,
            10,
            crate::deopt::predicate_speculation_id(10, crate::deopt::PredicateKind::ThreadLocal),
        );
        assert_eq!(plant(&mut g, &despec), 0);
        assert!(!cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_THREAD_LOCAL_LOOP_PREDICATE", Some("0"))],
            tl_loop_predicate_enabled
        ));
    }

    /// A `set` chain's lookup moves; its store stays in the loop on the moved
    /// found entry, at the chain's entry control and memory; the guards are
    /// header predicates.
    #[test]
    fn tlget21_3_a_set_chain_moves_its_lookup_and_keeps_its_store() {
        let (mut g, region, _c0, st) = tl_loop_with(false, true);
        let rec = g.tl_chains[0].clone();
        assert_eq!(plant(&mut g, &DespecRegistry::new()), 1);
        assert!(g.nodes[region as usize].inputs.contains(&rec.hit_ctrl));
        let n = &g.nodes[st as usize];
        assert_eq!(n.op, Op::Store(MemKind::Ref));
        assert_eq!(
            n.inputs[0], rec.entry_ctrl,
            "the store stays on the body control"
        );
        assert_eq!(n.inputs[1], rec.entry_mem, "and the body memory");
        assert_eq!(n.bytecode_pc, Some(rec.pc));
        let guards = (rec.first..rec.end)
            .filter(|&id| g.nodes[id as usize].op == Op::Guard { bci: 10 })
            .count();
        assert_eq!(guards, 8);
    }

    /// A chain node another frame names (here the map read) keeps the chain
    /// in the loop.
    #[test]
    fn tlget21_3_a_leaked_temporary_keeps_the_chain() {
        let (mut g, _, _, _) = tl_loop(false);
        let rec = g.tl_chains[0].clone();
        let map = (rec.first..rec.end)
            .find(|&id| g.nodes[id as usize].op == Op::Load(MemKind::Ref))
            .expect("the map read");
        g.safepoints
            .push(SafepointSnapshot::new(18, vec![map], vec![], vec![]));
        assert_eq!(plant(&mut g, &DespecRegistry::new()), 0);
    }
}
