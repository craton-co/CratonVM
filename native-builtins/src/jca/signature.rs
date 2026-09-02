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
//! The actual `update(byte[])` payload lives in a side table keyed on
//! `(NativeContext::vm_identity(), identity_hash_code(this))` — see
//! `sig_payload_table` below.  Identity-hash-code keys survive GC
//! compaction (`HashCodeTable::update_after_gc` in
//! `gc/src/compact_header.rs`).  Pre-C18 the table was keyed on
//! `this.as_ptr() as u64`; a compaction silently orphaned the entry and
//! `sign()` returned a signature over `b""`.  The `vm_identity` component
//! was added later: the store used to be `crypto_impl::sig_data_*_h`,
//! which is process-global, so two `Vm`s in one process could consume each
//! other's buffers whenever their receivers' identity hashes collided.

#![allow(clippy::collapsible_if)]

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallResult, RuntimeError};
use cratonvm_types::{ArrayElementType, ClassId, ObjectRef, Value};

use crate::crypto_impl;
use crate::try_alloc_concurrent_synthetic;

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
/// The application `SignatureSpi` instance, when this `Signature` came from a
/// third-party provider (see `sig_user_spi_table`). GC-scanned like slot 5.
const SIG_OFF_SPIOBJ: usize = 6;
const SIG_PRIVATE_SLOTS: usize = 7;

/// Is `this` one of THIS engine's own `java.security.Signature` synthetics,
/// i.e. does it carry the private slots above?
///
/// It does not when the object is a provider's SPI that `sig_get_instance`
/// handed back UNWRAPPED — see the `spi_is_signature_subclass` branch there.
/// Such a receiver is the provider's own class, and `synthetic_base_offset`
/// counts `java.security.Signature`'s fields, so `base + SIG_OFF_*` indexes
/// straight into the SUBCLASS's declared fields: a read returns the provider's
/// data mistaken for ours, and a write destroys it.
fn sig_slots_are_ours(ctx: &mut dyn NativeContext, this: ObjectRef) -> bool {
    ctx.class_name_of_id(ctx.class_id_of_object(this))
        .is_some_and(|n| n == "java/security/Signature")
}

/// Read one private slot, or `Value::Object(None)` when the receiver has none.
fn sig_slot_get(ctx: &mut dyn NativeContext, this: ObjectRef, off: usize) -> Value {
    if !sig_slots_are_ours(ctx, this) {
        return Value::Object(None);
    }
    let base = synthetic_base_offset(ctx, "java/security/Signature");
    ctx.get_field(this, base + off)
}

/// Write one private slot; a no-op when the receiver has none. Every caller
/// also writes the authoritative side table, so skipping the slot loses
/// nothing.
fn sig_slot_set(ctx: &mut dyn NativeContext, this: ObjectRef, off: usize, v: Value) {
    if !sig_slots_are_ours(ctx, this) {
        return;
    }
    let base = synthetic_base_offset(ctx, "java/security/Signature");
    ctx.set_field(this, base + off, v);
}

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
// slot read also returns `Object(None)`, throwing the checked `SignatureException`.
// `NativeContext::identity_hash_code` is GC-stable
// (`HashCodeTable::update_after_gc`, `gc/src/compact_header.rs`).  All
// accessors therefore thread `&mut dyn NativeContext`.  Mirrors the pattern
// from `lang_invoke::VH_META_TABLE` (`native-builtins/src/lang_invoke.rs`).
//
// VM-scope fix: the identity hash code is unique only *within one heap*.
// Rust tests (and any embedder) create several independent `Vm`s in one
// process, and these tables are `static` — so VM B's `Signature` whose
// identity hash happens to equal VM A's silently reads VM A's algorithm /
// state / key id.  `NativeContext::vm_identity`'s own doc states the rule
// ("Native side caches ... must scope entries to this value",
// `native-api/src/registry.rs`); the same omission in native-collections'
// `widened_obj_key` aliased two VMs' collections and aborted the process.
// Every key here is therefore `(vm_identity, identity_hash_code)` — the
// established shape, cf. `phases_late::net_channels` and `servlet.rs`.
//
// None of these tables hold heap `ObjectRef`s (algorithm index, state,
// `crypto_impl` key handle and the raw `update()` payload are all plain Rust
// data), so no GC scan/remap companion is required — only the keys had to
// become address- and heap-independent.
// ---------------------------------------------------------------------------

/// VM-scoped, GC-stable side-table key for a `Signature` receiver.
type SigKey = (usize, i32);

/// `(owning VM, GC-stable identity hash)` for `this`.
fn sig_key(ctx: &mut dyn NativeContext, this: ObjectRef) -> SigKey {
    (ctx.vm_identity(), ctx.identity_hash_code(this))
}

fn sig_algo_table() -> &'static parking_lot::Mutex<rustc_hash::FxHashMap<SigKey, i32>> {
    use std::sync::OnceLock;
    static T: OnceLock<parking_lot::Mutex<rustc_hash::FxHashMap<SigKey, i32>>> = OnceLock::new();
    T.get_or_init(|| parking_lot::Mutex::new(rustc_hash::FxHashMap::default()))
}

fn sig_state_table() -> &'static parking_lot::Mutex<rustc_hash::FxHashMap<SigKey, i32>> {
    use std::sync::OnceLock;
    static T: OnceLock<parking_lot::Mutex<rustc_hash::FxHashMap<SigKey, i32>>> = OnceLock::new();
    T.get_or_init(|| parking_lot::Mutex::new(rustc_hash::FxHashMap::default()))
}

fn sig_keyid_table() -> &'static parking_lot::Mutex<rustc_hash::FxHashMap<SigKey, u64>> {
    use std::sync::OnceLock;
    static T: OnceLock<parking_lot::Mutex<rustc_hash::FxHashMap<SigKey, u64>>> = OnceLock::new();
    T.get_or_init(|| parking_lot::Mutex::new(rustc_hash::FxHashMap::default()))
}

/// The `Signature.update(...)` payload store.
///
/// Was `crypto_impl::sig_data_{append,take,clear}_h`, a `static
/// HashMap<i32, Vec<u8>>` keyed on the identity hash alone.  That store is
/// process-global with no VM scope, so two VMs in one process could append
/// to — and `take` — each other's buffers: VM B's `sign()` would consume the
/// bytes VM A had accumulated (and leave VM A's `sign()` to fail the
/// `take_data` `None` check, now a checked `SignatureException`).  The payload lives
/// here instead, under the same `(vm_identity, identity_hash)` key as the
/// sibling tables above.  `Vec<u8>` — no `ObjectRef`s, so no GC hooks.
fn sig_payload_table() -> &'static parking_lot::Mutex<rustc_hash::FxHashMap<SigKey, Vec<u8>>> {
    use std::sync::OnceLock;
    static T: OnceLock<parking_lot::Mutex<rustc_hash::FxHashMap<SigKey, Vec<u8>>>> =
        OnceLock::new();
    T.get_or_init(|| parking_lot::Mutex::new(rustc_hash::FxHashMap::default()))
}

