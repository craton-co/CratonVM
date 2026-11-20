use super::*;

#[test]
fn manifest_rows_belong_to_their_vm_and_die_with_it() {
    const VM_A: usize = 0x21A0_0A01;
    const VM_B: usize = 0x21A0_0A02;
    const JAR: &str = "file:/tmp/jr21-manifest.jar";
    JDK_CLASS_PATH_MANIFESTS.with(VM_A, |rows| {
        rows.insert(JAR.to_string(), 11);
    });
    JDK_CLASS_PATH_MANIFESTS.with(VM_B, |rows| {
        rows.insert(JAR.to_string(), 22);
    });
    let row = |vm: usize| {
        JDK_CLASS_PATH_MANIFESTS
            .peek(vm, |rows| rows.get(JAR).copied())
            .flatten()
    };
    assert_eq!(row(VM_A), Some(11), "VM A answers its own handle");
    assert_eq!(row(VM_B), Some(22), "VM B answers its own handle");

    forget_vm_lang_class_tables(VM_A);
    assert!(
        !JDK_CLASS_PATH_MANIFESTS.has_row(VM_A),
        "VM teardown must drop the VM's manifest rows"
    );
    assert_eq!(row(VM_B), Some(22), "other VMs' rows survive");

    forget_vm_lang_class_tables(VM_B);
    assert!(!JDK_CLASS_PATH_MANIFESTS.has_row(VM_B));
}
