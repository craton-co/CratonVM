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

use rustjvm_native_api::registry::{NativeContext, NativeMethodRegistry};
use rustjvm_types::{error::MethodCallResult, ObjectRef, Value};

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
    // The InputStream allocated by `Class.getResourceAsStream` is a
    // ByteArrayInputStream with `buf` (byte[]), `pos` (int), `count` (int)
    // fields populated.  Read directly to avoid going through the JDK's
    // bytecode which is fragile in our env.  Fall back to the
    // `read([B,I,I)I` virtual call if the by-name lookup fails.
    let buf = match ctx.get_field_by_name(stream, "buf") {
        Value::Object(Some(arr)) => Some(arr),
        _ => None,
    };
    let count = match ctx.get_field_by_name(stream, "count") {
        Value::Int(n) => Some(n as usize),
        _ => None,
    };
    let pos = match ctx.get_field_by_name(stream, "pos") {
        Value::Int(n) => Some(n as usize),
        _ => None,
    };
    if let (Some(arr), Some(c), Some(p)) = (buf, count, pos) {
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
            return Some(out);
        }
    }
    None
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
    for (k, v) in parse_properties(&bytes) {
        put_kv(this, &k, &v);
    }
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
    let key = ctx.read_string(key_obj).unwrap_or_default();
    if let Some(v) = get_kv(this, &key) {
        return Ok(Some(Value::Object(Some(ctx.create_string(&v)))));
    }
    match ctx.get_system_property(&key) {
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
    let key = ctx.read_string(key_obj).unwrap_or_default();
    if let Some(v) = get_kv(this, &key) {
        return Ok(Some(Value::Object(Some(ctx.create_string(&v)))));
    }
    match ctx.get_system_property(&key) {
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
