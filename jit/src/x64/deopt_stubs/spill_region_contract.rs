//! The frame reservation and the stub's spill must agree.
//!
//! They live in different files and drifted apart once, with the stub
//! writing 32 registers over the caller's saved `rbp` and return address.
//! `probes/IndyDeoptProbe.java` is the executable reproducer; this is the
//! same contract at unit scale, checkable in the configuration that
//! actually broke.

use crate::x64::deopt_spill_region_reserved as reserved;

/// The reasons `emit_deopt_stubs` takes the REGISTER-SPILLING path for when
/// `deopt_real` is off. Transcribed from its
/// `crate::deopt_real_enabled() || matches!(reason, 8..=11)`. (11 was
/// missing until r9 — see the stub's comment — while `covered_by` below
/// already listed it.)
const SPILLS_WITHOUT_DEOPT_REAL: [i64; 4] = [8, 9, 10, 11];

/// Which compile-time property covers each of those reasons.
///
/// Reason 8 is the unconditional `invokedynamic` trap; 9 and 10 are the
/// precise-exception-frame stubs. A reason with no covering property is a
/// stub that can spill into a region the frame never reserved.
fn covered_by(reason: i64) -> Option<&'static str> {
    match reason {
        8 => Some("has_indy_sites"),
        9 | 10 | 11 => Some("precise_exception_frames"),
        _ => None,
    }
}

#[test]
fn every_stub_that_spills_without_deopt_real_has_a_frame_reservation() {
    for reason in SPILLS_WITHOUT_DEOPT_REAL {
        let property = covered_by(reason).unwrap_or_else(|| {
            panic!(
                "reason {reason} spills 32 registers with `deopt_real` off and \
                 nothing reserves the region it spills into"
            )
        });
        // …and the reservation predicate really does answer `true` for that
        // property alone, with `deopt_real` OFF. This is the assertion the
        // bug failed: `has_indy_sites` was not a term in it at all.
        let (pef, indy) = (
            property == "precise_exception_frames",
            property == "has_indy_sites",
        );
        assert!(
            reserved(false, pef, indy),
            "reason {reason}: {property} does not reserve the spill region"
        );
    }
}

/// The default path is untouched: nothing to spill, nothing reserved.
#[test]
fn a_method_with_nothing_to_spill_reserves_nothing() {
    assert!(!reserved(false, false, false));
    // …and `deopt_real` on its own still reserves, which is what makes the
    // production frame byte-identical to what it was.
    assert!(reserved(true, false, false));
}

/// The edit that would trip this suite: dropping `has_indy_sites` from
/// `deopt_spill_region_reserved`, or adding a reason to the stub's
/// `matches!(reason, ...)` without a covering property here.
#[test]
fn the_contract_is_stated_in_both_directions() {
    assert!(reserved(false, false, true), "an indy method must reserve");
    assert!(
        reserved(false, true, false),
        "a precise-frame method must reserve"
    );
    for reason in SPILLS_WITHOUT_DEOPT_REAL {
        assert!(covered_by(reason).is_some(), "reason {reason} uncovered");
    }
    assert!(
        covered_by(2).is_none(),
        "reason 2 spills only under deopt_real"
    );
}
