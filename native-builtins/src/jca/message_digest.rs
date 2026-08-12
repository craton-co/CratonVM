// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP6.2 — `java.security.MessageDigest` real-JDK native dispatch.
//!
//! The probe (`apps/digest_probe/DigestProbe.java`) calls
//! `MessageDigest.getInstance(algo)` → `update(byte[])` → `digest()` for
//! six algorithms (SHA-256/384/512, SHA3-256, SHA-1, MD5).  Real-JDK 25
//! routes that through `Security.getInstance` → `Provider.getService` →
//! `MessageDigestSpi.engineDigest` — a path that depends on
//! `sun.security.util.Debug`, `Provider$ServiceKey`, and the spiMap
//! reflective bootstrap.  None of those work yet, so we override the
//! six callables on `java/security/MessageDigest` directly and return
//! the digest bytes that match HotSpot byte-for-byte.
//!
//! ## Layout
//!
//! `MessageDigest` is allocated as a 2-field synthetic via
//! `alloc_concurrent_synthetic` (auto-widened to the real-JDK
//! instance-field count by `class_num_total_fields`):
//!
//! | Slot | Field                     |
//! |------|---------------------------|
//! |  0   | `algorithm` (String)      |
//! |  1   | `data` (byte[] accumulator) — appended to by `update`, hashed by `digest` |
//!
//! The accumulator strategy keeps state outside of the JDK's internal
//! `Sun*` Spi instances, which we deliberately do not allocate.
//!
//! ## Implementation
//!
//! Hashing uses `crate::compute_digest` (re-exported `pub(crate)` from
//! `lib.rs`) which dispatches to the in-tree `real_md5` / `real_sha1` /
//! `real_sha256` / `real_sha384` / `real_sha512` constant-time
//! implementations and the `sha3` crate for SHA-3.  Verified output
//! matches HotSpot 25.0.1 for the probe's six algorithms.

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ObjectRef, Value};

use crate::{try_alloc_concurrent_synthetic, compute_digest, obj_arg};

// C14 fix: the previous implementation keyed the side-table on `ObjectRef`,
// whose `Hash` impl derives from the raw pointer (`self.ptr.as_ptr() as usize`,
// see `types/src/value.rs`).  When the GC relocates a `MessageDigest`
// instance during compaction the entry becomes orphaned: every accumulated
// `update(byte[])` byte is unreachable and `digest()` on the relocated
// receiver hashes an empty input — silent data loss.
//
// Switch to keying on `NativeContext::identity_hash_code(this)`, which is
// GC-stable (`HashCodeTable::update_after_gc` remaps the table on
// compaction — see `gc/src/compact_header.rs`).  Mirrors the pattern from
// `lang_invoke::VH_META_TABLE` (`native-builtins/src/lang_invoke.rs:178+`).

// Slot indices (synthetic-mode layout — real-JDK Field map is wider so
// these are only used as fallbacks when the receiver isn't an actual
// real-JDK MessageDigest instance).  Real-JDK reads/writes go through
// `set_field_by_name("algorithm", ...)` which resolves against the
// loaded class layout (algorithm:String, state:int, provider:Provider).
const FIELD_ALGO: usize = 0;

// ---------------------------------------------------------------------------
// Side-table for the byte accumulator
//
// MessageDigest's real-JDK layout doesn't have a `byte[] data` slot — every
// real `Sun*` Spi keeps the streaming hash state inside its own internal
// arrays.  We don't allocate those Spi instances, so we need somewhere to
// put the accumulator.  Using a free slot on the MessageDigest object
// works in synthetic mode but is fragile in real-JDK mode (the object is
// allocated with the real layout, every "free" slot collides with an
// inherited field).  Side-table keyed by `identity_hash_code(receiver)`
// sidesteps that: the heap object stays untouched, the data lives in
// process-wide state, and the identity hash survives GC compaction.
// ---------------------------------------------------------------------------

fn accumulators() -> &'static parking_lot::Mutex<rustc_hash::FxHashMap<i32, Vec<u8>>> {
    use std::sync::OnceLock;
    static ACC: OnceLock<parking_lot::Mutex<rustc_hash::FxHashMap<i32, Vec<u8>>>> = OnceLock::new();
    ACC.get_or_init(|| parking_lot::Mutex::new(rustc_hash::FxHashMap::default()))
}

fn read_accumulator(ctx: &mut dyn NativeContext, this: ObjectRef) -> Vec<u8> {
    let key = ctx.identity_hash_code(this);
    accumulators().lock().get(&key).cloned().unwrap_or_default()
}

fn write_accumulator(ctx: &mut dyn NativeContext, this: ObjectRef, bytes: &[u8]) {
    let key = ctx.identity_hash_code(this);
    accumulators().lock().insert(key, bytes.to_vec());
}

fn append_accumulator(ctx: &mut dyn NativeContext, this: ObjectRef, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    let key = ctx.identity_hash_code(this);
    accumulators()
        .lock()
        .entry(key)
        .or_default()
        .extend_from_slice(bytes);
}

