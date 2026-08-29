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
//! ## Field access is by NAME, never by hardcoded index
//!
//! The SD object is allocated with the REAL `sun.nio.cs.StreamDecoder` class id
//! (so `InputStreamReader` bytecode dispatches `sd.read(...)` to our natives),
//! so it carries the real class's full (inherited + own) field layout. An
//! earlier version of this file addressed the underlying `InputStream` and its
//! side-table key via hardcoded absolute slot indices (`SD_INPUT = 0`,
//! `SD_ID = 4`), on the mistaken assumption that slot 0 is `StreamDecoder`'s
//! own `in` field. `javap` on the real class shows its declared field order is
//! `(closed, haveLeftoverChar, leftoverChar, cs, decoder, bb, in, ch)` — `in`
//! is the 7th field, not the 1st (and that's before even accounting for any
//! fields the object's absolute slot numbering inherits from `java.io.Reader`)
//! — so slot 0 is really `closed` (a primitive `boolean`) and slot 4 is really
//! `decoder` (a `CharsetDecoder` reference). Writing the `InputStream`
//! reference into slot 0 and an `int` id into slot 4 silently corrupted those
//! two real fields: a reference written where the real class's reference map
//! says a primitive lives is NOT relocated by a moving collector (the
//! `BufferedReader.in` reads-null-right-after-construction symptom — see
//! `fixed-suite-bugs/tomcat/form-authenticator-cookie-session-bare-assertion-FIXED.md`),
//! and conversely an `int` written where the map says a reference lives risks
//! the collector treating that bit pattern as a pointer. Fixed the same way
//! `stream_encoder.rs` fixed the analogous `StreamEncoder` corruption:
//! resolve the one real field this shim legitimately owns (`in`) **by name**
//! (`get_field_by_name`/`set_field_by_name`, which walk the real class's field
//! metadata instead of trusting a hand-counted index), and keep the side-table
//! key off any object field entirely — `ctx.identity_hash_code` instead of a
//! scratch primitive slot.
//!
//! Each `read` decodes the complete prefix of (carry + freshly-read bytes)
//! straight into the caller's `char[]` and carries the trailing incomplete
//! byte sequence (UTF-8 lead/continuation bytes, UTF-16 odd dangling byte)
//! to the next call, so multi-byte boundaries are preserved. No decoded
//! read-ahead `char[]` is buffered in an object field (such a field is not
//! in the real StreamDecoder reference map, so the collector would free it
//! mid-stream — the cause of the prior readLine hang). The mutable per-decoder
//! state (charset name + the incomplete-byte carry) lives in a Rust side-table
//! keyed by the object's stable identity hash; the underlying `InputStream` is
//! re-read from the real `in` field on every call (never cached in Rust, so GC
//! motion is transparent).

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult};
use cratonvm_types::{ArrayElementType, ObjectRef, Value};

use std::collections::VecDeque;

use cratonvm_native_api::charset as engine;

struct SdState {
    name: String,
    carry: Vec<u8>,
    /// Decoded read-ahead characters.  This deliberately lives beside `carry`
    /// rather than in a synthetic Java field: the real StreamDecoder layout
    /// has no collector-visible slot for it.  In particular, `Reader.read()`
    /// asks StreamDecoder for one or two chars at a time, so retaining a bulk
    /// refill here prevents resource parsers from turning every input byte into
    /// a separate native/virtual round trip.
    pending: VecDeque<u16>,
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

fn obj_arg(args: &[Value], i: usize) -> Option<ObjectRef> {
    match args.get(i) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    }
}

fn int_arg(args: &[Value], i: usize) -> i32 {
    args.get(i).and_then(|v| v.as_int()).unwrap_or(0)
}

/// Stable per-object side-table key. Unlike a monotonic counter stashed in a
/// scratch field slot (the previous, corrupting approach — see module doc),
/// this touches no real field at all.
fn sd_key(ctx: &dyn NativeContext, this: ObjectRef) -> i32 {
    ctx.identity_hash_code(this)
}

