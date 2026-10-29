// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 2, lane RESIDENCY: the loop half of the linear scan's spill cost
//! (`CRATONVM_JIT_IR_LOOP_PHI_RESIDENCY`, opt-in).
//!
//! # The imbalance this corrects
//!
//! `ls_pick_victim` divides the next-use distance of every
//! [`LiveModel::carried_and_read`] value by `ls_carry_relief()` (64), which
//! makes the score of every carried value in a loop body shorter than 64
//! positions zero. "Carried and read" covers two populations whose eviction
//! costs differ:
//!
//! * a value REDEFINED on every trip (a loop-header phi, an unrolled
//!   temporary): out of a register it costs its definition's store and a
//!   reload per read, every trip;
//! * a loop INVARIANT (defined before the loop, read in it): one definition
//!   serves every trip, so out of a register it costs only the reads -- and x86
//!   folds most of those into a memory operand (`cmp ebx, [rbp-x]`).
//!
//! Both scored zero, so under pressure (System V's five-register file, the
//! `run` kernel of `jr21-promoted-loop-bodies-spill-and-poll-every-trip`:
//! three cells and a counter against three invariants) the tie went to the
//! next-use distance and could evict a phi to keep an invariant, or a body
//! temporary re-arriving at its read could take a phi's register mid-loop (a
//! split phi is refused outright by `plan_register_residency`, so the phi then
//! lives in its home for the whole method).
//!
//! # The rule (one decision, [`loop_def_role`])
//!
//! A value's role is read off the liveness model: the loop depth of its
//! definition against the deepest loop that reads it. Under the switch:
//!
//! * an [`LoopDefRole::Invariant`] gets no carry relief (it competes by
//!   distance and weight like an uncarried value);
//! * a [`LoopDefRole::PerTrip`] value's weight counts its definition too
//!   ([`loop_spill_weight`]: uses + one definition per trip, the C2 / LLVM
//!   spill-weight shape), and a carried per-trip phi loses every tie on
//!   score to any other candidate, the arriving value included
//!   ([`needed_every_trip`], `ls_pick_victim_with`);
//! * among those phis, one of a main/post unroll's remainder loop yields to
//!   one of its main loop ([`eviction_rank`], j27 wave 5);
//! * the loop-carried reservation (`ir_lower::carried_reservation`) orders its
//!   candidates by the same weight, so a phi outranks an invariant read as
//!   often.
//!
//! Only WHICH value wins a register changes. Every safety property is the
//! allocator's and the residency plan's as before: a `Ref` is still refused by
//! type, a split value is still refused, a deopt-named value keeps its home
//! unless `deopt_nameable` / `deopt_named_reachable` say otherwise, and
//! `verify_allocation` still proves the result.

use super::{LiveModel, LOOP_WEIGHT_PER_DEPTH, MAX_LOOP_DEPTH_WEIGHT};
use crate::ir::{Graph, NodeId, Op};

/// `CRATONVM_JIT_IR_LOOP_PHI_RESIDENCY=1` (perf, **opt-in**; j27 wave 2, lane
/// RESIDENCY). Off is the pre-j27 spill score byte for byte. Opt-in because the
/// allocator's earlier "admit more under pressure" levers measured slower on
/// `probes/RegPressure.java` (`ir_residency_crossblock_enabled`) and this one
/// could not be measured where it was written. Read once per allocation
/// (`allocate_linear_scan`) and once per reservation; no `static`.
pub(crate) fn loop_phi_residency_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_LOOP_PHI_RESIDENCY")
}

/// How a value's definition relates to the innermost loop that reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LoopDefRole {
    /// Defined inside a loop, at least as deep as its deepest read: redefined
    /// on every trip (a header phi, a body temporary, an exit value).
    PerTrip,
    /// Read in a loop deeper than its definition: one definition serves every
    /// trip of that loop.
    Invariant,
    /// Neither: defined and read outside every loop, or not placed by the model.
    Straight,
}

/// The loop depth of the block holding position `pos`, if the model places it.
fn depth_at(live: &LiveModel, pos: usize) -> Option<u32> {
    let block = *live.block_of_pos.get(pos)?;
    live.loop_depth.get(block).copied()
}

