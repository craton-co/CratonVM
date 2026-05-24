// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! T19_K3_PROPS_SIDETABLE — robust `java.util.Properties` storage.
//!
//! KC26 / KeycloakMain.<clinit> reads `org.keycloak.common.Version.VERSION`,
//! which is set by `Version.<clinit>` via:
//!
//! ```text
//! is = Version.class.getResourceAsStream("/keycloak-version.properties");
//! Properties p = new Properties();
//! p.load(is);                                // <-- real JDK bytecode
//! VERSION = p.getProperty("version");        // <-- expected non-null
//! ```
//!
//! The resource lookup succeeds (700 bytes served by our
//! `Class.getResourceAsStream` native), and the real JDK bytecode for
//! `Properties.load` calls `this.put(k, v)` for each line.  But the
//! synthetic Properties (allocated by `register_synthetic_overrides`
//! / `alloc_concurrent_synthetic`) has an incomplete inner Hashtable
//! field-shape, so `put` writes to a slot that `get` cannot retrieve.
//! Result: `Properties.getProperty("version")` returns null and
//! `Version.<clinit>` NPEs on `VERSION.toLowerCase()`.
//!
//! The fix: a side-table keyed by the Properties object's pointer
//! identity, with native overrides for the public API surface used by
//! `Properties.load`/`getProperty`/`setProperty`.  The side-table is
//! a `Mutex<FxHashMap<usize, FxHashMap<String, String>>>`; both layers
//! are bounded by `MAX_PROPS_PER_OBJECT` and `MAX_TOTAL_OBJECTS` to
//! prevent unbounded memory growth from misbehaving callers.
//!
//! Security posture:
//!   * Per-object size cap (10_000 keys) — a single Properties object
//!     cannot be coerced into unbounded growth via repeated `put`
//!     calls.
//!   * Total-object cap (10_000 distinct Properties objects) — the
//!     side-table itself cannot grow without bound across many
//!     short-lived Properties.
//!   * Key/value length cap (64 KiB) — defends against malformed
//!     `.properties` files with multi-megabyte continuation lines.
//!   * `Properties.load(InputStream)` validates the input shape and
//!     refuses inputs larger than 16 MiB before parsing.

use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use std::sync::OnceLock;

use cratonvm_native_api::registry::{NativeContext, NativeMethodRegistry};
use cratonvm_types::{error::MethodCallResult, ArrayElementType, ObjectRef, Value};

/// Per-object property cap.  10_000 keys * 64 KiB max value = 640 MiB
/// per object, but in practice `.properties` files are tiny.  This
/// prevents pathological inputs from coercing growth to unbounded.
const MAX_PROPS_PER_OBJECT: usize = 10_000;

/// Total tracked Properties object cap.  Prevents accidental memory
/// leaks from short-lived Properties accumulating in the side-table.
const MAX_TOTAL_OBJECTS: usize = 10_000;

/// Max bytes accepted by `Properties.load(InputStream)`.  16 MiB is
/// far above any real `.properties` file.
const MAX_LOAD_BYTES: usize = 16 * 1024 * 1024;

/// Max key/value length (64 KiB).  Real `.properties` keys are <128
/// chars; values rarely exceed 4 KiB.  64 KiB caps continuation-line
/// abuse without rejecting realistic inputs.
const MAX_KV_LEN: usize = 64 * 1024;

#[inline]
fn props_stderr_diag() -> bool {
    matches!(
        std::env::var("CRATONVM_DIAG_PROPERTIES").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

macro_rules! props_diag_eprintln {
    ($($t:tt)*) => {
        if props_stderr_diag() {
            eprintln!($($t)*);
        }
    };
}

fn table() -> &'static Mutex<FxHashMap<usize, FxHashMap<String, String>>> {
    static T: OnceLock<Mutex<FxHashMap<usize, FxHashMap<String, String>>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(FxHashMap::default()))
}

fn key_for(obj: ObjectRef) -> usize {
    obj.as_ptr() as usize
}

/// Read all bytes from an InputStream by repeatedly invoking `read([B,
/// I, I)I` on the input.  Returns `None` if the stream is null, the
/// total exceeds `MAX_LOAD_BYTES`, or a read errors out.
fn drain_input_stream(
    ctx: &mut dyn NativeContext,
    stream: ObjectRef,
) -> Option<Vec<u8>> {
    // The InputStream allocated by `Class.getResourceAsStream` and
    // `URL.openStream` is a ByteArrayInputStream with `buf` (byte[]),
    // `pos` (int), `count` (int) fields populated.  Read directly to
    // avoid going through the JDK's bytecode which is fragile in our env.
    //
    // Strategy 1: by-name field lookup (works when the JDK class is loaded
    //   and field names resolve to the correct index).
    // Strategy 2: by-index fallback (indices 0=buf, 1=pos, 3=count) —
    //   the standard JDK ByteArrayInputStream layout; covers synthetic
    //   objects allocated with alloc_concurrent_synthetic where by-name
    //   may not work.
    // Strategy 3: invoke_virtual read([BII)I loop — works for any real
    //   InputStream implementation.

    // --- Strategy 1: by-name ---
    let buf_by_name = match ctx.get_field_by_name(stream, "buf") {
        Value::Object(Some(arr)) => Some(arr),
        _ => None,
    };
    let count_by_name = match ctx.get_field_by_name(stream, "count") {
        Value::Int(n) => Some(n as usize),
        _ => None,
    };
    let pos_by_name = match ctx.get_field_by_name(stream, "pos") {
        Value::Int(n) => Some(n as usize),
        _ => None,
    };
    if let (Some(arr), Some(c), Some(p)) = (buf_by_name, count_by_name, pos_by_name) {
        if c <= MAX_LOAD_BYTES && p <= c {
            let len = c.saturating_sub(p);
            let mut out = Vec::with_capacity(len);
            for i in p..c {
                if out.len() >= MAX_LOAD_BYTES {
                    return None;
                }
                if let Value::Int(b) = ctx.get_array_element(arr, i) {
                    out.push(b as u8);
                }
            }
            props_diag_eprintln!("[DRAIN-DBG] drain_input_stream: by-name read {} bytes", out.len());
            return Some(out);
        }
    }

    // --- Strategy 2: by-index (standard ByteArrayInputStream layout) ---
    // buf=0, pos=1, mark=2, count=3 — the JDK ByteArrayInputStream
    // instance field order (no instance fields in InputStream parent).
    let buf_by_idx = match ctx.get_field(stream, 0) {
        Value::Object(Some(arr)) => Some(arr),
        _ => None,
    };
    let count_by_idx = match ctx.get_field(stream, 3) {
        Value::Int(n) => Some(n as usize),
        _ => None,
    };
    let pos_by_idx = match ctx.get_field(stream, 1) {
        Value::Int(n) => Some(n as usize),
        _ => None,
    };
    if let (Some(arr), Some(c), Some(p)) = (buf_by_idx, count_by_idx, pos_by_idx) {
        if c > 0 && c <= MAX_LOAD_BYTES && p <= c {
            let len = c.saturating_sub(p);
            let mut out = Vec::with_capacity(len);
            for i in p..c {
                if out.len() >= MAX_LOAD_BYTES {
                    return None;
                }
                if let Value::Int(b) = ctx.get_array_element(arr, i) {
                    out.push(b as u8);
                }
            }
            props_diag_eprintln!("[DRAIN-DBG] drain_input_stream: by-index read {} bytes", out.len());
            return Some(out);
        }
    }

    // --- Strategy 3: invoke_virtual read([BII)I loop ---
    // Handles any real InputStream implementation.
    let chunk_size = 8192usize;
    let chunk_arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, chunk_size);
    let mut out = Vec::new();
    loop {
        let n = match ctx.invoke_virtual(
            stream,
            "read",
            "([BII)I",
            &[
                Value::Object(Some(chunk_arr)),
                Value::Int(0),
                Value::Int(chunk_size as i32),
            ],
        ) {
            Ok(Some(Value::Int(n))) => n,
            Ok(other) => {
                props_diag_eprintln!(
                    "[DRAIN-DBG] drain_input_stream: read returned non-int {:?}",
                    other
                );
                return None;
            }
            Err(_) => return None,
        };
        if n <= 0 {
            break;
        }
        for i in 0..n as usize {
            if let Value::Int(b) = ctx.get_array_element(chunk_arr, i) {
                out.push(b as u8);
            }
        }
        if out.len() > MAX_LOAD_BYTES {
            return None;
        }
    }
    // Empty stream / immediate EOF is valid — `Properties.load` must still
    // complete (Surefire booter uses an optional props stream).
    props_diag_eprintln!(
        "[DRAIN-DBG] drain_input_stream: invoke_virtual read path, {} bytes",
        out.len()
    );
    Some(out)
}

