// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Real-mode shim for `sun.nio.cs.StreamDecoder`.
//!
//! The JDK class normally wraps a `CharsetDecoder` around an
//! `InputStream` to produce a `Reader`.  The JDK bytecode reaches
//! deep into `sun.nio.ch.*` internals that we don't cover, so we
//! implement the public method surface natively over the underlying
//! `InputStream`.
//!
//! State model (GC-safe — see the comment on the slot constants below):
//! * slot 0 of the SD object holds the underlying `java.io.InputStream`
//!   (a real reference field the collector scans/relocates);
//! * slot 4 holds a stable `int` id (a primitive the collector ignores)
//!   keying a Rust side-table that owns the charset name and the
//!   incomplete-byte carry.
//!
//! Each `read` decodes the complete prefix of (carry + freshly-read bytes)
//! straight into the caller's `char[]` and carries the trailing incomplete
//! byte sequence (UTF-8 lead/continuation bytes, UTF-16 odd dangling byte)
//! to the next call, so multi-byte boundaries are preserved. No decoded
//! read-ahead `char[]` is buffered in an object field (such a field is not
//! in the real StreamDecoder reference map, so the collector would free it
//! mid-stream — the cause of the prior readLine hang).

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::{ArrayElementType, ObjectRef, Value};

use cratonvm_native_api::charset as engine;

// GC-safe state model.
//
// The SD object is allocated with the REAL `sun.nio.cs.StreamDecoder` class id
// (so `InputStreamReader` bytecode dispatches `sd.read(...)` to our natives) but
// only `SD_NUM_FIELDS` slots. The collector scans that object using the *real*
// class's reference map, which does NOT mark our scratch slots as references —
// so an object reference (a decoded `char[]` / carry `byte[]`) stored in one of
// them is NOT rooted and gets collected mid-stream (the readLine hang). Only:
//   * slot 0 — the underlying `InputStream` — is a real reference field (`in`)
//     that the collector scans and relocates, so it is safe to keep there; and
//   * a primitive (`int`) stored in a scratch slot persists (the collector
//     ignores it).
// Therefore the mutable per-decoder state (charset name + the incomplete-byte
// carry) lives in a Rust side-table keyed by a stable `int` id stored in a
// primitive slot. The `InputStream` is re-read from slot 0 on every call (never
// cached in Rust, so GC motion is transparent).
const SD_INPUT: usize = 0; // real `in` field — GC-scanned reference, persists
const SD_ID: usize = 4; // scratch primitive slot — holds the side-table key
const SD_NUM_FIELDS: usize = 7;

struct SdState {
    name: String,
    carry: Vec<u8>,
    /// `Some` when this decoder must implement `sun.util.PropertyResourceBundleCharset`
    /// semantics, used by `PropertyResourceBundle(InputStream)`: decode UTF-8,
    /// but on the first malformed/unmappable byte fall back to ISO-8859-1 for the
    /// rest of the stream (sticky). `None` = a plain charset (decoded by `name`).
    prop: Option<PropState>,
}

/// Mirrors the per-decoder state of the JDK's
/// `sun.util.PropertyResourceBundleCharset$PropertiesFileDecoder`.
#[derive(Clone, Copy)]
pub(crate) struct PropState {
    /// The charset's `strictUTF8` flag. When `true` the JDK reports UTF-8
    /// errors instead of falling back to ISO-8859-1 (only when the system
    /// property `java.util.PropertyResourceBundle.encoding` is set to `UTF-8`).
    strict: bool,
    /// Sticky: set once a UTF-8 error has switched the stream to ISO-8859-1.
    fell_back: bool,
}

fn sd_table() -> &'static std::sync::Mutex<std::collections::HashMap<i32, SdState>> {
    static T: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<i32, SdState>>> =
        std::sync::OnceLock::new();
    T.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

static SD_NEXT_ID: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(1);

fn obj_arg(args: &[Value], i: usize) -> Option<ObjectRef> {
    match args.get(i) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    }
}

fn int_arg(args: &[Value], i: usize) -> i32 {
    args.get(i).and_then(|v| v.as_int()).unwrap_or(0)
}

fn sd_id(ctx: &dyn NativeContext, this: ObjectRef) -> i32 {
    ctx.get_field(this, SD_ID).as_int().unwrap_or(0)
}

