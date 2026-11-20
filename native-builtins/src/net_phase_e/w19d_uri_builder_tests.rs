use super::*;
use crate::test_utils::{mock_ctx, MockNativeContext};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// A mock whose `java/net/URI` declares the real by-name fields, so the
/// publish's `set_field_by_name` writes land and read back.
fn ctx_with_real_uri_layout() -> MockNativeContext {
    let mut ctx = mock_ctx();
    let cid = ctx.ensure_class_initialized("java/net/URI").unwrap();
    let names = [
        "scheme",
        "fragment",
        "authority",
        "userInfo",
        "host",
        "port",
        "path",
        "query",
        "schemeSpecificPart",
        "decodedPath",
        "decodedSchemeSpecificPart",
        "string",
    ];
    let fields = names
        .iter()
        .enumerate()
        .map(|(slot, name)| cratonvm_native_api::FieldMetadata {
            name: name.to_string(),
            descriptor: if *name == "port" {
                "I"
            } else {
                "Ljava/lang/String;"
            }
            .to_string(),
            access_flags: 0,
            slot_index: slot,
            declaring_class_id: cid,
            is_static: false,
        })
        .collect();
    ctx.set_declared_fields(cid, fields);
    ctx
}

fn text(ctx: &MockNativeContext, uri: ObjectRef, name: &str) -> Option<String> {
    match ctx.get_field_by_name(uri, name) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

#[test]
fn make_uri_publishes_the_text_and_releases_its_roots() {
    let mut ctx = ctx_with_real_uri_layout();
    let uri = make_uri(&mut ctx, "http://example.test:8080/a/b?q=1#f").unwrap();
    assert_eq!(
        text(&ctx, uri, "string").as_deref(),
        Some("http://example.test:8080/a/b?q=1#f")
    );
    assert_eq!(text(&ctx, uri, "scheme").as_deref(), Some("http"));
    assert_eq!(text(&ctx, uri, "host").as_deref(), Some("example.test"));
    assert_eq!(text(&ctx, uri, "path").as_deref(), Some("/a/b"));
    assert_eq!(ctx.get_field_by_name(uri, "port"), Value::Int(8080));
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "every root must be released"
    );
}

/// `uri_define_string` is the JDK's `defineString` over a real URI whose
/// `string` cache is null -- what real `resolve`/`normalize` hand back.
/// Four shapes: server authority with an IPv6 host that needs brackets and
/// a port, registry authority (host null), opaque (path null -> the
/// scheme-specific part), and the all-null placeholder (None).
#[test]
fn uri_define_string_rebuilds_the_jdk_text_from_fields() {
    let mut ctx = ctx_with_real_uri_layout();
    let put = |ctx: &mut MockNativeContext, uri: ObjectRef, name: &str, v: &str| {
        let s = ctx.create_string_uninterned(v);
        ctx.set_field_by_name(uri, name, Value::Object(Some(s)));
    };
    let fresh = |ctx: &mut MockNativeContext| {
        let u = try_alloc_concurrent_synthetic(ctx, "java/net/URI", 18).unwrap();
        ctx.set_field_by_name(u, "port", Value::Int(-1));
        u
    };

    let a = fresh(&mut ctx);
    put(&mut ctx, a, "scheme", "http");
    put(&mut ctx, a, "userInfo", "u");
    put(&mut ctx, a, "host", "::1");
    ctx.set_field_by_name(a, "port", Value::Int(8080));
    put(&mut ctx, a, "path", "/x");
    put(&mut ctx, a, "query", "q=1");
    put(&mut ctx, a, "fragment", "f");
    assert_eq!(
        uri_define_string(&ctx, a).as_deref(),
        Some("http://u@[::1]:8080/x?q=1#f")
    );

    let b = fresh(&mut ctx);
    put(&mut ctx, b, "scheme", "x");
    put(&mut ctx, b, "authority", "a_b");
    put(&mut ctx, b, "path", "/p");
    assert_eq!(uri_define_string(&ctx, b).as_deref(), Some("x://a_b/p"));

    let c = fresh(&mut ctx);
    put(&mut ctx, c, "scheme", "mailto");
    put(&mut ctx, c, "schemeSpecificPart", "a@b.c");
    assert_eq!(uri_define_string(&ctx, c).as_deref(), Some("mailto:a@b.c"));

    let d = fresh(&mut ctx);
    assert_eq!(uri_define_string(&ctx, d), None);
    // Compatible (the mock's mode) never takes the fallback: a real-layout
    // URI with no `string` still reads as "", exactly as before.
    assert_eq!(uri_raw_string(&ctx, a), "");
}

#[test]
fn make_uri_units_rewrites_the_text_components_and_releases_its_roots() {
    let mut ctx = ctx_with_real_uri_layout();
    let raw: Vec<u16> = "http://example.test/a/b?q=1#f".encode_utf16().collect();
    let uri = make_uri_units(&mut ctx, &raw).unwrap();
    assert_eq!(
        text(&ctx, uri, "string").as_deref(),
        Some("http://example.test/a/b?q=1#f")
    );
    assert_eq!(text(&ctx, uri, "path").as_deref(), Some("/a/b"));
    assert_eq!(text(&ctx, uri, "query").as_deref(), Some("q=1"));
    assert_eq!(text(&ctx, uri, "fragment").as_deref(), Some("f"));
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "every root must be released"
    );
}
