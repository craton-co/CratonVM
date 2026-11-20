// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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

/// `CRATONVM_JIT_IR_LOOP_PHI_RESIDENCY` (perf, **default ON** since 2026-11-09, `=0` kills it; j27 wave 2, lane
/// RESIDENCY). Off is the pre-j27 spill score byte for byte. Opt-in because the
/// allocator's earlier "admit more under pressure" levers measured slower on
/// `probes/RegPressure.java` (`ir_residency_crossblock_enabled`) and this one
/// could not be measured where it was written. Read once per allocation
/// (`allocate_linear_scan`) and once per reservation; no `static`.
pub(crate) fn loop_phi_residency_enabled() -> bool {
    true}

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


