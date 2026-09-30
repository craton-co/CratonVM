// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Round 10, lane `deoptverify`: the single-pass x64 backend's deopt metadata is
//! verified at install time, and the shape that motivated it is rejected.
//!
//! `docs/internal/fixed-bugs/r10-earelock-singlepass-deopt-metadata-is-never-verified-FIXED-20260922.md`
//! found that `DeoptVerifier` was constructed in exactly one production place,
//! `jit/src/ir_lower.rs`, so every `DeoptimizationPoint` the SINGLE-PASS backend
//! published went out unchecked — including the one concrete defect wave 5 found
//! in that blind spot: Phase C could publish a `MonitorInfo::object` of
//! `FrameValue::VirtualObjectRef(new_pc)` naming a virtual object the locals lane
//! never defined, because `monitor_at` and `local_prov_at` are two independent
//! maps.
//!
//! Two things are pinned here, and they are different kinds of claim.
//!
//! 1. **A behavioural claim about `DeoptVerifier`**, exercised through the public
//!    API: the wave-5 shape really does produce
//!    `DeoptMetadataError::UndefinedVirtualObjectRef`, and the same frame with
//!    the definition present produces nothing. This is the half that says the
//!    check is worth running.
//!
//! 2. **A textual claim about the call site**, because the defect was an ABSENCE
//!    of a call and an absence cannot be exercised. The single-pass finalize path
//!    (`jit/src/x64/driver.rs`) must contain the verification call. A test that
//!    only asserted (1) would have passed just as happily on the day the backend
//!    had no verification at all — which is the state this page was opened about.
//!
//! Neither test compiles a Java method: this lane was not permitted to build or
//! run the VM, and a metadata assertion does not need one. What (2) cannot prove
//! is that the call is REACHED on every finalize path; a reviewer confirms that
//! by reading, and the bail-site census
//! (`note_jit_bail_site_at("deopt-metadata-violation", ..)`) reports it at
//! runtime.

use std::path::Path;

use cratonvm_jit::deopt::{
    DeoptAction, DeoptMetadataError, DeoptReason, DeoptVerifier, DeoptimizationPoint, FrameState,
    FrameValue, MonitorInfo, ResumeSemantics, VirtualObjectState,
};

/// The Phase C monitor lane, as `x64/deopt_stubs.rs::build_frame_state_at`
/// builds it: one entry per held scalar monitor, naming the object by the
/// intra-frame id edge `VirtualObjectRef(new_pc)`.
fn scalar_monitor(id: usize) -> MonitorInfo {
    MonitorInfo {
        object: FrameValue::VirtualObjectRef(id),
        // Non-zero: the verifier's monitor lane rejects `lock_depth == 0`
        // separately ("a lock nobody holds"), and this test is about the
        // reference edge, not about depth.
        lock_depth: 1,
        // Phase C's elided lock must be re-acquired on resume.
        relock: true,
    }
}

/// The locals lane's definition of the same object — what `sr_local_prov_at`
/// produces when the receiver IS stored into a local.
fn scalar_definition(id: usize) -> FrameValue {
    FrameValue::VirtualObject(VirtualObjectState {
        array_element_type: None,
        id,
        class_id: 1,
        // `num_fields` must equal `field_values.len()`, or
        // `VirtualObjectFieldCountMismatch` fires as well and this test would
        // be asserting about two defects at once.
        num_fields: 1,
        field_values: vec![FrameValue::Int(0)],
    })
}

/// A single-pass-shaped point: `bci == frame_state.bci` (both come from one
/// `resume_bci_for` call in `build_and_record_deopt_point`), and
/// `semantics == ResumeSemantics::for_reason(reason)` (the producer stamps
/// exactly that).
///
/// Keeping both identities true matters: `check_point` reports
/// `PointBciMismatch` and `ResumeSemanticsMismatch` independently of everything
/// else, and a fixture that got either wrong would make the assertions below
/// pass for the wrong reason.
fn point(locals: Vec<FrameValue>, monitors: Vec<MonitorInfo>) -> DeoptimizationPoint {
    DeoptimizationPoint {
        native_offset: 0x80,
        bci: 17,
        reason: DeoptReason::BoundsCheck,
        action: DeoptAction::Reinterpret,
        speculation_id: 0,
        semantics: ResumeSemantics::for_reason(DeoptReason::BoundsCheck),
        frame_state: FrameState {
            method_key: "T.m:()V".to_string(),
            bci: 17,
            locals,
            stack: Vec::new(),
            monitors,
            caller: None,
        },
    }
}

