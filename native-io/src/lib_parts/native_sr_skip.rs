#[allow(unused_imports)]
use super::super::*;

pub(crate) fn native_sr_skip(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    let n = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let key = io_side_key(ctx, this);
    let mut table = sr_state().lock();
    let state = sr_row(&mut table, key, io_side_owner(this))?;
    // `StringReader.skip` is the one `skip` in `java.io` that accepts a
    // negative argument, and the lower half of `n.clamp(0, remaining)` deleted
    // that whole half of the contract: "The n parameter may be negative, even
    // though the skip method of the Reader superclass throws an exception in
    // this case. Negative values of n cause the stream to skip backwards.
    // Negative return values indicate a skip backwards. It is not possible to
    // skip backwards past the beginning of the string."
    //
    // Clamping to 0 answered "skipped nothing" — a legal, unremarkable return
    // — for a rewind the caller had every right to expect, so a lookahead
    // parser that skips forward and then backs up read the same region twice
    // rather than the region before it. The real body is
    // `r = Math.min(length - next, n); r = Math.max(-next, r);` behind the
    // "If the entire string has been read or skipped, then this method has no
    // effect and always returns 0" guard, which is what this now mirrors.
    let pos = state.pos as i64;
    let length = state.units.len() as i64;
    if pos >= length {
        return Ok(Some(Value::Long(0)));
    }
    let skip = (length - pos).min(n).max(-pos);
    state.pos = (pos + skip) as usize;
    Ok(Some(Value::Long(skip)))
}

/// `StringReader.mark(int)` — stash the current position so `reset()` can come
/// back to it.
///
/// This had NO registration at all, while `markSupported()` next to it answered
/// `true`. In real-JDK mode the unshadowed `mark(I)V` ran real JDK 25 bytecode,
/// which pokes the `private final Reader r` delegate that our `<init>` native
/// never creates; in `synthetic-jdk` mode there was no `mark` to run. Either way
/// `reset()` below then rewound to 0 instead of the mark, so any consumer that
/// does the standard `mark(n) / read-ahead / reset()` probe (BufferedReader,
/// javax.xml's encoding sniffers, JSON/CSV lookahead parsers) silently re-read
/// the stream from the beginning and duplicated everything before the mark.
pub(crate) fn native_sr_mark(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // Real `StringReader.mark` rejects a negative read-ahead limit before it
    // checks whether the stream is open.
    let read_ahead_limit = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    if read_ahead_limit < 0 {
        return Err(RuntimeError::IllegalArgumentException {
            message: "Read-ahead limit < 0".to_string(),
        }
        .into());
    }
    let key = io_side_key(ctx, this);
    let mut table = sr_state().lock();
    // The limit itself is deliberately ignored: the whole string is already
    // buffered, so — exactly like the real `StringReader` — a mark never
    // expires no matter how far the caller reads past it.
    let state = sr_row(&mut table, key, io_side_owner(this))?;
    state.mark = state.pos;
    Ok(None)
}

pub(crate) fn native_sr_reset(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let key = io_side_key(ctx, this);
    let mut table = sr_state().lock();
    // Rewind to the last `mark()`, which is 0 when none was ever taken —
    // the JDK's documented "reset to the beginning if the stream has never
    // been marked" behaviour. This used to hardcode 0 unconditionally.
    let state = sr_row(&mut table, key, io_side_owner(this))?;
    state.pos = state.mark;
    Ok(None)
}

/// `StringReader.close()` — the real one drops its source, after which
/// `ensureOpen()` makes every `read`/`ready`/`skip`/`reset` throw
/// `IOException("Stream closed")`. The no-op that used to be registered here
/// left the reader fully readable AND leaked the decoded content: `SR_STATE` is
/// keyed by identity hash and nothing else ever evicts from it, so every
/// StringReader ever built kept its `Vec<u16>` alive for the whole process.
/// Removing the entry does both jobs at once — the storage goes, and the
/// now-missing entry is exactly what the accessors read as "closed".
///
/// A missing entry can only mean "closed": `<init>(String)` is registered
/// unconditionally and is the only constructor `java.io.StringReader` has, so
/// every instance gets an entry before any read can happen, and this is the
/// only thing that removes the entry of a reader that can still be called. (Since
/// gc-common w15-c the post-collection sweep also removes the entry of a reader
/// no one can reach any more, and VM teardown removes a disposed VM's; neither
/// can be observed by a `read`.) A tombstone would have defeated the point: the
/// entry has to actually go.
///
/// Idempotent: a second `close()` finds nothing to remove and returns quietly.
pub(crate) fn native_sr_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let key = io_side_row_key(ctx, this);
    sr_state().lock().remove(&key);
    Ok(None)
}

/// `java.io.Reader.close()` is ABSTRACT in the real JDK, so this can never fire
/// for real bytecode: a concrete Reader has to declare `close` itself, and the
/// native-override lookup stops at the receiver's own method (and, failing
/// that, at the first ancestor that has bytecode for it). It IS reachable in
/// synthetic mode — a synthetic stub class declares no methods at all, so the
/// superclass walk runs to whatever `jdk_superclass` gave it, and
/// `BufferedReader` / `InputStreamReader` / `FileReader` all chain through
/// `java/io/Reader` there. Both of those stub layouts park the wrapped source in
/// a field named `in` (`synthetic_stub_fields`, classloading/src/
/// class_manager.rs), so do what every java.io decorator's `close` does and
/// close what is being wrapped, instead of dropping it on the floor. A reader
/// with no `in` field owns nothing downstream and correctly does nothing.
/// Clearing the field keeps `close()` idempotent.
pub(crate) fn native_reader_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    if let Value::Object(Some(inner)) = ctx.get_field_by_name(this, "in") {
        // Clear BEFORE dispatching. The nested `close()` runs arbitrary Java, so
        // a moving young GC there can relocate `this` and leave a write made
        // afterwards pointing at a stale address (native stale-local family).
        // Clearing first also keeps `close()` idempotent when the nested call
        // throws.
        // The delegated close PROPAGATES. Every `java.io` decorator whose
        // `close()` this stands in for — `FilterReader` (`in.close()`),
        // `BufferedReader` (`in.close()` in a `try`/`finally` that only nulls
        // fields), `InputStreamReader` (`sd.close()`) — declares
        // `throws IOException` and catches nothing.
        // W7-57-close-flush-swallow-sweep.md
        ctx.set_field_by_name(this, "in", Value::Object(None));
        ctx.invoke_virtual(inner, "close", "()V", &[])?;
    }
    Ok(None)
}

pub(crate) fn native_sw_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // GC: rooted across the call below; `safe_native_call` releases
    // the pin stack to its entry floor on return.
    let this_pin = ctx.pin_native_root(this);
    let buf = ctx.new_array(cratonvm_types::ArrayElementType::Char, 32);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field(this, SW_FIELD_BUF, Value::Object(Some(buf)));
    sw_set_count(ctx, this, 0);
    Ok(None)
}

pub(crate) fn native_sw_init_cap(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // `if (initialSize < 0) throw new IllegalArgumentException("Negative
    // buffer size")` is the JDK constructor's first line. A negative size used
    // to be cast to `usize` and asked for as a ~2^64-element `char[]`, so the
    // caller got an `OutOfMemoryError` instead (gc-common w13-e, noted by
    // `common-w12e-native-io-write-side-growth-single-attempt`).
    let cap = match args.get(1) {
        Some(Value::Int(v)) if *v < 0 => {
            return Err(
                cratonvm_types::error::RuntimeError::IllegalArgumentException {
                    message: "Negative buffer size".into(),
                }
                .into(),
            );
        }
        Some(Value::Int(v)) => *v as usize,
        _ => 32,
    };
    // GC: rooted across the allocation below and released on both exits.
    let this_pin = ctx.pin_native_root(this);
    // gc-common w13-e: caller-sized, so a refusal reclaims and retries before
    // the `OutOfMemoryError` (HotSpot's `new StringBuffer(initialSize)` is
    // Java). `live` audit: `this` is pinned and re-read below; nothing else is
    // held. Registered-only (`StringWriter(int)`, `synthetic-jdk`).
    let buf = match new_prim_array_reclaiming(
        ctx,
        cratonvm_types::ArrayElementType::Char,
        cap.max(1),
        "char[] (StringWriter(int))",
    ) {
        Ok(buf) => buf,
        Err(oome) => {
            ctx.unpin_native_roots(this_pin);
            return Err(oome);
        }
    };
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field(this, SW_FIELD_BUF, Value::Object(Some(buf)));
    sw_set_count(ctx, this, 0);
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

/// Make room for `needed` more chars after the current count.
///
/// gc-common w13-e (`common-w12e-native-io-write-side-growth-single-attempt`):
/// the growth reclaims and retries before its `OutOfMemoryError`, as the
/// JDK's `AbstractStringBuilder.ensureCapacityInternal` does. The reclaim is a
/// moving collection, so `this` is `&mut` and comes back re-read through its
/// pin; the old buffer is pinned here. A caller must pin and re-read every
/// other `ObjectRef` it uses afterwards (`write(char[],int,int)`'s source).
pub(crate) fn sw_ensure_capacity(
    ctx: &mut dyn NativeContext,
    this: &mut ObjectRef,
    needed: usize,
) -> Result<(), MethodCallFailed> {
    let buf = match ctx.get_field(*this, SW_FIELD_BUF) {
        Value::Object(Some(b)) => b,
        _ => return Ok(()),
    };
    let cap = ctx.array_length(buf);
    let count = sw_count(ctx, *this);
    let want = count.saturating_add(needed);
    if want <= cap {
        return Ok(());
    }
    // GC: `this` and the old buffer both cross the growth allocation.
    let base = ctx.pin_native_root(*this);
    let buf_pin = ctx.pin_native_root(buf);
    let new_buf = match new_prim_array_reclaiming(
        ctx,
        cratonvm_types::ArrayElementType::Char,
        grown_array_capacity(cap, want),
        "char[] (StringWriter growth)",
    ) {
        Ok(new_buf) => new_buf,
        Err(oome) => {
            ctx.unpin_native_roots(base);
            return Err(oome);
        }
    };
    let buf = ctx.read_native_pin(buf_pin, buf);
    let live = count.min(cap);
    if !ctx.bulk_array_copy(buf, 0, new_buf, 0, live) {
        for i in 0..live {
            let v = ctx.get_array_element(buf, i);
            ctx.set_array_element(new_buf, i, v);
        }
    }
    *this = ctx.read_native_pin(base, *this);
    ctx.set_field(*this, SW_FIELD_BUF, Value::Object(Some(new_buf)));
    ctx.unpin_native_roots(base);
    Ok(())
}

pub(crate) fn native_sw_write_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // GC: rooted across the call below; `safe_native_call` releases
    // the pin stack to its entry floor on return.
    let this_pin = ctx.pin_native_root(this);
    let ch = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => return Ok(None),
    };
    // gc-common w13-e: the growth can reclaim (a moving collection). `live`
    // audit: `this` is pinned and re-read; nothing else is held.
    let mut grown = this;
    if let Err(oome) = sw_ensure_capacity(ctx, &mut grown, 1) {
        ctx.unpin_native_roots(this_pin);
        return Err(oome);
    }
    let this = ctx.read_native_pin(this_pin, grown);
    let count = sw_count(ctx, this);
    let buf = match ctx.get_field(this, SW_FIELD_BUF) {
        Value::Object(Some(b)) => b,
        _ => return Ok(None),
    };
    ctx.set_array_element(buf, count, Value::Int(ch));
    sw_set_count(ctx, this, count + 1);
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

pub(crate) fn native_sw_write_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // GC: rooted across the call below; `safe_native_call` releases
    // the pin stack to its entry floor on return.
    let this_pin = ctx.pin_native_root(this);
    let s = match args.get(1) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => return Ok(None),
    };
    let chars: Vec<u16> = s.encode_utf16().collect();
    // gc-common w13-e: the growth can reclaim (a moving collection). `live`
    // audit: `this` is pinned and re-read; the text is already in `chars`.
    let mut grown = this;
    if let Err(oome) = sw_ensure_capacity(ctx, &mut grown, chars.len()) {
        ctx.unpin_native_roots(this_pin);
        return Err(oome);
    }
    let this = ctx.read_native_pin(this_pin, grown);
    let count = sw_count(ctx, this);
    let buf = match ctx.get_field(this, SW_FIELD_BUF) {
        Value::Object(Some(b)) => b,
        _ => return Ok(None),
    };
    for (i, &ch) in chars.iter().enumerate() {
        ctx.set_array_element(buf, count + i, Value::Int(ch as i32));
    }
    sw_set_count(ctx, this, count + chars.len());
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

pub(crate) fn native_sw_write_chars(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // GC: rooted across the call below; `safe_native_call` releases
    // the pin stack to its entry floor on return.
    let this_pin = ctx.pin_native_root(this);
    let src = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // GC: rooted across the call below; `safe_native_call` releases
    // the pin stack to its entry floor on return.
    let src_pin = ctx.pin_native_root(src);
    let off_i = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let len_i = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    // `Objects.checkFromIndexSize(off, len, cbuf.length)`, as the JDK's
    // `StringWriter.write(char[],int,int)` does first. Without it a negative
    // `len` became a ~2^64 `usize`: the growth arithmetic wrapped and the copy
    // loop below ran off the end of both arrays (gc-common w13-e, found while
    // auditing this growth call site).
    if let Err(e) = check_array_bounds(off_i, len_i, ctx.array_length(src)) {
        ctx.unpin_native_roots(this_pin);
        return Err(e);
    }
    let off = off_i as usize;
    let len = len_i as usize;
    // gc-common w13-e: the growth can reclaim (a moving collection). `live`
    // audit: `this` and the source `src` are pinned and re-read after it.
    let mut grown = this;
    if let Err(oome) = sw_ensure_capacity(ctx, &mut grown, len) {
        ctx.unpin_native_roots(this_pin);
        return Err(oome);
    }
    let this = ctx.read_native_pin(this_pin, grown);
    let count = sw_count(ctx, this);
    let buf = match ctx.get_field(this, SW_FIELD_BUF) {
        Value::Object(Some(b)) => b,
        _ => return Ok(None),
    };
    let src = ctx.read_native_pin(src_pin, src);
    if !ctx.bulk_array_copy(src, off, buf, count, len) {
        for i in 0..len {
            let v = ctx.get_array_element(src, off + i);
            ctx.set_array_element(buf, count + i, v);
        }
    }
    sw_set_count(ctx, this, count + len);
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

pub(crate) fn native_sw_write_string_off(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // GC: rooted across the call below; `safe_native_call` releases
    // the pin stack to its entry floor on return.
    let this_pin = ctx.pin_native_root(this);
    let s = match args.get(1) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => return Ok(None),
    };
    let off = match args.get(2) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let len = match args.get(3) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let chars: Vec<u16> = s.encode_utf16().skip(off).take(len).collect();
    // gc-common w13-e: the growth can reclaim (a moving collection). `live`
    // audit: `this` is pinned and re-read; the text is already in `chars`.
    let mut grown = this;
    if let Err(oome) = sw_ensure_capacity(ctx, &mut grown, chars.len()) {
        ctx.unpin_native_roots(this_pin);
        return Err(oome);
    }
    let this = ctx.read_native_pin(this_pin, grown);
    let count = sw_count(ctx, this);
    let buf = match ctx.get_field(this, SW_FIELD_BUF) {
        Value::Object(Some(b)) => b,
        _ => return Ok(None),
    };
    for (i, &ch) in chars.iter().enumerate() {
        ctx.set_array_element(buf, count + i, Value::Int(ch as i32));
    }
    sw_set_count(ctx, this, count + chars.len());
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

pub(crate) fn native_sw_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let count = sw_count(ctx, this);
    let buf = match ctx.get_field(this, SW_FIELD_BUF) {
        Value::Object(Some(b)) => b,
        _ => return Ok(Some(Value::Object(None))),
    };
    let mut chars = Vec::with_capacity(count);
    for i in 0..count {
        if let Value::Int(ch) = ctx.get_array_element(buf, i) {
            chars.push(ch as u16);
        }
    }
    let s = String::from_utf16_lossy(&chars);
    let result = ctx.create_string(&s);
    Ok(Some(Value::Object(Some(result))))
}

/// `StringWriter.getBuffer()` — must return a `java.lang.StringBuffer`, NOT a
/// String. It was previously aliased to `native_sw_to_string` (returns a
/// String), so callers like Derby's `ErrorStringBuilder.reset()`
/// (`stringWriter.getBuffer().setLength(0)`) dispatched `setLength` on a String
/// → `NoSuchMethodError: java/lang/String.setLength(I)V` (4 DataSource test
/// classes). Build a real `StringBuffer` from the current content.
pub(crate) fn native_sw_get_buffer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let count = sw_count(ctx, this);
    let mut chars = Vec::with_capacity(count);
    if let Value::Object(Some(buf)) = ctx.get_field(this, SW_FIELD_BUF) {
        for i in 0..count {
            if let Value::Int(ch) = ctx.get_array_element(buf, i) {
                chars.push(ch as u16);
            }
        }
    }
    let s = String::from_utf16_lossy(&chars);
    let str_obj = ctx.create_string(&s);
    ctx.new_object_initialized(
        "java/lang/StringBuffer",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(str_obj))],
    )
}

pub(crate) fn native_sw_append_char(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    sw_append_returning_receiver(ctx, args, native_sw_write_int)
}

pub(crate) fn native_sw_append_cs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    sw_append_returning_receiver(ctx, args, native_sw_write_string)
}

/// `append(..)` is `write(..)` then `return this`.
///
/// gc-common w13-e: the write's growth can now reclaim, a moving collection,
/// so `args[0]` may name the receiver's pre-collection address by the time
/// the write returns. These two used to return `args[0]` as is; the receiver
/// is pinned across the write and the address returned is re-read.
pub(crate) fn sw_append_returning_receiver(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    write: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult,
) -> MethodCallResult {
    let receiver = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        other => {
            write(ctx, args)?;
            return Ok(Some(other.copied().unwrap_or(Value::Object(None))));
        }
    };
    let pin = ctx.pin_native_root(receiver);
    let written = write(ctx, args);
    let receiver = ctx.read_native_pin(pin, receiver);
    ctx.unpin_native_roots(pin);
    written?;
    Ok(Some(Value::Object(Some(receiver))))
}

// ===========================================================================
// DataInputStream — wraps another InputStream, reads typed data big-endian
// 2-field synthetic (field 0 = underlying InputStream, field 1 = Int bytesRead)
// DataOutputStream — wraps OutputStream, writes typed data big-endian
// 2-field synthetic (field 0 = underlying OutputStream, field 1 = Int bytesWritten)
// ===========================================================================

pub(crate) const DIS_FIELD_IN: usize = 0;
pub(crate) const DOS_FIELD_OUT: usize = 0;
/// Slot fallback for the `written` counter.
///
/// `written` is normally addressed by NAME, which works against a real-JDK
/// `DataOutputStream`. A synthetically-allocated one has no field names at all
/// (`ensure_synthetic_class` mints unnamed slots), so both the read and the
/// write silently no-op and `size()` reported 0 no matter how many bytes went
/// out. Keep the by-name path — it is the one that matches the real layout —
/// and fall back to this slot when the name does not resolve.
pub(crate) const DOS_WRITTEN_SLOT: usize = 1;
// NOTE: `written` is NOT at a fixed low slot. The real JDK layout is
// FilterOutputStream{out, closed, closeLock} then DataOutputStream{written, …},
// so `written` lives at slot 3 — NOT slot 1 (which is `closed`). These natives
// previously hardcoded slot 1: self-consistent for `size()` (native reader +
// native writer used the same wrong slot), but a SUBCLASS reading the real
// `written` via `getfield` (e.g. jboss-classfilewriter's
// `ByteArrayDataOutputStream.writeSize()`, which records `this.written` as a
// back-patch position) saw a stale 0 → it overwrote offset 0 of the buffer,
// corrupting the class-file magic to `FF FF FF FC` and making Weld's
// `Lookup.defineClass` reject every generated client proxy (`WELD-001524`).
// Access `written` by NAME so the native and real bytecode agree on the slot.
pub(crate) const DOS_WRITTEN_FIELD: &str = "written";

