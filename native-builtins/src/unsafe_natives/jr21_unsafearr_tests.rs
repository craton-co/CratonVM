//! JIT round 21 lane unsafearr: `R21UnsafeNarrowRows`'s `cae long-array`
//! row, access `I` at `k = 0` and `k = 4`. An int `compareAndExchange` on a
//! half of a `long[]` element answers the half's old bits and stores the
//! update into that half only (HotSpot 25 `-Xint`). Before the fix the
//! registered native compared the `Int` operand against the `Long` element,
//! never stored, and answered `expected` both times.
use super::{native_unsafe_cae_int, native_unsafe_cae_long};
use cratonvm_native_api::test_mock::MockNativeContext;
use cratonvm_native_api::NativeHeapAccess;
use cratonvm_types::ArrayElementType as Aet;
use cratonvm_types::Value;

const ABASE: i64 = 16;
/// `o.la[1]` for the probe's seed (its `-1316163872005681916` column).
const CELL: i64 = -1316163872005681916;

fn cae_int(ctx: &mut MockNativeContext, base: Value, off: i64, e: i32, x: i32) -> Value {
    let args = [
        Value::Object(None),
        base,
        Value::Long(off),
        Value::Int(e),
        Value::Int(x),
    ];
    native_unsafe_cae_int(ctx, &args)
        .expect("compareAndExchangeInt")
        .expect("a witness")
}

/// One `k` of the row: hit witness, miss witness, the element after.
fn row(k: i64, cur: i32, x: i32) -> (Value, Value, Value) {
    let mut ctx = MockNativeContext::new();
    let longs = ctx.new_array(Aet::Long, 2);
    ctx.set_array_element(longs, 1, Value::Long(CELL));
    let base = Value::Object(Some(longs));
    let off = ABASE + 8 + k;
    let hit = cae_int(&mut ctx, base, off, cur, x);
    let miss = cae_int(&mut ctx, base, off, cur, !x);
    (hit, miss, ctx.get_array_element(longs, 1))
}

#[test]
fn int_cae_on_the_low_half_of_a_long_element() {
    let (hit, miss, after) = row(0, -2031743740, -1340500334);
    assert_eq!(hit, Value::Int(-2031743740));
    assert_eq!(miss, Value::Int(-1340500334));
    assert_eq!(after, Value::Long(-1316163871314438510));
}

#[test]
fn int_cae_on_the_high_half_of_a_long_element() {
    let (hit, miss, after) = row(4, -306443282, 173706294);
    assert_eq!(hit, Value::Int(-306443282));
    assert_eq!(miss, Value::Int(173706294));
    assert_eq!(after, Value::Long(746062854102584580));
}

#[test]
fn long_cae_on_a_long_element_keeps_the_typed_path() {
    let mut ctx = MockNativeContext::new();
    let longs = ctx.new_array(Aet::Long, 2);
    ctx.set_array_element(longs, 1, Value::Long(CELL));
    let base = Value::Object(Some(longs));
    let x = -7685439252564745015i64;
    let call = |ctx: &mut MockNativeContext, e: i64, n: i64| {
        native_unsafe_cae_long(
            ctx,
            &[
                Value::Object(None),
                base,
                Value::Long(ABASE + 8),
                Value::Long(e),
                Value::Long(n),
            ],
        )
        .expect("compareAndExchangeLong")
    };
    assert_eq!(call(&mut ctx, CELL, x), Some(Value::Long(CELL)));
    assert_eq!(call(&mut ctx, CELL, !x), Some(Value::Long(x)));
    assert_eq!(ctx.get_array_element(longs, 1), Value::Long(x));
}

#[test]
fn int_cae_on_a_float_element_compares_the_bits() {
    let mut ctx = MockNativeContext::new();
    let floats = ctx.new_array(Aet::Float, 2);
    ctx.set_array_element(floats, 1, Value::Float(1.5));
    let base = Value::Object(Some(floats));
    let one_five = 1.5f32.to_bits() as i32;
    let w = cae_int(&mut ctx, base, ABASE + 4, one_five, 2.5f32.to_bits() as i32);
    assert_eq!(w, Value::Int(one_five));
    assert_eq!(ctx.get_array_element(floats, 1), Value::Float(2.5));
}
