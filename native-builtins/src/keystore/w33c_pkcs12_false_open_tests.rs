use super::*;

/// SEQUENCE { INTEGER 0, SEQUENCE { OID rsaEncryption, NULL },
/// OCTET STRING 4 } -- a well-formed, if tiny, PKCS#8 `PrivateKeyInfo`.
const PLAIN: &[u8] = &[
    0x30, 0x18, 0x02, 0x01, 0x00, 0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d,
    0x01, 0x01, 0x01, 0x05, 0x00, 0x04, 0x04, 0xde, 0xad, 0xbe, 0xef,
];

/// A deterministic salt / IV per `n`, so every run tests the same
/// envelopes (a PBES2 envelope is otherwise random by construction).
fn salt_iv(n: u32) -> ([u8; 20], [u8; 16]) {
    let mut salt = [0x5au8; 20];
    salt[..4].copy_from_slice(&n.to_le_bytes());
    let mut iv = [0xa5u8; 16];
    iv[..4].copy_from_slice(&n.wrapping_mul(0x9e37_79b9).to_le_bytes());
    (salt, iv)
}

/// `plain` under `password`, one PBKDF2 round (the count is read back out
/// of the envelope, so the decrypt side needs no change).
fn envelope(n: u32, plain: &[u8], password: &[u8]) -> p12::EncryptedPrivateKeyInfo {
    let (salt, iv) = salt_iv(n);
    pbes2_encrypt_with(plain, password, &salt, &iv, 1).expect("AES-CBC encrypt")
}

/// What the pre-w33-c chains accepted: any decryption whose padding fits.
fn padding_fits(epk: &p12::EncryptedPrivateKeyInfo, password: &str) -> Option<Vec<u8>> {
    decrypt_secret_pbes2(epk, password.as_bytes())
        .or_else(|| decrypt_secret_pbes2(epk, &pkcs12_bmp_string(password)))
}

#[test]
fn the_pkcs8_acceptor_takes_every_key_shape_and_nothing_else() {
    assert!(is_pkcs8_private_key_info(PLAIN), "RSA, NULL parameters");
    // Ed25519: no parameters at all.
    let mut ed = vec![
        0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22,
        0x04, 0x20,
    ];
    ed.extend_from_slice(&[7u8; 32]);
    assert!(is_pkcs8_private_key_info(&ed), "Ed25519, absent parameters");
    // RFC 5958 trailing `[0]` attributes (empty).
    let mut with_attrs = PLAIN.to_vec();
    with_attrs[1] += 2;
    with_attrs.extend_from_slice(&[0xa0, 0x00]);
    assert!(
        is_pkcs8_private_key_info(&with_attrs),
        "trailing [0] attributes"
    );
    // BER indefinite lengths (BouncyCastle), outer and inner.
    let indefinite: &[u8] = &[
        0x30, 0x80, 0x02, 0x01, 0x00, 0x30, 0x80, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7,
        0x0d, 0x01, 0x01, 0x01, 0x05, 0x00, 0x00, 0x00, 0x04, 0x04, 0xde, 0xad, 0xbe, 0xef,
        0x00, 0x00,
    ];
    assert!(
        is_pkcs8_private_key_info(indefinite),
        "BER indefinite lengths"
    );

    assert!(
        !is_pkcs8_private_key_info(b"\x30\x08PRIVKEY1"),
        "not a PrivateKeyInfo"
    );
    assert!(!is_pkcs8_private_key_info(&[]), "empty");
    let mut trailing = PLAIN.to_vec();
    trailing.push(0x00);
    assert!(
        !is_pkcs8_private_key_info(&trailing),
        "bytes past the SEQUENCE"
    );
    let mut v2 = PLAIN.to_vec();
    v2[4] = 2;
    assert!(!is_pkcs8_private_key_info(&v2), "version 2 does not exist");
    // An EncryptedPrivateKeyInfo is not a key either.
    let wrapped = yasna::construct_der(|w| envelope(0, PLAIN, b"pw").write(w));
    assert!(
        !is_pkcs8_private_key_info(&wrapped),
        "an envelope is not a key"
    );
}

