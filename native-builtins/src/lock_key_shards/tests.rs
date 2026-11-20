use super::*;
use cratonvm_types::ObjectRef;

/// Force the sharded mapping on this test thread for the guard's life.
struct Forced;
impl Forced {
    fn on() -> Self {
        FORCE_SHARDED.with(|c| c.set(Some(true)));
        Forced
    }
}
impl Drop for Forced {
    fn drop(&mut self) {
        FORCE_SHARDED.with(|c| c.set(None));
    }
}

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

/// VM `vm`'s live slots over every shard.
fn slots_of(vm: usize) -> usize {
    all_shards()
        .iter()
        .map(|s| {
            s.lock()
                .buckets
                .values()
                .map(|b| b.iter().filter(|e| e.vm == vm).count())
                .sum::<usize>()
        })
        .sum()
}

#[test]
fn gcg_k3_the_shard_map_is_total_stable_and_spreads_sequential_hashes() {
    let mut per_shard = [0usize; LOCK_KEY_SHARDS];
    for hash in 0u32..(LOCK_KEY_SHARDS as u32 * 16) {
        let i = shard_index(hash, true);
        assert!(i < LOCK_KEY_SHARDS);
        assert_eq!(i, shard_index(hash, true), "a hash keeps its shard");
        assert_eq!(shard_index(hash, false), 0, "unsharded: all in shard 0");
        per_shard[i] += 1;
    }
    assert!(per_shard.iter().all(|&n| n == 16), "{per_shard:?}");
    assert_eq!(shard_index(u32::MAX, true), LOCK_KEY_SHARDS - 1);
}

/// End to end through `lib.rs`'s registry functions with the sharded map
/// forced on this thread: keys of hashes in different shards and of two
/// colliders in one shard are all distinct, each is found again, owned by
/// its VM, and swept / forgotten from its own shard.
#[test]
fn gcg_k3_sharded_lock_keys_are_unique_found_swept_and_forgotten() {
    const VM: usize = 0xC3_0000_0001;
    let _forced = Forced::on();
    assert_eq!(lock_key_shards_in_use().len(), LOCK_KEY_SHARDS);
    let c = ctx_for(VM);
    // The mock's hash is the address truncated to i32: 8 bytes apart =
    // 8 shards apart; 4 GiB apart = the same hash (a collider, same shard).
    let base = 0x1_C3A0_0000usize;
    let addrs: Vec<usize> = (0..8)
        .map(|i| base + i * 8)
        .chain([base + (1usize << 32)])
        .collect();
    let keys: Vec<usize> = addrs
        .iter()
        .map(|&a| crate::gc_stable_weak_lock_key(&c, obj(a)).unwrap())
        .collect();
    let mut distinct = keys.clone();
    distinct.sort_unstable();
    distinct.dedup();
    assert_eq!(distinct.len(), keys.len(), "every key distinct: {keys:x?}");
    // addr-keyed: not an address -- shard indices of the test's keys.
    let shards: std::collections::HashSet<usize> = keys
        .iter()
        .map(|&k| lock_key_shard_of((k >> 32) as u32))
        .collect();
    assert!(shards.len() > 1, "the keys really spread over shards");
    for (&a, &k) in addrs.iter().zip(&keys) {
        assert_eq!(crate::existing_weak_lock_key(&c, obj(a)), Some(k));
        assert_eq!(crate::gc_stable_weak_lock_key(&c, obj(a)).unwrap(), k);
        assert_eq!(crate::lock_key_owner_vm(k as u64), Some(VM));
    }
    assert_eq!(slots_of(VM), addrs.len());
    // A sweep that keeps only the first object drops the others from
    // whichever shard holds them.
    let keep = addrs[0];
    assert_eq!(
        crate::gc_sweep_lock_keys(VM, &|a| a == keep),
        addrs.len() - 1
    );
    assert_eq!(slots_of(VM), 1);
    for &k in &keys[1..] {
        assert_eq!(crate::lock_key_owner_vm(k as u64), None);
    }
    // A reborn object at a swept address gets a key never handed out.
    let reborn = crate::gc_stable_weak_lock_key(&c, obj(addrs[1])).unwrap();
    assert!(!keys.contains(&reborn));
    crate::forget_vm_lock_keys(VM);
    assert_eq!(slots_of(VM), 0);
    assert_eq!(crate::lock_key_owner_vm(keys[0] as u64), None);
}
