//! Java-compatible `Double.toString` / `Float.toString` formatting.
//!
//! Rust's `Display` for `f64`/`f32` produces the shortest round-tripping
//! decimal *digits*, and it NEVER switches to scientific notation, so
//! `format!("{}", 1e7)` is `"10000000"` where Java requires `"1.0E7"`. This
//! module reuses Rust's shortest-digit generator (via the `{:e}` formatter,
//! i.e. Ryū) but applies the JLS layout rule:
//!
//! > If `10^-3 <= m < 10^7` the value is written in plain decimal form;
//! > otherwise "computerized scientific notation" `d.dddEexp` is used.
//! > (JLS / `java.lang.Double#toString`.)
//!
//! Both forms always carry at least one fractional digit (`"3.0"`, `"1.0E7"`),
//! and the exponent has no `+` sign and no leading zeros.
//!
//! Subnormal values need a small detour: Rust/Ryu can emit shorter strings such
//! as `"5e-324"` for `Double.MIN_VALUE`, while HotSpot/JDK 25 emits
//! `"4.9E-324"`. For subnormals only, we generate fixed-scientific candidates
//! with increasing precision and keep the first one that parses back to the
//! original bits, matching the JDK output shape.
//!
//! # The digits do NOT always agree, and this module's own doc said they did
//!
//! This file used to claim Rust's shortest digits "agree with Java's". They
//! agree on length, and whenever one candidate is strictly closer to the value
//! than any other. They disagree on the TIE: when two shortest decimals are
//! exactly equidistant from `v`, `Double.toString` is specified to pick "the
//! one whose least significant digit is even", and Ryu does not.
//!
//! MEASURED - the double `0x4309_9999_9999_999a` is exactly
//! `900719925474099.25`, and both `900719925474099.2` and `900719925474099.3`
//! round-trip to it:
//!
//! ```text
//!   HotSpot 25   9.007199254740992E14      (even last digit)
//!   Ryu          9.007199254740993E14
//! ```
//!
//! Found via `G10-1`, whose one divergent row was
//! `new BigDecimal(BigInteger.valueOf(9007199254740993L), 1).doubleValue()`.
//! That record attributes it to `doubleValue`; the raw bits are identical to
//! HotSpot's on both VMs, so the conversion was never wrong - the RENDERING
//! was, for every `double` that lands on a tie.

/// Format an `f64` exactly as `java.lang.Double.toString(double)` does.
pub fn java_double_to_string(v: f64) -> String {
    if v.is_nan() {
        return "NaN".to_string();
    }
    if v == f64::INFINITY {
        return "Infinity".to_string();
    }
    if v == f64::NEG_INFINITY {
        return "-Infinity".to_string();
    }
    if v == 0.0 {
        return if v.is_sign_negative() {
            "-0.0".to_string()
        } else {
            "0.0".to_string()
        };
    }
    // Shortest round-tripping digits + base-10 exponent of the leading digit.
    let abs = v.abs();
    if abs < f64::MIN_POSITIVE {
        return layout_subnormal_f64(v.is_sign_negative(), abs);
    }
    let sci = java_shortest_sci_f64(abs);
    layout_java(v.is_sign_negative(), &sci)
}

/// Format an `f32` exactly as `java.lang.Float.toString(float)` does. The JLS
/// layout rule (10^-3 .. 10^7 threshold) is identical to `double`; only the
/// shortest-digit set differs (it is the `f32`-precision one).
pub fn java_float_to_string(v: f32) -> String {
    if v.is_nan() {
        return "NaN".to_string();
    }
    if v == f32::INFINITY {
        return "Infinity".to_string();
    }
    if v == f32::NEG_INFINITY {
        return "-Infinity".to_string();
    }
    if v == 0.0 {
        return if v.is_sign_negative() {
            "-0.0".to_string()
        } else {
            "0.0".to_string()
        };
    }
    let abs = v.abs();
    if abs < f32::MIN_POSITIVE {
        return layout_subnormal_f32(v.is_sign_negative(), abs);
    }
    let sci = java_shortest_sci_f32(abs);
    layout_java(v.is_sign_negative(), &sci)
}

