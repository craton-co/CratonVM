use super::{java_modifier_to_string, native_class_new_instance};
use crate::test_utils::mock_ctx;
use cratonvm_native_api::{MethodMetadata, NativeClassAccess, NativeHeapAccess};
use cratonvm_types::error::MethodCallFailed;
use cratonvm_types::{ClassId, Value};

fn nullary_ctor(class_id: ClassId, access_flags: u16) -> MethodMetadata {
    MethodMetadata {
        name: "<init>".to_string(),
        descriptor: "()V".to_string(),
        access_flags,
        declaring_class_id: class_id,
        exceptions: Vec::new(),
        signature: None,
    }
}

#[test]
fn modifier_words_come_in_the_jdks_order() {
    assert_eq!(
        java_modifier_to_string(0x0001 | 0x0400 | 0x0008),
        "public abstract static"
    );
    assert_eq!(
        java_modifier_to_string(0x0002 | 0x0080),
        "private transient"
    );
    assert_eq!(java_modifier_to_string(0x1000), "");
}

/// Failed before JO5-5: the abstract class was allocated, its `<init>`
/// run, and the fabricated instance returned.
#[test]
fn class_new_instance_refuses_an_abstract_class() {
    let mut ctx = mock_ctx();
    let cid = ctx.declare_loaded_class("cratonvm/test/R15w4Abstract");
    ctx.set_class_access_flags(cid, 0x0001 | 0x0400);
    ctx.set_declared_methods(cid, vec![nullary_ctor(cid, 0x0001)]);
    let mirror = ctx.get_class_mirror(cid);
    let result = native_class_new_instance(&mut ctx, &[Value::Object(Some(mirror))]);
    let Err(MethodCallFailed::ExceptionThrown(exc)) = result else {
        panic!("Class.newInstance of an abstract class must throw");
    };
    let class = ctx.class_name_of_id(ctx.class_id_of_object(exc));
    assert_eq!(class.as_deref(), Some("java/lang/InstantiationException"));
}

/// Failed before JO5-5: an interface went on to `<init>` dispatch.
#[test]
fn class_new_instance_refuses_an_interface() {
    let mut ctx = mock_ctx();
    let cid = ctx.declare_loaded_class("cratonvm/test/R15w4Iface");
    ctx.set_class_access_flags(cid, 0x0001 | 0x0200 | 0x0400);
    let mirror = ctx.get_class_mirror(cid);
    let result = native_class_new_instance(&mut ctx, &[Value::Object(Some(mirror))]);
    let Err(MethodCallFailed::ExceptionThrown(exc)) = result else {
        panic!("Class.newInstance of an interface must throw");
    };
    let class = ctx.class_name_of_id(ctx.class_id_of_object(exc));
    assert_eq!(class.as_deref(), Some("java/lang/InstantiationException"));
}