// JDK-ONLY-CLASSIFY: stub — `DataInputStream`/`DataOutputStream`/`DataInput`/
// `DataOutput` declare no ACC_NATIVE method in JDK 25; 25 of these 37
// registrations shadow concrete bytecode and 4 land on abstract interface
// methods. These are byte-order/encoding conversions expressible in bytecode,
// which is the definition of "not a bridge".
//
// CORRECTION 2026-08-20 (H5-1): this paragraph used to end "Inherited `Bridge`
// from `register_io_natives`". Not true of the tree — the function sets
// `Bridge` explicitly below, for the measured reason the block comment inside
// it gives. Effective tag `Bridge` means `--real-jdk` admits all 37 rows:
// strict-mode LIVE, not dropped-because-stub.
//
// UPDATE 2026-08-20 (H11-B): the "4 land on abstract interface methods" half is
// GONE — see the removal note at the foot of this function. 33 rows remain, all
// on the two concrete stream classes, and 25 of those still shadow concrete
// bytecode. The interface rows were measured at ZERO invocations across 15
// corpus vectors; removing them changes no dispatch this tree can reach.
pub(crate) fn register_data_stream_natives(registry: &mut NativeMethodRegistry) {
    // `register_scanner_natives`. The verdict above is right and the tag is
    // still doing work.
    //
    // The classification holds: 0 ACC_NATIVE, 25 of 37 shadow concrete
    // bytecode. And unlike the cold registrars, this one is genuinely
    // exercised: 1039 invocations, five vectors.
    //
    // CORRECTION 2026-08-20 (H11-B): this paragraph used to continue "and 4
    // land on the `DataInput`/`DataOutput` INTERFACES (so this shim decides
    // dispatch for every USER implementor — worth removing on its own, once it
    // can be)". The parenthetical is FALSE and it is why the four rows survived
    // three census passes as a scary-looking blocker. Native dispatch keys on
    // the RECEIVER's runtime class, and the fallback walk follows `superclass`
    // links only, so an interface registration never serves a user implementor
    // — MEASURED: a `DataInput`-typed call on a user implementor returns the
    // USER's value and the interface slot stays at `invocations: 0`. The four
    // rows were removed at the foot of this function; the 33 that remain are
    // the load-bearing ones and none of them is on an interface.
    //
    // Retagged, that exercise reported the answer immediately:
    //
    //   RDataInputFastPull: skipped.next = -19
    //
    // The stream's position state is this VM's, not the real object's — see the
    // 2-field synthetic layout documented at the top of this section, and the
    // `written` slot fallback below it, which exists precisely because a
    // synthetically-allocated `DataOutputStream` has no field NAMES to address.
    // Refuse the shim and real bytecode reads a position it never wrote.
    //
    // Third instance of the same shape (G88-1 §8, with `scanner`): "stub"
    // describes what the code IS; load-bearing describes what the VM DEPENDS
    // ON. The state must move first, which is wave-2 work.
        let dis = "java/io/DataInputStream";
    // <init> intentionally not registered: real JDK bytecode correctly initializes
    // FilterInputStream.in (slot 0) via super(in) and readBuffer = new byte[8].
    registry.register(dis, "read", "()I", native_dis_read, cratonvm_native_api::NativeKind::Bridge);
    registry.register(dis, "read", "([BII)I", native_dis_read_bytes, cratonvm_native_api::NativeKind::Bridge);
    registry.register(dis, "readBoolean", "()Z", native_dis_read_boolean, cratonvm_native_api::NativeKind::Bridge);
    registry.register(dis, "readByte", "()B", native_dis_read_byte, cratonvm_native_api::NativeKind::Bridge);
    registry.register(
        dis,
        "readUnsignedByte",
        "()I",
        native_dis_read_unsigned_byte, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(dis, "readShort", "()S", native_dis_read_short, cratonvm_native_api::NativeKind::Bridge);
    registry.register(
        dis,
        "readUnsignedShort",
        "()I",
        native_dis_read_unsigned_short, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(dis, "readChar", "()C", native_dis_read_char, cratonvm_native_api::NativeKind::Bridge);
    registry.register(dis, "readInt", "()I", native_dis_read_int, cratonvm_native_api::NativeKind::Bridge);
    registry.register(dis, "readLong", "()J", native_dis_read_long, cratonvm_native_api::NativeKind::Bridge);
    registry.register(dis, "readFloat", "()F", native_dis_read_float, cratonvm_native_api::NativeKind::Bridge);
    registry.register(dis, "readDouble", "()D", native_dis_read_double, cratonvm_native_api::NativeKind::Bridge);
    registry.register(dis, "readUTF", "()Ljava/lang/String;", native_dis_read_utf, cratonvm_native_api::NativeKind::Bridge);
    registry.register(dis, "readFully", "([B)V", native_dis_read_fully, cratonvm_native_api::NativeKind::Bridge);
    registry.register(dis, "readFully", "([BII)V", native_dis_read_fully_off, cratonvm_native_api::NativeKind::Bridge);
    registry.register(dis, "skipBytes", "(I)I", native_dis_skip_bytes, cratonvm_native_api::NativeKind::Bridge);
    registry.register(dis, "available", "()I", native_dis_available, cratonvm_native_api::NativeKind::Bridge);
    registry.register(dis, "close", "()V", native_dis_close, cratonvm_native_api::NativeKind::Bridge);

    let dos = "java/io/DataOutputStream";
    registry.register(dos, "<init>", "(Ljava/io/OutputStream;)V", native_dos_init, cratonvm_native_api::NativeKind::Bridge);
    registry.register(dos, "write", "(I)V", native_dos_write, cratonvm_native_api::NativeKind::Bridge);
    registry.register(dos, "write", "([BII)V", native_dos_write_bytes, cratonvm_native_api::NativeKind::Bridge);
    registry.register(dos, "writeBoolean", "(Z)V", native_dos_write_boolean, cratonvm_native_api::NativeKind::Bridge);
    registry.register(dos, "writeByte", "(I)V", native_dos_write, cratonvm_native_api::NativeKind::Bridge);
    registry.register(dos, "writeShort", "(I)V", native_dos_write_short, cratonvm_native_api::NativeKind::Bridge);
    registry.register(dos, "writeChar", "(I)V", native_dos_write_short, cratonvm_native_api::NativeKind::Bridge);
    registry.register(dos, "writeInt", "(I)V", native_dos_write_int, cratonvm_native_api::NativeKind::Bridge);
    registry.register(dos, "writeLong", "(J)V", native_dos_write_long, cratonvm_native_api::NativeKind::Bridge);
    registry.register(dos, "writeFloat", "(F)V", native_dos_write_float, cratonvm_native_api::NativeKind::Bridge);
    registry.register(dos, "writeDouble", "(D)V", native_dos_write_double, cratonvm_native_api::NativeKind::Bridge);
    registry.register(
        dos,
        "writeUTF",
        "(Ljava/lang/String;)V",
        native_dos_write_utf, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(dos, "flush", "()V", native_dos_flush, cratonvm_native_api::NativeKind::Bridge);
    registry.register(dos, "close", "()V", native_dos_close, cratonvm_native_api::NativeKind::Bridge);
    registry.register(dos, "size", "()I", native_dos_size, cratonvm_native_api::NativeKind::Bridge);

    // REMOVED 2026-08-20 (H11-B) — the four "DataInput/DataOutput interface
    // registrations" that used to sit here:
    //
    //     registry.register("java/io/DataInput",  "readInt",   "()I",  native_dis_read_int);
    //     registry.register("java/io/DataInput",  "readLong",  "()J",  native_dis_read_long);
    //     registry.register("java/io/DataOutput", "writeInt",  "(I)V", native_dos_write_int);
    //     registry.register("java/io/DataOutput", "writeLong", "(J)V", native_dos_write_long);
    //
    // The block comment at the head of this function said these four "decide
    // dispatch for every USER implementor — worth removing on its own, once it
    // can be". It can be, and the premise was wrong: they decided dispatch for
    // NO user implementor.
    //
    // MEASURED (`fe59bf9d9`, `--real-jdk`, `--dump-native-registry`):
    //   * a probe that calls `readInt()` through a `DataInput`-typed local on a
    //     user class implementing `java.io.DataInput` directly gets the USER's
    //     answer (9999), identical to HotSpot 25.0.3+9, and the
    //     `java/io/DataInput.readInt()I` slot reports `invocations: 0`;
    //   * unioned over 15 corpus vectors — `RDataInputFastPull`, `RSerial`,
    //     `RFileTimes`, `RJdkIntrinsics3` among them — all four interface rows
    //     stay at 0 while `java/io/DataInputStream.readInt()I` takes 540 calls
    //     and `java/io/DataOutputStream.writeInt(I)V` takes 53.
    //
    // The mechanism is the same one recorded at the foot of
    // `register_scanner_natives`: dispatch keys on the RECEIVER's class, and
    // the one fallback walk follows `superclass` links only, so it never
    // reaches an interface. A SECOND, independent barrier sits at step 6 of
    // `execute_invoke_kind` and at `vm_exec.rs`'s `override_cb` arm, both of
    // which drop a native whose resolved declaring class is an interface
    // instance method unless the triple is force-listed — none of these four
    // is. The 33 `DataInputStream`/`DataOutputStream` rows —
    // the load-bearing half, the one whose 2026-08-19 retag broke
    // `RDataInputFastPull` — are untouched above and keep every one of those
    // 593 calls.
    //
    // FALSIFIER: an `AbstractMethodError` naming `java/io/DataInput.readInt`,
    // `DataInput.readLong`, `DataOutput.writeInt` or `DataOutput.writeLong`.
    // Those four image methods have no `Code` attribute, so that error is what
    // the `recv_is_bare_object` CP-class rescue in `execute_invoke_kind` would
    // now produce where it previously found a native. Restore the four lines
    // and record the receiver.
    }

/// Helper: read a single byte from the underlying stream of a DIS
pub(crate) fn dis_read_one(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Result<i32, cratonvm_types::error::MethodCallFailed> {
    let inner = match ctx.get_field(this, DIS_FIELD_IN) {
        Value::Object(Some(s)) => s,
        _ => return Ok(-1),
    };
    // A DataInputStream does not own its wrapped stream; Java code may read
    // that stream through another reference between typed reads. Prefetching
    // 8 KiB here removed bytes from the Java stream and hid them in this Rust
    // side buffer. ObjectInputStream does exactly that with its internal
    // BlockDataInputStream, and javac's class-file readers also mix consumers.
    // Read exactly the one byte requested by this helper so all consumers
    // observe the same stream position.
    // Same buffered-window shortcut the typed reads take (`dis_fast_pull`):
    // one byte that is already in the wrapped stream's buffer needs neither a
    // scratch array nor an interpreted `read(byte[],int,int)`.
    let mut one: Vec<u8> = Vec::with_capacity(1);
    if dis_fast_pull(ctx, inner, 1, &mut one) == 1 {
        return Ok(one[0] as i32);
    }
    let read_size = 1;
    // gc-common w20-f: `this` and the wrapped stream are pinned BEFORE the
    // scratch array's allocation. `this` was pinned after it (at its
    // pre-allocation address) and `inner` not at all, so `read` was invoked on
    // the wrapped stream's old address.
    let this_pin = ctx.pin_native_root(this);
    let inner_pin = ctx.pin_native_root(inner);
    let tmp = ctx.new_array(ArrayElementType::Byte, read_size);
    let inner: ObjectRef = ctx.read_native_pin(inner_pin, inner);
    let tmp_pin = ctx.pin_native_root(tmp);
    let result = match ctx.invoke_virtual(
        inner,
        "read",
        "([BII)I",
        &[
            Value::Object(Some(tmp)),
            Value::Int(0),
            Value::Int(read_size as i32),
        ],
    ) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let tmp = ctx.read_native_pin(tmp_pin, tmp);
    let n = match result {
        Some(Value::Int(v)) if v > 0 => v as usize,
        Some(Value::Int(0)) => {
            // InputStream implementations should not return zero for a
            // non-empty request, but user streams sometimes do. Match
            // DataInputStream's progress guarantee by falling back to the
            // scalar read instead of turning zero progress into false EOF.
            // Reload `this` after the virtual bulk call because it may have
            // triggered a moving collection.
            let this = ctx.read_native_pin(this_pin, this);
            let inner: ObjectRef = match ctx.get_field(this, DIS_FIELD_IN) {
                Value::Object(Some(stream)) => stream,
                _ => {
                    ctx.unpin_native_roots(this_pin);
                    return Ok(-1);
                }
            };
            let scalar = match ctx.invoke_virtual(inner, "read", "()I", &[]) {
                Ok(Some(Value::Int(value))) => value,
                Ok(_) => -1,
                Err(error) => {
                    ctx.unpin_native_roots(this_pin);
                    return Err(error);
                }
            };
            ctx.unpin_native_roots(this_pin);
            return Ok(scalar);
        }
        _ => {
            ctx.unpin_native_roots(this_pin);
            return Ok(-1);
        }
    };
    let mut bytes = vec![0u8; n];
    ctx.read_byte_array_into(tmp, 0, &mut bytes);
    ctx.unpin_native_roots(this_pin);
    Ok(bytes[0] as i32)
}

/// Copy up to `want` bytes straight out of the wrapped stream's OWN buffer,
/// in Rust, and advance its `pos`. Returns how many bytes were appended to
/// `out` (0 means "not applicable, use the generic path").
///
/// # Why this exists
///
/// `dis_read_exact` is the shared helper behind `readByte`, `readShort`,
/// `readUnsignedShort`, `readChar`, `readInt`, `readLong`, `readFloat`,
/// `readDouble`, `readBoolean` and `readUTF`. Its generic path, for a **two
/// byte** `readUnsignedShort`, allocates a Java `byte[2]` on the heap and then
/// re-enters the VM through `invoke_virtual` to run the whole interpreted
/// `BufferedInputStream.read(byte[],int,int)` chain (`read` -> `read1` ->
/// `getBufIfOpen` x2 -> `ensureOpen` -> `System.arraycopy`), then copies the
/// bytes back out. That is ~6 interpreted invocations plus an allocation per
/// two bytes of class file.
///
/// Tomcat's webapp deploy is exactly that shape: `ContextConfig`'s annotation
/// scan runs BCEL's `ClassParser` over every `.class` in every jar on the
/// container classpath, and `ClassParser` reads the whole file through
/// `DataInputStream.readUnsignedShort`/`readInt`. Measured on
/// `TestManagerWebapp.testBug57700` with `--stack-sample-ms`, those five
/// `BufferedInputStream` bodies were **78% of all interpreted time in the
/// run**, and none of them can compile: `read` and `read(byte[],int,int)` are
/// `ACC_SYNCHRONIZED`, and `read1`/`getBufIfOpen`/`ensureOpen`/`fill` are
/// private, so the tiering manager never sees any of them.
///
/// With the buffer already filled the bytes are simply sitting in `buf` at
/// `pos`, so the whole round trip is avoidable. The generic path still runs
/// whenever the buffer is exhausted — that is what refills it — so the cost
/// becomes one re-entry per 8 KiB rather than one per two bytes.
///
/// # Why it is faithful
///
/// * **Exact class only.** A subclass may override `read(byte[],int,int)`, and
///   only the generic `invoke_virtual` path honours an override. Anything that
///   is not exactly `java/io/BufferedInputStream` or
///   `java/io/ByteArrayInputStream` returns 0 here.
/// * **Same state transition.** Serving from the buffer is what
///   `BufferedInputStream.read1` and `ByteArrayInputStream.read` do: copy out
///   of `buf` starting at `pos`, then `pos += n`. Neither touches `markpos` on
///   that path, so `mark`/`reset` keep working.
/// * **Short reads are allowed.** `read(byte[],int,int)` may legally return
///   fewer bytes than asked; the caller already loops, so returning only what
///   the buffer holds needs no special handling.
/// * **Field reads are by NAME**, so a layout this VM does not model answers
///   `Int(0)` rather than a wrong slot — `count <= pos` then fails the guard
///   and the generic path runs.
/// * **No allocation**, therefore no GC, therefore no `ObjectRef` can go stale
///   inside this function.
///
/// Not synchronized, unlike the bytecode it replaces. `dis_read_exact`'s
/// existing loop already issues several `read` calls without holding anything
/// across them, so a `DataInputStream` shared between threads was never atomic
/// here; this does not add a race class.
pub(crate) fn dis_fast_window(
    ctx: &mut dyn NativeContext,
    inner: ObjectRef,
) -> Option<(ObjectRef, usize, usize)> {
    let class_id = ctx.class_id_of_object(inner);
    let class_name = ctx.class_name_of_id(class_id)?;
    if class_name != "java/io/BufferedInputStream" && class_name != "java/io/ByteArrayInputStream" {
        return None;
    }
    let pos = ctx.get_field_by_name(inner, "pos").as_int().unwrap_or(-1);
    let count = ctx.get_field_by_name(inner, "count").as_int().unwrap_or(-1);
    if pos < 0 || count <= pos {
        return None;
    }
    let Value::Object(Some(buf)) = ctx.get_field_by_name(inner, "buf") else {
        return None;
    };
    // Clamp to the array too: `count` is the VM's view of a field, the array
    // length is ground truth, and reading past it would be out of bounds.
    let count = (count as usize).min(ctx.array_length(buf));
    let pos = pos as usize;
    if pos >= count {
        return None;
    }
    Some((buf, pos, count))
}

/// Discard-only sibling of [`dis_fast_pull`], for `skipBytes`.
///
/// `Utility.skipFully` is how BCEL steps over every class-file attribute it
/// does not care about - which is most of them, `Code` included - so this runs
/// once per skipped attribute. The generic path below allocates an 8 KiB Java
/// scratch array and re-enters the VM to read-and-discard into it; when the
/// bytes are already buffered, advancing `pos` is the entire operation.
/// `BufferedInputStream.skip` does exactly that on its buffered path
/// (`long avail = count - pos; ... pos += n`), and like `read1` it leaves
/// `markpos` alone.
pub(crate) fn dis_fast_skip(ctx: &mut dyn NativeContext, inner: ObjectRef, want: usize) -> usize {
    if want == 0 {
        return 0;
    }
    let Some((_buf, pos, count)) = dis_fast_window(ctx, inner) else {
        return 0;
    };
    let n = want.min(count - pos);
    ctx.set_field_by_name(inner, "pos", Value::Int((pos + n) as i32));
    n
}

pub(crate) fn dis_fast_pull(
    ctx: &mut dyn NativeContext,
    inner: ObjectRef,
    want: usize,
    out: &mut Vec<u8>,
) -> usize {
    if want == 0 {
        return 0;
    }
    let Some((buf, pos, count)) = dis_fast_window(ctx, inner) else {
        return 0;
    };
    let n = want.min(count - pos);
    let base = out.len();
    out.resize(base + n, 0);
    let copied = ctx.read_byte_array_into(buf, pos, &mut out[base..]);
    if copied != n {
        // Defensive: the array read declined. Leave `pos` untouched so the
        // generic path re-reads these bytes rather than losing them.
        out.truncate(base);
        return 0;
    }
    ctx.set_field_by_name(inner, "pos", Value::Int((pos + n) as i32));
    n
}

pub(crate) fn dis_read_exact(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    len: usize,
) -> Result<Vec<u8>, MethodCallFailed> {
    // PERF FIX (2026-07-13, STW-takeover-cluster residual investigation,
    // Azure host follow-up): this used to loop `dis_read_one` (a full
    // array-alloc + invoke_virtual dispatch) once per byte. It's the shared
    // helper behind readByte/readShort/readUnsignedShort/readChar AND
    // (found via multi-snapshot gdb on the Azure host, confirmed the same
    // stuck frame 3 snapshots in a row 5s apart) `readUTF` — which calls it
    // with `len` up to 65535 (the modified-UTF-8 payload length), and
    // `readUTF` is exactly how class-file/JSP-compile constant-pool string
    // entries get decoded, so this was the dominant cost in the
    // TestJspConfig/TestELInterpreterTagSetters/TestEnvEntry/
    // TestWsWebSocketContainerTimeoutClient hang residual left after the
    // native_dis_read_bytes/dis_read_fully_impl/native_dis_skip_bytes fixes
    // (see elinjsp-socket-read-timeout.md).
    // Bulk-read instead, preserving the same zero-progress-guard fallback
    // `dis_read_one` had (a stream returning 0 for a non-empty request is a
    // contract violation but tolerated here via a scalar `read()` retry).
    if len == 0 {
        return Ok(Vec::new());
    }
    let inner = match ctx.get_field(this, DIS_FIELD_IN) {
        Value::Object(Some(s)) => s,
        _ => return Err(eof_exception()),
    };
    // Take whatever the wrapped stream already holds in its own buffer without
    // allocating or re-entering the VM - see `dis_fast_pull`. On the common
    // case (a filled `BufferedInputStream`, which is how every class-file
    // parser drives this) that satisfies the whole request and the generic
    // path below never runs.
    let mut fast: Vec<u8> = Vec::with_capacity(len);
    dis_fast_pull(ctx, inner, len, &mut fast);
    if fast.len() == len {
        return Ok(fast);
    }
    let want = len - fast.len();
    // Family-1 fix (cce0079): `inner` is dispatched repeatedly below — each
    // `read` can trigger a moving GC, so refresh it per iteration like `buf`.
    let inner_pin = ctx.pin_native_root(inner);
    let mut inner = inner;
    // gc-common w12-e: `want` is caller-sized for `readUTF` (up to 65535, the
    // encoded length), so the scratch array reclaims and retries before the
    // `OutOfMemoryError` (`common-w8b-native-factories-still-single-attempt`
    // residue 3). `live` audit: `inner` is pinned just above and re-read
    // below; `this` is not used after this point, and every caller already
    // treats this helper as a GC point (the read below dispatches
    // `InputStream.read`). Every caller is a registered `DataInputStream`
    // native.
    let buf = match new_byte_array_reclaiming(ctx, want, "byte[] (DataInputStream read)") {
        Ok(buf) => buf,
        Err(oome) => {
            ctx.unpin_native_roots(inner_pin);
            return Err(oome);
        }
    };
    let buf_pin = ctx.pin_native_root(buf);
    let mut buf = buf;
    // `new_array` can collect and relocate the wrapped stream. The pin keeps
    // it live, but ObjectRef is an address-like handle in the moving heap, so
    // reload it before the first virtual read just as the loop does after
    // every subsequent GC-capable call.
    inner = ctx.read_native_pin(inner_pin, inner);
    let mut total = 0usize;
    while total < want {
        let remaining = (want - total) as i32;
        let n = match ctx.invoke_virtual(
            inner,
            "read",
            "([BII)I",
            &[
                Value::Object(Some(buf)),
                Value::Int(total as i32),
                Value::Int(remaining),
            ],
        ) {
            Ok(Some(Value::Int(n))) => n,
            Ok(_) => -1,
            Err(e) => {
                ctx.unpin_native_roots(inner_pin);
                return Err(e);
            }
        };
        buf = ctx.read_native_pin(buf_pin, buf);
        inner = ctx.read_native_pin(inner_pin, inner);
        if n == 0 {
            // Contract-violating zero-progress read: fall back to a scalar
            // single-byte read so a misbehaving stream still makes forward
            // progress instead of spinning forever on remaining==0 never
            // being satisfied.
            let scalar = match ctx.invoke_virtual(inner, "read", "()I", &[]) {
                Ok(Some(Value::Int(v))) if v >= 0 => v,
                Ok(_) => -1,
                Err(e) => {
                    ctx.unpin_native_roots(inner_pin);
                    return Err(e);
                }
            };
            if scalar < 0 {
                ctx.unpin_native_roots(inner_pin);
                return Err(eof_exception());
            }
            buf = ctx.read_native_pin(buf_pin, buf);
            inner = ctx.read_native_pin(inner_pin, inner);
            ctx.set_array_element(buf, total, Value::Int(scalar));
            total += 1;
            continue;
        }
        if n < 0 {
            ctx.unpin_native_roots(inner_pin);
            return Err(eof_exception());
        }
        total += n as usize;
    }
    let base = fast.len();
    let mut out = fast;
    out.resize(len, 0);
    ctx.read_byte_array_into(buf, 0, &mut out[base..]);
    ctx.unpin_native_roots(inner_pin);
    Ok(out)
}

pub(crate) fn native_dis_read(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let b = dis_read_one(ctx, this)?;
    Ok(Some(Value::Int(b)))
}

pub(crate) fn native_dis_read_bytes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let buf = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let off = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let len = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    if len <= 0 {
        return Ok(Some(Value::Int(0)));
    }
    // PERF FIX (2026-07-13, STW-takeover-cluster residual investigation):
    // this used to loop `dis_read_one` (a full array-alloc +
    // invoke_virtual dispatch) once per byte — O(len) interpreter round
    // trips for a single bulk read() call. For a multi-KB/MB read (a
    // compiled JSP class file, a JAR entry, ...) that's minutes of VM
    // overhead where real Java does one native syscall, which manifested
    // as an apparent permanent hang (confirmed NOT infinite — it just never
    // finished within a 300s budget) in the STW-takeover-cluster residual
    // investigation (see stw-crossthread-jit-takeover-hang-cluster.md and
    // elinjsp-socket-read-timeout.md). `DataInputStream.read(byte[],int,int)`
    // in real JDK is a single delegating call to `in.read(b, off, len)` —
    // it does not prefetch or over-read, so making exactly one call here
    // for the exact requested length matches real semantics precisely
    // while avoiding the per-byte multiplier. `dis_read_one` (unchanged)
    // remains correct and cheap for the true single-byte callers
    // (`readByte`/`readBoolean`/etc, `dis_read_exact` with len 1-8).
    let inner = match ctx.get_field(this, DIS_FIELD_IN) {
        Value::Object(Some(s)) => s,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let result = ctx.invoke_virtual(
        inner,
        "read",
        "([BII)I",
        &[Value::Object(Some(buf)), Value::Int(off), Value::Int(len)],
    )?;
    let n = match result {
        Some(Value::Int(n)) => n,
        _ => -1,
    };
    Ok(Some(Value::Int(n)))
}

pub(crate) fn native_dis_read_boolean(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let bytes = dis_read_exact(ctx, this, 1)?;
    Ok(Some(Value::Int(if bytes[0] != 0 { 1 } else { 0 })))
}

pub(crate) fn native_dis_read_byte(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let bytes = dis_read_exact(ctx, this, 1)?;
    Ok(Some(Value::Int(bytes[0] as i8 as i32)))
}

pub(crate) fn native_dis_read_unsigned_byte(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let bytes = dis_read_exact(ctx, this, 1)?;
    Ok(Some(Value::Int(bytes[0] as i32)))
}

pub(crate) fn native_dis_read_short(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let bytes = dis_read_exact(ctx, this, 2)?;
    let val = i16::from_be_bytes([bytes[0], bytes[1]]);
    Ok(Some(Value::Int(val as i32)))
}

pub(crate) fn native_dis_read_unsigned_short(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let bytes = dis_read_exact(ctx, this, 2)?;
    let val = u16::from_be_bytes([bytes[0], bytes[1]]);
    Ok(Some(Value::Int(val as i32)))
}

pub(crate) fn native_dis_read_char(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_dis_read_unsigned_short(ctx, args)
}

pub(crate) fn native_dis_read_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let bytes = dis_read_exact(ctx, this, 4)?;
    Ok(Some(Value::Int(i32::from_be_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3],
    ]))))
}

