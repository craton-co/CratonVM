// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Wave-3 lane B — why a workload that is nothing but old-to-young stores can
//! still report `rset_sources=1`, and how to tell that from a broken barrier.
//!
//! # What this file is evidence for
//!
//! `tools/probes/G1RsetWideProbe.java` was written in wave 2 to produce a
//! workload with thousands of remembered-set SOURCE regions, so that the
//! Phase-2 seed partition (`CRATONVM_G1_PARALLEL_SEED`) would have something
//! to partition. On 2,730 promoted holders each holding a live old-to-young
//! edge it reported `rset_sources=1`, and that number has two possible causes
//! with opposite responses:
//!
//! * **entries never created** — a store that should reach
//!   `post_write_barrier_rset` does not, or reaches it and is not recorded.
//!   That is a use-after-free waiting for some other over-retention (the
//!   Phase-4 walk, a conservative root, the JIT pin set) to be narrowed;
//! * **entries created and then filtered** — the generation-stamp staleness
//!   test, the card screen or `live_rset_sources` is dropping them. A design
//!   question, and possibly a correct answer.
//!
//! It was neither. The heap really did have one source region, because a
//! COPYING collector does not preserve the address spread a probe builds at
//! allocation time. The probe padded each small holder with a SEPARATE 48 KiB
//! byte array; the first evacuation copied objects in closure order, which
//! copies every holder back to back while scanning the root array and only
//! then reaches any padding — so 2,730 holders totalling ~109 KiB were
//! re-packed into one region, and the padding, being a `byte[]`, has no
//! reference slots and can never be a remembered-set source at all.
//!
//! [`a_separately_padded_holder_set_collapses_to_one_source_region`] is that
//! mechanism, pinned. [`a_holder_that_is_its_own_padding_stays_spread`] is the
//! shape the probe was reshaped to, and the two together are what makes
//! `rset_sources=1` a readable number rather than an alarming one.
//!
//! # Why the pair, rather than one test of the fixed shape
//!
//! A test of the fixed shape alone would pass just as happily if the collector
//! started coarsening every remembered set into one source, which is the
//! failure the probe's number originally looked like. The control arm is the
//! point: the same holder count, the same live bytes, the same number of
//! old-to-young STORES, differing only in whether the bytes that spread the
//! holders belong to the holder. If the source count did not move between
//! them, the source count is not measuring region spread and neither test
//! means anything.

use cratonvm_gc::collector::{GarbageCollector, MonitorCleanup};
use cratonvm_gc::g1::G1CollectorConfig;
use cratonvm_gc::region::RegionType;
use cratonvm_gc::G1Collector;
use cratonvm_types::{ArrayElementType, ClassId, ObjectRef, Value};

struct NoopMonitors;
impl MonitorCleanup for NoopMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

/// 1 MiB regions, 64 MiB heap — the same geometry as the wave-2 source-walk
/// fixture, so the two are comparable.
///
/// `promotion_age: 2` IS LOAD-BEARING, for the reason
/// `gc/tests/g1_w2a_walk_common/mod.rs` spells out at length: the collector
/// default is 15, a fixture that runs a handful of pauses at that default
/// tenures nothing, every holder stays in a Survivor region, every Survivor
/// region is in the next pause's collection set — and a holder inside the
/// collection set needs no remembered-set entry at all, because the pause
/// evacuates it and walks its slots directly. A fixture written that way
/// reports `rset_sources` near zero on BOTH arms and proves nothing. The
/// `Old > 0` assertion in [`build`] is the tripwire.
fn config() -> G1CollectorConfig {
    G1CollectorConfig {
        heap_size: 64 * 1024 * 1024,
        initial_heap_size: 64 * 1024 * 1024,
        region_size: 1024 * 1024,
        promotion_age: 2,
        ..Default::default()
    }
}

/// Bytes of padding per holder. 48 KiB is the probe's own figure: roughly 21
/// holders per 1 MiB region, and comfortably under the humongous threshold
/// (half a region) so the holders are evacuated like ordinary objects rather
/// than allocated straight into Old.
const PAD_BYTES: usize = 48 * 1024;
const PAD_SLOTS: usize = PAD_BYTES / 8;

/// Enough holders to cover many regions once they are spread: 240 x 48 KiB is
/// about 11 MiB, i.e. ~11 regions, against the single region the collapsed arm
/// produces. Small enough that the whole fixture fits a 64 MiB heap with room
/// for the garbage each pause needs.
const HOLDERS: usize = 240;

