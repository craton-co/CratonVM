#[allow(unused_imports)]
use super::super::*;

/// `PrintWriter.flush()` — push buffered output at both layers.
///
/// A no-op here loses output outright: a `PrintWriter` over a `StringWriter`
/// or a file never hands its bytes on, and callers that flush precisely
/// because they are about to read the sink (test captures, a log file checked
/// by the harness, `printStackTrace` before an abort) see nothing. Two sinks
/// have to be pushed because writes can take either route — the Java-side
/// sink object for `write`/`printf`-through-`out`, and the VM fd for the
/// print natives that address stdout/stderr through `fd_table` directly.
pub fn native_printwriter_flush(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(None);
    };
    // KEPT SWALLOW, NARROWED. `java.io.PrintWriter.flush()` is
    //
    //     synchronized (lock) {
    //         try { ensureOpen(); out.flush(); }
    //         catch (IOException x) { trouble = true; }
    //     }
    //
    // — it really does absorb the delegated failure, and `PrintWriter` does
    // not declare `throws IOException` at all, so making this propagate would
    // be a fresh divergence, not a fix. But that `catch` names `IOException`
    // and nothing wider: a `NoSuchMethodError` out of this dispatch means our
    // own method lookup failed, and absorbing it here converts a broken VM
    // into a quietly wrong one. `absorb_io_exception` keeps the JDK's half and
    // gives back the other. W7-57-close-flush-swallow-sweep.md
    //
    // RECORDED since W7-64: the `catch` body is `trouble = true` and
    // `checkError()` is its only reader, so an absorbed `IOException` that is
    // not written down is *unobservable* rather than merely unthrown.
    // W7-64-printstream-trouble-and-errormanager.md
    let sink = printwriter_sink(&*ctx, this);
    let console_fd = stream_fd(ctx, args);
    if let Some(sink) = sink {
        let flushed = ctx.invoke_virtual(sink, "flush", "()V", &[]);
        cratonvm_native_api::print_error_state::absorb_io_exception_recording(
            &*ctx, this, flushed,
        )?;
    }
    if let Some(fd) = console_fd {
        if ctx.fd_table().flush(fd).is_err() {
            cratonvm_native_api::print_error_state::set_trouble(&*ctx, this);
        }
    }
    Ok(None)
}

/// Does the wrapper chain hanging off `start` bottom out at `System.out` /
/// `System.err`? Used by [`native_printwriter_close`] to refuse to close the
/// process console. Walks the three link fields a print sink can use — the
/// real-JDK `out`, `OutputStreamWriter`'s `se`, and our synthetic
/// `field 0 = stream` slot — with a small bound, since these chains are at
/// most `PrintWriter → BufferedWriter → OutputStreamWriter → StreamEncoder →
/// PrintStream` deep.
pub fn sink_reaches_system_stream(ctx: &dyn NativeContext, start: ObjectRef) -> bool {
    let out = ctx.get_system_stream("out");
    let err = ctx.get_system_stream("err");
    let mut cur = start;
    for _ in 0..6 {
        for known in [&out, &err].into_iter().flatten() {
            if std::ptr::eq(cur.as_ptr(), known.as_ptr()) {
                return true;
            }
        }
        let next = match ctx.get_field_by_name(cur, "out") {
            Value::Object(Some(n)) => Some(n),
            _ => match ctx.get_field_by_name(cur, "se") {
                Value::Object(Some(n)) => Some(n),
                _ => match ctx.get_field(cur, 0) {
                    Value::Object(Some(n)) => Some(n),
                    _ => None,
                },
            },
        };
        match next {
            Some(n) => cur = n,
            None => break,
        }
    }
    false
}

/// `PrintWriter.close()` — flush, then close the underlying sink.
///
/// Two deliberate departures from the JDK:
///   * stdout/stderr are never actually closed (same rule as
///     `native_printstream_close`) — a library closing its `PrintWriter` must
///     not take the process's console with it.
///   * `out` is NOT nulled. The JDK uses `out == null` as its closed marker,
///     but our `print*`/`write` natives read a null `out` as "this is a
///     console stream" and fall back to the fd table — so nulling it would
///     redirect a closed writer's output to stdout instead of dropping it.
///     Closing the sink is what makes further writes fail, which is the
///     observable part.
///
/// Since W7-70-printstream-close-noop.md `native_printstream_close` closes too,
/// so the cross-reference above is no longer to a no-op. The two bodies still
/// differ in three ways, all deliberate:
///   * it detects the console as `out == null` (a `PrintStream`'s own
///     invariant) where this one needs `stream_fd` + `sink_reaches_system_stream`,
///     because a `PrintWriter`'s synthetic slot-0 sink can BE `System.out`;
///   * it latches `java.io.PrintStream`'s real `closing` field, which
///     `java.io.PrintWriter` does not declare, so a second `close()` here still
///     reaches the sink;
///   * its flush is HotSpot's (`textOut.close()` bottoms out in
///     `StreamEncoder.implClose`'s `out.flush()`, measured), where the flush
///     here is ours.
pub fn native_printwriter_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(None);
    };
    let sink = printwriter_sink(&*ctx, this);
    let console_fd = stream_fd(ctx, args);
    let is_console =
        console_fd.is_some() || sink.is_some_and(|sink| sink_reaches_system_stream(&*ctx, sink));
    // KEPT SWALLOW, NARROWED — same reasoning as `native_printwriter_flush`.
    // `java.io.PrintWriter.close()` is `try { if (out != null) { out.close();
    // out = null; } } catch (IOException x) { trouble = true; }`: the JDK
    // absorbs an `IOException` and `close()` declares no checked exception, so
    // propagating everything would be a fresh divergence. An `Error` is not an
    // `IOException` and must come out. W7-57-close-flush-swallow-sweep.md
    //
    // The preceding `flush()` has no counterpart in HotSpot's `close()` (it
    // closes without flushing, relying on the sink's own `close()` to do it);
    // it is ours, so it takes the same `IOException`-absorbing policy rather
    // than a stricter one — see the `vm_only_best_effort` reasoning in
    // `native-api/src/delegated_close.rs`.
    //
    // RECORDED since W7-64. HotSpot's `catch (IOException x)` body is
    // `trouble = true`, and `checkError()` is its only reader. Measured on
    // HotSpot 25.0.3.9: a `Writer` whose `close()` raises an `IOException`
    // leaves `PrintWriter.close()` silent and `checkError()` TRUE; the same
    // `close()` raising an `Error` propagates and leaves `checkError()` FALSE.
    // Both halves are asserted in `probes/CloseFlushSwallowProbe.java`.
    // W7-64-printstream-trouble-and-errormanager.md
    //
    // gc-common w20-c: `flush` and `close` are Java, and the writer and its
    // sink were used raw after them; both are read back from pins (the
    // dispatcher truncates the pin stack on the `?` returns).
    let this_pin = ctx.pin_native_root(this);
    if let Some(sink) = sink {
        let sink_pin = ctx.pin_native_root(sink);
        let flushed = ctx.invoke_virtual(sink, "flush", "()V", &[]);
        let this_now = ctx.read_native_pin(this_pin, this);
        cratonvm_native_api::print_error_state::absorb_io_exception_recording(
            &*ctx, this_now, flushed,
        )?;
        if !is_console {
            let sink_now = ctx.read_native_pin(sink_pin, sink);
            let closed = ctx.invoke_virtual(sink_now, "close", "()V", &[]);
            let this_now = ctx.read_native_pin(this_pin, this);
            cratonvm_native_api::print_error_state::absorb_io_exception_recording(
                &*ctx, this_now, closed,
            )?;
        }
    }
    let this: ObjectRef = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    if let Some(fd) = console_fd {
        if ctx.fd_table().flush(fd).is_err() {
            cratonvm_native_api::print_error_state::set_trouble(&*ctx, this);
        }
    }
    // `out = null`, the second half of the JDK's `if (out != null) { out.close();
    // out = null; }`. It is the state every later `ensureOpen()` reads, and
    // omitting it left "this writer is closed" with no representation at all on
    // the `PrintWriter` side, which has no `closing` field to fall back on.
    //
    // NOT for a console writer: that branch deliberately does not close the
    // sink, so it is not closed, and nulling would strand `System.out`.
    if !is_console && sink.is_some() {
        ctx.set_field_by_name(this, "out", Value::Object(None));
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// The READ side of `trouble` — `checkError` / `setError` / `clearError`
// ---------------------------------------------------------------------------
//
// SYNTHETIC-JDK ONLY. In Compatible mode all three of these run the real
// `java.io` bytecode over the real `trouble` field, and shadowing correct real
// bytecode with a native is the thing this workspace calls a contract-1.4
// shadow. They are registered from `register_synthetic_overrides` alone.
//
// The RED they close is not "the flag reads wrong": measured on this branch,
// synthetic-mode `PrintStream.checkError()` resolves to no native and to no
// method on the fabricated class, so it raises
// `NoSuchMethodError: java/io/PrintStream.checkError()Z`. `setError()` and
// `clearError()` — the JDK's `protected` write side, which a `PrintStream`
// subclass uses to report a failure of its own — do the same.
// W7-64-printstream-trouble-and-errormanager.md

/// Is `obj` an instance of `class_name`? The question the `instanceof` opcode
/// asks — by `ClassId` hierarchy, never by class name comparison.
pub fn print_sink_is_a(ctx: &dyn NativeContext, obj: ObjectRef, class_name: &str) -> bool {
    ctx.class_id_by_name(class_name)
        .is_some_and(|root| ctx.is_subclass(ctx.class_id_of_object(obj), root))
}

/// `checkError()` for both classes.
///
/// ```text
/// PrintStream:  if (out != null) flush();
///               if (out instanceof PrintStream ps) return ps.checkError();
///               return trouble;
/// PrintWriter:  if (out != null) flush();
///               if (out instanceof PrintWriter pw) return pw.checkError();
///               else if (psOut != null)            return psOut.checkError();
///               return trouble;
/// ```
///
/// The flush is NOT incidental and is the half a "just read the flag"
/// implementation loses: measured on HotSpot 25.0.3.9, a stream that has never
/// failed but whose sink's `flush()` throws answers `checkError() == true` on
/// the first call, and the sink records the flush attempt. Both halves are
/// asserted in `probes/CloseFlushSwallowProbe.java`.
///
/// `psOut` has no counterpart in our layout, because our
/// `PrintWriter(OutputStream)` puts the `OutputStream` straight into `out`
/// rather than the JDK's `BufferedWriter(OutputStreamWriter(out))` — so the
/// `out instanceof PrintStream` test below IS the `psOut` branch here.
pub fn printstream_check_error(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Int(0)));
    };
    let out = match ctx.get_field_by_name(this, "out") {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    };
    // `if (out != null) flush();` — and a CLOSED `PrintStream` has `out ==
    // null` in HotSpot, so it skips the flush. `native_printstream_close`
    // cannot null `out` (a null `out` is this VM's "console stream" marker —
    // see its doc comment, and `native_printwriter_close`'s); it latches
    // `closing` instead, and this reads that latch as the same fact. Without
    // the gate, a stream closed CLEANLY over a sink whose `flush()`-after-
    // `close()` throws would flush, absorb, record and answer `true` where
    // HotSpot answers `false` — a `checkError()` reporting the close it was
    // asked about. `java.io.PrintWriter` declares no `closing` field, so
    // `is_closing` is `false` on that side of this shared body and its
    // behaviour is unchanged (its own residual is named in
    // W7-64-printstream-trouble-and-errormanager.md).
    // W7-70-printstream-close-noop.md
    let already_closed = cratonvm_native_api::print_error_state::is_closing(&*ctx, this);
    // `flush()` is virtual in the JDK too, so a subclass override runs. Our
    // own `flush` natives absorb an `IOException` into `trouble`; an `Error`
    // out of them is not something any JDK `catch` on this path names, so it
    // propagates — the same rule the flush site itself follows.
    if !already_closed {
        ctx.invoke_virtual(this, "flush", "()V", &[])?;
    }
    if let Some(out) = out {
        for delegate_to in ["java/io/PrintWriter", "java/io/PrintStream"] {
            if print_sink_is_a(&*ctx, out, delegate_to) {
                return ctx.invoke_virtual(out, "checkError", "()Z", &[]);
            }
        }
    }
    let trouble = cratonvm_native_api::print_error_state::is_trouble(&*ctx, this);
    Ok(Some(Value::Int(i32::from(trouble))))
}

/// `protected void setError()` — "subsequent invocations of `checkError()`
/// return `true` until `clearError()` is invoked".
pub fn printstream_set_error(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(Value::Object(Some(this))) = args.first().copied() {
        cratonvm_native_api::print_error_state::set_trouble(&*ctx, this);
    }
    Ok(None)
}

/// `protected void clearError()`.
pub fn printstream_clear_error(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(Value::Object(Some(this))) = args.first().copied() {
        cratonvm_native_api::print_error_state::clear_trouble(&*ctx, this);
    }
    Ok(None)
}

pub fn printwriter_get_backing_writer(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Option<ObjectRef> {
    let by_name = ctx.get_field_by_name(this, "out");
    let candidate = if matches!(by_name, Value::Object(Some(_))) {
        by_name
    } else {
        ctx.get_field(this, 0)
    };
    if let Value::Object(Some(out_obj)) = candidate {
        if matches!(sink_is_writer(ctx, out_obj), Some(true)) {
            // Exclude BufferedWriter: it is the JDK wrapper placed by
            // PrintWriter(OutputStream) → those connect to stdout/stderr and
            // must keep using the fd-table path.
            let cid = ctx.class_id_of_object(out_obj);
            let cn = ctx.class_name_of_id(cid).unwrap_or_default();
            if cn != "java/io/BufferedWriter" {
                return Some(out_obj);            }        }    }
    None
}

/// Format a double like Java does (no trailing zeros for integers, etc.)
// ---------------------------------------------------------------------------
// Step 2: StringBuilder / StringBuffer natives
// ---------------------------------------------------------------------------

/// Helper: read field 0 (char[] buffer) and field 1 (int count) from StringBuilder.
/// Helper: ensure the StringBuilder has capacity for `additional` more chars.
/// Returns the char[] buffer (possibly newly allocated and copied).
/// Helper: append a slice of u16 chars to a StringBuilder.
/// Helper: append a Rust string to a StringBuilder.
/// Call `toString()` on an arbitrary object via virtual dispatch.
///
/// If the object is a Java String, reads it directly. Otherwise invokes
/// `toString()` which will find overridden versions in user-defined classes.
// ---------------------------------------------------------------------------
// StringBuilder mutation methods
// ---------------------------------------------------------------------------

/// Helper: read the current content of a StringBuilder as a Vec<u16>.
/// Helper: write a Vec<u16> back into a StringBuilder, replacing all content.
/// insert(int, String) — insert string at offset
/// insert(int, char) — insert single char
/// insert(int, int) — insert int as string
/// insert(int, Object) — insert Object via toString
/// delete(int, int) — remove range [start, end)
/// deleteCharAt(int) — remove single char
/// replace(int, int, String) — replace range with string
/// setCharAt(int, char) — set char at index
/// setLength(int) — truncate or extend with null chars
/// indexOf(String) — find substring, return -1 if not found
/// indexOf(String, int) — find substring from offset
/// substring(int) — substring from index to end
/// substring(int, int) — substring [start, end)
/// capacity() — return backing array length
/// ensureCapacity(int) — grow if needed
/// Format a float like Java does.
// ---------------------------------------------------------------------------
// Step 3: Additional String natives
// ---------------------------------------------------------------------------

/// Helper: read a String's char[] into a Vec<u16>.
// ---------------------------------------------------------------------------
// Phase 8 Part 7: Additional String methods
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Step 4: System properties + utilities
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Phase 13 Step 4: Runtime + System.lineSeparator
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Phase 14 Step 2: java.util.UUID
// ---------------------------------------------------------------------------

pub fn register_uuid_natives(registry: &mut NativeMethodRegistry) {
    // census-tag: UUID fromString/toString/bit-accessors are spec-exact
    // fast-paths for a class with real JDK bytecode → Intrinsic (randomUUID
    // draws host entropy but the format/equality contract is exact).
    let c = "java/util/UUID";
    registry.register(c, "<init>", "(JJ)V", native_uuid_init, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register(c, "randomUUID", "()Ljava/util/UUID;", native_uuid_random, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register(
        c,
        "fromString",
        "(Ljava/lang/String;)Ljava/util/UUID;",
        native_uuid_from_string, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(c, "toString", "()Ljava/lang/String;", native_uuid_to_string, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register(c, "getMostSignificantBits", "()J", native_uuid_get_msb, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register(c, "getLeastSignificantBits", "()J", native_uuid_get_lsb, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register(c, "equals", "(Ljava/lang/Object;)Z", native_uuid_equals, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register(c, "hashCode", "()I", native_uuid_hash_code, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register(c, "version", "()I", native_uuid_version, cratonvm_native_api::NativeKind::Intrinsic);
}

pub const UUID_FIELD_MSB: usize = 0;
pub const UUID_FIELD_LSB: usize = 1;

pub fn uuid_set_msb(ctx: &mut dyn NativeContext, obj: cratonvm_types::ObjectRef, v: i64) {
    ctx.set_field_by_name(obj, "mostSigBits", Value::Long(v));
    // Best-effort fallback for synthetic-jdk layout where the natives use indices 0/1.
    ctx.set_field(obj, UUID_FIELD_MSB, Value::Long(v));
}

pub fn uuid_set_lsb(ctx: &mut dyn NativeContext, obj: cratonvm_types::ObjectRef, v: i64) {
    ctx.set_field_by_name(obj, "leastSigBits", Value::Long(v));
    ctx.set_field(obj, UUID_FIELD_LSB, Value::Long(v));
}

pub fn uuid_get_msb(ctx: &mut dyn NativeContext, obj: cratonvm_types::ObjectRef) -> i64 {
    if let Value::Long(v) = ctx.get_field_by_name(obj, "mostSigBits") {
        if v != 0 {
            return v;
        }
    }
    match ctx.get_field(obj, UUID_FIELD_MSB) {
        Value::Long(v) => v,
        _ => 0,
    }
}

pub fn uuid_get_lsb(ctx: &mut dyn NativeContext, obj: cratonvm_types::ObjectRef) -> i64 {
    if let Value::Long(v) = ctx.get_field_by_name(obj, "leastSigBits") {
        if v != 0 {
            return v;
        }
    }
    match ctx.get_field(obj, UUID_FIELD_LSB) {
        Value::Long(v) => v,
        _ => 0,
    }
}

pub fn alloc_uuid(
    ctx: &mut dyn NativeContext,
    msb: i64,
    lsb: i64,
) -> Result<cratonvm_types::ObjectRef, MethodCallFailed> {
    let class_id = match ctx.ensure_class_initialized("java/util/UUID") {
        Ok(id) => id,
        // Fallible since 2026-08-10 (Real-JDK wave 2, step 3): a 2-slot
        // synthetic `java.util.UUID` is a compatibility stand-in for a class
        // `java.base` declares, so a strict run refuses it and the caller sees
        // a `NoClassDefFoundError` naming it. On a complete image the `Ok` arm
        // is what runs.
        Err(_) => crate::util_concurrent_ext::refused_class(ctx, "java/util/UUID", 2)?,
    };
    let obj = ctx.alloc_object(class_id, 2);
    uuid_set_msb(ctx, obj, msb);
    uuid_set_lsb(ctx, obj, lsb);
    Ok(obj)
}

pub fn native_uuid_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let msb = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let lsb = match args.get(2) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    uuid_set_msb(ctx, this, msb);
    uuid_set_lsb(ctx, this, lsb);
    Ok(None)
}

pub fn native_uuid_random(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // Generate pseudo-random UUID v4
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(42);
    // Simple LCG-based random generation
    let mut state = nanos;
    let mut next = || -> u64 {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        state
    };
    let mut msb = next() as i64;
    let mut lsb = next() as i64;
    // Set version to 4 (bits 12-15 of msb)
    msb = (msb & !0xF000i64) | 0x4000;
    // Set variant to 2 (bits 62-63 of lsb)
    lsb = (lsb & !(3i64 << 62)) | (2i64 << 62);
    let uuid = alloc_uuid(ctx, msb, lsb);
    Ok(Some(Value::Object(Some(uuid?))))
}

pub fn native_uuid_from_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // `UUID.fromString(null)` is a NullPointerException on HotSpot, not a null
    // return — a native whose descriptor returns java/util/UUID must not answer
    // the void-shaped `Ok(None)`.
    let s = match args.first() {
        Some(Value::Object(Some(r))) => ctx.read_string(*r).unwrap_or_default(),
        _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
    };
    // The previous body stripped EVERY dash and checked only that 32 hex digits
    // remained. That is over-strict and over-lax at once — the same missing
    // spec seen from two sides:
    //   * it REJECTED "1-2-3-4-5", which HotSpot accepts and zero-pads,
    //   * it ACCEPTED a dash-less 32-char string and a 7-dash string,
    //   * `unwrap_or(0)` turned "zzzz…" into the nil UUID instead of throwing,
    //   * `&hex[0..16]` byte-slices a String and panics on a multi-byte char.
    // The real algorithm is five `Long.parseLong` calls with masking and no
    // dash-position count. `uuid_from_string_bits` transcribes it.
    // docs/feature-designs/native-dispatch-architecture.md
    let (msb, lsb) = crate::phases_late::uuid_from_string_bits(&s)?;
    let uuid = alloc_uuid(ctx, msb, lsb);
    Ok(Some(Value::Object(Some(uuid?))))
}

pub fn native_uuid_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let msb = uuid_get_msb(ctx, this) as u64;
    let lsb = uuid_get_lsb(ctx, this) as u64;
    let s = format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        (msb >> 32) & 0xFFFFFFFF,
        (msb >> 16) & 0xFFFF,
        msb & 0xFFFF,
        (lsb >> 48) & 0xFFFF,
        lsb & 0xFFFF_FFFF_FFFF
    );
    // UUID text is dynamically produced.  Interning every distinct value keeps
    // the entire monotonicity-test output alive and turns normal allocation
    // pressure into repeated intern-table stalls.  The receiver is pinned by
    // safe_native_call and its fields have already been read above, so this
    // native can safely request collection before the transient allocation.
    let str_obj = ctx.create_string_uninterned_gc_safe(&s);
    Ok(Some(Value::Object(Some(str_obj))))
}

pub fn native_uuid_get_msb(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Long(0))),
    };
    Ok(Some(Value::Long(uuid_get_msb(ctx, this))))
}

pub fn native_uuid_get_lsb(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Long(0))),
    };
    Ok(Some(Value::Long(uuid_get_lsb(ctx, this))))
}

pub fn native_uuid_equals(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let eq = uuid_get_msb(ctx, this) == uuid_get_msb(ctx, other)
        && uuid_get_lsb(ctx, this) == uuid_get_lsb(ctx, other);
    Ok(Some(Value::Int(if eq { 1 } else { 0 })))
}

pub fn native_uuid_hash_code(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let msb = uuid_get_msb(ctx, this);
    let lsb = uuid_get_lsb(ctx, this);
    let hilo = msb ^ lsb;
    let hash = ((hilo >> 32) ^ hilo) as i32;
    Ok(Some(Value::Int(hash)))
}

pub fn native_uuid_version(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let msb = uuid_get_msb(ctx, this);
    let version = ((msb >> 12) & 0xF) as i32;
    Ok(Some(Value::Int(version)))
}

// =====================================================================
// CRIT perf — sharded side-store globals for the Unsafe / Class$Atomic
// hot path.
//
// Three of the original globals (`static_long_store`, `static_int_store`,
// `static_obj_store`) plus the offset-interning map were
// `std::sync::Mutex<HashMap<...>>` (SipHash + per-call
// `.lock().unwrap_or_else` handshake on a single process-wide mutex).
// Under heavy concurrent CAS / static-field traffic (ChmStress, AQS
// bursts, JDK lazy-init guards on Class mirrors) they collapsed onto one
// lock and one SipHash chain per operation.
//
// Fix: shard each map into `UNSAFE_SHARDS = 32` parking_lot mutexes
// indexed by FxHash of the key.  parking_lot::Mutex avoids the
// poisoning machinery (no Result unwrap on the lock) and FxHashMap
// removes the SipHash overhead — neither global is exposed to
// untrusted user-input keys (they are derived from JIT/class-loader
// state), so DoS-resistance is not required here.
//
// Round-8 Bug 4 follow-up: `synthetic_field_store` and
// `class_atomic_side_store` were missed in the original sweep and were
// still `std::sync::Mutex<std::collections::HashMap<...>>`. They have
// now been migrated to `parking_lot::Mutex<FxHashMap<...>>` for the same
// reasons (poisoning + SipHash overhead). They are NOT yet sharded —
// contention has been low in the workloads we measure (per-receiver
// keys spread naturally), but if profiling shows otherwise the pattern
// from `static_*_store` can be lifted in.
// =====================================================================

