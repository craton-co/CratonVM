// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Lane-D regression tests: the generational ZGC soundness properties whose
//! failure mode is a **live object freed with no error anywhere**.
//!
//! # Why a separate file
//!
//! `gc/tests/zgc_module_integration.rs` tests *seams* — two modules disagreeing
//! about a type or a numbering. Everything here is a different shape of bug:
//! each module is internally consistent, each seam type-checks, and the defect
//! is that an obligation **nobody discharges** falls between them. Those do not
//! show up as a compile error or as a disagreement; they show up as a young
//! collection that frees something.
//!
//! Every test below pins one such obligation. A failure is not "these two
//! modules drifted", it is "the remembered set no longer protects the edge it
//! is supposed to protect", so they are named for the corruption they prevent.
//!
//! # No wall-clock assertions
//!
//! Same rule as the integration suite: every assertion is on a count, a
//! classification or set membership. A fixed timing bound is a CI flake on a
//! loaded shared host and this tree has already recorded that.

#![cfg(feature = "zgc")]

use std::sync::Arc;

use cratonvm_gc::zgc::census::{
    ZCensusHeapView, ZCensusObjectKind, ZSlotCensus, ZSlotObservation, ZSlotShape, VALUE_TAG_OBJECT,
};
use cratonvm_gc::zgc::generation::{
    self, ZGenerationId, ZGenerationMarker, ZGenerationScope, ZMarkReport,
};
use cratonvm_gc::zgc::page;
use cratonvm_gc::zgc::remembered::{
    ZGenerationContext, ZPageIdNamespace, ZRangeGenerationContext, ZRememberedSet,
    ZRememberedSetTable, ZStoreBarrier, ZStoreBarrierOutcome, Z_REMSET_GRAIN_BYTES,
};
use cratonvm_types::ClassId;

// ===========================================================================
// Fixtures
// ===========================================================================

/// The 1/512-scale geometry `page.rs`'s own tests use, copied for the same
/// reason `zgc_module_integration.rs` copies it: a test helper in `src` would
/// be a `pub` surface nothing in production needs.
fn small_geometry() -> page::ZPageConfig {
    page::ZPageConfig {
        granule_size: 4096,
        small_page_size: 8192,
        medium_page_size: 65536,
        small_object_limit: 8192 / 8,
        medium_object_limit: 65536 / 8,
        max_capacity: 4096 * 64,
    }
}

fn small_allocator() -> Arc<page::ZPageAllocator> {
    Arc::new(
        page::ZPageAllocator::new(small_geometry()).expect("scaled test geometry must validate"),
    )
}

fn generational_heap(
    allocator: Arc<page::ZPageAllocator>,
    promotion_age: u32,
) -> generation::ZGenerationalHeap {
    let config = generation::ZGenerationalConfig {
        promotion: generation::ZPromotionPolicy::with_age(promotion_age),
        ..generation::ZGenerationalConfig::default()
    };
    generation::ZGenerationalHeap::new(allocator, config)
}

/// A marker that reports every in-scope page fully live, so nothing is freed
/// and every survivor ages. The promotion tests want survivors, not garbage.
struct MarkEverythingLive;

impl ZGenerationMarker for MarkEverythingLive {
    fn mark_from_roots(
        &self,
        _id: ZGenerationId,
        _roots: &[u64],
        scope: &ZGenerationScope,
    ) -> ZMarkReport {
        let mut report = ZMarkReport::new();
        for entry in scope.entries() {
            let extent = entry.end.saturating_sub(entry.start);
            if extent > 0 {
                report.record(entry.page_id, extent);
                report.objects_marked += 1;
            }
        }
        report
    }
}

// ===========================================================================
// 1. The remembered set forgets on the SECOND cycle, not the first
// ===========================================================================

const PAGE_ID: u64 = 7;
const PAGE_BASE: u64 = 0x4000_0000;
const PAGE_SIZE: usize = 4096;

