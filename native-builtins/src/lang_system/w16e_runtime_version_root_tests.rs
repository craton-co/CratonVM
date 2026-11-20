use super::*;
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// Each VM keeps its own singleton, and teardown drops the row. Under the
/// old process-wide slot, VM B's publish overwrote the one handle with a
/// slot number of B's table, and VM A then resolved that number in ITS
/// table (here: nothing, so A lost its singleton).
#[test]
fn each_vm_keeps_its_own_runtime_version_singleton() {
    const VM_A: usize = 0xE16_0311;
    const VM_B: usize = 0xE16_0312;
    RUNTIME_VERSION_ROOT.forget(VM_A);
    RUNTIME_VERSION_ROOT.forget(VM_B);
    let mut a = mock_ctx();
    a.set_vm_identity(VM_A);
    let mut b = mock_ctx();
    b.set_vm_identity(VM_B);
    // Offset B's handle numbering, so the two VMs' first singleton
    // handles are different numbers.
    let filler = b.fresh_object_ref();
    let _ = b.add_global_root(filler);

    let version_a = a.fresh_object_ref();
    let version_b = b.fresh_object_ref();
    assert_eq!(runtime_version_publish(&mut a, version_a), version_a);
    assert_eq!(runtime_version_publish(&mut b, version_b), version_b);
    assert_eq!(runtime_version_cached(&a), Some(version_a));
    assert_eq!(runtime_version_cached(&b), Some(version_b));
    // A second publish in a VM answers that VM's existing singleton.
    let another = a.fresh_object_ref();
    assert_eq!(runtime_version_publish(&mut a, another), version_a);

    forget_vm_system_singletons(VM_A);
    assert_eq!(runtime_version_cached(&a), None, "teardown drops A's row");
    assert_eq!(runtime_version_cached(&b), Some(version_b), "and only A's");
    forget_vm_system_singletons(VM_B);
}
