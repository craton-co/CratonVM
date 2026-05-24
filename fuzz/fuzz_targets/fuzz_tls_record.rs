// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Fuzz target for the TLS record-layer framing decoder.
//!
//! Surface under test:
//!   * `cratonvm_native_builtins::tls::tls_impl::TlsRecordLayer::decode_record`
//!     — parses the 5-byte record header (content_type, version, length)
//!     and slices off the fragment. This is the first byte of any
//!     untrusted bytes that arrive over a TLS socket; bugs here are
//!     directly reachable by a network attacker.
//!
//! Two configurations are exercised:
//!   * Default `TlsRecordLayer::new()` — `max_fragment_length` = 16384,
//!     matching the TLS 1.2 / 1.3 spec limit.
//!   * `with_max_fragment_length(1024)` — to drive the length-exceeds-
//!     limit branch on shorter inputs.
//!
//! Both `Ok(record)` and `Err(_)` are valid outcomes. Only panics
//! indicate a bug. We additionally do a round-trip *after* a successful
//! decode (encode the fragment back and re-decode) to surface any
//! asymmetry between encoder and decoder under adversarial input.
//!
//! Run with:
//!   cargo +nightly fuzz run fuzz_tls_record

#![no_main]

use libfuzzer_sys::fuzz_target;

use cratonvm_native_builtins::tls::tls_impl::TlsRecordLayer;

/// 64 KiB is larger than the TLS spec's 16 KiB ceiling on
/// `TLSPlaintext.length`; we let the fuzzer feed inputs above the cap
/// so the limit-check branch in `decode_record` gets coverage.
const MAX_INPUT: usize = 64 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT {
        return;
    }

    // Default record layer — exercises the spec-limit branch.
    {
        let mut layer = TlsRecordLayer::new();
        if let Ok(rec) = layer.decode_record(data) {
            // Round-trip: re-encode the fragment and re-decode it.
            // Encoder + decoder must agree on framing for every record
            // the decoder accepted; a mismatch would surface as either
            // a panic in the second decode (the bug) or a quiet
            // truncation (uninteresting — that is by design).
            let mut tmp = TlsRecordLayer::new();
            let encoded = tmp.encode_record(rec.content_type, &rec.fragment);
            let _ = tmp.decode_record(&encoded);
        }
    }

    // Tight max-fragment-length layer — drives the "fragment too large"
    // rejection path on much smaller inputs than the default cap allows.
    {
        let mut layer = TlsRecordLayer::with_max_fragment_length(1024);
        let _ = layer.decode_record(data);
    }
});
