use super::{serialization_ctor_abstract_refusal, serialization_ctor_nullary_refusal};
use crate::test_utils::mock_ctx;
use cratonvm_native_api::{FieldMetadata, NativeHeapAccess};
use cratonvm_types::access_flags::{ACC_ABSTRACT, ACC_INTERFACE, ACC_PUBLIC};
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

/// Failed before wave 98: the serialization arm allocated an instance of
/// an abstract class (and of an interface) and ignored a non-empty
/// argument array for the nullary constructor.
#[test]
fn nullary_serialization_ctor_refuses_extra_arguments_then_abstract_targets() {
    let mut ctx = mock_ctx();
    let ctor_class = ctx.declare_loaded_class("cratonvm/test/I98CtorShape");
    ctx.set_declared_fields(
        ctor_class,
        vec![field("parameterTypes", "[Ljava/lang/Class;", 0, ctor_class)],
    );
    let concrete = ctx.declare_loaded_class("cratonvm/test/I98Concrete");
    ctx.set_class_access_flags(concrete, ACC_PUBLIC);
    let abstract_class = ctx.declare_loaded_class("cratonvm/test/I98Abstract");
    ctx.set_class_access_flags(abstract_class, ACC_PUBLIC | ACC_ABSTRACT);
    let interface = ctx.declare_loaded_class("cratonvm/test/I98Iface");
    ctx.set_class_access_flags(interface, ACC_PUBLIC | ACC_ABSTRACT | ACC_INTERFACE);

    let ctor = ctx.alloc_object(ctor_class, 1);
    let no_params = ctx.new_ref_array(ctor_class, 0);
    ctx.set_field_by_name(ctor, "parameterTypes", Value::Object(Some(no_params)));

    // Concrete class, no arguments: nothing to refuse.
    assert!(serialization_ctor_nullary_refusal(&mut ctx, ctor, concrete, None).is_none());
    assert!(serialization_ctor_abstract_refusal(&mut ctx, concrete).is_none());
    let empty = ctx.new_ref_array(ctor_class, 0);
    let empty_pin = ctx.pin_native_root(empty);
    assert!(
        serialization_ctor_nullary_refusal(&mut ctx, ctor, concrete, Some((empty_pin, empty)))
            .is_none()
    );
    // One argument for a nullary constructor: refused (the count check).
    let one = ctx.new_ref_array(ctor_class, 1);
    let one_pin = ctx.pin_native_root(one);
    assert!(
        serialization_ctor_nullary_refusal(&mut ctx, ctor, concrete, Some((one_pin, one)))
            .is_some()
    );
    // Abstract class and interface: refused.
    assert!(serialization_ctor_abstract_refusal(&mut ctx, abstract_class).is_some());
    assert!(serialization_ctor_abstract_refusal(&mut ctx, interface).is_some());
    assert!(serialization_ctor_nullary_refusal(&mut ctx, ctor, abstract_class, None).is_some());
    // A constructor WITH parameters is `serialization_ctor_with_arguments`'s.
    let one_param = ctx.new_ref_array(ctor_class, 1);
    ctx.set_field_by_name(ctor, "parameterTypes", Value::Object(Some(one_param)));
    assert!(serialization_ctor_nullary_refusal(&mut ctx, ctor, abstract_class, None).is_none());
}
