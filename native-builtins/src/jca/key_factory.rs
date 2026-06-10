// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP6.4 + WP6.6 — `java.security.KeyPairGenerator`, `KeyPair`, `KeyFactory`,
//! `PublicKey` / `PrivateKey` accessors. Real-JDK mode.
//!
//! ## Why duplicate `crypto.rs::register_key_pair_generator`?
//!
//! `crypto.rs` is gated behind `legacy-synthetic-crypto` AND is only wired
//! into `register_synthetic_overrides` — i.e. it never runs in default
//! real-JDK mode.  Real-JDK `KeyPairGenerator.getInstance` therefore falls
//! through to the JDK 25 bytecode which calls
//! `sun.security.jca.GetInstance.getServices(...)` and NPEs because we
//! don't materialize a real `Provider.services` map.
//!
//! Registering native overrides for the public surface (`getInstance`,
//! `initialize`, `generateKeyPair`, `KeyPair.getPublic` / `getPrivate`,
//! `Key.getAlgorithm` / `getEncoded` / `getFormat`, plus `KeyFactory`)
//! short-circuits the bytecode path entirely.  We back the real RSA + EC
//! key generation with the `crypto_impl` software primitives that already
//! power the synthetic-mode crypto path.
//!
//! ## Object layout
//!
//! Both `KeyPairGenerator` and `KeyFactory` are 3-field synthetics:
//!
//! | Slot | Field          |
//! |------|----------------|
//! |  0   | `algo_idx` Int |
//! |  1   | `key_size` Int |
//! |  2   | `state`    Int |  (0=created, 1=initialized)
//!
//! `PublicKey` / `PrivateKey` are 4-field synthetics that match the
//! existing `crypto.rs` shape, so the `Signature` natives in
//! `jca::signature` can read `key_id` from slot 3:
//!
//! | Slot | Field          |
//! |------|----------------|
//! |  0   | `algo_idx`  Int|
//! |  1   | `size_bits` Int|
//! |  2   | `enc_len`   Int|
//! |  3   | `key_id`    Long  (0 = no real key, otherwise crypto_impl handle) |
//!
//! ## Algorithm indexing
//!
//! Aligned with `crypto.rs::KPG_ALGORITHMS`:
//! `0=ML-KEM-512, 1=ML-KEM-768, 2=ML-KEM-1024, 3=ML-DSA-44, 4=ML-DSA-65,
//!  5=ML-DSA-87, 6=RSA, 7=EC, 8=Ed25519, 9=X25519`.
//!
//! Wave 6 in scope: 6 (RSA) and 7 (EC).  The other indices are still
//! recognised so the synthetic Ed25519 / ML-* probes don't regress, but
//! their `generateKeyPair` only touches the synthetic key shape.

#![allow(clippy::collapsible_if)]

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::{ArrayElementType, ClassId, ObjectRef, Value};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};

use crate::alloc_concurrent_synthetic;
use crate::crypto_impl;

const ALGO_RSA: i32 = 6;
const ALGO_EC: i32 = 7;
const ALGO_ED25519: i32 = 8;

// ---------------------------------------------------------------------------
// Real-JDK class instance-field counts (number of slots used by the real
// layout before our synthetic state begins). All our private state slots
// are placed *after* the real layout so the VM's descriptor-aware
// `set_field`/`get_field` doesn't coerce our `Int(...)` writes to
// `Object(None)` on slots that the real JDK class declares as references.
//
// JDK 25:
//   * `java.security.KeyPairGenerator` extends `KeyPairGeneratorSpi`:
//        - KPGSpi: 0 instance fields
//        - KPG: `algorithm: String`, `provider: Provider` -> 2
//   * `java.security.KeyFactory`:
//        - 5 instance fields (algorithm, provider, spi, lock, serviceIterator)
//   * `java.security.KeyPair`:
//        - 2 fields (privateKey, publicKey) - both Object, order doesn't
//          matter as long as our accessors match what set_field writes via
//          the same indices, so we ignore the JDK ordering here and use
//          slot 0 = pub / slot 1 = priv as our internal convention.
//
// We resolve the actual field counts at runtime via `class_num_total_fields`
// instead of hard-coding so this stays robust against future JDK layout
// changes. The `kpg_base_offset` helper returns the first usable slot index
// past the real layout.
fn synthetic_base_offset(ctx: &mut dyn NativeContext, class_name: &str) -> usize {
    let cid = ctx
        .ensure_class_initialized(class_name)
        .unwrap_or(ClassId::new(0));
    ctx.class_num_total_fields(cid)
}

// ---------------------------------------------------------------------------
// SigProbe fix: process-wide side tables for KPG / KeyFactory algorithm +
// key size.  Real-JDK class layouts make raw-slot writes of `Value::Int`
// silently turn into `Value::Object(None)` (the inherited slot 0 is an
// object reference, not an int), so the raw-slot path is unreliable across
// the `getInstance` → `initialize` → `generateKeyPair` chain.  Side tables
// keyed on the receiver `ObjectRef` survive layout changes — same proven
// pattern as `message_digest::accumulators`.  The base-offset slot writes
// below are kept as a secondary store for synthetic-mode callers.
// ---------------------------------------------------------------------------

fn kpg_algo_table()
    -> &'static parking_lot::Mutex<rustc_hash::FxHashMap<ObjectRef, i32>> {
    use std::sync::OnceLock;
    static T: OnceLock<parking_lot::Mutex<rustc_hash::FxHashMap<ObjectRef, i32>>> =
        OnceLock::new();
    T.get_or_init(|| parking_lot::Mutex::new(rustc_hash::FxHashMap::default()))
}

fn kpg_keysize_table()
    -> &'static parking_lot::Mutex<rustc_hash::FxHashMap<ObjectRef, i32>> {
    use std::sync::OnceLock;
    static T: OnceLock<parking_lot::Mutex<rustc_hash::FxHashMap<ObjectRef, i32>>> =
        OnceLock::new();
    T.get_or_init(|| parking_lot::Mutex::new(rustc_hash::FxHashMap::default()))
}

fn set_kpg_algo(this: ObjectRef, idx: i32) {
    kpg_algo_table().lock().insert(this, idx);
}

fn get_kpg_algo(this: ObjectRef) -> Option<i32> {
    kpg_algo_table().lock().get(&this).copied()
}

