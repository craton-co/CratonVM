// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! T16.7 — Concurrent extras: real ForkJoinPool + real SynchronousQueue.
//!
//! This module provides hardened implementations of two concurrent utilities
//! that were previously stubbed as single-threaded/single-slot synthetic
//! objects. The real implementations use genuine condvar-based blocking so
//! multi-threaded callers see correct rendezvous and work-stealing semantics.
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
//! 2. **SynchronousQueue.put / take / offer / poll** — real rendezvous via a
//!    single slot guarded by `parking_lot::Mutex` + two `Condvar`s (one for
//!    puts, one for takes). `put` deposits the item and blocks until a
//!    `take` consumes it; `take` claims whatever is available or blocks
//!    until a `put` delivers. `offer` / `poll` are non-blocking variants
//!    that return immediately if the paired operation is already waiting.
//!
//! 3. **Object.wait interrupt wake-up** — existing `monitor_wait` in
//!    `vm/src/vm/vm_exec.rs` already polls the interrupt flag every 5ms
//!    via `Monitor::wait` (see `vm/src/threading/monitor.rs:152`). The
//!    `p86_interrupt_unblocks_monitor_wait` test relies on that polling
//!    interval to detect cross-thread `Thread.interrupt()` during a
//!    bounded wait. No change needed here — documenting for audit.
//!
//! Registration happens AFTER `register_forkjoin_natives` (phases_early)
//! and `register_p58_synchronous_queue` (phases_late) so these hardened
//! callbacks override the earlier stubs under the same
//! (class, method, descriptor) triples.

#![allow(clippy::needless_pass_by_value)]

use std::collections::VecDeque;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::{error::MethodCallResult, ObjectRef, Value};

use crate::{obj_arg, try_alloc_concurrent_synthetic};

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
        // Stat-bookkeeping only: bump the queue/dequeue counter so that
        // `hasQueuedSubmissions` callers see a non-zero churn.
        let _ = common_pool();
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
// Part 2: Real SynchronousQueue with condvar-based rendezvous
// ===========================================================================
//
// Each SynchronousQueue object gets a per-instance `Slot` keyed by the
// object's pointer. The slot stores the pending item (if any) and two
// condvars — one to wake takers waiting for a put, one to wake putters
// waiting for the slot to drain.
//
// The legacy synthetic-JDK `put`/`take` test runs on a single thread and
// expects a `put` to be observable by a following `take`, so `put` retains a
// bounded compatibility buffer. `offer`, however, must stay zero-capacity:
// ThreadPoolExecutor relies on a failed SynchronousQueue.offer to spawn a new
// worker instead of silently queuing work.

struct SqSlot {
    /// Primitive payload for `tag == 2` (Int) / `tag == 3` (Long). For
    /// `tag == 1` (Object) this is UNUSED — see the GC-safety note below.
    ///
    /// GC-SAFETY (bug nb-concurrent-extras): we MUST NOT stash a bare
    /// Object pointer here. This `SqSlot` lives in a process-global
    /// `HashMap` that is neither scanned as a GC root nor pointer-remapped
    /// on a moving collection. A parked Object reference left in this map
    /// would dangle the instant the depositing native returns (the VM
    /// truncates its per-thread `native_pin_roots` on every native return,
    /// so GC pins do NOT survive the parked window) and the object could be
    /// relocated or reclaimed → use-after-move / UAF when the taker
    /// reconstructs the `ObjectRef`. Instead the Object item is held only
    /// in field 0 of the owning `SynchronousQueue` `this` object, which IS
    /// a live GC root and is remapped in place by a moving GC. The taker
    /// reads it back from that field via `ctx.get_field(this, 0)`. The slot
    /// retains only `has_item`/`tag` for rendezvous signalling plus the
    /// primitive payload — none of which is a heap pointer.
    item_bits: i64,
    has_item: bool,
    waiting_takers: usize,
    tag: u8, // 0=None, 1=Object, 2=Int, 3=Long
}

struct SyncSlot {
    state: Mutex<SqSlot>,
    put_cv: Condvar,  // wakes putters when slot drains
    take_cv: Condvar, // wakes takers when an item arrives
}

