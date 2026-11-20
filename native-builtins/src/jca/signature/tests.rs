use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn algo_idx_canonical() {
    assert_eq!(algo_idx("SHA256withRSA"), SIG_SHA256_RSA);
    assert_eq!(algo_idx("sha256withrsa"), SIG_SHA256_RSA);
    assert_eq!(algo_idx("SHA256withECDSA"), SIG_SHA256_ECDSA);
    assert_eq!(algo_idx("SHA384withECDSA"), SIG_SHA384_ECDSA);
    assert_eq!(algo_idx("Ed25519"), SIG_ED25519);
    assert_eq!(algo_idx("Ed448"), SIG_ED448);
    assert_eq!(algo_idx("EdDSA"), SIG_EDDSA);
    assert_eq!(algo_idx("nope"), -1);
}

#[test]
fn algo_name_canonical() {
    assert_eq!(algo_name(SIG_SHA256_RSA), "SHA256withRSA");
    assert_eq!(algo_name(SIG_SHA256_ECDSA), "SHA256withECDSA");
    assert_eq!(algo_name(SIG_SHA384_ECDSA), "SHA384withECDSA");
    assert_eq!(algo_name(SIG_ED25519), "Ed25519");
    assert_eq!(algo_name(SIG_ED448), "Ed448");
    assert_eq!(algo_name(SIG_EDDSA), "EdDSA");
    assert_eq!(algo_name(SIG_NONE_RSA), "NONEwithRSA");
}

/// `NONEwithRSA` is recognised by name, is natively dispatched, and — the
/// row that is easy to get wrong — reports SunJCE, not SunRsaSign. It is
/// the one RSA signature name the JDK does not serve from SunRsaSign, and
/// `sig_get_provider_null`'s generic `contains("RSA")` arm answers
/// SunRsaSign for everything it reaches.
#[test]
fn nonewithrsa_is_recognised_and_natively_dispatched() {
    assert_eq!(algo_idx("NONEwithRSA"), SIG_NONE_RSA);
    assert_eq!(algo_idx("nonewithrsa"), SIG_NONE_RSA);
    assert_eq!(algo_idx("NONEWITHRSA"), SIG_NONE_RSA);
    assert!(natively_dispatched(SIG_NONE_RSA));
    assert!(get_instance_offers(0, "NONEwithRSA"));
}

/// The `NONEwithRSA` primitive itself, against its own inverse and against
/// the failure modes SunJCE refuses: a payload too long for the modulus, a
/// signature of the wrong length, and a forged block.
///
/// Cross-checking the BYTES against HotSpot is `probes/NoneWithRsaProbe`,
/// which signs on one VM and verifies on the other — a round trip here
/// cannot catch a padding scheme that is self-consistent and wrong.
#[test]
fn none_with_rsa_round_trips_and_refuses_what_sunjce_refuses() {
    use crate::crypto_impl::Rsa;
    let (pk, sk) = Rsa::generate_keypair(1024);
    let payload = b"32-bytes-of-caller-chosen-digest";
    let sig = Rsa::sign_none(&sk, payload).expect("128-byte modulus fits a 32-byte payload");
    assert_eq!(sig.len(), 128, "signature is modulus-sized");
    assert_eq!(Rsa::verify_none(&pk, payload, &sig), Some(true));
    assert_eq!(
        Rsa::verify_none(&pk, b"a different payload", &sig),
        Some(false),
        "a different payload must be a genuine NO, not a refusal"
    );
    let mut forged = sig.clone();
    forged[100] ^= 0x01;
    assert_eq!(Rsa::verify_none(&pk, payload, &forged), Some(false));
    // k - 11 is the largest payload that fits; one more is a refusal on
    // both sides, not a `false`.
    assert!(Rsa::sign_none(&sk, &vec![0u8; 128 - 11]).is_some());
    assert!(Rsa::sign_none(&sk, &vec![0u8; 128 - 10]).is_none());
    assert_eq!(Rsa::verify_none(&pk, &vec![0u8; 128 - 10], &sig), None);
    assert_eq!(Rsa::verify_none(&pk, payload, &sig[1..]), None);
}

// ---- ML-DSA post-quantum Signature routing ----

