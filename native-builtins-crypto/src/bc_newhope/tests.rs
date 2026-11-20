use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn reduce_spot_checks() {
    // montgomery(0) and barrett(0) are 0; barrett reduces values >= Q.
    assert_eq!(montgomery(0), 0);
    assert_eq!(barrett(0), 0);
    // barrett(a) for a in [0, Q) is the identity (no reduction needed):
    for a in [1i16, 100, 12288] {
        assert_eq!(
            ((barrett(a) as i32).rem_euclid(Q)),
            (a as i32).rem_euclid(Q)
        );
    }
    // barrett of a value >= Q reduces it into range mod Q.
    let big = 24577i32; // 2*Q - 1
    assert_eq!(barrett(big as i16) as i32 & 0xFFFF, (big - Q));
}

/// FNV-1a over the unsigned-16-bit coefficient view, matching the Java
/// probe's checksum (Java `long` wraps == `u64` wrapping).
fn fnv(a: &[i16; N]) -> i64 {
    let mut s: u64 = 0xcbf2_9ce4_8422_2325;
    for &v in a.iter() {
        s ^= (v as u16) as u64;
        s = s.wrapping_mul(0x0000_0100_0000_01b3);
    }
    s as i64
}


#[test]
fn ntt_matches_hotspot() {
    let mut x = [0i16; N];
    for i in 0..N {
        x[i] = (((i * 7 + 13) % 12289) as i32) as i16;
    }
    to_ntt(&mut x);
    let head: Vec<i32> = (0..8).map(|i| (x[i] as u16) as i32).collect();
    assert_eq!(head, [8477, 7986, 2020, 14350, 7831, 6381, 14052, 13714]);
    // Full-array checksum — verified equal element-for-element to HotSpot's
    // real Poly.toNTT across all 1024 coefficients during the port.
    assert_eq!(fnv(&x), -5417792707627256840);

    from_ntt(&mut x);
    let head2: Vec<i32> = (0..8).map(|i| (x[i] as u16) as i32).collect();
    assert_eq!(head2, [13, 3597, 1805, 5389, 909, 4493, 2701, 6285]);
    assert_eq!(fnv(&x), -7401367446177724180);
}

/// SHAKE128("") known-answer (NIST) — confirms the `sha3` crate's SHAKE128
/// matches BC's `SHAKEDigest(128)` byte stream, which the rejection sampler
/// consumes.
#[test]
fn shake128_empty_kat() {
    use sha3::digest::{ExtendableOutput, XofReader};
    let mut out = [0u8; 32];
    let mut r = sha3::Shake128::default().finalize_xof();
    r.read(&mut out);
    let expect = [
        0x7f, 0x9c, 0x2b, 0xa4, 0xe8, 0x8f, 0x82, 0x7d, 0x61, 0x60, 0x45, 0x50, 0x76, 0x05,
        0x85, 0x3e, 0xd7, 0x3b, 0x80, 0x93, 0xf6, 0xef, 0xbc, 0x88, 0xeb, 0x1a, 0x6e, 0xac,
        0xfa, 0x66, 0xef, 0x26,
    ];
    assert_eq!(out, expect);
}

/// `uniform` validated against HotSpot ground truth (BC's real
/// `Poly.uniform` with seed = 0..31) — confirms the SHAKE128 stream +
/// rejection sampler are byte-identical. Also checks the `[0, 5*Q)` range.
#[test]
fn uniform_matches_hotspot() {
    let mut a = [0i16; N];
    let seed: Vec<u8> = (0..32u8).collect();
    uniform(&mut a, &seed);
    let head: Vec<i32> = (0..8).map(|i| (a[i] as u16) as i32).collect();
    assert_eq!(head, [27142, 7478, 30150, 22264, 52686, 11200, 8485, 4234]);
    assert_eq!(fnv(&a), -4161122429893521255);
    for &c in a.iter() {
        assert!((c as u16 as i32) < 5 * Q, "coefficient out of range");
    }
}