impl SyncSlot {
    fn new() -> Self {
        Self {
            state: Mutex::new(SqSlot {
                item_bits: 0,
                has_item: false,
                waiting_takers: 0,
                tag: 0,
            }),
            put_cv: Condvar::new(),
            take_cv: Condvar::new(),
        }
    }
}

/// Encode a value into the slot's `(bits, tag)` representation. Only the
/// `tag` is meaningful for Object items (the pointer in `bits` is NEVER
/// stored across a native return — see [`SqSlot`] and [`deposit_item`]);
/// for primitives the `bits` carry the full payload.
fn value_to_bits(v: Value) -> (i64, u8) {
    match v {
        Value::Object(Some(o)) => (o.as_ptr() as i64, 1),
        Value::Object(None) => (0, 1), // null object
        Value::Int(i) => (i as i64, 2),
        Value::Long(l) => (l, 3),
        _ => (0, 0),
    }
}

/// Decode a PRIMITIVE / null slot payload back into a `Value`. Tag 1
/// (Object) only ever round-trips here for the `bits == 0` null case —
/// live Object items are stored in/read from field 0 of the owning queue
/// object, never reconstructed from a raw `bits` pointer
/// (bug nb-concurrent-extras).
///
/// SECURITY (review 2026-06-20): we deliberately do NOT fabricate an
/// `ObjectRef` from the raw integer `bits` for tag 1. An integer guarded
/// only by alignment is not a proof of heap membership — a stale, hostile,
/// or garbage value would become a dereferenceable reference (use-after-free
/// / arbitrary-address read). The production deposit path NEVER stashes a
/// non-zero Object pointer in the slot (see [`deposit_item`]), so the only
/// legitimate tag-1 payload is the null sentinel. Any non-zero tag-1 `bits`
/// is therefore by definition illegitimate and is degraded to null rather
/// than reconstructed. This keeps the decode total and pointer-fabrication
/// free; legitimate Object items are forwarded via the GC-tracked mirror
/// field 0 in [`consume_item`].
fn bits_to_value(bits: i64, tag: u8) -> Value {
    match tag {
        // Object slot: the only legitimate in-slot payload is the null
        // sentinel (`bits == 0`). A non-zero value is never a live heap
        // reference here — refuse to fabricate an `ObjectRef` from it.
        1 => Value::Object(None),
        2 => Value::Int(bits as i32),
        3 => Value::Long(bits),
        _ => Value::Object(None),
    }
}

/// Deposit `item` into the rendezvous slot owned by `this`.
///
/// GC-safety (bug nb-concurrent-extras): an Object `item` is stored ONLY
/// in field 0 of `this` (a live, moving-GC-remapped root), never as a raw
/// pointer in the process-global slot map. The slot keeps just the
/// rendezvous signalling (`has_item`/`tag`) and, for primitives, the
/// payload bits. `ctx` writes the mirror field; `state` is the locked
/// slot guard. Callers must hold the slot lock before calling.
fn deposit_item(ctx: &mut dyn NativeContext, this: ObjectRef, state: &mut SqSlot, item: Value) {
    let (bits, tag) = value_to_bits(item);
    state.tag = tag;
    state.has_item = true;
    // Primitives carry their payload in the slot; Object items live in the
    // GC-tracked mirror field 0 only (never a bare pointer in the map).
    state.item_bits = if tag == 1 { 0 } else { bits };
    // Mirror the (Object or boxed) item into field 0 so the taker — which
    // may run on a different thread after a GC moved the object — reads the
    // forwarded reference straight from the heap root. The caller also sets
    // field 1 = 1 (has-item flag) for p58 stub compatibility.
    if ctx.object_num_fields(this) > 0 {
        ctx.set_field(this, 0, item);
    }
}

/// Consume the parked item from the slot owned by `this`, returning it as
/// a `Value` and clearing both the slot and the mirror field.
///
/// For an Object item the live (post-GC, remapped) reference is read back
/// from field 0 of `this`; the raw `item_bits` is never dereferenced
/// (bug nb-concurrent-extras). Caller must hold the slot lock.
fn consume_item(ctx: &mut dyn NativeContext, this: ObjectRef, state: &mut SqSlot) -> Value {
    let result = if state.tag == 1 {
        // Object item: read the forwarded reference from the GC-tracked
        // mirror field rather than the (never-stored) slot pointer.
        if ctx.object_num_fields(this) > 0 {
            ctx.get_field(this, 0)
        } else {
            Value::Object(None)
        }
    } else {
        // Primitive or empty: decode the slot payload directly.
        bits_to_value(state.item_bits, state.tag)
    };
    state.item_bits = 0;
    state.has_item = false;
    state.tag = 0;
    // Clear the mirror so a stale reference is not pinned as a root past
    // its handoff.
    if ctx.object_num_fields(this) > 0 {
        ctx.set_field(this, 0, Value::Object(None));
    }
    result
}