pub(crate) fn native_dis_read_long(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    let bytes = dis_read_exact(ctx, this, 8)?;
    Ok(Some(Value::Long(i64::from_be_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]))))
}

pub(crate) fn native_dis_read_float(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let int_result = native_dis_read_int(ctx, args)?;
    if let Some(Value::Int(bits)) = int_result {
        Ok(Some(Value::Float(f32::from_bits(bits as u32))))
    } else {
        Ok(Some(Value::Float(0.0)))
    }
}

pub(crate) fn native_dis_read_double(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let long_result = native_dis_read_long(ctx, args)?;
    if let Some(Value::Long(bits)) = long_result {
        Ok(Some(Value::Double(f64::from_bits(bits as u64))))
    } else {
        Ok(Some(Value::Double(0.0)))
    }
}

/// T2.4.18: `DataInputStream.readUTF` — read a modified UTF-8 string
/// per JVMS §4.4.7. Differs from standard UTF-8 on two points:
///
///   * The null character `U+0000` is encoded as the two-byte sequence
///     `0xC0 0x80` (never as a single zero byte).
///   * Supplementary characters `U+10000..U+10FFFF` are encoded as a
///     UTF-16 surrogate pair, with each surrogate then emitted in the
///     3-byte form. A supplementary code point therefore occupies
///     **six** bytes in the modified UTF-8 stream, not four.
///
/// The 2-byte length prefix counts **bytes**, not characters. The
/// return value is a newly allocated Java String containing the
/// decoded code units. On a malformed stream this native throws
/// `java.io.UTFDataFormatException`, built out of the image by
/// [`utf_data_format_error`], with a message identifying the byte offset of the
/// fault. It said `IOException` *"for now, as the dedicated exception class is
/// not yet in our throwable registry"* until 2026-08-22, and that premise was
/// never checked: nothing has to be in the `RuntimeError` enum to be thrown —
/// `new_object` + `<init>` raises the real class, which is how the channel
/// refusals in this file have always worked.
pub(crate) fn native_dis_read_utf(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Family-1 fix (cce0079): the first `dis_read_exact` dispatches
    // `InputStream.read` (GC-capable) — refresh `this` before the second
    // call (canary-caught live during WildFly `Currency.<clinit>`).
    let this_pin = ctx.pin_native_root(this);
    let len_bytes = match dis_read_exact(ctx, this, 2) {
        Ok(b) => b,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let len = u16::from_be_bytes([len_bytes[0], len_bytes[1]]) as usize;
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    let bytes = dis_read_exact(ctx, this, len)?;
    if data_utf_units_exact() {
        return read_utf_payload_units(ctx, &bytes);
    }
    let s = match decode_modified_utf8(&bytes) {
        Ok(s) => s,
        Err(e) => return Err(utf_data_format_error(ctx, &e)),
    };
    let result = ctx.create_string(&s);
    Ok(Some(Value::Object(Some(result))))
}

/// `readUTF`'s answer for a payload already read in full, units-exact
/// (JO3-3, round 15 wave 4 lane jdkonly5): the JDK's decode and messages
/// ([`decode_modified_utf8_units`]) and a FRESH `String` -- HotSpot returns
/// `new String(chararr, 0, n)`, never a pooled one, and every distinct
/// decoded text used to enter the never-pruned pool. Shared by every
/// `readUTF` native of this crate and `native-builtins`' synthetic
/// `RandomAccessFile`.
pub fn read_utf_payload_units(ctx: &mut dyn NativeContext, bytes: &[u8]) -> MethodCallResult {
    match decode_modified_utf8_units(bytes) {
        Ok(units) => {
            let result = ctx.create_string_from_units(&units);
            Ok(Some(Value::Object(Some(result))))
        }
        Err(message) => {
            let msg = ctx.create_string_uninterned(&message);
            Err(nio_native::construct_thrown(
                ctx,
                "java/io/UTFDataFormatException",
                "(Ljava/lang/String;)V",
                &[Value::Object(Some(msg))],
                // Only when the class cannot be built at all: the supertype,
                // so a malformed record never decodes to something.
                || RuntimeError::IOException { message }.into(),
            ))
        }
    }
}

/// Decode a slice of modified-UTF-8 bytes into a Rust `String`,
/// round-tripping surrogate pairs through the matching supplementary
/// code point. Every error branch identifies a byte offset so the
/// error message is precise enough for diagnostics.
///
/// # Why this is exported
///
/// `readUTF`/`writeUTF` are a **wire format**, not a string conversion, and
/// this crate is not the only one that implements them: the synthetic
/// `java.io.RandomAccessFile` in `native-builtins/src/phases_late/nio_file.rs`
/// has its own pair, and that pair reads and writes **plain** UTF-8
/// (`String::from_utf8_lossy` / `s.as_bytes()`). Plain and modified UTF-8 differ
/// on exactly two inputs — `U+0000`, which modified UTF-8 spells `C0 80` and
/// plain spells `00`, and every supplementary character, which modified UTF-8
/// spells as a six-byte surrogate PAIR and plain spells as one four-byte
/// sequence — so a record written by one implementation and read by the other is
/// silently mis-framed rather than rejected. Exporting the codec is what lets
/// that second implementation converge onto this one instead of growing a third
/// spelling. See W7-8-fabricated-success-io-sweep.md.
/// What a malformed modified-UTF-8 stream owes its reader:
/// `java.io.UTFDataFormatException`, not a bare `java.io.IOException`.
///
/// MEASURED, `regression-suite/probes/W4Data.java`, Linux/JDK 25.0.4, both
/// modes, feeding `readUTF` a 1-byte payload whose only byte is a continuation
/// byte (`00 01 80`):
///
/// ```text
///   HotSpot    java.io.UTFDataFormatException
///   CratonVM   java.io.IOException
/// ```
///
/// `DataInput.readUTF`'s javadoc names `UTFDataFormatException` explicitly and
/// separately from `IOException`, and the distinction is the whole point of the
/// two types: one says the DATA is corrupt, the other says the CHANNEL failed.
/// A caller that retries on `IOException` and gives up on
/// `UTFDataFormatException` -- which is the sensible way round -- retries
/// forever against a corrupt record.
///
/// `UTFDataFormatException extends IOException`, so tightening this cannot
/// break a handler that already compiled. This is the same repair, and the same
/// argument, as `afc_closed_channel_error` above; the comment on `readUTF` used
/// to say the dedicated class was *"not yet in our throwable registry"*, and it
/// does not need to be — `new_object` + `<init>` builds the real one out of the
/// image, exactly as the channel refusals do.
///
/// Round 15 wave 3, lane jdkonly3: built by [`nio_native::construct_thrown`]
/// (rooted across `<init>`, the forwarded reference thrown; the message a
/// fresh string). The old `new_object` + `invoke` held the allocation raw
/// across the message's allocation and the constructor and threw that address.
pub(crate) fn utf_data_format_error(ctx: &mut dyn NativeContext, detail: &str) -> MethodCallFailed {
    let message = format!("readUTF: {detail}");
    let msg = ctx.create_string_uninterned(&message);
    nio_native::construct_thrown(
        ctx,
        "java/io/UTFDataFormatException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(msg))],
        // Only reached when the class cannot be built at all (a mock context, or
        // an image without it). The supertype keeps the failure LOUD rather than
        // letting a malformed record decode to something.
        || RuntimeError::IOException { message }.into(),
    )
}

pub fn decode_modified_utf8(bytes: &[u8]) -> Result<String, String> {
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b0 = bytes[i];
        let cp: u32 = if b0 & 0x80 == 0 {
            // 1-byte form: 0xxxxxxx
            i += 1;
            b0 as u32
        } else if (b0 & 0xE0) == 0xC0 {
            // 2-byte form: 110xxxxx 10xxxxxx
            if i + 1 >= bytes.len() {
                return Err(format!("truncated 2-byte sequence at offset {i}"));
            }
            let b1 = bytes[i + 1];
            if (b1 & 0xC0) != 0x80 {
                return Err(format!("bad continuation byte at offset {}", i + 1));
            }
            let v = (((b0 as u32) & 0x1F) << 6) | ((b1 as u32) & 0x3F);
            i += 2;
            v
        } else if (b0 & 0xF0) == 0xE0 {
            // 3-byte form: 1110xxxx 10xxxxxx 10xxxxxx
            if i + 2 >= bytes.len() {
                return Err(format!("truncated 3-byte sequence at offset {i}"));
            }
            let b1 = bytes[i + 1];
            let b2 = bytes[i + 2];
            if (b1 & 0xC0) != 0x80 || (b2 & 0xC0) != 0x80 {
                return Err(format!("bad continuation byte at offset {}", i + 1));
            }
            let v =
                (((b0 as u32) & 0x0F) << 12) | (((b1 as u32) & 0x3F) << 6) | ((b2 as u32) & 0x3F);
            i += 3;
            v
        } else {
            return Err(format!("illegal leading byte 0x{b0:02x} at offset {i}"));
        };

        // Handle surrogate pairs: a high surrogate must be followed by
        // a low surrogate, which combine into a supplementary code
        // point per UTF-16.
        if (0xD800..=0xDBFF).contains(&cp) {
            // Decode the matching low surrogate.
            if i >= bytes.len() {
                return Err("lone high surrogate at end of stream".to_string());
            }
            let b0 = bytes[i];
            if (b0 & 0xF0) != 0xE0 || i + 2 >= bytes.len() {
                return Err(format!(
                    "expected 3-byte low surrogate after high surrogate at offset {i}"
                ));
            }
            let b1 = bytes[i + 1];
            let b2 = bytes[i + 2];
            if (b1 & 0xC0) != 0x80 || (b2 & 0xC0) != 0x80 {
                return Err(format!("bad surrogate continuation at offset {}", i + 1));
            }
            let low =
                (((b0 as u32) & 0x0F) << 12) | (((b1 as u32) & 0x3F) << 6) | ((b2 as u32) & 0x3F);
            i += 3;
            if !(0xDC00..=0xDFFF).contains(&low) {
                return Err(format!(
                    "high surrogate not followed by low surrogate at offset {i}"
                ));
            }
            let supplementary = 0x10000 + ((cp - 0xD800) << 10) + (low - 0xDC00);
            match char::from_u32(supplementary) {
                Some(c) => out.push(c),
                None => {
                    return Err(format!(
                        "invalid supplementary code point U+{supplementary:06X}"
                    ))
                }
            }
        } else if (0xDC00..=0xDFFF).contains(&cp) {
            return Err(format!("unpaired low surrogate at offset {i}"));
        } else {
            match char::from_u32(cp) {
                Some(c) => out.push(c),
                None => return Err(format!("invalid code point U+{cp:04X} at offset {i}")),
            }
        }
    }
    Ok(out)
}

/// Encode a Rust `&str` into modified UTF-8 (JVMS §4.4.7) and return
/// the byte buffer. Use from `native_dos_write_utf`.
///
/// Exported for the reason spelled out on [`decode_modified_utf8`]: the length
/// prefix `DataOutput.writeUTF` writes is the length of THIS encoding, so a
/// caller that measures `s.as_bytes().len()` and then writes plain UTF-8 is
/// wrong twice — in the count and in the payload — and `readUTF` cannot tell.
pub fn encode_modified_utf8(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    for c in s.chars() {
        let cp = c as u32;
        if cp == 0 {
            // U+0000 → 0xC0 0x80 (two bytes, never one).
            out.push(0xC0);
            out.push(0x80);
        } else if cp < 0x80 {
            // 1-byte form.
            out.push(cp as u8);
        } else if cp < 0x800 {
            // 2-byte form.
            out.push(0xC0 | ((cp >> 6) as u8));
            out.push(0x80 | ((cp & 0x3F) as u8));
        } else if cp < 0x10000 {
            // 3-byte form (covers the full BMP).
            out.push(0xE0 | ((cp >> 12) as u8));
            out.push(0x80 | (((cp >> 6) & 0x3F) as u8));
            out.push(0x80 | ((cp & 0x3F) as u8));
        } else {
            // Supplementary → UTF-16 surrogate pair, each 3-byte form.
            // (4-byte UTF-8 form is NOT used in modified UTF-8.)
            let v = cp - 0x10000;
            let high = 0xD800 | (v >> 10);
            let low = 0xDC00 | (v & 0x3FF);
            out.push(0xE0 | ((high >> 12) as u8));
            out.push(0x80 | (((high >> 6) & 0x3F) as u8));
            out.push(0x80 | ((high & 0x3F) as u8));
            out.push(0xE0 | ((low >> 12) as u8));
            out.push(0x80 | (((low >> 6) & 0x3F) as u8));
            out.push(0x80 | ((low & 0x3F) as u8));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The units-exact modified-UTF-8 codec (round 15 wave 4, lane jdkonly5;
// proposal JO3-3 of `docs/feature-designs/jit-ir-optimizations.md`,
// page `r15w3-jdkonly3-compat-data-utf-divergence-FIXED-20260930.md`).
//
// `readUTF` / `writeUTF` are defined over UTF-16 UNITS, not code points: the
// JDK encodes each `char` on its own (JVMS 4.4.7) and decodes each 1/2/3-byte
// form into one `char`, with no surrogate pairing either way. The `&str` pair
// above cannot represent an unpaired surrogate (a lone `\uD800` was written as
// `EF BF BD` and read back refused), words its refusals in its own terms, and
// its callers interned the result. These follow `DataOutputStream.writeUTF` /
// `DataInputStream.readUTF` of JDK 25 step for step, messages included; every
// `--compatible` `writeUTF`/`readUTF` native (this crate's `DataInputStream`,
// `DataOutputStream` and synthetic `RandomAccessFile`, and `native-builtins`'
// synthetic `RandomAccessFile`) uses them while [`data_utf_units_exact`] is on.
// ---------------------------------------------------------------------------

/// `CRATONVM_COMPAT_DATA_UTF_UNITS` (default on): whether the `--compatible`
/// `readUTF`/`writeUTF` natives use the units-exact codec below (HotSpot's
/// bytes, messages and a fresh result `String`). `0` restores the `&str`
/// codec (`encode_modified_utf8`/`decode_modified_utf8`), its messages and the
/// interned result. Read live: every reader is an I/O native whose own cost
/// dwarfs one flag lookup.
pub fn data_utf_units_exact() -> bool {
    true
}

/// The modified-UTF-8 length of `units`, `DataOutputStream.writeUTF`'s
/// `utflen`: one byte for `1..=0x7F`, two for `0` and `..=0x7FF`, three for
/// every other unit (a surrogate included, each on its own). Exact in `u64`,
/// which is what `tooLongMsg` reconstructs from its overflowing `int`.
pub fn modified_utf8_units_len(units: &[u16]) -> u64 {
    units
        .iter()
        .map(|&c| {
            if c != 0 && c < 0x80 {
                1u64
            } else if c < 0x800 {
                2
            } else {
                3
            }
        })
        .sum()
}

/// `DataOutputStream.writeUTF`'s payload for `units` (without the two-byte
/// length), byte for byte. The caller checks [`modified_utf8_units_len`]
/// against 65535 first.
pub fn encode_modified_utf8_units(units: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(units.len());
    for &unit in units {
        let c = unit as u32;
        if c != 0 && c < 0x80 {
            out.push(c as u8);
        } else if c >= 0x800 {
            out.push((0xE0 | ((c >> 12) & 0x0F)) as u8);
            out.push((0x80 | ((c >> 6) & 0x3F)) as u8);
            out.push((0x80 | (c & 0x3F)) as u8);
        } else {
            out.push((0xC0 | ((c >> 6) & 0x1F)) as u8);
            out.push((0x80 | (c & 0x3F)) as u8);
        }
    }
    out
}

/// `DataOutputStream.tooLongMsg(s, utflen)`, as UTF-16 units (its head and
/// tail are `String.substring`s, which can cut a surrogate pair):
/// `encoded string (<first 8>...<last 8>) too long: <utflen> bytes`. Only
/// reached with `utflen > 65535`, so `units` holds at least 21846 units; the
/// bounds are clamped anyway.
pub fn modified_utf8_too_long_message(units: &[u16], utflen: u64) -> Vec<u16> {
    let head = &units[..units.len().min(8)];
    let tail = &units[units.len().saturating_sub(8)..];
    let mut message: Vec<u16> = "encoded string (".encode_utf16().collect();
    message.extend_from_slice(head);
    message.extend("...".encode_utf16());
    message.extend_from_slice(tail);
    message.extend(format!(") too long: {utflen} bytes").encode_utf16());
    message
}

/// `DataInputStream.readUTF(DataInput)`'s decode of a `utflen`-byte payload,
/// step for step: each form is one unit (a surrogate is returned as read,
/// never paired or refused), and a refusal is the JDK's own
/// `UTFDataFormatException` message -- `malformed input: partial character
/// at end`, or `malformed input around byte N` with the JDK's `N` (the lead
/// byte's offset for a bad lead; the offset past a two-byte form; the offset
/// of the third byte's predecessor for a three-byte form).
pub fn decode_modified_utf8_units(bytes: &[u8]) -> Result<Vec<u16>, String> {
    let utflen = bytes.len();
    let mut out: Vec<u16> = Vec::with_capacity(utflen);
    let mut count = 0usize;
    while count < utflen {
        let c = bytes[count] as u32;
        match c >> 4 {
            0..=7 => {
                // 0xxxxxxx
                count += 1;
                out.push(c as u16);
            }
            12 | 13 => {
                // 110x xxxx   10xx xxxx
                count += 2;
                if count > utflen {
                    return Err("malformed input: partial character at end".to_string());
                }
                let char2 = bytes[count - 1] as u32;
                if (char2 & 0xC0) != 0x80 {
                    return Err(format!("malformed input around byte {count}"));
                }
                out.push((((c & 0x1F) << 6) | (char2 & 0x3F)) as u16);
            }
            14 => {
                // 1110 xxxx  10xx xxxx  10xx xxxx
                count += 3;
                if count > utflen {
                    return Err("malformed input: partial character at end".to_string());
                }
                let char2 = bytes[count - 2] as u32;
                let char3 = bytes[count - 1] as u32;
                if (char2 & 0xC0) != 0x80 || (char3 & 0xC0) != 0x80 {
                    return Err(format!("malformed input around byte {}", count - 1));
                }
                out.push((((c & 0x0F) << 12) | ((char2 & 0x3F) << 6) | (char3 & 0x3F)) as u16);
            }
            _ => {
                // 10xx xxxx,  1111 xxxx
                return Err(format!("malformed input around byte {count}"));
            }
        }
    }
    Ok(out)
}

pub(crate) fn native_dis_read_fully(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // `readFully(null)` is `readFully(b, 0, b.length)` — an NPE. Returning
    // quietly reported a SUCCESSFUL full read of a record that was never read.
    let buf = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::NullPointerException { message: None }.into(),
            )
        }
    };
    let len = ctx.array_length(buf) as i32;
    dis_read_fully_impl(ctx, this, buf, 0, len as usize)
}

pub(crate) fn native_dis_read_fully_off(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let buf = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::NullPointerException { message: None }.into(),
            )
        }
    };
    let off = match args.get(2) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let len = match args.get(3) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    dis_read_fully_impl(ctx, this, buf, off, len)
}

