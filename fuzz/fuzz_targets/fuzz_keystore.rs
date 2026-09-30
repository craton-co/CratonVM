// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Fuzz target for the JKS and PKCS#12 keystore loaders.
//!
//! Surface under test:
//!   * `cratonvm_native_builtins::keystore::load_jks` — JDK 1.2 legacy
//!     format. Header is `FEEDFEED`, body is alias records ending in
//!     an HMAC-SHA1 over the rest of the file.
//!   * `cratonvm_native_builtins::keystore::load_pkcs12` — PKCS#12
//!     (`.p12` / `.pfx`), the modern default. We delegate to the
//!     `p12` crate plus our own DER follow-up for cert/key extraction.
//!   * `cratonvm_native_builtins::keystore::load_keystore` — the
//!     dispatcher that sniffs the magic and routes to one of the above.
//!
//! Keystores reach this code from `KeyStore.load(InputStream, char[])`
//! and from the TLS layer's default-trust-store bootstrap. Both load
//! paths are reachable from any classloaded JAR that asks the JCA
//! layer to validate a chain, so robustness on adversarial bytes is
//! load-bearing.
//!
//! Per-input layout:
//!   * byte 0: dispatch selector (3-way: JKS / PKCS#12 / sniffer).
//!   * bytes 1..N where N <= 32: password (the remainder of the input
//!     is the keystore body). Capping the password keeps the body
//!     under the input budget and matches realistic usage.
//!
//! Run with:
//!   cargo +nightly fuzz run fuzz_keystore

#![no_main]

use libfuzzer_sys::fuzz_target;

use cratonvm_native_builtins::keystore;

/// Practical keystores cluster around 2-10 KiB; 256 KiB is far over the
/// upper end of anything we would see in CI fixtures yet still bounded
/// enough to keep PKCS#12's nested ASN.1 decoders fast.
const MAX_INPUT: usize = 256 * 1024;

/// Bound on the per-input password length. JKS / PKCS#12 password
/// handling is character-derived (UTF-16BE expansion), so the bound
/// also caps PBE derivation cost.
const MAX_PASSWORD: usize = 32;

fuzz_target!(|data: &[u8]| {
    if data.len() < 2 || data.len() > MAX_INPUT {
        return;
    }

    let selector = data[0];
    let rest = &data[1..];

    // Split the input into (password, body). Empty password is fine —
    // both JKS and PKCS#12 accept it.
    let pw_len = (rest[0] as usize).min(MAX_PASSWORD).min(rest.len() - 1);
    let password = &rest[1..1 + pw_len];
    let body = &rest[1 + pw_len..];
    if body.is_empty() {
        return;
    }

    match selector % 3 {
        0 => {
            let _ = keystore::load_jks(body, password);
        }
        1 => {
            let _ = keystore::load_pkcs12(body, password);
        }
        _ => {
            // The dispatcher path: this exercises the magic sniff and
            // routes through one of the two loaders. It is the surface
            // a real `KeyStore.load(InputStream)` caller goes through.
            let _ = keystore::load_keystore(body, password);
        }
    }
});
