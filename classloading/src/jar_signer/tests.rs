use super::*;

// -----------------------------------------------------------------
// DER builder helpers — minimal encoder used by the test fixtures.
// We deliberately re-implement these here (instead of pulling
// `native-builtins::jca::asn1`) to keep the test self-contained and
// independent of the rest of the workspace.
// -----------------------------------------------------------------

/// Task #40: trust-store-aware tests below build real X.509-shaped
/// fixtures and supply a proper [`TrustStore`].  The older
/// self-consistency-only tests in this module embed marker-shaped
/// "certs" that won't parse as X.509 — they use this permissive
/// store to keep covering the pre-chain code paths.
fn legacy_ts() -> TrustStore {
    TrustStore::permissive_legacy_tests()
}

/// The `permissive_legacy` flag switches off BOTH the SignerInfo
/// public-key check and the chain walk. Every constructor that a
/// production build can reach must leave it `false` — the only one that
/// sets it is `permissive_legacy_tests`, which is `#[cfg(test)]` and so
/// does not exist outside this configuration.
///
/// If a new non-test constructor ever sets the flag, this test fails and
/// the regression is caught before it becomes a silent "signature
/// verification is off in release" bug.
#[test]
fn production_trust_store_constructors_are_never_permissive() {
    assert!(
        !TrustStore::default().permissive_legacy,
        "TrustStore::default() must enforce the pubkey + chain checks"
    );
    assert!(
        !TrustStore::empty().permissive_legacy,
        "TrustStore::empty() must enforce the pubkey + chain checks"
    );
    // An empty store is not "permissive" — it is maximally strict: with
    // no anchors, `verify_chain` bottoms out in NoTrustAnchor.
    assert_eq!(TrustStore::empty().anchor_count(), 0);
    // And the test-only escape hatch really does flip it, so the two
    // assertions above are not vacuous.
    assert!(TrustStore::permissive_legacy_tests().permissive_legacy);
}

fn enc_len(len: usize) -> Vec<u8> {
    if len < 0x80 {
        vec![len as u8]
    } else if len < 0x100 {
        vec![0x81, len as u8]
    } else if len < 0x10000 {
        vec![0x82, (len >> 8) as u8, len as u8]
    } else {
        vec![0x83, (len >> 16) as u8, (len >> 8) as u8, len as u8]
    }
}

fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    out.extend(enc_len(content.len()));
    out.extend_from_slice(content);
    out
}

fn seq(content: &[u8]) -> Vec<u8> {
    tlv(TAG_SEQUENCE, content)
}

fn set(content: &[u8]) -> Vec<u8> {
    tlv(TAG_SET, content)
}

fn octet(content: &[u8]) -> Vec<u8> {
    tlv(TAG_OCTET_STRING, content)
}

fn integer(value: u64) -> Vec<u8> {
    if value == 0 {
        return tlv(TAG_INTEGER, &[0]);
    }
    let mut bytes = Vec::new();
    let mut v = value;
    while v > 0 {
        bytes.push((v & 0xFF) as u8);
        v >>= 8;
    }
    bytes.reverse();
    if bytes[0] & 0x80 != 0 {
        let mut content = vec![0u8];
        content.extend(bytes);
        return tlv(TAG_INTEGER, &content);
    }
    tlv(TAG_INTEGER, &bytes)
}

fn oid(dotted: &str) -> Vec<u8> {
    let arcs: Vec<u64> = dotted.split('.').map(|s| s.parse().unwrap()).collect();
    let mut content = vec![(arcs[0] * 40 + arcs[1]) as u8];
    for &a in &arcs[2..] {
        if a == 0 {
            content.push(0);
            continue;
        }
        let mut buf: Vec<u8> = Vec::new();
        let mut v = a;
        while v > 0 {
            buf.push((v & 0x7F) as u8);
            v >>= 7;
        }
        for (i, b) in buf.iter().rev().enumerate() {
            let last = i + 1 == buf.len();
            content.push(if last { *b } else { *b | 0x80 });
        }
    }
    tlv(TAG_OID, &content)
}

fn ctx_imp(n: u8, content: &[u8]) -> Vec<u8> {
    // Implicit context-specific constructed tag.
    tlv(0xA0 | (n & 0x1F), content)
}

fn algorithm_identifier(o: &str) -> Vec<u8> {
    let mut inner = oid(o);
    inner.extend_from_slice(&[TAG_NULL, 0x00]); // params NULL
    seq(&inner)
}

/// Build a minimal X.509-shaped SEQUENCE with a recognizable CN.
/// The bytes are NOT a valid signed certificate — they only need to
/// be a well-formed top-level SEQUENCE that our cert-set splitter
/// keeps.  We embed a "marker" payload so the test can confirm the
/// returned DER matches the input cert byte-for-byte.
fn fake_cert_der(marker: &str) -> Vec<u8> {
    let inner = octet(marker.as_bytes());
    seq(&inner)
}

/// Build a valid IssuerAndSerialNumber containing a CN-only Name.
fn issuer_and_serial(cn: &str, serial: u64) -> Vec<u8> {
    // Name ::= SEQUENCE OF RDN; each RDN is SET OF ATV.
    // ATV ::= SEQUENCE { type OID, value ANY (we use PrintableString=0x13) }
    let atv = seq(&[
        oid("2.5.4.3").as_slice(),
        tlv(0x13, cn.as_bytes()).as_slice(),
    ]
    .concat());
    let rdn = set(&atv);
    let name = seq(&rdn);
    let iasn_inner = [name.as_slice(), integer(serial).as_slice()].concat();
    seq(&iasn_inner)
}

/// Build a SignedData ContentInfo binding a single SignerInfo whose
/// `messageDigest` authenticated attribute is `digest_alg(sf_bytes)`.
/// `tamper_digest=true` replaces the computed digest with random
/// bytes to exercise the integrity-rejection path.
fn build_signer_block(sf_bytes: &[u8], digest_alg: DigestAlg, tamper_digest: bool) -> Vec<u8> {
    let dig = match digest_alg {
        DigestAlg::Sha256 => sha256::digest(sf_bytes).to_vec(),
        DigestAlg::Sha1 => sha1::digest(sf_bytes).to_vec(),
        _ => vec![0; 32],
    };
    let stored = if tamper_digest {
        vec![
            0xDE, 0xAD, 0xBE, 0xEF, 0xDE, 0xAD, 0xBE, 0xEF, 0xDE, 0xAD, 0xBE, 0xEF, 0xDE, 0xAD,
            0xBE, 0xEF, 0xDE, 0xAD, 0xBE, 0xEF, 0xDE, 0xAD, 0xBE, 0xEF, 0xDE, 0xAD, 0xBE, 0xEF,
            0xDE, 0xAD, 0xBE, 0xEF,
        ]
    } else {
        dig
    };
    let digest_oid_dotted = match digest_alg {
        DigestAlg::Sha256 => OID_SHA256,
        DigestAlg::Sha1 => OID_SHA1,
        _ => OID_SHA256,
    };

    // authenticatedAttributes [0] IMPLICIT SET OF Attribute
    let attr_content_type = seq(&[
        oid(OID_CONTENT_TYPE).as_slice(),
        set(&oid(OID_DATA)).as_slice(),
    ]
    .concat());
    let attr_message_digest = seq(&[
        oid(OID_MESSAGE_DIGEST).as_slice(),
        set(&octet(&stored)).as_slice(),
    ]
    .concat());
    let attrs_set_content =
        [attr_content_type.as_slice(), attr_message_digest.as_slice()].concat();
    let auth_attrs = ctx_imp(0, &attrs_set_content);

    // SignerInfo
    let signer_info = seq(&[
        integer(1).as_slice(),
        issuer_and_serial("TestSigner", 0x42).as_slice(),
        algorithm_identifier(digest_oid_dotted).as_slice(),
        auth_attrs.as_slice(),
        algorithm_identifier("1.2.840.113549.1.1.1").as_slice(), // signatureAlgorithm
        octet(b"fake-signature-bytes").as_slice(),               // signatureValue
    ]
    .concat());

    // Certificates [0] IMPLICIT — one fake cert.
    let cert = fake_cert_der("MARKER-CERT");
    let certs = ctx_imp(0, &cert);

    // encapContentInfo ::= SEQUENCE { contentType OID }  (detached, no content)
    let encap = seq(&oid(OID_DATA));

    // digestAlgorithms SET
    let digest_algs = set(&algorithm_identifier(digest_oid_dotted));

    // SignedData SEQUENCE
    let signed_data = seq(&[
        integer(1).as_slice(),
        digest_algs.as_slice(),
        encap.as_slice(),
        certs.as_slice(),
        set(&signer_info).as_slice(),
    ]
    .concat());

    // Outer ContentInfo
    seq(&[
        oid(OID_SIGNED_DATA).as_slice(),
        ctx_imp(0, &signed_data).as_slice(),
    ]
    .concat())
}

#[test]
fn verifies_self_consistent_sha256_signer_block() {
    let sf = b"Signature-Version: 1.0\r\nSHA-256-Digest-Manifest: abc123=\r\n\r\n";
    let block = build_signer_block(sf, DigestAlg::Sha256, /*tamper=*/ false);
    let vs =
        verify_signer_block(&block, sf, &legacy_ts()).expect("well-formed block should verify");
    assert_eq!(vs.digest_alg, DigestAlg::Sha256);
    assert_eq!(vs.chain.len(), 1);
    // Cert bytes must be the *embedded* cert DER, not the outer block.
    assert_eq!(vs.chain[0], fake_cert_der("MARKER-CERT"));
    // Principal extraction is best-effort but should contain the CN.
    assert!(
        vs.principal.contains("TestSigner"),
        "principal {:?} should contain CN value",
        vs.principal
    );
}

#[test]
fn verifies_self_consistent_sha1_signer_block() {
    // Legacy jarsigner JARs (pre-JDK 9) used SHA-1.  We must still
    // accept those — but only when the .SF digest agrees.
    let sf = b"Signature-Version: 1.0\r\nSHA1-Digest-Manifest: xyz=\r\n\r\n";
    let block = build_signer_block(sf, DigestAlg::Sha1, /*tamper=*/ false);
    let vs = verify_signer_block(&block, sf, &legacy_ts()).expect("SHA-1 block should verify");
    assert_eq!(vs.digest_alg, DigestAlg::Sha1);
    assert_eq!(vs.chain.len(), 1);
}

#[test]
fn rejects_tampered_sf_via_digest_mismatch() {
    // An attacker modifies the .SF after signing.  The messageDigest
    // attribute stored in the SignerInfo (which we cannot rewrite
    // without the signing key) will no longer equal SHA-256(.SF).
    let sf_original = b"Signature-Version: 1.0\r\n\r\n";
    let block = build_signer_block(sf_original, DigestAlg::Sha256, /*tamper=*/ false);

    let sf_tampered = b"Signature-Version: 1.0\r\nEvil: yes\r\n\r\n";
    assert!(
        verify_signer_block(&block, sf_tampered, &legacy_ts()).is_none(),
        "tampered .SF must be rejected"
    );

    // Sanity: original .SF still verifies.
    assert!(verify_signer_block(&block, sf_original, &legacy_ts()).is_some());
}

