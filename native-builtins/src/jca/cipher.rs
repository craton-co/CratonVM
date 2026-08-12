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

use cratonvm_types::error::MethodCallFailed;
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
use crate::{try_alloc_concurrent_synthetic, obj_arg};

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
    /// PBES2 (`PBEWithHmacSHA*AndAES_*`) salt, captured at `init` time from the
    /// `AlgorithmParameters` argument. Empty for non-PBES2 ciphers. Plain bytes
    /// (not an `ObjectRef`) for the same GC-safety reason as `key_bytes` — and
    /// so `Cipher.getParameters()` can rebuild a fresh `AlgorithmParameters`
    /// on demand without holding a heap reference across calls.
    pbe_salt: Vec<u8>,
    /// PBES2 iteration count, paired with `pbe_salt`. Zero for non-PBES2.
    pbe_iterations: u32,
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
fn clone_byte_field(ctx: &mut dyn NativeContext, this: ObjectRef, slot: usize) -> Option<ObjectRef> {
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
    let id = crate::crypto_impl::rsa_realkey_map_get(ctx.vm_identity(), ctx.identity_hash_code(key))
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
    if let Some(reason) = aes_key_length_reason(&algo, key_bytes.len()) {
        return Err(crate::phases_early::throw_jca_exc(
            ctx,
            "java/security/InvalidKeyException",
            &reason,
        ));
    }

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
            GetInstanceForm::Anonymous => Err(
                crate::jca::provider_chain::throw_no_such_algorithm_public(
                    ctx,
                    &format!("Cannot find any provider supporting {algo}"),
                ),
            ),
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
            GetInstanceForm::Anonymous => Err(
                crate::jca::provider_chain::throw_no_such_algorithm_public(
                    ctx,
                    &format!("Cannot find any provider supporting {algo}"),
                ),
            ),
            GetInstanceForm::WithProvider => Err(
                crate::jca::provider_chain::throw_no_such_algorithm_public(
                    ctx,
                    &format!("No such algorithm: {algo}"),
                ),
            ),
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
                "KW" => aes_padding_verdict(
                    p,
                    &named_padding,
                    &["NOPADDING"],
                    CipherFamily::AesKeyWrap,
                ),
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
        // CBC only, and the mode must be SPELLED. `cipher_do_final_impl`'s
        // route table hardcodes `"CBC"` for this family whatever the caller
        // wrote, so admitting `DESede/ECB/PKCS5Padding` would serve CBC under
        // an ECB name — the mode-substitution twin of the algorithm
        // substitution this lane is fixing. The bare `DESede` form defaults to
        // ECB on SunJCE and is refused here for exactly that reason.
        CipherFamily::DesFamily => {
            let (Some(m), Some(p)) = (mode_u.as_deref(), pad_u.as_deref()) else {
                return TransformVerdict::NoSuchAlgorithm;
            };
            if m != "CBC" {
                return TransformVerdict::NoSuchAlgorithm;
            }
            if p == "NOPADDING" || p == "PKCS5PADDING" {
                TransformVerdict::Serviceable(family)
            } else {
                TransformVerdict::NoSuchPadding(named_padding)
            }
        }
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
/// Two rules, both measured on jdk-25.0.3.9-hotspot:
///
/// * a size-suffixed name pins the length exactly —
///   `AES_128/GCM/NoPadding` with a 32-byte key gives
///   `InvalidKeyException: The key must be 16 bytes`;
/// * plain AES takes 16, 24 or 32 — a 17-byte key gives
///   `InvalidKeyException: Invalid AES key length: 17 bytes`.
///
/// Families whose key length this engine does not constrain (RSA components,
/// the PBES2 password, DES/DESede parity keys handed to the real SunJCE SPI,
/// which does its own checking) return `None` and are left alone.
///
/// An EMPTY key is deliberately not reported here. It means `extract_key_bytes`
/// could not read the key at all, which is a different defect with its own
/// downstream handling; folding it in would convert that diagnosis into a
/// length complaint.
fn aes_key_length_reason(algo: &str, key_len: usize) -> Option<String> {
    if key_len == 0 {
        return None;
    }
    if let Some(pinned) = transformation_pinned_key_len(algo) {
        return (key_len != pinned).then(|| format!("The key must be {pinned} bytes"));
    }
    let family = cipher_family(&tokenize_transformation(algo).ok()?.0)?;
    if !matches!(family, CipherFamily::Aes | CipherFamily::AesKeyWrap) {
        return None;
    }
    (!matches!(key_len, 16 | 24 | 32))
        .then(|| format!("Invalid AES key length: {key_len} bytes"))
}

/// Allocate a freshly initialised Cipher synthetic and register an empty
/// `CipherState` for it. The algorithm string is stashed in the side-table — we
/// do **not** write it to any instance field of the real JDK class, because
/// field 5 is `initialized:Z`, a primitive boolean, and storing an Object there
/// breaks the `expected object reference` invariant on read-back.
///
/// Call only after [`check_transformation_supported`] has admitted `algo`: this
/// function allocates unconditionally, so reaching it with an unserviceable
/// name is how a fabricated `Cipher` gets built.
fn cipher_alloc(ctx: &mut dyn NativeContext, algo: ObjectRef) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, "javax/crypto/Cipher", 6)?;
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
    Ok(obj)
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
                // These two hardcode `"CBC"` whatever the caller wrote, which
                // is sound ONLY because `classify_transformation` admits no
                // other mode for this family. Widening the admission table
                // without widening this line would serve CBC under another
                // mode's name — the same substitution, one field over.
                (Some(CipherFamily::DesFamily), _) if cn.eq_ignore_ascii_case("DES") => {
                    Some(("com/sun/crypto/provider/DESCipher", "DES", "CBC"))
                }
                (Some(CipherFamily::DesFamily), _) => {
                    Some(("com/sun/crypto/provider/DESedeCipher", "DESede", "CBC"))
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
        // Not reachable through `Cipher.getInstance`, which now refuses every
        // name outside the table — so reaching it means the admission table and
        // this dispatch have drifted apart, not that a user asked for something
        // odd. Say so, rather than computing AES and calling it success.
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
            let obj = try_alloc_concurrent_synthetic(ctx, "javax/crypto/KeyGenerator", 2)?;
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
            let obj = try_alloc_concurrent_synthetic(ctx, "javax/crypto/KeyGenerator", 2)?;
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
            check_transformation_supported(ctx, &algo_str, GetInstanceForm::Anonymous)?;
            let obj = cipher_alloc(ctx, algo)?;
            Ok(Some(Value::Object(Some(obj))))
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
            check_transformation_supported(ctx, &algo_str, GetInstanceForm::WithProvider)?;
            let obj = cipher_alloc(ctx, algo)?;
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
            crate::jca::provider_chain::check_provider_ownership(
                ctx,
                args,
                1,
                "Cipher",
                &algo_str,
                crate::jca::provider_chain::ProviderArgWording::Cipher,
            )?;
            check_transformation_supported(ctx, &algo_str, GetInstanceForm::WithProvider)?;
            let obj = cipher_alloc(ctx, algo)?;
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

    // getProvider()Ljava/security/Provider; — the real body opens
    // `synchronized (lock)` on a field this synthetic never wrote, so it threw
    // `NullPointerException: Cannot enter synchronized block because
    // "this.lock" is null` on a Cipher that encrypts and decrypts correctly.
    r.register(
        cipher,
        "getProvider",
        "()Ljava/security/Provider;",
        |ctx, args| {
            let _this = obj_arg(args, 0)?;
            let p = crate::jca::make_named_provider(ctx, "SunJCE")?;
            Ok(Some(Value::Object(Some(p))))
        },
    );

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
                        "Cipher.doFinal produced no output buffer ({other:?}); \
                         refusing to report 0 bytes written as success"
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
        let tkey = obj_key(ctx, this);
        let algo = with_table_read(|t| {
            t.get(&tkey)
                .map(|s| s.algorithm.clone())
                .unwrap_or_default()
        });
        let (cipher_name, _mode, _pad) = parse_transformation(&algo);
        let block = match cipher_name.to_ascii_uppercase().as_str() {
            // 64-bit block ciphers.
            "DES" | "DESEDE" | "TRIPLEDES" | "BLOWFISH" | "RC2" | "IDEA" => 8,
            // Stream ciphers and asymmetric transformations have no block.
            "RC4" | "ARCFOUR" | "CHACHA20" | "CHACHA20-POLY1305" | "RSA" | "ECIES" => 0,
            // AES and everything else this module can actually service.
            _ if algo.is_empty() => 0,
            _ => 16,
        };
        Ok(Some(Value::Int(block)))
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
        let key_bytes = obj_arg(args, 1)?;
        let algo = obj_arg(args, 2)?;
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
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess};
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
        assert!(!is_aes_key_wrap_transformation("AES/KWP/NoPadding"));
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
        matches!(classify_transformation(t), TransformVerdict::NoSuchAlgorithm)
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
    fn chacha20_is_refused_rather_than_served_as_aes() {
        assert!(refuses_algorithm("ChaCha20"));
        assert!(refuses_algorithm("ChaCha20-Poly1305"));
        assert!(refuses_algorithm("chacha20-poly1305"));
        assert!(refuses_algorithm("ChaCha20-Poly1305/None/NoPadding"));
        // An AEAD this engine cannot authenticate must be refused, never
        // approximated: a cipher that cannot fail on a bad tag is worse than a
        // missing one, because the caller's integrity guarantee evaporates
        // silently. The one AEAD that IS implemented stays admitted.
        assert!(!refuses_algorithm("AES/GCM/NoPadding"));
    }

    /// MUST RAISE. It was never only ChaCha20 — `cipher_algorithm_known`
    /// accepted a whole catalogue, and every name in it reached the same ECB
    /// arm. Measured: `Blowfish` and `RC4` produced the SAME ciphertext as each
    /// other from a 16-byte key, because both were AES-128-ECB.
    #[test]
    fn the_other_names_that_were_silently_aes_are_refused_too() {
        for t in [
            "Blowfish", "RC4", "ARCFOUR", "RC2", "IDEA", "SEED", "SM4", "Camellia", "Twofish",
            "Serpent", "CAST5", "Salsa20", "Skipjack", "ECIES", "ElGamal", "NULL",
        ] {
            assert!(refuses_algorithm(t), "{t} must be refused, not served as AES");
        }
        assert!(refuses_algorithm("CRATONVM-NO-SUCH-CIPHER"));
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
            "AES/KWP/NoPadding",
            "AES_128/KWP/NoPadding",
            "DESede/ECB/PKCS5Padding",
            "DESede",
        ] {
            assert!(refuses_algorithm(t), "{t} must be refused at getInstance");
        }
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
        // AEAD and key wrap take NoPadding only.
        assert!(refuses_padding("AES/GCM/PKCS5Padding"));
        assert!(refuses_padding("AES/KW/PKCS5Padding"));
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
        assert_eq!(transformation_pinned_key_len("AES_128/GCM/NoPadding"), Some(16));
        assert_eq!(transformation_pinned_key_len("AES_192/CBC/NoPadding"), Some(24));
        assert_eq!(transformation_pinned_key_len("AES_256/ECB/NoPadding"), Some(32));
        assert_eq!(transformation_pinned_key_len("AES/GCM/NoPadding"), None);
        // HotSpot's measured wording, both flavours.
        assert_eq!(
            aes_key_length_reason("AES_128/GCM/NoPadding", 32).as_deref(),
            Some("The key must be 16 bytes")
        );
        assert_eq!(
            aes_key_length_reason("AES/GCM/NoPadding", 17).as_deref(),
            Some("Invalid AES key length: 17 bytes")
        );
        // MUST STILL WORK: the legal sizes, and the families this rule does
        // not govern.
        assert!(aes_key_length_reason("AES_128/GCM/NoPadding", 16).is_none());
        for n in [16, 24, 32] {
            assert!(aes_key_length_reason("AES/CBC/PKCS5Padding", n).is_none());
        }
        assert!(aes_key_length_reason("RSA/ECB/PKCS1Padding", 294).is_none());
        assert!(aes_key_length_reason("PBEWithHmacSHA1AndAES_128", 9).is_none());
        assert!(aes_key_length_reason("DESede/CBC/PKCS5Padding", 24).is_none());
        // An unreadable key is a different diagnosis and must not be reported
        // as a length complaint.
        assert!(aes_key_length_reason("AES/GCM/NoPadding", 0).is_none());
    }

    /// A bare `AES` really is `AES/ECB/PKCS5Padding` on SunJCE (measured:
    /// HotSpot's ciphertext for the two is byte-identical), so that default is
    /// kept — but it is now a property of the AES arm rather than of every
    /// algorithm name that reaches `parse_transformation`.
    #[test]
    fn the_ecb_default_is_scoped_to_the_family_that_has_one() {
        assert!(transformation_is_serviceable("AES"));
        assert!(refuses_algorithm("ChaCha20"));
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
        assert!(n_len > 0 && e_len > 0, "the real key components are recorded");
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