pub const UNSAFE_SHARDS: usize = 32;
pub const UNSAFE_SHARD_MASK: usize = UNSAFE_SHARDS - 1;

pub type UnsafeShardedMap<K, V> = [parking_lot::Mutex<rustc_hash::FxHashMap<K, V>>; UNSAFE_SHARDS];

/// Fallback store for Unsafe.{getAndAdd,CAS,get,put}Long on a null object
/// (static-field access via absolute offset). Used to service callers like
/// `Thread$ThreadIdentifiers.next()` which invoke `Unsafe.getAndAddLong(null,
/// NEXT_TID_OFFSET, 1)` against a static `long` field. Without this, the
/// unwrap on a null receiver would panic and abort the whole VM.
///
/// Keyed by `(vm_identity, offset)` since gc-common w8-a
/// (`common-w7a-unsafe-static-field-targets-and-primitive-side-stores-are-process-global`):
/// offsets are minted process-wide from `(class name, field name)`, so two VMs
/// that reached this fallback for the same field used to share one slot and
/// see each other's CASes -- including the `Thread$ThreadIdentifiers` counter,
/// which is why VM B's first application thread did not get tid 2. Lock the
/// shard with [`lock_static_long_shard`].
pub fn static_long_store() -> &'static UnsafeShardedMap<UnsafeVmOffsetKey, i64> {
    static T: std::sync::OnceLock<UnsafeShardedMap<UnsafeVmOffsetKey, i64>> =
        std::sync::OnceLock::new();
    T.get_or_init(new_unsafe_sharded_map)
}

/// The `int` twin of [`static_long_store`], keyed the same way.
pub fn static_int_store() -> &'static UnsafeShardedMap<UnsafeVmOffsetKey, i32> {
    static T: std::sync::OnceLock<UnsafeShardedMap<UnsafeVmOffsetKey, i32>> =
        std::sync::OnceLock::new();
    T.get_or_init(new_unsafe_sharded_map)
}

/// Lock the shard of the null-base `long` store that holds `offset` (every
/// VM's row for it). Index it with `(ctx.vm_identity(), offset)`.
#[inline]
pub fn lock_static_long_shard(
    offset: usize,
) -> parking_lot::MutexGuard<'static, rustc_hash::FxHashMap<UnsafeVmOffsetKey, i64>> {
    lock_unsafe_vm_offset_shard(static_long_store(), offset)
}

/// Lock the shard of the null-base `int` store that holds `offset`. Index it
/// with `(ctx.vm_identity(), offset)`.
#[inline]
pub fn lock_static_int_shard(
    offset: usize,
) -> parking_lot::MutexGuard<'static, rustc_hash::FxHashMap<UnsafeVmOffsetKey, i32>> {
    lock_unsafe_vm_offset_shard(static_int_store(), offset)
}

/// Key of the null-base object side store: `(vm_identity, offset)`.
///
/// gc-common w7-a (`common-w6a-unsafe-synthetic-field-store-is-shared-across-vms`):
/// the key used to be the bare offset, and offsets are minted process-wide
/// from `(class name, field name)` (`synthetic_offset_for`), so two VMs that
/// reached this fallback for the same field shared one slot -- one VM read, and
/// CAS'd against, an object in the other VM's heap, and the `unsafe-side-store`
/// root row handed each VM's collector the other VM's values. The shard is
/// still chosen by the offset alone ([`lock_static_obj_shard`]).
pub type StaticObjKey = (usize, usize);

pub type StaticObjMap =
    rustc_hash::FxHashMap<StaticObjKey, Option<cratonvm_types::ObjectRef>>;

pub type StaticObjStore = UnsafeShardedMap<StaticObjKey, Option<cratonvm_types::ObjectRef>>;

pub fn static_obj_store() -> &'static StaticObjStore {
    static T: std::sync::OnceLock<StaticObjStore> = std::sync::OnceLock::new();
    T.get_or_init(new_unsafe_sharded_map)
}

/// Lock the shard of the null-base object store that holds `offset` (every
/// VM's row for that offset). Index it with `(ctx.vm_identity(), offset)`.
#[inline]
pub fn lock_static_obj_shard(
    offset: usize,
) -> parking_lot::MutexGuard<'static, StaticObjMap> {
    static_obj_store()[unsafe_shard_of(&offset)].lock()
}

/// `Thread.PRIMORDIAL_TID` of JDK 25: the `Thread.threadId()` of the
/// primordial (`main`) thread. JDK 25's `Thread.<init>` gives an attached
/// thread created before `VM.initLevel() >= 1` this constant and every other
/// thread `ThreadIdentifiers.next()`, whose counter HotSpot starts at
/// `PRIMORDIAL_TID + 1` (ids 1 and 2 are reserved by `ObjectMonitor`'s owner
/// encoding). CratonVM builds `main`'s mirror itself (`vm_exec`'s
/// `populate_current_thread_object`), so it assigns this id there (wave 82,
/// `i81-L6-main-thread-id-is-2-pow-40-20261113.md`).
pub const PRIMORDIAL_JAVA_TID: i64 = 3;

/// The first id `ThreadIdentifiers.next()` hands out: `PRIMORDIAL_TID + 1`,
/// as HotSpot 25 seeds its `next_thread_id`.
pub const FIRST_MINTED_JAVA_TID: i64 = PRIMORDIAL_JAVA_TID + 1;

/// The synthetic offset of `Thread$ThreadIdentifiers`' tid counter, seeding
/// `vm`'s counter to [`FIRST_MINTED_JAVA_TID`] on first use. The offset is
/// the same in every VM; the counter behind it is `vm`'s own (see
/// [`static_long_store`]).
pub fn thread_next_tid_offset(vm: usize) -> usize {
    let offset = synthetic_offset_for("java/lang/Thread$ThreadIdentifiers", "native:nextTid");
    let mut map = lock_static_long_shard(offset);
    // Seeded past the primordial id: `main`'s id is never minted here. The
    // native constructors also clamp with `max(FIRST_MINTED_JAVA_TID)`
    // ([`next_java_thread_tid`]); the REAL constructor (`ThreadIdentifiers
    // .next()` -> `Unsafe.getAndAddLong(null, offset, 1)`, SBR round 2 lane
    // `thread`) reads this counter directly and would otherwise hand its first
    // thread the main thread's id.
    map.entry((vm, offset)).or_insert(FIRST_MINTED_JAVA_TID);
    offset
}

/// Mint the next Java `Thread.tid` of VM `vm`.
pub fn next_java_thread_tid(vm: usize) -> i64 {
    let offset = thread_next_tid_offset(vm);
    let mut map = lock_static_long_shard(offset);
    let entry = map.entry((vm, offset)).or_insert(FIRST_MINTED_JAVA_TID);
    // The VM bootstrap main Thread is already visible as
    // `PRIMORDIAL_JAVA_TID` but is not constructed through this native.
    // Never hand out an id at or below it.
    let tid = (*entry).max(FIRST_MINTED_JAVA_TID);
    *entry = tid.saturating_add(1);
    tid
}

/// Mint a `Thread.tid` for a thread mirror the VM builds itself (a
/// JNI-attached or VM-owned thread other than `main`), from the same
/// per-VM counter `ThreadIdentifiers.next()` reads -- as HotSpot's attached
/// `Thread.<init>` does once `VM.initLevel() >= 1`. One numbering authority,
/// so the id can never collide with a Java-constructed thread's (the
/// 2026-07-07 `ReentrantReadWriteLock` hold-counter collision).
pub fn mint_vm_thread_java_tid(vm: usize) -> i64 {
    next_java_thread_tid(vm)
}

/// Current thread's Java `Thread.tid` (what `Thread.currentThread().threadId()`
/// returns): read it off the current thread's mirror so native-built
/// `LogRecord`s carry the same id that thread-keyed consumers (JULI's
/// `OneLineFormatter` -> `ThreadMXBean.getThreadInfo(long)`) resolve against.
/// Falls back to the VM thread id when the mirror exposes no positive `tid`
/// (e.g. synthetic Thread layouts) -- `threadId()` must stay positive.
pub fn current_java_thread_tid(ctx: &mut dyn NativeContext) -> i64 {
    let thread_obj = ctx.current_thread_object();
    // Round 15 wave 4 (lane threadvm): `Thread`'s own `tid`
    // (`lang_misc::thread_field`), not a subclass's `long tid` field.
    match crate::lang_misc::thread_field(ctx, thread_obj, "tid") {
        Value::Long(id) if id > 0 => id,
        Value::Int(id) if id > 0 => id as i64,
        _ => ctx.thread_id().max(1) as i64,
    }
}

/// The JDK's legacy int thread id for a long tid: ids that fit keep their
/// value, larger (e.g. VM-fabricated high-range) tids collapse onto the
/// same "above Integer.MAX_VALUE/2" convention `LogRecord.shortThreadID`
/// uses. The exact high-range value is only ever a display label.
pub fn short_thread_id(tid: i64) -> i32 {
    if tid <= (i32::MAX / 2) as i64 {
        tid as i32
    } else {
        i32::MAX / 2 + 1
    }
}

/// Wall-clock now in epoch millis, for stamping `LogRecord` creation time.
pub fn epoch_millis_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Whether `rec` is a real-JDK-layout `java.util.logging.LogRecord`. The real
/// class resolves its private `longThreadID` long by NAME (typed zero even
/// before any write); synthetic WildFly/JBoss mirrors have no such field, so
/// the lookup yields `Object(None)`. Slot-based accessor fallbacks are only
/// valid on the synthetic layouts: the real JDK 25 declaration order
/// (level=0, sequenceNumber=1, sourceClassName=2, sourceMethodName=3,
/// message=4, threadID=5, longThreadID=6, instant=7, loggerName=8,
/// resourceBundle=9, resourceBundleName=10, parameters=11, thrown=12)
/// disagrees with the synthetic slots almost everywhere.
pub fn log_record_real_layout(
    ctx: &dyn NativeContext,
    rec: cratonvm_types::ObjectRef,
) -> bool {
    matches!(ctx.get_field_by_name(rec, "longThreadID"), Value::Long(_))
}

// =====================================================================
// CAS-livelock fix — synthetic-offset side store for Unsafe.{CAS,get,put}.
//
// Background: `Unsafe.objectFieldOffset(C.class, "name")` is called by JDK
// internals such as `java.lang.Class$Atomic.{cas,get}ReflectionData`,
// `jdk.internal.module.ServicesCatalog.getServicesCatalog` and
// `jdk.internal.loader.AbstractClassLoaderValue.putIfAbsent`.  These all
// follow the same pattern: cache a field offset once at <clinit> time, then
// drive a `compareAndSetReference` (or `compareAndSetInt`) loop for lazy
// initialisation.
//
// If our `objectFieldOffset1` cannot resolve the (Class, fieldName) pair —
// for example when the field is declared on `java.lang.Class` itself but our
// declared-fields list for `java/lang/Class` does not include the synthetic
// `reflectionData` / `annotationData` / `annotationType` slots — it used to
// return `0`.  The caller's CAS loop then reads slot 0 of the receiver
// (which is the classId Int slot for a Class mirror, or the object header
// for a generic object).  Because that slot's value never matches the
// `expected` reference the lazy-init guard is comparing against, the CAS
// loop never makes progress and the thread livelocks — observable as the
// 60-second watchdog dump on Spring Boot startup and WildFly bootstrap.
//
// Fix: instead of returning `0`, mint a unique non-zero synthetic offset
// in the range `[SYNTHETIC_OFFSET_BASE, SYNTHETIC_OFFSET_BASE + 2^28)` and
// route Unsafe load/CAS/store on that offset through a side-channel
// `(ObjectRef, offset) -> Value` store.  The store is per-object so two
// Class mirrors don't alias each other's lazy state.  The synthetic
// offset is well below `MAX_REASONABLE_OFFSET = 1 << 30` so the existing
// `unsafe_offset` clamp doesn't strip it back to 0.
// =====================================================================

pub const SYNTHETIC_OFFSET_BASE: usize = 0x2000_0000;
pub const SYNTHETIC_OFFSET_END: usize = 0x3FFF_FFFF;

#[inline]
pub fn is_synthetic_offset(off: usize) -> bool {
    (SYNTHETIC_OFFSET_BASE..=SYNTHETIC_OFFSET_END).contains(&off)
}

/// Map a `(class_name, field_name)` pair to a stable non-zero synthetic
/// offset.  Stable across calls so the JDK's "cache offset at <clinit>,
/// reuse forever" pattern stays consistent.
///
/// Perf note: the key type is `(Arc<str>, Arc<str>)` interned via
/// `cratonvm_types::intern_arc` so repeat lookups perform zero heap
/// allocations (the string pool returns an existing Arc on hit).  The
/// map is sharded across `UNSAFE_SHARDS` parking_lot mutexes to avoid
/// the single-mutex contention point under concurrent <clinit> traffic.
/// Mint and register a static field's `Unsafe` offset, exactly as
/// `Unsafe.staticFieldOffset(Field)` does.
///
/// Exists for `post_clinit_fixup`. `sun.misc.Unsafe.<clinit>` calls
/// `staticFieldBase`/`staticFieldOffset` for its memory-access warning latch
/// before either native is registered, so both return their return type's zero
/// -- `null` and `0L` -- with no exception, and the latch's accesses then miss
/// the static-field side table entirely.
///
/// The REGISTRATION is the load-bearing half: an offset that
/// `unsafe_static_field_target` cannot resolve routes a null-base access into
/// a private side store instead of the real static slot. Minting the number
/// without registering it would move the defect rather than fix it.
///
/// VM-LESS, kept only for its one caller until
/// `docs/feature-designs/native-dispatch-architecture.md`
/// moves it to [`register_static_field_offset_in_vm`]. A class id means
/// nothing outside the VM that minted it, so this records the class NAME too,
/// and a reader in any VM accepts the registration only for a class of that
/// name in its own VM (gc-common w8-a).
pub fn register_static_field_offset(
    class_name: &str,
    field_name: &str,
    class_id: ClassId,
    field_index: usize,
) -> i64 {
    let offset = synthetic_offset_for(class_name, &format!("static:{field_name}"));
    crate::unsafe_natives_ext::remember_unscoped_static_field_offset(
        offset,
        class_name,
        class_id,
        field_index,
    );
    offset as i64
}

/// The synthetic `Unsafe` offset of static `field_name` of the class
/// `class_id` named `class_name` -- what `staticFieldOffset` answers, and what
/// the per-VM static-field table ([`unsafe_natives_ext::remember_unsafe_static_field_offset`],
/// keyed by `(vm, offset)`) resolves back to that class's slot.
///
/// Interpreter round i5 wave 103, lane L2 (page
/// `interpreter-i103-L2-compatible-unsafe-static-offsets-keyed-by-class-name-FIXED-20261016`):
/// the class id is part of the key. Minted by NAME alone, two classes of one
/// name in one VM (an application class and a user loader's copy of the same
/// bytes) shared one offset, and the table answered whichever registered
/// last: `Field.setBoolean` / `getInt` on the application class's static, served
/// by the JDK's Unsafe accessors under `--compatible`, read and wrote the copy's
/// static (`L2W103RetiredBodyEscape`'s `direct` rows). The offset ignores the
/// base object on that path, so the key is what must tell the classes apart.
/// Still stable per class across calls (the JDK caches it at `<clinit>`), and
/// the same in every VM that gives the class the same id.
pub fn static_field_synthetic_offset(
    class_name: &str,
    field_name: &str,
    class_id: ClassId,
) -> usize {
    synthetic_offset_for(
        class_name,
        &format!("static:{field_name}#{}", class_id.as_u32()),
    )
}

/// [`register_static_field_offset`] for VM `vm_identity` (`SharedVm::vm_identity`):
/// the registration is visible to that VM's Unsafe accesses only, which is
/// what `Unsafe.staticFieldOffset` itself does (gc-common w8-a).
pub fn register_static_field_offset_in_vm(
    vm_identity: usize,
    class_name: &str,
    field_name: &str,
    class_id: ClassId,
    field_index: usize,
) -> i64 {
    let offset = static_field_synthetic_offset(class_name, field_name, class_id);
    crate::unsafe_natives_ext::remember_unsafe_static_field_offset(
        vm_identity,
        offset,
        class_id,
        field_index,
    );
    offset as i64
}

pub fn synthetic_offset_for(class_name: &str, field_name: &str) -> usize {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    type Key = (Arc<str>, Arc<str>);
    static T: std::sync::OnceLock<UnsafeShardedMap<Key, usize>> = std::sync::OnceLock::new();
    static NEXT: AtomicUsize = AtomicUsize::new(SYNTHETIC_OFFSET_BASE);
    let shards = T.get_or_init(new_unsafe_sharded_map);
    // intern_arc: pool hit returns existing Arc<str> with no allocation;
    // first-time miss allocates once and is amortised across the lifetime
    // of the JIT/class-loader state that derives these names.
    let key: Key = (
        cratonvm_types::intern_arc(class_name),
        cratonvm_types::intern_arc(field_name),
    );
    let shard_idx = unsafe_shard_of(&key);
    let mut map = shards[shard_idx].lock();
    if let Some(off) = map.get(&key) {
        return *off;
    }
    let off = NEXT.fetch_add(1, Ordering::Relaxed);
    debug_assert!(off < SYNTHETIC_OFFSET_END);
    map.insert(key, off);
    off
}

#[derive(Clone, Copy)]
pub struct UnsafeStaticFieldTarget {
    pub class_id: u32,
    pub field_index: usize,
    /// JIT round 21 (lane unsafe): the static's declared descriptor byte,
    /// resolved on the first sub-word access (`0` = not yet); the lane
    /// natives read it here instead of scanning the class's fields by name
    /// on every access (`unsafe_natives_ext::unsafe_static_desc_at`).
    pub desc: u8,
}

/// Per-object side store for fields stored at a synthetic offset.  Keyed
/// by `(identity_hash, offset)` so each receiver has its own slot and
/// CAS sees a consistent value across the load and the compare-and-store.
///
/// Round-8 Bug 4: migrated from `std::sync::Mutex<HashMap<...>>` (poisoning
/// + SipHash overhead) to `parking_lot::Mutex<FxHashMap<...>>`. Keys are
/// derived from VM-internal identity hashes and synthetic offsets, never
/// from untrusted user input, so DoS-resistance via SipHash is not required.
///
/// Round-9 native-builtins HIGH (Bug 7): the key changed from
/// `(ObjectRef-as-usize, offset)` to `(i32-identity-hash, usize-offset)`.
/// Round-8 fixed only the *Mutex* type; the raw-pointer key remained
/// vulnerable to the same GC-relocation aliasing that took out the
/// LongAdder cells — after a moving GC, two unrelated objects can land at
/// the same address and silently share a side-store slot. JVM identity
/// hash is GC-stable, so it is the correct key.
///
/// gc-common w7-a (`common-w6a-unsafe-synthetic-field-store-is-shared-across-vms`):
/// the key now leads with the VM ([`SyntheticFieldKey`]). Identity hashes are
/// minted per heap from a counter that starts at the same value in every VM,
/// so the Nth object hashed in VM A and the Nth in VM B used to share every
/// synthetic slot: a CAS in one VM compared against, and returned, an object
/// in the other VM's heap, and the `unsafe-side-store` root row rooted and
/// remapped both VMs' values in whichever VM collected.
///
/// gc-common w29-b (`common-w28b-remaining-identity-hash-keyed-side-tables`,
/// rank 5): the receiver half of the key is the receiver's WEAK lock key, not
/// its identity hash. Two live receivers of one VM share an identity hash once
/// the heap's counter has wrapped, and they shared every synthetic slot -- a
/// CAS on one compared against, and returned, the other's value. And a row
/// used to stay until VM teardown, with its value rooted (the
/// `unsafe-side-store` root row), for every receiver that ever used a
/// synthetic slot; now the lock-key registry frees the key when the receiver
/// dies, and [`forget_synthetic_field_keys`] drops its rows (and so unroots
/// their values).
pub fn synthetic_field_store() -> &'static SyntheticFieldStore {
    static T: std::sync::OnceLock<SyntheticFieldStore> = std::sync::OnceLock::new();
    T.get_or_init(|| parking_lot::Mutex::new(rustc_hash::FxHashMap::default()))
}

pub type SyntheticFieldStore = parking_lot::Mutex<rustc_hash::FxHashMap<SyntheticFieldKey, Value>>;

/// `(vm_identity, receiver's weak lock key, synthetic offset)`. See
/// [`synthetic_field_store`]. The VM leads, as the root scan and remap
/// (`unsafe_natives_ext::gc_scan_unsafe_side_store_roots` /
/// `gc_update_unsafe_side_store_refs`) select a VM's rows by it; the lock key
/// alone is already unique across VMs.
pub type SyntheticFieldKey = (usize, usize, usize);

/// The store key of `obj`'s synthetic slot at `offset`, in the calling VM,
/// minting the receiver's weak lock key on first use. For writes.
#[inline]
pub fn synthetic_field_key(
    ctx: &dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
    offset: usize,
) -> SyntheticFieldKey {
    // Round-9 Bug 7: the key must be GC-stable, which a raw pointer is not;
    // gc-common w29-b: and unique per live object, which the identity hash
    // is not. The weak lock key is both.
    (ctx.vm_identity(), lib_side_key(ctx, obj), offset)
}

/// [`synthetic_field_key`] without minting: `None` when `obj` was never keyed,
/// so it has no synthetic slot. For reads, which then answer the default.
#[inline]
pub fn existing_synthetic_field_key(
    ctx: &dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
    offset: usize,
) -> Option<SyntheticFieldKey> {
    Some((ctx.vm_identity(), existing_weak_lock_key(ctx, obj)?, offset))
}

/// Drop every synthetic slot filed under `keys`, which the lock-key registry
/// has just freed (the receiver died, or its VM is being torn down): its
/// values stop being `unsafe-side-store` roots. Called through
/// [`forget_core_object_state_keys`] with the registry lock released; the
/// store lock is a leaf.
pub fn forget_synthetic_field_keys(keys: &[usize]) {
    if keys.is_empty() {
        return;
    }
    let mut map = synthetic_field_store().lock();
    if map.is_empty() {
        return;
    }
    let freed: rustc_hash::FxHashSet<usize> = keys.iter().copied().collect();
    map.retain(|&(_, key, _), _| !freed.contains(&key));
}

pub fn synthetic_get(
    ctx: &mut dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
    offset: usize,
) -> Value {
    // JIT round 20 wave 11 (lane huntDisp): byte `k` of a static under
    // `CRATONVM_UNSAFE_BYTE_FIELD_OFFSETS`.
    if let Some(v) = crate::unsafe_natives_ext::unsafe_static_sub_get(&*ctx, Some(obj), offset) {
        return v;
    }
    if let Some((class_id, field_index)) = unsafe_static_field_target_for_base(ctx, obj, offset) {
        return ctx.get_static_field(class_id, field_index);
    }
    let Some(key) = existing_synthetic_field_key(&*ctx, obj, offset) else {
        return Value::Object(None);
    };
    let map = synthetic_field_store().lock();
    map.get(&key).copied().unwrap_or(Value::Object(None))
}

pub fn synthetic_put(
    ctx: &mut dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
    offset: usize,
    val: Value,
) {
    // JIT round 20 wave 11 (lane huntDisp): byte `k` of a static.
    if crate::unsafe_natives_ext::unsafe_static_sub_put(ctx, Some(obj), offset, val) {
        return;
    }
    if let Some((class_id, field_index)) = unsafe_static_field_target_for_base(ctx, obj, offset) {
        ctx.set_static_field(class_id, field_index, val);
        return;
    }
    let key = synthetic_field_key(&*ctx, obj, offset);
    let mut map = synthetic_field_store().lock();
    map.insert(key, val);
}

