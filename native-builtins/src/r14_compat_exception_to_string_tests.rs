use super::exception_to_string_localized;
use crate::test_utils::{mock_ctx, MockNativeContext};
use cratonvm_native_api::NativeContext;
use cratonvm_types::error::MethodCallFailed;
use cratonvm_types::{ObjectRef, Value};

// Every trait method is called through `dyn NativeContext`, as the
// natives themselves do, so no supertrait import is needed.
fn dynctx(m: &mut MockNativeContext) -> &mut dyn NativeContext {
    m
}

fn new_obj(ctx: &mut dyn NativeContext, class: &str) -> ObjectRef {
    match ctx.new_object(class) {
        Ok(Some(Value::Object(Some(o)))) => o,
        other => panic!("new_object({class}) answered {other:?}"),
    }
}

fn to_string_text(m: &mut MockNativeContext, exc: ObjectRef) -> Option<String> {
    let ctx = dynctx(m);
    match exception_to_string_localized(ctx, exc) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
        other => panic!("toString answered {other:?}"),
    }
}

/// `class MyEx extends RuntimeException { getLocalizedMessage() { "L-" + ... } }`:
/// the inherited `toString` must print the overridden text, not `detailMessage`.
#[test]
fn inherited_to_string_prints_the_overridden_localized_message() {
    let mut m = mock_ctx();
    let exc = new_obj(dynctx(&mut m), "com/example/MyEx");
    let localized = dynctx(&mut m).create_string("L-m");
    m.set_invoke_virtual_result(Ok(Some(Value::Object(Some(localized)))));
    assert_eq!(
        to_string_text(&mut m, exc).as_deref(),
        Some("com.example.MyEx: L-m")
    );
}

#[test]
fn a_null_localized_message_prints_the_bare_class_name() {
    let mut m = mock_ctx();
    let exc = new_obj(dynctx(&mut m), "com/example/MyEx");
    m.set_invoke_virtual_result(Ok(Some(Value::Object(None))));
    assert_eq!(
        to_string_text(&mut m, exc).as_deref(),
        Some("com.example.MyEx")
    );
}

/// HotSpot propagates an exception thrown by an overriding
/// `getLocalizedMessage` out of `toString`; it must not be swallowed.
#[test]
fn an_exception_from_get_localized_message_propagates() {
    let mut m = mock_ctx();
    let exc = new_obj(dynctx(&mut m), "com/example/MyEx");
    let thrown = new_obj(dynctx(&mut m), "java/lang/IllegalStateException");
    m.set_invoke_virtual_result(Err(MethodCallFailed::ExceptionThrown(thrown)));
    match exception_to_string_localized(dynctx(&mut m), exc) {
        Err(MethodCallFailed::ExceptionThrown(t)) => assert_eq!(t, thrown),
        other => panic!("expected the thrown exception, got {other:?}"),
    }
}
