// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Round 10 wave 7, lane `splice`: a deopt point published from inside an
//! inlined splice describes the CALLEE's frame, not the caller's.
//!
//! `docs/internal/fixed-bugs/r10-deoptverify-splice-published-point-would-carry-the-callers-locals-FIXED-20260922.md`
//! found that `x64/deopt_stubs.rs::build_frame_state_at` filled `self.num_locals`
//! — the ROOT method's local count, read out of the ROOT method's local homes and
//! described by the ROOT method's liveness/oop/kind analyses — under whatever
//! identity it stamped. M1 had taught it to stamp the spliced CALLEE's key and to
//! publish the bci in the callee's own space; the CONTENTS stayed the root's. So
//! `inlining.rs`'s claim that "the FIRST edit to publish one produces well-formed
//! metadata" was false on that axis, and the shape it would produce is one the
//! VM's identity gate accepts.
//!
//! Three kinds of claim are pinned here, and they are deliberately different:
//!
//! 1. **Behavioural, through the public `DeoptVerifier` API.** The pre-fix shape
//!    (a callee-identified frame carrying the root's local count) really is
//!    rejected as `LocalCountMismatch` once `driver.rs`'s per-callee
//!    `MethodFrameLimits` are registered, and the post-fix shape (the callee's
//!    own count, every slot `Unsupported`) really does verify clean. This is the
//!    half that says the new geometry is the one the install-time check wants.
//!
//! 2. **Behavioural, about resumability.** An all-`Unsupported` callee frame is
//!    not merely well formed, it is FAIL-CLOSED: `deopt::frame_state_is_resumable`
//!    answers `false`, which is what makes every VM sink refuse the resume and
//!    take the safe re-run instead of reconstructing the callee out of the
//!    caller's locals. Without this, "well formed" would be the whole claim, and
//!    well formed is not the same as safe.
//!
//! 3. **Textual, about the producer.** The defect was a value read from the wrong
//!    field, and no unit test can reach `build_frame_state_at` from here (it is a
//!    private method on a `Compiler` this crate does not export). The scan below
//!    asserts that the splice branch and its geometry source are present in the
//!    file. It proves text, never behaviour, and says so — a reviewer confirms the
//!    branch is REACHED by reading, and `CRATONVM_DBG_EXCFRAME` prints a
//!    `SPLICE FRAME` line the first time it ever is.
//!
//! No test here compiles a Java method: this lane was not permitted to build,
//! test or probe, and every assertion below is about types this crate owns or
//! about its own source text.

use std::path::Path;

use cratonvm_jit::deopt::{
    frame_state_is_resumable, DeoptAction, DeoptReason, DeoptVerifier, DeoptimizationPoint,
    FrameState, FrameValue, MethodFrameLimits, ResumeSemantics,
};

/// The compiling (root) method, as `driver.rs` registers it: `compiler.method_key`
/// with `orig_code_len` and this compile's `max_locals`.
const ROOT: &str = "Caller.run:()V";
const ROOT_CODE_LEN: u32 = 64;
/// Deliberately LARGER than the callee's. That is the ordinary case — the callee
/// was inlined into this method — and it is the direction in which the pre-fix
/// shape is detectable at all: a 9-entry locals vector under a key whose
/// `max_locals` is 2. The other direction (a callee with MORE locals than its
/// caller) publishes a vector that is too SHORT, silently loses slots, and is
/// within the verifier's upper bound; that half is unobservable here and is
/// recorded in the known-issues page rather than asserted.
const ROOT_MAX_LOCALS: u16 = 9;

/// The spliced callee, as `driver.rs` registers it from the `InlineSite`:
/// `"{class}.{method}:{descriptor}"` with `callee_code_len` / `callee_max_locals`,
/// and `u16::MAX` for `max_stack` because no callee `max_stack` reaches the
/// driver.
const CALLEE: &str = "Callee.leaf:(I)I";
const CALLEE_CODE_LEN: u32 = 12;
const CALLEE_MAX_LOCALS: u16 = 2;

