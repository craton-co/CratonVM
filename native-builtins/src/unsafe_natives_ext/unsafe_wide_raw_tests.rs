use super::{
    native_unsafe_cas_int, native_unsafe_cas_long, native_unsafe_compare_and_exchange_long,
    native_unsafe_get_long_mb, native_unsafe_get_long_volatile_arena,
    native_unsafe_put_int_volatile_arena,
};
use cratonvm_native_api::test_mock::MockNativeContext;
use cratonvm_native_api::NativeHeapAccess;
use cratonvm_types::ArrayElementType as Aet;
use cratonvm_types::Value;

const ABASE: i64 = 16;

#[test]
fn long_volatile_and_cas_over_a_byte_array_use_the_raw_bytes() {
    let mut ctx = MockNativeContext::new();
    let bytes = ctx.new_array(Aet::Byte, 16);
    for i in 0..16usize {
        ctx.set_array_element(bytes, i, Value::Int(i as i32 + 1));
    }
    let base = Value::Object(Some(bytes));
    let want = i64::from_le_bytes([1, 2, 3, 4, 5, 6, 7, 8]);
    let args = [Value::Object(None), base, Value::Long(ABASE)];
    assert_eq!(
        native_unsafe_get_long_volatile_arena(&mut ctx, &args).expect("getLongVolatile"),
        Some(Value::Long(want))
    );
    // The plain accessor agrees (it was already raw).
    assert_eq!(
        native_unsafe_get_long_mb(&mut ctx, &args).expect("getLong"),
        Some(Value::Long(want))
    );
    // A failing CAS leaves the bytes; a matching one swaps all eight.
    let cas = |ctx: &mut MockNativeContext, ex: i64, nv: i64| {
        native_unsafe_cas_long(
            ctx,
            &[
                Value::Object(None),
                base,
                Value::Long(ABASE),
                Value::Long(ex),
                Value::Long(nv),
            ],
        )
        .expect("compareAndSetLong")
    };
    assert_eq!(cas(&mut ctx, 0, -1), Some(Value::Int(0)));
    assert_eq!(cas(&mut ctx, want, -1), Some(Value::Int(1)));
    for i in 0..8usize {
        assert_eq!(ctx.get_array_element(bytes, i), Value::Int(-1), "byte {i}");
    }
    assert_eq!(
        ctx.get_array_element(bytes, 8),
        Value::Int(9),
        "byte 8 untouched"
    );
    // compareAndExchange answers the witnessed raw value.
    let cae = native_unsafe_compare_and_exchange_long(
        &mut ctx,
        &[
            Value::Object(None),
            base,
            Value::Long(ABASE),
            Value::Long(0),
            Value::Long(5),
        ],
    )
    .expect("compareAndExchangeLong");
    assert_eq!(cae, Some(Value::Long(-1)));
}

#[test]
fn int_cas_over_a_float_array_compares_the_bits() {
    let mut ctx = MockNativeContext::new();
    let floats = ctx.new_array(Aet::Float, 2);
    ctx.set_array_element(floats, 1, Value::Float(1.5));
    let base = Value::Object(Some(floats));
    // `compareAndSetFloat(a, base + 4, 1.5f, 2.5f)` is this call.
    let r = native_unsafe_cas_int(
        &mut ctx,
        &[
            Value::Object(None),
            base,
            Value::Long(ABASE + 4),
            Value::Int(1.5f32.to_bits() as i32),
            Value::Int(2.5f32.to_bits() as i32),
        ],
    )
    .expect("compareAndSetInt");
    assert_eq!(r, Some(Value::Int(1)));
    assert_eq!(ctx.get_array_element(floats, 1), Value::Float(2.5));
}

#[test]
fn int_volatile_store_into_a_long_array_writes_one_lane() {
    let mut ctx = MockNativeContext::new();
    let longs = ctx.new_array(Aet::Long, 1);
    let base = Value::Object(Some(longs));
    native_unsafe_put_int_volatile_arena(
        &mut ctx,
        &[
            Value::Object(None),
            base,
            Value::Long(ABASE + 4),
            Value::Int(0x1234_5678),
        ],
    )
    .expect("putIntVolatile");
    assert_eq!(
        ctx.get_array_element(longs, 0),
        Value::Long(0x1234_5678_0000_0000)
    );
}
