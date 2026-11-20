use super::{
    bd_narrowing_truncates_to_zero, bd_plain_string_check, bd_pow_ten_check, bd_product_scale,
    bd_rescale_operand, bd_to_big_integer_check, bd_to_f32, bd_to_f64, bd_truncate,
    bi_pow_check_range, bi_shift_right_str,
};
use crate::bigint::BigInt;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_types::error::{MethodCallFailed, VmError};

/// `"<ExceptionClass>: <message>"` — the same shape as the `!!` column of
/// the `java` transcripts these rows were read off, so a row can be
/// compared to its transcript line by eye.
fn thrown(e: MethodCallFailed) -> String {
    match e {
        MethodCallFailed::InternalError(VmError::Runtime(r)) => format!("{r}"),
        other => format!("{other}"),
    }
}

fn plain(unscaled: &str, scale: i32) -> Result<(), String> {
    bd_plain_string_check(unscaled, scale).map_err(thrown)
}

fn to_bigint(unscaled: &str, scale: i32) -> Result<String, String> {
    let u = BigInt::from_decimal(unscaled);
    bd_to_big_integer_check(&u, scale).map_err(thrown)?;
    Ok(bd_truncate(&u, scale).to_decimal())
}

/// `BigDecimal.longValue()`, exactly as `native_bd_long_value` composes it.
fn narrow_long(unscaled: &str, scale: i32) -> i64 {
    let u = BigInt::from_decimal(unscaled);
    if bd_narrowing_truncates_to_zero(&u, scale) {
        return 0;
    }
    crate::bigint_low_twos_complement(&bd_truncate(&u, scale), 64) as i64
}

/// `BigDecimal.intValue()`, exactly as `native_bd_int_value` composes it.
fn narrow_int(unscaled: &str, scale: i32) -> i32 {
    let u = BigInt::from_decimal(unscaled);
    if bd_narrowing_truncates_to_zero(&u, scale) {
        return 0;
    }
    crate::bigint_low_twos_complement(&bd_truncate(&u, scale), 32) as u32 as i32
}

/// `a.add(b)`, exactly as `native_bd_add` composes it: `(unscaled, scale)`
/// of the exact result, or the refusal text.
fn add_scales(ua: &str, sa: i32, ub: &str, sb: i32) -> Result<(String, i32), String> {
    let (ua, ub) = (BigInt::from_decimal(ua), BigInt::from_decimal(ub));
    let s = sa.max(sb);
    let a = bd_rescale_operand(&ua, i64::from(s) - i64::from(sa)).map_err(thrown)?;
    let b = bd_rescale_operand(&ub, i64::from(s) - i64::from(sb)).map_err(thrown)?;
    Ok((a.add(&b).to_decimal(), s))
}

const OVERFLOW_RANGE: &str = "ArithmeticException: BigInteger would overflow supported range";
const UNDERFLOW: &str = "ArithmeticException: Underflow";
const OVERFLOW: &str = "ArithmeticException: Overflow";
const TOO_LARGE: &str = "OutOfMemoryError: too large to fit in a String";

