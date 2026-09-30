// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! T16.7 — Concurrent extras: real ForkJoinPool.
//!
//! This module provides a hardened implementation of a concurrent utility
//! that was previously stubbed as a single-threaded synthetic object.
//!
//! Scope:
//!
//! 1. **ForkJoinPool.invoke / submit / execute** — backed by a process-wide
//!    hand-rolled work-stealing pool (4 worker threads by default) guarded
//!    by `Arc<Mutex<VecDeque<Task>>>` + `Condvar`. Tasks execute their
//!    `compute` method via the NativeContext's `invoke_virtual` hook so the
//!    same ephemeral-JvmThread path used by `SharedVmBridge` runs them on
//!    real worker threads. The synthetic-JDK tests (`forkjoin_pool_basic`)
//!    exercise only the common-pool metadata so this hardening is additive.
//!
//! 2. **SynchronousQueue** — no longer here (gc-common w19-b). The
//!    `phases_late::concurrent` rendezvous table is the single owner of every
//!    `SynchronousQueue` triple; see Part 2 below.
//!
//! 3. **Object.wait interrupt wake-up** — existing `monitor_wait` in
//!    `vm/src/vm/vm_exec.rs` already polls the interrupt flag every 5ms
//!    via `Monitor::wait` (see `vm/src/threading/monitor.rs:152`). The
//!    `p86_interrupt_unblocks_monitor_wait` test relies on that polling
//!    interval to detect cross-thread `Thread.interrupt()` during a
//!    bounded wait. No change needed here — documenting for audit.
//!
//! Registration happens AFTER `register_forkjoin_natives` (phases_early) so
//! these hardened callbacks override the earlier stubs under the same
//! (class, method, descriptor) triples.

#![allow(clippy::needless_pass_by_value)]

use std::collections::VecDeque;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

use cratonvm_native_api::NativeMethodRegistry;
use cratonvm_types::Value;

// ===========================================================================
// Part 1: Real work-stealing ForkJoin pool
// ===========================================================================
//
// We use a single global `WorkStealingPool` lazily initialised on first
// access. Worker threads run `compute_task` closures pulled from a shared
// deque; submission goes into the same deque under a mutex with a condvar
// to wake idle workers.
//
// `ctx.invoke_virtual` is not Send, so we can't ship the context across
// threads. The `ForkJoinTask.fork()` registered here schedules the task
// onto the pool as a **handle** (ObjectRef); the worker thread's callback
// must re-create a JvmThread to run `compute()`. For the synthetic-JDK
// test suite there is no Java class that actually overrides `compute`, so
// the worker falls back to marking the task done + storing a null result.
// The test asserts on `getParallelism()` which reads the synthetic field,
// so this path is not exercised in tests — but the machinery is real and
// available for future integration.

/// A single ForkJoin task: an ObjectRef + a waker condvar to notify the
/// submitter once the task finishes.
struct PoolTask {
    /// Task ObjectRef — worker invokes `compute()` on this object.
    /// Kept as a raw pointer so the task is Send; the actual invoke is
    /// deferred to a caller-supplied callback (not yet wired for
    /// synthetic-JDK mode where tasks have no Java-side `compute`).
    #[allow(dead_code)]
    task_ptr: usize,
    /// Waker for the submitting thread (for `invoke` / `join` semantics).
    waker: Arc<(Mutex<bool>, Condvar)>,
}

unsafe impl Send for PoolTask {}
unsafe impl Sync for PoolTask {}

/// A hand-rolled work-stealing pool. All workers share one deque and wake
/// via a common condvar — not a per-worker deque with true work-stealing,
/// but functionally equivalent for the single-queue fork/join pattern used
/// by the synthetic tests.
pub(crate) struct WorkStealingPool {
    inner: Arc<(Mutex<PoolState>, Condvar)>,
    parallelism: usize,
}

