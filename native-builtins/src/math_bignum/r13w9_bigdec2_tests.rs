use super::{
    bd_add_mc_core, bd_exact_divide_core, bd_exact_divide_core_factored, bd_rescale_operand,
    bd_round_to_precision, bigint_pow_limbs, native_bd_add_mc, native_bd_compare_to_exact,
    native_bd_divide_exact, register_bigdecimal_arithmetic_overrides,
    register_biginteger_arithmetic_overrides,
};
use crate::bigint::BigInt;
use crate::test_utils::mock_ctx;
use crate::{
    BD_ROUND_CEILING, BD_ROUND_DOWN, BD_ROUND_FLOOR, BD_ROUND_HALF_DOWN, BD_ROUND_HALF_EVEN,
    BD_ROUND_HALF_UP, BD_ROUND_UNNECESSARY, BD_ROUND_UP,
};
use cratonvm_native_api::{NativeHeapAccess, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, RuntimeError, VmError};
use cratonvm_types::Value;

const MODES: [i32; 8] = [
    BD_ROUND_UP,
    BD_ROUND_DOWN,
    BD_ROUND_CEILING,
    BD_ROUND_FLOOR,
    BD_ROUND_HALF_UP,
    BD_ROUND_HALF_DOWN,
    BD_ROUND_HALF_EVEN,
    BD_ROUND_UNNECESSARY,
];

fn thrown(e: MethodCallFailed) -> String {
    match e {
        MethodCallFailed::InternalError(VmError::Runtime(r)) => format!("{r}"),
        other => format!("{other}"),
    }
}

fn big(v: i64) -> BigInt {
    BigInt::from_decimal(&v.to_string())
}

fn shown(r: Result<(BigInt, i32), MethodCallFailed>) -> Result<(String, i32), String> {
    r.map(|(q, s)| (q.to_decimal(), s)).map_err(thrown)
}

/// Item 1: the factored exact divide is the digit loop's answer on every
/// shape, the scale-extreme refusals included.
#[test]
fn exact_divide_by_factors_matches_the_digit_loop() {
    let mut dividends: Vec<i64> = (-300i64..=300).step_by(7).collect();
    dividends.extend([0, 1, 2, 3, 1_000_000, -999_999_937]);
    let divisors = [
        -40i64, -8, -3, -1, 1, 2, 3, 4, 5, 6, 7, 8, 12, 16, 20, 25, 40, 64, 80, 96, 99, 125,
        250, 625, 1000, 1024, 3125, 7_812_500, 0,
    ];
    let scales = [
        (0, 0),
        (2, 0),
        (0, 3),
        (-1, 1),
        (i32::MAX, 0),
        (i32::MAX - 1, 0),
        (i32::MAX - 5, 0),
        (i32::MAX, -3),
        (i32::MIN, 0),
        (0, i32::MIN),
        (i32::MIN, 1),
    ];
    let mut checked = 0usize;
    for &a in &dividends {
        for &b in &divisors {
            for &(sa, sb) in &scales {
                let old = shown(bd_exact_divide_core(&big(a), sa, &big(b), sb));
                let new = shown(bd_exact_divide_core_factored(&big(a), sa, &big(b), sb));
                assert_eq!(new, old, "{a}e-{sa} / {b}e-{sb}");
                checked += 1;
            }
        }
    }
    assert!(checked > 20_000, "vacuous differential: {checked}");
}

/// The shape that was ~bits(divisor) passes: a wide divisor with no
/// factor 2 or 5 refuses at once; a wide power of two terminates at
/// exactly its exponent.
#[test]
fn exact_divide_by_factors_decides_wide_divisors_up_front() {
    let wide_odd = bigint_pow_limbs(&big(3), 20_000);
    assert_eq!(
        shown(bd_exact_divide_core_factored(&big(1), 0, &wide_odd, 0)),
        Err("ArithmeticException: Non-terminating decimal expansion; \
             no exact representable decimal result."
            .to_string())
    );
    let two_3000 = big(1).shl(3000);
    let want = crate::bigint_pow5(3000).mul(&big(7));
    let got = bd_exact_divide_core_factored(&big(7), 0, &two_3000, 0).expect("terminates");
    assert_eq!(got, (want, 3000));
    let old = bd_exact_divide_core(&big(7), 0, &two_3000, 0).expect("terminates");
    assert_eq!(got, old);
}

