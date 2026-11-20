use super::*;
use cratonvm_native_api::NativeMethodRegistry;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn shim_registers_all_clinits() {
    let mut r = NativeMethodRegistry::new();
    register_cipher_clinit_shim(&mut r);

    // 2026-04-25: Provider.<clinit> intentionally NOT shimmed any more —
    // the real clinit populates `knownEngines` which Provider$Service.<init>
    // and getEngineName depend on; no-opping it caused chain-NPEs during
    // BouncyCastleProvider.<init>. See `jca/cipher.rs::register_cipher_clinit_shim`.
    for cls in [
        "javax/crypto/Cipher",
        "java/security/Security",
        "sun/security/util/Debug",
        "sun/security/jca/Providers",
    ] {
        assert!(
            r.find(cls, "<clinit>", "()V").is_some(),
            "expected <clinit> shim for {cls}"
        );
    }
    // Provider clinit must NOT be shimmed.
    assert!(
        r.find("java/security/Provider", "<clinit>", "()V")
            .is_none(),
        "Provider.<clinit> must run real bytecode to populate knownEngines"
    );
}

#[test]
fn shim_is_idempotent() {
    let mut r = NativeMethodRegistry::new();
    register_cipher_clinit_shim(&mut r);
    let count_after_first = r.len();
    // Re-register same triples — must not panic on hash collision
    // and must not change the registry size.
    register_cipher_clinit_shim(&mut r);
    assert_eq!(r.len(), count_after_first);
}

#[test]
fn shim_registers_cipher_dispatch() {
    let mut r = NativeMethodRegistry::new();
    register_cipher_clinit_shim(&mut r);

    // Probe-relevant Cipher entry points
    assert!(r
        .find(
            "javax/crypto/Cipher",
            "getInstance",
            "(Ljava/lang/String;)Ljavax/crypto/Cipher;"
        )
        .is_some());
    assert!(r
        .find(
            "javax/crypto/Cipher",
            "init",
            "(ILjava/security/Key;Ljava/security/spec/AlgorithmParameterSpec;)V"
        )
        .is_some());
    assert!(r.find("javax/crypto/Cipher", "doFinal", "([B)[B").is_some());

    // Spec-type constructors
    assert!(r
        .find("javax/crypto/spec/GCMParameterSpec", "<init>", "(I[B)V")
        .is_some());
    assert!(r
        .find(
            "javax/crypto/spec/SecretKeySpec",
            "<init>",
            "([BLjava/lang/String;)V"
        )
        .is_some());
}

