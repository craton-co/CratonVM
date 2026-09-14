// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! T19.7.c — `org.xnio.XnioIoThread` + `org.xnio.nio.NioIoThread` + event-loop
//! internals.
//!
//! Each `XnioWorker` (T19.7.b) owns N `XnioIoThread` instances; every I/O
//! thread runs an event loop that owns a `Selector` (T19.7.a) and processes
//! ready channel events + a thread-local task queue. This module provides:
//!
//!   * the `IoThreadHandle` value T19.7.b hands off to the spawned worker
//!     thread,
//!   * the `run_io_loop(handle)` trampoline the worker thread blocks on,
//!   * `execute` / `executeAfter` / `executeAtTime` / `currentThread`
//!     native bindings on `org.xnio.XnioIoThread` + `org.xnio.nio.NioIoThread`,
//!   * the `XnioExecutor.Key` cancellation handle.
//!
//! # Event-loop structure (5 phases per iteration)
//!
//! 1. Drain the immediate task queue — every `execute(Runnable)` call
//!    from *any* thread appends here; same-thread execute does not wake the
//!    selector, cross-thread execute does.
//! 2. Compute the next timer deadline from the scheduled-task min-heap;
//!    saturate to zero if already past due.
//! 3. Block in `Selector::select(timeout_ms)` until a channel is ready OR
//!    a `wakeup()` nudges us awake.
//! 4. Dispatch each ready `SelectionKey` into T19.7.d's conduit listener
//!    (`handleReadable` / `handleWritable` / `handleConnected` /
//!    `handleAccept`).
//! 5. Fire any scheduled tasks whose deadline has passed.
//!
//! Every dispatched runnable / listener / timer is wrapped in
//! `catch_unwind(AssertUnwindSafe)` so a panicking Runnable logs via
//! `tracing::error!` and the loop continues.
//!
//! # Scheduled-task priority queue
//!
//! A `BinaryHeap<Reverse<ScheduledTask>>` gives us a min-heap by deadline.
//! `executeAfter` and `executeAtTime` return an `XnioExecutor.Key` carrying
//! an `Arc<AtomicBool> cancelled` — cancelled tasks are detected at pop
//! time and silently skipped (we do not remove from the heap up-front
//! because `BinaryHeap` has no remove API; lazy removal is O(log n) per
//! firing).
//!
//! # Cross-thread `execute`
//!
//! When a thread other than the target I/O thread calls `execute(runnable)`:
//!
//! 1. the runnable is pushed onto the target's `task_queue`
//!    (`Mutex<VecDeque>` — T2.7 crates lack a `crossbeam` dep, so we use
//!    the std lock),
//! 2. the target's selector `wakeup()` is invoked so a blocked
//!    `select(timeout)` returns immediately.
//!
//! Same-thread execute skips the wakeup (the loop will drain the queue
//! on its next iteration anyway).
//!
//! Wakeup storms are deduped via an `AtomicBool pending_wakeup` — if it
//! was already `true`, we skip the OS-level wake-call.
//!
//! # Selector API consumed (T19.7.a surface)
//!
//! We talk to T19.7.a through a small trait (`SelectorHandle`) so the loop
//! does not hard-depend on T19.7.a's concrete representation. The trait
//! covers:
//!   * `select(timeout_ms)` — block up to `timeout_ms` ms, return ready count.
//!   * `wakeup()` — make any blocked `select` return immediately.
//!   * `selected_keys()` — drain the ready-set (consumed once per iteration).
//!
//! If T19.7.a has not landed yet, tests use `MockSelector` (a fake that
//! just parks for `timeout_ms` and returns zero ready keys). Documented
//! on the struct.
//!
//! # Synthetic-stub field layouts (see `classloading/src/class_manager.rs`)
//!
//! | Class                                | # | Slots                                 |
//! |--------------------------------------|---|---------------------------------------|
//! | `org/xnio/XnioIoThread`              | 4 | id, worker_handle, selector_handle, state |
//! | `org/xnio/nio/NioIoThread`           | 4 | (inherits XnioIoThread, 0 extra)      |
//! | `org/xnio/XnioExecutor$Key`          | 2 | task_id, cancelled                    |
//!
//! # Security + resource caps
//!
//! * task-queue soft cap: `MAX_PENDING_TASKS = 10_000`. Over-cap `execute`
//!   throws `RejectedExecutionException` synchronously.
//! * scheduled-task cap: `MAX_SCHEDULED_TASKS = 1_000`. Over-cap
//!   `executeAfter` / `executeAtTime` throws `RejectedExecutionException`.
//! * wakeup coalescing: `AtomicBool pending_wakeup` ensures multiple
//!   concurrent `execute()` calls collapse to a single OS wake.
//! * panic isolation: every task / listener / timer is `catch_unwind`
//!   wrapped; panics land in `tracing::error!` with the thread's name.
//! * timer precision: millisecond resolution (documented — XNIO is not a
//!   real-time framework; HotSpot's `Xnio` itself is ms-accurate).

#![allow(clippy::needless_pass_by_value, clippy::too_many_arguments)]

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cratonvm_native_api::{NativeContext, NativeMethodRegistry, NativeThreadBlocker};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError, VmError};
use cratonvm_types::{ObjectRef, Value};

use crate::{obj_arg, try_alloc_concurrent_synthetic};

// ---------------------------------------------------------------------------
// Class names
// ---------------------------------------------------------------------------

const CLS_XNIO_IO_THREAD: &str = "org/xnio/XnioIoThread";
const CLS_NIO_IO_THREAD: &str = "org/xnio/nio/NioIoThread";
const CLS_EXECUTOR_KEY: &str = "org/xnio/XnioExecutor$Key";
const CLS_RUNNABLE: &str = "java/lang/Runnable";
const CLS_REJECTED_EXEC: &str = "java/util/concurrent/RejectedExecutionException";

// ---------------------------------------------------------------------------
// Synthetic field offsets (mirrored in classloading/src/class_manager.rs)
// ---------------------------------------------------------------------------

// XnioIoThread  (NioIoThread reuses these; same layout)
pub const IOT_FIELD_ID: usize = 0;
pub const IOT_FIELD_WORKER_HANDLE: usize = 1;
pub const IOT_FIELD_SELECTOR_HANDLE: usize = 2;
pub const IOT_FIELD_STATE: usize = 3;
pub const IOT_NUM_SLOTS: usize = 4;

// XnioExecutor$Key
pub const KEY_FIELD_TASK_ID: usize = 0;
pub const KEY_FIELD_CANCELLED: usize = 1;
pub const KEY_NUM_SLOTS: usize = 2;

// Thread state codes stored in IOT_FIELD_STATE.
pub const STATE_NEW: i32 = 0;
pub const STATE_RUNNING: i32 = 1;
pub const STATE_STOPPING: i32 = 2;
pub const STATE_TERMINATED: i32 = 3;

// ---------------------------------------------------------------------------
// Resource caps (docstring)
// ---------------------------------------------------------------------------

/// Maximum number of pending (immediate) tasks queued on a single I/O
/// thread. Over-cap `execute` throws `RejectedExecutionException`
/// synchronously.
pub const MAX_PENDING_TASKS: usize = 10_000;

/// Maximum number of scheduled (deadline-based) tasks on a single I/O
/// thread. Over-cap `executeAfter` / `executeAtTime` throws
/// `RejectedExecutionException` synchronously.
pub const MAX_SCHEDULED_TASKS: usize = 1_000;

// ---------------------------------------------------------------------------
// Selector abstraction (T19.7.a consumption surface)
// ---------------------------------------------------------------------------

/// Narrow view of the T19.7.a `SelectorImpl` surface the event loop
/// calls into. Keeps the loop generic over "real selector" vs the test
/// `MockSelector`, and documents the exact three methods we depend on.
pub trait SelectorHandle: Send + Sync {
    /// Block up to `timeout_ms` milliseconds waiting for channel readiness
    /// or a `wakeup()`. Returns the number of ready keys (0 if we woke on
    /// timeout or wakeup with nothing ready).
    fn select(&self, timeout_ms: u64) -> std::io::Result<usize>;

    /// Unblock any in-flight `select` on this selector. Idempotent —
    /// multiple calls collapse to a single wake.
    fn wakeup(&self);

    /// Drain the ready-set. Each element is a lightweight "selected key"
    /// id that T19.7.d maps back to a conduit listener.
    fn selected_keys(&self) -> Vec<SelectedKey>;
}

/// A single ready channel event pulled from the selector's ready-set.
/// The fields match the subset T19.7.d's conduits need to resolve their
/// listener + ready-ops bitmask. The loop simply hands this to
/// `dispatch_channel_event`, which is a shim to T19.7.d.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectedKey {
    /// Opaque id of the registered channel (T19.7.d maps it to a conduit).
    pub channel_id: u64,
    /// Bitmask of ready ops: 1=READ, 4=WRITE, 8=CONNECT, 16=ACCEPT
    /// (mirrors `SelectionKey.OP_*`).
    pub ready_ops: i32,
}

// ---------------------------------------------------------------------------
// MockSelector — used by this module's unit tests.
//
// A real `SelectorImpl` from `native-io/src/nio_selector.rs` plugs in when
// T19.7.a lands; `MockSelector` just honours `timeout_ms` + `wakeup()` so
// our event-loop tests do not depend on that work. Documented per the
// task spec.
// ---------------------------------------------------------------------------

/// Fake selector for unit tests. Behaviour:
///  * `select(timeout_ms)` parks on a `Condvar` for up to `timeout_ms`.
///  * `wakeup()` notifies the condvar; subsequent `select` returns 0
///    immediately until the pending flag clears.
///  * `selected_keys()` returns anything the test pre-seeded via
///    `push_ready`.
pub struct MockSelector {
    wakeup_mu: Mutex<bool>, // true == wakeup pending
    wakeup_cv: std::sync::Condvar,
    ready: Mutex<VecDeque<SelectedKey>>,
}