/// `BigDecimal.toPlainString()` — `scratchpad/f31/{Bd,Bd2,Bd4,Bd5}.java` on
/// `openjdk 25.0.3 2026-04-21 LTS (25.0.3+9-LTS)`. `bd(u,s)` is
/// `new BigDecimal(BigInteger.valueOf(u), s)`; every row below is a line of
/// that transcript. See [`super::bd_plain_string_check`] for the full one.
///
/// The three pairs that make this more than a smoke test, because each one
/// separates two candidate rules that agree everywhere else:
///
/// * `bd(0,MIN)` = `0` but `bd(0,MAX)` OOMEs — the zero short-circuit is on
///   the NEGATIVE-scale road only.
/// * `bd(1,-2147483646)` passes but `bd(12,-2147483646)` and
///   `bd(-1,-2147483646)` do not — `len` counts the SIGNED rendering there.
/// * `bd(1,2147483645)` passes but `bd(-1,2147483645)` does not — `len` on
///   the positive road is `(signum<0 ? 3 : 2) + scale`, with no digits in
///   it at all.
#[test]
fn plain_string_refusals_match_hotspot() {
    // `checkScaleNonZero` CASTS, so only i32::MIN fails, and it lands
    // negative: "Overflow" — the opposite word from `toBigInteger`'s.
    assert_eq!(plain("1", i32::MIN), Err(OVERFLOW.to_string()));
    assert_eq!(plain("-1", i32::MIN), Err(OVERFLOW.to_string()));
    // `if (signum() == 0) return "0";` — negative scales only.
    assert_eq!(plain("0", i32::MIN), Ok(()));
    assert_eq!(plain("0", -5), Ok(()));
    assert_eq!(plain("0", 5), Ok(()));
    assert_eq!(plain("0", i32::MAX), Err(TOO_LARGE.to_string()));
    // Negative road: `len = str.length() + trailingZeros`, `str` signed.
    assert_eq!(plain("1", -2147483647), Err(TOO_LARGE.to_string()));
    assert_eq!(plain("1", -2147483646), Ok(()));
    assert_eq!(plain("1", -2147483645), Ok(()));
    assert_eq!(plain("12", -2147483646), Err(TOO_LARGE.to_string()));
    assert_eq!(plain("-1", -2147483646), Err(TOO_LARGE.to_string()));
    assert_eq!(plain("-1", -2147483645), Ok(()));
    // Positive road: `len = (signum < 0 ? 3 : 2) + scale`.
    assert_eq!(plain("1", 2147483646), Err(TOO_LARGE.to_string()));
    assert_eq!(plain("1", 2147483645), Ok(()));
    assert_eq!(plain("-1", 2147483645), Err(TOO_LARGE.to_string()));
    // Ordinary rows, both signs and both sides of the decimal point.
    assert_eq!(plain("1", 0), Ok(()));
    assert_eq!(plain("123", 3), Ok(()));
    assert_eq!(plain("-123", 4), Ok(()));
    assert_eq!(plain("1", -3), Ok(()));
}

/// `BigDecimal.toBigInteger()` — `scratchpad/f31/{Bd,Bd2}.java`.
///
/// The word is `"Underflow"` here and `"Overflow"` in
/// [`plain_string_refusals_match_hotspot`] for the SAME scale, because
/// `setScale` reaches the clamping instance `checkScale` and
/// `toPlainString` reaches the casting static `checkScaleNonZero`. If the
/// two are ever unified, exactly one of these two tests goes red.
#[test]
fn to_big_integer_refusals_match_hotspot() {
    assert_eq!(to_bigint("1", i32::MIN), Err(UNDERFLOW.to_string()));
    assert_eq!(to_bigint("-1", i32::MIN), Err(UNDERFLOW.to_string()));
    assert_eq!(
        to_bigint("1", i32::MIN + 1),
        Err(OVERFLOW_RANGE.to_string())
    );
    assert_eq!(
        to_bigint("1", -715_827_883),
        Err(OVERFLOW_RANGE.to_string())
    );
    // Positive scales refuse too: `setScale(0)` builds `bigTenToThe(drop)`.
    assert_eq!(to_bigint("1", i32::MAX), Err(OVERFLOW_RANGE.to_string()));
    assert_eq!(to_bigint("1", 715_827_883), Err(OVERFLOW_RANGE.to_string()));
    // `zero can have any scale` — `setScale`'s signum test comes first.
    assert_eq!(to_bigint("0", i32::MIN), Ok("0".to_string()));
    assert_eq!(to_bigint("0", i32::MAX), Ok("0".to_string()));
    assert_eq!(to_bigint("0", 715_827_883), Ok("0".to_string()));
    // Values.
    assert_eq!(to_bigint("1", -3), Ok("1000".to_string()));
    assert_eq!(to_bigint("123456", 5), Ok("1".to_string()));
    assert_eq!(to_bigint("-123456", 5), Ok("-1".to_string()));
    assert_eq!(to_bigint("1", 0), Ok("1".to_string()));
}

