// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `java.io` stream natives: Pushback streams/readers and Object{Input,Output}Stream stubs.
//!
//! Pure code move out of `phases_late.rs` (no logic, signature or ordering
//! changes). Every registration call site is untouched and the per-phase
//! dispatchers stay in the parent module, so the native registration SEQUENCE
//! is byte-identical to before the split.

use super::*;

// =============================================================================
// PushbackInputStream = 3-field (in=0, buf=1 byte[], pos=2)
// PushbackReader = 3-field (in=0, buf=1 char[], pos=2)
// =============================================================================

pub(crate) fn register_p58_pushback(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let pi = "java/io/PushbackInputStream";
    r.register(
        pi,
        "<init>",
        "(Ljava/io/InputStream;)V",
        p58_pushback_in_init,
    );
    r.register(
        pi,
        "<init>",
        "(Ljava/io/InputStream;I)V",
        p58_pushback_in_init_size,
    );
    r.register(pi, "read", "()I", p58_pushback_in_read);
    r.register(pi, "unread", "(I)V", p58_pushback_in_unread);
    r.register(pi, "available", "()I", p58_pushback_in_available);
    r.register(pi, "close", "()V", p58_pushback_in_close);

    let pr = "java/io/PushbackReader";
    r.register(
        pr,
        "<init>",
        "(Ljava/io/Reader;)V",
        p58_pushback_reader_init,
    );
    r.register(
        pr,
        "<init>",
        "(Ljava/io/Reader;I)V",
        p58_pushback_reader_init_size,
    );
    r.register(pr, "read", "()I", p58_pushback_reader_read);
    r.register(pr, "unread", "(I)V", p58_pushback_reader_unread);
    r.register(pr, "close", "()V", |ctx, args| {
        // `PushbackReader.close()` is `synchronized (lock) { super.close();
        // buf = null; }`, and `FilterReader.close()` is a bare `in.close()`.
        // Nothing on that chain catches, so the delegated failure PROPAGATES.
        // W7-57-close-flush-swallow-sweep.md
        //
        // DEAD BY LAST-WRITE-WINS. `register_p66_pushback_reader` runs later in
        // `register_synthetic_overrides` (phase58 then phase66) and re-registers
        // every `java/io/PushbackReader` triple in this block, so none of the
        // five bodies above dispatches. Left in place rather than deleted
        // because deleting it is a registration change and this lane is not
        // making one; named here so the next reader does not repair a body that
        // cannot run, which is what G4-1 found had already happened once to
        // `p58_pushback_reader_read`.
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(underlying)) = ctx.get_field(this, 0) {
            ctx.invoke_virtual(underlying, "close", "()V", &[])?;
        }
        Ok(None)
    });
    r.set_category(__prev_cat);
}

/// `java.io.PushbackInputStream.close()V`.
///
/// Named rather than inline so the close/after-close contract it establishes is
/// reachable from `mod tests` below; the registration is unchanged.
pub(crate) fn p58_pushback_in_close(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Close by delegating to the underlying stream (field 0).
    //
    // `PushbackInputStream.close()` is `if (in == null) return; in.close();
    // in = null; buf = null;` — it declares `throws IOException` and
    // catches nothing, so the delegated failure PROPAGATES. Dropping it
    // here reported a failed close as a clean one.
    // W7-57-close-flush-swallow-sweep.md
    //
    // 2026-08-17: the delegation propagated but the two NULLINGS were never
    // performed, and they are not bookkeeping — they are the whole of this
    // class's closed state. Without them `read()`, `available()` and
    // `unread()` all kept working after `close()`, and `close()` was not
    // idempotent: a second call re-closed the wrapped stream.
    //
    // Both halves are MEASURED on Eclipse Adoptium 25.0.3+9-LTS
    // (2026-08-17), against an `InputStream` that counts its own calls:
    //
    // ```text
    // pi.close(); pi.close();  -> inner close() count stays 1
    // pi.read()      after close -> java.io.IOException: Stream closed
    // pi.available() after close -> java.io.IOException: Stream closed
    // pi.unread(65)  after close -> java.io.IOException: Stream closed
    // pi.skip(1)     after close -> java.io.IOException: Stream closed
    // inner read()/available() counts after those calls -> UNCHANGED
    // ```
    //
    // That last line is the one that makes this a fabricated success rather
    // than a missing diagnostic: with a sink whose own `close()` is a no-op
    // — `ByteArrayInputStream`, the commonest thing to wrap — a post-close
    // `read()` went on returning real bytes from a stream the caller had
    // closed, and a post-close `unread()` went on mutating its buffer.
    //
    // The closed marker is **slot 1 (`buf`)**, not slot 0 (`in`), even
    // though HotSpot's own early-return keys on `in`. Two reasons, both
    // concrete: HotSpot nulls the pair together so the two markers are
    // interchangeable for this purpose; and slot 0 is legitimately null on a
    // receiver built with no wrapped stream, which is exactly the fixture
    // `pushback_input_stream_p58` in `vm/src/vm/tests.rs` constructs. Keying
    // on `in` would declare that fixture closed before it had been. It is
    // also the convention the `PushbackReader` half of this file already
    // uses (`p66_pushback_reader_closed`), so the two classes now agree.
    let this = obj_arg(args, 0)?;
    if matches!(ctx.get_field(this, 1), Value::Object(None)) {
        // Already closed. HotSpot's `if (in == null) return;` runs BEFORE
        // the delegation, so the wrapped stream is not closed twice.
        return Ok(None);
    }
    // The delegated `close()` is arbitrary Java: it can allocate, collect and
    // move `this` before the field writes below. Pin across it and re-derive.
    let this_pin = ctx.pin_native_root(this);
    if let Value::Object(Some(underlying)) = ctx.get_field(this, 0) {
        ctx.invoke_virtual(underlying, "close", "()V", &[])?;
    }
    let this = ctx.read_native_pin(this_pin, this);
    // After the delegation, and with no `finally` — HotSpot's `in = null;
    // buf = null;` sit after `in.close()` in a straight-line body, so a
    // failed close leaves the stream NOT marked closed there either, and a
    // retry re-attempts it.
    ctx.set_field(this, 0, Value::Object(None));
    ctx.set_field(this, 1, Value::Object(None));
    Ok(None)
}

