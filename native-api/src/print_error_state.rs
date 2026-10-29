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

/// `java.io.PrintStream`'s own recursion/idempotence latch.
///
/// ```java
/// private boolean closing = false; /* To avoid recursive closing */
/// public void close() {
///     synchronized (this) {
///         if (!closing) {
///             closing = true;
///             ...
/// ```
///
/// It is never cleared, so it is also what makes a second `close()` a total
/// no-op — measured on HotSpot 25.0.3.9: a second `close()` throws nothing,
/// does not reach the sink a second time, and does not move `trouble`.
/// `java.io.PrintWriter` declares no such field (it uses `out == null`), so
/// reading it there answers `false` and changes nothing.
/// W7-70-printstream-close-noop.md
pub const CLOSING_FIELD: &str = "closing";

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

/// Has this `PrintStream` already run its `close()` body?
///
/// The read half of [`CLOSING_FIELD`]. A receiver whose class declares no
/// `closing` field reads as `false`, which is what a stream that has never
/// been closed answers — so this is inert on `PrintWriter` and on any
/// fabricated shape that has no slot for it, exactly like [`is_trouble`].
pub fn is_closing(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    matches!(ctx.get_field_by_name(this, CLOSING_FIELD), Value::Int(v) if v != 0)
}

/// Run `closing = true`, HotSpot's first statement inside `close()`.
///
/// Set BEFORE the delegation, never after: the field's stated purpose is "to
/// avoid recursive closing", and in HotSpot the recursion is real —
/// `charOut = new OutputStreamWriter(this, charset)` means closing the
/// character layer calls back into `this.close()`. It is also never cleared,
/// including on the path where the delegated close throws, so a retry after a
/// propagated `Error` is a no-op there too (measured).
pub fn latch_closing(ctx: &dyn NativeContext, this: ObjectRef) {
    ctx.set_field_by_name(this, CLOSING_FIELD, Value::Int(1));
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

/// What a delegated `print`/`println`/`write` actually did — the three
/// outcomes the `bool` these sites used to return collapsed into two.
///
/// The distinction exists because the write natives cannot propagate. They
/// stand in for `PrintStream.write(String)` / `PrintStream.write(byte[],int,int)`
/// and their `PrintWriter` twins, whose bodies are
///
/// ```text
/// try { …; out.write(…); … }
/// catch (InterruptedIOException x) { Thread.currentThread().interrupt(); }
/// catch (IOException x)            { trouble = true; }
/// ```
///
/// so an `IOException` is a failure HotSpot **handles** — the bytes go
/// nowhere, `checkError()` starts answering `true`, and `println` returns
/// normally — while an `Error` is a failure HotSpot lets straight out. Those
/// are not the same event, and answering "the write did not happen" for both
/// is what made this VM echo a handled failure to the console (a write HotSpot
/// performs nowhere) and swallow an unhandled one in silence.
///
/// W7-81-write-route-three-way.md
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelegatedWrite {
    /// The sink took the bytes.
    Delivered,
    /// The sink raised the throwable the JDK's own `catch` names, the JDK's
    /// `catch` body has been run (`trouble` set, or the interrupt re-asserted),
    /// and — as in HotSpot — **the bytes were written nowhere else**.
    Absorbed,
    /// The sink could not take the call at all: an `Error` (a
    /// `NoSuchMethodError` from our own dispatch above all), a
    /// `RuntimeException`, or a `MethodCallFailed::InternalError`, which is not
    /// a Java throwable and can never be the `IOException` a JDK `catch` names.
    ///
    /// HotSpot propagates every one of these out of `println`. This VM's write
    /// natives cannot, so the caller falls back to the console fd instead —
    /// louder than HotSpot, but the one thing HotSpot never does here is stay
    /// silent, and silence is the only other option available.
    Refused,
}

impl DelegatedWrite {
    /// The decision table, as a pure function of what the failure *was*.
    ///
    /// Split out from [`classify_write_failure`] so the table can be tested
    /// without a `NativeContext`: the class-hierarchy half is
    /// [`take_absorbed`]'s and is tested where it lives, and this half — which
    /// of the three answers each shape gets — is the part a future edit is
    /// likely to get wrong.
    ///
    /// `io` is "assignable to `java/io/IOException`", which is the whole of
    /// what both `catch` clauses name — `InterruptedIOException` is a subclass,
    /// so it lands on the same answer and differs only in which `catch` BODY
    /// runs, which is [`classify_write_failure`]'s business, not this table's.
    pub fn classify(internal_error: bool, io: bool) -> DelegatedWrite {
        // A `MethodCallFailed::InternalError` is not a Java throwable and can
        // never be the `IOException` a JDK `catch` names, so it is never
        // absorbed however the second argument reads. Everything else the
        // `catch` does not name — every `Error`, every `RuntimeException` — is
        // refused for the same reason: HotSpot lets all of them out.
        if io && !internal_error {
            DelegatedWrite::Absorbed
        } else {
            DelegatedWrite::Refused
        }
    }

    /// Did the delegation reach a conclusion the caller must NOT second-guess?
    ///
    /// `true` for [`DelegatedWrite::Delivered`] (the sink has the bytes) and
    /// for [`DelegatedWrite::Absorbed`] (HotSpot wrote them nowhere, so neither
    /// may we). `false` only for [`DelegatedWrite::Refused`], which is what
    /// keeps the console fallback alive on the case it exists for.
    pub fn routed(self) -> bool {
        match self {
            DelegatedWrite::Delivered | DelegatedWrite::Absorbed => true,
            DelegatedWrite::Refused => false,
        }
    }

    /// Did the delegated call return cleanly?
    ///
    /// The predicate [`record_write_failure`] answers, kept separate from
    /// [`DelegatedWrite::routed`] on purpose: they used to be the same `bool`
    /// and that is precisely the conflation W7-81 unpicked.
    pub fn delivered(self) -> bool {
        matches!(self, DelegatedWrite::Delivered)
    }
}

/// Run the JDK's `catch` bodies for a delegated write and say which of the
/// three outcomes happened.
///
/// The recording half is unchanged from W7-64 — an `InterruptedIOException`
/// re-asserts the thread's interrupt and does **not** set `trouble`; any other
/// `IOException` sets it; nothing else touches it, because HotSpot's `catch`
/// never sees anything else and a `trouble` HotSpot would not set is fresh
/// invented state rather than parity.
///
/// What is new is that the caller can now tell an absorbed `IOException` from a
/// refused call. Absorbs everything either way: these sites still cannot
/// propagate. W7-81-write-route-three-way.md
pub fn classify_write_failure(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    result: MethodCallResult,
) -> DelegatedWrite {
    let thrown = match result {
        Ok(_) => return DelegatedWrite::Delivered,
        Err(MethodCallFailed::InternalError(_)) => {
            return DelegatedWrite::classify(true, false);
        }
        Err(MethodCallFailed::ExceptionThrown(obj)) => obj,
    };
    let thrown_class = ctx.class_id_of_object(thrown);
    let is = |ctx: &dyn NativeContext, name: &str| {
        ctx.class_id_by_name(name)
            .is_some_and(|root| ctx.is_subclass(thrown_class, root))
    };
    let io = is(&*ctx, "java/io/IOException");
    let outcome = DelegatedWrite::classify(false, io);
    if outcome == DelegatedWrite::Absorbed {
        if is(&*ctx, "java/io/InterruptedIOException") {
            let current = ctx.current_thread_object();
            ctx.thread_interrupt(current);
        } else {
            set_trouble(&*ctx, this);
        }
    }
    outcome
}

/// Record a delegated write's failure at a call site that **cannot**
/// propagate, and report whether the write succeeded.
///
/// [`classify_write_failure`] with its three-way answer narrowed back to the
/// one question this predicate has always asked — "did the sink take it?" —
/// for the call sites that have no console fallback to make the wider answer
/// mean anything. Absorbs everything, exactly as the `let _ = …` these sites
/// had did.
///
/// **Residual, and deliberate:** an `Error` — a `NoSuchMethodError` from our
/// own dispatch above all — is absorbed here where HotSpot lets it out, and it
/// does NOT set `trouble` (HotSpot's `catch` never sees it, so a `trouble`
/// that HotSpot would not set would be fresh invented state, not parity).
///
/// Callers that DO have somewhere else to send the text must use
/// [`classify_write_failure`] instead: this `bool` cannot distinguish "HotSpot
/// wrote it nowhere" from "the sink could not take the call", and answering
/// "not written" for the first is what made `route_write_through_out` echo an
/// absorbed `IOException` to the console.
/// W7-70-printstream-close-noop.md, W7-81-write-route-three-way.md
pub fn record_write_failure(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    result: MethodCallResult,
) -> bool {
    classify_write_failure(ctx, this, result).delivered()
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
pub fn report_handler_error(
    ctx: &mut dyn NativeContext,
    handler: ObjectRef,
    ex: ObjectRef,
    code: i32,
) -> bool {
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

#[cfg(test)]
mod tests {
    use super::DelegatedWrite;

    /// The whole reason the answer is three-way rather than two.
    ///
    /// This table is not observable from inside a Java program: every one of
    /// the three answers returns from `println` without throwing, and the only
    /// thing they change is whether the caller falls back to the raw console
    /// fd — a write no code in the JVM can see, which is exactly why W7-70
    /// could not measure this and why `probes/CloseFlushSwallowProbe.java`
    /// asserts the Java-visible half and prints the rest.
    /// W7-81-write-route-three-way.md
    #[test]
    fn an_absorbed_ioexception_is_routed_so_it_is_not_echoed() {
        // HotSpot's `catch (IOException x) { trouble = true; }` runs and the
        // bytes go NOWHERE. A caller that treats this as "not written" writes
        // them a second time, to a console HotSpot never touched.
        assert_eq!(
            DelegatedWrite::classify(false, true),
            DelegatedWrite::Absorbed
        );
        assert!(DelegatedWrite::Absorbed.routed());
        assert!(!DelegatedWrite::Absorbed.delivered());
    }

    #[test]
    fn an_error_is_refused_so_the_console_fallback_survives() {
        // The picocli / JUnit-console `NoSuchMethodError` shape. HotSpot lets
        // it out of `println`; this VM cannot, so "not routed" — and the
        // console fallback it triggers — is what keeps the text from vanishing.
        assert_eq!(
            DelegatedWrite::classify(false, false),
            DelegatedWrite::Refused
        );
        assert!(!DelegatedWrite::Refused.routed());
        assert!(!DelegatedWrite::Refused.delivered());
    }

    #[test]
    fn an_internal_error_is_refused_and_never_absorbed() {
        // `MethodCallFailed::InternalError` is not a Java throwable and can
        // never be the `IOException` a JDK `catch` names, so absorbing it is
        // never JDK parity — whatever the second argument says.
        assert_eq!(
            DelegatedWrite::classify(true, false),
            DelegatedWrite::Refused
        );
        assert_eq!(
            DelegatedWrite::classify(true, true),
            DelegatedWrite::Refused
        );
    }

    #[test]
    fn a_clean_write_is_the_only_delivered_answer() {
        assert!(DelegatedWrite::Delivered.routed());
        assert!(DelegatedWrite::Delivered.delivered());
        // `record_write_failure`'s `bool` is `delivered()`, NOT `routed()`.
        // Conflating the two is the defect W7-81 unpicked, so pin them apart.
        assert_ne!(
            DelegatedWrite::Absorbed.routed(),
            DelegatedWrite::Absorbed.delivered()
        );
    }
}
