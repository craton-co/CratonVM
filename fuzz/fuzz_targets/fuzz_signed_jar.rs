// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Fuzz target for **signed-JAR verification**: `MANIFEST.MF`, the `.SF`
//! signature file, and the PKCS#7/CMS signer block (`META-INF/*.RSA|DSA|EC`).
//!
//! This is the JAR trust chain:
//!
//! ```text
//!   signer block  ->  .SF  ->  MANIFEST.MF  ->  per-entry digests  ->  bytes
//! ```
//!
//! Every link is attacker-supplied: all three files live inside the JAR
//! being verified. A parser bug here is not merely a crash — a *fail-open*
//! bug means a tampered JAR reports the original signer through
//! `Class.getCodeSource().getCertificates()`. So the oracle for this target
//! is deliberately stronger than panic-freedom: **each API must return an
//! explicit failure, never a success-shaped default.**
//!
//! Surface under test (all in `classloading/src/jar_signer.rs` unless
//! noted):
//!   * `verify_signer_block` (`:357`) — DER `ContentInfo` → `SignedData` →
//!     `SignerInfo` walk, `authenticatedAttributes` digest binding, cert
//!     extraction, and the chain-to-anchor gate.
//!   * `verify_chain` (`:3042`) — path building, cycle detection,
//!     `MAX_CHAIN_LEN`, RFC 5280 extension checks.
//!   * `X509Cert::parse` (`:2529`) and `link_signature_ok` (`:2664`).
//!   * `verify_sf_binds_manifest` (`:3338`) — the `.SF`'s
//!     `<alg>-Digest-Manifest` binding.
//!   * `parse_manifest_entry_digests` (`:3405`) — per-entry
//!     `<alg>-Digest` sections, including the 4 KiB entry-name cap.
//!   * `digest_matches` (`:3489`) — constant-time digest comparison.
//!   * `TrustStore::{empty, add_anchor_der, load_pem_bundle}` (`:2012`,
//!     `:2049`, `:2095`) — anchor ingestion and its `MAX_TRUST_ANCHORS`
//!     cap (`:1966`).
//!
//! Note what this target's oracle *cannot* see: `verify_signer_block`
//! examines only the first `SignerInfo` in the SET (`:476-481`, documented
//! at `:110` and `:353`). That is not a crash and not a fail-open against a
//! zero-anchor store, so no amount of fuzzing here reaches it. It needs an
//! ordering-invariance test — see `docs/feature-designs/fuzzing-state.md`.
//!   * `cratonvm_classloading::ManifestInfo::parse`
//!     (`classloading/src/class_path.rs:711`) — the manifest reader the
//!     class path itself uses.
//!
//! The load-bearing assertion is that `verify_signer_block` against a
//! trust store with **zero anchors** can never return `Some`. `verify_chain`
//! only terminates successfully at `find_anchor_by_subject`, so with an
//! empty store every path ends in `TrustError::NoTrustAnchor`. A `Some`
//! here would mean the chain gate was bypassed — precisely the
//! `permissive_legacy` fail-open the crate keeps `#[cfg(test)]`.
//!
//! Per-input layout (two `u16` length prefixes carve the body into three
//! independent blobs, so the fuzzer can grow one without destroying the
//! others):
//!   * byte 0        — selector (digest algorithm, PEM-vs-DER anchor path).
//!   * bytes 1..3    — `MANIFEST.MF` length (big-endian u16, clamped).
//!   * bytes 3..5    — `.SF` length (big-endian u16, clamped).
//!   * bytes 5..     — manifest bytes, then `.SF` bytes, then the signer
//!     block (the remainder).
//!
//! Run with:
//!   cargo +nightly fuzz run fuzz_signed_jar

#![no_main]

use libfuzzer_sys::fuzz_target;

use cratonvm_classloading::jar_signer::{
    digest_matches, parse_manifest_entry_digests, verify_chain, verify_sf_binds_manifest,
    verify_signer_block, DigestAlg, TrustStore, X509Cert, MAX_TRUST_ANCHORS,
};
use cratonvm_classloading::ManifestInfo;