/// `BigDecimal.intValue()` / `longValue()` — `scratchpad/f31/{Bd,Bd4}.java`.
///
/// Every row here shares its `(unscaled, scale)` with a row of
/// [`to_big_integer_refusals_match_hotspot`] that REFUSES. That is the
/// whole point of the split: HotSpot answers `0` in under a millisecond for
/// `bd(1,MIN)` and `bd(1,MAX)`, so a guard on the shared truncation helper
/// would have been a fresh divergence, and this file's previous body
/// answered `1` for `bd(1,MIN).intValue()` in release (it negated
/// `i32::MIN`, wrapped, and took `bigint_mul_pow10`'s `n <= 0` arm).
#[test]
fn narrowing_conversions_match_hotspot() {
    // The three fast paths, at their exact boundaries.
    assert_eq!(narrow_long("1", i32::MIN), 0);
    assert_eq!(narrow_long("0", i32::MIN), 0);
    assert_eq!(narrow_long("1", i32::MAX), 0);
    assert_eq!(narrow_long("1", 715_827_883), 0);
    assert_eq!(narrow_long("1", -65), 0);
    assert_eq!(narrow_long("1", -64), 0);
    assert_eq!(narrow_long("7", -64), 0);
    // `scale <= -64` is exact, not conservative: one less is NOT zero.
    assert_eq!(narrow_long("1", -63), i64::MIN);
    assert_eq!(narrow_long("7", -63), i64::MIN);
    // `fractionOnly()` — `precision() <= scale`.
    assert_eq!(narrow_long("1", 5), 0);
    assert_eq!(narrow_long("123456", 5), 1);
    assert_eq!(narrow_long("123456", 6), 0);
    assert_eq!(narrow_long("123456", 7), 0);
    assert_eq!(narrow_long("-123456", 5), -1);
    // The upper-bound `precision()` does NOT fire at 31 digits/scale 31
    // (`bits/3 + 1` is 34 there), so this row proves the fall-through
    // division reaches HotSpot's answer anyway.
    let ten_pow_30 = format!("1{}", "0".repeat(30));
    assert_eq!(narrow_long(&ten_pow_30, 30), 1);
    assert_eq!(narrow_long(&ten_pow_30, 31), 0);
    assert_eq!(narrow_long(&ten_pow_30, -2), -8_814_407_033_341_083_648);
    // Ordinary negative scales.
    assert_eq!(narrow_long("1", -1), 10);
    assert_eq!(narrow_long("-7", -3), -7000);
    assert_eq!(narrow_long("3", -20), 4_852_094_820_647_174_144);
    // intValue is `(int) longValue()`, i.e. the low 32 bits of the same.
    assert_eq!(narrow_int("1", i32::MIN), 0);
    assert_eq!(narrow_int("1", i32::MAX), 0);
    assert_eq!(narrow_int("1", -63), 0);
    assert_eq!(narrow_int("1", -33), 0);
    assert_eq!(narrow_int("1", -32), 0);
    assert_eq!(narrow_int("3", -20), 691_011_584);
    assert_eq!(narrow_int("-123456", 5), -1);
}

