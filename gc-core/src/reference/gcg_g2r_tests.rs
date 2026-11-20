use super::*;

/// d3n: the batch merges the spans of several slices (overlapping,
/// adjacent, out of order, empty) and prunes them in ONE walk, with the
/// same verdict the per-slice prunes would have reached: the dead rows in
/// any freed span go, a row between spans stays, and a row registered
/// after the sweep's snapshot on a freed block stays.
#[test]
fn gcg_g2r_batched_sweep_prune_matches_the_per_slice_prunes() {
    let build = || {
        let mut p = ReferenceProcessor::new_with_policy(1000);
        p.discover_reference(ReferenceType::Weak, 0x1000, 0x9000, None);
        p.discover_reference(ReferenceType::Soft, 0x1040, 0x9040, None);
        p.discover_reference(ReferenceType::Phantom, 0x2000, 0x9080, Some(0xa000));
        p.discover_reference(ReferenceType::Weak, 0x3000, 0x90c0, None);
        p.discover_reference(ReferenceType::Finalizer, 0x1080, 0x1080, None);
        let snap = p.registration_seq();
        // A new `Reference` on a block slice 1 freed, after the snapshot.
        p.discover_reference(ReferenceType::Weak, 0x1000, 0x9100, None);
        (p, snap)
    };
    let slices: [&[(usize, usize)]; 3] = [
        &[(0x1000, 0x40)],
        // Adjacent to slice 1 (a coalesce absorbed it) and an empty span.
        &[(0x1000, 0x80), (0x1fff, 0)],
        &[(0x2000, 0x20)],
    ];

    let (mut per_slice, snap) = build();
    let mut per_slice_dropped = 0;
    for s in slices {
        let mut sorted = s.to_vec();
        coalesce_spans(&mut sorted);
        per_slice_dropped += per_slice
            .remove_reference_objects_registered_before(snap, &|a| span_contains(&sorted, a));
    }

    let (mut batched, snap2) = build();
    let mut batch = SweptReferenceRowBatch::new(snap2);
    for s in slices {
        batch.add_slice(s);
    }
    assert_eq!(batch.pending_spans(), 3, "the empty span is not held");
    assert_eq!(batch.flush(&mut batched), per_slice_dropped);
    assert_eq!(
        per_slice_dropped, 3,
        "weak 0x1000, soft 0x1040, phantom 0x2000"
    );
    assert_eq!(batch.flushes(), 1, "one walk for three slices");
    assert_eq!(batch.rows_dropped(), 3);
    assert_eq!(batch.pending_spans(), 0);

    let rows = |p: &ReferenceProcessor| {
        let mut v: Vec<(usize, usize)> = p
            .soft_refs
            .iter()
            .chain(p.weak_refs.iter())
            .chain(p.phantom_refs.iter())
            .map(|e| (e.reference_obj, e.referent))
            .collect();
        v.sort_unstable();
        v
    };
    assert_eq!(rows(&batched), rows(&per_slice));
    assert_eq!(rows(&batched), vec![(0x1000, 0x9100), (0x3000, 0x90c0)]);
    assert_eq!(
        batched.finalizer_refs.len(),
        1,
        "finalizer rows are never pruned here"
    );
}

/// d3n: an empty batch neither walks nor counts; `wants_flush` asks only
/// when something is held and a pause is due or the cap is reached.
#[test]
fn gcg_g2r_batch_flushes_only_at_a_pause_the_cap_or_the_end() {
    let mut p = ReferenceProcessor::new_with_policy(1000);
    p.discover_reference(ReferenceType::Weak, 0x1000, 0x9000, None);
    let mut batch = SweptReferenceRowBatch::new(p.registration_seq());
    assert!(!batch.wants_flush(true), "nothing held");
    assert_eq!(batch.flush(&mut p), 0);
    assert_eq!(batch.flushes(), 0, "an empty flush is not a walk");

    batch.add_slice(&[(0x5000, 0x10)]);
    assert!(!batch.wants_flush(false), "no pause, under the cap");
    assert!(batch.wants_flush(true), "a pause is about to be joined");

    let mut full = SweptReferenceRowBatch::new(0);
    let spans: Vec<(usize, usize)> = (0..SWEPT_ROW_BATCH_MAX_SPANS)
        .map(|i| (0x10_0000 + i * 0x20, 0x10))
        .collect();
    full.add_slice(&spans[..SWEPT_ROW_BATCH_MAX_SPANS - 1]);
    assert!(!full.wants_flush(false));
    full.add_slice(&spans[SWEPT_ROW_BATCH_MAX_SPANS - 1..]);
    assert!(full.wants_flush(false), "the cap");
}

