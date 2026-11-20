use super::*;

/// P-256 identity, generated once with openssl and frozen here so these
/// tests need no crypto dependency and run on every platform. The key is
/// PKCS#8 WITH `publicKey [1]`; `strip_to_jdk_shape` reduces it to what the
/// JDK emits.
const P256_KEY: &str = "\
    308187020100301306072a8648ce3d020106082a8648ce3d030107046d306b0201010420\
    52938df7e0c9a16537f034339c7c7359eced61a35d1b4c87760275ff735ee055a1440342\
    00040e06bf5c39a8aa566ca83cb86b72d7e38686fee8ce84850064372f42433a7ad3cd49\
    da99d89ec101763121462a25f8e6c18c90fb7eb089fcfe0e73aa0d743c42";
const P256_CRT: &str = "\
    3082017c30820123a00302010202141f64638ec784227782eb3d08509bde7d8e9fc10c30\
    0a06082a8648ce3d04030230143112301006035504030c096c6f63616c686f7374301e17\
    0d3236303831323138303333365a170d3336303830393138303333365a30143112301006\
    035504030c096c6f63616c686f73743059301306072a8648ce3d020106082a8648ce3d03\
    0107034200040e06bf5c39a8aa566ca83cb86b72d7e38686fee8ce84850064372f42433a\
    7ad3cd49da99d89ec101763121462a25f8e6c18c90fb7eb089fcfe0e73aa0d743c42a353\
    3051301d0603551d0e0416041468f6f21e5636c47e25780b9b832afe4de707a302301f06\
    03551d2304183016801468f6f21e5636c47e25780b9b832afe4de707a302300f0603551d\
    130101ff040530030101ff300a06082a8648ce3d0403020347003044022001ce1e112093\
    114d88086e2105680bb39606ba9c5f67332da35764aafc8d977402204d049c6b89003aa0\
    5b34697a1a6380393dba365220e82d3f25a6b368d1807289";

/// P-384 identity — the splice must be curve-agnostic, since it copies the
/// point out of the certificate rather than computing it.
const P384_KEY: &str = "\
    3081b6020100301006072a8648ce3d020106052b8104002204819e30819b020101043004\
    0e7e93856a01d61ca3d12ac7adafd39eccdb844fbeeda287df27b950794f39f4d8277cea\
    3f4beb35df0ce4e5dec82aa16403620004e185c328a18debe1215987b59333173d761ddc\
    fc5d5a6172461f40cb8b5e0c2d350792d8008d0b653c81f93c44f8ff8d1b28751f84772b\
    90662213b6bf2d90fb31f60027c18a38d23d7cc2bd80644a628de877b04077416b879732\
    ead4163238";
const P384_CRT: &str = "\
    308201ba30820140a00302010202145594e93072003edfb390fffc595b84db6628df7c30\
    0a06082a8648ce3d04030230143112301006035504030c096c6f63616c686f7374301e17\
    0d3236303831323138303333365a170d3336303830393138303333365a30143112301006\
    035504030c096c6f63616c686f73743076301006072a8648ce3d020106052b8104002203\
    620004e185c328a18debe1215987b59333173d761ddcfc5d5a6172461f40cb8b5e0c2d35\
    0792d8008d0b653c81f93c44f8ff8d1b28751f84772b90662213b6bf2d90fb31f60027c1\
    8a38d23d7cc2bd80644a628de877b04077416b879732ead4163238a3533051301d060355\
    1d0e0416041402e7bb3ae8de78e5dae7449f71da2296fd395874301f0603551d23041830\
    16801402e7bb3ae8de78e5dae7449f71da2296fd395874300f0603551d130101ff040530\
    030101ff300a06082a8648ce3d040302036800306502301806748b77941e85ada3cbe438\
    5e6a5878ffd9b3a1950ffc0265ff13ef21958816d6811b8bffcd3c35f4d0b63f185b1c02\
    3100afa5d9b1a03c4df25a3807ff3a4408ba7efa1ffd9d6e68aa13429ee73d39b23f3d61\
    1e36f3ed53be1379ed00737f5d44";