/// CAS on the synthetic per-object slot.  Returns true on success.  The
/// expected/new pair may be Object, Int or Long; the comparison is by the
/// raw bit pattern (Object identity for refs, value for primitives).
pub fn synthetic_cas(
    ctx: &mut dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
    offset: usize,
    expected: Value,
    new_val: Value,
) -> bool {
    // JIT round 20 wave 11 (lane huntDisp): byte `k` of a static.
    if let Some(ok) =
        crate::unsafe_natives_ext::unsafe_static_sub_cas(ctx, Some(obj), offset, expected, new_val)
    {
        return ok;
    }
    if let Some(row) = crate::unsafe_natives_ext::unsafe_static_field_row_for_base(ctx, obj, offset)
    {
        let class_id = ClassId::new(row.class_id);
        let field_index = row.field_index;
        // JIT round 23 wave 1, lane L4 (`CRATONVM_UNSAFE_STATIC_HW_RMW`): a
        // mirror-based static CAS (`U.compareAndSetInt(Foo.class, OFF, ..)`,
        // and every `getAndAdd` / `getAndSet` loop over it, e.g.
        // `ForkJoinPool.getAndAddPoolIds`) is one hardware CAS on the static
        // cell. The read-compare-write below takes no lock at all, so two
        // racing CASes could both succeed.
        // JIT round 23 wave 2, lane L4: under the class mirror's monitor when
        // the hardware update declines (`unsafe_static_rmw`).
        // JIT round 21, lane atomics (STATICCAS21-2): publish the pair for the
        // compiled `Unsafe` RMW prefixes' static arm (a no-op unless a compile
        // armed it).
        // A cached refusal belongs to this registration, including its mirror.
        if matches!(row.desc, 0 | b'I' | b'J') {
            crate::unsafe_natives_ext::unsafe_note_static_atomic_target(
                &*ctx,
                offset,
                class_id,
                field_index,
            );
        }
        let live = [expected, new_val];
        if let Some((_, ok)) = crate::unsafe_natives_ext::unsafe_static_rmw(
            ctx,
            class_id,
            field_index,
            &live,
            &|cur, live| {
                let nv = crate::unsafe_natives_ext::unsafe_retag_bits_like(&cur, live[1]);
                unsafe_cas_values_equal(cur, live[0]).then_some(nv)
            },
        ) {
            return ok;
        }
        let cur = ctx.get_static_field(class_id, field_index);
        // Interpreter round i6 wave 127, lane L4: a bit-equal float / double
        // static keeps its tag (`unsafe_retag_bits_like`).
        let new_val = crate::unsafe_natives_ext::unsafe_retag_bits_like(&cur, new_val);
        let ok = unsafe_cas_values_equal(cur, expected);
        if ok {
            ctx.set_static_field(class_id, field_index, new_val);
        }
        return ok;
    }
    // Mints the receiver's key: a CAS is a write (a weak slot, dropped with
    // the receiver, so a failing CAS leaves nothing that outlives it).
    let key = synthetic_field_key(&*ctx, obj, offset);
    let mut map = synthetic_field_store().lock();
    let cur = map.get(&key).copied().unwrap_or(Value::Object(None));
    let eq = unsafe_cas_values_equal(cur, expected);
    if eq {
        map.insert(key, new_val);
    }
    eq
}

/// Per-VM teardown for the two `ObjectRef`-holding Unsafe side stores (the
/// null-base object store and the synthetic per-receiver store): drop every
/// row of `vm`. Idempotent. Rows of a disposed VM are never scanned by another
/// VM's collection (both GC hooks walk only the collecting VM's rows), so this
/// only returns their memory; `vm_identity` is never recycled.
///
/// gc-common w7-a. Wired into `vm_init::release_vm_native_state` by
/// `docs/feature-designs/native-dispatch-architecture.md`.
///
/// gc-common w8-a: also drops `vm`'s rows of the two null-base primitive
/// stores (which hold its tid counter) and of the offset -> static-field map,
/// all keyed by `(vm_identity, offset)` since then, and runs
/// [`forget_vm_native_root_stores`] -- this is the teardown hook
/// `release_vm_native_state` already calls, so the root-source stores w8-a
/// made VM-scoped are released without a second call site in `vm_init.rs`.
pub fn forget_vm_unsafe_side_stores(vm: usize) {
    forget_vm_native_root_stores(vm);
    for shard in static_obj_store().iter() {
        shard.lock().retain(|&(owner, _), _| owner != vm);
    }
    for shard in static_long_store().iter() {
        shard.lock().retain(|&(owner, _), _| owner != vm);
    }
    for shard in static_int_store().iter() {
        shard.lock().retain(|&(owner, _), _| owner != vm);
    }
    crate::unsafe_natives_ext::forget_vm_unsafe_static_field_targets(vm);
    synthetic_field_store()
        .lock()
        .retain(|&(owner, _, _), _| owner != vm);
}

/// Per-VM teardown for the root-source stores gc-common w8-a made VM-scoped
/// (`common-b-process-global-root-sources`): the lambda `CallSite` cache, the
/// jboss-msc service rows, the `DatagramSocket` / `InetAddress` side tables,
/// the scheduled-pump tasks and the xnio future / OptionMap / Builder
/// registries. Idempotent.
///
/// None of this is needed for correctness -- another VM's collection no longer
/// scans these rows -- but a torn-down VM's rows otherwise hold raw addresses
/// into a freed heap for the life of the process, and an address-keyed row
/// (the socket tables) could then answer a lookup for a new VM's object at the
/// same address. Runs from [`forget_vm_unsafe_side_stores`], which
/// `vm_init::release_vm_native_state` calls at VM teardown.
///
/// The `http-handlers` server registry goes too, since gc-common w16-a, from
/// inside `net_phase_e::forget_vm_socket_side_tables`
/// (`net_phase_e::forget_vm_http_servers`): dropping a row closes its
/// listener, which was the reason to leave it to `HttpServer.stop`, but at
/// teardown no thread of the VM can serve that listener any more.
///
/// Not here: the `panama-upcalls` trampolines (leaked for the life of the
/// process by design: C code may still hold the code pointer).
pub fn forget_vm_native_root_stores(vm: usize) {
    crate::lang_invoke::forget_vm_lambda_callsite_cache(vm);
    crate::jboss_msc::forget_vm_msc_service_roots(vm);
    crate::net_phase_e::forget_vm_socket_side_tables(vm);
    crate::servlet::forget_vm_s2_sockets(vm);
    crate::plain_socket::forget_vm_plain_sockets(vm);
    crate::scheduled_pump::registry().forget_vm(vm);
    crate::xnio_async::forget_vm_xnio_roots(vm);
    // gc-common w9-b: the fork-join and TLS rows, and the per-VM singletons
    // and identity-hash registries lane B9 re-keyed. The TLS tables go before
    // the lock keys: their scan asks `lock_key_owner_vm` which VM a context
    // key belongs to.
    crate::phases_early::forget_vm_forkjoin_state(vm);
    crate::t27_tls::forget_vm_tls_state(vm);
    crate::x509_manager::forget_vm_key_manager_states(vm);
    crate::http_url_connection::forget_vm_http_url_connection_state(vm);
    cratonvm_native_api::server_socket_ports::forget_vm(vm);
    crate::uncaught_handlers::forget_vm_uncaught_handlers(vm);
    forget_vm_async_worker_pool(vm);
    forget_vm_lock_keys(vm);
    forget_vm_native_object_singletons(vm);
    crate::wildfly_core::forget_vm_wildfly_jboss_state(vm);
    crate::logmanager::forget_vm_logging_state(vm);
    crate::phases_late::net_channels::forget_vm_http_link_rows(vm);
    crate::phases_late::forget_vm_phases_late_state(vm);
    crate::phases_late::concurrent::forget_vm_concurrent_tables(vm);
    // gc-common w19-a: this VM's `File.deleteOnExit()` files are deleted at its
    // termination, not at the host process's (`common-w18f-delete-on-exit-list-is-process-wide`).
    crate::phases_late::nio_file::forget_vm_delete_on_exit(vm);
    // gc-common w19-a: the Tomcat `Mapper.internalMap` memo's rows hold this
    // heap's raw addresses.
    mapper_internal_fast_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|&(owner, _), _| owner != vm);
    crate::quarkus_staticinit::forget_vm_quarkus_timing(vm);
    crate::phases_early::forget_vm_synthetic_security_state(vm); // gc-common w19-c
    crate::jca::provider_chain::forget_vm_security_property_overrides(vm); // gc-common w19-c handoff
    crate::panama::forget_vm_ffm_singletons(vm); // gc-common w20-f
    crate::shared_secrets_bridge::forget_vm_shared_secrets_singletons(vm); // gc-common w20-f
    crate::wildfly_undertow::forget_vm_undertow_rows(vm); // gc-common w20-g
    forget_vm_netty_queue_state(vm); // gc-common w20-c: after `forget_vm_lock_keys`
    #[cfg(any(feature = "experimental-serialization", feature = "synthetic-jdk"))]
    crate::serialization::forget_vm_serialization_state(vm); // gc-common w21-c: after `forget_vm_lock_keys`
    crate::lang_class::forget_vm_lang_class_tables(vm); // gc-common w21-d
    forget_vm_real_charset_cache(vm); // gc-common w21-a
    forget_vm_system_stream_overrides(vm); // gc-common w21-a
    crate::hooks::jfr_forget_vm_jfr_state(vm); // gc-common w21-a
    crate::jca::provider_chain::forget_vm_real_provider_roots(vm); // gc-common w21-a
    crate::xml_xerces::forget_vm_spring_xml_grammar_pool(vm); // gc-common w22-a
    crate::lucene_es::forget_vm_randomized_caches(vm); // gc-common w22-a
    crate::jboss_jdkspecific::forget_vm_jboss_module_state(vm); // gc-common w22-a
    crate::xnio_conduits::forget_vm_xnio_conduit_rows(vm); // gc-common w22-a
    crate::xnio_worker::forget_vm_xnio_worker_rows(vm); // gc-common w23-b
    crate::jca::provider_chain::forget_vm_provider_registry(vm); // gc-common w22-b
    crate::infinispan_local::forget_vm_infinispan_state(vm); // gc-common w30-a
}

// =====================================================================
// Class$Atomic CAS overrides — sidestep broken Unsafe path for the JDK's
// internal lazy-init guards on Class.{reflectionData,annotationType,
// annotationData}. These are static helpers in `java.lang.Class$Atomic`
// that drive a `U.compareAndSetReference(c, FIELD_OFFSET, exp, new)`
// loop. Our java.lang.Class mirror does not expose those synthetic
// fields, so the offset cache resolves to a slot that never CAS-matches
// and we livelock (observed in WildFlySecurityManager.<clinit>). Bypass
// Unsafe entirely: store the per-Class-mirror state in a Rust-side
// table keyed by mirror pointer.
// =====================================================================

// Keys are VM-internal (Class mirror identity hash + slot tag) so SipHash
// DoS-resistance is not required. See the cluster-comment above
// `static_long_store` for the wider rationale.
//
// Round-9 native-builtins HIGH (Bug 7): the key is the mirror's identity hash,
// not its address. The raw-pointer key was vulnerable to GC moves — a Class
// mirror surviving a G1 evacuation lands at a different address, breaking the
// side-store lookup and making the CAS loop livelock again. Identity hash is
// GC-stable.
//
// gc-common w3-b (`common-w2b-class-atomic-side-store-roots-every-reflected-class-FIXED-20260923.md`):
// three defects of the old `(identity hash, tag) -> Option<ObjectRef>` store,
// closed together.
//
// * **It was one process-global map.** Two VMs in one process shared rows, so
//   VM B's `Class.reflectionData()` could return a `SoftReference` in VM A's
//   heap. Now partitioned by `vm_identity` (`VmScoped`); the per-VM row is
//   dropped by `forget_vm_class_atomic_side_store`.
// * **An identity-hash collision served another class's state.** Hashes are
//   32-bit, so two mirrors can share one, and `Class.reflectionData()` then
//   answered class B's `getDeclaredMethods()` with class A's `ReflectionData`.
//   Every slot now records the `ClassId` of the mirror that wrote it, and a
//   read from a mirror of any other class is a MISS (the caller rebuilds and
//   overwrites the row).
// * **Every row was an unconditional GC root.** The `SoftReference ->
//   ReflectionData -> Method -> Class -> loader` chain (and `AnnotationData`'s
//   annotation proxies) therefore made every class that was ever reflected on
//   immortal, with its loader and every class that loader defined. The owning
//   `ClassId` is what lets the VM's `class-atomic-slots` root source pin a
//   user-loader class's rows to that loader (`metadata_pin`) instead of
//   rooting them, the same conditional edge statics and `ClassValue` results
//   use; `prune_class_atomic_side_store` drops rows of unloaded classes and
//   rows whose value did not survive.
//
// Dropping a row is always safe: `Class.reflectionData()` rebuilds on a miss
// (`native_class_new_reflection_data`), and a CAS finding no row of its class
// succeeds whatever it expected (`class_atomic_cas`). That second half matters
// since interpreter round i1 wave 27 (lane L3): a JVMTI redefinition advances
// `classRedefinedCount`, so `Class.annotationData()` now re-CASes over a
// NON-null stale value, which a dropped row (or a colliding class's) no longer
// matches; the old "only CAS'd from null" rule made that a livelock.

/// One `Class$Atomic` slot: the value and the class whose mirror wrote it.
#[derive(Clone, Copy)]
pub struct ClassAtomicSlot {
    /// `ClassId` of the mirror the slot belongs to (`class_id_from_mirror`);
    /// `None` for a mirror the VM cannot map to a class (primitive mirrors).
    pub class_id: Option<u32>,
    pub value: Option<cratonvm_types::ObjectRef>,
}

/// `(mirror identity hash, slot tag)`. Tags: reflectionData=0,
/// annotationType=1, annotationData=2.
pub type ClassAtomicKey = (i32, u8);

/// Per-VM `Class$Atomic` side store. Lock discipline is `VmScoped`'s: nothing
/// allocates, dispatches Java, or takes another VM lock while holding it (the
/// GC root scan takes it at a safepoint). `class_id_from_mirror` reads the
/// VM's reverse-mirror map, so the owner is always resolved BEFORE the lock.
pub static CLASS_ATOMIC_SIDE_STORE: cratonvm_native_api::vm_scoped::VmScoped<
    rustc_hash::FxHashMap<ClassAtomicKey, ClassAtomicSlot>,
> = cratonvm_native_api::vm_scoped::VmScoped::new();

/// The store key and owning class of `mirror`'s `tag` slot. Resolved outside
/// the store lock (see [`CLASS_ATOMIC_SIDE_STORE`]).
pub fn class_atomic_key(
    ctx: &dyn NativeContext,
    mirror: cratonvm_types::ObjectRef,
    tag: u8,
) -> (ClassAtomicKey, Option<u32>) {
    (
        (ctx.identity_hash_code(mirror), tag),
        ctx.class_id_from_mirror(mirror).map(|c| c.as_u32()),
    )
}

/// The value of a slot, or `None` when the slot is empty OR was written by a
/// mirror of a different class (an identity-hash collision).
pub fn class_atomic_get(
    vm: usize,
    key: ClassAtomicKey,
    owner: Option<u32>,
) -> Option<cratonvm_types::ObjectRef> {
    CLASS_ATOMIC_SIDE_STORE
        .peek(vm, |rows| {
            rows.get(&key)
                .filter(|slot| slot.class_id == owner)
                .and_then(|slot| slot.value)
        })
        .flatten()
}

/// Unconditional store (the `newReflectionData` path).
pub fn class_atomic_put(
    vm: usize,
    key: ClassAtomicKey,
    owner: Option<u32>,
    value: Option<cratonvm_types::ObjectRef>,
) {
    CLASS_ATOMIC_SIDE_STORE.with(vm, |rows| {
        rows.insert(
            key,
            ClassAtomicSlot {
                class_id: owner,
                value,
            },
        );
    });
}

/// Compare-and-set. A row another class wrote reads as absent, and a CAS that
/// finds no row of its own class succeeds whatever it expected: the store
/// holds no value to lose, and the JDK's callers pass the heap field they
/// just read as `expected`. So a colliding class's CAS succeeds instead of
/// failing forever (the old store's collision was a livelock as well as a
/// wrong answer: the JDK's `annotationData()` loop retries while its own heap
/// field stays unchanged), and so does a CAS over a stale non-null value whose
/// row was pruned (reachable since a redefinition moves
/// `classRedefinedCount`; interpreter round i1 wave 27, lane L3).
pub fn class_atomic_cas(
    vm: usize,
    key: ClassAtomicKey,
    owner: Option<u32>,
    expected: Option<cratonvm_types::ObjectRef>,
    new_val: Option<cratonvm_types::ObjectRef>,
) -> bool {
    CLASS_ATOMIC_SIDE_STORE.with(vm, |rows| {
        let cur = rows
            .get(&key)
            .filter(|slot| slot.class_id == owner)
            .map(|slot| slot.value);
        if cur.map_or(true, |cur| cur == expected) {
            rows.insert(
                key,
                ClassAtomicSlot {
                    class_id: owner,
                    value: new_val,
                },
            );
            true
        } else {
            false
        }
    })
}

/// GC root scan half of the VM's `class-atomic-slots` root source
/// (`vm/src/memory/native_roots.rs`): every non-null slot of `vm`, with the
/// `ClassId` that owns it, so the VM can pin a user-loader class's slots to
/// that loader instead of rooting them. Read-only (`peek`): a VM that never
/// reflected gets no row.
pub fn gc_scan_class_atomic_side_store(
    vm: usize,
    visit: &mut dyn FnMut(Option<u32>, cratonvm_types::ObjectRef),
) {
    CLASS_ATOMIC_SIDE_STORE.peek(vm, |rows| {
        for slot in rows.values() {
            if let Some(v) = slot.value {
                visit(slot.class_id, v);
            }
        }
    });
}

/// Post-move remap half of the `class-atomic-slots` root source. Rows of `vm`
/// only; entries absent from the map did not move.
pub fn gc_update_class_atomic_side_store_refs(vm: usize, pointer_map: &cratonvm_types::PointerMap) {
    if pointer_map.is_empty() {
        return;
    }
    if !CLASS_ATOMIC_SIDE_STORE.has_row(vm) {
        return;
    }
    CLASS_ATOMIC_SIDE_STORE.with(vm, |rows| {
        for slot in rows.values_mut() {
            if let Some(o) = slot.value.as_mut() {
                let old_addr = o.as_ptr() as usize;
                if let Some(&new_addr) = pointer_map.get(&old_addr) {
                    debug_assert!(new_addr != 0, "GC pointer map contains null address");
                    *o = unsafe { cratonvm_types::ObjectRef::from_raw(new_addr as *mut u8) };
                }
            }
        }
    });
}

/// Drop the rows of `vm` whose owning class is dead (`dead_class`) or whose
/// value did not survive this collection (`is_live`, over PRE-remap
/// addresses). Returns the number of rows dropped.
///
/// Called from class unloading (every row of an unloaded class) and from the
/// post-GC mirror reconciliation (a value deferred to a loader that died must
/// not stay keyed: the post-move fix-up would otherwise "remap" a reclaimed
/// address onto whatever the allocator put there next). Always safe — see the
/// store's doc.
pub fn prune_class_atomic_side_store(
    vm: usize,
    dead_class: &dyn Fn(u32) -> bool,
    is_live: &dyn Fn(usize) -> bool,
) -> usize {
    if !CLASS_ATOMIC_SIDE_STORE.has_row(vm) {
        return 0;
    }
    CLASS_ATOMIC_SIDE_STORE.with(vm, |rows| {
        let before = rows.len();
        rows.retain(|_, slot| {
            !slot.class_id.is_some_and(dead_class)
                && slot.value.map_or(true, |v| is_live(v.as_ptr() as usize))
        });
        before - rows.len()
    })
}

/// Per-VM teardown for the `Class$Atomic` side store. Called (through
/// `lang_class::forget_vm_annotation_proxies`) from `release_vm_native_state`.
pub fn forget_vm_class_atomic_side_store(vm: usize) {
    CLASS_ATOMIC_SIDE_STORE.forget(vm);
}


