// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
pub const LOCK_KEY_SHARDS: usize = 64;

/// `CRATONVM_LOCK_KEY_SHARDS`, read once per process.
fn sharded() -> bool {
    if let Some(forced) = forced_sharded() {
        return forced;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let on = true;
        if on && std::env::var_os("CRATONVM_LOCK_KEY_SHARDS").is_some() {
            // The switch's engagement control (stderr, once per process):
            // printed only when the registry is actually first used, and only
            // when the switch is named explicitly -- a default-on switch must
            // not write to stderr (HotSpot prints nothing; a boot image restores
            // this cell already set, so it could not repeat the line anyway).
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
pub fn lock_key_shard_of(hash: u32) -> usize {
    shard_index(hash, sharded())
}

/// The registry shard holding every slot and tombstone of `hash`.
#[inline]
pub fn lock_key_shard(hash: u32) -> &'static Mutex<LockKeyRegistry> {
    &all_shards()[lock_key_shard_of(hash)]
}

/// The shards a whole-registry walker visits, one at a time: all of them when
/// sharded, else shard 0 alone (the old single registry).
#[inline]
pub fn lock_key_shards_in_use() -> &'static [Mutex<LockKeyRegistry>] {
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
pub fn unsharded_registry() -> &'static Mutex<LockKeyRegistry> {
    &all_shards()[0]
}

#[cfg(test)]
mod tests;