#[test]
fn rejects_tampered_message_digest_attribute() {
    // The block itself has been tampered to flip the messageDigest
    // attribute (without recomputing the signature).  Even though
    // the .SF is genuine, the recomputed SHA-256 won't equal the
    // stored bytes.
    let sf = b"Signature-Version: 1.0\r\n\r\n";
    let block = build_signer_block(sf, DigestAlg::Sha256, /*tamper=*/ true);
    assert!(verify_signer_block(&block, sf, &legacy_ts()).is_none());
}

#[test]
fn rejects_empty_block_without_panic() {
    assert!(verify_signer_block(&[], b"sf", &legacy_ts()).is_none());
}

#[test]
fn rejects_truncated_block_without_panic() {
    // A 3-byte stub — not even enough for a TLV header / length.
    assert!(verify_signer_block(&[0x30, 0x82, 0xFF], b"sf", &legacy_ts()).is_none());
}

#[test]
fn rejects_garbage_bytes_without_panic() {
    // 256 random-ish bytes that aren't valid DER.
    let garbage: Vec<u8> = (0..=255u8).collect();
    assert!(verify_signer_block(&garbage, b"sf", &legacy_ts()).is_none());
}

#[test]
fn rejects_oversized_block_without_panic() {
    let huge = vec![0u8; MAX_SIGNER_BLOCK + 1];
    assert!(verify_signer_block(&huge, b"sf", &legacy_ts()).is_none());
}

#[test]
fn rejects_block_with_wrong_outer_oid() {
    // Replace pkcs7-signedData with pkcs7-data — should bounce.
    let inner = seq(&oid("1.2.3.4.5"));
    let bad = seq(&[oid(OID_DATA).as_slice(), ctx_imp(0, &inner).as_slice()].concat());
    assert!(verify_signer_block(&bad, b"sf", &legacy_ts()).is_none());
}

#[test]
fn rejects_block_missing_signer_info() {
    // SignedData with empty signerInfos SET.
    let signed_data = seq(&[
        integer(1).as_slice(),
        set(&algorithm_identifier(OID_SHA256)).as_slice(),
        seq(&oid(OID_DATA)).as_slice(), // encap
        set(&[]).as_slice(),            // empty signerInfos
    ]
    .concat());
    let block = seq(&[
        oid(OID_SIGNED_DATA).as_slice(),
        ctx_imp(0, &signed_data).as_slice(),
    ]
    .concat());
    assert!(verify_signer_block(&block, b"sf", &legacy_ts()).is_none());
}

#[test]
fn rejects_block_missing_authenticated_attributes() {
    // The integrity check is anchored in the messageDigest
    // attribute; a SignerInfo without authenticatedAttributes must
    // be refused outright (otherwise an attacker just strips them).
    let signer_info = seq(&[
        integer(1).as_slice(),
        issuer_and_serial("TestSigner", 1).as_slice(),
        algorithm_identifier(OID_SHA256).as_slice(),
        // no [0] IMPLICIT attrs
        algorithm_identifier("1.2.840.113549.1.1.1").as_slice(),
        octet(b"sig").as_slice(),
    ]
    .concat());
    let cert = fake_cert_der("CERT");
    let signed_data = seq(&[
        integer(1).as_slice(),
        set(&algorithm_identifier(OID_SHA256)).as_slice(),
        seq(&oid(OID_DATA)).as_slice(),
        ctx_imp(0, &cert).as_slice(),
        set(&signer_info).as_slice(),
    ]
    .concat());
    let block = seq(&[
        oid(OID_SIGNED_DATA).as_slice(),
        ctx_imp(0, &signed_data).as_slice(),
    ]
    .concat());
    assert!(verify_signer_block(&block, b"sf", &legacy_ts()).is_none());
}

#[test]
fn rejects_unsupported_digest_alg() {
    // SHA-512 — recognised OID but the implementation returns an
    // empty digest, which can never equal the stored 64-byte digest.
    let sf = b"any";
    let block = build_signer_block(sf, DigestAlg::Sha512, /*tamper=*/ false);
    assert!(verify_signer_block(&block, sf, &legacy_ts()).is_none());
}

#[test]
fn decode_oid_handles_signed_data_oid() {
    let bytes = [0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x07, 0x02];
    assert_eq!(decode_oid(&bytes).unwrap(), OID_SIGNED_DATA);
}

#[test]
fn decode_oid_rejects_truncated_arc() {
    // Continuation bit set on the last byte → truncated arc.
    let bytes = [0x2A, 0x86];
    assert!(decode_oid(&bytes).is_err());
}

// ---------------------------------------------------------------------
// Task #40 — trust-store + chain verification tests
//
// Each fixture builds a synthetic X.509-shaped certificate whose
// `signatureAlgorithm` is `OID_STUB_SIG` and whose `signatureValue`
// is `SHA-256(TBSCertificate || parent.subject_dn)`.  The
// synthetic algorithm is recognised exclusively by these tests —
// production chains hitting `OID_STUB_SIG` would already be
// rejected upstream as a non-standard OID.
// ---------------------------------------------------------------------

/// Build a `Name ::= SEQUENCE { RDN }` with a single CN attribute.
fn x509_name(cn: &str) -> Vec<u8> {
    let atv = seq(&[
        oid("2.5.4.3").as_slice(),
        tlv(0x13, cn.as_bytes()).as_slice(),
    ]
    .concat());
    let rdn = set(&atv);
    seq(&rdn)
}

/// Minimal Validity ::= SEQUENCE { UTCTime, UTCTime }.  Values are
/// fixed strings — the chain code doesn't look at them.
fn x509_validity() -> Vec<u8> {
    let nb = tlv(0x17, b"260101000000Z"); // 2026-01-01
    let na = tlv(0x17, b"360101000000Z"); // 2036-01-01
    seq(&[nb.as_slice(), na.as_slice()].concat())
}

/// Minimal SubjectPublicKeyInfo ::= SEQUENCE { AlgorithmIdentifier,
/// BIT STRING }.  We pin the subject DN of the cert as the
/// "public key" bytes so that [`X509Cert::link_signature_ok`] can
/// recover the parent's identity from the child's signature
/// equation.  This is a stand-in for real RSA/ECDSA encoding.
fn x509_spki(subject_dn: &[u8]) -> Vec<u8> {
    let algid = algorithm_identifier(OID_STUB_SIG);
    // BIT STRING = leading 0x00 (unused bits) || subject_dn
    let mut bs = vec![0u8];
    bs.extend_from_slice(subject_dn);
    let bit_string = tlv(0x03, &bs);
    seq(&[algid.as_slice(), bit_string.as_slice()].concat())
}

/// Build the TBSCertificate body for a synthetic X.509 cert.
fn build_tbs(subject_cn: &str, issuer_cn: &str) -> (Vec<u8>, Vec<u8>) {
    let subject_dn = x509_name(subject_cn);
    let issuer_dn = x509_name(issuer_cn);
    let version = ctx_imp(0, &integer(2)); // v3
    let serial = integer(1);
    let signature_alg = algorithm_identifier(OID_STUB_SIG);
    let validity = x509_validity();
    let spki = x509_spki(&subject_dn);
    let tbs_body = [
        version.as_slice(),
        serial.as_slice(),
        signature_alg.as_slice(),
        issuer_dn.as_slice(),
        validity.as_slice(),
        subject_dn.as_slice(),
        spki.as_slice(),
    ]
    .concat();
    (seq(&tbs_body), subject_dn)
}

/// Build a complete X.509 certificate DER, signing with the
/// synthetic stub algorithm.  `parent_subject_dn` is the DER of the
/// parent's `subject` Name TLV — for a root, pass its own
/// subject_dn (self-signed).
fn build_x509_cert(subject_cn: &str, issuer_cn: &str, parent_subject_dn: &[u8]) -> Vec<u8> {
    let (tbs, _) = build_tbs(subject_cn, issuer_cn);
    // signature value = SHA-256(tbs || parent_subject_dn), as a
    // BIT STRING with 0 unused bits.
    let mut buf = Vec::with_capacity(tbs.len() + parent_subject_dn.len());
    buf.extend_from_slice(&tbs);
    buf.extend_from_slice(parent_subject_dn);
    let sig = sha256::digest(&buf);
    let mut bs = vec![0u8];
    bs.extend_from_slice(&sig);
    let bit_string = tlv(0x03, &bs);
    let sig_alg = algorithm_identifier(OID_STUB_SIG);
    seq(&[tbs.as_slice(), sig_alg.as_slice(), bit_string.as_slice()].concat())
}

#[test]
fn task40_self_signed_chain_rejected_without_anchor() {
    // Acceptance #4 case (a): self-signed leaf with no trust anchor
    // present.  verify_chain must refuse with NoTrustAnchor — the
    // exact attack the original task description called out.
    let root_subject_dn = x509_name("SelfSigner");
    let cert_der = build_x509_cert("SelfSigner", "SelfSigner", &root_subject_dn);
    let leaf = X509Cert::parse(&cert_der).expect("parse self-signed");
    assert!(leaf.is_self_signed(), "fixture must be self-signed");

    let ts = TrustStore::empty();
    let err = verify_chain(&leaf, &[], &ts).expect_err("must reject");
    assert_eq!(err, TrustError::NoTrustAnchor);
}

#[test]
fn task40_chain_rooted_in_test_anchor_verifies() {
    // Acceptance #4 case (b): leaf → intermediate → root anchor.
    // Trust store contains the root; verify_chain must succeed.
    let root_subject_dn = x509_name("RootCA");
    let root_der = build_x509_cert("RootCA", "RootCA", &root_subject_dn);

    let int_subject_dn = x509_name("IntermediateCA");
    // Intermediate is signed by the root → parent_subject_dn = root_subject_dn.
    let int_der = build_x509_cert("IntermediateCA", "RootCA", &root_subject_dn);

    // Leaf is signed by the intermediate → parent_subject_dn = int_subject_dn.
    let leaf_der = build_x509_cert("LeafSigner", "IntermediateCA", &int_subject_dn);

    let leaf = X509Cert::parse(&leaf_der).expect("parse leaf");
    let int_cert = X509Cert::parse(&int_der).expect("parse intermediate");

    let mut ts = TrustStore::empty();
    assert!(ts.add_anchor_der(root_der.clone()));
    assert_eq!(ts.anchor_count(), 1);

    verify_chain(&leaf, &[int_cert], &ts).expect("must verify against in-store root");
}

#[test]
fn task40_chain_with_broken_intermediate_signature_fails() {
    // Acceptance #4 case (c): chain whose **intermediate** cert
    // has been tampered after signing — the link from the
    // intermediate up to the root anchor must reject because the
    // intermediate's signatureValue no longer covers its TBS.
    let root_subject_dn = x509_name("RootCA");
    let root_der = build_x509_cert("RootCA", "RootCA", &root_subject_dn);

    let int_subject_dn = x509_name("IntermediateCA");
    // Mint a legitimately-signed intermediate, then tamper its
    // signature byte to break the intermediate→root link
    // specifically.  The leaf→intermediate link must STILL be
    // valid so the walk reaches the broken step.
    let mut int_der = build_x509_cert("IntermediateCA", "RootCA", &root_subject_dn);
    let last = int_der.len() - 1;
    int_der[last] ^= 0xFF;

    let leaf_der = build_x509_cert("LeafSigner", "IntermediateCA", &int_subject_dn);

    let leaf = X509Cert::parse(&leaf_der).expect("parse leaf");
    let int_cert = X509Cert::parse(&int_der).expect("parse intermediate");
    let mut ts = TrustStore::empty();
    ts.add_anchor_der(root_der);

    let err = verify_chain(&leaf, &[int_cert], &ts).expect_err("must reject");
    assert_eq!(err, TrustError::BadSignature);
}

