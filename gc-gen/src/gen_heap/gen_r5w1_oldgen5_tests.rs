use super::*;

/// `gengc-r4w4-oldgen4-a-walk-gap-hides-old-to-young-roots-from-the-card-scan`:
/// an old object `h` behind a header no walk can size, holding the only
/// reference to a young object, with `h`'s card dirty. The precise scan
/// cannot parse `h`, so it yields no `(holder, slot)` for it; the gap scan
/// must report the young object as a conservative root (and the word's
/// card, to keep dirty). Before this, both were silently absent.
#[test]
fn a_young_object_named_only_from_a_walk_gap_is_a_conservative_root() {
    for bot in [true, false] {
        let heap = GenerationalHeap::with_sizes(4 * 1024 * 1024, 4 * 1024 * 1024);
        heap.old_gen.lock().set_block_offset_table_enabled(bot);
        let y = heap.alloc_object(ClassId::new(0), 0);
        assert!(heap.is_in_young(y.as_ptr()));
        let size = HEADER_SIZE + SLOT_SIZE;
        // `b` then `h`, carved end to end out of ONE old-gen block, so no
        // allocation anchor lies between them: `h` sits in `b`'s gap.
        let (b, h) = {
            let mut og = heap.old_gen.lock();
            let p = og.alloc(2 * size, 8).expect("room");
            // SAFETY: a fresh, zeroed block holding two one-slot objects.
            unsafe {
                (*(p as *mut ObjectHeader)).set_num_slots(1);
                (*(p.add(size) as *mut ObjectHeader)).set_num_slots(1);
                std::ptr::write(
                    p.add(size + HEADER_SIZE) as *mut Value,
                    Value::Object(Some(y)),
                );
                // Not a valid `ObjectKind`: every walk breaks at `b`.
                std::ptr::write(p.add(cratonvm_types::KIND_TAGS_BYTE_OFFSET), 0xFFu8);
            }
            (p as usize, p as usize + size)
        };
        heap.card_table.mark_dirty(h);
        let old_gen = heap.old_gen.lock();
        let young = heap.lock_young_from();
        let mut extra = Vec::new();
        let gap =
            GenerationalHeap::scan_dirty_cards(&heap.card_table, &old_gen, &young, &mut extra);
        assert!(
            extra.iter().all(|&(o, _, _)| o.as_ptr() as usize != h),
            "bot={bot}: `h` cannot be parsed, so it has no precise slot"
        );
        assert!(
            gap.young_words.contains(&(y.as_ptr() as usize)),
            "bot={bot}: the young object must be a conservative root: {gap:?}"
        );
        assert!(
            gap.card_addrs.iter().any(|&a| a > h && a < h + size),
            "bot={bot}: {gap:?}"
        );
        assert_eq!(gap.windows, 1, "bot={bot}");
        assert!(gap.window_bytes >= size, "bot={bot}");
        let _ = b;
        // The table (when on) is still on, and counted the window.
        let s = old_gen.block_offset_stats();
        assert_eq!(s.enabled, bot);
        assert!(s.gap_windows >= 1, "{s:?}");
    }
}

/// `GenR4W4HumongousFragProbe` in miniature: humongous arrays A, B, C back
/// to back in old gen, A and C dead, a request larger than either hole
/// the in-place sweep would leave. The refusal arms a compaction
/// (`OldGen::note_humongous_refusal`), the next major compacts instead of
/// sweeping in place, B slides down with its contents, and the request
/// fits. Before, the major freed A and C in place and the request was
/// refused until `OutOfMemoryError`.
#[test]
fn a_refused_humongous_array_makes_the_next_major_compact() {
    if !gc_flags().old_oom_compact {
        return; // `CRATONVM_GC_OLD_OOM_COMPACT=0`: the ladder's kill switch
    }
    let heap = GenerationalHeap::with_sizes(64 * 1024, 256 * 1024);
    let len = 70 * 1024 / 4; // an int[] of 70 KiB: humongous (> 32 KiB)
    let mk = || {
        heap.try_alloc_array(ClassId::new(0), ArrayElementType::Int, len)
            .expect("room for a 70 KiB array")
    };
    let (a, b, c) = (mk(), mk(), mk());
    assert!(
        [a, b, c].iter().all(|o| heap.is_in_old(o.as_ptr())),
        "humongous: old gen"
    );
    let int_at = |o: usize, i: usize| (o + ARRAY_DATA_OFFSET + i * 4) as *mut i32;
    // SAFETY: `b` is a live int[] of `len` elements.
    unsafe { std::ptr::write(int_at(b.as_ptr() as usize, len - 1), 7) };
    let big = 100 * 1024 / 4; // 100 KiB: fits neither hole A and C leave
    assert!(
        heap.try_alloc_array(ClassId::new(0), ArrayElementType::Int, big)
            .is_none(),
        "precondition: refused (and too large for the 64 KiB nursery)"
    );
    let requests = heap.old_gen_sizing_stats().humongous_compaction_requests;
    assert!(requests >= 1, "the refusal armed a compaction");
    let map = {
        let young = heap.lock_young_from();
        let mut og = heap.old_gen.lock();
        assert!(og.humongous_refusal_pending());
        let mut roots = [b];
        GenerationalHeap::major_gc(&mut roots, &young, &mut og, &[])
    };
    let b_old = b.as_ptr() as usize;
    let b_new = map.get(&b_old).copied().unwrap_or(b_old);
    assert!(b_new < b_old, "B slid down into A's hole");
    // SAFETY: `b_new` is B's post-compaction address.
    assert_eq!(
        unsafe { std::ptr::read(int_at(b_new, len - 1)) },
        7,
        "B's contents moved with it"
    );
    assert!(
        !heap.old_gen.lock().humongous_refusal_pending(),
        "the request was consumed"
    );
    assert!(
        heap.try_alloc_array(ClassId::new(0), ArrayElementType::Int, big)
            .is_some(),
        "after the compaction the request fits"
    );
    let _ = (a, c);
}

