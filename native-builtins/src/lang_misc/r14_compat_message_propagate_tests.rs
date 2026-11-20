use super::{native_throwable_get_localized_message, native_throwable_to_string};
use crate::test_utils::{mock_ctx, MockNativeContext};
use cratonvm_native_api::NativeContext;
use cratonvm_types::error::MethodCallFailed;
use cratonvm_types::{ObjectRef, Value};

fn dynctx(m: &mut MockNativeContext) -> &mut dyn NativeContext {
    m
}

fn new_obj(ctx: &mut dyn NativeContext, class: &str) -> ObjectRef {
    match ctx.new_object(class) {
        Ok(Some(Value::Object(Some(o)))) => o,
        other => panic!("new_object({class}) answered {other:?}"),
    }
}

#[test]
fn to_string_propagates_a_throwing_get_localized_message() {
    let mut m = mock_ctx();
    let exc = new_obj(dynctx(&mut m), "com/example/MyThrowable");
    let thrown = new_obj(dynctx(&mut m), "java/lang/ArithmeticException");
    m.set_invoke_virtual_result(Err(MethodCallFailed::ExceptionThrown(thrown)));
    match native_throwable_to_string(dynctx(&mut m), &[Value::Object(Some(exc))]) {
        Err(MethodCallFailed::ExceptionThrown(t)) => assert_eq!(t, thrown),
        other => panic!("expected the thrown exception, got {other:?}"),
    }
}

#[test]
fn get_localized_message_propagates_a_throwing_get_message() {
    let mut m = mock_ctx();
    let exc = new_obj(dynctx(&mut m), "com/example/MyThrowable");
    let thrown = new_obj(dynctx(&mut m), "java/lang/ArithmeticException");
    m.set_invoke_virtual_result(Err(MethodCallFailed::ExceptionThrown(thrown)));
    match native_throwable_get_localized_message(dynctx(&mut m), &[Value::Object(Some(exc))]) {
        Err(MethodCallFailed::ExceptionThrown(t)) => assert_eq!(t, thrown),
        other => panic!("expected the thrown exception, got {other:?}"),
    }
}

#[test]
fn to_string_uses_the_dispatched_text() {
    let mut m = mock_ctx();
    let exc = new_obj(dynctx(&mut m), "com/example/MyThrowable");
    let text = dynctx(&mut m).create_string("L-m");
    m.set_invoke_virtual_result(Ok(Some(Value::Object(Some(text)))));
    let ctx = dynctx(&mut m);
    match native_throwable_to_string(ctx, &[Value::Object(Some(exc))]) {
        Ok(Some(Value::Object(Some(s)))) => {
            assert_eq!(
                ctx.read_string(s).as_deref(),
                Some("com.example.MyThrowable: L-m")
            )
        }
        other => panic!("toString answered {other:?}"),
    }
}
