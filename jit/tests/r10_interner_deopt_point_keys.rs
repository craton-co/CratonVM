// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Round 10, lane `interner`: two decisions about deopt metadata, pinned.
//!
//! ## 1. `native_offset` is not a unique key, and the verifier deliberately
//!    tolerates the duplicate
//!
//! `DeoptVerifier`'s structural lane checks that `deopt_points` is ordered by
//! `native_offset`, with `w[0].native_offset > w[1].native_offset` — **strictly
//! greater**, so two points at the SAME offset are accepted. That looks like an
//! oversight and is not: `x64/deopt_stubs.rs::build_and_record_deopt_point`
//! records `native_offset = buf.pos()` and *emits no machine code*, and the
//! three recorder families that call it keep their idempotence in three maps
//! that do not see each other (`deopt_box_ptr_by_bci`,
//! `osr_exit_box_ptr_by_bci`, `exc_frame_box_ptr_by_bci`). Two adjacent
//! metadata-only records at one pc therefore land on one offset, and tightening
//! the lane to `>=` would refuse a well-formed artifact to prove a hypothesis.
//!
//! This lane was not permitted to build or run the VM, so **no compile was
//! observed producing a duplicate** — the claim pinned here is only about what
//! the verifier does with one, which is a pure-API question and does not need a
//! compile. The reachability argument, and the three places in the tree that
//! currently assert uniqueness in prose, are in
//! `docs/internal/fixed-bugs/r10-interner-native-offset-is-not-a-unique-deopt-point-key-RESOLVED-20260922.md`.
//!
//! Both halves are asserted, because only having one is how a "tolerance"
//! silently becomes "the check does nothing": the equal pair must be accepted
//! AND the decreasing pair must be reported.
//!
//! ## 2. The interned frame-state representation is retired, not hiding
//!
//! `FrameStateInterner` and friends were deleted from `jit/src/deopt.rs` on
//! 2026-09-21 (`docs/jit/deopt-frame-state-interning.md` §7). The last two tests
//! are textual ratchets on that: a *reappearance* of the interner, or a silent
//! tightening of the sortedness lane to `>=`, should fail here and send the
//! author to the argument rather than letting them rediscover it.
//!
//! A textual test proves only what the source text says. It cannot prove the
//! lane is reached — that is what `DeoptVerifier`'s single install-time entry
//! point and `jit/tests/r10_deoptverify_singlepass_metadata.rs` cover.

use std::path::Path;

use cratonvm_jit::deopt::{
    DeoptAction, DeoptMetadataError, DeoptReason, DeoptVerifier, DeoptimizationPoint, FrameState,
    ResumeSemantics,
};

/// A point that every always-on lane accepts, so the only thing a violation can
/// be about is the ordering.
///
/// Deliberately minimal: no `MethodFrameLimits` is registered on the verifier
/// (so the scope lane is inactive), no oop coverage (so the agreement lane is
/// inactive), empty locals/stack/monitors (so no slot lane fires), and
/// `semantics` matches `for_reason(reason)` so the exception-state-agreement
/// lane is satisfied. `bci == frame_state.bci`, which the point-vs-frame
/// identity check requires.
fn clean_point_at(native_offset: u32, bci: u32) -> DeoptimizationPoint {
    DeoptimizationPoint {
        native_offset,
        bci,
        reason: DeoptReason::BoundsCheck,
        action: DeoptAction::Reinterpret,
        speculation_id: 0,
        frame_state: FrameState {
            method_key: String::from("T.m:(I)I"),
            bci,
            locals: Vec::new(),
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        },
        semantics: ResumeSemantics::for_reason(DeoptReason::BoundsCheck),
    }
}

/// Control: one clean point produces nothing. If this fails, every other
/// assertion in this file is measuring the wrong thing.
#[test]
fn the_fixture_point_is_clean() {
    let errs = DeoptVerifier::new().violations(&[clean_point_at(0x40, 7)]);
    assert!(
        errs.is_empty(),
        "fixture point is not clean, so the ordering assertions below prove nothing: {errs:?}"
    );
}

/// Two points at one `native_offset` are LEGAL metadata.
///
/// The decision, not an accident. `find_deopt_point` would have resolved such a
/// pair arbitrarily, which is why that search was deleted rather than wired into
/// a resume path (see the retirement note in `jit/src/lib.rs`); the refusal
/// belongs at whichever
/// producer would be wrong to emit the pair, not at the verifier, which cannot
/// tell a legitimate pair from a mistaken one.
#[test]
fn two_deopt_points_at_one_native_offset_are_not_reported() {
    // Different bcis, same offset: the shape that would make an offset-keyed
    // lookup ambiguous about WHICH bci to resume at.
    let points = [clean_point_at(0x80, 11), clean_point_at(0x80, 12)];
    let errs = DeoptVerifier::new().violations(&points);
    assert!(
        errs.is_empty(),
        "the sortedness lane is deliberately `>` and must accept the equal pair — \
         if this is now a violation, the change needs the argument in \
         docs/internal/fixed-bugs/r10-interner-native-offset-is-not-a-unique-deopt-point-key-RESOLVED-20260922.md \
         answered first: {errs:?}"
    );
}