pub(crate) fn p58_pushback_in_init(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
    // Pin across the buffer alloc below — a moving young GC there would
    // relocate `this` (native stale-local family).
    let this_pin = ctx.pin_native_root(this);
    let buf = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 1);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field(this, 1, Value::Object(Some(buf)));
    ctx.set_field(this, 2, Value::Int(1)); // pos = buf.length means buffer empty
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

pub(crate) fn p58_pushback_in_init_size(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // `*v as usize` below turns a NEGATIVE size into `usize::MAX` and asks
    // `new_array` for a 16-exbibyte buffer. The JDK refuses it outright — see
    // `p58_pushback_size_refusal`.
    if let Some(refused) = p58_pushback_size_refusal(args.get(2)) {
        return Err(refused);
    }
    ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
    let size = match args.get(2) {
        Some(Value::Int(v)) if *v > 0 => *v as usize,
        _ => 1,
    };
    // Pin across the buffer alloc below — a moving young GC there would
    // relocate `this` (native stale-local family).
    let this_pin = ctx.pin_native_root(this);
    let buf = ctx.new_array(cratonvm_types::ArrayElementType::Byte, size);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field(this, 1, Value::Object(Some(buf)));
    ctx.set_field(this, 2, Value::Int(size as i32)); // pos = size means buffer empty
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

/// What `PushbackInputStream.ensureOpen()` throws once `close()` has nulled
/// `buf`.
///
/// **Transcribed, not derived.** Measured on Eclipse Adoptium 25.0.3+9-LTS
/// (2026-08-17): `read()`, `available()`, `unread()` and `skip()` on a closed
/// `PushbackInputStream` all raise `java.io.IOException: Stream closed`.
///
/// That is the SAME string `PushbackReader` uses, which is worth stating
/// explicitly because the two classes are famous in this file for NOT sharing:
/// their pushback-overflow messages are `Push back buffer is full` and
/// `Pushback buffer overflow` respectively. The closed message genuinely
/// coincides; the overflow message genuinely does not. Both facts are
/// measurements, and neither is derivable from the other.
fn p58_pushback_in_closed() -> cratonvm_types::error::MethodCallFailed {
    p66_pushback_reader_closed()
}

pub(crate) fn p58_pushback_in_read(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let pos = match ctx.get_field(this, 2) {
        Value::Int(v) => v,
        _ => 0,
    };
    let buf = match ctx.get_field(this, 1) {
        Value::Object(Some(b)) => b,
        // `close()` nulls slot 1 and both `<init>`s always fill it, so a null
        // buffer here IS the closed state — see the close registration above.
        // The old code fell through to the delegate arm and answered `-1`, i.e.
        // a clean end-of-stream for a stream the caller had closed: the same
        // fabricated-EOF species `native_fis_read` was repaired for one package
        // over. This is the identical discriminator, and identical wording, that
        // `register_p66_pushback_reader`'s `read` already uses on the char side.
        _ => return Err(p58_pushback_in_closed()),
    };
    let buf_len = ctx.array_length(buf) as i32;
    if pos < buf_len {
        // Read from pushback buffer
        let b = ctx.get_array_element(buf, pos as usize);
        ctx.set_field(this, 2, Value::Int(pos + 1));
        return Ok(Some(b));
    }
    // Read from underlying stream
    if let Value::Object(Some(is)) = ctx.get_field(this, 0) {
        let b = ctx.invoke_virtual(is, "read", "()I", &[])?;
        Ok(b.or(Some(Value::Int(-1))))
    } else {
        Ok(Some(Value::Int(-1)))
    }
}

pub(crate) fn p58_pushback_in_unread(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let byte_val = args.get(1).copied().unwrap_or(Value::Int(0));
    let pos = match ctx.get_field(this, 2) {
        Value::Int(v) => v,
        _ => 0,
    };
    // CLOSED is checked BEFORE the overflow test, because `ensureOpen()` is the
    // first statement of the JDK's `unread` and a closed stream with a full
    // buffer would otherwise report the wrong one of two IOExceptions. Measured:
    // `pi.unread(65)` after `close()` is `IOException: Stream closed`, not
    // `Push back buffer is full`. The `PushbackReader` half of this file already
    // orders it this way and says so at its own site.
    let buf = match ctx.get_field(this, 1) {
        Value::Object(Some(b)) => b,
        _ => return Err(p58_pushback_in_closed()),
    };
    if pos <= 0 {
        // FABRICATED SUCCESS. `pos == 0` means the pushback buffer is FULL, and
        // this body's `if pos > 0 { … }` had no `else`: the byte the caller
        // pushed back was DISCARDED and `unread` returned normally. A parser
        // that pushes back a lookahead byte it has just decided it cannot
        // consume then reads the NEXT byte instead, silently losing one byte of
        // input with no diagnostic anywhere.
        //
        // HotSpot raises `java.io.IOException` (measured on Adoptium 25.0.3+9,
        // 2026-08-16). The message is `PushbackInputStream`'s own and is NOT the
        // one `PushbackReader` uses ("Pushback buffer overflow") — transcribed
        // per class, not unified.
        // G4-1-the-io-and-nio-fabricated-success-sweep-measured-20260816.md
        return Err(RuntimeError::IOException {
            message: "Push back buffer is full".into(),
        }
        .into());
    }
    let new_pos = pos - 1;
    ctx.set_array_element(buf, new_pos as usize, byte_val);
    ctx.set_field(this, 2, Value::Int(new_pos));
    Ok(None)
}

pub(crate) fn p58_pushback_in_available(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let pos = match ctx.get_field(this, 2) {
        Value::Int(v) => v,
        _ => 0,
    };
    // `available()` is `ensureOpen(); return (buf.length - pos) + in.available();`
    // — the closed check is first, and a closed stream answers
    // `IOException: Stream closed` rather than a number. `0` was the worse of
    // the two possible wrong answers here: it is a legal reading of an OPEN
    // stream ("nothing buffered right now"), so a poller could not tell the two
    // apart. Measured 2026-08-17; the post-close call also does NOT reach the
    // wrapped stream, which the old body did.
    let buf_avail = match ctx.get_field(this, 1) {
        Value::Object(Some(buf)) => ctx.array_length(buf) as i32 - pos,
        _ => return Err(p58_pushback_in_closed()),
    };
    let stream_avail = if let Value::Object(Some(is)) = ctx.get_field(this, 0) {
        match ctx.invoke_virtual(is, "available", "()I", &[])? {
            Some(Value::Int(v)) => v,
            _ => 0,
        }
    } else {
        0
    };
    Ok(Some(Value::Int(buf_avail + stream_avail)))
}

pub(crate) fn p58_pushback_reader_init(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
    // Pin across the buffer alloc below — a moving young GC there would
    // relocate `this` (native stale-local family).
    let this_pin = ctx.pin_native_root(this);
    let buf = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field(this, 1, Value::Object(Some(buf)));
    ctx.set_field(this, 2, Value::Int(1));
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

pub(crate) fn p58_pushback_reader_init_size(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // As in `p58_pushback_in_init_size`: a negative size became `usize::MAX`.
    if let Some(refused) = p58_pushback_size_refusal(args.get(2)) {
        return Err(refused);
    }
    ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
    let size = match args.get(2) {
        Some(Value::Int(v)) if *v > 0 => *v as usize,
        _ => 1,
    };
    // Pin across the buffer alloc below — a moving young GC there would
    // relocate `this` (native stale-local family).
    let this_pin = ctx.pin_native_root(this);
    let buf = ctx.new_array(cratonvm_types::ArrayElementType::Reference, size);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field(this, 1, Value::Object(Some(buf)));
    ctx.set_field(this, 2, Value::Int(size as i32));
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

pub(crate) fn p58_pushback_reader_read(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let pos = match ctx.get_field(this, 2) {
        Value::Int(v) => v,
        _ => 0,
    };
    if let Value::Object(Some(buf)) = ctx.get_field(this, 1) {
        let buf_len = ctx.array_length(buf) as i32;
        if pos < buf_len {
            let ch = ctx.get_array_element(buf, pos as usize);
            ctx.set_field(this, 2, Value::Int(pos + 1));
            return Ok(Some(ch));
        }
    }
    if let Value::Object(Some(reader)) = ctx.get_field(this, 0) {
        let ch = ctx.invoke_virtual(reader, "read", "()I", &[])?;
        Ok(ch.or(Some(Value::Int(-1))))
    } else {
        Ok(Some(Value::Int(-1)))
    }
}

pub(crate) fn p58_pushback_reader_unread(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let ch_val = args.get(1).copied().unwrap_or(Value::Int(0));
    let pos = match ctx.get_field(this, 2) {
        Value::Int(v) => v,
        _ => 0,
    };
    if pos <= 0 {
        // Same silent discard as `p58_pushback_in_unread` above, on the char
        // side, with `PushbackReader`'s own message (measured, and different
        // from `PushbackInputStream`'s).
        // G4-1-the-io-and-nio-fabricated-success-sweep-measured-20260816.md
        return Err(RuntimeError::IOException {
            message: "Pushback buffer overflow".into(),
        }
        .into());
    }
    let new_pos = pos - 1;
    if let Value::Object(Some(buf)) = ctx.get_field(this, 1) {
        ctx.set_array_element(buf, new_pos as usize, ch_val);
    }
    ctx.set_field(this, 2, Value::Int(new_pos));
    Ok(None)
}

// =============================================================================
// PushbackReader = 3-field (reader=0, buf=1 char[], pos=2)
// =============================================================================

/// What `PushbackReader.ensureOpen()` throws once `close()` has nulled `buf`.
///
/// **Transcribed, not derived.** Measured on Eclipse Adoptium 25.0.3+9-LTS
/// (2026-08-16): `read()`, `ready()` and `unread()` on a closed
/// `PushbackReader` all raise `java.io.IOException: Stream closed` — a
/// lower-case `c`, where `FileInputStream`/`FileOutputStream` in the same run
/// say `"Stream Closed"`. The two strings must not be shared.
fn p66_pushback_reader_closed() -> cratonvm_types::error::MethodCallFailed {
    RuntimeError::IOException {
        message: "Stream closed".into(),
    }
    .into()
}

/// The refusal both pushback constructors owe a non-positive buffer size.
///
/// Measured on Eclipse Adoptium 25.0.3+9-LTS (2026-08-16) — all four spellings
/// agree, and the string carries no size:
///
/// ```text
/// new PushbackReader(r, 0)       -> IllegalArgumentException: size <= 0
/// new PushbackReader(r, -1)      -> IllegalArgumentException: size <= 0
/// new PushbackInputStream(i, 0)  -> IllegalArgumentException: size <= 0
/// new PushbackInputStream(i, -1) -> IllegalArgumentException: size <= 0
/// ```
///
/// The two constructors this replaces disagreed with each other and both were
/// wrong: `register_p66_pushback_reader` clamped with `(*v).max(1)`, silently
/// giving a caller who asked for zero pushback a one-character buffer, and
/// `p58_pushback_reader_init_size` did `*v as usize`, which turns `-1` into
/// `usize::MAX` and asks the allocator for a 16-exbibyte array.
fn p58_pushback_size_refusal(
    size: Option<&Value>,
) -> Option<cratonvm_types::error::MethodCallFailed> {
    match size {
        Some(Value::Int(v)) if *v <= 0 => Some(
            RuntimeError::IllegalArgumentException {
                message: "size <= 0".into(),
            }
            .into(),
        ),
        _ => None,
    }
}

pub(crate) fn register_p66_pushback_reader(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let pr = "java/io/PushbackReader";
    r.register(pr, "<init>", "(Ljava/io/Reader;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
        let buf = ctx.new_array(cratonvm_types::ArrayElementType::Char, 1);
        ctx.set_field(this, 1, Value::Object(Some(buf)));
        ctx.set_field(this, 2, Value::Int(1)); // pos = buf.length means no pushback data
        Ok(None)
    });
    r.register(pr, "<init>", "(Ljava/io/Reader;I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Refuse before anything is stored — the JDK's check is the first
        // statement of the constructor. See `p58_pushback_size_refusal`.
        if let Some(refused) = p58_pushback_size_refusal(args.get(2)) {
            return Err(refused);
        }
        ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
        let size = match args.get(2) {
            Some(Value::Int(v)) if *v > 0 => *v as usize,
            _ => 1,
        };
        let buf = ctx.new_array(cratonvm_types::ArrayElementType::Char, size);
        ctx.set_field(this, 1, Value::Object(Some(buf)));
        ctx.set_field(this, 2, Value::Int(size as i32));
        Ok(None)
    });
    r.register(pr, "read", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = match ctx.get_field(this, 2) {
            Value::Int(v) => v as usize,
            _ => 0,
        };
        let buf = match ctx.get_field(this, 1) {
            // `close()` below nulls slot 1, and `<init>` always fills it, so a
            // null buffer here IS the closed state. HotSpot's `ensureOpen()`
            // raises `IOException("Stream closed")` (measured on Adoptium
            // 25.0.3+9, 2026-08-16 — lower-case `c`, unlike
            // `FileInputStream`'s "Stream Closed").
            Value::Object(Some(b)) => b,
            _ => return Err(p66_pushback_reader_closed()),
        };
        let buf_len = ctx.array_length(buf);
        if pos < buf_len {
            // Read from pushback buffer
            let ch = ctx.get_array_element(buf, pos);
            ctx.set_field(this, 2, Value::Int((pos + 1) as i32));
            return Ok(Some(ch));
        }
        // FABRICATED EOF. This arm used to be `Ok(Some(Value::Int(-1)))` with
        // the comment "simplified — real impl would delegate to inner reader",
        // and it is the whole class: a `PushbackReader` starts with an EMPTY
        // pushback buffer, so the very first `read()` took this arm and
        // answered `-1`. Every `while ((c = r.read()) != -1)` over a
        // `PushbackReader` therefore terminated immediately and read NOTHING,
        // successfully. Measured on HotSpot: a fresh
        // `PushbackReader(new StringReader("abc"))` answers 97, 98, 99, -1.
        //
        // This registrar runs AFTER `register_p58_pushback` inside
        // `register_synthetic_overrides` (lib.rs: phase58 then phase66) and
        // `register()` is last-write-wins, so it is THIS body that dispatches
        // and the correct sibling twenty lines away in the same file
        // (`p58_pushback_reader_read`, which does delegate) never ran. The
        // "scripted edit landed on the wrong twin" shape, arrived at by hand.
        // G4-1-the-io-and-nio-fabricated-success-sweep-measured-20260816.md
        if let Value::Object(Some(reader)) = ctx.get_field(this, 0) {
            let ch = ctx.invoke_virtual(reader, "read", "()I", &[])?;
            return Ok(ch.or(Some(Value::Int(-1))));
        }
        Ok(Some(Value::Int(-1)))
    });
    r.register(pr, "unread", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ch = args.get(1).copied().unwrap_or(Value::Int(0));
        let pos = match ctx.get_field(this, 2) {
            Value::Int(v) => v as usize,
            _ => 0,
        };
        let buf = match ctx.get_field(this, 1) {
            Value::Object(Some(b)) => b,
            // Closed — checked BEFORE the overflow test, as `ensureOpen()` is
            // the first statement of the JDK's `unread`.
            _ => return Err(p66_pushback_reader_closed()),
        };
        if pos == 0 {
            // WRONG TYPE, corrected. HotSpot raises `java.io.IOException`
            // ("Pushback buffer overflow" — measured), and
            // `IllegalStateException` is NOT an `IOException`, so the
            // `catch (IOException)` every caller of a `Reader` writes did not
            // match and the failure escaped as an unchecked exception out of a
            // method declared `throws IOException`.
            //
            // The sibling class does NOT share this string:
            // `PushbackInputStream` says "Push back buffer is full" (measured
            // in the same run). Transcribed per class, not unified.
            return Err(RuntimeError::IOException {
                message: "Pushback buffer overflow".into(),
            }
            .into());
        }
        ctx.set_array_element(buf, pos - 1, ch);
        ctx.set_field(this, 2, Value::Int((pos - 1) as i32));
        Ok(None)
    });
    r.register(pr, "ready", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = match ctx.get_field(this, 2) {
            Value::Int(v) => v as usize,
            _ => 0,
        };
        let buf = match ctx.get_field(this, 1) {
            Value::Object(Some(b)) => b,
            _ => return Err(p66_pushback_reader_closed()),
        };
        let buf_len = ctx.array_length(buf);
        if pos < buf_len {
            return Ok(Some(Value::Int(1)));
        }
        // Same fabrication as `read` above, one method over: with the pushback
        // buffer empty the JDK answers `in.ready()`, and this answered a flat
        // `false`. Measured: `pr.ready()` is `true` even at end-of-input on a
        // `StringReader`, because `StringReader.ready()` is unconditionally
        // true — so "false" was never merely conservative.
        if let Value::Object(Some(reader)) = ctx.get_field(this, 0) {
            let ready = ctx.invoke_virtual(reader, "ready", "()Z", &[])?;
            return Ok(ready.or(Some(Value::Int(0))));
        }
        Ok(Some(Value::Int(0)))
    });
    r.register(pr, "close", "()V", p66_pushback_reader_close);
    r.set_category(__prev_cat);
}

