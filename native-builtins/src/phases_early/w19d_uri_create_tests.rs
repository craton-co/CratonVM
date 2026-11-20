use super::*;
use crate::test_utils::{mock_ctx, MockNativeContext};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn text_at(ctx: &MockNativeContext, obj: ObjectRef, slot: usize) -> Option<String> {
    match ctx.get_field(obj, slot) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

#[test]
fn uri_create_fills_the_synthetic_slots_and_releases_its_roots() {
    let mut ctx = mock_ctx();
    let mut registry = NativeMethodRegistry::new();
    register_phase54_net_extras(&mut registry);
    let create = registry
        .find(
            "java/net/URI",
            "create",
            "(Ljava/lang/String;)Ljava/net/URI;",
        )
        .expect("URI.create(String) native should be registered");
    let raw = "http://example.test:8080/a/b";
    let text = ctx.create_string(raw);
    let uri = match create(&mut ctx, &[Value::Object(Some(text))]) {
        Ok(Some(Value::Object(Some(o)))) => o,
        other => panic!("URI.create answered {other:?}"),
    };
    assert_eq!(text_at(&ctx, uri, 0).as_deref(), Some("http"));
    assert_eq!(text_at(&ctx, uri, 1).as_deref(), Some("example.test"));
    assert_eq!(ctx.get_field(uri, 2), Value::Int(8080));
    assert_eq!(text_at(&ctx, uri, 3).as_deref(), Some("/a/b"));
    assert_eq!(ctx.get_field(uri, 4), Value::Object(None));
    assert_eq!(ctx.get_field(uri, 5), Value::Object(None));
    assert_eq!(text_at(&ctx, uri, 6).as_deref(), Some(raw));
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "every root must be released"
    );
}
