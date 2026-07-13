// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Real transcoding implementations for `java.nio.charset.*`.
//!
//! The pure-Rust transcoding engine lives in
//! `cratonvm_native_api::charset`; this module adapts it to the
//! synthetic layouts used by `Charset` (1 field: name), `CharsetEncoder`
//! / `CharsetDecoder` (3 fields: charset, avg, max), `ByteBuffer` /
//! `CharBuffer` (5 fields: array, pos, limit, capacity, mark) and
//! `CoderResult` (1 field: tag).
//!
//! The module intentionally *does not* re-register `Charset.forName` /
//! `Charset.newEncoder` / `Charset.newDecoder` — those are already
//! installed by `register_charset_natives` in `lib.rs` and
//! `register_p58_charset_coder` in `phases_late.rs`. Instead it
//! replaces the previously-stubbed `encode` / `decode` methods (which
//! returned `UNDERFLOW` without transcoding any bytes) with real ones.

use cratonvm_native_api::charset as engine;
use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ArrayElementType, ObjectRef, Value};

use crate::{alloc_concurrent_synthetic, normalize_charset_name, CHARSET_FIELD_NAME};

/// Layout constants for the synthetic ByteBuffer / CharBuffer.  Keep in
/// sync with `native-io/src/lib.rs::BB_FIELD_*` — the two modules both
/// operate on the same heap objects.
const BUF_FIELD_ARRAY: usize = 0;
const BUF_FIELD_POS: usize = 1;
const BUF_FIELD_LIMIT: usize = 2;
const BUF_FIELD_CAPACITY: usize = 3;
const BUF_FIELD_MARK: usize = 4;
const BUF_NUM_FIELDS: usize = 5;

/// `CoderResult` tag values used by both synthetic native methods and
/// the existing stubs.  0 = UNDERFLOW (success), 1 = OVERFLOW,
/// 2 = MALFORMED (with length), 3 = UNMAPPABLE (with length).
const CR_UNDERFLOW: i32 = 0;
const CR_OVERFLOW: i32 = 1;
const CR_MALFORMED: i32 = 2;
const CR_UNMAPPABLE: i32 = 3;

/// Read the UTF-16 code units from a Java `String` object.  Returns the
/// empty vec if the object isn't a String.
pub(crate) fn read_string_utf16(ctx: &dyn NativeContext, s: ObjectRef) -> Vec<u16> {
    match ctx.read_string(s) {
        Some(t) => t.encode_utf16().collect(),
        None => Vec::new(),
    }
}

/// Read a Rust `String` from a Java `String` slot of an object.  Used
/// to pull the charset name out of a synthetic Charset.
pub(crate) fn read_string_field(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    field: usize,
) -> Option<String> {
    match ctx.get_field(obj, field) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

/// Extract the canonical charset name from either a Charset object or a
/// String ("UTF-8").  Falls back to UTF-8 on unrecognised input, since
/// the synthetic layer already rejected invalid names at `forName` /
/// `isSupported` time.
pub(crate) fn charset_name_of(ctx: &dyn NativeContext, value: Value) -> String {
    match value {
        Value::Object(Some(o)) => {
            // Try as Charset (slot 0 = name String).
            if let Some(name) = read_string_field(ctx, o, CHARSET_FIELD_NAME) {
                let norm = normalize_charset_name(&name);
                if !norm.is_empty() {
                    return norm;
                }
                return name;
            }
            // Fall through: maybe the argument is the String itself.
            if let Some(name) = ctx.read_string(o) {
                let norm = normalize_charset_name(&name);
                if !norm.is_empty() {
                    return norm;
                }
                return name;
            }
            "UTF-8".to_string()
        }
        _ => "UTF-8".to_string(),
    }
}

/// Read a `[B` array from the heap into a Rust `Vec<u8>` between
/// `[off, off+len)`.
fn read_byte_array(ctx: &dyn NativeContext, arr: ObjectRef, off: usize, len: usize) -> Vec<u8> {
    let cap = ctx.array_length(arr);
    let end = off.saturating_add(len).min(cap);
    let start = off.min(end);
    let mut out = Vec::with_capacity(end - start);
    for i in start..end {
        match ctx.get_array_element(arr, i) {
            Value::Int(v) => out.push((v & 0xFF) as u8),
            _ => out.push(0),
        }
    }
    out
}

/// Read a `[C` array from the heap into a Rust `Vec<u16>`.
fn read_char_array(ctx: &dyn NativeContext, arr: ObjectRef, off: usize, len: usize) -> Vec<u16> {
    let cap = ctx.array_length(arr);
    let end = off.saturating_add(len).min(cap);
    let start = off.min(end);
    let mut out = Vec::with_capacity(end - start);
    for i in start..end {
        match ctx.get_array_element(arr, i) {
            Value::Int(v) => out.push((v & 0xFFFF) as u16),
            _ => out.push(0),
        }
    }
    out
}

/// Write bytes into a `[B` array starting at `off`.
fn write_byte_array(ctx: &dyn NativeContext, arr: ObjectRef, off: usize, bytes: &[u8]) -> usize {
    let cap = ctx.array_length(arr);
    let mut written = 0;
    for (i, &b) in bytes.iter().enumerate() {
        if off + i >= cap {
            break;
        }
        ctx.set_array_element(arr, off + i, Value::Int(b as i8 as i32));
        written += 1;
    }
    written
}

/// Write UTF-16 units into a `[C` array starting at `off`.
fn write_char_array(ctx: &dyn NativeContext, arr: ObjectRef, off: usize, chars: &[u16]) -> usize {
    let cap = ctx.array_length(arr);
    let mut written = 0;
    for (i, &c) in chars.iter().enumerate() {
        if off + i >= cap {
            break;
        }
        ctx.set_array_element(arr, off + i, Value::Int(c as i32));
        written += 1;
    }
    written
}

/// Allocate a fresh ByteBuffer (synthetic 5-field layout) wrapping a
/// newly-allocated byte[] containing `bytes`.
pub(crate) fn alloc_byte_buffer(ctx: &mut dyn NativeContext, bytes: &[u8]) -> ObjectRef {
    let cap = bytes.len();
    // Allocate the CONCRETE `HeapByteBuffer`, not the abstract `ByteBuffer`:
    // the abstract base leaves `isDirect()`/`isReadOnly()`/`base()` unbound
    // (AbstractMethodError "has no Code attribute"), which trips real-JDK
    // consumers that call them — e.g. `sun.security.util.PBEUtil.encodePassword`
    // reads `isReadOnly()` on the `CharsetEncoder.encode(...)` result. The
    // named-field writes below match HeapByteBuffer's real layout.
    let obj = alloc_concurrent_synthetic(ctx, "java/nio/HeapByteBuffer", BUF_NUM_FIELDS);
    let arr = ctx.new_array(ArrayElementType::Byte, cap);
    for (i, &b) in bytes.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
    }
    // Real-JDK field names (Buffer + ByteBuffer). Without these, when the
    // concrete `java/nio/ByteBuffer` class is loaded from the JDK image its
    // `hasArray()` / `array()` bytecode reads the named `hb` / `isReadOnly`
    // fields — which stayed null/0 — so `hasArray()` returned false and
    // `array()` threw UnsupportedOperationException. That broke Tomcat
    // `MessageBytes.toBytes` (`encoder.encode(cb).array()`), failing 288
    // MessageBytes-conversion tests. Mirrors the same fix already applied to
    // `alloc_char_buffer` above.
    ctx.set_field_by_name(obj, "hb", Value::Object(Some(arr)));
    ctx.set_field_by_name(obj, "offset", Value::Int(0));
    ctx.set_field_by_name(obj, "isReadOnly", Value::Int(0));
    ctx.set_field_by_name(obj, "position", Value::Int(0));
    ctx.set_field_by_name(obj, "limit", Value::Int(cap as i32));
    ctx.set_field_by_name(obj, "capacity", Value::Int(cap as i32));
    ctx.set_field_by_name(obj, "mark", Value::Int(-1));
    // Indexed fallback for synthetic-mode consumers.
    ctx.set_field(obj, BUF_FIELD_ARRAY, Value::Object(Some(arr)));
    ctx.set_field(obj, BUF_FIELD_POS, Value::Int(0));
    ctx.set_field(obj, BUF_FIELD_LIMIT, Value::Int(cap as i32));
    ctx.set_field(obj, BUF_FIELD_CAPACITY, Value::Int(cap as i32));
    ctx.set_field(obj, BUF_FIELD_MARK, Value::Int(-1));
    // `java.nio.Buffer.address` (long). A real `HeapByteBuffer` sets it to
    // `ARRAY_BYTE_BASE_OFFSET + offset` (= 16 + 0). Without it the inherited
    // bulk-get bytecode (`ByteBuffer.get(byte[])` → `getArray` →
    // `ScopedMemoryAccess.copyMemory`) computes a source offset of
    // `address(0) + position(0) = 0`, below `arrayBaseOffset` (16), which fails
    // the `Unsafe.copyMemory` array-offset decode → AIOOBE. Surfaced by
    // `CharsetEncoder.encode(...).get(byte[])` in `sun.security.util.PBEUtil`
    // (real SunJCE PBKDF2). 16 == `Unsafe.arrayBaseOffset(byte[])` here. Written
    // LAST, by name, so the indexed BUF_FIELD_* writes above (which can alias the
    // real `address` slot) can't clobber it.
    ctx.set_field_by_name(obj, "address", Value::Long(16));
    obj
}