#[test]
fn task40_trust_store_load_default_records_sources() {
    // load_default should always at minimum push the placeholder
    // entries for the system-root-store seam and (optionally) the
    // JDK cacerts path.  We just check the type contract.
    let ts = TrustStore::load_default();
    assert!(
        !ts.sources_loaded.is_empty(),
        "load_default must record at least the system-root placeholder line"
    );
    assert!(
        ts.sources_loaded
            .iter()
            .any(|s| s.starts_with("system-root-store")),
        "system-root-store source line missing from {:?}",
        ts.sources_loaded
    );
}

#[test]
fn task40_real_crypto_algorithm_returns_not_implemented() {
    // A chain link whose outer signatureAlgorithm names a real OID
    // (here sha256WithRSAEncryption, 1.2.840.113549.1.1.11) but whose
    // *parent* carries a synthetic stub SPKI (the test fixtures use
    // `OID_STUB_SIG` as the key-algorithm OID, not a real RSA/EC/DSA
    // key) must be rejected as NotImplemented.  `parse_spki` maps the
    // unknown key OID to `PublicKey::Other`, which cannot bind to the
    // RSA verifier → `SigVerify::Unsupported` → `TrustError::
    // NotImplemented` (fail-closed).  NOTE: RSA/ECDSA/DSA verification
    // is fully implemented — a link with a *real* key of the matching
    // family and a bad signature returns `BadSignature` instead; this
    // case only exercises the unknown-key-type fail-closed path.
    let root_subject_dn = x509_name("RealRoot");
    // Hand-roll a cert whose outer signatureAlgorithm OID is the
    // real PKCS#1 v1.5 RSA-SHA256 identifier.
    let (tbs, _) = build_tbs("RealLeaf", "RealRoot");
    let sig_alg = algorithm_identifier("1.2.840.113549.1.1.11");
    let bit_string = tlv(0x03, &[0u8, 0xDE, 0xAD]); // bogus sig bytes
    let leaf_der = seq(&[tbs.as_slice(), sig_alg.as_slice(), bit_string.as_slice()].concat());

    let leaf = X509Cert::parse(&leaf_der).expect("parse leaf");
    let mut ts = TrustStore::empty();
    // Make a (mismatched) self-signed root anchor with stub-sig so
    // the DN lookup succeeds and we drive the signature step.
    let root_der = build_x509_cert("RealRoot", "RealRoot", &root_subject_dn);
    ts.add_anchor_der(root_der);
    let err = verify_chain(&leaf, &[], &ts).expect_err("real RSA not implemented");
    assert_eq!(err, TrustError::NotImplemented);
}

#[test]
fn task40_pem_bundle_decode_round_trip() {
    // PEM-encode a synthetic anchor DER and confirm
    // TrustStore::load_pem_bundle picks it up.  This exercises the
    // local base64 decoder.
    let root_subject_dn = x509_name("PemRoot");
    let root_der = build_x509_cert("PemRoot", "PemRoot", &root_subject_dn);

    // Hand-roll base64 (no `base64` dep) using std::fmt.
    fn b64(input: &[u8]) -> String {
        const ALPHA: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
        let mut i = 0;
        while i + 3 <= input.len() {
            let b0 = input[i] as u32;
            let b1 = input[i + 1] as u32;
            let b2 = input[i + 2] as u32;
            let n = (b0 << 16) | (b1 << 8) | b2;
            out.push(ALPHA[((n >> 18) & 63) as usize] as char);
            out.push(ALPHA[((n >> 12) & 63) as usize] as char);
            out.push(ALPHA[((n >> 6) & 63) as usize] as char);
            out.push(ALPHA[(n & 63) as usize] as char);
            i += 3;
        }
        let rem = input.len() - i;
        if rem == 1 {
            let n = (input[i] as u32) << 16;
            out.push(ALPHA[((n >> 18) & 63) as usize] as char);
            out.push(ALPHA[((n >> 12) & 63) as usize] as char);
            out.push('=');
            out.push('=');
        } else if rem == 2 {
            let n = ((input[i] as u32) << 16) | ((input[i + 1] as u32) << 8);
            out.push(ALPHA[((n >> 18) & 63) as usize] as char);
            out.push(ALPHA[((n >> 12) & 63) as usize] as char);
            out.push(ALPHA[((n >> 6) & 63) as usize] as char);
            out.push('=');
        }
        out
    }

    let mut pem = String::from("-----BEGIN CERTIFICATE-----\n");
    for chunk in b64(&root_der).as_bytes().chunks(64) {
        pem.push_str(std::str::from_utf8(chunk).unwrap());
        pem.push('\n');
    }
    pem.push_str("-----END CERTIFICATE-----\n");

    let mut ts = TrustStore::empty();
    let n = ts.load_pem_bundle(&pem);
    assert_eq!(n, 1, "exactly one anchor must be decoded");
    assert_eq!(ts.anchor_count(), 1);
}

// ---------------------------------------------------------------------
// Task #1 — real RSA PKCS#1 v1.5 verification tests.
//
// We use a fixed, small (512-bit) textbook RSA key.  512-bit RSA is
// cryptographically dead, but it exercises the *exact* code path real
// 2048/4096-bit jarsigner keys use (modpow + DigestInfo compare) at a
// size the in-test signer can produce quickly.  Signing in the test is
// `DigestInfo^d mod n`; verification is the production `sig^e mod n`.
// ---------------------------------------------------------------------

// A real 512-bit RSA key (n = p*q, e = 65537, d = e^-1 mod phi(n)).
// Generated offline; embedded big-endian.  512-bit is dead crypto but
// exercises the exact production code path (modpow + DigestInfo).
const RSA512_N: &[u8] = &[
    0x9f, 0x99, 0x9b, 0x45, 0xc9, 0xaf, 0xc0, 0x21, 0xf5, 0x6b, 0x81, 0xb7, 0x57, 0xf2, 0x4c,
    0x82, 0x8e, 0x8a, 0xca, 0xa8, 0xb9, 0xc6, 0xbd, 0x61, 0xab, 0xd5, 0xe2, 0xea, 0x35, 0x8c,
    0xd0, 0x63, 0x4b, 0xee, 0x4c, 0x67, 0x39, 0x79, 0x70, 0x41, 0x72, 0xd8, 0xcb, 0xaa, 0x7d,
    0x6d, 0x4e, 0x29, 0xa2, 0x52, 0xa0, 0x26, 0x7a, 0xd9, 0x55, 0x8c, 0xdb, 0xce, 0x22, 0xf2,
    0xc1, 0xc5, 0x9c, 0x09,
];
const RSA512_D: &[u8] = &[
    0x3e, 0x1c, 0x88, 0x8a, 0x1b, 0x58, 0xb3, 0x7c, 0x43, 0xc7, 0xa7, 0xfe, 0xd3, 0x52, 0x2f,
    0xa6, 0x6b, 0x94, 0xe6, 0x13, 0xcd, 0xe0, 0xe3, 0x58, 0xfc, 0x87, 0xcb, 0xbc, 0x7c, 0x44,
    0xa5, 0xe0, 0x2e, 0xc1, 0x4e, 0xbf, 0xc0, 0x4f, 0xc8, 0x72, 0x25, 0x58, 0xff, 0x5c, 0x85,
    0x9b, 0x88, 0x23, 0xa1, 0x79, 0x3a, 0x53, 0xeb, 0x3d, 0x82, 0xc5, 0x94, 0x80, 0x47, 0x9a,
    0x39, 0x1d, 0x6c, 0x01,
];

// A 1024-bit RSA test key (k = 128).  The 512-bit key above cannot hold a
// PKCS#1 v1.5 EMSA block for SHA-384/512 (a 64-byte hash + 19-byte
// DigestInfo prefix + 11-byte minimum padding needs k >= 94), so the
// round-trip test for those digests requires a larger modulus.
const RSA1024_N: &[u8] = &[
    0xe5, 0x2f, 0x42, 0xb1, 0xda, 0x1c, 0x87, 0xb6, 0xcf, 0x39, 0x39, 0xd8, 0x78, 0xc2, 0x3b,
    0x59, 0x53, 0x5b, 0x0f, 0x3a, 0xd8, 0x47, 0xb2, 0x07, 0xd7, 0xb2, 0xdc, 0x49, 0x97, 0xbc,
    0x32, 0xbe, 0x5e, 0x6f, 0xfa, 0xd7, 0xbd, 0xf0, 0x83, 0xb1, 0xfe, 0xe0, 0xc6, 0xaf, 0x69,
    0x84, 0xec, 0x36, 0x90, 0xb6, 0x9f, 0xc3, 0x0a, 0xd8, 0x8b, 0x53, 0x15, 0x4b, 0x1c, 0xaa,
    0xd7, 0x02, 0xf6, 0x0c, 0x94, 0x6f, 0xc8, 0x65, 0xec, 0x8c, 0xf6, 0xb0, 0x6f, 0x03, 0x53,
    0xcc, 0x69, 0x28, 0x41, 0x47, 0x6e, 0x9d, 0x3b, 0x85, 0x3e, 0xd7, 0xed, 0xd5, 0xf7, 0x23,
    0xa8, 0x4b, 0xbc, 0xa7, 0x2d, 0xaa, 0xa3, 0x6b, 0xac, 0xfa, 0xe6, 0x69, 0xbd, 0x13, 0x1c,
    0xcc, 0x3b, 0x08, 0x27, 0x3c, 0x71, 0x2c, 0x83, 0xf0, 0x07, 0x94, 0xfc, 0x0d, 0x15, 0x01,
    0x5a, 0x98, 0x65, 0x34, 0x46, 0x16, 0x20, 0x45,
];
const RSA1024_D: &[u8] = &[
    0x9d, 0xa6, 0x41, 0xd1, 0x87, 0x80, 0x62, 0x96, 0x8c, 0xbb, 0x07, 0xa0, 0x71, 0x88, 0xe2,
    0x3c, 0x52, 0xcb, 0x6b, 0x91, 0x85, 0xde, 0xe3, 0x86, 0xe3, 0x88, 0x24, 0x61, 0xf7, 0x1f,
    0x3d, 0x24, 0x98, 0x5f, 0x9d, 0x04, 0x34, 0xa2, 0xb2, 0x64, 0x89, 0x37, 0xe3, 0x54, 0x1c,
    0x58, 0x94, 0x08, 0x00, 0xc9, 0xae, 0xe2, 0x02, 0x9e, 0xec, 0x4f, 0xcd, 0x70, 0xea, 0x9a,
    0x55, 0xe6, 0xb2, 0x8a, 0xad, 0x7c, 0x0f, 0xd5, 0x27, 0x70, 0x73, 0x72, 0x31, 0x1b, 0x75,
    0xc6, 0x21, 0x0e, 0x8b, 0x9d, 0x88, 0x99, 0x1a, 0xe6, 0xcd, 0xc0, 0x8c, 0x71, 0x63, 0xa4,
    0xa6, 0x6a, 0x0c, 0x95, 0x85, 0xc0, 0x35, 0xda, 0x5f, 0xc5, 0xc7, 0x65, 0x24, 0x42, 0xb7,
    0xf3, 0x1e, 0xb6, 0xbe, 0x95, 0x03, 0x00, 0xea, 0x09, 0x0a, 0xc3, 0xd8, 0x52, 0xa0, 0x9f,
    0x0d, 0x12, 0xb9, 0xbf, 0x3f, 0xf1, 0x72, 0xad,
];

