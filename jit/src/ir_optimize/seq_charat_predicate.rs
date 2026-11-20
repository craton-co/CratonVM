// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT round 25 wave 6 (lane W6-C): a loop-entry `String` class guard for the
//! loop-invariant receiver of every `CharSequence.charAt` call in a loop
//! (`CRATONVM_JIT_IR_SEQ_CHARAT_PREDICATE`; opt-in by its own key, ON by
//! default through the `CRATONVM_JIT_IR_CHAIN_LOOP_PROOFS` bundle since j27
//! 2026-10-28).
//!
//! # Why
//!
//! The regex bodies read their subject through `CharSequence seq`:
//! `Pattern$BmpCharPropertyGreedy.match` runs
//! `while (i < to && predicate.is(seq.charAt(i))) { i++; n++; }`, and
//! `Pattern$Start.match` runs `for (; i <= guard; i++) next.match(matcher, i,
//! seq)` around the spliced node chain that does the same. Each `seq.charAt`
//! is an `invokeinterface` the optimizing tier keeps as an `Op::Call` and
//! lowers as the `SeqCharAt` prefix
//! (`ir_lower::Lowerer::try_emit_ir_string_search_prefix`). Per CHARACTER that
//! prefix executes eight conditional branches and two unconditional jumps:
//! the layout-epoch compare, the receiver null test, the array kind-tag test,
//! the `String` class-id compare, the per-object compact test (and the jump
//! over the legacy arm), the `value` null test, the coder test (and the jump
//! over the UTF16 arm) and the index compare. Only the last depends on the
//! iteration: `seq` is one object for the whole loop.
//!
//! The machinery that answers the invariant part once already exists, but
//! never reaches these loops:
//!
//! * `charat_trap` makes a `SeqCharAt` call DEOPT on a miss instead of calling
//!   (so it writes no memory and the field forward admits the loop), and
//!   `charat_trap::hoist_seq_charat_string_reads`
//!   (`CRATONVM_JIT_IR_SEQ_CHARAT_HOIST`) rewrites it as IR: the class test and
//!   the `value` / `coder` / length reads at the loop entry, two index guards
//!   and the element load in the body. Both are licensed only by a loop-entry
//!   class guard that `class_predicate` planted ([`Graph::seq_charat_trap_headers`]).
//! * `class_predicate` plants only for a receiver the BODY class-tests (a
//!   guarded splice's `ExactClassIs`). The `SeqCharAt` call has no such node,
//!   and the regex loops' other receivers (`this.predicate`, `this.next`) are
//!   field reads inside the loop, never invariant. So no regex loop ever got a
//!   guard, no call ever trapped, and no hoist ran.
//! * `class_predicate`, the forward after it and the hoist all refuse an
//!   OSR-door graph (`Graph::osr_compile`) or the OSR route outright. The
//!   regex `match` bodies are OSR-door builds (the parent page,
//!   `docs/feature-designs/jit-ir-optimizations.md`), so even a planted
//!   guard would have been refused there.
//!
//! # What
//!
//! For each loop (outer loops first, so one guard in an outer pre-header covers
//! the inner loops and the spliced callee loops inside it), that holds at least
//! one `SeqCharAt` call not yet licensed to trap, and whose EVERY such call has
//! a loop-invariant reference receiver: one
//! `Guard{bci: header}(ExactClassIs(recv, String))` per distinct receiver, on
//! the loop's entry control, with the loop-entry frame (the header snapshot
//! with every header phi at its entry value), exactly the guard
//! `class_predicate` plants; and the header's bci goes into
//! [`Graph::seq_charat_trap_headers`]. From then on the existing predicates
//! read it: [`super::charat_trap::seq_charat_traps_on_miss`] names the loop's
//! calls (the lowering emits the prefix with every decline on a deopt at the
//! call's pc), `loop_field_forward` admits them, and under
//! `CRATONVM_JIT_IR_SEQ_CHARAT_HOIST=1` the hoist rewrites them.
//!
//! In an OSR graph (the OSR door's compile, or the OSR route) a loop is taken
//! only when every way into it passes its pre-header
//! ([`super::loop_entered_only_through_its_pre_header`]: the entry is the
//! opaque OSR merge of its own header, and the body imports nothing from
//! another OSR arm). That is the rule `CRATONVM_JIT_IR_NULL_BASE_LOAD_PREDICATE_OSR`
//! and `CRATONVM_JIT_IR_OSR_ROUTE_ARM_PREDICATES` already rely on.
//!
//! # Why it is sound
//!
//! * The guard only DEOPTS, and it is the shape `class_predicate` plants:
//!   `ExactClassIs` is pure and total (a null or an array answers `0`), the
//!   receiver is one SSA value defined outside the loop, and a failing guard
//!   resumes the interpreter at the loop header with the state the loop is
//!   entered with (the loop-entry frame). Nothing has run between the
//!   pre-header and that point, so the interpreter runs the loop and every
//!   `charAt` itself, with the bytecode's own dispatch and exceptions.
//! * The trap the guard licenses fails closed on its own: every test of the
//!   prefix stays (`charat_trap`'s module notes); only the miss target changes
//!   from the call to a deopt AT the call's pc, with the frame
//!   `class_predicate_withheld` (asked here, as `class_predicate` asks it)
//!   proved exists. A receiver that is not a `String` at the call -- the guard
//!   was skipped by an entry, or the speculation was wrong -- deopts there and
//!   the interpreter re-runs the `invokeinterface`.
//! * Entries. A method-entry body's ordinary OSR entry stub at the header
//!   would skip the guard: `ir_lower::Lowerer::emit_osr_entry_stubs` refuses
//!   an entry past a `Guard` planted at the entry block's own bci in a block
//!   that dominates it (`CRATONVM_JIT_IR_OSR_PREDICATE_REFUSAL`, default ON),
//!   as for `class_predicate`. This pass plants nothing when that switch is
//!   off. An OSR graph's loop is entered only through the guard (above). An
//!   inner header's own entry, past an outer guard, finds the calls still
//!   fully tested (the trap above); a value the hoist made live across such
//!   an entry is the entry stub's business (re-executed or the entry refused,
//!   as for every pre-header value).
//! * The deopt state at the hoisted guard matches the interpreter's at the
//!   header: the frame is the header's own snapshot with each header phi
//!   replaced by its value on the entry edge (`loop_entry_frame`), which in an
//!   OSR graph is the opaque merge's phi -- the interpreter's local on the arm,
//!   the method's value on the normal path. A slot naming any other in-loop
//!   value refuses the loop.
//!
//! # Cost when the speculation is wrong
//!
//! Unlike `class_predicate`, no profile backs the guess: the call site is a
//! `CharSequence` interface call, and the guess is the prefix's own fast
//! path (`String`). A loop whose receiver is a `StringBuilder` deopts at its
//! entry; the failing guard is charged at the header bci as a
//! `PredicateKind::Class` speculation, and after `PER_BCI_DESPEC_LIMIT`
//! charges [`crate::deopt::predicate_withdrawn`] answers yes and the next
//! compile plants nothing there (and `class_predicate` nothing either: the
//! two share the kind). A call whose own site was de-specialised keeps its
//! call for the same reason (`charat_trap::withheld_by_despec`).

use super::{
    build_users, is_loop_invariant, loop_body_with, loop_entered_only_through_its_pre_header,
    loop_entry_frame, loop_headers,
};
use crate::ir::{Graph, IrType, NodeId, Op, NO_NODE};
use rustc_hash::FxHashSet;

/// At most this many receiver guards per compile.
const MAX_SEQ_CHARAT_GUARDS: usize = 4;

/// `CRATONVM_JIT_IR_SEQ_CHARAT_PREDICATE` (JIT round 25 wave 6, lane W6-C) --
/// its own key is opt-in (`=1`), but the `CRATONVM_JIT_IR_CHAIN_LOOP_PROOFS`
/// bundle turns it on, and that bundle is default ON since j27 2026-10-28, so
/// the effective default is ON; `=0` of either key kills it. Read per compile,
/// never cached.
pub(super) fn seq_charat_predicate_enabled() -> bool {
    false
        // JIT r25 wave 11 (W11-4): or the chain loop-proof bundle.
        || super::r25_w11_4_chain_loop_proofs::bundled("CRATONVM_JIT_IR_SEQ_CHARAT_PREDICATE")
}

/// What [`plant_seq_charat_predicates`] did, for the `CRATONVM_DBG_JITC` line.
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub(super) struct SeqCharAtPredicateCensus {
    /// Loops holding at least one `SeqCharAt` call.
    pub(super) loops: usize,
    /// Of those, loops whose calls were already licensed to trap (by
    /// `class_predicate` or by an outer loop's guard).
    pub(super) already: usize,
    /// Loops that received at least one guard.
    pub(super) loops_planted: usize,
    /// Guards planted.
    pub(super) guards: usize,
    /// Refused: the header's class speculation was withdrawn.
    pub(super) refused_despec: usize,
    /// Refused: an OSR graph's loop that some entry reaches past its pre-header.
    pub(super) refused_osr_entry: usize,
    /// Refused: `charat_trap::class_predicate_withheld` (a de-specialised call
    /// site, or a call with no frame its trap could resume).
    pub(super) refused_withheld: usize,
    /// Refused: a call's receiver varies in the loop (or is no reference).
    pub(super) refused_receiver: usize,
    /// Refused: no loop-entry frame, the guard budget, or a frame that could
    /// not be named.
    pub(super) refused_frame: usize,
    /// Calls the charAt hoist rewrote afterwards (filled by the caller).
    pub(super) hoisted: usize,
    /// JIT round 25 wave 7 (lane W7-D): loops holding a `charAt` whose header
    /// is SPLICED from a callee (only an inlined chain snapshot at its pc),
    /// counted whatever `CRATONVM_JIT_IR_SEQ_CHARAT_PREDICATE_SPLICED` says:
    /// the W6-C refusal of such a loop is `osr-entry` (OSR graph) or `frame`.
    pub(super) spliced_headers: usize,
    /// Of `loops_planted`, the loops planted through the spliced arm.
    pub(super) planted_spliced: usize,
    /// Refused: `CRATONVM_JIT_IR_SEQ_CHARAT_PREDICATE_PROFILE` and a call
    /// whose receiver profile is not monomorphic `String`.
    pub(super) refused_profile: usize,
}

