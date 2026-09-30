// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Wave-2 lane A — scan-time remembered-set pruning, made cheap.
//!
//! `RememberedSet::retain_and_collect_sources` is the fused
//! "snapshot the entries, judge them, act on the live ones, delete the dead
//! ones" primitive `G1Collector::live_rset_sources` calls when
//! `CRATONVM_G1_RSET_PRUNE_ON_SCAN` is on. Wave 1 measured the prune arm as
//! slower and pushed it back to opt-in; wave 2's finding is that what it cost
//! was the WRITING, which it did on every scanned source whether or not a
//! single entry was dead.
//!
//! The rewrite makes the write conditional on there being something to delete.
//! That is only safe if the primitive's OBSERVABLE behaviour is unchanged, so
//! this file pins the behaviour rather than the shape:
//!
//! * it returns exactly the surviving sources;
//! * it deletes exactly the refused ones and nothing else;
//! * refusing nothing leaves the set bit-for-bit as it was, including the
//!   generation stamps — the case the fast path exists for;
//! * and the result is stable under repetition, which is what makes "a `false`
//!   here is a permanent judgement" true.
//!
//! An over-eager prune is a dropped remembered-set entry, which is a source
//! region a later pause never walks, which is a live cross-region reference
//! whose referent is freed. That is why this is pinned by test and not by
//! reading.

use cratonvm_gc::region::RememberedSet;

fn populated(n: usize) -> RememberedSet {
    let rset = RememberedSet::default();
    for s in 0..n {
        // Generation = source index, so a predicate can select on either and
        // the test can tell which one the implementation actually passed.
        rset.add_reference_in_generation(s, s as u64);
    }
    rset
}

fn sorted(mut v: Vec<usize>) -> Vec<usize> {
    v.sort_unstable();
    v
}

#[test]
fn keeping_everything_returns_everything_and_changes_nothing() {
    let rset = populated(200);
    let before = rset.source_count();

    let live = rset.retain_and_collect_sources(|_, _| true);
    assert_eq!(sorted(live), (0..200).collect::<Vec<_>>());
    assert_eq!(rset.source_count(), before);

    // Every stamp survives untouched. The fast path returns before it can
    // write, so a stamp that moved would mean it wrote anyway.
    for s in 0..200 {
        assert_eq!(
            rset.recorded_generation(s),
            Some(s as u64),
            "source {s}'s generation stamp was disturbed by a prune that deleted nothing"
        );
    }
}

#[test]
fn refusing_some_deletes_exactly_those_and_returns_exactly_the_rest() {
    let rset = populated(200);

    // Refuse the multiples of three.
    let live = rset.retain_and_collect_sources(|s, _| s % 3 != 0);
    let want: Vec<usize> = (0..200).filter(|s| s % 3 != 0).collect();
    assert_eq!(sorted(live), want);
    assert_eq!(rset.source_count(), want.len());

    for s in 0..200 {
        if s % 3 == 0 {
            assert_eq!(
                rset.recorded_generation(s),
                None,
                "source {s} was refused but is still named"
            );
            assert!(!rset.names_source(s));
        } else {
            assert_eq!(
                rset.recorded_generation(s),
                Some(s as u64),
                "source {s} was kept but its stamp changed"
            );
            assert!(rset.names_source(s));
        }
    }
}

/// The predicate must see the recorded GENERATION, not just the index. The
/// staleness test the collector drives this with
/// (`recorded_generation < source.recycled_in_generation`) is a statement about
/// the stamp, and a primitive that passed the wrong one would prune on the
/// wrong evidence.
#[test]
fn the_predicate_is_handed_the_recorded_generation() {
    let rset = RememberedSet::default();
    rset.add_reference_in_generation(10, 7);
    rset.add_reference_in_generation(11, 99);

    let mut seen: Vec<(usize, u64)> = Vec::new();
    let live = rset.retain_and_collect_sources(|s, g| {
        seen.push((s, g));
        g >= 50
    });
    assert_eq!(live, vec![11]);
    assert_eq!(sorted_pairs(seen), vec![(10, 7), (11, 99)]);
    assert!(!rset.names_source(10));
    assert!(rset.names_source(11));
}

fn sorted_pairs(mut v: Vec<(usize, u64)>) -> Vec<(usize, u64)> {
    v.sort_unstable();
    v
}

/// Re-recording an entry after it was pruned puts it back with the NEWER stamp.
///
/// This is what makes scan-time pruning sound rather than merely cheap: the
/// mutator post-write barrier re-records any edge the re-typed source stores
/// afterwards, so a pruned-then-relevant source is named again before the pause
/// that needs it.
#[test]
fn a_pruned_source_comes_back_when_the_barrier_records_it_again() {
    let rset = populated(8);
    rset.retain_and_collect_sources(|s, _| s != 3);
    assert!(!rset.names_source(3));

    rset.add_reference_in_generation(3, 42);
    assert!(rset.names_source(3));
    assert_eq!(rset.recorded_generation(3), Some(42));

    let live = rset.retain_and_collect_sources(|_, _| true);
    assert_eq!(sorted(live), (0..8).collect::<Vec<_>>());
}

/// The judgement cannot un-fire, so running the prune twice with the same
/// predicate is idempotent — the second call finds nothing to delete and takes
/// the no-write path.
#[test]
fn the_prune_is_idempotent() {
    let rset = populated(64);
    let first = sorted(rset.retain_and_collect_sources(|s, _| s % 5 != 0));
    let second = sorted(rset.retain_and_collect_sources(|s, _| s % 5 != 0));
    assert_eq!(first, second);
    assert_eq!(rset.source_count(), first.len());
}

/// An empty set is not a special case anywhere, and the fast path must not turn
/// it into one.
#[test]
fn an_empty_remembered_set_prunes_to_nothing() {
    let rset = RememberedSet::default();
    assert!(rset.retain_and_collect_sources(|_, _| true).is_empty());
    assert!(rset.retain_and_collect_sources(|_, _| false).is_empty());
    assert_eq!(rset.source_count(), 0);
}

/// A COARSENED remembered set names no individual source, so there is nothing
/// to prune — and pruning it must not un-coarsen it, because coarsening means
/// "any region could hold an edge into me" and forgetting that would drop every
/// source at once.
#[test]
fn pruning_a_coarsened_set_leaves_it_coarsened() {
    let rset = RememberedSet::default();
    // Drive it over a small cap through the test-only entry point, so this does
    // not depend on the process-global `CRATONVM_G1_RSET_SOURCE_CAP`.
    for s in 0..32 {
        rset.add_reference_in_generation_within(s, 1, 8);
    }
    assert!(rset.is_coarsened());

    let live = rset.retain_and_collect_sources(|_, _| false);
    assert!(live.is_empty());
    assert!(
        rset.is_coarsened(),
        "a prune un-coarsened a remembered set, which drops every source it stood for"
    );
    assert!(rset.names_source(12345));
}
