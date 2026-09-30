// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `ZForwardIndex` answers exactly what a map of the same pairs would, at every
//! size, on both sides of the bucket-directory threshold.
//!
//! # Why this file exists
//!
//! [`cratonvm_gc::zgc::relocate::ZForwardIndex`] is the one table the
//! stop-the-world slide's reference-rewrite pass consults, once per non-null
//! reference slot of every survivor. On 2026-09-20 its lookup stopped being a
//! plain `binary_search` over the whole entry array and became a **bucket
//! directory plus a search inside one bucket** — `buckets[b]` is the first
//! entry whose `(from - lo) >> bucket_shift` is at least `b`, and `get` narrows
//! to `entries[buckets[b] .. buckets[b + 1]]`.
//!
//! That is a pure performance change and it must stay one. The failure mode if
//! it is not is the worst this subsystem has: a bucket whose sub-slice excludes
//! the matching entry answers `None`, `get` reports "this object did not move",
//! and the rewrite pass leaves a reference pointing into a span the slide
//! vacated and `compact_low_to` then **zeroed**. The reader sees a valid,
//! all-zero header — `num_slots=0` — and walks off the end of a zero-length
//! object. That is the exact signature
//! `docs/internal/fixed-suite-bugs/netty/zgc-relocate-slid-survivors-over-an-unselected-page-FIXED-20260814.md`
//! records, reached from a different direction, and the reason the check here
//! is a differential one against `HashMap` rather than a spot check of a few
//! addresses.
//!
//! So every test below asserts **agreement with a `HashMap` built from the same
//! pairs**, over sizes that straddle `ZFWD_INDEX_MIN_ENTRIES` and over key
//! distributions chosen to stress the directory rather than the average case:
//! dense runs (many keys per bucket), one enormous gap (many empty buckets),
//! and keys at both ends of the range (the `b` and `b + 1` index bounds).
//!
//! No wall-clock assertions anywhere, per this repo's CI-flake rule: the change
//! is about cost and every assertion here is about answers.

#![cfg(feature = "zgc")]

use std::collections::HashMap;

use cratonvm_gc::zgc::relocate::{ZForwardIndex, ZFWD_INDEX_MIN_ENTRIES};

/// A deterministic, dependency-free 64-bit stream. `rand` is not a dev
/// dependency of this crate and a GC differential test must be reproducible
/// from its seed alone — a flake here is indistinguishable from the corruption
/// it is looking for.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// The reference implementation: what the index must agree with, for every
/// probe, always. Deliberately the dumbest possible thing.
fn oracle(pairs: &[(usize, usize)]) -> HashMap<usize, usize> {
    pairs
        .iter()
        .copied()
        .filter(|(from, to)| from != to)
        .collect()
}

/// Probe `index` and `map` with the same address and require the same answer
/// from all three accessors.
#[track_caller]
fn agree(index: &ZForwardIndex, map: &HashMap<usize, usize>, probe: usize) {
    let expected = map.get(&probe).copied();
    assert_eq!(
        index.get(probe),
        expected,
        "ZForwardIndex::get disagreed with the oracle at {probe:#x} \
         ({} entries) — a bucket sub-slice that excludes its own key answers \
         `did not move`, and the rewrite pass then leaves a reference in a \
         span the slide zeroed",
        index.len(),
    );
    assert_eq!(
        index.contains_from(probe),
        expected.is_some(),
        "contains_from disagreed with get at {probe:#x}",
    );
    assert_eq!(
        index.resolve(probe),
        expected.unwrap_or(probe),
        "resolve must be `get` with the identity as its default, at {probe:#x}",
    );
}

