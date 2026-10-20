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
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_LOOP_FIELD_FORWARD")
}

/// `CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_PREDICATED` (nio21 wave 4, lane L5;
/// **default ON**, `=0` is the kill switch): run the forward in a
/// METHOD-ENTRY body too, for the loops `class_predicate` guarded
/// (`ir_optimize::optimize_passes`). Needs the general switch as well.
pub(super) fn predicated_forward_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_PREDICATED")
}

/// Rounds of [`forward_loop_fields`]: a forwarded reference field makes the
/// object it names invariant, and the next round forwards that object's
/// fields (the iterator's `this$0`, then the list's `size`).
const MAX_ROUNDS: usize = 3;

/// The most memory nodes one forwarded field's chain walk may visit.
const MAX_WALK: usize = 4096;

/// Forward the fields this module's header describes. Returns how many reads
/// were removed. `only`, when given, names the loop headers to visit (the
/// method-entry use, nio21 wave 4); `None` visits every loop (an OSR-door
/// graph).
// JIT round 20 wave 3: production callers pass the registry
// (`forward_loop_fields_with`); the unit tests use this form.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn forward_loop_fields(graph: &mut Graph, only: Option<&FxHashSet<NodeId>>) -> usize {
    forward_loop_fields_with(graph, only, None)
}

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
        eprintln!("[ir] loop field forward: {removed} read(s) removed");
    }
    removed
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
    Some((base, idx, kind))
}

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
        removed += forward_loop(graph, region, entry_pred, &back_ctrls, despec);
    }
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
        if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_LOOP_FIELD_FORWARD") {
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
    let mut null_guarded: FxHashSet<NodeId> = FxHashSet::default();
    let mut null_const: Option<NodeId> = None;
    for key in &keys {
        let base = key.base;
        if null_guarded.contains(&base)
            || exact_at_entry.contains_key(&base)
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
            vec![entry_pred, mem, key.base, idx_node],
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
/// loopopt) -- **default OFF** (opt-in): forward a base that is not known
/// non-null too, behind a pre-header `Guard{bci: header}(base != null)`.
/// Off, such a base is not forwarded (correctness: see `forward_loop`). Kept
/// for an A/B on the OSR for-each iterator rows once the guarded route's
/// failure on `R20ReviewForwardNullBase` is explained. Read once per loop.
pub(super) fn null_guard_route_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_NULL_GUARD")
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
    cratonvm_types::flags::runtime_flag_default_on(
        "CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_DISTINCT_CLASSES",
    )
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
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_RELOAD")
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
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_RELOAD_JOIN")
}

/// `CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_RELOAD_SPLICED` (nio21 wave 19, lane
/// L3; **default ON**, `=0` refuses shape 3 of [`post_writer_frame`] as wave
/// 18 did). Needs `CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_RELOAD`.
pub(super) fn splice_reload_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on(
        "CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_RELOAD_SPLICED",
    )
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
    for n in graph.nodes.iter() {
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
        if class_id == 0 {
            continue;
        }
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

/// `CRATONVM_JIT_IR_STATIC_FINAL_EXACT_CLASS`: let `read`, a load of the same
/// field as `sample` placed at a synthetic pc, take the compact row `sample`
/// takes (its own `getfield` pc's, or the row a pass named for it), so the
/// forwarded read stays the inline compact read instead of falling to the
/// legacy-only arm and the checked helper for a compact object.
fn inherit_field_row(graph: &mut Graph, sample: NodeId, read: NodeId) {
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
    if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_LOOP_FIELD_FORWARD")
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
fn benign_opaque(op: &Op) -> bool {
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
fn class_tested_in(graph: &Graph, base: NodeId, body_nodes: &[NodeId]) -> bool {
    let inputs_too = cratonvm_types::flags::runtime_flag_default_on(
        "CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_CLASS_INPUTS",
    );
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
fn base_outside_ea(graph: &Graph, base: NodeId) -> bool {
    if graph.node_opt(base).map(|n| n.ty) != Some(IrType::Ref) {
        return false;
    }
    let origin = graph.ref_origin(base);
    !(origin.allocs.as_ref().is_some_and(|a| !a.is_empty()) && !origin.pre_existing)
}

/// Push the pre-header's frame (see the module comment) and return its index.
fn preheader_snapshot(
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
            let chains: Vec<usize> = graph
                .safepoints
                .iter()
                .enumerate()
                .filter(|(_, s)| s.bci == header_bci && s.is_splice_state() && !s.post_call_only)
                .map(|(i, _)| i)
                .collect();
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
        Some(value)
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
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE")
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
        eprintln!("[ir] loop field promote: {promoted} cell(s) kept in registers");
    }
    promoted
}

/// `CRATONVM_DBG_LOOP_FIELD_FORWARD=1`: why a loop's cells were not promoted.
fn promote_refuse(header_bci: usize, why: &str, node: NodeId) -> usize {
    if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_LOOP_FIELD_FORWARD")
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
        m = token;
    }
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
            Op::Guard { .. } => {
                if guard_is_proven_null_check(graph, id, &proven) {
                    continue;
                }
                if pending {
                    deopt_inside = true;
                    deopt_nodes.push(id);
                    continue;
                }
                return promote_refuse(header_bci, "a guard that can deopt", id);
            }
            // OSR2-3: a primitive array element read or an array length read
            // (array / length alias classes: never a field cell) deopts only
            // through its null and bounds pads, which write the cells back.
            Op::ArrayLoad(kind) | Op::ArrayStore(kind)
                if pending
                    && !matches!(kind, MemKind::Ref)
                    && graph.memory_effect(id).order == MemOrder::Plain =>
            {
                deopt_inside = true;
                deopt_nodes.push(id);
                continue;
            }
            Op::ArrayLength if pending && graph.memory_effect(id).order == MemOrder::Plain => {
                deopt_inside = true;
                deopt_nodes.push(id);
                continue;
            }
            _ => {}
        }
        let eff = graph.memory_effect(id);
        if eff.order != MemOrder::Plain || matches!(eff.writes, AliasClass::Any) {
            return promote_refuse(header_bci, "an ordering or opaque write", id);
        }
        if !crate::ir_lower::node_cannot_deopt(graph, node) {
            return promote_refuse(header_bci, "a node that can deopt", id);
        }
    }

    // OSR5-2 (round 19 wave 6, lane osr6,
    // `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PREDICT`): a deopt point the lowering
    // would give no write-back pad refuses the whole compile; the causes the
    // graph already shows refuse this loop's promotion instead.
    if deopt_inside && crate::ir_lower::ir_loop_field_promote_predict_enabled() {
        if let Some((id, why)) = predicted_pad_refusal(graph, &deopt_nodes) {
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
        cells.push((key, init, own));
    }
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
///   value may be in a frame word instead.
///
/// `None` leaves the rest to the lowering, whose refusal then costs one retry
/// (`Graph::promote_pending_withheld`), not the method.
fn predicted_pad_refusal(graph: &Graph, deopt_nodes: &[NodeId]) -> Option<(NodeId, &'static str)> {
    let splices = crate::ir::splice_resume_map_this_build();
    predicted_pad_refusal_in(graph, deopt_nodes, splices.as_ref())
}

/// [`predicted_pad_refusal`] against the build's splice map `splices`
/// (`None`: nothing was spliced).
fn predicted_pad_refusal_in(
    graph: &Graph,
    deopt_nodes: &[NodeId],
    splices: Option<&crate::ir::IrSpliceResumeMap>,
) -> Option<(NodeId, &'static str)> {
    // Wave 7 (lane osr7, OSR5-1): under the chain switch a spliced point may
    // take its pad (a chain point does; a flat one still refuses in the
    // lowering, which the retry absorbs), so it is not predicted here.
    let chain_pads = crate::ir_lower::ir_loop_field_promote_pending_chain_enabled();
    let spliced = |pc: usize| !chain_pads && splices.is_some_and(|m| pc >= m.code_len);
    let xmm_saved = crate::ir_lower::ir_loop_field_promote_pad_xmm_save_enabled();
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
/// wave 5, lane osr5, OSR2-4). **Default OFF**. On top of
/// `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE`: a loop with exits from inside its
/// body (a `break` / an early `return` path) besides the header's own test is
/// promoted too; each exit gets its own conditional write-back of the cells'
/// values at that exit. Admitted only when every interior exit is reached
/// from the header without a join (`interior_exit_spine`), the body's writes
/// form one memory chain (`body_write_chain`), and every memory user outside
/// the loop is attributable to one exit (`outside_user_exit`). Read without a
/// `static` (`process_global_statics_ratchet`).
pub(crate) fn body_exits_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_BODY_EXITS")
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
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PENDING_DEOPT")
}

/// `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_JOIN_WALK=1` (JIT round 19 proposal
/// wave 8, lane osr8, OSR7-2). **Default OFF**. Two things, both about the
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
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_JOIN_WALK")
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
/// OSR4-2 of `docs/internal/proposals/jit/jit-r19-osr4-proposals.md`):
/// **default ON**, `=0` is the kill switch. Inert without
/// `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE` (the shared poll write-back machinery
/// and the same OSR-door placement). Read without a `static`
/// (`process_global_statics_ratchet`), once per optimize.
pub(crate) fn static_promote_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_LOOP_STATIC_PROMOTE")
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
    cratonvm_types::flags::runtime_flag_default_on(
        "CRATONVM_JIT_IR_LOOP_STATIC_PROMOTE_METHOD_ENTRY",
    )
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
            is_volatile: false,
        } => (class_id, field_index, type_tag),
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
        promoted += promote_static_loop(graph, region, entry_pred, &back_ctrls, &headers);
    }
    if promoted > 0 && crate::x64::dbg_jitc_enabled() {
        eprintln!("[ir] loop static promote: {promoted} static cell(s) kept in registers");
    }
    promoted
}

/// `CRATONVM_DBG_LOOP_FIELD_FORWARD=1`: why a loop's static cells were not
/// promoted.
fn static_promote_refuse(header_bci: usize, why: &str, node: NodeId) -> usize {
    if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_LOOP_FIELD_FORWARD")
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
        Some(value)
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
    // write of a static cell is a direct `putstatic`.
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
        let eff = graph.memory_effect(id);
        if eff.order != MemOrder::Plain || matches!(eff.writes, AliasClass::Any) {
            return static_promote_refuse(header_bci, "an ordering or opaque write", id);
        }
        if !crate::ir_lower::node_cannot_deopt(graph, node) {
            return static_promote_refuse(header_bci, "a node that can deopt", id);
        }
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
    for &(key, init, value, _) in &ready {
        let (join, phi, store) =
            add_static_cell_writeback(graph, ctrl, wmem, key, init, value, header_bci, si);
        ctrl = join;
        wmem = phi;
        // The lowerer finds the cell from this store when it gives the
        // header's poll a write-back exit (OSR2-1's pad, static arm).
        graph.promoted_cell_writebacks.push((region, store));
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
    // the store's own token.
    let mut done = 0usize;
    for &(key, _, _, _) in &ready {
        for &st in stores.get(&key).map(Vec::as_slice).unwrap_or(&[]) {
            if bypass_token(graph, st) {
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
            "[ir] loop static promote: header bci {header_bci}: {done} static cell(s), stores sunk to the exit"
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
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_LOOP_FRESH_FIELD_PROMOTE")
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
        && cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_JIT_IR_FRESH_PROMOTE_STATIC_FEED",
        )
}

/// `CRATONVM_DBG_LOOP_FIELD_FORWARD=1`: why a loop's fresh-object cells were
/// not promoted.
fn fresh_promote_refuse(header_bci: usize, why: &str, node: NodeId) -> usize {
    if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_LOOP_FIELD_FORWARD")
        || super::loop_pass_report_enabled()
    {
        eprintln!(
            "[ir] loop fresh field promote: header bci {header_bci} refused -- {why} (n{node})"
        );
    }
    0
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

/// `CRATONVM_DBG_LOOP_FIELD_FORWARD=1` (JIT round 21 wave 1, lane loops,
/// LOOP21-1): an ALLOCATION of this compile whose fields a loop accesses
/// failed [`fresh_base_unpublished`] at `user`: name the use (op and pc), the
/// line the `R20PLoopStaticBarrier` engagement question needs. Silent for a
/// base that is no allocation (a parameter, a load), which was never a
/// candidate.
fn note_fresh_publication(graph: &Graph, header_bci: usize, base: NodeId, user: NodeId) {
    if !cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_LOOP_FIELD_FORWARD") {
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

/// Promote the fresh-object cells the section describes, loop by loop.
/// Returns how many cells were promoted.
pub(super) fn promote_fresh_loop_fields(graph: &mut Graph) -> usize {
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
        promoted += promote_fresh_loop(graph, region, entry_pred, &back_ctrls, &headers);
    }
    if promoted > 0 && crate::x64::dbg_jitc_enabled() {
        eprintln!("[ir] loop fresh field promote: {promoted} cell(s) kept in registers");
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
}

impl FreshWalk<'_> {
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
        Some(value)
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
        return fresh_promote_refuse(header_bci, "a header another promotion took", region);
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
        return fresh_promote_refuse(header_bci, "not exactly one header memory phi", region);
    };
    let header_mem = *header_mem;
    let mut body = loop_body(graph, region, entry_pred, back_ctrls);
    if headers.iter().any(|&h| h != region && body.contains(&h)) {
        return fresh_promote_refuse(header_bci, "an inner loop", region);
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
        return fresh_promote_refuse(header_bci, "not exactly one exit edge", region);
    };
    let exit = *exit;
    let exit_from_header = graph.node_opt(exit).is_some_and(|x| {
        matches!(x.op, Op::Proj(_))
            && x.input_opt(0)
                .and_then(|i| graph.node_opt(i))
                .is_some_and(|i| i.op == Op::If && i.input_opt(0) == Some(region))
    });
    if !exit_from_header {
        return fresh_promote_refuse(header_bci, "the exit is not the header's test", exit);
    }

    // Census. A field access through a fresh, unpublished base defined
    // outside the loop, at an `int` / `long` identified field, is a candidate
    // cell; every other node must be unable to transfer, order or write `Any`.
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
            return fresh_promote_refuse(header_bci, "a volatile access", id);
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
                        Ok(()) => true,
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
        let eff = graph.memory_effect(id);
        if eff.order != MemOrder::Plain || matches!(eff.writes, AliasClass::Any) {
            return fresh_promote_refuse(header_bci, "an ordering or opaque write", id);
        }
        if !crate::ir_lower::node_cannot_deopt(graph, node) {
            return fresh_promote_refuse(header_bci, "a node that can deopt", id);
        }
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
                return fresh_promote_refuse(header_bci, "a read of an unpromoted fresh field", ld);
            }
        }
    }
    keys.sort_by_key(|k| (k.base, k.idx, k.decl));
    if keys.is_empty() {
        return fresh_promote_refuse(header_bci, "no fresh field cell", region);
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
                return fresh_promote_refuse(header_bci, "loop memory leaves past the header", id);
            }
            outside.push((id, i));
        }
    }
    let Some(si) = promotion_header_frame(graph, header_bci) else {
        return fresh_promote_refuse(header_bci, "no header frame", region);
    };
    let Some(pre_si) = preheader_snapshot(graph, region, fwd, header_bci, &body) else {
        return fresh_promote_refuse(header_bci, "no pre-header frame", region);
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
        if failed {
            continue;
        }
        ready.push((key, init, idx_node, exit_value, answers));
    }
    if ready.is_empty() {
        return fresh_promote_refuse(header_bci, "no exit value", region);
    }

    // The write-backs, chained on the exit edge.
    let exit_users: Vec<NodeId> = graph.users_of(exit);
    let mut ctrl = exit;
    let mut wmem = header_mem;
    for (key, init, idx_node, value, _) in &ready {
        let (join, phi) = add_cell_writeback(
            graph, ctrl, wmem, key, *init, *idx_node, *value, header_bci, si,
        );
        ctrl = join;
        wmem = phi;
        // The lowerer finds the cell from this store when it gives the
        // header's poll a write-back exit (OSR2-1's pad).
        if let Some(store) = graph.node_opt(phi).and_then(|n| n.input_opt(1)) {
            graph.promoted_cell_writebacks.push((region, store));
        }
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
    // The in-loop stores of the promoted cells go.
    let mut done = 0usize;
    for (key, _, _, _, _) in &ready {
        for &st in stores.get(key).map(Vec::as_slice).unwrap_or(&[]) {
            if bypass_token(graph, st) {
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
            "[ir] loop fresh field promote: header bci {header_bci}: {done} fresh-object cell(s), stores sunk to the exit"
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
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_METHOD_ENTRY")
}

/// `CRATONVM_JIT_IR_LOOP_PROMOTE_SPLICED` (JIT round 21 wave 1, lane loops;
/// perf, **default OFF**, opt-in `=1`): the cell promotions reach the loop of
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
pub(crate) fn promote_spliced_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_LOOP_PROMOTE_SPLICED")
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
            let chains: Vec<usize> = graph
                .safepoints
                .iter()
                .enumerate()
                .filter(|(_, s)| s.bci == header_bci && s.is_splice_state() && !s.post_call_only)
                .map(|(i, _)| i)
                .collect();
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
fn base_null_checked_before(graph: &Graph, base: NodeId, entry_pred: NodeId) -> bool {
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
                graph.node_opt(g).is_some_and(|gn| {
                    matches!(gn.op, Op::Guard { .. })
                        && gn.input_opt(1) == Some(t)
                        && gn
                            .input_opt(0)
                            .is_some_and(|c| super::control_dominates(graph, c, entry_pred))
                })
            })
    })
}

/// Is `base` the compiling method's receiver (`Graph::receiver_param`'s
/// `Op::Param`)? `None` (a static method, or a hand-built graph that said
/// nothing) never matches.
fn is_receiver_param(graph: &Graph, base: NodeId) -> bool {
    let Some(recv) = graph.receiver_param else {
        return false;
    };
    graph
        .node_opt(base)
        .is_some_and(|n| n.op == Op::Param(recv) && n.ty == IrType::Ref)
}

