use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn algorithm_supported_accepts_known_set() {
    for algo in [
        "MD5",
        "md5",
        "SHA-1",
        "SHA1",
        "SHA-224",
        "sha-224",
        "SHA224",
        "SHA-256",
        "SHA256",
        "SHA-384",
        "SHA-512",
        "SHA-512/224",
        "SHA-512/256",
        "sha-512/256",
        "SHA3-224",
        "SHA3-256",
        "SHA3-384",
        "SHA3-512",
        "MD2",
        "md2",
        "SHAKE128-256",
        "shake128-256",
        "SHAKE256-512",
    ] {
        assert!(algorithm_supported(algo), "{algo} should be supported");
    }
}

#[test]
fn algorithm_supported_rejects_unknown() {
    for algo in ["BLAKE2", "Whirlpool", "FAKEHASH", ""] {
        assert!(!algorithm_supported(algo), "{algo} should be rejected");
    }
}

/// The bare `SHAKE128` / `SHAKE256` spellings are `Alg.Alias` rows on
/// HotSpot, and `getInstance` resolves them there. They used to be pinned
/// as REJECTED here, on two premises that were both false: that "the
/// provider chain's alias table [resolves them] before this predicate is
/// consulted" — `md_get_instance` reads no registry, this predicate is its
/// whole gate — and that admitting them would grow
/// `Security.getAlgorithms("MessageDigest")` from 15 to 17, which it
/// cannot, because that set is built from the SERVICE rows by
/// `provider_chain::algorithms_for_service` and never consults this
/// function. Both halves are asserted below, so the pair cannot drift back
/// into "advertised count is right, `getInstance` refuses the name".
/// W7-63-jca-advertise-vs-serve.md §3 #2, alias half.
#[test]
fn the_shake_aliases_resolve_but_are_not_separate_algorithms() {
    for (alias, canonical) in [("SHAKE128", "SHAKE128-256"), ("SHAKE256", "SHAKE256-512")] {
        assert!(
            algorithm_supported(alias),
            "{alias} is an alias of {canonical} and MessageDigest.getInstance must serve it"
        );
        assert_eq!(canonical_algorithm(alias), canonical);
        assert_eq!(digest_length_bytes(alias), digest_length_bytes(canonical));
        assert_eq!(
            compute_digest(canonical_algorithm(alias), b"abc").unwrap(),
            compute_digest(canonical, b"abc").unwrap(),
            "{alias} must produce byte-identical output to {canonical}, which is what \
             HotSpot 25 was measured doing"
        );
    }
    // Anti-vacuity: canonicalisation must be confined to the two aliases.
    assert_eq!(canonical_algorithm("SHA-256"), "SHA-256");
    assert_eq!(canonical_algorithm("FAKEHASH"), "FAKEHASH");
}

/// The gate, the length table and `compute_digest` are three separate
/// match arms over the same set of names, and this campaign's dominant
/// defect is exactly two such lists drifting apart. Assert they agree
/// rather than maintaining them by hand.
#[test]
fn every_supported_algorithm_computes_and_has_a_length() {
    for algo in [
        "MD2",
        "MD5",
        "SHA-1",
        "SHA-224",
        "SHA-256",
        "SHA-384",
        "SHA-512",
        "SHA-512/224",
        "SHA-512/256",
        "SHA3-224",
        "SHA3-256",
        "SHA3-384",
        "SHA3-512",
        "SHAKE128-256",
        "SHAKE256-512",
    ] {
        assert!(algorithm_supported(algo), "{algo} must be supported");
        let want = digest_length_bytes(algo)
            .unwrap_or_else(|| panic!("{algo} is supported but has no declared length"));
        let got = compute_digest(algo, b"advertised must equal served")
            .unwrap_or_else(|_| panic!("{algo} is supported but compute_digest refuses it"));
        assert_eq!(
            got.len(),
            want,
            "{algo}: getDigestLength() says {want} but compute_digest produced {}",
            got.len()
        );
    }
}

