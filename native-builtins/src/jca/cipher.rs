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
use cratonvm_types::error::MethodCallFailed;
use cratonvm_types::error::{MethodCallResult, RuntimeError};
use cratonvm_types::{ObjectRef, Value};

// Round-9 MED-2: migrated `CIPHER_TABLE` from `std::sync::RwLock` to
// `parking_lot::RwLock` — removes the per-access `unwrap_or_else(into_inner)`
// poison dance and yields a smaller, faster lock.
use parking_lot::RwLock;
use rustc_hash::FxHashMap;

use crate::crypto_impl::{Aes, AesGcm, AesKey};
use crate::phases_early::CIPHER_IV;
use crate::{obj_arg, try_alloc_concurrent_synthetic};

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
// `RwLock<FxHashMap<CipherKey, CipherState>>` lookup; no allocation per
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
    /// The `crypto_impl` key handle behind this cipher's private key, when the
    /// key has one. `(n, d)` above is everything the decrypt path needs to be
    /// *correct*, but it cannot reach the CRT parameters, which live on the
    /// whole `RsaPrivateKey` in `crypto_impl`'s store; the handle can, and CRT
    /// is ~3x on the private op. `None` for a key with no handle (a real JDK
    /// key object this VM did not mint), which simply keeps the `(n, d)` path.
    /// A handle, not a key ref, for the same GC-safety reason as `key_bytes`.
    ///
    /// The handle is NOT trusted on its own: `rsa_cipher_decrypt_by_id` only
    /// honours it when the key it names carries `rsa_n` above, because the
    /// field-slot fallback in `rsa_private_key_handle` can read an unrelated
    /// small integer off a genuine JDK key and collide with a live id.
    rsa_key_id: Option<u64>,
    /// PBES2 (`PBEWithHmacSHA*AndAES_*`) salt, captured at `init` time from the
    /// `AlgorithmParameters` argument. Empty for non-PBES2 ciphers. Plain bytes
    /// (not an `ObjectRef`) for the same GC-safety reason as `key_bytes` — and
    /// so `Cipher.getParameters()` can rebuild a fresh `AlgorithmParameters`
    /// on demand without holding a heap reference across calls.
    pbe_salt: Vec<u8>,
    /// PBES2 iteration count, paired with `pbe_salt`. Zero for non-PBES2.
    pbe_iterations: u32,
    /// Digest of the (key, nonce) this Cipher last ENCRYPT-initialised under,
    /// for the ChaCha20 nonce-reuse refusal. `None` until the first such init.
    /// See `chacha20_check_nonce_reuse` for why this is per-instance.
    chacha_last_encrypt: Option<[u8; 32]>,
    /// ChaCha20 initial block counter, from `ChaCha20ParameterSpec.getCounter()`.
    ///
    /// Separate from `iv_bytes` because it is not part of the nonce and the two
    /// have different lifetimes in the RFC 8439 state: the nonce occupies words
    /// 13-15 and the counter word 12, and a caller that seeks into a stream
    /// varies only the counter. Zero for every other cipher, and unread by
    /// them.
    chacha_counter: u32,
    /// This `Cipher` is a thin wrapper over a THIRD-PARTY provider's own
    /// `CipherSpi`; every method below forwards to it and none of the state
    /// above is used. See `try_delegate_cipher_to_named_provider`.
    ///
    /// The SPI object itself is NOT here — it lives in the `Cipher`'s own
    /// `spi` field on the Java heap, so the collector roots and remaps it like
    /// any other reference. This table's invariant ("holds only plain Rust
    /// data, never an `ObjectRef`", see `CipherKey`) is preserved.
    delegated: bool,
}

/// VM-scoped, GC-stable side-table key: `(vm_identity, identity_hash_code)`.
///
/// The `vm_identity` component is NOT optional. An `identityHashCode` is unique
/// only *within one heap*, but `CIPHER_TABLE` is a process-global `static`;
/// Rust tests (and any embedder) create several independent `Vm`s in one
/// process, so a bare-`i32` key let VM B's `Cipher` read VM A's mode / key
/// bytes / IV / accumulated plaintext whenever their identity hashes collided.
/// `NativeContext::vm_identity`'s own doc states the rule ("Native side caches
/// ... must scope entries to this value", `native-api/src/registry.rs`); the
/// same omission in native-collections' `widened_obj_key` aliased two VMs'
/// collections and aborted the process.
///
/// `CipherState` holds only plain Rust data (`String`/`Vec<u8>`/ints) and never
/// an `ObjectRef`, so no collector scan/remap companion is required.
type CipherKey = (usize, i32);

static CIPHER_TABLE: RwLock<Option<FxHashMap<CipherKey, CipherState>>> = RwLock::new(None);

fn with_table_write<R>(f: impl FnOnce(&mut FxHashMap<CipherKey, CipherState>) -> R) -> R {
    // Round-9 MED-2: parking_lot — no poison handling.
    let mut g = CIPHER_TABLE.write();
    if g.is_none() {
        *g = Some(FxHashMap::default());
    }
    f(g.as_mut().expect("table just initialised"))
}