/// The wave-5 shape: `new; dup; <init>()V; dup; monitorenter` locks the receiver
/// straight off the operand stack and never stores it, so the monitor lane names
/// a shell the locals lane does not define.
///
/// CONTRACT THE ASSERTION RESTS ON. `DeoptVerifier::violations` returns EVERY
/// violation it finds and never short-circuits (its own doc: "the second
/// violation is usually the one that explains the first"), so the right
/// assertion is about the CONTENTS of the returned `Vec`, not about its being a
/// singleton — and it is checked as a singleton here only because every other
/// lane is deliberately quiet for this fixture:
///
///   * no `MethodFrameLimits` is registered, so the scope lane is off
///     (`check_scope` guards it on `!self.methods.is_empty()`);
///   * `requiring_oop_map` defaults to `false` and no `OopCoverage` is
///     registered, so neither oop-map direction can fire;
///   * `lock_depth` is 1 and the object appears once, so the monitor-balance
///     lane is quiet;
///   * `monitor_object_defect` returns `None` for `VirtualObjectRef` (it is a
///     reference descriptor), and `value_blocks_resume` returns `false` for it
///     (`names_unspilled_register`'s catch-all arm), so the "cannot be
///     reconstructed" and "not a reference" reports do not fire either;
///   * one point means `windows(2)` is empty, so the sortedness lane is quiet.
///
/// If a future lane starts reporting something else about this fixture, the
/// length assertion fails and names what appeared — which is the outcome wanted,
/// not a surprise.
#[test]
fn a_monitor_naming_an_undefined_virtual_object_is_rejected() {
    let points = vec![point(
        // The locals lane defines NOTHING: this is the defect.
        vec![FrameValue::Int(0)],
        vec![scalar_monitor(7)],
    )];

    let violations = DeoptVerifier::new().violations(&points);

    assert_eq!(
        violations.len(),
        1,
        "expected exactly the undefined-reference report; got {violations:?}"
    );
    assert!(
        matches!(
            &violations[0],
            DeoptMetadataError::UndefinedVirtualObjectRef { id, .. } if *id == 7
        ),
        "expected UndefinedVirtualObjectRef for id 7, got {:?}",
        violations[0]
    );
}

/// The positive control, and the reason the check above is not simply "monitors
/// referencing virtual objects are illegal": the SAME monitor lane is accepted
/// once the locals lane defines the object it names. That is the shape Phase C
/// produces when the receiver is stored (`astore`), which is the case the
/// feature exists for.
#[test]
fn the_same_monitor_is_accepted_once_the_locals_lane_defines_the_object() {
    let points = vec![point(vec![scalar_definition(7)], vec![scalar_monitor(7)])];

    let violations = DeoptVerifier::new().violations(&points);

    assert!(
        violations.is_empty(),
        "a defined virtual object referenced by a monitor is well-formed metadata; got \
         {violations:?}"
    );
}

/// `DeoptVerifier::violations` is also what catches the monitor lane's other
/// documented failure mode, and it is worth one assertion here because the
/// single-pass backend is the producer that could emit it: a monitor entry whose
/// object is a PRIMITIVE descriptor. The resume would build a `Value::Int`, the
/// interpreter's method-exit `monitorexit` would try to unlock it, and the real
/// monitor would stay held — "a hang in whatever thread asks for it next,
/// arbitrarily far from the deopt that caused it".
///
/// CONTRACT: `monitor_object_defect` returns `Some(..)` for every `FrameValue`
/// that is not one of the reference-shaped or already-unreconstructable
/// variants, and `check_scope` turns that into
/// `MonitorObjectNotAReference`. `FrameValue::StackSlot(-8)` is an `int` frame
/// slot, so it takes that arm. It does NOT take the `value_blocks_resume` arm
/// (an int slot is perfectly reconstructable), so exactly one report is
/// expected — this is the assertion the round's "reason each one through the
/// callee's actual contract" caution is about, because guessing two would have
/// been just as plausible.
#[test]
fn a_primitive_monitor_object_is_rejected() {
    let points = vec![point(
        vec![FrameValue::Int(0)],
        vec![MonitorInfo {
            object: FrameValue::StackSlot(-8),
            lock_depth: 1,
            relock: false,
        }],
    )];

    let violations = DeoptVerifier::new().violations(&points);

    assert_eq!(
        violations.len(),
        1,
        "expected exactly the not-a-reference report; got {violations:?}"
    );
    assert!(
        matches!(
            &violations[0],
            DeoptMetadataError::MonitorObjectNotAReference { .. }
        ),
        "expected MonitorObjectNotAReference, got {:?}",
        violations[0]
    );
}

