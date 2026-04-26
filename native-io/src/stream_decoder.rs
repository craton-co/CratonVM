//! Real-mode shim for `sun.nio.cs.StreamDecoder`.
//!
//! The JDK class normally wraps a `CharsetDecoder` around an
//! `InputStream` to produce a `Reader`.  The JDK bytecode reaches
//! deep into `sun.nio.ch.*` internals that we don't cover, so we
//! expose a synthetic layout and implement the public method surface
//! natively.
//!
//! Synthetic field layout (7 fields):
//! | slot | meaning                                                   |
//! |------|-----------------------------------------------------------|
//! | 0    | underlying `java.io.InputStream`                          |
//! | 1    | `java.lang.String` — canonical charset name               |
//! | 2    | `char[]` — decoded read-ahead buffer (may be null)        |
//! | 3    | `int`    — read-ahead buffer read position                |
//! | 4    | `int`    — read-ahead buffer valid length                 |
//! | 5    | `byte[]` — carryover of incomplete trailing byte sequence |
//! | 6    | `int`    — valid bytes in the carryover buffer            |
//!
//! Multi-byte boundaries are preserved correctly by carrying the
//! trailing incomplete bytes (UTF-8 leading or continuation bytes,
//! UTF-16 odd dangling byte) between calls.

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::MethodCallResult;
use rustjvm_types::{ArrayElementType, ObjectRef, Value};

use rustjvm_native_api::charset as engine;

/// Slot indices.
const SD_INPUT: usize = 0;
const SD_NAME: usize = 1;
const SD_CHARS: usize = 2;
const SD_POS: usize = 3;
const SD_LEN: usize = 4;
const SD_CARRY: usize = 5;
const SD_CARRY_LEN: usize = 6;

const SD_NUM_FIELDS: usize = 7;

/// Maximum bytes to pull from the underlying stream per refill.
const REFILL_BYTES: usize = 4096;

fn obj_arg(args: &[Value], i: usize) -> Option<ObjectRef> {
    match args.get(i) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    }
}

fn int_arg(args: &[Value], i: usize) -> i32 {
    args.get(i).and_then(|v| v.as_int()).unwrap_or(0)
}

/// Read the canonical charset name from slot 1.
fn name_of(ctx: &dyn NativeContext, this: ObjectRef) -> String {
    match ctx.get_field(this, SD_NAME) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_else(|| "UTF-8".to_string()),
        _ => "UTF-8".to_string(),
    }
}

/// Allocate and return a synthetic StreamDecoder wrapping `is`.
pub(crate) fn alloc_stream_decoder(
    ctx: &mut dyn NativeContext,
    is: ObjectRef,
    charset_name: &str,
) -> ObjectRef {
    let cid = match ctx.ensure_class_initialized("sun/nio/cs/StreamDecoder") {
        Ok(c) => c,
        Err(_) => rustjvm_types::ClassId::new(0),
    };
    let obj = ctx.alloc_object(cid, SD_NUM_FIELDS);
    let name = ctx.create_string(charset_name);
    ctx.set_field(obj, SD_INPUT, Value::Object(Some(is)));
    ctx.set_field(obj, SD_NAME, Value::Object(Some(name)));
    ctx.set_field(obj, SD_CHARS, Value::Object(None));
    ctx.set_field(obj, SD_POS, Value::Int(0));
    ctx.set_field(obj, SD_LEN, Value::Int(0));
    ctx.set_field(obj, SD_CARRY, Value::Object(None));
    ctx.set_field(obj, SD_CARRY_LEN, Value::Int(0));
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
    rustjvm_native_builtins_normalize(n)
}

/// Thin wrapper so we don't pull in the full `rustjvm-native-builtins`
/// crate from native-io (it would create a cycle). The rule set is
/// intentionally the strict subset our engine understands.
fn rustjvm_native_builtins_normalize(name: &str) -> String {
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
    // Allocate a 1-element output char[] and delegate to read([CII).
    let out = ctx.new_array(ArrayElementType::Char, 1);
    let n = refill_and_copy(ctx, this, out, 0, 1)?;
    if n <= 0 {
        return Ok(Some(Value::Int(-1)));
    }
    let ch = match ctx.get_array_element(out, 0) {
        Value::Int(v) => v & 0xFFFF,
        _ => -1,
    };
    Ok(Some(Value::Int(ch)))
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
    let n = refill_and_copy(ctx, this, out, off, len)?;
    Ok(Some(Value::Int(n)))
}

/// `close()` — closes the underlying InputStream.
fn native_sd_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(None),
    };
    if let Value::Object(Some(is)) = ctx.get_field(this, SD_INPUT) {
        let _ = ctx.invoke_virtual(is, "close", "()V", &[]);
    }
    ctx.set_field(this, SD_INPUT, Value::Object(None));
    Ok(None)
}

