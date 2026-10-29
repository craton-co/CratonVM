//! JIT round 23 wave 8 (lane W8-L4): the FFM element inline expansion's
//! segment-field reads, read once per loop entry
//! (`CRATONVM_JIT_IR_FFM_FIELD_HOIST`).
//!
//! # Why
//!
//! `IrBuilder::try_emit_ffm_element_inline` expands `s.getAtIndex(L, k)` into
//! up to five guarded reads of the segment (`length`, `readOnly`, `scope`,
//! `base` + `offset` or `min`: `Op::Unbox` `FieldLong` / `FieldBool` /
//! `FieldRef`), the bounds and alignment guards and one scoped access. Each
//! read tests the receiver for null, its exact class and the layout epoch,
//! and each sits on the memory chain, so it is an opaque node to every loop
//! pass: LICM sees a hard barrier and a write, the static hoist refuses the
//! loop (the layout `getstatic` stays in it), and a loop of 64 accesses pays
//! all of it 64 times (`r23-w2-memorysegment-heap-accessors-10000x`, wave 7:
//! about 110 instructions per access, against HotSpot's hoisted fields).
//!
//! # What
//!
//! For each loop (outer loops first), every expansion read
//! ([`Graph::ffm_final_field_reads`]) whose receiver is defined outside the
//! loop and invariant in it is replaced by ONE fresh read per
//! `(receiver, field, class)` in the loop's pre-header:
//!
//! * control: the loop-entry predecessor; memory: the loop's entry state
//!   (after every pre-loop write);
//! * bytecode pc: the loop HEADER's, and its frame the loop-entry frame (the
//!   header snapshot with every header φ at its entry value,
//!   [`super::loop_entry_frame`]) -- the length predicate's discipline for its
//!   pre-header `ArrayLength`;
//! * the in-body read's memory consumers take its own incoming token (the read
//!   wrote nothing; the chain closes over it), its value users the new read,
//!   and it is killed. A `byte[]` base mark (`Graph::byte_array_nodes`) moves
//!   with it.
//!
//! # Why it is sound
//!
//! * **Value.** Every field the expansion reads is `final` in JDK 25
//!   (`AbstractMemorySegmentImpl.length` / `readOnly` / `scope`,
//!   `HeapMemorySegmentImpl.offset` / `base`, `NativeMemorySegmentImpl.min`),
//!   of a class in `jdk.internal.foreign`, which `java.base` does not open:
//!   no reflective write can reach it, and HotSpot's C2 trusts these finals
//!   as constants (`ciField`: `jdk/internal/foreign` is a trusted package).
//!   The receiver is one SSA value defined before the loop, so it was fully
//!   constructed before the loop ran; no store in the loop -- the scoped
//!   access's raw put included -- can change what the read answers. The scope
//!   is read once, but the session's STATE is still read by the scoped access
//!   on every iteration, so a closing arena is still seen.
//! * **Deopts.** The read's own tests (null, exact class, layout epoch, the
//!   reference read's `JIT_READ_BOUNDS` liveness) now resume the interpreter
//!   at the loop header with the state the loop is entered with. Nothing of
//!   the loop has run, so the interpreter runs the loop itself: zero trips if
//!   the body would not have run, the JDK accessor's own exception at its own
//!   bci if it would. The fresh read has a higher id than every node already
//!   in the pre-header and takes the loop-entry memory, so it runs after the
//!   pre-loop code (`ir_schedule` keeps impure nodes in id order).
//! * **Speculation.** A receiver that is null or of another class at a loop
//!   entry deopts there although the body might not have read it. Such a deopt
//!   is recorded at the header bci, which withdraws the hoist for the next
//!   compile ([`crate::deopt::predicate_withdrawn`], asked as the class
//!   predicate's kind: any non-predicate speculation at the header withdraws
//!   it). A receiver the body compares by identity (`seg == null`, `a == b`)
//!   is left in the loop, as `class_predicate` leaves one.
//! * **OSR.** Not on the optimizing OSR route or an OSR-door graph (their
//!   entries skip the pre-header). A method-entry compile's OSR entry stub at
//!   the header sees the hoisted value live into the loop and cannot produce
//!   it, so that one entry is refused, as for an LICM-hoisted load.
//!
//! # Where
//!
//! Before the loop rounds (just ahead of `loop_static_hoist`), so the static
//! hoist then finds the loop free of opaque reads and moves the layout
//! `getstatic` out too, and LICM and the unroller see the bounds arithmetic
//! (`length - width`, `offset + k*w`) on invariant inputs.

use super::{
    build_users, is_loop_invariant, is_memory_token_slot, loop_body_with, loop_entry_frame,
    loop_entry_memory, loop_headers, FxHashSet,
};
use crate::ir::{Graph, IrType, NodeId, Op, UnboxOp, NO_NODE};

/// `CRATONVM_JIT_IR_FFM_FIELD_HOIST` (JIT round 23 wave 8, lane W8-L4) --
/// perf, **default ON**, `=0` is the kill switch. Read once per compile, no
/// `static`.
pub(super) fn ffm_field_hoist_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_FFM_FIELD_HOIST")
}

/// Is `op` one of the guarded field reads the expansion emits?
fn is_segment_field_read(op: UnboxOp) -> bool {
    matches!(
        op,
        UnboxOp::FieldLong(_) | UnboxOp::FieldBool(_) | UnboxOp::FieldRef(_)
    )
}