fn unhex(s: &str) -> Vec<u8> {
    let clean: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    (0..clean.len() / 2)
        .map(|i| u8::from_str_radix(&clean[i * 2..i * 2 + 2], 16).unwrap())
        .collect()
}

/// Reduce a PKCS#8 EC key's inner SEC1 to `version` + `privateKey` —
/// exactly what a stock JDK's `getEncoded()` produces.
fn strip_to_jdk_shape(der: &[u8]) -> Vec<u8> {
    let (outer, _) = der_tlv(der, 0).unwrap();
    let ver_end = der_tlv_end(der, outer).unwrap();
    let alg_end = der_tlv_end(der, ver_end).unwrap();
    let (oct_body, _) = der_tlv(der, alg_end).unwrap();
    let (sec1_body, _) = der_tlv(der, oct_body).unwrap();
    let iv_end = der_tlv_end(der, sec1_body).unwrap();
    let ipk_end = der_tlv_end(der, iv_end).unwrap();

    let inner = der[sec1_body..ipk_end].to_vec();
    let mut body = Vec::new();
    body.extend_from_slice(&der[outer..alg_end]);
    body.extend_from_slice(&der_tlv_encode(0x04, &der_tlv_encode(0x30, &inner)));
    der_tlv_encode(0x30, &body)
}

/// `(parameters[0] present, publicKey[1] present)` for a PKCS#8 EC key.
fn inner_optionals(der: &[u8]) -> (bool, bool) {
    let (outer, _) = der_tlv(der, 0).unwrap();
    let ver_end = der_tlv_end(der, outer).unwrap();
    let alg_end = der_tlv_end(der, ver_end).unwrap();
    let (oct_body, _) = der_tlv(der, alg_end).unwrap();
    let (sec1_body, sec1_end) = der_tlv(der, oct_body).unwrap();
    let mut p = der_tlv_end(der, der_tlv_end(der, sec1_body).unwrap()).unwrap();
    let (mut a, mut b) = (false, false);
    while p < sec1_end {
        match der[p] {
            0xa0 => a = true,
            0xa1 => b = true,
            _ => {}
        }
        p = der_tlv_end(der, p).unwrap();
    }
    (a, b)
}

fn accepted_by_ring(pkcs8: &[u8]) -> bool {
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(pkcs8.to_vec()));
    rustls::crypto::ring::sign::any_supported_type(&key).is_ok()
}

fn round_trip(key_hex: &str, crt_hex: &str, label: &str) {
    let full = unhex(key_hex);
    let cert = unhex(crt_hex);
    assert!(
        inner_optionals(&full).1,
        "{label}: fixture must carry publicKey[1] to be a valid control"
    );
    assert!(
        accepted_by_ring(&full),
        "{label}: control — ring must accept the unmodified fixture"
    );

    let jdk = strip_to_jdk_shape(&full);
    assert_eq!(
        inner_optionals(&jdk),
        (false, false),
        "{label}: stripped key must carry no optional fields"
    );
    // The bug, pinned. If this ever starts passing, ring learned to derive
    // the public key and the splice below can be deleted.
    assert!(
        !accepted_by_ring(&jdk),
        "{label}: ring accepted a PKCS#8 EC key with no publicKey"
    );

    let repaired = ec_pkcs8_splice_public_key(&jdk, &cert)
        .unwrap_or_else(|| panic!("{label}: splice declined a stripped EC key"));
    assert!(
        inner_optionals(&repaired).1,
        "{label}: repaired key must carry publicKey[1]"
    );
    assert!(
        accepted_by_ring(&repaired),
        "{label}: ring must accept the repaired key"
    );
}

#[test]
fn jdk_shaped_p256_identity_is_repaired_from_its_certificate() {
    round_trip(P256_KEY, P256_CRT, "P-256");
}