/// `compute_digest` normalises by stripping `-` and `/`;
/// `algorithm_supported` / `digest_length_bytes` normalise by filtering to
/// alphanumerics. For the SHAKE names the two happen to agree, which is
/// what makes ONE pair of arm spellings valid in both files. That is a
/// coincidence of these particular names, not a property of the two
/// functions, so it is asserted rather than assumed —
/// W7-29-jca-advertise-implement-gaps-RETIRED-20260917.md asked for exactly this.
#[test]
fn shake_normalisations_agree_across_the_two_filters() {
    for (raw, expected) in [
        ("SHAKE128-256", "SHAKE128256"),
        ("SHAKE256-512", "SHAKE256512"),
        ("SHA-512/256", "SHA512256"),
    ] {
        let strip: String = raw.to_uppercase().replace(['-', '/'], "");
        let alnum: String = raw
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_uppercase();
        assert_eq!(strip, expected, "strip-filter normalisation of {raw}");
        assert_eq!(alnum, expected, "alphanumeric normalisation of {raw}");
    }
}

/// Known-answer vectors, measured on jdk-25.0.3.9-hotspot and recorded in
/// probes/JcaAdvertisedVsServedProbe.expected.txt §C. A round trip cannot
/// catch a wrong algorithm — encrypt-then-decrypt or hash-then-hash with
/// the same wrong function agrees with itself — so the acceptance test for
/// a newly implemented digest has to be a published vector.
#[test]
fn shake_matches_hotspot_vectors() {
    let hex = |h: Vec<u8>| -> String { h.iter().map(|b| format!("{b:02x}")).collect() };
    assert_eq!(
        hex(compute_digest("SHAKE128-256", b"").unwrap()),
        "7f9c2ba4e88f827d616045507605853ed73b8093f6efbc88eb1a6eacfa66ef26"
    );
    assert_eq!(
        hex(compute_digest("SHAKE128-256", b"abc").unwrap()),
        "5881092dd818bf5cf8a3ddb793fbcba74097d5c526a6d35f97b83351940f2cc8"
    );
    assert_eq!(
        hex(compute_digest("SHAKE256-512", b"").unwrap()),
        "46b9dd2b0ba88d13233b3feb743eeb243fcd52ea62b81b82b50c27646ed5762f\
         d75dc4ddd8c0f200cb05019d67b592f6fc821c49479ab48640292eacb3b7c4be"
    );
    assert_eq!(
        hex(compute_digest("SHAKE256-512", b"abc").unwrap()),
        "483366601360a8771c6863080cc4114d8db44530f8f1e1ee4f94ea37e78b5739\
         d5a15bef186a5386c75744c0527e1faa9f8726e462a12a4feb06bd8801e751e4"
    );
}

/// The fallback that made this record necessary. `compute_digest` used to
/// end `_ => Ok(real_sha256(data))`, so an unimplemented name was served
/// SHA-256's bytes under its own label.
///
/// Note the second half. Asserting only "the unknown name errors" would
/// pass on a `compute_digest` that errored on EVERYTHING, which is a
/// probe that cannot fail in the direction that matters. The control
/// establishes that the same call shape succeeds for a real name.
#[test]
fn an_unimplemented_digest_fails_rather_than_answering_sha256() {
    assert!(
        compute_digest("SHA-256", b"abc").is_ok(),
        "control: a real algorithm must still compute, or this test is vacuous"
    );
    for bogus in ["NO-SUCH-DIGEST", "BLAKE2", "Whirlpool", ""] {
        assert!(
            compute_digest(bogus, b"abc").is_err(),
            "{bogus} must be refused, not served SHA-256's bytes"
        );
        assert!(
            digest_length_bytes(bogus).is_none(),
            "{bogus} must have no declared length, not a defaulted 32"
        );
    }
}