/// Build an RSA `SubjectPublicKeyInfo` from raw big-endian n + e.
fn rsa_spki(n_be: &[u8], e: u64) -> Vec<u8> {
    // INTEGER encoding: prepend 0x00 if the high bit is set (positive).
    fn der_int(bytes: &[u8]) -> Vec<u8> {
        let mut b = bytes.to_vec();
        // Strip leading zero bytes except one that guards a high bit.
        while b.len() > 1 && b[0] == 0 && b[1] & 0x80 == 0 {
            b.remove(0);
        }
        if b[0] & 0x80 != 0 {
            let mut g = vec![0u8];
            g.extend_from_slice(&b);
            b = g;
        }
        tlv(TAG_INTEGER, &b)
    }
    let mut e_be = Vec::new();
    let mut v = e;
    while v > 0 {
        e_be.insert(0, (v & 0xFF) as u8);
        v >>= 8;
    }
    let rsa_pub = seq(&[der_int(n_be).as_slice(), der_int(&e_be).as_slice()].concat());
    let algid = {
        let mut inner = oid("1.2.840.113549.1.1.1");
        inner.extend_from_slice(&[TAG_NULL, 0x00]);
        seq(&inner)
    };
    let mut bs = vec![0u8];
    bs.extend_from_slice(&rsa_pub);
    let bit_string = tlv(0x03, &bs);
    seq(&[algid.as_slice(), bit_string.as_slice()].concat())
}

#[test]
fn sha384_512_digests_match_known_vectors() {
    // NIST FIPS 180-4 example: SHA-384/512 of "abc".
    let abc = b"abc";
    let d384 = super::sha2ext::sha384(abc);
    let d512 = super::sha2ext::sha512(abc);
    // First bytes of the well-known digests.
    assert_eq!(&d384[..4], &[0xcb, 0x00, 0x75, 0x3f]);
    assert_eq!(&d512[..4], &[0xdd, 0xaf, 0x35, 0xa1]);
}

#[test]
fn parse_spki_recovers_rsa_key() {
    let spki = rsa_spki(RSA512_N, 65537);
    match super::parse_spki(&spki).expect("parse rsa spki") {
        super::PublicKey::Rsa(k) => {
            assert_eq!(k.k, 64, "512-bit modulus => k = 64 bytes");
            assert_eq!(
                k.e.cmp(&super::BigUint::from_bytes_be(&[1, 0, 1])),
                std::cmp::Ordering::Equal
            );
        }
        _ => panic!("expected RSA key"),
    }
}

/// Produce an RSA PKCS#1 v1.5 signature for `message` under the test
/// key, using the production `BigUint::modpow` with the private `d`.
fn rsa_sign(message: &[u8], digest_alg: DigestAlg, n_be: &[u8], d_be: &[u8]) -> Vec<u8> {
    use super::BigUint;
    let n = BigUint::from_bytes_be(n_be);
    let d = BigUint::from_bytes_be(d_be);
    let k = (n.bit_length() + 7) / 8;
    let hash = super::raw_digest(digest_alg, message);
    let prefix = super::digest_info_prefix(digest_alg);
    let t_len = prefix.len() + hash.len();
    let ps_len = k - t_len - 3;
    let mut em = Vec::with_capacity(k);
    em.push(0x00);
    em.push(0x01);
    em.extend(std::iter::repeat(0xff).take(ps_len));
    em.push(0x00);
    em.extend_from_slice(prefix);
    em.extend_from_slice(&hash);
    let m = BigUint::from_bytes_be(&em);
    m.modpow(&d, &n).to_bytes_be_padded(k)
}

#[test]
fn rsa_verify_accepts_valid_signature() {
    use super::{DigestAlg, SigVerify};
    // Use the 1024-bit key so even SHA-512's EMSA block fits the modulus.
    let spki = rsa_spki(RSA1024_N, 65537);
    for alg in [
        DigestAlg::Sha1,
        DigestAlg::Sha256,
        DigestAlg::Sha384,
        DigestAlg::Sha512,
    ] {
        let msg = b"the quick brown fox";
        let sig = rsa_sign(msg, alg, RSA1024_N, RSA1024_D);
        assert_eq!(
            verify_signature_with_spki(&spki, sig_alg_oid_for(alg), msg, &sig),
            SigVerify::Ok,
            "valid {:?} signature must verify",
            alg
        );
        // Tampered message → Bad.
        assert_eq!(
            verify_signature_with_spki(&spki, sig_alg_oid_for(alg), b"tampered", &sig),
            SigVerify::Bad
        );
        // Tampered signature → Bad.
        let mut bad = sig.clone();
        bad[10] ^= 0xFF;
        assert_eq!(
            verify_signature_with_spki(&spki, sig_alg_oid_for(alg), msg, &bad),
            SigVerify::Bad
        );
    }
}

fn sig_alg_oid_for(alg: DigestAlg) -> &'static str {
    match alg {
        DigestAlg::Sha1 => "1.2.840.113549.1.1.5",
        DigestAlg::Sha256 => "1.2.840.113549.1.1.11",
        DigestAlg::Sha384 => "1.2.840.113549.1.1.12",
        DigestAlg::Sha512 => "1.2.840.113549.1.1.13",
    }
}

#[test]
fn rsa_verify_rejects_structural_garbage() {
    use super::{DigestAlg, SigVerify};
    let spki = rsa_spki(RSA512_N, 65537);
    let key = match super::parse_spki(&spki).unwrap() {
        super::PublicKey::Rsa(k) => k,
        _ => unreachable!(),
    };
    // Wrong signature length → `Unsupported`, NOT `Bad`.
    //
    // SunRsaSign raises `SignatureException("Signature length not
    // correct")` here, *before* any RSA operation runs — so no
    // verification decision was reached. Reporting it as `Bad` would
    // claim we compared this signature against the key and found it
    // wanting, which we did not. Both outcomes refuse; only one is
    // honest about why. (A genuine mismatch always has the correct
    // length, so this arm can never swallow a real negative — see the
    // `s == n` case below, which does stay `Bad`.)
    assert_eq!(
        super::rsa_pkcs1v15_verify(&key, DigestAlg::Sha256, b"msg", &[0u8; 10]),
        SigVerify::Unsupported,
        "a malformed signature encoding is 'not checked', not 'checked and failed'"
    );
    // s == n (>= n): correct length, so the RSA operation really does
    // run and really does reject → the preserved negative, `Bad`.
    let n_be = key.n.to_bytes_be_padded(key.k);
    assert_eq!(
        super::rsa_pkcs1v15_verify(&key, DigestAlg::Sha256, b"msg", &n_be),
        SigVerify::Bad
    );
}

#[test]
fn end_to_end_real_rsa_signer_block_and_chain() {
    use super::*;
    // Build a real RSA-signed SignerInfo over a `.SF`, plus a leaf cert
    // whose SPKI is the RSA test key and whose chain roots in a trust
    // anchor.  This exercises BOTH the SignerInfo pubkey check and the
    // chain link RSA verify on the production path (no permissive mode).

    // --- Leaf cert: subject = "RsaLeaf", issuer = "RsaRoot", RSA SPKI,
    //     self-... no: signed by root.  We sign the leaf TBS with the
    //     same test key for simplicity (root SPKI == same key).
    let leaf_spki = rsa_spki(RSA512_N, 65537);
    let subject_dn = x509_name("RsaLeaf");
    let issuer_dn = x509_name("RsaRoot");
    let root_dn = x509_name("RsaRoot");
    // Validity window covering "now" (2020..2099).
    let validity = seq(&[
        tlv(0x17, b"200101000000Z").as_slice(),
        tlv(0x18, b"20990101000000Z").as_slice(),
    ]
    .concat());
    let build_cert = |subj: &[u8], iss: &[u8]| -> Vec<u8> {
        let tbs = seq(&[
            ctx_imp(0, &integer(2)).as_slice(),
            integer(1).as_slice(),
            algorithm_identifier("1.2.840.113549.1.1.11").as_slice(),
            iss,
            validity.as_slice(),
            subj,
            leaf_spki.as_slice(),
        ]
        .concat());
        // Sign the TBS with the RSA key (sha256WithRSA).
        let sig = rsa_sign(&tbs, DigestAlg::Sha256, RSA512_N, RSA512_D);
        let mut bs = vec![0u8];
        bs.extend_from_slice(&sig);
        seq(&[
            tbs.as_slice(),
            algorithm_identifier("1.2.840.113549.1.1.11").as_slice(),
            tlv(0x03, &bs).as_slice(),
        ]
        .concat())
    };
    let leaf_der = build_cert(&subject_dn, &issuer_dn);
    let root_der = build_cert(&root_dn, &root_dn); // self-signed root

    // --- SignerInfo / SignedData over a `.SF`.
    let sf = b"Signature-Version: 1.0\r\nSHA-256-Digest-Manifest: zzz=\r\n\r\n";
    let dig = sha256::digest(sf).to_vec();
    let attr_ct = seq(&[
        oid(OID_CONTENT_TYPE).as_slice(),
        set(&oid(OID_DATA)).as_slice(),
    ]
    .concat());
    let attr_md = seq(&[
        oid(OID_MESSAGE_DIGEST).as_slice(),
        set(&octet(&dig)).as_slice(),
    ]
    .concat());
    let attrs_inner = [attr_ct.as_slice(), attr_md.as_slice()].concat();
    // SignedAttributes signed form = explicit SET.
    let signed_attrs_der = set(&attrs_inner);
    let si_sig = rsa_sign(&signed_attrs_der, DigestAlg::Sha256, RSA512_N, RSA512_D);
    let auth_attrs = ctx_imp(0, &attrs_inner);

    let signer_info = seq(&[
        integer(1).as_slice(),
        issuer_and_serial("RsaRoot", 1).as_slice(),
        algorithm_identifier(OID_SHA256).as_slice(),
        auth_attrs.as_slice(),
        algorithm_identifier("1.2.840.113549.1.1.11").as_slice(),
        octet(&si_sig).as_slice(),
    ]
    .concat());

    // certificates [0] IMPLICIT: leaf first, then root.
    let mut certs_concat = leaf_der.clone();
    certs_concat.extend_from_slice(&root_der);
    let certs = ctx_imp(0, &certs_concat);
    let encap = seq(&oid(OID_DATA));
    let digest_algs = set(&algorithm_identifier(OID_SHA256));
    let signed_data = seq(&[
        integer(1).as_slice(),
        digest_algs.as_slice(),
        encap.as_slice(),
        certs.as_slice(),
        set(&signer_info).as_slice(),
    ]
    .concat());
    let block = seq(&[
        oid(OID_SIGNED_DATA).as_slice(),
        ctx_imp(0, &signed_data).as_slice(),
    ]
    .concat());

    // Trust store with the root anchor.
    let mut ts = TrustStore::empty();
    assert!(ts.add_anchor_der(root_der.clone()));

    let vs = verify_signer_block(&block, sf, &ts)
        .expect("real RSA signer block + chain must verify end-to-end");
    assert_eq!(vs.digest_alg, DigestAlg::Sha256);
    assert_eq!(vs.chain.len(), 2);

    // Negative: tamper the .SF → reject.
    let bad_sf = b"Signature-Version: 1.0\r\nEvil: 1\r\n\r\n";
    assert!(verify_signer_block(&block, bad_sf, &ts).is_none());

    // Negative: empty trust store → no anchor → reject.
    assert!(verify_signer_block(&block, sf, &TrustStore::empty()).is_none());
}

