use super::*;
use num_bigint::BigUint;

// Limb vectors captured from JDK 25's real

// MontgomeryIntegerPolynomialP256.{mult,square} (ecprobe_tmp/MontSamples.java).
// Each MULT entry is (a_limbs, b_limbs, expected_r_limbs).
#[rustfmt::skip]
const MULT_VECTORS: &[(&[i64; 5], &[i64; 5], &[i64; 5])] = &[
    // zero * x
    (&[0x0, 0x0, 0x0, 0x0, 0x0],
     &[0xdeadbeefd, 0x300000000000, 0xffffff2152411, 0x1102fffffffff, 0xeadbeeef1524],
     &[0x0, 0x0, 0x0, 0x0, 0x0]),
    // one(mont) * x
    (&[0x10, 0xf000000000000, 0xfffffffffffff, 0xffeffffffffff, 0xfffff],
     &[0x99998962fc961, 0x68acf1356999, 0x66666676af37c, 0xf1377530eca96, 0xfc962fc868ac],
     &[0x99998962fc961, 0x68acf1356999, 0x66666676af37c, 0xf1377530eca96, 0xfc962fc868ac]),
    // x * one(mont)
    (&[0xba997530eca87, 0x790000000fedc, 0x2345668acf135, 0x3578fffffff01, 0x654320efacf1],
     &[0x10, 0xf000000000000, 0xfffffffffffff, 0xffeffffffffff, 0xfffff],
     &[0xba997530eca87, 0x790000000fedc, 0x2345668acf135, 0x3578fffffff01, 0x654320efacf1]),
    // (p-1) * (p-1)
    (&[0xfffffffffffef, 0x10fffffffffff, 0x0, 0x11000000000, 0xffffffef0000],
     &[0xfffffffffffef, 0x10fffffffffff, 0x0, 0x11000000000, 0xffffffef0000],
     &[0x10, 0xf000000000000, 0xfffffffffffff, 0xffeffffffffff, 0xfffff]),
    // random 1
    (&[0x842eff98a9759, 0xe0996c9949873, 0xadd036e077ccf, 0x9c4a28b369013, 0x9b18fd15a035],
     &[0xfdeef74613ef0, 0xd1abc768a0775, 0x7492792376809, 0x871743f7451f6, 0x51d467691430],
     &[0x5d345a7ca4667, 0xb48fc3b761b32, 0x3eb9eb427c5d3, 0xf10a7242677fc, 0x1c5bea89843c]),
    // random 2
    (&[0xfffee789abcc6, 0x54aaaaaab7fff, 0x21876543, 0x431f555555400, 0x789abcbf8765],
     &[0xaaa899999996e, 0x5eccccccaaaaa, 0x5555575555555, 0x77e5333333755, 0xeeeeeee57777],
     &[0xca82d7fcf7571, 0x251b27abfacdd, 0x1eb83d0c8db0e, 0x8632fde04ab05, 0xb44cc347d916]),
];

// SQUARE entries (a_limbs, expected_r_limbs) from the real JDK square().
#[rustfmt::skip]
const SQUARE_VECTORS: &[(&[i64; 5], &[i64; 5])] = &[
    (&[0x842eff98a9759, 0xe0996c9949873, 0xadd036e077ccf, 0x9c4a28b369013, 0x9b18fd15a035],
     &[0x4f4424402354b, 0xb23fcdfd72be7, 0xf1bea8600adff, 0x68720d8f836c7, 0x7366dace0f4b]),
    (&[0x10, 0xf000000000000, 0xfffffffffffff, 0xffeffffffffff, 0xfffff],
     &[0x10, 0xf000000000000, 0xfffffffffffff, 0xffeffffffffff, 0xfffff]),
    (&[0xfffffffffffef, 0x10fffffffffff, 0x0, 0x11000000000, 0xffffffef0000],
     &[0x10, 0xf000000000000, 0xfffffffffffff, 0xffeffffffffff, 0xfffff]),
];

