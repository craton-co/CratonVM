use super::*;

const VM_A: usize = 0x9B00_0601;
const VM_B: usize = 0x9B00_0602;

/// The page's `StampedLock` scenario: two VMs' locks with the same
/// identity hash. The mock's identity hash is the address truncated to
/// `i32`, so two addresses 4 GiB apart share one; the old "lone occupant
/// moved" rebind handed VM B's lock VM A's key. Never dereferenced.
#[test]
fn two_vms_locks_with_one_identity_hash_get_distinct_keys() {
    let mut a = crate::test_utils::mock_ctx();
    a.set_vm_identity(VM_A);
    let mut b = crate::test_utils::mock_ctx();
    b.set_vm_identity(VM_B);
    let in_a = unsafe { ObjectRef::from_raw(0x1_9B00_3000usize as *mut u8) };
    let in_b = unsafe { ObjectRef::from_raw(0x2_9B00_3000usize as *mut u8) };

    let ka = gc_stable_lock_key(&mut a, in_a).unwrap();
    let kb = gc_stable_lock_key(&mut b, in_b).unwrap();
    assert_ne!(ka, kb, "VM B's lock must not share VM A's lock state");
    assert_eq!(gc_stable_lock_key(&mut a, in_a).unwrap(), ka);
    assert_eq!(lock_key_owner_vm(ka as u64), Some(VM_A));
    assert_eq!(lock_key_owner_vm(kb as u64), Some(VM_B));

    // A relocation inside one VM keeps its key: the epilogue re-addresses
    // the slot (gc-common w25-a: a lookup at another address without that
    // remap is another object, so this no longer skips it), and VM B's
    // slot is neither judged nor moved by VM A's collection.
    let moved = unsafe { ObjectRef::from_raw(0x3_9B00_3000usize as *mut u8) };
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(in_a.as_ptr() as usize, moved.as_ptr() as usize);
    assert_eq!(
        gc_sweep_and_remap_lock_keys(VM_A, &|x| map.contains_key(&x), &map),
        0
    );
    assert_eq!(gc_stable_lock_key(&mut a, moved).unwrap(), ka);
    assert_eq!(gc_stable_lock_key(&mut b, in_b).unwrap(), kb);

    forget_vm_lock_keys(VM_A);
    assert_eq!(lock_key_owner_vm(ka as u64), None);
    assert_eq!(lock_key_owner_vm(kb as u64), Some(VM_B));
    // Never re-minted: a later VM's lock with the same hash gets a fresh
    // generation, so it cannot inherit state `stamped_lock` still holds.
    let mut c = crate::test_utils::mock_ctx();
    c.set_vm_identity(VM_A + 16);
    let kc = gc_stable_lock_key(&mut c, in_a).unwrap();
    assert_ne!(kc, ka);
    assert_ne!(kc, kb);
    forget_vm_lock_keys(VM_B);
    forget_vm_lock_keys(VM_A + 16);
}

/// `common-w8a-process-global-object-singletons-in-native-builtins`: the
/// async worker pool is one per VM, not one per process.
#[test]
fn async_worker_pool_slot_is_per_vm() {
    // gc-common w25-a: identities of its own -- the lock-key test above
    // tears down VM_A / VM_B, and the two run in parallel.
    const POOL_VM_A: usize = 0x9B00_0621;
    const POOL_VM_B: usize = 0x9B00_0622;
    let mut a = crate::test_utils::mock_ctx();
    a.set_vm_identity(POOL_VM_A);
    let mut b = crate::test_utils::mock_ctx();
    b.set_vm_identity(POOL_VM_B);
    // SAFETY: a cached slot value only; the mock's var-handle registry is
    // empty, so the getter answers the cached ref without reading it.
    let pool = unsafe { ObjectRef::from_raw(0x9B00_5000usize as *mut u8) };
    ASYNC_POOL.with(POOL_VM_A, |slot| *slot = Some((11, pool)));
    assert_eq!(async_worker_pool_existing(&mut a), Some(pool));
    assert_eq!(
        async_worker_pool_existing(&mut b),
        None,
        "VM B must not be handed VM A's pool"
    );
    forget_vm_async_worker_pool(POOL_VM_A);
    assert_eq!(async_worker_pool_existing(&mut a), None);
}
