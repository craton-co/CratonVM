// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Global lock acquisition order (L0-L10) — re-export.
//!
//! The definitions live in [`cratonvm_types::lock_order`], one crate below
//! both `vm` and `cratonvm-gc`. They used to live here, which meant
//! `VmHeap`'s interior locks — L8 in the hierarchy, defined in the separate
//! `cratonvm-gc` crate — had no way to name [`LockLevel`] without inverting
//! the `vm -> gc` dependency edge. Moving the core down to `cratonvm-types`
//! removes that obstacle: `gc` can now announce L8 with [`enter_level`], or
//! wrap its interior locks in [`OrderedPlMutex`] / [`OrderedPlRwLock`]
//! directly.
//!
//! Nothing else changed. `crate::runtime::lock_order::X` still resolves for
//! every `X` it resolved for before, the per-thread tracker is still a
//! single `thread_local!` (it moved, it was not duplicated), and the
//! hierarchy table, the enforcement rules and the
//! `CRATONVM_LOCK_ORDER_CHECK` opt-in are unchanged — see
//! [`cratonvm_types::lock_order`] for all of it.
//!
//! Current wiring in `SharedVm` (see `crate::vm::realms`):
//!
//! - `shared.classes.class_manager` — [`OrderedPlRwLock`] at
//!   [`LockLevel::ClassManager`] (L10).
//! - `shared.mem.ref_processor` — [`OrderedPlMutex`] at
//!   [`LockLevel::RefProcessor`] (L7).
//! - `shared.natives.native_methods` — L9 is reserved; the registry is
//!   immutable after `SharedVm::new` and holds no lock.
//! - `shared.mem.heap` — L8, now *unblocked* but not yet wrapped: doing so
//!   would change locking behaviour and is left to a follow-up.

pub use cratonvm_types::lock_order::*;

#[cfg(test)]
mod tests {
    use super::*;

    /// Turn lock-order enforcement on for the tests that need the checker.
    ///
    /// Enforcement is unconditional in debug builds but **off by default in
    /// release** (see `cratonvm_types::lock_order::tracking::enforced`), so
    /// under `cargo test --release` every `#[should_panic(expected = "lock
    /// order violation")]` test below stopped panicking and failed -- 18
    /// failures that were pure build-config noise but had to be re-triaged as
    /// "pre-existing" on every release run. Enabling it explicitly makes a
    /// release run exercise the same path a debug run does. The call is
    /// idempotent and can only ever *enable* checking, so debug behaviour is
    /// unchanged.
    fn require_enforcement() {
        tracking::force_enable_for_testing();
        assert!(
            tracking::enforced(),
            "lock-order enforcement must be active for this test"
        );
    }

    // -- LockLevel tests ----------------------------------------------------

    #[test]
    fn lock_level_ordering() {
        // Numeric ordering matches the doc's table — higher number is acquired
        // earlier in the descending hierarchy.
        assert!(LockLevel::Scratch < LockLevel::JvmThread);
        assert!(LockLevel::JvmThread < LockLevel::NativeMemory);
        assert!(LockLevel::NativeMemory < LockLevel::CleanerActions);
        assert!(LockLevel::CleanerActions < LockLevel::FlightRecorder);
        assert!(LockLevel::FlightRecorder < LockLevel::ThreadRegistry);
        assert!(LockLevel::ThreadRegistry < LockLevel::Monitors);
        assert!(LockLevel::Monitors < LockLevel::RefProcessor);
        assert!(LockLevel::RefProcessor < LockLevel::Heap);
        assert!(LockLevel::Heap < LockLevel::NativeMethods);
        assert!(LockLevel::NativeMethods < LockLevel::ClassManager);
    }