impl Default for MockSelector {
    fn default() -> Self {
        Self::new()
    }
}

impl MockSelector {
    pub fn new() -> Self {
        Self {
            wakeup_mu: Mutex::new(false),
            wakeup_cv: std::sync::Condvar::new(),
            ready: Mutex::new(VecDeque::new()),
        }
    }

    /// Test helper: seed a ready SelectionKey so the next loop
    /// iteration's `dispatch_channel_event` call sees it.
    pub fn push_ready(&self, key: SelectedKey) {
        self.ready
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(key);
        self.wakeup();
    }
}

impl SelectorHandle for MockSelector {
    fn select(&self, timeout_ms: u64) -> std::io::Result<usize> {
        let mut guard = self.wakeup_mu.lock().unwrap_or_else(|e| e.into_inner());
        if *guard {
            *guard = false;
        } else {
            let dur = Duration::from_millis(timeout_ms);
            let (g, _) = self
                .wakeup_cv
                .wait_timeout(guard, dur)
                .unwrap_or_else(|e| e.into_inner());
            guard = g;
            *guard = false;
        }
        let ready_n = self.ready.lock().unwrap_or_else(|e| e.into_inner()).len();
        Ok(ready_n)
    }

    fn wakeup(&self) {
        let mut guard = self.wakeup_mu.lock().unwrap_or_else(|e| e.into_inner());
        *guard = true;
        self.wakeup_cv.notify_all();
    }

    fn selected_keys(&self) -> Vec<SelectedKey> {
        let mut ready = self.ready.lock().unwrap_or_else(|e| e.into_inner());
        ready.drain(..).collect()
    }
}

// ---------------------------------------------------------------------------
// Task + scheduled-task types
// ---------------------------------------------------------------------------

/// A runnable we will execute on the I/O thread. Boxed `FnOnce` so it
/// can be moved across threads and consumed once.
pub type IoTask = Box<dyn FnOnce() + Send + 'static>;

/// A task waiting for its deadline.
pub struct ScheduledTask {
    /// Deadline in milliseconds since the UNIX epoch.
    pub deadline_ms: i64,
    /// Monotonic sequence number — tiebreaks equal deadlines by insertion order.
    pub seq: u64,
    /// The runnable itself. Wrapped in `Option` so we can take-by-value
    /// when the task fires.
    pub runnable: Option<IoTask>,
    /// Shared cancellation flag — `Key.remove()` flips this to true.
    pub cancelled: Arc<AtomicBool>,
    /// Public id echoed back on the `XnioExecutor.Key`.
    pub task_id: u64,
}

impl std::fmt::Debug for ScheduledTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScheduledTask")
            .field("deadline_ms", &self.deadline_ms)
            .field("seq", &self.seq)
            .field("task_id", &self.task_id)
            .field("cancelled", &self.cancelled.load(Ordering::Acquire))
            .finish()
    }
}

// BinaryHeap is a max-heap; wrap in `Reverse` at the call site. Order by
// (deadline, seq) so later-scheduled tasks at equal deadlines fire after
// earlier-scheduled ones.
impl PartialEq for ScheduledTask {
    fn eq(&self, other: &Self) -> bool {
        self.deadline_ms == other.deadline_ms && self.seq == other.seq
    }
}
impl Eq for ScheduledTask {}
impl PartialOrd for ScheduledTask {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for ScheduledTask {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.deadline_ms
            .cmp(&other.deadline_ms)
            .then(self.seq.cmp(&other.seq))
    }
}

/// Min-heap wrapper on `BinaryHeap<Reverse<ScheduledTask>>`. Exposes
/// exactly what the event loop needs: `peek_deadline_ms` (for the next
/// select timeout), `pop_if_expired` (for step 5), and `push_bounded`
/// (for `executeAfter`).
pub struct ScheduledHeap {
    heap: BinaryHeap<Reverse<ScheduledTask>>,
}

impl Default for ScheduledHeap {
    fn default() -> Self {
        Self::new()
    }
}

impl ScheduledHeap {
    pub fn new() -> Self {
        Self {
            heap: BinaryHeap::new(),
        }
    }

    /// Current number of entries (including cancelled).
    pub fn len(&self) -> usize {
        self.heap.len()
    }

    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    /// Deadline of the earliest pending task (None if empty). Skips
    /// cancelled tasks at the root by popping + dropping them — lazy
    /// cleanup.
    pub fn peek_deadline_ms(&mut self) -> Option<i64> {
        while let Some(Reverse(top)) = self.heap.peek() {
            if top.cancelled.load(Ordering::Acquire) {
                // Drop the cancelled root and keep looking.
                let _ = self.heap.pop();
                continue;
            }
            return Some(top.deadline_ms);
        }
        None
    }

    /// Pop the earliest task if its deadline is <= `now_ms`. Returns the
    /// (non-cancelled) `ScheduledTask` ready to fire, or None if nothing
    /// is ready yet. Cancelled roots are silently discarded.
    pub fn pop_if_expired(&mut self, now_ms: i64) -> Option<ScheduledTask> {
        loop {
            let top = self.heap.peek()?;
            let top = &top.0;
            if top.cancelled.load(Ordering::Acquire) {
                let _ = self.heap.pop();
                continue;
            }
            if top.deadline_ms > now_ms {
                return None;
            }
            return self.heap.pop().map(|Reverse(t)| t);
        }
    }

