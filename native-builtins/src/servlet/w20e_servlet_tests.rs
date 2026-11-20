//! gc-common w20-e: the synthetic `HttpResponse` builds its body `String`
//! first and roots it across the response's allocation. The mock never
//! moves an object at an allocation, so this checks shape and root balance.
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn w20e_stub_response_carries_status_and_body_and_leaves_no_pin() {
    let mut ctx = MockNativeContext::new();
    let base = ctx.native_pin_count_for_test();
    let Ok(Some(Value::Object(Some(resp)))) = s3_stub_response(&mut ctx, 503, "w20e body")
    else {
        panic!("HttpResponse not built");
    };
    assert_eq!(ctx.get_field(resp, 0), Value::Int(503));
    let Value::Object(Some(body)) = ctx.get_field(resp, 1) else {
        panic!("no body String");
    };
    assert_eq!(ctx.read_string(body).as_deref(), Some("w20e body"));
    assert_eq!(ctx.get_field(resp, 2), Value::Object(None));
    assert_eq!(
        ctx.native_pin_count_for_test(),
        base,
        "every root is released"
    );
}