/// The method-entry run of the section above: `(reads forwarded, cells
/// promoted)`. The caller asks [`method_entry_promote_enabled`] and the two
/// passes' own switches, and runs it only in a method-entry body.
pub(super) fn promote_method_entry_accumulators(
    graph: &mut Graph,
    despec: ForwardDespec<'_>,
) -> (usize, usize) {
    let headers = super::loop_static_hoist::accumulator_loop_headers(graph);
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
/// loops, LOOPS21-1) -- perf, **default OFF** (opt-in `=1`): the array-element
/// accumulator promotion above, in method-entry and OSR-door bodies (never on
/// the optimizing OSR route). Inert without `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE`.
/// Read once per optimize, no `static`.
pub(crate) fn array_element_promote_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_LOOP_ARRAY_ELEMENT_PROMOTE")
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
    if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_LOOP_FIELD_FORWARD")
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
        Some(value)
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
        promoted += promote_elem_loop(graph, region, entry_pred, &back_ctrls, &headers, despec);
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
                return elem_promote_refuse(header_bci, "loop memory leaves past the header", id);
            }
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

#[cfg(test)]
mod tests {
    use super::forward_loop_fields;
    use crate::ir::{Graph, IrBuilder, IrType, Op};
    use std::collections::HashMap;

    /// `static int f(P p, int n) { int s = 0; for (int i = 0; i < n; i++) {
    /// s += p.x; p.y = i; } return s; }` -- `x` field 0, `y` field 1.
    const READ_ONE_WRITE_OTHER: [u8; 29] = [
        0x03, 0x3d, 0x03, 0x3e, // 0: s = 0; 2: i = 0
        0x1d, 0x1b, 0xa2, 0x00, 0x15, // 4: header: if (i >= n) goto 27
        0x1c, 0x2a, 0xb4, 0x00, 0x01, 0x60, 0x3d, // 9: s += p.x (getfield at 11)
        0x2a, 0x1d, 0xb5, 0x00, 0x02, // 16: p.y = i (putfield at 18)
        0x84, 0x03, 0x01, 0xa7, 0xff, 0xec, // 21: i++; 24: goto 4
        0x1c, 0xac, // 27: return s
    ];

    /// `static int f(P p, Q q, int n) { int s = 0; for (int i = 0; i < n;
    /// i++) { s += p.x; q.z = i; } return s; }` -- `x` and `z` both field 0.
    const READ_P_WRITE_Q: [u8; 32] = [
        0x03, 0x3e, 0x03, 0x36, 0x04, // 0: s = 0; 2: i = 0
        0x15, 0x04, 0x1c, 0xa2, 0x00, 0x16, // 5: header: if (i >= n) goto 30
        0x1d, 0x2a, 0xb4, 0x00, 0x01, 0x60, 0x3e, // 11: s += p.x (getfield at 13)
        0x2b, 0x15, 0x04, 0xb5, 0x00, 0x02, // 18: q.z = i (putfield at 21)
        0x84, 0x04, 0x01, 0xa7, 0xff, 0xea, // 24: i++; 27: goto 5
        0x1d, 0xac, // 30: return s
    ];

    /// `static int f(P p, int n) { for (int i = 0; i < n; i++) p.x = p.x +
    /// 1; return p.x; }`.
    const INCREMENT_IN_PLACE: [u8; 28] = [
        0x03, 0x3d, // 0: i = 0
        0x1c, 0x1b, 0xa2, 0x00, 0x13, // 2: header: if (i >= n) goto 23
        0x2a, 0x2a, 0xb4, 0x00, 0x01, 0x04, 0x60, 0xb5, 0x00, 0x01, // 7: p.x = p.x + 1
        0x84, 0x02, 0x01, 0xa7, 0xff, 0xee, // 17: i++; 20: goto 2
        0x2a, 0xb4, 0x00, 0x01, 0xac, // 23: return p.x (getfield at 24)
    ];

    fn build(
        code: &[u8],
        params: &[IrType],
        max_locals: usize,
        fields: &[(usize, usize)],
        decls: &[(usize, u32)],
    ) -> Graph {
        let mut b = IrBuilder::new(params.len(), max_locals);
        b.set_param_types(params);
        b.set_field_info(fields.iter().map(|&(pc, idx)| (pc, (idx, b'I'))).collect());
        b.set_field_decl_classes(decls.iter().copied().collect::<HashMap<usize, u32>>());
        let mut g = b.build(code, code.len()).expect("builds");
        // The pipeline's cleanup runs first: it folds the builder's `φ(x, x)`
        // for every local the loop never writes, `p` among them.
        super::super::scalar_cleanup(&mut g);
        g
    }

    fn live_loads_at(graph: &Graph, pc: usize) -> usize {
        graph
            .nodes
            .iter()
            .filter(|n| matches!(n.op, Op::Load(_)) && n.bytecode_pc == Some(pc))
            .count()
    }

    /// The forwarding mechanics, on the hand-built loops whose bases are
    /// parameters (not known non-null): with the opt-in guarded route, as the
    /// pass ran before JIT round 20 wave 4.
    fn forward(graph: &mut Graph) -> usize {
        cratonvm_types::flags::with_thread_overrides(
            &[
                ("CRATONVM_JIT_IR_LOOP_FIELD_FORWARD", None),
                ("CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_NULL_GUARD", Some("1")),
            ],
            || forward_loop_fields(graph, None),
        )
    }

    /// JIT round 20 (lane loopopt): a base not known non-null gets a
    /// pre-header null guard at the header bci before its read, and a header
    /// the de-spec registry withdrew forwards no such base.
    #[test]
    fn jr20_a_possibly_null_base_is_null_guarded_and_honours_despec() {
        let fresh = || {
            build(
                &INCREMENT_IN_PLACE,
                &[IrType::Ref, IrType::Int],
                3,
                &[(9, 0), (14, 0), (24, 0)],
                &[(9, 50), (14, 50), (24, 50)],
            )
        };
        // JIT round 20 wave 4: by default a parameter base is not forwarded
        // at all (it may be null on an entry whose trips never read it).
        let mut d = fresh();
        let n_default = cratonvm_types::flags::with_thread_overrides(
            &[
                ("CRATONVM_JIT_IR_LOOP_FIELD_FORWARD", None),
                ("CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_NULL_GUARD", None),
            ],
            || forward_loop_fields(&mut d, None),
        );
        assert_eq!(n_default, 0, "a possibly-null base keeps its in-loop reads");
        let mut g = fresh();
        assert_eq!(forward(&mut g), 1);
        let null_guard = g.nodes.iter().any(|n| {
            matches!(n.op, Op::Guard { bci: 2 })
                && n.frame_snapshot.is_some()
                && n.inputs
                    .get(1)
                    .is_some_and(|&c| g.nodes[c as usize].op == Op::Cmp(crate::ir::CmpOp::Ne))
        });
        assert!(
            null_guard,
            "the pre-header read is guarded against a null base"
        );
        let reg = crate::deopt::DespecRegistry::new();
        reg.insert("T.m:()V", 2, crate::deopt::SPECULATION_ID_ANY);
        let mut h = fresh();
        let n = cratonvm_types::flags::with_thread_overrides(
            &[
                ("CRATONVM_JIT_IR_LOOP_FIELD_FORWARD", None),
                ("CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_NULL_GUARD", Some("1")),
            ],
            || super::forward_loop_fields_with(&mut h, None, Some((&reg, "T.m:()V"))),
        );
        assert_eq!(n, 0, "a de-specialised header keeps its in-loop reads");
    }

    /// The loop reads `p.x` once in its pre-header, at the header's bci with
    /// a frame of its own, and not at all inside; the write to another field
    /// does not stop it.
    #[test]
    fn a_loop_invariant_field_is_read_once_in_the_pre_header() {
        let mut g = build(
            &READ_ONE_WRITE_OTHER,
            &[IrType::Ref, IrType::Int],
            4,
            &[(11, 0), (18, 1)],
            &[(11, 50), (18, 50)],
        );
        assert_eq!(live_loads_at(&g, 11), 1);
        assert_eq!(forward(&mut g), 1);
        assert_eq!(
            live_loads_at(&g, 11),
            0,
            "the loop's read is answered in a register"
        );
        let pre: Vec<_> = g
            .nodes
            .iter()
            .filter(|n| matches!(n.op, Op::Load(_)) && n.bytecode_pc == Some(4))
            .collect();
        let [read] = pre.as_slice() else {
            panic!("one pre-header read, got {}", pre.len());
        };
        let si = read
            .frame_snapshot
            .expect("the pre-header read names its frame");
        assert_eq!(g.safepoints[si as usize].bci, 4);
        assert!(crate::ir_verify::verify_graph_at_phase(&g, "post-optimize").is_ok());
    }

    /// JIT round 20 wave 19 (lane yBuf, MEM20-4): the pre-header read of a
    /// field whose in-loop read carries the `byte[]` fact
    /// (`Graph::byte_array_nodes`) carries it too; a field without it gains
    /// nothing.
    #[test]
    fn jr20_ybuf_the_pre_header_read_keeps_the_byte_array_fact() {
        let fresh = || {
            build(
                &READ_ONE_WRITE_OTHER,
                &[IrType::Ref, IrType::Int],
                4,
                &[(11, 0), (18, 1)],
                &[(11, 50), (18, 50)],
            )
        };
        let pre_read = |g: &Graph| {
            g.nodes
                .iter()
                .enumerate()
                .filter(|(_, n)| matches!(n.op, Op::Load(_)) && n.bytecode_pc == Some(4))
                .map(|(i, _)| i as crate::ir::NodeId)
                .collect::<Vec<_>>()
        };
        let mut g = fresh();
        let in_loop = g
            .nodes
            .iter()
            .position(|n| matches!(n.op, Op::Load(_)) && n.bytecode_pc == Some(11))
            .expect("the loop's read") as crate::ir::NodeId;
        g.byte_array_nodes.insert(in_loop);
        assert_eq!(forward(&mut g), 1);
        let reads = pre_read(&g);
        assert_eq!(reads.len(), 1, "one pre-header read");
        let read = reads[0];
        assert!(
            g.byte_array_nodes.contains(&read),
            "the fact rides the forward"
        );
        let mut plain = fresh();
        assert_eq!(forward(&mut plain), 1);
        let reads = pre_read(&plain);
        assert_eq!(reads.len(), 1, "one pre-header read");
        let read = reads[0];
        assert!(
            !plain.byte_array_nodes.contains(&read),
            "no fact, none invented"
        );
    }

    /// Field identity decides: another object's field 0 with ANOTHER
    /// declaring class leaves `p.x` forwardable; the same declaring class (it
    /// may be `p` itself) or none refuses.
    #[test]
    fn a_write_with_the_same_field_identity_refuses() {
        let params = [IrType::Ref, IrType::Ref, IrType::Int];
        let fields = [(13, 0), (21, 0)];
        let mut other = build(&READ_P_WRITE_Q, &params, 5, &fields, &[(13, 50), (21, 51)]);
        assert_eq!(forward(&mut other), 1);
        let mut same = build(&READ_P_WRITE_Q, &params, 5, &fields, &[(13, 50), (21, 50)]);
        assert_eq!(forward(&mut same), 0);
        let mut unknown = build(&READ_P_WRITE_Q, &params, 5, &fields, &[(13, 50)]);
        assert_eq!(forward(&mut unknown), 0);
    }

    /// A field the loop writes through the same base is answered by the value
    /// the last store wrote, carried around the back edge by a value φ; the
    /// store stays, and so does the read after the loop.
    #[test]
    fn a_field_the_loop_writes_is_forwarded_through_a_value_phi() {
        let mut g = build(
            &INCREMENT_IN_PLACE,
            &[IrType::Ref, IrType::Int],
            3,
            &[(9, 0), (14, 0), (24, 0)],
            &[(9, 50), (14, 50), (24, 50)],
        );
        assert_eq!(forward(&mut g), 1);
        assert_eq!(live_loads_at(&g, 9), 0);
        assert_eq!(live_loads_at(&g, 24), 1, "the read after the loop stays");
        assert!(g
            .nodes
            .iter()
            .any(|n| matches!(n.op, Op::Store(_)) && n.bytecode_pc == Some(14)));
        assert!(
            g.nodes
                .iter()
                .any(|n| n.op == Op::Phi && n.ty == IrType::Int && n.bytecode_pc == Some(2)),
            "a value φ at the header carries the stored value"
        );
        assert!(crate::ir_verify::verify_graph_at_phase(&g, "post-optimize").is_ok());
    }