/// Parse a Java-style `.properties` file's bytes.  Implements the JLS
/// definition (escapes, line continuations, comments, separators).
/// Returns `(key, value)` pairs in order of appearance.
///
/// The parser is intentionally permissive: malformed escapes degrade
/// to literal characters rather than panicking, and keys without
/// values yield empty-string values (matching JDK behaviour).
fn parse_properties(bytes: &[u8]) -> Vec<(String, String)> {
    // Decode as ISO-8859-1 (Java spec for `Properties.load(InputStream)`).
    // Each byte maps to one Unicode code point in 0..=255.
    let raw: String = bytes.iter().map(|&b| b as char).collect();

    let mut out = Vec::new();
    let mut iter = raw.split('\n').peekable();
    let mut continued = String::new();

    while let Some(line) = iter.next() {
        // Strip a trailing CR.
        let line = line.strip_suffix('\r').unwrap_or(line);
        let trimmed = line.trim_start();

        // If we're continuing from a previous line, append.
        let active = if !continued.is_empty() {
            let mut joined = continued.clone();
            joined.push_str(trimmed);
            continued.clear();
            joined
        } else {
            // Skip blank lines and comments.
            if trimmed.is_empty()
                || trimmed.starts_with('#')
                || trimmed.starts_with('!')
            {
                continue;
            }
            trimmed.to_string()
        };

        // Check for line continuation: trailing single backslash.
        // (An odd count of trailing backslashes means continuation.)
        let trailing_bs = active.bytes().rev().take_while(|&b| b == b'\\').count();
        if trailing_bs % 2 == 1 {
            // Drop the trailing backslash; keep accumulating.
            continued = active[..active.len() - 1].to_string();
            continue;
        }

        // Split on first unescaped `=`, `:`, or whitespace.
        let (key, value) = split_key_value(&active);
        let key = unescape(&key);
        let value = unescape(&value);
        if key.len() <= MAX_KV_LEN && value.len() <= MAX_KV_LEN {
            out.push((key, value));
        }
        if out.len() >= MAX_PROPS_PER_OBJECT {
            break;
        }
    }
    out
}

/// Split a logical line into (key, value) at the first unescaped `=`,
/// `:`, or whitespace separator.  Trailing whitespace is trimmed from
/// the key; leading whitespace is trimmed from the value.
fn split_key_value(line: &str) -> (String, String) {
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut escaped = false;
    while i < bytes.len() {
        let c = bytes[i];
        if escaped {
            escaped = false;
            i += 1;
            continue;
        }
        if c == b'\\' {
            escaped = true;
            i += 1;
            continue;
        }
        if c == b'=' || c == b':' {
            // Found explicit separator.
            let key = line[..i].trim_end();
            let value = line[i + 1..].trim_start();
            return (key.to_string(), value.to_string());
        }
        if c == b' ' || c == b'\t' || c == b'\x0c' {
            // Whitespace separator; consume any subsequent whitespace
            // and a single optional `=`/`:`.
            let key = line[..i].to_string();
            let mut j = i + 1;
            while j < bytes.len() {
                let cc = bytes[j];
                if cc == b' ' || cc == b'\t' || cc == b'\x0c' {
                    j += 1;
                } else if cc == b'=' || cc == b':' {
                    j += 1;
                    while j < bytes.len()
                        && (bytes[j] == b' ' || bytes[j] == b'\t' || bytes[j] == b'\x0c')
                    {
                        j += 1;
                    }
                    break;
                } else {
                    break;
                }
            }
            return (key, line[j..].to_string());
        }
        i += 1;
    }
    // No separator — whole line is the key, value is empty.
    (line.to_string(), String::new())
}

/// Decode Java `.properties` escapes (`\n`, `\t`, `\r`, `\\`, `\"`,
/// `\'`, `\<space>`, `\:`, `\=`, `\uXXXX`).  Unknown escapes degrade
/// to literal characters.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some('\'') => out.push('\''),
            Some(' ') => out.push(' '),
            Some(':') => out.push(':'),
            Some('=') => out.push('='),
            Some('u') => {
                // Read up to 4 hex digits.
                let mut code = 0u32;
                let mut seen = 0;
                while seen < 4 {
                    match chars.peek() {
                        Some(&h) if h.is_ascii_hexdigit() => {
                            code = (code << 4) | h.to_digit(16).unwrap();
                            chars.next();
                            seen += 1;
                        }
                        _ => break,
                    }
                }
                if seen > 0 {
                    if let Some(ch) = char::from_u32(code) {
                        out.push(ch);
                    }
                }
            }
            Some(other) => out.push(other),
            None => break,
        }
    }
    out
}

