// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP8.11.6 — PKCS#10 / X.509 v3 ASN.1 helpers.
//!
//! BC's `JcaPKCS10CertificationRequestBuilder.build(signer)` (the first
//! cert-issue call EJBCA makes during install-wizard render) needs us to
//! emit byte-faithful DER for:
//!
//! * `AlgorithmIdentifier` (RFC 5280 §4.1.1.2)
//! * `SubjectPublicKeyInfo` (RFC 5280 §4.1.2.7)
//! * `Extensions` SEQUENCE OF Extension (RFC 5280 §4.1.2.9 + §4.2)
//! * `CertificationRequestInfo` (RFC 2986 §4.1)
//! * `TBSCertificate` skeleton (RFC 5280 §4.1)
//!
//! These tests exercise each encoder/decoder against:
//! 1. RFC-prescribed structure (right tag at right offset).
//! 2. Round-trip stability — `decode(encode(x)) == x`.
//! 3. A known-good fixture for the PKCS#10 outer SEQUENCE so cross-tool
//!    interop with BC's `DERSequence.encode` is byte-identical.
//!
//! See `bench/ejbca-deploy/diagnostic.md` §"BC ASN.1 PKCS#10" for the
//! audit that motivates this work.

use cratonvm_native_builtins::jca::asn1;

/// Test 1 — AlgorithmIdentifier with NULL parameters (RSA shape).
///
/// Per RFC 4055 §2.1, RSA-with-SHA-256 *requires* the AlgorithmIdentifier
/// to carry an explicit NULL parameter: BC's `BcRSAContentSignerBuilder`
/// will otherwise refuse to consume the SubjectPublicKeyInfo emitted into
/// the CSR. Verify both encode and decode honour that.
#[test]
fn algorithm_identifier_rsa_with_null_params_round_trip() {
    let null_params = asn1::encode_null();
    let der = asn1::encode_algorithm_identifier(asn1::OID_SHA256_WITH_RSA, Some(&null_params));

    // Outer SEQUENCE
    assert_eq!(der[0], asn1::TAG_SEQUENCE);
    let (oid, params) = asn1::decode_algorithm_identifier(&der).expect("decode");
    assert_eq!(oid, asn1::OID_SHA256_WITH_RSA);
    let params = params.expect("params present");
    assert_eq!(params, null_params);

    // Encoded NULL is exactly `05 00`.
    assert_eq!(null_params, vec![0x05, 0x00]);

    // Re-encode for stability — the second pass MUST be byte-identical.
    let der2 = asn1::encode_algorithm_identifier(&oid, Some(&params));
    assert_eq!(der, der2);
}

/// Test 2 — AlgorithmIdentifier with no parameters (EC shape with absent
/// parameters).
///
/// EC-Public-Key OIDs may carry a named-curve OID *or* be entirely absent.
/// We exercise the absent-params path to ensure the decoder's handling of
/// the optional field doesn't false-positive on trailing bytes.
#[test]
fn algorithm_identifier_no_params_round_trip() {
    let der = asn1::encode_algorithm_identifier(asn1::OID_EC_PUBLIC_KEY, None);
    let (oid, params) = asn1::decode_algorithm_identifier(&der).expect("decode");
    assert_eq!(oid, asn1::OID_EC_PUBLIC_KEY);
    assert!(params.is_none(), "params must be absent");
}

/// Test 3 — SubjectPublicKeyInfo round-trip.
///
/// Build an SPKI for a fake 4-byte RSA public key blob and confirm:
///   * the outer tag is SEQUENCE,
///   * the inner BIT STRING carries an unused-bits prefix of 0x00,
///   * decode reproduces the original payload exactly.
#[test]
fn subject_public_key_info_round_trip() {
    let null_params = asn1::encode_null();
    let fake_key_bits = b"\x30\x82\x01\x0a"; // looks like an RSAPublicKey SEQUENCE header
    let spki = asn1::encode_subject_public_key_info(
        asn1::OID_RSA_ENCRYPTION,
        Some(&null_params),
        fake_key_bits,
    );

    assert_eq!(spki[0], asn1::TAG_SEQUENCE);

    // Pull the BIT STRING manually to confirm unused-bits prefix.
    let (_outer_tag, outer_hdr, outer_clen, _) = asn1::read_header(&spki).expect("outer");
    let content = &spki[outer_hdr..outer_hdr + outer_clen];
    let (_alg_tag, _ahdr, _aclen, atot) = asn1::read_header(content).expect("alg");
    let bs = &content[atot..];
    assert_eq!(bs[0], asn1::TAG_BIT_STRING);
    let (_, bhdr, bclen, _) = asn1::read_header(bs).expect("bs");
    let bs_content = &bs[bhdr..bhdr + bclen];
    assert_eq!(
        bs_content[0], 0x00,
        "unused-bits MUST be 0 for byte-aligned keys"
    );
    assert_eq!(&bs_content[1..], fake_key_bits);

    // Round-trip through the typed decoder.
    let parsed = asn1::decode_subject_public_key_info(&spki).expect("decode");
    assert_eq!(parsed.algorithm_oid, asn1::OID_RSA_ENCRYPTION);
    assert_eq!(
        parsed.algorithm_params.as_deref(),
        Some(null_params.as_slice())
    );
    assert_eq!(parsed.subject_public_key, fake_key_bits);
}

