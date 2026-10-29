// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! gcg g3/k (`common-w28a-proposal-shard-the-lock-key-registry-REFUTED-20261015`): the weak /
//! held lock-key registry (`LockKeyRegistry` in `lib.rs`) split into
//! [`LOCK_KEY_SHARDS`] mutexes by identity hash, behind
//! `CRATONVM_LOCK_KEY_SHARDS` (default OFF).
//!
//! # Why
//!
//! Every `gc_stable_weak_lock_key` / `existing_weak_lock_key` lookup took ONE
//! process-wide mutex, and since gc-common w28-a that includes per-call socket
//! paths (`Socket$SocketInputStream.read()I`, once per byte for a
//! byte-at-a-time reader). A server with many connection threads serialised
//! every socket read on it.
//!
//! # Why sharding by hash keeps every key unique
//!
//! A key is `(hash << 32) | generation`, and a hash lives in exactly ONE
//! shard, so each shard's own `next_generation` (the field already in
//! `LockKeyRegistry`) never re-mints a key: the "skip a generation still in
//! use" loop and the tombstone check run per hash, under the shard that holds
//! every slot and tombstone of that hash. With the switch OFF every hash maps
//! to shard 0, which is then exactly the old single registry (one counter,
//! one mutex), and the walkers visit shard 0 only.
//!
//! # Lock discipline
//!
//! Each shard is the same `LockLevel`-free leaf the single mutex was: no path
//! holds it while calling into the VM or taking another lock, and no path
//! holds two shards at once (the walkers take them one at a time; none needs a
//! consistent view across hashes). The switch is read once per process: the
//! registry is process-wide, and a slot must be found in the shard it was
//! minted in for the life of the process.

use parking_lot::Mutex;

use super::LockKeyRegistry;

/// Shards when `CRATONVM_LOCK_KEY_SHARDS` is on. A power of two. Identity
/// hashes are numbered sequentially (`lib.rs`'s registry note), so their low
/// bits spread consecutive objects round-robin over the shards.
pub(crate) const LOCK_KEY_SHARDS: usize = 64;

/// `CRATONVM_LOCK_KEY_SHARDS`, read once per process.
fn sharded() -> bool {
    if let Some(forced) = forced_sharded() {
        return forced;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let on = cratonvm_types::flags::runtime_flag_on("CRATONVM_LOCK_KEY_SHARDS");
        if on {
            // The switch's engagement control (stderr, once per process):
            // printed only when the registry is actually first used.
            eprintln!("[lock-key-shards] on shards={LOCK_KEY_SHARDS}");
        }
        on
    })
}

#[cfg(test)]
thread_local! {
    /// Test-only, per test thread: force the sharded mapping on or off for
    /// lookups made on THIS thread. Safe to mix with other tests' threads
    /// because every test keys under its own VM identity and the walkers
    /// filter by VM.
    static FORCE_SHARDED: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn forced_sharded() -> Option<bool> {
    FORCE_SHARDED.with(std::cell::Cell::get)
}

#[cfg(not(test))]
#[inline(always)]
fn forced_sharded() -> Option<bool> {
    None
}

fn all_shards() -> &'static [Mutex<LockKeyRegistry>; LOCK_KEY_SHARDS] {
    static R: std::sync::OnceLock<[Mutex<LockKeyRegistry>; LOCK_KEY_SHARDS]> =
        std::sync::OnceLock::new();
    R.get_or_init(|| std::array::from_fn(|_| parking_lot::Mutex::new(LockKeyRegistry::default())))
}

/// The shard index of `hash` among [`lock_key_shards_in_use`]: `hash`'s low
/// bits when sharded, else 0.
#[inline]
fn shard_index(hash: u32, sharded: bool) -> usize {
    if sharded {
        (hash as usize) & (LOCK_KEY_SHARDS - 1)
    } else {
        0
    }
}

/// The index of `hash`'s shard in [`lock_key_shards_in_use`].
#[inline]
pub(crate) fn lock_key_shard_of(hash: u32) -> usize {
    shard_index(hash, sharded())
}

/// The registry shard holding every slot and tombstone of `hash`.
#[inline]
pub(crate) fn lock_key_shard(hash: u32) -> &'static Mutex<LockKeyRegistry> {
    &all_shards()[lock_key_shard_of(hash)]
}

/// The shards a whole-registry walker visits, one at a time: all of them when
/// sharded, else shard 0 alone (the old single registry).
#[inline]
pub(crate) fn lock_key_shards_in_use() -> &'static [Mutex<LockKeyRegistry>] {
    let all = all_shards();
    if sharded() {
        &all[..]
    } else {
        &all[..1]
    }
}

/// Shard 0: the whole registry with the switch off. For the `lib.rs` test
/// helpers that inspect the registry directly.
#[cfg(test)]
pub(crate) fn unsharded_registry() -> &'static Mutex<LockKeyRegistry> {
    &all_shards()[0]
}

#[cfg(test)]
mod tests {
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
}