    /// nio21 wave 4 (lane L5): a PURE class test (`ExactClassIs`, which
    /// `loop_body` never lists) consumed by an `If` of the loop marks its base
    /// class-tested.
    #[test]
    fn nio21w4_a_pure_class_test_feeding_a_loop_branch_marks_the_base() {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let base = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let other = g.add(Op::Param(1), IrType::Ref, vec![start], None);
        let is = g.add(
            Op::ExactClassIs { class_id: 7 },
            IrType::Int,
            vec![base],
            None,
        );
        let iff = g.add(Op::If, IrType::Control, vec![ctrl, is], Some(3));
        assert!(super::class_tested_in(&g, base, &[iff]));
        assert!(!super::class_tested_in(&g, other, &[iff]));
        let old = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_CLASS_INPUTS", Some("0"))],
            || super::class_tested_in(&g, base, &[iff]),
        );
        assert!(!old, "the switch restores the pinned-node-only answer");
    }

    /// nio21 wave 17 (lane L4): the loop-entry exact classes are read from
    /// `Guard{bci: header}(ExactClassIs(base))` on the entry control only; a
    /// base guarded to two classes, a guard on another control or at another
    /// bci, names nothing.
    #[test]
    fn nio21w17_entry_exact_classes_read_the_entry_guards_only() {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let other = g.add(Op::Proj(1), IrType::Control, vec![start], None);
        let src = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let dst = g.add(Op::Param(1), IrType::Ref, vec![start], None);
        let both = g.add(Op::Param(2), IrType::Ref, vec![start], None);
        let far = g.add(Op::Param(3), IrType::Ref, vec![start], None);
        fn guard(g: &mut Graph, base: u32, class_id: u32, ctrl: u32, bci: usize) {
            let c = g.add(Op::ExactClassIs { class_id }, IrType::Int, vec![base], None);
            g.add(Op::Guard { bci }, IrType::Void, vec![ctrl, c], Some(bci));
        }
        guard(&mut g, src, 7, entry, 5);
        guard(&mut g, dst, 9, entry, 5);
        guard(&mut g, both, 7, entry, 5);
        guard(&mut g, both, 9, entry, 5);
        guard(&mut g, far, 11, other, 5);
        guard(&mut g, far, 11, entry, 6);
        let m = super::entry_exact_classes(&g, entry, 5);
        assert_eq!(m.get(&src), Some(&7));
        assert_eq!(m.get(&dst), Some(&9));
        assert!(!m.contains_key(&both), "two classes: conflicted");
        assert!(!m.contains_key(&far), "another control or bci");
        let off = cratonvm_types::flags::with_thread_overrides(
            &[(
                "CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_DISTINCT_CLASSES",
                Some("0"),
            )],
            super::distinct_class_stores_enabled,
        );
        assert!(!off);
    }

    // ── nio21 wave 18 (lane L3): reload after an opaque writer ──────────

    const RELOAD: &str = "CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_RELOAD";

    /// `static int f(P p, int n) { int s = 0; for (int i = 0; i < n; i++) {
    /// s += p.x; if (i == 7) s += g(); } return s; }` -- `x` field 0 (the
    /// `getfield` at 11), `g()I` at 23, its successor 26.
    const CALL_ON_A_COLD_ARM: [u8; 36] = [
        0x03, 0x3d, 0x03, 0x3e, // 0: s = 0; 2: i = 0
        0x1d, 0x1b, 0xa2, 0x00, 0x1c, // 4: header: if (i >= n) goto 34
        0x1c, 0x2a, 0xb4, 0x00, 0x01, 0x60, 0x3d, // 9: s += p.x (getfield at 11)
        0x1d, 0x10, 0x07, 0xa0, 0x00, 0x09, // 16: if (i != 7) goto 28
        0x1c, 0xb8, 0x00, 0x02, 0x60, 0x3d, // 22: s += g() (invokestatic at 23)
        0x84, 0x03, 0x01, 0xa7, 0xff, 0xe5, // 28: i++; 31: goto 4
        0x1c, 0xac, // 34: return s
    ];

    /// The same loop with a VOID call, `if (i == 7) g();`: `g()V` at 22, its
    /// successor 25 is the join of the two arms.
    const VOID_CALL_ON_A_COLD_ARM: [u8; 33] = [
        0x03, 0x3d, 0x03, 0x3e, // 0: s = 0; 2: i = 0
        0x1d, 0x1b, 0xa2, 0x00, 0x19, // 4: header: if (i >= n) goto 31
        0x1c, 0x2a, 0xb4, 0x00, 0x01, 0x60, 0x3d, // 9: s += p.x (getfield at 11)
        0x1d, 0x10, 0x07, 0xa0, 0x00, 0x06, // 16: if (i != 7) goto 25
        0xb8, 0x00, 0x02, // 22: g() (invokestatic at 22)
        0x84, 0x03, 0x01, 0xa7, 0xff, 0xe8, // 25: i++; 28: goto 4
        0x1c, 0xac, // 31: return s
    ];

    fn build_call(code: &[u8], call_pc: usize, ret: u8) -> Graph {
        let mut b = IrBuilder::new(2, 4);
        b.set_param_types(&[IrType::Ref, IrType::Int]);
        b.set_field_info(HashMap::from([(11usize, (0usize, b'I'))]));
        b.set_field_decl_classes(HashMap::from([(11usize, 50u32)]));
        // A live row (leaked: the `Op::Call` bakes its address), so any arm
        // of the builder that reads the row reads a real one.
        let info: &'static crate::JitInvokeInfo = Box::leak(Box::new(crate::JitInvokeInfo {
            class_name: "Q",
            method_name: "g",
            descriptor: if ret == b'V' { "()V" } else { "()I" },
            num_jit_args: 0,
            return_type: ret,
            invoke_kind: 3,
            declaring_class_id: 0,
            owner_class_id: 0,
        }));
        let info_ptr = info as *const crate::JitInvokeInfo as usize;
        b.set_invoke_info(HashMap::from([(call_pc, (info_ptr, 0usize, ret))]));
        let mut g = b.build(code, code.len()).expect("builds");
        super::super::scalar_cleanup(&mut g);
        g
    }

    fn call_node(g: &Graph) -> crate::ir::NodeId {
        let calls: Vec<_> = g
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| matches!(n.op, Op::Call { .. }))
            .map(|(i, _)| i as crate::ir::NodeId)
            .collect();
        let [c] = calls.as_slice() else {
            panic!("one call, got {}", calls.len());
        };
        *c
    }

    fn forward_reload(graph: &mut Graph, on: bool) -> usize {
        cratonvm_types::flags::with_thread_overrides(
            &[
                ("CRATONVM_JIT_IR_LOOP_FIELD_FORWARD", None),
                ("CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_NULL_GUARD", Some("1")),
                (RELOAD, if on { None } else { Some("0") }),
            ],
            || forward_loop_fields(graph, None),
        )
    }

    /// The loop's read of `p.x` is answered in a register although the loop
    /// holds a call: the field is read once in the pre-header and once right
    /// after the call, at the call's successor bci, ordered after the call
    /// and before every memory user the call had.
    #[test]
    fn nio21w18_a_call_in_the_loop_is_followed_by_a_reload() {
        let mut g = build_call(&CALL_ON_A_COLD_ARM, 23, b'I');
        let call = call_node(&g);
        assert_eq!(super::post_writer_frame(&g, call).map(|f| f.succ), Ok(26));
        assert_eq!(live_loads_at(&g, 11), 1);
        assert!(forward_reload(&mut g, true) >= 1);
        assert_eq!(live_loads_at(&g, 11), 0, "the loop's read is answered");
        assert_eq!(live_loads_at(&g, 4), 1, "one pre-header read");
        let reloads: Vec<_> = g
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| matches!(n.op, Op::Load(_)) && n.bytecode_pc == Some(26))
            .map(|(i, _)| i as crate::ir::NodeId)
            .collect();
        let [r] = reloads.as_slice() else {
            panic!("one reload after the call, got {}", reloads.len());
        };
        let rn = g.node_opt(*r).expect("reload");
        assert_eq!(
            rn.input_opt(1),
            Some(call),
            "the reload reads the call's memory"
        );
        assert_eq!(
            rn.input_opt(0),
            g.node_opt(call).and_then(|c| c.input_opt(0))
        );
        for u in g.users_of(call) {
            let un = g.node_opt(u).expect("user");
            for (i, &x) in un.inputs.as_slice().iter().enumerate() {
                if x == call && crate::ir::is_memory_token_slot(un, i) {
                    assert_eq!(u, *r, "only the reload takes the call's memory");
                }
            }
        }
        assert!(g.synthetic_field_access_pcs.contains(&26));
        assert!(g.synthetic_field_access_pcs.contains(&4));
        assert!(crate::ir_verify::verify_graph_at_phase(&g, "post-optimize").is_ok());
    }

    /// A void call whose successor is the arms' join: its frame there is the
    /// call's own (the join adds no φ), so the reload is placed at 25.
    #[test]
    fn nio21w18_a_void_call_reloads_at_its_join_successor() {
        let mut g = build_call(&VOID_CALL_ON_A_COLD_ARM, 22, b'V');
        let call = call_node(&g);
        assert_eq!(super::post_writer_frame(&g, call).map(|f| f.succ), Ok(25));
        assert!(forward_reload(&mut g, true) >= 1);
        assert_eq!(live_loads_at(&g, 11), 0);
        assert_eq!(live_loads_at(&g, 25), 1);
        assert!(crate::ir_verify::verify_graph_at_phase(&g, "post-optimize").is_ok());
    }

    /// `static int f(P p, int n) { int s = 0; for (int i = 0; i < n; i++) {
    /// s += p.x; s += g(); } return s; }` -- the call runs on every trip.
    const CALL_ON_EVERY_TRIP: [u8; 30] = [
        0x03, 0x3d, 0x03, 0x3e, // 0: s = 0; 2: i = 0
        0x1d, 0x1b, 0xa2, 0x00, 0x16, // 4: header: if (i >= n) goto 28
        0x1c, 0x2a, 0xb4, 0x00, 0x01, 0x60, 0x3d, // 9: s += p.x (getfield at 11)
        0x1c, 0xb8, 0x00, 0x02, 0x60, 0x3d, // 16: s += g() (invokestatic at 17)
        0x84, 0x03, 0x01, 0xa7, 0xff, 0xeb, // 22: i++; 25: goto 4
        0x1c, 0xac, // 28: return s
    ];

    /// A call on every trip: the frame is fine, but no trip avoids the
    /// writer, so the loop is left as it was (each read would only be traded
    /// for a reload).
    #[test]
    fn nio21w18_a_call_on_every_trip_is_left_alone() {
        let mut g = build_call(&CALL_ON_EVERY_TRIP, 17, b'I');
        let call = call_node(&g);
        assert_eq!(super::post_writer_frame(&g, call).map(|f| f.succ), Ok(20));
        assert_eq!(forward_reload(&mut g, true), 0);
        assert_eq!(live_loads_at(&g, 11), 1);
        assert_eq!(live_loads_at(&g, 20), 0);
    }

    /// nio21 wave 19, shape 2: the successor's stack top is a join's value
    /// phi the call feeds (a guarded splice's miss call, a call prefix). The
    /// frame is accepted as a COPY with the phi replaced by the call; a phi
    /// the call does not feed, or the switch off, refuses.
    #[test]
    fn nio21w19_a_join_successor_is_copied_with_the_call_as_its_top() {
        let mut g = build_call(&CALL_ON_A_COLD_ARM, 23, b'I');
        let call = call_node(&g);
        let ctrl = g.node_opt(call).and_then(|c| c.input_opt(0)).expect("ctrl");
        let other = g.add(Op::Const(3), IrType::Int, vec![], None);
        let phi = g.add(Op::Phi, IrType::Int, vec![ctrl, other, call], Some(26));
        let si = g
            .safepoints
            .iter()
            .position(|s| s.bci == 26 && !s.is_splice_state())
            .expect("a frame at 26");
        let top = g.safepoints[si].stack.len() - 1;
        assert!(g.set_safepoint_slot(si, crate::ir::SafepointSlotKind::Stack, top, phi));
        let f = super::post_writer_frame(&g, call).expect("shape 2 is admitted");
        assert_eq!(f.succ, 26);
        let copy = f.copy.expect("a copy");
        assert_eq!(copy.bci, 26);
        assert_eq!(
            copy.stack.last(),
            Some(&call),
            "the phi is replaced by the call"
        );
        assert_eq!(copy.locals, g.safepoints[si].locals);
        let off = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_RELOAD_JOIN", Some("0"))],
            || super::post_writer_frame(&g, call).is_err(),
        );
        assert!(off, "=0 refuses shape 2");
        // A phi the call does not feed.
        let stranger = g.add(Op::Phi, IrType::Int, vec![ctrl, other, other], Some(26));
        assert!(g.set_safepoint_slot(si, crate::ir::SafepointSlotKind::Stack, top, stranger));
        assert!(super::post_writer_frame(&g, call).is_err());
    }

    /// nio21 wave 19, shape 2 end to end: the reload names the pushed copy.
    #[test]
    fn nio21w19_the_reload_names_the_join_copy() {
        let mut g = build_call(&CALL_ON_A_COLD_ARM, 23, b'I');
        let call = call_node(&g);
        // A well-formed two-input phi (the arms' join at 28), standing in for
        // the hit/miss join a guarded splice puts at the successor.
        let join = g
            .nodes
            .iter()
            .position(|n| n.op == Op::Merge && n.bytecode_pc == Some(28) && n.inputs.len() == 2)
            .expect("the arms' join") as crate::ir::NodeId;
        let other = g.add(Op::Const(3), IrType::Int, vec![], None);
        let phi = g.add(Op::Phi, IrType::Int, vec![join, other, call], Some(28));
        let si = g
            .safepoints
            .iter()
            .position(|s| s.bci == 26 && !s.is_splice_state())
            .expect("a frame at 26");
        let top = g.safepoints[si].stack.len() - 1;
        assert!(g.set_safepoint_slot(si, crate::ir::SafepointSlotKind::Stack, top, phi));
        assert!(forward_reload(&mut g, true) >= 1);
        let reload = g
            .nodes
            .iter()
            .find(|n| matches!(n.op, Op::Load(_)) && n.bytecode_pc == Some(26))
            .expect("a reload at 26");
        let named = reload.frame_snapshot.expect("the reload names its frame") as usize;
        assert_ne!(named, si, "not the builder's by-bci frame");
        assert_eq!(g.safepoints[named].stack.last(), Some(&call));
    }

    /// nio21 wave 19, shape 3: a call with no flat frame at its pc (a spliced
    /// call) takes its one post-call chain snapshot, re-marked as an ordinary
    /// chain; two such snapshots, none, or the switch off refuse.
    #[test]
    fn nio21w19_a_spliced_call_takes_its_post_call_chain() {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let call = g.add(
            Op::Call { info_ptr: 0 },
            IrType::Int,
            vec![ctrl, mem],
            Some(100),
        );
        let chain = crate::ir::SpliceScopeChain { scopes: Vec::new() };
        let mut post = crate::ir::SafepointSnapshot::new(103, vec![], vec![call], vec![]);
        post.splice_scope = Some(Box::new(chain));
        post.post_call_of = call;
        post.post_call_only = true;
        assert!(
            super::post_writer_frame(&g, call).is_err(),
            "no post-call chain yet"
        );
        g.push_safepoint(post.clone());
        let f = super::post_writer_frame(&g, call).expect("shape 3 is admitted");
        assert_eq!(f.succ, 103);
        let copy = f.copy.expect("a copy");
        assert!(copy.is_guard_splice_state(), "an ordinary chain snapshot");
        assert_eq!(copy.post_call_of, crate::ir::NO_NODE);
        assert_eq!(copy.stack, vec![call]);
        let off = cratonvm_types::flags::with_thread_overrides(
            &[(
                "CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_RELOAD_SPLICED",
                Some("0"),
            )],
            || super::post_writer_frame(&g, call).is_err(),
        );
        assert!(off, "=0 refuses shape 3");
        g.push_safepoint(post);
        assert!(
            super::post_writer_frame(&g, call).is_err(),
            "two post-call chains"
        );
    }

    /// `=0` restores the refusal of a loop holding a call.
    #[test]
    fn nio21w18_the_reload_switch_restores_the_refusal() {
        let mut g = build_call(&CALL_ON_A_COLD_ARM, 23, b'I');
        assert_eq!(forward_reload(&mut g, false), 0);
        assert_eq!(live_loads_at(&g, 11), 1);
        assert_eq!(live_loads_at(&g, 26), 0);
        let off = cratonvm_types::flags::with_thread_overrides(
            &[(RELOAD, Some("0"))],
            super::reload_after_writer_enabled,
        );
        assert!(!off);
    }

    /// A call with an exceptional continuation in this body (a call-site
    /// exception edge) has no reload frame: its throw path would leave past
    /// the reload with the call's memory. The loop is refused as before.
    #[test]
    fn nio21w18_a_call_with_an_exception_edge_is_not_reloaded_after() {
        let mut g = build_call(&CALL_ON_A_COLD_ARM, 23, b'I');
        let call = call_node(&g);
        let ctrl = g.node_opt(call).and_then(|c| c.input_opt(0)).expect("ctrl");
        g.add(
            Op::CallThrew { site: 0, index: 0 },
            IrType::Int,
            vec![ctrl, call],
            Some(23),
        );
        assert!(super::post_writer_frame(&g, call).is_err());
        assert_eq!(forward_reload(&mut g, true), 0);
        assert_eq!(live_loads_at(&g, 11), 1);
    }

    /// A frame at the successor that is not the call's (here: the result
    /// missing from the stack) refuses, and so does a non-call writer.
    #[test]
    fn nio21w18_the_successor_frame_must_be_the_calls() {
        let mut g = build_call(&CALL_ON_A_COLD_ARM, 23, b'I');
        let call = call_node(&g);
        let si = g
            .safepoints
            .iter()
            .position(|s| s.bci == 26 && !s.is_splice_state())
            .expect("a frame at 26");
        let top = g.safepoints[si].stack.len() - 1;
        let other = g.node_opt(call).and_then(|c| c.input_opt(0)).expect("ctrl");
        assert!(g.set_safepoint_slot(si, crate::ir::SafepointSlotKind::Stack, top, other));
        assert!(super::post_writer_frame(&g, call).is_err());
        let start = g.entry;
        assert!(super::post_writer_frame(&g, start).is_err());
    }

    // ── JIT round 19 wave 2 (lane osr2): FG16-5 cell promotion ──────────

    const PROMOTE: &str = "CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE";

    fn promote(graph: &mut Graph) -> usize {
        cratonvm_types::flags::with_thread_overrides(&[(PROMOTE, Some("1"))], || {
            super::promote_loop_cells(graph)
        })
    }

    fn increment_in_place() -> Graph {
        let mut g = build(
            &INCREMENT_IN_PLACE,
            &[IrType::Ref, IrType::Int],
            3,
            &[(9, 0), (14, 0), (24, 0)],
            &[(9, 50), (14, 50), (24, 50)],
        );
        assert_eq!(forward(&mut g), 1);
        super::super::scalar_cleanup(&mut g);
        g
    }

    fn live_stores(g: &Graph) -> Vec<crate::ir::NodeId> {
        (0..g.nodes.len())
            .filter(|&i| matches!(g.nodes[i].op, Op::Store(_)))
            .map(|i| i as crate::ir::NodeId)
            .collect()
    }

    /// The write-back on EXIT: `p.x = p.x + 1` in a counted loop loses its
    /// in-loop store; one store of the header's value runs on the exit edge,
    /// under `if (v_exit != v_entry)`, and the read after the loop reads the
    /// memory state past it. The header is recorded for the lowering's poll
    /// refusal, and the graph verifies.
    #[test]
    fn r19_osr2_a_promoted_cell_is_written_back_once_on_the_exit_edge() {
        let mut g = increment_in_place();
        assert_eq!(promote(&mut g), 1);
        assert_eq!(g.promoted_cell_header_bcis, vec![2]);
        let stores = live_stores(&g);
        let [st] = stores.as_slice() else {
            panic!("one store left, got {}", stores.len());
        };
        let store = &g.nodes[*st as usize];
        assert_eq!(
            store.bytecode_pc,
            Some(2),
            "the write-back, not the loop's store at 14"
        );
        assert_eq!(
            store.frame_snapshot.map(|si| g.safepoints[si as usize].bci),
            Some(2)
        );
        // Its control is the taken edge of `If(Cmp(Ne, v, init))`.
        let taken = &g.nodes[store.inputs[0] as usize];
        assert_eq!(taken.op, Op::Proj(0));
        let iff = &g.nodes[taken.inputs[0] as usize];
        assert_eq!(iff.op, Op::If);
        let cond = &g.nodes[iff.inputs[1] as usize];
        assert_eq!(cond.op, Op::Cmp(crate::ir::CmpOp::Ne));
        let init = &g.nodes[cond.inputs[1] as usize];
        assert!(
            matches!(init.op, Op::Load(_)) && init.bytecode_pc == Some(2),
            "compared with the pre-header read"
        );
        assert_eq!(
            store.inputs[4], cond.inputs[0],
            "it writes the value it compared"
        );
        // The read after the loop is ordered after the write-back.
        let after = g
            .nodes
            .iter()
            .find(|n| matches!(n.op, Op::Load(_)) && n.bytecode_pc == Some(24))
            .expect("the read after the loop stays");
        let mem = &g.nodes[after.inputs[1] as usize];
        assert!(
            mem.op == Op::Phi && mem.ty == IrType::Memory && mem.inputs.contains(st),
            "the read after the loop sees the write-back"
        );
        assert!(crate::ir_verify::verify_graph_at_phase(&g, "post-optimize").is_ok());
    }

    /// A store whose null check could fire (its base `q` is not read in the
    /// pre-header) is a deopt point inside the loop: nothing is promoted and
    /// the stores stay. So is a guard on a loop value; a null guard of a base
    /// the pre-header already dereferenced cannot fire and is admitted.
    #[test]
    fn r19_osr2_a_loop_that_can_deopt_keeps_its_stores() {
        let mut other = build(
            &READ_P_WRITE_Q,
            &[IrType::Ref, IrType::Ref, IrType::Int],
            5,
            &[(13, 0), (21, 0)],
            &[(13, 50), (21, 51)],
        );
        assert_eq!(forward(&mut other), 1);
        assert_eq!(promote(&mut other), 0);
        assert_eq!(live_stores(&other).len(), 1);
        assert!(other.promoted_cell_header_bcis.is_empty());

        // A guard on the loop's own value: a transfer the pending write would miss.
        let mut guarded = increment_in_place();
        let st = live_stores(&guarded)[0];
        let ctrl = guarded.nodes[st as usize].inputs[0];
        let value = guarded.nodes[st as usize].inputs[4];
        let zero = guarded.add(Op::Const(0), IrType::Int, vec![], None);
        let cond = guarded.add(
            Op::Cmp(crate::ir::CmpOp::Ne),
            IrType::Int,
            vec![value, zero],
            Some(14),
        );
        guarded.add(
            Op::Guard { bci: 14 },
            IrType::Void,
            vec![ctrl, cond],
            Some(14),
        );
        assert_eq!(promote(&mut guarded), 0, "a guard that can fire refuses");
        assert_eq!(live_stores(&guarded).len(), 1);
        assert!(guarded.promoted_cell_header_bcis.is_empty());

        // `p != null`, with `p` read in the pre-header: admitted.
        let mut proven = increment_in_place();
        let st = live_stores(&proven)[0];
        let ctrl = proven.nodes[st as usize].inputs[0];
        let base = proven.nodes[st as usize].inputs[2];
        let null = proven.add(Op::Const(0), IrType::Ref, vec![], None);
        let cond = proven.add(
            Op::Cmp(crate::ir::CmpOp::Ne),
            IrType::Int,
            vec![base, null],
            Some(14),
        );
        proven.add(
            Op::Guard { bci: 14 },
            IrType::Void,
            vec![ctrl, cond],
            Some(14),
        );
        assert_eq!(
            promote(&mut proven),
            1,
            "a null guard that cannot fire is admitted"
        );
        assert_eq!(proven.promoted_cell_header_bcis, vec![2]);
    }

    /// The write-back at a DEOPT point inside the loop is the back-edge poll's
    /// mode exit, the one transfer the graph does not show: the lowering
    /// refuses it at a promoted header, before any other test.
    #[test]
    fn r19_osr2_the_lowering_refuses_a_poll_exit_at_a_promoted_header() {
        let src = include_str!("../ir_lower.rs");
        let body = src
            .split("fn back_edge_mode_exit_state(")
            .nth(1)
            .expect("the mode-exit resolver is in ir_lower.rs")
            .split("\n    }\n")
            .next()
            .expect("it ends");
        let check = body
            .find("promoted_cell_header_bcis.contains(&header_bci)")
            .expect("the promoted-header refusal");
        let spliced = body
            .find("pc_is_in_a_spliced_body(header_bci)")
            .expect("the splice arm");
        assert!(check < spliced, "asked before any exit is built");
    }

    /// Promotion asks the base's USERS whether a branch of the loop tests its
    /// class (a class-word compare included), so it never rests on the
    /// forwarding's `class_tested_in`, which cannot see a pure test.
    #[test]
    fn r19_osr2_a_class_tested_base_is_seen_through_its_users() {
        let mut g = increment_in_place();
        let st = live_stores(&g)[0];
        let ctrl = g.nodes[st as usize].inputs[0];
        let base = g.nodes[st as usize].inputs[2];
        let (region, entry, backs) = super::loop_headers(&g)
            .into_iter()
            .next()
            .expect("the loop");
        let body = super::loop_body(&g, region, entry, &backs);
        assert!(!super::base_class_tested_by_loop(&g, base, &body));
        let word = g.add(Op::ClassWordOf, IrType::Int, vec![base], Some(14));
        let id = g.add(Op::Const(7), IrType::Int, vec![], None);
        let cmp = g.add(
            Op::Cmp(crate::ir::CmpOp::Eq),
            IrType::Int,
            vec![word, id],
            Some(14),
        );
        let guard = g.add(
            Op::Guard { bci: 14 },
            IrType::Void,
            vec![ctrl, cmp],
            Some(14),
        );
        let body = super::loop_body(&g, region, entry, &backs);
        assert!(body.contains(&guard));
        assert!(!body.contains(&word), "the pure test is not in the body");
        assert!(super::base_class_tested_by_loop(&g, base, &body));
        assert_eq!(promote(&mut g), 0);
    }

    /// JIT round 20 (lane loopopt): the forward's own `class_tested_in` sees
    /// a class-word test through its `Cmp` too (the class-word chain and
    /// single-test forms of a guarded splice).
    #[test]
    fn jr20_the_forward_sees_a_class_word_test_through_its_cmp() {
        let mut g = increment_in_place();
        let st = live_stores(&g)[0];
        let ctrl = g.nodes[st as usize].inputs[0];
        let base = g.nodes[st as usize].inputs[2];
        let (region, entry, backs) = super::loop_headers(&g)
            .into_iter()
            .next()
            .expect("the loop");
        let nodes = |g: &Graph| -> Vec<crate::ir::NodeId> {
            super::loop_body(g, region, entry, &backs)
                .into_iter()
                .collect()
        };
        assert!(!super::class_tested_in(&g, base, &nodes(&g)));
        let word = g.add(Op::ClassWordOf, IrType::Int, vec![base], Some(14));
        let id = g.add(Op::Const(7), IrType::Int, vec![], None);
        let cmp = g.add(
            Op::Cmp(crate::ir::CmpOp::Eq),
            IrType::Int,
            vec![word, id],
            Some(14),
        );
        let _guard = g.add(
            Op::Guard { bci: 14 },
            IrType::Void,
            vec![ctrl, cmp],
            Some(14),
        );
        assert!(super::class_tested_in(&g, base, &nodes(&g)));
    }

    // ── JIT round 19 wave 4 (lane osr4): OSR2-1 poll write-backs ─────────

    const POLL_WRITEBACK: &str = "CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_POLL_WRITEBACK";

    extern "C" fn no_verdict(_flag_addr: i64, _body_id: i64) -> i64 {
        0
    }

    extern "C" fn fake_putfield(_obj: i64, _field_index: i64, _val: i64) {}

    extern "C" fn fake_getfield(_obj: i64, _field_index: i64) -> i64 {
        0
    }

    /// Lower `g` (`INCREMENT_IN_PLACE`, promoted) as a method-entry body with
    /// back-edge mode exits, the write-back switch as given.
    fn lower_promoted(g: &Graph, writeback: bool) -> Option<crate::CompiledMethod> {
        let schedule = crate::ir_schedule::schedule(g);
        // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; the test wires only the poll and the field helpers.
        let mut helpers: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        // Leaked rather than a `static` (`process_global_statics_ratchet`).
        let flag: &'static u8 = Box::leak(Box::new(1u8));
        helpers.safepoint_flag_addr = flag as *const u8 as usize; // Cast: baked flag address
        helpers.safepoint_slow_path = no_verdict as *const () as usize; // Cast: helper address
        helpers.putfield_int = fake_putfield as *const () as usize; // Cast: helper address
        helpers.putfield_long = fake_putfield as *const () as usize; // Cast: helper address
        helpers.getfield = fake_getfield as *const () as usize; // Cast: helper address
        let code = &INCREMENT_IN_PLACE;
        cratonvm_types::flags::with_thread_overrides(
            &[(POLL_WRITEBACK, Some(if writeback { "1" } else { "0" }))],
            || {
                crate::ir_lower::lower_inner_with_mode_exit_polls(
                    &crate::DirectHelperTable::EMPTY,
                    g,
                    &schedule,
                    2,
                    3,
                    &helpers,
                    &HashMap::new(),
                    &[],
                    None,
                    &HashMap::new(),
                    &HashMap::new(),
                    &HashMap::new(),
                    &crate::ir::IrInlineFrameSites::default(),
                    &HashMap::new(),
                    &std::collections::HashSet::new(),
                    None,
                    &[],
                    Some(&[]),
                    crate::ir_lower::IrPollModeExits::MethodEntry,
                    Some(crate::ir_lower::PollExitBytecode {
                        code,
                        code_len: code.len(),
                        num_params: 2,
                        num_locals: 3,
                        handlers: &[],
                    }),
                    None,
                )
            },
        )
    }

    fn poll_exit_bcis(cm: &crate::CompiledMethod) -> Vec<u32> {
        cm._deopt_point_boxes
            .iter()
            .filter(|p| p.reason == crate::deopt::DeoptReason::OsrExit)
            .map(|p| p.bci)
            .collect()
    }

    fn helper_calls(cm: &crate::CompiledMethod) -> usize {
        // SAFETY: `entry_ptr` / `code_len` describe the method's own finalized code.
        let code = unsafe { std::slice::from_raw_parts(cm.entry_ptr(), cm.code_len()) };
        // Cast: the stand-in helper's address, as `MOV RAX, imm64` bakes it.
        let addr = (fake_putfield as *const () as u64).to_le_bytes();
        code.windows(8).filter(|w| *w == addr).count()
    }

    /// OSR2-1: a deopt at the back-edge poll of a promoted loop. Without the
    /// write-back switch the poll's mode exit is refused (wave 2: the
    /// interpreter would resume over stale memory); with it the exit is given
    /// at the header, and its pad calls the field helper once more than the
    /// body does elsewhere -- the write-back that makes the transfer observe
    /// every store the loop made. (The pad's code itself is run by
    /// `ir_lower::r19_osr4_cell_writeback_tests`.)
    #[test]
    fn r19_osr4_a_promoted_header_poll_exit_writes_the_cells_back() {
        if !crate::deopt_real_enabled() || !crate::x64::jit_safepoint_polls_enabled() {
            return;
        }
        let mut g = increment_in_place();
        assert_eq!(promote(&mut g), 1);
        let [(region, store)] = g.promoted_cell_writebacks.as_slice() else {
            panic!("one write-back recorded: {:?}", g.promoted_cell_writebacks);
        };
        assert_eq!(g.nodes[*region as usize].bytecode_pc, Some(2));
        assert!(matches!(g.nodes[*store as usize].op, Op::Store(_)));
        crate::ir_lower::clear_lower_bail();
        let off = lower_promoted(&g, false).unwrap_or_else(|| {
            panic!("lowers (refused: {:?})", crate::ir_lower::last_lower_bail())
        });
        let on = lower_promoted(&g, true).unwrap_or_else(|| {
            panic!("lowers (refused: {:?})", crate::ir_lower::last_lower_bail())
        });
        assert!(
            poll_exit_bcis(&off).is_empty(),
            "switch off: the promoted header's poll exit is refused"
        );
        assert_eq!(
            poll_exit_bcis(&on),
            vec![2],
            "switch on: one exit, at the header"
        );
        assert_eq!(
            helper_calls(&on),
            helper_calls(&off) + 1,
            "the exit's pad writes the cell back through the helper"
        );
    }

    /// A header the promotion recorded by bci but whose node has no recorded
    /// write-back (a copy made after the promotion) keeps the refusal even
    /// under the switch: the pad would not know its cells.
    #[test]
    fn r19_osr4_an_unrecorded_promoted_header_keeps_the_refusal() {
        if !crate::deopt_real_enabled() || !crate::x64::jit_safepoint_polls_enabled() {
            return;
        }
        let mut g = increment_in_place();
        assert_eq!(promote(&mut g), 1);
        g.promoted_cell_writebacks.clear();
        let on = lower_promoted(&g, true).expect("lowers");
        assert!(poll_exit_bcis(&on).is_empty());
    }

    /// The promotion never takes a volatile cell, nor a loop that synchronizes
    /// (a monitor is an ordering write and can deopt); the JMM lets a plain
    /// field's stores be sunk only where no synchronization intervenes.
    #[test]
    fn r19_osr4_volatile_and_synchronized_loops_are_never_promoted() {
        // A volatile store: the census refuses the loop.
        let mut vol = increment_in_place();
        let st = live_stores(&vol)[0];
        vol.nodes[st as usize].volatile_access = true;
        assert!(vol.nodes[st as usize].is_volatile_access());
        assert_eq!(promote(&mut vol), 0);
        assert!(vol.promoted_cell_writebacks.is_empty());
        // A monitor enter/exit pair inside the loop body.
        let mut sync = increment_in_place();
        let st = live_stores(&sync)[0];
        let ctrl = sync.nodes[st as usize].inputs[0];
        let mem = sync.nodes[st as usize].inputs[1];
        let base = sync.nodes[st as usize].inputs[2];
        let enter = sync.add(
            Op::MonitorEnter,
            IrType::Memory,
            vec![ctrl, mem, base],
            Some(14),
        );
        sync.set_input(st, 1, enter);
        assert_eq!(promote(&mut sync), 0, "a monitor in the loop refuses");
        assert!(sync.promoted_cell_writebacks.is_empty());
    }

    /// The switch is opt-in, and a graph that is not promoted records nothing.
    #[test]
    fn r19_osr2_the_promote_switch_is_opt_in() {
        assert!(!cratonvm_types::flags::with_thread_overrides(
            &[(PROMOTE, Some("0"))],
            super::promote_enabled
        ));
        let g = increment_in_place();
        assert!(g.promoted_cell_header_bcis.is_empty());
    }

    /// The kill switch.
    #[test]
    fn the_switch_turns_the_pass_off() {
        let off = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_LOOP_FIELD_FORWARD", Some("0"))],
            super::loop_field_forward_enabled,
        );
        assert!(!off);
    }

    // ── JIT round 19 wave 5 (lane osr5): OSR2-3 pending stores at deopts ──

    const PENDING: &str = "CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PENDING_DEOPT";

    fn promote_pending(graph: &mut Graph) -> usize {
        cratonvm_types::flags::with_thread_overrides(
            &[(PROMOTE, Some("1")), (PENDING, Some("1"))],
            || super::promote_loop_cells(graph),
        )
    }

    /// `static int f(P p, int[] a, int n) { for (int i = 0; i < n; i++)
    /// p.x = p.x + a[i]; return p.x; }` -- `x` field 0.
    const ACCUMULATE_ARRAY: [u8; 30] = [
        0x03, 0x3e, // 0: i = 0
        0x1d, 0x1c, 0xa2, 0x00, 0x15, // 2: header: if (i >= n) goto 25
        0x2a, 0x2a, 0xb4, 0x00, 0x01, // 7: p, p.x (getfield at 9)
        0x2b, 0x1d, 0x2e, 0x60, // 12: a[i] (iaload at 14); iadd
        0xb5, 0x00, 0x01, // 16: putfield p.x
        0x84, 0x03, 0x01, 0xa7, 0xff, 0xec, // 19: i++; 22: goto 2
        0x2a, 0xb4, 0x00, 0x01, 0xac, // 25: return p.x (getfield at 26)
    ];

    /// `static int f(P p, int[] a, int n) { for (int i = 0; i < n; i++) if
    /// (a[i] > 0) p.x = p.x + 1; return p.x; }` -- a store under a branch.
    const CONDITIONAL_STORE: [u8; 34] = [
        0x03, 0x3e, // 0: i = 0
        0x1d, 0x1c, 0xa2, 0x00, 0x19, // 2: header: if (i >= n) goto 29
        0x2b, 0x1d, 0x2e, // 7: a[i] (iaload at 9)
        0x9e, 0x00, 0x0d, // 10: ifle 23
        0x2a, 0x2a, 0xb4, 0x00, 0x01, 0x04, 0x60, // 13: p.x + 1 (getfield at 15)
        0xb5, 0x00, 0x01, // 20: putfield p.x
        0x84, 0x03, 0x01, 0xa7, 0xff, 0xe8, // 23: i++; 26: goto 2
        0x2a, 0xb4, 0x00, 0x01, 0xac, // 29: return p.x (getfield at 30)
    ];

    fn accumulate_array() -> Graph {
        let mut g = build(
            &ACCUMULATE_ARRAY,
            &[IrType::Ref, IrType::Ref, IrType::Int],
            4,
            &[(9, 0), (16, 0), (26, 0)],
            &[(9, 50), (16, 50), (26, 50)],
        );
        assert_eq!(forward(&mut g), 1);
        super::super::scalar_cleanup(&mut g);
        g
    }

    /// `increment_in_place` with `Guard(p.x' != 0)` right after the store,
    /// on the store's own control: a guard that CAN fire, positioned after
    /// the cell's write in the iteration.
    fn guarded_after_store() -> (Graph, crate::ir::NodeId) {
        let mut g = increment_in_place();
        let st = live_stores(&g)[0];
        let ctrl = g.nodes[st as usize].inputs[0];
        let value = g.nodes[st as usize].inputs[4];
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let cond = g.add(
            Op::Cmp(crate::ir::CmpOp::Ne),
            IrType::Int,
            vec![value, zero],
            Some(14),
        );
        let guard = g.add(
            Op::Guard { bci: 14 },
            IrType::Void,
            vec![ctrl, cond],
            Some(14),
        );
        (g, guard)
    }

    /// The plan the lowerer follows, over the production schedule.
    fn plan(g: &Graph) -> crate::ir_lower::PromotedPending {
        let schedule = crate::ir_schedule::schedule(g);
        crate::ir_lower::plan_promoted_pending(g, &schedule)
            .expect("a sound plan")
            .expect("silent stores recorded")
    }

    /// OSR2-3: a guard that can fire no longer refuses the promotion under
    /// the switch; the in-loop store is KEPT (silent) and recorded against
    /// its cell's exit write-back, which is built exactly as before. Without
    /// the switch the same graph is refused (wave 2's rule).
    #[test]
    fn r19_osr5_a_loop_with_a_guard_keeps_its_store_as_a_silent_one() {
        let (mut off, _) = guarded_after_store();
        assert_eq!(
            promote(&mut off),
            0,
            "switch off: a guard that can fire refuses"
        );
        assert!(off.promoted_sunk_stores.is_empty());

        let (mut g, _) = guarded_after_store();
        let in_loop = live_stores(&g)[0];
        assert_eq!(promote_pending(&mut g), 1);
        assert_eq!(g.promoted_cell_header_bcis, vec![2]);
        let [(_, exit)] = g.promoted_cell_writebacks.as_slice() else {
            panic!("one cell: {:?}", g.promoted_cell_writebacks);
        };
        assert_eq!(g.promoted_sunk_stores, vec![(*exit, in_loop)]);
        assert!(
            matches!(g.nodes[in_loop as usize].op, Op::Store(_)),
            "the in-loop store is kept, not killed"
        );
        assert_eq!(live_stores(&g).len(), 2, "the silent store and the exit's");
        assert!(crate::ir_verify::verify_graph_at_phase(&g, "post-optimize").is_ok());
    }

    /// The soundness core: the value a deopt pad writes back is the cell's
    /// value AT that point of the iteration. A guard after the store sees the
    /// stored value (the interpreter resumes after the write and must observe
    /// it); a guard in the header block, before any store of the iteration,
    /// sees the header value -- the value the previous iterations left,
    /// which on the first trip IS the value read on entry, so the pad's
    /// `value != init` test writes nothing (never writing back a value this
    /// thread only read).
    #[test]
    fn r19_osr5_a_guard_that_fires_mid_loop_writes_back_every_store_before_it() {
        let (mut g, guard) = guarded_after_store();
        let in_loop = live_stores(&g)[0];
        let stored = g.nodes[in_loop as usize].inputs[4];
        assert_eq!(promote_pending(&mut g), 1);
        let (region, exit) = g.promoted_cell_writebacks[0];
        let header_value = g.nodes[exit as usize].inputs[4];
        // A second guard, in the header block (before the iteration's store).
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let cond = g.add(
            Op::Cmp(crate::ir::CmpOp::Ne),
            IrType::Int,
            vec![header_value, zero],
            Some(2),
        );
        let early = g.add(
            Op::Guard { bci: 2 },
            IrType::Void,
            vec![region, cond],
            Some(2),
        );
        let p = plan(&g);
        assert!(p.silent.contains(&in_loop));
        assert_eq!(
            p.at_node.get(&guard),
            Some(&vec![(exit, Some(stored))]),
            "after the store: the stored value"
        );
        assert_eq!(
            p.at_node.get(&early),
            Some(&vec![(exit, Some(header_value))]),
            "before it: the header value"
        );
        assert!(!p.body_blocks.is_empty());
        // The exit write-back's compare is against the entry read.
        let shape_init = {
            let iff = g.nodes[g.nodes[exit as usize].inputs[0] as usize].inputs[0];
            let cmp = g.nodes[iff as usize].inputs[1];
            g.nodes[cmp as usize].inputs[1]
        };
        assert!(matches!(g.nodes[shape_init as usize].op, Op::Load(_)));
    }

    /// An exception thrown mid-loop: `p.x += a[i]` with `a[i]` out of
    /// bounds. The array read's null / bounds deopt happens BEFORE the
    /// iteration's store, so its pad writes the header value (every earlier
    /// iteration's sum) and the interpreter re-executes the `iaload`, throws,
    /// and leaves `p.x` exactly as HotSpot does. Lowered for real: the read's
    /// pads carry the write-back helper call, the silent store emits none.
    #[test]
    fn r19_osr5_an_exception_mid_loop_sees_every_completed_iteration() {
        let mut off = accumulate_array();
        assert_eq!(promote(&mut off), 0, "switch off: an array read refuses");

        let mut g = accumulate_array();
        assert_eq!(promote_pending(&mut g), 1);
        let (_, exit) = g.promoted_cell_writebacks[0];
        let header_value = g.nodes[exit as usize].inputs[4];
        let load = (0..g.nodes.len())
            .find(|&i| matches!(g.nodes[i].op, Op::ArrayLoad(_)))
            .expect("the a[i] read") as crate::ir::NodeId;
        let p = plan(&g);
        assert_eq!(
            p.at_node.get(&load),
            Some(&vec![(exit, Some(header_value))]),
            "the read precedes the store: the pad writes the header value"
        );
    }

    /// A store under a branch makes a memory phi in the body: with a deopt
    /// point in the loop the lowerer could not name ONE value per cell at the
    /// join, so the promotion refuses (fail closed).
    #[test]
    fn r19_osr5_a_store_under_a_branch_with_a_deopt_point_refuses() {
        let mut g = build(
            &CONDITIONAL_STORE,
            &[IrType::Ref, IrType::Ref, IrType::Int],
            4,
            &[(15, 0), (20, 0), (30, 0)],
            &[(15, 50), (20, 50), (30, 50)],
        );
        forward(&mut g);
        super::super::scalar_cleanup(&mut g);
        assert_eq!(promote_pending(&mut g), 0);
        assert!(g.promoted_sunk_stores.is_empty());
        assert!(g.promoted_cell_writebacks.is_empty());
    }

    /// Volatile and synchronized loops stay unpromoted under the switch too.
    #[test]
    fn r19_osr5_volatile_and_synchronized_loops_are_still_never_promoted() {
        let (mut vol, _) = guarded_after_store();
        let st = live_stores(&vol)[0];
        vol.nodes[st as usize].volatile_access = true;
        assert_eq!(promote_pending(&mut vol), 0);
        assert!(vol.promoted_sunk_stores.is_empty());
        let (mut sync, _) = guarded_after_store();
        let st = live_stores(&sync)[0];
        let ctrl = sync.nodes[st as usize].inputs[0];
        let mem = sync.nodes[st as usize].inputs[1];
        let base = sync.nodes[st as usize].inputs[2];
        let enter = sync.add(
            Op::MonitorEnter,
            IrType::Memory,
            vec![ctrl, mem, base],
            Some(14),
        );
        sync.set_input(st, 1, enter);
        assert_eq!(
            promote_pending(&mut sync),
            0,
            "a monitor in the loop refuses"
        );
        assert!(sync.promoted_sunk_stores.is_empty());
    }

    /// A plan whose silent store was deleted by a later pass is refused (the
    /// lowerer then refuses the compile rather than lose the store).
    #[test]
    fn r19_osr5_a_deleted_silent_store_refutes_the_plan() {
        let (mut g, _) = guarded_after_store();
        assert_eq!(promote_pending(&mut g), 1);
        let (_, st) = g.promoted_sunk_stores[0];
        g.kill(st);
        let schedule = crate::ir_schedule::schedule(&g);
        assert!(crate::ir_lower::plan_promoted_pending(&g, &schedule).is_err());
    }

    /// Lowered: the guard's deopt pad calls the field helper (the
    /// write-back), the silent store calls nothing. Against the same loop
    /// without the guard (wave 2's deletion), exactly one helper call more:
    /// the pad's.
    #[test]
    fn r19_osr5_the_guard_pad_writes_back_and_the_silent_store_emits_nothing() {
        if !crate::deopt_real_enabled() {
            return;
        }
        let mut plain = increment_in_place();
        assert_eq!(promote(&mut plain), 1);
        let (mut g, _) = guarded_after_store();
        assert_eq!(promote_pending(&mut g), 1);
        crate::ir_lower::clear_lower_bail();
        let base = lower_promoted(&plain, false).unwrap_or_else(|| {
            panic!("lowers (refused: {:?})", crate::ir_lower::last_lower_bail())
        });
        let pending = cratonvm_types::flags::with_thread_overrides(&[(PENDING, Some("1"))], || {
            lower_promoted(&g, false)
        })
        .unwrap_or_else(|| panic!("lowers (refused: {:?})", crate::ir_lower::last_lower_bail()));
        assert_eq!(
            helper_calls(&pending),
            helper_calls(&base) + 1,
            "one write-back, in the guard's pad"
        );
    }

    // ── JIT round 19 wave 6 (lane osr6): OSR5-2 predicted pad refusals ──

    const PREDICT: &str = "CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PREDICT";
    const PAD_XMM_SAVE: &str = "CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PAD_XMM_SAVE";

    /// The guarded loop's admitted deopt points, as the census collects them.
    fn deopt_points(g: &Graph) -> Vec<crate::ir::NodeId> {
        (0..g.nodes.len())
            .filter(|&i| matches!(g.nodes[i].op, Op::Guard { .. }))
            .map(|i| i as crate::ir::NodeId)
            .collect()
    }

    /// OSR5-2: a deopt point inside a spliced body (its pc at or past the
    /// compiling method's own code) gets no write-back pad, which refused the
    /// whole compile; predicted, it refuses the promotion. The guarded loop's
    /// guard sits at bci 14: a splice map whose method is 10 bytes long puts
    /// it in a splice, one 28 bytes long does not.
    #[test]
    fn r19_osr6_a_deopt_point_in_a_splice_is_predicted() {
        let (g, guard) = guarded_after_store();
        let points = deopt_points(&g);
        assert!(points.contains(&guard));
        let inside = crate::ir::IrSpliceResumeMap {
            code_len: 10,
            bodies: vec![(10, 20, 3)],
        };
        let outside = crate::ir::IrSpliceResumeMap {
            code_len: 28,
            bodies: Vec::new(),
        };
        let refused = super::predicted_pad_refusal_in(&g, &points, Some(&inside));
        assert_eq!(
            refused,
            Some((guard, "a deopt point inside a splice (predicted)"))
        );
        assert_eq!(
            super::predicted_pad_refusal_in(&g, &points, Some(&outside)),
            None
        );
        assert_eq!(super::predicted_pad_refusal_in(&g, &points, None), None);
    }

    /// OSR5-2: a frame at the deopt point's bci naming a `double` may keep it
    /// in an XMM register the pad's helper call clobbers (`xmm=true`, a
    /// whole-compile refusal); predicted, unless OSR5-4's pads save the XMM
    /// homes.
    #[test]
    fn r19_osr6_an_fp_frame_is_predicted_unless_the_pads_save_xmm() {
        let (mut g, guard) = guarded_after_store();
        let d = g.add(Op::ConstF(1.5f64.to_bits()), IrType::Double, vec![], None);
        let mut sp = g.safepoints[0].clone();
        sp.bci = 14;
        sp.locals.push(d);
        g.safepoints.push(sp);
        let points = deopt_points(&g);
        let off =
            cratonvm_types::flags::with_thread_overrides(&[(PAD_XMM_SAVE, Some("0"))], || {
                super::predicted_pad_refusal_in(&g, &points, None)
            });
        assert_eq!(
            off,
            Some((guard, "a deopt frame naming a float or double (predicted)"))
        );
        let on = cratonvm_types::flags::with_thread_overrides(&[(PAD_XMM_SAVE, Some("1"))], || {
            super::predicted_pad_refusal_in(&g, &points, None)
        });
        assert_eq!(on, None);
        // Under the predict switch the promotion itself refuses that loop.
        let promoted = cratonvm_types::flags::with_thread_overrides(
            &[
                (PROMOTE, Some("1")),
                (PENDING, Some("1")),
                (PREDICT, Some("1")),
                (PAD_XMM_SAVE, Some("0")),
            ],
            || super::promote_loop_cells(&mut g),
        );
        assert_eq!(promoted, 0);
        assert!(g.promoted_sunk_stores.is_empty());
    }

    /// OSR5-2: the predict switch leaves a loop it cannot fault alone (the
    /// guarded loop, no splice, no FP frame), and the IR door's retry
    /// (`Graph::promote_pending_withheld`) promotes no loop holding a deopt
    /// point -- wave 2's rule -- while the plain loop still promotes.
    #[test]
    fn r19_osr6_the_retry_withholds_only_deopt_pad_promotions() {
        let (mut g, _) = guarded_after_store();
        let n = cratonvm_types::flags::with_thread_overrides(
            &[
                (PROMOTE, Some("1")),
                (PENDING, Some("1")),
                (PREDICT, Some("1")),
                (PAD_XMM_SAVE, Some("0")),
            ],
            || super::promote_loop_cells(&mut g),
        );
        assert_eq!(n, 1, "nothing predicted: promoted as before");

        let (mut retry, _) = guarded_after_store();
        retry.promote_pending_withheld = true;
        assert_eq!(promote_pending(&mut retry), 0);
        assert!(retry.promoted_sunk_stores.is_empty());
        assert!(retry.promoted_cell_writebacks.is_empty());

        let mut plain = increment_in_place();
        plain.promote_pending_withheld = true;
        assert_eq!(
            promote_pending(&mut plain),
            1,
            "no deopt point: still promoted"
        );
    }

    /// The retry reads exactly the promotion's lowering refusals, and both
    /// new switches are opt-in.
    #[test]
    fn r19_osr6_the_switches_are_opt_in_and_the_retry_reads_its_refusals() {
        assert!(crate::ir_lower::is_promoted_cells_lower_bail(
            "promoted field cells: a deopt without a write-back"
        ));
        assert!(crate::ir_lower::is_promoted_cells_lower_bail(
            "promoted field cells: pending-store plan"
        ));
        assert!(!crate::ir_lower::is_promoted_cells_lower_bail(
            "synchronized splice window"
        ));
        cratonvm_types::flags::with_thread_overrides(
            &[(PREDICT, None), (PAD_XMM_SAVE, None)],
            || {
                assert!(!crate::ir_lower::ir_loop_field_promote_predict_enabled());
                assert!(!crate::ir_lower::ir_loop_field_promote_pad_xmm_save_enabled());
            },
        );
    }

    // ── JIT round 19 wave 5 (lane osr5): OSR2-4 exits from inside the body ──

    const BODY_EXITS: &str = "CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_BODY_EXITS";

    /// `static int f(P p, int n, int at) { int i = 0; for (; i < n; i++) {
    /// p.x += 7; if (i == at) break; } return p.x; }` -- `x` field 0.
    const BREAK_AFTER_STORE: [u8; 34] = [
        0x03, 0x3e, // 0: i = 0
        0x1d, 0x1b, 0xa2, 0x00, 0x19, // 2: header: if (i >= n) goto 29
        0x2a, 0x2a, 0xb4, 0x00, 0x01, // 7: p, p.x (getfield at 9)
        0x10, 0x07, 0x60, // 12: + 7
        0xb5, 0x00, 0x01, // 15: putfield p.x
        0x1d, 0x1c, 0x9f, 0x00, 0x09, // 18: if (i == at) goto 29 (break)
        0x84, 0x03, 0x01, 0xa7, 0xff, 0xe8, // 23: i++; 26: goto 2
        0x2a, 0xb4, 0x00, 0x01, 0xac, // 29: return p.x (getfield at 30)
    ];

    fn break_after_store() -> Graph {
        let mut g = build(
            &BREAK_AFTER_STORE,
            &[IrType::Ref, IrType::Int, IrType::Int],
            4,
            &[(9, 0), (15, 0), (30, 0)],
            &[(9, 50), (15, 50), (30, 50)],
        );
        assert_eq!(forward(&mut g), 1);
        super::super::scalar_cleanup(&mut g);
        g
    }

    fn promote_body_exits(graph: &mut Graph) -> usize {
        cratonvm_types::flags::with_thread_overrides(
            &[(PROMOTE, Some("1")), (BODY_EXITS, Some("1"))],
            || super::promote_loop_cells(graph),
        )
    }

    /// OSR2-4: a `break` after the store. Without the switch the loop has two
    /// exits and is refused (wave 2). With it, each exit gets its own
    /// conditional write-back: the header exit writes the header value (the
    /// cell's value when the test fails), the `break` writes the value the
    /// iteration just stored -- so the read after the join sees the right
    /// cell on both paths. Only the header exit's store is recorded for the
    /// lowerer (the poll / deopt write-backs read a cell from it).
    #[test]
    fn r19_osr5_a_break_exit_gets_its_own_write_back() {
        let mut off = break_after_store();
        assert_eq!(promote(&mut off), 0, "switch off: two exits refuse");

        let mut g = break_after_store();
        let in_loop = live_stores(&g)[0];
        let stored = g.nodes[in_loop as usize].inputs[4];
        assert_eq!(promote_body_exits(&mut g), 1);
        let stores = live_stores(&g);
        assert_eq!(
            stores.len(),
            2,
            "two write-backs, the in-loop store gone: {stores:?}"
        );
        assert!(!stores.contains(&in_loop));
        let [(_, recorded)] = g.promoted_cell_writebacks.as_slice() else {
            panic!("one recorded cell: {:?}", g.promoted_cell_writebacks);
        };
        let header_value = g.nodes[*recorded as usize].inputs[4];
        assert!(
            matches!(g.nodes[header_value as usize].op, Op::Phi),
            "the header exit writes the header value"
        );
        let other = stores
            .iter()
            .copied()
            .find(|s| s != recorded)
            .expect("the break's");
        assert_eq!(
            g.nodes[other as usize].inputs[4], stored,
            "the break writes the value the iteration stored"
        );
        assert!(crate::ir_verify::verify_graph_at_phase(&g, "post-optimize").is_ok());
    }

    /// The interior exit's write-back is conditional, exactly as the header
    /// exit's: `If(Cmp(Ne, v, init))` over the entry read.
    #[test]
    fn r19_osr5_the_break_write_back_compares_against_the_entry_read() {
        let mut g = break_after_store();
        assert_eq!(promote_body_exits(&mut g), 1);
        let (_, recorded) = g.promoted_cell_writebacks[0];
        for st in live_stores(&g) {
            let taken = g.nodes[st as usize].inputs[0];
            let iff = g.nodes[taken as usize].inputs[0];
            assert_eq!(g.nodes[iff as usize].op, Op::If);
            let cmp = g.nodes[iff as usize].inputs[1];
            assert_eq!(g.nodes[cmp as usize].op, Op::Cmp(crate::ir::CmpOp::Ne));
            let init = g.nodes[cmp as usize].inputs[1];
            assert!(matches!(g.nodes[init as usize].op, Op::Load(_)));
            let rec_cmp = {
                let t = g.nodes[recorded as usize].inputs[0];
                let i = g.nodes[t as usize].inputs[0];
                g.nodes[i as usize].inputs[1]
            };
            assert_eq!(init, g.nodes[rec_cmp as usize].inputs[1], "one entry read");
        }
    }

    // ── JIT round 19 wave 7 (lane osr7): OSR6-1, OSR5-1, OSR5-3, OSR6-2 ──

    const RESIDENT: &str = "CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PENDING_RESIDENT";
    const CHAIN: &str = "CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PENDING_CHAIN";
    const BRANCH: &str = "CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PENDING_BRANCH";

    fn conditional_store() -> Graph {
        let mut g = build(
            &CONDITIONAL_STORE,
            &[IrType::Ref, IrType::Ref, IrType::Int],
            4,
            &[(15, 0), (20, 0), (30, 0)],
            &[(15, 50), (20, 50), (30, 50)],
        );
        forward(&mut g);
        super::super::scalar_cleanup(&mut g);
        g
    }

    /// OSR6-1: under the resident switch the per-iteration values a pad may
    /// write back (the silent store's value, the cell's header value) leave
    /// the deopt-named set, so their homes may drop; the entry read and the
    /// base (written once, before the loop) stay. The slot pins are
    /// unchanged either way: a home that IS written stays readable.
    #[test]
    fn r19_osr7_the_resident_switch_names_only_the_entry_read_and_the_base() {
        let mut g = accumulate_array();
        assert_eq!(promote_pending(&mut g), 1);
        let schedule = crate::ir_schedule::schedule(&g);
        let (exit, st) = g.promoted_sunk_stores[0];
        let stored = g.nodes[st as usize].inputs[4];
        let header_value = g.nodes[exit as usize].inputs[4];
        let base = g.nodes[exit as usize].inputs[2];
        let pins = crate::ir_lower::promoted_pending_slot_pins(&g, &schedule);
        let named_off =
            cratonvm_types::flags::with_thread_overrides(&[(RESIDENT, Some("0"))], || {
                crate::ir_lower::promoted_pending_deopt_named(&g, &schedule)
            });
        let named_on =
            cratonvm_types::flags::with_thread_overrides(&[(RESIDENT, Some("1"))], || {
                crate::ir_lower::promoted_pending_deopt_named(&g, &schedule)
            });
        assert_eq!(named_off, pins, "switch off: the wave-5 set");
        for v in [stored, header_value, base] {
            assert!(pins.contains(&v), "n{v} keeps its slot pin");
        }
        assert!(named_on.contains(&base));
        assert!(
            !named_on.contains(&stored),
            "the stored value is not deopt-named"
        );
        assert!(
            !named_on.contains(&header_value),
            "the header value phi is not deopt-named"
        );
        assert_eq!(named_on.len(), 2, "entry read and base: {named_on:?}");
    }

    /// OSR6-1, lowered: the guarded loop still lowers under the resident
    /// switch with exactly the wave-5 pad (one write-back helper call more
    /// than the deletion), whatever each operand is described by.
    #[test]
    fn r19_osr7_the_guard_pad_still_writes_back_under_the_resident_switch() {
        if !crate::deopt_real_enabled() {
            return;
        }
        let mut plain = increment_in_place();
        assert_eq!(promote(&mut plain), 1);
        let (mut g, _) = guarded_after_store();
        assert_eq!(promote_pending(&mut g), 1);
        crate::ir_lower::clear_lower_bail();
        let base = lower_promoted(&plain, false).unwrap_or_else(|| {
            panic!("lowers (refused: {:?})", crate::ir_lower::last_lower_bail())
        });
        let resident = cratonvm_types::flags::with_thread_overrides(
            &[(PENDING, Some("1")), (RESIDENT, Some("1"))],
            || lower_promoted(&g, false),
        )
        .unwrap_or_else(|| panic!("lowers (refused: {:?})", crate::ir_lower::last_lower_bail()));
        assert_eq!(
            helper_calls(&resident),
            helper_calls(&base) + 1,
            "one write-back, in the guard's pad"
        );
    }

    /// OSR5-3: a store under a branch with a deopt point (the `a[i]` read)
    /// is promoted under the branch switch, its store kept silent; the plan
    /// names the join's value phi -- the walk's, whose inputs are the stored
    /// value and the header value -- at a deopt point after the join, and
    /// the cell is unknown there with the switch off.
    #[test]
    fn r19_osr7_a_store_under_a_branch_reads_the_joins_value_phi() {
        let mut off = conditional_store();
        assert_eq!(promote_pending(&mut off), 0, "switch off: refused (wave 5)");

        let mut g = conditional_store();
        let promoted = cratonvm_types::flags::with_thread_overrides(
            &[
                (PROMOTE, Some("1")),
                (PENDING, Some("1")),
                (BRANCH, Some("1")),
            ],
            || super::promote_loop_cells(&mut g),
        );
        assert_eq!(promoted, 1);
        let (exit, st) = g.promoted_sunk_stores[0];
        let stored = g.nodes[st as usize].inputs[4];
        let header_value = g.nodes[exit as usize].inputs[4];
        // The walk's value phi at the join: (stored, header value) in some order.
        let join_phi = (0..g.nodes.len())
            .find(|&i| {
                let n = &g.nodes[i];
                n.op == Op::Phi
                    && n.ty == IrType::Int
                    && n.inputs.len() == 3
                    && n.inputs[1..].contains(&stored)
                    && n.inputs[1..].contains(&header_value)
            })
            .expect("the join's value phi") as crate::ir::NodeId;
        let join = g.nodes[join_phi as usize].inputs[0];
        // A deopt point after the join.
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let cond = g.add(
            Op::Cmp(crate::ir::CmpOp::Ne),
            IrType::Int,
            vec![header_value, zero],
            Some(23),
        );
        let late = g.add(
            Op::Guard { bci: 23 },
            IrType::Void,
            vec![join, cond],
            Some(23),
        );
        let schedule = crate::ir_schedule::schedule(&g);
        let on = cratonvm_types::flags::with_thread_overrides(&[(BRANCH, Some("1"))], || {
            crate::ir_lower::plan_promoted_pending(&g, &schedule)
        })
        .expect("a sound plan")
        .expect("silent stores");
        assert_eq!(on.at_node.get(&late), Some(&vec![(exit, Some(join_phi))]));
        assert!(on.join_phis >= 1);
        let off_plan = cratonvm_types::flags::with_thread_overrides(&[(BRANCH, Some("0"))], || {
            crate::ir_lower::plan_promoted_pending(&g, &schedule)
        })
        .expect("a sound plan")
        .expect("silent stores");
        assert_eq!(off_plan.at_node.get(&late), Some(&vec![(exit, None)]));
        assert_eq!(off_plan.join_phis, 0);
        // The branch switch also pins the join phi's home word.
        let pins = cratonvm_types::flags::with_thread_overrides(&[(BRANCH, Some("1"))], || {
            crate::ir_lower::promoted_pending_slot_pins(&g, &schedule)
        });
        assert!(pins.contains(&join_phi));
        assert!(crate::ir_verify::verify_graph_at_phase(&g, "post-optimize").is_ok());
    }

    /// OSR5-1: under the chain switch a spliced deopt point is not predicted
    /// (a chain point takes its pad; the lowering still refuses a flat one).
    #[test]
    fn r19_osr7_the_chain_switch_lifts_the_splice_prediction() {
        let (g, guard) = guarded_after_store();
        let points = deopt_points(&g);
        assert!(points.contains(&guard));
        let inside = crate::ir::IrSpliceResumeMap {
            code_len: 10,
            bodies: vec![(10, 20, 3)],
        };
        let refused = cratonvm_types::flags::with_thread_overrides(&[(CHAIN, Some("0"))], || {
            super::predicted_pad_refusal_in(&g, &points, Some(&inside))
        });
        assert!(refused.is_some());
        let chained = cratonvm_types::flags::with_thread_overrides(&[(CHAIN, Some("1"))], || {
            super::predicted_pad_refusal_in(&g, &points, Some(&inside))
        });
        assert_eq!(chained, None);
    }

    /// OSR6-2: the census cells add and read back (monotonic: other tests in
    /// the process may add too).
    #[test]
    fn r19_osr7_the_promote_census_cells_count() {
        use crate::ir_check_elim as ce;
        let before = ce::promote_census();
        ce::note_promote_census(ce::PROMOTE_CENSUS_PREDICTED, 2);
        ce::note_promote_census(ce::PROMOTE_CENSUS_JOIN_PHIS, 1);
        ce::note_promote_census(99, 5); // ignored
        let after = ce::promote_census();
        assert!(after[ce::PROMOTE_CENSUS_PREDICTED] >= before[ce::PROMOTE_CENSUS_PREDICTED] + 2);
        assert!(after[ce::PROMOTE_CENSUS_JOIN_PHIS] > before[ce::PROMOTE_CENSUS_JOIN_PHIS]);
        assert_eq!(ce::PROMOTE_CENSUS_NAMES.len(), after.len());
    }

    /// The three wave-7 switches are opt-in.
    #[test]
    fn r19_osr7_the_switches_are_opt_in() {
        cratonvm_types::flags::with_thread_overrides(
            &[(RESIDENT, None), (CHAIN, None), (BRANCH, None)],
            || {
                assert!(!crate::ir_lower::ir_loop_field_promote_pending_resident_enabled());
                assert!(!crate::ir_lower::ir_loop_field_promote_pending_chain_enabled());
                assert!(!crate::ir_lower::ir_loop_field_promote_pending_branch_enabled());
            },
        );
    }

    // ── JIT round 19 wave 8 (lane osr8): OSR7-1, OSR7-2 ──

    const LIVE: &str = "CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PENDING_LIVE";
    const JOIN_WALK: &str = "CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_JOIN_WALK";

    /// `static int f(P p, int[] a, int n) { for (int i = 0; i < n; i++) if
    /// (i > 0) p.x = p.x + 1; return p.x; }` -- a store under a branch and NO
    /// deopt point (the `a[i]` of `CONDITIONAL_STORE` is `iload_3; nop; nop`).
    const BRANCH_NO_DEOPT: [u8; 34] = [
        0x03, 0x3e, // 0: i = 0
        0x1d, 0x1c, 0xa2, 0x00, 0x19, // 2: header: if (i >= n) goto 29
        0x1d, 0x00, 0x00, // 7: i
        0x9e, 0x00, 0x0d, // 10: ifle 23
        0x2a, 0x2a, 0xb4, 0x00, 0x01, 0x04, 0x60, // 13: p.x + 1 (getfield at 15)
        0xb5, 0x00, 0x01, // 20: putfield p.x
        0x84, 0x03, 0x01, 0xa7, 0xff, 0xe8, // 23: i++; 26: goto 2
        0x2a, 0xb4, 0x00, 0x01, 0xac, // 29: return p.x (getfield at 30)
    ];

    /// `static int f(P p, int[] a, int n) { for (int i = 0; i < n; i++) { if
    /// (a[i] > 0) p.x = p.x + 1; p.x = a[i + 1]; } return p.x; }` -- a join
    /// the exit walk never crosses (the unconditional store after it does not
    /// read the cell), with a deopt point (`a[i + 1]`) between the two.
    const JOIN_THEN_STORE: [u8; 43] = [
        0x03, 0x3e, // 0: i = 0
        0x1d, 0x1c, 0xa2, 0x00, 0x22, // 2: header: if (i >= n) goto 36
        0x2b, 0x1d, 0x2e, // 7: a[i] (iaload at 9)
        0x9e, 0x00, 0x0d, // 10: ifle 23
        0x2a, 0x2a, 0xb4, 0x00, 0x01, 0x04, 0x60, // 13: p.x + 1 (getfield at 15)
        0xb5, 0x00, 0x01, // 20: putfield p.x
        0x2a, 0x2b, 0x1d, 0x04, 0x60, 0x2e, // 23: p, a[i + 1] (iaload at 28)
        0xb5, 0x00, 0x01, // 29: putfield p.x
        0x84, 0x03, 0x01, 0xa7, 0xff, 0xdf, // 32: i++; 35: goto 2
        0x2a, 0xb4, 0x00, 0x01, 0xac, // 38: return p.x (getfield at 39)
    ];

    fn branch_graph(code: &[u8], fields: &[(usize, usize)], decls: &[(usize, u32)]) -> Graph {
        let mut g = build(
            code,
            &[IrType::Ref, IrType::Ref, IrType::Int],
            4,
            fields,
            decls,
        );
        forward(&mut g);
        super::super::scalar_cleanup(&mut g);
        g
    }

    /// OSR7-2 (a): without a deopt point, the default promotion refuses a
    /// store under a branch (the body-join memory φ is not in `loop_body`);
    /// under the join-walk switch it promotes it, deletes the store and
    /// writes the walk's exit value back.
    #[test]
    fn r19_osr8_a_store_under_a_branch_without_a_deopt_point_is_promoted() {
        let fields = [(15, 0), (20, 0), (30, 0)];
        let decls = [(15, 50), (20, 50), (30, 50)];
        let mut off = branch_graph(&BRANCH_NO_DEOPT, &fields, &decls);
        // (Overrides do not nest: each arm names every switch it needs.)
        let refused = cratonvm_types::flags::with_thread_overrides(
            &[(PROMOTE, Some("1")), (JOIN_WALK, Some("0"))],
            || super::promote_loop_cells(&mut off),
        );
        assert_eq!(refused, 0, "switch off: refused");
        assert!(off.promoted_cell_writebacks.is_empty());

        let mut g = branch_graph(&BRANCH_NO_DEOPT, &fields, &decls);
        let promoted = cratonvm_types::flags::with_thread_overrides(
            &[(PROMOTE, Some("1")), (JOIN_WALK, Some("1"))],
            || super::promote_loop_cells(&mut g),
        );
        assert_eq!(promoted, 1);
        super::super::scalar_cleanup(&mut g);
        assert!(g.promoted_sunk_stores.is_empty(), "no deopt point: deleted");
        assert_eq!(live_stores(&g).len(), 1, "only the exit write-back stays");
        assert!(g.promoted_join_phis.is_empty(), "no deopt point: none kept");
        assert!(crate::ir_verify::verify_graph_at_phase(&g, "post-optimize").is_ok());
    }

    /// OSR7-2 (b): a join the exit walk never crosses has no value φ, so the
    /// plan cannot name the cell at the `a[i + 1]` read after it (the lowering
    /// then refuses the compile); under the join-walk switch the forced walk
    /// builds that φ, dead-node elimination keeps it, and the plan names it.
    #[test]
    fn r19_osr8_the_forced_walk_names_a_join_the_exit_never_crosses() {
        let fields = [(15, 0), (20, 0), (29, 0), (39, 0)];
        let decls = [(15, 50), (20, 50), (29, 50), (39, 50)];
        let arms = |walk: &str| {
            let mut g = branch_graph(&JOIN_THEN_STORE, &fields, &decls);
            let promoted = cratonvm_types::flags::with_thread_overrides(
                &[
                    (PROMOTE, Some("1")),
                    (PENDING, Some("1")),
                    (BRANCH, Some("1")),
                    (JOIN_WALK, Some(walk)),
                ],
                || {
                    let n = super::promote_loop_cells(&mut g);
                    super::super::scalar_cleanup(&mut g);
                    n
                },
            );
            (g, promoted)
        };
        let late_point = |g: &Graph| {
            (0..g.nodes.len())
                .find(|&i| {
                    matches!(g.nodes[i].op, Op::ArrayLoad(_)) && g.nodes[i].bytecode_pc == Some(28)
                })
                .expect("the a[i + 1] read") as crate::ir::NodeId
        };
        let plan_at = |g: &Graph, at: crate::ir::NodeId| {
            let schedule = crate::ir_schedule::schedule(g);
            let plan = cratonvm_types::flags::with_thread_overrides(&[(BRANCH, Some("1"))], || {
                crate::ir_lower::plan_promoted_pending(g, &schedule)
            })
            .expect("a sound plan")
            .expect("silent stores");
            plan.at_node.get(&at).cloned().expect("a body node")
        };

        let (off, n_off) = arms("0");
        assert_eq!(n_off, 1);
        assert!(off.promoted_join_phis.is_empty());
        let at_off = plan_at(&off, late_point(&off));
        assert_eq!(at_off.len(), 1);
        assert_eq!(at_off[0].1, None, "switch off: unknown at the late read");

        let (on, n_on) = arms("1");
        assert_eq!(n_on, 1);
        assert!(
            !on.promoted_join_phis.is_empty(),
            "the forced join phi is kept"
        );
        for &phi in &on.promoted_join_phis {
            assert_eq!(on.nodes[phi as usize].op, Op::Phi, "survives the cleanup");
        }
        let at_on = plan_at(&on, late_point(&on));
        let named = at_on[0].1.expect("the join's value phi");
        assert!(on.promoted_join_phis.contains(&named));
        assert!(crate::ir_verify::verify_graph_at_phase(&on, "post-optimize").is_ok());
    }

    /// OSR7-1: under the live switch every pending value the plan names at a
    /// deopt point is live from its definition to one past the point, in the
    /// allocator's model, so `pending_resident_reg`'s strict test can pass.
    #[test]
    fn r19_osr8_pending_values_are_live_to_their_pads() {
        let (mut g, guard) = guarded_after_store();
        assert_eq!(promote_pending(&mut g), 1);
        let schedule = crate::ir_schedule::schedule(&g);
        let widen = |live: &str| {
            cratonvm_types::flags::with_thread_overrides(
                &[(RESIDENT, Some("1")), (LIVE, Some(live))],
                || crate::ir_lower::pending_deopt_live_widening(&g, &schedule),
            )
        };
        assert!(widen("0").rows.is_empty(), "switch off: nothing");
        let without_resident = cratonvm_types::flags::with_thread_overrides(
            &[(RESIDENT, Some("0")), (LIVE, Some("1"))],
            || crate::ir_lower::pending_deopt_live_widening(&g, &schedule),
        );
        assert!(without_resident.rows.is_empty(), "inert without RESIDENT");
        let on = widen("1");
        assert!(!on.rows.is_empty());
        let named = plan(&g).at_node.get(&guard).cloned().expect("the guard");
        let mut live = crate::regalloc::build_live_model(&g, &schedule);
        if !live.converged {
            return;
        }
        assert_eq!(live.span, on.span, "one position numbering");
        let p = live.pos_of[guard as usize].expect("the guard is placed");
        crate::ir_lower::widen_live_model_for_pending(&mut live, &on);
        for (_, v) in named {
            let v = v.expect("a known value at the guard");
            if matches!(g.nodes[v as usize].op, Op::Const(_)) {
                continue;
            }
            let r = live.range[v as usize].expect("a located value");
            assert!(
                r.lo < p && p < r.hi,
                "n{v} [{}, {}] strictly holds {p}",
                r.lo,
                r.hi
            );
        }
    }

    /// The wave-8 switches are opt-in.
    #[test]
    fn r19_osr8_the_switches_are_opt_in() {
        cratonvm_types::flags::with_thread_overrides(&[(LIVE, None), (JOIN_WALK, None)], || {
            assert!(!crate::ir_lower::ir_loop_field_promote_pending_live_enabled());
            assert!(!super::join_walk_enabled());
        });
    }
}