pub(crate) fn eof_exception() -> MethodCallFailed {
    MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::EOFException {
        // HotSpot's `DataInputStream` throws the no-arg constructor: the
        // message is NULL. "Unexpected EOF" was ours and reads like a JDK
        // string, which is what kept it.
        message: String::new(),
    }))
}

/// Shared implementation for readFully — bulk-reads on the inner stream,
/// looping only on genuine short reads (a `read()` call returning fewer
/// bytes than asked for — normal for e.g. a socket stream, and typically
/// just 1-2 extra iterations for a buffered file/JAR stream). Throws
/// EOFException if the stream ends before all requested bytes have been
/// read, matching the Java specification.
///
/// PERF FIX (2026-07-13): this used to loop `dis_read_one` byte-by-byte —
/// O(len) interpreter-dispatch round trips for what real Java does as a
/// handful of native `read()` calls. Independently root-caused twice the
/// same day from two different angles: the STW-takeover-cluster residual
/// investigation (stw-crossthread-jit-takeover-hang-cluster.md — compiled JSP class
/// files/JAR entries via Jasper's classloading path) and the jar-signature
/// investigation (inputstream-readallbytes-readnbytes-readfully-byte-at-a-time-FIXED.md —
/// Spring Boot loader's `JarEntriesStream.assertSameContent()`, once per
/// up-to-4KB chunk per jar entry). Both turned a sub-millisecond real-JDK
/// operation into minutes of VM overhead, confirmed NOT infinite — it just
/// never finished within a 300s test-suite budget.
///
/// Unlike `dis_read_one` (which deliberately reads only 1 byte at a time —
/// see its own comment — to avoid silently prefetching bytes a *different*
/// concurrent reader of the same shared underlying stream might need),
/// bulk-reading here is safe: `readFully(buf, off, len)` is itself a bulk
/// request for exactly `len` bytes, so requesting up to the *remaining*
/// unfulfilled portion of that same `len` via one `read([BII)I` call never
/// reads a single byte past what the caller already asked for.
///
/// `this`/`buf` are pinned and re-fetched via `read_native_pin` around each
/// `invoke_virtual` call (which can trigger a moving GC) — a version of this
/// fix landed concurrently on `dev` (`5a457f969`) without this pinning,
/// leaving `inner`/`buf` referenced across the loop via stale `ObjectRef`s
/// if a GC moves them mid-call; kept the pinned version here.
pub(crate) fn dis_read_fully_impl(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    buf: ObjectRef,
    off: usize,
    len: usize,
) -> MethodCallResult {
    let this_pin = ctx.pin_native_root(this);
    let buf_pin = ctx.pin_native_root(buf);
    let mut total = 0usize;
    while total < len {
        let this_cur = ctx.read_native_pin(this_pin, this);
        let inner = match ctx.get_field(this_cur, DIS_FIELD_IN) {
            Value::Object(Some(s)) => s,
            _ => {
                ctx.unpin_native_roots(this_pin);
                return Err(eof_exception());
            }
        };
        let buf_cur = ctx.read_native_pin(buf_pin, buf);
        let n = match ctx.invoke_virtual(
            inner,
            "read",
            "([BII)I",
            &[
                Value::Object(Some(buf_cur)),
                Value::Int((off + total) as i32),
                Value::Int((len - total) as i32),
            ],
        ) {
            Ok(Some(Value::Int(n))) if n > 0 => n as usize,
            Ok(Some(Value::Int(0))) => {
                // Non-compliant streams sometimes return 0 for a non-empty
                // request instead of blocking — same edge case `dis_read_one`
                // guards against. Fall back to one scalar byte rather than
                // treating no-progress-this-call as EOF.
                let this_cur = ctx.read_native_pin(this_pin, this);
                match dis_read_one(ctx, this_cur) {
                    Ok(b) if b >= 0 => {
                        let buf_cur = ctx.read_native_pin(buf_pin, buf);
                        ctx.set_array_element(buf_cur, off + total, Value::Int(b));
                        1
                    }
                    Ok(_) => {
                        ctx.unpin_native_roots(this_pin);
                        return Err(eof_exception());
                    }
                    Err(e) => {
                        ctx.unpin_native_roots(this_pin);
                        return Err(e);
                    }
                }
            }
            Ok(_) => {
                ctx.unpin_native_roots(this_pin);
                return Err(eof_exception());
            }
            Err(e) => {
                ctx.unpin_native_roots(this_pin);
                return Err(e);
            }
        };
        total += n;
    }
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

pub(crate) fn native_dis_skip_bytes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let n = match args.get(1) {
        Some(Value::Int(v)) => *v as i64,
        _ => 0,
    };
    if n <= 0 {
        return Ok(Some(Value::Int(0)));
    }
    // PERF FIX (2026-07-13, STW-takeover-cluster residual investigation):
    // same byte-by-byte-via-dis_read_one issue as native_dis_read_bytes/
    // dis_read_fully_impl above — read-and-discard in bulk instead, capped
    // at an 8 KiB scratch buffer per call so a huge `n` doesn't itself
    // allocate a huge array.
    //
    // `inner` (fetched once before the loop) is pinned and re-fetched via
    // `read_native_pin` each iteration — it's reused across multiple
    // `invoke_virtual` calls, any of which can trigger a moving GC; the
    // originally-landed version of this fix only pinned `scratch`, leaving
    // `inner` referenced via a potentially-stale `ObjectRef` after the first
    // GC-triggering call (see `dis_read_fully_impl`'s doc comment for the
    // same class of gap in a sibling function).
    const SKIP_CHUNK: i64 = 8192;
    let this_pin = ctx.pin_native_root(this);
    let inner = match ctx.get_field(this, DIS_FIELD_IN) {
        Value::Object(Some(s)) => s,
        _ => {
            ctx.unpin_native_roots(this_pin);
            return Ok(Some(Value::Int(0)));
        }
    };
    // Consume the wrapped stream's already-buffered bytes in Rust first - see
    // `dis_fast_skip`. An attribute smaller than what the buffer holds (the
    // common case) is skipped entirely here, with no scratch allocation and no
    // VM re-entry. `skipBytes` is allowed to return short, so if the buffer
    // runs out mid-skip the loop below finishes the job.
    let fast_skipped = dis_fast_skip(ctx, inner, n as usize) as i64;
    if fast_skipped >= n {
        ctx.unpin_native_roots(this_pin);
        return Ok(Some(Value::Int(fast_skipped as i32)));
    }
    // `inner` gets its own pin handle (distinct from `this_pin`) — `pin_native_root`
    // pins one object per call; `unpin_native_roots(this_pin)` below releases
    // both, since pins are released from a handle onward.
    let inner_pin = ctx.pin_native_root(inner);
    let scratch = ctx.new_array(
        ArrayElementType::Byte,
        SKIP_CHUNK.min(n - fast_skipped) as usize,
    );
    let scratch_pin = ctx.pin_native_root(scratch);
    let mut scratch = scratch;
    let mut total_skipped = fast_skipped;
    while total_skipped < n {
        let inner_cur = ctx.read_native_pin(inner_pin, inner);
        let want = (n - total_skipped).min(SKIP_CHUNK) as i32;
        let scratch_cur = ctx.read_native_pin(scratch_pin, scratch);
        let read = match ctx.invoke_virtual(
            inner_cur,
            "read",
            "([BII)I",
            &[
                Value::Object(Some(scratch_cur)),
                Value::Int(0),
                Value::Int(want),
            ],
        ) {
            Ok(v) => v,
            Err(e) => {
                ctx.unpin_native_roots(this_pin);
                return Err(e);
            }
        };
        scratch = ctx.read_native_pin(scratch_pin, scratch);
        let n_read = match read {
            Some(Value::Int(v)) => v,
            _ => -1,
        };
        if n_read < 0 {
            break;
        }
        total_skipped += n_read as i64;
    }
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Int(total_skipped as i32)))
}

pub(crate) fn native_dis_available(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let inner = match ctx.get_field(this, DIS_FIELD_IN) {
        Value::Object(Some(s)) => s,
        _ => return Ok(Some(Value::Int(0))),
    };
    let result = ctx.invoke_virtual(inner, "available", "()I", &[])?;
    Ok(Some(result.unwrap_or(Value::Int(0))))
}

/// `DataInputStream.close()` closes the underlying stream
/// (`FilterInputStream.close` → `in.close()`). This registration used to be
/// `native_noop_void`, a straight no-op — safe for a `<init>`-not-registered
/// class that runs real bytecode for `close()`, but `DataInputStream` (and
/// `DataOutputStream`, whose own `close` is correctly implemented above as
/// `native_dos_close`) DOES get a native `close` registered directly on the
/// class. Since `DataInputStream` declares no bytecode of its own for
/// `close()` (it inherits `FilterInputStream.close()`), the interpreter's
/// dispatch prefers this class's own registered native over walking the
/// hierarchy to find that inherited real bytecode — so the no-op ran
/// instead, and `close()` never propagated to the wrapped stream. Traced via
/// a minimal, Spring-Boot-independent repro (`try (DataInputStream d = new
/// DataInputStream(tracingStream)) {}` never called `tracingStream.close()`)
/// while root-causing a `FileDataBlock` handle leak in Spring Boot loader's
/// `SecurityInfoTests`/`NestedJarFileTests` (`SecurityInfo.load()`'s
/// `JarEntriesStream.matches()` wraps each entry's content in `new
/// DataInputStream(...)`, so its close() never released the per-entry
/// `ZipContent.Entry.openContent()` reference).
pub(crate) fn native_dis_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    if let Value::Object(Some(inner)) = ctx.get_field(this, DIS_FIELD_IN) {
        ctx.invoke_virtual_declared("java/io/InputStream", inner, "close", "()V", &[])?;
    }
    Ok(None)
}

pub(crate) fn native_dos_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // GC: rooted across the allocation below and released after the re-read
    // (interpreter round i2 wave 62, lane L4, `[pin-balance] LEAK` census).
    let this_pin = ctx.pin_native_root(this);
    ctx.set_field(this, DOS_FIELD_OUT, args[1]);
    dos_set_written(ctx, this, 0);
    // Real JDK 25's `DataOutputStream(OutputStream)` constructor also
    // allocates `private final byte[] writeBuffer = new byte[8]` -- an
    // internal scratch buffer real bytecode for `writeChars`/`writeUTF`
    // (neither has a native override here) reads via a plain `getfield`
    // and hands to `jdk/internal/util/ByteArray.setUnsignedShort`/
    // `OutputStream.write([BII)V`. Every other DataOutputStream method is
    // natively overridden and never touches this field, so its absence was
    // invisible until real bytecode for one of those two methods ran: a
    // null `writeBuffer` there means `writeChars` silently loses every
    // byte with no exception raised (whichever of the null-array store or
    // the subsequent `out.write(null, 0, 2)` is the one swallowing it was
    // not pinned down further -- the observable, verified fact is just
    // that seeding this field fixes the symptom). Seed it
    // here exactly like the real constructor does, so any current or
    // future not-natively-overridden method that depends on it works.
    // See bug-h2-dataoutputstream-writechars-data-loss-FIXED.md.
    let write_buffer = ctx.new_array(ArrayElementType::Byte, 8);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    ctx.set_field_by_name(this, "writeBuffer", Value::Object(Some(write_buffer)));
    Ok(None)
}

/// Write one byte through the wrapped stream and bump `written`.
///
/// GC-safety (DOM18 stale-canary backtrace, 2026-07-15): the `write(I)V`
/// invoke can run a moving GC. `this` is pinned across it and re-read for
/// the `written` update, and the CURRENT address is returned — multi-byte
/// writers MUST rebind their local to the returned ref before the next call
/// (the pre-fix `writeUTF` loop handed a stale `this` to every iteration
/// after a GC, tripping CRATONVM_DBG_STALE_OBJREF in the WildFly Host
/// Controller).
/// Read the `written` counter, by name where the receiver has real field
/// names and from [`DOS_WRITTEN_SLOT`] otherwise.
pub(crate) fn dos_written(ctx: &mut dyn NativeContext, this: ObjectRef) -> i32 {
    if let Value::Int(w) = ctx.get_field_by_name(this, DOS_WRITTEN_FIELD) {
        return w;
    }
    if ctx.object_num_fields(this) > DOS_WRITTEN_SLOT {
        if let Value::Int(w) = ctx.get_field(this, DOS_WRITTEN_SLOT) {
            return w;
        }
    }
    0
}

/// Companion writer for [`dos_written`]. Writes BOTH spellings so a receiver
/// that later resolves by name agrees with one that only has slots.
pub(crate) fn dos_set_written(ctx: &mut dyn NativeContext, this: ObjectRef, v: i32) {
    // Round 15 wave 7 (lane drainA, w7a `R15DrainADosCloseOnce`): the slot
    // fallback only when the name does not resolve. On the REAL layout slot 1
    // is `FilterOutputStream.closed`, so writing the count there too marked
    // every stream that had written a byte closed (`close()` then returned at
    // once). No switch: a counter stored into an unrelated field is never an
    // answer, and this is the per-write path (no flag read here).
    let by_name = ctx
        .resolve_field_index_by_class_id(ctx.class_id_of_object(this), DOS_WRITTEN_FIELD)
        .is_some();
    ctx.set_field_by_name(this, DOS_WRITTEN_FIELD, Value::Int(v));
    if ctx.object_num_fields(this) > DOS_WRITTEN_SLOT && !by_name {
        ctx.set_field(this, DOS_WRITTEN_SLOT, Value::Int(v));
    }
}



pub(crate) fn dos_write_one(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    b: i32,
) -> Result<ObjectRef, cratonvm_types::error::MethodCallFailed> {
    // `new DataOutputStream(null).writeInt(1)` is an NPE on `out.write(..)`:
    // `FilterOutputStream` stores whatever it is handed and fails on use.
    // Returning `this` counted the bytes as written into nothing, and
    // `size()` then reported a length no stream holds.
    let inner = match ctx.get_field(this, DOS_FIELD_OUT) {
        Value::Object(Some(s)) => s,
        // JO7-6: `out.write(b)` in `write(int)` / `writeBoolean` / `writeByte`.
        _ => {
            return Err(helpful_npe_cannot_invoke(
                "java.io.OutputStream.write(int)",
                "this.out",
            ))
        }
    };
    let this_pin = ctx.pin_native_root(this);
    let r = ctx.invoke_virtual(inner, "write", "(I)V", &[Value::Int(b & 0xFF)]);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    r?;
    // `incCount(1)`: it saturates at `Integer.MAX_VALUE`; `written + 1`
    // overflowed (a debug-build panic past 2 GiB).
    dos_inc_count(ctx, this, 1);
    Ok(this)
}



/// JDK 25's `writeShort`/`writeChar`/`writeInt`/`writeLong` shape:
/// `writeBuffer[..n] = bytes; out.write(writeBuffer, 0, n); incCount(n);`.
/// The receiver's own `writeBuffer` is handed to the wrapped stream when it
/// is long enough (it is the array the JDK hands over), else a fresh
/// `byte[]`. A throwing `out.write` leaves `written` unchanged, as
/// `incCount` never runs.
pub(crate) fn dos_write_bulk(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    bytes: &[u8],
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    let n = bytes.len();
    // GC: `this` is rooted across the allocation and the dispatch below.
    let this_pin = ctx.pin_native_root(this);
    let scratch = match ctx.get_field_by_name(this, "writeBuffer") {
        Value::Object(Some(buf)) if ctx.array_length(buf) >= n => buf,
        _ => ctx.new_array(ArrayElementType::Byte, n),
    };
    let this = ctx.read_native_pin(this_pin, this);
    let inner = match ctx.get_field(this, DOS_FIELD_OUT) {
        Value::Object(Some(s)) => s,
        _ => {
            ctx.unpin_native_roots(this_pin);
            // JO7-6: `out.write(writeBuffer, 0, n)`.
            return Err(helpful_npe_cannot_invoke(DOS_OUT_WRITE_BULK, "this.out"));
        }
    };
    ctx.write_byte_array_from(scratch, 0, bytes);
    let r = ctx.invoke_virtual(
        inner,
        "write",
        "([BII)V",
        &[
            Value::Object(Some(scratch)),
            Value::Int(0),
            Value::Int(n as i32),
        ],
    );
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    r?;
    dos_inc_count(ctx, this, n as i32);
    Ok(())
}

/// `DataOutputStream.incCount`: `written + value`, pinned at
/// `Integer.MAX_VALUE` once it overflows.
pub(crate) fn dos_inc_count(ctx: &mut dyn NativeContext, this: ObjectRef, value: i32) {
    let temp = dos_written(ctx, this).wrapping_add(value);
    dos_set_written(ctx, this, if temp < 0 { i32::MAX } else { temp });
}

pub(crate) fn native_dos_write(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let b = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    dos_write_one(ctx, this, b)?;
    Ok(None)
}

/// `DataOutputStream.flush()` MUST propagate to the underlying stream — it is
/// `FilterOutputStream.flush()` (`out.flush()`). A no-op here silently strips
/// the flush, so e.g. `Manifest.write(new BufferedOutputStream(jos))` (the
/// `JarOutputStream(out, manifest)` constructor) leaves the manifest buffered
/// in the BufferedOutputStream and never written to the JAR — producing an
/// empty MANIFEST.MF.
pub(crate) fn native_dos_flush(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    match ctx.get_field(this, DOS_FIELD_OUT) {
        Value::Object(Some(inner)) => {
            ctx.invoke_virtual_declared("java/io/OutputStream", inner, "flush", "()V", &[])?;
        }
        // JO7-6: JDK 25's `DataOutputStream.flush()` is `out.flush()`, an NPE
        // on a null `out`; this flushed nothing and returned. The silent
        // return stays under `CRATONVM_NATIVE_HELPFUL_NPE=0`.
        _ if true => {
            return Err(helpful_npe_cannot_invoke(
                "java.io.OutputStream.flush()",
                "this.out",
            ));
        }
        _ => {}
    }
    Ok(None)
}

