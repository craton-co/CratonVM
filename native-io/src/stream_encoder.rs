// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Real-mode shim for `sun.nio.cs.StreamEncoder`.
//!
//! Symmetric to [`crate::stream_decoder`]: wraps an `OutputStream` with
//! a configurable charset, encoding UTF-16 chars from the caller to
//! bytes and forwarding them via the underlying stream's
//! `write([BII)V` method.  The JDK bytecode reaches into sun.nio.ch
//! internals we don't cover; this synthetic implementation keeps its
//! state in three indexed scratch fields:
//!
//! | slot | meaning                                    |
//! |------|--------------------------------------------|
//! | 0    | underlying `java.io.OutputStream`          |
//! | 1    | `java.lang.String` — canonical charset name|
//! | 2    | `int` — `1` if closed, `0` otherwise        |
//!
//! Cross-call surrogate carry uses the REAL `sun.nio.cs.StreamEncoder` fields
//! `haveLeftoverChar` (boolean) and `leftoverChar` (char) by name — the exact
//! mechanism the JDK's own `StreamEncoder` uses to hold an unmatched high
//! surrogate between writes. They must be the real primitive fields (not an
//! indexed scratch slot): the encoder is allocated with the real class id, so
//! low indexed slots alias the real reference-typed fields (`cs`/`encoder`/`bb`)
//! and would not round-trip a primitive `int`. Without this carry, a surrogate
//! pair split across two writes (e.g. a servlet `Writer` writing one char at a
//! time) encodes each half as a lone surrogate and corrupts supplementary-plane
//! text to U+FFFD (Tomcat BUG-TC0622).

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
    // No pending high surrogate yet (real-field carry, cleared explicitly).
    clear_pending(ctx, obj);
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

/// `forOutputStreamWriter(OutputStream, Object, CharsetEncoder) -> StreamEncoder`.
///
/// Resolves the charset name from the encoder's `charset` field and remembers
/// the encoder itself (in the real `encoder` field) so `write_bytes` can honour
/// its configured error actions. The previous registration routed this
/// descriptor through `native_se_for_osw_charset`, which treated the
/// `CharsetEncoder` as a `Charset`, failed to read a name, and silently fell
/// back to UTF-8 — so `OutputStreamWriter(os, charset.newEncoder())` encoded as
/// UTF-8 and never reported unmappable input (Tomcat `TestURLEncoder`, whose
/// `URLEncoder.encode` relies on a REPORT encoder raising
/// `UnmappableCharacterException` → `IOException` → `IllegalArgumentException`).
fn native_se_for_osw_encoder(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let os = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Object(None))),
    };
    let enc = obj_arg(args, 2);
    let name = match enc {
        Some(e) => match ctx.get_field_by_name(e, "charset") {
            Value::Object(Some(cs)) => resolve_name(ctx, Some(cs)),
            _ => "UTF-8".to_string(),
        },
        None => "UTF-8".to_string(),
    };
    let se = alloc_stream_encoder(ctx, os, &name);
    if let Some(e) = enc {
        // Real `encoder` field (distinct from the synthetic SE_OUTPUT/SE_NAME/
        // SE_CLOSED scratch slots): write_bytes reads its error actions.
        ctx.set_field_by_name(se, "encoder", Value::Object(Some(e)));
    }
    Ok(Some(Value::Object(Some(se))))
}

/// True for a UTF-16 high surrogate (the leading unit of a supplementary pair).
fn is_high_surrogate(u: u16) -> bool {
    (0xD800..=0xDBFF).contains(&u)
}