    #[test]
    fn lock_level_discriminants_match_docs() {
        // These integer values are the canonical hierarchy (see this module's
        // doc comment) and MUST not drift. If you change them, change the table
        // in the module doc too.
        assert_eq!(LockLevel::Scratch as u8, 0);
        assert_eq!(LockLevel::JvmThread as u8, 1);
        assert_eq!(LockLevel::NativeMemory as u8, 2);
        assert_eq!(LockLevel::CleanerActions as u8, 3);
        assert_eq!(LockLevel::FlightRecorder as u8, 4);
        assert_eq!(LockLevel::ThreadRegistry as u8, 5);
        assert_eq!(LockLevel::Monitors as u8, 6);
        assert_eq!(LockLevel::RefProcessor as u8, 7);
        assert_eq!(LockLevel::Heap as u8, 8);
        assert_eq!(LockLevel::NativeMethods as u8, 9);
        assert_eq!(LockLevel::ClassManager as u8, 10);
    }

    #[test]
    fn lock_level_equality() {
        assert_eq!(LockLevel::Heap, LockLevel::Heap);
        assert_ne!(LockLevel::Heap, LockLevel::ClassManager);
    }

    #[test]
    fn lock_level_display() {
        let s = format!("{}", LockLevel::Heap);
        assert_eq!(s, "Heap");
    }

    // -- OrderedMutex basic -------------------------------------------------

    #[test]
    fn mutex_single_lock_unlock() {
        let m = OrderedMutex::new(42, LockLevel::Heap);
        {
            let g = m.lock().unwrap();
            assert_eq!(*g, 42);
        }
        // Can re-acquire after drop.
        let mut g = m.lock().unwrap();
        *g = 99;
        assert_eq!(*g, 99);
    }

    #[test]
    fn mutex_level_accessor() {
        let m = OrderedMutex::new((), LockLevel::Heap);
        assert_eq!(m.level(), LockLevel::Heap);
    }

    #[test]
    fn mutex_descending_order_ok() {
        // ClassManager (10) -> Heap (8) -> Scratch (0): the canonical
        // descending acquisition the doc describes.
        let a = OrderedMutex::new(1, LockLevel::ClassManager);
        let b = OrderedMutex::new(2, LockLevel::Heap);
        let c = OrderedMutex::new(3, LockLevel::Scratch);

        let ga = a.lock().unwrap();
        let gb = b.lock().unwrap();
        let gc = c.lock().unwrap();
        assert_eq!(*ga + *gb + *gc, 6);
    }

    #[test]
    fn mutex_non_adjacent_levels_ok() {
        let a = OrderedMutex::new((), LockLevel::ClassManager);
        let b = OrderedMutex::new((), LockLevel::Scratch);
        let _ga = a.lock().unwrap();
        let _gb = b.lock().unwrap();
    }

    #[test]
    #[should_panic(expected = "lock order violation")]
    fn mutex_ascending_order_panics() {
        require_enforcement();
        // Holding Heap (8) and then trying to acquire ClassManager (10) is the
        // forbidden inversion described in this module's doc comment.
        let a = OrderedMutex::new((), LockLevel::Heap);
        let b = OrderedMutex::new((), LockLevel::ClassManager);
        let _ga = a.lock().unwrap();
        let _gb = b.lock().unwrap(); // boom
    }

    #[test]
    #[should_panic(expected = "lock order violation")]
    fn mutex_same_level_panics() {
        require_enforcement();
        let a = OrderedMutex::new((), LockLevel::ThreadRegistry);
        let b = OrderedMutex::new((), LockLevel::ThreadRegistry);
        let _ga = a.lock().unwrap();
        let _gb = b.lock().unwrap(); // same level => violation
    }

    #[test]
    fn mutex_release_then_higher_ok() {
        let a = OrderedMutex::new((), LockLevel::Heap);
        let b = OrderedMutex::new((), LockLevel::ClassManager);

        {
            let _ga = a.lock().unwrap();
            // drop ga
        }
        // Now ClassManager is fine because nothing is held.
        let _gb = b.lock().unwrap();
    }

    // -- OrderedRwLock basic ------------------------------------------------

