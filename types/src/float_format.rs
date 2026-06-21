//! Java-compatible `Double.toString` / `Float.toString` formatting.
//!
//! Rust's `Display` for `f64`/`f32` produces the shortest round-tripping
//! decimal *digits* — which agree with Java's — but it NEVER switches to
//! scientific notation, so `format!("{}", 1e7)` is `"10000000"` where Java
//! requires `"1.0E7"`. This module reuses Rust's shortest-digit generator (via
//! the `{:e}` formatter, i.e. Ryū) but applies the JLS layout rule:
//!
//! > If `10^-3 <= m < 10^7` the value is written in plain decimal form;
//! > otherwise "computerized scientific notation" `d.dddEexp` is used.
//! > (JLS / `java.lang.Double#toString`.)
//!
//! Both forms always carry at least one fractional digit (`"3.0"`, `"1.0E7"`),
//! and the exponent has no `+` sign and no leading zeros.
//!
//! NOTE: for the smallest subnormals (e.g. `Double.MIN_VALUE`) the JDK emits a
//! *non-shortest* representation (`"4.9E-324"`) while Ryū yields the genuinely
//! shortest `"5.0E-324"`; both round-trip to the same bits. We follow Ryū, so
//! those extreme edge cases differ from HotSpot in their last digit. Every
//! normal-magnitude value matches HotSpot exactly.

/// Format an `f64` exactly as `java.lang.Double.toString(double)` does
/// (modulo the documented smallest-subnormal edge case).
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
    let sci = format!("{:e}", v.abs());
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
    let sci = format!("{:e}", v.abs());
    layout_java(v.is_sign_negative(), &sci)
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
        assert_eq!(java_double_to_string(0.30000000000000004), "0.30000000000000004");
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

    // Round-trip: every formatted normal value must parse back to the same bits.
    #[test]
    fn double_round_trips() {
        for &v in &[
            3.0f64, 1024.0, 0.1, 123.456, 1e7, 1.5e10, 1e-4, 1e300, 1e-300,
            f64::MAX, -7.0, 9999999.0, 6.022e23, 1.602e-19,
        ] {
            let s = java_double_to_string(v);
            let parsed: f64 = s.parse().unwrap_or_else(|_| panic!("unparseable: {s}"));
            assert_eq!(parsed.to_bits(), v.to_bits(), "round-trip failed: {v} -> {s}");
        }
    }
}