/// JIT round 20 wave 11 (lane pLoop, OSR4-2): the static cell promotion on
/// hand-built `COUNTER += i` loops.
#[cfg(test)]
mod jr20_ploop_static_promote_tests {
    use super::{
        promote_loop_statics, static_promote_enabled, static_promote_method_entry_enabled,
    };
    use crate::ir::{Graph, IrType, NodeId, Op, SafepointSnapshot};

    /// What the loop body holds besides `COUNTER = COUNTER + i`.
    #[derive(Clone, Copy, PartialEq, Debug)]
    enum Extra {
        None,
        /// The counter is volatile.
        Volatile,
        /// A call in the body.
        Call,
        /// A guard (a deopt point) in the body.
        Guard,
        /// The cell is not on the compile's direct list (helper route).
        NotDirect,
        /// A `long` counter.
        Long,
    }

    struct Built {
        g: Graph,
        load: NodeId,
        store: NodeId,
        after: NodeId,
        header: NodeId,
    }

    /// `for (i = 0; i < 100; i++) COUNTER += i; x = COUNTER;` -- header
    /// `Merge(entry, back)` at bci 2, `getstatic` at 5, `putstatic` at 8,
    /// the read after the loop at 20.
    fn build(extra: Extra) -> Built {
        let long = extra == Extra::Long;
        let (ty, tag) = if long {
            (IrType::Long, b'J')
        } else {
            (IrType::Int, b'I')
        };
        let is_volatile = extra == Extra::Volatile;
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let mem0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let header = g.add(Op::Merge, IrType::Control, vec![entry], Some(2));
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let hundred = g.add(Op::Const(100), IrType::Int, vec![], None);
        let i = g.add(Op::Phi, IrType::Int, vec![header, zero], None);
        let mphi = g.add(Op::Phi, IrType::Memory, vec![header, mem0], None);
        let cmp = g.add(
            Op::Cmp(crate::ir::CmpOp::Lt),
            IrType::Int,
            vec![i, hundred],
            None,
        );
        let iff = g.add(Op::If, IrType::Control, vec![header, cmp], Some(4));
        let stay = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
        let exit = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
        let get = Op::LoadStatic {
            class_id: 9,
            field_index: 0,
            type_tag: tag,
            is_volatile,
        };
        let load = g.add(get.clone(), ty, vec![stay, mphi], Some(5));
        let addend = if long {
            g.add(Op::I2L, IrType::Long, vec![i], None)
        } else {
            i
        };
        let sum = g.add(Op::Add, ty, vec![load, addend], None);
        let mut mem = load;
        if extra == Extra::Call {
            mem = g.add(
                Op::Call { info_ptr: 0 },
                IrType::Void,
                vec![stay, mem],
                Some(6),
            );
        }
        if extra == Extra::Guard {
            let c = g.add(
                Op::Cmp(crate::ir::CmpOp::Ne),
                IrType::Int,
                vec![i, hundred],
                None,
            );
            g.add(Op::Guard { bci: 7 }, IrType::Void, vec![stay, c], Some(7));
        }
        let store = g.add(
            Op::StoreStatic {
                class_id: 9,
                field_index: 0,
                type_tag: tag,
                is_volatile,
            },
            IrType::Memory,
            vec![stay, mem, sum],
            Some(8),
        );
        let next = g.add(Op::Add, IrType::Int, vec![i, one], None);
        g.nodes[header as usize].inputs.push(stay);
        g.nodes[i as usize].inputs.push(next);
        g.nodes[mphi as usize].inputs.push(store);
        let after = g.add(get, ty, vec![exit, mphi], Some(20));
        let ret = g.add(Op::Return, IrType::Void, vec![exit], None);
        g.exit = ret;
        g.push_safepoint(SafepointSnapshot::new(2, vec![i], vec![], vec![]));
        if extra != Extra::NotDirect {
            g.direct_static_fields = vec![(9, 0)];
        }
        Built {
            g,
            load,
            store,
            after,
            header,
        }
    }

