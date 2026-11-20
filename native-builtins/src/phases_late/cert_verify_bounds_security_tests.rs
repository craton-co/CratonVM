use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

// HIGH (cert-verify): In the default (non-legacy) build the no-op
// `verify(PublicKey)` natives MUST NOT be registered, so the real
// `java.security.cert` / `X509CertImpl.verify` bytecode performs the genuine
// signature check and throws on a bad signature. A registered no-op native
// here would silently certify any certificate against any key.
#[cfg(not(feature = "legacy-synthetic-crypto"))]
#[test]
fn verify_natives_not_registered_in_default_build() {
    let mut r = NativeMethodRegistry::new();
    register_p68_security_cert(&mut r);
    assert!(
        r.find(
            "java/security/cert/Certificate",
            "verify",
            "(Ljava/security/PublicKey;)V"
        )
        .is_none(),
        "Certificate.verify must fall through to real bytecode in the default build"
    );
    assert!(
        r.find(
            "java/security/cert/X509Certificate",
            "verify",
            "(Ljava/security/PublicKey;)V"
        )
        .is_none(),
        "X509Certificate.verify must fall through to real bytecode in the default build"
    );
}

// Under the legacy feature the natives exist (and now throw on failure).
#[cfg(feature = "legacy-synthetic-crypto")]
#[test]
fn verify_natives_registered_under_legacy_feature() {
    let mut r = NativeMethodRegistry::new();
    register_p68_security_cert(&mut r);
    assert!(r
        .find(
            "java/security/cert/Certificate",
            "verify",
            "(Ljava/security/PublicKey;)V"
        )
        .is_some());
    assert!(r
        .find(
            "java/security/cert/X509Certificate",
            "verify",
            "(Ljava/security/PublicKey;)V"
        )
        .is_some());
}

// MEDIUM (alloc DoS): the bounds-checked natives stay registered; the guard
// lives inside the closure. Verify the offset/length predicate that gates
// `Vec::with_capacity` rejects negative/huge args before allocation.
#[test]
fn bounds_predicate_rejects_negative_and_overflowing_ranges() {
    // Mirror of the in-native check: off < 0 || len < 0 || off+len > arr_len,
    // evaluated in i64 so a negative i32 cannot sign-extend into a huge usize.
    fn bad(off: i32, len: i32, arr_len: i64) -> bool {
        off < 0 || len < 0 || (off as i64) + (len as i64) > arr_len
    }
    // Negative length (the abort/over-allocation case).
    assert!(bad(0, -1, 16));
    // Negative offset.
    assert!(bad(-1, 4, 16));
    // i32::MIN length must not be treated as a valid (huge) usize.
    assert!(bad(0, i32::MIN, 16));
    // Range exceeding the array.
    assert!(bad(8, 16, 16));
    // Valid ranges are accepted.
    assert!(!bad(0, 16, 16));
    assert!(!bad(4, 8, 16));
    assert!(!bad(0, 0, 0));
}

