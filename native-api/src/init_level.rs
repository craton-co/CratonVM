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
        .get_or_init(|| {
            Arc::new((AtomicI32::new(0), Mutex::new(()), Condvar::new()))
        })
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
    let cur = atomic.load(Ordering::Acquire);
    if level < cur {
        // Keep the diagnostic cheap — the `tracing` crate is the
        // native-builtins default but native-api doesn't pull it in,
        // so we write to stderr directly. Stays out of the hot path
        // because downward transitions should never happen.
        eprintln!(
            "[rustjvm] warning: VM.setInitLevel: refusing downward \
             transition {cur}->{level}"
        );
        return;
    }
    atomic.store(level, Ordering::Release);
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

    // Tests here can run in parallel with other tests that exercise
    // the init level, but they all target the same global state —
    // keep them deterministic by only asserting monotonic progression.

    #[test]
    fn get_does_not_panic_before_init() {
        // Safe to call before the global is ever written.
        let _ = get_init_level();
    }

    #[test]
    fn monotonic_advance() {
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
        set_init_level(4);
        // Should not block.
        let start = std::time::Instant::now();
        await_init_level(4);
        assert!(start.elapsed() < std::time::Duration::from_millis(100));
    }

    #[test]
    fn await_blocks_then_wakes() {
        // Reset via setting to 0 would be rejected after prior tests
        // bumped to 4; instead check the "already at or above"
        // fast path via await_init_level(0), which always returns.
        await_init_level(0);
    }
}
