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
///
/// Two slots follow it: the SPI at `base`, and the requested algorithm name at
/// `base + 1` (which is what [`ka_get_provider`] answers from).
fn base_offset(ctx: &mut dyn NativeContext) -> usize {
    let cid = ctx
        .ensure_class_initialized(KA_CLASS)
        .unwrap_or(ClassId::new(0));
    ctx.class_num_total_fields(cid)
}

/// Offset of the algorithm-name slot, past the SPI.
const KA_OFF_NAME: usize = 1;
/// Total synthetic slots appended past the real layout.
const KA_NUM_SLOTS: usize = 2;

/// The real `KeyAgreementSpi` class the JDK own providers register for `alg`,
/// with the provider that registers it.
///
/// Read off HotSpot JDK 25 by enumerating `Provider.getServices()`, not
/// guessed: SunEC owns the whole EC/XDH surface including the `XDH` umbrella
/// (whose SPI is the NON-nested base class, the same split
/// `xdh_kpg_spi_class` records for key generation), while finite-field DH is
/// SunJCE and is registered under the name `DiffieHellman`.
///
/// `KeyAgreement.getInstance("X25519")` refused before 2026-08-14 — this
/// engine knew one algorithm — which is one of the four
/// `NoSuchAlgorithmException` rows the JCA engine residuals page opened with.
fn ka_spi_class(alg: &str) -> Option<(&'static str, &'static str)> {
    match alg.to_ascii_uppercase().as_str() {
        "ECDH" | "ECDHC" | "ECCDH" => Some((ECDH_SPI, "SunEC")),
        "X25519" => Some(("sun/security/ec/XDHKeyAgreement$X25519", "SunEC")),
        "X448" => Some(("sun/security/ec/XDHKeyAgreement$X448", "SunEC")),
        "XDH" => Some(("sun/security/ec/XDHKeyAgreement", "SunEC")),
        "DH" | "DIFFIEHELLMAN" => Some(("com/sun/crypto/provider/DHKeyAgreement", "SunJCE")),
        _ => None,
    }
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
    let requested_provider = crate::jca::provider_chain::provider_arg_name(ctx, args, 1);
    // A named THIRD-PARTY provider supplies its own `KeyAgreementSpi`. Every
    // native on this class already forwards to whatever SPI sits in the slot, so
    // this is a change of which class gets constructed and nothing else — but it
    // is the difference between `getInstance("ECDH", "BC")` running BouncyCastle
    // and running SunEC under BouncyCastle's name (`getProvider()` answered
    // `SunEC`, where HotSpot answers `BC`).
    let third_party_spi = requested_provider.as_deref().and_then(|p| {
        crate::jca::provider_chain::third_party_service_class(Some(p), "KeyAgreement", &alg)
    });
    let spi_class_owned = match third_party_spi {
        Some(cls) => cls.replace('.', "/"),
        None => {
            let Some((spi_class, _provider)) = ka_spi_class(&alg) else {
                return Err(throw_no_such_algorithm(
                    ctx,
                    &format!("Algorithm {alg} not available"),
                ));
            };
            spi_class.to_string()
        }
    };
    let spi_class = spi_class_owned.as_str();
    // Construct the real provider SPI and stash it in a GC-scanned slot.
    let spi = match ctx.new_object_initialized(spi_class, "()V", &[])? {
        Some(Value::Object(Some(o))) => o,
        _ => {
            return Err(throw_no_such_algorithm(
                ctx,
                &format!("Algorithm {alg} not available (no {spi_class} SPI)"),
            ))
        }
    };
    let pin = ctx.pin_native_root(spi);
    let base = base_offset(ctx);
    let obj = try_alloc_concurrent_synthetic(ctx, KA_CLASS, base + KA_NUM_SLOTS)?;
    let obj_pin = ctx.pin_native_root(obj);
    let spi = ctx.read_native_pin(pin, spi);
    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.set_field(obj, base, Value::Object(Some(spi)));
    // `create_string` allocates, so re-read the receiver through its pin
    // afterwards — the name write must land in the post-move object.
    let name = ctx.create_string(&alg);
    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.set_field(obj, base + KA_OFF_NAME, Value::Object(Some(name)));
    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.unpin_native_roots(pin);
    // Attribution — see `ka_get_provider`.
    if let Some(provider) = requested_provider.as_deref() {
        crate::jca::provider_chain::record_requested_provider(ctx, obj, provider);
    }
    let obj = ctx.read_native_pin(obj_pin, obj);
    Ok(Some(Value::Object(Some(obj))))
}

/// `getAlgorithm()` — the name this engine was asked for.
fn ka_get_algorithm(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let base = base_offset(ctx);
    Ok(Some(ctx.get_field(this, base + KA_OFF_NAME)))
}

/// `getProvider()`.
///
/// Nothing was registered for it, so the real JDK bytecode ran and read the
/// `provider` field this VM never assigns — and on a synthetic receiver that
/// THREW, which `probes/JcaGetInstanceProbe` records as `provider=?`. The same
/// one-line treatment `kpg_get_provider` got.
fn ka_get_provider(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // A provider the caller named at `getInstance` wins over the algorithm-keyed
    // guess below — see `provider_chain::record_requested_provider`.
    if let Some(p) = crate::jca::provider_chain::recorded_requested_provider(ctx, this) {
        return Ok(Some(Value::Object(Some(p))));
    }
    let base = base_offset(ctx);
    let alg = match ctx.get_field(this, base + KA_OFF_NAME) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    // A `KeyAgreement` can only exist for an algorithm `ka_spi_class` matched,
    // so the fallback is unreachable in practice; `SunEC` is what every EC/XDH
    // row answers and the least surprising default if it ever is reached.
    let provider = ka_spi_class(&alg).map(|(_, p)| p).unwrap_or("SunEC");
    let p = crate::jca::make_named_provider(ctx, provider)?;
    Ok(Some(Value::Object(Some(p))))
}