/// Check whether an accumulator entry exists for `this`.  Used by `digest()`
/// to distinguish "freshly-reset, empty input" from "side-table never seen
/// this receiver" (which after the GC-key fix should only happen if the
/// receiver was never returned by our `getInstance`).
fn accumulator_present(ctx: &mut dyn NativeContext, this: ObjectRef) -> bool {
    let key = ctx.identity_hash_code(this);
    accumulators().lock().contains_key(&key)
}

/// Read a Java-byte array from a `Value::Object(Some(arr))`.
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

/// Materialise a Java byte[] populated with `bytes`.
fn make_byte_array(ctx: &mut dyn NativeContext, bytes: &[u8]) -> ObjectRef {
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
    for (i, &b) in bytes.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
    }
    arr
}

// ---------------------------------------------------------------------------
// Native callbacks
// ---------------------------------------------------------------------------

/// `MessageDigest.getInstance(String)` → `MessageDigest`.
///
/// Validates the algorithm name against the supported set.  HotSpot's
/// JDK 25 surfaces unsupported algorithms via `NoSuchAlgorithmException`
/// — we mirror that contract; the probe depends on the supported set
/// (SHA-256/384/512, SHA3-256, SHA-1, MD5) succeeding.
fn md_get_instance(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let algo_raw = match args.first() {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    };
    if algo_raw.is_empty() {
        return Err(RuntimeError::IllegalArgumentException {
            message: "algorithm must be non-null".to_string(),
        }
        .into());
    }
    if !algorithm_supported(&algo_raw) {
        // Real JDK throws `NoSuchAlgorithmException`, which every caller
        // catches by name. This used to raise `SecurityException` on the
        // premise that "we don't carry NSAE" — but the crate does construct a
        // genuine `java/security/NoSuchAlgorithmException` (see
        // `provider_chain::throw_no_such_algorithm`, which `KeyFactory`
        // already uses), and `SecurityException` is unchecked, so it sailed
        // straight past every `catch (NoSuchAlgorithmException)` handler
        // instead of being handled. Message text was already correct.
        return Err(crate::jca::provider_chain::throw_no_such_algorithm_public(
            ctx,
            &format!("{algo_raw} MessageDigest not available"),
        ));
    }
    let md = try_alloc_concurrent_synthetic(ctx, "java/security/MessageDigest", 4)?;
    let algo_str = ctx.create_string(&algo_raw);
    // Real JDK has `algorithm:String` as a declared instance field; resolve
    // it by name so the slot index matches the actual class layout (the
    // raw slot 0 collides with `MessageDigestSpi.tempArray` and stores
    // a byte[] there, which `getAlgorithm()` then reads back as null).
    ctx.set_field_by_name(md, "algorithm", Value::Object(Some(algo_str)));
    // Synthetic fallback slot for legacy callers that go through raw
    // slot indexing.  Harmless if it lands on an inherited field — the
    // accumulator and algorithm both flow through the side-table /
    // by-name path going forward.
    ctx.set_field(md, FIELD_ALGO, Value::Object(Some(algo_str)));
    // Initialise the side-table accumulator to empty so update→digest
    // round-trips don't see stale state from a previous getInstance
    // (the side-table is process-wide, keyed by identity hash code).
    // We `write_accumulator(..&[])` rather than `reset_accumulator` so
    // the post-GC missing-state check in `md_digest` can distinguish
    // "this instance was initialised but never written" from "this
    // instance was never seen by getInstance" (orphan lookup).
    write_accumulator(ctx, md, &[]);
    Ok(Some(Value::Object(Some(md))))
}

/// `MessageDigest.getInstance(String, String|Provider)` → `MessageDigest`.
///
/// Same digest as the single-argument form; the provider argument only decides
/// whether the call is legal at all. Real JDK resolves the named provider
/// before the algorithm, so an unregistered name is `NoSuchProviderException`
/// and an empty one is `IllegalArgumentException` — see
/// `provider_chain::check_named_provider_arg`.
fn md_get_instance_with_provider(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    crate::jca::provider_chain::check_named_provider_arg(
        ctx,
        args,
        1,
        crate::jca::provider_chain::ProviderArgWording::Shared,
    )?;
    // Once a provider has been named, an unsupported algorithm is reported
    // against THAT provider ("no such algorithm: X for provider Y"), not with
    // the provider-less "X MessageDigest not available" wording the
    // single-argument form uses. Verified against HotSpot 25.
    let algo = match args.first() {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    };
    if !algo.is_empty() && !algorithm_supported(&algo) {
        if let Some(Value::Object(Some(p))) = args.get(1) {
            if let Some(provider) = ctx.read_string(*p) {
                return Err(crate::jca::provider_chain::throw_no_such_algorithm_public(
                    ctx,
                    &format!("no such algorithm: {algo} for provider {provider}"),
                ));
            }
        }
    }
    md_get_instance(ctx, args)
}