/// `BigDecimal.add`/`subtract` scale alignment —
/// `scratchpad/f31/{Bd,Bd2,Bd4}.java`.
///
/// Rows 1-6 all used to compute `s - sa` in `i32`. `i32::MAX - i32::MIN`
/// panics in debug and wraps to `-1` in release, and `-1` is
/// `bigint_mul_pow10`'s "nothing to do" arm, so the release build returned
/// the operand UNSCALED — a wrong answer, silently.
///
/// Rows 7-13 are the reason the guard tests the RAISED OPERAND and not the
/// raise: `("1",MIN)+("0",MAX)` refuses and `("0",MIN)+("1",MAX)` answers,
/// and they differ in nothing but which operand is the zero.
#[test]
fn add_scale_alignment_matches_hotspot() {
    assert_eq!(
        add_scales("1", i32::MIN, "1", i32::MAX),
        Err(UNDERFLOW.into())
    );
    assert_eq!(
        add_scales("1", i32::MAX, "1", i32::MIN),
        Err(UNDERFLOW.into())
    );
    assert_eq!(
        add_scales("1", i32::MIN, "0", i32::MAX),
        Err(UNDERFLOW.into())
    );
    assert_eq!(add_scales("0", 0, "1", i32::MIN), Err(UNDERFLOW.into()));
    assert_eq!(
        add_scales("0", i32::MAX, "1", i32::MIN),
        Err(UNDERFLOW.into())
    );
    assert_eq!(
        add_scales("1", 0, "1", 715_827_883),
        Err(OVERFLOW_RANGE.into())
    );
    assert_eq!(
        add_scales("1", 0, "0", 715_827_883),
        Err(OVERFLOW_RANGE.into())
    );
    assert_eq!(
        add_scales("0", 715_827_883, "1", 0),
        Err(OVERFLOW_RANGE.into())
    );
    // A ZERO raised operand is exempt from BOTH guards, and is not built.
    assert_eq!(
        add_scales("0", i32::MIN, "1", i32::MAX),
        Ok(("1".to_string(), i32::MAX))
    );
    assert_eq!(
        add_scales("0", 0, "1", i32::MAX),
        Ok(("1".to_string(), i32::MAX))
    );
    assert_eq!(
        add_scales("1", i32::MAX, "0", 0),
        Ok(("1".to_string(), i32::MAX))
    );
    assert_eq!(
        add_scales("0", 0, "1", 715_827_883),
        Ok(("1".to_string(), 715_827_883))
    );
    // Ordinary alignment: 0.01 + 0.3 = 0.31, and 1000 + 0.3 = 1000.3.
    assert_eq!(add_scales("1", 2, "3", 1), Ok(("31".to_string(), 2)));
    assert_eq!(add_scales("1", -3, "3", 1), Ok(("10003".to_string(), 1)));
}

fn refused(v: &str, exp: i32) -> bool {
    bi_pow_check_range(&BigInt::from_decimal(v), exp).is_err()
}

/// `scratchpad/f7/Pow.java`:
///
/// ```text
/// TWO.pow(MAX)       !! ArithmeticException: BigInteger would overflow supported range [94 ms]
/// TWO.pow(MAX-1)      = <signum=1 bitLength=2147483647>                                [36 ms]
/// TWO.pow(1<<30)      = <signum=1 bitLength=1073741825>                                [50 ms]
/// THREE.pow(MAX)     !! …overflow…  [0 ms]      THREE.pow(1<<20) = <bitLength=1661954> [335 ms]
/// TEN.pow(MAX)       !! …overflow…  [0 ms]      TEN.pow(1000000000) !! …overflow…      [0 ms]
/// (2^100).pow(MAX)   !! …overflow…  [0 ms]      (2^100).pow(1<<26)  !! …overflow…      [0 ms]
/// (-2).pow(MAX)      !! …overflow…  [68 ms]     (2^31-1).pow(MAX)   !! …overflow…      [0 ms]
/// TEN.pow(3) = 1000     (-2).pow(3) = -8     (-2).pow(4) = 16
/// ```
///
/// A further 2,560 (base, exponent) rows agree on class, message and value
/// (`scratchpad/f7/PowParity.java`: `POW PARITY cases=2560 diffs=0
/// value-compared=1635`).
#[test]
fn pow_range_guard_matches_hotspot() {
    assert!(refused("2", i32::MAX));
    assert!(refused("-2", i32::MAX));
    assert!(refused("3", i32::MAX));
    assert!(refused("-3", i32::MAX));
    assert!(refused("10", i32::MAX));
    assert!(refused("10", 1_000_000_000));
    assert!(refused("2147483647", i32::MAX));
    // 2^100: refused by the `powersOfTwo * exponent` narrowing, not by the
    // scale factor — the two arms are separate rows of the JDK's guard.
    let p100 = BigInt::from_decimal("1267650600228229401496703205376");
    assert!(bi_pow_check_range(&p100, i32::MAX).is_err());
    assert!(bi_pow_check_range(&p100, 1 << 26).is_err());

    assert!(!refused("2", i32::MAX - 1));
    assert!(!refused("2", 1 << 30));
    assert!(!refused("3", 1 << 20));
    assert!(!refused("10", 3));
    assert!(!refused("-2", 3));
    assert!(!refused("-2", 4));
}

