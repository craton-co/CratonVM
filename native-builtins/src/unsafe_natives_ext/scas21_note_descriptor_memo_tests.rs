#[test]
fn scas21_the_static_publication_note_reads_the_memoised_descriptor() {
    const SRC: &str = concat!(include_str!("../unsafe_natives_ext/mod.rs"), "\n", include_str!("../unsafe_natives_ext/unsafe_offset_is_heap_slot.rs"), "\n", include_str!("../unsafe_natives_ext/native_unsafe_get_long_volatile.rs"), "\n", include_str!("../unsafe_natives_ext/unsafe_array_read_bytes.rs"));
    let start = SRC
        .find("pub fn unsafe_note_static_atomic_target(")
        .expect("unsafe_note_static_atomic_target moved");
    let rest = &SRC[start..];
    let end = rest.find("\n}\n").expect("function end");
    let body = &rest[..end];
    assert!(
        body.contains("unsafe_static_desc_at("),
        "the note must use the memoised descriptor"
    );
    assert!(
        !body.contains("unsafe_static_declared_desc("),
        "the note must not scan the declared fields itself"
    );
}