/// The umbrella name, the three parameter-set names, and their X.509 OIDs
/// all classify as ML-DSA so `Signature.getInstance` routes them to the real
/// SUN-provider SPI (FIPS 204 sig OIDs 2.16.840.1.101.3.4.3.{17,18,19}).
#[test]
fn algo_idx_recognizes_mldsa_names_and_oids() {
    assert_eq!(algo_idx("ML-DSA"), SIG_MLDSA);
    assert_eq!(algo_idx("ml-dsa-44"), SIG_MLDSA_44);
    assert_eq!(algo_idx("ML-DSA-65"), SIG_MLDSA_65);
    assert_eq!(algo_idx("ML-DSA-87"), SIG_MLDSA_87);
    assert_eq!(algo_idx("2.16.840.1.101.3.4.3.17"), SIG_MLDSA_44);
    assert_eq!(algo_idx("2.16.840.1.101.3.4.3.18"), SIG_MLDSA_65);
    assert_eq!(algo_idx("2.16.840.1.101.3.4.3.19"), SIG_MLDSA_87);
    assert_eq!(algo_name(SIG_MLDSA), "ML-DSA");
    assert_eq!(algo_name(SIG_MLDSA_65), "ML-DSA-65");
}

/// The ECDSA twin of `every_dsa_family_signature_name_maps_to_an_spi_class`,
/// and it exists because the list it replaces had already drifted.
///
/// `seed_sunec_services` carried sixteen hand-written `(name, class)` pairs
/// where HotSpot has twenty; the four `SHA3-*withECDSAinP1363Format` rows
/// were simply missing. Both sides are derived from the same two functions
/// now, so the advertised set and the SPI the engine drives cannot
/// disagree — but only while every name in the list still maps, which is
/// what this asserts.
#[test]
fn every_ecdsa_family_signature_name_maps_to_an_spi_class() {
    for name in ECDSA_FAMILY_SIGNATURE_NAMES {
        assert!(
            ecdsa_family_spi_class(name).is_some(),
            "{name} is advertised and has no SPI class",
        );
        let idx = algo_idx(name);
        assert!(
            idx >= 0,
            "{name} must resolve to an index, or getInstance refuses it",
        );
        assert_eq!(
            ecdsa_family_spi_class(&name.to_ascii_lowercase()),
            ecdsa_family_spi_class(name),
            "{name} must resolve case-insensitively",
        );
    }
    assert_eq!(ECDSA_FAMILY_SIGNATURE_NAMES.len(), 20);
    // Not ECDSA names, and the DSA family in particular must not be
    // captured: `withDSA` and `withECDSA` differ by two characters and the
    // suffix test is what separates them.
    for other in [
        "SHA256withDSA",
        "SHA256withDSAinP1363Format",
        "SHA256withRSA",
        "Ed25519",
        "ECDSA",
    ] {
        assert_eq!(
            ecdsa_family_spi_class(other),
            None,
            "{other} must not be read as an ECDSA-family name",
        );
    }
    // The two encodings are different classes — the reason the
    // `inP1363Format` names route rather than being a flag this engine
    // could apply.
    assert_ne!(
        ecdsa_family_spi_class("SHA256withECDSA"),
        ecdsa_family_spi_class("SHA256withECDSAinP1363Format"),
    );
    // And no two names share a class, which is where a copied arm hides.
    let mut seen = std::collections::HashSet::new();
    for name in ECDSA_FAMILY_SIGNATURE_NAMES {
        assert!(
            seen.insert(ecdsa_family_spi_class(name).unwrap()),
            "{name} shares an SPI class with another name",
        );
    }
}