/// Which arm of the A/B this is. The NULL ARM is `SeparatePad`: same holder
/// count, same live bytes, same stores, and no reason for the source count to
/// differ except the one thing under test.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Padding {
    /// The original probe shape — a small holder with a reference to a big
    /// `byte[]` that is supposed to push the next holder away.
    SeparatePad,
    /// The reshaped probe — the holder IS a 48 KiB reference array, element 0
    /// its young slot.
    HolderIsPad,
}

/// Slot 0 of a `SeparatePad` holder is the young reference; slot 1 is the pad.
const YOUNG: usize = 0;
const PAD: usize = 1;

/// Build the retained set, tenure it, store one fresh young object into every
/// holder, run a young pause, and return
/// `(rset_sources_the_pause_scanned, old_regions)`.
///
/// Every holder ends up with a live reference to a young object allocated after
/// the holder was promoted, so on both arms there are exactly `HOLDERS`
/// old-to-young edges. Only their REGION SPREAD differs.
fn build(padding: Padding) -> (u32, usize) {
    let gc = G1Collector::new(config());

    let root = gc.alloc_object(ClassId::new(42), HOLDERS);
    let mut holders: Vec<ObjectRef> = Vec::with_capacity(HOLDERS);
    for h in 0..HOLDERS {
        let holder = match padding {
            Padding::SeparatePad => {
                // Two slots: the young reference and the pad reference. The pad
                // is a BYTE array, exactly as the probe had it — a primitive
                // array has no reference slots, so however many regions it
                // covers, none of them can ever be a remembered-set source.
                let o = gc.alloc_object(ClassId::new(41), 2);
                let pad = gc.alloc_array(ClassId::new(43), ArrayElementType::Byte, PAD_BYTES);
                gc.set_field(o, PAD, Value::Object(Some(pad)));
                o
            }
            // One 48 KiB reference array. Element 0 is the young slot; the rest
            // stay null and are the padding. The bytes that spread this holder
            // across the heap are the holder's own bytes, so evacuation moves
            // them together and the spread survives the copy.
            Padding::HolderIsPad => {
                gc.alloc_array(ClassId::new(44), ArrayElementType::Reference, PAD_SLOTS)
            }
        };
        gc.set_field(root, h, Value::Object(Some(holder)));
        holders.push(holder);
    }

    // Age the retained set into Old. `promotion_age: 2`, so three pauses is
    // comfortably enough; the assertion below is what actually decides it.
    let mut roots = vec![root];
    for _ in 0..4 {
        gc.young_collection(&mut roots, &NoopMonitors);
    }
    let root = roots[0];
    for (h, holder) in holders.iter_mut().enumerate() {
        let Value::Object(Some(cur)) = gc.get_field(root, h) else {
            panic!("holder {h} was lost while ageing");
        };
        *holder = cur;
    }
    let old_regions = gc.count_regions(RegionType::Old);
    assert!(
        old_regions > 0,
        "{padding:?}: no holder tenured, so every holder is still inside the \
         collection set and needs no remembered-set entry — this fixture would \
         report a source count of ~0 on BOTH arms and decide nothing. Check \
         config()'s promotion_age against the collector default."
    );

    // THE LOAD-BEARING STORE. One fresh young object into every holder, after
    // promotion, so each is a genuine old-to-young cross-region store running
    // the mutator's `post_write_barrier_rset`.
    for (h, &holder) in holders.iter().enumerate() {
        let young = gc.alloc_object(ClassId::new(45), 2);
        gc.set_field(young, 0, Value::Int(1_000_000 + h as i32));
        match padding {
            Padding::SeparatePad => gc.set_field(holder, YOUNG, Value::Object(Some(young))),
            Padding::HolderIsPad => gc
                .set_array_element(holder, 0, Value::Object(Some(young)))
                .expect("young slot store"),
        }
    }
    // Garbage, so the pause has a real Eden to reclaim and a non-trivial CSet.
    for _ in 0..4000 {
        let junk = gc.alloc_object(ClassId::new(46), 2);
        gc.set_field(junk, 0, Value::Int(0));
    }

    let mut roots = vec![root];
    gc.young_collection(&mut roots, &NoopMonitors);

    let facts = cratonvm_gc::gc_metrics::last_g1_cycle()
        .expect("a young pause just ran, so the G1 cycle census must exist");

    // Read every young object back through its holder. A source region the
    // pause did not scan loses the EDGE rather than the holder: the holder
    // survives with a slot naming memory Phase 5 freed. This is the check that
    // makes a low source count trustworthy — a count that is low because the
    // pause skipped sources would fail here.
    let root = roots[0];
    for h in 0..HOLDERS {
        let Value::Object(Some(holder)) = gc.get_field(root, h) else {
            panic!("{padding:?}: holder {h} was lost by the pause");
        };
        let young = match padding {
            Padding::SeparatePad => gc.get_field(holder, YOUNG),
            Padding::HolderIsPad => gc.get_array_element(holder, 0).expect("young slot read"),
        };
        let Value::Object(Some(young)) = young else {
            panic!(
                "{padding:?}: holder {h} lost the young object reachable only \
                 through its remembered-set edge"
            );
        };
        assert_eq!(
            gc.get_field(young, 0).as_int(),
            Some(1_000_000 + h as i32),
            "{padding:?}: holder {h}'s young object was not evacuated intact"
        );
    }

    // Printed so `cargo test -- --nocapture` reports how many times the thing
    // being measured actually happened, rather than only whether a threshold
    // held. The wave's measurement rules ask for that number explicitly.
    eprintln!(
        "[w3b] {padding:?}: holders={HOLDERS} old_regions={old_regions} \
         rset_sources={} cset_young={}",
        facts.rset_sources_scanned, facts.cset_young
    );
    (facts.rset_sources_scanned, old_regions)
}

