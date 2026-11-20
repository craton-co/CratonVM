use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_types::ClassId;

const VM_A: usize = 0xA10_7101;
const VM_B: usize = 0xA10_7102;

struct ForgetBoth;
impl Drop for ForgetBoth {
    fn drop(&mut self) {
        crate::forget_vm_lock_keys(VM_A);
        crate::forget_vm_lock_keys(VM_B);
        forget_vm_thread_local_side_tables(VM_A);
        forget_vm_thread_local_side_tables(VM_B);
    }
}

#[test]
fn with_initial_suppliers_and_inheritable_rows_are_per_vm() {
    let _forget = ForgetBoth;
    let mut a = MockNativeContext::new();
    a.set_vm_identity(VM_A);
    let b = MockNativeContext::new();
    b.set_vm_identity(VM_B);

    // `withInitial` in A registers its supplier under A only. Identity
    // hashes collide across VMs by construction, so B looking up the
    // same object must miss rather than run A's supplier (since
    // gc-common w29-a the row is keyed by A's weak lock key for it, which
    // B's lookup never matches).
    let supplier = a.fresh_object_ref();
    let tl = match native_tl_with_initial(&mut a, &[Value::Object(Some(supplier))]) {
        Ok(Some(Value::Object(Some(tl)))) => tl,
        other => panic!("withInitial answered {:?}", other.ok()),
    };
    assert_eq!(tl_with_initial_supplier(&a, tl), Some(supplier));
    assert_eq!(
        tl_with_initial_supplier(&b, tl),
        None,
        "VM B must not find VM A's withInitial supplier"
    );

    // An `InheritableThreadLocal` constructed in A is inherited in A only.
    let itl = a.alloc_object(ClassId::new(0), 0);
    native_itl_init(&mut a, &[Value::Object(Some(itl))]).expect("ITL <init>");
    assert!(tl_vm_has_inheritable(VM_A));
    assert!(
        !tl_vm_has_inheritable(VM_B),
        "VM B's children must not run childValue on VM A's ThreadLocal"
    );

    // Teardown drops A's rows.
    forget_vm_thread_local_side_tables(VM_A);
    assert!(!tl_vm_has_inheritable(VM_A));
    assert_eq!(tl_with_initial_supplier(&a, tl), None);
}

/// gc-common w22-a (`common-w21a-more-process-wide-global-root-handle-caches`
/// item 7): a snapshot queued for a child thread that never started is
/// dropped with its VM, and another VM's queued snapshot is not.
#[test]
fn w22a_inherited_pending_rows_go_with_their_vm() {
    const W22A_VM_A: usize = 0x0A22_8A08;
    const W22A_VM_B: usize = 0x0A22_8B08;
    struct Forget;
    impl Drop for Forget {
        fn drop(&mut self) {
            forget_vm_thread_local_side_tables(W22A_VM_A);
            forget_vm_thread_local_side_tables(W22A_VM_B);
        }
    }
    let _forget = Forget;
    let mut a = MockNativeContext::new();
    a.set_vm_identity(W22A_VM_A);
    let mut b = MockNativeContext::new();
    b.set_vm_identity(W22A_VM_B);
    let child_a = a.fresh_object_ref();
    let child_b = b.fresh_object_ref();
    let bucket = |owner: ObjectRef| -> InheritedTlBucket {
        vec![(
            7,
            TlRow {
                owner: 0,
                owner_fallback: owner,
                value: ThreadLocalValue::Plain(Value::Int(7)),
            },
        )]
    };
    queue_inherited_tl_for_child(&mut a, child_a, bucket(child_a));
    queue_inherited_tl_for_child(&mut b, child_b, bucket(child_b));
    assert!(inherited_tl_queued_for_child(&a, child_a));
    assert!(inherited_tl_queued_for_child(&b, child_b));

    forget_vm_thread_local_side_tables(W22A_VM_A);
    assert!(!inherited_tl_queued_for_child(&a, child_a));
    assert!(
        !tl_inherited_pending()
            .lock()
            .keys()
            .any(|&(vm, _)| vm == W22A_VM_A),
        "no row of the torn-down VM is left"
    );
    assert!(
        inherited_tl_queued_for_child(&b, child_b),
        "B's queued snapshot survives A's teardown"
    );
}
