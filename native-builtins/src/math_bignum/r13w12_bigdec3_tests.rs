use super::{
    bd_compare_by_bits, bd_compare_unscaled, bd_to_f32, bd_to_f32_binary, bd_to_f64,
    bd_to_f64_binary, bigint_decimal_digit_count, bigint_pow10, native_bd_double_value,
    native_bd_double_value_binary, register_bigdecimal_arithmetic_overrides,
};
use crate::bigint::BigInt;
use cratonvm_native_api::{NativeKind, NativeMethodRegistry};

fn lcg(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    *state ^ (*state >> 29)
}

/// `m * 2^e` as the exact `(unscaled, scale)` pair `new BigDecimal(double)`
/// builds for it.
fn exact(m: u64, e: i32) -> (BigInt, i32) {
    let mag = BigInt::from_le_words(false, vec![m as u32, (m >> 32) as u32]);
    if e >= 0 {
        (mag.shl(e as u32), 0)
    } else {
        (mag.mul(&crate::bigint_pow5(e.unsigned_abs())), -e)
    }
}

fn check64(u: &BigInt, s: i32) -> f64 {
    let want = bd_to_f64(u, s);
    let got = bd_to_f64_binary(u, s);
    assert_eq!(
        got.to_bits(),
        want.to_bits(),
        "doubleValue of {} scale {s}: binary {got:e}, reference {want:e}",
        u.to_decimal()
    );
    got
}

fn check32(u: &BigInt, s: i32) -> f32 {
    let want = bd_to_f32(u, s);
    let got = bd_to_f32_binary(u, s);
    assert_eq!(
        got.to_bits(),
        want.to_bits(),
        "floatValue of {} scale {s}: binary {got:e}, reference {want:e}",
        u.to_decimal()
    );
    got
}

/// The HotSpot-measured rows of `double_value_matches_hotspot` /
/// `float_value_matches_hotspot`, on the binary road.
#[test]
fn binary_road_answers_the_measured_rows() {
    let d = |u: &str, s: i32| bd_to_f64_binary(&BigInt::from_decimal(u), s);
    assert_eq!(d("1", i32::MIN), f64::INFINITY);
    assert_eq!(d("-1", i32::MIN), f64::NEG_INFINITY);
    assert_eq!(d("1", i32::MIN + 1), f64::INFINITY);
    assert_eq!(d("1", i32::MAX), 0.0);
    assert!(d("-1", i32::MAX).is_sign_negative());
    assert!(d("0", i32::MIN).is_sign_positive());
    assert!(d("0", 324).is_sign_positive());
    assert_eq!(d("1", -308), 1.0e308);
    assert_eq!(d("1", -309), f64::INFINITY);
    assert_eq!(d("15", -308), f64::INFINITY);
    assert_eq!(d("17976931348623157", -292), 1.7976931348623157e308);
    assert_eq!(d("17976931348623159", -292), f64::INFINITY);
    assert_eq!(d("1", 323), 9.9e-324);
    assert_eq!(d("1", 324), 0.0);
    assert_eq!(d("15", 324), 1.5e-323);
    assert_eq!(d("49", 326), 0.0);
    assert_eq!(d("1", 325), 0.0);
    assert!(d("-1", 324).is_sign_negative());
    assert_eq!(d("1", 1), 0.1);
    assert_eq!(d("1", -1), 10.0);
    assert_eq!(d("-15", -1), -150.0);
    assert_eq!(d("9007199254740993", 0), 9.007199254740992e15);
    assert_eq!(d("9007199254740993", 324), 9.007199254740994e-309);
    let f = |u: &str, s: i32| bd_to_f32_binary(&BigInt::from_decimal(u), s);
    assert_eq!(f("1", i32::MIN), f32::INFINITY);
    assert_eq!(f("1", i32::MAX), 0.0);
    assert!(f("-1", i32::MAX).is_sign_negative());
    assert_eq!(f("1", -308), f32::INFINITY);
    assert!(f("-1", 308).is_sign_negative());
    assert_eq!(f("1", 1), 0.1f32);
    assert_eq!(f("-15", -1), -150.0f32);
    assert_eq!(f("9007199254740993", 1), 9.0071994e14f32);
}