/// `java.io.PushbackReader.close()V` — the body that actually dispatches (this
/// registrar runs after `register_p58_pushback` and overwrites its triple).
///
/// Named rather than inline so `mod tests` below can pin the double-close
/// contract, which is the half that differs from the sibling class.
pub(crate) fn p66_pushback_reader_close(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // Delegate close() to the underlying Reader (field 0) if present, then
    // clear our buffer.
    //
    // `PushbackReader.close()` = `synchronized (lock) { super.close();
    // buf = null; }` over `FilterReader.close()`'s bare `in.close()`:
    // nothing catches, so the delegated failure PROPAGATES — and the
    // buffer clear sits AFTER it in HotSpot too (no `finally`), so a
    // failed close leaves the reader unclosed there as well.
    // W7-57-close-flush-swallow-sweep.md
    //
    // SLOT 0 IS DELIBERATELY NOT NULLED, and the two sibling classes in this
    // file genuinely disagree about this. `PushbackInputStream.close()`
    // nulls `in` and early-returns on it, so it is idempotent; the JDK's
    // `PushbackReader.close()` never touches `in` at all, so a second
    // `close()` calls `in.close()` AGAIN. Measured on Eclipse Adoptium
    // 25.0.3+9-LTS (2026-08-17), against a `Reader` that counts its closes:
    //
    // ```text
    // pr.close(); pr.close();  -> inner close() count = 2
    // pi.close(); pi.close();  -> inner close() count = 1
    // ```
    //
    // Nulling slot 0 here made the second close a silent no-op — which is
    // "correct-looking" and wrong, and matters for a wrapped stream whose
    // `close()` is not idempotent (a counting or refcounting sink sees one
    // release where HotSpot delivers two). The closed marker stays slot 1,
    // which `read`/`ready`/`unread` above already key on, so refusing the
    // post-close surface does not depend on slot 0 being cleared.
    // The delegated `close()` is arbitrary Java and can move `this` before the
    // closed-marker write below. Pin across it and re-derive.
    let this_pin = ctx.pin_native_root(this);
    if let Value::Object(Some(inner)) = ctx.get_field(this, 0) {
        ctx.invoke_virtual(inner, "close", "()V", &[])?;
    }
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field(this, 1, Value::Object(None));
    Ok(None)
}

