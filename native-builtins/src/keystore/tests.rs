//! Hermetic tests with inlined fixture bytes. The fixtures are tiny
//! synthetic keystores produced specifically for this test (a single
//! self-signed leaf + one trusted-cert entry, both ≤ 4 KiB).
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

use super::*;

// Synthesise a JKS file in-memory by writing the format directly. This
// keeps the test free of external tooling (no need for `keytool` to be on
// the test machine). The cert + key payloads here are deliberately tiny
// dummy DER blobs — the JKS parser only treats them as opaque bytes.
fn synth_jks(password: &[u8]) -> Vec<u8> {
    let mut body: Vec<u8> = Vec::new();
    body.extend_from_slice(&JKS_MAGIC.to_be_bytes());
    body.extend_from_slice(&2u32.to_be_bytes()); // version 2
    body.extend_from_slice(&2u32.to_be_bytes()); // entry_count = 2

    // Entry 1: TrustedCertEntry, alias "trusted-ca"
    body.extend_from_slice(&2u32.to_be_bytes()); // tag = 2
    let alias1 = b"trusted-ca";
    body.extend_from_slice(&(alias1.len() as u16).to_be_bytes());
    body.extend_from_slice(alias1);
    body.extend_from_slice(&123_456_789u64.to_be_bytes()); // creation date
    let cert_type = b"X.509";
    body.extend_from_slice(&(cert_type.len() as u16).to_be_bytes());
    body.extend_from_slice(cert_type);
    let cert_der = b"\x30\x06DUMMY1"; // 8-byte placeholder
    body.extend_from_slice(&(cert_der.len() as u32).to_be_bytes());
    body.extend_from_slice(cert_der);

    // Entry 2: PrivateKeyEntry, alias "leaf-key"
    body.extend_from_slice(&1u32.to_be_bytes()); // tag = 1
    let alias2 = b"leaf-key";
    body.extend_from_slice(&(alias2.len() as u16).to_be_bytes());
    body.extend_from_slice(alias2);
    body.extend_from_slice(&987_654_321u64.to_be_bytes());
    let enc_key = b"\x30\x07PKCS8KEY";
    body.extend_from_slice(&(enc_key.len() as u32).to_be_bytes());
    body.extend_from_slice(enc_key);
    body.extend_from_slice(&1u32.to_be_bytes()); // chain_count
    body.extend_from_slice(&(cert_type.len() as u16).to_be_bytes());
    body.extend_from_slice(cert_type);
    let leaf_der = b"\x30\x06DUMMY2";
    body.extend_from_slice(&(leaf_der.len() as u32).to_be_bytes());
    body.extend_from_slice(leaf_der);

    // Append integrity tag.
    let mac = jks_password_mac(password, &body);
    body.extend_from_slice(&mac);
    body
}

// -- BER -> DER length normalisation -----------------------------------

/// The discrimination `engine_get_key` refuses on: a real key must never
/// read as an envelope, or a correct password starts throwing.
///
/// It is exact rather than heuristic, and this states why: a PKCS#8
/// `PrivateKeyInfo` opens with an INTEGER version where an
/// `EncryptedPrivateKeyInfo` opens with an `AlgorithmIdentifier` SEQUENCE,
/// so the two cannot be confused by a parser that reads the first element.
/// KS-6: the recovery funnel has to open BOTH protection schemes, because
/// `getKey` is handed one password and the entry could have arrived in
/// either envelope. Written to fail on the pre-KS-6 funnel, which knew only
/// the JKS one: the PKCS#12 assertions below are exactly the rows a
/// HotSpot-written store produces.
#[test]
fn both_protection_schemes_open_with_the_right_password_and_neither_with_the_wrong_one() {
    let plain: &[u8] = &[
        0x30, 0x18, 0x02, 0x01, 0x00, 0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7,
        0x0d, 0x01, 0x01, 0x01, 0x05, 0x00, 0x04, 0x04, 0xde, 0xad, 0xbe, 0xef,
    ];

    let jks = jks_protect_key(plain, b"entrypw").expect("OS entropy for the JKS envelope");
    assert_eq!(
        recover_key_any_scheme(&jks, b"entrypw").as_deref(),
        Some(plain),
        "the JKS envelope must still open through the widened funnel"
    );
    assert_eq!(
        recover_key_any_scheme(&jks, b"storepw"),
        None,
        "a wrong password must not open the JKS envelope"
    );

    let epki = pbes2_encrypt(plain, b"entrypw").expect("OS entropy for the PBES2 envelope");
    let p12 = yasna::construct_der(|w| epki.write(w));
    assert_eq!(
        recover_key_any_scheme(&p12, b"entrypw").as_deref(),
        Some(plain),
        "KS-6: a PKCS#12 envelope must open too -- the funnel knew only JKS \
         before, which is why a separately-protected shrouded bag came back \
         out of getKey as ciphertext wearing a PrivateKey mirror"
    );
    assert_eq!(
        recover_key_any_scheme(&p12, b"storepw"),
        None,
        "a wrong password must not open the PKCS#12 envelope"
    );

    // And the two envelopes are not interchangeable: each is recognisable,
    // so `engine_get_key` can pick the message HotSpot would have printed.
    assert!(is_jks_encrypted_private_key(&jks));
    assert!(!is_jks_encrypted_private_key(&p12));
    assert!(is_encrypted_private_key(&p12));
}