    fn live_static_stores(g: &Graph) -> Vec<NodeId> {
        g.nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| matches!(n.op, Op::StoreStatic { .. }))
            // Cast: arena index to node id.
            .map(|(id, _)| id as NodeId)
            .collect()
    }

    #[test]
    fn a_counter_loop_reads_once_and_writes_back_at_the_exit() {
        for extra in [Extra::None, Extra::Long] {
            let mut b = build(extra);
            assert_eq!(promote_loop_statics(&mut b.g), 1, "{extra:?}");
            assert_eq!(
                b.g.nodes[b.load as usize].op,
                Op::Dead,
                "the in-loop read is gone"
            );
            assert_eq!(
                b.g.nodes[b.store as usize].op,
                Op::Dead,
                "the in-loop store is gone"
            );
            let stores = live_static_stores(&b.g);
            assert_eq!(stores.len(), 1, "one exit write-back");
            let wb = stores[0];
            assert_eq!(b.g.promoted_cell_writebacks, vec![(b.header, wb)]);
            assert_eq!(b.g.promoted_cell_header_bcis, vec![2]);
            // Conditional: under the taken edge of `If(Cmp(Ne, ..))`.
            let taken = b.g.nodes[wb as usize].inputs[0];
            assert_eq!(b.g.nodes[taken as usize].op, Op::Proj(0));
            // The read after the loop reads the write-back's memory φ.
            let after_mem = b.g.nodes[b.after as usize].inputs[1];
            let phi = &b.g.nodes[after_mem as usize];
            assert!(phi.op == Op::Phi && phi.ty == IrType::Memory);
            assert!(phi.inputs.contains(&wb), "the φ joins the write-back");
            // One read of the cell before the loop, at the header's bci.
            let pre =
                b.g.nodes
                    .iter()
                    .filter(|n| matches!(n.op, Op::LoadStatic { .. }) && n.bytecode_pc == Some(2))
                    .count();
            assert_eq!(pre, 1, "{extra:?}");
        }
    }

    #[test]
    fn a_volatile_call_guard_or_helper_routed_cell_is_left_alone() {
        for extra in [Extra::Volatile, Extra::Call, Extra::Guard, Extra::NotDirect] {
            let mut b = build(extra);
            assert_eq!(promote_loop_statics(&mut b.g), 0, "{extra:?}");
            assert!(b.g.nodes[b.store as usize].op != Op::Dead, "{extra:?}");
            assert!(b.g.promoted_cell_header_bcis.is_empty(), "{extra:?}");
            assert!(b.g.promoted_cell_writebacks.is_empty(), "{extra:?}");
        }
    }

    #[test]
    fn the_switch_is_default_on_with_a_zero_kill() {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_LOOP_STATIC_PROMOTE", None)],
            || assert!(static_promote_enabled()),
        );
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_LOOP_STATIC_PROMOTE", Some("0"))],
            || assert!(!static_promote_enabled()),
        );
    }

    /// JIT round 20 wave 17 (lane pAtom2): the method-entry placement is its
    /// own default-ON row with a zero kill.
    #[test]
    fn the_method_entry_placement_is_default_on_with_a_zero_kill() {
        const KEY: &str = "CRATONVM_JIT_IR_LOOP_STATIC_PROMOTE_METHOD_ENTRY";
        cratonvm_types::flags::with_thread_overrides(&[(KEY, None)], || {
            assert!(static_promote_method_entry_enabled())
        });
        cratonvm_types::flags::with_thread_overrides(&[(KEY, Some("0"))], || {
            assert!(!static_promote_method_entry_enabled())
        });
    }
}

