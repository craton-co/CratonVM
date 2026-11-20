use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_types::ClassId;

fn obj_with(ctx: &mut MockNativeContext, values: &[Value]) -> ObjectRef {
    let obj = ctx.alloc_object(ClassId::new(1), values.len());
    for (i, v) in values.iter().enumerate() {
        ctx.set_field(obj, i, *v);
    }
    obj
}

fn args(obj: ObjectRef, offset: usize, rest: &[Value]) -> Vec<Value> {
    let mut a = vec![
        Value::Object(None),
        Value::Object(Some(obj)),
        Value::Long(offset as i64),
    ];
    a.extend_from_slice(rest);
    a
}

#[test]
fn narrow_reads_of_a_wider_field_answer_the_narrow_value_in_both_encodings() {
    for byte_offsets in [true, false] {
        let mut ctx = MockNativeContext::new();
        ctx.unsafe_byte_field_offsets_flag.set(byte_offsets);
        let obj = obj_with(&mut ctx, &[Value::Long(0x1122_3344_AABB_CCDD)]);
        let off = unsafe_encode_field_offset(byte_offsets, 0);
        let a = args(obj, off, &[]);
        assert_eq!(
            native_unsafe_get_short_mb(&mut ctx, &a).expect("getShort"),
            Some(Value::Int(-13091)),
            "short-of-long, byte offsets {byte_offsets}"
        );
        assert_eq!(
            native_unsafe_get_char_mb(&mut ctx, &a).expect("getChar"),
            Some(Value::Int(0xCCDD))
        );
        assert_eq!(
            native_unsafe_get_byte_mb(&mut ctx, &a).expect("getByte"),
            Some(Value::Int(-35))
        );
        assert_eq!(
            native_unsafe_get_int_mb(&mut ctx, &a).expect("getInt"),
            Some(Value::Int(0xAABB_CCDD_u32 as i32))
        );
        assert_eq!(
            native_unsafe_get_short_volatile(&mut ctx, &a).expect("getShortVolatile"),
            Some(Value::Int(-13091))
        );
        assert_eq!(
            native_unsafe_get_char_volatile(&mut ctx, &a).expect("getCharVolatile"),
            Some(Value::Int(0xCCDD))
        );
        assert_eq!(
            native_unsafe_get_byte_volatile(&mut ctx, &a).expect("getByteVolatile"),
            Some(Value::Int(-35))
        );
    }
}

#[test]
fn narrow_reads_inside_a_field_take_only_their_own_bytes() {
    let mut ctx = MockNativeContext::new();
    ctx.unsafe_byte_field_offsets_flag.set(true);
    let obj = obj_with(&mut ctx, &[Value::Long(0x1122_3344_AABB_CCDD)]);
    let off = unsafe_encode_field_offset(true, 0);
    assert_eq!(
        native_unsafe_get_short_mb(&mut ctx, &args(obj, off + 2, &[])).expect("getShort +2"),
        Some(Value::Int(0xAABB_u16 as i16 as i32))
    );
    assert_eq!(
        native_unsafe_get_char_mb(&mut ctx, &args(obj, off + 6, &[])).expect("getChar +6"),
        Some(Value::Int(0x1122))
    );
    assert_eq!(
        native_unsafe_get_byte_mb(&mut ctx, &args(obj, off + 5, &[])).expect("getByte +5"),
        Some(Value::Int(0x33))
    );
}

#[test]
fn a_big_endian_unaligned_read_of_a_same_kind_field_is_swapped() {
    let mut ctx = MockNativeContext::new();
    ctx.unsafe_byte_field_offsets_flag.set(true);
    let obj = obj_with(&mut ctx, &[Value::Int(0x1122_3344)]);
    let off = unsafe_encode_field_offset(true, 0);
    assert_eq!(
        native_unsafe_get_int_mb(&mut ctx, &args(obj, off, &[Value::Int(1)]))
            .expect("getIntUnaligned big-endian"),
        Some(Value::Int(0x4433_2211))
    );
    assert_eq!(
        native_unsafe_get_int_mb(&mut ctx, &args(obj, off, &[Value::Int(0)]))
            .expect("getIntUnaligned little-endian"),
        Some(Value::Int(0x1122_3344))
    );
}

