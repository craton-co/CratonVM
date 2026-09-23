// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Lane E, wave 2 (2026-09-20) — the other half of humongous placement, and the
//! counters that say when it was not enough.
//!
//! Wave 1 made the contiguous-run search best-fit, which stops a humongous SPAN
//! eating the head of a long run. It does nothing about the case
//! `lane-e-proposals.md` P3 actually describes: *"A 4-region hole left by a dead
//! 4 MiB array is fragmented by four unrelated Old promotions, and G1 has no
//! compaction pass that can rebuild it."* A single-region claim out of the
//! MIDDLE of a run of length `n` leaves two runs summing to `n − 1`, and a
//! humongous allocation can only use the longer of them — so the heap's ability
//! to satisfy a large span falls by far more than the one region the claim
//! took, permanently.
//!
//! `CRATONVM_G1_HUMONGOUS_RUN_GUARD=1` makes such a claim shorten the longest
//! run instead of splitting it. That is placement only: both arms hand back a
//! Free region, and neither can fail where the other succeeds.
//!
//! The second half of this file is proposal P6, the smallest item on that page
//! and probably the one that saves the most debugging time: an
//! `OutOfMemoryError` on a heap with 40 % of its regions Free is the signature
//! of humongous fragmentation, and until now nothing in any log said so.

use cratonvm_gc::g1::{g1_dbg_best_fit_run, g1_dbg_run_preserving_choice};
use cratonvm_gc::{G1Collector, G1CollectorConfig};
use cratonvm_types::ArrayElementType;

/// `.` = free, `#` = occupied. Reads like the heap diagram in the proposal.
fn map(pattern: &str) -> Vec<bool> {
    pattern.chars().map(|c| c == '.').collect()
}

// ---------------------------------------------------------------------------
// The run guard, as placement
// ---------------------------------------------------------------------------

#[test]
fn a_single_region_claim_shortens_the_longest_run_instead_of_splitting_it() {
    //                    0123456789
    let free = map("#.........");
    // The longest run is [1, 10). Only its LAST region may be claimed; every
    // other index would split it.
    assert_eq!(g1_dbg_run_preserving_choice(&free, 0), Some(9));
    // ...regardless of where the rotating hint happens to be pointing.
    for hint in 0..free.len() {
        assert_eq!(
            g1_dbg_run_preserving_choice(&free, hint),
            Some(9),
            "hint {hint} escaped the guard"
        );
    }
}

#[test]
fn a_claim_prefers_a_region_outside_the_longest_run_altogether() {
    //             0123456789abcd
    let free = map("#.##.........#");
    // Index 1 is an isolated Free region and index 4..13 is the longest run.
    // Spending the isolated one first is strictly better: it costs no span
    // capacity at all.
    assert_eq!(g1_dbg_run_preserving_choice(&free, 0), Some(1));
}

#[test]
fn the_guard_still_finds_a_region_when_every_free_one_is_in_the_run() {
    // A heap whose ONLY free regions are the interior of one long run must
    // still be able to serve an Old claim — a placement policy that could
    // refuse would turn fragmentation into an OutOfMemoryError.
    let free = map("###....###");
    let choice = g1_dbg_run_preserving_choice(&free, 0).expect("must still serve the claim");
    assert!(free[choice], "the guard handed out an occupied region");
    assert_eq!(choice, 6, "and it must be the END of the run");
}

#[test]
fn the_guard_does_not_protect_runs_too_short_to_split() {
    // A run of one or two cannot be SPLIT by a single-region claim — taking
    // either end of a two-run leaves one run of one, not two runs. Protecting
    // it would cost the hinted scan for nothing.
    let free = map("#.#.#.#.");
    let choice = g1_dbg_run_preserving_choice(&free, 0).unwrap();
    assert!(free[choice]);
    let free2 = map("#..#..#.");
    let choice2 = g1_dbg_run_preserving_choice(&free2, 0).unwrap();
    assert!(free2[choice2]);
}

