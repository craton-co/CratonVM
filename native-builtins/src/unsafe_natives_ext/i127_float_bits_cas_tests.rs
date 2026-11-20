use super::*;

#[test]
fn float_and_double_slots_compare_by_raw_bits() {
    let f = 4.0f32;
    let d = 7.0f64;
    assert!(unsafe_cas_values_equal(
        Value::Float(f),
        Value::Int(f.to_bits() as i32)
    ));
    assert!(unsafe_cas_values_equal(
        Value::Int(f.to_bits() as i32),
        Value::Float(f)
    ));
    assert!(!unsafe_cas_values_equal(
        Value::Float(-0.0),
        Value::Int(0.0f32.to_bits() as i32)
    ));
    assert!(unsafe_cas_values_equal(
        Value::Double(d),
        Value::Long(d.to_bits() as i64)
    ));
    assert!(!unsafe_cas_values_equal(
        Value::Double(d),
        Value::Long(3.5f64.to_bits() as i64)
    ));
    // A NaN payload matches its own bits.
    let nan = f32::from_bits(0x7FC0_1234);
    assert!(unsafe_cas_values_equal(
        Value::Float(nan),
        Value::Int(0x7FC0_1234)
    ));
}

#[test]
fn witness_bits_and_static_retag() {
    assert_eq!(
        unsafe_int_bits_of(&Value::Float(2.5)),
        Some(2.5f32.to_bits() as i32)
    );
    assert_eq!(
        unsafe_long_bits_of(&Value::Double(3.5)),
        Some(3.5f64.to_bits() as i64)
    );
    assert_eq!(unsafe_int_bits_of(&Value::Object(None)), None);
    match unsafe_retag_bits_like(&Value::Float(1.0), Value::Int(2.5f32.to_bits() as i32)) {
        Value::Float(v) => assert_eq!(v.to_bits(), 2.5f32.to_bits()),
        other => panic!("float static lost its tag: {other:?}"),
    }
    match unsafe_retag_bits_like(&Value::Double(1.0), Value::Long(7.0f64.to_bits() as i64)) {
        Value::Double(v) => assert_eq!(v.to_bits(), 7.0f64.to_bits()),
        other => panic!("double static lost its tag: {other:?}"),
    }
    assert!(matches!(
        unsafe_retag_bits_like(&Value::Int(1), Value::Int(9)),
        Value::Int(9)
    ));
}
