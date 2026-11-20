// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 5 (lane OSRRETIRE): a loop header whose operand stack is live is
//! retired at the OSR offer, before anything is compiled for it.
//!
//! Neither OSR entry seeds an operand stack (`osr_trampoline` and the
//! optimizing tier's stubs seed locals only), so every door refuses such a
//! header: `jit_bridge::try_osr` at `!frame.stack.is_empty()`,
//! `validate_osr_entry` with `OSR_REFUSE_OPERAND_STACK`, the single-pass
//! emitter's `-1` at the pc and `emit_osr_entry_stubs`. Under the default
//! `CRATONVM_BG_COMPILE`, `try_osr_offer` used to queue a background OSR task
//! for it first, and learn the answer from the published (unenterable) body
//! one to three compiles later
//! (`docs/feature-designs/jit-osr-exit-and-recompile.md`).
//!
//! The stack the offer sees IS the stack at the header: every back-edge site
//! offers after the branch landed -- a conditional branch popped its operands
//! before `cond_branch_arm!` (the fused compare forms never pushed them), and
//! `goto` / the decoded slow-path handlers have none left. Verified bytecode
//! fixes the depth per pc, so the verdict holds for every arrival, in both the
//! background and the inline (`CRATONVM_BG_COMPILE=0`) arm, which therefore
//! both retire here: the inline arm used to spend its five charged offers on
//! `try_osr`'s refusal.

use crate::threading::jvm_thread::JvmThread;
use crate::vm::SharedVm;

/// `CRATONVM_JIT_OSR_LIVE_STACK_RETIRE` (default ON, `=0` restores the old
/// offers): read only for a header whose stack is live, so the ordinary offer
/// pays nothing for it.
const LIVE_STACK_RETIRE_KEY: &str = "CRATONVM_JIT_OSR_LIVE_STACK_RETIRE";

/// The decision: a header with `stack_depth` live operand slots is retired
/// when the switch (asked only then) is on.
#[inline]
pub(super) fn osr_live_stack_header_retires(
    stack_depth: usize,
    switch_on: impl FnOnce() -> bool,
) -> bool {
    stack_depth != 0 && switch_on()
}

/// Retire the frame's loop header at `entry_pc` for the activation when its
/// operand stack is live, through the same bookkeeping as the
/// `published_but_unenterable` arm (`charge_osr_rejection` with
/// `OSR_MAX_ATTEMPTS`, which also charges the activation to the method-wide
/// budget). `true`: retired, the offer is declined.
pub(super) fn retire_live_stack_osr_header(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    entry_pc: usize,
) -> bool {
    let depth = thread.frames[frame_idx].stack.len();
    if !osr_live_stack_header_retires(depth, || {
        cratonvm_types::flags::runtime_flag_default_on(LIVE_STACK_RETIRE_KEY)
    }) {
        return false;
    }
    // The refusal `try_osr` would have recorded had it been asked: an entry
    // refused for its live state.
    cratonvm_jit::metrics::record_osr_event("osr_refused_entry");
    if crate::runtime::env_cache::dbg_jitc() {
        let f = &thread.frames[frame_idx];
        eprintln!(
            "[cratonvm-jitc] osr live-stack retire {}.{}{} pc={entry_pc} depth={depth}",
            f.class_name(),
            f.method_name(),
            f.method_descriptor(),
        );
    }
    super::charge_osr_rejection(
        shared,
        thread,
        frame_idx,
        entry_pc,
        crate::runtime::frame::OSR_MAX_ATTEMPTS,
    );
    true
}

#[cfg(test)]
mod tests;
