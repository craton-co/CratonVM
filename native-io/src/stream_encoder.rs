// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Real-mode shim for `sun.nio.cs.StreamEncoder`.
//!
//! Symmetric to [`crate::stream_decoder`]: wraps an `OutputStream` with
//! a configurable charset, encoding UTF-16 chars from the caller to
//! bytes and forwarding them via the underlying stream's
//! `write([BII)V` method.  The JDK bytecode reaches into sun.nio.ch
//! internals we don't cover; this synthetic implementation keeps its
//! state in three fields:
//!
//! | slot | meaning                                    |
//! |------|--------------------------------------------|
//! | 0    | underlying `java.io.OutputStream`          |
//! | 1    | `java.lang.String` — canonical charset name|
//! | 2    | `int` — `1` if closed, `0` otherwise        |

use cratonvm_native_api::charset as engine;
use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::{ArrayElementType, ClassId, ObjectRef, Value};

const SE_OUTPUT: usize = 0;
const SE_NAME: usize = 1;
const SE_CLOSED: usize = 2;
const SE_NUM_FIELDS: usize = 3;

fn obj_arg(args: &[Value], i: usize) -> Option<ObjectRef> {
    match args.get(i) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    }
}

fn int_arg(args: &[Value], i: usize) -> i32 {
    args.get(i).and_then(|v| v.as_int()).unwrap_or(0)
}

fn normalize(n: &str) -> String {
    normalize_supported(n).unwrap_or_else(|| "UTF-8".to_string())
}

/// Canonicalize a user-supplied charset *name* and confirm the transcoding
/// engine can actually encode it. Returns `None` for an unknown name or for
/// a name that maps to a charset the engine does not implement — the JDK
/// reports both as `UnsupportedEncodingException`.
///
/// Small copy — see `stream_decoder::normalize_supported`.
fn normalize_supported(name: &str) -> Option<String> {
    let canon = match name.to_uppercase().replace(['-', '_'], "").as_str() {
        "UTF8" => "UTF-8",
        "UTF16" => "UTF-16",
        "UTF16BE" => "UTF-16BE",
        "UTF16LE" => "UTF-16LE",
        "UTF32BE" => "UTF-32BE",
        "UTF32LE" => "UTF-32LE",
        "UTF32" => "UTF-32",
        "USASCII" | "ASCII" => "US-ASCII",
        "ISO88591" | "LATIN1" => "ISO-8859-1",
        "ISO88592" => "ISO-8859-2",
        "ISO885915" => "ISO-8859-15",
        "WINDOWS1252" | "CP1252" => "windows-1252",
        "WINDOWS1251" | "CP1251" => "windows-1251",
        "KOI8R" => "KOI8-R",
        _ => return None,
    };
    // Probe with an empty slice: the engine's name `match` returns
    // `UnsupportedCharset` before encoding anything, so this is free.
    if matches!(
        engine::encode_chars(canon, &[]),
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
/// for the offending charset name, falling back to a generic `IOException`
/// (its superclass) if the concrete class cannot be constructed.
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

fn resolve_name(ctx: &dyn NativeContext, charset: Option<ObjectRef>) -> String {
    if let Some(cs) = charset {
        if let Value::Object(Some(s)) = ctx.get_field(cs, 0) {
            if let Some(n) = ctx.read_string(s) {
                let norm = normalize(&n);
                if !norm.is_empty() {
                    return norm;
                }
            }
        }
        if let Some(n) = ctx.read_string(cs) {
            let norm = normalize(&n);
            if !norm.is_empty() {
                return norm;
            }
        }
    }
    "UTF-8".to_string()
}

fn name_of(ctx: &dyn NativeContext, this: ObjectRef) -> String {
    match ctx.get_field(this, SE_NAME) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_else(|| "UTF-8".to_string()),
        _ => "UTF-8".to_string(),
    }
}

pub(crate) fn alloc_stream_encoder(
    ctx: &mut dyn NativeContext,
    os: ObjectRef,
    charset_name: &str,
) -> ObjectRef {
    let cid = match ctx.ensure_class_initialized("sun/nio/cs/StreamEncoder") {
        Ok(c) => c,
        Err(_) => ClassId::new(0),
    };
    let obj = ctx.alloc_object(cid, SE_NUM_FIELDS);
    let name = ctx.create_string(charset_name);
    ctx.set_field(obj, SE_OUTPUT, Value::Object(Some(os)));
    ctx.set_field(obj, SE_NAME, Value::Object(Some(name)));
    ctx.set_field(obj, SE_CLOSED, Value::Int(0));
    obj
}

/// `forOutputStreamWriter(OutputStream, Object, String) -> StreamEncoder`.
///
/// The JDK factory declares `throws UnsupportedEncodingException`; an unknown
/// or unsupported charset *name* must surface that exception rather than
/// silently encoding the stream as UTF-8.
fn native_se_for_osw_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let os = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Object(None))),
    };
    let name = match obj_arg(args, 2) {
        Some(s) => {
            let raw = ctx.read_string(s).unwrap_or_default();
            match normalize_supported(&raw) {
                Some(n) => n,
                None => return Err(throw_unsupported_encoding(ctx, &raw)),
            }
        }
        None => "UTF-8".to_string(),
    };
    let se = alloc_stream_encoder(ctx, os, &name);
    Ok(Some(Value::Object(Some(se))))
}

