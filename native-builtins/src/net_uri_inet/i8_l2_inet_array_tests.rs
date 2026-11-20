use super::native_inet_get_all_by_name;
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};
use cratonvm_types::Value;

/// `getAllByName` answers the declared `InetAddress[]`, with every
/// element carrying the host name and a resolved address (i8-L2).
#[test]
fn i8_l2_get_all_by_name_returns_an_inet_address_typed_array() {
    let mut ctx = mock_ctx();
    let ia = ctx
        .ensure_class_initialized("java/net/InetAddress")
        .expect("mock registers the class");
    let host = ctx.create_string("127.0.0.1");
    let arr = match native_inet_get_all_by_name(&mut ctx, &[Value::Object(Some(host))]) {
        Ok(Some(Value::Object(Some(arr)))) => arr,
        other => panic!("expected an array, got {other:?}"),
    };
    assert_eq!(ctx.ref_array_component(arr), Some(ia));
    assert!(ctx.array_length(arr) >= 1);
    let first = match ctx.get_array_element(arr, 0) {
        Value::Object(Some(o)) => o,
        other => panic!("expected an InetAddress, got {other:?}"),
    };
    let name = match ctx.get_field(first, 0) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    };
    assert_eq!(name.as_deref(), Some("127.0.0.1"));
}