#[cfg(test)]
mod jr20_patom2_fresh_promote_tests {
    use super::{fresh_base_unpublished, fresh_promote_enabled, promote_fresh_loop_fields};
    use crate::ir::{Graph, IrType, MemKind, NodeId, Op, SafepointSnapshot};

    /// What the graph holds besides the accumulator loop.
    #[derive(Clone, Copy, PartialEq, Debug)]
    enum Extra {
        None,
        /// A `long` cell.
        Long,
        /// A call in the loop body.
        Call,
        /// A guard (a deopt point) in the loop body.
        Guard,
        /// The object is stored into another object's field before the loop.
        StoredBefore,
        /// The object is passed to a call before the loop (an unspliced `<init>`).
        CallArgBefore,
        /// The object is stored into another object's field AFTER the loop:
        /// still a store of the base, so still refused (the rule names no time).
        StoredAfter,
    }

    struct Built {
        g: Graph,
        base: NodeId,
        load: NodeId,
        store: NodeId,
        after: NodeId,
        header: NodeId,
    }

    /// `Acc a = new Acc(); for (i = 0; i < 100; i++) a.v += i; x = a.v;
    /// return a;` -- the allocation before the header, the header `Merge` at
    /// bci 2, `getfield` at 5, `putfield` at 8, the read after the loop at 20.
    fn build(extra: Extra) -> Built {
        let long = extra == Extra::Long;
        let (ty, kind) = if long {
            (IrType::Long, MemKind::Long)
        } else {
            (IrType::Int, MemKind::Int)
        };
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let mem0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let other = g.add(Op::Param(0), IrType::Ref, vec![], None);
        let base = g.add(
            Op::New {
                class_id: 5,
                num_fields: 2,
            },
            IrType::Ref,
            vec![entry, mem0],
            Some(0),
        );
        let idx = g.add(Op::Const(16), IrType::Int, vec![], None);
        let ref_idx = g.add(Op::Const(24), IrType::Int, vec![], None);
        let mut pre_mem = base;
        if extra == Extra::StoredBefore {
            let st = g.add(
                Op::Store(MemKind::Ref),
                IrType::Memory,
                vec![entry, pre_mem, other, ref_idx, base],
                Some(1),
            );
            g.field_decl_class.insert(st, 6);
            pre_mem = st;
        }
        if extra == Extra::CallArgBefore {
            pre_mem = g.add(
                Op::Call { info_ptr: 0 },
                IrType::Void,
                vec![entry, pre_mem, base],
                Some(1),
            );
        }
        let header = g.add(Op::Merge, IrType::Control, vec![entry], Some(2));
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let hundred = g.add(Op::Const(100), IrType::Int, vec![], None);
        let i = g.add(Op::Phi, IrType::Int, vec![header, zero], None);
        let mphi = g.add(Op::Phi, IrType::Memory, vec![header, pre_mem], None);
        let cmp = g.add(
            Op::Cmp(crate::ir::CmpOp::Lt),
            IrType::Int,
            vec![i, hundred],
            None,
        );
        let iff = g.add(Op::If, IrType::Control, vec![header, cmp], Some(4));
        let stay = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
        let exit = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
        let load = g.add(Op::Load(kind), ty, vec![stay, mphi, base, idx], Some(5));
        g.field_decl_class.insert(load, 5);
        let addend = if long {
            g.add(Op::I2L, IrType::Long, vec![i], None)
        } else {
            i
        };
        let sum = g.add(Op::Add, ty, vec![load, addend], None);
        let mut mem = load;
        if extra == Extra::Call {
            mem = g.add(
                Op::Call { info_ptr: 0 },
                IrType::Void,
                vec![stay, mem],
                Some(6),
            );
        }
        if extra == Extra::Guard {
            let c = g.add(
                Op::Cmp(crate::ir::CmpOp::Ne),
                IrType::Int,
                vec![i, hundred],
                None,
            );
            g.add(Op::Guard { bci: 7 }, IrType::Void, vec![stay, c], Some(7));
        }
        let store = g.add(
            Op::Store(kind),
            IrType::Memory,
            vec![stay, mem, base, idx, sum],
            Some(8),
        );
        g.field_decl_class.insert(store, 5);
        let next = g.add(Op::Add, IrType::Int, vec![i, one], None);
        g.nodes[header as usize].inputs.push(stay);
        g.nodes[i as usize].inputs.push(next);
        g.nodes[mphi as usize].inputs.push(store);
        let after = g.add(Op::Load(kind), ty, vec![exit, mphi, base, idx], Some(20));
        g.field_decl_class.insert(after, 5);
        if extra == Extra::StoredAfter {
            let st = g.add(
                Op::Store(MemKind::Ref),
                IrType::Memory,
                vec![exit, after, other, ref_idx, base],
                Some(22),
            );
            g.field_decl_class.insert(st, 6);
        }
        let ret = g.add(Op::Return, IrType::Void, vec![exit, base], None);
        g.exit = ret;
        g.push_safepoint(SafepointSnapshot::new(2, vec![i, base], vec![], vec![]));
        Built {
            g,
            base,
            load,
            store,
            after,
            header,
        }
    }

