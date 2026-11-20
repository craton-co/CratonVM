use super::{bi_gcd_str, bi_to_byte_array_int, bi_to_byte_array_str, bigint_mul_pow10_memo};
use crate::bigint::BigInt;

fn lcg(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *state >> 11
}

fn random(state: &mut u64, words: usize, neg: bool) -> BigInt {
    let mag: Vec<u32> = (0..words).map(|_| lcg(state) as u32).collect();
    BigInt::from_le_words(neg, mag)
}

#[test]
fn mul_pow10_memo_is_the_uncached_product() {
    let mut st = 0x10_u64;
    let values = [
        BigInt::zero(),
        BigInt::from_decimal("1"),
        BigInt::from_decimal("-1"),
        random(&mut st, 3, false),
        random(&mut st, 7, true),
        random(&mut st, 90, false),
    ];
    let ns = (0u32..64).chain([27, 28, 100, 511, 1000, 2199, 2200, 2201, 3000]);
    for n in ns {
        let p = crate::bigint_pow5(n).shl(n);
        for v in &values {
            // Twice: the second call is served from the memo.
            assert_eq!(bigint_mul_pow10_memo(v, n), v.mul(&p), "n {n}");
            assert_eq!(bigint_mul_pow10_memo(v, n), v.mul(&p), "n {n}, memo");
        }
        assert_eq!(
            crate::bigint_mul_pow10(&values[4], n as i32),
            values[4].mul(&p),
            "lib n {n}"
        );
    }
}

#[test]
fn gcd_limb_road_is_exact() {
    let mut st = 0x9cd_u64;
    for words in [1usize, 2, 3, 5, 9, 17, 40] {
        let g0 = random(&mut st, words, false).add(&BigInt::from_decimal("1"));
        let k = random(&mut st, words + 1, false);
        // `k` and `k + 1` are coprime, so the gcd is exactly `g0`.
        let a = g0.mul(&k);
        let b = g0.mul(&k.add(&BigInt::from_decimal("1")));
        let (sa, sb, sg) = (a.to_decimal(), b.to_decimal(), g0.to_decimal());
        assert_eq!(bi_gcd_str(&sa, &sb), sg, "words {words}");
        assert_eq!(
            bi_gcd_str(&format!("-{sa}"), &sb),
            sg,
            "neg a, words {words}"
        );
        assert_eq!(
            bi_gcd_str(&sb, &format!("-{sa}")),
            sg,
            "swapped, words {words}"
        );
        assert_eq!(bi_gcd_str("0", &sa), sa, "gcd(0, a)");
        assert_eq!(bi_gcd_str(&sa, "0"), sa, "gcd(a, 0)");
        assert_eq!(bi_gcd_str(&sa, &sa), sa, "gcd(a, a)");
    }
    assert_eq!(bi_gcd_str("0", "0"), "0");
    assert_eq!(bi_gcd_str("12", "-18"), "6");
}

/// `toByteArray()` by its specification: `bitLength() / 8 + 1` bytes of
/// the two's-complement value, big-endian.
fn to_byte_array_spec(v: &BigInt) -> Vec<u8> {
    let len = (v.bit_length() / 8 + 1) as usize;
    let x = if v.is_neg() {
        BigInt::from_decimal("1").shl(8 * len as u32).add(v)
    } else {
        v.clone()
    };
    let mut le: Vec<u8> = x.mag_le().iter().flat_map(|w| w.to_le_bytes()).collect();
    le.resize(len, 0);
    le.reverse();
    le
}

#[test]
fn to_byte_array_limb_road_matches_the_spec() {
    let mut st = 0xb7e5_u64;
    let mut values: Vec<BigInt> = Vec::new();
    for words in 1usize..24 {
        values.push(random(&mut st, words, false));
        values.push(random(&mut st, words, true));
    }
    // Powers of two (the negative ones need one byte less) and their neighbours.
    let one = BigInt::from_decimal("1");
    for bits in [63u32, 64, 65, 127, 128, 255, 256, 257, 511, 1000] {
        let p = one.shl(bits);
        for v in [p.clone(), p.sub(&one), p.add(&one)] {
            values.push(v.neg_value());
            values.push(v);
        }
    }
    for k in -300i64..300 {
        values.push(BigInt::from_decimal(&k.to_string()));
    }
    for v in values {
        let want = to_byte_array_spec(&v);
        assert_eq!(bi_to_byte_array_int(&v), want, "int {}", v.to_decimal());
        if v.is_zero() {
            continue;
        }
        assert_eq!(
            bi_to_byte_array_str(&v.to_decimal()),
            want,
            "{}",
            v.to_decimal()
        );
    }
}