/// `gengc-r4w6-tlab6-humongous-reference-arrays-zeroed-under-the-old-gen-lock`:
/// the `long[]` disguise a humongous reference array wears while its body
/// is zeroed outside the lock spans EXACTLY the array's footprint, so every
/// walker strides over the block as it will over the finished array, and
/// it is a primitive array (no reader scans its body as references).
#[test]
fn the_humongous_reference_disguise_has_the_arrays_exact_footprint() {
    for len in [0usize, 1, 2, 3, 4097, 8191, 1 << 20, (1 << 20) + 1] {
        let data = array_data_size(len, ArrayElementType::Reference).unwrap();
        let disguise = humongous_ref_disguise(ClassId::new(7), data).expect("encodable");
        let mut real = ObjectHeader::new(
            ClassId::new(7),
            ObjectKind::Array,
            ArrayElementType::Reference,
            len as u32,
            len as u32,
        );
        real.add_gc_flags(GC_FLAG_OLD_GEN);
        assert_eq!(
            gen_object_total_size(&disguise),
            gen_object_total_size(&real),
            "len {len}"
        );
        assert_eq!(disguise.element_type(), ArrayElementType::Long);
        assert_ne!(disguise.gc_flags() & GC_FLAG_OLD_GEN, 0);
        assert_eq!(disguise.class_id.as_u32(), 7);
    }
    // The arm is opt-in, reference-only and bounded like the primitive one.
    let big = YOUNG_ZERO_OUTSIDE_LOCK_MIN;
    assert!(!humongous_ref_body_zeroes_outside_the_lock(
        false,
        ArrayElementType::Reference,
        big
    ));
    assert!(humongous_ref_body_zeroes_outside_the_lock(
        true,
        ArrayElementType::Reference,
        big
    ));
    assert!(!humongous_ref_body_zeroes_outside_the_lock(
        true,
        ArrayElementType::Long,
        big
    ));
    assert!(!humongous_ref_body_zeroes_outside_the_lock(
        true,
        ArrayElementType::Reference,
        big - 8
    ));
}

/// `review6` finding 3, the root-slice half: a root in the pinned range
/// that is a one-past-the-end word (`arr + size`, equal to the NEXT
/// object's base) pins the object it ends, not only the next one.
#[test]
fn a_one_past_the_end_root_in_the_pin_range_pins_the_object_it_ends() {
    let heap = GenerationalHeap::with_sizes(64 * 1024, 256 * 1024);
    let obj = |og: &mut OldGen, slots: u32| -> usize {
        let p = og
            .alloc(HEADER_SIZE + SLOT_SIZE * slots as usize, 8)
            .expect("room");
        // SAFETY: a fresh, zeroed block sized for a header and `slots` slots.
        unsafe { (*(p as *mut ObjectHeader)).set_num_slots(slots) };
        p as usize
    };
    let (arr, next) = {
        let mut og = heap.old_gen.lock();
        let _dead = obj(&mut *og, 256); // a hole in front of `arr`
        let arr = obj(&mut *og, 4);
        let next = obj(&mut *og, 2);
        og.request_fragmentation_compaction();
        (arr, next)
    };
    let arr_end = arr + HEADER_SIZE + 4 * SLOT_SIZE;
    assert_eq!(
        arr_end, next,
        "precondition: `next` starts where `arr` ends"
    );
    let roots = [
        // Index 0: a precise root to `arr`, outside the pinned range.
        // SAFETY: a live old-gen object base.
        unsafe { ObjectRef::from_raw(arr as *mut u8) },
        // Index 1: the peers' range — the one-past-the-end word.
        // SAFETY: 8-aligned and non-null (it is `next`'s base).
        unsafe { ObjectRef::from_raw(arr_end as *mut u8) },
    ];
    let (_reclaimed, map) = heap.sweep_old_gen_non_moving_with(
        &roots,
        &cratonvm_types::PointerMap::default(),
        |n| {
            assert_eq!(n, 2);
            Some(Ok(OldPinnedCompact::with_range(Vec::new(), (1, 2))))
        },
    );
    assert_eq!(
        map.get(&arr).copied().unwrap_or(arr),
        arr,
        "`arr` is pinned by its end word"
    );
    assert_eq!(
        map.get(&next).copied().unwrap_or(next),
        next,
        "`next` is pinned as a base"
    );
    let s = heap.old_gen_sizing_stats();
    assert_eq!(
        (s.pinned_compactions, s.pinned_compaction_pins_last),
        (1, 2),
        "{s:?}"
    );
}
