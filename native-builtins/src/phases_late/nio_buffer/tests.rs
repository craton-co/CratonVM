use super::*;

/// Byte assembly is the half that needs no VM, and the half that returns a
/// plausible WRONG number when it is wrong.
#[test]
fn assembly_follows_the_buffer_byte_order() {
    assert_eq!(assemble_u16([0x01, 0x02], true), 0x0102);
    assert_eq!(assemble_u16([0x01, 0x02], false), 0x0201);
    assert_eq!(assemble_u32([1, 2, 3, 4], true), 0x0102_0304);
    assert_eq!(assemble_u32([1, 2, 3, 4], false), 0x0403_0201);
    assert_eq!(
        assemble_u64([1, 2, 3, 4, 5, 6, 7, 8], true),
        0x0102_0304_0506_0708
    );
    assert_eq!(
        assemble_u64([1, 2, 3, 4, 5, 6, 7, 8], false),
        0x0807_0605_0403_0201
    );
}

/// `getShort` is signed and `getChar` is not, off the same two bytes.
#[test]
fn short_is_signed_and_char_is_not() {
    assert_eq!(decode_short([0xFF, 0xFE], true), Value::Int(-2));
    assert_eq!(decode_char([0xFF, 0xFE], true), Value::Int(0xFFFE));
}

/// A NaN payload must survive; a numeric cast would not preserve it.
#[test]
fn float_decoding_preserves_the_raw_bit_pattern() {
    let bits: u32 = 0x7FC0_0001;
    let Value::Float(f) = decode_float(bits.to_be_bytes(), true) else {
        panic!("expected a float");
    };
    assert_eq!(f.to_bits(), bits);
    let dbits: u64 = 0x7FF8_0000_0000_0001;
    let Value::Double(d) = decode_double(dbits.to_be_bytes(), true) else {
        panic!("expected a double");
    };
    assert_eq!(d.to_bits(), dbits);
}