#[test]
fn jdk_shaped_p384_identity_is_repaired_from_its_certificate() {
    round_trip(P384_KEY, P384_CRT, "P-384");
}

/// The mTLS `KeyManager` resolver is the one identity path that never
/// parses PEM: `JavaKeyManagerResolver::resolve_via_java` takes DER
/// straight from `km_alias_material` and hands it to
/// `CertifiedKey::from_der`. It was missed when the repair first landed, so
/// a JDK-generated EC *client* certificate delivered through a Java
/// `KeyManager` still resolved to nothing.
///
/// Pinned through `CertifiedKey::from_der` rather than
/// `any_supported_type`, because that is the call the resolver makes — a
/// test against the lower-level entry point would not have caught the
/// missing call site either.
#[test]
fn a_key_manager_supplied_jdk_ec_identity_is_repaired_too() {
    let cert = CertificateDer::from(unhex(P256_CRT));
    let jdk = strip_to_jdk_shape(&unhex(P256_KEY));
    let provider = rustls::crypto::ring::default_provider();

    // Control: the shape `resolve_via_java` used to build is genuinely
    // rejected, so the positive half below cannot pass vacuously.
    assert!(
        CertifiedKey::from_der(
            vec![cert.clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(jdk.clone())),
            &provider,
        )
        .is_err(),
        "control — CertifiedKey::from_der must reject the stripped JDK shape"
    );

    // The function `resolve_via_java` calls, not a re-creation of it.
    assert!(
        certified_key_from_der_repairing_ec(vec![cert], jdk, &provider).is_ok(),
        "the KeyManager identity path must repair the JDK EC key it is handed"
    );
}

#[test]
fn splice_declines_a_key_that_already_has_a_public_key() {
    assert!(
        ec_pkcs8_splice_public_key(&unhex(P256_KEY), &unhex(P256_CRT)).is_none(),
        "a key ring already accepts must be left byte-identical"
    );
}

#[test]
fn splice_declines_when_the_certificate_cannot_lend_a_matching_point() {
    // A P-384 certificate must not have its point spliced into a P-256 key:
    // the splice keys off the cert, so a mismatched pair must be refused by
    // ring rather than silently producing an identity that signs wrong.
    let jdk_p256 = strip_to_jdk_shape(&unhex(P256_KEY));
    let spliced = ec_pkcs8_splice_public_key(&jdk_p256, &unhex(P384_CRT))
        .expect("the P-384 cert does carry an EC point");
    assert!(
        !accepted_by_ring(&spliced),
        "ring must reject a public key that does not match the private scalar"
    );
}

#[test]
fn splice_declines_non_ec_keys() {
    // An RSA PKCS#8 (any bytes with the RSA algorithm OID) must be left
    // alone — those parse fine and rewriting them could only break them.
    let rsa_alg_pkcs8 = unhex("30820102020100300d06092a864886f70d0101010500048200ec3082");
    assert!(ec_pkcs8_splice_public_key(&rsa_alg_pkcs8, &unhex(P256_CRT)).is_none());
}

#[test]
fn splice_declines_garbage_without_panicking() {
    let cert = unhex(P256_CRT);
    for bad in [
        &b""[..],
        &b"\x30"[..],
        &b"\x30\x82"[..],
        &b"\x30\x03\x02\x01\x00"[..],
        &b"\x02\x01\x00"[..],
        &[0x30, 0x84, 0xff, 0xff, 0xff, 0xff][..],
        &[0x30, 0x80, 0x02, 0x01, 0x00][..],
    ] {
        assert!(ec_pkcs8_splice_public_key(bad, &cert).is_none());
    }
    let jdk = strip_to_jdk_shape(&unhex(P256_KEY));
    for bad_cert in [
        &b""[..],
        &b"\x30\x03\x02\x01\x00"[..],
        &[0x30, 0x84, 0xff, 0xff, 0xff, 0xff][..],
    ] {
        assert!(ec_pkcs8_splice_public_key(&jdk, bad_cert).is_none());
    }
}