#[test]
fn an_envelope_is_told_from_a_key_exactly() {
    // SEQUENCE { INTEGER 0, SEQUENCE { OID rsaEncryption, NULL },
    //            OCTET STRING 4 } -- a well-formed, if tiny, PKCS#8.
    let plain: &[u8] = &[
        0x30, 0x18, 0x02, 0x01, 0x00, 0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7,
        0x0d, 0x01, 0x01, 0x01, 0x05, 0x00, 0x04, 0x04, 0xde, 0xad, 0xbe, 0xef,
    ];
    assert!(
        !is_encrypted_private_key(plain),
        "a plaintext PKCS#8 key must not read as an envelope -- if it does, \
         every getKey with the CORRECT password throws"
    );

    let wrapped = jks_protect_key(plain, b"changeit")
        .expect("OS entropy is available in the test environment");
    assert!(
        is_encrypted_private_key(&wrapped),
        "the JKS key protector's own output must read as an envelope"
    );
    assert!(is_jks_encrypted_private_key(&wrapped));

    // And it round-trips, so the envelope this recognises is one the
    // right password still opens.
    assert_eq!(
        jks_recover_key(&wrapped, b"changeit").as_deref(),
        Some(plain)
    );
    assert_eq!(jks_recover_key(&wrapped, b"wrong"), None);
}
#[test]
fn der_input_is_returned_unchanged() {
    // SEQUENCE { INTEGER 5 }, already definite-length.
    let der = [0x30u8, 0x03, 0x02, 0x01, 0x05];
    assert_eq!(ber_to_definite_length(&der).expect("rewrite"), der.to_vec());
}

#[test]
fn indefinite_length_becomes_definite() {
    // SEQUENCE (indefinite) { INTEGER 5 } EOC
    let ber = [0x30u8, 0x80, 0x02, 0x01, 0x05, 0x00, 0x00];
    assert_eq!(
        ber_to_definite_length(&ber).expect("rewrite"),
        vec![0x30, 0x03, 0x02, 0x01, 0x05]
    );
}

#[test]
fn nested_indefinite_lengths_are_rewritten_innermost_first() {
    // SEQ(indef){ SEQ(indef){ INTEGER 5 } } - the outer length can only be
    // known once the inner one has been rewritten.
    let ber = [
        0x30u8, 0x80, 0x30, 0x80, 0x02, 0x01, 0x05, 0x00, 0x00, 0x00, 0x00,
    ];
    assert_eq!(
        ber_to_definite_length(&ber).expect("rewrite"),
        vec![0x30, 0x05, 0x30, 0x03, 0x02, 0x01, 0x05]
    );
}

#[test]
fn segmented_octet_string_is_joined_and_made_primitive() {
    // constructed OCTET STRING (indef) { OCTET STRING AA BB, OCTET STRING CC }
    let ber = [
        0x24u8, 0x80, 0x04, 0x02, 0xAA, 0xBB, 0x04, 0x01, 0xCC, 0x00, 0x00,
    ];
    assert_eq!(
        ber_to_definite_length(&ber).expect("rewrite"),
        vec![0x04, 0x03, 0xAA, 0xBB, 0xCC]
    );
}