/// Every PKCS#1 v1.5 RSA name this engine offers maps to exactly one
/// digest, and no two names share it.
///
/// The nine that were advertised-and-refused all had the same failure —
/// no arm — and the shape of the fix (one index per digest, one table for
/// sign and verify) is what makes a future tenth a two-line change. The
/// distinctness half is the one that matters: `SHA-256`, `SHA-512/256` and
/// `SHA3-256` all produce 32 bytes, so a copied arm gives a signature of
/// the right size that verifies against nothing.
#[test]
fn every_pkcs1v15_rsa_name_maps_to_a_distinct_digest() {
    use cratonvm_native_builtins_crypto::signature::DigestAlgorithm as D;
    let names: &[(&str, D)] = &[
        ("MD2withRSA", D::Md2),
        ("MD5withRSA", D::Md5),
        ("SHA1withRSA", D::Sha1),
        ("SHA224withRSA", D::Sha224),
        ("SHA256withRSA", D::Sha256),
        ("SHA384withRSA", D::Sha384),
        ("SHA512withRSA", D::Sha512),
        ("SHA512/224withRSA", D::Sha512_224),
        ("SHA512/256withRSA", D::Sha512_256),
        ("SHA3-224withRSA", D::Sha3_224),
        ("SHA3-256withRSA", D::Sha3_256),
        ("SHA3-384withRSA", D::Sha3_384),
        ("SHA3-512withRSA", D::Sha3_512),
    ];
    let mut seen = std::collections::HashSet::new();
    for (name, want) in names {
        let idx = algo_idx(name);
        assert!(idx >= 0, "{name} has no algo_idx arm");
        assert_eq!(
            rsa_pkcs1_digest(idx),
            Some(*want),
            "{name} resolves to the wrong digest",
        );
        assert!(
            seen.insert(*want),
            "{name} shares a digest with another RSA signature name",
        );
    }
    // The PSS and no-digest names are NOT in this table: they are different
    // padding schemes and must never be reached through it.
    for other in ["RSASSA-PSS", "NONEwithRSA", "MD5andSHA1withRSA"] {
        assert_eq!(
            rsa_pkcs1_digest(algo_idx(other)),
            None,
            "{other} is not a PKCS#1 v1.5 DigestInfo signature",
        );
    }
}

fn every_dsa_family_signature_name_maps_to_an_spi_class() {
    for name in DSA_FAMILY_SIGNATURE_NAMES {
        assert!(
            dsa_family_spi_class(name).is_some(),
            "{name} is advertised and has no SPI class",
        );
        assert_eq!(
            algo_idx(name),
            if *name == "SHA1withDSA" {
                SIG_SHA1_DSA
            } else if *name == "SHA256withDSA" {
                SIG_SHA256_DSA
            } else {
                SIG_DSA_REAL
            },
            "{name} must resolve to a DSA index, or getInstance refuses it",
        );
    }
    // Every spelling the map accepts is advertised under its canonical
    // name. Case and the `SHA-256` hyphenation are accepted as INPUT
    // (JCA lookup is case-insensitive and callers spell both ways) and are
    // deliberately not separate advertised names.
    for name in DSA_FAMILY_SIGNATURE_NAMES {
        assert_eq!(
            dsa_family_spi_class(&name.to_ascii_lowercase()),
            dsa_family_spi_class(name),
            "{name} must resolve case-insensitively",
        );
    }
    assert_eq!(DSA_FAMILY_SIGNATURE_NAMES.len(), 20);
    // Not a DSA name; must not be captured by the suffix test.
    assert_eq!(dsa_family_spi_class("SHA256withECDSA"), None);
    assert_eq!(dsa_family_spi_class("SHA256withRSA"), None);
    assert_eq!(dsa_family_spi_class("DSA"), None);
    // The two encodings are different classes, which is the whole reason
    // the `inP1363Format` names route rather than being a flag.
    assert_ne!(
        dsa_family_spi_class("SHA256withDSA"),
        dsa_family_spi_class("SHA256withDSAinP1363Format"),
    );
}

fn mldsa_spi_class_for_name_maps_parameter_sets() {
    assert_eq!(
        mldsa_spi_class_for_name("ML-DSA-44"),
        Some("sun/security/provider/ML_DSA_Impls$SIG2")
    );
    assert_eq!(
        mldsa_spi_class_for_name("ml-dsa-65"),
        Some("sun/security/provider/ML_DSA_Impls$SIG3")
    );
    assert_eq!(
        mldsa_spi_class_for_name("ML-DSA-87"),
        Some("sun/security/provider/ML_DSA_Impls$SIG5")
    );
    assert_eq!(mldsa_spi_class_for_name("EC"), None);
    assert_eq!(mldsa_spi_class_for_name("ML-KEM-768"), None);
}

/// `is_mldsa` gates the real-SPI route on the ML-DSA indices only (umbrella
/// included), and not on the classical RSA/EC/Ed25519 indices.
#[test]
fn is_mldsa_gates_pqc_indices_only() {
    // Default-on routing (CRATONVM_SYNTHETIC_PQC unset in the test env).
    assert!(is_mldsa(SIG_MLDSA));
    assert!(is_mldsa(SIG_MLDSA_44));
    assert!(is_mldsa(SIG_MLDSA_65));
    assert!(is_mldsa(SIG_MLDSA_87));
    assert!(!is_mldsa(SIG_SHA256_RSA));
    assert!(!is_mldsa(SIG_SHA256_ECDSA));
    assert!(!is_mldsa(SIG_ED25519));
}