#[inline]
pub fn class_atomic_cas_impl(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    slot_tag: u8,
) -> MethodCallResult {
    // Static method: args[0]=Class mirror, args[1]=expected, args[2]=new.
    let class_ref = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let expected = match args.get(1) {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    let new_val = match args.get(2) {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    let (key, owner) = class_atomic_key(&*ctx, class_ref, slot_tag);
    if class_atomic_cas(ctx.vm_identity(), key, owner, expected, new_val) {
        Ok(Some(Value::Int(1)))
    } else {
        Ok(Some(Value::Int(0)))
    }
}

pub fn native_class_atomic_cas_reflection_data(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Same as `class_atomic_cas_impl` but ALSO mirrors the new value
    // into slot 11 (`Class.reflectionData`) of the receiver class
    // mirror, so the direct `getfield this.reflectionData` performed
    // by `Class.reflectionData()` and `Class.newReflectionData()` sees
    // the value stored by the CAS. Without this mirror-write the cas
    // succeeds once, then `newReflectionData()`'s `while(true)` loop
    // livelocks forever (T20.RD on WildFly / SportMe bootstrap).
    // Round-9 Bug 7: thread ctx through so the side store keys by identity hash.
    let result = class_atomic_cas_impl(ctx, args, 0)?;
    if let Some(Value::Int(1)) = result {
        if let (Some(Value::Object(Some(class_mirror))), Some(Value::Object(new_soft_ref))) =
            (args.first(), args.get(2))
        {
            ctx.set_field(*class_mirror, 11, Value::Object(*new_soft_ref));
        }
    }
    Ok(result)
}

/// T20.RD: native body of `java/lang/Class$ReflectionData.<init>(I)V`.
///
/// The JDK source is `this.redefinedCount = redefinedCount;` plus the
/// implicit super-constructor call. Implementing this as a native
/// (instead of running the bytecode body) bypasses any class-init /
/// JIT path that may itself be blocked on reflection during early
/// WildFly bootstrap.
pub fn native_class_reflection_data_init(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let count = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    if let Some(idx) = ctx.resolve_field_index("java/lang/Class$ReflectionData", "redefinedCount") {
        ctx.set_field(this, idx, Value::Int(count));
    } else {
        // Fall back to set-by-name; if the field can't be resolved,
        // leave the slot at default (0). The reflection-data cache
        // logic only checks for equality against the receiver's
        // classRedefinedCount (0 until a JVMTI redefinition of the class).
        ctx.set_field_by_name(this, "redefinedCount", Value::Int(count));
    }
    Ok(None)
}

/// T20.RD: native body of `java/lang/Class.newReflectionData(SoftReference, int)`.
///
/// Allocates a fresh `Class$ReflectionData`, runs our `<init>(I)V`
/// native to set `redefinedCount`, wraps it in a `SoftReference`,
/// writes the SoftReference into slot 11 (reflectionData) of the
/// receiver class mirror, AND mirrors it into the cas side store so
/// any subsequent `casReflectionData` observes the same value. The
/// JDK's `while(true)` CAS loop is replaced by a single allocation —
/// safe because we hold no contention against any other thread on
/// this slot (the side store and the heap slot are written together).
///
/// Native body of `java/lang/Class.reflectionData()Ljava/lang/Class$ReflectionData;`.
///
/// The JDK getter reads `this.reflectionData` (heap slot 11) directly. That slot
/// is an old(Class mirror)→young(SoftReference) edge that the young collector
/// reclaims/relocates WITHOUT honoring (its card never goes dirty — confirmed by
/// `CRATONVM_DBG_RSET_AUDIT`: `Class fld[11] -> young CLEAN` every cycle), so the
/// getfield reads a freed / dangling SoftReference (all-zero header) and
/// `SoftReference.get()` on it faults the reflection path — the Tomcat DoHead
/// start/stop flood whose receiver is always `java/lang/ref/SoftReference`.
///
/// The cas side store (`CLASS_ATOMIC_SIDE_STORE`, slot_tag 0) holds the SAME
/// SoftReference and is GC-safe (the VM's `class-atomic-slots` root source
/// keeps it live — rooted, or pinned to a user-loader class's loader — and
/// remaps it). Read from there instead of the dangling heap slot.
/// `newReflectionData` keeps both in sync on write, and a missing / cleared /
/// other-class entry simply re-creates the cache.
///
/// Like the JDK body it replaces, a cached `ReflectionData` is current only if
/// it was built for the class's `classRedefinedCount`, which a JVMTI
/// redefinition advances (`vm_object::note_class_redefined_for_reflection`;
/// interpreter round i1 wave 27, lane L3). Until one has happened in this
/// process the count is 0 everywhere and neither field is read.
pub fn native_class_reflection_data(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let redefined_count = if cratonvm_classloading::any_class_redefined() {
        match ctx.get_field_by_name(this, "classRedefinedCount") {
            Value::Int(count) => count,
            _ => 0,
        }
    } else {
        0
    };
    let cached = {
        let (key, owner) = class_atomic_key(&*ctx, this, 0);
        class_atomic_get(ctx.vm_identity(), key, owner)
    };
    if let Some(soft_ref) = cached {
        // Soft referents are NOT nulled pre-GC (only weak/phantom are), and the
        // referent is kept live by tracing the SoftReference, so this returns
        // the live ReflectionData (or `null` after a soft clear, which rebuilds).
        if let Value::Object(Some(rd)) = ctx.get_field_by_name(soft_ref, "referent") {
            let built_for = if redefined_count == 0 {
                0
            } else {
                match ctx.get_field_by_name(rd, "redefinedCount") {
                    Value::Int(count) => count,
                    _ => 0,
                }
            };
            if built_for == redefined_count {
                return Ok(Some(Value::Object(Some(rd))));
            }
        }
    }
    // No live, current ReflectionData — create a fresh one for the current
    // count (repopulates slot 11 + side store). `oldSoftRef` is irrelevant to
    // our single-shot create path.
    native_class_new_reflection_data(
        ctx,
        &[
            Value::Object(Some(this)),
            Value::Object(None),
            Value::Int(redefined_count),
        ],
    )
}

pub fn native_class_new_reflection_data(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let count = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };

    // gc-common w20-c: both allocations below can collect; the mirror and the
    // `ReflectionData` were used raw after them. Pinned (the dispatcher
    // truncates the pin stack on the early returns).
    let this_pin = ctx.pin_native_root(this);

    // 1. Allocate a fresh `Class$ReflectionData`.
    let rd = match ctx.new_object("java/lang/Class$ReflectionData")? {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let rd_pin = ctx.pin_native_root(rd);
    // 2. Initialise `redefinedCount` directly (the <init> native does
    //    this; calling it through `invoke` would also work but keep
    //    this path self-contained to avoid recursive native dispatch).
    if let Some(idx) = ctx.resolve_field_index("java/lang/Class$ReflectionData", "redefinedCount") {
        ctx.set_field(rd, idx, Value::Int(count));
    } else {
        ctx.set_field_by_name(rd, "redefinedCount", Value::Int(count));
    }

    // 3. Wrap in a SoftReference. We use `new_object` + bytecode
    //    `<init>(Object)V`. SoftReference's referent is stored in the
    //    Reference superclass field `referent`.
    let soft_ref = match ctx.new_object("java/lang/ref/SoftReference")? {
        Some(Value::Object(Some(o))) => o,
        _ => {
            let rd = ctx.read_native_pin(rd_pin, rd);
            ctx.unpin_native_roots(this_pin);
            return Ok(Some(Value::Object(Some(rd))));
        }
    };
    let rd: ObjectRef = ctx.read_native_pin(rd_pin, rd);
    let this: ObjectRef = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    // SoftReference's `<init>(Object)V` calls Reference's <init>(Object,ReferenceQueue)
    // with a null queue. Set the referent field directly — works regardless of
    // whether SoftReference's constructor runs (avoids reentrant reflection
    // during bootstrap).
    ctx.set_field_by_name(soft_ref, "referent", Value::Object(Some(rd)));

    // 4. Write the SoftReference into mirror slot 11 (reflectionData).
    ctx.set_field(this, 11, Value::Object(Some(soft_ref)));

    // 5. Mirror into the cas side store so future casReflectionData
    //    calls observe the current value. Keyed by identity hash (Round-9
    //    Bug 7) and tagged with the owning class (gc-common w3-b).
    {
        let (key, owner) = class_atomic_key(&*ctx, this, 0);
        class_atomic_put(ctx.vm_identity(), key, owner, Some(soft_ref));
    }

    Ok(Some(Value::Object(Some(rd))))
}

pub fn native_class_atomic_cas_annotation_type(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // gc-common w3-b: mirror a successful CAS into the heap field, exactly as
    // `casAnnotationData` / `casReflectionData` do. `AnnotationType.getInstance`
    // reads `Class.annotationType` directly (`JavaLangAccess.getAnnotationType`)
    // and CASes `null -> fresh` only when that read is null; with the field
    // never written, the SECOND call for the same annotation type read null,
    // lost its CAS against the side store's first value, re-read the field —
    // still null — and returned `null` to `AnnotationParser` (an NPE on the
    // next `memberTypes()`). With the write, the second call sees the cached
    // instance and never CASes.
    let result = class_atomic_cas_impl(ctx, args, 1)?;
    if matches!(result, Some(Value::Int(1))) {
        if let (Some(Value::Object(Some(class_mirror))), Some(new_value)) =
            (args.first(), args.get(2))
        {
            ctx.set_field_by_name(*class_mirror, "annotationType", *new_value);
        }
    }
    Ok(result)
}

pub fn native_class_atomic_cas_annotation_data(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // `Class.annotationData()` reads this field directly before retrying its
    // CAS loop.  Keep that heap field coherent with the GC-safe side table,
    // just as `casReflectionData` does: otherwise every read observes null
    // even after a successful CAS and rebuilds the annotation graph.
    let result = class_atomic_cas_impl(ctx, args, 2)?;
    if matches!(result, Some(Value::Int(1))) {
        if let (Some(Value::Object(Some(class_mirror))), Some(new_value)) =
            (args.first(), args.get(2))
        {
            ctx.set_field_by_name(*class_mirror, "annotationData", *new_value);
        }
    }
    Ok(result)
}

// Sentinel offset (chosen in the usize-sparse high range to not collide
// with real slot indices) for Buffer.address probes; recognised by
// native_unsafe_get_long which answers non-zero.
pub const BUFFER_ADDRESS_SENTINEL: usize = 0x7fff_ffff_ffff_fffe;

/// Derive the `Unsafe.arrayIndexScale` value from an array class's
/// internal name (e.g. `[I`, `[Ljava/lang/Object;`). Returns 1 for
/// non-array or unrecognised shapes.
pub fn array_index_scale_for_name(name: &str) -> i32 {
    let bytes = name.as_bytes();
    if bytes.first() != Some(&b'[') {
        // A NON-ARRAY class answers 0. NOT because the javadoc says so --
        // it does not, and this comment claimed otherwise until 2026-09-02.
        // JDK 25's javadoc reads "arrays of 'narrow' types will generally not
        // work properly with accessors like getByte(Object, long), so the
        // scale factor for such classes is reported as zero": that is about
        // ARRAYS of narrow types, and it says nothing about a non-array.
        //
        // So 0 is this VM's CHOICE. The reason it is the right one: HotSpot
        // intends a refusal here and fails to deliver it (see below), and
        // reproducing a botched throw would make us wrong the day the JDK
        // fixes it -- while 0 is the answer callers guard on
        // (`if (scale == 0) throw`) and the safe direction for a caller doing
        // address arithmetic. It answered the
        // catch-all 1 until 2026-08-29 -- a plausible basis for address
        // arithmetic over a class that has no elements, which in this family
        // is the dangerous direction.
        //
        // The oracle cannot referee this row: HotSpot's own refusal names
        // `java/lang/InvalidClassException`, which does not exist, so the
        // throw fails to link and the caller gets a `NoClassDefFoundError`.
        // Recorded in
        // `unsafe-objectfieldoffset-accepted-a-static-and-the-jdk-refusal-that-is-itself-broken-20260826-RETIRED-20260920.md`,
        // and a prior session declined to change the value because the blast
        // radius was unmeasured. That page is RETIRED as of 2026-09-20: a
        // non-array no longer reaches this helper at all, because
        // `refuse_non_array_class` refuses a positively-named one in front of
        // both accessors. The `0` below is still the answer for a mirror this
        // VM cannot name, and is still this VM's choice rather than a match.
        // (Round 15 wave 4, lane jdkonly4: the refusal itself is now
        // HotSpot's botched throw, byte for byte -- `NoClassDefFoundError:
        // java/lang/InvalidClassException` -- because `AGENTS.md` binds both
        // modes to HotSpot; the "wrong the day the JDK fixes it" argument
        // above lost to that rule.)
        //
        // MEASURED 2026-08-29, whole 117-vector corpus, both modes: this
        // function was ASKED 161 (compatible) / 175 (strict) times, in every
        // one of the 117 vectors -- and NOT ONCE with a non-array class. The
        // blast radius is zero and the arm is unreachable from the corpus, so
        // taking the specified answer costs nothing and makes a
        // `scale == 0` guard fire where it should.
        return 0;
    }
    match bytes.get(1).copied() {
        Some(b'Z') | Some(b'B') => 1, // boolean, byte
        Some(b'S') | Some(b'C') => 2, // short, char
        Some(b'I') | Some(b'F') => 4, // int, float
        Some(b'J') | Some(b'D') => 8, // long, double
        Some(b'L') | Some(b'[') => 8, // reference / nested array
        _ => 1,
    }
}

pub fn native_scoped_memory_get_byte(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Round 12 wave 6 (lane ffm3): held from before the session check to
    // after the Unsafe access, so a shared close waits for it (see
    // `ffm_fast::AccessWindow`).
    let _window = crate::ffm_fast::AccessWindow::open();
    let epoch = crate::ffm_fast::epoch();
    crate::phases_late::foreign_ffm::p67_check_scoped_session_arg(ctx, args, 1)?;
    crate::capability_gate::gate_raw_memory_any(ctx)?;
    if let Some(Value::Object(Some(session))) = args.get(1) {
        crate::phases_late::foreign_ffm::p67_note_checked_session(ctx, *session, epoch);
    }
    let adapted = scoped_memory_access_unsafe_args(args);
    native_unsafe_get_byte_mb(ctx, &adapted)
}

pub fn native_scoped_memory_put_byte(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Round 12 wave 6 (lane ffm3): held from before the session check to
    // after the Unsafe access, so a shared close waits for it (see
    // `ffm_fast::AccessWindow`).
    let _window = crate::ffm_fast::AccessWindow::open();
    let epoch = crate::ffm_fast::epoch();
    crate::phases_late::foreign_ffm::p67_check_scoped_session_arg(ctx, args, 1)?;
    crate::capability_gate::gate_raw_memory_any(ctx)?;
    if let Some(Value::Object(Some(session))) = args.get(1) {
        crate::phases_late::foreign_ffm::p67_note_checked_session(ctx, *session, epoch);
    }
    let adapted = scoped_memory_access_unsafe_args(args);
    native_unsafe_put_byte_mb(ctx, &adapted)
}

scoped_scalar_access!(native_scoped_memory_put_short, native_unsafe_put_short_mb);
scoped_scalar_access!(native_scoped_memory_get_char, native_unsafe_get_char_mb);
scoped_scalar_access!(native_scoped_memory_put_char, native_unsafe_put_char_mb);
scoped_scalar_access!(native_scoped_memory_get_int, native_unsafe_get_int_mb);
scoped_scalar_access!(native_scoped_memory_put_int, native_unsafe_put_int_mb);
scoped_scalar_access!(native_scoped_memory_get_long, native_unsafe_get_long_mb);
scoped_scalar_access!(native_scoped_memory_put_long, native_unsafe_put_long_mb);

pub fn native_scoped_memory_copy_memory(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Two sessions: `(srcSession, dstSession, srcBase, srcOffset, dstBase,
    // dstOffset, bytes)`. Either or both may be null (a heap<->heap or a
    // null-session copy is legal), so each is checked only when present.
    // Round 12 wave 6 (lane ffm3): one window over both checks and the copy.
    let _window = crate::ffm_fast::AccessWindow::open();
    crate::phases_late::foreign_ffm::p67_check_scoped_session_arg(ctx, args, 1)?;
    crate::phases_late::foreign_ffm::p67_check_scoped_session_arg(ctx, args, 2)?;
    crate::capability_gate::gate_raw_memory_any(ctx)?;
    let adapted = [
        args.first().copied().unwrap_or(Value::Object(None)),
        args.get(3).copied().unwrap_or(Value::Object(None)),
        args.get(4).copied().unwrap_or(Value::Long(0)),
        args.get(5).copied().unwrap_or(Value::Object(None)),
        args.get(6).copied().unwrap_or(Value::Long(0)),
        args.get(7).copied().unwrap_or(Value::Long(0)),
    ];
    unsafe_natives::native_unsafe_copy_memory_consolidated(ctx, &adapted)
}

/// `ScopedMemoryAccess.copySwapMemoryInternal(srcSession, dstSession, srcBase,
/// srcOffset, destBase, destOffset, bytes, elemSize)`: a reviewed intrinsic
/// (round nio20 wave 3, lane L5; the review is at the registration). One window
/// over both session checks and the swapping copy, which is the
/// `copySwapMemory0` body (`[this, srcBase, srcOffset, destBase, destOffset,
/// bytes, elemSize]`).
pub fn native_scoped_memory_copy_swap_memory(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let _window = crate::ffm_fast::AccessWindow::open();
    crate::phases_late::foreign_ffm::p67_check_scoped_session_arg(ctx, args, 1)?;
    crate::phases_late::foreign_ffm::p67_check_scoped_session_arg(ctx, args, 2)?;
    let adapted = [
        args.first().copied().unwrap_or(Value::Object(None)),
        args.get(3).copied().unwrap_or(Value::Object(None)),
        args.get(4).copied().unwrap_or(Value::Long(0)),
        args.get(5).copied().unwrap_or(Value::Object(None)),
        args.get(6).copied().unwrap_or(Value::Long(0)),
        args.get(7).copied().unwrap_or(Value::Long(0)),
        args.get(8).copied().unwrap_or(Value::Long(0)),
    ];
    unsafe_jdk25::native_unsafe_copy_swap_memory(ctx, &adapted)
}

/// `ScopedMemoryAccess.setMemoryInternal(session, o, offset, bytes, value)`: a
/// reviewed intrinsic (round nio20 wave 3, lane L5). One window over the
/// session check and the fill, which is the `setMemory0` body (`[this, o,
/// offset, bytes, value]`, the session dropped by
/// `scoped_memory_access_unsafe_args` exactly as for the scalar rows).
pub fn native_scoped_memory_set_memory(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let _window = crate::ffm_fast::AccessWindow::open();
    crate::phases_late::foreign_ffm::p67_check_scoped_session_arg(ctx, args, 1)?;
    let adapted = scoped_memory_access_unsafe_args(args);
    unsafe_natives::native_unsafe_set_memory_consolidated(ctx, &adapted)
}

/// `ScopedMemoryAccess.vectorizedMismatchInternal(aSession, bSession, a,
/// aOffset, b, bOffset, length, log2ArrayIndexScale)`: a reviewed intrinsic
/// (round nio20 wave 3, lane L5). One window over both session checks and the
/// comparison, which is the `ArraysSupport.vectorizedMismatch` intrinsic the
/// bytecode body calls (a STATIC method: `[a, aOffset, b, bOffset, length,
/// log2ArrayIndexScale]`, no receiver), so the answer -- an element index, or
/// the JDK's `~remaining` tail encoding -- is the one the bytecode route gives.
pub fn native_scoped_memory_vectorized_mismatch(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let _window = crate::ffm_fast::AccessWindow::open();
    crate::phases_late::foreign_ffm::p67_check_scoped_session_arg(ctx, args, 1)?;
    crate::phases_late::foreign_ffm::p67_check_scoped_session_arg(ctx, args, 2)?;
    let adapted = [
        args.get(3).copied().unwrap_or(Value::Object(None)),
        args.get(4).copied().unwrap_or(Value::Long(0)),
        args.get(5).copied().unwrap_or(Value::Object(None)),
        args.get(6).copied().unwrap_or(Value::Long(0)),
        args.get(7).copied().unwrap_or(Value::Int(0)),
        args.get(8).copied().unwrap_or(Value::Int(0)),
    ];
    crate::phases_early::native_arrays_support_vectorized_mismatch(ctx, &adapted)
}

pub fn asm_u16(b: &[u8], big_endian: bool) -> i64 {
    let v = if big_endian {
        u16::from_be_bytes([b[0], b[1]])
    } else {
        u16::from_le_bytes([b[0], b[1]])
    };
    v as i64
}
pub fn asm_i16(b: &[u8], big_endian: bool) -> i64 {
    let v = if big_endian {
        i16::from_be_bytes([b[0], b[1]])
    } else {
        i16::from_le_bytes([b[0], b[1]])
    };
    v as i64
}
pub fn asm_i32(b: &[u8], big_endian: bool) -> i64 {
    let v = if big_endian {
        i32::from_be_bytes([b[0], b[1], b[2], b[3]])
    } else {
        i32::from_le_bytes([b[0], b[1], b[2], b[3]])
    };
    v as i64
}

pub fn try_yield_virtual_park(
    ctx: &mut dyn NativeContext,
    timeout: Option<std::time::Duration>,
) -> Option<MethodCallResult> {
    if !ctx.is_current_virtual() || ctx.vt_pin_count() != 0 {
        return None;
    }
    let duration = timeout.unwrap_or(std::time::Duration::ZERO);
    if !ctx.vt_park_for(duration) {
        return None;
    }
    Some(Err(MethodCallFailed::InternalError(
        cratonvm_types::error::VmError::ContinuationYield {
            wake_after_nanos: timeout
                .map(|d| d.as_nanos().min(u64::MAX as u128) as u64)
                .unwrap_or(0),
        },
    )))
}

/// Maximum CAS retry iterations before yielding to prevent livelock.
pub const CAS_MAX_RETRIES: usize = 1024;

/// Byte width of one element of a primitive array.
pub fn array_elem_byte_width(t: cratonvm_types::ArrayElementType) -> usize {
    match t {
        cratonvm_types::ArrayElementType::Boolean | cratonvm_types::ArrayElementType::Byte => 1,
        cratonvm_types::ArrayElementType::Char | cratonvm_types::ArrayElementType::Short => 2,
        cratonvm_types::ArrayElementType::Int | cratonvm_types::ArrayElementType::Float => 4,
        cratonvm_types::ArrayElementType::Long
        | cratonvm_types::ArrayElementType::Double
        | cratonvm_types::ArrayElementType::Reference => 8,
    }
}

/// Upper bound on a single `Unsafe.copyMemory` / `Unsafe.setMemory` request.
/// Attacker-controlled `bytes` would otherwise drive an unbounded per-slot
/// loop (`ctx.set_field`) for non-byte-exact targets. Mirrors the 256 MiB cap
/// that `panama.rs` applies to `MemorySegment` copy/fill (finding M3).
pub const MAX_UNSAFE_COPY_SIZE: usize = 256 * 1024 * 1024; // 256 MiB
/// `CRATONVM_BOOT_LOOKUP_EXACT` -- parity, default ON, `=0` off (interpreter
/// round i7 wave 138, lane L5; page
/// `zs4-rp-boot-forname-of-sun-reflect-misc-trampoline-throws-cnfe`): the
/// bootstrap-namespace doors (`ClassLoader.findBootstrapClass`,
/// `JavaLangAccess.findBootstrapClassOrNull`) answer the class the BOOTSTRAP
/// loader defined under a name (the context's `bootstrap_class_id_exact`)
/// and never another loader's same-named class.
/// Read once per process (a switch, not compatibility state).
pub fn boot_lookup_exact_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| true)
}

/// `CRATONVM_BOOT_SEARCH_BOOT_MODULES_ONLY` -- parity, default ON since the i7 triage (WARNING: i7-default-flip; `=0` off; was opt-in:
/// this VM's built-in loaders may still be served through the boot search),
/// interpreter round i7 wave 139, lane L5; page
/// `i138-L5-boot-search-answers-application-and-platform-module-classes`.
/// Read once per process.
pub fn boot_search_boot_modules_only_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| true)
}

/// Whether the boot search (`findBootstrapClass`,
/// `JavaLangAccess.findBootstrapClassOrNull`) must answer `null` for
/// `internal` as HotSpot's does: an image class of a PLATFORM or APPLICATION
/// module (`java/sql/Date`; this VM tags every image class with the boot
/// loader's id), or a name the image does not define, in both modes. A class
/// appended to the boot search, or one defined into the boot loader at run
/// time (`bootstrap_class_id_exact` holds it and the image does not), is the
/// boot loader's. `false` with the switch off.
pub fn boot_search_refuses(ctx: &mut dyn NativeContext, internal: &str) -> bool {
    if !boot_search_boot_modules_only_enabled()
        || internal.starts_with('[')
        || ctx.is_bootstrap_appended_class(internal)
    {
        return false;
    }
    if crate::classloader::image_class_outside_boot_modules(ctx, internal) {
        return true;
    }
    ctx.runtime_image_defines_class(internal) == Some(false)
        && ctx.bootstrap_class_id_exact(internal).is_none()
}

/// ClassLoader.findBootstrapClass — find a class in the bootstrap class path.
pub fn native_classloader_find_bootstrap_class(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // W7-86. `findBootstrapClass` is **`private static native`** on JDK 25's
    // `ClassLoader` (`javap -p --module java.base java.lang.ClassLoader`,
    // Adoptium 25.0.3.9), so `args[0]` IS the name and there is no receiver
    // slot. The comment this replaces described its neighbour
    // `findLoadedClass0`, which really is `private final native` — an INSTANCE
    // method — and whose `args[1]` indexing below is correct. Copying it here
    // made this native read one past the end of a one-element `args`, take the
    // `_` arm and answer **null for every name**, on every call, since it was
    // written. The registration owns its slot and the real method is
    // `acc_native: true, has_code: false` (`--dump-native-registry`), so no
    // bytecode was answering behind it.
    //
    // Mode: Compatible (`--fast-boot`, the default). This is a HotSpot-parity
    // fix — HotSpot resolves the bootstrap class — but note it wakes a code
    // path that has been inert, so it is committed on its own.
    let name = match args.first() {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Ok(Some(Value::Object(None))),
    };
    // Convert "java.lang.String" → "java/lang/String"
    let internal_name = name.replace('.', "/");
    // A generated dynamic-proxy class is never a bootstrap class; resolving one
    // here would leak a user loader's proxy into the bootstrap/parent-delegation
    // path (ClassUtilsTests.isCacheSafe). Bootstrap genuinely has no proxies.
    if crate::classloader::is_generated_proxy_name(&internal_name) {
        return Ok(Some(Value::Object(None)));
    }
    if boot_search_refuses(ctx, &internal_name) {
        return Ok(Some(Value::Object(None)));
    }
    // `--real-jdk`: a name the runtime image does not define, that is not an
    // appended boot class and is not loaded yet, is not a bootstrap class, and
    // LOADING it to find that out defines it in the flat class table: the
    // package of an application class then counts as "defined" for the
    // application loader before the loader's own `defineClass` ran, and the
    // JDK's `defineOrCheckPackage` reports a `Sealed: true` jar's package as
    // "already defined" (`Sbr3RefmiscSealed`). HotSpot's boot search is a
    // lookup, not a load. SBR3 `refmisc`.
    //
    // "Not loaded yet" means not loaded BY THE BOOTSTRAP LOADER: a class of
    // that name a URL or user loader defined is no bootstrap class either, and
    // `load_class` below did not find it -- it defined a SECOND copy from the
    // class path under `Application` (then answered null), which refused the
    // application loader's own later `defineClass` of the name as a duplicate
    // (interpreter round i2 wave 61, lane L5; probe
    // `L5W60UrlParentDelegation`, row `app-grandparent`).
    if ctx.is_jdk_only()
        && ctx.runtime_image_defines_class(&internal_name) == Some(false)
        && !ctx.is_bootstrap_appended_class(&internal_name)
        && ctx
            .class_id_by_name(&internal_name)
            .is_none_or(|cid| ctx.loader_id_of_class(cid) != 0)
    {
        return Ok(Some(Value::Object(None)));
    }
    // Interpreter round i7 wave 138, lane L5: the bootstrap loader's own
    // dictionary first (`bootstrap_class_id_exact`, no delegation, no load).
    // A name the boot loader already holds is that class, whatever other
    // loaders defined under it: `sun/reflect/misc/Trampoline`, which
    // `MethodUtil` also defines, answered `null` (the load below found the
    // user copy and refused it) where HotSpot's boot search answers its own.
    // `CRATONVM_BOOT_LOOKUP_EXACT=0` restores the load-first order.
    if boot_lookup_exact_enabled() {
        if let Some(cid) = ctx
            .bootstrap_class_id_exact(&internal_name)
            .filter(|&cid| !ctx.is_class_hidden(cid))
        {
            return Ok(Some(Value::Object(Some(ctx.get_class_mirror(cid)))));
        }
    }
    // Try to load the class — load_class returns MethodCallResult where
    // Ok(Some(Value::Object(Some(obj)))) contains the class mirror. CratonVM's
    // flat global class store can also contain classes defined by application
    // or user loaders. Those are not bootstrap classes; returning one here
    // lets parent delegation leak a sibling loader's dynamic class by name.
    match ctx.load_class(&internal_name) {
        Ok(Some(Value::Object(Some(mirror)))) => {
            if ctx
                .class_id_from_mirror(mirror)
                .and_then(|cid| {
                    crate::classloader::defining_loader_for(crate::classloader::loader_tables(ctx), cid.as_u32())
                })
                .is_some()
            {
                return Ok(Some(Value::Object(None)));
            }
            // SBR2-LOADERS: under `--real-jdk` the real `BuiltinClassLoader`
            // bytecode (the platform loader's `loadClassOrNull`) reaches here
            // for an application class name and must get null, as HotSpot's
            // boot search answers: a class the application loader defined is
            // not bootstrap (an appended boot jar's class is).
            if ctx.is_jdk_only() {
                if let Some(cid) = ctx.class_id_from_mirror(mirror) {
                    if ctx.loader_id_of_class(cid) >= 2
                        && !ctx.is_bootstrap_appended_class(&internal_name)
                    {
                        return Ok(Some(Value::Object(None)));
                    }
                }
            }
            Ok(Some(Value::Object(Some(mirror))))
        }
        Ok(result) => Ok(result),
        Err(_) => Ok(Some(Value::Object(None))),
    }
}

