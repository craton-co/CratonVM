//! gc-common w19-d: the phase-60 `HttpClient` stubs make their payload
//! (method String, body String, URI / String argument) first and root it
//! across the carrier's allocation. The mock never moves an object at an
//! allocation, so these check the assembled shape and the root balance.
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn w19d_obj(v: MethodCallResult) -> ObjectRef {
    match v.expect("the stub must not fail") {
        Some(Value::Object(Some(o))) => o,
        other => panic!("expected an object, got {other:?}"),
    }
}

fn w19d_text(ctx: &MockNativeContext, v: Value) -> Option<String> {
    match v {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

#[test]
fn w19d_request_builders_carry_their_uri_and_get() {
    let mut ctx = MockNativeContext::new();
    let uri = ctx.fresh_object_ref();
    let with_uri = w19d_obj(p60_http_request_builder_uri(
        &mut ctx,
        &[Value::Object(Some(uri))],
    ));
    assert_eq!(ctx.get_field(with_uri, 0), Value::Object(Some(uri)));
    assert_eq!(
        w19d_text(&ctx, ctx.get_field(with_uri, 1)).as_deref(),
        Some("GET")
    );
    assert_eq!(ctx.get_field(with_uri, 2), Value::Object(None));

    let bare = w19d_obj(p60_http_request_builder(&mut ctx, &[]));
    assert_eq!(ctx.get_field(bare, 0), Value::Object(None));
    assert_eq!(
        w19d_text(&ctx, ctx.get_field(bare, 1)).as_deref(),
        Some("GET")
    );
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "every root must be released"
    );
}

#[test]
fn w19d_the_stub_send_answers_200_with_an_empty_body() {
    let mut ctx = MockNativeContext::new();
    let response = w19d_obj(p60_http_send(&mut ctx, &[]));
    assert_eq!(ctx.get_field(response, 0), Value::Int(200));
    assert_eq!(
        w19d_text(&ctx, ctx.get_field(response, 1)).as_deref(),
        Some("")
    );
    assert_eq!(ctx.get_field(response, 2), Value::Object(None));
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "every root must be released"
    );
}

#[test]
fn w19d_body_publisher_of_string_holds_its_argument() {
    let mut r = NativeMethodRegistry::new();
    register_p60_http_client(&mut r);
    let of_string = r
        .find(
            "java/net/http/HttpRequest$BodyPublishers",
            "ofString",
            "(Ljava/lang/String;)Ljava/net/http/HttpRequest$BodyPublisher;",
        )
        .expect("BodyPublishers.ofString must be registered");
    let mut ctx = MockNativeContext::new();
    let text = ctx.create_string("payload");
    let publisher = w19d_obj(of_string(&mut ctx, &[Value::Object(Some(text))]));
    assert_eq!(ctx.get_field(publisher, 0), Value::Object(Some(text)));
    let empty = w19d_obj(of_string(&mut ctx, &[Value::Object(None)]));
    assert_eq!(ctx.get_field(empty, 0), Value::Object(None));
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "every root must be released"
    );
}
