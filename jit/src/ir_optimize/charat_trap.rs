//! nio21 wave 5 (lane L5): `CharSequence.charAt` as a TRAP rather than a call
//! inside a class-predicated loop (`CRATONVM_JIT_IR_SEQ_CHARAT_TRAP`).
//!
//! # Why
//!
//! `UTF_8$Encoder.encodeBufferLoop` splices `StringCharBuffer.get()`, whose
//! `str.charAt(i)` is an `invokeinterface CharSequence`. The optimizing tier
//! answers it with the `SeqCharAt` call PREFIX
//! (`ir_lower::Lowerer::try_emit_ir_string_search_prefix`): a String receiver
//! and an in-range index are read inline, anything else DECLINES to the
//! interface call. In the IR the site therefore stays an `Op::Call`, whose
//! memory effect is `Any`, and `loop_field_forward` refuses every loop that
//! holds one -- so the buffers' `position` / `limit` / `hb` / `offset` / `str`
//! are re-read per char even after `class_predicate` has proven both buffers'
//! classes at the loop entry
//! (`r21-encode-loop-field-hoist-and-buffer-bce-need-a-call-free-loop-FIXED-20261012.md`).
//!
//! # What
//!
//! [`seq_charat_traps_on_miss`] names the `SeqCharAt` calls whose prefix
//! DEOPTS on a miss instead of calling: the call sits in the body of a loop
//! whose entry carries a `class_predicate` guard (the
//! `Guard{bci: header}(ExactClassIs(..))` that pass plants on the loop's entry
//! control). `class_predicate` plants no such guard for a loop holding a call
//! whose pc has no frame a deopt there can resume (see below), or once the
//! method has de-specialised the trap's site ([`class_predicate_withheld`]). For such a
//! call the lowering emits the prefix with every
//! decline edge landing on one deopt at the call's pc
//! (`DeoptReason::ReceiverTypeChanged`), and the field forward admits the call:
//! it then runs no Java and writes no memory, so it is a READ of the String's
//! `value` / `coder` and a possible exit, exactly like the guards the forward
//! already accepts.
//!
//! ONE predicate, asked by both sides (`loop_field_forward`'s census and the
//! lowering), so the forward can only rely on a call the lowering makes trap:
//! the store-clause pattern of `ir_lower::store_cannot_deopt`. The lowering
//! refuses the compile (a bailout, never a silent call) when it cannot emit the
//! trapping form for a call the predicate names.
//!
//! # Why it is sound
//!
//! * A miss runs nothing: the prefix's tests (null receiver, a non-String
//!   class, a `null` `value`, an index outside `[0, length)`, a layout epoch
//!   change) all precede any write, and the deopt resumes the interpreter AT the
//!   `invokeinterface` with the frame the builder recorded at the top of that pc
//!   (the chain snapshot inside a splice, whose callee frames are parked
//!   mid-`invoke`; the flat snapshot for a call of the compiling method itself).
//!   The interpreter re-runs the call, so every receiver gets the method the
//!   bytecode dispatches to, and an out-of-range index throws the JDK's own
//!   `StringIndexOutOfBoundsException`.
//! * The FLAT frame of the outermost invoke is never used for a call inside a
//!   splice: it would re-run the spliced body's committed stores (`nextGetIndex`
//!   has already advanced `position`). `class_predicate` requires a
//!   guard-capable chain snapshot at a pc any chain snapshot names, and the
//!   lowering checks that the chain resolves before it emits (and refuses the
//!   compile otherwise: a snapshot pruned after optimize, or a chain whose
//!   values cannot be published).
//! * A hit reads only `String.value` (final, its array never written after
//!   construction) and `coder`; the forward's argument (no synchronisation
//!   action, no write to a forwarded field in the loop) is unchanged.
//!
//! # Cost when the speculation is wrong
//!
//! A non-String receiver deopts at the call per miss, where the call used to
//! cost one compare. The predicate applies only inside a loop
//! `class_predicate` already speculated on (one profiled class per invariant
//! receiver), and `class_predicate` plants nothing for a loop holding such a
//! call once the VM has de-specialised the site the trap is charged at
//! ([`withheld_by_despec`]; per site since nio21 wave 6, the outermost
//! invoke of a spliced call's chain), so the next compile keeps the call.
//! The VM records that de-spec after `PER_BCI_DESPEC_LIMIT` (4) charged traps
//! at the site, on every sink that charges a trap of the body's own
//! (`deopt_resume::real_frame_deopt_resume_or_throw_and_despeculate`,
//! `helpers::despeculate_trapped_method`, and since nio21 wave 7 the doors'
//! re-run and IR-resume arms and the OSR door's rejected bail, through
//! `deopt_resume::despec_own_stash_site_after_charge`;
//! `r21-charat-trap-despec-skips-the-rerun-sinks-FIXED-20261012.md`).
//!
//! # Armed call-site exception edges
//!
//! A trapping call inside a `try` may also carry call-site exception edges
//! (`Op::CallThrew`, `CRATONVM_JIT_IR_CALL_SITE_EXCEPTION_EDGES`). The trap
//! makes no call, so those edges' pad is never entered, and an out-of-range
//! index deopts where the edge would have caught the JDK's
//! `StringIndexOutOfBoundsException` in compiled code. That is deliberate
//! (nio21 wave 7, lane L2; `r21-ir-charat-trap-deopts-a-call-whose-exception-edges-are-armed-FIXED-20261012.md`):
//! the predicate does NOT refuse such a call, because catch-all rows arm
//! edges too (`US_ASCII` / `ISO_8859_1`'s `encodeBufferLoop` loop inside
//! `try { .. } finally`, a row that covers the `charAt` compiled or spliced);
//! refusing would cost those encode loops their forwarding the day the edges
//! default on. The decline is charged as the trap's own
//! `ReceiverTypeChanged` at the site, so a loop that keeps indexing out of
//! range pays at most `PER_BCI_DESPEC_LIMIT` deopts before
//! [`withheld_by_despec`] keeps the call (and its armed edges) on the next
//! compile.