/// `ready() -> boolean`.
fn native_sd_ready(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Int(0))),
    };
    let valid = ctx.get_field(this, SD_LEN).as_int().unwrap_or(0)
        - ctx.get_field(this, SD_POS).as_int().unwrap_or(0);
    if valid > 0 {
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

/// Fill the read-ahead char buffer if needed and copy up to `len`
/// chars into `out[off..off+len]`. Returns number of chars copied, or
/// -1 at EOF.
fn refill_and_copy(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    out: ObjectRef,
    off: usize,
    len: usize,
) -> Result<i32, rustjvm_types::error::MethodCallFailed> {
    let mut produced = 0usize;
    while produced < len {
        let pos = ctx.get_field(this, SD_POS).as_int().unwrap_or(0) as usize;
        let limit = ctx.get_field(this, SD_LEN).as_int().unwrap_or(0) as usize;
        if pos < limit {
            let buf = match ctx.get_field(this, SD_CHARS) {
                Value::Object(Some(a)) => a,
                _ => break,
            };
            let take = (limit - pos).min(len - produced);
            for i in 0..take {
                let c = ctx.get_array_element(buf, pos + i);
                ctx.set_array_element(out, off + produced + i, c);
            }
            ctx.set_field(this, SD_POS, Value::Int((pos + take) as i32));
            produced += take;
            continue;
        }

        // Buffer empty: refill from underlying stream.
        let refilled = refill(ctx, this)?;
        if refilled == 0 {
            // EOF
            break;
        }
    }
    if produced == 0 {
        // Either requested 0, or EOF on empty buffer. Return -1 only
        // on EOF per `Reader.read` contract.
        let pos = ctx.get_field(this, SD_POS).as_int().unwrap_or(0);
        let limit = ctx.get_field(this, SD_LEN).as_int().unwrap_or(0);
        if pos >= limit {
            return Ok(-1);
        }
    }
    Ok(produced as i32)
}

/// Pull bytes from the underlying InputStream and decode into the
/// read-ahead buffer. Returns the number of chars produced (0 at EOF).
fn refill(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Result<usize, rustjvm_types::error::MethodCallFailed> {
    let is = match ctx.get_field(this, SD_INPUT) {
        Value::Object(Some(s)) => s,
        _ => return Ok(0),
    };

    // Read REFILL_BYTES from the InputStream into a temporary byte[].
    let tmp = ctx.new_array(ArrayElementType::Byte, REFILL_BYTES);
    let n = ctx.invoke_virtual(
        is,
        "read",
        "([BII)I",
        &[
            Value::Object(Some(tmp)),
            Value::Int(0),
            Value::Int(REFILL_BYTES as i32),
        ],
    )?;
    let n = match n {
        Some(Value::Int(v)) => v,
        _ => -1,
    };

    // Copy read bytes into a Rust Vec<u8>, prepending any carryover.
    let mut bytes: Vec<u8> = Vec::new();
    let carry_len = ctx.get_field(this, SD_CARRY_LEN).as_int().unwrap_or(0) as usize;
    if carry_len > 0 {
        if let Value::Object(Some(carr)) = ctx.get_field(this, SD_CARRY) {
            for i in 0..carry_len {
                if let Value::Int(b) = ctx.get_array_element(carr, i) {
                    bytes.push((b & 0xFF) as u8);
                }
            }
        }
    }

    if n > 0 {
        for i in 0..(n as usize) {
            if let Value::Int(b) = ctx.get_array_element(tmp, i) {
                bytes.push((b & 0xFF) as u8);
            }
        }
    }

    if bytes.is_empty() {
        // EOF with no carryover.
        return Ok(0);
    }

    // Split off incomplete trailing bytes for the next call. At true
    // EOF (`n < 0`) we flush everything, so a stale incomplete
    // sequence emerges as replacement chars.
    let name = name_of(ctx, this);
    let split = if n < 0 {
        bytes.len()
    } else {
        split_complete_prefix(&name, &bytes)
    };
    let (decodable, carry) = bytes.split_at(split);

    // Decode with lossy fallback so arbitrary bad bytes turn into
    // U+FFFD instead of erroring out mid-stream.
    let chars = engine::decode_bytes_lossy(&name, decodable);

    // Stash chars into the read-ahead buffer.
    let char_arr = ctx.new_array(ArrayElementType::Char, chars.len());
    for (i, &c) in chars.iter().enumerate() {
        ctx.set_array_element(char_arr, i, Value::Int(c as i32));
    }
    ctx.set_field(this, SD_CHARS, Value::Object(Some(char_arr)));
    ctx.set_field(this, SD_POS, Value::Int(0));
    ctx.set_field(this, SD_LEN, Value::Int(chars.len() as i32));

    // Save carryover bytes (if any) for the next refill.
    if !carry.is_empty() {
        let carr = ctx.new_array(ArrayElementType::Byte, carry.len());
        for (i, &b) in carry.iter().enumerate() {
            ctx.set_array_element(carr, i, Value::Int(b as i8 as i32));
        }
        ctx.set_field(this, SD_CARRY, Value::Object(Some(carr)));
        ctx.set_field(this, SD_CARRY_LEN, Value::Int(carry.len() as i32));
    } else {
        ctx.set_field(this, SD_CARRY, Value::Object(None));
        ctx.set_field(this, SD_CARRY_LEN, Value::Int(0));
    }

    Ok(chars.len())
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
            let name_val = ctx.get_field(this, SD_NAME);
            Ok(Some(name_val))
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