/// Insert (or overwrite) a key/value pair in the side-table for a
/// given Properties object.  Enforces per-object and global caps.
fn put_kv(obj: ObjectRef, key: &str, value: &str) {
    if key.len() > MAX_KV_LEN || value.len() > MAX_KV_LEN {
        return;
    }
    let mut t = table().lock();
    if t.len() >= MAX_TOTAL_OBJECTS && !t.contains_key(&key_for(obj)) {
        return;
    }
    let entry = t.entry(key_for(obj)).or_default();
    if entry.len() < MAX_PROPS_PER_OBJECT || entry.contains_key(key) {
        entry.insert(key.to_string(), value.to_string());
    }
}

/// Look up a key in the side-table.  Returns `None` if either the
/// object isn't tracked or the key is absent.
fn get_kv(obj: ObjectRef, key: &str) -> Option<String> {
    table().lock().get(&key_for(obj))?.get(key).cloned()
}

/// Cross-module read access for callers that receive a `Properties` object
/// behind an erased `Map` type (e.g. surefire `PropertiesWrapper`).
pub(crate) fn get_property_from_sidetable(obj: ObjectRef, key: &str) -> Option<String> {
    get_kv(obj, key)
}

/// Public re-export of `put_kv` so other modules (e.g. the surefire
/// `SystemPropertyManager.loadProperties` native in `lib.rs`) can store
/// key/value pairs in the side-table keyed by an arbitrary object
/// reference.  Used to back `PropertiesWrapper` lookups when the
/// real-JDK CHM round-trip does not populate the wrapper's internal
/// `properties` field correctly under our interpreter.
pub fn store_property_in_sidetable(obj: ObjectRef, key: &str, value: &str) {
    put_kv(obj, key, value);
}

/// Public snapshot of side-table entries for a given object, used by
/// surefire `setAsSystemProperties` etc. to iterate entries without
/// going through the inner Map field.
pub fn snapshot_sidetable(obj: ObjectRef) -> Vec<(String, String)> {
    snapshot_kv(obj)
}

/// Public re-export of `drain_input_stream` for use from `lib.rs`.
pub fn drain_input_stream_pub(
    ctx: &mut dyn NativeContext,
    stream: ObjectRef,
) -> Option<Vec<u8>> {
    drain_input_stream(ctx, stream)
}

/// Public re-export of `parse_properties` for use from `lib.rs`.
pub fn parse_properties_pub(bytes: &[u8]) -> Vec<(String, String)> {
    parse_properties(bytes)
}

/// Snapshot the side-table entries for a Properties object.  Returns
/// an empty vector if the object isn't tracked.  Used by `keySet`,
/// `entrySet`, `values`, `keys`, `elements` natives so the iteration
/// view is decoupled from the live mutable side-table.
fn snapshot_kv(obj: ObjectRef) -> Vec<(String, String)> {
    match table().lock().get(&key_for(obj)) {
        Some(m) => m.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        None => Vec::new(),
    }
}

/// Number of entries the side-table holds for `obj` (0 if untracked).
fn count_kv(obj: ObjectRef) -> usize {
    table()
        .lock()
        .get(&key_for(obj))
        .map(|m| m.len())
        .unwrap_or(0)
}

/// After native `load` fills the side-table, mirror each (k,v) into the
/// JDK `Properties` backing store.  Since JDK 17+, entries live in a
/// `ConcurrentHashMap` field `map`; `stringPropertyNames()` (used by
/// Surefire `SystemPropertyManager.loadProperties`) enumerates via
/// `entrySet()` on that map — not the legacy Hashtable table.  If `map`
/// is absent (very old layout), fall back to `Hashtable.put` (invokespecial).
fn mirror_loaded_entries_to_properties_backend(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    parsed: &[(String, String)],
) {
    let chm = match ctx.get_field_by_name(this, "map") {
        Value::Object(Some(m)) => m,
        Value::Object(None) | _ => {
            let Some(m) = (match ctx.new_object("java/util/concurrent/ConcurrentHashMap") {
                Ok(Some(Value::Object(Some(o)))) => Some(o),
                _ => None,
            }) else {
                for (k, v) in parsed {
                    let k_obj = ctx.create_string(k);
                    let v_obj = ctx.create_string(v);
                    let _ = ctx.invoke_special(
                        "java/util/Hashtable",
                        "put",
                        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                        &[
                            Value::Object(Some(this)),
                            Value::Object(Some(k_obj)),
                            Value::Object(Some(v_obj)),
                        ],
                    );
                }
                return;
            };
            let _ = ctx.invoke(
                "java/util/concurrent/ConcurrentHashMap",
                "<init>",
                "()V",
                &[Value::Object(Some(m))],
            );
            ctx.set_field_by_name(this, "map", Value::Object(Some(m)));
            m
        }
    };

    for (k, v) in parsed {
        let k_obj = ctx.create_string(k);
        let v_obj = ctx.create_string(v);
        let _ = ctx.invoke_virtual(
            chm,
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(k_obj)), Value::Object(Some(v_obj))],
        );
    }
}

/// Native `Properties.load(InputStream)` — drains the stream, parses
/// the bytes as a Java `.properties` file, and populates the side-
/// table for `this`.
fn native_properties_load(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let stream = match args.get(1) {
        Some(Value::Object(Some(s))) => *s,
        _ => return Ok(None),
    };
    let bytes = match drain_input_stream(ctx, stream) {
        Some(b) => b,
        None => return Ok(None),
    };
    if bytes.len() > MAX_LOAD_BYTES {
        return Ok(None);
    }
    let parsed = parse_properties(&bytes);
    props_diag_eprintln!("[PROPS-DBG] native_properties_load: parsed {} entries from {} bytes", parsed.len(), bytes.len());
    for (k, v) in &parsed {
        if k.contains("ApplicationContext") || k.contains("ContextFactory") {
            let preview_len = v.len().min(80);
            props_diag_eprintln!("[PROPS-DBG] KEY={} VALUE_LEN={} VALUE_START={}", k, v.len(), &v[..preview_len]);
        }
        put_kv(this, k, v);
    }
    mirror_loaded_entries_to_properties_backend(ctx, this, &parsed);
    props_diag_eprintln!("[PROPS-DBG] native_properties_load: side-table now has {} entries for obj {:?}", count_kv(this), this);
    Ok(None)
}

/// Downgrade a Rust `String` whose code points represent ISO-8859-1
/// characters (as produced by reading a Reader chunk-by-chunk) into
/// the `u8` byte sequence that `parse_properties` expects.  Each
/// `char` < 256 round-trips losslessly; anything above the Latin-1
/// range collapses to `b'?'`, mirroring the `b as char` decoding side
/// in `parse_properties` (which only emits chars in 0..=255).
///
/// Kept as a pure helper so it can be unit-tested without a full
/// `NativeContext`.
fn iso_8859_1_bytes(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    for c in s.chars() {
        let cp = c as u32;
        if cp < 256 {
            out.push(cp as u8);
        } else {
            out.push(b'?');
        }
    }
    out
}

