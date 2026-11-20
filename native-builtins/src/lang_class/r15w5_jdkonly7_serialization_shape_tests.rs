use super::constructor_is_plain_with_parameters;
use crate::test_utils::mock_ctx;
use cratonvm_native_api::{FieldMetadata, NativeClassAccess, NativeHeapAccess};
use cratonvm_types::{ClassId, Value};

fn field(name: &str, descriptor: &str, slot_index: usize, owner: ClassId) -> FieldMetadata {
    FieldMetadata {
        name: name.to_string(),
        descriptor: descriptor.to_string(),
        access_flags: 0x0002,
        slot_index,
        declaring_class_id: owner,
        is_static: false,
    }
}

/// Failed before J1: the serialization arm keyed on the accessor alone, so
/// a plain constructor with parameters was built through `<init>()V`.
#[test]
fn only_an_ancestor_or_nullary_constructor_is_the_serialization_shape() {
    let mut ctx = mock_ctx();
    let ctor_class = ctx.declare_loaded_class("cratonvm/test/R15w5CtorShape");
    ctx.set_declared_fields(
        ctor_class,
        vec![
            field("clazz", "Ljava/lang/Class;", 0, ctor_class),
            field("parameterTypes", "[Ljava/lang/Class;", 1, ctor_class),
        ],
    );
    let target = ctx.declare_loaded_class("cratonvm/test/R15w5Target");
    let ancestor = ctx.declare_loaded_class("cratonvm/test/R15w5Ancestor");
    let ctor = ctx.alloc_object(ctor_class, 2);
    let target_mirror = ctx.get_class_mirror(target);
    ctx.set_field_by_name(ctor, "clazz", Value::Object(Some(target_mirror)));
    let one_param = ctx.new_ref_array(ctor_class, 1);
    ctx.set_field_by_name(ctor, "parameterTypes", Value::Object(Some(one_param)));
    assert_eq!(ctx.class_id_from_mirror(target_mirror), Some(target));
    // A plain constructor `Target(x)`: its handle allocates `clazz` itself.
    assert!(constructor_is_plain_with_parameters(&ctx, ctor, target));
    // A serialization constructor: `clazz` is an ancestor of what it allocates.
    assert!(!constructor_is_plain_with_parameters(&ctx, ctor, ancestor));
    // A plain nullary constructor: the arm's construction is the same.
    let no_params = ctx.new_ref_array(ctor_class, 0);
    ctx.set_field_by_name(ctor, "parameterTypes", Value::Object(Some(no_params)));
    assert!(!constructor_is_plain_with_parameters(&ctx, ctor, target));
}
