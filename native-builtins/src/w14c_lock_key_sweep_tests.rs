use super::*;

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

fn slots_of(vm: usize) -> usize {
    lock_key_registry()
        .lock()
        .buckets
        .values()
        .map(|b| b.iter().filter(|s| s.vm == vm).count())
        .sum()
}

fn tombstones_of(vm: usize) -> usize {
    lock_key_registry()
        .lock()
        .tombstones
        .values()
        .filter(|owner| **owner == vm)
        .count()
}

#[test]
fn dead_weak_slots_are_dropped_and_their_keys_never_reminted() {
    const VM: usize = 0xC140_0001;
    let c = ctx_for(VM);
    let dead: Vec<usize> = (0..8).map(|i| 0x1_C141_0000 + i * 0x40).collect();
    let live = 0x1_C141_1000usize;
    let dead_keys: Vec<usize> = dead
        .iter()
        .map(|&a| gc_stable_weak_lock_key(&c, obj(a)).unwrap())
        .collect();
    let live_key = gc_stable_weak_lock_key(&c, obj(live)).unwrap();
    assert_eq!(slots_of(VM), 9);

    assert_eq!(gc_sweep_lock_keys(VM, &|a| a == live), 8);
    assert_eq!(slots_of(VM), 1, "only the survivor keeps a slot");
    for k in &dead_keys {
        assert_eq!(lock_key_owner_vm(*k as u64), None);
    }
    assert_eq!(lock_key_owner_vm(live_key as u64), Some(VM));
    assert_eq!(gc_stable_weak_lock_key(&c, obj(live)).unwrap(), live_key);
    // A new object on a dead one's address (same mock hash) gets a new key.
    let reborn = gc_stable_weak_lock_key(&c, obj(dead[0])).unwrap();
    assert!(!dead_keys.contains(&reborn));
    assert_ne!(reborn, live_key);
    forget_vm_lock_keys(VM);
    assert_eq!(slots_of(VM), 0);
}

/// gc-common w14-c handoff part 3: a dead weak key's `StampedLock` state
/// goes with its slot; a live one's stays.
#[test]
fn a_dead_weak_keys_stamped_state_goes_with_it() {
    const VM: usize = 0xC140_00A1;
    let c = ctx_for(VM);
    let (dead, live) = (0x1_C14A_0000usize, 0x1_C14A_1000usize);
    let dk = gc_stable_weak_lock_key(&c, obj(dead)).unwrap();
    let lk = gc_stable_weak_lock_key(&c, obj(live)).unwrap();
    crate::stamped_lock::stamped_init(dk);
    crate::stamped_lock::stamped_init(lk);
    assert!(crate::stamped_lock::stamped_has_slot(dk));
    assert_eq!(gc_sweep_lock_keys(VM, &|a| a == live), 1);
    assert!(
        !crate::stamped_lock::stamped_has_slot(dk),
        "dead lock's state survived"
    );
    assert!(!crate::stamped_lock::stamped_is_locked(dk));
    assert!(
        crate::stamped_lock::stamped_has_slot(lk),
        "live lock's state dropped"
    );
    forget_vm_lock_keys(VM);
    assert!(
        !crate::stamped_lock::stamped_has_slot(lk),
        "teardown kept the state"
    );
}

/// The page's scenario 2: a dead slot no longer sits in the live object's
/// bucket, and a moved survivor is found at its NEW address (step 1).
#[test]
fn a_live_key_survives_a_move_next_to_a_dead_collider() {
    const VM: usize = 0xC140_0002;
    let c = ctx_for(VM);
    let d = 0x1_C142_1000usize;
    let l_old = 0x2_C142_1000usize;
    let l_new = 0x5_C142_1000usize;
    let kd = gc_stable_weak_lock_key(&c, obj(d)).unwrap();
    assert_eq!(gc_sweep_lock_keys(VM, &|_| false), 1);
    let kl = gc_stable_weak_lock_key(&c, obj(l_old)).unwrap();
    assert_ne!(
        kl, kd,
        "the dead collider's key is not handed to the live object"
    );

    let mut moved = cratonvm_types::PointerMap::default();
    moved.insert(l_old, l_new);
    assert_eq!(
        gc_sweep_and_remap_lock_keys(VM, &|a| moved.contains_key(&a), &moved),
        0
    );
    assert_eq!(gc_stable_weak_lock_key(&c, obj(l_new)).unwrap(), kl);
    assert_eq!(slots_of(VM), 1);
    // Judged at its new address by the next collection.
    assert_eq!(gc_sweep_lock_keys(VM, &|a| a == l_new), 0);
    assert_eq!(lock_key_owner_vm(kl as u64), Some(VM));
    forget_vm_lock_keys(VM);
}

/// Hash 0 never takes the "lone occupant moved" rebind, so before the
/// remap a moved hash-0 object lost its key (and its lock state).
#[test]
fn a_hash_zero_object_keeps_its_key_across_a_move() {
    const VM: usize = 0xC140_0003;
    let c = ctx_for(VM);
    let old = 0x7_0000_0000usize;
    let new = 0x9_0000_0000usize;
    let k = gc_stable_weak_lock_key(&c, obj(old)).unwrap();
    let mut moved = cratonvm_types::PointerMap::default();
    moved.insert(old, new);
    gc_sweep_and_remap_lock_keys(VM, &|a| moved.contains_key(&a), &moved);
    assert_eq!(gc_stable_weak_lock_key(&c, obj(new)).unwrap(), k);
    forget_vm_lock_keys(VM);
}

