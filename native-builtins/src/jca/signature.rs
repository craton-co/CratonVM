// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP6.4 — `java.security.Signature` real-JDK natives.
//!
//! ## Probe surface
//!
//! `apps/sig_probe/SigProbe.java` exercises the canonical sign+verify
//! round-trip for two algorithms:
//!
//! * `SHA256withRSA`  — PKCS#1 v1.5 signature on a 2048-bit RSA key.
//! * `SHA256withECDSA`— DER-encoded `(r, s)` signature on a P-256 EC key.
//!
//! Each algorithm is exercised through the public API:
//!
//! ```java
//! Signature s = Signature.getInstance(alg);
//! s.initSign(privKey); s.update(msg); byte[] sig = s.sign();
//! Signature v = Signature.getInstance(alg);
//! v.initVerify(pubKey); v.update(msg); boolean ok = v.verify(sig);
//! ```
//!
//! ## Object layout
//!
//! Five-field synthetic — matches the existing `crypto.rs` shape so the
//! same `crypto_impl::rsa_sign` / `ecdsa_sign_sha256` backends keep
//! working when both modules are loaded:
//!
//! | Slot | Field        | Notes                            |
//! |------|--------------|----------------------------------|
//! |  0   | `algo_idx`  Int | 3=SHA256withRSA, 4=SHA256withECDSA, 7=SHA384withECDSA, 5=Ed25519 |
//! |  1   | `state`     Int | 0=UNINIT, 1=SIGN, 2=VERIFY       |
//! |  2   | `provider`  Int | reserved                         |
//! |  3   | `pending`   Int | bytes accumulated since init     |
//! |  4   | `key_id`    Long  | crypto_impl handle from KPG    |
//!
//! The actual `update(byte[])` payload lives in a process-wide side
//! table keyed on `NativeContext::identity_hash_code(this)` — see
//! `crypto_impl::sig_data_{append,take,clear}_h`.  Identity-hash-code keys
//! survive GC compaction (`HashCodeTable::update_after_gc` in
//! `gc/src/compact_header.rs`).  Pre-C18 the table was keyed on
//! `this.as_ptr() as u64`; a compaction silently orphaned the entry and
//! `sign()` returned a signature over `b""`.

#![allow(clippy::collapsible_if)]

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallResult, RuntimeError};
use cratonvm_types::{ArrayElementType, ClassId, ObjectRef, Value};

use crate::alloc_concurrent_synthetic;
use crate::crypto_impl;

// `java.security.Signature` (JDK 25) extends `SignatureSpi` and declares
// instance fields that overlap our intended synthetic state. To avoid the
// VM's descriptor-aware `set_field` coercing our `Value::Int(...)` writes
// into `Object(None)` on slots the real class declares as references, our
// synthetic state is appended *after* the real-layout field count.
//
// Offsets are relative to `synthetic_base_offset(...)`.
fn synthetic_base_offset(ctx: &mut dyn NativeContext, class_name: &str) -> usize {
    let cid = ctx
        .ensure_class_initialized(class_name)
        .unwrap_or(ClassId::new(0));
    ctx.class_num_total_fields(cid)
}

const SIG_OFF_ALGO: usize = 0;
const SIG_OFF_STATE: usize = 1;
const SIG_OFF_PROVIDER: usize = 2;
const SIG_OFF_PENDING: usize = 3;
const SIG_OFF_KEYID: usize = 4;
// Slot 5 holds the real EC key object (ECPrivateKeyImpl / ECPublicKeyImpl) for
// the SunEC ECDSA drive path (`crate::route_ec_to_real`); the GC scans synthetic
// object slots, so the ref stays live/forwarded across init→update→sign.
const SIG_OFF_KEYOBJ: usize = 5;
const SIG_PRIVATE_SLOTS: usize = 6;

// ---------------------------------------------------------------------------
// SigProbe fix: process-wide side tables for Signature algorithm / state /
// key id.  Real-JDK `Signature` is allocated against the loaded class whose
// inherited layout makes raw-slot writes of `Value::Int` collide with
// `Object`-typed slots (`engine`, `provider`, …), silently coercing reads to
// `Object(None)`.  Side tables keyed on the receiver's identity carry the
// state reliably across `getInstance` → `init*` → `sign`/`verify`.
//
// C15 fix: keys are `i32` identity-hash-code, NOT `ObjectRef`.  `ObjectRef`'s
// `Hash` impl hashes the raw pointer (`self.ptr.as_ptr() as usize`,
// `types/src/value.rs`).  When the GC relocates a `Signature` instance during
// compaction every entry becomes orphaned: `Signature.sign()` after GC then
// reports `state == 0` (`UNINIT`) from the side-table miss and the fallback
// slot read also returns `Object(None)`, throwing `IllegalStateException`.
// `NativeContext::identity_hash_code` is GC-stable
// (`HashCodeTable::update_after_gc`, `gc/src/compact_header.rs`).  All
// accessors therefore thread `&mut dyn NativeContext`.  Mirrors the pattern
// from `lang_invoke::VH_META_TABLE` (`native-builtins/src/lang_invoke.rs`).
// ---------------------------------------------------------------------------

fn sig_algo_table() -> &'static parking_lot::Mutex<rustc_hash::FxHashMap<i32, i32>> {
    use std::sync::OnceLock;
    static T: OnceLock<parking_lot::Mutex<rustc_hash::FxHashMap<i32, i32>>> = OnceLock::new();
    T.get_or_init(|| parking_lot::Mutex::new(rustc_hash::FxHashMap::default()))
}

fn sig_state_table() -> &'static parking_lot::Mutex<rustc_hash::FxHashMap<i32, i32>> {
    use std::sync::OnceLock;
    static T: OnceLock<parking_lot::Mutex<rustc_hash::FxHashMap<i32, i32>>> = OnceLock::new();
    T.get_or_init(|| parking_lot::Mutex::new(rustc_hash::FxHashMap::default()))
}

fn sig_keyid_table() -> &'static parking_lot::Mutex<rustc_hash::FxHashMap<i32, u64>> {
    use std::sync::OnceLock;
    static T: OnceLock<parking_lot::Mutex<rustc_hash::FxHashMap<i32, u64>>> = OnceLock::new();
    T.get_or_init(|| parking_lot::Mutex::new(rustc_hash::FxHashMap::default()))
}

