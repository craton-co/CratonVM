// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Real-mode shim for `sun.nio.cs.StreamEncoder`.
//!
//! Symmetric to [`crate::stream_decoder`]: wraps an `OutputStream` with
//! a configurable charset, encoding UTF-16 chars from the caller to
//! bytes and forwarding them via the underlying stream's
//! `write([BII)V` method.  The JDK bytecode reaches into sun.nio.ch
//! internals we don't cover; this synthetic implementation keeps its
//! state partly in REAL fields of the real `sun/nio/cs/StreamEncoder`
//! class (by name, never by hardcoded index — see below) and partly in
//! a Rust side-table keyed by the object's identity hash.
//!
//! ## Field access is by NAME, never by hardcoded index
//!
//! The encoder is allocated with the REAL `sun/nio/cs/StreamEncoder` class id
//! (see `alloc_stream_encoder`), so it carries the real class's full field
//! layout, INCLUDING the two fields it inherits from `java.io.Writer`
//! (`writeBuffer`, `lock`) ahead of its own (`closed`, `cs`, `encoder`, `bb`,
//! `maxBufferCapacity`, `out`, `haveLeftoverChar`, `leftoverChar`, `lcb`).
//! An earlier version of this file addressed its own bookkeeping via
//! hardcoded indices 0/1/2, on the mistaken assumption that those were
//! `StreamEncoder`'s own first three fields — they are actually
//! `Writer.writeBuffer`, `Writer.lock`, and `StreamEncoder.closed`. Writing
//! the underlying `OutputStream` / canonical charset name / a monotonic id
//! there silently corrupted those three REAL fields (`lock` ending up holding
//! the charset name; `closed` reading `true` from construction, since the id
//! counter starts at 1) — see
//! `spring-web-flow-outputstreamwriter-close-corruption-FIXED.md`.
//! Fixed by resolving fields **by name** (`get_field_by_name`/
//! `set_field_by_name`, which walk the real class's field metadata rather
//! than trusting a hand-counted index — see
//! `native-hardcoded-inherited-field-slots.md`'s fix
//! recipe) for the two real fields this shim legitimately owns semantically
//! (`out`, `closed`), and keeping everything else (the canonical charset
//! name, the pending-bytes buffer) in the Rust-side table below, keyed by
//! `ctx.identity_hash_code` instead of a scratch field slot.
//!
//! Cross-call surrogate carry uses the REAL `sun.nio.cs.StreamEncoder` fields
//! `haveLeftoverChar` (boolean) and `leftoverChar` (char) by name — the exact
//! mechanism the JDK's own `StreamEncoder` uses to hold an unmatched high
//! surrogate between writes. Without the surrogate carry, a pair split
//! across two writes (e.g. a servlet `Writer` writing one char at a time)
//! encodes each half as a lone surrogate and corrupts supplementary-plane
//! text to U+FFFD (Tomcat BUG-TC0622).
//!
//! ## Output buffering (byte-count-based commit-threshold fidelity)
//!
//! Earlier versions of this shim called the underlying `OutputStream.write`
//! on EVERY `write(String|char[]|int)` call — i.e. no internal buffering at
//! all. The real JDK `sun.nio.cs.StreamEncoder` buffers encoded bytes into an
//! internal `ByteBuffer` (`INITIAL_BYTE_BUFFER_CAPACITY = 512`, growable up to
//! `MAX_BYTE_BUFFER_CAPACITY = 8192`) and only calls the underlying stream's
//! `write` when that buffer actually fills — e.g. writing 1024 separate
//! 16-byte `Writer.print()` calls reaches the underlying stream via a HANDFUL
//! of ~512-byte batched writes on real HotSpot, not 1024 individual 16-byte
//! writes.
//!
//! This distinction is directly observable through
//! `HttpServlet`'s legacy `NoBodyOutputStream.checkCommit()`, which flips the
//! response to committed the first time its running byte counter exceeds the
//! configured buffer size — a counter fed by however many bytes arrive per
//! underlying-stream `write` call. Flushing eagerly (one small write per
//! `Writer` call) overshoots that threshold on a different byte boundary than
//! real HotSpot's batched flush, decoupling CratonVM's commit timing from
//! upstream Tomcat's carefully-tuned `bufferSize` adjustment formula in
//! `HttpServletDoHeadBaseTest` (see
//! `dohead-streamencoder-eager-flush-commit-threshold-FIXED.md`).
//!
//! The Rust-side `SeState.pending` buffer below reproduces the real
//! 512-byte-initial / 8192-byte-max growable-buffer behaviour so the
//! underlying stream sees the same write-call granularity as real HotSpot.
//! It cannot live in an object field (see the GC-safety note on
//! [`crate::stream_decoder`] — the encoder is allocated with the REAL
//! `sun/nio/cs/StreamEncoder` class id, so the collector scans it via that
//! class's real reference map and would not root an extra scratch-slot
//! reference), so it lives in the same kind of side-table keyed by a stable
//! `int` id that `stream_decoder.rs` uses for its charset name / carry state.

