use super::unsafe_changed_byte_span;

/// JIT round 21, lane atomics (UNSAFE21-3): the process-lock arms write
/// back only the span of bytes an update changes -- `R20AtomSplitLock`'s
/// CAS increments the low 24 bits and must leave the top byte (where a
/// plain store put the marker) alone.
#[test]
fn the_lock_arms_write_back_only_the_changed_span() {
    assert_eq!(
        unsafe_changed_byte_span(0x0700_0012, 0x0700_0013, 4),
        Some((0, 0))
    );
    assert_eq!(
        unsafe_changed_byte_span(0x0700_FFFF, 0x0701_0000, 4),
        Some((0, 2)),
        "a carry into the third byte"
    );
    assert_eq!(unsafe_changed_byte_span(0x0700_0012, 0x0700_0012, 4), None);
    assert_eq!(
        unsafe_changed_byte_span(0x11, 0x1100_0000_0000_0011, 8),
        Some((7, 7))
    );
    assert_eq!(
        unsafe_changed_byte_span(0x1234, 0xFF_1234, 2),
        None,
        "bytes past the width are not the update's"
    );
    assert_eq!(
        unsafe_changed_byte_span(0x0001_0000_0000_0001, 0, 8),
        Some((0, 6))
    );
}