/// Is `id` a pre-header read this pass planted: a filed segment-field read
/// whose own frame is a flat snapshot at its own pc, and whose control enters
/// a loop header (`Region` / `Merge`) at that pc? Such a read raises nothing:
/// any failure deopts to that loop's entry, where the interpreter runs the
/// loop from its first bytecode. `ir_optimize::preheader_may_trap` exempts it
/// (a node hoisted there with a first-iteration frame may run before or after
/// it; either failure resumes at or before the other's bytecode with the
/// loop's entry values).
pub(super) fn resumes_at_its_loop_entry(graph: &Graph, id: NodeId) -> bool {
    // JIT round 23 close: a session pre-check this pass planted cannot trap
    // at all (`UnboxOp::SessionPrecheck` answers instead of deopting).
    if graph.ffm_session_prechecks.contains(&id) {
        return true;
    }
    let read = graph.ffm_final_field_reads.contains(&id);
    // JIT r23 wave 9 (lane L4): and a heap-session guard it planted beside
    // them (`plant_heap_session_guards`), which also only deopts there.
    let guard = !read && graph.ffm_session_guards.contains(&id);
    if !read && !guard {
        return false;
    }
    let Some(node) = graph.node_opt(id) else {
        return false;
    };
    let shape_ok = if read {
        matches!(node.op, Op::Unbox { op, .. } if is_segment_field_read(op))
    } else {
        matches!(node.op, Op::Guard { .. })
    };
    if !shape_ok {
        return false;
    }
    let Some(sp) = node
        .frame_snapshot
        .and_then(|si| graph.safepoints.get(si as usize))
    else {
        return false;
    };
    if sp.is_splice_state() || node.bytecode_pc != Some(sp.bci) {
        return false;
    }
    let Some(ctrl) = node.input_opt(0) else {
        return false;
    };
    graph.users_of(ctrl).into_iter().any(|u| {
        graph.node_opt(u).is_some_and(|un| {
            matches!(un.op, Op::Region | Op::Merge) && un.bytecode_pc == Some(sp.bci)
        })
    })
}

/// `CRATONVM_JIT_IR_FFM_HEAP_SESSION_HOIST` (JIT round 23 wave 9, lane L4) --
/// perf, **default ON** since the round-23 in-session triage (R23-TRI-FLIP;
/// `=0` kills). n23-w13m, 5 pinned reps in rotated order, median on vs off:
/// `HgZero normal` n=4096 5814 vs 6540 ns/call (every rep faster; the `plain`
/// control 4674 vs 4651), `R23W4SegMono` heapput 114 vs 140, heapget 131 vs
/// 147 ns per 64 accesses. It was opt-in after being mixed on two builds (`R23W4SegMono`, P-cores, ns per
/// 64 accesses, on vs off): n23-w9a heapget 143/159/144 vs 167/147/150, heapput
/// 119/129/119 vs 147/120/128; n23-w9b heapget 112/141/126 vs 130/140/112,
/// heapput 101/116/147 vs 102/103/91, nativeget 264/292/280 vs 242/282/218.
/// Read once per loop, no `static`. See [`plant_heap_session_guards`].
pub(super) fn ffm_heap_session_hoist_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_FFM_HEAP_SESSION_HOIST")
}

/// JIT round 23 wave 9 (lane L4): for each of `reads` (this loop's fresh
/// pre-header reads) that is a heap segment's `scope`
/// ([`Graph::ffm_heap_scope_reads`]), plant `Guard{bci: header}(Cmp(Ne,
/// ExactClassIs(scope, HeapSession), 0))` on the loop's entry edge, named by
/// the loop-entry frame `si`, and file the read in
/// [`Graph::ffm_never_closing_scopes`]: `ir_lower`'s scoped access whose
/// session is that read then emits no session check (reload, null test,
/// class load, the admitted-id compare chain: 6-8 instructions per access).
///
/// Sound: `GlobalSession$HeapSession` never closes (`checkValidStateRaw`
/// never fails for it: the lowering's own never-closing pass admits it with
/// no further test), the read is one SSA value, so its class is the same at
/// every use, and the guard runs before every use: it is a newer node than
/// the read in the same pre-header (`ir_schedule` keeps impure nodes in id
/// order) and every use is inside the loop it enters. A scope of any other
/// class (a heap segment `reinterpret`ed onto an arena) fails the guard: the
/// interpreter runs the loop, and the de-spec record at the header withdraws
/// the hoist next compile.
///
/// The condition is wrapped in a `Cmp(Ne, .., 0)` on purpose: a bare
/// pre-header `Guard(ExactClassIs(..))` at a header bci is the shape the class
/// predicate and the `charAt` trap read as their own licence
/// (`class_predicate::class_guard_hoisted_headers`,
/// `charat_trap::seq_charat_traps_on_miss`). Returns how many it planted.
fn plant_heap_session_guards(
    graph: &mut Graph,
    reads: Vec<NodeId>,
    entry_pred: NodeId,
    header_bci: usize,
    si: u32,
) -> usize {
    let class_id = graph.ffm_heap_session_class;
    if class_id == 0 || !ffm_heap_session_hoist_enabled() {
        return 0;
    }
    let mut planted = 0usize;
    for read in reads {
        if !graph.ffm_heap_scope_reads.contains(&read) || graph.ffm_never_closing_scopes.contains(&read)
        {
            continue;
        }
        // No pc on the test: only the guard naming the frame anchors the
        // header's bci in the pre-header.
        let is = graph.add(Op::ExactClassIs { class_id }, IrType::Int, vec![read], None);
        let zero = graph.add(Op::Const(0), IrType::Int, vec![], None);
        let cond = graph.add(Op::Cmp(crate::ir::CmpOp::Ne), IrType::Int, vec![is, zero], None);
        let guard = graph.add(
            Op::Guard { bci: header_bci },
            IrType::Void,
            vec![entry_pred, cond],
            Some(header_bci),
        );
        if !graph.set_node_frame_snapshot(guard, si) {
            graph.kill(guard);
            graph.kill(cond);
            graph.kill(is);
            continue;
        }
        graph.ffm_session_guards.insert(guard);
        graph.ffm_never_closing_scopes.insert(read);
        planted += 1;
    }
    planted
}