/// Allocate a fresh CharBuffer containing `chars`.
///
/// Round 58 — write the (hb, position, limit, capacity, mark) tuple via
/// `set_field_by_name` so the layout matches real-JDK CharBuffer's actual
/// field names (Buffer.mark/position/limit/capacity + CharBuffer.hb).
/// The previous indexed-slot writes (0..=4) collided with real-JDK
/// Buffer's int-field block when `alloc_concurrent_synthetic` widened
/// the field count from 5 to the loaded class's real total, surfacing
/// later as "CharBuffer has no backing array" inside Tomcat's
/// `CharsetUtil.isAsciiSuperset`. We still also write the indexed
/// fallback slots so any caller assuming the synthetic 5-field overlay
/// (e.g. older `register_p62_char_buffer` natives compiled only in
/// synthetic mode) continues to see consistent state.
pub(crate) fn alloc_char_buffer(ctx: &mut dyn NativeContext, chars: &[u16]) -> ObjectRef {
    let cap = chars.len();
    // Concrete `HeapCharBuffer` (not abstract `CharBuffer`) — see alloc_byte_buffer.
    let obj = alloc_concurrent_synthetic(ctx, "java/nio/HeapCharBuffer", BUF_NUM_FIELDS);
    let arr = ctx.new_array(ArrayElementType::Char, cap);
    for (i, &c) in chars.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(c as i32));
    }
    // Real-JDK field names (Buffer + CharBuffer):
    ctx.set_field_by_name(obj, "hb", Value::Object(Some(arr)));
    ctx.set_field_by_name(obj, "offset", Value::Int(0));
    ctx.set_field_by_name(obj, "position", Value::Int(0));
    ctx.set_field_by_name(obj, "limit", Value::Int(cap as i32));
    ctx.set_field_by_name(obj, "capacity", Value::Int(cap as i32));
    ctx.set_field_by_name(obj, "mark", Value::Int(-1));
    // Indexed fallback for synthetic-mode consumers.
    ctx.set_field(obj, BUF_FIELD_ARRAY, Value::Object(Some(arr)));
    ctx.set_field(obj, BUF_FIELD_POS, Value::Int(0));
    ctx.set_field(obj, BUF_FIELD_LIMIT, Value::Int(cap as i32));
    ctx.set_field(obj, BUF_FIELD_CAPACITY, Value::Int(cap as i32));
    ctx.set_field(obj, BUF_FIELD_MARK, Value::Int(-1));
    // `java.nio.Buffer.address` — see `alloc_byte_buffer`. A real
    // `HeapCharBuffer` sets `ARRAY_CHAR_BASE_OFFSET + (offset<<1)` (= 16 + 0);
    // without it the inherited bulk-get bytecode strides off-grid. Written last.
    ctx.set_field_by_name(obj, "address", Value::Long(16));
    // ByteBuffer default order is BIG_ENDIAN (`bigEndian=true`); a 0 default
    // would make multi-byte views little-endian. Harmless for byte get/array
    // but kept correct for `asCharBuffer`/`getInt` consumers.
    ctx.set_field_by_name(obj, "bigEndian", Value::Int(1));
    obj
}

/// Allocate a CoderResult with the given tag.
fn alloc_coder_result(ctx: &mut dyn NativeContext, tag: i32) -> ObjectRef {
    let cr = alloc_concurrent_synthetic(ctx, "java/nio/charset/CoderResult", 1);
    ctx.set_field(cr, 0, Value::Int(tag));
    cr
}

/// Read the `(array, pos, limit)` triple from a Buffer-shaped object.
///
/// Prefer the real-JDK named fields (`hb`/`position`/`limit`). When the
/// concrete `java/nio/{Char,Byte}Buffer` class is loaded from the JDK image
/// (which it is in the default real-JDK build), the synthetic indexed slots
/// 0/1/2 no longer line up with `hb`/pos/limit — slot 0 is some inherited
/// `Buffer` int field, so the indexed read returns `None`/0 and
/// `CharsetEncoder.encode(CharBuffer.wrap(...))` produced an EMPTY ByteBuffer
/// (Tomcat `MessageBytes.toBytes` → wrong bytes / 288 test failures). The
/// named read matches what `cb_write_hb` / `alloc_*_buffer` actually populate.
/// Indexed slots are kept as a fallback for synthetic-mode-only consumers.
fn buf_state(ctx: &dyn NativeContext, this: ObjectRef) -> Option<(ObjectRef, i32, i32)> {
    if let Value::Object(Some(a)) = ctx.get_field_by_name(this, "hb") {
        let pos = ctx
            .get_field_by_name(this, "position")
            .as_int()
            .unwrap_or(0);
        let lim = ctx.get_field_by_name(this, "limit").as_int().unwrap_or(0);
        return Some((a, pos, lim));
    }
    let arr = match ctx.get_field(this, BUF_FIELD_ARRAY) {
        Value::Object(Some(a)) => a,
        _ => return None,
    };
    let pos = ctx.get_field(this, BUF_FIELD_POS).as_int().unwrap_or(0);
    let lim = ctx.get_field(this, BUF_FIELD_LIMIT).as_int().unwrap_or(0);
    Some((arr, pos, lim))
}

fn set_pos(ctx: &dyn NativeContext, obj: ObjectRef, pos: i32) {
    // Update both the real-JDK named `position` field (read back by buffer
    // bytecode) and the synthetic indexed slot, mirroring `buf_state`'s
    // dual-layout read.
    ctx.set_field_by_name(obj, "position", Value::Int(pos));
    ctx.set_field(obj, BUF_FIELD_POS, Value::Int(pos));
}

// ---------------------------------------------------------------------------
// Public-facing transcoding helpers (used outside this module, e.g. by
// String.getBytes(Charset) and the Stream{Decoder,Encoder} shims).
// ---------------------------------------------------------------------------

/// Decode `bytes` using the charset stored on the `charset` object.
/// Falls back to UTF-8 on unknown names; invalid input is replaced with
/// U+FFFD, matching the default REPLACE action of `CharsetDecoder`.
pub fn decode_with_charset(ctx: &dyn NativeContext, charset: ObjectRef, bytes: &[u8]) -> Vec<u16> {
    let name = read_string_field(ctx, charset, CHARSET_FIELD_NAME)
        .map(|n| normalize_charset_name(&n))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "UTF-8".to_string());
    engine::decode_bytes_lossy(&name, bytes)
}

/// Encode `chars` using the charset stored on the `charset` object.
/// Unmappable characters are replaced with `'?'`.
pub fn encode_with_charset(ctx: &dyn NativeContext, charset: ObjectRef, chars: &[u16]) -> Vec<u8> {
    let name = read_string_field(ctx, charset, CHARSET_FIELD_NAME)
        .map(|n| normalize_charset_name(&n))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "UTF-8".to_string());
    engine::encode_chars_lossy(&name, chars)
}

