use super::*;

fn lcg(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *state >> 11
}

fn random(state: &mut u64, words: usize) -> BigInt {
    let mag: Vec<u32> = (0..words).map(|_| lcg(state) as u32).collect();
    BigInt::normalize(mag, lcg(state) & 1 == 1)
}

fn check(x: &BigInt, base_words: usize, recip_base_bits: u64) {
    let want = x.to_decimal_chunked();
    let got = x
        .to_decimal_dc(base_words, recip_base_bits)
        .expect("the divide-and-conquer conversion answers");
    assert_eq!(
        got, want,
        "base_words {base_words}, recip_base_bits {recip_base_bits}"
    );
}

#[test]
fn dc_conversion_matches_the_chunked_loop_on_random_values() {
    let mut st = 0x5eed_b16d_ec14_u64;
    for words in [1usize, 2, 3, 5, 8, 13, 21, 34, 55, 89, 144, 233] {
        for _ in 0..6 {
            let x = random(&mut st, words);
            check(&x, 1, 64);
            check(&x, 3, 64);
            check(&x, 8, 200);
        }
    }
}

/// Decimal zero runs are where a padding mistake shows: `10^k`, its
/// neighbours, and a multiple of it, for every `k` below 400 (every
/// `D_k` boundary of the small levels included).
#[test]
fn dc_conversion_pads_zero_runs_exactly() {
    let one = BigInt::from_le_words(false, vec![1]);
    let ten = BigInt::from_le_words(false, vec![10]);
    let seven = BigInt::from_le_words(false, vec![7]);
    let mut p = one.clone();
    for _ in 0..400 {
        for v in [p.clone(), p.sub(&one), p.add(&one), p.mul(&seven).add(&one)] {
            check(&v, 1, 64);
            check(&v, 2, 96);
            check(&v.neg_value(), 1, 64);
        }
        p = p.mul(&ten);
    }
}

#[test]
fn dc_conversion_at_the_shipping_thresholds() {
    let mut st = 0xdec1_3a1_u64;
    for words in [TO_DECIMAL_DC_MIN_WORDS, 700, 1500] {
        let x = random(&mut st, words);
        check(&x, TO_DECIMAL_DC_BASE_WORDS, RECIPROCAL_BASE_BITS);
        assert_eq!(x.to_decimal(), x.to_decimal_chunked());
    }
    // A power of ten just under a wide level boundary, and one over it.
    let big = BigInt::from_le_words(false, vec![10]);
    let mut p = BigInt::from_le_words(false, vec![1]);
    for _ in 0..(9 * 1024) {
        p = p.mul(&big);
    }
    let one = BigInt::from_le_words(false, vec![1]);
    for v in [p.sub(&one), p.clone(), p.add(&one)] {
        check(&v, TO_DECIMAL_DC_BASE_WORDS, RECIPROCAL_BASE_BITS);
        check(&v, 16, 512);
    }
}

#[test]
fn reciprocal_is_the_exact_floor() {
    let mut st = 7u64;
    let one = BigInt::from_le_words(false, vec![1]);
    for bits in [65u32, 100, 129, 200, 257, 500, 1000, 3000] {
        for _ in 0..8 {
            let words = bits.div_ceil(32) as usize;
            let mut mag: Vec<u32> = (0..words).map(|_| lcg(&mut st) as u32).collect();
            let top = (bits - 1) % 32;
            mag[words - 1] &= (1u32 << top) | ((1u32 << top) - 1);
            mag[words - 1] |= 1u32 << top;
            let d = BigInt::normalize(mag, false);
            assert_eq!(d.magnitude_bits(), u64::from(bits));
            let r = BigInt::reciprocal(&d, u64::from(bits), 64).expect("reciprocal");
            assert_eq!(r, one.shl(2 * bits).div(&d), "bits {bits}");
        }
    }
}

/// The correction is what makes the conversion exact, so it must recover
/// from ANY estimate: close ones by stepping, wild ones through Knuth D.
#[test]
fn correct_quotient_recovers_from_any_estimate() {
    let x = BigInt::from_decimal("123456789012345678901234567890");
    let d = BigInt::from_decimal("98765432109876543");
    let want = x.divmod(&d);
    let one = BigInt::from_le_words(false, vec![1]);
    let two = one.add(&one);
    let near = [
        want.0.clone(),
        want.0.add(&one),
        want.0.sub(&one),
        want.0.add(&two),
        want.0.sub(&two),
    ];
    let far = ["0", "-5", "1", "999999999999999999999"].map(BigInt::from_decimal);
    for q in near.into_iter().chain(far) {
        let r = x.sub(&q.mul(&d));
        assert_eq!(
            BigInt::correct_quotient(&x, &d, q.clone(), r),
            want,
            "estimate {q:?}"
        );
    }
}
