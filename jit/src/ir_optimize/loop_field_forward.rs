// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! OSR round (2026-10-06): forward a loop-invariant object's fields through
//! its loop (`CRATONVM_JIT_IR_LOOP_FIELD_FORWARD`).
//!
//! # Why
//!
//! An optimizing OSR body entered at a for-each's OWN header takes the
//! iterator from the interpreter frame: the object predates the compiled code,
//! so escape analysis cannot delete it, and nothing proves no other reference
//! to it exists (an `iterator()` override may have kept one). Every
//! `hasNext()`/`next()` then read `cursor`, `this$0` and `expectedModCount`
//! from the heap, and the list's `size`, `modCount` and `elementData` through
//! `this$0`, each read paying the per-object layout test and, for a reference,
//! the heap-containment check (`R16ArraylistSplit iter-foreach 5`: 6 ns per
//! element, 3 in the method-entry body that deletes the iterator).
//!
//! # What
//!
//! For a loop whose header has one forward predecessor, and a reference
//! defined OUTSIDE the loop (so the same object on every iteration), a field
//! of that object no other write in the loop can reach is read once, in the
//! pre-header, and every read of it inside the loop takes the value the last
//! store of the loop wrote, or the pre-header's read: a value φ at every
//! memory φ the chain crosses, the loop header's included. The stores stay,
//! so memory holds exactly what it held before at every program point, and a
//! deopt, a call-free exit or another thread sees nothing new.
//!
//! Run on OSR-door compiles only (`ir_optimize::optimize_passes`): their
//! entries all arrive through an opaque arm, whose merge is the pre-header.
//!
//! # Why it is sound
//!
//! * Field identity: a `getfield`/`putfield` names a field by its declaring
//!   class and index (`Graph::field_decl_class`), and two accesses with
//!   different identities never touch the same cell, whatever their bases.
//!   Every write in the loop with the forwarded field's identity must be a
//!   store through the SAME base node; one with no identity at that index
//!   refuses the field. A write the alias model cannot place (`Any`: a call, a
//!   monitor) refuses the loop -- except a `CharSequence.charAt` call the
//!   lowering emits as the String prefix with a deopt on every miss
//!   (`charat_trap`, nio21 wave 5): it runs no Java and writes nothing.
//! * nio21 wave 18 (lane L3, `CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_RELOAD`): an
//!   `Any` writer of the compiling method's own code (a call, a deferred
//!   `new`, a bridged indy) with no exceptional continuation in the body no
//!   longer refuses the loop when [`post_writer_frame`] finds its successor's
//!   frame. Every forwarded field is READ AGAIN right after it (a `Load`
//!   whose memory input is the writer, threaded before every memory user the
//!   writer had, at the successor's bci with that frame), so no value read
//!   before the writer is used after it on any path: a call is exactly the
//!   point where the forwarding's facts die. The writer may synchronise
//!   inside (a lock, a volatile read): every read the loop answers is still
//!   the value of the last access to the cell on its own path with no call
//!   and no ordering node in between, which is the plain-read argument below.
//! * No ordering: any node of the loop with acquire or release semantics, or
//!   a volatile access, refuses the loop. With no synchronisation action in
//!   the loop, no write by another thread is ordered before one of these
//!   reads, so reading the value this thread last wrote (or read) is a legal
//!   JMM execution, the same argument as merging two plain reads
//!   (`eliminate_hoisted_duplicate_loads`).
//! * The pre-header read runs once per loop entry, at the header's bci, with
//!   a frame of its own: the header's snapshot with every header φ replaced by
//!   its forward input, the state the loop is entered with. If it deopts (a
//!   null base, a layout guard), the interpreter resumes at the header and
//!   runs the loop itself, which is what it would have done.
//! * A base whose every origin is an allocation of this compile is left to
//!   escape analysis, which runs later and deletes such an object outright.

use super::{loop_body, loop_headers, FxHashMap, FxHashSet};
use crate::ir::{
    AliasClass, Graph, IrType, MemKind, MemOrder, NodeId, Op, SafepointSnapshot, NO_NODE,
};

/// `CRATONVM_JIT_IR_LOOP_FIELD_FORWARD` (OSR round, 2026-10-06; **default
/// ON**, `=0` is the kill switch). Read without a `static`
/// (`process_global_statics_ratchet`); asked once per optimize.
pub(super) fn loop_field_forward_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_PREDICATED` (nio21 wave 4, lane L5;
/// **default ON**, `=0` is the kill switch): run the forward in a
/// METHOD-ENTRY body too, for the loops `class_predicate` guarded
/// (`ir_optimize::optimize_passes`). Needs the general switch as well.
pub(super) fn predicated_forward_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_METHOD_ENTRY` (JIT r22 nio hand-ins,
/// W19L3-2 of `jit-r21-nio-proposals-w19-L3.md`; opt-in): run the forward over
/// EVERY loop of a method-entry body, as the OSR door's body does, not only the
/// class-predicated / guard-hoisted ones. The price is the one the predicated
/// rule protects: a header that gains a value φ loses its OSR entry stub
/// (`Lowerer::emit_osr_entry_stubs` withholds it), so an interpreter frame
/// looping there takes the OSR door's own compile instead. The compile cannot
/// see whether a door was ever needed for the header, hence a switch. Needs the
/// general switch as well.
pub(super) fn method_entry_forward_enabled() -> bool {
    false}

/// Rounds of [`forward_loop_fields`]: a forwarded reference field makes the
/// object it names invariant, and the next round forwards that object's
/// fields (the iterator's `this$0`, then the list's `size`).
const MAX_ROUNDS: usize = 3;

/// The most memory nodes one forwarded field's chain walk may visit.
const MAX_WALK: usize = 4096;

/// The de-spec question of [`forward_loop_fields_with`]: the VM's registry
/// and the compiling method's key.
pub(super) type ForwardDespec<'a> = Option<(&'a crate::deopt::DespecRegistry, &'a str)>;

/// [`forward_loop_fields`] with the VM's de-spec registry (JIT round 20 wave
/// 3, lane loopopt): a base that needs the pre-header null guard is not
/// forwarded at a header whose bci the registry has de-specialised, so a
/// loop entered with a null base again and again deopts at most the
/// registry's limit of times before the next compile keeps its in-loop reads
/// (`class_predicate`'s rule for its own guards). `None` (the unit tests)
/// never withholds.
pub(super) fn forward_loop_fields_with(
    graph: &mut Graph,
    only: Option<&FxHashSet<NodeId>>,
    despec: ForwardDespec<'_>,
) -> usize {
    let mut removed = 0usize;
    for _ in 0..MAX_ROUNDS {
        let n = forward_round(graph, only, despec);
        if n == 0 {
            break;
        }
        removed += n;
        // A field the loop never writes is answered by `φ(init, itself)` at
        // the header; the cleanup folds it to the pre-header read, which the
        // next round then sees as the invariant base it is.
        super::scalar_cleanup(graph);
    }
    if removed > 0 && crate::x64::dbg_jitc_enabled() {
        eprintln!("[ir] loop field forward: {removed} read(s) removed");    }
    if false {
        let (pushed, again) = FRAME_COPY_CENSUS.with(|c| {
            let c = c.borrow();
            (c.pushed, c.again)
        });
        if pushed > 0 {
            eprintln!(
                "[ir] loop field forward: writer frame copies pushed={pushed} \
                 again-for-a-copied-writer={again} (this compile so far)"
            );
        }
    }
    removed
}

/// JIT r22 nio hand-ins (W19L3-3's census): the writers whose
/// [`WriterFrame::copy`] a forward of this compile pushed, and how many pushes
/// repeated one an earlier loop or round already made. Kept only under
/// `CRATONVM_DBG_LOOP_FIELD_FORWARD`; per compile (cleared by
/// [`reset_frame_copy_census`] at `optimize_with`'s entry), per thread.
#[derive(Default)]
struct FrameCopyCensus {
    writers: FxHashSet<NodeId>,
    pushed: usize,
    again: usize,
}

thread_local! {
    static FRAME_COPY_CENSUS: std::cell::RefCell<FrameCopyCensus> =
        std::cell::RefCell::new(FrameCopyCensus::default());
}

/// Clear [`FRAME_COPY_CENSUS`] for a new compile.
pub(super) fn reset_frame_copy_census() {
    FRAME_COPY_CENSUS.with(|c| *c.borrow_mut() = FrameCopyCensus::default());
}

fn note_frame_copy_pushed(writer: NodeId) {
    if !false {
        return;
    }
    FRAME_COPY_CENSUS.with(|c| {
        let mut c = c.borrow_mut();
        c.pushed += 1;
        if !c.writers.insert(writer) {
            c.again += 1;
        }
    });
}

/// One field of one loop: reads of `base.#idx` declared by `decl`, at `kind`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct FieldKey {
    base: NodeId,
    idx: i64,
    decl: u32,
    kind: MemKind,
}

/// A full-form `Op::Load`/`Op::Store` as `(base, constant index, kind)`.
fn field_access(graph: &Graph, id: NodeId) -> Option<(NodeId, i64, MemKind)> {
    let node = graph.node_opt(id)?;
    let kind = match node.op {
        Op::Load(k) | Op::Store(k) => k,
        _ => return None,
    };
    crate::ir::memory_token_slot(node)?;
    let base = node.input_opt(2)?;
    let idx = match graph.node_opt(node.input_opt(3)?)?.op {
        Op::Const(v) => v,
        _ => return None,
    };
    Some((base, idx, kind))}

fn forward_round(
    graph: &mut Graph,
    only: Option<&FxHashSet<NodeId>>,
    despec: ForwardDespec<'_>,
) -> usize {
    // A memory op in a COMPACT layout carries no token, so a chain walk would
    // step over it: refuse the graph, as `redundant_loads_pass` does.
    if graph.nodes.iter().any(|n| {
        n.op != Op::Dead
            && n.op.memory_shape().is_some()
            && crate::ir::memory_token_slot(n).is_none()
    }) {
        return 0;
    }
    let mut removed = 0usize;
    for (region, entry_pred, back_ctrls) in loop_headers(graph) {
        if only.is_some_and(|set| !set.contains(&region)) {
            continue;
        }
        // j27 (lane FRAMES): the pre-header / post-writer frame copies this
        // loop's forward pushed and left unnamed (a refusal after the push,
        // a walk that unthreaded its reloads), and the copies whose in-loop
        // namers it replaced, are collected here -- else they sit behind the
        // header's own snapshot as a second unclaimed frame at its bci
        // (`super::orphan_frames`). Every node this forward makes at a copy's
        // bci is stamped with that copy or killed, so `pushed_since` hides no
        // half-finished copy.
        removed += super::orphan_frames::collecting_orphaned_frames(graph, |graph| {
            forward_loop(graph, region, entry_pred, &back_ctrls, despec)        });    }
    removed
}

fn forward_loop(
    graph: &mut Graph,
    region: NodeId,
    entry_pred: NodeId,
    back_ctrls: &[NodeId],
    despec: ForwardDespec<'_>,
) -> usize {
    let Some(header_bci) = graph.node_opt(region).and_then(|n| n.bytecode_pc) else {
        return 0;
    };
    // The forward input's slot in the region (and so, + 1, in each φ).
    let Some(fwd) = graph
        .node_opt(region)
        .and_then(|n| n.inputs.iter().position(|&c| c == entry_pred))
    else {
        return 0;
    };
    let mem_phis: Vec<NodeId> = graph
        .users_of(region)
        .into_iter()
        .filter(|&u| {
            graph.node_opt(u).is_some_and(|n| {
                n.op == Op::Phi && n.ty == IrType::Memory && n.input_opt(0) == Some(region)
            })
        })
        .collect();
    let [header_mem] = mem_phis.as_slice() else {
        return refuse(header_bci, "not exactly one header memory phi", region);
    };
    let header_mem = *header_mem;
    let body = loop_body(graph, region, entry_pred, back_ctrls);
    let body_nodes: Vec<NodeId> = body.iter().copied().collect();

    // Census of the loop's memory operations.
    let mut loads: FxHashMap<FieldKey, Vec<NodeId>> = FxHashMap::default();
    let mut stores: Vec<(NodeId, NodeId, i64, MemKind, Option<u32>)> = Vec::new();
    let mut other_field_writes: Vec<AccessIdx> = Vec::new();
    let mut unidentified_reads = 0usize;
    // nio21 wave 18 (lane L3): the opaque writers the forward reads again
    // after, `writer -> its successor bci and frame` (`post_writer_frame`).
    let mut writers: FxHashMap<NodeId, WriterFrame> = FxHashMap::default();
    for &id in &body_nodes {
        let Some(node) = graph.node_opt(id) else {
            continue;
        };
        if node.op == Op::Dead {
            continue;
        }
        if node.is_volatile_access() {
            return refuse(header_bci, "a volatile access", id);
        }
        // Merges and control flow carry no effect of their own: a memory φ
        // joins its inputs' effects, which are each in the census.
        if benign_opaque(&node.op) || node.op == Op::Phi || node.op.is_control() {
            continue;
        }
        // nio21 wave 5 (lane L5): a `CharSequence.charAt` call the lowering
        // emits as the String prefix with a DEOPT on every miss
        // (`charat_trap`, the predicate both sides read): it runs no Java and
        // writes nothing, so it only reads the String and may exit. The walk
        // below steps over it through its token like any non-writer.
        if matches!(node.op, Op::Call { .. })
            && super::charat_trap::seq_charat_traps_on_miss(graph, id)
        {
            continue;
        }
        let eff = graph.memory_effect(id);
        // nio21 wave 18 (lane L3): a call (or deferred `new`, or bridged
        // indy) is no longer a refusal when the fields can be read again
        // right after it, with the frame the interpreter resumes at the
        // call's successor (`post_writer_frame`): the walk below answers
        // every read past it with that fresh read (`Walk::reload_after`).
        if eff.order == MemOrder::Plain
            && matches!(eff.writes, AliasClass::Any)
            && reload_after_writer_enabled()
        {
            match post_writer_frame(graph, id) {
                Ok(frame) => {
                    writers.insert(id, frame);
                    continue;
                }
                Err(why) => {
                    if super::loop_pass_report_enabled() {
                        eprintln!(
                            "[ir] loop field forward: header bci {header_bci} refused -- an \
                             opaque write with no reload frame ({why}): n{id} {:?} at pc {:?}",
                            node.op, node.bytecode_pc
                        );
                    }
                    return 0;
                }
            }
        }
        if eff.order != MemOrder::Plain || matches!(eff.writes, AliasClass::Any) {
            // nio21 wave 16 (lane L3): name the writer (op and pc) so a host
            // trace says WHICH call or monitor kept the loop's reads.
            if super::loop_pass_report_enabled() {
                eprintln!(
                    "[ir] loop field forward: header bci {header_bci} refused -- an ordering or \
                     opaque write: n{id} {:?} at pc {:?}",
                    node.op, node.bytecode_pc
                );
            }
            return 0;
        }
        let decl = graph.field_decl_class.get(&id).copied();
        match node.op {
            Op::Load(_) => {
                let (Some((base, idx, kind)), Some(decl)) = (field_access(graph, id), decl) else {
                    unidentified_reads += 1;
                    continue;
                };
                loads
                    .entry(FieldKey {
                        base,
                        idx,
                        decl,
                        kind,
                    })
                    .or_default()
                    .push(id);
            }
            Op::Store(_) => match field_access(graph, id) {
                Some((base, idx, kind)) => stores.push((id, base, idx, kind, decl)),
                // A store the walk cannot read: refuse the loop.
                None => return refuse(header_bci, "a store the walk cannot read", id),
            },
            _ => {
                if let AliasClass::Field { offset, .. } = eff.writes {
                    other_field_writes.push(AccessIdx::from(offset));
                }
            }
        }
    }

    // nio21 wave 18 (lane L3): a reload an earlier round placed after a
    // writer is the forward's own read, not one of the loop's: answering it
    // again would only place another reload after the same writer (and
    // another pre-header read) each round.
    if !writers.is_empty() {
        for reads in loads.values_mut() {
            reads.retain(|&ld| !is_reload_after_writer(graph, ld, &writers));
        }
        loads.retain(|_, reads| !reads.is_empty());
        // The reload pays only where some iteration runs no writer (the
        // cold-arm shape this is for). When every trip passes a call, the
        // forwarded values live across it in spill slots and each read is
        // traded for a reload: no gain, so the loop is left as it was.
        if !writer_free_cycle(graph, region, &body, back_ctrls, &writers) {
            return refuse(
                header_bci,
                "every iteration passes an opaque writer",
                region,
            );
        }
    }

    // nio21 wave 6 (lane L2): the receivers a `getfield` / `putfield` of the
    // body carrying exception edges dereferences
    // (`crate::ir::exception_edge_anchors`). No field of one is forwarded: the
    // pre-header read would decide its null check ahead of the loop, and the
    // anchor's edge into its handler would be dead. Empty unless the
    // default-OFF edge switches built one.
    let edge_anchors = crate::ir::exception_edge_anchors(graph);
    let anchor_bases: FxHashSet<NodeId> = edge_anchors
        .iter()
        .filter(|&&a| body.contains(&a))
        .filter_map(|&a| graph.node_opt(a))
        .filter(|n| matches!(n.op, Op::Load(_) | Op::Store(_)))
        .filter_map(|n| n.input_opt(2))
        .collect();

    // nio21 wave 17 (lane L4): the loop-entry exact classes `class_predicate`
    // guarded. Two bases guarded to DIFFERENT exact classes are different
    // objects, so a store to the same field identity through the other one
    // (`dst.position` beside `src.position`, both `Buffer.position`) cannot
    // write the forwarded cell.
    let entry_classes = if distinct_class_stores_enabled() {
        entry_exact_classes(graph, entry_pred, header_bci)
    } else {
        FxHashMap::default()
    };
    let provably_distinct =
        |a: NodeId, b: NodeId| match (entry_classes.get(&a), entry_classes.get(&b)) {
            (Some(&ca), Some(&cb)) => ca != cb,
            _ => false,
        };

    // The forwardable fields.
    let mut keys: Vec<FieldKey> = Vec::new();
    for key in loads.keys() {
        if body.contains(&key.base)
            || anchor_bases.contains(&key.base)
            || !base_outside_ea(graph, key.base)
            || class_tested_in(graph, key.base, &body_nodes)
        {
            continue;
        }
        let ok_stores = stores.iter().all(|&(_, base, idx, kind, decl)| {
            if idx != key.idx {
                return true;
            }
            match decl {
                Some(d) if d == key.decl => {
                    (base == key.base && kind == key.kind) || provably_distinct(base, key.base)
                }
                Some(_) => true,
                None => false,
            }
        });
        let ok_others = other_field_writes.iter().all(|w| w.differs_from(key.idx));
        if ok_stores && ok_others {
            keys.push(*key);
        }
    }
    if keys.is_empty() {
        if false {
            for key in loads.keys() {
                eprintln!(
                    "[ir] loop field forward: header bci {header_bci}: n{} #{} decl {} -- inside={} ea={} class_tested={}",
                    key.base,
                    key.idx,
                    key.decl,
                    body.contains(&key.base),
                    !base_outside_ea(graph, key.base),
                    class_tested_in(graph, key.base, &body_nodes),
                );
            }
            eprintln!(
                "[ir] loop field forward: header bci {header_bci}: {} identified read key(s), {} store(s), {} unidentified read(s)",
                loads.len(),
                stores.len(),
                unidentified_reads,
            );
        }
        return refuse(header_bci, "no forwardable field", region);
    }
    keys.sort_by_key(|k| (k.base, k.idx, k.decl));
    // JIT round 20 wave 3 (lane loopopt): a base that would need the null
    // guard below is left alone at a de-specialised header (see
    // `forward_loop_fields_with`).
    let header_despecialised = despec.is_some_and(|(reg, key)| {
        u32::try_from(header_bci).is_ok_and(|bci| reg.contains(key, bci))
    });
    // JIT round 20 wave 4 (lane loopopt; correctness, unconditional): a base
    // not KNOWN non-null at the loop entry is not forwarded. The pre-header
    // read runs on every entry while the loop may read the field only on
    // some trips (`if (use) s += h.v`), and a null base makes that read raise
    // an NPE the program never raises (`R20ReviewForwardNullBase`: HotSpot
    // prints `null-base 5000000`, every build up to w3d threw NPE at
    // `drive:22`, the wave-1 pre-header null guard notwithstanding). Known
    // non-null: an allocation / constant-pool value (`definitely_non_null`),
    // or a loop-entry exact-class guard on the base (`class_predicate`'s,
    // which fails for null and deopts). The guarded route is kept behind
    // `CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_NULL_GUARD=1` (opt-in) for an A/B
    // once its failure on that probe is explained.
    let null_guard_route = null_guard_route_enabled();
    // JIT round 21 wave 1 (lane loops, HOIST13-2,
    // `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_METHOD_ENTRY`, opt-in): the
    // compiling method's own receiver is known non-null too (the JVM enters
    // an instance method only through a null-checked call site;
    // `Graph::receiver_param` states that one fact, and SSA gives the `Param`
    // no other definition). Off, a receiver base is treated as before.
    let receiver_non_null = method_entry_promote_enabled();
    // JIT round 21 wave 1 (lane loops, `CRATONVM_JIT_IR_LOOP_PROMOTE_SPLICED`,
    // opt-in): or a base a passing null check guards before the loop entry
    // (a spliced callee's receiver: `base_null_checked_before`).
    let checked_non_null = promote_spliced_enabled();
    if header_despecialised || !null_guard_route {
        let exact = entry_exact_classes(graph, entry_pred, header_bci);
        keys.retain(|k| {
            exact.contains_key(&k.base)
                || static_final_base_non_null(graph, k.base)
                || (receiver_non_null && is_receiver_param(graph, k.base))
                || (checked_non_null && base_null_checked_before(graph, k.base, entry_pred))
                || graph
                    .node_opt(k.base)
                    .is_some_and(|n| crate::ir_check_elim::definitely_non_null(&n.op))
                || (crate::ir::ir_static_final_exact_class_enabled()
                    && super::phi_non_null_on_every_edge(graph, k.base, &mut FxHashSet::default(), 0))
        });
        if keys.is_empty() {
            return refuse(
                header_bci,
                "no base known non-null at the loop entry",
                region,
            );
        }
    }

    // The pre-header's frame: the header's own snapshot, every header φ
    // replaced by its forward input.
    let Some(snapshot) = preheader_snapshot(graph, region, fwd, header_bci, &body) else {
        return refuse(header_bci, "no pre-header frame", region);
    };

    // JIT round 20 (lane loopopt, review; correctness, unconditional): the
    // pre-header read runs on EVERY loop entry, but the loop may read the
    // field only on some trips (`if (use) s += h.v`, `if (h != null) ..`).
    // A null base does not deopt at an `Op::Load`: the helper raises the NPE
    // and the compiled code takes its exceptional exit (or a covering
    // handler), so a read the program never made would throw. Each base not
    // already known non-null (an allocation / constant-pool value, or a
    // loop-entry exact-class guard, which fails for null) gets a
    // `Guard{bci: header}(base != null)` with the pre-header frame first: a
    // null base deopts to the loop entry and the interpreter runs the loop
    // itself, raising whatever the bytecode raises where it raises it.
    let exact_at_entry = entry_exact_classes(graph, entry_pred, header_bci);
    // A base proven by a loop-entry class guard needs no null guard of its own,
    // but its pre-header read still names the class guard as its licence: with
    // only positional id order the scheduler placed the read first, on a
    // receiver of another class (a `String` where the guard names a wrapper,
    // its `value` array then dereferenced as the wrapper's field).
    let class_guard_of: FxHashMap<NodeId, NodeId> = entry_class_guards(graph, entry_pred, header_bci)
        .into_iter()
        .map(|(base, _, guard)| (base, guard))
        .collect();
    let mut null_guarded: FxHashSet<NodeId> = FxHashSet::default();
    // JIT r22 wave 2 (lane loops, LOOPS22-5): base -> its null guard, so the
    // pre-header read names the guard that licenses it as its guard TOKEN
    // (`ir::with_licensing_guard`, always -- not gated on
    // `CRATONVM_JIT_IR_GUARD_TOKEN`): the guard-before-read order is then a
    // graph edge. Positional id order alone let the scheduler place the read
    // first (`R20ReviewForwardNullBase`'s NPE under the null-guard route).
    let mut null_guard_of: FxHashMap<NodeId, NodeId> = FxHashMap::default();
    let mut null_const: Option<NodeId> = None;
    for key in &keys {
        let base = key.base;
        if null_guarded.contains(&base)
            || exact_at_entry.contains_key(&base)
            || static_final_base_non_null(graph, base)
            || (receiver_non_null && is_receiver_param(graph, base))
            || (checked_non_null && base_null_checked_before(graph, base, entry_pred))
            || graph
                .node_opt(base)
                .is_some_and(|n| crate::ir_check_elim::definitely_non_null(&n.op))
        {
            continue;
        }
        let null =
            *null_const.get_or_insert_with(|| graph.add(Op::Const(0), IrType::Ref, vec![], None));
        let cond = graph.add(
            Op::Cmp(crate::ir::CmpOp::Ne),
            IrType::Int,
            vec![base, null],
            Some(header_bci),
        );
        let guard = graph.add(
            Op::Guard { bci: header_bci },
            IrType::Void,
            vec![entry_pred, cond],
            Some(header_bci),
        );
        if !graph.set_node_frame_snapshot(guard, snapshot) {
            // Unreachable (the guard's pc is the snapshot's bci); without its
            // guard the base is not forwarded.
            graph.kill(guard);
            graph.kill(cond);
            return refuse(
                header_bci,
                "a null guard without the pre-header frame",
                base,
            );
        }
        null_guarded.insert(base);
        null_guard_of.insert(base, guard);
    }

    // The pre-header reads, threaded onto the forward memory edge.
    let Some(mut mem) = graph
        .node_opt(header_mem)
        .and_then(|n| n.input_opt(fwd + 1))
    else {
        return 0;
    };
    let mut initial: FxHashMap<FieldKey, NodeId> = FxHashMap::default();
    for key in &keys {
        let Some(&sample) = loads.get(key).and_then(|v| v.first()) else {
            continue;
        };
        let (Some(idx_node), Some(ty)) = (
            graph.node_opt(sample).and_then(|n| n.input_opt(3)),
            graph.node_opt(sample).map(|n| n.ty),
        ) else {
            continue;
        };
        let read = graph.add(
            Op::Load(key.kind),
            ty,
            crate::ir::with_licensing_guard(
                vec![entry_pred, mem, key.base, idx_node],
                null_guard_of
                    .get(&key.base)
                    .or_else(|| class_guard_of.get(&key.base))
                    .copied()
                    .unwrap_or(NO_NODE),
            ),
            Some(header_bci),
        );
        graph.field_decl_class.insert(read, key.decl);
        carry_byte_array_fact(
            graph,
            loads.get(key).map(Vec::as_slice).unwrap_or(&[]),
            read,
        );
        inherit_field_row(graph, sample, read);
        // nio21 wave 18 (lane L3): the header's bci is not this field's
        // `getfield`; its compact row (if any) is not this read's.
        graph.synthetic_field_access_pcs.insert(header_bci);
        if !graph.set_node_frame_snapshot(read, snapshot) {
            // Unreachable: the read's pc is the snapshot's bci. Undo by
            // killing the unconsumed read; nothing names it yet.
            graph.kill(read);
            continue;
        }
        mem = read;
        initial.insert(*key, read);
    }
    if initial.is_empty() {
        return 0;
    }
    graph.set_input(header_mem, fwd + 1, mem);

    // Answer every in-loop read of each forwarded field.
    let mut replace: Vec<(NodeId, NodeId)> = Vec::new();
    // nio21 wave 18 (lane L3): every reload placed after a writer, any key.
    let mut reload_nodes: FxHashSet<NodeId> = FxHashSet::default();
    // nio21 wave 19 (lane L3): the frame copy pushed for each writer whose
    // reload needs one (`WriterFrame::copy`), shared by every key's reload.
    let mut pushed_frames: FxHashMap<NodeId, u32> = FxHashMap::default();
    let mut reloads_made = 0usize;
    for key in &keys {
        let Some(&init) = initial.get(key) else {
            continue;
        };
        let Some(&sample) = loads.get(key).and_then(|v| v.first()) else {
            continue;
        };
        let mut walk = Walk {
            key: *key,
            fwd,
            header_mem,
            init,
            body: &body,
            memo: FxHashMap::default(),
            budget: MAX_WALK,
            writers: &writers,
            reload_nodes: &mut reload_nodes,
            pushed_frames: &mut pushed_frames,
            sample,
            reload_of: FxHashMap::default(),
            made: Vec::new(),
        };
        let mut answers: Vec<(NodeId, NodeId)> = Vec::new();
        let mut failed = false;
        for &ld in loads.get(key).map(Vec::as_slice).unwrap_or(&[]) {
            let Some(token) = graph
                .node_opt(ld)
                .and_then(|n| crate::ir::memory_token_slot(n).and_then(|t| n.input_opt(t)))
            else {
                failed = true;
                break;
            };
            match walk.value_at(graph, token) {
                Some(v) => answers.push((ld, v)),
                None => {
                    failed = true;
                    break;
                }
            }
        }
        if failed {
            // The pre-header read stays (an extra read, never a wrong one); the
            // value φs the walk built are dead and the cleanup reclaims them.
            // nio21 wave 18: the reloads this key placed are unthreaded.
            walk.unthread_reloads(graph);
            continue;
        }
        reloads_made += walk.made.len();
        replace.extend(answers);
    }
    if reloads_made > 0 && super::loop_pass_report_enabled() {
        eprintln!(
            "[ir] loop field forward: header bci {header_bci}: {reloads_made} read(s) placed \
             after {} opaque writer(s)",
            writers.len()
        );
    }
    if replace.is_empty() {
        return 0;
    }

    // A read may be answered by another removed read (`x.f = x.f`): resolve
    // through the map before rewiring.
    let map: FxHashMap<NodeId, NodeId> = replace.iter().copied().collect();
    let resolve = |mut v: NodeId| {
        let mut seen = 0usize;
        while let Some(&next) = map.get(&v) {
            v = next;
            seen += 1;
            if seen > map.len() {
                return NO_NODE;
            }
        }
        v
    };
    let mut removed = 0usize;
    for &(ld, v) in &replace {
        let v = resolve(v);
        if v == NO_NODE || v == ld {
            continue;
        }
        let Some(token) = graph
            .node_opt(ld)
            .and_then(|n| crate::ir::memory_token_slot(n).and_then(|t| n.input_opt(t)))
        else {
            continue;
        };
        // Token users first: they want the chain, not the value.
        for user in graph.users_of(ld) {
            let Some(node) = graph.node_opt(user) else {
                continue;
            };
            let slots: Vec<usize> = node
                .inputs
                .as_slice()
                .iter()
                .enumerate()
                .filter(|&(i, &x)| x == ld && crate::ir::is_memory_token_slot(node, i))
                .map(|(i, _)| i)
                .collect();
            for slot in slots {
                graph.set_input(user, slot, token);
            }
        }
        graph.replace_all_uses(ld, v);
        graph.kill(ld);
        removed += 1;
    }
    if removed > 0 && crate::x64::dbg_jitc_enabled() {
        eprintln!(
            "[ir] loop field forward: header bci {header_bci}: {} field(s), {removed} read(s) answered in registers",
            initial.len()
        );
    }
    removed
}