/// Returns `true` when `canon` (a name already produced by
/// `normalize_charset_name`) is a charset the transcoding engine can
/// actually decode/encode. A name like `"KOI8-U"` normalizes cleanly yet the
/// engine does not implement it, so the lossy helpers would silently fall back
/// to Latin-1; this probe lets the name-taking native methods surface an
/// unsupported-charset error instead. (Legacy CJK names such as `"Shift_JIS"`
/// ARE implemented — via encoding_rs — and so report supported.)
///
/// Probing with an empty input slice does no real transcoding work: an
/// unsupported name hits the engine's `UnsupportedCharset` arm immediately,
/// and a supported one decodes the empty slice to an empty result.
pub(crate) fn engine_supports(canon: &str) -> bool {
    !matches!(
        engine::decode_bytes(canon, &[]),
        Err(engine::CodingError {
            kind: engine::CodingErrorKind::UnsupportedCharset,
            ..
        })
    )
}

/// Encode a Rust `&str` as bytes for the named charset.  Used by
/// PrintStream / PrintWriter / OutputStreamWriter and
/// `String.getBytes(String)`.
pub fn encode_str_named(name: &str, s: &str) -> Vec<u8> {
    let norm = normalize_charset_name(name);
    if norm.is_empty() {
        return s.as_bytes().to_vec();
    }
    let chars: Vec<u16> = s.encode_utf16().collect();
    engine::encode_chars_lossy(&norm, &chars)
}

/// Decode bytes as a Rust `String` using the named charset.  Used by
/// the `new String([B, Charset)` family and by the StreamDecoder shim.
pub fn decode_str_named(name: &str, bytes: &[u8]) -> String {
    let norm = normalize_charset_name(name);
    if norm.is_empty() {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let chars = engine::decode_bytes_lossy(&norm, bytes);
    String::from_utf16_lossy(&chars)
}

// ---------------------------------------------------------------------------
// Native method bodies — CharsetEncoder.encode / CharsetDecoder.decode
// ---------------------------------------------------------------------------

fn enc_name(ctx: &dyn NativeContext, this: ObjectRef) -> String {
    match ctx.get_field(this, 0) {
        Value::Object(Some(cs)) => read_string_field(ctx, cs, CHARSET_FIELD_NAME)
            .map(|n| normalize_charset_name(&n))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "UTF-8".to_string()),
        _ => "UTF-8".to_string(),
    }
}

/// True when the decoder's `malformedInputAction` is `CodingErrorAction.REPLACE`
/// (substitute U+FFFD and continue). Default is REPORT, so an unreadable/absent
/// field conservatively returns false (preserving the strict error behavior).
fn decoder_malformed_is_replace(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    if let Value::Object(Some(action)) = ctx.get_field_by_name(this, "malformedInputAction") {
        // CodingErrorAction's only instance field is `String name`
        // ("REPLACE" / "REPORT" / "IGNORE").
        if let Value::Object(Some(s)) = ctx.get_field_by_name(action, "name") {
            return ctx.read_string(s).as_deref() == Some("REPLACE");
        }
    }
    false
}

fn is_high_surrogate(u: u16) -> bool {
    (0xD800..=0xDBFF).contains(&u)
}
fn is_low_surrogate(u: u16) -> bool {
    (0xDC00..=0xDFFF).contains(&u)
}

/// True when the named `CodingErrorAction` field of `this` (an encoder or
/// decoder) is `REPLACE`. Default is REPORT, so an unreadable/absent field
/// conservatively returns false (preserving the strict error behavior).
fn coding_action_is_replace(ctx: &dyn NativeContext, this: ObjectRef, field: &str) -> bool {
    if let Value::Object(Some(action)) = ctx.get_field_by_name(this, field) {
        // CodingErrorAction's only instance field is `String name`
        // ("REPLACE" / "REPORT" / "IGNORE").
        if let Value::Object(Some(s)) = ctx.get_field_by_name(action, "name") {
            return ctx.read_string(s).as_deref() == Some("REPLACE");
        }
    }
    false
}

/// The encoder's REPLACE-action replacement bytes. The JDK default is a single
/// `'?'` (0x3F) for every charset we implement; honour a custom `replacement`
/// byte[] if one is set, else fall back to `'?'`.
fn encoder_replacement(ctx: &dyn NativeContext, this: ObjectRef) -> Vec<u8> {
    if let Value::Object(Some(arr)) = ctx.get_field_by_name(this, "replacement") {
        let len = ctx.array_length(arr);
        if len > 0 {
            return read_byte_array(ctx, arr, 0, len);
        }
    }
    vec![b'?']
}

/// Read a `CodingErrorAction`-valued field (`malformedInputAction` /
/// `unmappableCharacterAction`) off a coder object and map it to the engine's
/// [`engine::CodingAction`]. An unreadable/absent field conservatively yields
/// REPORT (HotSpot's default), preserving strict error behavior. Three-way
/// variant of [`coding_action_is_replace`], used by the UTF-8 decode
/// orchestrator (which distinguishes REPLACE from IGNORE).
fn coding_action(ctx: &dyn NativeContext, obj: ObjectRef, field: &str) -> engine::CodingAction {
    if let Value::Object(Some(action)) = ctx.get_field_by_name(obj, field) {
        if let Value::Object(Some(s)) = ctx.get_field_by_name(action, "name") {
            return match ctx.read_string(s).as_deref() {
                Some("REPLACE") => engine::CodingAction::Replace,
                Some("IGNORE") => engine::CodingAction::Ignore,
                _ => engine::CodingAction::Report,
            };
        }
    }
    engine::CodingAction::Report
}

