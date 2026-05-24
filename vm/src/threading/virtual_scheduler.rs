// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Virtual thread scheduler (JEP 444, Java 21).
//!
//! Implements a simplified virtual thread model: virtual threads are still
//! backed by OS threads, but their concurrency is bounded by a carrier-thread
//! semaphore. When a virtual thread blocks (sleep, park, wait, contended lock),
//! it releases its carrier permit so another virtual thread can run.
//!
//! The carrier count defaults to the number of available hardware threads.

use std::sync::Arc;

use parking_lot::{Condvar, Mutex};

/// Manages carrier-thread permits for virtual threads.
///
/// Virtual threads must acquire a permit before executing and release it
/// when blocking. This bounds the number of concurrently running virtual
/// threads to `carrier_count`, matching the JDK's ForkJoinPool behavior.
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
    pub fn release(&self) {
        let mut state = self.state.lock();
        state.available += 1;
        self.condvar.notify_one();
    }

    /// The total number of carrier threads.
    pub fn carrier_count(&self) -> usize {
        self.carrier_count
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