/// `PSSParameterSpec`s installed via `Signature.setParameter`, same
/// `(vm_identity, identity_hash)` key discipline as the tables above and the
/// same reason: plain Rust data, GC-stable key, no heap refs to scan.
/// ARCH-2026-08-04 A6 — `LockLevel::Scratch` (L0) — both sites (`get_sig_pss` and
/// `setParameter`) compute `sig_key(ctx, ..)` first and then do one
/// `get(..).copied()` / `insert(..)` under a temporary guard.
fn sig_pss_table(
) -> &'static cratonvm_types::lock_order::OrderedPlMutex<rustc_hash::FxHashMap<SigKey, PssParams>> {
    use std::sync::OnceLock;
    static T: OnceLock<
        cratonvm_types::lock_order::OrderedPlMutex<rustc_hash::FxHashMap<SigKey, PssParams>>,
    > = OnceLock::new();
    T.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            rustc_hash::FxHashMap::default(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

fn get_sig_pss(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<PssParams> {
    let key = sig_key(ctx, this);
    sig_pss_table().lock().get(&key).copied()
}

/// `(provider name, SPI class)` for a `Signature` obtained from a THIRD-PARTY
/// provider, i.e. one this VM does not service natively.
///
/// See `provider_chain::third_party_service_class`. When an entry is present,
/// every operation on this `Signature` is forwarded to the application's own
/// `SignatureSpi` object (slot `SIG_OFF_SPIOBJ`) rather than to the native
/// dispatch tables — the whole point of the application having registered it.
/// ARCH-2026-08-04 A6 — `LockLevel::Scratch` (L0) — same shape and the same
/// key-before-guard discipline as [`sig_pss_table`].
fn sig_user_spi_table() -> &'static cratonvm_types::lock_order::OrderedPlMutex<
    rustc_hash::FxHashMap<SigKey, (String, String)>,
> {
    use std::sync::OnceLock;
    static T: OnceLock<
        cratonvm_types::lock_order::OrderedPlMutex<rustc_hash::FxHashMap<SigKey, (String, String)>>,
    > = OnceLock::new();
    T.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            rustc_hash::FxHashMap::default(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

fn get_sig_user_spi(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<(String, String)> {
    let key = sig_key(ctx, this);
    sig_user_spi_table().lock().get(&key).cloned()
}

/// The live application `SignatureSpi` for this `Signature`, if it has one.
fn sig_user_spi_obj(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    // An SPI that is ITSELF a `java.security.Signature` subclass is returned to
    // the caller unwrapped, so the receiver here IS the SPI. There is no
    // wrapper and no slot to read; forwarding to `this` is what makes every
    // engine call below reach the provider's own `engine*` bytecode.
    if !sig_slots_are_ours(ctx, this) {
        return get_sig_user_spi(ctx, this).map(|_| this);
    }
    match sig_slot_get(ctx, this, SIG_OFF_SPIOBJ) {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    }
}

fn set_sig_algo(ctx: &mut dyn NativeContext, this: ObjectRef, idx: i32) {
    let key = sig_key(ctx, this);
    sig_algo_table().lock().insert(key, idx);
}
fn get_sig_algo(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<i32> {
    let key = sig_key(ctx, this);
    sig_algo_table().lock().get(&key).copied()
}
fn set_sig_state(ctx: &mut dyn NativeContext, this: ObjectRef, st: i32) {
    let key = sig_key(ctx, this);
    sig_state_table().lock().insert(key, st);
}
fn get_sig_state(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<i32> {
    let key = sig_key(ctx, this);
    sig_state_table().lock().get(&key).copied()
}
fn set_sig_keyid(ctx: &mut dyn NativeContext, this: ObjectRef, kid: u64) {
    let key = sig_key(ctx, this);
    sig_keyid_table().lock().insert(key, kid);
}
fn get_sig_keyid(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<u64> {
    let key = sig_key(ctx, this);
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
/// `NONEwithRSA` — PKCS#1 v1.5 block type 1 over the caller's own bytes, with
/// no hashing and no DigestInfo.
///
/// The one RSA signature name SunJCE serves rather than SunRsaSign, because it
/// is implemented there by encrypting under the private key
/// (`com.sun.crypto.provider.RSACipherAdaptor`). `getInstance` refused it until
/// 2026-08-14 — `algo_idx` had no arm and no provider in the chain advertises
/// it, so `signature_name_is_offered` answered false on both halves.
const SIG_NONE_RSA: i32 = 22;
/// `MD5andSHA1withRSA` — the TLS 1.0/1.1 CertificateVerify signature: PKCS#1
/// v1.5 block type 1 over `MD5(m) || SHA1(m)` with NO DigestInfo (there is no
/// OID for the pair). SunJSSE serves it, not SunRsaSign, and netty's
/// `JdkDelegatingPrivateKeyMethod` maps `SSL_SIGN_RSA_PKCS1_MD5_SHA1` onto it.
const SIG_MD5_SHA1_RSA: i32 = 23;
/// Every OTHER member of the `sun.security.provider.DSA` family — the eighteen
/// names `SHA1withDSA` and `SHA256withDSA` are the only two of, plus their
/// `inP1363Format` twins.
///
/// ONE index for all of them, because the concrete SPI class is a pure function
/// of the caller's own spelling (`dsa_family_spi_class`) and this engine
/// computes none of them itself: the whole of a DSA signature here is
/// `construct the JDK's SPI, init, update, sign/verify`. Eighteen constants
/// would be eighteen names for the same behaviour, and every switch on them
/// would have eighteen identical arms.
///
/// `SHA1withDSA` and `SHA256withDSA` keep their own indices above so nothing
/// that already resolves changes route — their `algo_idx` arms are matched
/// first, and `algo_name` still answers for them.
const SIG_DSA_REAL: i32 = 24;
/// `HSS/LMS` (RFC 8554) — the stateful hash-based signature the SUN provider
/// added in JDK 21, VERIFY-only there as here.
///
/// Same shape as `SIG_DSA_REAL`: this crate implements no Merkle-tree
/// signature, the platform's `sun.security.provider.HSS` does, and the drive is
/// the same construct/init/update/verify. It gets its own index rather than
/// joining the DSA family because its SPI class is not derivable from the name
/// and because a caller CAN tell the two apart — `initSign` on this one fails
/// on HotSpot too (`HSS/LMS` signing is not implemented in the JDK), and that
/// refusal has to come from the platform's own SPI rather than from a guess
/// here.
const SIG_HSS_LMS: i32 = 25;
/// The nine `SunRsaSign` PKCS#1 v1.5 names that were ADVERTISED, admitted by
/// `getInstance`, and then failed at `sign()`/`verify()` — the gap
/// `W7-63-jca-advertise-vs-serve.md` §8 records as "ordinary unimplemented-
/// algorithm" and §3 #5 declines to close.
///
/// ONE index per digest rather than one per name, because the digest is the
/// only thing that varies: PKCS#1 v1.5 over RSA is `hash`, then a `DigestInfo`
/// whose only per-digest inputs are an OID and a length
/// (`DigestAlgorithm::pkcs1v15_digest_info_prefix`), then block type 1. Every
/// one of the nine digests was already implemented in this tree — MD2 by this
/// very record's §3 #1, the rest by `compute_digest`'s own arms — so what was
/// missing was the arm, not the cryptography.
/// Every OTHER member of the `sun.security.ec.ECDSASignature` family — the
/// seventeen names `SHA{256,384,512}withECDSA` are the only three of.
///
/// Same shape and same reason as `SIG_DSA_REAL`: this engine computes no ECDSA
/// itself (the three that work are already the platform's SPI, driven by
/// `drive_real_signature_spi`), and the concrete class is a function of the
/// caller's spelling. Ten of the seventeen are `inP1363Format` twins, which are
/// separate classes emitting the fixed-width `r || s` rather than the DER
/// `SEQUENCE` — so routing to the class is what makes the ENCODING right, not
/// only the signature.
///
/// Fourteen of them were ADVERTISED and admitted by `getInstance` and then
/// failed at `sign()`; four were not advertised at all. Both halves are the
/// species `W7-63-jca-advertise-vs-serve.md` is named for, and §8's second
/// bullet counts only the RSA nine.
const SIG_ECDSA_REAL: i32 = 35;
const SIG_MD2_RSA: i32 = 26;
const SIG_MD5_RSA: i32 = 27;
const SIG_SHA224_RSA: i32 = 28;
const SIG_SHA512_224_RSA: i32 = 29;
const SIG_SHA512_256_RSA: i32 = 30;
const SIG_SHA3_224_RSA: i32 = 31;
const SIG_SHA3_256_RSA: i32 = 32;
const SIG_SHA3_384_RSA: i32 = 33;
const SIG_SHA3_512_RSA: i32 = 34;

/// The `DigestAlgorithm` a PKCS#1 v1.5 RSA index signs and verifies with, or
/// `None` when the index is not one of them.
///
/// One table for both directions: `sign_dispatch` and `verify_dispatch` used to
/// carry parallel per-index arms, which is how `SHA512withRSA` came to sign
/// after the verify side had taken a `DigestAlgorithm` "all along" (the comment
/// on `SIG_SHA1_RSA`'s sign arm records that asymmetry).
fn rsa_pkcs1_digest(alg: i32) -> Option<cratonvm_native_builtins_crypto::signature::DigestAlgorithm> {
    use cratonvm_native_builtins_crypto::signature::DigestAlgorithm as D;
    Some(match alg {
        SIG_MD2_RSA => D::Md2,
        SIG_MD5_RSA => D::Md5,
        SIG_SHA1_RSA => D::Sha1,
        SIG_SHA224_RSA => D::Sha224,
        SIG_SHA256_RSA => D::Sha256,
        SIG_SHA384_RSA => D::Sha384,
        SIG_SHA512_RSA => D::Sha512,
        SIG_SHA512_224_RSA => D::Sha512_224,
        SIG_SHA512_256_RSA => D::Sha512_256,
        SIG_SHA3_224_RSA => D::Sha3_224,
        SIG_SHA3_256_RSA => D::Sha3_256,
        SIG_SHA3_384_RSA => D::Sha3_384,
        SIG_SHA3_512_RSA => D::Sha3_512,
        _ => return None,
    })
}

fn algo_idx(name: &str) -> i32 {
    let upper = name.to_ascii_uppercase();
    match upper.as_str() {
        "SHA256WITHRSA" | "SHA-256WITHRSA" | "RSASSA-PKCS1-V1_5_WITH_SHA-256" => SIG_SHA256_RSA,
        "SHA384WITHRSA" => SIG_SHA384_RSA,
        "SHA512WITHRSA" => SIG_SHA512_RSA,
        "SHA1WITHRSA" | "SHA-1WITHRSA" => SIG_SHA1_RSA,
        // The nine that were advertised and refused at `sign()`. Both
        // spellings of each, since JCA lookup is case-insensitive and callers
        // write the digest hyphenated as often as not.
        "MD2WITHRSA" => SIG_MD2_RSA,
        "MD5WITHRSA" => SIG_MD5_RSA,
        "SHA224WITHRSA" | "SHA-224WITHRSA" => SIG_SHA224_RSA,
        "SHA512/224WITHRSA" | "SHA-512/224WITHRSA" => SIG_SHA512_224_RSA,
        "SHA512/256WITHRSA" | "SHA-512/256WITHRSA" => SIG_SHA512_256_RSA,
        "SHA3-224WITHRSA" => SIG_SHA3_224_RSA,
        "SHA3-256WITHRSA" => SIG_SHA3_256_RSA,
        "SHA3-384WITHRSA" => SIG_SHA3_384_RSA,
        "SHA3-512WITHRSA" => SIG_SHA3_512_RSA,
        "NONEWITHRSA" => SIG_NONE_RSA,
        "MD5ANDSHA1WITHRSA" => SIG_MD5_SHA1_RSA,
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
        // The rest of the `sun.security.provider.DSA` family — `NONEwithDSA`,
        // `SHA{224,384,512}withDSA`, `SHA3-{224,256,384,512}withDSA` and the
        // nine `inP1363Format` twins. Last, so every explicit arm above wins.
        "HSS/LMS" => SIG_HSS_LMS,
        other if ecdsa_family_spi_class(other).is_some() => SIG_ECDSA_REAL,
        other if dsa_family_spi_class(other).is_some() => SIG_DSA_REAL,
        _ => -1,
    }
}

/// The real SunEC `sun.security.ec.ECDSASignature$*` SPI class for an
/// ECDSA-family signature name, or `None` when the name is not one.
///
/// Derived rather than tabulated, exactly as `dsa_family_spi_class` is, and by
/// the same two substitutions: the digest's `-` becomes `_` (`SHA3-256` ->
/// `$SHA3_256`, since `-` is not a Java identifier character) and `NONE` is
/// spelled `Raw`. Verified against HotSpot 25's own
/// `Security.getProvider("SunEC").getServices()` rather than recalled — every
/// string below appears there verbatim as a service's `getClassName()`.
fn ecdsa_family_spi_class(name: &str) -> Option<&'static str> {
    let upper = name.to_ascii_uppercase();
    let (digest, p1363) = match upper.strip_suffix("INP1363FORMAT") {
        Some(rest) => (rest.strip_suffix("WITHECDSA")?, true),
        None => (upper.as_str().strip_suffix("WITHECDSA")?, false),
    };
    Some(match (digest, p1363) {
        ("NONE", false) => "sun/security/ec/ECDSASignature$Raw",
        ("NONE", true) => "sun/security/ec/ECDSASignature$RawinP1363Format",
        ("SHA1", false) | ("SHA-1", false) => "sun/security/ec/ECDSASignature$SHA1",
        ("SHA1", true) | ("SHA-1", true) => {
            "sun/security/ec/ECDSASignature$SHA1inP1363Format"
        }
        ("SHA224", false) | ("SHA-224", false) => "sun/security/ec/ECDSASignature$SHA224",
        ("SHA224", true) | ("SHA-224", true) => {
            "sun/security/ec/ECDSASignature$SHA224inP1363Format"
        }
        ("SHA256", false) | ("SHA-256", false) => "sun/security/ec/ECDSASignature$SHA256",
        ("SHA256", true) | ("SHA-256", true) => {
            "sun/security/ec/ECDSASignature$SHA256inP1363Format"
        }
        ("SHA384", false) | ("SHA-384", false) => "sun/security/ec/ECDSASignature$SHA384",
        ("SHA384", true) | ("SHA-384", true) => {
            "sun/security/ec/ECDSASignature$SHA384inP1363Format"
        }
        ("SHA512", false) | ("SHA-512", false) => "sun/security/ec/ECDSASignature$SHA512",
        ("SHA512", true) | ("SHA-512", true) => {
            "sun/security/ec/ECDSASignature$SHA512inP1363Format"
        }
        ("SHA3-224", false) => "sun/security/ec/ECDSASignature$SHA3_224",
        ("SHA3-224", true) => "sun/security/ec/ECDSASignature$SHA3_224inP1363Format",
        ("SHA3-256", false) => "sun/security/ec/ECDSASignature$SHA3_256",
        ("SHA3-256", true) => "sun/security/ec/ECDSASignature$SHA3_256inP1363Format",
        ("SHA3-384", false) => "sun/security/ec/ECDSASignature$SHA3_384",
        ("SHA3-384", true) => "sun/security/ec/ECDSASignature$SHA3_384inP1363Format",
        ("SHA3-512", false) => "sun/security/ec/ECDSASignature$SHA3_512",
        ("SHA3-512", true) => "sun/security/ec/ECDSASignature$SHA3_512inP1363Format",
        _ => return None,
    })
}

/// Every ECDSA-family signature name this engine offers, in HotSpot's own
/// spelling — the seed list, kept one set with [`ecdsa_family_spi_class`] by
/// `every_ecdsa_family_signature_name_maps_to_an_spi_class`.
pub(crate) const ECDSA_FAMILY_SIGNATURE_NAMES: &[&str] = &[
    "NONEwithECDSA",
    "SHA1withECDSA",
    "SHA224withECDSA",
    "SHA256withECDSA",
    "SHA384withECDSA",
    "SHA512withECDSA",
    "SHA3-224withECDSA",
    "SHA3-256withECDSA",
    "SHA3-384withECDSA",
    "SHA3-512withECDSA",
    "NONEwithECDSAinP1363Format",
    "SHA1withECDSAinP1363Format",
    "SHA224withECDSAinP1363Format",
    "SHA256withECDSAinP1363Format",
    "SHA384withECDSAinP1363Format",
    "SHA512withECDSAinP1363Format",
    "SHA3-224withECDSAinP1363Format",
    "SHA3-256withECDSAinP1363Format",
    "SHA3-384withECDSAinP1363Format",
    "SHA3-512withECDSAinP1363Format",
];

/// The JDK class each name in [`ECDSA_FAMILY_SIGNATURE_NAMES`] resolves to, in
/// the source spelling `Provider.Service.getClassName()` reports.
pub(crate) fn ecdsa_family_service_class(name: &str) -> Option<String> {
    ecdsa_family_spi_class(name).map(|c| c.replace('/', "."))
}

/// The real JDK `sun.security.provider.DSA$*` SPI class for a DSA-family
/// signature name, or `None` when the name is not one.
///
/// Derived from the name rather than tabulated, because the JDK's own nested
/// class names are the algorithm names with two mechanical substitutions: the
/// digest's `-` becomes `_` (`SHA3-256withDSA` ->
/// `DSA$SHA3_256withDSA`, since `-` is not a Java identifier character), and
/// `NONEwithDSA` is spelled `RawDSA`. Verified against HotSpot 25's own
/// `Security.getProvider("SUN").getServices()` output rather than recalled —
/// every string below appears there verbatim as a service's `getClassName()`.
///
/// The `inP1363Format` variants are NOT a formatting flag this engine could
/// apply itself: they are separate SPI classes emitting the IEEE P1363 fixed-
/// width `r || s` instead of the DER `SEQUENCE`, and the JDK implements them by
/// subclassing. Routing to the class is therefore also what makes the FORMAT
/// right.
fn dsa_family_spi_class(name: &str) -> Option<&'static str> {
    let upper = name.to_ascii_uppercase();
    let (digest, p1363) = match upper.strip_suffix("INP1363FORMAT") {
        Some(rest) => (rest.strip_suffix("WITHDSA")?, true),
        None => (upper.as_str().strip_suffix("WITHDSA")?, false),
    };
    Some(match (digest, p1363) {
        ("NONE", false) => "sun/security/provider/DSA$RawDSA",
        ("NONE", true) => "sun/security/provider/DSA$RawDSAinP1363Format",
        ("SHA1", false) | ("SHA-1", false) => "sun/security/provider/DSA$SHA1withDSA",
        ("SHA1", true) | ("SHA-1", true) => {
            "sun/security/provider/DSA$SHA1withDSAinP1363Format"
        }
        ("SHA224", false) | ("SHA-224", false) => "sun/security/provider/DSA$SHA224withDSA",
        ("SHA224", true) | ("SHA-224", true) => {
            "sun/security/provider/DSA$SHA224withDSAinP1363Format"
        }
        ("SHA256", false) | ("SHA-256", false) => "sun/security/provider/DSA$SHA256withDSA",
        ("SHA256", true) | ("SHA-256", true) => {
            "sun/security/provider/DSA$SHA256withDSAinP1363Format"
        }
        ("SHA384", false) | ("SHA-384", false) => "sun/security/provider/DSA$SHA384withDSA",
        ("SHA384", true) | ("SHA-384", true) => {
            "sun/security/provider/DSA$SHA384withDSAinP1363Format"
        }
        ("SHA512", false) | ("SHA-512", false) => "sun/security/provider/DSA$SHA512withDSA",
        ("SHA512", true) | ("SHA-512", true) => {
            "sun/security/provider/DSA$SHA512withDSAinP1363Format"
        }
        ("SHA3-224", false) => "sun/security/provider/DSA$SHA3_224withDSA",
        ("SHA3-224", true) => "sun/security/provider/DSA$SHA3_224withDSAinP1363Format",
        ("SHA3-256", false) => "sun/security/provider/DSA$SHA3_256withDSA",
        ("SHA3-256", true) => "sun/security/provider/DSA$SHA3_256withDSAinP1363Format",
        ("SHA3-384", false) => "sun/security/provider/DSA$SHA3_384withDSA",
        ("SHA3-384", true) => "sun/security/provider/DSA$SHA3_384withDSAinP1363Format",
        ("SHA3-512", false) => "sun/security/provider/DSA$SHA3_512withDSA",
        ("SHA3-512", true) => "sun/security/provider/DSA$SHA3_512withDSAinP1363Format",
        _ => return None,
    })
}

/// Every DSA-family signature name this engine offers, in the spelling HotSpot
/// advertises. The seed list and [`dsa_family_spi_class`] are kept one set by
/// `every_dsa_family_signature_name_maps_to_an_spi_class`.
pub(crate) const DSA_FAMILY_SIGNATURE_NAMES: &[&str] = &[
    "NONEwithDSA",
    "SHA1withDSA",
    "SHA224withDSA",
    "SHA256withDSA",
    "SHA384withDSA",
    "SHA512withDSA",
    "SHA3-224withDSA",
    "SHA3-256withDSA",
    "SHA3-384withDSA",
    "SHA3-512withDSA",
    "NONEwithDSAinP1363Format",
    "SHA1withDSAinP1363Format",
    "SHA224withDSAinP1363Format",
    "SHA256withDSAinP1363Format",
    "SHA384withDSAinP1363Format",
    "SHA512withDSAinP1363Format",
    "SHA3-224withDSAinP1363Format",
    "SHA3-256withDSAinP1363Format",
    "SHA3-384withDSAinP1363Format",
    "SHA3-512withDSAinP1363Format",
];

/// The JDK class each name in [`DSA_FAMILY_SIGNATURE_NAMES`] resolves to, for
/// the seed rows — `Provider.getServices()` reports the implementation class,
/// and reporting the marker string there would leave the enumeration diff open
/// on twenty rows this change is closing.
pub(crate) fn dsa_family_service_class(name: &str) -> Option<String> {
    // Internal (`/`-separated) to source (`.`-separated) form. The `$` that
    // separates the nested class stays as it is — that is how the JDK spells it
    // in `Provider.Service.getClassName()` too.
    dsa_family_spi_class(name).map(|c| c.replace('/', "."))
}

fn algo_name(idx: i32) -> &'static str {
    match idx {
        SIG_SHA256_RSA => "SHA256withRSA",
        SIG_SHA384_RSA => "SHA384withRSA",
        SIG_SHA512_RSA => "SHA512withRSA",
        SIG_SHA1_RSA => "SHA1withRSA",
        SIG_MD2_RSA => "MD2withRSA",
        SIG_MD5_RSA => "MD5withRSA",
        SIG_SHA224_RSA => "SHA224withRSA",
        SIG_SHA512_224_RSA => "SHA512/224withRSA",
        SIG_SHA512_256_RSA => "SHA512/256withRSA",
        SIG_SHA3_224_RSA => "SHA3-224withRSA",
        SIG_SHA3_256_RSA => "SHA3-256withRSA",
        SIG_SHA3_384_RSA => "SHA3-384withRSA",
        SIG_SHA3_512_RSA => "SHA3-512withRSA",
        SIG_NONE_RSA => "NONEwithRSA",
        SIG_MD5_SHA1_RSA => "MD5andSHA1withRSA",
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
    match sig_slot_get(ctx, this, SIG_OFF_KEYID) {
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
    // The map is keyed `(vm_identity, identity_hash)`: an identity hash is
    // unique only within one heap, and the map is a process-global static.
    if let Some(id) =
        crypto_impl::rsa_realkey_map_get(ctx.vm_identity(), ctx.identity_hash_code(key))
    {
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
    let cur = match sig_slot_get(ctx, this, SIG_OFF_PENDING) {
        Value::Int(n) => n,
        _ => 0,
    };
    sig_slot_set(
        ctx,
        this,
        SIG_OFF_PENDING,
        Value::Int(cur + data.len() as i32),
    );
    let key = sig_key(ctx, this);
    sig_payload_table()
        .lock()
        .entry(key)
        .or_default()
        .extend_from_slice(data);
}

/// Remove the receiver's accumulated payload from the side table.
///
/// Returns `Err(SignatureException)` when the side-table entry is
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
    sig_slot_set(ctx, this, SIG_OFF_PENDING, Value::Int(0));
    let key = sig_key(ctx, this);
    let taken = sig_payload_table().lock().remove(&key);
    match taken {
        Some(bytes) => Ok(bytes),
        // Reached only from `sign`/`verify`, both of which declare
        // `SignatureException`, so the checked class is the catchable one.
        // HotSpot cannot produce this state at all — it means `init*()` never
        // ran through this registrar — but the caller's handler is the same
        // handler either way, and an unchecked throw skips it.
        None => Err(refuse_uninitialized(
            ctx,
            "object not initialized for signature or verification              (Signature payload missing post-GC or init*() never called)",
        )),
    }
}

fn clear_data(ctx: &mut dyn NativeContext, this: ObjectRef) {
    let key = sig_key(ctx, this);
    // Seed an *empty* buffer rather than removing the entry: `take_data`
    // distinguishes "init*() ran, no update() bytes" (Some(empty)) from
    // "never initialised / entry lost" (None → SignatureException).
    sig_payload_table().lock().insert(key, Vec::new());
}

// ---------------------------------------------------------------------------
// Sign / verify dispatch
// ---------------------------------------------------------------------------

/// Hash-then-sign for `SHA*withRSA` family.  RSA's `crypto_impl::Rsa::sign_sha256`
/// hashes internally, so for the SHA-256 variant we use it directly.  For
/// SHA-384/512 we hash first via the `hash_function`-returning helper and
/// then pad-and-modpow through PKCS#1 v1.5.  Out of scope for the probe;
/// the probe is SHA-256 only.
/// The `PSSParameterSpec` a caller installed with
/// `Signature.setParameter(...)`: message digest, MGF1 digest, salt length.
///
/// PSS is the one JCA signature family whose ALGORITHM NAME does not fix its
/// parameters — `Signature.getInstance("RSASSA-PSS")` carries none until
/// `setParameter` supplies them, and JSSE, netty and every TLS stack rely on
/// that. `setParameter` was a no-op here, so the spec never reached the
/// signer: `sign()` used SHA-256/salt-32 whatever was asked for, and `verify()`
/// accepted a signature made under a DIFFERENT spec (measured: sign with
/// SHA-256/32, verify with SHA-512/64 → `true`, where HotSpot answers `false`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PssParams {
    hash: crypto_impl::PssHash,
    mgf_hash: crypto_impl::PssHash,
    salt_len: usize,
}

fn sign_dispatch(alg: i32, key_id: u64, data: &[u8]) -> Option<Vec<u8>> {
    sign_dispatch_with(alg, key_id, data, None)
}

fn sign_dispatch_with(
    alg: i32,
    key_id: u64,
    data: &[u8],
    pss: Option<PssParams>,
) -> Option<Vec<u8>> {
    use cratonvm_native_builtins_crypto::signature::DigestAlgorithm as D;
    if let (Some(p), true) = (pss, is_pss(alg)) {
        return crypto_impl::rsa_sign_pss_ex_by_id(key_id, p.hash, p.mgf_hash, p.salt_len, data);
    }
    match alg {
        SIG_SHA256_RSA => crypto_impl::rsa_sign(key_id, data),
        // SHA-1/384/512 differ from SHA-256 only in the DigestInfo prefix, and
        // the VERIFY side has taken a `DigestAlgorithm` all along. Until this
        // arm existed, `Signature.getInstance("SHA512withRSA").sign()` refused
        // with "this VM has no native implementation for that algorithm" —
        // after `getInstance` had already advertised the name and `initSign`
        // had already accepted the key.
        // ONE arm for every PKCS#1 v1.5 name, resolved through
        // `rsa_pkcs1_digest`. This was four hand-written arms (SHA-1/256/384/
        // 512) and nine advertised names with none, which is why
        // `Signature.getInstance("SHA3-256withRSA").sign()` raised
        // `SignatureException` after `getInstance` had accepted the name and
        // `initSign` had accepted the key — W7-63 §8's second bullet.
        other if rsa_pkcs1_digest(other).is_some() => {
            crypto_impl::rsa_sign_digest(key_id, rsa_pkcs1_digest(other)?, data)
        }
        SIG_MD5_SHA1_RSA => crypto_impl::rsa_sign_md5_sha1(key_id, data),
        SIG_NONE_RSA => crypto_impl::rsa_sign_none(key_id, data),
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

/// Whether `sign_dispatch`/`verify_dispatch` have a native arm for `alg` at
/// all.
///
/// Kept in lock-step with the two `match`es by
/// `tests::natively_dispatched_matches_the_dispatch_arms`. Used only to say
/// *why* a dispatch returned `None`: an algorithm with no arm is "we cannot do
/// this at all", an algorithm with an arm is "the key handle was not in the
/// backing store". Both refuse; the messages differ.
fn natively_dispatched(alg: i32) -> bool {
    matches!(
        alg,
        SIG_SHA256_RSA
            | SIG_SHA1_RSA
            | SIG_SHA384_RSA
            | SIG_SHA512_RSA
            | SIG_MD5_SHA1_RSA
            | SIG_NONE_RSA
            | SIG_PSS_SHA256
            | SIG_PSS_SHA384
            | SIG_PSS_SHA512
            | SIG_SHA256_ECDSA
            | SIG_SHA384_ECDSA
            | SIG_ED25519
    )
}

/// `java.security.SignatureException` — the checked exception that BOTH
/// `Signature.sign()` and `Signature.verify()` declare, so every caller of
/// either can catch it. Deliberately preferred over `NoSuchAlgorithmException`
/// (undeclared here, and the algorithm was already accepted at `getInstance`
/// time) and over `ProviderException` (unchecked, so it escapes the
/// `catch (SignatureException)` that signature-verifying code is written
/// around).
const SIGNATURE_EXCEPTION: &str = "java/security/SignatureException";

/// Refuse an operation on a `Signature` that is not in the state it needs.
///
/// **Every one of these used to be an unchecked `IllegalStateException`, and
/// every one of them is `SignatureException` on HotSpot.** Measured on Temurin
/// 25.0.3+9:
///
/// ```text
/// sign()   on a fresh object      SignatureException: object not initialized for signing
/// sign()   after initVerify       SignatureException: object not initialized for signing
/// verify() on a fresh object      SignatureException: object not initialized for verification
/// verify() after initSign         SignatureException: object not initialized for verification
/// update() on a fresh object      SignatureException: object not initialized for signature or verification
/// sign(byte[],int,int) too small  SignatureException: partial signatures not returned
/// ```
///
/// This is the same defect species as the RSA `BadPaddingException` one door
/// away in `jca/cipher.rs`: `Signature.sign()`, `verify()` and `update()` all
/// DECLARE `SignatureException`, so a caller's `catch (SignatureException e)`
/// is already written and was simply dead against this VM — the failure escaped
/// as an unchecked throw through code that believed it had handled it. Raising
/// the checked class cannot break a caller that compiles today; it can only
/// make a dead handler start working.
///
/// Contrast `javax.crypto.Mac`, where `IllegalStateException` is what HotSpot
/// raises and is what `doFinal` DECLARES (measured:
/// `IllegalStateException: MAC not initialized`). Unchecked is not wrong by
/// itself — it is wrong when the JDK's own signature says otherwise.
fn refuse_uninitialized(
    ctx: &mut dyn NativeContext,
    msg: &str,
) -> cratonvm_types::error::MethodCallFailed {
    crate::phases_early::throw_jca_exc(ctx, SIGNATURE_EXCEPTION, msg)
}

/// Refuse a `sign`/`verify` whose dispatch returned `None`.
///
/// **This is the core of the P0 fix in this file.** `None` from
/// `sign_dispatch`/`verify_dispatch` means the cryptographic question was
/// never asked — either no native arm exists for the algorithm, or the key
/// handle is absent from `crypto_impl`'s key store (see
/// `crypto_impl::rsa_verify`: `guard.get(&id).map(..)`, so `None` is
/// unambiguously "no such key", never a verification outcome).
///
/// Before this, `sign()` did `.unwrap_or_default()` — an **empty `byte[]`
/// presented as a signature** — and `verify()` did `.unwrap_or(false)`, which
/// the caller cannot tell from a forgery. `false` is the answer to "is this
/// signature valid?"; it is not the answer to "we never checked".
fn refuse_unanswerable(
    ctx: &mut dyn NativeContext,
    alg: i32,
    op: &str,
) -> cratonvm_types::error::MethodCallFailed {
    let why = if natively_dispatched(alg) {
        "the key handle is not present in this VM's key store (the key was \
         never registered, or was registered against a different Signature \
         instance)"
    } else {
        "this VM has no native implementation for that algorithm"
    };
    crate::phases_early::throw_jca_exc(
        ctx,
        SIGNATURE_EXCEPTION,
        &format!(
            "Signature.{op} could not be performed for {} ({}): refusing to \
             report a cryptographic result for an operation that never ran.",
            algo_name(alg),
            why
        ),
    )
}

fn is_pss(alg: i32) -> bool {
    matches!(alg, SIG_PSS_SHA256 | SIG_PSS_SHA384 | SIG_PSS_SHA512)
}

fn verify_dispatch(alg: i32, key_id: u64, data: &[u8], sig: &[u8]) -> Option<bool> {
    verify_dispatch_with(alg, key_id, data, sig, None)
}

fn verify_dispatch_with(
    alg: i32,
    key_id: u64,
    data: &[u8],
    sig: &[u8],
    pss: Option<PssParams>,
) -> Option<bool> {
    use cratonvm_native_builtins_crypto::signature::DigestAlgorithm as D;
    if let (Some(p), true) = (pss, is_pss(alg)) {
        return crypto_impl::rsa_verify_pss_ex_by_id(
            key_id, p.hash, p.mgf_hash, p.salt_len, data, sig,
        );
    }
    match alg {
        SIG_SHA256_RSA => crypto_impl::rsa_verify(key_id, data, sig),
        // The verify twin of the sign arm above, through the same table, so
        // the two directions cannot come to disagree about which digest a name
        // means.
        other if rsa_pkcs1_digest(other).is_some() => {
            crypto_impl::rsa_verify_digest(key_id, rsa_pkcs1_digest(other)?, data, sig)
        }
        SIG_MD5_SHA1_RSA => crypto_impl::rsa_verify_md5_sha1(key_id, data, sig),
        SIG_NONE_RSA => crypto_impl::rsa_verify_none(key_id, data, sig),
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
///
/// `ctx` is consulted because `route_ec_to_real()` only says we *prefer* the
/// real SunEC bytecode — it does not say the bytecode is present. Under the
/// synthetic JDK the named class would be a fabricated, code-less stub, and
/// `jca::key_factory::real_ec_keypair_available` makes EC keygen fall back to
/// `crypto_impl` in exactly that case. The two checks must agree: a synthetic
/// EC key carries a `crypto_impl` `key_id` that a SunEC SPI cannot read, and a
/// real `ECPrivateKeyImpl` carries no `key_id` for the synthetic dispatch. Both
/// keys and both signature operations therefore key off the same question.
fn ecdsa_real_spi_class(ctx: &dyn NativeContext, alg: i32, name: &str) -> Option<&'static str> {
    if !crate::route_ec_to_real() {
        return None;
    }
    let cls = match alg {
        SIG_SHA256_ECDSA => "sun/security/ec/ECDSASignature$SHA256",
        SIG_SHA384_ECDSA => "sun/security/ec/ECDSASignature$SHA384",
        SIG_SHA512_ECDSA => "sun/security/ec/ECDSASignature$SHA512",
        // `SIG_ECDSA_REAL` carries the other seventeen and resolves its class
        // from the caller's spelling — see `ecdsa_family_spi_class`. The three
        // above keep their own indices so nothing that already resolves
        // changes route.
        SIG_ECDSA_REAL => ecdsa_family_spi_class(name)?,
        _ => return None,
    };
    if ctx.would_fabricate_synthetic_stub(cls) {
        return None;
    }
    Some(cls)
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
///
/// `SIG_DSA_REAL` covers the other eighteen names of the family and resolves its
/// SPI from the caller's own spelling, which is why this takes `ctx`/`this`:
/// one index, eighteen classes, and `sig_algorithm_name` is where the spelling
/// survived (the `algorithm` field `getInstance` set).
fn dsa_real_spi_class(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    alg: i32,
) -> Option<&'static str> {
    if !crate::route_dsa_to_real() {
        return None;
    }
    match alg {
        SIG_SHA256_DSA => Some("sun/security/provider/DSA$SHA256withDSA"),
        SIG_SHA1_DSA => Some("sun/security/provider/DSA$SHA1withDSA"),
        SIG_DSA_REAL => dsa_family_spi_class(&sig_algorithm_name(ctx, this)),
        // Not a DSA class, but the same drive and the same reason — routed
        // here so it shares `drive_real_signature_spi` rather than growing a
        // parallel copy of it.
        SIG_HSS_LMS => Some("sun/security/provider/HSS"),
        _ => None,
    }
}

/// Drive the real SunEC `ECDSASignature$*` SPI: `new` → `engineInitSign/Verify(key)`
/// → `engineUpdate(buffer)` → `engineSign()`/`engineVerify(sig)`. `verify_sig`
/// `None` → sign (returns the DER `byte[]`); `Some(sig)` → verify (returns
/// `Int(0/1)`). The real key is read from `SIG_OFF_KEYOBJ`; the payload from the
/// identity-hash-keyed side table via `take_data`.
/// The algorithm string this `Signature` was created for, as the caller spelled
/// it. `algo_name(idx)` answers `"Unknown"` for anything outside this engine's
/// own table, which is exactly the case a chain fallback has to name.
fn sig_algorithm_name(ctx: &mut dyn NativeContext, this: ObjectRef) -> String {
    if let Value::Object(Some(s)) = ctx.get_field_by_name(this, "algorithm") {
        if let Some(text) = ctx.read_string(s) {
            if !text.is_empty() {
                return text;
            }
        }
    }
    get_sig_algo(ctx, this)
        .map(algo_name)
        .unwrap_or("Unknown")
        .to_string()
}

/// Construct `spi_class`, init it with `key`, feed it `data`, and sign or
/// verify. The body `drive_real_signature_spi` used to inline, lifted out so a
/// fallback can run it against a different class without re-taking the data
/// (`take_data` is destructive — a second call would sign an empty buffer).
fn drive_spi_class_with(
    ctx: &mut dyn NativeContext,
    spi_class: &str,
    key: ObjectRef,
    data: &[u8],
    verify_sig: Option<&[u8]>,
) -> MethodCallResult {
    let key_pin = ctx.pin_native_root(key);
    let result = (|| {
        let spi = match ctx.new_object_initialized(spi_class, "()V", &[])? {
            Some(Value::Object(Some(o))) => o,
            _ => {
                return Err(RuntimeError::NotImplemented {
                    feature: spi_class.to_string(),
                }
                .into())
            }
        };
        let spi_pin = ctx.pin_native_root(spi);
        let key = ctx.read_native_pin(key_pin, key);
        let (init_m, init_desc) = if verify_sig.is_some() {
            ("engineInitVerify", "(Ljava/security/PublicKey;)V")
        } else {
            ("engineInitSign", "(Ljava/security/PrivateKey;)V")
        };
        ctx.invoke_virtual(spi, init_m, init_desc, &[Value::Object(Some(key))])?;
        let spi = ctx.read_native_pin(spi_pin, spi);
        let arr = alloc_byte_array(ctx, data);
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
                let sigarr = alloc_byte_array(ctx, sig_bytes);
                let spi = ctx.read_native_pin(spi_pin, spi);
                let ok = ctx.invoke_virtual(
                    spi,
                    "engineVerify",
                    "([B)Z",
                    &[Value::Object(Some(sigarr))],
                )?;
                Ok(match ok {
                    Some(Value::Int(n)) => Some(Value::Int(i32::from(n != 0))),
                    _ => Some(Value::Int(0)),
                })
            }
            None => ctx.invoke_virtual(spi, "engineSign", "()[B", &[]),
        }
    })();
    ctx.unpin_native_roots(key_pin);
    result
}

/// The JDK's delayed provider selection, applied where this engine can observe
/// the refusal.
///
/// `java.security.Signature.getInstance(alg)` does NOT bind a provider: the
/// returned `Signature$Delegate` picks one at `initSign`/`initVerify` time and
/// moves to the next provider whenever the current one refuses the key. This VM
/// binds eagerly — a JDK SPI for the algorithm name — so a key minted by another
/// provider reaches an SPI that will not take it and the call dies there. Two
/// measured shapes, both from bc-java:
///
/// * `its` — BouncyCastle's `ECDSA` generator mints keys whose `getAlgorithm()`
///   is `"ECDSA"`, and `sun.security.ec.ECKeyFactory.checkKey` requires exactly
///   `"EC"`: `InvalidKeyException: Not an EC key: ECDSA`.
/// * `tsp`/`eac` — an algorithm this engine has no table entry for at all, so
///   `sign()`/`verify()` refused with "could not be performed for Unknown"
///   while BouncyCastle implements it.
///
/// Returns `None` when no installed third-party provider offers the name, in
/// which case the caller's own refusal stands unchanged.
fn try_chain_signature_spi(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    key: Option<ObjectRef>,
    data: &[u8],
    verify_sig: Option<&[u8]>,
) -> Option<MethodCallResult> {
    let key = key?;
    let alg = sig_algorithm_name(ctx, this);
    let classes = crate::jca::provider_chain::chain_third_party_service_classes("Signature", &alg);
    for spi_class in classes {
        let pin = ctx.pin_native_root(key);
        let key_now = ctx.read_native_pin(pin, key);
        let attempt = drive_spi_class_with(ctx, &spi_class, key_now, data, verify_sig);
        ctx.unpin_native_roots(pin);
        if attempt.is_ok() {
            return Some(attempt);
        }
    }
    None
}

/// The key object stashed at `init` time, if any.
fn sig_key_object(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    let base = synthetic_base_offset(ctx, "java/security/Signature");
    match sig_slot_get(ctx, this, SIG_OFF_KEYOBJ) {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    }
}

fn drive_real_signature_spi(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    spi_class: &'static str,
    verify_sig: Option<Vec<u8>>,
) -> MethodCallResult {
    let Some(key) = sig_key_object(ctx, this) else {
        return Err(refuse_uninitialized(
            ctx,
            "object not initialized for signature or verification (no EC key)",
        ));
    };
    let data = take_data(ctx, this)?;
    let this_pin = ctx.pin_native_root(this);
    let key_pin = ctx.pin_native_root(key);
    let attempt = drive_spi_class_with(ctx, spi_class, key, &data, verify_sig.as_deref());
    let this = ctx.read_native_pin(this_pin, this);
    let key = ctx.read_native_pin(key_pin, key);
    ctx.unpin_native_roots(this_pin);
    match attempt {
        Ok(v) => Ok(v),
        // The bound JDK SPI refused. Do what the JDK's own delayed provider
        // selection does and offer the key to the next provider that claims the
        // algorithm — see `try_chain_signature_spi`.
        Err(refusal) => {
            match try_chain_signature_spi(ctx, this, Some(key), &data, verify_sig.as_deref()) {
                Some(result) => result,
                None => Err(refusal),
            }
        }
    }
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
    let key = match sig_slot_get(ctx, this, SIG_OFF_KEYOBJ) {
        Value::Object(Some(o)) => o,
        _ => {
            return Err(refuse_uninitialized(
                ctx,
                "object not initialized for signature or verification (no ML-DSA key)",
            ))
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

/// Whether `Signature.getInstance` may hand back a receiver for `name`.
///
/// `Signature.getInstance` used to accept **every string**. Measured against a
/// running binary in both arms (W7-29-jca-advertise-implement-gaps.md
/// residual 5): `getInstance("ML-KEM")`, `("AES")`, `("HmacSHA256")`,
/// `("NO-SUCH-SIG")` and `("")` all returned a live object whose
/// `getAlgorithm()` was the sentinel `"Unknown"`, where HotSpot 25 raises
/// `NoSuchAlgorithmException: <name> Signature not available` for each.
///
/// This is not the `Cipher`/`Mac` species — the engine does not fabricate a
/// cryptographic result, because `sign_dispatch`/`verify_dispatch` refuse an
/// unrecognised index and `refuse_unanswerable` converts that into a
/// `SignatureException` rather than an empty signature or a bare `false`. It
/// is a **deferred and mistyped refusal**, which is its own harm: a caller
/// writing the ordinary
///
/// ```java
/// try { s = Signature.getInstance(name); } catch (NoSuchAlgorithmException e) { fallback(); }
/// ```
///
/// takes the wrong branch, concludes the algorithm is available, and meets the
/// failure much later at a point where its `catch` clauses are written for a
/// bad signature rather than a missing algorithm. Probing an engine for a name
/// it may not have is ordinary library behaviour.
///
/// **The gate is a DISJUNCTION, and W7-29's prescription — gate on
/// `find_service_provider("Signature", algo)` alone — would have been a
/// regression.** The registry is seeded with friendly names only, while
/// `algo_idx` deliberately also carries the signature-algorithm OIDs
/// (`1.2.840.113549.1.1.11` and neighbours) because X.509 `cert.verify()`
/// resolves `Signature.getInstance(signatureAlgorithm.getId())` by OID, not by
/// friendly name. A registry-only gate refuses every one of those at
/// `getInstance` and breaks certificate verification outright — the same
/// shape as this record's other correction, where a record's observation is
/// right and its prescribed fix has been overtaken.
///
/// So: accept a name this engine has a concept of (`idx >= 0`), OR a name some
/// provider in the live chain advertises. Nothing else. That closes both
/// directions at once — no unadvertised-and-unimplemented name is served, and
/// no advertised name is refused, which is what
/// `every_advertised_signature_name_is_offered_by_get_instance` ratchets.
///
/// A name that is advertised but has no `algo_idx` arm (`SHA3-256withRSA` and
/// eight neighbours on `SunRsaSign`) still gets a receiver and still fails at
/// `sign()`/`verify()` with the checked `SignatureException`. That is an
/// ordinary unimplemented-algorithm gap and NOT this record's species — the
/// name is real, the advertisement is truthful, and the failure is closed and
/// catchable. Narrowing it belongs to whoever implements those arms.
/// W7-63-jca-advertise-vs-serve.md.
fn signature_name_is_offered(idx: i32, name: &str) -> bool {
    idx >= 0 || crate::jca::provider_chain::find_service_provider("Signature", name).is_some()
}

/// The same gate, by name only, for the provider-chain ratchet — which owns
/// the seed lists and the `#[cfg(test)]` lock that serialises the process-wide
/// service map, so the test has to live over there.
pub(crate) fn get_instance_offers(name: &str) -> bool {
    signature_name_is_offered(algo_idx(name), name)
}

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
    if !signature_name_is_offered(idx, &alg) {
        return Err(crate::jca::provider_chain::throw_no_such_algorithm_public(
            ctx,
            &format!("{alg} Signature not available"),
        ));
    }
    // A THIRD-PARTY provider's own implementation class, if this lookup names
    // one. Resolved here (not at `initSign`) because the SPI instance has to
    // exist before `setParameter`, which callers do first.
    let requested_provider = match args.get(1) {
        Some(Value::Object(Some(p))) => {
            let is_string = ctx
                .class_name_of_id(ctx.class_id_of_object(*p))
                .is_some_and(|n| n == "java/lang/String");
            if is_string {
                ctx.read_string(*p)
            } else {
                match ctx.invoke_virtual(*p, "getName", "()Ljava/lang/String;", &[]) {
                    Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
                    _ => None,
                }
            }
        }
        _ => None,
    };
    let user_spi = crate::jca::provider_chain::third_party_service_class(
        requested_provider.as_deref(),
        "Signature",
        &alg,
    );
    let base = synthetic_base_offset(ctx, "java/security/Signature");
    let obj =
        try_alloc_concurrent_synthetic(ctx, "java/security/Signature", base + SIG_PRIVATE_SLOTS)?;
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
    if let Some(spi_class) = user_spi {
        // Construct the application's SPI now, exactly as
        // `Provider.Service.newInstance` does. A provider whose class cannot
        // be constructed is not a usable provider — but that is the
        // APPLICATION's class, so let its failure escape rather than falling
        // back to the native engine and signing with the wrong thing.
        let obj_pin = ctx.pin_native_root(obj);
        let spi = match ctx.new_object_initialized(&spi_class, "()V", &[])? {
            Some(Value::Object(Some(o))) => o,
            _ => {
                ctx.unpin_native_roots(obj_pin);
                return Err(crate::jca::provider_chain::throw_no_such_algorithm_public(
                    ctx,
                    &format!(
                        "{alg} Signature: provider class {spi_class} could not be instantiated"
                    ),
                ));
            }
        };
        let obj = ctx.read_native_pin(obj_pin, obj);
        ctx.unpin_native_roots(obj_pin);
        let owner = requested_provider.unwrap_or_else(|| {
            crate::jca::provider_chain::find_service_provider("Signature", &alg).unwrap_or_default()
        });
        // THE JDK'S OWN RULE, which this engine did not implement.
        // `Signature.getInstance` wraps the SPI in a `Signature$Delegate` ONLY
        // when the SPI is not already a `Signature`:
        //
        //     if (instance.impl instanceof Signature sig) { sig.algorithm = ...;
        //     } else { sig = new Delegate((SignatureSpi) instance.impl, ...); }
        //     sig.provider = instance.provider; return sig;
        //
        // Wrapping unconditionally is not a cosmetic difference: a provider
        // extends `java.security.Signature` PRECISELY so callers can cast the
        // result to its own interface. BouncyCastle's stateful PQC signers do
        // exactly that — `XMSSSignatureSpi` extends `Signature` and implements
        // `StateAwareSignature`, and every caller opens with
        //
        //     (StateAwareSignature) Signature.getInstance(oid, "BCPQC")
        //
        // which on this VM was `ClassCastException: class java.security
        // .Signature cannot be cast to ...StateAwareSignature`, deterministically,
        // on the first call (`pqc.jcajce.provider.test.XMSSTest.testExhaustion`,
        // `.testKeyExtraction`; HotSpot returns
        // `XMSSSignatureSpi$withSha256`). The failing cast also emitted the
        // reclaim guard's `in_published_snapshot=false site="checkcast"` line,
        // which `op_checkcast` prints for EVERY failed cast and which reads as
        // a collector defect — it is not one; there had been no collection at
        // all when the probe reproduced this.
        //
        // The unwrapped object is its own SPI: `sig_user_spi_obj` answers
        // `this` for it, so `initSign`/`update`/`sign`/`verify` forward to the
        // provider's own `engine*` methods, and the private slots are skipped
        // (`sig_slots_are_ours`) because they would land on the subclass's
        // fields. All of the state this engine keeps for it lives in the
        // identity-keyed side tables, which do not care whose class it is.
        if spi_is_signature_subclass(ctx, spi) {
            let spi_pin = ctx.pin_native_root(spi);
            let algo_str = ctx.create_string(&alg);
            let spi = ctx.read_native_pin(spi_pin, spi);
            ctx.set_field_by_name(spi, "algorithm", Value::Object(Some(algo_str)));
            ctx.unpin_native_roots(spi_pin);
            set_sig_algo(ctx, spi, idx);
            set_sig_state(ctx, spi, STATE_UNINIT);
            set_sig_keyid(ctx, spi, 0);
            let key = sig_key(ctx, spi);
            sig_user_spi_table().lock().insert(key, (owner, spi_class));
            return Ok(Some(Value::Object(Some(spi))));
        }
        ctx.set_field(obj, base + SIG_OFF_SPIOBJ, Value::Object(Some(spi)));
        let key = sig_key(ctx, obj);
        sig_user_spi_table().lock().insert(key, (owner, spi_class));
        return Ok(Some(Value::Object(Some(obj))));
    }
    Ok(Some(Value::Object(Some(obj))))
}

/// Is `spi` a `java.security.Signature` subclass rather than a bare
/// `SignatureSpi`?
///
/// The JDK asks `instance.impl instanceof Signature` and this is the same
/// question. `java.security.Signature` itself does not count: this engine's
/// own synthetic is that class, and a provider registering it verbatim would
/// have no `engine*` overrides to forward to.
fn spi_is_signature_subclass(ctx: &mut dyn NativeContext, spi: ObjectRef) -> bool {
    let mut cid = ctx.class_id_of_object(spi);
    if ctx
        .class_name_of_id(cid)
        .is_some_and(|n| n == "java/security/Signature")
    {
        return false;
    }
    while let Some(parent) = ctx.superclass_of(cid) {
        if ctx
            .class_name_of_id(parent)
            .is_some_and(|n| n == "java/security/Signature")
        {
            return true;
        }
        cid = parent;
    }
    false
}

/// Forward one call to the application's `SignatureSpi`, pinning the receiver
/// across it. Any exception the SPI raises propagates verbatim — that is the
/// contract callers such as netty's provider search depend on
/// (`InvalidKeyException` from `engineInitSign` is how it learns to try the
/// next provider).
fn user_spi_call(
    ctx: &mut dyn NativeContext,
    spi: ObjectRef,
    method: &str,
    desc: &str,
    args: &[Value],
) -> MethodCallResult {
    ctx.invoke_virtual(spi, method, desc, args)
}

/// Complete a `sign()`/`verify()` on an application `SignatureSpi`: hand it the
/// buffered `update()` payload, then ask it for the answer.
///
/// The payload is buffered here rather than forwarded per `update()` call for
/// the same reason `drive_real_signature_spi` does it — one `engineUpdate`
/// with the whole message is what every `SignatureSpi` implementation
/// supports, and it keeps `update()` free of a second code path.
fn drive_user_spi(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    spi: ObjectRef,
    verify_sig: Option<Vec<u8>>,
) -> MethodCallResult {
    let data = take_data(ctx, this)?;
    let spi_pin = ctx.pin_native_root(spi);
    let result = (|| {
        if !data.is_empty() {
            let arr = alloc_byte_array(ctx, &data);
            let spi = ctx.read_native_pin(spi_pin, spi);
            user_spi_call(
                ctx,
                spi,
                "engineUpdate",
                "([BII)V",
                &[
                    Value::Object(Some(arr)),
                    Value::Int(0),
                    Value::Int(data.len() as i32),
                ],
            )?;
        }
        let spi = ctx.read_native_pin(spi_pin, spi);
        match verify_sig {
            Some(sig_bytes) => {
                let sigarr = alloc_byte_array(ctx, &sig_bytes);
                let spi = ctx.read_native_pin(spi_pin, spi);
                let ok = user_spi_call(
                    ctx,
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
            None => user_spi_call(ctx, spi, "engineSign", "()[B", &[]),
        }
    })();
    ctx.unpin_native_roots(spi_pin);
    result
}

/// `java.security.InvalidKeyException` — what BOTH `initSign` and `initVerify`
/// declare, and what HotSpot raises for a key its provider cannot use.
///
/// **`init*` is where a caller decides which provider to use.** netty's
/// `JdkDelegatingPrivateKeyMethod.findCompatibleSignature` asks the default
/// provider first, `catch (InvalidKeyException)`, and only then walks
/// `Security.getProviders()` looking for one that accepts the key — which is
/// how an opaque `PrivateKey` (`getEncoded() == null`, the entire point of
/// `OpenSslPrivateKeyMethod`) finds the application provider that CAN sign
/// with it. Accepting the key here and refusing at `sign()` instead makes that
/// search stop at the first provider and commit to one that can never work:
/// the exception then arrives from inside the TLS callback, not from the
/// probe, and the handshake fails.
///
/// Measured on Temurin 25.0.3+9 with a `PrivateKey` whose `getEncoded()`
/// returns null: `SHA*withRSA` → `InvalidKeyException: Missing key encoding`,
/// `RSASSA-PSS` → `InvalidKeyException: key must be RSAPrivateKey`.
fn refuse_unusable_key(
    ctx: &mut dyn NativeContext,
    alg: i32,
) -> cratonvm_types::error::MethodCallFailed {
    crate::phases_early::throw_jca_exc(
        ctx,
        "java/security/InvalidKeyException",
        &format!(
            "Missing key encoding: this key carries no material this VM can use for \
             {} (getEncoded() returned null, or the key was created by another provider)",
            algo_name(alg)
        ),
    )
}

/// The algorithms whose ONLY route is the synthetic `crypto_impl` RSA key
/// store, so a key that is not in that store cannot be used at all.
///
/// Deliberately narrow: EC/EdDSA/ML-DSA/DSA are driven through the real JDK
/// SPI with the key OBJECT (`SIG_OFF_KEYOBJ`), where a zero `key_id` is normal
/// and the real SPI raises its own `InvalidKeyException`.
fn needs_registered_rsa_key(alg: i32) -> bool {
    matches!(
        alg,
        SIG_SHA256_RSA
            | SIG_SHA1_RSA
            | SIG_SHA384_RSA
            | SIG_SHA512_RSA
            | SIG_MD5_SHA1_RSA
            | SIG_NONE_RSA
            | SIG_PSS_SHA256
            | SIG_PSS_SHA384
            | SIG_PSS_SHA512
    )
}

fn sig_init_sign(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    if let Some(spi) = sig_user_spi_obj(ctx, this) {
        // The application's own SPI decides whether it can use this key, and
        // its `InvalidKeyException` must reach the caller unchanged: a
        // provider search (netty's `findCompatibleSignature`) is written
        // around catching exactly that to move on to the next provider.
        let key = match args.get(1) {
            Some(Value::Object(Some(k))) => Value::Object(Some(*k)),
            _ => Value::Object(None),
        };
        // `initSign(key, random)` and `initSign(key)` share this native, and the
        // random is not decoration: `SignatureSpi.engineInitSign(key, random)`
        // stores it as `appRandom`, and that is where a signer takes its
        // per-signature nonce from. Dropping it left BouncyCastle's ECDSA
        // drawing `k` from its own source, so a test that pins `k` with a
        // deterministic random got a DIFFERENT `r` on every run —
        // `DSATest.testECDSA239bitPrime`, "r component wrong", where HotSpot
        // reproduces the published J.3.2 vector exactly.
        match args.get(2) {
            Some(Value::Object(Some(random))) => {
                let random = Value::Object(Some(*random));
                user_spi_call(
                    ctx,
                    spi,
                    "engineInitSign",
                    "(Ljava/security/PrivateKey;Ljava/security/SecureRandom;)V",
                    &[key, random],
                )?;
            }
            _ => {
                user_spi_call(
                    ctx,
                    spi,
                    "engineInitSign",
                    "(Ljava/security/PrivateKey;)V",
                    &[key],
                )?;
            }
        }
        set_sig_state(ctx, this, STATE_SIGN);
        clear_data(ctx, this);
        return Ok(None);
    }
    let base = synthetic_base_offset(ctx, "java/security/Signature");
    set_sig_state(ctx, this, STATE_SIGN);
    sig_slot_set(ctx, this, SIG_OFF_STATE, Value::Int(STATE_SIGN));
    sig_slot_set(ctx, this, SIG_OFF_PENDING, Value::Int(0));
    if let Some(Value::Object(Some(k))) = args.get(1) {
        // `k` borrows `args`. Take a local: the register_* funnel below
        // allocates, and both the `extract_key_id_from_key` re-read and
        // the SIG_OFF_KEYOBJ store after it must use the POST-move ref --
        // that field is GC-scanned, so a stale ref there is durable.
        let mut k = *k;
        let mut kid = extract_key_id_from_key(ctx, k);
        let alg = get_sig_algo(ctx, this).unwrap_or(-1);
        if needs_registered_rsa_key(alg) && !crypto_impl::rsa_key_registered(kid) {
            // A key this VM did not mint. Import it from the standard
            // `java.security.interfaces.RSA{Private,Public}Key` accessors
            // before refusing — the JDK's own engines consume any provider's
            // key that exposes that interface, and so must ours. This became
            // load-bearing the moment `KeyPairGenerator.getInstance(alg, "BC")`
            // started returning BouncyCastle's OWN keys: a `BCRSAPrivateCrtKey`
            // handed to an ANONYMOUS `Signature.getInstance("SHA256withRSA")`
            // was refused with "Missing key encoding" while HotSpot signs with
            // it through SunRsaSign, and bc-java's `cmp` suite does exactly
            // that pairing. `register_rsa_priv_sign_material` no-ops for a key with no
            // usable accessors, so a genuinely OPAQUE key still lands on the
            // refusal below — which is the behaviour netty's provider search
            // depends on.
            crate::jca::key_factory::register_rsa_priv_sign_material(ctx, &mut k);
            kid = extract_key_id_from_key(ctx, k);
            if !crypto_impl::rsa_key_registered(kid) {
                return Err(refuse_unusable_key(ctx, alg));
            }
        }
        set_sig_keyid(ctx, this, kid);
        sig_slot_set(ctx, this, SIG_OFF_KEYID, Value::Long(kid as i64));
        // Stash the real key object for the SunEC ECDSA drive path (slot is
        // GC-scanned, so the ref survives init→update→sign relocations).
        sig_slot_set(ctx, this, SIG_OFF_KEYOBJ, Value::Object(Some(k)));
    }
    clear_data(ctx, this);
    Ok(None)
}

fn sig_init_verify(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    if let Some(spi) = sig_user_spi_obj(ctx, this) {
        let key = match args.get(1) {
            Some(Value::Object(Some(k))) => Value::Object(Some(*k)),
            _ => Value::Object(None),
        };
        user_spi_call(
            ctx,
            spi,
            "engineInitVerify",
            "(Ljava/security/PublicKey;)V",
            &[key],
        )?;
        set_sig_state(ctx, this, STATE_VERIFY);
        clear_data(ctx, this);
        return Ok(None);
    }
    let base = synthetic_base_offset(ctx, "java/security/Signature");
    set_sig_state(ctx, this, STATE_VERIFY);
    sig_slot_set(ctx, this, SIG_OFF_STATE, Value::Int(STATE_VERIFY));
    sig_slot_set(ctx, this, SIG_OFF_PENDING, Value::Int(0));
    if let Some(Value::Object(Some(k))) = args.get(1) {
        // `k` borrows `args`. Take a local: the register_* funnel below
        // allocates, and both the `extract_key_id_from_key` re-read and
        // the SIG_OFF_KEYOBJ store after it must use the POST-move ref --
        // that field is GC-scanned, so a stale ref there is durable.
        let mut k = *k;
        let mut kid = extract_key_id_from_key(ctx, k);
        let alg = get_sig_algo(ctx, this).unwrap_or(-1);
        if needs_registered_rsa_key(alg) && !crypto_impl::rsa_key_registered(kid) {
            // A key this VM did not mint. Import it from the standard
            // `java.security.interfaces.RSA{Private,Public}Key` accessors
            // before refusing — the JDK's own engines consume any provider's
            // key that exposes that interface, and so must ours. This became
            // load-bearing the moment `KeyPairGenerator.getInstance(alg, "BC")`
            // started returning BouncyCastle's OWN keys: a `BCRSAPrivateCrtKey`
            // handed to an ANONYMOUS `Signature.getInstance("SHA256withRSA")`
            // was refused with "Missing key encoding" while HotSpot signs with
            // it through SunRsaSign, and bc-java's `cmp` suite does exactly
            // that pairing. `register_rsa_pub_verify_material` no-ops for a key with no
            // usable accessors, so a genuinely OPAQUE key still lands on the
            // refusal below — which is the behaviour netty's provider search
            // depends on.
            crate::jca::key_factory::register_rsa_pub_verify_material(ctx, &mut k);
            kid = extract_key_id_from_key(ctx, k);
            if !crypto_impl::rsa_key_registered(kid) {
                return Err(refuse_unusable_key(ctx, alg));
            }
        }
        set_sig_keyid(ctx, this, kid);
        sig_slot_set(ctx, this, SIG_OFF_KEYID, Value::Long(kid as i64));
        // Stash the real key object for the SunEC ECDSA drive path (slot is
        // GC-scanned, so the ref survives init→update→sign relocations).
        sig_slot_set(ctx, this, SIG_OFF_KEYOBJ, Value::Object(Some(k)));
    }
    clear_data(ctx, this);
    Ok(None)
}

/// `Signature.initVerify(Certificate)` — extract the certificate's public key,
/// then initialise exactly as the `PublicKey` overload does.
///
/// This overload was registered against `sig_init_verify` itself, whose body
/// reads `args[1]` as the KEY. So the certificate was stored as the
/// verification key and forwarded to the SPI, where it surfaced differently per
/// algorithm and never as itself:
///
/// * ECDSA — `ECKeyFactory.toECKey` calls `key.getAlgorithm()`, which
///   `sun.security.x509.X509CertImpl` does not declare (that method is
///   `java.security.Key`'s), so `NoSuchMethodError:
///   sun.security.x509.X509CertImpl.getAlgorithm()Ljava/lang/String;`;
/// * EdDSA — `EdDSASignature.engineInitVerify` rejects the non-key outright
///   with `InvalidKeyException: Unsupported key type`.
///
/// Both messages point at the SPI and neither names the real defect, which is
/// why the known-issue page recorded them as two separate causes (a missing
/// `X509CertImpl` accessor, and unimplemented Ed25519/Ed448 verification). They
/// are one wrong argument. `io.netty.pkitesting.CertificateBuilderTest`'s five
/// `createCertIssuedBy*` tests all reach it through
/// `signature.initVerify(root.getCertificate())`.
///
/// The key-usage check mirrors `java.security.Signature.initVerify(Certificate)`
/// exactly: a certificate whose KeyUsage extension is present AND explicitly
/// denies `digitalSignature` (bit 0) must be refused. A `null` key usage — no
/// such extension — is silently allowed, which is what makes this a no-op for
/// every certificate that does not carry the extension.
fn sig_init_verify_cert(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let cert = match args.get(1) {
        Some(Value::Object(Some(c))) => *c,
        // A null certificate: let the key-taking body raise whatever it raises
        // for a null argument rather than inventing a different exception.
        _ => return sig_init_verify(ctx, args),
    };
    let cert_pin = ctx.pin_native_root(cert);
    let usage_denies_signing = match ctx.invoke_virtual(cert, "getKeyUsage", "()[Z", &[]) {
        Ok(Some(Value::Object(Some(arr)))) => {
            ctx.array_length(arr) > 0 && ctx.get_array_element(arr, 0) == Value::Int(0)
        }
        // `getKeyUsage` is not on `Certificate`, only on `X509Certificate`; a
        // non-X509 certificate simply has no usage bits to consult.
        _ => false,
    };
    if usage_denies_signing {
        ctx.unpin_native_roots(cert_pin);
        return Err(crate::phases_early::throw_jca_exc(
            ctx,
            "java/security/InvalidKeyException",
            "Wrong key usage",
        ));
    }
    let cert = ctx.read_native_pin(cert_pin, cert);
    let key = ctx.invoke_virtual(cert, "getPublicKey", "()Ljava/security/PublicKey;", &[]);
    ctx.unpin_native_roots(cert_pin);
    let key = match key? {
        Some(v @ Value::Object(Some(_))) => v,
        // Fail CLOSED. Passing the certificate on (the old behaviour) is what
        // produced the misleading SPI errors above, and silently initialising
        // with no key would let `verify()` answer on nothing.
        _ => {
            return Err(crate::phases_early::throw_jca_exc(
                ctx,
                "java/security/InvalidKeyException",
                "certificate has no public key",
            ))
        }
    };
    let this = this_arg(args)?;
    sig_init_verify(ctx, &[Value::Object(Some(this)), key])
}

/// Route an `update()` payload: straight to an application `SignatureSpi` when
/// this `Signature` wraps one, otherwise into the accumulator that
/// `drive_real_signature_spi` / `drive_user_spi` flush at `sign()`/`verify()`.
///
/// A provider's SPI has to SEE the updates as they happen. `SignatureSpi`
/// implementations key real behaviour off "am I in the middle of a message":
/// BouncyCastle's ML-DSA and SLH-DSA services refuse `engineSetParameter` with
/// `ProviderException: cannot call setParameter in the middle of update`, and
/// with every byte withheld until `sign()` the signer never was in the middle
/// of one, so the refusal never came
/// (`SignatureSetParameterTest.testSetParameterMidUpdateStillRejected`).
///
/// Buffering stays for this VM's own engines, where nothing can observe the
/// difference. The two paths compose rather than race: which one a payload
/// takes is decided by whether an SPI is attached, an SPI is attached at
/// `initSign`/`initVerify` and never detached, and `drive_user_spi` flushes
/// any accumulator content BEFORE asking the SPI for the answer — so bytes
/// buffered before an SPI existed still reach it in order.
fn sig_append_or_forward(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    bytes: &[u8],
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    if bytes.is_empty() {
        return Ok(());
    }
    let Some(spi) = sig_user_spi_obj(ctx, this) else {
        append_data(ctx, this, bytes);
        return Ok(());
    };
    // Record an EMPTY payload rather than nothing at all. `take_data` reads the
    // accumulator's presence as "this receiver was initialised through this
    // registrar" and raises `SignatureException: object not initialized` on a
    // miss, and for the paths where `clear_data` does not run at init time the
    // entry was being created as a side effect of the first `append_data`.
    // Forwarding without this left `sign()` on an otherwise healthy delegated
    // signer refusing itself — measured as five `RuntimeOperatorException:
    // exception obtaining signature` in `cms`.
    append_data(ctx, this, &[]);
    let pin = ctx.pin_native_root(spi);
    let arr = alloc_byte_array(ctx, bytes);
    let spi = ctx.read_native_pin(pin, spi);
    let result = user_spi_call(
        ctx,
        spi,
        "engineUpdate",
        "([BII)V",
        &[
            Value::Object(Some(arr)),
            Value::Int(0),
            Value::Int(bytes.len() as i32),
        ],
    );
    ctx.unpin_native_roots(pin);
    result.map(|_| ())
}

fn sig_update_byte(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    // `Signature.update` DECLARES `SignatureException`, and HotSpot raises it
    // here: measured `SignatureException: object not initialized for signature
    // or verification` for every `update` overload on a fresh object. This VM
    // appended the bytes and returned — it raised NOTHING — so an
    // update-before-init went unnoticed until `sign()` produced a signature
    // over data the caller never meant to sign.
    require_initialized_for_update(ctx, this)?;
    let b = match args.get(1) {
        Some(Value::Int(v)) => *v as u8,
        _ => 0,
    };
    sig_append_or_forward(ctx, this, &[b])?;
    Ok(None)
}

fn sig_update_bytes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    // `Signature.update` DECLARES `SignatureException`, and HotSpot raises it
    // here: measured `SignatureException: object not initialized for signature
    // or verification` for every `update` overload on a fresh object. This VM
    // appended the bytes and returned — it raised NOTHING — so an
    // update-before-init went unnoticed until `sign()` produced a signature
    // over data the caller never meant to sign.
    require_initialized_for_update(ctx, this)?;
    if let Some(Value::Object(Some(arr))) = args.get(1) {
        let buf = read_byte_array_full(ctx, *arr);
        sig_append_or_forward(ctx, this, &buf)?;
    }
    Ok(None)
}

fn sig_update_bytes_off_len(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    // `Signature.update` DECLARES `SignatureException`, and HotSpot raises it
    // here: measured `SignatureException: object not initialized for signature
    // or verification` for every `update` overload on a fresh object. This VM
    // appended the bytes and returned — it raised NOTHING — so an
    // update-before-init went unnoticed until `sign()` produced a signature
    // over data the caller never meant to sign.
    require_initialized_for_update(ctx, this)?;
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
        sig_append_or_forward(ctx, this, &buf)?;
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
/// `SignatureException` instead of silently degrading to a slot-read
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
    Err(refuse_uninitialized(
        ctx,
        "object not initialized for signature or verification          (Signature state missing post-GC or never initialized)",
    ))
}

/// `update()` is legal only after `initSign`/`initVerify`.
///
/// HotSpot's `Signature.update` is `if (state == UNINITIALIZED) throw new
/// SignatureException("object not initialized for signature or verification")`,
/// and both of the states it admits are admitted here for the same reason: the
/// bytes are accumulated identically for signing and for verifying, so which
/// one it is does not matter until `sign()`/`verify()`.
fn require_initialized_for_update(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    let state = require_sig_state(ctx, this)?;
    if state == STATE_SIGN || state == STATE_VERIFY {
        return Ok(());
    }
    Err(refuse_uninitialized(
        ctx,
        "object not initialized for signature or verification",
    ))
}

fn require_sig_algo(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Result<i32, cratonvm_types::error::MethodCallFailed> {
    if let Some(a) = get_sig_algo(ctx, this) {
        return Ok(a);
    }
    Err(refuse_uninitialized(
        ctx,
        "object not initialized for signature or verification          (Signature state missing post-GC or never initialized)",
    ))
}

fn sig_sign(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let state = require_sig_state(ctx, this)?;
    if state != STATE_SIGN {
        return Err(refuse_uninitialized(
            ctx,
            "object not initialized for signing",
        ));
    }
    if let Some(spi) = sig_user_spi_obj(ctx, this) {
        return drive_user_spi(ctx, this, spi, None);
    }
    let alg = require_sig_algo(ctx, this)?;
    // EC: drive the real SunEC ECDSASignature SPI (real key, real DER output).
    let sig_name = sig_algorithm_name(ctx, this);
    if let Some(spi_class) = ecdsa_real_spi_class(ctx, alg, &sig_name) {
        return drive_real_signature_spi(ctx, this, spi_class, None);
    }
    if let Some(spi_class) = eddsa_real_spi_class(alg) {
        return drive_real_signature_spi(ctx, this, spi_class, None);
    }
    // DSA: drive the real sun.security.provider.DSA$* SPI (no native DSA crypto).
    if let Some(spi_class) = dsa_real_spi_class(ctx, this, alg) {
        return drive_real_signature_spi(ctx, this, spi_class, None);
    }
    // ML-DSA: drive the real SUN ML_DSA_Impls$SIG* SPI (real lattice signature).
    if is_mldsa(alg) {
        return drive_real_mldsa(ctx, this, alg, None);
    }
    let key_id = key_id_of(ctx, this);
    // C18: surface a missing payload as the checked SignatureException rather than
    // silently signing/verifying `b""` (the pre-fix raw-pointer keying
    // could orphan the buffer after GC compaction and the empty-fallback
    // produced an apparent success).
    let data = take_data(ctx, this)?;

    // P0: `.unwrap_or_default()` here produced an EMPTY byte[] and returned it
    // as the signature. A caller storing that into a JWS/JAR/token sees a
    // successful `sign()` and ships an unsigned artefact.
    let sig_bytes = match sign_dispatch_with(alg, key_id, &data, get_sig_pss(ctx, this)) {
        Some(bytes) => bytes,
        None => {
            // No native implementation for this name. Before refusing, offer it
            // to any installed third-party provider that DOES implement it —
            // the anonymous `getInstance` promised "whatever the chain gives
            // me", and this engine's table is not the whole chain.
            let key = sig_key_object(ctx, this);
            if let Some(result) = try_chain_signature_spi(ctx, this, key, &data, None) {
                return result;
            }
            return Err(refuse_unanswerable(ctx, alg, "sign()"));
        }
    };
    let arr = alloc_byte_array(ctx, &sig_bytes);
    Ok(Some(Value::Object(Some(arr))))
}

fn sig_sign_into(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let state = require_sig_state(ctx, this)?;
    if state != STATE_SIGN {
        return Err(refuse_uninitialized(
            ctx,
            "object not initialized for signing",
        ));
    }
    if let Some(spi) = sig_user_spi_obj(ctx, this) {
        // Produce the bytes through the application SPI, then apply the same
        // "a signature that does not fit is not a shorter signature" rule.
        let produced = drive_user_spi(ctx, this, spi, None)?;
        let sig_bytes = match produced {
            Some(Value::Object(Some(arr))) => read_byte_array_full(ctx, arr),
            _ => Vec::new(),
        };
        let off = match args.get(2) {
            Some(Value::Int(n)) => *n as usize,
            _ => 0,
        };
        let max_len = match args.get(3) {
            Some(Value::Int(n)) => *n as usize,
            _ => sig_bytes.len(),
        };
        if max_len < sig_bytes.len() {
            return Err(crate::phases_early::throw_jca_exc(
                ctx,
                SIGNATURE_EXCEPTION,
                "partial signatures not returned",
            ));
        }
        if let Some(Value::Object(Some(out))) = args.get(1) {
            for (i, &b) in sig_bytes.iter().enumerate() {
                ctx.set_array_element(*out, off + i, Value::Int(b as i8 as i32));
            }
        }
        return Ok(Some(Value::Int(sig_bytes.len() as i32)));
    }
    let alg = require_sig_algo(ctx, this)?;
    let key_id = key_id_of(ctx, this);
    // C18: surface a missing payload as the checked SignatureException rather than
    // silently signing/verifying `b""` (the pre-fix raw-pointer keying
    // could orphan the buffer after GC compaction and the empty-fallback
    // produced an apparent success).
    let data = take_data(ctx, this)?;
    // P0, as in `sig_sign`: an empty signature written into the caller's
    // buffer with a `written` count of 0 reads as "signed, zero-length" rather
    // than "not signed".
    let sig_bytes = match sign_dispatch_with(alg, key_id, &data, get_sig_pss(ctx, this)) {
        Some(bytes) => bytes,
        None => return Err(refuse_unanswerable(ctx, alg, "sign(byte[],int,int)")),
    };

    let off = match args.get(2) {
        Some(Value::Int(n)) => *n as usize,
        _ => 0,
    };
    let max_len = match args.get(3) {
        Some(Value::Int(n)) => *n as usize,
        _ => sig_bytes.len(),
    };
    // A signature that does not FIT is not a shorter signature. HotSpot:
    // `SignatureException: partial signatures not returned` (measured, a
    // 4-byte window for an RSA-2048 signature). This used to write
    // `min(len, max_len)` bytes and RETURN THAT COUNT, so a caller with a
    // too-small buffer was handed the first four bytes of a 256-byte signature
    // and told the operation succeeded — a wrong answer with no exception at
    // all, which is worse than the wrong exception class this record is about.
    if max_len < sig_bytes.len() {
        return Err(crate::phases_early::throw_jca_exc(
            ctx,
            SIGNATURE_EXCEPTION,
            "partial signatures not returned",
        ));
    }
    let written = sig_bytes.len();
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
        return Err(refuse_uninitialized(
            ctx,
            "object not initialized for verification",
        ));
    }
    if let Some(spi) = sig_user_spi_obj(ctx, this) {
        let provided = match args.get(1) {
            Some(Value::Object(Some(arr))) => read_byte_array_full(ctx, *arr),
            _ => Vec::new(),
        };
        return drive_user_spi(ctx, this, spi, Some(provided));
    }
    let alg = require_sig_algo(ctx, this)?;
    // EC: drive the real SunEC ECDSASignature SPI (real key, real DER verify).
    let sig_name = sig_algorithm_name(ctx, this);
    if let Some(spi_class) = ecdsa_real_spi_class(ctx, alg, &sig_name) {
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
    if let Some(spi_class) = dsa_real_spi_class(ctx, this, alg) {
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
    // C18: surface a missing payload as the checked SignatureException rather than
    // silently signing/verifying `b""` (the pre-fix raw-pointer keying
    // could orphan the buffer after GC compaction and the empty-fallback
    // produced an apparent success).
    let data = take_data(ctx, this)?;
    let provided = match args.get(1) {
        Some(Value::Object(Some(arr))) => read_byte_array_full(ctx, *arr),
        _ => Vec::new(),
    };

    // P0. `Some(false)` is a PRESERVED NEGATIVE — the signature really was
    // checked against the key and really did not match; that is the security
    // decision the caller asked for and it stays a `false`. `None` is
    // "never checked" and now raises. `.unwrap_or(false)` conflated the two,
    // so an unusable key was reported as a bad signature.
    let ok = match verify_dispatch_with(alg, key_id, &data, &provided, get_sig_pss(ctx, this)) {
        Some(answer) => answer,
        None => {
            // See the `sign()` sibling — ask the chain before refusing.
            let key = sig_key_object(ctx, this);
            if let Some(result) = try_chain_signature_spi(ctx, this, key, &data, Some(&provided)) {
                return result;
            }
            return Err(refuse_unanswerable(ctx, alg, "verify()"));
        }
    };
    Ok(Some(Value::Int(if ok { 1 } else { 0 })))
}

fn sig_verify_off_len(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let state = require_sig_state(ctx, this)?;
    if state != STATE_VERIFY {
        return Err(refuse_uninitialized(
            ctx,
            "object not initialized for verification",
        ));
    }
    let alg = require_sig_algo(ctx, this)?;
    let key_id = key_id_of(ctx, this);
    // C18: surface a missing payload as the checked SignatureException rather than
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
    // P0 — same split as `sig_verify`: `Some(false)` is the preserved genuine
    // negative, `None` is "the question was never asked" and raises.
    let ok = match verify_dispatch_with(alg, key_id, &data, &provided, get_sig_pss(ctx, this)) {
        Some(answer) => answer,
        None => return Err(refuse_unanswerable(ctx, alg, "verify(byte[],int,int)")),
    };
    Ok(Some(Value::Int(if ok { 1 } else { 0 })))
}

fn sig_get_algorithm(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    // The name the CALLER asked for, which `sig_get_instance` wrote into the
    // real `algorithm` field. `algo_name(idx)` is this engine's own canonical
    // spelling and answers `"Unknown"` for every name outside its table, which
    // is not a name any caller can use: BouncyCastle's
    // `X509SignatureUtil.setSignatureParameters` feeds `signature.getAlgorithm()`
    // straight back to `AlgorithmParameters.getInstance(..)` and got
    // `NoSuchAlgorithmException: no AlgorithmParameters Unknown implementation
    // for provider BC` while verifying an RSASSA-PSS certificate. It also
    // rewrote `SHA256withRSA/PSS` to `SHA256withRSAandMGF1`, where HotSpot
    // echoes the request verbatim.
    if let Value::Object(Some(name)) = ctx.get_field_by_name(this, "algorithm") {
        if let Some(text) = ctx.read_string(name) {
            if !text.is_empty() {
                let s = ctx.create_string(&text);
                return Ok(Some(Value::Object(Some(s))));
            }
        }
    }
    // `getAlgorithm()` is a benign accessor — keep the slot-read fallback
    // so callers that only invoke it after a state-eroding bug elsewhere
    // still get *some* answer instead of an exception cascade.  The loud
    // error path is reserved for the cryptographic operations above.
    let base = synthetic_base_offset(ctx, "java/security/Signature");
    let idx =
        get_sig_algo(ctx, this).unwrap_or_else(|| match sig_slot_get(ctx, this, SIG_OFF_ALGO) {
            Value::Int(i) => i,
            _ => -1,
        });
    let s = ctx.create_string(algo_name(idx));
    Ok(Some(Value::Object(Some(s))))
}

/// Map a JCA digest NAME (`"SHA-256"`, `"SHA256"`, …) onto a `PssHash`.
fn pss_hash_for_jca_name(name: &str) -> Option<crypto_impl::PssHash> {
    let n: String = name
        .chars()
        .filter(|c| !c.is_ascii_whitespace() && *c != '-')
        .collect::<String>()
        .to_ascii_uppercase();
    match n.as_str() {
        "SHA1" | "SHA" => Some(crypto_impl::PssHash::Sha1),
        "SHA256" => Some(crypto_impl::PssHash::Sha256),
        "SHA384" => Some(crypto_impl::PssHash::Sha384),
        "SHA512" => Some(crypto_impl::PssHash::Sha512),
        _ => None,
    }
}

/// `Signature.setParameter(AlgorithmParameterSpec)` — record a
/// `PSSParameterSpec` so the PSS sign/verify actually uses it.
///
/// Everything that is not a `PSSParameterSpec` stays a no-op, exactly as
/// before: the other algorithms here take no parameters, and inventing a
/// refusal for a spec we simply ignore would break callers that pass one
/// harmlessly.
fn sig_set_parameter(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Ok(this) = this_arg(args) else {
        return Ok(None);
    };
    if let Some(spi) = sig_user_spi_obj(ctx, this) {
        // Only the `AlgorithmParameterSpec` overload has an SPI counterpart
        // worth forwarding; the deprecated `(String, Object)` one is a no-op
        // on the JDK providers too.
        if let Some(Value::Object(Some(spec))) = args.get(1) {
            let is_string = ctx
                .class_name_of_id(ctx.class_id_of_object(*spec))
                .is_some_and(|n| n == "java/lang/String");
            if !is_string {
                let spec = Value::Object(Some(*spec));
                user_spi_call(
                    ctx,
                    spi,
                    "engineSetParameter",
                    "(Ljava/security/spec/AlgorithmParameterSpec;)V",
                    &[spec],
                )?;
            }
        }
        return Ok(None);
    }
    // The `(String, Object)` overload never carries a PSSParameterSpec.
    let Some(Value::Object(Some(spec))) = args.get(1) else {
        return Ok(None);
    };
    let spec = *spec;
    if !ctx
        .class_name_of_id(ctx.class_id_of_object(spec))
        .is_some_and(|n| n == "java/security/spec/PSSParameterSpec")
    {
        return Ok(None);
    }
    let pin = ctx.pin_native_root(this);
    let read_name = |ctx: &mut dyn NativeContext, recv: ObjectRef, m: &str| -> Option<String> {
        match ctx.invoke_virtual(recv, m, "()Ljava/lang/String;", &[]) {
            Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
            _ => None,
        }
    };
    let hash = read_name(ctx, spec, "getDigestAlgorithm").and_then(|n| pss_hash_for_jca_name(&n));
    // `getMGFParameters()` is the MGF1ParameterSpec; its digest is the MGF
    // digest, which RFC 8017 allows to differ from the message digest.
    let mgf_hash = match ctx.invoke_virtual(
        spec,
        "getMGFParameters",
        "()Ljava/security/spec/AlgorithmParameterSpec;",
        &[],
    ) {
        Ok(Some(Value::Object(Some(mgf)))) => {
            read_name(ctx, mgf, "getDigestAlgorithm").and_then(|n| pss_hash_for_jca_name(&n))
        }
        _ => None,
    };
    let salt_len = ctx
        .invoke_virtual(spec, "getSaltLength", "()I", &[])
        .ok()
        .flatten()
        .and_then(|v| v.as_int())
        .filter(|v| *v >= 0)
        .map(|v| v as usize);
    let this = ctx.read_native_pin(pin, this);
    ctx.unpin_native_roots(pin);
    // A spec we cannot fully read is not applied at all — a HALF-applied spec
    // would sign under parameters no caller asked for, which is worse than the
    // algorithm-name default.
    if let (Some(hash), Some(salt_len)) = (hash, salt_len) {
        let params = PssParams {
            hash,
            mgf_hash: mgf_hash.unwrap_or(hash),
            salt_len,
        };
        let key = sig_key(ctx, this);
        sig_pss_table().lock().insert(key, params);
    }
    Ok(None)
}

/// `Signature.getProvider()`.
///
/// Returned a bare `null`, which a real `Signature.getProvider()` never does: a
/// `Signature` you successfully obtained always has one. Callers write
/// `sig.getProvider().getName()` — JSSE and the JDK's own JAR verification do —
/// so `null` is an immediate `NullPointerException: … because the return value
/// of "java.security.Signature.getProvider()" is null`.
///
/// Answer with the provider HotSpot 25 resolves each family to, keyed off this
/// engine's own algorithm index so the name cannot drift from what
/// `getAlgorithm()` reports. An algorithm this module does not recognise keeps
/// returning `null` rather than being assigned a fabricated provider.
fn sig_get_provider_null(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Ok(this) = this_arg(args) else {
        return Ok(Some(Value::Object(None)));
    };
    // A `Signature` from a third-party provider reports THAT provider, not
    // the one this VM would have used for the algorithm name.
    if let Some((provider, _)) = get_sig_user_spi(ctx, this) {
        if !provider.is_empty() {
            let p = crate::jca::make_named_provider(ctx, &provider)?;
            return Ok(Some(Value::Object(Some(p))));
        }
    }
    let base = synthetic_base_offset(ctx, "java/security/Signature");
    let idx =
        get_sig_algo(ctx, this).unwrap_or_else(|| match sig_slot_get(ctx, this, SIG_OFF_ALGO) {
            Value::Int(i) => i,
            _ => -1,
        });
    // Measured on jdk-25: *withECDSA and the Edwards curves → SunEC,
    // *withDSA and ML-DSA → SUN, anything RSA (incl. PSS) → SunRsaSign.
    let name = match algo_name(idx) {
        // BEFORE the `contains("RSA")` arm: `NONEwithRSA` is the one RSA name
        // SunJCE owns, and the generic arm below would answer SunRsaSign.
        "NONEwithRSA" => "SunJCE",
        // Also before the `contains("RSA")` arm: the MD5+SHA1 pair has no OID,
        // so SunRsaSign does not offer it — SunJSSE does (measured on jdk-25).
        "MD5andSHA1withRSA" => "SunJSSE",
        a if a.ends_with("ECDSA") => "SunEC",
        "Ed25519" | "Ed448" | "EdDSA" => "SunEC",
        a if a.ends_with("DSA") || a.starts_with("ML-DSA") => "SUN",
        a if a.contains("RSA") => "SunRsaSign",
        // ...and for a name this engine has no INDEX for, ask the registry
        // instead of answering null.
        //
        // `signature_name_is_offered` is a disjunction: a name this module
        // indexes, OR a name some provider advertises. The second arm is
        // why `Signature.getInstance("MD2withRSA")` succeeds while
        // `algo_name(idx)` has nothing to say about it — and the match
        // above is keyed on exactly that index, so every offered-but-not-
        // indexed name fell through to `null`.
        //
        // MEASURED against HotSpot 25 over the 583 names the five JDK
        // providers reach by alias or canonical: 71 of them answer a
        // provider there and answered `null` here, including every
        // `*withECDSA` but `SHA256withECDSA` and every OID spelling of a
        // signature algorithm. `sig.getProvider().getName()` is what
        // bc-java writes, so each was a NullPointerException one call on.
        //
        // The registry is the right source and not a wider guess: it is the
        // same table `getInstance` consulted to accept the name, it
        // resolves aliases, and it walks the chain in order, so a caller
        // provider that owns the name outranks the JDK one. A name nothing
        // advertises still answers `null` rather than a fabricated owner.
        _ => {
            let requested = match ctx.get_field_by_name(this, "algorithm") {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            let owner = match requested.is_empty() {
                true => None,
                false => crate::jca::provider_chain::find_service_provider("Signature", &requested),
            };
            let Some(owner) = owner else {
                return Ok(Some(Value::Object(None)));
            };
            let p = crate::jca::make_named_provider(ctx, &owner)?;
            return Ok(Some(Value::Object(Some(p))));
        }
    };
    let p = crate::jca::make_named_provider(ctx, name)?;
    Ok(Some(Value::Object(Some(p))))
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
    // NOT `sig_init_verify`: that body reads `args[1]` as the verification KEY,
    // and this overload's `args[1]` is a CERTIFICATE. See
    // `sig_init_verify_cert`.
    r.register(
        cls,
        "initVerify",
        "(Ljava/security/cert/Certificate;)V",
        sig_init_verify_cert,
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

    // `getParameters()` / `getParameter(String)`. Neither was registered, so
    // both fell through to the real `java.security.Signature` bytecode, which
    // calls `this.engineGetParameters()` — and `this` is a synthetic
    // `java.security.Signature`, whose inherited `SignatureSpi.engineGetParameters`
    // is `throw new UnsupportedOperationException()`. Every bc-java
    // `SignatureSetParameterTest` case that reads back the context it had just
    // set died there. The application SPI, when there is one, is the only thing
    // that knows the answer; without one there are no parameters to report and
    // `null` is the value the method is declared to return.
    r.register(
        cls,
        "getParameters",
        "()Ljava/security/AlgorithmParameters;",
        |ctx, args| {
            let this = this_arg(args)?;
            if let Some(spi) = sig_user_spi_obj(ctx, this) {
                return user_spi_call(
                    ctx,
                    spi,
                    "engineGetParameters",
                    "()Ljava/security/AlgorithmParameters;",
                    &[],
                );
            }
            Ok(Some(Value::Object(None)))
        },
    );
    r.register(
        cls,
        "getParameter",
        "(Ljava/lang/String;)Ljava/lang/Object;",
        |ctx, args| {
            let this = this_arg(args)?;
            if let Some(spi) = sig_user_spi_obj(ctx, this) {
                let name = args.get(1).copied().unwrap_or(Value::Object(None));
                return user_spi_call(
                    ctx,
                    spi,
                    "engineGetParameter",
                    "(Ljava/lang/String;)Ljava/lang/Object;",
                    &[name],
                );
            }
            Ok(Some(Value::Object(None)))
        },
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
        assert!(get_instance_offers("NONEwithRSA"));
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

    /// Parameter-set → concrete SUN `ML_DSA_Impls$SIG{2,3,5}` SPI mapping
    /// (matching `key_factory::pqc_spi_classes`' 2/3/5 NIST-category suffixes),
    /// and a non-ML-DSA name rejects.
    #[test]
    /// The seed list and the SPI map are ONE set, in both directions.
    ///
    /// They are separate declarations — one is what `Security.getAlgorithms`
    /// reports, the other is what `sign()` drives — and a name in the first
    /// without an arm in the second is precisely the defect
    /// `W7-63-jca-advertise-vs-serve.md` is named for: a provider advertising
    /// an algorithm it will not serve. `seed_direct_native_engine_services`
    /// would panic on that (`.expect`), which is a loud failure at VM start
    /// rather than a quiet one at `getInstance` — but only if a VM is started,
    /// so it is asserted here too.
    ///
    /// The reverse direction is the one that would rot silently: an SPI arm
    /// nothing advertises serves a name `Security.getAlgorithms("Signature")`
    /// says does not exist.
    #[test]
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
}