#[test]
fn the_guard_never_refuses_a_claim_the_plain_scan_would_serve() {
    // The correctness invariant, over every free-map of eight regions: the two
    // arms agree on WHETHER a region exists, and the guard's answer is always a
    // Free region. They differ only in WHICH one, which is the whole point.
    for bits in 0u32..256 {
        let free: Vec<bool> = (0..8).map(|i| bits & (1 << i) != 0).collect();
        let any_free = free.iter().any(|&f| f);
        for hint in 0..8usize {
            let guarded = g1_dbg_run_preserving_choice(&free, hint);
            assert_eq!(
                guarded.is_some(),
                any_free,
                "bits={bits:08b} hint={hint}: the guard disagreed about whether a \
                 free region exists"
            );
            if let Some(i) = guarded {
                assert!(free[i], "bits={bits:08b}: the guard handed out region {i}");
            }
        }
    }
}

#[test]
fn the_guard_preserves_span_capacity_that_the_plain_scan_destroys() {
    // The number the change is actually for, stated as a before/after on the
    // longest run. Ten free regions; claim four single regions from the hint at
    // index 0, which is what a run of Old promotions does.
    let run_of_ten = "#..........";

    let mut plain = map(run_of_ten);
    let mut guarded = map(run_of_ten);
    for _ in 0..4 {
        // The plain scan takes the lowest free region at or after the hint.
        let i = plain.iter().position(|&f| f).unwrap();
        plain[i] = false;
        let j = g1_dbg_run_preserving_choice(&guarded, 0).unwrap();
        guarded[j] = false;
    }

    // Both heaps now hold six free regions. The question is whether they are
    // adjacent.
    assert_eq!(plain.iter().filter(|&&f| f).count(), 6);
    assert_eq!(guarded.iter().filter(|&&f| f).count(), 6);
    assert_eq!(
        g1_dbg_best_fit_run(&guarded, 6),
        Some(1),
        "the guard must leave a six-region run intact"
    );
    assert!(
        g1_dbg_best_fit_run(&plain, 6).is_some(),
        "this particular pattern happens to survive the plain scan too — the \
         interesting case is the interleaved one below"
    );

    // Now the case the proposal actually describes: claims arriving with a
    // rotating hint, which is what `free_scan_hint` produces under mixed
    // allocation. The plain scan splits; the guard does not.
    let mut plain = map("#..........");
    let mut guarded = map("#..........");
    for (n, hint) in [3usize, 7, 5, 9].into_iter().enumerate() {
        let i = (hint..plain.len())
            .chain(0..hint)
            .find(|&i| plain[i])
            .unwrap();
        plain[i] = false;
        let j = g1_dbg_run_preserving_choice(&guarded, hint).unwrap();
        guarded[j] = false;
        assert_eq!(
            plain.iter().filter(|&&f| f).count(),
            guarded.iter().filter(|&&f| f).count(),
            "claim {n}: the two arms must consume the same NUMBER of regions"
        );
    }
    let longest = |m: &[bool]| -> usize {
        let (mut best, mut run) = (0usize, 0usize);
        for &f in m {
            run = if f { run + 1 } else { 0 };
            best = best.max(run);
        }
        best
    };
    assert!(
        longest(&guarded) > longest(&plain),
        "the guard left a longest run of {} where the plain scan left {} — if \
         these are equal the guard is not buying anything on this shape",
        longest(&guarded),
        longest(&plain)
    );
    assert_eq!(
        longest(&guarded),
        6,
        "four claims off the end of a ten-run must leave a six-run"
    );
}

// ---------------------------------------------------------------------------
// Proposal P6 — a humongous failure that says which kind it was
// ---------------------------------------------------------------------------

