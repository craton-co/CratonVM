use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

/// Two live selectors with ONE identity hash (the mock's hash is the
/// address truncated to `i32`) get their own sockets; one that dies
/// unclosed takes its socket with its lock key; VM teardown takes the rest.
#[test]
fn w29d_same_hash_selectors_get_their_own_wakeup_sockets() {
    const VM: usize = 0x29D0_5E01;
    let ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM);
    let (a_addr, b_addr) = (0x1_29D0_5E00usize, 0x2_29D0_5E00usize);
    // SAFETY: keys only, 8-byte aligned, never dereferenced here.
    let a = unsafe { ObjectRef::from_raw(a_addr as *mut u8) };
    let b = unsafe { ObjectRef::from_raw(b_addr as *mut u8) };
    assert_eq!(
        ctx.identity_hash_code(a),
        ctx.identity_hash_code(b),
        "premise: the two selectors share an identity hash"
    );
    let addr_a = ensure_wakeup_channel(&ctx, a, |ch| ch.socket.local_addr().ok()).flatten();
    let addr_b = ensure_wakeup_channel(&ctx, b, |ch| ch.socket.local_addr().ok()).flatten();
    if addr_a.is_none() || addr_b.is_none() {
        // No loopback socket on this host: nothing to compare.
        crate::forget_vm_lock_keys(VM);
        return;
    }
    assert_ne!(addr_a, addr_b, "one wakeup socket per selector");

    let (ka, kb) = (selector_key(&ctx, a), selector_key(&ctx, b));
    crate::gc_sweep_lock_keys(VM, &|addr| addr == b_addr);
    assert!(
        !selector_wakeups().lock().contains_key(&ka),
        "a selector that died unclosed takes its socket with it"
    );
    assert!(selector_wakeups().lock().contains_key(&kb));
    crate::forget_vm_lock_keys(VM);
    assert!(!selector_wakeups().lock().contains_key(&kb));
}