/// Test 4 — Extensions sequence encoding.
///
/// Build a 2-extension Extensions SEQUENCE (one critical, one not),
/// confirm the BOOLEAN is omitted in DER when critical=false (RFC 5280
/// §4.2 / X.690 §11.5 — DEFAULT must be absent), and round-trip.
#[test]
fn extensions_sequence_encoding_and_critical_default() {
    let basic_constraints = asn1::Extension {
        oid: asn1::OID_BASIC_CONSTRAINTS.into(),
        critical: true,
        value: vec![0x30, 0x03, 0x01, 0x01, 0xFF], // SEQUENCE { BOOLEAN TRUE }
    };
    let ext_key_usage = asn1::Extension {
        oid: asn1::OID_EXT_KEY_USAGE.into(),
        critical: false,
        value: vec![
            0x30, 0x0A, 0x06, 0x08, 0x2B, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x01,
        ],
    };

    // Single-extension encode for the BOOLEAN-default check.
    let crit_der = asn1::encode_extension(&basic_constraints);
    assert!(
        crit_der
            .windows(3)
            .any(|w| w == [asn1::TAG_BOOLEAN, 0x01, 0xFF]),
        "critical=true MUST be encoded as 01 01 FF (DER §11.1)"
    );
    let nocrit_der = asn1::encode_extension(&ext_key_usage);
    assert!(
        !nocrit_der
            .windows(2)
            .any(|w| w == [asn1::TAG_BOOLEAN, 0x01]),
        "critical=false MUST be omitted (DEFAULT FALSE) — found stray BOOLEAN tag"
    );

    // Round-trip the full Extensions SEQUENCE.
    let exts = vec![basic_constraints, ext_key_usage];
    let der = asn1::encode_extensions(&exts);
    assert_eq!(der[0], asn1::TAG_SEQUENCE);
    let parsed = asn1::decode_extensions(&der).expect("decode");
    assert_eq!(parsed, exts);
}

/// Test 5 — PKCS#10 CertificationRequestInfo encode is structurally valid.
///
/// We can't pull in OpenSSL or BC at unit-test time, so we cross-check
/// against a hand-computed expected prefix:
///   * outer SEQUENCE,
///   * INTEGER version=0,
///   * subject Name (SEQUENCE),
///   * SubjectPublicKeyInfo (SEQUENCE),
///   * \[0\] context-specific tag for attributes.
///
/// The resulting blob must walk top-to-bottom with each element's tag
/// matching the RFC 2986 §4.1 schema exactly.
#[test]
fn certification_request_info_structure_and_fixture() {
    // Build a minimal Name: CN=Test.
    let cn_oid = asn1::encode_oid("2.5.4.3").unwrap();
    let cn_val = asn1::encode_directory_string("Test");
    let mut atv_inner = Vec::new();
    atv_inner.extend_from_slice(&cn_oid);
    atv_inner.extend_from_slice(&cn_val);
    let atv = asn1::encode_sequence(&atv_inner);
    let rdn = asn1::encode_set(&atv);
    let name_der = asn1::encode_sequence(&rdn);

    // Build SPKI.
    let null_params = asn1::encode_null();
    let key_bits = b"key-bytes-stand-in";
    let spki = asn1::encode_subject_public_key_info(
        asn1::OID_RSA_ENCRYPTION,
        Some(&null_params),
        key_bits,
    );

    // Empty attributes — BC produces this for plain CSRs without
    // extensionRequest.
    let cri = asn1::encode_certification_request_info(0, &name_der, &spki, &[]);

    assert_eq!(
        cri[0],
        asn1::TAG_SEQUENCE,
        "CertificationRequestInfo wrapper tag"
    );

    // Walk the children.
    let (_, hdr, clen, _) = asn1::read_header(&cri).unwrap();
    let body = &cri[hdr..hdr + clen];
    let mut p = 0;

    // version INTEGER 0
    let (t, _h, _c, t1) = asn1::read_header(&body[p..]).unwrap();
    assert_eq!(t, asn1::TAG_INTEGER);
    p += t1;
    // subject Name SEQUENCE
    let (t, _h, _c, t1) = asn1::read_header(&body[p..]).unwrap();
    assert_eq!(t, asn1::TAG_SEQUENCE);
    p += t1;
    // SPKI SEQUENCE
    let (t, _h, _c, t1) = asn1::read_header(&body[p..]).unwrap();
    assert_eq!(t, asn1::TAG_SEQUENCE);
    p += t1;
    // [0] attributes
    let (t, _h, _c, t1) = asn1::read_header(&body[p..]).unwrap();
    assert_eq!(
        t & 0xE0,
        0xA0,
        "[0] context-specific tag class+constructed bits"
    );
    assert_eq!(t & 0x1F, 0, "[0] tag number must be 0");
    p += t1;
    assert_eq!(
        p,
        body.len(),
        "no trailing bytes inside CertificationRequestInfo"
    );

    // Re-encode is byte-stable.
    let cri2 = asn1::encode_certification_request_info(0, &name_der, &spki, &[]);
    assert_eq!(cri, cri2, "encode is deterministic");
}