use super::{build_users, loop_body_with, loop_headers};
use crate::ir::{Graph, IrType, Node, NodeId, Op, NO_NODE};
use rustc_hash::FxHashSet;

/// `CRATONVM_JIT_IR_SEQ_CHARAT_TRAP` (nio21 wave 5, lane L5) -- **default
/// ON**, `=0` is the kill switch (the prefix then always declines to the call,
/// and the field forward refuses the loop, as in wave 4). Read per asked call.
pub(crate) fn seq_charat_trap_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_SEQ_CHARAT_TRAP")
}

/// Is `node` a live `CharSequence.charAt(I)C` interface call the optimizing
/// tier's `SeqCharAt` prefix answers (`crate::ir_string_search_prefix_kind`)?
/// The cheap name screen runs before the resolver.
pub(crate) fn is_seq_charat_call(node: &Node) -> bool {
    let Op::Call { info_ptr } = node.op else {
        return false;
    };
    if info_ptr == 0 || node.inputs.len() != 4 || node.ty != IrType::Int {
        return false;
    }
    // SAFETY: a non-zero `info_ptr` on an `Op::Call` addresses a
    // `JitInvokeInfo` the planner boxed and keeps alive on the artifact for the
    // life of the compiled method (tests leak theirs); the call lowering bakes
    // the same pointer (`ir::is_holds_lock_info_row` reads it the same way).
    let info: &crate::JitInvokeInfo = unsafe { &*(info_ptr as *const crate::JitInvokeInfo) };
    info.invoke_kind == 2
        && info.method_name == "charAt"
        && info.descriptor == "(I)C"
        && crate::ir_string_search_prefix_kind(info.class_name, info.method_name, info.descriptor)
            == Some(crate::IrStringSearchKind::SeqCharAt)
}

/// Does the graph hold a frame a deopt AT `pc` (re-executing the invoke) can
/// resume? Inside a splice (some snapshot at `pc` carries a scope chain, or
/// the build's splice map places `pc` in a relocated body) it must be a
/// guard-capable chain snapshot the lowering can publish; outside one, the
/// method's own flat snapshot at `pc`.
///
/// nio21 wave 6 (lane L5): the chain arm now also asks, from the graph
/// alone, the refusals `ir_lower::Lowerer::seq_charat_trap_frame_ok` would
/// otherwise meet only at lowering, where they cost the whole optimizing
/// compile (`refuse_seq_charat_trap`): a `pc` inside a splice with no chain
/// snapshot (the splice frame states off), no compiling-method key (no chain
/// is ever built), a synchronized splice window in the graph (its sites
/// publish the flat frame), a chain that does not split, holds a monitor, or
/// is deeper than the VM's resume budget (`deopt::inlined_chain_refusal`).
/// Fail closed: a `false` here only withholds the class predicate for that
/// loop (the wave-4 shape, the call kept).
fn trap_frame_exists(graph: &Graph, pc: usize, map: Option<&crate::ir::IrSpliceResumeMap>) -> bool {
    let mut chain = false;
    let mut spliced = false;
    let mut flat = false;
    for s in graph.safepoints.iter().filter(|s| s.bci == pc) {
        if s.is_splice_state() {
            spliced = true;
            chain |= s.is_guard_splice_state() && chain_snapshot_publishable(s);
        } else if !s.post_call_only {
            flat = true;
        }
    }
    // A relocated pc the map attributes to an enclosing invoke is inside a
    // splice whatever its snapshots say.
    if map.is_some_and(|m| m.resume_bci(pc) != Some(pc)) {
        spliced = true;
    }
    if spliced {
        chain && !graph.scope_method_key.is_empty() && graph.sync_splice_windows.is_empty()
    } else {
        flat
    }
}

/// The graph-only half of `ir_lower`'s chain publication check for chain
/// snapshot `s`: the scope chain splits the snapshot's slots, no monitor is
/// held in it, and it is no deeper than the VM's resume budget.
fn chain_snapshot_publishable(s: &crate::ir::SafepointSnapshot) -> bool {
    let Some(chain) = s.splice_scope.as_deref() else {
        return false;
    };
    s.monitors.is_empty()
        && chain.scopes.len() >= 2
        && chain.scopes.len() - 1 <= crate::deopt::MAX_OSR_INLINE_RESUME_DEPTH
        && chain.split(&s.locals, &s.stack).is_some()
}

/// nio21 wave 6 (lane L5): the bci the VM charges a failing trap at `pc` to,
/// and so the bci its per-site de-spec is recorded at
/// (`deopt_resume::real_frame_deopt_resume_or_throw_and_despeculate`: an
/// inlined chain at its OUTERMOST scope's bci, the call site of the top-level
/// splice in the compiling method; a flat frame at its own bci). The build's
/// splice map answers it for a relocated `pc` ([`crate::ir::IrSpliceResumeMap::resume_bci`],
/// the rule `ir_lower`'s `resume_bci` applies); with no map (the build
/// spliced nothing), `pc` is the method's own unless a chain snapshot sits
/// there. `None` when it cannot be attributed.
fn trap_charge_bci(
    graph: &Graph,
    pc: usize,
    map: Option<&crate::ir::IrSpliceResumeMap>,
) -> Option<usize> {
    match map {
        Some(m) => m.resume_bci(pc),
        None => (!graph
            .safepoints
            .iter()
            .any(|s| s.bci == pc && s.is_splice_state()))
        .then_some(pc),
    }
}

/// `CRATONVM_JIT_IR_SEQ_CHARAT_TRAP_SITE_DESPEC` (nio21 wave 6, lane L5) --
/// **default ON**; `=0` restores wave 5's per-method withdrawal rule (any
/// de-spec in the method withholds the trap). Read per asked loop.
pub(super) fn site_despec_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_SEQ_CHARAT_TRAP_SITE_DESPEC")
}