/// The two scopes `driver.rs` registers for a compile that splices once.
fn verifier() -> DeoptVerifier {
    DeoptVerifier::new()
        .with_method(MethodFrameLimits::new(
            ROOT,
            ROOT_CODE_LEN,
            ROOT_MAX_LOCALS,
            u16::MAX,
        ))
        .with_method(MethodFrameLimits::new(
            CALLEE,
            CALLEE_CODE_LEN,
            CALLEE_MAX_LOCALS,
            u16::MAX,
        ))
}

/// The CALLER scope `push_inline_scope` captures: the root method's frame at the
/// invoke, with the callee's arguments already popped off its operand stack.
///
/// Its locals are `Int` rather than a machine-location descriptor on purpose.
/// `check_value`'s frame-slot lane would also have an opinion about a
/// `StackSlot*` offset, and a fixture that tripped it would make the assertions
/// below pass or fail for a second reason.
fn caller_scope() -> FrameState {
    FrameState {
        method_key: ROOT.to_string(),
        bci: 21,
        locals: vec![FrameValue::Int(1); ROOT_MAX_LOCALS as usize],
        stack: Vec::new(),
        monitors: Vec::new(),
        caller: None,
    }
}

/// A point as `build_and_record_deopt_point` publishes it from inside a splice:
/// the callee's key, a bci in the callee's own space, and the caller chain
/// `inline_caller_chain` attaches.
///
/// `bci` and `frame_state.bci` are the same value because the producer computes
/// one `resume_bci_for` and writes it to both; `semantics` is
/// `ResumeSemantics::for_reason(reason)` for the same reason. Getting either
/// wrong would add `PointBciMismatch` / `ResumeSemanticsMismatch` to every
/// assertion below.
fn splice_point(locals: Vec<FrameValue>, stack: Vec<FrameValue>) -> DeoptimizationPoint {
    let callee_bci = 5u32;
    DeoptimizationPoint {
        native_offset: 0x40,
        bci: callee_bci,
        reason: DeoptReason::BoundsCheck,
        action: DeoptAction::Reinterpret,
        speculation_id: 0,
        semantics: ResumeSemantics::for_reason(DeoptReason::BoundsCheck),
        frame_state: FrameState {
            method_key: CALLEE.to_string(),
            bci: callee_bci,
            locals,
            stack,
            monitors: Vec::new(),
            caller: Some(Box::new(caller_scope())),
        },
    }
}

/// THE DEFECT. `build_frame_state_at` used to fill `self.num_locals` entries
/// whatever identity it stamped, so a splice-published point named the callee and
/// carried the ROOT's locals — here nine slots under a key whose `max_locals` is
/// two.
///
/// CONTRACT THE ASSERTION RESTS ON: the scope lane's local check is
/// `state.locals.len() > limits.max_locals`, per scope, at every depth of the
/// caller chain (`check_point` walks `state.caller` and calls `check_scope` for
/// each). So the CALLER scope's nine locals are fine — the root's bound is nine —
/// and only the innermost frame is out of range. One report, not two.
#[test]
fn the_root_local_count_under_the_callees_key_is_rejected() {
    let points = vec![splice_point(
        vec![FrameValue::Int(1); ROOT_MAX_LOCALS as usize],
        Vec::new(),
    )];

    let violations = verifier().violations(&points);

    assert_eq!(
        violations.len(),
        1,
        "expected exactly the local-count report; got {violations:?}"
    );
    assert!(
        matches!(
            &violations[0],
            cratonvm_jit::deopt::DeoptMetadataError::LocalCountMismatch {
                method_key,
                found,
                max_locals,
                scope_depth,
                ..
            } if method_key == CALLEE
                && *found == ROOT_MAX_LOCALS as usize
                && *max_locals == CALLEE_MAX_LOCALS
                && *scope_depth == 0
        ),
        "expected LocalCountMismatch on the innermost (callee) scope, got {:?}",
        violations[0]
    );
}

