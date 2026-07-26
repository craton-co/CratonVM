// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP6.3 — `javax.crypto.Cipher` real-JDK class-init shim.
//!
//! ## Why this module exists
//!
//! `phases_early.rs::register_phase53_crypto` already registers a complete
//! native dispatch for `javax.crypto.Cipher.getInstance` / `init` / `update`
//! / `updateAAD` / `doFinal` against the real-AES/GCM/CBC/CTR/ChaCha20-
//! Poly1305 backend in the same file (`cipher_do_final`,
//! lines 8625-8800).  The dispatch covers the full scope of the WP6.3
//! probe (`AES/GCM/NoPadding`, encrypt + decrypt round-trip).
//!
//! What the dispatch does *not* fix is the fact that the real-JDK
//! `javax/crypto/Cipher` class still has a non-trivial `<clinit>` that
//! triggers `sun.security.util.Debug.getInstance("jca", "Cipher")`, which
//! in turn forces `java.security.Security.<clinit>` → `SecPropLoader.
//! loadAll()` → `FileInputStream` over `${java.home}/conf/security/
//! java.security`.  In our boot-strap that read currently surfaces as
//! `IOException("Is a directory")` and is rethrown as
//! `InternalError("Error loading java.security file")`, blowing up the
//! probe before native dispatch ever gets a chance to fire on
//! `Cipher.getInstance`.
//!
//! The fix here is targeted and minimal: register **`<clinit>` no-op
//! intercepts** for the chain of real-JDK classes that would otherwise
//! force the `Security` properties read.  Once those classes are marked
//! initialized without running their bytecode, the existing native
//! `Cipher.getInstance` intercept in `phases_early.rs` allocates a
//! synthetic-shaped `Cipher` instance (via `alloc_concurrent_synthetic`,
//! which already widens to the larger of synthetic-field-count vs the
//! loaded class's instance-field count, so field-index OOB is impossible)
//! and routes `init` / `doFinal` straight to `cipher_do_final` →
//! `aes_gcm_encrypt` / `aes_gcm_decrypt`.
//!
//! This is a `<clinit>` shim only — every other Cipher method
//! (getInstance, init, doFinal, etc.) is registered in
//! `phases_early.rs::register_phase53_crypto` and is intentionally
//! NOT duplicated here.  Duplicating would either be ignored (same
//! triple → same callback overwrites itself idempotently) or, if we
//! diverged, would create dueling dispatch.
//!
//! ## Probe (apps/cipher_probe/CipherProbe.java)
//!
//! ```java
//! import javax.crypto.*;
//! import javax.crypto.spec.*;
//! import java.security.SecureRandom;
//! public class CipherProbe {
//!     public static void main(String[] a) throws Exception {
//!         byte[] key = new byte[32];
//!         byte[] iv  = new byte[12];
//!         SecureRandom r = new SecureRandom();
//!         r.nextBytes(key); r.nextBytes(iv);
//!         SecretKeySpec ks = new SecretKeySpec(key, "AES");
//!         GCMParameterSpec gcm = new GCMParameterSpec(128, iv);
//!
//!         Cipher c = Cipher.getInstance("AES/GCM/NoPadding");
//!         c.init(Cipher.ENCRYPT_MODE, ks, gcm);
//!         byte[] ct = c.doFinal("hello aes-gcm".getBytes("UTF-8"));
//!
//!         Cipher d = Cipher.getInstance("AES/GCM/NoPadding");
//!         d.init(Cipher.DECRYPT_MODE, ks, gcm);
//!         byte[] pt = d.doFinal(ct);
//!         if (!"hello aes-gcm".equals(new String(pt, "UTF-8"))) {
//!             System.out.println("FAIL"); System.exit(1);
//!         }
//!         System.out.println("OK");
//!     }
//! }
//! ```
//!
//! End-to-end the probe must print `OK` on stdout and exit 0 with the
//! shim registered.

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallResult, RuntimeError};
use cratonvm_types::{ObjectRef, Value};

// Round-9 MED-2: migrated `CIPHER_TABLE` from `std::sync::RwLock` to
// `parking_lot::RwLock` — removes the per-access `unwrap_or_else(into_inner)`
// poison dance and yields a smaller, faster lock.
use parking_lot::RwLock;
use rustc_hash::FxHashMap;

use crate::crypto_impl::{Aes, AesGcm};
use crate::phases_early::CIPHER_IV;
use crate::{alloc_concurrent_synthetic, obj_arg};

// ---------------------------------------------------------------------------
// Cipher state — kept in a process-wide side-table keyed on the
// `identity_hash_code` of the Cipher synthetic.  We CANNOT reuse the
// synthetic field-index pattern from `phases_early.rs::register_phase53_crypto`
// in real-JDK mode: when `javax.crypto.Cipher` is loaded as a real
// class, instance-field 5 maps to `initialized:Z` (a primitive
// boolean), so storing `Value::Object(Some(byte_arr))` there throws
// `expected object reference, got double` on read-back.  The
// side-table keeps the storage independent of the real-JDK class
// layout — it works in both modes and is cheap (single
// `RwLock<FxHashMap<i32, CipherState>>` lookup; no allocation per
// call).
//
// Round-13 C13 fix: keys are GC-stable identity hash codes (see
// `gc/src/compact_header.rs::HashCodeTable::update_after_gc`), not raw
// heap pointers.  Previously this table was keyed by `obj.as_ptr() as
// usize` — when the GC relocated a live Cipher during compaction, the
// stored `mode`/`key_bytes`/`iv_bytes`/`accumulated`/`aad` became
// orphaned and the next `cipher_do_final` saw `mode == 0` and silently
// returned a NULL ciphertext (apparent "success" producing wrong data).
// Routing every read/write through `NativeContext::identity_hash_code`
// means GC compaction no longer corrupts the table.
//
// Memory note: the table still grows monotonically until VM shutdown
// (no weak-ref machinery hooked up yet) — fine for finite-lived JVM
// processes; not suitable for a long-running daemon doing millions of
// Cipher instances per minute.
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct CipherState {
    /// Algorithm transformation string, e.g. `"AES/GCM/NoPadding"`.
    algorithm: String,
    /// Operation mode — 0 = uninit, 1 = ENCRYPT, 2 = DECRYPT,
    /// 3 = WRAP, 4 = UNWRAP.
    mode: i32,
    /// Raw key bytes harvested from `SecretKeySpec.getEncoded()` at
    /// init time.  We snapshot the bytes (not the ref) so the side-
    /// table never holds a heap reference, sidestepping any GC race.
    key_bytes: Vec<u8>,
    /// IV bytes copied out of the spec object at init time.
    iv_bytes: Vec<u8>,
    /// Accumulator for `update(byte[])` calls, drained on `doFinal`.
    accumulated: Vec<u8>,
    /// AEAD additional-authenticated-data accumulator, drained on
    /// `doFinal`.
    aad: Vec<u8>,
    /// RSA modulus magnitude (big-endian), captured at `init` time when the
    /// transformation is an `RSA/...` cipher. Empty for non-RSA ciphers. We
    /// snapshot the raw components (not the Key ref) so the side-table holds no
    /// heap reference — identical GC-safety rationale to `key_bytes`.
    rsa_n: Vec<u8>,
    /// RSA exponent magnitude (big-endian) appropriate to the `init` mode — the
    /// public exponent for ENCRYPT/WRAP, the private exponent for DECRYPT/UNWRAP.
    /// Empty for non-RSA ciphers.
    rsa_exp: Vec<u8>,
    /// PBES2 (`PBEWithHmacSHA*AndAES_*`) salt, captured at `init` time from the
    /// `AlgorithmParameters` argument. Empty for non-PBES2 ciphers. Plain bytes
    /// (not an `ObjectRef`) for the same GC-safety reason as `key_bytes` — and
    /// so `Cipher.getParameters()` can rebuild a fresh `AlgorithmParameters`
    /// on demand without holding a heap reference across calls.
    pbe_salt: Vec<u8>,
    /// PBES2 iteration count, paired with `pbe_salt`. Zero for non-PBES2.
    pbe_iterations: u32,
}

static CIPHER_TABLE: RwLock<Option<FxHashMap<i32, CipherState>>> = RwLock::new(None);

fn with_table_write<R>(f: impl FnOnce(&mut FxHashMap<i32, CipherState>) -> R) -> R {
    // Round-9 MED-2: parking_lot — no poison handling.
    let mut g = CIPHER_TABLE.write();
    if g.is_none() {
        *g = Some(FxHashMap::default());
    }
    f(g.as_mut().expect("table just initialised"))
}

fn with_table_read<R>(f: impl FnOnce(&FxHashMap<i32, CipherState>) -> R) -> R {
    // Round-9 MED-2: parking_lot — no poison handling.
    let g = CIPHER_TABLE.read();
    match g.as_ref() {
        Some(t) => f(t),
        None => f(&FxHashMap::default()),
    }
}

/// GC-stable key for the side-table.  Routes through
/// `NativeContext::identity_hash_code` so the entry survives moving-GC
/// compaction; the underlying `HashCodeTable::update_after_gc` remaps
/// codes whenever objects are relocated.  See the module-level Round-13
/// C13 note for why this matters.
fn obj_key(ctx: &mut dyn NativeContext, obj: ObjectRef) -> i32 {
    ctx.identity_hash_code(obj)
}

/// `<clinit>` no-op — used to mark a real-JDK class as initialized
/// without executing its static initializer bytecode.  Equivalent in
/// effect to `lib.rs::native_noop`, restated here so the file is
/// self-contained and the call sites read cleanly.
#[inline]
fn clinit_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

/// Real-JCA `java/security/Security.<clinit>` replacement.
///
/// The real `<clinit>` fails in `initialize()` (it tries to read the
/// `java.security` config file and dies with "Is a directory"), which is why
/// the default build no-ops it.  But the real EC keygen path
/// (`AlgorithmParameters.getInstance("EC")` → `Security.getImpl` →
/// `Security.getSpiClass`) dereferences the static `spiMap` field, which the
/// no-op leaves null → "Cannot invoke get on null".  Here we set `spiMap` to a
/// fresh empty `ConcurrentHashMap` (exactly what the real `<clinit>` assigns to
/// it) and skip the failing `initialize()`.  With a non-null but empty map,
/// `getSpiClass(type)` finds no entry and falls through to building the SPI
/// class name (`"java.security." + type + "Spi"`) by reflection — the correct
/// result for AlgorithmParameters/KeyFactory/KeyPairGenerator/Signature.
fn security_clinit_spimap(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    if let Some(Value::Object(Some(m))) =
        ctx.new_object_initialized("java/util/concurrent/ConcurrentHashMap", "()V", &[])?
    {
        ctx.set_static_field_by_name("java/security/Security", "spiMap", Value::Object(Some(m)));
    }
    Ok(None)
}

/// `Provider.getEngineName(String)` override — bypasses the `knownEngines`
/// HashMap lookup that real-JDK `Provider.<clinit>` populates.  Because we
/// no-op the clinit (see `register_cipher_clinit_shim`), the static field
/// stays null and the real implementation NPEs at `knownEngines.get(name)`.
///
/// The OpenJDK 25 fallback when `knownEngines.get` returns null is
/// `return name` unchanged, so this override returns the input string
/// directly — spec-equivalent for any caller that does not rely on
/// canonical-case rewriting (BouncyCastle's `BouncyCastleProvider`
/// register-loop calls `Provider.put("MessageDigest.SHA-256", ...)` which
/// internally calls `getEngineName("MessageDigest")` — pass-through is
/// correct).
///
/// This was discovered when `BcProbe` triggered:
///   `InternalError: cannot create instance of GOST3411$Mappings :
///    NullPointerException @ Provider.getEngineName pc=9`
#[allow(dead_code)]
fn provider_get_engine_name(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Static method — args[0] is the String parameter (no `this`).
    // Retained as a fallback in case real-JDK Provider.<clinit> is ever
    // re-disabled; not currently registered.
    match args.first() {
        Some(Value::Object(opt @ Some(_))) => Ok(Some(Value::Object(*opt))),
        _ => Ok(Some(Value::Object(None))),
    }
}

