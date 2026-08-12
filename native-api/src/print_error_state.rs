// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Where an absorbed failure is **recorded**.
//!
//! `delegated_close` decides which throwables a JDK method's own `catch`
//! swallows. This module is the other half of that same `catch`: what its
//! **body** does. Two families in the JDK absorb an error and then store it
//! somewhere the caller can go and read, and for both of them "absorbed" and
//! "lost" are different outcomes:
//!
//! * `java.io.PrintStream` / `java.io.PrintWriter` —
//!   `catch (IOException x) { trouble = true; }`, read back by `checkError()`.
//! * `java.util.logging.Handler` — `catch (Exception ex) { reportError(null,
//!   ex, ErrorManager.<CODE>); }`, delivered to the handler's `ErrorManager`.
//!
//! A native that absorbs but does not record has not matched the JDK — it has
//! converted a reportable failure into complete silence, and `checkError()`
//! answers `false` forever. `delegated_close::absorb_io_exception` answers
//! `Ok(None)` for a clean void return and for an absorbed throwable alike, so
//! the recording sites need to know **which** of the two happened; that is
//! what [`absorb_io_exception_recording`] and [`take_absorbed`] provide.
//!
//! Field access is by NAME and is a no-op when the receiver's class declares
//! no such field (`NativeContext::set_field_by_name`'s documented contract).
//! That is deliberate: in Compatible mode the receiver is the real
//! `java.io.PrintStream`, which declares `trouble`; a synthetic receiver that
//! does not declare it simply does not record, exactly as it did before this
//! module existed, rather than writing over some other class's slot 0.

use crate::registry::NativeContext;
use cratonvm_types::error::{MethodCallFailed, MethodCallResult};
use cratonvm_types::{ObjectRef, Value};

/// The private flag `java.io.PrintStream` and `java.io.PrintWriter` set from
/// their `catch (IOException x)` bodies, and that `checkError()` returns.
pub const TROUBLE_FIELD: &str = "trouble";

/// `java.util.logging.ErrorManager.WRITE_FAILURE`.
pub const ERROR_MANAGER_WRITE_FAILURE: i32 = 1;
/// `java.util.logging.ErrorManager.FLUSH_FAILURE`.
pub const ERROR_MANAGER_FLUSH_FAILURE: i32 = 2;
/// `java.util.logging.ErrorManager.CLOSE_FAILURE`.
pub const ERROR_MANAGER_CLOSE_FAILURE: i32 = 3;

/// Run the JDK's `catch (IOException x) { trouble = true; }` body.
///
/// Sets `this.trouble = true` when the receiver's class declares the field,
/// and does nothing otherwise. Never throws: the JDK's `catch` body cannot.
pub fn set_trouble(ctx: &dyn NativeContext, this: ObjectRef) {
    ctx.set_field_by_name(this, TROUBLE_FIELD, Value::Int(1));
}

/// Read the flag back the way `checkError()`'s final `return trouble;` does.
///
/// A receiver whose class declares no `trouble` field reads as `false`, which
/// is what a `PrintStream` that has never failed answers.
pub fn is_trouble(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    matches!(ctx.get_field_by_name(this, TROUBLE_FIELD), Value::Int(v) if v != 0)
}

/// Clear the flag, as `PrintWriter.clearError()` / `PrintStream.clearError()`
/// do.
pub fn clear_trouble(ctx: &dyn NativeContext, this: ObjectRef) {
    ctx.set_field_by_name(this, TROUBLE_FIELD, Value::Int(0));
}

/// Split a delegated call's outcome into "absorbed, and here is the throwable"
/// and "propagate".
///
/// `delegated_close::absorb_thrown` is the same decision with the throwable
/// discarded. Use this one wherever the JDK's `catch` body does something with
/// its `x` / `ex` parameter — sets `trouble`, or hands it to an
/// `ErrorManager` — and that one where the body only sets a flag this VM has
/// no place to put.
///
/// `absorbed_root` is an internal class name (`java/io/IOException`). When it
/// is not loaded, nothing can be an instance of it, so the failure propagates.
pub fn take_absorbed(
    ctx: &dyn NativeContext,
    result: MethodCallResult,
    absorbed_root: &str,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let thrown = match result {
        Ok(_) => return Ok(None),
        // Not a Java throwable, so no `catch` clause in any JDK method can
        // name it. Always propagates.
        Err(e @ MethodCallFailed::InternalError(_)) => return Err(e),
        Err(MethodCallFailed::ExceptionThrown(obj)) => obj,
    };
    let Some(root) = ctx.class_id_by_name(absorbed_root) else {
        return Err(MethodCallFailed::ExceptionThrown(thrown));
    };
    if ctx.is_subclass(ctx.class_id_of_object(thrown), root) {
        Ok(Some(thrown))
    } else {
        Err(MethodCallFailed::ExceptionThrown(thrown))
    }
}

