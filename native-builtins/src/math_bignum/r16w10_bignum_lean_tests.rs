use super::{
    bd_compact_of, bd_double_shifted_le, bd_double_unscaled_reference, bignum_le_to_be,
    bignum_lean_slot, BIGNUM_LEAN_STACK_WORDS,
};
use crate::bigint::BigInt;
use cratonvm_native_api::FieldMetadata;

fn lcg(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    *state ^ (*state >> 29)
}

/// The compact rule `bd_alloc_bigint` / `bd_write_into_bigint` apply
/// inline, transcribed.
fn historical_compact(v: &BigInt) -> Option<i64> {
    let le = v.mag_le();
    if le.len() <= 2 {
        let mag = (le.first().copied().unwrap_or(0) as u64)
            | ((le.get(1).copied().unwrap_or(0) as u64) << 32);
        if mag <= i64::MAX as u64 {
            return Some(if v.is_neg() {
                -(mag as i64)
            } else {
                mag as i64
            });
        }
    }
    None
}

/// `native_bd_init_double`'s decomposition (finite, nonzero).
fn decompose(d: f64) -> (bool, u64, i32) {
    let bits = d.to_bits();
    let neg = (bits >> 63) != 0;
    let biased = ((bits >> 52) & 0x7ff) as i32;
    let frac = bits & ((1u64 << 52) - 1);
    let (mut sig, mut exp) = if biased == 0 {
        (frac << 1, -1075i32)
    } else {
        (frac | (1u64 << 52), biased - 1075)
    };
    while sig & 1 == 0 {
        sig >>= 1;
        exp += 1;
    }
    (neg, sig, exp)
}

#[test]
fn shifted_limbs_are_the_bigint_shift() {
    let mut s = 0x5eed_u64;
    let mut sigs = vec![
        1u64,
        3,
        (1u64 << 53) - 1,
        (1u64 << 52) + 1,
        0xFFFF_FFFF,
        0x1_0000_0001,
    ];
    for _ in 0..200 {
        sigs.push((lcg(&mut s) & ((1u64 << 53) - 1)) | 1);
    }
    for &sig in &sigs {
        for exp in (0u32..=1023)
            .step_by(7)
            .chain([31, 32, 33, 63, 64, 65, 971, 1023])
        {
            let want =
                BigInt::from_le_words(false, vec![sig as u32, (sig >> 32) as u32]).shl(exp);
            let (le, len) = bd_double_shifted_le(sig, exp).expect("a double's shift fits");
            assert_eq!(&le[..len], want.mag_le(), "sig={sig:#x} exp={exp}");
        }
    }
    assert_eq!(bd_double_shifted_le(0, 5).map(|(_, n)| n), Some(0));
    // Past the buffer the caller takes the BigInt road.
    assert!(bd_double_shifted_le(1, (BIGNUM_LEAN_STACK_WORDS as u32) * 32).is_none());
}

#[test]
fn compact_rule_matches_the_historical_one() {
    let cases: Vec<BigInt> = vec![
        BigInt::zero(),
        BigInt::from_le_words(false, vec![1]),
        BigInt::from_le_words(true, vec![1]),
        BigInt::from_le_words(false, vec![u32::MAX, 0x7FFF_FFFF]),
        BigInt::from_le_words(true, vec![u32::MAX, 0x7FFF_FFFF]),
        BigInt::from_le_words(false, vec![0, 0x8000_0000]),
        BigInt::from_le_words(true, vec![0, 0x8000_0000]),
        BigInt::from_le_words(false, vec![u32::MAX, u32::MAX]),
        BigInt::from_le_words(false, vec![0, 0, 1]),
        BigInt::from_le_words(true, vec![5, 0, 1]),
    ];
    for v in &cases {
        assert_eq!(
            bd_compact_of(v.is_neg(), v.mag_le()),
            historical_compact(v),
            "{}",
            v.to_decimal()
        );
    }
    // `Long.MIN_VALUE` is the INFLATED sentinel: never compact.
    assert_eq!(bd_compact_of(true, &[0, 0x8000_0000]), None);
    assert_eq!(
        bd_compact_of(true, &[u32::MAX, 0x7FFF_FFFF]),
        Some(-i64::MAX)
    );
}