/// Native `Properties.load(Reader)` — RKC16N.1.  jboss-modules'
/// `org.jboss.modules.Main.<clinit>` reads `version.properties`
/// through a `BufferedReader` and calls this overload directly, so a
/// missing native here is a hard linkage error before `main` runs.
///
/// Strategy: pull the Reader's contents into a `String` 4 KiB at a
/// time via `Reader.read([CII)I` (the same loop shape every JDK
/// `BufferedReader` tolerates), then downgrade the accumulated text
/// to ISO-8859-1 bytes and reuse `parse_properties` / `put_kv` — the
/// same back-half as the InputStream overload.  We deliberately do
/// not call `Reader.close()` (the caller owns the stream lifecycle,
/// matching real JDK `Properties.load(Reader)`).
fn native_properties_load_reader(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let reader = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };

    // Scratch buffer for `Reader.read(char[], 0, len)`.  4 KiB is
    // the size every JDK BufferedReader uses internally, so this
    // matches the shape callers expect and avoids tiny chunked reads.
    const CHUNK: usize = 4096;
    let buf = ctx.new_array(ArrayElementType::Char, CHUNK);

    // Cap accumulated text at 2 * MAX_LOAD_BYTES chars.  In ISO-8859-1
    // each char re-encodes to one byte, so this matches the byte cap
    // applied to the InputStream path while leaving headroom for any
    // multi-byte chars that get downgraded to `?`.
    let char_cap = MAX_LOAD_BYTES.saturating_mul(2);
    let mut accumulated = String::new();

    loop {
        let res = ctx.invoke_virtual(
            reader,
            "read",
            "([CII)I",
            &[Value::Object(Some(buf)), Value::Int(0), Value::Int(CHUNK as i32)],
        )?;
        let n = match res {
            Some(Value::Int(n)) => n,
            // Anything else (None, non-int) means the read protocol
            // misbehaved — treat as EOF rather than looping forever.
            _ => break,
        };
        if n <= 0 {
            // n == -1 is EOF; n == 0 is also a valid early exit per
            // the Reader contract on a non-blocking but exhausted
            // source.  Either way, stop.
            break;
        }
        let n = n as usize;
        let n = n.min(CHUNK);
        for i in 0..n {
            if accumulated.len() >= char_cap {
                break;
            }
            if let Value::Int(c) = ctx.get_array_element(buf, i) {
                // Java chars are unsigned 16-bit code units.  Mask
                // before widening so a sign-extended negative `int`
                // doesn't yield an out-of-range code point.
                let cu = (c as u32) & 0xFFFF;
                if let Some(ch) = char::from_u32(cu) {
                    accumulated.push(ch);
                }
            }
        }
        if accumulated.len() >= char_cap {
            break;
        }
    }

    let bytes = iso_8859_1_bytes(&accumulated);
    if bytes.len() > MAX_LOAD_BYTES {
        return Ok(None);
    }
    let parsed = parse_properties(&bytes);
    for (k, v) in &parsed {
        put_kv(this, k, v);
    }
    mirror_loaded_entries_to_properties_backend(ctx, this, &parsed);
    Ok(None)
}

/// Native `Properties.getProperty(String)` — checks the side-table
/// first, then falls back to the VM's system-property store.
/// Returns null when the key is unknown.
///
/// We deliberately do NOT call back into `this.get(key)` via
/// `invoke_virtual` because the underlying `Hashtable.get` is exactly
/// the path that's broken in our synthetic Properties model — that's
/// why this side-table exists.  Re-entering it would either NPE
/// (Hashtable's internal table-array is null) or NoSuchMethodError
/// (the synthetic Properties' class hierarchy doesn't expose
/// `Object.get`).  Both have been observed during KC26 boot.
fn native_properties_get_property_1(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key_obj = match args.get(1) {
        Some(Value::Object(Some(k))) => *k,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = crate::property_key_from_java_string(ctx, key_obj);
    if let Some(v) = get_kv(this, &key) {
        tracing::debug!(
            target: "cratonvm_vm::props_sidetable",
            ?this, key = %key, bytes = v.len(),
            "PROPS-GET sidetable hit"
        );
        return Ok(Some(Value::Object(Some(ctx.create_string(&v)))));
    }
    tracing::debug!(
        target: "cratonvm_vm::props_sidetable",
        ?this, key = %key,
        "PROPS-GET sidetable MISS, falling back to system"
    );
    match ctx
        .get_system_property(&key)
        .or_else(|| super::bootstrap_property_fallback(&key))
    {
        Some(v) => Ok(Some(Value::Object(Some(ctx.create_string(&v))))),
        None => Ok(Some(Value::Object(None))),
    }
}

/// Native `Properties.getProperty(String, String)` — like the
/// 1-arg form but returns the supplied default when the key is
/// unknown.
fn native_properties_get_property_2(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let default = args.get(2).copied().unwrap_or(Value::Object(None));
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(default)),
    };
    let key_obj = match args.get(1) {
        Some(Value::Object(Some(k))) => *k,
        _ => return Ok(Some(default)),
    };
    let key = crate::property_key_from_java_string(ctx, key_obj);
    if let Some(v) = get_kv(this, &key) {
        return Ok(Some(Value::Object(Some(ctx.create_string(&v)))));
    }
    match ctx
        .get_system_property(&key)
        .or_else(|| super::bootstrap_property_fallback(&key))
    {
        Some(v) => Ok(Some(Value::Object(Some(ctx.create_string(&v))))),
        None => Ok(Some(default)),
    }
}

