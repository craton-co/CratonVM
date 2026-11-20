use super::bd_exact_divide_scale_overflows;
use crate::bigint::BigInt;

fn ovf(a: &str, sa: i32, b: &str, sb: i32) -> bool {
    bd_exact_divide_scale_overflows(&BigInt::from_decimal(a), sa, &BigInt::from_decimal(b), sb)
}

/// Rows measured on HotSpot 25.0.3 (`divide(BigDecimal)`): `true` where it
/// throws "Non-terminating" from the scale arithmetic alone.
#[test]
fn matches_hotspot_scale_edges() {
    assert!(ovf("1", i32::MAX, "1", -10)); // 1E-2147483647 / 1E+10
    assert!(ovf("1", i32::MAX, "1", 0)); // 1E-2147483647 / 1
    assert!(ovf("1", 2147483640, "1", -5)); // 1E-2147483640 / 1E+5
    assert!(ovf("1", -i32::MAX, "1", 10)); // 1E+2147483647 / 1E-10
    assert!(!ovf("1", -i32::MAX, "1", 1)); // 1E+2147483647 / 1E-1 = 1E+2147483648
    assert!(!ovf("1", -i32::MAX, "4", 1)); // = 2.5E+2147483647
    assert!(!ovf("1", -2147483640, "1", 8)); // 1E+2147483640 / 1E-8
    assert!(!ovf("123", 3, "7", 1)); // ordinary scales
}