/// Allocate and return a synthetic StreamDecoder wrapping `is`.
pub(crate) fn alloc_stream_decoder(
    ctx: &mut dyn NativeContext,
    is: ObjectRef,
    charset_name: &str,
    prop: Option<PropState>,
) -> ObjectRef {
    let cid = match ctx.ensure_class_initialized("sun/nio/cs/StreamDecoder") {
        Ok(c) => c,
        // `ensure_class_initialized` can transiently fail under concurrent
        // class-loading pressure (many test methods hammering readLine()
        // back-to-back, each racing to initialize this same class the first
        // time). Falling back to `ClassId::new(0)` (`java/lang/Object`, zero
        // declared fields) here produced an object whose class is literally
        // `Object` -- every later `sd.read(...)` call then failed with
        // `NoSuchMethodError: java/lang/Object.read([CII)I`, surfacing as an
        // intermittent, non-deterministic failure anywhere a
        // `BufferedReader`/`InputStreamReader` chain happened to construct a
        // fresh decoder at the wrong moment (observed in
        // TestFormAuthenticatorA/B/C's SimpleHttpClient.readLine). Use the
        // documented `ensure_synthetic_class` fallback instead -- it always
        // returns a class that actually declares `SD_NUM_FIELDS` fields, so
        // the object stays usable even on the rare initialization race.
        Err(_) => ctx.ensure_synthetic_class("sun/nio/cs/StreamDecoder", SD_NUM_FIELDS),
    };
    let obj = ctx.alloc_object(cid, SD_NUM_FIELDS);
    let id = SD_NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // slot 0 (`in`) is a real GC-scanned reference; slot 4 is a scratch
    // primitive holding the side-table key. Everything else stays zero-init.
    ctx.set_field(obj, SD_INPUT, Value::Object(Some(is)));
    ctx.set_field(obj, SD_ID, Value::Int(id));
    sd_table().lock().unwrap().insert(
        id,
        SdState {
            name: charset_name.to_string(),
            carry: Vec::new(),
            prop,
        },
    );
    obj
}

/// Detect a `sun.util.PropertyResourceBundleCharset` charset (or its inner
/// `PropertiesFileDecoder`) passed to `forInputStreamReader`.
///
/// `PropertyResourceBundle(InputStream)` builds its reader from this charset's
/// decoder, which decodes UTF-8 but falls back to ISO-8859-1 when the bytes are
/// not valid UTF-8. Our shim resolves only a charset *name* and would otherwise
/// decode the whole stream as strict UTF-8 (mojibake for ISO-8859-1 property
/// files), so we mark the decoder for the two-pass fallback. Returns
/// `Some(PropState)` for that charset/decoder, else `None`.
fn detect_prop_resource_bundle(ctx: &dyn NativeContext, obj: ObjectRef) -> Option<PropState> {
    let cid = ctx.class_id_of_object(obj);
    let cname = ctx.class_name_of_id(cid)?;
    if !cname.contains("PropertyResourceBundleCharset") {
        return None;
    }
    // `obj` is either the charset (Charset overload) or its non-static inner
    // `PropertiesFileDecoder` (CharsetDecoder overload). The decoder's
    // `CharsetDecoder.charset` field points back to the enclosing charset that
    // carries `strictUTF8`; default to non-strict when it can't be read.
    let strict = read_strict_utf8(ctx, obj)
        .or_else(|| match ctx.get_field_by_name(obj, "charset") {
            Value::Object(Some(cs)) => read_strict_utf8(ctx, cs),
            _ => None,
        })
        .unwrap_or(false);
    Some(PropState {
        strict,
        fell_back: false,
    })
}

fn read_strict_utf8(ctx: &dyn NativeContext, charset: ObjectRef) -> Option<bool> {
    ctx.get_field_by_name(charset, "strictUTF8")
        .as_int()
        .map(|v| v != 0)
}

/// `forInputStreamReader(InputStream, Object, Charset) -> StreamDecoder`.
fn native_sd_for_isr_charset(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let is = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Object(None))),
    };
    let charset_obj = obj_arg(args, 2);
    let prop = match charset_obj {
        Some(o) => detect_prop_resource_bundle(ctx, o),
        None => None,
    };
    let name = resolve_name(ctx, charset_obj, args.get(2));
    let sd = alloc_stream_decoder(ctx, is, &name, prop);
    Ok(Some(Value::Object(Some(sd))))
}