/// What [`plant_seq_charat_predicates`] returns.
#[derive(Default)]
pub(super) struct SeqCharAtPredicateOutcome {
    /// The loop headers (region nodes) that received at least one guard.
    pub(super) headers: FxHashSet<NodeId>,
    pub(super) census: SeqCharAtPredicateCensus,
}

/// Plant the guards the module notes describe. `osr_route` is the compile's
/// route (`OptimizeContext::osr_route`); an OSR-door graph is recognised by
/// `Graph::osr_compile` itself.
pub(super) fn plant_seq_charat_predicates(
    graph: &mut Graph,
    despec: &crate::deopt::DespecRegistry,
    method_key: &str,
    osr_route: bool,
) -> SeqCharAtPredicateOutcome {
    let mut out = SeqCharAtPredicateOutcome::default();
    if !seq_charat_predicate_enabled()
        || graph.safepoints.is_empty()
        || !super::charat_trap::seq_charat_trap_enabled()
        || crate::x64::narrow_oops_block_inline_fields()
        // The method-entry body's header entries are kept out from under the
        // guard only by this refusal (see the module notes).
        || !true
    {
        return out;
    }
    // The prefix's own compile-time premises: without them a call the guard
    // names would refuse the whole compile at lowering.
    let Some(layout) = graph.string_layout else {
        return out;
    };
    if !layout.has_coder || layout.string_class_id == 0 {
        return out;
    }
    let string_class = layout.string_class_id;
    let osr_graph = osr_route || graph.osr_compile || graph.optimize_osr_route;
    let dbg = super::loop_pass_report_enabled();
    let splice_map = crate::ir::splice_resume_map_this_build();
    // Outer loops first (the largest body), as `class_predicate` orders them.
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
        let users = build_users(graph);
        let body = loop_body_with(graph, &users, region, entry_pred, &back_ctrls);
        let mut calls: Vec<NodeId> = body
            .iter()
            .copied()
            .filter(|&id| {
                graph
                    .node_opt(id)
                    .is_some_and(super::charat_trap::is_seq_charat_call)
            })
            .collect();
        if calls.is_empty() {
            continue;
        }
        calls.sort_unstable();
        out.census.loops += 1;
        if calls
            .iter()
            .all(|&c| super::charat_trap::seq_charat_traps_on_miss(graph, c))
        {
            out.census.already += 1;
            continue;
        }
        let refuse = |why: &str| {
            if dbg {
                eprintln!(
                    "[ir-licm] seq charAt predicate: {method_key} header {region}: not planted: {why}"
                );
            }
        };
        if out.census.guards >= MAX_SEQ_CHARAT_GUARDS {
            out.census.refused_frame += 1;
            refuse("the per-compile guard budget is spent");
            continue;
        }
        let Some(header_bci) = graph.node_opt(region).and_then(|r| r.bytecode_pc) else {
            out.census.refused_frame += 1;
            refuse("the header has no bci");
            continue;
        };
        let Ok(bci32) = u32::try_from(header_bci) else {
            out.census.refused_frame += 1;
            refuse("the header bci does not fit");
            continue;
        };
        if entry_pred == NO_NODE || body.contains(&entry_pred) {
            out.census.refused_frame += 1;
            refuse("no single entry control outside the body");
            continue;
        }
        // JIT round 25 wave 7 (lane W7-D): a header spliced from a callee
        // (`w7d` module notes). Its withdrawal is asked below, once its chain
        // frame names the call site the VM charges it at.
        let header_spliced = super::header_is_spliced(graph, region);
        if header_spliced {
            out.census.spliced_headers += 1;
        }
        let spliced = header_spliced && w7d::spliced_enabled();
        if !spliced
            && crate::deopt::predicate_withdrawn(
                despec,
                method_key,
                bci32,
                crate::deopt::PredicateKind::Class,
            )
        {
            out.census.refused_despec += 1;
            refuse("the header's class speculation was withdrawn");
            continue;
        }
        if osr_graph
            && !w7d::entered_only_through_pre_header(graph, region, entry_pred, &body, spliced)
        {
            out.census.refused_osr_entry += 1;
            refuse("an OSR graph's loop some entry reaches past its pre-header");
            continue;
        }
        if let Some(why) = super::charat_trap::class_predicate_withheld(
            graph,
            despec,
            method_key,
            region,
            entry_pred,
            &back_ctrls,
            splice_map.as_ref(),
        ) {
            out.census.refused_withheld += 1;
            refuse(why);
            continue;
        }
        let Some(receivers) = invariant_receivers(graph, region, &body, &calls) else {
            out.census.refused_receiver += 1;
            refuse("a charAt receiver varies in the loop");
            continue;
        };
        // JIT round 25 wave 7 (lane W7-D, W6C-P4): only behind a monomorphic
        // `String` receiver profile at every call, under its switch.
        if w7d::profile_enabled() && !w7d::calls_profiled_string(graph, &calls, string_class) {
            out.census.refused_profile += 1;
            refuse("a charAt site's receiver profile is not monomorphic String");
            continue;
        }
        let Some(entry_slot) = graph
            .node_opt(region)
            .and_then(|r| r.inputs.iter().position(|&p| p == entry_pred))
            .map(|p| p + 1)
        else {
            out.census.refused_frame += 1;
            refuse("no entry slot");
            continue;
        };
        let entry_frame = if spliced {
            super::loop_entry_chain_frame(graph, region, entry_slot, &body)
        } else {
            loop_entry_frame(graph, region, entry_slot, &body)
        };
        let Some((bci, frame)) = entry_frame else {
            out.census.refused_frame += 1;
            refuse("no loop-entry frame");
            continue;
        };
        if bci != header_bci {
            out.census.refused_frame += 1;
            refuse("the loop-entry frame is not at the header");
            continue;
        }
        if spliced
            && w7d::withdrawn(
                despec,
                method_key,
                header_bci,
                &frame,
                crate::deopt::PredicateKind::Class,
            )
        {
            out.census.refused_despec += 1;
            refuse("the spliced header's class speculation was withdrawn at its call site");
            continue;
        }
        let nodes_before = graph.nodes.len();
        let budget = MAX_SEQ_CHARAT_GUARDS - out.census.guards;
        let planted = install(
            graph,
            header_bci,
            entry_pred,
            string_class,
            &receivers,
            frame,
            budget,
        );
        if planted < receivers.len() {
            // A partial plant would leave a call whose receiver no guard
            // covers; `install` plants all or nothing.
            out.census.refused_frame += 1;
            refuse("the frame could not be named, or the budget ran out");
            continue;
        }
        if spliced {
            w7d::register_chain_guards(graph, nodes_before, crate::deopt::PredicateKind::Class);
            out.census.planted_spliced += 1;
        }
        out.census.guards += planted;
        out.census.loops_planted += 1;
        out.headers.insert(region);
        graph.seq_charat_trap_headers.insert(header_bci);
        if dbg {
            eprintln!(
                "[ir-licm] seq charAt predicate: {method_key} header {region} bci {header_bci}: \
                 planted {planted} String guard(s) for {} call(s)",
                calls.len()
            );
        }
    }
    out
}

/// The distinct receivers of `calls`, in id order, when every one is a
/// reference defined outside the loop and invariant in it (not an allocation
/// of this compile, which escape analysis knows the class of); `None` when any
/// call's is not.
fn invariant_receivers(
    graph: &Graph,
    region: NodeId,
    body: &FxHashSet<NodeId>,
    calls: &[NodeId],
) -> Option<Vec<NodeId>> {
    let mut out: Vec<NodeId> = Vec::new();
    for &c in calls {
        let r = graph.node_opt(c)?.input_opt(2)?;
        if r == NO_NODE {
            return None;
        }
        let rn = graph.node_opt(r)?;
        if rn.ty != IrType::Ref
            || matches!(
                rn.op,
                Op::New { .. }
                    | Op::NewArray { .. }
                    | Op::MultiNewArray { .. }
                    | Op::NewCp { .. }
                    | Op::Dead
            )
            || crate::ir::is_null_const(rn)
            || body.contains(&r)
            || !is_loop_invariant(graph, r, region, body)
        {
            return None;
        }
        if !out.contains(&r) {
            out.push(r);
        }
    }
    out.sort_unstable();
    Some(out)
}

/// Plant one guard per receiver on `entry_pred`, all or nothing (at most
/// `budget`). Returns how many were planted: `receivers.len()` or `0`.
fn install(
    graph: &mut Graph,
    header_bci: usize,
    entry_pred: NodeId,
    string_class: u32,
    receivers: &[NodeId],
    frame: crate::ir::SafepointSnapshot,
    budget: usize,
) -> usize {
    if receivers.is_empty() || receivers.len() > budget {
        return 0;
    }
    // One frame copy for every guard of this loop (`class_predicate`'s
    // `install_class_plan`), taken back out when none is planted.
    graph.push_safepoint(frame);
    let si = (graph.safepoints.len() - 1) as u32;
    let mut made: Vec<(NodeId, NodeId)> = Vec::new();
    let mut ok = true;
    for &r in receivers {
        // No pc on the test: no node of the pre-header may anchor the header's
        // bci but the guard that names its frame (`class_predicate`'s rule).
        let cond = graph.add(
            Op::ExactClassIs {
                class_id: string_class,
            },
            IrType::Int,
            vec![r],
            None,
        );
        let guard = graph.add(
            Op::Guard { bci: header_bci },
            IrType::Void,
            vec![entry_pred, cond],
            Some(header_bci),
        );
        made.push((guard, cond));
        if !graph.set_node_frame_snapshot(guard, si) {
            ok = false;
            break;
        }
    }
    if ok {
        return made.len();
    }
    for (guard, cond) in made {
        graph.kill(guard);
        graph.kill(cond);
    }
    let keep: Vec<bool> = (0..graph.safepoints.len())
        .map(|i| i != si as usize)
        .collect();
    let _ = graph.retain_safepoints(&keep);
    0
}

