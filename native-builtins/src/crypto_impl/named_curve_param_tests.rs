use super::*;

#[test]
fn the_generator_is_on_the_curve() {
    for curve in [&NIST_P256, &NIST_P384, &NIST_P521] {
        let params = curve.params();
        let p = &params.p;
        let lhs = mod_sqr(&params.gy, p);
        let rhs = mod_add(
            &mod_sub(
                &mod_mul(&mod_sqr(&params.gx, p), &params.gx, p),
                &mod_mul_small(&params.gx, 3, p),
                p,
            ),
            &params.b,
            p,
        );
        assert_eq!(
            lhs.to_bytes_be(),
            rhs.to_bytes_be(),
            "{}: the generator is not on y^2 = x^3 - 3x + b — a parameter is wrong",
            curve.name
        );
    }
}

/// `n * G` is the identity: the definition of the group order, and the one
/// relation that exercises the whole double-and-add ladder — both point
/// formulas, on every curve — against a value that is not derived from
/// them.
#[test]
fn the_generator_has_the_stated_order() {
    for curve in [&NIST_P256, &NIST_P384, &NIST_P521] {
        let params = curve.params();
        let g = JPoint::from_affine(params.gx.clone(), params.gy.clone());
        assert!(
            g.scalar_mul(&params.n, &params.p).is_infinity(),
            "{}: n*G is not the identity — the group law or a parameter is wrong",
            curve.name
        );
        // …and (n-1)*G is NOT, or a ladder that returned infinity for
        // every scalar would pass the line above just as well.
        assert!(
            !g.scalar_mul(&params.n.sub(&BigUint::one()), &params.p)
                .is_infinity(),
            "{}: (n-1)*G is the identity — the ladder collapses everything",
            curve.name
        );
    }
}

#[test]
fn field_and_order_have_the_documented_bit_lengths() {
    for (curve, bits) in [(&NIST_P256, 256), (&NIST_P384, 384), (&NIST_P521, 521)] {
        let params = curve.params();
        assert_eq!(params.p.bit_length(), bits, "{} p", curve.name);
        assert_eq!(params.n.bit_length(), bits, "{} n", curve.name);
        assert_eq!(
            curve.field_bytes,
            bits.div_ceil(8),
            "{} field_bytes",
            curve.name
        );
    }
}

/// The OID is READ, not assumed. That is the whole difference from
/// `parse_ecdsa_public_key`, which discards the named-curve OID and then
/// fails on the point LENGTH — so a perfectly good P-384 key read as a
/// corrupt one and the caller reported a bad signature.
#[test]
fn an_unsupported_curve_oid_parses_to_none() {
    // secp256k1 (1.3.132.0.10): a real curve, deliberately not implemented.
    assert!(nist_curve_for_oid(&[0x2b, 0x81, 0x04, 0x00, 0x0a]).is_none());
    assert_eq!(
        nist_curve_for_oid(OID_EC_P256).map(|c| c.name),
        Some("P-256")
    );
    assert_eq!(
        nist_curve_for_oid(OID_EC_P384).map(|c| c.name),
        Some("P-384")
    );
    assert_eq!(
        nist_curve_for_oid(OID_EC_P521).map(|c| c.name),
        Some("P-521")
    );
}

/// A key that is not ON the curve must be refused before the group law
/// touches it — the invalid-curve attack, which this verifier is reachable
/// from certificate parsing by.
#[test]
fn a_public_key_off_the_curve_is_refused() {
    let params = NIST_P384.params();
    let key = NamedEcPublicKey {
        curve: &NIST_P384,
        x: params.gx.clone(),
        y: mod_add(&params.gy, &BigUint::one(), &params.p),
    };
    // A well-formed (r, s), so the refusal cannot be the signature
    // decoder's doing instead.
    let sig = der_encode_ecdsa_signature(&[0x01, 0x02], &[0x03, 0x04]);
    assert!(!verify_named_ecdsa(&key, &[7u8; 48], &sig));
}