/// Read a JVM byte[] (stored as i32 with i8-bit cast) into a Vec<u8>.
fn read_bytes(ctx: &mut dyn NativeContext, arr: ObjectRef) -> Vec<u8> {
    let len = ctx.array_length(arr);
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        if let Value::Int(b) = ctx.get_array_element(arr, i) {
            out.push(b as u8);
        }
    }
    out
}

/// Build a JVM `byte[]` from a `&[u8]` (the inverse of [`read_bytes`]).
fn make_bytes_array(ctx: &mut dyn NativeContext, bytes: &[u8]) -> ObjectRef {
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
    for (i, &b) in bytes.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
    }
    arr
}

/// `true` when the transformation names the RSA cipher (`"RSA/ECB/…"`).
fn is_rsa_transformation(algo: &str) -> bool {
    algo.split('/')
        .next()
        .map(|c| c.eq_ignore_ascii_case("RSA"))
        .unwrap_or(false)
}

/// PKCS#12 PBES2 AES transformations (`PBEWithHmacSHA{1,224,256}AndAES_{128,256}`
/// — the default keystore/key-protection algorithms since JDK 8u191): maps the
/// transformation name to `(PBKDF2 PRF code, AES key length in bytes)`. SunJCE's
/// `PBEKeyFactory` for these names returns a `PBEKey` holding the RAW password
/// bytes (no derivation — see `phases_early::is_known_pbe_keyfactory_alg`); the
/// actual AES key is derived via PBKDF2 from those password bytes + the
/// `AlgorithmParameters`' embedded salt/iterationCount only at `Cipher.init`
/// time. Limited to the SHA1/224/256 (64-byte-block) PRFs `pbkdf2_derive_for`
/// supports — SHA384/512/512_224/512_256 variants aren't PKCS12's default and
/// fall through to the (pre-existing, unchanged) generic path.
fn pbes2_aes_params(algo: &str) -> Option<(i32, usize)> {
    match algo {
        "PBEWithHmacSHA1AndAES_128" => Some((1, 16)),
        "PBEWithHmacSHA1AndAES_256" => Some((1, 32)),
        "PBEWithHmacSHA224AndAES_128" => Some((224, 16)),
        "PBEWithHmacSHA224AndAES_256" => Some((224, 32)),
        "PBEWithHmacSHA256AndAES_128" => Some((256, 16)),
        "PBEWithHmacSHA256AndAES_256" => Some((256, 32)),
        _ => None,
    }
}

/// Extract `(salt, iterationCount, iv)` from a real `java.security.
/// AlgorithmParameters` wrapping a `com.sun.crypto.provider.
/// PBES2Parameters$*` SPI (the shape `provider_chain::seed_sunjce_pbe_services`
/// wires `AlgorithmParameters.getInstance("PBEWithHmacSHA*AndAES_*")` to).
/// `iv` is the (possibly auto-generated by `PBES2Parameters.engineInit`)
/// block-cipher IV stored in the SPI's `cipherParam` field as an
/// `IvParameterSpec`. Returns `None` if any field can't be resolved/read —
/// e.g. a foreign or not-yet-`init`ed `AlgorithmParameters` — so the caller
/// falls back to the pre-existing (raw-password-as-key) behavior rather than
/// deriving from garbage.
fn pbes2_extract_params(
    ctx: &mut dyn NativeContext,
    alg_params: ObjectRef,
) -> Option<(Vec<u8>, u32, Vec<u8>)> {
    let spi_slot = ctx.resolve_field_index("java/security/AlgorithmParameters", "paramSpi")?;
    let spi = match ctx.get_field(alg_params, spi_slot) {
        Value::Object(Some(o)) => o,
        _ => return None,
    };
    let salt_slot = ctx.resolve_field_index("com/sun/crypto/provider/PBES2Parameters", "salt")?;
    let icount_slot =
        ctx.resolve_field_index("com/sun/crypto/provider/PBES2Parameters", "iCount")?;
    let cipher_param_slot =
        ctx.resolve_field_index("com/sun/crypto/provider/PBES2Parameters", "cipherParam")?;
    let salt = match ctx.get_field(spi, salt_slot) {
        Value::Object(Some(arr)) => read_bytes(ctx, arr),
        _ => return None,
    };
    let icount = match ctx.get_field(spi, icount_slot) {
        Value::Int(i) if i > 0 => i as u32,
        _ => return None,
    };
    let iv = match ctx.get_field(spi, cipher_param_slot) {
        Value::Object(Some(ivps)) => extract_iv_bytes(ctx, ivps),
        _ => Vec::new(),
    };
    Some((salt, icount, iv))
}

/// Invoke `key.method()` → `BigInteger`, then `BigInteger.toByteArray()` → the
/// raw two's-complement big-endian magnitude. Returns `None` if either virtual
/// call fails (e.g. a synthetic key with no `getModulus()` behaviour). The
/// `BigInteger` is pinned across the `toByteArray()` call so a moving GC can't
/// leave it stale.
fn call_biginteger_bytes(
    ctx: &mut dyn NativeContext,
    key: ObjectRef,
    method: &str,
) -> Option<Vec<u8>> {
    let bi = match ctx.invoke_virtual(key, method, "()Ljava/math/BigInteger;", &[]) {
        Ok(Some(Value::Object(Some(o)))) => o,
        _ => return None,
    };
    let pin = ctx.pin_native_root(bi);
    let bi = ctx.read_native_pin(pin, bi);
    let arr = ctx.invoke_virtual(bi, "toByteArray", "()[B", &[]);
    ctx.unpin_native_roots(pin);
    match arr {
        Ok(Some(Value::Object(Some(a)))) => Some(read_bytes(ctx, a)),
        _ => None,
    }
}

/// Extract the RSA `(modulus, exponent)` magnitudes from a `Key` at init time.
///
/// Works for BOTH key flavours: genuine `sun.security.rsa.RSAPublic/PrivateKeyImpl`
/// (the default `route_rsa_to_real` path) expose `getModulus()` /
/// `get{Public,Private}Exponent()`; the bare synthetic keys
/// (`CRATONVM_SYNTHETIC_RSA=1`) carry a `crypto_impl` `key_id` (slot 3 or the
/// GC-stable identity bridge) from which the components are recovered. The
/// exponent matches the `init` mode — private exponent for DECRYPT(2)/UNWRAP(4),
/// public exponent otherwise.
fn rsa_key_components(
    ctx: &mut dyn NativeContext,
    key: ObjectRef,
    mode: i32,
) -> Option<(Vec<u8>, Vec<u8>)> {
    let want_private = mode == 2 || mode == 4;
    let exp_method = if want_private {
        "getPrivateExponent"
    } else {
        "getPublicExponent"
    };
    // Real RSA keys — pin the key across the two virtual calls.
    let pin = ctx.pin_native_root(key);
    let key_r = ctx.read_native_pin(pin, key);
    let n = call_biginteger_bytes(ctx, key_r, "getModulus");
    let key_r = ctx.read_native_pin(pin, key);
    let e = call_biginteger_bytes(ctx, key_r, exp_method);
    ctx.unpin_native_roots(pin);
    if let (Some(n), Some(e)) = (n, e) {
        if !n.is_empty() && !e.is_empty() {
            return Some((n, e));
        }
    }
    // Synthetic keys — resolve via the crypto_impl key_id.
    let id = crate::crypto_impl::rsa_realkey_map_get(ctx.identity_hash_code(key))
        .or_else(|| match ctx.get_field(key, 3) {
            Value::Long(i) => Some(i as u64),
            Value::Int(i) => Some(i as u64),
            _ => None,
        })
        .filter(|&i| i != 0)?;
    if want_private {
        crate::crypto_impl::rsa_key_get_priv(id)
    } else {
        crate::crypto_impl::rsa_key_get_pub(id)
    }
}

/// Shared `Cipher.init` recorder: snapshot mode + key bytes + IV, and (for RSA
/// transformations) the key's modulus/exponent components, into the side-table.
fn cipher_init_record(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    mode: i32,
    key: ObjectRef,
    iv_bytes: Vec<u8>,
) -> MethodCallResult {
    let key_bytes = extract_key_bytes(ctx, key);
    let tkey = obj_key(ctx, this);
    let algo = with_table_read(|t| {
        t.get(&tkey)
            .map(|s| s.algorithm.clone())
            .unwrap_or_default()
    });
    let (rsa_n, rsa_exp) = if is_rsa_transformation(&algo) {
        rsa_key_components(ctx, key, mode).unwrap_or_default()
    } else {
        (Vec::new(), Vec::new())
    };
    with_table_write(|t| {
        let s = t.entry(tkey).or_default();
        s.mode = mode;
        s.key_bytes = key_bytes;
        s.iv_bytes = iv_bytes;
        s.rsa_n = rsa_n;
        s.rsa_exp = rsa_exp;
        s.accumulated.clear();
        s.aad.clear();
    });
    Ok(None)
}

/// `Cipher.init(mode, key, AlgorithmParameters)` for a PKCS#12 PBES2 AES
/// transformation (`PBEWithHmacSHA*AndAES_*`): derive the real AES key via
/// PBKDF2 from the raw-password `PBEKey` bytes + the `AlgorithmParameters`'
/// embedded salt/iterationCount, and the block-cipher IV from its embedded
/// `cipherParam`, then record those DERIVED bytes exactly like a plain
/// `AES/CBC` cipher would (`cipher_do_final_impl`'s routing table recognises
/// this transformation name and forwards to the real `AESCipher$General`
/// SPI). Falls back to the generic (raw-key-bytes) `cipher_init_record` when
/// the transformation isn't a recognised PBES2-AES name or the params object
/// isn't shaped as expected — e.g. a null `AlgorithmParameters` (some callers
/// pass null and expect the cipher to self-generate on first `doFinal`,
/// which we don't support; behaves as before, not worse).
fn cipher_init_record_pbes2(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    mode: i32,
    key: ObjectRef,
    alg_params: Option<ObjectRef>,
) -> MethodCallResult {
    let tkey = obj_key(ctx, this);
    let algo = with_table_read(|t| {
        t.get(&tkey)
            .map(|s| s.algorithm.clone())
            .unwrap_or_default()
    });
    if let (Some((prf, keylen)), Some(alg_params)) = (pbes2_aes_params(&algo), alg_params) {
        if let Some((salt, iters, mut iv)) = pbes2_extract_params(ctx, alg_params) {
            // ENCRYPT (mode 1) with no embedded IV: the real
            // `PBES2Core`/`PBEUtil$PBES2Params` auto-generates a random
            // block-size IV in this situation (`PBEParameterSpec(salt,
            // iterationCount)`, the common 2-arg form, carries no IV — the
            // caller expects the CIPHER to make one up and hand it back via
            // `getParameters()` for the caller to persist). We do the same
            // here; `Cipher.getParameters()` (below) rebuilds an
            // `AlgorithmParameters` embedding this exact IV so
            // `PKCS12KeyStore.encryptPrivateKey`'s `new AlgorithmId(oid,
            // cipher.getParameters())` — and any later decrypt — sees it.
            if iv.is_empty() && mode == 1 {
                let mut generated = vec![0u8; 16];
                if crate::securerandom::os_random_bytes(&mut generated) {
                    iv = generated;
                }
            }
            let password_bytes = extract_key_bytes(ctx, key);
            let aes_key =
                crate::phases_early::pbkdf2_derive_for(prf, &password_bytes, &salt, iters, keylen);
            with_table_write(|t| {
                let s = t.entry(tkey).or_default();
                s.mode = mode;
                s.key_bytes = aes_key;
                s.iv_bytes = iv;
                s.rsa_n = Vec::new();
                s.rsa_exp = Vec::new();
                s.pbe_salt = salt;
                s.pbe_iterations = iters;
                s.accumulated.clear();
                s.aad.clear();
            });
            return Ok(None);
        }
    }
    cipher_init_record(ctx, this, mode, key, Vec::new())
}