/// The `CRATONVM_DBG_JITC` line, one per body while the switch is on, zeros
/// included (a zero is a claim about the workload, so it is printed).
pub(super) fn note_census(census: &SeqCharAtPredicateCensus, method_key: &str) {
    if !crate::x64::dbg_jitc_enabled() || !seq_charat_predicate_enabled() {
        return;
    }
    eprintln!(
        "[cratonvm-jitc] ir-seq-charat-predicate {}: loops with charAt={} already trapping={} \
         planted loops={} guards={} hoisted calls={} refused: despec={} osr-entry={} withheld={} \
         receiver={} frame={} profile={} spliced headers={} planted spliced={}",
        if method_key.is_empty() {
            "?"
        } else {
            method_key
        },
        census.loops,
        census.already,
        census.loops_planted,
        census.guards,
        census.hoisted,
        census.refused_despec,
        census.refused_osr_entry,
        census.refused_withheld,
        census.refused_receiver,
        census.refused_frame,
        census.refused_profile,
        census.spliced_headers,
        census.planted_spliced,
    );
}

#[cfg(test)]
mod tests;

// ── JIT round 25 wave 7 (lane W7-D) ──────────────────────────────────────

/// JIT round 25 wave 7 (lane W7-D): the regex `charAt` loops W6-C refused,
/// and three of its proposals (W6C-P2, P3, P4). Every arm is opt-in, read per
/// compile, and changes nothing with its switch unset.
///
/// # Why W6-C refused the `regex-groups` loops (`CRATONVM_JIT_IR_SEQ_CHARAT_PREDICATE_SPLICED`)
///
/// The `charAt` loops of `(\w+)=(\w+)` live in `Pattern$BmpCharPropertyGreedy.match`
/// (`while (i < to && predicate.is(seq.charAt(i)))`), which the find path
/// reaches SPLICED: `Matcher.search` -> `Start.match` -> `GroupHead.match` ->
/// `BmpCharPropertyGreedy.match`, each a guarded splice of the one above. A
/// loop spliced from a callee has no frame of the compiling method at its
/// header, only the inlined CHAIN snapshot, and no opaque OSR merge (the
/// builder plants none inside a splice). W6-C's pass asked for both: in an
/// OSR-door graph `loop_entered_only_through_its_pre_header` (the entry must
/// be the opaque merge of the header's own bci) refused it as `osr-entry`, in
/// a method-entry graph `loop_entry_frame` (a flat snapshot at the header)
/// refused it as `frame`. The compiling method's own loop around the splices
/// (`Start.match`'s `for (; i <= guard; i++)`) holds every `charAt` of the
/// node chain, and the deep ones (`GroupHead -> Greedy -> GroupTail -> Single
/// -> GroupHead -> Greedy`) need a publishable chain at their pc; one call
/// without it withholds the whole loop (`withheld`), all or nothing. On
/// `split-ws` (`\s+`: `Start -> Greedy`) the chain is shallow, which is why
/// that row planted `Start.match` and `regex-groups` planted nothing.
///
/// The spliced arm takes such a loop the way the null-base predicate's chain
/// arm (`CRATONVM_JIT_IR_NULL_BASE_LOAD_PREDICATE_CHAIN`) does: the guard's
/// frame is the loop-entry CHAIN (`ir_optimize::loop_entry_chain_frame`:
/// the callee at its own loop header with each header phi at its entry value,
/// every caller parked at its invoke), so a failure resumes the interpreter
/// in the callee at the loop's first test; in an OSR graph only a loop no OSR
/// entry can bypass (`ir_optimize::spliced_loop_entered_only_through_its_pre_header`);
/// the guard is listed in `Graph::chain_predicate_guards`, so the lowering
/// gives it the chain id the VM's chain sinks charge at the OUTERMOST call
/// site, and the next compile asks [`crate::deopt::chain_predicate_withdrawn`]
/// there. The charAt hoist takes the same frame and the same withdrawal.
///
/// # W6C-P4 (`CRATONVM_JIT_IR_SEQ_CHARAT_PREDICATE_PROFILE`)
///
/// `Graph::seq_charat_receiver_rows` holds the receiver counts of every
/// `charAt` site (the callee's own profile for a spliced one, the compiling
/// method's for its own); with the switch on, a loop is planted only when
/// every call's row names `String` alone with at least
/// `MIN_GUARDED_RECEIVER_EVIDENCE` exact samples. A loop with no row is
/// refused (`profile=`), which also keeps a `StringBuilder` subject from
/// paying entry deopts until the de-spec limit.
///
/// # W6C-P2 (`CRATONVM_JIT_IR_SEQ_CHARAT_FIELD_CLASS_PREDICATE`)
///
/// `predicate.is(c)` and `next.match(..)` are guarded splices whose receiver
/// is a READ of a field of an invariant object (`this.predicate`,
/// `this.next`), so `class_predicate` (an SSA-invariant receiver) never takes
/// them, and a re-run after the forward would not either: the forward keeps
/// those reads in the loop because the miss arms' calls may write anything.
/// The two facts depend on each other, and are proved together by induction
/// instead. For a loop already guarded (its `charAt` calls trap), a field key
/// `(base, index, declaring class)` qualifies when
///
/// * `base` is defined outside the loop, invariant, not an allocation escape
///   analysis owns, and no node of the loop tests its class;
/// * every class test of a read of the key is `ExactClassIs` of ONE class `C`,
///   consumed only by guarded-splice `If`s (miss edge = a call) or guards;
/// * every node of the loop that may write memory is a trapping `charAt`, a
///   miss-arm call of one of those `If`s, or a write that provably misses the
///   field (another field identity, an array element, a static); nothing has
///   acquire / release order and nothing is volatile. A key some write may
///   reach is dropped, and the loop is refused if its tests' miss calls would
///   then stay.
///
/// Then the pre-header reads `p0 = base.f` (behind a null guard unless `base`
/// is known non-null) and guards `ExactClassIs(p0, C)`, both with the
/// loop-entry frame. Induction over the iterations: while every earlier
/// iteration ran the hit arms, nothing wrote `base.f` (the only writers that
/// could were the miss calls), so the next read of it is `p0`, its class is
/// `C`, and the hit arm runs again. So with the guard passing the miss arms
/// never run: every class test becomes `1` (the prune cuts the miss calls) and
/// every read of the key becomes `p0`, its memory users taking its own
/// incoming token. With no synchronisation in the loop, keeping the entry
/// value is a legal JMM execution (the forward's argument). A failing guard
/// deopts to the loop header with the state the loop is entered with. Up to
/// three rounds: a read through `p0` (`this.next.next`) qualifies in the next
/// one, ordered after `p0`'s class guard by its token.
///
/// # W6C-P3 (`CRATONVM_JIT_IR_SEQ_CHARAT_ENTRY_BOUND`)
///
/// After the hoist, each rewritten call keeps `Guard(0 <= i)` and
/// `Guard(i < count)` per character. When the call's control is dominated,
/// through a chain of `If` projections inside the loop, by an edge stating
/// `i < x` (or `i <= x`) for an invariant `x`, the pre-header guard
/// `x <= count` (`x < count`) answers the bound for every iteration and the
/// in-body one goes. When that edge is the header's own first test (its other
/// edge leaves the loop) and `i` is a header phi stepped only by `+ 1`, `i`
/// never overflows (`i + 1 <= count <= MAX`), so `0 <= init` at the entry
/// (nothing for a non-negative constant) answers `0 <= i` too. Both guards
/// deopt to the header with the loop-entry frame (a header the registry
/// withdrew `Monotone` at keeps its in-body guards); a subject shorter than
/// the loop's bound then runs that activation interpreted, which raises the
/// bytecode's own `StringIndexOutOfBoundsException` where it raises it.
pub(super) mod w7d {
    pub(in crate::ir_optimize) fn plant_entry_bounds(
        _graph: &mut Graph,
        _despec: &DespecRegistry,
        _method_key: &str,
        _headers: &FxHashSet<NodeId>,
        _census: &mut W7dCensus,
    ) -> usize {
        0
    }
    use super::super::class_predicate::miss_edge_calls;
    use super::super::loop_field_forward as lff;
    use super::super::{
        build_users, header_is_spliced, is_loop_invariant, loop_body_with, loop_entry_chain_frame,
        loop_entry_frame, loop_headers, spliced_loop_entered_only_through_its_pre_header,
    };
    use crate::deopt::{DespecRegistry, PredicateKind};
    use crate::ir::{
        AccessOffset, AliasClass, CmpOp, Graph, IrType, MemKind, MemOrder, NodeId, Op,
        SafepointSnapshot, NO_NODE,
    };
    use rustc_hash::{FxHashMap, FxHashSet};

    /// `CRATONVM_JIT_IR_SEQ_CHARAT_PREDICATE_SPLICED` -- its own key is opt-in;
    /// ON through the chain loop-proof bundle (default ON since j27
    /// 2026-10-28) unless `=0`. Read per asked loop, never cached.
    pub(in crate::ir_optimize) fn spliced_enabled() -> bool {
        false
            || super::super::r25_w11_4_chain_loop_proofs::bundled(
                "CRATONVM_JIT_IR_SEQ_CHARAT_PREDICATE_SPLICED",
            )
    }

    /// `CRATONVM_JIT_IR_SEQ_CHARAT_PREDICATE_PROFILE` -- **opt-in**.
    pub(in crate::ir_optimize) fn profile_enabled() -> bool {
        false
    }