/// Allocate and return a synthetic StreamDecoder wrapping `is`.
pub(crate) fn alloc_stream_decoder(
    ctx: &mut dyn NativeContext,
    is: ObjectRef,
    charset_name: &str,
    prop: Option<PropState>,
) -> Result<ObjectRef, MethodCallFailed> {
    // GC-safety: `is` is a Rust local the caller extracted from its own args
    // slice before calling in (see `native_sd_for_isr_charset`/`_name`), and
    // `ensure_class_initialized` below runs `<clinit>` bytecode — arbitrary,
    // allocating — before `is` is finally stored into the new object's `in`
    // field. Per the `pin_native_root` contract, a moving GC in that window
    // leaves `is` stale (resolving to whatever now occupies the reused slot),
    // silently corrupting the StreamDecoder's own `in` field. Same "Family 1"
    // native-stale-local pattern as the WildFly Surefire-fork boot-crash fix
    // in `native-builtins/src/lang_class.rs` — pin now, re-read right before
    // use.
    let is_pin = ctx.pin_native_root(is);
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
        // documented synthetic fallback instead -- it retries the real load
        // first and otherwise returns a class that actually declares the
        // requested fields, so the object stays usable even on the rare
        // initialization race.
        //
        // Fallible since 2026-08-10 (JDK-only wave 2, step 3), and the
        // race this arm exists for is unaffected: the fallible spelling
        // still re-attempts `load_class_concurrent` first, so a transient
        // `<clinit>` failure resolves to the REAL `sun.nio.cs.StreamDecoder`
        // exactly as before. Only a genuine "this class is nowhere" reaches
        // the policy, and there `--jdk-only` refusing is the point.
        Err(_) => crate::refused_class(ctx, "sun/nio/cs/StreamDecoder", 9)?,
    };
    // `alloc_object` clamps the slot count up to the resolved real class's
    // total declared instance-field count, so `0` here is fine — the object
    // ends up with every real `Reader`/`StreamDecoder` field, not a
    // hand-picked scratch few (see module doc for why hardcoding a smaller
    // count and indexing into it corrupted real fields).
    let obj = ctx.alloc_object(cid, 0);
    let is = ctx.read_native_pin(is_pin, is);
    ctx.set_field_by_name(obj, "in", Value::Object(Some(is)));
    ctx.unpin_native_roots(is_pin);
    let key = sd_key(ctx, obj);
    sd_table().lock().unwrap().insert(
        key,
        SdState {
            name: charset_name.to_string(),
            carry: Vec::new(),
            pending: VecDeque::new(),
            prop,
        },
    );
    Ok(obj)
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
    let sd = alloc_stream_decoder(ctx, is, &name, prop)?;
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
    let sd = alloc_stream_decoder(ctx, is, &norm, None)?;
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
    // AUDIT 2026-07-26 (native-io-audit): drain the read-ahead queue FIRST.
    // This native used to go straight to `decode_into`, ignoring
    // `SdState::pending` entirely, so interleaving `read(char[],int,int)`
    // (which fills `pending`) with `read()` on the same reader skipped every
    // queued character and returned the ones *after* them — out-of-order
    // delivery, then loss at `close()`. It is also what makes the surplus
    // stashed by `decode_into` reachable on the single-char path.
    {
        let key = sd_key(ctx, this);
        let mut table = sd_table().lock().unwrap();
        if let Some(state) = table.get_mut(&key) {
            if let Some(ch) = state.pending.pop_front() {
                return Ok(Some(Value::Int(ch as i32)));
            }
        }
    }
    // GC-safety: `decode_into` allocates and re-enters Java
    // (`InputStream.read`), either of which can run a moving GC, and this
    // loop re-uses `this` and `out` across those windows. Pin both and
    // re-read the current address through the pin before every use.
    let this_pin = ctx.pin_native_root(this);
    let out = ctx.new_array(ArrayElementType::Char, 1);
    let out_pin = ctx.pin_native_root(out);
    // `decode_into` always consumes ≥1 fresh byte when the carry alone can't
    // form a char, so the loop makes progress and terminates (a full char
    // needs ≤4 bytes; EOF flushes). Bounded for safety.
    let result: MethodCallResult = (|| {
        for _ in 0..8 {
            let cur_this = ctx.read_native_pin(this_pin, this);
            let cur_out = ctx.read_native_pin(out_pin, out);
            let n = decode_into(ctx, cur_this, cur_out, 0, 1)?;
            if n > 0 {
                let cur_out = ctx.read_native_pin(out_pin, out);
                let ch = match ctx.get_array_element(cur_out, 0) {
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
    })();
    // Releases `out_pin` too — `unpin_native_roots` truncates the pin stack
    // to `base`, and `this_pin` was taken first.
    ctx.unpin_native_roots(this_pin);
    result
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

    let key = sd_key(ctx, this);
    let mut chars = Vec::with_capacity(len);
    {
        let mut table = sd_table().lock().unwrap();
        if let Some(state) = table.get_mut(&key) {
            while chars.len() < len {
                match state.pending.pop_front() {
                    Some(ch) => chars.push(ch),
                    None => break,
                }
            }
        }
    }
    if chars.len() == len {
        ctx.write_char_array_from(out, off, &chars);
        return Ok(Some(Value::Int(chars.len() as i32)));
    }

    // A StreamDecoder `read()` call is implemented by the real JDK bytecode
    // in terms of `read(char[], 0, 2)`.  Refilling only those two characters
    // makes a configuration file's comment tokenizer do one full JNI/native
    // round trip for every byte.  Read a normal chunk and retain the surplus
    // above, while preserving the existing no-read-ahead Java-field invariant.
    const READ_AHEAD_CHARS: usize = 4096;
    let request = (len - chars.len()).max(READ_AHEAD_CHARS);
    let this_pin = ctx.pin_native_root(this);
    let out_pin = ctx.pin_native_root(out);
    let result: MethodCallResult = (|| {
        let tmp = ctx.new_array(ArrayElementType::Char, request);
        let tmp_pin = ctx.pin_native_root(tmp);
        let cur_this = ctx.read_native_pin(this_pin, this);
        let cur_tmp = ctx.read_native_pin(tmp_pin, tmp);
        let n = decode_into(ctx, cur_this, cur_tmp, 0, request)?;
        let tmp = ctx.read_native_pin(tmp_pin, tmp);
        ctx.unpin_native_roots(tmp_pin);

        if n > 0 {
            let mut fetched = Vec::with_capacity(n as usize);
            for index in 0..n as usize {
                if let Value::Int(ch) = ctx.get_array_element(tmp, index) {
                    fetched.push((ch & 0xffff) as u16);
                }
            }
            let take = (len - chars.len()).min(fetched.len());
            chars.extend_from_slice(&fetched[..take]);
            if take < fetched.len() {
                let mut table = sd_table().lock().unwrap();
                let state = table.entry(key).or_insert_with(|| SdState {
                    name: "UTF-8".to_string(),
                    carry: Vec::new(),
                    pending: VecDeque::new(),
                    prop: None,
                });
                state.pending.extend(fetched[take..].iter().copied());
            }
        }

        if !chars.is_empty() {
            let cur_out = ctx.read_native_pin(out_pin, out);
            ctx.write_char_array_from(cur_out, off, &chars);
            Ok(Some(Value::Int(chars.len() as i32)))
        } else {
            Ok(Some(Value::Int(n)))
        }
    })();
    ctx.unpin_native_roots(this_pin);
    ctx.unpin_native_roots(out_pin);
    result
}

/// `close()` — closes the underlying InputStream and drops side-table state.
fn native_sd_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(None),
    };
    // GC-safety: `close()` re-enters Java; `this` is used again afterward
    // (field clear + side-table key), so pin it across the call and re-read
    // the current address before those uses.
    //
    // The delegated close PROPAGATES. `StreamDecoder.implClose()` is exactly
    // `if (ch != null) ch.close(); else in.close();` under
    // `throws IOException`, and `close()` calls it inside a `try` whose
    // `finally` only sets `closed = true` — there is no `catch`.
    // W7-57-close-flush-swallow-sweep.md
    //
    // The `closed` marker and the side-table drop still run on the failing
    // path, matching that `finally`, and the failure is reported after.
    let this_pin = ctx.pin_native_root(this);
    let closed = if let Value::Object(Some(is)) = ctx.get_field_by_name(this, "in") {
        ctx.invoke_virtual(is, "close", "()V", &[]).map(|_| ())
    } else if let Value::Object(Some(ch)) = ctx.get_field_by_name(this, "ch") {
        // Channel-backed decoder (`Channels.newReader(ReadableByteChannel, ...)`)
        // — no InputStream exists, close the channel instead so a FileChannel
        // opened for e.g. a Flyway migration script isn't leaked.
        ctx.invoke_virtual(ch, "close", "()V", &[]).map(|_| ())
    } else {
        Ok(())
    };
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    ctx.set_field_by_name(this, "in", Value::Object(None));
    let key = sd_key(ctx, this);
    sd_table().lock().unwrap().remove(&key);
    closed?;
    Ok(None)
}

