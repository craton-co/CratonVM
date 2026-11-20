use super::*;
use crate::{
    bi_add_str, bi_bitwise_and, bi_bitwise_or, bi_bitwise_xor, bi_compare, bi_div_str,
    bi_is_probable_prime_str, bi_mod_inverse_str, bi_mod_pow_str, bi_mod_str, bi_mul_str,
    bi_shift_left_str, bi_shift_right_str, bi_sub_str,
};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

// Deterministic LCG so the spread is reproducible without a rand dep.
fn lcg(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *state
}

/// Random positive odd `BigInt` of exactly `bits` bits (top bit set, low
/// bit set). `bits >= 1`; `bits == 1` yields 1.
fn rand_bits_odd(state: &mut u64, bits: u32) -> BigInt {
    let words = bits.div_ceil(32) as usize;
    let mut mag: Vec<u32> = (0..words).map(|_| lcg(state) as u32).collect();
    let top = (bits - 1) % 32;
    mag[words - 1] &= (1u32 << top) | ((1u32 << top) - 1);
    mag[words - 1] |= 1u32 << top;
    mag[0] |= 1;
    BigInt::normalize(mag, false)
}

/// Random signed decimal string, 1..=48 digits, ~half negative.
fn rand_decimal(state: &mut u64) -> String {
    let len = (lcg(state) % 48) as usize + 1;
    let mut s = String::new();
    if lcg(state) & 1 == 0 {
        s.push('-');
    }
    // First digit 1..=9 (no leading zero).
    s.push((b'0' + 1 + (lcg(state) % 9) as u8) as char);
    for _ in 1..len {
        s.push((b'0' + (lcg(state) % 10) as u8) as char);
    }
    s
}