/// …and the check is not vacuous: a point at a LOWER offset following a higher
/// one is reported, with both offsets named.
#[test]
fn a_decreasing_native_offset_pair_is_reported() {
    let points = [clean_point_at(0x80, 11), clean_point_at(0x40, 12)];
    let errs = DeoptVerifier::new().violations(&points);
    let unsorted: Vec<&DeoptMetadataError> = errs
        .iter()
        .filter(|e| matches!(e, DeoptMetadataError::DeoptPointsUnsorted { .. }))
        .collect();
    assert_eq!(
        unsorted.len(),
        1,
        "expected exactly one DeoptPointsUnsorted for one inversion; got {errs:?}"
    );
    let DeoptMetadataError::DeoptPointsUnsorted { first, second } = unsorted[0] else {
        unreachable!("filtered above")
    };
    assert_eq!((*first, *second), (0x80, 0x40));

    // The rendered message must name the producer, not a search with no caller.
    // Round 10 retired the `find_deopt_point` justification and then the
    // function: a reader who followed that citation found a dead end and could
    // not tell whether the lane protected a live search or a fossil.
    let msg = unsorted[0].to_string();
    assert!(
        msg.contains("0x80") && msg.contains("0x40"),
        "the message must name both offsets: {msg}"
    );
    assert!(
        !msg.contains("find_deopt_point"),
        "the ordering finding is a statement about the PRODUCER (the emitter walks \
         forward), not about a binary search nothing calls: {msg}"
    );
}

// ---------------------------------------------------------------------------
// Textual ratchets on the two round-10 decisions
// ---------------------------------------------------------------------------

fn deopt_rs() -> (std::path::PathBuf, String) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("deopt.rs");
    let src = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    (path, src)
}

/// The interned representation stays deleted, or its return is argued for.
///
/// Textual and it says so: this is the confirmation procedure from
/// `docs/internal/fixed-bugs/r10-deoptverify-frame-state-interner-has-no-production-user-RETIRED-20260922.md`
/// inverted. That page's check was "these names appear nowhere outside
/// `jit/src/deopt.rs`" and the resolution was to delete them from there too, so
/// the ratchet is that they appear nowhere at all except as prose in the
/// retirement note.
///
/// A reappearance is not forbidden — stage 2 is still a real backlog item. What
/// is forbidden is a reappearance that does not come with a producer, because
/// "landed, unreached, measured only by its own tests" is the state this round
/// deleted 2 065 lines over.
#[test]
fn the_frame_state_interner_is_not_back_without_a_producer() {
    let (path, src) = deopt_rs();
    for name in [
        "FrameStateInterner",
        "SharedFrameState",
        "InternedDeoptPoint",
        "InterningStats",
        "violations_interned",
    ] {
        // Prose is allowed: the retirement note in `deopt.rs` names every one of
        // these while explaining that they are gone. Only code is forbidden, so
        // comment lines are skipped — the same distinction
        // `vm/tests/no_test_only_public_api.rs` had to be taught ("a doc comment
        // is not a use", and its own honest header was hiding the corpse).
        let code_hits: Vec<&str> = src
            .lines()
            .map(str::trim)
            .filter(|l| !l.starts_with("//"))
            .filter(|l| l.contains(name))
            .collect();
        assert!(
            code_hits.is_empty(),
            "`{name}` is back in {} as code, not prose: {code_hits:?}. \
             docs/jit/deopt-frame-state-interning.md §7 says what a return has to come with.",
            path.display()
        );
    }
}

/// The sortedness lane stays `>`.
///
/// This is the one decision in this file that a future author could reverse in a
/// single character, in good faith, believing it a tightening. The assertion is
/// on the comparison itself so that reversing it fails here rather than in a
/// probe arm.
#[test]
fn the_sortedness_lane_still_tolerates_the_equal_pair() {
    let (path, src) = deopt_rs();
    let compares: Vec<&str> = src
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with("//"))
        .filter(|l| l.contains("w[0].native_offset") && l.contains("w[1].native_offset"))
        .collect();
    assert_eq!(
        compares.len(),
        1,
        "expected exactly one native-offset ordering comparison in {}; found {compares:?}",
        path.display()
    );
    assert!(
        compares[0].contains("w[0].native_offset > w[1].native_offset"),
        "the ordering lane must stay strictly `>`: `>=` would refuse an artifact whose two \
         metadata-only deopt records landed at one pc, which nothing in the tree forbids. \
         Found: {:?}",
        compares[0]
    );
}