/// Read the algorithm name back from a MessageDigest receiver.  Tries
/// the real-JDK `algorithm:String` field first, falls back to slot 0.
fn read_algo(ctx: &mut dyn NativeContext, this: ObjectRef) -> String {
    let by_name = ctx.get_field_by_name(this, "algorithm");
    if let Value::Object(Some(s)) = by_name {
        if let Some(t) = ctx.read_string(s) {
            if !t.is_empty() {
                return t;
            }
        }
    }
    match ctx.get_field(this, FIELD_ALGO) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_else(|| "SHA-256".to_string()),
        _ => "SHA-256".to_string(),
    }
}

fn md_update_bytes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let arr = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let bytes = read_byte_array(ctx, arr);
    append_accumulator(ctx, this, &bytes);
    Ok(None)
}

fn md_update_byte(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let b = match args.get(1) {
        Some(Value::Int(v)) => *v as u8,
        _ => 0,
    };
    append_accumulator(ctx, this, &[b]);
    Ok(None)
}

fn md_update_bytes_off(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let arr = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let off = match args.get(2) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let len = match args.get(3) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let total = ctx.array_length(arr);
    if off.saturating_add(len) > total {
        return Err(RuntimeError::aioobe_index_only((off + len) as i32).into());
    }
    let mut bytes = Vec::with_capacity(len);
    for i in 0..len {
        if let Value::Int(b) = ctx.get_array_element(arr, off + i) {
            bytes.push(b as u8);
        }
    }
    append_accumulator(ctx, this, &bytes);
    Ok(None)
}

/// `MessageDigest.update(ByteBuffer)` — consume the buffer's remaining bytes
/// (advancing its position to the limit, per the JDK contract) and feed them to
/// the digest accumulator. Without this native, the real `update(ByteBuffer)`
/// bytecode runs `engineUpdate(b, off, len)` on our synthetic bare
/// `java.security.MessageDigest`, resolving to the abstract `MessageDigestSpi`
/// method → `AbstractMethodError: engineUpdate([BII)V has no Code attribute`.
/// Groovy hashes through this overload (it aborted every test in the `buildSrc`
/// `SpringRepositoriesExtensionTests`). Reading through the buffer's own bulk
/// `get([B)` works for both heap and direct buffers regardless of layout.
/// See `apps/spring-boot/cratonvm-bug-reports/SB-13`.
fn md_update_bytebuffer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let buf = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let rem = match ctx.invoke_virtual(buf, "remaining", "()I", &[])? {
        Some(Value::Int(n)) if n > 0 => n as usize,
        _ => return Ok(None),
    };
    let tmp = ctx.new_array(cratonvm_types::ArrayElementType::Byte, rem);
    // Bulk get reads `rem` bytes and advances position → limit (consumes input).
    ctx.invoke_virtual(
        buf,
        "get",
        "([B)Ljava/nio/ByteBuffer;",
        &[Value::Object(Some(tmp))],
    )?;
    let mut bytes = Vec::with_capacity(rem);
    for i in 0..rem {
        if let Value::Int(b) = ctx.get_array_element(tmp, i) {
            bytes.push(b as u8);
        }
    }
    append_accumulator(ctx, this, &bytes);
    Ok(None)
}

fn md_digest(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // C14: surface side-table misses as a loud IllegalStateException rather
    // than silently hashing empty input.  After the identity-hash-code key
    // fix, the only way to land here with no entry is if the receiver was
    // never produced by our `getInstance` — caller bug, not a GC artefact.
    if !accumulator_present(ctx, this) {
        return Err(RuntimeError::IllegalStateException {
            message: "MessageDigest state missing post-GC or never initialized".into(),
        }
        .into());
    }
    let algo = read_algo(ctx, this);
    let data = read_accumulator(ctx, this);
    // Canonicalise the ALIAS spellings only here, not in the stored field:
    // `getAlgorithm()` echoes the caller's `SHAKE128` on HotSpot, while
    // `compute_digest` only has arms for the primary names.
    let hash = compute_digest(canonical_algorithm(&algo), &data)?;
    // Reset accumulator after digest() per JDK contract.  Re-seed with an
    // empty entry so subsequent update→digest round-trips on the same
    // instance still satisfy the presence check above.
    write_accumulator(ctx, this, &[]);
    let arr = make_byte_array(ctx, &hash);
    Ok(Some(Value::Object(Some(arr))))
}

/// `digest(byte[])` — equivalent to `update(byte[]); return digest();`.
fn md_digest_input(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let arr = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let bytes = read_byte_array(ctx, arr);
    append_accumulator(ctx, this, &bytes);
    md_digest(ctx, &[Value::Object(Some(this))])
}

