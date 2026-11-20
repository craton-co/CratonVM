#[test]
fn only_the_heap_buffer_array_row_is_a_candidate() {
    assert!(super::byte_array_field_row("hb", "[B"));
    assert!(!super::byte_array_field_row("hb", "[C"));
    assert!(!super::byte_array_field_row("hb", "[I"));
    assert!(!super::byte_array_field_row("value", "[B"));
    assert!(!super::byte_array_field_row("hb", "Ljava/lang/Object;"));
}
