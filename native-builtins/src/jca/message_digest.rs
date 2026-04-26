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

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::{MethodCallResult, RuntimeError};
use rustjvm_types::{ObjectRef, Value};

use crate::{alloc_concurrent_synthetic, compute_digest, obj_arg};

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
// inherited field).  Side-table keyed by `ObjectRef` sidesteps that:
// the heap object stays untouched, the data lives in process-wide state,
// and the receiver identity (ObjectRef) is the lookup key.
// ---------------------------------------------------------------------------

fn accumulators() -> &'static parking_lot::Mutex<rustc_hash::FxHashMap<ObjectRef, Vec<u8>>> {
    use std::sync::OnceLock;
    static ACC: OnceLock<parking_lot::Mutex<rustc_hash::FxHashMap<ObjectRef, Vec<u8>>>> =
        OnceLock::new();
    ACC.get_or_init(|| parking_lot::Mutex::new(rustc_hash::FxHashMap::default()))
}

fn read_accumulator(_ctx: &mut dyn NativeContext, this: ObjectRef) -> Vec<u8> {
    accumulators()
        .lock()
        .get(&this)
        .cloned()
        .unwrap_or_default()
}

fn write_accumulator(_ctx: &mut dyn NativeContext, this: ObjectRef, bytes: &[u8]) {
    accumulators().lock().insert(this, bytes.to_vec());
}

fn append_accumulator(_ctx: &mut dyn NativeContext, this: ObjectRef, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    accumulators()
        .lock()
        .entry(this)
        .or_default()
        .extend_from_slice(bytes);
}

fn reset_accumulator(this: ObjectRef) {
    accumulators().lock().remove(&this);
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
    let arr = ctx.new_array(rustjvm_types::ArrayElementType::Byte, bytes.len());
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
        // The JDK throws NoSuchAlgorithmException; rustjvm's nearest
        // mapping is SecurityException (we don't carry NSAE).  Use an
        // illegal-argument message that spells out the bad algorithm so
        // callers see a useful trace.
        return Err(RuntimeError::SecurityException {
            message: format!("{algo_raw} MessageDigest not available"),
        }
        .into());
    }
    let md = alloc_concurrent_synthetic(ctx, "java/security/MessageDigest", 4);
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
    // (the side-table is process-wide, keyed by ObjectRef).
    reset_accumulator(md);
    Ok(Some(Value::Object(Some(md))))
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
        return Err(RuntimeError::ArrayIndexOutOfBoundsException {
            index: (off + len) as i32,
        }
        .into());
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

fn md_digest(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let algo = read_algo(ctx, this);
    let data = read_accumulator(ctx, this);
    let hash = compute_digest(&algo, &data);
    // Reset accumulator after digest() per JDK contract.
    reset_accumulator(this);
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

fn md_reset(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    reset_accumulator(this);
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
    Ok(Some(Value::Int(digest_length_bytes(&algo) as i32)))
}

/// `getProvider()` returns a fresh "SUN" Provider synthetic so callers
/// who chain `md.getProvider().getName()` see a sensible answer.  Reuses
/// the layout-aware path from `provider_chain::make_provider` (set fields
/// by name so the real-JDK class layout is honoured).
fn md_get_provider(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let p = alloc_concurrent_synthetic(ctx, "java/security/Provider", 8);
    let name = ctx.create_string("SUN");
    let info = ctx.create_string("SUN security provider (rust-jvm)");
    let ver_str = ctx.create_string("25");
    ctx.set_field_by_name(p, "name", Value::Object(Some(name)));
    ctx.set_field_by_name(p, "version", Value::Double(25.0));
    ctx.set_field_by_name(p, "versionStr", Value::Object(Some(ver_str)));
    ctx.set_field_by_name(p, "info", Value::Object(Some(info)));
    ctx.set_field(p, 0, Value::Object(Some(name)));
    ctx.set_field(p, 1, Value::Double(25.0));
    ctx.set_field(p, 2, Value::Object(Some(info)));
    Ok(Some(Value::Object(Some(p))))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn algorithm_supported(algo: &str) -> bool {
    let normalised: String = algo
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_uppercase();
    matches!(
        normalised.as_str(),
        "MD5"
            | "SHA"
            | "SHA1"
            | "SHA256"
            | "SHA384"
            | "SHA512"
            | "SHA3256"
            | "SHA3384"
            | "SHA3512"
    )
}

fn digest_length_bytes(algo: &str) -> usize {
    let normalised: String = algo
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_uppercase();
    match normalised.as_str() {
        "MD5" => 16,
        "SHA" | "SHA1" => 20,
        "SHA256" | "SHA3256" => 32,
        "SHA384" | "SHA3384" => 48,
        "SHA512" | "SHA3512" => 64,
        _ => 32,
    }
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
    r.register(md, "update", "([B)V", md_update_bytes);
    r.register(md, "update", "(B)V", md_update_byte);
    r.register(md, "update", "([BII)V", md_update_bytes_off);
    r.register(md, "digest", "()[B", md_digest);
    r.register(md, "digest", "([B)[B", md_digest_input);
    r.register(md, "reset", "()V", md_reset);
    r.register(md, "getAlgorithm", "()Ljava/lang/String;", md_get_algorithm);
    r.register(md, "getDigestLength", "()I", md_get_digest_length);
    r.register(md, "getProvider", "()Ljava/security/Provider;", md_get_provider);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn algorithm_supported_accepts_known_set() {
        for algo in [
            "MD5", "md5", "SHA-1", "SHA1", "SHA-256", "SHA256", "SHA-384", "SHA-512", "SHA3-256",
            "SHA3-384", "SHA3-512",
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

    #[test]
    fn digest_lengths_match_jdk25() {
        assert_eq!(digest_length_bytes("MD5"), 16);
        assert_eq!(digest_length_bytes("SHA-1"), 20);
        assert_eq!(digest_length_bytes("SHA-256"), 32);
        assert_eq!(digest_length_bytes("SHA-384"), 48);
        assert_eq!(digest_length_bytes("SHA-512"), 64);
        assert_eq!(digest_length_bytes("SHA3-256"), 32);
        assert_eq!(digest_length_bytes("SHA3-384"), 48);
        assert_eq!(digest_length_bytes("SHA3-512"), 64);
    }

    #[test]
    fn compute_digest_matches_hotspot_for_hello_world() {
        // SHA-256 of "hello world" — published HotSpot 25.0.1 result.
        let hash = compute_digest("SHA-256", b"hello world");
        let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex, "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }
}
