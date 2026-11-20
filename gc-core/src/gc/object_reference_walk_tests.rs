use super::*;
use crate::heap::Heap;
use cratonvm_types::ClassId;

fn children_at_width(
    obj: usize,
    width: usize,
    read: &dyn Fn(usize) -> u64,
) -> (bool, Vec<usize>) {
    let mut out = Vec::new();
    // SAFETY: `obj` is a live array of this test's own heap, nothing runs
    // concurrently, and every slot the reader is handed lies inside it.
    let walked = unsafe {
        for_each_object_reference_at_width(obj, usize::MAX, width, read, &mut |c| out.push(c))
    };
    (walked, out)
}

/// gci i45/bh: the oracle walk reads reference-array elements at the
/// configured reference width and through the configured decoder. The
/// same 16 bytes holding two packed 4-byte narrow codes decode to the two
/// elements at width 4, and to one undecoded word naming neither at the
/// flat 8-byte reading the walk used before.
#[test]
fn bh45_the_oracle_walk_reads_reference_elements_at_the_configured_width() {
    let heap = Heap::with_capacity(64 * 1024);
    let e1 = heap.alloc_object(ClassId::new(1), 1).as_ptr() as usize;
    let e2 = heap.alloc_object(ClassId::new(1), 1).as_ptr() as usize;
    let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Reference, 4);
    let arr_addr = arr.as_ptr() as usize;
    let base = (e1.min(e2) & !7) - 8;
    let code = |a: usize| -> u32 { u32::try_from((a - base) >> 3).expect("fits") };
    // SAFETY: the array's data area is 4 * 8 = 32 bytes (wide elements),
    // all inside the allocation; the test owns the heap.
    unsafe {
        let data = (arr_addr + ARRAY_DATA_OFFSET) as *mut u8;
        std::ptr::write_bytes(data, 0, 4 * 8);
        (data as *mut u32).write_unaligned(code(e1));
        (data.add(4) as *mut u32).write_unaligned(code(e2));
    }
    let narrow = |p: usize| -> u64 {
        // SAFETY: `p` is a 4-byte element slot inside the array above.
        let n = unsafe { (p as *const u32).read_unaligned() };
        if n == 0 {
            0
        } else {
            base as u64 + (u64::from(n) << 3)
        }
    };
    let (walked, kids) = children_at_width(arr_addr, 4, &narrow);
    assert!(walked);
    assert_eq!(kids, vec![e1, e2], "both narrow elements decoded");

    // The pre-fix reading: a flat u64 per element at an 8-byte stride.
    let flat = |p: usize| -> u64 {
        // SAFETY: an 8-byte word inside the array's 32-byte data area.
        unsafe { (p as *const u64).read_unaligned() }
    };
    let (_, flat_kids) = children_at_width(arr_addr, 8, &flat);
    assert!(
        !flat_kids.contains(&e1) && !flat_kids.contains(&e2),
        "the flat reading names neither element: {flat_kids:x?}"
    );
}

/// gci i45/bh: with wide references (the default, and the only width a
/// unit test may run) the public walk reads exactly the elements it read
/// before the width fix.
#[test]
fn bh45_the_wide_oracle_walk_still_names_each_element() {
    let heap = Heap::with_capacity(64 * 1024);
    let e1 = heap.alloc_object(ClassId::new(1), 1);
    let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Reference, 3);
    heap.set_array_element(arr, 1, Value::Object(Some(e1)))
        .unwrap();
    let mut kids = Vec::new();
    // SAFETY: a live array of this test's own heap; nothing runs concurrently.
    let walked = unsafe {
        for_each_object_reference(arr.as_ptr() as usize, usize::MAX, &mut |c| kids.push(c))
    };
    assert!(walked);
    assert_eq!(kids, vec![e1.as_ptr() as usize]);
}