/// Build a `Signature` via `getInstance(alg)` and `initSign`/`initVerify` it
/// with `key` (mirrors the public API order). Returns the Signature ref.
fn make_inited_sig(
    ctx: &mut crate::test_utils::MockNativeContext,
    alg: &str,
    key: Option<ObjectRef>,
    verifying: bool,
) -> ObjectRef {
    let name = ctx.create_string(alg);
    let sig = match sig_get_instance(ctx, &[Value::Object(Some(name))])
        .unwrap()
        .unwrap()
    {
        Value::Object(Some(o)) => o,
        other => panic!("getInstance returned {other:?}"),
    };
    let mut args = vec![Value::Object(Some(sig))];
    args.push(Value::Object(key));
    let state = if verifying { STATE_VERIFY } else { STATE_SIGN };
    if key.is_some() {
        if verifying {
            sig_init_verify(ctx, &args).unwrap();
        } else {
            sig_init_sign(ctx, &args).unwrap();
        }
    } else {
        // `initSign(null)` / `initVerify(null)` are now refused with the
        // JDK's `InvalidKeyException: Key must not be null` (`L6JcaSweep`
        // rows 149 and 150), so they can no longer serve as a shortcut to
        // the state these callers actually want: INITED, holding no key.
        // Set that state directly. The refusal under test belongs to
        // `sign()`/`verify()`, not to `initSign()` -- and routing through
        // a call that now throws would have tested the new guard twice
        // and the fail-closed contract not at all.
        set_sig_state(ctx, sig, state);
        sig_slot_set(ctx, sig, SIG_OFF_STATE, Value::Int(state));
        sig_slot_set(ctx, sig, SIG_OFF_PENDING, Value::Int(0));
    }
    sig
}

/// Fail-closed: an ML-DSA `sign()` with no init key (no `SIG_OFF_KEYOBJ`
/// object) must raise rather than silently produce a signature over `b""`.
/// This is the no-synthetic-stubs contract — the synthetic `sign_dispatch`
/// path would have `unwrap_or_default()`-ed to an empty byte[] success.
#[test]
fn mldsa_sign_without_key_fails_closed() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    // initSign with a null key leaves SIG_OFF_KEYOBJ unset.
    let sig = make_inited_sig(&mut ctx, "ML-DSA-65", None, false);
    let err = sig_sign(&mut ctx, &[Value::Object(Some(sig))])
        .expect_err("ML-DSA sign with no key must fail closed");
    // The refusal is the CHECKED `SignatureException` that `sign()`
    // declares, not the unchecked `IllegalStateException` this used to
    // raise. `assert_signature_exception` accepts `throw_jca_exc`'s
    // fallback arm as well, because whether the mock context can build a
    // real JDK class is a property of the mock and not of this refusal —
    // what it will not accept is a success.
    assert_signature_exception(&mut ctx, err);
}

/// Routing proof: an ML-DSA `sign()` WITH an init key is dispatched into the
/// real-SPI drive (`drive_real_mldsa`), NOT the synthetic `sign_dispatch`.
/// The mock SPI's `engineSign` yields no bytes, so the routed result is
/// `Ok(None)` — whereas the synthetic path would have returned
/// `Ok(Some(Object(byte[])))` (an empty-but-present false signature). The
/// `None` therefore distinguishes "took the real route" from "fell through
/// to the synthetic empty-sig stub".
#[test]
fn mldsa_sign_with_key_takes_real_spi_route() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    // A stand-in for a real `ML_DSA_Impls` private key whose getAlgorithm()
    // the mock can't answer — we use the parameter-set-specific algorithm
    // name so the SPI is pinned without needing getAlgorithm().
    let key = match ctx.new_object("java/security/PrivateKey").unwrap().unwrap() {
        Value::Object(Some(o)) => o,
        other => panic!("expected key object, got {other:?}"),
    };
    let sig = make_inited_sig(&mut ctx, "ML-DSA-65", Some(key), false);
    let r = sig_sign(&mut ctx, &[Value::Object(Some(sig))])
        .expect("routed ML-DSA sign should not error in the mock");
    assert!(
        r.is_none(),
        "ML-DSA sign must route to the real SPI (Ok(None) from the mock SPI), \
         not fall through to the synthetic empty-signature stub; got {r:?}"
    );
}