/// Read the carried pending high surrogate, if any, from the real JDK
/// `haveLeftoverChar`/`leftoverChar` fields. Returns `None` unless the flag is
/// set AND the stored unit is a genuine high surrogate (defensive: a stray
/// non-surrogate is never treated as a carry).
fn take_pending(ctx: &dyn NativeContext, this: ObjectRef) -> Option<u16> {
    if ctx.get_field_by_name(this, "haveLeftoverChar").as_int().unwrap_or(0) == 0 {
        return None;
    }
    let u = (ctx.get_field_by_name(this, "leftoverChar").as_int().unwrap_or(0) & 0xFFFF) as u16;
    if is_high_surrogate(u) {
        Some(u)
    } else {
        None
    }
}

/// Stash an unmatched high surrogate for the next call.
fn set_pending(ctx: &dyn NativeContext, this: ObjectRef, hi: u16) {
    ctx.set_field_by_name(this, "leftoverChar", Value::Int(hi as i32));
    ctx.set_field_by_name(this, "haveLeftoverChar", Value::Int(1));
}

/// Clear any pending high surrogate.
fn clear_pending(ctx: &dyn NativeContext, this: ObjectRef) {
    ctx.set_field_by_name(this, "haveLeftoverChar", Value::Int(0));
}

/// True when `coder`'s `field` action (`malformedInputAction` /
/// `unmappableCharacterAction`) is `CodingErrorAction.REPORT`. A real
/// `CharsetEncoder`'s default is REPORT, so an unreadable/absent field is
/// treated as REPORT (strict) — but this is only consulted when an explicit
/// encoder was supplied to the OutputStreamWriter.
fn action_is_report(ctx: &dyn NativeContext, coder: ObjectRef, field: &str) -> bool {
    if let Value::Object(Some(action)) = ctx.get_field_by_name(coder, field) {
        if let Value::Object(Some(s)) = ctx.get_field_by_name(action, "name") {
            return ctx.read_string(s).as_deref() == Some("REPORT");
        }
    }
    true
}

/// Build and throw (as `Err(ExceptionThrown)`) a `java.nio.charset`
/// coding-error exception via its JDK `(int inputLength)` constructor. These
/// extend `CharacterCodingException` → `IOException`, so they propagate up
/// through `OutputStreamWriter.write` exactly like the real StreamEncoder's
/// `cr.throwException()` does.
fn throw_coding_error(
    ctx: &mut dyn NativeContext,
    class: &str,
    length: i32,
) -> cratonvm_types::error::MethodCallFailed {
    if let Ok(Some(Value::Object(Some(exc)))) =
        ctx.new_object_initialized(class, "(I)V", &[Value::Int(length)])
    {
        return cratonvm_types::error::MethodCallFailed::ExceptionThrown(exc);
    }
    cratonvm_types::error::RuntimeError::IOException {
        message: format!("{class}: coding error"),
    }
    .into()
}

/// Encode `chars` for the stream. When the OutputStreamWriter was built from an
/// explicit `CharsetEncoder` (stored in the real `encoder` field) whose error
/// actions are REPORT, encode strictly and throw on malformed/unmappable input
/// — matching `OutputStreamWriter(os, charset.newEncoder())`. Otherwise (the
/// OSW(Charset)/OSW(name) path, which the JDK configures as REPLACE) substitute
/// the charset's replacement byte, preserving the prior lossy behaviour.
fn encode_for_stream(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    name: &str,
    chars: &[u16],
) -> Result<Vec<u8>, cratonvm_types::error::MethodCallFailed> {
    if let Value::Object(Some(enc)) = ctx.get_field_by_name(this, "encoder") {
        let malformed_report = action_is_report(ctx, enc, "malformedInputAction");
        let unmappable_report = action_is_report(ctx, enc, "unmappableCharacterAction");
        if malformed_report || unmappable_report {
            match engine::encode_chars(name, chars) {
                Ok(b) => return Ok(b),
                Err(e) => {
                    let (report, cls) = match e.kind {
                        engine::CodingErrorKind::Unmappable => (
                            unmappable_report,
                            "java/nio/charset/UnmappableCharacterException",
                        ),
                        engine::CodingErrorKind::Malformed => {
                            (malformed_report, "java/nio/charset/MalformedInputException")
                        }
                        // UnsupportedCharset / Incomplete: fall through to lossy.
                        _ => (false, ""),
                    };
                    if report {
                        return Err(throw_coding_error(ctx, cls, e.length.max(1) as i32));
                    }
                    return Ok(engine::encode_chars_lossy(name, chars));
                }
            }
        }
    }
    Ok(engine::encode_chars_lossy(name, chars))
}