/// `CharsetEncoder.encode(CharBuffer, ByteBuffer, boolean end_of_input)
///  -> CoderResult`
///
/// Consumes UTF-16 code units from the input CharBuffer between its current
/// position and limit, encodes them via the engine, and writes them into the
/// output ByteBuffer. Advances both buffers' positions by the amount consumed /
/// produced. Returns UNDERFLOW on success, OVERFLOW when the output buffer fills
/// before all input is consumed, MALFORMED/UNMAPPABLE (under the REPORT action)
/// on bad input.
///
/// Encoding proceeds atom-by-atom — a BMP code unit or a complete surrogate
/// pair — so that:
///   * a trailing lone high surrogate is left buffered (UNDERFLOW) when more
///     input may follow, reuniting a pair split across two calls (a servlet
///     `Writer` writing one char at a time → Tomcat BUG-TC0622);
///   * OVERFLOW stops on a whole-atom boundary, never emitting a truncated
///     multi-byte sequence and never over/under-consuming the input (the prior
///     proportional estimate corrupted variable-width output when the byte
///     buffer filled mid-character);
///   * the encoder's REPLACE actions are honoured (substitute the replacement
///     bytes and continue) instead of always REPORTing — matching the real
///     `java.nio.charset.CharsetEncoder.encode` orchestrator this shadows, and
///     symmetric to `native_decoder_decode`.
fn native_encoder_encode(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = arg_obj(args, 0)?;
    let cb = arg_obj(args, 1)?;
    let bb = arg_obj(args, 2)?;
    // args[3] = boolean endOfInput (passed as Int 0/1).
    let end_of_input = matches!(args.get(3), Some(Value::Int(v)) if *v != 0);

    let (carr, cpos, clim) = match buf_state(ctx, cb) {
        Some(s) => s,
        None => {
            return Ok(Some(Value::Object(Some(alloc_coder_result(
                ctx,
                CR_UNDERFLOW,
            )))))
        }
    };
    let (barr, bpos, blim) = match buf_state(ctx, bb) {
        Some(s) => s,
        None => {
            return Ok(Some(Value::Object(Some(alloc_coder_result(
                ctx,
                CR_OVERFLOW,
            )))))
        }
    };

    let name = enc_name(ctx, this);
    let chars = read_char_array(ctx, carr, cpos as usize, (clim - cpos).max(0) as usize);
    if chars.is_empty() {
        let r = alloc_coder_result(ctx, CR_UNDERFLOW);
        return Ok(Some(Value::Object(Some(r))));
    }

    let avail = (blim - bpos).max(0) as usize;

    // Fast path: the whole slice encodes cleanly (no surrogate/mapping errors)
    // and fits in the destination. Avoids per-atom work for the common case.
    if let Ok(encoded) = engine::encode_chars(&name, &chars) {
        if encoded.len() <= avail {
            let written = write_byte_array(ctx, barr, bpos as usize, &encoded);
            set_pos(ctx, bb, bpos + written as i32);
            set_pos(ctx, cb, cpos + chars.len() as i32);
            let r = alloc_coder_result(ctx, CR_UNDERFLOW);
            return Ok(Some(Value::Object(Some(r))));
        }
    }

    // Precise path: encode atom by atom.
    let malformed_replace = coding_action_is_replace(ctx, this, "malformedInputAction");
    let unmappable_replace = coding_action_is_replace(ctx, this, "unmappableCharacterAction");
    let replacement = encoder_replacement(ctx, this);

    let mut out: Vec<u8> = Vec::new();
    let mut i = 0usize;
    let mut tag = CR_UNDERFLOW;
    while i < chars.len() {
        let c = chars[i];
        // Resolve the next atom into (units, bytes), or set `tag`+break for a
        // REPORT-action error or an incomplete trailing surrogate.
        let (atom_units, atom_bytes): (usize, Vec<u8>) = if is_high_surrogate(c) {
            match chars.get(i + 1).copied() {
                Some(lo) if is_low_surrogate(lo) => match engine::encode_chars(&name, &[c, lo]) {
                    Ok(b) => (2, b),
                    Err(e) if e.kind == engine::CodingErrorKind::Unmappable => {
                        if unmappable_replace {
                            (2, replacement.clone())
                        } else {
                            tag = CR_UNMAPPABLE;
                            break;
                        }
                    }
                    Err(_) => {
                        if malformed_replace {
                            (2, replacement.clone())
                        } else {
                            tag = CR_MALFORMED;
                            break;
                        }
                    }
                },
                None if !end_of_input => {
                    // Incomplete trailing high surrogate: leave it buffered and
                    // report UNDERFLOW (tag stays CR_UNDERFLOW).
                    break;
                }
                _ => {
                    // Lone high surrogate at end-of-input, or followed by a
                    // non-low-surrogate unit: malformed.
                    if malformed_replace {
                        (1, replacement.clone())
                    } else {
                        tag = CR_MALFORMED;
                        break;
                    }
                }
            }
        } else if is_low_surrogate(c) {
            if malformed_replace {
                (1, replacement.clone())
            } else {
                tag = CR_MALFORMED;
                break;
            }
        } else {
            match engine::encode_chars(&name, &[c]) {
                Ok(b) => (1, b),
                Err(e) if e.kind == engine::CodingErrorKind::Unmappable => {
                    if unmappable_replace {
                        (1, replacement.clone())
                    } else {
                        tag = CR_UNMAPPABLE;
                        break;
                    }
                }
                Err(_) => {
                    if malformed_replace {
                        (1, replacement.clone())
                    } else {
                        tag = CR_MALFORMED;
                        break;
                    }
                }
            }
        };

        if out.len() + atom_bytes.len() > avail {
            // The next atom won't fit: stop on this whole-atom boundary.
            tag = CR_OVERFLOW;
            break;
        }
        out.extend_from_slice(&atom_bytes);
        i += atom_units;
    }

    let written = write_byte_array(ctx, barr, bpos as usize, &out);
    set_pos(ctx, bb, bpos + written as i32);
    // `i` is the exact number of input units consumed; on a REPORT error or an
    // incomplete trailing surrogate it points at the offending unit (left
    // unconsumed), exactly matching the real encoder's CoderResult contract.
    set_pos(ctx, cb, cpos + i as i32);
    let r = alloc_coder_result(ctx, tag);
    Ok(Some(Value::Object(Some(r))))
}

/// `CharsetDecoder.decode(ByteBuffer, CharBuffer, boolean end_of_input)
///  -> CoderResult`
fn native_decoder_decode(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = arg_obj(args, 0)?;
    let bb = arg_obj(args, 1)?;
    let cb = arg_obj(args, 2)?;
    // args[3] = boolean endOfInput (passed as Int 0/1). When false, a
    // truncated trailing multi-byte sequence is UNDERFLOW (wait for more
    // input), not MALFORMED — matching java.nio.charset.CharsetDecoder.decode.
    let end_of_input = matches!(args.get(3), Some(Value::Int(v)) if *v != 0);

    let (barr, bpos, blim) = match buf_state(ctx, bb) {
        Some(s) => s,
        None => {
            return Ok(Some(Value::Object(Some(alloc_coder_result(
                ctx,
                CR_UNDERFLOW,
            )))))
        }
    };
    let (carr, cpos, clim) = match buf_state(ctx, cb) {
        Some(s) => s,
        None => {
            return Ok(Some(Value::Object(Some(alloc_coder_result(
                ctx,
                CR_OVERFLOW,
            )))))
        }
    };

    let name = enc_name(ctx, this);
    let bytes = read_byte_array(ctx, barr, bpos as usize, (blim - bpos).max(0) as usize);
    if bytes.is_empty() {
        let r = alloc_coder_result(ctx, CR_UNDERFLOW);
        return Ok(Some(Value::Object(Some(r))));
    }

    // UTF-8 is decoded through a JDK-faithful streaming state machine so the
    // number / placement of U+FFFD substitutions in REPLACE mode is
    // byte-for-byte identical to `sun.nio.cs.UTF_8.Decoder` (the W3C
    // maximal-subpart rule HotSpot follows). The generic engine path below
    // (Rust `from_utf8_lossy`) diverges at multi-byte-sequence buffer
    // boundaries — see `cratonvm_native_api::charset::utf8_decode`.
    if name == "UTF-8" {
        let action = coding_action(ctx, this, "malformedInputAction");
        let avail = (clim - cpos).max(0) as usize;
        let mut units: Vec<u16> = Vec::new();
        let (consumed, status) =
            engine::utf8_decode(&bytes, end_of_input, action, &mut units, avail);
        let written = write_char_array(ctx, carr, cpos as usize, &units);
        set_pos(ctx, cb, cpos + written as i32);
        set_pos(ctx, bb, bpos + consumed as i32);
        let tag = match status {
            engine::Utf8DecodeStatus::Underflow => CR_UNDERFLOW,
            engine::Utf8DecodeStatus::Overflow => CR_OVERFLOW,
            engine::Utf8DecodeStatus::Malformed => CR_MALFORMED,
        };
        let r = alloc_coder_result(ctx, tag);
        return Ok(Some(Value::Object(Some(r))));
    }

    // `input_len` = how many input bytes the `decoded` units represent
    // (== bytes.len() except when we stop early at a truncated trailing
    // sequence, leaving its bytes buffered for the next call).
    //
    // On a genuinely MALFORMED sequence we must honor the decoder's configured
    // `malformedInputAction`: REPORT (the default) surfaces it as a MALFORMED
    // CoderResult; REPLACE substitutes the replacement char (U+FFFD) for each
    // ill-formed subsequence and continues — exactly what the real
    // `java.nio.charset.CharsetDecoder.decode()` orchestrator does (which this
    // native shadows). Without this, REPLACE-mode callers (e.g. Tomcat's
    // `TestUtf8`/`Utf8Decoder` conformance suite, and any `onMalformedInput(
    // REPLACE)` decoder) hit a spurious MalformedInputException.
    let replace = decoder_malformed_is_replace(ctx, this);
    let (decoded, input_len) = match engine::decode_bytes(&name, &bytes) {
        Ok(chars) => (chars, bytes.len()),
        Err(e) if e.kind == engine::CodingErrorKind::Incomplete && !end_of_input => {
            // Decode only the valid prefix; the partial trailing bytes stay in
            // the buffer (position advances only past the prefix) and the
            // decoder reports UNDERFLOW. A truncated trailing sequence is NOT
            // yet malformed, so REPLACE does not apply here.
            match engine::decode_bytes(&name, &bytes[..e.offset]) {
                Ok(chars) => (chars, e.offset),
                Err(_) if replace => (engine::decode_bytes_lossy(&name, &bytes), bytes.len()),
                Err(_) => {
                    set_pos(ctx, bb, bpos + e.offset as i32);
                    let r = alloc_coder_result(ctx, CR_MALFORMED);
                    return Ok(Some(Value::Object(Some(r))));
                }
            }
        }
        Err(_) if replace => (engine::decode_bytes_lossy(&name, &bytes), bytes.len()),
        Err(e) => {
            set_pos(ctx, bb, bpos + e.offset as i32);
            let r = alloc_coder_result(ctx, CR_MALFORMED);
            return Ok(Some(Value::Object(Some(r))));
        }
    };

    let avail = (clim - cpos).max(0) as usize;
    let to_write = decoded.len().min(avail);
    let written = write_char_array(ctx, carr, cpos as usize, &decoded[..to_write]);
    set_pos(ctx, cb, cpos + written as i32);

    if to_write < decoded.len() {
        // Output buffer full before all decoded chars were written → OVERFLOW;
        // consume a proportional slice of the (prefix) input.
        let consumed = proportional_input_consumed_bytes(&bytes[..input_len], &decoded, to_write);
        set_pos(ctx, bb, bpos + consumed as i32);
        let r = alloc_coder_result(ctx, CR_OVERFLOW);
        Ok(Some(Value::Object(Some(r))))
    } else {
        set_pos(ctx, bb, bpos + input_len as i32);
        let r = alloc_coder_result(ctx, CR_UNDERFLOW);
        Ok(Some(Value::Object(Some(r))))
    }
}