/// `DataOutputStream.close()` flushes then closes the underlying stream
/// (`FilterOutputStream.close`). A no-op loses any buffered output.
pub(crate) fn native_dos_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // Round 15 wave 7 (lane drainA; N7 of `r15w6-jni6-review-findings`):
    // `FilterOutputStream.close()` returns at once once `closed` is set, and
    // sets it before it flushes; a second `close()` here flushed and closed
    // the wrapped stream again (a counting or non-idempotent sink saw two).
    // The flag is the real field, `FilterOutputStream`'s own (a subclass's
    // same-named field must not answer; the synthetic layout has none and
    // keeps the old answer). The rest of the JDK body follows (`try { flush();
    // } finally { out.close(); }`: a close failure wins, the flush failure
    // suppressed into it; a null `out` is that NPE pair). w7a: the first cut
    // returned at once for every stream that had written a byte, because
    // `dos_set_written` also stored the count into slot 1 -- `closed` on the
    // real layout (fixed there). `CRATONVM_COMPAT_DATA_OUTPUT_CLOSE_ONCE=0`
    // restores the old body.
    let jdk_body = true;
    let closed_slot = ctx
        .resolve_field_index("java/io/FilterOutputStream", "closed")
        .filter(|&slot| {
            let cid = ctx.class_id_of_object(this);
            jdk_body
                && slot < ctx.object_num_fields(this)
                && ctx
                    .class_id_by_name("java/io/FilterOutputStream")
                    .is_some_and(|filter| cid == filter || ctx.is_subclass(cid, filter))
        });
    if let Some(slot) = closed_slot {
        if matches!(ctx.get_field(this, slot), Value::Int(v) if v != 0) {
            return Ok(None);
        }
        ctx.set_field(this, slot, Value::Int(1));
    }
    if jdk_body && !matches!(ctx.get_field(this, DOS_FIELD_OUT), Value::Object(Some(_))) {
        // `flush()` is `out.flush()`, then `finally { out.close(); }`.
        let flush_failure = helpful_npe_cannot_invoke("java.io.OutputStream.flush()", "this.out");
        let close_failure = helpful_npe_cannot_invoke("java.io.OutputStream.close()", "this.out");
        return Err(dos_close_failure(ctx, flush_failure, close_failure));
    }
    if let Value::Object(Some(inner)) = ctx.get_field(this, DOS_FIELD_OUT) {
        // Family-1 fix (cce0079): the `flush` dispatch is GC-capable —
        // refresh `inner` before the `close` dispatch, or close() runs on a
        // stale/wrong stream (leaking the real one).
        //
        // The flush PROPAGATES too, and its failure wins: `FilterOutputStream
        // .close()` rethrows the flush exception from its `catch (Throwable)`
        // and only *suppresses* a close failure into it. The close beneath was
        // already propagating; the flush beside it was not, so a
        // `DataOutputStream` over a full disk reported a clean close.
        // W7-57-close-flush-swallow-sweep.md
        let inner_pin = ctx.pin_native_root(inner);
        let flushed = ctx
            .invoke_virtual_declared("java/io/OutputStream", inner, "flush", "()V", &[])
            .map(|_| ());
        let inner = ctx.read_native_pin(inner_pin, inner);
        ctx.unpin_native_roots(inner_pin);
        let closed = ctx
            .invoke_virtual_declared("java/io/OutputStream", inner, "close", "()V", &[])
            .map(|_| ());
        if jdk_body {
            // JDK: when both fail, the CLOSE failure is thrown with the flush
            // failure suppressed into it.
            return match (flushed, closed) {
                (Err(f), Err(c)) => Err(dos_close_failure(ctx, f, c)),
                (Err(f), Ok(())) => Err(f),
                (Ok(()), Err(c)) => Err(c),
                (Ok(()), Ok(())) => Ok(None),
            };
        }
        flushed?;
        closed?;
    }
    Ok(None)
}

/// `FilterOutputStream.close()`'s `catch (Throwable closeException) { if
/// (flushException != closeException) closeException.addSuppressed(
/// flushException); throw closeException; }`. Both failures are made Java
/// throwables (a native NPE through its `(String)` constructor) so the
/// suppressed one is visible; when either cannot be built, the close failure
/// alone is the answer.
pub(crate) fn dos_close_failure(
    ctx: &mut dyn NativeContext,
    flush_failure: MethodCallFailed,
    close_failure: MethodCallFailed,
) -> MethodCallFailed {
    let flush_exc = match dos_failure_object(ctx, flush_failure) {
        Ok(exc) => exc,
        Err(_) => return close_failure,
    };
    let flush_pin = ctx.pin_native_root(flush_exc);
    let close_exc = match dos_failure_object(ctx, close_failure) {
        Ok(exc) => exc,
        Err(original) => {
            ctx.unpin_native_roots(flush_pin);
            return original;
        }
    };
    let flush_exc = ctx.read_native_pin(flush_pin, flush_exc);
    if flush_exc.as_ptr() != close_exc.as_ptr() {
        let close_pin = ctx.pin_native_root(close_exc);
        let added = ctx.invoke_virtual(
            close_exc,
            "addSuppressed",
            "(Ljava/lang/Throwable;)V",
            &[Value::Object(Some(flush_exc))],
        );
        let close_exc = ctx.read_native_pin(close_pin, close_exc);
        ctx.unpin_native_roots(flush_pin);
        if let Err(thrown) = added {
            return thrown;
        }
        return MethodCallFailed::ExceptionThrown(close_exc);
    }
    ctx.unpin_native_roots(flush_pin);
    MethodCallFailed::ExceptionThrown(close_exc)
}

/// `failure` as a Java throwable: a thrown one as is, a native NPE built
/// through `NullPointerException(String)` (its message kept). `Err` hands any
/// other failure back unchanged.
pub(crate) fn dos_failure_object(
    ctx: &mut dyn NativeContext,
    failure: MethodCallFailed,
) -> Result<ObjectRef, MethodCallFailed> {
    match failure {
        MethodCallFailed::ExceptionThrown(exc) => Ok(exc),
        MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::NullPointerException {
            message,
        })) => {
            let text = message.as_deref().map(|m| ctx.create_string_uninterned(m));
            match cratonvm_native_api::throwable_builder::construct_new(
                ctx,
                "java/lang/NullPointerException",
                "(Ljava/lang/String;)V",
                &[Value::Object(text)],
            ) {
                Ok(Some(exc)) => Ok(exc),
                Ok(None) => Err(RuntimeError::NullPointerException { message }.into()),
                Err(thrown) => Err(thrown),
            }
        }
        other => Err(other),
    }
}

pub(crate) fn native_dos_write_bytes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // `DataOutputStream.write(byte[], int, int)` validates before it writes —
    // `out.write(b, off, len)` reaches `OutputStream.write`, which null-checks
    // the array and range-checks the window. Returning `Ok(None)` for both made
    // them SILENT NO-OPS: the same shape as `FilterOutputStream`'s two in part
    // three, in the class most likely to be wrapping a socket.
    //
    //   dos.write(null, 0, 1)        HotSpot NPE,  this VM nothing
    //   dos.write(new byte[2], 1, 5) HotSpot IOOBE, this VM nothing
    //
    // MEASURED with `L4CensusTail.java`.
    //
    // JO7-6 (round 15 wave 6, lane jni6): the JDK body is `out.write(b, off,
    // len)`, so a null `out` is the first failure, whatever `b` is.
    if !matches!(ctx.get_field(this, DOS_FIELD_OUT), Value::Object(Some(_)))
    {
        return Err(helpful_npe_cannot_invoke(DOS_OUT_WRITE_BULK, "this.out"));
    }
    let buf = match args.get(1) {
        Some(Value::Object(None)) => {
            return Err(
                cratonvm_types::error::RuntimeError::NullPointerException { message: None }.into(),
            )
        }
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let off_i = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
    let len_i = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
    let cap = ctx.array_length(buf) as i64;
    if off_i < 0 || len_i < 0 || (len_i as i64) > cap - (off_i as i64) {
        return Err(
            cratonvm_types::error::RuntimeError::IndexOutOfBoundsException {
                message: Some(format!("off {off_i}, len {len_i}, buffer length {cap}")),
            }
            .into(),
        );
    }
    {
        // `out.write(b, off, len); incCount(len);` -- the caller's own array,
        // one call, the count moved only once it returned.
        let this_pin = ctx.pin_native_root(this);
        let inner = match ctx.get_field(this, DOS_FIELD_OUT) {
            Value::Object(Some(s)) => s,
            _ => {
                ctx.unpin_native_roots(this_pin);
                return Err(helpful_npe_cannot_invoke(DOS_OUT_WRITE_BULK, "this.out"));
            }
        };
        let r = ctx.invoke_virtual(
            inner,
            "write",
            "([BII)V",
            &[
                Value::Object(Some(buf)),
                Value::Int(off_i),
                Value::Int(len_i),
            ],
        );
        let this = ctx.read_native_pin(this_pin, this);
        ctx.unpin_native_roots(this_pin);
        r?;
        dos_inc_count(ctx, this, len_i);
        Ok(None)
    }
}

pub(crate) fn native_dos_write_boolean(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let b = match args.get(1) {
        Some(Value::Int(v)) => {
            if *v != 0 {
                1
            } else {
                0
            }
        }
        _ => 0,
    };
    dos_write_one(ctx, this, b)?;
    Ok(None)
}

pub(crate) fn native_dos_write_short(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let v = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    {
        dos_write_bulk(ctx, this, &(v as u16).to_be_bytes())?;
        Ok(None)
    }
}

pub(crate) fn native_dos_write_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let v = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    {
        dos_write_bulk(ctx, this, &v.to_be_bytes())?;
        Ok(None)
    }
}

pub(crate) fn native_dos_write_long(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let v = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    {
        dos_write_bulk(ctx, this, &v.to_be_bytes())?;
        Ok(None)
    }
}

pub(crate) fn native_dos_write_float(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let f = match args.get(1) {
        Some(Value::Float(v)) => *v,
        _ => 0.0,
    };
    // `writeInt(Float.floatToIntBits(v))`: every NaN is written as the
    // canonical 0x7fc00000 (this wrote the raw payload).
    let bits = if f.is_nan() {
        0x7fc0_0000
    } else {
        f.to_bits() as i32
    };
    let new_args = [args[0], Value::Int(bits)];
    native_dos_write_int(ctx, &new_args)
}

pub(crate) fn native_dos_write_double(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let d = match args.get(1) {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    // `writeLong(Double.doubleToLongBits(v))`: the canonical NaN.
    let bits = if d.is_nan() {
        0x7ff8_0000_0000_0000
    } else {
        d.to_bits() as i64
    };
    let new_args = [args[0], Value::Long(bits)];
    native_dos_write_long(ctx, &new_args)
}

/// T2.4.19: `DataOutputStream.writeUTF` — encode in modified UTF-8
/// per JVMS §4.4.7 and write a 2-byte big-endian length followed by
/// the payload. Rejects strings whose encoded form exceeds 65535 bytes
/// with a `UTFDataFormatException` (an `IOException` only when that class
/// cannot be built) to match the JDK contract.
pub(crate) fn native_dos_write_utf(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // `writeUTF(null)` reaches `str.length()` and is an NPE. Encoding the
    // empty string instead wrote a well-formed two-byte zero-length record, so
    // the reader could not tell a bug from a genuinely empty string.
    let str_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Err(write_utf_null_npe()),
    };
    let bytes = if data_utf_units_exact() {
        // JO3-3 (round 15 wave 4, lane jdkonly5): the text's UTF-16 units,
        // each encoded on its own, so an unpaired surrogate is `ED xx xx` as
        // HotSpot writes it (a `&str` read turned it into U+FFFD); the
        // refusal carries `tooLongMsg`'s wording.
        write_utf_payload_units(ctx, str_obj)?
    } else {
        let s = ctx.read_string(str_obj).unwrap_or_default();
        let bytes = encode_modified_utf8(&s);
        if bytes.len() > 65535 {
            let message = format!("encoded string too long: {} bytes", bytes.len());
            let too_long = bytes.len();
            return Err(utf_data_format_exception(ctx, &message, || {
                cratonvm_types::error::RuntimeError::IOException {
                    message: format!(
                        "writeUTF: encoded string too long ({too_long} bytes, max 65535)"
                    ),
                }
                .into()
            }));
        }
        bytes
    };
    let len = bytes.len();
    {
        // `writeUTF(str, this)`: the length and payload in one array, then
        // `out.write(bytearr, 0, utflen + 2)` with `out` the stream ITSELF --
        // a virtual call, so a subclass overriding `write([BII)V` sees the
        // record, and `written` moves inside that `write`.
        let mut record = Vec::with_capacity(len + 2);
        record.extend_from_slice(&(len as u16).to_be_bytes());
        record.extend_from_slice(&bytes);
        let this_pin = ctx.pin_native_root(this);
        let arr = ctx.new_array(ArrayElementType::Byte, record.len());
        let this = ctx.read_native_pin(this_pin, this);
        ctx.unpin_native_roots(this_pin);
        ctx.write_byte_array_from(arr, 0, &record);
        ctx.invoke_virtual(
            this,
            "write",
            "([BII)V",
            &[
                Value::Object(Some(arr)),
                Value::Int(0),
                Value::Int(record.len() as i32),
            ],
        )?;
        Ok(None)
    }
}

/// The `NullPointerException` of `writeUTF(null)` (round 15 wave 5, lane
/// jdkonly7; w4a `R15Jdkonly5DataUtfUnits` V2 under `--compatible`). JDK 25's
/// body reaches `str.length()` in `DataOutputStream.writeUTF(String str,
/// DataOutput out)`, and HotSpot's helpful message names it:
/// `Cannot invoke "String.length()" because "str" is null` (the reference run's
/// `V2 first:` line; `--compatible` answered `null`). Shared by every `writeUTF`
/// native of this crate and `native-builtins`' synthetic `RandomAccessFile`.
/// `CRATONVM_COMPAT_WRITE_UTF_NULL_HELPFUL_NPE=0` restores the message-less
/// NPE (read only on the refusal).
pub fn write_utf_null_npe() -> MethodCallFailed {
    let message = true
    .then(|| "Cannot invoke \"String.length()\" because \"str\" is null".to_string());
    RuntimeError::NullPointerException { message }.into()
}



/// `out.write(byte[], int, int)` as HotSpot's helpful NPE spells the callee.
pub(crate) const DOS_OUT_WRITE_BULK: &str = "java.io.OutputStream.write(byte[], int, int)";

/// JO7-6 (round 15 wave 6, lane jni6): the JEP 358 message of an invoke on a
/// null receiver, `Cannot invoke "<method>" because "<what>" is null`, for a
/// native standing in for a JDK body whose bytecode makes that call -- the
/// natives threw a message-less NPE, where HotSpot (helpful NPEs on by
/// default since JDK 15) names the call. `method` is spelled as
/// `bytecodeUtils.cpp` prints it: the holder's external name (only
/// `java.lang.Object` / `java.lang.String` shortened), the method name, the
/// parameter types' external names joined by `", "`; `what` is the null
/// expression (`this.out` for `aload_0; getfield out`). Under
/// `CRATONVM_NATIVE_HELPFUL_NPE=0` the old message-less NPE. A
/// `-XX:-ShowCodeDetailsInExceptionMessages` run (HotSpot: null message) is
/// not visible to a native; neither is it to `write_utf_null_npe`.
pub(crate) fn helpful_npe_cannot_invoke(method: &str, what: &str) -> MethodCallFailed {
    let message = true
        .then(|| format!("Cannot invoke \"{method}\" because \"{what}\" is null"));
    RuntimeError::NullPointerException { message }.into()
}

/// `writeUTF`'s payload (without the two-byte length) for the `String`
/// `str_obj`, units-exact (JO3-3, round 15 wave 4 lane jdkonly5), or the
/// `UTFDataFormatException` HotSpot throws when it would exceed 65535 bytes,
/// with `DataOutputStream.tooLongMsg`'s text (built from units: its head and
/// tail may cut a surrogate pair). Allocates only on the refusal path. Shared
/// by every `writeUTF` native of this crate and `native-builtins`' synthetic
/// `RandomAccessFile`.
pub fn write_utf_payload_units(
    ctx: &mut dyn NativeContext,
    str_obj: ObjectRef,
) -> Result<Vec<u8>, MethodCallFailed> {
    let units = ctx.read_string_units(str_obj).unwrap_or_default();
    let utflen = modified_utf8_units_len(&units);
    if utflen <= 65535 {
        return Ok(encode_modified_utf8_units(&units));
    }
    let message = modified_utf8_too_long_message(&units, utflen);
    let text = String::from_utf16_lossy(&message);
    let msg = ctx.create_string_from_units(&message);
    Err(nio_native::construct_thrown(
        ctx,
        "java/io/UTFDataFormatException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(msg))],
        || RuntimeError::IOException { message: text }.into(),
    ))
}

pub(crate) fn native_dos_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(dos_written(ctx, this))))
}

// ===========================================================================
// Phase 32: java.nio.file — Path, Paths, Files
// ===========================================================================

/// The slot a synthetic `Path`'s String used to live in, unconditionally, and
/// still does on a mock or a VM that cannot load the platform's Path class.
///
/// **Not the answer on its own any more.** Where a real Path keeps its String
/// is `cratonvm_native_api::path_layout` — `stringValue`(2) on
/// `sun.nio.fs.UnixPath`, `path` on `WindowsPath` — and slot 0 there is `fs`.
/// This is the fallback arm of that map, kept because `read_path_str` still has
/// to serve objects built at the historical layout (the crate's own mock
/// fixtures do exactly that).
pub(crate) const PATH_FIELD_STR: usize = 0;

/// Resolve a numeric uid/gid to its name via the passwd/group database.
/// `db` is `/etc/passwd` or `/etc/group`; both are `name:x:<id>:...` records.
/// Returns `None` off Unix, when the file is unreadable, or when the id has no
/// entry — callers then use the decimal id, exactly like the JDK's own
/// `UnixUserPrincipals` fallback.
pub(crate) fn unix_id_name(db: &str, id: i32) -> Option<String> {
    #[cfg(not(unix))]
    {
        let _ = (db, id);
        return None;
    }
    #[cfg(unix)]
    {
        let text = std::fs::read_to_string(db).ok()?;
        for line in text.lines() {
            let mut parts = line.split(':');
            let name = parts.next()?;
            let _passwd = parts.next();
            let entry_id = parts.next()?.trim().parse::<i32>().ok();
            if entry_id == Some(id) && !name.is_empty() {
                return Some(name.to_string());
            }
        }
        None
    }
}

