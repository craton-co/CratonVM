// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! nio21 wave 4 (lane L5): loop-entry class predicates for a loop-invariant
//! receiver (`CRATONVM_JIT_IR_LOOP_CLASS_PREDICATE`).
//!
//! # What
//!
//! `UTF_8$Encoder.encodeBufferLoop` (JDK 25) runs, per char,
//! `src.get()` and `dst.put(b)` on two PARAMETERS whose static types are the
//! abstract `CharBuffer` and `ByteBuffer`. The builder splices each behind a
//! profile class test whose miss edge is the virtual call
//! (`IrBuilder::begin_guarded_receiver_splice`):
//!
//! ```text
//! If(ExactClassIs(src, StringCharBuffer))
//!   Proj(0): <StringCharBuffer.get spliced>
//!   Proj(1): Call(CharBuffer.get)
//! Merge; Phi(mem); Phi(value)
//! ```
//!
//! The receiver is the same object on every iteration, so the test has the
//! same answer on every iteration; the loop still pays a class-id load and a
//! branch per site per char, and every miss arm's call joins the memory chain
//! at a merge (a memory φ that writes `Any`), which is what keeps every
//! loop-level read hoist and the field forward away from the buffers' fields.
//!
//! This is HotSpot's loop unswitching on an invariant type check, in its
//! speculative form (the form C2 uses for an invariant check it predicates):
//! for each receiver `r` defined outside the loop and tested in the body by
//! `ExactClassIs(r, C)` for ONE class `C`, a pre-header
//! `Guard(ExactClassIs(r, C))` deopts to the LOOP ENTRY with the loop-entry
//! frame (the header snapshot with every header φ at its entry value; the
//! range predicates' discipline and de-spec key, the header bci), and every
//! in-body `If` / `Guard` on that test is rewired to the constant `1`, and so
//! is every in-body `Guard(r != null)` (the receiver null check of each
//! statically bound splice on `r`, `src.hasRemaining()` among them: the
//! class guard has already proven `r` non-null). The cleanup then cuts each
//! miss arm (`prune_constant_ifs`) and strips each guard
//! (`eliminate_trivial_guards`).
//!
//! # Why it is sound
//!
//! * `ExactClassIs` is pure and total: the header class id of an object never
//!   changes, a null answers `0`, an array answers `0`. `r` is one SSA value
//!   defined outside the loop, so every in-body evaluation of
//!   `ExactClassIs(r, C)` answers what the pre-header one answered, and the
//!   pre-header guard dominates the whole body (it sits on the loop's single
//!   entry edge; `loop_headers` admits only single-entry loops whose header
//!   dominates every back edge).
//! * The guard only DEOPTS: a failing one resumes the interpreter at the
//!   loop's header with the state the loop is entered with; nothing has run
//!   between the pre-header and that point, so the interpreter runs the loop
//!   itself and dispatches every call the bytecode makes. A guard that keeps
//!   failing is withdrawn by the de-spec registry (the header bci), after which
//!   the loop keeps its in-body tests.
//! * An OSR entry at the header would skip the guard: `ir_lower`'s
//!   `emit_osr_entry_stubs` refuses an entry past a `Guard` planted at the
//!   entry block's own bci in a dominating block
//!   (`CRATONVM_JIT_IR_OSR_PREDICATE_REFUSAL`, the range predicates' rule), and
//!   this pass does not run on the optimizing OSR route or an OSR-door graph.
//!
//! # What it does not plant for
//!
//! A receiver tested against two classes in the loop (a bimorphic or
//! polymorphic site: the profile saw both, and one would fail every entry), a
//! receiver the loop compares (`r == null`, `r == s`: its identity, and so its
//! class, plausibly varies between entries), an allocation of this compile
//! (escape analysis knows its class), a receiver a plain (non-splice) class
//! `If` tests, and a loop whose header has no snapshot of its own (a spliced
//! callee's loop). Until nio21 wave 16 also a receiver with no class `If` at
//! all ("the guard-only form costs a compare the trap already pays"); a
//! trap-form receiver guard is now a candidate too
//! (`CRATONVM_JIT_IR_LOOP_CLASS_PREDICATE_GUARDS`), because leaving it in the
//! body also keeps the receiver's fields out of the field forward
//! (`class_tested_in`) and its per-iteration class test in the loop.
//!
//! # Cost when the speculation is wrong
//!
//! A receiver of another class deopts at the loop entry and runs that
//! activation's loop interpreted; after the registry's per-bci limit the
//! header is de-specialised and the next compile keeps the in-body tests.
//! The de-spec is keyed by the header bci, so it also withdraws the other
//! loop-entry predicates of that header (range, length, mask, monotone).

