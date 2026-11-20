use super::*;
use crate::bigint::BigInt;

/// Schoolbook `a * b mod m` on limb slices, via `BigInt` — the oracle the
/// Montgomery product is checked against.
fn ref_mulmod(a: &[u32], b: &[u32], m: &[u32]) -> Vec<u32> {
    let ba = BigInt::from_le_words(false, a.to_vec());
    let bb = BigInt::from_le_words(false, b.to_vec());
    let bm = BigInt::from_le_words(false, m.to_vec());
    ba.mul(&bb).modulo(&bm).mag_le().to_vec()
}

fn pad(v: &[u32], n: usize) -> Vec<u32> {
    let mut o = v.to_vec();
    o.resize(n, 0);
    o
}

fn xorshift(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

fn rand_odd(state: &mut u64, limbs: usize) -> Vec<u32> {
    let mut v: Vec<u32> = (0..limbs).map(|_| xorshift(state) as u32).collect();
    v[0] |= 1; // odd
    let top = limbs - 1;
    v[top] |= 1 << 31; // full width, so `n` is what we asked for
    v
}

/// `R mod m` and `R^2 mod m` for a context, via the division path.
fn radix_constants(m: &[u32], n: usize) -> (Vec<u32>, Vec<u32>) {
    let bm = BigInt::from_le_words(false, m.to_vec());
    let one = BigInt::from_le_words(false, vec![1]);
    let r1 = pad(one.shl(32 * n as u32).modulo(&bm).mag_le(), n);
    let r2 = pad(one.shl(64 * n as u32).modulo(&bm).mag_le(), n);
    (r1, r2)
}

#[test]
fn neg_inv_is_the_inverse_of_every_odd_word() {
    let mut state = 0x1234_5678_9abc_def1u64;
    for _ in 0..2000 {
        let m0 = (xorshift(&mut state) as u32) | 1;
        let ni = neg_inv_2_32(m0);
        // -m0^-1 * m0 == -1 == 0xFFFF_FFFF (mod 2^32)
        assert_eq!(ni.wrapping_mul(m0), u32::MAX, "neg_inv for {m0:#x}");
    }
    assert_eq!(neg_inv_2_32(1).wrapping_mul(1), u32::MAX);
    assert_eq!(neg_inv_2_32(u32::MAX).wrapping_mul(u32::MAX), u32::MAX);
}

#[test]
fn new_rejects_moduli_with_no_montgomery_form() {
    assert!(Montgomery::new(&[]).is_none(), "zero");
    assert!(Montgomery::new(&[0, 0]).is_none(), "zero, padded");
    assert!(Montgomery::new(&[1]).is_none(), "one");
    assert!(Montgomery::new(&[2]).is_none(), "even");
    assert!(Montgomery::new(&[4, 7]).is_none(), "even, multi-limb");
    assert!(Montgomery::new(&[0, 1]).is_none(), "2^32, even");
    assert!(Montgomery::new(&[3]).is_some());
    // Trailing zero limbs are trimmed, not counted.
    let m = Montgomery::new(&[7, 0, 0]).unwrap();
    assert_eq!(m.limbs(), 1);
}

#[test]
fn mul_matches_schoolbook_mulmod_over_random_operands() {
    let mut state = 0xdead_beef_0bad_f00du64;
    for limbs in 1..=9 {
        for _ in 0..40 {
            let m = rand_odd(&mut state, limbs);
            let mont = Montgomery::new(&m).unwrap();
            let n = mont.limbs();
            let (r1, r2) = radix_constants(&m, n);
            // 1 in Montgomery form really is R mod m.
            assert_eq!(mont.out_of_mont(&r1), pad(&[1], n), "r1 is 1*R");

            for _ in 0..8 {
                let a: Vec<u32> = (0..n).map(|_| xorshift(&mut state) as u32).collect();
                let b: Vec<u32> = (0..n).map(|_| xorshift(&mut state) as u32).collect();
                let ba = pad(&ref_mulmod(&a, &[1], &m), n);
                let bb = pad(&ref_mulmod(&b, &[1], &m), n);
                // mont.mul(a*R, b*R) == (a*b)*R
                let am = mont.mul(&ba, &r2);
                let bmm = mont.mul(&bb, &r2);
                let prod = mont.mul(&am, &bmm);
                assert_eq!(
                    mont.out_of_mont(&prod),
                    pad(&ref_mulmod(&ba, &bb, &m), n),
                    "montgomery mul, {limbs} limbs"
                );
                // Round-tripping through Montgomery form is the identity.
                assert_eq!(mont.out_of_mont(&am), ba, "to/from mont round trip");
                // The result is always fully reduced.
                assert_eq!(
                    cmp_mag(&prod, mont.modulus()),
                    std::cmp::Ordering::Less,
                    "mul result not reduced"
                );
            }
            // Boundary operands: 0, 1, m-1.
            let zero = vec![0u32; n];
            let mut mm1 = m.clone();
            mm1[0] -= 1; // m odd, so m-1 needs no borrow
            let cases = [zero, pad(&[1], n), mm1];
            for a in &cases {
                for b in &cases {
                    let am = mont.mul(a, &r2);
                    let bmn = mont.mul(b, &r2);
                    let got = mont.out_of_mont(&mont.mul(&am, &bmn));
                    assert_eq!(got, pad(&ref_mulmod(a, b, &m), n), "boundary mul");
                }
            }
        }
    }
}

/// The window machinery is where an off-by-one hides. Drive `modpow_odd`
/// across every exponent bit length that changes the window width, against
/// a reference square-and-multiply that shares none of its code.
#[test]
fn modpow_odd_matches_square_and_multiply_across_window_widths() {
    fn ref_modpow(base: &[u32], exp: &[u32], m: &[u32]) -> Vec<u32> {
        let bm = BigInt::from_le_words(false, m.to_vec());
        let mut result = BigInt::from_le_words(false, vec![1]).modulo(&bm);
        let mut b = BigInt::from_le_words(false, base.to_vec()).modulo(&bm);
        for i in 0..bit_len(exp) {
            if (exp[i / 32] >> (i % 32)) & 1 == 1 {
                result = result.mul(&b).modulo(&bm);
            }
            b = b.mul(&b).modulo(&bm);
        }
        result.mag_le().to_vec()
    }

    let mut state = 0x0f0f_1e1e_2d2d_3c3cu64;
    for limbs in [1usize, 2, 3, 5, 8] {
        let m = rand_odd(&mut state, limbs);
        let mont = Montgomery::new(&m).unwrap();
        let n = mont.limbs();
        let (r1, r2) = radix_constants(&m, n);
        // Every `window_width` boundary, plus one either side of each.
        let widths = [
            1usize, 2, 3, 4, 5, 6, 23, 24, 25, 26, 95, 96, 97, 98, 319, 320, 321, 322, 959,
            960, 961, 962, 2048, 2687, 2688, 2689, 2690,
        ];
        for &ebits in &widths {
            for trial in 0..3 {
                // Exponent with exactly `ebits` bits.
                let ewords = ebits.div_ceil(32);
                let mut e: Vec<u32> =
                    (0..ewords).map(|_| xorshift(&mut state) as u32).collect();
                let topbit = (ebits - 1) % 32;
                e[ewords - 1] &= (1u32 << topbit) - 1 | (1u32 << topbit);
                e[ewords - 1] |= 1u32 << topbit;
                if trial == 1 {
                    // All-ones exponent: every window is full.
                    for w in e.iter_mut() {
                        *w = u32::MAX;
                    }
                    e[ewords - 1] = if topbit == 31 {
                        u32::MAX
                    } else {
                        (1u32 << (topbit + 1)) - 1
                    };
                } else if trial == 2 {
                    // Single set bit: every window but one is zero.
                    for w in e.iter_mut() {
                        *w = 0;
                    }
                    e[ewords - 1] = 1u32 << topbit;
                }
                assert_eq!(bit_len(&e), ebits, "exponent construction");

                let base: Vec<u32> = (0..n).map(|_| xorshift(&mut state) as u32).collect();
                let base_mod = ref_mulmod(&base, &[1], &m);
                let got = modpow_odd(&mont, &base_mod, &e, &r1, &r2);
                let want = ref_modpow(&base_mod, &e, &m);
                assert_eq!(got, want, "modpow {limbs} limbs, {ebits}-bit exponent");
            }
        }
    }
}

#[test]
fn modpow_odd_handles_degenerate_bases_and_exponents() {
    let mut state = 0x7777_8888_9999_aaaau64;
    for limbs in 1..=4 {
        let m = rand_odd(&mut state, limbs);
        let mont = Montgomery::new(&m).unwrap();
        let n = mont.limbs();
        let (r1, r2) = radix_constants(&m, n);
        let mut mm1 = m.clone();
        mm1[0] -= 1;

        // x^0 == 1 for every base, including 0.
        for base in [vec![], vec![1], vec![2], mm1.clone()] {
            assert_eq!(modpow_odd(&mont, &base, &[], &r1, &r2), vec![1], "x^0 == 1");
            assert_eq!(
                modpow_odd(&mont, &base, &[0, 0], &r1, &r2),
                vec![1],
                "x^0 == 1, padded exponent"
            );
        }
        // 0^e == 0 for e > 0.
        for e in [vec![1u32], vec![2], vec![0xFFFF_FFFF, 0xFFFF_FFFF]] {
            assert_eq!(modpow_odd(&mont, &[], &e, &r1, &r2), Vec::<u32>::new());
        }
        // 1^e == 1.
        assert_eq!(modpow_odd(&mont, &[1], &[0xDEAD_BEEF], &r1, &r2), vec![1]);
        // x^1 == x.
        let base = ref_mulmod(&[0x1234_5678, 0x9abc_def0], &[1], &m);
        assert_eq!(modpow_odd(&mont, &base, &[1], &r1, &r2), base);
    }
}

#[test]
fn bit_len_counts_the_highest_set_bit() {
    assert_eq!(bit_len(&[]), 0);
    assert_eq!(bit_len(&[0]), 0);
    assert_eq!(bit_len(&[0, 0, 0]), 0);
    assert_eq!(bit_len(&[1]), 1);
    assert_eq!(bit_len(&[2]), 2);
    assert_eq!(bit_len(&[0xFFFF_FFFF]), 32);
    assert_eq!(bit_len(&[0, 1]), 33);
    assert_eq!(bit_len(&[0xFFFF_FFFF, 0xFFFF_FFFF]), 64);
    assert_eq!(bit_len(&[5, 1, 0, 0]), 33);
}

#[test]
fn window_at_reads_little_endian_bit_runs() {
    // 0b1011_0110 = 0xB6
    let e = [0xB6u32];
    assert_eq!(window_at(&e, 0, 4), 0x6);
    assert_eq!(window_at(&e, 4, 4), 0xB);
    assert_eq!(window_at(&e, 6, 4), 0x2); // bits 6,7 = 1,0 -> 0b0010
    assert_eq!(window_at(&e, 32, 4), 0); // past the end
                                         // A window straddling the word boundary.
    let e2 = [0x8000_0000u32, 0x0000_0001];
    assert_eq!(window_at(&e2, 31, 2), 0b11);
    assert_eq!(window_at(&e2, 30, 4), 0b0110);
}

#[test]
fn window_width_is_monotone_and_bounded() {
    let mut prev = 0;
    for bits in [
        1usize, 4, 5, 24, 25, 96, 97, 320, 321, 960, 961, 2688, 2689, 8192,
    ] {
        let w = window_width(bits);
        assert!(w >= prev, "width regressed at {bits}");
        assert!((1..=7).contains(&w), "width {w} out of range at {bits}");
        prev = w;
    }
}