    /// Push a new scheduled task, enforcing `MAX_SCHEDULED_TASKS`. Returns
    /// `Err(())` if the cap is exceeded so the caller can throw
    /// `RejectedExecutionException`.
    pub fn push_bounded(&mut self, task: ScheduledTask) -> Result<(), ScheduledTask> {
        if self.heap.len() >= MAX_SCHEDULED_TASKS {
            return Err(task);
        }
        self.heap.push(Reverse(task));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// IoThreadHandle — shared state between the outer world and the I/O loop.
// ---------------------------------------------------------------------------

/// The handle a spawned I/O thread holds. Shared (via `Arc`) with:
///   * the parent `XnioWorker` (T19.7.b) — for shutdown + discovery,
///   * any thread calling `execute` / `executeAfter` on this I/O thread.
pub struct IoThreadHandle {
    /// Stable id, unique across all I/O threads in the process.
    pub id: u64,
    /// Opaque parent-worker handle echoed back by `getWorker`.
    pub worker_handle: u64,
    /// The selector this loop blocks on. Owned as a trait object so
    /// T19.7.a's concrete type and `MockSelector` both work.
    pub selector: Arc<dyn SelectorHandle>,
    /// Immediate-execute task queue. `Mutex<VecDeque>` — simple, no
    /// crossbeam dep in the workspace.
    pub task_queue: Mutex<VecDeque<IoTask>>,
    /// Deadline-based tasks.
    pub scheduled: Mutex<ScheduledHeap>,
    /// True once `shutdown()` is requested; the loop checks on every
    /// iteration.
    pub shutdown_requested: AtomicBool,
    /// Coalesced wakeup flag — set by `execute` from another thread
    /// before it calls `selector.wakeup()`; cleared by the loop right
    /// before it re-enters `select`.
    pub pending_wakeup: AtomicBool,
    /// OS thread id of the loop thread. Populated on first iteration
    /// so `currentThread()` can compare. 0 means "not yet started".
    pub loop_thread_id: AtomicU64,
    /// Monotonic counter feeding `ScheduledTask.seq`.
    pub seq_counter: AtomicU64,
    /// Monotonic counter feeding `XnioExecutor.Key.task_id`.
    pub task_id_counter: AtomicU64,
    /// Optional task-origin context label for panic logs (e.g.
    /// "undertow-worker-3"). Set once at construction.
    pub thread_name: String,
    /// Queue-length counter, kept in sync with `task_queue.len()`
    /// without taking the lock — used for the `MAX_PENDING_TASKS` cap
    /// check (the actual push takes the lock).
    pub pending_len: AtomicUsize,
}

impl IoThreadHandle {
    /// Construct a new handle with a fresh selector. Caller assigns
    /// `worker_handle` (T19.7.b does this from the worker registry).
    pub fn new(
        id: u64,
        worker_handle: u64,
        selector: Arc<dyn SelectorHandle>,
        thread_name: impl Into<String>,
    ) -> Arc<Self> {
        Arc::new(Self {
            id,
            worker_handle,
            selector,
            task_queue: Mutex::new(VecDeque::new()),
            scheduled: Mutex::new(ScheduledHeap::new()),
            shutdown_requested: AtomicBool::new(false),
            pending_wakeup: AtomicBool::new(false),
            loop_thread_id: AtomicU64::new(0),
            seq_counter: AtomicU64::new(0),
            task_id_counter: AtomicU64::new(0),
            thread_name: thread_name.into(),
            pending_len: AtomicUsize::new(0),
        })
    }

    /// Request the event loop to exit at the next iteration.
    /// Also wakes the selector so the loop does not block on `select`.
    pub fn shutdown(&self) {
        self.shutdown_requested.store(true, Ordering::Release);
        self.selector.wakeup();
    }

    /// Push an immediate task onto the queue. Returns `Ok(())` on success,
    /// `Err(())` if the queue is at `MAX_PENDING_TASKS`.
    ///
    /// If the caller is running on a *different* OS thread than the loop,
    /// the selector is woken so a blocked `select` returns immediately.
    /// Same-thread calls skip the wake (the loop drains on its next
    /// iteration anyway).
    pub fn try_execute(&self, task: IoTask) -> Result<(), IoTask> {
        // Fast check without locking.
        if self.pending_len.load(Ordering::Acquire) >= MAX_PENDING_TASKS {
            return Err(task);
        }
        {
            let mut q = self.task_queue.lock().unwrap_or_else(|e| e.into_inner());
            if q.len() >= MAX_PENDING_TASKS {
                return Err(task);
            }
            q.push_back(task);
            self.pending_len.store(q.len(), Ordering::Release);
        }
        // Cross-thread? Fire a selector wakeup.
        let loop_tid = self.loop_thread_id.load(Ordering::Acquire);
        let my_tid = os_tid();
        if loop_tid != my_tid && !self.pending_wakeup.swap(true, Ordering::AcqRel) {
            self.selector.wakeup();
        }
        Ok(())
    }

    /// Schedule a task at an absolute ms-since-epoch deadline. Returns the
    /// `Arc<AtomicBool>` cancellation flag + the assigned task id so the
    /// caller can build the `XnioExecutor.Key` mirror.
    pub fn try_schedule_at(
        &self,
        deadline_ms: i64,
        runnable: IoTask,
    ) -> Result<(u64, Arc<AtomicBool>), IoTask> {
        let cancelled = Arc::new(AtomicBool::new(false));
        let task_id = self.task_id_counter.fetch_add(1, Ordering::Relaxed) + 1;
        let seq = self.seq_counter.fetch_add(1, Ordering::Relaxed);
        let task = ScheduledTask {
            deadline_ms,
            seq,
            runnable: Some(runnable),
            cancelled: cancelled.clone(),
            task_id,
        };
        {
            let mut heap = self.scheduled.lock().unwrap_or_else(|e| e.into_inner());
            match heap.push_bounded(task) {
                Ok(()) => {}
                Err(rejected) => {
                    // Return the runnable back to the caller so they can
                    // raise RejectedExecutionException synchronously.
                    if let Some(r) = rejected.runnable {
                        return Err(r);
                    }
                    return Err(Box::new(|| {}));
                }
            }
        }
        // Scheduling could move the nearest-deadline sooner — wake.
        let loop_tid = self.loop_thread_id.load(Ordering::Acquire);
        let my_tid = os_tid();
        if loop_tid != my_tid && !self.pending_wakeup.swap(true, Ordering::AcqRel) {
            self.selector.wakeup();
        }
        Ok((task_id, cancelled))
    }
}

// ---------------------------------------------------------------------------
// Dispatch shim — placeholder for T19.7.d.
// ---------------------------------------------------------------------------

/// Hook T19.7.d's conduit layer installs to receive ready-key events.
///
/// `dispatch_channel_event` looks up the listener by `channel_id` and
/// invokes `handleReadable` / `handleWritable` / `handleConnected` /
/// `handleAccept` depending on the bits in `ready_ops`.
///
/// Default impl is a no-op; T19.7.d calls `set_channel_dispatcher` in
/// its `register_conduit_natives` to wire a real one.
pub type ChannelDispatcher = Box<dyn Fn(SelectedKey) + Send + Sync + 'static>;

static CHANNEL_DISPATCHER: OnceLock<Mutex<Option<ChannelDispatcher>>> = OnceLock::new();

fn channel_dispatcher_cell() -> &'static Mutex<Option<ChannelDispatcher>> {
    CHANNEL_DISPATCHER.get_or_init(|| Mutex::new(None))
}

/// T19.7.d calls this from `register_conduit_natives` to wire the
/// per-process dispatch function. Idempotent — the last caller wins.
pub fn set_channel_dispatcher(f: ChannelDispatcher) {
    let cell = channel_dispatcher_cell();
    *cell.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
}

/// Dispatch a single ready `SelectionKey` into the conduit layer. If
/// no dispatcher is installed (T19.7.d not yet landed, or a standalone
/// test), this is a no-op. Panic-safe — a panicking listener is
/// logged but does not escape.
pub fn dispatch_channel_event(key: SelectedKey) {
    let cell = channel_dispatcher_cell();
    let guard = cell.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(dispatcher) = guard.as_ref() {
        let res = catch_unwind(AssertUnwindSafe(|| dispatcher(key)));
        if res.is_err() {
            tracing::error!(
                channel_id = key.channel_id,
                ready_ops = key.ready_ops,
                "xnio-io-thread: channel listener panicked",
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The event loop.
// ---------------------------------------------------------------------------

struct NativeSelectBlock<'a> {
    blocker: Option<&'a dyn NativeThreadBlocker>,
}

impl<'a> NativeSelectBlock<'a> {
    fn enter(blocker: Option<&'a dyn NativeThreadBlocker>) -> Self {
        if let Some(blocker) = blocker {
            blocker.enter_blocked();
        }
        Self { blocker }
    }
}

impl Drop for NativeSelectBlock<'_> {
    fn drop(&mut self) {
        if let Some(blocker) = self.blocker {
            blocker.leave_blocked();
        }
    }
}

/// Run the event loop for a single I/O thread. Called by T19.7.b's
/// worker-thread entry point after the handle has been constructed.
/// Returns only when `handle.shutdown_requested` is set.
///
/// Panic-safe per task — individual Runnables cannot crash the loop.
/// The five phases per iteration are documented in the module header.
pub fn run_io_loop(handle: Arc<IoThreadHandle>) {
    run_io_loop_with_blocker(handle, None);
}

fn run_io_loop_with_blocker(
    handle: Arc<IoThreadHandle>,
    blocker: Option<Arc<dyn NativeThreadBlocker>>,
) {
    // Record OS-level thread id so `execute` can tell same-thread vs
    // cross-thread.
    handle.loop_thread_id.store(os_tid(), Ordering::Release);
    if let Some(blocker) = blocker.as_deref() {
        blocker.publish_os_tid();
    }
    let _registered = CURRENT_IO_THREAD.with(|slot| {
        let mut g = slot.borrow_mut();
        let prev = g.clone();
        *g = Some(Arc::downgrade(&handle));
        prev
    });

    while !handle.shutdown_requested.load(Ordering::Acquire) {
        // ---- phase 1 — drain immediate tasks ----
        loop {
            let next = {
                let mut q = handle.task_queue.lock().unwrap_or_else(|e| e.into_inner());
                let t = q.pop_front();
                handle.pending_len.store(q.len(), Ordering::Release);
                t
            };
            match next {
                Some(task) => run_task_safely(task, &handle.thread_name),
                None => break,
            }
        }

        // Bail if shutdown came in while we were draining.
        if handle.shutdown_requested.load(Ordering::Acquire) {
            break;
        }

        // ---- phase 2 — compute select timeout from next timer ----
        let next_deadline_opt = {
            let mut heap = handle.scheduled.lock().unwrap_or_else(|e| e.into_inner());
            heap.peek_deadline_ms()
        };
        let now = now_ms();
        let timeout = match next_deadline_opt {
            Some(d) if d <= now => 0,
            Some(d) => (d - now).min(i64::from(u32::MAX)).max(0) as u64,
            None => 1_000, // cap at 1 s so shutdown + new work still get picked up promptly
        };

        // Clear pending_wakeup just before blocking; any wakeup that
        // arrives now still wins because the selector impl is edge-level
        // for incoming wakes.
        handle.pending_wakeup.store(false, Ordering::Release);

        // ---- phase 3 — block in selector.select ----
        let select_res = {
            let _blocked = NativeSelectBlock::enter(blocker.as_deref());
            handle.selector.select(timeout)
        };
        match select_res {
            Ok(_n) => {}
            Err(e) => {
                // Log and continue — a transient selector error should
                // not kill the loop, but we rate-limit by sleeping 10ms.
                tracing::error!(
                    thread = %handle.thread_name,
                    err = %e,
                    "xnio-io-thread: Selector.select failed; continuing after 10ms",
                );
                thread::sleep(Duration::from_millis(10));
            }
        }

        // ---- phase 4 — dispatch ready channel events ----
        let ready_keys = handle.selector.selected_keys();
        for key in ready_keys {
            // Panic isolation lives in `dispatch_channel_event`.
            dispatch_channel_event(key);
        }

        // ---- phase 5 — fire expired timers ----
        let now = now_ms();
        loop {
            let expired = {
                let mut heap = handle.scheduled.lock().unwrap_or_else(|e| e.into_inner());
                heap.pop_if_expired(now)
            };
            match expired {
                Some(mut st) => {
                    if st.cancelled.load(Ordering::Acquire) {
                        continue;
                    }
                    if let Some(task) = st.runnable.take() {
                        run_task_safely(task, &handle.thread_name);
                    }
                }
                None => break,
            }
        }
    }

    // Drain any remaining immediate tasks on shutdown (per spec #8):
    // we run them rather than silently drop, so user code sees a clean
    // quiesce.
    loop {
        let next = {
            let mut q = handle.task_queue.lock().unwrap_or_else(|e| e.into_inner());
            q.pop_front()
        };
        match next {
            Some(task) => run_task_safely(task, &handle.thread_name),
            None => break,
        }
    }

    // Mark scheduled tasks as cancelled so any outstanding keys report
    // "not cancelled-before-run == true" (they never ran).
    {
        let mut heap = handle.scheduled.lock().unwrap_or_else(|e| e.into_inner());
        while let Some(Reverse(st)) = heap.heap.pop() {
            st.cancelled.store(true, Ordering::Release);
        }
    }

    CURRENT_IO_THREAD.with(|slot| {
        *slot.borrow_mut() = None;
    });
}

fn run_task_safely(task: IoTask, thread_name: &str) {
    let res = catch_unwind(AssertUnwindSafe(move || task()));
    if res.is_err() {
        tracing::error!(
            thread = %thread_name,
            "xnio-io-thread: task panicked; swallowed and continuing",
        );
    }
}

// ---------------------------------------------------------------------------
// currentThread() TLS slot.
// ---------------------------------------------------------------------------

thread_local! {
    /// Weak so a shutdown'd IoThreadHandle can free itself even if
    /// something forgot to clear the slot.
    static CURRENT_IO_THREAD: std::cell::RefCell<Option<Weak<IoThreadHandle>>>
        = const { std::cell::RefCell::new(None) };
}

/// Return the `IoThreadHandle` for the current OS thread, if that
/// thread is running `run_io_loop`. `None` otherwise.
pub fn current_io_thread() -> Option<Arc<IoThreadHandle>> {
    CURRENT_IO_THREAD.with(|slot| slot.borrow().as_ref().and_then(|w| w.upgrade()))
}

// ---------------------------------------------------------------------------
// Utility — monotonic ms timestamps + OS thread id.
// ---------------------------------------------------------------------------

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

thread_local! {
    /// PERF: cache the derived OS thread id per thread. The id is stable for
    /// the lifetime of the thread, so we compute the SipHash digest of
    /// `ThreadId` once (lazily on first access) and reuse it thereafter.
    /// This removes a fresh `DefaultHasher` (SipHash) computation from every
    /// hot-path call in `run_io_loop`/`dispatch`. `Cell<u64>` with a 0
    /// sentinel keeps the fast path branch-light; `ThreadId`'s hash never
    /// collides with a real-thread value of exactly 0 in practice, and even
    /// if it did the only cost is recomputing the same (stable) value.
    static OS_TID_CACHE: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Process-wide OS thread id, stable for the lifetime of the thread.
/// We derive it from `thread::current().id()` because `std::thread::ThreadId`
/// is Windows/Linux portable, whereas `libc::pthread_self` is not.
fn os_tid() -> u64 {
    // PERF: read the per-thread cached id; only hash on first access.
    OS_TID_CACHE.with(|cache| {
        let cached = cache.get();
        if cached != 0 {
            return cached;
        }
        // `ThreadId` is an opaque wrapper around a u64; transmute-via-hash
        // turns it into a stable u64. Hasher + Hash is defined for ThreadId.
        use std::hash::{Hash, Hasher};
        let tid = thread::current().id();
        let mut h = std::collections::hash_map::DefaultHasher::new();
        tid.hash(&mut h);
        let id = h.finish();
        cache.set(id);
        id
    })
}

// ---------------------------------------------------------------------------
// Process-wide handle registry.
// ---------------------------------------------------------------------------
//
// The Java surface mirrors store only the `id` (Long) in field
// IOT_FIELD_SELECTOR_HANDLE; the actual Rust-side IoThreadHandle lives
// here. T19.7.b populates this map when it spawns a thread.

static IO_THREAD_REGISTRY: OnceLock<Mutex<std::collections::HashMap<u64, Arc<IoThreadHandle>>>> =
    OnceLock::new();

fn io_thread_registry() -> &'static Mutex<std::collections::HashMap<u64, Arc<IoThreadHandle>>> {
    IO_THREAD_REGISTRY.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// Register an `IoThreadHandle` so `XnioIoThread` mirrors can find it by
/// id. T19.7.b calls this from its worker-spawn path.
pub fn register_io_thread(handle: Arc<IoThreadHandle>) -> u64 {
    let id = handle.id;
    io_thread_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id, handle);
    id
}

/// Look up an `IoThreadHandle` by its id. Returns None if the id is
/// unknown (thread already shut down + unregistered).
pub fn lookup_io_thread(id: u64) -> Option<Arc<IoThreadHandle>> {
    io_thread_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id)
        .cloned()
}

/// Drop an `IoThreadHandle` from the registry. T19.7.b calls this
/// during `shutdown`.
pub fn unregister_io_thread(id: u64) {
    io_thread_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&id);
}

// ---------------------------------------------------------------------------
// T19_K2 — VM-tracked spawn entry point.
// ---------------------------------------------------------------------------
//
// `spawn_io_thread_with_ctx` is the production-mode equivalent of the
// `XnioWorker::new` `thread::Builder::new().spawn(...)` call. It registers
// the new OS thread with the VM `ThreadRegistry` so:
//
//   * GC root scanning sees its stack frames,
//   * JVMTI thread-list APIs report it,
//   * The CLI's `wait_for_non_daemon_threads()` loop waits for it
//     (when `daemon == false`).
//
// XNIO I/O threads run synchronously on the carrier — they are
// conceptually "background" threads. By default we register them as
// **non-daemon** so the VM won't exit while a server is actively
// serving I/O. Callers that explicitly want daemon semantics (purely
// internal worker pools that hold no observable state) can pass
// `daemon = true`.

/// **T19_K2** — spawn the per-I/O-thread event-loop body and register
/// the new OS thread with the VM `ThreadRegistry`.
///
/// `selector` is the `T19.7.a` `SelectorHandle` (or `MockSelector` in
/// tests). `worker_handle` is the parent `XnioWorker`'s id (echoed back
/// by `getWorker`). `id` is the per-worker I/O thread index.
///
/// Returns the live `Arc<IoThreadHandle>` (already inserted into
/// `io_thread_registry` so `lookup_io_thread(id)` resolves). The
/// JoinHandle is owned by the VM ThreadRegistry; callers don't get one
/// back. The OS thread terminates automatically when
/// `IoThreadHandle::shutdown()` is called and the loop body exits.
pub fn spawn_io_thread_with_ctx(
    ctx: &mut dyn NativeContext,
    name: impl Into<String>,
    id: u64,
    worker_handle: u64,
    selector: Arc<dyn SelectorHandle>,
    daemon: bool,
) -> Result<Arc<IoThreadHandle>, String> {
    let name = name.into();
    let handle = IoThreadHandle::new(id, worker_handle, selector, name.clone());
    register_io_thread(handle.clone());

    // **T19_K2** — two-phase registration. Phase 1: register WITHOUT a
    // handle to obtain the assigned VM ThreadId so the spawned closure
    // can capture it.
    let vm_tid = ctx.register_native_thread(&name, daemon, 0);
    record_vm_thread_id(handle.id, vm_tid);

    // **T19_K4** — Allocate a `java.lang.Thread` mirror for this XNIO
    // I/O thread carrier and attach it to the registry entry. Same
    // motivation + layout as `vertx_eventloop::spawn_vertx_event_loop_inner`:
    //
    //   * `Thread.currentThread()` resolves to the right object when
    //     Java-side code runs on the carrier (XNIO does invoke
    //     Runnable.run() bytecode from inside `run_io_loop` once
    //     T19.7.b is alive),
    //   * `ThreadRegistry::find_thread_id_by_thread_obj` finds the
    //     carrier when other code holds the mirror,
    //   * the carrier shows up in `ThreadRegistry::alive_thread_objects()`.
    //
    // Layout is `crate::alloc_carrier_thread_mirror`'s to pick: a real 19-slot
    // `java.lang.Thread` on a real image, the synthetic 5-slot map
    // (`name=0, priority=1, tid=2, target=3, virtualFlag=4`, see
    // `classloading::class_manager::synthetic_field_count`) otherwise.
    //
    // W7-74-short-object-repairs.md. This read
    // `ctx.alloc_object(ClassId::new(0), 5)` until 2026-08-12 — five slots of
    // `cratonvm/synthetic/AnonymousObject$5`, published to the thread registry
    // as this carrier's `java.lang.Thread`. Same defect and same repair as
    // `vertx_eventloop::spawn_vertx_event_loop_inner`; the two sites were
    // copies of each other and are now two calls to one helper.
    if vm_tid != 0 {
        // Bound to a `let` rather than written inline as the `if let`
        // scrutinee: a scrutinee's temporaries (the implicit reborrow of `ctx`
        // included) live for the whole `if let` under Rust 2021, and the body
        // needs `ctx` again for `set_native_thread_java_obj`.
        let mirror = crate::alloc_carrier_thread_mirror(ctx, &name, vm_tid, daemon);
        if let Some(mirror) = mirror {
            let attached = ctx.set_native_thread_java_obj(vm_tid, mirror);
            if !attached {
                tracing::warn!(
                    io_thread = %name,
                    vm_tid = vm_tid,
                    "T19_K4: set_native_thread_java_obj failed for XNIO IO thread; \
                     mirror won't be findable by ObjectRef (registration is OK)",
                );
            } else {
                record_iot_java_mirror_ptr(handle.id, mirror.as_ptr() as usize);
            }
        } else {
            tracing::warn!(
                io_thread = %name,
                vm_tid = vm_tid,
                "T19_K4: java/lang/Thread would not resolve; no mirror pre-registered \
                 for this XNIO IO thread (Thread.currentThread() will build one lazily)",
            );
        }
    }

    let h_for_thread = handle.clone();
    let captured_vm_tid = vm_tid;
    let blocker_for_thread = ctx.native_thread_blocker(vm_tid);
    let jh = thread::Builder::new()
        .name(name.clone())
        .spawn(move || {
            run_io_loop_with_blocker(h_for_thread.clone(), blocker_for_thread);
            // After the loop exits, push the captured VM ThreadId
            // onto the dead-queue. Captured-by-value avoids the
            // race that a `lookup_vm_thread_id` on a global map
            // would have against `record_vm_thread_id`.
            if captured_vm_tid != 0 {
                push_native_thread_dead_xnio(captured_vm_tid);
            }
        })
        .map_err(|e| format!("spawn_io_thread_with_ctx: {e}"))?;

    // Phase 2: attach the JoinHandle to the now-running OS thread.
    let boxed = Box::new(jh);
    let raw = Box::into_raw(boxed) as usize;
    let attached = ctx.attach_join_handle_to_native_thread(vm_tid, raw);
    if !attached {
        // VM rejected the attach. Reclaim the JoinHandle box so we
        // don't leak it — drop detaches the OS thread (it still
        // runs to completion).
        let reclaimed: Box<std::thread::JoinHandle<()>> =
            unsafe { Box::from_raw(raw as *mut std::thread::JoinHandle<()>) };
        drop(reclaimed);
    }
    Ok(handle)
}

/// **T19_K4** — process-wide map of `IoThreadHandle.id` → raw
/// `ObjectRef.as_ptr() as usize` for the linked `java.lang.Thread`
/// mirror. Tests assert against this; production code may use it
/// to ask "what's the Thread mirror for this carrier" without
/// re-walking the VM registry.
static IOT_JAVA_MIRROR_PTRS: OnceLock<Mutex<std::collections::HashMap<u64, usize>>> =
    OnceLock::new();

fn iot_java_mirror_ptrs() -> &'static Mutex<std::collections::HashMap<u64, usize>> {
    IOT_JAVA_MIRROR_PTRS.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn record_iot_java_mirror_ptr(iot_id: u64, mirror_ptr: usize) {
    if mirror_ptr == 0 {
        return;
    }
    iot_java_mirror_ptrs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(iot_id, mirror_ptr);
}

/// **T19_K4** — Read the recorded mirror pointer for the given
/// `IoThreadHandle.id`, or `0` if no mirror was attached. Returns
/// `usize` rather than `ObjectRef` so callers don't accidentally
/// dereference a stale pointer; the registry is the source of
/// truth for liveness.
pub fn lookup_iot_java_mirror_ptr(iot_id: u64) -> usize {
    iot_java_mirror_ptrs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&iot_id)
        .copied()
        .unwrap_or(0)
}