use super::{build_users, is_loop_invariant, loop_body_with, loop_entry_frame, loop_headers};
use crate::ir::{Graph, IrType, NodeId, Op, NO_NODE};
use rustc_hash::FxHashSet;

/// At most this many class guards per compile.
const MAX_CLASS_GUARDS: usize = 4;

/// `CRATONVM_JIT_IR_LOOP_CLASS_PREDICATE` (nio21 wave 4, lane L5) -- **default
/// ON**, `=0` is the kill switch. Read once per compile.
pub(super) fn loop_class_predicate_enabled() -> bool {
    true
}

/// What [`plant_class_predicates`] did.
#[derive(Default)]
pub(super) struct ClassPredicateOutcome {
    /// Guards planted.
    pub(super) planted: usize,
    /// The loop headers that received at least one guard.
    pub(super) headers: FxHashSet<NodeId>,
}

/// One receiver's speculation.
struct ReceiverPlan {
    receiver: NodeId,
    class_id: u32,
    /// The in-body `If`s and `Guard`s whose condition is `ExactClassIs(r, C)`.
    tests: Vec<NodeId>,
}

/// One loop's plan, decided without touching the graph.
struct LoopPlan {
    header_bci: usize,
    entry_pred: NodeId,
    receivers: Vec<ReceiverPlan>,
    frame: crate::ir::SafepointSnapshot,
}

/// Plant the guards and fold the tests the module notes describe, unless
/// `despec` withdrew a speculation at a loop's header.
pub(super) fn plant_class_predicates(
    graph: &mut Graph,
    despec: &crate::deopt::DespecRegistry,
    method_key: &str,
) -> ClassPredicateOutcome {
    let mut out = ClassPredicateOutcome::default();
    if graph.safepoints.is_empty() {
        return out;
    }
    // nio21 wave 16 (lane L3): also under `ir-bailout` / `ir-compiles`, with
    // the method named, and for every loop (not only a planted one).
    let dbg = super::loop_pass_report_enabled();
    // nio21 wave 6 (lane L5): the build's splice map, which names the bci a
    // charAt trap is charged at (`charat_trap::withheld_by_despec`).
    let splice_map = crate::ir::splice_resume_map_this_build();
    // Outer loops first: a guard in an outer pre-header covers the inner
    // loops too, and their tests are then already folded.
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
    for (_, (region, entry_pred, back_ctrls)) in sized {
        if out.planted >= MAX_CLASS_GUARDS {
            break;
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
                    "[ir-licm] class predicate: {method_key} header {region} bci {header_bci} \
                     de-specialised, not planted"
                );
            }
            continue;
        }
        // nio21 wave 5 (lane L5): a planted guard turns the loop's
        // `CharSequence.charAt` calls into traps (`charat_trap`); plant
        // nothing for such a loop once the VM has de-specialised the site a
        // call's trap is charged at (nio21 wave 6: per site, falling back to
        // "anything in this method" for an unattributable call; the next
        // compile keeps the call), or when a call has no frame its trap could
        // resume (see that module).
        if let Some(why) = super::charat_trap::class_predicate_withheld(
            graph,
            despec,
            method_key,
            region,
            entry_pred,
            &back_ctrls,
            splice_map.as_ref(),
        ) {
            if dbg {
                eprintln!(
                    "[ir-licm] class predicate: {method_key} header {region} bci {header_bci} \
                     holds a charAt call: {why}, not planted"
                );
            }
            continue;
        }
        let plan = match class_plan(graph, region, entry_pred, &back_ctrls) {
            Ok(plan) => plan,
            Err(why) => {
                if dbg {
                    eprintln!(
                        "[ir-licm] class predicate: {method_key} header {region} bci \
                         {header_bci} not planted: {why}"
                    );
                }
                continue;
            }
        };
        let (planted, folded) = install_class_plan(graph, plan, MAX_CLASS_GUARDS - out.planted);
        if planted > 0 {
            out.headers.insert(region);
            // JIT round 20 (lane strings): this header's guard passed the
            // charAt trap's screen above; the trap's licence reads it
            // (`charat_trap::seq_charat_traps_on_miss`).
            graph.seq_charat_trap_headers.insert(header_bci);
        }
        out.planted += planted;
        if dbg && planted > 0 {
            eprintln!(
                "[ir-licm] class predicate: {method_key} header {region} bci {header_bci} \
                 planted {planted}, folded {folded} in-body test(s)"
            );
        }
    }
    out
}