fn proportional_input_consumed_bytes(bytes: &[u8], decoded: &[u16], to_write: usize) -> usize {
    if decoded.is_empty() {
        return bytes.len();
    }
    let num = to_write as u64 * bytes.len() as u64;
    (num / decoded.len() as u64) as usize
}

// ---------------------------------------------------------------------------
// Charset.encode(...) / decode(...) — high-level one-shot conversions
// ---------------------------------------------------------------------------

fn native_charset_encode_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = arg_obj(args, 0)?;
    let s = match args.get(1) {
        Some(Value::Object(Some(s))) => *s,
        _ => return Ok(Some(Value::Object(Some(alloc_byte_buffer(ctx, &[]))))),
    };
    let chars = read_string_utf16(ctx, s);
    let bytes = encode_with_charset(ctx, this, &chars);
    Ok(Some(Value::Object(Some(alloc_byte_buffer(ctx, &bytes)))))
}

fn native_charset_encode_charbuf(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = arg_obj(args, 0)?;
    let cb = match args.get(1) {
        Some(Value::Object(Some(b))) => *b,
        _ => return Ok(Some(Value::Object(Some(alloc_byte_buffer(ctx, &[]))))),
    };
    let (carr, cpos, clim) = match buf_state(ctx, cb) {
        Some(s) => s,
        None => return Ok(Some(Value::Object(Some(alloc_byte_buffer(ctx, &[]))))),
    };
    let chars = read_char_array(ctx, carr, cpos as usize, (clim - cpos).max(0) as usize);
    let bytes = encode_with_charset(ctx, this, &chars);
    // Advance the input position — matches HotSpot's contract that
    // Charset.encode(CharBuffer) consumes the buffer.
    set_pos(ctx, cb, clim);
    Ok(Some(Value::Object(Some(alloc_byte_buffer(ctx, &bytes)))))
}

fn native_charset_decode_bytebuf(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = arg_obj(args, 0)?;
    let bb = match args.get(1) {
        Some(Value::Object(Some(b))) => *b,
        _ => return Ok(Some(Value::Object(Some(alloc_char_buffer(ctx, &[]))))),
    };
    // Round 58 — tolerate real-JDK ByteBuffer layouts. Our synthetic
    // ByteBuffer uses (array=0, pos=1, limit=2, cap=3, mark=4) but real-JDK
    // `HeapByteBuffer` stores `hb` at a different slot. Scan field slots
    // 0..=8 for the first byte[] reference when the normal (pos, limit)
    // lookup cannot identify the buffer layout. This lets
    // Tomcat's `CharsetUtil.isAsciiSuperset` loop (which feeds a 1-byte
    // ByteBuffer into `decode` per iteration) work even though the
    // ByteBuffer came from real-JDK bytecode rather than our `allocate`
    // native.
    let mut bytes: Vec<u8> = Vec::new();
    let mut read_buffer_state = false;
    if let Some((barr, bpos, blim)) = buf_state(ctx, bb) {
        read_buffer_state = true;
        let want = (blim - bpos).max(0) as usize;
        if want > 0 {
            bytes = read_byte_array(ctx, barr, bpos as usize, want);
            set_pos(ctx, bb, blim);
        }
    }
    if !read_buffer_state {
        // Layout fallback: probe slots 0..=8 for the first byte[] field.
        for slot in 0..=8 {
            if let Value::Object(Some(arr)) = ctx.get_field(bb, slot) {
                let len = ctx.array_length(arr);
                if len > 0 {
                    // Heuristic: cap at 256 to avoid pulling a huge backing
                    // array. The Tomcat probe path uses 1-byte buffers; any
                    // larger consumer should be routed through our synthetic
                    // allocate path which preserves pos/limit.
                    let n = len.min(256);
                    bytes = read_byte_array(ctx, arr, 0, n);
                    break;
                }
            }
        }
    }
    let chars = decode_with_charset(ctx, this, &bytes);
    Ok(Some(Value::Object(Some(alloc_char_buffer(ctx, &chars)))))
}

// ---------------------------------------------------------------------------
// Override: String.getBytes(Charset) — the previous implementation always
// emitted UTF-8 regardless of charset.
// ---------------------------------------------------------------------------

fn native_string_get_bytes_charset(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = arg_obj(args, 0)?;
    let val = ctx.read_string(this).unwrap_or_default();
    let charset_ref = match args.get(1) {
        Some(Value::Object(Some(c))) => *c,
        _ => {
            // Null charset → NullPointerException in HotSpot; keep it
            // lenient for defensive code paths and default to UTF-8.
            let arr = ctx.new_array(ArrayElementType::Byte, val.len());
            for (i, &b) in val.as_bytes().iter().enumerate() {
                ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
            }
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    // FIX (charset-nb, MEDIUM): refuse to silently produce wrong bytes for a
    // charset the transcoding engine cannot actually encode. Previously this
    // dropped straight into `encode_with_charset` → `encode_chars_lossy`, which
    // for an unsupported canonical name (e.g. "Shift_JIS", "EUC-JP") falls back
    // to a Latin-1 / byte-identity substitution — emitting plausible-looking but
    // SEMANTICALLY WRONG bytes with no error. `String.getBytes(Charset)` takes a
    // already-constructed `Charset` object and is NOT declared to throw a checked
    // exception, so we surface the VM's limitation as an unchecked
    // `UnsupportedOperationException` rather than misencoding. Charsets the engine
    // genuinely supports (UTF-*, ISO-8859-*, US-ASCII, windows-125x, KOI8-R)
    // continue to encode correctly through the normal path below.
    let canon = charset_name_of(ctx, Value::Object(Some(charset_ref)));
    if !engine_supports(&canon) {
        return Err(RuntimeError::UnsupportedOperationException {
            message: format!(
                "charset \"{}\" is not supported by this VM's transcoding engine \
                 (String.getBytes(Charset) would otherwise emit incorrect bytes)",
                canon
            ),
        }
        .into());
    }
    let chars: Vec<u16> = val.encode_utf16().collect();
    let bytes = encode_with_charset(ctx, charset_ref, &chars);
    let arr = ctx.new_array(ArrayElementType::Byte, bytes.len());
    for (i, &b) in bytes.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
    }
    Ok(Some(Value::Object(Some(arr))))
}

fn native_string_get_bytes_named(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = arg_obj(args, 0)?;
    let val = ctx.read_string(this).unwrap_or_default();
    let name = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    };
    let norm = normalize_charset_name(&name);
    // `String.getBytes(String)` throws the *checked* `UnsupportedEncodingException`
    // for a name that is unknown (`norm` empty) OR that normalizes to a canonical
    // charset the transcoding engine cannot actually encode (e.g. "Shift_JIS").
    // Previously the latter slipped through to `encode_chars_lossy`, which
    // silently produced Latin-1 bytes for an unsupported charset.
    if norm.is_empty() || !engine_supports(&norm) {
        return Err(RuntimeError::IOException {
            message: format!("UnsupportedEncodingException: {}", name),
        }
        .into());
    }
    let chars: Vec<u16> = val.encode_utf16().collect();
    let bytes = engine::encode_chars_lossy(&norm, &chars);
    let arr = ctx.new_array(ArrayElementType::Byte, bytes.len());
    for (i, &b) in bytes.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
    }
    Ok(Some(Value::Object(Some(arr))))
}

