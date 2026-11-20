// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 4 (lane MONROWS): two monitor rows of the round-18 backlog
//! (`docs/feature-designs/jit-ir-optimizations.md`).
//!
//! * **MON17-5** -- the loop block coarsening
//!   (`ir_optimize::plan_loop_block_coarsening`) admitted only `Load`, `Store`,
//!   a direct `LoadStatic` / `StoreStatic` and a `Guard` at its own bci as the
//!   nodes of a block that may leave the compiled code. A primitive array
//!   element access and an array length leave exactly as a field access does:
//!   their null and bounds checks deopt at their own bci (`ir_lower`'s
//!   `ArrayLoad` / `ArrayStore` / `ArrayLength` arms, deopt-on-fault), so each
//!   copy stamps its clone with a per-copy frame that holds the monitor once,
//!   as it stamps a `Load`. Not admitted: a reference element store
//!   (`aastore` calls `jit_aastore`: covariance check, barriers, allocation)
//!   and a reference element load (under ZGC it calls the load-barrier
//!   helper). An access the implicit-exception edges armed is never reached
//!   here: its pad and the block's catch-any handler lie outside the body, so
//!   the planner refuses the loop on "a body value is read outside the body".
//!   Opt-in (`CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_ARRAY_OPS=1`) until the
//!   executed tests and `J27MonitorRows` have run on the host.
//! * **SY16-3** -- the adjacent-pair coarsening
//!   (`ir_optimize::coarsenable_monitor_pair`) could merge one half of a
//!   caller-held synchronized direct CALL (`enter, Call, exit` at one invoke
//!   pc) with a bytecode block's half on the same object. `ir_lower`'s
//!   `sync_direct_call_monitor` then found no `MonitorEnter` under the call
//!   and refused the whole compile. The builder now records those halves on
//!   the graph (`Graph::sync_call_monitor_ops`), and a pair is merged only
//!   when both halves are caller-held or neither is, as the synchronized
//!   splice windows already are. Refusing a merge only keeps the graph the
//!   builder made, so it needs no switch.

use crate::ir::{Graph, MemKind, NodeId, Op};

/// `CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_ARRAY_OPS=1` (opt-in; j27 wave 4, lane
/// MONROWS, MON17-5): the loop block coarsening also admits a primitive array
/// element load or store and an array length ([`block_admits_array_op`]).
/// Read live, per admitted array node (no `static`).
pub(super) fn ir_loop_lock_coarsen_array_ops_enabled() -> bool {
    false}

/// Is `op` an array access whose only way out is a deopt at its own bci: an
/// `ArrayLength`, or an `ArrayLoad` / `ArrayStore` of a primitive kind?
fn is_deopt_only_array_op(op: &Op) -> bool {
    match op {
        Op::ArrayLength => true,
        Op::ArrayLoad(kind) | Op::ArrayStore(kind) => *kind != MemKind::Ref,
        _ => false,
    }
}

/// MON17-5: may the loop block coarsening take `op` as a node of the block
/// that can leave (with a per-copy frame)? Only a deopt-only array access
/// ([`is_deopt_only_array_op`]), and only under
/// [`ir_loop_lock_coarsen_array_ops_enabled`]. The switch is read only for
/// such an op, so every other refusal stays one match arm.
pub(super) fn block_admits_array_op(op: &Op) -> bool {
    is_deopt_only_array_op(op) && ir_loop_lock_coarsen_array_ops_enabled()
}

impl Graph {
    /// SY16-3: record the two monitor halves of a caller-held synchronized
    /// direct CALL the builder just emitted: `exit`, and the call's memory
    /// input when it is the `MonitorEnter` the builder put there (both the
    /// static shape and `enter_receiver_monitor` leave it so). Anything else
    /// records the exit alone, which only keeps more pairs apart.
    pub(crate) fn note_sync_call_pair(&mut self, call: NodeId, exit: NodeId) {
        let enter = self
            .nodes
            .get(call as usize)
            .and_then(|c| c.input_opt(1))
            .filter(|&e| {
                self.nodes
                    .get(e as usize)
                    .is_some_and(|n| n.op == Op::MonitorEnter)
            });
        self.sync_call_monitor_ops.extend(enter);
        self.sync_call_monitor_ops.push(exit);
    }
}