#[test]
fn ecdsa_malformed_ec_key_is_bad_not_accepted() {
    use super::{verify_signature_with_spki, SigVerify};
    // FEAT(jar-signer): an EC SPKI carrying an all-zero (off-curve)
    // "point" must NEVER verify.  The point fails to decode, so no
    // ECDSA verification runs at all → `Unsupported` (fail-closed,
    // never `Ok`; and deliberately not `Bad`, which would claim a
    // verdict we never reached).
    let ec_algid = {
        let mut inner = oid("1.2.840.10045.2.1"); // id-ecPublicKey
        inner.extend_from_slice(&oid("1.2.840.10045.3.1.7")); // prime256v1
        seq(&inner)
    };
    let bs = vec![0u8; 65]; // uncompressed point placeholder (off-curve)
    let mut bit = vec![0u8];
    bit.extend_from_slice(&bs);
    let ec_spki = seq(&[ec_algid.as_slice(), tlv(0x03, &bit).as_slice()].concat());
    let r = verify_signature_with_spki(
        &ec_spki,
        "1.2.840.10045.4.3.2", // ecdsa-with-SHA256
        b"message",
        &[0u8; 64],
    );
    assert_ne!(r, SigVerify::Ok, "off-curve EC key must never verify");
}

#[test]
fn ecdsa_p256_round_trip_real_signature() {
    use super::{verify_signature_with_spki, DigestAlg, SigVerify};
    use p256::ecdsa::signature::hazmat::PrehashSigner;
    use p256::ecdsa::SigningKey;
    use spki::EncodePublicKey;

    // Fixed 32-byte scalar → deterministic key (no RNG needed).
    let sk = SigningKey::from_slice(&[0x11u8; 32]).expect("p256 signing key");
    let vk = sk.verifying_key();
    // `EncodePublicKey` on the elliptic-curve `PublicKey` is gated on
    // `pkcs8` (which we enable); the ecdsa `VerifyingKey` wrapper's
    // own impl is gated on `pem` (which we don't), so convert.
    let pk = p256::PublicKey::from(vk);
    let spki = pk
        .to_public_key_der()
        .expect("spki encode")
        .as_bytes()
        .to_vec();

    let msg = b"jarsigner-signed-attributes-bytes";
    // ECDSA-with-SHA256: sign the SHA-256 prehash; emit DER (r,s).
    let prehash = super::raw_digest(DigestAlg::Sha256, msg);
    let der_sig: ecdsa::der::Signature<p256::NistP256> =
        sk.sign_prehash(&prehash).expect("p256 sign");
    let sig_der = der_sig.as_bytes().to_vec();

    assert_eq!(
        verify_signature_with_spki(&spki, "1.2.840.10045.4.3.2", msg, &sig_der),
        SigVerify::Ok,
        "valid P-256 ECDSA signature must verify"
    );
    // Tampered message → Bad.
    assert_eq!(
        verify_signature_with_spki(&spki, "1.2.840.10045.4.3.2", b"evil", &sig_der),
        SigVerify::Bad
    );
    // The SPKI must classify as EcP256.
    assert!(matches!(
        super::parse_spki(&spki).unwrap(),
        super::PublicKey::EcP256(_)
    ));
}

#[test]
fn ecdsa_p384_round_trip_real_signature() {
    use super::{verify_signature_with_spki, DigestAlg, SigVerify};
    use p384::ecdsa::signature::hazmat::PrehashSigner;
    use p384::ecdsa::SigningKey;
    use spki::EncodePublicKey;

    let sk = SigningKey::from_slice(&[0x22u8; 48]).expect("p384 signing key");
    let vk = sk.verifying_key();
    let pk = p384::PublicKey::from(vk);
    let spki = pk
        .to_public_key_der()
        .expect("spki encode")
        .as_bytes()
        .to_vec();

    let msg = b"another signed-attrs blob";
    let prehash = super::raw_digest(DigestAlg::Sha384, msg);
    let der_sig: ecdsa::der::Signature<p384::NistP384> =
        sk.sign_prehash(&prehash).expect("p384 sign");
    let sig_der = der_sig.as_bytes().to_vec();

    assert_eq!(
        verify_signature_with_spki(&spki, "1.2.840.10045.4.3.3", msg, &sig_der),
        SigVerify::Ok,
        "valid P-384 ECDSA signature must verify"
    );
    assert!(matches!(
        super::parse_spki(&spki).unwrap(),
        super::PublicKey::EcP384(_)
    ));
}

/// Parse a run of hex into big-endian bytes (test helper for DSA params).
fn hexbytes(h: &str) -> Vec<u8> {
    let cleaned: Vec<u8> = h.bytes().filter(|b| b.is_ascii_hexdigit()).collect();
    cleaned
        .chunks(2)
        .map(|c| {
            let hi = (c[0] as char).to_digit(16).unwrap();
            let lo = (c[1] as char).to_digit(16).unwrap();
            (hi * 16 + lo) as u8
        })
        .collect()
}

#[test]
fn dsa_round_trip_real_signature() {
    use super::{verify_signature_with_spki, DigestAlg, SigVerify};
    use dsa::pkcs8::EncodePublicKey;
    use dsa::signature::hazmat::PrehashSigner;
    use dsa::{BoxedUint, Components, SigningKey, VerifyingKey};

    // RFC 6979 A.2.1 1024-bit DSA test key (same vector the `dsa`
    // crate ships in its own tests — exercises the real verify path).
    let p = BoxedUint::from_be_slice_vartime(&hexbytes(
        "86F5CA03DCFEB225063FF830A0C769B9DD9D6153AD91D7CE27F787C43278B447\
         E6533B86B18BED6E8A48B784A14C252C5BE0DBF60B86D6385BD2F12FB763ED88\
         73ABFD3F5BA2E0A8C0A59082EAC056935E529DAF7C610467899C77ADEDFC846C\
         881870B7B19B2B58F9BE0521A17002E3BDD6B86685EE90B3D9A1B02B782B1779",
    ));
    let q =
        BoxedUint::from_be_slice_vartime(&hexbytes("996F967F6C8E388D9E28D01E205FBA957A5698B1"));
    let g = BoxedUint::from_be_slice_vartime(&hexbytes(
        "07B0F92546150B62514BB771E2A0C0CE387F03BDA6C56B505209FF25FD3C133D\
         89BBCD97E904E09114D9A7DEFDEADFC9078EA544D2E401AEECC40BB9FBBF78FD\
         87995A10A1C27CB7789B594BA7EFB5C4326A9FE59A070E136DB77175464ADCA4\
         17BE5DCE2F40D10A46A3A3943F26AB7FD9C0398FF8C76EE0A56826A8A88F1DBD",
    ));
    let x =
        BoxedUint::from_be_slice_vartime(&hexbytes("411602CB19A6CCC34494D79D98EF1E7ED5AF25F7"));
    let y = BoxedUint::from_be_slice_vartime(&hexbytes(
        "5DF5E01DED31D0297E274E1691C192FE5868FEF9E19A84776454B100CF16F653\
         92195A38B90523E2542EE61871C0440CB87C322FC4B4D2EC5E1E7EC766E1BE8D\
         4CE935437DC11C3C8FD426338933EBFE739CB3465F4D3668C5E473508253B1E6\
         82F65CBDC4FAE93C2EA212390E54905A86E2223170B44EAA7DA5DD9FFCFB7F3B",
    ));
    let components = Components::from_components(p, q, g).expect("dsa components");
    let vk = VerifyingKey::from_components(components, y).expect("dsa verifying key");
    let sk = SigningKey::from_components(vk.clone(), x).expect("dsa signing key");

    let spki = vk
        .to_public_key_der()
        .expect("dsa spki")
        .as_bytes()
        .to_vec();
    assert!(matches!(
        super::parse_spki(&spki).unwrap(),
        super::PublicKey::Dsa(_)
    ));

    let msg = b"dsa-signed .SF attributes";
    let prehash = super::raw_digest(DigestAlg::Sha1, msg);
    let sig: dsa::Signature = sk.sign_prehash(&prehash).expect("dsa sign");
    let sig_der = {
        use dsa::signature::SignatureEncoding;
        sig.to_bytes().to_vec()
    };

    // id-dsa-with-sha1 (1.2.840.10040.4.3).
    assert_eq!(
        verify_signature_with_spki(&spki, "1.2.840.10040.4.3", msg, &sig_der),
        SigVerify::Ok,
        "valid DSA signature must verify"
    );
    // Tampered message → Bad.
    assert_eq!(
        verify_signature_with_spki(&spki, "1.2.840.10040.4.3", b"evil", &sig_der),
        SigVerify::Bad
    );
}

// ---------------------------------------------------------------------
// FEAT(jar-signer) — RFC 5280 extension processing tests.
//
// These build *real* RSA-signed certs (reusing the RSA-512 test key)
// carrying X.509v3 extensions, then drive them through `verify_chain`
// on the production path (no stub-sig short-circuit).
// ---------------------------------------------------------------------

/// Build a real RSA (sha256WithRSA) cert with the given v3 `extensions`
/// DER (the inner SEQUENCE OF Extension content; pass `&[]` for none).
/// Signed by the RSA-512 test key; SPKI is that same key.
fn build_rsa_cert_with_exts(subject_cn: &str, issuer_cn: &str, extensions: &[u8]) -> Vec<u8> {
    let spki = rsa_spki(RSA512_N, 65537);
    let subject_dn = x509_name(subject_cn);
    let issuer_dn = x509_name(issuer_cn);
    let validity = seq(&[
        tlv(0x17, b"200101000000Z").as_slice(),
        tlv(0x18, b"20990101000000Z").as_slice(),
    ]
    .concat());
    let mut tbs_body = [
        ctx_imp(0, &integer(2)).as_slice(),
        integer(1).as_slice(),
        algorithm_identifier("1.2.840.113549.1.1.11").as_slice(),
        issuer_dn.as_slice(),
        validity.as_slice(),
        subject_dn.as_slice(),
        spki.as_slice(),
    ]
    .concat();
    if !extensions.is_empty() {
        // extensions [3] EXPLICIT SEQUENCE OF Extension.
        let ext_seq = seq(extensions);
        let ext_explicit = tlv(0xA3, &ext_seq);
        tbs_body.extend_from_slice(&ext_explicit);
    }
    let tbs = seq(&tbs_body);
    let sig = rsa_sign(&tbs, DigestAlg::Sha256, RSA512_N, RSA512_D);
    let mut bs = vec![0u8];
    bs.extend_from_slice(&sig);
    seq(&[
        tbs.as_slice(),
        algorithm_identifier("1.2.840.113549.1.1.11").as_slice(),
        tlv(0x03, &bs).as_slice(),
    ]
    .concat())
}