fn edge_cases() -> Vec<String> {
    vec![
        "0",
        "1",
        "-1",
        "2",
        "-2",
        "7",
        "-7",
        "10",
        "-10",
        "4294967295",
        "4294967296",
        "4294967297",
        "-4294967296",
        "18446744073709551615",
        "18446744073709551616",
        "340282366920938463463374607431768211456", // 2^128
        "115792089237316195423570985008687907853269984665640564039457584007913129639936", // 2^256
        "-115792089237316195423570985008687907853269984665640564039457584007913129639936",
        "999999999999999999999999999999",
        "-999999999999999999999999999999",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

fn b(s: &str) -> BigInt {
    BigInt::from_decimal(s)
}

#[test]
fn decimal_roundtrip_and_le_words() {
    for s in edge_cases() {
        let v = b(&s);
        // Canonical decimal round-trips (HotSpot canonicalizes "+0" → "0").
        assert_eq!(v.to_decimal(), s, "decimal roundtrip {s}");
        // le-words → BigInt reproduces the same value.
        let rebuilt = BigInt::from_le_words(v.is_neg(), v.mag_le().to_vec());
        assert_eq!(rebuilt, v, "le-words roundtrip {s}");
    }
    // Spot-check the actual little-endian words for a known value:
    // 2^32 + 1 == [1, 1] little-endian.
    assert_eq!(b("4294967297").mag_le(), &[1u32, 1u32]);
    assert_eq!(b("0").mag_le(), &[] as &[u32]);
    assert_eq!(b("4294967296").mag_le(), &[0u32, 1u32]);
}

#[test]
fn add_sub_mul_cmp_match_decimal() {
    let mut state = 0x1234_5678_9abc_def0u64;
    let mut operands = edge_cases();
    for _ in 0..400 {
        operands.push(rand_decimal(&mut state));
    }
    // All edge × edge pairs + a sweep of random pairs.
    let n = operands.len();
    for i in 0..n {
        for j in 0..n {
            // Keep the full matrix for the (small) edge set; sample the
            // random tail to bound runtime.
            if i >= 20 && (i + j) % 7 != 0 {
                continue;
            }
            let a = &operands[i];
            let c = &operands[j];
            let ba = b(a);
            let bc = b(c);
            assert_eq!(ba.add(&bc).to_decimal(), bi_add_str(a, c), "add {a}+{c}");
            assert_eq!(ba.sub(&bc).to_decimal(), bi_sub_str(a, c), "sub {a}-{c}");
            assert_eq!(ba.mul(&bc).to_decimal(), bi_mul_str(a, c), "mul {a}*{c}");
            assert_eq!(ba.cmp(&bc), bi_compare(a, c).cmp(&0), "cmp {a} ? {c}");
        }
    }
}

#[test]
fn div_rem_mod_match_decimal() {
    let mut state = 0x0bad_c0de_1357_9bdfu64;
    let mut operands = edge_cases();
    for _ in 0..400 {
        operands.push(rand_decimal(&mut state));
    }
    let n = operands.len();
    for i in 0..n {
        for j in 0..n {
            if i >= 20 && (i + j) % 7 != 0 {
                continue;
            }
            let a = &operands[i];
            let c = &operands[j];
            if c == "0" {
                continue; // divide-by-zero raised upstream; not exercised here
            }
            let ba = b(a);
            let bc = b(c);

            // Truncated divide + sign-of-dividend remainder vs the decimal
            // reference.
            assert_eq!(ba.div(&bc).to_decimal(), bi_div_str(a, c), "div {a}/{c}");
            assert_eq!(ba.rem(&bc).to_decimal(), bi_mod_str(a, c), "rem {a}%{c}");

            // divmod agrees with the separate div/rem.
            let (q, r) = ba.divmod(&bc);
            assert_eq!(q, ba.div(&bc), "divmod q {a}/{c}");
            assert_eq!(r, ba.rem(&bc), "divmod r {a}%{c}");

            // Fundamental identity: a == q*c + r.
            assert_eq!(q.mul(&bc).add(&r), ba, "q*c+r==a for {a},{c}");
            // |r| < |c|.
            assert_eq!(
                BigInt::cmp_mag(r.mag_le(), bc.mag_le()),
                Ordering::Less,
                "|r| < |c| for {a},{c}"
            );

            // BigInteger.mod is non-negative and == (rem + |c|) % |c|.
            let m = ba.modulo(&bc);
            assert!(!m.is_neg(), "mod non-negative {a} mod {c}");
            let want_mod = {
                let c_abs = c.trim_start_matches('-');
                let rr = bi_mod_str(a, c_abs);
                if let Some(stripped) = rr.strip_prefix('-') {
                    if stripped == "0" {
                        "0".to_string()
                    } else {
                        bi_add_str(&rr, c_abs)
                    }
                } else {
                    rr
                }
            };
            assert_eq!(m.to_decimal(), want_mod, "mod {a} mod {c}");
        }
    }
}

#[test]
fn modpow_matches_decimal() {
    let mut state = 0xa5a5_5a5a_dead_0001u64;
    // non-negative exponents, positive moduli > 1
    let bases = [
        "0",
        "1",
        "2",
        "7",
        "255",
        "4294967297",
        "123456789012345678901234567890",
        "115792089237316195423570985008687907853269984665640564039457584007913129639747",
    ];
    let exps = ["0", "1", "2", "3", "17", "65537", "1000003"];
    let mods = [
        "2",
        "3",
        "97",
        "65537",
        "1000000007",
        "987654321098765432109876543211",
        "115792089237316195423570985008687907853269984665640564039457584007913129639747",
    ];
    for ba in bases {
        for e in exps {
            for m in mods {
                let got = b(ba).modpow(&b(e), &b(m)).to_decimal();
                let want = bi_mod_pow_str(ba, e, m);
                assert_eq!(got, want, "modpow({ba}^{e} mod {m})");
            }
        }
    }
    // a few random non-negative cases
    for _ in 0..120 {
        let ba = rand_decimal(&mut state).trim_start_matches('-').to_string();
        let e = rand_decimal(&mut state).trim_start_matches('-').to_string();
        let m = {
            let s = rand_decimal(&mut state).trim_start_matches('-').to_string();
            if s == "0" || s == "1" {
                "1000000007".to_string()
            } else {
                s
            }
        };
        assert_eq!(
            b(&ba).modpow(&b(&e), &b(&m)).to_decimal(),
            bi_mod_pow_str(&ba, &e, &m),
            "rand modpow({ba}^{e} mod {m})"
        );
    }
}

/// The Montgomery rewrite's safety net.
///
/// `modpow` dispatches on the parity of the modulus: odd goes to windowed
/// Montgomery, even to `modpow_classic`. This drives both arms across
/// operand sizes with a *third*, independent oracle — the decimal
/// `bi_mod_pow_str` — so neither arm is checked only against itself, and
/// then cross-checks the two arms against each other on odd moduli (where
/// both are defined). A Montgomery bug that produced plausible-looking
/// wrong residues would have to fool all three to survive.
#[test]
fn modpow_montgomery_and_classic_agree_with_decimal() {
    let mut state = 0x1bad_c0de_5eed_0007u64;

    // Structural cases first: the shapes that break window/limb bookkeeping.
    let structural: &[(&str, &str, &str)] = &[
        // modulus 1 -> everything is 0
        ("123456789", "987654321", "1"),
        // odd single-limb moduli at the word boundary
        ("4294967295", "4294967295", "4294967295"),
        ("4294967296", "4294967296", "4294967295"),
        ("1", "0", "3"),
        ("0", "0", "3"),
        ("0", "1", "3"),
        // exponent whose top word has 31 leading zero bits (the old
        // `mag.len() * 32` bound squared all of them for nothing)
        ("7", "4294967296", "1000000007"),
        ("7", "18446744073709551616", "1000000007"),
        // even moduli, including powers of two (the fallback arm)
        ("123456789", "65537", "2"),
        ("123456789", "65537", "4294967296"),
        (
            "123456789",
            "65537",
            "340282366920938463463374607431768211456",
        ),
        ("123456789", "65537", "1000000008"),
        (
            "99999999999999999999999999",
            "123456789",
            "618970019642690137449562112",
        ),
        // negative base: BigInteger.modPow is always non-negative
        ("-2", "3", "5"),
        ("-2", "2", "5"),
        ("-123456789012345678901234567890", "65537", "1000000007"),
        ("-123456789012345678901234567890", "65537", "1000000008"),
        // large odd modulus, RSA-ish exponent
        (
            "123456789012345678901234567890123456789012345678901234567890",
            "65537",
            "115792089237316195423570985008687907853269984665640564039457584007913129639747",
        ),
    ];
    for &(ba, e, m) in structural {
        let got = b(ba).modpow(&b(e), &b(m)).to_decimal();
        assert_eq!(got, bi_mod_pow_str(ba, e, m), "modpow({ba}^{e} mod {m})");
    }

    // Random sweep across operand widths. `bits` is the modulus width, so
    // this walks 1-limb moduli up through multi-limb ones; the exponent and
    // base are independently sized so the window code sees short and long
    // exponents against both narrow and wide moduli.
    for &bits in &[1u32, 2, 8, 31, 32, 33, 64, 65, 127, 128, 200, 256] {
        // The decimal oracle is O(digits^2) per squaring, so it dominates
        // at the wide end; taper the repeat count rather than the widths.
        let repeats = if bits >= 128 { 2 } else { 8 };
        for _ in 0..repeats {
            let m_odd = rand_bits_odd(&mut state, bits);
            // Same magnitude made even, so both arms see comparable sizes.
            let m_even = m_odd.add(&BigInt::small(1));
            for m in [&m_odd, &m_even] {
                if m.cmp(&BigInt::small(1)) != Ordering::Greater {
                    continue;
                }
                for &ebits in &[1u32, 5, 17, 24, 70, 197, bits.max(1)] {
                    let base = rand_bits_odd(&mut state, bits.max(1));
                    let base = if lcg(&mut state) & 1 == 0 {
                        base.neg_value()
                    } else {
                        base
                    };
                    let exp = rand_bits_odd(&mut state, ebits);
                    let (bs, es, ms) = (base.to_decimal(), exp.to_decimal(), m.to_decimal());
                    assert_eq!(
                        base.modpow(&exp, m).to_decimal(),
                        bi_mod_pow_str(&bs, &es, &ms),
                        "modpow({bs}^{es} mod {ms})"
                    );
                    // Odd moduli: the two arms must agree bit for bit.
                    if m.mag[0] & 1 == 1 {
                        assert_eq!(
                            base.modpow(&exp, m),
                            base.modpow_classic(&exp, m),
                            "montgomery vs classic ({bs}^{es} mod {ms})"
                        );
                    }
                }
            }
        }
    }
}

/// RSA is the failure mode the doc names: a subtly wrong modPow produces
/// plausible-looking wrong signatures that still round-trip through the
/// encoding layers. Check the algebra directly — `(m^e)^d == m (mod n)` for
/// a real keypair, plus the CRT half-exponentiations — so a wrong residue
/// cannot hide behind a self-consistent implementation.
#[test]
fn modpow_round_trips_an_rsa_keypair() {
    // p, q distinct primes; n = p*q, e = 65537, d = e^-1 mod phi.
    let p = b("177250851143413106261106096231831452933");
    let q = b("329539864610483636976818094878219646841");
    assert!(p.is_probable_prime(), "p prime");
    assert!(q.is_probable_prime(), "q prime");
    let n = p.mul(&q);
    let e = b("65537");
    let phi = p.sub(&BigInt::small(1)).mul(&q.sub(&BigInt::small(1)));
    let d = e.mod_inverse(&phi).expect("e invertible mod phi");

    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    for _ in 0..12 {
        let msg = rand_bits_odd(&mut state, 200).modulo(&n);
        let c = msg.modpow(&e, &n);
        let back = c.modpow(&d, &n);
        assert_eq!(back, msg, "RSA round trip");
        assert_ne!(c, msg, "ciphertext is not the plaintext");
        // The signing direction, and CRT halves against the direct result.
        let sig = msg.modpow(&d, &n);
        assert_eq!(sig.modpow(&e, &n), msg, "RSA sign/verify round trip");
        let dp = d.modulo(&p.sub(&BigInt::small(1)));
        let dq = d.modulo(&q.sub(&BigInt::small(1)));
        assert_eq!(msg.modpow(&dp, &p), sig.modulo(&p), "CRT half mod p");
        assert_eq!(msg.modpow(&dq, &q), sig.modulo(&q), "CRT half mod q");
    }
}

#[test]
fn mod_inverse_matches_decimal() {
    let cases = [
        ("1", "2"),
        ("2", "3"),
        ("3", "11"),
        ("42", "2017"),
        ("-42", "2017"),
        (
            "123456789012345678901234567890",
            "115792089237316195423570985008687907853269984665640564039457584007913129639747",
        ),
        (
            "98765432109876543210987654321",
            "6277101735386680763835789423207666416083908700390324961279",
        ),
    ];
    for (a, m) in cases {
        let got = b(a).mod_inverse(&b(m)).map(|v| v.to_decimal());
        let want = bi_mod_inverse_str(a, m);
        assert_eq!(got, want, "modInverse({a}, {m})");
    }
    assert_eq!(b("2").mod_inverse(&b("4")), None);
    assert_eq!(b("9").mod_inverse(&b("1")).unwrap().to_decimal(), "0");

    let mut state = 0x5151_6262_7373_8484u64;
    let prime =
        "115792089237316195423570985008687907853269984665640564039457584007913129639747";
    for _ in 0..80 {
        let a = rand_decimal(&mut state);
        let got = b(&a).mod_inverse(&b(prime)).map(|v| v.to_decimal());
        let want = bi_mod_inverse_str(&a, prime);
        assert_eq!(got, want, "rand modInverse({a}, prime)");
    }
}

#[test]
fn is_probable_prime_matches_decimal() {
    let primes = [
        "2",
        "3",
        "5",
        "7",
        "97",
        "65537",
        "32416190071", // 10-digit prime
        "115792089237316195423570985008687907853269984665640564039457584007913129639747",
    ];
    let composites = ["0", "1", "4", "9", "15", "100", "32416190073"];
    for p in primes {
        assert!(b(p).is_probable_prime(), "{p} should be prime");
        assert_eq!(
            b(p).is_probable_prime(),
            bi_is_probable_prime_str(p),
            "prime {p} vs decimal ref"
        );
    }
    for c in composites {
        assert!(!b(c).is_probable_prime(), "{c} should be composite");
        assert_eq!(
            b(c).is_probable_prime(),
            bi_is_probable_prime_str(c),
            "composite {c} vs decimal ref"
        );
    }
    // Hard case the trial-division stub got wrong: a product of two large
    // primes (no small factor) must be detected composite by MR.
    let p256 = "115792089237316195423570985008687907853269984665640564039457584007913129639747";
    let q256 = "115792089237316195423570985008687907853269984665640564039457584007913129640297";
    let semiprime = b(p256).mul(&b(q256));
    assert!(!semiprime.is_probable_prime(), "p*q must be composite");
    assert_eq!(
        semiprime.is_probable_prime(),
        bi_is_probable_prime_str(&semiprime.to_decimal()),
        "semiprime vs decimal ref"
    );
}

#[test]
fn bit_ops_correct() {
    let one = b("1");
    let neg_one = b("-1");
    let zero = b("0");

    // Known exact values (BigInteger semantics).
    assert_eq!(b("5").not().to_decimal(), "-6"); // ~5 = -6
    assert_eq!(b("-5").not().to_decimal(), "4"); // ~(-5) = 4
    assert_eq!(b("0").not().to_decimal(), "-1"); // ~0 = -1
    assert_eq!(b("12").and(&b("10")).to_decimal(), "8");
    assert_eq!(b("12").or(&b("10")).to_decimal(), "14");
    assert_eq!(b("12").xor(&b("10")).to_decimal(), "6");
    // Negative two's-complement (matches java.math.BigInteger):
    // -8 = …11111000, 12 = …00001100 → &=…1000=8, |=…11111100=-4, ^=…11110100=-12.
    assert_eq!(b("-8").and(&b("12")).to_decimal(), "8");
    assert_eq!(b("-8").or(&b("12")).to_decimal(), "-4");
    assert_eq!(b("-8").xor(&b("12")).to_decimal(), "-12");
    // bitLength / bitCount / lowestSetBit edge cases.
    assert_eq!(neg_one.bit_length(), 0);
    assert_eq!(b("-2").bit_length(), 1);
    assert_eq!(b("-4").bit_length(), 2);
    assert_eq!(b("-3").bit_length(), 2);
    assert_eq!(b("17").bit_length(), 5);
    assert_eq!(b("255").bit_count(), 8);
    assert_eq!(neg_one.bit_count(), 0);
    assert_eq!(b("-256").bit_count(), 8);
    assert_eq!(b("0").lowest_set_bit(), -1);
    assert_eq!(b("48").lowest_set_bit(), 4); // 48 = 0b110000
    assert_eq!(b("-48").lowest_set_bit(), 4);

    // Algebraic identities over a deterministic spread of both signs.
    let mut state = 0x1313_2424_3535_4646u64;
    let mut vals: Vec<BigInt> = edge_cases().iter().map(|s| b(s)).collect();
    for _ in 0..150 {
        vals.push(b(&rand_decimal(&mut state)));
    }
    for x in &vals {
        // ~x == -(x+1); x ^ -1 == ~x; x ^ 0 == x; x & 0 == 0; x | 0 == x.
        assert_eq!(
            x.not(),
            x.add(&one).neg_value(),
            "~x for {}",
            x.to_decimal()
        );
        assert_eq!(x.xor(&neg_one), x.not(), "x^-1 for {}", x.to_decimal());
        assert_eq!(x.xor(&zero), *x);
        assert_eq!(x.and(&zero), zero);
        assert_eq!(x.or(&zero), *x);
        assert_eq!(x.and(x), *x);
        assert_eq!(x.or(x), *x);
        assert_eq!(x.xor(x), zero);
        // testBit consistency with the value: x.testBit(i) reconstructs x
        // for a few low bits via OR of set bits is overkill; check against
        // shifting instead: bit i of x == ((x >> i) is odd).
        for i in [0u32, 1, 5, 31, 32, 33, 64] {
            let shifted_odd = x.shr(i).and(&one) == one;
            assert_eq!(
                x.test_bit(i),
                shifted_odd,
                "testBit {} bit {i}",
                x.to_decimal()
            );
        }
    }
    // Cross-identity: (a&b) | (a^b) == a|b; De Morgan ~(a&b)==(~a)|(~b).
    for x in &vals {
        for y in vals.iter().take(20) {
            assert_eq!(x.and(y).or(&x.xor(y)), x.or(y), "consistency");
            assert_eq!(x.and(y).not(), x.not().or(&y.not()), "De Morgan");
        }
    }
}

fn positive_bit_ops_match_decimal_cases(random_cases: usize, peer_cases: usize) {
    // For non-negative operands the decimal reference (which works on
    // magnitudes) is authoritative.
    let mut state = 0x9999_7777_5555_3333u64;
    let mut pos: Vec<String> = vec!["0", "1", "255", "65535", "4294967296"]
        .into_iter()
        .map(String::from)
        .collect();
    for _ in 0..random_cases {
        pos.push(rand_decimal(&mut state).trim_start_matches('-').to_string());
    }
    for a in &pos {
        // bitLength / bitCount vs decimal ref.
        assert_eq!(
            b(a).bit_length(),
            crate::bi_bit_length_str(a) as u32,
            "bitLen {a}"
        );
        assert_eq!(
            b(a).bit_count(),
            crate::bi_bit_count_str(a) as u32,
            "bitCnt {a}"
        );
        for c in pos.iter().take(peer_cases) {
            assert_eq!(
                b(a).and(&b(c)).to_decimal(),
                bi_bitwise_and(a, c),
                "and {a}&{c}"
            );
            assert_eq!(
                b(a).or(&b(c)).to_decimal(),
                bi_bitwise_or(a, c),
                "or {a}|{c}"
            );
            assert_eq!(
                b(a).xor(&b(c)).to_decimal(),
                bi_bitwise_xor(a, c),
                "xor {a}^{c}"
            );
        }
    }
}

#[test]
fn positive_bit_ops_match_decimal() {
    positive_bit_ops_match_decimal_cases(24, 10);
}

#[test]
#[ignore = "exhaustive decimal cross-check takes several minutes; run explicitly before bigint rewrites"]
fn positive_bit_ops_match_decimal_exhaustive() {
    positive_bit_ops_match_decimal_cases(150, 25);
}

/// `test_bit` must answer a huge bit address from the sign alone, without
/// materializing the two's complement out to that word. The old body built
/// `vec![0u32; (n/32)+2]`, so every row here allocated ~256 MB; this test
/// would have taken minutes and ~4 GB.
///
/// Expected values MEASURED on Microsoft OpenJDK 25.0.3+9
/// (`scratchpad/f7/TestBit.java`), each `[0 ms]`:
///
/// ```text
/// ONE.testBit(Integer.MAX_VALUE)  = false      (-1).testBit(Integer.MAX_VALUE) = true
/// ZERO.testBit(Integer.MAX_VALUE) = false      (-1).testBit(0)                 = true
/// (-2).testBit(0) = false                      (-2).testBit(1)                 = true
/// (2^64).testBit(0)     = false                (-(2^64)).testBit(0)  = false
/// (-(2^64)).testBit(64) = true                 (-(2^64)).testBit(65) = true
/// (-(2^64+1)).testBit(0) = true                (-(2^64+1)).testBit(1) = true
/// ```
///
/// The full transliteration of this body agrees with `java.math.BigInteger`
/// on 371,547 (operand, bit) pairs with zero diffs.
#[test]
fn test_bit_is_allocation_free_at_huge_addresses() {
    const MAX: u32 = i32::MAX as u32;
    assert!(!b("1").test_bit(MAX));
    assert!(b("-1").test_bit(MAX));
    assert!(!b("0").test_bit(MAX));
    assert!(b("-1").test_bit(0));
    assert!(!b("-2").test_bit(0));
    assert!(b("-2").test_bit(1));
    // 2^64 == mag [0, 0, 1]: the low limbs are zero, which is what
    // separates `-magInt` from `~magInt` in the JDK's `getInt`.
    let p64 = b("18446744073709551616");
    let n64 = b("-18446744073709551616");
    assert!(!p64.test_bit(0));
    assert!(!n64.test_bit(0));
    assert!(n64.test_bit(64));
    assert!(n64.test_bit(65));
    assert!(n64.test_bit(MAX));
    let n64p1 = b("-18446744073709551617");
    assert!(n64p1.test_bit(0));
    assert!(n64p1.test_bit(1));
    // A positive value is 0 above its magnitude, a negative one is 1 —
    // for every address past the top limb, not just the huge ones.
    for &n in &[96u32, 97, 1000, 1 << 20, 1 << 26, MAX - 1, MAX] {
        assert!(!p64.test_bit(n), "positive bit {n}");
        assert!(n64.test_bit(n), "negative bit {n}");
    }
}

#[test]
fn shifts_match_decimal() {
    let mut state = 0xfeed_face_dead_beefu64;
    let mut operands = edge_cases();
    for _ in 0..200 {
        operands.push(rand_decimal(&mut state));
    }
    let shifts: [u32; 9] = [0, 1, 2, 7, 31, 32, 33, 64, 130];
    for a in &operands {
        let ba = b(a);
        for &s in &shifts {
            assert_eq!(
                ba.shl(s).to_decimal(),
                bi_shift_left_str(a, s as i32),
                "shl {a} << {s}"
            );
            assert_eq!(
                ba.shr(s).to_decimal(),
                bi_shift_right_str(a, s as i32),
                "shr {a} >> {s}"
            );
        }
    }
}

/// **No-panic pin (lane G10, 2026-08-16).** `divmod_mag`'s zero-divisor
/// guard was a `debug_assert!`, which is compiled out of `--release`; the
/// next line indexes `v[n - 1]` and `0usize - 1` is a slice-index panic.
/// A panic in a native is a VM abort, not a Java exception.
///
/// The four public wrappers short-circuit first, so the assertions here are
/// on their documented convention (zero out) AND on the fact that every one
/// of them RETURNS. `divmod_mag` itself is private, so it is reached
/// through them; a zero-length magnitude is what `BigInt::zero()` carries.
#[test]
fn division_by_zero_returns_instead_of_panicking() {
    let zero = BigInt::zero();
    for v in ["0", "1", "-1", "255", "-9007199254740993", "10"] {
        let x = b(v);
        assert_eq!(x.div(&zero).to_decimal(), "0", "{v} / 0");
        assert_eq!(x.rem(&zero).to_decimal(), "0", "{v} rem 0");
        assert_eq!(x.modulo(&zero).to_decimal(), "0", "{v} mod 0");
        let (q, r) = x.divmod(&zero);
        assert_eq!((q.to_decimal(), r.to_decimal()), ("0".into(), "0".into()));
        // modpow with a zero modulus is the same shape one level up.
        assert_eq!(x.modpow(&b("3"), &zero).to_decimal(), "0");
    }
    // `from_le_words` normalizes, so an all-zero-limb divisor arrives at
    // `divmod_mag` already trimmed to `n == 0` — the exact input the
    // `debug_assert!` was the only thing standing in front of.
    let padded_zero = BigInt::from_le_words(false, vec![0, 0, 0]);
    assert!(padded_zero.is_zero());
    assert_eq!(b("12345").div(&padded_zero).to_decimal(), "0");
    assert_eq!(b("12345").rem(&padded_zero).to_decimal(), "0");
}

fn limbs(state: &mut u64, n: usize, pattern: u32) -> Vec<u32> {
    (0..n)
        .map(|i| match pattern {
            0 => lcg(state) as u32,
            // All-ones: every partial sum carries.
            1 => u32::MAX,
            // Zero limbs scattered through the operand, including the
            // low half's top limb (the split point) on some shapes.
            _ => {
                if i % 5 == 0 {
                    0
                } else {
                    lcg(state) as u32
                }
            }
        })
        .collect()
}

/// Karatsuba is the schoolbook product limb for limb: balanced, odd,
/// unbalanced (slab path) and just-over-threshold shapes, three limb
/// patterns each.
#[test]
fn karatsuba_matches_schoolbook() {
    let mut state = 0x5eed_0000_1234_abcdu64;
    let shapes: [(usize, usize); 10] = [
        (80, 80),
        (81, 80),
        (159, 80),
        (160, 80),
        (161, 90),
        (200, 199),
        (333, 1000),
        (97, 300),
        (1024, 1024),
        (80, 700),
    ];
    for &(la, lb) in &shapes {
        for pattern in 0..3 {
            let a = limbs(&mut state, la, pattern);
            let c = limbs(&mut state, lb, pattern);
            assert_eq!(
                BigInt::mul_mag_karatsuba(&a, &c),
                BigInt::mul_mag(&a, &c),
                "karatsuba {la}x{lb} pattern {pattern}"
            );            }        }    }