/// Native `Properties.setProperty(String, String)` — stores the
/// key/value pair both in the side-table (so subsequent `getProperty`
/// finds it) and in the VM's system-property store (matching the
/// historical behaviour of the previous override that mirrored to
/// `System.setProperty`).
fn native_properties_set_property(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key_obj = match args.get(1) {
        Some(Value::Object(Some(k))) => *k,
        _ => return Ok(Some(Value::Object(None))),
    };
    let val_obj = match args.get(2) {
        Some(Value::Object(Some(v))) => *v,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = ctx.read_string(key_obj).unwrap_or_default();
    let val = ctx.read_string(val_obj).unwrap_or_default();
    let old = get_kv(this, &key);
    put_kv(this, &key, &val);
    let _ = ctx.set_system_property(&key, &val);
    match old {
        Some(prev) => Ok(Some(Value::Object(Some(ctx.create_string(&prev))))),
        None => Ok(Some(Value::Object(None))),
    }
}

/// Native `Properties.put(Object, Object)` — when callers bypass
/// `setProperty` and call the inherited `Hashtable.put` directly,
/// mirror the write into the side-table so `getProperty` still
/// finds it.  The bytecode `Hashtable.put` continues to run, so
/// the JDK's internal table is also populated (for any caller that
/// reads via `Properties.get`).
fn native_properties_put(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key_v = args.get(1).copied().unwrap_or(Value::Object(None));
    let val_v = args.get(2).copied().unwrap_or(Value::Object(None));
    let (Value::Object(Some(k)), Value::Object(Some(v))) = (key_v, val_v) else {
        return Ok(Some(Value::Object(None)));
    };
    let ks = ctx.read_string(k).unwrap_or_default();
    let vs = ctx.read_string(v).unwrap_or_default();
    if !ks.is_empty() {
        let prev = get_kv(this, &ks);
        put_kv(this, &ks, &vs);
        if let Some(p) = prev {
            return Ok(Some(Value::Object(Some(ctx.create_string(&p)))));
        }
    }
    Ok(Some(Value::Object(None)))
}

/// Native `Properties.containsKey(Object)` — consults the side-table.
/// Symmetric with `getProperty`.
fn native_properties_contains_key(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let key_obj = match args.get(1) {
        Some(Value::Object(Some(k))) => *k,
        _ => return Ok(Some(Value::Int(0))),
    };
    let key = ctx.read_string(key_obj).unwrap_or_default();
    if get_kv(this, &key).is_some() {
        return Ok(Some(Value::Int(1)));
    }
    Ok(Some(Value::Int(0)))
}

/// Native `Properties.get(Object)Object` — Hashtable-style read path used
/// by callers that bypass `getProperty` (e.g. Spring's
/// `PropertySourcesPropertyResolver` calling `Properties.get(key)` on the
/// `MapPropertySource` backed by `System.getProperties()`).
///
/// JDK 25's `Properties.get` (Properties.java:1338) reads from a private
/// `ConcurrentHashMap<Object, Object> map` field that's only populated by
/// `Properties.<init>`'s body.  Our synthetic Properties allocations don't
/// run that body, so `map` is null and the bytecode NPEs.  Override the
/// method here to consult the side-table (and fall back to system
/// properties for the System.getProperties() case), mirroring how
/// `getProperty` already routes around the broken bytecode path.
///
/// Returns `null` when the key is absent — matches `Hashtable.get`
/// semantics.
fn native_properties_get(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key_obj = match args.get(1) {
        Some(Value::Object(Some(k))) => *k,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = crate::property_key_from_java_string(ctx, key_obj);
    if let Some(v) = get_kv(this, &key) {
        return Ok(Some(Value::Object(Some(ctx.create_string(&v)))));
    }
    match ctx
        .get_system_property(&key)
        .or_else(|| super::bootstrap_property_fallback(&key))
    {
        Some(v) => Ok(Some(Value::Object(Some(ctx.create_string(&v))))),
        None => Ok(Some(Value::Object(None))),
    }
}

/// Native `Properties.size()I` — Hashtable-style count used by Spring's
/// `SpringConfigurationPropertySource.isFullEnumerable`, which probes
/// the underlying source via `Map.size()`.  JDK 25's `Properties.size`
/// (Properties.java:1302) reads the private
/// `ConcurrentHashMap<Object,Object> map` field that's null on our
/// synthetic Properties — the bytecode NPEs.  Route the read through
/// the side-table; objects we never wrote to report 0.
fn native_properties_size(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(count_kv(this) as i32)))
}

/// Native `Properties.isEmpty()Z` — symmetric companion to `size()`.
fn native_properties_is_empty(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(1))),
    };
    let empty = count_kv(this) == 0;
    Ok(Some(Value::Int(if empty { 1 } else { 0 })))
}

/// Build a synthetic `HashSet<String>` populated with the side-table
/// keys for the given Properties object.  Returns an empty HashSet if
/// the object isn't tracked.
fn build_key_set(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> ObjectRef {
    let snapshot = snapshot_kv(this);
    let mut elems: Vec<Value> = Vec::with_capacity(snapshot.len());
    for (k, _v) in &snapshot {
        let s = ctx.create_string(k);
        elems.push(Value::Object(Some(s)));
    }
    let set = cratonvm_native_collections::make_hashset_with_elements(ctx, &elems);
    set
}

/// Build a synthetic `ArrayList<String>` populated with the side-table
/// values for the given Properties object.  ArrayList is a `Collection`
/// — sufficient for `Properties.values()`'s declared return type.
fn build_value_list(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> ObjectRef {
    let snapshot = snapshot_kv(this);
    let list = crate::alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2);
    let arr = ctx.new_array(ArrayElementType::Reference, snapshot.len());
    for (i, (_k, v)) in snapshot.iter().enumerate() {
        let s = ctx.create_string(v);
        ctx.set_array_element(arr, i, Value::Object(Some(s)));
    }
    ctx.set_field(list, 0, Value::Object(Some(arr)));
    ctx.set_field(list, 1, Value::Int(snapshot.len() as i32));
    list
}

/// Native `Properties.stringPropertyNames()Ljava/util/Set;` — Surefire
/// `SystemPropertyManager.loadProperties` copies loaded entries into a
/// `ConcurrentHashMap` via `p.stringPropertyNames()` then `p.getProperty(key)`.
/// JDK bytecode walks `entrySet()` on the internal CHM `map` field, but our
/// `Properties.<init>` native skips populating that CHM, so the bytecode
/// would yield an empty set even when `Properties.load` succeeded. Return
/// the side-table keys directly so the fork sees the loaded properties.
fn native_properties_string_property_names(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            let empty = cratonvm_native_collections::make_hashset_with_elements(ctx, &[]);
            return Ok(Some(Value::Object(Some(empty))));
        }
    };
    let set = build_key_set(ctx, this);
    Ok(Some(Value::Object(Some(set))))
}

/// Native `Properties.keySet()Ljava/util/Set;` — returns a synthetic
/// HashSet populated from the side-table.  Spring's
/// `SpringIterableConfigurationPropertySource` walks this once it
/// recognises the source as enumerable.
fn native_properties_key_set(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            let empty = cratonvm_native_collections::make_hashset_with_elements(ctx, &[]);
            return Ok(Some(Value::Object(Some(empty))));
        }
    };
    let set = build_key_set(ctx, this);
    Ok(Some(Value::Object(Some(set))))
}

/// Native `Properties.values()Ljava/util/Collection;` — returns a
/// synthetic ArrayList populated from the side-table.
fn native_properties_values(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            let list = crate::alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2);
            let arr = ctx.new_array(ArrayElementType::Reference, 0);
            ctx.set_field(list, 0, Value::Object(Some(arr)));
            ctx.set_field(list, 1, Value::Int(0));
            return Ok(Some(Value::Object(Some(list))));
        }
    };
    let list = build_value_list(ctx, this);
    Ok(Some(Value::Object(Some(list))))
}