/// PREVENTS: a long-lived old→young reference being freed on the second young
/// cycle after the store that created it.
///
/// This is the failure the read-only scan produces and it is *scheduled*, not
/// racy — which is why it survived review. The bit is set once by the store
/// barrier; cycle 1's swap makes it the snapshot and it is scanned; cycle 2's
/// swap **wipes** it, because nothing wrote it again and nothing renewed it.
/// From cycle 2 onward the edge does not exist and the young object it points
/// at is unreachable from any root the young cycle can see.
#[test]
fn a_plain_snapshot_scan_loses_a_still_live_edge_on_the_second_cycle() {
    let set = ZRememberedSet::new(PAGE_ID, PAGE_SIZE);
    let slot = 512usize;
    assert!(set.remember(slot), "an in-range offset must be recorded");

    // Cycle 1: the collector sees the edge.
    set.swap();
    let mut seen: Vec<usize> = Vec::new();
    set.iterate_snapshot(|off| seen.push(off));
    assert_eq!(seen, vec![slot], "cycle 1 must see the edge");

    // Cycle 2: no mutator store happened in between — a long-lived reference is
    // stored once and never again — and the plain scan wrote nothing back.
    set.swap();
    let mut seen: Vec<usize> = Vec::new();
    set.iterate_snapshot(|off| seen.push(off));
    assert!(
        seen.is_empty(),
        "documenting the hazard: the plain scan does NOT carry the edge \
         forward, so cycle 2 sees nothing. If this ever starts failing, the \
         swap semantics changed and the retaining scan below may be redundant."
    );
}

/// PREVENTS: the same use-after-free, by pinning the repair.
///
/// `iterate_snapshot_retaining` re-sets the bit in the buffer that *becomes*
/// the next snapshot, for every slot the caller says still points into young —
/// which is what OpenJDK's `ZRemembered::scan_field` does and what this module
/// had no way to express.
#[test]
fn the_retaining_scan_carries_a_still_young_edge_across_every_cycle() {
    let set = ZRememberedSet::new(PAGE_ID, PAGE_SIZE);
    let slot = 512usize;
    assert!(set.remember(slot));

    for cycle in 1..=4u32 {
        set.swap();
        let mut seen: Vec<usize> = Vec::new();
        let retained = set.iterate_snapshot_retaining(|off| {
            seen.push(off);
            // "Yes, this slot still holds a young reference."
            true
        });
        assert_eq!(seen, vec![slot], "cycle {cycle} must still see the edge");
        assert_eq!(retained, 1, "cycle {cycle} must carry it forward");
    }

    assert_eq!(
        set.stats().bits_retained,
        4,
        "bits_retained is the engagement counter: zero with several cycles and \
         a stable edge means nobody is calling the retaining scan"
    );
}

/// PREVENTS: an over-approximating set that never shrinks.
///
/// The retaining scan must drop an edge whose target died, was promoted, or was
/// overwritten with null — otherwise the set accumulates every reference the
/// program has ever made and the "sparse page" argument that makes a minor GC
/// cheap stops holding.
#[test]
fn the_retaining_scan_drops_an_edge_whose_target_is_no_longer_young() {
    let set = ZRememberedSet::new(PAGE_ID, PAGE_SIZE);
    assert!(set.remember(512));
    assert!(set.remember(1024));

    set.swap();
    // Only the first slot still points into young.
    let retained = set.iterate_snapshot_retaining(|off| off == 512);
    assert_eq!(retained, 1);

    set.swap();
    let mut seen: Vec<usize> = Vec::new();
    set.iterate_snapshot(|off| seen.push(off));
    assert_eq!(seen, vec![512], "the dead edge must not survive");
}

// ===========================================================================
// 2. An edge that cannot be attributed must not be dropped
// ===========================================================================

/// A context that calls a whole range old but can name no page inside it.
///
/// This models the real hazard: a generation context and a page allocator that
/// disagree about what is old. `is_old` says yes, `page_of` says `None`, and
/// there is no page id to coarsen.
struct OldButUnmappable;

impl ZGenerationContext for OldButUnmappable {
    fn is_old(&self, addr: u64) -> bool {
        addr >= PAGE_BASE
    }
    fn is_young(&self, addr: u64) -> bool {
        addr > 0 && addr < PAGE_BASE
    }
    fn page_of(&self, _addr: u64) -> Option<(u64, usize)> {
        None
    }
}