/// `forInputStreamReader(InputStream, Object, String) -> StreamDecoder`.
///
/// The JDK factory declares `throws UnsupportedEncodingException`; an
/// unknown or unsupported charset *name* must surface that exception
/// rather than silently decoding the stream as UTF-8.
fn native_sd_for_isr_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let is = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Object(None))),
    };
    let name_str = match obj_arg(args, 2) {
        Some(s) => ctx.read_string(s).unwrap_or_else(|| "UTF-8".to_string()),
        None => "UTF-8".to_string(),
    };
    let norm = match normalize_supported(&name_str) {
        Some(n) => n,
        None => return Err(throw_unsupported_encoding(ctx, &name_str)),
    };
    let sd = alloc_stream_decoder(ctx, is, &norm, None);
    Ok(Some(Value::Object(Some(sd))))
}

/// Resolve a Charset or String argument into a canonical charset name.
fn resolve_name(
    ctx: &dyn NativeContext,
    charset: Option<ObjectRef>,
    raw: Option<&Value>,
) -> String {
    if let Some(cs) = charset {
        // Try as Charset — slot 0 holds the name String.
        if let Value::Object(Some(s)) = ctx.get_field(cs, 0) {
            if let Some(n) = ctx.read_string(s) {
                let norm = normalize(&n);
                if !norm.is_empty() {
                    return norm;
                }
            }
        }
        // Try as raw String.
        if let Some(n) = ctx.read_string(cs) {
            let norm = normalize(&n);
            if !norm.is_empty() {
                return norm;
            }
        }
    }
    // Last-resort: inspect raw argument.
    if let Some(Value::Object(Some(o))) = raw {
        if let Some(n) = ctx.read_string(*o) {
            let norm = normalize(&n);
            if !norm.is_empty() {
                return norm;
            }
        }
    }
    "UTF-8".to_string()
}

fn normalize(n: &str) -> String {
    normalize_supported(n).unwrap_or_else(|| "UTF-8".to_string())
}

/// Canonicalize a user-supplied charset *name* and confirm the transcoding
/// engine can actually decode it. Returns `None` when the name is unknown
/// (no canonical mapping) or maps to a charset the engine does not implement
/// — both of which the JDK reports as `UnsupportedEncodingException`.
///
/// Alias resolution goes through the shared table in
/// `cratonvm_native_api::charset::canonical_charset_name` (native-io must
/// not depend on `cratonvm-native-builtins` — cycle — but the canonical
/// table now lives beside the engine itself). The private copy this function
/// used to carry went stale: it lacked `IBM850` and the multibyte families,
/// so an `InputStreamReader(is, ibm850Charset)` silently *decoded* the
/// stream as UTF-8 (Tomcat `TestDefaultServletEncoding*` fileEnc[ibm850]
/// cases).
fn normalize_supported(name: &str) -> Option<String> {
    let canon = engine::canonical_charset_name(name)?;
    // Probe with an empty slice: the engine's name `match` returns
    // `UnsupportedCharset` before examining any byte, so this is free —
    // and it keeps canonical-but-codecless names (e.g. KOI8-U) rejected.
    if matches!(
        engine::decode_bytes(canon, &[]),
        Err(engine::CodingError {
            kind: engine::CodingErrorKind::UnsupportedCharset,
            ..
        })
    ) {
        return None;
    }
    Some(canon.to_string())
}

/// Build (and request the throw of) a `java.io.UnsupportedEncodingException`
/// carrying the offending charset name. Falls back to a generic `IOException`
/// (its superclass — still catchable as `IOException`) if the concrete class
/// cannot be constructed in the current build.
fn throw_unsupported_encoding(
    ctx: &mut dyn NativeContext,
    name: &str,
) -> cratonvm_types::error::MethodCallFailed {
    let detail = ctx.create_string(name);
    if let Ok(Some(Value::Object(Some(exc)))) = ctx.new_object_initialized(
        "java/io/UnsupportedEncodingException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(detail))],
    ) {
        return cratonvm_types::error::MethodCallFailed::ExceptionThrown(exc);
    }
    cratonvm_types::error::RuntimeError::IOException {
        message: format!("UnsupportedEncodingException: {name}"),
    }
    .into()
}