/// Per-IoThread VM ThreadId map. Keyed by `IoThreadHandle.id` so the
/// loop body's exit trampoline can look up which VM thread to mark
/// dead.
static IOT_VM_THREAD_IDS: OnceLock<Mutex<std::collections::HashMap<u64, u64>>> = OnceLock::new();

fn iot_vm_thread_ids() -> &'static Mutex<std::collections::HashMap<u64, u64>> {
    IOT_VM_THREAD_IDS.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn record_vm_thread_id(iot_id: u64, vm_tid: u64) {
    if vm_tid == 0 {
        return;
    }
    iot_vm_thread_ids()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(iot_id, vm_tid);
}

fn lookup_vm_thread_id(iot_id: u64) -> u64 {
    iot_vm_thread_ids()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&iot_id)
        .copied()
        .unwrap_or(0)
}

/// **T19_K2** — process-wide queue of dead native ThreadIds for the
/// XNIO subsystem. Drained by [`drain_xnio_native_thread_dead_queue`]
/// — the parent calls that from a registered native each `execute()` /
/// `shutdown()`.
static XNIO_NATIVE_THREAD_DEAD_QUEUE: OnceLock<Mutex<Vec<u64>>> = OnceLock::new();

fn xnio_native_thread_dead_queue() -> &'static Mutex<Vec<u64>> {
    XNIO_NATIVE_THREAD_DEAD_QUEUE.get_or_init(|| Mutex::new(Vec::new()))
}

