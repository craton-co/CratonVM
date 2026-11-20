use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};

fn logger(ctx: &mut MockNativeContext) -> ObjectRef {
    let cid = ctx.ensure_class_initialized(CLS_JUL_LOGGER).unwrap();
    let field =
        |name: &str, descriptor: &str, slot_index: usize| cratonvm_native_api::FieldMetadata {
            name: name.to_string(),
            descriptor: descriptor.to_string(),
            access_flags: 0,
            slot_index,
            declaring_class_id: cid,
            is_static: false,
        };
    ctx.set_declared_fields(
        cid,
        vec![
            field("manager", "Ljava/util/logging/LogManager;", 1),
            field("isSystemLogger", "Z", 11),
        ],
    );
    let l = ctx.alloc_object(cid, LOGGER_NUM_FIELDS);
    // A fresh mock object does not read its reference slots back as a
    // null reference; the VM's would.
    ctx.set_field(l, 1, Value::Object(None));
    l
}

#[test]
fn a_logger_without_manager_or_system_flag_is_minted() {
    let mut ctx = MockNativeContext::new();
    let l = logger(&mut ctx);
    ctx.set_field(l, 11, Value::Int(0));
    assert!(jul_logger_is_minted(&ctx, l));
}

#[test]
fn a_constructed_logger_is_not_minted() {
    let mut ctx = MockNativeContext::new();
    let l = logger(&mut ctx);
    let manager = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
    ctx.set_field(l, 1, Value::Object(Some(manager)));
    ctx.set_field(l, 11, Value::Int(0));
    assert!(!jul_logger_is_minted(&ctx, l));
}

/// `getName()` of a constructed logger without a name (an anonymous one)
/// is `null`, as on HotSpot; a minted one keeps the `""` fallback.
#[test]
fn a_nameless_constructed_logger_answers_a_null_name() {
    let mut ctx = MockNativeContext::new();
    let l = logger(&mut ctx);
    let manager = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
    ctx.set_field(l, 1, Value::Object(Some(manager)));
    ctx.set_field(l, 11, Value::Int(0));
    assert_eq!(jul_logger_get_name_value(&mut ctx, l), Value::Object(None));

    let m = logger(&mut ctx);
    ctx.set_field(m, 11, Value::Int(0));
    match jul_logger_get_name_value(&mut ctx, m) {
        Value::Object(Some(s)) => assert_eq!(ctx.read_string(s).as_deref(), Some("")),
        other => panic!("minted logger name: {other:?}"),
    }
}

/// `Logger.global`: built by the private `Logger(String)` constructor,
/// `manager` still null, `isSystemLogger = true`.
#[test]
fn the_global_logger_is_not_minted() {
    let mut ctx = MockNativeContext::new();
    let l = logger(&mut ctx);
    ctx.set_field(l, 11, Value::Int(1));
    assert!(!jul_logger_is_minted(&ctx, l));
}
