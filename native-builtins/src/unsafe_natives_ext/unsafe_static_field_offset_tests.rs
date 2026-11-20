use super::*;
use crate::test_utils::MockNativeContext;
use cratonvm_native_api::FieldMetadata;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn static_field_offset_uses_static_storage_not_class_mirror_slots() {
    let mut ctx = MockNativeContext::new();
    let class_id = ctx
        .ensure_class_initialized("sun/misc/Unsafe")
        .expect("mock class id");
    let meta = FieldMetadata {
        name: "memoryAccessWarned".to_string(),
        descriptor: "Z".to_string(),
        access_flags: 0x0008,
        slot_index: 24,
        declaring_class_id: class_id,
        is_static: true,
    };
    ctx.set_static_field(class_id, 24, Value::Int(0));

    let field = crate::lang_class::create_field_object(&mut ctx, &meta);
    let offset = match native_unsafe_static_field_offset(
        &mut ctx,
        &[Value::Object(None), Value::Object(Some(field))],
    )
    .expect("staticFieldOffset")
    .expect("offset value")
    {
        Value::Long(offset) => offset as usize,
        other => panic!("unexpected offset value: {other:?}"),
    };

    assert!(is_synthetic_offset(offset));
    assert_ne!(offset, 24);

    let base = ctx.get_class_mirror(class_id);
    ctx.set_field(base, 24, Value::Int(77));

    let before = native_unsafe_get_int_volatile(
        &mut ctx,
        &[
            Value::Object(None),
            Value::Object(Some(base)),
            Value::Long(offset as i64),
        ],
    )
    .expect("getBooleanVolatile")
    .expect("read value");
    assert_eq!(before, Value::Int(0));

    let cas = native_unsafe_cas_int(
        &mut ctx,
        &[
            Value::Object(None),
            Value::Object(Some(base)),
            Value::Long(offset as i64),
            Value::Int(0),
            Value::Int(1),
        ],
    )
    .expect("compareAndSetBoolean")
    .expect("cas value");
    assert_eq!(cas, Value::Int(1));
    assert_eq!(ctx.get_static_field(class_id, 24), Value::Int(1));
    assert_eq!(ctx.get_field(base, 24), Value::Int(77));
}

#[test]
fn static_long_null_base_unsafe_uses_real_static_storage() {
    let mut ctx = MockNativeContext::new();
    let class_id = ctx
        .ensure_class_initialized("java/lang/Thread$ThreadIdentifiers")
        .expect("mock class id");
    let meta = FieldMetadata {
        name: "next".to_string(),
        descriptor: "J".to_string(),
        access_flags: 0x0008,
        slot_index: 7,
        declaring_class_id: class_id,
        is_static: true,
    };
    ctx.set_static_field(class_id, 7, Value::Long(42));

    let field = crate::lang_class::create_field_object(&mut ctx, &meta);
    let offset = match native_unsafe_static_field_offset(
        &mut ctx,
        &[Value::Object(None), Value::Object(Some(field))],
    )
    .expect("staticFieldOffset")
    .expect("offset value")
    {
        Value::Long(offset) => offset as usize,
        other => panic!("unexpected offset value: {other:?}"),
    };

    let before = native_unsafe_get_long_volatile(
        &mut ctx,
        &[
            Value::Object(None),
            Value::Object(None),
            Value::Long(offset as i64),
        ],
    )
    .expect("getLongVolatile")
    .expect("read value");
    assert_eq!(before, Value::Long(42));

    let old = native_unsafe_get_and_add_long(
        &mut ctx,
        &[
            Value::Object(None),
            Value::Object(None),
            Value::Long(offset as i64),
            Value::Long(1),
        ],
    )
    .expect("getAndAddLong")
    .expect("old value");

    assert_eq!(old, Value::Long(42));
    assert_eq!(ctx.get_static_field(class_id, 7), Value::Long(43));
}

#[test]
fn thread_next_tid_offset_seeds_positive_null_base_counter() {
    let mut ctx = MockNativeContext::new();
    // A private VM identity: the counter is per VM since gc-common w8-a,
    // and mock contexts otherwise all share identity 0.
    let vm = 0x7A80_0101;
    ctx.set_vm_identity(vm);
    let offset = thread_next_tid_offset(vm);
    // `&mut ctx`: this test OWNS its MockNativeContext, unlike the 21
    // production call sites which already hold a `&mut dyn NativeContext`.
    let _ = note_unsafe_side_store_offset(&mut ctx, offset, line!(), 3);
    lock_static_long_shard(offset).insert((vm, offset), 1);

    let first = native_unsafe_get_and_add_long(
        &mut ctx,
        &[
            Value::Object(None),
            Value::Object(None),
            Value::Long(offset as i64),
            Value::Long(1),
        ],
    )
    .expect("first getAndAddLong")
    .expect("first old value");
    let second = native_unsafe_get_and_add_long(
        &mut ctx,
        &[
            Value::Object(None),
            Value::Object(None),
            Value::Long(offset as i64),
            Value::Long(1),
        ],
    )
    .expect("second getAndAddLong")
    .expect("second old value");

    assert_ne!(offset, 0);
    assert_eq!(first, Value::Long(1));
    assert_eq!(second, Value::Long(2));
    crate::forget_vm_unsafe_side_stores(vm);
}