pub(crate) fn register_nio_file_natives(registry: &mut NativeMethodRegistry) {
    let paths = "java/nio/file/Paths";
    let files = "java/nio/file/Files";

    // JDK 25 UnixFileSystem initializes this dispatcher during early real-JDK
    // filesystem setup. Return no optional capabilities so Java falls back to
    // portable paths instead of failing class initialization.
    //
    // KEEP: the return value is a CAPABILITY BITMASK (openat/futimes/birthtime
    // /…), not a status code — `0` is the honest "none of these syscalls are
    // available through this VM" answer, and the JDK's own
    // `UnixNativeDispatcher` treats it exactly that way by taking its portable
    // fallbacks. Claiming a capability we do not implement is what would break.
    //
    // TWO bits are claimed, under `--real-jdk` on Linux only -- the two
    // HotSpot 25 answers there, each paired with the natives it commits to
    // (SBR wave 2, lane fs, where the `sun/nio/fs` shadows over them retired):
    //
    //  * `SUPPORTS_BIRTHTIME` (`1 << 16`) tells
    //    `UnixFileAttributes.creationTime()` to trust the `st_birthtime_*`
    //    pair, which `unix_attrs_fill` fills from `statx` (`Metadata::created`)
    //    as HotSpot's `stat0` does. Left at 0, real `creationTime()` answers
    //    `ctime` where HotSpot answers the birth time.
    //  * `SUPPORTS_OPENAT` (`1 << 1`) makes real
    //    `UnixFileSystemProvider.newDirectoryStream` build the
    //    `UnixSecureDirectoryStream` HotSpot builds (the type a secure
    //    recursive delete checks for); its `dup`/`fdopendir`/`openat0`/
    //    `fstatat0`/`unlinkat0`/`renameat0`/`fchmodat0` are registered with it.
    //
    // `SUPPORTS_XATTR` stays off: the `fgetxattr0` family is not implemented.
    // Both policies report the capabilities implemented by these native primitives.
    registry.register(
        "sun/nio/fs/UnixNativeDispatcher",
        "init",
        "()I",
        |ctx, _args| {
            const SUPPORTS_OPENAT: i32 = 1 << 1;
            const SUPPORTS_BIRTHTIME: i32 = 1 << 16;
            let caps = if cfg!(target_os = "linux") {
                SUPPORTS_OPENAT | SUPPORTS_BIRTHTIME
            } else {
                0
            };
            Ok(Some(cratonvm_types::Value::Int(caps)))
        },
        NativeKind::Bridge,
    );
    // `UnixUserPrincipals.fromUid(uid)` / `fromGid(gid)` — reached from
    // `UnixFileAttributes.owner()`/`group()`, i.e. from `Files.getOwner` and
    // from any `PosixFileAttributes` consumer (Spring Boot's
    // `ApplicationTemp` ownership check among them). Without these the call
    // raised `UnsatisfiedLinkError` on the very first owner lookup.
    //
    // The real dispatcher throws `UnixException` for an unknown id and
    // `fromUid`/`fromGid` then fall back to the decimal id as the name; we
    // return those same decimal bytes directly rather than synthesising a
    // `UnixException`, which is indistinguishable to every caller.
    registry.register(
        "sun/nio/fs/UnixNativeDispatcher",
        "getpwuid",
        "(I)[B",
        |ctx, args| {
            let uid = args.first().and_then(|v| v.as_int()).unwrap_or(0);
            let name = unix_id_name("/etc/passwd", uid).unwrap_or_else(|| uid.to_string());
            let bytes = name.as_bytes();
            let arr = ctx.new_array(ArrayElementType::Byte, bytes.len());
            ctx.write_byte_array_from(arr, 0, bytes);
            Ok(Some(Value::Object(Some(arr))))
        },
        NativeKind::Bridge,
    );
    registry.register(
        "sun/nio/fs/UnixNativeDispatcher",
        "getgrgid",
        "(I)[B",
        |ctx, args| {
            let gid = args.first().and_then(|v| v.as_int()).unwrap_or(0);
            let name = unix_id_name("/etc/group", gid).unwrap_or_else(|| gid.to_string());
            let bytes = name.as_bytes();
            let arr = ctx.new_array(ArrayElementType::Byte, bytes.len());
            ctx.write_byte_array_from(arr, 0, bytes);
            Ok(Some(Value::Object(Some(arr))))
        },
        NativeKind::Bridge,
    );
    registry.register(
        "sun/nio/fs/UnixNativeDispatcher",
        "getcwd",
        "()[B",
        |ctx, _args| {
            let cwd = std::env::current_dir().map_err(io_err)?;
            let text = cwd.to_string_lossy();
            let bytes = text.as_bytes();
            let arr = ctx.new_array(ArrayElementType::Byte, bytes.len());
            ctx.write_byte_array_from(arr, 0, bytes);
            Ok(Some(Value::Object(Some(arr))))
        },
        NativeKind::Bridge,
    );

    // `UnixNativeDispatcher.open0(long pathAddress, int oflags, int mode)` -- the
    // door under `UnixChannelFactory.open`, i.e. under every REAL
    // `FileChannel.open` / `Files.newByteChannel` under `--real-jdk` (the
    // synthetic `FileChannel.open` native is dropped there). Without it the
    // first `FileChannel.open` died with
    // `UnsatisfiedLinkError: sun/nio/fs/UnixNativeDispatcher.open0(JII)I`
    // (`ResourceTests.readableChannelProvidesContent`, `FileSystemResource`
    // `readableChannel()`). The fd it returns is an `FdTable` id, the same id
    // space the `FileDispatcherImpl` natives (registered above) read from
    // `FileDescriptor.fd`.
    registry.register(
        "sun/nio/fs/UnixNativeDispatcher",
        "open0",
        "(JII)I",
        native_unix_open0,
        NativeKind::Bridge,
    );

    // `stat0` / `lstat0` / `fstat0` / `access0` -- the metadata siblings of
    // `open0`, and genuinely `ACC_NATIVE` in the real JDK the same way `open0`
    // is, so registering them wins over real bytecode in EITHER mode: door (d)
    // of `docs/architecture/natives-over-real-jdk-classes.md` §1, no `Code`
    // attribute exists to prefer.
    //
    // What they unblock, from the parallel measurement that landed on `dev`:
    // without `stat0` specifically, every `--real-jdk` call that reached
    // `Files.isRegularFile`/`isDirectory`/etc. through real bytecode (which
    // `--real-jdk` prefers there, since those `Files`-level natives are
    // `Bridge` shadowing real, non-native bytecode) died with an
    // `UnsatisfiedLinkError` the first time it walked down to
    // `UnixFileSystemProvider` -> `UnixFileAttributes` -> here. That includes
    // `Security`'s own bootstrap stat-ing `java.security`, so every
    // `--real-jdk` `KeyStore`/`Security`/`Provider` call was affected, not
    // only the ones that look like file IO.
    //
    // The rest of the batch is the layer below: wave 18 ran the lane's probe
    // battery, implemented exactly the natives the run named in an
    // `UnsatisfiedLinkError`, and re-ran. See `native_unix_unlink0` for why
    // the set is what it is and what is deliberately left out.
    //
    // Registered only on Unix, which is the one place this block disagrees
    // with the `dev` side it is merged with: that side registered
    // unconditionally and gave the fill helper a `cfg(not(unix))` no-op. On
    // Windows `UnixNativeDispatcher` is declared by no image, so a
    // registration there is a bucket-E row that `--real-jdk` refuses -- and a
    // native that can only answer `ENOSYS` is not worth a row. Nothing on the
    // Linux side changes: every triple that side registered is registered
    // here.
    #[cfg(unix)]
    {
        registry.register(
            "sun/nio/fs/UnixNativeDispatcher",
            "stat0",
            "(JLsun/nio/fs/UnixFileAttributes;)I",
            native_unix_stat0,
            NativeKind::Bridge,
        );
        registry.register(
            "sun/nio/fs/UnixNativeDispatcher",
            "lstat0",
            "(JLsun/nio/fs/UnixFileAttributes;)V",
            native_unix_lstat0,
            NativeKind::Bridge,
        );
        registry.register(
            "sun/nio/fs/UnixNativeDispatcher",
            "fstat0",
            "(ILsun/nio/fs/UnixFileAttributes;)V",
            native_unix_fstat0,
            NativeKind::Bridge,
        );
        registry.register(
            "sun/nio/fs/UnixNativeDispatcher",
            "access0",
            "(JI)I",
            native_unix_access0,
            NativeKind::Bridge,
        );
        for (name, descriptor, cb) in [
            (
                "unlink0",
                "(J)V",
                native_unix_unlink0 as cratonvm_native_api::NativeCallback,
            ),
            ("rmdir0", "(J)V", native_unix_rmdir0),
            ("mkdir0", "(JI)V", native_unix_mkdir0),
            ("rename0", "(JJ)V", native_unix_rename0),
            ("link0", "(JJ)V", native_unix_link0),
            ("symlink0", "(JJ)V", native_unix_symlink0),
            ("readlink0", "(J)[B", native_unix_readlink0),
            ("realpath0", "(J)[B", native_unix_realpath0),
            ("chmod0", "(JI)V", native_unix_chmod0),
            ("chown0", "(JII)V", native_unix_chown0),
            ("lchown0", "(JII)V", native_unix_lchown0),
            ("close0", "(I)V", native_unix_close0),
            ("strerror", "(I)[B", native_unix_strerror),
        ] {
            registry.register(
                "sun/nio/fs/UnixNativeDispatcher",
                name,
                descriptor,
                cb,
                NativeKind::Bridge,
            );
        }
        // SBR wave 2 (lane fs). The layer below the retired
        // `sun/nio/fs` Linux shadows (`RETIRED_SHADOW_SBR_FS_TRIPLES`): with
        // `LinuxFileSystemProvider.newDirectoryStream`/`copy`,
        // `getFileAttributeView` and the `UnixDirectoryStream` rows handed to
        // real bytecode, these are the natives it names -- MEASURED with the
        // `CRATONVM_ENFORCE_NATIVE_SHADOW` dial over the suite
        // (`futimens0` in `RFileTimes`, the `UnixDirectoryStream` family in
        // `RJdkNio`/`RCrypto`'s `JceSecurity` policy-directory scan).
        //
        // Registered in BOTH modes, explicitly `Bridge`: each is a genuine
        // `ACC_NATIVE` method, so `--real-jdk` must only ever REFUSE rows
        // relative to `--compatible`, never add them
        // (`the native-registry census::strict_registry_drops_only_the_stubs`). In
        // `--compatible` a correct native for an `ACC_NATIVE` method can only
        // replace an `UnsatisfiedLinkError` with HotSpot's answer; `init()`
        // still reports no capabilities there, so the `*at` set stays unused.
        {
            for (class, name, descriptor, cb) in [
                (
                    "sun/nio/fs/UnixNativeDispatcher",
                    "opendir0",
                    "(J)J",
                    native_unix_opendir0 as cratonvm_native_api::NativeCallback,
                ),
                (
                    "sun/nio/fs/UnixNativeDispatcher",
                    "readdir0",
                    "(J)[B",
                    native_unix_readdir0,
                ),
                (
                    "sun/nio/fs/UnixNativeDispatcher",
                    "closedir",
                    "(J)V",
                    native_unix_closedir,
                ),
                (
                    "sun/nio/fs/UnixNativeDispatcher",
                    "futimens0",
                    "(IJJ)V",
                    native_unix_futimens0,
                ),
                (
                    "sun/nio/fs/UnixNativeDispatcher",
                    "utimensat0",
                    "(IJJJI)V",
                    native_unix_utimensat0,
                ),
                (
                    "sun/nio/fs/UnixNativeDispatcher",
                    "fchmod0",
                    "(II)V",
                    native_unix_fchmod0,
                ),
                (
                    "sun/nio/fs/UnixNativeDispatcher",
                    "fchown0",
                    "(III)V",
                    native_unix_fchown0,
                ),
                // The secure-directory-stream set `init()`'s `SUPPORTS_OPENAT`
                // answer commits to -- see `native_unix_openat0`'s block.
                (
                    "sun/nio/fs/UnixNativeDispatcher",
                    "dup",
                    "(I)I",
                    native_unix_dup,
                ),
                (
                    "sun/nio/fs/UnixNativeDispatcher",
                    "fdopendir",
                    "(I)J",
                    native_unix_fdopendir,
                ),
                (
                    "sun/nio/fs/UnixNativeDispatcher",
                    "openat0",
                    "(IJII)I",
                    native_unix_openat0,
                ),
                (
                    "sun/nio/fs/UnixNativeDispatcher",
                    "unlinkat0",
                    "(IJI)V",
                    native_unix_unlinkat0,
                ),
                (
                    "sun/nio/fs/UnixNativeDispatcher",
                    "renameat0",
                    "(IJIJ)V",
                    native_unix_renameat0,
                ),
                (
                    "sun/nio/fs/UnixNativeDispatcher",
                    "fchmodat0",
                    "(IJII)V",
                    native_unix_fchmodat0,
                ),
                // glibc's `fchmodat(.., AT_SYMLINK_NOFOLLOW)` is `ENOTSUP` on a
                // symlink; answering "unsupported" sends the JDK down the same
                // `openat`+`fchmod` arm HotSpot takes when its probe says so.
                (
                    "sun/nio/fs/UnixNativeDispatcher",
                    "fchmodatNoFollowSupported0",
                    "()Z",
                    |_ctx, _args| Ok(Some(Value::Int(0))),
                ),
                (
                    "sun/nio/fs/UnixFileSystem",
                    "bufferedCopy0",
                    "(IIJIJ)V",
                    native_unix_buffered_copy0,
                ),
                // `LinuxNativeDispatcher.<clinit>` calls `init()`; without it
                // the class never initialises and `LinuxFileSystem.directCopy`
                // dies before `posix_fadvise`. Both are advisory in the JDK
                // too (`init` caches symbols, `posix_fadvise` is a hint).
                (
                    "sun/nio/fs/LinuxNativeDispatcher",
                    "init",
                    "()V",
                    |_ctx, _args| Ok(None),
                ),
                (
                    "sun/nio/fs/LinuxNativeDispatcher",
                    "posix_fadvise",
                    "(IJJI)I",
                    |_ctx, _args| Ok(Some(Value::Int(0))),
                ),
                (
                    "sun/nio/fs/LinuxNativeDispatcher",
                    "directCopy0",
                    "(IIJ)I",
                    native_linux_direct_copy0,
                ),
            ] {
                registry.register(class, name, descriptor, cb, NativeKind::Bridge);
            }
            #[cfg(target_os = "linux")]
            for (name, descriptor, cb) in [
                (
                    "fstatat0",
                    "(IJILsun/nio/fs/UnixFileAttributes;)V",
                    native_unix_fstatat0 as cratonvm_native_api::NativeCallback,
                ),
                ("flistxattr", "(IJI)I", native_unix_flistxattr),
                ("fgetxattr0", "(IJJI)I", native_unix_fgetxattr0),
                ("fsetxattr0", "(IJJI)V", native_unix_fsetxattr0),
                (
                    "statvfs0",
                    "(JLsun/nio/fs/UnixFileStoreAttributes;)V",
                    native_unix_statvfs0,
                ),
                // SBR wave 5 (lane fs2): the watch service's inotify/wakeup
                // reads and writes, and the mount-table walk real
                // `LinuxFileSystem.getMountEntries` does for `getFileStore`.
                ("read0", "(IJI)I", native_unix_read0),
                ("write0", "(IJI)I", native_unix_write0),
                ("getlinelen", "(J)I", native_unix_getlinelen),
                ("rewind", "(J)V", native_unix_rewind),
            ] {
                registry.register(
                    "sun/nio/fs/UnixNativeDispatcher",
                    name,
                    descriptor,
                    cb,
                    NativeKind::Bridge,
                );
            }
            // SBR wave 5 (lane fs2): the genuine `ACC_NATIVE` methods under the
            // real `LinuxWatchService` (`RETIRED_SHADOW_SBR_FS2_TRIPLES`) and
            // the `LinuxNativeDispatcher` mount-table trio under real
            // `LinuxFileStore`. Both modes, explicitly `Bridge`, for the same
            // reason as the block above: `--real-jdk` may only refuse rows.
            // In `--compatible` nothing reaches them while the shadows win
            // (`LinuxWatchService.<init>` never runs there: the watch
            // service is minted natively).
            #[cfg(target_os = "linux")]
            for (class, name, descriptor, cb) in [
                (
                    "sun/nio/fs/LinuxWatchService",
                    "eventSize",
                    "()I",
                    native_linux_ws_event_size as cratonvm_native_api::NativeCallback,
                ),
                (
                    "sun/nio/fs/LinuxWatchService",
                    "eventOffsets",
                    "()[I",
                    native_linux_ws_event_offsets,
                ),
                (
                    "sun/nio/fs/LinuxWatchService",
                    "inotifyInit",
                    "()I",
                    native_linux_ws_inotify_init,
                ),
                (
                    "sun/nio/fs/LinuxWatchService",
                    "inotifyAddWatch",
                    "(IJI)I",
                    native_linux_ws_inotify_add_watch,
                ),
                (
                    "sun/nio/fs/LinuxWatchService",
                    "inotifyRmWatch",
                    "(II)V",
                    native_linux_ws_inotify_rm_watch,
                ),
                (
                    "sun/nio/fs/LinuxWatchService",
                    "configureBlocking",
                    "(IZ)V",
                    native_linux_ws_configure_blocking,
                ),
                (
                    "sun/nio/fs/LinuxWatchService",
                    "socketpair",
                    "([I)V",
                    native_linux_ws_socketpair,
                ),
                (
                    "sun/nio/fs/LinuxWatchService",
                    "poll",
                    "(II)I",
                    native_linux_ws_poll,
                ),
                (
                    "sun/nio/fs/LinuxNativeDispatcher",
                    "setmntent0",
                    "(JJ)J",
                    native_linux_setmntent0,
                ),
                (
                    "sun/nio/fs/LinuxNativeDispatcher",
                    "getmntent0",
                    "(JLsun/nio/fs/UnixMountEntry;JI)I",
                    native_linux_getmntent0,
                ),
                (
                    "sun/nio/fs/LinuxNativeDispatcher",
                    "endmntent",
                    "(J)V",
                    native_linux_endmntent,
                ),
            ] {
                registry.register(class, name, descriptor, cb, NativeKind::Bridge);
            }
        }
    }

    // Paths factory
    registry.register(
        paths,
        "get",
        "(Ljava/lang/String;[Ljava/lang/String;)Ljava/nio/file/Path;",
        native_paths_get, cratonvm_native_api::NativeKind::Bridge,
    );
    // The one-argument spelling is CratonVM's own. `Paths.get` is varargs on
    // every JDK, so `(Ljava/lang/String;)Ljava/nio/file/Path;` is declared by no
    // image and a census scores it `method-nowhere` — indistinguishable, from
    // the image alone, from a dead entry. It is pinned by
    // `io_tests::path_and_files_methods_registered`, which is what went red when
    // `dc55e8057` deleted it as dead. Restored 2026-08-10.
    registry.register(
        paths,
        "get",
        "(Ljava/lang/String;)Ljava/nio/file/Path;",
        native_paths_get_simple, cratonvm_native_api::NativeKind::Bridge,
    );

    // Path methods
    let path = "java/nio/file/Path";
    registry.register(
        path,
        "toString",
        "()Ljava/lang/String;",
        native_path_to_string, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        path,
        "getFileName",
        "()Ljava/nio/file/Path;",
        native_path_get_file_name, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        path,
        "getParent",
        "()Ljava/nio/file/Path;",
        native_path_get_parent, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        path,
        "getRoot",
        "()Ljava/nio/file/Path;",
        native_path_get_root, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(path, "isAbsolute", "()Z", native_path_is_absolute, cratonvm_native_api::NativeKind::Bridge);
    registry.register(
        path,
        "resolve",
        "(Ljava/lang/String;)Ljava/nio/file/Path;",
        native_path_resolve_str, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        path,
        "resolve",
        "(Ljava/nio/file/Path;)Ljava/nio/file/Path;",
        native_path_resolve_path, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        path,
        "toAbsolutePath",
        "()Ljava/nio/file/Path;",
        native_path_to_absolute, cratonvm_native_api::NativeKind::Bridge,
    );
    // `Path.toRealPath` has no native — without one, the abstract
    // interface-method dispatch falls back to returning a null Path,
    // which broke Jetty's `start.jar` launcher: `DirConfigSource.<init>`
    // does `dir.resolve("start.ini").normalize().toAbsolutePath()
    // .toRealPath()` inside a `catch (NoSuchFileException)` and expects
    // the throw when `start.ini` is absent. A null return slipped past
    // the catch and surfaced downstream as `FS.canReadFile(null)` NPE.
    registry.register(
        path,
        "toRealPath",
        "([Ljava/nio/file/LinkOption;)Ljava/nio/file/Path;",
        native_path_to_real_path, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        path,
        "normalize",
        "()Ljava/nio/file/Path;",
        native_path_normalize, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(path, "getNameCount", "()I", native_path_get_name_count, cratonvm_native_api::NativeKind::Bridge);
    registry.register(
        path,
        "getName",
        "(I)Ljava/nio/file/Path;",
        native_path_get_name, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        path,
        "startsWith",
        "(Ljava/lang/String;)Z",
        native_path_starts_with, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        path,
        "endsWith",
        "(Ljava/lang/String;)Z",
        native_path_ends_with, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(path, "toFile", "()Ljava/io/File;", native_path_to_file, cratonvm_native_api::NativeKind::Bridge);
    registry.register(path, "equals", "(Ljava/lang/Object;)Z", native_path_equals, cratonvm_native_api::NativeKind::Bridge);
    registry.register(path, "hashCode", "()I", native_path_hash_code, cratonvm_native_api::NativeKind::Bridge);
    registry.register(
        path,
        "compareTo",
        "(Ljava/nio/file/Path;)I",
        native_path_compare_to, cratonvm_native_api::NativeKind::Bridge,
    );

    // Files static methods
    registry.register(
        files,
        "exists",
        "(Ljava/nio/file/Path;[Ljava/nio/file/LinkOption;)Z",
        native_files_exists, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        files,
        "isDirectory",
        "(Ljava/nio/file/Path;[Ljava/nio/file/LinkOption;)Z",
        native_files_is_directory, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        files,
        "isRegularFile",
        "(Ljava/nio/file/Path;[Ljava/nio/file/LinkOption;)Z",
        native_files_is_regular_file, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(files, "size", "(Ljava/nio/file/Path;)J", native_files_size, cratonvm_native_api::NativeKind::Bridge);
    registry.register(
        files,
        "delete",
        "(Ljava/nio/file/Path;)V",
        native_files_delete, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        files,
        "deleteIfExists",
        "(Ljava/nio/file/Path;)Z",
        native_files_delete_if_exists, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        files,
        "createFile",
        "(Ljava/nio/file/Path;[Ljava/nio/file/attribute/FileAttribute;)Ljava/nio/file/Path;",
        native_files_create_file, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        files,
        "createDirectory",
        "(Ljava/nio/file/Path;[Ljava/nio/file/attribute/FileAttribute;)Ljava/nio/file/Path;",
        native_files_create_directory, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        files,
        "createDirectories",
        "(Ljava/nio/file/Path;[Ljava/nio/file/attribute/FileAttribute;)Ljava/nio/file/Path;",
        native_files_create_directories, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        files,
        "readAllBytes",
        "(Ljava/nio/file/Path;)[B",
        native_files_read_all_bytes, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        files,
        "readString",
        "(Ljava/nio/file/Path;)Ljava/lang/String;",
        native_files_read_string, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        files,
        "write",
        "(Ljava/nio/file/Path;[B[Ljava/nio/file/OpenOption;)Ljava/nio/file/Path;",
        native_files_write_bytes, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(files, "writeString", "(Ljava/nio/file/Path;Ljava/lang/CharSequence;[Ljava/nio/file/OpenOption;)Ljava/nio/file/Path;", native_files_write_string, cratonvm_native_api::NativeKind::Bridge);
    registry.register(
        files,
        "readAllLines",
        "(Ljava/nio/file/Path;)Ljava/util/List;",
        native_files_read_all_lines, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        files,
        "copy",
        "(Ljava/nio/file/Path;Ljava/nio/file/Path;[Ljava/nio/file/CopyOption;)Ljava/nio/file/Path;",
        native_files_copy, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        files,
        "move",
        "(Ljava/nio/file/Path;Ljava/nio/file/Path;[Ljava/nio/file/CopyOption;)Ljava/nio/file/Path;",
        native_files_move, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        files,
        "isReadable",
        "(Ljava/nio/file/Path;)Z",
        native_files_is_readable, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        files,
        "isWritable",
        "(Ljava/nio/file/Path;)Z",
        native_files_is_writable, cratonvm_native_api::NativeKind::Bridge,
    );

    // File.toPath()
    registry.register(
        "java/io/File",
        "toPath",
        "()Ljava/nio/file/Path;",
        native_file_to_path, cratonvm_native_api::NativeKind::Bridge,
    );
    }

pub(crate) fn alloc_path(ctx: &mut dyn NativeContext, path_str: &str) -> ObjectRef {
    // RA.6: Allocate under a Rust-owned synthetic subclass if it exists, else
    // under `java.nio.file.Path`.
    //
    // WIDTH AND SLOT COME FROM `path_layout`, not from a literal: this is the
    // third site in the workspace that produces this carrier, and the other two
    // are in `native-builtins`. Before 2026-09-10 all three wrote the String at
    // slot 0, which on the class `getClass()` reports (`sun.nio.fs.UnixPath`) is
    // where the owning `UnixFileSystem` belongs.
    let slots = cratonvm_native_api::path_layout::resolve(ctx);
    let path = match ctx.ensure_class_initialized("java/nio/file/Path") {
        Ok(cid) => {
            let real = ctx.class_num_total_fields(cid);
            let n = real.max(slots.width);
            ctx.alloc_object(cid, n)
        }
        Err(_) => ctx.alloc_object(cratonvm_types::ClassId::new(0), slots.width),
    };
    // GC: nothing in Java refers to this object yet, so this Rust local is
    // its ONLY reference — and the allocation below can collect. Under the
    // moving collector that leaves the local stale; under the Generational
    // collector's NON-MOVING young sweep the object is unreachable and gets
    // ZEROED in place. Pin across the allocation and read the live address
    // back (`unpinned-native-locals` family; `native-io` was outside the
    // 2026-08-25 audit's scope, which covered `native-builtins/src`).
    let __pin = ctx.pin_native_root(path);
    let stored = cratonvm_native_api::path_layout::stored_form(path_str);
    let s = ctx.create_string(&stored);
    let path = ctx.read_native_pin(__pin, path);
    ctx.set_field(path, slots.string, Value::Object(Some(s)));
    // Dual-write by name — covers a concrete real-JDK path type whose String
    // field is also called `path` (`sun.nio.fs.WindowsPath`), and no-ops when
    // the class has no such field. It cannot substitute for the indexed write
    // above: an object stamped with the INTERFACE resolves no `UnixPath` field
    // name at all, which is why two earlier by-name attempts at this were
    // measured inert.
    ctx.set_field_by_name(path, "path", Value::Object(Some(s)));
    // The encoded bytes, where the platform class keeps them separately from
    // the String — real `UnixPath.compareTo`/`hashCode`/`initOffsets` read
    // these, and a null array there is an NPE rather than a wrong answer.
    if let Some(bytes_slot) = slots.bytes {
        let raw = stored.as_bytes();
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, raw.len());
        ctx.write_byte_array_from(arr, 0, raw);
        let path = ctx.read_native_pin(__pin, path);
        ctx.set_field(path, bytes_slot, Value::Object(Some(arr)));
    }
    let path = ctx.read_native_pin(__pin, path);
    ctx.unpin_native_roots(__pin);
    path
}

/// Windows `Path` syntax validation, close enough to
/// `sun.nio.fs.WindowsPathParser`'s character checks for `Paths.get`/
/// `File.toPath()` to reject what real JDK rejects. A colon is only legal as
/// the second character of a drive specifier (`C:...`) — anywhere else
/// (including a bare `scheme:rest` string like Spring's `ping:foo`
/// `ProtocolResolver` probe) it's illegal, along with the usual reserved
/// characters and control bytes. `GenericApplicationContextTests.
/// getResourceWithCustomResourceLoader` relies on `FileSystemResourceLoader
/// .getResource("ping:foo")` throwing `InvalidPathException` on Windows
/// *before* any `ProtocolResolver` runs — `Paths.get`/`File.toPath()`
/// previously wrapped any string verbatim with no validation at all.
pub(crate) fn validate_windows_path(s: &str) -> Result<(), &'static str> {
    if !cfg!(windows) {
        return Ok(());
    }
    let bytes = s.as_bytes();
    // A `\\?\` verbatim prefix disables normalization; JDK still accepts
    // almost anything there, so skip further validation for that rare case.
    if bytes.len() >= 4
        && (bytes[0] == b'\\' || bytes[0] == b'/')
        && (bytes[1] == b'\\' || bytes[1] == b'/')
        && bytes[2] == b'?'
    {
        return Ok(());
    }
    // A UNC prefix (`\\`/`//`, already past the `\\?\` long-path form above)
    // must be followed by a non-empty hostname before the next separator —
    // real `WindowsPathParser.parse` throws "UNC path is missing hostname"
    // when it is not. `Paths.get("//")` is the minimal reproduction: two
    // separators and nothing after. Same fix as
    // `native-builtins`'s `p57_validate_windows_path`, kept in sync since
    // this copy is not shared (see that function's doc comment for why).
    if bytes.len() >= 2
        && (bytes[0] == b'\\' || bytes[0] == b'/')
        && (bytes[1] == b'\\' || bytes[1] == b'/')
    {
        let hostname_empty = bytes.len() == 2 || bytes[2] == b'\\' || bytes[2] == b'/';
        if hostname_empty {
            return Err("UNC path is missing hostname");
        }
    }
    // Index of the one legal colon (the drive specifier), if this path
    // starts with `<letter>:`.
    let drive_colon = if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        Some(1usize)
    } else {
        None
    };
    for (i, &b) in bytes.iter().enumerate() {
        if b == b':' && Some(i) != drive_colon {
            return Err("Illegal char <:>");
        }
        if b < 0x20 || matches!(b, b'<' | b'>' | b'"' | b'|' | b'?' | b'*') {
            return Err("Illegal char");
        }
    }
    Ok(())
}

/// A refusal of `sun.nio.fs.WindowsPathParser.parse(input, true)`: the
/// exception's `input` (after a `\\?\` prefix is stripped, as the JDK's is),
/// its `reason`, and its `index` (`-1`: the two-argument constructor's).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsPathRefusal {
    pub input: String,
    pub reason: String,
    pub index: i32,
}

/// Round 15 wave 6 (lane sweepC; `r15w4-jdkonly4-builder-review-findings`
/// R4): JDK 25 `WindowsPathParser.parse(input, true)`'s refusals, ported line
/// for line over UTF-16 units, so `getIndex()` and `getMessage()` (`Illegal
/// char <?> at index 1: a?b`) are HotSpot's. [`validate_windows_path`]
/// answered index-less reasons, skipped the `\\?\` checks, and missed the
/// `Trailing char < >` refusals. Indices are the JDK's: positions in `root +
/// input[off..]`, so a directory-relative `/a?b` (root `\`, `off` 0) reports
/// 3. Pure; every platform answers the same (the caller asks only on Windows).
pub fn windows_path_parser_refusal(input: &str) -> Option<WindowsPathRefusal> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Kind {
        Absolute,
        Unc,
        DriveRelative,
        DirectoryRelative,
        Relative,
    }
    fn is_slash(c: u16) -> bool {
        c == u16::from(b'\\') || c == u16::from(b'/')
    }
    // `reservedChars = "<>:\"|?*"` and `ch < ' '`.
    fn invalid(c: u16) -> bool {
        c < 0x20 || matches!(c, 0x3C | 0x3E | 0x3A | 0x22 | 0x7C | 0x3F | 0x2A)
    }
    fn next_non_slash(s: &[u16], mut off: usize) -> usize {
        while off < s.len() && is_slash(s[off]) {
            off += 1;
        }
        off
    }
    /// `nextSlash`: the first slash at or after `off`, or the offending char
    /// and its index.
    fn next_slash(s: &[u16], mut off: usize) -> Result<usize, (u16, usize)> {
        while off < s.len() && !is_slash(s[off]) {
            if invalid(s[off]) {
                return Err((s[off], off));
            }
            off += 1;
        }
        Ok(off)
    }
    fn starts_at(s: &[u16], at: usize, lit: &str) -> bool {
        lit.encode_utf16()
            .enumerate()
            .all(|(i, u)| s.get(at + i) == Some(&u))
    }
    fn unit(c: u16) -> String {
        String::from_utf16_lossy(&[c])
    }
    let all: Vec<u16> = input.encode_utf16().collect();
    let (units, expected): (Vec<u16>, Option<Kind>) = if starts_at(&all, 0, "\\\\?\\") {
        if starts_at(&all, 4, "UNC\\") {
            let mut v: Vec<u16> = "\\\\".encode_utf16().collect();
            v.extend_from_slice(&all[8..]);
            (v, Some(Kind::Unc))
        } else {
            (all[4..].to_vec(), Some(Kind::Absolute))
        }
    } else {
        (all, None)
    };
    let refuse = |reason: String, index: i32| {
        Some(WindowsPathRefusal {
            input: String::from_utf16_lossy(&units),
            reason,
            index,
        })
    };
    let len = units.len();
    let mut root: Vec<u16> = Vec::new();
    let mut kind = Kind::Relative;
    let mut off = 0usize;
    if len > 1 {
        let (c0, c1) = (units[0], units[1]);
        if is_slash(c0) && is_slash(c1) {
            kind = Kind::Unc;
            off = next_non_slash(&units, 2);
            let next = match next_slash(&units, off) {
                Ok(n) => n,
                Err((c, at)) => {
                    return refuse(
                        format!("Illegal character [{}] in path", unit(c)),
                        at as i32,
                    )
                }
            };
            if off == next {
                return refuse("UNC path is missing hostname".to_string(), -1);
            }
            let host = units[off..next].to_vec();
            off = next_non_slash(&units, next);
            let next = match next_slash(&units, off) {
                Ok(n) => n,
                Err((c, at)) => {
                    return refuse(
                        format!("Illegal character [{}] in path", unit(c)),
                        at as i32,
                    )
                }
            };
            if off == next {
                return refuse("UNC path is missing sharename".to_string(), -1);
            }
            root.extend("\\\\".encode_utf16());
            root.extend_from_slice(&host);
            root.push(u16::from(b'\\'));
            root.extend_from_slice(&units[off..next]);
            root.push(u16::from(b'\\'));
            off = next;
        } else if (c0 < 0x80 && (c0 as u8).is_ascii_alphabetic()) && c1 == u16::from(b':') {
            root.extend_from_slice(&units[0..2]);
            if len > 2 && is_slash(units[2]) {
                root.push(u16::from(b'\\'));
                off = 3;
                kind = Kind::Absolute;
            } else {
                off = 2;
                kind = Kind::DriveRelative;
            }
        }
    }
    // `off == 0`: directory-relative or (the initial `kind`) relative.
    if off == 0 && len > 0 && is_slash(units[0]) {
        kind = Kind::DirectoryRelative;
        root = vec![u16::from(b'\\')];
    }
    match expected {
        Some(Kind::Absolute) if kind != Kind::Absolute => {
            return refuse(
                "Long path prefix can only be used with an absolute path".to_string(),
                -1,
            )
        }
        Some(Kind::Unc) if kind != Kind::Unc => {
            return refuse(
                "Long UNC path prefix can only be used with a UNC path".to_string(),
                -1,
            )
        }
        _ => {}
    }
    // `normalize(root, input, off)`'s checks over `root + input[off..]`.
    let mut result = root.clone();
    result.extend_from_slice(&units[off..]);
    let mut pos = next_non_slash(&result, root.len());
    let mut src = pos;
    let mut last: u16 = 0;
    while pos < result.len() {
        let c = result[pos];
        if is_slash(c) {
            if last == u16::from(b' ') {
                return refuse("Trailing char < >".to_string(), pos as i32 - 1);
            }
            pos = next_non_slash(&result, pos);
            src = pos;
        } else {
            if invalid(c) {
                return refuse(format!("Illegal char <{}>", unit(c)), pos as i32);
            }
            last = c;
            pos += 1;
        }
    }
    if src != pos && last == u16::from(b' ') {
        return refuse("Trailing char < >".to_string(), pos as i32 - 1);
    }
    None
}