fn set_sig_algo(ctx: &mut dyn NativeContext, this: ObjectRef, idx: i32) {
    let key = ctx.identity_hash_code(this);
    sig_algo_table().lock().insert(key, idx);
}
fn get_sig_algo(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<i32> {
    let key = ctx.identity_hash_code(this);
    sig_algo_table().lock().get(&key).copied()
}
fn set_sig_state(ctx: &mut dyn NativeContext, this: ObjectRef, st: i32) {
    let key = ctx.identity_hash_code(this);
    sig_state_table().lock().insert(key, st);
}
fn get_sig_state(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<i32> {
    let key = ctx.identity_hash_code(this);
    sig_state_table().lock().get(&key).copied()
}
fn set_sig_keyid(ctx: &mut dyn NativeContext, this: ObjectRef, kid: u64) {
    let key = ctx.identity_hash_code(this);
    sig_keyid_table().lock().insert(key, kid);
}
fn get_sig_keyid(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<u64> {
    let key = ctx.identity_hash_code(this);
    sig_keyid_table().lock().get(&key).copied()
}

const STATE_UNINIT: i32 = 0;
const STATE_SIGN: i32 = 1;
const STATE_VERIFY: i32 = 2;

// Algorithm index — matches crypto.rs SIG_ALGORITHMS layout.
const SIG_SHA256_RSA: i32 = 3;
const SIG_SHA384_ECDSA: i32 = 4;
const SIG_ED25519: i32 = 5;
const SIG_SHA256_DSA: i32 = 6;
// Wave 6 additions — kept >= 7 so the existing `mldsa_sig_bytes` table
// in crypto.rs continues to identify the legacy algorithms by index.
const SIG_SHA256_ECDSA: i32 = 7;
const SIG_SHA384_RSA: i32 = 8;
const SIG_SHA512_RSA: i32 = 9;
const SIG_SHA1_RSA: i32 = 10;
// SHA512withECDSA (ES512, typically P-521). Real SunEC drive only — there is no
// synthetic crypto_impl fallback for it.
const SIG_SHA512_ECDSA: i32 = 11;
// RSASSA-PSS (JWA PS256/PS384/PS512). signature.rs-local indices > 6 so they
// never collide with crypto.rs's `SIG_ALGORITHMS` (0..=6). Verified natively via
// `crypto_impl::rsa_verify_pss_by_id` (the BC PSS SPI needs the provider list).
const SIG_PSS_SHA256: i32 = 12;
const SIG_PSS_SHA384: i32 = 13;
const SIG_PSS_SHA512: i32 = 14;
// Post-quantum ML-DSA (FIPS 204). CratonVM has no native lattice signature
// crypto, so these are sign/verify-routed to the real JDK 25 SUN-provider SPI
// (`sun.security.provider.ML_DSA_Impls$SIG{2,3,5}`) behind
// `crate::route_pqc_to_real()` — the exact mirror of the SunEC ECDSA route, and
// the companion to the ML-DSA *keygen*/keyfactory routing already in
// `jca::key_factory` (`pqc_spi_classes`). `SIG_MLDSA` is the umbrella name
// (`Signature.getInstance("ML-DSA")`), where the concrete parameter set is
// resolved from the init key's `getAlgorithm()`.
const SIG_MLDSA: i32 = 15;
const SIG_MLDSA_44: i32 = 16;
const SIG_MLDSA_65: i32 = 17;
const SIG_MLDSA_87: i32 = 18;
const SIG_ED448: i32 = 19;
const SIG_EDDSA: i32 = 20;
// DSA with SHA-1 (the classic jarsigner default, `Signature.getInstance("DSA")`
// / `"SHA1withDSA"`). CratonVM has no native DSA crypto (`crypto_impl` never
// had a DSA path — `verify_dispatch`/`sign_dispatch` fall through to `_ =>
// None`/`unwrap_or(false)`, so DSA verification always silently failed, no
// exception). Routed to the real JDK SPI below, same as ECDSA/EdDSA/ML-DSA.
const SIG_SHA1_DSA: i32 = 21;

fn algo_idx(name: &str) -> i32 {
    let upper = name.to_ascii_uppercase();
    match upper.as_str() {
        "SHA256WITHRSA" | "SHA-256WITHRSA" | "RSASSA-PKCS1-V1_5_WITH_SHA-256" => SIG_SHA256_RSA,
        "SHA384WITHRSA" => SIG_SHA384_RSA,
        "SHA512WITHRSA" => SIG_SHA512_RSA,
        "SHA1WITHRSA" | "SHA-1WITHRSA" => SIG_SHA1_RSA,
        // RSASSA-PSS (JWA PS256/384/512). keycloak's `JavaAlgorithm` resolves
        // these to BouncyCastle's `SHA{256,384,512}withRSAandMGF1`; accept the
        // `/PSS` aliases too. (Bare "RSASSA-PSS" carries its hash in a
        // PSSParameterSpec we don't see here; default it to SHA-256.)
        "SHA256WITHRSAANDMGF1" | "SHA256WITHRSA/PSS" | "SHA-256WITHRSA/PSS" | "RSASSA-PSS" => {
            SIG_PSS_SHA256
        }
        "SHA384WITHRSAANDMGF1" | "SHA384WITHRSA/PSS" | "SHA-384WITHRSA/PSS" => SIG_PSS_SHA384,
        "SHA512WITHRSAANDMGF1" | "SHA512WITHRSA/PSS" | "SHA-512WITHRSA/PSS" => SIG_PSS_SHA512,
        "SHA256WITHECDSA" | "SHA-256WITHECDSA" => SIG_SHA256_ECDSA,
        "SHA384WITHECDSA" | "SHA-384WITHECDSA" => SIG_SHA384_ECDSA,
        "SHA512WITHECDSA" | "SHA-512WITHECDSA" => SIG_SHA512_ECDSA,
        "ED25519" => SIG_ED25519,
        "ED448" => SIG_ED448,
        "EDDSA" => SIG_EDDSA,
        "SHA256WITHDSA" | "SHA-256WITHDSA" => SIG_SHA256_DSA,
        "SHA1WITHDSA" | "SHA-1WITHDSA" | "DSA" | "DSS" => SIG_SHA1_DSA,
        // Post-quantum ML-DSA (FIPS 204). The umbrella "ML-DSA" name carries no
        // parameter set; the concrete SPI suffix is resolved from the init key's
        // `getAlgorithm()` (see `mldsa_spi_class`). The explicit param-set names
        // (and their dotted OIDs, 2.16.840.1.101.3.4.3.{17,18,19}) pin the SPI
        // directly.
        "ML-DSA" => SIG_MLDSA,
        "ML-DSA-44" | "2.16.840.1.101.3.4.3.17" => SIG_MLDSA_44,
        "ML-DSA-65" | "2.16.840.1.101.3.4.3.18" => SIG_MLDSA_65,
        "ML-DSA-87" | "2.16.840.1.101.3.4.3.19" => SIG_MLDSA_87,
        // Signature-algorithm OIDs. X.509 `cert.verify()` resolves
        // `Signature.getInstance(signatureAlgorithm.getId())` by OID, not the
        // friendly name (e.g. BC's `X509CertificateObject.verify()`); without
        // these the lookup returned -1 and EC/RSA cert verification silently
        // returned false ("certificate does not verify with supplied key").
        // ecdsa-with-SHA*:
        "1.2.840.10045.4.3.2" => SIG_SHA256_ECDSA,
        "1.2.840.10045.4.3.3" => SIG_SHA384_ECDSA,
        "1.2.840.10045.4.3.4" => SIG_SHA512_ECDSA,
        // sha*WithRSAEncryption:
        "1.2.840.113549.1.1.5" => SIG_SHA1_RSA,
        "1.2.840.113549.1.1.11" => SIG_SHA256_RSA,
        "1.2.840.113549.1.1.12" => SIG_SHA384_RSA,
        "1.2.840.113549.1.1.13" => SIG_SHA512_RSA,
        // Ed25519:
        "1.3.101.112" => SIG_ED25519,
        // id-dsa-with-sha1 / dsaWithSHA256:
        "1.2.840.10040.4.3" => SIG_SHA1_DSA,
        "2.16.840.1.101.3.4.3.2" => SIG_SHA256_DSA,
        _ => -1,
    }
}

fn algo_name(idx: i32) -> &'static str {
    match idx {
        SIG_SHA256_RSA => "SHA256withRSA",
        SIG_SHA384_RSA => "SHA384withRSA",
        SIG_SHA512_RSA => "SHA512withRSA",
        SIG_SHA1_RSA => "SHA1withRSA",
        SIG_SHA384_ECDSA => "SHA384withECDSA",
        SIG_SHA256_ECDSA => "SHA256withECDSA",
        SIG_SHA512_ECDSA => "SHA512withECDSA",
        SIG_ED25519 => "Ed25519",
        SIG_ED448 => "Ed448",
        SIG_EDDSA => "EdDSA",
        SIG_SHA256_DSA => "SHA256withDSA",
        SIG_SHA1_DSA => "SHA1withDSA",
        SIG_PSS_SHA256 => "SHA256withRSAandMGF1",
        SIG_PSS_SHA384 => "SHA384withRSAandMGF1",
        SIG_PSS_SHA512 => "SHA512withRSAandMGF1",
        SIG_MLDSA => "ML-DSA",
        SIG_MLDSA_44 => "ML-DSA-44",
        SIG_MLDSA_65 => "ML-DSA-65",
        SIG_MLDSA_87 => "ML-DSA-87",
        _ => "Unknown",
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn this_arg(args: &[Value]) -> Result<ObjectRef, cratonvm_types::error::MethodCallFailed> {
    match args.first() {
        Some(Value::Object(Some(o))) => Ok(*o),
        _ => Err(RuntimeError::NullPointerException {
            message: Some("Signature: this is null".into()),
        }
        .into()),
    }
}

fn read_string(ctx: &mut dyn NativeContext, args: &[Value], idx: usize) -> String {
    match args.get(idx) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    }
}

fn read_byte_array_full(ctx: &mut dyn NativeContext, arr: ObjectRef) -> Vec<u8> {
    let len = ctx.array_length(arr);
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        if let Value::Int(b) = ctx.get_array_element(arr, i) {
            out.push(b as u8);
        }
    }
    out
}

fn read_byte_array_range(
    ctx: &mut dyn NativeContext,
    arr: ObjectRef,
    off: usize,
    len: usize,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let arr_len = ctx.array_length(arr);
    let end = (off + len).min(arr_len);
    for i in off..end {
        if let Value::Int(b) = ctx.get_array_element(arr, i) {
            out.push(b as u8);
        }
    }
    out
}

fn alloc_byte_array(ctx: &mut dyn NativeContext, bytes: &[u8]) -> ObjectRef {
    let arr = ctx.new_array(ArrayElementType::Byte, bytes.len());
    for (i, &b) in bytes.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
    }
    arr
}

fn key_id_of(ctx: &mut dyn NativeContext, this: ObjectRef) -> u64 {
    if let Some(kid) = get_sig_keyid(ctx, this) {
        if kid != 0 {
            return kid;
        }
    }
    let base = synthetic_base_offset(ctx, "java/security/Signature");
    match ctx.get_field(this, base + SIG_OFF_KEYID) {
        Value::Long(id) => id as u64,
        Value::Int(id) => id as u64,
        _ => 0,
    }
}

fn extract_key_id_from_key(ctx: &mut dyn NativeContext, key: ObjectRef) -> u64 {
    // Real RSA keys (`sun.security.rsa.RSAPublic/PrivateKeyImpl`, handed out when
    // `route_rsa_to_real()` is on) carry no synthetic `key_id` slot — slot 3 is a
    // real field (e.g. a BigInteger ref). They're bridged to their crypto_impl
    // `key_id` via the GC-stable identity map registered at keygen/import, so the
    // fast Rust sign/verify still applies. Check that FIRST; a synthetic key is
    // never in the map and falls through to its slot-3 `key_id`.
    if let Some(id) = crypto_impl::rsa_realkey_map_get(ctx.identity_hash_code(key)) {
        return id;
    }
    match ctx.get_field(key, 3) {
        Value::Long(id) => id as u64,
        Value::Int(id) => id as u64,
        _ => 0,
    }
}

fn append_data(ctx: &mut dyn NativeContext, this: ObjectRef, data: &[u8]) {
    let base = synthetic_base_offset(ctx, "java/security/Signature");
    let cur = match ctx.get_field(this, base + SIG_OFF_PENDING) {
        Value::Int(n) => n,
        _ => 0,
    };
    ctx.set_field(
        this,
        base + SIG_OFF_PENDING,
        Value::Int(cur + data.len() as i32),
    );
    let key = ctx.identity_hash_code(this);
    crypto_impl::sig_data_append_h(key, data);
}

/// Remove the receiver's accumulated payload from the side table.
///
/// Returns `Err(IllegalStateException)` when the side-table entry is
/// missing — i.e. the receiver was never `init*`-ed through this
/// registrar (so `clear_data` never seeded an empty buffer).  Should
/// also catch any future regression to a GC-orphaning key scheme.
/// `clear_data` (called from `sig_init_sign` / `sig_init_verify`) seeds
/// an empty buffer, so a normal `init → sign` with no intervening
/// `update()` still resolves to `Ok(Vec::new())`.  A silent empty-buffer
/// fallback at this layer would have produced a valid-looking signature
/// over `b""` — precisely the failure mode C18 fixes.
fn take_data(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Result<Vec<u8>, cratonvm_types::error::MethodCallFailed> {
    let base = synthetic_base_offset(ctx, "java/security/Signature");
    ctx.set_field(this, base + SIG_OFF_PENDING, Value::Int(0));
    let key = ctx.identity_hash_code(this);
    crypto_impl::sig_data_take_h(key).ok_or_else(|| {
        RuntimeError::IllegalStateException {
            message: "Signature payload missing post-GC or init*() never called".into(),
        }
        .into()
    })
}

fn clear_data(ctx: &mut dyn NativeContext, this: ObjectRef) {
    let key = ctx.identity_hash_code(this);
    crypto_impl::sig_data_clear_h(key);
}

// ---------------------------------------------------------------------------
// Sign / verify dispatch
// ---------------------------------------------------------------------------

/// Hash-then-sign for `SHA*withRSA` family.  RSA's `crypto_impl::Rsa::sign_sha256`
/// hashes internally, so for the SHA-256 variant we use it directly.  For
/// SHA-384/512 we hash first via the `hash_function`-returning helper and
/// then pad-and-modpow through PKCS#1 v1.5.  Out of scope for the probe;
/// the probe is SHA-256 only.
fn sign_dispatch(alg: i32, key_id: u64, data: &[u8]) -> Option<Vec<u8>> {
    match alg {
        SIG_SHA256_RSA => crypto_impl::rsa_sign(key_id, data),
        SIG_PSS_SHA256 => {
            crypto_impl::rsa_sign_pss_by_id(key_id, crypto_impl::PssHash::Sha256, data)
        }
        SIG_PSS_SHA384 => {
            crypto_impl::rsa_sign_pss_by_id(key_id, crypto_impl::PssHash::Sha384, data)
        }
        SIG_PSS_SHA512 => {
            crypto_impl::rsa_sign_pss_by_id(key_id, crypto_impl::PssHash::Sha512, data)
        }
        SIG_SHA256_ECDSA => crypto_impl::ecdsa_sign_sha256(key_id, data),
        SIG_SHA384_ECDSA => crypto_impl::ecdsa_sign(key_id, data),
        SIG_ED25519 => crypto_impl::ed25519_sign(key_id, data),
        _ => None,
    }
}

fn verify_dispatch(alg: i32, key_id: u64, data: &[u8], sig: &[u8]) -> Option<bool> {
    match alg {
        SIG_SHA256_RSA => crypto_impl::rsa_verify(key_id, data, sig),
        SIG_PSS_SHA256 => {
            crypto_impl::rsa_verify_pss_by_id(key_id, crypto_impl::PssHash::Sha256, data, sig)
        }
        SIG_PSS_SHA384 => {
            crypto_impl::rsa_verify_pss_by_id(key_id, crypto_impl::PssHash::Sha384, data, sig)
        }
        SIG_PSS_SHA512 => {
            crypto_impl::rsa_verify_pss_by_id(key_id, crypto_impl::PssHash::Sha512, data, sig)
        }
        SIG_SHA256_ECDSA => crypto_impl::ecdsa_verify_sha256(key_id, data, sig),
        SIG_SHA384_ECDSA => crypto_impl::ecdsa_verify(key_id, data, sig),
        SIG_ED25519 => crypto_impl::ed25519_verify(key_id, data, sig),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// EC-scoped real-SunEC ECDSA routing (crate::route_ec_to_real, default ON)
// ---------------------------------------------------------------------------
//
// The synthetic ECDSA sign/verify reads a synthetic `key_id` (slot 3) that a
// real `sun.security.ec.ECPrivateKeyImpl` (produced by the real KeyFactory/KPG
// path) does not carry. When EC routing is on we instead DRIVE the real SunEC
// `ECDSASignature$*` SPI over the buffered payload + the real key (stored at
// `SIG_OFF_KEYOBJ`), yielding/verifying real DER signatures. RSA/Ed25519 keep
// the synthetic dispatch above.

/// Real-SunEC `ECDSASignature$*` SPI class for an algo index, or `None` when EC
/// routing is off or the algo is not ECDSA.
fn ecdsa_real_spi_class(alg: i32) -> Option<&'static str> {
    if !crate::route_ec_to_real() {
        return None;
    }
    match alg {
        SIG_SHA256_ECDSA => Some("sun/security/ec/ECDSASignature$SHA256"),
        SIG_SHA384_ECDSA => Some("sun/security/ec/ECDSASignature$SHA384"),
        SIG_SHA512_ECDSA => Some("sun/security/ec/ECDSASignature$SHA512"),
        _ => None,
    }
}

/// Real SunEC EdDSA SPI for the requested curve. The generic `EdDSA` SPI
/// selects its curve from the supplied key during initialization.
fn eddsa_real_spi_class(alg: i32) -> Option<&'static str> {
    if !crate::route_ec_to_real() {
        return None;
    }
    match alg {
        SIG_ED25519 => Some("sun/security/ec/ed/EdDSASignature$Ed25519"),
        SIG_ED448 => Some("sun/security/ec/ed/EdDSASignature$Ed448"),
        SIG_EDDSA => Some("sun/security/ec/ed/EdDSASignature"),
        _ => None,
    }
}

/// Real JDK `sun.security.provider.DSA$*` SPI class for a DSA algo index, or
/// `None` when DSA routing is off or the algo is not DSA.
///
/// CratonVM has no native DSA sign/verify at all (unlike RSA/ECDSA, which
/// have a fast synthetic Rust path with real-key routing layered on top) —
/// `crypto_impl`'s `verify_dispatch`/`sign_dispatch` simply don't have a DSA
/// arm, so `Signature.verify()` for any DSA algorithm silently returned
/// `false` (`.unwrap_or(false)`) with no exception. Route directly to the
/// real JDK 25 SUN-provider SPI instead, same mechanism as ECDSA/EdDSA/
/// ML-DSA above — `sun.security.provider.DSA$SHA256withDSA`/`SHA1withDSA`
/// are plain, dependency-free classes (construct + `engineInitVerify(key)` +
/// `engineUpdate(buf)` + `engineVerify(sig)`), so there's no reason to
/// hand-roll DSA modular-exponentiation crypto in Rust when the real
/// implementation is already sitting in the boot classpath.
///
/// Found while root-causing `SecurityInfoTests.getWhenJarIsSigned`/
/// `NestedJarFileTests.verifySignedJar`: `bcprov-jdk18on`'s real jarsigner
/// signature uses a 2048-bit DSA key (`SHA256withDSA`) — the `.DSA` file
/// extension is literal here, not just jarsigner's default naming.
///
/// Kill-switch `CRATONVM_SYNTHETIC_DSA=1` restores the legacy (always-false)
/// behavior for debugging / regression bisecting.
fn dsa_real_spi_class(alg: i32) -> Option<&'static str> {
    if !crate::route_dsa_to_real() {
        return None;
    }
    match alg {
        SIG_SHA256_DSA => Some("sun/security/provider/DSA$SHA256withDSA"),
        SIG_SHA1_DSA => Some("sun/security/provider/DSA$SHA1withDSA"),
        _ => None,
    }
}

/// Drive the real SunEC `ECDSASignature$*` SPI: `new` → `engineInitSign/Verify(key)`
/// → `engineUpdate(buffer)` → `engineSign()`/`engineVerify(sig)`. `verify_sig`
/// `None` → sign (returns the DER `byte[]`); `Some(sig)` → verify (returns
/// `Int(0/1)`). The real key is read from `SIG_OFF_KEYOBJ`; the payload from the
/// identity-hash-keyed side table via `take_data`.
fn drive_real_signature_spi(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    spi_class: &'static str,
    verify_sig: Option<Vec<u8>>,
) -> MethodCallResult {
    let base = synthetic_base_offset(ctx, "java/security/Signature");
    let key = match ctx.get_field(this, base + SIG_OFF_KEYOBJ) {
        Value::Object(Some(o)) => o,
        _ => {
            return Err(RuntimeError::IllegalStateException {
                message: "Signature not initialized (no EC key)".into(),
            }
            .into())
        }
    };
    let data = take_data(ctx, this)?;
    let verifying = verify_sig.is_some();
    // Pin the EC key across the (allocating) SPI construction + calls.
    let key_pin = ctx.pin_native_root(key);
    let result = (|| {
        let spi = match ctx.new_object_initialized(spi_class, "()V", &[])? {
            Some(Value::Object(Some(o))) => o,
            _ => {
                return Err(RuntimeError::NotImplemented {
                    feature: spi_class.into(),
                }
                .into())
            }
        };
        let spi_pin = ctx.pin_native_root(spi);
        let key = ctx.read_native_pin(key_pin, key);
        let (init_m, init_desc) = if verifying {
            ("engineInitVerify", "(Ljava/security/PublicKey;)V")
        } else {
            ("engineInitSign", "(Ljava/security/PrivateKey;)V")
        };
        ctx.invoke_virtual(spi, init_m, init_desc, &[Value::Object(Some(key))])?;
        let spi = ctx.read_native_pin(spi_pin, spi);
        let arr = alloc_byte_array(ctx, &data);
        let spi = ctx.read_native_pin(spi_pin, spi);
        ctx.invoke_virtual(
            spi,
            "engineUpdate",
            "([BII)V",
            &[
                Value::Object(Some(arr)),
                Value::Int(0),
                Value::Int(data.len() as i32),
            ],
        )?;
        let spi = ctx.read_native_pin(spi_pin, spi);
        match verify_sig {
            Some(sig_bytes) => {
                let sigarr = alloc_byte_array(ctx, &sig_bytes);
                let spi = ctx.read_native_pin(spi_pin, spi);
                let ok = ctx.invoke_virtual(
                    spi,
                    "engineVerify",
                    "([B)Z",
                    &[Value::Object(Some(sigarr))],
                )?;
                // Normalize to Int(0/1) so the caller's Z return is well-formed.
                Ok(match ok {
                    Some(Value::Int(n)) => Some(Value::Int(if n != 0 { 1 } else { 0 })),
                    _ => Some(Value::Int(0)),
                })
            }
            None => ctx.invoke_virtual(spi, "engineSign", "()[B", &[]),
        }
    })();
    ctx.unpin_native_roots(key_pin);
    result
}

// ---------------------------------------------------------------------------
// Post-quantum ML-DSA → real JDK SUN-provider SPI routing
// (crate::route_pqc_to_real, default ON)
// ---------------------------------------------------------------------------
//
// CratonVM has no native ML-DSA lattice signature crypto. JDK 25 ships a real
// pure-Java implementation in the SUN provider: `sun.security.provider.
// ML_DSA_Impls`, with one concrete `SignatureSpi` per NIST parameter set —
// `$SIG2` (ML-DSA-44), `$SIG3` (ML-DSA-65), `$SIG5` (ML-DSA-87) — exactly the
// `$KPG{n}`/`$KF{n}` naming the keygen route already drives
// (`jca::key_factory::pqc_spi_classes`). We drive that SPI the same way
// `drive_real_ecdsa` drives `sun.security.ec.ECDSASignature$*`: construct the
// SPI, `engineInitSign/Verify(key)`, `engineUpdate(buffer)`, then
// `engineSign()`/`engineVerify(sig)`. This is only correct because the native
// `SHA3.keccak` override (lib.rs) gives the JDK SHAKE256 sponge real output —
// the same precondition the ML-DSA keygen route documents.

/// `true` once `route_pqc_to_real()` is on AND `alg` is an ML-DSA index. The
/// umbrella `SIG_MLDSA` qualifies too: its concrete parameter set is resolved
/// from the init key's algorithm at drive time.
fn is_mldsa(alg: i32) -> bool {
    crate::route_pqc_to_real()
        && matches!(alg, SIG_MLDSA | SIG_MLDSA_44 | SIG_MLDSA_65 | SIG_MLDSA_87)
}

/// Real-SUN `ML_DSA_Impls$SIG{2,3,5}` class for an ML-DSA parameter-set name
/// (e.g. "ML-DSA-65"), or `None` when the name is not a recognised ML-DSA set.
/// The suffix mapping matches `jca::key_factory::pqc_spi_classes` (2/3/5 by NIST
/// category), so the Signature SPI is the same provider that minted the key.
fn mldsa_spi_class_for_name(name: &str) -> Option<&'static str> {
    match name.to_ascii_uppercase().as_str() {
        "ML-DSA-44" => Some("sun/security/provider/ML_DSA_Impls$SIG2"),
        "ML-DSA-65" => Some("sun/security/provider/ML_DSA_Impls$SIG3"),
        "ML-DSA-87" => Some("sun/security/provider/ML_DSA_Impls$SIG5"),
        _ => None,
    }
}

