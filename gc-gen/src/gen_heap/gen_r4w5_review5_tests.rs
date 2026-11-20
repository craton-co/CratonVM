use super::*;

/// `dead_young_ref_reason` must answer an address on an UNCOMMITTED young
/// granule without reading it (it used to dereference the header after a
/// containment test alone, which faults on a released granule), must
/// refuse a header that would run off the end of the semi-space, and must
/// still read a committed one.
#[test]
fn a_dead_young_address_on_an_uncommitted_granule_is_answered_without_a_read() {
    use cratonvm_gc_core::reservation::GRANULE;
    let heap = GenerationalHeap::with_sizes(8 * GRANULE, 8 * GRANULE);
    let obj = heap
        .try_alloc_object(ClassId::new(0), 2)
        .expect("a fresh semi-space serves one small object");
    let obj_addr = obj.as_ptr() as usize;
    let (base, cap, uncommitted) = {
        let from = heap.lock_young_from();
        let base = from.base_ptr() as usize;
        let cap = from.capacity();
        let uncommitted = (1..cap / GRANULE)
            .map(|g| g * GRANULE)
            .find(|&off| !from.is_readable_at(off));
        (base, cap, uncommitted)
    };
    assert!(
        obj_addr >= base && obj_addr < base + cap,
        "the object is young"
    );

    // A live object's header is not zero: not dead.
    assert_eq!(heap.dead_young_ref_reason(obj_addr), None);
    // Committed, never-written memory beside it reads as a zero header.
    assert_eq!(
        heap.dead_young_ref_reason(obj_addr + 4096),
        Some("ZERO-HEADER-IN-YOUNG")
    );

    match uncommitted {
        Some(off) => {
            assert_eq!(
                heap.dead_young_ref_reason(base + off),
                Some("UNCOMMITTED-YOUNG"),
                "a released or never-committed granule holds no object and must not be read"
            );
            // A header whose LAST byte is on the uncommitted granule: a
            // screen of the first byte alone would pass it.
            assert_eq!(
                heap.dead_young_ref_reason(base + off - 8),
                Some("UNCOMMITTED-YOUNG")
            );
        }
        None => {
            // A wholly committed fallback store (reservation refused or
            // disabled): every young byte is readable and a never-written
            // one is a zero header.
            assert_eq!(
                heap.dead_young_ref_reason(base + 4 * GRANULE),
                Some("ZERO-HEADER-IN-YOUNG")
            );
        }
    }
    // The last word of the semi-space: a header there would run off the
    // end, so no object starts there and nothing is read.
    assert_eq!(
        heap.dead_young_ref_reason(base + cap - 8),
        Some("HEADER-PAST-ACTIVE-SEMISPACE-END")
    );
}
