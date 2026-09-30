// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `ZRememberedSet::iterate_snapshot_retaining` — the edge that must survive
//! more than one young cycle, and the ratchet it must not become.
//!
//! # What broke, and why it had no test
//!
//! Wave 1 of this round found that `ZRememberedSet::swap` wipes the buffer the
//! *previous* cycle scanned, so a bit protected its old→young edge for exactly
//! **one** young cycle:
//!
//! ```text
//!   interval 0   mutator stores old.f = young_obj   -> bit set in buffer A
//!   cycle 1      swap: A is the snapshot, B is wiped and goes current
//!                scan A, trace young_obj, it survives
//!   interval 1   nobody stores into old.f again     -> B stays clean
//!   cycle 2      swap: B is the snapshot, A IS WIPED
//!                scan B: empty. old.f -> young_obj is NOT a root.
//!                young_obj is freed while old.f still points at it.
//! ```
//!
//! The bit is not lost to a race. It is erased **on schedule**, by a swap
//! doing exactly what it is documented to do, and the store barrier cannot
//! repair it because no store happens in interval 1 — that is the definition
//! of a long-lived edge. The fix was `iterate_snapshot_retaining`, which
//! re-sets into `current` every slot the scan finds still pointing into young.
//!
//! The orchestrator's probe exercised it end to end (five young cycles, 2.05 M
//! promotions, 89.6 k re-cards, checksum identical). What it did not have was a
//! unit-level pin, so the property that a *five*-cycle probe happens to cover
//! could be lost by a change that only ever gets run for two.
//!
//! # The two halves
//!
//! 1. **It must persist.** A single store, then N cycles with no further
//!    store, and the edge is still a root on every one of them. The negative
//!    control is the plain `iterate_snapshot`, which loses it at cycle 2 —
//!    that is the *bug*, asserted here so the test cannot pass vacuously.
//! 2. **It must not ratchet.** "Re-set every bit you just read" is the shape
//!    of a structure that only grows, and a remembered set that only grows
//!    turns every young cycle into a whole-heap scan one bit at a time. The
//!    bound is structural — a fixed-size bitmap over the page — and this pins
//!    it against an `f` that retains *everything*, forever.
//!
//! No wall-clock assertions: every assertion here is a count or a membership.

#![cfg(feature = "zgc")]

use cratonvm_gc::zgc::remembered::{ZRememberedSet, Z_REMSET_GRAIN_BYTES};

/// A 4 KiB old page. Small enough that "every slot" is a cheap loop and large
/// enough that the bitmap is several words.
const PAGE_BYTES: usize = 4096;

/// One long-lived old→young edge, at a slot that is never stored into again.
const EDGE_OFFSET: usize = 8 * Z_REMSET_GRAIN_BYTES;

/// One young cycle's worth of remembered-set lifecycle: swap, then scan.
///
/// `retain` is what the caller answers for each edge — i.e. "does this slot
/// still hold a young reference?". Returns the offsets the scan yielded, which
/// is exactly the set of roots the young cycle would have traced.
fn cycle_retaining(set: &ZRememberedSet, retain: bool) -> Vec<usize> {
    set.swap();
    let mut seen = Vec::new();
    set.iterate_snapshot_retaining(|offset| {
        seen.push(offset);
        retain
    });
    set.clear_snapshot();
    seen
}

/// The same cycle with the **non**-retaining scan: the pre-fix behaviour, kept
/// as the negative control.
fn cycle_forgetting(set: &ZRememberedSet) -> Vec<usize> {
    set.swap();
    let mut seen = Vec::new();
    set.iterate_snapshot(|offset| seen.push(offset));
    set.clear_snapshot();
    seen
}

/// **The use-after-free this method exists to prevent.**
///
/// One store, then five young cycles with no further store. The edge must be a
/// root on every single one of them; the first cycle at which it is not is the
/// cycle that frees a live object.
#[test]
fn a_stored_edge_survives_every_young_cycle_not_just_the_first() {
    let set = ZRememberedSet::new(7, PAGE_BYTES);
    assert!(set.remember(EDGE_OFFSET));

    for cycle in 1..=5 {
        let roots = cycle_retaining(&set, true);
        assert_eq!(
            roots,
            vec![EDGE_OFFSET],
            "cycle {cycle}: the old->young edge must still be a root. The first \
             cycle at which it is not is the cycle that frees a live young object, \
             and nothing else reports it.",
        );
    }

    assert_eq!(
        set.stats().bits_retained,
        5,
        "one carry-forward per cycle -- `bits_retained == 0` on a multi-cycle run \
         with a stable edge is the inert state, i.e. nobody is calling the \
         retaining scan at all",
    );
}