    fn live_field_stores(g: &Graph) -> Vec<NodeId> {
        g.nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| matches!(n.op, Op::Store(MemKind::Int | MemKind::Long)))
            // Cast: arena index to node id.
            .map(|(id, _)| id as NodeId)
            .collect()
    }

    #[test]
    fn an_accumulator_of_a_fresh_returned_object_is_written_back_at_the_exit() {
        for extra in [Extra::None, Extra::Long] {
            let mut b = build(extra);
            assert!(fresh_base_unpublished(&b.g, b.base).is_ok(), "{extra:?}");
            assert_eq!(promote_fresh_loop_fields(&mut b.g), 1, "{extra:?}");
            assert_eq!(
                b.g.nodes[b.load as usize].op,
                Op::Dead,
                "the in-loop read is gone"
            );
            assert_eq!(
                b.g.nodes[b.store as usize].op,
                Op::Dead,
                "the in-loop store is gone"
            );
            let stores = live_field_stores(&b.g);
            assert_eq!(stores.len(), 1, "one exit write-back");
            let wb = stores[0];
            assert_eq!(
                b.g.nodes[wb as usize].inputs[2], b.base,
                "through the same base"
            );
            assert_eq!(b.g.promoted_cell_writebacks, vec![(b.header, wb)]);
            assert_eq!(b.g.promoted_cell_header_bcis, vec![2]);
            let taken = b.g.nodes[wb as usize].inputs[0];
            assert_eq!(b.g.nodes[taken as usize].op, Op::Proj(0));
            // The read after the loop reads the write-back's memory φ.
            let after_mem = b.g.nodes[b.after as usize].inputs[1];
            let phi = &b.g.nodes[after_mem as usize];
            assert!(phi.op == Op::Phi && phi.ty == IrType::Memory);
            assert!(phi.inputs.contains(&wb), "the φ joins the write-back");
            // One read of the cell before the loop, at the header's bci.
            let pre =
                b.g.nodes
                    .iter()
                    .filter(|n| matches!(n.op, Op::Load(_)) && n.bytecode_pc == Some(2))
                    .count();
            assert_eq!(pre, 1, "{extra:?}");
        }
    }

    #[test]
    fn a_published_base_or_a_transfer_in_the_loop_is_left_alone() {
        for extra in [
            Extra::Call,
            Extra::Guard,
            Extra::StoredBefore,
            Extra::CallArgBefore,
            Extra::StoredAfter,
        ] {
            let mut b = build(extra);
            assert_eq!(promote_fresh_loop_fields(&mut b.g), 0, "{extra:?}");
            assert!(b.g.nodes[b.store as usize].op != Op::Dead, "{extra:?}");
            assert!(b.g.nodes[b.load as usize].op != Op::Dead, "{extra:?}");
            assert!(b.g.promoted_cell_header_bcis.is_empty(), "{extra:?}");
            assert!(b.g.promoted_cell_writebacks.is_empty(), "{extra:?}");
        }
        for extra in [
            Extra::StoredBefore,
            Extra::CallArgBefore,
            Extra::StoredAfter,
        ] {
            let b = build(extra);
            assert!(fresh_base_unpublished(&b.g, b.base).is_err(), "{extra:?}");
        }
    }

    /// A parameter is not a fresh object of this compile.
    #[test]
    fn only_an_allocation_of_this_compile_is_fresh() {
        let b = build(Extra::None);
        let param =
            b.g.nodes
                .iter()
                .position(|n| matches!(n.op, Op::Param(_)))
                .expect("the fixture's parameter");
        // Cast: arena index to node id.
        assert!(fresh_base_unpublished(&b.g, param as NodeId).is_err());
    }

    #[test]
    fn the_switch_is_default_on_with_a_zero_kill() {
        const KEY: &str = "CRATONVM_JIT_IR_LOOP_FRESH_FIELD_PROMOTE";
        cratonvm_types::flags::with_thread_overrides(&[(KEY, None)], || {
            assert!(fresh_promote_enabled())
        });
        cratonvm_types::flags::with_thread_overrides(&[(KEY, Some("0"))], || {
            assert!(!fresh_promote_enabled())
        });
    }
}

#[cfg(test)]
mod r21_loops_method_entry_promote_tests {
    use super::{method_entry_promote_enabled, promote_method_entry_accumulators};
    use crate::ir::{Graph, IrBuilder, IrType, NodeId, Op};
    use std::collections::HashMap;

    const KEY: &str = "CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_METHOD_ENTRY";

    /// `int f(int n) { for (int i = 0; i < n; i++) this.x = this.x + 1;
    /// return this.x; }` with `this` local 0 (the forward tests'
    /// `INCREMENT_IN_PLACE`, read as an instance method when `receiver`).
    const INCREMENT_IN_PLACE: [u8; 28] = [
        0x03, 0x3d, // 0: i = 0
        0x1c, 0x1b, 0xa2, 0x00, 0x13, // 2: header: if (i >= n) goto 23
        0x2a, 0x2a, 0xb4, 0x00, 0x01, 0x04, 0x60, 0xb5, 0x00, 0x01, // 7: x = x + 1
        0x84, 0x02, 0x01, 0xa7, 0xff, 0xee, // 17: i++; 20: goto 2
        0x2a, 0xb4, 0x00, 0x01, 0xac, // 23: return x (getfield at 24)
    ];

