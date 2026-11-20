// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 8 (lane W8LOOP; the "wider unroll for a register-only body" lever
//! of `docs/feature-designs/jit-ir-optimizations.md`):
//! the main/post unroll's factor for a body none of whose nodes can deopt,
//! `CRATONVM_JIT_IR_UNROLL_WIDE_PURE=1` (opt-in).
//!
//! HotSpot's C2 unrolls a small register-only counted loop 8-16x
//! (`LoopUnrollLimit`), so `counterInt` runs ~1.4 instructions per iteration;
//! CratonVM's main loop is 4 copies (`partial_unroll_factor`), so the latch
//! (test, poll, jump) is paid once per 4 trips. A body that cannot deopt needs
//! no per-copy frames (`main_post_unroll_loop`'s `frames == None` route), so
//! widening it adds only code: the copies, and on a promoted accumulation the
//! reassociation (`linear_reassoc`) folds them back to one add per cell.
//!
//! The rule ([`main_post_factor`]): with the switch on and no explicit
//! `CRATONVM_JIT_IR_PARTIAL_UNROLL_FACTOR`, the largest factor up to
//! [`WIDE_PURE_MAX_FACTOR`] whose total stays inside the unroller's body
//! budget, never below the base factor. Everything else -- the stay test,
//! the licence `n - d < n`, the post loop -- is `main_post_unroll_loop`'s,
//! which admits factors through 16 (`d = (factor - 1) * s` fits an
//! `int` for `s <= 64`).
//!
//! Opt-in because it was written without a build: code growth against the
//! acceptance gate is exactly what the A/B must price.

/// The widest factor this lever asks for: `main_post_unroll_loop`'s own cap.
pub(super) const WIDE_PURE_MAX_FACTOR: usize = 16;

/// `CRATONVM_JIT_IR_UNROLL_WIDE_PURE=1` (perf, default ON; j27 wave 8, lane
/// W8LOOP). Read per loop offered; no `static`.
fn unroll_wide_pure_enabled() -> bool {
    true
}

/// Did the run name the partial unroll factor itself? Then it decides.
fn factor_pinned_by_user() -> bool {
    Err::<String, std::env::VarError>(std::env::VarError::NotPresent).is_ok()
}

/// The pure rule: the factor a `body_len`-node body gets under `budget`.
/// `base` when the body can deopt, when the switch is off, or when nothing
/// wider fits.
pub(super) fn wide_factor(base: usize, body_len: usize, pure: bool, budget: usize) -> usize {
    if !pure || body_len == 0 {
        return base;
    }
    let fits = budget / body_len;
    fits.min(WIDE_PURE_MAX_FACTOR).max(base)
}

/// The factor `main_post_unroll_loop` is offered for this loop. `pure`: no
/// node of the body can deopt (the `frames == None` route).
pub(super) fn main_post_factor(base: usize, body_len: usize, pure: bool, budget: usize) -> usize {
    if !unroll_wide_pure_enabled() || factor_pinned_by_user() {
        return base;
    }
    wide_factor(base, body_len, pure, budget)
}

// ── Wave 7b (lane ir-opt-inline): iota reductions yield to the vector loop ──
//
// `long s = 0; for (int i = 0; i < n; i++) s += (long) (i + 1000);` (the
// `OpCostRows.box` row once its box folds) is a pure body, so this lever gave
// it 16 scalar copies -- and the vector pre-loop (`ir_vector_loop`, whose
// iota form serves exactly `s += I2L(iv + c)`) then found only the post loop:
// the main loop steps by 16. The unroller already leaves a loop the vector
// pre-loop admits to it (`unroll_yields_to_vector_enabled`), but it plans
// that only in a graph with an element read (`Op::ArrayLoad`), and an iota
// reduction has none. Whether one is present then depended on the mode: the
// real-JDK `Long.valueOf` splice reads `LongCache.cache[..]`, the
// `--compatible` intrinsic reads nothing, so only `--real-jdk` vectorized.
// [`unroll_in_yielding_iota`] plans the vector loops for such a graph too and
// offers the unroller every other header.

/// `CRATONVM_JIT_IR_UNROLL_YIELDS_IOTA_TO_VECTOR` (wave 7b; perf, **default
/// ON**, `=0` kills). Read per unroll call; no `static`.
pub(super) fn unroll_yields_iota_to_vector_enabled() -> bool {
    true
}

/// Could `graph` hold a loop only the iota form of the vector pre-loop
/// serves, one the unroller's own yield does not plan? No element read (that
/// graph is planned by the unroller itself), and a widening `I2L` (the
/// `s += (long) (iv + c)` reduction's lane conversion).
pub(super) fn iota_reduction_candidate(graph: &crate::ir::Graph) -> bool {
    use crate::ir::{IrType, Op};
    !graph.nodes.iter().any(|n| matches!(n.op, Op::ArrayLoad(_)))
        && graph
            .nodes
            .iter()
            .any(|n| n.op == Op::I2L && n.ty == IrType::Long)
}

/// The loop headers the unroller may take when the vector pre-loop serves
/// `vector` and the caller restricted the pass to `only`: every `Region` /
/// `Merge` of `graph` in `only` (all of them for `None`) and not in
/// `vector`.
pub(super) fn headers_left_to_unroll(
    graph: &crate::ir::Graph,
    only: Option<&rustc_hash::FxHashSet<crate::ir::NodeId>>,
    vector: &[crate::ir::NodeId],
) -> rustc_hash::FxHashSet<crate::ir::NodeId> {
    use crate::ir::{NodeId, Op};
    graph
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| matches!(n.op, Op::Region | Op::Merge))
        // Cast: an arena index is a `NodeId`.
        .map(|(id, _)| id as NodeId)
        .filter(|id| only.is_none_or(|s| s.contains(id)) && !vector.contains(id))
        .collect()
}

/// `super::unroll_in(graph, only, late)`, leaving the loops the vector
/// pre-loop's iota form serves to it (see the section notes above). A graph
/// that is no [`iota_reduction_candidate`], or whose vector plan is empty,
/// is offered exactly as before.
pub(super) fn unroll_in_yielding_iota(
    graph: &mut crate::ir::Graph,
    only: Option<&rustc_hash::FxHashSet<crate::ir::NodeId>>,
    late: bool,
) -> bool {
    if !unroll_yields_iota_to_vector_enabled() || !iota_reduction_candidate(graph) {
        return super::unroll_in(graph, only, late);
    }
    // The unroller plans on a graph with its single-input merges collapsed;
    // plan the vector loops on the same graph.
    super::collapse_trivial_merges(graph);
    let vector = crate::ir_vector_loop::vector_loop_headers(graph);
    if vector.is_empty() {
        return super::unroll_in(graph, only, late);
    }
    let allowed = headers_left_to_unroll(graph, only, &vector);
    super::unroll_in(graph, Some(&allowed), late)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
#[path = "w7_unroll_iota_tests.rs"]
mod w7_iota_tests;