// ---------------------------------------------------------------------------
// Helpers shared with the rest of the crate
// ---------------------------------------------------------------------------

fn arg_obj(args: &[Value], i: usize) -> Result<ObjectRef, cratonvm_types::error::MethodCallFailed> {
    match args.get(i) {
        Some(Value::Object(Some(o))) => Ok(*o),
        _ => Err(RuntimeError::NullPointerException {
            message: Some(format!("charset arg {} is null", i)),
        }
        .into()),
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Install the real transcoding implementations, replacing the
/// previously-stubbed ones.  The registry dedups on (class, name,
/// descriptor) so re-registering is idempotent.
pub fn register_real_charset_natives(registry: &mut NativeMethodRegistry) {
    let enc = "java/nio/charset/CharsetEncoder";
    registry.register(
        enc,
        "encode",
        "(Ljava/nio/CharBuffer;Ljava/nio/ByteBuffer;Z)Ljava/nio/charset/CoderResult;",
        native_encoder_encode,
    );
    // Charset.encode(CharBuffer) / encode(String) one-shots (Java public API).
    registry.register(
        enc,
        "encode",
        "(Ljava/nio/CharBuffer;)Ljava/nio/ByteBuffer;",
        native_charset_encode_charbuf_via_encoder,
    );

    let dec = "java/nio/charset/CharsetDecoder";
    registry.register(
        dec,
        "decode",
        "(Ljava/nio/ByteBuffer;Ljava/nio/CharBuffer;Z)Ljava/nio/charset/CoderResult;",
        native_decoder_decode,
    );
    registry.register(
        dec,
        "decode",
        "(Ljava/nio/ByteBuffer;)Ljava/nio/CharBuffer;",
        native_charset_decode_bytebuf_via_decoder,
    );

    let cs = "java/nio/charset/Charset";
    registry.register(
        cs,
        "encode",
        "(Ljava/lang/String;)Ljava/nio/ByteBuffer;",
        native_charset_encode_string,
    );
    registry.register(
        cs,
        "encode",
        "(Ljava/nio/CharBuffer;)Ljava/nio/ByteBuffer;",
        native_charset_encode_charbuf,
    );
    registry.register(
        cs,
        "decode",
        "(Ljava/nio/ByteBuffer;)Ljava/nio/CharBuffer;",
        native_charset_decode_bytebuf,
    );

    // Fix the String side: honour the charset instead of always UTF-8.
    let s = "java/lang/String";
    registry.register(
        s,
        "getBytes",
        "(Ljava/nio/charset/Charset;)[B",
        native_string_get_bytes_charset,
    );
    registry.register(
        s,
        "getBytes",
        "(Ljava/lang/String;)[B",
        native_string_get_bytes_named,
    );
    // Round 24 — `String.getBytes()` (no-arg, default charset). The JDK
    // bytecode for this method calls `Charset.defaultCharset()` and then
    // dispatches via `String.encode(Charset, byte coder, byte[] value)`.
    // That path goes through `CharsetEncoder.encode(CharBuffer, ByteBuffer, Z)`
    // with a real-JDK HeapCharBuffer/HeapByteBuffer whose field layout
    // does not match our synthetic 5-field Buffer overlay used by the
    // encoder native — so the encode loop reads zero chars and returns
    // an empty byte array. Keycloak / WildFly's
    // `ProcessEnvironment.obtainProcessUUID` then writes a 0-byte
    // process.uuid file and the subsequent `Files.readAllBytes` returns
    // empty / triggers the IOException-Cannot-find-file cascade.
    //
    // Override with a direct UTF-8 encode (matches the platform default
    // charset we report from `Charset.defaultCharset`).
    registry.register(s, "getBytes", "()[B", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        let text = ctx.read_string(this).unwrap_or_default();
        let bytes = text.as_bytes();
        let arr = ctx.new_array(ArrayElementType::Byte, bytes.len());
        for (i, &b) in bytes.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
        }
        Ok(Some(Value::Object(Some(arr))))
    });
}

/// Read every remaining byte from a ByteBuffer and advance its position to the
/// limit — mirrors `native_charset_decode_bytebuf`'s robust two-path read:
/// normal pos/limit, then a slot scan only when the real-JDK layout cannot be
/// identified.
fn read_and_consume_bytebuffer(ctx: &mut dyn NativeContext, bb: ObjectRef) -> Vec<u8> {
    let mut bytes: Vec<u8> = Vec::new();
    let mut read_buffer_state = false;
    if let Some((barr, bpos, blim)) = buf_state(ctx, bb) {
        read_buffer_state = true;
        let want = (blim - bpos).max(0) as usize;
        if want > 0 {
            bytes = read_byte_array(ctx, barr, bpos as usize, want);
            set_pos(ctx, bb, blim);
        }
    }
    if !read_buffer_state {
        for slot in 0..=8 {
            if let Value::Object(Some(arr)) = ctx.get_field(bb, slot) {
                let len = ctx.array_length(arr);
                if len > 0 {
                    let n = len.min(256);
                    bytes = read_byte_array(ctx, arr, 0, n);
                    break;
                }
            }
        }
    }
    bytes
}

/// Decode `bytes` honouring a CharsetDecoder's malformed-input action.
/// `Ok(units)` on success (REPLACE substitutes U+FFFD, IGNORE skips the bad
/// input); `Err((exc_class, input_len))` when the action is REPORT and the
/// input is malformed — the caller throws `exc_class(input_len)`.
fn decode_honoring_action(
    name: &str,
    bytes: &[u8],
    malformed: engine::CodingAction,
) -> Result<Vec<u16>, (&'static str, i32)> {
    if name == "UTF-8" {
        let mut units = Vec::new();
        // Each input byte yields at most one UTF-16 unit, so the byte count is
        // always sufficient output capacity for a one-shot decode (no OVERFLOW).
        let cap = bytes.len() + 1;
        let (_consumed, status) = engine::utf8_decode(bytes, true, malformed, &mut units, cap);
        return match status {
            engine::Utf8DecodeStatus::Malformed => {
                Err(("java/nio/charset/MalformedInputException", 1))
            }
            _ => Ok(units),
        };
    }
    match engine::decode_bytes(name, bytes) {
        Ok(units) => Ok(units),
        // Unknown charset: fall back to the lossy (Latin-1) path rather than
        // throwing, matching the name-taking decode helpers.
        Err(e) if matches!(e.kind, engine::CodingErrorKind::UnsupportedCharset) => {
            Ok(engine::decode_bytes_lossy(name, bytes))
        }
        Err(e) => match malformed {
            engine::CodingAction::Report => Err((
                "java/nio/charset/MalformedInputException",
                e.length.max(1) as i32,
            )),
            // REPLACE / IGNORE substitute U+FFFD (byte-oriented charsets rarely
            // malform, so a distinct IGNORE path is not worth the complexity).
            _ => Ok(engine::decode_bytes_lossy(name, bytes)),
        },
    }
}

/// Encode `chars` honouring a CharsetEncoder's error actions. Symmetric to
/// [`decode_honoring_action`]: a REPORT action throws, REPLACE / IGNORE
/// substitute the charset's replacement byte.
fn encode_honoring_actions(
    name: &str,
    chars: &[u16],
    malformed: engine::CodingAction,
    unmappable: engine::CodingAction,
) -> Result<Vec<u8>, (&'static str, i32)> {
    match engine::encode_chars(name, chars) {
        Ok(bytes) => Ok(bytes),
        Err(e) => {
            let (action, cls) = match e.kind {
                engine::CodingErrorKind::Unmappable => {
                    (unmappable, "java/nio/charset/UnmappableCharacterException")
                }
                engine::CodingErrorKind::Malformed => {
                    (malformed, "java/nio/charset/MalformedInputException")
                }
                // UnsupportedCharset / Incomplete: keep the prior lossy fallback.
                _ => return Ok(engine::encode_chars_lossy(name, chars)),
            };
            match action {
                engine::CodingAction::Report => Err((cls, e.length.max(1) as i32)),
                _ => Ok(engine::encode_chars_lossy(name, chars)),
            }
        }
    }
}