    #[test]
    fn rwlock_read_write() {
        let rw = OrderedRwLock::new(String::from("hello"), LockLevel::Monitors);
        {
            let r = rw.read().unwrap();
            assert_eq!(&*r, "hello");
        }
        {
            let mut w = rw.write().unwrap();
            w.push_str(" world");
        }
        {
            let r = rw.read().unwrap();
            assert_eq!(&*r, "hello world");
        }
    }

    #[test]
    fn rwlock_level_accessor() {
        let rw = OrderedRwLock::new((), LockLevel::ClassManager);
        assert_eq!(rw.level(), LockLevel::ClassManager);
    }

    #[test]
    fn rwlock_descending_read_ok() {
        let a = OrderedRwLock::new(1, LockLevel::ClassManager);
        let b = OrderedRwLock::new(2, LockLevel::Monitors);
        let ga = a.read().unwrap();
        let gb = b.read().unwrap();
        assert_eq!(*ga + *gb, 3);
    }

    #[test]
    fn rwlock_descending_write_ok() {
        let a = OrderedRwLock::new(1, LockLevel::ClassManager);
        let b = OrderedRwLock::new(2, LockLevel::Heap);
        let _ga = a.write().unwrap();
        let _gb = b.write().unwrap();
    }

    #[test]
    #[should_panic(expected = "lock order violation")]
    fn rwlock_ascending_read_panics() {
        require_enforcement();
        let a = OrderedRwLock::new((), LockLevel::Heap);
        let b = OrderedRwLock::new((), LockLevel::ClassManager);
        let _ga = a.read().unwrap();
        let _gb = b.read().unwrap();
    }

    #[test]
    #[should_panic(expected = "lock order violation")]
    fn rwlock_ascending_write_panics() {
        require_enforcement();
        let a = OrderedRwLock::new((), LockLevel::Monitors);
        let b = OrderedRwLock::new((), LockLevel::ClassManager);
        let _ga = a.write().unwrap();
        let _gb = b.write().unwrap();
    }

    #[test]
    #[should_panic(expected = "lock order violation")]
    fn rwlock_same_level_panics() {
        require_enforcement();
        let a = OrderedRwLock::new((), LockLevel::Monitors);
        let b = OrderedRwLock::new((), LockLevel::Monitors);
        let _ga = a.read().unwrap();
        let _gb = b.write().unwrap();
    }

    // -- Mixed mutex + rwlock -----------------------------------------------

    #[test]
    fn mixed_rwlock_then_mutex_descending_ok() {
        // The textbook combo from this module: class_manager (RwLock, L10) held,
        // then heap (Mutex, L8) acquired.
        let rw = OrderedRwLock::new((), LockLevel::ClassManager);
        let m = OrderedMutex::new((), LockLevel::Heap);
        let _gr = rw.write().unwrap();
        let _gm = m.lock().unwrap();
    }

    #[test]
    fn mixed_mutex_then_rwlock_descending_ok() {
        let m = OrderedMutex::new((), LockLevel::Heap);
        let rw = OrderedRwLock::new((), LockLevel::Monitors);
        let _gm = m.lock().unwrap();
        let _gr = rw.read().unwrap();
    }

    #[test]
    #[should_panic(expected = "lock order violation")]
    fn mixed_mutex_then_rwlock_ascending_panics() {
        require_enforcement();
        // The exact "Forbidden: monitor -> class manager" case from this module.
        let m = OrderedMutex::new((), LockLevel::Monitors);
        let rw = OrderedRwLock::new((), LockLevel::ClassManager);
        let _gm = m.lock().unwrap();
        let _gr = rw.read().unwrap();
    }

    // -- LockOrderViolation error type --------------------------------------

    #[test]
    fn violation_display() {
        let v = LockOrderViolation {
            attempted: LockLevel::ClassManager,
            held: LockLevel::Heap,
        };
        let s = format!("{}", v);
        assert!(s.contains("lock order violation"));
        assert!(s.contains("ClassManager"));
        assert!(s.contains("Heap"));
    }