/// The negative control, and the reason the test above is not vacuous: with
/// the plain scan the same edge is gone by the **second** cycle.
///
/// If this ever stops failing to find the edge, `swap` has stopped wiping and
/// the whole retention argument needs rewriting.
#[test]
fn the_plain_scan_loses_the_same_edge_at_cycle_two() {
    let set = ZRememberedSet::new(7, PAGE_BYTES);
    assert!(set.remember(EDGE_OFFSET));

    assert_eq!(
        cycle_forgetting(&set),
        vec![EDGE_OFFSET],
        "cycle 1 sees the edge either way",
    );
    assert!(
        cycle_forgetting(&set).is_empty(),
        "cycle 2 must lose it -- that is the defect `iterate_snapshot_retaining` \
         exists to fix, and if it does not happen here the positive test above \
         proves nothing",
    );
}

/// An edge whose target died, was promoted, or was overwritten with `null`
/// answers `false` and is **dropped**. Without this the set would accumulate
/// every edge the program has ever made.
#[test]
fn an_edge_the_caller_declines_is_not_carried_forward() {
    let set = ZRememberedSet::new(7, PAGE_BYTES);
    assert!(set.remember(EDGE_OFFSET));

    assert_eq!(cycle_retaining(&set, false), vec![EDGE_OFFSET]);
    assert!(
        cycle_retaining(&set, true).is_empty(),
        "a declined edge must be gone by the next cycle, whatever the next \
         cycle's answer would have been",
    );
    assert_eq!(set.stats().bits_retained, 0);
}

/// **The retain path cannot grow the set without bound.**
///
/// The adversarial caller: retain *everything*, on a set where every slot is
/// dirty, for many more cycles than any workload would run between majors. The
/// destination is a fixed-size bitmap over the page, `fetch_or` is idempotent,
/// and the scan adds no pages — so the population converges on "one bit per
/// reference-sized slot the page can physically hold" and stays there.
///
/// The failure this pins is a retain path that ever spilled to a list, a map
/// or a second buffer: the bit count would climb past the slot count and the
/// young cycle's cost would climb with it.
#[test]
fn retaining_everything_converges_on_the_page_and_does_not_ratchet() {
    let set = ZRememberedSet::new(7, PAGE_BYTES);
    let slots = PAGE_BYTES / Z_REMSET_GRAIN_BYTES;

    // Dirty every slot on the page.
    for i in 0..slots {
        assert!(set.remember(i * Z_REMSET_GRAIN_BYTES));
    }

    let mut previous = 0usize;
    for cycle in 1..=32 {
        let roots = cycle_retaining(&set, true);
        assert_eq!(
            roots.len(),
            slots,
            "cycle {cycle}: the scan must yield each slot exactly once",
        );
        let now = set.stats().bits_set;
        assert!(
            now <= slots,
            "cycle {cycle}: {now} bits set on a page with only {slots} \
             reference-sized slots -- the retain path has grown a second store",
        );
        if cycle > 1 {
            assert_eq!(
                now, previous,
                "cycle {cycle}: the set must be at a fixed point, not still growing",
            );
        }
        previous = now;
    }

    // Storage is a function of the page, not of how many times it was scanned.
    let stats = set.stats();
    assert_eq!(stats.page_size, PAGE_BYTES);
    assert_eq!(
        stats.retained_bytes,
        std::mem::size_of::<ZRememberedSet>()
            + 2 * stats.word_count * std::mem::size_of::<std::sync::atomic::AtomicU64>(),
        "two fixed-size bitmaps and the struct -- nothing the scan can add to",
    );
}

/// An offset past the page's tail is refused rather than remembered, so a
/// retaining scan cannot be talked into writing outside the bitmap by a
/// caller answering `true` to a bogus offset.
#[test]
fn an_out_of_range_offset_is_refused_by_both_halves() {
    let set = ZRememberedSet::new(7, PAGE_BYTES);
    assert!(
        !set.remember(PAGE_BYTES + Z_REMSET_GRAIN_BYTES),
        "an offset past the page must be refused so the caller coarsens",
    );
    assert_eq!(set.stats().bits_set, 0);
    assert!(cycle_retaining(&set, true).is_empty());
}