/// ClassLoader.findLoadedClass0 — check if a class is already loaded.
pub fn native_classloader_find_loaded_class(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0]=this ClassLoader, args[1]=name String
    let name = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Ok(Some(Value::Object(None))),
    };
    // `--real-jdk`: HotSpot's native answers null for a name with a '/' or a
    // leading '[' (`ClassLoader.c`: `VerifyFixClassname`, `verifyClassname`);
    // the dot-to-slash conversion below would otherwise find `p/X` (wave 42
    // follow-up, lane L5). `--compatible` too with the `FindLoadedNameCheck`
    // row (wave 50, lane L5).
    if ctx.aligned(cratonvm_types::HotSpotAlignment::FindLoadedNameCheck)
        && !crate::classloader_real::find_loaded_class_name_is_legal(&name)
    {
        if let Some(Value::Object(Some(this))) = args.first() {
            crate::classloader_real::note_find_loaded_name_check_divergence(ctx, *this, &name);
        }
        return Ok(Some(Value::Object(None)));
    }
    let internal_name = name.replace('.', "/");

    // JVMS §5.3: `findLoadedClass0(name)` reports a class only if THIS loader
    // loaded it — NOT one some OTHER loader (typically the application loader)
    // happens to have loaded. Loader-scoped for user-defined loaders, global for
    // built-in loaders. (Shared with the public `findLoadedClass` natives so all
    // entry points agree; in real-JDK mode the public `findLoadedClass` native
    // usually shadows this one, but a direct `findLoadedClass0` call must match.)
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Then, `--real-jdk`, a class this loader INITIATED (JVMS §5.3; interpreter
    // round i1 wave 38, lane L5): `classloader_real::initiated_class_mirror`.
    // A class HotSpot never defined (its package's sealing refused it at a
    // door) is not found (`hide_package_refused`, interpreter round i4 wave
    // 95, lane L5).
    // zt df4 §9c: the Java-facing lookup never materialises an image class
    // (`find_loaded_class_for_loader_java`).
    let found = match crate::classloader::find_loaded_class_for_loader_java(ctx, this, &internal_name)
    {
        Some(mirror) => Some(mirror),
        None => crate::classloader_real::initiated_class_mirror(ctx, this, &internal_name),
    };
    // zt t22/df10: a built-in loader answers an ancestor's class only when it
    // initiated it (`classloader::find_loaded_answer_for_java`).
    let found = crate::classloader::find_loaded_answer_for_java(ctx, this, &internal_name, found);
    Ok(Some(Value::Object(
        crate::classloader_real::hide_package_refused(ctx, found),
    )))
}

/// Reference.refersTo0 — check if a Reference's referent is the given object.
/// `Reference.clear0()` (and the `PhantomReference` redeclaration of it) —
/// the JDK-25 native that `Reference.clear()` delegates to. It must null the
/// `referent` slot; leaving it set kept a cleared Weak/Soft/Phantom reference
/// reporting a live referent forever, and — because the GC's reference
/// processor treats a non-null referent as a discovered edge — kept the
/// referent itself alive, defeating every explicit `clear()`-based cache
/// eviction.
///
/// Mirror image of `native_reference_refers_to` below, but resolving the slot
/// the way `reference::ref_init_impl` writes it: real-JDK `Reference` declares
/// `referent` at an order we must not assume, while the two-slot synthetic
/// shape keeps it at `REF_FIELD_REFERENT`.
pub fn native_reference_clear0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(None);
    };
    let class_id = ctx.class_id_of_object(this);
    if ctx
        .resolve_field_index_by_class_id(class_id, "referent")
        .is_some()
    {
        ctx.set_field_by_name(this, "referent", Value::Object(None));
    } else if ctx.object_num_fields(this) > REF_FIELD_REFERENT {
        ctx.set_field(this, REF_FIELD_REFERENT, Value::Object(None));
    }
    // gc-common w2-f: settle the GC registry's row too, so a cleared reference
    // is never enqueued by the collector. See `retire_cleared_reference`.
    reference::retire_cleared_reference(ctx, this);
    Ok(None)
}

/// `Reference.clear0()` with `CRATONVM_REFERENCE_LAYOUT_MEMO` (interpreter
/// round i3 wave 77, lane L5): the same clear and the same GC-registry
/// retirement as [`native_reference_clear0`], on `Reference`'s own slots
/// resolved once per VM (`reference::ReferenceLayout`) instead of by name on
/// the receiver's class per call. A VM without that layout (the synthetic
/// two-slot shape) or a receiver that does not fit it runs
/// [`native_reference_clear0`].
pub fn native_reference_clear0_layout(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(None);
    };
    if reference::clear0_on_layout(ctx, this) {
        return Ok(None);
    }
    native_reference_clear0(ctx, args)
}

/// The `clear0()` body this registry installs: the memoized-layout one with
/// `CRATONVM_REFERENCE_LAYOUT_MEMO`, the by-name one otherwise (decided once,
/// at registration, so the switch costs the default path nothing).
pub fn reference_clear0_callback(registry: &NativeMethodRegistry) -> NativeCallback {
    if registry.reference_layout_memo() {
        native_reference_clear0_layout as NativeCallback
    } else {
        native_reference_clear0 as NativeCallback
    }
}

