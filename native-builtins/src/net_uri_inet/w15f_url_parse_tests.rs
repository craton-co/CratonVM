//! gc-common w15-f: `url_parse` / `uri_store_named` now root the receiver
//! and store each String as it is allocated. The mock never moves an
//! object, so these pin the WRITES (same fields, same values, one String
//! shared where it used to be shared) and the root balance, not a move.
use super::*;
use crate::test_utils::MockNativeContext;
use cratonvm_native_api::FieldMetadata;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn text(ctx: &mut MockNativeContext, obj: ObjectRef, slot: usize) -> Option<String> {
    match ctx.get_field(obj, slot) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

/// A receiver whose class declares `names` at slots `0..names.len()`.
fn real_layout(ctx: &mut MockNativeContext, class_name: &str, names: &[&str]) -> ObjectRef {
    let cid = ctx
        .ensure_class_initialized(class_name)
        .expect("mock class");
    let fields = names
        .iter()
        .enumerate()
        .map(|(slot, name)| FieldMetadata {
            name: name.to_string(),
            descriptor: "Ljava/lang/Object;".to_string(),
            access_flags: 0,
            slot_index: slot,
            declaring_class_id: cid,
            is_static: false,
        })
        .collect();
    ctx.set_declared_fields(cid, fields);
    ctx.alloc_object(cid, names.len())
}

#[test]
fn w15f_url_parse_fills_the_synthetic_slots_and_returns_the_receiver() {
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(0xF15_0101);
    let obj = ctx.alloc_object(ClassId::new(0), 6);
    let back = url_parse(&mut ctx, obj, "http://h:8080/p?q=1#frag");
    assert_eq!(back, obj);
    assert_eq!(
        text(&mut ctx, obj, URL_FIELD_PROTOCOL).as_deref(),
        Some("http")
    );
    assert_eq!(text(&mut ctx, obj, URL_FIELD_HOST).as_deref(), Some("h"));
    assert_eq!(ctx.get_field(obj, URL_FIELD_PORT), Value::Int(8080));
    assert_eq!(text(&mut ctx, obj, URL_FIELD_PATH).as_deref(), Some("/p"));
    assert_eq!(text(&mut ctx, obj, URL_FIELD_QUERY).as_deref(), Some("q=1"));
    assert_eq!(
        text(&mut ctx, obj, URL_FIELD_FULL).as_deref(),
        Some("http://h:8080/p?q=1#frag")
    );
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "the receiver's root is released"
    );
}

#[test]
fn w15f_url_parse_file_arm_on_the_synthetic_layout() {
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(0xF15_0102);
    let obj = ctx.alloc_object(ClassId::new(0), 6);
    url_parse(&mut ctx, obj, "file:/C:/x.jar?y#z");
    assert_eq!(
        text(&mut ctx, obj, URL_FIELD_PROTOCOL).as_deref(),
        Some("file")
    );
    assert_eq!(text(&mut ctx, obj, URL_FIELD_HOST).as_deref(), Some(""));
    assert_eq!(ctx.get_field(obj, URL_FIELD_PORT), Value::Int(-1));
    assert_eq!(
        text(&mut ctx, obj, URL_FIELD_PATH).as_deref(),
        Some("/C:/x.jar")
    );
    assert_eq!(text(&mut ctx, obj, URL_FIELD_QUERY).as_deref(), Some("y"));
    assert_eq!(ctx.native_pin_count_for_test(), 0);
}

#[test]
fn w15f_url_parse_real_layout_writes_by_name_and_shares_the_scheme_string() {
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(0xF15_0103);
    let names = [
        "protocol",
        "host",
        "port",
        "file",
        "path",
        "query",
        "ref",
        "authority",
        "scheme",
    ];
    let obj = real_layout(&mut ctx, "java/net/URL", &names);
    url_parse(&mut ctx, obj, "http://h:81/a/b?k=v#r");
    assert_eq!(text(&mut ctx, obj, 0).as_deref(), Some("http"));
    assert_eq!(text(&mut ctx, obj, 1).as_deref(), Some("h"));
    assert_eq!(ctx.get_field(obj, 2), Value::Int(81));
    assert_eq!(text(&mut ctx, obj, 3).as_deref(), Some("/a/b?k=v"));
    assert_eq!(text(&mut ctx, obj, 4).as_deref(), Some("/a/b"));
    assert_eq!(text(&mut ctx, obj, 5).as_deref(), Some("k=v"));
    assert_eq!(text(&mut ctx, obj, 6).as_deref(), Some("r"));
    assert_eq!(
        ctx.get_field(obj, 0),
        ctx.get_field(obj, 8),
        "`protocol` and `scheme` hold ONE String, as before"
    );
    assert_eq!(ctx.native_pin_count_for_test(), 0);
}