/// The loop depth of `id`'s definition.
fn def_depth(live: &LiveModel, id: usize) -> Option<u32> {
    let pos = live.pos_of.get(id).copied().flatten()?;
    depth_at(live, pos)
}

/// `LOOP_WEIGHT_PER_DEPTH^depth`, capped -- the frequency `build_live_model`
/// weighs a read at that depth by.
fn frequency(depth: u32) -> u64 {
    u64::from(
        LOOP_WEIGHT_PER_DEPTH
            .checked_pow(depth)
            .unwrap_or(MAX_LOOP_DEPTH_WEIGHT)
            .min(MAX_LOOP_DEPTH_WEIGHT),
    )
}

/// The role of `id` (see [`LoopDefRole`]). Pure; reads the model only.
pub(crate) fn loop_def_role(live: &LiveModel, id: usize) -> LoopDefRole {
    let Some(def) = def_depth(live, id) else {
        return LoopDefRole::Straight;
    };
    let read = live.uses.get(id).map_or(0, |uses| {
        uses.iter()
            .filter_map(|&u| depth_at(live, u))
            .max()
            .unwrap_or(0)
    });
    if read > def {
        LoopDefRole::Invariant
    } else if def > 0 {
        LoopDefRole::PerTrip
    } else {
        LoopDefRole::Straight
    }
}

/// The cost of keeping `id` out of a register: its loop-weighted reads
/// (`LiveModel::weight`), plus its definition's frequency when it is
/// redefined every trip (out of a register that definition is a store).
pub(crate) fn loop_spill_weight(live: &LiveModel, id: usize) -> u64 {
    let reads = live.weight.get(id).copied().unwrap_or(0);
    if loop_def_role(live, id) != LoopDefRole::PerTrip {
        return reads;
    }
    reads.saturating_add(def_depth(live, id).map_or(0, frequency))
}

/// The weight `ls_pick_victim_with` and the carried reservation rank `id` by:
/// [`loop_spill_weight`] under the switch, the read weight otherwise.
pub(crate) fn ranking_weight(live: &LiveModel, id: usize, loop_aware: bool) -> u64 {
    if loop_aware {
        loop_spill_weight(live, id)
    } else {
        live.weight.get(id).copied().unwrap_or(0)
    }
}

/// Does `id` keep the carry relief? Every carried-and-read value without the
/// switch; under it, every one but a loop invariant.
pub(crate) fn keeps_carry_relief(live: &LiveModel, id: usize, loop_aware: bool) -> bool {
    let carried = live.carried_and_read.get(id).copied().unwrap_or(false);
    carried && !(loop_aware && loop_def_role(live, id) == LoopDefRole::Invariant)
}

/// Under the switch, a carried PHI redefined every trip is needed again on
/// the next trip whatever its next-use distance says, so it loses every tie on
/// score (`ls_pick_victim_with`'s `evictable` key, ahead of the distance).
///
/// j27 wave 4 (lane RESIDFLIP) narrowed this in two ways:
///
/// * A phi only. A back-edge source (`c' = c ^ t`, `i' = i + 1`) is
///   carried-and-read and per-trip too, because `build_live_model` reads it at
///   the edge position. But its value is not needed next trip: the phi's is.
///   And a split phi loses all its residency (`plan_register_residency`
///   refuses it), while a split source loses one register.
/// * A key of its own, not a zero distance. Zero is also the real distance of
///   a value at its last read, so the old form tied such a value with every
///   relieved phi, and "evict an active, not the arrival" then split the phi.
///
/// The j27 flip of `CRATONVM_JIT_IR_RESIDENCY_FRAME_PINNED` put exactly those
/// sources into the pressure loop's scan.
pub(crate) fn needed_every_trip(
    graph: &Graph,
    live: &LiveModel,
    id: usize,
    loop_aware: bool,
) -> bool {
    loop_aware
        && graph.nodes.get(id).is_some_and(|node| node.op == Op::Phi)
        && live.carried_and_read.get(id).copied().unwrap_or(false)
        && loop_def_role(live, id) == LoopDefRole::PerTrip
}

