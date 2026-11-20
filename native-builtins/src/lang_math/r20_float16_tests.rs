use super::{float16_bits_to_float_bits, float_to_float16_bits};

/// Rows printed by `R20HuntIntrMathExact` (`pt.f16`, `pt.f16b`) on HotSpot
/// 25.0.3, default and `-Xint` identical.
#[test]
fn matches_hotspot_rows() {
    let f = |x: f32| float_to_float16_bits(x.to_bits()) as i16;
    assert_eq!(f(65520.0), 31744);
    assert_eq!(f(65519.996), 31743);
    assert_eq!(f(5.9604645E-8), 1);
    assert_eq!(f(2.9802322E-8), 0);
    assert_eq!(f(2.9802326E-8), 1);
    assert_eq!(f(f32::NAN), 32256);
    assert_eq!(float_to_float16_bits(0x7fc0_1234) as i16, 32256);
    assert_eq!(float_to_float16_bits(0xff80_1234) as i16, -512);
    assert_eq!(float16_bits_to_float_bits(0x7c01), 2_143_297_536);
    assert_eq!(float16_bits_to_float_bits(0xfe00), 0xffc0_0000);
    assert_eq!(
        f32::from_bits(float16_bits_to_float_bits(0x0001)),
        5.9604645E-8
    );
    assert_eq!(float16_bits_to_float_bits(0x8000), 0x8000_0000);
    assert_eq!(f(-0.0), i16::MIN);
    assert_eq!(f(f32::NEG_INFINITY) as u16, 0xfc00);
}

/// Every non-NaN binary16 survives the round trip bit for bit, and its
/// float is the exact value.
#[test]
fn non_nan_round_trip_is_exact() {
    for h in 0..=u16::MAX {
        if (h & 0x7c00) == 0x7c00 && (h & 0x03ff) != 0 {
            continue;
        }
        let f = f32::from_bits(float16_bits_to_float_bits(h));
        assert_eq!(float_to_float16_bits(f.to_bits()), h, "h={h:#06x}");
    }
}
