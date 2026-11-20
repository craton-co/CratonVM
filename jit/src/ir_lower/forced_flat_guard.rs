// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 4, lane DEOPTROWS (CH16-5 of the r18 backlog,
//! `docs/feature-designs/jit-ir-optimizations.md`): forced
//! exits at FLAT guards.
//!
//! `CRATONVM_DEOPT_EAGER_CHAINS` sends the passing arrivals of a CHAIN point
//! (a guard inside a spliced callee, publishing the callee's frame over its
//! parked callers) to the point's own pad on a schedule
//! (`Lowerer::emit_forced_chain_deopt`), so a soak runs the production resume
//! path of points that almost never fail. A flat point -- a guard of the
//! compiled method's own bytecode, resumed into one interpreter frame -- had
//! no such soak, although a wrong flat frame state is the same silent
//! miscompile (wrong locals after the trap). `CRATONVM_DEOPT_EAGER_GUARDS`
//! extends the soak to the flat points of the guards that build through
//! `Op::Guard` and `emit_deopt_unless`, with the same block, cell and tagged
//! pad.
//!
//! Inert without `CRATONVM_DEOPT_EAGER_CHAINS` (it only widens that soak's
//! set), so with both switches off nothing changes, and the existing
//! suppressions (`emit_unconditional_deopt`, `emit_seq_charat_trap`, which
//! clear `forced_chain_deopts` around a guard that never passes) hold for
//! flat points too. A guard whose condition is the constant 0 (an IR site
//! trap) never passes either and gets no block.
//!
//! What differs from a chain point is the VM side: a flat trap's sinks do not
//! all claim the forced mark, so a forced flat trip may be CHARGED like a real
//! failure (recorded, and after enough of them the body recompiled without
//! the speculation or left interpreted). That costs soak coverage, never
//! correctness: the resume is the natural one. Run the soak with
//! `CRATONVM_DEOPT_EAGER_CHAINS_FIRST=0` (or a seed) when the charge empties
//! the compiled set too early.

use super::Lowerer;
use crate::deopt::DeoptReason;
use crate::ir::{NodeId, Op};

/// `CRATONVM_DEOPT_EAGER_GUARDS` (j27 wave 4, lane DEOPTROWS; diagnostic,
/// opt-in, inert without `CRATONVM_DEOPT_EAGER_CHAINS`). No `static`: read
/// per flat guard of a lowering that already has the eager-chains switch on.
pub(super) fn forced_flat_guards_enabled() -> bool {
    false}

/// Does a guard get the forced-exit block? A chain point whenever the eager
/// chains are on (unchanged); a flat point only when they are on, the flat
/// switch (`flat_on`, asked last) is on too, and the guard can pass at all.
pub(super) fn guard_forces_exit(
    chains_on: bool,
    is_chain_point: bool,
    never_passes: bool,
    reason: DeoptReason,
    flat_on: impl FnOnce() -> bool,
) -> bool {
    // Class-loading guards publish the state for their failing path. The eager
    // soak runs after the guard passes, where resuming that state can skip
    // non-null work (for example String.valueOf's Object.toString call).
    // Similarly, a guard that never passes, is a site trap, or was hoisted
    // from an arm-entry trap publishes frame state only for its failing arrival;
    // a passing arrival must never be forced into that frame.
    chains_on
        && reason != DeoptReason::ClassLoading
        && !never_passes
        && (is_chain_point || flat_on())
}

impl Lowerer<'_> {
    /// The forced-exit block after a guard's own `Jcc` to `point_ptr`'s pad,
    /// when [`guard_forces_exit`] says so. Every guard site that used to call
    /// `emit_forced_chain_deopt` for its chain points calls this instead.
    pub(super) fn emit_forced_guard_deopt(
        &mut self,
        point_ptr: u64,
        is_chain_point: bool,
        never_passes: bool,
        reason: DeoptReason,
    ) {
        if guard_forces_exit(
            self.forced_chain_deopts,
            is_chain_point,
            never_passes,
            reason,
            forced_flat_guards_enabled,
        ) {
            self.emit_forced_chain_deopt(point_ptr);
        }
    }

    /// Is `cond` (an unfused `Op::Guard`'s condition) the constant 0, so the
    /// guard fails on every arrival?
    pub(super) fn guard_cond_never_passes(&self, cond: NodeId) -> bool {
        self.graph
            .nodes
            .get(cond as usize)
            .is_some_and(|c| matches!(c.op, Op::Const(0)))
    }

    /// Does this guard never pass, or is it a site trap / hoisted arm-entry trap
    /// whose snapshot belongs only to the failing path?
    pub(super) fn guard_is_failure_only(&self, cond: NodeId, bci: usize, raw_bci: usize) -> bool {
        self.guard_cond_never_passes(cond)
            || self.graph.site_trap_pcs.contains(&bci)
            || self.graph.site_trap_pcs.contains(&raw_bci)
            || self.graph.hoisted_arm_trap_pcs.contains(&bci)
            || self.graph.hoisted_arm_trap_pcs.contains(&raw_bci)
    }
}

// x86-64 only, like the `ir_lower` tests whose lowerer fixture it borrows.
#[cfg(all(test, target_arch = "x86_64"))]
mod tests;