/// PREVENTS: a silently dropped old→young edge — the one case where the
/// table's own "never *silently drop* an edge" promise used to be false.
///
/// The branch returned `Coarsened` while recording nothing anywhere, because
/// coarsening needs a page id and this branch has none. The repair is the
/// honest over-approximation: coarsen the whole old generation, so a full scan
/// finds the edge wherever it lives.
#[test]
fn an_old_address_that_maps_to_no_page_coarsens_the_whole_old_generation() {
    let table = Arc::new(ZRememberedSetTable::new());
    table.register_old_page(PAGE_ID, PAGE_SIZE);
    table.register_old_page(PAGE_ID + 1, PAGE_SIZE);
    let barrier = ZStoreBarrier::new(Arc::clone(&table));

    assert!(!table.coarsen_all_pending());
    let outcome = barrier.on_reference_store(&OldButUnmappable, PAGE_BASE + 64, 0x1000);
    assert_eq!(outcome, ZStoreBarrierOutcome::Coarsened);
    assert!(
        table.coarsen_all_pending(),
        "an unattributable edge must leave a latch behind, not a log line"
    );

    let mut coarse = table.take_coarse_pages();
    coarse.sort_unstable();
    assert_eq!(
        coarse,
        vec![PAGE_ID, PAGE_ID + 1],
        "every registered old page must be scanned, because the edge could be \
         in any of them"
    );
    assert!(
        !table.coarsen_all_pending(),
        "the latch is consumed by the scan it forced"
    );
}

/// PREVENTS: the same drop on the promotion path, where it is worse — it is not
/// one edge but every outgoing reference of a just-promoted object, and this
/// call is the only record any of them will ever have.
#[test]
fn a_promotion_into_an_unmappable_address_coarsens_rather_than_losing_the_object() {
    let table = Arc::new(ZRememberedSetTable::new());
    table.register_old_page(PAGE_ID, PAGE_SIZE);
    let barrier = ZStoreBarrier::new(Arc::clone(&table));

    let remembered = barrier.on_promote(
        &OldButUnmappable,
        PAGE_BASE + 4096,
        &[(0, 0x1000), (8, 0x2000)],
    );
    assert_eq!(remembered, 0, "nothing could be recorded precisely");
    assert!(
        table.coarsen_all_pending(),
        "and therefore everything must be scanned"
    );
}

// ===========================================================================
// 3. A page id is not a unique name for a generation of objects
// ===========================================================================

/// PREVENTS: a recycled page inheriting the previous occupant's remembered set.
///
/// `ZPageAllocator` caches freed Small/Medium pages and hands them back
/// carrying their original id, so `register_old_page`'s idempotence — right for
/// the promotion path — is wrong for the recycle path. The bits of the page
/// that is gone name slot offsets of objects that no longer exist; every one of
/// them is a wild read for whatever consumes the scan.
#[test]
fn recycling_a_page_id_does_not_inherit_the_previous_pages_bits() {
    let table = ZRememberedSetTable::new();
    let set = table.register_old_page_with_base(PAGE_ID, PAGE_BASE, PAGE_SIZE);
    set.remember(512);
    set.remember(1024);
    assert_eq!(set.bits_set(), 2);

    // Plain re-registration is idempotent and KEEPS the bits: that is the
    // promotion path, and wiping there would drop live edges.
    let again = table.register_old_page_with_base(PAGE_ID, PAGE_BASE, PAGE_SIZE);
    assert_eq!(again.bits_set(), 2, "re-registration must not wipe");

    // The recycle path says so explicitly and starts clean.
    let fresh = table.recycle_old_page(PAGE_ID, PAGE_BASE, PAGE_SIZE);
    assert_eq!(
        fresh.bits_set(),
        0,
        "a recycled id must not carry the previous page's edges"
    );
}

/// PREVENTS: the same id silently describing two different addresses.
///
/// A re-registration at a different base is a recycle that skipped `remove`.
/// Keeping the bits would hand the collector offsets of objects that are gone,
/// so the set is cleared and the base corrected.
#[test]
fn re_registering_a_page_id_at_a_different_base_clears_the_stale_bitmap() {
    let table = ZRememberedSetTable::new();
    let set = table.register_old_page_with_base(PAGE_ID, PAGE_BASE, PAGE_SIZE);
    set.remember(512);
    assert_eq!(set.bits_set(), 1);

    let moved = table.register_old_page_with_base(PAGE_ID, PAGE_BASE + 0x1_0000, PAGE_SIZE);
    assert_eq!(moved.bits_set(), 0);
    assert_eq!(moved.base_address(), Some(PAGE_BASE + 0x1_0000));
}

// ===========================================================================
// 4. A bitmap offset is not a root until something adds the base
// ===========================================================================