/// Build the index from `pairs` and probe it with every key, every key ± one
/// grid unit, and a scatter of addresses across (and outside) the whole range.
fn differential(pairs: &[(usize, usize)], probe_span: (usize, usize)) {
    let index = ZForwardIndex::from_pairs(pairs);
    let map = oracle(pairs);
    assert_eq!(
        index.len(),
        map.len(),
        "the index must hold one entry per distinct non-identity pair",
    );

    // Every key, and its two grid neighbours. The neighbours are where an
    // off-by-one in the bucket bound shows up: they usually fall in the same
    // bucket as a real key and must still answer `None`.
    for &(from, _) in pairs {
        agree(&index, &map, from);
        agree(&index, &map, from.wrapping_sub(8));
        agree(&index, &map, from.wrapping_add(8));
    }

    // A sweep across the whole probe span on the object grid, so empty buckets
    // and the terminator are exercised as well as populated ones.
    let (lo, hi) = probe_span;
    let step = (((hi - lo) / 997) | 7) + 1; // 8-aligned, ~1000 samples
    let mut a = lo;
    while a <= hi {
        agree(&index, &map, a);
        a += step;
    }

    // Outside the range, both ends. These must be rejected by the `[lo, hi]`
    // test before any bucket index is computed — which is also what makes the
    // in-range bucket arithmetic unchecked-safe.
    agree(&index, &map, lo.saturating_sub(4096));
    agree(&index, &map, hi + 4096);
}

/// Dense, contiguous survivors: the shape a slide over a well-packed region
/// produces, and the one that puts many keys in a single bucket.
#[test]
fn a_dense_run_of_moves_answers_exactly_what_a_map_would() {
    const BASE: usize = 0x1_0000_0000;
    for &n in &[1usize, 2, 8, 63, 64, 65, 1000, 4096] {
        let pairs: Vec<(usize, usize)> = (0..n)
            .map(|i| (BASE + i * 32, BASE + 0x20_0000 + i * 32))
            .collect();
        differential(&pairs, (BASE - 64, BASE + n * 32 + 64));
    }
}

/// The threshold itself, from both sides. `ZFWD_INDEX_MIN_ENTRIES` selects
/// between the plain search and the bucketed one, so the two arms have to be
/// shown to agree at the exact entry count where the choice flips — otherwise
/// a suite that only ever builds small indices tests the arm production does
/// not run, which is how `CRATONVM_ZGC_PARSWEEP` came to be green and vacuous.
#[test]
fn both_lookup_arms_agree_across_the_directory_threshold() {
    const BASE: usize = 0x2_0000_0000;
    for delta in 0..3usize {
        let n = ZFWD_INDEX_MIN_ENTRIES - 1 + delta;
        let pairs: Vec<(usize, usize)> = (0..n)
            .map(|i| (BASE + i * 8, BASE + 0x10_0000 + i * 8))
            .collect();
        differential(&pairs, (BASE - 64, BASE + n * 8 + 64));
    }
}

/// Sparse keys separated by one enormous gap: most buckets are empty, and an
/// empty bucket's sub-slice must be empty rather than out of range or (worse)
/// the neighbouring bucket's.
#[test]
fn a_sparse_range_with_a_huge_gap_answers_exactly_what_a_map_would() {
    const BASE: usize = 0x3_0000_0000;
    const GAP: usize = 64 * 1024 * 1024;
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    for i in 0..200usize {
        pairs.push((BASE + i * 16, BASE + 8 + i * 16));
    }
    for i in 0..200usize {
        pairs.push((BASE + GAP + i * 16, BASE + GAP + 8 + i * 16));
    }
    differential(&pairs, (BASE - 64, BASE + GAP + 200 * 16 + 64));
}

/// Unsorted input with both slide directions mixed, which is what
/// `relocate_stw_admitted` actually hands `from_pairs`: the low slide emits
/// ascending pairs and the high pack descending ones, so the concatenation is
/// neither.
#[test]
fn an_unsorted_two_directional_pair_list_answers_exactly_what_a_map_would() {
    const BASE: usize = 0x4_0000_0000;
    let mut rng = SplitMix64(0xC0FF_EE00_1234_5678);
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    // Low slide: ascending sources moving down.
    for i in 0..1500usize {
        pairs.push((BASE + 0x40_0000 + i * 24, BASE + i * 24));
    }
    // High pack: descending sources moving up.
    for i in 0..500usize {
        let from = BASE + 0x100_0000 - i * 40;
        pairs.push((from, from + 0x8_0000));
    }
    // And shuffle, so nothing about the answer can depend on input order.
    for i in (1..pairs.len()).rev() {
        let j = (rng.next() % (i as u64 + 1)) as usize;
        pairs.swap(i, j);
    }
    differential(&pairs, (BASE - 64, BASE + 0x100_0000 + 64));
}