/// Every double's exact value, the exact midpoint to its successor (the
/// tie, which must go to even) and the two values one unit of a further
/// decimal place either side of that midpoint (which must not), normals
/// and subnormals, both formats, against the reference.
#[test]
fn binary_road_rounds_exact_values_and_ties_like_the_reference() {
    let mut st = 0x5eed_b16d_ec03u64;
    let mut checked = 0usize;
    for i in 0..6000u32 {
        let bits: u64 = match i % 4 {
            // Anywhere in the finite range.
            0 => lcg(&mut st) % 0x7fef_ffff_ffff_ffff + 1,
            // Subnormal and just above.
            1 => lcg(&mut st) % 0x0020_0000_0000_0000 + 1,
            // Near the top of the range.
            2 => 0x7fef_ffff_ffff_ffff - lcg(&mut st) % 0x0010_0000_0000_0000,
            // Small integers and simple fractions: the fast path's domain.
            _ => ((lcg(&mut st) % 1_000_000 + 1) as f64 / 1024.0).to_bits(),
        };
        let biased = ((bits >> 52) & 0x7ff) as i32;
        let frac = bits & ((1u64 << 52) - 1);
        let (sig, exp) = if biased == 0 {
            (frac, -1074)
        } else {
            (frac | (1 << 52), biased - 1075)
        };
        if sig == 0 {
            continue;
        }
        let (u, s) = exact(sig, exp);
        assert_eq!(check64(&u, s).to_bits(), f64::from_bits(bits).to_bits());
        check32(&u, s);
        let neg = u.neg_value();
        assert_eq!(check64(&neg, s), -f64::from_bits(bits));
        let (mid, ms) = exact(2 * sig + 1, exp - 1);
        let at = check64(&mid, ms);
        check32(&mid, ms);
        let lower = f64::from_bits(bits);
        let (up_u, up_s) = exact(2 * sig + 2, exp - 1);
        let upper = check64(&up_u, up_s);
        let even = if sig & 1 == 0 { lower } else { upper };
        assert_eq!(at.to_bits(), even.to_bits(), "tie of {lower:e}");
        let ten = BigInt::from_decimal("10");
        let one = BigInt::from_decimal("1");
        let above = mid.mul(&ten).add(&one);
        let below = mid.mul(&ten).sub(&one);
        let (a, b) = (check64(&above, ms + 1), check64(&below, ms + 1));
        assert_eq!(a.to_bits(), upper.to_bits(), "above tie of {lower:e}");
        assert_eq!(b.to_bits(), lower.to_bits(), "below tie of {lower:e}");
        check32(&above, ms + 1);
        check32(&below, ms + 1);
        checked += 1;
    }
    assert!(checked > 5000, "vacuous: {checked}");
}

/// `float` midpoints: a tie at `f32` precision goes to even at `f32`
/// precision (a road through `f64` would round twice).
#[test]
fn float_road_rounds_its_own_ties() {
    let mut st = 0xf10a7u64;
    for i in 0..4000u32 {
        let bits = match i % 3 {
            0 => (lcg(&mut st) % 0x7f7f_ffff) as u32 + 1,
            1 => (lcg(&mut st) % 0x0100_0000) as u32 + 1,
            _ => 0x7f7f_ffff - (lcg(&mut st) % 0x0080_0000) as u32,
        };
        let biased = ((bits >> 23) & 0xff) as i32;
        let frac = u64::from(bits & ((1u32 << 23) - 1));
        let (sig, exp) = if biased == 0 {
            (frac, -149)
        } else {
            (frac | (1 << 23), biased - 150)
        };
        if sig == 0 {
            continue;
        }
        let (u, s) = exact(sig, exp);
        assert_eq!(check32(&u, s).to_bits(), bits);
        check64(&u, s);
        let (mid, ms) = exact(2 * sig + 1, exp - 1);
        let at = check32(&mid, ms);
        check64(&mid, ms);
        let lower = f32::from_bits(bits);
        let (up_u, up_s) = exact(2 * sig + 2, exp - 1);
        let upper = check32(&up_u, up_s);
        let even = if sig & 1 == 0 { lower } else { upper };
        assert_eq!(at.to_bits(), even.to_bits(), "f32 tie of {lower:e}");
    }
}

