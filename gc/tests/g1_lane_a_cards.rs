// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Lane A (G1 remembered sets and the card table) — the properties Phase 2's
//! card screen and the remembered set's pruning rest on, as an OUT-OF-CRATE
//! test.
//!
//! These live outside `gc/src` on purpose. The in-module unit tests in
//! `g1_cards.rs` can reach private state and so can assert about the bitset's
//! representation; this file can only use what the rest of the tree can use,
//! which is exactly the surface a caller could get wrong. The two properties
//! below are the ones whose failure is a use-after-free rather than a wrong
//! number:
//!
//! * the card CURSOR (`CardSet::next_dirty_addr`) must decide identically to
//!   the per-object query (`CardSet::any_in_span`) it replaced. Deciding
//!   "clean" for an object the old form called dirty means Phase 2 steps over
//!   a live cross-region reference, its referent is not evacuated, and Phase 5
//!   frees the region holding it;
//! * remembered-set PRUNING must be monotone in the collector's generation
//!   clock. An entry the scan side drops is an entry no later pause will ever
//!   act on, and that is only true while "stale" cannot un-fire.

use cratonvm_gc::g1_cards::{G1CardTable, G1_CARD_BYTES};
use cratonvm_gc::region::{RememberedSet, RSET_GENERATION_PINNED};

/// A synthetic arena. The table never dereferences the addresses it is given,
/// so a plausible, card-aligned base is enough.
const BASE: usize = 0x2000_0000;

/// A 1 MiB region's worth of cards — the real shape, so the word-at-a-time
/// scan is exercised across more than one `u64`.
const CARDS: usize = 2048;
const SPAN: usize = CARDS * G1_CARD_BYTES;

/// A cheap deterministic spread of dirty cards: no RNG dependency, but not a
/// pattern that lines up with 64-card word boundaries either.
fn dirty_pattern(table: &G1CardTable, stride: usize, offset: usize) {
    let mut c = offset;
    while c < CARDS {
        table.dirty_addr(BASE + c * G1_CARD_BYTES + 7);
        c += stride;
    }
}

/// The cursor and the per-object query must agree for EVERY object in a
/// synthetic grid, under several dirty-card densities and several object
/// sizes — including sizes larger than a card, which is the case where the
/// cursor's "is the card start below the object's end" test does the work.
#[test]
fn the_card_cursor_decides_exactly_what_the_per_object_query_decides() {
    for (stride, offset) in [
        (1usize, 0usize),
        (2, 1),
        (7, 3),
        (64, 0),
        (97, 5),
        (4096, 0),
    ] {
        let table = G1CardTable::new(BASE, SPAN);
        dirty_pattern(&table, stride, offset);
        let snap = table.snapshot(BASE, SPAN);

        for size in [8usize, 16, 24, 40, 512, 520, 1024, 1536, 4096] {
            // The walk's own state, maintained exactly as
            // `scan_source_region_for_cset_refs` maintains it.
            let mut cursor = snap.first_dirty_addr();
            let mut addr = BASE;
            while addr + size <= BASE + SPAN {
                if cursor.is_some_and(|d| d + G1_CARD_BYTES <= addr) {
                    cursor = snap.next_dirty_addr(addr);
                }
                let by_cursor = cursor.is_some_and(|d| d < addr + size);
                assert_eq!(
                    by_cursor,
                    snap.any_in_span(addr, size),
                    "stride={stride} offset={offset} size={size} at +{:#x}",
                    addr - BASE
                );
                // ... and against the live table, which is what the screen read
                // before the snapshot became unconditional.
                assert_eq!(
                    by_cursor,
                    table.any_dirty_in(addr, size),
                    "cursor disagrees with the TABLE: stride={stride} size={size} at +{:#x}",
                    addr - BASE
                );
                addr += size;
            }
        }
    }
}

/// The cursor is only ever consulted by a forward walk, and a forward walk
/// must never see it go backwards: that is what bounds the whole region walk's
/// cursor work at O(cards) instead of O(objects).
#[test]
fn the_cursor_never_moves_backwards_across_a_forward_walk() {
    let table = G1CardTable::new(BASE, SPAN);
    dirty_pattern(&table, 13, 2);
    let snap = table.snapshot(BASE, SPAN);

    let mut last = BASE;
    let mut addr = BASE;
    while addr < BASE + SPAN {
        if let Some(d) = snap.next_dirty_addr(addr) {
            assert!(d >= last, "cursor went backwards at +{:#x}", addr - BASE);
            assert!(
                d + G1_CARD_BYTES > addr,
                "cursor at +{:#x} names a card the walk has already passed",
                addr - BASE
            );
            last = d;
        }
        addr += 37 * 8;
    }
}

