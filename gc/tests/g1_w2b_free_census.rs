// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Lane B wave 2 (2026-09-20) — a pause that FREES regions has to republish the
//! free/young census, not only a pause that went through `collect_garbage`.
//!
//! # The finding (filed by lane E, fixed in lane B's `free_or_keep_cset`)
//!
//! `G1Collector::free_region_count` is a MAINTAINED cache, not a scan. It is
//! decremented at every Free -> occupied transition by
//! `note_free_regions_claimed`, and re-derived by `publish_region_census`. The
//! increment side did not exist: nothing that FREED a region touched it. The
//! only production republish points were `GarbageCollector::collect_garbage`
//! (via `recount_free_regions`) and `cleanup`, so a caller that drives
//! `young_collection` / `mixed_collection` DIRECTLY — `g1_concurrent.rs` is one
//! such caller in the tree — left the cache reading the pre-pause value until
//! the `NEEDS_GC_RECOUNT_INTERVAL` backstop in `needs_gc` fired.
//!
//! It is not a correctness bug; the backstop bounds the staleness. It matters
//! because every consumer reads a stale-LOW value as "the heap is tight", and
//! in all four that is the pause-storm direction: `needs_gc` keeps answering
//! `true`, `note_region_consumed_locked` re-latches `native_alloc_pressure`,
//! `refill_tlab`'s emergency reserve refuses TLABs, and
//! `mixed_phase_has_work`'s `tight` waiver waives the `heap_waste_percent`
//! floor. A pause storm diagnosed to a cache is the worst kind of pause storm,
//! because nothing about the collector's policy explains it.
//!
//! # Why the assertion is written against `count_regions`
//!
//! The cache's whole contract is "equal to what a scan would say". Asserting
//! `after > before` would pass on a cache that is merely less wrong; asserting
//! equality with `count_regions(RegionType::Free)` is the contract itself, and
//! it is the assertion that FAILS on the pre-2026-09-20 code.

use cratonvm_gc::collector::MonitorCleanup;
use cratonvm_gc::{G1Collector, G1CollectorConfig, GarbageCollector, RegionType};
use cratonvm_types::ClassId;

struct NoMonitors;
impl MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

fn heap() -> G1Collector {
    G1Collector::new(G1CollectorConfig {
        heap_size: 64 * 64 * 1024,
        initial_heap_size: 64 * 64 * 1024,
        region_size: 64 * 1024,
        ..Default::default()
    })
}

/// The measurement behind `CRATONVM_G1_FREE_CENSUS_ON_RECLAIM`'s default-ON.
#[test]
fn w2b_free_census_is_exact_after_a_direct_young_collection() {
    let gc = heap();

    // A live root set small enough that the pause reclaims most of what it
    // collects. The garbage is what makes the pause FREE regions, which is the
    // transition the cache had no increment side for.
    let mut roots: Vec<_> = (0..16)
        .map(|_| gc.alloc_object(ClassId::new(1), 2))
        .collect();
    for _ in 0..4096 {
        // Dropped immediately: unreachable by the time the pause runs, so its
        // regions are freed rather than kept.
        let _garbage = gc.alloc_object(ClassId::new(2), 4);
    }

    let claimed_free = gc.free_region_count();
    let true_free = gc.count_regions(RegionType::Free);
    assert_eq!(
        claimed_free, true_free,
        "the cache must already be exact BEFORE the pause, or this test is \
         measuring the wrong thing"
    );
    assert!(
        true_free < gc.num_regions(),
        "the allocations above must have consumed at least one region"
    );

    // NOT `collect_garbage`: that path calls `recount_free_regions` afterwards
    // and would hide the finding. This is the door `g1_concurrent.rs` uses.
    gc.young_collection(&mut roots, &NoMonitors);

    let after_cache = gc.free_region_count();
    let after_true = gc.count_regions(RegionType::Free);
    assert_eq!(
        after_cache, after_true,
        "a pause that frees regions must leave `free_region_count` equal to a \
         fresh count ({after_true}); it read {after_cache}. Without the \
         `publish_region_census` call in `free_or_keep_cset` this is the \
         PRE-pause value, and every consumer of the cache then reads the heap \
         as tighter than it is."
    );
    assert!(
        after_true > true_free,
        "the pause must actually have freed something ({true_free} -> \
         {after_true}), or the assertion above is vacuous"
    );

    // The young half of the same one-pass census. `free_or_keep_cset` retypes
    // Eden -> Survivor for every kept region, so this counter moves on exactly
    // the pauses the free counter does; publishing only one of the two would
    // leave the other stale for the same reason.
    assert_eq!(
        gc.young_region_count(),
        gc.count_regions(RegionType::Eden) + gc.count_regions(RegionType::Survivor),
        "`young_region_count` is the other half of `publish_region_census` and \
         must be exact at the same moment"
    );

    // Keep the roots alive across the assertions above.
    assert_eq!(roots.len(), 16);
}

/// A second pause must not drift either. The first pause's republish could be
/// accidentally correct (the cache and the truth can coincide once); two
/// pauses with different amounts of garbage cannot both coincide by luck.
#[test]
fn w2b_free_census_stays_exact_across_consecutive_pauses() {
    let gc = heap();
    let mut roots: Vec<_> = (0..8)
        .map(|_| gc.alloc_object(ClassId::new(1), 2))
        .collect();

    for round in 0..3 {
        for _ in 0..(512 * (round + 1)) {
            let _garbage = gc.alloc_object(ClassId::new(2), 4);
        }
        gc.young_collection(&mut roots, &NoMonitors);
        assert_eq!(
            gc.free_region_count(),
            gc.count_regions(RegionType::Free),
            "round {round}: the free census drifted"
        );
    }
    assert_eq!(roots.len(), 8);
}