/// `digest(byte[] buf, int offset, int len)` — compute the digest and write it
/// into the caller's buffer, returning the number of bytes written (JDK
/// contract). Without this native the real `MessageDigest.digest([BII)I`
/// bytecode runs `engineDigest(buf, off, len)` on our bare synthetic
/// `java.security.MessageDigest`; the default
/// `MessageDigestSpi.engineDigest(byte[],int,int)` then calls the abstract
/// no-arg `engineDigest()` → `AbstractMethodError: engineDigest()[B has no Code
/// attribute`. SunJCE's `ML_KEM.generateKemKeyPair` hashes the public key
/// through this overload (the SHA3-256 `digest(out, off, len)` of FIPS-203
/// keygen), so every ML-KEM `KeyPairGenerator.generateKeyPair()` aborted here
/// before reaching the KEM SPI.
fn md_digest_into(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let buf = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "No output buffer given".to_string(),
            }
            .into())
        }
    };
    let offset = match args.get(2) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let len = match args.get(3) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    if !accumulator_present(ctx, this) {
        return Err(RuntimeError::IllegalStateException {
            message: "MessageDigest state missing post-GC or never initialized".into(),
        }
        .into());
    }
    let algo = read_algo(ctx, this);
    let data = read_accumulator(ctx, this);
    // Alias-canonicalised for the same reason as `md_digest`.
    let hash = compute_digest(canonical_algorithm(&algo), &data)?;
    // JDK contract (MessageDigestSpi.engineDigest(byte[],int,int)): the caller's
    // window must be able to hold the whole digest, else DigestException.
    let buf_len = ctx.array_length(buf);
    if len < hash.len() {
        return Err(throw_digest_exception(ctx, "partial digests not returned"));
    }
    if buf_len.saturating_sub(offset) < hash.len() {
        return Err(throw_digest_exception(
            ctx,
            "insufficient space in the output buffer to store the digest",
        ));
    }
    for (i, &b) in hash.iter().enumerate() {
        ctx.set_array_element(buf, offset + i, Value::Int(b as i8 as i32));
    }
    // Reset accumulator after digest() per JDK contract (see md_digest).
    write_accumulator(ctx, this, &[]);
    Ok(Some(Value::Int(hash.len() as i32)))
}

/// Construct & throw a real `java.security.DigestException` (a
/// `GeneralSecurityException`, caught by Java callers exactly as under HotSpot).
/// Falls back to a catchable `IllegalStateException` if the JDK class can't be
/// built.
fn throw_digest_exception(ctx: &mut dyn NativeContext, msg: &str) -> MethodCallFailed {
    let detail = ctx.create_string(msg);
    if let Ok(Some(Value::Object(Some(exc)))) = ctx.new_object_initialized(
        "java/security/DigestException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(detail))],
    ) {
        return MethodCallFailed::ExceptionThrown(exc);
    }
    RuntimeError::IllegalStateException {
        message: msg.to_string(),
    }
    .into()
}

fn md_reset(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // Reset to an empty accumulator (rather than removing the entry) so
    // the post-GC presence check in `md_digest` still recognises this
    // instance after `reset()`.
    write_accumulator(ctx, this, &[]);
    Ok(None)
}

fn md_get_algorithm(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let by_name = ctx.get_field_by_name(this, "algorithm");
    if matches!(&by_name, Value::Object(Some(_))) {
        return Ok(Some(by_name));
    }
    Ok(Some(ctx.get_field(this, FIELD_ALGO)))
}

fn md_get_digest_length(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let algo = read_algo(ctx, this);
    // `digest_length_bytes` returns `Option` and has no default arm, so a
    // digest object carrying an algorithm this VM cannot compute reports 0
    // rather than corroborating a fabricated 32-byte answer. This receiver
    // cannot exist through either `getInstance` — both gate on
    // `algorithm_supported` — so a `None` here means the algorithm field was
    // corrupted or overwritten, and 0 is the honest report of that.
    // W7-63-jca-advertise-vs-serve.md.
    Ok(Some(Value::Int(
        digest_length_bytes(&algo).unwrap_or(0) as i32
    )))
}

