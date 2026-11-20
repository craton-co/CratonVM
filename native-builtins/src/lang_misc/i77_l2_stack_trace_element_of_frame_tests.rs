use super::native_stack_trace_element_of_frame;
use crate::test_utils::mock_ctx;
use cratonvm_types::error::{MethodCallFailed, RuntimeError, VmError};
use cratonvm_types::Value;

fn body(head: &str) -> String {
    let src = include_str!("../lang_misc.rs");
    src.split(head)
        .nth(1)
        .and_then(|rest| rest.split("\npub(crate) fn ").next())
        .expect("function body")
        .to_string()
}

#[test]
fn the_of_frame_intrinsic_shares_the_init_fill_and_the_format_answer() {
    let of = body("pub fn native_stack_trace_element_of_frame(");
    assert!(of.contains("crate::native_init_stack_trace_element("));
    assert!(of.contains("class_computed_format("));
    assert!(of.contains("(\"declaringClassObject\", Value::Object(None))"));
    let array = body("pub fn native_stack_trace_element_of_backtrace(");
    assert!(array.contains("finish_stack_trace_element_formats("));
    let src = include_str!("../lang_misc.rs");
    let finish = src
        .split("\nfn finish_stack_trace_element_formats(")
        .nth(1)
        .and_then(|rest| rest.split("\nfn ").next())
        .expect("finish body");
    assert!(finish.contains("class_computed_format("));
}

/// The only Java caller passes `this`; a null frame is HotSpot's
/// non-null resolution failing, answered as a `NullPointerException`.
#[test]
fn a_null_frame_is_a_null_pointer_exception() {
    let mut ctx = mock_ctx();
    let got = native_stack_trace_element_of_frame(&mut ctx, &[Value::Object(None)]);
    assert!(matches!(
        got,
        Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::NullPointerException { .. }
        )))
    ));
}
