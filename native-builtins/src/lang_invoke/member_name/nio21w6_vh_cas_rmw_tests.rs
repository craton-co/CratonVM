use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};

/// The RMW answers the replaced value and leaves `update(old)` behind.
#[test]
fn r21w6_vh_cas_rmw_answers_old_and_stores_the_update() {
    let mut ctx = MockNativeContext::new();
    let obj = ctx.alloc_object(ClassId::new(0), 2);
    ctx.set_field(obj, 1, Value::Int(5));
    let old = vh_cas_rmw(&mut ctx, obj, 1, VhRmwSlot::Field, |v| match v {
        Value::Int(x) => Value::Int(x + 3),
        other => *other,
    });
    assert_eq!(old, Value::Int(5));
    assert_eq!(ctx.get_field(obj, 1), Value::Int(8));
    let old = vh_cas_rmw(&mut ctx, obj, 1, VhRmwSlot::Field, |_| Value::Int(-1));
    assert_eq!(old, Value::Int(8));
    assert_eq!(ctx.get_field(obj, 1), Value::Int(-1));
}

/// nio21 wave 7 L1: a CAS that can never match what the read decodes
/// (the mock compares with `PartialEq`, so a `NaN` slot never equals the
/// `NaN` it read) ends through the stuck guard instead of spinning.
#[test]
fn r21w7_vh_cas_rmw_stuck_guard_ends_a_cas_that_cannot_land() {
    let mut ctx = MockNativeContext::new();
    let obj = ctx.alloc_object(ClassId::new(0), 2);
    ctx.set_field(obj, 1, Value::Float(f32::NAN));
    let old = vh_cas_rmw(&mut ctx, obj, 1, VhRmwSlot::Field, |_| Value::Float(2.5));
    assert!(matches!(old, Value::Float(f) if f.is_nan()));
    assert_eq!(ctx.get_field(obj, 1), Value::Float(2.5));
}

#[test]
fn r21w7_vh_rmw_same_bits_is_bitwise_for_floats() {
    assert!(vh_rmw_same_bits(
        &Value::Float(f32::NAN),
        &Value::Float(f32::NAN)
    ));
    assert!(vh_rmw_same_bits(
        &Value::Double(f64::NAN),
        &Value::Double(f64::NAN)
    ));
    assert!(!vh_rmw_same_bits(&Value::Float(0.0), &Value::Float(-0.0)));
    assert!(vh_rmw_same_bits(&Value::Int(3), &Value::Int(3)));
    assert!(!vh_rmw_same_bits(&Value::Int(0), &Value::Object(None)));
}