/// Read a single byte from an InputStream. Returns -1 on EOF.
pub(crate) fn ois_read_byte(ctx: &mut dyn NativeContext, stream: ObjectRef) -> i32 {
    // NOTE ON THE SWALLOW HERE — deliberate, and narrowed rather than removed.
    //
    // `_ => -1` maps a THROWN `IOException` from the underlying stream onto the
    // same `-1` that means end-of-stream, so a serialization read over a
    // failing socket reported a truncated-but-clean object graph. That is this
    // record's species. It is not repaired here because the repair is a
    // signature change: `ois_read_byte` returns a bare `i32` and its eleven
    // call sites (`ois_read_n`, `readObject`, `readInt`, `readLong`, `readUTF`,
    // `readBoolean`, `readDouble`, `readFloat`, `readByte`, `readChar`,
    // `readShort`) all consume it as one, so propagating means touching all of
    // them, and this whole `ObjectInputStream` fallback is a
    // `--synthetic-jdk`-only shape (`register_p70_object_streams` is in
    // `SYNTHETIC_ONLY_CLOSURE`) whose wire format is already a stub.
    //
    // A SENTINEL WAS TRIED HERE AND WITHDRAWN, which is worth recording because
    // it is the cheap-looking wrong answer. Returning `i32::MIN` for the `Err`
    // arm keeps every `if b < 0 { break }` correct — but `readByte` does
    // `b as i8 as i32`, and `i32::MIN as i8` is `0`, so the sentinel silently
    // turned a failed read into the byte `0` at one of the four call sites.
    // Half-distinguishing a failure is worse than naming it, so the swallow
    // stays whole and is named instead. Recorded in the "What this lane did NOT
    // do" section of
    // G4-1-the-io-and-nio-fabricated-success-sweep-measured-20260816.md.
    match ctx.invoke_virtual(stream, "read", "()I", &[]) {
        Ok(Some(Value::Int(b))) => b,
        _ => -1,
    }
}