/// Random operands of 1-40 limbs over a scale window that crosses both
/// clamps, plus the scale extremes.
#[test]
fn binary_road_matches_the_reference_on_random_operands() {
    let mut st = 0xb16_dec3u64;
    let mut checked = 0usize;
    for i in 0..6000u32 {
        let widest = if i % 10 == 0 { 40 } else { 4 };
        let limbs = 1 + (lcg(&mut st) % widest) as usize;
        let words: Vec<u32> = (0..limbs).map(|_| lcg(&mut st) as u32).collect();
        let u = BigInt::from_le_words(i % 3 == 0, words);
        if u.is_zero() {
            continue;
        }
        let digits = (u.magnitude_bits() as f64 * std::f64::consts::LOG10_2) as i64;
        let s = match i % 5 {
            0 => (lcg(&mut st) % 61) as i64 - 30,
            1 => digits + 290 + (lcg(&mut st) % 50) as i64,
            2 => digits - 330 + (lcg(&mut st) % 50) as i64,
            3 => digits + 30 + (lcg(&mut st) % 30) as i64,
            _ => digits - 50 + (lcg(&mut st) % 30) as i64,
        };
        let s = s as i32;
        check64(&u, s);
        check32(&u, s);
        checked += 1;
    }
    let scales = [
        i32::MIN,
        i32::MIN + 1,
        -400,
        -23,
        -22,
        0,
        22,
        23,
        400,
        i32::MAX - 1,
        i32::MAX,
    ];
    let edges = [
        "1",
        "-1",
        "4503599627370495",
        "4503599627370496",
        "4194303",
        "4194304",
        "-9007199254740993",
    ];
    for s in scales {
        for u in edges {
            check64(&BigInt::from_decimal(u), s);
            check32(&BigInt::from_decimal(u), s);
        }
    }
    assert!(checked > 5000, "vacuous: {checked}");
}

/// A 20 000-digit value divided down to the double range is one division,
/// not a 20 000-digit rendering; the answer still matches the reference.
#[test]
fn binary_road_handles_wide_values() {
    let wide = crate::bigint_pow5(28_000).mul(&BigInt::from_decimal("7"));
    let digits = (wide.magnitude_bits() as f64 * std::f64::consts::LOG10_2) as i32;
    for s in [
        digits - 300,
        digits - 1,
        digits,
        digits + 1,
        digits + 300,
        digits + 330,
    ] {
        check64(&wide, s);
        check32(&wide, s);
    }
}

/// The in-register digit count of one- and two-limb magnitudes is the
/// rendered length, at every power-of-ten boundary and at `u64::MAX`.
#[test]
fn small_digit_count_matches_the_rendering() {
    let mut values: Vec<u64> = vec![1, 9, u64::MAX, u64::MAX - 1, u64::from(u32::MAX), 1 << 32];
    let mut p = 1u64;
    while let Some(next) = p.checked_mul(10) {
        values.extend([p - 1, p, p + 1]);
        p = next;
    }
    values.extend([p - 1, p, p + 1]);
    for v in values {
        for neg in [false, true] {
            let b = BigInt::from_le_words(neg, vec![v as u32, (v >> 32) as u32]);
            if b.is_zero() {
                continue;
            }
            let want = b.to_decimal().trim_start_matches('-').len() as i32;
            assert_eq!(bigint_decimal_digit_count(&b), want, "{v} neg={neg}");
        }
    }
    // Three limbs take the bit-length bracket or the power-of-ten compare.
    let wide = BigInt::from_decimal("100000000000000000000");
    assert_eq!(bigint_decimal_digit_count(&wide), 21);
    assert_eq!(bigint_decimal_digit_count(&BigInt::zero()), 1);
}

