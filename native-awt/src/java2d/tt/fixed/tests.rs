use super::*;

#[test]
fn rounding_is_sign_symmetric() {
    assert_eq!(mul_fix(3, 0x8000), 2);
    assert_eq!(mul_fix(-3, 0x8000), -2);
    assert_eq!(mul_div(7, 3, 2), 11);
    assert_eq!(mul_div(-7, 3, 2), -11);
    assert_eq!(mul_fix14(0x4000, 0x4000), 0x4000);
    assert_eq!(div_fix(1, 2), 0x8000);
}

#[test]
fn norm_len_axis_and_diagonal() {
    assert_eq!(norm_len(5, 0), (0x10000, 0));
    assert_eq!(norm_len(0, -7), (0, -0x10000));
    let (x, y) = norm_len(100, 100);
    assert!((x - 46341).abs() <= 2 && (y - 46341).abs() <= 2, "{x} {y}");
}
