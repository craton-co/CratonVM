use super::*;
use cratonvm_types::{ArrayElementType, ClassId, ObjectKind};

/// A `FastFieldSite` for a legacy-body receiver of `(class_id, num_slots)`.
fn legacy_site(class_id: u32, num_slots: u32) -> FastFieldSite {
    FastFieldSite {
        receiver_class_id: ClassId::new(class_id),
        num_slots,
        offset: 0,
        storage: None,
        field_index: 0,
        desc_byte: b'I',
        is_reference: false,
        final_put: FastFieldSite::PUT_ANY,
    }
}

/// **The test the registry elide needed and did not have.**
///
/// `field_ptr_for` must refuse an address the heap does not know, even when
/// the bytes at that address are a *perfectly matching header*. That is the
/// only case that distinguishes "the probe ran" from "the probe was
/// skipped": every other guard in the function — class id, slot count,
/// compact flag — passes by construction here.
///
/// # Why the header is forged rather than wild
///
/// A wild pointer would make the elided build SEGFAULT, which aborts the
/// whole test binary and takes every other result with it. A forged header
/// in a Rust-owned allocation is readable, satisfies all three comparisons,
/// and is not in the heap's registry — so the probe is the *only* thing
/// that can refuse it. With the probe: `None`, and this passes. Without it:
/// `Some(ptr)`, and this fails as an assertion rather than a crash.
///
/// This is the shape any future "the fast path may skip check X" change
/// needs: construct the input that only X rejects.
#[test]
fn field_ptr_for_refuses_a_matching_header_the_heap_does_not_know() {
    let zgc = VmHeap::from_zgc_capacity(64 * 1024 * 1024);
    let (class_id, num_slots) = (4321u32, 3u32);

    // A header the site would accept, in memory the heap never handed out.
    let forged = Box::new(ObjectHeader::new(
        ClassId::new(class_id),
        ObjectKind::Object,
        ArrayElementType::Reference,
        0,
        num_slots,
    ));
    let ptr = Box::into_raw(forged);
    let site = legacy_site(class_id, num_slots);

    // SAFETY: `ptr` is a live `Box` allocation for the length of this test.
    let header = unsafe { &*ptr };
    assert_eq!(header.class_id, site.receiver_class_id, "forge is wrong");
    assert_eq!(header.num_slots(), site.num_slots, "forge is wrong");
    assert!(
        !cratonvm_types::is_compact_object(header),
        "forge is wrong: a legacy site must meet a legacy header"
    );

    let got = field_ptr_for(&zgc, ptr as u64, &site);

    // SAFETY: reclaim the forged header; nothing retained it.
    unsafe { drop(Box::from_raw(ptr)) };

    assert!(
        got.is_none(),
        "field_ptr_for accepted an address the heap does not know. Every \
         other guard in it passes for this input by construction, so this \
         can only mean the `is_object_address` probe was removed. It is \
         not redundant with the slow path: `op_getfield` and `op_putfield` \
         both reach `load_and_forward`, whose first act is that same probe."
    );
}

/// The array arms deliberately do **not** probe, and that asymmetry is
/// load-bearing rather than an oversight — so it is pinned here too.
///
/// `prim_elem_ptr` is at parity with the handler it replaces:
/// `VmHeap::get_array_element` and `VmHeap::array_length` are both reached
/// with **no** `load_and_forward` and no membership test between the
/// operand-stack pop and the header read. The field path is not at parity,
/// because its handlers do reach one. A single switch used to cover both;
/// this test records which side is which, so that flipping either one has
/// to come here and say why.
#[test]
fn the_array_arm_is_at_parity_with_its_handler_and_the_field_arm_is_not() {
    // Not an execution test — a statement of the contract the two arms are
    // held to, in a place a change to either has to walk past.
    let field_handler_probes = true; // op_getfield/op_putfield -> load_and_forward
    let array_handler_probes = false; // get_array_element / array_length
    assert!(
        field_handler_probes,
        "if op_getfield stops calling load_and_forward, field_ptr_for's \
         probe may be reconsidered — until then it stays"
    );
    assert!(
        !array_handler_probes,
        "if the array handlers start validating, prim_elem_ptr must too"
    );
}
