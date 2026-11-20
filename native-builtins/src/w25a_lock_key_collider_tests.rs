use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::NativeHeapAccess as _;

fn obj(addr: usize) -> ObjectRef {
    // SAFETY: a key only; the registry and the mock hash the address and
    // never dereference it.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn ctx_for(vm: usize) -> crate::test_utils::MockNativeContext {
    let c = crate::test_utils::mock_ctx();
    c.set_vm_identity(vm);
    c
}

fn tombstones_of(vm: usize) -> usize {
    lock_key_registry()
        .lock()
        .tombstones
        .values()
        .filter(|owner| **owner == vm)
        .count()
}

fn same_nonzero_hash(c: &crate::test_utils::MockNativeContext, a: usize, b: usize) {
    let (ha, hb) = (c.identity_hash_code(obj(a)), c.identity_hash_code(obj(b)));
    assert_eq!(ha, hb, "premise: the two objects share an identity hash");
    assert_ne!(ha, 0, "premise: a real (assigned) identity hash");
}

/// The page's retire test, weak form: two live `StampedLock`s of one VM
/// with one identity hash get two keys, so one's write lock does not
/// exclude the other. The old "lone occupant" step answered the first
/// lock's key for the second.
#[test]
fn two_live_weak_colliders_get_distinct_keys() {
    const VM: usize = 0xC250_0001;
    let c = ctx_for(VM);
    let (a, b) = (0x1_C251_1000usize, 0x2_C251_1000usize);
    same_nonzero_hash(&c, a, b);

    let ka = gc_stable_weak_lock_key(&c, obj(a)).unwrap();
    assert_eq!(
        existing_weak_lock_key(&c, obj(b)),
        None,
        "a lookup of the never-keyed collider found the other's key"
    );
    let kb = gc_stable_weak_lock_key(&c, obj(b)).unwrap();
    assert_ne!(ka, kb, "two live colliders share one key");
    assert_eq!(gc_stable_weak_lock_key(&c, obj(a)).unwrap(), ka);
    assert_eq!(existing_weak_lock_key(&c, obj(a)), Some(ka));
    assert_eq!(existing_weak_lock_key(&c, obj(b)), Some(kb));

    crate::stamped_lock::stamped_init(ka);
    crate::stamped_lock::stamped_init(kb);
    let sa = crate::stamped_lock::stamped_try_write_lock(ka);
    assert_ne!(sa, 0);
    let sb = crate::stamped_lock::stamped_try_write_lock(kb);
    assert_ne!(sb, 0, "the other lock's write hold excluded this one");
    assert!(crate::stamped_lock::stamped_unlock_write(ka, sa));
    assert!(crate::stamped_lock::stamped_unlock_write(kb, sb));

    // Each follows its own move, and neither is judged at the other's
    // address.
    let (a2, b2) = (0x3_C251_1000usize, 0x4_C251_1000usize);
    let mut moved = cratonvm_types::PointerMap::default();
    moved.insert(a, a2);
    moved.insert(b, b2);
    assert_eq!(
        gc_sweep_and_remap_lock_keys(VM, &|x| moved.contains_key(&x), &moved),
        0
    );
    assert_eq!(gc_stable_weak_lock_key(&c, obj(a2)).unwrap(), ka);
    assert_eq!(gc_stable_weak_lock_key(&c, obj(b2)).unwrap(), kb);

    // One dies: its key and lock state go, the other keeps both.
    assert_eq!(gc_sweep_lock_keys(VM, &|x| x == a2), 1);
    assert_eq!(lock_key_owner_vm(kb as u64), None);
    assert!(!crate::stamped_lock::stamped_has_slot(kb));
    assert_eq!(lock_key_owner_vm(ka as u64), Some(VM));
    assert!(crate::stamped_lock::stamped_has_slot(ka));
    assert_eq!(existing_weak_lock_key(&c, obj(a2)), Some(ka));
    forget_vm_lock_keys(VM);
    assert!(!crate::stamped_lock::stamped_has_slot(ka));
}

/// The page's retire test, held form (the `SSLContext` keys): two live
/// colliders get two keys and two owners' tombstones, and a weak re-key
/// answers each its own.
#[test]
fn two_live_held_colliders_get_distinct_keys() {
    const VM: usize = 0xC250_0002;
    let mut c = ctx_for(VM);
    let (a, b) = (0x1_C252_2000usize, 0x2_C252_2000usize);
    same_nonzero_hash(&c, a, b);

    let ka = gc_stable_lock_key(&mut c, obj(a)).unwrap();
    let kb = gc_stable_lock_key(&mut c, obj(b)).unwrap();
    assert_ne!(ka, kb, "two live colliders share one key");
    assert_eq!(gc_stable_lock_key(&mut c, obj(a)).unwrap(), ka);
    assert_eq!(gc_stable_weak_lock_key(&c, obj(b)).unwrap(), kb);

    // `b` dies: its key becomes a tombstone of its own, `a` keeps its key.
    assert_eq!(gc_sweep_lock_keys(VM, &|x| x == a), 0, "held: tombstoned");
    assert_eq!(tombstones_of(VM), 1);
    assert_eq!(lock_key_owner_vm(kb as u64), Some(VM));
    assert_eq!(gc_stable_lock_key(&mut c, obj(a)).unwrap(), ka);
    release_lock_key(kb as u64);
    assert_eq!(lock_key_owner_vm(kb as u64), None);
    assert_eq!(lock_key_owner_vm(ka as u64), Some(VM));
    forget_vm_lock_keys(VM);
    assert_eq!(lock_key_owner_vm(ka as u64), None);
    assert_eq!(tombstones_of(VM), 0);
}

/// The page's second scenario: an object dies and, before the sweep, a
/// newcomer with its hash is keyed. The newcomer used to take the dead
/// object's key, which that sweep then freed, lock state and all. Now it
/// has a key of its own, and the sweep frees only the dead one's.
#[test]
fn a_newcomer_keyed_before_the_sweep_keeps_its_own_state() {
    const VM: usize = 0xC250_0003;
    let c = ctx_for(VM);
    let (dead, newcomer) = (0x1_C253_3000usize, 0x2_C253_3000usize);
    same_nonzero_hash(&c, dead, newcomer);

    let kd = gc_stable_weak_lock_key(&c, obj(dead)).unwrap();
    crate::stamped_lock::stamped_init(kd);
    // `dead` is unreachable from here on; the sweep has not run yet.
    let kn = gc_stable_weak_lock_key(&c, obj(newcomer)).unwrap();
    assert_ne!(kn, kd, "the newcomer inherited the dead object's key");
    crate::stamped_lock::stamped_init(kn);
    let sn = crate::stamped_lock::stamped_try_write_lock(kn);
    assert_ne!(sn, 0);

    assert_eq!(
        gc_sweep_lock_keys(VM, &|x| x == newcomer),
        1,
        "only the dead object's slot goes"
    );
    assert!(!crate::stamped_lock::stamped_has_slot(kd));
    assert!(
        crate::stamped_lock::stamped_has_slot(kn),
        "the sweep freed the newcomer's lock state"
    );
    assert!(crate::stamped_lock::stamped_is_write_locked(kn));
    assert_eq!(gc_stable_weak_lock_key(&c, obj(newcomer)).unwrap(), kn);
    assert_eq!(lock_key_owner_vm(kn as u64), Some(VM));
    assert!(crate::stamped_lock::stamped_unlock_write(kn, sn));
    forget_vm_lock_keys(VM);
}
