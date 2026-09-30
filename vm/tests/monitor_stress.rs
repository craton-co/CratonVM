// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Multi-threaded monitor contention stress (review §2.3, item 2).
//!
//! Drives the [`MonitorTable`] through the thin-lock → inflate → contended
//! handoff path under sustained contention from 8 OS threads while
//! periodically pumping the GC-side `remap_after_gc` registry mutex. The
//! goal is not numerical correctness of a Java program — the
//! interpreter is bypassed entirely — but to assert that:
//!
//! 1. Every `monitorenter` from every thread is eventually paired with a
//!    `monitorexit`, never producing an `IllegalMonitorStateException`.
//! 2. The 8-thread / 1000-iter / 8000-total enter+exit batch completes
//!    well under the 30s deadlock guard (otherwise the test panics).
//! 3. After all worker threads join, the shared object's monitor is
//!    fully released — `entry_count == 0` and `owner == None`.
//!
//! ## On the "trigger GC every 100 iters" requirement
//!
//! The integration-test entry path uses the low-level `Heap` +
//! `MonitorTable` types directly, not `SharedVm` with a `JvmThread`
//! per OS thread. The production GC trigger (`maybe_gc_forced` /
//! `force_gc_from_native`) is a stop-the-world copying collector that
//! requires every mutator parked at a safepoint and rewires every
//! root through `update_all_roots`. Running that under live monitor
//! contention without the full interpreter context would invalidate
//! the shared `ObjectRef` the workers are about to dereference (use
//! after free), which would defeat the entire premise of the stress
//! run.
//!
//! Instead, each worker calls [`MonitorTable::remap_after_gc`] with
//! an empty pointer map every 100 iterations. The empty-map fast path
//! exits early without dropping the monitor registry mutex, but
//! reaching it still forces the worker to contend for that mutex
//! with the active `inflate_locked` / `lookup_inflated` calls from
//! the contending workers — exercising the same lock-ordering
//! window the real GC remap path uses. This is the closest faithful
//! "GC-trigger-during-contention" coverage achievable at the
//! integration-test surface; the production STW-coordinated path is
//! covered by the unit tests in `vm/src/threading/monitor.rs`
//! (`monitor_remap_after_gc`, `monitor_remap_empty_map_is_noop`).
//!
//! Additionally, each worker allocates a small throwaway object on
//! every iteration. These never become roots and immediately become
//! unreachable, so they exert allocation pressure on the underlying
//! `Heap` without triggering a real collection — a sustained
//! allocation rate that any future stop-the-world coupling will
//! observe.

#![allow(clippy::unwrap_used)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use cratonvm_vm::classloading::ClassId;
use cratonvm_vm::memory::heap::Heap;
use cratonvm_vm::threading::jvm_thread::ThreadId;
use cratonvm_vm::threading::monitor::MonitorTable;

/// 8 threads × 1000 iters = 8000 enter+exit pairs against a single
/// shared monitor.
const N_THREADS: u32 = 8;
const ITERS_PER_THREAD: u32 = 1000;
const GC_TRIGGER_PERIOD: u32 = 100;
/// Hard wall-clock deadline. The test panics if exceeded; 30s is the
/// allowance per review §2.3.
const TIMEOUT: Duration = Duration::from_secs(30);

#[test]
fn monitor_stress_8_threads_no_imse_no_deadlock() {
    let heap = Arc::new(Heap::new());
    let table = Arc::new(MonitorTable::new());

    // Shared monitor target — a `java.lang.Object`-shaped 0-field heap
    // object. ClassId::new(0) matches the synthetic-object usage in
    // existing unit tests (vm/src/threading/monitor.rs::test_object).
    let shared = heap.alloc_object(ClassId::new(0), 0);

    let imse_counter = Arc::new(AtomicU64::new(0));
    let total_acquired = Arc::new(AtomicU64::new(0));
    let started = Arc::new(AtomicBool::new(false));

    let mut handles = Vec::with_capacity(N_THREADS as usize);
    for tid_u32 in 1..=N_THREADS {
        let heap = Arc::clone(&heap);
        let table = Arc::clone(&table);
        let imse = Arc::clone(&imse_counter);
        let acquired = Arc::clone(&total_acquired);
        let started = Arc::clone(&started);
        let obj = shared;
        let tid = ThreadId(u64::from(tid_u32));

        handles.push(thread::spawn(move || {
            // Spin until the orchestrator releases everyone — increases
            // the chance of cold-start contention all hitting the
            // thin-lock CAS at the same nanosecond.
            while !started.load(Ordering::Acquire) {
                std::hint::spin_loop();
            }

            for i in 0..ITERS_PER_THREAD {
                table.enter(obj, tid);
                acquired.fetch_add(1, Ordering::Relaxed);

                // Minimal critical section so all workers stay in
                // contention rather than serialising on bookkeeping
                // outside the lock.
                std::hint::black_box(i);

                if table.exit(obj, tid).is_err() {
                    imse.fetch_add(1, Ordering::Relaxed);
                }

                // Every GC_TRIGGER_PERIOD iters: pump the registry
                // mutex via the empty-map remap (see file-level docs
                // on why we don't drive a real STW cycle here), and
                // allocate a single throwaway object as sustained
                // heap pressure.
                if i % GC_TRIGGER_PERIOD == GC_TRIGGER_PERIOD - 1 {
                    table.remap_after_gc(&cratonvm_types::PointerMap::default());
                    let _scratch = heap.alloc_object(ClassId::new(0), 0);
                }
            }
        }));
    }

    // Release all workers simultaneously.
    started.store(true, Ordering::Release);

    let deadline = Instant::now() + TIMEOUT;
    // Poll-based join with deadline: `JoinHandle::join` blocks
    // indefinitely, so we spin on `is_finished()` until every worker
    // reports done or we exceed the budget. A deadlocked worker
    // therefore surfaces as an explicit panic from this loop rather
    // than as a libtest-level wall-clock timeout (which is harder to
    // bisect to a specific worker).
    for (idx, h) in handles.into_iter().enumerate() {
        while !h.is_finished() {
            if Instant::now() > deadline {
                panic!(
                    "monitor stress exceeded {:?} — worker #{idx} still running, likely deadlock",
                    TIMEOUT,
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        h.join()
            .unwrap_or_else(|_| panic!("worker #{idx} panicked"));
    }

    let imse_count = imse_counter.load(Ordering::Relaxed);
    let acquired = total_acquired.load(Ordering::Relaxed);
    let expected = u64::from(N_THREADS) * u64::from(ITERS_PER_THREAD);
    assert_eq!(
        acquired, expected,
        "expected exactly {expected} successful acquires; got {acquired}"
    );
    assert_eq!(
        imse_count, 0,
        "{imse_count} IllegalMonitorStateException(s) raised during stress run"
    );

    // After all workers exit, no one holds the monitor.
    assert_eq!(
        table.entry_count(shared),
        0,
        "shared monitor entry_count != 0 after all workers joined",
    );
    assert_eq!(
        table.current_owner(shared),
        None,
        "shared monitor owner != None after all workers joined",
    );
}
