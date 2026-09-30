// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP4.3 — `java.util.concurrent.ForkJoinPool` infrastructure.
//!
//! Provides a process-wide work-stealing pool that the native overrides in
//! `native-builtins/src/concurrent_extras.rs` and
//! `native-builtins/src/phases_early.rs` can submit tasks to. The pool's
//! workers cannot directly `invoke_virtual` Java methods (the
//! [`NativeContext`] trait is not `Send`), so the public API mirrors the
//! single-threaded eager-compute semantics that the existing native
//! callbacks implement: tasks are queued, popped, and a completion-waker
//! is signalled so cross-thread submitters can `block_until_complete`.
//!
//! The unique contribution of this module over the inline pool that lived
//! in `concurrent_extras.rs` is:
//!
//!   * a *named* singleton accessor (`common_pool()`) usable by
//!     `vm-cli`-side benchmark probes,
//!   * statistics counters (`steal_count`, `running_workers`,
//!     `parallelism`) that JFR / `ForkJoinPool.toString()` reads,
//!   * a `shutdown()` path that drains the queue and joins workers
//!     deterministically so probes don't leak background threads.
//!
//! References:
//!
//!   * `apps/fjp_probe/FjpProbe.java` — the WP4.3 acceptance probe.
//!   * `native-builtins/src/concurrent_extras.rs` — the older inline
//!     pool (kept around so it can be wired up to use this module's
//!     singleton; that wiring is queued for the ripple sweep, not done
//!     here so the WP4.4/WP4.5 siblings don't conflict on the same
//!     register-side files).
//!
//! Note on long-array arithmetic: `FjpProbe` reduces over a `long[]` and
//! observed that the operand-stack `Lastore` / `Dastore` instructions
//! lost the value-tag and zeroed the array. That regression is fixed in
//! `vm/src/runtime/interpreter.rs` (search for "WP4.3 fix") at the same
//! time as this module landed.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

/// One unit of work submitted to the pool. Carries an opaque task pointer
/// (the `ObjectRef` of the `ForkJoinTask`, kept as `usize` so it crosses
/// thread boundaries) and a waker condvar that the submitter blocks on.
struct PoolTask {
    /// Opaque task identifier. The worker side does not invoke Java code
    /// directly; the submitter owns the actual `compute()` call.
    task_id: usize,
    /// Completion waker — `(done_flag, condvar)`.
    waker: Arc<(Mutex<bool>, Condvar)>,
}

// SAFETY: `task_id` is a raw `usize` (no pointer dereferenced on the
// worker side); `waker` is already `Arc<(Mutex<bool>, Condvar)>` which
// is itself `Send`+`Sync`. The struct therefore satisfies both.
unsafe impl Send for PoolTask {}
unsafe impl Sync for PoolTask {}

struct PoolState {
    queue: VecDeque<PoolTask>,
    shutdown: bool,
    active: usize,
}

/// Process-wide work-stealing pool. Workers share a single queue guarded
/// by a [`Mutex`] + [`Condvar`]; `submit` enqueues, `worker_loop` dequeues.
pub struct ForkJoinPool {
    inner: Arc<(Mutex<PoolState>, Condvar)>,
    parallelism: usize,
    steal_count: AtomicU64,
    /// Worker handles — drained on `shutdown` so we can deterministically
    /// join them. We cannot keep them inside `PoolState` because joining
    /// requires owning the handle, and the state is shared.
    workers: Mutex<Vec<JoinHandle<()>>>,
}

impl ForkJoinPool {
    /// Create a pool with the given worker count. `parallelism` is clamped
    /// to `[1, 256]` to match the `ForkJoinPool.MAX_CAP` defined by the
    /// JDK. The workers spawn eagerly so the first submission does not
    /// pay the spawn cost.
    pub fn new(parallelism: usize) -> Self {
        let parallelism = parallelism.clamp(1, 256);
        let inner = Arc::new((
            Mutex::new(PoolState {
                queue: VecDeque::new(),
                shutdown: false,
                active: 0,
            }),
            Condvar::new(),
        ));
        let mut handles = Vec::with_capacity(parallelism);
        for i in 0..parallelism {
            let inner2 = inner.clone();
            let handle = std::thread::Builder::new()
                .name(format!("fj-worker-{i}"))
                .spawn(move || Self::worker_loop(inner2))
                .expect("failed to spawn fj-worker");
            handles.push(handle);
        }
        Self {
            inner,
            parallelism,
            steal_count: AtomicU64::new(0),
            workers: Mutex::new(handles),
        }
    }

    fn worker_loop(inner: Arc<(Mutex<PoolState>, Condvar)>) {
        loop {
            let (lock, cv) = &*inner;
            let task = {
                let mut state = lock.lock();
                loop {
                    if state.shutdown && state.queue.is_empty() {
                        return;
                    }
                    if let Some(t) = state.queue.pop_front() {
                        state.active += 1;
                        break t;
                    }
                    cv.wait(&mut state);
                }
            };
            // Worker cannot invoke Java code (NativeContext not Send).
            // The submitter inline-runs `compute()` via `invoke_virtual`
            // and uses this pool only for completion bookkeeping. We
            // therefore signal the waker immediately to unblock the
            // caller-side `block_until_complete`.
            let _ = task.task_id;
            let (mx, cv2) = &*task.waker;
            let mut done = mx.lock();
            *done = true;
            cv2.notify_all();
            drop(done);

            let (lock, _) = &*inner;
            let mut state = lock.lock();
            if state.active > 0 {
                state.active -= 1;
            }
        }
    }

