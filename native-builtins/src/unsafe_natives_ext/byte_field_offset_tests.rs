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
fn encode_and_decode_round_trip_in_both_arms() {
    for slot in [0usize, 1, 2, 7, 1000] {
        assert_eq!(unsafe_encode_field_offset(false, slot), slot);
        assert_eq!(unsafe_decode_field_offset(false, slot), (slot, 0));
        let off = unsafe_encode_field_offset(true, slot);
        assert_eq!(off, UNSAFE_BYTE_FIELD_OFFSET_BASE + slot * 8);
        for k in 0..8 {
            assert_eq!(unsafe_decode_field_offset(true, off + k), (slot, k));
        }
    }
    // Below the base names no field: never slot 0.
    assert_ne!(unsafe_decode_field_offset(true, 3).0, 0);
    // The off arm is the identity for every offset.
    for off in [0usize, 1, 9, 17, 0x2000_0000] {
        assert_eq!(unsafe_decode_field_offset(false, off), (off, 0));
    }
}

#[test]
fn the_off_arm_keeps_slot_offsets() {
    let mut ctx = MockNativeContext::new();
    let obj = obj_with(&mut ctx, &[Value::Int(5), Value::Int(6), Value::Int(7)]);
    assert_eq!(unsafe_field_slot(&ctx, obj, 1), (1, 0));
    assert_eq!(
        native_unsafe_get_int(&mut ctx, &args(obj, 1, &[])).expect("getInt"),
        Some(Value::Int(6))
    );
    native_unsafe_put_int(&mut ctx, &args(obj, 2, &[Value::Int(9)])).expect("putInt");
    assert_eq!(ctx.get_field(obj, 2), Value::Int(9));
    assert_eq!(ctx.get_field(obj, 1), Value::Int(6));
}

#[test]
fn the_on_arm_addresses_the_bytes_inside_a_field() {
    let mut ctx = MockNativeContext::new();
    ctx.unsafe_byte_field_offsets_flag.set(true);
    let obj = obj_with(
        &mut ctx,
        &[Value::Int(0x1122_3344), Value::Long(0x0102_0304_0506_0708)],
    );
    let off_i = unsafe_encode_field_offset(true, 0);
    let off_l = unsafe_encode_field_offset(true, 1);
    assert_eq!(unsafe_field_slot(&ctx, obj, off_l + 4), (1, 4));
    // Whole fields.
    assert_eq!(
        native_unsafe_get_int(&mut ctx, &args(obj, off_i, &[])).expect("getInt"),
        Some(Value::Int(0x1122_3344))
    );
    assert_eq!(
        native_unsafe_get_long(&mut ctx, &args(obj, off_l, &[])).expect("getLong"),
        Some(Value::Long(0x0102_0304_0506_0708))
    );
    // Inside a field: its own bytes, little-endian (clamped to the field).
    assert_eq!(
        native_unsafe_get_int(&mut ctx, &args(obj, off_l + 4, &[])).expect("getInt +4"),
        Some(Value::Int(0x0102_0304))
    );
    assert_eq!(
        native_unsafe_get_int(&mut ctx, &args(obj, off_i + 1, &[])).expect("getInt +1"),
        Some(Value::Int(0x0011_2233))
    );
    // A store inside a field replaces only those bytes.
    native_unsafe_put_byte_mb(&mut ctx, &args(obj, off_i + 3, &[Value::Int(0x7f)]))
        .expect("putByte +3");
    assert_eq!(ctx.get_field(obj, 0), Value::Int(0x7f22_3344));
    native_unsafe_put_int(&mut ctx, &args(obj, off_l + 4, &[Value::Int(-1)]))
        .expect("putInt +4");
    assert_eq!(
        ctx.get_field(obj, 1),
        Value::Long(0xFFFF_FFFF_0506_0708_u64 as i64)
    );
    // An int CAS on the long field's low half (narrower than the field).
    assert_eq!(
        native_unsafe_cas_int(
            &mut ctx,
            &args(
                obj,
                off_l,
                &[Value::Int(0x0506_0708), Value::Int(0x0A0B_0C0D)]
            )
        )
        .expect("compareAndSetInt"),
        Some(Value::Int(1))
    );
    assert_eq!(
        ctx.get_field(obj, 1),
        Value::Long(0xFFFF_FFFF_0A0B_0C0D_u64 as i64)
    );
    // The first field is untouched by the second's accesses.
    assert_eq!(ctx.get_field(obj, 0), Value::Int(0x7f22_3344));
}

/// Interpreter round i4 wave 95, lane L3: a narrow read INSIDE a field is
/// typed as the access (`getShort` sign-extends, `getChar` zero-extends,
/// `getByte` one byte), not the int-width fallback's four bytes.
#[test]
fn i95_narrow_reads_inside_a_field_are_narrowed() {
    let mut ctx = MockNativeContext::new();
    ctx.unsafe_byte_field_offsets_flag.set(true);
    let obj = obj_with(&mut ctx, &[Value::Long(0x11F2_3344_5566_7788)]);
    let off_l = unsafe_encode_field_offset(true, 0);
    assert_eq!(
        native_unsafe_get_short_mb(&mut ctx, &args(obj, off_l + 2, &[])).expect("getShort +2"),
        Some(Value::Int(0x5566))
    );
    assert_eq!(
        native_unsafe_get_short_mb(&mut ctx, &args(obj, off_l + 6, &[])).expect("getShort +6"),
        Some(Value::Int(0x11F2))
    );
    assert_eq!(
        native_unsafe_get_char_mb(&mut ctx, &args(obj, off_l + 5, &[])).expect("getChar +5"),
        Some(Value::Int(0xF233))
    );
    assert_eq!(
        native_unsafe_get_byte_mb(&mut ctx, &args(obj, off_l + 6, &[])).expect("getByte +6"),
        Some(Value::Int(-14))
    );
    // Past a 2-byte read's end inside the field: clamped, as a getInt.
    assert_eq!(
        native_unsafe_get_short_mb(&mut ctx, &args(obj, off_l + 7, &[])).expect("getShort +7"),
        Some(Value::Int(0x11))
    );
}