/// Append `input[offset..offset+len]` (the offset/length form of `update` /
/// `doFinal`) to the cipher's accumulator. No-op when the input arg is null.
fn accumulate_slice(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    input: Option<&Value>,
    offset: Option<&Value>,
    len: Option<&Value>,
) {
    let Some(Value::Object(Some(arr))) = input else {
        return;
    };
    let all = read_bytes(ctx, *arr);
    let ofs = offset.and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
    let n = len.and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
    let slice = all.get(ofs..ofs.saturating_add(n)).unwrap_or(&[]).to_vec();
    let tkey = obj_key(ctx, this);
    with_table_write(|t| {
        if let Some(s) = t.get_mut(&tkey) {
            s.accumulated.extend_from_slice(&slice);
        }
    });
}

/// Build the result byte[] for a successful `doFinal`, reset the per-cipher
/// accumulators (the JDK contract: `doFinal` resets the cipher to its
/// post-`init` state), and return it.
fn finish_cipher_bytes(
    ctx: &mut dyn NativeContext,
    table_key: i32,
    bytes: &[u8],
) -> MethodCallResult {
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
    for (i, &b) in bytes.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
    }
    with_table_write(|t| {
        if let Some(s) = t.get_mut(&table_key) {
            s.accumulated.clear();
            s.aad.clear();
        }
    });
    Ok(Some(Value::Object(Some(arr))))
}

/// Allocate a freshly initialised Cipher synthetic and register an
/// empty `CipherState` keyed on its heap pointer.  The algorithm
/// string is stashed in the side-table — we do **not** write to any
/// instance field of the real JDK class, because field-5 is
/// `initialized:Z` (a primitive boolean) and storing an Object there
/// breaks the `expected object reference` invariant on read-back.
/// Reject `Cipher.getInstance` transformations that the requested JDK
/// provider does not actually supply, so the native shim's "accept
/// everything" behaviour doesn't mask a real-JDK `NoSuchAlgorithmException`.
///
/// Concretely: SunJCE provides AES in ECB/CBC/PCBC/CTR/CTS/CFB/OFB/GCM/KW/KWP
/// but NOT **CCM** (that AEAD mode ships with BouncyCastle, not the JDK). On
/// HotSpot `Cipher.getInstance("AES/CCM/…","SunJCE")` throws
/// `NoSuchAlgorithmException: No such algorithm: AES/CCM/…`. Our native AES
/// dispatch can't do CCM either, so accepting it (returning a synthetic
/// Cipher) is strictly wrong — it silently masks the rejection that callers
/// like Tomcat's `EncryptInterceptor` depend on to refuse the transform
/// (TestEncryptInterceptorAlgorithms `doTestShouldNotSucceed`). Throw the
/// catchable checked exception so the real-JDK call site behaves as on HotSpot.
fn check_transformation_supported(
    ctx: &mut dyn NativeContext,
    algo: &str,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    let (_cipher, mode, _pad) = parse_transformation(algo);
    if mode == "CCM" {
        return Err(crate::phases_early::throw_jca_exc(
            ctx,
            "java/security/NoSuchAlgorithmException",
            &format!("No such algorithm: {algo}"),
        ));
    }
    Ok(())
}

fn cipher_alloc(ctx: &mut dyn NativeContext, algo: ObjectRef) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(ctx, "javax/crypto/Cipher", 6);
    let algo_str = ctx.read_string(algo).unwrap_or_default();
    // Compute key outside the closure — `obj_key` borrows `ctx` and the
    // table write-guard must not depend on the ctx borrow.
    let key = obj_key(ctx, obj);
    with_table_write(|t| {
        t.insert(
            key,
            CipherState {
                algorithm: algo_str,
                ..Default::default()
            },
        );
    });
    obj
}

/// Read the encoded key bytes out of a `SecretKeySpec`-shaped object.
/// Real `SecretKeySpec.key:byte[]` is the very first instance field
/// (index 0) and the slot type is an Object reference, so this read
/// is layout-compatible with the real JDK class.  When the spec is
/// allocated by our `register_param_specs` synthetic, field 0 is the
/// IV/key array; when it is allocated by real-JDK bytecode (via
/// `<init>` we intercept), we copy the bytes there ourselves.
fn extract_key_bytes(ctx: &mut dyn NativeContext, key_obj: ObjectRef) -> Vec<u8> {
    // First try the historical layout (synthetic Key with byte[] at slot 0).
    if let Value::Object(Some(arr)) = ctx.get_field(key_obj, 0) {
        let bytes = read_bytes(ctx, arr);
        if !bytes.is_empty() {
            return bytes;
        }
    }
    // bc_probe: KeyGenerator.generateKey() returns a 3-field synthetic from
    // crypto.rs::alloc_key — (alg_idx Int @0, size_bits Int @1, enc_len Int @2).
    // Slot 0 is NOT a byte[] here. Fall back to invoking getEncoded() which
    // produces a fresh byte[] of length enc_len.
    if let Ok(Some(Value::Object(Some(arr)))) =
        ctx.invoke_virtual(key_obj, "getEncoded", "()[B", &[])
    {
        return read_bytes(ctx, arr);
    }
    Vec::new()
}

/// Read the IV byte array out of an `IvParameterSpec` /
/// `GCMParameterSpec` object.  Both real classes hold the IV at
/// instance field 0 (an Object reference slot), so the layout
/// matches in either mode.
fn extract_iv_bytes(ctx: &mut dyn NativeContext, spec: ObjectRef) -> Vec<u8> {
    match ctx.get_field(spec, 0) {
        Value::Object(Some(arr)) => read_bytes(ctx, arr),
        _ => Vec::new(),
    }
}

/// Parse a Cipher transformation string (`"AES/GCM/NoPadding"`) into
/// `(cipherName, mode_uppercase, padding_bool)`.
fn parse_transformation(algo: &str) -> (String, String, bool) {
    let parts: Vec<&str> = algo.split('/').collect();
    let cipher_name = parts.first().copied().unwrap_or("AES").to_string();
    let mode = parts.get(1).copied().unwrap_or("ECB").to_uppercase();
    let pad = parts
        .get(2)
        .map(|p| !p.eq_ignore_ascii_case("NoPadding"))
        .unwrap_or(true);
    (cipher_name, mode, pad)
}

/// Whether `algo` names the RFC 3394 AES Key Wrap cipher supplied by SunJCE.
/// `AESWrap` and the size-specific `AESWrap_128` aliases are used by
/// Keycloak/Elytron; JDK callers may also use the canonical
/// `AES/KW/NoPadding` transformation.
fn is_aes_key_wrap_transformation(algo: &str) -> bool {
    matches!(
        algo.to_ascii_uppercase().as_str(),
        "AESWRAP"
            | "AESWRAP_128"
            | "AESWRAP128"
            | "AESWRAP_192"
            | "AESWRAP192"
            | "AESWRAP_256"
            | "AESWRAP256"
            | "AES/KW"
            | "AES/KW/NOPADDING"
    )
}

fn aes_wrap_expected_kek_len(algo: &str) -> Option<usize> {
    match algo.to_ascii_uppercase().as_str() {
        "AESWRAP_128" | "AESWRAP128" => Some(16),
        "AESWRAP_192" | "AESWRAP192" => Some(24),
        "AESWRAP_256" | "AESWRAP256" => Some(32),
        _ => None,
    }
}

const AES_KW_DEFAULT_IV: [u8; 8] = [0xA6; 8];