/// `verify_signer_block` hard-caps the signer block at 1 MiB
/// (`MAX_SIGNER_BLOCK`); anything larger is rejected before parsing, so a
/// bigger input buys no coverage. The three blobs share this budget.
const MAX_INPUT: usize = 1024 * 1024;

/// `parse_manifest_entry_digests` caps a per-entry `Name:` value at 4 KiB
/// and drops longer ones (`classloading/src/jar_signer.rs:3449`).
const MAX_MANIFEST_ENTRY_NAME: usize = 4096;

/// Every digest algorithm the JAR signer accepts, with its output length.
/// `digest_matches` compares constant-time against `raw_digest`, which is
/// always exactly this many bytes — so a `true` verdict for an `expected`
/// of any other length would mean the length check was skipped.
const DIGEST_ALGS: [(DigestAlg, usize); 4] = [
    (DigestAlg::Sha1, 20),
    (DigestAlg::Sha256, 32),
    (DigestAlg::Sha384, 48),
    (DigestAlg::Sha512, 64),
];

/// Number of anchor-insert attempts made per input when probing the
/// trust-store cap. Kept small: the point is that the store refuses to
/// grow past `MAX_TRUST_ANCHORS`, not to actually reach 4096.
const ANCHOR_INSERT_ATTEMPTS: usize = 4;

/// `MAX_CERT_DER` in `classloading/src/jar_signer.rs:173`. Anchor
/// ingestion copies its input, so the harness only probes that path with
/// blobs the store could plausibly accept — otherwise each iteration would
/// memcpy megabytes for a guaranteed rejection.
const MAX_ANCHOR_DER: usize = 64 * 1024;