/// The bit-length bracket of wide magnitudes: every power of ten and its
/// neighbours up to 10^1500, and random widths, against the rendering.
#[test]
fn wide_digit_count_matches_the_rendering() {
    let one = BigInt::from_decimal("1");
    for k in 19..1500u32 {
        let p = bigint_pow10(k);
        for v in [p.sub(&one), p.clone(), p.add(&one)] {
            let want = v.to_decimal().len() as i32;
            assert_eq!(bigint_decimal_digit_count(&v), want, "10^{k} neighbour");
            assert_eq!(
                bigint_decimal_digit_count(&v.neg_value()),
                want,
                "-10^{k} neighbour"
            );
        }
    }
    let mut st = 0xd161_75u64;
    for _ in 0..3000 {
        let limbs = 3 + (lcg(&mut st) % 120) as usize;
        let mut words: Vec<u32> = (0..limbs).map(|_| lcg(&mut st) as u32).collect();
        // Vary the top limb's width too.
        let top = (lcg(&mut st) % 32) as u32;
        if let Some(w) = words.last_mut() {
            *w = (*w >> top).max(1);
        }
        let v = BigInt::from_le_words(false, words);
        assert_eq!(bigint_decimal_digit_count(&v), v.to_decimal().len() as i32);
    }
}

/// `compareTo` from bit lengths never disagrees with the exact road, and
/// decides most operand pairs of different magnitudes.
#[test]
fn compare_by_bits_agrees_with_the_exact_road() {
    let mut st = 0xc0a1_bb175u64;
    let (mut decided, mut total) = (0usize, 0usize);
    for i in 0..20_000u32 {
        let widest: u64 = if i % 7 == 0 { 30 } else { 3 };
        let operand = |st: &mut u64| {
            let limbs = 1 + (lcg(st) % widest) as usize;
            let words: Vec<u32> = (0..limbs)
                .map(|_| (lcg(st) as u32) >> (lcg(st) % 32))
                .collect();
            let v = BigInt::from_le_words(lcg(st) % 5 == 0, words);
            let s = (lcg(st) % 801) as i32 - 400;
            (v, s)
        };
        let (ua, sa) = operand(&mut st);
        let (ub, sb) = operand(&mut st);
        total += 1;
        if let Some(o) = bd_compare_by_bits(&ua, sa, &ub, sb) {
            decided += 1;
            assert_eq!(
                o,
                bd_compare_unscaled(&ua, sa, &ub, sb),
                "{}e-{sa} vs {}e-{sb}",
                ua.to_decimal(),
                ub.to_decimal()
            );
        }
    }
    // Scale extremes: the interval arithmetic is in f64, never i32.
    let one = BigInt::from_decimal("1");
    let neg = BigInt::from_decimal("-7");
    for (sa, sb) in [
        (i32::MIN, i32::MAX),
        (i32::MAX, i32::MIN),
        (i32::MIN, 0),
        (0, i32::MAX),
    ] {
        for (a, b) in [(&one, &one), (&neg, &neg)] {
            if let Some(o) = bd_compare_by_bits(a, sa, b, sb) {
                assert_eq!(o, bd_compare_unscaled(a, sa, b, sb), "{sa} vs {sb}");
            }
        }
    }
    assert!(
        decided * 3 > total,
        "compare_by_bits decided only {decided} of {total}"
    );
}