/// Randomly scattered 8-aligned keys. The distributions above are the ones the
/// slide produces; this one is the one that finds an arithmetic error in the
/// directory, because it satisfies no structure the code could accidentally be
/// relying on.
#[test]
fn scattered_random_keys_answer_exactly_what_a_map_would() {
    const BASE: usize = 0x5_0000_0000;
    const SPAN: u64 = 32 * 1024 * 1024;
    for seed in [1u64, 0xDEAD_BEEF, 0x5A47_4352_4C43_0001] {
        let mut rng = SplitMix64(seed);
        let mut froms: Vec<usize> = (0..3000)
            .map(|_| BASE + ((rng.next() % SPAN) & !7u64) as usize)
            .collect();
        // Distinct sources only: one address forwarded to two destinations is a
        // contradiction the index refuses (`debug_assert!`), not a case to test.
        froms.sort_unstable();
        froms.dedup();
        let pairs: Vec<(usize, usize)> = froms
            .iter()
            .enumerate()
            .map(|(i, &f)| (f, BASE + SPAN as usize + i * 8))
            .collect();
        differential(&pairs, (BASE - 64, BASE + SPAN as usize + 64));
    }
}

/// An identity pair is not a move, at any size, and must not create a bucket
/// entry that makes `contains_from` claim the slide vacated an address it left
/// alone — which is the fact `rewrite_target_is_walkable` uses to tell "this
/// slide broke the object" from "it arrived broken".
#[test]
fn identity_pairs_never_enter_the_index_however_many_there_are() {
    const BASE: usize = 0x6_0000_0000;
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    for i in 0..2000usize {
        let from = BASE + i * 16;
        if i % 3 == 0 {
            pairs.push((from, from)); // did not move
        } else {
            pairs.push((from, from + 0x40_0000));
        }
    }
    let index = ZForwardIndex::from_pairs(&pairs);
    let map = oracle(&pairs);
    assert_eq!(index.len(), map.len());
    for i in 0..2000usize {
        let from = BASE + i * 16;
        agree(&index, &map, from);
        if i % 3 == 0 {
            assert!(
                !index.contains_from(from),
                "{from:#x} did not move, so the slide did not vacate it",
            );
        }
    }
    // The stored entries stay ascending and identity-free, which is what the
    // `src_pages` filter in `relocate_stw_admitted` iterates.
    assert!(index.entries().windows(2).all(|w| w[0].0 < w[1].0));
    assert!(index.entries().iter().all(|(f, t)| f != t));
}

/// An empty index forwards nothing and must not index a bucket array it never
/// built. `lo = usize::MAX`, `hi = 0` is the state that makes the range test
/// reject everything before any arithmetic runs.
#[test]
fn an_empty_index_forwards_nothing_and_touches_no_directory() {
    let index = ZForwardIndex::from_pairs(&[]);
    assert!(index.is_empty());
    assert_eq!(index.len(), 0);
    for probe in [0usize, 8, 0x1000, usize::MAX / 2, usize::MAX] {
        assert_eq!(index.get(probe), None);
        assert!(!index.contains_from(probe));
        assert_eq!(index.resolve(probe), probe);
    }
    // And the same for an input that is entirely identity pairs, which reduces
    // to the empty case after filtering.
    let all_identity: Vec<(usize, usize)> =
        (0..100).map(|i| (0x1000 + i * 8, 0x1000 + i * 8)).collect();
    let index = ZForwardIndex::from_pairs(&all_identity);
    assert!(index.is_empty());
    assert_eq!(index.get(0x1000), None);
}
