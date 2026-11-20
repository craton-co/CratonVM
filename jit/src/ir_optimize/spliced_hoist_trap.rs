// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT round 23 wave 8 (lane W8-L7; `CRATONVM_JIT_IR_NULL_BASE_LOAD_PREDICATE_SPLICED_HOIST`,
//! perf, **default ON**, `=0` kill): the loop-entry predicates of a loop
//! SPLICED from a callee step over the pre-header copy LICM made of that same
//! loop's array length or element read.
//!
//! # What it fixes
//!
//! `W2cTypecheckLoops`' timed rows run through lambdas (`() -> nocastRow(subs,
//! holder)`). The body that runs is the lambda's method-entry compile with the
//! row spliced into it (`lambda$main$2`, `full/ir`), not the row's own OSR body.
//! In that body LICM hoists the header's `a.length` into the pre-header, where
//! its null check is a trapping node whose deopt resumes at the lambda's
//! `invoke` (a flat frame of the compiling method, before the header's combined
//! pc). [`super::preheader_traps_block_entry_predicate`] accepted only a trap
//! whose own frame is a flat frame AT OR PAST the header, so the null-base
//! predicate answered `header .. pre-header may trap` and `h.k`, `h.tab` and
//! `h.tab[5]` stayed in the loop: three field rows with their epoch compares,
//! compact selects and helper arms, plus the element's null and bounds checks,
//! on every element (n23-w6 `lambda$main$2` disassembly, 0x13f-0x399). The
//! checkcast predicate (the `cast` row) asks the same question.
//!
//! # Why stepping over it is sound
//!
//! The node is a copy LICM moved out of THIS loop: an `ArrayLength` or
//! `ArrayLoad` whose pc is at or past the header's, in the same spliced body
//! (a pre-header node built there has a pc BEFORE the header; nothing after the
//! loop is anchored in its pre-header). Its deopt resumes at the outermost
//! `invoke` with the caller's frame (LICM's `install_preheader_deopt_frame`;
//! no chain snapshot at its pc could re-route it to its in-loop position), so
//! the interpreter re-runs the callee from its first bytecode. The predicate's
//! guard resumes at the callee's loop header with the loop-entry values.
//! Both points are at or before every bytecode of the loop, nothing between
//! them and the pre-header is skipped, so whichever of the two fails first,
//! the interpreter raises what bytecode order raises first. That is the
//! argument the existing flat-frame exemption makes, with "at or before the
//! header" instead of "at the header".
//!
//! Any other trapping pre-header node still refuses, as does a hoisted node
//! whose deopt names a chain frame -- except the element copy at the header's
//! own pc that names the loop-entry chain (see
//! [`licm_array_load_spliced_chain_enabled`]), which resumes at the header.

use crate::ir::{Graph, IrSpliceResumeMap, Node, NodeId, Op};