/// Read one character (int, or -1 at EOF).
fn native_sd_read(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Int(-1))),
    };
    let out = ctx.new_array(ArrayElementType::Char, 1);
    // `decode_into` always consumes ≥1 fresh byte when the carry alone can't
    // form a char, so the loop makes progress and terminates (a full char
    // needs ≤4 bytes; EOF flushes). Bounded for safety.
    for _ in 0..8 {
        let n = decode_into(ctx, this, out, 0, 1)?;
        if n > 0 {
            let ch = match ctx.get_array_element(out, 0) {
                Value::Int(v) => v & 0xFFFF,
                _ => -1,
            };
            return Ok(Some(Value::Int(ch)));
        }
        if n < 0 {
            return Ok(Some(Value::Int(-1)));
        }
        // n == 0: incomplete multi-byte sequence; decode_into pulled more
        // bytes into the carry — retry.
    }
    Ok(Some(Value::Int(-1)))
}

/// `read(char[] cbuf, int off, int len) -> int`.
fn native_sd_read_chars(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Int(-1))),
    };
    let out = match obj_arg(args, 1) {
        Some(o) => o,
        None => return Ok(Some(Value::Int(-1))),
    };
    let off = int_arg(args, 2) as usize;
    let len = int_arg(args, 3) as usize;
    if len == 0 {
        return Ok(Some(Value::Int(0)));
    }
    // May return 0 when only an incomplete multi-byte tail was read; the
    // caller (`BufferedReader.fill`'s `do { } while (n == 0)`) retries, and
    // each call consumes fresh bytes so it converges (or hits EOF → -1).
    let n = decode_into(ctx, this, out, off, len)?;
    Ok(Some(Value::Int(n)))
}

/// `close()` — closes the underlying InputStream and drops side-table state.
fn native_sd_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(None),
    };
    if let Value::Object(Some(is)) = ctx.get_field(this, SD_INPUT) {
        let _ = ctx.invoke_virtual(is, "close", "()V", &[]);
    }
    ctx.set_field(this, SD_INPUT, Value::Object(None));
    let id = sd_id(ctx, this);
    sd_table().lock().unwrap().remove(&id);
    Ok(None)
}

/// `ready() -> boolean`.
fn native_sd_ready(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Int(0))),
    };
    let id = sd_id(ctx, this);
    let has_carry = sd_table()
        .lock()
        .unwrap()
        .get(&id)
        .map(|s| !s.carry.is_empty())
        .unwrap_or(false);
    if has_carry {
        return Ok(Some(Value::Int(1)));
    }
    if let Value::Object(Some(is)) = ctx.get_field(this, SD_INPUT) {
        let r = ctx.invoke_virtual(is, "available", "()I", &[])?;
        if let Some(Value::Int(v)) = r {
            return Ok(Some(Value::Int(if v > 0 { 1 } else { 0 })));
        }
    }
    Ok(Some(Value::Int(0)))
}