/// The eviction key part `ls_pick_victim_with` places right after the score:
/// among equal scores the HIGHER rank is evicted first.
///
/// * Off: 1 for every value (the pre-j27 order, byte for byte).
/// * On: 0 for a [`needed_every_trip`] phi of an ordinary or MAIN loop, 1 for
///   one of a main/post unroll's REMAINDER loop ([`in_remainder_loop`]), 2 for
///   every other value.
///
/// j27 wave 5 (lane UNROLLRES): the defined loser of an unrolled loop. A
/// main/post unroll has eight carried phis (four per loop) and the live model
/// gives each one interval, from its first incoming edge to its last. Laid
/// out, the remainder's phis can begin inside the main loop, so all eight
/// overlap there, and the five-register file cannot hold them. Before this
/// rank, the next-use distance (the layout) chose which phi was split. The
/// remainder runs at most `factor - 1` trips per entry, so its phis yield.
pub(crate) fn eviction_rank(graph: &Graph, live: &LiveModel, id: usize, loop_aware: bool) -> u8 {
    if !loop_aware {
        return 1;
    }
    if !needed_every_trip(graph, live, id, true) {
        return 2;
    }
    u8::from(in_remainder_loop(graph, id))
}

/// Is `id` a phi of the remainder (post) loop of a main/post unroll?
fn in_remainder_loop(graph: &Graph, id: usize) -> bool {
    let header = graph.nodes.get(id).and_then(|phi| phi.input_opt(0));
    header.is_some_and(|h| is_remainder_header(graph, h))
}

/// The post loop of `ir_optimize::main_post_unroll_loop`: a partially unrolled
/// header WITH a bci (the original loop), entered from a join that the exit of
/// a pc-less partially unrolled header (the main loop) reaches. A per-copy-test
/// unroll's header has a bci too, but it is entered from its preheader, so it
/// is not one.
fn is_remainder_header(graph: &Graph, header: NodeId) -> bool {
    let Some(node) = graph.node_opt(header) else {
        return false;
    };
    if node.bytecode_pc.is_none() || !graph.partially_unrolled_headers.contains(&header) {
        return false;
    }
    let join = node.input_opt(0).and_then(|j| graph.node_opt(j));
    join.is_some_and(|j| {
        j.op == Op::Merge && j.inputs.iter().any(|&ctrl| exits_a_main_header(graph, ctrl))
    })
}

/// Is `ctrl` a projection of the test of a main loop header (a partially
/// unrolled header without a bci)?
fn exits_a_main_header(graph: &Graph, ctrl: NodeId) -> bool {
    let test = graph
        .node_opt(ctrl)
        .filter(|proj| matches!(proj.op, Op::Proj(_)))
        .and_then(|proj| proj.input_opt(0))
        .and_then(|t| graph.node_opt(t))
        .filter(|t| t.op == Op::If);
    let header = test.and_then(|t| t.input_opt(0));
    header.is_some_and(|h| {
        graph.partially_unrolled_headers.contains(&h)
            && graph.node_opt(h).is_some_and(|n| n.bytecode_pc.is_none())
    })
}

#[cfg(test)]
mod tests {
    use super::super::{
        allocate_linear_scan, verify_allocation, MachineModel, PhysReg, PosRange, RegClass,
        RegFile, RegSpec, ValueLoc,
    };
    use super::*;
    use crate::ir::{Graph, IrType, NodeId, Op};

    /// One preheader block (positions 0..=2, depth 0) and one loop block
    /// (positions 3..=15, depth 1), with three values:
    ///
    /// * `inv` -- defined in the preheader at 1, read in the loop at 8 and 14
    ///   (a hoisted `k * SCALE`): carried, read, INVARIANT;
    /// * `phi` -- the header phi at 3, read at 6, 12 and 14 (an accumulator):
    ///   carried, read, PER TRIP;
    /// * `tmp` -- a body temporary at 5, read at 7 and 10: per trip, not carried.
    ///
    /// Two registers, so `tmp`'s arrival at 5 is a contest.
    struct LoopFixture {
        graph: Graph,
        live: LiveModel,
        inv: NodeId,
        phi: NodeId,
        tmp: NodeId,
    }