fn kw_hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn kw_hexstr(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The three AES key-wrap transformations, against SunJCE's own answers.
///
/// Measured on OpenJDK 25.0.4 with KEK `000102030405060708090a0b0c0d0e0f`
/// via `Cipher.getInstance(t).doFinal(data)` — the entry point that used to
/// raise `IllegalStateException: Cipher mode 'KW' not implemented`.
#[test]
fn aes_key_wrap_matches_sunjce() {
    let kek = kw_hex("000102030405060708090a0b0c0d0e0f");
    let d16 = kw_hex("00112233445566778899aabbccddeeff");
    let d20 = kw_hex("00112233445566778899aabbccddeeff00112233");

    // RFC 3394 proper — this one already worked through `Cipher.wrap`.
    assert_eq!(
        kw_hexstr(&aes_key_wrap(&kek, &d16).unwrap()),
        "1fa68b0a8112b447aef34bd8fb5a7b829d3e862371d2cfe5"
    );
    // PKCS#5 at an EIGHT-byte block size, then RFC 3394.
    assert_eq!(
        kw_hexstr(&aes_key_wrap(&kek, &pkcs5_pad8(&d16)).unwrap()),
        "b05471fa00ab70570ea62b3cfc244f1001af95366e5fe1f430ed8ac55b16c5da"
    );
    assert_eq!(
        kw_hexstr(&aes_key_wrap(&kek, &pkcs5_pad8(&d20)).unwrap()),
        "a694a9bc72fdcf00782dd32f2ed1e75859b7b87730d71efbba4e6a5e5f9bb8bd"
    );
    // RFC 5649.
    assert_eq!(
        kw_hexstr(&aes_key_wrap_with_padding(&kek, &d16).unwrap()),
        "2cef0c9e30de26016c230cb78bc60d51b1fe083ba0c79cd5"
    );
    assert_eq!(
        kw_hexstr(&aes_key_wrap_with_padding(&kek, &d20).unwrap()),
        "23cf017f0dc30969899318b8b400c0eca73290dba36289217fbb33d964653ae9"
    );
}

/// Every wrap round-trips at its ORIGINAL length — the property RFC 5649's
/// length field exists for, and the one a zero-padded unwrap would break.
#[test]
fn aes_key_wrap_round_trips_at_the_original_length() {
    let kek = kw_hex("000102030405060708090a0b0c0d0e0f");
    // KWP accepts any length from 1 up; KW/PKCS5 needs 8 (the pad always
    // adds a block, and RFC 3394 needs two). Both measured on SunJCE.
    for len in [1usize, 7, 8, 9, 15, 16, 20, 24, 31, 32, 64] {
        let data: Vec<u8> = (0..len).map(|i| (i * 13 + 1) as u8).collect();
        let wrapped = aes_key_wrap_with_padding(&kek, &data).unwrap();
        assert_eq!(
            aes_key_unwrap_with_padding(&kek, &wrapped).unwrap(),
            data,
            "KWP len={len}"
        );
        if len >= 8 {
            let p5 = aes_key_wrap(&kek, &pkcs5_pad8(&data)).unwrap();
            assert_eq!(
                pkcs5_unpad8(&aes_key_unwrap(&kek, &p5).unwrap()).unwrap(),
                data,
                "KW/PKCS5 len={len}"
            );
        }
    }
    // The wrapped SIZES are SunJCE's too, and they are what a caller sizing
    // a buffer depends on.
    assert_eq!(
        aes_key_wrap_with_padding(&kek, &[0u8; 1]).unwrap().len(),
        16
    );
    assert_eq!(
        aes_key_wrap_with_padding(&kek, &[0u8; 8]).unwrap().len(),
        16
    );
    assert_eq!(
        aes_key_wrap_with_padding(&kek, &[0u8; 9]).unwrap().len(),
        24
    );
    assert_eq!(
        aes_key_wrap_with_padding(&kek, &[0u8; 16]).unwrap().len(),
        24
    );
    assert_eq!(
        aes_key_wrap_with_padding(&kek, &[0u8; 17]).unwrap().len(),
        32
    );
    assert!(aes_key_wrap_with_padding(&kek, &[]).is_err());
}

/// A tampered wrap is an integrity failure, never a plaintext.
#[test]
fn aes_kwp_rejects_tampering() {
    let kek = kw_hex("000102030405060708090a0b0c0d0e0f");
    let data = kw_hex("00112233445566778899aabbccddeeff00112233");
    let wrapped = aes_key_wrap_with_padding(&kek, &data).unwrap();
    for i in 0..wrapped.len() {
        let mut bad = wrapped.clone();
        bad[i] ^= 1;
        assert!(
            aes_key_unwrap_with_padding(&kek, &bad).is_err(),
            "byte {i} flipped and the unwrap still succeeded"
        );
    }
    // A wrong KEK likewise.
    let mut other = kek.clone();
    other[0] ^= 1;
    assert!(aes_key_unwrap_with_padding(&other, &wrapped).is_err());
}

#[test]
fn parse_transformation_aes_gcm_nopadding() {
    let (cipher, mode, pad) = parse_transformation("AES/GCM/NoPadding");
    assert_eq!(cipher, "AES");
    assert_eq!(mode, "GCM");
    assert!(!pad);
}

#[test]
fn parse_transformation_aes_cbc_pkcs5() {
    let (cipher, mode, pad) = parse_transformation("AES/CBC/PKCS5Padding");
    assert_eq!(cipher, "AES");
    assert_eq!(mode, "CBC");
    assert!(pad);
}

/// `AES/ECB/NoPadding` must parse as "no padding". The ECB arm of
/// `cipher_do_final_impl` used to bind this flag to `_pad` and pad anyway,
/// which made its ciphertext 16 bytes longer than HotSpot's and made its
/// decrypt truncate real plaintext whenever the last byte fell in 1..=16.
#[test]
fn parse_transformation_aes_ecb_nopadding_reports_no_padding() {
    let (cipher, mode, pad) = parse_transformation("AES/ECB/NoPadding");
    assert_eq!(cipher, "AES");
    assert_eq!(mode, "ECB");
    assert!(!pad, "NoPadding must not be reported as padded");
    // A bare `AES` / `AES/ECB` defaults to ECB + PKCS5Padding, as SunJCE does.
    assert!(parse_transformation("AES").2);
    assert_eq!(parse_transformation("AES").1, "ECB");
}

/// The PKCS7 verifier the ECB decrypt arm now delegates to must REFUSE
/// exactly the shapes the deleted inline strip accepted:
///   * a last byte outside 1..=16 (the old code silently stripped nothing
///     and returned the padding as plaintext);
///   * a plausible pad length whose padding BYTES do not all match (the old
///     code never looked at them, so a tampered / wrong-key block decrypted
///     to a plausible plaintext instead of `BadPaddingException`).
#[test]
fn ecb_padding_verifier_rejects_what_the_old_inline_strip_accepted() {
    use crate::phases_early::{pkcs7_pad, pkcs7_unpad};

    // Round-trip still works for every pad length.
    for n in 0..=32usize {
        let msg = vec![0xABu8; n];
        let padded = pkcs7_pad(&msg);
        assert_eq!(padded.len() % 16, 0);
        assert_eq!(pkcs7_unpad(&padded).expect("valid padding"), msg);
    }

    // Last byte 0x00 — old code: `pad >= 1` false, no strip, padding bytes
    // handed back as plaintext.
    let mut zero_tail = vec![0u8; 16];
    zero_tail[15] = 0x00;
    assert!(pkcs7_unpad(&zero_tail).is_err());

    // Last byte 0x11 (17) — out of range, same silent no-strip.
    let mut over = vec![0u8; 16];
    over[15] = 0x11;
    assert!(pkcs7_unpad(&over).is_err());

    // Plausible length, wrong content: claims 4 bytes of padding but only
    // the last one is 0x04. Old code truncated 4 bytes and reported success.
    let mut wrong = vec![0u8; 16];
    wrong[15] = 0x04;
    assert!(pkcs7_unpad(&wrong).is_err());

    // A non-block-multiple buffer is structurally invalid.
    assert!(pkcs7_unpad(&[0u8; 15]).is_err());
}

#[test]
fn aes_key_wrap_matches_rfc3394_vector() {
    // RFC 3394, section 4.1: 128-bit KEK wrapping a 128-bit key data.
    let kek = [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
        0x0e, 0x0f,
    ];
    let plaintext = [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
        0xee, 0xff,
    ];
    let expected = [
        0x1f, 0xa6, 0x8b, 0x0a, 0x81, 0x12, 0xb4, 0x47, 0xae, 0xf3, 0x4b, 0xd8, 0xfb, 0x5a,
        0x7b, 0x82, 0x9d, 0x3e, 0x86, 0x23, 0x71, 0xd2, 0xcf, 0xe5,
    ];

    let wrapped = aes_key_wrap(&kek, &plaintext).expect("RFC vector must wrap");
    assert_eq!(wrapped, expected);
    assert_eq!(
        aes_key_unwrap(&kek, &wrapped).expect("RFC vector must unwrap"),
        plaintext
    );
}

#[test]
fn aes_key_unwrap_rejects_tampered_integrity_value() {
    let kek = [0x11; 16];
    let plaintext = [0x22; 16];
    let mut wrapped = aes_key_wrap(&kek, &plaintext).expect("wrap must succeed");
    wrapped[0] ^= 1;
    assert!(aes_key_unwrap(&kek, &wrapped).is_err());
}

#[test]
fn aes_key_wrap_recognises_sunjce_and_keycloak_names() {
    assert!(is_aes_key_wrap_transformation("AESWrap"));
    assert!(is_aes_key_wrap_transformation("AESWrap_128"));
    assert!(is_aes_key_wrap_transformation("AES/KW/NoPadding"));
    // This predicate delegates to `classify_transformation` and so means
    // "is in the key-wrap FAMILY", which KWP now joins. The distinction
    // between the three schemes lives in `aes_wrap_flavour`, and it is the
    // one the dispatch actually needs: RFC 3394, RFC 3394 over PKCS#5-
    // padded input, and RFC 5649 are three different computations.
    assert!(is_aes_key_wrap_transformation("AES/KWP/NoPadding"));
    assert!(matches!(
        aes_wrap_flavour("AES/KWP/NoPadding"),
        Some(AesWrapFlavour::Kwp)
    ));
    assert!(matches!(
        aes_wrap_flavour("AES/KW/PKCS5Padding"),
        Some(AesWrapFlavour::KwPkcs5)
    ));
    assert!(matches!(
        aes_wrap_flavour("AES/KW/NoPadding"),
        Some(AesWrapFlavour::Kw)
    ));
    assert!(matches!(
        aes_wrap_flavour("AESWrap"),
        Some(AesWrapFlavour::Kw)
    ));
    assert!(aes_wrap_flavour("AES/GCM/NoPadding").is_none());
    assert_eq!(aes_wrap_expected_kek_len("AESWrap_128"), Some(16));
    assert_eq!(aes_wrap_expected_kek_len("AESWrap_192"), Some(24));
    assert_eq!(aes_wrap_expected_kek_len("AESWrap_256"), Some(32));
}

// -----------------------------------------------------------------------
// W7-15 — the admission table.
//
// Every "must raise" below has a measured RED behind it: the transformation
// named produced AES ciphertext on this tree's release binary before the
// table landed. Every "must still work" is a transformation the same binary
// computed correctly and byte-identically to HotSpot 25, so the fix is
// pinned on both sides.
// -----------------------------------------------------------------------

fn refuses_algorithm(t: &str) -> bool {
    matches!(
        classify_transformation(t),
        TransformVerdict::NoSuchAlgorithm
    )
}

fn refuses_padding(t: &str) -> bool {
    matches!(
        classify_transformation(t),
        TransformVerdict::NoSuchPadding(_)
    )
}

/// MUST RAISE. The headline defect: both ChaCha20 names produced
/// AES-256-ECB, byte-identically to `AES/ECB/PKCS5Padding`, with the nonce
/// discarded and no AEAD tag.
#[test]
fn chacha20_is_served_as_chacha20_and_never_as_aes() {
    // This test asserted the opposite for one day, and both verdicts were
    // right in their moment: while nothing computed ChaCha20 the only safe
    // answer was to refuse the name, because admitting it meant AES-256-ECB.
    // `crate::chacha20` (RFC 8439) landed 2026-08-11, so the names are
    // admitted again — and the DISPATCH now keys on the family, which is
    // what makes admitting them safe.
    assert!(!refuses_algorithm("ChaCha20"));
    assert!(!refuses_algorithm("ChaCha20-Poly1305"));
    assert!(!refuses_algorithm("chacha20-poly1305"));
    assert!(!refuses_algorithm("ChaCha20/None/NoPadding"));
    assert!(!refuses_algorithm("ChaCha20-Poly1305/None/NoPadding"));
    // …and they resolve to their OWN families, not to AES. This is the
    // assertion that would have caught the original defect: the name was
    // admitted then and would have passed the four lines above.
    assert!(matches!(
        cipher_family("ChaCha20"),
        Some(CipherFamily::ChaCha20)
    ));
    assert!(matches!(
        cipher_family("ChaCha20-Poly1305"),
        Some(CipherFamily::ChaCha20Poly1305)
    ));
    // A BLOCK-cipher mode on ChaCha20 is still refused: tolerating `ECB`
    // here is exactly how the substitution happened.
    assert!(refuses_algorithm("ChaCha20/ECB/NoPadding"));
    assert!(refuses_algorithm("ChaCha20/CBC/PKCS5Padding"));
    // The one AEAD that was always implemented stays admitted.
    assert!(!refuses_algorithm("AES/GCM/NoPadding"));
}

/// MUST RAISE. It was never only ChaCha20 — `cipher_algorithm_known`
/// accepted a whole catalogue, and every name in it reached the same ECB
/// arm. Measured: `Blowfish` and `RC4` produced the SAME ciphertext as each
/// other from a 16-byte key, because both were AES-128-ECB.
///
/// `Blowfish`, `RC4` and `ARCFOUR` left this list on 2026-08-12 (W7-39) —
/// they are computed now, by the real SunJCE SPI, and
/// `blowfish_and_rc4_are_their_own_ciphers_and_not_aes` is where they went.
/// The rest stay, and the list keeps its name: what it pins is that an
/// algorithm nothing computes is REFUSED, not that these particular thirteen
/// names are forever unimplementable.
#[test]
fn the_other_names_that_were_silently_aes_are_refused_too() {
    for t in [
        "RC2", "IDEA", "SEED", "SM4", "Camellia", "Twofish", "Serpent", "CAST5", "Salsa20",
        "Skipjack", "ECIES", "ElGamal", "NULL",
    ] {
        assert!(
            refuses_algorithm(t),
            "{t} must be refused, not served as AES"
        );
    }
    assert!(refuses_algorithm("CRATONVM-NO-SUCH-CIPHER"));
}

/// The twin of the test above, for the two names that moved off it. This is
/// the assertion that would have caught the ORIGINAL defect: both were
/// admitted then, so "does `getInstance` accept it" proves nothing on its
/// own — what matters is that each resolves to its OWN family, so
/// `cipher_do_final_impl`'s family-keyed dispatch reaches
/// `BlowfishCipher`/`ARCFOURCipher` and never `Aes::key_expansion`.
///
/// Measured on jdk-25.0.3.9-hotspot with a 16-byte key `30..3f` over the
/// 16-byte plaintext `"sixteen byte msg"`, which is the same input
/// `probes/CryptoTrioProbe.java` uses:
///
/// ```text
/// AES/ECB/NoPadding  178c380cadc0514ffe26d8b26351c673
/// Blowfish           33b63e40d662746425f71a69f8cffcdadadae7ffa8950336   (24B, PKCS5)
/// RC4                27ca482b161e3ab93f812659b904df95                   (16B, stream)
/// ```
///
/// Three different algorithms, three different answers. Before this lane all
/// three of those lines read `178c380c…`.
#[test]
fn blowfish_and_rc4_are_their_own_ciphers_and_not_aes() {
    for t in [
        "Blowfish",
        "BLOWFISH",
        "Blowfish/ECB/PKCS5Padding",
        "Blowfish/ECB/NoPadding",
    ] {
        assert!(transformation_is_serviceable(t), "{t} must resolve");
        let (name, _, _) = parse_transformation(t);
        assert!(
            matches!(cipher_family(&name), Some(CipherFamily::Blowfish)),
            "{t} must resolve to the Blowfish family, not to AES"
        );
    }
    for t in [
        "RC4",
        "ARCFOUR",
        "rc4",
        "RC4/ECB/NoPadding",
        "ARCFOUR/ECB/NoPadding",
    ] {
        assert!(transformation_is_serviceable(t), "{t} must resolve");
        let (name, _, _) = parse_transformation(t);
        assert!(
            matches!(cipher_family(&name), Some(CipherFamily::Arcfour)),
            "{t} must resolve to the ARCFOUR family, not to AES"
        );
    }
    // The SPI route each family takes, asserted rather than assumed: a
    // Blowfish transformation driven by `ARCFOURCipher` (or either driven
    // with the other's padding) is a substitution, and the only place that
    // pairing is written down is `real_spi_ecb_route`.
    assert_eq!(
        real_spi_ecb_route(CipherFamily::Blowfish, true),
        Some((
            "com/sun/crypto/provider/BlowfishCipher",
            "PKCS5Padding",
            "Blowfish"
        ))
    );
    assert_eq!(
        real_spi_ecb_route(CipherFamily::Blowfish, false),
        Some((
            "com/sun/crypto/provider/BlowfishCipher",
            "NoPadding",
            "Blowfish"
        ))
    );
    // A stream cipher's padding does not depend on the transformation,
    // because `classify_transformation` admits only `NoPadding` for it.
    for padded in [true, false] {
        assert_eq!(
            real_spi_ecb_route(CipherFamily::Arcfour, padded),
            Some(("com/sun/crypto/provider/ARCFOURCipher", "NoPadding", "RC4"))
        );
    }
    // No other family may reach this driver: every one of them either has
    // its own in-crate path or routes through `drive_real_cipher` with an
    // IV, and both of those SPIs refuse the no-parameters `engineInit` this
    // one calls.
    for f in [
        CipherFamily::Aes,
        CipherFamily::AesFixed(16),
        CipherFamily::AesKeyWrap,
        CipherFamily::DesFamily,
        CipherFamily::Rsa,
        CipherFamily::Pbes2,
        CipherFamily::ChaCha20,
        CipherFamily::ChaCha20Poly1305,
    ] {
        assert!(
            real_spi_ecb_route(f, true).is_none(),
            "{f:?} must not route to the no-parameters SPI driver"
        );
    }
}

/// MUST RAISE. The modes and paddings HotSpot serves for these two families
/// and this engine does not — under-service, refused honestly.
///
/// `Blowfish/CBC/PKCS5Padding` and `Blowfish/CTR/NoPadding` both resolve and
/// encrypt on HotSpot 25 (measured, including the RANDOM IV HotSpot
/// generates and reports through `getIV()`: `0417208096327740` on one run).
/// Admitting them here without implementing the generate-and-report-an-IV
/// contract would encrypt under an all-zero IV and call it success — which
/// is the fabricated-success shape W7-38 measured, not a smaller version of
/// it. `Blowfish/ECB/ISO10126Padding` resolves on HotSpot too and its
/// padding bytes are RANDOM; serving PKCS5 in its place is a substitution.
#[test]
fn blowfish_and_rc4_modes_this_engine_does_not_compute_are_refused() {
    for t in [
        "Blowfish/CBC/PKCS5Padding",
        "Blowfish/CBC/NoPadding",
        "Blowfish/CTR/NoPadding",
        "Blowfish/CFB/NoPadding",
        "Blowfish/OFB/NoPadding",
        "Blowfish/PCBC/PKCS5Padding",
        "RC4/CBC/NoPadding",
    ] {
        assert!(refuses_algorithm(t), "{t} must be refused at getInstance");
    }
    assert!(refuses_padding("Blowfish/ECB/ISO10126Padding"));
    // …and the shapes HotSpot ALSO refuses, so these rows are parity rather
    // than under-service. Measured: `NoSuchAlgorithmException: Cannot find
    // any provider supporting <name>` for each.
    for t in [
        "Blowfish/None/NoPadding",
        "RC4/None/NoPadding",
        "RC4/NONE/NoPadding",
        // A padding on a stream cipher is a missing SERVICE on SunJCE, not a
        // missing padding — the service carries no `SupportedPaddings`
        // beyond NoPadding, so the lookup finds nothing at all and the
        // exception is `NoSuchAlgorithmException`. The two are separately
        // catchable, so which one is thrown is part of the parity.
        "RC4/ECB/PKCS5Padding",
        "ARCFOUR/ECB/PKCS5Padding",
    ] {
        assert!(
            refuses_algorithm(t),
            "{t} must be refused, as HotSpot refuses it"
        );
    }
    // `Blowfish/ECB/PKCS7Padding` is refused by HotSpot as well, and as a
    // PADDING failure here — the algorithm and mode do resolve.
    assert!(refuses_padding("Blowfish/ECB/PKCS7Padding"));
}

/// The key lengths each family accepts, in HotSpot's own wording, checked at
/// `init` where `Cipher.init` declares `InvalidKeyException` — not at
/// `doFinal`, which does not declare it. Every bound measured on
/// jdk-25.0.3.9-hotspot by calling `Cipher.init` with that many bytes.
///
/// The asymmetry is real and is why this is a table and not a rule: a
/// 3-byte Blowfish key is ACCEPTED (SunJCE has no lower bound on it) while
/// a 4-byte RC4 key is refused.
#[test]
fn blowfish_and_rc4_key_lengths_match_hotspot() {
    for n in [3usize, 4, 8, 16, 32, 56] {
        assert!(
            key_length_reason("Blowfish", n).is_none(),
            "HotSpot accepts a {n}-byte Blowfish key"
        );
    }
    assert_eq!(
        key_length_reason("Blowfish", 57).as_deref(),
        Some("Key too long (> 448 bits)")
    );
    for n in [5usize, 16, 128] {
        assert!(
            key_length_reason("RC4", n).is_none(),
            "HotSpot accepts a {n}-byte RC4 key"
        );
    }
    for n in [4usize, 129] {
        assert_eq!(
            key_length_reason("ARCFOUR", n).as_deref(),
            Some("Key length must be between 40 and 1024 bit"),
            "HotSpot refuses a {n}-byte RC4 key at init"
        );
    }
}

/// `getBlockSize()` and `getOutputSize()` must derive the block from the
/// SAME place, which is what `cipher_block_size` is for. They did not:
/// `getOutputSize` hardcoded 16 while `getBlockSize` carried the real table,
/// so the two disagreed for every 64-bit-block cipher and for every stream
/// cipher. Measured on HotSpot: `Blowfish` reports `getBlockSize()=8` and
/// `RC4` reports 0.
#[test]
fn block_size_is_the_ciphers_own_and_stream_ciphers_have_none() {
    assert_eq!(cipher_block_size("Blowfish"), 8);
    assert_eq!(cipher_block_size("Blowfish/ECB/PKCS5Padding"), 8);
    assert_eq!(cipher_block_size("DESede/CBC/PKCS5Padding"), 8);
    assert_eq!(cipher_block_size("RC4"), 0);
    assert_eq!(cipher_block_size("ARCFOUR/ECB/NoPadding"), 0);
    assert_eq!(cipher_block_size("ChaCha20"), 0);
    assert_eq!(cipher_block_size("RSA/ECB/PKCS1Padding"), 0);
    assert_eq!(cipher_block_size("AES/GCM/NoPadding"), 16);
    // An un-inited Cipher has no transformation at all; 0 rather than a
    // default 16, so a caller cannot align on a block this cipher may not
    // have.
    assert_eq!(cipher_block_size(""), 0);
}

/// MUST RAISE. Modes with no implementation here. Each used to be admitted
/// by `getInstance` and then die at `doFinal` on an UNCHECKED
/// `IllegalStateException` naming "WP6.3 dispatch" — the wrong exception,
/// at the wrong call, uncatchable by `catch (GeneralSecurityException)`.
#[test]
fn unimplemented_modes_are_refused_at_getinstance() {
    for t in [
        "AES/CTS/NoPadding",
        "AES/PCBC/PKCS5Padding",
        "AES/CFB8/NoPadding",
        "AES/CCM/NoPadding",
    ] {
        assert!(refuses_algorithm(t), "{t} must be refused at getInstance");
    }
    // `AES/CTR/NoPadding` was on this list until 2026-09-11 and is now
    // COMPUTED — the same move `DESede` made below. SunJCE ships it, so
    // refusing it was this engine disclaiming a transformation every other
    // JCA provider has: `L6JcaSweep` rows 6 and 91 measured
    // `NoSuchAlgorithmException: Cannot find any provider supporting
    // AES/CTR/NoPadding` here against a keystream on HotSpot 25.0.4+7.
    //
    // The padded spelling stays refused: CTR is a stream mode and SunJCE
    // offers no padded form of it.
    assert!(
        !refuses_algorithm("AES/CTR/NoPadding"),
        "AES/CTR/NoPadding is computed now"
    );
    // The padded spelling is refused as a PADDING, not as an algorithm:
    // `aes_padding_verdict` names the padding it will not take, and
    // `classify_transformation` turns that into
    // `NoSuchPaddingException`. That is the JDK's distinction too — the
    // transformation exists, the padding does not go with it — and this
    // assertion said `refuses_algorithm` on the first attempt, which is
    // the wrong half of it.
    assert!(
        refuses_padding("AES/CTR/PKCS5Padding"),
        "CTR is a stream mode: the padding is what is refused, not the mode"
    );
    // `DESede` and `DESede/ECB/PKCS5Padding` were on this list until
    // 2026-08-27 and are now COMPUTED, by the same rule that put
    // `AES/KWP/NoPadding` on the other side: the admission table and
    // `cipher_do_final_impl`'s route were widened together, so ECB is
    // served by ECB rather than by CBC under an ECB name. Asserted here
    // rather than only below, so whoever reads this list sees the move.
    for t in [
        "DESede",
        "DESede/ECB/PKCS5Padding",
        "DESede/ECB/NoPadding",
        "DESede/CBC/PKCS5Padding",
    ] {
        assert!(
            transformation_is_serviceable(t),
            "{t} must be served at getInstance"
        );
    }
    // Still refused, and these are the forms that were NOT measured or
    // that name a mode with no route: one token spelled and not the other,
    // and any mode outside {CBC, ECB}.
    for t in [
        "DESede/CFB/PKCS5Padding",
        "DESede/OFB/PKCS5Padding",
        "DESede/CTR/NoPadding",
    ] {
        assert!(refuses_algorithm(t), "{t} must be refused at getInstance");
    }
    // `AES/KWP/NoPadding` was on this list until 2026-08-11 and is now
    // computed (RFC 5649, `aes_key_wrap_with_padding`), so it belongs on
    // the "must still work" side. Asserted here rather than only there, so
    // that whoever deletes an arm from `classify_transformation` sees the
    // move rather than a silently shorter list.
    assert!(transformation_is_serviceable("AES/KWP/NoPadding"));
    // The size-pinned `AES_128/KWP/...` spelling is deliberately NOT
    // asserted either way: SunJCE's behaviour for it was not measured for
    // this change, and asserting an unmeasured verdict is how a wrong
    // expectation becomes a "requirement".
}

/// MUST RAISE, as a PADDING failure specifically — the JDK distinguishes
/// the two exceptions and a caller may well catch only one. Measured on
/// HotSpot: `AES/CBC/PKCS7Padding` is refused outright, and
/// `AES/CBC/ISO10126Padding` is served with RANDOM padding bytes. This
/// engine implements PKCS#7-as-PKCS5Padding and nothing else, and served
/// BOTH of those as PKCS5.
#[test]
fn unimplemented_paddings_are_refused_not_aliased_onto_pkcs5() {
    assert!(refuses_padding("AES/CBC/PKCS7Padding"));
    assert!(refuses_padding("AES/CBC/ISO10126Padding"));
    assert!(refuses_padding("AES/ECB/CRATONVM-NO-SUCH-PADDING"));
    // AEAD takes NoPadding only.
    assert!(refuses_padding("AES/GCM/PKCS5Padding"));
    // RFC 3394 key wrap takes BOTH, since 2026-08-11: SunJCE's
    // `AES/KW/PKCS5Padding` pads to a multiple of eight and then wraps
    // (measured — a 16-byte payload comes back as 32 bytes, not 24).
    assert!(transformation_is_serviceable("AES/KW/PKCS5Padding"));
    // …and still refuses a padding neither scheme has.
    assert!(refuses_padding("AES/KW/ISO10126Padding"));
    // RSA admits exactly what `RsaCipherPadding::from_transformation` does.
    assert!(refuses_padding("RSA/ECB/NoPadding"));
    assert!(refuses_padding("RSA/ECB/OAEPWithSHA-512AndMGF1Padding"));
    assert!(refuses_algorithm("RSA/None/PKCS1Padding"));
}

/// MUST STILL WORK — the twin every refusal needs. These are the
/// transformations the measured binary computed correctly, several of them
/// byte-identically to HotSpot 25; none may become collateral damage.
#[test]
fn every_transformation_that_worked_before_still_resolves() {
    for t in [
        "AES/GCM/NoPadding",
        "AES",
        "AES/ECB/PKCS5Padding",
        "AES/ECB/NoPadding",
        "AES/CBC/PKCS5Padding",
        "AES/CBC/NoPadding",
        "AES/CFB/PKCS5Padding",
        "AES/OFB/PKCS5Padding",
        "AES/KW/NoPadding",
        "AESWrap",
        "AESWrap_128",
        "AESWrap_192",
        "AESWrap_256",
        "AES_128/GCM/NoPadding",
        "AES_256/CBC/NoPadding",
        "DES/CBC/PKCS5Padding",
        "DESede/CBC/PKCS5Padding",
        "TripleDES/CBC/PKCS5Padding",
        "RSA",
        "RSA/ECB/PKCS1Padding",
        "RSA/ECB/OAEPWithSHA-256AndMGF1Padding",
        "RSA/ECB/OAEPPadding",
        "PBEWithHmacSHA1AndAES_128",
        "PBEWithHmacSHA256AndAES_256",
        // JCA lookup is case-insensitive and the JDK trims each token —
        // measured, `aes / gcm / nopadding` resolves on HotSpot.
        "aes/gcm/nopadding",
        "AES / GCM / NoPadding",
    ] {
        assert!(
            transformation_is_serviceable(t),
            "{t} worked before this lane and must still resolve"
        );
    }
}

/// The tokenizer is the JDK's, messages included — these four shapes were
/// all accepted before, with `parse_transformation` inventing whatever it
/// needed. `"AES/CBC"` became a padded CBC cipher; the JDK calls it invalid.
#[test]
fn malformed_transformations_carry_the_jdk_messages() {
    let msg = |t: &str| match classify_transformation(t) {
        TransformVerdict::InvalidFormat(m) => m,
        _ => panic!("{t} must be an invalid format"),
    };
    assert_eq!(msg("AES/CBC"), "Invalid transformation format:AES/CBC");
    assert_eq!(
        msg("AES/CBC/"),
        "Invalid transformation: missing mode and/or padding-AES/CBC/"
    );
    assert_eq!(
        msg("/CBC/NoPadding"),
        "Invalid transformation: algorithm not specified-/CBC/NoPadding"
    );
    // `getInstance` checks empty BEFORE tokenizing and says so; measured.
    assert_eq!(msg(""), "Null or empty transformation");
    // The JDK's own guard for an algorithm with a `/` inside its NAME: the
    // first slash of `SHA512/224` must not be read as a mode separator.
    assert_eq!(
        tokenize_transformation("PBEWithHmacSHA512/224AndAES_128").unwrap(),
        ("PBEWithHmacSHA512/224AndAES_128".to_string(), None, None)
    );
}

/// `AES_128` means "AES with a 128-bit key". Before this lane the suffix
/// was decorative: measured, `AES_128/GCM/NoPadding` initialised from a
/// 256-bit key and encrypted with AES-256.
#[test]
fn size_pinned_names_pin_the_key_length() {
    assert_eq!(
        transformation_pinned_key_len("AES_128/GCM/NoPadding"),
        Some(16)
    );
    assert_eq!(
        transformation_pinned_key_len("AES_192/CBC/NoPadding"),
        Some(24)
    );
    assert_eq!(
        transformation_pinned_key_len("AES_256/ECB/NoPadding"),
        Some(32)
    );
    assert_eq!(transformation_pinned_key_len("AES/GCM/NoPadding"), None);
    // HotSpot's measured wording, both flavours.
    assert_eq!(
        key_length_reason("AES_128/GCM/NoPadding", 32).as_deref(),
        Some("The key must be 16 bytes")
    );
    assert_eq!(
        key_length_reason("AES/GCM/NoPadding", 17).as_deref(),
        Some("Invalid AES key length: 17 bytes")
    );
    // MUST STILL WORK: the legal sizes, and the families this rule does
    // not govern.
    assert!(key_length_reason("AES_128/GCM/NoPadding", 16).is_none());
    for n in [16, 24, 32] {
        assert!(key_length_reason("AES/CBC/PKCS5Padding", n).is_none());
    }
    assert!(key_length_reason("RSA/ECB/PKCS1Padding", 294).is_none());
    assert!(key_length_reason("PBEWithHmacSHA1AndAES_128", 9).is_none());
    assert!(key_length_reason("DESede/CBC/PKCS5Padding", 24).is_none());
    // An unreadable key is a different diagnosis and must not be reported
    // as a length complaint.
    assert!(key_length_reason("AES/GCM/NoPadding", 0).is_none());
}

/// A bare `AES` really is `AES/ECB/PKCS5Padding` on SunJCE (measured:
/// HotSpot's ciphertext for the two is byte-identical), so that default is
/// kept — but it is now a property of the AES arm rather than of every
/// algorithm name that reaches `parse_transformation`.
#[test]
fn the_ecb_default_is_scoped_to_the_family_that_has_one() {
    assert!(transformation_is_serviceable("AES"));
    // ChaCha20 is serviceable again (RFC 8439 landed 2026-08-11), but the
    // ECB DEFAULT must still not reach it — that default is what turned the
    // name into AES-256-ECB, and it belongs to the AES family alone.
    assert!(transformation_is_serviceable("ChaCha20"));
    assert!(refuses_algorithm("ChaCha20/ECB/NoPadding"));
    // `AES_128` alone is not a service on SunJCE either — measured,
    // `Cipher.getInstance("AES_128")` raises while `AES_128/CBC/NoPadding`
    // resolves — so the default must not manufacture one.
    assert!(refuses_algorithm("AES_128"));
    assert!(transformation_is_serviceable("AES_128/CBC/NoPadding"));
}

#[test]
fn parse_transformation_bare_aes() {
    let (cipher, mode, pad) = parse_transformation("AES");
    assert_eq!(cipher, "AES");
    assert_eq!(mode, "ECB");
    assert!(pad);
}

// -----------------------------------------------------------------------
// P0 — `Cipher.init` must reject a key it cannot use.
//
// The "must raise" case has a "must still work" twin with a real RSA key,
// and a third case proving the guard is scoped to RSA transformations
// only (an AES init with no RSA components is legitimate and unaffected).
// -----------------------------------------------------------------------

/// Set up a synthetic `Cipher` whose recorded transformation is `algo`.
fn cipher_for(
    ctx: &mut crate::test_utils::MockNativeContext,
    algo: &str,
) -> cratonvm_types::ObjectRef {
    let cid = ctx.ensure_class_initialized("javax/crypto/Cipher").unwrap();
    let obj = ctx.alloc_object(cid, 8);
    let tkey = obj_key(ctx, obj);
    with_table_write(|t| {
        t.entry(tkey).or_default().algorithm = algo.to_string();
    });
    obj
}

/// A synthetic `Key` carrying `key_id` in slot 3 (0 = "no handle").
fn key_with_handle(
    ctx: &mut crate::test_utils::MockNativeContext,
    key_id: u64,
) -> cratonvm_types::ObjectRef {
    let cid = ctx.ensure_class_initialized("java/security/Key").unwrap();
    let key = ctx.alloc_object(cid, 8);
    ctx.set_field(key, 3, Value::Long(key_id as i64));
    key
}

// MUST RAISE.
#[test]
fn rsa_cipher_init_with_an_unusable_key_raises_invalid_key() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let cipher_obj = cipher_for(&mut ctx, "RSA/ECB/PKCS1Padding");
    // No `getModulus()` behaviour in the mock and no crypto_impl handle:
    // `rsa_key_components` returns `None`.
    let key = key_with_handle(&mut ctx, 0);
    let err = cipher_init_record(&mut ctx, cipher_obj, 1, key, Vec::new())
        .expect_err("init must reject a key with no usable RSA components");
    match err {
        MethodCallFailed::ExceptionThrown(exc) => {
            let cid = ctx.class_id_of_object(exc);
            assert_eq!(
                ctx.class_name_arc_of_id(cid).as_deref(),
                Some("java/security/InvalidKeyException"),
                "Cipher.init declares InvalidKeyException for exactly this"
            );
        }
        // `throw_jca_exc`'s fallback — still loud, still not a success.
        MethodCallFailed::InternalError(e) => {
            let text = format!("{e}");
            assert!(
                text.contains("IllegalArgumentException"),
                "unexpected fallback: {text}"
            );
        }
    }
    // And the cipher must NOT have been left holding an empty key.
    let tkey = obj_key(&mut ctx, cipher_obj);
    let recorded = with_table_read(|t| t.get(&tkey).map(|s| s.mode).unwrap_or(0));
    assert_eq!(recorded, 0, "a refused init must not record a mode");
}

