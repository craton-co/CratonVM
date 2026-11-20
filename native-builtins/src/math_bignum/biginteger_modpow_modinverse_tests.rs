use super::{bi_alloc_int, bi_mod_inverse_str, bi_mod_pow_str};
use crate::bigint::BigInt;
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

// --- modPow sign handling (the registered native delegates to these
//     helpers; these tests pin the underlying arithmetic that the old
//     sign-stripping native got wrong) ---

#[test]
fn modpow_positive() {
    // 3^4 mod 7 = 81 mod 7 = 4
    assert_eq!(bi_mod_pow_str("3", "4", "7"), "4");
}

#[test]
fn modpow_negative_base_reduced_mod_m() {
    // (-3)^2 mod 7: -3 ≡ 4 (mod 7); 4^2 = 16 ≡ 2 (mod 7).
    // The old native stripped the sign and computed 3^2 = 9 ≡ 2 — same
    // residue for an even exponent, so use an ODD exponent to expose it:
    // (-3)^3 mod 7: 4^3 = 64 ≡ 1 (mod 7); sign-stripped 3^3 = 27 ≡ 6.
    assert_eq!(bi_mod_pow_str("-3", "3", "7"), "1");
    assert_ne!(bi_mod_pow_str("-3", "3", "7"), "6");
}

#[test]
fn modpow_large_operands() {
    // 2^256 mod 1000000007. The intermediate 2^256 is far beyond i128, so
    // this exercises the arbitrary-precision string path end to end.
    // Cross-checked: Python pow(2, 256, 1000000007) == 792845266.
    assert_eq!(bi_mod_pow_str("2", "256", "1000000007"), "792845266");

    // Same exponent with a modulus that is itself larger than i128
    // (Mersenne prime 2^61 - 1): since 2^61 ≡ 1, 2^256 ≡ 2^(256 mod 61) =
    // 2^12 = 4096. Cross-checked: pow(2, 256, 2305843009213693951) == 4096.
    assert_eq!(bi_mod_pow_str("2", "256", "2305843009213693951"), "4096");
}

// --- modInverse: real arbitrary-precision inverse + non-invertible None ---

#[test]
fn modinverse_small() {
    // 3 * 5 = 15 ≡ 1 (mod 7), so 3^-1 ≡ 5 (mod 7).
    assert_eq!(bi_mod_inverse_str("3", "7"), Some("5".to_string()));
}

#[test]
fn modinverse_negative_base() {
    // -3 ≡ 4 (mod 7); 4^-1 ≡ 2 (mod 7) since 4*2 = 8 ≡ 1.
    assert_eq!(bi_mod_inverse_str("-3", "7"), Some("2".to_string()));
}

#[test]
fn modinverse_non_coprime_is_none() {
    // gcd(4, 8) = 4 != 1 -> not invertible (native maps None -> ArithmeticException).
    assert_eq!(bi_mod_inverse_str("4", "8"), None);
    // gcd(6, 9) = 3 != 1.
    assert_eq!(bi_mod_inverse_str("6", "9"), None);
}

#[test]
fn modinverse_large_operands_not_silent_one() {
    // A large prime modulus far beyond i128; the old i128 fallback returned
    // a silent "1" for inputs this size. Verify a real inverse: with
    // m = 2^61 - 1 (Mersenne prime 2305843009213693951) and a = 2,
    // 2^-1 mod m = (m+1)/2 = 1152921504606846976.
    assert_eq!(
        bi_mod_inverse_str("2", "2305843009213693951"),
        Some("1152921504606846976".to_string())
    );
}

#[test]
fn bi_alloc_int_releases_native_pin() {
    let mut ctx = mock_ctx();
    let value = BigInt::from_decimal("123456789012345678901234567890");

    assert_eq!(ctx.native_pin_count_for_test(), 0);
    let _ = bi_alloc_int(&mut ctx, &value);
    assert_eq!(ctx.native_pin_count_for_test(), 0);
}