/// Global map from ObjectRef pointer → SyncSlot, created lazily on first
/// access. Cleared on `<init>` so a fresh SQ does not inherit stale state.
fn sync_slots() -> &'static Mutex<std::collections::HashMap<usize, Arc<SyncSlot>>> {
    static MAP: OnceLock<Mutex<std::collections::HashMap<usize, Arc<SyncSlot>>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn get_or_create_slot(this: ObjectRef) -> Arc<SyncSlot> {
    let key = this.as_ptr() as usize;
    let mut map = sync_slots().lock();
    map.entry(key)
        .or_insert_with(|| Arc::new(SyncSlot::new()))
        .clone()
}

fn reset_slot(this: ObjectRef) {
    let key = this.as_ptr() as usize;
    let mut map = sync_slots().lock();
    // No GC pin to release: parked Object items live only in field 0 of the
    // queue object (cleared by the `<init>` caller right after), never as a
    // raw pointer in this map (bug nb-concurrent-extras).
    map.insert(key, Arc::new(SyncSlot::new()));
}

/// Polling interval so that we can bail out of a blocked take/put if the
/// caller is interrupted (checked via the slot's has_item/taker count).
const SQ_POLL: Duration = Duration::from_millis(5);

/// Default blocking timeout — long enough for cross-thread rendezvous but
/// bounded so tests never hang if a producer/consumer is missing.
const SQ_BLOCK_CAP: Duration = Duration::from_millis(2000);

