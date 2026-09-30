// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Wave-2 lane A — the DENSITY-ADAPTIVE card cursor.
//!
//! Wave 1 landed `CRATONVM_G1_CARD_CURSOR` default-on and then pushed it back
//! to opt-in on a measurement. The diagnosis in
//! `docs/internal/g1-2026-09-20/orchestrator-wave-1-measurements.md` §4 was
//! that the cursor pays only when a region's dirty cards are SPARSE, so wave 2
//! makes the choice per region. This file covers the two things that has to be
//! true for that to be safe:
//!
//! * **the choice cannot change what is scanned.** The cursor and the
//!   per-object query decide identically at EVERY density, so switching
//!   between them mid-heap — which is what a per-region choice does — can
//!   never make the walk step over an object. If it could, the failure would
//!   not be a slow pause: it would be a live cross-region reference the pause
//!   never followed, whose referent Phase 5 then frees;
//! * **the density predicate says what it means.** `is_at_most_percent_dirty`
//!   is what the walk's threshold is compared against, and a predicate that is
//!   wrong at the boundary picks the wrong arm — harmlessly, but silently, and
//!   a lever nobody can predict is a lever nobody can measure.

use cratonvm_gc::g1_cards::{G1CardTable, G1_CARD_BYTES};

/// A synthetic arena. The table never dereferences the addresses it is given.
const BASE: usize = 0x3000_0000;

/// A 1 MiB region's worth of cards — the real shape, so the density sweep
/// crosses many `u64` words.
const CARDS: usize = 2048;
const SPAN: usize = CARDS * G1_CARD_BYTES;

/// The threshold `gc/src/g1.rs`'s `LANE_A_CURSOR_MAX_DIRTY_PERCENT` carries.
/// Duplicated here on purpose: this file is outside the crate and can only use
/// the public surface, and a test that imported the constant would pass for any
/// value of it.
const THRESHOLD_PERCENT: usize = 75;

/// Dirty every `stride`-th card starting at `offset`, i.e. a density of
/// `1/stride`.
fn dirty_every(table: &G1CardTable, stride: usize, offset: usize) {
    let mut c = offset;
    while c < CARDS {
        table.dirty_addr(BASE + c * G1_CARD_BYTES + 7);
        c += stride;
    }
}

/// THE property. For every density the adaptive gate can be handed, and for
/// object sizes on both sides of a card, the cursor's verdict equals the
/// query's — so a walk that switches arms per region scans exactly the same
/// objects as one that never switches.
#[test]
fn the_two_arms_agree_at_every_density_the_adaptive_gate_can_pick() {
    // 1 = every card dirty (fully dense), 2 = half, ... 4096 = one card in a
    // 2048-card region (as sparse as this shape gets).
    for stride in [1usize, 2, 3, 4, 7, 16, 64, 97, 512, 2048, 4096] {
        for offset in [0usize, 1, 5] {
            let table = G1CardTable::new(BASE, SPAN);
            dirty_every(&table, stride, offset);
            let snap = table.snapshot(BASE, SPAN);

            for size in [8usize, 16, 24, 40, 96, 512, 520, 1024, 1536, 4096] {
                // The walk's own cursor state, maintained exactly as
                // `walk_source_region_for_cset_refs` maintains it.
                let mut next_dirty = snap.first_dirty_addr();
                let mut addr = BASE;
                while addr + size <= BASE + SPAN {
                    if next_dirty.is_some_and(|d| d + G1_CARD_BYTES <= addr) {
                        next_dirty = snap.next_dirty_addr(addr);
                    }
                    let by_cursor = next_dirty.is_some_and(|d| d < addr + size);
                    let by_query = snap.any_in_span(addr, size);
                    assert_eq!(
                        by_cursor,
                        by_query,
                        "cursor and query disagreed: stride={stride} offset={offset} \
                         size={size} at +{:#x} (dirty={} of {})",
                        addr - BASE,
                        snap.count(),
                        snap.covered_cards(),
                    );
                    // ...and against the LIVE table, which is what the walk
                    // falls back to when no snapshot was taken.
                    assert_eq!(
                        by_query,
                        table.any_dirty_in(addr, size),
                        "snapshot and live table disagreed: stride={stride} size={size} \
                         at +{:#x}",
                        addr - BASE,
                    );
                    addr += size;
                }
            }
        }
    }
}

/// The predicate the threshold is compared against, at and around its boundary.
#[test]
fn the_density_predicate_is_exact_at_the_boundary() {
    // An all-clean run is 0% dirty and therefore sparse by any threshold.
    let clean = G1CardTable::new(BASE, SPAN);
    let snap = clean.snapshot(BASE, SPAN);
    assert_eq!(snap.count(), 0);
    assert!(snap.is_at_most_percent_dirty(0));
    assert!(snap.is_at_most_percent_dirty(THRESHOLD_PERCENT));

    // Every card dirty is 100%: dense for the shipping threshold, and still
    // "at most 100%" for a threshold of 100.
    let full = G1CardTable::new(BASE, SPAN);
    dirty_every(&full, 1, 0);
    let snap = full.snapshot(BASE, SPAN);
    assert_eq!(snap.count(), CARDS);
    assert!(!snap.is_at_most_percent_dirty(THRESHOLD_PERCENT));
    assert!(!snap.is_at_most_percent_dirty(99));
    assert!(snap.is_at_most_percent_dirty(100));

    // Exactly half. Sparse under the shipping threshold, dense under 49.
    let half = G1CardTable::new(BASE, SPAN);
    dirty_every(&half, 2, 0);
    let snap = half.snapshot(BASE, SPAN);
    assert_eq!(snap.count(), CARDS / 2);
    assert!(snap.is_at_most_percent_dirty(50));
    assert!(snap.is_at_most_percent_dirty(THRESHOLD_PERCENT));
    assert!(!snap.is_at_most_percent_dirty(49));

    // Three cards in four — exactly the shipping boundary, which the predicate
    // must include rather than exclude (`<=`).
    let three_quarters = G1CardTable::new(BASE, SPAN);
    for c in 0..CARDS {
        if c % 4 != 3 {
            three_quarters.dirty_addr(BASE + c * G1_CARD_BYTES);
        }
    }
    let snap = three_quarters.snapshot(BASE, SPAN);
    assert_eq!(snap.count(), CARDS * 3 / 4);
    assert!(snap.is_at_most_percent_dirty(THRESHOLD_PERCENT));
    assert!(!snap.is_at_most_percent_dirty(THRESHOLD_PERCENT - 1));
}

/// An EMPTY covered run is not "sparse". A region whose walked span is zero
/// cards has no density, and a predicate that answered `true` there would be
/// deciding a policy from a division by zero.
#[test]
fn an_empty_covered_run_is_not_sparse() {
    let table = G1CardTable::new(BASE, SPAN);
    // A span of zero bytes covers no cards.
    let snap = table.snapshot(BASE, 0);
    assert_eq!(snap.covered_cards(), 0);
    assert!(!snap.is_at_most_percent_dirty(THRESHOLD_PERCENT));
    assert!(!snap.is_at_most_percent_dirty(100));
    // ...and the cursor over it finds nothing, so the walk that takes the
    // adaptive `false` branch and the one that takes `true` both scan nothing.
    assert!(snap.first_dirty_addr().is_none());
}