/// `CRATONVM_JIT_IR_NULL_BASE_LOAD_PREDICATE_SPLICED_HOIST` -- **default ON**,
/// `=0` kill. Read per compile; no `static`.
pub(super) fn spliced_hoist_trap_exempt_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_LICM_ARRAY_LOAD_SPLICED_CHAIN` (JIT round 23 wave 8, lane
/// W8-L7; perf, **default ON** since the w2c close (2026-10-24), `=0` kill:
/// with the late unroll round it is what takes `h.tab[5]` out of the timed
/// loop, which the main/post copies then no longer repeat; opt-in until then,
/// NEUTRAL alone on the n23-w8g A/B, W2c nocast
/// 2.11/1.21/3.88 ns/elem default vs 1.99/1.34/2.04 at `=0`, cast 3.05/1.94/3.18
/// vs 2.69/2.10/2.79): the array-load hoist's second round
/// (`CRATONVM_JIT_IR_LICM_ARRAY_LOAD_AFTER_PREDICATES`) also serves a loop
/// SPLICED from a callee, naming the loop-entry chain frame the loop's
/// null-base predicate guard already names ([`chain_predicate_entry_frame`]).
///
/// `h.tab[5]` in `W2cTypecheckLoops`' lambda-driven rows: after the null-base
/// predicate moved `h.tab` to the pre-header the element read was invariant,
/// but the planner wanted the header's FLAT snapshot, which a spliced header
/// never has, so its null test, bounds check and load stayed in the loop
/// (n23-w8c `lambda$main$2`, 0x342-0x360).
///
/// Sound as the flat hoist is: the copy sits in the pre-header after the
/// guard (a higher node id), reads the loop's entry memory in a body that
/// writes nothing it may alias, and a failing null or bounds check deopts with
/// the callee at its loop header and the entry values, every caller parked at
/// its invoke -- the interpreter runs the loop from its first test and raises
/// whatever the bytecode raises, where it raises it. Such a deopt is not a
/// listed chain predicate, so the VM charges it at the call site under the
/// wildcard, after which `chain_predicate_withdrawn` keeps the next compile's
/// null-base predicate (and so this hoist) out of that splice. Read per
/// compile; no `static`.
pub(super) fn licm_array_load_spliced_chain_enabled() -> bool {
    true
}

/// The snapshot index of the loop-entry CHAIN frame a chain predicate guard
/// (`Graph::chain_predicate_guards`) of the loop at combined pc `header_bci`
/// names: a live `Guard { bci: header_bci }` anchored at `entry_pred` whose
/// frame is a guard-usable chain snapshot at that pc. `None` with the switch
/// off, or when there is none.
pub(super) fn chain_predicate_entry_frame(
    graph: &Graph,
    entry_pred: NodeId,
    header_bci: usize,
) -> Option<u32> {
    if !licm_array_load_spliced_chain_enabled() {
        return None;
    }
    graph.chain_predicate_guards.iter().find_map(|&(g, _)| {
        let n = graph.node_opt(g)?;
        if !matches!(n.op, Op::Guard { bci } if bci == header_bci)
            || n.inputs.first() != Some(&entry_pred)
        {
            return None;
        }
        let si = n.frame_snapshot?;
        let sp = graph.safepoints.get(si as usize)?;
        (sp.is_guard_splice_state() && sp.bci == header_bci).then_some(si)
    })
}

/// The spliced body `[base, base + len)` holding combined pc `pc`: its base.
fn body_base(map: &IrSpliceResumeMap, pc: usize) -> Option<usize> {
    if pc < map.code_len {
        return None;
    }
    map.bodies
        .iter()
        .find(|&&(base, len, _)| pc >= base && pc < base.saturating_add(len))
        .map(|&(base, _, _)| base)
}

/// Is `node`, anchored in the pre-header of the loop whose header sits at
/// combined pc `header_bci`, LICM's copy of that spliced loop's own length or
/// element read, whose deopt resumes at or before the loop's first bytecode?
/// See the module notes. `false` whenever unsure.
pub(super) fn hoisted_from_spliced_loop(
    graph: &Graph,
    node: &Node,
    header_bci: usize,
    map: &IrSpliceResumeMap,
) -> bool {
    if !matches!(node.op, Op::ArrayLength | Op::ArrayLoad(_)) {
        return false;
    }
    let Some(pc) = node.bytecode_pc else {
        return false;
    };
    if pc < header_bci {
        return false;
    }
    let Some(header_body) = body_base(map, header_bci) else {
        return false;
    };
    if body_base(map, pc) != Some(header_body) {
        return false;
    }
    let Some(outer) = map.resume_bci(header_bci) else {
        return false;
    };
    match node.frame_snapshot {
        // Resolved by pc at lowering: the outermost invoke, unless a chain
        // snapshot describes the pc (it would resume in the loop, uncorrected).
        None => !graph
            .safepoints
            .iter()
            .any(|s| s.is_guard_splice_state() && s.bci == pc),
        // A flat frame at the outermost invoke, or (the element copy
        // `chain_predicate_entry_frame` lets the array-load hoist make) the
        // loop-entry chain itself, which resumes at the header.
        Some(si) => graph.safepoints.get(si as usize).is_some_and(|s| {
            (!s.is_splice_state() && s.bci == outer)
                || (s.is_guard_splice_state() && s.bci == header_bci && pc == header_bci)
        }),
    }
}


