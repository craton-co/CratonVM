// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `javax.crypto.KeyAgreement` — ECDH key agreement.
//!
//! The real `KeyAgreement.getInstance("ECDH")` routes through
//! `sun.security.jca.GetInstance.getService(...)`, which CratonVM's minimal
//! provider list cannot satisfy → `NoSuchAlgorithmException: Algorithm ECDH not
//! available` (keycloak's `BCEcdhEsAlgorithmProvider.deriveKey` /
//! ECDH-ES JWE). Rather than reimplement the EC Diffie-Hellman primitive, we
//! intercept the public `KeyAgreement` surface and drive the real
//! `sun.security.ec.ECDHKeyAgreement` SPI directly (the same approach used for
//! the EC/RSA `KeyFactory` and `Cipher` SPIs). EC field math is interpreted
//! (the `intpoly` JIT ban), so the shared secret is computed correctly.
//!
//! The SPI object is stashed in a GC-scanned synthetic slot appended after the
//! real `KeyAgreement` field layout, so it stays live/forwarded across the
//! `getInstance` → `init` → `doPhase` → `generateSecret` call sequence (the same
//! pattern as `signature.rs`'s `SIG_OFF_KEYOBJ`).

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ClassId, ObjectRef, Value};

use crate::{try_alloc_concurrent_synthetic, obj_arg};

const KA_CLASS: &str = "javax/crypto/KeyAgreement";
const ECDH_SPI: &str = "sun/security/ec/ECDHKeyAgreement";

/// First synthetic slot index (= real `KeyAgreement` instance-field count).
fn base_offset(ctx: &mut dyn NativeContext) -> usize {
    let cid = ctx
        .ensure_class_initialized(KA_CLASS)
        .unwrap_or(ClassId::new(0));
    ctx.class_num_total_fields(cid)
}

fn is_ecdh(alg: &str) -> bool {
    let u = alg.to_ascii_uppercase();
    u == "ECDH" || u == "ECDHC" || u == "ECCDH"
}

fn read_string(ctx: &mut dyn NativeContext, args: &[Value], idx: usize) -> String {
    match args.get(idx) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    }
}

