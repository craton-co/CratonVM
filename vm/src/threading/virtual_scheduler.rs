// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! VESTIGIAL — this module has **no live callers** and bounds nothing.
//! Do not wire it up. Scheduled for deletion; see
//! `vt-resume-gc-fixup.md` §3.
//!
//! It was an early sketch of JEP 444: a counting semaphore meant to bound how
//! many virtual threads run at once. The bound was never real. The permits
//! here are **disjoint from the actual carrier pool**, which lives in
//! [`crate::threading::VirtualThreadManager`] / `ForkJoinScheduler`
//! (`virtual_threads.rs`) and owns the real carrier OS threads. Nothing in
//! this module can start, stop, park, or free a carrier: `release()`
//! increments a counter, and the carrier OS thread it was named after keeps
//! running (or keeps blocking) regardless.
//!
//! Every call site was removed on 2026-07-26 (`vm/src/vm/vm_exec.rs`). Three
//! were already unreachable — they sat inside the platform-spawn closure,
//! past the `is_virtual` early return. Two more (`vt_release_carrier` /
//! `vt_acquire_carrier`) were reached only from `Thread.sleep` natives whose
//! guard `release && vt_park_for(..)` evaluates the *same* predicate twice, so
//! the `ContinuationYield` return always won. The last pair, in
//! `NativeContextImpl::park`, was the only reachable one and was a hang risk:
//! nothing else acquired from this pool, so once more virtual threads had
//! parked concurrently than `carrier_count`, the post-park `acquire()` blocked
//! a real carrier OS thread on a permit no one would return.
//!
//! That is the danger this module poses and why it should go rather than be
//! "fixed": it reads as backpressure, so a reader concludes the carrier pool
//! is bounded and that virtual-thread starvation is already handled. It is
//! not. The genuine bound, and the genuine starvation compensation, are the
//! watchdog in `virtual_threads.rs`.
//!
//! What still keeps the type alive is purely structural, in files outside this
//! change's ownership — see the cross-owner request in the doc above:
//! `threading/mod.rs` (`pub mod` / `pub use`), `vm/realms/thread_realm.rs`
//! (the `virtual_scheduler` field), `vm/vm_init.rs` (`new_default()`), and two
//! tests in `vm/src/vm.rs`. The implementation below is therefore left
//! behaviourally intact so those keep compiling and passing.

use std::sync::Arc;

use parking_lot::{Condvar, Mutex};

/// A counting semaphore that once claimed to bound virtual-thread
/// concurrency. **Nothing calls it** — see the module header. Retained only
/// so the (non-owned) construction site and tests keep compiling.
///
/// The counter is self-consistent; it simply has no relationship to any
/// carrier. Treat `carrier_count` as "the number this was constructed with",
/// not as a live property of the VM.
pub struct VirtualThreadScheduler {
    state: Mutex<SchedulerState>,
    condvar: Condvar,
    carrier_count: usize,
}

struct SchedulerState {
    available: usize,
}

