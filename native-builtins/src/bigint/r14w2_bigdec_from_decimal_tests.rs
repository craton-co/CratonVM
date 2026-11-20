use super::*;

fn lcg(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *state >> 11
}

fn digits(state: &mut u64, len: usize, zero_bias: bool) -> String {
    (0..len)
        .map(|_| {
            let r = lcg(state);
            if zero_bias && r % 3 == 0 {
                '0'
            } else if zero_bias && r % 3 == 1 {
                '9'
            } else {
                char::from(b'0' + (r % 10) as u8)
            }
        })
        .collect()
}

fn reference(s: &str) -> BigInt {
    let (neg, d) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    BigInt::from_decimal_digitwise(neg, d)    }

fn check(s: &str) {
    let want = reference(s);
    let (neg, d) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    for base in [9usize, 10, 17, 18, 27, 64, FROM_DECIMAL_DC_BASE_DIGITS] {
        assert_eq!(
            BigInt::from_decimal_fast(neg, d.as_bytes(), base),
            want,
            "{s:?} base {base}"
        );
    }
    assert_eq!(BigInt::from_decimal(s), want, "{s:?} shipping");
}

#[test]
fn fast_parse_matches_the_digit_loop_on_every_length() {
    let mut st = 0xb16_dec_14_u64;
    for len in (0..160usize).chain((160..400).step_by(7)) {
        for zero_bias in [false, true] {
            let d = digits(&mut st, len, zero_bias);
            check(&d);
            check(&format!("-{d}"));
            check(&format!("+{d}"));
            check(&format!("000{d}"));
        }
    }
}

#[test]
fn fast_parse_edge_forms() {
    for s in [
        "",
        "-",
        "+",
        "0",
        "-0",
        "+0",
        "00000000000000000000000000000",
        "-000000000000000000000000000",
        "999999999",
        "1000000000",
        "999999999999999999",
        "1000000000000000000",
        "18446744073709551615",
        "18446744073709551616",
        "-18446744073709551616",
        "1000000000000000000000000000000000000000000000000000000000000000000000000000000",
        // Non-digits are skipped by both roads (callers pass "1.5"-shaped text).
        "1.5",
        "-12345678901234567890.0987654321",
        "12_345_678_901_234_567_890_123",
        "--5",
        "1e30",
        "١٢٣٤٥٦٧٨٩٠١٢٣٤٥٦٧٨٩٠١٢٣",
        "12345678901234567890١23",
    ] {
        check(s);
    }
    // Powers of ten and their neighbours straddle every chunk and split boundary.
    for n in (1..160usize).chain((160..300).step_by(9)) {
        let p = format!("1{}", "0".repeat(n));
        let m = "9".repeat(n);
        check(&p);
        check(&m);
        check(&format!("-{p}"));
        check(&format!("{}1", "0".repeat(n)));
    }
}

#[test]
fn fast_parse_at_the_shipping_threshold_round_trips() {
    let mut st = 0x2000_d161_u64;
    for len in [
        1151usize, 1152, 1153, 2303, 2304, 2305, 5000, 12_345, 40_000,
    ] {
        let mut d = digits(&mut st, len, false);
        if d.starts_with('0') {
            d.replace_range(0..1, "7");
        }
        let fast = BigInt::from_decimal(&d);
        assert_eq!(fast, reference(&d), "len {len}");
        assert_eq!(fast.to_decimal(), d, "round trip, len {len}");
        let neg = BigInt::from_decimal(&format!("-{d}"));
        assert_eq!(neg, fast.neg_value(), "negative, len {len}");
    }
}
