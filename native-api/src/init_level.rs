// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP1.3 — Process-wide JVM bootstrap init-level state.
//!
//! Mirrors HotSpot's `VM::_init_level` integer state machine so the
//! `jdk/internal/misc/VM.initLevel()` / `awaitInitLevel(int)` natives
//! can observe and block on bootstrap progress without holding a
//! reference to the running `SharedVm`.
//!
//! State transitions (matching HotSpot's `init.cpp`):
//! * 0 — VM initialization has not started.
//! * 1 — Primordial classes loaded (`Object`, `Class`, `String`,
//!   wrappers). Roughly "after `bootstrap_core_classes`".
//! * 2 — `System.initPhase1` ran — system properties, encoding,
//!   and `System.in/out/err` installed.
//! * 3 — `System.initPhase2` ran — modules + classpath finalised,
//!   `ModuleLayer.boot()` populated.
//! * 4 — VM fully initialized — `initPhase3` ran,
//!   `ClassLoader.getSystemClassLoader()` usable, user `main()` about
//!   to run.
//!
//! `vm/src/vm/vm_init.rs` mirrors this state inside `SharedVm` so
//! in-process callers with a `&SharedVm` don't have to go through the
//! `OnceLock`.  Both sides write through [`set_init_level`] so
//! readers on either side see the same value.

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

/// Lazy-init global init-level registry.
///
/// `(atomic_level, monitor_mutex, condvar)`.  The mutex guards a
/// trivial `()` state; the real level lives in the atomic so the
/// read path stays lock-free.  The condvar only exists to wake
/// threads blocked in [`await_init_level`] after a
/// [`set_init_level`] call.
static GLOBAL: OnceLock<Arc<(AtomicI32, Mutex<()>, Condvar)>> = OnceLock::new();

/// Accessor — creates the global state on first use.  Safe to call
/// before any `SharedVm` exists.
pub fn global() -> Arc<(AtomicI32, Mutex<()>, Condvar)> {
    GLOBAL
        .get_or_init(|| Arc::new((AtomicI32::new(0), Mutex::new(()), Condvar::new())))
        .clone()
}

/// Return the current init level (0..=4).  Uses `Ordering::Acquire`
/// so callers observing a bumped level also see every prior
/// classloader / static-field write guarded by the bump.
#[inline]
pub fn get_init_level() -> i32 {
    global().0.load(Ordering::Acquire)
}

/// Advance the init level to `level`. Rejects downward transitions
/// (logs a warning and is otherwise a no-op). Wakes every thread
/// blocked in [`await_init_level`].
pub fn set_init_level(level: i32) {
    let state = global();
    let (atomic, lock, cv) = (&state.0, &state.1, &state.2);
    // Use `fetch_max` so two threads racing with monotonically-
    // increasing values can never violate the monotonic-progression
    // contract: the higher value always wins regardless of interleaving.
    // The previous load-then-store sequence had a classic lost-update
    // race — thread A reads 1, thread B reads 1, A stores 3, B stores 2,
    // and the final level is 2 instead of 3.
    //
    // `fetch_max` returns the *previous* value; from it we can both
    // detect (and warn about) a refused downward transition and decide
    // whether to notify waiters.
    let prev = atomic.fetch_max(level, Ordering::AcqRel);
    if level < prev {
        // Keep the diagnostic cheap — the `tracing` crate is the
        // native-builtins default but native-api doesn't pull it in,
        // so we write to stderr directly. Stays out of the hot path
        // because downward transitions should never happen.
        eprintln!(
            "[cratonvm] warning: VM.setInitLevel: refusing downward \
             transition {prev}->{level}"
        );
        return;
    }
    if level == prev {
        // No-op store — no waiters could become unblocked by this call,
        // so skip the lock/notify dance entirely.
        return;
    }
    // Release the lock before notifying so the woken thread can
    // re-acquire immediately.
    {
        // AUDIT 2026-05-16: a poisoned mutex during bootstrap previously
        // panicked the whole VM. The mutex only guards () (notification
        // sync), so recovering the inner value is safe.
        let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    }
    cv.notify_all();
}