/// `(entry control, header bci)` of every live loop-entry class guard
/// `class_predicate` plants: `Guard{bci: b}` at pc `b`, condition
/// `ExactClassIs`, control the loop's entry predecessor.
///
/// JIT round 20 (lane strings, page
/// `r21-charat-trap-reads-a-length-predicate-class-guard`): only at a header
/// `class_predicate` itself planted for ([`Graph::seq_charat_trap_headers`]),
/// i.e. a loop that passed [`class_predicate_withheld`]'s frame and de-spec
/// screen. Before, the SHAPE was the licence, and any other planter of it
/// (`plant_length_predicates` moving an in-body trap-form receiver guard to
/// the pre-header, with `CRATONVM_JIT_IR_LENGTH_PREDICATE_CHARAT_SCREEN=0` or
/// any future pass) licensed an unscreened trap: a compile refused for a
/// missing frame, or a trap re-planted after the site's de-spec. Unconditional
/// (it only withholds traps nothing screened).
fn class_guarded_entries(graph: &Graph) -> FxHashSet<(NodeId, usize)> {
    let mut out = FxHashSet::default();
    if graph.seq_charat_trap_headers.is_empty() {
        return out;
    }
    for n in graph.nodes.iter() {
        let Op::Guard { bci } = n.op else {
            continue;
        };
        if n.bytecode_pc != Some(bci) || !graph.seq_charat_trap_headers.contains(&bci) {
            continue;
        }
        let (Some(ctrl), Some(cond)) = (n.input_opt(0), n.input_opt(1)) else {
            continue;
        };
        if ctrl == NO_NODE
            || !graph
                .node_opt(cond)
                .is_some_and(|c| matches!(c.op, Op::ExactClassIs { .. }))
        {
            continue;
        }
        out.insert((ctrl, bci));
    }
    out
}

/// Is `id` a `SeqCharAt` call whose prefix deopts on a miss (see the module
/// notes)? The one predicate the field forward and the lowering share.
pub(crate) fn seq_charat_traps_on_miss(graph: &Graph, id: NodeId) -> bool {
    // The narrow-oop screen is the prefix's own compile-time refusal
    // (`try_emit_ir_string_search_prefix`); asking it here keeps the lowering's
    // bailout for a named call to the per-compile shapes alone.
    if !seq_charat_trap_enabled() || crate::x64::narrow_oops_block_inline_fields() {
        return false;
    }
    let Some(node) = graph.node_opt(id) else {
        return false;
    };
    // Structure first: the info row is read only for a 4-input `int` call
    // with a pc (the trap resumes at it).
    //
    // No frame test here, deliberately: the predicate must answer the same
    // after optimize as at lowering, and snapshot pruning runs in between.
    // `class_predicate` refuses a loop whose call has no trap frame
    // ([`class_predicate_withheld`]), and the lowering refuses the compile
    // when a named call's frame has gone since.
    let Some(ctrl) = node.input_opt(0) else {
        return false;
    };
    if node.bytecode_pc.is_none()
        || !matches!(node.op, Op::Call { .. })
        || ctrl == NO_NODE
        || !is_seq_charat_call(node)
    {
        return false;
    }
    let entries = class_guarded_entries(graph);
    if entries.is_empty() {
        return false;
    }
    let users = build_users(graph);
    loop_headers(graph)
        .into_iter()
        .any(|(region, entry_pred, back_ctrls)| {
            let Some(header_bci) = graph.node_opt(region).and_then(|r| r.bytecode_pc) else {
                return false;
            };
            if !entries.contains(&(entry_pred, header_bci)) {
                return false;
            }
            let body = loop_body_with(graph, &users, region, entry_pred, &back_ctrls);
            body.contains(&id) || body.contains(&ctrl)
        })
}

/// Why `class_predicate` must plant nothing for the loop at `region`, when it
/// holds a `SeqCharAt` call that a planted guard would turn into a trap
/// ([`seq_charat_traps_on_miss`]); `None` when it may plant.
///
/// * The VM has de-specialised the site a call's trap is charged at
///   ([`withheld_by_despec`]).
/// * A call has no frame a trap at its pc could resume (`trap_frame_exists`):
///   the lowering would have to refuse the compile.
///
/// `map` is the build's splice resume map
/// (`crate::ir::splice_resume_map_this_build`, read once by the caller).
#[allow(clippy::too_many_arguments)]
pub(super) fn class_predicate_withheld(
    graph: &Graph,
    despec: &crate::deopt::DespecRegistry,
    method_key: &str,
    region: NodeId,
    entry_pred: NodeId,
    back_ctrls: &[NodeId],
    map: Option<&crate::ir::IrSpliceResumeMap>,
) -> Option<&'static str> {
    if !seq_charat_trap_enabled() || crate::x64::narrow_oops_block_inline_fields() {
        return None;
    }
    let users = build_users(graph);
    let calls: Vec<NodeId> = loop_body_with(graph, &users, region, entry_pred, back_ctrls)
        .into_iter()
        .filter(|&id| graph.node_opt(id).is_some_and(is_seq_charat_call))
        .collect();
    if calls.is_empty() {
        return None;
    }
    if withheld_by_despec(despec, method_key, graph, &calls, map) {
        return Some("a charAt trap site was de-specialised");
    }
    let framed = calls.iter().all(|&id| {
        graph
            .node_opt(id)
            .and_then(|n| n.bytecode_pc)
            .is_some_and(|pc| trap_frame_exists(graph, pc, map))
    });
    (!framed).then_some("a charAt call has no trap frame")
}