/// Encode `chars` with the encoder's charset and forward to the
/// underlying OutputStream via `write([BII)V`.
///
/// Carries an unmatched trailing high surrogate across calls (the real
/// `haveLeftoverChar`/`leftoverChar` fields): any surrogate held from a previous
/// call is prepended, and if the combined run ends on a lone high surrogate it
/// is stashed for the next call instead of being encoded as U+FFFD. This makes a
/// surrogate pair split across two writes encode to its true supplementary code
/// point.
fn write_bytes(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    chars: &[u16],
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    // Combine any carried high surrogate with this chunk.
    let pending = take_pending(ctx, this);
    if chars.is_empty() && pending.is_none() {
        return Ok(());
    }
    let mut combined: Vec<u16> = Vec::with_capacity(chars.len() + 1);
    if let Some(hi) = pending {
        combined.push(hi);
    }
    combined.extend_from_slice(chars);

    // If the combined run ends on a lone high surrogate, hold it back for the
    // next call (its low surrogate may arrive then); encode only the prefix.
    let encode_len = match combined.last() {
        Some(&last) if is_high_surrogate(last) => {
            set_pending(ctx, this, last);
            combined.len() - 1
        }
        _ => {
            clear_pending(ctx, this);
            combined.len()
        }
    };
    if encode_len == 0 {
        return Ok(());
    }
    let to_encode = &combined[..encode_len];

    let os = match ctx.get_field(this, SE_OUTPUT) {
        Value::Object(Some(s)) => s,
        _ => return Ok(()),
    };
    let name = name_of(ctx, this);
    let bytes = encode_for_stream(ctx, this, &name, to_encode)?;
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
    // End of input: any still-unpaired high surrogate can no longer be
    // completed, so flush it now as the replacement char (U+FFFD), matching
    // the JDK's encoder.encode(.., endOfInput=true) + flush at close. Done
    // before the stream is flushed/closed so the bytes actually go out.
    flush_pending_surrogate(ctx, this)?;
    if let Value::Object(Some(os)) = ctx.get_field(this, SE_OUTPUT) {
        let _ = ctx.invoke_virtual(os, "flush", "()V", &[]);
        let _ = ctx.invoke_virtual(os, "close", "()V", &[]);
    }
    ctx.set_field(this, SE_OUTPUT, Value::Object(None));
    ctx.set_field(this, SE_CLOSED, Value::Int(1));
    Ok(None)
}

/// Emit a carried (now unmatched) high surrogate as the replacement char and
/// clear the carry. Used at end-of-input (close), where a lone surrogate is
/// genuinely malformed and must be substituted rather than silently dropped.
fn flush_pending_surrogate(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    let Some(hi) = take_pending(ctx, this) else {
        return Ok(());
    };
    clear_pending(ctx, this);
    let os = match ctx.get_field(this, SE_OUTPUT) {
        Value::Object(Some(s)) => s,
        _ => return Ok(()),
    };
    let name = name_of(ctx, this);
    // Lossy encode of a lone surrogate yields the charset's replacement bytes
    // (U+FFFD → EF BF BD for UTF-8), exactly as HotSpot's REPLACE action does.
    let bytes = engine::encode_chars_lossy(&name, &[hi]);
    if bytes.is_empty() {
        return Ok(());
    }
    let buf = ctx.new_array(ArrayElementType::Byte, bytes.len());
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
        native_se_for_osw_encoder,
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