/// The whole of a `java.io.PrintStream` / `java.io.PrintWriter`
/// `catch (IOException x) { trouble = true; }` clause: absorb an
/// `IOException`, record it on `this`, and propagate everything the JDK's
/// `catch` does not name — every `Error`, every `RuntimeException`, and every
/// `MethodCallFailed::InternalError`.
///
/// This is [`delegated_close::absorb_io_exception`] plus the body of the
/// catch. Prefer it at every `PrintStream`/`PrintWriter` delegation: an
/// absorbed failure that is not recorded is unobservable rather than merely
/// unthrown, which is a strictly worse divergence than the one being fixed.
///
/// [`delegated_close::absorb_io_exception`]: crate::delegated_close::absorb_io_exception
pub fn absorb_io_exception_recording(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    result: MethodCallResult,
) -> MethodCallResult {
    if take_absorbed(ctx, result, "java/io/IOException")?.is_some() {
        set_trouble(ctx, this);
    }
    Ok(None)
}

/// `java.io.PrintStream.write(int)` and friends widen their `catch` by one
/// clause the flush/close pair does not have:
///
/// ```text
/// catch (InterruptedIOException x) { Thread.currentThread().interrupt(); }
/// catch (IOException x)            { trouble = true; }
/// ```
///
/// An `InterruptedIOException` therefore does NOT set `trouble` — it
/// re-asserts the thread's interrupt flag instead. Both clauses are on every
/// `write`/`writeln`/`newLine`/`format` body in `PrintStream` and
/// `PrintWriter`; neither is on `flush()` or `close()`.
///
/// Returns `Ok(None)` in both absorbing cases, propagates the rest.
pub fn absorb_write_exception_recording(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    result: MethodCallResult,
) -> MethodCallResult {
    let Some(thrown) = take_absorbed(&*ctx, result, "java/io/IOException")? else {
        return Ok(None);
    };
    let thrown_class = ctx.class_id_of_object(thrown);
    let interrupted_root = ctx.class_id_by_name("java/io/InterruptedIOException");
    let interrupted = interrupted_root.is_some_and(|root| ctx.is_subclass(thrown_class, root));
    if interrupted {
        let current = ctx.current_thread_object();
        ctx.thread_interrupt(current);
    } else {
        set_trouble(&*ctx, this);
    }
    Ok(None)
}

/// Record a delegated write's failure at a call site that **cannot**
/// propagate, and report whether the write succeeded.
///
/// The `print`/`println`/`write` natives funnel through helpers that return
/// `()` or `bool` across a dozen call sites, so the `Error`-propagating half
/// of [`absorb_write_exception_recording`] is a signature change rather than a
/// one-line fix there. What is a one-line fix is the half this lane is
/// chartered on: the JDK's `catch (IOException x) { trouble = true; }` body
/// still runs, so a failed write is *observable* through `checkError()` even
/// where it is still (wrongly) unthrown.
///
/// Absorbs everything, exactly as the `let _ = …` these sites had did.
/// Returns `true` when the delegated call returned cleanly.
///
/// **Residual, and deliberate:** an `Error` — a `NoSuchMethodError` from our
/// own dispatch above all — is absorbed here where HotSpot lets it out, and it
/// does NOT set `trouble` (HotSpot's `catch` never sees it, so a `trouble`
/// that HotSpot would not set would be fresh invented state, not parity).
pub fn record_write_failure(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    result: MethodCallResult,
) -> bool {
    let thrown = match result {
        Ok(_) => return true,
        Err(MethodCallFailed::InternalError(_)) => return false,
        Err(MethodCallFailed::ExceptionThrown(obj)) => obj,
    };
    let thrown_class = ctx.class_id_of_object(thrown);
    let is = |ctx: &dyn NativeContext, name: &str| {
        ctx.class_id_by_name(name)
            .is_some_and(|root| ctx.is_subclass(thrown_class, root))
    };
    if is(&*ctx, "java/io/InterruptedIOException") {
        let current = ctx.current_thread_object();
        ctx.thread_interrupt(current);
    } else if is(&*ctx, "java/io/IOException") {
        set_trouble(&*ctx, this);
    }
    false
}

/// Same for a raw host-level write/flush failure on the fd a console
/// `PrintStream` is backed by.
///
/// The fd path is this VM's stand-in for the `out.write(...)` the JDK
/// delegates to, so a host `io::Error` there is precisely the `IOException`
/// the JDK's `catch` names.
pub fn record_host_io_failure<T, E>(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    result: Result<T, E>,
) -> bool {
    if result.is_err() {
        set_trouble(ctx, this);
        return false;
    }
    true
}

/// Run `java.util.logging.Handler.reportError(null, ex, code)` — the JDK's own
/// route from an absorbed `Exception` to the handler's `ErrorManager`.
///
/// `Handler.reportError` is `protected` but virtual, and its body is
/// `try { errorManager.error(msg, ex, code); } catch (Exception ex2) { … }`,
/// so a subclass that overrides it sees the call. Dispatched by descriptor,
/// never by name-matching a class.
///
/// Best-effort by construction: this is the JDK's *error-reporting* path, and
/// `Handler.flush()` / `Handler.close()` declare no checked exception. A
/// failure to report must not become a second, different failure on the
/// caller — that is the fault `reportError`'s own `catch (Exception ex2)`
/// exists to prevent. Returns whether the report was delivered so a caller
/// that wants to know can ask.
pub fn report_handler_error(ctx: &mut dyn NativeContext, handler: ObjectRef, ex: ObjectRef, code: i32) -> bool {
    ctx.invoke_virtual(
        handler,
        "reportError",
        "(Ljava/lang/String;Ljava/lang/Exception;I)V",
        &[
            Value::Object(None),
            Value::Object(Some(ex)),
            Value::Int(code),
        ],
    )
    .is_ok()
}