#[test]
fn int_volatile_accesses_of_a_long_field_take_its_low_half() {
    for byte_offsets in [true, false] {
        let mut ctx = MockNativeContext::new();
        ctx.unsafe_byte_field_offsets_flag.set(byte_offsets);
        let obj = obj_with(&mut ctx, &[Value::Long(0x1122_3344_5566_7788)]);
        let off = unsafe_encode_field_offset(byte_offsets, 0);
        assert_eq!(
            native_unsafe_get_int_volatile(&mut ctx, &args(obj, off, &[]))
                .expect("getIntVolatile"),
            Some(Value::Int(0x5566_7788))
        );
        native_unsafe_put_int_volatile(&mut ctx, &args(obj, off, &[Value::Int(-1)]))
            .expect("putIntVolatile");
        assert_eq!(
            ctx.get_field(obj, 0),
            Value::Long(0x1122_3344_FFFF_FFFF),
            "byte offsets {byte_offsets}"
        );
    }
}

/// uns21 R4: a primitive volatile store into a REFERENCE field is ignored
/// (as the field view does), not coerced to a cleared reference.
#[test]
fn a_primitive_volatile_store_into_a_reference_field_keeps_the_reference() {
    let mut ctx = MockNativeContext::new();
    let held = ctx.alloc_object(ClassId::new(1), 0);
    let obj = obj_with(&mut ctx, &[Value::Object(Some(held))]);
    native_unsafe_put_int_volatile(&mut ctx, &args(obj, 0, &[Value::Int(7)]))
        .expect("putIntVolatile");
    assert_eq!(ctx.get_field(obj, 0), Value::Object(Some(held)));
    native_unsafe_put_long_volatile(&mut ctx, &args(obj, 0, &[Value::Long(7)]))
        .expect("putLongVolatile");
    assert_eq!(ctx.get_field(obj, 0), Value::Object(Some(held)));
}

#[test]
fn short_atomics_on_a_char_array_element_compare_the_low_16_bits() {
    let mut ctx = MockNativeContext::new();
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Char, 3);
    ctx.set_array_element(arr, 1, Value::Int(0xFFFF));
    // compareAndSetChar(a, base + 2, 0xFFFF, 0x8001), chars as signed shorts.
    let a = args(
        arr,
        18,
        &[Value::Int(-1), Value::Int(0x8001_u16 as i16 as i32)],
    );
    assert_eq!(
        native_unsafe_cas_short(&mut ctx, &a).expect("compareAndSetShort"),
        Some(Value::Int(1))
    );
    assert_eq!(ctx.get_array_element(arr, 1), Value::Int(0x8001));
    // getAndAddShort(a, base + 2, -1): witness as a short, element zero-extended.
    assert_eq!(
        native_unsafe_get_and_add_short(&mut ctx, &args(arr, 18, &[Value::Int(-1)]))
            .expect("getAndAddShort"),
        Some(Value::Int(0x8001_u16 as i16 as i32))
    );
    assert_eq!(ctx.get_array_element(arr, 1), Value::Int(0x8000));
}

#[test]
fn a_narrow_cas_on_a_wider_field_is_a_lane_with_slot_offsets_too() {
    let mut ctx = MockNativeContext::new();
    let obj = obj_with(&mut ctx, &[Value::Int(0x1234_FFFE)]);
    // compareAndSetShort(o, slot 0, (short) -2, (short) 7): the low half only.
    assert_eq!(
        native_unsafe_cas_short(&mut ctx, &args(obj, 0, &[Value::Int(-2), Value::Int(7)]))
            .expect("compareAndSetShort"),
        Some(Value::Int(1))
    );
    assert_eq!(ctx.get_field(obj, 0), Value::Int(0x1234_0007));
}

#[test]
fn the_char_field_short_atomics_find_their_field_under_byte_offsets() {
    for byte_offsets in [true, false] {
        let mut ctx = MockNativeContext::new();
        ctx.unsafe_byte_field_offsets_flag.set(byte_offsets);
        let obj = obj_with(&mut ctx, &[Value::Int(0xFFFF)]);
        ctx.set_declared_fields(
            ClassId::new(1),
            vec![cratonvm_native_api::FieldMetadata {
                name: "c".to_string(),
                descriptor: "C".to_string(),
                access_flags: 0,
                slot_index: 0,
                declaring_class_id: ClassId::new(1),
                is_static: false,
            }],
        );
        let off = unsafe_encode_field_offset(byte_offsets, 0);
        // compareAndSetChar(o, off, 0xFFFF, 0x8001) reaches the Short
        // native with both chars narrowed to signed shorts (`c2s`).
        let cas = char_field_short_atomic(
            &mut ctx,
            &args(
                obj,
                off,
                &[Value::Int(-1), Value::Int(0x8001_u16 as i16 as i32)],
            ),
            ArenaAtomic::Cas,
        )
        .expect("a char field")
        .expect("no throw");
        assert_eq!(cas, Some(Value::Int(1)), "byte offsets {byte_offsets}");
        assert_eq!(ctx.get_field(obj, 0), Value::Int(0x8001));
    }
}