    #[test]
    fn violation_is_error() {
        let v = LockOrderViolation {
            attempted: LockLevel::ClassManager,
            held: LockLevel::Monitors,
        };
        let e: &dyn std::error::Error = &v;
        assert!(e.to_string().contains("lock order violation"));
    }

    // -- Thread isolation ---------------------------------------------------

    #[test]
    fn separate_threads_independent() {
        use std::sync::Arc;
        use std::thread;

        // Two threads each acquire the *same* level independently -- no
        // violation because tracking is per-thread.
        let m1 = Arc::new(OrderedMutex::new((), LockLevel::Heap));
        let m2 = Arc::new(OrderedMutex::new((), LockLevel::Heap));

        let m1c = Arc::clone(&m1);
        let m2c = Arc::clone(&m2);

        let t1 = thread::spawn(move || {
            let _g = m1c.lock().unwrap();
        });
        let t2 = thread::spawn(move || {
            let _g = m2c.lock().unwrap();
        });

        t1.join().unwrap();
        t2.join().unwrap();
    }

    #[test]
    fn full_descending_chain() {
        // Mirrors the descending hierarchy documented in this module from L10
        // down to L0. Acquiring in this order must succeed.
        let locks: Vec<OrderedMutex<usize>> = vec![
            OrderedMutex::new(10, LockLevel::ClassManager),
            OrderedMutex::new(9, LockLevel::NativeMethods),
            OrderedMutex::new(8, LockLevel::Heap),
            OrderedMutex::new(7, LockLevel::RefProcessor),
            OrderedMutex::new(6, LockLevel::Monitors),
            OrderedMutex::new(5, LockLevel::ThreadRegistry),
            OrderedMutex::new(4, LockLevel::FlightRecorder),
            OrderedMutex::new(3, LockLevel::CleanerActions),
            OrderedMutex::new(2, LockLevel::NativeMemory),
            OrderedMutex::new(1, LockLevel::JvmThread),
            OrderedMutex::new(0, LockLevel::Scratch),
        ];

        let guards: Vec<_> = locks.iter().map(|l| l.lock().unwrap()).collect();
        let sum: usize = guards.iter().map(|g| **g).sum();
        assert_eq!(sum, 10 + 9 + 8 + 7 + 6 + 5 + 4 + 3 + 2 + 1 + 0);
    }

    // -- Smoke test: every documented L -> L transition succeeds ------------
    //
    // This module's hierarchy permits combinations such as:
    //   class_manager (L10) -> heap (L8) -> ref_processor (L7)
    //   class_manager (L10) -> monitors (L6) -> thread_registry (L5)
    //   heap (L8)          -> monitors (L6) -> thread_registry (L5)
    //
    // The smoke test below exercises each adjacent pair in the module's table
    // (acquire the higher-level lock, then acquire the lower-level lock, then
    // drop both) to prove the wrappers and the doc agree.

    #[test]
    fn smoke_every_adjacent_descending_pair() {
        // Pairs are (higher-level, lower-level). Each must be acquirable
        // higher-then-lower without tripping the assertion.
        let pairs: &[(LockLevel, LockLevel)] = &[
            (LockLevel::ClassManager, LockLevel::NativeMethods),
            (LockLevel::NativeMethods, LockLevel::Heap),
            (LockLevel::Heap, LockLevel::RefProcessor),
            (LockLevel::RefProcessor, LockLevel::Monitors),
            (LockLevel::Monitors, LockLevel::ThreadRegistry),
            (LockLevel::ThreadRegistry, LockLevel::FlightRecorder),
            (LockLevel::FlightRecorder, LockLevel::CleanerActions),
            (LockLevel::CleanerActions, LockLevel::NativeMemory),
            (LockLevel::NativeMemory, LockLevel::JvmThread),
            (LockLevel::JvmThread, LockLevel::Scratch),
        ];

        for (high, low) in pairs {
            let outer = OrderedMutex::new(*high as u8, *high);
            let inner = OrderedMutex::new(*low as u8, *low);
            let _go = outer.lock().unwrap();
            let _gi = inner.lock().unwrap();
            assert_eq!(*_go, *high as u8);
            assert_eq!(*_gi, *low as u8);
            // Both drop here, in inner-then-outer order (LIFO).
        }
    }

