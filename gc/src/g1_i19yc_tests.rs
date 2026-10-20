// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! gci i19/yc -- unit tests for the parallel evacuator's holder body screen
//! (`G1Collector::parallel_holder_body_refused`, called from
//! `SharedEvac::process_object`) and the oversized-copy guard of
//! `SharedEvac::tlab_alloc` (`evac_copy_fits_a_region`).
//!
//! A child module of `g1` (see the `#[path]` declaration in `g1.rs`), so the
//! private items are reachable through `super`.
//!
//! Filter: `cargo test -j 5 -p cratonvm-gc --lib gci_i19_yc_`.

use super::*;
use crate::collector::GarbageCollector;
use cratonvm_types::{ArrayElementType, ClassId, ObjectHeader, ObjectKind};

const REGION: usize = 1024 * 1024;

fn collector() -> G1Collector {
    G1Collector::new(G1CollectorConfig {
        heap_size: 8 * REGION,
        // The whole heap committed up front, as `small_config` does: these
        // tests read the region table directly.
        initial_heap_size: 8 * REGION,
        region_size: REGION,
        ..G1CollectorConfig::default()
    })
}

/// A legacy holder whose own header sizes it larger than a region is refused
/// by the parallel arm, as the serial arm (`scan_and_evacuate_refs`) and the
/// Phase-4 fix-up (`fixup_walk_region`) already refuse it. Before gci i19/yc
/// the parallel arm clamped the walk to the region end and rewrote every
/// 16-byte cell up to it.
#[test]
fn gci_i19_yc_parallel_holder_screen_refuses_a_legacy_body_larger_than_a_region() {
    let gc = collector();
    // 1 << 17 legacy cells = 2 MiB of body, twice the region.
    let big = ObjectHeader::new(ClassId::new(7), ObjectKind::Object, ArrayElementType::Reference, 0, 1 << 17);
    let small = ObjectHeader::new(ClassId::new(7), ObjectKind::Object, ArrayElementType::Reference, 0, 4);
    let regions = gc.regions.read();
    let view = RegionView::Slice(&regions[..]);
    let declared = gc
        .parallel_holder_body_refused(view, &big as *const ObjectHeader as *const u8, &big)
        .expect("a legacy body larger than a region must be refused");
    assert!(declared > REGION, "declared={declared} must exceed the region");
    assert_eq!(
        gc.parallel_holder_body_refused(view, &small as *const ObjectHeader as *const u8, &small),
        None,
        "an ordinary four-slot holder is walked"
    );
}

/// The humongous exemption: a humongous object legitimately spans more than
/// one region, so its size is no evidence and it is never refused.
#[test]
fn gci_i19_yc_parallel_holder_screen_exempts_a_humongous_object() {
    let gc = collector();
    // 300 000 longs = 2.4 MB, above the half-region humongous threshold.
    let arr = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 300_000);
    let addr = arr.as_ptr() as usize;
    // SAFETY: `arr` is the live array just allocated; only its header is read.
    let header = unsafe { &*(addr as *const ObjectHeader) };
    let regions = gc.regions.read();
    assert!(
        gc.addr_is_in_humongous_region(&regions[..], addr),
        "fixture: the array must be humongous"
    );
    assert!(
        gc.holder_body_cannot_fit_a_region(header).is_some(),
        "fixture: its size alone would be refused"
    );
    assert_eq!(
        gc.parallel_holder_body_refused(RegionView::Slice(&regions[..]), addr as *const u8, header),
        None,
        "a humongous holder must never be refused on size"
    );
}

/// `SharedEvac::tlab_alloc` answers `None` up front for a copy no region can
/// hold, instead of claiming and abandoning every pool region first.
#[test]
fn gci_i19_yc_an_oversized_copy_fits_no_region() {
    assert!(evac_copy_fits_a_region(16, REGION));
    assert!(evac_copy_fits_a_region(REGION, REGION), "exactly a region still fits");
    assert!(!evac_copy_fits_a_region(REGION + 8, REGION));
    assert!(!evac_copy_fits_a_region(usize::MAX, REGION));
    assert!(
        evac_copy_fits_a_region(usize::MAX, 0),
        "an unknown geometry never refuses"
    );
}