/// A clean-and-redirty pass leaves exactly the kept cards dirty and leaves the
/// saturation counter agreeing with a fresh scan of the table — the two
/// answers come from different code and a divergence would make the
/// "do the old regions saturate?" measurement meaningless.
#[test]
fn the_dirty_total_tracks_what_clean_and_redirty_leaves_behind() {
    let table = G1CardTable::new(BASE, SPAN);
    dirty_pattern(&table, 1, 0);
    assert_eq!(table.dirty_card_total(), CARDS);

    let mut keep = table.empty_set(BASE, SPAN);
    for c in (0..CARDS).step_by(16) {
        keep.insert_addr(BASE + c * G1_CARD_BYTES + 11);
    }
    let kept = keep.count();
    table.clean_and_redirty(BASE, SPAN, &keep);

    assert_eq!(table.dirty_card_total(), kept);
    assert_eq!(table.dirty_cards_in(BASE, SPAN), kept);
    // And the surviving set is reachable by the cursor, in order.
    let snap = table.snapshot(BASE, SPAN);
    let mut seen = 0usize;
    let mut at = snap.first_dirty_addr();
    while let Some(d) = at {
        seen += 1;
        at = snap.next_dirty_addr(d + G1_CARD_BYTES);
    }
    assert_eq!(seen, kept);
}

/// Cleaning is bounded by what the walk EXAMINED, never by the region cursor.
/// A walk that broke early must leave the cards past the break alone, or it
/// drops edges it never looked at.
#[test]
fn cleaning_a_prefix_leaves_the_suffix_untouched() {
    let table = G1CardTable::new(BASE, SPAN);
    dirty_pattern(&table, 1, 0);
    let keep = table.empty_set(BASE, SPAN / 4);
    table.clean_and_redirty(BASE, SPAN / 4, &keep);
    assert_eq!(table.dirty_cards_in(BASE, SPAN / 4), 0);
    assert_eq!(
        table.dirty_cards_in(BASE + SPAN / 4, SPAN - SPAN / 4),
        CARDS - CARDS / 4
    );
    // A zero-length walk cleans nothing at all: `walked_upto == 0` is what a
    // region whose very first header was unreadable produces.
    let table2 = G1CardTable::new(BASE, SPAN);
    dirty_pattern(&table2, 1, 0);
    let none = table2.empty_set(BASE, 0);
    table2.clean_and_redirty(BASE, 0, &none);
    assert_eq!(table2.dirty_card_total(), CARDS);
}

// ── remembered set ──────────────────────────────────────────────────────

/// `retain_and_collect_sources` must return exactly the survivors, and must
/// have actually deleted the rest — a prune that only filters its return value
/// would leave the additive set growing while reporting that it had not.
#[test]
fn prune_and_collect_deletes_what_it_does_not_return() {
    let rset = RememberedSet::default();
    for s in 0..200usize {
        rset.add_reference_in_generation(s, s as u64);
    }
    assert_eq!(rset.source_count(), 200);

    // "Stale" here stands in for `rset_entry_is_stale`: drop every entry whose
    // recorded generation is below 100.
    let live = rset.retain_and_collect_sources(|_s, gen| gen >= 100);
    assert_eq!(live.len(), 100);
    assert_eq!(rset.source_count(), 100);
    for s in live {
        assert!(s >= 100, "returned a source it claimed to drop: {s}");
        assert!(rset.names_source(s));
    }
    for s in 0..100usize {
        assert!(!rset.names_source(s), "source {s} survived the prune");
    }

    // The criterion is monotone in the real collector, so re-running it is a
    // no-op. That is the property the scan-side prune rests on.
    let again = rset.retain_and_collect_sources(|_s, gen| gen >= 100);
    assert_eq!(again.len(), 100);
    assert_eq!(rset.source_count(), 100);
}

/// A generation-less entry (`RSET_GENERATION_PINNED`) is the fail-safe: it must
/// outlive any age-based prune, because the caller that recorded it had no
/// generation to compare against and over-retention is the safe direction.
#[test]
fn a_pinned_entry_survives_every_age_prune() {
    let rset = RememberedSet::default();
    rset.add_reference(7); // the generation-less entry point
    rset.add_reference_in_generation(8, 3);
    assert_eq!(rset.recorded_generation(7), Some(RSET_GENERATION_PINNED));

    // Anything recorded before generation `u64::MAX` is dropped; the pinned
    // entry is not, for any finite "recycled in" generation.
    let live = rset.retain_and_collect_sources(|_s, gen| gen >= u64::MAX);
    assert_eq!(live, vec![7]);
    assert!(rset.names_source(7));
    assert!(!rset.names_source(8));
}

/// A COARSENED remembered set means "every region", so `names_source` must say
/// yes to all of them — a verifier that read the emptied map directly would
/// report every edge in the heap as unrecorded.
#[test]
fn a_coarsened_rset_names_every_source() {
    let rset = RememberedSet::default();
    // Drive the cap through the parameterised entry point so this test does not
    // publish process-global state to the rest of the binary.
    for s in 0..8usize {
        rset.add_reference_in_generation_within(s, 1, 4);
    }
    assert!(rset.is_coarsened());
    assert_eq!(rset.source_count(), 0);
    for s in [0usize, 7, 999, usize::MAX] {
        assert!(rset.names_source(s), "coarsened rset denied source {s}");
    }
    // ... and clearing (what `G1Region::reset` does) takes the precision back.
    rset.clear();
    assert!(!rset.is_coarsened());
    assert!(!rset.names_source(0));
}