/// A monitor on a MATERIALIZED object must ask the resume to acquire it.
///
/// Added in round 10 wave 6 (lane `deoptverify`) as a new lane of the existing
/// monitor-balance check. `relock == false` asserts "the compiled code already
/// holds this lock", which cannot be true of an object that does not exist until
/// the resume materializes it. If it were accepted, the resume would skip the
/// `monitorenter` and the interpreter frame would still run the `monitorexit`
/// the bytecode pairs with the enter — releasing a monitor this thread never
/// took.
///
/// CONTRACT THE ASSERTION RESTS ON, and why the count is one and not two: the
/// object here is `VirtualObjectRef(7)` and the locals lane DOES define id 7, so
/// the undefined-reference lane is quiet; `monitor_object_defect` returns `None`
/// for a `VirtualObjectRef` (it is reference-shaped), so the not-a-reference
/// lane is quiet; and `lock_depth` is 1 with no duplicate, so the depth and
/// re-entrancy lanes are quiet. Exactly one lane has anything to say.
#[test]
fn a_materialized_monitor_that_declines_to_relock_is_rejected() {
    let points = vec![point(
        vec![scalar_definition(7)],
        vec![MonitorInfo {
            object: FrameValue::VirtualObjectRef(7),
            lock_depth: 1,
            // The defect: nothing can be holding this object's monitor.
            relock: false,
        }],
    )];

    let violations = DeoptVerifier::new().violations(&points);

    assert_eq!(
        violations.len(),
        1,
        "expected exactly the unbalanced-monitor report; got {violations:?}"
    );
    assert!(
        matches!(&violations[0], DeoptMetadataError::UnbalancedMonitor { .. }),
        "expected UnbalancedMonitor, got {:?}",
        violations[0]
    );
}

/// The same entry with `relock: true` — what both producers actually emit — is
/// well-formed. Without this control the test above would also pass if the new
/// lane rejected every virtual-object monitor outright.
#[test]
fn a_materialized_monitor_that_relocks_is_accepted() {
    let points = vec![point(vec![scalar_definition(7)], vec![scalar_monitor(7)])];
    assert!(
        DeoptVerifier::new().violations(&points).is_empty(),
        "a relocking monitor on a defined virtual object is what Phase C emits"
    );
}

// ---------------------------------------------------------------------------
// The call site
// ---------------------------------------------------------------------------

/// The single-pass finalize path must actually run the verifier.
///
/// This is the grep from the known-issues page's "how a reviewer confirms this"
/// section, turned into a test: the page's confirmation procedure was
/// `rg -n "DeoptVerifier|verify_deopt_metadata" jit/src/x64/`, with "no hits"
/// meaning the gap was open. A hit is now required.
///
/// Textual, and it says so. It proves that the call exists in the file that
/// finalizes a single-pass artifact; it does not prove the call is reached, and
/// it would not notice the call being moved behind a condition that is never
/// true. That residual is exactly why the refusal is also a named bail site: a
/// verification that stops happening shows up as a census row that stops
/// appearing, rather than as nothing at all.
#[test]
fn the_single_pass_finalize_path_verifies_its_deopt_metadata() {
    let driver = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("x64")
        .join("driver.rs");
    let src = std::fs::read_to_string(&driver)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", driver.display()));

    // Not a comment mention: the call, with its argument list opening.
    let calls: Vec<&str> = src
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with("//"))
        .filter(|l| l.contains("verify_deopt_metadata("))
        .collect();
    assert_eq!(
        calls.len(),
        1,
        "expected exactly one deopt-metadata verification call in {}; found {:?}. \
         If a second install path grew its own call, register the new one here and say why \
         two are needed; if the call is gone, \
         docs/internal/fixed-bugs/r10-earelock-singlepass-deopt-metadata-is-never-verified-FIXED-20260922.md \
         is open again.",
        driver.display(),
        calls
    );

    // The refusal must be named. An unnamed refusal is a compile that vanishes,
    // which is the one failure mode worse than the defect it is refusing.
    assert!(
        src.contains("\"deopt-metadata-violation\""),
        "the verification refusal in {} must record a bail site so `jit-method-stats` can \
         report it",
        driver.display()
    );
}

/// The verifier is a shared component, so the *other* producer must keep its
/// call too. `ir_lower.rs` has verified its metadata since the checker was
/// written; this pins that closing the single-pass gap did not come at the cost
/// of the path that was already covered (a refactor that moved the call into one
/// shared helper and then lost a caller would be invisible otherwise).
#[test]
fn the_optimizing_backend_still_verifies_its_deopt_metadata() {
    let lowerer = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("ir_lower.rs");
    let src = std::fs::read_to_string(&lowerer)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", lowerer.display()));
    assert!(
        src.lines()
            .map(str::trim)
            .filter(|l| !l.starts_with("//"))
            .any(|l| l.contains("DeoptVerifier::new()")),
        "{} must still build a DeoptVerifier over the points it installs",
        lowerer.display()
    );
}
