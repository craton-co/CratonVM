use super::bi_to_radix_string;
use crate::bigint::BigInt;
use crate::java_radix_or_ten;

fn s(dec: &str, radix: u32) -> String {
    bi_to_radix_string(&BigInt::from_decimal(dec), radix)
}

/// The regression this file's `toString(int)` actually had: negatives were
/// rendered with `format!("{:x}", i128)`, i.e. the two's-complement bit
/// pattern. The JDK renders SIGN-MAGNITUDE.
#[test]
fn negatives_are_sign_magnitude_never_twos_complement() {
    assert_eq!(s("-1", 16), "-1");
    assert_eq!(s("-1", 2), "-1");
    assert_eq!(s("-1", 8), "-1");
    assert_eq!(s("-255", 16), "-ff");
    assert_eq!(s("-5", 2), "-101");
    assert_eq!(s("-255", 36), "-73");
    // The exact shape the old body produced, spelled out so a revert is
    // unmistakable rather than merely "not equal".
    assert_ne!(s("-1", 16), "ffffffffffffffffffffffffffffffff");
    assert_ne!(s("-1", 16), "ffffffff");
}

/// Radices other than 2/8/16 used to fall through to the DECIMAL string.
#[test]
fn every_legal_radix_is_honoured_not_just_two_eight_sixteen() {
    assert_eq!(s("255", 3), "100110");
    assert_eq!(s("255", 36), "73");
    assert_eq!(s("255", 16), "ff");
    assert_eq!(s("255", 8), "377");
    assert_eq!(s("255", 2), "11111111");
    assert_eq!(s("255", 10), "255");
    // Both boundary radices.
    assert_eq!(s("5", 2), "101");
    assert_eq!(s("5", 36), "5");
}

/// The old body narrowed through `i128` with `.unwrap_or(0)`, so anything
/// past `i128::MAX` silently answered "0".
#[test]
fn values_past_i128_keep_every_digit() {
    // 2^200 + 7
    let huge = "1606938044258990275541962092341162602522202993782792835301383";
    assert_eq!(
        s(huge, 16),
        "100000000000000000000000000000000000000000000000007"
    );
    assert_eq!(s(huge, 2).len(), 201);
    assert_eq!(s(huge, 10), huge);
    let big = "123456789012345678901234567890";
    assert_eq!(s(big, 16), "18ee90ff6c373e0ee4e3f0ad2");
    assert_eq!(s(big, 36), "byw97um9s91dlz68tsi");
    assert_eq!(s(big, 8), "143564417755415637016711617605322");
    assert_eq!(s(&format!("-{big}"), 16), "-18ee90ff6c373e0ee4e3f0ad2");
    assert_eq!(s(&format!("-{big}"), 36), "-byw97um9s91dlz68tsi");
}

/// `Long.MIN_VALUE` has no positive counterpart — the classic overflow
/// trap. As a `BigInteger` it is just another magnitude, so assert it.
#[test]
fn long_min_value_and_zero() {
    assert_eq!(s("-9223372036854775808", 16), "-8000000000000000");
    assert_eq!(s("-9223372036854775808", 36), "-1y2p0ij32e8e8");
    assert_eq!(
        s("-9223372036854775808", 2),
        "-1000000000000000000000000000000000000000000000000000000000000000"
    );
    assert_eq!(s("9223372036854775807", 36), "1y2p0ij32e8e7");
    for r in 2..=36u32 {
        assert_eq!(s("0", r), "0", "zero in radix {r}");
    }
}

/// Interior chunks must be zero-padded to the full chunk width; only the
/// most significant chunk may be short. A padding bug shows up as digits
/// going missing in the middle of a long rendering.
#[test]
fn chunk_padding_round_trips_through_decimal() {
    for r in 2..=36u32 {
        for dec in [
            "1606938044258990275541962092341162602522202993782792835301383",
            "123456789012345678901234567890",
            "4294967296",
            "18446744073709551616",
            "-18446744073709551616",
        ] {
            let rendered = bi_to_radix_string(&BigInt::from_decimal(dec), r);
            let (neg, abs) = match rendered.strip_prefix('-') {
                Some(rest) => (true, rest),
                None => (false, rendered.as_str()),
            };
            let mut acc = BigInt::zero();
            let base = BigInt::from_le_words(false, vec![r]);
            for ch in abs.chars() {
                let d = ch.to_digit(r).expect("digit in range");
                acc = acc.mul(&base).add(&BigInt::from_le_words(false, vec![d]));
            }
            if neg {
                acc = acc.neg_value();
            }
            assert_eq!(acc.to_decimal(), dec, "radix {r} round-trip of {dec}");
        }
    }
}

/// `toString(int)` IGNORES an out-of-range radix and uses 10 — it does not
/// throw and it does not clamp to 2/36. Measured on real JDK 25.
#[test]
fn out_of_range_radix_substitutes_ten() {
    for bad in [0, 1, -1, 37, 40, i32::MIN, i32::MAX] {
        assert_eq!(java_radix_or_ten(bad), 10, "radix {bad}");
        assert_eq!(s("255", java_radix_or_ten(bad)), "255", "radix {bad}");
        assert_eq!(s("-255", java_radix_or_ten(bad)), "-255", "radix {bad}");
    }
    assert_eq!(java_radix_or_ten(2), 2);
    assert_eq!(java_radix_or_ten(36), 36);
}
