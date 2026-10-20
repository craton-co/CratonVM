// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Fuzz target for the ASN.1 / DER decoders used by the JCA layer.
//!
//! Surface under test:
//!   * `cratonvm_native_builtins::jca::asn1::read_header` — the lowest-
//!     level TLV header walker (tag + length validation).
//!   * `cratonvm_native_builtins::jca::asn1::read_oid` — variable-length
//!     OID arc decoding.
//!   * `cratonvm_native_builtins::jca::asn1::decode_subject_public_key_info`,
//!     `decode_algorithm_identifier`, `decode_extensions` — three of
//!     the highest-level structured DER decoders.
//!   * `cratonvm_native_builtins::x509_manager::parse_certificate` —
//!     the full X.509 certificate parser, which sits on top of the
//!     ASN.1 primitives and exercises the longest decoding chain we
//!     have on untrusted bytes.
//!
//! The decoders are reached from untrusted bytes via:
//!   * `KeyStore` / PKCS#12 / JKS chains (`load_pkcs12`, `load_jks`),
//!   * the TLS handshake (peer certificate validation),
//!   * `java.security.cert.CertificateFactory.generateCertificate`.
//!
//! Per-input we dispatch by the first byte to spread coverage across
//! the decoders; the rest of the input becomes the DER payload.
//!
//! Run with:
//!   cargo +nightly fuzz run fuzz_asn1

#![no_main]

use libfuzzer_sys::fuzz_target;

use cratonvm_native_builtins::jca::asn1;
use cratonvm_native_builtins::x509_manager;

/// 1 MiB is well over any legitimate single ASN.1 structure we might
/// see in the wild — X.509 certs cluster around 1-4 KiB, SPKIs under
/// 1 KiB. The cap keeps libFuzzer's `Vec`-allocation work bounded.
const MAX_INPUT: usize = 1024 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.is_empty() || data.len() > MAX_INPUT {
        return;
    }

    // Spread coverage across the decoder family by selecting on a
    // leading byte. Every branch is `Result`-returning; the oracle is
    // panic-only.
    let selector = data[0];
    let payload = &data[1..];

    match selector % 5 {
        0 => {
            let _ = asn1::read_header(payload);
        }
        1 => {
            let _ = asn1::read_oid(payload);
        }
        2 => {
            let _ = asn1::decode_algorithm_identifier(payload);
        }
        3 => {
            let _ = asn1::decode_subject_public_key_info(payload);
            let _ = asn1::decode_extensions(payload);
        }
        _ => {
            // Full X.509 parse — longest dependency chain on top of
            // the primitives. This is the path TLS peer-cert validation
            // takes for every inbound handshake.
            let _ = x509_manager::parse_certificate(payload);
        }
    }
});
