use super::*;
use crate::test_utils::MockNativeContext;
use cratonvm_native_api::FieldMetadata;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_types::ClassId;

/// A private VM identity per test, so the process-wide offset table rows
/// of one test never answer for another's.
const VM_BASE: usize = 0x7A21_5000;
const STATIC_INDEX: usize = 3;

/// One static of descriptor `desc` holding `value`, registered at
/// `offset` for VM `vm`; answers the static's class and its mirror.
fn one_static(
    ctx: &mut MockNativeContext,
    vm: usize,
    offset: usize,
    desc: &str,
    value: Value,
) -> (ClassId, ObjectRef) {
    ctx.set_vm_identity(vm);
    let class_id = ClassId::new(4321);
    ctx.set_declared_fields(
        class_id,
        vec![FieldMetadata {
            name: "S".to_string(),
            descriptor: desc.to_string(),
            access_flags: 0x0008,
            slot_index: STATIC_INDEX,
            declaring_class_id: class_id,
            is_static: true,
        }],
    );
    ctx.set_static_field(class_id, STATIC_INDEX, value);
    remember_unsafe_static_field_offset(vm, offset, class_id, STATIC_INDEX);
    let mirror = ctx.get_class_mirror(class_id);
    (class_id, mirror)
}

fn args(base: ObjectRef, offset: usize, rest: &[Value]) -> Vec<Value> {
    let mut a = vec![
        Value::Object(None),
        Value::Object(Some(base)),
        Value::Long(offset as i64),
    ];
    a.extend_from_slice(rest);
    a
}

/// The offset each encoding answers: a minted one (slot offsets) or a
/// scaled, 8-aligned one (byte field offsets).
fn offset_for(byte_offsets: bool, n: usize) -> usize {
    if byte_offsets {
        UNSAFE_BYTE_STATIC_OFFSET_BASE + 0x0100_0000 + n * 8
    } else {
        SYNTHETIC_OFFSET_BASE + 0x0700_0000 + n
    }
}

type Put = fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult;

/// `(put native, static descriptor, initial, stored operand, expected)`
/// -- the probe's `put-plain` / `put-volatile` static rows at `k = 0`.
fn rows() -> Vec<(&'static str, Put, &'static str, Value, Value, Value)> {
    vec![
        // static-short, putByte(-99): HotSpot keeps the high byte.
        (
            "putByte short",
            native_unsafe_put_byte_mb as Put,
            "S",
            Value::Int(0x7F01),
            Value::Int(-99),
            Value::Int(0x7F9D),
        ),
        (
            "putByteVolatile short",
            native_unsafe_put_byte_volatile as Put,
            "S",
            Value::Int(-2403_i32 & !0xFF | 0x01),
            Value::Int(-99),
            Value::Int(-2403),
        ),
        // static-short, putChar(43366): the 16 bits as a short.
        (
            "putChar short",
            native_unsafe_put_char_mb as Put,
            "S",
            Value::Int(0),
            Value::Int(43366),
            Value::Int(-22170),
        ),
        // static-char, putShort(-20340): the 16 bits as a char.
        (
            "putShort char",
            native_unsafe_put_short_mb as Put,
            "C",
            Value::Int(0),
            Value::Int(-20340),
            Value::Int(45196),
        ),
        (
            "putShortVolatile char",
            native_unsafe_put_short_volatile as Put,
            "C",
            Value::Int(0xFFFF),
            Value::Int(-20340),
            Value::Int(45196),
        ),
        // static-char, putByte(-99) into 0x9E01: the high byte stays.
        (
            "putByte char",
            native_unsafe_put_byte_mb as Put,
            "C",
            Value::Int(0x9E01),
            Value::Int(-99),
            Value::Int(0x9E9D),
        ),
        // static-int, putShort / putChar / putByte at its offset.
        (
            "putShort int",
            native_unsafe_put_short_mb as Put,
            "I",
            Value::Int(0x1122_3344),
            Value::Int(-20340),
            Value::Int(0x1122_B08C),
        ),
        (
            "putChar int",
            native_unsafe_put_char_mb as Put,
            "I",
            Value::Int(-2104934499),
            Value::Int(43366),
            Value::Int((-2104934499_i32 & !0xFFFF) | 43366),
        ),
        (
            "putByteVolatile int",
            native_unsafe_put_byte_volatile as Put,
            "I",
            Value::Int(0x1122_3344),
            Value::Int(-99),
            Value::Int(0x1122_339D),
        ),
        // static-long, putByte / putInt / putIntVolatile at its offset.
        (
            "putByte long",
            native_unsafe_put_byte_mb as Put,
            "J",
            Value::Long(0x1122_3344_5566_7788),
            Value::Int(-99),
            Value::Long(0x1122_3344_5566_779D),
        ),
        (
            "putInt long",
            native_unsafe_put_int_mb as Put,
            "J",
            Value::Long(0x1122_3344_5566_7788),
            Value::Int(-1),
            Value::Long(0x1122_3344_FFFF_FFFF),
        ),
        (
            "putIntVolatile long",
            native_unsafe_put_int_volatile_arena as Put,
            "J",
            Value::Long(0x1122_3344_5566_7788),
            Value::Int(-1),
            Value::Long(0x1122_3344_FFFF_FFFF),
        ),
        // Same width, same kind: unchanged (the whole static).
        (
            "putByte byte",
            native_unsafe_put_byte_mb as Put,
            "B",
            Value::Int(5),
            Value::Int(-99),
            Value::Int(-99),
        ),
        (
            "putInt int",
            native_unsafe_put_int_mb as Put,
            "I",
            Value::Int(5),
            Value::Int(-7),
            Value::Int(-7),
        ),
    ]
}