fn with_table_read<R>(f: impl FnOnce(&FxHashMap<CipherKey, CipherState>) -> R) -> R {
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
/// C13 note for why this matters.  The `vm_identity` component keeps two
/// `Vm`s in the same process from sharing entries -- see [`CipherKey`].
fn obj_key(ctx: &mut dyn NativeContext, obj: ObjectRef) -> CipherKey {
    (ctx.vm_identity(), ctx.identity_hash_code(obj))
}

/// `<clinit>` no-op — used to mark a real-JDK class as initialized
/// without executing its static initializer bytecode.  Equivalent in
/// effect to `lib.rs::native_noop`, restated here so the file is
/// self-contained and the call sites read cleanly.
#[inline]
fn clinit_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

/// Shared verification decision behind `JceSecurity.canUseProvider` and
/// `JceSecurity.getVerificationResult` — the two accessors MUST agree
/// (`canUseProvider(p)` is literally `getVerificationResult(p) == null` in
/// the real JDK), so they route through one function instead of being two
/// independent constants that can drift apart.
///
/// Real JDK: `verifyProviderJar(p.getClass().getProtectionDomain()
/// .getCodeSource())` — a **null CodeSource** (a JDK-bundled provider on the
/// boot/platform loader) is verified outright; anything else must carry a JCE
/// code-signing signature checked against the JDK's JCE signing roots.
///
/// CratonVM implements the first half faithfully and *cannot* implement the
/// second: it ships no JCE code-signing trust anchors, and `JceSecurity
/// .<clinit>` is no-op'd (registered below) so `verificationResults` /
/// `PROVIDER_VERIFIED` / `queue` do not exist to consult. Rather than return
/// a bare "verified" constant, we read the class's real CodeSource and, when
/// it is an unsigned non-boot source (which HotSpot would reject), disclose
/// the gap on the `tracing` log before accepting.
///
/// Note this gate is far less load-bearing here than on HotSpot: every
/// `JceSecurity.getInstance` consumer (`Cipher`, `KeyGenerator`, `Mac`,
/// `SecretKeyFactory`, `KeyAgreement`) is dispatched natively to
/// `crate::crypto_impl`, so a provider passing this check still never gets
/// its own crypto code invoked through the JCE path.
///
/// Returns `None` for "verified", `Some(reason)` for a verification failure.
fn jce_verify_provider(ctx: &mut dyn NativeContext, prov: Option<ObjectRef>) -> Option<String> {
    // Real JDK NPEs on `p.getClass()`; reporting a failure is closer to that
    // than silently answering "verified" for a provider that is not there.
    let p = match prov {
        Some(p) => p,
        None => return Some("provider is null".to_string()),
    };
    let cid = ctx.class_id_of_object(p);
    let name = ctx
        .class_name_of_id(cid)
        .unwrap_or_else(|| "<unknown>".to_string());
    // No CodeSource == boot/platform provider; real `verifyProviderJar`
    // returns null (verified) for exactly this case.
    let code_base = ctx.class_code_base(cid)?;
    if ctx.class_code_source_certs(cid).is_empty() && jce_first_unsigned_report(&name) {
        tracing::warn!(
            provider = %name,
            code_base = %code_base,
            "JceSecurity: provider code source carries no signer certificates; CratonVM has no \
             JCE code-signing trust anchors, so it is accepted unverified (HotSpot would reject \
             it). Crypto still dispatches natively, not through this provider."
        );
    }
    None
}

/// True the first time `class_name` is reported as an unsigned provider
/// source — keeps `jce_verify_provider`'s disclosure to one line per
/// provider class instead of one per `getInstance`.
fn jce_first_unsigned_report(class_name: &str) -> bool {
    use std::collections::HashSet;
    use std::sync::OnceLock;
    static SEEN: OnceLock<RwLock<HashSet<String>>> = OnceLock::new();
    SEEN.get_or_init(|| RwLock::new(HashSet::new()))
        .write()
        .insert(class_name.to_string())
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

/// Read the `byte[]` at instance-field `slot` and return a FRESH copy of it,
/// or `None` when the field is null.
///
/// The accessor idiom for every JCA key/spec class whose real implementation
/// ends `return this.<field>.clone()` — `SecretKeySpec.getEncoded`,
/// `IvParameterSpec.getIV`, `GCMParameterSpec.getIV`. The clone is not
/// defensive politeness in these classes; it is the contract callers build key
/// hygiene on, and the JDK's own providers scrub the array they get back.
fn clone_byte_field(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    slot: usize,
) -> Option<ObjectRef> {
    let Value::Object(Some(arr)) = ctx.get_field(this, slot) else {
        return None;
    };
    let bytes = read_bytes(ctx, arr);
    Some(make_bytes_array(ctx, &bytes))
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
    // The map is keyed `(vm_identity, identity_hash)`: an identity hash is
    // unique only within one heap, and the map is a process-global static.
    let id =
        crate::crypto_impl::rsa_realkey_map_get(ctx.vm_identity(), ctx.identity_hash_code(key))
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

/// The `crypto_impl` key handle behind an RSA private key object, if it has one.
///
/// Same resolution the synthetic-key branch of [`rsa_key_components`] uses — the
/// real-key side table first, then the handle field — but run unconditionally,
/// because a key can expose real `getModulus()`/`getPrivateExponent()` AND still
/// have a handle. `rsa_key_components` stops at the first, so it never learns
/// the second; the decrypt path wants both.
fn rsa_private_key_handle(ctx: &mut dyn NativeContext, key: ObjectRef) -> Option<u64> {
    crate::crypto_impl::rsa_realkey_map_get(ctx.vm_identity(), ctx.identity_hash_code(key))
        .or_else(|| match ctx.get_field(key, 3) {
            Value::Long(i) => Some(i as u64),
            Value::Int(i) => Some(i as u64),
            _ => None,
        })
        .filter(|&i| i != 0)
}

/// Shared `Cipher.init` recorder: snapshot mode + key bytes + IV, and (for RSA
/// transformations) the key's modulus/exponent components, into the side-table.
/// This Cipher's transformation string, as recorded by `getInstance`.
///
/// `init` needs it before `cipher_init_record` runs, because the ChaCha20
/// spec-type rule is a property of the TRANSFORMATION and the spec together.
fn cipher_algorithm_of(ctx: &mut dyn NativeContext, this: ObjectRef) -> String {
    let tkey = obj_key(ctx, this);
    with_table_read(|t| {
        t.get(&tkey)
            .map(|s| s.algorithm.clone())
            .unwrap_or_default()
    })
}

/// The `(nonce, counter)` a ChaCha20 transformation needs, plus SunJCE's
/// spec-TYPE rule, which is not interchangeable between the two ciphers:
///
/// ```text
/// ChaCha20         + IvParameterSpec        InvalidAlgorithmParameterException:
///                                             ChaCha20 algorithm requires ChaCha20ParameterSpec
/// ChaCha20-Poly1305 + ChaCha20ParameterSpec InvalidAlgorithmParameterException:
///                                             ChaCha20-Poly1305 requires IvParameterSpec
/// ```
///
/// Measured on OpenJDK 25.0.4. The asymmetry is real and worth enforcing: the
/// raw cipher needs a counter and the AEAD must not be given one, because the
/// AEAD's counter is fixed by RFC 8439 (0 for the one-time Poly1305 key, 1 for
/// the ciphertext) and a caller-chosen counter would silently overlap them.
///
/// `Ok(None)` means "not a ChaCha transformation" and the caller carries on.
fn chacha20_spec_params(
    ctx: &mut dyn NativeContext,
    algo: &str,
    spec: Option<ObjectRef>,
    mode: i32,
) -> Result<Option<(Vec<u8>, u32)>, cratonvm_types::error::MethodCallFailed> {
    if !is_chacha20_family(algo) {
        return Ok(None);
    }
    let aead = is_chacha20_poly1305_transformation(algo);
    let Some(spec) = spec else {
        // No spec at all. SunJCE GENERATES a fresh random nonce for ENCRYPT
        // (measured: two `init(ENCRYPT_MODE, key)` calls produce different
        // ciphertext for the same plaintext) and the caller recovers it through
        // `getIV()`. For DECRYPT there is nothing to generate and SunJCE
        // refuses; `chacha20_do_final` reports the empty nonce there.
        //
        // The nonce MUST come from the CSPRNG and never from a counter or the
        // clock: ChaCha20 nonce reuse under one key reveals the XOR of two
        // plaintexts, which is the whole reason SunJCE also refuses a repeated
        // (key, nonce) pair on a second ENCRYPT init.
        if mode == 1 || mode == 3 {
            let mut nonce = vec![0u8; 12];
            crate::crypto_impl::secure_random_fill(0, &mut nonce);
            return Ok(Some((nonce, 0)));
        }
        return Ok(Some((Vec::new(), 0)));
    };
    let spec_class = ctx
        .class_name_of_id(ctx.class_id_of_object(spec))
        .unwrap_or_default();
    let is_cc20_spec = spec_class == "javax/crypto/spec/ChaCha20ParameterSpec";
    let is_iv_spec = spec_class == "javax/crypto/spec/IvParameterSpec";
    if aead && is_cc20_spec {
        return Err(crate::phases_early::throw_jca_exc(
            ctx,
            "java/security/InvalidAlgorithmParameterException",
            "ChaCha20-Poly1305 requires IvParameterSpec",
        ));
    }
    if !aead && is_iv_spec {
        return Err(crate::phases_early::throw_jca_exc(
            ctx,
            "java/security/InvalidAlgorithmParameterException",
            "ChaCha20 algorithm requires ChaCha20ParameterSpec",
        ));
    }
    // Field 0 is the nonce/iv on BOTH classes (`javap -p`: ChaCha20ParameterSpec
    // declares `byte[] nonce` then `int counter`, with NONCE_LENGTH static and
    // so not an instance slot). Field 1 is the counter, and only the raw
    // cipher has one.
    let nonce = extract_iv_bytes(ctx, spec);
    let counter = if is_cc20_spec {
        match ctx.get_field(spec, 1) {
            Value::Int(c) => c as u32,
            _ => 0,
        }
    } else {
        0
    };
    Ok(Some((nonce, counter)))
}

/// A digest of (key, nonce), used to remember what a Cipher instance last
/// encrypted under without keeping the key material itself around.
fn chacha20_key_nonce_fingerprint(key: &[u8], nonce: &[u8]) -> [u8; 32] {
    // `sha2` directly rather than `aot_pipeline::sha256_bytes`: that module is
    // behind `#[cfg(feature = "experimental-aot")]`, so calling it would make
    // this refusal silently vanish in a default build — an inert security check
    // that still reads as present.
    use sha2::Digest;
    let mut input = Vec::with_capacity(key.len() + nonce.len());
    input.extend_from_slice(key);
    input.extend_from_slice(nonce);
    sha2::Sha256::digest(&input).into()
}

/// Refuse a repeated (key, nonce) on a second ENCRYPT `init` OF THE SAME
/// CIPHER INSTANCE.
///
/// SunJCE's `ChaCha20Cipher` keeps the previous key and nonce on the SPI object
/// and refuses a matching re-init (measured: `InvalidKeyException: Matching key
/// and nonce from previous initialization`). The scope is the instance, not the
/// process — two independent `Cipher` objects may legitimately use the same
/// pair, and a process-wide set refuses ordinary code (it refused this change's
/// own regression vector, which encrypts under a fixed RFC nonce in several
/// tests).
///
/// The guard is worth having at all because ChaCha20 is a stream cipher:
/// encrypting two different plaintexts under one (key, nonce) emits the same
/// keystream twice, so their XOR falls out for anyone who sees both
/// ciphertexts.
///
/// DECRYPT is exempt: decrypting the same message twice is ordinary, and
/// refusing it would prevent nothing — the keystream is already determined by
/// the ciphertext the attacker has.
fn chacha20_check_nonce_reuse(
    ctx: &mut dyn NativeContext,
    algo: &str,
    mode: i32,
    key_bytes: &[u8],
    nonce: &[u8],
    previous: Option<[u8; 32]>,
) -> Result<Option<[u8; 32]>, cratonvm_types::error::MethodCallFailed> {
    if !is_chacha20_family(algo) || !(mode == 1 || mode == 3) || nonce.is_empty() {
        return Ok(previous);
    }
    let fingerprint = chacha20_key_nonce_fingerprint(key_bytes, nonce);
    if previous == Some(fingerprint) {
        return Err(crate::phases_early::throw_jca_exc(
            ctx,
            "java/security/InvalidKeyException",
            "Matching key and nonce from previous initialization",
        ));
    }
    Ok(Some(fingerprint))
}

fn cipher_init_record(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    mode: i32,
    key: ObjectRef,
    iv_bytes: Vec<u8>,
) -> MethodCallResult {
    cipher_init_record_with_counter(ctx, this, mode, key, iv_bytes, 0)
}

/// [`cipher_init_record`] with an explicit ChaCha20 block counter. Every other
/// cipher passes 0 and never reads it back.
/// The IV length a transformation needs when the caller supplied none, or
/// `None` for a transformation that takes no IV at all.
///
/// `Cipher.init(ENCRYPT_MODE, key)` on an IV-taking mode does not fail on a real
/// provider: SunJCE GENERATES a random IV and hands it back through `getIV()`
/// and `getParameters()`, and the caller is expected to persist it alongside the
/// ciphertext. This engine recorded an EMPTY IV instead, and the damage surfaced
/// two layers away — bc-java's `cms` `SunProviderTest` builds its CMS content
/// encryptor exactly that way, reads `cipher.getParameters()` (which answered
/// null), writes an `AlgorithmIdentifier` with ABSENT parameters, and the very
/// next `doFinal` died inside SunJCE's own `engineInit` with
/// `InvalidAlgorithmParameterException: Wrong IV length: must be 16 bytes long`.
///
/// The `ChaCha20` families are not here: they generate their nonce in
/// `chacha20_spec_params`, which also has to police nonce REUSE.
fn auto_generated_iv_len(algo: &str) -> Option<usize> {
    let (name, mode, _) = parse_transformation(algo);
    let block = match cipher_family(&name)? {
        CipherFamily::Aes | CipherFamily::AesFixed(_) => 16,
        CipherFamily::DesFamily | CipherFamily::Blowfish => 8,
        // ECB-only or IV-less by construction: key wrap, RSA, RC4, PBES2
        // (whose own arm derives the IV from the parameters).
        _ => return None,
    };
    match mode.as_str() {
        // A GCM nonce is 12 bytes whatever the block size — SunJCE's own
        // default, and the only length its `engineInit` will auto-generate.
        "GCM" => Some(12),
        "CBC" | "CFB" | "OFB" | "CTR" => Some(block),
        _ => None,
    }
}

fn cipher_init_record_with_counter(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    mode: i32,
    key: ObjectRef,
    iv_bytes: Vec<u8>,
    chacha_counter: u32,
) -> MethodCallResult {
    let key_bytes = extract_key_bytes(ctx, key);
    let tkey = obj_key(ctx, this);
    let algo = with_table_read(|t| {
        t.get(&tkey)
            .map(|s| s.algorithm.clone())
            .unwrap_or_default()
    });
    // An EMPTY transformation is not a transformation, and the write below is
    // `entry(tkey).or_default()` — which happily invents a state with no
    // algorithm in it and a live `mode`, so `init` reports success and the
    // first `doFinal` lands in the AES arm with nothing to dispatch on. That is
    // exactly what the relocated-`algo` defect in `cipher_alloc` produced
    // (`CipherStreamTest2`: `Unexpected exception Serpent/CBC/PKCS5Padding`,
    // `RC6/CTR/NoPadding`, and whichever other name the collector happened to
    // land on). `cipher_alloc` no longer creates such a state, and this refuses
    // to initialise one if any path ever does again: `Cipher.init` DECLARES
    // `InvalidKeyException`, but this is not a key problem, and the JDK's own
    // answer for an unusable `Cipher` is the unchecked `IllegalStateException`
    // (the same one `cipher_do_final_impl` raises for a missing state).
    //
    // Every `javax.crypto.Cipher` in this VM is minted by `cipher_alloc`, which
    // writes the transformation into the side-table before returning, so a
    // reachable caller cannot trip this.
    if algo.is_empty() {
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: "Cipher.init on a Cipher with no recorded transformation: the \
                      side-table entry is missing or empty, so no algorithm can be \
                      dispatched. Refusing to initialise it rather than computing a \
                      substitute at doFinal time."
                .into(),
        }
        .into());
    }
    // P0: `.unwrap_or_default()` here recorded an EMPTY modulus/exponent as
    // this cipher's key state — a `Cipher.init` that reported success while
    // installing no key at all. `cipher_do_final_impl` does catch the empty
    // pair, but only at `doFinal` time and as an `IllegalStateException`,
    // which is neither where nor what the JDK specifies: `Cipher.init`
    // declares `InvalidKeyException` precisely so a key the provider cannot
    // use is rejected at init. Reject it here, naming the key.
    //
    // Only RSA transformations are affected: for every other transformation
    // the pair is legitimately empty and unused.
    // The transformation's own key-length constraint, enforced where the JDK
    // enforces it. `Cipher.init` declares `InvalidKeyException`; `doFinal` does
    // not, and `Aes::key_expansion`'s failure surfaced there as an UNCHECKED
    // `IllegalArgumentException` that no `catch (GeneralSecurityException)`
    // matches — when it surfaced at all. For `AES_128/GCM/NoPadding` with a
    // 256-bit key it did not surface: the suffix was never read, so the cipher
    // ran as AES-256 under an AES-128 name.
    if let Some(reason) = key_length_reason(&algo, key_bytes.len()) {
        return Err(crate::phases_early::throw_jca_exc(
            ctx,
            "java/security/InvalidKeyException",
            &reason,
        ));
    }

    let rsa_key_id = if is_rsa_transformation(&algo) && (mode == 2 || mode == 4) {
        rsa_private_key_handle(ctx, key)
    } else {
        None
    };
    let (rsa_n, rsa_exp) = if is_rsa_transformation(&algo) {
        match rsa_key_components(ctx, key, mode) {
            Some(pair) if !pair.0.is_empty() && !pair.1.is_empty() => pair,
            _ => {
                return Err(crate::phases_early::throw_jca_exc(
                    ctx,
                    "java/security/InvalidKeyException",
                    &format!(
                        "Cipher.init({algo}): the supplied key exposes no usable RSA \
                         modulus/exponent (neither getModulus()/get*Exponent() nor a \
                         crypto_impl key handle). Refusing to initialise a cipher with \
                         no key — an empty key is not a key."
                    ),
                ));
            }
        }
    } else {
        (Vec::new(), Vec::new())
    };
    // ENCRYPT (1) and WRAP (3) with no IV supplied: mint one, as SunJCE does.
    // DECRYPT/UNWRAP has nothing to invent — the IV has to come from the
    // caller — so an empty IV there stays empty and the refusal stands.
    let iv_bytes = if iv_bytes.is_empty() && (mode == 1 || mode == 3) {
        match auto_generated_iv_len(&algo) {
            Some(n) => {
                let mut generated = vec![0u8; n];
                crate::crypto_impl::secure_random_fill(0, &mut generated);
                generated
            }
            None => iv_bytes,
        }
    } else {
        iv_bytes
    };
    let previous = with_table_read(|t| t.get(&tkey).and_then(|s| s.chacha_last_encrypt));
    let last_encrypt =
        chacha20_check_nonce_reuse(ctx, &algo, mode, &key_bytes, &iv_bytes, previous)?;
    with_table_write(|t| {
        let s = t.entry(tkey).or_default();
        s.mode = mode;
        s.key_bytes = key_bytes;
        s.iv_bytes = iv_bytes;
        s.chacha_counter = chacha_counter;
        s.chacha_last_encrypt = last_encrypt;
        s.rsa_n = rsa_n;
        s.rsa_exp = rsa_exp;
        s.rsa_key_id = rsa_key_id;
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
/// Unwrap a `java.security.AlgorithmParameters` into the
/// `AlgorithmParameterSpec` this engine reads its IV/nonce out of.
///
/// `AlgorithmParameters` and `AlgorithmParameterSpec` are the SAME parameters in
/// two containers, and every real provider converts the first into the second
/// (`engineInit` -> `engineGetParameterSpec`) before touching it. Tries the
/// spec classes an intercepted transformation can actually consume, most
/// specific first, and returns `None` when the parameters carry none of them —
/// `getParameterSpec` answers `InvalidParameterSpecException` for a class it
/// cannot produce, which is an ordinary outcome here, not a failure to report.
fn algorithm_parameters_to_spec(
    ctx: &mut dyn NativeContext,
    alg_params: ObjectRef,
    algo: &str,
) -> Option<ObjectRef> {
    let (_, mode, _) = parse_transformation(algo);
    // GCM parameters carry a tag length an `IvParameterSpec` cannot express, so
    // for a GCM transformation that class is asked for FIRST. Everything else
    // this engine computes takes a plain IV.
    let candidates: &[&str] = if mode == "GCM" {
        &[
            "javax/crypto/spec/GCMParameterSpec",
            "javax/crypto/spec/IvParameterSpec",
        ]
    } else {
        &[
            "javax/crypto/spec/IvParameterSpec",
            "javax/crypto/spec/GCMParameterSpec",
        ]
    };
    let pin = ctx.pin_native_root(alg_params);
    let mut found = None;
    for name in candidates {
        let Some(cid) = ctx.class_id_by_name(name) else {
            continue;
        };
        let mirror = ctx.get_class_mirror(cid);
        let params_now = ctx.read_native_pin(pin, alg_params);
        if let Ok(Some(Value::Object(Some(spec)))) = ctx.invoke_virtual(
            params_now,
            "getParameterSpec",
            "(Ljava/lang/Class;)Ljava/security/spec/AlgorithmParameterSpec;",
            &[Value::Object(Some(mirror))],
        ) {
            found = Some(spec);
            break;
        }
    }
    ctx.unpin_native_roots(pin);
    found
}

/// `Cipher.init(mode, key, AlgorithmParameters)` for every transformation this
/// engine computes itself.
///
/// PBES2 keeps its own arm — it needs the salt and iteration count, not just an
/// IV. Everything else used to fall through to `cipher_init_record(.., Vec::new())`,
/// which recorded an EMPTY IV and dropped the caller's parameters on the floor.
/// A CBC encrypt initialised that way ran with no IV at all and died later, at
/// `doFinal`, inside SunJCE's own `engineInit` with
/// `InvalidAlgorithmParameterException: Wrong IV length: must be 16 bytes long`
/// — reported by bc-java as `CRMFException: cannot process data: Error during
/// cipher finalisation` and `CMSException: unable to parse internal stream`,
/// because `EnvelopedDataHelper.createContentCipher` is exactly this call: it
/// rebuilds an `AlgorithmParameters` from the ASN.1 `AlgorithmIdentifier` and
/// inits with THAT, never with a spec.
fn cipher_init_from_algorithm_parameters(
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
    if pbes2_aes_params(&algo).is_some() {
        return cipher_init_record_pbes2(ctx, this, mode, key, alg_params);
    }
    // `getParameterSpec` runs real bytecode, so `this` and `key` can both move
    // under it; re-read both from their pins afterwards.
    let this_pin = ctx.pin_native_root(this);
    let key_pin = ctx.pin_native_root(key);
    let spec = alg_params.and_then(|p| algorithm_parameters_to_spec(ctx, p, &algo));
    let this = ctx.read_native_pin(this_pin, this);
    let key = ctx.read_native_pin(key_pin, key);
    ctx.unpin_native_roots(this_pin);
    if let Some((nonce, counter)) = chacha20_spec_params(ctx, &algo, spec, mode)? {
        return cipher_init_record_with_counter(ctx, this, mode, key, nonce, counter);
    }
    let iv_bytes = match spec {
        Some(spec) => extract_iv_bytes(ctx, spec),
        None => Vec::new(),
    };
    cipher_init_record(ctx, this, mode, key, iv_bytes)
}

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
                s.rsa_key_id = None;
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
    table_key: CipherKey,
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

/// The gate in front of `cipher_alloc`: admit the transformation, or raise the
/// exception the JDK specifies for it.
///
/// ## What this gate has had to learn, in order
///
/// It began as a MODE check for `AES/CCM` alone — SunJCE ships AES in
/// ECB/CBC/PCBC/CTR/CTS/CFB/OFB/GCM/KW/KWP but not CCM, and Tomcat's
/// `TestEncryptInterceptorAlgorithms` `doTestShouldNotSucceed` depends on the
/// refusal. A mode check could not see that the base ALGORITHM was never
/// questioned, so `Cipher.getInstance("CRATONVM-NO-SUCH-CIPHER")` returned a
/// fully-formed `Cipher` — the fabricated success
/// `regression-suite/src/RJdkFailure.java:309` measures. That was fixed by
/// adding an algorithm allow-list, `cipher_algorithm_known`, which was
/// deliberately over-inclusive.
///
/// Over-inclusive was the third mistake, and the largest: the names it admitted
/// but could not compute were not refused later, they were served as AES. See
/// [`classify_transformation`], which replaces it, for the measurements. The
/// gate is total now — every transformation is admitted as a named family or
/// refused, with no arm in between.
///
/// Returns the admitted [`CipherFamily`] so a caller that needs to know which
/// engine it just resolved does not have to re-derive it.
fn check_transformation_supported(
    ctx: &mut dyn NativeContext,
    algo: &str,
    form: GetInstanceForm,
) -> Result<CipherFamily, cratonvm_types::error::MethodCallFailed> {
    match classify_transformation(algo) {
        TransformVerdict::Serviceable(family) => Ok(family),
        // A malformed transformation string is rejected identically by every
        // overload — `tokenizeTransformation` runs before any provider is
        // consulted — so the message does not depend on the form.
        TransformVerdict::InvalidFormat(msg) => Err(
            crate::jca::provider_chain::throw_no_such_algorithm_public(ctx, &msg),
        ),
        TransformVerdict::NoSuchPadding(padding) => match form {
            // Measured on jdk-25.0.3.9-hotspot: the ANONYMOUS overload never
            // raises `NoSuchPaddingException` for an unavailable padding. It
            // walks the provider chain and a service that cannot take the
            // padding is simply skipped, so the loop falls out the bottom into
            // `throw new NoSuchAlgorithmException("Cannot find any provider
            // supporting " + transformation)` (`Cipher.java`). Confirmed:
            // `Cipher.getInstance("AES/CBC/CRATONVM-NO-SUCH-PADDING")` gives
            // `NoSuchAlgorithmException: Cannot find any provider supporting
            // AES/CBC/CRATONVM-NO-SUCH-PADDING`, NOT a padding exception.
            GetInstanceForm::Anonymous => {
                Err(crate::jca::provider_chain::throw_no_such_algorithm_public(
                    ctx,
                    &format!("Cannot find any provider supporting {algo}"),
                ))
            }
            // Both provider-named overloads DO, because they interrogate one
            // named provider's single service: measured
            // `NoSuchPaddingException: Padding not supported:
            // CRATONVM-NO-SUCH-PADDING` — the PADDING alone, not the whole
            // transformation.
            GetInstanceForm::WithProvider => Err(crate::phases_early::throw_jca_exc(
                ctx,
                "javax/crypto/NoSuchPaddingException",
                &format!("Padding not supported: {padding}"),
            )),
        },
        TransformVerdict::NoSuchAlgorithm => match form {
            GetInstanceForm::Anonymous => {
                Err(crate::jca::provider_chain::throw_no_such_algorithm_public(
                    ctx,
                    &format!("Cannot find any provider supporting {algo}"),
                ))
            }
            GetInstanceForm::WithProvider => {
                Err(crate::jca::provider_chain::throw_no_such_algorithm_public(
                    ctx,
                    &format!("No such algorithm: {algo}"),
                ))
            }
        },
    }
}

/// `true` when `Cipher.getInstance(transformation)` will hand back a working
/// cipher rather than refusing.
///
/// Exists so `jca::provider_chain` can assert, in a test, that every
/// transformation it ADVERTISES is one this engine can COMPUTE. The two lists
/// drifted for three waves in both directions at once — `ChaCha20` advertised
/// and served as AES, `DESede` served and never advertised — and a census that
/// is only ever run by hand drifts again. See
/// `provider_chain::every_advertised_sunjce_cipher_is_serviceable`.
pub(crate) fn transformation_is_serviceable(transformation: &str) -> bool {
    matches!(
        classify_transformation(transformation),
        TransformVerdict::Serviceable(_)
    )
}

/// Which `Cipher.getInstance` overload is asking. The two differ in their
/// REFUSAL wording and in whether an unavailable padding is reportable at all —
/// both measured, both in `check_transformation_supported`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum GetInstanceForm {
    /// `getInstance(String)` — searches the whole provider chain.
    Anonymous,
    /// `getInstance(String, String)` / `getInstance(String, Provider)` — asks
    /// one named provider.
    WithProvider,
}

/// The cipher families this engine can actually COMPUTE — the whole list.
///
/// The verdict this enum is half of replaces `cipher_algorithm_known`, whose
/// doc comment claimed being "deliberately over-inclusive on the accept side"
/// was safe because "the failure mode a too-narrow list would produce is a
/// `NoSuchAlgorithmException` for valid input, which is strictly worse than the
/// fabrication being fixed". Measurement falsified that sentence: an accepted
/// name this engine cannot compute did not fail, it produced AES. See
/// [`classify_transformation`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CipherFamily {
    /// AES with the key length taken from the KEY: `crypto_impl::Aes` /
    /// `AesGcm` in-crate for ECB and GCM, the real SunJCE
    /// `AESCipher$General` SPI (via `drive_real_cipher`) for CBC/CFB/OFB.
    Aes,
    /// `AES_128` / `AES_192` / `AES_256` — the same code paths as
    /// [`CipherFamily::Aes`], but the transformation NAME pins the key length
    /// (carried here in bytes) and `cipher_init_record` enforces it. Without
    /// that enforcement the name is decorative: measured before this change,
    /// `AES_128/GCM/NoPadding` initialised happily from a 256-bit key and
    /// encrypted with AES-256, which is the same silent-substitution defect as
    /// the ChaCha20 one, one field over.
    AesFixed(usize),
    /// RFC 3394 key wrap: `AESWrap`, `AESWrap_128/192/256`, and the canonical
    /// `AES*/KW/NoPadding` spellings. `wrap`/`unwrap`/`doFinal` only.
    AesKeyWrap,
    /// `DES` / `DESede` / `TripleDES` in CBC, through the real SunJCE
    /// `DESCipher` / `DESedeCipher` SPI.
    DesFamily,
    /// RSA through `crypto_impl`'s modexp + RFC 8017 padding.
    Rsa,
    /// PKCS#12 PBES2 (`PBEWithHmacSHA{1,224,256}AndAES_{128,256}`).
    Pbes2,
    /// RFC 8439 ChaCha20, raw stream cipher (`ChaCha20`,
    /// `ChaCha20/None/NoPadding`). `crate::chacha20`.
    ChaCha20,
    /// RFC 8439 ChaCha20-Poly1305 AEAD (`ChaCha20-Poly1305`). `crate::chacha20`.
    ///
    /// Separate from [`CipherFamily::ChaCha20`] rather than a flag on it,
    /// because the two take DIFFERENT parameter spec types, differ on whether
    /// AAD is legal, and differ on who owns the block counter — the AEAD's is
    /// fixed by RFC 8439 (0 for the one-time Poly1305 key, 1 for the
    /// ciphertext) and must not be caller-chosen.
    ChaCha20Poly1305,
    /// `Blowfish` in ECB, through the real SunJCE `BlowfishCipher` SPI.
    ///
    /// No Blowfish core is written here, on purpose. Adding one would mean a
    /// second implementation of a primitive the image already ships — the
    /// defect shape this campaign keeps finding — and it would mean restating
    /// the P-array and four S-boxes, 4168 bytes of constants derived from pi
    /// that no reviewer can check by reading. `phases_early::drive_real_cipher`
    /// already routes AES-CBC/CFB/OFB and the whole DES family to the genuine
    /// SunJCE SPI for exactly this reason; this is the same move, and its
    /// output is HotSpot's by construction rather than by comparison.
    Blowfish,
    /// `ARCFOUR`, and `RC4` which is SunJCE's alias for it, through the real
    /// SunJCE `ARCFOURCipher` SPI.
    ///
    /// RC4 is broken cryptography — RFC 7465 removed it from TLS, and it is
    /// here for parity, not as a recommendation. That is not a reason to refuse
    /// it: a workload that reads an RC4-encrypted legacy blob runs on HotSpot
    /// and failed hard here, and this VM lacking the algorithm stops nobody
    /// from using RC4 anywhere else. It IS a reason not to hand-roll it: the
    /// real SPI carries the 40..1024-bit key-length rule and the KSA/PRGA that
    /// a short reimplementation gets subtly wrong.
    Arcfour,
}

/// The two families [`drive_real_ecb_cipher`] serves, and the SunJCE SPI class,
/// `engineSetPadding` argument and `SecretKeySpec` algorithm each takes.
///
/// One table rather than three `match`es, because the three have to agree: a
/// `BlowfishCipher` driven with `"NoPadding"` and a key labelled `"AES"` is not
/// a diagnostic, it is different bytes.
///
/// The block size in bytes of the cipher `algo` names — 0 for a stream cipher
/// or an asymmetric transformation, which is what `Cipher.getBlockSize()`
/// contractually answers for those.
///
/// Split out of the `getBlockSize` registration because `getOutputSize` needs
/// the same number and was hardcoding 16. That silently over-sized every DES,
/// DESede and (as of W7-39) Blowfish and RC4 answer: a 16-byte Blowfish
/// plaintext really encrypts to 24 bytes and `getOutputSize` claimed 32.
/// Over-reporting is safe — the contract is an upper bound and a too-large
/// buffer never throws — but two functions deriving the same property from two
/// tables is how they end up disagreeing, and one of them is already the
/// documented fix for a hardcoded 16.
fn cipher_block_size(algo: &str) -> usize {
    if algo.is_empty() {
        return 0;
    }
    let (cipher_name, _mode, _pad) = parse_transformation(algo);
    match cipher_name.to_ascii_uppercase().as_str() {
        // 64-bit block ciphers.
        "DES" | "DESEDE" | "TRIPLEDES" | "BLOWFISH" | "RC2" | "IDEA" => 8,
        // Stream ciphers and asymmetric transformations have no block.
        "RC4" | "ARCFOUR" | "CHACHA20" | "CHACHA20-POLY1305" | "RSA" | "ECIES" => 0,
        // AES and everything else this module can actually service.
        _ => 16,
    }
}

/// `padded` is `parse_transformation`'s third element — true unless the caller
/// spelled `NoPadding`, which is also how a bare name arrives.
fn real_spi_ecb_route(
    family: CipherFamily,
    padded: bool,
) -> Option<(&'static str, &'static str, &'static str)> {
    match family {
        // PKCS5 unless the caller wrote NoPadding — `Blowfish` bare is
        // `Blowfish/ECB/PKCS5Padding` on SunJCE (measured: the bare name and the
        // full spelling encrypt to the same 24 bytes, `33b63e40…`, while
        // `Blowfish/ECB/NoPadding` gives 16).
        CipherFamily::Blowfish => {
            let pad = if padded { "PKCS5Padding" } else { "NoPadding" };
            Some(("com/sun/crypto/provider/BlowfishCipher", pad, "Blowfish"))
        }
        // A stream cipher takes no padding at all, and SunJCE refuses to be
        // asked for one: `Cipher.getInstance("RC4/ECB/PKCS5Padding")` raises
        // `NoSuchAlgorithmException` (measured), which is why
        // `classify_transformation` admits `NoPadding` only and this arm can
        // hardcode it. The key algorithm is `"RC4"` because that is what a
        // caller writes into `SecretKeySpec`; `ARCFOURCipher` does not read it.
        CipherFamily::Arcfour => {
            Some(("com/sun/crypto/provider/ARCFOURCipher", "NoPadding", "RC4"))
        }
        _ => None,
    }
}

/// What `Cipher.getInstance` must do with a transformation string.
enum TransformVerdict {
    /// This engine computes it, as the named family.
    Serviceable(CipherFamily),
    /// `NoSuchAlgorithmException`. Covers an unknown ALGORITHM *and* an
    /// unavailable MODE — the JDK reports both that way, because a mode lives
    /// inside the service name (`AES/KW/NoPadding`) or inside the service's
    /// `SupportedModes` attribute, and either way a miss means no service was
    /// found. Measured: `AES/CRATONVM-NO-SUCH-MODE/NoPadding` raises
    /// `NoSuchAlgorithmException`, not `NoSuchPaddingException`.
    NoSuchAlgorithm,
    /// `NoSuchPaddingException` — algorithm and mode resolve, the padding
    /// scheme does not. Carries the PADDING token, which is all HotSpot's
    /// message names.
    NoSuchPadding(String),
    /// The string is not a transformation at all. Carries HotSpot's own
    /// message; see [`tokenize_transformation`].
    InvalidFormat(String),
}

/// `javax.crypto.Cipher.tokenizeTransformation`, restated.
///
/// Quoting the specifying javadoc on every `getInstance` overload
/// (`java.base/javax/crypto/Cipher.java`, JDK 25 `src.zip`):
///
/// > `@throws NoSuchAlgorithmException` if `transformation` is `null`, empty,
/// > in an invalid format, or if no provider supports a `CipherSpi`
/// > implementation for the specified algorithm
///
/// "in an invalid format" is this function, and the four messages below are the
/// real ones, measured rather than paraphrased. `parse_transformation` — still
/// used downstream for the mode/padding of an ALREADY-ADMITTED transformation —
/// cannot do this job: it silently accepts any shape, so `"AES/CBC"` (two
/// tokens, which the JDK rejects outright) became mode `CBC` with padding
/// defaulted on, and `"AES/CBC/"` became a padded CBC cipher.
///
/// The `SHA512/2` scan is the JDK's own guard, not an embellishment: it stops
/// `PBEWithHmacSHA512/224AndAES_128` — an algorithm with a `/` inside its own
/// NAME — being split at that slash. `to_ascii_uppercase` rather than a
/// Unicode uppercase deliberately: the search index is used to slice the
/// ORIGINAL string, and only an ASCII fold is guaranteed to preserve byte
/// offsets.
///
/// Returns `(algorithm, mode, padding)`; mode and padding are `None` for the
/// algorithm-only form, and are never `Some("")` — the JDK rejects an empty
/// token rather than defaulting it.
fn tokenize_transformation(
    transformation: &str,
) -> Result<(String, Option<String>, Option<String>), String> {
    const SHA512_TRUNCATED: &str = "SHA512/2";
    let start_idx = match transformation.to_ascii_uppercase().find(SHA512_TRUNCATED) {
        Some(i) => i + SHA512_TRUNCATED.len(),
        None => 0,
    };
    let first_slash = transformation
        .get(start_idx..)
        .and_then(|tail| tail.find('/'))
        .map(|i| i + start_idx);
    let Some(first_slash) = first_slash else {
        // Algorithm-only form.
        let algo = transformation.trim();
        if algo.is_empty() {
            return Err(format!(
                "Invalid transformation: algorithm not specified-{transformation}"
            ));
        }
        return Ok((algo.to_string(), None, None));
    };
    let algo = transformation[..first_slash].trim();
    if algo.is_empty() {
        return Err(format!(
            "Invalid transformation: algorithm not specified-{transformation}"
        ));
    }
    let rest = first_slash + 1;
    let Some(second_slash) = transformation
        .get(rest..)
        .and_then(|tail| tail.find('/'))
        .map(|i| i + rest)
    else {
        return Err(format!("Invalid transformation format:{transformation}"));
    };
    let mode = transformation[rest..second_slash].trim();
    let padding = transformation[second_slash + 1..].trim();
    if mode.is_empty() || padding.is_empty() {
        return Err(format!(
            "Invalid transformation: missing mode and/or padding-{transformation}"
        ));
    }
    Ok((
        algo.to_string(),
        Some(mode.to_string()),
        Some(padding.to_string()),
    ))
}

/// Map a transformation's ALGORITHM token onto a family this engine computes.
/// `None` means "no family" — which is a refusal, not a default.
///
/// Case-insensitive because JCA lookup is (`aes/gcm/nopadding` resolves on
/// HotSpot; measured). Deliberately NOT prefix-matched: the retired
/// `cipher_algorithm_known` accepted anything starting `AES`, which is how
/// `AES_128` reached a code path with no key-length check at all.
fn cipher_family(algo: &str) -> Option<CipherFamily> {
    // PBES2 names carry no mode/padding and are matched case-sensitively by
    // `pbes2_aes_params` (they are the exact SunJCE spellings), so try that
    // table first and fall back to the case-folded match below.
    if pbes2_aes_params(algo).is_some() {
        return Some(CipherFamily::Pbes2);
    }
    match algo.to_ascii_uppercase().as_str() {
        "AES" => Some(CipherFamily::Aes),
        "AES_128" => Some(CipherFamily::AesFixed(16)),
        "AES_192" => Some(CipherFamily::AesFixed(24)),
        "AES_256" => Some(CipherFamily::AesFixed(32)),
        // `AESWrap` is SunJCE's own alias for `AES/KW/NoPadding`; the
        // size-suffixed spellings are what Keycloak/Elytron ask for.
        "AESWRAP" | "AESWRAP_128" | "AESWRAP_192" | "AESWRAP_256" => Some(CipherFamily::AesKeyWrap),
        // `TripleDES` is `Alg.Alias.Cipher.TripleDES = DESede` on SunJCE.
        "DES" | "DESEDE" | "TRIPLEDES" => Some(CipherFamily::DesFamily),
        "RSA" => Some(CipherFamily::Rsa),
        // Implemented 2026-08-11 (`crate::chacha20`, RFC 8439). These were
        // REFUSED here for one day, which was the right answer while the engine
        // had no ChaCha20: admitting the name let `cipher_do_final_impl`'s
        // mode-only dispatch run AES-256-ECB over a 32-byte key. Now that the
        // algorithms exist, admitting them is again the right answer — and the
        // dispatch checks the family rather than the mode.
        "CHACHA20" => Some(CipherFamily::ChaCha20),
        "CHACHA20-POLY1305" => Some(CipherFamily::ChaCha20Poly1305),
        // Implemented 2026-08-12 (W7-39) against the real SunJCE SPI. Same
        // history as the two ChaCha20 names above: admitted while nothing
        // computed them, which made both AES-128-ECB and byte-identical to each
        // other; refused on 08-11 because a wrong cipher is worse than a missing
        // one; admitted again now that they resolve to their OWN families and
        // `cipher_do_final_impl` keys the dispatch on the family.
        //
        // `RC4` and `ARCFOUR` land on one family deliberately — on SunJCE they
        // are one service and an alias for it, not two algorithms, and giving
        // them separate variants is how the two would drift apart.
        "BLOWFISH" => Some(CipherFamily::Blowfish),
        "RC4" | "ARCFOUR" => Some(CipherFamily::Arcfour),
        _ => None,
    }
}

/// The whole admission decision for `Cipher.getInstance`, and the reason this
/// lane exists.
///
/// ## What it replaces
///
/// The retired gate asked two questions — "is the mode CCM?" and "does the
/// algorithm name start with something plausible?" — and then threw the
/// algorithm name away. `cipher_do_final_impl` dispatched on the MODE alone,
/// and `parse_transformation` defaults an absent mode to `ECB`. Three
/// independent failures composed into one:
///
///   1. the requested algorithm was never dispatched on;
///   2. an absent mode became ECB, the one mode a caller almost never wants;
///   3. an AEAD transformation was served by a non-authenticating cipher.
///
/// Measured on this tree's own release binary against HotSpot 25, same fixed
/// 32-byte key, same 32-byte plaintext:
///
/// ```text
/// CratonVM  ChaCha20            ct = c27bed76770d7897735157e3d11726f5…
/// CratonVM  ChaCha20-Poly1305   ct = c27bed76770d7897735157e3d11726f5…
/// CratonVM  AES/ECB/PKCS5Padding ct= c27bed76770d7897735157e3d11726f5…
/// HotSpot   AES/ECB/PKCS5Padding ct= c27bed76770d7897735157e3d11726f5…
/// HotSpot   ChaCha20            ct = ed7e0180b33c00ac3e8f765cb17e83b2…
/// ```
///
/// Byte-identical: both ChaCha20 names were AES-256-ECB. A 32-byte ChaCha20 key
/// is a valid AES-256 key, so `Aes::key_expansion` succeeded rather than
/// erroring; the 12-byte nonce was discarded, output was deterministic per
/// (key, block), and `ChaCha20-Poly1305` produced no tag — decrypting a
/// ciphertext with one bit flipped returned 32 bytes of "plaintext" with no
/// authentication failure anywhere on the path. HotSpot raises
/// `AEADBadTagException: Tag mismatch` on that same input.
///
/// And it was never only ChaCha20. `Blowfish` and `RC4` — both real SunJCE
/// algorithms — produced the SAME ciphertext as each other from the same
/// 16-byte key, because both were AES-128-ECB. `AES/CBC/PKCS7Padding`, which
/// HotSpot refuses outright, was served as PKCS5. `AES/CBC/ISO10126Padding`,
/// whose padding bytes HotSpot fills at random, was served as PKCS5. Every one
/// of those is the same shape: a name accepted, then discarded.
///
/// ## The rule this function enforces
///
/// One table, matching what `provider_chain` advertises, `Option`-returning,
/// **no `_ =>` arm on the algorithm dispatch**. An algorithm this engine cannot
/// compute is refused with the exception the JDK specifies — never approximated.
/// This is the same shape `phases_late::ssl_security`'s `Mac` lane landed for
/// `mac_algorithm_supported` / `mac_compute_hmac`, and for the same reason: a
/// wrong cipher is worse than a missing one, because a caller cannot catch it.
///
/// Under-serving is the deliberate half of that trade. HotSpot's SunJCE answers
/// 60 `Cipher` names; this engine answers the families above. Refusing the rest
/// is truthful precisely because `getInstance` now refuses them — an
/// application that asks for Blowfish gets a catchable
/// `NoSuchAlgorithmException` at the call site that names the algorithm,
/// instead of AES ciphertext it will never be able to decrypt anywhere else.
fn classify_transformation(transformation: &str) -> TransformVerdict {
    // `Cipher.getInstance`'s own first line, ahead of tokenizing:
    // `if ((transformation == null) || transformation.isEmpty()) throw new
    // NoSuchAlgorithmException("Null or empty transformation")`. A null
    // argument arrives here as an empty string (`read_string` on a null ref),
    // and the JDK gives both the same message, so one arm covers both.
    if transformation.is_empty() {
        return TransformVerdict::InvalidFormat("Null or empty transformation".to_string());
    }
    let (algo, mode, padding) = match tokenize_transformation(transformation) {
        Ok(parts) => parts,
        Err(msg) => return TransformVerdict::InvalidFormat(msg),
    };
    let Some(family) = cipher_family(&algo) else {
        return TransformVerdict::NoSuchAlgorithm;
    };
    let mode_u = mode.as_deref().map(str::to_ascii_uppercase);
    let pad_u = padding.as_deref().map(str::to_ascii_uppercase);
    let named_padding = padding.clone().unwrap_or_default();

    match family {
        // ChaCha20 takes ONE optional qualified spelling and no other. SunJCE
        // accepts the bare name and `ChaCha20/None/NoPadding`, and refuses
        // `ChaCha20/ECB/NoPadding` outright (measured:
        // `NoSuchAlgorithmException: Cannot find any provider supporting
        // ChaCha20/ECB/NoPadding`). Refusing a block-cipher mode here is not
        // pedantry — a mode-only dispatch that tolerated `ECB` is exactly how
        // this family came to be AES-256-ECB.
        CipherFamily::ChaCha20 | CipherFamily::ChaCha20Poly1305 => {
            match (mode_u.as_deref(), pad_u.as_deref()) {
                (None, None) => TransformVerdict::Serviceable(family),
                (Some("NONE"), Some("NOPADDING")) => TransformVerdict::Serviceable(family),
                (Some("NONE"), Some(_)) => TransformVerdict::NoSuchPadding(named_padding),
                _ => TransformVerdict::NoSuchAlgorithm,
            }
        }
        // A default mode is a per-ALGORITHM property, never a global one. SunJCE
        // really does default a bare `AES` to `AES/ECB/PKCS5Padding` (measured:
        // HotSpot's ciphertext for `"AES"` is byte-identical to its
        // `"AES/ECB/PKCS5Padding"` ciphertext), so keeping that default here is
        // fidelity, not laxity. The defect was applying it to algorithms that
        // have no ECB mode at all.
        CipherFamily::Aes => {
            let m = mode_u.as_deref().unwrap_or("ECB");
            let p = pad_u.as_deref().unwrap_or("PKCS5PADDING");
            match m {
                // AEAD and key wrap: `NoPadding` is the only padding either
                // takes, on this engine and on SunJCE alike.
                "GCM" => aes_padding_verdict(p, &named_padding, &["NOPADDING"], family),
                // RFC 3394 takes NoPadding, and — since 2026-08-11 — PKCS5
                // as well: SunJCE's `AES/KW/PKCS5Padding` pads to a multiple of
                // EIGHT and then wraps, which is why a 16-byte payload comes
                // back as 32 bytes rather than 24.
                "KW" => aes_padding_verdict(
                    p,
                    &named_padding,
                    &["NOPADDING", "PKCS5PADDING"],
                    CipherFamily::AesKeyWrap,
                ),
                // RFC 5649, a DIFFERENT scheme: its own ICV and an explicit
                // length, not RFC 3394 with a padding bolted on. NoPadding is
                // its only spelling — the padding is intrinsic to the mode.
                "KWP" => {
                    aes_padding_verdict(p, &named_padding, &["NOPADDING"], CipherFamily::AesKeyWrap)
                }
                // ECB is in-crate; CBC/CFB/OFB drive the real SunJCE SPI. Both
                // paths implement exactly PKCS#7 (spelled PKCS5Padding by JCA)
                // and no padding — nothing else, which is why `PKCS7Padding`
                // and `ISO10126Padding` are refused rather than aliased onto
                // PKCS5.
                "ECB" | "CBC" | "CFB" | "OFB" => {
                    aes_padding_verdict(p, &named_padding, &["NOPADDING", "PKCS5PADDING"], family)
                }
                // Everything else is a mode this engine does not compute:
                // CTR, CTS, PCBC, KWP, the numbered CFB8/OFB8 variants, and
                // CCM (which SunJCE does not ship either). Each of these used
                // to reach `cipher_do_final_impl` and die there on an
                // UNCHECKED `IllegalStateException` naming "WP6.3 dispatch",
                // which no `catch (GeneralSecurityException)` matches.
                _ => TransformVerdict::NoSuchAlgorithm,
            }
        }
        // `AES_128` and friends are whole SERVICE names on SunJCE, not an
        // algorithm plus a default: measured, `Cipher.getInstance("AES_128")`
        // raises while `AES_128/CBC/NoPadding` resolves, and the advertised set
        // is exactly {CBC, CFB, ECB, GCM, KW, KWP} x NoPadding (+ KW/PKCS5Padding).
        // So the triple must be spelled in full and the padding must be
        // `NoPadding`; anything else is a missing service, not a missing
        // padding, and HotSpot answers `NoSuchAlgorithmException` accordingly.
        CipherFamily::AesFixed(_) => {
            let (Some(m), Some(p)) = (mode_u.as_deref(), pad_u.as_deref()) else {
                return TransformVerdict::NoSuchAlgorithm;
            };
            if p != "NOPADDING" {
                return TransformVerdict::NoSuchAlgorithm;
            }
            match m {
                "KW" => TransformVerdict::Serviceable(CipherFamily::AesKeyWrap),
                "ECB" | "CBC" | "CFB" | "OFB" | "GCM" => TransformVerdict::Serviceable(family),
                _ => TransformVerdict::NoSuchAlgorithm,
            }
        }
        // `AESWrap*` is an algorithm-only name. A mode/padding on it is not a
        // transformation SunJCE registers, so it is refused rather than
        // silently ignored.
        CipherFamily::AesKeyWrap => match (mode_u.as_deref(), pad_u.as_deref()) {
            (None, None) => TransformVerdict::Serviceable(family),
            _ => TransformVerdict::NoSuchAlgorithm,
        },
        // CBC and ECB, and the route table below is widened in the same change
        // — which is the condition the previous version of this arm set.
        //
        // It admitted CBC only, and said why: the route table hardcoded `"CBC"`
        // for this family whatever the caller wrote, so admitting an ECB name
        // would have served CBC under it. That reasoning was right, and the
        // answer it chose (refuse) was right while the route was fixed. Both
        // move together here: `cipher_do_final_impl` now forwards the parsed
        // mode to `engineSetMode`, so ECB is served BY ECB.
        //
        // MEASURED on HotSpot 25, SunJCE, one 8-byte block under a fixed
        // 24-byte key:
        //
        // ```text
        // DESede                    ct=61db204ee34fb78a8f45b5be23f16c4e  iv=none
        // DESede/ECB/PKCS5Padding   ct=61db204ee34fb78a8f45b5be23f16c4e  iv=none
        // DESede/ECB/NoPadding      ct=61db204ee34fb78a
        // DESede/CBC/PKCS5Padding   ct=61db204ee34fb78a942288836d960969  iv=0*8
        // ```
        //
        // The algorithm-only form is byte-identical to ECB/PKCS5Padding, which
        // is what makes `(None, None)` admissible: `parse_transformation`
        // already defaults an unspelled mode to ECB, so the bare name reaches
        // the route as ECB without a special case. `auto_generated_iv_len`
        // returns `None` for ECB, so no IV is minted and `getIV()` stays null
        // as HotSpot's does.
        //
        // One token spelled and not the other is NOT admitted: that form was
        // not measured, and an unmeasured verdict is how a wrong answer gets
        // in. Same reason `AES_128/KWP/...` is asserted neither way.
        CipherFamily::DesFamily => match (mode_u.as_deref(), pad_u.as_deref()) {
            (None, None) => TransformVerdict::Serviceable(family),
            (Some(m), Some(p)) => {
                if m != "CBC" && m != "ECB" {
                    return TransformVerdict::NoSuchAlgorithm;
                }
                if p == "NOPADDING" || p == "PKCS5PADDING" {
                    TransformVerdict::Serviceable(family)
                } else {
                    TransformVerdict::NoSuchPadding(named_padding)
                }
            }
            _ => TransformVerdict::NoSuchAlgorithm,
        },
        // `RSACipher.engineSetMode` (JDK 25 source) is
        // `if (!mode.equalsIgnoreCase("ECB")) throw new NoSuchAlgorithmException`,
        // so ECB — or the algorithm-only form, which defaults to it — is the
        // whole mode set. The padding set is whatever
        // `RsaCipherPadding::from_transformation` really implements, asked
        // directly so the gate and the computation cannot drift; `NoPadding`
        // is NOT in it, which is why `RSA/ECB/NoPadding` (serviceable on
        // HotSpot) is refused here instead of raising an unchecked
        // `IllegalStateException` at `doFinal`.
        CipherFamily::Rsa => {
            if let Some(m) = mode_u.as_deref() {
                if m != "ECB" {
                    return TransformVerdict::NoSuchAlgorithm;
                }
            }
            let padding_name = padding.as_deref().unwrap_or("PKCS1Padding");
            if crate::crypto_impl::RsaCipherPadding::from_transformation(padding_name).is_some() {
                TransformVerdict::Serviceable(family)
            } else {
                TransformVerdict::NoSuchPadding(named_padding)
            }
        }
        // The PBES2 names carry no mode or padding — the cipher and padding are
        // fixed by the PKCS#12 scheme itself (AES-CBC + PKCS#5), not chosen by
        // the caller.
        CipherFamily::Pbes2 => match (mode_u.as_deref(), pad_u.as_deref()) {
            (None, None) => TransformVerdict::Serviceable(family),
            _ => TransformVerdict::NoSuchAlgorithm,
        },
        // ECB only, and the padding set is PKCS5/NoPadding — which is a
        // NARROWER admission than SunJCE's. Measured on HotSpot 25,
        // `Blowfish/CBC/PKCS5Padding`, `Blowfish/CTR/NoPadding` and
        // `Blowfish/ECB/ISO10126Padding` all resolve and encrypt; each is
        // refused here.
        //
        // CBC and CTR are refused because `Cipher.init` in ENCRYPT_MODE with no
        // parameter spec must GENERATE a random IV and expose it through
        // `getIV()`/`getParameters()` (measured: HotSpot returned
        // `iv=0417208096327740` for a `Blowfish/CBC/PKCS5Padding` encrypt the
        // caller gave no IV), and this engine's `init` surface does not do that
        // for a family it drives per-`doFinal`. Admitting the mode and quietly
        // encrypting under an all-zero IV would be a fabricated success of the
        // exact shape W7-38 measured. ISO10126 is refused for the reason the AES
        // arm refuses it: its padding bytes are RANDOM, and serving PKCS5 in its
        // place is a substitution, not an approximation.
        //
        // `Blowfish/ECB/PKCS7Padding` and `Blowfish/None/NoPadding` are refused
        // here AND on HotSpot (measured: `NoSuchAlgorithmException: Cannot find
        // any provider supporting Blowfish/None/NoPadding`), so those two are
        // parity rather than under-service.
        CipherFamily::Blowfish => {
            let m = mode_u.as_deref().unwrap_or("ECB");
            let p = pad_u.as_deref().unwrap_or("PKCS5PADDING");
            if m != "ECB" {
                return TransformVerdict::NoSuchAlgorithm;
            }
            aes_padding_verdict(p, &named_padding, &["NOPADDING", "PKCS5PADDING"], family)
        }
        // A stream cipher: `ECB` is the only mode SunJCE's `ARCFOURCipher`
        // accepts and `NoPadding` the only padding, and both of those are
        // measured refusals rather than inferred ones —
        // `Cipher.getInstance("RC4/ECB/PKCS5Padding")`,
        // `RC4/None/NoPadding` and `RC4/CBC/NoPadding` each raise
        // `NoSuchAlgorithmException: Cannot find any provider supporting …` on
        // HotSpot 25. So this arm is parity in BOTH directions, not a narrowing.
        //
        // `ECB` naming a stream cipher's mode is nonsense on its face, and it is
        // what the JDK does: `ARCFOURCipher.engineSetMode` accepts "ECB" and
        // rejects "NONE" (measured: `NoSuchAlgorithmException: Unsupported mode
        // NONE` straight from the SPI). Matching the platform beats matching the
        // dictionary.
        CipherFamily::Arcfour => {
            let m = mode_u.as_deref().unwrap_or("ECB");
            let p = pad_u.as_deref().unwrap_or("NOPADDING");
            if m != "ECB" {
                return TransformVerdict::NoSuchAlgorithm;
            }
            // Deliberately `NoSuchAlgorithm`, not `NoSuchPadding`: HotSpot
            // answers `NoSuchAlgorithmException` for `RC4/ECB/PKCS5Padding`
            // because SunJCE's service carries no `SupportedPaddings` beyond
            // NoPadding, so the lookup finds no service at all. The two
            // exceptions are separately catchable and the JDK's choice is the
            // one to copy.
            if p != "NOPADDING" {
                return TransformVerdict::NoSuchAlgorithm;
            }
            TransformVerdict::Serviceable(family)
        }
    }
}

/// Admit `padding_upper` if it is one of `allowed`, else report the padding —
/// spelled as the caller wrote it — as unavailable. Split out so the AES arms
/// cannot disagree about which exception an unavailable padding produces.
fn aes_padding_verdict(
    padding_upper: &str,
    padding_as_written: &str,
    allowed: &[&str],
    family: CipherFamily,
) -> TransformVerdict {
    if allowed.contains(&padding_upper) {
        TransformVerdict::Serviceable(family)
    } else {
        TransformVerdict::NoSuchPadding(padding_as_written.to_string())
    }
}

/// The key length a transformation NAME pins, in bytes, or `None` when the
/// name leaves it to the key.
///
/// `AES_128` means "AES with a 128-bit key" — the size is part of the service
/// name, and SunJCE enforces it at `init` with
/// `InvalidKeyException("The key must be 16 bytes")` (measured wording). This
/// engine ignored the suffix entirely, so `AES_128/GCM/NoPadding` initialised
/// from a 256-bit key and encrypted with AES-256.
fn transformation_pinned_key_len(algo: &str) -> Option<usize> {
    match cipher_family(&tokenize_transformation(algo).ok()?.0)? {
        CipherFamily::AesFixed(n) => Some(n),
        _ => None,
    }
}

/// Why `key_len` bytes is not a usable key for `algo`, in HotSpot's own wording,
/// or `None` if it is fine.
///
/// Four rules, every one measured on jdk-25.0.3.9-hotspot:
///
/// * a size-suffixed name pins the length exactly —
///   `AES_128/GCM/NoPadding` with a 32-byte key gives
///   `InvalidKeyException: The key must be 16 bytes`;
/// * plain AES takes 16, 24 or 32 — a 17-byte key gives
///   `InvalidKeyException: Invalid AES key length: 17 bytes`;
/// * Blowfish takes up to 56 — a 57-byte key gives
///   `InvalidKeyException: Key too long (> 448 bits)`, and 3 and 4 bytes are
///   both ACCEPTED, which is why there is no lower bound here;
/// * RC4/ARCFOUR takes 5..=128 — 4 and 129 both give
///   `InvalidKeyException: Key length must be between 40 and 1024 bit`.
///
/// The last two are checked here even though the real SunJCE SPI checks them
/// again inside `engineInit`, and the duplication is the point: this engine
/// drives that SPI from `doFinal`, so without a check at `init` the exception
/// arrives from `Cipher.doFinal`, which does not declare `InvalidKeyException`
/// at all. `Cipher.init` declares it precisely so a key the provider cannot use
/// is rejected there — the same reasoning the RSA arm above records.
///
/// Families whose key length this engine does not constrain (RSA components,
/// the PBES2 password, DES/DESede parity keys handed to the real SunJCE SPI,
/// which does its own checking) return `None` and are left alone.
///
/// An EMPTY key is deliberately not reported here. It means `extract_key_bytes`
/// could not read the key at all, which is a different defect with its own
/// downstream handling; folding it in would convert that diagnosis into a
/// length complaint.
fn key_length_reason(algo: &str, key_len: usize) -> Option<String> {
    if key_len == 0 {
        return None;
    }
    if let Some(pinned) = transformation_pinned_key_len(algo) {
        return (key_len != pinned).then(|| format!("The key must be {pinned} bytes"));
    }
    match cipher_family(&tokenize_transformation(algo).ok()?.0)? {
        CipherFamily::Aes | CipherFamily::AesKeyWrap => (!matches!(key_len, 16 | 24 | 32))
            .then(|| format!("Invalid AES key length: {key_len} bytes")),
        CipherFamily::Blowfish => (key_len > 56).then(|| "Key too long (> 448 bits)".to_string()),
        CipherFamily::Arcfour => (!(5..=128).contains(&key_len))
            .then(|| "Key length must be between 40 and 1024 bit".to_string()),
        _ => None,
    }
}

/// A `java.security.AlgorithmParameters` carrying `iv`, named for `algo`'s own
/// family — what `Cipher.getParameters()` answers for a non-PBE transformation.
///
/// Returns `Ok(null)` rather than an error whenever the parameters cannot be
/// built (no IV recorded, a family with no `AlgorithmParameters` service, an
/// `init` this VM cannot service): `getParameters()` is DECLARED to return null
/// when the cipher has none, so a null is a legitimate answer and never a
/// reason to fail the caller's `init`.
fn cipher_iv_parameters(ctx: &mut dyn NativeContext, algo: &str, iv: &[u8]) -> MethodCallResult {
    if iv.is_empty() {
        return Ok(Some(Value::Object(None)));
    }
    let (name, mode, _) = parse_transformation(algo);
    let Some(family) = cipher_family(&name) else {
        return Ok(Some(Value::Object(None)));
    };
    // The service name `AlgorithmParameters` is registered under, which is the
    // ALGORITHM, not the transformation.
    let service = match family {
        CipherFamily::Aes | CipherFamily::AesFixed(_) => {
            if mode == "GCM" {
                "GCM"
            } else {
                "AES"
            }
        }
        CipherFamily::DesFamily => {
            if name.eq_ignore_ascii_case("DES") {
                "DES"
            } else {
                "DESede"
            }
        }
        CipherFamily::Blowfish => "Blowfish",
        _ => return Ok(Some(Value::Object(None))),
    };
    let service_s = ctx.create_string(service);
    let ap = ctx.invoke(
        "java/security/AlgorithmParameters",
        "getInstance",
        "(Ljava/lang/String;)Ljava/security/AlgorithmParameters;",
        &[Value::Object(Some(service_s))],
    );
    let Ok(Some(Value::Object(Some(ap_obj)))) = ap else {
        return Ok(Some(Value::Object(None)));
    };
    let ap_pin = ctx.pin_native_root(ap_obj);
    let iv_arr = make_bytes_array(ctx, iv);
    let spec = if service == "GCM" {
        // A GCM tag is 128 bits unless the caller said otherwise, and this
        // engine only computes the 128-bit one.
        ctx.new_object_initialized(
            "javax/crypto/spec/GCMParameterSpec",
            "(I[B)V",
            &[Value::Int(128), Value::Object(Some(iv_arr))],
        )
    } else {
        ctx.new_object_initialized(
            "javax/crypto/spec/IvParameterSpec",
            "([B)V",
            &[Value::Object(Some(iv_arr))],
        )
    };
    let ap_obj = ctx.read_native_pin(ap_pin, ap_obj);
    let Ok(Some(spec)) = spec else {
        ctx.unpin_native_roots(ap_pin);
        return Ok(Some(Value::Object(None)));
    };
    let init = ctx.invoke_virtual(
        ap_obj,
        "init",
        "(Ljava/security/spec/AlgorithmParameterSpec;)V",
        &[spec],
    );
    let ap_obj = ctx.read_native_pin(ap_pin, ap_obj);
    ctx.unpin_native_roots(ap_pin);
    match init {
        Ok(_) => Ok(Some(Value::Object(Some(ap_obj)))),
        Err(_) => Ok(Some(Value::Object(None))),
    }
}

/// Allocate a freshly initialised Cipher synthetic and register an empty
/// `CipherState` for it. The algorithm string is stashed in the side-table — we
/// do **not** write it to any instance field of the real JDK class, because
/// field 5 is `initialized:Z`, a primitive boolean, and storing an Object there
/// breaks the `expected object reference` invariant on read-back.
///
/// Call only after [`check_transformation_supported`] has admitted the name:
/// this function allocates unconditionally, so reaching it with an
/// unserviceable name is how a fabricated `Cipher` gets built.
///
/// The transformation arrives as a **Rust string, not the caller's `String`
/// `ObjectRef`**, and that is the whole point. This used to take the argument
/// reference and `read_string` it HERE — one line *after*
/// `try_alloc_concurrent_synthetic`, which allocates and can therefore relocate
/// it under the compacting collector. The read then landed on the vacated
/// address and `unwrap_or_default()` turned that miss into an EMPTY
/// transformation, which is how a `Cipher` came to carry `algorithm: ""` in the
/// side-table and `transformation = ""` in its own field. Nothing downstream
/// recovers from that: `cipher_do_final_impl` classifies "" as family `None`
/// and its guard fires, naming the admission table for a defect the admission
/// table had no part in (`CipherStreamTest2`, `RC6/CTR/NoPadding`).
///
/// Every caller already holds the string it read from the argument before it
/// allocated anything, so taking `&str` removes the hazard rather than pinning
/// around it. `obj` still has to be pinned across `create_string` below, for
/// the same reason.
fn cipher_alloc(
    ctx: &mut dyn NativeContext,
    algo_str: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, "javax/crypto/Cipher", 6)?;
    let pin = ctx.pin_native_root(obj);
    // Compute key outside the closure — `obj_key` borrows `ctx` and the
    // table write-guard must not depend on the ctx borrow.
    let key = obj_key(ctx, obj);
    with_table_write(|t| {
        t.insert(
            key,
            CipherState {
                algorithm: algo_str.to_string(),
                ..Default::default()
            },
        );
    });
    // `Cipher.toString()` is real JDK bytecode reading the `transformation` and
    // `provider` fields directly, and this native never wrote either — so every
    // Cipher this VM handed out printed `Cipher.null, … algorithm from: (no
    // provider)`. The transformation is known here; the provider is filled in by
    // the caller, which is the only layer that knows whether one was NAMED.
    let t_str = ctx.create_string(algo_str);
    let obj = ctx.read_native_pin(pin, obj);
    ctx.unpin_native_roots(pin);
    ctx.set_field_by_name(obj, "transformation", Value::Object(Some(t_str)));
    Ok(obj)
}

/// [`crate::jca::provider_chain::record_requested_provider`], answering with the
/// engine object's POST-call reference.
///
/// That helper builds a `Provider` — real bytecode, real allocation — so a
/// caller that goes on to hand the `Cipher` back to Java must not keep the
/// pre-call `ObjectRef`. It pins internally for its own use and drops the
/// forwarded value on the floor; this returns it.
fn record_provider_and_reread(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    provider: &str,
) -> ObjectRef {
    let pin = ctx.pin_native_root(obj);
    crate::jca::provider_chain::record_requested_provider(ctx, obj, provider);
    let obj = ctx.read_native_pin(pin, obj);
    ctx.unpin_native_roots(pin);
    obj
}

// ---------------------------------------------------------------------------
// Third-party provider delegation
// ---------------------------------------------------------------------------
//
// CratonVM implements `javax.crypto.Cipher` natively, and until 2026-08-13 the
// `(String, Provider)` and `(String, String)` overloads used that
// implementation no matter WHICH provider the caller named — the provider
// argument was only ever validated, never honoured. For the transformations
// this VM implements that is invisible (SunJCE and BouncyCastle agree on
// AES/CBC/PKCS5Padding), but for everything else it turned a working call into
// `NoSuchAlgorithmException`.
//
// Measured against HotSpot 25 with the same jars: BouncyCastle reads every one
// of netty's encrypted test keys, because `JceOpenSSLPKCS8DecryptorProviderBuilder`
// asks ITS OWN provider for the PKCS#12 PBE OID
// (`Cipher.getInstance("1.2.840.113549.1.12.1.3", bcProvider)`). On CratonVM
// that raised `NoSuchAlgorithmException`, BouncyCastle's reader returned null,
// and netty fell back to handing raw PKCS#1/PKCS#8 bytes to the JDK's own
// parser — which is where `docs/known-issues/netty`'s
// "Cannot find any provider supporting PBEWithMD5AndDES" and
// "IOException: Invalid lenByte" both came from. Neither was a missing
// algorithm; both were a provider that never got to run.
//
// SCOPE, deliberately narrow: delegation is attempted ONLY on the path where
// this VM would otherwise THROW. A transformation CratonVM can serve is still
// served by CratonVM, exactly as before, so no currently-passing call changes
// behaviour. That keeps this strictly additive — the failure mode of a bug in
// the code below is "still throws", not "silently computes something else".

/// Descriptor of `CipherSpi.engineInit(int, Key, AlgorithmParameterSpec, SecureRandom)`.
const SPI_INIT_SPEC: &str =
    "(ILjava/security/Key;Ljava/security/spec/AlgorithmParameterSpec;Ljava/security/SecureRandom;)V";
/// Descriptor of `CipherSpi.engineInit(int, Key, AlgorithmParameters, SecureRandom)`.
const SPI_INIT_PARAMS: &str =
    "(ILjava/security/Key;Ljava/security/AlgorithmParameters;Ljava/security/SecureRandom;)V";
/// Descriptor of `CipherSpi.engineInit(int, Key, SecureRandom)`.
const SPI_INIT_PLAIN: &str = "(ILjava/security/Key;Ljava/security/SecureRandom;)V";

/// Shared body of the two provider-taking `Cipher.getInstance` overloads.
///
/// A THIRD-PARTY provider the caller named by hand wins outright, even for a
/// transformation this engine could serve itself. `getInstance(t, p)` is not a
/// request for the best available implementation of `t` — it is a request for
/// `p`'s, and answering it with another one is wrong however good the answer
/// is. It had also become self-contradictory: `record_requested_provider` made
/// `getProvider()` report the name the caller asked for while the work went to
/// this crate's own AES/GCM engine, so the object described a provider it was
/// not using. That is what let `AESTest`'s GCM reuse checks fail — BouncyCastle
/// refuses a second `doFinal` and a repeated nonce, this engine had no such
/// guard, and the `Cipher` said "BC" throughout.
///
/// "Third-party" is `provider_chain::third_party_service_class`'s own test: a
/// provider outside `NATIVELY_SERVICED_PROVIDERS` that really registers the
/// service. Naming `SunJCE` or another provider this VM implements natively
/// still takes the native path, which is the same implementation by a
/// different route.
///
/// Otherwise this VM's own implementation is preferred whenever it can serve
/// the transformation, and only when it CANNOT does the named provider get a
/// turn; if that provider does not own the service either, the original
/// refusal is raised unchanged.
/// Resolve an ALIAS spelling onto the transformation the provider's own table
/// names, before `classify_transformation` ever sees it.
///
/// Seeding `Alg.Alias.Cipher.<oid>` is necessary and not sufficient. The alias
/// table gets a caller past `check_provider_ownership`, which reads
/// `get_service_entry`; this engine then decides what it can COMPUTE from the
/// transformation STRING, and `2.16.840.1.101.3.4.1.42` does not parse as
/// `AES_256/CBC/NoPadding` however many registry rows point at it.
///
/// MEASURED: with the 264 measured alias rows seeded and nothing else, 212 of
/// them resolved and 52 did not — 43 Cipher, 6 KeyAgreement, 3 KEM. Those are
/// exactly the engines that gate on a hand-written name table without asking
/// the registry first, which is what this closes for Cipher.
///
/// Returns `None` when the name is already one this engine recognises, so a
/// spelled-out transformation never takes a registry lookup.
fn canonical_transformation(provider: Option<&str>, algo: &str) -> Option<String> {
    crate::jca::provider_chain::canonical_if_unrecognised(provider, "Cipher", algo, &|name| {
        !matches!(
            classify_transformation(name),
            TransformVerdict::NoSuchAlgorithm
        )
    })
}

fn cipher_get_instance_with_provider(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    algo_str: &str,
) -> MethodCallResult {
    let requested_provider = crate::jca::provider_chain::provider_arg_name(ctx, args, 1);
    // An alias spelling becomes the transformation it names here, once,
    // ahead of every reader below. See `canonical_transformation`.
    let canonical = canonical_transformation(requested_provider.as_deref(), algo_str);
    let algo_str: &str = canonical.as_deref().unwrap_or(algo_str);
    if let Some(provider) = requested_provider.as_deref() {
        // Asked about EVERY name the transformation may be registered under,
        // not just the bare algorithm: a provider may own only the fuller form
        // (`GOST3412-2015/CFB8`). See `cipher_transform_candidates`.
        if cipher_transform_candidates(algo_str)
            .into_iter()
            .any(|(service, _, _)| {
                crate::jca::provider_chain::third_party_service_class(
                    Some(provider),
                    "Cipher",
                    &service,
                )
                .is_some()
            })
        {
            let mut obj = cipher_alloc(ctx, algo_str)?;
            // An `Err` here is the named provider refusing its own service's
            // mode or padding, which is exactly what HotSpot surfaces from
            // `Transform.setModePadding`. It must not be swallowed in favour of
            // this engine's answer — see `try_delegate_cipher_to_named_provider`.
            if try_delegate_cipher_to_named_provider(ctx, provider, algo_str, &mut obj)? {
                let obj = record_provider_and_reread(ctx, obj, provider);
                return Ok(Some(Value::Object(Some(obj))));
            }
        }
    }
    match check_transformation_supported(ctx, algo_str, GetInstanceForm::WithProvider) {
        Ok(_) => {
            let mut obj = cipher_alloc(ctx, algo_str)?;
            if let Some(provider) = requested_provider.as_deref() {
                obj = record_provider_and_reread(ctx, obj, provider);
            }
            Ok(Some(Value::Object(Some(obj))))
        }
        Err(refusal) => {
            // The provider comes from the NAME resolved at the top of this
            // function, not from an `args[1]` re-read here: `cipher_alloc`
            // allocates, so every `ObjectRef` in `args` is potentially stale
            // from this point on. `requested_provider` is a `String` and is not.
            let mut obj = cipher_alloc(ctx, algo_str)?;
            let delegated = match requested_provider.as_deref() {
                Some(provider) => {
                    try_delegate_cipher_to_named_provider(ctx, provider, algo_str, &mut obj)?
                }
                None => false,
            };
            match delegated {
                true => {
                    if let Some(provider) = requested_provider.as_deref() {
                        obj = record_provider_and_reread(ctx, obj, provider);
                    }
                    Ok(Some(Value::Object(Some(obj))))
                }
                false => Err(refusal),
            }
        }
    }
}

/// Put a delegate SPI's `byte[]` answer into the caller's output `ByteBuffer`
/// and report how many bytes were written — the `(ByteBuffer, ByteBuffer)`
/// overloads' return contract.
///
/// Writes through the buffer's own `put(byte[])` so heap and DIRECT buffers
/// take the same path, and so the output buffer's position advances the way
/// `Cipher.update`/`doFinal` promise.
fn cipher_put_result_into_buffer(
    ctx: &mut dyn NativeContext,
    output: Option<Value>,
    result: Option<Value>,
) -> MethodCallResult {
    let out_bytes = match result {
        Some(Value::Object(Some(a))) => read_bytes(ctx, a),
        _ => Vec::new(),
    };
    if out_bytes.is_empty() {
        return Ok(Some(Value::Int(0)));
    }
    let Some(Value::Object(Some(output))) = output else {
        return Err(RuntimeError::NullPointerException {
            message: Some("output ByteBuffer is null".to_string()),
        }
        .into());
    };
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, out_bytes.len());
    ctx.write_byte_array_from(arr, 0, &out_bytes);
    ctx.invoke_virtual(
        output,
        "put",
        "([B)Ljava/nio/ByteBuffer;",
        &[Value::Object(Some(arr))],
    )?;
    Ok(Some(Value::Int(out_bytes.len() as i32)))
}

