use super::coerce_reflective_field_value;
use cratonvm_types::{jlong_bits_as_aligned_object_ptr, CompactValue, ObjectRef, Value};

/// A reference read out of a `long` / `double` slot coerces to the slot's
/// RAW bits — what `getfield J` / `getfield D` read — and never to the
/// bare heap address, which would be an unminted smuggled handle in a
/// Java `long`.
#[test]
fn a_reference_in_a_long_slot_answers_its_raw_bits_not_its_address() {
    let addr = 0x0000_5555_6666_7780usize;
    // SAFETY: never dereferenced; only its bits are used.
    let r = unsafe { ObjectRef::from_raw(addr as *mut u8) };
    let raw = CompactValue::from_value(Value::Object(Some(r))).to_bits();
    let long = coerce_reflective_field_value(Value::Object(Some(r)), "J");
    assert_eq!(long, Value::Long(raw as i64));
    assert_ne!(long, Value::Long(addr as i64));
    assert_eq!(jlong_bits_as_aligned_object_ptr(raw), None);
    match coerce_reflective_field_value(Value::Object(Some(r)), "D") {
        Value::Double(d) => assert_eq!(d.to_bits(), raw),
        other => panic!("expected a double, got {other:?}"),
    }
}