/// PREVENTS: the consumer-side `page_id -> base` map whose missing entry is a
/// silently dropped root.
///
/// The base now lives on the set, so `iterate_slot_addresses` yields machine
/// addresses directly — and a page registered without a base is *counted*
/// rather than quietly skipped.
#[test]
fn slot_addresses_come_from_the_set_and_a_baseless_page_is_counted_not_hidden() {
    let table = ZRememberedSetTable::new();
    let with_base = table.register_old_page_with_base(PAGE_ID, PAGE_BASE, PAGE_SIZE);
    let without_base = table.register_old_page(PAGE_ID + 1, PAGE_SIZE);
    assert_eq!(with_base.base_address(), Some(PAGE_BASE));
    assert_eq!(without_base.base_address(), None);

    with_base.remember(512);
    without_base.remember(512);
    assert_eq!(
        without_base.slot_address(512),
        None,
        "a set with no base must refuse rather than return 0 + offset"
    );
    table.swap_all();

    let mut roots: Vec<u64> = Vec::new();
    let (visited, retained, baseless) = table.iterate_slot_addresses(true, &mut |addr| {
        roots.push(addr);
        true
    });
    assert_eq!(roots, vec![PAGE_BASE + 512]);
    assert_eq!(visited, 1);
    assert_eq!(
        retained, 1,
        "the retaining form must carry the edge forward"
    );
    assert_eq!(
        baseless, 1,
        "the page with no base must be reported, because its edges are NOT \
         roots this cycle"
    );
}

/// PREVENTS: a grain-aligned offset being reported as something other than the
/// slot it names, once the base is added.
#[test]
fn a_slot_address_is_the_page_base_plus_the_grain_aligned_offset() {
    let set = ZRememberedSet::new_with_base(PAGE_ID, PAGE_BASE, PAGE_SIZE);
    // Any byte inside a grain names the grain.
    assert!(set.remember(1027));
    let mut seen: Vec<u64> = Vec::new();
    set.iterate(|off| seen.push(set.slot_address(off).expect("base is known")));
    assert_eq!(seen, vec![PAGE_BASE + 1024]);
    assert_eq!(1024 % Z_REMSET_GRAIN_BYTES, 0);
    assert_eq!(
        set.slot_address(PAGE_SIZE),
        None,
        "an offset at the page end is outside the page"
    );
}

/// PREVENTS: the N7 namespace defect reappearing through the new entry points.
#[test]
fn register_old_page_with_base_still_declares_the_allocator_namespace() {
    let table = ZRememberedSetTable::new();
    table.register_old_page_with_base(PAGE_ID, PAGE_BASE, PAGE_SIZE);
    assert_eq!(table.page_id_namespace(), Some(ZPageIdNamespace::Allocator));

    let barrier = ZStoreBarrier::new(Arc::new(ZRememberedSetTable::new()));
    let ctx = ZRangeGenerationContext::new(0x1000, 0x1000, PAGE_BASE, 0x1000, PAGE_SIZE);
    assert!(
        barrier.context_namespace_matches(&ctx),
        "an empty table is compatible with any context"
    );
}

// ===========================================================================
// 5. Promotion has to be nameable, or its obligation has no caller
// ===========================================================================

/// PREVENTS: the promotion → remembered-set obligation being undischargeable.
///
/// A promoted page's objects hold references that were young→young a moment ago
/// — filtered by the store barrier, correctly — and are old→young now. Only the
/// remembered set can find them, and it does not have them. The driver has to
/// register each promoted page and re-scan it, and it cannot do that from a
/// *count*. This test pins that the ids are published.
#[test]
fn a_minor_cycle_names_the_pages_it_promoted_not_just_how_many() {
    const PROMOTION_AGE: u32 = 1;
    let allocator = small_allocator();
    let heap = generational_heap(Arc::clone(&allocator), PROMOTION_AGE);

    let addr = heap
        .allocate_young(64, 8)
        .expect("young allocation")
        .address as u64;
    let page_id = allocator
        .page_for(addr as usize)
        .expect("page must resolve")
        .id();

    let report = heap.collect_young(
        &[addr],
        &generation::ZEmptyRememberedSet,
        &MarkEverythingLive,
    );
    assert_eq!(report.pages_promoted, 1);
    assert_eq!(
        report.promoted_page_ids,
        vec![page_id],
        "the report must NAME the promoted page: the driver has to register it \
         on the remembered-set table and re-scan its fields, and it cannot \
         iterate a count"
    );
    assert_eq!(
        heap.page_generation(page_id),
        Some(generation::ZGeneration::Old),
    );
    // Nothing died, so nothing is freed — and the list must say so rather than
    // being left at whatever the previous cycle put there.
    assert!(report.freed_page_ids.is_empty());
}