/// The control arm, and the explanation of the probe's `rset_sources=1`.
///
/// Every one of the `HOLDERS` old-to-young edges is recorded — the read-back in
/// [`build`] proves it, because none of those young objects is reachable any
/// other way — and they nevertheless come out of a handful of source regions,
/// because evacuation re-packed the holders. So `rset_sources=1` on the
/// original probe was the CORRECT answer to the question that heap posed, and
/// nothing is filtering anything.
#[test]
fn a_separately_padded_holder_set_collapses_to_one_source_region() {
    let (sources, old_regions) = build(Padding::SeparatePad);
    assert!(
        old_regions >= 4,
        "the padding should still cover several Old regions ({old_regions}) — \
         if it does not, this arm is not the shape whose collapse it documents"
    );
    assert!(
        sources <= 3,
        "a holder set padded by SEPARATE arrays should re-pack into one or two \
         regions when evacuated, so the pause should see ~1 remembered-set \
         source, not {sources}. A larger number means the collector stopped \
         re-packing by closure order, which makes the reshaped probe \
         unnecessary and this file's explanation of it wrong."
    );
}

/// The treatment arm: the same holders, the same edges, the padding moved
/// INSIDE the holder.
#[test]
fn a_holder_that_is_its_own_padding_stays_spread() {
    let (sources, old_regions) = build(Padding::HolderIsPad);
    assert!(
        old_regions >= 4,
        "the holders should cover several Old regions, not {old_regions}"
    );
    assert!(
        sources >= 4,
        "with the padding inside the holder, the {HOLDERS} holders cover \
         several Old regions and every one of them holds a young edge, so the \
         pause should scan several remembered-set sources, not {sources}. This \
         is the assertion that would have caught a filter emptying the \
         remembered set — the control arm above cannot, because its low count \
         is expected."
    );
}

/// The A/B stated as one comparison, which is the form the wave's measurement
/// rules ask for: the null arm and the treatment arm in the same test, with the
/// count of how many times the thing being measured actually happened.
///
/// Both arms store exactly `HOLDERS` old-to-young references. If the source
/// count does not move between them, it is not measuring what this file says it
/// measures, and both tests above are decorative.
#[test]
fn the_source_count_moves_only_because_of_where_the_padding_lives() {
    let (collapsed, _) = build(Padding::SeparatePad);
    let (spread, _) = build(Padding::HolderIsPad);
    assert!(
        spread > collapsed * 2,
        "same holder count ({HOLDERS}), same live bytes, same number of \
         old-to-young stores; only the padding moved. Sources went \
         {collapsed} -> {spread}, which is not a difference — so the source \
         count is not a measure of region spread and the reshape of \
         tools/probes/G1RsetWideProbe.java rests on nothing."
    );
}