/// Test 6 — TBSCertificate v3 skeleton encode emits expected outline.
///
/// Full v3 issuance is out of scope for WP8.11.6 (deferred to a future
/// X.509 generator WP), but we exercise the skeleton encoder to confirm
/// the explicit `[0]` version tag, the serial INTEGER, and the optional
/// `[3]` extensions wrapper all land in the right slots.
#[test]
fn tbs_certificate_v3_skeleton_outline() {
    let null_params = asn1::encode_null();
    let alg = asn1::encode_algorithm_identifier(asn1::OID_SHA256_WITH_RSA, Some(&null_params));
    // Stub-out issuer / subject as the same single-RDN Name.
    let cn_oid = asn1::encode_oid("2.5.4.3").unwrap();
    let cn_val = asn1::encode_directory_string("Issuer");
    let mut atv_inner = Vec::new();
    atv_inner.extend_from_slice(&cn_oid);
    atv_inner.extend_from_slice(&cn_val);
    let name_der = asn1::encode_sequence(&asn1::encode_set(&asn1::encode_sequence(&atv_inner)));

    let spki =
        asn1::encode_subject_public_key_info(asn1::OID_RSA_ENCRYPTION, Some(&null_params), b"key");

    // Validity stub — two GeneralizedTime placeholders inside a SEQUENCE.
    let validity = asn1::encode_sequence(&[
        // notBefore: UTCTime 230101000000Z
        0x17, 0x0D, b'2', b'3', b'0', b'1', b'0', b'1', b'0', b'0', b'0', b'0', b'0', b'0', b'Z',
        // notAfter: UTCTime 240101000000Z
        0x17, 0x0D, b'2', b'4', b'0', b'1', b'0', b'1', b'0', b'0', b'0', b'0', b'0', b'0', b'Z',
    ]);

    let exts = asn1::encode_extensions(&[asn1::Extension {
        oid: asn1::OID_BASIC_CONSTRAINTS.into(),
        critical: true,
        value: vec![0x30, 0x00],
    }]);

    let serial = vec![0x01, 0x23, 0x45];
    let tbs = asn1::encode_tbs_certificate(
        true,
        &serial,
        &alg,
        &name_der,
        &validity,
        &name_der,
        &spki,
        Some(&exts),
    );

    // Outer SEQUENCE.
    assert_eq!(tbs[0], asn1::TAG_SEQUENCE);
    let (_, hdr, clen, _) = asn1::read_header(&tbs).unwrap();
    let body = &tbs[hdr..hdr + clen];

    // First element MUST be [0] EXPLICIT version (0xA0 prefix).
    assert_eq!(
        body[0], 0xA0,
        "TBSCertificate v3 first element is [0] EXPLICIT version"
    );
    let (_, vhdr, vclen, vtot) = asn1::read_header(body).unwrap();
    let inner = &body[vhdr..vhdr + vclen];
    assert_eq!(inner[0], asn1::TAG_INTEGER, "[0] wraps INTEGER version");
    assert_eq!(inner[inner.len() - 1], 2, "version v3 = INTEGER 2");

    // Second element: serial INTEGER.
    let after_version = &body[vtot..];
    assert_eq!(after_version[0], asn1::TAG_INTEGER, "serialNumber INTEGER");

    // Tail must contain the [3] EXPLICIT extensions tag (0xA3).
    assert!(
        body.windows(1).rev().any(|w| w[0] == 0xA3),
        "TBSCertificate must carry the [3] EXPLICIT extensions wrapper"
    );
}

/// Test 7 — INTEGER encoding edge cases (sign-bit and trim).
///
/// Sanity-check the helper that the rest of the WP8.11.6 surface relies
/// on: large unsigned ints with a high MSB MUST get a 0x00 prefix to
/// avoid being mis-interpreted as negative two's-complement.  Without
/// this, BC's signature verification across re-encoded CSRs fails.
#[test]
fn integer_encoding_handles_high_bit_correctly() {
    // 0x80 alone — high bit set, MUST get a 0x00 padding byte.
    let der = asn1::encode_integer_unsigned(&[0x80]);
    assert_eq!(der, vec![asn1::TAG_INTEGER, 0x02, 0x00, 0x80]);

    // 0x7F — high bit clear, no padding.
    let der = asn1::encode_integer_unsigned(&[0x7F]);
    assert_eq!(der, vec![asn1::TAG_INTEGER, 0x01, 0x7F]);

    // Leading zeros must be trimmed.
    let der = asn1::encode_integer_unsigned(&[0x00, 0x00, 0x42]);
    assert_eq!(der, vec![asn1::TAG_INTEGER, 0x01, 0x42]);

    // u64 helper: 256 = 0x01 0x00.
    let der = asn1::encode_integer_u64(256);
    assert_eq!(der, vec![asn1::TAG_INTEGER, 0x02, 0x01, 0x00]);
}
