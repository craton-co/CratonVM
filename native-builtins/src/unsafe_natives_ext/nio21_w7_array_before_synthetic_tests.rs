use super::{
    native_unsafe_cas_int, native_unsafe_get_int_volatile,
    unsafe_array_before_synthetic_enabled, unsafe_synthetic_field_offset,
};
use cratonvm_native_api::test_mock::MockNativeContext;
use cratonvm_native_api::NativeHeapAccess;
use cratonvm_types::{ArrayElementType as Aet, ClassId, Value};

#[test]
fn nio21_w7_an_array_base_never_takes_the_synthetic_band() {
    if !unsafe_array_before_synthetic_enabled() {
        return;
    }
    let mut ctx = MockNativeContext::new();
    let ints = ctx.new_array(Aet::Int, 4);
    let plain = ctx.alloc_object(ClassId::new(0), 2);
    let off = crate::SYNTHETIC_OFFSET_BASE + 0x40;
    assert!(
        !unsafe_synthetic_field_offset(&ctx, ints, off),
        "an array base is never synthetic"
    );
    assert!(
        unsafe_synthetic_field_offset(&ctx, plain, off),
        "a plain base keeps the band"
    );
    assert!(
        !unsafe_synthetic_field_offset(&ctx, plain, 3),
        "below the band"
    );
    let base = Value::Object(Some(ints));
    let cas = native_unsafe_cas_int(
        &mut ctx,
        &[
            Value::Object(None),
            base,
            Value::Long(off as i64),
            Value::Int(0),
            Value::Int(7),
        ],
    )
    .expect("compareAndSetInt");
    assert_eq!(
        cas,
        Some(Value::Int(0)),
        "out of range on the array arm: the CAS fails"
    );
    for i in 0..4 {
        assert_eq!(
            ctx.get_array_element(ints, i),
            Value::Int(0),
            "element {i} untouched"
        );
    }
    assert!(
        !matches!(crate::synthetic_get(&mut ctx, ints, off), Value::Int(7)),
        "nothing was written to a side store keyed by the array"
    );
    let get = native_unsafe_get_int_volatile(
        &mut ctx,
        &[Value::Object(None), base, Value::Long(off as i64)],
    )
    .expect("getIntVolatile");
    assert_eq!(get, Some(Value::Int(0)));    }