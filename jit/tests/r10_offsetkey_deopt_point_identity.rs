// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Round 10, lane `offsetkey`: the deopt-point identity invariant, pinned so it
//! cannot be re-derived from nothing.
//!
//! ## The invariant
//!
//! There are exactly three ways to name a deopt point, and only one of them is
//! an identity:
//!
//! | Key | Unique? | Who uses it |
//! |---|---|---|
//! | the address of the `deopt_points` element, baked as an imm64 by the point's own stub | **yes** | every path that RECONSTRUCTS a frame (`x64/deopt_stubs.rs::resolve_baked_point` → `deopt::x64_deopt_entry`) |
//! | `bci` | no — several points share one | the four VM-side reason lookups, `resume_image`, `osr_entry_frame_state`; each either refuses on disagreement or re-verifies against the live frame |
//! | `native_offset` | **no** | nothing. `CompiledMethod::find_deopt_point` was keyed on it, had no production caller, and was deleted in round 10 wave 9 |
//!
//! Round 10 wave 7 established the middle fact's harder half from the producer
//! side (`docs/internal/fixed-bugs/r10-interner-native-offset-is-not-a-unique-deopt-point-key-RESOLVED-20260922.md`):
//! `build_and_record_deopt_point` records `native_offset = buf.pos()` and *emits
//! no machine code*, and the three recorder families keep their idempotence in
//! three maps that never consult each other, so two metadata-only records at one
//! emitter pc land on one offset.
//!
//! ## Why this file is a TEXTUAL ratchet, and what that replaces
//!
//! Wave 7 could not edit the two files that asserted the opposite, so it left a
//! confirmation procedure made of two greps: `rg -n "unique per point" jit/src/`
//! and `rg -n "find_deopt_point" jit/src/osr_exit.rs jit/src/x64/loop_rewrite.rs`
//! "must go silent when the comments are corrected".
//!
//! Both of those still hit after the correction, and they hit *because* it
//! landed: a comment that explains why a retired claim was wrong has to quote
//! the claim and name the function. A recipe that cannot distinguish "the claim
//! is asserted" from "the claim is refuted" would have read as OPEN forever,
//! which is how a resolved page gets reopened by a reader following its own
//! instructions. So the procedure is this test instead, and it asserts the
//! distinction directly: the refutation must be PRESENT and the recommendation
//! must be GONE.
//!
//! A textual test proves only what the source text says. The behavioural half —
//! that the bci lookups refuse rather than guess on a same-offset pair, and that
//! the exceptional lookup has no reachable ambiguity — is asserted in
//! `jit/src/osr_exit.rs`'s own unit tests, which can reach private helpers this
//! file cannot.

use std::path::{Path, PathBuf};

fn jit_src(rel: &[&str]) -> (PathBuf, String) {
    let mut path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for part in rel {
        path = path.join(part);
    }
    let src = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    (path, src)
}

/// `osr_exit.rs` must not recommend an offset-keyed lookup to a caller that
/// holds an offset.
///
/// The retired sentence was *"A lookup keyed on `native_offset`
/// (`CompiledMethod::find_deopt_point`) is unique per point and is the right
/// answer wherever the offset is available"*, in `deopt_reason_at_bci`'s doc. It
/// is the dangerous half, not the factual error: `deopt_reason_at_bci` is sound
/// either way (it answers `Ambiguous` rather than picking), but a reader who
/// took the advice on an escalation or de-speculation path would have swapped
/// that refusal for a `binary_search_by_key` that has no `Ambiguous` to return.
///
/// The function it named has since been deleted (round 10 wave 9), which does
/// not retire this test: the recommendation would be just as wrong re-written
/// against a freshly re-added search, and what is pinned here is that the
/// *advice* stays gone, not that its callee does.
#[test]
fn the_reason_lookup_does_not_recommend_an_offset_keyed_alternative() {
    let (path, src) = jit_src(&["osr_exit.rs"]);
    assert!(
        !src.contains("is the right answer wherever the offset is available"),
        "{} recommends an offset-keyed lookup again. The offset is not an \
         identity and `find_deopt_point` was deleted for having no production \
         caller; the argument is in the retirement note left where it was \
         defined (`jit/src/lib.rs`) and in \
         docs/internal/fixed-bugs/r10-interner-native-offset-is-not-a-unique-deopt-point-key-RESOLVED-20260922.md",
        path.display()
    );
    // …and the refutation is present, so this is not vacuously green on a file
    // that simply lost the paragraph.
    assert!(
        src.contains("`native_offset` is not unique."),
        "{} no longer states that `native_offset` is not unique. Removing the \
         claim is not the same as refuting it: the next reader re-derives \
         uniqueness from the fact that nothing denies it.",
        path.display()
    );
}

