// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Rebuilding an interpreter frame from a compiled one.
//!
//! A deoptimisation hands back a `FrameState` — locals and operand stack as
//! `FrameValue`s — and this module turns that into a real `Frame` the
//! interpreter can resume in. `build_deopt_frame_inner` does the
//! construction; `verify_reconstructed_frame` and
//! `verify_reconstructed_oops` check it before anything runs on it,
//! because the failure mode of a wrong reconstruction is a reference-typed
//! slot holding an integer.
//!
//! `transfer_osr_exit_into_live_frame_checked` is the OSR-exit half: the
//! same reconstruction, but written into a frame that already exists and is
//! currently executing.
//!
//! `despeculate_stashed_frame_method` is the part that makes deopt
//! *progress* rather than loop — resuming without withdrawing the
//! assumption that failed just re-enters the same compiled code and traps
//! again.

use super::*;
/// Materialise an inlined caller chain for a sink that can take one — N2.
///
/// # Why this exists, and why it is separate from `build_deopt_frame_inner`
///
/// `jit/src/x64/deopt_stubs.rs` sets `frame_state.caller = self
/// .inline_caller_chain()`, and until this landed EVERY VM-side rebuild sink
/// refused a non-empty chain outright. So the one producer of owned caller
/// chains fed only consumers that could not read them: a trap inside a splice
/// took the `None` path into the side-effect check and, for any method with
/// stores, raised `InternalError: precise deoptimization unavailable … refusing
/// side-effecting replay`.
///
/// That is not live today and this lane's predecessor made that a guarantee
/// rather than an accident: `jit/src/x64/inlining.rs`'s splice postcondition
/// rolls back any splice that published a point, so no artifact carries a
/// chain. The refusal becomes acute the moment that postcondition is relaxed,
/// which is why this half lands FIRST — see N5 in `NOTES-deopt.md` and the
/// `NOTES-deopt2.md` entry for the `jit/src/x64.rs` half that must precede it.
///
/// It is a separate function rather than a branch inside
/// `build_deopt_frame_inner` because that function returns a single `Frame` by
/// value, and a chain is N frames that must be pushed outermost-first. A sink
/// therefore has to *opt in*, and the sinks that have not are counted as
/// [`DeoptFrameBail::InlinedChain`] rather than silently reading as "no chains
/// occur".
///
/// Returns the chain outermost-first, ready for [`push_inlined_chain`], or
/// `None` after counting the refusal. **Nothing is pushed and nothing is
/// pinned** — every check that can refuse has already run, so a caller holding
/// a `Some` still has the whole-method re-run available and a caller holding a
/// `None` has lost nothing.
///
/// Test-only since round 13 wave 8: every production sink holds its stashed
/// point and calls [`build_deopt_frame_chain_for_point`].
#[cfg(test)]
pub(crate) fn build_deopt_frame_chain(
    shared: &SharedVm,
    cached: &Arc<CachedBytecodeMethod>,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> Option<DeoptFrameChain> {
    build_deopt_frame_chain_for_point(shared, cached, rframe, 0)
}

/// Push a materialised chain and run it to completion, for a sink that needs
/// the method's RETURN VALUE rather than a pushed frame — N2.
///
/// `try_resume_trapped_callee` (`vm/src/jit/helpers.rs`) sits between a
/// compiled caller and a compiled callee that trapped, and its whole contract
/// is to hand the caller the value the callee would have returned. For a single
/// frame that is `execute_prebuilt_frame`. For an inlined chain it is the same
/// thing one level up: push every frame outermost-first and run until the stack
/// comes back to the depth it started at, which is exactly when the OUTERMOST
/// frame — the callee the compiled caller actually invoked — returns.
///
/// **The depth is taken BEFORE the first push, and that is the whole point.**
/// `execute_prebuilt_frame` takes it inside itself, i.e. after the outer frames
/// would already be on the stack, and would therefore return the moment the
/// INNERMOST frame returned — leaving the rest of the chain stranded mid-method
/// and handing the compiled caller the wrong method's return value.
///
/// Test-only since gcd d1/b moved both production sinks to
/// [`run_deopt_frame_chain_to_completion_releasing_pins`], which releases the
/// compiled frame's pins before the chain runs (`no_test_only_public_api`).
#[cfg(test)]
pub(crate) fn run_deopt_frame_chain_to_completion(
    shared: &SharedVm,
    thread: &mut JvmThread,
    mut chain: DeoptFrameChain,
) -> Option<crate::error::MethodCallResult> {
    let trace = dbg_deopt_enabled();
    // Round 13 wave 8 (lane chain3): the chain's scalar-replaced objects are
    // re-allocated here, before anything is pushed, so a full heap gets the
    // single frame's answer -- `OutOfMemoryError` at the caller, as HotSpot
    // pops the frames (`resume_real_ir_deopt_or_throw` does the same) --
    // instead of the callers' `InternalError` for a chain that could not be
    // pushed. Such a chain holds no monitor (`chain_virtuals_refusal`), so no
    // lock of the abandoned activation needs releasing. A no-op for a chain
    // that names none; `push_inlined_chain` finds the work done.
    if let Err(why) = materialise_chain_virtuals(shared, thread, &mut chain) {
        if frame_build_heap_failure_is_answered(why) {
            return Some(Err(rematerialisation_out_of_memory(shared, thread)));
        }
        return None;
    }
    let depth_before = thread.frames.len();
    // `push_inlined_chain` is the one push path, shared with the frame-pushing
    // sink so the two cannot drift in their GC-rooting or pool handling. It
    // answers `None` only if an operand-stack push fails, which materialisation
    // already bounded against each frame's own `max_stack`; on that path some
    // frames are already pushed, so the caller must NOT treat the `None` as
    // "nothing happened" — see its own comment there. (Since round 13 wave 5
    // it also answers `None`, with nothing pushed, when the chain's
    // scalar-replaced objects could not be re-allocated; the callers' answer,
    // an error at the call and no replay, is sound for both.)
    push_inlined_chain(shared, thread, chain, trace)?;
    if !chain_runs_from_its_outermost_frame(shared) {
        return Some(crate::runtime::interpreter::run_pushed_frame_to_completion(
            shared,
            thread,
            depth_before,
        ));
    }
    Some(run_pushed_chain_to_completion(shared, thread, depth_before))
}

/// The pre-round-12-wave-5 shape of [`resume_real_ir_deopt_or_throw`], which
/// the unit tests below were written against (none of them exhausts the heap).
#[cfg(test)]
pub(super) fn resume_real_ir_deopt(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cached: &Arc<CachedBytecodeMethod>,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> Option<CachedCallResult> {
    resume_real_ir_deopt_or_throw(shared, thread, cached, rframe, &[], &[])
        .ok()
        .flatten()
}

/// [`trap_reason_at_frame`], with `UncommonTrap` for a guard that cannot be
/// named. `UncommonTrap` is CHARGED, so an unattributable trap still shows in
/// the trap rate and backs the method off, but it escalates on the ordinary
/// count-based schedule: an attribution nobody could make cannot blacklist a
/// method by itself (`UnreachedCode`, the old fallback, is `MakeNotCompilable`
/// on the first occurrence). Picking the first point in list order instead is
/// how a `PendingException` point used to be reported for a guard trap —
/// never charged, the artifact evicted and recompiled with the same wrong
/// speculation, forever.
///
/// Test-only since round 11 wave 5: every production sink asks
/// [`trap_reason_for_frame_with_cause`], which answers exactly this when the
/// stash carries no cause.
#[cfg(test)]
pub(crate) fn trap_reason_for_frame(
    compiled: &crate::jit::CompiledMethod,
    code: &[u8],
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> cratonvm_jit::deopt::DeoptReason {
    trap_reason_at_frame(compiled, code, rframe)
        .unwrap_or(cratonvm_jit::deopt::DeoptReason::UncommonTrap)
}

/// The `Option` shape of [`real_frame_deopt_resume_or_throw_and_despeculate`]
/// the unit tests below were written against (none of them exhausts the heap).
#[cfg(test)]
pub(super) fn real_frame_deopt_resume_and_despeculate(
    shared: &SharedVm,
    thread: &mut JvmThread,
    compiled: &crate::jit::CompiledMethod,
    cached: &Arc<CachedBytecodeMethod>,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    cause: Option<cratonvm_jit::deopt::DeoptCause>,
    point_addr: usize,
) -> Option<CachedCallResult> {
    real_frame_deopt_resume_or_throw_and_despeculate(
        shared, thread, compiled, cached, rframe, cause, point_addr,
    )
    .ok()
    .flatten()
}



/// Round 12 wave 6 (lane jni): a trap out of a body compiled after its class's
/// last redefinition resumes; one compiled before it does not.
#[cfg(test)]
mod r12w6_jni_current_body_tests;

/// Round 12 wave 4 (lane replay2): the door re-run verdict reads a stashed
/// frame's bci only when the frame is this body's own.
#[cfg(test)]
mod r12w4_replay2_door_tests;

/// Round 12 wave 5 (lane replay3): the full-heap re-materialisation answer and
/// the callee services' census rows.
#[cfg(test)]
mod r12w5_replay3_tests;

/// Round 12 wave 7 (lane replay4): the door census reads the single-pass
/// finalizer's frameless-trap stamp.
#[cfg(test)]
mod r12w7_replay4_frameless_verdict_tests;

/// Round 13 wave 3 (lane replay2; R13-2): the one "own stash, no resume,
/// replay unsound" predicate, with the doors' truth table
/// (`jit_bridge`'s `only_an_unsound_own_stash_of_a_class_never_redefined_is_refused`).
/// The kill switch row is the environment's, so it is not pinned here.
#[cfg(test)]
mod r13w3_replay2_rerun_answer_tests;

/// Round 13 wave 8 (lane replay4): every re-run the replay rule calls unsound
/// is refused, a refusal leaves the unsound column, and a bare sentinel is
/// judged by the trap site that raised it.
#[cfg(test)]
mod r13w8_replay4_rerun_refusal_tests;

/// Round 13 wave 5 (lane resume): an inlined chain whose scopes name
/// scalar-replaced objects (`r13w3-framestate-vm-chain-resume-gaps` item 1).
/// The producer numbers virtual objects chain-wide, so one object two scopes
/// name must become ONE heap object.
#[cfg(test)]
mod r13w5_resume_chain_virtual_tests;



/// Round 13 wave 8 (lane chain3): the planless OSR guard exit's chain arm,
/// the chain run-to-completion sink's refusals, and the outermost-scope
/// redefinition rule.
#[cfg(test)]
mod r13w8_chain3_tests;

/// Round 13 wave 12 (lane replay6): the VM sinks judge a re-run with the
/// exception edges the body that ran can take
/// (`r13w12-osrdoor-vm-sinks-replay-extent-ignores-handler-edges`).
#[cfg(test)]
mod r13w12_replay6_sink_handler_edge_tests;

#[cfg(test)]
mod r14w2_deopt_callsite_own_source_tests;

#[cfg(test)]
mod r14w3_resume_tests;

#[cfg(test)]
mod r15w3_chain3_chain_census_tests;

#[cfg(test)]
mod r15w4_chain4_forced_census_tests;



#[cfg(test)]
mod r16_chain_test_support;

#[cfg(test)]
mod r15w5_chain6_census_verdict_tests;

/// gcf f3/f: the owned-arguments hand-off census.
#[cfg(test)]
mod gcf_f3f_handoff_census_tests;



/// nio21 wave 20, lane L4: the cause-less twin of the guard-box rule.
#[cfg(test)]
mod nio21w20_fired_point_reason_tests;

/// JIT round 21 wave 16 (lane deopt-resume): a chain scope with no class-id
/// row is resolved by the enclosing class's id when it names that class, and
/// through the defining loader of the enclosing pool's constant-pool class
/// when the enclosing loader cannot name it
/// (`CRATONVM_DEOPT_RESUME_SCOPE_BY_LOADER`,
/// `r21-review-w15-jit-findings-FIXED-20261020` item 5). Hand-built classes: no
/// real JDK bytecode is needed.


/// JIT round 23 wave 4 (lane L1): a CHAIN trap is attributed to a spliced
/// loop's entry predicate only by that predicate's own chain id at the
/// innermost scope's header, under `UncommonTrap`.
#[cfg(test)]
mod r23_l1_chain_predicate_charge_tests;

mod ir_deopt_resume_enabled;
#[allow(unused_imports)]
pub use ir_deopt_resume_enabled::*;
#[allow(unused_imports)]
use ir_deopt_resume_enabled::*;
mod static_handed_hold_is_the_mirror;
#[allow(unused_imports)]
pub use static_handed_hold_is_the_mirror::*;
#[allow(unused_imports)]
use static_handed_hold_is_the_mirror::*;
mod charge_osr_chain_predicate_exit;
#[allow(unused_imports)]
pub use charge_osr_chain_predicate_exit::*;
#[allow(unused_imports)]
use charge_osr_chain_predicate_exit::*;
