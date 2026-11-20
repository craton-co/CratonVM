use super::pop_ref_operand_is_null;
use crate::runtime::ValueStack;
use crate::types::Value;

#[test]
fn each_null_shape_pops_one_slot_and_answers_as_the_decode() {
    let cases = [
        (Value::Object(None), true),
        // The JNI null handle carried as a long, and a never-written
        // reference slot (see `ref_operand_is_null`).
        (Value::Long(0), true),
        (Value::Int(0), true),
        (Value::Long(0x7000), false),
        (Value::Int(5), false),
    ];
    for (value, null) in cases {
        let mut stack = ValueStack::new(4);
        stack.push_checked(Value::Int(77)).expect("room");
        stack.push_checked(value).expect("room");
        assert_eq!(pop_ref_operand_is_null(&mut stack), null, "{value:?}");
        assert_eq!(stack.len(), 1, "{value:?}");
        assert_eq!(stack.pop_int_unchecked(), 77);
    }
}