/// RFC 3394 AES Key Wrap, section 2.2.1.  This is deliberately kept next to
/// the `Cipher` dispatch rather than routing through a real `CipherSpi`: the
/// synthetic `Cipher` never initialises the real JDK provider fields, which is
/// precisely why `Cipher.wrap`/`unwrap` must not fall through to its bytecode.
fn aes_key_wrap(kek_bytes: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, String> {
    if plaintext.len() < 16 || plaintext.len() % 8 != 0 {
        return Err(format!(
            "AES Key Wrap plaintext length {} must be a multiple of 8 bytes and at least 16 bytes",
            plaintext.len()
        ));
    }
    let kek =
        Aes::key_expansion(kek_bytes).map_err(|e| format!("invalid AES Key Wrap key: {e:?}"))?;
    let n = plaintext.len() / 8;
    let mut a = AES_KW_DEFAULT_IV;
    let mut r = plaintext.to_vec();

    for j in 0..6 {
        for i in 0..n {
            let mut block = [0u8; 16];
            block[..8].copy_from_slice(&a);
            block[8..].copy_from_slice(&r[i * 8..(i + 1) * 8]);
            let encrypted = Aes::encrypt_block(&kek, &block);
            let t = (n * j + i + 1) as u64;
            let t_bytes = t.to_be_bytes();
            for k in 0..8 {
                a[k] = encrypted[k] ^ t_bytes[k];
            }
            r[i * 8..(i + 1) * 8].copy_from_slice(&encrypted[8..]);
        }
    }

    let mut wrapped = Vec::with_capacity(plaintext.len() + 8);
    wrapped.extend_from_slice(&a);
    wrapped.extend_from_slice(&r);
    Ok(wrapped)
}

/// RFC 3394 AES Key Unwrap, section 2.2.2.  The final IV verification is
/// performed without an early exit so malformed wrapped keys do not expose
/// which IV byte differed.
fn aes_key_unwrap(kek_bytes: &[u8], wrapped: &[u8]) -> Result<Vec<u8>, String> {
    if wrapped.len() < 24 || wrapped.len() % 8 != 0 {
        return Err(format!(
            "AES Key Wrap ciphertext length {} must be a multiple of 8 bytes and at least 24 bytes",
            wrapped.len()
        ));
    }
    let kek =
        Aes::key_expansion(kek_bytes).map_err(|e| format!("invalid AES Key Wrap key: {e:?}"))?;
    let n = wrapped.len() / 8 - 1;
    let mut a = [0u8; 8];
    a.copy_from_slice(&wrapped[..8]);
    let mut r = wrapped[8..].to_vec();

    for j in (0..6).rev() {
        for i in (0..n).rev() {
            let t = (n * j + i + 1) as u64;
            let t_bytes = t.to_be_bytes();
            let mut block = [0u8; 16];
            for k in 0..8 {
                block[k] = a[k] ^ t_bytes[k];
            }
            block[8..].copy_from_slice(&r[i * 8..(i + 1) * 8]);
            let decrypted = Aes::decrypt_block(&kek, &block);
            a.copy_from_slice(&decrypted[..8]);
            r[i * 8..(i + 1) * 8].copy_from_slice(&decrypted[8..]);
        }
    }

    let mismatch = a
        .iter()
        .zip(AES_KW_DEFAULT_IV.iter())
        .fold(0u8, |diff, (actual, expected)| diff | (actual ^ expected));
    if mismatch != 0 {
        return Err("AES Key Wrap integrity check failed".into());
    }
    Ok(r)
}

/// Native implementation of `Cipher.wrap(Key)`.  The JVM's real `Cipher`
/// bytecode cannot be used because native `init` stores state in our side
/// table, not in the JDK object's private `spi` and `initialized` fields.
fn cipher_wrap_impl(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    key_to_wrap: ObjectRef,
) -> MethodCallResult {
    let table_key = obj_key(ctx, this);
    let state = with_table_read(|t| t.get(&table_key).cloned());
    let Some(state) = state else {
        return Err(RuntimeError::IllegalStateException {
            message: "Cipher state missing (init never called or stale post-GC)".into(),
        }
        .into());
    };
    if state.mode != 3 {
        return Err(RuntimeError::IllegalStateException {
            message: "Cipher not initialized for wrapping".into(),
        }
        .into());
    }
    if !is_aes_key_wrap_transformation(&state.algorithm) {
        return Err(RuntimeError::IllegalStateException {
            message: format!("Cipher.wrap not implemented for {}", state.algorithm),
        }
        .into());
    }
    if let Some(expected_len) = aes_wrap_expected_kek_len(&state.algorithm) {
        if state.key_bytes.len() != expected_len {
            return Err(crate::phases_early::throw_jca_exc(
                ctx,
                "java/security/InvalidKeyException",
                &format!(
                    "{} requires a {}-bit key-encryption key, got {} bits",
                    state.algorithm,
                    expected_len * 8,
                    state.key_bytes.len() * 8
                ),
            ));
        }
    }
    let encoded = extract_key_bytes(ctx, key_to_wrap);
    if encoded.is_empty() {
        return Err(crate::phases_early::throw_jca_exc(
            ctx,
            "java/security/InvalidKeyException",
            "Key to wrap has no encoded form",
        ));
    }
    let wrapped = aes_key_wrap(&state.key_bytes, &encoded).map_err(|message| {
        crate::phases_early::throw_jca_exc(ctx, "java/security/InvalidKeyException", &message)
    })?;
    Ok(Some(Value::Object(Some(make_bytes_array(ctx, &wrapped)))))
}

/// Native implementation of `Cipher.unwrap(byte[], String, int)`, including
/// creation of the resulting `SecretKeySpec` for the JWE AES content-encryption
/// key. `AesKeyWrapAlgorithmProvider.decodeCek` uses exactly this SECRET_KEY
/// path on a fresh Cipher initialised only for `UNWRAP_MODE`.
fn cipher_unwrap_impl(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let wrapped = obj_arg(args, 1)?;
    let algorithm = obj_arg(args, 2)?;
    let key_type = args.get(3).and_then(Value::as_int).unwrap_or(0);
    let table_key = obj_key(ctx, this);
    let state = with_table_read(|t| t.get(&table_key).cloned());
    let Some(state) = state else {
        return Err(RuntimeError::IllegalStateException {
            message: "Cipher state missing (init never called or stale post-GC)".into(),
        }
        .into());
    };
    if state.mode != 4 {
        return Err(RuntimeError::IllegalStateException {
            message: "Cipher not initialized for unwrapping".into(),
        }
        .into());
    }
    if !is_aes_key_wrap_transformation(&state.algorithm) {
        return Err(RuntimeError::IllegalStateException {
            message: format!("Cipher.unwrap not implemented for {}", state.algorithm),
        }
        .into());
    }
    if let Some(expected_len) = aes_wrap_expected_kek_len(&state.algorithm) {
        if state.key_bytes.len() != expected_len {
            return Err(crate::phases_early::throw_jca_exc(
                ctx,
                "java/security/InvalidKeyException",
                &format!(
                    "{} requires a {}-bit key-encryption key, got {} bits",
                    state.algorithm,
                    expected_len * 8,
                    state.key_bytes.len() * 8
                ),
            ));
        }
    }
    // Cipher.SECRET_KEY is 3. AES Key Wrap returns raw symmetric key material;
    // public/private-key reconstruction requires algorithm-specific parsers and
    // is intentionally not pretended to work here.
    if key_type != 3 {
        return Err(crate::phases_early::throw_jca_exc(
            ctx,
            "java/security/InvalidKeyException",
            "AES Key Wrap supports Cipher.SECRET_KEY unwrap only",
        ));
    }
    let key_algorithm = ctx.read_string(algorithm).unwrap_or_default();
    if key_algorithm.is_empty() {
        return Err(crate::phases_early::throw_jca_exc(
            ctx,
            "java/security/InvalidKeyException",
            "Unwrapped key algorithm must not be empty",
        ));
    }
    let plaintext =
        aes_key_unwrap(&state.key_bytes, &read_bytes(ctx, wrapped)).map_err(|message| {
            crate::phases_early::throw_jca_exc(ctx, "java/security/InvalidKeyException", &message)
        })?;

    let key_bytes = make_bytes_array(ctx, &plaintext);
    let pin = ctx.pin_native_root(key_bytes);
    let algo = ctx.create_string(&key_algorithm);
    let key_bytes = ctx.read_native_pin(pin, key_bytes);
    let result = ctx.new_object_initialized(
        "javax/crypto/spec/SecretKeySpec",
        "([BLjava/lang/String;)V",
        &[Value::Object(Some(key_bytes)), Value::Object(Some(algo))],
    );
    ctx.unpin_native_roots(pin);
    result
}

/// Execute `doFinal` against the configured cipher state.
///
/// Currently dispatches the WP6.3-probe-required `AES/GCM/NoPadding`
/// path through `crypto_impl::AesGcm::{encrypt, decrypt}`.  Other
/// modes (ECB / CBC / CTR / ChaCha20-Poly1305) are out of probe scope
/// and return an `IllegalStateException` describing the requested
/// transformation rather than silently producing wrong bytes.
fn cipher_do_final_impl(ctx: &mut dyn NativeContext, this: ObjectRef) -> MethodCallResult {
    let key = obj_key(ctx, this);
    let state = with_table_read(|t| t.get(&key).cloned());

    // Round-13 C13: missing-state used to silently return
    // `Ok(Some(Value::Object(None)))` — apparent "success" producing a
    // NULL ciphertext.  With the GC-stable identity-hash key in place,
    // a missing entry now means the caller never invoked `init` (or a
    // future regression has re-introduced a key-instability bug); fail
    // loudly so the symptom surfaces at the first wrong call instead of
    // propagating silent NULLs through the user's pipeline.
    let Some(state) = state else {
        return Err(RuntimeError::IllegalStateException {
            message: "Cipher state missing (init never called or stale post-GC)".into(),
        }
        .into());
    };

    let mode = state.mode;
    if mode == 0 {
        return Err(RuntimeError::IllegalStateException {
            message: "Cipher state missing (init never called or stale post-GC)".into(),
        }
        .into());
    }

    let algo = state.algorithm.clone();
    let key_bytes = state.key_bytes.clone();
    let iv_bytes = state.iv_bytes.clone();
    let data = state.accumulated.clone();
    let aad = state.aad.clone();
    let rsa_n = state.rsa_n.clone();
    let rsa_exp = state.rsa_exp.clone();

    // RSA cipher (`RSA/ECB/{PKCS1Padding, OAEPWith…}`). The symmetric AES path
    // below would misread the RSA key encoding as an AES key (the historic
    // "Invalid AES key: InvalidKeyLength(294)" bug), so route RSA through the
    // real-crypto modexp + RFC-8017 padding in `crypto_impl`. Works for genuine
    // and synthetic RSA keys alike (components captured at `init`).
    if is_rsa_transformation(&algo) {
        let padding = algo.split('/').nth(2).unwrap_or("PKCS1Padding");
        let pad = match crate::crypto_impl::RsaCipherPadding::from_transformation(padding) {
            Some(p) => p,
            None => {
                return Err(RuntimeError::IllegalStateException {
                    message: format!("RSA cipher padding '{}' not supported", padding),
                }
                .into())
            }
        };
        if rsa_n.is_empty() || rsa_exp.is_empty() {
            return Err(RuntimeError::IllegalStateException {
                message:
                    "RSA cipher: key components unavailable (init did not capture modulus/exponent)"
                        .into(),
            }
            .into());
        }
        // 1 = ENCRYPT, 2 = DECRYPT, 3 = WRAP, 4 = UNWRAP.
        let encrypt = mode == 1 || mode == 3;
        let result = if encrypt {
            crate::crypto_impl::rsa_cipher_encrypt(&rsa_n, &rsa_exp, pad, &data)
        } else {
            crate::crypto_impl::rsa_cipher_decrypt(&rsa_n, &rsa_exp, pad, &data)
        };
        return match result {
            Ok(bytes) => finish_cipher_bytes(ctx, key, &bytes),
            Err(msg) => Err(RuntimeError::IllegalStateException { message: msg }.into()),
        };
    }

    if key_bytes.is_empty() {
        return Err(RuntimeError::IllegalStateException {
            message: "No key provided".into(),
        }
        .into());
    }

    // Route block-cipher transformations the synthetic AES path can't do
    // (CBC/CFB/OFB chaining; DES/DESede have no native impl) to the real
    // SunJCE CipherSpi. Used by PEMFile to decrypt encrypted private keys
    // (AES/CBC, DESede/CBC, DES/CBC) and by Tomcat tribes
    // TestEncryptInterceptorAlgorithms (AES/CFB/PKCS5Padding,
    // AES/OFB/PKCS5Padding round-trips — SunJCE supports these feedback modes,
    // our in-tree AES only does GCM/CBC/CTR/ECB). Done BEFORE the AES
    // key_expansion below so 8-byte DES keys aren't rejected. `route_mode` is
    // the SunJCE mode string passed to `engineSetMode`; for DES/DESede we keep
    // the historical "CBC" (their PEM keys are CBC), for AES we forward the
    // actual feedback mode.
    {
        // PBES2 AES transformations (`PBEWithHmacSHA*AndAES_*`) have no `/` —
        // `parse_transformation` can't split them into (cipher, mode) — but
        // `cipher_init_record_pbes2` already derived a real AES key/IV via
        // PBKDF2 and stored them in `key_bytes`/`iv_bytes`, so they route the
        // same as a plain `AES/CBC` cipher (SunJCE's PBES2 always uses CBC
        // internally — see `PBES2Parameters`'s `aes128CBC_OID`/`aes256CBC_OID`).
        let is_pbes2_aes = pbes2_aes_params(&algo).is_some();
        let (cn, cm, _pad) = parse_transformation(&algo);
        let route: Option<(&'static str, &'static str, &'static str)> = if is_pbes2_aes {
            Some(("com/sun/crypto/provider/AESCipher$General", "AES", "CBC"))
        } else {
            match (cn.to_ascii_uppercase().as_str(), cm.as_str()) {
                ("AES", "CBC") => Some(("com/sun/crypto/provider/AESCipher$General", "AES", "CBC")),
                ("AES", "CFB") => Some(("com/sun/crypto/provider/AESCipher$General", "AES", "CFB")),
                ("AES", "OFB") => Some(("com/sun/crypto/provider/AESCipher$General", "AES", "OFB")),
                ("DESEDE", _) | ("TRIPLEDES", _) => {
                    Some(("com/sun/crypto/provider/DESedeCipher", "DESede", "CBC"))
                }
                ("DES", _) => Some(("com/sun/crypto/provider/DESCipher", "DES", "CBC")),
                _ => None,
            }
        };
        if let Some((spi_class, key_algo, route_mode)) = route {
            let pad_str = if is_pbes2_aes {
                "PKCS5Padding"
            } else if algo
                .split('/')
                .nth(2)
                .map(|p| p.eq_ignore_ascii_case("NoPadding"))
                .unwrap_or(false)
            {
                "NoPadding"
            } else {
                "PKCS5Padding"
            };
            let out = crate::phases_early::drive_real_cipher(
                ctx, spi_class, route_mode, pad_str, mode, &key_bytes, key_algo, &iv_bytes, &data,
            )?;
            // Reset accumulators for reuse — but FIRST capture the result so a
            // moving GC during the reset alloc can't relocate it.
            if let Some(Value::Object(Some(_))) = out {
                with_table_write(|t| {
                    if let Some(s) = t.get_mut(&key) {
                        s.accumulated.clear();
                        s.aad.clear();
                    }
                });
            }
            return Ok(out);
        }
    }

    let aes_key = match Aes::key_expansion(&key_bytes) {
        Ok(k) => k,
        Err(e) => {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("Invalid AES key: {:?}", e),
            }
            .into())
        }
    };

    let (_cipher_name, mode_str, _pad) = parse_transformation(&algo);
    let encrypt = mode == 1;

    let result_bytes: Result<Vec<u8>, String> = match mode_str.as_str() {
        "GCM" => {
            if iv_bytes.len() != 12 {
                Err(format!(
                    "AES-GCM requires a 12-byte IV, got {}",
                    iv_bytes.len()
                ))
            } else {
                let mut nonce = [0u8; 12];
                nonce.copy_from_slice(&iv_bytes);
                if encrypt {
                    let out = AesGcm::encrypt(&aes_key, &nonce, &data, &aad);
                    let mut buf = out.ciphertext;
                    buf.extend_from_slice(&out.tag);
                    Ok(buf)
                } else if data.len() < 16 {
                    Err("AES-GCM ciphertext shorter than 16-byte tag".to_string())
                } else {
                    let split = data.len() - 16;
                    let ct = &data[..split];
                    let mut tag = [0u8; 16];
                    tag.copy_from_slice(&data[split..]);
                    AesGcm::decrypt(&aes_key, &nonce, ct, &aad, &tag)
                        .map_err(|e| format!("AES-GCM decrypt failed: {:?}", e))
                }
            }
        }
        "ECB" | "" => {
            // PKCS#7-padded AES/ECB: encrypt/decrypt each 16-byte block
            // independently with the expanded AES round keys. Probe surface
            // is `Cipher.getInstance("AES/ECB/PKCS7Padding", "BC")` —
            // BouncyCastle's PKCS7 padding is identical to PKCS5 (block
            // size 16, pad byte = pad count, full pad block on aligned
            // input).
            let block_size = 16usize;
            if encrypt {
                let pad_len = block_size - (data.len() % block_size);
                let mut padded = data.clone();
                // For PKCS7 we ALWAYS pad — a full-block-aligned input
                // gets a full pad block (pad_len == 16 here), which is
                // the standard behaviour and required for unambiguous
                // unpadding on decrypt.
                padded.extend(std::iter::repeat(pad_len as u8).take(pad_len));
                let mut out = Vec::with_capacity(padded.len());
                for chunk in padded.chunks(block_size) {
                    let mut block = [0u8; 16];
                    block.copy_from_slice(chunk);
                    let ct = Aes::encrypt_block(&aes_key, &block);
                    out.extend_from_slice(&ct);
                }
                Ok(out)
            } else if data.len() % block_size != 0 {
                Err(format!(
                    "AES/ECB ciphertext length {} not a multiple of 16",
                    data.len()
                ))
            } else {
                let mut out = Vec::with_capacity(data.len());
                for chunk in data.chunks(block_size) {
                    let mut block = [0u8; 16];
                    block.copy_from_slice(chunk);
                    let pt = Aes::decrypt_block(&aes_key, &block);
                    out.extend_from_slice(&pt);
                }
                // Strip PKCS7 padding from last byte.
                if let Some(&pad) = out.last() {
                    if pad as usize >= 1 && (pad as usize) <= block_size {
                        let new_len = out.len().saturating_sub(pad as usize);
                        out.truncate(new_len);
                    }
                }
                Ok(out)
            }
        }
        other => Err(format!(
            "Cipher mode '{}' not implemented in WP6.3 dispatch (probe scope: AES/GCM/NoPadding)",
            other
        )),
    };

    match result_bytes {
        Ok(bytes) => {
            // FIX (TestEncryptInterceptorLargeHeap hard-abort-instead-of-OOME):
            // this used to allocate the output via the panicking `new_array`,
            // which `std::process::abort()`s the entire VM (killing every
            // remaining test in the batch) when a huge result (observed: a
            // ~1 GiB AES-GCM round-trip) can't fit in the young generation —
            // instead of the catchable `OutOfMemoryError` HotSpot throws. Use
            // the fallible `try_new_array` (same `try_new_ref_array`/
            // `try_alloc_array_full` idiom as the `ArrayList(int)` abend fix,
            // see `docs/internal/gaps/crash-01-arraylist-capacity-oom-abend.md`)
            // and throw a catchable OOME on `None` instead. (There is a
            // second, near-identical `cipher_do_final` in
            // `native-builtins/src/phases_early.rs` with the same pattern —
            // fixed separately; this is the one actually dispatched for
            // AES/GCM, confirmed via a live gdb backtrace at the abort site.)
            let Some(arr) = ctx.try_new_array(cratonvm_types::ArrayElementType::Byte, bytes.len())
            else {
                return Err(RuntimeError::OutOfMemoryError {
                    message: "Java heap space".to_string(),
                }
                .into());
            };
            for (i, &b) in bytes.iter().enumerate() {
                ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
            }
            // Reset accumulators after a successful doFinal so the
            // Cipher can be reused for another encrypt/decrypt cycle —
            // matching the JDK contract that "doFinal automatically
            // resets the cipher to the state it was in after the last
            // call to init".
            with_table_write(|t| {
                if let Some(s) = t.get_mut(&key) {
                    s.accumulated.clear();
                    s.aad.clear();
                }
            });
            Ok(Some(Value::Object(Some(arr))))
        }
        Err(msg) => Err(RuntimeError::IllegalStateException { message: msg }.into()),
    }
}