use cratonvm_native_api::charset as engine;
use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult};
use cratonvm_types::{ArrayElementType, ClassId, ObjectRef, Value};

/// Per-encoder pending-bytes buffer plus the canonical charset name,
/// side-tabled by the object's stable identity hash (see the module doc's
/// GC-safety + real-field-corruption note). Mirrors the real StreamEncoder's
/// `bb` growable `ByteBuffer` field closely enough to reproduce its flush
/// granularity: starts at 512 bytes, grows (capped at 8192) only when a
/// pending encode would overflow the current capacity, and is flushed to the
/// underlying stream only when actually full — never eagerly per `write()`
/// call.
#[derive(Default)]
struct SeState {
    pending: Vec<u8>,
    capacity: usize,
    name: String,
    /// Whether a byte-order-mark has already been emitted for this stream.
    /// `engine::encode_chars`/`encode_chars_lossy` are pure per-call
    /// functions with no memory of prior calls, so "UTF-16"/"UTF-32" (the
    /// BOM-prefixed JDK charset names, as opposed to the fixed-endian
    /// "UTF-16BE"/"UTF-16LE"/etc.) would otherwise get a fresh BOM prepended
    /// on EVERY `write()` call instead of once at the start of the stream —
    /// real JDK's `sun.nio.cs.UTF_16.Encoder` tracks this with an internal
    /// `first` flag and only writes the BOM before the very first character.
    /// See `effective_encode_name` below for how this is used.
    bom_written: bool,
}

/// Mirrors `sun.nio.cs.StreamEncoder.INITIAL_BYTE_BUFFER_CAPACITY`.
const INITIAL_BYTE_BUFFER_CAPACITY: usize = 512;
/// Mirrors `sun.nio.cs.StreamEncoder.MAX_BYTE_BUFFER_CAPACITY`.
const MAX_BYTE_BUFFER_CAPACITY: usize = 8192;

fn se_table() -> &'static std::sync::Mutex<std::collections::HashMap<i32, SeState>> {
    static T: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<i32, SeState>>> =
        std::sync::OnceLock::new();
    T.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Stable per-object side-table key. Unlike a monotonic counter stashed in a
