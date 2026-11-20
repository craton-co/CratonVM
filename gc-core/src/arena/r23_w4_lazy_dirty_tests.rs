use super::*;

/// Spans merge; a hand-out overlapping one is reported and removed (and
/// only the overlap is removed); one that misses is not.
#[test]
fn r23_w4_take_lazy_dirty_reports_and_removes_only_the_overlap() {
    let mut a = Arena::new(64 * 1024);
    a.note_lazy_dirty(&[(4096, 1024), (5120, 1024), (16 * 1024, 512)]);
    assert_eq!(a.zero_sets.lazy_dirty.as_vec(), vec![(4096, 2048), (16 * 1024, 512)]);
    assert_eq!(a.lazy_dirty_end(), 16 * 1024 + 512);
    assert_eq!(a.take_lazy_dirty(0, 4096), None, "touching is not overlapping");
    assert_eq!(a.take_lazy_dirty(5000, 200), Some((5000, 5200)));
    assert_eq!(
        a.zero_sets.lazy_dirty.as_vec(),
        vec![(4096, 904), (5200, 944), (16 * 1024, 512)]
    );
    assert_eq!(
        a.take_lazy_dirty(16 * 1024 - 64, 1 << 20),
        Some((16 * 1024, 16 * 1024 + 512)),
        "a hand-out past it reports the clipped dirty extent"
    );
    assert_eq!(a.lazy_dirty_end(), 5200 + 944);
    assert_eq!(a.take_lazy_dirty(8192, 64), None);
}

/// t1 lane F (`gcj-j2yg2` re-measure): the counted take reports the exact
/// dirty bytes inside the hand-out -- the gaps between spans and the parts
/// outside the hand-out are not counted -- and removes what the plain
/// take removes.
#[test]
fn t1_f_take_lazy_dirty_counted_sums_only_the_clipped_overlap() {
    let mut a = Arena::new(64 * 1024);
    a.note_lazy_dirty(&[(1000, 100), (1200, 100), (1400, 100)]);
    // [1050, 1450): 50 of the first, all 100 of the second, 50 of the third.
    assert_eq!(a.take_lazy_dirty_counted(1050, 400), Some(((1050, 1450), 200)));
    assert_eq!(a.zero_sets.lazy_dirty.as_vec(), vec![(1000, 50), (1450, 50)]);
    assert_eq!(a.take_lazy_dirty_counted(1100, 300), None, "the hole is clean");
    assert_eq!(a.take_lazy_dirty_counted(0, 1 << 20), Some(((1000, 1500), 100)));
    assert_eq!(a.lazy_dirty_end(), 0);
}

/// R-L4z item 3: the B-tree set merges in any insertion order, joins a
/// span that bridges several, and a removal spanning several spans keeps
/// both clipped ends.
#[test]
fn r23_w4_review_lazy_set_merges_and_splits_like_the_sorted_vec() {
    let mut s = LazyDirtySet::default();
    s.insert(100, 200);
    s.insert(300, 400);
    s.insert(500, 600);
    s.insert(200, 300); // touches both neighbours
    assert_eq!(s.as_vec(), vec![(100, 300), (500, 100)]);
    s.insert(50, 700); // swallows everything
    assert_eq!(s.as_vec(), vec![(50, 650)]);
    s.insert(800, 900);
    assert_eq!(s.remove(600, 850), Some((600, 850)));
    assert_eq!(s.as_vec(), vec![(50, 550), (850, 50)]);
    assert_eq!(s.remove(0, 40), None);
    assert_eq!(s.remove(700, 800), None, "a gap");
    assert_eq!(s.overlapping(500, 860), vec![(50, 600), (850, 900)]);
    assert_eq!(s.end(), 900);
}

/// t1 lane F (`t1-F-proposal-young-reset-phase-share-on-windows`):
/// `insert_batch` matches individual `insert` calls for any mixture of
/// overlapping, touching, unsorted, and zero-length spans.
#[test]
fn t1_f_lazy_set_insert_batch_matches_one_by_one() {
    let spans = [
        (500, 100),
        (100, 50),
        (150, 50), // touches (100, 50)
        (300, 0),  // empty span
        (800, 200),
        (900, 150), // overlaps (800, 200) -> 800..1050
        (250, 50),
        (600, 100), // touches (500, 100) -> 500..700
    ];
    let mut individual = LazyDirtySet::default();
    for &(o, l) in &spans {
        if l > 0 {
            individual.insert(o, o + l);
        }
    }
    let mut batch = LazyDirtySet::default();
    batch.insert_batch(&spans);
    assert_eq!(batch.as_vec(), individual.as_vec());
    assert_eq!(batch.end(), individual.end());

    // Also test merging into an already populated set:
    let extra = [(50, 60), (400, 150), (1050, 50)];
    for &(o, l) in &extra {
        if l > 0 {
            individual.insert(o, o + l);
        }
    }
    batch.insert_batch(&extra);
    assert_eq!(batch.as_vec(), individual.as_vec());
    assert_eq!(batch.end(), individual.end());
}