    /// `CRATONVM_JIT_IR_SEQ_CHARAT_FIELD_CLASS_PREDICATE` -- its own key is
    /// opt-in; ON through the chain loop-proof bundle (default ON since j27
    /// 2026-10-28) unless `=0`.
    pub(in crate::ir_optimize) fn field_class_enabled() -> bool {
        false
            || super::super::r25_w11_4_chain_loop_proofs::bundled(
                "CRATONVM_JIT_IR_SEQ_CHARAT_FIELD_CLASS_PREDICATE",
            )
    }

    /// `CRATONVM_JIT_IR_SEQ_CHARAT_ENTRY_BOUND` -- **opt-in**.
    pub(in crate::ir_optimize) fn entry_bound_enabled() -> bool {
        false
            || super::super::r25_w11_4_chain_loop_proofs::bundled(
                "CRATONVM_JIT_IR_SEQ_CHARAT_ENTRY_BOUND",
            )
            // JIT rx27 wave 3 (lane scanloop): the outer-loop hoist brings
            // its bound with it (`charat_trap`, the rx27 wave 3 section),
            // unless this key is killed explicitly (as under the bundle).
            || (super::super::charat_trap::outer_hoist_enabled()
                && !super::super::r25_w11_4_chain_loop_proofs::explicitly_off(
                    "CRATONVM_JIT_IR_SEQ_CHARAT_ENTRY_BOUND",
                ))
    }

    /// Is `region` a header spliced from a callee that the spliced arm takes?
    pub(in crate::ir_optimize) fn spliced_header(graph: &Graph, region: NodeId) -> bool {
        spliced_enabled() && header_is_spliced(graph, region)
    }

    /// In an OSR graph: does every way into the loop pass `entry_pred`? The
    /// compiling method's own header needs its opaque merge; a spliced one
    /// is never an OSR entry, under its own rule.
    pub(in crate::ir_optimize) fn entered_only_through_pre_header(
        graph: &Graph,
        region: NodeId,
        entry_pred: NodeId,
        body: &FxHashSet<NodeId>,
        spliced: bool,
    ) -> bool {
        if spliced {
            spliced_loop_entered_only_through_its_pre_header(graph, region, body)
        } else {
            super::loop_entered_only_through_its_pre_header(graph, region, entry_pred, body)
        }
    }

    /// Has the registry withdrawn planter `kind` at this header? A chain
    /// frame is asked at its outermost call site under the innermost scope's
    /// chain id (the null-base chain arm's rule); a flat one at the header.
    pub(in crate::ir_optimize) fn withdrawn(
        despec: &DespecRegistry,
        method_key: &str,
        header_bci: usize,
        frame: &SafepointSnapshot,
        kind: PredicateKind,
    ) -> bool {
        match frame.splice_scope.as_deref() {
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
            None => u32::try_from(header_bci).map_or(true, |b| {
                crate::deopt::predicate_withdrawn(despec, method_key, b, kind)
            }),
        }
    }

    /// List every `Op::Guard` added from node `from` on as a chain predicate
    /// of `kind` (`Graph::chain_predicate_guards`).
    pub(in crate::ir_optimize) fn register_chain_guards(
        graph: &mut Graph,
        from: usize,
        kind: PredicateKind,
    ) {
        for i in from..graph.nodes.len() {
            if matches!(graph.nodes[i].op, Op::Guard { .. }) {
                // Cast: arena index to node id.
                graph.chain_predicate_guards.push((i as NodeId, kind as u8));
            }
        }
    }

    /// Record one planted guard as planter `kind`'s (`tag_predicate_guards`'
    /// rule), and as a chain predicate when its frame is a chain.
    fn tag(graph: &mut Graph, guard: NodeId, kind: PredicateKind, chain: bool) {
        if crate::deopt::predicate_own_speculation_enabled() {
            graph.predicate_guard_kinds.push((guard, kind as u8));
        }
        if chain {
            graph.chain_predicate_guards.push((guard, kind as u8));
        }
    }

    /// The loop-entry frame, the chain one for a spliced header.
    fn entry_frame(
        graph: &Graph,
        region: NodeId,
        entry_slot: usize,
        body: &FxHashSet<NodeId>,
        spliced: bool,
    ) -> Option<(usize, SafepointSnapshot)> {
        if spliced {
            loop_entry_chain_frame(graph, region, entry_slot, body)
        } else {
            loop_entry_frame(graph, region, entry_slot, body)
        }
    }

    // ── W6C-P4: the receiver profile ──

    /// Does every call of `calls` have a receiver row naming `string_class`
    /// alone, with enough exact samples?
    pub(in crate::ir_optimize) fn calls_profiled_string(
        graph: &Graph,
        calls: &[NodeId],
        string_class: u32,
    ) -> bool {
        string_class != 0
            && calls.iter().all(|&c| {
                graph
                    .node_opt(c)
                    .and_then(|n| n.bytecode_pc)
                    .and_then(|pc| graph.seq_charat_receiver_rows.get(&pc))
                    .and_then(|counts| {
                        crate::profile::ReceiverEvidence::of(counts)
                            .only_class(crate::MIN_GUARDED_RECEIVER_EVIDENCE)
                    })
                    == Some(string_class)
            })
    }

    /// `lib.rs`, before `optimize`: the compiling method's own `charAt`
    /// sites' receiver counts (pcs below `code_len`) into
    /// `Graph::seq_charat_receiver_rows`. Nothing with the switch off.
    pub(crate) fn seed_seq_charat_own_receiver_rows(
        graph: &mut Graph,
        profile: Option<&crate::profile::MethodProfile>,
        code_len: usize,
    ) {
        if !profile_enabled() {
            return;
        }
        let Some(profile) = profile else {
            return;
        };
        let pcs: Vec<usize> = graph
            .nodes
            .iter()
            .filter(|n| super::super::charat_trap::is_seq_charat_call(n))
            .filter_map(|n| n.bytecode_pc)
            .filter(|&pc| pc < code_len)
            .collect();
        for pc in pcs {
            if let Some(counts) = profile.receivers.get(&pc) {
                graph.seq_charat_receiver_rows.insert(pc, counts.clone());
            }
        }
    }

    // ── The census ──

    /// What the W6C-P2 / P3 arms did, for the `CRATONVM_DBG_JITC` line.
    #[derive(Default, Debug, Clone, PartialEq, Eq)]
    pub(in crate::ir_optimize) struct W7dCensus {
        /// P2: guarded loops examined.
        pub(in crate::ir_optimize) field_loops: usize,
        /// P2: loops that received at least one field class guard.
        pub(in crate::ir_optimize) field_planted_loops: usize,
        /// P2: field class guards planted.
        pub(in crate::ir_optimize) field_guards: usize,
        /// P2: in-body class tests folded to `1`.
        pub(in crate::ir_optimize) field_folded: usize,
        /// P2: in-body field reads replaced by the pre-header read.
        pub(in crate::ir_optimize) field_reads: usize,
        /// P2 refused: no field read a guarded splice class-tests.
        pub(in crate::ir_optimize) field_refused_candidate: usize,
        /// P2 refused: a writer the induction cannot step over.
        pub(in crate::ir_optimize) field_refused_writer: usize,
        /// P2 refused: no loop-entry frame, a withdrawn header, or an OSR
        /// entry past the pre-header.
        pub(in crate::ir_optimize) field_refused_entry: usize,
        /// P3: loops holding a hoisted bound guard.
        pub(in crate::ir_optimize) bound_loops: usize,
        /// P3: in-body `i < count` guards answered at the entry.
        pub(in crate::ir_optimize) bounds: usize,
        /// P3: in-body `0 <= i` guards answered at the entry.
        pub(in crate::ir_optimize) non_negative: usize,
        /// P3 refused: no dominating `i < x` edge with an invariant `x`.
        pub(in crate::ir_optimize) bound_refused_fact: usize,
        /// P3 refused: no loop-entry frame or a withdrawn header.
        pub(in crate::ir_optimize) bound_refused_entry: usize,
    }

    /// The line, one per body while either arm is on, zeros included.
    pub(in crate::ir_optimize) fn note_census(census: &W7dCensus, method_key: &str) {
        if !crate::x64::dbg_jitc_enabled() || !(field_class_enabled() || entry_bound_enabled()) {
            return;
        }
        eprintln!(
            "[cratonvm-jitc] ir-seq-charat-w7d {}: field-class loops={} planted loops={} guards={} \
             tests folded={} reads replaced={} refused: candidate={} writer={} entry={}; \
             entry-bound loops={} bounds={} non-negative={} refused: fact={} entry={}",
            if method_key.is_empty() {
                "?"
            } else {
                method_key
            },
            census.field_loops,
            census.field_planted_loops,
            census.field_guards,
            census.field_folded,
            census.field_reads,
            census.field_refused_candidate,
            census.field_refused_writer,
            census.field_refused_entry,
            census.bound_loops,
            census.bounds,
            census.non_negative,
            census.bound_refused_fact,
            census.bound_refused_entry,
        );
    }

    // ── W6C-P2: field-read receivers ──

    /// At most this many field class guards per compile.
    const MAX_FIELD_CLASS_GUARDS: usize = 4;
    /// Rounds (a read through a guarded read qualifies in the next one).
    const MAX_FIELD_CLASS_ROUNDS: usize = 3;

