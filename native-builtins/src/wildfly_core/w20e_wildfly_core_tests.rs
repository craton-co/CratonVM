//! gc-common w20-e: the `Level` / `Logger` / executor mirrors are built
//! through `wf_store_fresh_string`, which roots the mirror across its
//! name's allocation. The mock never moves an object at an allocation, so
//! these check shape and root balance.
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

fn string_slot(ctx: &MockNativeContext, obj: ObjectRef, slot: usize) -> Option<String> {
    match ctx.get_field(obj, slot) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

#[test]
fn w20e_level_and_logger_mirrors_carry_their_names_and_leave_no_pin() {
    let mut ctx = MockNativeContext::new();
    let base = ctx.native_pin_count_for_test();

    let Ok(Some(Value::Object(Some(level)))) = native_level_get(&mut ctx, JulLevel::Warning)
    else {
        panic!("Level mirror not built");
    };
    assert_eq!(
        string_slot(&ctx, level, LVL_FIELD_NAME).as_deref(),
        Some(JulLevel::Warning.name())
    );
    assert_eq!(
        ctx.get_field(level, LVL_FIELD_VALUE),
        Value::Int(JulLevel::Warning.int_value())
    );

    let name = ctx.create_string("w20e.logger");
    let Ok(Some(Value::Object(Some(logger)))) =
        native_log_manager_get_logger(&mut ctx, &[Value::Object(Some(name))])
    else {
        panic!("Logger mirror not built");
    };
    assert_eq!(
        string_slot(&ctx, logger, LOG_FIELD_NAME).as_deref(),
        Some("w20e.logger")
    );
    assert_eq!(
        ctx.native_pin_count_for_test(),
        base,
        "every root is released"
    );
}
