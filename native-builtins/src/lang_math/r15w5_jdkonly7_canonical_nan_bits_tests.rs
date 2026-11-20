use super::{java_double_to_long_bits, java_float_to_int_bits};

/// Failed before the fix at the `ByteArray.setFloat`/`setDouble` natives,
/// which wrote `to_bits()` of a NaN payload.
#[test]
fn a_nan_payload_is_written_as_the_canonical_nan() {
    assert_eq!(
        java_float_to_int_bits(f32::from_bits(0x7fc0_0001)),
        0x7fc0_0000
    );
    assert_eq!(
        java_float_to_int_bits(f32::from_bits(0xffc0_1234)),
        0x7fc0_0000
    );
    assert_eq!(java_float_to_int_bits(-0.0), 0x8000_0000);
    assert_eq!(
        java_double_to_long_bits(f64::from_bits(0x7ff8_0000_0000_0001)),
        0x7ff8_0000_0000_0000
    );
    assert_eq!(java_double_to_long_bits(1.5), 1.5f64.to_bits());
}