/// THE FIX. The same point built to the callee's own geometry verifies clean.
///
/// Every slot is `Unsupported` because there is no per-slot analysis of a callee:
/// `local_oop_masks`/`local_liveness`/`local_kinds` are analyses of the ROOT
/// method indexed by root pcs, and inside a splice the pc is in a different
/// bytecode space. `Unsupported` is the house encoding for "this cannot be
/// described" and no verifier lane objects to it (`check_value`'s catch-all arm),
/// which is exactly the property that makes an honest frame publishable.
#[test]
fn the_callee_geometry_with_undescribed_slots_verifies_clean() {
    let points = vec![splice_point(
        vec![FrameValue::Unsupported; CALLEE_MAX_LOCALS as usize],
        // The callee's own operand-stack window: entries the callee pushed above
        // `InlineCalleeScope::caller_stack_floor`. Two of them here.
        vec![FrameValue::Unsupported; 2],
    )];

    let violations = verifier().violations(&points);

    assert!(
        violations.is_empty(),
        "a frame built to the callee's own geometry is well-formed metadata; got \
         {violations:?}"
    );
}

/// ...and it is fail-closed, which is the half that matters more than being well
/// formed.
///
/// CONTRACT: `frame_state_is_resumable` is `false` as soon as any local or stack
/// slot is `Unsupported` (`value_blocks_resume`), and it scans caller scopes too.
/// The VM sink maps that to a refusal and the method takes the safe re-run, so a
/// resume can never reconstruct the callee's frame out of whatever these slots
/// happened to name. The control is the caller scope on its own, which IS
/// resumable — otherwise this test would pass for a frame that was unresumable
/// for some unrelated reason.
#[test]
fn an_undescribed_callee_frame_is_unresumable_and_its_caller_scope_is_not() {
    let fs = splice_point(
        vec![FrameValue::Unsupported; CALLEE_MAX_LOCALS as usize],
        Vec::new(),
    )
    .frame_state;
    assert!(
        !frame_state_is_resumable(&fs),
        "an Unsupported callee local must make the frame unresumable, or the resume \
         would invent values for slots the producer could not describe"
    );
    assert!(
        frame_state_is_resumable(&caller_scope()),
        "the caller scope is described precisely (the root's locals at the invoke, \
         from the root's own analyses) and must stay resumable"
    );
}

/// The converse malformed pair, which the postcondition's bci-range clause exists
/// for: the callee's key on an ENCLOSING-method bci.
///
/// Every publisher the splice walk actually contains
/// (`emit_post_invoke_exception_check`, `emit_post_alloc_oom_check`, the precise
/// NPE/AIOOBE array arms) keys on `Compiler::dbg_last_pc`, which only the OUTER
/// walk assigns — so the pc is the caller's invoke while `current_bytecode_owner`
/// stamps the callee. Those sites are interlocked off in any compile that inlines;
/// this pins what the install-time check would say if one ever fired, which is why
/// refusing the SPLICE (in `inlining.rs`) is better than letting the artifact
/// reach here and lose itself.
#[test]
fn a_callee_identified_point_at_an_enclosing_bci_is_out_of_range() {
    let mut p = splice_point(
        vec![FrameValue::Unsupported; CALLEE_MAX_LOCALS as usize],
        Vec::new(),
    );
    // Past the end of the callee, inside the root: what `dbg_last_pc` holds
    // during a splice.
    p.bci = 40;
    p.frame_state.bci = 40;

    let violations = verifier().violations(&[p]);

    assert!(
        violations.iter().any(|v| matches!(
            v,
            cratonvm_jit::deopt::DeoptMetadataError::BciOutOfRange {
                method_key,
                bci: 40,
                code_len,
                ..
            } if method_key == CALLEE && *code_len == CALLEE_CODE_LEN
        )),
        "an enclosing-method bci under the callee's key must be reported out of \
         range; got {violations:?}"
    );
}

// ---------------------------------------------------------------------------
// The producer
// ---------------------------------------------------------------------------

