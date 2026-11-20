use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_types::ObjectRef;
use std::cell::Cell;

thread_local! {
    /// (old, new) address pairs the wrapped call moves.
    static W20B_MOVES: Cell<[(usize, usize); 2]> = const { Cell::new([(0, 0); 2]) };
    /// Answers of the wrapped `read` in order (-1 once exhausted), and
    /// the answer of `available`.
    static W20B_BYTES: Cell<[i32; 2]> = const { Cell::new([-1, -1]) };
    static W20B_READS: Cell<usize> = const { Cell::new(0) };
}

fn addr(o: ObjectRef) -> usize {
    o.as_ptr() as usize
}

fn w20b_hook(
    ctx: &mut MockNativeContext,
    _receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    if method_name != "read" && method_name != "available" {
        return None;
    }
    for (old, new) in W20B_MOVES.with(|m| m.get()) {
        if old != 0 {
            ctx.remap_native_pin_addr_for_test(old, new);
        }
    }
    if method_name == "available" {
        return Some(Ok(Some(Value::Int(4))));
    }
    let n = W20B_READS.with(|r| {
        let n = r.get();
        r.set(n + 1);
        n
    });
    let b = W20B_BYTES.with(|b| b.get()).get(n).copied().unwrap_or(-1);
    Some(Ok(Some(Value::Int(b))))
}

/// Clears the thread-locals even when an assertion panics.
struct Reset;
impl Drop for Reset {
    fn drop(&mut self) {
        W20B_MOVES.with(|m| m.set([(0, 0); 2]));
        W20B_BYTES.with(|b| b.set([-1, -1]));
        W20B_READS.with(|r| r.set(0));
    }
}

/// A LineNumberInputStream copy: wrapped stream, line 0, `pushback`.
fn lnis_copy(ctx: &mut MockNativeContext, inner: ObjectRef, pushback: i32) -> ObjectRef {
    let o = ctx.fresh_object_ref();
    ctx.set_field(o, 0, Value::Object(Some(inner)));
    ctx.set_field(o, 1, Value::Int(0));
    ctx.set_field(o, 2, Value::Int(0));
    ctx.set_field(o, 3, Value::Int(pushback));
    o
}

fn registry() -> NativeMethodRegistry {
    let mut r = NativeMethodRegistry::new();
    register_deprecated_util_natives(&mut r);
    r
}

#[test]
fn w20b_lnis_read_counts_the_line_on_the_moved_receiver() {
    let _reset = Reset;
    let reg = registry();
    let mut ctx = MockNativeContext::new();
    let inner = ctx.fresh_object_ref();
    let this_old = lnis_copy(&mut ctx, inner, -1);
    let this_new = lnis_copy(&mut ctx, inner, -1);
    W20B_MOVES.with(|m| m.set([(addr(this_old), addr(this_new)), (0, 0)]));
    // '\r' then 'x': the CR is delivered as '\n', counts a line, and
    // pushes 'x' back -- two writes after two wrapped reads.
    W20B_BYTES.with(|b| b.set([b'\r' as i32, b'x' as i32]));
    ctx.set_invoke_virtual_hook(w20b_hook);

    let cb = reg
        .find("java/io/LineNumberInputStream", "read", "()I")
        .expect("LineNumberInputStream.read()I registered");
    let out = cb(&mut ctx, &[Value::Object(Some(this_old))]);

    assert!(matches!(out, Ok(Some(Value::Int(10)))), "got {out:?}");
    assert!(matches!(ctx.get_field(this_new, 1), Value::Int(1)));
    assert!(matches!(ctx.get_field(this_new, 3), Value::Int(0x78)));
    assert!(matches!(ctx.get_field(this_old, 1), Value::Int(0)));
    assert!(matches!(ctx.get_field(this_old, 3), Value::Int(-1)));
    assert_eq!(ctx.native_pin_count_for_test(), 0, "every root is released");
}

#[test]
fn w20b_lnis_read_array_fills_the_moved_buffer() {
    let _reset = Reset;
    let reg = registry();
    let mut ctx = MockNativeContext::new();
    let inner = ctx.fresh_object_ref();
    let this_old = lnis_copy(&mut ctx, inner, -1);
    let this_new = lnis_copy(&mut ctx, inner, -1);
    let buf_old = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 4);
    let buf_new = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 4);
    W20B_MOVES.with(|m| {
        m.set([
            (addr(this_old), addr(this_new)),
            (addr(buf_old), addr(buf_new)),
        ])
    });
    W20B_BYTES.with(|b| b.set([b'\n' as i32, -1]));
    ctx.set_invoke_virtual_hook(w20b_hook);

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

    assert!(matches!(out, Ok(Some(Value::Int(1)))), "got {out:?}");
    assert!(matches!(ctx.get_array_element(buf_new, 0), Value::Int(10)));
    assert!(matches!(ctx.get_array_element(buf_old, 0), Value::Int(0)));
    assert!(matches!(ctx.get_field(this_new, 1), Value::Int(1)));
    assert!(matches!(ctx.get_field(this_old, 1), Value::Int(0)));
    assert_eq!(ctx.native_pin_count_for_test(), 0, "every root is released");
}

/// The pushback is part of `available()`'s answer; it must be read
/// before the wrapped stream's `available()` can move the receiver (the
/// vacated copy is clobbered here to what a collector leaves behind).
#[test]
fn w20b_lnis_available_counts_the_pushback_read_before_the_call() {
    let _reset = Reset;
    let reg = registry();
    let mut ctx = MockNativeContext::new();
    let inner = ctx.fresh_object_ref();
    let this = lnis_copy(&mut ctx, inner, 0x41);
    let vacated = lnis_copy(&mut ctx, inner, -1);
    // The wrapped call "moves" `this`: afterwards its entry address reads
    // like the garbage a collector leaves (no pushback). Nothing is
    // pinned here, so the move is modelled by clobbering that slot.
    fn clobber_hook(
        ctx: &mut MockNativeContext,
        receiver: ObjectRef,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> Option<MethodCallResult> {
        let answer = w20b_hook(ctx, receiver, method_name, descriptor, args);
        let target = W20B_MOVES.with(|m| m.get())[0].0;
        if target != 0 {
            // SAFETY: `target` is the address of a live mock object.
            let o = unsafe { ObjectRef::from_raw(target as *mut u8) };
            ctx.set_field(o, 3, Value::Int(-1));
        }
        answer
    }
    W20B_MOVES.with(|m| m.set([(addr(this), addr(vacated)), (0, 0)]));
    ctx.set_invoke_virtual_hook(clobber_hook);

    let cb = reg
        .find("java/io/LineNumberInputStream", "available", "()I")
        .expect("LineNumberInputStream.available()I registered");
    let out = cb(&mut ctx, &[Value::Object(Some(this))]);

    // (4 + 1) / 2 + 1 for the pushback held when the call was made.
    assert!(matches!(out, Ok(Some(Value::Int(3)))), "got {out:?}");
}