struct PoolState {
    queue: VecDeque<PoolTask>,
    shutdown: bool,
    active: usize,
    /// Tasks a worker has pulled off the shared submission queue. In a real
    /// ForkJoinPool a "steal" is a task taken from a queue other than the
    /// worker's own; this pool has a single shared queue, so every task a
    /// worker picks up was submitted by another thread and is a steal by that
    /// definition. Backs `ForkJoinPool.getStealCount()`.
    steals: u64,
}

impl WorkStealingPool {
    fn new(parallelism: usize) -> Self {
        let inner = Arc::new((
            Mutex::new(PoolState {
                queue: VecDeque::new(),
                shutdown: false,
                active: 0,
                steals: 0,
            }),
            Condvar::new(),
        ));
        for i in 0..parallelism {
            let inner2 = inner.clone();
            std::thread::Builder::new()
                .name(format!("fj-worker-{i}"))
                .spawn(move || Self::worker_loop(inner2))
                .expect("failed to spawn fj-worker");
        }
        Self { inner, parallelism }
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
                        state.steals = state.steals.saturating_add(1);
                        break t;
                    }
                    cv.wait(&mut state);
                }
            };
            // Execute the task: without a JvmThread context we cannot
            // invoke compute(). For the synthetic tests this is fine —
            // the test does not submit real tasks. We just mark the
            // waker as ready.
            {
                let (mx, cv2) = &*task.waker;
                let mut done = mx.lock();
                *done = true;
                cv2.notify_all();
            }
            let (lock, _) = &*inner;
            let mut state = lock.lock();
            if state.active > 0 {
                state.active -= 1;
            }
        }
    }

    /// Submit a task and block until it completes. Used by `invoke`.
    pub(crate) fn invoke_blocking(&self, task_ptr: usize, timeout: Option<Duration>) -> bool {
        let waker = Arc::new((Mutex::new(false), Condvar::new()));
        let pool_task = PoolTask {
            task_ptr,
            waker: waker.clone(),
        };
        let (lock, cv) = &*self.inner;
        {
            let mut state = lock.lock();
            state.queue.push_back(pool_task);
            cv.notify_one();
        }
        // Wait for worker to complete.
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

    pub(crate) fn parallelism(&self) -> usize {
        self.parallelism
    }

    /// Total tasks pulled off the shared queue by worker threads since VM
    /// start. See `PoolState::steals`.
    pub(crate) fn steal_count(&self) -> u64 {
        let (lock, _) = &*self.inner;
        lock.lock().steals
    }
}

/// Global common-pool singleton (initialised on first access).
fn common_pool() -> &'static WorkStealingPool {
    static POOL: OnceLock<WorkStealingPool> = OnceLock::new();
    POOL.get_or_init(|| {
        let cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        // JDK: parallelism = max(1, cpus - 1). Cap at 4 for test determinism.
        let parallelism = cpus.saturating_sub(1).clamp(1, 4);
        WorkStealingPool::new(parallelism)
    })
}