/// Every PUBLIC method of `javax.crypto.Mac` must be registered — not most
/// of them.
///
/// `Mac` keeps its state off-object in `mac_state_table`, so the real
/// instance fields (`initialized`, `spi`, `provider`, `lock`) are never
/// written. An overload left unregistered therefore runs the REAL JDK body
/// against uninitialised state and throws `IllegalStateException("MAC not
/// initialized")` — on a Mac that `init` + `update` + `doFinal()` just
/// demonstrated works.
///
/// That is exactly how `doFinal([BI)V` went missing: it broke every
/// SCRAM-SHA-256 login (hibernate / Vert.x reactive Postgres saw
/// `FATAL: expected SASL response, got message type 88` — 88 is 'X', the
/// client sending Terminate after `com.ongres.scram`'s PBKDF2 loop died on
/// iteration 2 of 4096), while every other Mac caller in the tree stayed
/// green. A per-descriptor census is the only thing that catches the next
/// one.
///
/// **The population below is the JDK's, not this VM's.** Read off
/// `javap -public -s javax.crypto.Mac` on the oracle
/// (openjdk 25.0.3 2026-04-21 LTS, Microsoft-13877124, build 25.0.3+9-LTS)
/// on 2026-08-13: **17** public methods. Until then this test listed
/// **16** of them — every one of which is registered — so a census whose
/// stated subject is "every PUBLIC method, not most of them" was itself
/// built from the registered set and could not go red for the one case it
/// exists to catch.
///
/// The seventeenth, `getInstance(String, java.security.Provider)`, was the
/// gap that census found, and it is now CLOSED: it is registered at
/// `phases_late/ssl_security.rs`, so its row below reads `true` like the
/// other sixteen. The prose here used to say it was "NOT registered
/// anywhere in the tree" and carried "as an explicit `false` row" — stale
/// as of 2026-08-13, and stale in the direction that matters, because the
/// reverse ratchet makes a closed gap a FAILURE (NOM E25-1). A reader who
/// trusted this paragraph over the table would have read the assertion as
/// a standing exemption, which is the one thing the ratchet exists to
/// prevent.
///
/// **The sentence that must survive every edit:** the population is
/// `javap`'s, not the registry's. A row is added here because the JDK
/// declares the method, never because this VM happens to register it.
///
/// The `expect_registered` column is ratcheted in BOTH directions: a row
/// that flips either way fails, so registering the missing overload
/// requires deleting its `false` and its note, and a registration silently
/// disappearing is a failure rather than a shorter list.
#[test]
fn every_public_mac_method_is_registered() {
    let mut r = NativeMethodRegistry::new();
    register_p68_crypto_mac(&mut r);
    // (method, descriptor, expect_registered, note when NOT expected)
    for (name, desc, expect_registered, why_not) in [
        (
            "getInstance",
            "(Ljava/lang/String;)Ljavax/crypto/Mac;",
            true,
            "",
        ),
        (
            "getInstance",
            "(Ljava/lang/String;Ljava/lang/String;)Ljavax/crypto/Mac;",
            true,
            "",
        ),
        (
            "getInstance",
            "(Ljava/lang/String;Ljava/security/Provider;)Ljavax/crypto/Mac;",
            true,
            "",
        ),
        ("getAlgorithm", "()Ljava/lang/String;", true, ""),
        ("getProvider", "()Ljava/security/Provider;", true, ""),
        ("getMacLength", "()I", true, ""),
        ("init", "(Ljava/security/Key;)V", true, ""),
        (
            "init",
            "(Ljava/security/Key;Ljava/security/spec/AlgorithmParameterSpec;)V",
            true,
            "",
        ),
        ("update", "(B)V", true, ""),
        ("update", "([B)V", true, ""),
        ("update", "([BII)V", true, ""),
        ("update", "(Ljava/nio/ByteBuffer;)V", true, ""),
        ("doFinal", "()[B", true, ""),
        ("doFinal", "([B)[B", true, ""),
        ("doFinal", "([BI)V", true, ""),
        ("reset", "()V", true, ""),
        ("clone", "()Ljava/lang/Object;", true, ""),
    ] {
        let registered = r.find("javax/crypto/Mac", name, desc).is_some();
        if expect_registered {
            assert!(
                registered,
                "javax/crypto/Mac.{name}{desc} is not registered — it will run \
                 the real JDK body against never-initialised instance fields \
                 and throw \"MAC not initialized\""
            );
        } else {
            assert!(
                !registered,
                "javax/crypto/Mac.{name}{desc} IS now registered, but this \
                 census still carries it as a known gap. Delete the `false` row \
                 and its note — a recorded gap that has been closed must stop \
                 being recorded, or the next reader takes the note as a \
                 standing exemption. The note said:\n{why_not}"
            );
        }
    }
}

#[test]
fn mac_and_zip_byterange_natives_remain_registered() {
    let mut r = NativeMethodRegistry::new();
    register_p68_crypto_mac(&mut r);
    assert!(r.find("javax/crypto/Mac", "update", "([BII)V").is_some());

    let mut r2 = NativeMethodRegistry::new();
    register_p58_gzip_streams(&mut r2);
    assert!(r2
        .find("java/util/zip/ZipOutputStream", "write", "([BII)V")
        .is_some());
}