/// `CRATONVM_JIT_IR_FFM_SESSION_PRECHECK` (JIT round 23 close, the native
/// rows of `r23-w2-memorysegment-heap-accessors-*`) -- perf, **default ON**,
/// `=0` kills. Read once per loop, no `static`. See
/// [`plant_session_prechecks`].
pub(super) fn ffm_session_precheck_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_FFM_SESSION_PRECHECK")
}

/// Can nothing in `body` run Java on this thread (and so close a confined
/// arena this thread owns)? An allowlist: arithmetic, constants, branches
/// and header reads ([`crate::ir_lower::op_cannot_deopt`]), guards (a failed
/// one leaves the loop for the interpreter), field / element / scoped
/// accesses and the FFM reads (`Op::Unbox`), and a direct `getstatic`
/// (initialised class: no `<clinit>`). A call, an allocation, a `putstatic`,
/// a monitor, a helper-route static read -- anything else -- answers `false`.
fn body_runs_no_java(graph: &Graph, body: &FxHashSet<NodeId>) -> bool {
    body.iter().all(|&id| {
        let Some(n) = graph.node_opt(id) else {
            return true;
        };
        match n.op {
            Op::Dead | Op::Phi | Op::KeepAlive => true,
            Op::Guard { .. }
            | Op::Load(_)
            | Op::Store(_)
            | Op::ArrayLoad(_)
            | Op::ArrayStore(_)
            | Op::ArrayLength
            | Op::Unbox { .. } => !n.volatile_access,
            Op::LoadStatic {
                class_id,
                field_index,
                is_volatile: false,
                ..
            } => super::static_read_is_direct(graph, class_id, field_index),
            Op::Return | Op::Throw => false,
            ref op => op.is_control() || crate::ir_lower::op_cannot_deopt(op),
        }
    })
}

/// JIT round 23 close (`CRATONVM_JIT_IR_FFM_SESSION_PRECHECK`): for each of
/// `reads` (this loop's fresh pre-header reads) that is a NATIVE segment's
/// `scope` ([`Graph::ffm_native_scope_reads`]), when the loop runs no Java
/// ([`body_runs_no_java`]), plant `p = Unbox{SessionPrecheck}(scope)` in the
/// pre-header and give every scoped access of the loop that names the scope
/// as its session `p` instead. Returns how many it planted.
///
/// `p` is `null` exactly when the session passed at the loop entry: a
/// never-closing class, or a confined session (the JDK's `ConfinedSession`,
/// the model's owned session) whose owner is this thread and whose state
/// reads open -- the rows `ir_lower::lower_scoped_raw` itself admits. It is
/// the session otherwise, and the access then runs its full check as before
/// (a shared arena, a wrong thread, a closed arena all keep today's
/// per-access behaviour).
///
/// Sound: a never-closing session never closes, and a confined one closes
/// only through `justClose()` run by its owner, which is this thread; a loop
/// that runs no Java on this thread cannot call it, so the answer at the
/// entry holds on every iteration. A guard that fails leaves for the
/// interpreter, which re-runs the JDK's own check on every access, and the
/// loop is re-entered only through this pre-header (the OSR entry stub at the
/// header cannot produce the hoisted values and is refused, as for every read
/// this pass moves). `p` itself cannot trap: it answers instead.
///
/// Returns the pre-checks it planted (JIT round 24 B2 caches the loop's arena
/// resolutions behind them, `super::ffm_arena_cache`).
fn plant_session_prechecks(
    graph: &mut Graph,
    reads: Vec<NodeId>,
    entry_pred: NodeId,
    header_bci: usize,
    body: &FxHashSet<NodeId>,
) -> Vec<NodeId> {
    if !ffm_session_precheck_enabled() {
        return Vec::new();
    }
    let native: Vec<NodeId> = reads
        .into_iter()
        .filter(|r| graph.ffm_native_scope_reads.contains(r))
        .collect();
    if native.is_empty() || !body_runs_no_java(graph, body) {
        return Vec::new();
    }
    let mut planted: Vec<NodeId> = Vec::new();
    for read in native {
        let Some(mem) = graph.node_opt(read).and_then(|n| n.input_opt(1)) else {
            continue;
        };
        // The loop's scoped accesses whose session is this read.
        let sites: Vec<NodeId> = graph
            .users_of(read)
            .into_iter()
            .filter(|&u| {
                body.contains(&u)
                    && graph.node_opt(u).is_some_and(|un| {
                        matches!(un.op, Op::Unbox { op, .. } if op.is_scoped())
                            && un.input_opt(3) == Some(read)
                    })
            })
            .collect();
        if sites.is_empty() {
            continue;
        }
        let p = graph.add(
            Op::Unbox {
                op: UnboxOp::SessionPrecheck,
                class_id: 0,
            },
            IrType::Ref,
            vec![entry_pred, mem, read],
            Some(header_bci),
        );
        for u in sites {
            graph.set_input(u, 3, p);
        }
        graph.ffm_session_prechecks.insert(p);
        planted.push(p);
    }
    planted
}

/// The receivers `body` compares by identity in a branch (`If(Cmp(r, x))`).
fn compared_receivers(graph: &Graph, body: &FxHashSet<NodeId>) -> FxHashSet<NodeId> {
    let mut out = FxHashSet::default();
    for &id in body {
        let Some(n) = graph.node_opt(id) else {
            continue;
        };
        if n.op != Op::If {
            continue;
        }
        let Some(cn) = n.input_opt(1).and_then(|c| graph.node_opt(c)) else {
            continue;
        };
        if !matches!(cn.op, Op::Cmp(_)) {
            continue;
        }
        for &x in cn.inputs.iter() {
            if graph.node_opt(x).is_some_and(|xn| xn.ty == IrType::Ref) {
                out.insert(x);
            }
        }
    }
    out
}