/// `CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_NULL_GUARD` (JIT round 20 wave 4, lane
/// loopopt): forward a base that is not known non-null too, behind a
/// pre-header `Guard{bci: header}(base != null)`. Off, such a base is not
/// forwarded (correctness: see `forward_loop`). Read once per loop.
///
/// JIT round 24 (the r21/r22 loops books' close): **default ON**, `=0` kills.
/// The guarded route's `R20ReviewForwardNullBase` failure (an NPE where the
/// interpreter returned) was the pre-header read hoisted above its null
/// guard: it now names the guard as its licence (`Graph::with_licensing_guard`,
/// `preheader_read`), and the probe answers HotSpot's facts in both modes.
/// `R21LoopAccum`, 7 interleaved reps, both modes: `field-box` 2054 -> 578
/// ns/op (0.28-0.30x), `field-this-long` 0.67x, `field-this-int` 0.85x, every
/// other row 0.94-0.99x, facts unchanged.
pub(super) fn null_guard_route_enabled() -> bool {
    true
}

/// JIT round 20 wave 19 (lane yBuf, MEM20-4; a missed engagement, so
/// unconditional): a read this pass places (the pre-header read, a reload
/// after a writer) reads the SAME field as `of` -- same base, field index,
/// declaring class and kind (`FieldKey`) -- so the `byte[]` fact any of those
/// carries (`Graph::byte_array_nodes`, today `ByteBuffer.hb`: a `byte[]` or
/// null) is `read`'s too. Without it a forwarded `hb` was a new node the
/// scoped fold's lowering did not know, and every access of the loop ran the
/// primitive-array screen and the element-size read again
/// (`Lowerer::lower_scoped_raw`). `CRATONVM_JIT_IR_SCOPED_BYTE_ARRAY_BASE=0`
/// still turns the lowering's use of the set off.
fn carry_byte_array_fact(graph: &mut Graph, of: &[NodeId], read: NodeId) {
    if of.iter().any(|ld| graph.byte_array_nodes.contains(ld)) {
        graph.byte_array_nodes.insert(read);
    }
}

/// `CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_DISTINCT_CLASSES` (nio21 wave 17, lane
/// L4; **default ON**, `=0` restores the same-base-only store rule): a store
/// of the forwarded field identity through ANOTHER base leaves the field
/// forwardable when both bases carry loop-entry exact-class guards naming
/// different classes ([`entry_exact_classes`]).
pub(super) fn distinct_class_stores_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_RELOAD` (nio21 wave 18, lane L3;
/// **default ON**, `=0` restores the refusal of every loop holding a call):
/// a call, deferred `new` or bridged `invokedynamic` of the compiling
/// method's own code in the loop no longer refuses the forward; every
/// forwarded field is read again right after it (`Walk::reload_after`), with
/// the frame of the call's successor ([`post_writer_frame`]). HotSpot C2's
/// memory graph does the same: the value is re-read past the call on that
/// path only and merged at the join. Read once per loop.
pub(super) fn reload_after_writer_enabled() -> bool {
    true
}

/// nio21 wave 18 (lane L3): where, and with which frame, the forward reads a
/// field again right after an opaque writer ([`post_writer_frame`]).
#[derive(Clone, Debug)]
pub(super) struct WriterFrame {
    /// The bci the reload sits at: the writer's successor (a combined pc for
    /// a writer inside a spliced body).
    pub(super) succ: usize,
    /// `None`: the builder's own flat snapshot at `succ` IS the writer's
    /// post-call frame, found by bci. `Some`: a copy to push and name from
    /// the reload (`Node::frame_snapshot`) -- the successor's frame with a
    /// join's value phi replaced by the writer's result (nio21 wave 19), or
    /// the post-call CHAIN snapshot the builder recorded for a call inside a
    /// spliced body, re-marked as an ordinary chain snapshot.
    pub(super) copy: Option<SafepointSnapshot>,
}

/// nio21 wave 18 (lane L3): may the forward read its fields again right after
/// the opaque writer `w`, and with which frame? The answer names the bci of
/// the instruction after `w` and a frame that is exactly the interpreter's
/// state once `w` returned: the frame a deopt of the reload resumes into.
///
/// Three shapes are admitted:
///
/// 1. **A call of the compiling method's own code** whose successor's flat
///    snapshot is the writer's frame with the arguments popped and the
///    result pushed: the same locals and monitors, the operand stack a prefix
///    of the writer's own (no more popped than the writer has inputs past its
///    control and memory), then the writer itself exactly when it returns a
///    value. `copy == None`.
/// 2. **The same, but the successor is a join whose stack top is a value phi
///    with the writer among its inputs** (nio21 wave 19,
///    [`join_successor_reload_enabled`]): a guarded splice's miss call meeting
///    its hit body, a box or String call prefix. Everything else must still
///    be the writer's frame; the copy is the successor's frame with that phi
///    replaced by the writer -- the frame the writer's own edge reaches, the
///    substitution the builder makes for a nested miss call (SPLICE2-5).
/// 3. **A call inside a spliced body** (nio21 wave 19,
///    [`splice_reload_enabled`]): the builder recorded the frames that call
///    returns into, at its successor, as a post-call chain snapshot
///    (`SafepointSnapshot::post_call_of`), the very frame its post-call exit
///    leaves through. The copy is that snapshot, re-marked as an ordinary
///    chain snapshot, so the lowering resolves it like any spliced trapping
///    node's (`splice_chain_snapshot_for_site` prefers the node's own named
///    snapshot at its raw pc). Exactly one such snapshot must exist.
///
/// Refused (`Err(why)`), so the loop is refused as before: anything but an
/// `Op::Call`, `Op::NewCp` or `Op::IndyBridge`; a writer with an exceptional
/// continuation in this body (`Op::CallThrew` on it, `Op::CatchEntryFlag` at
/// its bci: its throw path would leave past the reload); `succ - pc` not 3
/// or 5 (a dead pc between); any other difference between the frames.
pub(super) fn post_writer_frame(graph: &Graph, w: NodeId) -> Result<WriterFrame, &'static str> {
    let node = graph.node_opt(w).ok_or("no node")?;
    if !matches!(
        node.op,
        Op::Call { .. } | Op::NewCp { .. } | Op::IndyBridge { .. }
    ) {
        return Err("not a call, deferred new or bridged indy");
    }
    let pc = node.bytecode_pc.ok_or("no bytecode pc")?;
    let has_exception_arm = graph.nodes.iter().any(|n| match n.op {
        Op::CallThrew { .. } => n.input_opt(1) == Some(w),
        Op::CatchEntryFlag { call_bci, .. } => call_bci as usize == pc,
        _ => false,
    });
    if has_exception_arm {
        return Err("an exception edge in this body");
    }
    let flat_first = |bci: usize| -> Option<usize> {
        graph
            .safepoints
            .iter()
            .position(|s| s.bci == bci && !s.is_splice_state() && !s.post_call_only)
    };
    let Some(before) = flat_first(pc) else {
        // Shape 3: a call inside a spliced body.
        if !splice_reload_enabled() {
            return Err("no flat frame at the writer (a spliced call; the chain arm is off)");
        }
        let posts: Vec<&SafepointSnapshot> = graph
            .safepoints
            .iter()
            .filter(|s| s.post_call_of == w && s.post_call_only && s.is_splice_state())
            .collect();
        let [post] = posts.as_slice() else {
            return Err("a spliced writer without exactly one post-call chain snapshot");
        };
        if post.bci <= pc || !matches!(post.bci - pc, 3 | 5) {
            return Err("the post-call chain snapshot is not at the writer's successor");
        }
        let mut copy = (*post).clone();
        copy.post_call_of = NO_NODE;
        copy.post_call_only = false;
        return Ok(WriterFrame {
            succ: post.bci,
            copy: Some(copy),
        });
    };
    let succ = graph
        .safepoints
        .iter()
        .filter(|s| !s.is_splice_state() && !s.post_call_only && s.bci > pc)
        .map(|s| s.bci)
        .min()
        .ok_or("no frame after the writer")?;
    if !matches!(succ - pc, 3 | 5) {
        return Err("the next frame is not the writer's successor");
    }
    let after = flat_first(succ).ok_or("no frame at the successor")?;
    let (b, a) = (&graph.safepoints[before], &graph.safepoints[after]);
    if a.locals != b.locals
        || a.monitors != b.monitors
        || a.monitor_elided_mask != b.monitor_elided_mask
    {
        // `CRATONVM_JIT_IR_STATIC_FINAL_EXACT_CLASS`: the successor is a join
        // (`try { .. } catch (E e) { ..; call(); }` falling into the code
        // after the `try`), whose snapshot merges the other path's locals. The
        // frame the interpreter resumes in after THIS call is the call's own
        // one with its arguments popped and its result pushed (JVMS
        // `invoke*`: locals and monitors untouched): built from the snapshot
        // AT the call, whose stack must end in exactly the call's arguments.
        if !crate::ir::ir_static_final_exact_class_enabled() {
            return Err("the successor's locals or monitors differ");
        }
        let args: Vec<NodeId> = node.inputs.iter().skip(2).copied().collect();
        let Some(kept) = b.stack.len().checked_sub(args.len()) else {
            return Err("the successor's locals or monitors differ");
        };
        if b.stack[kept..] != args[..] || b.locals.contains(&w) || b.stack.contains(&w) {
            return Err("the successor's locals or monitors differ");
        }
        let mut stack = b.stack[..kept].to_vec();
        if node.ty != IrType::Void {
            stack.push(w);
        }
        let mut c = SafepointSnapshot::new(succ, b.locals.clone(), stack, b.monitors.clone());
        c.monitor_elided_mask = b.monitor_elided_mask;
        return Ok(WriterFrame {
            succ,
            copy: Some(c),
        });
    }
    let returns = node.ty != IrType::Void;
    // Shape 2: the top is a join's value phi the writer feeds.
    let mut joined = false;
    let kept = match (returns, a.stack.last()) {
        (true, Some(&top)) if top == w => a.stack.len() - 1,
        (true, Some(&top)) => {
            let feeds = graph.node_opt(top).is_some_and(|t| {
                t.op == Op::Phi
                    && t.ty == node.ty
                    && t.inputs.as_slice().iter().skip(1).any(|&x| x == w)
            });
            if !(feeds && join_successor_reload_enabled()) {
                return Err("the successor's stack does not hold the result");
            }
            joined = true;
            a.stack.len() - 1
        }
        (true, None) => return Err("the successor's stack does not hold the result"),
        (false, _) => a.stack.len(),
    };
    if kept > b.stack.len() || a.stack[..kept] != b.stack[..kept] {
        return Err("the successor's stack is not the writer's");
    }
    let popped = b.stack.len() - kept;
    if popped > node.inputs.len().saturating_sub(2) {
        return Err("the successor's stack pops more than the writer's arguments");
    }
    if a.stack[..kept].contains(&w) || a.locals.contains(&w) || a.monitors.contains(&w) {
        return Err("the writer's result is in its own frame");
    }
    let copy = if joined {
        let mut c =
            SafepointSnapshot::new(succ, a.locals.clone(), a.stack.clone(), a.monitors.clone());
        c.monitor_elided_mask = a.monitor_elided_mask;
        if let Some(top) = c.stack.last_mut() {
            *top = w;
        }
        Some(c)
    } else {
        None
    };
    Ok(WriterFrame { succ, copy })
}

/// `CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_RELOAD_JOIN` (nio21 wave 19, lane L3;
/// **default ON**, `=0` refuses shape 2 of [`post_writer_frame`] as wave 18
/// did). Needs `CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_RELOAD`.
pub(super) fn join_successor_reload_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_RELOAD_SPLICED` (nio21 wave 19, lane
/// L3; **default ON**, `=0` refuses shape 3 of [`post_writer_frame`] as wave
/// 18 did). Needs `CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_RELOAD`.
pub(super) fn splice_reload_enabled() -> bool {
    true
}

/// nio21 wave 18 (lane L3): is there a trip around the loop -- header to a
/// back edge along control nodes of `body` -- that enters no control node a
/// writer is pinned to? A writer pinned to the header itself runs on every
/// trip, so it answers `false`.
fn writer_free_cycle(
    graph: &Graph,
    region: NodeId,
    body: &FxHashSet<NodeId>,
    back_ctrls: &[NodeId],
    writers: &FxHashMap<NodeId, WriterFrame>,
) -> bool {
    let blocked: FxHashSet<NodeId> = writers
        .keys()
        .filter_map(|&w| graph.node_opt(w).and_then(|n| n.input_opt(0)))
        .collect();
    if blocked.contains(&region) {
        return false;
    }
    let backs: FxHashSet<NodeId> = back_ctrls.iter().copied().collect();
    let mut seen: FxHashSet<NodeId> = FxHashSet::default();
    let mut work = vec![region];
    seen.insert(region);
    while let Some(c) = work.pop() {
        if backs.contains(&c) {
            return true;
        }
        for u in graph.users_of(c) {
            if u == region || !body.contains(&u) || blocked.contains(&u) || seen.contains(&u) {
                continue;
            }
            if graph.node_opt(u).is_some_and(|n| n.op.is_control()) {
                seen.insert(u);
                work.push(u);
            }
        }
    }
    false
}

/// nio21 wave 18 (lane L3): is `ld` a reload [`Walk::reload_after`] placed
/// -- a `Load` on a writer's control whose memory input is that writer, at
/// the writer's successor bci? The reloads of several keys after one writer
/// form a chain (each threaded right after the writer), so the memory input
/// may be another such reload.
fn is_reload_after_writer(
    graph: &Graph,
    ld: NodeId,
    writers: &FxHashMap<NodeId, WriterFrame>,
) -> bool {
    let Some(n) = graph.node_opt(ld) else {
        return false;
    };
    if !matches!(n.op, Op::Load(_)) {
        return false;
    }
    let (Some(pc), Some(ctrl)) = (n.bytecode_pc, n.input_opt(0)) else {
        return false;
    };
    let token_of =
        |m: &crate::ir::Node| crate::ir::memory_token_slot(m).and_then(|t| m.input_opt(t));
    let mut cur = token_of(n);
    for _ in 0..64 {
        let Some(c) = cur else {
            return false;
        };
        if let Some(frame) = writers.get(&c) {
            return frame.succ == pc
                && graph.node_opt(c).and_then(|w| w.input_opt(0)) == Some(ctrl);
        }
        let Some(cn) = graph.node_opt(c) else {
            return false;
        };
        if !(matches!(cn.op, Op::Load(_))
            && cn.bytecode_pc == Some(pc)
            && cn.input_opt(0) == Some(ctrl))
        {
            return false;
        }
        cur = token_of(cn);
    }
    false
}

/// nio21 wave 17 (lane L4): `base -> class id` for every loop-entry class
/// guard of the loop entered through `entry_pred` -- `Guard{bci: header}`
/// at pc `header`, condition `ExactClassIs { class_id }(base)`, control
/// `entry_pred` (the shape `class_predicate::install_class_plan` plants). A
/// base guarded to two classes (the loop never runs past its entry) is left
/// out. Every one of these guards runs on each entry before the header, and a
/// guarded base is loop-invariant (`class_plan` guards only those), so on
/// every iteration two bases with different exact classes are two objects.
fn entry_exact_classes(
    graph: &Graph,
    entry_pred: NodeId,
    header_bci: usize,
) -> FxHashMap<NodeId, u32> {
    let mut out: FxHashMap<NodeId, u32> = FxHashMap::default();
    let mut conflicted: FxHashSet<NodeId> = FxHashSet::default();
    for (base, class_id, _) in entry_class_guards(graph, entry_pred, header_bci) {
        match out.get(&base) {
            Some(&c) if c != class_id => {
                conflicted.insert(base);
            }
            _ => {
                out.insert(base, class_id);
            }
        }
    }
    for b in conflicted {
        out.remove(&b);
    }
    out
}

/// `(base, class id, guard)` for every loop-entry class guard
/// [`entry_exact_classes`] reads. The guard is what a pre-header read of the
/// base must name as its licence: the class decides the field layout the read
/// assumes, and a read the scheduler placed first would run on any class.
fn entry_class_guards(
    graph: &Graph,
    entry_pred: NodeId,
    header_bci: usize,
) -> Vec<(NodeId, u32, NodeId)> {
    let mut out = Vec::new();
    for (id, n) in graph.nodes.iter().enumerate() {
        let Op::Guard { bci } = n.op else {
            continue;
        };
        if bci != header_bci
            || n.bytecode_pc != Some(header_bci)
            || n.input_opt(0) != Some(entry_pred)
        {
            continue;
        }
        let Some(cond) = n.input_opt(1).and_then(|c| graph.node_opt(c)) else {
            continue;
        };
        let Op::ExactClassIs { class_id } = cond.op else {
            continue;
        };
        let Some(base) = cond.input_opt(0).filter(|&b| b != NO_NODE) else {
            continue;
        };
        if class_id != 0 {
            out.push((base, class_id, id as NodeId));
        }
    }
    out
}

/// `CRATONVM_JIT_IR_STATIC_FINAL_EXACT_CLASS`: let `read`, a load of the same
/// field as `sample` placed at a synthetic pc, take the compact row `sample`
/// takes (its own `getfield` pc's, or the row a pass named for it), so the
/// forwarded read stays the inline compact read instead of falling to the
/// legacy-only arm and the checked helper for a compact object.
pub(super) fn inherit_field_row(graph: &mut Graph, sample: NodeId, read: NodeId) {
    if !crate::ir::ir_static_final_exact_class_enabled() {
        return;
    }
    if let Some(&row) = graph.field_rows.get(&sample) {
        graph.field_rows.insert(read, row);
        return;
    }
    if let Some(&pc) = graph.field_row_pcs.get(&sample) {
        graph.field_row_pcs.insert(read, pc);
        return;
    }
    if let Some(pc) = graph.node_opt(sample).and_then(|n| n.bytecode_pc) {
        if !graph.synthetic_field_access_pcs.contains(&pc) {
            graph.field_row_pcs.insert(read, pc);
        }
    }
}

/// `CRATONVM_DBG_LOOP_FIELD_FORWARD=1`: say why a loop was left alone.
/// nio21 wave 16 (lane L3): also under `ir-bailout` / `ir-compiles` / `licm`
/// (`ir_optimize::loop_pass_report_enabled`).
fn refuse(header_bci: usize, why: &str, node: NodeId) -> usize {
    if false
        || super::loop_pass_report_enabled()
    {
        eprintln!("[ir] loop field forward: header bci {header_bci} refused -- {why} (n{node})");
    }
    0
}

/// An op the effect model calls `Opaque` that writes no field and orders
/// nothing, in this pass's sense:
///
/// * `Integer.intValue()` / `Long.longValue()` (`Op::Unbox`): a guarded read
///   of an immutable box. The model is `Opaque` for the whole op because its
///   `Atomic*` variants write and fence; those stay refused. The scoped
///   (`ScopedMemoryAccess`) variants need no entry here: under
///   `Graph::scoped_raw_alias` (round nio20 wave 4) `Graph::memory_effect`
///   classes them as RAW memory, which is not `Any` and names no field, so the
///   census below admits them and no field walk ever stops at one.
/// * a `checkcast` whose target class the compile resolved
///   (`crate::typecheck_target_for_site`, the row its helper reads by the same
///   pointer): the helper answers from that row, loads no class and runs no
///   Java, and on a failure throws out of the loop.
pub(super) fn benign_opaque(op: &Op) -> bool {
    match *op {
        Op::Unbox {
            op: crate::ir::UnboxOp::IntValue | crate::ir::UnboxOp::LongValue,
            ..
        } => true,
        Op::CheckCast { name_ptr, .. } => {
            name_ptr != 0
                && crate::typecheck_target_for_site(name_ptr as *const u8).is_some_and(|c| c != 0)
        }
        // `CRATONVM_JIT_IR_STATIC_FINAL_EXACT_CLASS`: `checkcast`'s non-throwing
        // sibling against the same kind of resolved target -- a type test that
        // runs no Java, initializes nothing and writes no field
        // (`HttpUtil.validateToken`'s `value instanceof AsciiString`, inside
        // `HttpHeaderValidationUtilTest`'s name loop).
        Op::InstanceOf { name_ptr, .. } if crate::ir::ir_static_final_exact_class_enabled() => {
            name_ptr != 0
                && crate::typecheck_target_for_site(name_ptr as *const u8).is_some_and(|c| c != 0)
        }
        _ => false,
    }
}

/// The index half of a non-`Store` field write, for the conservative test.
struct AccessIdx(Option<i64>);

impl AccessIdx {
    fn from(offset: crate::ir::AccessOffset) -> AccessIdx {
        match offset {
            crate::ir::AccessOffset::Const(v) => AccessIdx(Some(v)),
            _ => AccessIdx(None),
        }
    }

    /// Provably not field `idx`.
    fn differs_from(&self, idx: i64) -> bool {
        self.0.is_some_and(|v| v != idx)
    }
}

/// Does a node of the loop test `base`'s class? A read of `base` inside the
/// loop may then be legal only past that test (a guarded splice's receiver,
/// an unboxing guard), and a pre-header read would run before it: on an
/// object of another class it reads another layout's word, which, for a
/// reference field held across the loop's safepoints, the collector would
/// trace. Such a base is left alone.
///
/// nio21 wave 4 (lane L5): `ExactClassIs` is PURE, so `loop_body` never
/// counts it a member (the body holds control and pinned nodes only), and
/// the arm that matched it here never fired: a guarded splice's
/// `If(ExactClassIs(base, C))` / `Guard(ExactClassIs(base, C))` in the loop
/// left `base` forwardable, and its C-declared fields were read in the
/// pre-header on whatever object arrived. The test is now also asked of every
/// body node's INPUTS: a class test consumed by a node of the loop.
/// `CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_CLASS_INPUTS=0` restores the old
/// (unsound) answer, for bisecting a performance change only.
pub(super) fn class_tested_in(graph: &Graph, base: NodeId, body_nodes: &[NodeId]) -> bool {
    let inputs_too = true;
    let tests_base = |n: &crate::ir::Node| {
        matches!(
            n.op,
            Op::ExactClassIs { .. }
                // JIT round 19 (lane osr19): a class-word chain head is a
                // class test of its input too (the `Cmp`s read its answer).
                | Op::ClassWordOf
                | Op::InstanceOf { .. }
                | Op::CheckCast { .. }
                | Op::Unbox { .. }
        ) && n.inputs.as_slice().contains(&base)
    };
    // JIT round 20 (lane loopopt): the class-word forms
    // (`CRATONVM_JIT_IR_CLASS_WORD_CHAIN` / `..._SINGLE_TEST`) build the test
    // as `If(Cmp(Eq, ClassWordOf(base), id))` / `Guard(Cmp(..))`, so the
    // consumer's input is the `Cmp`, one step further from the head than the
    // direct-input check reaches: such a guarded splice's receiver was still
    // forwardable. A `Cmp` one of whose inputs is a class test of `base`
    // counts as one (`base_class_tested_by_loop`'s rule, which promotion
    // already asks).
    // JIT round 20 wave 3 (lane loopopt): and through the pure combinators a
    // condition may be built of (`And` / `Or` / `Xor` of tests, a `Cmp` of
    // one), to a small depth; deeper shapes are not built today.
    fn through_pure(
        graph: &Graph,
        base: NodeId,
        x: NodeId,
        depth: u32,
        tests_base: &dyn Fn(&crate::ir::Node) -> bool,
    ) -> bool {
        let Some(n) = graph.node_opt(x) else {
            return false;
        };
        if tests_base(n) {
            return true;
        }
        depth > 0
            && matches!(n.op, Op::Cmp(_) | Op::And | Op::Or | Op::Xor)
            && n.inputs.as_slice().iter().any(|&y| {
                y != NO_NODE && y != base && through_pure(graph, base, y, depth - 1, tests_base)
            })
    }
    let tests_base_through_cmp = |n: &crate::ir::Node| {
        tests_base(n)
            || (matches!(n.op, Op::Cmp(_) | Op::And | Op::Or | Op::Xor)
                && n.inputs.as_slice().iter().any(|&y| {
                    y != NO_NODE && y != base && through_pure(graph, base, y, 2, &tests_base)
                }))
    };
    body_nodes.iter().any(|&id| {
        graph.node_opt(id).is_some_and(|n| {
            tests_base(n)
                || (inputs_too
                    && n.inputs.as_slice().iter().any(|&x| {
                        x != NO_NODE
                            && x != base
                            && graph.node_opt(x).is_some_and(&tests_base_through_cmp)
                    }))
        })
    })
}

/// `false` when every origin of `base` is an allocation of this compile:
/// escape analysis, which runs later, deletes such an object outright and
/// answers its reads itself.
pub(super) fn base_outside_ea(graph: &Graph, base: NodeId) -> bool {
    if graph.node_opt(base).map(|n| n.ty) != Some(IrType::Ref) {
        return false;
    }
    let origin = graph.ref_origin(base);
    !(origin.allocs.as_ref().is_some_and(|a| !a.is_empty()) && !origin.pre_existing)
}

/// Push the pre-header's frame (see the module comment) and return its index.
/// JIT r22 wave 2: also `loop_retire`'s inclusive-bound guard (LOOPS22-3).
pub(super) fn preheader_snapshot(
    graph: &mut Graph,
    region: NodeId,
    fwd: usize,
    header_bci: usize,
    body: &FxHashSet<NodeId>,
) -> Option<u32> {
    let flat = graph
        .safepoints
        .iter()
        .position(|s| s.bci == header_bci && !s.is_splice_state() && !s.post_call_only);
    // `CRATONVM_JIT_IR_STATIC_FINAL_EXACT_CLASS`: a loop inside a spliced
    // body has only the chain snapshot at its header (innermost scope's
    // locals, then its stack and every enclosing scope's frame). Its copy,
    // mapped the same way, resumes the interpreter at the header with the
    // whole caller chain -- the state the loop is entered with.
    let si = match flat {
        Some(si) => si,
        None if crate::ir::ir_static_final_exact_class_enabled() => {
            let mut chains: Vec<usize> = graph
                .safepoints
                .iter()
                .enumerate()
                .filter(|(_, s)| s.bci == header_bci && s.is_splice_state() && !s.post_call_only)
                .map(|(i, _)| i)
                .collect();
            // The r23 L8 heap-buffer close
            // (`CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_SPLICED_EXACT`): a chain
            // predicate planted before this forward (the affine or length
            // predicate of a spliced loop) named a second chain snapshot at the
            // header's pc, its loop-entry copy. Set those aside: the header's
            // own snapshot is the one this copy maps.
            if chains.len() > 1 && super::class_predicate::spliced_exact_forward_enabled() {
                let named: Vec<usize> = graph
                    .chain_predicate_guards
                    .iter()
                    .filter_map(|&(g, _)| graph.node_opt(g)?.frame_snapshot)
                    .map(|si| si as usize)
                    .collect();
                chains.retain(|si| !named.contains(si));
            }
            let [si] = chains.as_slice() else {
                return None;
            };
            *si
        }
        None => return None,
    };
    let sp = graph.safepoints.get(si)?.clone();
    let map = |graph: &Graph, v: NodeId| -> Option<NodeId> {
        if v == NO_NODE {
            return Some(NO_NODE);
        }
        let node = graph.node_opt(v)?;
        if node.op == Op::Phi && node.input_opt(0) == Some(region) {
            return node.input_opt(fwd + 1);
        }
        // Anything else the header's frame names must already exist when
        // the loop is entered.
        (!body.contains(&v)).then_some(v)
    };
    let mut locals = Vec::with_capacity(sp.locals.len());
    for &v in &sp.locals {
        locals.push(map(graph, v)?);
    }
    let mut stack = Vec::with_capacity(sp.stack.len());
    for &v in &sp.stack {
        stack.push(map(graph, v)?);
    }
    let mut monitors = Vec::with_capacity(sp.monitors.len());
    for &v in &sp.monitors {
        monitors.push(map(graph, v)?);
    }
    let mut copy = SafepointSnapshot::new(header_bci, locals, stack, monitors);
    copy.monitor_elided_mask = sp.monitor_elided_mask;
    copy.splice_scope = sp.splice_scope.clone();
    let index = u32::try_from(graph.safepoints.len()).ok()?;
    graph.push_safepoint(copy);
    Some(index)
}

/// The value of one forwarded field at a memory state inside the loop.
struct Walk<'a> {
    key: FieldKey,
    /// The header's forward predecessor slot (its φs' input `fwd + 1`).
    fwd: usize,
    header_mem: NodeId,
    init: NodeId,
    body: &'a FxHashSet<NodeId>,
    memo: FxHashMap<NodeId, NodeId>,
    budget: usize,
    /// nio21 wave 18 (lane L3): the loop's opaque writers the census
    /// admitted, `writer -> successor bci and frame` ([`post_writer_frame`]).
    writers: &'a FxHashMap<NodeId, WriterFrame>,
    /// Every reload placed after a writer so far, for any key: a walk steps
    /// over another key's reload (a read writes nothing).
    reload_nodes: &'a mut FxHashSet<NodeId>,
    /// nio21 wave 19: `writer -> the snapshot index of its pushed frame copy`.
    pushed_frames: &'a mut FxHashMap<NodeId, u32>,
    /// One in-loop read of the key: the index node and value type a reload
    /// copies.
    sample: NodeId,
    /// `writer -> this key's reload after it`.
    reload_of: FxHashMap<NodeId, NodeId>,
    /// This key's reloads, in the order placed.
    made: Vec<NodeId>,
}