/// Build and throw (as `Err(ExceptionThrown)`) a `java.nio.charset`
/// coding-error exception via its JDK `(int inputLength)` constructor.
fn throw_coding_exception(
    ctx: &mut dyn NativeContext,
    class: &str,
    length: i32,
) -> MethodCallFailed {
    match ctx.new_object_initialized(class, "(I)V", &[Value::Int(length)]) {
        Ok(Some(Value::Object(Some(exc)))) => MethodCallFailed::ExceptionThrown(exc),
        _ => RuntimeError::IOException {
            message: format!("{class}: coding error"),
        }
        .into(),
    }
}

/// `CharsetEncoder.encode(CharBuffer) -> ByteBuffer` — the public convenience
/// method. Honours the encoder's configured error actions (symmetric to
/// [`native_charset_decode_bytebuf_via_decoder`]): a REPORT encoder throws an
/// `UnmappableCharacterException` / `MalformedInputException` on bad input
/// rather than silently substituting the charset's replacement byte. The prior
/// shim delegated to the `Charset.encode` one-shot, which always REPLACEs.
fn native_charset_encode_charbuf_via_encoder(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = arg_obj(args, 0)?;
    let cb = match args.get(1) {
        Some(Value::Object(Some(b))) => *b,
        _ => return Ok(Some(Value::Object(Some(alloc_byte_buffer(ctx, &[]))))),
    };
    let (carr, cpos, clim) = match buf_state(ctx, cb) {
        Some(s) => s,
        None => return Ok(Some(Value::Object(Some(alloc_byte_buffer(ctx, &[]))))),
    };
    let chars = read_char_array(ctx, carr, cpos as usize, (clim - cpos).max(0) as usize);
    let name = enc_name(ctx, this);
    let malformed = coding_action(ctx, this, "malformedInputAction");
    let unmappable = coding_action(ctx, this, "unmappableCharacterAction");
    match encode_honoring_actions(&name, &chars, malformed, unmappable) {
        Ok(bytes) => {
            // CharsetEncoder.encode(CharBuffer) consumes the input buffer.
            set_pos(ctx, cb, clim);
            Ok(Some(Value::Object(Some(alloc_byte_buffer(ctx, &bytes)))))
        }
        Err((cls, len)) => Err(throw_coding_exception(ctx, cls, len)),
    }
}