    #[test]
    fn smoke_canonical_doc_examples() {
        // Example: "GC stops the world" — heap (L8) holds, then
        // monitors (L6), then thread_registry (L5).
        {
            let heap = OrderedMutex::new((), LockLevel::Heap);
            let monitors = OrderedMutex::new((), LockLevel::Monitors);
            let registry = OrderedMutex::new((), LockLevel::ThreadRegistry);
            let _h = heap.lock().unwrap();
            let _m = monitors.lock().unwrap();
            let _r = registry.lock().unwrap();
        }

        // Example: "interpreter calls into the heap" — heap (L8) -> ref_processor (L7).
        {
            let heap = OrderedMutex::new((), LockLevel::Heap);
            let refp = OrderedMutex::new((), LockLevel::RefProcessor);
            let _h = heap.lock().unwrap();
            let _r = refp.lock().unwrap();
        }

        // Example: class_manager (L10) -> heap (L8).
        {
            let cm = OrderedRwLock::new((), LockLevel::ClassManager);
            let heap = OrderedMutex::new((), LockLevel::Heap);
            let _c = cm.write().unwrap();
            let _h = heap.lock().unwrap();
        }
    }

    // -- Debug trait ---------------------------------------------------------

    #[test]
    fn debug_impls() {
        let m = OrderedMutex::new(42_i32, LockLevel::Heap);
        let dbg = format!("{:?}", m);
        assert!(dbg.contains("OrderedMutex"));
        assert!(dbg.contains("Heap"));

        let rw = OrderedRwLock::new(7_i32, LockLevel::ClassManager);
        let dbg = format!("{:?}", rw);
        assert!(dbg.contains("OrderedRwLock"));
        assert!(dbg.contains("ClassManager"));
    }

    // -- V11 wiring invariants (monitors registry) --------------------------

    // SECURITY FIX (V11): the real `MonitorTable` now holds two L6 registries
    // (`monitors` and `cas_locks`). The checker must forbid nesting one inside
    // the other (equal level is not strictly descending). This reproduces the
    // exact shape of the bug that `remap_after_gc` was restructured to avoid.
    #[test]
    #[should_panic(expected = "lock order violation")]
    fn v11_two_monitor_level_registries_must_not_nest() {
        require_enforcement();
        let monitors = OrderedMutex::new((), LockLevel::Monitors);
        let cas_locks = OrderedMutex::new((), LockLevel::Monitors);
        let _m = monitors.lock().unwrap();
        let _c = cas_locks.lock().unwrap(); // same level (L6) => violation
    }

    // SECURITY FIX (V11): acquiring a lower-level lock *after* the L6 monitors
    // registry is the documented, allowed direction and must NOT trip.
    #[test]
    fn v11_monitors_then_lower_ok() {
        let monitors = OrderedMutex::new((), LockLevel::Monitors);
        let scratch = OrderedMutex::new((), LockLevel::Scratch); // L0 < L6
        let _m = monitors.lock().unwrap();
        let _s = scratch.lock().unwrap();
    }

    // SECURITY FIX (V11): holding the L6 monitors registry and then reaching
    // *up* for class_manager (L10) is the canonical forbidden monitor ->
    // class_manager inversion documented in this module. Even though
    // class_manager itself is not yet wrapped, the check fires the moment any
    // higher-level OrderedRwLock is acquired under a held monitor.
    #[test]
    #[should_panic(expected = "lock order violation")]
    fn v11_monitors_then_classmanager_inverts() {
        require_enforcement();
        let monitors = OrderedMutex::new((), LockLevel::Monitors);
        let class_manager = OrderedRwLock::new((), LockLevel::ClassManager);
        let _m = monitors.lock().unwrap();
        let _c = class_manager.write().unwrap(); // L10 under L6 => violation
    }