/// A held key (the `SSLContext` rows' key) is never reused while a table
/// may still name it: its dead object leaves a tombstone that keeps the
/// owner and is never rebound, until `release_lock_key`.
#[test]
fn a_held_key_keeps_its_owner_and_is_not_reused_until_released() {
    const VM: usize = 0xC140_0004;
    let mut c = ctx_for(VM);
    let ctx_obj = 0x3_C143_2000usize;
    let other = 0x4_C143_2000usize;
    let kc = gc_stable_lock_key(&mut c, obj(ctx_obj)).unwrap();
    assert_eq!(
        gc_sweep_lock_keys(VM, &|_| false),
        0,
        "tombstoned, not dropped"
    );
    assert_eq!(lock_key_owner_vm(kc as u64), Some(VM));
    assert_eq!((slots_of(VM), tombstones_of(VM)), (0, 1));

    // Same hash, same VM: the dead context's slot was this object's lone
    // occupant, which the old step 2 used to rebind (w25-a removed that
    // step); a tombstone is never rebound either way.
    let ko = gc_stable_lock_key(&mut c, obj(other)).unwrap();
    assert_ne!(ko, kc);
    // Re-sweeping never re-judges the tombstone.
    assert_eq!(gc_sweep_lock_keys(VM, &|a| a == other), 0);
    assert_eq!(lock_key_owner_vm(kc as u64), Some(VM));

    release_lock_key(kc as u64);
    assert_eq!(lock_key_owner_vm(kc as u64), None);
    release_lock_key(kc as u64);

    // Released while alive: its death drops the slot outright.
    release_lock_key(ko as u64);
    assert_eq!(lock_key_owner_vm(ko as u64), Some(VM));
    assert_eq!(gc_sweep_lock_keys(VM, &|_| false), 1);
    assert_eq!(lock_key_owner_vm(ko as u64), None);
    assert_eq!((slots_of(VM), tombstones_of(VM)), (0, 0));
    // A weak re-key of a held slot keeps it held.
    let kh = gc_stable_lock_key(&mut c, obj(ctx_obj)).unwrap();
    assert_eq!(gc_stable_weak_lock_key(&c, obj(ctx_obj)).unwrap(), kh);
    gc_sweep_lock_keys(VM, &|_| false);
    assert_eq!(lock_key_owner_vm(kh as u64), Some(VM));
    forget_vm_lock_keys(VM);
    assert_eq!(lock_key_owner_vm(kh as u64), None);
    assert_eq!(tombstones_of(VM), 0);
}

/// A lookup through a STALE copy of a moved object's address (same
/// identity hash) must not cost the live object its key: the next sweep
/// still judges the object at its real address. The old "lone occupant
/// moved" rebind pointed the slot at the vacated address, the sweep found
/// that dead, and the live object lost its key.
///
/// Premise changed by gc-common w25-a
/// (`common-w24a-lock-key-registry-merges-live-identity-hash-colliders`):
/// the stale lookup used to be answered the moved object's key (the
/// "lone occupant" step, which could not tell a stale copy from a live
/// collider). It now gets a key of its own at the vacated address -- a
/// miss -- which the next sweep retires (a held key: into a tombstone,
/// until `release_lock_key`). The live object's key is untouched.
#[test]
fn a_stale_address_lookup_does_not_cost_the_live_object_its_key() {
    const VM: usize = 0xC140_0005;
    let mut c = ctx_for(VM);
    let stale = 0x1_C144_3000usize;
    let current = 0x2_C144_3000usize;
    let k = gc_stable_lock_key(&mut c, obj(stale)).unwrap();
    let mut moved = cratonvm_types::PointerMap::default();
    moved.insert(stale, current);
    gc_sweep_and_remap_lock_keys(VM, &|a| moved.contains_key(&a), &moved);

    let ks = gc_stable_lock_key(&mut c, obj(stale)).unwrap();
    assert_ne!(ks, k, "a stale copy is not answered the moved object's key");
    assert_eq!(
        gc_sweep_lock_keys(VM, &|a| a == current),
        0,
        "the stale lookup's held slot is tombstoned, not dropped"
    );
    assert_eq!(gc_stable_lock_key(&mut c, obj(current)).unwrap(), k);
    assert_eq!(
        lock_key_registry()
            .lock()
            .buckets
            .values()
            .flatten()
            .filter(|s| s.vm == VM && s.last_ptr == current)
            .count(),
        1,
        "still live at its real address, not a tombstone"
    );
    assert_eq!(lock_key_owner_vm(k as u64), Some(VM));
    assert_eq!(tombstones_of(VM), 1, "only the stale lookup's own key");
    release_lock_key(ks as u64);
    assert_eq!(tombstones_of(VM), 0);
    forget_vm_lock_keys(VM);
}

/// An out-of-pause sweep judges copied addresses with the registry lock
/// released (the predicate may take the old-gen lock, and `t27_tls` asks
/// the registry under its own locks); a slot minted while it judges is
/// left for the next reclamation.
#[test]
fn the_predicate_runs_without_the_registry_lock() {
    const VM: usize = 0xC140_0006;
    let c = std::cell::RefCell::new(ctx_for(VM));
    let dying = 0x1_C145_4000usize;
    let minted = 0x1_C145_5000usize;
    let k = gc_stable_weak_lock_key(&*c.borrow(), obj(dying)).unwrap();
    let fresh = std::cell::Cell::new(0usize);
    let dropped = gc_sweep_lock_keys(VM, &|a| {
        // Would deadlock if the sweep held the registry lock here.
        assert_eq!(lock_key_owner_vm(k as u64), Some(VM));
        if a == dying {
            fresh.set(gc_stable_weak_lock_key(&*c.borrow(), obj(minted)).unwrap());
        }
        false
    });
    assert_eq!(dropped, 1);
    assert_eq!(lock_key_owner_vm(k as u64), None);
    assert_eq!(lock_key_owner_vm(fresh.get() as u64), Some(VM));
    forget_vm_lock_keys(VM);
}