#[test]
fn w15f_url_parse_jar_arm_shares_file_and_path() {
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(0xF15_0104);
    let names = [
        "protocol",
        "host",
        "port",
        "file",
        "path",
        "query",
        "authority",
    ];
    let obj = real_layout(&mut ctx, "java/net/URL", &names);
    ctx.set_field(obj, 5, Value::Int(7));
    ctx.set_field(obj, 6, Value::Int(7));
    url_parse(&mut ctx, obj, "jar:file:/x.jar!/a");
    assert_eq!(text(&mut ctx, obj, 0).as_deref(), Some("jar"));
    assert_eq!(text(&mut ctx, obj, 1).as_deref(), Some(""));
    assert_eq!(ctx.get_field(obj, 2), Value::Int(-1));
    assert_eq!(text(&mut ctx, obj, 3).as_deref(), Some("file:/x.jar!/a"));
    assert_eq!(
        ctx.get_field(obj, 3),
        ctx.get_field(obj, 4),
        "one String for both"
    );
    assert_eq!(ctx.get_field(obj, 5), Value::Object(None), "query nulled");
    assert_eq!(
        ctx.get_field(obj, 6),
        Value::Object(None),
        "authority nulled"
    );
    assert_eq!(ctx.native_pin_count_for_test(), 0);
}

#[test]
fn w15f_uri_store_named_writes_every_component_and_returns_the_receiver() {
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(0xF15_0105);
    let names = [
        "string",
        "scheme",
        "schemeSpecificPart",
        "path",
        "authority",
        "query",
        "fragment",
        "port",
    ];
    let obj = real_layout(&mut ctx, "java/net/URI", &names);
    let back = uri_store_named(&mut ctx, obj, "http://u@h:1/p?q#f");
    assert_eq!(back, obj);
    assert_eq!(
        text(&mut ctx, obj, 0).as_deref(),
        Some("http://u@h:1/p?q#f")
    );
    assert_eq!(text(&mut ctx, obj, 1).as_deref(), Some("http"));
    assert_eq!(text(&mut ctx, obj, 2).as_deref(), Some("//u@h:1/p?q"));
    assert_eq!(text(&mut ctx, obj, 3).as_deref(), Some("/p"));
    assert_eq!(text(&mut ctx, obj, 4).as_deref(), Some("u@h:1"));
    assert_eq!(text(&mut ctx, obj, 5).as_deref(), Some("q"));
    assert_eq!(text(&mut ctx, obj, 6).as_deref(), Some("f"));
    assert_eq!(ctx.native_pin_count_for_test(), 0);
}

#[test]
fn w15f_get_all_by_name_builds_its_array_from_rooted_addresses() {
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(0xF15_0106);
    let host = ctx.create_string("127.0.0.1");
    let arr = match native_inet_get_all_by_name(&mut ctx, &[Value::Object(Some(host))]) {
        Ok(Some(Value::Object(Some(arr)))) => arr,
        other => panic!("expected an array, got {other:?}"),
    };
    let len = ctx.array_length(arr);
    assert!(len >= 1);
    for i in 0..len {
        let Value::Object(Some(ia)) = ctx.get_array_element(arr, i) else {
            panic!("element {i} is not an address");
        };
        assert_eq!(text(&mut ctx, ia, 0).as_deref(), Some("127.0.0.1"));
        assert_eq!(text(&mut ctx, ia, 1).as_deref(), Some("127.0.0.1"));
    }
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "every address's root is released"
    );
}
