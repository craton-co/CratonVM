// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 4 (lane MONROWS): two monitor rows of the round-18 backlog
//! (`docs/known-issues/jit/j27-r18-backlog-live-rows-20261028.md`).
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
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_ARRAY_OPS")
}

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
    cratonvm_types::flags::runtime_flag_on(
        "CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_DOMINATED_FIELD_BASE",
    )
}

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

#[cfg(test)]
mod tests {
    use super::super::{
        coarsen_adjacent_monitors, coarsen_loop_sync_windows, plan_loop_block_coarsening,
        LoopBlockPlan, LOOP_LOCK_COARSEN_FACTOR,
    };
    use super::*;
    use crate::ir::{CmpOp, IrType, SafepointSnapshot, NO_NODE};

    const ARRAY_OPS: &str = "CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_ARRAY_OPS";
    const BLOCKS: &str = "CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_BLOCKS";
    const STATIC_STORES: &str = "CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_STATIC_STORES";
    const ON: &[(&str, Option<&str>)] = &[
        (BLOCKS, Some("1")),
        (ARRAY_OPS, Some("1")),
        (STATIC_STORES, Some("0")),
    ];
    const OFF: &[(&str, Option<&str>)] = &[
        (BLOCKS, Some("1")),
        (ARRAY_OPS, None),
        (STATIC_STORES, Some("0")),
    ];
    const REFUSED: &str =
        "a body node could call, allocate or resume elsewhere with the monitor held";
    /// The class declaring `LOCK` (field 0) and `int[] A` (field 1).
    const HOLDER: u32 = 7;

    // ── MON17-5 ─────────────────────────────────────────────────────────

    #[test]
    fn only_deopt_only_array_ops_are_candidates() {
        assert!(is_deopt_only_array_op(&Op::ArrayLength));
        for kind in [MemKind::Int, MemKind::Long, MemKind::Byte, MemKind::Double] {
            assert!(is_deopt_only_array_op(&Op::ArrayLoad(kind)), "{kind:?}");
            assert!(is_deopt_only_array_op(&Op::ArrayStore(kind)), "{kind:?}");
        }
        assert!(
            !is_deopt_only_array_op(&Op::ArrayStore(MemKind::Ref)),
            "aastore calls jit_aastore"
        );
        assert!(
            !is_deopt_only_array_op(&Op::ArrayLoad(MemKind::Ref)),
            "a reference element load may call the ZGC barrier helper"
        );
        assert!(!is_deopt_only_array_op(&Op::Load(MemKind::Int)));
        assert!(!is_deopt_only_array_op(&Op::Call { info_ptr: 0 }));
    }

    #[test]
    fn the_array_switch_is_opt_in() {
        let op = Op::ArrayLoad(MemKind::Int);
        assert!(!cratonvm_types::flags::with_thread_overrides(OFF, || {
            block_admits_array_op(&op)
        }));
        assert!(cratonvm_types::flags::with_thread_overrides(ON, || {
            block_admits_array_op(&op)
        }));
    }

    struct Fx {
        g: Graph,
        header: NodeId,
        elem: NodeId,
        store: NodeId,
    }