fn set_kpg_keysize(this: ObjectRef, bits: i32) {
    kpg_keysize_table().lock().insert(this, bits);
}

fn get_kpg_keysize(this: ObjectRef) -> Option<i32> {
    kpg_keysize_table().lock().get(&this).copied()
}

// ---------------------------------------------------------------------------
// EC-scoped real-SunEC routing (crate::route_ec_to_real, default ON)
// ---------------------------------------------------------------------------
//
// The synthetic EC keygen returns a bare `java/security/PublicKey` *interface*
// object, so keycloak's `(java.security.interfaces.ECPublicKey) pub` cast throws
// ClassCastException ("Error obtaining ECParameterSpec for P-256 curve"). When
// `crate::route_ec_to_real()` is set (default), the EC family instead DRIVES the
// real, pure-Java JDK-25 SunEC SPIs (`sun.security.ec.*`) and returns concrete
// `ECPublicKeyImpl`/`ECPrivateKeyImpl` keys + a real `KeyPair`. This runs real,
// JIT-eligible SunEC bytecode; the `AlgorithmParameters.getInstance("EC")` it
// needs is served by the provider-machinery bridge wired (also under
// `route_ec_to_real`) in `provider_chain.rs`/`cipher.rs`. RSA/AES stay synthetic
// (kill-switch `CRATONVM_SYNTHETIC_EC=1` restores the legacy synthetic EC).

/// Class name of an object (`internal/slash/form`), or empty if unknown.
fn obj_class_name(ctx: &dyn NativeContext, obj: ObjectRef) -> String {
    ctx.class_name_of_id(ctx.class_id_of_object(obj))
        .unwrap_or_default()
}

/// True if `v` is one of our synthetic bare-interface key objects whose class is
/// exactly `class_name` (`java/security/PublicKey` or `java/security/PrivateKey`).
fn is_synthetic_key_obj(ctx: &dyn NativeContext, v: &Value, class_name: &str) -> bool {
    matches!(v, Value::Object(Some(o)) if obj_class_name(ctx, *o) == class_name)
}

/// Drive the real `sun.security.ec.ECKeyPairGenerator` SPI: `new` →
/// `initialize(spec|keysize, SecureRandom)` → `generateKeyPair()`. Returns a real
/// `java.security.KeyPair` (`privateKey`@0, `publicKey`@1) of concrete
/// `ECPrivateKeyImpl`/`ECPublicKeyImpl`. Honours the requested curve: when an
/// `AlgorithmParameterSpec` (e.g. `ECGenParameterSpec("secp384r1")`) was supplied
/// via `initialize`, it is forwarded so the real SunEC code resolves the curve
/// (P-256/384/521); otherwise the stored keysize (default 256 → P-256) is used.
fn drive_real_ec_keypair(ctx: &mut dyn NativeContext, this: ObjectRef) -> MethodCallResult {
    // Read the requested keysize + spec (curve) BEFORE any allocation.
    let base = synthetic_base_offset(ctx, "java/security/KeyPairGenerator");
    let keysize = get_kpg_keysize(this)
        .filter(|n| *n > 0)
        .or_else(|| match ctx.get_field(this, base + KPG_OFF_KEYSIZE) {
            Value::Int(n) if n > 0 => Some(n),
            _ => None,
        })
        .unwrap_or(256);
    let spec0 = match ctx.get_field(this, base + KPG_OFF_SPEC) {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    };
    // Pin the spec (if any) FIRST so it survives the SPI allocation below.
    let spec_pin = spec0.map(|s| ctx.pin_native_root(s));
    let spi = match ctx.new_object_initialized("sun/security/ec/ECKeyPairGenerator", "()V", &[]) {
        Ok(Some(Value::Object(Some(o)))) => o,
        other => {
            if let Some(p) = spec_pin {
                ctx.unpin_native_roots(p);
            }
            other?;
            return Err(RuntimeError::NotImplemented {
                feature: "sun.security.ec.ECKeyPairGenerator".into(),
            }
            .into());
        }
    };
    let spi_pin = ctx.pin_native_root(spi);
    let unpin_base = spec_pin.unwrap_or(spi_pin);
    let result = (|| {
        let rnd = match ctx.new_object_initialized("java/security/SecureRandom", "()V", &[])? {
            Some(Value::Object(Some(o))) => o,
            _ => {
                return Err(RuntimeError::NotImplemented {
                    feature: "java.security.SecureRandom".into(),
                }
                .into())
            }
        };
        let spi = ctx.read_native_pin(spi_pin, spi);
        match (spec0, spec_pin) {
            (Some(s0), Some(sp)) => {
                let s = ctx.read_native_pin(sp, s0);
                ctx.invoke_virtual(
                    spi,
                    "initialize",
                    "(Ljava/security/spec/AlgorithmParameterSpec;Ljava/security/SecureRandom;)V",
                    &[Value::Object(Some(s)), Value::Object(Some(rnd))],
                )?;
            }
            _ => {
                ctx.invoke_virtual(
                    spi,
                    "initialize",
                    "(ILjava/security/SecureRandom;)V",
                    &[Value::Int(keysize), Value::Object(Some(rnd))],
                )?;
            }
        }
        let spi = ctx.read_native_pin(spi_pin, spi);
        ctx.invoke_virtual(spi, "generateKeyPair", "()Ljava/security/KeyPair;", &[])
    })();
    ctx.unpin_native_roots(unpin_base);
    result
}

/// Drive the real `sun.security.ec.ECKeyFactory` SPI's `engineGeneratePublic` /
/// `engineGeneratePrivate` over the supplied `EC{Public,Private}KeySpec`, yielding
/// a concrete `sun.security.ec.EC{Public,Private}KeyImpl`.
fn drive_real_ec_keyfactory(
    ctx: &mut dyn NativeContext,
    spec: ObjectRef,
    engine: &'static str,
    ret_desc: &'static str,
) -> MethodCallResult {
    // Pin the KeySpec across the ECKeyFactory alloc.
    let pin = ctx.pin_native_root(spec);
    let result = (|| {
        let kf = match ctx.new_object_initialized("sun/security/ec/ECKeyFactory", "()V", &[])? {
            Some(Value::Object(Some(o))) => o,
            _ => {
                return Err(RuntimeError::NotImplemented {
                    feature: "sun.security.ec.ECKeyFactory".into(),
                }
                .into())
            }
        };
        let spec = ctx.read_native_pin(pin, spec);
        let desc = format!("(Ljava/security/spec/KeySpec;){ret_desc}");
        ctx.invoke_virtual(kf, engine, &desc, &[Value::Object(Some(spec))])
    })();
    ctx.unpin_native_roots(pin);
    result
}