// -----------------------------------------------------------------------
// P0 — "a security API never encodes failure as ordinary output"
//
// Every "must raise" case below has a "must still work" twin using a real
// RSA key, so the suite cannot be satisfied by refusing everything. The
// genuine-negative case (a real signature that really does not match) is
// asserted to stay a `false` with NO exception.
// -----------------------------------------------------------------------

/// One 1024-bit RSA key pair for the whole module — key generation is the
/// expensive part and the tests only need *a* usable key.
fn shared_rsa_key_id() -> u64 {
    use std::sync::OnceLock;
    static ID: OnceLock<u64> = OnceLock::new();
    *ID.get_or_init(|| {
        let (public_key, private_key) = crypto_impl::Rsa::generate_keypair(1024);
        let id = crypto_impl::rsa_key_next_id();
        crypto_impl::rsa_key_store(
            id,
            crypto_impl::RsaKeyPairData {
                public_key,
                private_key,
            },
        );
        id
    })
}

/// A stand-in `Key` object carrying `key_id` in slot 3, which is where
/// `extract_key_id_from_key` reads a synthetic key's handle from.
fn key_object(ctx: &mut crate::test_utils::MockNativeContext, key_id: u64) -> ObjectRef {
    let cid = ctx.ensure_class_initialized("java/security/Key").unwrap();
    let key = ctx.alloc_object(cid, 8);
    ctx.set_field(key, 3, Value::Long(key_id as i64));
    key
}

/// The classification helper must not drift away from the two `match`es it
/// summarises: every algorithm `sign_dispatch` handles must be reported as
/// natively dispatched, and every one it does not must not be.
#[test]
fn natively_dispatched_matches_the_dispatch_arms() {
    // Handled by both dispatch tables.
    for alg in [
        SIG_SHA256_RSA,
        SIG_SHA1_RSA,
        SIG_SHA384_RSA,
        SIG_SHA512_RSA,
        SIG_MD5_SHA1_RSA,
        SIG_NONE_RSA,
        SIG_PSS_SHA256,
        SIG_PSS_SHA384,
        SIG_PSS_SHA512,
        SIG_SHA256_ECDSA,
        SIG_SHA384_ECDSA,
        SIG_ED25519,
    ] {
        assert!(natively_dispatched(alg), "alg {alg} has a dispatch arm");
    }
    // Not handled — these reach the `_ => None` arm and must be reported
    // as such so the refusal message is accurate.
    for alg in [
        SIG_SHA512_ECDSA,
        SIG_SHA256_DSA,
        SIG_SHA1_DSA,
        SIG_MLDSA,
        -1,
    ] {
        assert!(
            !natively_dispatched(alg),
            "alg {alg} has no dispatch arm and must not be claimed as one"
        );
    }
}

#[test]
fn every_rsa_digest_signs_and_verifies_and_the_digests_do_not_cross() {
    // Before this, only SHA256withRSA had a sign arm: `getInstance`
    // advertised SHA-1/384/512 and `MD5andSHA1withRSA`, `initSign`
    // accepted the key, and `sign()` then refused with "this VM has no
    // native implementation for that algorithm". netty's
    // `JdkDelegatingPrivateKeyMethod` asks for all five by name.
    let id = shared_rsa_key_id();
    let msg = b"the message that was signed";
    let algs = [
        SIG_SHA1_RSA,
        SIG_SHA256_RSA,
        SIG_SHA384_RSA,
        SIG_SHA512_RSA,
        SIG_MD5_SHA1_RSA,
    ];
    let mut sigs = Vec::new();
    for alg in algs {
        let sig = sign_dispatch(alg, id, msg)
            .unwrap_or_else(|| panic!("{} must sign", algo_name(alg)));
        assert!(!sig.is_empty(), "{} produced no signature", algo_name(alg));
        assert_eq!(
            sig.len(),
            128,
            "{} must be modulus-length for a 1024-bit key",
            algo_name(alg)
        );
        assert_eq!(
            verify_dispatch(alg, id, msg, &sig),
            Some(true),
            "{} must verify its own signature",
            algo_name(alg)
        );
        assert_eq!(
            verify_dispatch(alg, id, b"a different message", &sig),
            Some(false),
            "{} must reject a different message",
            algo_name(alg)
        );
        sigs.push((alg, sig));
    }
    // The DigestInfo prefix is what separates these algorithms; a shared
    // one would make them interchangeable and every signature meaningless.
    for (a, sig) in &sigs {
        for b in algs {
            if b == *a {
                continue;
            }
            assert_ne!(
                verify_dispatch(b, id, msg, sig),
                Some(true),
                "{} accepted a {} signature",
                algo_name(b),
                algo_name(*a)
            );
        }
    }
}

