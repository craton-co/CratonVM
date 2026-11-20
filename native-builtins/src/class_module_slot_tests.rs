/// Two VMs on one thread whose `Class` keeps `module` in different slots:
/// each reads its own, before and after the other has filled the memo.
#[test]
fn the_module_slot_memo_does_not_answer_another_vm() {
    use crate::test_utils::MockNativeContext;
    use cratonvm_native_api::{FieldMetadata, NativeContext};
    let make = |vm: usize, slot: usize| {
        let mut mock = MockNativeContext::new();
        mock.set_vm_identity(vm);
        let ctx: &mut dyn NativeContext = &mut mock;
        let cid = ctx
            .ensure_class_initialized("java/lang/Class")
            .expect("Class");
        mock.set_declared_fields(
            cid,
            vec![FieldMetadata {
                name: "module".to_string(),
                descriptor: "Ljava/lang/Module;".to_string(),
                access_flags: 0,
                slot_index: slot,
                declaring_class_id: cid,
                is_static: false,
            }],
        );
        mock
    };
    let a = make(7_101, 3);
    let b = make(7_102, 5);
    assert_eq!(super::class_module_slot(&a), Some(3));
    assert_eq!(super::class_module_slot(&b), Some(5));
    assert_eq!(super::class_module_slot(&a), Some(3));
}