/// `scratchpad/f7/Scale.java`:
///
/// ```text
/// 1.5.setScale(MAX_VALUE, HALF_UP) !! ArithmeticException: BigInteger would overflow supported range [15 ms]
/// 1.5.setScale(1000000)             = <1000002 chars, scale=1000000>                                [2950 ms]
/// ```
#[test]
fn set_scale_pow_ten_guard_matches_hotspot() {
    // `1.5` has scale 1, so `setScale(Integer.MAX_VALUE)` raises by MAX-1.
    assert!(bd_pow_ten_check(i32::MAX - 1).is_err());
    assert!(bd_pow_ten_check(i32::MAX).is_err());
    assert!(bd_pow_ten_check(1_000_000).is_ok());
    // Every scale a real caller uses.
    assert!(bd_pow_ten_check(0).is_ok());
    assert!(bd_pow_ten_check(1).is_ok());
    assert!(bd_pow_ten_check(18).is_ok());
    // The boundary itself: `TEN.pow(n)` refuses once `3n >= Integer.MAX_VALUE`.
    assert!(bd_pow_ten_check(715_827_882).is_ok());
    assert!(bd_pow_ten_check(715_827_883).is_err());
}

/// `scratchpad/f7/Shr.java`:
///
/// ```text
/// (-1).shiftRight(MAX) = -1      (-9).shiftRight(MAX) = -1     (0).shiftRight(MAX) = 0
/// (-1).shiftRight(100) = -1      (-9).shiftRight(1)   = -5     (9).shiftRight(1)   = 4
/// (2^100).shiftRight(MAX) = 0    (-2^100).shiftRight(MAX) = -1
/// ```
///
/// The `q == "0"` guard in the loop only ever fires for a NON-NEGATIVE
/// value: the negative arm computes `ceildiv(q, 2)`, whose fixpoint is 1.
/// Each row below used to run the full `n` iterations — 2^31 decimal
/// divisions — to return a value that had stopped changing after four.
#[test]
fn shift_right_str_negative_stops_at_the_fixpoint() {
    assert_eq!(bi_shift_right_str("-1", i32::MAX), "-1");
    assert_eq!(bi_shift_right_str("-9", i32::MAX), "-1");
    assert_eq!(
        bi_shift_right_str("1267650600228229401496703205376", i32::MAX),
        "0"
    );
    assert_eq!(
        bi_shift_right_str("-1267650600228229401496703205376", i32::MAX),
        "-1"
    );
    assert_eq!(bi_shift_right_str("-1", 100), "-1");
    assert_eq!(bi_shift_right_str("-9", 1), "-5");
    assert_eq!(bi_shift_right_str("9", 1), "4");
    assert_eq!(bi_shift_right_str("0", i32::MAX), "0");
}

// -----------------------------------------------------------------
// G10 (2026-08-16) — `multiply`'s product scale, `doubleValue`, and the
// no-panic pin over every rescale path. Every expectation below is a
// transcript line from `scratchpad/g10/{BdProbe,Bd2Probe}.java` on Temurin
// OpenJDK 25.0.3+9, not a derivation from this implementation.
// -----------------------------------------------------------------

