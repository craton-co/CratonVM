use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// JIT r22 wave 11, lane intrinsics: the essential `String.lastIndexOf(String)`
/// row answers a UTF-16 index and raises for a null needle (it was a closure
/// answering a code-point index and -1).
#[test]
fn essential_last_index_of_str_counts_utf16_units() {
    let mut r = NativeMethodRegistry::new();
    register_essential_natives_with_shims(&mut r, app_shims::ShimSelection::NONE);
    let f = r
        .find("java/lang/String", "lastIndexOf", "(Ljava/lang/String;)I")
        .expect("lastIndexOf(String) is registered");
    let mut ctx = crate::test_utils::mock_ctx();
    let hay = ctx.create_string("\u{10437}ab");
    let needle = ctx.create_string("ab");
    let got = f(&mut ctx, &[Value::Object(Some(hay)), Value::Object(Some(needle))]);
    assert_eq!(got.ok(), Some(Some(Value::Int(2))), "a UTF-16 index, as HotSpot");
    let null_needle = f(&mut ctx, &[Value::Object(Some(hay)), Value::Object(None)]);
    assert!(null_needle.is_err(), "HotSpot throws for a null needle");
    // The sibling `indexOf(String)` / `indexOf(String, int)` rows, same rule.
    let g = r
        .find("java/lang/String", "indexOf", "(Ljava/lang/String;)I")
        .expect("indexOf(String) is registered");
    let got = g(&mut ctx, &[Value::Object(Some(hay)), Value::Object(Some(needle))]);
    assert_eq!(got.ok(), Some(Some(Value::Int(2))));
    assert!(g(&mut ctx, &[Value::Object(Some(hay)), Value::Object(None)]).is_err());
    let h = r
        .find("java/lang/String", "indexOf", "(Ljava/lang/String;I)I")
        .expect("indexOf(String, int) is registered");
    for (from, want) in [(i32::MIN, 2), (-1, 2), (2, 2), (3, -1), (99, -1)] {
        let got = h(
            &mut ctx,
            &[Value::Object(Some(hay)), Value::Object(Some(needle)), Value::Int(from)],
        );
        assert_eq!(got.ok(), Some(Some(Value::Int(want))), "from {from}");
    }
    assert!(h(&mut ctx, &[Value::Object(Some(hay)), Value::Object(None), Value::Int(0)]).is_err());
}