/// `ready() -> boolean`.
fn native_sd_ready(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Int(0))),
    };
    let key = sd_key(ctx, this);
    // AUDIT 2026-07-26 (native-io-audit): `pending` holds FULLY DECODED
    // read-ahead chars — those are unconditionally ready. Only `carry` (an
    // incomplete byte sequence) was checked here, so a reader whose surplus
    // sat in `pending` while the underlying stream had `available() == 0`
    // reported `ready() == false` with a character immediately deliverable.
    let has_buffered = sd_table()
        .lock()
        .unwrap()
        .get(&key)
        .map(|s| !s.carry.is_empty() || !s.pending.is_empty())
        .unwrap_or(false);
    if has_buffered {
        return Ok(Some(Value::Int(1)));
    }
    if let Value::Object(Some(is)) = ctx.get_field_by_name(this, "in") {
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
    let key = sd_key(ctx, this);
    let (name, mut bytes, prop) = {
        let t = sd_table().lock().unwrap();
        match t.get(&key) {
            Some(s) => (s.name.clone(), s.carry.clone(), s.prop),
            // No side-table entry: this decoder was built by the real (never
            // intercepted at construction) `StreamDecoder.forDecoder(ReadableByteChannel,
            // CharsetDecoder, int)` factory that `Channels.newReader` uses — read
            // the actual charset off the object's own real `cs` field instead of
            // assuming UTF-8, so a non-UTF-8 `Channels.newReader(ch, decoder, cap)`
            // decodes correctly too.
            None => {
                let resolved = match ctx.get_field_by_name(this, "cs") {
                    Value::Object(Some(cs)) => {
                        let n = resolve_name(ctx, Some(cs), None);
                        if n.is_empty() {
                            "UTF-8".to_string()
                        } else {
                            n
                        }
                    }
                    _ => "UTF-8".to_string(),
                };
                (resolved, Vec::new(), None)
            }
        }
    };

    // GC-safety: the refill below allocates (`new_array`) and re-enters Java
    // (`InputStream.read`), either of which can run a moving GC — and `this`,
    // `out`, and the temporary refill buffer are all used after those
    // windows. Pin each and re-read the current address through the pin
    // before every post-window use (the WildFly domain-boot server-output
    // reader threads tripped the CRATONVM_DBG_STALE_OBJREF canary exactly
    // here — see wildfly-domain-hc0053-server-inventory-timeout.md).
    let this_pin = ctx.pin_native_root(this);
    let out_pin = ctx.pin_native_root(out);

    // Keep total bytes ≤ len so decoded chars ≤ len. Force ≥1 fresh byte when
    // the carry alone fills `len` (tiny len) so we always make progress.
    let mut want = len.saturating_sub(bytes.len());
    if want == 0 && !bytes.is_empty() {
        want = 4;
    }
    let mut eof = false;
    if want > 0 {
        if matches!(ctx.get_field_by_name(this, "in"), Value::Object(Some(_))) {
            // Allocate the buffer BEFORE resolving the stream reference: the
            // allocation itself can move `this` (and its `in` referent), so
            // the stream is re-read through the refreshed `this` right before
            // the call, leaving no GC window between the read and its use.
            let tmp = ctx.new_array(ArrayElementType::Byte, want);
            let tmp_pin = ctx.pin_native_root(tmp);
            let cur_this = ctx.read_native_pin(this_pin, this);
            let r = match ctx.get_field_by_name(cur_this, "in") {
                Value::Object(Some(is)) => ctx.invoke_virtual(
                    is,
                    "read",
                    "([BII)I",
                    &[
                        Value::Object(Some(tmp)),
                        Value::Int(0),
                        Value::Int(want as i32),
                    ],
                ),
                _ => Ok(Some(Value::Int(-1))),
            };
            // Re-read the buffer through its pin before touching it: the
            // (potentially blocking) read above is exactly where a
            // peer-initiated moving GC runs.
            let tmp = ctx.read_native_pin(tmp_pin, tmp);
            ctx.unpin_native_roots(tmp_pin);
            let n = match r {
                Ok(Some(Value::Int(v))) => v,
                Ok(_) => -1,
                Err(e) => {
                    ctx.unpin_native_roots(this_pin);
                    return Err(e);
                }
            };
            if n > 0 {
                let start = bytes.len();
                bytes.resize(start + n as usize, 0);
                ctx.read_byte_array_into(tmp, 0, &mut bytes[start..]);
            } else {
                eof = true;
            }
        } else if matches!(ctx.get_field_by_name(this, "ch"), Value::Object(Some(_))) {
            // `java.nio.channels.Channels.newReader(ReadableByteChannel, ...)`
            // builds a StreamDecoder via the real (unshimmed)
            // `StreamDecoder.forDecoder` factory, which sets the real `ch`
            // field instead of `in` — no InputStream exists at all. Without
            // this branch every such decoder read `in` as null and fell
            // straight to the `eof = true` case below, so `readLine()`
            // returned null on the very first call: Flyway's
            // `FileSystemResource.read()` (which wraps a `FileChannel` this
            // way) silently saw an empty migration script and reported
            // "successfully applied" a migration that created zero tables.
            // See fixed-suite-bugs/springboot/quartzautoconfigurationtests-jdbc-jobstore-not-applied-FIXED.md.
            let bb = crate::alloc_byte_buffer(ctx, want);
            let bb_pin = ctx.pin_native_root(bb);
            let cur_this = ctx.read_native_pin(this_pin, this);
            let r = match ctx.get_field_by_name(cur_this, "ch") {
                Value::Object(Some(ch)) => {
                    let cur_bb = ctx.read_native_pin(bb_pin, bb);
                    ctx.invoke_virtual(
                        ch,
                        "read",
                        "(Ljava/nio/ByteBuffer;)I",
                        &[Value::Object(Some(cur_bb))],
                    )
                }
                _ => Ok(Some(Value::Int(-1))),
            };
            let n = match r {
                Ok(Some(Value::Int(v))) => v,
                Ok(_) => -1,
                Err(e) => {
                    ctx.unpin_native_roots(bb_pin);
                    ctx.unpin_native_roots(this_pin);
                    return Err(e);
                }
            };
            if n > 0 {
                // The channel read filled the buffer's backing array from
                // offset 0 (a freshly allocated buffer starts at position 0).
                let cur_bb = ctx.read_native_pin(bb_pin, bb);
                if let Value::Object(Some(arr)) = ctx.get_field_by_name(cur_bb, "hb") {
                    let start = bytes.len();
                    bytes.resize(start + n as usize, 0);
                    ctx.read_byte_array_into(arr, 0, &mut bytes[start..]);
                }
            } else {
                eof = true;
            }
            ctx.unpin_native_roots(bb_pin);
        } else {
            eof = true;
        }
    }

    if bytes.is_empty() {
        // NOTE: `unpin_native_roots` TRUNCATES the thread's pin stack to
        // `base`, so releasing `this_pin` (taken first) also releases
        // `out_pin` and any nested pin. No separate `out_pin` release is
        // needed on any exit path here — verified against
        // `vm/src/vm/vm_exec.rs::unpin_native_roots`.
        ctx.unpin_native_roots(this_pin);
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
        // Re-read the destination through its pin: the refill window above
        // may have moved it.
        let cur_out = ctx.read_native_pin(out_pin, out);
        ctx.write_char_array_from(cur_out, off, &chars[..ncopy]);
    }
    ctx.unpin_native_roots(this_pin);

    // Persist the incomplete trailing bytes (and any updated property-decoder
    // fallback state) for the next call.
    {
        let mut t = sd_table().lock().unwrap();
        let entry = t.entry(key).or_insert_with(|| SdState {
            name: name.clone(),
            carry: Vec::new(),
            pending: VecDeque::new(),
            prop: None,
        });
        entry.carry = rest;
        // AUDIT 2026-07-26 (native-io-audit): chars beyond `ncopy` used to be
        // DROPPED here — not written to `out`, not carried in `rest` (which
        // only holds *undecoded* bytes). The doc comment above claims the
        // total byte count is kept <= `len` so this cannot happen, but the
        // `want = 4` progress-forcing branch (~line 579) deliberately breaks
        // that invariant whenever the carry alone already fills `len`. That
        // is the normal state of `StreamDecoder.read()` (len == 1) the moment
        // it meets a multi-byte character:
        //
        //   UTF-8 "eabcd" with a leading 2-byte 'e-acute' (C3 A9 61 62 63 64)
        //     call 1: len=1, want=1 -> reads C3, incomplete, carry=[C3], ret 0
        //     call 2: carry fills len -> want=4 -> reads A9 61 62 63
        //             chars = ['e-acute','a','b','c'], ncopy = 1
        //             -> 'a','b','c' vanished, and `rest` is empty so the
        //                carry could not hold them either
        //   Reader.read() therefore yields "e-acute" then 'd': three
        //   characters lost silently, no exception, no short-read signal.
        //
        // `SdState::pending` already exists for exactly this purpose (see
        // `native_sd_read_chars`), so stash the surplus there. Both readers
        // drain it before pulling fresh bytes, which keeps stream order.
        if chars.len() > ncopy {
            entry.pending.extend(chars[ncopy..].iter().copied());
        }
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

// JDK-ONLY-CLASSIFY: stub — `sun.nio.cs.StreamDecoder` declares no ACC_NATIVE
// method in JDK 25; all 9 resolvable registrations here shadow concrete
// bytecode. The comment at the call site in `register_io_natives` is explicit
// that these exist to "override the JDK bytecode that reaches into
// unimplemented sun.nio.ch internals" — that is a compatibility shim for a gap
// in this VM, which is exactly what jdk-only-native-review.md calls a valid
// classification and an invalid destination. Under `--jdk-only` the honest
// outcome is a structured `MissingNative` from the sun.nio.ch layer, not a
// silent charset re-implementation. Do not delete before that layer is real.
/// Register the StreamDecoder natives on the registry.
pub fn register_stream_decoder_natives(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    // RETAGGED to match this file's own JDK-ONLY-CLASSIFY verdict above,
    // which already reads "stub": `sun.nio.cs.StreamDecoder` declares no
    // ACC_NATIVE method on JDK 25 and every registration here shadows
    // concrete bytecode. Only the TAG disagreed with the classification.
    //
    // MEASURED 2026-08-19 (`--dump-native-registry`): registrations 10,
    // invocations 0, overwrote 0, and 0 ACC_NATIVE targets.
    //
    // The comment above says "do not DELETE before that layer is real";
    // this does not delete. Under `--jdk-only` a SyntheticStub is refused,
    // which is the "structured MissingNative from the sun.nio.ch layer"
    // that comment calls the honest outcome — instead of a silent charset
    // re-implementation standing in for it. Under --real-jdk nothing
    // changes: the stub still registers and still answers.
    registry.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
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
        let open = matches!(ctx.get_field_by_name(this, "in"), Value::Object(Some(_)));
        Ok(Some(Value::Int(if open { 1 } else { 0 })))
    });
    registry.register(sd, "getEncoding", "()Ljava/lang/String;", |ctx, args| {
        let this = match obj_arg(args, 0) {
            Some(o) => o,
            None => return Ok(Some(Value::Object(None))),
        };
        let key = sd_key(ctx, this);
        let name = sd_table()
            .lock()
            .unwrap()
            .get(&key)
            .map(|s| s.name.clone())
            .unwrap_or_else(|| "UTF-8".to_string());
        // The real `StreamDecoder.encodingName()` reports the HISTORICAL name
        // for any `HistoricallyNamedCharset`, which is most of `java.base`:
        // `new InputStreamReader(in, UTF_8).getEncoding()` is "UTF8".
        let s = ctx.create_string(engine::historical_charset_name(&name));
        Ok(Some(Value::Object(Some(s))))
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

    // --- AUDIT 2026-07-26 (native-io-audit) regressions ---
    //
    // These drive the real natives through `MockNativeContext`, whose
    // `read([BII)I` is scripted to behave like a genuine `InputStream`
    // (`script_input_stream`). `sd_table` is a process-global keyed by
    // identity hash, and the mock hands out low, reusable pointers, so these
    // tests serialize on `confine_test_lock` and clear their own key first.

    use crate::test_support::{confine_test_lock, MockNativeContext};

    /// Build a StreamDecoder-shaped mock object over `bytes`, with a fresh
    /// `sd_table` entry for `charset`.
    fn decoder_over(ctx: &mut MockNativeContext, bytes: &[u8], charset: &str) -> ObjectRef {
        let is = ctx.alloc_object(1);
        let this = ctx.alloc_object(4);
        ctx.set_field_by_name(this, "in", Value::Object(Some(is)));
        ctx.script_input_stream(bytes);
        let key = sd_key(ctx, this);
        let mut t = sd_table().lock().unwrap();
        t.remove(&key);
        t.insert(
            key,
            SdState {
                name: charset.to_string(),
                carry: Vec::new(),
                pending: VecDeque::new(),
                prop: None,
            },
        );
        drop(t);
        this
    }

    fn drain_reader(ctx: &mut MockNativeContext, this: ObjectRef) -> String {
        let mut out = String::new();
        for _ in 0..64 {
            match native_sd_read(ctx, &[Value::Object(Some(this))]) {
                Ok(Some(Value::Int(-1))) => break,
                Ok(Some(Value::Int(c))) => out.push(char::from_u32(c as u32).unwrap_or('\u{fffd}')),
                _ => break,
            }
        }
        out
    }

    /// The headline defect: `decode_into` truncated its decoded chars to
    /// `len` and dropped the surplus, so single-char `Reader.read()` over a
    /// stream containing a multi-byte character silently lost every character
    /// decoded alongside it. "éabcd" used to read back as "éd".
    #[test]
    fn audit_read_single_char_does_not_drop_surplus_after_multibyte() {
        let _g = confine_test_lock().lock();
        let mut ctx = MockNativeContext::new();
        // C3 A9 = 'é', then plain ASCII.
        let this = decoder_over(&mut ctx, "éabcd".as_bytes(), "UTF-8");
        let got = drain_reader(&mut ctx, this);
        assert_eq!(
            got, "éabcd",
            "every character must survive; the surplus decoded alongside \
             the multi-byte char used to be discarded"
        );
        sd_table().lock().unwrap().remove(&sd_key(&ctx, this));
    }

    /// Two multi-byte characters back to back — exercises the carry AND the
    /// surplus path together.
    #[test]
    fn audit_read_single_char_handles_consecutive_multibyte() {
        let _g = confine_test_lock().lock();
        let mut ctx = MockNativeContext::new();
        let this = decoder_over(&mut ctx, "中文ok".as_bytes(), "UTF-8");
        let got = drain_reader(&mut ctx, this);
        assert_eq!(got, "中文ok");
        sd_table().lock().unwrap().remove(&sd_key(&ctx, this));
    }

    /// `read()` must consume the read-ahead queue that `read(char[],int,int)`
    /// fills, rather than pulling fresh bytes past it.
    #[test]
    fn audit_read_single_char_drains_pending_before_refilling() {
        let _g = confine_test_lock().lock();
        let mut ctx = MockNativeContext::new();
        let this = decoder_over(&mut ctx, b"abcdef", "UTF-8");
        // Ask for 2 chars; the bulk path reads ahead and parks the rest.
        let out = ctx.new_array(ArrayElementType::Char, 2);
        let n = native_sd_read_chars(
            &mut ctx,
            &[
                Value::Object(Some(this)),
                Value::Object(Some(out)),
                Value::Int(0),
                Value::Int(2),
            ],
        );
        assert!(matches!(n, Ok(Some(Value::Int(2)))));
        let queued = sd_table()
            .lock()
            .unwrap()
            .get(&sd_key(&ctx, this))
            .map(|s| s.pending.len())
            .unwrap_or(0);
        assert!(queued > 0, "bulk read should have parked read-ahead chars");
        // The single-char reader must continue from 'c', not skip the queue.
        let rest = drain_reader(&mut ctx, this);
        assert_eq!(rest, "cdef");
        sd_table().lock().unwrap().remove(&sd_key(&ctx, this));
    }

    /// `ready()` must report true while fully decoded chars sit in `pending`,
    /// even though `carry` is empty and the stream has nothing left.
    #[test]
    fn audit_ready_accounts_for_decoded_read_ahead() {
        let _g = confine_test_lock().lock();
        let mut ctx = MockNativeContext::new();
        let this = decoder_over(&mut ctx, b"abcdef", "UTF-8");
        let out = ctx.new_array(ArrayElementType::Char, 1);
        let _ = native_sd_read_chars(
            &mut ctx,
            &[
                Value::Object(Some(this)),
                Value::Object(Some(out)),
                Value::Int(0),
                Value::Int(1),
            ],
        );
        // Stream is drained; only `pending` holds the remaining chars.
        let r = native_sd_ready(&mut ctx, &[Value::Object(Some(this))]);
        assert!(
            matches!(r, Ok(Some(Value::Int(1)))),
            "ready() must be true with decoded chars buffered"
        );
        sd_table().lock().unwrap().remove(&sd_key(&ctx, this));
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