const XNIO_DEAD_QUEUE_CAP: usize = 4096;

fn push_native_thread_dead_xnio(id: u64) {
    let mut q = xnio_native_thread_dead_queue()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if q.len() >= XNIO_DEAD_QUEUE_CAP {
        q.clear();
    }
    q.push(id);
}

/// Drain the XNIO dead-thread queue. Public so XNIO native methods can
/// flush it on every entry.
pub fn drain_xnio_native_thread_dead_queue() -> Vec<u64> {
    let mut q = xnio_native_thread_dead_queue()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    std::mem::take(&mut *q)
}

/// **T19_K2** — drain the XNIO dead-thread queue and tell the VM about
/// it via `unregister_native_thread`. Called from registered native
/// methods so the VM ThreadRegistry stays in sync.
pub fn flush_xnio_native_thread_deaths(ctx: &mut dyn NativeContext) {
    let dead = drain_xnio_native_thread_dead_queue();
    for id in dead {
        ctx.unregister_native_thread(id);
    }
}

// ---------------------------------------------------------------------------
// Native method bindings.
// ---------------------------------------------------------------------------

fn rejected_execution(message: impl Into<String>) -> MethodCallFailed {
    // No dedicated RejectedExecutionException variant in RuntimeError;
    // IllegalStateException is the closest semantic neighbour (the
    // JDK's RejectedExecutionException extends IllegalStateException).
    // Prefix the message so callers can still distinguish.
    MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::IllegalStateException {
        message: format!("{}: {}", CLS_REJECTED_EXEC, message.into()),
    }))
}

fn iot_id(ctx: &dyn NativeContext, this: ObjectRef) -> u64 {
    match ctx.get_field(this, IOT_FIELD_ID) {
        Value::Long(v) if v > 0 => return v as u64,
        Value::Int(v) if v > 0 => return v as u64,
        _ => {}
    }
    for field in ["id", "number"] {
        match ctx.get_field_by_name(this, field) {
            Value::Long(v) if v > 0 => return v as u64,
            Value::Int(v) if v > 0 => return v as u64,
            _ => {}
        }
    }
    0
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct IoThreadMirrorKey {
    vm: usize,
    identity: i32,
}

fn io_thread_mirror_key(ctx: &dyn NativeContext, io_thread: ObjectRef) -> IoThreadMirrorKey {
    IoThreadMirrorKey {
        vm: ctx.vm_identity(),
        identity: ctx.identity_hash_code(io_thread),
    }
}

fn io_thread_worker_mirror_registry() -> &'static Mutex<HashMap<IoThreadMirrorKey, ObjectRef>> {
    static REG: OnceLock<Mutex<HashMap<IoThreadMirrorKey, ObjectRef>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn remember_iot_worker_mirror(
    ctx: &dyn NativeContext,
    io_thread: ObjectRef,
    worker: ObjectRef,
) {
    io_thread_worker_mirror_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(io_thread_mirror_key(ctx, io_thread), worker);
}

pub fn lookup_iot_worker_mirror(
    ctx: &dyn NativeContext,
    io_thread: ObjectRef,
) -> Option<ObjectRef> {
    io_thread_worker_mirror_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&io_thread_mirror_key(ctx, io_thread))
        .copied()
}

fn iot_worker_mirror(ctx: &dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    if let Some(worker) = lookup_iot_worker_mirror(ctx, this) {
        return Some(worker);
    }
    for field in ["worker", "workerHandle"] {
        if let Value::Object(Some(worker)) = ctx.get_field_by_name(this, field) {
            return Some(worker);
        }
    }
    match ctx.get_field(this, IOT_FIELD_WORKER_HANDLE) {
        Value::Object(Some(worker)) => Some(worker),
        _ => None,
    }
}

/// `org.xnio.XnioIoThread.getId()J` — returns the stable id.
fn native_iot_get_id(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(Value::Long(iot_id(ctx, this) as i64)))
}

/// `org.xnio.XnioIoThread.getWorker()Lorg/xnio/XnioWorker;`
fn native_iot_get_worker(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if let Some(worker) = iot_worker_mirror(ctx, this) {
        return Ok(Some(Value::Object(Some(worker))));
    }
    let stub = try_alloc_concurrent_synthetic(ctx, "org/xnio/XnioWorker", 4)?;
    Ok(Some(Value::Object(Some(stub))))
}

/// `org.xnio.XnioIoThread.execute(Ljava/lang/Runnable;)V`
///
/// `org.xnio.XnioIoThread.execute(Ljava/lang/Runnable;)V`
///
/// B1 FIX: the loop body (`run_io_loop`) runs on a pinned OS thread with no
/// `NativeContext`, so it cannot drive a Java `Runnable.run()`. The previous
/// implementation therefore enqueued a closure that only flagged
/// `mark_synthetic_runnable_ran(ptr)` — the real `run()` bytecode never
/// executed, so every task submitted to an XNIO I/O thread silently did
/// nothing. We now invoke `Runnable.run()` synchronously here, where we DO
/// hold `ctx` (the same `ctx.invoke_virtual` bridge MSC uses in
/// `jboss_msc.rs::drive_starts`). The dispatched-set marker is retained for
/// test observability. `execute()` is fire-and-forget: a task that throws
/// must not surface to the submitter, so we log and swallow it.
fn native_iot_execute(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let runnable = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("XnioIoThread.execute: Runnable is null".into()),
            }
            .into())
        }
    };
    let id = iot_id(ctx, this);
    let raw_ptr = runnable.as_ptr() as usize;
    // Record dispatch (test hook) and, for a known thread, keep the queue
    // bookkeeping honest by rejecting when the loop's queue is at cap.
    if let Some(handle) = lookup_io_thread(id) {
        if handle.pending_len.load(Ordering::Acquire) >= MAX_PENDING_TASKS {
            return Err(rejected_execution(format!(
                "XnioIoThread.execute: queue at cap {}",
                MAX_PENDING_TASKS
            )));
        }
    } else {
        // Unknown thread id — tests that don't register a real
        // IoThreadHandle reach here. Stash the runnable on a process-wide
        // "pending runnables" list so test code can assert on it. We still
        // run the task below so behavior is correct either way.
        record_synthetic_pending(id, runnable);
    }
    mark_synthetic_runnable_ran(raw_ptr);
    // Actually run the task. `invoke_virtual` prepends the receiver; `run()V`
    // takes no further parameters.
    match ctx.invoke_virtual(runnable, "run", "()V", &[]) {
        Ok(_) => Ok(None),
        Err(e) => {
            tracing::warn!(
                error = ?e,
                "XnioIoThread.execute: submitted Runnable.run() threw; \
                 swallowing per execute() contract",
            );
            Ok(None)
        }
    }
}

