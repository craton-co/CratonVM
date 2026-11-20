use super::{
    bd_add_mc_road, bd_divide_mc_road, bd_mc_rounds, bd_quotient_cmp, bigint_pow5_memo_in,
    native_bd_float_value_binary, register_bigdecimal_arithmetic_overrides, BdMcAnswer,
    Pow5Memo,
};
use crate::bigint::BigInt;
use cratonvm_native_api::{NativeKind, NativeMethodRegistry};
use std::cmp::Ordering;

fn big(v: i64) -> BigInt {
    BigInt::from_decimal(&v.to_string())
}

fn build() -> NativeMethodRegistry {
    let mut reg = NativeMethodRegistry::new();
    register_bigdecimal_arithmetic_overrides(&mut reg);
    reg
}


#[test]
fn pow5_memo_answers_the_power_in_both_arms() {
    let mut memo = Pow5Memo {
        on: None,
        slots: Vec::new(),
    };
    // Twice over, so the second pass reads the slots the first filled;
    // `n` and `n + 64` share a slot and evict each other.
    for pass in 0..2 {
        for n in (0..=2300u32)
            .step_by(7)
            .chain([63, 127, 64, 128, 2199, 2200, 2201])
        {
            let want = crate::bigint_pow5(n);
            assert_eq!(
                *bigint_pow5_memo_in(&mut memo, true, n),
                want,
                "on, n {n}, pass {pass}"
            );
            assert_eq!(
                *bigint_pow5_memo_in(&mut memo, false, n),
                want,
                "off, n {n}"
            );
        }
    }
    assert!(memo.slots.len() == super::POW5_MEMO_SLOTS);
}

#[test]
fn digit_drop_is_decided_by_the_precision() {
    assert!(!bd_mc_rounds(&big(999), 3));
    assert!(bd_mc_rounds(&big(1000), 3));
    assert!(bd_mc_rounds(&big(-1000), 3));
    assert!(!bd_mc_rounds(&big(123_456), 0));
    assert!(!bd_mc_rounds(&BigInt::zero(), 1));
}

/// `add(augend, mc)` (`subtract` negates the augend first), JDK 25:
/// `ZERO.add(x, mc)` is `x` itself, `x.add(ZERO, mc)` is the receiver,
/// a digit drop is a fresh object, a raise to the preferred scale and
/// the both-non-zero road are `valueOf`s.
#[test]
fn add_mc_roads() {
    let z = BigInt::zero();
    // 0 (scale 0) + 12.5 (scale 1), precision 16: preferred 1 = x's scale.
    assert_eq!(
        bd_add_mc_road((&z, 0), (&big(125), 1), 16, 1, false),
        BdMcAnswer::Operand
    );
    // 12.5 + 0: the receiver.
    assert_eq!(
        bd_add_mc_road((&big(125), 1), (&z, 0), 16, 1, false),
        BdMcAnswer::Receiver
    );
    // subtract: 0 - 12.5 is `(12.5).negate()`, a `valueOf`.
    assert_eq!(
        bd_add_mc_road((&z, 0), (&big(-125), 1), 16, 1, true),
        BdMcAnswer::Constant
    );
    // 12.5 - 0: the receiver.
    assert_eq!(
        bd_add_mc_road((&big(125), 1), (&z, 0), 16, 1, true),
        BdMcAnswer::Receiver
    );
    // 0 (scale 3) + 5 (scale 0), precision 16: raised to 5.000 -- `setScale`.
    assert_eq!(
        bd_add_mc_road((&z, 3), (&big(5), 0), 16, 3, false),
        BdMcAnswer::Constant
    );
    // 0 + 12345 at precision 2: `doRound` drops digits -- fresh.
    assert_eq!(
        bd_add_mc_road((&z, 0), (&big(12_345), 0), 2, -3, false),
        BdMcAnswer::Fresh
    );
    // Both non-zero, both zero, precision 0: `valueOf` roads.
    assert_eq!(
        bd_add_mc_road((&big(1), 0), (&big(1), 0), 16, 0, false),
        BdMcAnswer::Constant
    );
    assert_eq!(
        bd_add_mc_road((&z, 0), (&z, 2), 16, 2, false),
        BdMcAnswer::Constant
    );
    assert_eq!(
        bd_add_mc_road((&z, 0), (&big(7), 0), 0, 0, false),
        BdMcAnswer::Constant
    );
}

#[test]
fn quotient_comparison() {
    // 0.96 / 1 against 1: below.
    assert_eq!(
        bd_quotient_cmp(&big(96), 2, &big(1), 0, &big(1)),
        Some(Ordering::Less)
    );
    // 1.4 / 1 against 1: above.
    assert_eq!(
        bd_quotient_cmp(&big(14), 1, &big(1), 0, &big(1)),
        Some(Ordering::Greater)
    );
    // 1.0 / 1 and 3 / 0.3 against 1 and 10: equal.
    assert_eq!(
        bd_quotient_cmp(&big(10), 1, &big(1), 0, &big(1)),
        Some(Ordering::Equal)
    );
    assert_eq!(
        bd_quotient_cmp(&big(3), 0, &big(3), 1, &big(10)),
        Some(Ordering::Equal)
    );
    // Signs are ignored; a scale gap out of proportion is not compared.
    assert_eq!(
        bd_quotient_cmp(&big(-96), 2, &big(1), 0, &big(1)),
        Some(Ordering::Less)
    );
    assert_eq!(bd_quotient_cmp(&big(1), 1000, &big(1), 0, &big(1)), None);
}

/// `divide(divisor, mc)`: only a `1` (precision 1) or `10` (precision 2) at
/// scale 0 can be `doRound`'s fresh object.
#[test]
fn divide_mc_roads() {
    let one = (&big(1), 0);
    // ONE.divide(ONE, precision 1): the stripped quotient is `valueOf(1, 0)`.
    assert_eq!(
        bd_divide_mc_road((&big(1), 0), (&big(1), 0), 1, one),
        BdMcAnswer::Constant
    );
    // new BigDecimal("1.0").divide(ONE, precision 1): the quotient `1.0`
    // keeps scale 1 (preferred), `doRound` makes a fresh `1`.
    assert_eq!(
        bd_divide_mc_road((&big(10), 1), (&big(1), 0), 1, one),
        BdMcAnswer::Fresh
    );
    // 0.96 / 1 at precision 1 rounds UP across the decade: fresh.
    assert_eq!(
        bd_divide_mc_road((&big(96), 2), (&big(1), 0), 1, one),
        BdMcAnswer::Fresh
    );
    // 1.4 / 1 at precision 1 rounds down to `valueOf(1, 0)`.
    assert_eq!(
        bd_divide_mc_road((&big(14), 1), (&big(1), 0), 1, one),
        BdMcAnswer::Constant
    );
    // 30 / 3 at precision 2: exact, preferred 0.
    assert_eq!(
        bd_divide_mc_road((&big(30), 0), (&big(3), 0), 2, (&big(10), 0)),
        BdMcAnswer::Constant
    );
    // Values that `doRound` cannot produce are always the `valueOf` road.
    assert_eq!(
        bd_divide_mc_road((&big(7), 0), (&big(1), 0), 1, (&big(7), 0)),
        BdMcAnswer::Constant
    );
    assert_eq!(
        bd_divide_mc_road(
            (&big(2), 0),
            (&big(3), 0),
            16,
            (&big(6_666_666_666_666_667), 16)
        ),
        BdMcAnswer::Constant
    );
}