/// Item 3: `doRound`'s scale arithmetic is `checkScaleNonZero` of a long,
/// both passes. The message follows the TRUNCATED sign: `(int)(MIN - 1)` is
/// `MAX`, so it is "Underflow".
#[test]
fn round_to_precision_checks_the_scale_in_a_long() {
    let r =
        |u: i64, s: i32, p: i32| shown(bd_round_to_precision(&big(u), s, p, BD_ROUND_HALF_UP));
    assert_eq!(
        r(123, i32::MIN + 1, 1),
        Err("ArithmeticException: Underflow".to_string())
    );
    // 999 @2 carries into a second pass: MIN + 2 - 1 - 1 = MIN, in range.
    assert_eq!(r(999, i32::MIN + 2, 2), Ok(("10".to_string(), i32::MIN)));
    // ... and one place further the second pass leaves `int`.
    assert_eq!(
        r(999, i32::MIN + 1, 2),
        Err("ArithmeticException: Underflow".to_string())
    );
    assert_eq!(
        r(123_456, i32::MAX, 3),
        Ok(("123".to_string(), i32::MAX - 3))
    );
}

fn old_add_mc(
    ua: &BigInt,
    sa: i32,
    ub: &BigInt,
    sb: i32,
    p: i32,
    mode: i32,
) -> Result<(BigInt, i32), MethodCallFailed> {
    let s = sa.max(sb);
    let sum = bd_rescale_operand(ua, i64::from(s) - i64::from(sa))?
        .add(&bd_rescale_operand(ub, i64::from(s) - i64::from(sb))?);
    bd_round_to_precision(&sum, s, p, mode)
}

/// The JDK's zero-operand and `preAlign` roads give the value (and scale)
/// the exact-sum-then-round body gave, wherever that body could compute
/// it.
#[test]
fn add_mc_core_agrees_with_exact_then_round() {
    let values = [
        -99_999i64, -1000, -123, -10, -1, 0, 1, 7, 10, 999, 12_345, 100_001,
    ];
    let scales = [-3, 0, 2, 5, 12, 25];
    let mut checked = 0usize;
    for &a in &values {
        for &b in &values {
            for &sa in &scales {
                for &sb in &scales {
                    for p in 1..=6 {
                        for &m in &MODES {
                            let (ua, ub) = (big(a), big(b));
                            let new = shown(bd_add_mc_core(&ua, sa, &ub, sb, p, m));
                            let old = shown(old_add_mc(&ua, sa, &ub, sb, p, m));
                            assert_eq!(new, old, "{a}e-{sa} + {b}e-{sb} @{p} mode {m}");
                            checked += 1;
                        }
                    }
                }
            }
        }
    }
    assert!(checked > 100_000, "vacuous differential: {checked}");
}

/// Scale gaps the old body multiplied out (or refused), answered by the
/// JDK algorithm: `ONE.add(1E-100000000, DECIMAL64)` is `1.000000000000000`;
/// UP rounds the invisible tail up; a gap past `bigTenToThe`'s range still
/// answers; a zero operand keeps only the scale the precision allows.
#[test]
fn add_mc_core_answers_huge_scale_gaps_without_the_power_of_ten() {
    let one = big(1);
    let ten15 = "1000000000000000".to_string();
    let add = |a: &BigInt, sa: i32, b: &BigInt, sb: i32, p: i32, m: i32| {
        shown(bd_add_mc_core(a, sa, b, sb, p, m))
    };
    assert_eq!(
        add(&one, 0, &one, 100_000_000, 16, BD_ROUND_HALF_EVEN),
        Ok((ten15.clone(), 15))
    );
    assert_eq!(
        add(&one, 0, &one, 100_000_000, 16, BD_ROUND_UP),
        Ok(("1000000000000001".to_string(), 15))
    );
    assert_eq!(
        add(&one, 0, &one, 2_000_000_000, 16, BD_ROUND_HALF_EVEN),
        Ok((ten15.clone(), 15))
    );
    assert!(old_add_mc(&one, 0, &one, 2_000_000_000, 16, BD_ROUND_HALF_EVEN).is_err());
    // subtract: 1 - 1E-100000000 @16 DOWN is 0.9999999999999999.
    assert_eq!(
        add(&one, 0, &big(-1), 100_000_000, 16, BD_ROUND_DOWN),
        Ok(("9999999999999999".to_string(), 16))
    );
    // ZERO at scale 10^9 plus ONE @16: 1.000000000000000.
    let zero = BigInt::zero();
    assert_eq!(
        add(&zero, 1_000_000_000, &one, 0, 16, BD_ROUND_HALF_EVEN),
        Ok((ten15, 15))
    );
    // 0.00 + 1.5 @5 = 1.50; 1 + 0.00000 @3 = 1.00.
    assert_eq!(
        add(&zero, 2, &big(15), 1, 5, BD_ROUND_HALF_UP),
        Ok(("150".to_string(), 2))
    );
    assert_eq!(
        add(&one, 0, &zero, 5, 3, BD_ROUND_HALF_UP),
        Ok(("100".to_string(), 2))
    );
}

fn npe_message(r: Result<Option<Value>, MethodCallFailed>) -> Option<String> {
    match r {
        Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::NullPointerException { message },
        ))) => message,
        _ => None,
    }
}