/// `forOutputStreamWriter(OutputStream, Object, Charset) -> StreamEncoder`.
fn native_se_for_osw_charset(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let os = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Object(None))),
    };
    let charset = obj_arg(args, 2);
    let name = resolve_name(ctx, charset);
    let se = alloc_stream_encoder(ctx, os, &name);
    Ok(Some(Value::Object(Some(se))))
}

/// Encode `chars` with the encoder's charset and forward to the
/// underlying OutputStream via `write([BII)V`.
fn write_bytes(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    chars: &[u16],
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    if chars.is_empty() {
        return Ok(());
    }
    let os = match ctx.get_field(this, SE_OUTPUT) {
        Value::Object(Some(s)) => s,
        _ => return Ok(()),
    };
    let name = name_of(ctx, this);
    let bytes = engine::encode_chars_lossy(&name, chars);
    if bytes.is_empty() {
        return Ok(());
    }
    let buf = ctx.new_array(ArrayElementType::Byte, bytes.len());
    // AUDIT 2026-05-17: bulk write via NativeContext intrinsic.
    ctx.write_byte_array_from(buf, 0, &bytes);
    ctx.invoke_virtual(
        os,
        "write",
        "([BII)V",
        &[
            Value::Object(Some(buf)),
            Value::Int(0),
            Value::Int(bytes.len() as i32),
        ],
    )?;
    Ok(())
}

/// `write(char[], int, int)`.
fn native_se_write_chars(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(None),
    };
    let cbuf = match obj_arg(args, 1) {
        Some(o) => o,
        None => return Ok(None),
    };
    let off = int_arg(args, 2) as usize;
    let len = int_arg(args, 3) as usize;
    let cap = ctx.array_length(cbuf);
    let end = off.saturating_add(len).min(cap);
    let take = end.saturating_sub(off);
    // AUDIT 2026-05-17: bulk read via NativeContext intrinsic.
    let mut chars = vec![0u16; take];
    if take > 0 {
        ctx.read_char_array_into(cbuf, off, &mut chars);
    }
    write_bytes(ctx, this, &chars)?;
    Ok(None)
}

/// `write(int c)`.
fn native_se_write_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(None),
    };
    let c = int_arg(args, 1) as u16;
    write_bytes(ctx, this, &[c])?;
    Ok(None)
}

/// `write(String, int, int)`.
fn native_se_write_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(None),
    };
    let s = match obj_arg(args, 1) {
        Some(o) => ctx.read_string(o).unwrap_or_default(),
        None => return Ok(None),
    };
    let off = int_arg(args, 2) as usize;
    let len = int_arg(args, 3) as usize;
    let chars: Vec<u16> = s.encode_utf16().collect();
    let end = off.saturating_add(len).min(chars.len());
    let slice: Vec<u16> = chars
        .into_iter()
        .skip(off)
        .take(end - off.min(end))
        .collect();
    write_bytes(ctx, this, &slice)?;
    Ok(None)
}

