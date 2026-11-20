//! Round nio20 wave 6 (lane L1): the byte-buffer trap-miss site rule and
//! the plain-static-read feed.
use super::*;

#[test]
fn only_a_byte_buffer_call_into_a_java_nio_byte_buffer_body_is_a_buffer_site() {
    assert!(ir_buffer_trap_site(
        "java/nio/ByteBuffer",
        "java/nio/HeapByteBuffer"
    ));
    assert!(ir_buffer_trap_site(
        "java/nio/ByteBuffer",
        "java/nio/DirectByteBuffer"
    ));
    assert!(ir_buffer_trap_site(
        "java/nio/MappedByteBuffer",
        "java/nio/DirectByteBufferR"
    ));
    assert!(ir_buffer_trap_site(
        "java/nio/ByteBuffer",
        "java/nio/HeapByteBufferR"
    ));
    // Another static type, another body, a user subclass: no.
    assert!(!ir_buffer_trap_site(
        "java/nio/CharBuffer",
        "java/nio/HeapCharBuffer"
    ));
    assert!(!ir_buffer_trap_site(
        "java/nio/Buffer",
        "java/nio/HeapByteBuffer"
    ));
    assert!(!ir_buffer_trap_site(
        "java/nio/ByteBuffer",
        "java/nio/ByteBufferAsIntBufferB"
    ));
    assert!(!ir_buffer_trap_site(
        "java/nio/ByteBuffer",
        "com/example/MyBuffer"
    ));
}



#[test]
fn the_plain_static_feed_needs_a_resolved_base_and_a_plain_read() {
    // A table with no static-base resolver answers nothing.
    let table = DirectHelperTable::EMPTY;
    let rows = [
        (5u32, 0usize, b'L', false),
        (5, 1, b'I', true),
        (6, 0, b'I', false),
        (5, 0, b'L', false),
    ];
    assert!(ir_plain_static_read_rows(&table, rows.iter()).is_empty());
    // A resolver that answers for class 5 only.
    extern "C" fn class_five(_ctx: i64, class_id: i64, _field: i64) -> i64 {
        if class_id == 5 {
            0x1000
        } else {
            0
        }
    }
    let mut table = DirectHelperTable::EMPTY;
    table.static_base_resolver = class_five as *const () as usize;
    table.static_base_resolver_ctx = 1;
    let on = ir::ir_plain_static_read_enabled() && x64::inline_getstatic_enabled();
    let got = ir_plain_static_read_rows(&table, rows.iter());
    if on {
        // The volatile read and the unresolved class are left out; the
        // duplicate collapses.
        assert_eq!(got, vec![(5, 0)]);
    } else {
        assert!(got.is_empty());
    }
}
