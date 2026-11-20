use super::*;

fn vm(validate_once: bool) -> crate::vm::SharedVm {
    crate::vm::SharedVm::new(crate::config::VmConfig {
        static_memo_key_validate_once: validate_once,
        ..crate::config::VmConfig::default()
    })
}

/// Both arms key a live object by its class, decline an array and key a
/// primitive or `null` first argument (or none) as not-an-object.
#[test]
fn both_arms_give_the_same_key() {
    for validate_once in [false, true] {
        let vm = vm(validate_once);
        let obj = vm.mem.heap.alloc_object(ClassId::new(7), 2);
        let arr = vm.mem.heap.alloc_array(
            ClassId::new(7),
            cratonvm_types::ArrayElementType::Byte,
            4,
        );
        use cratonvm_types::Value;
        let key = |args: &[Value]| static_first_arg_key(&vm, args);
        let first_obj = [Value::Object(Some(obj)), Value::Int(1)];
        assert_eq!(key(&first_obj), Some(7), "{validate_once}");
        assert_eq!(key(&[Value::Object(Some(arr))]), None, "{validate_once}");
        assert_eq!(key(&[Value::Int(3)]), Some(STATIC_FIRST_NOT_AN_OBJECT));
        assert_eq!(key(&[Value::Object(None)]), Some(STATIC_FIRST_NOT_AN_OBJECT));
        assert_eq!(key(&[]), Some(STATIC_FIRST_NOT_AN_OBJECT));
    }
}

/// The validate-once arm declines an address that is not an object start
/// (the default arm keyed it under class 0): no memo row for it.
#[test]
fn the_validated_arm_declines_an_interior_address() {
    let vm = vm(true);
    let obj = vm.mem.heap.alloc_object(ClassId::new(7), 4);
    // SAFETY: 8-byte aligned and non-null; only its address is examined.
    let inner = unsafe { cratonvm_types::ObjectRef::from_raw(obj.as_ptr().add(8)) };
    assert_eq!(
        static_first_object_class_validated_once(&vm.mem.heap, obj),
        Some(ClassId::new(7))
    );
    assert_eq!(
        static_first_object_class_validated_once(&vm.mem.heap, inner),
        None
    );
}