impl Walk<'_> {
    fn value_at(&mut self, graph: &mut Graph, mem: NodeId) -> Option<NodeId> {
        let mut path: Vec<NodeId> = Vec::new();
        let mut cur = mem;
        let answer = loop {
            if let Some(&v) = self.memo.get(&cur) {
                break v;
            }
            self.budget = self.budget.checked_sub(1)?;
            let node = graph.node_opt(cur)?.clone();
            if cur == self.header_mem || (node.op == Op::Phi && node.ty == IrType::Memory) {
                break self.phi_value(graph, cur, &node)?;
            }
            // nio21 wave 18 (lane L3): this key's reload answers itself;
            // another key's is a plain read and is stepped over.
            if self.reload_nodes.contains(&cur) {
                if self.made.contains(&cur) {
                    break cur;
                }
                let token = crate::ir::memory_token_slot(&node).and_then(|t| node.input_opt(t))?;
                path.push(cur);
                cur = token;
                continue;
            }
            // nio21 wave 18 (lane L3): past an opaque writer the field is
            // read again, right after it.
            if let Some(frame) = self.writers.get(&cur) {
                let frame = frame.clone();
                break self.reload_after(graph, cur, frame)?;
            }
            if node.op == Op::Store(self.key.kind) {
                if let Some((base, idx, _)) = field_access(graph, cur) {
                    if base == self.key.base
                        && idx == self.key.idx
                        && graph.field_decl_class.get(&cur) == Some(&self.key.decl)
                    {
                        break node.input_opt(4)?;
                    }
                }
            }
            // The chain may only leave the loop through its header φ.
            if !self.body.contains(&cur) {
                return None;
            }
            let token = crate::ir::memory_token_slot(&node).and_then(|t| node.input_opt(t))?;
            path.push(cur);
            cur = token;
        };
        for p in path {
            self.memo.insert(p, answer);
        }
        Some(answer)
    }

    /// nio21 wave 18 (lane L3): this key's read of the field right after
    /// `writer` -- `Load [writer's control, writer, base, index]` at the
    /// writer's successor bci `succ`, whose frame (the interpreter at `succ`
    /// with the writer's result pushed, `post_writer_frame`) is the one a
    /// deopt of the read resumes into -- threaded into the memory chain
    /// between the writer and every memory user it had, so every later
    /// access of the loop is ordered after it. Placed once per writer.
    ///
    /// The base is the forwarded one: defined outside the loop and already
    /// dereferenced by the pre-header read, so it is non-null here, and its
    /// class is the one every in-loop read of the key assumed.
    fn reload_after(
        &mut self,
        graph: &mut Graph,
        writer: NodeId,
        frame: WriterFrame,
    ) -> Option<NodeId> {
        if let Some(&r) = self.reload_of.get(&writer) {
            return Some(r);
        }
        let succ = frame.succ;
        let ctrl = graph.node_opt(writer)?.input_opt(0)?;
        let (idx_node, ty) = {
            let s = graph.node_opt(self.sample)?;
            (s.input_opt(3)?, s.ty)
        };
        if ctrl == NO_NODE || idx_node == NO_NODE {
            return None;
        }
        let read = graph.add(
            Op::Load(self.key.kind),
            ty,
            vec![ctrl, writer, self.key.base, idx_node],
            Some(succ),
        );
        graph.field_decl_class.insert(read, self.key.decl);
        carry_byte_array_fact(graph, &[self.sample], read);
        inherit_field_row(graph, self.sample, read);
        graph.synthetic_field_access_pcs.insert(succ);
        // nio21 wave 19: a frame the builder's by-bci snapshot is not (a
        // join's phi replaced by the writer, or a spliced call's post-call
        // chain): pushed once per writer, named by every reload after it.
        if let Some(copy) = frame.copy {
            let si = match self.pushed_frames.get(&writer) {
                Some(&si) => si,
                None => {
                    let Ok(si) = u32::try_from(graph.safepoints.len()) else {
                        graph.kill(read);
                        return None;
                    };
                    graph.push_safepoint(copy);
                    note_frame_copy_pushed(writer);
                    self.pushed_frames.insert(writer, si);
                    si
                }
            };
            if !graph.set_node_frame_snapshot(read, si) {
                graph.kill(read);
                return None;
            }
        }
        for user in graph.users_of(writer) {
            if user == read {
                continue;
            }
            let Some(un) = graph.node_opt(user) else {
                continue;
            };
            let slots: Vec<usize> = un
                .inputs
                .as_slice()
                .iter()
                .enumerate()
                .filter(|&(i, &x)| x == writer && crate::ir::is_memory_token_slot(un, i))
                .map(|(i, _)| i)
                .collect();
            for slot in slots {
                graph.set_input(user, slot, read);
            }
        }
        self.reload_nodes.insert(read);
        self.reload_of.insert(writer, read);
        self.made.push(read);
        Some(read)
    }

    /// Undo [`Self::reload_after`] for a key whose walk failed: each reload's
    /// memory users take its own token back, and the reload dies. Newest
    /// first, so a reload threaded after another is gone before that one
    /// moves.
    fn unthread_reloads(&mut self, graph: &mut Graph) {
        for read in std::mem::take(&mut self.made).into_iter().rev() {
            let Some(token) = graph
                .node_opt(read)
                .and_then(|n| crate::ir::memory_token_slot(n).and_then(|t| n.input_opt(t)))
            else {
                continue;
            };
            for user in graph.users_of(read) {
                let Some(un) = graph.node_opt(user) else {
                    continue;
                };
                let slots: Vec<usize> = un
                    .inputs
                    .as_slice()
                    .iter()
                    .enumerate()
                    .filter(|&(i, &x)| x == read && crate::ir::is_memory_token_slot(un, i))
                    .map(|(i, _)| i)
                    .collect();
                for slot in slots {
                    graph.set_input(user, slot, token);
                }
            }
            // A failed walk handed the read to no rewired user, but a value φ
            // it built may still name it: then the read stays, unthreaded (an
            // extra read of a cell nothing wrote since the writer, never a
            // wrong one), and the cleanup reclaims it with those dead φs.
            if graph.users_of(read).is_empty() {
                graph.kill(read);
            }
            self.reload_nodes.remove(&read);
        }
        self.reload_of.clear();
    }

    /// A value φ over the memory φ `phi`, in the same predecessor order. At
    /// the loop header the forward edge carries the pre-header read.
    fn phi_value(
        &mut self,
        graph: &mut Graph,
        phi: NodeId,
        node: &crate::ir::Node,
    ) -> Option<NodeId> {
        let ctrl = node.input_opt(0)?;
        if phi != self.header_mem && !self.body.contains(&ctrl) {
            return None;
        }
        let ty = graph.node_opt(self.init)?.ty;
        let preds: Vec<NodeId> = node.inputs.as_slice().get(1..)?.to_vec();
        let mut inputs = vec![ctrl];
        inputs.extend(std::iter::repeat(NO_NODE).take(preds.len()));
        // Placeholders first, so a cycle back to this φ (a back edge) finds it
        // in the memo.
        let value = graph.add(Op::Phi, ty, inputs, node.bytecode_pc);
        self.memo.insert(phi, value);
        for (k, pred_mem) in preds.into_iter().enumerate() {
            let v = if phi == self.header_mem && k == self.fwd {
                self.init
            } else {
                self.value_at(graph, pred_mem)?
            };
            graph.set_input(value, k + 1, v);
        }
        Some(super::dedup_walk_phi(graph, ctrl, value, &mut self.memo))
    }
}

// ── FG16-5 / IROPS19-6: promote a loop's field cells to registers ─────────
//
// JIT round 19 proposal wave 2 (lane osr2), `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE`
// (opt-in). Forwarding (above) answers a loop's READS of a cell from registers
// but keeps every store, so memory is exact at every program point. This pass
// takes the next step for a careful subset: the cell's in-loop STORES are
// deleted and one write-back runs at the loop's exit.
//
// # The subset, and why each condition is there
//
// * An OSR-door compile, after forwarding (the same pre-header and entry
//   argument), and a loop whose cell was forwarded: the pass finds the
//   forwarding's pre-header read of the cell on the header φ's forward memory
//   edge and takes it as the cell's value on entry.
// * `int` / `long` cells (`MemKind::Int` -- which also carries the narrowed
//   `byte`/`short`/`char`/`boolean` fields -- and `MemKind::Long`) of a base
//   defined outside the loop, every write of that field identity in the loop a
//   `Store` through that base (the forwarding's own rule).
// * NO TRANSFER INSIDE THE LOOP. Memory is stale between the first deleted
//   store and the exit, and a frame state names locals and stack, never a
//   pending heap write, so no deopt, exception or mode exit may leave the
//   loop: every node of its body is `ir_lower::node_cannot_deopt`, except
//   (a) a `Store` (the cells' own, deleted, or another cell's) and (b) a
//   `Guard(base != null)`, both through a base the pre-header has already
//   dereferenced (its read deopts to the header on a null, before the loop
//   runs, so neither can ever fire), and no load of any kind remains. The
//   back-edge polls are the one transfer the graph does not show: their mode
//   exits are refused at these headers (`Graph::promoted_cell_header_bcis`,
//   `Lowerer::back_edge_mode_exit_state`) -- unless, since round 19 wave 4
//   (lane osr4, OSR2-1, `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_POLL_WRITEBACK`,
//   opt-in), the exit's pad writes every cell back first
//   (`Graph::promoted_cell_writebacks`, `Lowerer::promoted_cell_poll_writebacks`);
//   a collection at a poll needs
//   nothing (the cells are `int`/`long`), and another thread reading the field
//   mid-loop sees the value before the loop, a legal JMM execution for a plain
//   field with no synchronisation in the loop (calls, monitors and volatile or
//   ordered accesses are all refused), exactly as for the forwarded reads.
// * ONE EXIT, the header's own test, with no store pinned to the header: the
//   cell's value at the exit is its value at the header's memory φ, and every
//   memory user outside the loop reads that φ.
// * The write-back is CONDITIONAL: `if (v_exit != v_entry) base.f = v_exit`.
//   An unconditional one would write the field on a path where the program
//   never did (a zero-trip loop; a store under a branch that never ran --
//   a shape promoted only under `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_JOIN_WALK`,
//   round 19 wave 8: `loop_body` holds no body-join memory φ, so without it
//   such a loop is refused as `loop memory leaves past the header`) and
//   could overwrite another thread's write with the value this thread read:
//   an execution the JMM does not allow. Skipping it when the value is the
//   one read is the execution in which this thread's last write (of that
//   same value) came right after its read, which it does allow.
//
// The write-back's own null check cannot fire (the base was dereferenced in
// the pre-header); it carries the header's frame only because the lowering
// asks every store for one.

/// `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE=1` (JIT round 19 wave 2, lane osr2,
/// FG16-5 / IROPS19-6). **Default ON** since the round-19 proposal round (`=0` turns it off). Read without a `static`
/// (`process_global_statics_ratchet`), once per optimize.
pub(super) fn promote_enabled() -> bool {
    true
}

/// Promote the field cells this section describes, loop by loop. Returns how
/// many cells were promoted.
pub(super) fn promote_loop_cells(graph: &mut Graph) -> usize {
    promote_loop_cells_in(graph, None)
}

/// [`promote_loop_cells`] over the loops whose headers `only` names (`None`:
/// every loop). JIT round 21 wave 1 (lane loops): the method-entry use
/// ([`promote_method_entry_accumulators`]) visits only the loops it forwarded.
pub(super) fn promote_loop_cells_in(graph: &mut Graph, only: Option<&FxHashSet<NodeId>>) -> usize {
    if graph.nodes.iter().any(|n| {
        n.op != Op::Dead
            && n.op.memory_shape().is_some()
            && crate::ir::memory_token_slot(n).is_none()
    }) {
        return 0;
    }
    let mut promoted = 0usize;
    let mut tried: FxHashSet<NodeId> = FxHashSet::default();
    // A promotion rewires its exit edge, which may be the next loop's entry:
    // the loop list is taken again after each one.
    loop {
        let loops = loop_headers(graph);
        let headers: FxHashSet<NodeId> = loops.iter().map(|l| l.0).collect();
        let Some((region, entry_pred, back_ctrls)) = loops
            .into_iter()
            .find(|l| !tried.contains(&l.0) && only.is_none_or(|set| set.contains(&l.0)))
        else {
            break;
        };
        tried.insert(region);
        promoted += promote_loop(graph, region, entry_pred, &back_ctrls, &headers);
    }
    if promoted > 0 && crate::x64::dbg_jitc_enabled() {
        eprintln!(
            "[ir] loop field promote: {} {promoted} cell(s) kept in registers",
            graph.scope_method_key
        );
    }
    promoted
}

/// `CRATONVM_DBG_LOOP_FIELD_FORWARD=1`: why a loop's cells were not promoted.
fn promote_refuse(header_bci: usize, why: &str, node: NodeId) -> usize {
    if false
        || super::loop_pass_report_enabled()
    {
        eprintln!("[ir] loop field promote: header bci {header_bci} refused -- {why} (n{node})");
    }
    0
}

/// `Guard(Cmp(Ne, b, null))` (either operand order) with `b` in `proven`.
fn guard_is_proven_null_check(graph: &Graph, guard: NodeId, proven: &FxHashSet<NodeId>) -> bool {
    let Some(cond) = graph.node_opt(guard).and_then(|g| g.input_opt(1)) else {
        return false;
    };
    let Some(c) = graph.node_opt(cond) else {
        return false;
    };
    if c.op != Op::Cmp(crate::ir::CmpOp::Ne) {
        return false;
    }
    let (Some(l), Some(r)) = (c.input_opt(0), c.input_opt(1)) else {
        return false;
    };
    let is_null = |v: NodeId| {
        graph
            .node_opt(v)
            .is_some_and(|n| n.op == Op::Const(0) && n.ty == IrType::Ref)
    };
    (is_null(r) && proven.contains(&l)) || (is_null(l) && proven.contains(&r))
}

/// Is `base`'s class tested by a branch or guard of the loop? Asked of the
/// base's USERS, not of the loop body (which holds no pure node, so a test
/// like `ExactClassIs` is never in it: the defect
/// `r19-loop-field-forward-never-sees-a-pure-class-test-FIXED-20261012.md`, left to
/// its own fix). A class test (`ExactClassIs`, `ClassWordOf` -- through its
/// `Cmp` --, `InstanceOf`, `CheckCast`, `Unbox`) whose answer an `If` or
/// `Guard` of `body` reads means the cell's accesses may be legal only on one
/// edge of that test; promotion leaves such a base alone, so it never rests on
/// the forwarding's dead check.
fn base_class_tested_by_loop(graph: &Graph, base: NodeId, body: &FxHashSet<NodeId>) -> bool {
    let read_by_body_branch = |v: NodeId| {
        graph.users_of(v).into_iter().any(|u| {
            body.contains(&u)
                && graph
                    .node_opt(u)
                    .is_some_and(|n| matches!(n.op, Op::If | Op::Guard { .. }))
        })
    };
    graph.users_of(base).into_iter().any(|t| {
        let Some(n) = graph.node_opt(t) else {
            return false;
        };
        match n.op {
            Op::ExactClassIs { .. }
            | Op::InstanceOf { .. }
            | Op::CheckCast { .. }
            | Op::Unbox { .. } => body.contains(&t) || read_by_body_branch(t),
            Op::ClassWordOf => {
                read_by_body_branch(t)
                    || graph.users_of(t).into_iter().any(|c| {
                        graph
                            .node_opt(c)
                            .is_some_and(|cn| matches!(cn.op, Op::Cmp(_)))
                            && read_by_body_branch(c)
                    })
            }
            _ => false,
        }
    })
}

/// One in-loop field store: `(node, base, index, kind, declaring class)`.
type LoopStore = (NodeId, NodeId, i64, MemKind, Option<u32>);