/// scratch field slot (the previous, corrupting approach — see module doc),
/// this touches no real field at all.
fn se_key(ctx: &dyn NativeContext, this: ObjectRef) -> i32 {
    ctx.identity_hash_code(this)
}

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
/// Alias resolution goes through the shared table in
/// `cratonvm_native_api::charset::canonical_charset_name`. The private copy
/// this function used to carry went stale — it lacked `IBM850` and the
/// multibyte families the engine has since gained, so
/// `OutputStreamWriter(os, ibm850Charset)` silently fell back to encoding
/// UTF-8 (Tomcat `TestDefaultServletEncoding*`: the DefaultServlet include
/// conversion put `C2 BD` on an ibm850 wire instead of `AB`).
fn normalize_supported(name: &str) -> Option<String> {
    let canon = engine::canonical_charset_name(name)?;
    // Probe with an empty slice: the engine's name `match` returns
    // `UnsupportedCharset` before encoding anything, so this is free —
    // and it keeps canonical-but-codecless names (e.g. KOI8-U) rejected.
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

/// Build (and request the throw of) a `java.io.IOException("Stream closed")`,
/// matching real `StreamEncoder.ensureOpen()`'s exact message.
fn throw_stream_closed(ctx: &mut dyn NativeContext) -> cratonvm_types::error::MethodCallFailed {
    let detail = ctx.create_string("Stream closed");
    if let Ok(Some(Value::Object(Some(exc)))) = ctx.new_object_initialized(
        "java/io/IOException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(detail))],
    ) {
        return cratonvm_types::error::MethodCallFailed::ExceptionThrown(exc);
    }
    cratonvm_types::error::RuntimeError::IOException {
        message: "Stream closed".to_string(),
    }
    .into()
}

/// `ensureOpen()`: real `StreamEncoder.write*`/`flush*` all check this first
/// and throw `IOException("Stream closed")` once `closed` is true. The
/// previous version of this shim had no such check at all — a write after
/// `close()` silently no-op'd instead of throwing, which is what actually
/// hung `OutputStreamPublisherTests.closed()` (an uncaught `AssertionError`
/// from AssertJ's `assertThatIOException()` finding no exception killed the
/// executor worker thread before the `Flow.Subscriber` ever got a terminal
/// signal). Reads the REAL `closed` field by name.
fn ensure_open(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    let closed = ctx.get_field_by_name(this, "closed").as_int().unwrap_or(0) != 0;
    if closed {
        return Err(throw_stream_closed(ctx));
    }
    Ok(())
}

fn is_closed(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    ctx.get_field_by_name(this, "closed").as_int().unwrap_or(0) != 0
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
    let key = se_key(ctx, this);
    se_table()
        .lock()
        .unwrap()
        .get(&key)
        .map(|s| s.name.clone())
        .unwrap_or_else(|| "UTF-8".to_string())
}

/// For the BOM-prefixed charset names ("UTF-16"/"UTF-32"), returns the name
/// to actually hand to `engine::encode_chars`/`encode_chars_lossy` for THIS
/// write call: `name` unchanged (BOM included) the first time this stream
/// encodes anything, or the fixed big-endian variant ("UTF-16BE"/"UTF-32BE",
/// matching the endianness `encode_utf16_with_bom`/`encode_utf32_with_bom`
/// already commit to) on every call after that — see `SeState::bom_written`.
/// Any other charset name is returned unchanged.
fn effective_encode_name(ctx: &dyn NativeContext, this: ObjectRef, name: &str) -> String {
    let fixed_be = match name {
        "UTF-16" => "UTF-16BE",
        "UTF-32" => "UTF-32BE",
        _ => return name.to_string(),
    };
    let key = se_key(ctx, this);
    let mut table = se_table().lock().unwrap();
    let state = table.entry(key).or_default();
    if state.bom_written {
        fixed_be.to_string()
    } else {
        state.bom_written = true;
        name.to_string()
    }
}

pub(crate) fn alloc_stream_encoder(
    ctx: &mut dyn NativeContext,
    os: ObjectRef,
    charset_name: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    // GC-safety: `os` is a Rust local the caller extracted from its own args
    // slice before calling in, and `ensure_class_initialized` below runs
    // `<clinit>` bytecode — arbitrary, allocating — before `os` is finally
    // stored into the new object's `out` field. Same "Family 1" native-stale-
    // local pattern as `alloc_stream_decoder`'s matching fix (and the WildFly
    // Surefire-fork boot-crash fix in native-builtins/src/lang_class.rs) —
    // pin now, re-read right before use.
    let os_pin = ctx.pin_native_root(os);
    let cid = match ctx.ensure_class_initialized("sun/nio/cs/StreamEncoder") {
        Ok(c) => c,
        // See the matching fix in stream_decoder.rs's alloc_stream_decoder:
        // `ClassId::new(0)` is `java/lang/Object` (zero declared fields), so
        // falling back to it here would mint an encoder object with no
        // usable "out"/"closed" fields and no real `write`/`flush` methods,
        // the write-side sibling of the readLine `NoSuchMethodError:
        // java/lang/Object.read([CII)I` bug. The synthetic fallback retries
        // loading the real class first (this always succeeds for a real JDK
        // bootstrap class like this one) and only degrades to a stub with
        // the requested field count as a last resort.
        //
        // Fallible since 2026-08-10 (JDK-only wave 2, step 3); see the
        // matching note in `stream_decoder.rs` for why the race this arm
        // exists for is unaffected.
        Err(_) => crate::refused_class(ctx, "sun/nio/cs/StreamEncoder", 12)?,
    };
    // `alloc_object` clamps the slot count up to the resolved real class's
    // total declared instance-field count, so `0` here is fine — the object
    // ends up with every real `Writer`/`StreamEncoder` field, not just a
    // hand-picked scratch few (see module doc for why hardcoding a smaller
    // count and indexing into it corrupted real fields).
    let obj = ctx.alloc_object(cid, 0);
    let os = ctx.read_native_pin(os_pin, os);
    ctx.set_field_by_name(obj, "out", Value::Object(Some(os)));
    ctx.unpin_native_roots(os_pin);
    ctx.set_field_by_name(obj, "closed", Value::Int(0));
    let key = se_key(ctx, obj);
    se_table().lock().unwrap().insert(
        key,
        SeState {
            pending: Vec::with_capacity(INITIAL_BYTE_BUFFER_CAPACITY),
            capacity: INITIAL_BYTE_BUFFER_CAPACITY,
            name: charset_name.to_string(),
            bom_written: false,
        },
    );
    // No pending high surrogate yet (real-field carry, cleared explicitly).
    clear_pending(ctx, obj);
    Ok(obj)
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
    let se = alloc_stream_encoder(ctx, os, &name)?;
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
    let se = alloc_stream_encoder(ctx, os, &name)?;
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
    let se = alloc_stream_encoder(ctx, os, &name)?;
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
    if ctx
        .get_field_by_name(this, "haveLeftoverChar")
        .as_int()
        .unwrap_or(0)
        == 0
    {
        return None;
    }
    let u = (ctx
        .get_field_by_name(this, "leftoverChar")
        .as_int()
        .unwrap_or(0)
        & 0xFFFF) as u16;
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
    ensure_open(ctx, this)?;

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

    // GC: `encode_for_stream` reads the stream's `encoder` and asks it for its
    // malformed/unmappable actions through `invoke_virtual`, so it runs Java
    // and can collect on a path that returns normally. `this` is then handed
    // to `buffer_and_maybe_flush`.
    let name = name_of(ctx, this);
    let encode_name = effective_encode_name(ctx, this, &name);
    let pin = ctx.pin_native_root(this);
    let bytes = encode_for_stream(ctx, this, &encode_name, to_encode)?;
    if bytes.is_empty() {
        ctx.unpin_native_roots(pin);
        return Ok(());
    }
    let this = ctx.read_native_pin(pin, this);
    ctx.unpin_native_roots(pin);
    buffer_and_maybe_flush(ctx, this, &name, &bytes)?;
    Ok(())
}

/// Append `bytes` to this encoder's pending buffer (side-tabled — see the
/// module doc), growing it the way real `StreamEncoder.growByteBufferIfNeeded`
/// does, and flush the accumulated bytes to the underlying stream only when
/// the buffer is actually full. This reproduces the real JDK's write-call
/// granularity to the underlying `OutputStream` (batches of up to
/// `MAX_BYTE_BUFFER_CAPACITY` bytes) instead of one underlying `write` per
/// `Writer` call — see the module doc for why that granularity matters
/// (`NoBodyOutputStream.checkCommit`'s byte-count-based commit threshold).
///
/// `name` is accepted (unused beyond documentation intent) for symmetry with
/// the real method's signature, which estimates the grow target from
/// `maxBytesPerChar()`; here we already have the real encoded byte length so
/// no per-charset estimate is needed.
fn buffer_and_maybe_flush(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    _name: &str,
    bytes: &[u8],
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    let key = se_key(ctx, this);

    // A single write larger than the max buffer capacity is written straight
    // through (after flushing anything already pending), rather than
    // growing the buffer unboundedly — mirrors real StreamEncoder's overflow
    // handling, which never grows `bb` past `maxBufferCapacity`.
    if bytes.len() > MAX_BYTE_BUFFER_CAPACITY {
        // GC: `flush_pending_buffer` reaches `write_through`, which allocates a
        // byte array and calls `write` on the wrapped stream.
        let pin = ctx.pin_native_root(this);
        flush_pending_buffer(ctx, this)?;
        let this = ctx.read_native_pin(pin, this);
        ctx.unpin_native_roots(pin);
        return write_through(ctx, this, bytes);
    }

    let flushed = {
        let mut table = se_table().lock().unwrap();
        let state = table.entry(key).or_insert_with(|| SeState {
            pending: Vec::with_capacity(INITIAL_BYTE_BUFFER_CAPACITY),
            capacity: INITIAL_BYTE_BUFFER_CAPACITY,
            name: "UTF-8".to_string(),
            bom_written: false,
        });
        // Grow toward (but never past) MAX_BYTE_BUFFER_CAPACITY if the
        // incoming bytes wouldn't fit in the buffer's CURRENT capacity —
        // mirrors `growByteBufferIfNeeded` only reallocating when needed.
        if state.capacity < MAX_BYTE_BUFFER_CAPACITY && bytes.len() > state.capacity {
            state.capacity = bytes.len().min(MAX_BYTE_BUFFER_CAPACITY);
        }
        if bytes.len() > state.capacity.saturating_sub(state.pending.len()) {
            // Doesn't fit in the remaining space even after growth: flush
            // what's pending first, then place `bytes` into the now-empty
            // buffer (it fits, since bytes.len() <= MAX_BYTE_BUFFER_CAPACITY
            // <= capacity after growth, checked above).
            let flushed = std::mem::take(&mut state.pending);
            state.pending.extend_from_slice(bytes);
            Some(flushed)
        } else {
            state.pending.extend_from_slice(bytes);
            None
        }
    };
    if let Some(flushed) = flushed {
        write_through(ctx, this, &flushed)?;
    }

    // Deliberately do NOT flush here even if the buffer is now exactly full.
    // Real `StreamEncoder.implWrite` only calls `writeBytes()` when the NEXT
    // encode attempt overflows (`CoderResult.isOverflow()`) — an exactly-full
    // `bb` sits unflushed until something tries to add more to it. This
    // matters: a request that stops writing right when the buffer reaches
    // capacity (e.g. the last of N fixed-size `Writer` calls) never triggers
    // that extra flush on real HotSpot, so the underlying stream sees one
    // fewer batch than an eager "flush when full" implementation would
    // produce — which changes `NoBodyOutputStream.checkCommit`'s observed
    // byte count and the resulting commit timing (see the module doc).
    Ok(())
}

/// Perform the actual `OutputStream.write([BII)V` call. Split out from
/// [`buffer_and_maybe_flush`] so both the batched-flush path and the
/// oversize-direct-write fallback share one call site.
fn write_through(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    bytes: &[u8],
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    if bytes.is_empty() {
        return Ok(());
    }
    let os = match ctx.get_field_by_name(this, "out") {
        Value::Object(Some(s)) => s,
        _ => return Ok(()),
    };
    let buf = ctx.new_array(ArrayElementType::Byte, bytes.len());
    // AUDIT 2026-05-17: bulk write via NativeContext intrinsic.
    ctx.write_byte_array_from(buf, 0, bytes);
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

/// Flush any bytes sitting in the pending buffer to the underlying stream
/// (but do NOT flush/close the stream itself — callers do that). Used by
/// `flush()`/`flushBuffer()`/`close()`/`implClose()` so buffered bytes are
/// not lost when the caller expects them to have been delivered.
fn flush_pending_buffer(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    let key = se_key(ctx, this);
    let pending = {
        let mut table = se_table().lock().unwrap();
        match table.get_mut(&key) {
            Some(state) => std::mem::take(&mut state.pending),
            None => return Ok(()),
        }
    };
    write_through(ctx, this, &pending)
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
    ensure_open(ctx, this)?;
    // Deliver any bytes still sitting in the pending buffer before flushing
    // the underlying stream, or `flush()` would be a no-op from the caller's
    // point of view (real `StreamEncoder.implFlush` does the same:
    // `implFlushBuffer()` then `out.flush()`).
    // …and `implFlush` propagates that `out.flush()` — it is a two-line body
    // under `throws IOException` with no `catch`. Dropping the failure here
    // was the worst possible place for it: the caller flushed precisely to
    // learn whether the encoded bytes reached the sink.
    // W7-57-close-flush-swallow-sweep.md
    flush_pending_buffer(ctx, this)?;
    if let Value::Object(Some(os)) = ctx.get_field_by_name(this, "out") {
        ctx.invoke_virtual(os, "flush", "()V", &[])?;
    }
    Ok(None)
}

/// `close()` / `implClose()`. Idempotent, matching real
/// `StreamEncoder.close()` (`if (closed) return;`) — a second close must not
/// re-flush or re-close the underlying stream.
fn native_se_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(None),
    };
    if is_closed(ctx, this) {
        return Ok(None);
    }
    // End of input: any still-unpaired high surrogate can no longer be
    // completed, so flush it now as the replacement char (U+FFFD), matching
    // the JDK's encoder.encode(.., endOfInput=true) + flush at close. Done
    // before the stream is flushed/closed so the bytes actually go out.
    flush_pending_surrogate(ctx, this)?;
    // Deliver any bytes still sitting in the pending buffer — without this
    // the last partial batch (< buffer capacity) would be silently dropped
    // on close, matching real `StreamEncoder.implClose`'s final
    // `implFlushBuffer()` before closing `out`.
    //
    // Both delegations PROPAGATE. `StreamEncoder.implClose()` is
    // `try (out) { …; out.flush(); } catch (IOException x) { encoder.reset();
    // throw x; }` — the `catch` rethrows, and the `try`-with-resources runs
    // `out.close()` on both paths, suppressing its own failure into the body's
    // when there was one. `close()` wraps that in `try { implClose(); }
    // finally { closed = true; }`, so the closed marker is set either way.
    // Dropping the failures here reported a truncated file as a clean close.
    // W7-57-close-flush-swallow-sweep.md
    //
    // (The `addSuppressed` link between the two is not reproduced; recorded as
    // a residual in that record.)
    flush_pending_buffer(ctx, this)?;
    let (flushed, closed) = if let Value::Object(Some(os)) = ctx.get_field_by_name(this, "out") {
        let flushed = ctx.invoke_virtual(os, "flush", "()V", &[]).map(|_| ());
        // Attempted regardless, exactly as the `try`-with-resources does.
        let closed = ctx.invoke_virtual(os, "close", "()V", &[]).map(|_| ());
        (flushed, closed)
    } else {
        (Ok(()), Ok(()))
    };
    ctx.set_field_by_name(this, "closed", Value::Int(1));
    ctx.set_field_by_name(this, "out", Value::Object(None));
    se_table().lock().unwrap().remove(&se_key(ctx, this));
    flushed?;
    closed?;
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
    let os = match ctx.get_field_by_name(this, "out") {
        Value::Object(Some(s)) => s,
        _ => return Ok(()),
    };
    let name = name_of(ctx, this);
    let encode_name = effective_encode_name(ctx, this, &name);
    // Lossy encode of a lone surrogate yields the charset's replacement bytes
    // (U+FFFD → EF BF BD for UTF-8), exactly as HotSpot's REPLACE action does.
    let bytes = engine::encode_chars_lossy(&encode_name, &[hi]);
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

// JDK-ONLY-CLASSIFY: stub — mirror of `stream_decoder.rs`. `sun.nio.cs.
// StreamEncoder` declares no ACC_NATIVE method in JDK 25 and all 11 resolvable
// registrations shadow concrete bytecode. Same reasoning, same caveat: these
// stand in for unimplemented `sun.nio.ch` internals, so removing them from the
// strict path is only safe once that layer exists.
pub fn register_stream_encoder_natives(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    // RETAGGED to match this file's own JDK-ONLY-CLASSIFY verdict above,
    // which already reads "stub": `sun.nio.cs.StreamEncoder` declares no
    // ACC_NATIVE method on JDK 25 and every registration here shadows
    // concrete bytecode. Only the TAG disagreed with the classification.
    //
    // MEASURED 2026-08-19 (`--dump-native-registry`): registrations 12,
    // invocations 0, overwrote 0, and 0 ACC_NATIVE targets.
    //
    // The comment above says "do not DELETE before that layer is real";
    // this does not delete. Under `--jdk-only` a SyntheticStub is refused,
    // which is the "structured MissingNative from the sun.nio.ch layer"
    // that comment calls the honest outcome — instead of a silent charset
    // re-implementation standing in for it. Under --real-jdk nothing
    // changes: the stub still registers and still answers.
    registry.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
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
        let name = name_of(ctx, this);
        // The real `StreamEncoder.encodingName()` reports the HISTORICAL name
        // for any `HistoricallyNamedCharset`, which is most of `java.base`:
        // `new OutputStreamWriter(os, UTF_8).getEncoding()` is "UTF8".
        let s = ctx.create_string(engine::historical_charset_name(&name));
        Ok(Some(Value::Object(Some(s))))
    });
    registry.register(se, "isOpen", "()Z", |ctx, args| {
        let this = match obj_arg(args, 0) {
            Some(o) => o,
            None => return Ok(Some(Value::Int(0))),
        };
        Ok(Some(Value::Int(if is_closed(ctx, this) { 0 } else { 1 })))
    });
    registry.set_category(__prev_cat);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

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
    fn normalize_supported_accepts_engine_supported_charsets() {
        // These were rejected by the old stale private alias table even
        // though the engine encodes them (IBM850 hand-written, the CJK
        // families via encoding_rs) — the shared table accepts them.
        assert_eq!(
            normalize_supported("Shift_JIS").as_deref(),
            Some("Shift_JIS")
        );
        assert_eq!(normalize_supported("GBK").as_deref(), Some("GBK"));
        assert_eq!(normalize_supported("ibm850").as_deref(), Some("IBM850"));
        assert_eq!(normalize_supported("Cp850").as_deref(), Some("IBM850"));
    }

    #[test]
    fn normalize_supported_rejects_canonical_but_codecless_charsets() {
        // KOI8-U canonicalizes in the shared table but the engine has no
        // codec for it — the engine probe must still reject the name.
        assert_eq!(normalize_supported("KOI8-U"), None);
    }

    #[test]
    fn normalize_keeps_utf8_fallback_for_object_path() {
        // The infallible `normalize` (used only on the already-validated
        // Charset-object path) still maps unknowns to UTF-8.
        assert_eq!(normalize("NoSuchCharset-42"), "UTF-8");
    }
}
