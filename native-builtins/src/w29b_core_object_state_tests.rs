use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{NativeHeapAccess as _, NativeSystemAccess as _};

fn obj(addr: usize) -> ObjectRef {
    // SAFETY: a key only, never dereferenced by the code under test.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn ctx_for(vm: usize) -> crate::test_utils::MockNativeContext {
    let c = crate::test_utils::mock_ctx();
    c.set_vm_identity(vm);
    c
}

struct ForgetVm(usize);
impl Drop for ForgetVm {
    fn drop(&mut self) {
        forget_vm_lock_keys(self.0);
        forget_vm_unsafe_side_stores(self.0);
        forget_vm_lock_state(self.0);
    }
}

fn synthetic_rows_of(vm: usize) -> usize {
    synthetic_field_store()
        .lock()
        .keys()
        .filter(|&&(owner, _, _)| owner == vm)
        .count()
}

/// Rank 5: two live receivers with one hash each keep their own synthetic
/// slot (a CAS on one no longer compares against the other's value), and a
/// dead receiver's slot -- a GC root while it lasts -- goes with its key.
#[test]
fn synthetic_slots_of_hash_colliders_stay_apart_and_die_with_their_receiver() {
    const VM: usize = 0xB29_2001;
    let _g = ForgetVm(VM);
    let mut c = ctx_for(VM);
    // A private synthetic offset: no static-field registration names it,
    // so the accessors go straight to the per-receiver store.
    let off = synthetic_offset_for("w29b/SyntheticSlotProbe", "slot");
    let (a, b) = (obj(0x1_B29C_0000), obj(0x2_B29C_0000));
    assert_eq!(c.identity_hash_code(a), c.identity_hash_code(b), "premise");

    assert!(synthetic_cas(
        &mut c,
        a,
        off,
        Value::Object(None),
        Value::Int(1)
    ));
    assert!(
        matches!(synthetic_get(&mut c, b, off), Value::Object(None)),
        "b read a's slot"
    );
    assert!(
        synthetic_cas(&mut c, b, off, Value::Object(None), Value::Int(2)),
        "b's CAS compared against a's value"
    );
    assert!(matches!(synthetic_get(&mut c, a, off), Value::Int(1)));
    assert!(matches!(synthetic_get(&mut c, b, off), Value::Int(2)));
    assert_eq!(synthetic_rows_of(VM), 2);

    let a_addr = a.as_ptr() as usize;
    assert_eq!(gc_sweep_lock_keys(VM, &|x| x == a_addr), 1);
    assert_eq!(
        synthetic_rows_of(VM),
        1,
        "the dead receiver's slot survived"
    );
    assert!(matches!(synthetic_get(&mut c, a, off), Value::Int(1)));
}

/// Rank 11: two live `ReentrantLock`s with one hash have separate states,
/// and a lock that dies HELD takes its row along -- a later lock at its
/// address (same hash) is not born held.
#[test]
fn reentrant_lock_states_of_hash_colliders_stay_apart_and_die_with_their_lock() {
    const VM: usize = 0xB29_2002;
    let _g = ForgetVm(VM);
    let mut c = ctx_for(VM);
    let (held, other) = (obj(0x1_B29D_0000), obj(0x2_B29D_0000));
    assert_eq!(
        c.identity_hash_code(held),
        c.identity_hash_code(other),
        "premise"
    );
    let kh = rl_key(&mut c, held);
    rl_with(kh, |st| {
        st.owner = 77;
        st.hold = 1;
    });
    assert_eq!(
        rl_get(rl_key(&mut c, other)).owner,
        RL_UNOWNED,
        "shared lock state"
    );
    assert_eq!(rl_get(kh).owner, 77);

    let other_addr = other.as_ptr() as usize;
    gc_sweep_lock_keys(VM, &|x| x == other_addr);
    assert!(
        !rl_state_table()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&kh),
        "the dead lock's held row survived"
    );
    // A new lock on the dead one's address: a fresh key, an unheld state.
    let reborn = rl_key(&mut c, held);
    assert_ne!(reborn, kh);
    assert_eq!(rl_get(reborn).owner, RL_UNOWNED, "a new lock was born held");
}

/// Rank 14: two ScopedValues with one hash push and look up different
/// `key_id`s; the id survives a move (the epilogue's remap); an unbound
/// ScopedValue's lookup mints nothing.
#[test]
fn scoped_value_key_ids_tell_hash_colliders_apart_and_survive_a_move() {
    const VM: usize = 0xB29_2003;
    let _g = ForgetVm(VM);
    let mut c = ctx_for(VM);
    let (a, b, unbound) = (0x1_B29E_0000usize, 0x2_B29E_0000usize, 0x3_B29E_0000usize);
    assert_eq!(
        c.identity_hash_code(obj(a)),
        c.identity_hash_code(obj(b)),
        "premise"
    );
    let (ka, kb) = (
        pd_scoped_value_key_id(&c, obj(a)),
        pd_scoped_value_key_id(&c, obj(b)),
    );
    assert_ne!(ka, kb, "two ScopedValues shared one binding key");
    assert_eq!(pd_scoped_value_key_id(&c, obj(a)), ka, "the key is stable");

    let moved_a = 0x7_B29E_0000usize;
    let mut moved = cratonvm_types::PointerMap::default();
    moved.insert(a, moved_a);
    gc_sweep_and_remap_lock_keys(VM, &|x| x == a || x == b, &moved);
    assert_eq!(
        pd_scoped_value_key_id(&c, obj(moved_a)),
        ka,
        "a moved binding key changed"
    );

    assert!(pd_scoped_value_binding(&mut c, obj(unbound)).is_none());
    assert_eq!(
        existing_weak_lock_key(&c, obj(unbound)),
        None,
        "an unbound ScopedValue's lookup minted a key"
    );
}

/// The legacy CountDownLatch wait key is out of the holder keys' range
/// (1..=i32::MAX), stable, and distinct per latch.
#[test]
fn legacy_latch_wait_keys_are_tagged_stable_and_distinct() {
    const VM: usize = 0xB29_2004;
    let _g = ForgetVm(VM);
    let mut c = ctx_for(VM);
    // Real mock objects: `cdl_wait_key` reads the count slot, which holds
    // the mock's default `Int(0)` -- the legacy, holder-less shape.
    let (l1, l2) = (c.fresh_object_ref(), c.fresh_object_ref());
    let (k1, k2) = (cdl_wait_key(&mut c, l1), cdl_wait_key(&mut c, l2));
    assert!(
        k1 > i32::MAX as u64 && k2 > i32::MAX as u64,
        "in the holder-key range"
    );
    assert_ne!(k1, k2);
    assert_eq!(cdl_wait_key(&mut c, l1), k1);
}

/// The async worker pool is re-read through its global root, not a
/// var-handle root looked up by identity hash: the stored raw copy is
/// only the fallback.
#[test]
fn the_async_pool_is_reread_through_its_global_root() {
    const VM: usize = 0xB29_2005;
    let mut c = ctx_for(VM);
    let (stale, current) = (obj(0x9B29_6000), obj(0x9B29_7000));
    let handle = c.add_global_root(current);
    ASYNC_POOL.with(VM, |slot| *slot = Some((handle, stale)));
    assert_eq!(async_worker_pool_existing(&mut c), Some(current));
    forget_vm_async_worker_pool(VM);
    assert_eq!(async_worker_pool_existing(&mut c), None);
}