    /// One field cell of an invariant object.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    struct FieldKey {
        base: NodeId,
        idx: i64,
        decl: u32,
    }

    /// One key's plan.
    struct FieldPlan {
        key: FieldKey,
        /// The index operand of the reads (a `Const`).
        idx_node: NodeId,
        class_id: u32,
        /// Every in-loop read of the key.
        reads: Vec<NodeId>,
        /// The `If`s and `Guard`s whose condition is `ExactClassIs(read, C)`.
        tests: Vec<NodeId>,
        /// The guards whose condition is `read != null`.
        null_tests: Vec<NodeId>,
    }

    enum FieldRefusal {
        Candidate(&'static str),
        Writer(&'static str),
    }

    /// Plant the W6C-P2 guards in the loops of `headers` (see the module
    /// notes); returns the guards planted. Runs its own constant-`If` prune
    /// and scalar cleanup after each round that planted.
    pub(in crate::ir_optimize) fn plant_field_class_predicates(
        graph: &mut Graph,
        despec: &DespecRegistry,
        method_key: &str,
        headers: &FxHashSet<NodeId>,
        census: &mut W7dCensus,
    ) -> usize {
        if !field_class_enabled() || headers.is_empty() || graph.safepoints.is_empty() {
            return 0;
        }
        let dbg = super::super::loop_pass_report_enabled();
        let osr_graph = graph.osr_compile || graph.optimize_osr_route;
        let mut guards = 0usize;
        let mut planted_loops: FxHashSet<NodeId> = FxHashSet::default();
        // A read this pass planted -> its class guard: a proof of non-null and
        // of the class, which a later round's read through it names as token.
        let mut classed: FxHashMap<NodeId, NodeId> = FxHashMap::default();
        for round in 0..MAX_FIELD_CLASS_ROUNDS {
            let mut planted_round = 0usize;
            for (region, entry_pred, back_ctrls) in loop_headers(graph) {
                if !headers.contains(&region) || entry_pred == NO_NODE {
                    continue;
                }
                if guards >= MAX_FIELD_CLASS_GUARDS {
                    break;
                }
                let first = round == 0;
                if first {
                    census.field_loops += 1;
                }
                let refuse = |why: &str| {
                    if dbg {
                        eprintln!(
                            "[ir-licm] seq charAt field class predicate: {method_key} header \
                             {region} round {round}: not planted: {why}"
                        );
                    }
                };
                let Some(header_bci) = graph.node_opt(region).and_then(|r| r.bytecode_pc) else {
                    continue;
                };
                let users = build_users(graph);
                let body = loop_body_with(graph, &users, region, entry_pred, &back_ctrls);
                if body.contains(&entry_pred) {
                    continue;
                }
                let spliced = spliced_header(graph, region);
                if osr_graph
                    && !entered_only_through_pre_header(graph, region, entry_pred, &body, spliced)
                {
                    if first {
                        census.field_refused_entry += 1;
                    }
                    refuse("an OSR entry may bypass the pre-header");
                    continue;
                }
                let plans = match field_class_plans(graph, &users, region, &body, &classed) {
                    Ok(plans) => plans,
                    Err(FieldRefusal::Candidate(why)) => {
                        if first {
                            census.field_refused_candidate += 1;
                        }
                        refuse(why);
                        continue;
                    }
                    Err(FieldRefusal::Writer(why)) => {
                        if first {
                            census.field_refused_writer += 1;
                        }
                        refuse(why);
                        continue;
                    }
                };
                let Some(entry_slot) = graph
                    .node_opt(region)
                    .and_then(|r| r.inputs.iter().position(|&p| p == entry_pred))
                    .map(|p| p + 1)
                else {
                    continue;
                };
                let frame = match entry_frame(graph, region, entry_slot, &body, spliced) {
                    Some((bci, frame)) if bci == header_bci => frame,
                    _ => {
                        if first {
                            census.field_refused_entry += 1;
                        }
                        refuse("no loop-entry frame");
                        continue;
                    }
                };
                if withdrawn(despec, method_key, header_bci, &frame, PredicateKind::Class) {
                    if first {
                        census.field_refused_entry += 1;
                    }
                    refuse("the header's class speculation was withdrawn");
                    continue;
                }
                let n = install_field_class_plans(
                    graph,
                    region,
                    entry_pred,
                    header_bci,
                    frame,
                    plans,
                    MAX_FIELD_CLASS_GUARDS - guards,
                    spliced,
                    &mut classed,
                    census,
                );
                if n > 0 {
                    guards += n;
                    planted_round += n;
                    planted_loops.insert(region);
                    if dbg {
                        eprintln!(
                            "[ir-licm] seq charAt field class predicate: {method_key} header \
                             {region} bci {header_bci} round {round}: planted {n} guard(s)"
                        );
                    }
                }
            }
            if planted_round == 0 {
                break;
            }
            super::super::prune_constant_ifs(graph);
            super::super::scalar_cleanup(graph);
        }
        census.field_guards += guards;
        census.field_planted_loops += planted_loops.len();
        guards
    }

    /// The qualifying keys of the loop at `region` (module notes), or why
    /// there are none.
    fn field_class_plans(
        graph: &Graph,
        users: &[Vec<NodeId>],
        region: NodeId,
        body: &FxHashSet<NodeId>,
        classed: &FxHashMap<NodeId, NodeId>,
    ) -> Result<Vec<FieldPlan>, FieldRefusal> {
        let at = |id: NodeId| users.get(id as usize).map(Vec::as_slice).unwrap_or(&[]);
        let mut ids: Vec<NodeId> = body.iter().copied().collect();
        ids.sort_unstable();
        // The full-form reference field reads of the body, by key.
        let mut reads: Vec<(FieldKey, NodeId, Vec<NodeId>)> = Vec::new();
        for &id in &ids {
            let Some(n) = graph.node_opt(id) else {
                continue;
            };
            if n.op != Op::Load(MemKind::Ref)
                || n.ty != IrType::Ref
                || n.is_volatile_access()
                || n.inputs.len() != 4
            {
                continue;
            }
            let (base, idx_node) = (n.inputs[2], n.inputs[3]);
            let idx = match graph.node_opt(idx_node).map(|x| &x.op) {
                Some(Op::Const(v)) => *v,
                _ => continue,
            };
            let Some(&decl) = graph.field_decl_class.get(&id) else {
                continue;
            };
            let key = FieldKey { base, idx, decl };
            match reads.iter_mut().find(|r| r.0 == key) {
                Some(r) => r.2.push(id),
                None => reads.push((key, idx_node, vec![id])),
            }
        }
        if reads.is_empty() {
            return Err(FieldRefusal::Candidate(
                "no reference field read in the loop",
            ));
        }
        let mut plans: Vec<FieldPlan> = Vec::new();
        let mut why = "no field read a guarded splice class-tests";
        'keys: for (key, idx_node, key_reads) in reads {
            let Some(bn) = graph.node_opt(key.base) else {
                continue;
            };
            if bn.ty != IrType::Ref
                || matches!(
                    bn.op,
                    Op::New { .. }
                        | Op::NewArray { .. }
                        | Op::MultiNewArray { .. }
                        | Op::NewCp { .. }
                        | Op::Dead
                )
                || body.contains(&key.base)
                || !is_loop_invariant(graph, key.base, region, body)
                || !lff::base_outside_ea(graph, key.base)
                || (!classed.contains_key(&key.base) && lff::class_tested_in(graph, key.base, &ids))
            {
                why = "a read's base varies in the loop or the loop tests its class";
                continue;
            }
            let mut class: Option<u32> = None;
            let mut tests: Vec<NodeId> = Vec::new();
            let mut null_tests: Vec<NodeId> = Vec::new();
            let mut splice_ifs = 0usize;
            for &r in &key_reads {
                for &u in at(r) {
                    let Some(un) = graph.node_opt(u) else {
                        continue;
                    };
                    match un.op {
                        Op::ExactClassIs { class_id } if un.input_opt(0) == Some(r) => {
                            if class_id == 0 || class.is_some_and(|c| c != class_id) {
                                why = "a read tested against two classes";
                                continue 'keys;
                            }
                            class = Some(class_id);
                            for &t in at(u) {
                                let Some(tn) = graph.node_opt(t) else {
                                    continue;
                                };
                                if tn.op == Op::Dead {
                                    continue;
                                }
                                let names_test = tn.inputs.len() == 2 && tn.input_opt(1) == Some(u);
                                match tn.op {
                                    Op::If if names_test && miss_edge_calls(graph, users, t) => {
                                        tests.push(t);
                                        splice_ifs += 1;
                                    }
                                    Op::Guard { .. } if names_test => tests.push(t),
                                    _ => {
                                        why = "a class test of the read is not a guarded splice's";
                                        continue 'keys;
                                    }
                                }
                            }
                        }
                        Op::Cmp(CmpOp::Ne) => {
                            let other = if un.input_opt(0) == Some(r) {
                                un.input_opt(1)
                            } else {
                                un.input_opt(0)
                            };
                            if !other
                                .and_then(|o| graph.node_opt(o))
                                .is_some_and(crate::ir::is_null_const)
                            {
                                continue;
                            }
                            for &t in at(u) {
                                if graph.node_opt(t).is_some_and(|tn| {
                                    matches!(tn.op, Op::Guard { .. })
                                        && tn.inputs.len() == 2
                                        && tn.input_opt(1) == Some(u)
                                }) {
                                    null_tests.push(t);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            let Some(class_id) = class else {
                continue;
            };
            if splice_ifs == 0 {
                continue;
            }
            tests.sort_unstable();
            tests.dedup();
            null_tests.sort_unstable();
            null_tests.dedup();
            plans.push(FieldPlan {
                key,
                idx_node,
                class_id,
                reads: key_reads,
                tests,
                null_tests,
            });
        }
        if plans.is_empty() {
            return Err(FieldRefusal::Candidate(why));
        }
        // The miss-arm calls of the guarded splices about to fold, by plan.
        let mut miss_calls: FxHashMap<NodeId, usize> = FxHashMap::default();
        for (pi, p) in plans.iter().enumerate() {
            for &t in &p.tests {
                if graph.node_opt(t).map(|n| &n.op) != Some(&Op::If) {
                    continue;
                }
                for &pj in at(t) {
                    if !graph
                        .node_opt(pj)
                        .is_some_and(|pn| pn.op == Op::Proj(1) && pn.ty == IrType::Control)
                    {
                        continue;
                    }
                    for &c in at(pj) {
                        if graph.node_opt(c).is_some_and(|cn| {
                            matches!(cn.op, Op::Call { .. }) && cn.input_opt(0) == Some(pj)
                        }) {
                            miss_calls.insert(c, pi);
                        }
                    }
                }
            }
        }
        // JIT r25 wave 12 (W12-4, `CRATONVM_JIT_IR_CHAIN_INNER_LOOP_PROOFS`):
        // and a call the miss projection reaches through straight-line
        // control (guards, no merge): it is as dead as the projection once
        // the test folds.
        if super::super::r25_w12_4::inner_loop_proofs_enabled() {
            for &id in &ids {
                if miss_calls.contains_key(&id)
                    || !graph
                        .node_opt(id)
                        .is_some_and(|n| matches!(n.op, Op::Call { .. }))
                {
                    continue;
                }
                if let Some(pi) = super::super::r25_w12_4::miss_arm_plan(graph, id, |t| {
                    plans.iter().position(|p| p.tests.contains(&t))
                }) {
                    miss_calls.insert(id, pi);
                }
            }
        }
        // Every writer of the loop.
        let mut written = vec![false; plans.len()];
        for &id in &ids {
            let Some(n) = graph.node_opt(id) else {
                continue;
            };
            if n.op == Op::Dead || n.op == Op::Phi || n.op.is_control() || lff::benign_opaque(&n.op)
            {
                continue;
            }
            if n.is_volatile_access() {
                return Err(FieldRefusal::Writer("a volatile access"));
            }
            if matches!(n.op, Op::Call { .. })
                && (miss_calls.contains_key(&id)
                    || super::super::charat_trap::seq_charat_traps_on_miss(graph, id))
            {
                continue;
            }
            let eff = graph.memory_effect(id);
            if eff.order != MemOrder::Plain {
                return Err(FieldRefusal::Writer(
                    "an ordering node (a monitor, a fence)",
                ));
            }
            match &eff.writes {
                AliasClass::Any => {
                    // JIT r25 wave 12 (W12-4): name the writer.
                    if super::super::loop_pass_report_enabled() {
                        eprintln!(
                            "[ir-licm] seq charAt field class predicate: header {region}: the \
                             opaque writer is {}",
                            super::super::r25_w12_4::describe_node(graph, id)
                        );
                    }
                    return Err(FieldRefusal::Writer(
                        "an opaque writer (a call) outside a folded miss arm",
                    ));
                }
                AliasClass::Monitor { .. } => {
                    return Err(FieldRefusal::Writer("a monitor"));
                }
                AliasClass::Field { offset, decl, .. } => {
                    let decl = decl.or_else(|| graph.field_decl_class.get(&id).copied());
                    for (pi, p) in plans.iter().enumerate() {
                        let other_index =
                            matches!(offset, AccessOffset::Const(v) if *v != p.key.idx);
                        let other_field = decl.is_some_and(|d| d != p.key.decl);
                        if !(other_index || other_field) {
                            written[pi] = true;
                        }
                    }
                }
                _ => {}
            }
        }
        if miss_calls.values().any(|&pi| written[pi]) {
            return Err(FieldRefusal::Writer(
                "the loop writes a tested field, so its miss arm's call stays",
            ));
        }
        let plans: Vec<FieldPlan> = plans
            .into_iter()
            .enumerate()
            .filter(|(pi, _)| !written[*pi])
            .map(|(_, p)| p)
            .collect();
        if plans.is_empty() {
            return Err(FieldRefusal::Writer("the loop writes every tested field"));
        }
        Ok(plans)
    }

    /// Plant the pre-header read and class guard of each plan (at most
    /// `budget`), fold its tests and answer its reads. Returns the class
    /// guards planted; nothing at all when the header has no single memory
    /// phi.
    #[allow(clippy::too_many_arguments)]
    fn install_field_class_plans(
        graph: &mut Graph,
        region: NodeId,
        entry_pred: NodeId,
        header_bci: usize,
        frame: SafepointSnapshot,
        plans: Vec<FieldPlan>,
        budget: usize,
        spliced: bool,
        classed: &mut FxHashMap<NodeId, NodeId>,
        census: &mut W7dCensus,
    ) -> usize {
        if budget == 0 {
            return 0;
        }
        let Some(fwd) = graph
            .node_opt(region)
            .and_then(|r| r.inputs.iter().position(|&p| p == entry_pred))
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
            return 0;
        };
        let header_mem = *header_mem;
        let Some(mut mem) = graph
            .node_opt(header_mem)
            .and_then(|n| n.input_opt(fwd + 1))
        else {
            return 0;
        };
        let Ok(si) = u32::try_from(graph.safepoints.len()) else {
            return 0;
        };
        graph.push_safepoint(frame);
        let hpc = Some(header_bci);
        let one = graph.add(Op::Const(1), IrType::Int, vec![], None);
        let mut null: Option<NodeId> = None;
        let mut planted = 0usize;
        for p in plans {
            if planted >= budget {
                break;
            }
            let base = p.key.base;
            // A proof `base` is not null at the entry, or a null guard (the
            // null-base predicate's shape: a zero-trip loop entered with a
            // null base deopts to the header and the interpreter runs it).
            let mut token = classed.get(&base).copied().unwrap_or(NO_NODE);
            let known_non_null = token != NO_NODE
                || graph
                    .node_opt(base)
                    .is_some_and(|n| crate::ir_check_elim::definitely_non_null(&n.op))
                || lff::is_receiver_param(graph, base)
                || lff::base_null_checked_before(graph, base, entry_pred);
            let mut null_guard = NO_NODE;
            if !known_non_null {
                let nc =
                    *null.get_or_insert_with(|| graph.add(Op::Const(0), IrType::Ref, vec![], None));
                let ne = graph.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![base, nc], None);
                let g = graph.add(
                    Op::Guard { bci: header_bci },
                    IrType::Void,
                    vec![entry_pred, ne],
                    hpc,
                );
                if !graph.set_node_frame_snapshot(g, si) {
                    graph.kill(g);
                    graph.kill(ne);
                    continue;
                }
                null_guard = g;
                token = g;
            }
            // The pre-header read: the in-loop reads' form, at the header's
            // pc with the loop-entry frame, threaded on the forward memory
            // edge, ordered after its licence by the guard token.
            let mut inputs = vec![entry_pred, mem, base, p.idx_node];
            if token != NO_NODE {
                inputs.push(token);
            }
            let read = graph.add(Op::Load(MemKind::Ref), IrType::Ref, inputs, hpc);
            graph.field_decl_class.insert(read, p.key.decl);
            if let Some(&sample) = p.reads.first() {
                lff::inherit_field_row(graph, sample, read);
            }
            // The header's bci is not this field's `getfield`.
            graph.synthetic_field_access_pcs.insert(header_bci);
            let _ = graph.set_node_frame_snapshot(read, si);
            // No pc on the test: only the guard that names the frame anchors
            // the header's bci in the pre-header (`class_predicate`'s rule).
            let is = graph.add(
                Op::ExactClassIs {
                    class_id: p.class_id,
                },
                IrType::Int,
                vec![read],
                None,
            );
            let guard = graph.add(
                Op::Guard { bci: header_bci },
                IrType::Void,
                vec![entry_pred, is],
                hpc,
            );
            if !graph.set_node_frame_snapshot(guard, si) {
                graph.kill(guard);
                graph.kill(is);
                graph.kill(read);
                if null_guard != NO_NODE {
                    graph.kill(null_guard);
                }
                continue;
            }
            if null_guard != NO_NODE {
                tag(graph, null_guard, PredicateKind::Class, spliced);
            }
            tag(graph, guard, PredicateKind::Class, spliced);
            mem = read;
            classed.insert(read, guard);
            for &t in &p.tests {
                if graph.set_input(t, 1, one) {
                    census.field_folded += 1;
                }
            }
            for &t in &p.null_tests {
                let _ = graph.set_input(t, 1, one);
            }
            for &r in &p.reads {
                let Some(token_in) = graph
                    .node_opt(r)
                    .and_then(|n| crate::ir::memory_token_slot(n).and_then(|t| n.input_opt(t)))
                else {
                    // Kept: it still reads `p0`'s value, and its tests are
                    // folded by the same induction.
                    continue;
                };
                // Token users first: they want the chain, not the value.
                for user in graph.users_of(r) {
                    let Some(un) = graph.node_opt(user) else {
                        continue;
                    };
                    let slots: Vec<usize> = un
                        .inputs
                        .as_slice()
                        .iter()
                        .enumerate()
                        .filter(|&(i, &x)| x == r && crate::ir::is_memory_token_slot(un, i))
                        .map(|(i, _)| i)
                        .collect();
                    for slot in slots {
                        graph.set_input(user, slot, token_in);
                    }
                }
                graph.replace_all_uses(r, read);
                graph.kill(r);
                census.field_reads += 1;
            }
            planted += 1;
        }
        if planted > 0 {
            graph.set_input(header_mem, fwd + 1, mem);
        } else {
            graph.kill(one);
            if let Some(nc) = null {
                graph.kill(nc);
            }
            let keep: Vec<bool> = (0..graph.safepoints.len())
                .map(|i| i != si as usize)
                .collect();
            let _ = graph.retain_safepoints(&keep);
        }
        planted
    }

    // ── JIT rx27 wave 4 (lane scanloop, P-1): identity class tests ──
    //
    // `CRATONVM_JIT_IR_SEQ_CHARAT_IDENTITY_TESTS` (opt-in `=1`), page
    // `docs/feature-designs/jit-ir-optimizations.md`.
    //
    // # What it answers
    //
    // In `Pattern$Start.match`'s position loop every iteration reads
    // `this.next`, class-tests it (the guarded `Greedy` splice), reads
    // `next.predicate` and class-tests that (the lambda splice). P2 above
    // would move those reads to the entry, but its induction needs the loop to
    // write neither field, and the greedy's kept back-off `next.match` call is
    // an opaque writer on a path to the back edge. So P2 refuses the loop.
    //
    // This arm needs no writer analysis. It leaves every read where it is
    // and replaces only the CLASS TEST: at the loop entry it reads the same
    // chain once (`P1 = root.f1`, `P2 = P1.f2`, ...) and guards each entry
    // value's exact class (`ExactClassIs(Pk, Ck)`, deopt to the loop entry
    // with the loop-entry frame, charged as a class predicate). Then each
    // in-loop test `ExactClassIs(x, Ck)` of a read `x` of that key becomes
    // `x == Pk`.
    //
    // # Why it is exact
    //
    // `Pk`'s class is `Ck` (its guard dominates the loop). So `x == Pk`
    // implies `x`'s class is `Ck`: the new test is true only where the old one
    // was. Where the old test was true and the new one is false (a node graph
    // changed under the loop), the `If` takes its miss arm, the virtual call
    // the splice replaced, which is correct for any receiver; a `Guard` deopts
    // at its own frame, as it would for a wrong class. Nothing else changes:
    // the reads stay in the loop and see every write, so the JMM and the
    // calls in the loop are untouched. The entry reads only read: the root
    // is non-null (a receiver, a proven value, or a null guard first, as P2
    // does) and a deeper read's base `P(k-1)` is non-null and of the class the
    // in-loop read's field index belongs to, by its own guard.
    //
    // # What it saves
    //
    // Per test: the class test (null test, kind tag, class id, and unless
    // `CRATONVM_JIT_IR_FUSED_CLASS_TEST_BRANCH` the 0/1 materialisation, its
    // home store and reload) becomes one compare, which the default compare
    // fusion branches on. That covers the j27 box's item (b) and the class
    // half of item (a). The reads themselves stay while the back-off call
    // stays a call (P-2, chain-splice admission, another lane).

    /// `CRATONVM_JIT_IR_SEQ_CHARAT_IDENTITY_TESTS` (JIT rx27 wave 4, lane
    /// scanloop) -- **default on** (`=1`). Read per compile.
    pub(in crate::ir_optimize) fn identity_tests_enabled() -> bool {
        true
    }

    /// At most this many entry reads (and class guards) per loop.
    const MAX_IDENTITY_KEYS: usize = 8;

    /// One field cell of the chain: the root (a value defined outside the
    /// loop) or another key's entry value, then a field.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum IdBase {
        Root(NodeId),
        Key(usize),
    }

    struct IdKey {
        base: IdBase,
        idx: i64,
        idx_node: NodeId,
        decl: u32,
        class_id: u32,
        /// One in-loop read of the key, whose field row the entry read takes.
        sample: NodeId,
        /// `(test, read)`: an `If` or `Guard` on `ExactClassIs(read, class)`.
        tests: Vec<(NodeId, NodeId)>,
    }

    /// What [`plant_identity_tests`] did in one loop, for its line.
    #[derive(Default, Debug, Clone, PartialEq, Eq)]
    pub(in crate::ir_optimize) struct IdentityCensus {
        pub(in crate::ir_optimize) keys: usize,
        pub(in crate::ir_optimize) entry_guards: usize,
        pub(in crate::ir_optimize) ifs: usize,
        pub(in crate::ir_optimize) guards: usize,
        pub(in crate::ir_optimize) refused: Option<&'static str>,
    }

    /// The class-tested reference reads of `body` whose base chain ends at a
    /// value defined outside the loop, grouped by key, parents before
    /// children.
    fn identity_keys(graph: &Graph, region: NodeId, body: &FxHashSet<NodeId>) -> Vec<IdKey> {
        let mut ids: Vec<NodeId> = body.iter().copied().collect();
        ids.sort_unstable();
        // read -> (class, its tests)
        let mut tested: FxHashMap<NodeId, (u32, Vec<NodeId>)> = FxHashMap::default();
        let mut bad: FxHashSet<NodeId> = FxHashSet::default();
        for &t in &ids {
            let Some(tn) = graph.node_opt(t) else {
                continue;
            };
            if !matches!(tn.op, Op::If | Op::Guard { .. }) || tn.inputs.len() != 2 {
                continue;
            }
            let Some(c) = graph.node_opt(tn.inputs[1]) else {
                continue;
            };
            let Op::ExactClassIs { class_id } = c.op else {
                continue;
            };
            let Some(x) = c.input_opt(0) else {
                continue;
            };
            if class_id == 0 || x == NO_NODE || !body.contains(&x) {
                continue;
            }
            let e = tested.entry(x).or_insert((class_id, Vec::new()));
            if e.0 != class_id {
                bad.insert(x);
            }
            e.1.push(t);
        }
        let field = |x: NodeId| -> Option<(NodeId, NodeId, i64, u32)> {
            let n = graph.node_opt(x)?;
            if n.op != Op::Load(MemKind::Ref)
                || n.ty != IrType::Ref
                || n.is_volatile_access()
                || n.inputs.len() < 4
            {
                return None;
            }
            let (base, idx_node) = (n.inputs[2], n.inputs[3]);
            let idx = match graph.node_opt(idx_node).map(|i| &i.op) {
                Some(Op::Const(v)) => *v,
                _ => return None,
            };
            let decl = *graph.field_decl_class.get(&x)?;
            Some((base, idx_node, idx, decl))
        };
        let root_ok = |b: NodeId| {
            graph.node_opt(b).is_some_and(|bn| {
                bn.ty == IrType::Ref
                    && !matches!(
                        bn.op,
                        Op::New { .. }
                            | Op::NewArray { .. }
                            | Op::MultiNewArray { .. }
                            | Op::NewCp { .. }
                            | Op::Dead
                    )
            }) && !body.contains(&b)
                && is_loop_invariant(graph, b, region, body)
                && lff::base_outside_ea(graph, b)
        };
        // A field the loop itself stores to changes under it by design (a
        // cursor), so its identity test would send every iteration to the
        // miss arm: such a key is not taken. Calls are not screened (the
        // node graph a regex loop walks is written only while compiling).
        let stored = |idx: i64, decl: u32| {
            ids.iter().any(|&id| {
                matches!(
                    &graph.memory_effect(id).writes,
                    AliasClass::Field { offset, decl: d, .. }
                        if !matches!(offset, AccessOffset::Const(v) if *v != idx)
                            && !d.or_else(|| graph.field_decl_class.get(&id).copied())
                                .is_some_and(|d| d != decl)
                )
            })
        };
        let mut keys: Vec<IdKey> = Vec::new();
        // read -> its key
        let mut key_of: FxHashMap<NodeId, usize> = FxHashMap::default();
        // Parents first: a read's base is a root or an earlier-keyed read.
        // The chain is short (`this.next.predicate.arg`), so a few passes.
        let mut reads: Vec<NodeId> = tested
            .keys()
            .copied()
            .filter(|x| !bad.contains(x))
            .collect();
        reads.sort_unstable();
        for _ in 0..4 {
            let mut progress = false;
            for &x in &reads {
                if key_of.contains_key(&x) {
                    continue;
                }
                let Some((base, idx_node, idx, decl)) = field(x) else {
                    continue;
                };
                if stored(idx, decl) {
                    continue;
                }
                let base = if let Some(&k) = key_of.get(&base) {
                    IdBase::Key(k)
                } else if root_ok(base) {
                    IdBase::Root(base)
                } else {
                    continue;
                };
                let (class_id, tests) = &tested[&x];
                let found = keys
                    .iter()
                    .position(|k| k.base == base && k.idx == idx && k.decl == decl);
                let k = match found {
                    Some(k) if keys[k].class_id == *class_id => k,
                    Some(_) => continue,
                    None => {
                        keys.push(IdKey {
                            base,
                            idx,
                            idx_node,
                            decl,
                            class_id: *class_id,
                            sample: x,
                            tests: Vec::new(),
                        });
                        keys.len() - 1
                    }
                };
                keys[k].tests.extend(tests.iter().map(|&t| (t, x)));
                key_of.insert(x, k);
                progress = true;
            }
            if !progress {
                break;
            }
        }
        keys
    }

    /// Plant the identity tests the section above describes in the loops of
    /// `headers` (outer loops first: a nested loop's tests are in its outer
    /// loop's body, and are rewritten there). Returns the tests rewritten.
    pub(in crate::ir_optimize) fn plant_identity_tests(
        graph: &mut Graph,
        despec: &DespecRegistry,
        method_key: &str,
        headers: &FxHashSet<NodeId>,
    ) -> usize {
        if !identity_tests_enabled() || headers.is_empty() || graph.safepoints.is_empty() {
            return 0;
        }
        let osr_graph = graph.osr_compile || graph.optimize_osr_route;
        let mut loops: Vec<(usize, (NodeId, NodeId, Vec<NodeId>))> = {
            let g: &Graph = graph;
            let users = build_users(g);
            loop_headers(g)
                .into_iter()
                .filter(|l| headers.contains(&l.0) && l.1 != NO_NODE)
                .map(|l| (loop_body_with(g, &users, l.0, l.1, &l.2).len(), l))
                .collect()
        };
        loops.sort_by(|a, b| b.0.cmp(&a.0).then(a.1 .0.cmp(&b.1 .0)));
        let mut rewritten = 0usize;
        for (_, (region, entry_pred, back_ctrls)) in loops {
            let mut census = IdentityCensus::default();
            identity_tests_in_loop(
                graph,
                despec,
                method_key,
                region,
                entry_pred,
                &back_ctrls,
                osr_graph,
                &mut census,
            );
            rewritten += census.ifs + census.guards;
            note_identity_census(graph, method_key, region, &census);
        }
        rewritten
    }

    #[allow(clippy::too_many_arguments)]
    fn identity_tests_in_loop(
        graph: &mut Graph,
        despec: &DespecRegistry,
        method_key: &str,
        region: NodeId,
        entry_pred: NodeId,
        back_ctrls: &[NodeId],
        osr_graph: bool,
        census: &mut IdentityCensus,
    ) {
        let Some(header_bci) = graph.node_opt(region).and_then(|r| r.bytecode_pc) else {
            census.refused = Some("the header has no bci");
            return;
        };
        let users = build_users(graph);
        let body = loop_body_with(graph, &users, region, entry_pred, back_ctrls);
        if body.contains(&entry_pred) {
            census.refused = Some("no single entry control outside the body");
            return;
        }
        let spliced = spliced_header(graph, region);
        if osr_graph && !entered_only_through_pre_header(graph, region, entry_pred, &body, spliced)
        {
            census.refused = Some("an OSR entry may bypass the pre-header");
            return;
        }
        // Only tests not yet rewritten count (an outer loop's pass already
        // took them: their condition is no longer an `ExactClassIs`). Parents
        // come first, so the cut drops children only.
        let mut keys = identity_keys(graph, region, &body);
        keys.truncate(MAX_IDENTITY_KEYS);
        if keys.is_empty() {
            census.refused = Some("no class-tested read with an invariant base chain");
            return;
        }
        let Some(entry_slot) = graph
            .node_opt(region)
            .and_then(|r| r.inputs.iter().position(|&p| p == entry_pred))
            .map(|p| p + 1)
        else {
            census.refused = Some("no entry slot");
            return;
        };
        let frame = match entry_frame(graph, region, entry_slot, &body, spliced) {
            Some((bci, frame)) if bci == header_bci => frame,
            _ => {
                census.refused = Some("no loop-entry frame");
                return;
            }
        };
        if withdrawn(despec, method_key, header_bci, &frame, PredicateKind::Class) {
            census.refused = Some("the header's class speculation was withdrawn");
            return;
        }
        // The header's one memory phi: the entry reads thread its forward
        // edge, as P2's do.
        let fwd = entry_slot - 1;
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
            census.refused = Some("the header has no single memory phi");
            return;
        };
        let header_mem = *header_mem;
        let Some(mut mem) = graph
            .node_opt(header_mem)
            .and_then(|n| n.input_opt(fwd + 1))
        else {
            census.refused = Some("no forward memory edge");
            return;
        };
        let Ok(si) = u32::try_from(graph.safepoints.len()) else {
            return;
        };
        graph.push_safepoint(frame);
        let hpc = Some(header_bci);
        // key -> (entry read, its class guard)
        let mut entry: Vec<Option<(NodeId, NodeId)>> = vec![None; keys.len()];
        let mut null: Option<NodeId> = None;
        for (k, key) in keys.iter().enumerate() {
            let (base, mut token) = match key.base {
                IdBase::Root(b) => (b, NO_NODE),
                IdBase::Key(p) => match entry[p] {
                    Some((read, guard)) => (read, guard),
                    None => continue,
                },
            };
            if let IdBase::Root(b) = key.base {
                let known_non_null = graph
                    .node_opt(b)
                    .is_some_and(|n| crate::ir_check_elim::definitely_non_null(&n.op))
                    || lff::is_receiver_param(graph, b)
                    || lff::base_null_checked_before(graph, b, entry_pred);
                if !known_non_null {
                    let nc = *null
                        .get_or_insert_with(|| graph.add(Op::Const(0), IrType::Ref, vec![], None));
                    let ne = graph.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![b, nc], None);
                    let g = graph.add(
                        Op::Guard { bci: header_bci },
                        IrType::Void,
                        vec![entry_pred, ne],
                        hpc,
                    );
                    if !graph.set_node_frame_snapshot(g, si) {
                        graph.kill(g);
                        graph.kill(ne);
                        continue;
                    }
                    tag(graph, g, PredicateKind::Class, spliced);
                    census.entry_guards += 1;
                    token = g;
                }
            }
            let mut inputs = vec![entry_pred, mem, base, key.idx_node];
            if token != NO_NODE {
                inputs.push(token);
            }
            let read = graph.add(Op::Load(MemKind::Ref), IrType::Ref, inputs, hpc);
            graph.field_decl_class.insert(read, key.decl);
            lff::inherit_field_row(graph, key.sample, read);
            graph.synthetic_field_access_pcs.insert(header_bci);
            let _ = graph.set_node_frame_snapshot(read, si);
            let is = graph.add(
                Op::ExactClassIs {
                    class_id: key.class_id,
                },
                IrType::Int,
                vec![read],
                None,
            );
            let guard = graph.add(
                Op::Guard { bci: header_bci },
                IrType::Void,
                vec![entry_pred, is],
                hpc,
            );
            if !graph.set_node_frame_snapshot(guard, si) {
                graph.kill(guard);
                graph.kill(is);
                graph.kill(read);
                continue;
            }
            tag(graph, guard, PredicateKind::Class, spliced);
            mem = read;
            entry[k] = Some((read, guard));
            census.keys += 1;
            census.entry_guards += 1;
            for &(t, x) in &key.tests {
                let is_if = graph.node_opt(t).is_some_and(|n| n.op == Op::If);
                let same = graph.add(Op::Cmp(CmpOp::Eq), IrType::Int, vec![x, read], None);
                if graph.set_input(t, 1, same) {
                    if is_if {
                        census.ifs += 1;
                    } else {
                        census.guards += 1;
                    }
                } else {
                    graph.kill(same);
                }
            }
        }
        if census.keys > 0 {
            graph.set_input(header_mem, fwd + 1, mem);
        } else {
            let keep: Vec<bool> = (0..graph.safepoints.len())
                .map(|i| i != si as usize)
                .collect();
            let _ = graph.retain_safepoints(&keep);
            census.refused = Some("no entry read could be planted");
        }
    }

    /// One `CRATONVM_DBG_JITC` line per offered loop.
    fn note_identity_census(graph: &Graph, method_key: &str, region: NodeId, c: &IdentityCensus) {
        if !crate::x64::dbg_jitc_enabled() {
            return;
        }
        eprintln!(
            "[cratonvm-jitc] ir-seq-charat-identity-tests {} header n{region} bci {:?}: keys={} \
             entry guards={} ifs={} guards={}{}",
            if method_key.is_empty() {
                "?"
            } else {
                method_key
            },
            graph.node_opt(region).and_then(|r| r.bytecode_pc),
            c.keys,
            c.entry_guards,
            c.ifs,
            c.guards,
            c.refused
                .map(|r| format!(" refused: {r}"))
                .unwrap_or_default(),
        );
    }

    // ── W6C-P3: the entry bound ──

    /// One hoisted call's in-body bound guard and what answers it.
    struct BoundCandidate {
        /// `Guard{bci: call}(i < count)`.
        bound_guard: NodeId,
        count: NodeId,
        /// The invariant `x` of the dominating `i < x` / `i <= x` edge.
        x: NodeId,
        /// `i < x` (`true`) or `i <= x`.
        strict: bool,
        /// `(Guard{bci: call}(0 <= i), init)` when `i` cannot go negative.
        non_negative: Option<(NodeId, NodeId)>,
    }



    fn is_int_const(graph: &Graph, id: NodeId, v: i64) -> bool {
        graph
            .node_opt(id)
            .is_some_and(|n| n.ty == IrType::Int && n.op == Op::Const(v))
    }

    /// Walking up from `ctrl` through `If` projections inside the loop: the
    /// first edge stating `idx < x` (`strict`) or `idx <= x` for an invariant
    /// int `x` defined outside the loop, and whether that edge is the header's
    /// own first test whose other edge leaves the loop.
    fn deciding_edge(
        graph: &Graph,
        region: NodeId,
        body: &FxHashSet<NodeId>,
        ctrl: NodeId,
        idx: NodeId,
    ) -> Option<(NodeId, bool, bool)> {
        let mut cur = ctrl;
        for _ in 0..64 {
            let n = graph.node_opt(cur)?;
            // JIT r25 wave 12 (W12-4): a guard on the way up only narrows the
            // path; it dominates nothing the edge above it does not.
            if matches!(n.op, Op::Guard { .. })
                && super::super::r25_w12_4::inner_loop_proofs_enabled()
            {
                cur = n.input_opt(0)?;
                if cur == region || !body.contains(&cur) {
                    return None;
                }
                continue;
            }
            let Op::Proj(k) = n.op else {
                return None;
            };
            let iff = n.input_opt(0)?;
            let ifn = graph.node_opt(iff)?;
            if ifn.op != Op::If {
                return None;
            }
            let (if_ctrl, cond) = (ifn.input_opt(0)?, ifn.input_opt(1)?);
            if let Some((x, strict)) = implied_upper_bound(graph, cond, k == 0, idx) {
                if !body.contains(&x)
                    && graph.node_opt(x).is_some_and(|xn| xn.ty == IrType::Int)
                    && is_loop_invariant(graph, x, region, body)
                {
                    let leaves = graph
                        .users_of(iff)
                        .into_iter()
                        .filter(|&p| p != cur)
                        .all(|p| !body.contains(&p));
                    return Some((x, strict, if_ctrl == region && leaves));
                }
            }
            if if_ctrl == region {
                return None;
            }
            cur = if_ctrl;
        }
        None
    }

    /// `(x, strict)` when the edge of `cond` taken on `truth` states
    /// `idx < x` (`strict`) or `idx <= x`.
    fn implied_upper_bound(
        graph: &Graph,
        cond: NodeId,
        truth: bool,
        idx: NodeId,
    ) -> Option<(NodeId, bool)> {
        let c = graph.node_opt(cond)?;
        let Op::Cmp(op) = c.op else {
            return None;
        };
        let [a, b] = c.inputs.as_slice() else {
            return None;
        };
        let op = if truth { op } else { op.negate() };
        if *a == idx {
            match op {
                CmpOp::Lt => Some((*b, true)),
                CmpOp::Le => Some((*b, false)),
                _ => None,
            }
        } else if *b == idx {
            match op {
                CmpOp::Gt => Some((*a, true)),
                CmpOp::Ge => Some((*a, false)),
                _ => None,
            }
        } else {
            None
        }
    }

}
