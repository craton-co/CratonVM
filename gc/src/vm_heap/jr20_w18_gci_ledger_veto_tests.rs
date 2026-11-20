use crate::gc_quiescence::ledger_names_young_object;
use crate::heap::{ArrayElementType, ObjectHeader, ObjectKind};

/// A ledger word at the object's base or anywhere inside it vetoes the
/// precise-root exemption; a word just past its end, or none, does not.
#[test]
fn a_ledger_word_inside_the_object_names_it_and_one_past_the_end_does_not() {
    let mut buf = [0u64; 16];
    let header = ObjectHeader::new(
        cratonvm_types::ClassId::new(7),
        ObjectKind::Object,
        ArrayElementType::Reference,
        0,
        2,
    );
    let size = crate::gen_heap::gen_object_total_size(&header);
    // SAFETY: `buf` is 128 bytes, 8-aligned, larger than any header.
    unsafe { std::ptr::write(buf.as_mut_ptr() as *mut ObjectHeader, header) };
    let base = buf.as_ptr() as usize;
    assert!(size > 0 && size <= 128, "size {size}");
    let in_words =
        |words: Vec<usize>| move |s: usize, e: usize| words.iter().any(|&w| w >= s && w < e);
    assert!(ledger_names_young_object(&in_words(vec![base]), base));
    assert!(ledger_names_young_object(
        &in_words(vec![base + size - 8]),
        base
    ));
    assert!(!ledger_names_young_object(
        &in_words(vec![base + size]),
        base
    ));
    assert!(!ledger_names_young_object(&in_words(Vec::new()), base));
}