/// `getProvider()` returns a fresh "SUN" Provider synthetic so callers
/// who chain `md.getProvider().getName()` see a sensible answer.  Reuses
/// the layout-aware path from `provider_chain::make_provider` (set fields
/// by name so the real-JDK class layout is honoured).
fn md_get_provider(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // Shared with `Mac`, `SecretKeyFactory` and `Signature` — this was the
    // original of that object shape and three copies had accreted from it.
    let p = crate::jca::make_named_provider(ctx, "SUN")?;
    Ok(Some(Value::Object(Some(p))))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The alphanumeric-only normalisation `algorithm_supported` and
/// `digest_length_bytes` both key on. `crate::compute_digest` uses a
/// `-`/`/`-stripping filter instead; the two agree on every name this engine
/// serves, which `shake_normalisations_agree_across_the_two_filters` asserts
/// rather than assumes.
fn normalise(algo: &str) -> String {
    algo.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_uppercase()
}

/// Resolve an `Alg.Alias.MessageDigest.*` spelling onto the primary service
/// name, before any of the three name-keyed tables is consulted.
///
/// HotSpot 25's `SUN` carries `Alg.Alias.MessageDigest.SHAKE128 =
/// SHAKE128-256` and `SHAKE256 = SHAKE256-512`; measured, `getInstance`
/// resolves both bare spellings and returns bytes byte-identical to the
/// hyphenated primaries, while `Security.getAlgorithms("MessageDigest")`
/// lists only the two primaries.
///
/// **This is the half of W7-63 §3 #2 that did not land.** The alias rows went
/// into the provider chain (`put_alias(SUN, "MessageDigest", "SHAKE128", …)`)
/// and the ratchet asserted them through `get_service_entry` — but
/// `md_get_instance` consults NO registry: its only gate is
/// `algorithm_supported`, whose own test asserted the bare spellings must be
/// rejected "because they are resolved by the provider chain's alias table
/// before this predicate is consulted". They are not; nothing on this path
/// reads that table, so `MessageDigest.getInstance("SHAKE128")` raised
/// `NoSuchAlgorithmException` in both shipping modes while the registry-level
/// ratchet stayed green. That test's other premise — that admitting the
/// aliases here would grow `Security.getAlgorithms("MessageDigest")` to 17 —
/// is false in the same way and for the same reason: the advertised set is
/// built by `provider_chain::algorithms_for_service` from the SERVICE rows,
/// which this predicate cannot reach either.
///
/// The caller's own spelling is what `md_get_instance` stores, so
/// `getAlgorithm()` still echoes `SHAKE128` as HotSpot does; only the three
/// name-keyed tables see the canonical form.
fn canonical_algorithm(algo: &str) -> &str {
    match normalise(algo).as_str() {
        "SHAKE128" => "SHAKE128-256",
        "SHAKE256" => "SHAKE256-512",
        _ => algo,
    }
}

/// The set of digests `MessageDigest.getInstance` will serve. This is the
/// gate the ADVERTISED set is ratcheted against — see
/// `provider_chain::every_advertised_sun_message_digest_is_serviceable` —
/// so a name added here without a `compute_digest` arm, or added to the
/// provider seed without being added here, fails that test rather than
/// reaching a caller. W7-63-jca-advertise-vs-serve.md.
///
/// Kept arm-for-arm in lockstep with `digest_length_bytes` and with
/// `crate::compute_digest`. All three are pinned together by
/// `every_supported_algorithm_computes_and_has_a_length`.
fn algorithm_supported(algo: &str) -> bool {
    let normalised = normalise(canonical_algorithm(algo));
    matches!(
        normalised.as_str(),
        // RFC 1319. HotSpot 25's SUN carries MD2 and this VM advertised it
        // for three waves while refusing it; `crate::real_md2` implements it
        // now. Broken as a hash, present for parity.
        "MD2"
            | "MD5"
            | "SHA"
            | "SHA1"
            | "SHA224"
            | "SHA256"
            | "SHA384"
            | "SHA512"
            // FIPS 180-4 §5.3.6 truncated SHA-512 variants (RFC 7616 DIGEST
            // auth offers SHA-512-256). The alphanumeric-only normalise above
            // collapses "SHA-512/256" → "SHA512256", "SHA-512/224" → "SHA512224".
            | "SHA512224"
            | "SHA512256"
            | "SHA3224"
            | "SHA3256"
            | "SHA3384"
            | "SHA3512"
            // The two SHAKE XOFs read out to a fixed length. Same two
            // normalised spellings `compute_digest`'s `-`/`/`-stripping
            // filter produces — asserted, not assumed, by
            // `shake_normalisations_agree_across_the_two_filters`.
            | "SHAKE128256"
            | "SHAKE256512"
    )
}

/// The synthetic-mode `crate::native_md_get_instance` needs this same gate —
/// it used to carry a second, drifted literal list with an empty `if` body.
/// One predicate, two doors.
pub(crate) fn algorithm_supported_public(algo: &str) -> bool {
    algorithm_supported(algo)
}

/// The synthetic-mode `crate::native_md_get_digest_length` needs this same
/// table. It used to carry a THIRD hand-maintained copy, drifted furthest of
/// the three — no MD2, no SHA-224, no truncated SHA-512s, no SHA3-224, and a
/// `-`-only normalisation that let `SHA-512/256` fall through to the default
/// and report 32 by accident rather than by arm. One table, every door.
pub(crate) fn digest_length_bytes_public(algo: &str) -> Option<usize> {
    digest_length_bytes(algo)
}

/// `None` for an algorithm this VM does not implement.
///
/// This used to end `_ => 32`, so `getDigestLength()` corroborated the
/// `compute_digest` SHA-256 fallback: a caller that asked for an
/// unimplemented digest got 32 bytes AND a `getDigestLength()` of 32 telling
/// it that was right. Two independent-looking observations agreeing because
/// they shared one wrong default is the shape that makes this species so hard
/// to see from inside. W7-63-jca-advertise-vs-serve.md.
fn digest_length_bytes(algo: &str) -> Option<usize> {
    let normalised = normalise(canonical_algorithm(algo));
    Some(match normalised.as_str() {
        "MD2" | "MD5" => 16,
        "SHA" | "SHA1" => 20,
        "SHA224" | "SHA3224" | "SHA512224" => 28,
        "SHA256" | "SHA3256" | "SHA512256" | "SHAKE128256" => 32,
        "SHA384" | "SHA3384" => 48,
        "SHA512" | "SHA3512" | "SHAKE256512" => 64,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub(crate) fn register(r: &mut NativeMethodRegistry) {
    let md = "java/security/MessageDigest";
    r.register(
        md,
        "getInstance",
        "(Ljava/lang/String;)Ljava/security/MessageDigest;",
        md_get_instance,
    );
    // The two-argument overloads used to be left to real JDK bytecode, which
    // reaches `Security.getImpl` → `getinstance_instance_provider` → the
    // provider-chain SERVICE TABLE. No `MessageDigest` service is registered
    // under "SUN" there, so `MessageDigest.getInstance("SHA-256", "SUN")` —
    // ordinary, correct application code — threw NoSuchAlgorithmException
    // while the single-argument form of the very same digest worked. Route
    // them at the same intrinsic instead, after validating the provider
    // argument the way real JDK does.
    r.register(
        md,
        "getInstance",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/security/MessageDigest;",
        md_get_instance_with_provider,
    );
    r.register(
        md,
        "getInstance",
        "(Ljava/lang/String;Ljava/security/Provider;)Ljava/security/MessageDigest;",
        md_get_instance_with_provider,
    );
    r.register(md, "update", "([B)V", md_update_bytes);
    r.register(md, "update", "(B)V", md_update_byte);
    r.register(md, "update", "([BII)V", md_update_bytes_off);
    r.register(
        md,
        "update",
        "(Ljava/nio/ByteBuffer;)V",
        md_update_bytebuffer,
    );
    r.register(md, "digest", "()[B", md_digest);
    r.register(md, "digest", "([B)[B", md_digest_input);
    r.register(md, "digest", "([BII)I", md_digest_into);
    r.register(md, "reset", "()V", md_reset);
    r.register(md, "getAlgorithm", "()Ljava/lang/String;", md_get_algorithm);
    r.register(md, "getDigestLength", "()I", md_get_digest_length);
    r.register(
        md,
        "getProvider",
        "()Ljava/security/Provider;",
        md_get_provider,
    );
    r.register(md, "clone", "()Ljava/lang/Object;", md_clone);
}

/// `MessageDigest.clone()` — JDK `MessageDigest`s whose SPI is `Cloneable`
/// support `clone()` to snapshot in-progress state (Gradle's
/// `Hashing$MessageDigestHashFunction` clones a template digest per hash). Our
/// synthetic MessageDigest is not `Cloneable`, so `Object.clone()` threw
/// `CloneNotSupportedException`. Produce a fresh MessageDigest with the same
/// algorithm and a COPY of the byte accumulator (snapshot semantics).
fn md_clone(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let algo = read_algo(ctx, this);
    let acc = read_accumulator(ctx, this);
    let md = try_alloc_concurrent_synthetic(ctx, "java/security/MessageDigest", 4)?;
    let algo_str = ctx.create_string(&algo);
    ctx.set_field_by_name(md, "algorithm", Value::Object(Some(algo_str)));
    ctx.set_field(md, FIELD_ALGO, Value::Object(Some(algo_str)));
    write_accumulator(ctx, md, &acc);
    Ok(Some(Value::Object(Some(md))))
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess};
    use super::*;

    #[test]
    fn algorithm_supported_accepts_known_set() {
        for algo in [
            "MD5",
            "md5",
            "SHA-1",
            "SHA1",
            "SHA-224",
            "sha-224",
            "SHA224",
            "SHA-256",
            "SHA256",
            "SHA-384",
            "SHA-512",
            "SHA-512/224",
            "SHA-512/256",
            "sha-512/256",
            "SHA3-224",
            "SHA3-256",
            "SHA3-384",
            "SHA3-512",
            "MD2",
            "md2",
            "SHAKE128-256",
            "shake128-256",
            "SHAKE256-512",
        ] {
            assert!(algorithm_supported(algo), "{algo} should be supported");
        }
    }

    #[test]
    fn algorithm_supported_rejects_unknown() {
        for algo in ["BLAKE2", "Whirlpool", "FAKEHASH", ""] {
            assert!(!algorithm_supported(algo), "{algo} should be rejected");
        }
    }

    /// The bare `SHAKE128` / `SHAKE256` spellings are `Alg.Alias` rows on
    /// HotSpot, and `getInstance` resolves them there. They used to be pinned
    /// as REJECTED here, on two premises that were both false: that "the
    /// provider chain's alias table [resolves them] before this predicate is
    /// consulted" — `md_get_instance` reads no registry, this predicate is its
    /// whole gate — and that admitting them would grow
    /// `Security.getAlgorithms("MessageDigest")` from 15 to 17, which it
    /// cannot, because that set is built from the SERVICE rows by
    /// `provider_chain::algorithms_for_service` and never consults this
    /// function. Both halves are asserted below, so the pair cannot drift back
    /// into "advertised count is right, `getInstance` refuses the name".
    /// W7-63-jca-advertise-vs-serve.md §3 #2, alias half.
    #[test]
    fn the_shake_aliases_resolve_but_are_not_separate_algorithms() {
        for (alias, canonical) in [("SHAKE128", "SHAKE128-256"), ("SHAKE256", "SHAKE256-512")] {
            assert!(
                algorithm_supported(alias),
                "{alias} is an alias of {canonical} and MessageDigest.getInstance must serve it"
            );
            assert_eq!(canonical_algorithm(alias), canonical);
            assert_eq!(digest_length_bytes(alias), digest_length_bytes(canonical));
            assert_eq!(
                compute_digest(canonical_algorithm(alias), b"abc").unwrap(),
                compute_digest(canonical, b"abc").unwrap(),
                "{alias} must produce byte-identical output to {canonical}, which is what \
                 HotSpot 25 was measured doing"
            );
        }
        // Anti-vacuity: canonicalisation must be confined to the two aliases.
        assert_eq!(canonical_algorithm("SHA-256"), "SHA-256");
        assert_eq!(canonical_algorithm("FAKEHASH"), "FAKEHASH");
    }

    /// The gate, the length table and `compute_digest` are three separate
    /// match arms over the same set of names, and this campaign's dominant
    /// defect is exactly two such lists drifting apart. Assert they agree
    /// rather than maintaining them by hand.
    #[test]
    fn every_supported_algorithm_computes_and_has_a_length() {
        for algo in [
            "MD2",
            "MD5",
            "SHA-1",
            "SHA-224",
            "SHA-256",
            "SHA-384",
            "SHA-512",
            "SHA-512/224",
            "SHA-512/256",
            "SHA3-224",
            "SHA3-256",
            "SHA3-384",
            "SHA3-512",
            "SHAKE128-256",
            "SHAKE256-512",
        ] {
            assert!(algorithm_supported(algo), "{algo} must be supported");
            let want = digest_length_bytes(algo)
                .unwrap_or_else(|| panic!("{algo} is supported but has no declared length"));
            let got = compute_digest(algo, b"advertised must equal served")
                .unwrap_or_else(|_| panic!("{algo} is supported but compute_digest refuses it"));
            assert_eq!(
                got.len(),
                want,
                "{algo}: getDigestLength() says {want} but compute_digest produced {}",
                got.len()
            );
        }
    }

    /// `compute_digest` normalises by stripping `-` and `/`;
    /// `algorithm_supported` / `digest_length_bytes` normalise by filtering to
    /// alphanumerics. For the SHAKE names the two happen to agree, which is
    /// what makes ONE pair of arm spellings valid in both files. That is a
    /// coincidence of these particular names, not a property of the two
    /// functions, so it is asserted rather than assumed —
    /// W7-29-jca-advertise-implement-gaps.md asked for exactly this.
    #[test]
    fn shake_normalisations_agree_across_the_two_filters() {
        for (raw, expected) in [
            ("SHAKE128-256", "SHAKE128256"),
            ("SHAKE256-512", "SHAKE256512"),
            ("SHA-512/256", "SHA512256"),
        ] {
            let strip: String = raw.to_uppercase().replace(['-', '/'], "");
            let alnum: String = raw
                .chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .collect::<String>()
                .to_uppercase();
            assert_eq!(strip, expected, "strip-filter normalisation of {raw}");
            assert_eq!(alnum, expected, "alphanumeric normalisation of {raw}");
        }
    }

    /// Known-answer vectors, measured on jdk-25.0.3.9-hotspot and recorded in
    /// probes/JcaAdvertisedVsServedProbe.expected.txt §C. A round trip cannot
    /// catch a wrong algorithm — encrypt-then-decrypt or hash-then-hash with
    /// the same wrong function agrees with itself — so the acceptance test for
    /// a newly implemented digest has to be a published vector.
    #[test]
    fn shake_matches_hotspot_vectors() {
        let hex = |h: Vec<u8>| -> String { h.iter().map(|b| format!("{b:02x}")).collect() };
        assert_eq!(
            hex(compute_digest("SHAKE128-256", b"").unwrap()),
            "7f9c2ba4e88f827d616045507605853ed73b8093f6efbc88eb1a6eacfa66ef26"
        );
        assert_eq!(
            hex(compute_digest("SHAKE128-256", b"abc").unwrap()),
            "5881092dd818bf5cf8a3ddb793fbcba74097d5c526a6d35f97b83351940f2cc8"
        );
        assert_eq!(
            hex(compute_digest("SHAKE256-512", b"").unwrap()),
            "46b9dd2b0ba88d13233b3feb743eeb243fcd52ea62b81b82b50c27646ed5762f\
             d75dc4ddd8c0f200cb05019d67b592f6fc821c49479ab48640292eacb3b7c4be"
        );
        assert_eq!(
            hex(compute_digest("SHAKE256-512", b"abc").unwrap()),
            "483366601360a8771c6863080cc4114d8db44530f8f1e1ee4f94ea37e78b5739\
             d5a15bef186a5386c75744c0527e1faa9f8726e462a12a4feb06bd8801e751e4"
        );
    }

    /// The fallback that made this record necessary. `compute_digest` used to
    /// end `_ => Ok(real_sha256(data))`, so an unimplemented name was served
    /// SHA-256's bytes under its own label.
    ///
    /// Note the second half. Asserting only "the unknown name errors" would
    /// pass on a `compute_digest` that errored on EVERYTHING, which is a
    /// probe that cannot fail in the direction that matters. The control
    /// establishes that the same call shape succeeds for a real name.
    #[test]
    fn an_unimplemented_digest_fails_rather_than_answering_sha256() {
        assert!(
            compute_digest("SHA-256", b"abc").is_ok(),
            "control: a real algorithm must still compute, or this test is vacuous"
        );
        for bogus in ["NO-SUCH-DIGEST", "BLAKE2", "Whirlpool", ""] {
            assert!(
                compute_digest(bogus, b"abc").is_err(),
                "{bogus} must be refused, not served SHA-256's bytes"
            );
            assert!(
                digest_length_bytes(bogus).is_none(),
                "{bogus} must have no declared length, not a defaulted 32"
            );
        }
    }

    #[test]
    fn digest_lengths_match_jdk25() {
        assert_eq!(digest_length_bytes("MD2"), Some(16));
        assert_eq!(digest_length_bytes("MD5"), Some(16));
        assert_eq!(digest_length_bytes("SHA-1"), Some(20));
        assert_eq!(digest_length_bytes("SHA-224"), Some(28));
        assert_eq!(digest_length_bytes("SHA3-224"), Some(28));
        assert_eq!(digest_length_bytes("SHA-256"), Some(32));
        assert_eq!(digest_length_bytes("SHA-384"), Some(48));
        assert_eq!(digest_length_bytes("SHA-512"), Some(64));
        assert_eq!(digest_length_bytes("SHA-512/224"), Some(28));
        assert_eq!(digest_length_bytes("SHA-512/256"), Some(32));
        assert_eq!(digest_length_bytes("SHA3-256"), Some(32));
        assert_eq!(digest_length_bytes("SHA3-384"), Some(48));
        assert_eq!(digest_length_bytes("SHA3-512"), Some(64));
        assert_eq!(digest_length_bytes("SHAKE128-256"), Some(32));
        assert_eq!(digest_length_bytes("SHAKE256-512"), Some(64));
    }

    #[test]
    fn compute_digest_matches_hotspot_for_hello_world() {
        // SHA-256 of "hello world" — published HotSpot 25.0.1 result.
        let hash = compute_digest("SHA-256", b"hello world").unwrap();
        let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn compute_digest_sha224_matches_reference() {
        // SHA-224 / SHA3-224 of "hello world" — Python hashlib reference.
        // Lowercase + dashed spelling exercises the alphanumeric-uppercase
        // normalisation (the `sha-224` Keycloak SD-JWT path).
        let hex = |h: Vec<u8>| -> String { h.iter().map(|b| format!("{b:02x}")).collect() };
        assert_eq!(
            hex(compute_digest("sha-224", b"hello world").unwrap()),
            "2f05477fc24bb4faefd86517156dafdecec45b8ad3cf2522a563582b"
        );
        assert_eq!(
            hex(compute_digest("SHA3-224", b"hello world").unwrap()),
            "dfb7f18c77e928bb56faeb2da27291bd790bc1045cde45f3210bb6c5"
        );
    }

    #[test]
    fn compute_digest_sha512_truncated_matches_fips_vectors() {
        // FIPS 180-4 §5.3.6 truncated SHA-512 variants. The "/256" and "/224"
        // spellings exercise the alphanumeric/slash normalisation. These are
        // the published FIPS example vectors for the one-block "abc" message
        // and the empty input — both distinct from SHA-256/SHA-224 (the whole
        // point of the alternate IV).
        let hex = |h: Vec<u8>| -> String { h.iter().map(|b| format!("{b:02x}")).collect() };
        assert_eq!(
            hex(compute_digest("SHA-512/256", b"abc").unwrap()),
            "53048e2681941ef99b2e29b76b4c7dabe4c2d0c634fc6d46e0e2f13107e7af23"
        );
        assert_eq!(
            hex(compute_digest("SHA-512/256", b"").unwrap()),
            "c672b8d1ef56ed28ab87c3622c5114069bdd3ad7b8f9737498d0c01ecef0967a"
        );
        assert_eq!(
            hex(compute_digest("SHA-512/224", b"abc").unwrap()),
            "4634270f707b6a54daae7530460842e20e37ed265ceee9a43e8924aa"
        );
        assert_eq!(
            hex(compute_digest("SHA-512/224", b"").unwrap()),
            "6ed0dd02806fa89e25de060c19d3ac86cabb87d6a0ddd05c333b84f4"
        );
        // Confirm SHA-512/256 ≠ SHA-256 (alternate IV, not a plain truncation).
        assert_ne!(
            hex(compute_digest("SHA-512/256", b"abc").unwrap()),
            hex(compute_digest("SHA-256", b"abc").unwrap())
        );
    }
}
