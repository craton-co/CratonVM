//! Round 12 wave 6 (lane ffm3): `copyFrom`, `mismatch`, `getString` and
//! `setString` serve a heap (`ofArray`) segment through its backing array
//! (`r12w5-ffm2-bulk-segment-natives-ignore-heap-segments`); each used to
//! answer as if there were nothing to read or write.
use super::*;
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn obj(o: ObjectRef) -> Value {
    Value::Object(Some(o))
}

fn byte_array(ctx: &mut dyn NativeContext, bytes: &[i32]) -> ObjectRef {
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
    for (i, b) in bytes.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(*b));
    }
    arr
}

fn of_array(ctx: &mut dyn NativeContext, arr: ObjectRef) -> ObjectRef {
    match pe_of_array_alias(ctx, &[obj(arr)]) {
        Ok(Some(Value::Object(Some(seg)))) => seg,
        other => panic!("ofArray gave {other:?}"),
    }
}

fn bytes_of(ctx: &dyn NativeContext, arr: ObjectRef) -> Vec<Value> {
    (0..ctx.array_length(arr))
        .map(|i| ctx.get_array_element(arr, i))
        .collect()
}

#[test]
fn heap_segments_are_copied_compared_and_decoded_through_their_arrays() {
    let mut registry = cratonvm_native_api::NativeMethodRegistry::new();
    super::register_pe_memory_segment(&mut registry);
    let ms = super::CRATON_SEGMENT_CLASS;
    let find = |name: &str, desc: &str| {
        registry
            .find(ms, name, desc)
            .unwrap_or_else(|| panic!("{name}{desc} is not registered"))
    };
    let copy_from = find(
        "copyFrom",
        "(Ljava/lang/foreign/MemorySegment;)Ljava/lang/foreign/MemorySegment;",
    );
    let mismatch = find("mismatch", "(Ljava/lang/foreign/MemorySegment;)J");
    let get_string = find("getString", "(J)Ljava/lang/String;");
    let set_string = find("setString", "(JLjava/lang/String;)V");

    let mut ctx = mock_ctx();
    let a_arr = byte_array(&mut ctx, &[1, 2, 3, 0]);
    let a = of_array(&mut ctx, a_arr);
    let b_arr = byte_array(&mut ctx, &[0, 0, 0, 0]);
    let b = of_array(&mut ctx, b_arr);
    let c_arr = byte_array(&mut ctx, &[1, 2, 9, 0]);
    let c = of_array(&mut ctx, c_arr);
    let wide_arr = byte_array(&mut ctx, &[7, 7, 7, 7, 7, 7]);
    let wide = of_array(&mut ctx, wide_arr);

    // copyFrom: HotSpot copies; this used to leave `b` all zero.
    assert!(copy_from(&mut ctx, &[obj(b), obj(a)]).is_ok());
    assert_eq!(
        bytes_of(&ctx, b_arr),
        vec![Value::Int(1), Value::Int(2), Value::Int(3), Value::Int(0)]
    );
    // A larger destination is legal (`copy(src, 0, this, 0, src.byteSize())`),
    // a smaller one is out of bounds.
    assert!(copy_from(&mut ctx, &[obj(wide), obj(a)]).is_ok());
    assert_eq!(
        bytes_of(&ctx, wide_arr),
        vec![
            Value::Int(1),
            Value::Int(2),
            Value::Int(3),
            Value::Int(0),
            Value::Int(7),
            Value::Int(7)
        ]
    );
    let err = match copy_from(&mut ctx, &[obj(a), obj(wide)]) {
        Ok(v) => panic!("a 6-byte source fit a 4-byte segment: {v:?}"),
        Err(err) => err,
    };
    assert!(format!("{err:?}").contains("IndexOutOfBounds"), "{err:?}");

    // mismatch: same contents -1, a differing byte its index, a prefix
    // the common length. The length rule used to answer -1 for all three
    // same-sized pairs.
    assert_eq!(
        mismatch(&mut ctx, &[obj(a), obj(b)]).ok(),
        Some(Some(Value::Long(-1)))
    );
    assert_eq!(
        mismatch(&mut ctx, &[obj(a), obj(c)]).ok(),
        Some(Some(Value::Long(2)))
    );
    assert_eq!(
        mismatch(&mut ctx, &[obj(a), obj(wide)]).ok(),
        Some(Some(Value::Long(4)))
    );

    // getString decodes up to the NUL; it answered null.
    let text = match get_string(&mut ctx, &[obj(a), Value::Long(0)]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
        other => panic!("getString gave {other:?}"),
    };
    assert_eq!(text.as_deref(), Some("\u{1}\u{2}\u{3}"));

    // setString writes the bytes and the NUL; it was a silent no-op.
    let hi = ctx.create_string("hi");
    assert!(set_string(&mut ctx, &[obj(b), Value::Long(1), obj(hi)]).is_ok());
    assert_eq!(
        bytes_of(&ctx, b_arr),
        vec![
            Value::Int(1),
            Value::Int(104),
            Value::Int(105),
            Value::Int(0)
        ]
    );
}

/// A negative length is the JDK's bounds failure, not the size cap's
/// `IllegalStateException`.
#[test]
fn a_negative_copy_length_is_out_of_bounds() {
    let mut registry = cratonvm_native_api::NativeMethodRegistry::new();
    super::register_pe_memory_segment(&mut registry);
    let copy = registry
        .find(
            super::CRATON_SEGMENT_CLASS,
            "copy",
            "(Ljava/lang/foreign/MemorySegment;JLjava/lang/foreign/MemorySegment;JJ)V",
        )
        .unwrap_or_else(|| panic!("copy is not registered"));
    let mut ctx = mock_ctx();
    let src_arr = byte_array(&mut ctx, &[1, 2, 3, 4]);
    let src = of_array(&mut ctx, src_arr);
    let dst_arr = byte_array(&mut ctx, &[0, 0, 0, 0]);
    let dst = of_array(&mut ctx, dst_arr);
    let err = match copy(
        &mut ctx,
        &[
            obj(src),
            Value::Long(0),
            obj(dst),
            Value::Long(0),
            Value::Long(-1),
        ],
    ) {
        Ok(v) => panic!("a negative copy length answered {v:?}"),
        Err(err) => err,
    };
    assert!(format!("{err:?}").contains("IndexOutOfBounds"), "{err:?}");    }