/// Construct and throw a real `java.nio.file.InvalidPathException` via its
/// public `(String input, String reason)` constructor, so `getMessage()`
/// (real JDK bytecode) includes `input` verbatim — required by
/// `assertThatExceptionOfType(InvalidPathException.class)
/// .withMessageContaining(pingLocation)`.
pub(crate) fn throw_invalid_path_exception(
    ctx: &mut dyn NativeContext,
    input: &str,
    reason: &str,
) -> MethodCallFailed {
    // Round 15 wave 4, lane jdkonly4
    // (`r15w3-jdkonly3-residual-new-object-init-builders-patch-FIXED-20260930.md`
    // #6): `nio_native::construct_thrown` (the object rooted across `<init>`,
    // a throwing constructor propagated) instead of `new_object` + an
    // `invoke` whose failure was ignored; both strings fresh (they were
    // interned). The input is rooted across the reason's allocation.
    let input_str = ctx.create_string_uninterned(input);
    let input_pin = ctx.pin_native_root(input_str);
    let reason_str = ctx.create_string_uninterned(reason);
    let input_str = ctx.read_native_pin(input_pin, input_str);
    ctx.unpin_native_roots(input_pin);
    nio_native::construct_thrown(
        ctx,
        "java/nio/file/InvalidPathException",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        &[
            Value::Object(Some(input_str)),
            Value::Object(Some(reason_str)),
        ],
        || {
            RuntimeError::IllegalArgumentException {
                message: format!("{reason}: {input}"),
            }
            .into()
        },
    )
}

/// Validate `path_str` as a Windows path before wrapping it in a synthetic
/// `Path` object; throws `InvalidPathException` (matching real JDK) instead
/// of silently accepting any string. Shared by the `Paths.get`/
/// `File.toPath()` entry points — the ONLY places a `Path` is built directly
/// from unvalidated user input (accessor natives like `getParent`/`getRoot`
/// split an already-validated path and don't need to re-check).
pub(crate) fn alloc_path_checked(ctx: &mut dyn NativeContext, path_str: &str) -> MethodCallResult {
    // Round 15 wave 6 (lane sweepC): the JDK parser's refusal
    // ([`windows_path_parser_refusal`]) under
    // `CRATONVM_COMPAT_WINDOWS_PATH_PARSER` (default on; read only when either
    // check refuses); `=0` keeps the index-less `validate_windows_path`.
    if cfg!(windows) {
        let parsed = windows_path_parser_refusal(path_str);
        let legacy = validate_windows_path(path_str);
        if parsed.is_some() || legacy.is_err() {
            if true
            {
                if let Some(r) = parsed {
                    return Err(throw_invalid_path_exception_indexed(
                        ctx, &r.input, &r.reason, r.index,
                    ));
                }
            } else if let Err(reason) = legacy {
                return Err(throw_invalid_path_exception(ctx, path_str, reason));
            }
        }
    }
    Ok(Some(Value::Object(Some(alloc_path(ctx, path_str)))))
}

/// [`throw_invalid_path_exception`] through the three-argument constructor:
/// `getIndex()` is `index`, and `getMessage()` carries ` at index N` when
/// `index > -1`, as `WindowsPathParser`'s indexed refusals do.
pub(crate) fn throw_invalid_path_exception_indexed(
    ctx: &mut dyn NativeContext,
    input: &str,
    reason: &str,
    index: i32,
) -> MethodCallFailed {
    let input_str = ctx.create_string_uninterned(input);
    let input_pin = ctx.pin_native_root(input_str);
    let reason_str = ctx.create_string_uninterned(reason);
    let input_str = ctx.read_native_pin(input_pin, input_str);
    ctx.unpin_native_roots(input_pin);
    nio_native::construct_thrown(
        ctx,
        "java/nio/file/InvalidPathException",
        "(Ljava/lang/String;Ljava/lang/String;I)V",
        &[
            Value::Object(Some(input_str)),
            Value::Object(Some(reason_str)),
            Value::Int(index),
        ],
        || {
            RuntimeError::IllegalArgumentException {
                message: format!("{reason}: {input}"),
            }
            .into()
        },
    )
}

pub(crate) fn read_path_str(ctx: &dyn NativeContext, path: ObjectRef) -> String {
    // Prefer by-name resolution for real concrete Path types.
    if let Value::Object(Some(s)) = ctx.get_field_by_name(path, "path") {
        if let Some(t) = ctx.read_string(s) {
            return cratonvm_native_api::path_layout::canonical_form(&t).into_owned();
        }
    }
    path_slot_string(ctx, path).unwrap_or_default()
}

/// The path String out of a `Path`'s own slot — **the one read** in this crate,
/// the twin of `nio_file.rs`'s `p57_path_slot_string`.
///
/// Two arms, and neither can mis-fire: `read_string` REFUSES a non-String, so
/// asking the historical slot 0 of a NEW-layout Path yields `None` (it holds
/// the `fs` object) rather than a mis-decoded path, and asking the mapped slot
/// of a LEGACY-layout object — this crate's own mock fixtures build one — is an
/// out-of-range read the accessor answers `None` for.
pub(crate) fn path_slot_string(ctx: &dyn NativeContext, path: ObjectRef) -> Option<String> {
    let canon = |t: String| cratonvm_native_api::path_layout::canonical_form(&t).into_owned();
    let mapped = cratonvm_native_api::path_layout::slots_for(ctx.vm_identity()).string;
    if let Value::Object(Some(s)) = ctx.get_field(path, mapped) {
        if let Some(t) = ctx.read_string(s) {
            return Some(canon(t));
        }
    }
    if mapped != PATH_FIELD_STR {
        if let Value::Object(Some(s)) = ctx.get_field(path, PATH_FIELD_STR) {
            if let Some(t) = ctx.read_string(s) {
                return Some(canon(t));
            }
        }
    }
    None
}

pub(crate) fn native_paths_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let s = match args.first() {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    };
    let mut parts: Vec<String> = Vec::new();
    if let Some(Value::Object(Some(arr))) = args.get(1) {
        let len = ctx.array_length(*arr);
        for i in 0..len {
            if let Value::Object(Some(part)) = ctx.get_array_element(*arr, i) {
                if let Some(part_str) = ctx.read_string(part) {
                    parts.push(part_str);
                }
            }
        }
    }
    // Round 15 wave 6 (lane patches6; `r15w6-jni6-review-findings` N2/N2b).
    // JDK `getPath(first, more...)`: an empty segment is skipped and a
    // separator goes only after non-empty text, so `Paths.get("", "b")` is
    // `b`, not `/b`. Windows joins with `\` and THEN parses: a separator run
    // the join makes at the start is a UNC prefix (`Path.of("/", "x")` is
    // `/\x`, "UNC path is missing sharename"), so the literal join is offered
    // to the parser first.
    if cfg!(windows) {
        let mut literal = s.clone();
        for part in parts.iter().filter(|p| !p.is_empty()) {
            if !literal.is_empty() {
                literal.push('\\');
            }
            literal.push_str(part);
        }
        if windows_path_parser_refusal(&literal).is_some() {
            return alloc_path_checked(ctx, &literal);
        }
    }
    // Otherwise the join-point run is collapsed, as the platform parser's
    // normalization would (on Unix `\` is a filename character).
    let is_sep = |c: char| c == '/' || (cfg!(windows) && c == '\\');
    let mut result = s;
    for part in parts.iter().filter(|p| !p.is_empty()) {
        if result.is_empty() {
            result.push_str(part);
            continue;
        }
        if !result.ends_with(is_sep) {
            result.push(std::path::MAIN_SEPARATOR);
        }
        result.push_str(part.trim_start_matches(is_sep));
    }
    alloc_path_checked(ctx, &result)
}



pub(crate) fn native_paths_get_simple(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let s = match args.first() {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    };
    alloc_path_checked(ctx, &s)
}

pub(crate) fn native_path_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // The stored String OBJECT, not a fresh copy of its text: `toString()` on
    // one Path twice hands back the same reference, which is what the real
    // `UnixPath` does out of its cached `stringValue` and what an identity
    // comparison in a caller's cache key depends on.
    let slot = cratonvm_native_api::path_layout::slots_for(ctx.vm_identity()).string;
    if let Value::Object(Some(s)) = ctx.get_field(this, slot) {
        if ctx.read_string(s).is_some() {
            return Ok(Some(Value::Object(Some(s))));
        }
    }
    if slot != PATH_FIELD_STR {
        if let Value::Object(Some(s)) = ctx.get_field(this, PATH_FIELD_STR) {
            if ctx.read_string(s).is_some() {
                return Ok(Some(Value::Object(Some(s))));
            }
        }
    }
    Ok(Some(Value::Object(None)))
}