    #[test]
    fn guard_debug() {
        let m = OrderedMutex::new(99, LockLevel::Heap);
        let g = m.lock().unwrap();
        let dbg = format!("{:?}", g);
        assert!(dbg.contains("99"));
    }

    // -- Enforcement gating --------------------------------------------------

    // Enforcement is unconditional in debug builds; in release it is off
    // unless opted into. This test used to assert the runner WAS a debug build,
    // which made it (and every `#[should_panic]` test above) fail under
    // `cargo test --release` for no reason other than the build profile. Pin
    // the real contract instead: debug always enforces, and `require_enforcement`
    // brings a release runner up to the same footing.
    #[test]
    fn enforcement_active_in_debug_builds() {
        if cfg!(debug_assertions) {
            assert!(
                tracking::enforced(),
                "lock-order enforcement must be active in debug builds"
            );
        }
        require_enforcement();
    }

    // Acquiring then releasing must leave the per-thread held-set empty so a
    // later same-or-higher acquisition is allowed — verifies the
    // `release_if_enforced` path stays balanced with `check_and_acquire`.
    #[test]
    fn acquire_release_is_balanced() {
        let a = OrderedMutex::new((), LockLevel::Monitors);
        {
            let _g = a.lock().unwrap();
        }
        // Nothing held now: a higher-level lock must be acquirable.
        let b = OrderedMutex::new((), LockLevel::ClassManager);
        let _gb = b.lock().unwrap();
    }

    // -- Drop-in parking_lot-backed wrappers (top-of-hierarchy wiring) ------
    //
    // `class_manager` (L10) and `ref_processor` (L7) are `OrderedPlRwLock` /
    // `OrderedPlMutex` in `SharedVm`. The tests below pin the behaviour those
    // fields now rely on: descending acquisition succeeds, every inversion is
    // detected, and the parking_lot family shares one per-thread tracker with
    // the std-backed family so a monitor (L6, `OrderedMutex`) held across a
    // `class_manager` (L10, `OrderedPlRwLock`) acquisition is caught.

    #[test]
    fn pl_mutex_basic() {
        let m = OrderedPlMutex::new(42, LockLevel::RefProcessor);
        assert_eq!(m.level(), LockLevel::RefProcessor);
        assert!(!m.is_locked());
        {
            let mut g = m.lock();
            assert_eq!(*g, 42);
            *g = 7;
        }
        {
            let g = m.try_lock().expect("uncontended try_lock must succeed");
            assert_eq!(*g, 7);
        }
        let mut m = m;
        *m.get_mut() = 11;
        assert_eq!(m.into_inner(), 11);
    }

    #[test]
    fn pl_rwlock_basic() {
        let rw = OrderedPlRwLock::new(String::from("hello"), LockLevel::ClassManager);
        assert_eq!(rw.level(), LockLevel::ClassManager);
        {
            let r = rw.read();
            assert_eq!(&*r, "hello");
        }
        {
            let mut w = rw.write();
            w.push_str(" world");
        }
        {
            let r = rw.try_read().expect("uncontended try_read must succeed");
            assert_eq!(&*r, "hello world");
        }
        {
            let w = rw.try_write().expect("uncontended try_write must succeed");
            assert_eq!(&**w, "hello world");
        }
        let mut rw = rw;
        rw.get_mut().push('!');
        assert_eq!(rw.into_inner(), "hello world!");
    }