fn throw_no_such_algorithm(ctx: &mut dyn NativeContext, msg: &str) -> MethodCallFailed {
    let detail = ctx.create_string(msg);
    if let Ok(Some(Value::Object(Some(exc)))) = ctx.new_object_initialized(
        "java/security/NoSuchAlgorithmException",
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

/// Read the stashed `ECDHKeyAgreement` SPI from the synthetic slot.
fn ka_spi(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    let base = base_offset(ctx);
    match ctx.get_field(this, base) {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    }
}

fn ka_get_instance(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let alg = read_string(ctx, args, 0);
    if !is_ecdh(&alg) {
        return Err(throw_no_such_algorithm(
            ctx,
            &format!("Algorithm {alg} not available"),
        ));
    }
    // Construct the real SunEC ECDH SPI and stash it in a GC-scanned slot.
    let spi = match ctx.new_object_initialized(ECDH_SPI, "()V", &[])? {
        Some(Value::Object(Some(o))) => o,
        _ => {
            return Err(throw_no_such_algorithm(
                ctx,
                "Algorithm ECDH not available (no ECDHKeyAgreement SPI)",
            ))
        }
    };
    let pin = ctx.pin_native_root(spi);
    let base = base_offset(ctx);
    let obj = try_alloc_concurrent_synthetic(ctx, KA_CLASS, base + 1)?;
    let spi = ctx.read_native_pin(pin, spi);
    ctx.set_field(obj, base, Value::Object(Some(spi)));
    ctx.unpin_native_roots(pin);
    Ok(Some(Value::Object(Some(obj))))
}

/// `init(Key)` / `init(Key, SecureRandom)` → `spi.engineInit(key, random)`.
fn ka_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key = match args.get(1) {
        Some(Value::Object(Some(k))) => *k,
        _ => {
            return Err(RuntimeError::IllegalStateException {
                message: "KeyAgreement.init: null key".into(),
            }
            .into())
        }
    };
    let random = match args.get(2) {
        Some(Value::Object(opt)) => Value::Object(*opt),
        _ => Value::Object(None),
    };
    let Some(spi) = ka_spi(ctx, this) else {
        return Err(RuntimeError::IllegalStateException {
            message: "KeyAgreement not initialized (no SPI)".into(),
        }
        .into());
    };
    ctx.invoke_virtual(
        spi,
        "engineInit",
        "(Ljava/security/Key;Ljava/security/SecureRandom;)V",
        &[Value::Object(Some(key)), random],
    )?;
    Ok(None)
}

/// `doPhase(Key, boolean)` → `spi.engineDoPhase(key, lastPhase)` (returns a Key,
/// or null for ECDH's terminal phase).
fn ka_do_phase(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key = match args.get(1) {
        Some(Value::Object(Some(k))) => *k,
        _ => {
            return Err(RuntimeError::IllegalStateException {
                message: "KeyAgreement.doPhase: null key".into(),
            }
            .into())
        }
    };
    let last = match args.get(2) {
        Some(Value::Int(b)) => *b,
        _ => 1,
    };
    let Some(spi) = ka_spi(ctx, this) else {
        return Err(RuntimeError::IllegalStateException {
            message: "KeyAgreement not initialized (no SPI)".into(),
        }
        .into());
    };
    ctx.invoke_virtual(
        spi,
        "engineDoPhase",
        "(Ljava/security/Key;Z)Ljava/security/Key;",
        &[Value::Object(Some(key)), Value::Int(last)],
    )
}

/// `generateSecret()` → `spi.engineGenerateSecret()` (raw shared secret bytes).
fn ka_generate_secret(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let Some(spi) = ka_spi(ctx, this) else {
        return Err(RuntimeError::IllegalStateException {
            message: "KeyAgreement not initialized (no SPI)".into(),
        }
        .into());
    };
    ctx.invoke_virtual(spi, "engineGenerateSecret", "()[B", &[])
}

/// `generateSecret(String algorithm)` → `spi.engineGenerateSecret(String)`
/// (wraps the shared secret in a `SecretKey` of the named algorithm).
fn ka_generate_secret_alg(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let alg = match args.get(1) {
        Some(Value::Object(Some(s))) => *s,
        _ => {
            return Err(RuntimeError::IllegalStateException {
                message: "KeyAgreement.generateSecret: null algorithm".into(),
            }
            .into())
        }
    };
    let Some(spi) = ka_spi(ctx, this) else {
        return Err(RuntimeError::IllegalStateException {
            message: "KeyAgreement not initialized (no SPI)".into(),
        }
        .into());
    };
    ctx.invoke_virtual(
        spi,
        "engineGenerateSecret",
        "(Ljava/lang/String;)Ljavax/crypto/SecretKey;",
        &[Value::Object(Some(alg))],
    )
}

pub fn register(r: &mut NativeMethodRegistry) {
    let cls = KA_CLASS;
    for desc in [
        "(Ljava/lang/String;)Ljavax/crypto/KeyAgreement;",
        "(Ljava/lang/String;Ljava/lang/String;)Ljavax/crypto/KeyAgreement;",
        "(Ljava/lang/String;Ljava/security/Provider;)Ljavax/crypto/KeyAgreement;",
    ] {
        r.register(cls, "getInstance", desc, ka_get_instance);
    }
    r.register(cls, "init", "(Ljava/security/Key;)V", ka_init);
    r.register(
        cls,
        "init",
        "(Ljava/security/Key;Ljava/security/SecureRandom;)V",
        ka_init,
    );
    r.register(
        cls,
        "doPhase",
        "(Ljava/security/Key;Z)Ljava/security/Key;",
        ka_do_phase,
    );
    r.register(cls, "generateSecret", "()[B", ka_generate_secret);
    r.register(
        cls,
        "generateSecret",
        "(Ljava/lang/String;)Ljavax/crypto/SecretKey;",
        ka_generate_secret_alg,
    );
}