/// Read a String-valued field by name, or `None` when the field is absent or
/// not a String. Helper for [`native_init_stack_trace_element`].
pub fn ste_field_string(ctx: &dyn NativeContext, obj: ObjectRef, name: &str) -> Option<String> {
    match ctx.get_field_by_name(obj, name) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

/// Same, by slot index — covers the four-slot synthetic `StackTraceElement`
/// shape, which has no named fields at all.
pub fn ste_slot_string(ctx: &dyn NativeContext, obj: ObjectRef, idx: usize) -> Option<String> {
    if ctx.object_num_fields(obj) <= idx {
        return None;
    }
    match ctx.get_field(obj, idx) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

/// `StackTraceElement.initStackTraceElement(StackTraceElement, StackFrameInfo)`
/// — the static native behind `StackFrameInfo.toStackTraceElement()`, i.e.
/// every `StackWalker`-derived element. It must populate declaringClass /
/// methodName / fileName / lineNumber on the freshly allocated, all-null
/// element it is handed; as a no-op the caller got a blank element back and
/// rendered it as `null.null(Unknown Source)`.
///
/// The frame itself only carries class + method + bci, so prefer the fully
/// populated element `lang_stackwalker::populate_sfi` already cached in the
/// frame's `ste` slot — that is where the source file and line survive. Fall
/// back to reading the frame directly (`className`/`name`) for frames built by
/// some other path.
pub fn native_init_stack_trace_element(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Static native: args[0] is the element to fill, args[1] the frame.
    let Some(Value::Object(Some(ste))) = args.first().copied() else {
        return Ok(None);
    };
    let Some(Value::Object(Some(sfi))) = args.get(1).copied() else {
        return Ok(None);
    };
    // Interpreter round i2 wave 54, lane L3: a frame whose element the walk
    // left to this first read (`CRATONVM_SFI_LAZY_ELEMENT`) is built from the
    // walk's element, settled against a redefinition of its class since the
    // walk -- HotSpot's `fill_in` at this same call
    // (`lang_stackwalker::fill_element_from_walk`; probe
    // `L3W40WalkedFrameReadAfterRedefinition`).
    if crate::lang_stackwalker::fill_element_from_walk(ctx, ste, sfi).is_some() {
        return Ok(None);
    }
    let source = match ctx.get_field_by_name(sfi, "ste") {
        Value::Object(Some(cached)) if cached != ste => cached,
        _ => sfi,
    };

    let class_dotted = ste_field_string(&*ctx, source, "declaringClass")
        .or_else(|| ste_field_string(&*ctx, source, "className"))
        .or_else(|| ste_slot_string(&*ctx, source, 0));
    let method = ste_field_string(&*ctx, source, "methodName")
        .or_else(|| ste_field_string(&*ctx, source, "name"))
        .or_else(|| ste_slot_string(&*ctx, source, 1));
    let file =
        ste_field_string(&*ctx, source, "fileName").or_else(|| ste_slot_string(&*ctx, source, 2));
    let line = match ctx.get_field_by_name(source, "lineNumber") {
        Value::Int(l) => l,
        _ => match ctx.get_field(source, 3) {
            Value::Int(l) => l,
            _ => -1,
        },
    };

    // Nothing identifiable on the frame — leave the element as the caller
    // allocated it rather than writing an empty class name over it.
    let Some(class_dotted) = class_dotted else {
        return Ok(None);
    };
    let method = method.unwrap_or_default();
    let class_slashed = class_dotted.replace('.', "/");
    crate::lang_misc::fill_stack_trace_element(
        ctx,
        ste,
        &class_slashed,
        &class_dotted,
        &method,
        file.as_deref(),
        line,
    );
    Ok(None)
}

/// `CRATONVM_REFERS_TO_LEAF` (default OFF): register the `Reference.refersTo`
/// family as leaf natives (one field read and a compare, no funnel). Read at
/// registration time.
pub fn refers_to_leaf_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_REFERS_TO_LEAF")
}

pub fn native_reference_refers_to(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args[0]=this Reference, args[1]=Object to compare
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    // Reference.referent is typically field 0. zt t23/zb8: a non-keep-alive
    // read (HotSpot's `refersTo0` is `AS_NO_KEEPALIVE`); only compared.
    let referent = ctx.get_field_no_keepalive(this, 0);
    let target = args.get(1).cloned().unwrap_or(Value::Object(None));
    let result = match (&referent, &target) {
        (Value::Object(Some(a)), Value::Object(Some(b))) => a.as_ptr() == b.as_ptr(),
        (Value::Object(None), Value::Object(None)) => true,
        _ => false,
    };
    if !result && dbg_refers_to() {
        if let (Value::Object(Some(a)), Value::Object(Some(b))) = (&referent, &target) {
            eprintln!(
                "[refersto] FALSE(refersTo0) this={:#x} referent={:#x} other={:#x}",
                this.as_ptr() as usize,
                a.as_ptr() as usize,
                b.as_ptr() as usize,
            );
        }
    }
    Ok(Some(Value::Int(result as i32)))
}

/// Reference.waitForReferencePendingList — block until pending references available.
/// Keep the handler in a VM-internal wait until GC publishes pending work.
pub fn native_reference_wait_pending(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // The collector roots pending References without touching Java queue links.
    // This genuine handler drains their discovered chain and takes each queue's
    // lock in Java. Keep the existing blocked-region discipline and 50 ms idle
    // tick; pending work is checked as well as the collection epoch, covering
    // a batch published after the wait sampled that collection's new epoch.
    //
    // Interpreter round i2 wave 66, lane L1 (`CRATONVM_VM_INTERNAL_WAITS_RUNNABLE`):
    // HotSpot's `JVM_WaitForReferencePendingList` waits on `Heap_lock` inside
    // the VM, so the `Reference Handler` thread reads RUNNABLE (its thread
    // dump: `java.lang.Thread.State: RUNNABLE` at
    // `Reference.waitForReferencePendingList(Native Method)`), not WAITING.
    //
    // Interpreter round i4 wave 98, lane L1
    // (`interpreter-i98-L1-reference-handler-reads-in-native-between-ticks-FIXED-20261130.md`):
    // the wait returns when HotSpot's does, after a collection, not on a
    // 100 ms clock. Each return runs the region's leave and entry deposits,
    // `processPendingReferences`' two `synchronized` blocks and the
    // `getAndClearReferencePendingList` call outside the region, with the
    // native-call funnel's excuse word raised, so a JMX `ThreadInfo` sampled
    // then reads `isInNative()` true. HotSpot's handler leaves
    // `JVM_WaitForReferencePendingList` only when a collection discovered
    // references, and then passes through its native wrapper
    // (`_thread_in_native`) the same way: an idle VM samples false
    // (`L1W66UnrecordedWaitKinds`), an allocating one sees true at some
    // point (`L1W98ReferenceHandlerSampled`, HotSpot measured on w104). The
    // first wave-98 version never returned at all and read false for both.
    reference_wait_pending_ticks(ctx, None);
    Ok(None)
}

/// The body of `Reference.waitForReferencePendingList`: one VM-internal wait
/// region, sleeping in 50 ms ticks until a collection completes after the
/// entry (`NativeContext::gc_collection_count`, an atomic read, legal inside
/// the region), or after `ticks` ticks (`None`: no bound, the native).
pub fn reference_wait_pending_ticks(ctx: &mut dyn NativeContext, ticks: Option<u32>) {
    let collections = ctx.gc_collection_count();
    begin_vm_internal_wait(ctx);
    let mut left = ticks;
    loop {
        std::thread::sleep(std::time::Duration::from_millis(50));
        if ctx.has_reference_pending_list() || ctx.gc_collection_count() != collections {
            break;
        }
        if let Some(n) = left {
            if n <= 1 {
                break;
            }
            left = Some(n - 1);
        }
    }
    ctx.end_blocking_region();
}

/// Open the blocking region of a wait HotSpot performs inside the VM, not in
/// a Java `wait` / `park` (interpreter round i2 wave 66, lane L1,
/// `CRATONVM_VM_INTERNAL_WAITS_RUNNABLE`, default on;
/// `docs/feature-designs/native-dispatch-architecture.md`):
/// `NativeContext::begin_vm_wait_region`, which reads RUNNABLE as HotSpot's
/// `_thread_blocked` inside a JVM entry does, and records no wait kind
/// because it is no Java wait. `=0`: the plain WAITING region it was. Paired
/// with exactly one `end_blocking_region` / `end_blocking_region_refs`.
#[track_caller]
pub fn begin_vm_internal_wait(ctx: &mut dyn NativeContext) {
    if true {
        ctx.begin_vm_wait_region();    } else {
        ctx.begin_blocking_region();    }}


/// MethodHandle.invoke / invokeExact / invokeBasic — signature-polymorphic dispatch.
/// This is a fallback for when the interpreter's specialized MH dispatch doesn't apply.
/// The real dispatch happens in the interpreter via CachedInvokeTarget; this native
/// fallback returns null for now (MH-heavy code paths use the interpreter's dispatch).
pub fn native_method_handle_invoke(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // A real-JDK Linker returns a downcall handle. Some signature-polymorphic
    // void call sites resolve the inherited MethodHandle entry first and reach
    // this fallback instead of the specialized interpreter route. Do not turn
    // those native calls into a silent null return.
    //
    // Both former tests are dead as written (P1-E). The class test named
    // `java/lang/foreign/DowncallHandle`, the invented class `--real-jdk`
    // refused; and the layout test read `Long` at slot 0 and a
    // `FunctionDescriptor` at slot 1, which the carrier no longer has — slot 0
    // is now the real `type` field (an object) and the downcall state lives
    // above the MethodHandle window. Left as they were, BOTH would go false and
    // this fallback would return null for every downcall. That is the failure
    // mode this comment block exists to prevent, so the two halves had to move
    // together.
    if let Some(Value::Object(Some(receiver))) = args.first() {
        if crate::panama::is_downcall_handle(ctx, *receiver) {
            return crate::panama::pe_downcall_invoke(ctx, args);
        }
    }
    // A CratonVM synthetic handle: dispatch it as `invoke` does with no call
    // site (interpreter round i1 wave 43, lane L4; see
    // `lang_invoke::mh_invoke_basic` for the JDK caller that got `null`).
    if let Some(result) = crate::lang_invoke::mh_invoke_basic(ctx, args) {
        return result;
    }
    // Signature-polymorphic methods are typically handled by the interpreter directly.
    // If we reach here, it means the call wasn't intercepted. Return null.
    Ok(Some(Value::Object(None)))
}

/// Raise the `linkTo*` "cannot dispatch" failure. Never return a constant
/// `null` from a linker intrinsic: a `linkTo*` call IS a method call, so a
/// fabricated `null` return is a wrong answer for a call that never happened
/// and surfaces as an NPE/CCE arbitrarily far from the cause.
pub fn link_to_unsupported(detail: &str) -> MethodCallResult {
    Err(MethodCallFailed::from(
        RuntimeError::UnsupportedOperationException {
            message: format!("MethodHandle.linkTo*: {detail}"),
        },
    ))
}

/// JVM descriptor for one `Class` mirror, e.g. `int` -> `I`,
/// `java/lang/String` -> `Ljava/lang/String;`, `[I` -> `[I`.
///
/// `lang_invoke` has the same logic in its private `mirror_to_descriptor` /
/// `class_name_to_descriptor` pair; this is the callable-from-here version.
/// It differs deliberately in one place: `mirror_to_descriptor` falls back to
/// `Ljava/lang/Object;` for an unresolvable mirror, which is fine for its
/// display/adaptation callers but would silently manufacture a wrong
/// descriptor for a dispatch. Here an unresolvable mirror is a hard `None`.
pub fn link_to_type_descriptor(ctx: &dyn NativeContext, mirror: ObjectRef) -> Option<String> {
    let Some(name) = crate::lang_invoke::resolve_class_name_robust(ctx, mirror) else {
        // A lambda proxy's mirror names no class: its own name, not "unrenderable"
        // (interpreter round i4 wave 92, lane L4; see
        // `lang_invoke::unnameable_mirror_descriptor`).
        let own = crate::lang_invoke::unnameable_mirror_descriptor(ctx, mirror);
        crate::mh_finder_census::event(if own.is_some() {
            "type-render.link.proxy-own-name"
        } else {
            "type-render.link.none"
        });
        return own;
    };
    if name.starts_with('[') {
        // Array mirrors already carry descriptor form.
        return Some(name);
    }
    Some(match name.as_str() {
        "void" => "V".to_string(),
        "int" => "I".to_string(),
        "long" => "J".to_string(),
        "float" => "F".to_string(),
        "double" => "D".to_string(),
        "boolean" => "Z".to_string(),
        "byte" => "B".to_string(),
        "char" => "C".to_string(),
        "short" => "S".to_string(),
        _ => format!("L{name};"),
    })
}

/// Method descriptor for a `MethodType`, covering both shapes CratonVM can
/// hand us: the real JDK object (named `rtype`/`ptypes` fields) and the
/// synthetic 2-field stand-in `lang_invoke` mints (returnType=0,
/// paramTypes=1). Returns `None` rather than guessing if any component
/// cannot be resolved.
pub fn link_to_method_descriptor(ctx: &dyn NativeContext, mt: ObjectRef) -> Option<String> {
    let ptypes = match ctx.get_field_by_name(mt, "ptypes") {
        Value::Object(Some(a)) => a,
        _ => match ctx.get_field(mt, 1) {
            Value::Object(Some(a)) => a,
            _ => return None,
        },
    };
    let mut s = String::from("(");
    let n = ctx.array_length(ptypes);
    for i in 0..n {
        match ctx.get_array_element(ptypes, i) {
            Value::Object(Some(p)) => s.push_str(&link_to_type_descriptor(ctx, p)?),
            _ => return None,
        }
    }
    s.push(')');
    let rtype = match ctx.get_field_by_name(mt, "rtype") {
        Value::Object(Some(r)) => Some(r),
        _ => match ctx.get_field(mt, 0) {
            Value::Object(Some(r)) => Some(r),
            _ => None,
        },
    };
    match rtype {
        Some(r) => s.push_str(&link_to_type_descriptor(ctx, r)?),
        None => s.push('V'),
    }
    Some(s)
}

/// Number of parameters in a method descriptor. One per declared parameter —
/// long/double are NOT double-counted, because CratonVM passes one `Value`
/// per argument regardless of category. `None` on a malformed descriptor.
pub fn descriptor_param_count(desc: &str) -> Option<usize> {
    let b = desc.as_bytes();
    if b.first() != Some(&b'(') {
        return None;
    }
    let mut i = 1usize;
    let mut n = 0usize;
    while i < b.len() && b[i] != b')' {
        while i < b.len() && b[i] == b'[' {
            i += 1;
        }
        if i >= b.len() {
            return None;
        }
        if b[i] == b'L' {
            while i < b.len() && b[i] != b';' {
                i += 1;
            }
            if i >= b.len() {
                return None;
            }
        }
        i += 1;
        n += 1;
    }
    if i >= b.len() {
        return None;
    }
    Some(n)
}

/// `MethodHandle.linkTo{Static,Virtual,Interface,Special,Native}` — HotSpot's
/// member-name linker intrinsics.
///
/// **Calling convention.** All five are declared `static native
/// @PolymorphicSignature Object linkToX(Object... args)`, so the VM does NOT
/// prepend a receiver: `args` is exactly the operand-stack argument list.
/// HotSpot's ABI is *callee arguments, then a trailing appendix* — the
/// `MemberName` naming the target (a `NativeEntryPoint` for `linkToNative`).
/// For the receiver-taking kinds the receiver is simply the FIRST callee
/// argument, already in place at `args[0]`.
///
/// **Why the layout is validated rather than assumed.** These five are
/// registered under the erased *declared* descriptor
/// `([Ljava/lang/Object;)Ljava/lang/Object;`, while a real signature-
/// polymorphic call site carries a concrete descriptor. The VM's
/// polymorphic-descriptor fallback (`vm/src/vm/vm_exec.rs`'s
/// signature-polymorphic block; `try_stackless_invoke`'s erased arm) takes
/// these five by name since interpreter round i2 wave 55, lane L4
/// (`vm_exec::is_method_handle_linker_intrinsic`,
/// `CRATONVM_MH_LINKTO_FROM_BYTECODE`): a JDK `LambdaForm` linker's
/// `invokestatic MethodHandle.linkToStatic(ILjava/lang/invoke/MemberName;)I`
/// arrives here with the SPREAD list. Before that the only way in from
/// bytecode was a call site literally typed
/// `([Ljava/lang/Object;)Ljava/lang/Object;`, whose single argument is a
/// packed `Object[]`. The natives' own callers (`mh_run_foreign_vmentry`,
/// `mh_real_direct_dispatch`) pass the spread list too. Rather than bet on one
/// layout, this decodes defensively and REJECTS anything that does not fit:
///
///   * the trailing argument must be a `java/lang/invoke/MemberName`
///     (a packed `Object[]`, a `NativeEntryPoint`, or an empty list is not),
///   * the MemberName must yield a declaring class, a name and a
///     `MethodType`-derived descriptor,
///   * and the number of remaining arguments must equal the descriptor's
///     parameter count (+1 for the receiver on the virtual/interface/special
///     kinds).
///
/// A shifted argument list therefore throws instead of invoking the target
/// with mis-aligned arguments, which would be worse than the old null.
///
/// **MemberName layout** (mirrors `lang_invoke::native_mhn_resolve` and the
/// `MemberName.getDeclaringClass`/`getName`/`getMethodType`/
/// `getReferenceKind` natives): slot 0 `clazz`, 1 `name`, 2 `type`,
/// 3 `flags`, with the reference kind in bits 24-27 of `flags`.
///
/// **GC safety**: every decode step below goes through `&self` accessors
/// (`get_field`, `get_field_by_name`, `array_length`, `get_array_element`,
/// `read_string`, `resolve_class_name_robust`), none of which can allocate
/// or reach a safepoint, so neither `member` nor the incoming `args` can
/// move mid-decode. The single `ctx.invoke*` call happens after the decode
/// is complete, with no live unpinned reference derived from it.
/// What `linkTo*` decodes from its trailing `MemberName` appendix.
pub struct LinkToRecord {
    pub ref_kind: i32,
    pub class: String,
    /// The declaring class (`MemberName.clazz`), for the settled static road.
    pub owner: Option<ClassId>,
    pub name: String,
    pub desc: String,
    pub param_count: usize,
    /// The declaring mirror names no class but a lambda proxy (interpreter round i2 wave 61,
    /// lane L4): its members run on the receiver.
    pub is_proxy: bool,
    /// As `lang_invoke::RealDirectTarget::settled`.
    pub settled: std::cell::Cell<Option<ClassId>>,
    /// JIT round 20 wave 19, lane pRefl (`CRATONVM_JIT_MH_COMPILED_LINK`): the
    /// address of the call info the VM interned for this member's compiled
    /// call ([`MhCompiledMember::leaf_word`]), `1` once refused, `0` before.
    pub jit_leaf: std::cell::Cell<usize>,
    /// Interpreter round i6 wave 116, lane L2 (`CRATONVM_JIT_MH_COMPILED_LINK_BY_ID`):
    /// the VM's by-class-id verdict for this member ([`MhCompiledMember::by_id_word`]):
    /// `0` unknown, `1` refused (the method declares an exception table), else
    /// the address of its interned call info. Kept apart from `jit_leaf`, whose
    /// infos the by-NAME dispatch may resolve.
    pub jit_by_id: std::cell::Cell<usize>,

    /// JIT round 21 lane reflect3 (`CRATONVM_JIT_MH_COMPILED_LINK_AGREE_MEMO`):
    /// the last basic-type agreement verdict a compiled link computed against
    /// this member ([`MhCompiledMember::agree_memo`]): the site text, the
    /// layout key, the verdict. JIT round 21 lane reflect4 (correctness of the
    /// opt-in memo, unconditional): keyed by the text's CONTENT, not its
    /// address and length -- a compiled site's descriptor lives in its
    /// `CompiledMethod`'s owned strings, freed with the body, so another
    /// site's different text of the same length could reuse the address and
    /// be answered this verdict.
    pub jit_agree: std::cell::RefCell<Option<(Box<str>, u8, bool)>>,
}

/// 2-way set-associative per-thread address memo with a splitmix64 set index.
/// Keyed by `(vm, gc_address_epoch, addr)` with `vm == 0` refused.
pub struct AddrMemoLine<T> {
    pub vm: usize,
    pub epoch: u64,
    pub addr: usize,
    pub record: Option<std::rc::Rc<T>>,
}

impl<T> AddrMemoLine<T> {
    pub fn empty() -> Self {
        Self { vm: 0, epoch: 0, addr: 0, record: None }
    }

    pub fn matches(&self, vm: usize, epoch: u64, addr: usize) -> bool {
        self.vm == vm && self.epoch == epoch && self.addr == addr
    }
}

pub struct AddrMemo<T> {
    pub sets: Vec<[AddrMemoLine<T>; 2]>,
    pub mask: usize,
}

impl<T> AddrMemo<T> {
    pub fn new(num_sets: usize) -> Self {
        assert!(num_sets.is_power_of_two() && num_sets > 0);
        let sets = (0..num_sets)
            .map(|_| [AddrMemoLine::empty(), AddrMemoLine::empty()])
            .collect();
        Self { sets, mask: num_sets - 1 }
    }

    #[inline]
    pub fn set_index(&self, addr: usize) -> usize {
        let mut x = addr as u64;
        x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
        x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        x ^= x >> 31;
        (x as usize) & self.mask
    }

    #[inline]
    pub fn get(&mut self, vm: usize, epoch: u64, addr: usize) -> Option<std::rc::Rc<T>> {
        if vm == 0 {
            return None;
        }
        let idx = self.set_index(addr);
        let set = &mut self.sets[idx];
        if set[0].matches(vm, epoch, addr) {
            return set[0].record.clone();
        }
        if set[1].matches(vm, epoch, addr) {
            set.swap(0, 1);
            return set[0].record.clone();
        }
        None
    }

    #[inline]
    pub fn insert(&mut self, vm: usize, epoch: u64, addr: usize, record: std::rc::Rc<T>) {
        if vm == 0 {
            return;
        }
        let idx = self.set_index(addr);
        let set = &mut self.sets[idx];
        let line = AddrMemoLine { vm, epoch, addr, record: Some(record) };
        if set[0].matches(vm, epoch, addr) {
            set[0] = line;
        } else {
            set.swap(0, 1);
            set[0] = line;
        }
    }
}

pub const LINK_TO_MEMO_SETS: usize = 64;

thread_local! {
    /// Per-thread memo of [`LinkToRecord`]s keyed by the `MemberName`'s address, valid for one
    /// collection epoch under one VM (the argument of `lang_invoke::REAL_HANDLE_MEMO`: nothing
    /// moves or is freed within an epoch, and the fields read are final once resolved). A
    /// context that reports VM identity `0` (the mocks) is never memoised.
    pub static LINK_TO_MEMO: std::cell::RefCell<AddrMemo<LinkToRecord>> =
        std::cell::RefCell::new(AddrMemo::new(LINK_TO_MEMO_SETS));
}

/// The [`LinkToRecord`] this thread's memo already holds for `member` in this
/// collection epoch, or `None` (a miss, or a context with VM identity `0`).
/// Pure reads; [`link_to_record`]'s own first step (JIT round 21 lane
/// reflect2).
pub fn link_to_record_memo_hit(
    ctx: &dyn NativeContext,
    member: ObjectRef,
) -> Option<std::rc::Rc<LinkToRecord>> {
    let vm = ctx.vm_identity();
    if vm == 0 {
        return None;
    }
    let epoch = ctx.gc_address_epoch();
    let addr = member.as_ptr() as usize;
    LINK_TO_MEMO.with(|memo| memo.borrow_mut().get(vm, epoch, addr))
}

/// The decoded `MemberName` `member` (a class, a name, a descriptor, the reference kind),
/// from the per-epoch memo or decoded now; `Err` is the refusal answer of the decode step.
pub fn link_to_record(
    ctx: &mut dyn NativeContext,
    member: ObjectRef,
) -> Result<std::rc::Rc<LinkToRecord>, MethodCallResult> {
    let vm = ctx.vm_identity();
    let epoch = ctx.gc_address_epoch();
    let addr = member.as_ptr() as usize;
    if let Some(record) = LINK_TO_MEMO.with(|memo| memo.borrow_mut().get(vm, epoch, addr)) {
        return Ok(record);
    }
    let Value::Object(Some(clazz_mirror)) = ctx.get_field(member, 0) else {
        return Err(link_to_unsupported("MemberName has no declaring class"));
    };
    // A lambda proxy's mirror names no class: its member runs on the receiver.
    let resolved = crate::lang_invoke::resolve_class_name_robust(ctx, clazz_mirror);
    let proxy_name = match resolved {
        Some(_) => None,
        None => crate::lang_invoke::lk_lambda_proxy_of_mirror(&*ctx, clazz_mirror).map(|(_, n)| n),
    };
    let is_proxy = proxy_name.is_some();
    let Some(class) = resolved.or(proxy_name) else {
        return Err(link_to_unsupported(
            "MemberName's declaring-class mirror did not resolve",
        ));
    };
    let name = match ctx.get_field(member, 1) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    if name.is_empty() {
        return Err(link_to_unsupported("MemberName has no method name"));
    }
    let Value::Object(Some(method_type)) = ctx.get_field(member, 2) else {
        return Err(link_to_unsupported("MemberName has no MethodType"));
    };
    let Some(desc) = link_to_method_descriptor(ctx, method_type) else {
        return Err(link_to_unsupported(
            "MemberName's MethodType did not yield a descriptor",
        ));
    };
    let flags = match ctx.get_field(member, 3) {
        Value::Int(f) => f,
        _ => 0,
    };
    let Some(param_count) = descriptor_param_count(&desc) else {
        return Err(link_to_unsupported(
            "MemberName's MethodType yielded a malformed descriptor",
        ));
    };
    let owner = ctx.class_id_from_mirror(clazz_mirror);
    let record = std::rc::Rc::new(LinkToRecord {
        ref_kind: (flags >> 24) & 0x0F,
        class,
        owner,
        name,
        desc,
        param_count,
        is_proxy,
        settled: std::cell::Cell::new(None),
        jit_leaf: std::cell::Cell::new(0),
        jit_by_id: std::cell::Cell::new(0),

        jit_agree: std::cell::RefCell::new(None),
    });
    // An unresolved `MemberName` (no reference kind yet) can still change: not filed.
    if vm != 0 && (5..=9).contains(&record.ref_kind) {
        LINK_TO_MEMO.with(|memo| memo.borrow_mut().insert(vm, epoch, addr, record.clone()));
    }
    Ok(record)
}

/// JIT round 20 wave 19, lane pRefl (`CRATONVM_JIT_MH_COMPILED_LINK`, default
/// ON, `=0` off): a decoded `MemberName` that a compiled `invokeBasic` /
/// `linkTo*` / linked `invokeExact` site calls as an ordinary compiled call
/// (`vm/src/jit/helpers.rs` `mh_compiled_link_redirect`) instead of crossing
/// this crate's door. It is [`link_to_record`]'s memoised record (same key:
/// the VM, the address epoch and the member's address), so the decode is the
/// one `linkTo*` itself makes, and it carries the VM's interned call info for
/// the member once the VM filed one, so a warm site builds no string.
pub struct MhCompiledMember {
    pub record: std::rc::Rc<LinkToRecord>,
}

impl MhCompiledMember {
    /// JVMS 5.4.3.5 reference kind (6 static, 5 virtual, 9 interface, ...).
    pub fn ref_kind(&self) -> i32 {
        self.record.ref_kind
    }

    /// Declared parameters, without the receiver.
    pub fn param_count(&self) -> usize {
        self.record.param_count
    }

    /// The member's method descriptor.
    pub fn desc(&self) -> &str {
        &self.record.desc
    }

    /// The address of the VM's interned call info for this member, `1` when
    /// the VM refused it ([`Self::bind_checked`] answered `None`), `0` before
    /// either. Valid for the record's epoch.
    pub fn leaf_word(&self) -> usize {
        self.record.jit_leaf.get()
    }

    /// Record [`Self::leaf_word`].
    pub fn set_leaf_word(&self, word: usize) {
        self.record.jit_leaf.set(word);
    }

    /// JIT round 21 lane reflect3 (`CRATONVM_JIT_MH_COMPILED_LINK_AGREE_MEMO`):
    /// the basic-type agreement verdict this record holds for a compiled site
    /// of descriptor `site` linked with layout key `layout`, or `None`. The
    /// verdict is a pure function of the site text, the layout and this
    /// member's descriptor (the record's own), so equal text is the key (JIT
    /// round 21 lane reflect4: compared by content, see the field).
    pub fn agree_memo(&self, site: &'static str, layout: u8) -> Option<bool> {
        match &*self.record.jit_agree.borrow() {
            Some((text, key, verdict)) if *key == layout && **text == *site => Some(*verdict),
            _ => None,
        }
    }

    /// Record [`Self::agree_memo`] (one entry: the last site asked).
    pub fn set_agree_memo(&self, site: &'static str, layout: u8, verdict: bool) {
        *self.record.jit_agree.borrow_mut() = Some((Box::from(site), layout, verdict));
    }

    /// The member's declaring class name, as the decode resolved it.
    pub fn class_name(&self) -> &str {
        &self.record.class
    }

    /// The member's method name.
    pub fn name(&self) -> &str {
        &self.record.name
    }

    /// JIT round 21 lane reflect (evidence for DEPTH21-1 of
    /// `docs/feature-designs/jit-ir-optimizations.md`): is the declaring
    /// class hidden -- a spun `LambdaForm$MH`, which [`Self::bind_checked`]
    /// refuses because the compiled call resolves names?
    pub fn owner_is_hidden(&self, ctx: &dyn NativeContext) -> bool {
        self.record
            .owner
            .is_some_and(|owner| ctx.is_class_hidden(owner))
    }

    /// JIT round 21 lane reflect: would [`Self::bind_checked`] answer
    /// differently later? Only its "static member of a class not initialized
    /// yet" refusal can lift (initialization is monotonic and the door that
    /// serves the call meanwhile initializes the class), so a caller memoising
    /// a refusal must not memoise that one.
    pub fn refusal_may_lift(&self, ctx: &dyn NativeContext, invoke_kind: u8) -> bool {
        invoke_kind == 3
            && self
                .record
                .owner
                .is_some_and(|owner| !ctx.is_class_initialized(owner))
    }

    /// The member as a compiled call target of `invoke_kind`, or `None` when a
    /// call by its NAME could land anywhere but on this very class, or could
    /// need work `linkTo*` does and a plain compiled call does not: a hidden or
    /// a second definition of the name (the dispatch resolves names), no owner
    /// id, or (`invoke_kind` 3) a class not yet initialized (`linkToStatic`
    /// initializes it first; initialization is monotonic, so a class seen
    /// initialized stays so).
    pub fn bind_checked(
        &self,
        ctx: &dyn NativeContext,
        invoke_kind: u8,
    ) -> Option<lang_invoke::MhCompiledBind> {
        let owner = self.record.owner?;
        if ctx.is_class_hidden(owner)
            || !matches!(
                ctx.classify_class_name(&self.record.class),
                cratonvm_native_api::NameLookup::Unique(id) if id == owner
            )
            || (invoke_kind == 3 && !ctx.is_class_initialized(owner))
        {
            return None;
        }
        Some(lang_invoke::MhCompiledBind {
            invoke_kind,
            class_id: owner.as_u32(),
            class: self.record.class.clone(),
            name: self.record.name.clone(),
            desc: self.record.desc.clone(),
            bound: false,
        })
    }

    /// JIT round 21 wave 1, lane reflect (`CRATONVM_JIT_MH_COMPILED_LINK_HIDDEN`,
    /// opt-in): the member as a static call into a HIDDEN class -- a spun
    /// `LambdaForm$MH` / `LambdaForm$DMH` form's entry, the one shape
    /// [`Self::bind_checked`] refuses because the compiled dispatch resolves
    /// names. The VM side calls it by the owner's id (`interpreter::execute`
    /// on that class, the door's own re-entry) instead of by name, so only the
    /// facts that call needs are asked: a static member (`REF_invokeStatic`)
    /// of a hidden owner that is already initialized (initialization is
    /// monotone; the first call through the door initializes it).
    pub fn bind_hidden_static(&self, ctx: &dyn NativeContext) -> Option<lang_invoke::MhCompiledBind> {
        let owner = self.record.owner?;
        if self.record.ref_kind != 6 || !ctx.is_class_hidden(owner) || !ctx.is_class_initialized(owner)
        {
            return None;
        }
        Some(lang_invoke::MhCompiledBind {
            invoke_kind: 3,
            class_id: owner.as_u32(),
            class: self.record.class.clone(),
            name: self.record.name.clone(),
            desc: self.record.desc.clone(),
            bound: false,
        })
    }
}

impl MhCompiledMember {
    /// The VM's by-class-id verdict for this member (`0` unknown, `1`
    /// refused, else an interned call info's address), valid for the record's
    /// epoch. Interpreter round i6 wave 116, lane L2.
    pub fn by_id_word(&self) -> usize {
        self.record.jit_by_id.get()
    }

    /// Record [`Self::by_id_word`].
    pub fn set_by_id_word(&self, word: usize) {
        self.record.jit_by_id.set(word);
    }

    /// [`Self::bind_checked`] without its two NAME conditions (hidden owner,
    /// unique name), for a caller that will never resolve the member by name:
    /// the by-class-id link of a compiled `invokeBasic`
    /// (`CRATONVM_JIT_MH_COMPILED_LINK_BY_ID`, interpreter round i6 wave 116,
    /// lane L2; `vm/src/jit/helpers.rs` `mh_compiled_basic_by_id`), which
    /// calls only a body it found in the code cache under the owner's id.
    /// Still requires an owner and, for a static (`invoke_kind` 3), an
    /// initialized owner (initialization is monotonic).
    pub fn bind_by_id(
        &self,
        ctx: &dyn NativeContext,
        invoke_kind: u8,
    ) -> Option<lang_invoke::MhCompiledBind> {
        let owner = self.record.owner?;
        if invoke_kind == 3 && !ctx.is_class_initialized(owner) {
            return None;
        }
        Some(lang_invoke::MhCompiledBind {
            invoke_kind,
            class_id: owner.as_u32(),
            class: self.record.class.clone(),
            name: self.record.name.clone(),
            desc: self.record.desc.clone(),
            bound: false,
        })
    }

    /// JIT round 21 lane reflect4 (REFLECT3-21-2 / REFLECT21-1,
    /// `CRATONVM_JIT_MH_COMPILED_LINK_HIDDEN`): the member as a compiled
    /// STATIC call target taken BY CLASS ID, for the one owner
    /// [`Self::bind_checked`] refuses because no name resolves it: a hidden
    /// class (a spun `LambdaForm$MH`, the `vmentry` of every customized
    /// `Method.invoke` adapter). Only a static member (`REF_invokeStatic`)
    /// of an initialized hidden class; the VM mints the call info with the
    /// owner's id as its `owner_class_id`, which every service of a compiled
    /// call asks before it would resolve the name (`vm/src/jit/helpers.rs`
    /// `jit_substituted_owner`). `None` with the switch off, which is the
    /// door as before.
    pub fn bind_checked_hidden(
        &self,
        ctx: &dyn NativeContext,
    ) -> Option<lang_invoke::MhCompiledBind> {
        if !lang_invoke::jit_mh_compiled_link_hidden_enabled() || self.record.ref_kind != 6 {
            return None;
        }
        let owner = self.record.owner?;
        if !ctx.is_class_hidden(owner) || !ctx.is_class_initialized(owner) {
            return None;
        }
        Some(lang_invoke::MhCompiledBind {
            invoke_kind: 3,
            class_id: owner.as_u32(),
            class: self.record.class.clone(),
            name: self.record.name.clone(),
            desc: self.record.desc.clone(),
            bound: false,
        })
    }
}

/// [`MhCompiledMember`] for `member`, or `None`: not a `MemberName`, a
/// decode `linkTo*` refuses, a lambda proxy's member (run on the receiver,
/// `native_method_handle_link_to`), no owner, or an unmemoised context.
pub fn mh_compiled_member(
    ctx: &mut dyn NativeContext,
    member: ObjectRef,
) -> Option<MhCompiledMember> {
    // JIT round 21 lane reflect2 (`CRATONVM_JIT_MH_COMPILED_LINK_LEAN`): a
    // record this epoch's memo holds for this very address was filed by
    // `link_to_record`, whose every caller checked the object a `MemberName`
    // first, and nothing moves or is freed within an epoch; so the class-name
    // read below (an `Arc` lookup per compiled link) answers nothing new.
    if crate::nbflags().jit_mh_compiled_link_lean {
        if let Some(record) = link_to_record_memo_hit(&*ctx, member) {
            if record.is_proxy || record.owner.is_none() || !(5..=9).contains(&record.ref_kind) {
                return None;
            }
            return Some(MhCompiledMember { record });
        }
    }
    if ctx.vm_identity() == 0
        || ctx
            .class_name_arc_of_id(ctx.class_id_of_object(member))
            .as_deref()
            != Some("java/lang/invoke/MemberName")
    {
        return None;
    }
    let record = link_to_record(ctx, member).ok()?;
    if record.is_proxy || record.owner.is_none() || !(5..=9).contains(&record.ref_kind) {
        return None;
    }
    Some(MhCompiledMember { record })
}

pub fn native_method_handle_link_to(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let Some(Value::Object(Some(member))) = args.last().copied() else {
        return link_to_unsupported("call carries no trailing MemberName appendix");
    };
    if ctx
        .class_name_arc_of_id(ctx.class_id_of_object(member))
        .as_deref()
        != Some("java/lang/invoke/MemberName")
    {
        // linkToNative's appendix is a NativeEntryPoint, not a MemberName.
        // Panama downcalls have their own route (`native_method_handle_invoke`
        // detects a DowncallHandle receiver and forwards to
        // `panama::pe_downcall_invoke`), so one arriving here has nowhere
        // left to go.
        return link_to_unsupported(
            "trailing appendix is not a MemberName (linkToNative's NativeEntryPoint \
             has no route here — Panama downcalls dispatch via panama::pe_downcall_invoke)",
        );
    }
    // JVMS 5.4.3.5 reference kinds.
    const REF_INVOKE_VIRTUAL: i32 = 5;
    const REF_INVOKE_STATIC: i32 = 6;
    const REF_INVOKE_SPECIAL: i32 = 7;
    const REF_NEW_INVOKE_SPECIAL: i32 = 8;
    const REF_INVOKE_INTERFACE: i32 = 9;
    // SBR5 lane avh: the decode below is a function of the `MemberName`'s final fields, and
    // every `LambdaForm` entry that ends in a `DirectMethodHandle` pays it per call (a class
    // name, two strings and a descriptor, about 1 us); filed per collection epoch.
    let record = match link_to_record(ctx, member) {
        Ok(record) => record,
        Err(unsupported) => return unsupported,
    };
    let (class_name, name, desc) = (&record.class, &record.name, &record.desc);
    let (ref_kind, param_count) = (record.ref_kind, record.param_count);
    let call_args = &args[..args.len() - 1];
    let takes_receiver = matches!(
        ref_kind,
        REF_INVOKE_VIRTUAL | REF_INVOKE_SPECIAL | REF_NEW_INVOKE_SPECIAL | REF_INVOKE_INTERFACE
    );
    let expected = param_count + usize::from(takes_receiver);
    if call_args.len() != expected {
        return link_to_unsupported(&format!(
            "argument-layout mismatch for {class_name}.{name}{desc} (refKind {ref_kind}): \
             got {} argument(s) before the MemberName, expected {expected}",
            call_args.len()
        ));
    }
    // A `null` receiver is HotSpot's message-less `NullPointerException`: the
    // linker intrinsics null-check it (`linkToSpecial` too) before any
    // dispatch. It was an `UnsupportedOperationException` here, so a real JDK
    // `DirectMethodHandle` over an instance method answered that for
    // `invokeExact(null, ..)` (interpreter round i2 wave 54, lane L4;
    // `L4W54RealJdkHandleArgs` row `direct-virtual-null`).
    if matches!(
        ref_kind,
        REF_INVOKE_VIRTUAL | REF_INVOKE_INTERFACE | REF_INVOKE_SPECIAL
    ) && matches!(call_args.first(), Some(Value::Object(None)))
    {
        return Err(RuntimeError::NullPointerException { message: None }.into());
    }
    // A lambda proxy is final and declares no static method: every member
    // with a receiver is the receiver's own, called virtually (the VM's
    // lambda dispatch serves the functional method, its bridges and a
    // serializable proxy's `writeReplace`).
    if record.is_proxy {
        return match (ref_kind, call_args.first().copied()) {
            (
                REF_INVOKE_VIRTUAL | REF_INVOKE_INTERFACE | REF_INVOKE_SPECIAL,
                Some(Value::Object(Some(receiver))),
            ) => ctx.invoke_virtual(receiver, &name, &desc, &call_args[1..]),
            _ => link_to_unsupported(&format!(
                "reference kind {ref_kind} on lambda proxy member {class_name}.{name}{desc}"
            )),
        };
    }
    match ref_kind {
        // `linkToStatic` / `linkToVirtual` / `linkToInterface` are tail calls on HotSpot (a
        // stub, no frame): a virtual thread that parks in the target unmounts. The native
        // hands the target's result back unchanged (the settled-method memo is a cache), so
        // it declares itself transparent for the nested call (`continuation_pin::tail_call`);
        // the constructor / special arm keeps post-call work and stays opaque.
        REF_INVOKE_STATIC if record.owner.is_some() => {
            // The declaring class is initialised first (JVMS 5.5), the resolved method kept.
            let (result, now_settled) = cratonvm_native_api::continuation_pin::tail_call(|| {
                ctx.invoke_static_settling(
                    record.owner,
                    record.settled.get(),
                    class_name,
                    name,
                    desc,
                    call_args,
                )
            });
            if now_settled.is_some() {
                record.settled.set(now_settled);
            }
            result
        }
        REF_INVOKE_STATIC => cratonvm_native_api::continuation_pin::tail_call(|| {
            ctx.invoke(class_name, name, desc, call_args)
        }),
        REF_INVOKE_VIRTUAL | REF_INVOKE_INTERFACE => {
            let Some(Value::Object(Some(receiver))) = call_args.first().copied() else {
                // HotSpot's `linkToVirtual` / `linkToInterface` null-check the
                // receiver (SBR3 lane mh): `NullPointerException`, not a VM refusal.
                return Err(RuntimeError::NullPointerException { message: None }.into());
            };
            cratonvm_native_api::continuation_pin::tail_call(|| {
                ctx.invoke_virtual_declared(&class_name, receiver, &name, &desc, &call_args[1..])
            })
        }
        // invokespecial semantics: exact binding on the declaring class, no
        // virtual re-dispatch. `invoke_special` takes the receiver as args[0],
        // which is where it already is.
        //
        // Interpreter round i2 wave 64, lane L4 (`CRATONVM_MH_SPECIAL_BY_OWNER_ID`,
        // `L4W64SpecialByOwnerId` under `--real-jdk`): by the declaring class's
        // id (`MemberName.clazz`'s mirror), not its name. `invoke_special`
        // re-resolved the name through the loader-blind global map, so a JDK
        // `findSpecial` handle over a class two loaders define ran a third,
        // by-name copy's body (`mid-?` where HotSpot prints `mid-A`).
        REF_INVOKE_SPECIAL | REF_NEW_INVOKE_SPECIAL => {
            let registered = link_to_special_constructor_registers(ctx, name, call_args);
            let result = match record.owner {
                Some(owner) if crate::nbflags().mh_special_by_owner_id => {
                    crate::lang_invoke::note_special_route(
                        "linkToSpecial",
                        class_name,
                        name,
                        desc,
                        true,
                    );
                    ctx.invoke_special_by_class_id(owner, class_name, name, desc, call_args)
                }
                _ => {
                    crate::lang_invoke::note_special_route(
                        "linkToSpecial",
                        class_name,
                        name,
                        desc,
                        false,
                    );
                    ctx.invoke_special(&class_name, &name, &desc, call_args)
                }
            };
            // JIT round 21 wave 10, lane finalize
            // (`CRATONVM_FINALIZER_REGISTER_AT_OBJECT_INIT`): a constructor
            // that threw before `Object.<init>` returned leaves no
            // registration, as on HotSpot (the VM decides from its unwinder's
            // proof; nothing happens unless one was noted for this receiver).
            if let (Some((receiver, epoch)), Err(_)) = (registered, &result) {
                ctx.withdraw_unconstructed_finalizable(receiver, epoch);
            }
            result
        }
        _ => link_to_unsupported(&format!(
            "unsupported reference kind {ref_kind} on {class_name}.{name}{desc} \
             (linkTo* is only defined for method kinds 5-9)"
        )),
    }
}

/// JIT round 21 wave 8, lane reflect (`CRATONVM_MH_CTOR_REGISTERS_FINALIZABLE`,
/// default ON; page
/// `docs/feature-designs/native-dispatch-architecture.md`):
/// the `linkToSpecial` of a `<init>` member is the constructor call of a
/// `newInvokeSpecial` form (`DirectMethodHandle.allocateInstance`, i.e.
/// `Unsafe.allocateInstance`, then this call), which is how every constructor
/// `MethodHandle` -- and so every JDK 25 `Constructor.newInstance`, through
/// `DirectConstructorHandleAccessor` -- builds its instance; the real-handle
/// door's `REF_newInvokeSpecial` arm (`lang_invoke::mh_real_direct_dispatch`)
/// ends here too. HotSpot registers a finalizable instance when its
/// `Object.<init>` returns (`RegisterFinalizersAtInit`); this VM registers at
/// the allocations that are always followed by a constructor, and
/// `Unsafe.allocateInstance` (rightly) is not one, so the instance was never
/// registered and its `finalize()` never ran. The receiver is registered here,
/// before its constructor runs -- as the interpreter's `new` is -- so a
/// constructor that throws after `super()` leaves it registered, as on HotSpot.
/// At most one row per object (`ReferenceProcessor::discover_reference`
/// ignores a second registration of an active finalizer row), so an instance
/// some other door already registered is not finalized twice. A `<init>` can
/// reach `linkToSpecial` on a fresh receiver only (`findSpecial` refuses
/// `<init>`; the verifier forbids it on an initialized one).
///
/// JIT round 21 wave 10, lane finalize: answers the receiver it offered and
/// the address epoch read after the offer, for the withdrawal when the
/// constructor throws before `Object.<init>` returns
/// (`NativeContext::withdraw_unconstructed_finalizable`); `None` when it
/// offered nothing.
pub fn link_to_special_constructor_registers(
    ctx: &mut dyn NativeContext,
    name: &str,
    call_args: &[Value],
) -> Option<(ObjectRef, u64)> {
    if name != "<init>" || false {
        return None;
    }
    if let Some(Value::Object(Some(receiver))) = call_args.first().copied() {
        ctx.register_if_finalizable(receiver);
        return Some((receiver, ctx.gc_address_epoch()));    }
    None
}

/// VM.getNanoTimeAdjustment — returns nanosecond offset from given epoch second.
pub fn native_vm_get_nano_time_adjustment(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let offset_secs = match args.get(0) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    // Current time in nanoseconds since Unix epoch
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let now_nanos = now.as_nanos() as i64;
    let offset_nanos = offset_secs * 1_000_000_000;
    Ok(Some(Value::Long(now_nanos - offset_nanos)))
}

/// `TimeZone.getSystemTimeZoneID(String javaHome)` — the platform's own zone,
/// mapped to an id `TimeZone.getTimeZone` understands.
///
/// W7-92: this used to be `let tz_id = "UTC";` with the comment *"A more
/// complete implementation would query the OS"*, and it is one of the two
/// producers of "the system zone is UTC" (the other is `timezone_default_ref`
/// in `register_essential_natives_with_shims`, which is what
/// `TimeZone.getDefault()` resolves to in `Compatible`/`--fast-boot` mode). This
/// one is the `--real-jdk` producer: the method is `ACC_NATIVE` with no `Code`,
/// so the native runs in every mode, and real `TimeZone.setDefaultZone()`
/// bytecode calls it whenever `user.timezone` is empty — which on Windows is
/// always, because `vm_init` seeds that property from `$TZ` alone.
///
/// `javaHome` is not decoration: `<java.home>/lib/tzmappings` is the file that
/// maps a Windows time-zone key name to an IANA id, and HotSpot's own
/// `TimeZone_md.c` is handed the same argument for the same reason. See
/// `tzdb::system_zone_id`.
///
/// Unresolvable hosts keep answering `"UTC"` rather than the JDK's `null`
/// (which `setDefaultZone` would turn into `GMT`): the point of this fix is to
/// change the answer where the platform DOES state a zone, not to change it
/// everywhere else.
pub fn native_timezone_get_system_id(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let java_home = match args.first() {
        Some(Value::Object(Some(o))) => ctx.read_string(*o),
        _ => None,
    }
    .filter(|home| !home.is_empty());
    let tz_id =
        crate::tzdb::system_zone_id(ctx, java_home.as_deref()).unwrap_or_else(|| "UTC".to_string());
    let s = ctx.create_string(&tz_id);
    Ok(Some(Value::Object(Some(s))))
}

/// `TimeZone.getSystemGMTOffsetID` — the host's standard UTC offset as a
/// custom `"GMT±HH:MM"` id, or `null` when the platform does not say.
///
/// W7-92: this used to be an unconditional `null`. `null` is a legal answer
/// and the JDK handles it, but it discards the one piece of information that
/// survives when a zone NAME cannot be resolved — the OFFSET.
/// `TimeZone.setDefaultZone()` calls this exactly when
/// `getTimeZone(getSystemTimeZoneID(...), false)` returned null, and uses the
/// result in place of the unresolvable id; with `null` here that path lands on
/// `GMT`, i.e. back on a wrong zero offset. See `tzdb::platform_gmt_offset_id`.
pub fn native_timezone_get_gmt_offset_id(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    match crate::tzdb::platform_gmt_offset_id() {
        Some(id) => {
            let s = ctx.create_string(&id);
            Ok(Some(Value::Object(Some(s))))
        }
        // The JDK's own "no custom GMT offset id" encoding.
        None => Ok(Some(Value::Object(None))),
    }
}

// ===========================================================================
// java.lang.Enum
// ===========================================================================

/// Enum.<init>(String name, int ordinal) — store name in field 0, ordinal in field 1
/// Enum.ordinal() — return field 1 (int)
/// Enum.name() and Enum.toString() — return field 0 (String ref)
/// Enum.compareTo(Enum other) — this.ordinal - other.ordinal
/// Enum.equals(Object) — identity comparison (reference equality)
/// Enum.hashCode() — identity hash code
/// Enum.getDeclaringClass() — return Class mirror for this enum's class
// ===========================================================================
// Integer.compare / Integer.compareTo / Long.compare / Long.compareTo
// ===========================================================================

// ===========================================================================
// java.util.Objects
// ===========================================================================

pub fn register_objects_natives(r: &mut NativeMethodRegistry, kind: cratonvm_native_api::NativeKind) {
    // census-tag: java.util.Objects.* fast-paths replicate real JDK bytecode
    // with spec-exact semantics → Intrinsic.
    //
    // The KIND is the caller's, because the same bodies mean two different
    // things depending on which registrar installed them:
    //
    //   * `register_synthetic_overrides` → `Intrinsic`. On a synthetic-JDK
    //     image there is no `java/util/Objects` bytecode at all, so these ARE
    //     the implementation and must always win.
    //   * `register_annotation_overrides` (reached from
    //     `register_essential_natives_with_shims`, i.e. the REAL-JDK path)
    //     → `SyntheticStub`. There they exist only so `requireNonNull` and
    //     friends still link when java.base stubs are partial. When the real
    //     class IS loaded they must yield, exactly like the `StringJoiner`,
    //     `EnumSet` and `Instant` stubs registered beside them.
    //
    // Registering the real-JDK path as `Intrinsic` made every one of these win
    // unconditionally on `invokestatic`, because `dispatch_static` arbitrates
    // on `NativeKind` alone -- so the real bytecode was never used, the method
    // was never JIT-compiled, and every call paid the full native funnel:
    // `Objects.equals` 203.5 ns against 47.2 ns for a byte-identical local
    // static, on 10M calls. See
    // `docs/feature-designs/native-dispatch-architecture.md`'s
    // `springboot-configurationpropertysources-native-collections-floor-20260821-RETIRED.md`,
    // Term 3.
    let o = "java/util/Objects";
    r.register(
        o,
        "requireNonNull",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_objects_require_non_null, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        o,
        "requireNonNull",
        "(Ljava/lang/Object;Ljava/lang/String;)Ljava/lang/Object;",
        native_objects_require_non_null_msg, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        o,
        "requireNonNullElse",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_objects_require_non_null_else, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        o,
        "requireNonNullElseGet",
        "(Ljava/lang/Object;Ljava/util/function/Supplier;)Ljava/lang/Object;",
        native_objects_require_non_null_else_get, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        o,
        "equals",
        "(Ljava/lang/Object;Ljava/lang/Object;)Z",
        native_objects_equals, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        o,
        "hashCode",
        "(Ljava/lang/Object;)I",
        native_objects_hash_code, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(o, "hash", "([Ljava/lang/Object;)I", native_objects_hash, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        o,
        "toString",
        "(Ljava/lang/Object;)Ljava/lang/String;",
        native_objects_to_string, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        o,
        "toString",
        "(Ljava/lang/Object;Ljava/lang/String;)Ljava/lang/String;",
        native_objects_to_string_default, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(o, "isNull", "(Ljava/lang/Object;)Z", native_objects_is_null, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        o,
        "nonNull",
        "(Ljava/lang/Object;)Z",
        native_objects_non_null, cratonvm_native_api::NativeKind::Bridge,
    );
}

/// Null-safe equality check for Objects.equals (duplicated from collections.rs values_equal)
pub fn objects_values_equal(
    ctx: &mut dyn NativeContext,
    a: &Value,
    b: &Value,
) -> Result<bool, MethodCallFailed> {
    match (a, b) {
        (Value::Object(None), Value::Object(None)) => Ok(true),
        (Value::Object(None), _) => Ok(false),
        // `Objects.equals` is `(a == b) || (a != null && a.equals(b))` — a
        // non-null `a` gets `a.equals(null)` dispatched, it is NOT short-
        // circuited to false. Classes whose `equals` accepts null are rare but
        // real and load-bearing: Spring's `NullBean` (the placeholder for a
        // `@Bean` method that returned null) is defined as
        // `return (this == obj || obj == null)` precisely so that
        // `getBean(name)` compares equal to null.
        (Value::Object(Some(ra)), Value::Object(None)) => {
            let r = ctx.invoke_virtual(
                *ra,
                "equals",
                "(Ljava/lang/Object;)Z",
                &[Value::Object(None)],
            )?;
            Ok(matches!(r, Some(Value::Int(v)) if v != 0))
        }
        (_, Value::Object(None)) => Ok(false),
        (Value::Object(Some(ra)), Value::Object(Some(rb))) => {
            if ra.as_ptr() == rb.as_ptr() {
                return Ok(true);
            }
            // String fast-path (the common case).
            if let (Some(sa), Some(sb)) = (ctx.read_string(*ra), ctx.read_string(*rb)) {
                return Ok(sa == sb);
            }
            // General `Objects.equals` contract: dispatch to the receiver's real
            // `equals(Object)`. The previous "compare field 0" fallback is
            // correct ONLY for primitive-wrapper boxes (Integer/Boolean/… whose
            // slot 0 holds the primitive value). For an ArrayList / HashSet /
            // record / any value class, slot 0 is a backing-array/element ref
            // that differs between two equal-but-distinct instances, so
            // `Objects.equals(list1, list2)` returned false — and since the JDK
            // `Optional.equals`, record `equals`, and most hand-written
            // `equals` route through `Objects.equals`, those all broke for
            // value-equal objects. Surfaced as Kafka DescribeConsumerGroups:
            // `ConsumerGroupDescription.equals` false for two value-equal
            // objects (via `Optional<MemberAssignment>` / `Optional<List>`).
            // Mirrors `values_equal` in native-collections.
            if crate::nbflags().dbg_objects {
                eprintln!(
                    "[objects-native] objects_values_equal -> invoke_virtual equals on ra={:?}",
                    ra
                );
            }
            let r = ctx.invoke_virtual(
                *ra,
                "equals",
                "(Ljava/lang/Object;)Z",
                &[Value::Object(Some(*rb))],
            )?;
            if crate::nbflags().dbg_objects {
                eprintln!(
                    "[objects-native] objects_values_equal invoke_virtual returned {:?}",
                    r
                );
            }
            match r {
                Some(Value::Int(v)) => Ok(v != 0),
                _ => Ok(false),
            }
        }
        _ => Ok(a == b),
    }
}

pub fn native_objects_require_non_null(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match args.first() {
        Some(Value::Object(None)) | None => {
            Err(cratonvm_types::error::RuntimeError::NullPointerException { message: None }.into())
        }
        Some(val) => Ok(Some(*val)),
    }
}

pub fn native_objects_require_non_null_msg(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match args.first() {
        Some(Value::Object(None)) | None => {
            let msg = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s),
                _ => None,
            };
            Err(cratonvm_types::error::RuntimeError::NullPointerException { message: msg }.into())        }
        Some(val) => Ok(Some(*val)),    }}

pub fn native_objects_require_non_null_else(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match args.first().copied().unwrap_or(Value::Object(None)) {
        Value::Object(None) => match args.get(1).copied().unwrap_or(Value::Object(None)) {
            Value::Object(None) => Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("defaultObj must not be null".to_string()),
            }
            .into()),
            default_obj => Ok(Some(default_obj)),
        },
        obj => Ok(Some(obj)),
    }
}