#[test]
fn a_pss_parameter_spec_reaches_the_signer() {
    // `Signature.setParameter` was a no-op, so PSS signed and verified
    // under the algorithm-name default whatever spec was installed —
    // measured against HotSpot: sign SHA-256/salt-32, verify with
    // SHA-512/salt-64 answered `true` here and `false` there.
    let id = shared_rsa_key_id();
    let msg = b"pss payload";
    let spec32 = PssParams {
        hash: crypto_impl::PssHash::Sha256,
        mgf_hash: crypto_impl::PssHash::Sha256,
        salt_len: 32,
    };
    let spec20 = PssParams {
        salt_len: 20,
        ..spec32
    };
    let sig = sign_dispatch_with(SIG_PSS_SHA256, id, msg, Some(spec32))
        .expect("PSS with an explicit spec must sign");
    assert_eq!(
        verify_dispatch_with(SIG_PSS_SHA256, id, msg, &sig, Some(spec32)),
        Some(true)
    );
    assert_eq!(
        verify_dispatch_with(SIG_PSS_SHA256, id, msg, &sig, Some(spec20)),
        Some(false),
        "a different salt length is a different signature scheme"
    );
    // No spec: the algorithm-name default, unchanged.
    assert_eq!(verify_dispatch(SIG_PSS_SHA256, id, msg, &sig), Some(true));
}

#[test]
fn pss_hash_names_are_read_the_way_jca_spells_them() {
    for (name, want) in [
        ("SHA-256", crypto_impl::PssHash::Sha256),
        ("SHA256", crypto_impl::PssHash::Sha256),
        ("sha-512", crypto_impl::PssHash::Sha512),
        ("SHA-1", crypto_impl::PssHash::Sha1),
        ("SHA-384", crypto_impl::PssHash::Sha384),
    ] {
        assert_eq!(pss_hash_for_jca_name(name), Some(want), "{name}");
    }
    assert_eq!(pss_hash_for_jca_name("SHA3-256"), None);
    assert_eq!(pss_hash_for_jca_name(""), None);
}

/// The `Option` contract the whole fix rests on: `None` means "never
/// checked", `Some(false)` means "checked, and it does not match".
#[test]
fn verify_dispatch_distinguishes_never_checked_from_did_not_match() {
    let id = shared_rsa_key_id();
    let msg = b"the message that was signed";
    let sig = sign_dispatch(SIG_SHA256_RSA, id, msg).expect("a registered key must sign");
    assert!(!sig.is_empty(), "a real signature is never zero-length");

    // MUST STILL WORK.
    assert_eq!(
        verify_dispatch(SIG_SHA256_RSA, id, msg, &sig),
        Some(true),
        "the signature this key just produced must verify"
    );
    // PRESERVED NEGATIVE — a forgery is `Some(false)`, not an error.
    let mut forged = sig.clone();
    forged[0] ^= 0xff;
    assert_eq!(
        verify_dispatch(SIG_SHA256_RSA, id, msg, &forged),
        Some(false)
    );
    assert_eq!(
        verify_dispatch(SIG_SHA256_RSA, id, b"a different message", &sig),
        Some(false)
    );
    // NEVER CHECKED — an unregistered handle and an unsupported algorithm
    // are both `None`, and neither may be collapsed into `false`.
    assert_eq!(verify_dispatch(SIG_SHA256_RSA, u64::MAX, msg, &sig), None);
    assert_eq!(verify_dispatch(SIG_SHA1_DSA, id, msg, &sig), None);
    assert_eq!(sign_dispatch(SIG_SHA256_RSA, u64::MAX, msg), None);
    assert_eq!(sign_dispatch(SIG_SHA1_DSA, id, msg), None);
}

