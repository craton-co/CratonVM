use super::{bd_compare_unscaled, bd_exact_divide_core, bd_parse_decimal_str};
use crate::bigint::BigInt;
use std::cmp::Ordering;

fn parse(s: &str) -> (String, i32) {
    let (u, sc) = bd_parse_decimal_str(s).unwrap_or_else(|| panic!("{s:?} must parse"));
    (u.to_decimal(), sc)
}

/// Every expected pair is what `new java.math.BigDecimal(s)` reports as
/// `(unscaledValue(), scale())` on a JDK -- the contract, not a guess.
#[test]
fn it_builds_the_pair_the_jdk_constructor_builds() {
    assert_eq!(parse("1.25"), ("125".into(), 2));
    assert_eq!(parse("-1.25"), ("-125".into(), 2));
    assert_eq!(parse("+7"), ("7".into(), 0));
    assert_eq!(parse("100"), ("100".into(), 0));
    assert_eq!(parse(".5"), ("5".into(), 1));
    assert_eq!(parse("5."), ("5".into(), 0));
    assert_eq!(parse("0.00"), ("0".into(), 2));
    // The exponent adjusts the scale, and this is the case the old synthetic
    // arm got wrong: it turned "1.5E+10" into the integer 1510.
    assert_eq!(parse("1.5E+10"), ("15".into(), -9));
    assert_eq!(parse("1.5e10"), ("15".into(), -9));
    assert_eq!(parse("2.5E-3"), ("25".into(), 4));
    assert_eq!(parse("  42  "), ("42".into(), 0), "surrounding whitespace");
}

/// Outside the grammar is `None`, never a number assembled from whatever
/// digits happened to be present -- which is what `BigInt::from_decimal`
/// alone would do, since it skips non-digits.
#[test]
fn it_refuses_what_is_not_a_decimal() {
    for bad in [
        "", ".", "-", "e5", "1e", "1e+", "1.2.3", "12a", "1,5", "NaN", "Infinity", "--1",
    ] {
        assert!(
            bd_parse_decimal_str(bad).is_none(),
            "{bad:?} must be refused"
        );
    }
}

/// A scale outside `i32` is refused rather than wrapped: `BigDecimal`'s scale
/// is an `int`, so such a string names no `BigDecimal`.
#[test]
fn it_refuses_a_scale_that_does_not_fit_an_int() {
    assert!(bd_parse_decimal_str("1E-3000000000").is_none());
    assert!(bd_parse_decimal_str("1E+3000000000").is_none());
}

/// **Defect 1, closed.** Two values agreeing in their leading ~17 digits
/// parse to the same `f64`, so the old synthetic `compareTo` called them
/// EQUAL. Through the exact parse they order correctly, and the `f64`
/// comparison is asserted alongside so this test proves the difference and
/// not merely an outcome.
#[test]
fn near_equal_values_are_ordered_exactly() {
    let a = "0.12345678901234567890";
    let b = "0.12345678901234567891";
    let fa: f64 = a.parse().unwrap();
    let fb: f64 = b.parse().unwrap();
    assert_eq!(fa, fb, "precondition: f64 cannot tell these apart");

    let (ua, sa) = bd_parse_decimal_str(a).unwrap();
    let (ub, sb) = bd_parse_decimal_str(b).unwrap();
    assert_eq!(bd_compare_unscaled(&ua, sa, &ub, sb), Ordering::Less);
    assert_eq!(bd_compare_unscaled(&ub, sb, &ua, sa), Ordering::Greater);
    // `compareTo` ignores scale: 2.0 and 2.00 are equal.
    let (u1, s1) = bd_parse_decimal_str("2.0").unwrap();
    let (u2, s2) = bd_parse_decimal_str("2.00").unwrap();
    assert_eq!(bd_compare_unscaled(&u1, s1, &u2, s2), Ordering::Equal);
}

/// **Defect 2, closed.** The old synthetic `divide` computed `1/3` in `f64`
/// and RETURNED `0.3333333333333333`; `BigDecimal.divide(BigDecimal)` must
/// throw for a non-terminating expansion. Through the exact core it does,
/// and a terminating division still returns the exact quotient.
#[test]
fn divide_throws_for_a_non_terminating_expansion_and_is_exact_otherwise() {
    let one = bd_parse_decimal_str("1").unwrap();
    let three = bd_parse_decimal_str("3").unwrap();
    assert!(
        bd_exact_divide_core(&one.0, one.1, &three.0, three.1).is_err(),
        "1/3 does not terminate and must raise, not round"
    );
    let four = bd_parse_decimal_str("4").unwrap();
    let (q, scale) =
        bd_exact_divide_core(&one.0, one.1, &four.0, four.1).expect("1/4 terminates");
    assert_eq!((q.to_decimal(), scale), ("25".into(), 2), "1/4 = 0.25");
    let zero = (BigInt::zero(), 0);
    assert!(
        bd_exact_divide_core(&one.0, one.1, &zero.0, zero.1).is_err(),
        "division by zero raises"
    );
}
