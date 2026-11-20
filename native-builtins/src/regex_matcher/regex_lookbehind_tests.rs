use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// The exact pattern Hazelcast's `AbstractXmlConfigHelper.schemaValidation`
/// passes to `String.split` — a negative look-behind whose `\G\S+` body is
/// variable length. Must compile (via the `fancy-regex` fallback) and split
/// identically to JDK 25.
#[test]
fn hazelcast_variable_lookbehind_pattern_compiles_and_splits() {
    let pat = r"(?<!\G\S+)\s";
    let re = compile_java_regex(pat, 0)
        .expect("Hazelcast variable-length look-behind pattern must compile");
    // Must have fallen through to the fancy-regex backend (no look-around
    // in the `regex` crate).
    assert!(
        matches!(re, JavaRegex::Fancy(_)),
        "look-behind pattern should use the fancy-regex backend"
    );

    // Reference values produced by JDK 25 `String.split("(?<!\\G\\S+)\\s")`:
    //   "hello world\tfoo  bar" -> ["hello world", "foo ", "bar"]
    let got = re.split("hello world\tfoo  bar");
    assert_eq!(
        got,
        vec!["hello world", "foo ", "bar"],
        "split must match JDK 25 reference output"
    );

    //   "<root>\n  <child a=\"1\"/>\n</root>"
    //     -> ["<root>\n", "", "<child a=\"1\"/>", "</root>"]
    let got2 = re.split("<root>\n  <child a=\"1\"/>\n</root>");
    assert_eq!(
        got2,
        vec!["<root>\n", "", "<child a=\"1\"/>", "</root>"],
        "split must match JDK 25 reference output"
    );
}

/// A plain variable-length positive look-behind must also compile now.
#[test]
fn variable_length_positive_lookbehind_compiles() {
    let re = compile_java_regex(r"(?<=\d+)x", 0)
        .expect("variable-length positive look-behind must compile");
    // "abc123x" -> the `x` is preceded by digits, so it matches.
    assert!(re.is_match("abc123x"));
    assert!(!re.is_match("abcx"));
}

/// `pem_block_to_der`: a PEM CERTIFICATE block decodes to its DER body;
/// raw DER (no armor) and non-base64 garbage pass through unchanged.
/// Regression guard for http-server-sslengine-identity-singleton-clobber
/// (Netty `SelfSignedCertificate` PEM → empty `getEncoded()`).
#[test]
fn pem_block_to_der_roundtrip() {
    let der = vec![0x30u8, 0x03, 0x02, 0x01, 0x2a]; // trivial DER SEQUENCE
    let b64 = String::from_utf8(b64_encode(&der, B64_VARIANT_BASIC, false)).unwrap();
    let pem = format!("-----BEGIN CERTIFICATE-----\n{b64}\n-----END CERTIFICATE-----\n");
    assert_eq!(pem_block_to_der(pem.as_bytes()), der, "PEM decodes to DER");
    // Raw DER (no armor) passes through untouched.
    assert_eq!(pem_block_to_der(&der), der, "raw DER unchanged");
    // A leading-garbage-then-armor PEM still finds the block.
    let noisy = format!("Bag Attributes\n{pem}");
    assert_eq!(
        pem_block_to_der(noisy.as_bytes()),
        der,
        "armor found past a preamble"
    );
    // Non-base64 body → hand original bytes back (caller's mirror fallback).
    let bad = b"-----BEGIN CERTIFICATE-----\n@@@@\n-----END CERTIFICATE-----\n";
    assert_eq!(
        pem_block_to_der(bad),
        bad.to_vec(),
        "invalid base64 falls back to input"
    );
}
