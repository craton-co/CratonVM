// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP4.7 — `java.util.concurrent.locks.StampedLock` and
//! `java.util.concurrent.locks.ReentrantReadWriteLock` integration shim.
//!
//! The lock state machine itself lives in
//! `cratonvm-native-builtins::stamped_lock` so the natives that implement
//! the JDK `<init>` / `readLock` / `writeLock` / `validate` /
//! `tryConvertTo*` / `unlockRead` / `unlockWrite` callbacks can call into
//! it without re-importing the entire VM. This module documents how the
//! pieces fit together for anyone walking the threading surface from the
//! VM side.
//!
//! # Wiring
//!
//! - `register_essential_natives` (in `native-builtins`) calls
//!   `register_rwlock_natives` which registers:
//!     - `java/util/concurrent/locks/StampedLock` — 14 native callbacks
//!       (`<init>`, `readLock`, `writeLock`, `tryOptimisticRead`,
//!       `validate`, `unlockRead`, `unlockWrite`, `tryReadLock`,
//!       `tryWriteLock`, `tryConvertToWriteLock`, `tryConvertToReadLock`,
//!       `isReadLocked`, `isWriteLocked`, `getReadLockCount`).
//!     - `java/util/concurrent/locks/ReentrantReadWriteLock` —
//!       `<init>()V` / `<init>(Z)V` plus `readLock`/`writeLock` factories.
//!     - `java/util/concurrent/locks/ReentrantReadWriteLock$ReadLock` —
//!       `lock`, `unlock`, `tryLock`, `tryLock(J,TimeUnit)`,
//!       `lockInterruptibly`.
//!     - `java/util/concurrent/locks/ReentrantReadWriteLock$WriteLock` —
//!       `lock`, `unlock`, `tryLock`, `tryLock(J,TimeUnit)`,
//!       `lockInterruptibly`, `isHeldByCurrentThread`.
//!
//! Each callback funnels into a process-wide lock-state map keyed by the
//! Java object's pointer address (`ObjectRef::as_ptr() as usize`). The
//! state map is a `parking_lot::Mutex` over `HashMap<usize, Arc<Slot>>`,
//! and each slot holds one `Mutex<State>` + `Condvar` pair so check-and-
//! park is race-free.
//!
//! # Why this lives in `native-builtins` and not in `vm`
//!
//! `vm` depends on `native-builtins`, not the other way around. The lock
//! state needs to be reachable from the natives in `native-builtins`
//! without that crate growing a circular dependency on `vm`. Putting the
//! state owner in `native-builtins` keeps the dependency graph clean and
//! lets the unit tests in
//! `native-builtins/src/stamped_lock.rs` exercise multi-threaded scenarios
//! without spinning up a full `SharedVm`.
//!
//! # `vm/src/threading/stamped.rs` (this file) is intentionally tiny
//!
//! It exists so a VM-side reader looking under `threading/` can find the
//! lock semantics, and it re-exports the pure-Rust API for any future
//! callers who want to compose the same lock with VM internals (e.g. a
//! GC pause that needs to grab the StampedLock for a snapshotted object
//! to render a heap dump).
//!
//! No state is held here — every function below is a thin re-export of
//! the corresponding `cratonvm_native_builtins::stamped_lock` symbol.

#![allow(dead_code)]

pub use cratonvm_native_builtins::stamped_lock::{
    rw_init, rw_is_write_locked, rw_read_count, rw_read_lock, rw_read_unlock, rw_try_read_lock,
    rw_try_write_lock, rw_write_is_held, rw_write_lock, rw_write_unlock,
    stamped_get_read_lock_count, stamped_init, stamped_is_read_locked, stamped_is_write_locked,
    stamped_read_lock, stamped_try_convert_to_read, stamped_try_convert_to_write,
    stamped_try_optimistic_read, stamped_try_read_lock, stamped_try_write_lock,
    stamped_unlock_read, stamped_unlock_write, stamped_validate, stamped_write_lock,
    STAMPED_ORIGIN,
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test that the re-exports actually resolve to the
    /// native-builtins backend and that a fresh init returns the
    /// canonical origin stamp.
    #[test]
    fn shim_resolves_to_native_builtins_backend() {
        // Use a bogus address far from any real allocation. The state
        // map keys on it just like the natives do; subsequent test
        // runs in the same process won't collide because we use a
        // fresh address each call.
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0xDEAD_0000);
        let a = N.fetch_add(8, std::sync::atomic::Ordering::SeqCst);

        stamped_init(a);
        assert_eq!(stamped_try_optimistic_read(a), STAMPED_ORIGIN);

        // Write lock should round-trip through the same backend.
        let s = stamped_write_lock(a);
        assert_eq!(s & 255, 128, "JDK write stamp mode field is WBIT");
        assert!(stamped_unlock_write(a, s));
    }

    #[test]
    fn rwlock_shim_basic_round_trip() {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0xCAFE_0000);
        let a = N.fetch_add(8, std::sync::atomic::Ordering::SeqCst);

        rw_init(a, false);
        rw_write_lock(a, 1);
        assert!(rw_is_write_locked(a));
        rw_write_unlock(a, 1);
        assert!(!rw_is_write_locked(a));
    }
}