#[test]
fn humongous_requests_are_counted_and_a_failure_names_the_shape() {
    // Eight regions of 64 KiB. The humongous threshold is `region_size / 2`, so
    // a 40 KiB array is not humongous and a 200 KiB one spans four regions.
    let region_size = 64 * 1024usize;
    let gc = G1Collector::new(G1CollectorConfig {
        heap_size: 8 * region_size,
        initial_heap_size: 8 * region_size,
        region_size,
        ..Default::default()
    });

    let (req0, fail0, _, _) = gc.humongous_alloc_counts();
    assert_eq!((req0, fail0), (0, 0));

    // One that fits.
    let ok = gc.try_alloc_array(
        cratonvm_types::ClassId::new(1),
        ArrayElementType::Long,
        // Three regions' worth of `long` elements: 8 bytes each, so
        // `3 * region_size / 8` elements is exactly three regions of data and
        // comfortably over the `region_size / 2` humongous threshold.
        3 * region_size / 8,
    );
    assert!(
        ok.is_some(),
        "a three-region span must fit in an empty heap"
    );

    // THE PRECONDITION, ASSERTED RATHER THAN ASSUMED.
    //
    // Everything below is about the humongous path, and every one of these
    // allocations reaches it only if the size arithmetic above is right. Get
    // one factor of 8 wrong and the array is an ordinary Eden object, the
    // humongous counters stay at zero, and the test passes while exercising
    // none of the code it is named for. (The first draft of this file had
    // exactly that bug: `3 * region_size / 8 / 8` elements is 3/8 of a region,
    // comfortably UNDER the `region_size / 2` humongous threshold.) So: the
    // region table must now actually contain a humongous span.
    let hum_start = gc.count_regions(cratonvm_gc::region::RegionType::HumongousStart);
    let hum_cont = gc.count_regions(cratonvm_gc::region::RegionType::HumongousContinuation);
    assert_eq!(
        hum_start, 1,
        "the allocation did not become a humongous span at all — the element          count is not over the region_size/2 threshold and nothing below this          line tests what it says it tests"
    );
    // THREE regions of element data, but FOUR regions of span: the span is
    // sized by the whole object, and the array header pushes
    // `3 * region_size + ARRAY_DATA_OFFSET` over the third region boundary, so
    // `size.div_ceil(region_size)` is 4. Asserted at the exact number rather
    // than `>= 2`, because the arithmetic below ("an 8-region heap cannot hold
    // three of these") depends on it and a span that quietly became 3 regions
    // would make that loop stop failing.
    assert_eq!(
        hum_cont, 3,
        "three regions of `long` data plus an array header is a FOUR-region          span: one HumongousStart and three HumongousContinuation"
    );

    let (req1, fail1, _, _) = gc.humongous_alloc_counts();
    assert!(
        req1 > req0,
        "a humongous allocation must be counted whether or not it succeeds"
    );
    assert_eq!(fail1, 0, "it fitted, so nothing failed");

    // Now exhaust the heap with humongous spans until one cannot be served.
    // Eight regions, four per span: the second fits exactly, the third cannot,
    // so this MUST fail rather than merely may.
    let mut failed = false;
    for _ in 0..16 {
        if gc
            .try_alloc_array(
                cratonvm_types::ClassId::new(1),
                ArrayElementType::Long,
                3 * region_size / 8,
            )
            .is_none()
        {
            failed = true;
            break;
        }
    }
    assert!(
        failed,
        "an 8-region heap cannot hold three 4-region spans, so one of these          allocations had to fail — if none did, they are not humongous"
    );
    {
        let (req, fail, free_at_fail, longest_at_fail) = gc.humongous_alloc_counts();
        assert!(req > fail, "every failure is also a request");
        assert!(fail > 0);
        // The two numbers that make the diagnosis. `longest_at_fail` shorter
        // than the span requested with a non-trivial `free_at_fail` IS the
        // fragmentation signature; equal-to-zero free regions is plain
        // exhaustion. Either is a legitimate outcome here — what matters is
        // that they are now REPORTED rather than inferred.
        assert!(longest_at_fail <= free_at_fail);
    }
}