/// d3n: the row registered before the snapshot is pruned even though the
/// batch (snapshot taken first) is flushed after more rows arrive.
#[test]
fn gcg_g2r_batch_keeps_rows_registered_between_slices() {
    let mut p = ReferenceProcessor::new_with_policy(1000);
    p.discover_reference(ReferenceType::Weak, 0x1000, 0x9000, None);
    let mut batch = SweptReferenceRowBatch::new(p.registration_seq());
    batch.add_slice(&[(0x1000, 0x40)]);
    // Between slices, no pause: a new `Reference` lands on the freed
    // block (a direct old-gen allocation), and another elsewhere.
    p.discover_reference(ReferenceType::Phantom, 0x1000, 0x9100, Some(0xa000));
    p.discover_reference(ReferenceType::Weak, 0x4000, 0x9200, None);
    batch.add_slice(&[(0x4000, 0x40)]);
    assert_eq!(batch.flush(&mut p), 1, "only the dead weak row");
    assert_eq!(p.weak_ref_count(), 1);
    assert_eq!(p.phantom_ref_count(), 1);
    // The application door finds the NEW row at the shared address.
    assert!(p.retire_by_application(0x1000));
}

/// The span helpers: the boundaries `addr_keyed::in_spans` pins, and the
/// merge of overlapping, adjacent and unsorted spans.
#[test]
fn gcg_g2r_span_helpers() {
    let spans = [(0x1000usize, 0x40usize), (0x2000, 0x10)];
    assert!(span_contains(&spans, 0x1000));
    assert!(span_contains(&spans, 0x103f));
    assert!(!span_contains(&spans, 0x1040));
    assert!(!span_contains(&spans, 0x0fff));
    assert!(!span_contains(&spans, 0x1800));
    assert!(span_contains(&spans, 0x200f));
    assert!(!span_contains(&spans, 0x2010));
    assert!(!span_contains(&[], 0x1000));

    let mut v = vec![
        (0x3000, 0x10),
        (0x1000, 0x40),
        (0x1040, 0x20),
        (0x1010, 0x8),
        (0x5000, 0),
    ];
    coalesce_spans(&mut v);
    assert_eq!(v, vec![(0x1000, 0x60), (0x3000, 0x10)]);
}

/// d3n, the race the sweep's own `stw_requested` test leaves: spans the
/// processor holds are flushed by the pause's first row reader (the
/// pre-collection pass), so the pause never hands out a freed row.
#[test]
fn gcg_g2r_deferred_spans_are_flushed_by_the_pre_collection_pass() {
    let mut p = ReferenceProcessor::new_with_policy(1000);
    p.discover_reference(ReferenceType::Weak, 0x1000, 0x9000, None);
    p.discover_reference(ReferenceType::Weak, 0x3000, 0x9100, None);
    let snap = p.registration_seq();
    p.defer_swept_spans(snap, &[]);
    assert!(
        !p.deferred_sweep_wants_flush(true),
        "an empty slice holds nothing"
    );
    p.defer_swept_spans(snap, &[(0x1000, 0x40)]);
    assert!(!p.deferred_sweep_wants_flush(false));
    assert!(p.deferred_sweep_wants_flush(true));
    assert_eq!(p.weak_ref_count(), 2, "deferral walks nothing");

    let triples = p.weak_phantom_active_triples();
    assert_eq!(
        triples.len(),
        1,
        "the freed row is not handed to the null pass"
    );
    assert_eq!(triples[0].0, 0x3000);
    assert_eq!(p.deferred_sweep_totals(), (1, 1));
    assert!(
        !p.deferred_sweep_wants_flush(true),
        "nothing held after the flush"
    );
    assert_eq!(p.flush_deferred_sweep(), 0);
    assert_eq!(
        p.deferred_sweep_totals(),
        (1, 1),
        "an empty flush is not counted"
    );
}