/// Same shape as `static_long_null_base_unsafe_uses_real_static_storage`
/// above, for an int static — the null-base int accessors previously
/// serviced `static_int_store` unconditionally, invisible to putstatic.
#[test]
fn static_int_null_base_unsafe_uses_real_static_storage() {
    let mut ctx = MockNativeContext::new();
    let class_id = ctx
        .ensure_class_initialized("com/example/IntStaticHolder")
        .expect("mock class id");
    let meta = FieldMetadata {
        name: "counter".to_string(),
        descriptor: "I".to_string(),
        access_flags: 0x0008,
        slot_index: 0,
        declaring_class_id: class_id,
        is_static: true,
    };
    ctx.set_static_field(class_id, 0, Value::Int(10));

    let field = crate::lang_class::create_field_object(&mut ctx, &meta);
    let offset = match native_unsafe_static_field_offset(
        &mut ctx,
        &[Value::Object(None), Value::Object(Some(field))],
    )
    .expect("staticFieldOffset")
    .expect("offset value")
    {
        Value::Long(offset) => offset as usize,
        other => panic!("unexpected offset value: {other:?}"),
    };

    let before = native_unsafe_get_int_volatile(
        &mut ctx,
        &[
            Value::Object(None),
            Value::Object(None),
            Value::Long(offset as i64),
        ],
    )
    .expect("getIntVolatile")
    .expect("read value");
    assert_eq!(before, Value::Int(10));

    let old = native_unsafe_get_and_add_int(
        &mut ctx,
        &[
            Value::Object(None),
            Value::Object(None),
            Value::Long(offset as i64),
            Value::Int(1),
        ],
    )
    .expect("getAndAddInt")
    .expect("old value");
    assert_eq!(old, Value::Int(10));
    assert_eq!(ctx.get_static_field(class_id, 0), Value::Int(11));

    let cas_ok = native_unsafe_cas_int(
        &mut ctx,
        &[
            Value::Object(None),
            Value::Object(None),
            Value::Long(offset as i64),
            Value::Int(11),
            Value::Int(99),
        ],
    )
    .expect("compareAndSetInt")
    .expect("cas result");
    assert_eq!(cas_ok, Value::Int(1));
    assert_eq!(ctx.get_static_field(class_id, 0), Value::Int(99));
}

/// Same shape, for a static Object field.
#[test]
fn static_object_null_base_unsafe_uses_real_static_storage() {
    let mut ctx = MockNativeContext::new();
    let class_id = ctx
        .ensure_class_initialized("com/example/ObjectStaticHolder")
        .expect("mock class id");
    let meta = FieldMetadata {
        name: "value".to_string(),
        descriptor: "Ljava/lang/Object;".to_string(),
        access_flags: 0x0008,
        slot_index: 0,
        declaring_class_id: class_id,
        is_static: true,
    };
    let seeded = ctx.create_string("seed");
    ctx.set_static_field(class_id, 0, Value::Object(Some(seeded)));

    let field = crate::lang_class::create_field_object(&mut ctx, &meta);
    let offset = match native_unsafe_static_field_offset(
        &mut ctx,
        &[Value::Object(None), Value::Object(Some(field))],
    )
    .expect("staticFieldOffset")
    .expect("offset value")
    {
        Value::Long(offset) => offset as usize,
        other => panic!("unexpected offset value: {other:?}"),
    };

    let before = native_unsafe_get_object_volatile(
        &mut ctx,
        &[
            Value::Object(None),
            Value::Object(None),
            Value::Long(offset as i64),
        ],
    )
    .expect("getObjectVolatile")
    .expect("read value");
    assert_eq!(before, Value::Object(Some(seeded)));

    let replacement = ctx.create_string("replacement");
    let prev = native_unsafe_get_and_set_object(
        &mut ctx,
        &[
            Value::Object(None),
            Value::Object(None),
            Value::Long(offset as i64),
            Value::Object(Some(replacement)),
        ],
    )
    .expect("getAndSetObject")
    .expect("previous value");
    assert_eq!(prev, Value::Object(Some(seeded)));
    assert_eq!(
        ctx.get_static_field(class_id, 0),
        Value::Object(Some(replacement))
    );
}