// Synthetic-slot offsets relative to `synthetic_base_offset(...)`.
const KPG_OFF_ALGO: usize = 0;
const KPG_OFF_KEYSIZE: usize = 1;
const KPG_OFF_STATE: usize = 2;
// Slot 3 holds the `AlgorithmParameterSpec` passed to `initialize(spec[,random])`
// (e.g. ECGenParameterSpec) so the real EC keygen drive can honour the requested
// curve (P-256/384/521) instead of assuming P-256. GC-scanned synthetic slot.
const KPG_OFF_SPEC: usize = 3;
const KPG_PRIVATE_SLOTS: usize = 4;

const KF_OFF_ALGO: usize = 0;
const KF_PRIVATE_SLOTS: usize = 1;

// `PublicKey` / `PrivateKey` are interfaces in JDK 25 (no instance fields),
// so we can keep using the legacy fixed slot layout here.
const KEY_FIELD_ALGO: usize = 0;
const KEY_FIELD_BITS: usize = 1;
const KEY_FIELD_ENCLEN: usize = 2;
const KEY_FIELD_KEYID: usize = 3;
const KEY_FIELD_DER: usize = 4;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn read_string(ctx: &mut dyn NativeContext, args: &[Value], idx: usize) -> String {
    match args.get(idx) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    }
}

fn this_arg(args: &[Value]) -> Result<ObjectRef, cratonvm_types::error::MethodCallFailed> {
    match args.first() {
        Some(Value::Object(Some(o))) => Ok(*o),
        _ => Err(RuntimeError::NullPointerException {
            message: Some("null this".into()),
        }
        .into()),
    }
}

fn algo_idx(name: &str) -> i32 {
    let upper = name.to_ascii_uppercase();
    match upper.as_str() {
        "ML-KEM-512" => 0,
        "ML-KEM-768" => 1,
        "ML-KEM-1024" => 2,
        "ML-DSA-44" => 3,
        "ML-DSA-65" => 4,
        "ML-DSA-87" => 5,
        "RSA" => ALGO_RSA,
        "EC" | "ECDSA" => ALGO_EC,
        "ED25519" | "EDDSA" => ALGO_ED25519,
        "X25519" => 9,
        _ => -1,
    }
}

fn algo_name(idx: i32) -> &'static str {
    match idx {
        0 => "ML-KEM-512",
        1 => "ML-KEM-768",
        2 => "ML-KEM-1024",
        3 => "ML-DSA-44",
        4 => "ML-DSA-65",
        5 => "ML-DSA-87",
        ALGO_RSA => "RSA",
        ALGO_EC => "EC",
        ALGO_ED25519 => "Ed25519",
        9 => "X25519",
        _ => "Unknown",
    }
}

fn alloc_byte_array(ctx: &mut dyn NativeContext, bytes: &[u8]) -> ObjectRef {
    let arr = ctx.new_array(ArrayElementType::Byte, bytes.len());
    for (i, &b) in bytes.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
    }
    arr
}

fn read_byte_array(ctx: &mut dyn NativeContext, arr: ObjectRef) -> Vec<u8> {
    let len = ctx.array_length(arr);
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        if let Value::Int(b) = ctx.get_array_element(arr, i) {
            out.push(b as u8);
        }
    }
    out
}

/// Build a 4-field public key with stored DER + key_id.
fn alloc_public_key(
    ctx: &mut dyn NativeContext,
    algo: i32,
    bits: i32,
    der: &[u8],
    key_id: u64,
) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(ctx, "java/security/PublicKey", 5);
    ctx.set_field(obj, KEY_FIELD_ALGO, Value::Int(algo));
    ctx.set_field(obj, KEY_FIELD_BITS, Value::Int(bits));
    ctx.set_field(obj, KEY_FIELD_ENCLEN, Value::Int(der.len() as i32));
    ctx.set_field(obj, KEY_FIELD_KEYID, Value::Long(key_id as i64));
    let arr = alloc_byte_array(ctx, der);
    ctx.set_field(obj, KEY_FIELD_DER, Value::Object(Some(arr)));
    obj
}

fn alloc_private_key(
    ctx: &mut dyn NativeContext,
    algo: i32,
    bits: i32,
    der: &[u8],
    key_id: u64,
) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(ctx, "java/security/PrivateKey", 5);
    ctx.set_field(obj, KEY_FIELD_ALGO, Value::Int(algo));
    ctx.set_field(obj, KEY_FIELD_BITS, Value::Int(bits));
    ctx.set_field(obj, KEY_FIELD_ENCLEN, Value::Int(der.len() as i32));
    ctx.set_field(obj, KEY_FIELD_KEYID, Value::Long(key_id as i64));
    let arr = alloc_byte_array(ctx, der);
    ctx.set_field(obj, KEY_FIELD_DER, Value::Object(Some(arr)));
    obj
}

fn alloc_keypair(ctx: &mut dyn NativeContext, pubk: ObjectRef, privk: ObjectRef) -> ObjectRef {
    let kp = alloc_concurrent_synthetic(ctx, "java/security/KeyPair", 2);
    ctx.set_field(kp, 0, Value::Object(Some(pubk)));
    ctx.set_field(kp, 1, Value::Object(Some(privk)));
    kp
}

/// Construct and throw a real JCA exception of `class_name` (internal form,
/// e.g. `java/security/NoSuchAlgorithmException` or
/// `java/security/spec/InvalidKeySpecException`) carrying `msg`. These are all
/// `GeneralSecurityException` subclasses, so Java callers catch them exactly as
/// under HotSpot.
///
/// No-synthetic-stubs policy: the fallbacks these replace minted keys with
/// empty DER / `key_id == 0`, presenting failed keygen or key import as
/// success — any app that then signed/encrypted/verified with one got garbage
/// or a false success. We fail loudly instead. If the real exception class
/// can't be constructed we still raise a catchable `SecurityException` rather
/// than returning an empty/unusable key.
fn throw_jca(ctx: &mut dyn NativeContext, class_name: &str, msg: &str) -> MethodCallFailed {
    let detail = ctx.create_string(msg);
    if let Ok(Some(Value::Object(Some(exc)))) = ctx.new_object_initialized(
        class_name,
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(detail))],
    ) {
        return MethodCallFailed::ExceptionThrown(exc);
    }
    RuntimeError::SecurityException {
        message: msg.to_string(),
    }
    .into()
}