pub(crate) fn native_path_get_file_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let s = read_path_str(ctx, this);
    let p = std::path::Path::new(&s);
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    if name.is_empty() {
        return Ok(Some(Value::Object(None)));
    }
    let result = alloc_path(ctx, &name);
    Ok(Some(Value::Object(Some(result))))
}

pub(crate) fn native_path_get_parent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let s = read_path_str(ctx, this);
    let p = std::path::Path::new(&s);
    match p.parent() {
        Some(parent) if parent.as_os_str().is_empty() => Ok(Some(Value::Object(None))),
        Some(parent) => {
            let result = alloc_path(ctx, &parent.to_string_lossy());
            Ok(Some(Value::Object(Some(result))))
        }
        None => Ok(Some(Value::Object(None))),
    }
}

/// keycloak-15: explicit Windows (sun.nio.fs.WindowsPath) root/name parsing.
/// `std::path::Component` does not reliably classify the drive/UNC prefix in this
/// build (it leaves `C:` as a Normal component), so getRoot/getNameCount/getName
/// must parse the prefix themselves. Returns `(root, names)` where `root` is the
/// JDK root string (e.g. `C:\`, `\\server\share\`, `C:`, `\`) or None for a
/// relative path, and `names` are the path elements after the root (curdir `.`
/// and parentdir `..` kept, matching HotSpot's name list).
pub(crate) fn parse_windows_path_root(s: &str) -> (Option<String>, Vec<String>) {
    let is_sep = |c: u8| c == b'\\' || c == b'/';
    let split_names = |rest: &str| -> Vec<String> {
        rest.split(|c| c == '\\' || c == '/')
            .filter(|seg| !seg.is_empty())
            .map(|seg| seg.to_string())
            .collect()
    };
    // Strip a `\\?\` (verbatim) prefix and parse the underlying form.
    let (work, verbatim) = {
        let b = s.as_bytes();
        if b.len() >= 4 && is_sep(b[0]) && is_sep(b[1]) && b[2] == b'?' && is_sep(b[3]) {
            (&s[4..], true)
        } else {
            (s, false)
        }
    };
    let wb = work.as_bytes();
    // Verbatim UNC: \\?\UNC\server\share\...
    if verbatim
        && wb.len() >= 4
        && work
            .get(..3)
            .map_or(false, |p| p.eq_ignore_ascii_case("UNC"))
        && is_sep(wb[3])
    {
        let after = &work[4..];
        let mut it = after.splitn(3, |c| c == '\\' || c == '/');
        let server = it.next().unwrap_or("");
        let share = it.next().unwrap_or("");
        let remainder = it.next().unwrap_or("");
        return (
            Some(format!("\\\\{}\\{}\\", server, share)),
            split_names(remainder),
        );
    }
    // UNC: \\server\share\...
    if wb.len() >= 2 && is_sep(wb[0]) && is_sep(wb[1]) {
        let after = &work[2..];
        let mut it = after.splitn(3, |c| c == '\\' || c == '/');
        let server = it.next().unwrap_or("");
        let share = it.next().unwrap_or("");
        if !server.is_empty() && !share.is_empty() {
            let remainder = it.next().unwrap_or("");
            return (
                Some(format!("\\\\{}\\{}\\", server, share)),
                split_names(remainder),
            );
        }
    }
    // Drive: `C:\...` / `C:/...` (absolute) or `C:foo` (drive-relative).
    if wb.len() >= 2 && (wb[0] as char).is_ascii_alphabetic() && wb[1] == b':' {
        let drive = format!("{}:", wb[0] as char);
        if wb.len() >= 3 && is_sep(wb[2]) {
            return (Some(format!("{}\\", drive)), split_names(&work[3..]));
        }
        return (Some(drive), split_names(&work[2..]));
    }
    // Single leading separator (root-relative on Windows): `\foo` / `/foo`.
    if !wb.is_empty() && is_sep(wb[0]) {
        return (Some("\\".to_string()), split_names(&work[1..]));
    }
    (None, split_names(work))
}

pub(crate) fn native_path_get_root(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let s = read_path_str(ctx, this);
    match parse_windows_path_root(&s).0 {
        Some(root) => {
            let result = alloc_path(ctx, &root);
            Ok(Some(Value::Object(Some(result))))
        }
        None => Ok(Some(Value::Object(None))),
    }
}

pub(crate) fn native_path_is_absolute(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let s = read_path_str(_ctx, this);
    let p = std::path::Path::new(&s);
    Ok(Some(Value::Int(if p.is_absolute() { 1 } else { 0 })))
}

pub(crate) fn native_path_resolve_str(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    };
    let base = read_path_str(ctx, this);
    let resolved = std::path::Path::new(&base)
        .join(&other)
        .to_string_lossy()
        .to_string();
    let result = alloc_path(ctx, &resolved);
    Ok(Some(Value::Object(Some(result))))
}

pub(crate) fn native_path_resolve_path(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(o))) => read_path_str(ctx, *o),
        _ => String::new(),
    };
    let base = read_path_str(ctx, this);
    let resolved = std::path::Path::new(&base)
        .join(&other)
        .to_string_lossy()
        .to_string();
    let result = alloc_path(ctx, &resolved);
    Ok(Some(Value::Object(Some(result))))
}

pub(crate) fn native_path_to_absolute(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let s = read_path_str(ctx, this);
    let abs = nio_absolute_path_string(&s);
    let result = alloc_path(ctx, &abs);
    Ok(Some(Value::Object(Some(result))))
}

pub(crate) fn nio_absolute_path_string(path: &str) -> String {
    #[cfg(windows)]
    {
        return nio_windows_absolute_path_string(path);
    }
    #[cfg(not(windows))]
    {
        let p = std::path::Path::new(path);
        if p.is_absolute() {
            path.to_string()
        } else {
            std::env::current_dir()
                .unwrap_or_default()
                .join(p)
                .to_string_lossy()
                .into_owned()
        }
    }
}

#[cfg(windows)]
pub(crate) fn nio_windows_absolute_path_string(path: &str) -> String {
    let s = path.replace('\\', "/");
    let b = s.as_bytes();
    let is_sep = |c: u8| c == b'/' || c == b'\\';
    let has_drive = b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':';
    let drive_absolute = has_drive && b.len() >= 3 && is_sep(b[2]);
    let unc_absolute = b.len() >= 2 && is_sep(b[0]) && is_sep(b[1]);
    if drive_absolute || unc_absolute {
        return s;
    }

    let mut cwd = std::env::current_dir()
        .unwrap_or_default()
        .to_string_lossy()
        .replace('\\', "/");
    if let Some(stripped) = cwd.strip_prefix("//?/") {
        cwd = stripped.to_string();
    }
    while cwd.len() > 3 && cwd.ends_with('/') {
        cwd.pop();
    }

    let cwd_drive = cwd
        .as_bytes()
        .get(0..2)
        .filter(|d| d[0].is_ascii_alphabetic() && d[1] == b':')
        .and_then(|_| cwd.get(0..2))
        .unwrap_or("");

    if b.first().is_some_and(|c| is_sep(*c)) {
        let rest = s.trim_start_matches(|c| c == '/' || c == '\\');
        if cwd_drive.is_empty() {
            return format!("/{rest}");
        }
        return format!("{cwd_drive}/{rest}");
    }

    if has_drive {
        let drive = &s[..2];
        let rest = s[2..].trim_start_matches(|c| c == '/' || c == '\\');
        if cwd.get(0..2).is_some_and(|d| d.eq_ignore_ascii_case(drive)) {
            if rest.is_empty() {
                return cwd;
            }
            return format!("{cwd}/{rest}");
        }
        if rest.is_empty() {
            return format!("{drive}/");
        }
        return format!("{drive}/{rest}");
    }

    if s.is_empty() {
        cwd
    } else {
        format!("{cwd}/{s}")
    }
}

/// `java.nio.file.Path.toRealPath([Ljava/nio/file/LinkOption;)` —
/// returns the *real* path of an existing file, resolving symbolic links.
///
/// Unlike `toAbsolutePath`, the JDK contract requires the file to exist:
/// if it does not, `toRealPath` throws `java.nio.file.NoSuchFileException`
/// (a subclass of `IOException`). Callers such as Jetty's
/// `DirConfigSource.<init>` rely on that throw — they wrap the call in
/// `catch (NoSuchFileException)` to detect a missing `start.ini`. Returning
/// a null Path here (the pre-fix behaviour of the missing-native fallback)
/// slipped past the catch and produced a downstream NPE.
pub(crate) fn native_path_to_real_path(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NullPointerException {
                    message: Some("Path.toRealPath: null receiver".to_string()),
                },
            )));
        }
    };
    let raw = read_path_str(ctx, this);
    // The stored path string may carry forward slashes and a stray
    // `/?/` fragment (an artifact of the synthetic Path representation).
    // Normalize to native separators and drop a leading `?/` / `\?\`
    // verbatim-prefix remnant before touching the filesystem, so
    // `canonicalize` sees a well-formed path rather than failing with
    // a Windows "invalid name" error (os error 123) for a path that is
    // simply absent.
    let s = normalize_real_path_input(&raw);
    // JDK `toRealPath` requires the file to exist; resolve existence
    // explicitly so a missing file yields `NoSuchFileException` (the
    // exception Jetty's `DirConfigSource.<init>` catches) regardless of
    // platform-specific `canonicalize` error mapping.
    if !std::path::Path::new(&s).exists() {
        return Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::NoSuchFileException { path: s },
        )));
    }
    // `std::fs::canonicalize` resolves symlinks for an existing path.
    match std::fs::canonicalize(&s) {
        Ok(real) => {
            // Strip the Windows `\\?\` verbatim prefix that `canonicalize`
            // adds, so the returned path string matches what the rest of
            // the launcher (and `toString`) expects.
            let real_str = real.to_string_lossy();
            let cleaned = real_str
                .strip_prefix(r"\\?\")
                .unwrap_or(&real_str)
                .to_string();
            let result = alloc_path(ctx, &cleaned);
            Ok(Some(Value::Object(Some(result))))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Err(MethodCallFailed::InternalError(
            VmError::Runtime(RuntimeError::NoSuchFileException { path: s }),
        )),
        Err(e) => Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::IOException {
                message: format!("{s}: {e}"),
            },
        ))),
    }
}

/// Normalize a synthetic-Path string into a well-formed native path:
/// drop a leading verbatim-prefix remnant (`/?/`, `\?\`, `\\?\`), strip a
/// spurious leading slash before a Windows drive letter (`/C:/foo` →
/// `C:/foo`), and translate separators to the platform's native separator.
///
/// The leading-slash strip is essential for Jetty's `start.jar`: its
/// `processCommandLine` round-trips `$JETTY_HOME` / `$JETTY_BASE` through
/// `Path.toUri().toString()` (`BaseHome` `normalizeURI`), so synthetic
/// Path strings frequently arrive in the URI form `/C:/jetty-home/...`.
/// Without this strip, `Path::new("\C:\jetty-home\start.ini").exists()`
/// reports `false` on Windows, `toRealPath` wrongly throws
/// `NoSuchFileException` for a config/module file that actually exists,
/// Jetty's module graph ends up empty, and `Main.start` finally NPEs on a
/// null `Classpath` (`Cannot invoke getClasspath on null`). Mirrors
/// `phases_late.rs::p57_to_os_path`.
pub(crate) fn normalize_real_path_input(raw: &str) -> String {
    let mut s = raw;
    for prefix in [r"\\?\", "/?/", r"\?\"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest;
            break;
        }
    }
    // `/C:/foo` → `C:/foo` (URI-style absolute path on Windows). Only when
    // the third byte is `:` so genuine Unix-rooted paths are untouched.
    if cfg!(windows) && s.len() >= 3 && s.starts_with('/') && s.as_bytes().get(2) == Some(&b':') {
        s = &s[1..];
    }
    if cfg!(windows) {
        s.replace('/', "\\")
    } else {
        s.replace('\\', "/")
    }
}

pub(crate) fn native_path_normalize(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let s = read_path_str(ctx, this);
    // Simple normalize: remove . and .. components
    let p = std::path::Path::new(&s);
    let mut components = Vec::new();
    for comp in p.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                components.pop();
            }
            other => components.push(other),
        }
    }
    let normalized: std::path::PathBuf = components.iter().collect();
    let result = alloc_path(ctx, &normalized.to_string_lossy());
    Ok(Some(Value::Object(Some(result))))
}

pub(crate) fn native_path_get_name_count(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let s = read_path_str(_ctx, this);
    let count = parse_windows_path_root(&s).1.len();
    Ok(Some(Value::Int(count as i32)))
}

pub(crate) fn native_path_get_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let idx = match args.get(1) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let s = read_path_str(ctx, this);
    let names = parse_windows_path_root(&s).1;
    if idx < names.len() {
        let result = alloc_path(ctx, &names[idx]);
        Ok(Some(Value::Object(Some(result))))
    } else {
        Ok(Some(Value::Object(None)))
    }
}

pub(crate) fn native_path_starts_with(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let prefix = match args.get(1) {
        Some(Value::Object(Some(o))) => _ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    };
    let s = read_path_str(_ctx, this);
    Ok(Some(Value::Int(if s.starts_with(&prefix) { 1 } else { 0 })))
}

pub(crate) fn native_path_ends_with(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let suffix = match args.get(1) {
        Some(Value::Object(Some(o))) => _ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    };
    let s = read_path_str(_ctx, this);
    Ok(Some(Value::Int(if s.ends_with(&suffix) { 1 } else { 0 })))
}

pub(crate) fn native_path_to_file(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let s = read_path_str(ctx, this);
    // WIDTH: `1` was enough while slot 0 was the only slot anyone wrote. A real
    // image `java.io.File` carries `path`, `prefixLength` and `pathStatus`, and
    // `file_write_real_fields` skips any index past the object own width -- so a
    // one-slot File would take the path and silently keep `prefixLength = 0`,
    // which is the defect that helper exists to close.
    let file = match ctx.ensure_class_initialized("java/io/File") {
        Ok(cid) => {
            let n = ctx.class_num_total_fields(cid).max(1);
            ctx.alloc_object(cid, n)
        }
        Err(_) => ctx.alloc_object(cratonvm_types::ClassId::new(0), 1),
    };
    // GC: nothing in Java refers to this object yet, so this Rust local is
    // its ONLY reference — and the allocation below can collect. Under the
    // moving collector that leaves the local stale; under the Generational
    // collector's NON-MOVING young sweep the object is unreachable and gets
    // ZEROED in place. Pin across the allocation and read the live address
    // back (`unpinned-native-locals` family; `native-io` was outside the
    // 2026-08-25 audit's scope, which covered `native-builtins/src`).
    let __pin = ctx.pin_native_root(file);
    let path_str = ctx.create_string(&s);
    let file = ctx.read_native_pin(__pin, file);
    ctx.set_field(file, 0, Value::Object(Some(path_str)));
    file_write_real_fields(ctx, file, path_str, &s);
    ctx.unpin_native_roots(__pin);
    Ok(Some(Value::Object(Some(file))))
}

pub(crate) fn native_path_equals(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let a = read_path_str(ctx, this);
    let b = read_path_str(ctx, other);
    Ok(Some(Value::Int(if a == b { 1 } else { 0 })))
}

pub(crate) fn native_path_hash_code(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let s = read_path_str(ctx, this);
    let mut h: i32 = 0;
    for b in s.bytes() {
        h = h.wrapping_mul(31).wrapping_add(b as i32);
    }
    Ok(Some(Value::Int(h)))
}

pub(crate) fn native_path_compare_to(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let a = read_path_str(ctx, this);
    let b = read_path_str(ctx, other);
    Ok(Some(Value::Int(a.cmp(&b) as i32)))
}

pub(crate) fn native_file_to_path(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // RA.6: Real JDK `java.io.File` declares `path` along with
    // `pathStatus` and `prefixLength`, so slot 0 is NOT the path on
    // a real-JDK-loaded File — it's `pathStatus`. Resolve by name.
    // Fall back to slot 0 for synthetic-mode File.
    let s = match ctx.get_field_by_name(this, "path") {
        Value::Object(Some(o)) => ctx.read_string(o).unwrap_or_default(),
        _ => match ctx.get_field(this, 0) {
            Value::Object(Some(o)) => ctx.read_string(o).unwrap_or_default(),
            _ => String::new(),
        },
    };
    if s.is_empty() {
        return Err(MethodCallFailed::InternalError(VmError::Internal {
            message: "File.toPath(): this.path is null".to_string(),
        }));
    }
    alloc_path_checked(ctx, &s)
}

// --- Files static methods ---
pub(crate) fn files_path_str(ctx: &dyn NativeContext, args: &[Value]) -> String {
    match args.first() {
        Some(Value::Object(Some(o))) => read_path_str(ctx, *o),
        _ => String::new(),
    }
}

pub(crate) fn native_files_exists(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let s = files_path_str(_ctx, args);
    Ok(Some(Value::Int(if std::path::Path::new(&s).exists() {
        1
    } else {
        0
    })))
}

pub(crate) fn native_files_is_directory(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let s = files_path_str(_ctx, args);
    Ok(Some(Value::Int(if std::path::Path::new(&s).is_dir() {
        1
    } else {
        0
    })))
}

pub(crate) fn native_files_is_regular_file(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let s = files_path_str(_ctx, args);
    Ok(Some(Value::Int(if std::path::Path::new(&s).is_file() {
        1
    } else {
        0
    })))
}

pub(crate) fn native_files_size(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let s = files_path_str(_ctx, args);
    let size = std::fs::metadata(&s).map(|m| m.len()).unwrap_or(0);
    Ok(Some(Value::Long(size as i64)))
}

pub(crate) fn native_files_delete(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let s = validated_path(&files_path_str(ctx, args))?;
    let p = std::path::Path::new(&s);
    let result = if p.is_dir() {
        std::fs::remove_dir(&s)
    } else {
        std::fs::remove_file(&s)
    };
    if let Err(e) = result {
        return Err(io_err(e));
    }
    Ok(None)
}

pub(crate) fn native_files_delete_if_exists(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let s = validated_path(&files_path_str(ctx, args))?;
    let p = std::path::Path::new(&s);
    if !p.exists() {
        return Ok(Some(Value::Int(0)));
    }
    let result = if p.is_dir() {
        std::fs::remove_dir(&s)
    } else {
        std::fs::remove_file(&s)
    };
    Ok(Some(Value::Int(if result.is_ok() { 1 } else { 0 })))
}

pub(crate) fn native_files_create_file(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // A creation attribute (`PosixFilePermissions.asFileAttribute(..)`) is the
    // JDK body's business: it turns it into the `open(2)` mode that
    // `native_unix_open0` applies. `File::create_new` below has no mode
    // parameter, so `rw-r-----` was created `rw-r--r--`.
    if let Some(Value::Object(Some(attrs))) = args.get(1) {
        if ctx.array_length(*attrs) > 0 && !ctx.is_class_synthetic_stub("java/nio/file/Files") {
            return ctx.invoke_special_bytecode_only(
                "java/nio/file/Files",
                "createFile",
                "(Ljava/nio/file/Path;[Ljava/nio/file/attribute/FileAttribute;)Ljava/nio/file/Path;",
                args,
            );
        }
    }
    let s = validated_path(&files_path_str(ctx, args))?;
    // `Files.createFile` is specified as `CREATE_NEW`: it must FAIL with
    // `FileAlreadyExistsException` when the path already exists. It is the
    // atomic create-if-absent primitive of `java.nio.file`, so callers use it
    // AS a lock rather than merely to make a file. `std::fs::File::create` is
    // `O_CREAT|O_WRONLY|O_TRUNC`, which did the opposite twice over: it
    // reported success on an existing path AND truncated whatever was in it.
    //
    // H2 `FilePathDisk.createFile` catches `FileAlreadyExistsException` to
    // return `false` (meaning: another process already holds this lock file),
    // so a silent success let two `FileLock` instances both believe they had
    // taken the database lock. `TestFileLock.testSimple` then saw
    // ERROR_OPENING_DATABASE_1 ("Concurrent update") from the second locker
    // where it asserts DATABASE_ALREADY_OPEN_1 — and, worse, the truncation
    // had already destroyed the first lock file holder id on disk.
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&s)
    {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            return Err(nio_native::file_already_exists(ctx, &s));
        }
        Err(e) => return Err(io_err_nio(e, &s)),
    }
    let path = match args.first() {
        Some(v) => *v,
        _ => Value::Object(None),
    };
    Ok(Some(path))
}

pub(crate) fn native_files_create_directory(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let s = validated_path(&files_path_str(ctx, args))?;
    if let Err(e) = std::fs::create_dir(&s) {
        return Err(io_err(e));
    }
    let path = match args.first() {
        Some(v) => *v,
        _ => Value::Object(None),
    };
    Ok(Some(path))
}