/// The expansion reads of `body` the hoist may move, ascending: a filed
/// segment-field read in the full `[ctrl, mem, receiver]` form, whose
/// receiver is a reference defined outside the loop, invariant in it, not an
/// allocation of this compile and not compared by identity in the body.
fn candidates(graph: &Graph, region: NodeId, body: &FxHashSet<NodeId>) -> Vec<NodeId> {
    let compared = compared_receivers(graph, body);
    let mut out: Vec<NodeId> = body
        .iter()
        .copied()
        .filter(|&id| {
            if !graph.ffm_final_field_reads.contains(&id) {
                return false;
            }
            let Some(n) = graph.node_opt(id) else {
                return false;
            };
            let Op::Unbox { op, .. } = n.op else {
                return false;
            };
            if !is_segment_field_read(op) || n.inputs.len() != 3 {
                return false;
            }
            let recv = n.inputs[2];
            let Some(rn) = graph.node_opt(recv) else {
                return false;
            };
            rn.ty == IrType::Ref
                && !matches!(
                    rn.op,
                    Op::New { .. }
                        | Op::NewArray { .. }
                        | Op::MultiNewArray { .. }
                        | Op::NewCp { .. }
                        | Op::Dead
                )
                && !crate::ir::is_null_const(rn)
                && !body.contains(&recv)
                && !compared.contains(&recv)
                && is_loop_invariant(graph, recv, region, body)
        })
        .collect();
    out.sort_unstable();
    out
}

