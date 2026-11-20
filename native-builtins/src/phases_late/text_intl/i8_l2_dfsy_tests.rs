use super::p61_dfsy_fill;
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};
use cratonvm_types::{ClassId, Value};

/// Every `DateFormatSymbols` table is a typed `String[]` holding the
/// expected names (i8-L2).
#[test]
fn i8_l2_date_format_symbols_tables_are_string_typed() {
    let mut ctx = mock_ctx();
    let string_id = ctx
        .ensure_class_initialized("java/lang/String")
        .expect("mock registers the class");
    let this = ctx.alloc_object(ClassId::new(0), 5);
    p61_dfsy_fill(&mut ctx, this);
    let expected_lengths = [8usize, 8, 13, 13, 2];
    for (slot, want) in expected_lengths.iter().enumerate() {
        let arr = match ctx.get_field(this, slot) {
            Value::Object(Some(a)) => a,
            other => panic!("slot {slot}: expected an array, got {other:?}"),
        };
        assert_eq!(ctx.ref_array_component(arr), Some(string_id), "slot {slot}");
        assert_eq!(ctx.array_length(arr), *want, "slot {slot}");
    }
    let weekdays = match ctx.get_field(this, 0) {
        Value::Object(Some(a)) => a,
        other => panic!("expected weekdays, got {other:?}"),
    };
    let monday = match ctx.get_array_element(weekdays, 2) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    };
    assert_eq!(monday.as_deref(), Some("Monday"));
}
