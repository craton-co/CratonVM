use super::*;

#[test]
fn field_round_trip_via_slot_index() {
    let mut ctx = MockNativeContext::new();
    let obj = ctx.alloc_object(ClassId::new(0), 4);
    // Unset slots default to Int(0).
    assert_eq!(ctx.get_field(obj, 2), Value::Int(0));
    ctx.set_field(obj, 2, Value::Long(0x1234_5678_9abc_def0));
    assert_eq!(ctx.get_field(obj, 2), Value::Long(0x1234_5678_9abc_def0));
    // Slot 2 round-trip must not disturb slot 1.
    assert_eq!(ctx.get_field(obj, 1), Value::Int(0));
}

#[test]
fn identity_hash_is_stable_per_object_and_distinct_across_objects() {
    let ctx = MockNativeContext::new();
    let a = ctx.fresh_object_ref();
    let b = ctx.fresh_object_ref();
    let ha1 = ctx.identity_hash_code(a);
    let ha2 = ctx.identity_hash_code(a);
    let hb = ctx.identity_hash_code(b);
    // Same object → same hash on repeated lookup.
    assert_eq!(ha1, ha2, "identity hash must be stable across calls");
    // Distinct objects → distinct hashes (counter is monotonic).
    assert_ne!(ha1, hb, "distinct objects must hash differently");
}

#[test]
fn invoke_virtual_returns_scripted_result_then_default() {
    let mut ctx = MockNativeContext::new();
    let recv = ctx.fresh_object_ref();
    ctx.set_invoke_virtual_result(Ok(Some(Value::Int(42))));

    // First call drains the scripted result.
    let first = ctx
        .invoke_virtual(recv, "magicAnswer", "()I", &[])
        .expect("scripted Ok result");
    assert_eq!(first, Some(Value::Int(42)));

    // Second call falls back to Ok(None) (Java void).
    let second = ctx
        .invoke_virtual(recv, "magicAnswer", "()I", &[])
        .expect("default Ok(None) on unscripted call");
    assert_eq!(second, None);
}

#[test]
fn default_atomic_fetch_add_int_drives_cas_loop_against_mock() {
    // The default `atomic_fetch_add_int` impl in `NativeContext` is a
    // CAS retry loop over `get_field_volatile` + `compare_and_swap_field`.
    // The mock's HashMap-backed field store is exactly what the loop
    // needs to make progress, so this exercises the trait default
    // through the mock rather than a re-implementation.
    let mut ctx = MockNativeContext::new();
    let obj = ctx.alloc_object(ClassId::new(0), 1);
    ctx.set_field(obj, 0, Value::Int(10));

    let prev = ctx
        .atomic_fetch_add_int(obj, 0, 5)
        .expect("CAS loop should succeed against a fresh Int field");
    assert_eq!(prev, 10, "fetch_add should return the previous value");
    assert_eq!(ctx.get_field(obj, 0), Value::Int(15));
}

fn fill_int_array(ctx: &MockNativeContext, arr: ObjectRef, values: &[i32]) {
    for (i, value) in values.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(*value));
    }
}

fn read_int_array(ctx: &MockNativeContext, arr: ObjectRef) -> Vec<i32> {
    (0..ctx.array_length(arr))
        .map(|i| match ctx.get_array_element(arr, i) {
            Value::Int(value) => value,
            other => panic!("expected Int array element, got {other:?}"),
        })
        .collect()
}

#[test]
fn default_native_memory_copy_fails_closed() {
    let mut ctx = MockNativeContext::new();
    let src = [1u8, 2, 3, 4];
    let mut out = [0xAAu8; 4];

    assert!(!ctx.copy_from_native_memory(src.as_ptr() as i64, &mut out));
    assert_eq!(out, [0xAA; 4], "failed read must not mutate output");

    let mut dst = [0u8; 4];
    assert!(!ctx.copy_to_native_memory(dst.as_mut_ptr() as i64, &[9, 8, 7, 6]));
    assert_eq!(dst, [0; 4], "failed write must not touch destination");
}