/// `KeyPairGenerator.generateKeyPair` contract for a recognised-but-unimplemented
/// algorithm.
fn throw_no_such_algorithm(ctx: &mut dyn NativeContext, msg: &str) -> MethodCallFailed {
    throw_jca(ctx, "java/security/NoSuchAlgorithmException", msg)
}

/// `KeyFactory.generatePublic` / `generatePrivate` contract when we cannot turn
/// the given `KeySpec` into a usable key (parse failure, or no implementation).
fn throw_invalid_key_spec(ctx: &mut dyn NativeContext, msg: &str) -> MethodCallFailed {
    throw_jca(ctx, "java/security/spec/InvalidKeySpecException", msg)
}

// ---------------------------------------------------------------------------
// KeyPairGenerator natives
// ---------------------------------------------------------------------------

fn kpg_get_instance(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let alg = read_string(ctx, args, 0);
    let idx = algo_idx(&alg);
    let base = synthetic_base_offset(ctx, "java/security/KeyPairGenerator");
    let kpg = alloc_concurrent_synthetic(
        ctx,
        "java/security/KeyPairGenerator",
        base + KPG_PRIVATE_SLOTS,
    );
    // SigProbe fix: the JDK 25 `KeyPairGenerator` class declares
    // `String algorithm` at the inherited `KeyPairGeneratorSpi` layout
    // boundary. The side table (keyed on the receiver ObjectRef) carries
    // the algorithm index reliably across the call chain, mirroring the
    // proven pattern in `message_digest::accumulators`.
    set_kpg_algo(kpg, idx);
    let default_bits = if idx == ALGO_RSA { 2048 } else if idx == ALGO_EC { 256 } else { 0 };
    set_kpg_keysize(kpg, default_bits);
    // Also write the algorithm string to the real-JDK named field so the
    // bytecode-side `getAlgorithm()` (if ever reached on this receiver)
    // sees the expected value.
    let algo_str = ctx.create_string(&alg);
    ctx.set_field_by_name(kpg, "algorithm", Value::Object(Some(algo_str)));
    // Base-offset slot path: appended past the real layout, so these
    // writes are a reliable secondary store for synthetic-mode callers.
    ctx.set_field(kpg, base + KPG_OFF_ALGO, Value::Int(idx));
    ctx.set_field(kpg, base + KPG_OFF_KEYSIZE, Value::Int(default_bits));
    ctx.set_field(kpg, base + KPG_OFF_STATE, Value::Int(0));
    Ok(Some(Value::Object(Some(kpg))))
}

fn kpg_initialize_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let base = synthetic_base_offset(ctx, "java/security/KeyPairGenerator");
    let bits = match args.get(1) {
        Some(Value::Int(n)) => *n,
        _ => 2048,
    };
    set_kpg_keysize(this, bits);
    ctx.set_field(this, base + KPG_OFF_KEYSIZE, Value::Int(bits));
    ctx.set_field(this, base + KPG_OFF_STATE, Value::Int(1));
    Ok(None)
}

fn kpg_initialize_int_random(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    kpg_initialize_int(ctx, args)
}

fn kpg_initialize_spec(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // ECGenParameterSpec / RSAKeyGenParameterSpec. RSA spec keysize is preserved
    // from the previous value / the default. For EC, stash the spec so the real
    // keygen drive (`drive_real_ec_keypair`) honours the requested curve.
    let this = this_arg(args)?;
    let base = synthetic_base_offset(ctx, "java/security/KeyPairGenerator");
    let cur = get_kpg_keysize(this).unwrap_or_else(|| match ctx.get_field(this, base + KPG_OFF_KEYSIZE) {
        Value::Int(n) => n,
        _ => 0,
    });
    let algo = get_kpg_algo(this).unwrap_or_else(|| match ctx.get_field(this, base + KPG_OFF_ALGO) {
        Value::Int(i) => i,
        _ => -1,
    });
    if let Some(Value::Object(Some(spec))) = args.get(1) {
        ctx.set_field(this, base + KPG_OFF_SPEC, Value::Object(Some(*spec)));
    }
    let bits = if algo == ALGO_EC { 256 } else if cur == 0 { 2048 } else { cur };
    set_kpg_keysize(this, bits);
    ctx.set_field(this, base + KPG_OFF_KEYSIZE, Value::Int(bits));
    ctx.set_field(this, base + KPG_OFF_STATE, Value::Int(1));
    Ok(None)
}

fn kpg_initialize_spec_random(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    kpg_initialize_spec(ctx, args)
}

