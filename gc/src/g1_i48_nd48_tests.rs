// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! gci nd48 -- unit tests for the TLAB reserve's reclaim credit
//! (`G1Collector::alloc_from_reclaim_credit`, `reserve_credit_admits`), page
//! `gci-i48-g1-native-door-oome-after-ladder-RETIRED-UNFIXED-20261018`.
//!
//! A child module of `g1` (see the `#[path]` declaration in `g1.rs`), so the
//! private items are reachable through `super`.
//!
//! Filter: `cargo test -j 5 -p cratonvm-gc --lib gci_i48_nd48_`.

use super::*;
use cratonvm_types::ClassId;

const MIB: usize = 1024 * 1024;

/// 16 one-MiB regions, all committed: the TLAB reserve is 2 regions, as at
/// `-Xmx64m` (64 regions) under `CRATONVM_G1_TLAB_RESERVE_FLOOR_ONE=0` (the
/// floor is set here; the default floor is one region).
fn collector() -> G1Collector {
    let mut gc = G1Collector::new(G1CollectorConfig {
        heap_size: 16 * MIB,
        initial_heap_size: 16 * MIB,
        region_size: MIB,
        string_dedup_enabled: false,
        ..Default::default()
    });
    gc.tlab_reserve_floor = 2;
    gc
}

/// Fill the top `count` regions as full Eden (no bytes written), leaving the
/// rest Free, and republish the census the claim funnel would have kept.
fn consume(gc: &G1Collector, count: usize) {
    let mut regions = gc.regions.write();
    for r in regions.iter_mut().rev().take(count) {
        r.set_region_type(RegionType::Eden);
        r.set_cursor(MIB);
    }
    gc.publish_region_census(&regions);
}

/// What a young pause did to the table, in miniature: the full Eden region
/// `freed` is reclaimed, and Free region `dest` becomes a `dest_type` region
/// holding `copied` bytes of survivors.
fn pause(gc: &G1Collector, freed: usize, dest: usize, dest_type: RegionType, copied: usize) {
    let mut regions = gc.regions.write();
    assert_eq!(regions[freed].region_type(), RegionType::Eden);
    assert_eq!(regions[dest].region_type(), RegionType::Free);
    regions[freed].set_region_type(RegionType::Free);
    regions[freed].set_cursor(0);
    regions[dest].set_region_type(dest_type);
    regions[dest].set_cursor(copied);
    gc.publish_region_census(&regions);
}

fn alloc(gc: &G1Collector) -> bool {
    gc.try_alloc_object(ClassId::new(1), 1).is_some()
}

/// The pure rule: one claim per half region given back, never the last Free
/// region.
#[test]
fn gci_i48_nd48_the_credit_rule() {
    let half = MIB / 2;
    assert!(reserve_credit_admits(2, half, 0, MIB), "half a region pays one claim");
    assert!(!reserve_credit_admits(2, half - 1, 0, MIB), "less does not");
    assert!(!reserve_credit_admits(1, 8 * MIB, 0, MIB), "never the last Free region");
    assert!(!reserve_credit_admits(2, MIB - 1, 1, MIB), "the second claim needs a region");
    assert!(reserve_credit_admits(2, MIB, 1, MIB));
    assert!(!reserve_credit_admits(0, MIB, 0, MIB));
}

/// The G1 near-full proxy-define page (`CRATONVM_G1_RESERVE_CREDIT_FINE`): a
/// claim costs a sixteenth of a region, still never the last Free region, and
/// nothing reclaimed still earns nothing.
#[test]
fn the_fine_credit_rule_costs_a_sixteenth() {
    let fine = reserve_credit_per_claim(MIB, true);
    assert_eq!(fine, MIB / 16);
    assert_eq!(reserve_credit_per_claim(MIB, false), MIB / 2);
    assert!(reserve_credit_admits_at(2, fine, 0, fine));
    assert!(!reserve_credit_admits_at(2, fine - 1, 0, fine));
    assert!(!reserve_credit_admits_at(2, 0, 0, fine), "a live-filling heap earns nothing");
    assert!(!reserve_credit_admits_at(1, 8 * MIB, 0, fine), "never the last Free region");
    assert!(reserve_credit_admits_at(3, 2 * fine, 1, fine));
}

/// The defect: a heap at the reserve churning garbage. The refusal arms the
/// credit; a pause that reclaims a garbage Eden and claims one Survivor region
/// leaves the pool at the reserve (2 Free), and the retry is now served from
/// it, leaving one Free region. Before nd48 it was refused (`door_oome`).
#[test]
fn gci_i48_nd48_a_reclaimed_eden_pays_for_the_retry() {
    let gc = collector();
    assert_eq!(gc.tlab_reserve_regions(16), 2);
    consume(&gc, 14);
    assert_eq!(gc.count_regions(RegionType::Free), 2);
    assert!(!alloc(&gc), "the first attempt at the reserve is refused");
    assert_eq!(gc.reserve_credit_counts(), (0, 1));

    // Region 15 (top) was Eden garbage; region 0 takes one surviving Box.
    pause(&gc, 15, 0, RegionType::Survivor, 128);
    assert_eq!(gc.count_regions(RegionType::Free), 2);
    assert!(alloc(&gc), "the retry spends the reclaimed region");
    assert_eq!(gc.count_regions(RegionType::Free), 1);
    assert_eq!(gc.reserve_credit_counts(), (1, 1));
    // The claimed Eden now serves the following requests without a claim.
    for _ in 0..64 {
        assert!(alloc(&gc));
    }
    assert_eq!(gc.count_regions(RegionType::Free), 1);
}