/// R-L4z item 2: the mirror marks exactly the noted bytes; a hand-out, a
/// forget and a reset clear exactly theirs; a suspend/restore round trip
/// leaves the set's bytes marked and the forgotten copies unmarked.
#[test]
fn r23_w4_review_mirror_tracks_the_set_exactly() {
    let mut a = Arena::new(64 * 1024);
    let base = a.base_ptr() as usize;
    assert!(!a.lazy_dirty_contains(base + 4096), "no mirror yet");
    assert!(a.lazy_dirty_mirror().is_none());
    a.note_lazy_dirty(&[(4096, 1024), (8192, 4096)]);
    assert!(a.lazy_dirty_mirror().is_some());
    assert!(a.lazy_dirty_contains(base + 4096));
    assert!(a.lazy_dirty_contains(base + 4096 + 1016));
    assert!(!a.lazy_dirty_contains(base + 4096 + 1024), "end is exclusive");
    assert!(!a.lazy_dirty_contains(base + 4088));
    // A hand-out clears exactly its bytes.
    assert!(a.take_lazy_dirty(4096 + 512, 64).is_some());
    assert!(!a.lazy_dirty_contains(base + 4096 + 512));
    assert!(!a.lazy_dirty_contains(base + 4096 + 568));
    assert!(a.lazy_dirty_contains(base + 4096 + 504));
    assert!(a.lazy_dirty_contains(base + 4096 + 576));
    // An in-place cycle: suspend over its destination, copy, forget the
    // copy, restore.
    a.suspend_lazy_mirror_over(&[(8192, 4096)]);
    assert!(!a.lazy_dirty_contains(base + 9000 - 9000 % 8));
    a.forget_lazy_dirty_over(&[(8192 + 1024, 256)]);
    a.restore_lazy_mirror_over(&[(8192, 4096)]);
    assert!(a.lazy_dirty_contains(base + 8192));
    assert!(!a.lazy_dirty_contains(base + 8192 + 1024), "the live copy");
    assert!(!a.lazy_dirty_contains(base + 8192 + 1272));
    assert!(a.lazy_dirty_contains(base + 8192 + 1280));
    assert!(a.lazy_dirty_contains(base + 4096), "outside the destination, untouched");
    // A reset clears every bit.
    a.reset();
    assert!(!a.lazy_dirty_contains(base + 4096));
    assert!(!a.lazy_dirty_contains(base + 8192));
    assert!(!a.lazy_dirty_contains(base + 12280));
}

/// gcj yg3-residuals: forgetting touching survivors as one run leaves the
/// set and the mirror exactly as forgetting each survivor alone does.
#[test]
fn gcj_yg3r_forgetting_touching_spans_as_a_run_matches_one_by_one() {
    let dirty = [(4096, 1024), (8192, 4096), (16384, 512)];
    // Survivors: three touching ones straddling a dirty edge, a gap, two
    // touching ones inside a span, and one over clean bytes.
    let kept = [
        (4096 + 896, 64),
        (4096 + 960, 64),
        (4096 + 1024, 64),
        (8192 + 64, 32),
        (8192 + 96, 32),
        (12288 + 256, 64),
    ];
    let mut runs = Arena::new(64 * 1024);
    let mut each = Arena::new(64 * 1024);
    runs.note_lazy_dirty(&dirty);
    each.note_lazy_dirty(&dirty);
    runs.forget_lazy_dirty_over(&kept);
    for &k in &kept {
        each.forget_lazy_dirty_over(&[k]);
    }
    assert_eq!(
        runs.zero_sets.lazy_dirty.as_vec(),
        each.zero_sets.lazy_dirty.as_vec()
    );
    let (rb, eb) = (runs.base_ptr() as usize, each.base_ptr() as usize);
    for off in (0..32 * 1024).step_by(8) {
        assert_eq!(
            runs.lazy_dirty_contains(rb + off),
            each.lazy_dirty_contains(eb + off),
            "mirror differs at offset {off}"
        );
    }
    assert!(!runs.lazy_dirty_contains(rb + 4096 + 896), "a survivor");
    assert!(runs.lazy_dirty_contains(rb + 4096 + 888), "dead bytes stay");
}

/// `reset` zeroes up to the set's end even ABOVE the cursor, and drops the
/// set: the bump tail reads zero again.
#[test]
fn r23_w4_reset_zeroes_the_lazy_dirty_tail_above_the_cursor() {
    let mut a = Arena::new(64 * 1024);
    let p = a.alloc(1024, 8).expect("room") as usize;
    let base = a.base_ptr() as usize;
    assert_eq!(p, base);
    // Stale bytes above the cursor, as a pinned in-place cycle leaves them.
    assert!(a.commit_for_relocation(8192, 4096), "commit the tail page");
    // SAFETY: committed arena memory past the cursor, owned by the test.
    unsafe { std::ptr::write_bytes((base + 8192) as *mut u8, 0xAB, 256) };
    a.note_lazy_dirty(&[(8192, 256)]);
    a.reset();
    assert_eq!(a.lazy_dirty_end(), 0, "the set is dropped");
    // SAFETY: as above.
    let tail = unsafe { std::slice::from_raw_parts((base + 8192) as *const u8, 256) };
    assert!(tail.iter().all(|&b| b == 0), "reset zeroed the dirty tail");
}

/// `reset_deferring_zero` hands the dirty tail to the wipe.
#[test]
fn r23_w4_the_deferred_wipe_covers_the_lazy_dirty_tail() {
    let mut a = Arena::new(64 * 1024);
    let _ = a.alloc(1024, 8).expect("room");
    a.note_lazy_dirty(&[(8192, 256)]);
    let wipe = a.reset_deferring_zero();
    assert!(wipe.low_end >= 8192 + 256);
    assert_eq!(a.lazy_dirty_end(), 0);
}
