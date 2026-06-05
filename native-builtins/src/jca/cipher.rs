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

use crate::{alloc_concurrent_synthetic, obj_arg};
use crate::crypto_impl::{Aes, AesGcm};
use crate::phases_early::CIPHER_IV;

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
        ctx.set_static_field_by_name(
            "java/security/Security",
            "spiMap",
            Value::Object(Some(m)),
        );
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

/// Allocate a freshly initialised Cipher synthetic and register an
/// empty `CipherState` keyed on its heap pointer.  The algorithm
/// string is stashed in the side-table — we do **not** write to any
/// instance field of the real JDK class, because field-5 is
/// `initialized:Z` (a primitive boolean) and storing an Object there
/// breaks the `expected object reference` invariant on read-back.
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
    if let Ok(Some(Value::Object(Some(arr)))) = ctx.invoke_virtual(
        key_obj,
        "getEncoded",
        "()[B",
        &[],
    ) {
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

    if key_bytes.is_empty() {
        return Err(RuntimeError::IllegalStateException {
            message: "No key provided".into(),
        }
        .into());
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
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
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
        r.register("java/security/Security", "<clinit>", "()V", security_clinit_spimap);
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
    r.register("sun/security/jca/ProviderList", "<clinit>", "()V", clinit_noop);

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
            let obj = cipher_alloc(ctx, algo);
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    r.register(cipher, "init", "(ILjava/security/Key;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let mode = args[1].as_int().unwrap_or(0);
        let key = obj_arg(args, 2)?;
        let key_bytes = extract_key_bytes(ctx, key);
        let tkey = obj_key(ctx, this);
        with_table_write(|t| {
            let s = t.entry(tkey).or_default();
            s.mode = mode;
            s.key_bytes = key_bytes;
            s.accumulated.clear();
            s.aad.clear();
        });
        Ok(None)
    });

    r.register(
        cipher,
        "init",
        "(ILjava/security/Key;Ljava/security/spec/AlgorithmParameterSpec;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let mode = args[1].as_int().unwrap_or(0);
            let key = obj_arg(args, 2)?;
            let key_bytes = extract_key_bytes(ctx, key);
            let iv_bytes = match args.get(3) {
                Some(Value::Object(Some(spec))) => extract_iv_bytes(ctx, *spec),
                _ => Vec::new(),
            };
            let tkey = obj_key(ctx, this);
            with_table_write(|t| {
                let s = t.entry(tkey).or_default();
                s.mode = mode;
                s.key_bytes = key_bytes;
                s.iv_bytes = iv_bytes;
                s.accumulated.clear();
                s.aad.clear();
            });
            Ok(None)
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
            let key_bytes = extract_key_bytes(ctx, key);
            let iv_bytes = match args.get(3) {
                Some(Value::Object(Some(spec))) => extract_iv_bytes(ctx, *spec),
                _ => Vec::new(),
            };
            let tkey = obj_key(ctx, this);
            with_table_write(|t| {
                let s = t.entry(tkey).or_default();
                s.mode = mode;
                s.key_bytes = key_bytes;
                s.iv_bytes = iv_bytes;
                s.accumulated.clear();
                s.aad.clear();
            });
            Ok(None)
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
            let key_bytes = extract_key_bytes(ctx, key);
            let tkey = obj_key(ctx, this);
            with_table_write(|t| {
                let s = t.entry(tkey).or_default();
                s.mode = mode;
                s.key_bytes = key_bytes;
                s.accumulated.clear();
                s.aad.clear();
            });
            Ok(None)
        },
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

    r.register(
        cipher,
        "getAlgorithm",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let tkey = obj_key(ctx, this);
            let algo = with_table_read(|t| {
                t.get(&tkey).map(|s| s.algorithm.clone()).unwrap_or_default()
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
        let iv_bytes = with_table_read(|t| {
            t.get(&tkey).map(|s| s.iv_bytes.clone()).unwrap_or_default()
        });
        if iv_bytes.is_empty() {
            return Ok(Some(Value::Object(None)));
        }
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, iv_bytes.len());
        for (i, &b) in iv_bytes.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
        }
        Ok(Some(Value::Object(Some(arr))))
    });
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
            r.find("java/security/Provider", "<clinit>", "()V").is_none(),
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
        assert!(r
            .find("javax/crypto/Cipher", "doFinal", "([B)[B")
            .is_some());

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
    fn parse_transformation_bare_aes() {
        let (cipher, mode, pad) = parse_transformation("AES");
        assert_eq!(cipher, "AES");
        assert_eq!(mode, "ECB");
        assert!(pad);
    }
}