/// Move the expansion's invariant segment-field reads of every loop to its
/// pre-header (see the module notes), unless `despec` withdrew the hoist at a
/// loop's header. Returns how many in-body reads were replaced.
pub(super) fn hoist_ffm_field_reads(
    graph: &mut Graph,
    despec: &crate::deopt::DespecRegistry,
    method_key: &str,
) -> usize {
    if graph.ffm_final_field_reads.is_empty() || graph.safepoints.is_empty() {
        return 0;
    }
    let dbg = super::loop_pass_report_enabled();
    // Outer loops first: a read moved to an outer pre-header leaves every
    // inner loop too.
    let sized = {
        let g: &Graph = graph;
        let users = build_users(g);
        let mut sized: Vec<(usize, (NodeId, NodeId, Vec<NodeId>))> = loop_headers(g)
            .into_iter()
            .map(|l| (loop_body_with(g, &users, l.0, l.1, &l.2).len(), l))
            .collect();
        sized.sort_by(|a, b| b.0.cmp(&a.0).then(a.1 .0.cmp(&b.1 .0)));
        sized
    };
    let mut replaced = 0usize;
    for (_, (region, entry_pred, back_ctrls)) in sized {
        if entry_pred == NO_NODE
            || !matches!(
                graph.node_opt(region).map(|n| &n.op),
                Some(Op::Region | Op::Merge)
            )
        {
            continue;
        }
        // Rebuilt per loop: the previous loop's rewiring changed use edges.
        let users = build_users(graph);
        let body = loop_body_with(graph, &users, region, entry_pred, &back_ctrls);
        let reads = candidates(graph, region, &body);
        if reads.is_empty() {
            continue;
        }
        let Some(header_bci) = graph.node_opt(region).and_then(|r| r.bytecode_pc) else {
            continue;
        };
        let Ok(bci32) = u32::try_from(header_bci) else {
            continue;
        };
        if crate::deopt::predicate_withdrawn(
            despec,
            method_key,
            bci32,
            crate::deopt::PredicateKind::Class,
        ) {
            if dbg {
                eprintln!(
                    "[ir-licm] ffm field hoist: {method_key} header {region} bci {header_bci} \
                     de-specialised, {} read(s) left in the loop",
                    reads.len()
                );
            }
            continue;
        }
        let Some(entry_slot) = graph
            .node_opt(region)
            .and_then(|r| r.inputs.iter().position(|&p| p == entry_pred))
            .map(|p| p + 1)
        else {
            continue;
        };
        let Some((frame_bci, frame)) = loop_entry_frame(graph, region, entry_slot, &body) else {
            if dbg {
                eprintln!(
                    "[ir-licm] ffm field hoist: {method_key} header {region} bci {header_bci} \
                     has no loop-entry frame"
                );
            }
            continue;
        };
        let entry_mem = loop_entry_memory(graph, region, entry_pred);
        graph.push_safepoint(frame);
        // Cast: a snapshot index; `push_safepoint` just grew the list.
        let si = (graph.safepoints.len() - 1) as u32;
        // `((receiver, op, class), fresh read)`.
        let mut planted: Vec<((NodeId, UnboxOp, u32), NodeId)> = Vec::new();
        let mut here = 0usize;
        for read in reads {
            let Some(n) = graph.node_opt(read) else {
                continue;
            };
            let Op::Unbox { op, class_id } = n.op else {
                continue;
            };
            let (own_mem, recv, ty) = (n.inputs[1], n.inputs[2], n.ty);
            // The loop's entry state: after every pre-loop write. With no
            // memory φ at the header the read's own token, when defined
            // outside the loop, is that state.
            let new_mem = match entry_mem {
                Some(m) => m,
                None if !body.contains(&own_mem) => own_mem,
                None => continue,
            };
            let key = (recv, op, class_id);
            let new = match planted.iter().find(|(k, _)| *k == key) {
                Some(&(_, new)) => new,
                None => {
                    let new = graph.add(
                        Op::Unbox { op, class_id },
                        ty,
                        vec![entry_pred, new_mem, recv],
                        Some(frame_bci),
                    );
                    if !graph.set_node_frame_snapshot(new, si) {
                        graph.kill(new);
                        continue;
                    }
                    graph.ffm_final_field_reads.insert(new);
                    planted.push((key, new));
                    new
                }
            };
            // The in-body read's memory consumers take its own incoming
            // token; every other use is a use of its value.
            let mut token_uses: Vec<(NodeId, usize)> = Vec::new();
            for u in graph.users_of(read) {
                let Some(un) = graph.node_opt(u) else {
                    continue;
                };
                if un.op == Op::Dead {
                    continue;
                }
                for (slot, &inp) in un.inputs.iter().enumerate() {
                    if inp == read && is_memory_token_slot(un, slot) {
                        token_uses.push((u, slot));
                    }
                }
            }
            for (u, slot) in token_uses {
                graph.set_input(u, slot, own_mem);
            }
            if graph.byte_array_nodes.remove(&read) {
                graph.byte_array_nodes.insert(new);
            }
            // JIT r23 wave 9 (lane L4): the heap-scope mark moves too.
            if graph.ffm_heap_scope_reads.remove(&read) {
                graph.ffm_heap_scope_reads.insert(new);
            }
            // JIT round 23 close: and the native-scope mark.
            if graph.ffm_native_scope_reads.remove(&read) {
                graph.ffm_native_scope_reads.insert(new);
            }
            graph.replace_all_uses(read, new);
            graph.ffm_final_field_reads.remove(&read);
            graph.kill(read);
            here += 1;
        }
        // JIT r23 wave 9 (lane L4, `CRATONVM_JIT_IR_FFM_HEAP_SESSION_HOIST`):
        // a heap segment's scope read now in the pre-header is proven a
        // `HeapSession` once, at the loop entry, for every access of the loop.
        let session_guards = plant_heap_session_guards(
            graph,
            planted.iter().map(|&(_, new)| new).collect(),
            entry_pred,
            frame_bci,
            si,
        );
        if dbg && session_guards > 0 {
            eprintln!(
                "[ir-licm] ffm heap session guard: {method_key} header {region} bci {header_bci}: \
                 {session_guards} scope(s) proven never-closing"
            );
        }
        // JIT round 23 close (`CRATONVM_JIT_IR_FFM_SESSION_PRECHECK`): a
        // native segment's scope is checked once, at the loop entry.
        let prechecks = plant_session_prechecks(
            graph,
            planted.iter().map(|&(_, new)| new).collect(),
            entry_pred,
            frame_bci,
            &body,
        );
        if dbg && !prechecks.is_empty() {
            eprintln!(
                "[ir-licm] ffm session precheck: {method_key} header {region} bci {header_bci}: \
                 {} native scope(s) checked at the loop entry",
                prechecks.len()
            );
        }
        // JIT round 24 B2 (`CRATONVM_JIT_IR_FFM_ARENA_CACHE`): behind those
        // pre-checks, each native access's arena resolution, once.
        let arena_cached = super::ffm_arena_cache::plant_arena_caches(
            graph,
            region,
            entry_pred,
            &back_ctrls,
            &body,
            &prechecks,
            frame_bci,
        );
        if dbg && arena_cached > 0 {
            eprintln!(
                "[ir-licm] ffm arena cache: {method_key} header {region} bci {header_bci}: \
                 {arena_cached} native access(es) resolved at the loop entry"
            );
        }
        if planted.is_empty() {
            let keep: Vec<bool> = (0..graph.safepoints.len())
                .map(|i| i != si as usize)
                .collect();
            let _ = graph.retain_safepoints(&keep);
        }
        replaced += here;
        if dbg {
            eprintln!(
                "[ir-licm] ffm field hoist: {method_key} header {region} bci {header_bci}: \
                 {here} read(s) -> {} pre-header read(s)",
                planted.len()
            );
        }
    }
    replaced
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{CmpOp, SafepointSnapshot};

    const SWITCH: &str = "CRATONVM_JIT_IR_FFM_FIELD_HOIST";
    const SEG_CLASS: u32 = 77;

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Recv {
        /// The segment is a parameter (invariant).
        Param,
        /// The segment is a header φ (varies per iteration).
        HeaderPhi,
        /// The body branches on `seg == null`.
        Compared,
    }

    struct Fx {
        g: Graph,
        region: NodeId,
        entry: NodeId,
        mem0: NodeId,
        mphi: NodeId,
        seg: NodeId,
        zero: NodeId,
        len: NodeId,
        len2: NodeId,
        scope: NodeId,
        base: NodeId,
        get: NodeId,
        last: NodeId,
    }

    /// `for (k = 0; k < n; k++) h += seg.getAtIndex(L, k)` after the inline
    /// expansion, reduced: two `length` reads (two sites), `scope`, `base`, a
    /// bounds guard on `length - 4` and the scoped access, all threaded on the
    /// loop's memory φ; the header snapshot at bci 5 names `[seg, n, k]`.
    fn segment_loop(recv: Recv) -> Fx {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let p0 = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let n = g.add(Op::Param(1), IrType::Int, vec![start], None);
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let four = g.add(Op::Const(4), IrType::Long, vec![], None);
        let addr = g.add(Op::Const(16), IrType::Long, vec![], None);
        let le = g.add(Op::Const(0), IrType::Int, vec![], None);
        let region = g.add(Op::Merge, IrType::Control, vec![entry], Some(5));
        let k = g.add(Op::Phi, IrType::Int, vec![region, zero], None);
        let mphi = g.add(Op::Phi, IrType::Memory, vec![region, mem0], None);
        let seg = if recv == Recv::HeaderPhi {
            g.add(Op::Phi, IrType::Ref, vec![region, p0], None)
        } else {
            p0
        };
        let ge = g.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![k, n], None);
        let head = g.add(Op::If, IrType::Control, vec![region, ge], Some(8));
        let exit = g.add(Op::Proj(0), IrType::Control, vec![head], None);
        let mut ctrl = g.add(Op::Proj(1), IrType::Control, vec![head], None);
        if recv == Recv::Compared {
            let null = g.add(Op::Const(0), IrType::Ref, vec![], None);
            let eq = g.add(Op::Cmp(CmpOp::Eq), IrType::Int, vec![seg, null], None);
            let br = g.add(Op::If, IrType::Control, vec![ctrl, eq], Some(10));
            let t = g.add(Op::Proj(0), IrType::Control, vec![br], Some(10));
            let f = g.add(Op::Proj(1), IrType::Control, vec![br], Some(10));
            ctrl = g.add(Op::Merge, IrType::Control, vec![t, f], Some(11));
        }
        let read = |g: &mut Graph, op: UnboxOp, mem: NodeId| {
            g.add(
                Op::Unbox {
                    op,
                    class_id: SEG_CLASS,
                },
                op.result_type(),
                vec![ctrl, mem, seg],
                Some(12),
            )
        };
        let len = read(&mut g, UnboxOp::FieldLong(0), mphi);
        let last = g.add(Op::Sub, IrType::Long, vec![len, four], Some(12));
        let ok = g.add(Op::Cmp(CmpOp::Le), IrType::Int, vec![addr, last], Some(12));
        g.add(Op::Guard { bci: 12 }, IrType::Void, vec![ctrl, ok], Some(12));
        let scope = read(&mut g, UnboxOp::FieldRef(2), len);
        let base = read(&mut g, UnboxOp::FieldRef(4), scope);
        let get = g.add(
            Op::Unbox {
                op: UnboxOp::ScopedGet {
                    width: 4,
                    signed: true,
                    ordered: true,
                },
                class_id: 0,
            },
            IrType::Int,
            vec![ctrl, base, seg, scope, base, addr, le],
            Some(12),
        );
        // A second site's `length` read of the same segment.
        let len2 = read(&mut g, UnboxOp::FieldLong(0), get);
        g.add(Op::KeepAlive, IrType::Void, vec![ctrl, get], Some(14));
        g.add(Op::KeepAlive, IrType::Void, vec![ctrl, len2], Some(14));
        let next = g.add(Op::Add, IrType::Int, vec![k, one], None);
        g.nodes[region as usize].inputs.push(ctrl);
        g.nodes[k as usize].inputs.push(next);
        g.nodes[mphi as usize].inputs.push(len2);
        if recv == Recv::HeaderPhi {
            let other = g.add(Op::Param(2), IrType::Ref, vec![start], None);
            g.nodes[seg as usize].inputs.push(other);
        }
        g.exit = g.add(Op::Return, IrType::Void, vec![exit], Some(22));
        g.push_safepoint(SafepointSnapshot::new(5, vec![seg, n, k], vec![], vec![]));
        for r in [len, scope, base, len2] {
            g.ffm_final_field_reads.insert(r);
        }
        Fx {
            g,
            region,
            entry,
            mem0,
            mphi,
            seg,
            zero,
            len,
            len2,
            scope,
            base,
            get,
            last,
        }
    }

    fn run(g: &mut Graph, despec: &crate::deopt::DespecRegistry) -> usize {
        // The session hoist is default ON (R23-TRI-FLIP); these tests pin it on.
        cratonvm_types::flags::with_thread_overrides(
            &[(SWITCH, None), ("CRATONVM_JIT_IR_FFM_HEAP_SESSION_HOIST", Some("1"))],
            || hoist_ffm_field_reads(g, despec, "T.m:()V"),
        )
    }

    #[test]
    fn the_invariant_segment_reads_move_to_the_pre_header_with_the_entry_frame() {
        let mut fx = segment_loop(Recv::Param);
        fx.g.byte_array_nodes.insert(fx.base);
        let despec = crate::deopt::DespecRegistry::new();
        assert_eq!(run(&mut fx.g, &despec), 4);
        for old in [fx.len, fx.len2, fx.scope, fx.base] {
            assert_eq!(fx.g.nodes[old as usize].op, Op::Dead, "n{old} replaced");
            assert!(!fx.g.ffm_final_field_reads.contains(&old));
        }
        // One fresh read per (receiver, field): the two length reads share.
        let fresh: Vec<NodeId> = fx
            .g
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| {
                matches!(n.op, Op::Unbox { op, .. } if is_segment_field_read(op))
            })
            .map(|(i, _)| i as NodeId)
            .collect();
        assert_eq!(fresh.len(), 3, "length, scope, base");
        for &f in &fresh {
            let n = &fx.g.nodes[f as usize];
            assert_eq!(n.inputs.as_slice(), &[fx.entry, fx.mem0, fx.seg][..]);
            assert_eq!(n.bytecode_pc, Some(5), "deopts at the loop header");
            let sp = &fx.g.safepoints[n.frame_snapshot.expect("names a frame") as usize];
            assert_eq!(sp.bci, 5);
            assert_eq!(sp.locals[2], fx.zero, "the header phi at its entry value");
            assert!(fx.g.ffm_final_field_reads.contains(&f));
        }
        let get = &fx.g.nodes[fx.get as usize];
        assert_eq!(get.inputs[1], fx.mphi, "the chain closes over the moved reads");
        let new_scope = get.inputs[3];
        let new_base = get.inputs[4];
        assert!(fresh.contains(&new_scope) && fresh.contains(&new_base));
        assert!(fx.g.byte_array_nodes.contains(&new_base), "the byte[] mark moves");
        let new_len = fx.g.nodes[fx.last as usize].inputs[0];
        assert!(fresh.contains(&new_len));
        // The back edge of the memory φ is the scoped access now.
        assert_eq!(fx.g.nodes[fx.mphi as usize].inputs[2], fx.get);
        // A second run finds nothing left in the loop.
        assert_eq!(run(&mut fx.g, &despec), 0);
        // The planted reads raise nothing to the pre-header trap test; the
        // in-body originals (dead now) and a body node never qualified.
        for &f in &fresh {
            assert!(resumes_at_its_loop_entry(&fx.g, f));
        }
        assert!(!resumes_at_its_loop_entry(&fx.g, fx.len));
        assert!(!resumes_at_its_loop_entry(&fx.g, fx.get));
        assert!(!crate::ir_optimize::preheader_may_trap(&fx.g, fx.entry));
    }

    /// r23 w9 (lane L4): a heap segment's scope, once in the pre-header, is
    /// proven a `HeapSession` by one loop-entry guard, and filed as
    /// never-closing for the lowering; the guard is no pre-header trap.
    #[test]
    fn a_heap_scope_gets_one_loop_entry_session_guard() {
        const HEAP_SESSION: u32 = 55;
        let prep = |class: u32| {
            let mut fx = segment_loop(Recv::Param);
            fx.g.ffm_heap_session_class = class;
            fx.g.ffm_heap_scope_reads.insert(fx.scope);
            fx
        };
        let despec = crate::deopt::DespecRegistry::new();
        let mut fx = prep(HEAP_SESSION);
        assert_eq!(run(&mut fx.g, &despec), 4);
        let new_scope = fx.g.nodes[fx.get as usize].inputs[3];
        assert!(fx.g.ffm_heap_scope_reads.contains(&new_scope), "the mark moved");
        assert!(fx.g.ffm_never_closing_scopes.contains(&new_scope));
        let guards: Vec<NodeId> = fx.g.ffm_session_guards.iter().copied().collect();
        let [guard] = guards.as_slice() else {
            panic!("one session guard: {guards:?}");
        };
        let gn = &fx.g.nodes[*guard as usize];
        assert_eq!(gn.op, Op::Guard { bci: 5 });
        assert_eq!(gn.inputs[0], fx.entry, "on the loop's entry edge");
        assert!(*guard > new_scope, "after the read it proves");
        let sp = &fx.g.safepoints[gn.frame_snapshot.expect("the loop-entry frame") as usize];
        assert_eq!(sp.bci, 5);
        let cond = &fx.g.nodes[gn.inputs[1] as usize];
        assert_eq!(cond.op, Op::Cmp(CmpOp::Ne), "not the bare class-guard licence shape");
        let is = &fx.g.nodes[cond.inputs[0] as usize];
        assert_eq!(is.op, Op::ExactClassIs { class_id: HEAP_SESSION });
        assert_eq!(is.inputs[0], new_scope);
        assert!(resumes_at_its_loop_entry(&fx.g, *guard));
        assert!(!crate::ir_optimize::preheader_may_trap(&fx.g, fx.entry));

        // No class known, or the switch off: no guard, nothing filed.
        let mut fx = prep(0);
        assert_eq!(run(&mut fx.g, &despec), 4);
        assert!(fx.g.ffm_session_guards.is_empty() && fx.g.ffm_never_closing_scopes.is_empty());
        let mut fx = prep(HEAP_SESSION);
        let n = cratonvm_types::flags::with_thread_overrides(
            &[(SWITCH, None), ("CRATONVM_JIT_IR_FFM_HEAP_SESSION_HOIST", Some("0"))],
            || hoist_ffm_field_reads(&mut fx.g, &despec, "T.m:()V"),
        );
        assert_eq!(n, 4);
        assert!(fx.g.ffm_session_guards.is_empty() && fx.g.ffm_never_closing_scopes.is_empty());
        // Default ON (R23-TRI-FLIP): unset plants the guard.
        let mut fx = prep(HEAP_SESSION);
        let n = cratonvm_types::flags::with_thread_overrides(
            &[(SWITCH, None), ("CRATONVM_JIT_IR_FFM_HEAP_SESSION_HOIST", None)],
            || hoist_ffm_field_reads(&mut fx.g, &despec, "T.m:()V"),
        );
        assert_eq!(n, 4);
        assert!(!fx.g.ffm_session_guards.is_empty());
        // A native segment's scope is never marked, so never guarded.
        let mut fx = segment_loop(Recv::Param);
        fx.g.ffm_heap_session_class = HEAP_SESSION;
        assert_eq!(run(&mut fx.g, &despec), 4);
        assert!(fx.g.ffm_session_guards.is_empty());
    }

    /// JIT round 23 close (`CRATONVM_JIT_IR_FFM_SESSION_PRECHECK`): a native
    /// segment's scope, once in the pre-header, gets one loop-entry session
    /// pre-check, which the loop's scoped access takes as its session; it is
    /// no pre-header trap. Off, or unmarked: nothing planted.
    #[test]
    fn r23_close_a_native_scope_gets_one_loop_entry_session_precheck() {
        const PRE: &str = "CRATONVM_JIT_IR_FFM_SESSION_PRECHECK";
        let prep = || {
            let mut fx = segment_loop(Recv::Param);
            fx.g.ffm_native_scope_reads.insert(fx.scope);
            fx
        };
        let despec = crate::deopt::DespecRegistry::new();
        let mut fx = prep();
        let n = cratonvm_types::flags::with_thread_overrides(&[(SWITCH, None), (PRE, None)], || {
            hoist_ffm_field_reads(&mut fx.g, &despec, "T.m:()V")
        });
        assert_eq!(n, 4);
        let pres: Vec<NodeId> = fx.g.ffm_session_prechecks.iter().copied().collect();
        let [pre] = pres.as_slice() else {
            panic!("one precheck: {pres:?}");
        };
        let pn = &fx.g.nodes[*pre as usize];
        assert_eq!(
            pn.op,
            Op::Unbox {
                op: UnboxOp::SessionPrecheck,
                class_id: 0
            }
        );
        assert_eq!(pn.ty, IrType::Ref);
        let new_scope = pn.inputs[2];
        assert!(fx.g.ffm_native_scope_reads.contains(&new_scope), "the mark moved");
        assert_eq!(pn.inputs.as_slice(), &[fx.entry, fx.mem0, new_scope][..]);
        assert_eq!(fx.g.nodes[fx.get as usize].inputs[3], *pre, "the access takes the precheck");
        assert!(*pre > new_scope, "after the read it checks");
        assert!(resumes_at_its_loop_entry(&fx.g, *pre));
        assert!(!crate::ir_optimize::preheader_may_trap(&fx.g, fx.entry));
        assert!(fx.g.ffm_never_closing_scopes.is_empty(), "nothing is filed as never closing");

        // `=0`: nothing planted, the access keeps the scope read.
        let mut fx = prep();
        cratonvm_types::flags::with_thread_overrides(&[(SWITCH, None), (PRE, Some("0"))], || {
            hoist_ffm_field_reads(&mut fx.g, &despec, "T.m:()V")
        });
        assert!(fx.g.ffm_session_prechecks.is_empty());
        let s3 = fx.g.nodes[fx.get as usize].inputs[3];
        assert!(matches!(fx.g.nodes[s3 as usize].op, Op::Unbox { op: UnboxOp::FieldRef(_), .. }));

        // A heap scope (unmarked as native) gets none.
        let mut fx = segment_loop(Recv::Param);
        assert_eq!(run(&mut fx.g, &despec), 4);
        assert!(fx.g.ffm_session_prechecks.is_empty());
    }

    /// The precheck's premise: a body that may run Java (a call) is refused;
    /// arithmetic, guards, FFM reads and a scoped access are not.
    #[test]
    fn r23_close_a_call_in_the_body_runs_java() {
        let fx = segment_loop(Recv::Param);
        let body: FxHashSet<NodeId> = (0..fx.g.nodes.len() as NodeId)
            .filter(|&i| !matches!(fx.g.nodes[i as usize].op, Op::Return | Op::Start))
            .collect();
        assert!(body_runs_no_java(&fx.g, &body));
        let mut g = fx.g;
        let call = g.add(Op::Call { info_ptr: 0 }, IrType::Int, vec![fx.entry, fx.mem0], Some(13));
        let mut with_call = body.clone();
        with_call.insert(call);
        assert!(!body_runs_no_java(&g, &with_call));
    }

    #[test]
    fn an_in_body_read_does_not_resume_at_the_loop_entry() {
        let fx = segment_loop(Recv::Param);
        assert!(!resumes_at_its_loop_entry(&fx.g, fx.len), "no frame of its own");
        assert!(!resumes_at_its_loop_entry(&fx.g, fx.region));
    }

    #[test]
    fn a_withdrawn_header_keeps_its_reads() {
        let mut fx = segment_loop(Recv::Param);
        let despec = crate::deopt::DespecRegistry::new();
        despec.insert("T.m:()V", 5, crate::deopt::SPECULATION_ID_ANY);
        let snaps = fx.g.safepoints.len();
        assert_eq!(run(&mut fx.g, &despec), 0);
        assert_ne!(fx.g.nodes[fx.len as usize].op, Op::Dead);
        assert_eq!(fx.g.safepoints.len(), snaps, "no frame left behind");
    }

    /// j27 wave 4 (lane SPLICEFFM): the premise that lets a compile whose
    /// element sites are all SPLICED set the `HeapSession` class. A loop whose
    /// header has only a chain frame (a loop spliced from a callee) gets no
    /// hoist and so no session guard: the guard exists only with a flat frame
    /// of the compiling method at its own header, whose failure the VM records
    /// at `(method key, header bci)` -- the key `predicate_withdrawn` asks.
    #[test]
    fn j27_spliceffm_a_chain_framed_header_gets_no_session_guard() {
        use crate::ir::{SpliceScopeChain, SpliceScopeFrame};
        let mut fx = segment_loop(Recv::Param);
        fx.g.ffm_heap_session_class = 55;
        fx.g.ffm_heap_scope_reads.insert(fx.scope);
        let frame = |method_key: &str, bci: u32| SpliceScopeFrame {
            method_key: method_key.into(),
            bci,
            code_len: 0,
            max_locals: 0,
            locals_len: 0,
            stack_len: 0,
        };
        for sp in fx.g.safepoints.iter_mut().filter(|s| s.bci == 5) {
            sp.splice_scope = Some(Box::new(SpliceScopeChain {
                scopes: vec![frame("p/C.put:()V", 3), frame("", 9)],
            }));
        }
        let despec = crate::deopt::DespecRegistry::new();
        let snaps = fx.g.safepoints.len();
        assert_eq!(run(&mut fx.g, &despec), 0, "nothing hoisted");
        assert!(fx.g.ffm_session_guards.is_empty() && fx.g.ffm_never_closing_scopes.is_empty());
        assert_eq!(fx.g.safepoints.len(), snaps, "no frame planted");
        assert_ne!(fx.g.nodes[fx.scope as usize].op, Op::Dead);
    }

    #[test]
    fn a_varying_or_compared_receiver_and_an_unfiled_read_stay() {
        let despec = crate::deopt::DespecRegistry::new();
        for recv in [Recv::HeaderPhi, Recv::Compared] {
            let mut fx = segment_loop(recv);
            assert_eq!(run(&mut fx.g, &despec), 0, "{recv:?}");
        }
        // A `FieldLong` the expansion did not emit (an `AtomicLong.value`
        // read): not a final segment field.
        let mut fx = segment_loop(Recv::Param);
        fx.g.ffm_final_field_reads.clear();
        assert_eq!(run(&mut fx.g, &despec), 0);
        let mut fx = segment_loop(Recv::Param);
        fx.g.ffm_final_field_reads.remove(&fx.scope);
        assert_eq!(run(&mut fx.g, &despec), 3, "only the filed reads move");
        assert_ne!(fx.g.nodes[fx.scope as usize].op, Op::Dead);
    }

    #[test]
    fn a_graph_with_no_frames_is_left_alone() {
        let mut fx = segment_loop(Recv::Param);
        fx.g.safepoints.clear();
        let despec = crate::deopt::DespecRegistry::new();
        assert_eq!(run(&mut fx.g, &despec), 0);
    }

    #[test]
    fn the_hoist_has_a_kill_switch() {
        let read = |v: Option<&str>| {
            cratonvm_types::flags::with_thread_overrides(&[(SWITCH, v)], ffm_field_hoist_enabled)
        };
        assert!(read(None), "default ON");
        assert!(!read(Some("0")));
    }
}