    /// `static void ring(int n) { for (int i = 0; i < n; i++) synchronized
    /// (LOCK) { A[i] = A[i] + 1; } }`, built as the round-16 static-block
    /// fixture builds `shortCs`: locals `n`, `i`, the monitor temp; the read of
    /// `LOCK` (bci 7) is the enter's memory input and object, a null guard on
    /// it at the enter (12), the read of `A` (13), the element load (17), the
    /// add (21), the element store (22), the exit (25) on the back edge, and a
    /// frame at every bci inside the block that can leave.
    fn ring() -> Fx {
        let mut g = Graph::empty(48);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let n = g.add(Op::Param(0), IrType::Int, vec![start], None);
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let null = g.add(Op::Const(0), IrType::Ref, vec![], None);
        let header = g.add(Op::Merge, IrType::Control, vec![c0], Some(2));
        let mem = g.add(Op::Phi, IrType::Memory, vec![header, m0], Some(2));
        let iv = g.add(Op::Phi, IrType::Int, vec![header, zero], Some(2));
        let cmp = g.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![iv, n], Some(4));
        let test = g.add(Op::If, IrType::Control, vec![header, cmp], Some(4));
        let out = g.add(Op::Proj(0), IrType::Control, vec![test], Some(4));
        let body = g.add(Op::Proj(1), IrType::Control, vec![test], Some(4));
        let get = |field_index: u32| Op::LoadStatic {
            class_id: HOLDER,
            field_index,
            type_tag: b'L',
            is_volatile: false,
        };
        let read = g.add(get(0), IrType::Ref, vec![body, mem], Some(7));
        let nonnull = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![read, null], Some(12));
        g.add(
            Op::Guard { bci: 12 },
            IrType::Void,
            vec![body, nonnull],
            Some(12),
        );
        let enter = g.add(
            Op::MonitorEnter,
            IrType::Memory,
            vec![body, read, read],
            Some(12),
        );
        let a = g.add(get(1), IrType::Ref, vec![body, enter], Some(13));
        let elem = g.add(
            Op::ArrayLoad(MemKind::Int),
            IrType::Int,
            vec![body, a, a, iv],
            Some(17),
        );
        let inc = g.add(Op::Add, IrType::Int, vec![elem, one], Some(21));
        let store = g.add(
            Op::ArrayStore(MemKind::Int),
            IrType::Memory,
            vec![body, elem, a, iv, inc],
            Some(22),
        );
        let exit = g.add(
            Op::MonitorExit,
            IrType::Memory,
            vec![body, store, read],
            Some(25),
        );
        let iv2 = g.add(Op::Add, IrType::Int, vec![iv, one], Some(26));
        g.exit = g.add(Op::Return, IrType::Void, vec![out, iv], Some(32));
        g.push_input(header, body);
        g.push_input(mem, exit);
        g.push_input(iv, iv2);
        g.direct_static_fields = vec![(HOLDER, 0), (HOLDER, 1)];
        let locals = vec![n, iv, read];
        g.safepoints.push(SafepointSnapshot::new(
            2,
            vec![n, iv, NO_NODE],
            vec![],
            vec![],
        ));
        g.safepoints.push(SafepointSnapshot::new(
            12,
            locals.clone(),
            vec![read],
            vec![],
        ));
        for (bci, stack) in [
            (13, vec![]),
            (17, vec![a, iv, a, iv]),
            (21, vec![a, iv, elem]),
            (22, vec![a, iv, inc]),
            (25, vec![read]),
        ] {
            g.safepoints.push(SafepointSnapshot::new(
                bci,
                locals.clone(),
                stack,
                vec![read],
            ));
        }
        Fx {
            g,
            header,
            elem,
            store,
        }
    }

    fn plan(flags: &[(&str, Option<&str>)]) -> Result<LoopBlockPlan, Option<&'static str>> {
        let _scope = crate::ir::IrBuildResultsScope::enter();
        let fx = ring();
        cratonvm_types::flags::with_thread_overrides(flags, || {
            plan_loop_block_coarsening(&fx.g, fx.header)
        })
    }

    /// Off (the default): the element accesses are refused exactly as before.
    #[test]
    fn switched_off_an_array_block_is_refused_as_before() {
        assert_eq!(plan(OFF).err(), Some(Some(REFUSED)));
    }

    /// On: planned, and both element accesses are nodes that leave, each with
    /// the frame of its own bci (which holds the monitor once).
    #[test]
    fn switched_on_the_element_accesses_leave_with_a_frame_of_their_own() {
        let fx = ring();
        let p = plan(ON).expect("planned");
        assert!(p.leaving.contains(&fx.elem), "{:?}", p.leaving);
        assert!(p.leaving.contains(&fx.store), "{:?}", p.leaving);
        assert!(p.frames.contains_key(&17) && p.frames.contains_key(&22));
    }

    /// On, rewritten: the original body and K copies each load and store the
    /// element once, and every copy's element access names a frame at its own
    /// bci that holds exactly one monitor.
    #[test]
    fn switched_on_each_copy_accesses_the_element_once_under_one_hold() {
        let _scope = crate::ir::IrBuildResultsScope::enter();
        let mut fx = ring();
        let rewritten = cratonvm_types::flags::with_thread_overrides(ON, || {
            coarsen_loop_sync_windows(&mut fx.g)
        });
        assert_eq!(rewritten, 1);
        let g = &fx.g;
        let k = LOOP_LOCK_COARSEN_FACTOR;
        let loads: Vec<NodeId> = (0..g.nodes.len() as NodeId)
            .filter(|&id| g.nodes[id as usize].op == Op::ArrayLoad(MemKind::Int))
            .collect();
        let stores = g
            .nodes
            .iter()
            .filter(|n| n.op == Op::ArrayStore(MemKind::Int))
            .count();
        assert_eq!(loads.len(), 1 + k, "the original and one per copy");
        assert_eq!(stores, 1 + k);
        for &id in loads.iter().filter(|&&id| id != fx.elem) {
            let sp = g.nodes[id as usize]
                .frame_snapshot
                .and_then(|si| g.safepoints.get(si as usize))
                .expect("a copy's element load names its own frame");
            assert_eq!(sp.bci, 17);
            assert_eq!(sp.monitors.len(), 1, "n{id} holds the monitor once");
        }
    }

    // ── SY16-3 ──────────────────────────────────────────────────────────

    /// `synchronized (o) { } o.syncCallee()` with the callee bound as a
    /// caller-held CALL: `enter(o) exit(o)` from the block, then `enter(o)
    /// Call exit(o)` from the call. `noted`: the builder recorded the call's
    /// pair. Returns the graph and the four monitor ops.
    fn block_then_call(noted: bool) -> (Graph, [NodeId; 4]) {
        let mut g = Graph::empty(16);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let o = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let e1 = g.add(Op::MonitorEnter, IrType::Memory, vec![c0, m0, o], Some(2));
        let x1 = g.add(Op::MonitorExit, IrType::Memory, vec![c0, e1, o], Some(4));
        let e2 = g.add(Op::MonitorEnter, IrType::Memory, vec![c0, x1, o], Some(6));
        let call = g.add(
            Op::Call { info_ptr: 0 },
            IrType::Void,
            vec![c0, e2, o],
            Some(6),
        );
        let x2 = g.add(Op::MonitorExit, IrType::Memory, vec![c0, call, o], Some(6));
        g.exit = g.add(Op::Return, IrType::Void, vec![c0], Some(9));
        if noted {
            g.note_sync_call_pair(call, x2);
        }
        (g, [e1, x1, e2, x2])
    }

    #[test]
    fn the_builder_record_names_both_halves_of_the_call_pair() {
        let (g, [_, _, e2, x2]) = block_then_call(true);
        assert_eq!(g.sync_call_monitor_ops, vec![e2, x2]);
    }

    /// The positive control: unrecorded, the block's exit and the call's
    /// enter merge (the pre-j27 behaviour, which left the call without an
    /// enter beneath it).
    #[test]
    fn unrecorded_the_block_exit_merges_with_the_call_enter() {
        let (mut g, [_, x1, e2, _]) = block_then_call(false);
        assert_eq!(coarsen_adjacent_monitors(&mut g), 1);
        assert_eq!(g.nodes[x1 as usize].op, Op::Dead);
        assert_eq!(g.nodes[e2 as usize].op, Op::Dead);
    }

    /// Recorded, the call's pair stays whole: nothing merges.
    #[test]
    fn recorded_a_block_half_never_merges_with_a_call_half() {
        let (mut g, ops) = block_then_call(true);
        assert_eq!(coarsen_adjacent_monitors(&mut g), 0);
        for id in ops {
            assert_ne!(g.nodes[id as usize].op, Op::Dead, "n{id} kept");
        }
    }

    /// Two recorded call pairs back to back still merge, as before: the
    /// lowering's chain walk serves that shape.
    #[test]
    fn two_call_pairs_still_merge_with_each_other() {
        let mut g = Graph::empty(16);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let o = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let e1 = g.add(Op::MonitorEnter, IrType::Memory, vec![c0, m0, o], Some(2));
        let call1 = g.add(
            Op::Call { info_ptr: 0 },
            IrType::Void,
            vec![c0, e1, o],
            Some(2),
        );
        let x1 = g.add(Op::MonitorExit, IrType::Memory, vec![c0, call1, o], Some(2));
        let e2 = g.add(Op::MonitorEnter, IrType::Memory, vec![c0, x1, o], Some(5));
        let call2 = g.add(
            Op::Call { info_ptr: 0 },
            IrType::Void,
            vec![c0, e2, o],
            Some(5),
        );
        let x2 = g.add(Op::MonitorExit, IrType::Memory, vec![c0, call2, o], Some(5));
        g.exit = g.add(Op::Return, IrType::Void, vec![c0], Some(8));
        g.note_sync_call_pair(call1, x1);
        g.note_sync_call_pair(call2, x2);
        assert_eq!(coarsen_adjacent_monitors(&mut g), 1);
        assert_eq!(g.nodes[x1 as usize].op, Op::Dead);
        assert_eq!(g.nodes[e2 as usize].op, Op::Dead);
    }

    // ── MON17W2-2 ───────────────────────────────────────────────────────

    const FIELD_BASE: &str = "CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_DOMINATED_FIELD_BASE";
    const INSTANCE: &str = "CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_INSTANCE_BLOCKS";
    const BASE_ON: &[(&str, Option<&str>)] = &[(INSTANCE, Some("1")), (FIELD_BASE, Some("1"))];
    const BASE_OFF: &[(&str, Option<&str>)] = &[(INSTANCE, Some("1")), (FIELD_BASE, None)];
    const NOT_RECEIVER: &str =
        "the monitor's getfield base is not the receiver (its re-read would need a null check)";

    struct HolderFx {
        g: Graph,
        header: NodeId,
        h: NodeId,
        read: NodeId,
    }

    /// `static long holderLoop(Holder h, int n) { long s = 0; for (int i = 0;
    /// i < n; i++) synchronized (h.lock) { s += i; } return s; }`: locals `h`,
    /// `n`, `s`, `i`, the monitor temp. The read of `h.lock` (bci 7) is the
    /// enter's memory input and object, a null guard on it at the enter (12),
    /// the exit (20) on the back edge. `h` is a parameter of a static method,
    /// so no receiver seed covers it.
    fn holder_loop() -> HolderFx {
        let mut g = Graph::empty(40);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let h = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let n = g.add(Op::Param(1), IrType::Int, vec![start], None);
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let null = g.add(Op::Const(0), IrType::Ref, vec![], None);
        let lock_off = g.add(Op::Const(12), IrType::Int, vec![], None);
        let header = g.add(Op::Merge, IrType::Control, vec![c0], Some(2));
        let mem = g.add(Op::Phi, IrType::Memory, vec![header, m0], Some(2));
        let iv = g.add(Op::Phi, IrType::Int, vec![header, zero], Some(2));
        let s = g.add(Op::Phi, IrType::Int, vec![header, zero], Some(2));
        let cmp = g.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![iv, n], Some(4));
        let test = g.add(Op::If, IrType::Control, vec![header, cmp], Some(4));
        let out = g.add(Op::Proj(0), IrType::Control, vec![test], Some(4));
        let body = g.add(Op::Proj(1), IrType::Control, vec![test], Some(4));
        let read = g.add(
            Op::Load(MemKind::Ref),
            IrType::Ref,
            vec![body, mem, h, lock_off],
            Some(7),
        );
        let nonnull = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![read, null], Some(12));
        g.add(
            Op::Guard { bci: 12 },
            IrType::Void,
            vec![body, nonnull],
            Some(12),
        );
        let enter = g.add(
            Op::MonitorEnter,
            IrType::Memory,
            vec![body, read, read],
            Some(12),
        );
        let s2 = g.add(Op::Add, IrType::Int, vec![s, iv], Some(16));
        let exit = g.add(
            Op::MonitorExit,
            IrType::Memory,
            vec![body, enter, read],
            Some(20),
        );
        let iv2 = g.add(Op::Add, IrType::Int, vec![iv, one], Some(26));
        g.exit = g.add(Op::Return, IrType::Int, vec![out, s], Some(32));
        g.push_input(header, body);
        g.push_input(mem, exit);
        g.push_input(iv, iv2);
        g.push_input(s, s2);
        g.safepoints.push(SafepointSnapshot::new(
            2,
            vec![h, n, s, iv, NO_NODE],
            vec![],
            vec![],
        ));
        g.safepoints.push(SafepointSnapshot::new(
            7,
            vec![h, n, s, iv, NO_NODE],
            vec![h],
            vec![],
        ));
        g.safepoints.push(SafepointSnapshot::new(
            12,
            vec![h, n, s, iv, read],
            vec![read],
            vec![],
        ));
        g.safepoints.push(SafepointSnapshot::new(
            20,
            vec![h, n, s2, iv, read],
            vec![read],
            vec![read],
        ));
        HolderFx { g, header, h, read }
    }

    fn holder_plan(
        flags: &[(&str, Option<&str>)],
    ) -> Result<LoopBlockPlan, Option<&'static str>> {
        let _scope = crate::ir::IrBuildResultsScope::enter();
        let fx = holder_loop();
        cratonvm_types::flags::with_thread_overrides(flags, || {
            plan_loop_block_coarsening(&fx.g, fx.header)
        })
    }

    /// Off (the default): a non-receiver base is refused exactly as before.
    #[test]
    fn switched_off_a_non_receiver_field_monitor_is_refused_as_before() {
        assert_eq!(holder_plan(BASE_OFF).err(), Some(Some(NOT_RECEIVER)));
    }

    /// On: planned as a field monitor.
    #[test]
    fn switched_on_a_non_receiver_field_monitor_is_planned() {
        let p = holder_plan(BASE_ON).expect("planned");
        assert_eq!(p.monitor, super::super::BlockMonitor::Field);
    }

    /// On, rewritten: the graph names copy 0's read of `h.lock` -- the node
    /// whose own null test (or NPE) runs before any monitor is entered -- as
    /// the proof of `h` for the code it dominates, and nothing else. Copy 0's
    /// read is the fast path's first read and the fast enter's object, and is
    /// not the original read (which stays the slow path's).
    #[test]
    fn switched_on_copy_zeros_read_is_recorded_as_the_base_proof() {
        let _scope = crate::ir::IrBuildResultsScope::enter();
        let mut fx = holder_loop();
        let rewritten = cratonvm_types::flags::with_thread_overrides(BASE_ON, || {
            coarsen_loop_sync_windows(&mut fx.g)
        });
        assert_eq!(rewritten, 1);
        let [(base, proof)] = fx.g.monitor_base_proofs.as_slice() else {
            panic!("one proof: {:?}", fx.g.monitor_base_proofs);
        };
        assert_eq!(*base, fx.h);
        assert_ne!(*proof, fx.read, "the slow path's read proves nothing for the fast copies");
        assert_eq!(fx.g.nodes[*proof as usize].op, Op::Load(MemKind::Ref));
        assert_eq!(fx.g.nodes[*proof as usize].inputs[2], fx.h);
        let enters_on_it = fx.g.nodes.iter().any(|n| {
            n.op == Op::MonitorEnter && n.inputs.get(2) == Some(proof)
        });
        assert!(enters_on_it, "copy 0's read is the fast enter's object");
    }

    /// Off, rewritten nothing and recorded nothing.
    #[test]
    fn switched_off_nothing_is_recorded() {
        let _scope = crate::ir::IrBuildResultsScope::enter();
        let mut fx = holder_loop();
        let rewritten = cratonvm_types::flags::with_thread_overrides(BASE_OFF, || {
            coarsen_loop_sync_windows(&mut fx.g)
        });
        assert_eq!(rewritten, 0);
        assert!(fx.g.monitor_base_proofs.is_empty());
    }

    /// The lowering's question: a base is seeded only in a block its proof's
    /// block STRICTLY dominates. Never in the proof's own block (the proof's
    /// null test runs there, after the block starts), never in a block the
    /// proof does not dominate (the loop header, the slow path), never for a
    /// proof that was not scheduled.
    #[test]
    fn a_base_is_seeded_only_below_its_proofs_block() {
        let mut g = Graph::empty(8);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let h = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let off = g.add(Op::Const(12), IrType::Int, vec![], None);
        let read = |g: &mut Graph| {
            g.add(
                Op::Load(MemKind::Ref),
                IrType::Ref,
                vec![start, start, h, off],
                Some(7),
            )
        };
        let proof = read(&mut g);
        let unscheduled = read(&mut g);
        g.monitor_base_proofs = vec![(h, proof), (h, unscheduled)];
        // Blocks: 0 entry, 1 the proof's, 2 below it, 3 beside it.
        let block_of = |id: NodeId| (id == proof).then_some(1usize);
        let dominates = |a: usize, b: usize| a == b || a == 0 || (a == 1 && b == 2);
        let seeded = |g: &Graph, b: usize| dominated_monitor_bases(g, b, block_of, dominates);
        assert_eq!(seeded(&g, 2), vec![h], "strictly below the proof");
        assert!(seeded(&g, 1).is_empty(), "not in the proof's own block");
        assert!(seeded(&g, 3).is_empty(), "not beside it");
        assert!(seeded(&g, 0).is_empty(), "not above it");
        // A proof a later pass killed proves nothing.
        g.nodes[proof as usize].op = Op::Dead;
        assert!(seeded(&g, 2).is_empty(), "a dead proof seeds nothing");
    }
}