/// Read fresh bytes from the underlying stream, decode the complete prefix
/// (prepending any carried incomplete bytes), copy the decoded chars straight
/// into `out[off..]`, and persist the new incomplete tail as the carry.
///
/// Returns the number of chars written, or -1 at EOF with nothing buffered.
/// May return 0 mid-stream when only an incomplete multi-byte tail was read
/// (the caller retries; each call consumes ≥1 fresh byte, so it converges).
///
/// The total bytes considered (`carry + fresh`) is kept ≤ `len`, and chars ≤
/// bytes for every charset, so the decoded chars always fit in `out[off..len]`
/// — no read-ahead char buffer (and thus no GC-collectable scratch field) is
/// needed.
fn decode_into(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    out: ObjectRef,
    off: usize,
    len: usize,
) -> Result<i32, cratonvm_types::error::MethodCallFailed> {
    if len == 0 {
        return Ok(0);
    }
    let id = sd_id(ctx, this);
    let (name, mut bytes, prop) = {
        let t = sd_table().lock().unwrap();
        match t.get(&id) {
            Some(s) => (s.name.clone(), s.carry.clone(), s.prop),
            None => ("UTF-8".to_string(), Vec::new(), None),
        }
    };

    // Keep total bytes ≤ len so decoded chars ≤ len. Force ≥1 fresh byte when
    // the carry alone fills `len` (tiny len) so we always make progress.
    let mut want = len.saturating_sub(bytes.len());
    if want == 0 && !bytes.is_empty() {
        want = 4;
    }
    let mut eof = false;
    if want > 0 {
        if let Value::Object(Some(is)) = ctx.get_field(this, SD_INPUT) {
            let tmp = ctx.new_array(ArrayElementType::Byte, want);
            let r = ctx.invoke_virtual(
                is,
                "read",
                "([BII)I",
                &[
                    Value::Object(Some(tmp)),
                    Value::Int(0),
                    Value::Int(want as i32),
                ],
            )?;
            let n = match r {
                Some(Value::Int(v)) => v,
                _ => -1,
            };
            if n > 0 {
                let start = bytes.len();
                bytes.resize(start + n as usize, 0);
                ctx.read_byte_array_into(tmp, 0, &mut bytes[start..]);
            } else {
                eof = true;
            }
        } else {
            eof = true;
        }
    }

    if bytes.is_empty() {
        return Ok(-1);
    }

    // Decode `bytes` (carried tail + freshly read) into chars, and compute the
    // incomplete trailing bytes to carry to the next call. The
    // PropertyResourceBundleCharset decoder needs its own two-pass path (it may
    // switch the whole stream to ISO-8859-1); every other charset uses the
    // straight prefix-split decode.
    let (chars, rest, new_prop) = match prop {
        Some(p) => decode_prop(&bytes, eof, p),
        None => {
            // At true EOF, flush everything (a dangling incomplete sequence
            // decodes to U+FFFD via the lossy decoder); otherwise carry the
            // incomplete tail.
            let split = if eof {
                bytes.len()
            } else {
                split_complete_prefix(&name, &bytes)
            };
            let (decodable, tail) = bytes.split_at(split);
            (
                engine::decode_bytes_lossy(&name, decodable),
                tail.to_vec(),
                None,
            )
        }
    };
    let ncopy = chars.len().min(len);
    if ncopy > 0 {
        ctx.write_char_array_from(out, off, &chars[..ncopy]);
    }

    // Persist the incomplete trailing bytes (and any updated property-decoder
    // fallback state) for the next call.
    {
        let mut t = sd_table().lock().unwrap();
        let entry = t.entry(id).or_insert_with(|| SdState {
            name: name.clone(),
            carry: Vec::new(),
            prop: None,
        });
        entry.carry = rest;
        if new_prop.is_some() {
            entry.prop = new_prop;
        }
    }

    if ncopy == 0 {
        if eof {
            return Ok(-1);
        }
        return Ok(0);
    }
    Ok(ncopy as i32)
}

/// Decode one refill for a `sun.util.PropertyResourceBundleCharset` decoder.
///
/// Mirrors the JDK `PropertiesFileDecoder.decodeLoop`: try UTF-8, and on the
/// first malformed/unmappable byte reset and decode the entire current buffer
/// (carry + fresh) as ISO-8859-1, sticking with ISO-8859-1 for the rest of the
/// stream. A truncated trailing UTF-8 sequence mid-stream is carried (not an
/// error yet); the same truncation at EOF is malformed → triggers the fallback.
/// When `strict` (the charset's `strictUTF8` flag) the UTF-8 errors are not
/// recovered — there is no fallback.
///
/// Returns `(decoded chars, bytes to carry, updated PropState)`. `bytes` is
/// never empty (the caller returns EOF first). The decoded chars are always ≤
/// `bytes.len()` for both UTF-8 and ISO-8859-1, so they fit the caller's buffer.
fn decode_prop(
    bytes: &[u8],
    eof: bool,
    mut p: PropState,
) -> (Vec<u16>, Vec<u8>, Option<PropState>) {
    // Already fell back: ISO-8859-1 maps every byte 1:1, nothing to carry.
    if p.fell_back {
        return (decode_latin1(bytes), Vec::new(), Some(p));
    }
    match engine::decode_bytes("UTF-8", bytes) {
        // Whole buffer is valid UTF-8 (no truncated tail).
        Ok(chars) => (chars, Vec::new(), Some(p)),
        Err(e) => match e.kind {
            // Truncated trailing multi-byte sequence mid-stream: emit the valid
            // prefix and carry the incomplete tail for the next refill.
            engine::CodingErrorKind::Incomplete if !eof => {
                let split = e.offset; // == valid_up_to()
                let head = engine::decode_bytes_lossy("UTF-8", &bytes[..split]);
                (head, bytes[split..].to_vec(), Some(p))
            }
            // Malformed/unmappable UTF-8 (or a truncated tail at EOF). The JDK
            // strict decoder would report the error; we lossily REPLACE so as
            // not to surface a checked exception from this read path. Otherwise
            // fall back to ISO-8859-1 for the whole buffer, sticky thereafter.
            _ => {
                if p.strict {
                    return (
                        engine::decode_bytes_lossy("UTF-8", bytes),
                        Vec::new(),
                        Some(p),
                    );
                }
                p.fell_back = true;
                (decode_latin1(bytes), Vec::new(), Some(p))
            }
        },
    }
}

