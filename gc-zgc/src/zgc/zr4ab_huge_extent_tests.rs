use super::*;

fn forge_len(arr: ObjectRef, len: u32) -> u32 {
    // SAFETY: a live array allocated by the test; only the length word of
    // its header is rewritten, and restored before any collection.
    let header = unsafe { &mut *(arr.as_ptr() as *mut ObjectHeader) };
    let old = header.array_length();
    header.set_array_length(len);
    old
}

/// The screen itself: an object, a length past the Java limit, an extent
/// inside the window, and an extent that runs off the arena are refused.
#[test]
fn the_huge_extent_screen_refuses_objects_clobbered_lengths_and_off_arena_extents() {
    let huge_ints = (ZgcRealHeap::MAX_ALLOC_EXTENT / 4 + 4096) as u32;
    let arr = ObjectHeader::new(
        ClassId::new(0),
        ObjectKind::Array,
        ArrayElementType::Int,
        huge_ints,
        huge_ints,
    );
    let base = 0x1000_0000usize;
    let size = huge_array_extent_of(&arr, base, usize::MAX).expect("a real huge int[]");
    assert!(size > ZgcRealHeap::MAX_ALLOC_EXTENT);
    assert_eq!(
        huge_array_extent_of(&arr, base, base + size - 1),
        None,
        "an extent past the arena end is refused"
    );
    assert_eq!(huge_array_extent_of(&arr, base, base + size), Some(size));
    let clobbered = ObjectHeader::new(
        ClassId::new(0),
        ObjectKind::Array,
        ArrayElementType::Long,
        (ZGC_REAL_MAX_ARRAY_LENGTH as u32) + 1,
        1,
    );
    assert_eq!(
        huge_array_extent_of(&clobbered, base, usize::MAX),
        None,
        "a length past ZGC_REAL_MAX_ARRAY_LENGTH is a clobbered header"
    );
    let small = ObjectHeader::new(
        ClassId::new(0),
        ObjectKind::Array,
        ArrayElementType::Int,
        16,
        16,
    );
    assert_eq!(huge_array_extent_of(&small, base, usize::MAX), None);
    let obj = ObjectHeader::new(
        ClassId::new(5),
        ObjectKind::Object,
        ArrayElementType::Reference,
        0,
        huge_ints,
    );
    assert_eq!(huge_array_extent_of(&obj, base, usize::MAX), None);
}

/// In overflow mode the candidate is a registry scan result. A clobbered
/// length on a small array must not claim the addresses after it (before
/// zr z4/ab `contains` sized it from `array_data_size` with no bound).
#[test]
fn overflow_mode_does_not_trust_a_clobbered_length() {
    let heap = ZgcRealHeap::with_capacity(16 << 20);
    let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Long, 4);
    let b = arr.as_ptr() as usize;
    heap.counters.huge_overflow.store(true, Ordering::Release);
    let old = forge_len(arr, (ZGC_REAL_MAX_ARRAY_LENGTH as u32) + 1);
    // Past the window but within the forged extent: the only candidate the
    // unbounded scan can return is `b`.
    let probe = b + ZgcRealHeap::MAX_ALLOC_EXTENT.min(8 << 20) + 64;
    let got = heap.huge_extent_containing(probe, true);
    forge_len(arr, old);
    heap.counters.huge_overflow.store(false, Ordering::Release);
    assert!(got.is_none(), "a clobbered header claimed {probe:#x}");
    assert_eq!(heap.huge_extent_hits(), 0);
}


/// zt t6/rv3 (`zgc-zt4x3-*` item 5): a held span 300 MiB into a
/// registered huge array is claimed by it. The straddle probe's window
/// stops `MAX_ALLOC_EXTENT` (256 MiB) below the span, so before the
/// huge-extent fallback the span read as unclaimed. Control: the same
/// span with the array out of the table reads unclaimed (the window alone).
#[test]
fn zt6rv3_a_held_span_deep_inside_a_huge_array_is_straddled() {
    const M: usize = 1 << 20;
    // The forged extent (320 MiB) must end inside the arena (the screen).
    let heap = ZgcRealHeap::with_capacity(640 * M);
    let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Long, 4);
    let b = arr.as_ptr() as usize;
    let base = heap.arena_base;
    let s = b - base + 300 * M;
    assert!(300 * M > ZgcRealHeap::MAX_ALLOC_EXTENT, "beyond the window");
    let old = forge_len(arr, (320 * M / 8) as u32);
    assert!(
        heap.huge_array_extent(b).is_some(),
        "precondition: the forged array passes the screen"
    );
    let window_only = heap.quarantine_span_straddled(base, s);
    heap.note_huge_extent(b);
    let with_table = heap.quarantine_span_straddled(base, s);
    heap.counters.huge_bases[0].store(0, Ordering::Release);
    heap.counters.huge_hwm.store(0, Ordering::Release);
    heap.counters.huge_overflow.store(false, Ordering::Release);
    forge_len(arr, old);
    assert!(!window_only, "control: the 256 MiB window cannot see the base");
    assert!(with_table, "the huge-extent table claims the span");
}