/// Rust's shortest scientific digits for `abs`, with Java's tie-break applied.
///
/// See the module header. The only disagreement with Ryu is the exact tie, and
/// the check is two-stage so the hot path pays one extra `format!` at most:
///
/// 1. Render one digit MORE than the shortest form. An exact tie has the true
///    expansion terminating at `...5`, so if that extra digit is not `'5'` no
///    tie is possible and Ryu's answer stands. This rejects almost everything.
/// 2. Only then render enough digits to see the whole exact expansion, and
///    confirm the tail really is `5` followed by nothing.
fn java_shortest_sci_f64(abs: f64) -> String {
    let sci = format!("{:e}", abs);
    let n = significant_digit_count(&sci);
    if n == 0 {
        return sci;
    }
    if !ends_in_five(&format!("{:.*e}", n, abs)) {
        return sci;
    }
    let exact = format!("{:.*e}", n + 40, abs);
    apply_java_tie_break(&sci, &exact, n).unwrap_or(sci)
}

/// The `f32` twin. Same rule, same two stages, `f32` precision throughout.
fn java_shortest_sci_f32(abs: f32) -> String {
    let sci = format!("{:e}", abs);
    let n = significant_digit_count(&sci);
    if n == 0 {
        return sci;
    }
    if !ends_in_five(&format!("{:.*e}", n, abs)) {
        return sci;
    }
    let exact = format!("{:.*e}", n + 20, abs);
    apply_java_tie_break(&sci, &exact, n).unwrap_or(sci)
}

fn significant_digit_count(sci: &str) -> usize {
    let mantissa = sci.split_once('e').map(|(m, _)| m).unwrap_or(sci);
    mantissa.chars().filter(|c| c.is_ascii_digit()).count()
}

fn ends_in_five(sci: &str) -> bool {
    let mantissa = sci.split_once('e').map(|(m, _)| m).unwrap_or(sci);
    mantissa.chars().filter(|c| c.is_ascii_digit()).next_back() == Some('5')
}

/// Given Ryu's shortest form and an exact expansion of the same value, return
/// the `n`-digit form Java would print, or `None` when there is no exact tie
/// (in which case Ryu's answer is already right).
fn apply_java_tie_break(sci: &str, exact: &str, n: usize) -> Option<String> {
    let (em, ee) = exact.split_once('e')?;
    let exp: i32 = ee.parse().ok()?;
    let digits: String = em.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() <= n {
        return None;
    }
    let (head, tail) = digits.split_at(n);
    // A tie is "exactly one half in the last place": `5`, and then nothing.
    if !tail.starts_with('5') || !tail[1..].bytes().all(|b| b == b'0') {
        return None;
    }
    // The two candidates are `head` and `head + 1`; Java takes the even one.
    let last = head.as_bytes()[n - 1];
    let (chosen, exp) = if (last - b'0') % 2 == 0 {
        (head.to_string(), exp)
    } else {
        let (bumped, carried) = increment_decimal(head);
        (bumped, if carried { exp + 1 } else { exp })
    };
    let rebuilt = if chosen.len() > 1 {
        format!("{}.{}e{}", &chosen[..1], &chosen[1..], exp)
    } else {
        format!("{chosen}e{exp}")
    };
    if rebuilt == sci {
        None
    } else {
        Some(rebuilt)
    }
}

/// Add one to a decimal digit string, keeping its LENGTH. Returns the new
/// digits and whether it carried out of the top (`"999"` -> `("100", true)`);
/// the caller absorbs that carry into the exponent.
fn increment_decimal(digits: &str) -> (String, bool) {
    let mut out: Vec<u8> = digits.as_bytes().to_vec();
    for i in (0..out.len()).rev() {
        if out[i] == b'9' {
            out[i] = b'0';
        } else {
            out[i] += 1;
            return (
                String::from_utf8(out).unwrap_or_else(|_| digits.to_string()),
                false,
            );
        }
    }
    out[0] = b'1';
    (
        String::from_utf8(out).unwrap_or_else(|_| digits.to_string()),
        true,
    )
}