/// `a.multiply(b)`'s product scale, exactly as `native_bd_multiply`
/// composes it: the RECEIVER's unscaled value decides the refusal.
fn product_scale(ua: &str, sa: i32, sb: i32) -> Result<i32, String> {
    bd_product_scale(&BigInt::from_decimal(ua), sa, sb).map_err(thrown)
}

/// ```text
/// bd(1,MAX).multiply(bd(1,MAX))  !! ArithmeticException: Underflow
/// bd(0,MAX).multiply(bd(1,MAX))   = unscaled=0 scale=2147483647
/// bd(1,MIN).multiply(bd(1,MIN))  !! ArithmeticException: Overflow
/// bd(0,MIN).multiply(bd(1,MIN))   = unscaled=0 scale=-2147483648
/// bd(1,MAX).multiply(bd(1,MIN))   = unscaled=1 scale=-1
/// bd(1,MAX).multiply(bd(1,0))     = unscaled=1 scale=2147483647
/// ```
#[test]
fn multiply_product_scale_matches_hotspot() {
    // Non-zero receiver, sum past Integer.MAX_VALUE -> "Underflow".
    assert_eq!(
        product_scale("1", i32::MAX, i32::MAX),
        Err("ArithmeticException: Underflow".to_string())
    );
    assert_eq!(
        product_scale("1", i32::MAX, 1),
        Err("ArithmeticException: Underflow".to_string())
    );
    assert_eq!(
        product_scale("1", 1_073_741_824, 1_073_741_824),
        Err("ArithmeticException: Underflow".to_string())
    );
    // Non-zero receiver, sum past Integer.MIN_VALUE -> "Overflow".
    // The two words are NOT interchangeable.
    assert_eq!(
        product_scale("1", i32::MIN, i32::MIN),
        Err("ArithmeticException: Overflow".to_string())
    );
    assert_eq!(
        product_scale("1", i32::MIN, -1),
        Err("ArithmeticException: Overflow".to_string())
    );
    assert_eq!(
        product_scale("1", -1_073_741_824, -1_073_741_825),
        Err("ArithmeticException: Overflow".to_string())
    );
    // A ZERO receiver clamps and answers -- and it is the RECEIVER's
    // zeroness, not the product's: `bd(1,MAX).multiply(bd(0,MAX))` throws.
    assert_eq!(product_scale("0", i32::MAX, i32::MAX), Ok(i32::MAX));
    assert_eq!(
        product_scale("0", 1_073_741_824, 1_073_741_824),
        Ok(i32::MAX)
    );
    assert_eq!(product_scale("0", i32::MIN, i32::MIN), Ok(i32::MIN));
    assert_eq!(product_scale("0", i32::MIN, -1), Ok(i32::MIN));
    // In-range sums are just the sum, at both extremes.
    assert_eq!(product_scale("1", i32::MAX, 0), Ok(i32::MAX));
    assert_eq!(product_scale("1", i32::MIN, 0), Ok(i32::MIN));
    assert_eq!(product_scale("1", i32::MAX, i32::MIN), Ok(-1));
    assert_eq!(product_scale("1", 2, 3), Ok(5));
    assert_eq!(product_scale("-1", -2, -3), Ok(-5));
}