/// Resolve the concrete `ML_DSA_Impls$SIG*` SPI class for this Signature.
///
/// Precedence: a parameter-set-specific algo index (`SIG_MLDSA_{44,65,87}`,
/// reached when the caller did `Signature.getInstance("ML-DSA-65")`) pins the
/// SPI directly. For the umbrella `SIG_MLDSA` ("ML-DSA"), read the init key's
/// `getAlgorithm()` — the real `ML_DSA_Impls` keys report their parameter set
/// there — and map that. Returns `None` (→ fail-closed at the call site) when
/// the set can't be determined, never a silently-wrong SPI.
fn mldsa_spi_class(ctx: &mut dyn NativeContext, alg: i32, key: ObjectRef) -> Option<&'static str> {
    match alg {
        SIG_MLDSA_44 => mldsa_spi_class_for_name("ML-DSA-44"),
        SIG_MLDSA_65 => mldsa_spi_class_for_name("ML-DSA-65"),
        SIG_MLDSA_87 => mldsa_spi_class_for_name("ML-DSA-87"),
        SIG_MLDSA => {
            let pin = ctx.pin_native_root(key);
            let k = ctx.read_native_pin(pin, key);
            let name = match ctx.invoke_virtual(k, "getAlgorithm", "()Ljava/lang/String;", &[]) {
                Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            ctx.unpin_native_roots(pin);
            mldsa_spi_class_for_name(&name)
        }
        _ => None,
    }
}