// MUST STILL WORK — the twin.
#[test]
fn rsa_cipher_init_with_a_real_key_succeeds() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let (public_key, private_key) = crate::crypto_impl::Rsa::generate_keypair(1024);
    let id = crate::crypto_impl::rsa_key_next_id();
    crate::crypto_impl::rsa_key_store(
        id,
        crate::crypto_impl::RsaKeyPairData {
            public_key,
            private_key,
        },
    );
    let cipher_obj = cipher_for(&mut ctx, "RSA/ECB/PKCS1Padding");
    let key = key_with_handle(&mut ctx, id);
    cipher_init_record(&mut ctx, cipher_obj, 1, key, Vec::new())
        .expect("a registered RSA key must initialise the cipher");
    let tkey = obj_key(&mut ctx, cipher_obj);
    let (mode, n_len, e_len) = with_table_read(|t| {
        let s = t.get(&tkey).expect("state recorded");
        (s.mode, s.rsa_n.len(), s.rsa_exp.len())
    });
    assert_eq!(mode, 1);
    assert!(
        n_len > 0 && e_len > 0,
        "the real key components are recorded"
    );
}

// MUST STILL WORK — the guard is RSA-scoped; a symmetric init has no RSA
// components by design and must not be refused.
//
// The key now carries sixteen bytes. It did not, and the test passed
// anyway, because `Cipher.init` accepted a key with no material at all —
// which `L6JcaSweep` row 108 measured as `InvalidKeyException: No
// installed provider supports this key: (null)` on HotSpot 25.0.4+7. A
// key-less AES init is not the thing this test is about, and giving it a
// real key keeps it about the RSA guard.
#[test]
fn non_rsa_cipher_init_is_unaffected_by_the_rsa_key_guard() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let cipher_obj = cipher_for(&mut ctx, "AES/GCM/NoPadding");
    let key = key_with_handle(&mut ctx, 0);
    let bytes = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 16);
    for i in 0..16 {
        ctx.set_array_element(bytes, i, Value::Int(i as i32));
    }
    ctx.set_field(key, 0, Value::Object(Some(bytes)));
    cipher_init_record(&mut ctx, cipher_obj, 1, key, vec![0u8; 12])
        .expect("an AES init must not be caught by the RSA key guard");
    let tkey = obj_key(&mut ctx, cipher_obj);
    assert_eq!(with_table_read(|t| t.get(&tkey).map(|s| s.mode)), Some(1));
}

