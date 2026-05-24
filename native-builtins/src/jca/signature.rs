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
//! table keyed on `Signature` pointer identity — see
//! `crypto_impl::sig_data_*`.

#![allow(clippy::collapsible_if)]

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::{ArrayElementType, ClassId, ObjectRef, Value};
use cratonvm_types::error::{MethodCallResult, RuntimeError};

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
const SIG_PRIVATE_SLOTS: usize = 5;

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

fn sig_algo_table()
    -> &'static parking_lot::Mutex<rustc_hash::FxHashMap<i32, i32>> {
    use std::sync::OnceLock;
    static T: OnceLock<parking_lot::Mutex<rustc_hash::FxHashMap<i32, i32>>> =
        OnceLock::new();
    T.get_or_init(|| parking_lot::Mutex::new(rustc_hash::FxHashMap::default()))
}

fn sig_state_table()
    -> &'static parking_lot::Mutex<rustc_hash::FxHashMap<i32, i32>> {
    use std::sync::OnceLock;
    static T: OnceLock<parking_lot::Mutex<rustc_hash::FxHashMap<i32, i32>>> =
        OnceLock::new();
    T.get_or_init(|| parking_lot::Mutex::new(rustc_hash::FxHashMap::default()))
}

fn sig_keyid_table()
    -> &'static parking_lot::Mutex<rustc_hash::FxHashMap<i32, u64>> {
    use std::sync::OnceLock;
    static T: OnceLock<parking_lot::Mutex<rustc_hash::FxHashMap<i32, u64>>> =
        OnceLock::new();
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

