use super::{bc_has_any_small_factors, BC_SMALL_FACTOR_GROUPS};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn mag_le(mut v: u128) -> Vec<u32> {
    let mut w = Vec::new();
    while v != 0 {
        w.push((v & 0xFFFF_FFFF) as u32);
        v >>= 32;
    }
    w
}

#[test]
fn group_products_fit_signed_int() {
    // Each group's modulus must equal the Java `int m` (positive, no overflow).
    for g in BC_SMALL_FACTOR_GROUPS {
        let m: u64 = g.iter().map(|&p| p as u64).product();
        assert!(
            m < (1u64 << 31),
            "group product {m} must fit a positive i32"
        );
    }
}

#[test]
fn detects_small_factors() {
    for &n in &[0u128, 4, 9, 15, 21, 211, 211 * 211, 2 * 1_000_003] {
        assert!(
            bc_has_any_small_factors(&mag_le(n), false),
            "n={n} should report a small factor"
        );
    }
}

#[test]
fn passes_values_with_no_small_factor() {
    assert!(!bc_has_any_small_factors(&mag_le(223), false)); // smallest prime > 211
    assert!(!bc_has_any_small_factors(&mag_le(1_000_003), false)); // prime
    assert!(!bc_has_any_small_factors(&mag_le((1u128 << 61) - 1), false)); // M61, prime
}

#[test]
fn empty_mag_is_zero_divisible() {
    assert!(bc_has_any_small_factors(&[], false)); // x == 0
}

#[test]
fn negative_fold_matches_nonneg_mod() {
    // BigInteger.mod is non-negative; divisibility by p is sign-invariant.
    assert!(bc_has_any_small_factors(&mag_le(15), true)); // -15 → factors 3,5
    assert!(bc_has_any_small_factors(&mag_le(211), true)); // -211 → factor 211
    assert!(!bc_has_any_small_factors(&mag_le(223), true)); // -223 → prime
}

// ---- util.BigIntegers.hasAnySmallFactors (odd primes 3..=743) ----
use super::{bc_util_has_any_small_factors, BC_ODD_SMALL_PRIMES};

fn sieve_odd_primes(limit: u32) -> Vec<u32> {
    let n = limit as usize;
    let mut s = vec![true; n + 1];
    (2..=n).for_each(|i| {
        if s[i] {
            let mut j = i * i;
            while j <= n {
                s[j] = false;
                j += i;
            }
        }
    });
    (3..=n)
        .filter(|&i| s[i] && i % 2 == 1)
        .map(|i| i as u32)
        .collect()
}

#[test]
fn odd_small_primes_set_is_exactly_3_to_743() {
    // The array must be the prime factors of SMALL_PRIMES_PRODUCT (odd
    // primes 3..=743; verified out-of-band that their product == the BC
    // hex literal). Lock it against an independent sieve.
    assert_eq!(BC_ODD_SMALL_PRIMES.to_vec(), sieve_odd_primes(743));
}

#[test]
fn util_detects_small_factors() {
    assert!(bc_util_has_any_small_factors(&mag_le(0))); // even (zero)
    assert!(bc_util_has_any_small_factors(&mag_le(2))); // even
    assert!(bc_util_has_any_small_factors(&mag_le(100))); // even
    assert!(bc_util_has_any_small_factors(&mag_le(743))); // factor 743
    assert!(bc_util_has_any_small_factors(&mag_le(743 * 1009))); // factor 743
    assert!(bc_util_has_any_small_factors(&mag_le(3 * 999_983))); // factor 3
}

#[test]
fn util_passes_values_with_no_small_factor() {
    assert!(!bc_util_has_any_small_factors(&mag_le(751))); // smallest prime > 743
    assert!(!bc_util_has_any_small_factors(&mag_le(999_983))); // prime
    assert!(!bc_util_has_any_small_factors(&mag_le((1u128 << 61) - 1))); // M61
}