/// Register the WP6.3 Cipher class-init shim.
///
/// The chain is: `Cipher.<clinit>` → `Debug.getInstance` →
/// `Security.<clinit>` (and, transitively, `Provider.<clinit>`).
/// Replacing every `<clinit>` along the chain with a no-op short-circuits
/// the entire walk so the existing native dispatch in
/// `phases_early.rs::register_phase53_crypto` is reached.
///
/// All registrations are idempotent — registering the same triple
/// (`class.method descriptor`) twice is a no-op in
/// `NativeMethodRegistry` (collision check passes when the entry is
/// identical, just overwrites).  Calling this from `lib.rs` after
/// `register_phase53_natives` is therefore safe.
pub fn register_cipher_clinit_shim(r: &mut NativeMethodRegistry) {
    // SyntheticStub: this fn's own registrations are bypass shims — <clinit>
    // no-ops for Security/JceSecurity/Providers, plus canUseProvider()=>true,
    // isRestricted()=>false, getVerificationResult()=>null. They pretend JCE
    // provider verification/policy setup succeeded without performing it. The
    // nested register_cipher_dispatch/keygen/param_specs are real (Bridge) and
    // set their own category.
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
    // `javax/crypto/Cipher.<clinit>` itself.  Static fields (`debug`,
    // `pdebug`, `skipDebug`, `warnCount`) stay null/zero, which is fine
    // because every Cipher method we dispatch on is replaced by a
    // native intercept that never reads those fields.
    r.register("javax/crypto/Cipher", "<clinit>", "()V", clinit_noop);

    // `java/security/Security.<clinit>` is the load that fails with
    // `Error loading java.security file / Is a directory`.  No-opping
    // it leaves `Security.props` null; we already register a complete
    // native shim for `getProviders` / `getProvider` /
    // `addProvider` / `removeProvider` in `phases_early.rs` (lines
    // 8917-8999) that bypasses `props` entirely.
    //
    // Real-JCA bring-up: the real EC keygen path resolves
    // `AlgorithmParameters.getInstance("EC")` via `Security.getImpl` →
    // `getSpiClass`, which reads the static `spiMap`.  The plain no-op
    // leaves it null and NPEs.  Use a variant that sets `spiMap` to an
    // empty map (still skipping the failing `initialize()`).
    // Also needed for EC-scoped default routing (`route_ec_to_real`): real
    // `AlgorithmParameters.getInstance("EC")` (driven by the real
    // `ECKeyPairGenerator.initialize`) reads `spiMap`. Setting it to an empty
    // map is harmless for the synthetic RSA/AES/digest paths (which never read
    // it). Only the pure-synthetic kill-switch keeps the bare no-op.
    if crate::real_jca_mode() || crate::route_ec_to_real() {
        r.register(
            "java/security/Security",
            "<clinit>",
            "()V",
            security_clinit_spimap,
        );
    } else {
        r.register("java/security/Security", "<clinit>", "()V", clinit_noop);
    }

    // `java/security/Provider.<clinit>` — DO NOT no-op. The real clinit
    // populates the static `knownEngines: HashMap<String,EngineDescription>`
    // map by calling `addEngine(...)` ~30 times. Every consumer in
    // Provider/Provider$Service (`getEngineName`, `Service.<init>`,
    // `parseLegacy`, etc.) reads `knownEngines.get(...)`, so leaving
    // the field null caused chain-NPEs ("Cannot invoke get on null")
    // when BouncyCastleProvider.<init> walked its ~500 algorithm
    // registrations. The clinit itself only touches HashMap + a
    // simple inner class — no unimplemented surfaces, so let it run.
    // (Previously: `r.register("java/security/Provider", "<clinit>", ...)
    //  with `clinit_noop`. Removed 2026-04-25 during BcProbe debugging.)

    // `sun/security/util/Debug.<clinit>` — sub-tree of the same chain.
    // The real `<clinit>` reads `Security.getProperty("java.security.debug")`
    // which would NPE on the no-op'd Security.  No-opping Debug means
    // its static `args` field is null, so any unintercepted call to
    // `Debug.isOn` / `Debug.println` is dead-code from the cipher path.
    r.register("sun/security/util/Debug", "<clinit>", "()V", clinit_noop);

    // `sun/security/jca/Providers.<clinit>` — final hop in the chain
    // because `Cipher.getInstance` on the real path would call
    // `Providers.getProviderList()`.  We never reach the real
    // getInstance (it is intercepted), but loading the class for
    // `<init>` of Cipher synthetic still triggers init of the
    // declared-fields graph, so cover Providers too.
    r.register("sun/security/jca/Providers", "<clinit>", "()V", clinit_noop);
    r.register(
        "sun/security/jca/ProviderList",
        "<clinit>",
        "()V",
        clinit_noop,
    );
    // `Providers.startJarVerification()` / `stopJarVerification(Object)` —
    // a SEPARATE consumer of this same no-op'd clinit, unrelated to the
    // Cipher/KeyGenerator bring-up this shim was written for.
    // `sun.security.util.SignatureFileVerifier.<init>` (real jar-signature
    // verification, e.g. `java.util.jar.JarEntry.getCertificates()` via
    // `JarInputStream`/`JarVerifier`) always wraps its body in
    // `try { obj = Providers.startJarVerification(); ... } finally {
    // Providers.stopJarVerification(obj); }`. The real `<clinit>` normally
    // sets the static fields `providerList` (`ProviderList
    // .fromSecurityProperties()`) and `threadLists` (`new ThreadLocal<>()`);
    // no-opping it leaves both null. `startJarVerification()` NPEs
    // dereferencing `getSystemProviderList().getJarList(...)` on the null
    // `providerList`, and the `finally` block's `stopJarVerification(null)`
    // → `endThreadProviderList(null)` NPEs calling `.remove()` on the null
    // `threadLists` `ThreadLocal` itself — the *finally*-block exception
    // replaces the try-block one (plain try/finally, not try-with-resources,
    // so nothing is recorded as suppressed), surfacing as: "Cannot invoke
    // java.lang.ThreadLocal.remove() because sun.security.jca.Providers
    // .threadLists is null". This broke every real-signed-jar case of
    // Spring Boot loader's `SecurityInfoTests`/`NestedJarFileTests`
    // (`SecurityInfo.load` → certs/codeSigners always null).
    //
    // The real purpose of this thread-local provider swap is to stop JAR
    // verification from recursively loading providers out of the very JAR
    // being verified — moot here, since CratonVM's actual crypto dispatch
    // (CertificateFactory/Signature/MessageDigest) is native, not routed
    // through a loaded `ProviderList` at all. So bypass the two entry
    // points directly rather than resurrecting `ProviderList
    // .fromSecurityProperties()`/`threadLists` bring-up (the exact chain
    // the original no-op was written to avoid).
    r.register(
        "sun/security/jca/Providers",
        "startJarVerification",
        "()Ljava/lang/Object;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );
    r.register(
        "sun/security/jca/Providers",
        "stopJarVerification",
        "(Ljava/lang/Object;)V",
        clinit_noop,
    );

    // `javax/crypto/JceSecurity.<clinit>` — the real JDK-25 clinit invokes
    // `setupJurisdictionPolicies()` which reads the `crypto.policy` Security
    // property (we've no-op'd `Security.<clinit>`, so its `props` map is
    // null) and then resolves `$JAVA_HOME/conf/security/policy/<name>/`.
    // Even though the policy files exist on disk, the policy-file loader
    // walks the synthetic `Security` props graph (null) and throws
    // `SecurityException: Missing mandatory jurisdiction policy files:
    // unlimited`, wrapped in `ExceptionInInitializerError` at
    // `KeyGenerator.getInstance` → `JceSecurity.<clinit>`.
    //
    // No-opping is safe for the BcProbe path because `KeyGenerator` /
    // `Cipher` getInstance are intercepted natively, and the only fields
    // the surviving real-JDK code reads are:
    //   - `isRestricted:Z` — defaults to `false` (= unlimited strength),
    //     which is the intended "unlimited policy" outcome anyway.
    //   - `defaultPolicy` / `exemptPolicy` — only read by
    //     `JceSecurity.getDefaultPolicy()` / `getExemptPolicy()` which
    //     BouncyCastle / KeyGenerator-via-native-intercept never reach.
    //   - `verificationResults` / `verifyingProviders` / `queue` /
    //     `codeBaseCacheRef` / `NULL_URL` / `PROVIDER_VERIFIED` — only
    //     used by `verifyProvider` / `getVerificationResult` /
    //     `canUseProvider`, which our `JceSecurity.canUseProvider`
    //     intercept can short-circuit (see below).
    r.register("javax/crypto/JceSecurity", "<clinit>", "()V", clinit_noop);
    // `JceSecurity.canUseProvider(Provider)` returns true so that
    // post-clinit `KeyGenerator` / `Mac` lookups don't fault on the null
    // `verifyingProviders` map.  Real-JDK behaviour for a signed-JCE
    // provider is `true`; for BouncyCastle (loaded via reflection in
    // BcProbe) we accept it unconditionally — provider verification is a
    // signing-cert check, not a security-policy gate.
    r.register(
        "javax/crypto/JceSecurity",
        "canUseProvider",
        "(Ljava/security/Provider;)Z",
        |_ctx, _args| Ok(Some(Value::Int(1))),
    );
    // `JceSecurity.isRestricted()` returns false (unlimited).  Matches the
    // default field value the no-op'd clinit leaves behind, but the static
    // accessor is explicitly registered so any reflective lookup sees a
    // resolved method instead of a null-method-table miss on the
    // synthetic class.
    r.register(
        "javax/crypto/JceSecurity",
        "isRestricted",
        "()Z",
        |_ctx, _args| Ok(Some(Value::Int(0))),
    );
    // `JceSecurity.getVerificationResult(Provider) -> Exception` returns
    // `null` to mean "provider passed JCE jar-signing verification".  Real-
    // JDK's clinit populates `verificationResults`/`verifyingProviders`/
    // `PROVIDER_VERIFIED` so this method can index into them — under our
    // no-op'd clinit those fields are null and the real bytecode NPEs at
    // `new WeakIdentityWrapper(p, queue)` or
    // `verificationResults.computeIfAbsent(...)`.  Bypass with `null`
    // so the `JceSecurity.getInstance(...)` overloads (we no-op those
    // too — see below) and any future caller see the "verified, no
    // failure" path.
    r.register(
        "javax/crypto/JceSecurity",
        "getVerificationResult",
        "(Ljava/security/Provider;)Ljava/lang/Exception;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );

    // `javax/crypto/JceSecurityManager.getCryptoPermission(String)` — the
    // real-JDK chokepoint for every Cipher crypto-strength decision:
    // `Cipher.getMaxAllowedKeyLength`, `Cipher.getMaxAllowedParameterSpec`,
    // and the init-time `checkCryptoPerm` all route through it. Its first
    // hop is `getDefaultPermission(alg)`, whose bytecode is
    // `getstatic JceSecurityManager.defaultPolicy` →
    // `defaultPolicy.getPermissionCollection(alg)`. Because we no-op
    // `JceSecurity.<clinit>` (above), `JceSecurity.defaultPolicy` is null;
    // `JceSecurityManager.<clinit>` copies it into
    // `JceSecurityManager.defaultPolicy`, so that too is null and the
    // `getPermissionCollection` invokevirtual NPEs ("Cannot invoke
    // CryptoPermissions.getPermissionCollection because defaultPolicy is
    // null"). This breaks any real-JDK Cipher path that consults the
    // policy — e.g. Tomcat tribes `TestEncryptInterceptor.test192/256BitKey`
    // gate on `Cipher.getMaxAllowedKeyLength("AES") >= 192/256`. (The sibling
    // `TestEncryptInterceptorAlgorithms` failures are a DIFFERENT root cause —
    // the native AES dispatch lacks CFB/OFB modes and the getInstance shim
    // doesn't reject SunJCE-unsupported transforms like CCM — not this NPE.)
    //
    // JDK 9+ ships `crypto.policy=unlimited` by default: the loaded
    // `defaultPolicy` grants `CryptoAllPermission` for every algorithm, so
    // `getCryptoPermission` resolves to `CryptoAllPermission.INSTANCE`
    // (whose `maxKeySize` is `Integer.MAX_VALUE`) — see the real method's
    // `if_acmpne` against `CryptoAllPermission.INSTANCE` early-return. We
    // reproduce that unlimited outcome directly by returning the singleton:
    // `getMaxAllowedKeyLength` then reports `Integer.MAX_VALUE` and every
    // `checkCryptoPerm` passes. `CryptoAllPermission`'s `<clinit>`/`<init>`
    // are trivial (just `new CryptoAllPermission()` → `CryptoPermission(
    // String)` which sets `maxKeySize = Integer.MAX_VALUE`); they perform
    // no `Security`/policy-file reads, so initializing the class here is
    // safe even with our no-op'd `Security`/`JceSecurity` clinits.
    r.register(
        "javax/crypto/JceSecurityManager",
        "getCryptoPermission",
        "(Ljava/lang/String;)Ljavax/crypto/CryptoPermission;",
        |ctx, _args| {
            let cid = ctx.ensure_class_initialized("javax/crypto/CryptoAllPermission")?;
            let idx = ctx.static_field_index_by_name(cid, "INSTANCE").ok_or(
                RuntimeError::IllegalStateException {
                    message: "javax/crypto/CryptoAllPermission.INSTANCE \
                              static field not found"
                        .to_string(),
                },
            )?;
            Ok(Some(ctx.get_static_field(cid, idx)))
        },
    );

    register_cipher_dispatch(r);
    register_keygen_dispatch(r);
    register_param_specs(r);
    r.set_category(__prev_cat);
}

/// Register the missing 2-arg `KeyGenerator.getInstance` overloads
/// (`(String, String)` and `(String, Provider)`).  The 1-arg form is
/// registered in `phases_early.rs::register_phase53_crypto`; the 2-arg
/// forms fall through to the real-JDK bytecode which routes through
/// `JceSecurity.getInstance(String, Class, String, String)` →
/// `GetInstance.getService(type, algo, providerName)`.  In our boot we
/// don't populate the per-provider Service tables, so `getService`
/// returns null and the JDK code NPEs at `service.getProvider()`
/// (KeyGenerator.java:288).
///
/// The native intercept ignores the provider name/object — we have a
/// single AES/HmacSHA* implementation in `crypto_impl`, so requesting
/// "BC" vs "SunJCE" yields identical bytes.  The synthetic layout
/// matches `register_phase53_crypto`'s `KeyGenerator` shim:
/// `(algorithm@0, keySize@1)`, so `KeyGenerator.init(int)` /
/// `generateKey()` (also registered in `phases_early.rs`) work
/// unchanged on instances allocated here.
fn register_keygen_dispatch(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let kg = "javax/crypto/KeyGenerator";
    r.register(
        kg,
        "getInstance",
        "(Ljava/lang/String;Ljava/lang/String;)Ljavax/crypto/KeyGenerator;",
        |ctx, args| {
            let algo = obj_arg(args, 0)?;
            let obj = alloc_concurrent_synthetic(ctx, "javax/crypto/KeyGenerator", 2);
            ctx.set_field(obj, 0, Value::Object(Some(algo)));
            ctx.set_field(obj, 1, Value::Int(128)); // default key size
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        kg,
        "getInstance",
        "(Ljava/lang/String;Ljava/security/Provider;)Ljavax/crypto/KeyGenerator;",
        |ctx, args| {
            let algo = obj_arg(args, 0)?;
            let obj = alloc_concurrent_synthetic(ctx, "javax/crypto/KeyGenerator", 2);
            ctx.set_field(obj, 0, Value::Object(Some(algo)));
            ctx.set_field(obj, 1, Value::Int(128));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.set_category(__prev_cat);
}

/// Register `Cipher.getInstance` / `init` / `update` / `updateAAD` /
/// `doFinal` and the small set of accessor methods the probe path
/// reaches.  Every method body is independent of `Security` /
/// `Providers` static state — the dispatch lives entirely inside this
/// crate's `crypto_impl` AES/GCM helpers — so it works in both
/// real-JDK and synthetic-JDK modes.
fn register_cipher_dispatch(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let cipher = "javax/crypto/Cipher";

    r.register(
        cipher,
        "getInstance",
        "(Ljava/lang/String;)Ljavax/crypto/Cipher;",
        |ctx, args| {
            let algo = obj_arg(args, 0)?;
            let algo_str = ctx.read_string(algo).unwrap_or_default();
            check_transformation_supported(ctx, &algo_str)?;
            let obj = cipher_alloc(ctx, algo);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        cipher,
        "getInstance",
        "(Ljava/lang/String;Ljava/lang/String;)Ljavax/crypto/Cipher;",
        |ctx, args| {
            let algo = obj_arg(args, 0)?;
            let algo_str = ctx.read_string(algo).unwrap_or_default();
            check_transformation_supported(ctx, &algo_str)?;
            let obj = cipher_alloc(ctx, algo);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        cipher,
        "getInstance",
        "(Ljava/lang/String;Ljava/security/Provider;)Ljavax/crypto/Cipher;",
        |ctx, args| {
            let algo = obj_arg(args, 0)?;
            let algo_str = ctx.read_string(algo).unwrap_or_default();
            check_transformation_supported(ctx, &algo_str)?;
            let obj = cipher_alloc(ctx, algo);
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    r.register(cipher, "init", "(ILjava/security/Key;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let mode = args[1].as_int().unwrap_or(0);
        let key = obj_arg(args, 2)?;
        cipher_init_record(ctx, this, mode, key, Vec::new())
    });

    r.register(
        cipher,
        "init",
        "(ILjava/security/Key;Ljava/security/spec/AlgorithmParameterSpec;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let mode = args[1].as_int().unwrap_or(0);
            let key = obj_arg(args, 2)?;
            let iv_bytes = match args.get(3) {
                Some(Value::Object(Some(spec))) => extract_iv_bytes(ctx, *spec),
                _ => Vec::new(),
            };
            cipher_init_record(ctx, this, mode, key, iv_bytes)
        },
    );

    r.register(
        cipher,
        "init",
        "(ILjava/security/Key;Ljava/security/spec/AlgorithmParameterSpec;Ljava/security/SecureRandom;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let mode = args[1].as_int().unwrap_or(0);
            let key = obj_arg(args, 2)?;
            let iv_bytes = match args.get(3) {
                Some(Value::Object(Some(spec))) => extract_iv_bytes(ctx, *spec),
                _ => Vec::new(),
            };
            cipher_init_record(ctx, this, mode, key, iv_bytes)
        },
    );

    // OAEP-256 (keycloak `DefaultRsaKeyEncryption256JWEAlgorithmProvider`) calls
    // `cipher.init(mode, key, AlgorithmParameters)` — note `AlgorithmParameters`,
    // NOT `…spec.AlgorithmParameterSpec`. Without this overload the call fell
    // through to the real `Cipher.init` bytecode → `chooseProvider` →
    // `synchronized (initLock)` on the synthetic Cipher's null `initLock` → NPE
    // (`monitorenter in Cipher.chooseProvider`). The OAEP digest is already
    // encoded in the transformation string, so we ignore the params object —
    // EXCEPT for a PBES2 AES transformation (`PBEWithHmacSHA*AndAES_*`,
    // PKCS12KeyStore's key-protection cipher), where the params object holds
    // the salt/iterationCount the raw-password `PBEKey` must be derived
    // through to get a real AES key (`cipher_init_record_pbes2`).
    r.register(
        cipher,
        "init",
        "(ILjava/security/Key;Ljava/security/AlgorithmParameters;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let mode = args[1].as_int().unwrap_or(0);
            let key = obj_arg(args, 2)?;
            let alg_params = match args.get(3) {
                Some(Value::Object(Some(p))) => Some(*p),
                _ => None,
            };
            cipher_init_record_pbes2(ctx, this, mode, key, alg_params)
        },
    );

    r.register(
        cipher,
        "init",
        "(ILjava/security/Key;Ljava/security/AlgorithmParameters;Ljava/security/SecureRandom;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let mode = args[1].as_int().unwrap_or(0);
            let key = obj_arg(args, 2)?;
            let alg_params = match args.get(3) {
                Some(Value::Object(Some(p))) => Some(*p),
                _ => None,
            };
            cipher_init_record_pbes2(ctx, this, mode, key, alg_params)
        },
    );

    r.register(
        cipher,
        "init",
        "(ILjava/security/Key;Ljava/security/SecureRandom;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let mode = args[1].as_int().unwrap_or(0);
            let key = obj_arg(args, 2)?;
            cipher_init_record(ctx, this, mode, key, Vec::new())
        },
    );

    // These cannot fall through to `java.security.Cipher` bytecode: native
    // init deliberately keeps the state in `CIPHER_TABLE`, leaving the real
    // object's private SPI fields unset. In particular Keycloak's external
    // JWE AES Key Wrap test initialises a fresh Cipher only for UNWRAP_MODE.
    r.register(cipher, "wrap", "(Ljava/security/Key;)[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let key_to_wrap = obj_arg(args, 1)?;
        cipher_wrap_impl(ctx, this, key_to_wrap)
    });
    r.register(
        cipher,
        "unwrap",
        "([BLjava/lang/String;I)Ljava/security/Key;",
        cipher_unwrap_impl,
    );

    r.register(cipher, "update", "([B)[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Some(Value::Object(Some(input))) = args.get(1) {
            let bytes = read_bytes(ctx, *input);
            let tkey = obj_key(ctx, this);
            with_table_write(|t| {
                if let Some(s) = t.get_mut(&tkey) {
                    s.accumulated.extend_from_slice(&bytes);
                }
            });
        }
        let empty = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 0);
        Ok(Some(Value::Object(Some(empty))))
    });

    r.register(cipher, "updateAAD", "([B)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Some(Value::Object(Some(aad_input))) = args.get(1) {
            let bytes = read_bytes(ctx, *aad_input);
            let tkey = obj_key(ctx, this);
            with_table_write(|t| {
                if let Some(s) = t.get_mut(&tkey) {
                    s.aad.extend_from_slice(&bytes);
                }
            });
        }
        Ok(None)
    });

    r.register(cipher, "doFinal", "([B)[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Some(Value::Object(Some(input))) = args.get(1) {
            let bytes = read_bytes(ctx, *input);
            let tkey = obj_key(ctx, this);
            with_table_write(|t| {
                if let Some(s) = t.get_mut(&tkey) {
                    s.accumulated.extend_from_slice(&bytes);
                }
            });
        }
        cipher_do_final_impl(ctx, this)
    });

    r.register(cipher, "doFinal", "()[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        cipher_do_final_impl(ctx, this)
    });

    // `doFinal(input, inputOffset, inputLen)` → byte[]. The offset/length
    // variant keycloak's AES-GCM decrypt and several BC callers use.
    r.register(cipher, "doFinal", "([BII)[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        accumulate_slice(ctx, this, args.get(1), args.get(2), args.get(3));
        cipher_do_final_impl(ctx, this)
    });

    // `doFinal(input, inputOffset, inputLen, output)` → int (bytes written).
    // keycloak's `AesGcmEncryptionProvider.encryptBytes` sizes `output` via
    // `getOutputSize` then calls this 4-arg form. Neither was intercepted, so
    // the call reached the real `Cipher` bytecode → `checkCipherState()` →
    // "Cipher not initialized" (the synthetic Cipher's real SPI state is unset
    // because we service `init` natively). Compute the result and copy it into
    // the caller's `output` array.
    r.register(cipher, "doFinal", "([BII[B)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        accumulate_slice(ctx, this, args.get(1), args.get(2), args.get(3));
        let output = obj_arg(args, 4)?;
        let opin = ctx.pin_native_root(output);
        let res = cipher_do_final_impl(ctx, this);
        let out_bytes = match res {
            Ok(Some(Value::Object(Some(a)))) => read_bytes(ctx, a),
            Ok(_) => Vec::new(),
            Err(e) => {
                ctx.unpin_native_roots(opin);
                return Err(e);
            }
        };
        let output = ctx.read_native_pin(opin, output);
        ctx.unpin_native_roots(opin);
        for (i, &b) in out_bytes.iter().enumerate() {
            ctx.set_array_element(output, i, Value::Int(b as i8 as i32));
        }
        Ok(Some(Value::Int(out_bytes.len() as i32)))
    });

    // `getOutputSize(inputLen)` → the byte count `doFinal` will produce, so the
    // caller can pre-size its output buffer. Must be EXACT for the AES-GCM
    // encrypt path (the provider uses the whole array, not the returned count):
    // GCM encrypt adds a 16-byte tag, GCM decrypt strips it.
    r.register(cipher, "getOutputSize", "(I)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let input_len = args.get(1).and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
        let tkey = obj_key(ctx, this);
        let (algo, mode, acc) = with_table_read(|t| {
            t.get(&tkey)
                .map(|s| (s.algorithm.clone(), s.mode, s.accumulated.len()))
                .unwrap_or_default()
        });
        let total = acc + input_len;
        let (cipher_name, mode_str, _pad) = parse_transformation(&algo);
        let encrypt = mode == 1 || mode == 3;
        let out = if cipher_name.eq_ignore_ascii_case("RSA") {
            // RSA output is always the modulus size; not on the JWE hot path.
            total.max(256)
        } else if mode_str == "GCM" {
            if encrypt {
                total + 16
            } else {
                total.saturating_sub(16)
            }
        } else if encrypt {
            // Block cipher with PKCS padding: round up to the next 16-byte block.
            (total / 16 + 1) * 16
        } else {
            total
        };
        Ok(Some(Value::Int(out as i32)))
    });

    // --- javax.crypto.SecretKeyFactory — PBKDF2 (real key derivation) ---
    // The real JCA path throws "PBKDF2With… SecretKeyFactory not available"
    // (no provider service) and the real PBKDF2KeyImpl trips a ByteBuffer bug.
    // Compute PBKDF2 natively (see phases_early::pbkdf2_*). getInstance handles
    // ONLY PBKDF2* algorithms; any other gets the same NoSuchAlgorithmException
    // the real path would have thrown — no regression for non-PBKDF2 callers.
    {
        let skf = "javax/crypto/SecretKeyFactory";
        r.register(
            skf,
            "getInstance",
            "(Ljava/lang/String;)Ljavax/crypto/SecretKeyFactory;",
            crate::phases_early::pbkdf2_get_instance,
        );
        r.register(
            skf,
            "getInstance",
            "(Ljava/lang/String;Ljava/lang/String;)Ljavax/crypto/SecretKeyFactory;",
            crate::phases_early::pbkdf2_get_instance,
        );
        r.register(
            skf,
            "generateSecret",
            "(Ljava/security/spec/KeySpec;)Ljavax/crypto/SecretKey;",
            crate::phases_early::pbkdf2_generate_secret,
        );
    }

    r.register(
        cipher,
        "getAlgorithm",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let tkey = obj_key(ctx, this);
            let algo = with_table_read(|t| {
                t.get(&tkey)
                    .map(|s| s.algorithm.clone())
                    .unwrap_or_default()
            });
            let s = ctx.create_string(&algo);
            Ok(Some(Value::Object(Some(s))))
        },
    );

    r.register(cipher, "getBlockSize", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(16)))
    });

    r.register(cipher, "getOutputSize", "(I)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let input_len = args[1].as_int().unwrap_or(0);
        let tkey = obj_key(ctx, this);
        let (algo, mode) = with_table_read(|t| {
            t.get(&tkey)
                .map(|s| (s.algorithm.clone(), s.mode))
                .unwrap_or_default()
        });
        let upper = algo.to_uppercase();
        if upper.contains("GCM") {
            if mode == 1 {
                Ok(Some(Value::Int(input_len + 16)))
            } else {
                Ok(Some(Value::Int((input_len - 16).max(0))))
            }
        } else {
            let out = ((input_len + 15) / 16) * 16;
            Ok(Some(Value::Int(out)))
        }
    });

    r.register(cipher, "getIV", "()[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let tkey = obj_key(ctx, this);
        let iv_bytes =
            with_table_read(|t| t.get(&tkey).map(|s| s.iv_bytes.clone()).unwrap_or_default());
        if iv_bytes.is_empty() {
            return Ok(Some(Value::Object(None)));
        }
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, iv_bytes.len());
        for (i, &b) in iv_bytes.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
        }
        Ok(Some(Value::Object(Some(arr))))
    });

    // `Cipher.getParameters()` — only meaningful (registered) for the PBES2
    // AES ciphers `cipher_init_record_pbes2` handles: rebuild a fresh
    // `AlgorithmParameters` embedding the salt/iterationCount/IV actually
    // used (including an IV we may have auto-generated at `init` time, since
    // the caller's own `AlgorithmParameters` argument never had one — see
    // `cipher_init_record_pbes2`). `PKCS12KeyStore.encryptPrivateKey` calls
    // this right after `doFinal` to build the `AlgorithmId` it persists
    // alongside the ciphertext, so a later decrypt (e.g.
    // `KeyManagerFactory.init` reading the key back out) uses the SAME IV.
    // Every other Cipher (AES/GCM, RSA, …) never reaches this — no PBES2
    // state means `pbe_salt` is empty and we return null, matching
    // `getInstance`'s "no such Cipher provider" — i.e. no regression for
    // callers that never worked before this native existed.
    r.register(
        cipher,
        "getParameters",
        "()Ljava/security/AlgorithmParameters;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let tkey = obj_key(ctx, this);
            let (algo, salt, iters, iv) = with_table_read(|t| {
                t.get(&tkey)
                    .map(|s| {
                        (
                            s.algorithm.clone(),
                            s.pbe_salt.clone(),
                            s.pbe_iterations,
                            s.iv_bytes.clone(),
                        )
                    })
                    .unwrap_or_default()
            });
            if pbes2_aes_params(&algo).is_none() || salt.is_empty() {
                return Ok(Some(Value::Object(None)));
            }
            let algo_str = ctx.create_string(&algo);
            let ap = ctx.invoke(
                "java/security/AlgorithmParameters",
                "getInstance",
                "(Ljava/lang/String;)Ljava/security/AlgorithmParameters;",
                &[Value::Object(Some(algo_str))],
            )?;
            let Some(Value::Object(Some(ap_obj))) = ap else {
                return Ok(Some(Value::Object(None)));
            };
            let salt_arr = make_bytes_array(ctx, &salt);
            let iv_arr = make_bytes_array(ctx, &iv);
            let iv_spec = ctx.new_object_initialized(
                "javax/crypto/spec/IvParameterSpec",
                "([B)V",
                &[Value::Object(Some(iv_arr))],
            )?;
            let Some(iv_spec) = iv_spec else {
                return Ok(Some(Value::Object(None)));
            };
            let pbe_spec = ctx.new_object_initialized(
                "javax/crypto/spec/PBEParameterSpec",
                "([BILjava/security/spec/AlgorithmParameterSpec;)V",
                &[
                    Value::Object(Some(salt_arr)),
                    Value::Int(iters as i32),
                    iv_spec,
                ],
            )?;
            let Some(pbe_spec) = pbe_spec else {
                return Ok(Some(Value::Object(None)));
            };
            ctx.invoke_virtual(
                ap_obj,
                "init",
                "(Ljava/security/spec/AlgorithmParameterSpec;)V",
                &[pbe_spec],
            )?;
            Ok(Some(Value::Object(Some(ap_obj))))
        },
    );

    r.set_category(__prev_cat);
}

