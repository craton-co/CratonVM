use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// The uncompressed affine coordinates of the P-256 base point, taken from
/// the `p256` crate itself so the test cannot drift from the curve it
/// exercises.
fn p256_generator() -> (Vec<u8>, Vec<u8>) {
    let g = p256::AffinePoint::generator().to_sec1_point(false);
    (
        g.x().expect("generator has an x").to_vec(),
        g.y().expect("generator has a y").to_vec(),
    )
}

fn le(be: &[u8]) -> Vec<u8> {
    be.iter().rev().copied().collect()
}

/// The uncompressed affine coordinates of a curve's base point, taken from
/// the curve crate itself.
macro_rules! generator_of {
    ($krate:ident) => {{
        use $krate::elliptic_curve::group::prime::PrimeCurveAffine as _;
        use $krate::elliptic_curve::sec1::ToSec1Point as _;
        let g = $krate::AffinePoint::generator().to_sec1_point(false);
        (
            g.x().expect("generator has an x").to_vec(),
            g.y().expect("generator has a y").to_vec(),
        )
    }};
}

/// Prove a hard-coded `*_ORDER_BE` constant is **exactly** the curve's
/// group order, using nothing but the backend's own canonicity test.
///
/// `Scalar::from_repr(v)` is `Some` iff `v < n`. So:
///   * `from_repr(ORDER)` is `None`      ⟹ `ORDER >= n`
///   * `from_repr(ORDER - 1)` is `Some`  ⟹ `ORDER - 1 < n`, i.e. `ORDER <= n`
/// Together the two bound it from both sides: `ORDER == n`. A single
/// mistyped digit in the constant fails one direction or the other, so the
/// constants cannot silently drift from the curves they gate.
macro_rules! assert_order_exact {
    ($krate:ident, $order:ident, $name:literal) => {{
        use $krate::elliptic_curve::ff::PrimeField as _;
        use $krate::Scalar;

        let at_order = Scalar::from_repr($order.into());
        assert!(
            bool::from(at_order.is_none()),
            "{}: the constant is accepted as canonical, so it is below the group order",
            $name
        );

        let mut below = $order;
        let last = below.len() - 1;
        below[last] -= 1; // every order ends in a non-zero byte, so no borrow
        let just_below = Scalar::from_repr(below.into());
        assert!(
            bool::from(just_below.is_some()),
            "{}: the constant minus one is rejected, so the constant is above the group order",
            $name
        );
    }};
}

#[test]
fn group_order_constants_are_exact() {
    assert_order_exact!(p256, P256_ORDER_BE, "P-256");
    assert_order_exact!(p384, P384_ORDER_BE, "P-384");
    assert_order_exact!(p521, P521_ORDER_BE, "P-521");
}

// ---- supported curves are recognised; everything else is rejected ----

#[test]
fn only_p256_p384_p521_field_classes_are_accepted() {
    for (cls, len) in [
        ("sun/security/util/math/intpoly/IntegerPolynomialP256", 32),
        (
            "sun/security/util/math/intpoly/MontgomeryIntegerPolynomialP256",
            32,
        ),
        ("sun/security/util/math/intpoly/IntegerPolynomialP384", 48),
        ("sun/security/util/math/intpoly/IntegerPolynomialP521", 66),
    ] {
        let c = Curve::from_field_class(cls).expect("supported curve");
        assert_eq!(c.byte_len(), len, "{cls}: wrong field width");
    }
}

/// An unrecognised curve must produce `None` so the caller throws. In
/// particular the empty string — what the old `unwrap_or_default()`
/// produced for an unresolvable class id — must not match anything.
#[test]
fn unsupported_curve_field_classes_are_declined() {
    for cls in [
        "",
        "sun/security/util/math/intpoly/IntegerPolynomial1305",
        "sun/security/util/math/intpoly/IntegerPolynomial25519",
        "sun/security/util/math/intpoly/IntegerPolynomialP192",
        "org/example/Secp256k1Field",
        "java/lang/Object",
    ] {
        assert!(
            Curve::from_field_class(cls).is_none(),
            "{cls} must not be accepted as a supported curve"
        );
    }
}

// ---- a valid multiply still succeeds ----

#[test]
fn generator_times_one_returns_the_generator() {
    let (gx, gy) = p256_generator();
    let (rx, ry) = scalar_mul_p256(&gx, &gy, &[1u8]).expect("1·G must succeed");
    assert_eq!(rx, gx);
    assert_eq!(ry, gy);
}