/// PREVENTS: a freed page's remembered set outliving the objects it describes.
///
/// The allocator hands a freed Small page straight back under its original id,
/// so the driver must `remove` each freed id. It needs the list to do that.
#[test]
fn a_cycle_names_the_pages_it_freed_so_their_remembered_sets_can_be_dropped() {
    let allocator = small_allocator();
    let heap = generational_heap(Arc::clone(&allocator), 3);

    let addr = heap
        .allocate_young(64, 8)
        .expect("young allocation")
        .address as u64;
    let page_id = allocator
        .page_for(addr as usize)
        .expect("page must resolve")
        .id();

    // No roots and a marker that finds nothing: the page is wholly dead.
    struct MarkNothing;
    impl ZGenerationMarker for MarkNothing {
        fn mark_from_roots(
            &self,
            _id: ZGenerationId,
            _roots: &[u64],
            _scope: &ZGenerationScope,
        ) -> ZMarkReport {
            ZMarkReport::new()
        }
    }

    let report = heap.collect_young(&[], &generation::ZEmptyRememberedSet, &MarkNothing);
    assert_eq!(report.pages_freed, 1);
    assert_eq!(report.freed_page_ids, vec![page_id]);
    assert!(report.promoted_page_ids.is_empty());
    assert_eq!(heap.page_generation(page_id), None);
}

/// PREVENTS: the remembered-entry count reading high.
///
/// It used to be `max(view.entry_count(), entries_yielded)`. Those answer
/// different questions — `entry_count` is an approximate *table* size whose
/// default impl is `0` — so the maximum was neither, and it was biased in the
/// direction that makes a generational run look more engaged than it was.
#[test]
fn the_remembered_entry_count_is_what_the_view_yielded_not_a_table_size() {
    struct OverstatingView(Vec<u64>);
    impl generation::ZRememberedSetView for OverstatingView {
        fn iterate_old_to_young(&self, f: &mut dyn FnMut(u64)) {
            for t in self.0.iter() {
                f(*t);
            }
        }
        fn entry_count(&self) -> usize {
            // A bitmap-backed view would answer with every bit set across every
            // old page, including ones this cycle never reached.
            9_999
        }
    }

    let allocator = small_allocator();
    let heap = generational_heap(Arc::clone(&allocator), 8);
    let addr = heap
        .allocate_young(64, 8)
        .expect("young allocation")
        .address as u64;

    let view = OverstatingView(vec![addr, addr + 64]);
    let report = heap.collect_young(&[addr], &view, &MarkEverythingLive);
    assert_eq!(
        report.remembered_entries, 2,
        "the report must carry what the view actually yielded this cycle"
    );
}

// ===========================================================================
// 6. The census's machine-readable row must not answer a different question
// ===========================================================================

/// A view holding one long-lived legacy object and one short-lived compact one.
struct TwoShapeView {
    include_compact: bool,
}

impl ZCensusHeapView for TwoShapeView {
    fn for_each_live_object(&self, f: &mut dyn FnMut(u64, ClassId, ZCensusObjectKind)) {
        f(0x1000, ClassId::new(1), ZCensusObjectKind::LegacyInstance);
        if self.include_compact {
            f(0x2000, ClassId::new(2), ZCensusObjectKind::CompactInstance);
        }
    }

    fn is_compact(&self, addr: u64) -> bool {
        addr == 0x2000
    }

    fn reference_slots(&self, addr: u64, f: &mut dyn FnMut(ZSlotObservation)) {
        if addr == 0x1000 {
            f(ZSlotObservation::tagged(
                ZSlotShape::LegacyField,
                0x1008,
                1,
                VALUE_TAG_OBJECT,
            ));
        } else if addr == 0x2000 {
            f(ZSlotObservation::bare(ZSlotShape::CompactField, 0x2008, 1));
        }
    }
}

