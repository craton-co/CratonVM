use super::*;

struct NoMonitors;
impl MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _: &cratonvm_types::PointerMap) {}
}

/// zr z2/rv: a derived pointer deep inside a live array past
/// `MAX_ALLOC_EXTENT` resolves to the array, through both interior probes.
///
/// Before the huge-extent table both probes answered `None` here: their
/// backwards scan stops `MAX_ALLOC_EXTENT` below the address and the base
/// is further down. Harmless while such an array was unsizable (never
/// freed); since `CRATONVM_ZGC_LARGE_ARRAY_SIZING` sizes it, and the
/// take-over pass roots derived pointers through these probes, a frozen
/// compiled loop's cursor past the 256 MiB mark left the array unmarked and
/// the sweep freed it under the loop. The tail of the test is the control:
/// once the array is dead and swept, the same word resolves to nothing and
/// the slot is pruned.
#[test]
fn a_derived_pointer_deep_inside_a_huge_array_resolves_to_its_base() {
    let heap = ZgcRealHeap::with_capacity(600 << 20);
    let len = ZgcRealHeap::MAX_PLAUSIBLE_BODY / 4 + 4096;
    let arr = heap
        .try_alloc_array(ClassId::new(0), ArrayElementType::Int, len)
        .expect("room for one huge array");
    let base = arr.as_ptr() as usize;
    // SAFETY: a fresh registered base; only its header is read.
    let header = unsafe { &*(base as *const ObjectHeader) };
    let size = ZgcRealHeap::alloc_size(header).expect("a real int[] past the body cap sizes");
    assert!(size > ZgcRealHeap::MAX_ALLOC_EXTENT);
    let deep = base + size - 64;
    assert!(
        deep - base > ZgcRealHeap::MAX_ALLOC_EXTENT,
        "outside the scan window"
    );
    let near = base + 4096;
    let ptr = |o: Option<ObjectRef>| o.map(|o| o.as_ptr() as usize);

    assert_eq!(
        ptr(heap.resolve_interior_for_pin(near)),
        Some(base),
        "inside the window"
    );
    assert_eq!(
        ptr(heap.resolve_interior_for_pin(deep)),
        Some(base),
        "pin probe, deep"
    );
    assert_eq!(ptr(heap.is_heap_addr(deep)), Some(base), "mark probe, deep");
    assert_eq!(
        ptr(heap.resolve_interior_for_pin(base + size)),
        Some(base),
        "one past the end names the object for pinning"
    );
    assert!(heap.huge_extent_hits() >= 3, "the table answered");

    // Live through a collection: still answered, slot kept.
    let stw = unsafe { StopTheWorldToken::new() };
    let mut roots = [arr];
    let _ = heap.collect_garbage(&stw, &mut roots, &NoMonitors);
    assert_eq!(
        roots[0].as_ptr() as usize,
        base,
        "a huge extent never moves"
    );
    assert_eq!(
        ptr(heap.is_heap_addr(deep)),
        Some(base),
        "survivor still resolves"
    );

    // Dead: swept, and the same word resolves to nothing.
    let _ = heap.collect_garbage(&stw, &mut [], &NoMonitors);
    assert!(!heap.registry.contains(base), "the dead array is reclaimed");
    assert_eq!(ptr(heap.resolve_interior_for_pin(deep)), None);
    assert_eq!(ptr(heap.is_heap_addr(deep)), None);
    assert_eq!(
        heap.counters.huge_hwm.load(Ordering::Relaxed),
        0,
        "slot pruned"
    );
}

/// zr z3/rv3: an array past `MAX_PLAUSIBLE_ARRAY_LEN` (1 << 28 elements),
/// which `alloc_size` refuses but the sweep frees, must resolve from a
/// derived pointer in its FIRST 256 MiB too. There the window scan finds
/// the base itself, and both probes used to stop at its unsizable header
/// with `None` -- a missed mark root for a frozen compiled cursor.
#[test]
fn a_derived_pointer_near_the_base_of_an_over_long_array_resolves() {
    let heap = ZgcRealHeap::with_capacity(320 << 20);
    let len = ZgcRealHeap::MAX_PLAUSIBLE_ARRAY_LEN + 4096;
    let arr = heap
        .try_alloc_array(ClassId::new(0), ArrayElementType::Byte, len)
        .expect("room for one over-long byte array");
    let base = arr.as_ptr() as usize;
    // SAFETY: a fresh registered base; only its header is read.
    let header = unsafe { &*(base as *const ObjectHeader) };
    assert!(
        ZgcRealHeap::alloc_size(header).is_none(),
        "precondition: alloc_size refuses the element count"
    );
    let near = base + 4096;
    let ptr = |o: Option<ObjectRef>| o.map(|o| o.as_ptr() as usize);
    assert_eq!(
        ptr(heap.resolve_interior_for_pin(near)),
        Some(base),
        "pin probe"
    );
    assert_eq!(ptr(heap.is_heap_addr(near)), Some(base), "mark probe");
    // Control: dead and swept, the same word resolves to nothing.
    let stw = unsafe { StopTheWorldToken::new() };
    let _ = heap.collect_garbage(&stw, &mut [], &NoMonitors);
    if !heap.registry.contains(base) {
        assert_eq!(ptr(heap.resolve_interior_for_pin(near)), None);
        assert_eq!(ptr(heap.is_heap_addr(near)), None);
    }
}

/// A slot whose base was re-used by a small object never answers for the
/// wrong object: the lookup re-validates the header's own extent.
#[test]
fn a_stale_huge_slot_cannot_answer_for_a_small_object() {
    let heap = ZgcRealHeap::with_capacity(16 << 20);
    let small = heap.alloc_object(ClassId::new(3), 2);
    let b = small.as_ptr() as usize;
    heap.counters.huge_bases[0].store(b, Ordering::Release);
    heap.counters.huge_hwm.store(1, Ordering::Release);
    assert!(heap.huge_extent_containing(b + 8, true).is_none());
    assert_eq!(heap.huge_extent_hits(), 0);
}