fn kpg_generate_key_pair(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let base = synthetic_base_offset(ctx, "java/security/KeyPairGenerator");
    // SigProbe fix: prefer the side-table read (survives real-JDK class
    // layouts where slot 0 collides with an inherited Object field).
    let algo = get_kpg_algo(this).or_else(|| match ctx.get_field(this, base + KPG_OFF_ALGO) {
        Value::Int(i) => Some(i),
        _ => None,
    });
    let algo = match algo {
        Some(i) => i,
        None => return Err(RuntimeError::NotImplemented {
            feature: "KeyPairGenerator with no algorithm".into(),
        }
        .into()),
    };
    let bits = get_kpg_keysize(this)
        .filter(|n| *n > 0)
        .or_else(|| match ctx.get_field(this, base + KPG_OFF_KEYSIZE) {
            Value::Int(n) if n > 0 => Some(n),
            _ => None,
        })
        .map(|n| n as usize)
        .unwrap_or(2048);

    if algo == ALGO_RSA {
        let (pk, sk) = crypto_impl::Rsa::generate_keypair(bits);
        let pk_der = crypto_impl::Rsa::public_key_to_der(&pk);
        let sk_der = crypto_impl::Rsa::private_key_to_der(&sk);
        let key_id = crypto_impl::rsa_key_next_id();
        crypto_impl::rsa_key_store(
            key_id,
            crypto_impl::RsaKeyPairData {
                public_key: pk,
                private_key: sk,
            },
        );
        let pub_obj = alloc_public_key(ctx, ALGO_RSA, bits as i32, &pk_der, key_id);
        let priv_obj = alloc_private_key(ctx, ALGO_RSA, bits as i32, &sk_der, key_id);
        return Ok(Some(Value::Object(Some(alloc_keypair(ctx, pub_obj, priv_obj)))));
    }

    if algo == ALGO_EC {
        // Route EC to the real SunEC SPI → concrete ECPublicKey/ECPrivateKey in a
        // real KeyPair (fixes the bare-interface CCE). RSA/AES keep the synthetic
        // path below; CRATONVM_SYNTHETIC_EC=1 restores the legacy synthetic EC.
        if crate::route_ec_to_real() {
            return drive_real_ec_keypair(ctx, this);
        }
        let (pk, sk) = crypto_impl::Ecdsa::generate_keypair();
        let pk_der = crypto_impl::Ecdsa::public_key_to_der(&pk);
        let sk_bytes = crypto_impl::Ecdsa::private_key_to_bytes(&sk);
        let key_id = crypto_impl::ecdsa_key_next_id();
        crypto_impl::ecdsa_key_store(
            key_id,
            crypto_impl::EcdsaKeyPairData {
                public_key: pk,
                private_key: sk,
            },
        );
        let pub_obj = alloc_public_key(ctx, ALGO_EC, 256, &pk_der, key_id);
        let priv_obj = alloc_private_key(ctx, ALGO_EC, 256, &sk_bytes, key_id);
        return Ok(Some(Value::Object(Some(alloc_keypair(ctx, pub_obj, priv_obj)))));
    }

    // Recognised but not implemented (ML-KEM, ML-DSA, Ed25519, X25519, or an
    // unknown name). Real key generation is unavailable, so honour the JDK
    // contract and throw `NoSuchAlgorithmException` rather than minting a
    // KeyPair with empty key material (no-synthetic-stubs policy). RSA and EC
    // returned above with real keys.
    Err(throw_no_such_algorithm(
        ctx,
        &format!("{} KeyPairGenerator not available", algo_name(algo)),
    ))
}

fn kpg_get_algorithm(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let base = synthetic_base_offset(ctx, "java/security/KeyPairGenerator");
    let idx = match ctx.get_field(this, base + KPG_OFF_ALGO) {
        Value::Int(i) => i,
        _ => -1,
    };
    let s = ctx.create_string(algo_name(idx));
    Ok(Some(Value::Object(Some(s))))
}

// ---------------------------------------------------------------------------
// KeyFactory natives
// ---------------------------------------------------------------------------

fn kf_get_instance(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let alg = read_string(ctx, args, 0);
    let idx = algo_idx(&alg);
    let base = synthetic_base_offset(ctx, "java/security/KeyFactory");
    let kf = alloc_concurrent_synthetic(
        ctx,
        "java/security/KeyFactory",
        base + KF_PRIVATE_SLOTS,
    );
    ctx.set_field(kf, base + KF_OFF_ALGO, Value::Int(idx));
    Ok(Some(Value::Object(Some(kf))))
}

/// generatePublic(KeySpec) -> PublicKey.  We surface the most common path
/// (X509EncodedKeySpec wrapping a SubjectPublicKeyInfo DER blob) and
/// re-parse via crypto_impl.  EC (default) routes to the real SunEC
/// KeyFactory.  When we cannot produce a usable key — the RSA/EC spec fails
/// to parse, or the algorithm has no real implementation — we throw
/// `InvalidKeySpecException` (generatePublic's declared checked exception)
/// rather than returning a `key_id == 0` key that silently fails every later
/// verify (no-synthetic-stubs policy).
fn kf_generate_public(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let base = synthetic_base_offset(ctx, "java/security/KeyFactory");
    let algo = match ctx.get_field(this, base + KF_OFF_ALGO) {
        Value::Int(i) => i,
        _ => -1,
    };
    // EC: drive the real SunEC KeyFactory over the ECPublicKeySpec → real
    // ECPublicKeyImpl (the synthetic path can't honour an ECPublicKeySpec).
    if algo == ALGO_EC && crate::route_ec_to_real() {
        if let Some(Value::Object(Some(spec))) = args.get(1) {
            return drive_real_ec_keyfactory(
                ctx,
                *spec,
                "engineGeneratePublic",
                "Ljava/security/PublicKey;",
            );
        }
    }
    let der = if let Some(Value::Object(Some(spec))) = args.get(1) {
        // X509EncodedKeySpec.encoded[B is at field 0 in the synthetic
        // KeySpec shape; for real-JDK KeySpec we read field 0 too —
        // both place the encoded-key byte[] first.
        match ctx.get_field(*spec, 0) {
            Value::Object(Some(arr)) => read_byte_array(ctx, arr),
            _ => Vec::new(),
        }
    } else {
        Vec::new()
    };

    if algo == ALGO_RSA {
        if let Some(pk) = crypto_impl::parse_rsa_public_key(&der) {
            let pk_der = crypto_impl::Rsa::public_key_to_der(&pk);
            // We can't honour an external private-key counterpart from a
            // public-only spec, so we register a public-only handle by
            // pairing with a placeholder private (only sign uses private).
            // Wave 6 leaves verify-only flows fully functional.
            let key_id = crypto_impl::rsa_key_next_id();
            // Manufacture a placeholder paired keypair so the registry
            // entry exists; sign attempts on this id will use the
            // placeholder private key and produce wrong bytes — which is
            // fine because callers only verify with public-import paths.
            crypto_impl::rsa_key_store(
                key_id,
                crypto_impl::RsaKeyPairData {
                    public_key: pk,
                    // Dummy private — never used because public-only
                    // imports always go through verify.
                    private_key: crypto_impl::RsaPrivateKey {
                        n: crypto_impl::BigUint::from_bytes_be(&[1]),
                        d: crypto_impl::BigUint::from_bytes_be(&[1]),
                        e: crypto_impl::BigUint::from_bytes_be(&[1]),
                    },
                },
            );
            return Ok(Some(Value::Object(Some(alloc_public_key(
                ctx, ALGO_RSA, 2048, &pk_der, key_id,
            )))));
        }
    }
    if algo == ALGO_EC {
        if let Some(pk) = crypto_impl::parse_ecdsa_public_key(&der) {
            let pk_der = crypto_impl::Ecdsa::public_key_to_der(&pk);
            let key_id = crypto_impl::ecdsa_key_next_id();
            // Placeholder private key.
            let (_, sk) = crypto_impl::Ecdsa::generate_keypair();
            crypto_impl::ecdsa_key_store(
                key_id,
                crypto_impl::EcdsaKeyPairData {
                    public_key: pk,
                    private_key: sk,
                },
            );
            return Ok(Some(Value::Object(Some(alloc_public_key(
                ctx, ALGO_EC, 256, &pk_der, key_id,
            )))));
        }
    }

    // No usable key could be produced: the RSA/EC spec failed to parse, or the
    // algorithm has no real implementation (ML-KEM, ML-DSA, Ed25519, X25519, or
    // an unknown name). Throw generatePublic's declared `InvalidKeySpecException`
    // rather than returning a `key_id == 0` key that silently fails every later
    // verify (no-synthetic-stubs policy).
    Err(throw_invalid_key_spec(
        ctx,
        &format!(
            "cannot generate a usable {} public key from the given KeySpec",
            algo_name(algo)
        ),
    ))
}

