use super::*;

const OVER_LEN: usize = ZgcRealHeap::MAX_PLAUSIBLE_ARRAY_LEN + 4096;

/// Garbage and two linked survivors in the low region, plus a live
/// over-long `byte[]` (large-object end). Returns the heap, the ascending
/// live set, the parent and the big array.
fn fixture(screen_on: bool) -> (ZgcRealHeap, Vec<usize>, ObjectRef, ObjectRef) {
    let heap = ZgcRealHeap::with_capacity(320 << 20);
    heap.set_tlab_enabled(false);
    heap.counters
        .rewrite_oversize_arrays
        .store(screen_on, Ordering::Relaxed);
    // Garbage below, so the survivors have somewhere to slide.
    for _ in 0..2048 {
        heap.alloc_object(ClassId::new(1), 4);
    }
    let parent = heap.alloc_object(ClassId::new(1), 1);
    let child = heap.alloc_object(ClassId::new(1), 0);
    heap.set_field(parent, 0, Value::Object(Some(child)));
    let big = heap
        .try_alloc_array(ClassId::new(0), ArrayElementType::Byte, OVER_LEN)
        .expect("room for one over-long byte array");
    let mut live: Vec<usize> = [parent, child, big]
        .iter()
        .map(|r| r.as_ptr() as usize)
        .collect();
    live.sort_unstable();
    (heap, live, parent, big)
}

/// The defect and its positive control in one: on the kill arm the
/// rewrite pass reports the real array unwalkable (the probe's guard
/// line); on the default arm it is sized by the oversize screen and
/// nothing is skipped. The slide must have moved something on both arms,
/// or the rewrite never ran and the zero is vacuous.
#[test]
fn an_over_long_live_array_is_walkable_on_the_default_arm_only() {
    for on in [false, true] {
        let (heap, live, _parent, big) = fixture(on);
        // SAFETY: a fresh registered base; only its header is read.
        let header = unsafe { &*(big.as_ptr() as *const ObjectHeader) };
        assert!(
            ZgcRealHeap::alloc_size(header).is_none(),
            "precondition: alloc_size's element cap refuses the real array"
        );
        let (moved, _reclaimed, _map) = heap.relocate_stw_for_test(&live);
        assert!(moved > 0, "the slide must move something (arm on={on})");
        if on {
            assert_eq!(heap.rewrite_unwalkable_reports(), 0, "nothing skipped");
            assert!(
                heap.oversize_survivors_sized() + heap.rewrite_oversize_arrays_walked() > 0,
                "engagement"
            );
        } else {
            assert_eq!(
                heap.rewrite_unwalkable_reports(),
                1,
                "kill arm: the pre-fix skip (positive control)"
            );
            assert_eq!(heap.rewrite_oversize_arrays_walked(), 0);
            assert_eq!(heap.oversize_survivors_sized(), 0);
        }
        assert!(heap.registry.contains(big.as_ptr() as usize), "never moved");
    }
}

/// The screen's refusals: the switch off, an unregistered base, and a
/// registered base inside the claimed extent (a clobbered length that
/// swallows a neighbour). A short array is not its business.
#[test]
fn the_oversize_screen_refuses_a_swallowed_neighbour_and_unregistered_bases() {
    let (heap, _live, parent, big) = fixture(true);
    let b = big.as_ptr() as usize;
    // SAFETY: as above.
    let header = unsafe { &*(b as *const ObjectHeader) };
    let size = heap
        .oversize_live_array_extent(b, header)
        .expect("the real array is sized");
    assert_eq!(size, ARRAY_DATA_OFFSET + OVER_LEN);
    // A forged start inside the extent: refused, then restored.
    let inside = b + 4096;
    heap.registry.insert(inside);
    assert_eq!(heap.oversize_live_array_extent(b, header), None);
    heap.registry.remove(inside);
    assert_eq!(heap.oversize_live_array_extent(b, header), Some(size));
    // Unregistered base: refused, then restored.
    heap.registry.remove(b);
    assert_eq!(heap.oversize_live_array_extent(b, header), None);
    heap.registry.insert(b);
    // Not an over-long array.
    // SAFETY: a live registered object allocated by this test.
    let ph = unsafe { &*(parent.as_ptr() as *const ObjectHeader) };
    assert_eq!(
        heap.oversize_live_array_extent(parent.as_ptr() as usize, ph),
        None
    );
    // Kill switch.
    heap.counters
        .rewrite_oversize_arrays
        .store(false, Ordering::Relaxed);
    assert_eq!(heap.oversize_live_array_extent(b, header), None);
}

