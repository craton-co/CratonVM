// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `javax.crypto.KEM` (ML-KEM, FIPS 203) real-provider routing.
//!
//! CratonVM has no native ML-KEM lattice crypto. JDK 25 ships a real pure-Java
//! implementation in SunJCE — `com.sun.crypto.provider.ML_KEM_Impls`, with one
//! concrete `KEMSpi` (`javax.crypto.KEMSpi`) per NIST parameter set:
//!
//! | KEM algorithm | SPI class               | NIST category |
//! |---------------|-------------------------|---------------|
//! | `ML-KEM-512`  | `ML_KEM_Impls$K2`       | 2             |
//! | `ML-KEM-768`  | `ML_KEM_Impls$K3`       | 3             |
//! | `ML-KEM-1024` | `ML_KEM_Impls$K5`       | 5             |
//! | `ML-KEM`      | `ML_KEM_Impls$K`        | from key      |
//!
//! These are the exact siblings of the `$KPG{n}`/`$KF{n}` keygen/keyfactory
//! classes the PQC keygen route already drives
//! (`jca::key_factory::pqc_spi_classes`), and the same SHA3/SHAKE256 native
//! precondition applies (`lib.rs` `SHA3.keccak` override gives the JDK lattice
//! code real sponge output).
//!
//! ## How the routing works
//!
//! We intercept the public `javax.crypto.KEM` surface the same way
//! `jca::signature` intercepts `java.security.Signature` for ML-DSA: a synthetic
//! mirror carries state and the real provider SPI is driven directly, bypassing
//! the synthetic `Security`/`Provider` resolution that would otherwise fail
//! closed with `NoSuchAlgorithmException`. We never rely on the real
//! `KEM`/`KEM$Encapsulator`/`KEM$Decapsulator` *instance* bytecode — every
//! method is a native that drives the real SPI's public `engine*` methods:
//!
//! * `KEM.getInstance(alg)` → construct the real `ML_KEM_Impls$K*` SPI (fail
//!   closed if the provider class is absent) and stash it on a synthetic
//!   `javax/crypto/KEM` mirror.
//! * `KEM.newEncapsulator(pk, …)` → `spi.engineNewEncapsulator(pk, spec, sr)`
//!   (the real `NamedKEM` translates the key + derives the parameter set), wrap
//!   the returned `KEMSpi$EncapsulatorSpi` in a synthetic `KEM$Encapsulator`.
//! * `Encapsulator.encapsulate()` → `e.engineEncapsulate(0, secretSize, "Generic")`
//!   returning the real `KEM$Encapsulated` (real `SecretKey` + ciphertext).
//! * Decapsulation mirrors the same shape via `engineNewDecapsulator` /
//!   `engineDecapsulate`.
//!
//! A null `SecureRandom` is passed through unchanged — `ML_KEM_Impls`
//! defaults it to `JCAUtil.getDefSecureRandom()` internally, exactly as the
//! real `KEM.newEncapsulator(pk)` overload relies on — so the synthetic mirror
//! reproduces HotSpot semantics rather than substituting our own RNG.
//!
//! ## Object layout
//!
//! Like `jca::signature`, the synthetic state is appended *after* the real
//! class's declared field count (`synthetic_base_offset`) so the VM's
//! descriptor-aware `set_field` never coerces a reference write onto a slot the
//! real class declares with an incompatible type, and the GC scans the
//! appended object slots (so the stashed real SPI survives compaction across the
//! `getInstance` → `newEncapsulator` → `encapsulate` call chain).
//!
//! | Mirror class             | Slot (rel. base) | Holds                         |
//! |--------------------------|------------------|-------------------------------|
//! | `javax/crypto/KEM`       | `KEM_OFF_ALGO`   | Int algorithm index           |
//! | `javax/crypto/KEM`       | `KEM_OFF_SPI`    | real `ML_KEM_Impls$K*` SPI    |
//! | `…/KEM$Encapsulator`     | `SPI_OFF_HANDLE` | real `KEMSpi$EncapsulatorSpi` |
//! | `…/KEM$Decapsulator`     | `SPI_OFF_HANDLE` | real `KEMSpi$DecapsulatorSpi` |
//!
//! ## Gating
//!
//! Registered only when `route_pqc_to_real()` is on (default) and we are not in
//! the full `real_jca_mode()` (where the real providers handle KEM end-to-end).
//! The `CRATONVM_SYNTHETIC_PQC=1` kill-switch skips registration entirely, so
//! `KEM.getInstance` falls through to real provider resolution and fails closed
//! with `NoSuchAlgorithmException` — never a synthetic stub that silently
//! returns garbage encapsulations.

