use super::*;

#[test]
fn base_offset_roundtrip() {
    // int[] at index 3 -> byte offset 16 + 3*4 = 28
    assert_eq!(index_to_byte_offset(3, 4), 28);
    assert_eq!(byte_offset_to_index(28, 4), 3);
}

#[test]
fn long_array_roundtrip() {
    // long[] at index 2 -> 16 + 2*8 = 32
    assert_eq!(index_to_byte_offset(2, 8), 32);
    assert_eq!(byte_offset_to_index(32, 8), 2);
}

#[test]
fn byte_array_roundtrip() {
    // byte[] at index 42 -> 16 + 42*1 = 58
    assert_eq!(index_to_byte_offset(42, 1), 58);
    assert_eq!(byte_offset_to_index(58, 1), 42);
}

#[test]
fn ref_array_scale_eight() {
    // Object[] uses scale=8 on 64-bit
    assert_eq!(index_to_byte_offset(7, 8), 16 + 7 * 8);
}

#[test]
fn byte_offset_below_base_yields_zero() {
    // Defensive: offset < ABASE should not underflow
    assert_eq!(byte_offset_to_index(0, 4), 0);
    assert_eq!(byte_offset_to_index(8, 4), 0);
}

#[test]
fn scale_zero_treats_as_one() {
    assert_eq!(index_to_byte_offset(5, 0), 16 + 5);
}