#[test]
fn generator_times_two_matches_the_curve_crate() {
    let (gx, gy) = p256_generator();
    let (rx, ry) = scalar_mul_p256(&gx, &gy, &[2u8]).expect("2·G must succeed");

    let expect = (p256::ProjectivePoint::from(p256::AffinePoint::generator())
        + p256::ProjectivePoint::from(p256::AffinePoint::generator()))
    .to_affine()
    .to_sec1_point(false);
    assert_eq!(rx, expect.x().unwrap().to_vec());
    assert_eq!(ry, expect.y().unwrap().to_vec());
}

// ---- malformed inputs are refused, not approximated ----

/// The fix under test: an over-long scalar used to be silently truncated to
/// its low `nbytes` bytes, so `s` and `s mod 2^256` returned the same point
/// with no indication that a different number had been multiplied.
#[test]
fn oversized_scalar_is_refused_not_truncated() {
    let (gx, gy) = p256_generator();
    // 33 little-endian bytes: value 1 + a significant 2^256 term.
    let mut s = vec![0u8; 33];
    s[0] = 1;
    s[32] = 1;
    assert!(
        scalar_mul_p256(&gx, &gy, &s).is_none(),
        "a scalar with significant bytes past the field width must be refused"
    );

    // Sanity: had it truncated, it would have produced exactly 1·G.
    let (one_x, _) = scalar_mul_p256(&gx, &gy, &[1u8]).unwrap();
    assert_eq!(one_x, gx, "truncation would have silently returned 1·G");
}

/// ...but a merely zero-padded scalar (BigInteger sign padding) is still
/// accepted. Refusing it would be a false positive on a legitimate call.
#[test]
fn zero_padded_oversized_scalar_is_still_accepted() {
    let (gx, gy) = p256_generator();
    let mut s = vec![0u8; 40];
    s[0] = 1; // little-endian 1, padded with high zero bytes
    let (rx, ry) = scalar_mul_p256(&gx, &gy, &s).expect("zero padding is harmless");
    assert_eq!(rx, gx);
    assert_eq!(ry, gy);
}

#[test]
fn wrong_length_coordinates_are_refused() {
    let (gx, gy) = p256_generator();
    assert!(scalar_mul_p256(&gx[..31], &gy, &[1u8]).is_none(), "short x");
    assert!(scalar_mul_p256(&gx, &gy[..31], &[1u8]).is_none(), "short y");
    assert!(scalar_mul_p256(&[], &[], &[1u8]).is_none(), "empty coords");
    // P-384/P-521 must reject P-256-sized coordinates rather than pad them.
    assert!(scalar_mul_p384(&gx, &gy, &[1u8]).is_none());
    assert!(scalar_mul_p521(&gx, &gy, &[1u8]).is_none());
}

/// A point that is not on the curve must be refused. Accepting it would
/// hand back a "result" on some other curve entirely — the classic
/// invalid-curve attack.
#[test]
fn off_curve_point_is_refused() {
    let (gx, mut gy) = p256_generator();
    gy[31] ^= 0x01;
    assert!(scalar_mul_p256(&gx, &gy, &[1u8]).is_none());
}

// ---- the documented point-at-infinity signal ----

/// `0·G` and `n·G` are both the neutral element, reported as empty
/// coordinate vectors (not as an error and not as a real point). This pins
/// the contract `ECDHKeyAgreement.validate` depends on; see the
/// `TRUST BOUNDARY` note on the non-canonical branch.
#[test]
fn identity_results_are_signalled_with_empty_coordinates() {
    let (gx, gy) = p256_generator();
    let zero = scalar_mul_p256(&gx, &gy, &[0u8; 32]).expect("0·G is defined");
    assert_eq!(zero, (Vec::new(), Vec::new()), "0·G must be the identity");

    let order = scalar_mul_p256(&gx, &gy, &le(&P256_ORDER_BE)).expect("n·G is defined");
    assert_eq!(order, (Vec::new(), Vec::new()), "n·G must be the identity");
}

/// ...and the same must hold on the two curves whose scalar arithmetic the
/// tests above do not exercise. Both cases short-circuit in the
/// non-canonical branch, so this pins the constant-driven decision itself,
/// not the curve crates' point arithmetic.
#[test]
fn the_group_order_is_the_identity_on_every_supported_curve() {
    let (gx, gy) = generator_of!(p384);
    assert_eq!(
        scalar_mul_p384(&gx, &gy, &le(&P384_ORDER_BE)).expect("n·G is defined"),
        (Vec::new(), Vec::new())
    );
    let (gx, gy) = generator_of!(p521);
    assert_eq!(
        scalar_mul_p521(&gx, &gy, &le(&P521_ORDER_BE)).expect("n·G is defined"),
        (Vec::new(), Vec::new())
    );
}