    fn loop_fixture() -> LoopFixture {
        let spec: [ValueSpec<'_>; 3] = [
            (Op::Mul, 1, 14, &[8, 14], 20, true),
            (Op::Phi, 3, 14, &[6, 12, 14], 30, true),
            (Op::Add, 5, 10, &[7, 10], 20, false),
        ];
        let (graph, live, ids) = model_of(&spec);
        LoopFixture {
            graph,
            live,
            inv: ids[0],
            phi: ids[1],
            tmp: ids[2],
        }
    }

    /// `(op, def, hi, uses, loop-weighted reads, carried and read)`.
    type ValueSpec<'a> = (Op, usize, usize, &'a [usize], u64, bool);

    /// A graph of `spec`'s `int` values over one preheader block (positions
    /// 0..=2, depth 0) and one loop block (positions 3..=15, depth 1).
    fn model_of(spec: &[ValueSpec<'_>]) -> (Graph, LiveModel, Vec<NodeId>) {
        model_over(Graph::empty(0), spec, &[])
    }

    /// [`model_of`] over a graph that already holds nodes (control nodes,
    /// which need no location); spec value `i` takes `inputs[i]`.
    fn model_over(
        mut graph: Graph,
        spec: &[ValueSpec<'_>],
        inputs: &[Vec<NodeId>],
    ) -> (Graph, LiveModel, Vec<NodeId>) {
        const TOTAL: usize = 16;
        let mut live = LiveModel {
            pos_of: Vec::new(),
            span: vec![(0, 2), (3, TOTAL - 1)],
            total_positions: TOTAL,
            block_of_pos: (0..TOTAL).map(|p| usize::from(p >= 3)).collect(),
            wants_loc: Vec::new(),
            range: Vec::new(),
            class: Vec::new(),
            is_ref: Vec::new(),
            pinned: Vec::new(),
            uses: Vec::new(),
            weight: Vec::new(),
            loop_depth: vec![0, 1],
            carried: Vec::new(),
            carried_and_read: Vec::new(),
            peak_live: 3,
            converged: true,
            block_live_in: Vec::new(),
        };
        for _ in 0..graph.nodes.len() {
            push_no_location(&mut live);
        }
        let mut ids = Vec::new();
        for (i, &(ref op, lo, hi, uses, weight, carried)) in spec.iter().enumerate() {
            let ins = inputs.get(i).cloned().unwrap_or_default();
            ids.push(graph.add(op.clone(), IrType::Int, ins, None));
            live.pos_of.push(Some(lo));
            live.wants_loc.push(true);
            live.range.push(Some(PosRange { lo, hi }));
            live.class.push(RegClass::of(IrType::Int));
            live.is_ref.push(false);
            live.pinned.push(false);
            live.uses.push(uses.to_vec());
            live.weight.push(weight);
            live.carried.push(carried);
            live.carried_and_read.push(carried);
        }
        (graph, live, ids)
    }

    /// The model's row for a node with no value (a control node).
    fn push_no_location(live: &mut LiveModel) {
        live.pos_of.push(None);
        live.wants_loc.push(false);
        live.range.push(None);
        live.class.push(None);
        live.is_ref.push(false);
        live.pinned.push(false);
        live.uses.push(Vec::new());
        live.weight.push(0);
        live.carried.push(false);
        live.carried_and_read.push(false);
    }

    /// The control shape `ir_optimize::main_post_unroll_loop` leaves: the main
    /// header (no bci) and its test, whose exit reaches the join that enters
    /// the remainder header (the original loop, with its bci). Both headers
    /// are listed as partially unrolled. Returns `(graph, main, remainder)`.
    fn main_post_controls() -> (Graph, NodeId, NodeId) {
        let mut g = Graph::empty(0);
        let main = g.add(Op::Merge, IrType::Control, vec![], None);
        let test = g.add(Op::If, IrType::Control, vec![main], None);
        let exit = g.add(Op::Proj(1), IrType::Control, vec![test], None);
        let join = g.add(Op::Merge, IrType::Control, vec![exit], None);
        let post = g.add(Op::Merge, IrType::Control, vec![join], Some(20));
        g.partially_unrolled_headers.extend([post, main]);
        (g, main, post)
    }

    fn two_registers() -> MachineModel {
        registers(2)
    }

    fn registers(count: u8) -> MachineModel {
        MachineModel {
            regs: RegFile::from_specs((0..count).map(|num| RegSpec {
                reg: PhysReg::gp(num),
                caller_saved: false,
            })),
            clobbers: Vec::new(),
            fixed: Vec::new(),
            safepoints: Vec::new(),
            refs_may_cross_safepoints: false,
            reads_precede_clobber: Vec::new(),
        }
    }

    fn with_switch<R>(on: bool, f: impl FnOnce() -> R) -> R {
        cratonvm_types::flags::with_thread_overrides(
            &[(
                "CRATONVM_JIT_IR_LOOP_PHI_RESIDENCY",
                Some(if on { "1" } else { "0" }),
            )],
            f,
        )
    }

    /// Is `id` in a register at EVERY position of `lo..=hi`?
    fn in_register_throughout(
        alloc: &super::super::Allocation,
        id: NodeId,
        lo: usize,
        hi: usize,
    ) -> bool {
        (lo..=hi).all(|p| matches!(alloc.location_at(id, p), Some(ValueLoc::Reg(_))))
    }

    #[test]
    fn j27_residency_roles_follow_definition_depth_against_read_depth() {
        let f = loop_fixture();
        assert_eq!(loop_def_role(&f.live, f.inv as usize), LoopDefRole::Invariant);
        assert_eq!(loop_def_role(&f.live, f.phi as usize), LoopDefRole::PerTrip);
        assert_eq!(loop_def_role(&f.live, f.tmp as usize), LoopDefRole::PerTrip);
        // A per-trip value pays its definition once per trip (depth 1: 10).
        assert_eq!(loop_spill_weight(&f.live, f.phi as usize), 30 + 10);
        assert_eq!(loop_spill_weight(&f.live, f.inv as usize), 20);
        assert_eq!(ranking_weight(&f.live, f.phi as usize, false), 30);
        assert!(keeps_carry_relief(&f.live, f.inv as usize, false));
        assert!(!keeps_carry_relief(&f.live, f.inv as usize, true));
        assert!(keeps_carry_relief(&f.live, f.phi as usize, true));
        assert!(needed_every_trip(&f.graph, &f.live, f.phi as usize, true));
        assert!(!needed_every_trip(&f.graph, &f.live, f.tmp as usize, true), "not carried");
        assert!(!needed_every_trip(&f.graph, &f.live, f.phi as usize, false));
    }

    /// Off (the default): both carried values score zero, the temporary loses
    /// at its definition, re-arrives at its read (7) and then evicts the PHI,
    /// whose next read (12) is the farthest -- the phi is split, which
    /// `plan_register_residency` refuses, so it would live in its home.
    #[test]
    fn j27_residency_off_a_body_temporary_splits_the_phi() {
        let f = loop_fixture();
        let model = two_registers();
        let alloc = with_switch(false, || allocate_linear_scan(&f.graph, &f.live, &model))
            .expect("allocates");
        assert!(
            !in_register_throughout(&alloc, f.phi, 3, 14),
            "precondition of the A/B: without the switch the phi is split"
        );
        assert!(in_register_throughout(&alloc, f.inv, 1, 14), "the invariant kept r0");
        verify_allocation(&f.graph, &f.live, &model, &alloc).expect("verifies");
    }

    /// On: the invariant is the victim (no carry relief, one definition for
    /// every trip), the temporary takes its register, and the phi keeps ONE
    /// register over its whole range -- the shape the residency plan admits.
    #[test]
    fn j27_residency_on_the_invariant_yields_and_the_phi_stays_whole() {
        let f = loop_fixture();
        let model = two_registers();
        let alloc = with_switch(true, || allocate_linear_scan(&f.graph, &f.live, &model))
            .expect("allocates");
        assert!(
            in_register_throughout(&alloc, f.phi, 3, 14),
            "the per-trip phi must hold one register for its whole range"
        );
        assert!(
            in_register_throughout(&alloc, f.tmp, 5, 7),
            "the temporary is served from a register at its first read"
        );
        // The invariant's timeline under the switch: r0 [1,4] (evicted by
        // `tmp` at 5), memory [5,7], r0 [8,9] (re-queued at its read 8, it
        // evicts `tmp`), memory [10,13] (`tmp` re-arrives at its read 10 and
        // evicts it), r0 [14,14]. So it is in memory at 6 and 11 -- not at 9,
        // which the first version of this test asserted against its own trace.
        for pos in [6, 11] {
            assert!(
                !matches!(alloc.location_at(f.inv, pos), Some(ValueLoc::Reg(_))),
                "the invariant is the value that went to memory (position {pos})"
            );
        }
        verify_allocation(&f.graph, &f.live, &model, &alloc).expect("verifies");
    }

    /// j27 wave 4 (lane RESIDFLIP): a value re-arriving at its LAST read scores
    /// zero, exactly like a relieved phi. One register, so the phi holds it
    /// from 3 and the invariant and the temporary keep coming back for it.
    ///
    /// * Off: the phi is evicted at the temporary's read (7), split as before.
    /// * On: at the temporary's last read (10) the tie on (score 0, distance 0)
    ///   used to fall to "evict an active rather than the arrival", so the phi
    ///   lost its register there. The per-trip phi must lose every such tie:
    ///   it stays whole, and the arrival is read from its home.
    #[test]
    fn j27_residency_on_a_value_at_its_last_read_does_not_split_the_phi() {
        let f = loop_fixture();
        let model = registers(1);
        let off = with_switch(false, || allocate_linear_scan(&f.graph, &f.live, &model))
            .expect("allocates off");
        assert!(
            !in_register_throughout(&off, f.phi, 3, 14),
            "off is unchanged: the phi is split"
        );
        let on = with_switch(true, || allocate_linear_scan(&f.graph, &f.live, &model))
            .expect("allocates on");
        assert!(
            in_register_throughout(&on, f.phi, 3, 14),
            "the per-trip phi must hold the one register for its whole range"
        );
        assert!(
            !matches!(on.location_at(f.tmp, 10), Some(ValueLoc::Reg(_))),
            "the temporary's last read is served from its home"
        );
        verify_allocation(&f.graph, &f.live, &model, &on).expect("verifies");
    }

    /// j27 wave 4 (lane RESIDFLIP): the pressure loop's shape under the j27
    /// default `CRATONVM_JIT_IR_RESIDENCY_FRAME_PINNED`, which lets single-use
    /// back-edge sources into the scan. The phi `p` (header at 3, its edge
    /// copy at 15) and two sources `s` (12) and `s2` (13), each read once, by
    /// the edge copy at 15. `build_live_model` reads a source at the edge, so
    /// both are carried-and-read and per-trip, like the phi.
    ///
    /// Two registers, so `s2`'s arrival at 13 is a contest that every candidate
    /// scores 0 (all relieved). The old rule protected the sources as if they
    /// were phis, the tie fell to the lowest node id, and that was the phi:
    /// split at 13 and so refused. Now only the phi is protected: a source
    /// yields, and the phi keeps one register from 3 to 15.
    #[test]
    fn j27_residency_on_back_edge_sources_do_not_split_the_phi() {
        let spec: [ValueSpec<'_>; 3] = [
            (Op::Phi, 3, 15, &[6, 12], 30, true),
            (Op::Add, 12, 15, &[15], 10, true),
            (Op::Add, 13, 15, &[15], 10, true),
        ];
        let (graph, live, ids) = model_of(&spec);
        let (p, s) = (ids[0], ids[1]);
        assert!(needed_every_trip(&graph, &live, p as usize, true));
        assert!(
            !needed_every_trip(&graph, &live, s as usize, true),
            "a back-edge source is carried and per-trip, but not a phi"
        );
        let model = two_registers();
        let off = with_switch(false, || allocate_linear_scan(&graph, &live, &model))
            .expect("allocates off");
        assert!(
            !in_register_throughout(&off, p, 3, 15),
            "off is unchanged: the phi, the lowest id, is split at 13"
        );
        let on = with_switch(true, || allocate_linear_scan(&graph, &live, &model))
            .expect("allocates on");
        assert!(
            in_register_throughout(&on, p, 3, 15),
            "the phi must hold one register for its whole range"
        );
        verify_allocation(&graph, &live, &model, &on).expect("verifies");
    }

    /// j27 wave 5 (lane UNROLLRES): only a phi of a remainder header ranks 1.
    #[test]
    fn j27_residency_rank_names_the_remainder_phi_only() {
        let (controls, main, post) = main_post_controls();
        let spec: [ValueSpec<'_>; 3] = [
            (Op::Phi, 3, 15, &[10, 14], 30, true),
            (Op::Phi, 5, 15, &[6, 12], 30, true),
            (Op::Add, 6, 12, &[12], 10, false),
        ];
        let inputs = [vec![main], vec![post]];
        let (mut graph, live, ids) = model_over(controls, &spec, &inputs);
        let rank = |id: NodeId, on: bool| eviction_rank(&graph, &live, id as usize, on);
        assert_eq!(rank(ids[0], true), 0, "a main loop's phi");
        assert_eq!(rank(ids[1], true), 1, "a remainder loop's phi");
        assert_eq!(rank(ids[2], true), 2, "any other value");
        for &id in &ids {
            assert_eq!(rank(id, false), 1, "off: one rank for every value");
        }
        // The same remainder header entered from its preheader (a per-copy-test
        // unroll's header has a bci too) is an ordinary loop.
        assert!(graph.set_input(post, 0, main), "rewire the remainder's entry");
        assert_eq!(
            eviction_rank(&graph, &live, ids[1] as usize, true),
            0,
            "a header not entered from a main loop's exit"
        );
    }

    /// j27 wave 5 (lane UNROLLRES,
    /// `docs/internal/jit/j27-loop-phi-residency-misses-unrolled-pressure-loop-FIXED-20261029.md`):
    /// a main loop's phi `m` (3..15, read at 10 and 14) and its remainder
    /// loop's phi `p` (5..15, read at 6 and 12) overlap, as the live model's
    /// one-interval phis of a main/post unroll do, over one register.
    ///
    /// * Off, and on before this wave: at `p`'s arrival (5) both score 0 and
    ///   rank alike, `m`'s next read (10) is the farther, so `m` is split.
    /// * On: the remainder phi ranks 1 and yields at every arrival; `m` keeps
    ///   the register from 3 to 15, and `p` is read from its home.
    #[test]
    fn j27_residency_on_a_remainder_phi_yields_to_the_main_loop_phi() {
        let (controls, main, post) = main_post_controls();
        let spec: [ValueSpec<'_>; 2] = [
            (Op::Phi, 3, 15, &[10, 14], 30, true),
            (Op::Phi, 5, 15, &[6, 12], 30, true),
        ];
        let inputs = [vec![main], vec![post]];
        let (graph, live, ids) = model_over(controls, &spec, &inputs);
        let (m, p) = (ids[0], ids[1]);
        let model = registers(1);
        let off = with_switch(false, || allocate_linear_scan(&graph, &live, &model))
            .expect("allocates off");
        assert!(
            !in_register_throughout(&off, m, 3, 15),
            "off is unchanged: the main phi is split at the remainder phi's arrival"
        );
        verify_allocation(&graph, &live, &model, &off).expect("verifies off");
        let on = with_switch(true, || allocate_linear_scan(&graph, &live, &model))
            .expect("allocates on");
        assert!(
            in_register_throughout(&on, m, 3, 15),
            "the main loop's phi must hold the register for its whole range"
        );
        for pos in [6, 12] {
            assert!(
                !matches!(on.location_at(p, pos), Some(ValueLoc::Reg(_))),
                "the remainder phi is the defined loser (read at {pos} from its home)"
            );
        }
        verify_allocation(&graph, &live, &model, &on).expect("verifies on");
    }
}