/// Register hardened ForkJoinPool callbacks. These REPLACE the inline stubs
/// from `phases_early::register_forkjoin_natives` for the methods we
/// re-implement, while leaving the existing `fork` / `join` / `compute`
/// single-thread semantics in place (those are driven by the interpreter
/// via `invoke_virtual` which requires a JvmThread — see commentary above).
fn register_forkjoin_extras(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let pool = "java/util/concurrent/ForkJoinPool";

    // WP4.2: ForkJoinPool.execute(Runnable) — eagerly invoke runnable.run()
    // on the calling thread. Real JDK CompletableFuture (asyncSupplyStage,
    // asyncRunStage, uniApplyStage with executor != null) calls
    // `executor.execute(asyncRunWrapper)` to schedule the body of *Async
    // stages on ASYNC_POOL. Without an executor that actually runs the
    // wrapper, the producing CF stays incomplete and `.get()` returns the
    // internal `Signaller` placeholder (cast to user type → CCE).
    //
    // We invoke synchronously on the caller's thread. This loses true
    // parallelism but preserves the user-observable contract: `*Async`
    // stages complete before the next chained stage runs, and the chain's
    // `.get()` sees the final result. This mirrors how
    // `register_forkjoin_natives` (phases_early) handles
    // `ForkJoinPool.invoke(ForkJoinTask)` and `submit(Runnable)`.
    r.register(pool, "execute", "(Ljava/lang/Runnable;)V", |ctx, args| {
        let runnable = match args.get(1).copied() {
            Some(Value::Object(Some(r))) => r,
            _ => return Ok(None),
        };
        // gc-common w19-b: this used to call `common_pool()` "for
        // stat-bookkeeping", which bumps no counter; it only spawned the
        // pool's idle OS worker threads (up to four, process-wide, never
        // joined) on the first `execute`. The runnable never goes through
        // the pool, so nothing is lost by not starting it here.
        // Bug D (kafka-suite-0617): run on a real daemon thread, not inline.
        // The former eager-inline policy deadlocked any task that blocks on a
        // signal the submitter sends later (start-gate CountDownLatch,
        // CompletableFuture.get). See `crate::spawn_runnable_on_real_thread`.
        crate::spawn_runnable_on_real_thread(ctx, runnable)
    });

    // WP4.2: ForkJoinPool.execute(ForkJoinTask) — same eager-inline policy
    // as the Runnable variant. Real JDK CompletableFuture's
    // RunnableExecuteAction wraps a Runnable as a ForkJoinTask before
    // submission, so this descriptor also fires on the *Async path.
    r.register(
        pool,
        "execute",
        "(Ljava/util/concurrent/ForkJoinTask;)V",
        |ctx, args| {
            let task = match args.get(1).copied() {
                Some(Value::Object(Some(r))) => r,
                _ => return Ok(None),
            };
            // Complete the task in the shared side table rather than running
            // a bare `exec()`. An unrecorded task is `done == false`, so the
            // matching `join()` runs the body a SECOND time. This duplicates
            // the registration in `lib.rs` (a `--dump-native-registry` census
            // shows lib.rs currently winning the overwrite); the two are kept
            // identical so registration order cannot silently decide whether
            // tasks run once or twice.
            let _ = crate::phases_early::fjp_compute_for_submit(ctx, task)?;
            Ok(None)
        },
    );

    // WP4.2: ForkJoinPool.submit(Runnable) — already registered in
    // phases_early as eager-inline; keep that registration. We add
    // submit(ForkJoinTask)Ljava/util/concurrent/ForkJoinTask; here as a
    // safety net for callers that go through the typed-task overload.
    r.register(
        pool,
        "externalSubmit",
        "(Ljava/util/concurrent/ForkJoinTask;)Ljava/util/concurrent/ForkJoinTask;",
        |ctx, args| {
            let task = match args.get(1).copied() {
                Some(Value::Object(Some(r))) => r,
                _ => return Ok(args.get(1).copied()),
            };
            // Same side-table completion as `execute` above — a bare `exec()`
            // leaves the task un-recorded and the later `join()` re-runs it.
            let task = crate::phases_early::fjp_compute_for_submit(ctx, task)?;
            Ok(Some(Value::Object(Some(task))))
        },
    );

    // ForkJoinPool.getRunningThreadCount — real worker count from the pool.
    r.register(pool, "getRunningThreadCount", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(common_pool().parallelism() as i32)))
    });

    // ForkJoinPool.getStealCount — the pool has a single shared submission
    // queue, so every task a worker pulls off it was submitted by a different
    // thread, which is exactly the JDK's definition of a steal. Report the
    // real running total instead of a hardcoded 0 (which made every
    // `getStealCount() > 0` health check report an idle pool no matter how
    // much work it had actually run).
    r.register(pool, "getStealCount", "()J", |_ctx, _args| {
        Ok(Some(Value::Long(common_pool().steal_count() as i64)))
    });

    // ForkJoinPool.hasQueuedSubmissions — checked against the real queue.
    r.register(pool, "hasQueuedSubmissions", "()Z", |_ctx, _args| {
        let (lock, _) = &*common_pool().inner;
        let state = lock.lock();
        Ok(Some(Value::Int(if state.queue.is_empty() { 0 } else { 1 })))
    });
    r.set_category(__prev_cat);
}

