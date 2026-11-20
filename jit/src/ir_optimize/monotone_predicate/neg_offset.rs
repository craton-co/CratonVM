// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 2 (lane TIER1PROP, `CRATONVM_JIT_IR_MONOTONE_NEG_OFFSET`, opt-in):
//! the planter half of BCE21-2's non-constant start.
//!
//! # What
//!
//! `for (int i = from; i < to; i++) d[i] = s[i - 1];` keeps the bounds check of
//! `s[i - 1]` on every iteration: BCE21-2 (`ir_check_elim`'s `a[i - c]` arm,
//! `CRATONVM_JIT_IR_CONST_OFFSET_PREDICATE`) proves it only for a unit-stride
//! `i` from a CONSTANT start `>= c`, and the monotone predicate planted guards
//! for `a[p]` only. HotSpot's range-check elimination handles any affine
//! `i - c` with a pre-loop predicate on the start. Under the switch the
//! monotone planter ([`super::monotone_plan`]) also takes `a[p - c]`, `c` in
//! `1..=i32::MAX`, hanging off a `p < x` edge, and plants, in the pre-header,
//! deopting to the loop entry with the loop-entry frame:
//!
//! * `x <= a.length`, as for `a[p]` (none when `x` is `a.length` itself);
//! * `c <= init` for the φ's entry value `init`, unless `init` is an `int`
//!   constant `>= c` (then nothing is owed). A constant start below `c` faults
//!   on the first iteration, so no guard is planted and the check stays.
//!
//! `ir_check_elim::monotone_phi_at_least` then proves `c <= p < a.length` at
//! the access (its section notes carry the soundness argument). A floor also
//! states `0 <= init`, so it replaces that φ's `0 <= init` guard.
//!
//! # Why it is sound
//!
//! Every guard only deopts to the loop entry with the entry frame, as
//! [`super::install_range_predicates`]' guards do: nothing has run between the
//! pre-header and the header, so the interpreter runs the loop itself and
//! raises whatever the bytecode raises where it raises it. A guard that keeps
//! failing is withdrawn by the de-spec registry at the header bci
//! (`PredicateKind::Monotone`, the module's existing key), after which the
//! method keeps its checks.

use crate::ir::{CmpOp, Graph, IrType, NodeId, Op, NO_NODE};

/// `(p, c)` when the access index `idx` is a monotone header φ `p` of `phis`
/// (`c == 0`), or, with `neg_on`, `p - c` / `p + (-c)` for such a `p` and `c`
/// in `1..=i32::MAX` (`ir_check_elim::negative_offset_index`).
pub(super) fn indexed_phi(
    graph: &Graph,
    idx: NodeId,
    phis: &[NodeId],
    neg_on: bool,
) -> Option<(NodeId, i64)> {
    if phis.contains(&idx) {
        return Some((idx, 0));
    }
    if !neg_on {
        return None;
    }
    crate::ir_check_elim::negative_offset_index(graph, idx).filter(|(p, _)| phis.contains(p))
}

/// What an `a[p - c]` access owes the entry value of its φ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Floor {
    /// The entry value is an `int` constant `>= c`: nothing to plant.
    Holds,
    /// Plant `Guard(c <= v)` for the entry value `v`.
    Guard(NodeId),
    /// No guard can help (a constant below `c`, a missing or non-`int` entry
    /// value, or one computed in the loop): the access keeps its check.
    Never,
}

/// The [`Floor`] of the header φ `p` (entry input at `entry_slot`) for an
/// offset `c >= 1`.
pub(super) fn floor_of(
    graph: &Graph,
    p: NodeId,
    entry_slot: usize,
    c: i64,
    in_body: &dyn Fn(NodeId) -> bool,
) -> Floor {
    let Some(init) = graph.node_opt(p).and_then(|n| n.input_opt(entry_slot)) else {
        return Floor::Never;
    };
    if init == NO_NODE || in_body(init) {
        return Floor::Never;
    }
    let Some(n) = graph.node_opt(init) else {
        return Floor::Never;
    };
    if n.ty != IrType::Int {
        return Floor::Never;
    }
    match n.op {
        // Read at `int` width, as `ir_check_elim::int_const` reads it.
        Op::Const(k) if i64::from(k as i32) >= c => Floor::Holds,
        Op::Const(_) => Floor::Never,
        _ => Floor::Guard(init),
    }
}

/// The floors one loop's `a[p - c]` accesses owe.
#[derive(Debug, Default)]
pub(super) struct FloorUses {
    /// `(v, c)`: `Guard(c <= v)`, the largest `c` asked of `v`.
    pub(super) floors: Vec<(NodeId, i64)>,
    /// The φs indexed with an offset (their room tests' `0 <= x` guards are
    /// owed as for a plain `a[p]`).
    pub(super) phis: Vec<NodeId>,
    /// Some floor guard serves an own-length access, so it is worth planting
    /// with no length guard.
    pub(super) alone: bool,
}

impl FloorUses {
    /// Record that an access `a[p - c]` was kept with `floor`; `own_length`
    /// when its bound is `a.length` itself (no length guard).
    pub(super) fn note(&mut self, p: NodeId, floor: Floor, c: i64, own_length: bool) {
        if !self.phis.contains(&p) {
            self.phis.push(p);
        }
        let Floor::Guard(v) = floor else {
            return;
        };
        self.alone |= own_length;
        match self.floors.iter_mut().find(|(x, _)| *x == v) {
            Some((_, have)) => *have = (*have).max(c),
            None => self.floors.push((v, c)),
        }
    }
}

/// Where a loop's floor guards go: the header bci they resume at, the
/// loop-entry control they hang off, and the loop-entry frame copy that names
/// them.
pub(super) struct FloorSite {
    pub(super) header_bci: usize,
    pub(super) entry_pred: NodeId,
    pub(super) snapshot: u32,
}

/// Plant `Guard{header_bci}(c <= v)` for each floor, at most `budget` of them,
/// each named by `site.snapshot`. Stops at the first guard that cannot be
/// named (its nodes are taken back out). Returns the number planted.
pub(super) fn install_floor_guards(
    graph: &mut Graph,
    floors: &[(NodeId, i64)],
    site: FloorSite,
    budget: usize,
) -> usize {
    let pc = Some(site.header_bci);
    let mut added = 0usize;
    for &(v, c) in floors.iter().take(budget) {
        let lo = graph.add(Op::Const(c), IrType::Int, vec![], None);
        let cond = graph.add(Op::Cmp(CmpOp::Le), IrType::Int, vec![lo, v], pc);
        let guard = graph.add(
            Op::Guard {
                bci: site.header_bci,
            },
            IrType::Void,
            vec![site.entry_pred, cond],
            pc,
        );
        if !graph.set_node_frame_snapshot(guard, site.snapshot) {
            graph.kill(guard);
            graph.kill(cond);
            graph.kill(lo);
            break;
        }
        added += 1;
    }
    added
}