/// generatePrivate(KeySpec) -> PrivateKey.  Only the real SunEC EC path can
/// import a usable private key; `crypto_impl` has no other private-key parser,
/// so every other algorithm would otherwise yield a `key_id == 0` key that can
/// never sign.  Honour generatePrivate's declared `InvalidKeySpecException`
/// rather than returning that unusable synthetic key (no-synthetic-stubs).
fn kf_generate_private(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let base = synthetic_base_offset(ctx, "java/security/KeyFactory");
    let algo = match ctx.get_field(this, base + KF_OFF_ALGO) {
        Value::Int(i) => i,
        _ => -1,
    };
    // EC: drive the real SunEC KeyFactory over the ECPrivateKeySpec → real
    // ECPrivateKeyImpl (the only private-key import we can satisfy).
    if algo == ALGO_EC && crate::route_ec_to_real() {
        if let Some(Value::Object(Some(spec))) = args.get(1) {
            return drive_real_ec_keyfactory(
                ctx,
                *spec,
                "engineGeneratePrivate",
                "Ljava/security/PrivateKey;",
            );
        }
    }
    Err(throw_invalid_key_spec(
        ctx,
        &format!(
            "cannot generate a usable {} private key from the given KeySpec",
            algo_name(algo)
        ),
    ))
}

fn kf_get_algorithm(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let base = synthetic_base_offset(ctx, "java/security/KeyFactory");
    let idx = match ctx.get_field(this, base + KF_OFF_ALGO) {
        Value::Int(i) => i,
        _ => -1,
    };
    let s = ctx.create_string(algo_name(idx));
    Ok(Some(Value::Object(Some(s))))
}

// ---------------------------------------------------------------------------
// KeyPair / Key accessors
// ---------------------------------------------------------------------------

fn keypair_get_public(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let slot0 = ctx.get_field(this, 0);
    // Our synthetic KeyPair stores publicKey@0 (a synthetic java/security/PublicKey).
    // A *real* java.security.KeyPair (from real EC keygen or `new KeyPair(pub,priv)`)
    // lays out privateKey@0, publicKey@1 — so for it the public key is at slot 1.
    if crate::route_ec_to_real() && !is_synthetic_key_obj(ctx, &slot0, "java/security/PublicKey") {
        return Ok(Some(ctx.get_field(this, 1)));
    }
    Ok(Some(slot0))
}

fn keypair_get_private(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let slot1 = ctx.get_field(this, 1);
    // Synthetic KeyPair: privateKey@1 (synthetic java/security/PrivateKey). A real
    // java.security.KeyPair has publicKey@1, privateKey@0.
    if crate::route_ec_to_real() && !is_synthetic_key_obj(ctx, &slot1, "java/security/PrivateKey") {
        return Ok(Some(ctx.get_field(this, 0)));
    }
    Ok(Some(slot1))
}

fn key_get_algorithm(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let idx = match ctx.get_field(this, KEY_FIELD_ALGO) {
        Value::Int(i) => i,
        _ => -1,
    };
    let s = ctx.create_string(algo_name(idx));
    Ok(Some(Value::Object(Some(s))))
}

fn key_get_encoded(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let der = match ctx.get_field(this, KEY_FIELD_DER) {
        Value::Object(Some(arr)) => read_byte_array(ctx, arr),
        _ => Vec::new(),
    };
    let arr = alloc_byte_array(ctx, &der);
    Ok(Some(Value::Object(Some(arr))))
}

fn pubkey_get_format(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let s = ctx.create_string("X.509");
    Ok(Some(Value::Object(Some(s))))
}

fn privkey_get_format(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let s = ctx.create_string("PKCS#8");
    Ok(Some(Value::Object(Some(s))))
}

// ---------------------------------------------------------------------------
// <clinit> shims for JCA classes the bytecode walks before our intercepts
// fire.
// ---------------------------------------------------------------------------