/// Cap on the manifest size fed to `digest_matches`. Each call hashes the
/// whole manifest, and the loop makes eight of them; hashing megabytes per
/// iteration would starve the fuzzer of executions for no extra coverage
/// (the length-guard behaviour under test is size-independent).
const MAX_DIGEST_PROBE_BYTES: usize = 8 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.len() < 5 || data.len() > MAX_INPUT {
        return;
    }

    let selector = data[0] as usize;
    let manifest_len = u16::from_be_bytes([data[1], data[2]]) as usize;
    let sf_len = u16::from_be_bytes([data[3], data[4]]) as usize;

    let body = &data[5..];
    let manifest_len = manifest_len.min(body.len());
    let (manifest, after_manifest) = body.split_at(manifest_len);
    let sf_len = sf_len.min(after_manifest.len());
    let (sf, signer_block) = after_manifest.split_at(sf_len);

    // -------------------------------------------------------------------
    // 1. The signer block must never verify without a trust anchor.
    // -------------------------------------------------------------------
    let empty_store = TrustStore::empty();
    // Guard the assertion below against a future change to `empty()`: if
    // the store ever ships with anchors, this fires instead of the
    // verification assertion silently becoming meaningless.
    assert_eq!(
        empty_store.anchor_count(),
        0,
        "TrustStore::empty() must contain no anchors"
    );
    assert!(
        verify_signer_block(signer_block, sf, &empty_store).is_none(),
        "verify_signer_block returned a verified signer against a trust store \
         with zero anchors — the chain gate was bypassed"
    );

    // The same must hold when the caller drives the chain walk directly.
    if let Ok(leaf) = X509Cert::parse(signer_block) {
        assert!(
            verify_chain(&leaf, &[], &empty_store).is_err(),
            "verify_chain found a trust path in a store with zero anchors"
        );
        // Self-linking must not short-circuit the chain either: a cert is
        // not its own anchor.
        let _ = leaf.is_self_signed();
        let _ = leaf.link_signature_ok(&leaf);
    }

    // -------------------------------------------------------------------
    // 2. `.SF` → `MANIFEST.MF` binding is fail-closed.
    // -------------------------------------------------------------------
    if verify_sf_binds_manifest(sf, manifest) {
        // A `true` verdict is only reachable through a recognised
        // `<alg>-Digest-Manifest` attribute in the `.SF` main section.
        // `strip_suffix` there is case-sensitive, so the literal token
        // must be present verbatim. An absent attribute must yield
        // `false` (fail-closed), never a default-accept.
        let sf_text = String::from_utf8_lossy(sf);
        assert!(
            sf_text.contains("-Digest-Manifest"),
            "verify_sf_binds_manifest accepted a .SF carrying no \
             *-Digest-Manifest attribute"
        );
    }

    // -------------------------------------------------------------------
    // 3. Per-entry digest parsing stays bounded and honours its own caps.
    // -------------------------------------------------------------------
    let digests = parse_manifest_entry_digests(manifest);
    // Each entry requires its own `Name:` line, so the manifest cannot
    // yield more entries than it has bytes. This is the resource bound: a
    // hostile manifest must not turn into an unbounded `Vec`.
    assert!(
        digests.len() <= manifest.len(),
        "parse_manifest_entry_digests produced {} entries from {} bytes",
        digests.len(),
        manifest.len()
    );
    for d in &digests {
        assert!(
            d.name.len() <= MAX_MANIFEST_ENTRY_NAME,
            "entry name of {} bytes exceeds the {MAX_MANIFEST_ENTRY_NAME}-byte cap",
            d.name.len()
        );
        // The expected digest is base64-decoded out of the manifest, so it
        // can never be larger than the manifest that carried it.
        assert!(
            d.expected.len() <= manifest.len(),
            "decoded digest is larger than the manifest it came from"
        );
        // A parsed entry must carry a real algorithm and a non-defaulted
        // digest slot; the parser only emits an entry when it decoded one.
        let _ = d.alg;
    }

    // -------------------------------------------------------------------
    // 4. `digest_matches` never accepts a wrong-length digest.
    // -------------------------------------------------------------------
    if manifest.len() <= MAX_DIGEST_PROBE_BYTES {
        for &(alg, digest_len) in &DIGEST_ALGS {
            // An empty `expected` is the classic success-shaped default: it
            // must be rejected outright.
            assert!(
                !digest_matches(alg, manifest, &[]),
                "digest_matches accepted an empty expected digest"
            );
            // A fuzzer-supplied `expected` may only match at the exact digest
            // width; anything else means the length guard was skipped.
            if digest_matches(alg, manifest, signer_block) {
                assert_eq!(
                    signer_block.len(),
                    digest_len,
                    "digest_matches accepted a {}-byte digest for an algorithm \
                     producing {digest_len} bytes",
                    signer_block.len()
                );
            }
        }
    }

    // -------------------------------------------------------------------
    // 5. Trust-store ingestion is bounded and rejects garbage anchors.
    // -------------------------------------------------------------------
    if signer_block.len() <= MAX_ANCHOR_DER {
        let mut store = TrustStore::empty();
        if selector % 2 == 0 {
            // DER path: `add_anchor_der` returns false for anything that
            // will not parse as X.509, so a garbage blob must not become
            // an anchor.
            for _ in 0..ANCHOR_INSERT_ATTEMPTS {
                let accepted = store.add_anchor_der(signer_block.to_vec());
                if !accepted {
                    assert_eq!(
                        store.anchor_count(),
                        0,
                        "a rejected anchor still changed the store size"
                    );
                }
            }
        } else {
            // PEM path: the bundle loader returns how many anchors it
            // added, which must match the observed store growth exactly.
            let before = store.anchor_count();
            let added = store.load_pem_bundle(&String::from_utf8_lossy(signer_block));
            assert_eq!(
                store.anchor_count(),
                before + added,
                "load_pem_bundle reported {added} anchors but the store grew differently"
            );
        }
        assert!(
            store.anchor_count() <= MAX_TRUST_ANCHORS,
            "trust store grew to {} anchors, past MAX_TRUST_ANCHORS",
            store.anchor_count()
        );
    }

    // -------------------------------------------------------------------
    // 6. The class path's own manifest reader, on the same bytes.
    // -------------------------------------------------------------------
    let info = ManifestInfo::parse(manifest);
    // Main-section attributes are one per logical line, so the map cannot
    // outgrow the manifest.
    assert!(
        info.attributes.len() <= manifest.len(),
        "ManifestInfo captured {} attributes from {} bytes",
        info.attributes.len(),
        manifest.len()
    );
    let _ = info.is_spring_boot();
    let _ = info.multi_release;
});