#[test]
fn default_bulk_array_copy_copies_matching_primitive_arrays() {
    let mut ctx = MockNativeContext::new();
    let src = ctx.new_array(ArrayElementType::Int, 4);
    let dst = ctx.new_array(ArrayElementType::Int, 4);
    fill_int_array(&ctx, src, &[10, 20, 30, 40]);

    assert!(ctx.bulk_array_copy(src, 1, dst, 0, 3));
    assert_eq!(read_int_array(&ctx, dst), vec![20, 30, 40, 0]);
}

#[test]
fn default_bulk_array_copy_handles_same_array_overlap() {
    let mut ctx = MockNativeContext::new();
    let arr = ctx.new_array(ArrayElementType::Int, 5);
    fill_int_array(&ctx, arr, &[1, 2, 3, 4, 5]);

    assert!(ctx.bulk_array_copy(arr, 0, arr, 1, 4));
    assert_eq!(read_int_array(&ctx, arr), vec![1, 1, 2, 3, 4]);
}

#[test]
fn default_bulk_array_copy_rejects_length_and_offset_overflow() {
    let mut ctx = MockNativeContext::new();
    let src = ctx.new_array(ArrayElementType::Int, 3);
    let dst = ctx.new_array(ArrayElementType::Int, 2);
    fill_int_array(&ctx, src, &[1, 2, 3]);
    fill_int_array(&ctx, dst, &[9, 9]);

    assert!(!ctx.bulk_array_copy(src, 0, dst, 0, 3));
    assert_eq!(read_int_array(&ctx, dst), vec![9, 9]);

    assert!(!ctx.bulk_array_copy(src, usize::MAX, dst, 0, 1));
    assert_eq!(read_int_array(&ctx, dst), vec![9, 9]);
}

#[test]
fn default_bulk_array_copy_validates_zero_length_operands() {
    let mut ctx = MockNativeContext::new();
    let src = ctx.new_array(ArrayElementType::Int, 3);
    let dst = ctx.new_array(ArrayElementType::Int, 3);
    fill_int_array(&ctx, dst, &[7, 8, 9]);

    assert!(ctx.bulk_array_copy(src, 3, dst, 3, 0));
    assert_eq!(read_int_array(&ctx, dst), vec![7, 8, 9]);

    assert!(!ctx.bulk_array_copy(src, 4, dst, 0, 0));
    assert!(!ctx.bulk_array_copy(src, 0, dst, 4, 0));

    let non_array = ctx.alloc_object(ClassId::new(0), 0);
    assert!(!ctx.bulk_array_copy(non_array, 0, dst, 0, 0));

    let bytes = ctx.new_array(ArrayElementType::Byte, 1);
    assert!(!ctx.bulk_array_copy(bytes, 0, dst, 0, 0));

    let refs = ctx.new_ref_array(ClassId::new(0), 1);
    assert!(!ctx.bulk_array_copy(refs, 0, refs, 0, 0));
    assert_eq!(read_int_array(&ctx, dst), vec![7, 8, 9]);
}

#[test]
fn default_bulk_array_copy_rejects_type_mismatch_and_references() {
    let mut ctx = MockNativeContext::new();
    let bytes = ctx.new_array(ArrayElementType::Byte, 2);
    let ints = ctx.new_array(ArrayElementType::Int, 2);
    fill_int_array(&ctx, bytes, &[1, 2]);
    fill_int_array(&ctx, ints, &[7, 7]);

    assert!(!ctx.bulk_array_copy(bytes, 0, ints, 0, 2));
    assert_eq!(read_int_array(&ctx, ints), vec![7, 7]);

    let refs = ctx.new_ref_array(ClassId::new(0), 1);
    assert!(!ctx.bulk_array_copy(refs, 0, refs, 0, 1));
}

/// The trait default of the round-16 reference bulk copy declines and
/// writes nothing, so a context with no bulk path keeps
/// `System.arraycopy`'s own per-element loop.
#[test]
fn default_copy_reference_array_elements_declines_without_writing() {
    let mut ctx = MockNativeContext::new();
    let src = ctx.new_ref_array(ClassId::new(0), 2);
    let dst = ctx.new_ref_array(ClassId::new(0), 2);
    let a = ctx.alloc_object(ClassId::new(0), 0);
    ctx.set_array_element(src, 0, Value::Object(Some(a)));

    assert!(!ctx.copy_reference_array_elements(src, 0, dst, 0, 2));
    assert_eq!(ctx.get_array_element(dst, 0), Value::Object(None));
}
