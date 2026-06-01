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
) -> ObjectRef {
    let cid = match ctx.ensure_class_initialized("sun/nio/cs/StreamDecoder") {
        Ok(c) => c,
        Err(_) => cratonvm_types::ClassId::new(0),
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
        },
    );
    obj
}

/// `forInputStreamReader(InputStream, Object, Charset) -> StreamDecoder`.
fn native_sd_for_isr_charset(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let is = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Object(None))),
    };
    let charset_obj = obj_arg(args, 2);
    let name = resolve_name(ctx, charset_obj, args.get(2));
    let sd = alloc_stream_decoder(ctx, is, &name);
    Ok(Some(Value::Object(Some(sd))))
}

/// `forInputStreamReader(InputStream, Object, String) -> StreamDecoder`.
fn native_sd_for_isr_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let is = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Object(None))),
    };
    let name_str = match obj_arg(args, 2) {
        Some(s) => ctx.read_string(s).unwrap_or_else(|| "UTF-8".to_string()),
        None => "UTF-8".to_string(),
    };
    let norm = normalize(&name_str);
    let sd = alloc_stream_decoder(ctx, is, &norm);
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
    cratonvm_native_builtins_normalize(n)
}

/// Thin wrapper so we don't pull in the full `cratonvm-native-builtins`
/// crate from native-io (it would create a cycle). The rule set is
/// intentionally the strict subset our engine understands.
fn cratonvm_native_builtins_normalize(name: &str) -> String {
    match name.to_uppercase().replace(['-', '_'], "").as_str() {
        "UTF8" => "UTF-8".to_string(),
        "UTF16" => "UTF-16".to_string(),
        "UTF16BE" => "UTF-16BE".to_string(),
        "UTF16LE" => "UTF-16LE".to_string(),
        "UTF32" => "UTF-32".to_string(),
        "UTF32BE" => "UTF-32BE".to_string(),
        "UTF32LE" => "UTF-32LE".to_string(),
        "USASCII" | "ASCII" => "US-ASCII".to_string(),
        "ISO88591" | "LATIN1" => "ISO-8859-1".to_string(),
        "ISO88592" => "ISO-8859-2".to_string(),
        "ISO885915" => "ISO-8859-15".to_string(),
        "WINDOWS1252" | "CP1252" => "windows-1252".to_string(),
        "WINDOWS1251" | "CP1251" => "windows-1251".to_string(),
        "KOI8R" => "KOI8-R".to_string(),
        _ => "UTF-8".to_string(),
    }
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
    let (name, mut bytes) = {
        let t = sd_table().lock().unwrap();
        match t.get(&id) {
            Some(s) => (s.name.clone(), s.carry.clone()),
            None => ("UTF-8".to_string(), Vec::new()),
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

    // At true EOF, flush everything (a dangling incomplete sequence decodes to
    // U+FFFD via the lossy decoder); otherwise carry the incomplete tail.
    let split = if eof {
        bytes.len()
    } else {
        split_complete_prefix(&name, &bytes)
    };
    let (decodable, rest) = bytes.split_at(split);
    let chars = engine::decode_bytes_lossy(&name, decodable);
    let ncopy = chars.len().min(len);
    if ncopy > 0 {
        ctx.write_char_array_from(out, off, &chars[..ncopy]);
    }

    // Persist the incomplete trailing bytes for the next call.
    {
        let mut t = sd_table().lock().unwrap();
        let entry = t.entry(id).or_insert_with(|| SdState {
            name: name.clone(),
            carry: Vec::new(),
        });
        entry.carry = rest.to_vec();
    }

    if ncopy == 0 {
        if eof {
            return Ok(-1);
        }
        return Ok(0);
    }
    Ok(ncopy as i32)
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
    registry.register(
        sd,
        "getEncoding",
        "()Ljava/lang/String;",
        |ctx, args| {
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
        },
    );
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
        assert_eq!(
            split_complete_prefix("UTF-32BE", &[0, 0, 0, 0x41, 0, 0]),
            4
        );
    }
}