#[test]
fn long_form_lengths_survive_the_round_trip() {
    // A 200-byte OCTET STRING needs the long form (0x81 0xC8) both ways.
    let mut der = vec![0x04u8, 0x81, 0xC8];
    der.extend(std::iter::repeat(0x41).take(200));
    assert_eq!(ber_to_definite_length(&der).expect("rewrite"), der);
}

#[test]
fn truncated_input_is_an_error_not_a_silent_short_read() {
    // SEQUENCE claiming 3 content bytes but carrying one.
    assert!(ber_to_definite_length(&[0x30u8, 0x03, 0x02]).is_err());
    // Indefinite length with no end-of-contents.
    assert!(ber_to_definite_length(&[0x30u8, 0x80, 0x02, 0x01, 0x05]).is_err());
}

#[test]
fn detects_jks_magic() {
    let bytes = synth_jks(b"changeit");
    assert_eq!(
        u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        JKS_MAGIC
    );
}

#[test]
fn jks_roundtrip() {
    let bytes = synth_jks(b"changeit");
    let store = load_jks(&bytes, b"changeit").expect("load_jks");
    assert_eq!(store.entries.len(), 2);
    assert!(store.entries.contains_key("trusted-ca"));
    assert!(store.entries.contains_key("leaf-key"));

    match &store.entries["trusted-ca"].kind {
        EntryKind::TrustedCert { cert_der } => {
            assert_eq!(cert_der, b"\x30\x06DUMMY1");
        }
        _ => panic!("expected TrustedCert"),
    }
    match &store.entries["leaf-key"].kind {
        EntryKind::PrivateKey { key_der, chain, .. } => {
            assert_eq!(key_der, b"\x30\x07PKCS8KEY");
            assert_eq!(chain.len(), 1);
            assert_eq!(chain[0], b"\x30\x06DUMMY2");
        }
        _ => panic!("expected PrivateKey"),
    }
}

#[test]
fn jks_hmac_mismatch_rejected() {
    let mut bytes = synth_jks(b"changeit");
    // Flip a byte in the body (the entry_count high byte).
    bytes[8] ^= 0x01;
    let err = load_jks(&bytes, b"changeit").unwrap_err();
    assert!(
        matches!(
            err,
            KeyStoreError::JksMacMismatch | KeyStoreError::BadJksTag(_)
        ),
        "got {err:?}"
    );
}

#[test]
fn jks_wrong_password_rejected() {
    let bytes = synth_jks(b"changeit");
    let err = load_jks(&bytes, b"wrong").unwrap_err();
    assert!(matches!(err, KeyStoreError::JksMacMismatch));
}

#[test]
fn jks_empty_password_works() {
    let bytes = synth_jks(b"");
    let store = load_jks(&bytes, b"").expect("load_jks empty pw");
    assert_eq!(store.entries.len(), 2);
}

#[test]
fn dispatch_picks_jks() {
    let bytes = synth_jks(b"x");
    let store = load_keystore(&bytes, b"x").expect("dispatch");
    assert!(store.entries.contains_key("trusted-ca"));
}

#[test]
fn dispatch_rejects_unknown_format() {
    let err = load_keystore(b"NOTAKEYSTORE", b"").unwrap_err();
    assert!(matches!(err, KeyStoreError::UnknownFormat));
}

#[test]
fn registry_round_trip() {
    let store = LoadedKeyStore {
        entries: [(
            "alpha".to_string(),
            KeyStoreEntry {
                alias: "alpha".to_string(),
                creation_time_ms: 42,
                kind: EntryKind::TrustedCert {
                    cert_der: b"hi".to_vec(),
                },
            },
        )]
        .into_iter()
        .collect(),
    };
    let id = keystore_register(store);
    assert!(id > 0);
    let got = keystore_lookup(id).expect("registered store");
    assert_eq!(got.entries.len(), 1);
    let cert = keystore_get_cert_der(id, "alpha").unwrap();
    assert_eq!(cert, b"hi");
}