#[test]
fn a_secret_bag_plaintext_is_parsed_not_trusted() {
    let alg = secret_key_alg_oid("AES").expect("AES has an OID");
    let plain = yasna::construct_der(|w| {
        w.write_sequence(|w| {
            w.next().write_u8(0);
            w.next().write_sequence(|w| {
                w.next().write_oid(&alg);
            });
            w.next().write_bytes(&[1u8; 16]);
        })
    });
    let (_, key) = secret_key_info(&plain).expect("a SunPKCS12 secret plaintext");
    assert_eq!(key, vec![1u8; 16]);
    assert!(secret_key_info(b"\x30\x08PRIVKEY1").is_none());
}

/// The acceptor, not the password, decides: a plaintext that is not a
/// PKCS#8 key never comes out of the funnel, whichever password made it.
#[test]
fn a_non_key_plaintext_never_opens_as_a_key() {
    let not_a_key = yasna::construct_der(|w| envelope(1, b"noise, not a key", b"pw").write(w));
    assert_eq!(recover_key_any_scheme(&not_a_key, b"pw"), None);
    let key = yasna::construct_der(|w| envelope(1, PLAIN, b"pw").write(w));
    assert_eq!(recover_key_any_scheme(&key, b"pw").as_deref(), Some(PLAIN));
}

/// Two thousand envelopes under the ENTRY password, each asked with the
/// STORE password: none may open. Before w33-c about one in 128 did (the
/// premise count below), handing `getKey` noise instead of an exception.
#[test]
fn a_wrong_password_never_opens_an_envelope_even_when_its_padding_fits() {
    let mut padding_fitted = 0usize;
    for n in 0..2000u32 {
        let epk = envelope(n, PLAIN, b"keypass");
        if padding_fits(&epk, "changeit").is_some() {
            padding_fitted += 1;
        }
        let der = yasna::construct_der(|w| epk.write(w));
        assert_eq!(
            recover_key_any_scheme(&der, b"changeit"),
            None,
            "envelope {n}: the store password opened the entry password's envelope"
        );
        assert_eq!(
            recover_key_any_scheme(&der, b"keypass").as_deref(),
            Some(PLAIN),
            "envelope {n}: the entry password must still open it"
        );
    }
    assert!(
        padding_fitted > 0,
        "premise: some wrong-password decryptions pass the padding check \
         (expected ~16 of 2000)"
    );
}

/// `LTKeyStoreEngineSweep`'s failing row, at the storage layer: an entry
/// protected with its own password, stored and reloaded with the STORE
/// password, whose envelope the store password happens to "open". The
/// reload used to keep that noise as the key, so `getKey` with the right
/// entry password returned a key unequal to the one stored.
#[test]
fn a_reload_with_the_store_password_keeps_the_entry_envelope_sealed() {
    let (n, epk) = (0..100_000u32)
        .map(|n| (n, envelope(n, PLAIN, b"keypass")))
        .find(|(_, epk)| padding_fits(epk, "changeit").is_some())
        .expect("about one salt in 128 lets the wrong password's padding fit");
    let noise = padding_fits(&epk, "changeit").expect("found above");
    assert_ne!(
        noise.as_slice(),
        PLAIN,
        "premise (salt {n}): the open is a false one"
    );
    assert!(
        !is_pkcs8_private_key_info(&noise),
        "premise: the noise is not a key"
    );

    let env_der = yasna::construct_der(|w| epk.write(w));
    let mut entries = IndexMap::new();
    entries.insert(
        "k".to_string(),
        KeyStoreEntry {
            alias: "k".to_string(),
            creation_time_ms: 0,
            kind: EntryKind::PrivateKey {
                key_der: PLAIN.to_vec(),
                chain: Vec::new(),
                protected: Some(env_der),
            },
        },
    );
    let der = write_pkcs12(&LoadedKeyStore { entries }, b"changeit").expect("write");
    let back = load_pkcs12(&der, b"changeit").expect("load with the store password");
    match &back.entries.get("k").expect("alias preserved").kind {
        EntryKind::PrivateKey { key_der, .. } => {
            assert!(
                is_encrypted_private_key(key_der),
                "salt {n}: the store password must leave the entry's envelope sealed"
            );
            assert_eq!(
                recover_key_any_scheme(key_der, b"keypass").as_deref(),
                Some(PLAIN),
                "salt {n}: getKey's entry password opens it to the key that was stored"
            );
        }
        other => panic!("came back as {other:?}"),
    }
}