    /// The four top levels of the documented hierarchy, acquired highest-first
    /// exactly as the table in this module's doc comment prescribes:
    /// `class_manager` (L10) -> `native_methods` (L9) -> `heap` (L8) ->
    /// `ref_processor` (L7) -> `monitors` (L6). This mixes all three
    /// enforcement mechanisms — `OrderedPlRwLock` (class_manager),
    /// `OrderedPlMutex` (ref_processor), `LevelScope` (heap, which lives in the
    /// `gc` crate and cannot be wrapped) and `OrderedMutex` (monitors) — to
    /// prove they share one per-thread tracker.
    #[test]
    fn top_of_hierarchy_descending_order_ok() {
        let class_manager = OrderedPlRwLock::new(10_u8, LockLevel::ClassManager);
        let native_methods = OrderedPlMutex::new(9_u8, LockLevel::NativeMethods);
        let ref_processor = OrderedPlMutex::new(7_u8, LockLevel::RefProcessor);
        let monitors = OrderedMutex::new(6_u8, LockLevel::Monitors);

        let cm = class_manager.write();
        let nm = native_methods.lock();
        let heap = enter_level(LockLevel::Heap);
        let rp = ref_processor.lock();
        let mon = monitors.lock().unwrap();

        assert_eq!(*cm + *nm + *rp + *mon, 10 + 9 + 7 + 6);
        assert_eq!(heap.level(), LockLevel::Heap);
        // Guards drop in reverse (LIFO) order here: L6, L7, L8, L9, L10.
    }

    #[test]
    fn pl_release_then_higher_ok() {
        let rp = OrderedPlMutex::new((), LockLevel::RefProcessor);
        let cm = OrderedPlRwLock::new((), LockLevel::ClassManager);
        {
            let _g = rp.lock();
        }
        // Nothing held: the higher level is acquirable again.
        let _cm = cm.read();
    }

    /// `read_recursive` on a lock this thread already reads is the documented
    /// reentrant case (`interpreter.rs::resolve_method_ref`) and must not trip.
    #[test]
    fn pl_read_recursive_reentrant_ok() {
        require_enforcement();
        let cm = OrderedPlRwLock::new(1_u8, LockLevel::ClassManager);
        let outer = cm.read();
        let inner = cm.read_recursive();
        assert_eq!(*outer, *inner);
        drop(inner);
        // The outer guard still holds the level, so a same-level plain read is
        // still (correctly) refused — see `pl_read_recursive_does_not_release`.
        assert!(tracking::is_held(LockLevel::ClassManager));
        drop(outer);
        assert!(!tracking::is_held(LockLevel::ClassManager));
    }

    /// A reentrant read must not clear the outer acquisition's record when it
    /// drops — otherwise the tracker would forget a still-held L10 lock.
    #[test]
    fn pl_read_recursive_does_not_release_outer() {
        require_enforcement();
        let cm = OrderedPlRwLock::new((), LockLevel::ClassManager);
        let _outer = cm.read();
        {
            let _inner = cm.read_recursive();
        }
        assert!(
            tracking::is_held(LockLevel::ClassManager),
            "dropping the reentrant read guard must leave the outer record intact"
        );
    }

    #[test]
    fn level_scope_descending_ok() {
        let _heap = enter_level(LockLevel::Heap);
        let _monitors = enter_level(LockLevel::Monitors);
        let _scratch = enter_level(LockLevel::Scratch);
    }

    #[test]
    fn pl_debug_impls() {
        let m = OrderedPlMutex::new(42_i32, LockLevel::RefProcessor);
        let dbg = format!("{:?}", m);
        assert!(dbg.contains("OrderedPlMutex"));
        assert!(dbg.contains("RefProcessor"));
        let g = m.lock();
        assert!(format!("{:?}", g).contains("42"));
        drop(g);

        let rw = OrderedPlRwLock::new(7_i32, LockLevel::ClassManager);
        let dbg = format!("{:?}", rw);
        assert!(dbg.contains("OrderedPlRwLock"));
        assert!(dbg.contains("ClassManager"));
        assert!(format!("{:?}", rw.read()).contains('7'));
        assert!(format!("{:?}", rw.write()).contains('7'));
        assert!(format!("{:?}", enter_level(LockLevel::Scratch)).contains("Scratch"));
    }