/// `CRATONVM_JIT_IR_LOOP_CLASS_PREDICATE_GUARDS` (nio21 wave 16, lane L3) --
/// **default ON**, `=0` restores the wave-4 rule (a receiver needs a guarded
/// splice's `If`). See [`class_plan`]'s guard-only arm. Read once per loop.
pub(super) fn guard_only_receivers_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_GUARD_HOISTED` (nio21 wave 7, lane
/// L5) -- **opt-in** (`=1`). Read once per compile. See
/// [`class_guard_hoisted_headers`]. Default OFF since the m7 host pass
/// (2026-10-10): it wins heapget (76 vs 94 ns/op) but loses directget
/// (125 vs 80, both modes), and the measurement plan's rule turns a switch
/// with a losing row off.
pub(super) fn guard_hoisted_forward_enabled() -> bool {
    false}

/// nio21 wave 7 (lane L5; item 3 of
/// `r20-buffer-loop-barriers-left-after-the-raw-class-CLOSED-20261012.md`): the
/// loops, outside `exclude`, whose pre-header already holds a loop-entry
/// class guard -- `Guard{bci: header}(ExactClassIs(r, C))` on the loop's
/// single entry control, which is the shape
/// `ir_optimize::plant_length_predicates` gives an in-body TRAP-form receiver
/// guard (`Guard(ExactClassIs(b, HeapByteBuffer))`, the default for `java.nio`
/// buffer sites under `CRATONVM_JIT_IR_BUFFER_TRAP_MISS`) when it moves it out
/// of the loop -- and whose body holds no `Op::Call`.
///
/// [`plant_class_predicates`] never sees such a loop: by the time it runs the
/// trap-form test has left the body, and it plants only for a guarded
/// splice's `If`. The field forward it then runs
/// (`loop_field_forward::forward_loop_fields`) is exactly as sound here, for
/// the two reasons `optimize_with` gives for the class-predicated loops: the
/// header-bci guard already refuses an OSR entry stub at the header
/// (`CRATONVM_JIT_IR_OSR_PREDICATE_REFUSAL`; a class guard is not the
/// re-checkable range shape), so the value φ the forward adds costs no entry
/// the body still had; and the guard is an older node than every read the
/// forward plants in the same pre-header block, so it runs first and is the
/// class proof those reads need. A loop with a call is left out: the forward
/// refuses an ordinary call, and a `CharSequence.charAt` call such a guard
/// would turn into a trap (`charat_trap::seq_charat_traps_on_miss` reads
/// the same guard shape) never passed `charat_trap::class_predicate_withheld`'s
/// frame and de-spec checks.
pub(super) fn class_guard_hoisted_headers(
    graph: &Graph,
    exclude: &FxHashSet<NodeId>,
) -> FxHashSet<NodeId> {
    let mut out = FxHashSet::default();
    let loops = loop_headers(graph);
    if loops.is_empty() {
        return out;
    }
    let mut users: Option<Vec<Vec<NodeId>>> = None;
    for (region, entry_pred, back_ctrls) in loops {
        if exclude.contains(&region) || entry_pred == NO_NODE {
            continue;
        }
        let Some(header_bci) = graph.node_opt(region).and_then(|r| r.bytecode_pc) else {
            continue;
        };
        let guarded = super::anchored_at(graph, entry_pred).into_iter().any(|g| {
            graph.node_opt(g).is_some_and(|gn| {
                gn.op == (Op::Guard { bci: header_bci })
                    && gn.input_opt(0) == Some(entry_pred)
                    && gn
                        .input_opt(1)
                        .is_some_and(|c| exact_class_test(graph, c).is_some())
            })
        });
        if !guarded {
            continue;
        }
        let u = users.get_or_insert_with(|| build_users(graph));
        let body = loop_body_with(graph, u.as_slice(), region, entry_pred, &back_ctrls);
        // nio21 wave 18 (lane L3): a call the forward reads its fields again
        // after (`loop_field_forward::post_writer_frame`, under
        // `CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_RELOAD`) no longer keeps the
        // loop out -- unless it is a `CharSequence.charAt` call, which the
        // guard would turn into a trap that never passed the class
        // predicate's frame and de-spec checks.
        let reload = super::loop_field_forward::reload_after_writer_enabled();
        let has_call = body.iter().any(|&id| {
            graph.node_opt(id).is_some_and(|n| {
                matches!(n.op, Op::Call { .. })
                    && (!reload
                        || super::charat_trap::is_seq_charat_call(n)
                        || super::loop_field_forward::post_writer_frame(graph, id).is_err())
            })
        });
        if !has_call {
            out.insert(region);
        }
    }
    out
}

/// `CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_SPLICED_EXACT` (the r23 L8 heap-buffer
/// close; perf, **default ON**, `=0` kills): the loops, outside `exclude`,
/// SPLICED from a callee whose body tests no class (no `Guard` on an
/// `ExactClassIs`) and holds no call -- a buffer kernel whose receiver the
/// planner proved exact (`nest-static-final`: a trusted `static final`
/// buffer), so it planted no class guard [`class_guard_hoisted_headers`]
/// could find. Offered to the heap-buffer forward too ([`heap_buffer_loops`]
/// filters them as it does the guard-hoisted ones).
///
/// The two reasons [`class_guard_hoisted_headers`] gives for its loops hold
/// here without the guard: a spliced header is never an OSR entry, so the
/// value φ the forward adds costs no entry; and the in-body reads the
/// forward replaces are behind no class test either, so the pre-header copy
/// needs no class proof the body read did not need.
///
/// Why: `absPut` and `heapbb` spliced into the probe's `loop` re-read the
/// buffer's `hb`, `limit`, `address`, `segment` and `bigEndian` with the full
/// field sequence per element -- their raw stores (and `heapbb`'s `position`
/// store) keep LICM's read-only hoist out, and no class guard offered the
/// forward. Read once per compile.
pub(super) fn spliced_exact_forward_enabled() -> bool {
    true
}

/// See [`spliced_exact_forward_enabled`].
pub(super) fn spliced_exact_receiver_loops(
    graph: &Graph,
    exclude: &FxHashSet<NodeId>,
) -> FxHashSet<NodeId> {
    let mut out = FxHashSet::default();
    let loops = loop_headers(graph);
    if loops.is_empty() {
        return out;
    }
    let users = build_users(graph);
    for (region, entry_pred, back_ctrls) in loops {
        if exclude.contains(&region)
            || entry_pred == NO_NODE
            || !super::header_is_spliced(graph, region)
        {
            continue;
        }
        let body = loop_body_with(graph, &users, region, entry_pred, &back_ctrls);
        let refused = body.iter().any(|&id| {
            graph.node_opt(id).is_some_and(|n| match n.op {
                Op::Call { .. } => true,
                Op::Guard { .. } => n
                    .input_opt(1)
                    .is_some_and(|c| exact_class_test(graph, c).is_some()),
                _ => false,
            })
        });
        if !refused {
            out.insert(region);
        }
    }
    out
}

/// `CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_HEAP_BUFFER` (JIT round 20 wave 19,
/// lane yBuf; MEM20-4 / LOOPOPT20-5 of `docs/feature-designs/jit-ir-optimizations.md`) --
/// **default ON**, `=0` is the kill switch. Read once per compile. With
/// [`guard_hoisted_forward_enabled`] off (its default since the m7 pass), the
/// guard-hoisted forward still runs for the loops [`heap_buffer_loops`]
/// keeps: the heap-buffer loops, where m7 measured the gain (`heapget` 79 vs
/// 93 ns/op, `Nio20RawAlias heapbb` 171 vs 220), and not the direct-buffer
/// loops, where it measured the loss (`directget` 125 vs 83). With
/// `GUARD_HOISTED=1` every guard-hoisted loop is forwarded, as before.
pub(super) fn heap_buffer_forward_enabled() -> bool {
    true
}

/// JIT round 20 wave 19 (lane yBuf, MEM20-4): of `headers`
/// ([`class_guard_hoisted_headers`]' answer), the loops whose body holds at
/// least one `ScopedMemoryAccess` fold (`Op::Unbox` with a scoped
/// [`crate::ir::UnboxOp`]: a buffer access the IR lowers raw) and in which
/// every such fold has a heap base -- an input that is not the null literal
/// (`HeapByteBuffer`'s `hb`). A fold over the null literal is a direct
/// buffer's off-heap access (`null, address + i`, resolved through the
/// arena table per access), the shape whose `directget` row the forward
/// slowed on m7 for a reason not yet attributed (LOOPOPT20-5): such a loop
/// keeps its in-loop reads unless `GUARD_HOISTED=1`.
///
/// Soundness is the forward's own (`loop_field_forward`'s module notes, and
/// [`class_guard_hoisted_headers`]' reasons for this guard shape): this only
/// narrows which loops it is offered. In particular the scoped access writes
/// [`crate::ir::AliasClass::Raw`] (`Graph::memory_effect` under
/// `CRATONVM_JIT_IR_SCOPED_RAW_ALIAS`): a primitive array element or native
/// memory, never an object field, so a PUT loop's raw stores do not stop the
/// buffer's own fields (`hb`, `offset`, `limit`, `bigEndian`, `segment`) from
/// being read once in the pre-header; a `position` store through the same
/// base is carried by a value φ. The fold's session test and bounds compares
/// stay in the body: the session operand is the fold's own input, re-tested
/// per access by the lowering, so a closing shared arena is still seen.
pub(super) fn heap_buffer_loops(graph: &Graph, headers: &FxHashSet<NodeId>) -> FxHashSet<NodeId> {
    let mut out = FxHashSet::default();
    if headers.is_empty() {
        return out;
    }
    let dbg = super::loop_pass_report_enabled();
    let users = build_users(graph);
    for (region, entry_pred, back_ctrls) in loop_headers(graph) {
        if !headers.contains(&region) {
            continue;
        }
        let body = loop_body_with(graph, &users, region, entry_pred, &back_ctrls);
        let (mut heap, mut off_heap) = (0usize, 0usize);
        for &id in &body {
            let Some(n) = graph.node_opt(id) else {
                continue;
            };
            let Op::Unbox { op, .. } = n.op else {
                continue;
            };
            if !op.is_scoped() {
                continue;
            }
            let null_base = n
                .input_opt(4)
                .and_then(|b| graph.node_opt(b))
                .map_or(true, crate::ir::is_null_const);
            if null_base {
                off_heap += 1;
            } else {
                heap += 1;
            }
        }
        let taken = heap > 0 && off_heap == 0;
        if taken {
            out.insert(region);
        }
        if dbg {
            let bci = graph.node_opt(region).and_then(|r| r.bytecode_pc);
            eprintln!(
                "[ir-licm] heap-buffer forward: header {region} bci {bci:?}: scoped heap={heap} \
                 off-heap={off_heap} -> {}",
                if taken {
                    "offered"                } else {
                    "left (GUARD_HOISTED=1 offers it)"                }
            );
        }
    }
    out
}

/// `(receiver, class)` when `cond` is `ExactClassIs(receiver, class)`.
pub(super) fn exact_class_test(graph: &Graph, cond: NodeId) -> Option<(NodeId, u32)> {
    let c = graph.node_opt(cond)?;
    let Op::ExactClassIs { class_id } = c.op else {
        return None;
    };
    let r = c.input_opt(0)?;
    (r != NO_NODE && class_id != 0).then_some((r, class_id))
}

/// `r` when `cond` is `Cmp(Ne, r, null)` (either operand order), the null
/// literal being the `Ref`-typed `Const(0)` (`IrBuilder::aconst_null`).
fn null_checked(graph: &Graph, cond: NodeId) -> Option<NodeId> {
    let c = graph.node_opt(cond)?;
    if c.op != Op::Cmp(crate::ir::CmpOp::Ne) {
        return None;
    }
    let [a, b] = c.inputs.as_slice() else {
        return None;
    };
    let is_null = |x: NodeId| {
        graph
            .node_opt(x)
            .is_some_and(|n| n.op == Op::Const(0) && n.ty == IrType::Ref)
    };
    match (is_null(*a), is_null(*b)) {
        (false, true) => Some(*a),
        (true, false) => Some(*b),
        _ => None,
    }
}

/// Is `iff`'s false edge (`Proj(1)`) the control of an `Op::Call` -- the
/// guarded splice's miss arm?
pub(super) fn miss_edge_calls(graph: &Graph, users: &[Vec<NodeId>], iff: NodeId) -> bool {
    let at = |id: NodeId| users.get(id as usize).map(Vec::as_slice).unwrap_or(&[]);
    at(iff).iter().any(|&p| {
        graph
            .node_opt(p)
            .is_some_and(|pn| pn.op == Op::Proj(1) && pn.ty == IrType::Control)
            && at(p).iter().any(|&c| {
                graph.node_opt(c).is_some_and(|cn| {
                    matches!(cn.op, Op::Call { .. }) && cn.input_opt(0) == Some(p)
                })
            })
    })
}

/// Decide the guards for the loop at `region`, or why nothing is planted (see
/// the module notes).
///
/// nio21 wave 16 (lane L3): a receiver whose every in-body class test is a
/// `Guard` (the trap-form splice `CRATONVM_JIT_IR_BUFFER_TRAP_MISS` gives a
/// `ByteBuffer` site: `dst.put(b)` in `encodeBufferLoop`) is a candidate too,
/// under [`guard_only_receivers_enabled`]. `plant_length_predicates` moves
/// such a guard to the pre-header itself, except in a loop holding a
/// `CharSequence.charAt` call (its `charAt` screen, because a hoisted class
/// guard licenses the trap); this pass runs `charat_trap`'s screen first, so
/// it may. Sound for the reason the in-body guard was: an exact class test of
/// a loop-invariant value answers the same on every iteration, and a failing
/// pre-header guard deopts to the loop entry, where the interpreter runs the
/// loop itself. A receiver also tested by a plain `If` (a source-level
/// `instanceof` of a final class, not a profile) stays out: its class may
/// legitimately vary between entries.
fn class_plan(
    graph: &Graph,
    region: NodeId,
    entry_pred: NodeId,
    back_ctrls: &[NodeId],
) -> Result<LoopPlan, &'static str> {
    let users = build_users(graph);
    let body = loop_body_with(graph, &users, region, entry_pred, back_ctrls);
    let entry_slot = graph
        .node_opt(region)
        .and_then(|r| r.inputs.iter().position(|&p| p == entry_pred))
        .ok_or("no entry slot")?
        + 1;
    let guard_only = guard_only_receivers_enabled();
    let mut ids: Vec<NodeId> = body.iter().copied().collect();
    ids.sort_unstable();

    // Every class test in the body, by receiver, in id order.
    // `(receiver, classes seen, tests, saw a splice If, saw a plain If)`.
    let mut seen: Vec<(NodeId, Vec<u32>, Vec<NodeId>, bool, bool)> = Vec::new();
    // Receivers the body compares by identity.
    let mut compared: FxHashSet<NodeId> = FxHashSet::default();
    // `(r, guard)` for every in-body `Guard(r != null)`: redundant once the
    // pre-header class guard (`ExactClassIs` answers `0` for null) holds.
    let mut null_guards: Vec<(NodeId, NodeId)> = Vec::new();
    for &id in &ids {
        let Some(n) = graph.node_opt(id) else {
            continue;
        };
        if n.op == Op::Dead {
            continue;
        }
        // Only a guarded splice's `If` -- whose miss edge (`Proj(1)`) runs
        // the virtual call (`IrBuilder::begin_guarded_receiver_splice`) --
        // makes a receiver a candidate: a source-level `instanceof` of a
        // final class tested the same way is a real branch, false on some
        // entries, not a profile the loop can speculate on.
        let is_if = n.op == Op::If && miss_edge_calls(graph, &users, id);
        if n.op == Op::If || matches!(n.op, Op::Guard { .. }) {
            if let Some(cond) = n.input_opt(1) {
                if let Some(cn) = graph.node_opt(cond) {
                    // A reference compare (`r == null`, `r == s`) BRANCHED on
                    // in the loop. A `Guard(r != null)` -- the receiver null
                    // check every statically bound splice plants
                    // (`IrBuilder::begin_splice`) -- does not count: it
                    // deopts, so the compiled loop never expected a null.
                    if n.op == Op::If && matches!(cn.op, Op::Cmp(_)) {
                        for &x in cn.inputs.iter() {
                            if graph.node_opt(x).is_some_and(|xn| xn.ty == IrType::Ref) {
                                compared.insert(x);
                            }
                        }
                    }
                    if n.op != Op::If {
                        if let Some(r) = null_checked(graph, cond) {
                            null_guards.push((r, id));
                        }
                    }
                }
                if let Some((r, class_id)) = exact_class_test(graph, cond) {
                    let plain_if = n.op == Op::If && !is_if;
                    match seen.iter_mut().find(|e| e.0 == r) {
                        Some(e) => {
                            if !e.1.contains(&class_id) {
                                e.1.push(class_id);
                            }
                            e.2.push(id);
                            e.3 |= is_if;
                            e.4 |= plain_if;
                        }
                        None => seen.push((r, vec![class_id], vec![id], is_if, plain_if)),
                    }
                }
            }
        }
    }
    if seen.is_empty() {
        return Err("no class test in the body");
    }
    let mut receivers: Vec<ReceiverPlan> = Vec::new();
    let mut why = "no receiver qualified";
    for (r, classes, tests, saw_if, saw_plain_if) in seen {
        let [class_id] = classes.as_slice() else {
            why = "a receiver tested against two classes";
            continue;
        };
        let admitted = saw_if || (guard_only && !saw_plain_if);
        if !admitted {
            why = "a receiver with no guarded splice (a plain class If)";
            continue;
        }
        if compared.contains(&r) {
            why = "a receiver the loop compares by identity";
            continue;
        }
        let Some(rn) = graph.node_opt(r) else {
            continue;
        };
        if rn.ty != IrType::Ref
            || matches!(
                rn.op,
                Op::New { .. }
                    | Op::NewArray { .. }
                    | Op::MultiNewArray { .. }
                    | Op::NewCp { .. }
                    | Op::Dead
            )
            || body.contains(&r)
            || !is_loop_invariant(graph, r, region, &body)
        {
            why = "a receiver that varies in the loop or is allocated here";
            continue;
        }
        let mut tests = tests;
        tests.extend(
            null_guards
                .iter()
                .filter(|&&(nr, _)| nr == r)
                .map(|&(_, g)| g),
        );
        receivers.push(ReceiverPlan {
            receiver: r,
            class_id: *class_id,
            tests,
        });
    }
    if receivers.is_empty() {
        return Err(why);
    }
    let (header_bci, frame) =
        loop_entry_frame(graph, region, entry_slot, &body).ok_or("no loop-entry frame")?;
    Ok(LoopPlan {
        header_bci,
        entry_pred,
        receivers,
        frame,
    })
}

/// Add one plan's guards, at most `budget` of them, and fold the tests each
/// one covers. Returns `(guards, folded tests)`.
fn install_class_plan(graph: &mut Graph, plan: LoopPlan, budget: usize) -> (usize, usize) {
    let LoopPlan {
        header_bci,
        entry_pred,
        receivers,
        frame,
    } = plan;
    if budget == 0 {
        return (0, 0);
    }
    // One frame copy for every guard of this loop; named by each guard, and
    // taken back out when none is (see `install_range_predicates`).
    let pc = Some(header_bci);
    graph.push_safepoint(frame);
    let si = (graph.safepoints.len() - 1) as u32;
    let one = graph.add(Op::Const(1), IrType::Int, vec![], None);
    let (mut planted, mut folded) = (0usize, 0usize);
    for rp in receivers {
        if planted >= budget {
            break;
        }
        // No pc on the test: no node of the pre-header may anchor the
        // header's bci but the guard that names its frame (the OSR arm
        // checks' rule, `IrBuilder::finish_osr_arm_checks`).
        let cond = graph.add(
            Op::ExactClassIs {
                class_id: rp.class_id,
            },
            IrType::Int,
            vec![rp.receiver],
            None,
        );
        let guard = graph.add(
            Op::Guard { bci: header_bci },
            IrType::Void,
            vec![entry_pred, cond],
            pc,
        );
        if !graph.set_node_frame_snapshot(guard, si) {
            graph.kill(guard);
            graph.kill(cond);
            break;
        }
        planted += 1;
        for t in rp.tests {
            if graph.set_input(t, 1, one) {
                folded += 1;
            }
        }
    }
    if planted == 0 {
        graph.kill(one);
        let keep: Vec<bool> = (0..graph.safepoints.len())
            .map(|i| i != si as usize)
            .collect();
        let _ = graph.retain_safepoints(&keep);
    }
    (planted, folded)
}