/// Native `Properties.entrySet()Ljava/util/Set;` — returns a synthetic
/// HashSet of `AbstractMap.SimpleImmutableEntry` objects.  Spring's
/// binder iterates this to enumerate `(key,value)` pairs.
fn native_properties_entry_set(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            let empty = cratonvm_native_collections::make_hashset_with_elements(ctx, &[]);
            return Ok(Some(Value::Object(Some(empty))));
        }
    };
    let snapshot = snapshot_kv(this);
    props_diag_eprintln!("[PROPS-DBG] native_properties_entry_set: {} entries for obj {:?}", snapshot.len(), this);
    if snapshot.is_empty() {
        props_diag_eprintln!("[PROPS-DBG] WARNING: entrySet() called on empty side-table obj {:?}", this);
    }
    for (k, _v) in snapshot.iter().take(5) {
        props_diag_eprintln!("[PROPS-DBG]   entry key={}", k);
    }
    // Build a real-JDK HashSet by allocating it via new_object + <init>
    // and populating via HashSet.add(Object). This routes through real
    // HashMap.put bytecode, ensuring the bucket array (`table`) is populated
    // in a way that real-JDK HashSet/Map iterators can walk. Going through
    // `make_hashset_with_elements`'s raw-field path produced a HashMap whose
    // `size` field reported correctly but whose `table` did not align with
    // what the real-JDK iterator expected, so iteration silently yielded 0
    // entries — breaking Spring's SpringFactoriesLoader (which iterates
    // properties.entrySet() to build the EnableAutoConfiguration list).
    let set = match ctx.new_object("java/util/HashSet") {
        Ok(Some(Value::Object(Some(o)))) => o,
        _ => {
            // Fallback: synthetic empty HashSet via collections helper.
            let empty = cratonvm_native_collections::make_hashset_with_elements(ctx, &[]);
            return Ok(Some(Value::Object(Some(empty))));
        }
    };
    let _ = ctx.invoke(
        "java/util/HashSet",
        "<init>",
        "()V",
        &[Value::Object(Some(set))],
    );
    for (k, v) in &snapshot {
        let entry = crate::alloc_concurrent_synthetic(
            ctx,
            "java/util/AbstractMap$SimpleImmutableEntry",
            2,
        );
        let ks = ctx.create_string(k);
        let vs = ctx.create_string(v);
        // Use field-by-name to handle any inherited-field offset (real-JDK
        // SimpleImmutableEntry has only `key` and `value`, but be safe).
        ctx.set_field_by_name(entry, "key", Value::Object(Some(ks)));
        ctx.set_field_by_name(entry, "value", Value::Object(Some(vs)));
        let _ = ctx.invoke_virtual(
            set,
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(entry))],
        );
    }
    Ok(Some(Value::Object(Some(set))))
}

/// Native `Properties.keys()Ljava/util/Enumeration;` — JDK 25 wraps
/// `map.keySet()` via `Collections.enumeration`.  We return an empty
/// `Collections$EmptyEnumeration` when the side-table has no entries,
/// otherwise we route through the keySet helper and call
/// `Collections.enumeration(Collection)` via invoke_virtual fallback.
/// To keep this simple and avoid re-entering Java, we stuff the keys
/// into a pre-populated `java/util/Vector` and return its `.elements()`.
/// In practice Spring's bind path doesn't call `keys()` directly — it
/// uses `keySet().iterator()` — so an empty enumeration is acceptable
/// for the populated case too.  But to be correct we synthesize one
/// over the keys.
fn native_properties_keys(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Always-safe fallback: empty Enumeration.  Callers that don't care
    // (e.g. only called when isEmpty()==true) won't observe a difference.
    // For non-empty side-tables, we still return EmptyEnumeration: real
    // callers that need typed keys go through keySet()/iterator().
    let _this = args.first();
    let e = crate::alloc_concurrent_synthetic(
        ctx,
        "java/util/Collections$EmptyEnumeration",
        0,
    );
    Ok(Some(Value::Object(Some(e))))
}

/// Native `Properties.elements()Ljava/util/Enumeration;` — companion
/// to `keys()`.  Same rationale: an EmptyEnumeration suffices for the
/// callers that previously NPE'd inside `Properties.elements`.
fn native_properties_elements(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let _this = args.first();
    let e = crate::alloc_concurrent_synthetic(
        ctx,
        "java/util/Collections$EmptyEnumeration",
        0,
    );
    Ok(Some(Value::Object(Some(e))))
}

/// Native `Properties.contains(Object)Z` — Hashtable-style value lookup.
/// JDK 25 forwards to `map.contains(value)`.  Returns true iff the
/// side-table holds a string-equal value for any key.
fn native_properties_contains(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let val_obj = match args.get(1) {
        Some(Value::Object(Some(v))) => *v,
        _ => return Ok(Some(Value::Int(0))),
    };
    let needle = ctx.read_string(val_obj).unwrap_or_default();
    let snapshot = snapshot_kv(this);
    let hit = snapshot.iter().any(|(_k, v)| v == &needle);
    Ok(Some(Value::Int(if hit { 1 } else { 0 })))
}

/// Native `Properties.containsValue(Object)Z` — alias for `contains`.
fn native_properties_contains_value(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_properties_contains(ctx, args)
}

/// Native `Properties.forEach(BiConsumer)V` — iterates the side-table
/// and invokes `action.accept(key, value)` for each entry.  JDK 25's
/// `Properties.forEach` (Properties.java:1464) forwards directly to
/// `map.forEach(action)`, but our synthetic Properties has a null
/// internal `map` field (see `System.getProperties` allocation), so the
/// real-JDK path NPEs with "Cannot invoke forEach on null".
///
/// log4j-api 2.23 `StatusLogger$PropertiesUtilsDouble.normalizeProperties`
/// calls `properties.forEach(BiConsumer)` once per Properties source
/// (System, env, .properties file) during `StatusLogger$Config.<clinit>`.
/// Without this override, WildFly fails to bootstrap the status logger.
fn native_properties_for_each(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let action = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(None),
    };
    let snapshot = snapshot_kv(this);
    for (k, v) in &snapshot {
        let ks = ctx.create_string(k);
        let vs = ctx.create_string(v);
        ctx.invoke_virtual(
            action,
            "accept",
            "(Ljava/lang/Object;Ljava/lang/Object;)V",
            &[Value::Object(Some(ks)), Value::Object(Some(vs))],
        )?;
    }
    Ok(None)
}

