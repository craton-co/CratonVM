use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

const VM_A: usize = 0x17F0_0A01;
const VM_B: usize = 0x17F0_0B01;

#[test]
fn w17f_a_mount_on_one_vm_is_invisible_to_another_vm_with_the_same_thread_id() {
    let mut a = MockNativeContext::new();
    a.set_vm_identity(VM_A);
    let b = MockNativeContext::new();
    b.set_vm_identity(VM_B);
    assert_eq!(a.thread_id(), b.thread_id(), "the collision under test");

    let cont = a.alloc_object(cratonvm_types::ClassId::new(0), 16);
    let frame = cont_push(&mut a, cont);

    let seen_by_a = cont_current(&a, Value::Object(None));
    let seen_by_b = cont_current(&b, Value::Object(None));
    let popped = cont_pop(&mut a, frame);
    let after_pop = cont_current(&a, Value::Object(None));
    forget_vm_concurrent_tables(VM_A);
    forget_vm_concurrent_tables(VM_B);

    assert_eq!(
        seen_by_a,
        Some(cont),
        "the mounting VM sees its continuation"
    );
    assert_eq!(
        seen_by_b, None,
        "another VM's thread with the same id must not see it (nor resolve \
         VM A's global-root handle against its own root table)"
    );
    assert_eq!(popped, Some(cont));
    assert_eq!(after_pop, None);
    assert!(!CONT_STACKS.has_row(VM_A), "forget drops the VM's row");
}

/// gc-common w29-a (`common-w28b-remaining-identity-hash-keyed-side-tables`
/// rank 23): a scope that shares its identity hash with the mounted
/// continuation's scope has no mounted continuation. The mock's identity
/// hash is the address truncated to `i32`, so two fake scopes 4 GiB apart
/// share one; they are only compared, never dereferenced.
#[test]
fn w29a_a_colliding_scope_has_no_mounted_continuation() {
    const VM: usize = 0x29a0_0C01;
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM);
    // SAFETY: compared only, never dereferenced.
    let (scope_a, scope_b) = unsafe {
        (
            ObjectRef::from_raw(0x1_29a0_0c08usize as *mut u8),
            ObjectRef::from_raw(0x2_29a0_0c08usize as *mut u8),
        )
    };
    assert_eq!(
        ctx.identity_hash_code(scope_a),
        ctx.identity_hash_code(scope_b),
        "premise: one identity hash"
    );
    let cont = ctx.alloc_object(cratonvm_types::ClassId::new(0), 16);
    let slot = cont_slots(&ctx, cont).scope;
    ctx.set_field(cont, slot, Value::Object(Some(scope_a)));
    let frame = cont_push(&mut ctx, cont);

    let for_a = cont_current(&ctx, Value::Object(Some(scope_a)));
    let for_b = cont_current(&ctx, Value::Object(Some(scope_b)));
    let for_any = cont_current(&ctx, Value::Object(None));
    let popped = cont_pop(&mut ctx, frame);
    forget_vm_concurrent_tables(VM);

    assert_eq!(
        for_a,
        Some(cont),
        "the mounted scope finds its continuation"
    );
    assert_eq!(
        for_b, None,
        "a colliding scope answered another scope's continuation"
    );
    assert_eq!(for_any, Some(cont));
    assert_eq!(popped, Some(cont));
}
