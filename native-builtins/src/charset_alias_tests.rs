use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn charset_for_name_initializes_alias_fields_and_aliases_native_caches_set() {
    let mut registry = NativeMethodRegistry::new();
    register_charset_natives(&mut registry);
    let for_name = registry
        .find(
            "java/nio/charset/Charset",
            "forName",
            "(Ljava/lang/String;)Ljava/nio/charset/Charset;",
        )
        .expect("Charset.forName native");
    let aliases = registry
        .find("java/nio/charset/Charset", "aliases", "()Ljava/util/Set;")
        .expect("Charset.aliases native");

    let mut ctx = MockNativeContext::new();
    let name = ctx.create_string("UTF-8");
    let charset = match for_name(&mut ctx, &[Value::Object(Some(name))])
        .expect("forName succeeds")
        .expect("forName returns value")
    {
        Value::Object(Some(o)) => o,
        other => panic!("forName should return Charset, got {other:?}"),
    };

    let alias_array = match ctx.get_field_by_name(charset, "aliases") {
        Value::Object(Some(o)) => o,
        other => panic!("Charset.aliases field should be an empty array, got {other:?}"),
    };
    assert_eq!(ctx.array_length(alias_array), 0);
    assert_eq!(
        ctx.get_field_by_name(charset, "aliasSet"),
        Value::Object(None)
    );

    let alias_set = match aliases(&mut ctx, &[Value::Object(Some(charset))])
        .expect("aliases native succeeds")
        .expect("aliases returns value")
    {
        Value::Object(Some(o)) => o,
        other => panic!("aliases should return Set, got {other:?}"),
    };
    assert_eq!(
        ctx.get_field_by_name(charset, "aliasSet"),
        Value::Object(Some(alias_set))
    );
}