/// Drive the real SUN `ML_DSA_Impls$SIG*` SPI: `new` → `engineInitSign/Verify(key)`
/// → `engineUpdate(buffer)` → `engineSign()`/`engineVerify(sig)`. `verify_sig`
/// `None` → sign (returns the raw `byte[]` signature); `Some(sig)` → verify
/// (returns `Int(0/1)`). The real ML-DSA key is read from `SIG_OFF_KEYOBJ`; the
/// payload from the identity-hash-keyed side table via `take_data`. Structurally
/// identical to `drive_real_ecdsa`.
fn drive_real_mldsa(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    alg: i32,
    verify_sig: Option<Vec<u8>>,
) -> MethodCallResult {
    let base = synthetic_base_offset(ctx, "java/security/Signature");
    let key = match ctx.get_field(this, base + SIG_OFF_KEYOBJ) {
        Value::Object(Some(o)) => o,
        _ => {
            return Err(RuntimeError::IllegalStateException {
                message: "Signature not initialized (no ML-DSA key)".into(),
            }
            .into())
        }
    };
    // Resolve the parameter-set SPI before consuming the payload, so a closed
    // fail leaves nothing half-done. Fail closed (no synthetic stub) when the
    // set can't be determined — never sign/verify with the wrong SPI.
    let spi_class = match mldsa_spi_class(ctx, alg, key) {
        Some(c) => c,
        None => {
            return Err(RuntimeError::NotImplemented {
                feature: "ML-DSA parameter set could not be resolved for Signature".into(),
            }
            .into())
        }
    };
    let data = take_data(ctx, this)?;
    let verifying = verify_sig.is_some();
    let key_pin = ctx.pin_native_root(key);
    let result = (|| {
        let spi = match ctx.new_object_initialized(spi_class, "()V", &[])? {
            Some(Value::Object(Some(o))) => o,
            _ => {
                return Err(RuntimeError::NotImplemented {
                    feature: spi_class.into(),
                }
                .into())
            }
        };
        let spi_pin = ctx.pin_native_root(spi);
        let key = ctx.read_native_pin(key_pin, key);
        let (init_m, init_desc) = if verifying {
            ("engineInitVerify", "(Ljava/security/PublicKey;)V")
        } else {
            ("engineInitSign", "(Ljava/security/PrivateKey;)V")
        };
        ctx.invoke_virtual(spi, init_m, init_desc, &[Value::Object(Some(key))])?;
        let spi = ctx.read_native_pin(spi_pin, spi);
        let arr = alloc_byte_array(ctx, &data);
        let spi = ctx.read_native_pin(spi_pin, spi);
        ctx.invoke_virtual(
            spi,
            "engineUpdate",
            "([BII)V",
            &[
                Value::Object(Some(arr)),
                Value::Int(0),
                Value::Int(data.len() as i32),
            ],
        )?;
        let spi = ctx.read_native_pin(spi_pin, spi);
        match verify_sig {
            Some(sig_bytes) => {
                let sigarr = alloc_byte_array(ctx, &sig_bytes);
                let spi = ctx.read_native_pin(spi_pin, spi);
                let ok = ctx.invoke_virtual(
                    spi,
                    "engineVerify",
                    "([B)Z",
                    &[Value::Object(Some(sigarr))],
                )?;
                Ok(match ok {
                    Some(Value::Int(n)) => Some(Value::Int(if n != 0 { 1 } else { 0 })),
                    _ => Some(Value::Int(0)),
                })
            }
            None => ctx.invoke_virtual(spi, "engineSign", "()[B", &[]),
        }
    })();
    ctx.unpin_native_roots(key_pin);
    result
}