fn promote_loop(
    graph: &mut Graph,
    region: NodeId,
    entry_pred: NodeId,
    back_ctrls: &[NodeId],
    headers: &FxHashSet<NodeId>,
) -> usize {
    let Some(header_bci) = graph.node_opt(region).and_then(|n| n.bytecode_pc) else {
        return 0;
    };
    let Some(fwd) = graph
        .node_opt(region)
        .and_then(|n| n.inputs.iter().position(|&c| c == entry_pred))
    else {
        return 0;
    };
    let mem_phis: Vec<NodeId> = graph
        .users_of(region)
        .into_iter()
        .filter(|&u| {
            graph.node_opt(u).is_some_and(|n| {
                n.op == Op::Phi && n.ty == IrType::Memory && n.input_opt(0) == Some(region)
            })
        })
        .collect();
    let [header_mem] = mem_phis.as_slice() else {
        return promote_refuse(header_bci, "not exactly one header memory phi", region);
    };
    let header_mem = *header_mem;
    let mut body = loop_body(graph, region, entry_pred, back_ctrls);
    if headers.iter().any(|&h| h != region && body.contains(&h)) {
        return promote_refuse(header_bci, "an inner loop", region);
    }
    // OSR5-3 (round 19 wave 7, lane osr7,
    // `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PENDING_BRANCH`): `loop_body` admits
    // only the phis anchored at the header, so a memory φ at a join INSIDE the
    // body (a store under a branch) was read as a memory user outside the loop
    // and refused (`loop memory leaves past the header`) whatever else the loop
    // held. Under the switch (with `..._PENDING_DEOPT`) such φs -- value and
    // memory -- join the body; a loop holding one is then promoted only when it
    // also holds a deopt point (the shape the switch is for; refused below
    // otherwise, as before).
    //
    // OSR7-2 (round 19 wave 8, lane osr8, `join_walk_enabled`): the same
    // admission for a loop WITHOUT a deopt point too. Its stores are deleted
    // and the exit value is the walk's (`Walk::value_at` builds a value φ at
    // every body memory φ on the way back from the header), so a store under
    // a branch is promoted as the wave-2 docs said it was. A loop holding a
    // deopt point still needs `..._PENDING_BRANCH` (refused below otherwise).
    let join_walk = join_walk_enabled();
    let branch_phis = join_walk
        || (pending_deopt_enabled()
            && !graph.promote_pending_withheld
            && crate::ir_lower::ir_loop_field_promote_pending_branch_enabled());
    if branch_phis {
        let inner: Vec<NodeId> = graph
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| {
                n.op == Op::Phi
                    && n.input_opt(0)
                        .is_some_and(|c| c != region && body.contains(&c))
            })
            // Cast: arena index to node id.
            .map(|(id, _)| id as NodeId)
            .collect();
        body.extend(inner);
    }

    // The one exit: the header's own test.
    let mut exits: Vec<NodeId> = Vec::new();
    for (id, n) in graph.nodes.iter().enumerate() {
        // Cast: arena index to node id.
        let id = id as NodeId;
        if n.op == Op::Dead || !n.op.is_control() || body.contains(&id) {
            continue;
        }
        if n.inputs.iter().any(|&i| i != NO_NODE && body.contains(&i)) {
            exits.push(id);
        }
    }
    let exit_from_header = |x: NodeId| {
        graph.node_opt(x).is_some_and(|x| {
            matches!(x.op, Op::Proj(_))
                && x.input_opt(0)
                    .and_then(|i| graph.node_opt(i))
                    .is_some_and(|i| i.op == Op::If && i.input_opt(0) == Some(region))
        })
    };
    // OSR2-4 (round 19 wave 5, lane osr5, `body_exits_enabled`): the
    // header's exit plus exits from inside the body (`break`), each the `Proj`
    // of a body `If` reached from the header without a join
    // (`interior_exit_spine`); each gets its own write-back.
    let mut interior: Vec<(NodeId, FxHashSet<NodeId>)> = Vec::new();
    let exit = if body_exits_enabled() && exits.len() > 1 {
        let header_exits: Vec<NodeId> = exits
            .iter()
            .copied()
            .filter(|&x| exit_from_header(x))
            .collect();
        let [exit] = header_exits.as_slice() else {
            return promote_refuse(header_bci, "not exactly one header exit", region);
        };
        for &x in &exits {
            if x == *exit {
                continue;
            }
            let Some(spine) = interior_exit_spine(graph, x, region, &body) else {
                return promote_refuse(header_bci, "an exit below a join", x);
            };
            interior.push((x, spine));
        }
        *exit
    } else {
        let [exit] = exits.as_slice() else {
            return promote_refuse(header_bci, "not exactly one exit edge", region);
        };
        if !exit_from_header(*exit) {
            return promote_refuse(header_bci, "the exit is not the header's test", *exit);
        }
        *exit
    };

    // The forwarding's pre-header reads, newest first along the forward
    // memory edge.
    let mut pre: FxHashMap<FieldKey, NodeId> = FxHashMap::default();
    let mut m = graph
        .node_opt(header_mem)
        .and_then(|n| n.input_opt(fwd + 1))
        .unwrap_or(NO_NODE);
    for _ in 0..MAX_WALK {
        let Some(n) = graph.node_opt(m) else {
            break;
        };
        if !(matches!(n.op, Op::Load(_))
            && n.input_opt(0) == Some(entry_pred)
            && n.bytecode_pc == Some(header_bci))
        {
            break;
        }
        let (Some((base, idx, kind)), Some(&decl)) =
            (field_access(graph, m), graph.field_decl_class.get(&m))
        else {
            break;
        };
        pre.entry(FieldKey {
            base,
            idx,
            decl,
            kind,
        })
        .or_insert(m);
        let Some(token) = crate::ir::memory_token_slot(n).and_then(|t| n.input_opt(t)) else {
            break;
        };
        m = token;    }
    if pre.is_empty() {
        return promote_refuse(header_bci, "no pre-header read", region);
    }
    let proven: FxHashSet<NodeId> = pre.keys().map(|k| k.base).collect();

    // Census: nothing in the loop may transfer to the interpreter -- unless
    // (round 19 wave 5, lane osr5, OSR2-3, `pending_deopt_enabled`) the
    // transfer is a guard or a primitive array read, whose deopt pad then
    // writes every cell back first (`deopt_inside`, below).
    //
    // OSR5-2 (round 19 wave 6, lane osr6): never on the IR door's retry after
    // a lowering refused such a loop (`Graph::promote_pending_withheld`).
    let pending = pending_deopt_enabled() && !graph.promote_pending_withheld;
    let mut deopt_inside = false;
    // OSR5-2: the body's admitted deopt points, for the prediction below.
    let mut deopt_nodes: Vec<NodeId> = Vec::new();
    let mut body_nodes: Vec<NodeId> = body.iter().copied().collect();
    body_nodes.sort_unstable();
    let mut stores: Vec<LoopStore> = Vec::new();
    for &id in &body_nodes {
        let Some(node) = graph.node_opt(id) else {
            continue;
        };
        if node.op == Op::Dead || node.op == Op::Phi {
            continue;
        }
        if node.is_volatile_access() {
            return promote_refuse(header_bci, "a volatile access", id);
        }
        match node.op {
            Op::Store(kind) => {
                let Some((base, idx, _)) = field_access(graph, id) else {
                    return promote_refuse(header_bci, "a store the pass cannot read", id);
                };
                if node.input_opt(0) == Some(region) {
                    return promote_refuse(header_bci, "a store at the header", id);
                }
                if !proven.contains(&base) {
                    return promote_refuse(header_bci, "a store that can deopt", id);
                }
                stores.push((
                    id,
                    base,
                    idx,
                    kind,
                    graph.field_decl_class.get(&id).copied(),
                ));
                continue;
            }
            Op::Guard { .. } if guard_is_proven_null_check(graph, id, &proven) => continue,
            _ => {}
        }
        let eff = graph.memory_effect(id);
        if eff.order != MemOrder::Plain || matches!(eff.writes, AliasClass::Any) {
            return promote_refuse(header_bci, "an ordering or opaque write", id);
        }
        if !crate::ir_lower::node_cannot_deopt(graph, node) {
            // OSR2-3: a guard, a primitive array element access or length
            // (array / length alias classes: never a field cell) or -- since
            // PROMOTE21-4 (lane promote residual) -- an integer division
            // deopts only through pads that write the cells back. One rule
            // with the static and fresh promotions (`pad_admitted_deopt_point`,
            // asked after the ordering check above, as theirs).
            if pending && pad_admitted_deopt_point(graph, id) {
                deopt_inside = true;
                deopt_nodes.push(id);
                continue;
            }
            if matches!(node.op, Op::Guard { .. }) {
                return promote_refuse(header_bci, "a guard that can deopt", id);
            }
            return promote_refuse(header_bci, "a node that can deopt", id);
        }
    }

    // OSR5-2 (round 19 wave 6, lane osr6,
    // `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PREDICT`): a deopt point the lowering
    // would give no write-back pad refuses the whole compile; the causes the
    // graph already shows refuse this loop's promotion instead.
    if deopt_inside && crate::ir_lower::ir_loop_field_promote_predict_enabled() {
        if let Some((id, why)) = predicted_pad_refusal(graph, &deopt_nodes, false) {
            // Wave 7 (lane osr7, OSR6-2): the predict switch's first layer.
            crate::ir_check_elim::note_promote_census(
                crate::ir_check_elim::PROMOTE_CENSUS_PREDICTED,
                1,
            );
            return promote_refuse(header_bci, why, id);
        }
    }

    // The cells.
    let mut keys: Vec<FieldKey> = pre.keys().copied().collect();
    keys.sort_by_key(|k| (k.base, k.idx, k.decl));
    let mut cells: Vec<(FieldKey, NodeId, Vec<NodeId>)> = Vec::new();
    for key in keys {
        if !matches!(key.kind, MemKind::Int | MemKind::Long)
            || body.contains(&key.base)
            || base_class_tested_by_loop(graph, key.base, &body)
        {
            continue;
        }
        let own: Vec<NodeId> = stores
            .iter()
            .filter(|&&(_, base, idx, kind, decl)| {
                base == key.base && idx == key.idx && kind == key.kind && decl == Some(key.decl)
            })
            .map(|s| s.0)
            .collect();
        if own.is_empty() {
            continue;
        }
        let aliased = stores.iter().any(|&(id, _, idx, _, decl)| {
            idx == key.idx && !own.contains(&id) && decl.is_none_or(|d| d == key.decl)
        });
        if aliased {
            continue;
        }
        let Some(&init) = pre.get(&key) else {
            continue;
        };
        cells.push((key, init, own));    }
    if cells.is_empty() {
        return promote_refuse(header_bci, "no promotable cell", region);
    }
    // OSR2-3: with a deopt point inside, the lowerer tracks each cell's value
    // along the schedule and needs ONE value per cell at every block entry; a
    // memory φ inside the body (a store under a branch) would need a value φ
    // the graph does not have. Refused, fail closed -- unless (round 19 wave
    // 7, lane osr7, OSR5-3, `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PENDING_BRANCH`)
    // the plan may take the exit walk's value φ at each such join
    // (`Walk::phi_value`, `ir_lower::plan_promoted_pending`); a join the walk
    // never crossed leaves the cell unknown there, and a deopt point reached
    // with it unknown refuses the compile in the lowering.
    if deopt_inside
        && !crate::ir_lower::ir_loop_field_promote_pending_branch_enabled()
        && body.iter().any(|&b| {
            b != header_mem
                && graph
                    .node_opt(b)
                    .is_some_and(|n| n.op == Op::Phi && n.ty == IrType::Memory)
        })
    {
        return promote_refuse(header_bci, "a body memory phi with a deopt point", region);
    }
    // OSR5-3: the switch admits body φs only for loops with a deopt point --
    // unless OSR7-2's `join_walk` admits them for every loop.
    if branch_phis
        && !join_walk
        && !deopt_inside
        && body.iter().any(|&b| {
            b != header_mem
                && graph
                    .node_opt(b)
                    .is_some_and(|n| n.op == Op::Phi && n.ty == IrType::Memory)
        })
    {
        return promote_refuse(
            header_bci,
            "a body memory phi without a deopt point",
            region,
        );
    }

    // OSR2-4: with an interior exit, the body's memory must be ONE chain of
    // writes from the header phi (no body memory phi), so the memory state at
    // each exit is the last write of that chain on the exit's own path.
    // `exit_mems[k]` is interior exit `k`'s state.
    let mut exit_mems: Vec<NodeId> = Vec::new();
    if !interior.is_empty() {
        if body.iter().any(|&b| {
            b != header_mem
                && graph
                    .node_opt(b)
                    .is_some_and(|n| n.op == Op::Phi && n.ty == IrType::Memory)
        }) {
            return promote_refuse(header_bci, "a body memory phi with a body exit", region);
        }
        let Some(chain) = body_write_chain(graph, header_mem, &body) else {
            return promote_refuse(header_bci, "a body write off the memory chain", region);
        };
        for (x, spine) in &interior {
            let mut at = header_mem;
            let mut off_path = false;
            for &w in &chain {
                let on_path = graph
                    .node_opt(w)
                    .and_then(|n| n.input_opt(0))
                    .is_some_and(|c| spine.contains(&c));
                if on_path && off_path {
                    return promote_refuse(header_bci, "a write before an exit off its path", *x);
                }
                if on_path {
                    at = w;
                } else {
                    off_path = true;
                }
            }
            exit_mems.push(at);
        }
    }

    // Every memory user outside the loop must read the header's memory φ --
    // or, with interior exits, the memory state of the exit it follows
    // (`outside_user_exit`).
    let mut outside: Vec<(NodeId, usize, Option<usize>)> = Vec::new();
    for (id, n) in graph.nodes.iter().enumerate() {
        // Cast: arena index to node id.
        let id = id as NodeId;
        if n.op == Op::Dead || body.contains(&id) || id == header_mem {
            continue;
        }
        for (i, &inp) in n.inputs.iter().enumerate() {
            if inp == NO_NODE || !(inp == header_mem || body.contains(&inp)) {
                continue;
            }
            if !crate::ir::is_memory_token_slot(n, i) {
                continue;
            }
            if interior.is_empty() {
                if inp != header_mem {
                    return promote_refuse(header_bci, "loop memory leaves past the header", id);
                }
                outside.push((id, i, None));
                continue;
            }
            let interior_exits: Vec<NodeId> = interior.iter().map(|(x, _)| *x).collect();
            let Some(which) = outside_user_exit(graph, id, i, exit, &interior_exits) else {
                return promote_refuse(header_bci, "loop memory read after a join of exits", id);
            };
            let expected = match which {
                None => header_mem,
                Some(k) => exit_mems[k],
            };
            if inp != expected {
                return promote_refuse(header_bci, "loop memory leaves past its exit", id);
            }
            outside.push((id, i, which));
        }
    }

    // The value of each cell at the exit (the header's memory state), walked
    // before any store is deleted: the walk answers a store with its value.
    // OSR2-4: and at each interior exit's memory state (`at_exits`).
    let mut ready: Vec<(FieldKey, NodeId, NodeId, Vec<NodeId>)> = Vec::new();
    let mut at_exits: Vec<Vec<NodeId>> = Vec::new();
    // OSR7-2: the body's memory φs other than the header's (sorted, so the
    // forced walks build their φs in a deterministic order), and the value
    // φs those walks built.
    let pending_branch = crate::ir_lower::ir_loop_field_promote_pending_branch_enabled();
    let mut body_mem_phis: Vec<NodeId> = if join_walk && deopt_inside && pending_branch {
        body.iter()
            .copied()
            .filter(|&b| {
                b != header_mem
                    && graph
                        .node_opt(b)
                        .is_some_and(|n| n.op == Op::Phi && n.ty == IrType::Memory)
            })
            .collect()
    } else {
        Vec::new()
    };
    body_mem_phis.sort_unstable();
    let mut forced_join_phis: Vec<NodeId> = Vec::new();
    // nio21 wave 18: the promotion admits no opaque writer (its census refuses
    // every `Any` write), so its walks place no reload.
    let no_writers: FxHashMap<NodeId, WriterFrame> = FxHashMap::default();
    let mut no_reloads: FxHashSet<NodeId> = FxHashSet::default();
    let mut no_frames: FxHashMap<NodeId, u32> = FxHashMap::default();
    for (key, init, own) in cells {
        let mut walk = Walk {
            key,
            fwd,
            header_mem,
            init,
            body: &body,
            memo: FxHashMap::default(),
            budget: MAX_WALK,
            writers: &no_writers,
            reload_nodes: &mut no_reloads,
            pushed_frames: &mut no_frames,
            sample: NO_NODE,
            reload_of: FxHashMap::default(),
            made: Vec::new(),
        };
        let Some(v) = walk.value_at(graph, header_mem) else {
            // A cell whose walk failed keeps its stores, which cannot deopt
            // (their base is proven): the census above already admitted them.
            continue;
        };
        let mut vs = Vec::with_capacity(exit_mems.len());
        for &m in &exit_mems {
            match walk.value_at(graph, m) {
                Some(x) => vs.push(x),
                None => break,
            }
        }
        if vs.len() != exit_mems.len() {
            continue; // an interior exit without a value: the cell keeps its stores
        }
        // OSR7-2 (round 19 wave 8, lane osr8, `join_walk_enabled`): with a
        // deopt point inside and the pads reading joins' value φs
        // (`..._PENDING_BRANCH`), walk EVERY body memory φ, so a join the exit
        // walk never crossed (an unconditional store of the cell follows it)
        // has its value φ too. Kept only if every such walk succeeds (a failed
        // walk can leave placeholder φs behind it in the memo); rooted for
        // dead-node elimination only once the loop is promoted (below).
        if join_walk && deopt_inside && pending_branch {
            let mut forced: Vec<NodeId> = Vec::new();
            let mut all = true;
            for &m in &body_mem_phis {
                if walk.value_at(graph, m).is_none() {
                    all = false;
                    break;
                }
            }
            if all {
                for &m in &body_mem_phis {
                    let Some(&vp) = walk.memo.get(&m) else {
                        continue;
                    };
                    let anchored = graph.node_opt(m).and_then(|n| n.input_opt(0));
                    if graph
                        .node_opt(vp)
                        .is_some_and(|n| n.op == Op::Phi && n.input_opt(0) == anchored)
                    {
                        forced.push(vp);
                    }
                }
                forced_join_phis.extend(forced);
            }
        }
        ready.push((key, init, v, own));
        at_exits.push(vs);
    }
    if ready.is_empty() {
        return promote_refuse(header_bci, "no exit value", region);
    }
    let Some(si) = promotion_header_frame(graph, header_bci) else {
        return promote_refuse(header_bci, "no header frame", region);
    };

    // The write-backs: one `if (v != init) base.f = v` diamond per cell,
    // chained on the exit edge.
    let exit_users: Vec<NodeId> = graph.users_of(exit);
    let mut ctrl = exit;
    let mut mem = header_mem;
    let mut done = 0usize;
    let mut deleted: Vec<NodeId> = Vec::new();
    for (key, init, value, own) in &ready {
        let Some(idx_node) = graph.node_opt(*init).and_then(|n| n.input_opt(3)) else {
            continue;
        };
        let (join, phi) = add_cell_writeback(
            graph, ctrl, mem, key, *init, idx_node, *value, header_bci, si,
        );
        // The diamond's store: the memory phi's first value input.
        let store = graph
            .node_opt(phi)
            .and_then(|n| n.input_opt(1))
            .unwrap_or(NO_NODE);
        ctrl = join;
        mem = phi;
        if deopt_inside {
            // OSR2-3: kept, silent, and recorded (see `Graph::promoted_sunk_stores`).
            graph
                .promoted_sunk_stores
                .extend(own.iter().map(|&st| (store, st)));
        } else {
            deleted.extend(own.iter().copied());
        }
        // Round 19 wave 4 (lane osr4, OSR2-1): the lowerer finds the cell
        // (base, field, kind, value at the header, value read on entry) from
        // this store when it gives the header's poll a write-back exit.
        graph.promoted_cell_writebacks.push((region, store));
        done += 1;
    }
    if done == 0 {
        return 0;
    }
    // OSR7-2: the loop is promoted, so its forced join value φs are kept.
    if !forced_join_phis.is_empty() {
        forced_join_phis.sort_unstable();
        forced_join_phis.dedup();
        let before = graph.promoted_join_phis.len();
        for phi in forced_join_phis {
            if !graph.promoted_join_phis.contains(&phi) {
                graph.promoted_join_phis.push(phi);
            }
        }
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[ir] loop field promote: header bci {header_bci}: {} join value phi(s) kept for the pads",
                graph.promoted_join_phis.len() - before
            );
        }
    }
    // OSR2-4: each interior exit's own write-backs, of the cells' values at
    // that exit (the same conditional diamonds; never recorded for the
    // lowerer, which reads a cell from its HEADER exit's store).
    let mut interior_mems: Vec<NodeId> = Vec::with_capacity(interior.len());
    for (k, (x, _)) in interior.iter().enumerate() {
        let x_users: Vec<NodeId> = graph.users_of(*x);
        let mut xctrl = *x;
        let mut xmem = exit_mems[k];
        for ((key, init, _, _), vs) in ready.iter().zip(&at_exits) {
            let Some(idx_node) = graph.node_opt(*init).and_then(|n| n.input_opt(3)) else {
                continue;
            };
            let (c, m) = add_cell_writeback(
                graph, xctrl, xmem, key, *init, idx_node, vs[k], header_bci, si,
            );
            xctrl = c;
            xmem = m;
        }
        for user in x_users {
            let Some(n) = graph.node_opt(user) else {
                continue;
            };
            let slots: Vec<usize> = n
                .inputs
                .iter()
                .enumerate()
                .filter(|&(_, &c)| c == *x)
                .map(|(i, _)| i)
                .collect();
            for slot in slots {
                graph.set_input(user, slot, xctrl);
            }
        }
        interior_mems.push(xmem);
    }
    // The exit's old successors now follow the last join; the memory users
    // outside the loop read the last write-back's φ.
    for user in exit_users {
        let Some(n) = graph.node_opt(user) else {
            continue;
        };
        let slots: Vec<usize> = n
            .inputs
            .iter()
            .enumerate()
            .filter(|&(_, &x)| x == exit)
            .map(|(i, _)| i)
            .collect();
        for slot in slots {
            graph.set_input(user, slot, ctrl);
        }
    }
    for (user, slot, which) in outside {
        let m = match which {
            None => mem,
            Some(k) => interior_mems[k],
        };
        graph.set_input(user, slot, m);
    }
    // The in-loop stores of the promoted cells go: their memory users take
    // the store's own token.
    for st in deleted {
        let Some(token) = graph
            .node_opt(st)
            .and_then(|n| crate::ir::memory_token_slot(n).and_then(|t| n.input_opt(t)))
        else {
            continue;
        };
        for user in graph.users_of(st) {
            let Some(node) = graph.node_opt(user) else {
                continue;
            };
            let slots: Vec<usize> = node
                .inputs
                .iter()
                .enumerate()
                .filter(|&(i, &x)| x == st && crate::ir::is_memory_token_slot(node, i))
                .map(|(i, _)| i)
                .collect();
            for slot in slots {
                graph.set_input(user, slot, token);
            }
        }
        graph.field_decl_class.remove(&st);
        graph.kill(st);
    }
    if !graph.promoted_cell_header_bcis.contains(&header_bci) {
        graph.promoted_cell_header_bcis.push(header_bci);
    }
    if crate::x64::dbg_jitc_enabled() {
        eprintln!(
            "[ir] loop field promote: header bci {header_bci}: {done} cell(s), stores sunk to the exit{}",
            if deopt_inside {
                " (deopt pads write back)"
            } else {
                ""
            }
        );
    }
    done
}

/// OSR5-2 (round 19 wave 6, lane osr6): the first of `deopt_nodes` (a
/// promoted loop's admitted deopt points) whose pad the lowering would refuse,
/// with the refusal's text, as far as the graph can tell:
///
/// * a point inside a SPLICED body (its pc, or a guard's frame bci, at or past
///   the compiling method's own code, `IrSpliceResumeMap::code_len`):
///   `Lowerer::begin_promoted_pending` describes no write-back there, since
///   the point resumes at the enclosing invoke (or publishes a chain) whose
///   memory state is not the one the plan tracks;
/// * unless the pads save the XMM homes
///   (`CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PAD_XMM_SAVE`), a point whose frame
///   (any snapshot at its bci) names a `float` / `double` value, which the
///   lowering may keep in an XMM register the pad's helper call can clobber
///   (`Lowerer::end_promoted_pending`'s `xmm=true`). Conservative: such a
///   value may be in a frame word instead. PROMOTE21-2 (lane promote): not
///   asked when `helper_free` -- the static promotion's pads are two inline
///   stores through RAX / RCX / R11 (`ir_lower::emit_static_cell_store`), no
///   call, so no XMM register can be clobbered.
///
/// `None` leaves the rest to the lowering, whose refusal then costs one retry
/// (`Graph::promote_pending_withheld`), not the method.
fn predicted_pad_refusal(
    graph: &Graph,
    deopt_nodes: &[NodeId],
    helper_free: bool,
) -> Option<(NodeId, &'static str)> {
    let splices = crate::ir::splice_resume_map_this_build();
    predicted_pad_refusal_in(graph, deopt_nodes, splices.as_ref(), helper_free)
}

/// [`predicted_pad_refusal`] against the build's splice map `splices`
/// (`None`: nothing was spliced).
fn predicted_pad_refusal_in(
    graph: &Graph,
    deopt_nodes: &[NodeId],
    splices: Option<&crate::ir::IrSpliceResumeMap>,
    helper_free: bool,
) -> Option<(NodeId, &'static str)> {
    // Wave 7 (lane osr7, OSR5-1): under the chain switch a spliced point may
    // take its pad (a chain point does; a flat one still refuses in the
    // lowering, which the retry absorbs), so it is not predicted here.
    let chain_pads = crate::ir_lower::ir_loop_field_promote_pending_chain_enabled();
    let spliced = |pc: usize| !chain_pads && splices.is_some_and(|m| pc >= m.code_len);
    let xmm_saved = helper_free || crate::ir_lower::ir_loop_field_promote_pad_xmm_save_enabled();
    let is_fp = |v: NodeId| {
        graph
            .node_opt(v)
            .is_some_and(|n| matches!(n.ty, IrType::Float | IrType::Double))
    };
    for &id in deopt_nodes {
        let Some(n) = graph.node_opt(id) else {
            continue;
        };
        let mut pcs: Vec<usize> = n.bytecode_pc.into_iter().collect();
        if let Op::Guard { bci } = n.op {
            pcs.push(bci);
        }
        if pcs.iter().any(|&pc| spliced(pc)) {
            return Some((id, "a deopt point inside a splice (predicted)"));
        }
        if !xmm_saved
            && graph.safepoints.iter().any(|sp| {
                pcs.contains(&sp.bci)
                    && sp
                        .locals
                        .iter()
                        .chain(sp.stack.iter())
                        .any(|&v| v != NO_NODE && is_fp(v))
            })
        {
            return Some((id, "a deopt frame naming a float or double (predicted)"));
        }
    }
    None
}

/// One cell's conditional write-back `if (value != init) base.f = value`,
/// chained after control `ctrl` and memory `mem`; returns the diamond's join
/// and its memory phi (whose first value input is the store). The store
/// never fires its null check (the base was dereferenced in the pre-header);
/// it names the header frame `si` only because the lowering asks every store
/// for one.
#[allow(clippy::too_many_arguments)]
fn add_cell_writeback(
    graph: &mut Graph,
    ctrl: NodeId,
    mem: NodeId,
    key: &FieldKey,
    init: NodeId,
    idx_node: NodeId,
    value: NodeId,
    header_bci: usize,
    si: u32,
) -> (NodeId, NodeId) {
    let pc = Some(header_bci);
    let cond = if key.kind == MemKind::Long {
        let three_way = graph.add(Op::LCmp, IrType::Int, vec![value, init], pc);
        let zero = graph.add(Op::Const(0), IrType::Int, vec![], None);
        graph.add(
            Op::Cmp(crate::ir::CmpOp::Ne),
            IrType::Int,
            vec![three_way, zero],
            pc,
        )
    } else {
        graph.add(
            Op::Cmp(crate::ir::CmpOp::Ne),
            IrType::Int,
            vec![value, init],
            pc,
        )
    };
    let iff = graph.add(Op::If, IrType::Control, vec![ctrl, cond], pc);
    let taken = graph.add(Op::Proj(0), IrType::Control, vec![iff], pc);
    let skipped = graph.add(Op::Proj(1), IrType::Control, vec![iff], pc);
    let store = graph.add(
        Op::Store(key.kind),
        IrType::Memory,
        vec![taken, mem, key.base, idx_node, value],
        pc,
    );
    graph.field_decl_class.insert(store, key.decl);
    graph.set_node_frame_snapshot(store, si);
    let join = graph.add(Op::Merge, IrType::Control, vec![taken, skipped], pc);
    let phi = graph.add(Op::Phi, IrType::Memory, vec![join, store, mem], pc);
    (join, phi)
}

/// `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_BODY_EXITS=1` (JIT round 19 proposal
/// wave 5, lane osr5, OSR2-4). **default on**. On top of
/// `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE`: a loop with exits from inside its
/// body (a `break` / an early `return` path) besides the header's own test is
/// promoted too; each exit gets its own conditional write-back of the cells'
/// values at that exit. Admitted only when every interior exit is reached
/// from the header without a join (`interior_exit_spine`), the body's writes
/// form one memory chain (`body_write_chain`), and every memory user outside
/// the loop is attributable to one exit (`outside_user_exit`). Read without a
/// `static` (`process_global_statics_ratchet`).
pub(crate) fn body_exits_enabled() -> bool {
    true
}

/// OSR2-4: the body control nodes on the path from the header `region` to
/// interior exit `x` (a `Proj` of a body `If`): every node from the `If`'s
/// control up to `region`, through `Proj`s of body `If`s and single-input
/// merges only. `None` when the path crosses a join (the memory state at the
/// exit would then depend on the path taken) or leaves the body.
fn interior_exit_spine(
    graph: &Graph,
    x: NodeId,
    region: NodeId,
    body: &FxHashSet<NodeId>,
) -> Option<FxHashSet<NodeId>> {
    let xn = graph.node_opt(x)?;
    if !matches!(xn.op, Op::Proj(_)) {
        return None;
    }
    let iff = xn.input_opt(0)?;
    if !body.contains(&iff) || graph.node_opt(iff)?.op != Op::If {
        return None;
    }
    let mut spine = FxHashSet::default();
    let mut c = graph.node_opt(iff)?.input_opt(0)?;
    for _ in 0..MAX_WALK {
        if !body.contains(&c) {
            return None;
        }
        spine.insert(c);
        if c == region {
            return Some(spine);
        }
        let cn = graph.node_opt(c)?;
        c = match cn.op {
            Op::Proj(_) => {
                let i = cn.input_opt(0)?;
                let inode = graph.node_opt(i)?;
                if inode.op != Op::If {
                    return None;
                }
                inode.input_opt(0)?
            }
            Op::Merge | Op::Region if cn.inputs.len() == 1 => cn.input_opt(0)?,
            _ => return None,
        };
    }
    None
}

/// OSR2-4: the body's writes as one chain from the header memory phi, in
/// program order. `None` when a write has two successors on the chain or a
/// body write is not on it.
fn body_write_chain(
    graph: &Graph,
    header_mem: NodeId,
    body: &FxHashSet<NodeId>,
) -> Option<Vec<NodeId>> {
    let is_write = |n: &crate::ir::Node| matches!(n.op, Op::Store(_) | Op::ArrayStore(_));
    let writes: Vec<NodeId> = body
        .iter()
        .copied()
        .filter(|&id| {
            graph
                .node_opt(id)
                .is_some_and(|n| n.op != Op::Dead && is_write(n))
        })
        .collect();
    let mut chain = Vec::with_capacity(writes.len());
    let mut m = header_mem;
    for _ in 0..=writes.len() {
        let next: Vec<NodeId> = writes
            .iter()
            .copied()
            .filter(|&w| {
                graph.node_opt(w).is_some_and(|n| {
                    crate::ir::memory_token_slot(n).and_then(|t| n.input_opt(t)) == Some(m)
                })
            })
            .collect();
        match next.as_slice() {
            [] => break,
            [w] => {
                chain.push(*w);
                m = *w;
            }
            _ => return None,
        }
    }
    (chain.len() == writes.len()).then_some(chain)
}

/// OSR2-4: which exit the outside memory use `(user, slot)` follows: `None`
/// for the header exit `exit`, `Some(k)` for `interior[k]`. A memory phi's
/// slot is attributed through its merge's predecessor control; any other
/// user through its own control. The walk goes up through `Proj` / `If` and
/// single-input merges only; anything else (a join of two exits) answers
/// `None` from the outer `Option`.
fn outside_user_exit(
    graph: &Graph,
    user: NodeId,
    slot: usize,
    exit: NodeId,
    interior: &[NodeId],
) -> Option<Option<usize>> {
    let un = graph.node_opt(user)?;
    let mut c = if un.op == Op::Phi {
        let merge = graph.node_opt(un.input_opt(0)?)?;
        merge.input_opt(slot.checked_sub(1)?)?
    } else {
        un.input_opt(0)?
    };
    for _ in 0..MAX_WALK {
        if c == exit {
            return Some(None);
        }
        if let Some(k) = interior.iter().position(|&x| x == c) {
            return Some(Some(k));
        }
        let cn = graph.node_opt(c)?;
        c = match cn.op {
            Op::Proj(_) => {
                let i = graph.node_opt(cn.input_opt(0)?)?;
                if i.op != Op::If {
                    return None;
                }
                i.input_opt(0)?
            }
            Op::Merge | Op::Region if cn.inputs.len() == 1 => cn.input_opt(0)?,
            _ => return None,
        };
    }
    None
}

/// `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PENDING_DEOPT=1` (JIT round 19
/// proposal wave 5, lane osr5, OSR2-3 / OSR4-1). **Default OFF**. On top of
/// `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE`: a loop whose body holds a guard that
/// can fire, or a primitive array read / write / length (null and bounds
/// deopts), is promoted too. Its in-loop cell stores are kept in the graph as
/// SILENT stores (`Graph::promoted_sunk_stores`): the lowerer emits nothing
/// for them, follows each cell's current value along the schedule, and makes
/// every deopt pad inside the loop write the cells back before the frame
/// leaves (`Lowerer::promoted_pending`, `emit_cell_writeback`). A deopt
/// point whose write-back cannot be described refuses the whole compile.
/// Read without a `static` (`process_global_statics_ratchet`).
pub(crate) fn pending_deopt_enabled() -> bool {
    false}

/// `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_JOIN_WALK=1` (JIT round 19 proposal
/// wave 8, lane osr8, OSR7-2). **default on**. Two things, both about the
/// memory φs at joins INSIDE a loop body (a cell store under a branch), which
/// `ir_optimize::loop_body` never admits (it adds only the header's φs):
///
/// * the promotion adds them to its body for EVERY loop, so a loop with a
///   store under a branch and no deopt point is promoted (its exit value is
///   the walk's, through a value φ at each join it crosses). Without the
///   switch such a loop is refused as `loop memory leaves past the header`,
///   whatever the wave-2 notes said;
/// * in a loop WITH a deopt point (under
///   `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PENDING_BRANCH`), it walks every body
///   memory φ, not only the ones on the exit walk's way, and keeps the value
///   φs it builds (`Graph::promoted_join_phis`, a dead-node root), so the
///   pending plan finds a value at a join an unconditional store of the same
///   cell follows (`ir_lower::pending_join_phi`) instead of refusing the
///   compile at a deopt point between the two.
pub(crate) fn join_walk_enabled() -> bool {
    true
}