/// An over-long array in the LOW region (where `alloc_raw` puts a large
/// object when the high end refuses it), garbage below and above it,
/// survivors above. Unsized (kill arm) the slide moves NOTHING on this
/// fixture -- the unsizable survivor starves compaction of the whole
/// region (measured: moved=0 vs 256). Sized, it is an obstacle that stays
/// put and everything around it compacts, with no byte of its payload and
/// no survivor payload changed. Returns (moved, pattern bytes clobbered,
/// survivors whose payload is wrong).
fn low_region_arm(on: bool) -> (usize, usize, usize) {
    const PAGE: usize = ZgcRealHeap::Z_LOGICAL_PAGE_BYTES;
    let heap = ZgcRealHeap::with_capacity(400 << 20);
    heap.set_tlab_enabled(false);
    heap.counters
        .rewrite_oversize_arrays
        .store(on, Ordering::Relaxed);
    let garbage = |bytes: usize| {
        for _ in 0..bytes / 80 {
            heap.alloc_object(ClassId::new(1), 4);
        }
    };
    garbage(3 * PAGE);
    let size = ARRAY_DATA_OFFSET + OVER_LEN;
    let ptr = heap.arena.lock().alloc(size, 8).expect("a low span");
    let b = ptr as usize;
    assert!(
        b < heap.arena_base + (heap.arena_end - heap.arena_base) / 2,
        "low end"
    );
    // SAFETY: a fresh, zeroed, unpublished span of `size` bytes; the
    // header goes in before the registry insert, as `alloc_raw` does.
    unsafe {
        std::ptr::write(
            ptr as *mut ObjectHeader,
            ObjectHeader::new(
                ClassId::new(0),
                ObjectKind::Array,
                ArrayElementType::Byte,
                OVER_LEN as u32,
                OVER_LEN as u32,
            ),
        );
        let mut off = 0;
        while off < OVER_LEN {
            *ptr.add(ARRAY_DATA_OFFSET + off) = 0x5A;
            off += 64 * 1024;
        }
    }
    heap.registry.insert(b);
    // Above it: garbage interleaved with survivors that carry a payload.
    let mut survivors = Vec::new();
    for i in 0..256usize {
        garbage(16 * 1024);
        let o = heap.alloc_object(ClassId::new(1), 2);
        heap.set_field(o, 0, Value::Long(0x1234_0000 + i as i64));
        survivors.push(o.as_ptr() as usize);
    }
    let mut live = survivors.clone();
    live.push(b);
    live.sort_unstable();
    let (moved, _reclaimed, map) = heap.relocate_stw_for_test(&live);
    eprintln!(
        "zs1-dg low-region arm on={on}: moved={moved} sized={} unwalkable={}",
        heap.oversize_survivors_sized(),
        heap.rewrite_unwalkable_reports()
    );
    let mut clobbered = 0usize;
    let mut off = 0;
    while off < OVER_LEN {
        // SAFETY: the array is live and never moves (checked below).
        if unsafe { *ptr.add(ARRAY_DATA_OFFSET + off) } != 0x5A {
            clobbered += 1;
        }
        off += 64 * 1024;
    }
    let mut wrong = 0usize;
    for (i, old) in survivors.iter().enumerate() {
        let now = map.get(old).copied().unwrap_or(*old);
        // SAFETY: a survivor's post-slide base.
        let o = unsafe { ObjectRef::from_raw(now as *mut u8) };
        if heap.get_field(o, 0) != Value::Long(0x1234_0000 + i as i64) {
            wrong += 1;
        }
    }
    assert!(map.get(&b).is_none(), "the over-long array never moves");
    (moved, clobbered, wrong)
}