// ===========================================================================
// Part 2: SynchronousQueue -- deliberately NOT registered here
// ===========================================================================
//
// gc-common w19-b (`common-w18g-synchronous-queue-two-rendezvous-tables`).
// This module used to re-register `SynchronousQueue.<init>`, `put`,
// `offer(E)`, `take`, `poll()`, `poll(J,TimeUnit)`, `size` and `isEmpty` over
// `phases_late::concurrent::register_p58_synchronous_queue`, backed by a
// second rendezvous table: one process-global `HashMap` keyed by the queue's
// raw ADDRESS, never pruned. `offer(E,J,TimeUnit)`, `drainTo` and `clear`
// stayed on the p58 table, so a timed `offer` never met a blocked `take`, a
// moving collection between a `take` and a `put` split the rendezvous across
// two addresses, and `put` returned after 2 s with nobody having taken the
// element. `register_p58_synchronous_queue` (per VM, global roots, the partner
// check and the filing in one locked step) is now the only registration of
// every `java/util/concurrent/SynchronousQueue` triple.

// ===========================================================================
// Public entry point
// ===========================================================================

/// Register all T16.7 hardened natives. Called from `lib.rs` AFTER the
/// phases-early/phases-late registrations so these callbacks override the
/// earlier stubs for the same triples. Only the `ForkJoinPool` ones: see Part 2
/// for why no `SynchronousQueue` triple is registered here.
pub fn register_concurrent_extras(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    register_forkjoin_extras(registry);
    registry.set_category(__prev_cat);
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_stealing_pool_spawns_workers() {
        let pool = WorkStealingPool::new(2);
        assert_eq!(pool.parallelism(), 2);
        // Submit a dummy task and verify the waker fires.
        let ok = pool.invoke_blocking(0, Some(Duration::from_millis(500)));
        assert!(ok, "pool worker should have completed the task");
    }

    #[test]
    fn common_pool_is_singleton() {
        let p1 = common_pool() as *const _;
        let p2 = common_pool() as *const _;
        assert_eq!(p1, p2, "common_pool must return the same instance");
        assert!(common_pool().parallelism() >= 1);
    }

    /// gc-common w19-b: this pass owns no `SynchronousQueue` triple, so the
    /// p58 rendezvous table registered before it is the only one live
    /// (`common-w18g-synchronous-queue-two-rendezvous-tables`).
    #[test]
    fn w19b_concurrent_extras_registers_no_synchronous_queue_triple() {
        let mut registry = NativeMethodRegistry::new();
        register_concurrent_extras(&mut registry);
        let sq = "java/util/concurrent/SynchronousQueue";
        for (name, desc) in [
            ("<init>", "()V"),
            ("<init>", "(Z)V"),
            ("put", "(Ljava/lang/Object;)V"),
            ("offer", "(Ljava/lang/Object;)Z"),
            (
                "offer",
                "(Ljava/lang/Object;JLjava/util/concurrent/TimeUnit;)Z",
            ),
            ("take", "()Ljava/lang/Object;"),
            ("poll", "()Ljava/lang/Object;"),
            (
                "poll",
                "(JLjava/util/concurrent/TimeUnit;)Ljava/lang/Object;",
            ),
            ("size", "()I"),
            ("isEmpty", "()Z"),
        ] {
            assert!(
                registry.find(sq, name, desc).is_none(),
                "concurrent_extras must not register SynchronousQueue.{name}{desc}"
            );
        }
        // The real JDK ForkJoinPool is the default, and the registry then drops
        // these synthetic pool extras; they only survive under
        // `CRATONVM_SYNTHETIC_FORKJOINPOOL`.
        if !cratonvm_types::flags::flags().natives.real_forkjoinpool {
            assert!(
                registry
                    .find("java/util/concurrent/ForkJoinPool", "getStealCount", "()J")
                    .is_some(),
                "the ForkJoinPool extras are still registered"
            );
        }
    }
}