// ── OSR4-2 (IROPS2-6): promote a loop's direct static cells to registers ──
//
// JIT round 20 wave 11 (lane pLoop), `CRATONVM_JIT_IR_LOOP_STATIC_PROMOTE`
// (default ON, `=0` restores the code before it exactly: nothing below runs
// and nothing is recorded for the lowering). The statics twin of the field
// promotion above, for the `COUNTER += x` loop: a non-volatile primitive
// static the loop both stores (and usually reads) is read ONCE in the
// pre-header, carried in a value φ through the loop, and written back by one
// conditional store at the exit.
//
// # Why the cell is exact
//
// A static cell has no base: `(class_id, field_index)` names it, and the only
// nodes that can touch it are a `getstatic` / `putstatic` of the same pair or
// a node that may run arbitrary code (a call, a helper-routed static access
// that may run `<clinit>`, a monitor). The census admits only DIRECT static
// accesses (`Graph::direct_static_access`: the class was initialised when the
// compile resolved it, so the lowering emits two `MOV`s and never the helper
// -- the same predicate `emit_inline_getstatic` / `emit_inline_putstatic`
// re-ask), refuses every other static access, every opaque or ordering node
// and every volatile access, so every write of the cell in the loop is one of
// its own `StoreStatic` nodes. No `CRATONVM_JIT_IR_STATIC_ALIAS_CLASS` or
// `CRATONVM_JIT_IR_LICM_DIRECT_STATIC` is needed: the pass classifies the
// direct accesses itself and never asks `Graph::memory_effect` about them.
//
// # The subset, and why each condition is there (the field promotion's)
//
// * An OSR-door compile (`Graph::osr_compile`): every entry arrives through
//   the opaque arm whose merge is the pre-header, so the pre-header read runs
//   on every entry. A direct read cannot trap and runs no `<clinit>`, so the
//   extra read on a zero-trip entry is unobservable.
// * NO TRANSFER INSIDE THE LOOP: every other body node is
//   `ir_lower::node_cannot_deopt`, so no deopt, exception or call leaves the
//   loop while memory is stale. The back-edge polls are the one transfer the
//   graph does not show: the header goes into
//   `Graph::promoted_cell_header_bcis`, so a poll's mode exit is refused --
//   unless the OSR2-1 pad (`CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_POLL_WRITEBACK`)
//   writes the cell back first, which for a static cell is the inline
//   `putstatic`'s two stores (`ir_lower::emit_cell_writeback`'s static arm,
//   no helper). A collection at a poll needs nothing (`int` / `long` cells).
//   JIT round 21 (TYPECHECK21-9, `CRATONVM_JIT_IR_LOOP_PROMOTE_DEOPT_PADS`, default ON):
//   a guard, primitive array access or integer division inside the loop is
//   admitted too, its pad writing the cells back first (see the
//   TYPECHECK21-9 section below `fresh_promote_refuse`).
// * ONE EXIT, the header's own test; every memory user outside the loop reads
//   the header's memory φ. The write-back is CONDITIONAL
//   (`if (v_exit != v_entry) STATIC = v_exit`) for the field promotion's JMM
//   reason: never write a value this thread only read over another thread's
//   store, and never write on a path where the program did not.
// * Other threads: no synchronisation action in the loop (calls, monitors,
//   volatile accesses refused), so a reader may legally see the value from
//   before the loop until the exit's write-back; `Thread.join` / a lock
//   after the loop orders the write-back before the reader. Class
//   initialisation is complete (the direct route), and no exception can
//   leave the loop between a deleted store and the write-back.
// * Stores under a branch: the body's join memory φs are walked (a value φ
//   at each, `StaticWalk::phi_value`), so `if (c) COUNTER++` is promoted too.
// * A header the field promotion already promoted is left alone (the field
//   census refuses every loop holding a static access, so the two never meet
//   in practice; the check keeps them disjoint by construction).

/// `CRATONVM_JIT_IR_LOOP_STATIC_PROMOTE` (JIT round 20 wave 11, lane pLoop,
/// OSR4-2 of `docs/feature-designs/deopt-osr.md`):
/// **default ON**, `=0` is the kill switch. Inert without
/// `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE` (the shared poll write-back machinery
/// and the same OSR-door placement). Read without a `static`
/// (`process_global_statics_ratchet`), once per optimize.
pub(crate) fn static_promote_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_LOOP_STATIC_PROMOTE_METHOD_ENTRY` (JIT round 20 wave 17,
/// lane pAtom2; perf, **default ON**, `=0` restores the OSR-door-only
/// placement): run [`promote_loop_statics`] in a METHOD-ENTRY body too. A
/// kernel called more than a handful of times runs its method-entry compile,
/// never the OSR-door body this pass was written for, so
/// `R20PLoopStaticCells`' counter rows measured the same on both arms of
/// `CRATONVM_JIT_IR_LOOP_STATIC_PROMOTE` (2.45 vs 2.47 ns; HotSpot 0.26).
///
/// Why the OSR-door premise is not needed for soundness, entry by entry:
///
/// * The method's own entry reaches the header through the pre-header, so it
///   runs the pre-header reads (as `forward_loop_fields_with`'s method-entry
///   use under `CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_PREDICATED` already relies on).
/// * An ordinary OSR entry stub of the method-entry body jumps to the header
///   itself, past the reads: the cell's value φ is then live across that entry
///   and named by no local, which `Lowerer::emit_osr_entry_stubs` refuses (it
///   is neither a constant nor a remat shape: the pre-header read is not in
///   `Graph::licm_hoisted_static_reads`), so that entry is simply not emitted --
///   the cost the OSR-door placement avoided, never a stale value.
/// * The back-edge poll's mode exit is decided by
///   `Graph::promoted_cell_header_bcis` for every body kind (the refusal, or the
///   OSR2-1 write-back pad), and the census admits no other transfer in the loop.
///
/// Read without a `static` (`process_global_statics_ratchet`), once per optimize.
pub(crate) fn static_promote_method_entry_enabled() -> bool {
    true
}

/// One promotable static cell: `(class_id, field_index)` and its descriptor
/// tag (`I`/`Z`/`B`/`C`/`S` or `J`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
struct StaticKey {
    class_id: u32,
    field: u32,
    tag: u8,
}

impl StaticKey {
    fn long(self) -> bool {
        self.tag == b'J'
    }

    fn ty(self) -> IrType {
        if self.long() {
            IrType::Long
        } else {
            IrType::Int
        }
    }
}

/// `(cell, is_write)` of a DIRECT, non-volatile static access the lowering
/// emits inline (`Graph::direct_static_access`), with its descriptor tag.
fn direct_static_cell(graph: &Graph, id: NodeId) -> Option<(StaticKey, bool)> {
    let node = graph.node_opt(id)?;
    let (class_id, field, tag) = match node.op {
        Op::LoadStatic {
            class_id,
            field_index,
            type_tag,
            is_volatile: false,
        }
        | Op::StoreStatic {
            class_id,
            field_index,
            type_tag,
            is_volatile: false,        } => (class_id, field_index, type_tag),
        _ => return None,
    };
    let (_, is_write) = graph.direct_static_access(node)?;
    Some((
        StaticKey {
            class_id,
            field,
            tag,
        },
        is_write,
    ))
}

/// Promote the static cells this section describes, loop by loop. Returns
/// how many cells were promoted.
pub(super) fn promote_loop_statics(graph: &mut Graph) -> usize {
    if graph.direct_static_fields.is_empty() {
        return 0;
    }
    if graph.nodes.iter().any(|n| {
        n.op != Op::Dead
            && n.op.memory_shape().is_some()
            && crate::ir::memory_token_slot(n).is_none()
    }) {
        return 0;
    }
    let mut promoted = 0usize;
    let mut tried: FxHashSet<NodeId> = FxHashSet::default();
    // A promotion rewires its exit edge, which may be the next loop's entry:
    // the loop list is taken again after each one.
    loop {
        let loops = loop_headers(graph);
        let headers: FxHashSet<NodeId> = loops.iter().map(|l| l.0).collect();
        let Some((region, entry_pred, back_ctrls)) =
            loops.into_iter().find(|l| !tried.contains(&l.0))
        else {
            break;
        };
        tried.insert(region);
        // j27 (lane FRAMES): collect the pre-header copy a refusal left.
        promoted += super::orphan_frames::collecting_orphaned_frames(graph, |graph| {
            promote_static_loop(graph, region, entry_pred, &back_ctrls, &headers)
        });
    }
    if promoted > 0 && crate::x64::dbg_jitc_enabled() {
        eprintln!(
            "[ir] loop static promote: {} {promoted} static cell(s) kept in registers",
            graph.scope_method_key
        );
    }
    promoted
}

/// `CRATONVM_DBG_LOOP_FIELD_FORWARD=1`: why a loop's static cells were not
/// promoted.
fn static_promote_refuse(header_bci: usize, why: &str, node: NodeId) -> usize {
    if false
        || super::loop_pass_report_enabled()
    {
        eprintln!("[ir] loop static promote: header bci {header_bci} refused -- {why} (n{node})");
    }
    0
}

/// The value of one promoted static cell at a memory state inside the loop.
struct StaticWalk<'a> {
    key: StaticKey,
    /// The header's forward predecessor slot (its φs' input `fwd + 1`).
    fwd: usize,
    header_mem: NodeId,
    init: NodeId,
    body: &'a FxHashSet<NodeId>,
    memo: FxHashMap<NodeId, NodeId>,
    budget: usize,
}

impl StaticWalk<'_> {
    fn value_at(&mut self, graph: &mut Graph, mem: NodeId) -> Option<NodeId> {
        let mut path: Vec<NodeId> = Vec::new();
        let mut cur = mem;
        let answer = loop {
            if let Some(&v) = self.memo.get(&cur) {
                break v;
            }
            self.budget = self.budget.checked_sub(1)?;
            let node = graph.node_opt(cur)?.clone();
            if cur == self.header_mem || (node.op == Op::Phi && node.ty == IrType::Memory) {
                break self.phi_value(graph, cur, &node)?;
            }
            // The chain may only leave the loop through its header φ.
            if !self.body.contains(&cur) {
                return None;
            }
            if let Some((key, true)) = direct_static_cell(graph, cur) {
                if key.class_id == self.key.class_id && key.field == self.key.field {
                    // `[ctrl, mem, value]`: the value is already narrowed to
                    // the field's type (the builder's `narrow_to_field_type`).
                    break node.input_opt(2)?;
                }
            }
            let token = crate::ir::memory_token_slot(&node).and_then(|t| node.input_opt(t))?;
            path.push(cur);
            cur = token;
        };
        for p in path {
            self.memo.insert(p, answer);
        }
        Some(answer)
    }

    /// A value φ over the memory φ `phi`, in the same predecessor order. At
    /// the loop header the forward edge carries the pre-header read.
    fn phi_value(
        &mut self,
        graph: &mut Graph,
        phi: NodeId,
        node: &crate::ir::Node,
    ) -> Option<NodeId> {
        let ctrl = node.input_opt(0)?;
        if phi != self.header_mem && !self.body.contains(&ctrl) {
            return None;
        }
        let ty = self.key.ty();
        let preds: Vec<NodeId> = node.inputs.as_slice().get(1..)?.to_vec();
        let mut inputs = vec![ctrl];
        inputs.extend(std::iter::repeat(NO_NODE).take(preds.len()));
        // Placeholders first, so a cycle back to this φ (a back edge) finds it
        // in the memo.
        let value = graph.add(Op::Phi, ty, inputs, node.bytecode_pc);
        self.memo.insert(phi, value);
        for (k, pred_mem) in preds.into_iter().enumerate() {
            let v = if phi == self.header_mem && k == self.fwd {
                self.init
            } else {
                self.value_at(graph, pred_mem)?
            };
            graph.set_input(value, k + 1, v);
        }
        Some(super::dedup_walk_phi(graph, ctrl, value, &mut self.memo))
    }
}

/// The memory-token input slots of `user` that name `of`.
fn token_slots_naming(graph: &Graph, user: NodeId, of: NodeId) -> Vec<usize> {
    let Some(node) = graph.node_opt(user) else {
        return Vec::new();
    };
    node.inputs
        .as_slice()
        .iter()
        .enumerate()
        .filter(|&(i, &x)| x == of && crate::ir::is_memory_token_slot(node, i))
        .map(|(i, _)| i)
        .collect()
}

/// Close the memory chain over `node` (a read or a deleted write): every
/// memory user takes `node`'s own incoming token.
fn bypass_token(graph: &mut Graph, node: NodeId) -> bool {
    let Some(token) = graph
        .node_opt(node)
        .and_then(|n| crate::ir::memory_token_slot(n).and_then(|t| n.input_opt(t)))
    else {
        return false;
    };
    for user in graph.users_of(node) {
        for slot in token_slots_naming(graph, user, node) {
            graph.set_input(user, slot, token);
        }
    }
    true
}

fn promote_static_loop(
    graph: &mut Graph,
    region: NodeId,
    entry_pred: NodeId,
    back_ctrls: &[NodeId],
    headers: &FxHashSet<NodeId>,
) -> usize {
    let Some(header_bci) = graph.node_opt(region).and_then(|n| n.bytecode_pc) else {
        return 0;
    };
    if graph.promoted_cell_header_bcis.contains(&header_bci) {
        return static_promote_refuse(header_bci, "a header the field promotion took", region);
    }
    let Some(fwd) = graph
        .node_opt(region)
        .and_then(|n| n.inputs.iter().position(|&c| c == entry_pred))
    else {
        return 0;
    };
    let mem_phis: Vec<NodeId> = graph
        .users_of(region)
        .into_iter()
        .filter(|&u| {
            graph.node_opt(u).is_some_and(|n| {
                n.op == Op::Phi && n.ty == IrType::Memory && n.input_opt(0) == Some(region)
            })
        })
        .collect();
    let [header_mem] = mem_phis.as_slice() else {
        return static_promote_refuse(header_bci, "not exactly one header memory phi", region);
    };
    let header_mem = *header_mem;
    let mut body = loop_body(graph, region, entry_pred, back_ctrls);
    if headers.iter().any(|&h| h != region && body.contains(&h)) {
        return static_promote_refuse(header_bci, "an inner loop", region);
    }
    // The φs at joins INSIDE the body (a store under a branch): `loop_body`
    // admits only the header's. Walked like the header's (`phi_value`).
    let inner: Vec<NodeId> = graph
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| {
            n.op == Op::Phi
                && n.input_opt(0)
                    .is_some_and(|c| c != region && body.contains(&c))
        })
        // Cast: arena index to node id.
        .map(|(id, _)| id as NodeId)
        .collect();
    body.extend(inner);

    // The one exit: the header's own test.
    let mut exits: Vec<NodeId> = Vec::new();
    for (id, n) in graph.nodes.iter().enumerate() {
        // Cast: arena index to node id.
        let id = id as NodeId;
        if n.op == Op::Dead || !n.op.is_control() || body.contains(&id) {
            continue;
        }
        if n.inputs.iter().any(|&i| i != NO_NODE && body.contains(&i)) {
            exits.push(id);
        }
    }
    let [exit] = exits.as_slice() else {
        return static_promote_refuse(header_bci, "not exactly one exit edge", region);
    };
    let exit = *exit;
    let exit_from_header = graph.node_opt(exit).is_some_and(|x| {
        matches!(x.op, Op::Proj(_))
            && x.input_opt(0)
                .and_then(|i| graph.node_opt(i))
                .is_some_and(|i| i.op == Op::If && i.input_opt(0) == Some(region))
    });
    if !exit_from_header {
        return static_promote_refuse(header_bci, "the exit is not the header's test", exit);
    }

    // Census: nothing in the loop may transfer to the interpreter, and every
    // write of a static cell is a direct `putstatic`. JIT round 21 (lane
    // promote, TYPECHECK21-9): unless a transfer is a deopt point whose pad
    // writes the cells back first (`deopt_pads_admitted`).
    let pads = deopt_pads_admitted(graph);
    let mut deopt_nodes: Vec<NodeId> = Vec::new();
    let mut body_nodes: Vec<NodeId> = body.iter().copied().collect();
    body_nodes.sort_unstable();
    let mut loads: FxHashMap<StaticKey, Vec<NodeId>> = FxHashMap::default();
    let mut stores: FxHashMap<StaticKey, Vec<NodeId>> = FxHashMap::default();
    for &id in &body_nodes {
        let Some(node) = graph.node_opt(id) else {
            continue;
        };
        if node.op == Op::Dead || node.op == Op::Phi {
            continue;
        }
        if node.is_volatile_access() {
            return static_promote_refuse(header_bci, "a volatile access", id);
        }
        if let Some((key, is_write)) = direct_static_cell(graph, id) {
            let map = if is_write { &mut stores } else { &mut loads };
            map.entry(key).or_default().push(id);
            continue;
        }
        if matches!(node.op, Op::LoadStatic { .. } | Op::StoreStatic { .. }) {
            // A volatile one, or one the lowering routes through its helper
            // (which may run `<clinit>`, i.e. any code).
            return static_promote_refuse(header_bci, "a static access that is not direct", id);
        }
        // JIT round 21 (lane loopopt, LOOP21-1): as in the fresh-object census.
        if nonnull_invariant_read_cannot_transfer(graph, id, &body) {
            continue;
        }
        let eff = graph.memory_effect(id);
        if eff.order != MemOrder::Plain || matches!(eff.writes, AliasClass::Any) {
            return static_promote_refuse(header_bci, "an ordering or opaque write", id);
        }
        if !crate::ir_lower::node_cannot_deopt(graph, node) {
            // TYPECHECK21-9: a deopt point the pads can serve.
            if pads && pad_admitted_deopt_point(graph, id) {
                deopt_nodes.push(id);
                continue;
            }
            note_pads_withheld(graph, id);
            note_unadmitted_deopt_point(graph, header_bci, id);
            return static_promote_refuse(header_bci, "a node that can deopt", id);
        }
    }
    // TYPECHECK21-9: the pads' own refusals the graph already shows
    // (`deopt_point_refusal`), before anything is rewritten.
    if let Some((id, why)) = deopt_point_refusal(graph, &deopt_nodes, &body, header_mem, true) {
        return static_promote_refuse(header_bci, why, id);
    }
    // The cells: a primitive `int`-category or `long` static the loop stores.
    // A cell written under two tags is not one cell (cannot happen for a
    // resolved field; refused rather than assumed).
    let mut keys: Vec<StaticKey> = stores
        .keys()
        .copied()
        .filter(|k| matches!(k.tag, b'I' | b'Z' | b'B' | b'C' | b'S' | b'J'))
        .filter(|k| {
            stores
                .keys()
                .chain(loads.keys())
                .all(|o| (o.class_id, o.field) != (k.class_id, k.field) || o == k)
        })
        .collect();
    keys.sort_unstable();
    if keys.is_empty() {
        return static_promote_refuse(header_bci, "no promotable static cell", region);
    }

    // Every memory user outside the loop must read the header's memory φ.
    let mut outside: Vec<(NodeId, usize)> = Vec::new();
    for (id, n) in graph.nodes.iter().enumerate() {
        // Cast: arena index to node id.
        let id = id as NodeId;
        if n.op == Op::Dead || body.contains(&id) || id == header_mem {
            continue;
        }
        for (i, &inp) in n.inputs.iter().enumerate() {
            if inp == NO_NODE || !(inp == header_mem || body.contains(&inp)) {
                continue;
            }
            if !crate::ir::is_memory_token_slot(n, i) {
                continue;
            }
            if inp != header_mem {
                return static_promote_refuse(header_bci, "loop memory leaves past the header", id);
            }
            outside.push((id, i));
        }
    }
    let Some(si) = promotion_header_frame(graph, header_bci) else {
        return static_promote_refuse(header_bci, "no header frame", region);
    };
    // The pre-header's frame, for the entry reads (the lowering never asks a
    // direct read for one; it is there because every read of the forward
    // carries one).
    let Some(pre_si) = preheader_snapshot(graph, region, fwd, header_bci, &body) else {
        return static_promote_refuse(header_bci, "no pre-header frame", region);
    };

    // The pre-header reads, threaded onto the forward memory edge.
    let Some(mut mem) = graph
        .node_opt(header_mem)
        .and_then(|n| n.input_opt(fwd + 1))
    else {
        return 0;
    };
    let mut initial: Vec<(StaticKey, NodeId)> = Vec::with_capacity(keys.len());
    for &key in &keys {
        let read = graph.add(
            Op::LoadStatic {
                class_id: key.class_id,
                field_index: key.field,
                type_tag: key.tag,
                is_volatile: false,
            },
            key.ty(),
            vec![entry_pred, mem],
            Some(header_bci),
        );
        if !graph.set_node_frame_snapshot(read, pre_si) {
            // Unreachable: the read's pc is the snapshot's bci.
            graph.kill(read);
            continue;
        }
        mem = read;
        initial.push((key, read));
    }
    if initial.is_empty() {
        return 0;
    }
    graph.set_input(header_mem, fwd + 1, mem);

    // Each cell's in-loop reads and its value at the exit, walked before any
    // node is deleted (the walk answers a store with its value). A cell whose
    // walk fails keeps its loads and stores (its pre-header read stays: one
    // extra direct read, never a wrong one; the cleanup reclaims the φs).
    let mut ready: Vec<(StaticKey, NodeId, NodeId, Vec<(NodeId, NodeId)>)> = Vec::new();
    // PROMOTE21-1: with a deopt point inside, the body's memory φs (none
    // unless `deopt_point_refusal` admitted them) and their value φs.
    let join_mem_phis = if deopt_nodes.is_empty() {
        Vec::new()
    } else {
        body_memory_phis(graph, &body, header_mem)
    };
    let mut join_values: Vec<NodeId> = Vec::new();
    for &(key, init) in &initial {
        let mut walk = StaticWalk {
            key,
            fwd,
            header_mem,
            init,
            body: &body,
            memo: FxHashMap::default(),
            budget: MAX_WALK,
        };
        let Some(exit_value) = walk.value_at(graph, header_mem) else {
            continue;
        };
        let mut answers: Vec<(NodeId, NodeId)> = Vec::new();
        let mut failed = false;
        for &ld in loads.get(&key).map(Vec::as_slice).unwrap_or(&[]) {
            let token = graph
                .node_opt(ld)
                .and_then(|n| crate::ir::memory_token_slot(n).and_then(|t| n.input_opt(t)));
            match token.and_then(|t| walk.value_at(graph, t)) {
                Some(v) => answers.push((ld, v)),
                None => {
                    failed = true;
                    break;
                }
            }
        }
        // PROMOTE21-1: every join's value, for the pads (a cell any join
        // walk fails for keeps its loads and stores).
        if !failed && !join_mem_phis.is_empty() {
            failed = join_mem_phis
                .iter()
                .any(|&m| walk.value_at(graph, m).is_none());
            if !failed {
                join_values.extend(pad_join_phis(graph, &walk.memo, &join_mem_phis));
            }
        }
        if failed {
            continue;
        }
        ready.push((key, init, exit_value, answers));
    }
    if ready.is_empty() {
        return static_promote_refuse(header_bci, "no exit value", region);
    }

    // The write-backs: one `if (v != init) STATIC = v` diamond per cell,
    // chained on the exit edge.
    let exit_users: Vec<NodeId> = graph.users_of(exit);
    let mut ctrl = exit;
    let mut wmem = header_mem;
    // TYPECHECK21-9: each cell's exit store, which names the cell to the
    // pending plan when its in-loop stores are kept silent.
    let mut exit_stores: Vec<NodeId> = Vec::with_capacity(ready.len());
    for &(key, init, value, _) in &ready {
        let (join, phi, store) =
            add_static_cell_writeback(graph, ctrl, wmem, key, init, value, header_bci, si);
        ctrl = join;
        wmem = phi;
        // The lowerer finds the cell from this store when it gives the
        // header's poll a write-back exit (OSR2-1's pad, static arm).
        graph.promoted_cell_writebacks.push((region, store));
        exit_stores.push(store);
    }
    for user in exit_users {
        let Some(n) = graph.node_opt(user) else {
            continue;
        };
        let slots: Vec<usize> = n
            .inputs
            .iter()
            .enumerate()
            .filter(|&(_, &x)| x == exit)
            .map(|(i, _)| i)
            .collect();
        for slot in slots {
            graph.set_input(user, slot, ctrl);
        }
    }
    for (user, slot) in outside {
        graph.set_input(user, slot, wmem);
    }

    // The in-loop reads take their values; a read may be answered by another
    // removed read (`S = S`): resolve through the map first.
    let map: FxHashMap<NodeId, NodeId> = ready.iter().flat_map(|r| r.3.iter().copied()).collect();
    let resolve = |mut v: NodeId| {
        let mut seen = 0usize;
        while let Some(&next) = map.get(&v) {
            v = next;
            seen += 1;
            if seen > map.len() {
                return NO_NODE;
            }
        }
        v
    };
    let mut answered: Vec<(NodeId, NodeId)> = Vec::new();
    for (ld, v) in map.iter().map(|(&ld, &v)| (ld, v)) {
        answered.push((ld, resolve(v)));
    }
    answered.sort_unstable();
    for (ld, v) in answered {
        if v == NO_NODE || v == ld || !bypass_token(graph, ld) {
            // Unreachable for a walked read; such a read stays a read of a
            // cell whose stores are about to go, so keep the stores too.
            continue;
        }
        graph.replace_all_uses(ld, v);
        graph.kill(ld);
    }
    // The in-loop stores of the promoted cells go: their memory users take
    // the store's own token. TYPECHECK21-9: with a deopt point inside they
    // stay as SILENT stores instead (`sink_or_keep_silent`).
    let mut done = 0usize;
    for (&(key, _, _, _), &exit_store) in ready.iter().zip(&exit_stores) {
        let own = stores.get(&key).cloned().unwrap_or_default();
        sink_or_keep_silent(graph, &own, exit_store, !deopt_nodes.is_empty());
        done += 1;
    }
    if !graph.promoted_cell_header_bcis.contains(&header_bci) {
        graph.promoted_cell_header_bcis.push(header_bci);
    }
    root_pad_join_phis(graph, join_values);
    note_pad_loop(deopt_nodes.len());
    if crate::x64::dbg_jitc_enabled() {
        eprintln!(
            "[ir] loop static promote: header bci {header_bci}: {done} static cell(s), stores sunk to the exit{}",
            pads_note(deopt_nodes.len())
        );
    }
    done
}

/// One static cell's conditional write-back `if (value != init) STATIC =
/// value`, chained after control `ctrl` and memory `mem`; returns the
/// diamond's join, its memory φ and the store. The store is a direct
/// `putstatic` (the census admitted only direct ones, and the lowering asks
/// the same resolver); it names the header frame `si` only because a helper
/// route, were it ever taken, would want one.
#[allow(clippy::too_many_arguments)]
fn add_static_cell_writeback(
    graph: &mut Graph,
    ctrl: NodeId,
    mem: NodeId,
    key: StaticKey,
    init: NodeId,
    value: NodeId,
    header_bci: usize,
    si: u32,
) -> (NodeId, NodeId, NodeId) {
    let pc = Some(header_bci);
    let cond = if key.long() {
        let three_way = graph.add(Op::LCmp, IrType::Int, vec![value, init], pc);
        let zero = graph.add(Op::Const(0), IrType::Int, vec![], None);
        graph.add(
            Op::Cmp(crate::ir::CmpOp::Ne),
            IrType::Int,
            vec![three_way, zero],
            pc,
        )
    } else {
        graph.add(
            Op::Cmp(crate::ir::CmpOp::Ne),
            IrType::Int,
            vec![value, init],
            pc,
        )
    };
    let iff = graph.add(Op::If, IrType::Control, vec![ctrl, cond], pc);
    let taken = graph.add(Op::Proj(0), IrType::Control, vec![iff], pc);
    let skipped = graph.add(Op::Proj(1), IrType::Control, vec![iff], pc);
    let store = graph.add(
        Op::StoreStatic {
            class_id: key.class_id,
            field_index: key.field,
            type_tag: key.tag,
            is_volatile: false,
        },
        IrType::Memory,
        vec![taken, mem, value],
        pc,
    );
    graph.set_node_frame_snapshot(store, si);
    let join = graph.add(Op::Merge, IrType::Control, vec![taken, skipped], pc);
    let phi = graph.add(Op::Phi, IrType::Memory, vec![join, store, mem], pc);
    (join, phi, store)
}

// ── jr20 pAtom2: promote a FRESH, unpublished object's loop cells ─────────
//
// JIT round 20 wave 17 (lane pAtom2; page
// `jr20-loop-accumulator-fields-of-a-fresh-escaping-object-stay-in-memory-CLOSED-20261012.md`),
// `CRATONVM_JIT_IR_LOOP_FRESH_FIELD_PROMOTE` (default ON, `=0` restores the
// code before it exactly: nothing below runs). The accumulator shape
//
// ```java
// Acc a = new Acc();
// for (int i = 0; i < n; i++) { a.v += k * SCALE + i; a.w += bias; }
// return a;
// ```
//
// which neither the field forward (a base allocated by this compile is left
// to escape analysis) nor escape analysis (the object escapes through the
// `return`) takes: two loop-carried memory chains per iteration. Here the
// `int` / `long` fields of such an object are read once in the pre-header,
// carried in value φs through the loop, and written back at the exit; the
// in-loop loads and stores go. HotSpot C2 does the same
// (`PhaseIdealLoop::try_move_store_after_loop` / `..._before_loop`).
//
// # Why the cell is exact: the base is never published
//
// The base `B` is an `Op::New` / `Op::NewCp` of this compile, defined outside
// the loop, and EVERY use of it in the whole graph is one of:
//
// * the BASE operand of a field `Load` / `Store` (input 2) -- never the
//   stored value, never an index;
// * a memory-token operand (the allocation is also a memory state);
// * the value of an `Op::Return` (input 1): a return ends the activation, so
//   it runs on a path that never reaches the loop or after the loop's exit;
// * a pure test of the reference that keeps no copy of it (`Cmp`,
//   `ExactClassIs`, `ClassWordOf`).
//
// So no call, store, array, φ, monitor or other thread ever holds `B` before
// the activation returns: no node of the graph other than `B` itself can
// evaluate to `B`, a load or store through any other base never touches its
// fields, and nothing outside this activation can observe them. Any other use
// -- a constructor CALL that was not spliced, a store of `B` anywhere, a φ --
// refuses the base (`fresh base may be published`), wherever it sits.
//
// # Why memory may be stale inside the loop (the static promotion's census)
//
// * NO TRANSFER INSIDE THE LOOP: every other body node is
//   `ir_lower::node_cannot_deopt` with plain ordering and no `Any` write (no
//   call, no monitor, no volatile access), so no deopt, exception or call
//   leaves the loop while the cells live in registers. The deleted accesses
//   are `B`'s own (a fresh allocation is never null; the offsets are
//   constants). The back-edge polls are the one transfer the graph does not
//   show: the header goes into `Graph::promoted_cell_header_bcis`, so a
//   poll's mode exit is refused, or (OSR2-1 pad,
//   `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_POLL_WRITEBACK`) writes the cells
//   back first from `Graph::promoted_cell_writebacks`. A collection at a poll
//   needs nothing: the cells are `int` / `long`.
//   JIT round 21 (TYPECHECK21-9, `CRATONVM_JIT_IR_LOOP_PROMOTE_DEOPT_PADS`, default ON):
//   or a deopt point whose pad writes the cells back first (the TYPECHECK21-9
//   section below).
// * ONE EXIT, the header's own test, and every memory user outside the loop
//   reads the header's memory φ, which is rewired to the write-backs' join:
//   every later read of a field (and every frame state past the loop, whose
//   heap is memory) sees the written-back value.
// * Entries: the method entry (and an OSR door's opaque arm) reaches the
//   header through the pre-header reads. An ordinary OSR entry stub at the
//   header leaves the cells' value φs live and named by no local, which
//   `Lowerer::emit_osr_entry_stubs` refuses, so that entry is not emitted.
//
// The write-back reuses the field promotion's conditional store
// (`add_cell_writeback`); with an unpublished base it could be unconditional,
// the conditional form is kept so the pad and the lowering see one shape.