    /// Submit a task and block the caller until a worker has acknowledged
    /// it (or `timeout` expires). Returns `true` if the worker signalled
    /// completion before the deadline.
    pub fn submit_blocking(&self, task_id: usize, timeout: Option<Duration>) -> bool {
        let waker = Arc::new((Mutex::new(false), Condvar::new()));
        let task = PoolTask {
            task_id,
            waker: waker.clone(),
        };
        {
            let (lock, cv) = &*self.inner;
            let mut state = lock.lock();
            state.queue.push_back(task);
            cv.notify_one();
        }
        self.steal_count.fetch_add(1, Ordering::Relaxed);
        let (mx, cv2) = &*waker;
        let mut done = mx.lock();
        match timeout {
            Some(d) => {
                let deadline = Instant::now() + d;
                while !*done {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        break;
                    }
                    cv2.wait_for(&mut done, remaining);
                }
            }
            None => {
                while !*done {
                    cv2.wait(&mut done);
                }
            }
        }
        *done
    }

    /// Number of workers reported by `ForkJoinPool.getParallelism()`.
    #[inline]
    pub fn parallelism(&self) -> usize {
        self.parallelism
    }

    /// Cumulative steal count reported by `ForkJoinPool.getStealCount()`.
    /// We surface the submission count as a proxy because the single-queue
    /// model has no genuine steal events.
    #[inline]
    pub fn steal_count(&self) -> u64 {
        self.steal_count.load(Ordering::Relaxed)
    }

    /// Number of currently-running workers (popped a task, not yet done).
    pub fn running_workers(&self) -> usize {
        let (lock, _) = &*self.inner;
        let state = lock.lock();
        state.active
    }

    /// Number of submissions still queued.
    pub fn queued(&self) -> usize {
        let (lock, _) = &*self.inner;
        let state = lock.lock();
        state.queue.len()
    }

    /// Drain the queue and join every worker. Blocks until all workers
    /// observe the `shutdown` flag and exit; idempotent — calling
    /// `shutdown` twice is safe.
    pub fn shutdown(&self) {
        {
            let (lock, cv) = &*self.inner;
            let mut state = lock.lock();
            state.shutdown = true;
            state.queue.clear();
            cv.notify_all();
        }
        let mut handles = self.workers.lock();
        let drained: Vec<_> = handles.drain(..).collect();
        drop(handles); // release the lock before joining
        for h in drained {
            // We deliberately swallow `JoinHandle::join` errors — a
            // worker that panicked has already torn its own thread
            // down and there's nothing useful to do here.
            let _ = h.join();
        }
    }
}

/// Lazy singleton common pool. Sized like the JDK's: `max(1, cpus - 1)`
/// clamped to `[1, 4]` so the build doesn't spawn dozens of threads on
/// CI machines.
pub fn common_pool() -> &'static ForkJoinPool {
    static POOL: OnceLock<ForkJoinPool> = OnceLock::new();
    POOL.get_or_init(|| {
        let cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        let parallelism = cpus.saturating_sub(1).clamp(1, 4);
        ForkJoinPool::new(parallelism)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_pool_singleton() {
        let p1 = common_pool() as *const _;
        let p2 = common_pool() as *const _;
        assert_eq!(p1, p2);
        assert!(common_pool().parallelism() >= 1);
    }

    #[test]
    fn submit_blocking_returns_quickly() {
        let pool = ForkJoinPool::new(2);
        let ok = pool.submit_blocking(0xdeadbeef_usize, Some(Duration::from_millis(500)));
        assert!(ok, "worker should have signalled completion");
        // steal_count should reflect at least the one submission we made.
        assert!(pool.steal_count() >= 1);
        pool.shutdown();
    }

    #[test]
    fn shutdown_is_idempotent() {
        let pool = ForkJoinPool::new(2);
        pool.shutdown();
        pool.shutdown();
        // Workers vec is drained, second call is a no-op.
        assert_eq!(pool.queued(), 0);
    }

    #[test]
    fn parallelism_is_clamped() {
        let p1 = ForkJoinPool::new(0);
        assert_eq!(p1.parallelism(), 1);
        p1.shutdown();
        let p2 = ForkJoinPool::new(1024);
        assert_eq!(p2.parallelism(), 256);
        p2.shutdown();
    }

    #[test]
    fn queued_reflects_in_flight_submissions() {
        let pool = ForkJoinPool::new(1);
        // Submit and block until done — queue should be empty after.
        let ok = pool.submit_blocking(1, Some(Duration::from_millis(500)));
        assert!(ok);
        // At steady state the worker has drained.
        assert_eq!(pool.queued(), 0);
        pool.shutdown();
    }
}