// ---------------------------------------------------------------------------
// Native methods
// ---------------------------------------------------------------------------

fn sig_get_instance(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Shared by all three `getInstance` overloads — see `check_named_provider_arg`.
    crate::jca::provider_chain::check_named_provider_arg(
        ctx,
        args,
        1,
        crate::jca::provider_chain::ProviderArgWording::Shared,
    )?;
    let alg = read_string(ctx, args, 0);
    crate::jca::provider_chain::check_provider_ownership(
        ctx,
        args,
        1,
        "Signature",
        &alg,
        crate::jca::provider_chain::ProviderArgWording::Shared,
    )?;
    let idx = algo_idx(&alg);
    let base = synthetic_base_offset(ctx, "java/security/Signature");
    let obj = alloc_concurrent_synthetic(ctx, "java/security/Signature", base + SIG_PRIVATE_SLOTS);
    // SigProbe fix: side-table is the authoritative store; the base-offset
    // slot writes remain for any synthetic-mode caller that goes through
    // slot indexing.  C15: keyed on identity hash code so GC compaction
    // doesn't orphan the entries.
    set_sig_algo(ctx, obj, idx);
    set_sig_state(ctx, obj, STATE_UNINIT);
    set_sig_keyid(ctx, obj, 0);
    let algo_str = ctx.create_string(&alg);
    ctx.set_field_by_name(obj, "algorithm", Value::Object(Some(algo_str)));
    ctx.set_field(obj, base + SIG_OFF_ALGO, Value::Int(idx));
    ctx.set_field(obj, base + SIG_OFF_STATE, Value::Int(STATE_UNINIT));
    ctx.set_field(obj, base + SIG_OFF_PROVIDER, Value::Int(0));
    ctx.set_field(obj, base + SIG_OFF_PENDING, Value::Int(0));
    ctx.set_field(obj, base + SIG_OFF_KEYID, Value::Long(0));
    Ok(Some(Value::Object(Some(obj))))
}