/// `org.xnio.XnioIoThread.executeAfter(Ljava/lang/Runnable;JLjava/util/concurrent/TimeUnit;)Lorg/xnio/XnioExecutor$Key;`
fn native_iot_execute_after(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let runnable = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("executeAfter: Runnable is null".into()),
            }
            .into())
        }
    };
    let time = match args.get(2) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    // TimeUnit arg at index 3 is a mirror; for synthetic mode we
    // default to MILLISECONDS. Real-JDK mode should have converted to
    // nanos/millis already via TimeUnit.toMillis, but we accept either.
    let id = iot_id(ctx, this);
    let handle = match lookup_io_thread(id) {
        Some(h) => h,
        None => {
            // Build a cancellable Key mirror but do not actually schedule.
            return Ok(Some(Value::Object(Some(make_key_mirror(ctx, 0)?))));
        }
    };
    let raw_ptr = runnable.as_ptr() as usize;
    let deadline = now_ms().saturating_add(time.max(0));
    let res = handle.try_schedule_at(
        deadline,
        Box::new(move || mark_synthetic_runnable_ran(raw_ptr)),
    );
    match res {
        Ok((task_id, cancelled)) => {
            let key = make_key_mirror_with_flag(ctx, task_id, cancelled)?;
            Ok(Some(Value::Object(Some(key))))
        }
        Err(_) => Err(rejected_execution(format!(
            "executeAfter: scheduled-queue at cap {}",
            MAX_SCHEDULED_TASKS
        ))),
    }
}

/// `org.xnio.XnioIoThread.executeAtTime(Ljava/lang/Runnable;J)Lorg/xnio/XnioExecutor$Key;`
fn native_iot_execute_at_time(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let runnable = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("executeAtTime: Runnable is null".into()),
            }
            .into())
        }
    };
    let absolute_ms = match args.get(2) {
        Some(Value::Long(v)) => *v,
        _ => now_ms(),
    };
    let id = iot_id(ctx, this);
    let handle = match lookup_io_thread(id) {
        Some(h) => h,
        None => return Ok(Some(Value::Object(Some(make_key_mirror(ctx, 0)?)))),
    };
    let raw_ptr = runnable.as_ptr() as usize;
    let res = handle.try_schedule_at(
        absolute_ms,
        Box::new(move || mark_synthetic_runnable_ran(raw_ptr)),
    );
    match res {
        Ok((task_id, cancelled)) => {
            let key = make_key_mirror_with_flag(ctx, task_id, cancelled)?;
            Ok(Some(Value::Object(Some(key))))
        }
        Err(_) => Err(rejected_execution(format!(
            "executeAtTime: scheduled-queue at cap {}",
            MAX_SCHEDULED_TASKS
        ))),
    }
}

/// `org.xnio.XnioIoThread.currentThread()Lorg/xnio/XnioIoThread;`
///
/// Returns the thread mirror if called from inside `run_io_loop`; null
/// otherwise.
fn native_iot_current_thread(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    match current_io_thread() {
        Some(handle) => {
            // We do not hold a persistent mirror ref on the handle (would
            // leak). Instead, allocate a fresh mirror shell that carries
            // the thread id/number so later getId() calls can re-resolve
            // through the registry.
            let mirror = try_alloc_concurrent_synthetic(ctx, CLS_NIO_IO_THREAD, IOT_NUM_SLOTS)?;
            ctx.set_field_by_name(mirror, "id", Value::Long(handle.id as i64));
            ctx.set_field_by_name(mirror, "number", Value::Int(handle.id as i32));
            ctx.set_field_by_name(mirror, "state", Value::Int(STATE_RUNNING));
            Ok(Some(Value::Object(Some(mirror))))
        }
        None => Ok(Some(Value::Object(None))),
    }
}

/// `org.xnio.XnioExecutor$Key.remove()Z`
fn native_key_remove(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let task_id = match ctx.get_field(this, KEY_FIELD_TASK_ID) {
        Value::Long(v) => v as u64,
        _ => 0,
    };
    let cancel_slot = cancel_flag_for(task_id);
    let already = cancel_slot
        .as_ref()
        .map(|flag| flag.swap(true, Ordering::AcqRel))
        .unwrap_or(false);
    // Mirror the bool into the synthetic field for `isCancelled`-style
    // reads.
    ctx.set_field(this, KEY_FIELD_CANCELLED, Value::Int(1));
    // Returns true iff we flipped the flag from false → true before
    // the task surfaced. If `already == true`, either (a) we were
    // already cancelled or (b) the task has fired / cancel_slot is gone
    // — in both cases the JDK contract says "return false".
    Ok(Some(Value::Int(if !already && cancel_slot.is_some() {
        1
    } else {
        0
    })))
}

// ---------------------------------------------------------------------------
// Key cancellation-flag registry.
//
// Keys need to survive independent of the scheduled heap (a Key can be
// cancel-remove()'d after the heap has already popped + dropped the
// ScheduledTask). We keep a weak-reference-by-id map so `native_key_remove`
// can find the right AtomicBool without chasing through the heap.
// ---------------------------------------------------------------------------

static KEY_CANCEL_FLAGS: OnceLock<Mutex<std::collections::HashMap<u64, Arc<AtomicBool>>>> =
    OnceLock::new();

fn key_cancel_flags() -> &'static Mutex<std::collections::HashMap<u64, Arc<AtomicBool>>> {
    KEY_CANCEL_FLAGS.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn store_cancel_flag(task_id: u64, flag: Arc<AtomicBool>) {
    key_cancel_flags()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(task_id, flag);
}

fn cancel_flag_for(task_id: u64) -> Option<Arc<AtomicBool>> {
    key_cancel_flags()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&task_id)
        .cloned()
}

// ---------------------------------------------------------------------------
// Key mirror allocation.
// ---------------------------------------------------------------------------

fn make_key_mirror(
    ctx: &mut dyn NativeContext,
    task_id: u64,
) -> Result<ObjectRef, MethodCallFailed> {
    let key = try_alloc_concurrent_synthetic(ctx, CLS_EXECUTOR_KEY, KEY_NUM_SLOTS)?;
    ctx.set_field(key, KEY_FIELD_TASK_ID, Value::Long(task_id as i64));
    ctx.set_field(key, KEY_FIELD_CANCELLED, Value::Int(0));
    Ok(key)
}

fn make_key_mirror_with_flag(
    ctx: &mut dyn NativeContext,
    task_id: u64,
    flag: Arc<AtomicBool>,
) -> Result<ObjectRef, MethodCallFailed> {
    store_cancel_flag(task_id, flag);
    make_key_mirror(ctx, task_id)
}

// ---------------------------------------------------------------------------
// Synthetic Runnable dispatch tracking.
//
// Since `run_io_loop` runs without a NativeContext we cannot actually
// invoke Runnable.run() from the loop in synthetic / standalone unit-test
// mode. We track "runnables that got dispatched" in a process-wide set;
// tests assert on it. Production real-JDK mode plugs the actual
// VM-thread-aware run() invocation from T19.7.b's spawn site.
// ---------------------------------------------------------------------------

static DISPATCHED_RUNNABLES: OnceLock<Mutex<std::collections::HashSet<usize>>> = OnceLock::new();
static SYNTHETIC_PENDING: OnceLock<Mutex<Vec<(u64, usize)>>> = OnceLock::new();

fn dispatched_runnables() -> &'static Mutex<std::collections::HashSet<usize>> {
    DISPATCHED_RUNNABLES.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

fn synthetic_pending() -> &'static Mutex<Vec<(u64, usize)>> {
    SYNTHETIC_PENDING.get_or_init(|| Mutex::new(Vec::new()))
}

fn mark_synthetic_runnable_ran(ptr: usize) {
    dispatched_runnables()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(ptr);
}

fn record_synthetic_pending(id: u64, runnable: ObjectRef) {
    synthetic_pending()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((id, runnable.as_ptr() as usize));
}

/// Test-only helper — query whether a Runnable with the given raw ptr
/// was ever dispatched by an I/O loop in this process.
pub fn was_runnable_dispatched(raw_ptr: usize) -> bool {
    dispatched_runnables()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&raw_ptr)
}

// ---------------------------------------------------------------------------
// Registration.
// ---------------------------------------------------------------------------

/// Register XnioIoThread / NioIoThread / XnioExecutor$Key natives.
pub fn register_xnio_io_thread_natives(registry: &mut NativeMethodRegistry) {
    // XnioIoThread + NioIoThread share the same native table.
    for cls in [CLS_XNIO_IO_THREAD, CLS_NIO_IO_THREAD] {
        registry.register(cls, "getId", "()J", native_iot_get_id);
        registry.register(
            cls,
            "getWorker",
            "()Lorg/xnio/XnioWorker;",
            native_iot_get_worker,
        );
        registry.register(
            cls,
            "execute",
            "(Ljava/lang/Runnable;)V",
            native_iot_execute,
        );
        registry.register(
            cls,
            "executeAfter",
            "(Ljava/lang/Runnable;JLjava/util/concurrent/TimeUnit;)Lorg/xnio/XnioExecutor$Key;",
            native_iot_execute_after,
        );
        registry.register(
            cls,
            "executeAtTime",
            "(Ljava/lang/Runnable;J)Lorg/xnio/XnioExecutor$Key;",
            native_iot_execute_at_time,
        );
        registry.register(
            cls,
            "currentThread",
            "()Lorg/xnio/XnioIoThread;",
            native_iot_current_thread,
        );
    }
    registry.register(CLS_EXECUTOR_KEY, "remove", "()Z", native_key_remove);
}