/// `class_predicate`'s withdrawal rule for a loop holding the `SeqCharAt`
/// calls `calls`.
///
/// nio21 wave 6 (lane L5, proposal L5W5-3): per SITE. A failing trap is
/// charged, and after the VM's per-bci limit de-specialised, at
/// [`trap_charge_bci`] of its call's pc (the outermost invoke of the chain
/// for a call inside a splice), which the splice map names ahead of time; the
/// trap is withheld when ANY call's site is de-specialised there (any
/// speculation id: the chain charge records the wildcard). A call whose site
/// cannot be attributed falls back to wave 5's per-method rule (the VM has
/// de-specialised SOMETHING in this method), the answer that is never too
/// weak; so does `CRATONVM_JIT_IR_SEQ_CHARAT_TRAP_SITE_DESPEC=0` for every
/// call. Either way the next compile keeps the loop's in-body class tests and
/// the call.
pub(super) fn withheld_by_despec(
    despec: &crate::deopt::DespecRegistry,
    method_key: &str,
    graph: &Graph,
    calls: &[NodeId],
    map: Option<&crate::ir::IrSpliceResumeMap>,
) -> bool {
    if !seq_charat_trap_enabled() {
        return false;
    }
    let per_method = || despec.count_for(method_key) > 0;
    if !site_despec_enabled() {
        return per_method();
    }
    calls.iter().any(|&id| {
        let site = graph
            .node_opt(id)
            .and_then(|n| n.bytecode_pc)
            .and_then(|pc| trap_charge_bci(graph, pc, map))
            .and_then(|b| u32::try_from(b).ok());
        match site {
            Some(bci) => despec.contains(method_key, bci),
            None => per_method(),
        }
    })
}

// ── nio21 wave 21 (lane L3): the trapping charAt's String reads, hoisted ──
//
// `CRATONVM_JIT_IR_SEQ_CHARAT_HOIST` (opt-in `=1` after the m5 A/B; off keeps the call and its
// per-char prefix).
//
// # What the prefix still costs per char
//
// A trapping `SeqCharAt` call (above) is lowered as the String prefix
// (`ir_lower::Lowerer::try_emit_ir_string_search_prefix`). On EVERY call the
// prefix re-proves the receiver and re-reads its fields: the exact `String`
// class-id compare, then for `value` and for `coder` the per-object
// `GC_FLAG_COMPACT` dispatch and the load, then `value.length`, then the
// bounds compare and the element load. In `encodeBufferLoop` the receiver is
// `src.str`, which the field forward makes ONE pre-header value. So that whole
// chain is loop-invariant except the index compare and the element load, yet
// it is machine code inside the call's lowering, where no pass can hoist it.
//
// # What this pass does
//
// For each trapping charAt call in a class-predicated loop whose receiver is
// loop-invariant, it rewrites the call as IR:
//
// * **Pre-header** (on the loop's entry control, threaded on the forward
//   memory edge, after the class predicate's guard and the forward's reads):
//   `Guard{bci: header}(ExactClassIs(recv, String))` with the loop-entry frame;
//   `coder = recv.coder`, `value = recv.value`, `Guard(value != null)`, and
//   `count = value.length >>> coder`. The loads carry the String layout's
//   compact rows (`Graph::field_rows`, the rows the compile door gives
//   `String` accessor sites).
// * **At the call**: `Guard{bci: call}(0 <= idx)`, `Guard{bci: call}(idx < count)`,
//   then the branchless LATIN1/UTF16 decode `IrBuilder::try_string_access_intrinsic`
//   builds (two byte loads at `idx << coder` and `+ coder`).
//
// The call's value uses take the decoded char, its memory users the last
// element load. The call is killed.
//
// # Why it is exact
//
// * `String.value` and `String.coder` are final fields of an immutable class,
//   written only by the constructor before the String is published (HotSpot
//   trusts both as stable). So reading them once at the loop entry gives every
//   iteration the values the call would have read, whatever the loop calls in
//   between. `value.length` is an array's length and never changes.
// * The pre-header guard fails exactly when the receiver is not a `String`
//   (`ExactClassIs` of null is `0`). It deopts to the LOOP ENTRY with the
//   loop-entry frame, and the interpreter runs the loop and every charAt
//   itself. A failing guard is charged at the header bci, where
//   `class_predicate` reads the de-spec registry, so after
//   `PER_BCI_DESPEC_LIMIT` charges the next compile plants no class predicate,
//   no trap and no hoist.
// * The two in-body guards fail exactly where the prefix declined (an index
//   outside `[0, length)`). They deopt AT the call's pc with the frame the trap
//   already required (`class_predicate`'s `trap_frame_exists`), and the
//   interpreter re-executes `charAt`, which throws the JDK's own exception.
//   Nothing has been committed between the call's pc and the guard.
// * The decode reads `value[idx << coder]` and `value[(idx << coder) + coder]`.
//   For `0 <= idx < length >>> coder`, both are inside the array (the builder
//   expansion's argument).
//
// Refused (the call stays): the switch off, no String layout with a `coder`
// and a class id, narrow oops, a receiver defined in the loop, a call with an
// exceptional continuation (`CallThrew` / `CatchEntryFlag`), no loop-entry
// frame, a header the method has de-specialised, and more than
// `MAX_CHARAT_HOISTS` calls per compile.

/// `CRATONVM_JIT_IR_SEQ_CHARAT_HOIST` (nio21 wave 21, lane L3) -- **opt-in**
/// (`=1`): the m5 host A/B measured it slower on the encode rows
/// (`Nio20EncodeLoop encode-ascii` 72 306 ns/op on vs 57 318 off), so every
/// trapping charAt stays a call (the wave-5 prefix) unless it is asked for.
pub(super) fn seq_charat_hoist_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_SEQ_CHARAT_HOIST")
}

/// The most charAt calls one compile rewrites.
const MAX_CHARAT_HOISTS: usize = 4;