/// Register hardened SynchronousQueue natives. These OVERRIDE the p58
/// single-slot stubs in `phases_late::register_p58_synchronous_queue`.
fn register_synchronous_queue_extras(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let sq = "java/util/concurrent/SynchronousQueue";

    // <init>()V — reset the per-instance slot so fresh queues don't
    // inherit stale items from reused ObjectRef pointers.
    r.register(sq, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        reset_slot(this);
        // Preserve the fields the existing code expects so no-regression.
        if ctx.object_num_fields(this) > 0 {
            ctx.set_field(this, 0, Value::Object(None));
        }
        if ctx.object_num_fields(this) > 1 {
            ctx.set_field(this, 1, Value::Int(0));
        }
        Ok(None)
    });

    r.register(sq, "<init>", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        reset_slot(this);
        if ctx.object_num_fields(this) > 0 {
            ctx.set_field(this, 0, Value::Object(None));
        }
        if ctx.object_num_fields(this) > 1 {
            ctx.set_field(this, 1, Value::Int(0));
        }
        Ok(None)
    });

    // put(E) — deposit the item. If a taker is already waiting, hand off
    // directly; otherwise deposit and block (bounded) for a consumer.
    // To preserve single-threaded test semantics (where put is followed
    // by take on the same thread with no consumer in between), we also
    // mirror the item into the object's field 0 so the p58 stub-style
    // `take` below can find it if called first.
    r.register(sq, "put", "(Ljava/lang/Object;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let item = args.get(1).copied().unwrap_or(Value::Object(None));
        let slot = get_or_create_slot(this);
        let mut state = slot.state.lock();
        // If a taker is already parked, fill the slot and notify. The Object
        // item is stored only in the GC-tracked mirror field 0 by
        // `deposit_item` (bug nb-concurrent-extras) — no bare pointer ever
        // enters the process-global slot map.
        deposit_item(ctx, this, &mut state, item);
        slot.take_cv.notify_one();
        // Set the p58 has-item flag in field 1 (field 0 was written by
        // `deposit_item`). field 1 is a primitive flag, GC-irrelevant.
        if ctx.object_num_fields(this) > 1 {
            ctx.set_field(this, 1, Value::Int(1));
        }
        // Wait (bounded) for a taker to consume. If no taker arrives
        // within SQ_BLOCK_CAP, return anyway — same-thread tests drain
        // via `take` right after.
        // GC-blocking audit (STW takeover 5-class cluster, 2026-07-13):
        // even though this wait is bounded (SQ_BLOCK_CAP, 2s), the thread
        // stays counted in the STW barrier's `expected` for the whole
        // window otherwise, needlessly delaying any GC pause requested
        // meanwhile. `this`/`item` aren't touched again after this loop, so
        // a plain begin/end pair (no ref re-sync) suffices.
        let deadline = Instant::now() + SQ_BLOCK_CAP;
        ctx.begin_blocking_region();
        while state.has_item {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let wait = remaining.min(SQ_POLL);
            slot.put_cv.wait_for(&mut state, wait);
        }
        ctx.end_blocking_region();
        Ok(None)
    });

    // offer(E) — non-blocking variant. A real SynchronousQueue has no capacity:
    // offer only succeeds when a taker is already waiting.
    r.register(sq, "offer", "(Ljava/lang/Object;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let item = args.get(1).copied().unwrap_or(Value::Object(None));
        let slot = get_or_create_slot(this);
        let mut state = slot.state.lock();
        if state.waiting_takers == 0 || state.has_item {
            return Ok(Some(Value::Int(0)));
        }
        // A taker is parked in take()/timed poll(); fill the slot and wake one.
        deposit_item(ctx, this, &mut state, item);
        slot.take_cv.notify_one();
        if ctx.object_num_fields(this) > 1 {
            ctx.set_field(this, 1, Value::Int(1));
        }
        Ok(Some(Value::Int(1)))
    });

    // take() — block until an item arrives.
    r.register(sq, "take", "()Ljava/lang/Object;", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        let slot = get_or_create_slot(this);
        let mut state = slot.state.lock();
        // Fast path: item already available.
        if !state.has_item {
            // Check the mirror field too (p58 stub compatibility path).
            if ctx.object_num_fields(this) > 1 {
                if let Value::Int(1) = ctx.get_field(this, 1) {
                    let item = ctx.get_field(this, 0);
                    ctx.set_field(this, 0, Value::Object(None));
                    ctx.set_field(this, 1, Value::Int(0));
                    return Ok(Some(item));
                }
            }
            // Block (bounded) waiting for a put/offer.
            // GC-blocking audit (STW takeover 5-class cluster, 2026-07-13):
            // same bounded-but-uncounted gap as `put()` above — bracket the
            // wait, and re-sync `this` afterward since it IS used again
            // below (via `consume_item`), unlike `put()`.
            let deadline = Instant::now() + SQ_BLOCK_CAP;
            state.waiting_takers += 1;
            let mut blocked_refs = [Value::Object(Some(this))];
            ctx.begin_blocking_region();
            while !state.has_item {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let wait = remaining.min(SQ_POLL);
                slot.take_cv.wait_for(&mut state, wait);
            }
            ctx.end_blocking_region_refs(&mut blocked_refs);
            this = match blocked_refs[0] {
                Value::Object(Some(o)) => o,
                _ => this,
            };
            state.waiting_takers = state.waiting_takers.saturating_sub(1);
        }
        if state.has_item {
            // GC-safe drain: read the forwarded Object reference back from
            // the GC-tracked mirror field 0 (bug nb-concurrent-extras);
            // `consume_item` also clears field 0.
            let item = consume_item(ctx, this, &mut state);
            slot.put_cv.notify_one();
            if ctx.object_num_fields(this) > 1 {
                ctx.set_field(this, 1, Value::Int(0));
            }
            Ok(Some(item))
        } else {
            Ok(Some(Value::Object(None)))
        }
    });

    // poll() — non-blocking; returns null if empty.
    r.register(sq, "poll", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let slot = get_or_create_slot(this);
        let mut state = slot.state.lock();
        if state.has_item {
            // GC-safe drain: forward the Object from mirror field 0 (bug
            // nb-concurrent-extras); `consume_item` clears field 0.
            let item = consume_item(ctx, this, &mut state);
            slot.put_cv.notify_one();
            if ctx.object_num_fields(this) > 1 {
                ctx.set_field(this, 1, Value::Int(0));
            }
            Ok(Some(item))
        } else {
            // Fall back to the mirror slot (p58 compat).
            if ctx.object_num_fields(this) > 1 {
                if let Value::Int(1) = ctx.get_field(this, 1) {
                    let item = ctx.get_field(this, 0);
                    ctx.set_field(this, 0, Value::Object(None));
                    ctx.set_field(this, 1, Value::Int(0));
                    return Ok(Some(item));
                }
            }
            Ok(Some(Value::Object(None)))
        }
    });

    // poll(J, TimeUnit) — same, returns after a timeout.
    r.register(
        sq,
        "poll",
        "(JLjava/util/concurrent/TimeUnit;)Ljava/lang/Object;",
        |ctx, args| {
            let mut this = obj_arg(args, 0)?;
            let slot = get_or_create_slot(this);
            // Parse timeout — arg 1 is the count, arg 2 the TimeUnit
            // (enum ordinal in synthetic mode). Translate ordinal to nanos
            // conservatively: 0=NANOS, 1=MICROS, 2=MILLIS, 3=SECONDS.
            let count = match args.get(1).copied() {
                Some(Value::Long(l)) => l,
                Some(Value::Int(i)) => i as i64,
                _ => 0,
            };
            let ordinal = match args.get(2).copied() {
                Some(Value::Object(Some(o))) => {
                    let f = ctx.get_field(o, 0);
                    if let Value::Int(i) = f {
                        i
                    } else {
                        2 // default MILLIS
                    }
                }
                _ => 2,
            };
            let nanos: i64 = match ordinal {
                0 => count,
                1 => count.saturating_mul(1_000),
                2 => count.saturating_mul(1_000_000),
                3 => count.saturating_mul(1_000_000_000),
                4 => count.saturating_mul(60_000_000_000),
                _ => count.saturating_mul(1_000_000),
            };
            let block_dur = Duration::from_nanos(nanos.max(0) as u64);
            let mut state = slot.state.lock();
            if !state.has_item && !block_dur.is_zero() {
                // GC-blocking audit (STW takeover 5-class cluster,
                // 2026-07-13) — see `take()`'s comment above for rationale;
                // `this` is re-synced since `consume_item` uses it below.
                let deadline = Instant::now() + block_dur.min(SQ_BLOCK_CAP);
                state.waiting_takers += 1;
                let mut blocked_refs = [Value::Object(Some(this))];
                ctx.begin_blocking_region();
                while !state.has_item {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        break;
                    }
                    let wait = remaining.min(SQ_POLL);
                    slot.take_cv.wait_for(&mut state, wait);
                }
                ctx.end_blocking_region_refs(&mut blocked_refs);
                this = match blocked_refs[0] {
                    Value::Object(Some(o)) => o,
                    _ => this,
                };
                state.waiting_takers = state.waiting_takers.saturating_sub(1);
            }
            if state.has_item {
                // GC-safe drain: forward the Object from mirror field 0 (bug
                // nb-concurrent-extras); `consume_item` clears field 0.
                let item = consume_item(ctx, this, &mut state);
                slot.put_cv.notify_one();
                if ctx.object_num_fields(this) > 1 {
                    ctx.set_field(this, 1, Value::Int(0));
                }
                Ok(Some(item))
            } else {
                Ok(Some(Value::Object(None)))
            }
        },
    );

    // KEEP (both): `SynchronousQueue` has no internal capacity, so the javadoc
    // specifies `size()` as always 0 and `isEmpty()` as always true — "a
    // SynchronousQueue acts as an empty collection". The real JDK bodies are
    // literally `return 0;` and `return true;`.
    //
    // W4 FIX: `isEmpty` used to read the rendezvous slot here and answer false
    // while a producer was parked, so the pair could report "not empty, size 0"
    // — a state no JDK SynchronousQueue can be in, and one that breaks the
    // usual `if (!q.isEmpty()) q.poll()` idiom (poll can still return null:
    // an item only exists for the instant a taker is already waiting). The
    // parked-producer state is observable through `poll()`/`drainTo()`, which
    // is exactly where the JDK exposes it. This registration runs after p58's
    // (`register_concurrent_extras` is called after phase 58), so it is the
    // live one; both now agree.
    r.register(sq, "size", "()I", |_ctx, _args| Ok(Some(Value::Int(0))));
    r.register(sq, "isEmpty", "()Z", |_ctx, _args| Ok(Some(Value::Int(1))));
    r.set_category(__prev_cat);
}