pub fn native_objects_require_non_null_else_get(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match args.first().copied().unwrap_or(Value::Object(None)) {
        Value::Object(None) => {
            let supplier = match args.get(1).copied().unwrap_or(Value::Object(None)) {
                Value::Object(Some(supplier)) => supplier,
                _ => {
                    return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                        message: Some("supplier must not be null".to_string()),
                    }
                    .into());
                }
            };
            match ctx.invoke_virtual(supplier, "get", "()Ljava/lang/Object;", &[])? {
                Some(Value::Object(None)) | None => {
                    Err(cratonvm_types::error::RuntimeError::NullPointerException {
                        message: Some("supplier.get() must not be null".to_string()),
                    }
                    .into())
                }
                Some(default_obj) => Ok(Some(default_obj)),
            }
        }
        obj => Ok(Some(obj)),
    }
}

pub fn native_objects_equals(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if crate::nbflags().dbg_objects {
        eprintln!(
            "[objects-native] native_objects_equals CALLED args={:?}",
            args
        );
    }
    let a = args.first().copied().unwrap_or(Value::Object(None));
    let b = args.get(1).copied().unwrap_or(Value::Object(None));
    let eq = objects_values_equal(ctx, &a, &b)?;
    Ok(Some(Value::Int(if eq { 1 } else { 0 })))
}

pub fn native_objects_value_hash_code(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
) -> Result<i32, MethodCallFailed> {
    if ctx.object_is_array(obj) {
        return Ok(ctx.identity_hash_code(obj));
    }

    let class_id = ctx.class_id_of_object(obj);
    // JIT round 19 wave 3, lane funnel3 (NIO2-17-5 candidate 5,
    // `CRATONVM_CLASS_FACTS_MEMO_VALUE_HASH=1`, default OFF): the value-hash
    // kind from the per-thread class-facts memo. The memo's kind maps back to
    // the class name the match below expects, so both arms run one body.
    let memo_name = crate::class_facts_memo::rung_facts(
        ctx,
        crate::class_facts_memo::Rung::ValueHash,
        class_id,
    )
    .map(|facts| {
        use crate::class_facts_memo::BoxKind;
        let facts = facts?;
        if facts.is_string() {
            return Some("java/lang/String");
        }
        Some(match facts.box_kind() {
            Some(BoxKind::Integer) => "java/lang/Integer",
            Some(BoxKind::Byte) => "java/lang/Byte",
            Some(BoxKind::Short) => "java/lang/Short",
            Some(BoxKind::Character) => "java/lang/Character",
            Some(BoxKind::Boolean) => "java/lang/Boolean",
            Some(BoxKind::Long) => "java/lang/Long",
            Some(BoxKind::Float) => "java/lang/Float",
            Some(BoxKind::Double) => "java/lang/Double",
            // Any other class: the virtual `hashCode()` arm.
            None => "",
        })
    });
    let by_name;
    let name = match memo_name {
        Some(name) => name,
        None => {
            by_name = ctx.class_name_arc_of_id(class_id);
            by_name.as_deref()
        }
    };
    match name {
        Some("java/lang/String") => {
            match native_string_hash_code(ctx, &[Value::Object(Some(obj))])? {
                Some(Value::Int(v)) => Ok(v),
                _ => Ok(0),
            }
        }
        Some(
            "java/lang/Integer" | "java/lang/Byte" | "java/lang/Short" | "java/lang/Character",
        ) => match ctx.get_field(obj, 0) {
            Value::Int(v) => Ok(v),
            _ => Ok(0),
        },
        Some("java/lang/Boolean") => match ctx.get_field(obj, 0) {
            Value::Int(v) => Ok(if v != 0 { 1231 } else { 1237 }),
            _ => Ok(1237),
        },
        Some("java/lang/Long") => match ctx.get_field(obj, 0) {
            Value::Long(v) => Ok((v ^ ((v as u64 >> 32) as i64)) as i32),
            _ => Ok(0),
        },
        Some("java/lang/Float") => match ctx.get_field(obj, 0) {
            Value::Float(v) => {
                let bits = if v.is_nan() { 0x7fc0_0000 } else { v.to_bits() };
                Ok(bits as i32)            }
            _ => Ok(0),
        },
        Some("java/lang/Double") => match ctx.get_field(obj, 0) {
            Value::Double(v) => {
                let bits = if v.is_nan() {
                    0x7ff8_0000_0000_0000u64                } else {
                    v.to_bits()                };
                Ok((bits ^ (bits >> 32)) as i32)            }
            _ => Ok(0),
        },
        _ => match ctx.invoke_virtual(obj, "hashCode", "()I", &[])? {
            Some(Value::Int(v)) => Ok(v),
            _ => Ok(0),
        },
    }
}

pub fn native_objects_hash_code(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if crate::nbflags().dbg_objects {
        eprintln!(
            "[objects-native] native_objects_hash_code CALLED args={:?}",
            args
        );
    }
    match args.first() {
        Some(Value::Object(Some(obj))) => {
            Ok(Some(Value::Int(native_objects_value_hash_code(ctx, *obj)?)))
        }
        _ => Ok(Some(Value::Int(0))),
    }
}

pub fn native_objects_hash(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let arr = match args.first() {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(Some(Value::Int(0))),
    };
    // Element hashCode() is virtual and can allocate/collect. Keep the input
    // array rooted and reload it before every element access rather than
    // dereferencing its pre-GC address on the next iteration.
    let arr_pin = ctx.pin_native_root(arr);
    let result = (|| -> Result<i32, MethodCallFailed> {
        let len = ctx.array_length(ctx.read_native_pin(arr_pin, arr));
        let mut result: i32 = 1;
        for i in 0..len {
            let arr = ctx.read_native_pin(arr_pin, arr);
            let elem = ctx.get_array_element(arr, i);
            let elem_hash = match elem {
                Value::Object(Some(obj)) => native_objects_value_hash_code(ctx, obj)?,
                Value::Int(v) => v,
                Value::Long(v) => (v ^ (v >> 32)) as i32,
                Value::Float(v) => {
                    let bits = if v.is_nan() { 0x7fc0_0000 } else { v.to_bits() };
                    bits as i32
                }
                Value::Double(v) => {
                    let bits = if v.is_nan() {
                        0x7ff8_0000_0000_0000u64                    } else {
                        v.to_bits()                    };
                    (bits ^ (bits >> 32)) as i32                }
                _ => 0,
            };
            result = result.wrapping_mul(31).wrapping_add(elem_hash);        }
        Ok(result)    })();
    ctx.unpin_native_roots(arr_pin);
    Ok(Some(Value::Int(result?)))
}

pub fn native_objects_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Real JDK: `Objects.toString(Object o)` is `return String.valueOf(o);`,
    // and the two-arg `Objects.toString(Object o, String nullDefault)` is
    // `return (o != null) ? o.toString() : nullDefault;` (the null case is
    // handled separately by native_objects_to_string_default below, which
    // never calls this function for a null `o`). Both non-null paths reduce
    // to the SAME thing: invoke the object's own (possibly overridden)
    // `toString()` virtually and use its result verbatim.
    //
    // BUG (found 2026-07-14 via CrossOriginAnnotationIntegrationTests /
    // RequestMappingMessageConversionIntegrationTests, both 0/N against a
    // real HTTP backend): this previously built `ClassName@hex` directly
    // from `identity_hash_code` for every non-String object, WITHOUT ever
    // dispatching to the object's real `toString()` override. Every other
    // toString-invoking path (`Object.toString()` itself, implicit string
    // concat, `String.valueOf(Object)`, `StringBuilder.append(Object)`) had
    // already been fixed/verified to virtually dispatch correctly (see
    // `object-tostring-uses-identity-not-virtual-hashcode` memory / commit
    // d7116160) — `Objects.toString(Object[, String])` was a separate,
    // missed native registration that silently regressed the exact same
    // class of bug. Concretely: Apache HttpComponents5's
    // `RequestTargetHost.process()` builds the outgoing `Host` header via
    // `httpRequest.addHeader("Host", authority)` where `authority` is a
    // `URIAuthority` (an `Object` overload!) — `BasicHeader`'s constructor
    // stores the header value via `Objects.toString(value, null)`, so this
    // bug sent the *raw identity form* of the `URIAuthority` object
    // (`org.apache.hc.core5.net.URIAuthority@<hex>`) as the literal `Host`
    // header text, which every server-side backend correctly rejected as a
    // malformed Host header (400 Bad Request / "Bad HostPort" / "the
    // character [@] is never valid in a domain name") — for EVERY request,
    // on every backend, in any WebFlux/httpclient5-based integration test.
    //
    // Fix: dispatch to invoke_to_string (same helper `String.valueOf(Object)`
    // already uses correctly) instead of hand-rolling the identity format.
    let s = match args.first() {
        Some(Value::Object(None)) | None => "null".to_string(),
        Some(Value::Object(Some(obj))) => {
            if let Some(s) = ctx.read_string(*obj) {
                s
            } else {
                invoke_to_string(ctx, *obj)?
            }
        }
        Some(Value::Int(v)) => v.to_string(),
        Some(Value::Long(v)) => v.to_string(),
        Some(Value::Float(v)) => format!("{v}"),
        Some(Value::Double(v)) => format!("{v}"),
        _ => "?".to_string(),
    };
    Ok(Some(Value::Object(Some(ctx.create_string(&s)))))
}