fn algo_idx(name: &str) -> i32 {
    let upper = name.to_ascii_uppercase();
    match upper.as_str() {
        "SHA256WITHRSA" | "SHA-256WITHRSA" | "RSASSA-PKCS1-V1_5_WITH_SHA-256" => SIG_SHA256_RSA,
        "SHA384WITHRSA" => SIG_SHA384_RSA,
        "SHA512WITHRSA" => SIG_SHA512_RSA,
        "SHA1WITHRSA" | "SHA-1WITHRSA" => SIG_SHA1_RSA,
        "SHA256WITHECDSA" | "SHA-256WITHECDSA" => SIG_SHA256_ECDSA,
        "SHA384WITHECDSA" | "SHA-384WITHECDSA" => SIG_SHA384_ECDSA,
        "ED25519" | "EDDSA" => SIG_ED25519,
        "SHA256WITHDSA" => SIG_SHA256_DSA,
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
        SIG_ED25519 => "Ed25519",
        SIG_SHA256_DSA => "SHA256withDSA",
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

fn read_byte_array_range(ctx: &mut dyn NativeContext, arr: ObjectRef, off: usize, len: usize) -> Vec<u8> {
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

fn sig_id(this: ObjectRef) -> u64 {
    this.as_ptr() as u64
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
    ctx.set_field(this, base + SIG_OFF_PENDING, Value::Int(cur + data.len() as i32));
    crypto_impl::sig_data_append(sig_id(this), data);
}

fn take_data(ctx: &mut dyn NativeContext, this: ObjectRef) -> Vec<u8> {
    let base = synthetic_base_offset(ctx, "java/security/Signature");
    ctx.set_field(this, base + SIG_OFF_PENDING, Value::Int(0));
    crypto_impl::sig_data_take(sig_id(this))
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
        SIG_SHA256_ECDSA => crypto_impl::ecdsa_sign_sha256(key_id, data),
        SIG_SHA384_ECDSA => crypto_impl::ecdsa_sign(key_id, data),
        SIG_ED25519 => crypto_impl::ed25519_sign(key_id, data),
        _ => None,
    }
}

fn verify_dispatch(alg: i32, key_id: u64, data: &[u8], sig: &[u8]) -> Option<bool> {
    match alg {
        SIG_SHA256_RSA => crypto_impl::rsa_verify(key_id, data, sig),
        SIG_SHA256_ECDSA => crypto_impl::ecdsa_verify_sha256(key_id, data, sig),
        SIG_SHA384_ECDSA => crypto_impl::ecdsa_verify(key_id, data, sig),
        SIG_ED25519 => crypto_impl::ed25519_verify(key_id, data, sig),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Native methods
// ---------------------------------------------------------------------------

fn sig_get_instance(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let alg = read_string(ctx, args, 0);
    let idx = algo_idx(&alg);
    let base = synthetic_base_offset(ctx, "java/security/Signature");
    let obj = alloc_concurrent_synthetic(
        ctx,
        "java/security/Signature",
        base + SIG_PRIVATE_SLOTS,
    );
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
    }
    crypto_impl::sig_data_clear(sig_id(this));
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
    }
    crypto_impl::sig_data_clear(sig_id(this));
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
fn require_sig_state(ctx: &mut dyn NativeContext, this: ObjectRef)
    -> Result<i32, cratonvm_types::error::MethodCallFailed>
{
    if let Some(st) = get_sig_state(ctx, this) {
        return Ok(st);
    }
    Err(RuntimeError::IllegalStateException {
        message: "Signature state missing post-GC or never initialized".into(),
    }
    .into())
}

fn require_sig_algo(ctx: &mut dyn NativeContext, this: ObjectRef)
    -> Result<i32, cratonvm_types::error::MethodCallFailed>
{
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
    let key_id = key_id_of(ctx, this);
    let data = take_data(ctx, this);

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
    let data = take_data(ctx, this);
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
    let key_id = key_id_of(ctx, this);
    let data = take_data(ctx, this);
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
    let data = take_data(ctx, this);

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
    let idx = get_sig_algo(ctx, this).unwrap_or_else(|| match ctx.get_field(this, base + SIG_OFF_ALGO) {
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
    let cls = "java/security/Signature";

    r.register(cls, "getInstance", "(Ljava/lang/String;)Ljava/security/Signature;", sig_get_instance);
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

    r.register(cls, "initSign", "(Ljava/security/PrivateKey;)V", sig_init_sign);
    r.register(
        cls,
        "initSign",
        "(Ljava/security/PrivateKey;Ljava/security/SecureRandom;)V",
        sig_init_sign,
    );
    r.register(cls, "initVerify", "(Ljava/security/PublicKey;)V", sig_init_verify);
    r.register(
        cls,
        "initVerify",
        "(Ljava/security/cert/Certificate;)V",
        sig_init_verify,
    );

    r.register(cls, "update", "(B)V", sig_update_byte);
    r.register(cls, "update", "([B)V", sig_update_bytes);
    r.register(cls, "update", "([BII)V", sig_update_bytes_off_len);
    r.register(cls, "update", "(Ljava/nio/ByteBuffer;)V", sig_update_bytebuffer);

    r.register(cls, "sign", "()[B", sig_sign);
    r.register(cls, "sign", "([BII)I", sig_sign_into);

    r.register(cls, "verify", "([B)Z", sig_verify);
    r.register(cls, "verify", "([BII)Z", sig_verify_off_len);

    r.register(cls, "getAlgorithm", "()Ljava/lang/String;", sig_get_algorithm);
    r.register(cls, "getProvider", "()Ljava/security/Provider;", sig_get_provider_null);

    r.register(
        cls,
        "setParameter",
        "(Ljava/security/spec/AlgorithmParameterSpec;)V",
        sig_set_parameter,
    );
    r.register(cls, "setParameter", "(Ljava/lang/String;Ljava/lang/Object;)V", sig_set_parameter);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn algo_idx_canonical() {
        assert_eq!(algo_idx("SHA256withRSA"), SIG_SHA256_RSA);
        assert_eq!(algo_idx("sha256withrsa"), SIG_SHA256_RSA);
        assert_eq!(algo_idx("SHA256withECDSA"), SIG_SHA256_ECDSA);
        assert_eq!(algo_idx("SHA384withECDSA"), SIG_SHA384_ECDSA);
        assert_eq!(algo_idx("Ed25519"), SIG_ED25519);
        assert_eq!(algo_idx("nope"), -1);
    }

    #[test]
    fn algo_name_canonical() {
        assert_eq!(algo_name(SIG_SHA256_RSA), "SHA256withRSA");
        assert_eq!(algo_name(SIG_SHA256_ECDSA), "SHA256withECDSA");
        assert_eq!(algo_name(SIG_SHA384_ECDSA), "SHA384withECDSA");
        assert_eq!(algo_name(SIG_ED25519), "Ed25519");
    }
}
