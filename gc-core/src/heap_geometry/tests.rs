use super::*;

// gce e2/o (`gce-e1x-heap-geometry-envelope-test-flakes-FIXED-20260929.md`): the
// table logic is tested on a LOCAL `HeapSpanTable`. The process-global
// `HEAP_SPANS` is rewritten, all three slots, by every `GenerationalHeap`
// any test in this crate builds (`gen_heap` publishes each arena), with no
// lock a test here could share; the old slot-2 tests read it back between
// their own publish and their assertion and flaked (1 in 4 Windows runs of
// `the_envelope_spans_every_published_slot`), and their `restore` could
// write a stale span over a live heap's. Only the out-of-range test below
// still touches the global, through a sentinel no real heap can hold.

#[test]
fn an_unpublished_slot_reads_as_zero_and_contributes_nothing() {
    let t = HeapSpanTable::new();
    assert_eq!(t.span(2), (0, 0));
    assert_eq!(t.envelope(), None, "an all-zero table has no envelope");
    t.publish(2, 0x1000, 0x2000);
    t.publish(2, 0, 0);
    assert_eq!(t.span(2), (0, 0));
    assert_eq!(t.envelope(), None);
}

#[test]
fn an_inverted_or_empty_span_clears_rather_than_publishing_nonsense() {
    let t = HeapSpanTable::new();
    t.publish(2, 0x9000, 0x1000);
    assert_eq!(t.span(2), (0, 0), "inverted span must not stand");
    t.publish(2, 0x1000, 0x1000);
    assert_eq!(t.span(2), (0, 0), "empty span must not stand");
}

#[test]
fn an_out_of_range_slot_is_declined_rather_than_wrapping_onto_another() {
    // On a local table first: exact, no neighbour can write it.
    let t = HeapSpanTable::new();
    for out_of_range in [HEAP_SPAN_SLOTS, HEAP_SPAN_SLOTS + 1, usize::MAX] {
        t.publish(out_of_range, 0xDEAD_0000, 0xDEAD_1000);
    }
    assert!((0..HEAP_SPAN_SLOTS).all(|s| t.span(s) == (0, 0)));
    assert_eq!(t.span(usize::MAX), (0, 0));
    // Then on the global, through the public functions:
    // The failure this rules out is a write that lands on slot 0 by
    // arithmetic -- `HEAP_SPAN_SLOTS % HEAP_SPAN_SLOTS` is 0 -- and
    // silently redescribes the live young generation.
    //
    // Asserted with a SENTINEL rather than by snapshotting the table before
    // and after, and that is the whole of the 2026-09-10 rewrite. The old
    // form compared `(0..HEAP_SPAN_SLOTS).map(heap_span)` across the two
    // out-of-range publishes -- a test that reads more of the global than
    // it wrote: slots 0..2 belong to whatever heap another test in this
    // process is building (`gen_heap` publishes one per generation, `zgc`
    // publishes slot 0), and no lock here serialises against those.
    //
    // It flaked exactly there under full-workspace load: slots 0 and 1 came
    // back holding EACH OTHER's spans -- one concurrent generational heap
    // build, not a wrap. Sorting the two sides would have made it pass by
    // destroying the slot-to-value binding the assertion exists to check,
    // which is why it is not what this does.
    //
    // A span no real heap can hold answers the actual question -- did the
    // out-of-range write land anywhere in range? -- and a sibling
    // publishing a genuine heap cannot perturb it.
    const SENTINEL: (usize, usize) = (0xDEAD_0000, 0xDEAD_1000);
    for out_of_range in [HEAP_SPAN_SLOTS, HEAP_SPAN_SLOTS + 1, usize::MAX] {
        publish_heap_span(out_of_range, SENTINEL.0, SENTINEL.1);
    }
    for slot in 0..HEAP_SPAN_SLOTS {
        assert_ne!(
            heap_span(slot),
            SENTINEL,
            "an out-of-range publish wrapped onto slot {slot}",
        );
    }
    // The read side declines too, and for the same reason: an out-of-range
    // READ must not fold onto an in-range slot and report its span.
    assert_eq!(heap_span(HEAP_SPAN_SLOTS), (0, 0));
    assert_eq!(heap_span(usize::MAX), (0, 0));
}