/// **The narrowing under test.** A non-canonical scalar that is *not* the
/// group order used to be answered with the identity — a plausible,
/// well-formed point — so a caller could not tell "your scalar was
/// rejected" from "the product really is the point at infinity". On a
/// private-key operand that is the failure-as-output shape this audit
/// exists to remove. It must now be a refusal (`None`), which the native
/// entry point turns into a thrown exception.
#[test]
fn non_canonical_scalars_other_than_the_group_order_are_refused() {
    // n + 1: still `>= n`, so still non-canonical, but not the one value
    // `ECDHKeyAgreement.validate` depends on. Every order ends in a byte
    // below 0xff, so incrementing cannot carry.
    let mut n_plus_1 = P256_ORDER_BE;
    let last = n_plus_1.len() - 1;
    n_plus_1[last] += 1;

    let (gx, gy) = p256_generator();
    assert!(
        scalar_mul_p256(&gx, &gy, &le(&n_plus_1)).is_none(),
        "n+1 must be refused, not answered with the identity"
    );

    // The all-ones scalar (2^256 - 1) is comfortably above n.
    assert!(
        scalar_mul_p256(&gx, &gy, &[0xffu8; 32]).is_none(),
        "a scalar far above the group order must be refused"
    );

    // ...and the guard did not swallow the one value that must survive it.
    assert_eq!(
        scalar_mul_p256(&gx, &gy, &le(&P256_ORDER_BE)).expect("n·G is still defined"),
        (Vec::new(), Vec::new())
    );

    // Same narrowing on the other two curves.
    let mut n_plus_1 = P384_ORDER_BE;
    let last = n_plus_1.len() - 1;
    n_plus_1[last] += 1;
    let (gx, gy) = generator_of!(p384);
    assert!(scalar_mul_p384(&gx, &gy, &le(&n_plus_1)).is_none());

    let mut n_plus_1 = P521_ORDER_BE;
    let last = n_plus_1.len() - 1;
    n_plus_1[last] += 1;
    let (gx, gy) = generator_of!(p521);
    assert!(scalar_mul_p521(&gx, &gy, &le(&n_plus_1)).is_none());
}

#[test]
fn p521_generator_times_one_and_two_and_identity() {
    let (gx, gy) = generator_of!(p521);
    let (rx, ry) = scalar_mul_p521(&gx, &gy, &[1u8]).expect("1·G must succeed");
    assert_eq!(rx, gx);
    assert_eq!(ry, gy);

    let (r2x, r2y) = scalar_mul_p521(&gx, &gy, &[2u8]).expect("2·G must succeed");
    use p521::elliptic_curve::group::prime::PrimeCurveAffine as _;
    use p521::elliptic_curve::sec1::ToSec1Point as _;
    let expect2 = (p521::ProjectivePoint::from(p521::AffinePoint::generator())
        + p521::ProjectivePoint::from(p521::AffinePoint::generator()))
    .to_affine()
    .to_sec1_point(false);
    assert_eq!(r2x, expect2.x().unwrap().to_vec());
    assert_eq!(r2y, expect2.y().unwrap().to_vec());

    // 0·G and n·G return identity (empty vecs)
    let zero = scalar_mul_p521(&gx, &gy, &[0u8; 66]).expect("0·G is defined");
    assert_eq!(zero, (Vec::new(), Vec::new()), "0·G must be identity");

    let order = scalar_mul_p521(&gx, &gy, &le(&P521_ORDER_BE)).expect("n·G is defined");
    assert_eq!(order, (Vec::new(), Vec::new()), "n·G must be identity");
}

#[test]
fn p521_wrong_length_and_off_curve_refused() {
    let (gx, mut gy) = generator_of!(p521);
    assert!(scalar_mul_p521(&gx[..65], &gy, &[1u8]).is_none(), "short x refused");
    assert!(scalar_mul_p521(&gx, &gy[..65], &[1u8]).is_none(), "short y refused");

    // Off-curve point
    let last = gy.len() - 1;
    gy[last] ^= 0x01;
    assert!(scalar_mul_p521(&gx, &gy, &[1u8]).is_none(), "off-curve point refused");
}