/// The reserve's own job (gc-common w3-c): on a heap filling with LIVE data
/// the pause copies what it frees, gives back no bytes, and the retry is
/// refused with the reserve intact.
#[test]
fn gci_i48_nd48_live_growth_earns_no_credit() {
    let gc = collector();
    consume(&gc, 14);
    assert!(!alloc(&gc));
    pause(&gc, 15, 0, RegionType::Old, MIB);
    assert!(!alloc(&gc), "a pause that reclaimed nothing pays for nothing");
    assert_eq!(gc.count_regions(RegionType::Free), 2);
    assert_eq!(gc.reserve_credit_counts(), (0, 2));
}

/// One credit per half region: a second claim after the same pause is
/// refused, and the refusal re-arms the credit at today's occupancy.
#[test]
fn gci_i48_nd48_one_pause_pays_once() {
    let gc = collector();
    consume(&gc, 13);
    // 3 Free: the ordinary arm serves, and leaves 2.
    assert!(alloc(&gc));
    consume(&gc, 14);
    assert_eq!(gc.count_regions(RegionType::Free), 2);
    assert!(!alloc(&gc));
    pause(&gc, 15, 0, RegionType::Survivor, 128);
    assert!(alloc(&gc));
    // Fill the credited Eden so the next request needs another region.
    {
        let mut regions = gc.regions.write();
        for r in regions.iter_mut() {
            if r.region_type() == RegionType::Eden {
                r.set_cursor(MIB);
            }
        }
    }
    assert!(!alloc(&gc), "the credit was spent, and only one Free region is left");
    assert_eq!(gc.reserve_credit_counts(), (1, 2));
}

/// Out of the reserve, the ordinary arm drops a recorded refusal.
#[test]
fn gci_i48_nd48_leaving_the_reserve_disarms() {
    let gc = collector();
    consume(&gc, 14);
    assert!(!alloc(&gc));
    assert_ne!(gc.reserve_credit_used.load(Ordering::Relaxed), RESERVE_CREDIT_UNARMED);
    {
        let mut regions = gc.regions.write();
        regions[15].set_region_type(RegionType::Free);
        regions[15].set_cursor(0);
        gc.publish_region_census(&regions);
    }
    assert_eq!(gc.count_regions(RegionType::Free), 3);
    assert!(alloc(&gc));
    assert_eq!(gc.reserve_credit_used.load(Ordering::Relaxed), RESERVE_CREDIT_UNARMED);
}

/// `CRATONVM_G1_RESERVE_RECLAIM_CREDIT=0` restores the plain refusal.
#[test]
fn gci_i48_nd48_the_switch_kills_the_credit() {
    cratonvm_types::flags::with_thread_overrides(
        &[("CRATONVM_G1_RESERVE_RECLAIM_CREDIT", Some("0"))],
        || {
            let gc = collector();
            consume(&gc, 14);
            assert!(!alloc(&gc));
            pause(&gc, 15, 0, RegionType::Survivor, 128);
            assert!(!alloc(&gc), "=0: the reserve refuses the retry");
            assert_eq!(gc.count_regions(RegionType::Free), 2);
            assert_eq!(gc.reserve_credit_counts(), (0, 0));
        },
    );
}

/// The G1 near-full proxy-define page (`CRATONVM_G1_RESERVE_LAST_CHANCE`): on
/// a heap whose collections reclaim nothing, the ladder's armed last chance
/// takes one reserve region and leaves the last Free region; it is consumed,
/// and never fires with one Free region.
#[test]
fn the_last_chance_takes_one_reserve_region_once() {
    let gc = collector();
    consume(&gc, 14);
    assert!(!alloc(&gc), "the reserve refuses");
    pause(&gc, 15, 0, RegionType::Old, MIB);
    assert!(!alloc(&gc), "a pause that reclaimed nothing pays for nothing");
    gc.arm_reserve_last_chance();
    assert!(alloc(&gc), "the ladder's final attempt is served");
    assert_eq!(gc.count_regions(RegionType::Free), 1);
    assert_eq!(gc.reserve_last_chance_claims(), 1);
    // Consumed: fill the claimed Eden, then the next region claim is refused.
    {
        let mut regions = gc.regions.write();
        for r in regions.iter_mut() {
            if r.region_type() == RegionType::Eden {
                r.set_cursor(MIB);
            }
        }
        gc.publish_region_census(&regions);
    }
    assert!(!alloc(&gc));
    gc.arm_reserve_last_chance();
    assert!(!alloc(&gc), "never the last Free region");
    assert_eq!(gc.count_regions(RegionType::Free), 1);
}
