use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};
use std::cell::Cell;

thread_local! {
    /// (old, new) address pairs the wrapped `read` moves.
    static W17F_MOVES: Cell<[(usize, usize); 2]> = const { Cell::new([(0, 0); 2]) };
    /// Bytes the wrapped `read` answers, in order; -1 once exhausted.
    static W17F_BYTES: Cell<[i32; 2]> = const { Cell::new([-1, -1]) };
    static W17F_READS: Cell<usize> = const { Cell::new(0) };
}

fn addr(o: ObjectRef) -> usize {
    o.as_ptr() as usize
}

fn w17f_read_hook(
    ctx: &mut MockNativeContext,
    _receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    if method_name != "read" {
        return None;
    }
    for (old, new) in W17F_MOVES.with(|m| m.get()) {
        if old != 0 {
            ctx.remap_native_pin_addr_for_test(old, new);
        }
    }
    let n = W17F_READS.with(|r| {
        let n = r.get();
        r.set(n + 1);
        n
    });
    let b = W17F_BYTES.with(|b| b.get()).get(n).copied().unwrap_or(-1);
    Some(Ok(Some(Value::Int(b))))
}

fn reset() {
    W17F_MOVES.with(|m| m.set([(0, 0); 2]));
    W17F_BYTES.with(|b| b.set([-1, -1]));
    W17F_READS.with(|r| r.set(0));
}

/// A LineNumberInputStream copy: wrapped stream, line 0, no pushback.
fn lnis_copy(ctx: &mut MockNativeContext, inner: ObjectRef) -> ObjectRef {
    let o = ctx.fresh_object_ref();
    ctx.set_field(o, 0, Value::Object(Some(inner)));
    ctx.set_field(o, 1, Value::Int(0));
    ctx.set_field(o, 2, Value::Int(0));
    ctx.set_field(o, 3, Value::Int(-1));
    o
}

fn registry() -> NativeMethodRegistry {
    let mut r = NativeMethodRegistry::new();
    register_line_number_input_stream(&mut r);
    r
}

#[test]
fn w17f_lnis_read_counts_the_line_on_the_moved_receiver() {
    reset();
    let reg = registry();
    let mut ctx = MockNativeContext::new();
    let inner = ctx.fresh_object_ref();
    let this_old = lnis_copy(&mut ctx, inner);
    let this_new = lnis_copy(&mut ctx, inner);
    W17F_MOVES.with(|m| m.set([(addr(this_old), addr(this_new)), (0, 0)]));
    W17F_BYTES.with(|b| b.set([b'\n' as i32, -1]));
    ctx.set_invoke_virtual_hook(w17f_read_hook);

    let cb = reg
        .find("java/io/LineNumberInputStream", "read", "()I")
        .expect("LineNumberInputStream.read()I registered");
    let out = cb(&mut ctx, &[Value::Object(Some(this_old))]);
    reset();

    assert!(matches!(out, Ok(Some(Value::Int(10)))));
    assert!(
        matches!(ctx.get_field(this_new, 1), Value::Int(1)),
        "the line number must be counted on the receiver's current address"
    );
    assert!(
        matches!(ctx.get_field(this_old, 1), Value::Int(0)),
        "nothing may be written through the vacated address"
    );
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "every root must be released"
    );
}

#[test]
fn w17f_lnis_read_array_fills_the_moved_buffer() {
    reset();
    let reg = registry();
    let mut ctx = MockNativeContext::new();
    let inner = ctx.fresh_object_ref();
    let this_old = lnis_copy(&mut ctx, inner);
    let this_new = lnis_copy(&mut ctx, inner);
    let buf_old = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 4);
    let buf_new = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 4);
    W17F_MOVES.with(|m| {
        m.set([
            (addr(this_old), addr(this_new)),
            (addr(buf_old), addr(buf_new)),
        ])
    });
    W17F_BYTES.with(|b| b.set([b'\n' as i32, -1]));
    ctx.set_invoke_virtual_hook(w17f_read_hook);

    let cb = reg
        .find("java/io/LineNumberInputStream", "read", "([BII)I")
        .expect("LineNumberInputStream.read([BII)I registered");
    let out = cb(
        &mut ctx,
        &[
            Value::Object(Some(this_old)),
            Value::Object(Some(buf_old)),
            Value::Int(0),
            Value::Int(4),
        ],
    );
    reset();

    assert!(matches!(out, Ok(Some(Value::Int(1)))));
    assert!(
        matches!(ctx.get_array_element(buf_new, 0), Value::Int(10)),
        "the byte must land in the buffer's current address"
    );
    assert!(matches!(ctx.get_array_element(buf_old, 0), Value::Int(0)));
    assert!(matches!(ctx.get_field(this_new, 1), Value::Int(1)));
    assert!(matches!(ctx.get_field(this_old, 1), Value::Int(0)));
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "every root must be released"
    );
}