/// `scratchpad/g10/Bd2Probe.java`. Every row is `bd(u,s).doubleValue()` on
/// Temurin OpenJDK 25.0.3+9, and every one of them answers in 0 ms without
/// HotSpot rendering anything — which is the point: the old body rendered
/// `scale.unsigned_abs()` zeros first.
#[test]
fn double_value_matches_hotspot() {
    let d = |u: &str, s: i32| bd_to_f64(&BigInt::from_decimal(u), s);
    // The scale extremes, on both signs and on zero.
    assert_eq!(d("1", i32::MIN), f64::INFINITY);
    assert_eq!(d("-1", i32::MIN), f64::NEG_INFINITY);
    assert_eq!(d("1", i32::MIN + 1), f64::INFINITY);
    assert_eq!(d("1", i32::MAX), 0.0);
    assert_eq!(d("-1", i32::MAX), -0.0);
    assert!(d("-1", i32::MAX).is_sign_negative());
    assert_eq!(d("0", i32::MIN), 0.0);
    assert_eq!(d("0", i32::MAX), 0.0);
    // A zero value is +0.0 at EVERY scale -- never -0.0.
    assert!(d("0", i32::MIN).is_sign_positive());
    assert!(d("0", 324).is_sign_positive());
    // The overflow boundary, one scale apart.
    assert_eq!(d("1", -308), 1.0e308);
    assert_eq!(d("1", -309), f64::INFINITY);
    assert_eq!(d("15", -308), f64::INFINITY);
    assert_eq!(d("17976931348623157", -292), 1.7976931348623157e308);
    assert_eq!(d("17976931348623159", -292), f64::INFINITY);
    // The underflow boundary. `bd(1,324)` rounds to zero but `bd(15,324)`
    // does not, and they share an adjusted exponent -- which is why the
    // clamp is at -325 and not -324.
    assert_eq!(d("1", 323), 9.9e-324);
    assert_eq!(d("1", 324), 0.0);
    assert_eq!(d("15", 324), 1.5e-323);
    assert_eq!(d("49", 326), 0.0);
    assert_eq!(d("1", 325), 0.0);
    assert_eq!(d("-1", 324), -0.0);
    assert!(d("-1", 324).is_sign_negative());
    // Ordinary values, and the round-to-nearest-even row that a truncating
    // implementation gets wrong.
    assert_eq!(d("1", 0), 1.0);
    assert_eq!(d("1", 1), 0.1);
    assert_eq!(d("1", -1), 10.0);
    assert_eq!(d("15", 1), 1.5);
    assert_eq!(d("-15", -1), -150.0);
    assert_eq!(d("9007199254740993", 0), 9.007199254740992e15);
    assert_eq!(d("9007199254740993", 324), 9.007199254740994e-309);
}

/// `floatValue()` is `Float.parseFloat(toString())`, NOT `(float)
/// doubleValue()` -- the clamps are `f32`'s and the rounding is single.
#[test]
fn float_value_matches_hotspot() {
    let f = |u: &str, s: i32| bd_to_f32(&BigInt::from_decimal(u), s);
    assert_eq!(f("1", i32::MIN), f32::INFINITY);
    assert_eq!(f("-1", i32::MIN), f32::NEG_INFINITY);
    assert_eq!(f("1", i32::MAX), 0.0);
    assert!(f("-1", i32::MAX).is_sign_negative());
    assert_eq!(f("0", i32::MIN), 0.0);
    // `bd(1,-308)` is 1.0E308 as a double and Infinity as a float.
    assert_eq!(f("1", -308), f32::INFINITY);
    assert_eq!(f("1", 308), 0.0);
    assert!(f("-1", 308).is_sign_negative());
    assert_eq!(f("1", 1), 0.1f32);
    assert_eq!(f("15", 1), 1.5f32);
    assert_eq!(f("-15", -1), -150.0f32);
    assert_eq!(f("9007199254740993", 1), 9.0071994e14f32);
}