/// `flushBuffer()` / `flush()`.
fn native_se_flush(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(None),
    };
    if let Value::Object(Some(os)) = ctx.get_field(this, SE_OUTPUT) {
        let _ = ctx.invoke_virtual(os, "flush", "()V", &[]);
    }
    Ok(None)
}

/// `close()` / `implClose()`.
fn native_se_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(None),
    };
    if let Value::Object(Some(os)) = ctx.get_field(this, SE_OUTPUT) {
        let _ = ctx.invoke_virtual(os, "flush", "()V", &[]);
        let _ = ctx.invoke_virtual(os, "close", "()V", &[]);
    }
    ctx.set_field(this, SE_OUTPUT, Value::Object(None));
    ctx.set_field(this, SE_CLOSED, Value::Int(1));
    Ok(None)
}

pub fn register_stream_encoder_natives(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    let se = "sun/nio/cs/StreamEncoder";

    registry.register(
        se,
        "forOutputStreamWriter",
        "(Ljava/io/OutputStream;Ljava/lang/Object;Ljava/lang/String;)Lsun/nio/cs/StreamEncoder;",
        native_se_for_osw_name,
    );
    registry.register(
        se,
        "forOutputStreamWriter",
        "(Ljava/io/OutputStream;Ljava/lang/Object;Ljava/nio/charset/Charset;)Lsun/nio/cs/StreamEncoder;",
        native_se_for_osw_charset,
    );
    registry.register(
        se,
        "forOutputStreamWriter",
        "(Ljava/io/OutputStream;Ljava/lang/Object;Ljava/nio/charset/CharsetEncoder;)Lsun/nio/cs/StreamEncoder;",
        native_se_for_osw_charset,
    );

    registry.register(se, "write", "([CII)V", native_se_write_chars);
    registry.register(se, "write", "(I)V", native_se_write_int);
    registry.register(
        se,
        "write",
        "(Ljava/lang/String;II)V",
        native_se_write_string,
    );
    registry.register(se, "flushBuffer", "()V", native_se_flush);
    registry.register(se, "flush", "()V", native_se_flush);
    registry.register(se, "close", "()V", native_se_close);
    registry.register(se, "implClose", "()V", native_se_close);
    registry.register(se, "getEncoding", "()Ljava/lang/String;", |ctx, args| {
        let this = match obj_arg(args, 0) {
            Some(o) => o,
            None => return Ok(Some(Value::Object(None))),
        };
        Ok(Some(ctx.get_field(this, SE_NAME)))
    });
    registry.register(se, "isOpen", "()Z", |ctx, args| {
        let this = match obj_arg(args, 0) {
            Some(o) => o,
            None => return Ok(Some(Value::Int(0))),
        };
        let open = matches!(ctx.get_field(this, SE_OUTPUT), Value::Object(Some(_)));
        Ok(Some(Value::Int(if open { 1 } else { 0 })))
    });
    registry.set_category(__prev_cat);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_supported_accepts_known_aliases() {
        assert_eq!(normalize_supported("UTF8").as_deref(), Some("UTF-8"));
        assert_eq!(normalize_supported("ascii").as_deref(), Some("US-ASCII"));
        assert_eq!(normalize_supported("KOI8-R").as_deref(), Some("KOI8-R"));
    }

    #[test]
    fn normalize_supported_rejects_unknown_name() {
        assert_eq!(normalize_supported("NoSuchCharset-42"), None);
    }

    #[test]
    fn normalize_supported_rejects_real_but_unimplemented_charsets() {
        // Real charset names the encoder engine does not implement must be
        // rejected, not silently encoded as UTF-8.
        assert_eq!(normalize_supported("Shift_JIS"), None);
        assert_eq!(normalize_supported("GBK"), None);
    }

    #[test]
    fn normalize_keeps_utf8_fallback_for_object_path() {
        // The infallible `normalize` (used only on the already-validated
        // Charset-object path) still maps unknowns to UTF-8.
        assert_eq!(normalize("NoSuchCharset-42"), "UTF-8");
    }
}