fn clinit_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register(r: &mut NativeMethodRegistry) {
    // Real-JCA bring-up: skip these synthetic short-circuit shims so
    // KeyPairGenerator/KeyFactory.getInstance fall through to the real
    // JDK 25 + BouncyCastle provider bytecode (yields concrete BCEC/BCRSA
    // keys instead of bare-interface `java/security/PrivateKey` synthetics).
    if crate::real_jca_mode() {
        return;
    }
    let kpg = "java/security/KeyPairGenerator";
    r.register(kpg, "getInstance", "(Ljava/lang/String;)Ljava/security/KeyPairGenerator;", kpg_get_instance);
    r.register(
        kpg,
        "getInstance",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/security/KeyPairGenerator;",
        kpg_get_instance,
    );
    r.register(
        kpg,
        "getInstance",
        "(Ljava/lang/String;Ljava/security/Provider;)Ljava/security/KeyPairGenerator;",
        kpg_get_instance,
    );
    r.register(kpg, "initialize", "(I)V", kpg_initialize_int);
    r.register(kpg, "initialize", "(ILjava/security/SecureRandom;)V", kpg_initialize_int_random);
    r.register(kpg, "initialize", "(Ljava/security/spec/AlgorithmParameterSpec;)V", kpg_initialize_spec);
    r.register(
        kpg,
        "initialize",
        "(Ljava/security/spec/AlgorithmParameterSpec;Ljava/security/SecureRandom;)V",
        kpg_initialize_spec_random,
    );
    r.register(kpg, "generateKeyPair", "()Ljava/security/KeyPair;", kpg_generate_key_pair);
    r.register(kpg, "genKeyPair", "()Ljava/security/KeyPair;", kpg_generate_key_pair);
    r.register(kpg, "getAlgorithm", "()Ljava/lang/String;", kpg_get_algorithm);
    // <clinit> shim — the JDK-25 KeyPairGenerator.<clinit> reads
    // `sun.security.util.Debug.getInstance("jca", "KeyPairGenerator")`
    // which we already shim, but defensively no-op the whole clinit so
    // any future field bring-up failure doesn't cascade.
    r.register(kpg, "<clinit>", "()V", clinit_noop);

    let kf = "java/security/KeyFactory";
    r.register(kf, "getInstance", "(Ljava/lang/String;)Ljava/security/KeyFactory;", kf_get_instance);
    r.register(
        kf,
        "getInstance",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/security/KeyFactory;",
        kf_get_instance,
    );
    r.register(
        kf,
        "getInstance",
        "(Ljava/lang/String;Ljava/security/Provider;)Ljava/security/KeyFactory;",
        kf_get_instance,
    );
    r.register(
        kf,
        "generatePublic",
        "(Ljava/security/spec/KeySpec;)Ljava/security/PublicKey;",
        kf_generate_public,
    );
    r.register(
        kf,
        "generatePrivate",
        "(Ljava/security/spec/KeySpec;)Ljava/security/PrivateKey;",
        kf_generate_private,
    );
    r.register(kf, "getAlgorithm", "()Ljava/lang/String;", kf_get_algorithm);
    r.register(kf, "<clinit>", "()V", clinit_noop);

    let kp = "java/security/KeyPair";
    r.register(kp, "getPublic", "()Ljava/security/PublicKey;", keypair_get_public);
    r.register(kp, "getPrivate", "()Ljava/security/PrivateKey;", keypair_get_private);

    // Public/Private Key common accessors.  The synthetic `PublicKey` /
    // `PrivateKey` classes are interfaces in the JDK; we treat them as
    // concrete proxies here.  The real-JDK implementation classes are
    // `sun.security.provider.RSAPublicKey` etc., but `getInstance` /
    // `KeyFactory.generatePublic` return our synthetic, and the JDK code
    // dispatches on the interface — invokeinterface walks our synthetic
    // class's method table.  That works because the registry is keyed
    // by class name and our synthetic class is named
    // `java/security/PublicKey`.
    r.register("java/security/PublicKey", "getAlgorithm", "()Ljava/lang/String;", key_get_algorithm);
    r.register("java/security/PublicKey", "getEncoded", "()[B", key_get_encoded);
    r.register("java/security/PublicKey", "getFormat", "()Ljava/lang/String;", pubkey_get_format);
    r.register("java/security/PrivateKey", "getAlgorithm", "()Ljava/lang/String;", key_get_algorithm);
    r.register("java/security/PrivateKey", "getEncoded", "()[B", key_get_encoded);
    r.register("java/security/PrivateKey", "getFormat", "()Ljava/lang/String;", privkey_get_format);

    // <clinit> shim for sun.security.jca.GetInstance — the bytecode-side
    // helper that throws our NPE.  No-opping is safe because we never
    // dispatch into this class once getInstance() is intercepted.
    r.register("sun/security/jca/GetInstance", "<clinit>", "()V", clinit_noop);
    r.register("sun/security/jca/JCAUtil", "<clinit>", "()V", clinit_noop);

    // Signature also goes through `Signature.<clinit>` -> Debug; shim it
    // too so jca::signature can fire its overrides without the bytecode
    // running first.
    r.register("java/security/Signature", "<clinit>", "()V", clinit_noop);
    r.register("java/security/MessageDigest", "<clinit>", "()V", clinit_noop);

    // ECGenParameterSpec.<init>(String) — no-op except for stashing the
    // curve name so initialize(spec) can inspect it.  We intentionally
    // do NOT register getName etc. — the EC native ignores the curve
    // because we only support P-256.
    r.register(
        "java/security/spec/ECGenParameterSpec",
        "<init>",
        "(Ljava/lang/String;)V",
        |ctx, args| {
            let this = this_arg(args)?;
            if let Some(Value::Object(Some(s))) = args.get(1) {
                ctx.set_field(this, 0, Value::Object(Some(*s)));
            }
            Ok(None)
        },
    );
    r.register(
        "java/security/spec/ECGenParameterSpec",
        "getName",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = this_arg(args)?;
            Ok(Some(ctx.get_field(this, 0)))
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn algo_idx_round_trip() {
        assert_eq!(algo_idx("RSA"), ALGO_RSA);
        assert_eq!(algo_idx("rsa"), ALGO_RSA);
        assert_eq!(algo_idx("EC"), ALGO_EC);
        assert_eq!(algo_idx("ECDSA"), ALGO_EC);
        assert_eq!(algo_idx("Ed25519"), ALGO_ED25519);
        assert_eq!(algo_idx("Garbage"), -1);
    }

    #[test]
    fn algo_name_round_trip() {
        assert_eq!(algo_name(ALGO_RSA), "RSA");
        assert_eq!(algo_name(ALGO_EC), "EC");
        assert_eq!(algo_name(ALGO_ED25519), "Ed25519");
        assert_eq!(algo_name(-1), "Unknown");
    }

    /// No-synthetic-stubs policy: a `KeyPairGenerator` for an algorithm we
    /// recognise but cannot implement (ML-KEM, ML-DSA, Ed25519, X25519) — or an
    /// outright unknown name — must throw from `generateKeyPair`, never return a
    /// `KeyPair` with empty key material. The previous fallback minted an
    /// empty-DER / `key_id == 0` key, presenting failed keygen as success.
    #[test]
    fn unimplemented_algorithm_keygen_throws_not_empty_key() {
        for algo in ["ML-KEM-512", "ML-KEM-768", "ML-DSA-44", "ML-DSA-65", "X25519", "Ed25519", "Totally-Bogus"] {
            let mut ctx = crate::test_utils::MockNativeContext::new();
            let name = ctx.create_string(algo);
            let kpg = kpg_get_instance(&mut ctx, &[Value::Object(Some(name))])
                .expect("getInstance should not fail")
                .expect("getInstance should return a KeyPairGenerator");
            let err = kpg_generate_key_pair(&mut ctx, &[kpg]).expect_err(&format!(
                "{algo} generateKeyPair must throw, not silently return an empty key"
            ));
            // Real-JDK mode constructs a genuine NoSuchAlgorithmException; the
            // mock's `new_object_initialized` yields a Throwable object, so we
            // get `ExceptionThrown` (the catchable path) rather than the
            // `SecurityException` defensive fallback.
            match err {
                MethodCallFailed::ExceptionThrown(_) => {}
                other => panic!(
                    "{algo}: expected ExceptionThrown(NoSuchAlgorithmException), got {other:?}"
                ),
            }
        }
    }

    /// Guard against the fail-closed change accidentally catching RSA: RSA must
    /// still produce a `KeyPair` backed by real DER + a non-zero `key_id`. Uses
    /// a small key size to keep the software keygen fast.
    #[test]
    fn rsa_keygen_still_returns_real_key_material() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let name = ctx.create_string("RSA");
        let kpg = kpg_get_instance(&mut ctx, &[Value::Object(Some(name))])
            .unwrap()
            .unwrap();
        let kpg_ref = match kpg {
            Value::Object(Some(o)) => o,
            other => panic!("expected KeyPairGenerator, got {other:?}"),
        };
        // initialize(512) — exercises the real keygen path without the cost of
        // a 2048-bit prime search.
        kpg_initialize_int(&mut ctx, &[Value::Object(Some(kpg_ref)), Value::Int(512)]).unwrap();
        let kp = match kpg_generate_key_pair(&mut ctx, &[Value::Object(Some(kpg_ref))])
            .unwrap()
            .unwrap()
        {
            Value::Object(Some(o)) => o,
            other => panic!("expected KeyPair object, got {other:?}"),
        };
        let pubk = match ctx.get_field(kp, 0) {
            Value::Object(Some(o)) => o,
            other => panic!("expected public key, got {other:?}"),
        };
        assert!(
            matches!(ctx.get_field(pubk, KEY_FIELD_KEYID), Value::Long(id) if id != 0),
            "RSA public key must carry a real key_id"
        );
        match ctx.get_field(pubk, KEY_FIELD_DER) {
            Value::Object(Some(der)) => {
                assert!(ctx.array_length(der) > 0, "RSA public DER must be non-empty")
            }
            other => panic!("expected DER byte[], got {other:?}"),
        }
    }

    /// Build a `KeyFactory` synthetic for `algo` and call `generatePublic` /
    /// `generatePrivate` with a KeySpec whose encoded byte[] (field 0) is `der`.
    fn kf_call(
        ctx: &mut crate::test_utils::MockNativeContext,
        algo: &str,
        method: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult,
        der: &[u8],
    ) -> Result<Option<Value>, MethodCallFailed> {
        let name = ctx.create_string(algo);
        let kf = kf_get_instance(ctx, &[Value::Object(Some(name))])
            .unwrap()
            .unwrap();
        let kf_ref = match kf {
            Value::Object(Some(o)) => o,
            other => panic!("expected KeyFactory, got {other:?}"),
        };
        let spec = alloc_concurrent_synthetic(ctx, "java/security/spec/X509EncodedKeySpec", 1);
        let der_arr = alloc_byte_array(ctx, der);
        ctx.set_field(spec, 0, Value::Object(Some(der_arr)));
        method(ctx, &[Value::Object(Some(kf_ref)), Value::Object(Some(spec))])
    }

    /// No-synthetic-stubs policy on the KeyFactory import path: when no usable
    /// key can be produced — an unimplemented algorithm, or an RSA/EC spec that
    /// fails to parse — generatePublic/generatePrivate must throw
    /// `InvalidKeySpecException`, never return a `key_id == 0` key that silently
    /// fails every later verify/sign.
    #[test]
    fn keyfactory_unproducible_key_throws_not_dead_key() {
        // generatePublic: unimplemented algorithms.
        for algo in ["ML-KEM-512", "ML-DSA-65", "X25519", "Ed25519", "Totally-Bogus"] {
            let mut ctx = crate::test_utils::MockNativeContext::new();
            let err = kf_call(&mut ctx, algo, kf_generate_public, &[1, 2, 3])
                .expect_err(&format!("{algo} generatePublic must throw, not return a dead key"));
            assert!(
                matches!(err, MethodCallFailed::ExceptionThrown(_)),
                "{algo} generatePublic: expected ExceptionThrown(InvalidKeySpecException), got {err:?}"
            );
        }
        // generatePublic: RSA with an unparseable spec.
        {
            let mut ctx = crate::test_utils::MockNativeContext::new();
            let err = kf_call(&mut ctx, "RSA", kf_generate_public, &[0xDE, 0xAD, 0xBE, 0xEF])
                .expect_err("RSA generatePublic with garbage DER must throw");
            assert!(matches!(err, MethodCallFailed::ExceptionThrown(_)), "got {err:?}");
        }
        // generatePrivate: no private-key importer exists for RSA (or anything
        // but the real-SunEC EC path) → must throw.
        for algo in ["RSA", "ML-DSA-65"] {
            let mut ctx = crate::test_utils::MockNativeContext::new();
            let err = kf_call(&mut ctx, algo, kf_generate_private, &[1, 2, 3])
                .expect_err(&format!("{algo} generatePrivate must throw, not return a dead key"));
            assert!(
                matches!(err, MethodCallFailed::ExceptionThrown(_)),
                "{algo} generatePrivate: expected ExceptionThrown(InvalidKeySpecException), got {err:?}"
            );
        }
    }

    /// Guard the real RSA public-key import path: a valid SubjectPublicKeyInfo
    /// DER must still round-trip to a key backed by a real `key_id` — the
    /// fail-closed change must not catch the parseable case.
    #[test]
    fn keyfactory_rsa_public_import_still_returns_real_key() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let (pk, _sk) = crypto_impl::Rsa::generate_keypair(512);
        let der = crypto_impl::Rsa::public_key_to_der(&pk);
        let pub_obj = match kf_call(&mut ctx, "RSA", kf_generate_public, &der)
            .expect("valid RSA spec must not throw")
            .expect("generatePublic must return a key")
        {
            Value::Object(Some(o)) => o,
            other => panic!("expected PublicKey, got {other:?}"),
        };
        assert!(
            matches!(ctx.get_field(pub_obj, KEY_FIELD_KEYID), Value::Long(id) if id != 0),
            "imported RSA public key must carry a real key_id"
        );
    }
}