/// A key with NO material is refused, which is the other half of the same
/// contract and had no test at all.
#[test]
fn a_cipher_init_with_a_key_that_has_no_material_is_refused() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let cipher_obj = cipher_for(&mut ctx, "AES/GCM/NoPadding");
    let key = key_with_handle(&mut ctx, 0);
    assert!(
        cipher_init_record(&mut ctx, cipher_obj, 1, key, vec![0u8; 12]).is_err(),
        "a key whose getEncoded() yields nothing cannot initialise a cipher"
    );
}

/// The CTR counter is one 128-bit big-endian integer, and its CARRY is
/// the part no probe row reaches: the sweep encrypts 20 bytes, which is
/// two blocks, so a carry bug would first show at block 256 — four
/// kilobytes in, in somebody's file.
#[test]
fn the_ctr_counter_carries_across_every_byte() {
    let mut block = [0u8; 16];
    increment_counter_block(&mut block);
    assert_eq!(block[15], 1, "the low byte increments");

    let mut block = [0u8; 16];
    block[15] = 0xff;
    increment_counter_block(&mut block);
    assert_eq!(block[15], 0x00);
    assert_eq!(block[14], 0x01, "the carry reaches the next byte");

    // The all-ones block wraps to zero rather than doing anything else.
    let mut block = [0xffu8; 16];
    increment_counter_block(&mut block);
    assert_eq!(block, [0u8; 16]);

    // A carry that has to cross the whole width.
    let mut block = [0u8; 16];
    block[0] = 0x01;
    for b in block.iter_mut().skip(1) {
        *b = 0xff;
    }
    increment_counter_block(&mut block);
    assert_eq!(block[0], 0x02);
    assert!(block[1..].iter().all(|b| *b == 0));
}

/// And an opmode outside 1..=4, which used to be taken as DECRYPT.
#[test]
fn a_cipher_init_with_a_bad_opmode_is_refused() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let cipher_obj = cipher_for(&mut ctx, "AES/GCM/NoPadding");
    let key = key_with_handle(&mut ctx, 0);
    assert!(
        cipher_init_record(&mut ctx, cipher_obj, 99, key, vec![0u8; 12]).is_err(),
        "opmode 99 is InvalidParameterException, not a silent DECRYPT"
    );
}