/// Build an Extension ::= SEQUENCE { extnID OID, critical BOOL?, extnValue OCTET STRING }.
fn extension(oid_dotted: &str, critical: bool, value_der: &[u8]) -> Vec<u8> {
    let mut inner = oid(oid_dotted);
    if critical {
        inner.extend_from_slice(&[0x01, 0x01, 0xFF]); // BOOLEAN TRUE
    }
    inner.extend_from_slice(&octet(value_der));
    seq(&inner)
}

/// BasicConstraints ::= SEQUENCE { cA BOOLEAN DEFAULT FALSE, ... }.
/// `cA=TRUE` is encoded explicitly; `cA=FALSE` is the canonical empty
/// SEQUENCE (the DEFAULT is omitted), which decodes to `ca = false`.
fn basic_constraints(ca: bool) -> Vec<u8> {
    if ca {
        seq(&[0x01, 0x01, 0xFF])
    } else {
        seq(&[])
    }
}

/// KeyUsage ::= BIT STRING.  `first_byte` is the big-endian usage byte
/// (bit 0 = digitalSignature = 0x80; bit 5 = keyCertSign = 0x04).
/// Encoded canonically (DER) with the unused-bits count set to the
/// number of trailing zero bits, as `der`'s BitString decoder expects.
fn key_usage(first_byte: u8) -> Vec<u8> {
    let unused = if first_byte == 0 {
        0
    } else {
        first_byte.trailing_zeros() as u8
    };
    tlv(0x03, &[unused, first_byte])
}

#[test]
fn rfc5280_leaf_keyusage_without_digitalsignature_rejected() {
    use super::*;
    // Leaf with KeyUsage = keyCertSign only (no digitalSignature) →
    // must be rejected for code-signing use.
    let ku = extension("2.5.29.15", true, &key_usage(0x04)); // keyCertSign
    let leaf_der = build_rsa_cert_with_exts("Leaf", "Root", &ku);
    let root_der = build_rsa_cert_with_exts("Root", "Root", &[]);

    let leaf = X509Cert::parse(&leaf_der).expect("parse leaf");
    let mut ts = TrustStore::empty();
    ts.add_anchor_der(root_der);
    let err = verify_chain(&leaf, &[], &ts).expect_err("leaf KU must reject");
    assert_eq!(err, TrustError::KeyUsageViolation);
}

#[test]
fn rfc5280_leaf_with_digitalsignature_and_codesigning_eku_accepted() {
    use super::*;
    // Leaf with digitalSignature KU + codeSigning EKU, chaining to a
    // root that is a proper CA → accepted.
    let ku = extension("2.5.29.15", true, &key_usage(0x80)); // digitalSignature
    let eku_val = seq(&oid("1.3.6.1.5.5.7.3.3")); // id-kp-codeSigning
    let eku = extension("2.5.29.37", false, &eku_val);
    let leaf_exts = [ku.as_slice(), eku.as_slice()].concat();
    let leaf_der = build_rsa_cert_with_exts("Leaf", "Root", &leaf_exts);

    // Root: CA + keyCertSign.
    let root_bc = extension("2.5.29.19", true, &basic_constraints(true));
    let root_ku = extension("2.5.29.15", true, &key_usage(0x04)); // keyCertSign
    let root_exts = [root_bc.as_slice(), root_ku.as_slice()].concat();
    let root_der = build_rsa_cert_with_exts("Root", "Root", &root_exts);

    let leaf = X509Cert::parse(&leaf_der).expect("parse leaf");
    let mut ts = TrustStore::empty();
    ts.add_anchor_der(root_der);
    verify_chain(&leaf, &[], &ts).expect("compliant code-signing chain must verify");
}

#[test]
fn rfc5280_intermediate_with_ca_false_rejected() {
    use super::*;
    // Intermediate carries BasicConstraints cA=FALSE but is used to
    // certify the leaf → BasicConstraintsViolation.
    let leaf_der = build_rsa_cert_with_exts("Leaf", "Inter", &[]);
    let int_bc = extension("2.5.29.19", true, &basic_constraints(false));
    let int_der = build_rsa_cert_with_exts("Inter", "Root", &int_bc);
    let root_der = build_rsa_cert_with_exts("Root", "Root", &[]);

    let leaf = X509Cert::parse(&leaf_der).expect("parse leaf");
    let int_cert = X509Cert::parse(&int_der).expect("parse int");
    let mut ts = TrustStore::empty();
    ts.add_anchor_der(root_der);
    let err = verify_chain(&leaf, std::slice::from_ref(&int_cert), &ts)
        .expect_err("cA=false intermediate must reject");
    assert_eq!(err, TrustError::BasicConstraintsViolation);
}

#[test]
fn rfc5280_unknown_critical_extension_rejected() {
    use super::*;
    // Leaf carries a bogus *critical* extension we do not process →
    // UnknownCriticalExtension (fail-closed, RFC 5280 §6.1.4(f)).
    let bogus = extension("1.2.3.4.5.6.7", true, &[0x01, 0x02]);
    let leaf_der = build_rsa_cert_with_exts("Leaf", "Root", &bogus);
    let root_der = build_rsa_cert_with_exts("Root", "Root", &[]);

    let leaf = X509Cert::parse(&leaf_der).expect("parse leaf");
    let mut ts = TrustStore::empty();
    ts.add_anchor_der(root_der);
    let err = verify_chain(&leaf, &[], &ts).expect_err("unknown critical ext must reject");
    assert_eq!(err, TrustError::UnknownCriticalExtension);
}

// ---------------------------------------------------------------------
// Task #40 — JKS trust-store parsing tests.
// ---------------------------------------------------------------------

/// Hand-assemble a minimal JKS file (version 2) with a single
/// TrustedCertEntry carrying `cert_der`, MAC-sealed under `password`.
fn build_jks(cert_der: &[u8], password: &[u8]) -> Vec<u8> {
    fn u16(v: u16, o: &mut Vec<u8>) {
        o.extend_from_slice(&v.to_be_bytes());
    }
    fn u32(v: u32, o: &mut Vec<u8>) {
        o.extend_from_slice(&v.to_be_bytes());
    }
    fn u64(v: u64, o: &mut Vec<u8>) {
        o.extend_from_slice(&v.to_be_bytes());
    }
    let mut body = Vec::new();
    u32(super::JKS_MAGIC, &mut body);
    u32(2, &mut body); // version
    u32(1, &mut body); // entry_count
    u32(2, &mut body); // tag = TrustedCertEntry
    let alias = b"testanchor";
    u16(alias.len() as u16, &mut body);
    body.extend_from_slice(alias);
    u64(0, &mut body); // creation_date_ms
    let ctype = b"X.509";
    u16(ctype.len() as u16, &mut body);
    body.extend_from_slice(ctype);
    u32(cert_der.len() as u32, &mut body);
    body.extend_from_slice(cert_der);
    // Append MAC.
    let mac = super::jks_password_mac(password, &body);
    let mut out = body;
    out.extend_from_slice(&mac);
    out
}

#[test]
fn jks_round_trip_extracts_trusted_cert() {
    let root_subject_dn = x509_name("JksRoot");
    let cert = build_x509_cert("JksRoot", "JksRoot", &root_subject_dn);
    let pw = b"changeit";
    let jks = build_jks(&cert, pw);

    let ders = super::parse_jks_trusted_certs(&jks, pw).expect("jks parse");
    assert_eq!(ders.len(), 1);
    assert_eq!(ders[0], cert);

    // Loading it as a trust store anchor must work.
    let mut ts = TrustStore::empty();
    let n = ts.extend_from_anchors(ders);
    assert_eq!(n, 1);
    assert_eq!(ts.anchor_count(), 1);
}

#[test]
fn jks_wrong_password_rejected() {
    let dn = x509_name("JksRoot");
    let cert = build_x509_cert("JksRoot", "JksRoot", &dn);
    let jks = build_jks(&cert, b"changeit");
    assert!(super::parse_jks_trusted_certs(&jks, b"wrongpw").is_err());
}

#[test]
fn jks_tampered_body_rejected() {
    let dn = x509_name("JksRoot");
    let cert = build_x509_cert("JksRoot", "JksRoot", &dn);
    let mut jks = build_jks(&cert, b"changeit");
    // Flip a byte in the body (not the trailing MAC).
    jks[12] ^= 0xFF;
    assert!(super::parse_jks_trusted_certs(&jks, b"changeit").is_err());
}

#[test]
fn validity_dates_reject_expired_cert() {
    // notAfter in the past must fail cert_dates_ok.
    let der = {
        let subject_dn = x509_name("ExpiredLeaf");
        let issuer_dn = x509_name("ExpiredLeaf");
        let version = ctx_imp(0, &integer(2));
        let serial = integer(1);
        let sig_alg = algorithm_identifier(OID_STUB_SIG);
        // notBefore 2000, notAfter 2001 — both in the past.
        let validity = seq(&[
            tlv(0x17, b"000101000000Z").as_slice(),
            tlv(0x17, b"010101000000Z").as_slice(),
        ]
        .concat());
        let spki = x509_spki(&subject_dn);
        let tbs = seq(&[
            version.as_slice(),
            serial.as_slice(),
            sig_alg.as_slice(),
            issuer_dn.as_slice(),
            validity.as_slice(),
            subject_dn.as_slice(),
            spki.as_slice(),
        ]
        .concat());
        let mut buf = tbs.clone();
        buf.extend_from_slice(&subject_dn);
        let sig = sha256::digest(&buf);
        let mut bs = vec![0u8];
        bs.extend_from_slice(&sig);
        seq(&[
            tbs.as_slice(),
            algorithm_identifier(OID_STUB_SIG).as_slice(),
            tlv(0x03, &bs).as_slice(),
        ]
        .concat())
    };
    let cert = X509Cert::parse(&der).expect("parse expired cert");
    assert!(
        !super::cert_dates_ok(&cert),
        "year-2001 notAfter must be expired"
    );
}

// -----------------------------------------------------------------
// V1 — per-entry manifest digest binding helpers.
// -----------------------------------------------------------------

