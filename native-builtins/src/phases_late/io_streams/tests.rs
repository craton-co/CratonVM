use super::*;
use crate::test_utils::{mock_ctx, MockNativeContext};
use cratonvm_native_api::NativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// How many `close()V` calls the hook below has seen. The mock's
/// `InvokeVirtualHook` is a bare `fn` pointer, so the counter cannot be a
/// captured local — but it must not be a process-global either.
///
/// It WAS a `static AtomicUsize`, and five tests share it
/// (`pushback_input_stream_close_is_idempotent_and_latches`,
/// `..._refuses_its_whole_surface_after_close`, `..._closed_beats_overflow`,
/// `pushback_reader_close_reaches_the_inner_reader_twice`,
/// `pushback_messages_differ_where_measured_and_agree_where_measured`).
/// Each opens with `store(0)` and later asserts an exact count, and cargo
/// runs them on PARALLEL THREADS in one process — so one test's reset
/// landing between another's `close()` and its `assert_eq!(.., 1)` reads 0,
/// and one test's close landing inside another's window reads 2. In the
/// full 4172-test suite the five are spread far enough apart that this hit
/// roughly one run in three, which is what made it look like an unexplained
/// flake rather than shared state; `cargo test --lib pushback`, which runs
/// only these five, failed 9 runs out of 10.
///
/// Thread-local fixes it outright and costs nothing: each test runs on its
/// own thread, the hook is called synchronously on that same thread, and no
/// serialization between the five is needed.
thread_local! {
    static INNER_CLOSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn inner_closes_reset() {
    INNER_CLOSES.with(|c| c.set(0));
}

fn inner_closes() -> usize {
    INNER_CLOSES.with(std::cell::Cell::get)
}

fn count_closes(
    _ctx: &mut MockNativeContext,
    _recv: ObjectRef,
    name: &str,
    desc: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    if name == "close" && desc == "()V" {
        INNER_CLOSES.with(|c| c.set(c.get() + 1));
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
    inner_closes_reset();
    let (this, _inner) = open_pushback(&mut ctx, ArrayElementType::Byte);

    p58_pushback_in_close(&mut ctx, &[Value::Object(Some(this))]).unwrap();
    assert_eq!(inner_closes(), 1);
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
        inner_closes(),
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
    inner_closes_reset();
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
    assert_eq!(inner_closes(), 1);
}

/// The closed check runs BEFORE the pushback-overflow check, so a closed
/// stream with a full buffer reports `Stream closed`, not
/// `Push back buffer is full`.
#[test]
fn pushback_input_stream_closed_beats_overflow() {
    let mut ctx = mock_ctx();
    ctx.set_invoke_virtual_hook(count_closes);
    inner_closes_reset();
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
    inner_closes_reset();
    let (this, _inner) = open_pushback(&mut ctx, ArrayElementType::Char);

    p66_pushback_reader_close(&mut ctx, &[Value::Object(Some(this))]).unwrap();
    assert_eq!(inner_closes(), 1);
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
        inner_closes(),
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
