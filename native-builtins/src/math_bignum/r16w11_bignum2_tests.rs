use super::{
    bd_compact_of, bd_compare_by_bits, bd_compare_unscaled, bd_compare_views,
    bd_double_compact_mag, bd_double_unscaled_reference, bigint_from_i64, BignumView,
    BIGNUM_LEAN_STACK_WORDS,
};
use crate::bigint::BigInt;

fn lcg(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    *state ^ (*state >> 29)
}

/// `mag` as the JDK stores it (big-endian `int`s) for `v`.
fn be_of(v: &BigInt) -> Vec<i32> {
    v.mag_le().iter().rev().map(|&w| w as i32).collect()
}

fn view_of(v: &BigInt) -> BignumView {
    BignumView::from_be(v.signum(), &be_of(v)).expect("fits the stack buffer")
}

/// Random values: zero, one- and two-limb (compact range), wide, with
/// shared prefixes so that equal lengths and equal top limbs occur.
fn values(seed: u64, n: usize) -> Vec<BigInt> {
    let mut s = seed;
    let mut out = vec![
        BigInt::zero(),
        bigint_from_i64(1),
        bigint_from_i64(-1),
        bigint_from_i64(i64::MAX),
        bigint_from_i64(-i64::MAX),
        BigInt::from_le_words(false, vec![0, 0x8000_0000]),
        BigInt::from_le_words(true, vec![0, 0x8000_0000]),
    ];
    let prefix = [lcg(&mut s) as u32, lcg(&mut s) as u32, lcg(&mut s) as u32];
    for i in 0..n {
        let len = (lcg(&mut s) % 7) as usize;
        let mut words: Vec<u32> = (0..len).map(|_| lcg(&mut s) as u32).collect();
        if i % 3 == 0 && len >= 3 {
            let k = words.len();
            words[k - 3..].copy_from_slice(&prefix);
        }
        out.push(BigInt::from_le_words(lcg(&mut s) & 1 == 1, words));
    }
    out
}

#[test]
fn views_order_and_measure_like_bigint() {
    let vs = values(0xB16_2, 300);
    for a in &vs {
        let va = view_of(a);
        assert_eq!(va.to_bigint(), *a);
        assert_eq!(va.signum(), a.signum());
        assert_eq!(va.magnitude_bits(), a.magnitude_bits());
        for b in &vs {
            assert_eq!(va.cmp_value(&view_of(b)), a.cmp(b));
        }
    }
}

/// `bi_read_int` reads a `mag` with high zero words and a negative
/// `signum` over an all-zero `mag` as normalised values; so does the view.
#[test]
fn views_normalise_like_bi_read_int() {
    let v = BignumView::from_be(-1, &[0, 0, 7, -1]).unwrap();
    assert_eq!(
        v.to_bigint(),
        BigInt::from_le_words(true, vec![u32::MAX, 7])
    );
    let z = BignumView::from_be(-1, &[0, 0]).unwrap();
    assert_eq!(z.signum(), 0);
    assert_eq!(z.to_bigint(), BigInt::zero());
    assert_eq!(BignumView::from_be(5, &[3]).unwrap().signum(), 1);
    let wide = vec![1i32; BIGNUM_LEAN_STACK_WORDS + 1];
    assert!(BignumView::from_be(1, &wide).is_none());
    assert!(BignumView::from_be(1, &wide[..BIGNUM_LEAN_STACK_WORDS]).is_some());
    for v in [
        0i64,
        1,
        -1,
        42,
        -42,
        i64::MAX,
        -i64::MAX,
        1 << 32,
        -(1 << 40),
    ] {
        assert_eq!(
            BignumView::from_i64(v).to_bigint(),
            bigint_from_i64(v),
            "{v}"
        );
    }
}

/// The borrowed `compareTo` equals `native_bd_compare_to_exact`'s road for
/// every pair, both settings of `CRATONVM_BIGDECIMAL_COMPARE_BY_BITS`.
#[test]
fn borrowed_bigdecimal_compare_is_the_exact_road() {
    let vs = values(0xC0_FFEE, 120);
    let scales = [0, 1, -1, 3, 18, 40, -25, i32::MAX, i32::MIN];
    let mut s = 7u64;
    for a in &vs {
        for b in &vs {
            let sa = scales[(lcg(&mut s) % scales.len() as u64) as usize];
            let sb = if lcg(&mut s) & 1 == 0 {
                sa
            } else {
                scales[(lcg(&mut s) % scales.len() as u64) as usize]
            };
            for by_bits in [false, true] {
                let want = (if by_bits {
                    bd_compare_by_bits(a, sa, b, sb)
                } else {
                    None
                })
                .unwrap_or_else(|| bd_compare_unscaled(a, sa, b, sb));
                let got = bd_compare_views(&view_of(a), sa, &view_of(b), sb, by_bits);
                assert_eq!(got, want, "{a:?}@{sa} vs {b:?}@{sb} by_bits={by_bits}");
            }
        }
    }
}

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

/// The compact decision of `new BigDecimal(double)` with a nonzero binary
/// exponent equals the compact rule applied to the exact unscaled value.
#[test]
fn double_compact_mag_is_the_compact_rule() {
    let mut s = 0xD0_0B1E_u64;
    let mut ds = vec![
        2.0,
        0.5,
        0.25,
        1.5,
        3.0,
        1e10,
        -1e10,
        1e18,
        9.223372036854775e18,
        9.223372036854776e18,
        2.0f64.powi(62),
        2.0f64.powi(63),
        -(2.0f64.powi(63)),
        0.1,
        1e-5,
        5e-324,
        f64::MAX,
        1.0 / 1024.0,
        3.0 / 8.0,
    ];
    for _ in 0..3000 {
        let d = f64::from_bits(lcg(&mut s));
        if d.is_finite() && d != 0.0 {
            ds.push(d);
        }
    }
    // Small dyadic and decimal-friendly values, the compact population.
    for k in 1..400u32 {
        ds.push(f64::from(k) / 64.0);
        ds.push(f64::from(k) * 1024.0);
    }
    for &d in &ds {
        let (_, sig, exp) = decompose(d);
        let (u, _) = bd_double_unscaled_reference(sig, exp);
        let want = bd_compact_of(false, u.mag_le()).map(|v| v as u64);
        assert_eq!(bd_double_compact_mag(sig, exp), want, "{d:e}");
    }
}