/// Is this `Cipher` a wrapper over a third-party provider's `CipherSpi`?
fn cipher_is_delegated(ctx: &mut dyn NativeContext, this: ObjectRef) -> bool {
    let key = obj_key(ctx, this);
    with_table_read(|t| t.get(&key).is_some_and(|s| s.delegated))
}

/// The delegate `CipherSpi`, read back from the `Cipher`'s own `spi` field.
/// The object at `idx` in a native call's argument list, or `None` for a null
/// or absent slot. The same three-line `match` this file spells inline in a
/// dozen places.
fn obj_at(args: &[Value], idx: usize) -> Option<ObjectRef> {
    match args.get(idx) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    }
}

fn cipher_delegate_spi(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    match ctx.get_field_by_name(this, "spi") {
        Value::Object(Some(spi)) => Some(spi),
        _ => None,
    }
}

/// The provider name this crate's own `Cipher` engine answers as.
///
/// Every `getProvider()` on a natively-served `Cipher` reports this, so it is
/// also this engine's POSITION in the installed chain for the purpose of the
/// anonymous `getInstance` — see `provider_chain::third_party_owner_before`.
const CIPHER_NATIVE_PROVIDER: &str = "SunJCE";

/// The service names `Cipher.getInstance` tries for one transformation, in the
/// JDK's own order, each paired with the mode and padding that form still has
/// to configure by hand.
///
/// `Cipher` does not simply strip the mode and padding off and configure the
/// bare algorithm: `getTransforms` builds FOUR candidates for `alg/mode/pad`
/// and the bare-algorithm one is the LAST. A provider is entitled to register
/// the fuller forms as distinct services with different classes, and
/// BouncyCastle does — `Cipher.GOST3412-2015` is `$ECB` while
/// `Cipher.GOST3412-2015/CFB8` is `$GCFB8`, a different cipher whose IV
/// register is 32 bytes rather than 16. Asking only for the bare name and then
/// calling `engineSetMode("CFB8")` builds a GENERIC CFB, which refused the
/// published test vector's 32-byte IV with
/// `InvalidAlgorithmParameterException: IV must be 16 bytes long`
/// (`GOST3412Test.testCFB`, which HotSpot passes).
fn cipher_transform_candidates(algo: &str) -> Vec<(String, Option<&str>, Option<&str>)> {
    let parts: Vec<&str> = algo.split('/').collect();
    if parts.len() == 3 {
        vec![
            (algo.to_string(), None, None),
            (format!("{}/{}", parts[0], parts[1]), None, Some(parts[2])),
            (format!("{}//{}", parts[0], parts[2]), Some(parts[1]), None),
            (parts[0].to_string(), Some(parts[1]), Some(parts[2])),
        ]
    } else {
        vec![(algo.to_string(), None, None)]
    }
}

/// Is this refusal one `Cipher.getInstance` is allowed to hand back unchanged?
///
/// Only the two checked exceptions `CipherSpi.engineSetMode`/`engineSetPadding`
/// declare. Everything else — a `NumberFormatException` out of a provider
/// parsing a mode name, say — is a provider failure the JDK converts into
/// "No such algorithm"; see the call site.
fn cipher_refusal_is_declared(ctx: &mut dyn NativeContext, refusal: &MethodCallFailed) -> bool {
    let MethodCallFailed::ExceptionThrown(exc) = refusal else {
        return false;
    };
    let mut class_id = ctx.class_id_of_object(*exc);
    loop {
        match ctx.class_name_arc_of_id(class_id).as_deref() {
            Some("java/security/NoSuchAlgorithmException")
            | Some("javax/crypto/NoSuchPaddingException") => return true,
            Some("java/lang/Throwable") | None => return false,
            _ => {}
        }
        match ctx.superclass_of(class_id) {
            Some(parent) => class_id = parent,
            None => return false,
        }
    }
}

/// Ask ONE named provider to serve `algo`, and on success turn `cipher_obj`
/// into a wrapper over its SPI.
/// `cipher_obj` is taken by `&mut` and REWRITTEN before this returns. Every
/// step below runs the provider's own bytecode — `build_jca_impl`
/// instantiates its SPI class, `engineSetMode`/`engineSetPadding` are ordinary
/// virtual calls — so the `Cipher` can relocate underneath this frame, and
/// both the `spi` field write at the end and the caller's eventual return value
/// would otherwise use its vacated address. The `spi` root was already pinned
/// here; the `Cipher` itself was the one that was not.
fn try_delegate_cipher_to_named_provider(
    ctx: &mut dyn NativeContext,
    provider: &str,
    algo: &str,
    cipher_obj: &mut ObjectRef,
) -> Result<bool, MethodCallFailed> {
    let cipher_pin = ctx.pin_native_root(*cipher_obj);
    // Every service name this transformation may be registered under, most
    // specific first — see `cipher_transform_candidates`. `owned` records that
    // the provider claimed at least one of them, which is what separates "not
    // this provider's algorithm" (the caller's own refusal stands) from "this
    // provider's algorithm, and it refused" (its refusal stands).
    let mut refusal: Option<MethodCallFailed> = None;
    let mut owned = false;
    let mut installed: Option<ObjectRef> = None;
    for (service, mode, padding) in cipher_transform_candidates(algo) {
        let Some(result) =
            crate::jca::provider_chain::build_jca_impl(ctx, provider, "Cipher", &service)
        else {
            continue;
        };
        owned = true;
        let spi = match result {
            Ok(Some(Value::Object(Some(spi)))) => spi,
            // The provider owns the name but its class would not instantiate.
            // Remember it and try the next form, exactly as `createCipher`'s
            // `catch (Exception)` does, rather than reporting our own answer —
            // "the provider you named is broken" is a different fact from "no
            // such algorithm".
            Err(e) => {
                refusal = Some(e);
                continue;
            }
            _ => continue,
        };
        // Mode and padding, only where this form still has to set them. Both
        // are `protected` on `CipherSpi` and both may legitimately refuse, in
        // which case this form cannot serve the transformation and the next
        // one gets a turn.
        let pin = ctx.pin_native_root(spi);
        let mut this_refusal: Option<MethodCallFailed> = None;
        for (arg, method) in [(mode, "engineSetMode"), (padding, "engineSetPadding")] {
            let Some(arg) = arg else {
                continue;
            };
            let spi_now = ctx.read_native_pin(pin, spi);
            let arg = ctx.create_string(arg);
            if let Err(e) = ctx.invoke_virtual(
                spi_now,
                method,
                "(Ljava/lang/String;)V",
                &[Value::Object(Some(arg))],
            ) {
                this_refusal = Some(e);
                break;
            }
        }
        let spi = ctx.read_native_pin(pin, spi);
        ctx.unpin_native_roots(pin);
        if let Some(e) = this_refusal {
            refusal = Some(e);
            continue;
        }
        installed = Some(spi);
        break;
    }
    *cipher_obj = ctx.read_native_pin(cipher_pin, *cipher_obj);
    ctx.unpin_native_roots(cipher_pin);
    if installed.is_none() {
        if !owned {
            return Ok(false);
        }
        let Some(refusal) = refusal else {
            return Ok(false);
        };
        // The provider OWNS the algorithm and refused the mode or the padding.
        // `Cipher.getInstance(t, provider)` lets a DECLARED refusal propagate —
        // measured on HotSpot 25, `AES/EAX/PKCS5Padding` with BouncyCastle is
        // `NoSuchPaddingException: Only NoPadding can be used with AEAD modes.`,
        // the PROVIDER's own message. Swallowing that and reporting this
        // engine's "No such algorithm" instead named the wrong layer and the
        // wrong defect.
        //
        // Anything else the provider throws is not a refusal, it is a provider
        // failing to parse the string, and the JDK does not let it out:
        // `createCipher` runs `setModePadding` inside a `catch (Exception)` and
        // ends the loop with `NoSuchAlgorithmException: No such algorithm: <t>`.
        // BouncyCastle's `engineSetMode` reads the bit count off a `CFB`/`OFB`
        // mode name with `Integer.parseInt`, so `AES/CFBNOT_REAL/NoPadding`
        // raises `NumberFormatException: For input string: "NOT_REAL"` — which
        // this returned verbatim, from a method declaring neither
        // (`BlockCipherTest.testIncorrectCipherModes`, index 6).
        //
        // The chain walk in `try_delegate_cipher_to_chain` ignores an `Err` and
        // moves to the next provider, which is what the anonymous overload
        // wants, so the conversion is done here rather than there.
        if !cipher_refusal_is_declared(ctx, &refusal) {
            return Err(crate::phases_early::throw_jca_exc(
                ctx,
                "java/security/NoSuchAlgorithmException",
                &format!("No such algorithm: {algo}"),
            ));
        }
        return Err(refusal);
    }
    let spi = installed.expect("installed is Some on this path");
    // The SPI lives in the Java-visible `spi` field so the collector owns it.
    ctx.set_field_by_name(*cipher_obj, "spi", Value::Object(Some(spi)));
    let key = obj_key(ctx, *cipher_obj);
    with_table_write(|t| {
        if let Some(state) = t.get_mut(&key) {
            state.delegated = true;
        }
    });
    Ok(true)
}