impl VirtualThreadScheduler {
    /// Create a scheduler with the given number of carrier threads.
    pub fn new(carrier_count: usize) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(SchedulerState {
                available: carrier_count,
            }),
            condvar: Condvar::new(),
            carrier_count,
        })
    }

    /// Create a scheduler sized to the available hardware parallelism.
    pub fn new_default() -> Arc<Self> {
        let count = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        Self::new(count)
    }

    /// Acquire a carrier permit, blocking until one is available.
    ///
    /// Must be called by virtual threads before entering the interpreter loop.
    pub fn acquire(&self) {
        let mut state = self.state.lock();
        while state.available == 0 {
            self.condvar.wait(&mut state);
        }
        state.available -= 1;
    }

    /// Release a carrier permit, waking one waiting virtual thread.
    ///
    /// Called when a virtual thread blocks (sleep, park, wait) or terminates.
    ///
    /// Bug B4 (round-9): the increment is clamped to `carrier_count`. An
    /// unbalanced or double `release()` (one not paired with a prior
    /// `acquire()`) previously raised `available` above `carrier_count`,
    /// permanently weakening the concurrency bound the semaphore exists to
    /// enforce (and, in the limit, could wrap on overflow). Saturating at
    /// `carrier_count` makes a stray release a no-op rather than corrupting
    /// the permit count. We only `notify_one()` when a permit actually became
    /// available so a clamped (already-full) release doesn't spuriously wake
    /// a waiter.
    pub fn release(&self) {
        let mut state = self.state.lock();
        let next = (state.available + 1).min(self.carrier_count);
        if next != state.available {
            state.available = next;
            self.condvar.notify_one();
        }
    }

    /// The total number of carrier threads.
    pub fn carrier_count(&self) -> usize {
        self.carrier_count
    }

    /// Number of currently-available permits (test-only introspection).
    #[cfg(test)]
    fn available(&self) -> usize {
        self.state.lock().available
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheduler_creation_default() {
        let sched = VirtualThreadScheduler::new_default();
        assert!(sched.carrier_count() > 0);
    }

    #[test]
    fn scheduler_acquire_release() {
        let sched = VirtualThreadScheduler::new(2);
        // Acquire two permits
        sched.acquire();
        sched.acquire();
        // Release one
        sched.release();
        // Acquire again should succeed
        sched.acquire();
        sched.release();
        sched.release();
    }

    #[test]
    fn release_cannot_exceed_carrier_count() {
        // Bug B4: an over-release (release without a paired acquire) must not
        // raise `available` past `carrier_count`.
        let sched = VirtualThreadScheduler::new(2);
        assert_eq!(sched.available(), 2);

        // Stray releases while already full are no-ops.
        sched.release();
        sched.release();
        sched.release();
        assert_eq!(
            sched.available(),
            2,
            "available must clamp to carrier_count"
        );

        // The bound is still enforced: exactly two acquires drain the pool.
        sched.acquire();
        sched.acquire();
        assert_eq!(sched.available(), 0);

        // Two releases matching the two acquires above are both legitimate
        // and fully restore the pool to capacity. (An earlier version of this
        // test asserted `1` here, expecting the second release to be treated
        // as a "double release" — but with 2 real acquires outstanding, both
        // releases genuinely pair with one, so both must count.
        //
        // The original rationale continued "…real callers pair
        // acquire/release per blocking event (see `vt_acquire_carrier` /
        // `vt_release_carrier` in `vm_exec.rs`)". Those callers are gone as of
        // 2026-07-26 and there are no real callers at all now; what is left
        // below is a semantics test of the counter itself.)
        sched.release();
        sched.release();
        assert_eq!(
            sched.available(),
            2,
            "two releases matching two prior acquires must fully restore the pool"
        );

        // A further release with no corresponding acquire is a genuine
        // over-release and is still clamped rather than accumulated.
        sched.release();
        assert_eq!(
            sched.available(),
            2,
            "an over-release beyond carrier_count is clamped, not accumulated"
        );
    }

    #[test]
    fn scheduler_blocks_when_exhausted() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::thread;
        use std::time::Duration;

        let sched = VirtualThreadScheduler::new(1);
        let sched2 = sched.clone();

        // Acquire the only permit
        sched.acquire();

        let acquired = Arc::new(AtomicBool::new(false));
        let acquired2 = acquired.clone();

        // Spawn a thread that tries to acquire — should block
        let handle = thread::spawn(move || {
            sched2.acquire();
            acquired2.store(true, Ordering::SeqCst);
            sched2.release();
        });

        // Give the other thread time to start blocking
        thread::sleep(Duration::from_millis(50));
        assert!(!acquired.load(Ordering::SeqCst), "should still be blocked");

        // Release our permit — the other thread should wake up
        sched.release();
        handle.join().unwrap();
        assert!(acquired.load(Ordering::SeqCst), "should have acquired");
    }
}