// (a, b, r) from HotSpot 25.0.4+7 `-Xint` (the Java body), out-of-range
// limbs: `SbrW5Bigint.java limbs`, 2026-09-29.
#[rustfmt::skip]
const HOTSPOT_EDGE_VECTORS: &[(&[i64; 5], &[i64; 5], &[i64; 5])] = &[
    (&[-1, -1, -1, -1, -1], &[-1, -1, -1, -1, -1],
     &[2903124356194211, 3672921276709762, 3375846714481153, 1410745367181051, 233113644889080]),
    (&[i64::MIN, i64::MIN, i64::MIN, i64::MIN, i64::MIN], &[1, 2, 3, 4, 5],
     &[844424795945008, 846073794949328, 3660823158874496, 2251802764921600, 17592186051841]),
    (&[4503599627370496, 4503599627370496, 4503599627370496, 4503599627370496, 4503599627370496],
     &[4503599627370495, 4503599627370495, 4503599627370495, 4503599627370495, 4503599627370495],
     &[4204395025727487, 4221986406465536, 4220956151185664, 4221024868565248, 4502491525808386]),
    (&[4503599627370495, 17592186044415, 0, 68719476736, 281474976645120],
     &[4503599627370495, 17592186044415, 0, 68719476736, 281474976645120],
     &[0, 0, 0, 0, 0]),
    (&[-1, 0, -1, 0, -1], &[i64::MAX, 12345, i64::MAX, -98765, 7],
     &[2445590755185333, 1147828580619821, 1969976928259022, 3121627012799220, 21500584191914]),
];

/// An independent, value-level reference: `a*b*2^-260 mod p` through
/// `num-bigint`, canonical limbs out. For canonical inputs the Java body
/// is fully reduced, so the two must agree limb for limb.
fn value_mult(a: &[i64; 5], b: &[i64; 5]) -> [i64; 5] {
    let p = BigUint::parse_bytes(
        b"ffffffff00000001000000000000000000000000ffffffffffffffffffffffff",
        16,
    )
    .unwrap();
    let dec = |l: &[i64; 5]| {
        let mut x = BigUint::from(0u8);
        for i in (0..5).rev() {
            x = (x << 52u32) + BigUint::from(l[i] as u64);
        }
        x
    };
    let r = (BigUint::from(1u8) << 260u32) % &p;
    let r_inv = r.modpow(&(&p - 2u32), &p);
    let mut v = dec(a) * dec(b) % &p * r_inv % &p;
    let mask = BigUint::from(LIMB_MASK as u64);
    let mut out = [0i64; 5];
    for slot in out.iter_mut() {
        *slot = (&v & &mask).iter_u64_digits().next().unwrap_or(0) as i64;
        v >>= 52u32;
    }
    out
}

#[test]
fn mult_matches_jdk_byte_for_byte() {
    for (a, b, expect) in MULT_VECTORS {
        assert_eq!(
            &jdk_mult(a, b),
            *expect,
            "mult mismatch for a={a:x?} b={b:x?}"
        );
    }
}

#[test]
fn square_matches_jdk_and_equals_mult_self() {
    for (a, expect) in SQUARE_VECTORS {
        assert_eq!(&jdk_mult(a, a), *expect, "square mismatch for a={a:x?}");
    }
}

/// Random canonical field elements: the transcription agrees with the
/// value-level reference (an independent code path) and stays canonical.
#[test]
fn canonical_inputs_agree_with_the_value_reference() {
    let p_limbs = MODULUS;
    let mut s: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    let lt_p = |x: &[i64; 5]| {
        for i in (0..5).rev() {
            if x[i] != p_limbs[i] {
                return x[i] < p_limbs[i];
            }
        }
        false
    };
    let mut checked = 0;
    while checked < 2000 {
        let mut a = [0i64; 5];
        let mut b = [0i64; 5];
        for i in 0..5 {
            a[i] = (next() as i64) & LIMB_MASK;
            b[i] = (next() as i64) & LIMB_MASK;
        }
        a[4] &= 0xffff_ffff_ffff; // below 2^256
        b[4] &= 0xffff_ffff_ffff;
        if !lt_p(&a) || !lt_p(&b) {
            continue;
        }
        let got = jdk_mult(&a, &b);
        assert_eq!(got, value_mult(&a, &b), "a={a:x?} b={b:x?}");
        for limb in got {
            assert!(
                (0..(1i64 << 52)).contains(&limb),
                "non-canonical limb {limb:#x}"
            );
        }
        checked += 1;
    }
}

/// Out-of-range limbs are where the old value-level body diverged: the
/// Java body is plain wrapping `long` arithmetic there, not a field
/// operation. The expected limbs are HotSpot 25.0.4+7's (`-Xint`, the Java
/// body) for `MontgomeryIntegerPolynomialP256.ONE.mult`, printed by
/// `SbrW5Bigint.java limbs`.
#[test]
fn out_of_range_limbs_follow_the_java_arithmetic() {
    for (a, b, expect) in HOTSPOT_EDGE_VECTORS {
        assert_eq!(&jdk_mult(a, b), *expect, "a={a:x?} b={b:x?}");
    }
}
