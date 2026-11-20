use super::*;
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_native_api::{NativeContext, NativeMethodRegistry};

#[test]
fn char_buffer_capacity10_put5_flip_get5_round_trip() {
    // Wire up just the CharBuffer natives the production code registers.
    let mut reg = NativeMethodRegistry::new();
    register_p62_char_buffer(&mut reg);

    let mut ctx = mock_ctx();

    // allocate(10) — mirrors `CharBuffer.allocate(10)`.
    let allocate = reg
        .find(
            "java/nio/CharBuffer",
            "allocate",
            "(I)Ljava/nio/CharBuffer;",
        )
        .expect("CharBuffer.allocate(I) must be registered");
    let buf = match allocate(&mut ctx, &[Value::Int(10)]).expect("allocate ok") {
        Some(Value::Object(Some(b))) => b,
        other => panic!("allocate returned {other:?}"),
    };

    // Freshly-allocated buffer must have capacity=10, limit=10, position=0,
    // mark=-1 — this is the invariant that Buffer.checkIndex relies on.
    assert_eq!(ctx.get_field(buf, CB_FIELD_CAPACITY), Value::Int(10));
    assert_eq!(ctx.get_field(buf, CB_FIELD_LIMIT), Value::Int(10));
    assert_eq!(ctx.get_field(buf, CB_FIELD_POS), Value::Int(0));
    assert_eq!(ctx.get_field(buf, CB_FIELD_MARK), Value::Int(-1));

    // put(C) five times — values 'A'..'E'.
    let put = reg
        .find("java/nio/CharBuffer", "put", "(C)Ljava/nio/CharBuffer;")
        .expect("CharBuffer.put(C) must be registered");
    for ch in b'A'..=b'E' {
        put(&mut ctx, &[Value::Object(Some(buf)), Value::Int(ch as i32)]).expect("put ok");
    }
    assert_eq!(ctx.get_field(buf, CB_FIELD_POS), Value::Int(5));

    // flip() — limit := pos, pos := 0.
    let flip = reg
        .find("java/nio/CharBuffer", "flip", "()Ljava/nio/CharBuffer;")
        .expect("CharBuffer.flip() must be registered");
    flip(&mut ctx, &[Value::Object(Some(buf))]).expect("flip ok");
    assert_eq!(ctx.get_field(buf, CB_FIELD_LIMIT), Value::Int(5));
    assert_eq!(ctx.get_field(buf, CB_FIELD_POS), Value::Int(0));

    // get() five times — must return 'A'..'E' in order.
    let get = reg
        .find("java/nio/CharBuffer", "get", "()C")
        .expect("CharBuffer.get() must be registered");
    let mut out = Vec::with_capacity(5);
    for _ in 0..5 {
        match get(&mut ctx, &[Value::Object(Some(buf))]).expect("get ok") {
            Some(Value::Int(v)) => out.push(v as u8),
            other => panic!("get returned {other:?}"),
        }
    }
    assert_eq!(out, b"ABCDE".to_vec());
    // Position advanced to limit; a further get() would underflow.
    assert_eq!(ctx.get_field(buf, CB_FIELD_POS), Value::Int(5));
}