fn layout_subnormal_f64(negative: bool, abs: f64) -> String {
    debug_assert!(abs > 0.0 && abs < f64::MIN_POSITIVE);
    for fractional_digits in 1..=17 {
        let sci = format!("{:.*e}", fractional_digits, abs);
        let candidate = layout_fixed_scientific(false, &sci);
        if candidate
            .parse::<f64>()
            .map(|parsed| parsed.to_bits() == abs.to_bits())
            .unwrap_or(false)
        {
            return if negative {
                format!("-{candidate}")
            } else {
                candidate
            };
        }
    }
    layout_java(negative, &format!("{:.17e}", abs))
}

fn layout_subnormal_f32(negative: bool, abs: f32) -> String {
    debug_assert!(abs > 0.0 && abs < f32::MIN_POSITIVE);
    for fractional_digits in 1..=9 {
        let sci = format!("{:.*e}", fractional_digits, abs);
        let candidate = layout_fixed_scientific(false, &sci);
        if candidate
            .parse::<f32>()
            .map(|parsed| parsed.to_bits() == abs.to_bits())
            .unwrap_or(false)
        {
            return if negative {
                format!("-{candidate}")
            } else {
                candidate
            };
        }
    }
    layout_java(negative, &format!("{:.9e}", abs))
}

fn layout_fixed_scientific(negative: bool, sci: &str) -> String {
    let (mantissa, exp_str) = sci.split_once('e').expect("Rust {:e} always has 'e'");
    let exp: i32 = exp_str.parse().expect("Rust {:e} exponent is an integer");
    let mut mantissa = mantissa.to_string();

    if let Some(dot) = mantissa.find('.') {
        while mantissa.ends_with('0') && mantissa.len() > dot + 2 {
            mantissa.pop();
        }
    } else {
        mantissa.push_str(".0");
    }

    let mut out = String::new();
    if negative {
        out.push('-');
    }
    out.push_str(&mantissa);
    out.push('E');
    out.push_str(&exp.to_string());
    out
}