    // ---- inversions that must be detected --------------------------------

    /// The historically real defect: holding a monitor (L6, std-backed
    /// `OrderedMutex`) and then reaching *up* for `class_manager` (L10,
    /// parking_lot-backed). This is the class_manager/vtable ABBA shape, and it
    /// only fires because both wrapper families share one per-thread tracker.
    #[test]
    #[should_panic(expected = "lock order violation")]
    fn pl_monitor_then_class_manager_inverts_across_families() {
        require_enforcement();
        let monitors = OrderedMutex::new((), LockLevel::Monitors);
        let class_manager = OrderedPlRwLock::new((), LockLevel::ClassManager);
        let _m = monitors.lock().unwrap();
        let _cm = class_manager.read(); // L10 under L6 => violation
    }

    /// `ref_processor` (L7) held while acquiring `class_manager` (L10) — the
    /// inversion the GC reference-processing path must never take.
    #[test]
    #[should_panic(expected = "lock order violation")]
    fn pl_ref_processor_then_class_manager_inverts() {
        require_enforcement();
        let ref_processor = OrderedPlMutex::new((), LockLevel::RefProcessor);
        let class_manager = OrderedPlRwLock::new((), LockLevel::ClassManager);
        let _rp = ref_processor.lock();
        let _cm = class_manager.write(); // L10 under L7 => violation
    }

    #[test]
    #[should_panic(expected = "lock order violation")]
    fn pl_rwlock_same_level_panics() {
        require_enforcement();
        let a = OrderedPlRwLock::new((), LockLevel::ClassManager);
        let b = OrderedPlRwLock::new((), LockLevel::ClassManager);
        let _ga = a.read();
        let _gb = b.write(); // same level (L10) on a *different* lock => violation
    }

    #[test]
    #[should_panic(expected = "lock order violation")]
    fn pl_mutex_ascending_panics() {
        require_enforcement();
        let low = OrderedPlMutex::new((), LockLevel::Monitors);
        let high = OrderedPlMutex::new((), LockLevel::NativeMethods);
        let _l = low.lock();
        let _h = high.lock(); // L9 under L6 => violation
    }

    /// `read_recursive` is only exempt for a level this thread *already* holds.
    /// Reaching up to L10 from a held L6 must still be rejected.
    #[test]
    #[should_panic(expected = "lock order violation")]
    fn pl_read_recursive_under_lower_level_panics() {
        require_enforcement();
        let monitors = OrderedMutex::new((), LockLevel::Monitors);
        let class_manager = OrderedPlRwLock::new((), LockLevel::ClassManager);
        let _m = monitors.lock().unwrap();
        let _cm = class_manager.read_recursive(); // L10 under L6 => violation
    }

    /// A `LevelScope` standing in for the `gc`-crate heap locks (L8) must be
    /// observed by the tracker: taking `class_manager` (L10) under it inverts.
    #[test]
    #[should_panic(expected = "lock order violation")]
    fn level_scope_then_higher_lock_panics() {
        require_enforcement();
        let _heap = enter_level(LockLevel::Heap);
        let class_manager = OrderedPlRwLock::new((), LockLevel::ClassManager);
        let _cm = class_manager.read(); // L10 under L8 => violation
    }

    #[test]
    #[should_panic(expected = "lock order violation")]
    fn level_scope_ascending_panics() {
        require_enforcement();
        let _monitors = enter_level(LockLevel::Monitors);
        let _heap = enter_level(LockLevel::Heap); // L8 under L6 => violation
    }

    #[test]
    fn enforcement_active_matches_debug_assertions() {
        // The public query must always agree with the internal one, and a debug
        // runner must have enforcement on without anyone asking for it.
        assert_eq!(enforcement_active(), tracking::enforced());
        if cfg!(debug_assertions) {
            assert!(enforcement_active());
        }
        // ... and once opted in, both agree it is on in release too.
        require_enforcement();
        assert!(enforcement_active());
    }
}
