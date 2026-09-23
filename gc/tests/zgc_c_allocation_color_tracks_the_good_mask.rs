// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `ZGoodMask::allocation_color` must be exactly the color the barrier is
//! letting through at that instant.
//!
//! The accessor used to be computed from `phase()` and `current_marked_bit()` —
//! two independent atomics that `flip_to_mark` writes as two independent stores.
//! The type's own documentation argues at length that the three masks are
//! *derived* from one word specifically so that a half-flipped state is
//! unrepresentable rather than merely unlikely; this accessor was the one
//! exception, and it is the one on the allocation path
//! (`ZTlabHeapHooks::allocation_color`, once per object).
//!
//! The property that matters is not "it reads one atomic" — that is an
//! implementation detail — but the consequence: **an object born with
//! `allocation_color()` is never born bad.** An object that fails the load
//! barrier's fast-path test at birth fails it on every load until something
//! recolors it, which is not a slow path but a permanent one.

use cratonvm_gc::zgc::vaddr::{
    color, ZColor, ZGlobalPhase, ZGoodMask, Z_MARKED0, Z_MARKED1, Z_REMAPPED,
};

/// A freshly allocated object is good in every phase, under both spellings of
/// the fast-path test.
#[test]
fn a_freshly_allocated_reference_is_never_bad_in_any_phase() {
    let mask = ZGoodMask::new();
    // Several full cycles, so both mark parities and the quiescent phase are
    // covered and the rotation is exercised rather than assumed.
    for cycle in 0..6u32 {
        for step in 0..3u32 {
            match step {
                0 => {
                    mask.flip_to_mark();
                }
                1 => mask.set_mark_complete(),
                _ => {
                    mask.flip_to_remap();
                }
            }
            let born = color(0x4000, mask.allocation_color());
            assert!(
                mask.is_good(born),
                "cycle {cycle} step {step}: an object born with allocation_color() \
                 failed the good-mask test (phase={:?}, good={:#x}, born={born:#x})",
                mask.phase(),
                mask.good(),
            );
            assert!(
                !mask.is_bad(born),
                "cycle {cycle} step {step}: ...and the bad-mask form must agree",
            );
            assert!(
                !mask.is_weak_bad(born),
                "cycle {cycle} step {step}: a weak load must not resurrect either",
            );
        }
    }
}

/// The color is the good mask's own bit, phase by phase — which is what makes
/// the agreement above structural rather than coincidental.
#[test]
fn the_allocation_color_is_the_good_masks_own_bit() {
    let mask = ZGoodMask::new();

    assert_eq!(mask.phase(), ZGlobalPhase::Relocate);
    assert_eq!(mask.good(), Z_REMAPPED);
    assert_eq!(mask.allocation_color(), ZColor::Remapped);

    assert_eq!(mask.flip_to_mark(), Z_MARKED0);
    assert_eq!(mask.allocation_color(), ZColor::Marked0);
    mask.set_mark_complete();
    assert_eq!(
        mask.allocation_color(),
        ZColor::Marked0,
        "mark-complete moves no colors, so it must move no allocation color",
    );

    assert_eq!(mask.flip_to_remap(), Z_REMAPPED);
    assert_eq!(mask.allocation_color(), ZColor::Remapped);

    assert_eq!(mask.flip_to_mark(), Z_MARKED1);
    assert_eq!(
        mask.allocation_color(),
        ZColor::Marked1,
        "the rotation must carry the allocation color with it, or objects \
         allocated in cycle N+1 are born wearing cycle N's mark",
    );

    // Stated as one invariant, for every state the type can be in.
    for _ in 0..4 {
        for flip in 0..2 {
            if flip == 0 {
                mask.flip_to_mark();
            } else {
                mask.flip_to_remap();
            }
            assert_eq!(
                mask.allocation_color().bit(),
                mask.good(),
                "allocation_color() must be the good mask's bit in phase {:?}",
                mask.phase(),
            );
        }
    }
}

/// Allocating mid-mark must not make work for the marker.
///
/// An object allocated during a mark cycle is implicitly live (nothing could
/// have referenced it before the snapshot). Born with the current mark bit it
/// is already good, so the barrier never pushes it onto a mark stack; born
/// `Remapped` it would be bad during marking — which is the *mechanism* by
/// which the barrier feeds the marker — and every reference to a brand-new
/// object would take the slow path exactly once for no reason.
#[test]
fn an_object_allocated_mid_mark_does_not_feed_the_mark_stack() {
    let mask = ZGoodMask::new();
    mask.flip_to_mark();

    let born = color(0x1000, mask.allocation_color());
    assert!(
        !mask.is_bad(born),
        "a mid-mark allocation must be born black"
    );

    // ...and the alternative really is bad, so the assertion above is not
    // vacuous.
    let remapped = color(0x1000, ZColor::Remapped);
    assert!(
        mask.is_bad(remapped),
        "Remapped is bad during marking — that is what makes the barrier the \
         marker's work source",
    );
    assert_ne!(remapped, born);
}