#[test]
fn digest_lengths_match_jdk25() {
    assert_eq!(digest_length_bytes("MD2"), Some(16));
    assert_eq!(digest_length_bytes("MD5"), Some(16));
    assert_eq!(digest_length_bytes("SHA-1"), Some(20));
    assert_eq!(digest_length_bytes("SHA-224"), Some(28));
    assert_eq!(digest_length_bytes("SHA3-224"), Some(28));
    assert_eq!(digest_length_bytes("SHA-256"), Some(32));
    assert_eq!(digest_length_bytes("SHA-384"), Some(48));
    assert_eq!(digest_length_bytes("SHA-512"), Some(64));
    assert_eq!(digest_length_bytes("SHA-512/224"), Some(28));
    assert_eq!(digest_length_bytes("SHA-512/256"), Some(32));
    assert_eq!(digest_length_bytes("SHA3-256"), Some(32));
    assert_eq!(digest_length_bytes("SHA3-384"), Some(48));
    assert_eq!(digest_length_bytes("SHA3-512"), Some(64));
    assert_eq!(digest_length_bytes("SHAKE128-256"), Some(32));
    assert_eq!(digest_length_bytes("SHAKE256-512"), Some(64));
}

#[test]
fn compute_digest_matches_hotspot_for_hello_world() {
    // SHA-256 of "hello world" — published HotSpot 25.0.1 result.
    let hash = compute_digest("SHA-256", b"hello world").unwrap();
    let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(
        hex,
        "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
    );
}

#[test]
fn compute_digest_sha224_matches_reference() {
    // SHA-224 / SHA3-224 of "hello world" — Python hashlib reference.
    // Lowercase + dashed spelling exercises the alphanumeric-uppercase
    // normalisation (the `sha-224` Keycloak SD-JWT path).
    let hex = |h: Vec<u8>| -> String { h.iter().map(|b| format!("{b:02x}")).collect() };
    assert_eq!(
        hex(compute_digest("sha-224", b"hello world").unwrap()),
        "2f05477fc24bb4faefd86517156dafdecec45b8ad3cf2522a563582b"
    );
    assert_eq!(
        hex(compute_digest("SHA3-224", b"hello world").unwrap()),
        "dfb7f18c77e928bb56faeb2da27291bd790bc1045cde45f3210bb6c5"
    );
}

#[test]
fn compute_digest_sha512_truncated_matches_fips_vectors() {
    // FIPS 180-4 §5.3.6 truncated SHA-512 variants. The "/256" and "/224"
    // spellings exercise the alphanumeric/slash normalisation. These are
    // the published FIPS example vectors for the one-block "abc" message
    // and the empty input — both distinct from SHA-256/SHA-224 (the whole
    // point of the alternate IV).
    let hex = |h: Vec<u8>| -> String { h.iter().map(|b| format!("{b:02x}")).collect() };
    assert_eq!(
        hex(compute_digest("SHA-512/256", b"abc").unwrap()),
        "53048e2681941ef99b2e29b76b4c7dabe4c2d0c634fc6d46e0e2f13107e7af23"
    );
    assert_eq!(
        hex(compute_digest("SHA-512/256", b"").unwrap()),
        "c672b8d1ef56ed28ab87c3622c5114069bdd3ad7b8f9737498d0c01ecef0967a"
    );
    assert_eq!(
        hex(compute_digest("SHA-512/224", b"abc").unwrap()),
        "4634270f707b6a54daae7530460842e20e37ed265ceee9a43e8924aa"
    );
    assert_eq!(
        hex(compute_digest("SHA-512/224", b"").unwrap()),
        "6ed0dd02806fa89e25de060c19d3ac86cabb87d6a0ddd05c333b84f4"
    );
    // Confirm SHA-512/256 ≠ SHA-256 (alternate IV, not a plain truncation).
    assert_ne!(
        hex(compute_digest("SHA-512/256", b"abc").unwrap()),
        hex(compute_digest("SHA-256", b"abc").unwrap())
    );
}