/// `build_frame_state_at` must build a splice-published frame from the spliced
/// callee's scope, not from the compile's own `num_locals`.
///
/// Textual, and it proves text only. What it can catch is the branch being
/// deleted or the geometry source being swapped back for `self.num_locals`; what
/// it cannot catch is the branch becoming unreachable. The known-issues page's own
/// confirmation grep was `rg -n "for i in 0..self.num_locals"`, and this is that
/// grep with the answer it now has to give.
#[test]
fn the_splice_branch_builds_from_the_callee_scope() {
    let src = read_jit_src(&["x64", "deopt_stubs.rs"]);

    // The one loop bounded by the ROOT's local count must still be the only one,
    // and it must now sit BELOW an early return for the splice case. Both facts
    // are checked by their own text rather than by their order, because line
    // order is the thing a refactor changes without changing behaviour.
    assert_eq!(
        src.matches("for i in 0..self.num_locals").count(),
        1,
        "the root method's locals loop should appear exactly once in \
         x64/deopt_stubs.rs; if it now appears twice, one of them is describing a \
         callee with the caller's geometry, which is \
         docs/internal/fixed-bugs/r10-deoptverify-splice-published-point-would-carry-the-callers-locals-FIXED-20260922.md \
         reopened"
    );
    assert!(
        src.contains("if let Some(scope) = self.current_callee_scope()"),
        "build_frame_state_at must branch on the innermost spliced callee's scope \
         before reading any per-slot analysis of the compiling method"
    );
    assert!(
        src.contains("locals: vec![FrameValue::Unsupported; scope.max_locals]"),
        "a splice-published frame must be sized to the CALLEE's max_locals with \
         undescribed slots; `Undefined` or a plausible-looking value here is the \
         silent wrong answer this page was opened about"
    );
}

/// The identity and the geometry must come from ONE stack entry.
///
/// Two parallel `Vec`s is how the identity gets pushed without its geometry — the
/// same failure `pop_inline_scope`'s doc already argues about the scope stack and
/// the identity stack ("they are pushed together and must be popped together").
/// `InlineCalleeScope` makes that unnecessary to argue, and this pins it.
#[test]
fn the_callee_identity_and_geometry_are_one_stack() {
    let x64 = read_jit_src(&["x64.rs"]);
    assert!(
        x64.contains("pub(super) inline_callee_scopes: Vec<InlineCalleeScope>"),
        "the splice identity stack must carry the frame geometry with it"
    );
    assert!(
        !x64.contains("inline_callee_identity"),
        "a second, identity-only stack beside inline_callee_scopes can be pushed \
         without its geometry, which is the defect this type removed"
    );
}

/// The interlock that keeps every one of this backend's conditional publishers
/// out of a splice must be stated at the splice, not only in the planner.
///
/// `precise_exception_frames` is what arms `emit_post_invoke_exception_check` and
/// its three siblings, and the splice walk reaches all four. `plan_inline` refuses
/// every site for it and `try_compile_inner` clears `inline_sites` — but that
/// second one does not clear `inline_guard_variants`, so on its own it would leave
/// the guarded-virtual splice path open. The refusal in `try_emit_inline_site` is
/// the statement at the place that can observe the hazard.
#[test]
fn the_splice_emitter_refuses_a_precise_exception_frames_compile() {
    let inlining = read_jit_src(&["x64", "inlining.rs"]);
    assert!(
        inlining
            .lines()
            .map(str::trim)
            .filter(|l| !l.starts_with("//"))
            .any(|l| l == "if self.precise_exception_frames {"),
        "x64/inlining.rs must refuse to splice in a compile that publishes precise \
         exception frames; the planner's InlineRefusal::PreciseExceptionFrames and \
         try_compile_inner's inline_sites.clear() are policy statements elsewhere, \
         and neither is where the hazard is observable"
    );
}

fn read_jit_src(parts: &[&str]) -> String {
    let mut path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for p in parts {
        path = path.join(p);
    }
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}