#[test]
fn jks_truncated_returns_error() {
    let bytes = synth_jks(b"x");
    let truncated = &bytes[..bytes.len() - 5];
    let err = load_jks(truncated, b"x").unwrap_err();
    // After moving HMAC to the front of `load_jks`, truncating the
    // trailing 5 bytes is detected as JksMacMismatch (the trailing
    // bytes interpreted as the 20-byte tag now cover real entry
    // bytes, so the SHA-1 over the now-shorter body diverges). The
    // older Truncated / BadJksTag paths are still reachable for
    // truncations large enough that the body cannot even fit the
    // 4+4+4+20 prelude.
    assert!(matches!(
        err,
        KeyStoreError::Truncated(_)
            | KeyStoreError::BadJksTag(_)
            | KeyStoreError::JksMacMismatch
    ));
}

#[test]
fn pkcs12_unknown_format_rejected() {
    // Just a SEQUENCE prefix with garbage payload — should fail parse.
    let bogus = b"\x30\x05\x00\x00\x00\x00\x00";
    let err = load_pkcs12(bogus, b"x").unwrap_err();
    assert!(matches!(
        err,
        KeyStoreError::Pkcs12Parse(_) | KeyStoreError::Pkcs12MacFailed
    ));
}

fn synth_sha256_mac_pkcs12(password: &str) -> Vec<u8> {
    let mut pfx = p12::PFX::new(b"\x30\x00", b"\x30\x00", None, password, "test")
        .expect("test PFX generation");
    let password_bmp = pkcs12_bmp_string(password);
    let auth_safe = pfx
        .auth_safe
        .data(&password_bmp)
        .expect("unencrypted AuthSafe");
    let mac_data = pfx.mac_data.as_mut().expect("MAC data");
    let digest = Pkcs12MacDigest::Sha256;
    mac_data.mac.digest_algorithm =
        p12::AlgorithmIdentifier::OtherAlg(p12::OtherAlgorithmIdentifier {
            algorithm_type: yasna::models::ObjectIdentifier::from_slice(&[
                2, 16, 840, 1, 101, 3, 4, 2, 1,
            ]),
            params: Some(vec![0x05, 0x00]),
        });
    let key = pkcs12_mac_kdf(
        digest,
        &password_bmp,
        &mac_data.salt,
        mac_data.iterations,
        3,
        digest.output_len(),
    );
    mac_data.mac.digest = pkcs12_hmac(digest, &key, &auth_safe);
    pfx.to_der()
}

#[test]
fn pkcs12_sha256_mac_accepts_correct_password_and_rejects_wrong_one() {
    // Since JDK 8u191, SunPKCS12 defaults to HmacPBESHA256.  `p12` 0.6
    // parses that MAC but its verifier unconditionally derives a SHA-1
    // key, producing a false "wrong password" result for Spring's .p12
    // fixtures.  Exercise the exact newer-MAC shape independently of any
    // workspace-local application fixture.
    let bytes = synth_sha256_mac_pkcs12("secret");
    let pfx = p12::PFX::parse(&bytes).expect("reparse generated PFX");
    assert!(verify_pkcs12_mac(&pfx, "secret"));
    load_pkcs12(&bytes, b"secret").expect("correct SHA-256 MAC password");
    let err = load_pkcs12(&bytes, b"wrong").unwrap_err();
    assert!(matches!(err, KeyStoreError::Pkcs12MacFailed));
}

/// SunPKCS12 (`keytool`) leaves the top-level AuthenticatedSafe content
/// unencrypted (`ContentInfo::Data`) and relies solely on the outer MAC
/// for integrity — only individual `PrivateKeyEntry` bags get their own
/// PBES2 encryption under a possibly-different entry password. `p12`
/// crate's own `PFX::new` doesn't model this (it PBE-encrypts the whole
/// cert `SafeContents`), so build the realistic shape by hand.
fn synth_unencrypted_content_pkcs12(password: &str) -> Vec<u8> {
    let cert_bag = p12::SafeBag {
        bag: p12::SafeBagKind::CertBag(p12::CertBag::X509(vec![0x30, 0x03, 0x02, 0x01, 0x00])),
        attributes: vec![],
    };
    let safe_contents =
        yasna::construct_der(|w| w.write_sequence_of(|w| cert_bag.write(w.next())));
    let inner_content_info = p12::ContentInfo::Data(safe_contents);
    let auth_safe_bytes =
        yasna::construct_der(|w| w.write_sequence_of(|w| inner_content_info.write(w.next())));
    let password_bmp = pkcs12_bmp_string(password);
    let mac_data = p12::MacData::new(&auth_safe_bytes, &password_bmp);
    let pfx = p12::PFX {
        version: 3,
        auth_safe: p12::ContentInfo::Data(auth_safe_bytes),
        mac_data: Some(mac_data),
    };
    pfx.to_der()
}