/// PREVENTS: the study's decision being taken off a lifetime-weighted number.
///
/// The cumulative `walk_*` columns re-count a long-lived object on **every**
/// collection, while short-lived objects are counted once each and then die —
/// so they read high on legacy, which is the direction that pushes the verdict
/// toward `LegacyDominant` and re-schedules real work off an artefact of the
/// walk count. The gauge is the most recent walk alone.
#[test]
fn the_census_publishes_a_live_set_gauge_beside_the_lifetime_weighted_sum() {
    let census = ZSlotCensus::new();
    census.enable();

    // Walk 1: one legacy object, one compact object — 50% legacy right now.
    census
        .run_walk(&TwoShapeView {
            include_compact: true,
        })
        .expect("enabled census must walk");
    // Walk 2: the compact object died; the legacy one survived. The live set is
    // now 100% legacy, and it is the same legacy object counted a second time.
    census
        .run_walk(&TwoShapeView {
            include_compact: false,
        })
        .expect("enabled census must walk");

    let gauge = census.last_walk_totals().expect("a walk has completed");
    assert_eq!(gauge.total_slots(), 1, "the gauge is one walk, not two");
    assert!(
        (gauge.legacy_share() - 1.0).abs() < 1e-9,
        "the live set is entirely legacy: {}",
        gauge.legacy_share(),
    );

    let cumulative = census.walk_totals();
    assert_eq!(cumulative.total_slots(), 3, "2 legacy + 1 compact");
    assert!(
        cumulative.legacy_share() < gauge.legacy_share(),
        "the two numbers answer different questions and must be reported \
         separately: cumulative {} vs gauge {}",
        cumulative.legacy_share(),
        gauge.legacy_share(),
    );

    // And the row carries both, under names that say which is which.
    let names: Vec<String> = ZSlotCensus::tsv_column_names();
    let row = census.to_tsv_row();
    let values: Vec<&str> = row.split('\t').collect();
    assert_eq!(values.len(), names.len(), "header and row must not drift");
    let idx = |want: &str| {
        names
            .iter()
            .position(|n| n == want)
            .unwrap_or_else(|| panic!("missing column {want}"))
    };
    assert_eq!(values[idx("last_walk_ran")], "1");
    assert_eq!(values[idx("last_walk_total_ref_slots")], "1");
    assert_eq!(values[idx("walk_total_ref_slots")], "3");
    assert_eq!(
        values[idx("last_walk_verdict")],
        "legacy_dominant",
        "the gauge's own verdict, not the cumulative one"
    );
}

/// PREVENTS: `last_walk_ran=0` being confused with a measurement of zero.
#[test]
fn a_census_with_no_walk_emits_a_placeholder_row_not_a_row_of_zeroes() {
    let census = ZSlotCensus::new();
    census.enable();
    assert!(census.last_walk_totals().is_none());

    let names = ZSlotCensus::tsv_column_names();
    let row = census.to_tsv_row();
    let values: Vec<&str> = row.split('\t').collect();
    assert_eq!(values.len(), names.len());
    let ran = names
        .iter()
        .position(|n| n == "last_walk_ran")
        .expect("last_walk_ran column");
    assert_eq!(values[ran], "0");

    let summary = census.format_summary();
    assert!(
        summary.contains("last_walk: NONE"),
        "the summary must say no walk ran: {summary}"
    );
}

/// PREVENTS: a real statics measurement being printed as `NOT WIRED`.
///
/// `set_statics_wired` is an out-of-band promise; the walk is the evidence. A
/// view that produced static slots IS wired, whatever anybody remembered to
/// declare, and printing `NOT WIRED` over real numbers is the same "silence is
/// not zero" failure the flag exists to prevent, pointed the other way.
#[test]
fn a_walk_that_reaches_statics_marks_the_column_wired_on_its_own() {
    struct StaticsView;
    impl ZCensusHeapView for StaticsView {
        fn for_each_live_object(&self, _f: &mut dyn FnMut(u64, ClassId, ZCensusObjectKind)) {}
        fn is_compact(&self, _addr: u64) -> bool {
            false
        }
        fn reference_slots(&self, _addr: u64, _f: &mut dyn FnMut(ZSlotObservation)) {}
        fn static_reference_slots(&self, f: &mut dyn FnMut(ClassId, ZSlotObservation)) {
            f(
                ClassId::new(9),
                ZSlotObservation::tagged(ZSlotShape::StaticField, 0x3008, 1, VALUE_TAG_OBJECT),
            );
        }
    }

    let census = ZSlotCensus::new();
    census.enable();
    assert!(!census.statics_wired());

    let walk = census.run_walk(&StaticsView).expect("walk");
    assert_eq!(walk.statics_blocks, 1);
    assert_eq!(census.statics_blocks(), 1);
    assert!(
        census.statics_wired(),
        "the walk reached statics, so the column is wired whether or not \
         anybody declared it"
    );
    let summary = census.format_summary();
    assert!(
        !summary.contains("NOT WIRED"),
        "a real measurement must not print NOT WIRED: {summary}"
    );
}