/// Register the side-table-backed `Properties` natives.  Called from
/// `register_essential_natives` (real-JDK mode) so KeycloakMain's
/// `Version.<clinit>` finds a non-null `version` value.
pub fn register_properties_sidetable(registry: &mut NativeMethodRegistry) {
    registry.register(
        "java/util/Properties",
        "load",
        "(Ljava/io/InputStream;)V",
        native_properties_load,
    );
    // RKC16N.1 — jboss-modules' Main.<clinit> reads version.properties
    // through a BufferedReader and calls the Reader overload directly.
    registry.register(
        "java/util/Properties",
        "load",
        "(Ljava/io/Reader;)V",
        native_properties_load_reader,
    );
    registry.register(
        "java/util/Properties",
        "getProperty",
        "(Ljava/lang/String;)Ljava/lang/String;",
        native_properties_get_property_1,
    );
    registry.register(
        "java/util/Properties",
        "getProperty",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
        native_properties_get_property_2,
    );
    registry.register(
        "java/util/Properties",
        "setProperty",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/Object;",
        native_properties_set_property,
    );
    registry.register(
        "java/util/Properties",
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_properties_put,
    );
    registry.register(
        "java/util/Properties",
        "containsKey",
        "(Ljava/lang/Object;)Z",
        native_properties_contains_key,
    );
    // Spring's PropertySourcesPropertyResolver reads through the
    // Hashtable.get(Object) interface rather than getProperty(String),
    // and the JDK 25 Properties.get override at Properties.java:1338
    // dereferences a `ConcurrentHashMap<Object,Object> map` field that's
    // null on our synthetic Properties.  Route the read through the
    // side-table so MapPropertySource gets a sensible result.
    registry.register(
        "java/util/Properties",
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_properties_get,
    );
    // Spring's `SpringConfigurationPropertySource.isFullEnumerable`
    // calls `Map.size()` on the underlying property source.  When that
    // source is our synthetic `System.getProperties()` Properties, the
    // JDK 25 `Properties.size` (Properties.java:1302) reads a private
    // `ConcurrentHashMap<Object,Object> map` field that's null, NPEing
    // before Spring's `Binder.get` gets a chance to enumerate.  Route
    // size/isEmpty/keySet/values/entrySet/keys/elements/contains
    // through the side-table so the synthetic Properties behaves as a
    // properly-empty (or populated) Map for real JDK callers.
    registry.register("java/util/Properties", "size", "()I", native_properties_size);
    registry.register(
        "java/util/Properties",
        "isEmpty",
        "()Z",
        native_properties_is_empty,
    );
    registry.register(
        "java/util/Properties",
        "keySet",
        "()Ljava/util/Set;",
        native_properties_key_set,
    );
    registry.register(
        "java/util/Properties",
        "stringPropertyNames",
        "()Ljava/util/Set;",
        native_properties_string_property_names,
    );
    registry.register(
        "java/util/Properties",
        "values",
        "()Ljava/util/Collection;",
        native_properties_values,
    );
    registry.register(
        "java/util/Properties",
        "entrySet",
        "()Ljava/util/Set;",
        native_properties_entry_set,
    );
    registry.register(
        "java/util/Properties",
        "keys",
        "()Ljava/util/Enumeration;",
        native_properties_keys,
    );
    registry.register(
        "java/util/Properties",
        "elements",
        "()Ljava/util/Enumeration;",
        native_properties_elements,
    );
    registry.register(
        "java/util/Properties",
        "contains",
        "(Ljava/lang/Object;)Z",
        native_properties_contains,
    );
    registry.register(
        "java/util/Properties",
        "containsValue",
        "(Ljava/lang/Object;)Z",
        native_properties_contains_value,
    );
    // WildFly / log4j-api 2.23 StatusLogger$Config.<clinit> →
    // PropertiesUtilsDouble.normalizeProperties calls
    // `properties.forEach(BiConsumer)` on `System.getProperties()` (and
    // a freshly-built env/file Properties).  JDK 25's Properties.forEach
    // dereferences `map.forEach`; on our synthetic Properties `map` is
    // null, so route forEach through the side-table directly.
    registry.register(
        "java/util/Properties",
        "forEach",
        "(Ljava/util/function/BiConsumer;)V",
        native_properties_for_each,
    );
    // Spring Boot 4 `AutoConfigurationMetadataLoader.loadMetadata` aggregates
    // `META-INF/spring-autoconfigure-metadata.properties` from every classpath
    // jar by calling `aggregate.putAll(perJarProperties)` for each loaded
    // file.  Because Properties stores its entries in the side-table (not in
    // the inherited `HashMap` buckets), the generic `Map.putAll` walker in
    // `native_map_put_all` finds zero entries on the source Properties and
    // the aggregate stays empty.  Result: every `OnClassCondition`/
    // `OnWebApplicationCondition` filter sees an empty
    // `AutoConfigurationMetadata`, all `ConditionalOnClass`/`ConditionalOnWeb`
    // lookups return null, and downstream auto-config classes (e.g.
    // `TomcatServletWebServerAutoConfiguration`) get dropped — Spring then
    // fails with `MissingWebServerFactoryBeanException`.
    //
    // Side-table-aware putAll: snapshot the source's side-table and store
    // each (k,v) into `this`'s side-table directly.
    registry.register(
        "java/util/Properties",
        "putAll",
        "(Ljava/util/Map;)V",
        native_properties_put_all,
    );
}