/// Register the parameter-spec types the probe (and any AEAD client)
/// hands to `Cipher.init`.  Each `<init>` natively copies the IV bytes
/// into synthetic-slot 0 so `register_cipher_dispatch::init` can pull
/// the IV back out without depending on the real JDK field layout.
fn register_param_specs(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let ivps = "javax/crypto/spec/IvParameterSpec";
    r.register(ivps, "<clinit>", "()V", clinit_noop);
    r.register(ivps, "<init>", "([B)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let iv_arr = obj_arg(args, 1)?;
        let len = ctx.array_length(iv_arr);
        let copy = ctx.new_array(cratonvm_types::ArrayElementType::Byte, len);
        for i in 0..len {
            if let Value::Int(b) = ctx.get_array_element(iv_arr, i) {
                ctx.set_array_element(copy, i, Value::Int(b));
            }
        }
        ctx.set_field(this, 0, Value::Object(Some(copy)));
        Ok(None)
    });
    r.register(ivps, "getIV", "()[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });

    let gcmps = "javax/crypto/spec/GCMParameterSpec";
    r.register(gcmps, "<clinit>", "()V", clinit_noop);
    r.register(gcmps, "<init>", "(I[B)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let t_len = args[1].as_int().unwrap_or(128);
        let iv_arr = obj_arg(args, 2)?;
        let len = ctx.array_length(iv_arr);
        let copy = ctx.new_array(cratonvm_types::ArrayElementType::Byte, len);
        for i in 0..len {
            if let Value::Int(b) = ctx.get_array_element(iv_arr, i) {
                ctx.set_array_element(copy, i, Value::Int(b));
            }
        }
        ctx.set_field(this, 0, Value::Object(Some(copy)));
        ctx.set_field(this, 1, Value::Int(t_len));
        Ok(None)
    });
    r.register(gcmps, "getIV", "()[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(gcmps, "getTLen", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });

    let sks = "javax/crypto/spec/SecretKeySpec";
    r.register(sks, "<clinit>", "()V", clinit_noop);
    r.register(sks, "<init>", "([BLjava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let key_bytes = obj_arg(args, 1)?;
        let algo = obj_arg(args, 2)?;
        ctx.set_field(this, 0, Value::Object(Some(key_bytes)));
        ctx.set_field(this, 1, Value::Object(Some(algo)));
        Ok(None)
    });
    r.register(sks, "getEncoded", "()[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(sks, "getAlgorithm", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
    r.register(sks, "getFormat", "()Ljava/lang/String;", |ctx, _args| {
        let s = ctx.create_string("RAW");
        Ok(Some(Value::Object(Some(s))))
    });
    r.set_category(__prev_cat);
}