/// Shared layout from Rust's `{:e}` scientific string (`"d"` or `"d.ddd"`,
/// then `'e'`, then a signed exponent) to Java's `toString` form.
fn layout_java(negative: bool, sci: &str) -> String {
    // `sci` is e.g. "1e0", "1.024e3", "9.999999e6", "4.9e-324".
    let (mantissa, exp_str) = sci.split_once('e').expect("Rust {:e} always has 'e'");
    let exp: i32 = exp_str.parse().expect("Rust {:e} exponent is an integer");
    // Significant digits, decimal point removed. `value == digits[0].digits[1..]
    // * 10^exp`, so the leading digit has place value 10^exp.
    let digits: String = mantissa.chars().filter(|&c| c != '.').collect();

    let mut out = String::new();
    if negative {
        out.push('-');
    }

    // JLS: plain decimal iff 10^-3 <= |v| < 10^7, i.e. -3 <= exp <= 6.
    if (-3..=6).contains(&exp) {
        if exp >= 0 {
            let int_len = (exp + 1) as usize;
            if digits.len() <= int_len {
                // All significant digits are in the integer part; pad with
                // trailing zeros and add the required ".0".
                out.push_str(&digits);
                for _ in 0..(int_len - digits.len()) {
                    out.push('0');
                }
                out.push_str(".0");
            } else {
                out.push_str(&digits[..int_len]);
                out.push('.');
                out.push_str(&digits[int_len..]);
            }
        } else {
            // exp in -3..=-1: "0." then (-exp-1) leading zeros then the digits.
            out.push_str("0.");
            for _ in 0..(-exp - 1) {
                out.push('0');
            }
            out.push_str(&digits);
        }
    } else {
        // Scientific: leading digit, '.', remaining digits (or "0"), 'E', exp.
        out.push_str(&digits[..1]);
        out.push('.');
        if digits.len() > 1 {
            out.push_str(&digits[1..]);
        } else {
            out.push('0');
        }
        out.push('E');
        out.push_str(&exp.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every assertion is the exact output of HotSpot JDK 25
    // `Double.toString` / `Float.toString`.
    #[test]
    fn double_plain_decimal() {
        assert_eq!(java_double_to_string(3.0), "3.0");
        assert_eq!(java_double_to_string(-7.0), "-7.0");
        assert_eq!(java_double_to_string(1024.0), "1024.0");
        assert_eq!(java_double_to_string(100.0), "100.0");
        assert_eq!(java_double_to_string(10.0), "10.0");
        assert_eq!(java_double_to_string(123.456), "123.456");
        assert_eq!(java_double_to_string(0.1), "0.1");
        assert_eq!(java_double_to_string(0.001), "0.001");
        assert_eq!(java_double_to_string(0.5), "0.5");
        assert_eq!(
            java_double_to_string(0.30000000000000004),
            "0.30000000000000004"
        );
        assert_eq!(java_double_to_string(1.0 / 3.0), "0.3333333333333333");
        assert_eq!(java_double_to_string(9999999.0), "9999999.0"); // < 10^7 → plain
        assert_eq!(java_double_to_string(1000000.0), "1000000.0");
        assert_eq!(java_double_to_string(1234.5), "1234.5");
    }

    #[test]
    fn double_scientific() {
        assert_eq!(java_double_to_string(1e7), "1.0E7"); // == 10^7 → scientific
        assert_eq!(java_double_to_string(10000000.0), "1.0E7");
        assert_eq!(java_double_to_string(1.5e10), "1.5E10");
        assert_eq!(java_double_to_string(1e-4), "1.0E-4");
        assert_eq!(java_double_to_string(1e300), "1.0E300");
        assert_eq!(java_double_to_string(1e-300), "1.0E-300");
        assert_eq!(java_double_to_string(f64::MAX), "1.7976931348623157E308");
    }

    #[test]
    fn double_subnormal_jdk25_cases() {
        assert_eq!(java_double_to_string(f64::from_bits(1)), "4.9E-324");
        assert_eq!(java_double_to_string(f64::from_bits(2)), "9.9E-324");
        assert_eq!(java_double_to_string(f64::from_bits(3)), "1.5E-323");
        assert_eq!(java_double_to_string(f64::from_bits(10)), "4.9E-323");
        assert_eq!(
            java_double_to_string(f64::from_bits(0x000f_ffff_ffff_ffff)),
            "2.225073858507201E-308"
        );
        assert_eq!(java_double_to_string(-f64::from_bits(1)), "-4.9E-324");
    }

    #[test]
    fn double_specials() {
        assert_eq!(java_double_to_string(f64::NAN), "NaN");
        assert_eq!(java_double_to_string(f64::INFINITY), "Infinity");
        assert_eq!(java_double_to_string(f64::NEG_INFINITY), "-Infinity");
        assert_eq!(java_double_to_string(0.0), "0.0");
        assert_eq!(java_double_to_string(-0.0), "-0.0");
    }

    #[test]
    fn float_cases() {
        assert_eq!(java_float_to_string(3.0), "3.0");
        assert_eq!(java_float_to_string(1e8), "1.0E8");
        assert_eq!(java_float_to_string(1e-4), "1.0E-4");
        assert_eq!(java_float_to_string(0.1), "0.1");
        assert_eq!(java_float_to_string(1.0 / 3.0), "0.33333334");
        assert_eq!(java_float_to_string(f32::INFINITY), "Infinity");
        assert_eq!(java_float_to_string(f32::NEG_INFINITY), "-Infinity");
        assert_eq!(java_float_to_string(f32::NAN), "NaN");
        assert_eq!(java_float_to_string(-0.0), "-0.0");
        assert_eq!(java_float_to_string(123.456), "123.456");
    }

    #[test]
    fn float_subnormal_jdk25_cases() {
        assert_eq!(java_float_to_string(f32::from_bits(1)), "1.4E-45");
        assert_eq!(java_float_to_string(f32::from_bits(2)), "2.8E-45");
        assert_eq!(java_float_to_string(f32::from_bits(3)), "4.2E-45");
        assert_eq!(java_float_to_string(f32::from_bits(10)), "1.4E-44");
        assert_eq!(
            java_float_to_string(f32::from_bits(0x007f_ffff)),
            "1.1754942E-38"
        );
        assert_eq!(java_float_to_string(-f32::from_bits(1)), "-1.4E-45");
    }

    // Round-trip: every formatted normal value must parse back to the same bits.
    #[test]
    fn double_round_trips() {
        for &v in &[
            3.0f64,
            1024.0,
            0.1,
            123.456,
            1e7,
            1.5e10,
            1e-4,
            1e300,
            1e-300,
            f64::MAX,
            -7.0,
            9999999.0,
            6.022e23,
            1.602e-19,
        ] {
            let s = java_double_to_string(v);
            let parsed: f64 = s.parse().unwrap_or_else(|_| panic!("unparseable: {s}"));
            assert_eq!(
                parsed.to_bits(),
                v.to_bits(),
                "round-trip failed: {v} -> {s}"
            );
        }
    }

    /// The exact tie Ryu and Java break differently.
    ///
    /// `0x4309_9999_9999_999a` is exactly `900719925474099.25`. Both
    /// `...099.2` and `...099.3` round-trip to it, so both are "shortest";
    /// `Double.toString` is specified to take the one whose least significant
    /// digit is EVEN. Ryu takes the other one.
    ///
    /// Every expected string here is the exact output of HotSpot JDK 25.
    #[test]
    fn double_exact_ties_round_half_to_even_like_java() {
        let v = f64::from_bits(0x4309_9999_9999_999a);
        // The premise: this really is a tie, and Ryu really does disagree.
        assert_eq!(format!("{:e}", v), "9.007199254740993e14", "Ryu's answer");
        assert_eq!(
            "900719925474099.2".parse::<f64>().unwrap().to_bits(),
            v.to_bits(),
            "the even candidate must round-trip, or it is not a tie"
        );
        assert_eq!(
            "900719925474099.3".parse::<f64>().unwrap().to_bits(),
            v.to_bits(),
            "the odd candidate must round-trip too, or it is not a tie"
        );
        assert_eq!(java_double_to_string(v), "9.007199254740992E14");
        assert_eq!(java_double_to_string(-v), "-9.007199254740992E14");
    }

    /// The control the fix must not break: values that are NOT ties keep Ryu's
    /// digits exactly. If the tie-break ever fires on these, it is guessing.
    #[test]
    fn non_ties_are_left_alone() {
        for (bits, expected) in [
            (0x4340_0000_0000_0000u64, "9.007199254740992E15"),
            (0x3ff0_0000_0000_0001, "1.0000000000000002"),
            (0x3ff0_0000_0000_0000, "1.0"),
            (0x4059_0000_0000_0000, "100.0"),
            (0x7fef_ffff_ffff_ffff, "1.7976931348623157E308"),
            (0x0010_0000_0000_0000, "2.2250738585072014E-308"),
        ] {
            let v = f64::from_bits(bits);
            assert_eq!(java_double_to_string(v), expected, "bits {bits:#018x}");
        }
    }

    /// Whatever the tie-break picks must still parse back to the same double.
    /// This is the property that makes the change unable to be *wrong*, only
    /// differently-spelled, and it is checked over a wide sweep rather than
    /// the handful of literals above.
    #[test]
    fn tie_break_never_breaks_the_round_trip() {
        let mut bits: u64 = 0x4009_2179_2143_7f11;
        for _ in 0..20_000 {
            // A cheap deterministic walk over a wide range of exponents.
            bits = bits.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let v = f64::from_bits(bits);
            if !v.is_finite() || v == 0.0 {
                continue;
            }
            let s = java_double_to_string(v);
            let back: f64 = s.parse().expect(&s);
            assert_eq!(back.to_bits(), v.to_bits(), "{s} did not round-trip");
        }
    }

    /// `increment_decimal` has to keep the digit COUNT and report the carry,
    /// because the caller absorbs it into the exponent. The all-nines row is
    /// the one that would silently lengthen the mantissa.
    #[test]
    fn increment_decimal_keeps_its_width() {
        assert_eq!(increment_decimal("1234"), ("1235".to_string(), false));
        assert_eq!(increment_decimal("1239"), ("1240".to_string(), false));
        assert_eq!(increment_decimal("999"), ("100".to_string(), true));
        assert_eq!(increment_decimal("9"), ("1".to_string(), true));
    }
}
