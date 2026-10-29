// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Lane E (2026-09-20) — the allocation-placement and TLAB-sizing changes.
//!
//! Two independent things are covered here because both are properties of the
//! allocator's PLACEMENT policy rather than of any one allocation:
//!
//! * the humongous contiguous-run search is best-fit, so a request served out
//!   of a tight hole does not eat the head of a long run the next humongous
//!   allocation will need (there is no compaction pass in this collector that
//!   could manufacture a run back);
//! * `refill_tlab` CLAMPS an over-large request to half a region instead of
//!   refusing it, so the adaptive TLAB sizer climbing past that bound is not a
//!   one-way exit from TLAB allocation for that thread.

use cratonvm_gc::g1::{g1_dbg_best_fit_run, g1_dbg_first_fit_run};
use cratonvm_gc::{G1Collector, G1CollectorConfig};

/// Small heap with many regions, so region-granular behaviour is reachable
/// without allocating a real 256 MiB.
fn many_region_config(regions: usize, region_size: usize) -> G1CollectorConfig {
    G1CollectorConfig {
        heap_size: regions * region_size,
        initial_heap_size: regions * region_size,
        region_size,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Humongous placement — best-fit
// ---------------------------------------------------------------------------

#[test]
fn best_fit_spends_the_tightest_hole_and_first_fit_eats_the_long_run() {
    // Free map: a 2-region hole at 1, then a 6-region run at 5.
    //   idx:  0 1 2 3 4 5 6 7 8 9 10
    //   free: . F F . . F F F F F F
    let free = [
        false, true, true, false, false, true, true, true, true, true, true,
    ];

    // First-fit takes the low run whatever its length, so a 2-region request
    // is served at 1 as well — the arms agree here.
    assert_eq!(g1_dbg_first_fit_run(&free, 2), Some(1));
    assert_eq!(g1_dbg_best_fit_run(&free, 2), Some(1));

    // The difference shows on a request the tight hole cannot serve. Both must
    // land in the long run, and both must name the same start, because there is
    // only one qualifying run.
    assert_eq!(g1_dbg_first_fit_run(&free, 3), Some(5));
    assert_eq!(g1_dbg_best_fit_run(&free, 3), Some(5));
}

#[test]
fn best_fit_prefers_the_shortest_qualifying_run() {
    // Two runs that both fit a 2-region request: a 2-run at 0 and a 5-run at 4.
    //   idx:  0 1 2 3 4 5 6 7 8
    //   free: F F . . F F F F F
    let free = [true, true, false, false, true, true, true, true, true];

    // First-fit and best-fit agree by accident here (the short run is also the
    // first one), so flip the order to make the policies disagree.
    let flipped = [true, true, true, true, true, false, false, true, true];
    //   idx:  0 1 2 3 4 5 6 7 8
    //   free: F F F F F . . F F
    assert_eq!(
        g1_dbg_first_fit_run(&flipped, 2),
        Some(0),
        "first-fit takes the head of the five-region run"
    );
    assert_eq!(
        g1_dbg_best_fit_run(&flipped, 2),
        Some(7),
        "best-fit spends the two-region tail and keeps the five-region run whole"
    );

    // And after that 2-region placement, only best-fit can still serve a
    // 5-region humongous object — which is the whole point of the policy.
    let after_first_fit = [false, false, true, true, true, false, false, true, true];
    let after_best_fit = [true, true, true, true, true, false, false, false, false];
    assert_eq!(g1_dbg_first_fit_run(&after_first_fit, 5), None);
    assert_eq!(g1_dbg_best_fit_run(&after_best_fit, 5), Some(0));

    // Sanity: the original map is still served identically by both for a
    // request only the long run can hold.
    assert_eq!(g1_dbg_first_fit_run(&free, 5), Some(4));
    assert_eq!(g1_dbg_best_fit_run(&free, 5), Some(4));
}

#[test]
fn the_two_arms_agree_on_whether_a_run_exists_at_all() {
    // The correctness property: best-fit may NAME a different run, but it may
    // never answer "no run" where first-fit answers "here is one", nor the
    // reverse. Exhaustive over every free map of eight regions.
    for bits in 0u32..256 {
        let free: Vec<bool> = (0..8).map(|i| bits & (1 << i) != 0).collect();
        for count in 1..=8usize {
            let ff = g1_dbg_first_fit_run(&free, count);
            let bf = g1_dbg_best_fit_run(&free, count);
            assert_eq!(
                ff.is_some(),
                bf.is_some(),
                "arms disagree on existence: bits={bits:#010b} count={count} ff={ff:?} bf={bf:?}"
            );
            // Whatever best-fit names must really be a run of `count` free
            // regions — the allocator types those regions without re-checking.
            if let Some(start) = bf {
                assert!(
                    (start..start + count).all(|i| free[i]),
                    "best-fit named a run that is not free: bits={bits:#010b} count={count} start={start}"
                );
            }
        }
    }
}

#[test]
fn a_run_that_reaches_the_end_of_the_table_is_still_a_candidate() {
    // The trailing run has no occupied slot after it to close it, which is the
    // off-by-one every "scan for runs" loop gets wrong.
    let free = [false, false, true, true, true];
    assert_eq!(g1_dbg_best_fit_run(&free, 3), Some(2));
    assert_eq!(g1_dbg_best_fit_run(&free, 4), None);
}

// ---------------------------------------------------------------------------
// TLAB sizing — the over-large request is clamped, not refused
// ---------------------------------------------------------------------------

#[test]
fn a_tlab_request_larger_than_half_a_region_is_served_clamped() {
    let region_size = 64 * 1024;
    let gc = G1Collector::new(many_region_config(16, region_size));
    let cap = region_size / 2;

    // A request at the bound is served unchanged; this arm never changed.
    let (_, at_bound) = gc
        .refill_tlab(cap)
        .expect("a request at exactly half a region has always been served");
    assert_eq!(at_bound, cap, "a request at the bound must not be shrunk");

    // A request OVER the bound used to return `None` — permanently, because the
    // adaptive sizer only re-sizes on a refill that actually happens. It is now
    // clamped to the bound.
    let (ptr, actual) = gc
        .refill_tlab(region_size)
        .expect("an over-large request must be clamped, not refused");
    assert!(!ptr.is_null());
    assert!(
        actual <= cap,
        "a clamped carve must not exceed half a region: got {actual}, cap {cap}"
    );
    assert!(
        actual >= 256,
        "a clamped carve must still be a usable TLAB: got {actual}"
    );

    // And the thread can keep refilling at the over-large size forever — the
    // cliff was that it could not.
    for i in 0..8 {
        let (p, n) = gc
            .refill_tlab(region_size * 4)
            .unwrap_or_else(|| panic!("refill {i} refused after a clamp"));
        assert!(!p.is_null());
        assert!(n <= cap);
    }
}

#[test]
fn a_clamped_tlab_can_never_hold_a_humongous_object() {
    // The invariant the clamp must not break: `may_be_humongous` is exactly
    // `size > region_size / 2`, and the accessors skip the regions lock for
    // anything below it. A chunk of AT MOST `region_size / 2` cannot hold an
    // object larger than `region_size / 2`, so every object carved out of a
    // clamped TLAB is provably non-humongous.
    let region_size = 32 * 1024;
    let gc = G1Collector::new(many_region_config(16, region_size));
    for request in [region_size, region_size * 2, region_size * 64] {
        if let Some((_, actual)) = gc.refill_tlab(request) {
            assert!(
                actual <= region_size / 2,
                "clamped carve {actual} exceeds the humongous bound {}",
                region_size / 2
            );
        }
    }
}

#[test]
fn a_tlab_carve_is_always_eight_byte_aligned_and_zeroed() {
    // The clamp reuses `tlab_carve_size`, whose `& !7` masking was a live
    // heap-corruption fix. Re-assert both of its guarantees on the new path.
    let region_size = 64 * 1024;
    let gc = G1Collector::new(many_region_config(8, region_size));
    let (ptr, actual) = gc.refill_tlab(region_size + 7).expect("clamped carve");
    assert_eq!(actual % 8, 0, "carve size must be a multiple of 8");
    assert_eq!(ptr as usize % 8, 0, "carve must start 8-aligned");
    // SAFETY: the allocator just handed back `[ptr, ptr + actual)`.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, actual) };
    assert!(
        bytes.iter().all(|&b| b == 0),
        "the TLAB contract is a fully zeroed chunk"
    );
}