/// `CharsetDecoder.decode(ByteBuffer) -> CharBuffer` — the public convenience
/// method. Unlike `Charset.decode` (which always REPLACEs), this honours the
/// decoder's `onMalformedInput` action: REPORT (the default) surfaces a
/// `MalformedInputException`, while REPLACE / IGNORE substitute U+FFFD or skip
/// the bad input. The prior shim delegated to the `Charset.decode` one-shot,
/// dropping the action entirely — a REPORT decoder never threw and an IGNORE
/// decoder still emitted U+FFFD (Tomcat `TestStringCache.testCodingErrorLookup`).
fn native_charset_decode_bytebuf_via_decoder(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = arg_obj(args, 0)?;
    let bb = match args.get(1) {
        Some(Value::Object(Some(b))) => *b,
        _ => return Ok(Some(Value::Object(Some(alloc_char_buffer(ctx, &[]))))),
    };
    let bytes = read_and_consume_bytebuffer(ctx, bb);
    let name = enc_name(ctx, this);
    let malformed = coding_action(ctx, this, "malformedInputAction");
    match decode_honoring_action(&name, &bytes, malformed) {
        Ok(units) => Ok(Some(Value::Object(Some(alloc_char_buffer(ctx, &units))))),
        Err((cls, len)) => Err(throw_coding_exception(ctx, cls, len)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::mock_ctx;

    fn make_charset(ctx: &mut dyn NativeContext, name: &str) -> ObjectRef {
        let cs = alloc_concurrent_synthetic(ctx, "java/nio/charset/Charset", 1);
        let n = ctx.create_string(name);
        ctx.set_field(cs, CHARSET_FIELD_NAME, Value::Object(Some(n)));
        cs
    }

    #[test]
    fn charset_name_readback() {
        let mut ctx = mock_ctx();
        let cs = make_charset(&mut ctx, "UTF-8");
        let name = read_string_field(&ctx, cs, CHARSET_FIELD_NAME);
        assert_eq!(name.as_deref(), Some("UTF-8"));
    }

    #[test]
    fn encode_with_charset_utf8() {
        let mut ctx = mock_ctx();
        let cs = make_charset(&mut ctx, "UTF-8");
        let chars: Vec<u16> = "A\u{00A9}".encode_utf16().collect();
        let bytes = encode_with_charset(&ctx, cs, &chars);
        assert_eq!(bytes, vec![0x41, 0xC2, 0xA9]);
    }

    #[test]
    fn decode_with_charset_utf8() {
        let mut ctx = mock_ctx();
        let cs = make_charset(&mut ctx, "UTF-8");
        let chars = decode_with_charset(&ctx, cs, &[0x41, 0xC2, 0xA9]);
        assert_eq!(String::from_utf16(&chars).unwrap(), "A\u{00A9}");
    }

    #[test]
    fn consuming_empty_bytebuffer_does_not_decode_its_backing_array() {
        // A legitimate empty ByteBuffer may retain a non-empty backing array:
        // ByteBuffer.wrap(array, offset, 0) is exactly such a view. The
        // real-layout slot fallback is only for buffers whose state cannot be
        // read, not for a known empty remaining range.
        let mut ctx = mock_ctx();
        let bb = alloc_byte_buffer(&mut ctx, &[b'A', b'B', b'C', b'D']);
        ctx.set_field(bb, BUF_FIELD_POS, Value::Int(2));
        ctx.set_field(bb, BUF_FIELD_LIMIT, Value::Int(2));
        assert!(read_and_consume_bytebuffer(&mut ctx, bb).is_empty());
    }

    #[test]
    fn latin1_roundtrip_via_named() {
        let s = "café";
        let b = encode_str_named("ISO-8859-1", s);
        assert_eq!(b.last().copied(), Some(0xE9));
        assert_eq!(decode_str_named("ISO-8859-1", &b), s);
    }

    #[test]
    fn engine_supports_known_canonical_names() {
        assert!(engine_supports("UTF-8"));
        assert!(engine_supports("ISO-8859-1"));
        assert!(engine_supports("windows-1252"));
    }

    #[test]
    fn engine_supports_rejects_unimplemented_canonical_name() {
        // `normalize_charset_name` canonicalizes "KOI8-U" successfully, but the
        // transcoding engine has no KOI8-U coder — the name-path natives must
        // surface an unsupported-charset error rather than fall back to a lossy
        // Latin-1 encode. (Shift_JIS / EUC-JP are now handled via encoding_rs;
        // see `shift_jis_decodes_via_encoding_rs`.)
        assert_eq!(normalize_charset_name("KOI8-U"), "KOI8-U");
        assert!(!engine_supports("KOI8-U"));
    }

    #[test]
    fn shift_jis_decodes_via_encoding_rs() {
        // "日本語" in Shift_JIS = 93FA 967B 8CEA. The engine must now decode it
        // (previously it fell through to a Latin-1 read that produced "ú{ê"),
        // matching HotSpot for Spring's ContentDisposition Shift_JIS filenames.
        assert!(engine_supports("Shift_JIS"));
        let bytes = [0x93u8, 0xFA, 0x96, 0x7B, 0x8C, 0xEA];
        let decoded = engine::decode_bytes("Shift_JIS", &bytes).expect("valid Shift_JIS");
        let expected: Vec<u16> = "日本語".encode_utf16().collect();
        assert_eq!(decoded, expected);
    }

    #[test]
    fn get_bytes_charset_supported_encodes_correctly() {
        // The supported-charset path must still produce real bytes. "A©" in
        // ISO-8859-1 = [0x41, 0xE9-ish]; use a code point representable in
        // Latin-1 to confirm the requested charset (not UTF-8) is honoured.
        let mut ctx = mock_ctx();
        let cs = make_charset(&mut ctx, "ISO-8859-1");
        let this = ctx.create_string("A\u{00A9}");
        let r = native_string_get_bytes_charset(
            &mut ctx,
            &[Value::Object(Some(this)), Value::Object(Some(cs))],
        )
        .expect("supported charset must not error");
        let arr = match r {
            Some(Value::Object(Some(a))) => a,
            other => panic!("expected byte[] result, got {:?}", other),
        };
        // 'A' = 0x41, '©' (U+00A9) = 0xA9 in Latin-1 (single byte, not the
        // 2-byte 0xC2 0xA9 a UTF-8 fallback would emit).
        assert_eq!(ctx.array_length(arr), 2);
        assert_eq!(ctx.get_array_element(arr, 0).as_int(), Some(0x41));
        assert_eq!(
            ctx.get_array_element(arr, 1).as_int(),
            Some(0xA9u8 as i8 as i32)
        );
    }

    #[test]
    fn get_bytes_charset_unsupported_throws_instead_of_latin1() {
        // FIX (charset-nb): `String.getBytes(Charset)` for an unsupported
        // charset must FAIL LOUD rather than silently substitute Latin-1 bytes.
        let mut ctx = mock_ctx();
        let cs = make_charset(&mut ctx, "KOI8-U");
        let this = ctx.create_string("hello");
        let err = native_string_get_bytes_charset(
            &mut ctx,
            &[Value::Object(Some(this)), Value::Object(Some(cs))],
        )
        .expect_err("unsupported charset must surface an error, not Latin-1 bytes");
        // It must be the UnsupportedOperationException we raise, mentioning the
        // offending charset name — not a silently-wrong byte array.
        let msg = format!("{:?}", err);
        assert!(
            msg.contains("UnsupportedOperationException"),
            "expected UnsupportedOperationException, got: {msg}"
        );
        assert!(
            msg.contains("KOI8-U"),
            "error should name the unsupported charset, got: {msg}"
        );
    }

    // --- BUG-TC0622: streaming encoder cross-call high-surrogate carry ---

    /// Build a synthetic `CharsetEncoder` whose charset (slot 0) is `name`.
    fn make_encoder(ctx: &mut dyn NativeContext, name: &str) -> ObjectRef {
        let cs = make_charset(ctx, name);
        let enc = alloc_concurrent_synthetic(ctx, "java/nio/charset/CharsetEncoder", 3);
        ctx.set_field(enc, 0, Value::Object(Some(cs)));
        enc
    }

    fn coder_tag(ctx: &dyn NativeContext, r: Option<Value>) -> i32 {
        match r {
            Some(Value::Object(Some(cr))) => ctx.get_field(cr, 0).as_int().unwrap_or(-1),
            other => panic!("expected CoderResult, got {:?}", other),
        }
    }

    fn cb_pos(ctx: &dyn NativeContext, cb: ObjectRef) -> i32 {
        ctx.get_field(cb, BUF_FIELD_POS).as_int().unwrap_or(-1)
    }

    fn bb_bytes(ctx: &dyn NativeContext, bb: ObjectRef) -> Vec<u8> {
        let (arr, _pos, _lim) = buf_state(ctx, bb).expect("byte buffer state");
        let upto = ctx.get_field(bb, BUF_FIELD_POS).as_int().unwrap_or(0) as usize;
        (0..upto)
            .map(|i| (ctx.get_array_element(arr, i).as_int().unwrap_or(0) & 0xFF) as u8)
            .collect()
    }

    #[test]
    fn encoder_buffers_trailing_high_surrogate_when_more_input_coming() {
        // 'A' followed by a lone high surrogate, endOfInput=false. The encoder
        // must emit 'A', leave the high surrogate buffered (input position stops
        // before it), and report UNDERFLOW — NOT substitute U+FFFD.
        let mut ctx = mock_ctx();
        let enc = make_encoder(&mut ctx, "UTF-8");
        let cb = alloc_char_buffer(&mut ctx, &[0x0041, 0xD800]);
        let bb = alloc_byte_buffer(&mut ctx, &[0u8; 8]);
        // alloc_byte_buffer sets limit/pos to the data length (8) with pos 0;
        // that's the writable window we need.
        let r = native_encoder_encode(
            &mut ctx,
            &[
                Value::Object(Some(enc)),
                Value::Object(Some(cb)),
                Value::Object(Some(bb)),
                Value::Int(0), // endOfInput = false
            ],
        )
        .expect("encode must not error");
        assert_eq!(coder_tag(&ctx, r), CR_UNDERFLOW);
        assert_eq!(bb_bytes(&ctx, bb), vec![0x41], "only 'A' should be written");
        assert_eq!(cb_pos(&ctx, cb), 1, "high surrogate must stay unconsumed");
    }

    #[test]
    fn encoder_completes_pair_in_single_buffer() {
        // A full surrogate pair (U+10000) in one buffer must encode to its true
        // 4-byte UTF-8 form and consume both units — the carry logic must not
        // mistakenly hold back a well-paired surrogate.
        let mut ctx = mock_ctx();
        let enc = make_encoder(&mut ctx, "UTF-8");
        let cb = alloc_char_buffer(&mut ctx, &[0xD800, 0xDC00]); // U+10000
        let bb = alloc_byte_buffer(&mut ctx, &[0u8; 8]);
        let r = native_encoder_encode(
            &mut ctx,
            &[
                Value::Object(Some(enc)),
                Value::Object(Some(cb)),
                Value::Object(Some(bb)),
                Value::Int(0),
            ],
        )
        .expect("encode must not error");
        assert_eq!(coder_tag(&ctx, r), CR_UNDERFLOW);
        assert_eq!(bb_bytes(&ctx, bb), vec![0xF0, 0x90, 0x80, 0x80]);
        assert_eq!(cb_pos(&ctx, cb), 2);
    }

    #[test]
    fn encoder_overflow_stops_on_whole_atom_boundary() {
        // 'A' + U+10000 (a surrogate pair = 4 UTF-8 bytes) into a 3-byte window.
        // 'A' fits (1 byte); the pair (4 bytes) does not. The encoder must stop
        // on the atom boundary — write ONLY 'A', report OVERFLOW, and consume
        // exactly 1 unit — never a truncated multi-byte sequence (the prior
        // proportional path wrote part of the pair and mis-consumed input,
        // corrupting Tomcat's supplementary-char response body).
        let mut ctx = mock_ctx();
        let enc = make_encoder(&mut ctx, "UTF-8");
        let cb = alloc_char_buffer(&mut ctx, &[0x0041, 0xD800, 0xDC00]);
        let bb = alloc_byte_buffer(&mut ctx, &[0u8; 3]);
        let r = native_encoder_encode(
            &mut ctx,
            &[
                Value::Object(Some(enc)),
                Value::Object(Some(cb)),
                Value::Object(Some(bb)),
                Value::Int(0),
            ],
        )
        .expect("encode must not error");
        assert_eq!(coder_tag(&ctx, r), CR_OVERFLOW);
        assert_eq!(
            bb_bytes(&ctx, bb),
            vec![0x41],
            "only the whole 'A' atom fits"
        );
        assert_eq!(
            cb_pos(&ctx, cb),
            1,
            "only 'A' consumed; the pair is retried"
        );
    }

    #[test]
    fn encoder_trailing_high_surrogate_is_malformed_at_end_of_input() {
        // The same trailing high surrogate, but endOfInput=true: no more input
        // can complete it, so it is genuinely MALFORMED (matches the JDK).
        let mut ctx = mock_ctx();
        let enc = make_encoder(&mut ctx, "UTF-8");
        let cb = alloc_char_buffer(&mut ctx, &[0x0041, 0xD800]);
        let bb = alloc_byte_buffer(&mut ctx, &[0u8; 8]);
        let r = native_encoder_encode(
            &mut ctx,
            &[
                Value::Object(Some(enc)),
                Value::Object(Some(cb)),
                Value::Object(Some(bb)),
                Value::Int(1), // endOfInput = true
            ],
        )
        .expect("encode must not error");
        assert_eq!(coder_tag(&ctx, r), CR_MALFORMED);
        assert_eq!(
            cb_pos(&ctx, cb),
            1,
            "position advances to the bad surrogate"
        );
    }
}
