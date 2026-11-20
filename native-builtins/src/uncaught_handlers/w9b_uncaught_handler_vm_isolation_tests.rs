use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

const VM_A: usize = 0x9B00_0901;
const VM_B: usize = 0x9B00_0902;

fn at(addr: usize) -> ObjectRef {
    // SAFETY: a key / cached value only, never dereferenced (the mock's
    // identity hash is the address; its var-handle registry is empty).
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

/// Forgets this module's VMs (keys and rows) even when an assertion
/// fails; each test names only its own VMs (lesson pp).
struct Forget(&'static [usize]);
impl Drop for Forget {
    fn drop(&mut self) {
        for &vm in self.0 {
            crate::forget_vm_lock_keys(vm);
            forget_vm_uncaught_handlers(vm);
        }
    }
}

#[test]
fn handlers_and_default_handler_are_per_vm() {
    let _forget = Forget(&[VM_A, VM_B]);
    let mut a = crate::test_utils::mock_ctx();
    a.set_vm_identity(VM_A);
    let b = crate::test_utils::mock_ctx();
    b.set_vm_identity(VM_B);
    let (thread, handler, default) = (at(0x9B04_1000), at(0x9B04_2000), at(0x9B04_3000));

    store_handler(&mut a, thread, handler);
    assert_eq!(get_uncaught_handler(&a, thread), Some(handler));
    assert_eq!(
        get_uncaught_handler(&b, thread),
        None,
        "VM B's thread with the same identity hash must not see VM A's handler"
    );
    assert_eq!(take_uncaught_handler(&b, thread), None);
    assert_eq!(
        get_uncaught_handler(&a, thread),
        Some(handler),
        "B's take left A's row"
    );

    store_default_handler(&mut a, Some(default));
    assert_eq!(default_uncaught_handler(&a), Some(default));
    assert_eq!(default_uncaught_handler(&b), None);

    forget_vm_uncaught_handlers(VM_A);
    forget_vm_uncaught_handlers(VM_B);
    assert_eq!(get_uncaught_handler(&a, thread), None);
    assert_eq!(default_uncaught_handler(&a), None);
}

/// gc-common w29-a (`common-w28b-remaining-identity-hash-keyed-side-tables`
/// rank 22): two Threads of ONE VM with one identity hash keep their own
/// handlers, and a dead Thread's row -- and its handler's root -- goes
/// with its lock key. Fake Threads 4 GiB apart share the mock's hash (the
/// address truncated to `i32`); these paths never dereference them.
#[test]
fn w29a_colliding_threads_keep_their_own_handlers() {
    const VM: usize = 0x29a0_9B01;
    let _forget = Forget(&[VM]);
    let mut ctx = crate::test_utils::mock_ctx();
    ctx.set_vm_identity(VM);
    let (thread_a, thread_b) = (at(0x1_29a0_9b08), at(0x2_29a0_9b08));
    assert_eq!(
        ctx.identity_hash_code(thread_a),
        ctx.identity_hash_code(thread_b),
        "premise: one identity hash"
    );
    let (handler_a, handler_b) = (at(0x29a0_9c08), at(0x29a0_9d08));
    let before = ctx.global_root_count();

    store_handler(&mut ctx, thread_a, handler_a);
    assert_eq!(
        get_uncaught_handler(&ctx, thread_b),
        None,
        "thread B ran thread A's handler"
    );
    store_handler(&mut ctx, thread_b, handler_b);
    assert_eq!(
        get_uncaught_handler(&ctx, thread_a),
        Some(handler_a),
        "B's set overwrote A's"
    );
    assert_eq!(get_uncaught_handler(&ctx, thread_b), Some(handler_b));
    assert_eq!(ctx.global_root_count(), before + 2);

    // Thread A dies: its key is freed and its row goes; the root is
    // released by the VM's next store.
    let a_addr = thread_a.as_ptr() as usize;
    assert_eq!(
        crate::gc_sweep_lock_keys(VM, &|addr: usize| addr != a_addr),
        1
    );
    assert_eq!(get_uncaught_handler(&ctx, thread_b), Some(handler_b));
    store_handler(&mut ctx, thread_b, handler_b);
    assert_eq!(
        ctx.global_root_count(),
        before + 1,
        "the dead Thread's handler must stop being rooted"
    );
    assert_eq!(
        get_uncaught_handler(&ctx, thread_a),
        None,
        "a new Thread at A's address inherited A's handler"
    );

    // `take` removes only B's row, and its root goes at the next store.
    assert_eq!(take_uncaught_handler(&ctx, thread_b), Some(handler_b));
    assert_eq!(get_uncaught_handler(&ctx, thread_b), None);
    store_handler(&mut ctx, thread_a, handler_a);
    clear_handler(&mut ctx, thread_a);
    assert_eq!(ctx.global_root_count(), before);
}