#[test]
fn a_low_region_over_long_array_is_an_obstacle_not_free_pages() {
    let (moved, clobbered, wrong) = low_region_arm(true);
    assert!(
        moved > 0,
        "default arm: the region compacts around the array"
    );
    assert_eq!(
        (clobbered, wrong),
        (0, 0),
        "default arm: nothing overwritten"
    );
    // Positive control: the kill arm's unsizable survivor starves the
    // slide (and corrupts nothing on this fixture either).
    let (moved, clobbered, wrong) = low_region_arm(false);
    assert_eq!(moved, 0, "kill arm: compaction starved");
    assert_eq!((clobbered, wrong), (0, 0));
}

/// A reference array past the element cap: `reference_slots_with` used to
/// omit every element (silently, at `debug`), so the rewrite left a slot
/// naming a moved object's OLD address. Needs a 2+ GiB arena, hence
/// ignored by default: `cargo test -p cratonvm-gc --lib
/// zs1_dg_oversize_rewrite_tests -- --ignored`.
#[test]
#[ignore = "allocates a 2 GiB reference array"]
fn an_over_long_reference_array_has_its_elements_rewritten() {
    let heap = ZgcRealHeap::with_capacity(2600 << 20);
    heap.set_tlab_enabled(false);
    for _ in 0..2048 {
        heap.alloc_object(ClassId::new(1), 4);
    }
    let target = heap.alloc_object(ClassId::new(1), 0);
    let arr = heap
        .try_alloc_array(ClassId::new(2), ArrayElementType::Reference, OVER_LEN)
        .expect("room for one over-long Object[]");
    let last = OVER_LEN - 1;
    heap.set_array_element(arr, last, Value::Object(Some(target)))
        .expect("in bounds");
    let old = target.as_ptr() as usize;
    let mut live = vec![old, arr.as_ptr() as usize];
    live.sort_unstable();
    let (moved, _, map) = heap.relocate_stw_for_test(&live);
    assert!(moved > 0, "the target must slide");
    let new = *map.get(&old).expect("the target moved");
    match heap.get_array_element(arr, last) {
        Ok(Value::Object(Some(o))) => {
            assert_eq!(
                o.as_ptr() as usize,
                new,
                "the element names the destination"
            );
        }
        other => panic!("element lost: {other:?}"),
    }
    assert_eq!(heap.oversize_ref_arrays_omitted(), 0);
}

struct NoMonitorsRv;
impl MonitorCleanup for NoMonitorsRv {
    fn remap_after_gc(&self, _: &cratonvm_types::PointerMap) {}
}

/// zs s2/rv (dg residual rows 2 and 4): a full collection with a live
/// over-long `byte[]` counts it as a survivor, not as an "unsizable"
/// retention, and `walk_objects` reports it. Positive control: the kill
/// arm (`rewrite_oversize_arrays = false`, the pre-fix sizing) still
/// counts it unsizable and omits it from the walk -- so the default arm's
/// zero is not vacuous (the sweep did visit the array).
#[test]
fn a_live_over_long_array_is_a_swept_survivor_and_a_walked_row() {
    for on in [false, true] {
        let (heap, _live, parent, big) = fixture(on);
        let mut roots = [parent, big];
        // SAFETY: these unit tests run the heap single-threaded.
        let stw = unsafe { StopTheWorldToken::new() };
        let _ = heap.collect_garbage(&stw, &mut roots, &NoMonitorsRv);
        let b = roots[1].as_ptr() as usize;
        assert_eq!(b, big.as_ptr() as usize, "never moved (arm on={on})");
        let walked = heap
            .walk_objects()
            .iter()
            .any(|(p, sz)| *p as usize == b && *sz > OVER_LEN);
        if on {
            assert_eq!(heap.unsizable_registered_objects(), 0, "a survivor");
            assert!(walked, "walk_objects reports the array");
        } else {
            assert!(
                heap.unsizable_registered_objects() > 0,
                "kill arm: the pre-fix unsizable retention (positive control)"
            );
            assert!(!walked, "kill arm: the pre-fix omission");
        }
        assert!(heap.registry.contains(b), "retained on both arms");
    }
}