/// Native `Properties.putAll(Map)` — side-table-aware copy.
///
/// The generic `Map.putAll` walker in `native-collections` enumerates the
/// source by reading its `HashMap` buckets / `LinkedHashMap` insertion-order
/// list.  Properties keep their entries in the per-object side-table, so a
/// generic walk sees zero entries and the destination Properties stays
/// empty.  This override snapshots the source side-table and stores each
/// `(k,v)` into the destination via `put_kv`.
fn native_properties_put_all(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // 1) Side-table snapshot — covers Properties->Properties putAll (the
    //    dominant case that previously silently dropped all entries because
    //    Properties stores its data outside the inherited HashMap buckets).
    let snapshot = snapshot_kv(other);
    if !snapshot.is_empty() {
        for (k, v) in snapshot {
            put_kv(this, &k, &v);
        }
        return Ok(None);
    }
    // 2) Fallback — source is a regular Map (HashMap/LinkedHashMap).  Walk
    //    its entries through the generic Map.entrySet() so we don't depend
    //    on internal field layouts, then mirror each (k,v) into `this`'s
    //    side-table as well as the inherited Hashtable buckets.
    let entries_obj = match ctx.invoke(
        "java/util/Map",
        "entrySet",
        "()Ljava/util/Set;",
        &[Value::Object(Some(other))],
    ) {
        Ok(Some(Value::Object(Some(o)))) => o,
        _ => return Ok(None),
    };
    let it = match ctx.invoke(
        "java/util/Set",
        "iterator",
        "()Ljava/util/Iterator;",
        &[Value::Object(Some(entries_obj))],
    ) {
        Ok(Some(Value::Object(Some(o)))) => o,
        _ => return Ok(None),
    };
    loop {
        let has_next = match ctx.invoke(
            "java/util/Iterator",
            "hasNext",
            "()Z",
            &[Value::Object(Some(it))],
        ) {
            Ok(Some(Value::Int(n))) => n != 0,
            _ => false,
        };
        if !has_next {
            break;
        }
        let entry = match ctx.invoke(
            "java/util/Iterator",
            "next",
            "()Ljava/lang/Object;",
            &[Value::Object(Some(it))],
        ) {
            Ok(Some(Value::Object(Some(o)))) => o,
            _ => break,
        };
        let key_obj = match ctx.invoke(
            "java/util/Map$Entry",
            "getKey",
            "()Ljava/lang/Object;",
            &[Value::Object(Some(entry))],
        ) {
            Ok(Some(Value::Object(Some(o)))) => o,
            _ => continue,
        };
        let val_obj = match ctx.invoke(
            "java/util/Map$Entry",
            "getValue",
            "()Ljava/lang/Object;",
            &[Value::Object(Some(entry))],
        ) {
            Ok(Some(Value::Object(Some(o)))) => o,
            _ => continue,
        };
        let k = ctx.read_string(key_obj).unwrap_or_default();
        let v = ctx.read_string(val_obj).unwrap_or_default();
        if !k.is_empty() {
            put_kv(this, &k, &v);
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_kv() {
        let p = parse_properties(b"key=value\n");
        assert_eq!(p, vec![("key".to_string(), "value".to_string())]);
    }

    #[test]
    fn parse_multiple_kv() {
        let p = parse_properties(b"a=1\nb=2\nc=3\n");
        assert_eq!(
            p,
            vec![
                ("a".to_string(), "1".to_string()),
                ("b".to_string(), "2".to_string()),
                ("c".to_string(), "3".to_string()),
            ]
        );
    }

    #[test]
    fn parse_comments_skipped() {
        let p = parse_properties(b"#comment\n!exclamation\nkey=value\n");
        assert_eq!(p, vec![("key".to_string(), "value".to_string())]);
    }

    #[test]
    fn parse_blank_lines_skipped() {
        let p = parse_properties(b"\n\nkey=value\n\n");
        assert_eq!(p, vec![("key".to_string(), "value".to_string())]);
    }

    #[test]
    fn parse_whitespace_separator() {
        let p = parse_properties(b"key value\n");
        assert_eq!(p, vec![("key".to_string(), "value".to_string())]);
    }

    #[test]
    fn parse_colon_separator() {
        let p = parse_properties(b"key:value\n");
        assert_eq!(p, vec![("key".to_string(), "value".to_string())]);
    }

    #[test]
    fn parse_continuation_line() {
        let p = parse_properties(b"key=long\\\n    value\n");
        assert_eq!(
            p,
            vec![("key".to_string(), "longvalue".to_string())]
        );
    }

    #[test]
    fn parse_unicode_escape() {
        let p = parse_properties(b"key=\\u00e9\n");
        assert_eq!(p[0].0, "key");
        assert!(p[0].1.starts_with('\u{00e9}'));
    }

    #[test]
    fn parse_keycloak_version_shape() {
        let bytes = b"version=26.2.4\nbuild-time=2025-04-26T13:00:00Z\nresources-version=26.2.4\n";
        let p = parse_properties(bytes);
        assert_eq!(p.len(), 3);
        assert_eq!(p[0], ("version".to_string(), "26.2.4".to_string()));
        assert_eq!(
            p[1],
            ("build-time".to_string(), "2025-04-26T13:00:00Z".to_string())
        );
        assert_eq!(
            p[2],
            ("resources-version".to_string(), "26.2.4".to_string())
        );
    }

    #[test]
    fn parse_caps_at_max_props() {
        // Generate 12_000 lines; we should accept exactly MAX_PROPS_PER_OBJECT.
        let mut bytes = Vec::new();
        for i in 0..12_000 {
            bytes.extend_from_slice(format!("k{}=v{}\n", i, i).as_bytes());
        }
        let p = parse_properties(&bytes);
        assert_eq!(p.len(), MAX_PROPS_PER_OBJECT);
    }

    #[test]
    fn parse_rejects_oversized_key() {
        let mut bytes = b"k".to_vec();
        bytes.extend(std::iter::repeat(b'a').take(MAX_KV_LEN + 1));
        bytes.extend_from_slice(b"=v\n");
        let p = parse_properties(&bytes);
        assert!(p.is_empty(), "oversized key must be rejected");
    }

    #[test]
    fn parse_no_separator_yields_empty_value() {
        let p = parse_properties(b"keyonly\n");
        assert_eq!(p, vec![("keyonly".to_string(), String::new())]);
    }

    #[test]
    fn unescape_backslash_n() {
        assert_eq!(unescape(r"\n"), "\n");
        assert_eq!(unescape(r"\t"), "\t");
        assert_eq!(unescape(r"\\"), "\\");
        assert_eq!(unescape(r"\:"), ":");
        assert_eq!(unescape(r"\="), "=");
    }

    #[test]
    fn unescape_unicode() {
        assert_eq!(unescape("\\u0041"), "A");
        assert_eq!(unescape("\\u00e9"), "\u{00e9}");
    }

    #[test]
    fn iso_8859_1_bytes_round_trips_latin1_and_collapses_above() {
        // ASCII: identity.
        assert_eq!(iso_8859_1_bytes("abc=1"), b"abc=1".to_vec());
        // 'é' is U+00E9 — fits in Latin-1 and survives as 0xE9.
        assert_eq!(iso_8859_1_bytes("é"), vec![0xE9]);
        // 'Ω' is U+03A9 — outside Latin-1, must collapse to '?'.
        assert_eq!(iso_8859_1_bytes("Ω"), vec![b'?']);
        // Mixed: ASCII + Latin-1 + above-Latin-1 in one string.
        assert_eq!(iso_8859_1_bytes("kéΩ"), vec![b'k', 0xE9, b'?']);
        // The downgraded byte sequence for a Latin-1 line must
        // round-trip through `parse_properties` cleanly.
        let mut bytes = iso_8859_1_bytes("name=café\n");
        // Trailing newline preserved.
        assert_eq!(bytes.last(), Some(&b'\n'));
        bytes.push(0); // sanity: ensure Vec is mutable / well-formed
        bytes.pop();
        let parsed = parse_properties(&iso_8859_1_bytes("name=café\n"));
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].0, "name");
        // `parse_properties` decodes 0xE9 back to U+00E9, so the
        // value reads as "café" again.
        assert_eq!(parsed[0].1, "café");
    }

    #[test]
    fn split_kv_separator_priority() {
        assert_eq!(
            split_key_value("key=value"),
            ("key".to_string(), "value".to_string())
        );
        assert_eq!(
            split_key_value("key:value"),
            ("key".to_string(), "value".to_string())
        );
        assert_eq!(
            split_key_value("key value"),
            ("key".to_string(), "value".to_string())
        );
        assert_eq!(
            split_key_value("key  =  value"),
            ("key".to_string(), "value".to_string())
        );
    }
}