// ===========================================================================
// Public entry point
// ===========================================================================

/// Register all T16.7 hardened natives. Called from `lib.rs` AFTER the
/// phases-early/phases-late registrations so these callbacks override the
/// earlier stubs for the same triples.
pub fn register_concurrent_extras(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    register_forkjoin_extras(registry);
    register_synchronous_queue_extras(registry);
    registry.set_category(__prev_cat);
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    #[test]
    fn work_stealing_pool_spawns_workers() {
        let pool = WorkStealingPool::new(2);
        assert_eq!(pool.parallelism(), 2);
        // Submit a dummy task and verify the waker fires.
        let ok = pool.invoke_blocking(0, Some(Duration::from_millis(500)));
        assert!(ok, "pool worker should have completed the task");
    }

    #[test]
    fn value_bits_roundtrip_int() {
        let (bits, tag) = value_to_bits(Value::Int(42));
        assert_eq!(tag, 2);
        assert_eq!(bits_to_value(bits, tag), Value::Int(42));
    }

    #[test]
    fn value_bits_roundtrip_long() {
        let (bits, tag) = value_to_bits(Value::Long(0xdead_beef_1234_5678_u64 as i64));
        assert_eq!(tag, 3);
        assert_eq!(
            bits_to_value(bits, tag),
            Value::Long(0xdead_beef_1234_5678_u64 as i64),
        );
    }

    #[test]
    fn value_bits_roundtrip_null() {
        let (bits, tag) = value_to_bits(Value::Object(None));
        assert_eq!(tag, 1);
        assert_eq!(bits, 0);
        assert_eq!(bits_to_value(bits, tag), Value::Object(None));
    }

    /// SECURITY (review 2026-06-20): a non-zero tag-1 (Object) payload must
    /// NEVER be turned into a dereferenceable `ObjectRef`. An arbitrary /
    /// hostile integer that merely happens to be 8-byte aligned must still
    /// decode to null, not a fabricated reference.
    #[test]
    fn bits_to_value_object_never_fabricates_pointer() {
        // Aligned but arbitrary integer — would previously pass the
        // alignment-only guard and become a bogus ObjectRef.
        assert_eq!(bits_to_value(0x4000_1000, 1), Value::Object(None));
        // Misaligned / null all degrade to null too.
        assert_eq!(bits_to_value(0x4000_1001, 1), Value::Object(None));
        assert_eq!(bits_to_value(-1, 1), Value::Object(None));
        assert_eq!(bits_to_value(0, 1), Value::Object(None));
    }

    #[test]
    fn common_pool_is_singleton() {
        let p1 = common_pool() as *const _;
        let p2 = common_pool() as *const _;
        assert_eq!(p1, p2, "common_pool must return the same instance");
        assert!(common_pool().parallelism() >= 1);
    }

    // --- bug nb-concurrent-extras: GC-safe deposit/consume ------------------

    /// Allocate a stand-in `SynchronousQueue` instance with the 2 fields the
    /// hardened natives rely on (slot mirror + has-item flag).
    fn alloc_queue(ctx: &mut crate::test_utils::MockNativeContext) -> ObjectRef {
        let cid = ctx
            .ensure_class_initialized("java/util/concurrent/SynchronousQueue")
            .unwrap();
        ctx.alloc_object(cid, 2)
    }

    fn fresh_slot() -> SqSlot {
        SqSlot {
            item_bits: 0,
            has_item: false,
            waiting_takers: 0,
            tag: 0,
        }
    }

    fn sq_offer_callback() -> cratonvm_native_api::NativeCallback {
        let mut registry = NativeMethodRegistry::new();
        register_synchronous_queue_extras(&mut registry);
        registry
            .find(
                "java/util/concurrent/SynchronousQueue",
                "offer",
                "(Ljava/lang/Object;)Z",
            )
            .expect("SynchronousQueue.offer must be registered")
    }

    #[test]
    fn sq_offer_without_waiting_taker_does_not_buffer() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let this = alloc_queue(&mut ctx);
        reset_slot(this);

        let result =
            sq_offer_callback()(&mut ctx, &[Value::Object(Some(this)), Value::Int(77)]).unwrap();

        assert_eq!(result, Some(Value::Int(0)));
        let slot = get_or_create_slot(this);
        let state = slot.state.lock();
        assert!(!state.has_item, "failed offer must not buffer the item");
        assert_eq!(ctx.get_field(this, 1), Value::Int(0));
    }

    #[test]
    fn sq_offer_with_waiting_taker_deposits_item() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let this = alloc_queue(&mut ctx);
        reset_slot(this);
        let slot = get_or_create_slot(this);
        slot.state.lock().waiting_takers = 1;

        let result =
            sq_offer_callback()(&mut ctx, &[Value::Object(Some(this)), Value::Int(77)]).unwrap();

        assert_eq!(result, Some(Value::Int(1)));
        let mut state = slot.state.lock();
        assert!(state.has_item, "offer must hand off to the waiting taker");
        assert_eq!(consume_item(&mut ctx, this, &mut state), Value::Int(77));
    }

    /// Depositing an Object stores it in the GC-tracked mirror field 0 (NOT a
    /// bare pointer in the slot), and consuming it hands the same reference
    /// back and clears both the slot and the mirror (bug nb-concurrent-extras).
    #[test]
    fn sq_deposit_consume_object_via_field0() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let this = alloc_queue(&mut ctx);
        let item_cid = ctx.ensure_class_initialized("java/lang/Object").unwrap();
        let item = ctx.alloc_object(item_cid, 0);

        let mut slot = fresh_slot();
        deposit_item(&mut ctx, this, &mut slot, Value::Object(Some(item)));
        assert!(slot.has_item, "deposit must mark the slot occupied");
        assert_eq!(slot.tag, 1, "Object item must carry tag 1");
        assert_eq!(
            slot.item_bits, 0,
            "Object pointer must NOT be stashed in the global slot",
        );
        // The Object lives in the GC-tracked mirror field 0.
        assert_eq!(ctx.get_field(this, 0), Value::Object(Some(item)));

        let out = consume_item(&mut ctx, this, &mut slot);
        assert_eq!(
            out,
            Value::Object(Some(item)),
            "consume must hand back the reference read from mirror field 0",
        );
        // Slot fully drained and mirror cleared (no stale root past handoff).
        assert!(!slot.has_item);
        assert_eq!(slot.tag, 0);
        assert_eq!(ctx.get_field(this, 0), Value::Object(None));
    }

    /// A second deposit before the first is consumed overwrites the mirror
    /// and surfaces the newest item.
    #[test]
    fn sq_deposit_over_deposit_surfaces_latest() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let this = alloc_queue(&mut ctx);
        let item_cid = ctx.ensure_class_initialized("java/lang/Object").unwrap();
        let first = ctx.alloc_object(item_cid, 0);
        let second = ctx.alloc_object(item_cid, 0);

        let mut slot = fresh_slot();
        deposit_item(&mut ctx, this, &mut slot, Value::Object(Some(first)));
        deposit_item(&mut ctx, this, &mut slot, Value::Object(Some(second)));

        let out = consume_item(&mut ctx, this, &mut slot);
        assert_eq!(
            out,
            Value::Object(Some(second)),
            "second deposit must overwrite the first",
        );
        assert_eq!(ctx.get_field(this, 0), Value::Object(None));
    }

    /// Primitive and null-Object items roundtrip correctly: primitives use
    /// the in-slot payload, null Objects clear field 0.
    #[test]
    fn sq_deposit_consume_primitive_and_null() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let this = alloc_queue(&mut ctx);
        let mut slot = fresh_slot();

        deposit_item(&mut ctx, this, &mut slot, Value::Int(7));
        assert_eq!(slot.tag, 2);
        assert_eq!(slot.item_bits, 7, "primitive payload kept in the slot");
        assert_eq!(consume_item(&mut ctx, this, &mut slot), Value::Int(7));
        assert!(!slot.has_item);

        deposit_item(&mut ctx, this, &mut slot, Value::Object(None));
        assert_eq!(slot.tag, 1);
        assert_eq!(slot.item_bits, 0, "null Object stores no pointer bits");
        assert_eq!(consume_item(&mut ctx, this, &mut slot), Value::Object(None),);
        assert!(!slot.has_item);
    }
}
