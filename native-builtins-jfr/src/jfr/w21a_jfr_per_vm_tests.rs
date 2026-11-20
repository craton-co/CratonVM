use super::*;
use cratonvm_native_builtins::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

// Private VM identities: no other test publishes under them.
const W21A_VM_A: usize = 0x0A21_1008;
const W21A_VM_B: usize = 0x0A21_2008;
const W21A_VM_C: usize = 0x0A21_3008;
const W21A_EVENT: &str = "w21a.test.ProbeEvent";

#[test]
fn w21a_event_configurations_are_per_vm_and_forgotten_with_the_vm() {
    let mut a = mock_ctx();
    a.set_vm_identity(W21A_VM_A);
    let mut b = mock_ctx();
    b.set_vm_identity(W21A_VM_B);
    let config_a = a.fresh_object_ref();
    // B's global-ref table: slot 1 holds an unrelated object, the same
    // index VM A's handle for its configuration gets in A's table.
    let unrelated_b = b.fresh_object_ref();
    let unrelated_handle = b.add_global_root(unrelated_b);

    set_event_configuration(&mut a, W21A_EVENT.to_string(), Some(config_a));
    assert_eq!(event_configuration(&a, W21A_EVENT), Some(config_a));
    assert_eq!(
        event_configuration(&b, W21A_EVENT),
        None,
        "VM B must not resolve VM A's handle in its own table"
    );

    let config_b = b.fresh_object_ref();
    set_event_configuration(&mut b, W21A_EVENT.to_string(), Some(config_b));
    assert_eq!(event_configuration(&b, W21A_EVENT), Some(config_b));
    assert_eq!(
        b.resolve_global_root(unrelated_handle),
        Some(unrelated_b),
        "B's first set must not release the root of B's that A's row names"
    );
    assert_eq!(event_configuration(&a, W21A_EVENT), Some(config_a));

    // A replace releases the replaced root in the SAME VM only.
    let config_b2 = b.fresh_object_ref();
    set_event_configuration(&mut b, W21A_EVENT.to_string(), Some(config_b2));
    assert_eq!(event_configuration(&b, W21A_EVENT), Some(config_b2));
    assert_eq!(b.resolve_global_root(unrelated_handle), Some(unrelated_b));
    set_event_configuration(&mut b, W21A_EVENT.to_string(), None);
    assert_eq!(event_configuration(&b, W21A_EVENT), None);
    assert_eq!(event_configuration(&a, W21A_EVENT), Some(config_a));

    forget_vm_jfr_state(W21A_VM_A);
    assert_eq!(event_configuration(&a, W21A_EVENT), None);
    assert!(!EVENT_CONFIGURATIONS.has_row(W21A_VM_A));
    forget_vm_jfr_state(W21A_VM_B);
    forget_vm_jfr_state(W21A_VM_A);
}

#[test]
fn w21a_excluded_threads_and_dump_path_are_per_vm_and_forgotten() {
    struct Forget;
    impl Drop for Forget {
        fn drop(&mut self) {
            cratonvm_native_builtins::forget_vm_lock_keys(W21A_VM_C);
            cratonvm_native_builtins::forget_vm_lock_keys(W21A_VM_C + 0x10);
        }
    }
    let _forget = Forget;
    let c = mock_ctx();
    c.set_vm_identity(W21A_VM_C);
    let other = mock_ctx();
    other.set_vm_identity(W21A_VM_C + 0x10);
    // SAFETY: a key only, never dereferenced.
    let thread = unsafe { ObjectRef::from_raw(0x0A21_3510usize as *mut u8) };
    jfr_exclude_thread(&c, thread).expect("exclude");
    DUMP_PATH.with(W21A_VM_C, |path| *path = Some("w21a-dump".to_string()));
    CREATED.with(W21A_VM_C, |created| *created = true);
    assert_eq!(
        CREATED.peek(W21A_VM_C + 0x10, |created| *created),
        None,
        "another VM's createJFR state is its own"
    );
    assert!(jfr_thread_is_excluded(&c, thread));
    assert!(
        !jfr_thread_is_excluded(&other, thread),
        "another VM sees no exclusion"
    );
    java_events_admitted()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .entry(W21A_VM_C)
        .or_default()
        .insert(W21A_EVENT.to_string());

    forget_vm_jfr_state(W21A_VM_C);
    assert!(!jfr_thread_is_excluded(&c, thread));
    assert!(!excluded_threads_lock().values().any(|&vm| vm == W21A_VM_C));
    assert!(!DUMP_PATH.has_row(W21A_VM_C));
    assert!(!CREATED.has_row(W21A_VM_C));
    assert!(!java_events_admitted()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .contains_key(&W21A_VM_C));
}

/// gc-common w29-a (`common-w28b-remaining-identity-hash-keyed-side-tables`
/// rank 34): a Thread that shares its identity hash with an excluded one
/// is not excluded, and a dead Thread's exclusion goes with its lock key.
/// Fake Threads 4 GiB apart share the mock's hash (the address truncated
/// to `i32`); these paths never dereference them.
#[test]
fn w29a_a_colliding_thread_is_not_excluded() {
    crate::install_hooks();
    const VM: usize = 0x29a0_F008;
    struct Forget;
    impl Drop for Forget {
        fn drop(&mut self) {
            cratonvm_native_builtins::forget_vm_lock_keys(VM);
            forget_vm_jfr_state(VM);
        }
    }
    let _forget = Forget;
    let ctx = mock_ctx();
    ctx.set_vm_identity(VM);
    // SAFETY: keys only, never dereferenced.
    let (thread_a, thread_b) = unsafe {
        (
            ObjectRef::from_raw(0x1_29a0_f008usize as *mut u8),
            ObjectRef::from_raw(0x2_29a0_f008usize as *mut u8),
        )
    };
    assert_eq!(
        ctx.identity_hash_code(thread_a),
        ctx.identity_hash_code(thread_b),
        "premise: one identity hash"
    );
    jfr_exclude_thread(&ctx, thread_a).expect("exclude");
    assert!(jfr_thread_is_excluded(&ctx, thread_a));
    assert!(
        !jfr_thread_is_excluded(&ctx, thread_b),
        "a colliding thread reported isExcluded()"
    );
    jfr_include_thread(&ctx, thread_b);
    assert!(
        jfr_thread_is_excluded(&ctx, thread_a),
        "B's include removed A's exclusion"
    );

    // Thread A dies: its exclusion goes with its key, and a new Thread at
    // its address is not excluded.
    let a_addr = thread_a.as_ptr() as usize;
    assert_eq!(
        cratonvm_native_builtins::gc_sweep_lock_keys(VM, &|addr: usize| addr != a_addr),
        1
    );
    assert!(!excluded_threads_lock().values().any(|&vm| vm == VM));
    assert!(!jfr_thread_is_excluded(&ctx, thread_a));
}
