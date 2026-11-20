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

/// A power of two has a rounding interval twice as wide above as below,
/// so the even candidate of an exact tie can fall outside it. The random
/// walk above never lands on one (a zero mantissa is 1 in 2^52).
/// HotSpot 25: `Double.toString(0x1p-24)` is `5.960464477539063E-8`.
#[test]
fn a_power_of_two_tie_keeps_the_candidate_that_round_trips() {
    assert_eq!(
        java_double_to_string(f64::from_bits(0x3e70_0000_0000_0000)),
        "5.960464477539063E-8"
    );
    for e in -1022..=1023 {
        let v = 2f64.powi(e);
        let s = java_double_to_string(v);
        assert_eq!(
            s.parse::<f64>().expect(&s).to_bits(),
            v.to_bits(),
            "2^{e}: {s}"
        );
    }
    for e in -126..=127 {
        let v = 2f32.powi(e);
        let s = java_float_to_string(v);
        assert_eq!(
            s.parse::<f32>().expect(&s).to_bits(),
            v.to_bits(),
            "2^{e}: {s}"
        );
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