/// Minimal standard base64 encoder for the manifest-digest tests.
fn b64enc(input: &[u8]) -> String {
    const ALPHA: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHA[((n >> 18) & 63) as usize] as char);
        out.push(ALPHA[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHA[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHA[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[test]
fn sf_binds_manifest_accepts_matching_digest() {
    let manifest = b"Manifest-Version: 1.0\r\n\r\nName: a/B.class\r\nSHA-256-Digest: xyz\r\n";
    let mdigest = b64enc(&raw_digest(DigestAlg::Sha256, manifest));
    let sf = format!("Signature-Version: 1.0\r\nSHA-256-Digest-Manifest: {mdigest}\r\n\r\n");
    assert!(verify_sf_binds_manifest(sf.as_bytes(), manifest));
}

#[test]
fn sf_binds_manifest_rejects_tampered_manifest() {
    let manifest = b"Manifest-Version: 1.0\r\n\r\nName: a/B.class\r\nSHA-256-Digest: xyz\r\n";
    let mdigest = b64enc(&raw_digest(DigestAlg::Sha256, manifest));
    let sf = format!("Signature-Version: 1.0\r\nSHA-256-Digest-Manifest: {mdigest}\r\n\r\n");
    let tampered = b"Manifest-Version: 1.0\r\n\r\nName: a/B.class\r\nSHA-256-Digest: zzz\r\n";
    assert!(!verify_sf_binds_manifest(sf.as_bytes(), tampered));
}

#[test]
fn sf_binds_manifest_fails_closed_without_digest_manifest() {
    // A `.SF` that only commits to the main attributes (not the whole
    // manifest) must NOT be accepted as binding the manifest.
    let manifest = b"Manifest-Version: 1.0\r\n\r\nName: a/B.class\r\nSHA-256-Digest: xyz\r\n";
    let sf = b"Signature-Version: 1.0\r\nSHA-256-Digest-Manifest-Main-Attributes: AAAA\r\n\r\n";
    assert!(!verify_sf_binds_manifest(sf, manifest));
}

#[test]
fn parse_manifest_entry_digests_extracts_each_section() {
    let body = b"hello world contents";
    let dig = b64enc(&raw_digest(DigestAlg::Sha256, body));
    let manifest = format!(
        "Manifest-Version: 1.0\r\n\r\n\
         Name: pkg/Foo.class\r\nSHA-256-Digest: {dig}\r\n\r\n\
         Name: pkg/dir/\r\n\r\n"
    );
    let parsed = parse_manifest_entry_digests(manifest.as_bytes());
    // Directory section (no digest) is dropped; only Foo.class remains.
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].name, "pkg/Foo.class");
    assert_eq!(parsed[0].alg, DigestAlg::Sha256);
    assert!(digest_matches(parsed[0].alg, body, &parsed[0].expected));
    // A different body must NOT match the recorded digest.
    assert!(!digest_matches(
        parsed[0].alg,
        b"tampered",
        &parsed[0].expected
    ));
}

#[test]
fn parse_manifest_entry_digests_picks_strongest_alg() {
    let body = b"abc";
    let d1 = b64enc(&raw_digest(DigestAlg::Sha1, body));
    let d256 = b64enc(&raw_digest(DigestAlg::Sha256, body));
    let manifest = format!(
        "Manifest-Version: 1.0\r\n\r\n\
         Name: X.class\r\nSHA1-Digest: {d1}\r\nSHA-256-Digest: {d256}\r\n\r\n"
    );
    let parsed = parse_manifest_entry_digests(manifest.as_bytes());
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].alg, DigestAlg::Sha256, "strongest digest wins");
    assert!(digest_matches(parsed[0].alg, body, &parsed[0].expected));
}

// ---------------------------------------------------------------------
// TRUST BOUNDARY — the three-valued result, and what "verified" covers.
//
// These pin the contract written up in
// `docs/security/signed-jar-trust.md`:
//
//   * "we checked and it failed" (`SigVerify::Bad`) and "we never
//     checked" (`SigVerify::Unsupported`) must stay distinguishable, and
//     both must refuse;
//   * an integrity check must never be readable as a trust check;
//   * a trust-shaped API must refuse unless every step it claims
//     actually ran — including for certificates it merely carried.
//
// The fixtures use real RSA (the 512-bit test key) so they drive the
// production verification path, not the `OID_STUB_SIG` test backdoor.
// ---------------------------------------------------------------------

/// A validity window that contains "now" — `notBefore` 2020 (UTCTime),
/// `notAfter` 2099 (GeneralizedTime), matching the encodings real CAs
/// emit either side of the 2050 boundary.
fn tb_validity_now() -> Vec<u8> {
    seq(&[
        tlv(0x17, b"200101000000Z").as_slice(),
        tlv(0x18, b"20990101000000Z").as_slice(),
    ]
    .concat())
}

/// A real `sha256WithRSAEncryption` certificate signed by (and carrying
/// the SPKI of) the RSA-512 test key, with a caller-chosen `validity`
/// SEQUENCE. Self-signed when `subject_cn == issuer_cn`.
fn tb_cert(subject_cn: &str, issuer_cn: &str, validity: &[u8]) -> Vec<u8> {
    let spki = rsa_spki(RSA512_N, 65537);
    let subject_dn = x509_name(subject_cn);
    let issuer_dn = x509_name(issuer_cn);
    let tbs = seq(&[
        ctx_imp(0, &integer(2)).as_slice(),
        integer(1).as_slice(),
        algorithm_identifier("1.2.840.113549.1.1.11").as_slice(),
        issuer_dn.as_slice(),
        validity,
        subject_dn.as_slice(),
        spki.as_slice(),
    ]
    .concat());
    let sig = rsa_sign(&tbs, DigestAlg::Sha256, RSA512_N, RSA512_D);
    let mut bs = vec![0u8];
    bs.extend_from_slice(&sig);
    seq(&[
        tbs.as_slice(),
        algorithm_identifier("1.2.840.113549.1.1.11").as_slice(),
        tlv(0x03, &bs).as_slice(),
    ]
    .concat())
}

/// A real RSA-signed CMS signer block over `sf`. The `certificates` set
/// is the leaf followed by `extra_certs` verbatim — the hook for the
/// "attacker appends a cert" case. `signer_sig_alg_oid` names the
/// SignerInfo `signatureAlgorithm`. Returns `(block, leaf_der, root_der)`.
fn tb_signer_block_with_alg(
    sf: &[u8],
    extra_certs: &[Vec<u8>],
    signer_sig_alg_oid: &str,
) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let validity = tb_validity_now();
    let leaf_der = tb_cert("TB-Leaf", "TB-Root", &validity);
    let root_der = tb_cert("TB-Root", "TB-Root", &validity);

    let dig = sha256::digest(sf).to_vec();
    let attr_ct = seq(&[
        oid(OID_CONTENT_TYPE).as_slice(),
        set(&oid(OID_DATA)).as_slice(),
    ]
    .concat());
    let attr_md = seq(&[
        oid(OID_MESSAGE_DIGEST).as_slice(),
        set(&octet(&dig)).as_slice(),
    ]
    .concat());
    let attrs_inner = [attr_ct.as_slice(), attr_md.as_slice()].concat();
    // RFC 5652 §5.4: the signature covers the explicit SET OF form.
    let signed_attrs_der = set(&attrs_inner);
    let si_sig = rsa_sign(&signed_attrs_der, DigestAlg::Sha256, RSA512_N, RSA512_D);
    let auth_attrs = ctx_imp(0, &attrs_inner);

    let signer_info = seq(&[
        integer(1).as_slice(),
        issuer_and_serial("TB-Root", 1).as_slice(),
        algorithm_identifier(OID_SHA256).as_slice(),
        auth_attrs.as_slice(),
        algorithm_identifier(signer_sig_alg_oid).as_slice(),
        octet(&si_sig).as_slice(),
    ]
    .concat());

    let mut certs_concat = leaf_der.clone();
    for c in extra_certs {
        certs_concat.extend_from_slice(c);
    }
    let signed_data = seq(&[
        integer(1).as_slice(),
        set(&algorithm_identifier(OID_SHA256)).as_slice(),
        seq(&oid(OID_DATA)).as_slice(),
        ctx_imp(0, &certs_concat).as_slice(),
        set(&signer_info).as_slice(),
    ]
    .concat());
    let block = seq(&[
        oid(OID_SIGNED_DATA).as_slice(),
        ctx_imp(0, &signed_data).as_slice(),
    ]
    .concat());
    (block, leaf_der, root_der)
}

/// [`tb_signer_block_with_alg`] with the ordinary `sha256WithRSA` OID.
fn tb_signer_block(sf: &[u8], extra_certs: &[Vec<u8>]) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    tb_signer_block_with_alg(sf, extra_certs, "1.2.840.113549.1.1.11")
}

#[test]
fn tb_unsupported_signature_algorithm_is_unsupported_not_bad() {
    use super::{verify_signature_with_spki, SigVerify};
    let spki = rsa_spki(RSA1024_N, 65537);
    let msg = b"payload";
    let sig = rsa_sign(msg, DigestAlg::Sha256, RSA1024_N, RSA1024_D);

    // RSASSA-PSS (1.2.840.113549.1.1.10) is a *different padding scheme*
    // and is deliberately absent from `sig_alg_digest` — verifying it
    // with the PKCS#1 v1.5 verifier would be the wrong operation.
    let r = verify_signature_with_spki(&spki, "1.2.840.113549.1.1.10", msg, &sig);
    assert_eq!(
        r,
        SigVerify::Unsupported,
        "an algorithm we do not implement must report that we did not check"
    );
    assert_ne!(
        r,
        SigVerify::Bad,
        "reporting 'unsupported' as 'the signature is bad' claims a verdict \
         that was never reached — that is the ambiguity this enum removes"
    );

    // Recognised OID, wrong key family: an ECDSA algorithm against an
    // RSA key. There is no verification to perform.
    assert_eq!(
        verify_signature_with_spki(&spki, "1.2.840.10045.4.3.2", msg, &sig),
        SigVerify::Unsupported
    );
    // An OID we have never heard of.
    assert_eq!(
        verify_signature_with_spki(&spki, "1.1.1.1", msg, &sig),
        SigVerify::Unsupported
    );
    // An SPKI that will not parse is likewise "not checked", not "failed".
    assert_eq!(
        verify_signature_with_spki(b"\x30\x03not-a-key", "1.2.840.113549.1.1.11", msg, &sig),
        SigVerify::Unsupported
    );
}

#[test]
fn tb_unsupported_algorithm_is_rejected_at_the_trust_api() {
    // The distinction has to survive all the way out, and `Unsupported`
    // must refuse just as hard as `Bad` does.
    let sf = b"Signature-Version: 1.0\r\nSHA-256-Digest-Manifest: AAAA\r\n\r\n";
    let (block, _leaf, root) = tb_signer_block_with_alg(sf, &[], "1.2.840.113549.1.1.10");

    let mut ts = TrustStore::empty();
    assert!(ts.add_anchor_der(root));
    assert!(
        verify_signer_block(&block, sf, &ts).is_none(),
        "a signer block whose algorithm we cannot verify must never be accepted"
    );

    // ...and the refusal says so, rather than accusing the JAR of
    // carrying a bad signature.
    let err = super::parse_signed_data(&block, sf, true)
        .expect_err("unsupported signer algorithm must refuse");
    assert!(
        err.contains("could not be verified"),
        "expected an 'unverifiable' diagnosis, got: {err}"
    );
    assert!(
        !err.contains("does not verify"),
        "an unsupported algorithm must not be reported as a failed verification: {err}"
    );
}

