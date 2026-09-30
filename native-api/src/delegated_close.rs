// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Failure policy for a Java call that a native **delegates** to.
//!
//! A large family of natives in this workspace stand in for a JDK method whose
//! whole job is to hand the call on to something else — `close()` propagating
//! to the stream it wraps, `flush()` pushing the sink. The delegation is
//! spelled `ctx.invoke_virtual(target, "close", "()V", &[])`, and the shape
//! that grew up around it discards the whole `Result`:
//!
//! ```ignore
//! let _ = ctx.invoke_virtual(inner, "close", "()V", &[]);
//! ```
//!
//! `MethodCallFailed` has two variants and that discards both — every Java
//! throwable the delegated call raised, and every internal VM error. On a
//! `close()` after buffered writes the consequence is not "an exception was
//! lost": it is **lost data reported as success**, because the caller's
//! `try`-with-resources sees a clean exit.
//!
//! Not every swallow is wrong. `java.io.PrintWriter.close()` really does
//! `catch (IOException x) { trouble = true; }`, and converting that into a
//! throw would be a fresh divergence. The distinction this module exists to
//! make is between *what the JDK method catches* and *everything else*:
//!
//! * `java.io.PrintWriter` / `java.io.PrintStream` catch `IOException`.
//! * `java.util.logging.StreamHandler` catches `Exception`.
//! * `java.util.Formatter` catches `IOException`.
//! * `java.io.FilterOutputStream`, `java.util.zip.DeflaterOutputStream`,
//!   `sun.nio.cs.StreamEncoder`, `java.util.Properties.store` and the rest of
//!   the `java.io` wrapper family catch **nothing** — they propagate.
//!
//! None of those `catch` clauses catches an `Error`. A `NoSuchMethodError`
//! raised by a delegated call is not a stream problem at all: it means *our
//! own* dispatch failed to find the method, and absorbing it into a `catch`
//! written for `IOException` converts a broken VM into a quietly wrong one.
//! `MethodCallFailed::InternalError` is not a Java throwable and can never be
//! the `IOException` a JDK `catch` names, so it always propagates.
//!
//! The type test is by `ClassId` hierarchy — the question the `instanceof`
//! opcode asks — never by class name.

use crate::registry::NativeContext;
use cratonvm_types::error::{MethodCallFailed, MethodCallResult};

/// Absorb a thrown throwable that is assignable to `absorbed_root`, exactly as
/// the JDK method's own `catch (absorbed_root x)` does; propagate everything
/// else, including every `MethodCallFailed::InternalError`.
///
/// `absorbed_root` is an internal class name (`java/io/IOException`). When it
/// is not loaded, nothing can be an instance of it, so the failure propagates.
pub fn absorb_thrown(
    ctx: &dyn NativeContext,
    result: MethodCallResult,
    absorbed_root: &str,
) -> MethodCallResult {
    let thrown = match result {
        Ok(v) => return Ok(v),
        // Not a Java throwable, so no `catch` clause in any JDK method can
        // name it. Always propagates.
        Err(e @ MethodCallFailed::InternalError(_)) => return Err(e),
        Err(MethodCallFailed::ExceptionThrown(obj)) => obj,
    };
    let Some(root) = ctx.class_id_by_name(absorbed_root) else {
        // The absorbed type was never loaded, so `thrown` cannot be one.
        return Err(MethodCallFailed::ExceptionThrown(thrown));
    };
    if ctx.is_subclass(ctx.class_id_of_object(thrown), root) {
        Ok(None)
    } else {
        Err(MethodCallFailed::ExceptionThrown(thrown))
    }
}

/// The JDK method we stand in for wraps this delegation in
/// `catch (IOException x)`. Absorb an `IOException`; propagate `Error`,
/// `RuntimeException` and any internal VM failure.
///
/// Used where the real body is literally
/// `try { out.flush(); } catch (IOException x) { trouble = true; }`
/// (`java.io.PrintWriter`, `java.io.PrintStream`, `java.util.Formatter`).
pub fn absorb_io_exception(ctx: &dyn NativeContext, result: MethodCallResult) -> MethodCallResult {
    absorb_thrown(ctx, result, "java/io/IOException")
}

/// The JDK method we stand in for wraps this delegation in
/// `catch (Exception ex) { reportError(...) }` — wider than `IOException`, but
/// still not an `Error`.
///
/// Used for `java.util.logging.StreamHandler.flush()` /
/// `flushAndClose()`, whose contract is explicitly "we don't want to throw an
/// exception here" and which reports through the `ErrorManager` instead.
pub fn absorb_exception(ctx: &dyn NativeContext, result: MethodCallResult) -> MethodCallResult {
    absorb_thrown(ctx, result, "java/lang/Exception")
}

/// A delegation this VM makes at a point **HotSpot does not make one at all**
/// — a durability flush inside a bridge, a probe stream closed so it is not
/// leaked, an error backstop that reports a failure it must not replace.
///
/// There is no JDK `catch` to copy here, so the rule is the one thing that is
/// certain either way: a Java throwable raised by work HotSpot never performs
/// must not become a Java-visible failure HotSpot never raises, but an
/// `Error` (a `NoSuchMethodError` from our own dispatch, a `StackOverflowError`,
/// an `OutOfMemoryError`) is never "the sink misbehaved" and must not be
/// silent. Absorbs `Exception`; propagates `Error` and `InternalError`.
///
/// This is deliberately the same policy as [`absorb_exception`] and a distinct
/// name so the two reasons never get confused when one of them is revisited:
/// there, the JDK wrote the `catch`; here, we did.
pub fn vm_only_best_effort(ctx: &dyn NativeContext, result: MethodCallResult) -> MethodCallResult {
    absorb_thrown(ctx, result, "java/lang/Exception")
}