/// d3n: held spans name PRE-collection addresses, so `update_after_gc`
/// flushes them before it relocates a row -- a live row moved ONTO a
/// freed span (compaction into a hole) is not taken for the dead one.
#[test]
fn gcg_g2r_deferred_spans_are_flushed_before_relocation() {
    let mut p = ReferenceProcessor::new_with_policy(1000);
    p.discover_reference(ReferenceType::Weak, 0x1000, 0x9000, None);
    p.discover_reference(ReferenceType::Phantom, 0x5000, 0x9100, Some(0xa000));
    let snap = p.registration_seq();
    p.defer_swept_spans(snap, &[(0x1000, 0x40)]);
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(0x5000, 0x1000);
    p.update_after_gc(&map);
    assert_eq!(
        p.weak_ref_count(),
        0,
        "the dead weak row went before the move"
    );
    assert_eq!(p.phantom_ref_count(), 1);
    assert_eq!(
        p.phantom_refs[0].reference_obj, 0x1000,
        "the moved phantom is kept"
    );
    assert_eq!(p.deferred_sweep_totals(), (1, 1));
}

/// d3n: every other pause-time reader flushes too, and a batch of an
/// earlier sweep (another snapshot) is flushed before a new sweep's spans
/// join it.
#[test]
fn gcg_g2r_deferred_spans_flush_at_every_pause_reader_and_per_sweep() {
    fn prepass(p: &mut ReferenceProcessor) {
        let _ = p.remove_collected_reference_objects(&|_| true);
    }
    fn process(p: &mut ReferenceProcessor) {
        let _ = p.process_references(&|_| true, 64, 0);
    }
    fn tail(p: &mut ReferenceProcessor) {
        p.remove_collected(&|_| true);
    }
    let readers: [(&str, fn(&mut ReferenceProcessor)); 3] = [
        ("remove_collected_reference_objects", prepass),
        ("process_references", process),
        ("remove_collected", tail),
    ];
    for (name, read) in readers {
        let mut p = ReferenceProcessor::new_with_policy(1000);
        p.discover_reference(ReferenceType::Soft, 0x1000, 0x9000, None);
        p.defer_swept_spans(p.registration_seq(), &[(0x1000, 0x40)]);
        read(&mut p);
        assert_eq!(p.soft_ref_count(), 0, "{name} flushed the held span");
    }

    let mut p = ReferenceProcessor::new_with_policy(1000);
    p.discover_reference(ReferenceType::Weak, 0x1000, 0x9000, None);
    let first = p.registration_seq();
    p.defer_swept_spans(first, &[(0x1000, 0x40)]);
    // The next sweep's snapshot, after a NEW row on the freed block.
    p.discover_reference(ReferenceType::Weak, 0x1000, 0x9100, None);
    let second = p.registration_seq();
    p.defer_swept_spans(second, &[(0x7000, 0x40)]);
    assert_eq!(
        p.deferred_sweep_totals(),
        (1, 1),
        "the first sweep's batch, alone"
    );
    assert_eq!(p.weak_ref_count(), 1);
    assert_eq!(p.weak_refs[0].referent, 0x9100, "the new row survives");
    assert_eq!(p.flush_deferred_sweep(), 0);
    assert_eq!(p.deferred_sweep_totals(), (2, 1));
}