/// Read exactly n bytes from an InputStream. Returns empty vec on EOF.
pub(crate) fn ois_read_n(ctx: &mut dyn NativeContext, stream: ObjectRef, n: usize) -> Vec<u8> {
    // Pin across the read callbacks below — a moving young GC there would
    // relocate the stream (native stale-local family).
    let stream_pin = ctx.pin_native_root(stream);
    let mut result = Vec::with_capacity(n);
    for _ in 0..n {
        let stream = ctx.read_native_pin(stream_pin, stream);
        let b = ois_read_byte(ctx, stream);
        if b < 0 {
            break;
        }
        result.push(b as u8);
    }
    ctx.unpin_native_roots(stream_pin);
    result
}

#[cfg(not(feature = "experimental-serialization"))]
pub(crate) fn register_p70_object_streams(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // ObjectOutputStream — field 0 = underlying OutputStream
    let oos = "java/io/ObjectOutputStream";
    r.register(oos, "<init>", "(Ljava/io/OutputStream;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
        // Write Java serialization magic + version header
        if let Value::Object(Some(stream)) = args.get(1).copied().unwrap_or(Value::Object(None)) {
            // Magic: 0xACED, Version: 0x0005
            oos_write_bytes(ctx, stream, &[0xAC, 0xED, 0x00, 0x05]);
        }
        Ok(None)
    });
    r.register(oos, "writeObject", "(Ljava/lang/Object;)V", |ctx, args| {
        // Simplified: write TC_NULL (0x70) for null, or a marker for non-null
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            match args.get(1) {
                Some(Value::Object(None)) | None => {
                    oos_write_bytes(ctx, stream, &[0x70]); // TC_NULL
                }
                Some(Value::Object(Some(obj))) => {
                    // TC_STRING (0x74) for String objects, TC_OBJECT (0x73) for others
                    if let Some(s) = ctx.read_string(*obj) {
                        let bytes = s.as_bytes();
                        oos_write_bytes(ctx, stream, &[0x74]); // TC_STRING
                        oos_write_bytes(ctx, stream, &(bytes.len() as u16).to_be_bytes());
                        oos_write_bytes(ctx, stream, bytes);
                    } else {
                        oos_write_bytes(ctx, stream, &[0x73]); // TC_OBJECT (simplified)
                    }
                }
                _ => {}
            }
        }
        Ok(None)
    });
    r.register(oos, "writeInt", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let val = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            oos_write_bytes(ctx, stream, &val.to_be_bytes());
        }
        Ok(None)
    });
    r.register(oos, "writeLong", "(J)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let val = match args.get(1) {
            Some(Value::Long(v)) => *v,
            _ => 0,
        };
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            oos_write_bytes(ctx, stream, &val.to_be_bytes());
        }
        Ok(None)
    });
    r.register(oos, "writeUTF", "(Ljava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let s = match args.get(1) {
            Some(Value::Object(Some(r))) => ctx.read_string(*r).unwrap_or_default(),
            _ => String::new(),
        };
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            let bytes = s.as_bytes();
            oos_write_bytes(ctx, stream, &(bytes.len() as u16).to_be_bytes());
            oos_write_bytes(ctx, stream, bytes);
        }
        Ok(None)
    });
    r.register(oos, "writeBoolean", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let val = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            oos_write_bytes(ctx, stream, &[if val != 0 { 1 } else { 0 }]);
        }
        Ok(None)
    });
    r.register(oos, "writeDouble", "(D)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let val = match args.get(1) {
            Some(Value::Double(v)) => *v,
            _ => 0.0,
        };
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            oos_write_bytes(ctx, stream, &val.to_be_bytes());
        }
        Ok(None)
    });
    r.register(oos, "writeFloat", "(F)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let val = match args.get(1) {
            Some(Value::Float(v)) => *v,
            _ => 0.0,
        };
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            oos_write_bytes(ctx, stream, &val.to_be_bytes());
        }
        Ok(None)
    });
    r.register(oos, "writeByte", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let val = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as u8;
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            oos_write_bytes(ctx, stream, &[val]);
        }
        Ok(None)
    });
    r.register(oos, "writeChar", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let val = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as u16;
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            oos_write_bytes(ctx, stream, &val.to_be_bytes());
        }
        Ok(None)
    });
    r.register(oos, "writeShort", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let val = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as i16;
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            oos_write_bytes(ctx, stream, &val.to_be_bytes());
        }
        Ok(None)
    });
    r.register(oos, "writeBytes", "(Ljava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let s = match args.get(1) {
            Some(Value::Object(Some(r))) => ctx.read_string(*r).unwrap_or_default(),
            _ => String::new(),
        };
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            oos_write_bytes(ctx, stream, s.as_bytes());
        }
        Ok(None)
    });
    r.register(oos, "flush", "()V", |ctx, args| {
        // `ObjectOutputStream.flush()` is a bare `bout.flush()` under
        // `throws IOException` — nothing catches, so it PROPAGATES. A dropped
        // failure here is the worst shape in the set: the caller flushed
        // precisely to learn whether the bytes landed.
        // W7-57-close-flush-swallow-sweep.md
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            ctx.invoke_virtual(stream, "flush", "()V", &[])?;
        }
        Ok(None)
    });
    r.register(oos, "close", "()V", |ctx, args| {
        // `ObjectOutputStream.close()` is `flush(); clear(); bout.close();` —
        // both delegations PROPAGATE, and the close is skipped when the flush
        // throws, exactly as the JDK's straight-line body does.
        // W7-57-close-flush-swallow-sweep.md
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            // `flush()` is arbitrary Java and can move `stream` before the
            // `close()` below dereferences it. Pin across, then re-derive.
            let s_pin = ctx.pin_native_root(stream);
            ctx.invoke_virtual(stream, "flush", "()V", &[])?;
            let stream = ctx.read_native_pin(s_pin, stream);
            ctx.invoke_virtual(stream, "close", "()V", &[])?;
        }
        Ok(None)
    });
    r.register(oos, "defaultWriteObject", "()V", |_ctx, _args| {
        // `defaultWriteObject()` writes the calling class's default-serializable
        // fields. This fallback OOS has no class-descriptor machinery to walk
        // them with, and it never enters a per-class `writeObject` callback, so
        // a no-op here silently DROPPED every field of every class that used the
        // normal `writeObject`/`defaultWriteObject` idiom. `NotActiveException`
        // is not a substitute for the real thing — it is what the real JDK
        // throws in exactly this state ("not in call to writeObject"), so the
        // caller learns the truth instead of shipping an empty object.
        Err(RuntimeError::IOException {
            message: "java.io.NotActiveException: not in call to writeObject".into(),
        }
        .into())
    });
    r.register(oos, "reset", "()V", |ctx, args| {
        // `reset()` is a WIRE-FORMAT operation, not just a bookkeeping one: the
        // JDK emits TC_RESET so the paired reader drops its back-reference
        // table at the same point in the byte stream. Skipping the byte left the
        // two sides disagreeing about stream position. (The matching TC_RESET
        // skip is in `ObjectInputStream.readObject` below.)
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            oos_write_bytes(ctx, stream, &[0x79]); // TC_RESET
        }
        Ok(None)
    });

    // ObjectInputStream — field 0 = underlying InputStream
    let ois = "java/io/ObjectInputStream";
    r.register(ois, "<init>", "(Ljava/io/InputStream;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
        // Read and validate magic + version header
        if let Value::Object(Some(stream)) = args.get(1).copied().unwrap_or(Value::Object(None)) {
            let header = ois_read_n(ctx, stream, 4);
            if header.len() == 4 && (header[0] != 0xAC || header[1] != 0xED) {
                // Not a valid serialization stream — but don't error, just proceed
            }
        }
        Ok(None)
    });
    r.register(ois, "readObject", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            // TC_RESET carries no payload — skip it and read the next item, or
            // `ObjectOutputStream.reset()`'s marker would be mis-read as an
            // unknown type code and answered with a bogus null.
            let mut tc = ois_read_byte(ctx, stream);
            while tc == 0x79 {
                tc = ois_read_byte(ctx, stream);
            }
            match tc {
                0x70 => Ok(Some(Value::Object(None))), // TC_NULL
                0x74 => {
                    // TC_STRING: read 2-byte length + UTF-8 bytes
                    let len_bytes = ois_read_n(ctx, stream, 2);
                    if len_bytes.len() == 2 {
                        let len = u16::from_be_bytes([len_bytes[0], len_bytes[1]]) as usize;
                        let str_bytes = ois_read_n(ctx, stream, len);
                        let s = String::from_utf8_lossy(&str_bytes).to_string();
                        let obj = ctx.create_string(&s);
                        Ok(Some(Value::Object(Some(obj))))
                    } else {
                        Ok(Some(Value::Object(None)))
                    }
                }
                _ => Ok(Some(Value::Object(None))),
            }
        } else {
            Ok(Some(Value::Object(None)))
        }
    });
    r.register(ois, "readInt", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            let bytes = ois_read_n(ctx, stream, 4);
            if bytes.len() == 4 {
                Ok(Some(Value::Int(i32::from_be_bytes([
                    bytes[0], bytes[1], bytes[2], bytes[3],
                ]))))
            } else {
                Ok(Some(Value::Int(0)))
            }
        } else {
            Ok(Some(Value::Int(0)))
        }
    });
    r.register(ois, "readLong", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            let bytes = ois_read_n(ctx, stream, 8);
            if bytes.len() == 8 {
                Ok(Some(Value::Long(i64::from_be_bytes([
                    bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
                ]))))
            } else {
                Ok(Some(Value::Long(0)))
            }
        } else {
            Ok(Some(Value::Long(0)))
        }
    });
    r.register(ois, "readUTF", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            let len_bytes = ois_read_n(ctx, stream, 2);
            if len_bytes.len() == 2 {
                let len = u16::from_be_bytes([len_bytes[0], len_bytes[1]]) as usize;
                let str_bytes = ois_read_n(ctx, stream, len);
                let s = String::from_utf8_lossy(&str_bytes).to_string();
                let obj = ctx.create_string(&s);
                Ok(Some(Value::Object(Some(obj))))
            } else {
                let obj = ctx.create_string("");
                Ok(Some(Value::Object(Some(obj))))
            }
        } else {
            let obj = ctx.create_string("");
            Ok(Some(Value::Object(Some(obj))))
        }
    });
    r.register(ois, "readBoolean", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            let b = ois_read_byte(ctx, stream);
            Ok(Some(Value::Int(if b != 0 { 1 } else { 0 })))
        } else {
            Ok(Some(Value::Int(0)))
        }
    });
    r.register(ois, "readDouble", "()D", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            let bytes = ois_read_n(ctx, stream, 8);
            if bytes.len() == 8 {
                Ok(Some(Value::Double(f64::from_be_bytes([
                    bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
                ]))))
            } else {
                Ok(Some(Value::Double(0.0)))
            }
        } else {
            Ok(Some(Value::Double(0.0)))
        }
    });
    r.register(ois, "readFloat", "()F", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            let bytes = ois_read_n(ctx, stream, 4);
            if bytes.len() == 4 {
                Ok(Some(Value::Float(f32::from_be_bytes([
                    bytes[0], bytes[1], bytes[2], bytes[3],
                ]))))
            } else {
                Ok(Some(Value::Float(0.0)))
            }
        } else {
            Ok(Some(Value::Float(0.0)))
        }
    });
    r.register(ois, "readByte", "()B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            let b = ois_read_byte(ctx, stream);
            Ok(Some(Value::Int(b as i8 as i32)))
        } else {
            Ok(Some(Value::Int(0)))
        }
    });
    r.register(ois, "readChar", "()C", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            let bytes = ois_read_n(ctx, stream, 2);
            if bytes.len() == 2 {
                Ok(Some(Value::Int(
                    u16::from_be_bytes([bytes[0], bytes[1]]) as i32
                )))
            } else {
                Ok(Some(Value::Int(0)))
            }
        } else {
            Ok(Some(Value::Int(0)))
        }
    });
    r.register(ois, "readShort", "()S", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            let bytes = ois_read_n(ctx, stream, 2);
            if bytes.len() == 2 {
                Ok(Some(Value::Int(
                    i16::from_be_bytes([bytes[0], bytes[1]]) as i32
                )))
            } else {
                Ok(Some(Value::Int(0)))
            }
        } else {
            Ok(Some(Value::Int(0)))
        }
    });
    r.register(ois, "close", "()V", |ctx, args| {
        // `ObjectInputStream.close()` ends in a bare `bin.close()` under
        // `throws IOException`, with a comment in the JDK insisting the close
        // be propagated to the underlying stream even when already closed.
        // Nothing catches. W7-57-close-flush-swallow-sweep.md
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            ctx.invoke_virtual(stream, "close", "()V", &[])?;
        }
        Ok(None)
    });
    r.register(ois, "defaultReadObject", "()V", |_ctx, _args| {
        // Mirror of `ObjectOutputStream.defaultWriteObject` above: a no-op here
        // left every default-serializable field at its default value while the
        // caller believed it had been restored. This fallback OIS has no class
        // descriptor to read them from and is never inside a per-class
        // `readObject` callback, which is precisely the state in which the real
        // JDK throws `NotActiveException`.
        Err(RuntimeError::IOException {
            message: "java.io.NotActiveException: not in call to readObject".into(),
        }
        .into())
    });
    // Delegate to the underlying stream, as `close()` above does; a constant 0
    // told every caller the stream was exhausted while bytes were still queued.
    r.register(ois, "available", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            match ctx.invoke_virtual(stream, "available", "()I", &[])? {
                Some(Value::Int(v)) => Ok(Some(Value::Int(v))),
                _ => Ok(Some(Value::Int(0))),
            }
        } else {
            Ok(Some(Value::Int(0)))
        }
    });
    r.set_category(__prev_cat);
}