fn sig_init_sign(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let base = synthetic_base_offset(ctx, "java/security/Signature");
    set_sig_state(ctx, this, STATE_SIGN);
    ctx.set_field(this, base + SIG_OFF_STATE, Value::Int(STATE_SIGN));
    ctx.set_field(this, base + SIG_OFF_PENDING, Value::Int(0));
    if let Some(Value::Object(Some(k))) = args.get(1) {
        let kid = extract_key_id_from_key(ctx, *k);
        set_sig_keyid(ctx, this, kid);
        ctx.set_field(this, base + SIG_OFF_KEYID, Value::Long(kid as i64));
        // Stash the real key object for the SunEC ECDSA drive path (slot is
        // GC-scanned, so the ref survives init→update→sign relocations).
        ctx.set_field(this, base + SIG_OFF_KEYOBJ, Value::Object(Some(*k)));
    }
    clear_data(ctx, this);
    Ok(None)
}

fn sig_init_verify(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let base = synthetic_base_offset(ctx, "java/security/Signature");
    set_sig_state(ctx, this, STATE_VERIFY);
    ctx.set_field(this, base + SIG_OFF_STATE, Value::Int(STATE_VERIFY));
    ctx.set_field(this, base + SIG_OFF_PENDING, Value::Int(0));
    if let Some(Value::Object(Some(k))) = args.get(1) {
        let kid = extract_key_id_from_key(ctx, *k);
        set_sig_keyid(ctx, this, kid);
        ctx.set_field(this, base + SIG_OFF_KEYID, Value::Long(kid as i64));
        // Stash the real key object for the SunEC ECDSA drive path (slot is
        // GC-scanned, so the ref survives init→update→sign relocations).
        ctx.set_field(this, base + SIG_OFF_KEYOBJ, Value::Object(Some(*k)));
    }
    clear_data(ctx, this);
    Ok(None)
}

fn sig_update_byte(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let b = match args.get(1) {
        Some(Value::Int(v)) => *v as u8,
        _ => 0,
    };
    append_data(ctx, this, &[b]);
    Ok(None)
}

fn sig_update_bytes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    if let Some(Value::Object(Some(arr))) = args.get(1) {
        let buf = read_byte_array_full(ctx, *arr);
        append_data(ctx, this, &buf);
    }
    Ok(None)
}

fn sig_update_bytes_off_len(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    if let Some(Value::Object(Some(arr))) = args.get(1) {
        let off = match args.get(2) {
            Some(Value::Int(n)) => *n as usize,
            _ => 0,
        };
        let len = match args.get(3) {
            Some(Value::Int(n)) => *n as usize,
            _ => 0,
        };
        let buf = read_byte_array_range(ctx, *arr, off, len);
        append_data(ctx, this, &buf);
    }
    Ok(None)
}

fn sig_update_bytebuffer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // The probe doesn't use this path.  Keep it as a no-op so any caller
    // that lands here gets a deterministic "no data appended" semantics.
    let _ = (ctx, args);
    Ok(None)
}