/// **The no-panic pin.** A Rust panic in a native is not a Java throwable:
/// it is not catchable and it takes the VM down where HotSpot throws. Every
/// rescale path in this file is exercised here at `Integer.MIN_VALUE`,
/// `Integer.MIN_VALUE + 1`, `Integer.MAX_VALUE` and `0` — the four scales
/// where a `-scale` / `sa + sb` / `s - sa` spelling overflows — on a zero
/// operand and a non-zero one. The assertion is not on the VALUE: it is
/// that each call RETURNS, either an answer or a `MethodCallFailed`.
///
/// These are the four shapes that have each been a live abort in this
/// family at some point: `apply_scale`'s `-scale` (F20), `bd_set_scale_impl`'s
/// `new_scale - scale` (F31), `bd_rescale_operand`'s `s - sa` (F31), and
/// `native_bd_multiply`'s `sa + sb` (G10).
#[test]
fn every_rescale_path_is_total_at_the_scale_extremes() {
    let scales = [i32::MIN, i32::MIN + 1, -1, 0, 1, i32::MAX - 1, i32::MAX];
    let operands = ["0", "1", "-1", "15", "-9007199254740993"];
    for &s in &scales {
        for u in &operands {
            let v = BigInt::from_decimal(u);
            // toBigInteger()'s guard, then its arithmetic. `bd_truncate`
            // is only reached when the guard admits, exactly as
            // `bd_truncated_bigint` composes them.
            if bd_to_big_integer_check(&v, s).is_ok() {
                let _ = bd_truncate(&v, s).to_decimal();
            }
            // intValue()/longValue(): no guard at all, by design.
            let _ = narrow_int(u, s);
            let _ = narrow_long(u, s);
            // toPlainString()'s guard.
            let _ = bd_plain_string_check(u, s);
            // doubleValue()/floatValue(): total, no guard needed.
            let _ = bd_to_f64(&v, s);
            let _ = bd_to_f32(&v, s);
            for &s2 in &scales {
                // multiply()'s product scale.
                let _ = bd_product_scale(&v, s, s2);
                // add()/subtract()'s per-operand raise. The `i64`
                // subtraction is the caller's job and is spelled here the
                // way `native_bd_add` spells it.
                let common = s.max(s2);
                let _ = bd_rescale_operand(&v, i64::from(common) - i64::from(s));
                let _ = bd_rescale_operand(&v, i64::from(common) - i64::from(s2));
            }
        }
    }
}

/// `bi_mod_pow_str` used to `panic!` on a negative exponent. MEASURED on
/// Temurin OpenJDK 25.0.3+9 (`scratchpad/g10/BiProbe.java`):
///
/// ```text
/// 3.modPow(-1, 7)  = 5      3.modPow(2, 1) = 0      (-3).modPow(3, 7) = 1
/// 2.modPow(-1, 4) !! ArithmeticException: BigInteger not invertible.
/// ```
#[test]
fn mod_pow_negative_exponent_no_longer_panics() {
    use super::{bi_mod_pow_str, bi_mod_pow_str_opt};
    // The invertible case is now COMPUTED, not aborted.
    assert_eq!(bi_mod_pow_str_opt("3", "-1", "7").as_deref(), Some("5"));
    assert_eq!(bi_mod_pow_str_opt("2", "-1", "7").as_deref(), Some("4"));
    assert_eq!(bi_mod_pow_str_opt("3", "-2", "10").as_deref(), Some("9"));
    assert_eq!(bi_mod_pow_str_opt("2", "-3", "7").as_deref(), Some("1"));
    // The one shape a `-> String` cannot express.
    assert_eq!(bi_mod_pow_str_opt("2", "-1", "4"), None);
    assert_eq!(bi_mod_pow_str_opt("2", "-1", "8"), None);
    assert_eq!(bi_mod_pow_str_opt("0", "-5", "7"), None);
    // ...and the infallible wrapper RETURNS there rather than aborting the
    // VM. `"0"` is a wrong value, deliberately preferred to a panic; no
    // in-tree caller can reach it.
    assert_eq!(bi_mod_pow_str("2", "-1", "4"), "0");
    assert_eq!(bi_mod_pow_str("0", "-5", "7"), "0");
    // The non-negative rows are unchanged.
    assert_eq!(bi_mod_pow_str("3", "4", "7"), "4");
    assert_eq!(bi_mod_pow_str("-3", "3", "7"), "1");
    assert_eq!(bi_mod_pow_str("3", "2", "1"), "0");
}
