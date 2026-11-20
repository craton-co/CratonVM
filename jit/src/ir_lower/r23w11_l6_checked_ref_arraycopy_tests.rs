use super::*;


/// The null skip must land exactly on the store: the bytes it jumps over
/// are the proof's, emitted by the same encoders the arm uses.
#[test]
fn the_null_skip_covers_exactly_the_proof() {
    let mut buf = ExecutableBuffer::new(64).expect("buffer");
    buf.emit(&[0xF6, 0x40, cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8, 0x03]);
    buf.emit(&[0x0F, 0x85, 0, 0, 0, 0]);
    buf.emit(&[0x3B, 0x10]);
    buf.emit(&[0x0F, 0x85, 0, 0, 0, 0]);
    assert_eq!(buf.pos(), usize::from(CHECKED_COPY_NULL_SKIP));
}