/// C15: convert a missing side-table state lookup into a loud
/// `IllegalStateException` instead of silently degrading to a slot-read
/// that the real-JDK layout will return as `Object(None)`.  After the
/// identity-hash-code key migration, a miss here means the receiver was
/// either never produced by our `getInstance` or — pre-fix — was orphaned
/// by GC compaction; either way, silently returning `STATE_UNINIT` masks
/// the underlying defect.
fn require_sig_state(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Result<i32, cratonvm_types::error::MethodCallFailed> {
    if let Some(st) = get_sig_state(ctx, this) {
        return Ok(st);
    }
    Err(RuntimeError::IllegalStateException {
        message: "Signature state missing post-GC or never initialized".into(),
    }
    .into())
}

fn require_sig_algo(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Result<i32, cratonvm_types::error::MethodCallFailed> {
    if let Some(a) = get_sig_algo(ctx, this) {
        return Ok(a);
    }
    Err(RuntimeError::IllegalStateException {
        message: "Signature state missing post-GC or never initialized".into(),
    }
    .into())
}

fn sig_sign(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let state = require_sig_state(ctx, this)?;
    if state != STATE_SIGN {
        return Err(RuntimeError::IllegalStateException {
            message: "Signature object not initialized for signing".into(),
        }
        .into());
    }
    let alg = require_sig_algo(ctx, this)?;
    // EC: drive the real SunEC ECDSASignature SPI (real key, real DER output).
    if let Some(spi_class) = ecdsa_real_spi_class(alg) {
        return drive_real_signature_spi(ctx, this, spi_class, None);
    }
    if let Some(spi_class) = eddsa_real_spi_class(alg) {
        return drive_real_signature_spi(ctx, this, spi_class, None);
    }
    // DSA: drive the real sun.security.provider.DSA$* SPI (no native DSA crypto).
    if let Some(spi_class) = dsa_real_spi_class(alg) {
        return drive_real_signature_spi(ctx, this, spi_class, None);
    }
    // ML-DSA: drive the real SUN ML_DSA_Impls$SIG* SPI (real lattice signature).
    if is_mldsa(alg) {
        return drive_real_mldsa(ctx, this, alg, None);
    }
    let key_id = key_id_of(ctx, this);
    // C18: surface a missing payload as IllegalStateException rather than
    // silently signing/verifying `b""` (the pre-fix raw-pointer keying
    // could orphan the buffer after GC compaction and the empty-fallback
    // produced an apparent success).
    let data = take_data(ctx, this)?;

    let sig_bytes = sign_dispatch(alg, key_id, &data).unwrap_or_default();
    let arr = alloc_byte_array(ctx, &sig_bytes);
    Ok(Some(Value::Object(Some(arr))))
}

fn sig_sign_into(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let state = require_sig_state(ctx, this)?;
    if state != STATE_SIGN {
        return Err(RuntimeError::IllegalStateException {
            message: "Signature object not initialized for signing".into(),
        }
        .into());
    }
    let alg = require_sig_algo(ctx, this)?;
    let key_id = key_id_of(ctx, this);
    // C18: surface a missing payload as IllegalStateException rather than
    // silently signing/verifying `b""` (the pre-fix raw-pointer keying
    // could orphan the buffer after GC compaction and the empty-fallback
    // produced an apparent success).
    let data = take_data(ctx, this)?;
    let sig_bytes = sign_dispatch(alg, key_id, &data).unwrap_or_default();

    let off = match args.get(2) {
        Some(Value::Int(n)) => *n as usize,
        _ => 0,
    };
    let max_len = match args.get(3) {
        Some(Value::Int(n)) => *n as usize,
        _ => sig_bytes.len(),
    };
    let written = sig_bytes.len().min(max_len);
    if let Some(Value::Object(Some(out))) = args.get(1) {
        for (i, &b) in sig_bytes.iter().take(written).enumerate() {
            ctx.set_array_element(*out, off + i, Value::Int(b as i8 as i32));
        }
    }
    Ok(Some(Value::Int(written as i32)))
}

fn sig_verify(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let state = require_sig_state(ctx, this)?;
    if state != STATE_VERIFY {
        return Err(RuntimeError::IllegalStateException {
            message: "Signature object not initialized for verification".into(),
        }
        .into());
    }
    let alg = require_sig_algo(ctx, this)?;
    // EC: drive the real SunEC ECDSASignature SPI (real key, real DER verify).
    if let Some(spi_class) = ecdsa_real_spi_class(alg) {
        let provided = match args.get(1) {
            Some(Value::Object(Some(arr))) => read_byte_array_full(ctx, *arr),
            _ => Vec::new(),
        };
        return drive_real_signature_spi(ctx, this, spi_class, Some(provided));
    }
    if let Some(spi_class) = eddsa_real_spi_class(alg) {
        let provided = match args.get(1) {
            Some(Value::Object(Some(arr))) => read_byte_array_full(ctx, *arr),
            _ => Vec::new(),
        };
        return drive_real_signature_spi(ctx, this, spi_class, Some(provided));
    }
    // DSA: drive the real sun.security.provider.DSA$* SPI (no native DSA crypto).
    if let Some(spi_class) = dsa_real_spi_class(alg) {
        let provided = match args.get(1) {
            Some(Value::Object(Some(arr))) => read_byte_array_full(ctx, *arr),
            _ => Vec::new(),
        };
        return drive_real_signature_spi(ctx, this, spi_class, Some(provided));
    }
    // ML-DSA: drive the real SUN ML_DSA_Impls$SIG* SPI (real lattice verify).
    if is_mldsa(alg) {
        let provided = match args.get(1) {
            Some(Value::Object(Some(arr))) => read_byte_array_full(ctx, *arr),
            _ => Vec::new(),
        };
        return drive_real_mldsa(ctx, this, alg, Some(provided));
    }
    let key_id = key_id_of(ctx, this);
    // C18: surface a missing payload as IllegalStateException rather than
    // silently signing/verifying `b""` (the pre-fix raw-pointer keying
    // could orphan the buffer after GC compaction and the empty-fallback
    // produced an apparent success).
    let data = take_data(ctx, this)?;
    let provided = match args.get(1) {
        Some(Value::Object(Some(arr))) => read_byte_array_full(ctx, *arr),
        _ => Vec::new(),
    };

    let ok = verify_dispatch(alg, key_id, &data, &provided).unwrap_or(false);
    Ok(Some(Value::Int(if ok { 1 } else { 0 })))
}

fn sig_verify_off_len(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let state = require_sig_state(ctx, this)?;
    if state != STATE_VERIFY {
        return Err(RuntimeError::IllegalStateException {
            message: "Signature object not initialized for verification".into(),
        }
        .into());
    }
    let alg = require_sig_algo(ctx, this)?;
    let key_id = key_id_of(ctx, this);
    // C18: surface a missing payload as IllegalStateException rather than
    // silently signing/verifying `b""` (the pre-fix raw-pointer keying
    // could orphan the buffer after GC compaction and the empty-fallback
    // produced an apparent success).
    let data = take_data(ctx, this)?;

    let off = match args.get(2) {
        Some(Value::Int(n)) => *n as usize,
        _ => 0,
    };
    let len = match args.get(3) {
        Some(Value::Int(n)) => *n as usize,
        _ => 0,
    };
    let provided = match args.get(1) {
        Some(Value::Object(Some(arr))) => read_byte_array_range(ctx, *arr, off, len),
        _ => Vec::new(),
    };
    let ok = verify_dispatch(alg, key_id, &data, &provided).unwrap_or(false);
    Ok(Some(Value::Int(if ok { 1 } else { 0 })))
}

fn sig_get_algorithm(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    // `getAlgorithm()` is a benign accessor — keep the slot-read fallback
    // so callers that only invoke it after a state-eroding bug elsewhere
    // still get *some* answer instead of an exception cascade.  The loud
    // error path is reserved for the cryptographic operations above.
    let base = synthetic_base_offset(ctx, "java/security/Signature");
    let idx =
        get_sig_algo(ctx, this).unwrap_or_else(|| match ctx.get_field(this, base + SIG_OFF_ALGO) {
            Value::Int(i) => i,
            _ => -1,
        });
    let s = ctx.create_string(algo_name(idx));
    Ok(Some(Value::Object(Some(s))))
}

fn sig_set_parameter(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

fn sig_get_provider_null(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register(r: &mut NativeMethodRegistry) {
    // Real-JCA bring-up: skip the synthetic Signature short-circuit so
    // Signature.getInstance falls through to the real JDK 25 + BouncyCastle
    // provider bytecode operating on concrete BC keys.
    if crate::real_jca_mode() {
        return;
    }
    let cls = "java/security/Signature";

    r.register(
        cls,
        "getInstance",
        "(Ljava/lang/String;)Ljava/security/Signature;",
        sig_get_instance,
    );
    r.register(
        cls,
        "getInstance",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/security/Signature;",
        sig_get_instance,
    );
    r.register(
        cls,
        "getInstance",
        "(Ljava/lang/String;Ljava/security/Provider;)Ljava/security/Signature;",
        sig_get_instance,
    );

    r.register(
        cls,
        "initSign",
        "(Ljava/security/PrivateKey;)V",
        sig_init_sign,
    );
    r.register(
        cls,
        "initSign",
        "(Ljava/security/PrivateKey;Ljava/security/SecureRandom;)V",
        sig_init_sign,
    );
    r.register(
        cls,
        "initVerify",
        "(Ljava/security/PublicKey;)V",
        sig_init_verify,
    );
    r.register(
        cls,
        "initVerify",
        "(Ljava/security/cert/Certificate;)V",
        sig_init_verify,
    );

    r.register(cls, "update", "(B)V", sig_update_byte);
    r.register(cls, "update", "([B)V", sig_update_bytes);
    r.register(cls, "update", "([BII)V", sig_update_bytes_off_len);
    r.register(
        cls,
        "update",
        "(Ljava/nio/ByteBuffer;)V",
        sig_update_bytebuffer,
    );

    r.register(cls, "sign", "()[B", sig_sign);
    r.register(cls, "sign", "([BII)I", sig_sign_into);

    r.register(cls, "verify", "([B)Z", sig_verify);
    r.register(cls, "verify", "([BII)Z", sig_verify_off_len);

    r.register(
        cls,
        "getAlgorithm",
        "()Ljava/lang/String;",
        sig_get_algorithm,
    );
    r.register(
        cls,
        "getProvider",
        "()Ljava/security/Provider;",
        sig_get_provider_null,
    );

    r.register(
        cls,
        "setParameter",
        "(Ljava/security/spec/AlgorithmParameterSpec;)V",
        sig_set_parameter,
    );
    r.register(
        cls,
        "setParameter",
        "(Ljava/lang/String;Ljava/lang/Object;)V",
        sig_set_parameter,
    );

    // `sun.security.util.SignatureUtil.{initVerify,initSign}WithParam` —
    // the real JDK indirects through `SharedSecrets.getJavaSecuritySignatureAccess()`
    // (set by `java.security.Signature.<clinit>`), but that clinit is no-op'd here
    // (it also triggers `Debug.getInstance`→Security-file read, same as Cipher), so
    // the accessor stays null → `X509CertImpl.verify` NPEs at
    // `SignatureUtil.initVerifyWithParam` ("Cannot invoke initVerify on null").
    // Intercept the helper to drive our registered `Signature.{initVerify,initSign,
    // setParameter}` natives directly — bypassing the null accessor and the
    // package-private `Signature.initVerify(key,params)` engine path. Used by
    // every real `X509Certificate.verify(key)` (EC and RSA certs alike).
    let sigutil = "sun/security/util/SignatureUtil";
    r.register(
        sigutil,
        "initVerifyWithParam",
        "(Ljava/security/Signature;Ljava/security/PublicKey;Ljava/security/spec/AlgorithmParameterSpec;)V",
        sigutil_init_verify_key,
    );
    r.register(
        sigutil,
        "initVerifyWithParam",
        "(Ljava/security/Signature;Ljava/security/cert/Certificate;Ljava/security/spec/AlgorithmParameterSpec;)V",
        sigutil_init_verify_cert,
    );
    r.register(
        sigutil,
        "initSignWithParam",
        "(Ljava/security/Signature;Ljava/security/PrivateKey;Ljava/security/spec/AlgorithmParameterSpec;Ljava/security/SecureRandom;)V",
        sigutil_init_sign,
    );
}

/// Shared body for the `SignatureUtil.{initVerify,initSign}WithParam` intercepts:
/// invoke the receiver `Signature`'s registered `initVerify`/`initSign` native
/// with `key`, then `setParameter(params)` when `params` is non-null. Pins the
/// `Signature` and `params` across the (possibly GC-triggering) virtual calls.
fn sigutil_drive(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    init_method: &str,
    init_desc: &str,
) -> MethodCallResult {
    let sig = match args.first() {
        Some(Value::Object(Some(s))) => *s,
        _ => return Ok(None),
    };
    let key = match args.get(1) {
        Some(Value::Object(Some(k))) => *k,
        _ => return Ok(None),
    };
    let params = match args.get(2) {
        Some(Value::Object(Some(p))) => Some(*p),
        _ => None,
    };
    let sp = ctx.pin_native_root(sig);
    let result = (|| -> MethodCallResult {
        let sig_r = ctx.read_native_pin(sp, sig);
        ctx.invoke_virtual(sig_r, init_method, init_desc, &[Value::Object(Some(key))])?;
        if let Some(p) = params {
            let sig_r = ctx.read_native_pin(sp, sig);
            ctx.invoke_virtual(
                sig_r,
                "setParameter",
                "(Ljava/security/spec/AlgorithmParameterSpec;)V",
                &[Value::Object(Some(p))],
            )?;
        }
        Ok(None)
    })();
    ctx.unpin_native_roots(sp);
    result
}

fn sigutil_init_verify_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    sigutil_drive(ctx, args, "initVerify", "(Ljava/security/PublicKey;)V")
}

fn sigutil_init_verify_cert(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    sigutil_drive(
        ctx,
        args,
        "initVerify",
        "(Ljava/security/cert/Certificate;)V",
    )
}

fn sigutil_init_sign(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    sigutil_drive(ctx, args, "initSign", "(Ljava/security/PrivateKey;)V")
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess};
    use super::*;

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

    /// Parameter-set → concrete SUN `ML_DSA_Impls$SIG{2,3,5}` SPI mapping
    /// (matching `key_factory::pqc_spi_classes`' 2/3/5 NIST-category suffixes),
    /// and a non-ML-DSA name rejects.
    #[test]
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
        if verifying {
            sig_init_verify(ctx, &args).unwrap();
        } else {
            sig_init_sign(ctx, &args).unwrap();
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
        use cratonvm_types::error::{MethodCallFailed, VmError};
        assert!(
            matches!(
                err,
                MethodCallFailed::InternalError(VmError::Runtime(
                    RuntimeError::IllegalStateException { .. }
                ))
            ),
            "expected IllegalStateException (no ML-DSA key), got {err:?}"
        );
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
}
