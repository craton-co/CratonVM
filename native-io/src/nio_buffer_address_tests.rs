use super::*;
use crate::test_support::MockNativeContext;
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};

#[test]
fn buf_set_mark_preserves_a_real_jdk_buffer_address() {
    let mut ctx = MockNativeContext::new();
    ctx.alias_nio_buffer_fields();
    let buf = ctx.alloc_object_with_class(8, "java/nio/HeapCharBuffer");
    // A SLICE: address is arrayBaseOffset + offset * scale, not the bare
    // base offset, so a fix that rewrites a constant 16 would corrupt it.
    ctx.set_field_by_name(buf, "address", Value::Long(116));

    buf_set_mark(&mut ctx, buf, -1);

    assert_eq!(
        ctx.get_field_by_name(buf, "address"),
        Value::Long(116),
        "the mark write must not land on `address`"
    );
    assert_eq!(ctx.get_field_by_name(buf, "mark"), Value::Int(-1));
    assert_eq!(
        ctx.get_field(buf, BB_FIELD_MARK),
        Value::Int(-1),
        "the synthetic indexed slot still has to be written"
    );
}

#[test]
fn buf_set_mark_leaves_a_synthetic_buffer_without_an_address() {
    let mut ctx = MockNativeContext::new();
    // Synthetic mode: no by-name `address` field exists at all. Nothing
    // should be fabricated for it.
    let buf = ctx.alloc_object(BB_NUM_FIELDS);

    buf_set_mark(&mut ctx, buf, 7);

    assert_eq!(ctx.get_field(buf, BB_FIELD_MARK), Value::Int(7));
    assert_eq!(
        ctx.get_field_by_name(buf, "address"),
        Value::Object(None),
        "no address field means no address write"
    );
}

#[test]
fn buf_set_mark_survives_a_whole_mutator_sequence() {
    let mut ctx = MockNativeContext::new();
    ctx.alias_nio_buffer_fields();
    let buf = ctx.alloc_object_with_class(8, "java/nio/HeapByteBuffer");
    ctx.set_field_by_name(buf, "address", Value::Long(16));

    // flip / clear / rewind / mark / reset all funnel through buf_set_mark;
    // before the fix each one of them reset `address` to the mark value.
    for v in [-1, 5, -1, 0, -1] {
        buf_set_mark(&mut ctx, buf, v);
        assert_eq!(
            ctx.get_field_by_name(buf, "address"),
            Value::Long(16),
            "address survives mark={}",
            v
        );
    }
}

#[test]
fn allocators_give_every_heap_buffer_family_an_address() {
    let mut ctx = MockNativeContext::new();
    ctx.alias_nio_buffer_fields();

    let bb = alloc_byte_buffer(&mut ctx, 32);
    assert_eq!(ctx.get_field_by_name(bb, "address"), Value::Long(16));

    // The typed families used to skip this entirely, which left `address`
    // reading as the mark (-1) and made every bulk put throw AIOOBE.
    for (cls, et) in [
        ("java/nio/HeapCharBuffer", ArrayElementType::Char),
        ("java/nio/HeapShortBuffer", ArrayElementType::Short),
        ("java/nio/HeapIntBuffer", ArrayElementType::Int),
        ("java/nio/HeapLongBuffer", ArrayElementType::Long),
        ("java/nio/HeapFloatBuffer", ArrayElementType::Float),
        ("java/nio/HeapDoubleBuffer", ArrayElementType::Double),
    ] {
        let b = alloc_typed_buffer(&mut ctx, cls, et, 16);
        assert_eq!(
            ctx.get_field_by_name(b, "address"),
            Value::Long(16),
            "{} must carry the array base offset",
            cls
        );
        assert_ne!(
            ctx.get_field_by_name(b, "address"),
            Value::Long(-1),
            "{} must not read back the mark",
            cls
        );
    }

    let mbb = alloc_mapped_byte_buffer(&mut ctx, 32);
    assert_eq!(ctx.get_field_by_name(mbb, "address"), Value::Long(16));
}