#[cfg(test)]
mod tests {
    use super::*;
    use cratonvm_native_api::NativeMethodRegistry;

    #[test]
    fn shim_registers_all_clinits() {
        let mut r = NativeMethodRegistry::new();
        register_cipher_clinit_shim(&mut r);

        // 2026-04-25: Provider.<clinit> intentionally NOT shimmed any more —
        // the real clinit populates `knownEngines` which Provider$Service.<init>
        // and getEngineName depend on; no-opping it caused chain-NPEs during
        // BouncyCastleProvider.<init>. See `jca/cipher.rs::register_cipher_clinit_shim`.
        for cls in [
            "javax/crypto/Cipher",
            "java/security/Security",
            "sun/security/util/Debug",
            "sun/security/jca/Providers",
        ] {
            assert!(
                r.find(cls, "<clinit>", "()V").is_some(),
                "expected <clinit> shim for {cls}"
            );
        }
        // Provider clinit must NOT be shimmed.
        assert!(
            r.find("java/security/Provider", "<clinit>", "()V")
                .is_none(),
            "Provider.<clinit> must run real bytecode to populate knownEngines"
        );
    }

    #[test]
    fn shim_is_idempotent() {
        let mut r = NativeMethodRegistry::new();
        register_cipher_clinit_shim(&mut r);
        let count_after_first = r.len();
        // Re-register same triples — must not panic on hash collision
        // and must not change the registry size.
        register_cipher_clinit_shim(&mut r);
        assert_eq!(r.len(), count_after_first);
    }