#[test]
fn a_narrow_store_at_a_statics_own_offset_merges_into_the_static() {
    for (enc, byte_offsets) in [false, true].into_iter().enumerate() {
        for (n, (label, put, desc, initial, operand, expected)) in
            rows().into_iter().enumerate()
        {
            let vm = VM_BASE + enc * 0x100 + n;
            let mut ctx = MockNativeContext::new();
            ctx.unsafe_byte_field_offsets_flag.set(byte_offsets);
            let offset = offset_for(byte_offsets, n);
            let (class_id, mirror) = one_static(&mut ctx, vm, offset, desc, initial);
            put(&mut ctx, &args(mirror, offset, &[operand])).expect(label);
            assert_eq!(
                ctx.get_static_field(class_id, STATIC_INDEX),
                expected,
                "{label}, byte offsets {byte_offsets}"
            );
            forget_vm_unsafe_static_field_targets(vm);
        }
    }
}

/// The narrow compare-and-exchange witness on a miss is the access's own
/// Java value (`(byte) 0x9D` = -99), in every tier. HotSpot 25's C2
/// intrinsic (`lock cmpxchgb` into `rax` = the expected value, no
/// sign-extension) answers `(expected & ~0xFF) | 0x9D` (157 for a positive
/// expected) once it has compiled the site; its interpreter answers -99.
/// The probe's `cae` rows hash both phases, so no VM matches them
/// deterministically; this VM keeps the JLS value.
#[test]
fn a_narrow_compare_and_exchange_witness_is_the_access_value() {
    for (enc, byte_offsets) in [false, true].into_iter().enumerate() {
        let vm = VM_BASE + 0x1000 + enc;
        let mut ctx = MockNativeContext::new();
        ctx.unsafe_byte_field_offsets_flag.set(byte_offsets);
        let offset = offset_for(byte_offsets, 40);
        let (class_id, mirror) = one_static(&mut ctx, vm, offset, "I", Value::Int(0x1122_339D));
        // compareAndExchangeByte(base, off, (byte) 63, (byte) 1): a miss.
        let w = native_unsafe_cae_byte(
            &mut ctx,
            &args(mirror, offset, &[Value::Int(63), Value::Int(1)]),
        )
        .expect("compareAndExchangeByte");
        assert_eq!(w, Some(Value::Int(-99)), "byte offsets {byte_offsets}");
        assert_eq!(
            ctx.get_static_field(class_id, STATIC_INDEX),
            Value::Int(0x1122_339D)
        );
        // compareAndExchangeShort(base, off, (short) 16328, (short) 1): a miss.
        let w = native_unsafe_cae_short(
            &mut ctx,
            &args(mirror, offset, &[Value::Int(16328), Value::Int(1)]),
        )
        .expect("compareAndExchangeShort");
        assert_eq!(w, Some(Value::Int(0x339D)), "byte offsets {byte_offsets}");
        forget_vm_unsafe_static_field_targets(vm);
    }
}
