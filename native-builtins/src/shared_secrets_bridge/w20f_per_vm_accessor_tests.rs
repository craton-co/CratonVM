//! gc-common w20-f: the `SharedSecrets` accessor memo is keyed by VM, so a
//! handle published by one VM (an index into ITS global-ref table) is never
//! resolved by another, and the rows go with the VM.
use super::*;
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

// Private VM identities: no other test publishes under them.
const W20F_VM_A: usize = 0x0F20_C008;
const W20F_VM_B: usize = 0x0F20_D008;
const W20F_OWNER: &str = "w20f/test/AccessOwner";

#[test]
fn w20f_accessor_rows_are_per_vm_and_forgotten_with_the_vm() {
    let mut a = mock_ctx();
    a.set_vm_identity(W20F_VM_A);
    let b = mock_ctx();
    b.set_vm_identity(W20F_VM_B);
    let obj = a.fresh_object_ref();

    assert_eq!(publish_owner_singleton(&mut a, W20F_OWNER, obj), obj);
    assert!(published_owner_singleton(&a, W20F_OWNER).is_some());
    assert_eq!(
        published_owner_singleton(&b, W20F_OWNER),
        None,
        "another VM must not see VM A's handle"
    );
    let other = a.fresh_object_ref();
    assert_eq!(
        publish_owner_singleton(&mut a, W20F_OWNER, other),
        obj,
        "a second publish in the same VM answers the first object"
    );

    forget_vm_shared_secrets_singletons(W20F_VM_A);
    assert_eq!(published_owner_singleton(&a, W20F_OWNER), None);
    forget_vm_shared_secrets_singletons(W20F_VM_A);
}

#[test]
fn w20f_carrier_thread_local_rows_are_keyed_by_vm_and_forgotten_with_it() {
    const W20F_VM_C: usize = 0x0F20_E008;
    let mut a = mock_ctx();
    a.set_vm_identity(W20F_VM_C);
    if a.is_jdk_only() {
        return; // the map is `--compatible`-only
    }
    let tl = a.fresh_object_ref();
    let value = a.fresh_object_ref();
    let set = jla_set_carrier_thread_local(
        &mut a,
        &[
            Value::Object(None),
            Value::Object(Some(tl)),
            Value::Object(Some(value)),
        ],
    );
    assert!(matches!(set, Ok(None)), "{set:?}");
    let key = crate::existing_weak_lock_key(&a, tl).expect("the set minted a key");
    assert!(
        carrier_thread_locals()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .rows
            .get(&key)
            .is_some_and(|row| row.vm == W20F_VM_C),
        "the row is filed under its VM"
    );
    let present = jla_is_carrier_thread_local_present(
        &mut a,
        &[Value::Object(None), Value::Object(Some(tl))],
    );
    assert!(matches!(present, Ok(Some(Value::Int(1)))), "{present:?}");

    forget_vm_shared_secrets_singletons(W20F_VM_C);
    let present = jla_is_carrier_thread_local_present(
        &mut a,
        &[Value::Object(None), Value::Object(Some(tl))],
    );
    assert!(matches!(present, Ok(Some(Value::Int(0)))), "{present:?}");
    crate::forget_vm_lock_keys(W20F_VM_C);
}

/// gc-common w29-a (`common-w28b-remaining-identity-hash-keyed-side-tables`
/// rank 12): two ThreadLocals of ONE VM with one identity hash keep their
/// own carrier values, and a dead one's row -- and its value's root -- goes
/// with its lock key. The mock's identity hash is the address truncated to
/// `i32`, so two fake ThreadLocals 4 GiB apart share one; these paths
/// never dereference them.
#[test]
fn w29a_colliding_carrier_thread_locals_keep_their_own_rows() {
    const VM: usize = 0x29a0_C008;
    struct Forget;
    impl Drop for Forget {
        fn drop(&mut self) {
            crate::forget_vm_lock_keys(VM);
            forget_vm_shared_secrets_singletons(VM);
        }
    }
    let _forget = Forget;
    let mut a = mock_ctx();
    a.set_vm_identity(VM);
    if a.is_jdk_only() {
        return; // the map is `--compatible`-only
    }
    // SAFETY: keys only, never dereferenced.
    let (tl_a, tl_b) = unsafe {
        (
            ObjectRef::from_raw(0x1_29a0_c008usize as *mut u8),
            ObjectRef::from_raw(0x2_29a0_c008usize as *mut u8),
        )
    };
    assert_eq!(
        a.identity_hash_code(tl_a),
        a.identity_hash_code(tl_b),
        "premise: one identity hash"
    );
    let (va, vb) = (a.fresh_object_ref(), a.fresh_object_ref());
    let before = a.global_root_count();
    for (tl, v) in [(tl_a, va), (tl_b, vb)] {
        let set = jla_set_carrier_thread_local(
            &mut a,
            &[
                Value::Object(None),
                Value::Object(Some(tl)),
                Value::Object(Some(v)),
            ],
        );
        assert!(matches!(set, Ok(None)), "{set:?}");
    }
    for (tl, v) in [(tl_a, va), (tl_b, vb)] {
        let got = jla_get_carrier_thread_local(
            &mut a,
            &[Value::Object(None), Value::Object(Some(tl))],
        );
        assert!(
            matches!(got, Ok(Some(Value::Object(Some(o)))) if o == v),
            "a colliding ThreadLocal's carrier value was served: {got:?}"
        );
    }
    // A dies: its key is freed, its row goes and its value's root is
    // released by the VM's next carrier-thread-local store.
    let a_addr = tl_a.as_ptr() as usize;
    assert_eq!(
        crate::gc_sweep_lock_keys(VM, &|addr: usize| addr != a_addr),
        1
    );
    let present = jla_is_carrier_thread_local_present(
        &mut a,
        &[Value::Object(None), Value::Object(Some(tl_b))],
    );
    assert!(
        matches!(present, Ok(Some(Value::Int(1)))),
        "B's row survives A's death"
    );
    let set = jla_set_carrier_thread_local(
        &mut a,
        &[
            Value::Object(None),
            Value::Object(Some(tl_b)),
            Value::Object(Some(vb)),
        ],
    );
    assert!(matches!(set, Ok(None)), "{set:?}");
    assert_eq!(
        a.global_root_count(),
        before + 1,
        "only B's value is still rooted"
    );
    let present = jla_is_carrier_thread_local_present(
        &mut a,
        &[Value::Object(None), Value::Object(Some(tl_a))],
    );
    assert!(
        matches!(present, Ok(Some(Value::Int(0)))),
        "a new ThreadLocal at A's address inherited A's row"
    );
}