#[test]
fn tb_genuine_signature_mismatch_is_bad_not_unsupported() {
    use super::{verify_signature_with_spki, SigVerify};
    // PRESERVED NEGATIVE: when a verifier really runs and says no, that
    // is a security decision and must stay one. Turning this into
    // `Unsupported` would be just as wrong in the other direction.
    let spki = rsa_spki(RSA1024_N, 65537);
    let msg = b"the real message";
    let sig = rsa_sign(msg, DigestAlg::Sha256, RSA1024_N, RSA1024_D);

    let other = b"a different message";
    assert_eq!(
        verify_signature_with_spki(&spki, "1.2.840.113549.1.1.11", other, &sig),
        SigVerify::Bad,
        "a completed verification that fails is a genuine negative"
    );

    // Corrupt signature bits keep the correct length, so the encoding
    // guard cannot fire — this is exactly what a forgery looks like.
    let mut corrupt = sig.clone();
    corrupt[5] ^= 0xFF;
    assert_eq!(
        verify_signature_with_spki(&spki, "1.2.840.113549.1.1.11", msg, &corrupt),
        SigVerify::Bad
    );

    // And the same input still verifies untouched, so the assertions
    // above are not passing for an unrelated reason.
    assert_eq!(
        verify_signature_with_spki(&spki, "1.2.840.113549.1.1.11", msg, &sig),
        SigVerify::Ok
    );
}

#[test]
fn tb_unsupported_and_bad_stay_distinct_trust_errors() {
    let validity = tb_validity_now();
    let parent_der = tb_cert("TB-Root", "TB-Root", &validity);
    let parent = X509Cert::parse(&parent_der).expect("parent parses");

    // Same key, tampered signature bytes → a verifier ran and said no.
    let mut bad_der = tb_cert("TB-Leaf", "TB-Root", &validity);
    let last = bad_der.len() - 1;
    bad_der[last] ^= 0xFF;
    let bad = X509Cert::parse(&bad_der).expect("tampered cert still parses");
    assert_eq!(
        bad.link_signature_ok(&parent)
            .expect_err("tampered link must reject"),
        TrustError::BadSignature
    );

    // A parent whose SPKI names a key type we do not carry → nothing
    // was verified → `NotImplemented`, which a caller matching on
    // `TrustError` cannot confuse with `BadSignature`.
    let stub_parent_der = build_x509_cert("TB-Root", "TB-Root", &x509_name("TB-Root"));
    let stub_parent = X509Cert::parse(&stub_parent_der).expect("stub parent parses");
    let child_der = tb_cert("TB-Leaf", "TB-Root", &validity);
    let child = X509Cert::parse(&child_der).expect("child parses");
    assert_eq!(
        child
            .link_signature_ok(&stub_parent)
            .expect_err("unverifiable link must reject"),
        TrustError::NotImplemented
    );
}

#[test]
fn tb_self_consistent_signature_without_an_anchor_is_not_trusted() {
    let sf = b"Signature-Version: 1.0\r\nSHA-256-Digest-Manifest: AAAA\r\n\r\n";
    let (block, leaf_der, root_der) = tb_signer_block(sf, &[]);

    // The block IS internally consistent — with its root as an anchor it
    // verifies. Establishing that first is what keeps the negative below
    // from passing vacuously.
    let mut anchored = TrustStore::empty();
    assert!(anchored.add_anchor_der(root_der.clone()));
    assert!(
        verify_signer_block(&block, sf, &anchored).is_some(),
        "fixture must be a genuinely valid signature"
    );

    // Identical bytes, no anchor: a valid signature by an unknown party
    // establishes nothing. This is the whole point of a trust anchor —
    // anyone can produce a self-consistent signed JAR.
    assert!(
        verify_signer_block(&block, sf, &TrustStore::empty()).is_none(),
        "a cryptographically valid signature with no path to an anchor \
         must not be reported as a verified signer"
    );

    // The reason is specifically the missing anchor, not a parse accident.
    let leaf = X509Cert::parse(&leaf_der).expect("leaf parses");
    assert_eq!(
        verify_chain(&leaf, &[], &TrustStore::empty())
            .expect_err("empty store must find no path"),
        TrustError::NoTrustAnchor
    );
}

#[test]
fn tb_unvalidated_certificates_are_not_surfaced_as_the_signers() {
    // A CMS `certificates` set is attacker-supplied. Appending a
    // certificate to an otherwise legitimately signed JAR must not get
    // it reported through `CodeSource.getCertificates()`: it took no
    // part in path construction, so nothing about it was verified.
    let sf = b"Signature-Version: 1.0\r\nSHA-256-Digest-Manifest: AAAA\r\n\r\n";
    let interloper = tb_cert("TB-Interloper", "TB-Interloper", &tb_validity_now());
    let (block, leaf_der, root_der) = tb_signer_block(sf, &[interloper.clone()]);

    let mut ts = TrustStore::empty();
    assert!(ts.add_anchor_der(root_der.clone()));
    let vs =
        verify_signer_block(&block, sf, &ts).expect("the legitimate signer must still verify");

    assert_eq!(
        vs.chain,
        vec![leaf_der, root_der],
        "chain must be exactly the validated path, leaf first, anchor last"
    );
    assert!(
        !vs.chain.contains(&interloper),
        "a certificate that was never validated was surfaced as the signer's"
    );
}

#[test]
fn tb_expired_and_not_yet_valid_certs_reject() {
    let in_date = tb_validity_now();
    let root_der = tb_cert("TB-Root", "TB-Root", &in_date);
    let mut ts = TrustStore::empty();
    assert!(ts.add_anchor_der(root_der));

    // notAfter in the past.
    let expired = tb_cert(
        "TB-Leaf",
        "TB-Root",
        &seq(&[
            tlv(0x17, b"000101000000Z").as_slice(),
            tlv(0x17, b"010101000000Z").as_slice(),
        ]
        .concat()),
    );
    let cert = X509Cert::parse(&expired).expect("expired cert parses");
    assert_eq!(
        verify_chain(&cert, &[], &ts).expect_err("expired leaf must reject"),
        TrustError::Expired
    );

    // notBefore in the future.
    let premature = tb_cert(
        "TB-Leaf",
        "TB-Root",
        &seq(&[
            tlv(0x18, b"20900101000000Z").as_slice(),
            tlv(0x18, b"20990101000000Z").as_slice(),
        ]
        .concat()),
    );
    let cert = X509Cert::parse(&premature).expect("not-yet-valid cert parses");
    assert_eq!(
        verify_chain(&cert, &[], &ts).expect_err("not-yet-valid leaf must reject"),
        TrustError::Expired
    );

    // The same fixture with a window covering "now" chains fine, so the
    // two rejections above are about the dates and nothing else.
    let good = tb_cert("TB-Leaf", "TB-Root", &in_date);
    let cert = X509Cert::parse(&good).expect("in-date cert parses");
    verify_chain(&cert, &[], &ts).expect("in-date leaf must chain to the anchor");
}

#[test]
fn tb_unreadable_validity_window_rejects() {
    // RFC 5280 §4.1.2.5 admits only UTCTime and GeneralizedTime. A
    // validity SEQUENCE carrying something else is structurally
    // parseable, so it used to reach `cert_dates_ok` — which answered
    // "in date" for a window it could not read. A window we could not
    // read is a window we did not check, and it now refuses.
    let bogus = seq(&[
        tlv(0x0C, b"whenever").as_slice(), // UTF8String, not a Time
        tlv(0x0C, b"eventually").as_slice(),
    ]
    .concat());
    let der = tb_cert("TB-Leaf", "TB-Root", &bogus);
    let cert = X509Cert::parse(&der).expect("structurally parseable");
    assert!(
        !super::cert_dates_ok(&cert),
        "an unreadable validity window must not be reported as in-date"
    );

    // ...and it is fatal to the chain, not merely logged.
    let root_der = tb_cert("TB-Root", "TB-Root", &tb_validity_now());
    let mut ts = TrustStore::empty();
    assert!(ts.add_anchor_der(root_der));
    assert_eq!(
        verify_chain(&cert, &[], &ts).expect_err("must reject"),
        TrustError::Expired
    );
}

#[test]
fn tb_entry_absent_from_manifest_is_not_covered() {
    // The classic partial-signing gap: an archive holds entries the
    // manifest never named. `parse_manifest_entry_digests` is the
    // authority on what the signer committed to, and it must simply not
    // list them — "absent" is the signal callers gate on.
    let signed_body = b"class bytes that were signed";
    let dig = b64enc(&raw_digest(DigestAlg::Sha256, signed_body));
    // The archive also contains `pkg/Injected.class` (no section at all)
    // and `pkg/NoDigest.class` (a section carrying no digest).
    let manifest = format!(
        "Manifest-Version: 1.0\r\n\r\n\
         Name: pkg/Signed.class\r\nSHA-256-Digest: {dig}\r\n\r\n\
         Name: pkg/NoDigest.class\r\nComment: not a digest attribute\r\n\r\n"
    );
    let declared = parse_manifest_entry_digests(manifest.as_bytes());
    let names: Vec<&str> = declared.iter().map(|d| d.name.as_str()).collect();

    assert_eq!(
        names,
        vec!["pkg/Signed.class"],
        "only entries the manifest commits to with a digest are covered"
    );
    assert!(
        !names.contains(&"pkg/Injected.class"),
        "an entry with no manifest section must never be treated as covered"
    );
    assert!(
        !names.contains(&"pkg/NoDigest.class"),
        "a Name: section without a recognised <alg>-Digest commits to nothing"
    );
    // Guard against the whole assertion set passing because the parser
    // returned nothing at all: the one covered entry really is covered.
    assert!(digest_matches(
        declared[0].alg,
        signed_body,
        &declared[0].expected
    ));
}

#[test]
fn tb_integrity_checking_still_works() {
    // The integrity half of the JAR chain — .SF → MANIFEST.MF → entry
    // bytes — is genuinely complete, and this audit preserves it rather
    // than rejecting it. It is named as integrity, not trust.
    let body = b"the real class bytes";
    let entry_digest = b64enc(&raw_digest(DigestAlg::Sha256, body));
    let manifest = format!(
        "Manifest-Version: 1.0\r\n\r\n\
         Name: pkg/Real.class\r\nSHA-256-Digest: {entry_digest}\r\n\r\n"
    );
    let manifest_digest = b64enc(&raw_digest(DigestAlg::Sha256, manifest.as_bytes()));
    let sf =
        format!("Signature-Version: 1.0\r\nSHA-256-Digest-Manifest: {manifest_digest}\r\n\r\n");

    assert!(
        verify_sf_binds_manifest(sf.as_bytes(), manifest.as_bytes()),
        ".SF must bind the manifest it committed to"
    );
    let declared = parse_manifest_entry_digests(manifest.as_bytes());
    assert_eq!(declared.len(), 1);
    assert!(
        digest_matches(declared[0].alg, body, &declared[0].expected),
        "the committed-to entry's bytes must match"
    );

    // Tamper at either link and it breaks.
    assert!(!digest_matches(
        declared[0].alg,
        b"swapped bytes",
        &declared[0].expected
    ));
    let tampered = manifest.replace("pkg/Real.class", "pkg/Evil.class");
    assert!(!verify_sf_binds_manifest(
        sf.as_bytes(),
        tampered.as_bytes()
    ));

    // TRUST BOUNDARY: everything above used no key and no certificate.
    // A `.SF` anyone can write produces the same `true`, so integrity is
    // not, and must not be read as, trust — that comes only from
    // `verify_signer_block` against a real anchor.
    assert!(
        verify_signer_block(sf.as_bytes(), sf.as_bytes(), &TrustStore::empty()).is_none(),
        "manifest integrity says nothing about who produced the bytes"
    );
}