#[test]
fn pkcs12_null_password_skips_mac_verification() {
    // `KeyStore.load(stream, null)` — a Java `null` char[], not an empty
    // one — must skip PKCS#12 integrity checking entirely, matching
    // real-JDK's PKCS12KeyStore. `WebServerSslBundleTests` relies on this:
    // it opens a keystore's key entries without a keyStorePassword,
    // supplying the entry password later through `getKey()`. Passing an
    // empty password to `load_pkcs12` (its `verify_mac=true` form) must
    // still fail, since that's a real empty-string password attempt, not
    // a null one — only the explicit `verify_mac=false` path skips it.
    let bytes = synth_unencrypted_content_pkcs12("secret");
    let err = load_pkcs12(&bytes, b"").unwrap_err();
    assert!(matches!(err, KeyStoreError::Pkcs12MacFailed));
    load_pkcs12_ex(&bytes, b"", false).expect("null password skips MAC check");
}

#[test]
fn detect_algo_idx_handles_unknown() {
    // No RSA/EC OID — defaults to 6 (RSA).
    assert_eq!(detect_algo_idx(&[0u8; 32]), 6);
}

#[test]
fn detect_algo_idx_finds_rsa_oid() {
    let mut blob = vec![0u8; 16];
    blob.extend_from_slice(&[
        0x06, 0x09, 0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x01, 0x01,
    ]);
    assert_eq!(detect_algo_idx(&blob), 6);
}

#[test]
fn detect_algo_idx_finds_ec_oid() {
    let mut blob = vec![0u8; 16];
    blob.extend_from_slice(&[0x06, 0x07, 0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x02, 0x01]);
    assert_eq!(detect_algo_idx(&blob), 7);
}

#[test]
fn fnv1a_32_stable_across_runs() {
    // Compile-time stable, sanity check the constants.
    assert_eq!(fnv1a_32(b""), 0x811C_9DC5);
    // Hash is deterministic per-input.
    let h1 = fnv1a_32(b"hello");
    let h2 = fnv1a_32(b"hello");
    assert_eq!(h1, h2);
    let h3 = fnv1a_32(b"world");
    assert_ne!(h1, h3);
}

// -----------------------------------------------------------------
// PKCS#12 writer (`pkcs12-setentry-secretkeyentry-…-20260805`)
// -----------------------------------------------------------------

fn secret_entry(alias: &str, bytes: &[u8], algorithm: &str) -> (String, KeyStoreEntry) {
    (
        alias.to_string(),
        KeyStoreEntry {
            alias: alias.to_string(),
            creation_time_ms: 0,
            kind: EntryKind::SecretKey {
                key_bytes: bytes.to_vec(),
                algorithm: algorithm.to_string(),
            },
        },
    )    }

fn cert_entry(alias: &str, der: &[u8]) -> (String, KeyStoreEntry) {
    (
        alias.to_string(),
        KeyStoreEntry {
            alias: alias.to_string(),
            creation_time_ms: 0,
            kind: EntryKind::TrustedCert {
                cert_der: der.to_vec(),
            },
        },
    )
}

fn store_of(entries: Vec<(String, KeyStoreEntry)>) -> LoadedKeyStore {
    let mut map: IndexMap<String, KeyStoreEntry> = IndexMap::new();
    for (alias, entry) in entries {
        map.insert(alias, entry);
    }
    LoadedKeyStore { entries: map }
}

