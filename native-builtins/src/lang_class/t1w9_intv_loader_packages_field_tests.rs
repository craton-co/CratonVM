use super::*;
use crate::test_utils::mock_ctx;
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};

fn field(name: &str, slot_index: usize, owner: ClassId) -> FieldMetadata {
    FieldMetadata {
        name: name.to_string(),
        descriptor: "Ljava/util/Map;".to_string(),
        access_flags: 0x0012,
        slot_index,
        declaring_class_id: owner,
        is_static: false,
    }
}

#[test]
fn a_subclass_field_named_packages_does_not_shadow_the_loaders_map() {
    let mut ctx = mock_ctx();
    let cl = ctx.ensure_class_initialized("java/lang/ClassLoader").unwrap();
    let sub = ctx.ensure_class_initialized("t1w9/ShadowingLoader").unwrap();
    ctx.set_superclass(sub, cl);
    ctx.set_declared_fields(cl, vec![field("packages", 0, cl)]);
    // The receiver class resolves the name to ITS field first, as
    // `resolve_field_index_in_hierarchy` does from a subclass.
    ctx.set_declared_fields(sub, vec![field("packages", 3, sub)]);
    let loader = ctx.alloc_object(sub, 4);
    let jdk_map = ctx.fresh_object_ref();
    let user_field = ctx.fresh_object_ref();
    ctx.set_field(loader, 0, Value::Object(Some(jdk_map)));
    ctx.set_field(loader, 3, Value::Object(Some(user_field)));
    assert_eq!(
        ctx.get_field_by_name(loader, "packages"),
        Value::Object(Some(user_field)),
        "the by-name read the readers used reaches the subclass field"
    );
    assert_eq!(
        classloader_packages_field(&ctx, loader),
        Value::Object(Some(jdk_map))
    );
}