/// **Every backend must publish.** A source witness, and it has to be one.
///
/// The defect this guards is not a wrong value, it is a call that does not
/// happen — and that is precisely the failure this whole module exists to
/// undo. `compressed_oops::enable_for_live_heap` answered "no live heap
/// regions published" for G1 and ZGC not because anything was broken but
/// because nobody had written the publish, and the symptom surfaced as a
/// statement about compressed oops rather than about a missing call. No
/// runtime assertion can see a call that was never made.
///
/// It cannot be a behavioural test either: this table is process-global and
/// this crate's tests share one process, so a test that constructed a G1
/// heap and read the table back could have its slot overwritten by a
/// generational heap another test built a microsecond later. The tests
/// above use a local `HeapSpanTable` precisely because of that (gce e2/o).
#[test]
fn every_backend_publishes_its_geometry() {
    for (name, src) in [
        ("gen_heap", concat!(include_str!("../../../gc-gen/src/gen_heap/mod.rs"), "\n", include_str!("../../../gc-gen/src/gen_heap/young_from_guard.rs"), "\n", include_str!("../../../gc-gen/src/gen_heap/geometry_cap_destination.rs"), "\n", include_str!("../../../gc-gen/src/gen_heap/publish_young_commit_bits.rs"), "\n", include_str!("../../../gc-gen/src/gen_heap/set_resolved_compact_primitive_field.rs"), "\n", include_str!("../../../gc-gen/src/gen_heap/set_field_full.rs"), "\n", include_str!("../../../gc-gen/src/gen_heap/non_moving_young_cycle.rs"), "\n", include_str!("../../../gc-gen/src/gen_heap/collect_garbage_inner_with_pins.rs"), "\n", include_str!("../../../gc-gen/src/gen_heap/young_drain_blocked.rs"), "\n", include_str!("../../../gc-gen/src/gen_heap/sweep_young_non_moving.rs"), "\n", include_str!("../../../gc-gen/src/gen_heap/sweep_old_gen_non_moving.rs"), "\n", include_str!("../../../gc-gen/src/gen_heap/mark_young_to_old_refs_deferring.rs"), "\n", include_str!("../../../gc-gen/src/gen_heap/cell_corrupt_diag_enabled.rs"), "\n", include_str!("../../../gc-gen/src/gen_heap/note_rejected_old_mark_candidate.rs"), "\n", include_str!("../../../gc-gen/src/gen_heap/zero_run_verdict.rs"))),
        ("g1", concat!(include_str!("../../../gc-g1/src/g1/mod.rs"), "\n", include_str!("../../../gc-g1/src/g1/value_from_unaligned_ptr.rs"), "\n", include_str!("../../../gc-g1/src/g1/config.rs"), "\n", include_str!("../../../gc-g1/src/g1/needs_gc_recount_due.rs"), "\n", include_str!("../../../gc-g1/src/g1/collector.rs"), "\n", include_str!("../../../gc-g1/src/g1/set_resolved_compact_primitive_field.rs"), "\n", include_str!("../../../gc-g1/src/g1/futile_young_escalation_count.rs"), "\n", include_str!("../../../gc-g1/src/g1/count_unretired_forwards.rs"), "\n", include_str!("../../../gc-g1/src/g1/collect_outgoing_cross_region_edges.rs"), "\n", include_str!("../../../gc-g1/src/g1/mark_root_grid_place.rs"), "\n", include_str!("../../../gc-g1/src/g1/record_collection_with_phases_within.rs"), "\n", include_str!("../../../gc-g1/src/g1/bump_satb_pre_barrier_epoch.rs"), "\n", include_str!("../../../gc-g1/src/g1/evac_copy_fits_a_region.rs"), "\n", include_str!("../../../gc-g1/src/g1/old_plab_bytes_for.rs"))),
                ("zgc", concat!(include_str!("../../../gc-zgc/src/zgc/mod.rs"), "\n", include_str!("../../../gc-zgc/src/zgc/hotspot_cause_for_mask.rs"), "\n", include_str!("../../../gc-zgc/src/zgc/compact_batch_reads_enabled.rs"), "\n", include_str!("../../../gc-zgc/src/zgc/trigger_tallies.rs"), "\n", include_str!("../../../gc-zgc/src/zgc/zgc_page_pinned_relocate.rs"), "\n", include_str!("../../../gc-zgc/src/zgc/relocate_stw_for_test.rs"), "\n", include_str!("../../../gc-zgc/src/zgc/verify_no_dangling_slots_after_slide.rs"), "\n", include_str!("../../../gc-zgc/src/zgc/zgc_relocate_budget_bytes.rs"), "\n", include_str!("../../../gc-zgc/src/zgc/zgc_flag_default_on.rs"), "\n", include_str!("../../../gc-zgc/src/zgc/z_phase_clock.rs"), "\n", include_str!("../../../gc-zgc/src/zgc/try_alloc_object.rs"))),
    ] {
        assert!(
            src.contains("heap_geometry::publish_heap_span("),
            "{name} no longer publishes its heap geometry. Every consumer of \
             `heap_geometry` — the narrow-oop window, a conservative \
             scanner's range prefilter — then reads this backend as though \
             it had no heap, which is the exact shape of the defect this \
             table was added to remove."
        );
    }
}

#[test]
fn the_envelope_spans_every_published_slot() {
    let t = HeapSpanTable::new();
    t.publish(2, 0x4000_0000, 0x5000_0000);
    assert_eq!(t.envelope(), Some((0x4000_0000, 0x5000_0000)));
    // Every slot counts; the gap between two arenas is inside the envelope.
    t.publish(0, 0x1000_0000, 0x1100_0000);
    t.publish(1, 0x6000_0000, 0x6800_0000);
    assert_eq!(t.envelope(), Some((0x1000_0000, 0x6800_0000)));
    // An unpublished slot contributes nothing.
    t.publish(1, 0, 0);
    assert_eq!(t.envelope(), Some((0x1000_0000, 0x5000_0000)));
}