/// Walk the installed provider chain, in order, for a transformation this
/// engine cannot compute — exactly what `Cipher.getInstance(String)` does on a
/// real JDK, and what the ANONYMOUS overload here never did.
///
/// The refusal it replaces was structural, not a missing algorithm: this VM's
/// `getInstance(String)` consulted a hand-written serviceable-transformation
/// table and threw `NoSuchAlgorithmException: Cannot find any provider
/// supporting <name>` if the name was not on it, without asking a single
/// installed provider. Every X.509/PKCS/CMS caller names its content cipher by
/// OID and lets the chain resolve it, so bc-java's `cms` and `pkcs` suites hit
/// this on `1.2.840.113549.1.12.1.3` (PKCS#12 PBEWithSHAAnd3KeyTripleDES) and
/// `2.16.840.1.101.3.4.4.1` (ML-KEM-512) while HotSpot served both from
/// BouncyCastle.
///
/// Deliberately ordered AFTER this engine's own verdict, never before it: a
/// name this VM can compute keeps being computed here, so no existing caller's
/// answer changes. Only the refusal path is reachable from here.
fn try_delegate_cipher_to_chain(
    ctx: &mut dyn NativeContext,
    algo: &str,
    cipher_obj: &mut ObjectRef,
) -> Result<bool, MethodCallFailed> {
    let pin = ctx.pin_native_root(*cipher_obj);
    for provider in crate::jca::provider_chain::chain_provider_names() {
        let mut obj = ctx.read_native_pin(pin, *cipher_obj);
        // A provider that owns the name but whose class will not instantiate is
        // reported by `try_delegate_cipher_to_named_provider` as `Err`. That is
        // one provider's problem, not the chain's — a real `ProviderList` walk
        // moves on to the next candidate — so keep looking.
        if let Ok(true) = try_delegate_cipher_to_named_provider(ctx, &provider, algo, &mut obj) {
            // Record WHICH provider answered. `Cipher.getProvider()` reports
            // this engine's own identity unless told otherwise, so a chain walk
            // that found a third-party SPI produced a working cipher that named
            // the wrong provider — `SlotTwoTest` decrypts correctly and then
            // fails on `decrypt.getProvider().getName()`, expecting `BC` and
            // getting `SunJCE`. The named-provider overloads have always
            // recorded it; the anonymous chain walk did not.
            let obj = ctx.read_native_pin(pin, obj);
            let obj = record_provider_and_reread(ctx, obj, &provider);
            *cipher_obj = ctx.read_native_pin(pin, obj);
            ctx.unpin_native_roots(pin);
            return Ok(true);
        }
    }
    *cipher_obj = ctx.read_native_pin(pin, *cipher_obj);
    ctx.unpin_native_roots(pin);
    Ok(false)
}

/// Forward `Cipher.init` to the delegate SPI.
///
/// The caller's `SecureRandom` is forwarded, not dropped. Passing null here
/// ("every `engineInit` treats null as use-the-provider's-default") is only
/// harmless for callers that never draw from it — and the ones that DO are
/// exactly the ones that can tell: bc-java's `AESTest.wrapTest(2, ...)` inits an
/// `AESRFC3211WRAP` cipher with a `FixedSecureRandom` and asserts a FIXED
/// ciphertext, and with the argument dropped BouncyCastle drew its IV from its
/// own default source and produced a different answer on every run. A null
/// argument is still passed through as null, which is what a JDK caller of
/// `init(mode, key)` gets.
/// Which `Cipher.init` overload the caller used — which is NOT the same
/// question as whether it passed a non-null parameter object.
///
/// `Cipher.init(int, Key, AlgorithmParameterSpec, SecureRandom)` calls
/// `engineInit(opmode, key, params, random)` even when `params` is null, and a
/// provider is entitled to answer that call differently from the three-argument
/// one. BouncyCastle does: its `engineInit(int, Key, SecureRandom)` is a
/// wrapper that catches `InvalidAlgorithmParameterException` and rethrows it as
/// `InvalidKeyException`. So routing a null spec to the three-argument form
/// turned `PBEKey requires parameters to specify salt` from the
/// `InvalidAlgorithmParameterException` the caller catches into an
/// `InvalidKeyException` that sails past the handler — `PBETest.testNullSalt`,
/// which passes `(AlgorithmParameterSpec)null` on purpose.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CipherInitParams {
    /// `init(int, Key)` / `init(int, Key, SecureRandom)`.
    None,
    /// `init(int, Key, AlgorithmParameterSpec[, SecureRandom])`.
    Spec,
    /// `init(int, Key, AlgorithmParameters[, SecureRandom])`.
    Params,
}

fn cipher_delegate_init(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    mode: i32,
    key: Option<ObjectRef>,
    params: Option<ObjectRef>,
    params_kind: CipherInitParams,
    random: Option<ObjectRef>,
) -> MethodCallResult {
    let Some(spi) = cipher_delegate_spi(ctx, this) else {
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: "Cipher was marked as provider-delegated but carries no SPI".to_string(),
        }
        .into());
    };
    // Every reference below survives the (allocating) `SecureRandom`
    // construction: pin them before it runs.
    let spi_pin = ctx.pin_native_root(spi);
    let key_pin = key.map(|k| ctx.pin_native_root(k));
    let params_pin = params.map(|p| ctx.pin_native_root(p));
    let key_v = Value::Object(key);
    // `Cipher.init(mode, key)` does NOT pass a null `SecureRandom` on a real
    // JDK — it passes `JCAUtil.getSecureRandom()`. A provider is entitled to
    // dereference it: BouncyCastle's `BaseWrapCipher.engineInit` wraps the
    // argument in a `ParametersWithRandom` and `RFC3211WrapEngine` calls
    // `nextBytes` on it, which is `NullPointerException: Cannot invoke
    // "java.security.SecureRandom.nextBytes(byte[])"` on every one of
    // bc-java's `RFC3211WrapTest` cases when the argument is null.
    let random = match random {
        Some(r) => Some(r),
        None => match ctx.new_object_initialized("java/security/SecureRandom", "()V", &[]) {
            Ok(Some(Value::Object(Some(r)))) => Some(r),
            _ => None,
        },
    };
    let random_v = Value::Object(random);
    // The parameter slot is filled from the OVERLOAD, not from whether the
    // object is null — see `CipherInitParams`.
    let args: Vec<Value> = match params_kind {
        CipherInitParams::None => vec![Value::Int(mode), key_v, random_v],
        _ => vec![Value::Int(mode), key_v, Value::Object(params), random_v],
    };
    let desc = match params_kind {
        CipherInitParams::None => SPI_INIT_PLAIN,
        CipherInitParams::Spec => SPI_INIT_SPEC,
        CipherInitParams::Params => SPI_INIT_PARAMS,
    };
    // Re-read every argument from its pin: `new SecureRandom()` above may have
    // moved them.
    let spi = ctx.read_native_pin(spi_pin, spi);
    let mut args = args;
    if let (Some(pin), Some(k)) = (key_pin, key) {
        args[1] = Value::Object(Some(ctx.read_native_pin(pin, k)));
    }
    if let (Some(pin), Some(p)) = (params_pin, params) {
        args[2] = Value::Object(Some(ctx.read_native_pin(pin, p)));
    }
    let result = ctx.invoke_virtual(spi, "engineInit", desc, &args);
    ctx.unpin_native_roots(spi_pin);
    result?;
    Ok(None)
}

/// Forward a byte-array-in / byte-array-out `Cipher` call to the delegate SPI.
/// `CipherSpi.engineUpdate`/`engineDoFinal(byte[], int, int, byte[], int)` —
/// the write-into-the-caller's-buffer form, which is exactly what
/// `javax.crypto.Cipher` invokes for the matching `update`/`doFinal`
/// overloads on a real provider.
///
/// The array-returning `([BII)[B` form cannot stand in for it. It CONSUMES the
/// input and hands back a fresh array, so the output buffer is only measured
/// afterwards — and a `ShortBufferException` raised at that point leaves the
/// cipher state already advanced, with the produced block still to come. The
/// next `update` then emits it as a spurious leading block: measured as
/// `BlockCipherTest` index 6, "update DES failed decryption", whose `got` is
/// the expected stream with one extra 8-byte block in front. Providers own
/// this decision and make it BEFORE processing — BouncyCastle's
/// `BaseBlockCipher.engineUpdate` refuses on
/// `outputOffset + getUpdateOutputSize(inputLen) > output.length` — so calling
/// their own method both keeps the state clean and reports the refusal in
/// their own words, instead of this file re-deciding it one step too late.
fn cipher_delegate_into_buffer(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    engine_method: &str,
    input: Option<ObjectRef>,
    off: i32,
    len: i32,
    output: ObjectRef,
    out_off: i32,
) -> MethodCallResult {
    let Some(spi) = cipher_delegate_spi(ctx, this) else {
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: "Cipher was marked as provider-delegated but carries no SPI".to_string(),
        }
        .into());
    };
    ctx.invoke_virtual(
        spi,
        engine_method,
        "([BII[BI)I",
        &[
            Value::Object(input),
            Value::Int(off),
            Value::Int(len),
            Value::Object(Some(output)),
            Value::Int(out_off),
        ],
    )
}

fn cipher_delegate_bytes(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    engine_method: &str,
    input: Option<ObjectRef>,
    off: i32,
    len: i32,
) -> MethodCallResult {
    let Some(spi) = cipher_delegate_spi(ctx, this) else {
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: "Cipher was marked as provider-delegated but carries no SPI".to_string(),
        }
        .into());
    };
    ctx.invoke_virtual(
        spi,
        engine_method,
        "([BII)[B",
        &[Value::Object(input), Value::Int(off), Value::Int(len)],
    )
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
///
/// **Post-admission only.** This is a lenient splitter, not a validator: it
/// accepts any shape, defaults an absent mode to `ECB` and an absent padding to
/// "padded". Those defaults are correct for the AES family (SunJCE really does
/// read a bare `AES` as `AES/ECB/PKCS5Padding`) and wrong for everything else,
/// which is how a nameless-mode `ChaCha20` became AES-ECB. Call it only on a
/// transformation [`classify_transformation`] has already admitted;
/// [`tokenize_transformation`] is the validating one.
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

/// Whether `algo` names SunJCE's raw `ChaCha20` stream cipher.
///
/// SunJCE accepts the bare name and `ChaCha20/None/NoPadding`, and REFUSES
/// `ChaCha20/ECB/NoPadding` at `getInstance` (measured on OpenJDK 25.0.4:
/// `NoSuchAlgorithmException: Cannot find any provider supporting
/// ChaCha20/ECB/NoPadding`). The `ECB` spelling is rejected rather than
/// tolerated because tolerating it is how this engine got here: a mode-only
/// dispatch that defaulted to ECB is what turned ChaCha20 into AES-256-ECB.
fn is_chacha20_transformation(algo: &str) -> bool {
    let (name, _, _) = parse_transformation(algo);
    matches!(cipher_family(&name), Some(CipherFamily::ChaCha20))
}

/// Whether `algo` names SunJCE's `ChaCha20-Poly1305` AEAD.
fn is_chacha20_poly1305_transformation(algo: &str) -> bool {
    let (name, _, _) = parse_transformation(algo);
    matches!(cipher_family(&name), Some(CipherFamily::ChaCha20Poly1305))
}

/// Either ChaCha20 form — the gate that keeps these transformations away from
/// `Aes::key_expansion`, which would otherwise ACCEPT a 32-byte ChaCha20 key as
/// a valid AES-256 key and encrypt with the wrong algorithm.
fn is_chacha20_family(algo: &str) -> bool {
    is_chacha20_transformation(algo) || is_chacha20_poly1305_transformation(algo)
}

/// Whether `algo` names the RFC 3394 AES Key Wrap cipher supplied by SunJCE.
/// `AESWrap` and the size-specific `AESWrap_128` aliases are used by
/// Keycloak/Elytron; JDK callers may also use the canonical
/// `AES/KW/NoPadding` transformation.
/// Asked of the ONE admission table rather than restated as a second literal
/// list. The old list carried `"AESWRAP128"` (no underscore) and `"AES/KW"`
/// (two tokens) — neither of which is a transformation `Cipher.getInstance`
/// accepts — while omitting `AES_128/KW/NoPadding`, which SunJCE advertises and
/// this engine can serve. A predicate that disagrees with the gate in front of
/// it is how `wrap()` and `doFinal()` came to answer differently for the same
/// cipher.
fn is_aes_key_wrap_transformation(algo: &str) -> bool {
    matches!(
        classify_transformation(algo),
        TransformVerdict::Serviceable(CipherFamily::AesKeyWrap)
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
        // SunJCE's wording, not ours — see `AES_KW_LENGTH_REFUSAL`.
        return Err(AES_KW_LENGTH_REFUSAL.to_string());
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

/// Append `bytes` to the receiver's `doFinal` accumulator.
fn accumulate_bytes(ctx: &mut dyn NativeContext, this: ObjectRef, bytes: &[u8]) {
    let tkey = obj_key(ctx, this);
    with_table_write(|t| {
        if let Some(s) = t.get_mut(&tkey) {
            s.accumulated.extend_from_slice(bytes);
        }
    });
}

/// Turn unwrapped key material into a `Key` of the requested `Cipher` type,
/// exactly as `javax.crypto.CipherSpi.engineUnwrap` does: `SECRET_KEY` (3) is a
/// `SecretKeySpec`, `PUBLIC_KEY` (1) and `PRIVATE_KEY` (2) go back through a
/// `KeyFactory` for the named algorithm with the standard X.509 / PKCS#8 specs.
fn build_unwrapped_key(
    ctx: &mut dyn NativeContext,
    plain: &[u8],
    key_algorithm: &str,
    key_type: i32,
) -> MethodCallResult {
    if key_algorithm.is_empty() {
        return Err(crate::phases_early::throw_jca_exc(
            ctx,
            "java/security/InvalidKeyException",
            "Unwrapped key algorithm must not be empty",
        ));
    }
    let key_bytes = make_bytes_array(ctx, plain);
    let pin = ctx.pin_native_root(key_bytes);
    let algo = ctx.create_string(key_algorithm);
    let key_bytes = ctx.read_native_pin(pin, key_bytes);
    if key_type == 3 {
        let result = ctx.new_object_initialized(
            "javax/crypto/spec/SecretKeySpec",
            "([BLjava/lang/String;)V",
            &[Value::Object(Some(key_bytes)), Value::Object(Some(algo))],
        );
        ctx.unpin_native_roots(pin);
        return result;
    }
    let (spec_class, kf_method, kf_desc) = if key_type == 1 {
        (
            "java/security/spec/X509EncodedKeySpec",
            "generatePublic",
            "(Ljava/security/spec/KeySpec;)Ljava/security/PublicKey;",
        )
    } else {
        (
            "java/security/spec/PKCS8EncodedKeySpec",
            "generatePrivate",
            "(Ljava/security/spec/KeySpec;)Ljava/security/PrivateKey;",
        )
    };
    let spec = ctx.new_object_initialized(spec_class, "([B)V", &[Value::Object(Some(key_bytes))]);
    ctx.unpin_native_roots(pin);
    let spec = match spec? {
        Some(Value::Object(Some(o))) => o,
        _ => {
            return Err(crate::phases_early::throw_jca_exc(
                ctx,
                "java/security/InvalidKeyException",
                "Cannot build a key spec for the unwrapped material",
            ))
        }
    };
    let spec_pin = ctx.pin_native_root(spec);
    let algo2 = ctx.create_string(key_algorithm);
    let kf = ctx.invoke(
        "java/security/KeyFactory",
        "getInstance",
        "(Ljava/lang/String;)Ljava/security/KeyFactory;",
        &[Value::Object(Some(algo2))],
    );
    let spec = ctx.read_native_pin(spec_pin, spec);
    ctx.unpin_native_roots(spec_pin);
    match kf? {
        Some(Value::Object(Some(kf))) => {
            ctx.invoke_virtual(kf, kf_method, kf_desc, &[Value::Object(Some(spec))])
        }
        _ => Err(crate::phases_early::throw_jca_exc(
            ctx,
            "java/security/NoSuchAlgorithmException",
            &format!("no KeyFactory for unwrapped key algorithm {key_algorithm}"),
        )),
    }
}

/// Native implementation of `Cipher.wrap(Key)`.  The JVM's real `Cipher`
/// bytecode cannot be used because native `init` stores state in our side
/// table, not in the JDK object's private `spi` and `initialized` fields.
fn cipher_wrap_impl(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    key_to_wrap: ObjectRef,
) -> MethodCallResult {
    // A Cipher wrapping a third-party `CipherSpi` wraps through IT. Without
    // this the RFC 3394 gate below refused every provider-supplied wrap
    // transformation this VM does not implement itself — with an
    // `IllegalStateException` naming our own limitation, on a cipher whose
    // provider implements the algorithm perfectly well.
    if cipher_is_delegated(ctx, this) {
        if let Some(spi) = cipher_delegate_spi(ctx, this) {
            return ctx.invoke_virtual(
                spi,
                "engineWrap",
                "(Ljava/security/Key;)[B",
                &[Value::Object(Some(key_to_wrap))],
            );
        }
    }
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
        // Every other transformation wraps the JDK's own way, which
        // `javax.crypto.CipherSpi.engineWrap` spells out in three lines:
        // take the key's encoding and `doFinal` it. This engine used to refuse
        // instead — `IllegalStateException: Cipher.wrap not implemented for
        // RSA/ECB/PKCS1Padding` — which is a statement about this VM, not about
        // the algorithm, and it broke every CMS/CRMF key-transport path in
        // bc-java (`cert.cmp`'s `testServerSideKey`, `its`, `pkcs`), all of
        // which wrap a content-encryption key under an RSA public key.
        let encoded = extract_key_bytes(ctx, key_to_wrap);
        if encoded.is_empty() {
            return Err(crate::phases_early::throw_jca_exc(
                ctx,
                "java/security/InvalidKeyException",
                "Cannot get an encoding of the key to be wrapped",
            ));
        }
        accumulate_bytes(ctx, this, &encoded);
        return cipher_do_final_impl(ctx, this);
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
    // See `cipher_wrap_impl` — the delegate owns this operation.
    if cipher_is_delegated(ctx, this) {
        if let Some(spi) = cipher_delegate_spi(ctx, this) {
            return ctx.invoke_virtual(
                spi,
                "engineUnwrap",
                "([BLjava/lang/String;I)Ljava/security/Key;",
                &[
                    Value::Object(Some(wrapped)),
                    Value::Object(Some(algorithm)),
                    Value::Int(key_type),
                ],
            );
        }
    }
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
        // `javax.crypto.CipherSpi.engineUnwrap`'s own body: decrypt, then build
        // a key of the requested type from the plaintext. See `cipher_wrap_impl`
        // for why refusing here was wrong.
        let ciphertext = read_bytes(ctx, wrapped);
        accumulate_bytes(ctx, this, &ciphertext);
        let plain = match cipher_do_final_impl(ctx, this)? {
            Some(Value::Object(Some(a))) => read_bytes(ctx, a),
            _ => {
                return Err(crate::phases_early::throw_jca_exc(
                    ctx,
                    "java/security/InvalidKeyException",
                    "Unwrap produced no key material",
                ))
            }
        };
        let key_algorithm = ctx.read_string(algorithm).unwrap_or_default();
        return build_unwrapped_key(ctx, &plain, &key_algorithm, key_type);
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
/// RFC 5649 §3 "AES Key Wrap with Padding" (`AES/KWP/NoPadding`).
///
/// Two things differ from RFC 3394, and both are load-bearing:
///
/// * the integrity check value is `A6 59 59 A6` followed by the **unpadded**
///   length as a big-endian `u32`, so an unwrap recovers the exact original
///   length rather than a zero-padded approximation; and
/// * a payload that fits one 8-byte block after padding is encrypted as a
///   SINGLE AES block (`AIV || padded`), not run through the six-round
///   wrapping schedule — RFC 3394's schedule is undefined for n = 1.
///
/// Verified against SunJCE on OpenJDK 25.0.4 with a 128-bit KEK
/// `000102030405060708090a0b0c0d0e0f`:
///
/// ```text
/// 16 bytes 00112233445566778899aabbccddeeff
///   -> 2cef0c9e30de26016c230cb78bc60d51b1fe083ba0c79cd5
/// 20 bytes 00112233445566778899aabbccddeeff00112233
///   -> 23cf017f0dc30969899318b8b400c0eca73290dba36289217fbb33d964653ae9
/// ```
const AES_KWP_AIV: [u8; 4] = [0xA6, 0x59, 0x59, 0xA6];

/// SunJCE's refusal for a payload RFC 3394 cannot wrap, verbatim — measured on
/// OpenJDK 25.0.4 for `AES/KW/NoPadding` at 0, 1, 7, 8, 9, 15 and 17 bytes and
/// for `AES/KW/PKCS5Padding` below 8. The wording is part of the API: a caller
/// that logs or matches on it sees HotSpot's string.
const AES_KW_LENGTH_REFUSAL: &str = "data should be at least 16 bytes and multiples of 8";

fn aes_key_wrap_with_padding(kek_bytes: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, String> {
    if plaintext.is_empty() {
        // SunJCE's exact wording, measured on OpenJDK 25.0.4.
        return Err("data should have at least 1 byte".to_string());
    }
    let kek = Aes::key_expansion(kek_bytes).map_err(|e| format!("invalid AES KWP key: {e:?}"))?;
    let mli = plaintext.len() as u32;
    let padded_len = plaintext.len().div_ceil(8) * 8;
    let mut aiv = [0u8; 8];
    aiv[..4].copy_from_slice(&AES_KWP_AIV);
    aiv[4..].copy_from_slice(&mli.to_be_bytes());

    let mut padded = plaintext.to_vec();
    padded.resize(padded_len, 0);

    if padded_len == 8 {
        // Single-block case: one AES encryption of AIV || the padded block.
        let mut block = [0u8; 16];
        block[..8].copy_from_slice(&aiv);
        block[8..].copy_from_slice(&padded);
        return Ok(Aes::encrypt_block(&kek, &block).to_vec());
    }
    Ok(aes_key_wrap_with_iv(&kek, &aiv, &padded))
}

/// RFC 3394's wrapping schedule with a caller-supplied initial `A`. Factored
/// out of [`aes_key_wrap`] so KW and KWP cannot drift: they differ only in that
/// initial value and in what surrounds them.
fn aes_key_wrap_with_iv(kek: &AesKey, iv: &[u8; 8], padded: &[u8]) -> Vec<u8> {
    let n = padded.len() / 8;
    let mut a = *iv;
    let mut r = padded.to_vec();
    for j in 0..6 {
        for i in 0..n {
            let mut block = [0u8; 16];
            block[..8].copy_from_slice(&a);
            block[8..].copy_from_slice(&r[i * 8..(i + 1) * 8]);
            let encrypted = Aes::encrypt_block(kek, &block);
            let t = ((n * j + i + 1) as u64).to_be_bytes();
            for k in 0..8 {
                a[k] = encrypted[k] ^ t[k];
            }
            r[i * 8..(i + 1) * 8].copy_from_slice(&encrypted[8..]);
        }
    }
    let mut out = Vec::with_capacity(padded.len() + 8);
    out.extend_from_slice(&a);
    out.extend_from_slice(&r);
    out
}

/// RFC 5649 unwrap. Returns the plaintext at its ORIGINAL length.
///
/// Every failure here is an integrity failure and must stay one: a wrong KEK, a
/// tampered wrap or a bad length all land on the same `Err`, and the caller
/// raises a checked exception. Returning the zero-padded bytes on a length
/// mismatch would be the same species of defect as the ChaCha20 substitution
/// this file was opened for.
fn aes_key_unwrap_with_padding(kek_bytes: &[u8], wrapped: &[u8]) -> Result<Vec<u8>, String> {
    let kek = Aes::key_expansion(kek_bytes).map_err(|e| format!("invalid AES KWP key: {e:?}"))?;
    if wrapped.len() < 16 || wrapped.len() % 8 != 0 {
        return Err(format!(
            "AES KWP ciphertext length {} must be a multiple of 8 and at least 16",
            wrapped.len()
        ));
    }
    let (aiv, padded) = if wrapped.len() == 16 {
        let mut only = [0u8; 16];
        only.copy_from_slice(wrapped);
        let block = Aes::decrypt_block(&kek, &only);
        let mut a = [0u8; 8];
        a.copy_from_slice(&block[..8]);
        (a, block[8..].to_vec())
    } else {
        aes_key_unwrap_with_iv(&kek, wrapped)
    };
    if aiv[..4] != AES_KWP_AIV {
        return Err("AES KWP integrity check failed".to_string());
    }
    let mli = u32::from_be_bytes([aiv[4], aiv[5], aiv[6], aiv[7]]) as usize;
    // The declared length must sit inside the last block: anything else means
    // the wrap was altered.
    if mli > padded.len() || padded.len().saturating_sub(mli) >= 8 {
        return Err("AES KWP integrity check failed (bad length)".to_string());
    }
    // …and the padding it implies must actually be zeroes.
    if padded[mli..].iter().any(|&b| b != 0) {
        return Err("AES KWP integrity check failed (non-zero padding)".to_string());
    }
    Ok(padded[..mli].to_vec())
}

/// RFC 3394's unwrapping schedule, returning `(A, R)` without checking `A`.
fn aes_key_unwrap_with_iv(kek: &AesKey, wrapped: &[u8]) -> ([u8; 8], Vec<u8>) {
    let n = wrapped.len() / 8 - 1;
    let mut a = [0u8; 8];
    a.copy_from_slice(&wrapped[..8]);
    let mut r = wrapped[8..].to_vec();
    for j in (0..6).rev() {
        for i in (0..n).rev() {
            let t = ((n * j + i + 1) as u64).to_be_bytes();
            let mut block = [0u8; 16];
            for k in 0..8 {
                block[k] = a[k] ^ t[k];
            }
            block[8..].copy_from_slice(&r[i * 8..(i + 1) * 8]);
            let decrypted = Aes::decrypt_block(kek, &block);
            a.copy_from_slice(&decrypted[..8]);
            r[i * 8..(i + 1) * 8].copy_from_slice(&decrypted[8..]);
        }
    }
    (a, r)
}

/// Which AES key-wrap flavour `algo` names, if any.
///
/// `AES/KW/PKCS5Padding` is RFC 3394 over PKCS#5-padded data at an EIGHT-byte
/// block size — not sixteen. Measured on OpenJDK 25.0.4: a 16-byte payload
/// wraps to 32 bytes, which is only consistent with padding to 24 first.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AesWrapFlavour {
    /// RFC 3394, input already a multiple of 8.
    Kw,
    /// RFC 3394 over PKCS#5-padded (block size 8) input.
    KwPkcs5,
    /// RFC 5649.
    Kwp,
}

fn aes_wrap_flavour(algo: &str) -> Option<AesWrapFlavour> {
    match algo.to_ascii_uppercase().as_str() {
        "AES/KW/PKCS5PADDING" => Some(AesWrapFlavour::KwPkcs5),
        "AES/KWP/NOPADDING" | "AESKWP" => Some(AesWrapFlavour::Kwp),
        a if is_aes_key_wrap_transformation(a) => Some(AesWrapFlavour::Kw),
        _ => None,
    }
}

/// PKCS#5 padding at an 8-byte block size, for `AES/KW/PKCS5Padding`.
fn pkcs5_pad8(data: &[u8]) -> Vec<u8> {
    let pad = 8 - (data.len() % 8);
    let mut out = data.to_vec();
    out.extend(std::iter::repeat(pad as u8).take(pad));
    out
}

fn pkcs5_unpad8(data: &[u8]) -> Result<Vec<u8>, String> {
    let pad = *data.last().ok_or("empty PKCS#5 block")? as usize;
    if pad == 0 || pad > 8 || pad > data.len() {
        return Err("bad PKCS#5 padding".to_string());
    }
    if data[data.len() - pad..].iter().any(|&b| b as usize != pad) {
        return Err("bad PKCS#5 padding".to_string());
    }
    Ok(data[..data.len() - pad].to_vec())
}

/// `doFinal` for the three AES key-wrap transformations.
///
/// These were ADVERTISED by `seed_direct_native_engine_services` and reachable
/// only through `Cipher.wrap`/`unwrap`; a `doFinal` on any of them fell through
/// to the block-mode dispatch and raised an UNCHECKED `IllegalStateException`
/// ("Cipher mode 'KW' not implemented in WP6.3 dispatch"), which sails past
/// `catch (GeneralSecurityException)`. SunJCE serves all three through
/// `doFinal`, and the vectors above are its answers.
fn aes_key_wrap_do_final(
    ctx: &mut dyn NativeContext,
    table_key: CipherKey,
    flavour: AesWrapFlavour,
    mode: i32,
    kek: &[u8],
    data: &[u8],
) -> MethodCallResult {
    let encrypt = mode == 1 || mode == 3;
    let result = match (flavour, encrypt) {
        (AesWrapFlavour::Kw, true) => aes_key_wrap(kek, data),
        (AesWrapFlavour::Kw, false) => aes_key_unwrap(kek, data),
        (AesWrapFlavour::KwPkcs5, true) => {
            // PKCS#5 at block size 8 always ADDS a block, so an 8-byte payload
            // pads to 16 and wraps; anything shorter cannot reach RFC 3394's
            // two-block minimum. Measured on SunJCE: 8 bytes wrap to 24, 7
            // bytes raise "data should be at least 16 bytes and multiples of 8".
            if data.len() < 8 {
                Err(AES_KW_LENGTH_REFUSAL.to_string())
            } else {
                aes_key_wrap(kek, &pkcs5_pad8(data))
            }
        }
        (AesWrapFlavour::KwPkcs5, false) => {
            aes_key_unwrap(kek, data).and_then(|p| pkcs5_unpad8(&p))
        }
        (AesWrapFlavour::Kwp, true) => aes_key_wrap_with_padding(kek, data),
        (AesWrapFlavour::Kwp, false) => aes_key_unwrap_with_padding(kek, data),
    };
    match result {
        Ok(bytes) => finish_cipher_bytes(ctx, table_key, &bytes),
        // SunJCE reports a length it cannot wrap as a CHECKED
        // IllegalBlockSizeException (measured: `AES/KW/NoPadding` on 20 bytes
        // -> "data should be at least 16 bytes and multiples of 8"), and an
        // integrity failure on unwrap AS THE SAME CLASS — measured on Temurin
        // 25.0.3+9, `javax.crypto.IllegalBlockSizeException: Integrity check
        // failed` for a wrapped key with one bit flipped, on `AES/KW`,
        // `AES/KWP` and `AESWrapPad` alike. An unchecked IllegalStateException
        // here is what the caller could not catch.
        //
        // This arm used to answer `BadPaddingException` on the unwrap side.
        // Both are checked so a `catch (GeneralSecurityException)` was
        // unaffected, but the two are siblings and not a hierarchy — a `catch
        // (IllegalBlockSizeException)` written against the JDK did not run —
        // and it made the SAME failure report two different classes depending
        // on which of this file's two RFC-3394 arms served it: the `"KW"` arm
        // of the block dispatch already answers IllegalBlockSizeException.
        // One algorithm, one answer.
        Err(msg) => Err(crate::phases_early::throw_jca_exc(
            ctx,
            "javax/crypto/IllegalBlockSizeException",
            &msg,
        )),
    }
}

/// `doFinal` for `ChaCha20` and `ChaCha20-Poly1305`.
///
/// Every exception class and message below is measured against SunJCE on
/// OpenJDK 25.0.4 rather than chosen, because the class is what decides whether
/// a caller's `catch` runs:
///
/// ```text
/// 16-byte key                      InvalidKeyException: Key length must be 256 bits
/// ChaCha20-Poly1305, 8-byte nonce  InvalidAlgorithmParameterException:
///                                    ChaCha20-Poly1305 nonce must be 12 bytes in length
/// ChaCha20 + updateAAD             IllegalStateException: Cipher is running in non-AEAD mode
/// tampered ciphertext              AEADBadTagException: Tag mismatch
/// ciphertext shorter than the tag  AEADBadTagException: Input too short - need tag
/// ```
///
/// The nonce-length check for the RAW cipher is deliberately NOT here: SunJCE
/// rejects a short nonce in the `ChaCha20ParameterSpec` CONSTRUCTOR
/// (`IllegalArgumentException: Nonce must be 12-bytes in length`), so by the
/// time a spec exists its nonce is already 12 bytes. A second check here would
/// fire only for a nonce this VM failed to read, and reporting that as the
/// caller's mistake would be a lie.
#[allow(clippy::too_many_arguments)]
fn chacha20_do_final(
    ctx: &mut dyn NativeContext,
    table_key: CipherKey,
    algo: &str,
    mode: i32,
    key_bytes: &[u8],
    iv_bytes: &[u8],
    aad: &[u8],
    data: &[u8],
    counter: u32,
) -> MethodCallResult {
    if key_bytes.len() != 32 {
        return Err(crate::phases_early::throw_jca_exc(
            ctx,
            "java/security/InvalidKeyException",
            "Key length must be 256 bits",
        ));
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(key_bytes);

    let aead = is_chacha20_poly1305_transformation(algo);
    if iv_bytes.len() != 12 {
        // A missing nonce is not the same mistake as a wrong-length one, but
        // both are unusable and SunJCE reports the AEAD case as an
        // InvalidAlgorithmParameterException. For the raw cipher the spec
        // constructor has already enforced the length, so an unusable nonce
        // here means `init` never supplied one.
        return Err(crate::phases_early::throw_jca_exc(
            ctx,
            "java/security/InvalidAlgorithmParameterException",
            &if aead {
                "ChaCha20-Poly1305 nonce must be 12 bytes in length".to_string()
            } else {
                format!(
                    "ChaCha20 requires a 12-byte nonce from a ChaCha20ParameterSpec, got {}",
                    iv_bytes.len()
                )
            },
        ));
    }
    let mut nonce = [0u8; 12];
    nonce.copy_from_slice(iv_bytes);

    // 1 = ENCRYPT, 2 = DECRYPT, 3 = WRAP, 4 = UNWRAP.
    let encrypt = mode == 1 || mode == 3;

    let out: Vec<u8> = if aead {
        if encrypt {
            let (mut ct, tag) = crate::chacha20::chacha20_poly1305_encrypt(&key, &nonce, aad, data);
            ct.extend_from_slice(&tag);
            ct
        } else {
            if data.len() < 16 {
                return Err(crate::phases_early::throw_jca_exc(
                    ctx,
                    "javax/crypto/AEADBadTagException",
                    "Input too short - need tag",
                ));
            }
            let split = data.len() - 16;
            let mut tag = [0u8; 16];
            tag.copy_from_slice(&data[split..]);
            match crate::chacha20::chacha20_poly1305_decrypt(
                &key,
                &nonce,
                aad,
                &data[..split],
                &tag,
            ) {
                Ok(pt) => pt,
                // The one outcome every AEAD caller writes a `catch` for.
                // `AEADBadTagException` is a checked `BadPaddingException`; an
                // `IllegalStateException` here would sail past
                // `catch (GeneralSecurityException)` and turn "reject this
                // token" into an uncaught throw.
                Err(()) => {
                    return Err(crate::phases_early::throw_jca_exc(
                        ctx,
                        "javax/crypto/AEADBadTagException",
                        "Tag mismatch",
                    ))
                }
            }
        }
    } else {
        if !aad.is_empty() {
            // SunJCE refuses AAD on the raw stream cipher rather than ignoring
            // it. Ignoring it is how a caller comes to believe data is
            // authenticated when nothing authenticates it.
            return Err(RuntimeError::IllegalStateException {
                message: "Cipher is running in non-AEAD mode".into(),
            }
            .into());
        }
        // Encryption and decryption are the same XOR against the same
        // keystream, so `encrypt` does not appear here at all.
        crate::chacha20::chacha20_apply(&key, &nonce, counter, data)
    };

    finish_cipher_bytes(ctx, table_key, &out)
}

/// Drive a real SunJCE `CipherSpi` that takes **no `AlgorithmParameterSpec` at
/// all** — `BlowfishCipher` in ECB, and `ARCFOURCipher`, which is a stream
/// cipher and has no parameters in any mode.
///
/// ## Why this is not `phases_early::drive_real_cipher`
///
/// That function is the same idiom and it is the one to use when there IS an
/// IV; it builds an `IvParameterSpec` unconditionally and passes the four-arg
/// `engineInit(int, Key, AlgorithmParameterSpec, SecureRandom)`. Neither of
/// these two families tolerates that. Measured on jdk-25.0.3.9-hotspot, driving
/// the real SPI by reflection exactly as `drive_real_cipher` does:
///
/// ```text
/// BlowfishCipher ECB/PKCS5Padding  params=null       -> 33b63e40d662746425f71a69f8cffcdadadae7ffa8950336
/// BlowfishCipher ECB/PKCS5Padding  params=IV[0]      -> InvalidAlgorithmParameterException: Wrong IV length: must be 8 bytes long
/// BlowfishCipher ECB/PKCS5Padding  params=IV[8]      -> InvalidAlgorithmParameterException: ECB mode cannot use IV
/// ARCFOURCipher  ECB/NoPadding     params=null       -> 27ca482b161e3ab93f812659b904df95
/// ARCFOURCipher  ECB/NoPadding     params=IV[0]      -> InvalidAlgorithmParameterException: Parameters not supported
/// ```
///
/// So the choice is between a zero-length IV (a different exception), an
/// eight-byte one (a wrong-mode exception), and no parameters. This calls the
/// THREE-arg `engineInit(int, Key, SecureRandom)` overload rather than passing a
/// null `AlgorithmParameterSpec` to the four-arg one, because a null there is
/// ambiguous between the two four-arg overloads (`AlgorithmParameterSpec` and
/// `AlgorithmParameters`) and because the three-arg form is the one
/// `Cipher.init(int, Key)` itself calls. Both spellings were measured to produce
/// the bytes above; this one says what it means.
///
/// The right end state is one driver taking `Option<&[u8]>` for the IV.
/// `drive_real_cipher` lives in `phases_early.rs`, which the W7-39 lane does not
/// own; merging them is a follow-up, and until then this doc comment and that
/// one point at each other so neither is edited alone.
///
/// ## What is deliberately NOT reimplemented
///
/// Everything. No Blowfish key schedule, no RC4 KSA/PRGA, no PKCS#5 padding —
/// the SPI does all of it, including the key-length refusals
/// (`key_length_reason` duplicates only the two that must surface at `init`
/// rather than `doFinal`). A second implementation of a primitive the image
/// already ships is the defect shape this campaign has found repeatedly; the
/// point of routing here is that the output is HotSpot's by construction.
fn drive_real_ecb_cipher(
    ctx: &mut dyn NativeContext,
    spi_class: &'static str,
    pad_str: &str,
    key_algo: &str,
    opmode: i32,
    key_bytes: &[u8],
    data: &[u8],
) -> MethodCallResult {
    let spi = match ctx.new_object_initialized(spi_class, "()V", &[])? {
        Some(Value::Object(Some(o))) => o,
        _ => {
            return Err(RuntimeError::NotImplemented {
                feature: spi_class.into(),
            }
            .into())
        }
    };
    // Pin the SPI across every allocation below (create_string, byte arrays,
    // SecretKeySpec): a moving GC between the constructor and `engineDoFinal`
    // would otherwise leave `spi` pointing at an abandoned from-space copy, and
    // the symptom would be a cipher that "worked" against uninitialised state.
    let pin = ctx.pin_native_root(spi);
    let result = (|| -> MethodCallResult {
        let mode_s = ctx.create_string("ECB");
        let spi_r = ctx.read_native_pin(pin, spi);
        ctx.invoke_virtual(
            spi_r,
            "engineSetMode",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(mode_s))],
        )?;
        let pad_s = ctx.create_string(pad_str);
        let spi_r = ctx.read_native_pin(pin, spi);
        ctx.invoke_virtual(
            spi_r,
            "engineSetPadding",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(pad_s))],
        )?;

        // `SecretKeySpec(key, algorithm)`. The array is built fresh from
        // `key_bytes` and handed straight to the constructor, which COPIES it
        // (`jca::secret_key_spec`, fixed 2026-08-11) — the side-table's own copy
        // is never aliased into Java, and nothing here scrubs a buffer the key
        // still points at. That direction is the one that produced the all-zero
        // AES key W7-38 measured.
        let key_arr = crate::phases_early::make_byte_array(ctx, key_bytes);
        let kpin = ctx.pin_native_root(key_arr);
        let algo_s = ctx.create_string(key_algo);
        let key_arr_r = ctx.read_native_pin(kpin, key_arr);
        let secret_key = match ctx.new_object_initialized(
            "javax/crypto/spec/SecretKeySpec",
            "([BLjava/lang/String;)V",
            &[Value::Object(Some(key_arr_r)), Value::Object(Some(algo_s))],
        )? {
            Some(Value::Object(Some(o))) => o,
            _ => {
                return Err(RuntimeError::IllegalStateException {
                    message: "SecretKeySpec construction failed".into(),
                }
                .into())
            }
        };
        let skpin = ctx.pin_native_root(secret_key);

        // `engineInit(opmode, key, null)` — the no-parameters overload. See the
        // measured table above for what each parameter-bearing spelling does.
        let spi_r = ctx.read_native_pin(pin, spi);
        let sk_r = ctx.read_native_pin(skpin, secret_key);
        ctx.invoke_virtual(
            spi_r,
            "engineInit",
            "(ILjava/security/Key;Ljava/security/SecureRandom;)V",
            &[
                Value::Int(opmode),
                Value::Object(Some(sk_r)),
                Value::Object(None),
            ],
        )?;

        let data_arr = crate::phases_early::make_byte_array(ctx, data);
        let dlen = data.len() as i32;
        let dpin = ctx.pin_native_root(data_arr);
        let spi_r = ctx.read_native_pin(pin, spi);
        let data_arr_r = ctx.read_native_pin(dpin, data_arr);
        ctx.invoke_virtual(
            spi_r,
            "engineDoFinal",
            "([BII)[B",
            &[
                Value::Object(Some(data_arr_r)),
                Value::Int(0),
                Value::Int(dlen),
            ],
        )
    })();
    ctx.unpin_native_roots(pin);
    result
}