/// Block the current thread until the init level reaches at least
/// `target`. Returns immediately when already at or above `target`.
/// Backs `jdk.internal.misc.VM.awaitInitLevel(int)`.
pub fn await_init_level(target: i32) {
    let state = global();
    let (atomic, lock, cv) = (&state.0, &state.1, &state.2);
    if atomic.load(Ordering::Acquire) >= target {
        return;
    }
    // AUDIT 2026-05-16: recover from poison rather than panic so a
    // single panicking holder doesn't take down every awaiter.
    let mut guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    while atomic.load(Ordering::Acquire) < target {
        guard = cv
            .wait(guard)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The init level is one process-wide, saturating, monotonic global, and
    // this module is its only user in this test binary. Every test that
    // asserts an absolute level — or that needs to start from a known floor —
    // takes `serial()` first, so no test can observe another's transient.

    /// Serialise the tests that read or drive the process-wide level.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static SERIAL: Mutex<()> = Mutex::new(());
        SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Lower the global back to the floor for a test that needs headroom.
    ///
    /// `set_init_level` correctly refuses this, so it goes to the atomic
    /// directly. Only sound while `serial()` is held.
    fn reset_to_floor() {
        global().0.store(0, Ordering::Release);
    }

    #[test]
    fn get_does_not_panic_before_init() {
        // Safe to call before the global is ever written.
        let _ = get_init_level();
    }

    #[test]
    fn monotonic_advance() {
        let _serial = serial();
        // Starts at whatever previous test left it at; bump to max
        // and verify we never go backward.
        let before = get_init_level();
        set_init_level(4);
        assert_eq!(get_init_level(), 4);
        set_init_level(before); // rejected
        assert_eq!(get_init_level(), 4);
    }

    #[test]
    fn await_returns_immediately_when_at_target() {
        let _serial = serial();
        set_init_level(4);
        // Should not block.
        let start = std::time::Instant::now();
        await_init_level(4);
        assert!(start.elapsed() < std::time::Duration::from_millis(100));
    }

    /// `await_init_level` must BLOCK below the target and WAKE on the set.
    ///
    /// It used to be one line — `await_init_level(0)`, the "already at or
    /// above" fast path, with no assertion at all. That exercised the branch
    /// that returns without ever touching the condvar, so the whole
    /// park-and-notify path this function exists for was uncovered: deleting
    /// the `cv.notify_all()` from `set_init_level`, or the wait loop from
    /// `await_init_level`, left the test green. The old comment explained the
    /// dodge — a reset "would be rejected after prior tests bumped to 4" —
    /// which `serial()` + `reset_to_floor()` now make unnecessary.
    ///
    /// Both halves are asserted, because either alone is satisfiable by a
    /// broken implementation: a function that returned immediately would pass
    /// a wake-up check, and one that never returned would pass a
    /// did-not-return-early check.
    ///
    /// Every wait is BOUNDED. A wake-up that never arrives fails with a named
    /// message in 10 s rather than hanging the test binary, and the 100 ms
    /// floor is a lower bound the test creates itself (the setter sleeps 150 ms
    /// first), so a slower machine makes the observed wait longer, never
    /// shorter.
    #[test]
    fn await_blocks_then_wakes() {
        use std::sync::mpsc;
        use std::thread;
        use std::time::{Duration, Instant};

        let _serial = serial();
        reset_to_floor();

        let (tx, rx) = mpsc::channel();
        let waiter = thread::spawn(move || {
            let start = Instant::now();
            await_init_level(3);
            let _ = tx.send(start.elapsed());
        });

        thread::sleep(Duration::from_millis(150));
        assert!(
            rx.try_recv().is_err(),
            "await_init_level(3) returned while the level was still {}; it did not park at all",
            get_init_level()
        );

        set_init_level(3);
        let waited = rx.recv_timeout(Duration::from_secs(10)).expect(
            "await_init_level(3) never returned after the level reached 3 — the waiter was not              woken. Check that set_init_level still calls cv.notify_all().",
        );
        assert!(
            waited >= Duration::from_millis(100),
            "the waiter returned after only {waited:?}, so it never actually parked"
        );
        waiter.join().unwrap();
        assert!(get_init_level() >= 3, "level: {}", get_init_level());

        // Leave the global where the rest of this module expects it.
        set_init_level(4);
    }

    #[test]
    fn concurrent_setters_preserve_monotonic_max() {
        // Regression: `set_init_level` previously did a load-then-store
        // on the underlying `AtomicI32`. Two threads racing — one with
        // a high value, one with a lower-but-still-above-current value
        // — could observe the same `cur` and then store in any order,
        // letting the lower write clobber the higher (last-write-wins).
        // With `fetch_max` the final value must equal the maximum any
        // racing thread tried to set, regardless of how the stores
        // interleave.
        //
        // THIS USED TO RACE A FRESH LOCAL ATOMIC through a test-local
        // `race_set` that reimplemented the production `fetch_max` line. So it
        // measured `AtomicI32::fetch_max` — a std guarantee — and never called
        // `set_init_level` at all. Restoring the load-then-store race in the
        // production function left it green, which is the one outcome it was
        // written to prevent.
        //
        // It now races the real `set_init_level` against the real global. That
        // is safe here for a stated reason rather than by luck: this module is
        // the global's only user in this test binary, and `serial()` excludes
        // every other test that reads or drives it, so nothing outside this
        // function can observe the floor it starts from.
        use std::thread;

        let _serial = serial();

        const TARGETS: &[i32] = &[1, 2, 3, 4, 2, 3, 1, 4, 3, 2];
        let expected_max = *TARGETS.iter().max().unwrap();

        // Repeated so the threads interleave differently across iterations.
        for _ in 0..32 {
            reset_to_floor();
            let mut handles = Vec::with_capacity(TARGETS.len());
            for &lvl in TARGETS {
                handles.push(thread::spawn(move || set_init_level(lvl)));
            }
            for h in handles {
                h.join().unwrap();
            }
            assert_eq!(
                get_init_level(),
                expected_max,
                "racing setters must converge to the max, not to the last writer"
            );
        }

        // A single-threaded control on the same path: without it, a
        // `set_init_level` that ignored its argument and always stored 4 would
        // satisfy every assertion above.
        reset_to_floor();
        set_init_level(2);
        assert_eq!(
            get_init_level(),
            2,
            "set_init_level must store the level it is given"
        );

        // Leave the global where the rest of this module expects it.
        set_init_level(4);
    }

    #[test]
    fn set_init_level_refuses_downward_after_race() {
        // Even after concurrent advancement to a high value, a later
        // call with a smaller value must not roll the global back.
        //
        // Takes `serial()` like the rest: it asserts an ABSOLUTE level, so it
        // must not run while `concurrent_setters_preserve_monotonic_max` or
        // `await_blocks_then_wakes` has the global at the floor.
        let _serial = serial();
        set_init_level(4);
        set_init_level(2);
        assert_eq!(get_init_level(), 4);
    }
}