#![allow(clippy::collapsible_if)]

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ObjectRef, Value};

use crate::try_alloc_concurrent_synthetic;

// Algorithm indices (KEM-local; not shared with key_factory's table).
const KEM_MLKEM_512: i32 = 0;
const KEM_MLKEM_768: i32 = 1;
const KEM_MLKEM_1024: i32 = 2;
const KEM_MLKEM: i32 = 3; // umbrella "ML-KEM" — parameter set derived from key
/// RFC 9180 `DHKEM`, the other `KEM` service SunJCE registers and the one this
/// table did not have. Nothing about the drive below is ML-KEM-specific — the
/// SPI is constructed by name and then `engineNewEncapsulator` /
/// `engineNewDecapsulator` do the work — so serving it is an index, a name and
/// a class. It is `com.sun.crypto.provider.DHKEM`, driven over an X25519/X448
/// or EC key pair.
const KEM_DHKEM: i32 = 4;

// Synthetic-state offsets, relative to `synthetic_base_offset(...)`.
const KEM_OFF_ALGO: usize = 0;
const KEM_OFF_SPI: usize = 1;
const KEM_PRIVATE_SLOTS: usize = 2;

// Encapsulator / Decapsulator mirrors each hold a single real SPI handle.
const SPI_OFF_HANDLE: usize = 0;
const SPI_PRIVATE_SLOTS: usize = 1;

const KEM_CLASS: &str = "javax/crypto/KEM";
const ENCAPSULATOR_CLASS: &str = "javax/crypto/KEM$Encapsulator";
const DECAPSULATOR_CLASS: &str = "javax/crypto/KEM$Decapsulator";

/// Default `algorithm` string for the no-arg `encapsulate()`/`decapsulate(byte[])`
/// overloads — matches the real `KEM$Encapsulator.encapsulate()` bytecode
/// (`encapsulate(0, secretSize(), "Generic")`).
const DEFAULT_SECRET_ALG: &str = "Generic";

// ---------------------------------------------------------------------------
// Name / SPI-class tables
// ---------------------------------------------------------------------------

fn kem_algo_idx(name: &str) -> i32 {
    match name.to_ascii_uppercase().as_str() {
        "ML-KEM" => KEM_MLKEM,
        // ML-KEM OIDs (NIST PQC arc 2.16.840.1.101.3.4.4.{1,2,3}).
        "ML-KEM-512" | "2.16.840.1.101.3.4.4.1" => KEM_MLKEM_512,
        "ML-KEM-768" | "2.16.840.1.101.3.4.4.2" => KEM_MLKEM_768,
        "ML-KEM-1024" | "2.16.840.1.101.3.4.4.3" => KEM_MLKEM_1024,
        "DHKEM" => KEM_DHKEM,
        _ => -1,
    }
}

fn kem_algo_name(idx: i32) -> &'static str {
    match idx {
        KEM_MLKEM_512 => "ML-KEM-512",
        KEM_MLKEM_768 => "ML-KEM-768",
        KEM_MLKEM_1024 => "ML-KEM-1024",
        KEM_DHKEM => "DHKEM",
        _ => "ML-KEM",
    }
}

