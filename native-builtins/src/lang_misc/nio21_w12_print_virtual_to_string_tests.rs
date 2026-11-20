use super::collect_throwable_chain_lines_checked;
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
fn a_throwing_to_string_prints_nothing_and_propagates() {
    let mut m = mock_ctx();
    let exc = new_obj(dynctx(&mut m), "com/example/BadToString");
    let thrown = new_obj(dynctx(&mut m), "java/lang/IllegalStateException");
    m.set_invoke_virtual_result(Err(MethodCallFailed::ExceptionThrown(thrown)));
    let (lines, error) = collect_throwable_chain_lines_checked(dynctx(&mut m), exc);
    assert!(lines.is_empty(), "{lines:?}");
    match error {
        Some(MethodCallFailed::ExceptionThrown(t)) => assert_eq!(t, thrown),
        other => panic!("expected the thrown exception, got {other:?}"),
    }
}

#[test]
fn an_overriding_to_string_is_the_header() {
    let mut m = mock_ctx();
    let exc = new_obj(dynctx(&mut m), "com/example/Custom");
    let text = dynctx(&mut m).create_string("custom header");
    m.set_invoke_virtual_result(Ok(Some(Value::Object(Some(text)))));
    let (lines, error) = collect_throwable_chain_lines_checked(dynctx(&mut m), exc);
    assert!(error.is_none());
    assert_eq!(lines.first().map(String::as_str), Some("custom header"));
}