/// `CRATONVM_JIT_IR_LOOP_FRESH_FIELD_PROMOTE` (JIT round 20 wave 17, lane
/// pAtom2): **default ON**, `=0` is the kill switch. Inert without
/// `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE` (the shared write-back and poll
/// machinery). Runs in method-entry and OSR-door bodies alike (see the
/// section comment). Read without a `static` (`process_global_statics_ratchet`),
/// once per optimize.
pub(crate) fn fresh_promote_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_FRESH_PROMOTE_STATIC_FEED` (JIT round 21 wave 1, lane
/// loops, LOOPS21-3; consistency, **default ON**, `=0` restores the old feed):
/// does `lib.rs` fill `Graph::direct_static_fields` on behalf of
/// [`promote_fresh_loop_fields`]? That pass admits a body `getstatic` only
/// through `direct_static_cell`, which reads the list; the feed used to be
/// asked only for the static hoist, the static promotion and the LICM read
/// exemption, so with those three at `=0` the fresh promotion refused every
/// loop holding a static read (`an ordering or opaque write`). `true` only
/// while the fresh promotion itself is on. Read once per compile, no `static`.
pub(crate) fn fresh_promote_static_feed_enabled() -> bool {
    fresh_promote_enabled()
}

/// `CRATONVM_DBG_LOOP_FIELD_FORWARD=1`: why a loop's fresh-object cells were
/// not promoted.
///
/// JIT round 21 (lane loopopt, LOOP21-1): names the compiling method
/// (`Graph::scope_method_key`, set when the body splices anything) and the
/// refusing node's op, so a log filtered by method name keeps the line.
fn fresh_promote_refuse(graph: &Graph, header_bci: usize, why: &str, node: NodeId) -> usize {
    if false
        || super::loop_pass_report_enabled()
    {
        eprintln!(
            "[ir] loop fresh field promote: {} header bci {header_bci} refused -- {why} (n{node} {:?})",
            graph.scope_method_key,
            graph.node_opt(node).map(|n| &n.op)
        );
    }
    0
}

/// `CRATONVM_JIT_IR_LOOP_PROMOTE_NONNULL_READS` (JIT round 21, lane loopopt,
/// LOOP21-1; perf, **opt-in** since 2026-10-26, `1` turns it on): the static and
/// fresh-object promotions admit, inside the promoted loop, a plain field READ
/// whose base is defined outside the loop and known non-null on entry
/// ([`nonnull_invariant_read_cannot_transfer`]). `R20PLoopStaticBarrier.run`
/// was refused at exactly such a node on host w1f (`a node that can deopt
/// (n34)`): a `getfield` of `this` the loop did not hoist. Read per node.
///
/// Opt-in after the 2026-10-26 host check (`jr21-typecheck-lane-perf-rows`,
/// retired): `run`'s method-entry body now promotes its two fresh cells with
/// the switch off as well (the receiver reads are hoisted before the census),
/// and the in-process instruction counts of every row it was meant for
/// (`run`, `runStatic`, the static counter loops, method-entry and OSR) were
/// equal on both arms. A default-ON switch that changes no measured body only
/// widens what the promotions admit.
fn nonnull_reads_enabled() -> bool {
    false}

/// Can the `Op::Load` `id` in a promoted loop never leave it? The promotions
/// need "no transfer inside the loop" -- memory is stale between a deleted
/// store and the exit's write-back, and a frame names no pending heap write --
/// and refuse every node `ir_lower::node_cannot_deopt` does not clear, which
/// never clears a `Load`. A plain field read leaves the compiled code in only
/// three ways, each closed here:
///
/// * its null check (the inline arms' `TEST; deopt`, the `jit_getfield`
///   helper's `i64::MIN` sentinel into the exception exit, the implicit-edge
///   NPE pad): the base is the method's receiver or a node
///   `ir_check_elim::definitely_non_null` vouches for
///   (`ir_optimize::base_non_null_on_entry`, the predicate LICM's speculative
///   hoist already trusts), defined OUTSIDE the loop, so the same non-null
///   object on every trip;
/// * the layout-epoch guard, which deopts only under the opt-in
///   `CRATONVM_JIT_IR_LAYOUT_GUARD_DEOPT` (refused then; by default a failed
///   guard takes the helper and rejoins);
/// * nothing else: a `getfield` runs no Java, takes no lock and is no
///   safepoint the poll machinery would turn into a mode exit; a collection
///   inside the helper needs nothing from the promoted `int` / `long` cells.
///
/// The read itself is untouched (it still runs every trip, against current
/// memory), and it cannot read a promoted cell: a fresh object's fields are
/// read through the fresh base only (the receiver or another non-null base
/// defined before the allocation is a different object: the fresh base is
/// unpublished), and a static cell is not a field. A volatile read is refused
/// (an ordering node).
fn nonnull_invariant_read_cannot_transfer(
    graph: &Graph,
    id: NodeId,
    body: &FxHashSet<NodeId>,
) -> bool {
    let Some(node) = graph.node_opt(id) else {
        return false;
    };
    if !matches!(node.op, Op::Load(_)) || node.is_volatile_access() {
        return false;
    }
    let Some((base, _, _)) = field_access(graph, id) else {
        return false;
    };
    nonnull_reads_enabled()
        && !body.contains(&base)
        && super::base_non_null_on_entry(graph, base)
        && !false}

// ── TYPECHECK21-9: deopt points inside a static / fresh promoted loop ────────
//
// JIT round 21 (lane promote), `CRATONVM_JIT_IR_LOOP_PROMOTE_DEOPT_PADS`
// (default ON, `=0` off; `ir_lower::ir_loop_promote_deopt_pads_enabled`). The two
// promotions above refused every loop holding a node that can deopt, because
// memory is stale between a deleted in-loop store and the exit's write-back.
// HotSpot promotes across an uncommon trap by describing the promoted value
// in the trap's state; here the trap's PAD makes memory exact instead, with
// the field promotion's round-19 machinery (OSR2-3,
// `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PENDING_DEOPT`):
//
// * the in-loop stores of a promoted cell are KEPT as silent stores
//   (`Graph::promoted_sunk_stores`, keyed by the cell's exit write-back
//   store): the lowering emits nothing for them, but they keep their place in
//   the schedule against every deopt point (`ir_schedule` never swaps a
//   memory-ordering node with a safepoint) and their values stay live;
// * the lowering's pending plan (`ir_lower::plan_promoted_pending`) follows
//   each cell's current value along the final schedule -- the header value φ
//   at the header, the stored value after each silent store -- and every
//   deopt pad emitted inside the loop runs `if (cur != init) cell = cur`
//   before `MOV DEOPT_ARG0` (`Lowerer::describe_pending_writebacks`): the
//   field helper for a fresh-object cell, the inline `putstatic`'s two words
//   for a static cell (`ir_lower::emit_static_cell_store`). The frame then
//   leaves, and the interpreter re-executes the trapping bytecode over exactly
//   the memory the unpromoted loop would have left. So an exception the
//   bytecode then throws (`ArrayIndexOutOfBoundsException`,
//   `ArithmeticException`) is raised after the cells are stored: a handler,
//   in this method or a caller, reads the current values.
// * The back-edge poll's mode exit keeps OSR2-1's pad
//   (`promoted_cell_poll_writebacks`), which reads the back edge's value.
//
// Admitted deopt points (`pad_admitted_deopt_point`): a guard, a plain
// primitive array element read / write or length (null and bounds pads), an
// integer division or remainder (its zero test). Each runs no Java, takes no
// lock, writes no field or static cell, and is no synchronisation action; a
// call, a monitor, a volatile or ordering access, an `Any` write and every
// other trapping node are still refused. Memory model: unchanged from the
// deopt-free promotions -- a non-volatile static (a fresh object is not even
// reachable by another thread) may stay stale until the exit or a pad, which
// HotSpot's promotion allows too, and a pad's write is conditional exactly as
// the exit's (never a value this thread only read back over another thread's
// write).
//
// Fail closed, at every level the field promotion has, plus one:
//
// * here (`deopt_point_refusal`): a body memory φ (a cell store under a
//   branch would need a value φ the plan reads only under
//   `..._PENDING_BRANCH`; not taken here), a deopt point inside a spliced
//   body or a frame naming a float / double (`predicted_pad_refusal`, asked
//   unconditionally);
// * in the lowering: a plan the schedule refutes, a deopt point whose pad
//   cannot describe every cell (a base not in a reference frame word, a value
//   not located, a static cell whose inline route is off), a rethrow pad, or
//   a point outside the per-node check refuses the compile
//   (`promoted field cells: ...`), and the IR door rebuilds ONCE without such
//   promotions (`Graph::promote_pending_withheld`, `lib.rs` `ir_tier`), which
//   gives back exactly today's refusal;
// * a cell whose exit store is missing keeps its in-loop stores REAL (never
//   silent without a record), so memory stays exact at every point anyway.
//
// The unroller never copies such a loop: its silent stores are stores, which
// `unroll` refuses as side effects.

/// TYPECHECK21-9: may the static / fresh promotions admit deopt points in
/// this compile? Never on the IR door's retry after a lowering refused a
/// pad (`Graph::promote_pending_withheld`).
fn deopt_pads_admitted(graph: &Graph) -> bool {
    crate::ir_lower::ir_loop_promote_deopt_pads_enabled() && !graph.promote_pending_withheld
}

/// TYPECHECK21-9: is `id` (a node `ir_lower::node_cannot_deopt` does not
/// clear) a deopt point a write-back pad can serve? A guard, a plain
/// primitive array element read / write or length, an integer division or
/// remainder. None of them runs Java, takes a lock or writes a field or static
/// cell; whatever they publish is checked again by the lowering's per-node
/// pad check (`Lowerer::end_promoted_pending`).
fn pad_admitted_deopt_point(graph: &Graph, id: NodeId) -> bool {
    let Some(node) = graph.node_opt(id) else {
        return false;
    };
    if node.is_volatile_access() {
        return false;
    }
    match node.op {
        Op::Guard { .. } => true,
        Op::ArrayLoad(kind) | Op::ArrayStore(kind) => {
            !matches!(kind, MemKind::Ref) && graph.memory_effect(id).order == MemOrder::Plain
        }
        Op::ArrayLength => graph.memory_effect(id).order == MemOrder::Plain,
        Op::Div | Op::Rem => matches!(node.ty, IrType::Int | IrType::Long),
        _ => false,
    }
}

/// TYPECHECK21-9: why a loop with the admitted deopt points `deopt_nodes`
/// cannot take the pads, as far as the graph can tell; `None` when it can
/// (or when there is no deopt point). See the section comment. `helper_free`:
/// every cell is a static one, whose pad calls no helper (PROMOTE21-2).
fn deopt_point_refusal(
    graph: &Graph,
    deopt_nodes: &[NodeId],
    body: &FxHashSet<NodeId>,
    header_mem: NodeId,
    helper_free: bool,
) -> Option<(NodeId, &'static str)> {
    if deopt_nodes.is_empty() {
        return None;
    }
    // PROMOTE21-1 (lane promote residual,
    // `CRATONVM_JIT_IR_LOOP_PROMOTE_DEOPT_PADS_BRANCH`): a store under a
    // branch is served by the joins' value φs (`pad_join_phis`).
    if !crate::ir_lower::ir_loop_promote_deopt_pads_branch_enabled() {
        if let Some(&phi) = body_memory_phis(graph, body, header_mem).first() {
            return Some((phi, "a body memory phi with a deopt point"));
        }
    }
    predicted_pad_refusal(graph, deopt_nodes, helper_free)
}

/// The loop body's memory φs other than the header's, sorted (so walks over
/// them build their value φs in a deterministic order).
fn body_memory_phis(graph: &Graph, body: &FxHashSet<NodeId>, header_mem: NodeId) -> Vec<NodeId> {
    let mut out: Vec<NodeId> = body
        .iter()
        .copied()
        .filter(|&b| {
            b != header_mem
                && graph
                    .node_opt(b)
                    .is_some_and(|n| n.op == Op::Phi && n.ty == IrType::Memory)
        })
        .collect();
    out.sort_unstable();
    out
}

/// PROMOTE21-1 (lane promote residual): the value φs a static / fresh walk
/// built for the body memory φs `mem_phis` (its `memo`), each anchored at
/// its memory φ's own merge -- the values the pending plan reads at those
/// joins (`ir_lower::pending_join_phi`). The caller roots them
/// (`Graph::promoted_join_phis`) once the loop is promoted, so a join the
/// exit walk never crossed (an unconditional store of the cell follows it)
/// keeps its value too, as the field promotion's forced walks do (OSR7-2).
fn pad_join_phis(graph: &Graph, memo: &FxHashMap<NodeId, NodeId>, mem_phis: &[NodeId]) -> Vec<NodeId> {
    let mut out = Vec::new();
    for &m in mem_phis {
        let Some(&vp) = memo.get(&m) else {
            continue;
        };
        let anchored = graph.node_opt(m).and_then(|n| n.input_opt(0));
        if graph
            .node_opt(vp)
            .is_some_and(|n| n.op == Op::Phi && n.input_opt(0) == anchored)
        {
            out.push(vp);
        }
    }
    out
}

/// PROMOTE21-1: root a promoted loop's join value φs for dead-node
/// elimination (`Graph::promoted_join_phis`).
fn root_pad_join_phis(graph: &mut Graph, mut phis: Vec<NodeId>) {
    phis.sort_unstable();
    phis.dedup();
    for phi in phis {
        if !graph.promoted_join_phis.contains(&phi) {
            graph.promoted_join_phis.push(phi);
        }
    }
}

/// TYPECHECK21-9: the in-loop stores `own` of one promoted cell whose exit
/// write-back store is `exit_store`. Without a deopt point (`keep` false):
/// deleted, their memory users taking each store's own token (the
/// promotions' rule). With one: kept as silent stores, recorded for the
/// lowering's pending plan -- unless the exit store is missing (`NO_NODE`),
/// when they stay ordinary stores (memory exact at every point, never a
/// silent store nothing describes).
fn sink_or_keep_silent(graph: &mut Graph, own: &[NodeId], exit_store: NodeId, keep: bool) {
    if keep {
        if exit_store != NO_NODE {
            graph
                .promoted_sunk_stores
                .extend(own.iter().map(|&st| (exit_store, st)));
        }
        return;
    }
    for &st in own {
        if bypass_token(graph, st) {
            graph.kill(st);
        }
    }
}

/// PROMOTE21-5 (lane promote): a static / fresh loop promoted with
/// `deopt_points` admitted deopt points counts once in the promotion census
/// (`static_fresh_pad_loops`); a loop without any counts nothing.
fn note_pad_loop(deopt_points: usize) {
    if deopt_points > 0 {
        crate::ir_check_elim::note_promote_census(
            crate::ir_check_elim::PROMOTE_CENSUS_STATIC_FRESH_PAD_LOOPS,
            1,
        );
    }
}

/// PROMOTE21-5: a static / fresh loop refused at `id` only because this is
/// the IR door's retry without deopt-pad promotions
/// (`Graph::promote_pending_withheld`) counts in `static_fresh_withheld`: the
/// loops one refused pad cost their method.
fn note_pads_withheld(graph: &Graph, id: NodeId) {
    if graph.promote_pending_withheld
        && crate::ir_lower::ir_loop_promote_deopt_pads_enabled()
        && pad_admitted_deopt_point(graph, id)
    {
        crate::ir_check_elim::note_promote_census(
            crate::ir_check_elim::PROMOTE_CENSUS_STATIC_FRESH_WITHHELD,
            1,
        );
    }
}

/// `CRATONVM_DBG_LOOP_FIELD_FORWARD=1`: the op of a deopt point the static /
/// fresh promotions refuse (`a node that can deopt`), which the refusal line
/// names only by id -- what a JDK loop would need admitted next.
fn note_unadmitted_deopt_point(graph: &Graph, header_bci: usize, id: NodeId) {
    if !false {
        return;
    }
    let (op, pc) = graph
        .node_opt(id)
        .map(|n| (format!("{:?}", n.op), n.bytecode_pc))
        .unwrap_or_else(|| (String::from("?"), None));
    eprintln!("[ir] loop promote: header bci {header_bci}: unadmitted deopt point n{id} {op} at pc {pc:?}");
}

/// TYPECHECK21-9: the `CRATONVM_DBG_JITC` success line's suffix.
fn pads_note(deopt_points: usize) -> &'static str {
    if deopt_points > 0 {
        " (deopt pads write back)"
    } else {
        ""
    }
}

/// Is `base` a fresh allocation of this compile that nothing can publish (see
/// the section comment)? `Err` names the first use that may.
fn fresh_base_unpublished(graph: &Graph, base: NodeId) -> Result<(), NodeId> {
    let Some(node) = graph.node_opt(base) else {
        return Err(base);
    };
    if !matches!(node.op, Op::New { .. } | Op::NewCp { .. }) || node.ty != IrType::Ref {
        return Err(base);
    }
    for user in graph.users_of(base) {
        let Some(u) = graph.node_opt(user) else {
            continue;
        };
        if u.op == Op::Dead {
            continue;
        }
        for (i, &x) in u.inputs.iter().enumerate() {
            if x != base {
                continue;
            }
            let ok = crate::ir::is_memory_token_slot(u, i)
                || (matches!(u.op, Op::Load(_) | Op::Store(_)) && i == 2)
                || (u.op == Op::Return && i == 1)
                || matches!(u.op, Op::Cmp(_) | Op::ExactClassIs { .. } | Op::ClassWordOf);
            if !ok {
                return Err(user);
            }
        }
    }
    Ok(())
}

/// Does the unpublished allocation `base` ([`fresh_base_unpublished`]) leave
/// the activation through a `return`? Only then is it this promotion's
/// (`CRATONVM_JIT_IR_LOOP_FRESH_FIELD_PROMOTE_RETURNED_ONLY`, default ON,
/// `=0` admits every unpublished base as before). An object that is never
/// returned is escape analysis's, which runs later and deletes it outright
/// -- with the promotion's pre-header reads and exit write-backs, leaving
/// the pre-header frame copy (`preheader_snapshot`) named by no node at the
/// header bci, which `ir_verify` then rejects for the whole method
/// (`FreshLocal.local`: 6478 ns against 2316 with the promotion off; with
/// the deopt pads on, every such loop holding an array read too). Found
/// while retiring TYPECHECK21-9 (lane promote).
fn fresh_base_returned(graph: &Graph, header_bci: usize, base: NodeId) -> bool {
    let returned = graph.users_of(base).into_iter().any(|u| {
        graph
            .node_opt(u)
            .is_some_and(|n| n.op == Op::Return && n.input_opt(1) == Some(base))    });
    if !returned && false {
        eprintln!(
            "[ir] loop fresh field promote: header bci {header_bci}: allocation n{base} is never \
             returned (left to escape analysis)"
        );
    }
    returned
}

/// `CRATONVM_DBG_LOOP_FIELD_FORWARD=1` (JIT round 21 wave 1, lane loops,
/// LOOP21-1): an ALLOCATION of this compile whose fields a loop accesses
/// failed [`fresh_base_unpublished`] at `user`: name the use (op and pc), the
/// line the `R20PLoopStaticBarrier` engagement question needs. Silent for a
/// base that is no allocation (a parameter, a load), which was never a
/// candidate -- except an OSR local (j27 wave 6), named for LOOPSTATIC21-2.
fn note_fresh_publication(graph: &Graph, header_bci: usize, base: NodeId, user: NodeId) {
    if !false {
        return;
    }
    // j27 wave 6 (lane T1LOOPSTATIC, LOOPSTATIC21-2's first step): on an
    // OSR-door body the accumulator arrives as an `OsrLocal`, whose freshness
    // the compile cannot see; named once per base (the census memoises it),
    // with the method, so the one-call row's line can be found. Its cells are
    // `promote_loop_cells`' (`loop field promote` lines) on that door.
    if graph
        .node_opt(base)
        .is_some_and(|n| matches!(n.op, Op::OsrLocal(_)))
    {
        eprintln!(
            "[ir] loop fresh field promote: {} header bci {header_bci}: base n{base} is an \
             OSR local (freshness unknown on the OSR door)",
            graph.scope_method_key
        );
        return;
    }
    let is_alloc = graph
        .node_opt(base)
        .is_some_and(|n| matches!(n.op, Op::New { .. } | Op::NewCp { .. }));
    if !is_alloc {
        return;
    }
    let (op, pc) = graph
        .node_opt(user)
        .map(|u| (format!("{:?}", u.op), u.bytecode_pc))
        .unwrap_or_else(|| (String::from("?"), None));
    eprintln!(
        "[ir] loop fresh field promote: header bci {header_bci}: allocation n{base} may be \
         published by n{user} {op} at pc {pc:?}"
    );
}

/// Under the promotions' refusal flags: the memory node a failed
/// [`FreshWalk`] stopped at, so a `no exit value` refusal says where the
/// cell's memory chain left the loop or met a node it cannot pass.
fn note_fresh_walk_stuck(graph: &Graph, header_bci: usize, key: &FieldKey, stuck: Option<NodeId>) {
    if !(false
        || super::loop_pass_report_enabled())
    {
        return;
    }
    let what = stuck
        .and_then(|s| {
            graph
                .node_opt(s)
                .map(|n| format!("n{s} {:?} at pc {:?}", n.op, n.bytecode_pc))
        })
        .unwrap_or_else(|| String::from("?"));
    eprintln!(
        "[ir] loop fresh field promote: header bci {header_bci}: cell n{} #{} walk stopped at {what}",
        key.base, key.idx
    );
}

/// Promote the fresh-object cells the section describes, loop by loop.
/// Returns how many cells were promoted.
pub(super) fn promote_fresh_loop_fields(graph: &mut Graph) -> usize {
    if let Some(compact) = graph.nodes.iter().position(|n| {
        n.op != Op::Dead
            && n.op.memory_shape().is_some()
            && crate::ir::memory_token_slot(n).is_none()
    }) {
        // JIT round 21 (lane typecheck, LOOP21-1): this whole-graph refusal
        // was the one silent exit of the pass, so a probe that printed no
        // `refused --` line could not tell it from "never ran". Diagnostic
        // only.
        if false {
            eprintln!(
                "[ir] loop fresh field promote: {} graph refused -- a memory op with no token (n{compact})",
                graph.scope_method_key
            );
        }
        return 0;
    }
    let mut promoted = 0usize;
    let mut tried: FxHashSet<NodeId> = FxHashSet::default();
    // A promotion rewires its exit edge, which may be the next loop's entry:
    // the loop list is taken again after each one.
    loop {
        let loops = loop_headers(graph);
        let headers: FxHashSet<NodeId> = loops.iter().map(|l| l.0).collect();
        let Some((region, entry_pred, back_ctrls)) =
            loops.into_iter().find(|l| !tried.contains(&l.0))
        else {
            break;
        };
        tried.insert(region);
        // j27 (lane FRAMES): collect the pre-header copy a refusal left.
        promoted += super::orphan_frames::collecting_orphaned_frames(graph, |graph| {
            promote_fresh_loop(graph, region, entry_pred, &back_ctrls, &headers)
        });
    }
    if promoted > 0 && crate::x64::dbg_jitc_enabled() {
        eprintln!(
            "[ir] loop fresh field promote: {} {promoted} cell(s) kept in registers",
            graph.scope_method_key
        );
    }
    promoted
}

/// The value of one promoted fresh-object cell at a memory state inside the
/// loop: [`StaticWalk`]'s walk, with the cell's own stores matched by base,
/// index, kind and declaring class.
struct FreshWalk<'a> {
    key: FieldKey,
    fwd: usize,
    header_mem: NodeId,
    init: NodeId,
    ty: IrType,
    body: &'a FxHashSet<NodeId>,
    memo: FxHashMap<NodeId, NodeId>,
    budget: usize,
    /// The memory node a failed walk stopped at (for the refusal line).
    stuck: Option<NodeId>,
}

impl FreshWalk<'_> {
    fn value_at(&mut self, graph: &mut Graph, mem: NodeId) -> Option<NodeId> {
        let mut path: Vec<NodeId> = Vec::new();
        let mut cur = mem;
        let answer = loop {
            if let Some(&v) = self.memo.get(&cur) {
                break v;
            }
            self.stuck = Some(cur);
            self.budget = self.budget.checked_sub(1)?;
            let node = graph.node_opt(cur)?.clone();
            if cur == self.header_mem || (node.op == Op::Phi && node.ty == IrType::Memory) {
                break self.phi_value(graph, cur, &node)?;
            }
            // The chain may only leave the loop through its header φ.
            if !self.body.contains(&cur) {
                return None;
            }
            if matches!(node.op, Op::Store(_))
                && field_access(graph, cur) == Some((self.key.base, self.key.idx, self.key.kind))
                && graph.field_decl_class.get(&cur).copied() == Some(self.key.decl)
            {
                // `[ctrl, mem, base, idx, value]`.
                break node.input_opt(4)?;
            }
            let token = crate::ir::memory_token_slot(&node).and_then(|t| node.input_opt(t))?;
            path.push(cur);
            cur = token;
        };
        for p in path {
            self.memo.insert(p, answer);
        }
        Some(answer)
    }

    fn phi_value(
        &mut self,
        graph: &mut Graph,
        phi: NodeId,
        node: &crate::ir::Node,
    ) -> Option<NodeId> {
        let ctrl = node.input_opt(0)?;
        if phi != self.header_mem && !self.body.contains(&ctrl) {
            return None;
        }
        let preds: Vec<NodeId> = node.inputs.as_slice().get(1..)?.to_vec();
        let mut inputs = vec![ctrl];
        inputs.extend(std::iter::repeat(NO_NODE).take(preds.len()));
        // Placeholders first, so a cycle back to this φ (a back edge) finds it
        // in the memo.
        let value = graph.add(Op::Phi, self.ty, inputs, node.bytecode_pc);
        self.memo.insert(phi, value);
        for (k, pred_mem) in preds.into_iter().enumerate() {
            let v = if phi == self.header_mem && k == self.fwd {
                self.init
            } else {
                self.value_at(graph, pred_mem)?
            };
            graph.set_input(value, k + 1, v);
        }
        Some(super::dedup_walk_phi(graph, ctrl, value, &mut self.memo))
    }
}