pub fn native_objects_to_string_default(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match args.first() {
        Some(Value::Object(None)) | None => {
            // Return the default string (arg 1)
            let default = args.get(1).copied().unwrap_or(Value::Object(None));
            Ok(Some(default))
        }
        _ => native_objects_to_string(ctx, args),
    }
}

pub fn native_objects_is_null(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    match args.first() {
        Some(Value::Object(None)) | None => Ok(Some(Value::Int(1))),
        _ => Ok(Some(Value::Int(0))),
    }
}

pub fn native_objects_non_null(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    match args.first() {
        Some(Value::Object(None)) | None => Ok(Some(Value::Int(0))),
        _ => Ok(Some(Value::Int(1))),
    }
}

// ---------------------------------------------------------------------------
// System.getenv
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// String modern methods (Java 11+)
// ---------------------------------------------------------------------------

/// repeat(int) — "ab".repeat(3) → "ababab"
/// isBlank() — true if empty or all whitespace
/// chars() — returns IntStream of char values
/// regionMatches(boolean ignoreCase, int toffset, String other, int ooffset, int len)
/// regionMatches(int toffset, String other, int ooffset, int len) — case-sensitive
/// formatted(Object[]) — instance method: this.formatted(args) → String.format(this, args)
// ============================================================================
// Phase 15 Step 1: java.util.regex.Pattern / Matcher
// ============================================================================

// Pattern = 2-field synthetic: field 0 = source pattern String, field 1 = Int flags
pub const PAT_FIELD_SOURCE: usize = 0;
pub const PAT_FIELD_FLAGS: usize = 1;
pub const PAT_NUM_FIELDS: usize = 2;

// Matcher = 5-field synthetic:
// field 0 = Pattern ref, field 1 = input String obj,
// field 2 = Int search offset, field 3 = Int last match start (-1),
// field 4 = Int last match end (-1)
pub const MAT_FIELD_PATTERN: usize = 0;
pub const MAT_FIELD_INPUT: usize = 1;
pub const MAT_FIELD_OFFSET: usize = 2;
pub const MAT_FIELD_MATCH_START: usize = 3;
pub const MAT_FIELD_MATCH_END: usize = 4;
pub const MAT_FIELD_LAST_APPEND: usize = 5;
pub const MAT_NUM_FIELDS: usize = 6;

/// Java regex flags (java.util.regex.Pattern constants)
pub const JAVA_REGEX_CASE_INSENSITIVE: i32 = 2;
pub const JAVA_REGEX_COMMENTS: i32 = 4;
pub const JAVA_REGEX_MULTILINE: i32 = 8;
pub const JAVA_REGEX_LITERAL: i32 = 16;
pub const JAVA_REGEX_DOTALL: i32 = 32;
pub const JAVA_REGEX_UNICODE_CASE: i32 = 64;
pub const JAVA_REGEX_UNICODE_CHARACTER_CLASS: i32 = 256;

/// A successful match, with byte offsets into the haystack. We materialise the
/// matched text as `String` so callers (which often need to bridge into the
/// Java heap) don't have to keep the haystack borrow alive.
pub struct JavaMatch {
    pub start: usize,
    pub end: usize,
    pub text: String,
}

/// Capture groups for a single match — index 0 is the whole match. Group N may
/// be `None` if the optional alternative didn't participate in the match.
pub struct JavaCaptures {
    pub groups: Vec<Option<JavaMatch>>,
    pub named: std::collections::HashMap<String, usize>,
}

impl JavaCaptures {
    pub fn len(&self) -> usize {
        self.groups.len()
    }
    pub fn get(&self, idx: usize) -> Option<&JavaMatch> {
        self.groups.get(idx).and_then(|o| o.as_ref())
    }
    pub fn name(&self, name: &str) -> Option<&JavaMatch> {
        self.named.get(name).and_then(|&i| self.get(i))
    }
}

/// One unit of a parsed Java replacement string.
pub enum JavaReplToken {
    Lit(String),
    Group(usize),
    Named(String),
}

/// Split a string on every match of a fancy-regex Regex. Mirrors the semantics
/// of `regex::Regex::split`: yields the text between matches, possibly
/// including empty leading/trailing pieces. We advance manually to handle
/// zero-width matches correctly (advance by one char to avoid infinite loops).
pub fn fancy_split_all(r: &fancy_regex::Regex, text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut last = 0usize;
    let mut pos = 0usize;
    while pos <= text.len() {
        match r.find_from_pos(text, pos) {
            Ok(Some(m)) => {
                out.push(text[last..m.start()].to_string());
                if m.end() == m.start() {
                    // Zero-width match: advance one char to avoid looping.
                    let mut step = pos + 1;
                    while step < text.len() && !text.is_char_boundary(step) {
                        step += 1;
                    }
                    last = m.end();
                    pos = step.max(m.end());
                } else {
                    last = m.end();
                    pos = m.end();
                }
            }
            _ => break,
        }
    }
    out.push(text[last..].to_string());
    out
}

pub fn fancy_splitn(r: &fancy_regex::Regex, text: &str, limit: usize) -> Vec<String> {
    if limit == 0 {
        return fancy_split_all(r, text);
    }
    let mut out = Vec::new();
    let mut last = 0usize;
    let mut pos = 0usize;
    while out.len() + 1 < limit && pos <= text.len() {
        match r.find_from_pos(text, pos) {
            Ok(Some(m)) => {
                out.push(text[last..m.start()].to_string());
                if m.end() == m.start() {
                    let mut step = pos + 1;
                    while step < text.len() && !text.is_char_boundary(step) {
                        step += 1;
                    }
                    last = m.end();
                    pos = step.max(m.end());
                } else {
                    last = m.end();
                    pos = m.end();
                }
            }
            _ => break,
        }
    }
    out.push(text[last..].to_string());
    out
}

/// Compile an already-translated regex pattern string straight into a
/// `JavaRegex`, trying the `regex` crate first and falling back to
/// `fancy-regex`. Unlike `compile_java_regex` this does NOT run
/// `translate_java_regex` or apply flag prefixes — the input is expected to be
/// a final pattern (e.g. an anchored `^(?:...)$` variant built from an
/// already-compiled `JavaRegex::as_str()`).
///
/// Results are memoised in a bounded cache; `Matcher.matches` / `lookingAt`
/// rebuild the same anchored pattern on every call, so caching avoids a
/// recompile per invocation. Returns `None` if neither engine accepts it.
pub fn compile_anchored_cached(full: &str) -> Option<JavaRegex> {
    const ANCHORED_CACHE_CAP: usize = 512;
    type AnchoredCache = std::collections::HashMap<String, Option<JavaRegex>>;
    static CACHE: OnceLock<Mutex<AnchoredCache>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()));

    if let Ok(guard) = cache.lock() {
        if let Some(re) = guard.get(full) {
            return re.clone();
        }
    }

    let compiled = match regex::Regex::new(full) {
        Ok(r) => Some(JavaRegex::Std(r)),
        Err(_) => fancy_regex::Regex::new(full)
            .ok()
            .map(|r| JavaRegex::Fancy(Box::new(r))),
    };

    if let Ok(mut guard) = cache.lock() {
        if guard.len() >= ANCHORED_CACHE_CAP {
            guard.clear();
        }
        guard.insert(full.to_string(), compiled.clone());
    }
    compiled
}

/// Rewrite the Perl character-class shorthands `\d \D \w \W \s \S` and the word
/// boundaries `\b \B` from the `regex` crate's Unicode-by-default meaning to
/// Java's **ASCII-by-default** meaning (i.e. the behavior when
/// `Pattern.UNICODE_CHARACTER_CLASS` is *not* set, which is the default).
///
/// | shorthand | Java ASCII default            |
/// |-----------|-------------------------------|
/// | `\d`      | `[0-9]`                       |
/// | `\D`      | `[^0-9]`                      |
/// | `\w`      | `[a-zA-Z0-9_]`                |
/// | `\W`      | `[^a-zA-Z0-9_]`               |
/// | `\s`      | `[ \t\n\x0B\f\r]`             |
/// | `\S`      | `[^ \t\n\x0B\f\r]`            |
/// | `\b` `\B` | ASCII word boundary `(?-u:…)`  |
///
/// The negated forms expand to *Unicode-mode* negated classes (`[^0-9]` etc.):
/// in the default Unicode `regex` mode `[^0-9]` matches any scalar except an
/// ASCII digit — exactly Java's `\D` — and (unlike `(?-u:\D)`, which matches a
/// non-ASCII *byte*) stays valid-UTF-8-safe for `&str` matching.
///
/// Context rules mirror Java: outside a `[...]` class the positive forms expand
/// to a bracketed class; inside a class they expand to the bare range
/// (`[\d.]` → `[0-9.]`). Inside a class the *negated* forms and the boundaries
/// are left untouched (a negated shorthand inside a positive set, or `\b` =
/// backspace inside a class, are rare and not additively expressible) — a
/// documented, narrow residual that only affects non-ASCII input.
///
/// Escapes are tracked so a literal backslash (`\\`) is never misread as the
/// start of a shorthand.
pub fn ascii_perl_classes(pattern: &str) -> String {
    // Cheap bail: nothing to do if there is no backslash at all.
    if !pattern.contains('\\') {
        return pattern.to_string();
    }
    let bytes = pattern.as_bytes();
    let mut out = String::with_capacity(pattern.len() + 8);
    let mut i = 0;
    let mut in_class = false;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\\' && i + 1 < bytes.len() {
            let n = bytes[i + 1];
            let bare = match n {
                b'd' => Some("0-9"),
                b'w' => Some("a-zA-Z0-9_"),
                b's' => Some(" \\t\\n\\x0B\\f\\r"),
                _ => None,
            };
            if !in_class {
                // Outside a character class: emit a full bracketed class.
                match n {
                    b'd' => out.push_str("[0-9]"),
                    b'D' => out.push_str("[^0-9]"),
                    b'w' => out.push_str("[a-zA-Z0-9_]"),
                    b'W' => out.push_str("[^a-zA-Z0-9_]"),
                    b's' => out.push_str("[ \\t\\n\\x0B\\f\\r]"),
                    b'S' => out.push_str("[^ \\t\\n\\x0B\\f\\r]"),
                    b'b' => out.push_str("(?-u:\\b)"),
                    b'B' => out.push_str("(?-u:\\B)"),
                    _ => {
                        // Any other escape — emit unchanged (consume both bytes).
                        out.push('\\');
                        out.push(n as char);
                    }
                }
                i += 2;
                continue;
            } else if let Some(rng) = bare {
                // Inside a class: emit the bare range (additive).
                out.push_str(rng);
                i += 2;
                continue;
            } else {
                // Inside a class, negated shorthand / boundary / other escape:
                // pass through unchanged (consume both bytes so `\]` etc. are
                // not misread as a class terminator).
                out.push('\\');
                out.push(n as char);
                i += 2;
                continue;
            }
        }
        // Track character-class nesting (naive single-level; `\[`/`\]` are
        // handled above by the escape branch and never reach here).
        if c == b'[' {
            in_class = true;
        } else if c == b']' {
            in_class = false;
        }
        let ch_len = utf8_char_len(c);
        let end = (i + ch_len).min(bytes.len());
        out.push_str(&pattern[i..end]);
        i = end;
    }
    out
}

/// Map a Java POSIX character-class name (`\p{Name}` with no `In`/`Is`/`java`
/// prefix) to a Rust regex character-class body. See the loop branch above
/// for why this table exists. Always US-ASCII-only, matching Java's default.
pub fn map_posix_character_class(name: &str) -> Option<&'static str> {
    match name {
        "Lower" => Some("a-z"),
        "Upper" => Some("A-Z"),
        "ASCII" => Some(r"\x00-\x7F"),
        "Alpha" => Some("a-zA-Z"),
        "Digit" => Some("0-9"),
        "Alnum" => Some("a-zA-Z0-9"),
        // `!"#$%&'()*+,-./` (\x21-\x2F) + `:;<=>?@` (\x3A-\x40) +
        // `[\]^_`` (\x5B-\x60) + `{|}~` (\x7B-\x7E) -- printable ASCII
        // minus letters and digits, as four contiguous byte ranges
        // (sidesteps escaping `]`/`\`/`-` as literal bracket-class chars).
        "Punct" => Some(r"\x21-\x2F\x3A-\x40\x5B-\x60\x7B-\x7E"),
        "Graph" => Some(r"\x21-\x7E"),
        "Print" => Some(r"\x20-\x7E"),
        "Blank" => Some(" \t"),
        "Cntrl" => Some(r"\x00-\x1F\x7F"),
        "XDigit" => Some("0-9a-fA-F"),
        "Space" => Some(r" \t\n\x0B\f\r"),
        _ => None,
    }
}

/// Map a Java Unicode block name (used after the `In` prefix in `\p{InXxx}`)
/// to a Rust regex character-class body covering the same code points. The
/// Rust regex crate doesn't expose Unicode blocks natively, so we substitute
/// equivalent code-point ranges for the common ones encountered in real-world
/// JDK / library code. Returns `None` for unknown blocks so the caller can
/// fall back to a less specific translation.
pub fn map_java_unicode_block(name: &str, is_block: bool) -> Option<&'static str> {
    if !is_block {
        // Script names ("Is" prefix) — Rust regex supports `\p{Latin}` etc.
        // already; let the caller re-emit without the prefix.
        return None;
    }
    // Block ranges from the Unicode Standard. Add more as needed.
    match name {
        "Basic_Latin" | "BasicLatin" => Some(r"\x00-\x7F"),
        "Latin-1_Supplement" | "Latin1Supplement" => Some(r"\u{0080}-\u{00FF}"),
        "Latin_Extended-A" | "LatinExtendedA" => Some(r"\u{0100}-\u{017F}"),
        "Latin_Extended-B" | "LatinExtendedB" => Some(r"\u{0180}-\u{024F}"),
        "IPA_Extensions" | "IPAExtensions" => Some(r"\u{0250}-\u{02AF}"),
        "Spacing_Modifier_Letters" | "SpacingModifierLetters" => Some(r"\u{02B0}-\u{02FF}"),
        // U+0300-U+036F — the block `StringUtils.stripAccents` filters via
        // `\p{InCombiningDiacriticalMarks}` (commons-lang3 StringUtils.<clinit>).
        "Combining_Diacritical_Marks" | "CombiningDiacriticalMarks" => Some(r"\u{0300}-\u{036F}"),
        "Greek" | "Greek_and_Coptic" => Some(r"\u{0370}-\u{03FF}"),
        "Cyrillic" => Some(r"\u{0400}-\u{04FF}"),
        "General_Punctuation" | "GeneralPunctuation" => Some(r"\u{2000}-\u{206F}"),
        "CJK_Unified_Ideographs" | "CJKUnifiedIdeographs" => Some(r"\u{4E00}-\u{9FFF}"),
        "Hiragana" => Some(r"\u{3040}-\u{309F}"),
        "Katakana" => Some(r"\u{30A0}-\u{30FF}"),
        "Hangul_Syllables" | "HangulSyllables" => Some(r"\u{AC00}-\u{D7AF}"),
        _ => None,
    }
}

/// Translate a `\p{java*}` / `\P{java*}` predefined Java character-class
/// name (see `java.util.regex.Pattern`'s `CharPredicates.forProperty`) into
/// a Rust-regex-crate-compatible class body — i.e. text valid to place
/// inside a single pair of `[...]` brackets. Returns `None` for names
/// outside this family (letting the caller fall back to copying the escape
/// literally, same as any other unrecognized `\p{...}` form).
///
/// Each entry mirrors the exact semantics documented on the corresponding
/// `java.lang.Character.isXxx(int)` method (verified against JDK 25
/// source), not a convenient approximation. Notably:
/// - `javaWhitespace` (`Character.isWhitespace`) is Unicode space
///   separators (`Zs`/`Zl`/`Zp`) *excluding* the three non-breaking spaces
///   (U+00A0, U+2007, U+202F), plus the ASCII control-whitespace range
///   U+0009-U+000D and the separator controls U+001C-U+001F. Expressed as
///   explicit codepoints/ranges rather than `\p{Z}` minus a subtraction, so
///   it doesn't depend on character-class set-operator support.
/// - `javaJavaIdentifierStart`/`javaJavaIdentifierPart` follow the older,
///   simpler `Character.isJavaIdentifierStart`/`Part` rules (letter/digit +
///   currency + connector punctuation, + combining/non-spacing marks for
///   `Part`) — NOT the same as `javaUnicodeIdentifierStart`/`Part`, which
///   follow the newer `ID_Start`/`ID_Continue`-based UAX31 profile.
pub fn map_java_predefined_class(name: &str) -> Option<&'static str> {
    match name {
        "javaLowerCase" => Some(r"\p{Lowercase}"),
        "javaUpperCase" => Some(r"\p{Uppercase}"),
        "javaAlphabetic" => Some(r"\p{Alphabetic}"),
        "javaIdeographic" => Some(r"\p{Ideographic}"),
        "javaTitleCase" => Some(r"\p{Lt}"),
        "javaDigit" => Some(r"\p{Nd}"),
        // isDefined = NOT unassigned (Cn) — enumerate every other general
        // category rather than relying on `--`/negation-inside-brackets.
        "javaDefined" => Some(r"\p{L}\p{M}\p{N}\p{P}\p{S}\p{Z}\p{Cc}\p{Cf}\p{Co}\p{Cs}"),
        "javaLetter" => Some(r"\p{L}"),
        "javaLetterOrDigit" => Some(r"\p{L}\p{Nd}"),
        // isJavaIdentifierStart: isLetter || LETTER_NUMBER || currency
        // symbol || connector punctuation.
        "javaJavaIdentifierStart" => Some(r"\p{L}\p{Nl}\p{Sc}\p{Pc}"),
        // isJavaIdentifierPart: the above plus digit, combining/non-spacing
        // marks, and isIdentifierIgnorable.
        "javaJavaIdentifierPart" => {
            Some(r"\p{L}\p{Nl}\p{Nd}\p{Mc}\p{Mn}\p{Sc}\p{Pc}\p{Cf}\x00-\x08\x0E-\x1B\x7F-\x9F")
        }
        // isUnicodeIdentifierStart: ID_Start plus VERTICAL TILDE (U+2E2F,
        // kept for backward compatibility per the UAX31 profile in the
        // javadoc).
        "javaUnicodeIdentifierStart" => Some(r"\p{ID_Start}\u{2E2F}"),
        // isUnicodeIdentifierPart: ID_Start + ID_Continue + U+2E2F +
        // isIdentifierIgnorable.
        "javaUnicodeIdentifierPart" => {
            Some(r"\p{ID_Start}\p{ID_Continue}\u{2E2F}\p{Cf}\x00-\x08\x0E-\x1B\x7F-\x9F")
        }
        // isIdentifierIgnorable: non-whitespace ISO control ranges plus the
        // FORMAT (Cf) general category.
        "javaIdentifierIgnorable" => Some(r"\x00-\x08\x0E-\x1B\x7F-\x9F\p{Cf}"),
        "javaSpaceChar" => Some(r"\p{Z}"),
        // isWhitespace: see the function-level doc comment above.
        "javaWhitespace" => Some(
            r"\x09-\x0D\x1C-\x1F\x20\u{1680}\u{2000}-\u{2006}\u{2008}-\u{200A}\u{2028}\u{2029}\u{205F}\u{3000}",
        ),
        "javaISOControl" => Some(r"\x00-\x1F\x7F-\x9F"),
        "javaMirrored" => Some(r"\p{Bidi_Mirrored}"),
        _ => None,
    }
}


#[inline]
pub fn utf8_char_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b < 0xC0 {
        1
    }
    // continuation byte (shouldn't happen at boundary)
    else if b < 0xE0 {
        2
    } else if b < 0xF0 {
        3
    } else {
        4
    }
}

pub fn native_standard_location_compute_is_module_oriented_location(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let name_obj = obj_arg(args, 0)?;
    let name = ctx.read_string(name_obj).unwrap_or_default();
    Ok(Some(Value::Int((name == "MODULE") as i32)))
}

/// Spring's forked compiler-test launcher sets the thread context class loader
/// immediately before rediscovering the test. CratonVM can otherwise reuse the
/// parent test class from its class cache, making Spring's original
/// `testClass.getClassLoader()` check false and recursively launching the same
/// test. The context loader is the authoritative state for that child run.
/// Outside that state this delegates to the original private method.
pub fn native_spring_is_using_forked_class_path_loader(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let this_pin = ctx.pin_native_root(this);
    let is_forked = (|| {
        let thread = ctx
            .invoke(
                "java/lang/Thread",
                "currentThread",
                "()Ljava/lang/Thread;",
                &[],
            )
            .ok()
            .flatten();
        let Some(Value::Object(Some(thread))) = thread else {
            return false;
        };
        let thread_pin = ctx.pin_native_root(thread);
        let loader = ctx
            .invoke(
                "java/lang/Thread",
                "getContextClassLoader",
                "()Ljava/lang/ClassLoader;",
                &[Value::Object(Some(thread))],
            )
            .ok()
            .flatten();
        ctx.unpin_native_roots(thread_pin);
        matches!(loader, Some(Value::Object(Some(loader)))
            if ctx.class_name_arc_of_id(ctx.class_id_of_object(loader)).as_deref()
                == Some("org/springframework/core/test/tools/CompileWithForkedClassLoaderClassLoader"))
    })();
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    if is_forked {
        return Ok(Some(Value::Int(1)));
    }
    ctx.invoke_virtual_bytecode_only(
        this,
        "isUsingForkedClassPathLoader",
        "(Lorg/junit/jupiter/api/extension/ExtensionContext;)Z",
        &args[1..],
    )
}

// `r3_br_pending_chars` (a `BufferedReader` pushed-back-char table keyed by
// bare identity hash) was deleted by gc-common w20-c: nothing read or wrote it.

// --- Matcher natives ---
//
// CURRENTLY UNREACHABLE IN DEFAULT (real-JDK) BUILDS. `register()`
// (`native-api/src/registry.rs`) silently drops every
// `java/util/regex/Pattern`/`Matcher` registration when
// `drop_real_layout_synthetic` is set — which `vm/src/vm/vm_init.rs` does
// unconditionally on the real-JDK arm, before `register_essential_natives`
// runs. Rationale (`registry.rs`): these natives predate the real JDK's
// actual field layout and corrupt real-JDK-allocated objects if forced to
// run against them, so in real-JDK mode `Pattern.compile()`/`Matcher.find()`
// /`group()` always run the REAL loaded bytecode instead — confirmed at
// runtime via unconditional instrumentation (an `eprintln!` at the top of
// `native_matcher_find`/`native_matcher_group_idx` never fired for a real-JDK
// `Pattern`/`Matcher` program).
//
// The functions below fix a genuine O(n^2) bug in THIS native bridge (full
// input redecode per `find()`/`group()` call, see
// `matcher-native-full-input-redecode-quadratic-FIXED.md` for
// the corrected writeup) — but because the bridge is dropped by default,
// this fix currently has NO effect on any real-JDK program. It's kept
// in case `drop_real_layout_synthetic` is ever narrowed (e.g. once the
// legacy-layout/real-layout mismatch above is fixed at the root) or a
// non-default config re-enables this bridge. The ACTUAL performance bug a
// user hits with `Pattern`/`Matcher` on today's default build is upstream of
// here, in the real JDK bytecode CratonVM's interpreter actually runs — see
// `substring-large-parent-quadratic-allocation-FIXED.md`.

/// One cached decode of a `Matcher`'s input `String`, filed under the
/// `Matcher` object's own `(VM, identity hash)` and answered only to the
/// Matcher at [`Self::owner_addr`] (see `regex_matcher::matcher_input_cache`).
pub struct MatcherInputCacheEntry {
    /// The Matcher's address when this row was written. gc-common w31-b
    /// (`common-w28b-remaining-identity-hash-keyed-side-tables`): the hash
    /// alone named every live Matcher of the VM that shared it, so a
    /// colliding Matcher over the same input read the other's capture spans.
    pub owner_addr: usize,
    /// Identity hash of the `MAT_FIELD_INPUT` object this decode was taken
    /// from — like the outer key, GC-move-stable but reassigned to a fresh
    /// value on a genuinely new allocation, so a `reset(CharSequence)` swap
    /// is detected without needing any GC to have happened.
    pub input_identity: i32,
    /// ... and its address (gc-common w31-b), so a different `String` that
    /// happens to share that hash is a miss too.
    pub input_addr: usize,
    pub decoded: std::sync::Arc<str>,
    /// Capture-group spans from the most recent `find()`-produced match on
    /// this `Matcher`, if any — see [`matcher_cache_store_captures`].
    pub captures: Option<MatcherCaptures>,
}