// =============================================================================
// G39-1 — the close/flush family, measured.
//
// Every expected value below was MEASURED on Eclipse Adoptium 25.0.3+9-LTS,
// Windows, 2026-08-17. See
// docs/known-issues/jdk-only/G39-1-the-close-family-closed-20260817.md.
//
// REACH, stated here so a green run is not over-read: `--dump-native-registry`
// in `--jdk-only` on this tree lists ZERO registrations for
// `java/io/PushbackInputStream`, `java/io/PushbackReader` and
// `java/io/ObjectOutputStream`. All three of this file's registrars are in
// `SYNTHETIC_ONLY_CLOSURE`. These tests pin Compatible/`--synthetic-jdk`
// behaviour and cannot move a `--jdk-only` vector.
// =============================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{mock_ctx, MockNativeContext};
    use cratonvm_native_api::NativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// How many `close()V` calls the hook below has seen. The mock's
    /// `InvokeVirtualHook` is a bare `fn` pointer, so the counter cannot be a
    /// captured local and has to be a static.
    static INNER_CLOSES: AtomicUsize = AtomicUsize::new(0);

    fn count_closes(
        _ctx: &mut MockNativeContext,
        _recv: ObjectRef,
        name: &str,
        desc: &str,
        _args: &[Value],
    ) -> Option<MethodCallResult> {
        if name == "close" && desc == "()V" {
            INNER_CLOSES.fetch_add(1, Ordering::SeqCst);
            return Some(Ok(None));
        }
        None
    }

    /// A 3-field Pushback* receiver in its post-`<init>` shape: an inner stream
    /// at slot 0, a buffer at slot 1, `pos == buf.length` at slot 2.
    fn open_pushback(ctx: &mut MockNativeContext, et: ArrayElementType) -> (ObjectRef, ObjectRef) {
        let inner = ctx.alloc_object(ClassId::new(0), 1);
        let this = ctx.alloc_object(ClassId::new(0), 3);
        let buf = ctx.new_array(et, 1);
        ctx.set_field(this, 0, Value::Object(Some(inner)));
        ctx.set_field(this, 1, Value::Object(Some(buf)));
        ctx.set_field(this, 2, Value::Int(1));
        (this, inner)
    }

    /// `pi.close(); pi.close();` closes the wrapped stream ONCE, and the close
    /// latches the state the post-close surface refuses on.
    #[test]
    fn pushback_input_stream_close_is_idempotent_and_latches() {
        let mut ctx = mock_ctx();
        ctx.set_invoke_virtual_hook(count_closes);
        INNER_CLOSES.store(0, Ordering::SeqCst);
        let (this, _inner) = open_pushback(&mut ctx, ArrayElementType::Byte);

        p58_pushback_in_close(&mut ctx, &[Value::Object(Some(this))]).unwrap();
        assert_eq!(INNER_CLOSES.load(Ordering::SeqCst), 1);
        assert_eq!(
            ctx.get_field(this, 0),
            Value::Object(None),
            "in must be nulled"
        );
        assert_eq!(
            ctx.get_field(this, 1),
            Value::Object(None),
            "buf must be nulled"
        );

        p58_pushback_in_close(&mut ctx, &[Value::Object(Some(this))]).unwrap();
        assert_eq!(
            INNER_CLOSES.load(Ordering::SeqCst),
            1,
            "a second close must NOT re-close the wrapped stream (measured)"
        );
    }

    /// After `close()`, every reading method refuses with
    /// `IOException: Stream closed` instead of answering EOF / 0 / success.
    #[test]
    fn pushback_input_stream_refuses_its_whole_surface_after_close() {
        let mut ctx = mock_ctx();
        ctx.set_invoke_virtual_hook(count_closes);
        INNER_CLOSES.store(0, Ordering::SeqCst);
        let (this, _inner) = open_pushback(&mut ctx, ArrayElementType::Byte);

        // The PAIR, taken first so none of the refusals below can pass
        // vacuously: on an OPEN stream all three answer normally.
        assert!(p58_pushback_in_read(&mut ctx, &[Value::Object(Some(this))]).is_ok());
        assert!(p58_pushback_in_available(&mut ctx, &[Value::Object(Some(this))]).is_ok());
        assert!(
            p58_pushback_in_unread(&mut ctx, &[Value::Object(Some(this)), Value::Int(65)]).is_ok()
        );

        p58_pushback_in_close(&mut ctx, &[Value::Object(Some(this))]).unwrap();

        for (what, res) in [
            (
                "read",
                p58_pushback_in_read(&mut ctx, &[Value::Object(Some(this))]),
            ),
            (
                "available",
                p58_pushback_in_available(&mut ctx, &[Value::Object(Some(this))]),
            ),
            (
                "unread",
                p58_pushback_in_unread(&mut ctx, &[Value::Object(Some(this)), Value::Int(65)]),
            ),
        ] {
            let err = res.expect_err("after close this must raise, not succeed");
            assert!(
                format!("{err:?}").contains("Stream closed"),
                "{what} after close raised the wrong thing: {err:?}"
            );
        }
        // And the refusals did NOT reach the wrapped stream: only the close did.
        assert_eq!(INNER_CLOSES.load(Ordering::SeqCst), 1);
    }

    /// The closed check runs BEFORE the pushback-overflow check, so a closed
    /// stream with a full buffer reports `Stream closed`, not
    /// `Push back buffer is full`.
    #[test]
    fn pushback_input_stream_closed_beats_overflow() {
        let mut ctx = mock_ctx();
        ctx.set_invoke_virtual_hook(count_closes);
        INNER_CLOSES.store(0, Ordering::SeqCst);
        let (this, _inner) = open_pushback(&mut ctx, ArrayElementType::Byte);
        // Fill the 1-slot pushback buffer, so `pos == 0` == overflow-on-next.
        p58_pushback_in_unread(&mut ctx, &[Value::Object(Some(this)), Value::Int(65)]).unwrap();
        // The PAIR: while OPEN, that same state really does report overflow.
        let overflow =
            p58_pushback_in_unread(&mut ctx, &[Value::Object(Some(this)), Value::Int(66)])
                .expect_err("a full buffer must refuse");
        assert!(
            format!("{overflow:?}").contains("Push back buffer is full"),
            "wrong overflow message: {overflow:?}"
        );

        p58_pushback_in_close(&mut ctx, &[Value::Object(Some(this))]).unwrap();
        let closed = p58_pushback_in_unread(&mut ctx, &[Value::Object(Some(this)), Value::Int(66)])
            .expect_err("a closed stream must refuse");
        assert!(
            format!("{closed:?}").contains("Stream closed"),
            "closed must win over overflow: {closed:?}"
        );
    }

    /// The sibling class DISAGREES about double close, and that is measured:
    /// `PushbackReader.close()` never nulls `in`, so a second close reaches the
    /// wrapped `Reader` a second time.
    #[test]
    fn pushback_reader_close_reaches_the_inner_reader_twice() {
        let mut ctx = mock_ctx();
        ctx.set_invoke_virtual_hook(count_closes);
        INNER_CLOSES.store(0, Ordering::SeqCst);
        let (this, _inner) = open_pushback(&mut ctx, ArrayElementType::Char);

        p66_pushback_reader_close(&mut ctx, &[Value::Object(Some(this))]).unwrap();
        assert_eq!(INNER_CLOSES.load(Ordering::SeqCst), 1);
        assert_eq!(
            ctx.get_field(this, 1),
            Value::Object(None),
            "buf is the closed marker and must be nulled"
        );
        assert!(
            matches!(ctx.get_field(this, 0), Value::Object(Some(_))),
            "in must NOT be nulled - HotSpot PushbackReader.close() never touches it"
        );

        p66_pushback_reader_close(&mut ctx, &[Value::Object(Some(this))]).unwrap();
        assert_eq!(
            INNER_CLOSES.load(Ordering::SeqCst),
            2,
            "a second PushbackReader.close() DOES re-close the wrapped Reader (measured)"
        );
    }

    /// The two classes' pushback-overflow messages differ and must not be
    /// unified; their closed messages coincide and must not be split. Both are
    /// measurements, and this pins the pair against a well-meaning tidy-up.
    #[test]
    fn pushback_messages_differ_where_measured_and_agree_where_measured() {
        let closed_in = format!("{:?}", p58_pushback_in_closed());
        let closed_rd = format!("{:?}", p66_pushback_reader_closed());
        assert!(closed_in.contains("Stream closed"));
        assert_eq!(
            closed_in, closed_rd,
            "both classes say 'Stream closed' after close"
        );
        assert!(
            !closed_in.contains("Stream Closed"),
            "lower-case c: 'Stream Closed' is the FileInputStream family string"
        );
    }
}