/// The defect this file records, reduced to the storage layer: a keystore
/// holding a secret key had to survive `store` -> `load` with its BYTES,
/// its ALGORITHM and its ALIAS intact. Asserting only "an entry came back"
/// would have passed against the "RAW" bug.
#[test]
fn pkcs12_round_trips_a_secret_key_with_its_algorithm() {
    let key = [
        0u8, 7, 14, 21, 28, 35, 42, 49, 56, 63, 70, 77, 84, 91, 98, 105,
    ];
    let store = store_of(vec![secret_entry("ske", &key, "AES")]);
    let der = write_pkcs12(&store, b"changeit").expect("write");
    let back = load_pkcs12(&der, b"changeit").expect("load");

    assert_eq!(back.entries.len(), 1, "entry count after round trip");
    let entry = back.entries.get("ske").expect("alias preserved");
    match &entry.kind {
        EntryKind::SecretKey {
            key_bytes,
            algorithm,
        } => {
            assert_eq!(key_bytes.as_slice(), &key, "key material");
            assert_eq!(algorithm, "AES", "algorithm name, not the format");
        }
        other => panic!("secret key came back as {other:?}"),
    }
}

/// DESede is the row a guess gets wrong (`1.3.14.3.2.17`, not PKCS#3's
/// `des-EDE3-CBC`), so it is worth its own assertion rather than trusting
/// the AES case to cover the table.
#[test]
fn pkcs12_round_trips_desede_under_the_oid_the_jdk_uses() {
    let store = store_of(vec![secret_entry("des", &[0x5au8; 24], "DESede")]);
    // The identifier itself sits INSIDE the encrypted SecretBag, so assert
    // it at the table rather than by scanning the ciphertext.
    assert_eq!(
        secret_key_alg_oid("DESede")
            .unwrap()
            .components()
            .as_slice(),
        &[1u64, 3, 14, 3, 2, 17]
    );
    let der = write_pkcs12(&store, b"changeit").expect("write");
    let back = load_pkcs12(&der, b"changeit").expect("load");
    match &back.entries.get("des").expect("alias").kind {
        EntryKind::SecretKey { algorithm, .. } => assert_eq!(algorithm, "DESede"),
        other => panic!("came back as {other:?}"),
    }
}

/// A store with all three entry kinds. The private key exercises the
/// shrouded-key-bag + localKeyId pairing, the trusted cert exercises the
/// `trustedKeyUsage` attribute, and the secret key is the reason the whole
/// writer exists.
#[test]
fn pkcs12_round_trips_a_mixed_store() {
    let leaf = b"\x30\x0aLEAFCERT01".to_vec();
    // A well-formed (tiny) PKCS#8 `PrivateKeyInfo`: since gc-common w33-c
    // a shrouded bag opens only to a plaintext that IS one, so the old
    // `\x30\x08PRIVKEY1` placeholder would stay sealed.
    let private_key: &[u8] = &[
        0x30, 0x18, 0x02, 0x01, 0x00, 0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7,
        0x0d, 0x01, 0x01, 0x01, 0x05, 0x00, 0x04, 0x04, 0xde, 0xad, 0xbe, 0xef,
    ];
    let mut entries = IndexMap::new();
    entries.insert(
        "pk".to_string(),
        KeyStoreEntry {
            alias: "pk".to_string(),
            creation_time_ms: 0,
            kind: EntryKind::PrivateKey {
                key_der: private_key.to_vec(),
                chain: vec![leaf.clone()],
                protected: None,
            },
        },
    );
    let (a, e) = cert_entry("ca", b"\x30\x08CACERT01");
    entries.insert(a, e);
    let (a, e) = secret_entry("sk", &[1u8, 2, 3, 4], "HmacSHA256");
    entries.insert(a, e);
    let store = LoadedKeyStore { entries };

    let der = write_pkcs12(&store, b"pw").expect("write");
    let back = load_pkcs12(&der, b"pw").expect("load");
    assert_eq!(back.entries.len(), 3, "aliases: {:?}", back.entries.keys());

    match &back.entries.get("pk").expect("pk").kind {
        EntryKind::PrivateKey { key_der, chain, .. } => {
            assert_eq!(key_der.as_slice(), private_key);
            assert_eq!(chain, &vec![leaf]);
        }
        other => panic!("pk came back as {other:?}"),
    }
    match &back.entries.get("ca").expect("ca").kind {
        EntryKind::TrustedCert { cert_der } => {
            assert_eq!(cert_der.as_slice(), b"\x30\x08CACERT01")
        }
        other => panic!("ca came back as {other:?}"),
    }
    match &back.entries.get("sk").expect("sk").kind {
        EntryKind::SecretKey {
            key_bytes,
            algorithm,
        } => {
            assert_eq!(key_bytes.as_slice(), &[1u8, 2, 3, 4]);
            assert_eq!(algorithm, "HmacSHA256");
        }
        other => panic!("sk came back as {other:?}"),
    }
}