    #[test]
    fn shim_registers_cipher_dispatch() {
        let mut r = NativeMethodRegistry::new();
        register_cipher_clinit_shim(&mut r);

        // Probe-relevant Cipher entry points
        assert!(r
            .find(
                "javax/crypto/Cipher",
                "getInstance",
                "(Ljava/lang/String;)Ljavax/crypto/Cipher;"
            )
            .is_some());
        assert!(r
            .find(
                "javax/crypto/Cipher",
                "init",
                "(ILjava/security/Key;Ljava/security/spec/AlgorithmParameterSpec;)V"
            )
            .is_some());
        assert!(r.find("javax/crypto/Cipher", "doFinal", "([B)[B").is_some());

        // Spec-type constructors
        assert!(r
            .find("javax/crypto/spec/GCMParameterSpec", "<init>", "(I[B)V")
            .is_some());
        assert!(r
            .find(
                "javax/crypto/spec/SecretKeySpec",
                "<init>",
                "([BLjava/lang/String;)V"
            )
            .is_some());
    }

    #[test]
    fn parse_transformation_aes_gcm_nopadding() {
        let (cipher, mode, pad) = parse_transformation("AES/GCM/NoPadding");
        assert_eq!(cipher, "AES");
        assert_eq!(mode, "GCM");
        assert!(!pad);
    }

    #[test]
    fn parse_transformation_aes_cbc_pkcs5() {
        let (cipher, mode, pad) = parse_transformation("AES/CBC/PKCS5Padding");
        assert_eq!(cipher, "AES");
        assert_eq!(mode, "CBC");
        assert!(pad);
    }

    #[test]
    fn aes_key_wrap_matches_rfc3394_vector() {
        // RFC 3394, section 4.1: 128-bit KEK wrapping a 128-bit key data.
        let kek = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f,
        ];
        let plaintext = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        let expected = [
            0x1f, 0xa6, 0x8b, 0x0a, 0x81, 0x12, 0xb4, 0x47, 0xae, 0xf3, 0x4b, 0xd8, 0xfb, 0x5a,
            0x7b, 0x82, 0x9d, 0x3e, 0x86, 0x23, 0x71, 0xd2, 0xcf, 0xe5,
        ];

        let wrapped = aes_key_wrap(&kek, &plaintext).expect("RFC vector must wrap");
        assert_eq!(wrapped, expected);
        assert_eq!(
            aes_key_unwrap(&kek, &wrapped).expect("RFC vector must unwrap"),
            plaintext
        );
    }

    #[test]
    fn aes_key_unwrap_rejects_tampered_integrity_value() {
        let kek = [0x11; 16];
        let plaintext = [0x22; 16];
        let mut wrapped = aes_key_wrap(&kek, &plaintext).expect("wrap must succeed");
        wrapped[0] ^= 1;
        assert!(aes_key_unwrap(&kek, &wrapped).is_err());
    }

    #[test]
    fn aes_key_wrap_recognises_sunjce_and_keycloak_names() {
        assert!(is_aes_key_wrap_transformation("AESWrap"));
        assert!(is_aes_key_wrap_transformation("AESWrap_128"));
        assert!(is_aes_key_wrap_transformation("AES/KW/NoPadding"));
        assert!(!is_aes_key_wrap_transformation("AES/KWP/NoPadding"));
        assert_eq!(aes_wrap_expected_kek_len("AESWrap_128"), Some(16));
        assert_eq!(aes_wrap_expected_kek_len("AESWrap_192"), Some(24));
        assert_eq!(aes_wrap_expected_kek_len("AESWrap_256"), Some(32));
    }

    #[test]
    fn parse_transformation_bare_aes() {
        let (cipher, mode, pad) = parse_transformation("AES");
        assert_eq!(cipher, "AES");
        assert_eq!(mode, "ECB");
        assert!(pad);
    }
}
