use super::*;

fn rooted(ctx: &MockNativeContext, root: usize) -> ObjectRef {
    ctx.resolve_global_root(root).expect("rooted")
}

/// `BufferedOutputStream.write(byte[], off, len)` with too little room
/// flushes first -- `out.write(buf, 0, count)`, arbitrary Java -- and then
/// copies the caller's array into the emptied buffer. The copy read the
/// array at its address from before the flush.
#[test]
fn w20f_bos_bulk_write_copies_from_the_moved_source_after_its_flush() {
    let mut ctx = MockNativeContext::new();
    let this = ctx.alloc_object(3);
    let out = ctx.alloc_object(0);
    let buf = ctx.new_array(ArrayElementType::Byte, 4);
    ctx.set_field(this, crate::BOS_FIELD_OUT, Value::Object(Some(out)));
    ctx.set_field(this, crate::BOS_FIELD_BUF, Value::Object(Some(buf)));
    ctx.set_field(this, crate::BOS_FIELD_COUNT, Value::Int(3));
    let src = ctx.new_array(ArrayElementType::Byte, 2);
    ctx.set_array_element(src, 0, Value::Int(7));
    ctx.set_array_element(src, 1, Value::Int(9));
    let this_root = ctx.add_global_root(this);
    ctx.w20f_relocate_on_invoke("write", "([BII)V");

    let r = crate::native_bos_write_bulk(
        &mut ctx,
        &[
            Value::Object(Some(this)),
            Value::Object(Some(src)),
            Value::Int(0),
            Value::Int(2),
        ],
    );

    assert!(matches!(r, Ok(None)), "got {r:?}");
    assert_eq!(
        ctx.relocation_count(),
        1,
        "the flush's write moved every object"
    );
    let moved = rooted(&ctx, this_root);
    assert!(matches!(
        ctx.get_field(moved, crate::BOS_FIELD_COUNT),
        Value::Int(2)
    ));
    let moved_buf = match ctx.get_field(moved, crate::BOS_FIELD_BUF) {
        Value::Object(Some(b)) => b,
        other => panic!("buf slot is {other:?}"),
    };
    assert_eq!(ctx.get_array_element(moved_buf, 0), Value::Int(7));
    assert_eq!(ctx.get_array_element(moved_buf, 1), Value::Int(9));
    assert_eq!(ctx.pinned_count(), 0, "no pin left behind");
}