fn promote_fresh_loop(
    graph: &mut Graph,
    region: NodeId,
    entry_pred: NodeId,
    back_ctrls: &[NodeId],
    headers: &FxHashSet<NodeId>,
) -> usize {
    let Some(header_bci) = graph.node_opt(region).and_then(|n| n.bytecode_pc) else {
        return 0;
    };
    if graph.promoted_cell_header_bcis.contains(&header_bci) {
        return fresh_promote_refuse(graph, header_bci, "a header another promotion took", region);
    }
    let Some(fwd) = graph
        .node_opt(region)
        .and_then(|n| n.inputs.iter().position(|&c| c == entry_pred))
    else {
        return 0;
    };
    let mem_phis: Vec<NodeId> = graph
        .users_of(region)
        .into_iter()
        .filter(|&u| {
            graph.node_opt(u).is_some_and(|n| {
                n.op == Op::Phi && n.ty == IrType::Memory && n.input_opt(0) == Some(region)
            })
        })
        .collect();
    let [header_mem] = mem_phis.as_slice() else {
        return fresh_promote_refuse(
            graph,
            header_bci,
            "not exactly one header memory phi",
            region,
        );
    };
    let header_mem = *header_mem;
    let mut body = loop_body(graph, region, entry_pred, back_ctrls);
    if headers.iter().any(|&h| h != region && body.contains(&h)) {
        return fresh_promote_refuse(graph, header_bci, "an inner loop", region);
    }
    // The φs at joins INSIDE the body (a store under a branch).
    let inner: Vec<NodeId> = graph
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| {
            n.op == Op::Phi
                && n.input_opt(0)
                    .is_some_and(|c| c != region && body.contains(&c))
        })
        // Cast: arena index to node id.
        .map(|(id, _)| id as NodeId)
        .collect();
    body.extend(inner);

    // The one exit: the header's own test.
    let mut exits: Vec<NodeId> = Vec::new();
    for (id, n) in graph.nodes.iter().enumerate() {
        // Cast: arena index to node id.
        let id = id as NodeId;
        if n.op == Op::Dead || !n.op.is_control() || body.contains(&id) {
            continue;
        }
        if n.inputs.iter().any(|&i| i != NO_NODE && body.contains(&i)) {
            exits.push(id);
        }
    }
    let [exit] = exits.as_slice() else {
        return fresh_promote_refuse(graph, header_bci, "not exactly one exit edge", region);
    };
    let exit = *exit;
    let exit_from_header = graph.node_opt(exit).is_some_and(|x| {
        matches!(x.op, Op::Proj(_))
            && x.input_opt(0)
                .and_then(|i| graph.node_opt(i))
                .is_some_and(|i| i.op == Op::If && i.input_opt(0) == Some(region))
    });
    if !exit_from_header {
        return fresh_promote_refuse(graph, header_bci, "the exit is not the header's test", exit);
    }

    // Census. A field access through a fresh, unpublished base defined
    // outside the loop, at an `int` / `long` identified field, is a candidate
    // cell; every other node must be unable to transfer, order or write `Any`
    // -- or (JIT round 21, lane promote, TYPECHECK21-9) be a deopt point whose
    // pad writes the cells back first (`deopt_pads_admitted`).
    let pads = deopt_pads_admitted(graph);
    let mut deopt_nodes: Vec<NodeId> = Vec::new();
    let mut body_nodes: Vec<NodeId> = body.iter().copied().collect();
    body_nodes.sort_unstable();
    let mut fresh: FxHashMap<NodeId, bool> = FxHashMap::default();
    let mut loads: FxHashMap<FieldKey, Vec<NodeId>> = FxHashMap::default();
    let mut stores: FxHashMap<FieldKey, Vec<NodeId>> = FxHashMap::default();
    let mut samples: FxHashMap<FieldKey, NodeId> = FxHashMap::default();
    for &id in &body_nodes {
        let Some(node) = graph.node_opt(id) else {
            continue;
        };
        if node.op == Op::Dead || node.op == Op::Phi {
            continue;
        }
        if node.is_volatile_access() {
            return fresh_promote_refuse(graph, header_bci, "a volatile access", id);
        }
        if let (Some((base, idx, kind)), Some(decl)) = (
            field_access(graph, id),
            graph.field_decl_class.get(&id).copied(),
        ) {
            if matches!(kind, MemKind::Int | MemKind::Long) && !body.contains(&base) {
                // JIT round 21 wave 1 (lane loops, LOOP21-1): an allocation
                // that fails the publication screen is NAMED under the
                // refusal flag (its accesses then fall to the census below,
                // which used to refuse only with `no fresh field cell`, never
                // saying which use kept the object).
                let is_fresh = *fresh
                    .entry(base)
                    .or_insert_with(|| match fresh_base_unpublished(graph, base) {
                        Ok(()) => fresh_base_returned(graph, header_bci, base),
                        Err(user) => {
                            note_fresh_publication(graph, header_bci, base, user);
                            false
                        }
                    });
                if is_fresh {
                    let key = FieldKey {
                        base,
                        idx,
                        decl,
                        kind,
                    };
                    let is_store = matches!(node.op, Op::Store(_));
                    let map = if is_store { &mut stores } else { &mut loads };
                    map.entry(key).or_default().push(id);
                    samples.entry(key).or_insert(id);
                    continue;
                }
            }
        }
        // A direct, non-volatile static access cannot transfer and writes no
        // heap field (the static promotion's own admission).
        if direct_static_cell(graph, id).is_some() {
            continue;
        }
        // JIT round 21 (lane loopopt, LOOP21-1): a read of a field of the
        // receiver (or of another base known non-null at the entry) that the
        // promotion does not touch: it cannot transfer (see
        // `nonnull_invariant_read_cannot_transfer`). Never a read through a
        // fresh, unpublished base itself (revjit, correctness): such a read
        // reaches here when it is no cell candidate (no `field_decl_class`
        // entry, or a narrow / reference kind) and may name the very field a
        // cell of the same base promotes -- its stores sunk to the exit, it
        // would read stale memory. It keeps the pre-LOOP21-1 refusal below.
        if nonnull_invariant_read_cannot_transfer(graph, id, &body)
            && !field_access(graph, id).is_some_and(|(base, _, _)| {
                *fresh
                    .entry(base)
                    .or_insert_with(|| fresh_base_unpublished(graph, base).is_ok())
            })
        {
            continue;
        }
        let eff = graph.memory_effect(id);
        if eff.order != MemOrder::Plain || matches!(eff.writes, AliasClass::Any) {
            return fresh_promote_refuse(graph, header_bci, "an ordering or opaque write", id);
        }
        if !crate::ir_lower::node_cannot_deopt(graph, node) {
            // TYPECHECK21-9: a deopt point the pads can serve.
            if pads && pad_admitted_deopt_point(graph, id) {
                deopt_nodes.push(id);
                continue;
            }
            note_pads_withheld(graph, id);
            note_unadmitted_deopt_point(graph, header_bci, id);
            return fresh_promote_refuse(graph, header_bci, "a node that can deopt", id);
        }
    }
    // TYPECHECK21-9: the pads' own refusals the graph already shows.
    if let Some((id, why)) = deopt_point_refusal(graph, &deopt_nodes, &body, header_mem, false) {
        return fresh_promote_refuse(graph, header_bci, why, id);
    }
    // A cell: a fresh-base field the loop stores. A base / index accessed
    // under two keys (kind or declaring class) is not one cell: refused
    // whole, never assumed.
    let all_keys: Vec<FieldKey> = loads.keys().chain(stores.keys()).copied().collect();
    let mut keys: Vec<FieldKey> = stores
        .keys()
        .copied()
        .filter(|k| {
            all_keys
                .iter()
                .all(|o| (o.base, o.idx) != (k.base, k.idx) || o == k)
        })
        .collect();
    if keys.len() != stores.len()
        || loads.keys().any(|k| {
            all_keys
                .iter()
                .any(|o| (o.base, o.idx) == (k.base, k.idx) && o != k)
        })
    {
        return fresh_promote_refuse(
            graph,
            header_bci,
            "a fresh field accessed under two identities",
            region,
        );
    }
    // A fresh field the loop only READS stays a read (a fresh base cannot
    // deopt it, but it is no cell): its loads must then be deopt-free too.
    for (k, lds) in &loads {
        if stores.contains_key(k) {
            continue;
        }
        for &ld in lds {
            let ok = graph
                .node_opt(ld)
                .is_some_and(|n| crate::ir_lower::node_cannot_deopt(graph, n));
            if !ok {
                return fresh_promote_refuse(
                    graph,
                    header_bci,
                    "a read of an unpromoted fresh field",
                    ld,
                );
            }
        }
    }
    keys.sort_by_key(|k| (k.base, k.idx, k.decl));
    if keys.is_empty() {
        return fresh_promote_refuse(graph, header_bci, "no fresh field cell", region);
    }

    // Every memory user outside the loop must read the header's memory φ.
    let mut outside: Vec<(NodeId, usize)> = Vec::new();
    for (id, n) in graph.nodes.iter().enumerate() {
        // Cast: arena index to node id.
        let id = id as NodeId;
        if n.op == Op::Dead || body.contains(&id) || id == header_mem {
            continue;
        }
        for (i, &inp) in n.inputs.iter().enumerate() {
            if inp == NO_NODE || !(inp == header_mem || body.contains(&inp)) {
                continue;
            }
            if !crate::ir::is_memory_token_slot(n, i) {
                continue;
            }
            if inp != header_mem {
                return fresh_promote_refuse(
                    graph,
                    header_bci,
                    "loop memory leaves past the header",
                    id,
                );
            }
            outside.push((id, i));
        }
    }
    let Some(si) = promotion_header_frame(graph, header_bci) else {
        return fresh_promote_refuse(graph, header_bci, "no header frame", region);
    };
    let Some(pre_si) = preheader_snapshot(graph, region, fwd, header_bci, &body) else {
        return fresh_promote_refuse(graph, header_bci, "no pre-header frame", region);
    };

    // The pre-header reads, threaded onto the forward memory edge.
    let Some(mut mem) = graph
        .node_opt(header_mem)
        .and_then(|n| n.input_opt(fwd + 1))
    else {
        return 0;
    };
    let mut initial: Vec<(FieldKey, NodeId, NodeId, IrType)> = Vec::with_capacity(keys.len());
    for &key in &keys {
        let Some(&sample) = samples.get(&key) else {
            continue;
        };
        let Some(idx_node) = graph.node_opt(sample).and_then(|n| n.input_opt(3)) else {
            continue;
        };
        let ty = if key.kind == MemKind::Long {
            IrType::Long
        } else {
            IrType::Int
        };
        let read = graph.add(
            Op::Load(key.kind),
            ty,
            vec![entry_pred, mem, key.base, idx_node],
            Some(header_bci),
        );
        graph.field_decl_class.insert(read, key.decl);
        // The header's bci is not this field's `getfield`; its compact row
        // (if any) is not this read's (the forward's own note).
        graph.synthetic_field_access_pcs.insert(header_bci);
        if !graph.set_node_frame_snapshot(read, pre_si) {
            // Unreachable: the read's pc is the snapshot's bci.
            graph.kill(read);
            continue;
        }
        mem = read;
        initial.push((key, read, idx_node, ty));
    }
    if initial.is_empty() {
        return 0;
    }
    graph.set_input(header_mem, fwd + 1, mem);

    // Each cell's in-loop reads and its value at the exit, walked before any
    // node is deleted. A cell whose walk fails keeps its loads and stores
    // (its pre-header read stays: one extra read, never a wrong one).
    #[allow(clippy::type_complexity)]
    let mut ready: Vec<(FieldKey, NodeId, NodeId, NodeId, Vec<(NodeId, NodeId)>)> = Vec::new();
    // PROMOTE21-1: with a deopt point inside, the body's memory φs (none
    // unless `deopt_point_refusal` admitted them) and their value φs.
    let join_mem_phis = if deopt_nodes.is_empty() {
        Vec::new()
    } else {
        body_memory_phis(graph, &body, header_mem)
    };
    let mut join_values: Vec<NodeId> = Vec::new();
    for &(key, init, idx_node, ty) in &initial {
        let mut walk = FreshWalk {
            key,
            fwd,
            header_mem,
            init,
            ty,
            body: &body,
            memo: FxHashMap::default(),
            budget: MAX_WALK,
            stuck: None,
        };
        let Some(exit_value) = walk.value_at(graph, header_mem) else {
            note_fresh_walk_stuck(graph, header_bci, &key, walk.stuck);
            continue;
        };
        let mut answers: Vec<(NodeId, NodeId)> = Vec::new();
        let mut failed = false;
        for &ld in loads.get(&key).map(Vec::as_slice).unwrap_or(&[]) {
            let token = graph
                .node_opt(ld)
                .and_then(|n| crate::ir::memory_token_slot(n).and_then(|t| n.input_opt(t)));
            match token.and_then(|t| walk.value_at(graph, t)) {
                Some(v) => answers.push((ld, v)),
                None => {
                    note_fresh_walk_stuck(graph, header_bci, &key, walk.stuck);
                    failed = true;
                    break;
                }
            }
        }
        // PROMOTE21-1: every join's value, for the pads (a cell any join
        // walk fails for keeps its loads and stores).
        if !failed && !join_mem_phis.is_empty() {
            failed = join_mem_phis
                .iter()
                .any(|&m| walk.value_at(graph, m).is_none());
            if !failed {
                join_values.extend(pad_join_phis(graph, &walk.memo, &join_mem_phis));
            }
        }
        if failed {
            continue;
        }
        ready.push((key, init, idx_node, exit_value, answers));
    }
    if ready.is_empty() {
        return fresh_promote_refuse(graph, header_bci, "no exit value", region);
    }

    // The write-backs, chained on the exit edge.
    let exit_users: Vec<NodeId> = graph.users_of(exit);
    let mut ctrl = exit;
    let mut wmem = header_mem;
    // TYPECHECK21-9: each cell's exit store (`NO_NODE` if the diamond lost
    // it, which keeps that cell's in-loop stores real: see below).
    let mut exit_stores: Vec<NodeId> = Vec::with_capacity(ready.len());
    for (key, init, idx_node, value, _) in &ready {
        let (join, phi) = add_cell_writeback(
            graph, ctrl, wmem, key, *init, *idx_node, *value, header_bci, si,
        );
        ctrl = join;
        wmem = phi;
        // The lowerer finds the cell from this store when it gives the
        // header's poll a write-back exit (OSR2-1's pad).
        let store = graph
            .node_opt(phi)
            .and_then(|n| n.input_opt(1))
            .unwrap_or(NO_NODE);
        if store != NO_NODE {
            graph.promoted_cell_writebacks.push((region, store));
        }
        exit_stores.push(store);
    }
    for user in exit_users {
        let Some(n) = graph.node_opt(user) else {
            continue;
        };
        let slots: Vec<usize> = n
            .inputs
            .iter()
            .enumerate()
            .filter(|&(_, &x)| x == exit)
            .map(|(i, _)| i)
            .collect();
        for slot in slots {
            graph.set_input(user, slot, ctrl);
        }
    }
    for (user, slot) in outside {
        graph.set_input(user, slot, wmem);
    }

    // The in-loop reads take their values; a read may be answered by another
    // removed read (`x.f = x.f`): resolve through the map first.
    let map: FxHashMap<NodeId, NodeId> = ready.iter().flat_map(|r| r.4.iter().copied()).collect();
    let resolve = |mut v: NodeId| {
        let mut seen = 0usize;
        while let Some(&next) = map.get(&v) {
            v = next;
            seen += 1;
            if seen > map.len() {
                return NO_NODE;
            }
        }
        v
    };
    let mut answered: Vec<(NodeId, NodeId)> = Vec::new();
    for (ld, v) in map.iter().map(|(&ld, &v)| (ld, v)) {
        answered.push((ld, resolve(v)));
    }
    answered.sort_unstable();
    for (ld, v) in answered {
        if v == NO_NODE || v == ld || !bypass_token(graph, ld) {
            continue;
        }
        graph.replace_all_uses(ld, v);
        graph.kill(ld);
    }
    // The in-loop stores of the promoted cells go. TYPECHECK21-9: with a
    // deopt point inside they stay as SILENT stores (`sink_or_keep_silent`).
    let mut done = 0usize;
    for ((key, _, _, _, _), &exit_store) in ready.iter().zip(&exit_stores) {
        let own = stores.get(key).cloned().unwrap_or_default();
        sink_or_keep_silent(graph, &own, exit_store, !deopt_nodes.is_empty());
        done += 1;
    }
    if !graph.promoted_cell_header_bcis.contains(&header_bci) {
        graph.promoted_cell_header_bcis.push(header_bci);
    }
    root_pad_join_phis(graph, join_values);
    note_pad_loop(deopt_nodes.len());
    if crate::x64::dbg_jitc_enabled() {
        eprintln!(
            "[ir] loop fresh field promote: {} header bci {header_bci}: {done} fresh-object cell(s), stores sunk to the exit{}",
            graph.scope_method_key,
            pads_note(deopt_nodes.len())
        );
    }
    done
}

// ── JIT round 21 wave 1 (lane loops): HOIST13-2 in a method-entry body ─────
//
// `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_METHOD_ENTRY` (perf, opt-in). The
// round-19 census (HOIST13-2, `loop_static_hoist::store_sink_census`) found the
// heap-store gap kernels to be ACCUMULATORS: one store to a field of a
// loop-invariant object, every other access in the loop a load of that same
// cell (`this.sum += v`, `box.sum += v`). Round 20 built the promotion for a
// static cell (both bodies), for a fresh unpublished object (both bodies) and
// for an invariant base in an OSR-door body (`promote_loop_cells`, after the
// forward). A METHOD-ENTRY body -- the one a kernel called more than a few
// times runs -- promoted no invariant-base field: the forward runs there only
// for class-predicated loops, and the promotion not at all.
//
// Under the switch, in a method-entry body (not the optimizing OSR route, not
// an OSR-door graph), every loop whose census shows a promotable field
// accumulator (`loop_static_hoist::accumulator_loop_headers`: a store of a
// constant-offset field of an invariant base, every other access of that
// offset a load through the same base, and no opaque node -- call,
// allocation, monitor, helper-routed static) is offered to the field forward
// and then, alone, to the cell promotion. Both are the OSR-door passes
// unchanged, so every condition they state holds here too:
//
// * the forward refuses a volatile access, an ordering node and an `Any`
//   writer it cannot reload after; it reads the field once at the header bci
//   with the pre-header frame and forwards only a base known non-null at the
//   loop entry (an allocation, a loop-entry exact-class guard, and -- under
//   this switch -- the compiling method's receiver);
// * the promotion refuses any node that can transfer to the interpreter (a
//   guard, a check, a call, a load of a cell left in memory), takes one exit
//   (the header's test) unless the body-exit switch is on, and writes each
//   cell back by the conditional store at that exit before any other node;
//   the header joins `Graph::promoted_cell_header_bcis`, so the back-edge
//   poll's mode exit is refused (or written back first by the OSR2-1 pad).
//
// What a method-entry body adds, entry by entry (the static promotion's
// method-entry argument, `static_promote_method_entry_enabled`):
// * the method's own entry reaches the header through the pre-header, so it
//   runs the reads;
// * an ordinary OSR entry stub at the header jumps past them: the cell's
//   value φ (and the pre-header read the write-back compares with) are live
//   into the header and named by no local, which `Lowerer::emit_osr_entry_stubs`
//   refuses -- that entry is not emitted, never seeded with a stale value.
//
// JMM: no synchronisation action in the loop (calls, monitors, volatile and
// ordered accesses all refuse), so another thread may legally see the value
// from before the loop until the exit's write-back; the write-back is
// conditional, so a zero-trip loop or a loop that never changed the value
// never writes over another thread's store. A thrown exception cannot leave
// the loop between a deleted store and the write-back: a throwing node is a
// node that can deopt or an exit edge, and both refuse.

/// `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_METHOD_ENTRY` (JIT round 21 wave 1,
/// lane loops, HOIST13-2) -- perf, **default OFF** (opt-in `=1`): the field
/// forward and the cell promotion run in a METHOD-ENTRY body for the
/// accumulator loops (section comment above), and the forward counts the
/// compiling method's receiver as known non-null (in every body it runs in).
/// Inert without `CRATONVM_JIT_IR_LOOP_FIELD_FORWARD` and
/// `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE` (both default ON). Read without a
/// `static` (`process_global_statics_ratchet`), once per optimize and once
/// per forwarded loop.
pub(crate) fn method_entry_promote_enabled() -> bool {
    // R22-AB-FLIP: default ON since the JIT r22 opt-in A/B (docs/feature-designs/jit-ir-optimizations.md); `=0` is the kill switch.
    true
}

/// `CRATONVM_JIT_IR_LOOP_PROMOTE_SPLICED` (JIT round 21 wave 1, lane loops;
/// perf, **default ON** since JIT round 24, `=0` kills): the cell promotions reach the loop of
/// a SPLICED callee -- the copy real code runs, since a small hot kernel is
/// inlined into its caller (`R21LoopAccum fieldThisInt` is spliced into the
/// driver's lambda: `UniqueConcreteMethod`). Two things kept that loop:
///
/// * the forward's base was the caller's argument (`o`, a field read of the
///   lambda), not the compiling method's receiver, and not known non-null --
///   although the splice itself null-checks its receiver before a byte of the
///   callee runs (`IrBuilder::begin_splice`'s `Guard(recv != null)`, or the
///   guarded hit edge's `ExactClassIs`). Under the switch a base whose
///   null check is such a passing `Guard` at a control node that dominates the
///   loop entry ([`base_null_checked_before`]) is known non-null;
/// * every promotion asked for the header's FLAT frame for its exit
///   write-back store ("no header frame"), which a spliced loop does not have:
///   it has the chain snapshot (innermost scope, then the callers), the one
///   [`preheader_snapshot`] already maps for the pre-header reads. Under the
///   switch the write-back takes that snapshot ([`promotion_header_frame`]).
///   The write-back's checks cannot fire (its base was dereferenced in the
///   pre-header, or it is a direct static), so the frame is never consulted;
///   it is there because the lowering asks every store for one. The lowering
///   refuses a back-edge poll exit at a promoted header inside a splice
///   (`promoted-field-cells-spliced`), whatever the poll write-back switch says.
///
/// Read without a `static`, per call.
///
/// JIT round 24 (the r21 loops book's close): the two blockers the switch
/// still met -- a guarded splice's `If(ExactClassIs)` hit edge as the base's
/// null proof, and the forward's own loop-entry copies taken for the header's
/// chain frame -- are fixed (`base_null_checked_before`,
/// `promotion_header_frame`). `R21LoopAccum`, 7 interleaved reps, both modes:
/// `field-this-int` 1098 -> 380 ns/op (0.35x), `field-this-long` 0.40x, every
/// other row 0.98-1.01x, process CPU 0.90x, facts unchanged.
pub(crate) fn promote_spliced_enabled() -> bool {
    true
}

/// Is the loop at `region` a spliced callee's: its header pc has a chain
/// snapshot and no flat one (the split [`promotion_header_frame`] makes)?
fn header_is_spliced(graph: &Graph, region: NodeId) -> bool {
    let Some(bci) = graph.node_opt(region).and_then(|n| n.bytecode_pc) else {
        return false;
    };
    let at = |chain: bool| {
        graph
            .safepoints
            .iter()
            .any(|s| s.bci == bci && s.is_splice_state() == chain && !s.post_call_only)
    };
    at(true) && !at(false)
}

/// The frame a promotion's exit write-back names: the header's flat snapshot,
/// or (under [`promote_spliced_enabled`], for a loop of a spliced callee) its
/// one chain snapshot. `None` refuses the promotion, as before.
fn promotion_header_frame(graph: &Graph, header_bci: usize) -> Option<u32> {
    let flat = graph
        .safepoints
        .iter()
        .position(|s| s.bci == header_bci && !s.is_splice_state() && !s.post_call_only);
    let at = match flat {
        Some(i) => i,
        None if promote_spliced_enabled() => {
            let mut chains: Vec<usize> = graph
                .safepoints
                .iter()
                .enumerate()
                .filter(|(_, s)| s.bci == header_bci && s.is_splice_state() && !s.post_call_only)
                .map(|(i, _)| i)
                .collect();
            // The loop-entry copies a node names as its own frame -- the
            // forward's pre-header reads and null guards
            // ([`preheader_snapshot`]), a chain predicate's guard -- are not
            // the header's snapshot: set them aside, as `preheader_snapshot`
            // sets the predicates' aside. Without this a forwarded spliced
            // loop always had two and was refused "no header frame".
            if chains.len() > 1 {
                let named: FxHashSet<usize> = graph
                    .nodes
                    .iter()
                    .filter(|n| n.op != Op::Dead)
                    .filter_map(|n| n.frame_snapshot)
                    .map(|si| si as usize)
                    .collect();
                chains.retain(|si| !named.contains(si));
            }
            let [i] = chains.as_slice() else {
                return None;
            };
            *i
        }
        None => return None,
    };
    u32::try_from(at).ok()
}

/// Under [`promote_spliced_enabled`]: has `base` passed a null check before
/// the loop is entered? A `Guard` on `Cmp(Ne, base, null)` (either order) or
/// on `ExactClassIs { class_id != 0 }(base)` (false for null), pinned to a
/// control node that dominates `entry_pred` ([`super::control_dominates`]:
/// single-predecessor steps only, conservative). A guard deopts when it
/// fails, so control past it has a non-null `base` (an SSA value: it cannot
/// become null later). At `entry_pred` itself the guard is ordered before the
/// pre-header reads, which are added later (higher ids; `ir_schedule` keeps a
/// block's impure nodes in id order).
pub(super) fn base_null_checked_before(graph: &Graph, base: NodeId, entry_pred: NodeId) -> bool {
    let is_null = |v: Option<NodeId>| {
        v.and_then(|v| graph.node_opt(v))
            .is_some_and(crate::ir::is_null_const)
    };
    graph.users_of(base).into_iter().any(|t| {
        let Some(tn) = graph.node_opt(t) else {
            return false;
        };
        let tests_base = match tn.op {
            Op::Cmp(crate::ir::CmpOp::Ne) => {
                (tn.input_opt(0) == Some(base) && is_null(tn.input_opt(1)))
                    || (tn.input_opt(1) == Some(base) && is_null(tn.input_opt(0)))
            }
            Op::ExactClassIs { class_id } => class_id != 0 && tn.input_opt(0) == Some(base),
            _ => false,
        };
        tests_base
            && graph.users_of(t).into_iter().any(|g| {
                graph.node_opt(g).is_some_and(|gn| match gn.op {
                    Op::Guard { .. } => {
                        gn.input_opt(1) == Some(t)
                            && gn
                                .input_opt(0)
                                .is_some_and(|c| super::control_dominates(graph, c, entry_pred))
                    }
                    // A profile-guarded splice tests its receiver with an
                    // `If(ExactClassIs(recv))` -- the hit edge enters the
                    // spliced body, the miss edge makes the call -- rather
                    // than a `Guard`. Its TRUE projection (`Proj(0)`) is as
                    // good a proof: the test is false for null, and control
                    // on that edge has passed it.
                    Op::If if gn.input_opt(1) == Some(t) => {
                        graph.users_of(g).into_iter().any(|pj| {
                            graph.node_opt(pj).is_some_and(|pn| {
                                pn.op == Op::Proj(0)
                                    && super::control_dominates(graph, pj, entry_pred)
                            })
                        })
                    }
                    _ => false,
                })
            })
    })
}

/// Is `base` the compiling method's receiver (`Graph::receiver_param`'s
/// `Op::Param`)? `None` (a static method, or a hand-built graph that said
/// nothing) never matches.
pub(super) fn is_receiver_param(graph: &Graph, base: NodeId) -> bool {
    let Some(recv) = graph.receiver_param else {
        return false;
    };
    graph
        .node_opt(base)
        .is_some_and(|n| n.op == Op::Param(recv) && n.ty == IrType::Ref)
}

/// The r23 L8 heap-buffer close (under
/// `CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_SPLICED_EXACT`): is `base` a read of a
/// trusted `static final` reference the VM filed non-null
/// (`Graph::static_final_non_null`, the fact `base_non_null_on_entry` reads)?
/// Its class is initialised and nothing writes the field again, so it is the
/// same non-null object at every loop entry -- the buffer a kernel spliced
/// from `heapbb(SMALL, i)` reads its fields from.
fn static_final_base_non_null(graph: &Graph, base: NodeId) -> bool {
    super::class_predicate::spliced_exact_forward_enabled()
        && graph.node_opt(base).is_some_and(|n| match n.op {
            Op::LoadStatic {
                class_id,
                field_index,
                type_tag: b'L',
                is_volatile: false,
            } => graph
                .static_final_non_null
                .contains(&(class_id, field_index)),
            _ => false,
        })
}