/// MUST STILL WORK, at the native-call level: sign then verify through the
/// registered natives, and confirm a tampered signature comes back as a
/// plain `false` with no exception.
#[test]
fn native_sign_verify_round_trip_and_genuine_mismatch_is_false() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let id = shared_rsa_key_id();

    let signing_key = key_object(&mut ctx, id);
    let signer = make_inited_sig(&mut ctx, "SHA256withRSA", Some(signing_key), false);
    let sig_arr = match sig_sign(&mut ctx, &[Value::Object(Some(signer))]) {
        Ok(Some(Value::Object(Some(a)))) => a,
        other => panic!("sign() must succeed for a registered key, got {other:?}"),
    };
    let sig_bytes = read_byte_array_full(&mut ctx, sig_arr);
    assert!(
        !sig_bytes.is_empty(),
        "sign() must not return an empty array"
    );

    // Genuine positive.
    let verify_key = key_object(&mut ctx, id);
    let verifier = make_inited_sig(&mut ctx, "SHA256withRSA", Some(verify_key), true);
    let good = alloc_byte_array(&mut ctx, &sig_bytes);
    assert_eq!(
        sig_verify(
            &mut ctx,
            &[Value::Object(Some(verifier)), Value::Object(Some(good))]
        )
        .unwrap(),
        Some(Value::Int(1))
    );

    // PRESERVED NEGATIVE: tampered bits are what a forgery looks like.
    // This is the real security decision and must stay a `false`.
    let mut tampered = sig_bytes.clone();
    tampered[0] ^= 0xff;
    let verify_key2 = key_object(&mut ctx, id);
    let verifier2 = make_inited_sig(&mut ctx, "SHA256withRSA", Some(verify_key2), true);
    let bad = alloc_byte_array(&mut ctx, &tampered);
    assert_eq!(
        sig_verify(
            &mut ctx,
            &[Value::Object(Some(verifier2)), Value::Object(Some(bad))]
        )
        .unwrap(),
        Some(Value::Int(0)),
        "a real digest mismatch must remain a `false`, not become an exception"
    );
}

/// MUST RAISE: `verify()` with a key this VM cannot use answered `false`
/// before — indistinguishable, at the call site, from a forged signature.
#[test]
fn native_verify_with_an_unusable_key_raises_instead_of_returning_false() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    // Slot 3 left at 0: no such handle in `crypto_impl`'s RSA key store.
    let verifier = make_inited_sig(&mut ctx, "SHA256withRSA", None, true);
    let sig = alloc_byte_array(&mut ctx, &[1, 2, 3, 4]);
    let err = sig_verify(
        &mut ctx,
        &[Value::Object(Some(verifier)), Value::Object(Some(sig))],
    )
    .expect_err("an unanswerable verify must raise, not report a mismatch");
    assert_signature_exception(&mut ctx, err);
}

/// MUST RAISE: `sign()` used to hand back an empty `byte[]` — a caller
/// storing that ships an unsigned artefact believing it signed one.
#[test]
fn native_sign_with_an_unusable_key_raises_instead_of_returning_an_empty_signature() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let signer = make_inited_sig(&mut ctx, "SHA256withRSA", None, false);
    let err = sig_sign(&mut ctx, &[Value::Object(Some(signer))])
        .expect_err("an unanswerable sign must raise, not return an empty signature");
    assert_signature_exception(&mut ctx, err);
}

/// MUST RAISE: an algorithm with no native arm at all (SHA-512withECDSA is
/// mapped by `algo_idx` but has no `sign_dispatch`/`verify_dispatch` arm)
/// and EC routing off. Uses the dispatch layer directly so the test does
/// not depend on the real-SPI routing flags.
#[test]
fn an_algorithm_with_no_backend_is_never_reported_as_a_mismatch() {
    assert_eq!(
        verify_dispatch(SIG_SHA512_ECDSA, shared_rsa_key_id(), b"m", b"s"),
        None,
        "an algorithm with no backend must not answer the verification question"
    );
}

fn assert_signature_exception(
    ctx: &mut crate::test_utils::MockNativeContext,
    err: cratonvm_types::error::MethodCallFailed,
) {
    use cratonvm_types::error::MethodCallFailed;
    match err {
        MethodCallFailed::ExceptionThrown(exc) => {
            let cid = ctx.class_id_of_object(exc);
            assert_eq!(
                ctx.class_name_arc_of_id(cid).as_deref(),
                Some(SIGNATURE_EXCEPTION),
                "the refusal must be the exception sign()/verify() declare"
            );
        }
        // `throw_jca_exc`'s fallback arm — still loud, still not a value.
        MethodCallFailed::InternalError(e) => {
            let text = format!("{e}");
            assert!(
                text.contains("IllegalArgumentException"),
                "unexpected fallback: {text}"
            );
        }
    }
}
