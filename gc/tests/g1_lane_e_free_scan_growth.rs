// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Lane E (2026-09-20) — filling a heap for the first time must not cost a
//! quadratic number of region probes.
//!
//! # Why this is its own test binary
//!
//! `free_region_scan_counts()` reads PROCESS-GLOBAL counters (`FREE_SCAN_CALLS`
//! and friends in `g1.rs`), so any other test in the same binary that claims a
//! region moves them. Cargo gives each `tests/*.rs` file its own binary, so the
//! single test below owns the counters outright. Adding a second test to this
//! file re-introduces exactly the interference the split exists to avoid.
//!
//! # The finding
//!
//! `claim_free_region_young` searches DOWNWARD from the top of the committed
//! prefix. While the heap is growing, every region inside that prefix is
//! already occupied, so the search probes the whole prefix, fails, and the
//! claim grows the prefix by one region. The next claim probes a prefix one
//! longer. Filling an N-region heap therefore costs Θ(N²) region-type reads
//! under the collector's EXCLUSIVE guard — the same shape `free_scan_hint`
//! was introduced to remove from the upward search, reintroduced in the
//! downward twin because a hint cannot encode "there is nothing below me".
//!
//! The fix asks the maintained free-region census instead: every region at or
//! above the committed prefix is Free by construction, so the Free regions
//! INSIDE the prefix are `free_region_count - (len - committed)`, and a zero
//! there means the scan cannot succeed. See `claim_free_region_young`.

use cratonvm_gc::g1::free_region_scan_counts;
use cratonvm_gc::{G1Collector, G1CollectorConfig};

#[test]
fn filling_a_fresh_heap_does_not_probe_a_quadratic_number_of_regions() {
    const REGIONS: usize = 256;
    const REGION_SIZE: usize = 8 * 1024;

    let gc = G1Collector::new(G1CollectorConfig {
        heap_size: REGIONS * REGION_SIZE,
        // `0` is the lazy-commit ergonomic (a sixteenth of the heap), which is
        // the configuration the finding is about: the prefix starts small and
        // every Eden claim past it has to grow it.
        initial_heap_size: 0,
        region_size: REGION_SIZE,
        ..Default::default()
    });
    assert_eq!(gc.num_regions(), REGIONS);

    let (calls_before, probes_before, _, _, _, _) = free_region_scan_counts();

    // Fill the heap. One object per half region guarantees a fresh Eden claim
    // roughly every two allocations, which is the region-consumption event the
    // search runs on — and it is the ONLY thing this test does, so the counter
    // delta is attributable.
    let obj_size = REGION_SIZE / 2 - 64;
    let mut claimed = 0usize;
    for _ in 0..(REGIONS * 4) {
        if gc.alloc_in_region(obj_size).is_none() {
            break;
        }
        claimed += 1;
    }
    assert!(
        claimed > REGIONS,
        "the workload must actually consume regions: {claimed} allocations over {REGIONS} regions"
    );

    let (calls_after, probes_after, worst, _, _, _) = free_region_scan_counts();
    let calls = calls_after - calls_before;
    let probes = probes_after - probes_before;
    assert!(calls > 0, "the free-region search must have run at all");

    // The bound is on the TOTAL, not on the mean, and the reason is the last
    // scan.
    //
    // Once the committed prefix covers the whole region table there is no
    // growth arm left to fall through to, so `claim_free_region_young`
    // deliberately keeps its exhaustive scan there: a Free-region cache that
    // read low would otherwise turn into a spurious "no free region", i.e. a
    // spurious collection or OOM. Filling this heap therefore ends with
    // exactly one full-table scan that finds nothing, which is a fixed cost
    // rather than a quadratic one — and it dominates a mean taken over the
    // handful of calls the growth phase makes.
    //
    // So the assertion is linear-in-the-region-count with wide slack. A
    // quadratic scan over 256 regions is ~32 900 probes (sum 1..=256); the
    // measured figure after the fix is ~272, of which 256 are that single
    // terminal scan.
    let budget = REGIONS * 4;
    assert!(
        probes as usize <= budget,
        "free-region search is not amortised O(1) while the heap grows: \
         {probes} probes over {calls} calls (worst single scan {worst}), budget {budget}; \
         a quadratic scan over {REGIONS} regions would be ~{}",
        REGIONS * (REGIONS + 1) / 2
    );
    // And no single scan may be worse than the whole table — that is what
    // "exhaustive, wrapping" means, and a regression that scanned twice would
    // show up here rather than in the total.
    assert!(
        worst as usize <= REGIONS,
        "a single free-region scan probed {worst} regions of {REGIONS}"
    );
}