/// ISO-8859-1 (Latin-1): every byte maps to the code unit of the same value.
fn decode_latin1(bytes: &[u8]) -> Vec<u16> {
    bytes.iter().map(|&b| b as u16).collect()
}

/// Return the byte index up to which `bytes` forms a complete multi-byte
/// sequence for the named charset. Bytes past this index should be
/// carried to the next refill.
pub(crate) fn split_complete_prefix(name: &str, bytes: &[u8]) -> usize {
    match name {
        "UTF-8" => utf8_complete_prefix(bytes),
        "UTF-16" | "UTF-16BE" | "UTF-16LE" => bytes.len() & !1, // even boundary
        "UTF-32" | "UTF-32BE" | "UTF-32LE" => bytes.len() & !3,
        _ => bytes.len(), // single-byte charsets: every byte is complete
    }
}

/// Find the largest prefix of `bytes` that forms complete UTF-8
/// sequences.  Trailing continuation/lead bytes that haven't been
/// finished get excluded so the caller can buffer them.
fn utf8_complete_prefix(bytes: &[u8]) -> usize {
    // Walk backwards up to 3 bytes looking for a leading byte (bit
    // pattern `11xxxxxx`).  If its required sequence length exceeds
    // what remains, that whole sequence is incomplete.
    for back in 1..=3 {
        if back > bytes.len() {
            break;
        }
        let idx = bytes.len() - back;
        let b = bytes[idx];
        if b & 0b1000_0000 == 0 {
            return bytes.len(); // ASCII; everything complete
        }
        if b & 0b1100_0000 == 0b1100_0000 {
            // This is a leading byte.  Determine required length.
            let required = if b & 0b1110_0000 == 0b1100_0000 {
                2
            } else if b & 0b1111_0000 == 0b1110_0000 {
                3
            } else if b & 0b1111_1000 == 0b1111_0000 {
                4
            } else {
                // Malformed — let the decoder handle it.
                return bytes.len();
            };
            let available = bytes.len() - idx;
            if available >= required {
                return bytes.len();
            } else {
                return idx;
            }
        }
        // Otherwise this is a continuation byte; continue walking back.
    }
    bytes.len()
}