/// `loop_rewrite.rs`'s `PointDifference::Divergent` must argue from the baked
/// box pointer alone.
///
/// The conclusion survived the loss of the offset disjunct — picking the wrong
/// unrolled copy of a point can only flip an OSR *entry* decision, because
/// `try_osr_entry` re-verifies every slot expectation against the live
/// interpreter frame — but a disjunction whose first half is fiction invites a
/// reader to lean on the wrong half. The surviving half is a true identity and
/// is per-copy: the `*_box_ptr_by_bci` maps are keyed by EMITTER pc, so each
/// copy gets its own entry and its own baked pointer.
#[test]
fn the_divergent_argument_rests_on_the_baked_pointer_not_the_offset() {
    let (path, src) = jit_src(&["x64", "loop_rewrite.rs"]);
    assert!(
        !src.contains("finds its point by native offset"),
        "{} argues from an offset-keyed lookup again. `find_deopt_point` was \
         deleted for having no production caller and the offset is not unique; \
         the baked box pointer (`x64::deopt_stubs::resolve_baked_point`) is the \
         whole argument.",
        path.display()
    );
    assert!(
        src.contains("resolve_baked_point"),
        "{} no longer names the identity its safety argument rests on. \
         `PointDifference::Divergent` is only safe because a reconstructing path \
         resolves its point through the address its own stub baked.",
        path.display()
    );
}

/// The offset-keyed search stays DELETED — definition included.
///
/// `CompiledMethod::find_deopt_point` is the function both retired comments
/// pointed at. Round 10 wave 9 deleted it: it had no production caller, its
/// only remaining users were three assertions in `ir_lower.rs`'s own test
/// module (replaced by direct assertions on `cm.deopt_points`, which can make
/// the stronger "exactly once" claim an offset-keyed search could not), and the
/// sortedness it needed is checked by `DeoptVerifier`'s structural lane on the
/// install path in release builds rather than by a `debug_assert`.
///
/// This test was previously "no production CALL SITE", which was the strongest
/// thing sayable while the definition survived. It is now the stronger claim,
/// and the weaker one is kept beside it so that re-adding the function without
/// re-adding a caller still fails here — the order of work, if a consumer ever
/// genuinely needs an offset-keyed lookup, is to give the recorders one
/// identity first, then make the verifier enforce it, then write the lookup.
#[test]
fn find_deopt_point_stays_deleted() {
    for rel in [
        vec!["lib.rs"],
        vec!["deopt.rs"],
        vec!["ir_lower.rs"],
        vec!["osr_exit.rs"],
        vec!["osr_entry.rs"],
        vec!["x64", "loop_rewrite.rs"],
        vec!["x64", "deopt_stubs.rs"],
        vec!["x64", "driver.rs"],
    ] {
        let (path, src) = jit_src(&rel);
        // A call is `find_deopt_point(`; a mention in prose is not. Comment
        // lines are skipped for the reason `vm/tests/no_test_only_public_api.rs`
        // had to be taught ("a doc comment is not a use"): every one of these
        // files still DISCUSSES this search — that is what keeps the decision
        // legible — and counting those lines would make the assertion fail for
        // the wrong reason.
        let hits: Vec<&str> = src
            .lines()
            .map(str::trim)
            .filter(|l| !l.starts_with("//"))
            .filter(|l| l.contains("find_deopt_point"))
            .collect();
        assert!(
            hits.is_empty(),
            "{} has `find_deopt_point` back in code (not prose). That search \
             resolves a same-`native_offset` pair arbitrarily, and the retirement \
             note in jit/src/lib.rs states the order of work required before any \
             offset-keyed lookup may exist again: make the recorders agree on one \
             identity, make the verifier enforce it, then write the lookup. \
             Found: {hits:?}",
            path.display()
        );
    }
}