/// `init(Key)` / `init(Key, SecureRandom)` → `spi.engineInit(key, random)`.
///
/// A NULL key is FORWARDED, not refused. `javax.crypto.KeyAgreement.init` does
/// no null check at all — its body is `spi.engineInit(key, random)` and
/// nothing else — and whether a null key is usable is the SPI's question,
/// not this layer's. BouncyCastle's NewHope answers "yes": the responder side
/// of an NH exchange has no private key and inits with `(null, random)` by
/// design (`pqc.jcajce.provider.test.NewHopeTest.testKeyExchange`, verbatim
/// from the protocol). Refusing it here turned a supported exchange into
/// `IllegalStateException: KeyAgreement.init: null key` on a call HotSpot
/// completes.
fn ka_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key = match args.get(1) {
        Some(Value::Object(k)) => *k,
        _ => None,
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
        &[Value::Object(key), random],
    )?;
    Ok(None)
}

/// `init(Key, AlgorithmParameterSpec)` / `init(Key, AlgorithmParameterSpec,
/// SecureRandom)` → `spi.engineInit(key, params, random)`.
///
/// Both were UNREGISTERED, so they ran the real `javax.crypto.KeyAgreement`
/// bytecode against a receiver whose state lives in this engine's own slots —
/// `chooseProvider()` opens with `synchronized (lock)` on a field this VM never
/// writes, so every parameterised key agreement died with
/// `NullPointerException: Cannot enter synchronized block because "this.lock" is
/// null`. Measured on bc-java's `crmf` suite, whose `PKIArchiveControlBuilder`
/// takes exactly this door (`JceKeyAgreeRecipientInfoGenerator` →
/// `KeyAgreement.init(key, ukmSpec, random)`). Same species as the `Mac`
/// `doFinal([BI)V` note in `phases_late/ssl_security.rs`: the object looks
/// healthy right up to the one overload nobody registered.
fn ka_init_spec(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // Forwarded rather than refused, for the reason `ka_init` gives.
    let key = match args.get(1) {
        Some(Value::Object(k)) => *k,
        _ => None,
    };
    let params = match args.get(2) {
        Some(Value::Object(opt)) => Value::Object(*opt),
        _ => Value::Object(None),
    };
    let random = match args.get(3) {
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
        "(Ljava/security/Key;Ljava/security/spec/AlgorithmParameterSpec;Ljava/security/SecureRandom;)V",
        &[Value::Object(key), params, random],
    )?;
    Ok(None)
}

/// `generateSecret(byte[] sharedSecret, int offset)` → `spi.engineGenerateSecret`.
/// The third unregistered overload of the same family; it returns the number of
/// bytes written.
fn ka_generate_secret_into(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let buf = args.get(1).copied().unwrap_or(Value::Object(None));
    let off = args.get(2).copied().unwrap_or(Value::Int(0));
    let Some(spi) = ka_spi(ctx, this) else {
        return Err(RuntimeError::IllegalStateException {
            message: "KeyAgreement not initialized (no SPI)".into(),
        }
        .into());
    };
    ctx.invoke_virtual(spi, "engineGenerateSecret", "([BI)I", &[buf, off])
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
        "init",
        "(Ljava/security/Key;Ljava/security/spec/AlgorithmParameterSpec;)V",
        ka_init_spec,
    );
    r.register(
        cls,
        "init",
        "(Ljava/security/Key;Ljava/security/spec/AlgorithmParameterSpec;Ljava/security/SecureRandom;)V",
        ka_init_spec,
    );
    r.register(
        cls,
        "doPhase",
        "(Ljava/security/Key;Z)Ljava/security/Key;",
        ka_do_phase,
    );
    r.register(cls, "generateSecret", "([BI)I", ka_generate_secret_into);
    r.register(cls, "generateSecret", "()[B", ka_generate_secret);
    r.register(
        cls,
        "generateSecret",
        "(Ljava/lang/String;)Ljavax/crypto/SecretKey;",
        ka_generate_secret_alg,
    );
    r.register(
        cls,
        "getProvider",
        "()Ljava/security/Provider;",
        ka_get_provider,
    );
    r.register(cls, "getAlgorithm", "()Ljava/lang/String;", ka_get_algorithm);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every algorithm this engine serves names the SPI class and the provider
    /// HotSpot own registration does — enumerated from `Provider.getServices()`
    /// on JDK 25, where the SunEC/SunJCE split is not derivable from the family.
    #[test]
    fn ka_spi_classes_match_the_jdk_registrations() {
        for (alg, spi, provider) in [
            ("ECDH", ECDH_SPI, "SunEC"),
            ("ecdh", ECDH_SPI, "SunEC"),
            ("X25519", "sun/security/ec/XDHKeyAgreement$X25519", "SunEC"),
            ("X448", "sun/security/ec/XDHKeyAgreement$X448", "SunEC"),
            ("XDH", "sun/security/ec/XDHKeyAgreement", "SunEC"),
            ("DH", "com/sun/crypto/provider/DHKeyAgreement", "SunJCE"),
            (
                "DiffieHellman",
                "com/sun/crypto/provider/DHKeyAgreement",
                "SunJCE",
            ),
        ] {
            assert_eq!(ka_spi_class(alg), Some((spi, provider)), "{alg}");
        }
        assert_eq!(ka_spi_class("TOTALLY-BOGUS-ALG"), None);
        assert_eq!(ka_spi_class(""), None);
    }
}