/// The method-entry run of the section above: `(reads forwarded, cells
/// promoted)`. The caller asks [`method_entry_promote_enabled`] and the two
/// passes' own switches, and runs it only in a method-entry body.
pub(super) fn promote_method_entry_accumulators(
    graph: &mut Graph,
    despec: ForwardDespec<'_>,
) -> (usize, usize) {
    let mut headers = super::loop_static_hoist::accumulator_loop_headers(graph);
    // The run is entered under either switch (`ir_optimize`); with the
    // method-entry one off, only a SPLICED callee's loops (a header with a
    // chain snapshot at its pc and no flat one) are this run's. Without that
    // filter, `PROMOTE_SPLICED` (default ON since the round-24 close) made
    // `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_METHOD_ENTRY=0` promote the
    // method's own loops all the same.
    if !method_entry_promote_enabled() {
        headers.retain(|&h| header_is_spliced(graph, h));
    }
    if headers.is_empty() {
        return (0, 0);
    }
    let forwarded = forward_loop_fields_with(graph, Some(&headers), despec);
    if forwarded == 0 {
        if super::loop_pass_report_enabled() {
            eprintln!(
                "[ir] loop field promote (method entry): {} accumulator loop(s), none forwarded",
                headers.len()
            );
        }
        return (0, 0);
    }
    super::scalar_cleanup(graph);
    let promoted = promote_loop_cells_in(graph, Some(&headers));
    if super::loop_pass_report_enabled() {
        eprintln!(
            "[ir] loop field promote (method entry): {} accumulator loop(s), {forwarded} read(s) \
             forwarded, {promoted} cell(s) promoted",
            headers.len()
        );
    }
    (forwarded, promoted)
}

// ── JIT round 21 wave 1 (lane loops, LOOPS21-1): an array element ─────────
//
// `CRATONVM_JIT_IR_LOOP_ARRAY_ELEMENT_PROMOTE` (perf, opt-in). The
// accumulator `a[k] += x` (a histogram slot, a fixed bucket) with `a` and `k`
// both loop-invariant: the element is read once in the pre-header, carried in
// a value φ through the loop, and written back by one conditional store at
// the exit; the in-loop `ArrayLoad` / `ArrayStore` of that element go. The
// static promotion's shape and census, with the element's own two hazards:
//
// * THE CHECKS. The in-loop accesses could throw (null, bounds) where the
//   promoted loop no longer looks. The pre-header therefore runs, in this
//   order (one block, impure nodes in id order), every one at the header bci
//   with the PRE-HEADER frame (the header snapshot at its loop-entry values,
//   [`preheader_snapshot`]): `len = a.length` (a dereference: a null `a`
//   deopts here), `Guard(0 <= k)`, `Guard(k < len)`, and only then the read
//   `a[k]`. A failing one resumes the interpreter at the loop header with the
//   state the loop is entered with; nothing has run, so the interpreter runs
//   the loop itself and throws exactly where (and whether) the bytecode
//   throws. A header the de-spec registry withdrew is not promoted again (so a
//   loop entered with a bad index keeps its in-loop checks after the trap
//   limit). The write-back store indexes the same `a` (an array's length
//   never changes) at the same `k`, both proven in range by the guards.
// * ALIASING. An element has no field identity: `b[j]` IS `a[k]` when
//   `b == a` and `j == k` at run time. So the loop may hold no other array
//   access of the cell's element kind (any base, any index), and one cell per
//   kind; the kinds are `int` and `long` only (a narrow `byte` / `char` /
//   `short` store truncates, which the forwarded value would not), and arrays
//   of different element kinds are different objects.
//
// Everything else is the static promotion's: no other node may transfer to the
// interpreter or write `Any` or order memory (calls, monitors, volatile
// accesses refuse), one exit (the header's test), every memory user outside
// the loop reads the header's memory φ, the write-back is conditional (the
// JMM reason: never write a value this thread only read over another
// thread's store), the header joins `Graph::promoted_cell_header_bcis` so the
// back-edge poll's mode exit is refused. The element's write-back is NOT
// recorded in `Graph::promoted_cell_writebacks`: the OSR2-1 poll pad knows no
// array shape, and a promoted header it has no record of refuses the exit
// (`promoted-field-cells-unrecorded-header`). For the same reason a header
// another promotion already took is left alone, and this pass runs after all
// of them. An ordinary OSR entry stub at the header is withheld (the value φ
// and the pre-header read are live and unnamed), as for the other cells.

/// `CRATONVM_JIT_IR_LOOP_ARRAY_ELEMENT_PROMOTE` (JIT round 21 wave 1, lane
/// loops, LOOPS21-1) -- perf, **default ON** since the round-24 close (`=0`
/// kills; `R21LoopAccum array-elem` 1380 -> 483 ns jdk-only, 1539 -> 528
/// compatible, 5 rotated reps, facts unchanged): the array-element
/// accumulator promotion above, in method-entry and OSR-door bodies (never on
/// the optimizing OSR route). Inert without `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE`.
/// Read once per optimize, no `static`.
pub(crate) fn array_element_promote_enabled() -> bool {
    true
}

/// One promotable element: the array node, the index node, the kind.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct ElemKey {
    array: NodeId,
    index: NodeId,
    kind: MemKind,
}

impl ElemKey {
    fn ty(self) -> IrType {
        if self.kind == MemKind::Long {
            IrType::Long
        } else {
            IrType::Int
        }
    }
}

/// `(array, index, kind, is_store)` of a full-form `ArrayLoad` / `ArrayStore`.
fn array_access(graph: &Graph, id: NodeId) -> Option<(NodeId, NodeId, MemKind, bool)> {
    let node = graph.node_opt(id)?;
    let (kind, is_store) = match node.op {
        Op::ArrayLoad(k) => (k, false),
        Op::ArrayStore(k) => (k, true),
        _ => return None,
    };
    crate::ir::memory_token_slot(node)?;
    Some((node.input_opt(2)?, node.input_opt(3)?, kind, is_store))
}

/// rev-ir finding 3 (JIT r21 wave 1): may a node with effect `eff` read or
/// write one of the promoted `keys`' elements? Asked of the graph's own effect
/// model: each cell is the class an `ArrayLoad` / `ArrayStore` of it would
/// have (`AliasClass::ArrayElem` at its array node, its index node, its kind)
/// and [`Graph::may_alias`] decides -- `Any` and `Raw` conflict with every
/// primitive cell, a field, static, monitor or length class with none, an
/// element of another kind on another array node with none. Reads count too:
/// a reader past the deleted stores would see the stale value.
fn elem_effect_conflicts(graph: &Graph, eff: crate::ir::MemEffect, keys: &[ElemKey]) -> bool {
    keys.iter().any(|k| {
        let cell = AliasClass::ArrayElem {
            array: k.array,
            index: crate::ir::AccessOffset::Dynamic(k.index),
            elem: k.kind,
        };
        graph.may_alias(eff.writes, cell) || graph.may_alias(eff.reads, cell)
    })
}

/// `CRATONVM_DBG_LOOP_FIELD_FORWARD=1`: why a loop's array element was not
/// promoted.
fn elem_promote_refuse(header_bci: usize, why: &str, node: NodeId) -> usize {
    if false
        || super::loop_pass_report_enabled()
    {
        eprintln!(
            "[ir] loop array element promote: header bci {header_bci} refused -- {why} (n{node})"
        );
    }
    0
}

/// The value of one promoted element at a memory state inside the loop:
/// [`StaticWalk`]'s walk with the cell's own `ArrayStore`s matched by node.
struct ElemWalk<'a> {
    key: ElemKey,
    fwd: usize,
    header_mem: NodeId,
    init: NodeId,
    body: &'a FxHashSet<NodeId>,
    memo: FxHashMap<NodeId, NodeId>,
    budget: usize,
}

impl ElemWalk<'_> {
    fn value_at(&mut self, graph: &mut Graph, mem: NodeId) -> Option<NodeId> {
        let mut path: Vec<NodeId> = Vec::new();
        let mut cur = mem;
        let answer = loop {
            if let Some(&v) = self.memo.get(&cur) {
                break v;
            }
            self.budget = self.budget.checked_sub(1)?;
            let node = graph.node_opt(cur)?.clone();
            if cur == self.header_mem || (node.op == Op::Phi && node.ty == IrType::Memory) {
                break self.phi_value(graph, cur, &node)?;
            }
            if !self.body.contains(&cur) {
                return None;
            }
            if let Some((a, k, kind, true)) = array_access(graph, cur) {
                if a == self.key.array && k == self.key.index && kind == self.key.kind {
                    // `[ctrl, mem, array, index, value]`.
                    break node.input_opt(4)?;
                }
            }
            let token = crate::ir::memory_token_slot(&node).and_then(|t| node.input_opt(t))?;
            path.push(cur);
            cur = token;
        };
        for p in path {
            self.memo.insert(p, answer);
        }
        Some(answer)
    }

    fn phi_value(
        &mut self,
        graph: &mut Graph,
        phi: NodeId,
        node: &crate::ir::Node,
    ) -> Option<NodeId> {
        let ctrl = node.input_opt(0)?;
        if phi != self.header_mem && !self.body.contains(&ctrl) {
            return None;
        }
        let preds: Vec<NodeId> = node.inputs.as_slice().get(1..)?.to_vec();
        let mut inputs = vec![ctrl];
        inputs.extend(std::iter::repeat(NO_NODE).take(preds.len()));
        let value = graph.add(Op::Phi, self.key.ty(), inputs, node.bytecode_pc);
        self.memo.insert(phi, value);
        for (k, pred_mem) in preds.into_iter().enumerate() {
            let v = if phi == self.header_mem && k == self.fwd {
                self.init
            } else {
                self.value_at(graph, pred_mem)?
            };
            graph.set_input(value, k + 1, v);
        }
        Some(super::dedup_walk_phi(graph, ctrl, value, &mut self.memo))
    }
}

/// Promote the array elements the section describes, loop by loop. Returns
/// how many elements were promoted. `despec` (the VM's registry and the
/// method key; `None` in the unit tests) withholds a header it withdrew.
pub(super) fn promote_loop_array_elements(graph: &mut Graph, despec: ForwardDespec<'_>) -> usize {
    if graph.nodes.iter().any(|n| {
        n.op != Op::Dead
            && n.op.memory_shape().is_some()
            && crate::ir::memory_token_slot(n).is_none()
    }) {
        if super::loop_pass_report_enabled() {
            eprintln!("[ir] loop array element promote: refused -- a compact-form memory op");
        }
        return 0;
    }
    let mut promoted = 0usize;
    let mut tried: FxHashSet<NodeId> = FxHashSet::default();
    loop {
        let loops = loop_headers(graph);
        let headers: FxHashSet<NodeId> = loops.iter().map(|l| l.0).collect();
        let Some((region, entry_pred, back_ctrls)) =
            loops.into_iter().find(|l| !tried.contains(&l.0))
        else {
            break;
        };
        tried.insert(region);
        // j27 (lane FRAMES): collect the pre-header copy a refusal left.
        promoted += super::orphan_frames::collecting_orphaned_frames(graph, |graph| {
            promote_elem_loop(graph, region, entry_pred, &back_ctrls, &headers, despec)
        });
    }
    if promoted > 0 && crate::x64::dbg_jitc_enabled() {
        eprintln!("[ir] loop array element promote: {promoted} element(s) kept in registers");
    }
    // JIT r21 wave 1 (w1 host run: no line at all for `arrayElem`): say that
    // the pass ran, so a silent run means the compile never reached it.
    if super::loop_pass_report_enabled() {
        eprintln!(
            "[ir] loop array element promote: ran over {} loop(s) (osr_compile={}), {promoted} promoted",
            tried.len(),
            graph.osr_compile
        );
    }
    promoted
}

fn promote_elem_loop(
    graph: &mut Graph,
    region: NodeId,
    entry_pred: NodeId,
    back_ctrls: &[NodeId],
    headers: &FxHashSet<NodeId>,
    despec: ForwardDespec<'_>,
) -> usize {
    let Some(header_bci) = graph.node_opt(region).and_then(|n| n.bytecode_pc) else {
        return 0;
    };
    if graph.promoted_cell_header_bcis.contains(&header_bci) {
        return elem_promote_refuse(header_bci, "a header another promotion took", region);
    }
    if despec.is_some_and(|(reg, key)| {
        u32::try_from(header_bci).is_ok_and(|bci| reg.contains(key, bci))
    }) {
        return elem_promote_refuse(header_bci, "a de-specialised header", region);
    }
    let Some(fwd) = graph
        .node_opt(region)
        .and_then(|n| n.inputs.iter().position(|&c| c == entry_pred))
    else {
        return 0;
    };
    let mem_phis: Vec<NodeId> = graph
        .users_of(region)
        .into_iter()
        .filter(|&u| {
            graph.node_opt(u).is_some_and(|n| {
                n.op == Op::Phi && n.ty == IrType::Memory && n.input_opt(0) == Some(region)
            })
        })
        .collect();
    let [header_mem] = mem_phis.as_slice() else {
        return elem_promote_refuse(header_bci, "not exactly one header memory phi", region);
    };
    let header_mem = *header_mem;
    let mut body = loop_body(graph, region, entry_pred, back_ctrls);
    if headers.iter().any(|&h| h != region && body.contains(&h)) {
        return elem_promote_refuse(header_bci, "an inner loop", region);
    }
    let inner: Vec<NodeId> = graph
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| {
            n.op == Op::Phi
                && n.input_opt(0)
                    .is_some_and(|c| c != region && body.contains(&c))
        })
        // Cast: arena index to node id.
        .map(|(id, _)| id as NodeId)
        .collect();
    body.extend(inner);

    // The one exit: the header's own test.
    let mut exits: Vec<NodeId> = Vec::new();
    for (id, n) in graph.nodes.iter().enumerate() {
        // Cast: arena index to node id.
        let id = id as NodeId;
        if n.op == Op::Dead || !n.op.is_control() || body.contains(&id) {
            continue;
        }
        if n.inputs.iter().any(|&i| i != NO_NODE && body.contains(&i)) {
            exits.push(id);
        }
    }
    let [exit] = exits.as_slice() else {
        return elem_promote_refuse(header_bci, "not exactly one exit edge", region);
    };
    let exit = *exit;
    let exit_from_header = graph.node_opt(exit).is_some_and(|x| {
        matches!(x.op, Op::Proj(_))
            && x.input_opt(0)
                .and_then(|i| graph.node_opt(i))
                .is_some_and(|i| i.op == Op::If && i.input_opt(0) == Some(region))
    });
    if !exit_from_header {
        return elem_promote_refuse(header_bci, "the exit is not the header's test", exit);
    }

    // Census. Every array access of the loop, by kind; every other node must
    // be unable to transfer, order or write `Any`.
    let mut body_nodes: Vec<NodeId> = body.iter().copied().collect();
    body_nodes.sort_unstable();
    let mut accesses: Vec<(NodeId, ElemKey, bool)> = Vec::new();
    // In-loop `a.length` reads: admitted only for a cell's own array, which the
    // pre-header has dereferenced (its null check then cannot fire; an array's
    // length never changes). Any other one can deopt.
    let mut lengths: Vec<(NodeId, NodeId)> = Vec::new();
    // JIT r21 wave 1 (rev-ir finding 3): every other node of the body, whose
    // memory effect is checked against the cells below by the graph's own
    // classification (`Graph::memory_effect` / `Graph::may_alias`), not by
    // opcode.
    let mut others: Vec<NodeId> = Vec::new();
    for &id in &body_nodes {
        let Some(node) = graph.node_opt(id) else {
            continue;
        };
        if node.op == Op::Dead || node.op == Op::Phi {
            continue;
        }
        if node.is_volatile_access() {
            return elem_promote_refuse(header_bci, "a volatile access", id);
        }
        if matches!(node.op, Op::ArrayLoad(_) | Op::ArrayStore(_)) {
            let Some((array, index, kind, is_store)) = array_access(graph, id) else {
                return elem_promote_refuse(header_bci, "an array access the pass cannot read", id);
            };
            accesses.push((
                id,
                ElemKey {
                    array,
                    index,
                    kind,
                },
                is_store,
            ));
            continue;
        }
        others.push(id);
        if node.op == Op::ArrayLength {
            if let Some(array) = node.input_opt(2) {
                lengths.push((id, array));
                continue;
            }
        }
        if direct_static_cell(graph, id).is_some() {
            continue;
        }
        let eff = graph.memory_effect(id);
        if eff.order != MemOrder::Plain || matches!(eff.writes, AliasClass::Any) {
            return elem_promote_refuse(header_bci, "an ordering or opaque write", id);
        }
        if !crate::ir_lower::node_cannot_deopt(graph, node) {
            return elem_promote_refuse(header_bci, "a node that can deopt", id);
        }
    }
    // The cells: per element kind, every access of that kind is the SAME
    // `(array, index)` pair, both invariant, `int` or `long`, with a store. An
    // access of any other kind, or of a kind with two pairs, keeps the loop
    // (it can deopt, or it may alias the cell).
    let mut keys: Vec<ElemKey> = Vec::new();
    for &(id, key, _) in &accesses {
        if !matches!(key.kind, MemKind::Int | MemKind::Long) {
            return elem_promote_refuse(header_bci, "an array access of another kind", id);
        }
        if keys.iter().any(|k| k.kind == key.kind && *k != key) {
            return elem_promote_refuse(header_bci, "two elements of one kind (may alias)", id);
        }
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    for &key in &keys {
        let invariant = |v: NodeId| super::is_loop_invariant(graph, v, region, &body);
        if body.contains(&key.array)
            || body.contains(&key.index)
            || !invariant(key.array)
            || !invariant(key.index)
        {
            return elem_promote_refuse(
                header_bci,
                "an element of a loop-variant array or index",
                region,
            );
        }
        if !accesses.iter().any(|&(_, k, st)| k == key && st) {
            return elem_promote_refuse(header_bci, "an element the loop only reads", region);
        }
        let index_ty = graph.node_opt(key.index).map(|n| n.ty);
        let array_ty = graph.node_opt(key.array).map(|n| n.ty);
        if index_ty != Some(IrType::Int) || array_ty != Some(IrType::Ref) {
            return elem_promote_refuse(header_bci, "an untyped element", region);
        }
    }
    if let Some(&(id, _)) = lengths
        .iter()
        .find(|&&(_, array)| !keys.iter().any(|k| k.array == array))
    {
        return elem_promote_refuse(header_bci, "a length read of another array", id);
    }
    // rev-ir finding 3: a node that is no `ArrayLoad` / `ArrayStore` but may
    // read or write a cell (an intrinsic fill or copy, a raw / `Unsafe` /
    // `VarHandle` access, anything the effect model places in an array
    // class) would be walked past by `ElemWalk` (it is no cell store) and
    // would see, or overwrite, memory the promotion keeps stale. Refused
    // unless its effect is provably disjoint from every cell.
    // A DIRECT static access is the one exemption: a class's statics block is
    // no array's storage (the premise `Graph::may_alias` records for
    // `AliasClass::Static`), and its op-level `Opaque` shape is the helper
    // route's, which a direct access never takes.
    if let Some(&id) = others.iter().find(|&&id| {
        direct_static_cell(graph, id).is_none()
            && elem_effect_conflicts(graph, graph.memory_effect(id), &keys)
    }) {
        return elem_promote_refuse(
            header_bci,
            "a node whose memory effect may touch the element",
            id,
        );
    }
    if keys.is_empty() {
        return elem_promote_refuse(header_bci, "no array element cell", region);
    }

    // Every memory user outside the loop must read the header's memory φ.
    let mut outside: Vec<(NodeId, usize)> = Vec::new();
    for (id, n) in graph.nodes.iter().enumerate() {
        // Cast: arena index to node id.
        let id = id as NodeId;
        if n.op == Op::Dead || body.contains(&id) || id == header_mem {
            continue;
        }
        for (i, &inp) in n.inputs.iter().enumerate() {
            if inp == NO_NODE || !(inp == header_mem || body.contains(&inp)) {
                continue;
            }
            if !crate::ir::is_memory_token_slot(n, i) {
                continue;
            }
            if inp != header_mem {
                return elem_promote_refuse(header_bci, "loop memory leaves past the header", id);            }
            outside.push((id, i));
        }
    }
    let Some(si) = promotion_header_frame(graph, header_bci) else {
        return elem_promote_refuse(header_bci, "no header frame", region);
    };
    let Some(pre_si) = preheader_snapshot(graph, region, fwd, header_bci, &body) else {
        return elem_promote_refuse(header_bci, "no pre-header frame", region);
    };

    // The pre-header: the length, the two index guards, then the read,
    // threaded onto the forward memory edge (see the section comment).
    let Some(mut mem) = graph
        .node_opt(header_mem)
        .and_then(|n| n.input_opt(fwd + 1))
    else {
        return 0;
    };
    let pc = Some(header_bci);
    let mut initial: Vec<(ElemKey, NodeId)> = Vec::with_capacity(keys.len());
    for &key in &keys {
        let mut made: Vec<NodeId> = Vec::new();
        let len = graph.add(
            Op::ArrayLength,
            IrType::Int,
            vec![entry_pred, mem, key.array],
            pc,
        );
        made.push(len);
        let zero = graph.add(Op::Const(0), IrType::Int, vec![], None);
        let non_negative = graph.add(
            Op::Cmp(crate::ir::CmpOp::Le),
            IrType::Int,
            vec![zero, key.index],
            pc,
        );
        let g_low = graph.add(
            Op::Guard { bci: header_bci },
            IrType::Void,
            vec![entry_pred, non_negative],
            pc,
        );
        made.push(g_low);
        let below = graph.add(
            Op::Cmp(crate::ir::CmpOp::Lt),
            IrType::Int,
            vec![key.index, len],
            pc,
        );
        let g_high = graph.add(
            Op::Guard { bci: header_bci },
            IrType::Void,
            vec![entry_pred, below],
            pc,
        );
        made.push(g_high);
        let read = graph.add(
            Op::ArrayLoad(key.kind),
            key.ty(),
            vec![entry_pred, mem, key.array, key.index],
            pc,
        );
        made.push(read);
        if !made.iter().all(|&n| graph.set_node_frame_snapshot(n, pre_si)) {
            // Unreachable: every node's pc is the snapshot's bci. Undo this
            // element and leave the loop: nothing is threaded yet (the
            // header's forward memory input is set only below), so the
            // earlier elements' pre-header nodes are unused and reclaimed.
            for n in made {
                graph.kill(n);
            }
            graph.kill(below);
            graph.kill(non_negative);
            return elem_promote_refuse(header_bci, "a pre-header node without its frame", region);
        }
        mem = read;
        initial.push((key, read));
    }
    if initial.is_empty() {
        return 0;
    }
    graph.set_input(header_mem, fwd + 1, mem);

    // Each element's in-loop reads and its value at the exit, walked before
    // any node is deleted. Unlike a static cell's, a left-over element access
    // CAN deopt (its null / bounds checks), and it would then transfer with
    // the other elements stale in registers: so if any walk fails, nothing is
    // promoted (the pre-header nodes stay: guards that deopt to the loop
    // entry and a checked extra read, never a wrong one).
    #[allow(clippy::type_complexity)]
    let mut ready: Vec<(ElemKey, NodeId, NodeId, Vec<(NodeId, NodeId)>)> = Vec::new();
    for &(key, init) in &initial {
        let mut walk = ElemWalk {
            key,
            fwd,
            header_mem,
            init,
            body: &body,
            memo: FxHashMap::default(),
            budget: MAX_WALK,
        };
        let Some(exit_value) = walk.value_at(graph, header_mem) else {
            continue;
        };
        let mut answers: Vec<(NodeId, NodeId)> = Vec::new();
        let mut failed = false;
        for &(ld, k, is_store) in &accesses {
            if k != key || is_store {
                continue;
            }
            let token = graph
                .node_opt(ld)
                .and_then(|n| crate::ir::memory_token_slot(n).and_then(|t| n.input_opt(t)));
            match token.and_then(|t| walk.value_at(graph, t)) {
                Some(v) => answers.push((ld, v)),
                None => {
                    failed = true;
                    break;
                }
            }
        }
        if failed {
            continue;
        }
        ready.push((key, init, exit_value, answers));
    }
    if ready.is_empty() || ready.len() != keys.len() {
        return elem_promote_refuse(header_bci, "no exit value", region);
    }

    // The write-backs: one `if (v != init) a[k] = v` diamond per element,
    // chained on the exit edge.
    let exit_users: Vec<NodeId> = graph.users_of(exit);
    let mut ctrl = exit;
    let mut wmem = header_mem;
    for &(key, init, value, _) in &ready {
        let cond = if key.kind == MemKind::Long {
            let three_way = graph.add(Op::LCmp, IrType::Int, vec![value, init], pc);
            let zero = graph.add(Op::Const(0), IrType::Int, vec![], None);
            graph.add(
                Op::Cmp(crate::ir::CmpOp::Ne),
                IrType::Int,
                vec![three_way, zero],
                pc,
            )
        } else {
            graph.add(
                Op::Cmp(crate::ir::CmpOp::Ne),
                IrType::Int,
                vec![value, init],
                pc,
            )
        };
        let iff = graph.add(Op::If, IrType::Control, vec![ctrl, cond], pc);
        let taken = graph.add(Op::Proj(0), IrType::Control, vec![iff], pc);
        let skipped = graph.add(Op::Proj(1), IrType::Control, vec![iff], pc);
        let store = graph.add(
            Op::ArrayStore(key.kind),
            IrType::Memory,
            vec![taken, wmem, key.array, key.index, value],
            pc,
        );
        graph.set_node_frame_snapshot(store, si);
        let join = graph.add(Op::Merge, IrType::Control, vec![taken, skipped], pc);
        let phi = graph.add(Op::Phi, IrType::Memory, vec![join, store, wmem], pc);
        ctrl = join;
        wmem = phi;
    }
    for user in exit_users {
        let Some(n) = graph.node_opt(user) else {
            continue;
        };
        let slots: Vec<usize> = n
            .inputs
            .iter()
            .enumerate()
            .filter(|&(_, &x)| x == exit)
            .map(|(i, _)| i)
            .collect();
        for slot in slots {
            graph.set_input(user, slot, ctrl);
        }
    }
    for (user, slot) in outside {
        graph.set_input(user, slot, wmem);
    }

    // The in-loop reads take their values (resolved through the map: a read
    // may be answered by another removed read, `a[k] = a[k]`).
    let map: FxHashMap<NodeId, NodeId> = ready.iter().flat_map(|r| r.3.iter().copied()).collect();
    let resolve = |mut v: NodeId| {
        let mut seen = 0usize;
        while let Some(&next) = map.get(&v) {
            v = next;
            seen += 1;
            if seen > map.len() {
                return NO_NODE;
            }
        }
        v
    };
    let mut answered: Vec<(NodeId, NodeId)> =
        map.iter().map(|(&ld, &v)| (ld, resolve(v))).collect();
    answered.sort_unstable();
    for (ld, v) in answered {
        if v == NO_NODE || v == ld || !bypass_token(graph, ld) {
            continue;
        }
        graph.replace_all_uses(ld, v);
        graph.kill(ld);
    }
    // The in-loop stores of the promoted elements go.
    let mut done = 0usize;
    for &(key, _, _, _) in &ready {
        for &(st, k, is_store) in &accesses {
            if k == key && is_store && bypass_token(graph, st) {
                graph.kill(st);
            }
        }
        done += 1;
    }
    if !graph.promoted_cell_header_bcis.contains(&header_bci) {
        graph.promoted_cell_header_bcis.push(header_bci);
    }
    if crate::x64::dbg_jitc_enabled() {
        eprintln!(
            "[ir] loop array element promote: header bci {header_bci}: {done} element(s), stores sunk to the exit"
        );
    }
    done
}



/// JIT round 20 wave 11 (lane pLoop, OSR4-2): the static cell promotion on
/// hand-built `COUNTER += i` loops.
#[cfg(test)]
mod jr20_ploop_static_promote_tests;

#[cfg(test)]
mod jr20_patom2_fresh_promote_tests;



#[cfg(test)]
mod r21_loops_array_element_promote_tests;



#[cfg(test)]
mod r21_loops_elem_effect_tests;

// Forward the fields this module's header describes. Returns how many reads
// were removed. `only`, when given, names the loop headers to visit (the
// method-entry use, nio21 wave 4); `None` visits every loop (an OSR-door
// graph).
// JIT round 20 wave 3: production callers pass the registry
// (`forward_loop_fields_with`); the unit tests use this form, which is
// `#[cfg(test)]` at the file's end since JIT round 22 wave 7 (lane review).
#[cfg(test)]
pub(super) fn forward_loop_fields(graph: &mut Graph, only: Option<&FxHashSet<NodeId>>) -> usize {
    forward_loop_fields_with(graph, only, None)
}

