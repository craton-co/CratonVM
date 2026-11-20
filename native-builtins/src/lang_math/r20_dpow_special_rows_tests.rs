use super::native_math_pow;
use cratonvm_types::Value;

fn pow_bits(x: f64, y: f64) -> u64 {
    let mut ctx = crate::test_utils::mock_ctx();
    match native_math_pow(&mut ctx, &[Value::Double(x), Value::Double(y)]) {
        Ok(Some(Value::Double(v))) => v.to_bits(),
        other => panic!("pow returned {other:?}"),
    }
}

/// HotSpot 25's `dpow` stub rows, measured with `R20HuntIntrMathExact` /
/// `R20HuntIntrPowUlp` (default == `-Xint`).
#[test]
fn hotspot_dpow_special_rows() {
    assert_eq!(pow_bits(f64::MAX, 0.5), 0x5fef_ffff_ffff_ffff);
    assert_eq!(
        pow_bits(0.9999999999999999, 0.5),
        0.9999999999999999_f64.to_bits()
    );
    assert_eq!(pow_bits(0.0, 0.5), 0);
    assert_eq!(pow_bits(f64::INFINITY, 0.5), f64::INFINITY.to_bits());
    // -0.0 is below +0 as a signed bit pattern: the general path, +0.0.
    assert_eq!(pow_bits(-0.0, 0.5), 0);
    assert_eq!(pow_bits(f64::NEG_INFINITY, 0.5), f64::INFINITY.to_bits());
    assert_eq!(pow_bits(3.0, 2.0), 9.0_f64.to_bits());
    assert_eq!(pow_bits(f64::NEG_INFINITY, 2.0), f64::INFINITY.to_bits());
    // The [2^51, 2^52) exponent rows of a -0.0 / -Infinity base.
    let ninf = f64::NEG_INFINITY.to_bits();
    let pinf = f64::INFINITY.to_bits();
    let nzero = (-0.0_f64).to_bits();
    assert_eq!(pow_bits(-0.0, -4503599627370495.5), ninf);
    assert_eq!(pow_bits(-0.0, -2251799813685248.5), ninf);
    assert_eq!(pow_bits(-0.0, -4503599627370495.0), pinf);
    assert_eq!(pow_bits(-0.0, 4503599627370495.0), 0);
    assert_eq!(pow_bits(-0.0, 2251799813685248.5), 0);
    assert_eq!(pow_bits(f64::NEG_INFINITY, -4503599627370495.5), nzero);
    assert_eq!(pow_bits(f64::NEG_INFINITY, -4503599627370494.0), 0);
    assert_eq!(pow_bits(f64::NEG_INFINITY, 4503599627370495.0), pinf);
    // Outside that range the JLS rule: 2^52 + 1 is odd.
    assert_eq!(pow_bits(-0.0, 4503599627370497.0), nzero);
}
