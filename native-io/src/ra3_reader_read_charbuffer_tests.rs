use super::*;
use crate::test_support::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// Drive `native_reader_read_charbuffer` through a scripted mock and
/// assert that `CharBuffer.put(char[], int, int)` is invoked with the
/// exact chars produced by the scripted `read([CII)I` call.
#[test]
fn ra3_put_is_called_with_right_chars() {
    let mut ctx = MockNativeContext::new();
    // Allocate a `this` Reader and a `target` CharBuffer.
    let this = ctx.alloc_object(2);
    let target = ctx.alloc_object(2);

    // position() = 0, limit() = 32 → remaining = 32. The native then
    // caps chunk at min(32, 4096) = 32.
    ctx.script("limit", "()I", Ok(Some(Value::Int(32))));
    ctx.script("position", "()I", Ok(Some(Value::Int(0))));
    // `this.read(chars, 0, 32)` returns 5 and fills chars[0..5] with
    // 'h','e','l','l','o' via the same `char[]` the native allocated.
    // We can't easily write to it from the script, so we fake it by
    // returning 5 and then asserting on the **put** args which will
    // receive the allocated (zero-initialised) array. The test below
    // asserts that the shape of the put call is correct: same array,
    // offset 0, length == read's return value.
    ctx.script("read", "([CII)I", Ok(Some(Value::Int(5))));
    ctx.script(
        "put",
        "([CII)Ljava/nio/CharBuffer;",
        Ok(Some(Value::Object(Some(target)))),
    );

    let result = native_reader_read_charbuffer(
        &mut ctx,
        &[Value::Object(Some(this)), Value::Object(Some(target))],
    )
    .expect("native ok");
    assert_eq!(result, Some(Value::Int(5)));

    let calls = ctx.recorded_calls();
    // 4 invoke_virtual calls: limit, position, read, put.
    assert_eq!(calls.len(), 4);
    assert_eq!(calls[0].method_name, "limit");
    assert_eq!(calls[0].descriptor, "()I");
    assert_eq!(calls[1].method_name, "position");
    assert_eq!(calls[1].descriptor, "()I");
    assert_eq!(calls[2].method_name, "read");
    assert_eq!(calls[2].descriptor, "([CII)I");
    // read(chars, 0, 32)
    assert!(matches!(calls[2].args[0], Value::Object(Some(_))));
    assert_eq!(calls[2].args[1], Value::Int(0));
    assert_eq!(calls[2].args[2], Value::Int(32));

    // The star of the test: put(chars, 0, 5).
    assert_eq!(calls[3].method_name, "put");
    assert_eq!(calls[3].descriptor, "([CII)Ljava/nio/CharBuffer;");
    // Same char[] as the `read` call.
    let read_chars = match calls[2].args[0] {
        Value::Object(Some(o)) => o,
        _ => panic!("read arg not a char[]"),
    };
    let put_chars = match calls[3].args[0] {
        Value::Object(Some(o)) => o,
        _ => panic!("put arg not a char[]"),
    };
    assert_eq!(
        read_chars.as_ptr() as usize,
        put_chars.as_ptr() as usize,
        "put must receive the same char[] that read filled"
    );
    assert_eq!(calls[3].args[1], Value::Int(0));
    assert_eq!(calls[3].args[2], Value::Int(5));
}

/// A zero-remaining buffer short-circuits with 0 and never calls `read` or `put`.
#[test]
fn ra3_zero_remaining_returns_zero_without_io() {
    let mut ctx = MockNativeContext::new();
    let this = ctx.alloc_object(0);
    let target = ctx.alloc_object(0);
    ctx.script("limit", "()I", Ok(Some(Value::Int(4))));
    ctx.script("position", "()I", Ok(Some(Value::Int(4))));

    let result = native_reader_read_charbuffer(
        &mut ctx,
        &[Value::Object(Some(this)), Value::Object(Some(target))],
    )
    .expect("native ok");
    assert_eq!(result, Some(Value::Int(0)));
    let calls = ctx.recorded_calls();
    assert_eq!(calls.len(), 2, "only limit/position should be queried");
    for c in calls {
        assert_ne!(c.method_name, "read");
        assert_ne!(c.method_name, "put");
    }
}

/// EOF: `read` returns -1, so `put` is NOT called and we return -1.
#[test]
fn ra3_eof_returns_minus_one_and_skips_put() {
    let mut ctx = MockNativeContext::new();
    let this = ctx.alloc_object(0);
    let target = ctx.alloc_object(0);
    ctx.script("limit", "()I", Ok(Some(Value::Int(16))));
    ctx.script("position", "()I", Ok(Some(Value::Int(0))));
    ctx.script("read", "([CII)I", Ok(Some(Value::Int(-1))));

    let result = native_reader_read_charbuffer(
        &mut ctx,
        &[Value::Object(Some(this)), Value::Object(Some(target))],
    )
    .expect("native ok");
    assert_eq!(result, Some(Value::Int(-1)));
    for c in ctx.recorded_calls() {
        assert_ne!(c.method_name, "put");
    }
}

/// `remaining > 4096` must cap the scratch array at 4096.
#[test]
fn ra3_caps_chunk_at_4096() {
    let mut ctx = MockNativeContext::new();
    let this = ctx.alloc_object(0);
    let target = ctx.alloc_object(0);
    ctx.script("limit", "()I", Ok(Some(Value::Int(1_000_000))));
    ctx.script("position", "()I", Ok(Some(Value::Int(0))));
    ctx.script("read", "([CII)I", Ok(Some(Value::Int(1))));
    ctx.script(
        "put",
        "([CII)Ljava/nio/CharBuffer;",
        Ok(Some(Value::Object(Some(target)))),
    );

    let _ = native_reader_read_charbuffer(
        &mut ctx,
        &[Value::Object(Some(this)), Value::Object(Some(target))],
    )
    .expect("native ok");
    let read_call = ctx
        .recorded_calls()
        .iter()
        .find(|c| c.method_name == "read")
        .expect("read was called");
    assert_eq!(read_call.args[2], Value::Int(4096));
}