/// Rewrite the trapping charAt calls this section describes. Returns how many
/// were rewritten.
pub(super) fn hoist_seq_charat_string_reads(
    graph: &mut Graph,
    despec: &crate::deopt::DespecRegistry,
    method_key: &str,
) -> usize {
    if !seq_charat_hoist_enabled() || crate::x64::narrow_oops_block_inline_fields() {
        return 0;
    }
    let Some(layout) = graph.string_layout else {
        return 0;
    };
    if !layout.has_coder || layout.string_class_id == 0 {
        return 0;
    }
    let mut done = 0usize;
    let mut seen: FxHashSet<NodeId> = FxHashSet::default();
    for (region, entry_pred, back_ctrls) in loop_headers(graph) {
        if done >= MAX_CHARAT_HOISTS {
            break;
        }
        let Some(header_bci) = graph.node_opt(region).and_then(|r| r.bytecode_pc) else {
            continue;
        };
        if !class_guarded_entries(graph).contains(&(entry_pred, header_bci)) {
            continue;
        }
        if u32::try_from(header_bci).is_ok_and(|b| despec.contains(method_key, b)) {
            continue;
        }
        let users = build_users(graph);
        let body = loop_body_with(graph, &users, region, entry_pred, &back_ctrls);
        let mut calls: Vec<NodeId> = body
            .iter()
            .copied()
            .filter(|&id| {
                !seen.contains(&id)
                    && graph.node_opt(id).is_some_and(is_seq_charat_call)
                    && seq_charat_traps_on_miss(graph, id)
            })
            .collect();
        calls.sort_unstable();
        for call in calls {
            if done >= MAX_CHARAT_HOISTS {
                break;
            }
            seen.insert(call);
            if hoist_one(graph, layout, region, entry_pred, header_bci, &body, call) {
                done += 1;
                if super::loop_pass_report_enabled() {
                    eprintln!(
                        "[ir-licm] charAt hoist: {method_key} header bci {header_bci}: the String \
                         reads of n{call} moved to the loop entry"
                    );
                }
            } else if super::loop_pass_report_enabled() {
                eprintln!(
                    "[ir-licm] charAt hoist: {method_key} header bci {header_bci}: n{call} kept \
                     (receiver not invariant, an exception arm, or no loop-entry frame)"
                );
            }
        }
    }
    done
}

/// The compact rows the String layout gives `value` and `coder`, under the
/// compile door's own conditions for its String accessor rows.
fn string_rows(layout: &crate::StringFieldLayout) -> Option<((u32, bool, u8), (u32, bool, u8))> {
    if !cratonvm_types::compact_ref_fields_enabled()
        || layout.value_compact_is_narrow
        || layout.value_compact_offset < 0
        || layout.coder_compact_offset < 0
    {
        return None;
    }
    let coder_tag = if layout.coder_compact_is_byte {
        b'B'
    } else {
        b'I'
    };
    Some((
        (layout.value_compact_offset as u32, true, b'['),
        (layout.coder_compact_offset as u32, false, coder_tag),
    ))
}

