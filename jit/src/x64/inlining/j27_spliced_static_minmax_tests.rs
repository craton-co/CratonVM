use super::*;

#[test]
fn j27_splice_serves_the_int_long_float_double_minmax_rows() {
    let m = "java/lang/Math";
    assert_eq!(
        spliced_static_minmax_entry(m, "max", "(II)I"),
        Some(crate::MATH_MAX_INT_INTRINSIC)
    );
    assert_eq!(
        spliced_static_minmax_entry(m, "min", "(II)I"),
        Some(crate::MATH_MIN_INT_INTRINSIC)
    );
    assert_eq!(
        spliced_static_minmax_entry("java/lang/StrictMath", "max", "(JJ)J"),
        Some(crate::MATH_MAX_LONG_INTRINSIC)
    );
    assert_eq!(
        spliced_static_minmax_entry(m, "min", "(JJ)J"),
        Some(crate::MATH_MIN_LONG_INTRINSIC)
    );
    assert_eq!(
        spliced_static_minmax_entry(m, "max", "(DD)D"),
        Some(crate::MATH_MAX_DOUBLE_INTRINSIC)
    );
    assert_eq!(
        spliced_static_minmax_entry(m, "min", "(FF)F"),
        Some(crate::MATH_MIN_FLOAT_INTRINSIC)
    );
    assert_eq!(spliced_static_minmax_entry(m, "sqrt", "(D)D"), None);
    assert_eq!(spliced_static_minmax_entry("java/lang/Integer", "max", "(II)I"), None);    }