/// SY16-3: are `exit` and `enter` both halves of caller-held CALL pairs, or
/// both not? Only then may `coarsenable_monitor_pair` merge them.
pub(super) fn sync_call_halves_agree(graph: &Graph, exit: NodeId, enter: NodeId) -> bool {
    let held = |n: NodeId| graph.sync_call_monitor_ops.contains(&n);
    held(exit) == held(enter)
}

// ── MON17W2-2: a field monitor through a non-receiver base ─────────────
//
// A coarsened `synchronized (h.lock)` block re-reads `h.lock` in copies 1..K,
// and that re-read has no honest frame (`plan_loop_block_coarsening`), so it
// must not be able to leave: its null test of `h` must be gone. Until now only
// the receiver parameter had that (the lowering's per-block `this` seed).
//
// The proof for any invariant base is copy 0's own read of `h.lock`. It runs
// at copy 0's pre-enter frame, before any monitor of the group is entered, and
// it cannot complete with `h` null: its null test leaves (the helper raises
// the NPE with its JEP 358 message, at the read's bci, with nothing held).
// Every block it STRICTLY dominates therefore sees `h` non-null; the read's
// own block proves `h` from the test on, which the lowering's block-local
// proof (`note_receiver_non_null`) already records. The null-holder and
// null-lock throws are untouched: both still come from copy 0 (the read's
// NPE, or the guard on the read before the enter), exactly as without the
// coarsening.
//
// The planner admits the base under the switch and the rewrite records
// `(base, copy 0's read)` (`Graph::monitor_base_proofs`); the lowering seeds
// the base in each block the read's block strictly dominates
// ([`dominated_monitor_bases`]). The decision is the record: nothing else
// seeds from it.

/// `CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_DOMINATED_FIELD_BASE=1` (opt-in; j27
/// wave 4, lane MONROWS, MON17W2-2): the loop block coarsening also takes a
/// field monitor whose invariant base is not the receiver, seeding the base
/// non-null below copy 0's read. Read per planned loop (no `static`).
pub(super) fn ir_loop_lock_coarsen_dominated_field_base_enabled() -> bool {
    false}

/// The rewrite's half: copy 0's read `l0` of a field monitor proves its base
/// for the code it dominates. Recorded only under the switch, and only for a
/// field read (`Load` with a base input), so the switch-off graph is the old
/// one.
pub(super) fn note_field_monitor_base_proof(graph: &mut Graph, is_field: bool, l0: NodeId) {
    if !is_field || !ir_loop_lock_coarsen_dominated_field_base_enabled() {
        return;
    }
    let base = graph
        .nodes
        .get(l0 as usize)
        .filter(|n| matches!(n.op, Op::Load(MemKind::Ref)))
        .and_then(|n| n.input_opt(2));
    if let Some(base) = base {
        graph.monitor_base_proofs.push((base, l0));
    }
}

/// The lowering's half: the bases to seed non-null at the start of `block`,
/// i.e. those whose proof's block (`block_of`; `None` for a node that was not
/// scheduled) strictly dominates `block` (`dominates` is reflexive, as
/// `ir_schedule::Dominators::dominates` is).
pub(crate) fn dominated_monitor_bases(
    graph: &Graph,
    block: usize,
    block_of: impl Fn(NodeId) -> Option<usize>,
    dominates: impl Fn(usize, usize) -> bool,
) -> Vec<NodeId> {
    graph
        .monitor_base_proofs
        .iter()
        .filter(|&&(base, proof)| {
            proof_still_reads(graph, base, proof)
                && block_of(proof).is_some_and(|pb| pb != block && dominates(pb, block))
        })
        .map(|&(base, _)| base)
        .collect()
}

/// Is `proof` still a live reference field read through `base`? A pass that
/// killed or rewrote it (load forwarding, GVN) leaves no null test behind, so
/// its record proves nothing any more.
fn proof_still_reads(graph: &Graph, base: NodeId, proof: NodeId) -> bool {
    graph.nodes.get(proof as usize).is_some_and(|n| {
        n.op == Op::Load(MemKind::Ref) && n.input_opt(2) == Some(base)
    })
}