#[allow(clippy::too_many_arguments)]
fn hoist_one(
    graph: &mut Graph,
    layout: crate::StringFieldLayout,
    region: NodeId,
    entry_pred: NodeId,
    header_bci: usize,
    body: &FxHashSet<NodeId>,
    call: NodeId,
) -> bool {
    let Some(node) = graph.node_opt(call).cloned() else {
        return false;
    };
    let (Some(ctrl), Some(call_mem), Some(recv), Some(idx), Some(pc)) = (
        node.input_opt(0),
        node.input_opt(1),
        node.input_opt(2),
        node.input_opt(3),
        node.bytecode_pc,
    ) else {
        return false;
    };
    if body.contains(&recv) || !super::is_loop_invariant(graph, recv, region, body) {
        return false;
    }
    let exception_arm = graph.nodes.iter().any(|n| match n.op {
        Op::CallThrew { .. } => n.input_opt(1) == Some(call),
        Op::CatchEntryFlag { call_bci, .. } => call_bci as usize == pc,
        _ => false,
    });
    if exception_arm {
        return false;
    }
    // The forward memory edge of the header's one memory phi.
    let Some(fwd) = graph
        .node_opt(region)
        .and_then(|r| r.inputs.iter().position(|&p| p == entry_pred))
    else {
        return false;
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
        return false;
    };
    let header_mem = *header_mem;
    let Some(mut pre_mem) = graph
        .node_opt(header_mem)
        .and_then(|n| n.input_opt(fwd + 1))
    else {
        return false;
    };
    let Some((bci, frame)) = super::loop_entry_frame(graph, region, fwd + 1, body) else {
        return false;
    };
    if bci != header_bci {
        return false;
    }
    let Ok(si) = u32::try_from(graph.safepoints.len()) else {
        return false;
    };
    graph.push_safepoint(frame);
    let rows = string_rows(&layout);

    // ── The pre-header. ──
    let hpc = Some(header_bci);
    let is_string = graph.add(
        Op::ExactClassIs {
            class_id: layout.string_class_id,
        },
        IrType::Int,
        vec![recv],
        None,
    );
    let class_guard = graph.add(
        Op::Guard { bci: header_bci },
        IrType::Void,
        vec![entry_pred, is_string],
        hpc,
    );
    if !graph.set_node_frame_snapshot(class_guard, si) {
        graph.kill(class_guard);
        graph.kill(is_string);
        return false;
    }
    let coder_idx = graph.add(
        Op::Const(layout.coder_field_index as i64),
        IrType::Int,
        vec![],
        None,
    );
    let coder = graph.add(
        Op::Load(crate::ir::MemKind::Int),
        IrType::Int,
        crate::ir::with_guard_token(vec![entry_pred, pre_mem, recv, coder_idx], class_guard),
        hpc,
    );
    pre_mem = coder;
    let value_idx = graph.add(
        Op::Const(layout.value_field_index as i64),
        IrType::Int,
        vec![],
        None,
    );
    let value = graph.add(
        Op::Load(crate::ir::MemKind::Ref),
        IrType::Ref,
        crate::ir::with_guard_token(vec![entry_pred, pre_mem, recv, value_idx], class_guard),
        hpc,
    );
    pre_mem = value;
    for load in [coder, value] {
        // Unreachable refusal: the loads' pc is the frame's bci.
        let _ = graph.set_node_frame_snapshot(load, si);
    }
    graph.synthetic_field_access_pcs.insert(header_bci);
    if let Some((value_row, coder_row)) = rows {
        graph.field_rows.insert(value, value_row);
        graph.field_rows.insert(coder, coder_row);
    }
    let null = graph.add(Op::Const(0), IrType::Ref, vec![], None);
    let value_ok = graph.add(
        Op::Cmp(crate::ir::CmpOp::Ne),
        IrType::Int,
        vec![value, null],
        None,
    );
    let value_guard = graph.add(
        Op::Guard { bci: header_bci },
        IrType::Void,
        vec![entry_pred, value_ok],
        hpc,
    );
    let _ = graph.set_node_frame_snapshot(value_guard, si);
    let byte_len = graph.add(
        Op::ArrayLength,
        IrType::Int,
        crate::ir::with_guard_token(vec![entry_pred, pre_mem, value], value_guard),
        hpc,
    );
    let _ = graph.set_node_frame_snapshot(byte_len, si);
    let count = graph.add(Op::UShr, IrType::Int, vec![byte_len, coder], None);
    graph.set_input(header_mem, fwd + 1, pre_mem);

    // ── At the call. ──
    let cpc = Some(pc);
    let zero = graph.add(Op::Const(0), IrType::Int, vec![], None);
    let non_negative = graph.add(
        Op::Cmp(crate::ir::CmpOp::Ge),
        IrType::Int,
        vec![idx, zero],
        None,
    );
    graph.add(
        Op::Guard { bci: pc },
        IrType::Void,
        vec![ctrl, non_negative],
        cpc,
    );
    let in_bounds = graph.add(
        Op::Cmp(crate::ir::CmpOp::Lt),
        IrType::Int,
        vec![idx, count],
        None,
    );
    let bounds_guard = graph.add(
        Op::Guard { bci: pc },
        IrType::Void,
        vec![ctrl, in_bounds],
        cpc,
    );
    let off = graph.add(Op::Shl, IrType::Int, vec![idx, coder], None);
    let lo_raw = graph.add(
        Op::ArrayLoad(crate::ir::MemKind::Byte),
        IrType::Int,
        crate::ir::with_guard_token(vec![ctrl, call_mem, value, off], bounds_guard),
        cpc,
    );
    let hi_off = graph.add(Op::Add, IrType::Int, vec![off, coder], None);
    let hi_raw = graph.add(
        Op::ArrayLoad(crate::ir::MemKind::Byte),
        IrType::Int,
        crate::ir::with_guard_token(vec![ctrl, lo_raw, value, hi_off], bounds_guard),
        cpc,
    );
    let byte_mask = graph.add(Op::Const(0xFF), IrType::Int, vec![], None);
    let lo = graph.add(Op::And, IrType::Int, vec![lo_raw, byte_mask], None);
    let hi = graph.add(Op::And, IrType::Int, vec![hi_raw, byte_mask], None);
    let eight = graph.add(Op::Const(8), IrType::Int, vec![], None);
    let hi_shifted = graph.add(Op::Shl, IrType::Int, vec![hi, eight], None);
    let coder_mask = graph.add(Op::Sub, IrType::Int, vec![zero, coder], None);
    let hi_selected = graph.add(Op::And, IrType::Int, vec![hi_shifted, coder_mask], None);
    let ch = graph.add(Op::Or, IrType::Int, vec![lo, hi_selected], None);

    // ── Rewire: memory users of the call take the last element load. ──
    for user in graph.users_of(call) {
        if user == lo_raw || user == hi_raw {
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
            .filter(|&(i, &x)| x == call && crate::ir::is_memory_token_slot(un, i))
            .map(|(i, _)| i)
            .collect();
        for slot in slots {
            graph.set_input(user, slot, hi_raw);
        }
    }
    graph.replace_all_uses(call, ch);
    graph.kill(call);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{CmpOp, MemKind, SafepointSnapshot};

    /// A leaked `CharSequence.charAt(I)C` interface row.
    fn charat_row() -> usize {
        let info: &'static crate::JitInvokeInfo = Box::leak(Box::new(crate::JitInvokeInfo {
            class_name: "java/lang/CharSequence",
            method_name: "charAt",
            descriptor: "(I)C",
            num_jit_args: 2,
            return_type: b'C',
            invoke_kind: 2,
            declaring_class_id: 0,
            owner_class_id: 0,
        }));
        info as *const crate::JitInvokeInfo as usize
    }

    /// `while (i < n) { s.charAt(src.f); i++; }` with a loop-entry
    /// class guard on `src` (when `guarded`) and a flat snapshot at the call's
    /// pc 12 (when `frame`). Returns `(graph, call, the field read)`.
    fn charat_loop(guarded: bool, frame: bool) -> (Graph, NodeId, NodeId) {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let src = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let s = g.add(Op::Param(1), IrType::Ref, vec![start], None);
        let n = g.add(Op::Param(2), IrType::Int, vec![start], None);
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        if guarded {
            let is = g.add(
                Op::ExactClassIs { class_id: 7 },
                IrType::Int,
                vec![src],
                None,
            );
            g.add(Op::Guard { bci: 5 }, IrType::Void, vec![entry, is], Some(5));
            // As `class_predicate` records a header it planted for.
            g.seq_charat_trap_headers.insert(5);
        }
        let region = g.add(Op::Merge, IrType::Control, vec![entry], Some(5));
        let i = g.add(Op::Phi, IrType::Int, vec![region, zero], None);
        let mem = g.add(Op::Phi, IrType::Memory, vec![region, mem0], None);
        let ge = g.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![i, n], None);
        let head = g.add(Op::If, IrType::Control, vec![region, ge], Some(8));
        let exit = g.add(Op::Proj(0), IrType::Control, vec![head], None);
        let stay = g.add(Op::Proj(1), IrType::Control, vec![head], None);
        let f0 = g.add(Op::Const(0), IrType::Int, vec![], None);
        let ld = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![stay, mem, src, f0],
            Some(10),
        );
        g.field_decl_class.insert(ld, 3);
        let call = g.add(
            Op::Call {
                info_ptr: charat_row(),
            },
            IrType::Int,
            // `s.charAt(src.f)`: the read is the call's token and its index.
            vec![stay, ld, s, ld],
            Some(12),
        );
        let i1 = g.add(Op::Add, IrType::Int, vec![i, one], None);
        g.nodes[region as usize].inputs.push(stay);
        g.nodes[i as usize].inputs.push(i1);
        g.nodes[mem as usize].inputs.push(call);
        g.exit = g.add(Op::Return, IrType::Void, vec![exit], Some(20));
        g.push_safepoint(SafepointSnapshot::new(
            5,
            vec![src, s, n, i],
            vec![],
            vec![],
        ));
        if frame {
            g.push_safepoint(SafepointSnapshot::new(
                12,
                vec![src, s, n, i],
                vec![s, i],
                vec![],
            ));
        }
        (g, call, ld)
    }

    /// The prefix kind the predicate keys on is resolvable in this build (a
    /// run with the String prefix switched off has nothing to test).
    fn prefix_names_charat() -> bool {
        !crate::x64::narrow_oops_block_inline_fields()
            && crate::ir_string_search_prefix_kind("java/lang/CharSequence", "charAt", "(I)C")
                == Some(crate::IrStringSearchKind::SeqCharAt)
    }

    fn with_trap<R>(value: Option<&str>, f: impl FnOnce() -> R) -> R {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_SEQ_CHARAT_TRAP", value)],
            f,
        )
    }

    #[test]
    fn nio21w5_a_charat_call_in_a_class_predicated_loop_traps_on_miss() {
        if !prefix_names_charat() {
            return;
        }
        let (g, call, _) = charat_loop(true, true);
        assert!(with_trap(None, || seq_charat_traps_on_miss(&g, call)));
        // The kill switch, then no loop-entry class guard.
        assert!(!with_trap(Some("0"), || seq_charat_traps_on_miss(&g, call)));
        let (g, call, _) = charat_loop(false, true);
        assert!(!with_trap(None, || seq_charat_traps_on_miss(&g, call)));
        // The frame is not the predicate's business (it must not change
        // between optimize and lowering); `class_predicate` asks it instead.
        let (g, call, _) = charat_loop(true, false);
        assert!(with_trap(None, || seq_charat_traps_on_miss(&g, call)));
        // JIT round 20: the same guard SHAPE at a header `class_predicate`
        // did not plant for (another pass moved it there) licenses nothing.
        let (mut g, call, _) = charat_loop(true, true);
        g.seq_charat_trap_headers.clear();
        assert!(!with_trap(None, || seq_charat_traps_on_miss(&g, call)));
    }

    /// `class_predicate`'s refusals for a loop holding a charAt call: a
    /// de-specialised method, a call with no trap frame; none with the switch
    /// off.
    #[test]
    fn nio21w5_class_predicate_is_withheld_without_a_frame_or_after_a_despec() {
        if !prefix_names_charat() {
            return;
        }
        let withheld = |g: &Graph, despec: &crate::deopt::DespecRegistry, trap: Option<&str>| {
            let (region, entry, backs) = loop_headers(g)
                .into_iter()
                .next()
                .expect("the loop is recognised");
            with_trap(trap, || {
                class_predicate_withheld(g, despec, "T.m:()V", region, entry, &backs, None)
            })
        };
        let clean = crate::deopt::DespecRegistry::new();
        let (g, _, _) = charat_loop(false, true);
        assert_eq!(withheld(&g, &clean, None), None);
        let (g, _, _) = charat_loop(false, false);
        assert!(withheld(&g, &clean, None).is_some());
        assert_eq!(withheld(&g, &clean, Some("0")), None);
        // nio21 wave 6: the call's own site (pc 12, the method's own code).
        let despecd = crate::deopt::DespecRegistry::new();
        despecd.insert("T.m:()V", 12, crate::deopt::SPECULATION_ID_ANY);
        let (g, _, _) = charat_loop(false, true);
        assert!(withheld(&g, &despecd, None).is_some());
    }

    /// The call no longer refuses the field forward when it traps; with the
    /// switch off it does, as in wave 4.
    #[test]
    fn nio21w5_the_field_forward_admits_a_trapping_charat() {
        if !prefix_names_charat() {
            return;
        }
        let forward = |g: &mut Graph, trap: Option<&str>| {
            cratonvm_types::flags::with_thread_overrides(
                &[
                    ("CRATONVM_JIT_IR_SEQ_CHARAT_TRAP", trap),
                    ("CRATONVM_JIT_IR_LOOP_FIELD_FORWARD", None),
                    // JIT round 20 wave 4: the test loop's base is a
                    // parameter; the forwarding mechanics are under test.
                    ("CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_NULL_GUARD", Some("1")),
                ],
                || super::super::loop_field_forward::forward_loop_fields(g, None),
            )
        };
        let (mut g, _, ld) = charat_loop(true, true);
        assert_eq!(forward(&mut g, None), 1);
        assert_eq!(g.nodes[ld as usize].op, Op::Dead);
        let (mut g, _, ld) = charat_loop(true, true);
        assert_eq!(forward(&mut g, Some("0")), 0);
        assert!(matches!(g.nodes[ld as usize].op, Op::Load(_)));
    }

    /// nio21 wave 6 (L5W5-3): the withdrawal is per SITE -- the bci the VM
    /// charges the trap at -- and wave 5's per-method rule is the kill
    /// switch's answer.
    #[test]
    fn nio21w6_a_despec_withholds_the_trap_only_at_its_own_site() {
        let (g, call, _) = charat_loop(false, true);
        let calls = [call];
        let ask = |despec: &crate::deopt::DespecRegistry, key: &str, site: Option<&str>| {
            cratonvm_types::flags::with_thread_overrides(
                &[
                    ("CRATONVM_JIT_IR_SEQ_CHARAT_TRAP", None),
                    ("CRATONVM_JIT_IR_SEQ_CHARAT_TRAP_SITE_DESPEC", site),
                ],
                || withheld_by_despec(despec, key, &g, &calls, None),
            )
        };
        let despec = crate::deopt::DespecRegistry::new();
        assert!(!ask(&despec, "T.m:()V", None));
        // Another site of the method: not this trap's.
        despec.insert("T.m:()V", 13, crate::deopt::SPECULATION_ID_ANY);
        assert!(!ask(&despec, "T.m:()V", None));
        assert!(ask(&despec, "T.m:()V", Some("0")), "the per-method rule");
        // The call's own pc, any speculation id there.
        despec.insert("T.m:()V", 12, 77);
        assert!(ask(&despec, "T.m:()V", None));
        assert!(!ask(&despec, "U.m:()V", None));
        assert!(!with_trap(Some("0"), || withheld_by_despec(
            &despec, "T.m:()V", &g, &calls, None
        )));
    }

    /// The charge site of a pc inside a splice is the OUTERMOST invoke the
    /// map names; an unattributable pc falls back to the per-method rule.
    #[test]
    fn nio21w6_a_spliced_trap_is_charged_at_its_outermost_invoke() {
        let (g, _, _) = charat_loop(false, true);
        // A body of 20 bytes at combined pc 100 replaced the invoke at 7; a
        // nested one at 200 replaced the invoke at combined pc 105.
        let map = crate::ir::IrSpliceResumeMap {
            code_len: 50,
            bodies: vec![(100, 20, 7), (200, 10, 105)],
        };
        assert_eq!(trap_charge_bci(&g, 12, Some(&map)), Some(12));
        assert_eq!(trap_charge_bci(&g, 110, Some(&map)), Some(7));
        assert_eq!(trap_charge_bci(&g, 203, Some(&map)), Some(7));
        assert_eq!(trap_charge_bci(&g, 400, Some(&map)), None);
        assert_eq!(trap_charge_bci(&g, 12, None), Some(12));
    }

    /// A pc the map places in a splice needs a chain snapshot: the flat one
    /// alone no longer counts as a trap frame.
    #[test]
    fn nio21w6_a_relocated_pc_needs_a_chain_frame() {
        let (g, _, _) = charat_loop(false, true);
        assert!(trap_frame_exists(&g, 12, None));
        let map = crate::ir::IrSpliceResumeMap {
            code_len: 10,
            bodies: vec![(10, 20, 3)],
        };
        assert!(!trap_frame_exists(&g, 12, Some(&map)));
    }

    // ── nio21 wave 21 (lane L3): the hoisted String reads ──

    fn with_layout(mut g: Graph) -> Graph {
        g.string_layout = Some(crate::StringFieldLayout::new(0, Some(1), 2, 7));
        g
    }

    fn hoist(g: &mut Graph, despec: &crate::deopt::DespecRegistry, on: bool) -> usize {
        cratonvm_types::flags::with_thread_overrides(
            &[(
                "CRATONVM_JIT_IR_SEQ_CHARAT_HOIST",
                if on { Some("1") } else { None },
            )],
            || hoist_seq_charat_string_reads(g, despec, "T.m:()V"),
        )
    }

    fn count(g: &Graph, f: impl Fn(&Node) -> bool) -> usize {
        g.nodes.iter().filter(|n| f(n)).count()
    }

    /// The call becomes: a loop-entry `String` class guard (named frame at
    /// the header), `coder` / `value` / length at the entry, and in the body
    /// two index guards and two byte loads at the call's pc. The loop's memory
    /// phi takes the last element load on its back edge.
    #[test]
    fn nio21w21_the_charat_string_reads_move_to_the_loop_entry() {
        if !prefix_names_charat() {
            return;
        }
        let (g, call, _) = charat_loop(true, true);
        let mut g = with_layout(g);
        let clean = crate::deopt::DespecRegistry::new();
        assert_eq!(hoist(&mut g, &clean, true), 1);
        assert_eq!(g.nodes[call as usize].op, Op::Dead, "the call is gone");
        // Entry: the String guard (class 7 on `s`) besides the loop's own.
        let string_guards = count(&g, |n| {
            n.op == (Op::Guard { bci: 5 })
                && n.input_opt(1).is_some_and(|c| {
                    g.nodes[c as usize].op == (Op::ExactClassIs { class_id: 7 })
                        // `s` is node 4 (start, entry, mem, src, s).
                        && g.nodes[c as usize].input_opt(0) == Some(4)
                })
        });
        assert_eq!(string_guards, 1, "the String class guard on s");
        assert_eq!(
            count(&g, |n| matches!(n.op, Op::Load(_))
                && n.bytecode_pc == Some(5)),
            2,
            "coder and value at the entry"
        );
        assert_eq!(
            count(&g, |n| n.op == Op::ArrayLength && n.bytecode_pc == Some(5)),
            1
        );
        assert_eq!(
            count(&g, |n| n.op == Op::ArrayLoad(MemKind::Byte)
                && n.bytecode_pc == Some(12)),
            2
        );
        assert_eq!(
            count(&g, |n| n.op == (Op::Guard { bci: 12 })),
            2,
            "0 <= i < count"
        );
        assert!(g.synthetic_field_access_pcs.contains(&5));
    }

    /// The switch off, a de-specialised header, or no String layout: the
    /// call stays.
    #[test]
    fn nio21w21_the_hoist_is_refused_without_its_premises() {
        if !prefix_names_charat() {
            return;
        }
        let clean = crate::deopt::DespecRegistry::new();
        let (g, call, _) = charat_loop(true, true);
        let mut g = with_layout(g);
        assert_eq!(hoist(&mut g, &clean, false), 0);
        assert!(matches!(g.nodes[call as usize].op, Op::Call { .. }));
        let despecd = crate::deopt::DespecRegistry::new();
        despecd.insert("T.m:()V", 5, crate::deopt::SPECULATION_ID_ANY);
        let (g, call, _) = charat_loop(true, true);
        let mut g = with_layout(g);
        assert_eq!(hoist(&mut g, &despecd, true), 0);
        assert!(matches!(g.nodes[call as usize].op, Op::Call { .. }));
        let (mut g, call, _) = charat_loop(true, true);
        assert_eq!(hoist(&mut g, &clean, true), 0, "no String layout");
        assert!(matches!(g.nodes[call as usize].op, Op::Call { .. }));
    }
}