/// The lean constructor's `(sign, limbs, scale)` and compact decision equal
/// the historical road's (`BigInt` value, `neg_value`, then the inline
/// compact rule) for normal, subnormal, huge and tiny doubles.
#[test]
fn lean_ctor_parts_match_the_historical_road() {
    let mut s = 0xB16_D0C_u64;
    let mut ds = vec![
        0.1,
        -0.1,
        1.0,
        -1.0,
        3.0,
        1e30,
        -1e30,
        1e23,
        9.223372036854775e18,
        -9.223372036854776e18,
        1.8446744073709552e19,
        f64::MAX,
        -f64::MAX,
        f64::MIN_POSITIVE,
        5e-324,
        -5e-324,
        1e-300,
        0.5,
        2.0f64.powi(62),
        2.0f64.powi(63),
        -(2.0f64.powi(63)),
        2.0f64.powi(64),
    ];
    for _ in 0..2000 {
        let d = f64::from_bits(lcg(&mut s));
        if d.is_finite() && d != 0.0 {
            ds.push(d);
        }
    }
    for &d in &ds {
        let (neg, sig, exp) = decompose(d);
        // Historical road.
        let (u, scale_ref) = bd_double_unscaled_reference(sig, exp);
        let u = if neg { u.neg_value() } else { u };
        // Lean road.
        let (le, scale): (Vec<u32>, i32) = if exp >= 0 {
            let (w, n) = bd_double_shifted_le(sig, exp as u32).unwrap();
            (w[..n].to_vec(), 0)
        } else {
            let (m, sc) = bd_double_unscaled_reference(sig, exp);
            (m.mag_le().to_vec(), sc)
        };
        assert_eq!(scale, scale_ref, "{d:e}");
        assert_eq!(le.as_slice(), u.mag_le(), "{d:e}");
        assert_eq!(bd_compact_of(neg, &le), historical_compact(&u), "{d:e}");
    }
}

#[test]
fn mag_is_written_most_significant_first() {
    let le = [1u32, 2, 0xFFFF_FFFF];
    let mut be = [0i32; 3];
    bignum_le_to_be(&le, &mut be);
    assert_eq!(be, [-1, 2, 1]);
    let mut empty: [i32; 0] = [];
    bignum_le_to_be(&[], &mut empty);
}

fn field(name: &str, descriptor: &str, slot: usize, is_static: bool) -> FieldMetadata {
    FieldMetadata {
        name: name.to_string(),
        descriptor: descriptor.to_string(),
        access_flags: 0,
        slot_index: slot,
        declaring_class_id: cratonvm_types::ClassId::new(0),
        is_static,
    }
}

/// A field the class metadata does not declare where and as the JDK does
/// refuses the lean boundary (the caller keeps the historical road).
#[test]
fn slots_are_served_only_where_the_metadata_agrees() {
    let fields = vec![
        field("signum", "I", 0, false),
        field("mag", "[I", 1, false),
        field("ZERO", "Ljava/math/BigInteger;", 0, true),
    ];
    assert_eq!(bignum_lean_slot(&fields, "signum", 0, "I"), Some((0, b'I')));
    assert_eq!(bignum_lean_slot(&fields, "mag", 1, "[I"), Some((1, b'[')));
    assert_eq!(bignum_lean_slot(&fields, "signum", 1, "I"), None);
    assert_eq!(bignum_lean_slot(&fields, "mag", 1, "[J"), None);
    assert_eq!(bignum_lean_slot(&fields, "intCompact", 2, "J"), None);
    assert_eq!(
        bignum_lean_slot(&fields, "ZERO", 0, "Ljava/math/BigInteger;"),
        None
    );
}