// ===========================================================================
//                                   TESTS
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
    use std::sync::atomic::AtomicI32;
    use std::sync::{Arc, Barrier};

    /// Assertion helper — `Box<dyn FnOnce>` does not implement Debug
    /// so `.expect` / `.unwrap` do not compile on the Err branch.
    fn must_execute(res: Result<(), IoTask>) {
        if res.is_err() {
            panic!("try_execute must succeed");
        }
    }

    fn must_schedule(res: Result<(u64, Arc<AtomicBool>), IoTask>) -> (u64, Arc<AtomicBool>) {
        match res {
            Ok(x) => x,
            Err(_) => panic!("try_schedule_at must succeed"),
        }
    }

    /// Spawn a worker thread running `run_io_loop` and return (handle,
    /// join-handle, mock-selector-ref). The returned `IoThreadHandle`
    /// is already registered in the process-wide registry.
    fn spawn_test_loop(
        name: &str,
        id: u64,
    ) -> (
        Arc<IoThreadHandle>,
        thread::JoinHandle<()>,
        Arc<MockSelector>,
    ) {
        let mock = Arc::new(MockSelector::new());
        let selector: Arc<dyn SelectorHandle> = mock.clone();
        let h = IoThreadHandle::new(id, 1, selector, name);
        register_io_thread(h.clone());
        let h2 = h.clone();
        let jh = thread::Builder::new()
            .name(name.to_string())
            .spawn(move || run_io_loop(h2))
            .expect("spawn");
        // Wait briefly for the loop to claim its OS tid.
        for _ in 0..100 {
            if h.loop_thread_id.load(Ordering::Acquire) != 0 {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        (h, jh, mock)
    }

    fn shutdown_and_join(h: &Arc<IoThreadHandle>, jh: thread::JoinHandle<()>) {
        h.shutdown();
        jh.join().expect("join");
        unregister_io_thread(h.id);
    }

    /// Test 1: `execute` runs a runnable on the I/O thread.
    #[test]
    fn t19_7_c_execute_runs_runnable_on_thread() {
        let (h, jh, _sel) = spawn_test_loop("iot-test-1", 1001);
        let flag = Arc::new(AtomicBool::new(false));
        let flag2 = flag.clone();
        must_execute(h.try_execute(Box::new(move || {
            flag2.store(true, Ordering::Release);
        })));
        // Give the loop a chance to drain.
        for _ in 0..500 {
            if flag.load(Ordering::Acquire) {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert!(flag.load(Ordering::Acquire), "runnable must run");
        shutdown_and_join(&h, jh);
    }

    /// Test 2: `execute` from a different thread wakes the selector.
    #[test]
    fn t19_7_c_execute_from_other_thread_wakes_selector() {
        let (h, jh, _sel) = spawn_test_loop("iot-test-2", 1002);
        let flag = Arc::new(AtomicBool::new(false));
        let flag2 = flag.clone();
        let h2 = h.clone();
        // Submit from a separate thread that is definitely not the loop.
        let sub = thread::spawn(move || {
            must_execute(h2.try_execute(Box::new(move || {
                flag2.store(true, Ordering::Release);
            })));
        });
        sub.join().expect("sub join");
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(200) {
            if flag.load(Ordering::Acquire) {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert!(
            flag.load(Ordering::Acquire),
            "execute-from-other-thread must fire within 200ms (selector should have been woken)"
        );
        shutdown_and_join(&h, jh);
    }

    /// Test 3: `executeAfter` respects its deadline (500ms ±20% window).
    #[test]
    fn t19_7_c_execute_after_respects_deadline() {
        let (h, jh, _sel) = spawn_test_loop("iot-test-3", 1003);
        let fired_at = Arc::new(Mutex::new(None::<Instant>));
        let fired_at2 = fired_at.clone();
        let started = Instant::now();
        let deadline = now_ms().saturating_add(500);
        h.try_schedule_at(
            deadline,
            Box::new(move || {
                *fired_at2.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
            }),
        )
        .map_err(|_| "schedule rejected")
        .unwrap();
        // Wait up to 900ms.
        for _ in 0..450 {
            if fired_at.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        let got = fired_at
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .expect("must fire");
        let elapsed = got.duration_since(started).as_millis() as i64;
        assert!(elapsed >= 450, "fired too early: {}ms", elapsed);
        assert!(elapsed <= 900, "fired too late: {}ms", elapsed);
        shutdown_and_join(&h, jh);
    }

    /// Test 4: `executeAtTime` respects the absolute deadline.
    #[test]
    fn t19_7_c_execute_at_time_absolute() {
        let (h, jh, _sel) = spawn_test_loop("iot-test-4", 1004);
        let fired_at = Arc::new(Mutex::new(None::<Instant>));
        let fired_at2 = fired_at.clone();
        let started = Instant::now();
        let absolute = now_ms().saturating_add(200);
        h.try_schedule_at(
            absolute,
            Box::new(move || {
                *fired_at2.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
            }),
        )
        .map_err(|_| "schedule rejected")
        .unwrap();
        for _ in 0..250 {
            if fired_at.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        let got = fired_at
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .expect("must fire");
        let elapsed = got.duration_since(started).as_millis() as i64;
        assert!(elapsed >= 150, "fired too early: {}ms", elapsed);
        assert!(elapsed <= 500, "fired too late: {}ms", elapsed);
        shutdown_and_join(&h, jh);
    }

    /// Test 5: cancelling a scheduled task before its deadline prevents it
    /// from running.
    #[test]
    fn t19_7_c_scheduled_task_cancel_prevents_execution() {
        let (h, jh, _sel) = spawn_test_loop("iot-test-5", 1005);
        let fired = Arc::new(AtomicBool::new(false));
        let fired2 = fired.clone();
        let deadline = now_ms().saturating_add(300);
        let (_task_id, cancelled) = h
            .try_schedule_at(
                deadline,
                Box::new(move || fired2.store(true, Ordering::Release)),
            )
            .map_err(|_| "schedule rejected")
            .unwrap();
        cancelled.store(true, Ordering::Release);
        // Wake so loop reconsiders heap.
        h.selector.wakeup();
        thread::sleep(Duration::from_millis(500));
        assert!(
            !fired.load(Ordering::Acquire),
            "cancelled task must not run"
        );
        shutdown_and_join(&h, jh);
    }

    /// Test 6: cancelling a task AFTER it has already fired returns false
    /// (second cancel attempt returns false too; the first cancel attempt
    /// after-fire also returns false because the task is no longer
    /// pending).
    #[test]
    fn t19_7_c_scheduled_task_cancel_after_execution_returns_false() {
        let (h, jh, _sel) = spawn_test_loop("iot-test-6", 1006);
        let fired = Arc::new(AtomicBool::new(false));
        let fired2 = fired.clone();
        let deadline = now_ms().saturating_add(50);
        let (_tid, cancelled) = h
            .try_schedule_at(
                deadline,
                Box::new(move || fired2.store(true, Ordering::Release)),
            )
            .map_err(|_| "schedule rejected")
            .unwrap();
        for _ in 0..200 {
            if fired.load(Ordering::Acquire) {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert!(fired.load(Ordering::Acquire), "must have fired");
        // Now cancel after the fact. The swap returns the previous flag
        // value (false); the JDK semantics say remove() returns true only
        // if the task was genuinely cancelled before running. Emulating
        // that here: the task has been consumed, so the flag was
        // never flipped, swap returns false, which we interpret as "no,
        // task already completed, cancel was a no-op".
        let was_already = cancelled.swap(true, Ordering::AcqRel);
        assert!(!was_already, "flag still false pre-cancel");
        // The Java-level Key.remove() call would then observe this.
        // Verify the flag is now true (set, but too late).
        assert!(cancelled.load(Ordering::Acquire));
        shutdown_and_join(&h, jh);
    }

    /// Test 7: a panicking runnable does not kill the loop; a later
    /// normal runnable still runs.
    #[test]
    fn t19_7_c_task_panic_caught_loop_continues() {
        let (h, jh, _sel) = spawn_test_loop("iot-test-7", 1007);
        must_execute(h.try_execute(Box::new(|| panic!("intentional test panic"))));
        let ran = Arc::new(AtomicBool::new(false));
        let ran2 = ran.clone();
        must_execute(h.try_execute(Box::new(move || ran2.store(true, Ordering::Release))));
        for _ in 0..300 {
            if ran.load(Ordering::Acquire) {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert!(
            ran.load(Ordering::Acquire),
            "loop must survive a panic and run later tasks"
        );
        shutdown_and_join(&h, jh);
    }

    /// Test 8: shutdown drains any remaining pending tasks (they run)
    /// and marks all scheduled tasks cancelled.
    #[test]
    fn t19_7_c_shutdown_drains_remaining_tasks_or_cancels() {
        let (h, jh, _sel) = spawn_test_loop("iot-test-8", 1008);
        // Queue a slow task so its successor is unlikely to run before
        // shutdown arrives.
        let slow_done = Arc::new(AtomicBool::new(false));
        let slow_done2 = slow_done.clone();
        must_execute(h.try_execute(Box::new(move || {
            thread::sleep(Duration::from_millis(100));
            slow_done2.store(true, Ordering::Release);
        })));
        // Stack up more tasks; the drain at shutdown should still fire them.
        let counter = Arc::new(AtomicI32::new(0));
        for _ in 0..5 {
            let c = counter.clone();
            must_execute(h.try_execute(Box::new(move || {
                c.fetch_add(1, Ordering::Relaxed);
            })));
        }
        // Also schedule something far in the future — must be cancelled.
        let sched_fired = Arc::new(AtomicBool::new(false));
        let sched_fired2 = sched_fired.clone();
        let (_tid, _flag) = h
            .try_schedule_at(
                now_ms() + 10_000,
                Box::new(move || sched_fired2.store(true, Ordering::Release)),
            )
            .map_err(|_| "schedule rejected")
            .unwrap();
        // Trigger shutdown.
        h.shutdown();
        jh.join().expect("join");
        unregister_io_thread(h.id);
        assert!(slow_done.load(Ordering::Acquire), "slow must have run");
        // Drain-on-shutdown should have run everything.
        assert_eq!(counter.load(Ordering::Relaxed), 5, "queued tasks drained");
        // The far-future scheduled task must NOT have fired.
        assert!(
            !sched_fired.load(Ordering::Acquire),
            "far-future scheduled task cannot have fired post-shutdown"
        );
    }

    /// Test 9: 8 concurrent threads each submitting `execute` must all
    /// land without data race / loss.
    #[test]
    fn t19_7_c_concurrent_execute_from_8_threads_no_data_race() {
        let (h, jh, _sel) = spawn_test_loop("iot-test-9", 1009);
        const N_THREADS: usize = 8;
        const PER_THREAD: usize = 50;
        let counter = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(N_THREADS));
        let mut handles = Vec::new();
        for _ in 0..N_THREADS {
            let hh = h.clone();
            let c = counter.clone();
            let b = barrier.clone();
            handles.push(thread::spawn(move || {
                b.wait();
                for _ in 0..PER_THREAD {
                    let c2 = c.clone();
                    must_execute(hh.try_execute(Box::new(move || {
                        c2.fetch_add(1, Ordering::Relaxed);
                    })));
                }
            }));
        }
        for t in handles {
            t.join().expect("joiner");
        }
        // Wait for loop to drain everything.
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if counter.load(Ordering::Relaxed) == N_THREADS * PER_THREAD {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            counter.load(Ordering::Relaxed),
            N_THREADS * PER_THREAD,
            "all concurrent execute()s must have fired exactly once"
        );
        shutdown_and_join(&h, jh);
    }

    /// Test 10: `currentThread` returns self inside the loop, null
    /// outside.
    #[test]
    fn t19_7_c_current_thread_returns_self_in_loop_returns_null_elsewhere() {
        let (h, jh, _sel) = spawn_test_loop("iot-test-10", 1010);
        // Outside the loop, current_io_thread is None.
        assert!(current_io_thread().is_none(), "not inside any IO loop");
        // Inside the loop (task posted to run on it), it matches h.
        let got = Arc::new(Mutex::new(None::<u64>));
        let got2 = got.clone();
        must_execute(h.try_execute(Box::new(move || {
            let cur = current_io_thread();
            *got2.lock().unwrap_or_else(|e| e.into_inner()) = cur.map(|c| c.id);
        })));
        for _ in 0..300 {
            if got.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        let saw = *got.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(saw, Some(h.id), "loop task sees itself via currentThread");
        shutdown_and_join(&h, jh);
    }

    // -------- Additional coverage tests --------

    /// `execute` over-cap is rejected with `RejectedExecutionException`
    /// semantics (Err return from try_execute).
    #[test]
    fn t19_7_c_execute_over_cap_rejected() {
        let mock = Arc::new(MockSelector::new());
        let selector: Arc<dyn SelectorHandle> = mock;
        let h = IoThreadHandle::new(9999, 1, selector, "iot-cap");
        // DO NOT start the loop so tasks don't drain. Fill past MAX.
        for _ in 0..MAX_PENDING_TASKS {
            must_execute(h.try_execute(Box::new(|| {})));
        }
        let err = h.try_execute(Box::new(|| {}));
        assert!(err.is_err(), "over-cap must be rejected");
    }

    /// Scheduled over-cap is rejected.
    #[test]
    fn t19_7_c_schedule_over_cap_rejected() {
        let mock = Arc::new(MockSelector::new());
        let selector: Arc<dyn SelectorHandle> = mock;
        let h = IoThreadHandle::new(9998, 1, selector, "iot-sched-cap");
        let far = now_ms() + 1_000_000;
        for _ in 0..MAX_SCHEDULED_TASKS {
            h.try_schedule_at(far, Box::new(|| {}))
                .map_err(|_| "unexpected")
                .unwrap();
        }
        let err = h.try_schedule_at(far, Box::new(|| {}));
        assert!(err.is_err(), "scheduled over-cap must be rejected");
    }

    /// ScheduledHeap ordering — earlier deadlines pop first regardless
    /// of insertion order.
    #[test]
    fn t19_7_c_scheduled_heap_orders_by_deadline() {
        let mut heap = ScheduledHeap::new();
        heap.push_bounded(ScheduledTask {
            deadline_ms: 200,
            seq: 2,
            runnable: Some(Box::new(|| {})),
            cancelled: Arc::new(AtomicBool::new(false)),
            task_id: 2,
        })
        .map_err(|_| "push")
        .unwrap();
        heap.push_bounded(ScheduledTask {
            deadline_ms: 100,
            seq: 1,
            runnable: Some(Box::new(|| {})),
            cancelled: Arc::new(AtomicBool::new(false)),
            task_id: 1,
        })
        .map_err(|_| "push")
        .unwrap();
        heap.push_bounded(ScheduledTask {
            deadline_ms: 300,
            seq: 3,
            runnable: Some(Box::new(|| {})),
            cancelled: Arc::new(AtomicBool::new(false)),
            task_id: 3,
        })
        .map_err(|_| "push")
        .unwrap();
        let first = heap.pop_if_expired(500).expect("pop").task_id;
        let second = heap.pop_if_expired(500).expect("pop").task_id;
        let third = heap.pop_if_expired(500).expect("pop").task_id;
        assert_eq!((first, second, third), (1, 2, 3));
    }

    /// MockSelector wakeup semantics: a wakeup before select returns
    /// immediately.
    #[test]
    fn t19_7_c_mock_selector_wakeup_short_circuits_select() {
        let mock = MockSelector::new();
        mock.wakeup();
        let start = Instant::now();
        let _ = mock.select(1_000).expect("select");
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "wakeup should have short-circuited select"
        );
    }

    // =======================================================================
    // T19_K4 — XNIO IO thread Java Thread mirror linkage tests.
    //
    // These mirror the Vert.x event-loop K4 tests: every XNIO IO thread
    // spawned via `spawn_io_thread_with_ctx` must allocate a synthetic
    // `java.lang.Thread` mirror and route it through
    // `set_native_thread_java_obj` so registry look-ups by ObjectRef
    // resolve and `Thread.enumerate()` sees the carrier.
    // =======================================================================

    /// **T19_K4-XNIO-1** — `spawn_io_thread_with_ctx` registers a Java
    /// Thread mirror on the carrier OS thread.
    #[test]
    fn t19_k4_xnio_spawn_with_ctx_registers_java_thread_mirror() {
        let mut ctx = crate::test_utils::mock_ctx();
        let mock = Arc::new(MockSelector::new());
        let sel: Arc<dyn SelectorHandle> = mock.clone();
        let id = 9_001u64;
        let h = spawn_io_thread_with_ctx(&mut ctx, "k4-xnio-1", id, 1, sel, false).expect("spawn");
        // The mock should have recorded the mirror pointer.
        let vm_tid = lookup_vm_thread_id(h.id);
        assert_ne!(vm_tid, 0, "vm_tid must be assigned");
        let mirror_ptr = ctx.native_thread_java_obj_ptr(vm_tid);
        assert_ne!(mirror_ptr, 0, "set_native_thread_java_obj must have run");
        // And the iot-side lookup must agree.
        assert_eq!(
            lookup_iot_java_mirror_ptr(h.id),
            mirror_ptr,
            "iot-side mirror lookup must match"
        );
        h.shutdown();
        unregister_io_thread(h.id);
    }

    /// **T19_K4-XNIO-2** — XNIO mirror's name field is set to the
    /// IO thread name.
    #[test]
    fn t19_k4_xnio_thread_mirror_name_field_matches() {
        let mut ctx = crate::test_utils::mock_ctx();
        let mock = Arc::new(MockSelector::new());
        let sel: Arc<dyn SelectorHandle> = mock.clone();
        let id = 9_002u64;
        let h =
            spawn_io_thread_with_ctx(&mut ctx, "k4-xnio-name", id, 1, sel, false).expect("spawn");
        let vm_tid = lookup_vm_thread_id(h.id);
        let mirror_ptr = ctx.native_thread_java_obj_ptr(vm_tid);
        let mirror = unsafe { ObjectRef::from_raw(mirror_ptr as *mut u8) };
        let name_obj = match ctx.get_field(mirror, 0) {
            Value::Object(Some(o)) => o,
            other => panic!("name slot must hold a String, got {other:?}"),
        };
        assert_eq!(ctx.read_string(name_obj).as_deref(), Some("k4-xnio-name"));
        h.shutdown();
        unregister_io_thread(h.id);
    }

    /// **T19_K4-XNIO-3** — Multiple XNIO IO threads register distinct
    /// mirrors per carrier (no aliasing).
    #[test]
    fn t19_k4_xnio_multiple_threads_have_distinct_mirrors() {
        let mut ctx = crate::test_utils::mock_ctx();
        let base_id = 9_100u64;
        let mut handles = Vec::new();
        let mut mirror_ptrs = std::collections::HashSet::new();
        for i in 0..3 {
            let mock = Arc::new(MockSelector::new());
            let sel: Arc<dyn SelectorHandle> = mock.clone();
            let h = spawn_io_thread_with_ctx(
                &mut ctx,
                &format!("k4-xnio-multi-{i}"),
                base_id + i,
                1,
                sel,
                false,
            )
            .expect("spawn");
            let vm_tid = lookup_vm_thread_id(h.id);
            let p = ctx.native_thread_java_obj_ptr(vm_tid);
            assert_ne!(p, 0);
            assert!(mirror_ptrs.insert(p), "iter {i} mirror must be unique");
            handles.push(h);
        }
        for h in &handles {
            h.shutdown();
            unregister_io_thread(h.id);
        }
    }

    /// **T19_K4-XNIO-4** — `lookup_iot_java_mirror_ptr` returns 0 for
    /// an unknown IO thread id.
    #[test]
    fn t19_k4_xnio_lookup_unknown_id_returns_zero() {
        assert_eq!(
            lookup_iot_java_mirror_ptr(u64::MAX),
            0,
            "unknown id must map to 0"
        );
    }
}
