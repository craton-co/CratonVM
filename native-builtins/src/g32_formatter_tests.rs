#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

use super::*;
use crate::test_utils::{mock_ctx, MockNativeContext};

fn registry() -> NativeMethodRegistry {
    let mut r = NativeMethodRegistry::new();
    register_string_format_real_jdk_natives(&mut r);
    r
}

fn call(
    r: &NativeMethodRegistry,
    ctx: &mut MockNativeContext,
    method: &str,
    desc: &str,
    args: &[Value],
) -> MethodCallResult {
    let cb = r
        .find("java/util/Formatter", method, desc)
        .unwrap_or_else(|| panic!("java/util/Formatter.{method}{desc} must be registered"));
    cb(ctx, args)
}

/// A two-slot Formatter: slot 0 = the sink, slot 1 = the locale.
fn make_formatter(ctx: &mut MockNativeContext, sink: Value) -> ObjectRef {
    let f = ctx.alloc_object(ClassId::new(0), 2);
    ctx.set_field(f, 0, sink);
    ctx.set_field(f, 1, Value::Object(None));
    f
}

/// An OPEN Formatter hands its sink back. `formatter_ensure_open` is the
/// JDK's `ensureOpen()`, and slot 0 IS the flag (`close()` is
/// `finally { a = null; }`).
#[test]
fn ensure_open_returns_the_sink_while_open() {
    let mut ctx = mock_ctx();
    let sink = ctx.alloc_object(ClassId::new(0), 2);
    let f = make_formatter(&mut ctx, Value::Object(Some(sink)));
    assert_eq!(formatter_ensure_open(&mut ctx, f).unwrap(), sink);
}

/// A null slot 0 is CLOSED, and the refusal is a real Java throwable, not an
/// internal error — `catch (FormatterClosedException)` has to see it.
#[test]
fn ensure_open_refuses_once_the_sink_is_null() {
    let mut ctx = mock_ctx();
    let f = make_formatter(&mut ctx, Value::Object(None));
    let e = formatter_ensure_open(&mut ctx, f).unwrap_err();
    assert!(
        matches!(e, MethodCallFailed::ExceptionThrown(_)),
        "a closed Formatter must raise a catchable Java exception, got {e:?}"
    );
}

/// `close()` nulls slot 0. This is the whole of the defect it repairs: the
/// body used to delegate the underlying close and RETURN, leaving the
/// Formatter open, so every post-close method answered normally and a
/// post-close `format()` kept appending to the caller's buffer
/// (`pre:0051` against HotSpot's `pre:005`).
#[test]
fn close_nulls_the_sink_slot() {
    let r = registry();
    let mut ctx = mock_ctx();
    let sink = ctx.alloc_object(ClassId::new(0), 2);
    let f = make_formatter(&mut ctx, Value::Object(Some(sink)));
    call(&r, &mut ctx, "close", "()V", &[Value::Object(Some(f))]).unwrap();
    assert_eq!(ctx.get_field(f, 0), Value::Object(None));
}

/// `close()` is IDEMPOTENT — the JDK's `if (a == null) return`. A second
/// close must not raise FormatterClosedException, which is the trap in
/// identifying "closed" with "the guard fires".
#[test]
fn close_twice_does_not_throw() {
    let r = registry();
    let mut ctx = mock_ctx();
    let sink = ctx.alloc_object(ClassId::new(0), 2);
    let f = make_formatter(&mut ctx, Value::Object(Some(sink)));
    call(&r, &mut ctx, "close", "()V", &[Value::Object(Some(f))]).unwrap();
    call(&r, &mut ctx, "close", "()V", &[Value::Object(Some(f))])
        .expect("a second close() must be a no-op, not a refusal");
}

/// `out()` answers slot 0 BY IDENTITY while open — `f.out() == f.out()` is
/// true on HotSpot, and it is what the caller-supplied-Appendable contract
/// rests on.
#[test]
fn out_answers_the_sink_by_identity_while_open() {
    let r = registry();
    let mut ctx = mock_ctx();
    let sink = ctx.alloc_object(ClassId::new(0), 2);
    let f = make_formatter(&mut ctx, Value::Object(Some(sink)));
    let got = call(
        &r,
        &mut ctx,
        "out",
        "()Ljava/lang/Appendable;",
        &[Value::Object(Some(f))],
    )
    .unwrap();
    assert_eq!(got, Some(Value::Object(Some(sink))));
}

/// All four of the guarded accessors refuse after `close()`.
///
/// `locale()` is in the list on purpose: it reads slot 1 and has no reason
/// of its own to look at the sink, so it is the row most likely to be
/// missed. MEASURED on HotSpot 25.0.3+9 — it throws like the rest.
#[test]
fn every_guarded_accessor_refuses_after_close() {
    let r = registry();
    let mut ctx = mock_ctx();
    let sink = ctx.alloc_object(ClassId::new(0), 2);
    let f = make_formatter(&mut ctx, Value::Object(Some(sink)));
    call(&r, &mut ctx, "close", "()V", &[Value::Object(Some(f))]).unwrap();

    for (method, desc) in [
        ("toString", "()Ljava/lang/String;"),
        ("out", "()Ljava/lang/Appendable;"),
        ("flush", "()V"),
        ("locale", "()Ljava/util/Locale;"),
    ] {
        let e = expect_refusal(
            call(&r, &mut ctx, method, desc, &[Value::Object(Some(f))]),
            method,
        );
        assert!(
            matches!(e, MethodCallFailed::ExceptionThrown(_)),
            "{method} after close() must refuse, got {e:?}"
        );
    }
}

/// `ioException()` is the one public method with NO `ensureOpen()` in the
/// JDK, and it is deliberately not registered here — MEASURED: it returns
/// normally after `close()`. Recorded as a test so a later lane does not
/// "complete" the guard set by adding it.
#[test]
fn io_exception_is_not_registered_and_so_is_not_guarded() {
    let r = registry();
    assert!(
        r.find(
            "java/util/Formatter",
            "ioException",
            "()Ljava/io/IOException;"
        )
        .is_none(),
        "ioException() must stay on real bytecode: it does NOT throw after close()"
    );
}

fn expect_refusal(r: MethodCallResult, what: &str) -> MethodCallFailed {
    match r {
        Err(e) => e,
        Ok(v) => panic!("{what} after close() RETURNED {v:?} instead of refusing"),
    }
}