/// Real SunJCE `ML_KEM_Impls$K*` KEM-SPI class for an algorithm index. The
/// suffix mapping (2/3/5 by NIST category, generic `$K`) matches
/// `key_factory::pqc_spi_classes`, so the KEM SPI is the same provider that
/// mints the ML-KEM keys.
fn mlkem_spi_class(idx: i32) -> Option<&'static str> {
    match idx {
        KEM_MLKEM_512 => Some("com/sun/crypto/provider/ML_KEM_Impls$K2"),
        KEM_MLKEM_768 => Some("com/sun/crypto/provider/ML_KEM_Impls$K3"),
        KEM_MLKEM_1024 => Some("com/sun/crypto/provider/ML_KEM_Impls$K5"),
        KEM_MLKEM => Some("com/sun/crypto/provider/ML_KEM_Impls$K"),
        // Not an ML_KEM_Impls class, and the function name now understates
        // what it does — kept as one table because the CALLER only needs "the
        // SPI class for this index", and splitting it would duplicate the
        // construct-and-wrap that follows.
        KEM_DHKEM => Some("com/sun/crypto/provider/DHKEM"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Helpers (mirror jca::signature)
// ---------------------------------------------------------------------------

/// Offset at which our appended synthetic state begins — the real class's total
/// declared field count, so reference writes never land on a slot the real
/// layout declares with an incompatible descriptor.
/// The fifth caller of the ONE base helper, not the fifth copy of it.
///
/// This was a private re-implementation: `ensure_class_initialized`, then
/// `class_num_total_fields`, with no fabricated-stub arm. It is now a forwarder
/// to `cratonvm_native_api::appended_slots::base_for_class`, whose module header
/// names these four `native-builtins/src/jca/` copies by hand as the ones still
/// outstanding.
///
/// Two things change, and one deliberately does not.
///
///   * It stops running `<clinit>`. `base_for_class` answers an already-loaded
///     class from `class_id_by_name`, so a JCA private-slot read is no longer a
///     Java re-entry that can move every unpinned `ObjectRef` its caller holds.
///   * It gains the fabricated-stub arm, which collapses the base to 0 on a
///     stub whose fields ARE the private map.
///   * The NUMBER does not change. Measured with a paired probe that computed
///     both answers on the same call: identical on every class either mode
///     reached (real-JDK `KeyPairGenerator` 2, `Signature` 4, `KeyFactory` 5,
///     `KeyAgreement` 6, `KEM` 4; synthetic-JDK all 0), with no value ever
///     moving between calls.
fn synthetic_base_offset(ctx: &mut dyn NativeContext, class_name: &str) -> usize {
    cratonvm_native_api::appended_slots::base_for_class(ctx, class_name)
}

fn this_arg(args: &[Value]) -> Result<ObjectRef, MethodCallFailed> {
    match args.first() {
        Some(Value::Object(Some(o))) => Ok(*o),
        _ => Err(RuntimeError::NullPointerException {
            message: Some("KEM: this is null".into()),
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

fn opt_obj(args: &[Value], idx: usize) -> Option<ObjectRef> {
    match args.get(idx) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    }
}

fn int_arg(args: &[Value], idx: usize) -> i32 {
    match args.get(idx) {
        Some(Value::Int(n)) => *n,
        _ => 0,
    }
}

/// Read the real SPI handle (KEM mirror → `ML_KEM_Impls$K*`; Encapsulator /
/// Decapsulator mirror → `KEMSpi$*Spi`) out of the appended synthetic slot.
fn read_spi(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    class_name: &str,
    off: usize,
) -> Option<ObjectRef> {
    let base = synthetic_base_offset(ctx, class_name);
    match ctx.get_field(this, base + off) {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    }
}

/// Construct & throw a real `java.security.NoSuchAlgorithmException` (a
/// `GeneralSecurityException`, caught by Java callers exactly as under HotSpot).
/// Falls back to a catchable `SecurityException` if the JDK class can't be
/// built — never a silent stub.
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

fn illegal_state(msg: &str) -> MethodCallFailed {
    RuntimeError::IllegalStateException {
        message: msg.to_string(),
    }
    .into()
}

// ---------------------------------------------------------------------------
// KEM.getInstance / getAlgorithm
// ---------------------------------------------------------------------------

fn kem_get_instance(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Static method: args[0] is the algorithm name (no `this`).
    let alg = read_string(ctx, args, 0);
    // `kem_algo_idx` already spells the three ML-KEM OIDs bare; HotSpot
    // registers each one TWICE, bare and `OID.`-prefixed, and the prefixed
    // spelling is what the three measured rows use. Resolve through the
    // registry rather than growing the match arm a second time.
    let alg = crate::jca::provider_chain::canonical_if_unrecognised(
        crate::jca::provider_chain::provider_arg_name(ctx, args, 1).as_deref(),
        "KEM",
        &alg,
        &|name| mlkem_spi_class(kem_algo_idx(name)).is_some(),
    )
    .unwrap_or(alg);
    let idx = kem_algo_idx(&alg);
    let spi_class = match mlkem_spi_class(idx) {
        Some(c) => c,
        None => {
            return Err(throw_no_such_algorithm(
                ctx,
                &format!("{alg} KEM not available"),
            ))
        }
    };
    // Construct the real ML_KEM_Impls$K* SPI (public no-arg ctor). Fail closed
    // with NoSuchAlgorithmException when the provider class is absent (e.g. a
    // pre-FIPS-203 JDK) — honest gap, never a stub.
    let spi = match ctx.new_object_initialized(spi_class, "()V", &[]) {
        Ok(Some(Value::Object(Some(o)))) => o,
        Ok(_) => {
            return Err(throw_no_such_algorithm(
                ctx,
                &format!("{alg} provider {spi_class} unavailable"),
            ))
        }
        Err(e) => return Err(e),
    };
    // Pin the SPI across the mirror allocation (which may GC).
    let spi_pin = ctx.pin_native_root(spi);
    let base = synthetic_base_offset(ctx, KEM_CLASS);
    let obj = try_alloc_concurrent_synthetic(ctx, KEM_CLASS, base + KEM_PRIVATE_SLOTS)?;
    let spi = ctx.read_native_pin(spi_pin, spi);
    ctx.set_field(obj, base + KEM_OFF_ALGO, Value::Int(idx));
    ctx.set_field(obj, base + KEM_OFF_SPI, Value::Object(Some(spi)));
    ctx.unpin_native_roots(spi_pin);
    Ok(Some(Value::Object(Some(obj))))
}

fn kem_get_algorithm(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let base = synthetic_base_offset(ctx, KEM_CLASS);
    let idx = match ctx.get_field(this, base + KEM_OFF_ALGO) {
        Value::Int(i) => i,
        _ => KEM_MLKEM,
    };
    let s = ctx.create_string(kem_algo_name(idx));
    Ok(Some(Value::Object(Some(s))))
}

// ---------------------------------------------------------------------------
// KEM.newEncapsulator / newDecapsulator
// ---------------------------------------------------------------------------

/// Shared body: drive `spi.engine{New}{Encapsulator,Decapsulator}` and wrap the
/// returned `KEMSpi$*Spi` in a synthetic `KEM$Encapsulator`/`$Decapsulator`
/// mirror. `engine_method`/`engine_desc` select the direction; `mirror_class`
/// is the result wrapper. `pk_or_sk` is the public/private key; `spec`/`sr` are
/// the (possibly null) parameter spec / SecureRandom passed straight through —
/// the real `NamedKEM` rejects a non-null spec and defaults a null SecureRandom.
#[allow(clippy::too_many_arguments)]
fn drive_new_consumer(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    engine_method: &str,
    engine_desc: &str,
    mirror_class: &str,
    call_args: &[Value],
) -> MethodCallResult {
    let spi = read_spi(ctx, this, KEM_CLASS, KEM_OFF_SPI)
        .ok_or_else(|| illegal_state("KEM not initialized (no SPI)"))?;
    // engine* args (key, [spec], [sr]) are roots for the call; `spi` is the
    // receiver-root. A thrown InvalidKey/InvalidAlgorithmParameterException
    // propagates as the real Java exception (correct fail-closed).
    let consumer_spi = match ctx.invoke_virtual(spi, engine_method, engine_desc, call_args)? {
        Some(Value::Object(Some(o))) => o,
        _ => return Err(illegal_state("KEM SPI returned a null consumer")),
    };
    // Pin the returned SPI across the mirror allocation.
    let pin = ctx.pin_native_root(consumer_spi);
    let base = synthetic_base_offset(ctx, mirror_class);
    let mirror = try_alloc_concurrent_synthetic(ctx, mirror_class, base + SPI_PRIVATE_SLOTS)?;
    let consumer_spi = ctx.read_native_pin(pin, consumer_spi);
    ctx.set_field(
        mirror,
        base + SPI_OFF_HANDLE,
        Value::Object(Some(consumer_spi)),
    );
    ctx.unpin_native_roots(pin);
    Ok(Some(Value::Object(Some(mirror))))
}

const NEW_ENCAPS_DESC: &str =
    "(Ljava/security/PublicKey;Ljava/security/spec/AlgorithmParameterSpec;Ljava/security/SecureRandom;)Ljavax/crypto/KEMSpi$EncapsulatorSpi;";
const NEW_DECAPS_DESC: &str =
    "(Ljava/security/PrivateKey;Ljava/security/spec/AlgorithmParameterSpec;)Ljavax/crypto/KEMSpi$DecapsulatorSpi;";

// newEncapsulator(PublicKey) → spec = null, sr = null
fn kem_new_encapsulator_pk(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let pk = Value::Object(opt_obj(args, 1));
    drive_new_consumer(
        ctx,
        this,
        "engineNewEncapsulator",
        NEW_ENCAPS_DESC,
        ENCAPSULATOR_CLASS,
        &[pk, Value::Object(None), Value::Object(None)],
    )
}

// newEncapsulator(PublicKey, SecureRandom) → spec = null
fn kem_new_encapsulator_pk_sr(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let pk = Value::Object(opt_obj(args, 1));
    let sr = Value::Object(opt_obj(args, 2));
    drive_new_consumer(
        ctx,
        this,
        "engineNewEncapsulator",
        NEW_ENCAPS_DESC,
        ENCAPSULATOR_CLASS,
        &[pk, Value::Object(None), sr],
    )
}

// newEncapsulator(PublicKey, AlgorithmParameterSpec, SecureRandom)
fn kem_new_encapsulator_pk_spec_sr(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = this_arg(args)?;
    let pk = Value::Object(opt_obj(args, 1));
    let spec = Value::Object(opt_obj(args, 2));
    let sr = Value::Object(opt_obj(args, 3));
    drive_new_consumer(
        ctx,
        this,
        "engineNewEncapsulator",
        NEW_ENCAPS_DESC,
        ENCAPSULATOR_CLASS,
        &[pk, spec, sr],
    )
}

// newDecapsulator(PrivateKey) → spec = null
fn kem_new_decapsulator_sk(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let sk = Value::Object(opt_obj(args, 1));
    drive_new_consumer(
        ctx,
        this,
        "engineNewDecapsulator",
        NEW_DECAPS_DESC,
        DECAPSULATOR_CLASS,
        &[sk, Value::Object(None)],
    )
}

// newDecapsulator(PrivateKey, AlgorithmParameterSpec)
fn kem_new_decapsulator_sk_spec(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let sk = Value::Object(opt_obj(args, 1));
    let spec = Value::Object(opt_obj(args, 2));
    drive_new_consumer(
        ctx,
        this,
        "engineNewDecapsulator",
        NEW_DECAPS_DESC,
        DECAPSULATOR_CLASS,
        &[sk, spec],
    )
}

// ---------------------------------------------------------------------------
// Encapsulator: encapsulate / secretSize / encapsulationSize
// ---------------------------------------------------------------------------

fn enc_secret_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let spi = read_spi(ctx, this, ENCAPSULATOR_CLASS, SPI_OFF_HANDLE)
        .ok_or_else(|| illegal_state("Encapsulator not initialized"))?;
    ctx.invoke_virtual(spi, "engineSecretSize", "()I", &[])
}

fn enc_encapsulation_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let spi = read_spi(ctx, this, ENCAPSULATOR_CLASS, SPI_OFF_HANDLE)
        .ok_or_else(|| illegal_state("Encapsulator not initialized"))?;
    ctx.invoke_virtual(spi, "engineEncapsulationSize", "()I", &[])
}

// encapsulate() → engineEncapsulate(0, secretSize(), "Generic")
fn enc_encapsulate_default(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let spi = read_spi(ctx, this, ENCAPSULATOR_CLASS, SPI_OFF_HANDLE)
        .ok_or_else(|| illegal_state("Encapsulator not initialized"))?;
    let pin = ctx.pin_native_root(spi);
    // secretSize first (spi is the receiver-root of the call).
    let spi = ctx.read_native_pin(pin, spi);
    let ss = match ctx.invoke_virtual(spi, "engineSecretSize", "()I", &[])? {
        Some(Value::Int(n)) => n,
        _ => 0,
    };
    // create_string may GC — re-read spi via the pin afterwards.
    let alg = ctx.create_string(DEFAULT_SECRET_ALG);
    let spi = ctx.read_native_pin(pin, spi);
    let res = ctx.invoke_virtual(
        spi,
        "engineEncapsulate",
        "(IILjava/lang/String;)Ljavax/crypto/KEM$Encapsulated;",
        &[Value::Int(0), Value::Int(ss), Value::Object(Some(alg))],
    );
    ctx.unpin_native_roots(pin);
    res
}

// encapsulate(int from, int to, String algorithm)
fn enc_encapsulate_range(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let spi = read_spi(ctx, this, ENCAPSULATOR_CLASS, SPI_OFF_HANDLE)
        .ok_or_else(|| illegal_state("Encapsulator not initialized"))?;
    let from = int_arg(args, 1);
    let to = int_arg(args, 2);
    let alg = Value::Object(opt_obj(args, 3));
    ctx.invoke_virtual(
        spi,
        "engineEncapsulate",
        "(IILjava/lang/String;)Ljavax/crypto/KEM$Encapsulated;",
        &[Value::Int(from), Value::Int(to), alg],
    )
}

// ---------------------------------------------------------------------------
// Decapsulator: decapsulate / secretSize / encapsulationSize
// ---------------------------------------------------------------------------

fn dec_secret_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let spi = read_spi(ctx, this, DECAPSULATOR_CLASS, SPI_OFF_HANDLE)
        .ok_or_else(|| illegal_state("Decapsulator not initialized"))?;
    ctx.invoke_virtual(spi, "engineSecretSize", "()I", &[])
}

fn dec_encapsulation_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let spi = read_spi(ctx, this, DECAPSULATOR_CLASS, SPI_OFF_HANDLE)
        .ok_or_else(|| illegal_state("Decapsulator not initialized"))?;
    ctx.invoke_virtual(spi, "engineEncapsulationSize", "()I", &[])
}

// decapsulate(byte[] encapsulation) → engineDecapsulate(enc, 0, secretSize(), "Generic")
fn dec_decapsulate_default(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let spi = read_spi(ctx, this, DECAPSULATOR_CLASS, SPI_OFF_HANDLE)
        .ok_or_else(|| illegal_state("Decapsulator not initialized"))?;
    let enc = match opt_obj(args, 1) {
        Some(o) => o,
        None => {
            return Err(RuntimeError::NullPointerException {
                message: Some("decapsulate: encapsulation is null".into()),
            }
            .into())
        }
    };
    // Pin both the SPI and the caller's encapsulation array across the
    // intervening invokes / string alloc (any of which may relocate them).
    let spi_pin = ctx.pin_native_root(spi);
    let enc_pin = ctx.pin_native_root(enc);
    let spi = ctx.read_native_pin(spi_pin, spi);
    let ss = match ctx.invoke_virtual(spi, "engineSecretSize", "()I", &[])? {
        Some(Value::Int(n)) => n,
        _ => 0,
    };
    let alg = ctx.create_string(DEFAULT_SECRET_ALG);
    let spi = ctx.read_native_pin(spi_pin, spi);
    let enc = ctx.read_native_pin(enc_pin, enc);
    let res = ctx.invoke_virtual(
        spi,
        "engineDecapsulate",
        "([BIILjava/lang/String;)Ljavax/crypto/SecretKey;",
        &[
            Value::Object(Some(enc)),
            Value::Int(0),
            Value::Int(ss),
            Value::Object(Some(alg)),
        ],
    );
    ctx.unpin_native_roots(enc_pin);
    ctx.unpin_native_roots(spi_pin);
    res
}

// decapsulate(byte[] encapsulation, int from, int to, String algorithm)
fn dec_decapsulate_range(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let spi = read_spi(ctx, this, DECAPSULATOR_CLASS, SPI_OFF_HANDLE)
        .ok_or_else(|| illegal_state("Decapsulator not initialized"))?;
    let enc = Value::Object(opt_obj(args, 1));
    let from = int_arg(args, 2);
    let to = int_arg(args, 3);
    let alg = Value::Object(opt_obj(args, 4));
    ctx.invoke_virtual(
        spi,
        "engineDecapsulate",
        "([BIILjava/lang/String;)Ljavax/crypto/SecretKey;",
        &[enc, Value::Int(from), Value::Int(to), alg],
    )
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register(r: &mut NativeMethodRegistry) {
    // In the full real-JCA mode the real SunJCE provider resolves ML-KEM via
    // its own Security/Provider chain — leave KEM entirely to it.
    if crate::real_jca_mode() {
        return;
    }
    // Kill-switch / opt-out: without PQC routing, do not intercept KEM at all,
    // so `KEM.getInstance` falls through to (synthetic) provider resolution and
    // fails closed with NoSuchAlgorithmException rather than a stub.
    if !crate::route_pqc_to_real() {
        return;
    }

    let prev = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);

    // --- javax.crypto.KEM ---
    r.register(
        KEM_CLASS,
        "getInstance",
        "(Ljava/lang/String;)Ljavax/crypto/KEM;",
        kem_get_instance,
    );
    r.register(
        KEM_CLASS,
        "getInstance",
        "(Ljava/lang/String;Ljava/security/Provider;)Ljavax/crypto/KEM;",
        kem_get_instance,
    );
    r.register(
        KEM_CLASS,
        "getInstance",
        "(Ljava/lang/String;Ljava/lang/String;)Ljavax/crypto/KEM;",
        kem_get_instance,
    );
    r.register(
        KEM_CLASS,
        "getAlgorithm",
        "()Ljava/lang/String;",
        kem_get_algorithm,
    );
    r.register(
        KEM_CLASS,
        "newEncapsulator",
        "(Ljava/security/PublicKey;)Ljavax/crypto/KEM$Encapsulator;",
        kem_new_encapsulator_pk,
    );
    r.register(
        KEM_CLASS,
        "newEncapsulator",
        "(Ljava/security/PublicKey;Ljava/security/SecureRandom;)Ljavax/crypto/KEM$Encapsulator;",
        kem_new_encapsulator_pk_sr,
    );
    r.register(
        KEM_CLASS,
        "newEncapsulator",
        "(Ljava/security/PublicKey;Ljava/security/spec/AlgorithmParameterSpec;Ljava/security/SecureRandom;)Ljavax/crypto/KEM$Encapsulator;",
        kem_new_encapsulator_pk_spec_sr,
    );
    r.register(
        KEM_CLASS,
        "newDecapsulator",
        "(Ljava/security/PrivateKey;)Ljavax/crypto/KEM$Decapsulator;",
        kem_new_decapsulator_sk,
    );
    r.register(
        KEM_CLASS,
        "newDecapsulator",
        "(Ljava/security/PrivateKey;Ljava/security/spec/AlgorithmParameterSpec;)Ljavax/crypto/KEM$Decapsulator;",
        kem_new_decapsulator_sk_spec,
    );

    // --- javax.crypto.KEM$Encapsulator ---
    r.register(
        ENCAPSULATOR_CLASS,
        "encapsulate",
        "()Ljavax/crypto/KEM$Encapsulated;",
        enc_encapsulate_default,
    );
    r.register(
        ENCAPSULATOR_CLASS,
        "encapsulate",
        "(IILjava/lang/String;)Ljavax/crypto/KEM$Encapsulated;",
        enc_encapsulate_range,
    );
    r.register(ENCAPSULATOR_CLASS, "secretSize", "()I", enc_secret_size);
    r.register(
        ENCAPSULATOR_CLASS,
        "encapsulationSize",
        "()I",
        enc_encapsulation_size,
    );

    // --- javax.crypto.KEM$Decapsulator ---
    r.register(
        DECAPSULATOR_CLASS,
        "decapsulate",
        "([B)Ljavax/crypto/SecretKey;",
        dec_decapsulate_default,
    );
    r.register(
        DECAPSULATOR_CLASS,
        "decapsulate",
        "([BIILjava/lang/String;)Ljavax/crypto/SecretKey;",
        dec_decapsulate_range,
    );
    r.register(DECAPSULATOR_CLASS, "secretSize", "()I", dec_secret_size);
    r.register(
        DECAPSULATOR_CLASS,
        "encapsulationSize",
        "()I",
        dec_encapsulation_size,
    );

    r.set_category(prev);
}