/// Register the StreamDecoder natives on the registry.
pub fn register_stream_decoder_natives(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    let sd = "sun/nio/cs/StreamDecoder";

    // Public factory methods.
    registry.register(
        sd,
        "forInputStreamReader",
        "(Ljava/io/InputStream;Ljava/lang/Object;Ljava/lang/String;)Lsun/nio/cs/StreamDecoder;",
        native_sd_for_isr_name,
    );
    registry.register(
        sd,
        "forInputStreamReader",
        "(Ljava/io/InputStream;Ljava/lang/Object;Ljava/nio/charset/Charset;)Lsun/nio/cs/StreamDecoder;",
        native_sd_for_isr_charset,
    );
    registry.register(
        sd,
        "forInputStreamReader",
        "(Ljava/io/InputStream;Ljava/lang/Object;Ljava/nio/charset/CharsetDecoder;)Lsun/nio/cs/StreamDecoder;",
        native_sd_for_isr_charset,
    );

    // Reader API surface.
    registry.register(sd, "read", "()I", native_sd_read);
    registry.register(sd, "read", "([CII)I", native_sd_read_chars);
    registry.register(sd, "close", "()V", native_sd_close);
    registry.register(sd, "implClose", "()V", native_sd_close);
    registry.register(sd, "ready", "()Z", native_sd_ready);
    registry.register(sd, "isOpen", "()Z", |ctx, args| {
        let this = match obj_arg(args, 0) {
            Some(o) => o,
            None => return Ok(Some(Value::Int(0))),
        };
        let open = matches!(ctx.get_field(this, SD_INPUT), Value::Object(Some(_)));
        Ok(Some(Value::Int(if open { 1 } else { 0 })))
    });
    registry.register(sd, "getEncoding", "()Ljava/lang/String;", |ctx, args| {
        let this = match obj_arg(args, 0) {
            Some(o) => o,
            None => return Ok(Some(Value::Object(None))),
        };
        let id = sd_id(ctx, this);
        let name = sd_table()
            .lock()
            .unwrap()
            .get(&id)
            .map(|s| s.name.clone())
            .unwrap_or_else(|| "UTF-8".to_string());
        let s = ctx.create_string(&name);
        Ok(Some(Value::Object(Some(s))))
    });
    registry.set_category(__prev_cat);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_complete_prefix_ascii() {
        assert_eq!(split_complete_prefix("UTF-8", b"abc"), 3);
    }

    #[test]
    fn utf8_complete_prefix_truncates_partial_2byte() {
        // 0xC2 begins a 2-byte sequence; on its own, incomplete.
        assert_eq!(split_complete_prefix("UTF-8", &[b'a', 0xC2]), 1);
    }

    #[test]
    fn utf8_complete_prefix_truncates_partial_4byte() {
        // 0xF0 0x9F = first two bytes of a 4-byte sequence.
        let data = &[b'a', 0xF0, 0x9F];
        assert_eq!(split_complete_prefix("UTF-8", data), 1);
    }

    #[test]
    fn utf8_complete_prefix_full_multibyte() {
        // Full 3-byte sequence: 0xE4 0xB8 0xAD = 中.
        let data = &[b'a', 0xE4, 0xB8, 0xAD];
        assert_eq!(split_complete_prefix("UTF-8", data), 4);
    }

    #[test]
    fn utf16_split_even() {
        assert_eq!(split_complete_prefix("UTF-16BE", &[0, 0x41, 0, 0x42]), 4);
        assert_eq!(split_complete_prefix("UTF-16BE", &[0, 0x41, 0]), 2);
    }

    #[test]
    fn utf32_split_quad() {
        assert_eq!(split_complete_prefix("UTF-32BE", &[0, 0, 0, 0x41, 0, 0]), 4);
    }

    #[test]
    fn normalize_supported_accepts_known_aliases() {
        assert_eq!(normalize_supported("utf-8").as_deref(), Some("UTF-8"));
        assert_eq!(normalize_supported("Latin1").as_deref(), Some("ISO-8859-1"));
        assert_eq!(
            normalize_supported("Cp1252").as_deref(),
            Some("windows-1252")
        );
    }

    #[test]
    fn normalize_supported_rejects_unknown_name() {
        // Genuinely unknown name → None (caller throws UnsupportedEncodingException)
        // instead of the old silent UTF-8 fallback.
        assert_eq!(normalize_supported("NoSuchCharset-42"), None);
    }

    #[test]
    fn normalize_supported_accepts_engine_supported_charsets() {
        // These were rejected by the old stale private alias table even
        // though the engine decodes them (IBM850 hand-written, the CJK
        // families via encoding_rs) — the shared table accepts them.
        assert_eq!(
            normalize_supported("Shift_JIS").as_deref(),
            Some("Shift_JIS")
        );
        assert_eq!(normalize_supported("EUC-JP").as_deref(), Some("EUC-JP"));
        assert_eq!(normalize_supported("ibm850").as_deref(), Some("IBM850"));
    }

    #[test]
    fn normalize_supported_rejects_canonical_but_codecless_charsets() {
        // KOI8-U canonicalizes in the shared table but the engine has no
        // codec for it — the engine probe must still reject the name.
        assert_eq!(normalize_supported("KOI8-U"), None);
    }

    // --- PropertyResourceBundleCharset decoder (Bug #19A) ---

    fn fresh() -> PropState {
        PropState {
            strict: false,
            fell_back: false,
        }
    }

    #[test]
    fn prop_decoder_falls_back_to_iso_on_invalid_utf8() {
        // "Umlaut: äöü" stored as ISO-8859-1: the umlaut bytes E4 F6 FC are not
        // valid UTF-8, so the JDK falls back to ISO-8859-1 → äöü (not '?').
        let mut bytes = b"Umlaut: ".to_vec();
        bytes.extend_from_slice(&[0xE4, 0xF6, 0xFC]);
        let (chars, rest, p) = decode_prop(&bytes, true, fresh());
        assert_eq!(
            String::from_utf16(&chars).unwrap(),
            "Umlaut: \u{00e4}\u{00f6}\u{00fc}"
        );
        assert!(rest.is_empty());
        assert!(p.unwrap().fell_back, "stream should be sticky-ISO now");
    }

    #[test]
    fn prop_decoder_keeps_valid_utf8() {
        // The same text stored as UTF-8 must stay UTF-8 (no spurious fallback).
        let bytes = "Umlaut: \u{00e4}\u{00f6}\u{00fc}".as_bytes().to_vec();
        let (chars, rest, p) = decode_prop(&bytes, true, fresh());
        assert_eq!(
            String::from_utf16(&chars).unwrap(),
            "Umlaut: \u{00e4}\u{00f6}\u{00fc}"
        );
        assert!(rest.is_empty());
        assert!(!p.unwrap().fell_back);
    }

    #[test]
    fn prop_decoder_carries_truncated_utf8_midstream() {
        // "ab" + the first two bytes of the 3-byte sequence for 中 (E4 B8): a
        // truncated tail mid-stream is carried, NOT treated as an error/fallback.
        let bytes = vec![b'a', b'b', 0xE4, 0xB8];
        let (chars, rest, p) = decode_prop(&bytes, false, fresh());
        assert_eq!(String::from_utf16(&chars).unwrap(), "ab");
        assert_eq!(rest, vec![0xE4, 0xB8]);
        assert!(!p.unwrap().fell_back);
    }

    #[test]
    fn prop_decoder_truncated_utf8_at_eof_falls_back() {
        // The same truncated tail at EOF cannot complete → malformed → ISO.
        let bytes = vec![b'a', b'b', 0xE4, 0xB8];
        let (chars, rest, p) = decode_prop(&bytes, true, fresh());
        // ISO-8859-1: 4 bytes -> 4 chars.
        assert_eq!(chars.len(), 4);
        assert_eq!(chars[0], b'a' as u16);
        assert_eq!(chars[2], 0xE4);
        assert!(rest.is_empty());
        assert!(p.unwrap().fell_back);
    }

    #[test]
    fn prop_decoder_sticky_iso_after_fallback() {
        // Once fallen back, valid-UTF-8 bytes still decode as ISO-8859-1.
        let mut p = fresh();
        p.fell_back = true;
        let bytes = "中".as_bytes().to_vec(); // E4 B8 AD (valid UTF-8)
        let (chars, rest, p2) = decode_prop(&bytes, false, p);
        assert_eq!(chars.len(), 3, "ISO decodes each byte separately");
        assert!(rest.is_empty());
        assert!(p2.unwrap().fell_back);
    }

    #[test]
    fn prop_decoder_strict_does_not_fall_back() {
        // strict=true (system property = UTF-8): invalid UTF-8 is REPLACE-decoded
        // and the decoder stays in UTF-8 mode (no ISO fallback).
        let mut p = fresh();
        p.strict = true;
        let bytes = vec![0xE4, 0xF6, 0xFC];
        let (chars, rest, p2) = decode_prop(&bytes, true, p);
        assert!(
            chars.iter().any(|&c| c == 0xFFFD),
            "REPLACE substitutes U+FFFD"
        );
        assert!(rest.is_empty());
        assert!(!p2.unwrap().fell_back);
    }
}
