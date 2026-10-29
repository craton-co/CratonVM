// SPDX-License-Identifier: Apache-2.0
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
//! (`docs/known-issues/jit/j27-osr-offer-skips-compile-for-a-live-stack-header-proposal-20261028.md`).
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
mod tests {
    use super::{osr_live_stack_header_retires, LIVE_STACK_RETIRE_KEY};
    use crate::config::VmConfig;
    use crate::runtime::frame::{Frame, OSR_MAX_ATTEMPTS};
    use crate::runtime::interpreter::{try_osr_with_backoff, OsrBackoffOutcome};
    use crate::threading::jvm_thread::{JvmThread, ThreadId};
    use crate::vm::SharedVm;
    use cratonvm_types::ClassId;
    use std::sync::atomic::Ordering;

    #[test]
    fn only_a_live_stack_retires_and_an_empty_one_never_reads_the_switch() {
        assert!(!osr_live_stack_header_retires(0, || panic!(
            "an empty stack must not read the switch"
        )));
        assert!(osr_live_stack_header_retires(1, || true));
        assert!(osr_live_stack_header_retires(3, || true));
        assert!(!osr_live_stack_header_retires(2, || false));
    }

    fn live_stack_frame(class_id: u32, method: &str) -> Frame {
        let mut f = Frame::new(
            ClassId::new(class_id),
            "cratonvm/test/J27OsrLiveStackHolder".to_string(),
            method.to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            1,
            1,
            &[],
        );
        f.stack.push_int(7).expect("max_stack 1");
        f
    }

    /// One due offer at a live-stack header retires it for the activation and
    /// charges the activation to the method-wide budget, without the
    /// background wait (whose `record_osr_background_pending` would zero the
    /// back-edge count) and in either compile mode. With the switch off the
    /// first offer does not retire.
    #[test]
    fn a_live_stack_header_is_retired_by_its_first_due_offer() {
        if !crate::runtime::env_cache::osr_backedge_enabled()
            || crate::runtime::env_cache::disable_jit()
        {
            return;
        }
        let shared = SharedVm::new(VmConfig::default());
        let mut thread = JvmThread::new(ThreadId(0), "osr-live-stack");
        thread.frames.push(live_stack_frame(0x27a1, "retired"));
        thread.frames.push(live_stack_frame(0x27a2, "offered"));
        let refused = || {
            shared
                .jit
                .bridge
                .osr_refused_loop_count
                .load(Ordering::Acquire)
        };

        thread.frames[0].backward_count = u32::MAX;
        let mut idx = 0;
        let out = cratonvm_types::flags::with_thread_overrides(
            &[(LIVE_STACK_RETIRE_KEY, None)],
            || try_osr_with_backoff(&shared, &mut thread, &mut idx, 0, 0),
        );
        assert!(matches!(out, OsrBackoffOutcome::Skip));
        assert_eq!(thread.frames[0].osr_attempts_at(0), Some(OSR_MAX_ATTEMPTS));
        assert_eq!(
            thread.frames[0].backward_count,
            u32::MAX,
            "no background wait was started"
        );
        assert_eq!(refused(), 1);

        thread.frames[1].backward_count = u32::MAX;
        let mut idx = 1;
        let out = cratonvm_types::flags::with_thread_overrides(
            &[(LIVE_STACK_RETIRE_KEY, Some("0"))],
            || try_osr_with_backoff(&shared, &mut thread, &mut idx, 1, 0),
        );
        assert!(matches!(out, OsrBackoffOutcome::Skip));
        assert_ne!(
            thread.frames[1].osr_attempts_at(0),
            Some(OSR_MAX_ATTEMPTS),
            "=0: the old offer, not a retirement"
        );
        assert_eq!(refused(), 1);
    }

    /// Source pin: the retirement sits after the method-wide budget (whose
    /// first ask records the header) and before the background arm, so both
    /// compile modes take it before any request or `try_osr`.
    #[test]
    fn the_offer_retires_a_live_stack_header_before_both_compile_arms() {
        let src = include_str!("../interpreter.rs").replace("\r\n", "\n");
        let body = src
            .split("\nfn try_osr_offer(")
            .nth(1)
            .and_then(|rest| rest.split("\n\u{7d}\n").next())
            .expect("try_osr_offer");
        let budget = body
            .find("jit_bridge::osr_loop_offer_allowed(")
            .expect("the method-wide budget");
        let retire = body
            .find("osr_live_stack::retire_live_stack_osr_header(")
            .expect("the live-stack retirement");
        let bg = body
            .find("if crate::runtime::env_cache::bg_compile() {")
            .expect("the background arm");
        let request = body.find("request_osr_classified").expect("the request");
        let inline = body.find("let osr_result = try_osr(").expect("the inline door");
        assert!(budget < retire && retire < bg && bg < request && retire < inline);
    }
}