    /// `for (..) { s += p.x; p.y = i; }` -- `x` field 0, `y` field 1: the
    /// store is not an accumulator (no load of its own cell).
    const READ_ONE_WRITE_OTHER: [u8; 29] = [
        0x03, 0x3d, 0x03, 0x3e, // 0: s = 0; 2: i = 0
        0x1d, 0x1b, 0xa2, 0x00, 0x15, // 4: header: if (i >= n) goto 27
        0x1c, 0x2a, 0xb4, 0x00, 0x01, 0x60, 0x3d, // 9: s += p.x (getfield at 11)
        0x2a, 0x1d, 0xb5, 0x00, 0x02, // 16: p.y = i (putfield at 18)
        0x84, 0x03, 0x01, 0xa7, 0xff, 0xec, // 21: i++; 24: goto 4
        0x1c, 0xac, // 27: return s
    ];

    fn build_code(
        code: &[u8],
        max_locals: usize,
        fields: &[(usize, usize)],
        decls: &[(usize, u32)],
        receiver: bool,
    ) -> Graph {
        let params = [IrType::Ref, IrType::Int];
        let mut b = IrBuilder::new(params.len(), max_locals);
        b.set_param_types(&params);
        b.set_field_info(fields.iter().map(|&(pc, idx)| (pc, (idx, b'I'))).collect());
        b.set_field_decl_classes(decls.iter().copied().collect::<HashMap<usize, u32>>());
        let mut g = b.build(code, code.len()).expect("builds");
        if receiver {
            g.receiver_param = Some(0);
        }
        super::super::scalar_cleanup(&mut g);
        g
    }

    fn increment(receiver: bool) -> Graph {
        build_code(
            &INCREMENT_IN_PLACE,
            3,
            &[(9, 0), (14, 0), (24, 0)],
            &[(9, 50), (14, 50), (24, 50)],
            receiver,
        )
    }

    fn run(g: &mut Graph, on: bool) -> (usize, usize) {
        cratonvm_types::flags::with_thread_overrides(
            &[
                (KEY, if on { Some("1") } else { None }),
                ("CRATONVM_JIT_IR_LOOP_FIELD_FORWARD", None),
                ("CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_NULL_GUARD", None),
                ("CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE", None),
            ],
            || promote_method_entry_accumulators(g, None),
        )
    }

    fn live_stores(g: &Graph) -> Vec<NodeId> {
        (0..g.nodes.len())
            .filter(|&i| matches!(g.nodes[i].op, Op::Store(_)))
            // Cast: arena index to node id.
            .map(|i| i as NodeId)
            .collect()
    }

    /// `this.x += 1` in a method-entry body: the census names the loop, the
    /// receiver needs no null guard, the in-loop load and store go, and one
    /// conditional write-back at the header's bci runs on the exit edge.
    #[test]
    fn r21_loops_a_receiver_accumulator_is_promoted_in_a_method_entry_body() {
        let mut g = increment(true);
        assert_eq!(
            super::super::loop_static_hoist::accumulator_loop_headers(&g).len(),
            1,
            "the census names the accumulator loop"
        );
        let (forwarded, promoted) = run(&mut g, true);
        assert!(forwarded >= 1, "the in-loop read is forwarded");
        assert_eq!(promoted, 1);
        assert_eq!(g.promoted_cell_header_bcis, vec![2]);
        let stores = live_stores(&g);
        let [st] = stores.as_slice() else {
            panic!("one store left, got {}", stores.len());
        };
        assert_eq!(
            g.nodes[*st as usize].bytecode_pc,
            Some(2),
            "the exit write-back, not the loop's store at 14"
        );
        let taken = g.nodes[*st as usize].inputs[0];
        assert_eq!(g.nodes[taken as usize].op, Op::Proj(0), "conditional");
        assert!(
            !g.nodes.iter().any(|n| matches!(n.op, Op::Guard { .. })),
            "the receiver is known non-null: no pre-header null guard"
        );
        assert!(crate::ir_verify::verify_graph_at_phase(&g, "post-optimize").is_ok());
    }

    /// The same loop in a STATIC method (`p` a parameter that may be null)
    /// keeps its in-loop accesses, and with the switch off the receiver rule
    /// is not applied either.
    #[test]
    fn r21_loops_a_possibly_null_base_or_the_switch_off_keeps_the_stores() {
        for (receiver, on) in [(false, true), (true, false)] {
            let mut g = increment(receiver);
            assert_eq!(run(&mut g, on), (0, 0), "receiver={receiver} on={on}");
            assert!(g.promoted_cell_header_bcis.is_empty());
            assert!(
                live_stores(&g)
                    .iter()
                    .any(|&s| g.nodes[s as usize].bytecode_pc == Some(14)),
                "the in-loop store stays"
            );
        }
    }

    /// A store with no load of its own cell is no accumulator: the screen
    /// offers the loop to nothing.
    #[test]
    fn r21_loops_a_store_of_another_cell_is_not_an_accumulator() {
        let g = build_code(
            &READ_ONE_WRITE_OTHER,
            4,
            &[(11, 0), (18, 1)],
            &[(11, 50), (18, 50)],
            true,
        );
        assert!(super::super::loop_static_hoist::accumulator_loop_headers(&g).is_empty());
    }

    #[test]
    fn r21_loops_the_switch_is_default_off() {
        cratonvm_types::flags::with_thread_overrides(&[(KEY, None)], || {
            assert!(!method_entry_promote_enabled())
        });
        cratonvm_types::flags::with_thread_overrides(&[(KEY, Some("1"))], || {
            assert!(method_entry_promote_enabled())
        });
    }
}

#[cfg(test)]
mod r21_loops_array_element_promote_tests {
    use super::{array_element_promote_enabled, fresh_promote_static_feed_enabled};
    use super::promote_loop_array_elements;
    use crate::ir::{Graph, IrBuilder, IrType, MemKind, NodeId, Op};

    /// `static void f(int[] a, int n) { for (int i = 0; i < n; i++) a[0] += i; }`
    /// -- header at bci 2, `iaload` at 10, `iastore` at 13.
    const ACC_ZERO: [u8; 21] = [
        0x03, 0x3d, // 0: i = 0
        0x1c, 0x1b, 0xa2, 0x00, 0x10, // 2: header: if (i >= n) goto 20
        0x2a, 0x03, 0x5c, 0x2e, 0x1c, 0x60, 0x4f, // 7: a[0] = a[0] + i
        0x84, 0x02, 0x01, 0xa7, 0xff, 0xf1, // 14: i++; 17: goto 2
        0xb1, // 20: return
    ];

    /// `for (..) a[0] += a[i];` -- a second `int` element of the same array
    /// (it may be the cell itself when `i == 0`).
    const ACC_ALIAS: [u8; 23] = [
        0x03, 0x3d, // 0: i = 0
        0x1c, 0x1b, 0xa2, 0x00, 0x12, // 2: header: if (i >= n) goto 22
        0x2a, 0x03, 0x5c, 0x2e, 0x2a, 0x1c, 0x2e, 0x60, 0x4f, // 7: a[0] = a[0] + a[i]
        0x84, 0x02, 0x01, 0xa7, 0xff, 0xef, // 16: i++; 19: goto 2
        0xb1, // 22: return
    ];

    fn build(code: &[u8]) -> Graph {
        let params = [IrType::Ref, IrType::Int];
        let mut b = IrBuilder::new(params.len(), 3);
        b.set_param_types(&params);
        let mut g = b.build(code, code.len()).expect("builds");
        super::super::scalar_cleanup(&mut g);
        g
    }

    fn live(g: &Graph, f: impl Fn(&Op) -> bool) -> Vec<NodeId> {
        (0..g.nodes.len())
            .filter(|&i| f(&g.nodes[i].op))
            // Cast: arena index to node id.
            .map(|i| i as NodeId)
            .collect()
    }

    /// `a[0] += i`: the in-loop load and store go; the pre-header holds the
    /// length, two index guards (with the pre-header frame) and the read; one
    /// conditional store at the header's bci runs on the exit edge.
    #[test]
    fn r21_loops_an_invariant_element_accumulator_is_promoted() {
        let mut g = build(&ACC_ZERO);
        assert_eq!(promote_loop_array_elements(&mut g, None), 1);
        assert_eq!(g.promoted_cell_header_bcis, vec![2]);
        assert!(
            g.promoted_cell_writebacks.is_empty(),
            "the poll pad has no element shape: no record, so its exit is refused"
        );
        let stores = live(&g, |op| matches!(op, Op::ArrayStore(MemKind::Int)));
        let [st] = stores.as_slice() else {
            panic!("one store left, got {}", stores.len());
        };
        assert_eq!(g.nodes[*st as usize].bytecode_pc, Some(2));
        let taken = g.nodes[*st as usize].inputs[0];
        assert_eq!(g.nodes[taken as usize].op, Op::Proj(0), "conditional");
        let loads = live(&g, |op| matches!(op, Op::ArrayLoad(MemKind::Int)));
        assert!(
            loads.iter().all(|&l| g.nodes[l as usize].bytecode_pc == Some(2)),
            "only the pre-header read is left"
        );
        let guards: Vec<NodeId> = live(&g, |op| matches!(op, Op::Guard { bci: 2 }));
        assert_eq!(guards.len(), 2, "0 <= k and k < a.length");
        assert!(guards.iter().all(|&x| g.nodes[x as usize].frame_snapshot.is_some()));
        assert!(crate::ir_verify::verify_graph_at_phase(&g, "post-optimize").is_ok());
    }

    /// A second element of the same kind (it may alias the cell) or a
    /// header the registry withdrew keeps every access.
    #[test]
    fn r21_loops_an_aliasing_access_or_a_withdrawn_header_keeps_the_loop() {
        let mut g = build(&ACC_ALIAS);
        assert_eq!(promote_loop_array_elements(&mut g, None), 0);
        assert!(g.promoted_cell_header_bcis.is_empty());
        assert_eq!(live(&g, |op| matches!(op, Op::ArrayStore(_))).len(), 1);
        assert!(live(&g, |op| matches!(op, Op::Guard { .. })).is_empty());

        let mut h = build(&ACC_ZERO);
        let reg = crate::deopt::DespecRegistry::new();
        reg.insert("T.f:([II)V", 2, crate::deopt::SPECULATION_ID_ANY);
        assert_eq!(promote_loop_array_elements(&mut h, Some((&reg, "T.f:([II)V"))), 0);
        assert!(h.promoted_cell_header_bcis.is_empty());
        assert!(live(&h, |op| matches!(op, Op::ArrayStore(_)))
            .iter()
            .any(|&s| h.nodes[s as usize].bytecode_pc == Some(13)));
    }

    #[test]
    fn r21_loops_the_element_switch_is_default_off() {
        const KEY: &str = "CRATONVM_JIT_IR_LOOP_ARRAY_ELEMENT_PROMOTE";
        cratonvm_types::flags::with_thread_overrides(&[(KEY, None)], || {
            assert!(!array_element_promote_enabled())
        });
        cratonvm_types::flags::with_thread_overrides(&[(KEY, Some("1"))], || {
            assert!(array_element_promote_enabled())
        });
    }

    /// LOOPS21-3: the fresh promotion's feed row is default ON, `=0` restores
    /// the old list, and it follows the fresh promotion's own switch.
    #[test]
    fn r21_loops_the_fresh_promotion_static_feed_row() {
        const FEED: &str = "CRATONVM_JIT_IR_FRESH_PROMOTE_STATIC_FEED";
        const FRESH: &str = "CRATONVM_JIT_IR_LOOP_FRESH_FIELD_PROMOTE";
        cratonvm_types::flags::with_thread_overrides(&[(FEED, None), (FRESH, None)], || {
            assert!(fresh_promote_static_feed_enabled())
        });
        cratonvm_types::flags::with_thread_overrides(&[(FEED, Some("0")), (FRESH, None)], || {
            assert!(!fresh_promote_static_feed_enabled())
        });
        cratonvm_types::flags::with_thread_overrides(&[(FEED, None), (FRESH, Some("0"))], || {
            assert!(!fresh_promote_static_feed_enabled())
        });
    }
}

#[cfg(test)]
mod r21_loops_spliced_promote_tests {
    use super::{base_null_checked_before, promote_spliced_enabled, promotion_header_frame};
    use crate::ir::{CmpOp, Graph, IrType, NodeId, Op, SafepointSnapshot, SpliceScopeChain};

    const KEY: &str = "CRATONVM_JIT_IR_LOOP_PROMOTE_SPLICED";

    /// `start -> c0`, a reference `p`, and (when `guarded`) the splice's
    /// `Guard(p != null)` at `c0`; then an `If` on an int whose two arms are
    /// `left` / `right`. Returns `(graph, p, c0, left, right)`.
    fn shape(guard_on_left: Option<bool>) -> (Graph, NodeId, NodeId, NodeId, NodeId) {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let p = g.add(Op::Param(0), IrType::Ref, vec![], None);
        let x = g.add(Op::Param(1), IrType::Int, vec![], None);
        let iff = g.add(Op::If, IrType::Control, vec![c0, x], Some(3));
        let left = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
        let right = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
        if let Some(on_left) = guard_on_left {
            let null = g.add(Op::Const(0), IrType::Ref, vec![], None);
            let ne = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![p, null], Some(1));
            let at = if on_left { left } else { c0 };
            g.add(Op::Guard { bci: 1 }, IrType::Void, vec![at, ne], Some(1));
        }
        (g, p, c0, left, right)
    }

    /// A null guard dominating the loop entry proves the base; one on another
    /// arm, or none, does not.
    #[test]
    fn r21_loops_a_dominating_null_guard_proves_the_base() {
        let (g, p, c0, left, right) = shape(Some(false));
        assert!(base_null_checked_before(&g, p, c0), "same control node");
        assert!(base_null_checked_before(&g, p, left), "through a branch projection");
        assert!(base_null_checked_before(&g, p, right));
        let (g, p, _, left, right) = shape(Some(true));
        assert!(base_null_checked_before(&g, p, left));
        assert!(!base_null_checked_before(&g, p, right), "the other arm");
        let (g, p, c0, _, _) = shape(None);
        assert!(!base_null_checked_before(&g, p, c0), "no guard");
    }

    /// A spliced loop's header has only a chain snapshot: it serves the
    /// write-back under the switch, and nothing does without it.
    #[test]
    fn r21_loops_a_spliced_header_frame_serves_only_under_the_switch() {
        let mut g = Graph::empty(0);
        let mut chain = SafepointSnapshot::new(40, vec![], vec![], vec![]);
        chain.splice_scope = Some(Box::new(SpliceScopeChain { scopes: Vec::new() }));
        g.push_safepoint(chain);
        g.push_safepoint(SafepointSnapshot::new(2, vec![], vec![], vec![]));
        cratonvm_types::flags::with_thread_overrides(&[(KEY, None)], || {
            assert!(!promote_spliced_enabled());
            assert_eq!(promotion_header_frame(&g, 40), None);
            assert_eq!(promotion_header_frame(&g, 2), Some(1), "a flat frame as before");
        });
        cratonvm_types::flags::with_thread_overrides(&[(KEY, Some("1"))], || {
            assert_eq!(promotion_header_frame(&g, 40), Some(0));
            assert_eq!(promotion_header_frame(&g, 7), None);
        });
    }
}

#[cfg(test)]
mod r21_loops_elem_effect_tests {
    use super::{elem_effect_conflicts, ElemKey};
    use crate::ir::{AccessOffset, AliasClass, Graph, IrType, MemEffect, MemKind, Op};

    /// rev-ir finding 3: a non-access node is judged by its EFFECT class. A
    /// write or read the model cannot separate from the cell (the same array,
    /// another array of the same kind, raw memory, anything) conflicts; a
    /// field, a static, a length word, nothing at all, or an element of
    /// another kind on another array does not.
    #[test]
    fn r21_loops_an_effect_that_may_touch_the_element_conflicts() {
        let mut g = Graph::empty(0);
        let a = g.add(Op::Param(0), IrType::Ref, vec![], None);
        let b = g.add(Op::Param(1), IrType::Ref, vec![], None);
        let k = g.add(Op::Const(1), IrType::Int, vec![], None);
        let j = g.add(Op::Param(2), IrType::Int, vec![], None);
        let keys = [ElemKey {
            array: a,
            index: k,
            kind: MemKind::Int,
        }];
        let elem = |array, index, elem| AliasClass::ArrayElem {
            array,
            index: AccessOffset::Dynamic(index),
            elem,
        };
        // An intrinsic-style write of the same array at any index.
        assert!(elem_effect_conflicts(&g, MemEffect::write(elem(a, j, MemKind::Int)), &keys));
        // Another `int[]` node may be the same array.
        assert!(elem_effect_conflicts(&g, MemEffect::write(elem(b, j, MemKind::Int)), &keys));
        // A reader sees the stale cell too.
        assert!(elem_effect_conflicts(&g, MemEffect::read(elem(a, k, MemKind::Int)), &keys));
        // Raw memory (a scoped / Unsafe access) and the top class.
        assert!(elem_effect_conflicts(&g, MemEffect::write(AliasClass::Raw), &keys));
        assert!(elem_effect_conflicts(&g, MemEffect::OPAQUE, &keys));
        // Disjoint by the model.
        assert!(!elem_effect_conflicts(&g, MemEffect::NONE, &keys));
        assert!(!elem_effect_conflicts(&g, MemEffect::write(elem(b, j, MemKind::Long)), &keys));
        assert!(!elem_effect_conflicts(
            &g,
            MemEffect::read(AliasClass::ArrayLength { array: a }),
            &keys
        ));
        assert!(!elem_effect_conflicts(
            &g,
            MemEffect::write(AliasClass::Static {
                class_id: 3,
                field: 0
            }),
            &keys
        ));
    }
}