/// Reads the transformation back out of the side-table and routes it by
/// FAMILY, in this order: RSA → PBES2 → the real SunJCE SPI (AES-CBC/CFB/OFB,
/// DES, DESede) → the in-crate AES paths (GCM, ECB, RFC 3394 key wrap). Every
/// arrival here has been admitted by `classify_transformation`, so there is no
/// arm that guesses.
///
/// The retired version of this comment said the non-GCM modes were "out of
/// probe scope and return an `IllegalStateException` describing the requested
/// transformation rather than silently producing wrong bytes". Half of that was
/// never true: `ChaCha20`, `Blowfish`, `RC4` and every other admitted-but-
/// uncomputable name did not reach the `IllegalStateException` at all — they
/// landed in the ECB arm and returned AES. A comment outlives its defect, and
/// this one outlived a defect it also misdescribed.
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
    let state_counter = state.chacha_counter;
    let rsa_n = state.rsa_n.clone();
    let rsa_exp = state.rsa_exp.clone();
    let rsa_key_id = state.rsa_key_id;

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
            // Genuinely a VM-state failure and not a data one: `init` ran and
            // captured nothing, so there is no key to fail against. HotSpot's
            // own answer to "doFinal on a Cipher that is not usable" is the
            // unchecked `IllegalStateException` (measured), so this one stays.
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
            // Prefer the key handle: it reaches the CRT parameters, which the
            // `(n, d)` form cannot see. A handle that is not in the store
            // returns `None` and falls through to the identical `(n, d)`
            // path — same body, same exception classes, just slower.
            rsa_key_id
                .and_then(|id| crate::crypto_impl::rsa_cipher_decrypt_by_id(id, &rsa_n, pad, &data))
                .unwrap_or_else(|| {
                    crate::crypto_impl::rsa_cipher_decrypt(&rsa_n, &rsa_exp, pad, &data)
                })
        };
        return match result {
            Ok(bytes) => finish_cipher_bytes(ctx, key, &bytes),
            // THE FIX. Every RSA failure used to arrive here as an `Err(String)`
            // and leave as an UNCHECKED `IllegalStateException`, so the JDK's
            // own `catch (BadPaddingException e)` around an RSA decrypt did not
            // run and the failure escaped through code that believed it had
            // handled it. `RsaCipherError` carries the class SunJCE raises —
            // `BadPaddingException` for a padding/integrity failure,
            // `IllegalBlockSizeException` for a length one — and all three names
            // are real JDK classes, so `throw_jca_exc` resolves them rather than
            // degrading a wrong-exception defect into a `NoClassDefFoundError`.
            //
            // Raising a CHECKED exception where an unchecked one was raised
            // cannot break a caller that compiles today: `doFinal` already
            // declares both of these, so any `catch` for them is already
            // written and was simply dead. This can only make a dead handler
            // start working.
            Err(e) => Err(crate::phases_early::throw_jca_exc(
                ctx,
                e.jca_class(),
                e.message(),
            )),
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
            // Match on the FAMILY, not the spelling. `("AES", "CBC")` missed
            // every `AES_128`/`AES_192`/`AES_256` transformation, which then
            // fell through to the mode-only dispatch below and died on
            // `mode 'CBC' not implemented`. The mode is still matched on the
            // token the caller wrote, because for this family it selects real
            // behaviour — see the DESede note.
            match (cipher_family(&cn), cm.as_str()) {
                (Some(CipherFamily::Aes | CipherFamily::AesFixed(_)), "CBC") => {
                    Some(("com/sun/crypto/provider/AESCipher$General", "AES", "CBC"))
                }
                (Some(CipherFamily::Aes | CipherFamily::AesFixed(_)), "CFB") => {
                    Some(("com/sun/crypto/provider/AESCipher$General", "AES", "CFB"))
                }
                (Some(CipherFamily::Aes | CipherFamily::AesFixed(_)), "OFB") => {
                    Some(("com/sun/crypto/provider/AESCipher$General", "AES", "OFB"))
                }
                // These used to hardcode `"CBC"` whatever the caller wrote,
                // sound only because `classify_transformation` admitted no
                // other mode for this family, and carrying a note that
                // widening the admission table without widening this line
                // would serve CBC under another mode's name. The admission
                // table is widened to ECB in the same change, so this line is
                // widened with it: the mode is MATCHED here rather than
                // assumed, and every arm names the mode it forwards.
                //
                // No wildcard arm: a mode this match does not name falls to
                // `_ => None` and takes the no-route path, so the pairing
                // cannot silently drift again.
                (Some(CipherFamily::DesFamily), "CBC") if cn.eq_ignore_ascii_case("DES") => {
                    Some(("com/sun/crypto/provider/DESCipher", "DES", "CBC"))
                }
                (Some(CipherFamily::DesFamily), "ECB") if cn.eq_ignore_ascii_case("DES") => {
                    Some(("com/sun/crypto/provider/DESCipher", "DES", "ECB"))
                }
                (Some(CipherFamily::DesFamily), "CBC") => {
                    Some(("com/sun/crypto/provider/DESedeCipher", "DESede", "CBC"))
                }
                (Some(CipherFamily::DesFamily), "ECB") => {
                    Some(("com/sun/crypto/provider/DESedeCipher", "DESede", "ECB"))
                }
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

    // Blowfish and RC4/ARCFOUR — also BEFORE `Aes::key_expansion`, and for the
    // same reason the ChaCha20 route below gives. A 16-byte Blowfish or RC4 key
    // is a valid AES-128 key, so the expansion succeeded rather than erroring
    // and the mode-only dispatch ran AES-128-ECB: measured, both names produced
    // `178c380cadc0514ffe26d8b26351c673` — the same bytes as each other AND as
    // `AES/ECB/NoPadding` on the same key. Keying on the FAMILY is what makes
    // admitting these names safe again; see `real_spi_ecb_route`.
    let (spi_name, _spi_mode, spi_padded) = parse_transformation(&algo);
    if let Some(family) = cipher_family(&spi_name) {
        if let Some((spi_class, pad_str, key_algo)) = real_spi_ecb_route(family, spi_padded) {
            let out =
                drive_real_ecb_cipher(ctx, spi_class, pad_str, key_algo, mode, &key_bytes, &data)?;
            // Capture the result BEFORE the reset, exactly as the route above
            // does: the reset allocates, and a moving GC between the two would
            // relocate the ciphertext array out from under `out`.
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

    // ChaCha20 / ChaCha20-Poly1305 — BEFORE `Aes::key_expansion`, which is the
    // whole point. A ChaCha20 key is 32 bytes, which is a VALID AES-256 key, so
    // the expansion below succeeds rather than erroring and the mode-only
    // dispatch then ran AES-256-ECB: nonce discarded, output deterministic per
    // (key, block), and for the AEAD form no tag at all. See `crate::chacha20`.
    if is_chacha20_family(&algo) {
        return chacha20_do_final(
            ctx,
            key,
            &algo,
            mode,
            &key_bytes,
            &iv_bytes,
            &aad,
            &data,
            state_counter,
        );
    }

    // The AES key wraps. `Cipher.wrap`/`unwrap` already reached RFC 3394; a
    // `doFinal` on the same transformation did not, and SunJCE serves both.
    if let Some(flavour) = aes_wrap_flavour(&algo) {
        return aes_key_wrap_do_final(ctx, key, flavour, mode, &key_bytes, &data);
    }

    // `pad` was `_pad` — parsed, then thrown away. Every consequence of
    // ignoring it is below; see the ECB arm.
    //
    // `_cipher_name` was the OTHER half of the defect this lane fixed: the
    // algorithm the caller asked for was parsed out here and then discarded,
    // so the `match mode_str` below decided everything. With a missing mode
    // defaulting to ECB, `Cipher.getInstance("ChaCha20")` landed in the ECB arm
    // with a 32-byte key that `Aes::key_expansion` was happy to accept as
    // AES-256. It is bound and CHECKED now: the mode-only dispatch is valid for
    // the AES family alone, and every other family has already returned above
    // (RSA, PBES2 and DES/DESede each route earlier in this function).
    let (cipher_name, parsed_mode, pad) = parse_transformation(&algo);
    let encrypt = mode == 1;

    let family = cipher_family(&cipher_name);
    match family {
        Some(CipherFamily::Aes | CipherFamily::AesFixed(_) | CipherFamily::AesKeyWrap) => {}
        // Not reachable through `Cipher.getInstance`, which refuses every name
        // outside the table. Say so, rather than computing AES and calling it
        // success.
        //
        // The wording used to add "so reaching it means the admission table and
        // this dispatch have drifted apart", and that diagnosis was WRONG and
        // cost a day. The name that reached here was not an odd one the table
        // let through — it was EMPTY, because `cipher_alloc` read the
        // transformation out of the caller's `String` argument one line AFTER
        // allocating the `Cipher`, and the allocation could relocate or reclaim
        // that argument (`unwrap_or_default()` then made the miss silent). The
        // admission table was never consulted for "" and never admitted it.
        // `cipher_init_record_with_counter` now refuses an empty transformation
        // at `init`, so a residual instance of that family cannot reach here at
        // all; if one does, suspect a root that is not pinned, not the table.
        other => {
            return Err(RuntimeError::IllegalStateException {
                message: format!(
                    "Cipher dispatch reached the AES path for transformation '{algo}' \
                     (family {other:?}); `classify_transformation` admitted a name this \
                     arm cannot compute. Refusing to encrypt with a substitute algorithm."
                ),
            }
            .into())
        }
    }

    // `AESWrap`, `AESWrap_128/192/256` carry no mode token, so
    // `parse_transformation` hands back the ECB default for them — which would
    // send `Cipher.getInstance("AESWrap").doFinal(..)` into the ECB arm and
    // return AES-ECB blocks where SunJCE returns an RFC 3394 wrap. The name IS
    // the mode for that family (`Alg.Alias.Cipher.AESWrap = AES/KW/NoPadding`
    // on SunJCE), so say so once, here, rather than letting a default decide.
    let mode_str = if matches!(family, Some(CipherFamily::AesKeyWrap)) {
        "KW".to_string()
    } else {
        parsed_mode
    };

    let aes_key = match Aes::key_expansion(&key_bytes) {
        Ok(k) => k,
        Err(e) => {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("Invalid AES key: {:?}", e),
            }
            .into())
        }
    };

    let result_bytes: Result<Vec<u8>, String> = match mode_str.as_str() {
        "GCM" => {
            if iv_bytes.len() != 12 {
                // A GAP, not a data failure: SunJCE derives J0 by GHASH for any
                // IV length and this engine implements only the 12-byte case.
                // The class still matters — the `Err(String)` tail below made
                // this an UNCHECKED `IllegalStateException`, so a caller could
                // not catch it at all, whereas
                // `InvalidAlgorithmParameterException` is a checked
                // `GeneralSecurityException` and is what SunJCE raises for a
                // GCM parameter it will not accept. The right eventual fix is
                // to implement the general J0 derivation; until then this fails
                // closed and catchably rather than closed and not.
                return Err(crate::phases_early::throw_jca_exc(
                    ctx,
                    "java/security/InvalidAlgorithmParameterException",
                    &format!(
                        "this engine implements AES-GCM for a 12-byte IV only, got {} \
                         (SunJCE derives J0 by GHASH for other lengths)",
                        iv_bytes.len()
                    ),
                ));
            } else {
                let mut nonce = [0u8; 12];
                nonce.copy_from_slice(&iv_bytes);
                if encrypt {
                    let out = AesGcm::encrypt(&aes_key, &nonce, &data, &aad);
                    let mut buf = out.ciphertext;
                    buf.extend_from_slice(&out.tag);
                    Ok(buf)
                } else if data.len() < 16 {
                    // A truncated AEAD ciphertext is an AUTHENTICATION
                    // failure, not a VM state error. SunJCE:
                    // `AEADBadTagException("Input too short - need tag")`.
                    return Err(crate::phases_early::throw_jca_exc(
                        ctx,
                        "javax/crypto/AEADBadTagException",
                        "Input too short - need tag",
                    ));
                } else {
                    let split = data.len() - 16;
                    let ct = &data[..split];
                    let mut tag = [0u8; 16];
                    tag.copy_from_slice(&data[split..]);
                    match AesGcm::decrypt(&aes_key, &nonce, ct, &aad, &tag) {
                        Ok(pt) => Ok(pt),
                        // A GCM tag mismatch is the one outcome every caller
                        // writes a `catch` for, and the exception CLASS decides
                        // whether that catch runs. `AEADBadTagException` is a
                        // checked `BadPaddingException`; the
                        // `IllegalStateException` the `Err(String)` tail below
                        // used to produce is UNCHECKED and sails straight past
                        // `catch (BadPaddingException | GeneralSecurityException)`,
                        // turning "reject this token" into an uncaught throw.
                        // (Same defect species as the W3-7
                        // `IllegalArgumentException`-for-`NoSuchAlgorithmException`
                        // fix in `phases_late/ssl_security.rs`.)
                        Err(_) => {
                            return Err(crate::phases_early::throw_jca_exc(
                                ctx,
                                "javax/crypto/AEADBadTagException",
                                "Tag mismatch",
                            ))
                        }
                    }
                }
            }
        }
        "ECB" | "" => {
            // AES/ECB. `pad` is the transformation's padding, which this arm
            // used to ignore entirely — it always padded on encrypt and always
            // tried to strip on decrypt. Three separate wrong answers came out
            // of that, and none of them raised:
            //
            //  * `AES/ECB/NoPadding` encrypt appended a full PKCS7 block, so
            //    the ciphertext was 16 bytes longer than HotSpot's.
            //  * `AES/ECB/NoPadding` decrypt then truncated the real plaintext
            //    whenever its last byte happened to land in 1..=16 — a silent
            //    ~6%-of-the-time data corruption on a mode used for key
            //    wrapping.
            //  * With padding, the strip never VERIFIED the padding bytes and
            //    did nothing at all when the last byte was out of range, so a
            //    tampered or wrong-key ciphertext produced a plausible
            //    plaintext where SunJCE throws `BadPaddingException`.
            //
            // `phases_early`'s `pkcs7_pad`/`pkcs7_unpad` are the same rule
            // implemented once, correctly (its unpad verifies in constant time
            // — that arm has already had its own Vaudenay-oracle fix). Call
            // them instead of keeping a second copy that can drift again.
            let block_size = 16usize;
            if encrypt {
                let padded = if pad {
                    crate::phases_early::pkcs7_pad(&data)
                } else if data.len() % block_size != 0 {
                    return Err(crate::phases_early::throw_jca_exc(
                        ctx,
                        "javax/crypto/IllegalBlockSizeException",
                        "Input length not multiple of 16 bytes",
                    ));
                } else {
                    data.clone()
                };
                let mut out = Vec::with_capacity(padded.len());
                for chunk in padded.chunks(block_size) {
                    let mut block = [0u8; 16];
                    block.copy_from_slice(chunk);
                    let ct = Aes::encrypt_block(&aes_key, &block);
                    out.extend_from_slice(&ct);
                }
                Ok(out)
            } else if data.is_empty() {
                // `doFinal` with nothing buffered produces nothing, in both
                // padding modes — `CipherCore` returns a zero-length array
                // rather than raising.
                Ok(Vec::new())
            } else if data.len() % block_size != 0 {
                // Structural, and a `IllegalBlockSizeException` on SunJCE —
                // a checked `GeneralSecurityException`, not the unchecked ISE
                // this used to become.
                return Err(crate::phases_early::throw_jca_exc(
                    ctx,
                    "javax/crypto/IllegalBlockSizeException",
                    "Input length must be multiple of 16 when decrypting with padded cipher",
                ));
            } else {
                let mut out = Vec::with_capacity(data.len());
                for chunk in data.chunks(block_size) {
                    let mut block = [0u8; 16];
                    block.copy_from_slice(chunk);
                    let pt = Aes::decrypt_block(&aes_key, &block);
                    out.extend_from_slice(&pt);
                }
                if pad {
                    match crate::phases_early::pkcs7_unpad(&out) {
                        Ok(stripped) => Ok(stripped),
                        Err(_) => {
                            return Err(crate::phases_early::throw_jca_exc(
                                ctx,
                                "javax/crypto/BadPaddingException",
                                "Given final block not properly padded. Such issues can arise \
                                 if a bad key is used during decryption.",
                            ))
                        }
                    }
                } else {
                    Ok(out)
                }
            }
        }
        // RFC 3394 key wrap reached through `doFinal` rather than
        // `wrap`/`unwrap`. SunJCE serves both surfaces from the same
        // `AESKeyWrap` SPI, and this engine's `aes_key_wrap`/`aes_key_unwrap`
        // are byte-identical to it — measured on jdk-25.0.3.9-hotspot with a
        // 256-bit KEK: wrap of a 16-byte key gives
        // `bc3b4783f41958fdef7f32ef69f09086da1a6666e883f93f` on both. Before
        // this arm existed, `AES/KW/NoPadding` was advertised by
        // `provider_chain`, worked through `wrap()`, and raised an UNCHECKED
        // `IllegalStateException` through `doFinal` — one algorithm with two
        // answers depending on which method the caller reached for.
        "KW" => {
            let wrapped = if encrypt {
                aes_key_wrap(&key_bytes, &data)
            } else {
                aes_key_unwrap(&key_bytes, &data)
            };
            match wrapped {
                Ok(bytes) => Ok(bytes),
                // SunJCE reports both a bad input length and a failed integrity
                // check from `doFinal` as `IllegalBlockSizeException` — measured
                // `javax.crypto.IllegalBlockSizeException: Integrity check
                // failed` for a wrapped key with one bit flipped. It is a
                // checked `GeneralSecurityException`, so a caller's
                // `catch` matches; the `Err(String)` tail below would have made
                // it an unchecked `IllegalStateException` that sails past.
                // (The detail text is this module's own, which is more specific
                // than HotSpot's; the CLASS is what a handler selects on.)
                Err(message) => {
                    return Err(crate::phases_early::throw_jca_exc(
                        ctx,
                        "javax/crypto/IllegalBlockSizeException",
                        &message,
                    ))
                }
            }
        }
        // No default arm. `classify_transformation` admits exactly the modes
        // above for the AES family, so an unadmitted mode cannot arrive here
        // through `Cipher.getInstance` — and if one does, the two tables have
        // drifted and the honest answer is to say which mode, not to fall back
        // on ECB. The retired arm produced `IllegalStateException("Cipher mode
        // 'CTR' not implemented in WP6.3 dispatch")`: unchecked, so
        // uncatchable by `catch (GeneralSecurityException)`, and raised at
        // `doFinal` rather than at `getInstance` where the spec puts it.
        other => Err(format!(
            "Cipher mode '{other}' is admitted by `classify_transformation` but not \
             computed by this dispatch — the admission table and the AES dispatch \
             have drifted. Refusing to substitute another mode."
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
            // see `gaps/crash-01-arraylist-capacity-oom-abend.md`)
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

/// The validation half of `Cipher.getConfiguredPermission(String)`, for the
/// two public `getMaxAllowed*` methods that would otherwise call it.
///
/// Real `getConfiguredPermission` is two steps: an explicit `if
/// (transformation == null) throw new NullPointerException();` and then
/// `tokenizeTransformation`, before it ever consults the policy. The policy
/// answer is unconditional under the shipped unlimited policy, so the callers
/// only need the two refusals. The NPE is unmessaged — measured on HotSpot 25,
/// `getMaxAllowedKeyLength(null)` is a `NullPointerException` with a null
/// message, NOT `tokenizeTransformation`'s `NoSuchAlgorithmException: No
/// transformation given`, because the explicit check runs first.
fn check_transformation_well_formed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> Result<(), MethodCallFailed> {
    let Some(Value::Object(Some(s))) = args.first() else {
        return Err(RuntimeError::NullPointerException { message: None }.into());
    };
    let transformation = ctx.read_string(*s).unwrap_or_default();
    match tokenize_transformation(&transformation) {
        Ok(_) => Ok(()),
        Err(msg) => Err(crate::jca::provider_chain::throw_no_such_algorithm_public(
            ctx, &msg,
        )),
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
    //
    // KEEP, re-audited in wave 2 (flagged in that sweep's hardcoded-pass
    // list because `canUseProvider` and `getVerificationResult` are literally
    // verification predicates pinned to "passed"). What they gate is the JCE
    // *jar-signing* check — whether a provider JAR carries Oracle's
    // code-signing certificate — not any cryptographic or authorization
    // decision. CratonVM's crypto dispatch is native and never routes through
    // a loaded `ProviderList`, so no key, certificate, signature or
    // permission is validated by these methods; and `isRestricted()=>false`
    // matches the JDK's own default since Java 9 (unlimited crypto policy).
    // They remain `SyntheticStub` so strict no-stubs mode drops them.
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
    //
    // KEEP: `null` is spec-correct here and this is NOT a signature-check
    // bypass. `startJarVerification()` returns the *previous* thread-local
    // provider list (`beginThreadProviderList(getJarList(...))`), whose only
    // job is to stop verification from recursively loading providers out of
    // the jar being verified; the actual signature check is
    // `SignatureFileVerifier.processImpl`, which still runs in full on real
    // bytecode. `null` is a legal prior-list value and is exactly what the
    // paired `stopJarVerification(null)` → `endThreadProviderList(null)`
    // expects, so the try/finally pair stays balanced.
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
    // `JceSecurity.canUseProvider(Provider)` — needed because the no-op'd
    // clinit leaves `verifyingProviders` null and the real bytecode would
    // NPE on it. Delegates to `jce_verify_provider` (which reads the
    // provider's real CodeSource) rather than returning a bare `true`, and
    // shares that decision with `getVerificationResult` below so the two can
    // never disagree — real JDK defines this method AS
    // `getVerificationResult(p) == null`.
    r.register(
        "javax/crypto/JceSecurity",
        "canUseProvider",
        "(Ljava/security/Provider;)Z",
        |ctx, args| {
            let prov = match args.first() {
                Some(Value::Object(o)) => *o,
                _ => None,
            };
            let ok = jce_verify_provider(ctx, prov).is_none();
            Ok(Some(Value::Int(i32::from(ok))))
        },
    );
    // `JceSecurity.isRestricted()` returns false (unlimited).  Matches the
    // default field value the no-op'd clinit leaves behind, but the static
    // accessor is explicitly registered so any reflective lookup sees a
    // resolved method instead of a null-method-table miss on the
    // synthetic class.
    // KEEP: spec-correct, not a stub. `isRestricted == false` is what a
    // stock JDK 9+ install reports (`crypto.policy=unlimited` ships by
    // default), and it is factually true of this VM — `crate::crypto_impl`
    // enforces no key-size ceiling at all, so there is nothing to restrict.
    // REACHABILITY (wave 4 — the `jca/mod.rs` header saying this module is
    // synthetic-only is stale for THIS registrar): `register_cipher_clinit_shim`
    // is called from both `register_essential_natives_with_shims` (the default
    // real-JDK path) and `register_synthetic_overrides`, so it is live in both
    // modes.
    r.register(
        "javax/crypto/JceSecurity",
        "isRestricted",
        "()Z",
        |_ctx, _args| Ok(Some(Value::Int(0))),
    );
    // `JceSecurity.getVerificationResult(Provider) -> Exception`: `null`
    // means "verified", a non-null `Exception` is the failure the caller
    // rethrows.  Real-JDK's clinit populates `verificationResults` /
    // `verifyingProviders` / `PROVIDER_VERIFIED` so this method can index
    // into them — under our no-op'd clinit those fields are null and the
    // real bytecode NPEs at `new WeakIdentityWrapper(p, queue)` /
    // `verificationResults.computeIfAbsent(...)`.  Same `jce_verify_provider`
    // decision as `canUseProvider` above; on failure we hand back a real
    // `SecurityException` so the caller sees the reason rather than a
    // silent "verified".
    r.register(
        "javax/crypto/JceSecurity",
        "getVerificationResult",
        "(Ljava/security/Provider;)Ljava/lang/Exception;",
        |ctx, args| {
            let prov = match args.first() {
                Some(Value::Object(o)) => *o,
                _ => None,
            };
            match jce_verify_provider(ctx, prov) {
                None => Ok(Some(Value::Object(None))),
                Some(reason) => {
                    let msg = ctx
                        .create_string(&format!("JCE cannot authenticate the provider: {reason}"));
                    ctx.new_object_initialized(
                        "java/lang/SecurityException",
                        "(Ljava/lang/String;)V",
                        &[Value::Object(Some(msg))],
                    )
                }
            }
        },
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

    // `javax/crypto/Cipher.getMaxAllowedKeyLength(String)` and its twin
    // `getMaxAllowedParameterSpec(String)` — the two PUBLIC doors into the
    // policy chokepoint above, and the reason the `getCryptoPermission` native
    // cannot be the whole answer on JDK 25.
    //
    // Neither of them can REACH that native. Their first act is `invokestatic
    // Cipher.getConfiguredPermission`, whose first act is `getstatic
    // JceSecurityManager.INSTANCE` — and that getstatic runs
    // `JceSecurityManager.<clinit>`, which on JDK 25 ends with
    //
    //     WALKER = StackWalker.getInstance(
    //         Set.of(Option.DROP_METHOD_INFO, Option.RETAIN_CLASS_REFERENCE));
    //
    // (`JceSecurityManager.java:71`; the `WALKER` field is new-ish — it is how
    // the class finds its caller now that `SecurityManager` is gone). Every
    // `java/lang/StackWalker$Option` constant reads back NULL in this VM, so
    // `Set.of` NPEs on `e0.equals(e1)` inside
    // `ImmutableCollections$Set12.<init>` and the whole clinit dies as
    // `ExceptionInInitializerError`. `getCryptoPermission` never runs; the
    // comment above it describes a LATER wall (`defaultPolicy` null) that the
    // class never survives long enough to hit.
    //
    // That StackWalker hole is not JCA's and is NOT fixed here. It is not new
    // either: measured identically on the pristine-dev 44044c7e2 control
    // binary, where `StackWalker$Option.RETAIN_CLASS_REFERENCE` is also null.
    // See `phases_late.rs`'s three `StackWalker$Option` static-field
    // registrations — they cover 3 of the enum's 4 constants (JDK 22 added
    // `DROP_METHOD_INFO`) and are not consulted for a `getstatic` when the real
    // class bytes are authoritative.
    //
    // Answering here keeps the entire clinit off the path. `Integer.MAX_VALUE`
    // and `null` are not conservative guesses: they are what HotSpot 25 returns
    // on this host with the `crypto.policy=unlimited` that has shipped by
    // default since Java 9 — measured
    // `getMaxAllowedKeyLength("AES"|"DES"|"RC4") = 2147483647` and
    // `getMaxAllowedParameterSpec("AES") = null`. Note what these methods do
    // NOT do: they never check that the algorithm EXISTS, so `"Bogus"` also
    // answers unlimited (measured). They check only that the transformation is
    // well FORMED, which is `tokenizeTransformation`'s job — and
    // `tokenize_transformation` above is that method's port, carrying its exact
    // messages.
    //
    // Live path, not a hypothetical: `AESKeyGenerator.<init>` calls
    // `SecurityProviderConstants.getDefAESKeySize`, which calls
    // `getMaxAllowedKeyLength("AES")`. All of that is real JDK bytecode we do
    // not intercept, so `KeyGenerator.getInstance("AES", "SunJCE")` could not
    // build its SPI without this. Covered by `RCrypto`'s `keygen2arg` checks.
    r.register(
        "javax/crypto/Cipher",
        "getMaxAllowedKeyLength",
        "(Ljava/lang/String;)I",
        |ctx, args| {
            check_transformation_well_formed(ctx, args)?;
            Ok(Some(Value::Int(i32::MAX)))
        },
    );
    r.register(
        "javax/crypto/Cipher",
        "getMaxAllowedParameterSpec",
        "(Ljava/lang/String;)Ljava/security/spec/AlgorithmParameterSpec;",
        |ctx, args| {
            check_transformation_well_formed(ctx, args)?;
            Ok(Some(Value::Object(None)))
        },
    );

    register_cipher_dispatch(r);
    // No `register_keygen_dispatch`: the two 2-arg `KeyGenerator.getInstance`
    // overloads it registered are DELETED, not moved. W7-21 Patch A. They minted
    // a 2-field synthetic `javax/crypto/KeyGenerator` whose `init`/`generateKey`
    // exist only in `phases_early::register_phase53_crypto` — reachable solely
    // from `lib::register_synthetic_overrides` — so in the two shipping modes
    // the very next call ran REAL bytecode against a receiver with no `spi`.
    // The real path they were bypassing works now: `provider_chain` seeds
    // thirteen `SunJCE` `KeyGenerator` services under real JDK class names
    // (W7-39) and the `sun/security/jca/GetInstance` bridges are wired whenever
    // `route_ec_to_real()` is on, which is the default. The 1-arg overload has
    // taken that same real path all along.
    register_param_specs(r);
    r.set_category(__prev_cat);
}

// `register_keygen_dispatch` — DELETED 2026-08-12, W7-21 Patch A. Its two
// registrations (`KeyGenerator.getInstance(String,String)` and
// `(String,Provider)`) were written when the per-provider service tables were
// empty, so the real path NPE'd at `service.getProvider()`. They replaced that
// with a worse failure one call later: the synthetic they returned had the
// `(algorithm@0, keySize@1)` layout of `register_phase53_crypto`'s shim, but
// that registrar is reachable only from `lib::register_synthetic_overrides`, so
// in `--real-jdk` and `--jdk-only` the matching `init` / `generateKey` natives
// DO NOT EXIST and the real bytecode ran against a receiver with no `spi`.
// Worse still under real-JDK, where `try_alloc_concurrent_synthetic` upsizes to
// the real `KeyGenerator` layout and slot 0 is `spi`, not `algorithm` — so the
// algorithm String was written into the SPI field.
//
// Nothing replaces them. `provider_chain::seed_direct_native_engine_services`
// seeds the `SunJCE` `KeyGenerator` services under real JDK class names with
// public no-arg constructors, and the `sun/security/jca/GetInstance` bridges
// answer both the named-provider and the search overloads whenever
// `route_ec_to_real()` is on — the default. The 1-arg overload has taken that
// route in shipping modes all along, which is what makes the deletion safe
// rather than hopeful. `provider_chain`'s own comment above that seed —
// "`KeyGenerator` is NOT natively intercepted in `--real-jdk` mode" — was made
// true by this deletion; these two registrations were the only thing making it
// false. Covered by `RCrypto`'s `keygen2arg` checks.

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
            // The anonymous overload resolves aliases too, against the
            // chain rather than one named provider.
            let algo_str = canonical_transformation(None, &algo_str).unwrap_or(algo_str);
            match check_transformation_supported(ctx, &algo_str, GetInstanceForm::Anonymous) {
                Ok(_) => {
                    // This engine can compute the transformation, but the
                    // anonymous overload is decided by CHAIN ORDER, and an
                    // application may have inserted a provider ahead of the one
                    // this engine answers as. See
                    // `provider_chain::third_party_owner_before`.
                    let candidates: Vec<String> = cipher_transform_candidates(&algo_str)
                        .into_iter()
                        .map(|(service, _, _)| service)
                        .collect();
                    if let Some(provider) = crate::jca::provider_chain::third_party_owner_before(
                        "Cipher",
                        &candidates,
                        CIPHER_NATIVE_PROVIDER,
                    ) {
                        let mut obj = cipher_alloc(ctx, &algo_str)?;
                        if try_delegate_cipher_to_named_provider(
                            ctx, &provider, &algo_str, &mut obj,
                        )? {
                            let obj = record_provider_and_reread(ctx, obj, &provider);
                            return Ok(Some(Value::Object(Some(obj))));
                        }
                    }
                    let obj = cipher_alloc(ctx, &algo_str)?;
                    Ok(Some(Value::Object(Some(obj))))
                }
                Err(refusal) => {
                    // Not a name this engine computes. Ask the installed
                    // providers, in chain order, before refusing — see
                    // `try_delegate_cipher_to_chain`.
                    let mut obj = cipher_alloc(ctx, &algo_str)?;
                    match try_delegate_cipher_to_chain(ctx, &algo_str, &mut obj)? {
                        true => Ok(Some(Value::Object(Some(obj)))),
                        false => Err(refusal),
                    }
                }
            }
        },
    );
    r.register(
        cipher,
        "getInstance",
        "(Ljava/lang/String;Ljava/lang/String;)Ljavax/crypto/Cipher;",
        |ctx, args| {
            // Real `Cipher.getInstance(String, String)` resolves the provider
            // name first (and capitalises both of its messages, unlike the
            // shared `GetInstance` path) — see `check_named_provider_arg`.
            crate::jca::provider_chain::check_named_provider_arg(
                ctx,
                args,
                1,
                crate::jca::provider_chain::ProviderArgWording::Cipher,
            )?;
            let algo = obj_arg(args, 0)?;
            let algo_str = ctx.read_string(algo).unwrap_or_default();
            crate::jca::provider_chain::check_provider_ownership(
                ctx,
                args,
                1,
                "Cipher",
                &algo_str,
                crate::jca::provider_chain::ProviderArgWording::Cipher,
            )?;
            cipher_get_instance_with_provider(ctx, args, &algo_str)
        },
    );
    r.register(
        cipher,
        "getInstance",
        "(Ljava/lang/String;Ljava/security/Provider;)Ljavax/crypto/Cipher;",
        |ctx, args| {
            let algo = obj_arg(args, 0)?;
            let algo_str = ctx.read_string(algo).unwrap_or_default();
            crate::jca::provider_chain::check_provider_ownership(
                ctx,
                args,
                1,
                "Cipher",
                &algo_str,
                crate::jca::provider_chain::ProviderArgWording::Cipher,
            )?;
            cipher_get_instance_with_provider(ctx, args, &algo_str)
        },
    );

    r.register(cipher, "init", "(ILjava/security/Key;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let mode = args[1].as_int().unwrap_or(0);
        let key = obj_arg(args, 2)?;
        if cipher_is_delegated(ctx, this) {
            return cipher_delegate_init(
                ctx,
                this,
                mode,
                Some(key),
                None,
                CipherInitParams::None,
                None,
            );
        }
        // A ChaCha20 cipher initialised with no parameters at all still needs a
        // nonce, and SunJCE GENERATES one for ENCRYPT rather than refusing —
        // the caller recovers it through `getIV()`. Routing this overload
        // through the same helper as the spec-taking ones is what makes
        // `init(ENCRYPT_MODE, key)` work at all.
        let algo = cipher_algorithm_of(ctx, this);
        if let Some((nonce, counter)) = chacha20_spec_params(ctx, &algo, None, mode)? {
            return cipher_init_record_with_counter(ctx, this, mode, key, nonce, counter);
        }
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
            let spec = match args.get(3) {
                Some(Value::Object(Some(spec))) => Some(*spec),
                _ => None,
            };
            if cipher_is_delegated(ctx, this) {
                return cipher_delegate_init(
                    ctx,
                    this,
                    mode,
                    Some(key),
                    spec,
                    CipherInitParams::Spec,
                    None,
                );
            }
            // ChaCha20 needs the spec's TYPE and its counter, not just its
            // field 0, so it is resolved before the generic IV read.
            let algo = cipher_algorithm_of(ctx, this);
            if let Some((nonce, counter)) = chacha20_spec_params(ctx, &algo, spec, mode)? {
                return cipher_init_record_with_counter(ctx, this, mode, key, nonce, counter);
            }
            let iv_bytes = match spec {
                Some(spec) => extract_iv_bytes(ctx, spec),
                None => Vec::new(),
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
            let spec = match args.get(3) {
                Some(Value::Object(Some(spec))) => Some(*spec),
                _ => None,
            };
            if cipher_is_delegated(ctx, this) {
                return cipher_delegate_init(
                    ctx,
                    this,
                    mode,
                    Some(key),
                    spec,
                    CipherInitParams::Spec,
                    obj_at(args, 4),
                );
            }
            // ChaCha20 needs the spec's TYPE and its counter, not just its
            // field 0, so it is resolved before the generic IV read.
            let algo = cipher_algorithm_of(ctx, this);
            if let Some((nonce, counter)) = chacha20_spec_params(ctx, &algo, spec, mode)? {
                return cipher_init_record_with_counter(ctx, this, mode, key, nonce, counter);
            }
            let iv_bytes = match spec {
                Some(spec) => extract_iv_bytes(ctx, spec),
                None => Vec::new(),
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
            if cipher_is_delegated(ctx, this) {
                return cipher_delegate_init(
                    ctx,
                    this,
                    mode,
                    Some(key),
                    alg_params,
                    CipherInitParams::Params,
                    None,
                );
            }
            cipher_init_from_algorithm_parameters(ctx, this, mode, key, alg_params)
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
            if cipher_is_delegated(ctx, this) {
                return cipher_delegate_init(
                    ctx,
                    this,
                    mode,
                    Some(key),
                    alg_params,
                    CipherInitParams::Params,
                    obj_at(args, 4),
                );
            }
            cipher_init_from_algorithm_parameters(ctx, this, mode, key, alg_params)
        },
    );

    // `Cipher.init(mode, Certificate)` and its `SecureRandom` twin. Both are
    // `Cipher.getPublicKey(cert)` followed by the `Key` form — the JDK body is
    // literally that, plus a KeyUsage check — but neither was registered, so
    // both fell through to real `Cipher` bytecode against a receiver whose state
    // lives in `CIPHER_TABLE`. S/MIME and CMS enveloping reach for exactly this
    // overload when the recipient is named by certificate rather than by key.
    //
    // The key-usage rule mirrors `javax.crypto.Cipher.init(int, Certificate)`:
    // an X.509 certificate whose KeyUsage extension is PRESENT and denies
    // `keyEncipherment` (bit 2) must be refused. An absent extension is a null
    // array and is silently allowed, which makes this a no-op for every
    // certificate that does not carry it.
    for desc in [
        "(ILjava/security/cert/Certificate;)V",
        "(ILjava/security/cert/Certificate;Ljava/security/SecureRandom;)V",
    ] {
        r.register(cipher, "init", desc, |ctx, args| {
            let this = obj_arg(args, 0)?;
            let mode = args[1].as_int().unwrap_or(0);
            let cert = obj_arg(args, 2)?;
            let cert_pin = ctx.pin_native_root(cert);
            let this_pin = ctx.pin_native_root(this);
            let usage_denies_encipherment =
                match ctx.invoke_virtual(cert, "getKeyUsage", "()[Z", &[]) {
                    Ok(Some(Value::Object(Some(arr)))) => {
                        ctx.array_length(arr) > 2 && ctx.get_array_element(arr, 2) == Value::Int(0)
                    }
                    _ => false,
                };
            let cert = ctx.read_native_pin(cert_pin, cert);
            let this = ctx.read_native_pin(this_pin, this);
            if usage_denies_encipherment {
                ctx.unpin_native_roots(cert_pin);
                return Err(crate::phases_early::throw_jca_exc(
                    ctx,
                    "java/security/InvalidKeyException",
                    "Wrong key usage",
                ));
            }
            let key = ctx.invoke_virtual(cert, "getPublicKey", "()Ljava/security/PublicKey;", &[]);
            let this = ctx.read_native_pin(this_pin, this);
            ctx.unpin_native_roots(cert_pin);
            let key = match key? {
                Some(Value::Object(Some(k))) => k,
                _ => {
                    return Err(crate::phases_early::throw_jca_exc(
                        ctx,
                        "java/security/InvalidKeyException",
                        "certificate carries no public key",
                    ))
                }
            };
            if cipher_is_delegated(ctx, this) {
                return cipher_delegate_init(
                    ctx,
                    this,
                    mode,
                    Some(key),
                    None,
                    CipherInitParams::None,
                    obj_at(args, 3),
                );
            }
            cipher_init_record(ctx, this, mode, key, Vec::new())
        });
    }

    // `getExemptionMechanism()` — the last unregistered public method on
    // `javax.crypto.Cipher`. There is no exemption mechanism here (and none on a
    // stock JDK either: `Cipher.getInstance("AES").getExemptionMechanism()` is
    // null on HotSpot 25), but leaving it unregistered means the real body runs
    // `chooseFirstProvider()` against a synthetic receiver.
    r.register(
        cipher,
        "getExemptionMechanism",
        "()Ljavax/crypto/ExemptionMechanism;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );

    r.register(
        cipher,
        "init",
        "(ILjava/security/Key;Ljava/security/SecureRandom;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let mode = args[1].as_int().unwrap_or(0);
            let key = obj_arg(args, 2)?;
            if cipher_is_delegated(ctx, this) {
                return cipher_delegate_init(
                    ctx,
                    this,
                    mode,
                    Some(key),
                    None,
                    CipherInitParams::None,
                    obj_at(args, 3),
                );
            }
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
        if cipher_is_delegated(ctx, this) {
            let input = match args.get(1) {
                Some(Value::Object(Some(b))) => Some(*b),
                _ => None,
            };
            let len = input.map_or(0, |b| ctx.array_length(b) as i32);
            return cipher_delegate_bytes(ctx, this, "engineUpdate", input, 0, len);
        }
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

    // `update(input, inputOffset, inputLen)` → byte[]. Registered 2026-08-13.
    //
    // Left unregistered it fell through to the real `Cipher.update` bytecode,
    // whose `checkCipherState()` throws `IllegalStateException: Cipher not
    // initialized` — because native `init` keeps its state in `CIPHER_TABLE`
    // and never writes the real object's SPI fields. That is the same species
    // the `update(ByteBuffer,ByteBuffer)` note below records, and it is the
    // overload every streaming caller reaches: BouncyCastle's
    // `jcajce.io.CipherInputStream.nextChunk` calls exactly this, which is how
    // a provider-delegated PKCS#12 PBE decrypt died one step after being
    // correctly set up.
    //
    // Non-delegated behaviour is deliberately identical to `update([B)[B`:
    // buffer into the accumulator and return an empty array, leaving the whole
    // result to `doFinal`.
    r.register(cipher, "update", "([BII)[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if cipher_is_delegated(ctx, this) {
            let input = match args.get(1) {
                Some(Value::Object(Some(b))) => Some(*b),
                _ => None,
            };
            let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
            let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
            return cipher_delegate_bytes(ctx, this, "engineUpdate", input, off, len);
        }
        accumulate_slice(ctx, this, args.get(1), args.get(2), args.get(3));
        let empty = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 0);
        Ok(Some(Value::Object(Some(empty))))
    });

    // update(ByteBuffer,ByteBuffer)I — same "buffered until doFinal" contract as
    // `update([B)[B` above: drain the input's remaining bytes into the
    // accumulator and write nothing, so this returns 0.
    //
    // Left unregistered, the real JDK body ran against a `Cipher` whose real
    // instance fields no `getInstance` ever wrote and threw
    // `IllegalStateException: Cipher not initialized` on a cipher that
    // `init` + `doFinal` had just used successfully — the same species as
    // `Mac.doFinal([BI)V` (see `phases_late::ssl_security`). Netty and the JDK's
    // own `SSLEngine` paths use the ByteBuffer overloads.
    r.register(
        cipher,
        "update",
        "(Ljava/nio/ByteBuffer;Ljava/nio/ByteBuffer;)I",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let Some(Value::Object(Some(input))) = args.get(1).cloned() else {
                return Ok(Some(Value::Int(0)));
            };
            let remaining = match ctx.invoke_virtual(input, "remaining", "()I", &[])? {
                Some(Value::Int(n)) if n > 0 => n as usize,
                _ => return Ok(Some(Value::Int(0))),
            };
            let tmp = ctx.new_array(cratonvm_types::ArrayElementType::Byte, remaining);
            // Bulk get consumes the input and advances position → limit, which
            // is what `Cipher.update(ByteBuffer,ByteBuffer)` promises.
            ctx.invoke_virtual(
                input,
                "get",
                "([B)Ljava/nio/ByteBuffer;",
                &[Value::Object(Some(tmp))],
            )?;
            // A delegated cipher must see its own bytes. Buffering them here
            // instead would hand the provider nothing at `doFinal` — the same
            // silent-loss shape the `updateAAD` note records.
            if cipher_is_delegated(ctx, this) {
                let res = cipher_delegate_bytes(
                    ctx,
                    this,
                    "engineUpdate",
                    Some(tmp),
                    0,
                    remaining as i32,
                )?;
                return cipher_put_result_into_buffer(ctx, args.get(2).cloned(), res);
            }
            let bytes = read_bytes(ctx, tmp);
            let tkey = obj_key(ctx, this);
            with_table_write(|t| {
                if let Some(s) = t.get_mut(&tkey) {
                    s.accumulated.extend_from_slice(&bytes);
                }
            });
            Ok(Some(Value::Int(0)))
        },
    );

    // doFinal(ByteBuffer,ByteBuffer)I — the partner of the `update` overload
    // above. Registering only `update` would have been worse than registering
    // neither: the input would be consumed into the accumulator and the
    // `doFinal` that must flush it would still hit the real JDK body and throw
    // `Cipher not initialized`, losing the plaintext silently.
    r.register(
        cipher,
        "doFinal",
        "(Ljava/nio/ByteBuffer;Ljava/nio/ByteBuffer;)I",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let delegated = cipher_is_delegated(ctx, this);
            let mut pending: Option<ObjectRef> = None;
            let mut pending_len = 0i32;
            if let Some(Value::Object(Some(input))) = args.get(1).cloned() {
                if let Some(Value::Int(n)) = ctx.invoke_virtual(input, "remaining", "()I", &[])? {
                    if n > 0 {
                        let tmp = ctx.new_array(cratonvm_types::ArrayElementType::Byte, n as usize);
                        ctx.invoke_virtual(
                            input,
                            "get",
                            "([B)Ljava/nio/ByteBuffer;",
                            &[Value::Object(Some(tmp))],
                        )?;
                        if delegated {
                            pending = Some(tmp);
                            pending_len = n;
                        } else {
                            let bytes = read_bytes(ctx, tmp);
                            let tkey = obj_key(ctx, this);
                            with_table_write(|t| {
                                if let Some(s) = t.get_mut(&tkey) {
                                    s.accumulated.extend_from_slice(&bytes);
                                }
                            });
                        }
                    }
                }
            }
            // See the `update(ByteBuffer,ByteBuffer)` sibling — a delegated
            // cipher finishes through its provider's own SPI.
            if delegated {
                let res =
                    cipher_delegate_bytes(ctx, this, "engineDoFinal", pending, 0, pending_len)?;
                return cipher_put_result_into_buffer(ctx, args.get(2).cloned(), res);
            }
            let out_bytes = match cipher_do_final_impl(ctx, this)? {
                Some(Value::Object(Some(a))) => read_bytes(ctx, a),
                // Same reasoning as `doFinal([BII[B)I`: this impl returns either
                // `Err` or a real byte[], so any other shape is an unexpected
                // state, not a legitimately empty ciphertext. Refusing beats
                // reporting "0 bytes written" as success.
                other => {
                    return Err(RuntimeError::IllegalStateException {
                        message: format!(
                            "Cipher.doFinal produced no output buffer ({other:?}); \
                             refusing to report 0 bytes written as success"
                        ),
                    }
                    .into());
                }
            };
            let Some(Value::Object(Some(output))) = args.get(2).cloned() else {
                return Err(RuntimeError::NullPointerException {
                    message: Some("output ByteBuffer is null".to_string()),
                }
                .into());
            };
            let written = out_bytes.len();
            if written > 0 {
                let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, written);
                ctx.write_byte_array_from(arr, 0, &out_bytes);
                // Through the buffer's own `put`, so heap and DIRECT buffers take
                // one path and the position advances as the contract requires.
                ctx.invoke_virtual(
                    output,
                    "put",
                    "([B)Ljava/nio/ByteBuffer;",
                    &[Value::Object(Some(arr))],
                )?;
            }
            Ok(Some(Value::Int(written as i32)))
        },
    );

    // getProvider()Ljava/security/Provider; — the real body opens
    // `synchronized (lock)` on a field this synthetic never wrote, so it threw
    // `NullPointerException: Cannot enter synchronized block because
    // "this.lock" is null` on a Cipher that encrypts and decrypts correctly.
    r.register(
        cipher,
        "getProvider",
        "()Ljava/security/Provider;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // A provider the caller NAMED at `getInstance` — including one whose
            // own `CipherSpi` is doing the work through the delegation path
            // below — is the answer HotSpot gives. `SunJCE` remains right for the
            // anonymous overload, which this VM genuinely does serve itself.
            if let Some(p) = crate::jca::provider_chain::recorded_requested_provider(ctx, this) {
                return Ok(Some(Value::Object(Some(p))));
            }
            let p = crate::jca::make_named_provider(ctx, "SunJCE")?;
            Ok(Some(Value::Object(Some(p))))
        },
    );

    r.register(cipher, "updateAAD", "([B)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // A provider-delegated AEAD cipher must be given its OWN associated
        // data. Without this branch the bytes landed in our side table and the
        // provider encrypted without them — silently, since an AEAD encrypt
        // with no AAD succeeds and simply produces a different tag. Measured on
        // bc-java's `AEADTest.checkCipherWithAD` (`AES/EAX/NoPadding` from
        // "BC", a transformation this VM does not implement and therefore
        // always delegates): "JCE encrypt with additional data failed", i.e.
        // the ciphertext did not match the KAT vector.
        if cipher_is_delegated(ctx, this) {
            if let Some(spi) = cipher_delegate_spi(ctx, this) {
                let (arr, len) = match args.get(1) {
                    Some(Value::Object(Some(b))) => (Some(*b), ctx.array_length(*b) as i32),
                    _ => (None, 0),
                };
                return ctx.invoke_virtual(
                    spi,
                    "engineUpdateAAD",
                    "([BII)V",
                    &[Value::Object(arr), Value::Int(0), Value::Int(len)],
                );
            }
        }
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

    // The two write-into-my-buffer `update` overloads. Left unregistered they
    // fell through to the real `Cipher.update` bytecode, whose
    // `checkCipherState()` throws `IllegalStateException: Cipher not
    // initialized` against a receiver whose state lives in `CIPHER_TABLE` —
    // measured on bc-java's `AEADTest.testGCMParameterSpecWithMultipleUpdates`,
    // which streams through `update(in, off, len, out, outOff)`. Same species
    // as the `doFinal([BII[B)I` note below and the `Mac.doFinal([BI)V` one in
    // `phases_late/ssl_security.rs`: the object looks healthy right up to the
    // one overload nobody registered.
    for desc in ["([BII[B)I", "([BII[BI)I"] {
        r.register(cipher, "update", desc, |ctx, args| {
            let this = obj_arg(args, 0)?;
            let input = match args.get(1) {
                Some(Value::Object(Some(b))) => Some(*b),
                _ => None,
            };
            let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
            let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
            let output = obj_arg(args, 4)?;
            let out_off = args.get(5).and_then(|v| v.as_int()).unwrap_or(0);
            if cipher_is_delegated(ctx, this) {
                // The provider writes into the caller's buffer itself and owns
                // the short-buffer refusal; see `cipher_delegate_into_buffer`.
                return cipher_delegate_into_buffer(
                    ctx,
                    this,
                    "engineUpdate",
                    input,
                    off,
                    len,
                    output,
                    out_off,
                );
            }
            // Non-delegated behaviour matches the other `update` overloads:
            // buffer into the accumulator, produce nothing until `doFinal`.
            accumulate_slice(ctx, this, args.get(1), args.get(2), args.get(3));
            Ok(Some(Value::Int(0)))
        });
    }

    // The other two `updateAAD` overloads. Left unregistered they fell through
    // to the real `Cipher.updateAAD` bytecode, whose `checkCipherState()` throws
    // `IllegalStateException: Cipher not initialized` against a receiver whose
    // state lives in `CIPHER_TABLE` — so a caller that passed its AAD by range
    // or by `ByteBuffer` got an exception where the array overload silently
    // dropped it. One engine, three doors, one behaviour.
    // The `ByteBuffer` AAD door. The note above says "one engine, three doors,
    // one behaviour" and then registered TWO — a caller that passed its AAD as a
    // buffer still fell through to the real `Cipher.updateAAD` bytecode and its
    // `checkCipherState()`. `javax.crypto.Cipher` declares exactly three.
    r.register(
        cipher,
        "updateAAD",
        "(Ljava/nio/ByteBuffer;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let Some(Value::Object(Some(src))) = args.get(1).cloned() else {
                return Ok(None);
            };
            let remaining = match ctx.invoke_virtual(src, "remaining", "()I", &[])? {
                Some(Value::Int(n)) if n > 0 => n as usize,
                _ => return Ok(None),
            };
            let tmp = ctx.new_array(cratonvm_types::ArrayElementType::Byte, remaining);
            // Bulk get consumes the buffer and advances position -> limit, which
            // is what `Cipher.updateAAD(ByteBuffer)` promises.
            ctx.invoke_virtual(
                src,
                "get",
                "([B)Ljava/nio/ByteBuffer;",
                &[Value::Object(Some(tmp))],
            )?;
            if cipher_is_delegated(ctx, this) {
                if let Some(spi) = cipher_delegate_spi(ctx, this) {
                    return ctx.invoke_virtual(
                        spi,
                        "engineUpdateAAD",
                        "([BII)V",
                        &[
                            Value::Object(Some(tmp)),
                            Value::Int(0),
                            Value::Int(remaining as i32),
                        ],
                    );
                }
            }
            let bytes = read_bytes(ctx, tmp);
            let tkey = obj_key(ctx, this);
            with_table_write(|t| {
                if let Some(s) = t.get_mut(&tkey) {
                    s.aad.extend_from_slice(&bytes);
                }
            });
            Ok(None)
        },
    );

    r.register(cipher, "updateAAD", "([BII)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let arr = match args.get(1) {
            Some(Value::Object(Some(b))) => Some(*b),
            _ => None,
        };
        let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
        let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
        if cipher_is_delegated(ctx, this) {
            if let Some(spi) = cipher_delegate_spi(ctx, this) {
                return ctx.invoke_virtual(
                    spi,
                    "engineUpdateAAD",
                    "([BII)V",
                    &[Value::Object(arr), Value::Int(off), Value::Int(len)],
                );
            }
        }
        if let Some(arr) = arr {
            let bytes = read_bytes(ctx, arr);
            let start = (off.max(0) as usize).min(bytes.len());
            let end = start.saturating_add(len.max(0) as usize).min(bytes.len());
            let slice = bytes[start..end].to_vec();
            let tkey = obj_key(ctx, this);
            with_table_write(|t| {
                if let Some(s) = t.get_mut(&tkey) {
                    s.aad.extend_from_slice(&slice);
                }
            });
        }
        Ok(None)
    });

    r.register(cipher, "doFinal", "([B)[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if cipher_is_delegated(ctx, this) {
            let input = match args.get(1) {
                Some(Value::Object(Some(b))) => Some(*b),
                _ => None,
            };
            let len = input.map_or(0, |b| ctx.array_length(b) as i32);
            return cipher_delegate_bytes(ctx, this, "engineDoFinal", input, 0, len);
        }
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
        if cipher_is_delegated(ctx, this) {
            return cipher_delegate_bytes(ctx, this, "engineDoFinal", None, 0, 0);
        }
        cipher_do_final_impl(ctx, this)
    });

    // `doFinal(input, inputOffset, inputLen)` → byte[]. The offset/length
    // variant keycloak's AES-GCM decrypt and several BC callers use.
    r.register(cipher, "doFinal", "([BII)[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if cipher_is_delegated(ctx, this) {
            let input = match args.get(1) {
                Some(Value::Object(Some(b))) => Some(*b),
                _ => None,
            };
            let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
            let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
            return cipher_delegate_bytes(ctx, this, "engineDoFinal", input, off, len);
        }
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
    // The three write-into-my-buffer `doFinal` overloads. All of them are one
    // engine call with the output range spelled differently, so they share one
    // body: `([BI)I` finalises whatever was accumulated, `([BII[B)I` writes at
    // offset 0, `([BII[BI)I` at the caller's offset. Only the first was
    // registered as its own arm; the other two fell through to the real
    // `Cipher` bytecode, whose `checkCipherState()` throws `IllegalStateException:
    // Cipher not initialized` against a receiver whose state lives in
    // `CIPHER_TABLE` — measured on bc-java's
    // `AEADTest.testGCMParameterSpecWithMultipleUpdates`, which streams through
    // `update(in, off, len, out, outOff)` and finishes with
    // `doFinal(in, off, len, out, outOff)`.
    for desc in ["([BI)I", "([BII[B)I", "([BII[BI)I"] {
        r.register(cipher, "doFinal", desc, |ctx, args| {
            let this = obj_arg(args, 0)?;
            // `register` takes a fn pointer, so which overload is running has to
            // come from the arguments themselves: receiver + 2 for `([BI)I`,
            // + 4 for `([BII[B)I`, + 5 for `([BII[BI)I`.
            let has_input = args.len() >= 5;
            let (input, off, len, output, out_off) = if has_input {
                let input = match args.get(1) {
                    Some(Value::Object(Some(b))) => Some(*b),
                    _ => None,
                };
                let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
                let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
                let output = obj_arg(args, 4)?;
                let out_off = args.get(5).and_then(|v| v.as_int()).unwrap_or(0);
                (input, off, len, output, out_off)
            } else {
                let output = obj_arg(args, 1)?;
                let out_off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
                (None, 0, 0, output, out_off)
            };
            // The three sibling `doFinal` overloads route a delegated cipher to
            // its provider; these did not, so a caller reaching for the
            // write-into-my-buffer form got OUR implementation of a
            // transformation the provider was chosen to supply. It goes to the
            // provider's OWN write-into-the-buffer method rather than to the
            // array-returning one plus a bounds check here — see
            // `cipher_delegate_into_buffer`. `Cipher.doFinal(byte[], int)`
            // passes `(null, 0, 0, output, outputOffset)`, which is exactly
            // what the no-input overload builds above.
            if cipher_is_delegated(ctx, this) {
                return cipher_delegate_into_buffer(
                    ctx,
                    this,
                    "engineDoFinal",
                    input,
                    off,
                    len,
                    output,
                    out_off,
                );
            }
            if has_input {
                accumulate_slice(ctx, this, args.get(1), args.get(2), args.get(3));
            }
            let opin = ctx.pin_native_root(output);
            let res = cipher_do_final_impl(ctx, this);
            let out_bytes = match res {
                Ok(Some(Value::Object(Some(a)))) => read_bytes(ctx, a),
                // P0: this used to be `Ok(_) => Vec::new()`, which wrote nothing
                // into the caller's buffer and returned 0 — "the cipher produced
                // zero bytes", indistinguishable from a legitimate empty result.
                // `cipher_do_final_impl` only ever returns `Err` or a real byte[],
                // so reaching here means an unexpected shape, not an empty
                // ciphertext. Refuse rather than report a successful zero-length
                // encryption.
                Ok(other) => {
                    ctx.unpin_native_roots(opin);
                    return Err(RuntimeError::IllegalStateException {
                        message: format!(
                            "Cipher.doFinal produced no output buffer ({other:?});                              refusing to report 0 bytes written as success"
                        ),
                    }
                    .into());
                }
                Err(e) => {
                    ctx.unpin_native_roots(opin);
                    return Err(e);
                }
            };
            let output = ctx.read_native_pin(opin, output);
            ctx.unpin_native_roots(opin);
            // The caller's buffer has to be able to hold the result. This loop
            // used to write `out_bytes.len()` elements into `output` with NO
            // bounds check at all — a 32-byte ciphertext into a 1-byte array —
            // so the census answer for `ShortBufferException` on this overload
            // was "raises NOTHING of its own"; what a caller saw was whatever
            // `set_array_element` did past the end, which is an
            // `ArrayIndexOutOfBoundsException` at best and is unchecked either
            // way.
            //
            // SunJCE measured on Temurin 25.0.3+9:
            // `ShortBufferException: Output buffer must be (at least) 32 bytes
            // long` — checked, and the exception `doFinal(byte[],int,int,byte[])`
            // declares. The check is made against the ACTUAL output length
            // rather than `getOutputSize`, which is documented one screen down
            // as an UPPER bound: refusing on the estimate would reject buffers
            // that fit.
            let out_off = out_off.max(0) as usize;
            if ctx.array_length(output) < out_off.saturating_add(out_bytes.len()) {
                return Err(crate::phases_early::throw_jca_exc(
                    ctx,
                    "javax/crypto/ShortBufferException",
                    &format!(
                        "Output buffer must be (at least) {} bytes long",
                        out_off + out_bytes.len()
                    ),
                ));
            }
            for (i, &b) in out_bytes.iter().enumerate() {
                ctx.set_array_element(output, out_off + i, Value::Int(b as i8 as i32));
            }
            Ok(Some(Value::Int(out_bytes.len() as i32)))
        });
    }

    // `getOutputSize(inputLen)` → the byte count `doFinal` will produce, so the
    // caller can pre-size its output buffer. Must be EXACT for the AES-GCM
    // encrypt path (the provider uses the whole array, not the returned count):
    // GCM encrypt adds a 16-byte tag, GCM decrypt strips it.
    r.register(cipher, "getOutputSize", "(I)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if cipher_is_delegated(ctx, this) {
            if let Some(spi) = cipher_delegate_spi(ctx, this) {
                let n = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
                return ctx.invoke_virtual(spi, "engineGetOutputSize", "(I)I", &[Value::Int(n)]);
            }
        }
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
            // Block cipher with PKCS padding: round up to the next whole block,
            // and PKCS#7 always adds one — an exact multiple gains a full block
            // of padding, which is why this is `total / b + 1` and not a ceiling.
            // The block size is the CIPHER's, taken from `cipher_block_size`;
            // this line read `(total / 16 + 1) * 16` until 2026-08-12, which
            // over-reported every 8-byte-block transformation and rounded a
            // stream cipher up to a block boundary it does not have.
            //
            // `_pad` is deliberately still ignored: `getOutputSize` is an UPPER
            // bound, and rounding a `NoPadding` cipher up costs a caller a few
            // spare bytes while getting it wrong the other way costs them a
            // `ShortBufferException`. RFC 3394 key wrap is the case that pins
            // this — `AES/KW/NoPadding` outputs input+8, so an exact-length
            // answer would be short.
            match cipher_block_size(&algo) {
                0 => total,
                b => (total / b + 1) * b,
            }
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
        // `getAlgorithm`/`getProvider` must be registered HERE, not only beside
        // the identical trio in `phases_early::register_phase53_natives`: that
        // registrar is reached only from `register_synthetic_overrides`, which
        // is `#[cfg(feature = "synthetic-jdk")]`. THIS module is the copy that
        // runs in the default real-JDK mode — which is why the phase-53 pair
        // alone left `getProvider()` still throwing `NullPointerException:
        // Cannot enter synchronized block because "this.lock" is null` on a
        // real-JDK run. An inert registration looks exactly like a missing one.
        r.register(
            skf,
            "getAlgorithm",
            "()Ljava/lang/String;",
            crate::phases_early::pbkdf2_get_algorithm,
        );
        r.register(
            skf,
            "getProvider",
            "()Ljava/security/Provider;",
            crate::phases_early::pbkdf2_get_provider,
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

    // STUB-REMOVAL (wave 2): this returned a hard-coded 16 for every Cipher.
    // `getBlockSize()` is contractually 0 for a stream cipher or an
    // asymmetric/"not a block cipher" transformation, and 8 for the 64-bit
    // block ciphers — a caller that sizes a buffer or aligns a loop on 16 for
    // DESede/Blowfish/RC2 silently mis-frames its data, and one that tests
    // `getBlockSize() == 0` to detect a stream cipher takes the wrong branch
    // for RC4/ChaCha20/RSA. Derive it from the transformation actually
    // configured at `init` time.
    r.register(cipher, "getBlockSize", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if cipher_is_delegated(ctx, this) {
            if let Some(spi) = cipher_delegate_spi(ctx, this) {
                return ctx.invoke_virtual(spi, "engineGetBlockSize", "()I", &[]);
            }
        }
        let tkey = obj_key(ctx, this);
        let algo = with_table_read(|t| {
            t.get(&tkey)
                .map(|s| s.algorithm.clone())
                .unwrap_or_default()
        });
        Ok(Some(Value::Int(cipher_block_size(&algo) as i32)))
    });

    // STUB-REMOVAL (wave 2), competing registration: `getOutputSize(I)I` was
    // registered TWICE on `javax/crypto/Cipher` inside this one function — the
    // documented, GCM/RSA/PKCS-padding-aware implementation ~80 lines above,
    // and a second, cruder copy right here. The registry is last-writer-wins,
    // so the crude copy was the one that ran, and it was wrong in three ways
    // that all silently truncate or oversize a caller's buffer: it ignored
    // bytes already buffered by `update()` (so a streaming caller under-sized
    // its output array), it treated every non-GCM transformation as a
    // 16-byte-block cipher (wrong for RSA, whose output is the modulus size),
    // and its `((len + 15) / 16) * 16` rounding returns `len` unchanged for an
    // exact block multiple where PKCS#7 requires a whole extra padding block.
    // It also indexed `args[1]` directly, which panics on a short arg list.
    // Removed; the earlier registration is now the live one.

    r.register(cipher, "getIV", "()[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if cipher_is_delegated(ctx, this) {
            if let Some(spi) = cipher_delegate_spi(ctx, this) {
                return ctx.invoke_virtual(spi, "engineGetIV", "()[B", &[]);
            }
        }
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
            if cipher_is_delegated(ctx, this) {
                if let Some(spi) = cipher_delegate_spi(ctx, this) {
                    return ctx.invoke_virtual(
                        spi,
                        "engineGetParameters",
                        "()Ljava/security/AlgorithmParameters;",
                        &[],
                    );
                }
            }
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
                // Not PBES2. `getParameters()` still has to answer the IV this
                // cipher is running with — that is how a caller persists it
                // (`AlgorithmIdentifier` in CMS, `AlgorithmId` in PKCS#12) and
                // how the decrypt side gets it back. Answering null made every
                // such caller write "no parameters" and then fail to decrypt.
                return cipher_iv_parameters(ctx, &algo, &iv);
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
    // `IvParameterSpec.getIV()` is `return this.iv.clone()` in the real class,
    // and `GCMParameterSpec.getIV()` likewise. Handing back the STORED array
    // let a caller edit the spec's IV in place — measured: `getIV()`, fill with
    // 0x77, `getIV()` again returns 0x77s where HotSpot returns the original.
    // The constructors already copied; only the accessors leaked.
    r.register(ivps, "getIV", "()[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Object(clone_byte_field(ctx, this, 0))))
    });

    let gcmps = "javax/crypto/spec/GCMParameterSpec";
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
        Ok(Some(Value::Object(clone_byte_field(ctx, this, 0))))
    });
    r.register(gcmps, "getTLen", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });

    let sks = "javax/crypto/spec/SecretKeySpec";
    r.register(sks, "<clinit>", "()V", clinit_noop);
    // `SecretKeySpec.<init>` is `this.key = key.clone()` in the real class
    // (`java.base/javax/crypto/spec/SecretKeySpec.java`, JDK 25 `src.zip`), and
    // the javadoc states why: "The contents of the array are copied to protect
    // against subsequent modification."
    //
    // Storing the caller's array by reference instead was not a shortcut, it
    // was an ALL-ZERO AES KEY. SunJCE's own key generators scrub their working
    // buffer the instant the key object exists —
    // `AESKeyGenerator.engineGenerateKey` is `new SecretKeySpec(keyBytes,
    // "AES"); Arrays.fill(keyBytes, (byte)0);` and
    // `KeyGeneratorCore.implGenerateKey` does the same in a `finally` — so the
    // scrub landed on the key itself. Measured on this tree's release binary,
    // real-JDK and --jdk-only alike:
    //
    //     KeyGenerator.getInstance("AES").generateKey().getEncoded()
    //       CratonVM -> 0000000000000000000000000000000000000000000000000000000000000000
    //       HotSpot  -> a55a06fd157aca61fea2322615a02b6c71c01805cbefca7b89ea10b26efc1ace
    //
    // and identically for `HmacSHA256`. `DESede` was unaffected only because
    // `DESedeKeyGenerator` happens not to scrub. Nothing raised anywhere: the
    // key had the right LENGTH and the right algorithm name, so every caller
    // downstream encrypted, signed and stored under a key of all zeros.
    r.register(sks, "<init>", "([BLjava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // The real `<init>` (JDK 25 src.zip) opens with
        //     if (key == null || algorithm == null)
        //         throw new IllegalArgumentException("Missing argument");
        //     if (key.length == 0) throw new IllegalArgumentException("Empty key");
        // Both checks precede any use. A zero-length key is exactly the
        // artefact the all-zero-key family produces, so accepting it removes
        // the one place the platform would have caught it. The null case is a
        // CLASS difference, not wording: `obj_arg` raises NullPointerException
        // (native-builtins/src/lib.rs:25137), which a caller's
        // `catch (IllegalArgumentException)` does not catch — so the null test
        // must run BEFORE obj_arg. W7-21 Patch C.
        if !matches!(args.get(1), Some(Value::Object(Some(_))))
            || !matches!(args.get(2), Some(Value::Object(Some(_))))
        {
            return Err(crate::phases_early::throw_jca_exc(
                ctx,
                "java/lang/IllegalArgumentException",
                "Missing argument",
            ));
        }
        let key_bytes = obj_arg(args, 1)?;
        let algo = obj_arg(args, 2)?;
        if ctx.array_length(key_bytes) == 0 {
            return Err(crate::phases_early::throw_jca_exc(
                ctx,
                "java/lang/IllegalArgumentException",
                "Empty key",
            ));
        }
        let raw = read_bytes(ctx, key_bytes);
        // `make_bytes_array` allocates, so both refs we still need must survive
        // a moving collection. Unpinning from the FIRST handle releases both.
        let this_pin = ctx.pin_native_root(this);
        let algo_pin = ctx.pin_native_root(algo);
        let copy = make_bytes_array(ctx, &raw);
        let this = ctx.read_native_pin(this_pin, this);
        let algo = ctx.read_native_pin(algo_pin, algo);
        ctx.unpin_native_roots(this_pin);
        ctx.set_field(this, 0, Value::Object(Some(copy)));
        ctx.set_field(this, 1, Value::Object(Some(algo)));
        Ok(None)
    });
    // `return this.key.clone()`, for the same reason and with the same
    // measurement behind it: handing the stored array back let a caller zero
    // the key through the accessor. `Cipher.init` reads the key through
    // `extract_key_bytes`, which takes field 0 directly and is unaffected by
    // the extra copy.
    r.register(sks, "getEncoded", "()[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Object(clone_byte_field(ctx, this, 0))))
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
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

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

    fn kw_hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    fn kw_hexstr(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// The three AES key-wrap transformations, against SunJCE's own answers.
    ///
    /// Measured on OpenJDK 25.0.4 with KEK `000102030405060708090a0b0c0d0e0f`
    /// via `Cipher.getInstance(t).doFinal(data)` — the entry point that used to
    /// raise `IllegalStateException: Cipher mode 'KW' not implemented`.
    #[test]
    fn aes_key_wrap_matches_sunjce() {
        let kek = kw_hex("000102030405060708090a0b0c0d0e0f");
        let d16 = kw_hex("00112233445566778899aabbccddeeff");
        let d20 = kw_hex("00112233445566778899aabbccddeeff00112233");

        // RFC 3394 proper — this one already worked through `Cipher.wrap`.
        assert_eq!(
            kw_hexstr(&aes_key_wrap(&kek, &d16).unwrap()),
            "1fa68b0a8112b447aef34bd8fb5a7b829d3e862371d2cfe5"
        );
        // PKCS#5 at an EIGHT-byte block size, then RFC 3394.
        assert_eq!(
            kw_hexstr(&aes_key_wrap(&kek, &pkcs5_pad8(&d16)).unwrap()),
            "b05471fa00ab70570ea62b3cfc244f1001af95366e5fe1f430ed8ac55b16c5da"
        );
        assert_eq!(
            kw_hexstr(&aes_key_wrap(&kek, &pkcs5_pad8(&d20)).unwrap()),
            "a694a9bc72fdcf00782dd32f2ed1e75859b7b87730d71efbba4e6a5e5f9bb8bd"
        );
        // RFC 5649.
        assert_eq!(
            kw_hexstr(&aes_key_wrap_with_padding(&kek, &d16).unwrap()),
            "2cef0c9e30de26016c230cb78bc60d51b1fe083ba0c79cd5"
        );
        assert_eq!(
            kw_hexstr(&aes_key_wrap_with_padding(&kek, &d20).unwrap()),
            "23cf017f0dc30969899318b8b400c0eca73290dba36289217fbb33d964653ae9"
        );
    }

    /// Every wrap round-trips at its ORIGINAL length — the property RFC 5649's
    /// length field exists for, and the one a zero-padded unwrap would break.
    #[test]
    fn aes_key_wrap_round_trips_at_the_original_length() {
        let kek = kw_hex("000102030405060708090a0b0c0d0e0f");
        // KWP accepts any length from 1 up; KW/PKCS5 needs 8 (the pad always
        // adds a block, and RFC 3394 needs two). Both measured on SunJCE.
        for len in [1usize, 7, 8, 9, 15, 16, 20, 24, 31, 32, 64] {
            let data: Vec<u8> = (0..len).map(|i| (i * 13 + 1) as u8).collect();
            let wrapped = aes_key_wrap_with_padding(&kek, &data).unwrap();
            assert_eq!(
                aes_key_unwrap_with_padding(&kek, &wrapped).unwrap(),
                data,
                "KWP len={len}"
            );
            if len >= 8 {
                let p5 = aes_key_wrap(&kek, &pkcs5_pad8(&data)).unwrap();
                assert_eq!(
                    pkcs5_unpad8(&aes_key_unwrap(&kek, &p5).unwrap()).unwrap(),
                    data,
                    "KW/PKCS5 len={len}"
                );
            }
        }
        // The wrapped SIZES are SunJCE's too, and they are what a caller sizing
        // a buffer depends on.
        assert_eq!(
            aes_key_wrap_with_padding(&kek, &[0u8; 1]).unwrap().len(),
            16
        );
        assert_eq!(
            aes_key_wrap_with_padding(&kek, &[0u8; 8]).unwrap().len(),
            16
        );
        assert_eq!(
            aes_key_wrap_with_padding(&kek, &[0u8; 9]).unwrap().len(),
            24
        );
        assert_eq!(
            aes_key_wrap_with_padding(&kek, &[0u8; 16]).unwrap().len(),
            24
        );
        assert_eq!(
            aes_key_wrap_with_padding(&kek, &[0u8; 17]).unwrap().len(),
            32
        );
        assert!(aes_key_wrap_with_padding(&kek, &[]).is_err());
    }

    /// A tampered wrap is an integrity failure, never a plaintext.
    #[test]
    fn aes_kwp_rejects_tampering() {
        let kek = kw_hex("000102030405060708090a0b0c0d0e0f");
        let data = kw_hex("00112233445566778899aabbccddeeff00112233");
        let wrapped = aes_key_wrap_with_padding(&kek, &data).unwrap();
        for i in 0..wrapped.len() {
            let mut bad = wrapped.clone();
            bad[i] ^= 1;
            assert!(
                aes_key_unwrap_with_padding(&kek, &bad).is_err(),
                "byte {i} flipped and the unwrap still succeeded"
            );
        }
        // A wrong KEK likewise.
        let mut other = kek.clone();
        other[0] ^= 1;
        assert!(aes_key_unwrap_with_padding(&other, &wrapped).is_err());
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

    /// `AES/ECB/NoPadding` must parse as "no padding". The ECB arm of
    /// `cipher_do_final_impl` used to bind this flag to `_pad` and pad anyway,
    /// which made its ciphertext 16 bytes longer than HotSpot's and made its
    /// decrypt truncate real plaintext whenever the last byte fell in 1..=16.
    #[test]
    fn parse_transformation_aes_ecb_nopadding_reports_no_padding() {
        let (cipher, mode, pad) = parse_transformation("AES/ECB/NoPadding");
        assert_eq!(cipher, "AES");
        assert_eq!(mode, "ECB");
        assert!(!pad, "NoPadding must not be reported as padded");
        // A bare `AES` / `AES/ECB` defaults to ECB + PKCS5Padding, as SunJCE does.
        assert!(parse_transformation("AES").2);
        assert_eq!(parse_transformation("AES").1, "ECB");
    }

    /// The PKCS7 verifier the ECB decrypt arm now delegates to must REFUSE
    /// exactly the shapes the deleted inline strip accepted:
    ///   * a last byte outside 1..=16 (the old code silently stripped nothing
    ///     and returned the padding as plaintext);
    ///   * a plausible pad length whose padding BYTES do not all match (the old
    ///     code never looked at them, so a tampered / wrong-key block decrypted
    ///     to a plausible plaintext instead of `BadPaddingException`).
    #[test]
    fn ecb_padding_verifier_rejects_what_the_old_inline_strip_accepted() {
        use crate::phases_early::{pkcs7_pad, pkcs7_unpad};

        // Round-trip still works for every pad length.
        for n in 0..=32usize {
            let msg = vec![0xABu8; n];
            let padded = pkcs7_pad(&msg);
            assert_eq!(padded.len() % 16, 0);
            assert_eq!(pkcs7_unpad(&padded).expect("valid padding"), msg);
        }

        // Last byte 0x00 — old code: `pad >= 1` false, no strip, padding bytes
        // handed back as plaintext.
        let mut zero_tail = vec![0u8; 16];
        zero_tail[15] = 0x00;
        assert!(pkcs7_unpad(&zero_tail).is_err());

        // Last byte 0x11 (17) — out of range, same silent no-strip.
        let mut over = vec![0u8; 16];
        over[15] = 0x11;
        assert!(pkcs7_unpad(&over).is_err());

        // Plausible length, wrong content: claims 4 bytes of padding but only
        // the last one is 0x04. Old code truncated 4 bytes and reported success.
        let mut wrong = vec![0u8; 16];
        wrong[15] = 0x04;
        assert!(pkcs7_unpad(&wrong).is_err());

        // A non-block-multiple buffer is structurally invalid.
        assert!(pkcs7_unpad(&[0u8; 15]).is_err());
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
        // This predicate delegates to `classify_transformation` and so means
        // "is in the key-wrap FAMILY", which KWP now joins. The distinction
        // between the three schemes lives in `aes_wrap_flavour`, and it is the
        // one the dispatch actually needs: RFC 3394, RFC 3394 over PKCS#5-
        // padded input, and RFC 5649 are three different computations.
        assert!(is_aes_key_wrap_transformation("AES/KWP/NoPadding"));
        assert!(matches!(
            aes_wrap_flavour("AES/KWP/NoPadding"),
            Some(AesWrapFlavour::Kwp)
        ));
        assert!(matches!(
            aes_wrap_flavour("AES/KW/PKCS5Padding"),
            Some(AesWrapFlavour::KwPkcs5)
        ));
        assert!(matches!(
            aes_wrap_flavour("AES/KW/NoPadding"),
            Some(AesWrapFlavour::Kw)
        ));
        assert!(matches!(
            aes_wrap_flavour("AESWrap"),
            Some(AesWrapFlavour::Kw)
        ));
        assert!(aes_wrap_flavour("AES/GCM/NoPadding").is_none());
        assert_eq!(aes_wrap_expected_kek_len("AESWrap_128"), Some(16));
        assert_eq!(aes_wrap_expected_kek_len("AESWrap_192"), Some(24));
        assert_eq!(aes_wrap_expected_kek_len("AESWrap_256"), Some(32));
    }

    // -----------------------------------------------------------------------
    // W7-15 — the admission table.
    //
    // Every "must raise" below has a measured RED behind it: the transformation
    // named produced AES ciphertext on this tree's release binary before the
    // table landed. Every "must still work" is a transformation the same binary
    // computed correctly and byte-identically to HotSpot 25, so the fix is
    // pinned on both sides.
    // -----------------------------------------------------------------------

    fn refuses_algorithm(t: &str) -> bool {
        matches!(
            classify_transformation(t),
            TransformVerdict::NoSuchAlgorithm
        )
    }

    fn refuses_padding(t: &str) -> bool {
        matches!(
            classify_transformation(t),
            TransformVerdict::NoSuchPadding(_)
        )
    }

    /// MUST RAISE. The headline defect: both ChaCha20 names produced
    /// AES-256-ECB, byte-identically to `AES/ECB/PKCS5Padding`, with the nonce
    /// discarded and no AEAD tag.
    #[test]
    fn chacha20_is_served_as_chacha20_and_never_as_aes() {
        // This test asserted the opposite for one day, and both verdicts were
        // right in their moment: while nothing computed ChaCha20 the only safe
        // answer was to refuse the name, because admitting it meant AES-256-ECB.
        // `crate::chacha20` (RFC 8439) landed 2026-08-11, so the names are
        // admitted again — and the DISPATCH now keys on the family, which is
        // what makes admitting them safe.
        assert!(!refuses_algorithm("ChaCha20"));
        assert!(!refuses_algorithm("ChaCha20-Poly1305"));
        assert!(!refuses_algorithm("chacha20-poly1305"));
        assert!(!refuses_algorithm("ChaCha20/None/NoPadding"));
        assert!(!refuses_algorithm("ChaCha20-Poly1305/None/NoPadding"));
        // …and they resolve to their OWN families, not to AES. This is the
        // assertion that would have caught the original defect: the name was
        // admitted then and would have passed the four lines above.
        assert!(matches!(
            cipher_family("ChaCha20"),
            Some(CipherFamily::ChaCha20)
        ));
        assert!(matches!(
            cipher_family("ChaCha20-Poly1305"),
            Some(CipherFamily::ChaCha20Poly1305)
        ));
        // A BLOCK-cipher mode on ChaCha20 is still refused: tolerating `ECB`
        // here is exactly how the substitution happened.
        assert!(refuses_algorithm("ChaCha20/ECB/NoPadding"));
        assert!(refuses_algorithm("ChaCha20/CBC/PKCS5Padding"));
        // The one AEAD that was always implemented stays admitted.
        assert!(!refuses_algorithm("AES/GCM/NoPadding"));
    }

    /// MUST RAISE. It was never only ChaCha20 — `cipher_algorithm_known`
    /// accepted a whole catalogue, and every name in it reached the same ECB
    /// arm. Measured: `Blowfish` and `RC4` produced the SAME ciphertext as each
    /// other from a 16-byte key, because both were AES-128-ECB.
    ///
    /// `Blowfish`, `RC4` and `ARCFOUR` left this list on 2026-08-12 (W7-39) —
    /// they are computed now, by the real SunJCE SPI, and
    /// `blowfish_and_rc4_are_their_own_ciphers_and_not_aes` is where they went.
    /// The rest stay, and the list keeps its name: what it pins is that an
    /// algorithm nothing computes is REFUSED, not that these particular thirteen
    /// names are forever unimplementable.
    #[test]
    fn the_other_names_that_were_silently_aes_are_refused_too() {
        for t in [
            "RC2", "IDEA", "SEED", "SM4", "Camellia", "Twofish", "Serpent", "CAST5", "Salsa20",
            "Skipjack", "ECIES", "ElGamal", "NULL",
        ] {
            assert!(
                refuses_algorithm(t),
                "{t} must be refused, not served as AES"
            );
        }
        assert!(refuses_algorithm("CRATONVM-NO-SUCH-CIPHER"));
    }

    /// The twin of the test above, for the two names that moved off it. This is
    /// the assertion that would have caught the ORIGINAL defect: both were
    /// admitted then, so "does `getInstance` accept it" proves nothing on its
    /// own — what matters is that each resolves to its OWN family, so
    /// `cipher_do_final_impl`'s family-keyed dispatch reaches
    /// `BlowfishCipher`/`ARCFOURCipher` and never `Aes::key_expansion`.
    ///
    /// Measured on jdk-25.0.3.9-hotspot with a 16-byte key `30..3f` over the
    /// 16-byte plaintext `"sixteen byte msg"`, which is the same input
    /// `probes/CryptoTrioProbe.java` uses:
    ///
    /// ```text
    /// AES/ECB/NoPadding  178c380cadc0514ffe26d8b26351c673
    /// Blowfish           33b63e40d662746425f71a69f8cffcdadadae7ffa8950336   (24B, PKCS5)
    /// RC4                27ca482b161e3ab93f812659b904df95                   (16B, stream)
    /// ```
    ///
    /// Three different algorithms, three different answers. Before this lane all
    /// three of those lines read `178c380c…`.
    #[test]
    fn blowfish_and_rc4_are_their_own_ciphers_and_not_aes() {
        for t in [
            "Blowfish",
            "BLOWFISH",
            "Blowfish/ECB/PKCS5Padding",
            "Blowfish/ECB/NoPadding",
        ] {
            assert!(transformation_is_serviceable(t), "{t} must resolve");
            let (name, _, _) = parse_transformation(t);
            assert!(
                matches!(cipher_family(&name), Some(CipherFamily::Blowfish)),
                "{t} must resolve to the Blowfish family, not to AES"
            );
        }
        for t in [
            "RC4",
            "ARCFOUR",
            "rc4",
            "RC4/ECB/NoPadding",
            "ARCFOUR/ECB/NoPadding",
        ] {
            assert!(transformation_is_serviceable(t), "{t} must resolve");
            let (name, _, _) = parse_transformation(t);
            assert!(
                matches!(cipher_family(&name), Some(CipherFamily::Arcfour)),
                "{t} must resolve to the ARCFOUR family, not to AES"
            );
        }
        // The SPI route each family takes, asserted rather than assumed: a
        // Blowfish transformation driven by `ARCFOURCipher` (or either driven
        // with the other's padding) is a substitution, and the only place that
        // pairing is written down is `real_spi_ecb_route`.
        assert_eq!(
            real_spi_ecb_route(CipherFamily::Blowfish, true),
            Some((
                "com/sun/crypto/provider/BlowfishCipher",
                "PKCS5Padding",
                "Blowfish"
            ))
        );
        assert_eq!(
            real_spi_ecb_route(CipherFamily::Blowfish, false),
            Some((
                "com/sun/crypto/provider/BlowfishCipher",
                "NoPadding",
                "Blowfish"
            ))
        );
        // A stream cipher's padding does not depend on the transformation,
        // because `classify_transformation` admits only `NoPadding` for it.
        for padded in [true, false] {
            assert_eq!(
                real_spi_ecb_route(CipherFamily::Arcfour, padded),
                Some(("com/sun/crypto/provider/ARCFOURCipher", "NoPadding", "RC4"))
            );
        }
        // No other family may reach this driver: every one of them either has
        // its own in-crate path or routes through `drive_real_cipher` with an
        // IV, and both of those SPIs refuse the no-parameters `engineInit` this
        // one calls.
        for f in [
            CipherFamily::Aes,
            CipherFamily::AesFixed(16),
            CipherFamily::AesKeyWrap,
            CipherFamily::DesFamily,
            CipherFamily::Rsa,
            CipherFamily::Pbes2,
            CipherFamily::ChaCha20,
            CipherFamily::ChaCha20Poly1305,
        ] {
            assert!(
                real_spi_ecb_route(f, true).is_none(),
                "{f:?} must not route to the no-parameters SPI driver"
            );
        }
    }

    /// MUST RAISE. The modes and paddings HotSpot serves for these two families
    /// and this engine does not — under-service, refused honestly.
    ///
    /// `Blowfish/CBC/PKCS5Padding` and `Blowfish/CTR/NoPadding` both resolve and
    /// encrypt on HotSpot 25 (measured, including the RANDOM IV HotSpot
    /// generates and reports through `getIV()`: `0417208096327740` on one run).
    /// Admitting them here without implementing the generate-and-report-an-IV
    /// contract would encrypt under an all-zero IV and call it success — which
    /// is the fabricated-success shape W7-38 measured, not a smaller version of
    /// it. `Blowfish/ECB/ISO10126Padding` resolves on HotSpot too and its
    /// padding bytes are RANDOM; serving PKCS5 in its place is a substitution.
    #[test]
    fn blowfish_and_rc4_modes_this_engine_does_not_compute_are_refused() {
        for t in [
            "Blowfish/CBC/PKCS5Padding",
            "Blowfish/CBC/NoPadding",
            "Blowfish/CTR/NoPadding",
            "Blowfish/CFB/NoPadding",
            "Blowfish/OFB/NoPadding",
            "Blowfish/PCBC/PKCS5Padding",
            "RC4/CBC/NoPadding",
        ] {
            assert!(refuses_algorithm(t), "{t} must be refused at getInstance");
        }
        assert!(refuses_padding("Blowfish/ECB/ISO10126Padding"));
        // …and the shapes HotSpot ALSO refuses, so these rows are parity rather
        // than under-service. Measured: `NoSuchAlgorithmException: Cannot find
        // any provider supporting <name>` for each.
        for t in [
            "Blowfish/None/NoPadding",
            "RC4/None/NoPadding",
            "RC4/NONE/NoPadding",
            // A padding on a stream cipher is a missing SERVICE on SunJCE, not a
            // missing padding — the service carries no `SupportedPaddings`
            // beyond NoPadding, so the lookup finds nothing at all and the
            // exception is `NoSuchAlgorithmException`. The two are separately
            // catchable, so which one is thrown is part of the parity.
            "RC4/ECB/PKCS5Padding",
            "ARCFOUR/ECB/PKCS5Padding",
        ] {
            assert!(
                refuses_algorithm(t),
                "{t} must be refused, as HotSpot refuses it"
            );
        }
        // `Blowfish/ECB/PKCS7Padding` is refused by HotSpot as well, and as a
        // PADDING failure here — the algorithm and mode do resolve.
        assert!(refuses_padding("Blowfish/ECB/PKCS7Padding"));
    }

    /// The key lengths each family accepts, in HotSpot's own wording, checked at
    /// `init` where `Cipher.init` declares `InvalidKeyException` — not at
    /// `doFinal`, which does not declare it. Every bound measured on
    /// jdk-25.0.3.9-hotspot by calling `Cipher.init` with that many bytes.
    ///
    /// The asymmetry is real and is why this is a table and not a rule: a
    /// 3-byte Blowfish key is ACCEPTED (SunJCE has no lower bound on it) while
    /// a 4-byte RC4 key is refused.
    #[test]
    fn blowfish_and_rc4_key_lengths_match_hotspot() {
        for n in [3usize, 4, 8, 16, 32, 56] {
            assert!(
                key_length_reason("Blowfish", n).is_none(),
                "HotSpot accepts a {n}-byte Blowfish key"
            );
        }
        assert_eq!(
            key_length_reason("Blowfish", 57).as_deref(),
            Some("Key too long (> 448 bits)")
        );
        for n in [5usize, 16, 128] {
            assert!(
                key_length_reason("RC4", n).is_none(),
                "HotSpot accepts a {n}-byte RC4 key"
            );
        }
        for n in [4usize, 129] {
            assert_eq!(
                key_length_reason("ARCFOUR", n).as_deref(),
                Some("Key length must be between 40 and 1024 bit"),
                "HotSpot refuses a {n}-byte RC4 key at init"
            );
        }
    }

    /// `getBlockSize()` and `getOutputSize()` must derive the block from the
    /// SAME place, which is what `cipher_block_size` is for. They did not:
    /// `getOutputSize` hardcoded 16 while `getBlockSize` carried the real table,
    /// so the two disagreed for every 64-bit-block cipher and for every stream
    /// cipher. Measured on HotSpot: `Blowfish` reports `getBlockSize()=8` and
    /// `RC4` reports 0.
    #[test]
    fn block_size_is_the_ciphers_own_and_stream_ciphers_have_none() {
        assert_eq!(cipher_block_size("Blowfish"), 8);
        assert_eq!(cipher_block_size("Blowfish/ECB/PKCS5Padding"), 8);
        assert_eq!(cipher_block_size("DESede/CBC/PKCS5Padding"), 8);
        assert_eq!(cipher_block_size("RC4"), 0);
        assert_eq!(cipher_block_size("ARCFOUR/ECB/NoPadding"), 0);
        assert_eq!(cipher_block_size("ChaCha20"), 0);
        assert_eq!(cipher_block_size("RSA/ECB/PKCS1Padding"), 0);
        assert_eq!(cipher_block_size("AES/GCM/NoPadding"), 16);
        // An un-inited Cipher has no transformation at all; 0 rather than a
        // default 16, so a caller cannot align on a block this cipher may not
        // have.
        assert_eq!(cipher_block_size(""), 0);
    }

    /// MUST RAISE. Modes with no implementation here. Each used to be admitted
    /// by `getInstance` and then die at `doFinal` on an UNCHECKED
    /// `IllegalStateException` naming "WP6.3 dispatch" — the wrong exception,
    /// at the wrong call, uncatchable by `catch (GeneralSecurityException)`.
    #[test]
    fn unimplemented_modes_are_refused_at_getinstance() {
        for t in [
            "AES/CTR/NoPadding",
            "AES/CTS/NoPadding",
            "AES/PCBC/PKCS5Padding",
            "AES/CFB8/NoPadding",
            "AES/CCM/NoPadding",
        ] {
            assert!(refuses_algorithm(t), "{t} must be refused at getInstance");
        }
        // `DESede` and `DESede/ECB/PKCS5Padding` were on this list until
        // 2026-08-27 and are now COMPUTED, by the same rule that put
        // `AES/KWP/NoPadding` on the other side: the admission table and
        // `cipher_do_final_impl`'s route were widened together, so ECB is
        // served by ECB rather than by CBC under an ECB name. Asserted here
        // rather than only below, so whoever reads this list sees the move.
        for t in [
            "DESede",
            "DESede/ECB/PKCS5Padding",
            "DESede/ECB/NoPadding",
            "DESede/CBC/PKCS5Padding",
        ] {
            assert!(
                transformation_is_serviceable(t),
                "{t} must be served at getInstance"
            );
        }
        // Still refused, and these are the forms that were NOT measured or
        // that name a mode with no route: one token spelled and not the other,
        // and any mode outside {CBC, ECB}.
        for t in [
            "DESede/CFB/PKCS5Padding",
            "DESede/OFB/PKCS5Padding",
            "DESede/CTR/NoPadding",
        ] {
            assert!(refuses_algorithm(t), "{t} must be refused at getInstance");
        }
        // `AES/KWP/NoPadding` was on this list until 2026-08-11 and is now
        // computed (RFC 5649, `aes_key_wrap_with_padding`), so it belongs on
        // the "must still work" side. Asserted here rather than only there, so
        // that whoever deletes an arm from `classify_transformation` sees the
        // move rather than a silently shorter list.
        assert!(transformation_is_serviceable("AES/KWP/NoPadding"));
        // The size-pinned `AES_128/KWP/...` spelling is deliberately NOT
        // asserted either way: SunJCE's behaviour for it was not measured for
        // this change, and asserting an unmeasured verdict is how a wrong
        // expectation becomes a "requirement".
    }

    /// MUST RAISE, as a PADDING failure specifically — the JDK distinguishes
    /// the two exceptions and a caller may well catch only one. Measured on
    /// HotSpot: `AES/CBC/PKCS7Padding` is refused outright, and
    /// `AES/CBC/ISO10126Padding` is served with RANDOM padding bytes. This
    /// engine implements PKCS#7-as-PKCS5Padding and nothing else, and served
    /// BOTH of those as PKCS5.
    #[test]
    fn unimplemented_paddings_are_refused_not_aliased_onto_pkcs5() {
        assert!(refuses_padding("AES/CBC/PKCS7Padding"));
        assert!(refuses_padding("AES/CBC/ISO10126Padding"));
        assert!(refuses_padding("AES/ECB/CRATONVM-NO-SUCH-PADDING"));
        // AEAD takes NoPadding only.
        assert!(refuses_padding("AES/GCM/PKCS5Padding"));
        // RFC 3394 key wrap takes BOTH, since 2026-08-11: SunJCE's
        // `AES/KW/PKCS5Padding` pads to a multiple of eight and then wraps
        // (measured — a 16-byte payload comes back as 32 bytes, not 24).
        assert!(transformation_is_serviceable("AES/KW/PKCS5Padding"));
        // …and still refuses a padding neither scheme has.
        assert!(refuses_padding("AES/KW/ISO10126Padding"));
        // RSA admits exactly what `RsaCipherPadding::from_transformation` does.
        assert!(refuses_padding("RSA/ECB/NoPadding"));
        assert!(refuses_padding("RSA/ECB/OAEPWithSHA-512AndMGF1Padding"));
        assert!(refuses_algorithm("RSA/None/PKCS1Padding"));
    }

    /// MUST STILL WORK — the twin every refusal needs. These are the
    /// transformations the measured binary computed correctly, several of them
    /// byte-identically to HotSpot 25; none may become collateral damage.
    #[test]
    fn every_transformation_that_worked_before_still_resolves() {
        for t in [
            "AES/GCM/NoPadding",
            "AES",
            "AES/ECB/PKCS5Padding",
            "AES/ECB/NoPadding",
            "AES/CBC/PKCS5Padding",
            "AES/CBC/NoPadding",
            "AES/CFB/PKCS5Padding",
            "AES/OFB/PKCS5Padding",
            "AES/KW/NoPadding",
            "AESWrap",
            "AESWrap_128",
            "AESWrap_192",
            "AESWrap_256",
            "AES_128/GCM/NoPadding",
            "AES_256/CBC/NoPadding",
            "DES/CBC/PKCS5Padding",
            "DESede/CBC/PKCS5Padding",
            "TripleDES/CBC/PKCS5Padding",
            "RSA",
            "RSA/ECB/PKCS1Padding",
            "RSA/ECB/OAEPWithSHA-256AndMGF1Padding",
            "RSA/ECB/OAEPPadding",
            "PBEWithHmacSHA1AndAES_128",
            "PBEWithHmacSHA256AndAES_256",
            // JCA lookup is case-insensitive and the JDK trims each token —
            // measured, `aes / gcm / nopadding` resolves on HotSpot.
            "aes/gcm/nopadding",
            "AES / GCM / NoPadding",
        ] {
            assert!(
                transformation_is_serviceable(t),
                "{t} worked before this lane and must still resolve"
            );
        }
    }

    /// The tokenizer is the JDK's, messages included — these four shapes were
    /// all accepted before, with `parse_transformation` inventing whatever it
    /// needed. `"AES/CBC"` became a padded CBC cipher; the JDK calls it invalid.
    #[test]
    fn malformed_transformations_carry_the_jdk_messages() {
        let msg = |t: &str| match classify_transformation(t) {
            TransformVerdict::InvalidFormat(m) => m,
            _ => panic!("{t} must be an invalid format"),
        };
        assert_eq!(msg("AES/CBC"), "Invalid transformation format:AES/CBC");
        assert_eq!(
            msg("AES/CBC/"),
            "Invalid transformation: missing mode and/or padding-AES/CBC/"
        );
        assert_eq!(
            msg("/CBC/NoPadding"),
            "Invalid transformation: algorithm not specified-/CBC/NoPadding"
        );
        // `getInstance` checks empty BEFORE tokenizing and says so; measured.
        assert_eq!(msg(""), "Null or empty transformation");
        // The JDK's own guard for an algorithm with a `/` inside its NAME: the
        // first slash of `SHA512/224` must not be read as a mode separator.
        assert_eq!(
            tokenize_transformation("PBEWithHmacSHA512/224AndAES_128").unwrap(),
            ("PBEWithHmacSHA512/224AndAES_128".to_string(), None, None)
        );
    }

    /// `AES_128` means "AES with a 128-bit key". Before this lane the suffix
    /// was decorative: measured, `AES_128/GCM/NoPadding` initialised from a
    /// 256-bit key and encrypted with AES-256.
    #[test]
    fn size_pinned_names_pin_the_key_length() {
        assert_eq!(
            transformation_pinned_key_len("AES_128/GCM/NoPadding"),
            Some(16)
        );
        assert_eq!(
            transformation_pinned_key_len("AES_192/CBC/NoPadding"),
            Some(24)
        );
        assert_eq!(
            transformation_pinned_key_len("AES_256/ECB/NoPadding"),
            Some(32)
        );
        assert_eq!(transformation_pinned_key_len("AES/GCM/NoPadding"), None);
        // HotSpot's measured wording, both flavours.
        assert_eq!(
            key_length_reason("AES_128/GCM/NoPadding", 32).as_deref(),
            Some("The key must be 16 bytes")
        );
        assert_eq!(
            key_length_reason("AES/GCM/NoPadding", 17).as_deref(),
            Some("Invalid AES key length: 17 bytes")
        );
        // MUST STILL WORK: the legal sizes, and the families this rule does
        // not govern.
        assert!(key_length_reason("AES_128/GCM/NoPadding", 16).is_none());
        for n in [16, 24, 32] {
            assert!(key_length_reason("AES/CBC/PKCS5Padding", n).is_none());
        }
        assert!(key_length_reason("RSA/ECB/PKCS1Padding", 294).is_none());
        assert!(key_length_reason("PBEWithHmacSHA1AndAES_128", 9).is_none());
        assert!(key_length_reason("DESede/CBC/PKCS5Padding", 24).is_none());
        // An unreadable key is a different diagnosis and must not be reported
        // as a length complaint.
        assert!(key_length_reason("AES/GCM/NoPadding", 0).is_none());
    }

    /// A bare `AES` really is `AES/ECB/PKCS5Padding` on SunJCE (measured:
    /// HotSpot's ciphertext for the two is byte-identical), so that default is
    /// kept — but it is now a property of the AES arm rather than of every
    /// algorithm name that reaches `parse_transformation`.
    #[test]
    fn the_ecb_default_is_scoped_to_the_family_that_has_one() {
        assert!(transformation_is_serviceable("AES"));
        // ChaCha20 is serviceable again (RFC 8439 landed 2026-08-11), but the
        // ECB DEFAULT must still not reach it — that default is what turned the
        // name into AES-256-ECB, and it belongs to the AES family alone.
        assert!(transformation_is_serviceable("ChaCha20"));
        assert!(refuses_algorithm("ChaCha20/ECB/NoPadding"));
        // `AES_128` alone is not a service on SunJCE either — measured,
        // `Cipher.getInstance("AES_128")` raises while `AES_128/CBC/NoPadding`
        // resolves — so the default must not manufacture one.
        assert!(refuses_algorithm("AES_128"));
        assert!(transformation_is_serviceable("AES_128/CBC/NoPadding"));
    }

    #[test]
    fn parse_transformation_bare_aes() {
        let (cipher, mode, pad) = parse_transformation("AES");
        assert_eq!(cipher, "AES");
        assert_eq!(mode, "ECB");
        assert!(pad);
    }

    // -----------------------------------------------------------------------
    // P0 — `Cipher.init` must reject a key it cannot use.
    //
    // The "must raise" case has a "must still work" twin with a real RSA key,
    // and a third case proving the guard is scoped to RSA transformations
    // only (an AES init with no RSA components is legitimate and unaffected).
    // -----------------------------------------------------------------------

    /// Set up a synthetic `Cipher` whose recorded transformation is `algo`.
    fn cipher_for(
        ctx: &mut crate::test_utils::MockNativeContext,
        algo: &str,
    ) -> cratonvm_types::ObjectRef {
        let cid = ctx.ensure_class_initialized("javax/crypto/Cipher").unwrap();
        let obj = ctx.alloc_object(cid, 8);
        let tkey = obj_key(ctx, obj);
        with_table_write(|t| {
            t.entry(tkey).or_default().algorithm = algo.to_string();
        });
        obj
    }

    /// A synthetic `Key` carrying `key_id` in slot 3 (0 = "no handle").
    fn key_with_handle(
        ctx: &mut crate::test_utils::MockNativeContext,
        key_id: u64,
    ) -> cratonvm_types::ObjectRef {
        let cid = ctx.ensure_class_initialized("java/security/Key").unwrap();
        let key = ctx.alloc_object(cid, 8);
        ctx.set_field(key, 3, Value::Long(key_id as i64));
        key
    }

    // MUST RAISE.
    #[test]
    fn rsa_cipher_init_with_an_unusable_key_raises_invalid_key() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let cipher_obj = cipher_for(&mut ctx, "RSA/ECB/PKCS1Padding");
        // No `getModulus()` behaviour in the mock and no crypto_impl handle:
        // `rsa_key_components` returns `None`.
        let key = key_with_handle(&mut ctx, 0);
        let err = cipher_init_record(&mut ctx, cipher_obj, 1, key, Vec::new())
            .expect_err("init must reject a key with no usable RSA components");
        match err {
            MethodCallFailed::ExceptionThrown(exc) => {
                let cid = ctx.class_id_of_object(exc);
                assert_eq!(
                    ctx.class_name_arc_of_id(cid).as_deref(),
                    Some("java/security/InvalidKeyException"),
                    "Cipher.init declares InvalidKeyException for exactly this"
                );
            }
            // `throw_jca_exc`'s fallback — still loud, still not a success.
            MethodCallFailed::InternalError(e) => {
                let text = format!("{e}");
                assert!(
                    text.contains("IllegalArgumentException"),
                    "unexpected fallback: {text}"
                );
            }
        }
        // And the cipher must NOT have been left holding an empty key.
        let tkey = obj_key(&mut ctx, cipher_obj);
        let recorded = with_table_read(|t| t.get(&tkey).map(|s| s.mode).unwrap_or(0));
        assert_eq!(recorded, 0, "a refused init must not record a mode");
    }

    // MUST STILL WORK — the twin.
    #[test]
    fn rsa_cipher_init_with_a_real_key_succeeds() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let (public_key, private_key) = crate::crypto_impl::Rsa::generate_keypair(1024);
        let id = crate::crypto_impl::rsa_key_next_id();
        crate::crypto_impl::rsa_key_store(
            id,
            crate::crypto_impl::RsaKeyPairData {
                public_key,
                private_key,
            },
        );
        let cipher_obj = cipher_for(&mut ctx, "RSA/ECB/PKCS1Padding");
        let key = key_with_handle(&mut ctx, id);
        cipher_init_record(&mut ctx, cipher_obj, 1, key, Vec::new())
            .expect("a registered RSA key must initialise the cipher");
        let tkey = obj_key(&mut ctx, cipher_obj);
        let (mode, n_len, e_len) = with_table_read(|t| {
            let s = t.get(&tkey).expect("state recorded");
            (s.mode, s.rsa_n.len(), s.rsa_exp.len())
        });
        assert_eq!(mode, 1);
        assert!(
            n_len > 0 && e_len > 0,
            "the real key components are recorded"
        );
    }

    // MUST STILL WORK — the guard is RSA-scoped; a symmetric init has no RSA
    // components by design and must not be refused.
    #[test]
    fn non_rsa_cipher_init_is_unaffected_by_the_rsa_key_guard() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let cipher_obj = cipher_for(&mut ctx, "AES/GCM/NoPadding");
        let key = key_with_handle(&mut ctx, 0);
        cipher_init_record(&mut ctx, cipher_obj, 1, key, vec![0u8; 12])
            .expect("an AES init must not be caught by the RSA key guard");
        let tkey = obj_key(&mut ctx, cipher_obj);
        assert_eq!(with_table_read(|t| t.get(&tkey).map(|s| s.mode)), Some(1));
    }
}