/// The MAC is not decoration: a wrong password must be rejected rather
/// than yielding an empty-looking keystore.
#[test]
fn pkcs12_write_produces_a_mac_that_rejects_the_wrong_password() {
    let store = store_of(vec![secret_entry("ske", &[9u8; 16], "AES")]);
    let der = write_pkcs12(&store, b"right").expect("write");
    assert!(load_pkcs12(&der, b"right").is_ok());
    assert!(matches!(
        load_pkcs12(&der, b"wrong"),
        Err(KeyStoreError::Pkcs12MacFailed)
    ));
}

/// `write_pkcs12` must not emit a file whose secret key would read back as
/// a DIFFERENT algorithm, so an unencodable name is an error, not a guess.
/// The same names a real JDK's `AlgorithmId.get` refuses are refused here.
#[test]
fn pkcs12_refuses_a_secret_key_algorithm_it_cannot_encode() {
    let store = store_of(vec![secret_entry("x", &[0u8; 16], "ChaCha20")]);
    let err = write_pkcs12(&store, b"pw").expect_err("must not invent an OID");
    assert!(
        err.contains("ChaCha20"),
        "message names the algorithm: {err}"
    );
}

/// A name this VM only READS (`AlgorithmId.getName()` is asymmetric for
/// three OIDs) and a bare dotted OID both have to encode, or `load` ->
/// `store` of somebody else's keystore would fail where it used to work.
#[test]
fn secret_key_alg_oid_accepts_read_side_names_and_bare_oids() {
    assert!(secret_key_alg_oid("DES/CBC").is_some());
    assert!(secret_key_alg_oid("RC2/CBC/PKCS5Padding").is_some());
    assert!(secret_key_alg_oid("1.2.840.113549.3.7").is_some());
    assert!(
        secret_key_alg_oid("aes").is_some(),
        "names are case-insensitive"
    );
    assert!(secret_key_alg_oid("not an algorithm").is_none());
    assert!(
        secret_key_alg_oid("9.99.1").is_none(),
        "an OID whose first arc is out of range must not reach the DER writer"
    );
}

#[test]
fn secret_key_alg_name_falls_back_to_the_dotted_oid() {
    let unknown = yasna::models::ObjectIdentifier::from_slice(&[1, 2, 3, 4, 5]);
    assert_eq!(secret_key_alg_name(&unknown), "1.2.3.4.5");
    let aes = yasna::models::ObjectIdentifier::from_slice(&[2, 16, 840, 1, 101, 3, 4, 1]);
    assert_eq!(secret_key_alg_name(&aes), "AES");
}

/// A secret key cannot go into a JKS body at all; it must be the store's
/// format choice that changes, never the entry that disappears.
#[test]
fn jks_still_omits_secret_keys_and_keeps_insertion_order() {
    let mut entries = IndexMap::new();
    let (a, e) = cert_entry("zzz-first", b"\x30\x06DUMMY1");
    entries.insert(a, e);
    let (a, e) = secret_entry("secret", &[1u8; 16], "AES");
    entries.insert(a, e);
    let (a, e) = cert_entry("aaa-second", b"\x30\x06DUMMY2");
    entries.insert(a, e);
    let store = LoadedKeyStore { entries };

    let jks = write_jks(&store, b"pw");
    let back = load_jks(&jks, b"pw").expect("load");
    assert_eq!(
        back.entries.keys().collect::<Vec<_>>(),
        vec!["zzz-first", "aaa-second"],
        "insertion order, not alphabetical, and no secret key"
    );
}
