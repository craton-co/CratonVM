// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 4, lane DEOPTROWS (CH16-5 of the r18 backlog,
//! `docs/known-issues/jit/j27-r18-backlog-live-rows-20261028.md`): forced
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
use crate::ir::{NodeId, Op};

/// `CRATONVM_DEOPT_EAGER_GUARDS` (j27 wave 4, lane DEOPTROWS; diagnostic,
/// opt-in, inert without `CRATONVM_DEOPT_EAGER_CHAINS`). No `static`: read
/// per flat guard of a lowering that already has the eager-chains switch on.
pub(super) fn forced_flat_guards_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_DEOPT_EAGER_GUARDS")
}

/// Does a guard get the forced-exit block? A chain point whenever the eager
/// chains are on (unchanged); a flat point only when they are on, the flat
/// switch (`flat_on`, asked last) is on too, and the guard can pass at all.
pub(super) fn guard_forces_exit(
    chains_on: bool,
    is_chain_point: bool,
    never_passes: bool,
    flat_on: impl FnOnce() -> bool,
) -> bool {
    chains_on && (is_chain_point || (!never_passes && flat_on()))
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
    ) {
        if guard_forces_exit(
            self.forced_chain_deopts,
            is_chain_point,
            never_passes,
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
}

// x86-64 only, like the `ir_lower` tests whose lowerer fixture it borrows.
#[cfg(all(test, target_arch = "x86_64"))]
mod tests {
    use super::guard_forces_exit;
    use cratonvm_types::flags::with_thread_overrides;

    const GUARDS: &str = "CRATONVM_DEOPT_EAGER_GUARDS";

    #[test]
    fn a_flat_guard_is_forced_only_inside_the_eager_chains_soak() {
        let on = || true;
        let off = || false;
        // Chain points: exactly as before (the flat switch is not asked).
        assert!(guard_forces_exit(true, true, false, off));
        assert!(!guard_forces_exit(false, true, false, on));
        // Flat points: both switches, and a guard that can pass.
        assert!(guard_forces_exit(true, false, false, on));
        assert!(!guard_forces_exit(true, false, false, off));
        assert!(!guard_forces_exit(false, false, false, on));
        assert!(!guard_forces_exit(true, false, true, on));
    }

    /// Bytes emitted after a flat guard's `Jcc`, and the points the lowerer
    /// recorded for the census, under `guards` with the eager chains `chains`.
    fn emitted(chains: bool, guards: Option<&str>, never_passes: bool) -> (usize, Vec<u64>) {
        with_thread_overrides(&[(GUARDS, guards)], || {
            let mut lo = super::super::tests::lowerer_with_resident_xmm(4096, None);
            lo.forced_chain_deopts = chains;
            let at = lo.buf.pos();
            lo.emit_forced_guard_deopt(0x1238, false, never_passes);
            (lo.buf.pos() - at, lo.forced_chain_deopt_points.clone())
        })
    }

    #[test]
    fn a_flat_point_gets_the_chain_block_under_both_switches() {
        assert_eq!(emitted(true, Some("1"), false), (26, vec![0x1238]));
        assert_eq!(emitted(true, None, false), (0, vec![]));
        assert_eq!(emitted(true, Some("0"), false), (0, vec![]));
        assert_eq!(emitted(false, Some("1"), false), (0, vec![]));
        assert_eq!(emitted(true, Some("1"), true), (0, vec![]), "a site trap");
    }

    #[test]
    fn a_chain_point_keeps_its_block_without_the_flat_switch() {
        with_thread_overrides(&[(GUARDS, None)], || {
            let mut lo = super::super::tests::lowerer_with_resident_xmm(4096, None);
            lo.forced_chain_deopts = true;
            let at = lo.buf.pos();
            lo.emit_forced_guard_deopt(0x1238, true, false);
            assert_eq!(lo.buf.pos() - at, 26);
        });
    }
}
