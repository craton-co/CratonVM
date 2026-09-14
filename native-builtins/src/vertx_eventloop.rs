// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! T19.6 — Vert.x / Netty NioEventLoop native bindings.
//!
//! Registers natives for `io.vertx.core.impl.VertxImpl` and
//! `io.netty.channel.nio.NioEventLoop` that Keycloak 26 needs when
//! bootstrapping Vert.x's HTTP serving layer.
//!
//! # Architecture
//!
//! This module is self-contained within `native-builtins` (which cannot
//! depend on `cratonvm-vm`).  It embeds its own event-loop runtime:
//!
//! * A process-wide `VertxEventLoopRegistry` keyed by a stable u64 id.
//! * Each `VertxEventLoop` runs a 5-phase loop identical in structure to
//!   `T19.7.c` (`xnio_io_thread.rs`):
//!   1. drain `task_queue`
//!   2. compute next timer deadline from `timer_heap`
//!   3. park on `WakeableCondvar` until deadline or wakeup
//!   4. (no channel events at this layer — Netty/Vert.x Java bytecode drives
//!      the JDK `Selector.select()` on this thread directly)
//!   5. pop + fire expired timers
//! * Per-task `catch_unwind(AssertUnwindSafe)` panic isolation.
//! * `DispatchStats` counters: tasks_run, timers_fired, panic_count, wakeups.
//!
//! # Synthetic field layouts
//!
//! | Class                                         | Slots | Layout                                      |
//! |-----------------------------------------------|-------|---------------------------------------------|
//! | `io/vertx/core/impl/VertxImpl`                |   4   | event_loop_id(Long), pool_size(Int), state(Int), name(Object) |
//! | `io/netty/channel/nio/NioEventLoop`           |   3   | event_loop_id(Long), state(Int), parent(Object) |
//! | `io/netty/channel/DefaultEventLoop`           |   3   | event_loop_id(Long), state(Int), parent(Object) |
//! | `io/netty/util/concurrent/SingleThreadEventExecutor` | 3 | event_loop_id(Long), state(Int), parent(Object) |
//!
//! # Natives registered
//!
//! `VertxImpl`:
//!   * `init(I)V` — allocate `eventLoopPoolSize` event loops
//!   * `getEventLoopId()J` — return field 0
//!   * `close()V` — shut down all owned loops
//!
//! `NioEventLoop` / `DefaultEventLoop` / `SingleThreadEventExecutor`:
//!   * `run()V`
//!   * `execute(Ljava/lang/Runnable;)V`
//!   * `schedule(Ljava/lang/Runnable;JLjava/util/concurrent/TimeUnit;)Ljava/util/concurrent/ScheduledFuture;`
//!   * `inEventLoop()Z`
//!   * `isShuttingDown()Z`
//!   * `shutdownGracefully(JJLjava/util/concurrent/TimeUnit;)Lio/netty/util/concurrent/Future;`
//!   * `awaitTermination(JLjava/util/concurrent/TimeUnit;)Z`
//!   * `submit(Ljava/lang/Runnable;)Ljava/util/concurrent/Future;`
//!
//! # Security
//!
//! `native_vertx_init` validates `poolSize` in `1..=MAX_POOL_SIZE` (256) and
//! rejects out-of-range values.  Every native that reads an event-loop id
//! validates it via `EventLoopId::from_raw` (positive, ≤ hwm) before
//! touching the registry.
//!
//! # Panic isolation
//!
//! Every queued task / timer fires under `catch_unwind(AssertUnwindSafe)`.
//! Panics are logged via `tracing::error!`, increment `DispatchStats::panic_count`,
//! and the loop continues.

#![allow(clippy::needless_pass_by_value, dead_code)]

use std::cell::{Cell, RefCell};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::thread;
use std::time::{Duration, Instant};

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ObjectRef, Value};

use crate::{obj_arg, try_alloc_concurrent_synthetic};

// ---------------------------------------------------------------------------
// Class name constants
// ---------------------------------------------------------------------------

const CLS_VERTX_IMPL: &str = "io/vertx/core/impl/VertxImpl";
const CLS_NIO_EVENT_LOOP: &str = "io/netty/channel/nio/NioEventLoop";
const CLS_DEFAULT_EVENT_LOOP: &str = "io/netty/channel/DefaultEventLoop";
const CLS_STE: &str = "io/netty/util/concurrent/SingleThreadEventExecutor";
const CLS_FUTURE: &str = "io/netty/util/concurrent/Future";
const CLS_SCHEDULED_FUTURE: &str = "java/util/concurrent/ScheduledFuture";

// ---------------------------------------------------------------------------
// Synthetic field offsets
// ---------------------------------------------------------------------------

// VertxImpl (4 slots)
pub const VERTX_FIELD_EVENT_LOOP_ID: usize = 0;
pub const VERTX_FIELD_POOL_SIZE: usize = 1;
pub const VERTX_FIELD_STATE: usize = 2;
pub const VERTX_FIELD_NAME: usize = 3;
pub const VERTX_NUM_SLOTS: usize = 4;

// NioEventLoop / DefaultEventLoop / SingleThreadEventExecutor (3 slots)
pub const NEL_FIELD_EVENT_LOOP_ID: usize = 0;
pub const NEL_FIELD_STATE: usize = 1;
pub const NEL_FIELD_PARENT: usize = 2;
pub const NEL_NUM_SLOTS: usize = 3;

const NEL_EXEC_DRAIN_LIMIT: usize = 16_384;

thread_local! {
    static NEL_EXEC_DEPTH: Cell<usize> = const { Cell::new(0) };
    static NEL_EXEC_QUEUE: RefCell<VecDeque<ObjectRef>> = RefCell::new(VecDeque::new());
}

// State codes
pub const STATE_NOT_STARTED: i32 = 0;
pub const STATE_STARTED: i32 = 1;
pub const STATE_SHUTTING_DOWN: i32 = 2;
pub const STATE_TERMINATED: i32 = 3;

// T19_K4 — the synthetic `java.lang.Thread` mirror layout used to be declared
// here (`THREAD_MIRROR_SLOTS/NAME_SLOT/TID_SLOT`). It moved to
// `crate::SYNTHETIC_THREAD_MIRROR_*` on 2026-08-12
// (W7-74-short-object-repairs.md) with the allocation itself: the width was
// being handed to `alloc_object(ClassId::new(0), …)` on EVERY image, including
// real ones where `java.lang.Thread` declares 19, and `xnio_io_thread.rs`
// carried an open-coded copy of the same three numbers. One declaration, one
// allocator (`crate::alloc_carrier_thread_mirror`), which asks the class.

// ---------------------------------------------------------------------------
// Resource caps
// ---------------------------------------------------------------------------

/// Maximum event-loop pool size a single `VertxImpl.init` call may request.
pub const MAX_POOL_SIZE: i32 = 256;
/// Per-loop immediate-task queue cap.
pub const MAX_PENDING_TASKS: usize = 10_000;
/// Per-loop scheduled-timer heap cap.
pub const MAX_SCHEDULED_TIMERS: usize = 1_000;
/// Maximum idle park duration between iterations.
const MAX_IDLE_PARK: Duration = Duration::from_millis(1_000);

// ===========================================================================
//  VertxEventLoop — the pinned OS-thread event-loop state
// ===========================================================================

// ---------------------------------------------------------------------------
// DispatchStats
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct DispatchStats {
    pub tasks_run: AtomicU64,
    pub timers_fired: AtomicU64,
    pub panic_count: AtomicU64,
    pub wakeups_sent: AtomicU64,
    pub select_calls: AtomicU64,
}

/// Immutable snapshot.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DispatchStatsSnapshot {
    pub tasks_run: u64,
    pub timers_fired: u64,
    pub panic_count: u64,
    pub wakeups_sent: u64,
    pub select_calls: u64,
}

impl DispatchStats {
    pub fn snapshot(&self) -> DispatchStatsSnapshot {
        DispatchStatsSnapshot {
            tasks_run: self.tasks_run.load(Ordering::Acquire),
            timers_fired: self.timers_fired.load(Ordering::Acquire),
            panic_count: self.panic_count.load(Ordering::Acquire),
            wakeups_sent: self.wakeups_sent.load(Ordering::Acquire),
            select_calls: self.select_calls.load(Ordering::Acquire),
        }
    }
}

// ---------------------------------------------------------------------------
// ELTask — opaque work unit
// ---------------------------------------------------------------------------

pub type ELTask = Box<dyn FnOnce() + Send + 'static>;

// ---------------------------------------------------------------------------
// ScheduledTimer + heap
// ---------------------------------------------------------------------------

pub struct ScheduledTimer {
    pub deadline: Instant,
    pub seq: u64,
    pub task: Option<ELTask>,
    pub cancelled: Arc<AtomicBool>,
    pub timer_id: u64,
}

impl std::fmt::Debug for ScheduledTimer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScheduledTimer")
            .field("deadline", &self.deadline)
            .field("seq", &self.seq)
            .field("timer_id", &self.timer_id)
            .field("cancelled", &self.cancelled.load(Ordering::Acquire))
            .finish()
    }
}
impl PartialEq for ScheduledTimer {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline && self.seq == other.seq
    }
}
impl Eq for ScheduledTimer {}
impl PartialOrd for ScheduledTimer {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for ScheduledTimer {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.deadline
            .cmp(&other.deadline)
            .then(self.seq.cmp(&other.seq))
    }
}

/// Min-heap wrapper.
pub struct TimerHeap {
    inner: BinaryHeap<Reverse<ScheduledTimer>>,
}

impl TimerHeap {
    fn new() -> Self {
        Self {
            inner: BinaryHeap::new(),
        }
    }

    fn len(&self) -> usize {
        self.inner.len()
    }

    fn peek_deadline(&mut self) -> Option<Instant> {
        loop {
            match self.inner.peek() {
                None => return None,
                Some(Reverse(top)) => {
                    if top.cancelled.load(Ordering::Acquire) {
                        self.inner.pop();
                        continue;
                    }
                    return Some(top.deadline);
                }
            }
        }
    }

    fn pop_if_expired(&mut self, now: Instant) -> Option<ScheduledTimer> {
        loop {
            let top = self.inner.peek()?;
            if top.0.cancelled.load(Ordering::Acquire) {
                self.inner.pop();
                continue;
            }
            if top.0.deadline > now {
                return None;
            }
            return self.inner.pop().map(|Reverse(t)| t);
        }
    }

    fn push_bounded(&mut self, t: ScheduledTimer) -> Result<(), ScheduledTimer> {
        if self.inner.len() >= MAX_SCHEDULED_TIMERS {
            return Err(t);
        }
        self.inner.push(Reverse(t));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// WakeableCondvar — deduped cross-thread wake
// ---------------------------------------------------------------------------

pub struct WakeableCondvar {
    mu: Mutex<bool>,
    cv: Condvar,
    pub pending: AtomicBool,
}

impl Default for WakeableCondvar {
    fn default() -> Self {
        Self::new()
    }
}

impl WakeableCondvar {
    pub fn new() -> Self {
        Self {
            mu: Mutex::new(false),
            cv: Condvar::new(),
            pending: AtomicBool::new(false),
        }
    }

    /// Park for up to `timeout`. Returns `true` if woken (not timeout).
    pub fn park(&self, timeout: Duration) -> bool {
        let mut guard = self.mu.lock().unwrap_or_else(|e| e.into_inner());
        if *guard {
            *guard = false;
            self.pending.store(false, Ordering::Release);
            return true;
        }
        let (g, res) = self
            .cv
            .wait_timeout(guard, timeout)
            .unwrap_or_else(|e| e.into_inner());
        let mut g = g;
        let woken = *g;
        *g = false;
        self.pending.store(false, Ordering::Release);
        woken || !res.timed_out() && woken
    }

    pub fn wake(&self) {
        if self.pending.swap(true, Ordering::AcqRel) {
            return; // already pending
        }
        let mut guard = self.mu.lock().unwrap_or_else(|e| e.into_inner());
        *guard = true;
        self.cv.notify_one();
    }
}

// ---------------------------------------------------------------------------
// VertxEventLoop — the main loop state struct
// ---------------------------------------------------------------------------

/// Shared state for one pinned Vert.x / Netty event-loop OS thread.
pub struct VertxEventLoop {
    /// Stable id — the value stored in the Java mirror's `event_loop_id` field.
    pub id: u64,
    /// Human-readable label for tracing / panic logs.
    pub name: String,
    task_queue: Mutex<VecDeque<ELTask>>,
    timer_heap: Mutex<TimerHeap>,
    parker: WakeableCondvar,
    pub shutdown_requested: AtomicBool,
    pub loop_thread_id: AtomicU64,
    seq_counter: AtomicU64,
    timer_id_counter: AtomicU64,
    pending_len: AtomicUsize,
    pub stats: DispatchStats,
    /// **T19_K2** — VM `ThreadRegistry` id assigned by
    /// `NativeContext::register_native_thread` when the loop is spawned
    /// via [`spawn_vertx_event_loop_with_ctx`]. `0` means "not VM-tracked"
    /// (loop spawned via the legacy [`spawn_vertx_event_loop`] path used
    /// by unit tests). Read-only after spawn; the loop body's exit
    /// trampoline reads it to push a "thread dead" notification onto
    /// [`NATIVE_THREAD_DEAD_QUEUE`].
    pub vm_thread_id: AtomicU64,
    /// **T19_K4** — Raw `ObjectRef::as_ptr() as usize` of the
    /// `java.lang.Thread` mirror linked to this loop's carrier OS
    /// thread (or 0 if none was allocated — the legacy
    /// [`spawn_vertx_event_loop`] path or a `register` call where
    /// the VM rejected the attach).
    ///
    /// Purpose: lets `current_vertx_loop()` callers ask "what's the
    /// `Thread` mirror for this loop" without taking the
    /// `EventLoopManager` lock. The pointer is stable for the loop's
    /// entire lifetime (the mirror is allocated once at spawn and
    /// the GC won't collect it because the registry holds a strong
    /// reference until `unregister_native_thread`).
    pub java_thread_mirror_ptr: AtomicUsize,
}

impl VertxEventLoop {
    pub fn new(id: u64, name: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            id,
            name: name.into(),
            task_queue: Mutex::new(VecDeque::new()),
            timer_heap: Mutex::new(TimerHeap::new()),
            parker: WakeableCondvar::new(),
            shutdown_requested: AtomicBool::new(false),
            loop_thread_id: AtomicU64::new(0),
            seq_counter: AtomicU64::new(0),
            timer_id_counter: AtomicU64::new(0),
            pending_len: AtomicUsize::new(0),
            stats: DispatchStats::default(),
            vm_thread_id: AtomicU64::new(0),
            java_thread_mirror_ptr: AtomicUsize::new(0),
        })
    }

    pub fn shutdown(&self) {
        self.shutdown_requested.store(true, Ordering::Release);
        self.parker.wake();
    }

    pub fn is_shutdown(&self) -> bool {
        self.shutdown_requested.load(Ordering::Acquire)
    }

    /// Push an immediate task.  Cross-thread calls wake the parker.
    pub fn schedule_task(&self, task: ELTask) -> Result<(), ELTask> {
        if self.shutdown_requested.load(Ordering::Acquire) {
            return Err(task);
        }
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
        let loop_tid = self.loop_thread_id.load(Ordering::Acquire);
        let my_tid = os_tid();
        if loop_tid == 0 || loop_tid != my_tid {
            self.stats.wakeups_sent.fetch_add(1, Ordering::Relaxed);
            self.parker.wake();
        }
        Ok(())
    }

    /// Schedule a timer at `deadline`. Returns `(timer_id, cancelled_flag)`.
    pub fn schedule_timer(
        &self,
        deadline: Instant,
        task: ELTask,
    ) -> Result<(u64, Arc<AtomicBool>), ELTask> {
        if self.shutdown_requested.load(Ordering::Acquire) {
            return Err(task);
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        let timer_id = self.timer_id_counter.fetch_add(1, Ordering::Relaxed) + 1;
        let seq = self.seq_counter.fetch_add(1, Ordering::Relaxed);
        let timer = ScheduledTimer {
            deadline,
            seq,
            task: Some(task),
            cancelled: cancelled.clone(),
            timer_id,
        };
        {
            let mut heap = self.timer_heap.lock().unwrap_or_else(|e| e.into_inner());
            if let Err(back) = heap.push_bounded(timer) {
                return Err(back.task.unwrap_or_else(|| Box::new(|| {})));
            }
        }
        let loop_tid = self.loop_thread_id.load(Ordering::Acquire);
        let my_tid = os_tid();
        if loop_tid == 0 || loop_tid != my_tid {
            self.parker.wake();
        }
        Ok((timer_id, cancelled))
    }

    pub fn pending_count(&self) -> usize {
        self.pending_len.load(Ordering::Acquire)
    }
}

// ---------------------------------------------------------------------------
// run_vertx_event_loop — 5-phase loop
// ---------------------------------------------------------------------------

/// Run the event loop for `el`.  Blocks until `el.shutdown()` is called.
/// Called from a dedicated OS thread.
pub fn run_vertx_event_loop(el: Arc<VertxEventLoop>) {
    el.loop_thread_id.store(os_tid(), Ordering::Release);

    CURRENT_VERTX_LOOP.with(|slot| {
        *slot.borrow_mut() = Some(Arc::downgrade(&el));
    });

    while !el.shutdown_requested.load(Ordering::Acquire) {
        // --- phase 1: drain immediate tasks --------------------------------
        loop {
            let next = {
                let mut q = el.task_queue.lock().unwrap_or_else(|e| e.into_inner());
                let t = q.pop_front();
                el.pending_len.store(q.len(), Ordering::Release);
                t
            };
            match next {
                Some(task) => run_el_task(&el, task),
                None => break,
            }
        }

        if el.shutdown_requested.load(Ordering::Acquire) {
            break;
        }

        // --- phase 2 + 3: compute deadline and park ------------------------
        let now = Instant::now();
        let park_dur = {
            let mut heap = el.timer_heap.lock().unwrap_or_else(|e| e.into_inner());
            match heap.peek_deadline() {
                Some(d) if d <= now => Duration::from_millis(0),
                Some(d) => (d - now).min(MAX_IDLE_PARK),
                None => MAX_IDLE_PARK,
            }
        };
        el.stats.select_calls.fetch_add(1, Ordering::Relaxed);
        if park_dur > Duration::from_millis(0) {
            el.parker.park(park_dur);
        }

        if el.shutdown_requested.load(Ordering::Acquire) {
            break;
        }

        // --- phase 4: channel events (no-op — Netty Java bytecode drives
        //              its own Selector.select() on this OS thread) ----------

        // --- phase 5: fire expired timers ----------------------------------
        let now = Instant::now();
        loop {
            let expired = {
                let mut heap = el.timer_heap.lock().unwrap_or_else(|e| e.into_inner());
                heap.pop_if_expired(now)
            };
            match expired {
                None => break,
                Some(mut t) => {
                    if t.cancelled.load(Ordering::Acquire) {
                        continue;
                    }
                    if let Some(task) = t.task.take() {
                        el.stats.timers_fired.fetch_add(1, Ordering::Relaxed);
                        run_el_task(&el, task);
                    }
                }
            }
        }
    }

    // Drain on shutdown.
    loop {
        let next = {
            let mut q = el.task_queue.lock().unwrap_or_else(|e| e.into_inner());
            q.pop_front()
        };
        match next {
            Some(task) => run_el_task(&el, task),
            None => break,
        }
    }

    // Cancel remaining timers.
    {
        let mut heap = el.timer_heap.lock().unwrap_or_else(|e| e.into_inner());
        while let Some(Reverse(t)) = heap.inner.pop() {
            t.cancelled.store(true, Ordering::Release);
        }
    }

    CURRENT_VERTX_LOOP.with(|slot| {
        *slot.borrow_mut() = None;
    });
}

fn run_el_task(el: &VertxEventLoop, task: ELTask) {
    let name = el.name.clone();
    let result = catch_unwind(AssertUnwindSafe(|| task()));
    match result {
        Ok(()) => {
            el.stats.tasks_run.fetch_add(1, Ordering::Relaxed);
        }
        Err(payload) => {
            el.stats.panic_count.fetch_add(1, Ordering::Relaxed);
            let msg = describe_panic(&payload);
            tracing::error!(
                event_loop = %name,
                loop_id = el.id,
                panic = %msg,
                "vertx-eventloop: task panicked; swallowed and continuing",
            );
        }
    }
}

fn describe_panic(payload: &Box<dyn std::any::Any + Send + 'static>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic>".to_string()
    }
}

// ---------------------------------------------------------------------------
// TLS — current event loop for the OS thread
// ---------------------------------------------------------------------------

thread_local! {
    static CURRENT_VERTX_LOOP: std::cell::RefCell<Option<Weak<VertxEventLoop>>>
        = const { std::cell::RefCell::new(None) };
}

pub fn current_vertx_loop() -> Option<Arc<VertxEventLoop>> {
    CURRENT_VERTX_LOOP.with(|slot| slot.borrow().as_ref().and_then(|w| w.upgrade()))
}

// ---------------------------------------------------------------------------
// Process-wide registry
// ---------------------------------------------------------------------------

static VERTX_LOOP_REGISTRY: OnceLock<Mutex<HashMap<u64, Arc<VertxEventLoop>>>> = OnceLock::new();
static VERTX_LOOP_JOIN_HANDLES: OnceLock<Mutex<HashMap<u64, thread::JoinHandle<()>>>> =
    OnceLock::new();
static NEXT_LOOP_ID: AtomicU64 = AtomicU64::new(1);
/// High-water mark: the next id that has NOT been allocated yet.
static LOOP_ID_HWM: AtomicU64 = AtomicU64::new(1);

fn vertx_loop_registry() -> &'static Mutex<HashMap<u64, Arc<VertxEventLoop>>> {
    VERTX_LOOP_REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn vertx_loop_join_handles() -> &'static Mutex<HashMap<u64, thread::JoinHandle<()>>> {
    VERTX_LOOP_JOIN_HANDLES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Allocate a new id and spawn an OS thread running `run_vertx_event_loop`.
/// Returns `Arc<VertxEventLoop>`.
///
/// **T19_K2**: this variant does NOT register the OS thread with the VM
/// `ThreadRegistry`. It is kept for unit tests + standalone usages where
/// no `NativeContext` is available. Production native paths
/// (`native_vertx_init`, `native_nel_run`) should use
/// [`spawn_vertx_event_loop_with_ctx`] instead so the VM holds the
/// process alive past `main()` while the event loop is running.
pub fn spawn_vertx_event_loop(name: impl Into<String>) -> Result<Arc<VertxEventLoop>, String> {
    spawn_vertx_event_loop_inner(name, None)
}

/// **T19_K2** — same as [`spawn_vertx_event_loop`] but registers the
/// new OS thread with the VM's `ThreadRegistry` as a non-daemon thread
/// (so [`wait_for_non_daemon_threads`] keeps the JVM process alive past
/// `main()` for as long as this loop is running).
///
/// `daemon = false` is the right default for Vert.x / Netty event loops
/// per the JDK contract: `Thread::isDaemon()` defaults to false on
/// freshly-constructed `Thread` objects, and Vert.x's bootstrap does
/// not call `setDaemon(true)` on its event-loop carriers.
///
/// Returns the `Arc<VertxEventLoop>`. The registered VM ThreadId is
/// stored on the loop itself (see [`VertxEventLoop::vm_thread_id`]) so
/// the run-loop body's exit path can flip the registry's `alive` flag
/// to false once the loop terminates.
pub fn spawn_vertx_event_loop_with_ctx(
    ctx: &mut dyn NativeContext,
    name: impl Into<String>,
    daemon: bool,
) -> Result<Arc<VertxEventLoop>, String> {
    spawn_vertx_event_loop_inner(name, Some((ctx, daemon)))
}

fn spawn_vertx_event_loop_inner(
    name: impl Into<String>,
    mut register: Option<(&mut dyn NativeContext, bool)>,
) -> Result<Arc<VertxEventLoop>, String> {
    let name = name.into();
    let id = NEXT_LOOP_ID.fetch_add(1, Ordering::Relaxed);
    LOOP_ID_HWM.store(id + 1, Ordering::Release);
    let el = VertxEventLoop::new(id, name.clone());

    // T19_K2 — Two-phase VM thread registration. We MUST know the
    // assigned VM ThreadId BEFORE spawning the OS thread so the
    // spawned closure can capture it as a value (rather than reading
    // from `el.vm_thread_id` later, which is a data race against the
    // setter). Phase 1 here registers without a handle and stamps
    // `el.vm_thread_id`; Phase 2 below attaches the JoinHandle once
    // the OS thread is alive.
    let registered_vm_tid: u64 = if let Some((ref mut ctx, daemon)) = register {
        let vm_tid = ctx.register_native_thread(&name, daemon, 0);
        el.vm_thread_id.store(vm_tid, Ordering::Release);

        // T19_K4 — Allocate a `java.lang.Thread` mirror for this
        // event-loop carrier and link it through the new
        // `set_native_thread_java_obj` API. This makes:
        //
        //   * `Thread.currentThread()` resolve to the right object
        //     when Java-side bytecode runs on the carrier (e.g. a
        //     `Runnable` posted via `NioEventLoop.execute(...)`),
        //   * `ThreadRegistry::find_thread_id_by_thread_obj` find
        //     this carrier when other code holds the mirror,
        //   * the carrier show up in
        //     `ThreadRegistry::alive_thread_objects()` so
        //     `Thread.enumerate()` and JVMTI thread-list APIs see
        //     it.
        //
        // Layout: `crate::alloc_carrier_thread_mirror` picks it. On a real
        // image the mirror is a genuine 19-slot `java.lang.Thread` built by
        // the registered `Thread.<init>(ThreadGroup, Runnable, String)`; on a
        // synthetic image it is the historical 5-slot map
        // `name=0, priority=1, tid=2, target=3, virtualFlag=4`
        // (`classloading::class_manager::synthetic_field_count`).
        //
        // W7-74-short-object-repairs.md. This read
        // `ctx.alloc_object(ClassId::new(0), THREAD_MIRROR_SLOTS)` until
        // 2026-08-12, which on a real image produced a five-slot
        // `cratonvm/synthetic/AnonymousObject$5` — not a `java.lang.Thread` at
        // all, and fourteen fields short of one — and then published it to the
        // registry, so it was what `Thread.currentThread()` handed back on
        // every Vert.x/Netty event-loop carrier. The comment it replaces was
        // not wrong, it was scoped to the synthetic image and stopped being
        // true when the image changed underneath it.
        if vm_tid != 0 {
            // `None` = the class could not be resolved (`--jdk-only` refuses to
            // fabricate one). Leaving the registry entry without a mirror is
            // the correct degradation: `current_thread_object` then builds a
            // full-width real mirror lazily on first use, which is precisely
            // what the short mirror used to pre-empt.
            // Bound to a `let` rather than written inline as the `if let`
            // scrutinee: a scrutinee's temporaries (this reborrow of `ctx`
            // included) live for the whole `if let` under Rust 2021, and the
            // body needs `ctx` again for `set_native_thread_java_obj`.
            let mirror = crate::alloc_carrier_thread_mirror(&mut **ctx, &name, vm_tid, daemon);
            if let Some(mirror) = mirror {
                // Attach to the registry. Failure here is recoverable —
                // the registration entry from above is still valid; the
                // mirror just won't be findable by ObjectRef. We log it
                // via `tracing::warn!` rather than escalating because
                // the only legitimate failure is "thread id missing"
                // which would mean the registry is in an inconsistent
                // state we can't fix from here.
                let attached = ctx.set_native_thread_java_obj(vm_tid, mirror);
                if !attached {
                    tracing::warn!(
                        event_loop = %name,
                        vm_tid = vm_tid,
                        "T19_K4: set_native_thread_java_obj failed; mirror won't be \
                         findable by ObjectRef (registration is otherwise OK)",
                    );
                } else {
                    el.java_thread_mirror_ptr
                        .store(mirror.as_ptr() as usize, Ordering::Release);
                }
            } else {
                tracing::warn!(
                    event_loop = %name,
                    vm_tid = vm_tid,
                    "T19_K4: java/lang/Thread would not resolve; no mirror pre-registered \
                     (Thread.currentThread() will build one lazily)",
                );
            }
        }

        vm_tid
    } else {
        0
    };

    let el_for_thread = Arc::clone(&el);
    let captured_vm_tid = registered_vm_tid;
    let jh = thread::Builder::new()
        .name(name.clone())
        .spawn(move || {
            run_vertx_event_loop(Arc::clone(&el_for_thread));
            // After the loop body exits, push the captured VM
            // ThreadId onto the dead-queue. We use the captured
            // value rather than re-reading `el_for_thread.vm_thread_id`
            // so the read is on a `Copy` local — no atomic read race
            // with the setter (which has by definition already
            // completed before this thread was spawned).
            if captured_vm_tid != 0 {
                push_native_thread_dead(captured_vm_tid);
            }
        })
        .map_err(|e| format!("spawn_vertx_event_loop: {e}"))?;

    if let Some((ctx, _daemon)) = register {
        // Phase 2: hand ownership of the `JoinHandle<()>` to the VM.
        let boxed = Box::new(jh);
        let raw = Box::into_raw(boxed) as usize;
        let attached = ctx.attach_join_handle_to_native_thread(registered_vm_tid, raw);
        if !attached {
            // VM rejected the attach (unknown id). Reclaim the box so
            // the OS thread doesn't leak.
            // SAFETY: VM didn't take ownership; we still own the
            // pointer.
            let reclaimed: Box<std::thread::JoinHandle<()>> =
                unsafe { Box::from_raw(raw as *mut std::thread::JoinHandle<()>) };
            // Drop the box — detaches the JoinHandle so the OS thread
            // runs to completion on its own. Not ideal, but matches
            // HotSpot's "daemon-on-shutdown abandonment" semantics.
            drop(reclaimed);
        }
        vertx_loop_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, Arc::clone(&el));
        // Note: we DON'T also stash the JoinHandle in
        // `vertx_loop_join_handles` — it now lives inside the VM
        // registry. `shutdown_vertx_loop` flips the loop's
        // `shutdown_requested` flag; the VM's
        // `wait_for_non_daemon_threads` then joins the handle.
    } else {
        vertx_loop_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, Arc::clone(&el));
        vertx_loop_join_handles()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, jh);
    }

    Ok(el)
}

/// **T19_K2** — A side-channel queue of VM-registered ThreadIds whose
/// owning OS threads have just finished their event-loop body.
///
/// We can't touch `NativeContext` from inside a native-spawned OS
/// thread (no `&mut Vm` is reachable there), but the next caller of
/// any registered native method *can* drain this queue and tell the
/// VM about the dead threads. The CLI wait-loop also drains it on
/// each iteration via [`drain_native_thread_dead_queue`].
///
/// Bounded: we cap at 4096 pending entries. If more accumulate (a
/// pathological event-loop teardown storm), the queue clears itself
/// and the rest of the entries are silently dropped — they're
/// already-dead OS threads so the registry's `is_alive` will
/// eventually time out anyway.
static NATIVE_THREAD_DEAD_QUEUE: OnceLock<Mutex<Vec<u64>>> = OnceLock::new();

fn native_thread_dead_queue() -> &'static Mutex<Vec<u64>> {
    NATIVE_THREAD_DEAD_QUEUE.get_or_init(|| Mutex::new(Vec::new()))
}

/// Drain the pending-dead queue. Returns the IDs that should be
/// passed to `NativeContext::unregister_native_thread`. Public so
/// the CLI / `native_vertx_close` can flush before checking
/// non-daemon-thread liveness.
pub fn drain_native_thread_dead_queue() -> Vec<u64> {
    let mut q = native_thread_dead_queue()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    std::mem::take(&mut *q)
}

#[allow(non_upper_case_globals)]
const NATIVE_THREAD_DEAD_QUEUE_CAP: usize = 4096;

#[allow(dead_code)]
fn push_native_thread_dead(id: u64) {
    let mut q = native_thread_dead_queue()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if q.len() >= NATIVE_THREAD_DEAD_QUEUE_CAP {
        q.clear();
    }
    q.push(id);
}

/// **T19_K2** — Drain the dead-thread queue and call
/// `unregister_native_thread` on the VM for each id. Called from the
/// Vert.x natives so any loops that exited between native calls get
/// their registry entries flipped to `dead` promptly. Cheap when the
/// queue is empty (the common path).
pub fn flush_native_thread_deaths(ctx: &mut dyn NativeContext) {
    let dead = drain_native_thread_dead_queue();
    for id in dead {
        ctx.unregister_native_thread(id);
    }
}

/// Validate and look up a loop id.
///
/// Rejects 0, negative casts, and ids past the high-water mark (impossible
/// ids that could not have been allocated in this process).
pub fn lookup_vertx_loop(raw_id: i64) -> Option<Arc<VertxEventLoop>> {
    if raw_id <= 0 {
        return None;
    }
    let id = raw_id as u64;
    // Reject ids past the HWM — they were never allocated.
    if id >= LOOP_ID_HWM.load(Ordering::Acquire) {
        return None;
    }
    vertx_loop_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id)
        .cloned()
}

/// Shut down and join a loop, removing it from the registry.
pub fn shutdown_vertx_loop(raw_id: i64) {
    if raw_id <= 0 {
        return;
    }
    let id = raw_id as u64;
    let el_opt = vertx_loop_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&id);
    let jh_opt = vertx_loop_join_handles()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&id);
    if let Some(el) = el_opt {
        el.shutdown();
    }
    if let Some(jh) = jh_opt {
        let _ = jh.join();
    }
}

/// Number of live loops in the registry.
pub fn live_vertx_loop_count() -> usize {
    vertx_loop_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .len()
}

// ---------------------------------------------------------------------------
// Per-VertxImpl group: maps mirror pointer → list of owned loop ids
// so close() can tear them all down.
// ---------------------------------------------------------------------------

static VERTX_GROUPS: OnceLock<Mutex<HashMap<usize, Vec<u64>>>> = OnceLock::new();

fn vertx_groups() -> &'static Mutex<HashMap<usize, Vec<u64>>> {
    VERTX_GROUPS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn store_group(ptr: usize, ids: Vec<u64>) {
    vertx_groups()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(ptr, ids);
}

fn take_group(ptr: usize) -> Option<Vec<u64>> {
    vertx_groups()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&ptr)
}

// ---------------------------------------------------------------------------
// OS thread id (same hash trick as T19.7.c)
// ---------------------------------------------------------------------------

fn os_tid() -> u64 {
    use std::hash::{Hash, Hasher};
    let tid = thread::current().id();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    tid.hash(&mut h);
    h.finish()
}

// ---------------------------------------------------------------------------
// Alloc helper
// ---------------------------------------------------------------------------

fn alloc_future(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    Ok(try_alloc_concurrent_synthetic(ctx, CLS_FUTURE, 1)?)
}

fn alloc_scheduled_future_obj(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    Ok(try_alloc_concurrent_synthetic(
        ctx,
        CLS_SCHEDULED_FUTURE,
        1,
    )?)
}

// ===========================================================================
//  Native implementations
// ===========================================================================

// ---------------------------------------------------------------------------
// VertxImpl
// ---------------------------------------------------------------------------

/// `io.vertx.core.impl.VertxImpl.init(I)V`
fn native_vertx_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let pool_size = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 1,
    };
    if pool_size < 1 || pool_size > MAX_POOL_SIZE {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!(
                "VertxImpl.init: eventLoopPoolSize={pool_size} must be in 1..={MAX_POOL_SIZE}"
            ),
        }
        .into());
    }
    // T19_K2 — drain any pending dead-thread notifications so the VM
    // registry is clean before we report new spawns. Cheap on the
    // common no-op path.
    flush_native_thread_deaths(ctx);

    let mut ids = Vec::with_capacity(pool_size as usize);
    for i in 0..pool_size {
        let name = format!("vert.x-eventloop-{i}");
        // T19_K2 — register the new OS thread with the VM
        // ThreadRegistry as a NON-daemon thread so the CLI's
        // `wait_for_non_daemon_threads()` blocks `main()` exit while
        // the loop is running. This is what keeps Quarkus/Keycloak
        // alive past `main()`.
        let el = spawn_vertx_event_loop_with_ctx(ctx, name, false).map_err(|e| {
            MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                RuntimeError::IllegalStateException {
                    message: format!("VertxImpl.init: spawn failed: {e}"),
                },
            ))
        })?;
        ids.push(el.id);
    }
    let first_id = ids[0] as i64;
    ctx.set_field(this, VERTX_FIELD_EVENT_LOOP_ID, Value::Long(first_id));
    ctx.set_field(this, VERTX_FIELD_POOL_SIZE, Value::Int(pool_size));
    ctx.set_field(this, VERTX_FIELD_STATE, Value::Int(STATE_STARTED));
    store_group(this.as_ptr() as usize, ids);
    Ok(None)
}

/// `io.vertx.core.impl.VertxImpl.getEventLoopId()J`
fn native_vertx_get_event_loop_id(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let id = match ctx.get_field(this, VERTX_FIELD_EVENT_LOOP_ID) {
        Value::Long(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Long(id)))
}

/// `io.vertx.core.impl.VertxImpl.close()V`
fn native_vertx_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    ctx.set_field(this, VERTX_FIELD_STATE, Value::Int(STATE_SHUTTING_DOWN));
    if let Some(ids) = take_group(this.as_ptr() as usize) {
        for raw in ids {
            shutdown_vertx_loop(raw as i64);
        }
    }
    // T19_K2 — drain dead-thread notifications so the VM ThreadRegistry
    // reflects the closed state before close() returns.
    flush_native_thread_deaths(ctx);
    ctx.set_field(this, VERTX_FIELD_STATE, Value::Int(STATE_TERMINATED));
    Ok(None)
}

// ---------------------------------------------------------------------------
// NioEventLoop / DefaultEventLoop / SingleThreadEventExecutor
// ---------------------------------------------------------------------------

/// `*.run()V` — enter the event-loop body.
fn native_nel_run(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    ctx.set_field(this, NEL_FIELD_STATE, Value::Int(STATE_STARTED));
    let raw = match ctx.get_field(this, NEL_FIELD_EVENT_LOOP_ID) {
        Value::Long(v) => v,
        _ => {
            // Auto-allocate an event loop for this thread.
            // T19_K2 — register as NON-daemon so the VM holds the
            // process alive while the listener event loop is running.
            let el = spawn_vertx_event_loop_with_ctx(ctx, "netty-eventloop-auto", false).map_err(
                |e| {
                    MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                        RuntimeError::IllegalStateException {
                            message: format!("NioEventLoop.run: spawn failed: {e}"),
                        },
                    ))
                },
            )?;
            let id = el.id as i64;
            ctx.set_field(this, NEL_FIELD_EVENT_LOOP_ID, Value::Long(id));
            id
        }
    };
    let el = match lookup_vertx_loop(raw) {
        Some(el) => el,
        None => return Ok(None),
    };
    run_vertx_event_loop(el);
    ctx.set_field(this, NEL_FIELD_STATE, Value::Int(STATE_TERMINATED));
    Ok(None)
}

fn method_call_failed_summary(ctx: &mut dyn NativeContext, err: &MethodCallFailed) -> String {
    match err {
        MethodCallFailed::ExceptionThrown(obj) => {
            let class_id = ctx.class_id_of_object(*obj);
            let class_name = ctx
                .class_name_of_id(class_id)
                .unwrap_or_else(|| format!("<class:{}>", class_id.as_u32()));
            format!("{}@{:p}", class_name, obj.as_ptr())
        }
        MethodCallFailed::InternalError(inner) => inner.to_string(),
    }
}

fn invoke_nel_runnable(ctx: &mut dyn NativeContext, runnable: ObjectRef, operation: &'static str) {
    if let Err(e) = ctx.invoke_virtual(runnable, "run", "()V", &[]) {
        let error_summary = method_call_failed_summary(ctx, &e);
        tracing::warn!(
            error = ?e,
            java_error = %error_summary,
            operation = operation,
            "NioEventLoop Runnable.run() threw; swallowing per execute/schedule contract",
        );
    }
}

fn run_or_enqueue_nel_runnable(
    ctx: &mut dyn NativeContext,
    runnable: ObjectRef,
    operation: &'static str,
) {
    let nested = NEL_EXEC_DEPTH.with(|depth| {
        if depth.get() > 0 {
            NEL_EXEC_QUEUE.with(|queue| queue.borrow_mut().push_back(runnable));
            true
        } else {
            depth.set(1);
            false
        }
    });
    if nested {
        return;
    }

    invoke_nel_runnable(ctx, runnable, operation);

    let mut drained = 0usize;
    loop {
        let next = NEL_EXEC_QUEUE.with(|queue| queue.borrow_mut().pop_front());
        let Some(next) = next else { break };
        drained += 1;
        if drained > NEL_EXEC_DRAIN_LIMIT {
            tracing::warn!(
                limit = NEL_EXEC_DRAIN_LIMIT,
                "NioEventLoop execute trampoline reached drain limit; leaving remaining tasks queued",
            );
            break;
        }
        invoke_nel_runnable(ctx, next, operation);
    }

    NEL_EXEC_DEPTH.with(|depth| depth.set(0));
}

/// `*.execute(Ljava/lang/Runnable;)V`
///
/// B1 FIX: previously this enqueued a Rust no-op stub onto the loop's
/// `task_queue` and dropped the real `Runnable` on the floor, so every task
/// submitted to a Netty/Vert.x event loop silently never ran. The loop's
/// `task_queue` carries `Box<dyn FnOnce() + Send>` Rust closures that fire on
/// the pinned OS thread *without* a `NativeContext`, so it cannot drive a Java
/// `run()`; deferring there is structurally impossible.
///
/// The correct fix is to run the `Runnable` here, where we *do* hold the
/// interpreter `ctx`. We invoke `Runnable.run()` synchronously via
/// `ctx.invoke_virtual` (the same bridge MSC uses to drive `Service.start()` —
/// see `jboss_msc.rs::drive_starts`). This guarantees the task actually
/// executes. Per the Netty `execute()` contract the call returns `void` and
/// the task runs "on the event loop"; an exception thrown by the task is
/// reported to the loop (logged) rather than propagated to the submitter, so
/// we catch and log it instead of failing this native.
fn native_nel_execute(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // Validate the Runnable is non-null.
    let runnable = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("NioEventLoop.execute: Runnable is null".into()),
            }
            .into())
        }
    };
    // Refresh the loop's wakeup stats so callers observing them still see the
    // submission, then actually run the task. `invoke_virtual` prepends the
    // receiver; the `run()V` signature takes no further parameters.
    if let Value::Long(raw) = ctx.get_field(this, NEL_FIELD_EVENT_LOOP_ID) {
        if let Some(el) = lookup_vertx_loop(raw) {
            el.stats.tasks_run.fetch_add(1, Ordering::Relaxed);
        }
    }
    // Nested Netty tasks often call execute() again while the current task is
    // still running. Running those recursively on the Java stack can overflow;
    // trampoline nested submissions through a per-thread FIFO and drain them
    // iteratively with the current NativeContext.
    run_or_enqueue_nel_runnable(ctx, runnable, "execute");
    Ok(None)
}

/// Best-effort `Throwable.toString()` for a swallowed `MethodCallFailed`, used
/// only for diagnostic logging (never propagated). Falls back to the plain
/// `Display` impl (which just prints the raw pointer) if the Throwable's own
/// `toString()` can't be resolved.
fn describe_thrown(
    ctx: &mut dyn NativeContext,
    e: &cratonvm_types::error::MethodCallFailed,
) -> String {
    if let cratonvm_types::error::MethodCallFailed::ExceptionThrown(obj) = e {
        if let Ok(Some(Value::Object(Some(s)))) =
            ctx.invoke_virtual(*obj, "toString", "()Ljava/lang/String;", &[])
        {
            if let Some(text) = ctx.read_string(s) {
                return text;
            }
        }
    }
    format!("{e}")
}

/// `*.schedule(Ljava/lang/Runnable;JLjava/util/concurrent/TimeUnit;)Ljava/util/concurrent/ScheduledFuture;`
fn native_nel_schedule(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // Validate Runnable.
    match args.get(1) {
        Some(Value::Object(Some(_))) => {}
        _ => {
            let sf = alloc_scheduled_future_obj(ctx);
            return Ok(Some(Value::Object(Some(sf?))));
        }
    }
    let delay_raw = match args.get(2) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let delay_ms = delay_raw.max(0) as u64;
    // SAFETY: validated non-null above.
    let runnable = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let sf = alloc_scheduled_future_obj(ctx);
            return Ok(Some(Value::Object(Some(sf?))));
        }
    };
    let raw = match ctx.get_field(this, NEL_FIELD_EVENT_LOOP_ID) {
        Value::Long(v) => v,
        _ => {
            let sf = alloc_scheduled_future_obj(ctx);
            return Ok(Some(Value::Object(Some(sf?))));
        }
    };
    let sf = alloc_scheduled_future_obj(ctx);
    // B1 FIX: the previous `schedule_timer(Box::new(|| {}))` dropped the real
    // Runnable — it only ever fired a no-op on the loop's timer heap. Like
    // `execute()`, the loop body runs on a pinned OS thread with no
    // `NativeContext`, so a deferred Rust closure cannot drive a Java `run()`.
    //
    // A zero/negative-delay schedule is the common "run ASAP on the loop"
    // case (e.g. `eventLoop.schedule(r, 0, MILLISECONDS)`); for that we invoke
    // `run()` now via the interpreter so the work actually happens. For a real
    // future delay we keep the timer registration (returns a working
    // ScheduledFuture/Key) rather than running synchronously now, which would
    // violate the delay contract — invoking deferred Java tasks on the carrier
    // is the broader gap tracked separately.
    if delay_ms == 0 {
        run_or_enqueue_nel_runnable(ctx, runnable, "schedule(delay=0)");
    } else if let Some(el) = lookup_vertx_loop(raw) {
        // Best-effort: register the timer so loop bookkeeping (deadline,
        // wakeups) stays consistent. The fired closure cannot itself invoke
        // Java; the deferred-dispatch wiring is a separate follow-up.
        let _ = el.schedule_timer(
            Instant::now() + Duration::from_millis(delay_ms),
            Box::new(|| {}),
        );
    }
    Ok(Some(Value::Object(Some(sf?))))
}

/// `*.inEventLoop()Z`
fn native_nel_in_event_loop(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if NEL_EXEC_DEPTH.with(|depth| depth.get() > 0) {
        return Ok(Some(Value::Int(1)));
    }
    let this = obj_arg(args, 0)?;
    let raw = match ctx.get_field(this, NEL_FIELD_EVENT_LOOP_ID) {
        Value::Long(v) => v,
        _ => return Ok(Some(Value::Int(0))),
    };
    let on_loop = current_vertx_loop()
        .map(|el| el.id as i64 == raw)
        .unwrap_or(false);
    Ok(Some(Value::Int(if on_loop { 1 } else { 0 })))
}

/// `*.isShuttingDown()Z`
fn native_nel_is_shutting_down(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let state = match ctx.get_field(this, NEL_FIELD_STATE) {
        Value::Int(v) => v,
        _ => STATE_NOT_STARTED,
    };
    let shutting = state == STATE_SHUTTING_DOWN || state == STATE_TERMINATED;
    Ok(Some(Value::Int(if shutting { 1 } else { 0 })))
}

/// `*.shutdownGracefully(JJLjava/util/concurrent/TimeUnit;)Lio/netty/util/concurrent/Future;`
fn native_nel_shutdown_gracefully(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    ctx.set_field(this, NEL_FIELD_STATE, Value::Int(STATE_SHUTTING_DOWN));
    let raw = match ctx.get_field(this, NEL_FIELD_EVENT_LOOP_ID) {
        Value::Long(v) => v,
        _ => {
            let f = alloc_future(ctx);
            return Ok(Some(Value::Object(Some(f?))));
        }
    };
    if let Some(el) = lookup_vertx_loop(raw) {
        el.shutdown();
    }
    let f = alloc_future(ctx);
    Ok(Some(Value::Object(Some(f?))))
}

/// `*.awaitTermination(JLjava/util/concurrent/TimeUnit;)Z`
fn native_nel_await_termination(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let state = match ctx.get_field(this, NEL_FIELD_STATE) {
        Value::Int(v) => v,
        _ => STATE_NOT_STARTED,
    };
    Ok(Some(Value::Int(if state == STATE_TERMINATED {
        1
    } else {
        0
    })))
}

/// `*.submit(Ljava/lang/Runnable;)Ljava/util/concurrent/Future;`
fn native_nel_submit(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_nel_execute(ctx, args)?;
    let f = alloc_future(ctx);
    Ok(Some(Value::Object(Some(f?))))
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register all T19.6 Vert.x / Netty natives.
pub fn register_vertx_eventloop_natives(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    // VertxImpl
    registry.register(CLS_VERTX_IMPL, "init", "(I)V", native_vertx_init);
    registry.register(
        CLS_VERTX_IMPL,
        "getEventLoopId",
        "()J",
        native_vertx_get_event_loop_id,
    );
    registry.register(CLS_VERTX_IMPL, "close", "()V", native_vertx_close);

    // A real Netty DefaultEventLoop owns its queue, worker lifecycle and
    // shutdown state. Its former CratonVM override ran tasks synchronously on
    // the submitting thread, which can deadlock asynchronous clients such as
    // the Cassandra driver. Keep these helpers for direct synthetic tests, but
    // do not register them: production Netty must always execute its bytecode.
    registry.set_category(__prev_cat);
}

// ===========================================================================
//                              UNIT TESTS
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::mock_ctx;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
    use std::sync::atomic::AtomicI32;
    use std::sync::Barrier;
    use std::sync::{Mutex as StdMutex, MutexGuard};

    // FIX(test-isolation): The exit trampoline of every loop spawned via
    // `spawn_vertx_event_loop_with_ctx` pushes its VM ThreadId onto the
    // PROCESS-GLOBAL `NATIVE_THREAD_DEAD_QUEUE` (see `push_native_thread_dead`).
    // Each per-test `mock_ctx()` hands out VM ThreadIds starting at 1, so
    // concurrently-running sibling tests produce *colliding* numeric ids on
    // that one shared queue. When this test's `flush_native_thread_deaths`
    // loop drains the queue, it sees ids pushed by OTHER tests (and other
    // tests drain ids pushed by THIS test), so its `alive_after == 0`
    // expectation never settles under `cargo test`'s default parallelism —
    // even though it is correct when run alone (`--test-threads=1`).
    //
    // Fix WITHOUT weakening any assertion: serialize EVERY test in this module
    // that touches ANY shared process-global (the dead-queue, the loop
    // registry, the join-handle map, the id counters, or the per-mirror group
    // map) on the SINGLE module-wide `TEST_LOCK`, and reset the globals to a
    // clean baseline at the start of each such test while holding the lock so a
    // sibling that finished just before us can't leave stragglers behind. The
    // unified `isolated_vertx_test()` helper (below) returns the held lock and
    // performs the reset; callers bind it to a `_guard` local that lives to end
    // of scope. `dead_queue_guard()` is a back-compat alias for the same thing.
    // Poisoning is recovered (a panicking sibling must not wedge the suite).
    static TEST_LOCK: StdMutex<()> = StdMutex::new(());

    // FIX(test-isolation): Inventory of every PROCESS-GLOBAL mutable static in
    // this module that a test in here reads, asserts on, or mutates — and the
    // exact reset each one needs for a clean, exclusive baseline. The unified
    // `isolated_vertx_test()` guard below holds `TEST_LOCK` (so no two guarded
    // tests run concurrently) and performs every reset listed here:
    //
    //   * `NATIVE_THREAD_DEAD_QUEUE` (Mutex<Vec<u64>>): drained to empty so a
    //     sibling's leftover dead-ids cannot be drained/flushed by this test.
    //   * `VERTX_LOOP_REGISTRY` (Mutex<HashMap<id,loop>>): not force-cleared
    //     (joining a stranger's loop is unsafe); instead every guarded test
    //     owns the lock for its whole body, so the only entries it ever sees
    //     are the ones it created. Tests assert on *deltas* / their own ids,
    //     never on an absolute registry size, so leftover entries from an
    //     UN-guarded local test are irrelevant.
    //   * `VERTX_LOOP_JOIN_HANDLES` (Mutex<HashMap<id,join>>): same as above —
    //     mutated only via spawn/shutdown which are now serialized.
    //   * `NEXT_LOOP_ID` / `LOOP_ID_HWM` (AtomicU64): monotonic counters. Tests
    //     never assert an exact id value, only "> 0" / "rejected past HWM", so
    //     no reset is required — serialization alone removes the interleaving
    //     hazard (a sibling bumping the HWM mid-`lookup_rejects_invalid_ids`).
    //   * `VERTX_GROUPS` (Mutex<HashMap<mirror_ptr,ids>>): keyed by the unique
    //     per-test mirror pointer, so entries never collide across tests; the
    //     init/close pair each test runs adds and removes its own key. Held
    //     under the lock for the whole body, so no concurrent writer exists.
    //
    // NOTE on per-loop DispatchStats: `VertxEventLoop.stats` is a PER-LOOP
    // field, NOT a global — `dispatch_stats_increment` reads `el.stats` of the
    // loop IT spawned. Its historical flakiness came from sharing the global
    // registry/join-handle maps and `NEXT_LOOP_ID` with concurrent siblings
    // (spawn/shutdown contention), not from a shared counter. Serializing it
    // under `isolated_vertx_test()` removes that contention; there is no global
    // dispatch-stats counter to zero.

    /// FIX(test-isolation): Acquire the module-wide serialization lock and
    /// reset every shared process-global this module's tests touch to a clean
    /// baseline (see the inventory comment above). Returns the held guard;
    /// bind it to a `_guard` local that lives to the end of the test so the
    /// lock is held for the whole body. Poisoning is recovered so a panicking
    /// sibling cannot wedge the rest of the suite.
    #[must_use]
    fn isolated_vertx_test() -> MutexGuard<'static, ()> {
        let guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_shared_globals();
        guard
    }

    /// FIX(test-isolation): Reset all test-observable shared globals while the
    /// caller holds `TEST_LOCK`. Drains the dead-queue (the only global that
    /// carries cross-test-colliding values); the registry/group maps and id
    /// counters need no value reset because tests assert on deltas/their own
    /// ids and the lock makes their view exclusive (see inventory above).
    fn reset_shared_globals() {
        // Drop any dead-ids a previous (now-finished) test left behind.
        let _ = drain_native_thread_dead_queue();
    }

    #[test]
    fn default_event_loop_has_no_registered_override() {
        let mut registry = NativeMethodRegistry::new();
        register_vertx_eventloop_natives(&mut registry);
        assert_eq!(
            registry.kind_of(CLS_DEFAULT_EVENT_LOOP, "execute", "(Ljava/lang/Runnable;)V"),
            None,
        );
    }

    /// FIX(test-isolation): Back-compat alias. The dead-queue tests historically
    /// called `dead_queue_guard()`; it now delegates to the single unified
    /// `isolated_vertx_test()` so there is exactly one `TEST_LOCK` and one reset
    /// path. Kept so the call sites read intent-fully where dead-queue is the
    /// focus, but it is the SAME lock + SAME reset.
    #[must_use]
    fn dead_queue_guard() -> MutexGuard<'static, ()> {
        isolated_vertx_test()
    }

    // FIX(test-isolation): Reset the shared dead-queue at a point INSIDE the
    // test where we are about to call a native (`native_vertx_init`) whose
    // body internally calls `flush_native_thread_deaths(ctx)`. The guard drains
    // the queue at acquire, but a *prior* (already-finished, lock-released)
    // sibling's event-loop carrier OS thread is DETACHED in the mock
    // (`attach_join_handle_to_native_thread` drops the `JoinHandle`), so it can
    // exit and `push_native_thread_dead(colliding_id)` AFTER our acquire-time
    // drain. Because every `mock_ctx()` hands out ThreadIds starting at 1, that
    // straggler id collides with an id THIS ctx just registered; init's internal
    // flush would then `unregister_native_thread(colliding_id)` and flip our own
    // freshly-registered entry's `alive` flag to false mid-test.
    //
    // We hold `TEST_LOCK` for the whole test, so no *guarded* sibling runs
    // concurrently — the only writer to the queue is such a detached straggler.
    // Spin-drain until the queue is observed empty immediately before each
    // queue-flushing native call: every straggler has a single bounded push at
    // carrier-exit, so once we observe an empty queue the prior carriers have
    // drained and no new guarded pusher exists. This makes init's internal
    // flush a no-op w.r.t. foreign ids, so it cannot touch this ctx's entries.
    fn reset_dead_queue_before_flush_native() {
        // Bounded spin: drain repeatedly until we observe an empty queue twice
        // in a row, so any in-flight straggler push has been absorbed before we
        // hand control to a native that flushes the queue into our ctx.
        for _ in 0..256 {
            let first = drain_native_thread_dead_queue();
            let second = drain_native_thread_dead_queue();
            if first.is_empty() && second.is_empty() {
                return;
            }
            thread::sleep(Duration::from_millis(1));
        }
        // Final unconditional drain so we never leave entries for the flush.
        let _ = drain_native_thread_dead_queue();
    }

    // -----------------------------------------------------------------------
    // T19.6-v1: vertx_init allocates loops and sets fields
    // -----------------------------------------------------------------------
    #[test]
    fn vertx_init_allocates_loops_and_sets_fields() {
        // FIX(test-isolation): init/close spawn _with_ctx loops + close()
        // flushes the shared dead-queue; serialize so this can't pollute (or
        // be polluted by) the K2/K4 dead-queue tests under `cargo test`.
        let _guard = dead_queue_guard();
        let mut ctx = mock_ctx();
        let mirror =
            try_alloc_concurrent_synthetic(&mut ctx, CLS_VERTX_IMPL, VERTX_NUM_SLOTS).unwrap();
        let result = native_vertx_init(&mut ctx, &[Value::Object(Some(mirror)), Value::Int(2)]);
        assert!(result.is_ok(), "init must succeed: {:?}", result.err());
        let eid = match ctx.get_field(mirror, VERTX_FIELD_EVENT_LOOP_ID) {
            Value::Long(v) => v,
            other => panic!("expected Long, got {other:?}"),
        };
        assert!(eid > 0, "eventLoopId must be positive, got {eid}");
        let pool = match ctx.get_field(mirror, VERTX_FIELD_POOL_SIZE) {
            Value::Int(v) => v,
            other => panic!("expected Int, got {other:?}"),
        };
        assert_eq!(pool, 2, "pool size must be stored");
        // Clean up.
        native_vertx_close(&mut ctx, &[Value::Object(Some(mirror))]).ok();
    }

    // -----------------------------------------------------------------------
    // T19.6-v2: init rejects invalid pool sizes
    // -----------------------------------------------------------------------
    #[test]
    fn vertx_init_rejects_invalid_pool_size() {
        // FIX(test-isolation): init touches the shared loop registry / id
        // counters on its way to validating the pool size; serialize so a
        // sibling spawn/shutdown can't race the maps mid-call.
        let _guard = isolated_vertx_test();
        let mut ctx = mock_ctx();
        let mirror =
            try_alloc_concurrent_synthetic(&mut ctx, CLS_VERTX_IMPL, VERTX_NUM_SLOTS).unwrap();
        assert!(
            native_vertx_init(&mut ctx, &[Value::Object(Some(mirror)), Value::Int(0)]).is_err(),
            "pool size 0 must be rejected"
        );
        assert!(
            native_vertx_init(&mut ctx, &[Value::Object(Some(mirror)), Value::Int(-5)]).is_err(),
            "negative pool size must be rejected"
        );
        assert!(
            native_vertx_init(
                &mut ctx,
                &[Value::Object(Some(mirror)), Value::Int(MAX_POOL_SIZE + 1)]
            )
            .is_err(),
            "over-cap pool size must be rejected"
        );
    }

    // -----------------------------------------------------------------------
    // T19.6-v3: getEventLoopId returns stored value
    // -----------------------------------------------------------------------
    #[test]
    fn vertx_get_event_loop_id_returns_stored() {
        let mut ctx = mock_ctx();
        let mirror =
            try_alloc_concurrent_synthetic(&mut ctx, CLS_VERTX_IMPL, VERTX_NUM_SLOTS).unwrap();
        ctx.set_field(mirror, VERTX_FIELD_EVENT_LOOP_ID, Value::Long(42));
        let res = native_vertx_get_event_loop_id(&mut ctx, &[Value::Object(Some(mirror))]);
        assert_eq!(res.unwrap(), Some(Value::Long(42)));
    }

    // -----------------------------------------------------------------------
    // T19.6-v4: close sets state to TERMINATED
    // -----------------------------------------------------------------------
    #[test]
    fn vertx_close_sets_terminated_state() {
        // FIX(test-isolation): init + close() flush the shared dead-queue.
        let _guard = dead_queue_guard();
        let mut ctx = mock_ctx();
        let mirror =
            try_alloc_concurrent_synthetic(&mut ctx, CLS_VERTX_IMPL, VERTX_NUM_SLOTS).unwrap();
        native_vertx_init(&mut ctx, &[Value::Object(Some(mirror)), Value::Int(1)]).expect("init");
        native_vertx_close(&mut ctx, &[Value::Object(Some(mirror))]).expect("close");
        let state = match ctx.get_field(mirror, VERTX_FIELD_STATE) {
            Value::Int(v) => v,
            other => panic!("expected Int, got {other:?}"),
        };
        assert_eq!(state, STATE_TERMINATED);
    }

    // -----------------------------------------------------------------------
    // T19.6-v5: NioEventLoop.execute enqueues on the loop (no error)
    // -----------------------------------------------------------------------
    #[test]
    fn nel_execute_enqueues_without_error() {
        // FIX(test-isolation): init + close() flush the shared dead-queue.
        let _guard = dead_queue_guard();
        let mut ctx = mock_ctx();
        // Allocate a VertxImpl (starts a loop).
        let vm = try_alloc_concurrent_synthetic(&mut ctx, CLS_VERTX_IMPL, VERTX_NUM_SLOTS).unwrap();
        native_vertx_init(&mut ctx, &[Value::Object(Some(vm)), Value::Int(1)]).expect("init");
        let eid = match ctx.get_field(vm, VERTX_FIELD_EVENT_LOOP_ID) {
            Value::Long(v) => v,
            other => panic!("expected Long, got {other:?}"),
        };
        // Wire a NioEventLoop mirror to that loop.
        let nel =
            try_alloc_concurrent_synthetic(&mut ctx, CLS_NIO_EVENT_LOOP, NEL_NUM_SLOTS).unwrap();
        ctx.set_field(nel, NEL_FIELD_EVENT_LOOP_ID, Value::Long(eid));
        let runnable = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Runnable", 0).unwrap();
        let res = native_nel_execute(
            &mut ctx,
            &[Value::Object(Some(nel)), Value::Object(Some(runnable))],
        );
        assert!(res.is_ok(), "execute must succeed: {:?}", res.err());
        native_vertx_close(&mut ctx, &[Value::Object(Some(vm))]).ok();
    }

    // -----------------------------------------------------------------------
    // B1 regression: execute() actually drives Runnable.run() (not a no-op).
    //
    // We can't observe the synthetic-loop side-effect directly in the mock,
    // but we CAN prove `native_nel_execute` reached `ctx.invoke_virtual`: if
    // it did, the pre-armed one-shot result is consumed (so a *second* read
    // sees `None`). If `execute()` still dropped the Runnable, the armed
    // result would remain. We arm an Err to additionally assert the
    // fire-and-forget contract: a throwing task must NOT fail `execute()`.
    // -----------------------------------------------------------------------
    #[test]
    fn nel_execute_invokes_runnable_and_swallows_task_error() {
        let mut ctx = mock_ctx();
        let nel =
            try_alloc_concurrent_synthetic(&mut ctx, CLS_NIO_EVENT_LOOP, NEL_NUM_SLOTS).unwrap();
        ctx.set_field(nel, NEL_FIELD_EVENT_LOOP_ID, Value::Long(0));
        let runnable = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Runnable", 0).unwrap();
        // Arm the next invoke_virtual (the Runnable.run() call) to throw.
        unsafe {
            *ctx.invoke_virtual_result.get() = Some(Err(MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Runtime(RuntimeError::IllegalStateException {
                    message: "task boom".to_string(),
                }),
            )));
        }
        let res = native_nel_execute(
            &mut ctx,
            &[Value::Object(Some(nel)), Value::Object(Some(runnable))],
        );
        // Fire-and-forget: a throwing task must not surface to the submitter.
        assert!(
            res.is_ok(),
            "execute() must swallow task error, got {:?}",
            res.err()
        );
        // Proof the Runnable was actually invoked: the one-shot armed result
        // was consumed by `invoke_virtual`, so it is now None.
        let consumed = unsafe { (*ctx.invoke_virtual_result.get()).is_none() };
        assert!(
            consumed,
            "execute() must invoke Runnable.run() (armed invoke_virtual result \
             should have been consumed)"
        );
    }

    // -----------------------------------------------------------------------
    // B1 regression: submit() also drives the Runnable and still returns a
    // non-null Future even when the task throws.
    // -----------------------------------------------------------------------
    #[test]
    fn nel_submit_invokes_runnable_and_returns_future_on_task_error() {
        let mut ctx = mock_ctx();
        let nel =
            try_alloc_concurrent_synthetic(&mut ctx, CLS_NIO_EVENT_LOOP, NEL_NUM_SLOTS).unwrap();
        ctx.set_field(nel, NEL_FIELD_EVENT_LOOP_ID, Value::Long(0));
        let runnable = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Runnable", 0).unwrap();
        unsafe {
            *ctx.invoke_virtual_result.get() = Some(Err(MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Runtime(RuntimeError::IllegalStateException {
                    message: "task boom".to_string(),
                }),
            )));
        }
        let res = native_nel_submit(
            &mut ctx,
            &[Value::Object(Some(nel)), Value::Object(Some(runnable))],
        );
        assert!(res.is_ok());
        assert!(matches!(res.unwrap(), Some(Value::Object(Some(_)))));
        let consumed = unsafe { (*ctx.invoke_virtual_result.get()).is_none() };
        assert!(consumed, "submit() must invoke Runnable.run()");
    }

    // -----------------------------------------------------------------------
    // T19.6-v6: inEventLoop returns 0 from a non-loop thread
    // -----------------------------------------------------------------------
    #[test]
    fn nel_in_event_loop_false_outside_loop() {
        let mut ctx = mock_ctx();
        let nel =
            try_alloc_concurrent_synthetic(&mut ctx, CLS_NIO_EVENT_LOOP, NEL_NUM_SLOTS).unwrap();
        ctx.set_field(nel, NEL_FIELD_EVENT_LOOP_ID, Value::Long(0));
        let res = native_nel_in_event_loop(&mut ctx, &[Value::Object(Some(nel))]);
        assert_eq!(res.unwrap(), Some(Value::Int(0)));
    }

    // -----------------------------------------------------------------------
    // T19.6-v7: isShuttingDown reflects state field
    // -----------------------------------------------------------------------
    #[test]
    fn nel_is_shutting_down_reflects_state() {
        let mut ctx = mock_ctx();
        let nel =
            try_alloc_concurrent_synthetic(&mut ctx, CLS_NIO_EVENT_LOOP, NEL_NUM_SLOTS).unwrap();
        for (state, expected) in [
            (STATE_NOT_STARTED, 0),
            (STATE_STARTED, 0),
            (STATE_SHUTTING_DOWN, 1),
            (STATE_TERMINATED, 1),
        ] {
            ctx.set_field(nel, NEL_FIELD_STATE, Value::Int(state));
            let r = native_nel_is_shutting_down(&mut ctx, &[Value::Object(Some(nel))]).unwrap();
            assert_eq!(r, Some(Value::Int(expected)), "state={state}");
        }
    }

    // -----------------------------------------------------------------------
    // T19.6-v8: shutdownGracefully returns non-null Future
    // -----------------------------------------------------------------------
    #[test]
    fn nel_shutdown_gracefully_returns_future() {
        let mut ctx = mock_ctx();
        let nel =
            try_alloc_concurrent_synthetic(&mut ctx, CLS_NIO_EVENT_LOOP, NEL_NUM_SLOTS).unwrap();
        ctx.set_field(nel, NEL_FIELD_EVENT_LOOP_ID, Value::Long(0));
        let res = native_nel_shutdown_gracefully(
            &mut ctx,
            &[
                Value::Object(Some(nel)),
                Value::Long(0),
                Value::Long(100),
                Value::Object(None),
            ],
        );
        assert!(res.is_ok());
        assert!(matches!(res.unwrap(), Some(Value::Object(Some(_)))));
        assert_eq!(
            ctx.get_field(nel, NEL_FIELD_STATE),
            Value::Int(STATE_SHUTTING_DOWN)
        );
    }

    // -----------------------------------------------------------------------
    // T19.6-v9: awaitTermination returns 1 only when TERMINATED
    // -----------------------------------------------------------------------
    #[test]
    fn nel_await_termination_reflects_state() {
        let mut ctx = mock_ctx();
        let nel =
            try_alloc_concurrent_synthetic(&mut ctx, CLS_NIO_EVENT_LOOP, NEL_NUM_SLOTS).unwrap();
        ctx.set_field(nel, NEL_FIELD_STATE, Value::Int(STATE_STARTED));
        assert_eq!(
            native_nel_await_termination(
                &mut ctx,
                &[
                    Value::Object(Some(nel)),
                    Value::Long(100),
                    Value::Object(None)
                ]
            )
            .unwrap(),
            Some(Value::Int(0))
        );
        ctx.set_field(nel, NEL_FIELD_STATE, Value::Int(STATE_TERMINATED));
        assert_eq!(
            native_nel_await_termination(
                &mut ctx,
                &[
                    Value::Object(Some(nel)),
                    Value::Long(100),
                    Value::Object(None)
                ]
            )
            .unwrap(),
            Some(Value::Int(1))
        );
    }

    // -----------------------------------------------------------------------
    // T19.6-v10: schedule returns non-null ScheduledFuture
    // -----------------------------------------------------------------------
    #[test]
    fn nel_schedule_returns_scheduled_future() {
        let mut ctx = mock_ctx();
        let nel =
            try_alloc_concurrent_synthetic(&mut ctx, CLS_NIO_EVENT_LOOP, NEL_NUM_SLOTS).unwrap();
        ctx.set_field(nel, NEL_FIELD_EVENT_LOOP_ID, Value::Long(0));
        let runnable = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Runnable", 0).unwrap();
        let res = native_nel_schedule(
            &mut ctx,
            &[
                Value::Object(Some(nel)),
                Value::Object(Some(runnable)),
                Value::Long(50),
                Value::Object(None),
            ],
        );
        assert!(res.is_ok());
        assert!(matches!(res.unwrap(), Some(Value::Object(Some(_)))));
    }

    // -----------------------------------------------------------------------
    // T19.6-v11: submit returns non-null Future
    // -----------------------------------------------------------------------
    #[test]
    fn nel_submit_returns_future() {
        let mut ctx = mock_ctx();
        let nel =
            try_alloc_concurrent_synthetic(&mut ctx, CLS_NIO_EVENT_LOOP, NEL_NUM_SLOTS).unwrap();
        ctx.set_field(nel, NEL_FIELD_EVENT_LOOP_ID, Value::Long(0));
        let runnable = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Runnable", 0).unwrap();
        let res = native_nel_submit(
            &mut ctx,
            &[Value::Object(Some(nel)), Value::Object(Some(runnable))],
        );
        assert!(res.is_ok());
        assert!(matches!(res.unwrap(), Some(Value::Object(Some(_)))));
    }

    // -----------------------------------------------------------------------
    // T19.6-v12: execute with null Runnable returns NullPointerException
    // -----------------------------------------------------------------------
    #[test]
    fn nel_execute_null_runnable_errors() {
        let mut ctx = mock_ctx();
        let nel =
            try_alloc_concurrent_synthetic(&mut ctx, CLS_NIO_EVENT_LOOP, NEL_NUM_SLOTS).unwrap();
        let res = native_nel_execute(&mut ctx, &[Value::Object(Some(nel)), Value::Object(None)]);
        assert!(res.is_err(), "null Runnable must error");
    }

    // -----------------------------------------------------------------------
    // T19.6-v13: event loop runs scheduled task
    // -----------------------------------------------------------------------
    #[test]
    fn vertx_loop_runs_scheduled_task() {
        // FIX(test-isolation): spawn/shutdown mutate the shared loop registry,
        // join-handle map and `NEXT_LOOP_ID`; serialize against siblings.
        let _guard = isolated_vertx_test();
        let el = spawn_vertx_event_loop("test-runs-task").expect("spawn");
        let raw_id = el.id as i64;

        // Wait for the loop thread to record its id.
        for _ in 0..200 {
            if el.loop_thread_id.load(Ordering::Acquire) != 0 {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }

        let ran = Arc::new(AtomicBool::new(false));
        let r2 = Arc::clone(&ran);
        let barrier = Arc::new(Barrier::new(2));
        let b2 = Arc::clone(&barrier);
        el.schedule_task(Box::new(move || {
            r2.store(true, Ordering::Release);
            b2.wait();
        }))
        .unwrap_or_else(|_| panic!("schedule failed"));
        barrier.wait();
        assert!(ran.load(Ordering::Acquire), "task must run in loop thread");

        shutdown_vertx_loop(raw_id);
    }

    // -----------------------------------------------------------------------
    // T19.6-v14: lookup_vertx_loop rejects negative / zero / too-large ids
    // -----------------------------------------------------------------------
    #[test]
    fn lookup_rejects_invalid_ids() {
        // FIX(test-isolation): `lookup_vertx_loop` reads the shared
        // `LOOP_ID_HWM`; serialize so a concurrent sibling spawn bumping the
        // high-water mark can't perturb the bounds this test probes.
        let _guard = isolated_vertx_test();
        assert!(lookup_vertx_loop(-1).is_none(), "negative id must be None");
        assert!(lookup_vertx_loop(0).is_none(), "zero id must be None");
        assert!(
            lookup_vertx_loop(i64::MAX).is_none(),
            "huge id must be None"
        );
    }

    // -----------------------------------------------------------------------
    // T19.6-v15: panic in task does not kill the loop
    // -----------------------------------------------------------------------
    #[test]
    fn loop_survives_panicking_task() {
        // FIX(test-isolation): spawn/shutdown mutate the shared loop registry,
        // join-handle map and `NEXT_LOOP_ID`; serialize against siblings.
        let _guard = isolated_vertx_test();
        let el = spawn_vertx_event_loop("test-panic").expect("spawn");
        let raw_id = el.id as i64;
        for _ in 0..200 {
            if el.loop_thread_id.load(Ordering::Acquire) != 0 {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }

        el.schedule_task(Box::new(|| panic!("intentional test panic")))
            .unwrap_or_else(|_| panic!("schedule panic failed"));
        thread::sleep(Duration::from_millis(50));

        // Loop must still be alive.
        let ran = Arc::new(AtomicBool::new(false));
        let r2 = Arc::clone(&ran);
        el.schedule_task(Box::new(move || r2.store(true, Ordering::Release)))
            .unwrap_or_else(|_| panic!("post-panic schedule failed"));

        let deadline = Instant::now() + Duration::from_millis(300);
        while !ran.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(ran.load(Ordering::Acquire), "loop must survive task panic");
        assert!(el.stats.panic_count.load(Ordering::Acquire) >= 1);

        shutdown_vertx_loop(raw_id);
    }

    // -----------------------------------------------------------------------
    // T19.6-v16: DispatchStats counters increment
    // -----------------------------------------------------------------------
    #[test]
    fn dispatch_stats_increment() {
        // FIX(test-isolation): `el.stats` is per-loop, but this test's
        // spawn/shutdown share the global loop registry, join-handle map and
        // `NEXT_LOOP_ID` with every other spawning test. Under default
        // `cargo test` parallelism that contention is what made this test
        // flake. Serialize on the unified lock so spawn/shutdown and the
        // dead-queue are exclusively ours; the per-loop stats it asserts on
        // are then read from a loop no sibling can touch.
        let _guard = isolated_vertx_test();
        let el = spawn_vertx_event_loop("test-stats").expect("spawn");
        let raw_id = el.id as i64;
        for _ in 0..200 {
            if el.loop_thread_id.load(Ordering::Acquire) != 0 {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }

        let barrier = Arc::new(Barrier::new(2));
        let b2 = Arc::clone(&barrier);
        for _ in 0..4 {
            el.schedule_task(Box::new(|| {})).ok();
        }
        el.schedule_task(Box::new(move || {
            b2.wait();
        }))
        .ok();
        barrier.wait();

        // FIX(test-isolation): `barrier.wait()` only proves the 5th task has
        // STARTED (it is parked in `b2.wait()`); the loop bumps `tasks_run`
        // when each task body RETURNS, so the 5th task's count lands slightly
        // after the rendezvous. Under parallel CPU load that lag can leave
        // `tasks_run` momentarily at 4. Poll for the eventual condition with a
        // generous timeout instead of asserting instantly — same assertion
        // intent (>=5 tasks run, >=1 select cycle), just not racing dispatch.
        let deadline = Instant::now() + Duration::from_secs(5);
        let snap = loop {
            let snap = el.stats.snapshot();
            if snap.tasks_run >= 5 && snap.select_calls >= 1 {
                break snap;
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for dispatch stats: tasks_run={}, select_calls={}",
                    snap.tasks_run, snap.select_calls
                );
            }
            thread::sleep(Duration::from_millis(2));
        };
        assert!(snap.tasks_run >= 5, "tasks_run={}", snap.tasks_run);
        assert!(snap.select_calls >= 1, "select_calls={}", snap.select_calls);

        shutdown_vertx_loop(raw_id);
    }

    // -----------------------------------------------------------------------
    // T19.6-v17: 8 threads concurrently schedule without data race
    // -----------------------------------------------------------------------
    #[test]
    fn concurrent_8_thread_schedule_no_race() {
        // FIX(test-isolation): spawn/shutdown mutate the shared loop registry,
        // join-handle map and `NEXT_LOOP_ID`; serialize against siblings.
        let _guard = isolated_vertx_test();
        let el = spawn_vertx_event_loop("test-concurrent").expect("spawn");
        let raw_id = el.id as i64;
        for _ in 0..200 {
            if el.loop_thread_id.load(Ordering::Acquire) != 0 {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }

        let counter = Arc::new(AtomicI32::new(0));
        let barrier = Arc::new(Barrier::new(8));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let el2 = Arc::clone(&el);
            let c2 = Arc::clone(&counter);
            let b2 = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                b2.wait();
                for _ in 0..25 {
                    let c3 = Arc::clone(&c2);
                    let _ = el2.schedule_task(Box::new(move || {
                        c3.fetch_add(1, Ordering::Relaxed);
                    }));
                }
            }));
        }
        for h in handles {
            h.join().expect("join");
        }

        let deadline = Instant::now() + Duration::from_millis(500);
        while counter.load(Ordering::Relaxed) < 200 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            counter.load(Ordering::Relaxed) > 0,
            "no concurrent tasks ran"
        );

        shutdown_vertx_loop(raw_id);
    }

    // =======================================================================
    // T19_K2 — Vert.x event-loop VM ThreadRegistry integration tests.
    // =======================================================================

    /// **T19_K2-1** — `spawn_vertx_event_loop_with_ctx` calls
    /// `register_native_thread` exactly once with the right name and
    /// `daemon == false`, so `wait_for_non_daemon_threads` will wait
    /// for the loop.
    #[test]
    fn t19_k2_spawn_with_ctx_registers_as_non_daemon() {
        // FIX(test-isolation): this test's shutdown pushes onto the shared
        // dead-queue and then drains it; serialize so it can't race siblings.
        let _guard = dead_queue_guard();
        let mut ctx = mock_ctx();
        let before = ctx.registered_native_threads().len();
        let el =
            spawn_vertx_event_loop_with_ctx(&mut ctx, "k2-test-1", false).expect("spawn_with_ctx");
        let after = ctx.registered_native_threads().len();
        assert_eq!(
            after - before,
            1,
            "exactly one register_native_thread call expected"
        );
        let last = ctx.registered_native_threads().last().cloned().unwrap();
        assert_eq!(last.0, "k2-test-1", "registered name");
        assert!(!last.1, "must register as NON-daemon (false)");
        assert!(last.2, "must register as alive");
        // vm_thread_id should be the id assigned by the mock (>= 1).
        assert!(
            el.vm_thread_id.load(Ordering::Acquire) > 0,
            "vm_thread_id should be set"
        );
        // Cleanup.
        shutdown_vertx_loop(el.id as i64);
        // After the loop body exits, our exit trampoline pushes the
        // dead-id onto NATIVE_THREAD_DEAD_QUEUE. The `flush` call
        // below routes it through `unregister_native_thread`.
        // Give the OS thread a moment to actually exit.
        for _ in 0..100 {
            if !drain_native_thread_dead_queue().is_empty() {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    /// **T19_K2-2** — `register_native_thread` returns a non-zero id
    /// in the production path; loop's `vm_thread_id` mirrors it.
    #[test]
    fn t19_k2_vm_thread_id_assigned_on_spawn_with_ctx() {
        // FIX(test-isolation): shutdown pushes onto the shared dead-queue.
        let _guard = dead_queue_guard();
        let mut ctx = mock_ctx();
        let el = spawn_vertx_event_loop_with_ctx(&mut ctx, "k2-test-2", false).expect("spawn");
        let vm_tid = el.vm_thread_id.load(Ordering::Acquire);
        assert_ne!(vm_tid, 0, "vm_thread_id must be assigned");
        shutdown_vertx_loop(el.id as i64);
    }

    /// **T19_K2-3** — exit trampoline pushes the vm_thread_id onto
    /// the dead-queue once the loop terminates.
    #[test]
    fn t19_k2_loop_exit_pushes_vm_thread_id_to_dead_queue() {
        // FIX(test-isolation): this test drains the shared dead-queue looking
        // for ITS id; a concurrent sibling draining first would steal it.
        // Serialize + reset (the guard drains leftover entries for us).
        let _guard = dead_queue_guard();

        let mut ctx = mock_ctx();
        let el = spawn_vertx_event_loop_with_ctx(&mut ctx, "k2-test-3", false).expect("spawn");
        let expected_vm_tid = el.vm_thread_id.load(Ordering::Acquire);
        assert_ne!(expected_vm_tid, 0);

        // Trigger shutdown and wait for the OS thread to exit.
        shutdown_vertx_loop(el.id as i64);

        // Poll the dead-queue for up to 1 second.
        let deadline = Instant::now() + Duration::from_millis(1_000);
        let mut found = false;
        while Instant::now() < deadline {
            let dead = drain_native_thread_dead_queue();
            if dead.contains(&expected_vm_tid) {
                found = true;
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            found,
            "exit trampoline must push vm_thread_id={} onto dead-queue",
            expected_vm_tid,
        );
    }

    /// **T19_K2-4** — `flush_native_thread_deaths` routes drained ids
    /// through `unregister_native_thread` so the mock's `alive` flag
    /// flips to false.
    #[test]
    fn t19_k2_flush_dead_threads_calls_unregister() {
        // FIX(test-isolation): this test repeatedly flushes the shared
        // dead-queue against its own ctx; serialize + reset.
        let _guard = dead_queue_guard();
        let mut ctx = mock_ctx();
        let el = spawn_vertx_event_loop_with_ctx(&mut ctx, "k2-test-4", false).expect("spawn");
        let vm_tid = el.vm_thread_id.load(Ordering::Acquire);
        assert_ne!(vm_tid, 0);

        // Sanity: alive before shutdown.
        let registered = ctx.registered_native_threads();
        let entry = registered
            .iter()
            .find(|e| e.0 == "k2-test-4")
            .expect("registered entry");
        assert!(entry.2, "entry must be alive before shutdown");

        // Shut down and wait for trampoline.
        shutdown_vertx_loop(el.id as i64);
        let deadline = Instant::now() + Duration::from_millis(1_000);
        loop {
            // Re-flush.
            flush_native_thread_deaths(&mut ctx);
            let r = ctx.registered_native_threads();
            let alive = r
                .iter()
                .find(|e| e.0 == "k2-test-4")
                .map(|e| e.2)
                .unwrap_or(true);
            if !alive {
                break;
            }
            if Instant::now() >= deadline {
                panic!("flush_native_thread_deaths did not flip alive->false");
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// **T19_K2-5** — `native_vertx_init` wires through
    /// `spawn_vertx_event_loop_with_ctx` so each pool slot registers
    /// its own VM thread.
    #[test]
    fn t19_k2_vertx_init_registers_one_vm_thread_per_pool_slot() {
        // FIX(test-isolation): close() flushes the shared dead-queue; serialize.
        let _guard = dead_queue_guard();
        let mut ctx = mock_ctx();
        let before = ctx.registered_native_threads().len();
        let mirror =
            try_alloc_concurrent_synthetic(&mut ctx, CLS_VERTX_IMPL, VERTX_NUM_SLOTS).unwrap();
        let pool = 3;
        // FIX(test-isolation): drain prior-test straggler dead-ids before the
        // init whose internal `flush_native_thread_deaths` would otherwise
        // unregister a colliding id and flip a `[before..]` entry's `alive`
        // flag — see `reset_dead_queue_before_flush_native` for the mechanism.
        reset_dead_queue_before_flush_native();
        native_vertx_init(&mut ctx, &[Value::Object(Some(mirror)), Value::Int(pool)])
            .expect("init");
        let after = ctx.registered_native_threads().len();
        assert_eq!(
            after - before,
            pool as usize,
            "init(N) must call register_native_thread N times"
        );
        // Each new entry must be non-daemon.
        let new_entries = ctx.registered_native_threads()[before..].to_vec();
        for (name, daemon, alive) in &new_entries {
            assert!(name.starts_with("vert.x-eventloop-"));
            assert!(!*daemon, "must be non-daemon");
            assert!(*alive);
        }
        // Cleanup.
        native_vertx_close(&mut ctx, &[Value::Object(Some(mirror))]).ok();
    }

    /// **T19_K2-6** — `native_vertx_close` flushes pending dead
    /// notifications via `flush_native_thread_deaths`.
    #[test]
    fn t19_k2_vertx_close_flushes_dead_threads() {
        // FIX(test-isolation): serialize on the shared dead-queue and reset it.
        let _guard = dead_queue_guard();
        let mut ctx = mock_ctx();
        let mirror =
            try_alloc_concurrent_synthetic(&mut ctx, CLS_VERTX_IMPL, VERTX_NUM_SLOTS).unwrap();
        native_vertx_init(&mut ctx, &[Value::Object(Some(mirror)), Value::Int(2)]).expect("init");
        let before_close = ctx
            .registered_native_threads()
            .iter()
            .filter(|e| e.2)
            .count();
        assert_eq!(before_close, 2, "two alive entries before close");
        // close shuts the loops down + flushes the dead queue.
        native_vertx_close(&mut ctx, &[Value::Object(Some(mirror))]).expect("close");
        // Wait for the OS threads to actually exit + close to flush.
        // close() calls flush once at the end; if exit races we might
        // need to flush a second time. Poll for up to 1 s.
        let deadline = Instant::now() + Duration::from_millis(1_000);
        let mut alive_after = 99usize;
        while Instant::now() < deadline {
            // Re-flush to catch late-exiting threads.
            flush_native_thread_deaths(&mut ctx);
            alive_after = ctx
                .registered_native_threads()
                .iter()
                .filter(|e| e.2)
                .count();
            if alive_after == 0 {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            alive_after, 0,
            "all threads must be marked dead after close"
        );
    }

    /// **T19_K2-7** — `spawn_vertx_event_loop` (the legacy variant
    /// with no ctx) does NOT register any VM thread. Important for
    /// keeping the unit tests deterministic.
    #[test]
    fn t19_k2_legacy_spawn_does_not_register_vm_thread() {
        // FIX(test-isolation): even the legacy (no-ctx) spawn mutates the
        // shared loop registry / join-handle map / `NEXT_LOOP_ID`, and this
        // test asserts a registered-thread DELTA of zero — a sibling's
        // `_with_ctx` spawn registering on the same mock would not affect this
        // ctx, but spawn/shutdown map contention is still serialized here.
        let _guard = isolated_vertx_test();
        let mut ctx = mock_ctx();
        let before = ctx.registered_native_threads().len();
        let el = spawn_vertx_event_loop("k2-test-7-legacy").expect("spawn");
        let after = ctx.registered_native_threads().len();
        assert_eq!(
            after, before,
            "legacy spawn must not call register_native_thread"
        );
        assert_eq!(
            el.vm_thread_id.load(Ordering::Acquire),
            0,
            "legacy spawn leaves vm_thread_id at 0"
        );
        shutdown_vertx_loop(el.id as i64);
    }

    /// **T19_K2-8** — daemon flag wired through correctly when the
    /// caller asks for it.
    #[test]
    fn t19_k2_spawn_with_ctx_daemon_true_registered_as_daemon() {
        // FIX(test-isolation): shutdown pushes onto the shared dead-queue.
        let _guard = dead_queue_guard();
        let mut ctx = mock_ctx();
        let el = spawn_vertx_event_loop_with_ctx(&mut ctx, "k2-test-8", true).expect("spawn");
        let last = ctx.registered_native_threads().last().cloned().unwrap();
        assert_eq!(last.0, "k2-test-8");
        assert!(last.1, "daemon flag must be true");
        shutdown_vertx_loop(el.id as i64);
    }

    /// **T19_K2-9** — exit trampoline is robust against a loop body
    /// that exits before `vm_thread_id` is stored. The vm_thread_id
    /// stays at 0; the dead-queue is not poisoned.
    #[test]
    fn t19_k2_legacy_loop_exit_does_not_push_dead_id() {
        // FIX(test-isolation): this test drains the shared dead-queue and
        // inspects every entry; a sibling's pushed ids would pollute it.
        let _guard = dead_queue_guard();
        let el = spawn_vertx_event_loop("k2-test-9-legacy").expect("spawn");
        shutdown_vertx_loop(el.id as i64);
        // Wait briefly for OS thread exit.
        thread::sleep(Duration::from_millis(50));
        let dead = drain_native_thread_dead_queue();
        assert!(
            dead.is_empty() || !dead.iter().any(|&id| id == 0),
            "legacy spawn (no register) must not push 0 onto dead-queue: {:?}",
            dead,
        );
    }

    /// **T19_K2-10** — Multiple concurrent VertxImpl instances each
    /// register their own pool of non-daemon VM threads (no leakage,
    /// no races).
    #[test]
    fn t19_k2_multiple_vertx_instances_register_independently() {
        // FIX(test-isolation): close() flushes the shared dead-queue; serialize.
        let _guard = dead_queue_guard();
        let mut ctx = mock_ctx();
        let before = ctx.registered_native_threads().len();
        let mirror_a =
            try_alloc_concurrent_synthetic(&mut ctx, CLS_VERTX_IMPL, VERTX_NUM_SLOTS).unwrap();
        let mirror_b =
            try_alloc_concurrent_synthetic(&mut ctx, CLS_VERTX_IMPL, VERTX_NUM_SLOTS).unwrap();
        // FIX(test-isolation): `native_vertx_init` internally calls
        // `flush_native_thread_deaths(ctx)`. Drain any prior-test straggler
        // (colliding) dead-id from the shared queue right before each init so
        // that internal flush cannot `unregister_native_thread` one of THIS
        // ctx's freshly-registered (colliding-id) entries and flip its `alive`
        // flag — which would make a `[before..]` entry's `assert!(alive)` fail.
        reset_dead_queue_before_flush_native();
        native_vertx_init(&mut ctx, &[Value::Object(Some(mirror_a)), Value::Int(2)])
            .expect("init A");
        reset_dead_queue_before_flush_native();
        native_vertx_init(&mut ctx, &[Value::Object(Some(mirror_b)), Value::Int(3)])
            .expect("init B");
        let after = ctx.registered_native_threads().len();
        assert_eq!(after - before, 5, "2 + 3 = 5 new VM threads");
        // All 5 must be marked non-daemon + alive.
        for entry in &ctx.registered_native_threads()[before..] {
            assert!(!entry.1, "non-daemon");
            assert!(entry.2, "alive");
        }
        // Cleanup.
        native_vertx_close(&mut ctx, &[Value::Object(Some(mirror_a))]).ok();
        native_vertx_close(&mut ctx, &[Value::Object(Some(mirror_b))]).ok();
    }

    /// **T19_K2-11** — `current_vertx_loop()` resolves the running
    /// loop from inside its body — proxy for "Thread.currentThread"
    /// would resolve correctly since the OS thread is dedicated.
    #[test]
    fn t19_k2_current_vertx_loop_resolves_inside_loop_body() {
        // FIX(test-isolation): shutdown pushes onto the shared dead-queue.
        let _guard = dead_queue_guard();
        let mut ctx = mock_ctx();
        let el = spawn_vertx_event_loop_with_ctx(&mut ctx, "k2-test-11", false).expect("spawn");
        for _ in 0..200 {
            if el.loop_thread_id.load(Ordering::Acquire) != 0 {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        let observed_id = Arc::new(AtomicU64::new(0));
        let oid2 = Arc::clone(&observed_id);
        let barrier = Arc::new(Barrier::new(2));
        let b2 = Arc::clone(&barrier);
        el.schedule_task(Box::new(move || {
            if let Some(curr) = current_vertx_loop() {
                oid2.store(curr.id, Ordering::Release);
            }
            b2.wait();
        }))
        .unwrap_or_else(|_| panic!("schedule_task failed"));
        barrier.wait();
        assert_eq!(
            observed_id.load(Ordering::Acquire),
            el.id,
            "current_vertx_loop in loop body must resolve to the right loop"
        );
        shutdown_vertx_loop(el.id as i64);
    }

    /// **T19_K2-12** — `drain_native_thread_dead_queue` is idempotent
    /// — calling it twice in a row returns the entries once and
    /// then nothing.
    #[test]
    fn t19_k2_drain_dead_queue_is_idempotent() {
        // FIX(test-isolation): this test inspects the shared dead-queue
        // directly and asserts draining empties it — a concurrent sibling
        // pushing/draining would break the idempotence check. Serialize + reset.
        let _guard = dead_queue_guard();
        // Push synthetic ids by spawning + shutting down.
        let mut ctx = mock_ctx();
        let el = spawn_vertx_event_loop_with_ctx(&mut ctx, "k2-test-12", false).expect("spawn");
        shutdown_vertx_loop(el.id as i64);
        // Wait until the trampoline has run.
        let deadline = Instant::now() + Duration::from_millis(1_000);
        loop {
            // Peek by draining and then re-pushing if needed (we want
            // to assert idempotence — so use a separate call ordering).
            thread::sleep(Duration::from_millis(5));
            // Check if there's something there.
            let q = native_thread_dead_queue()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if !q.is_empty() {
                break;
            }
            drop(q);
            if Instant::now() >= deadline {
                panic!("dead queue never populated");
            }
        }
        let first = drain_native_thread_dead_queue();
        let second = drain_native_thread_dead_queue();
        assert!(!first.is_empty(), "first drain returns ids");
        assert!(second.is_empty(), "second drain returns empty");
    }

    // =======================================================================
    // T19_K4 — Java Thread mirror linkage tests.
    //
    // K2 left `java_thread_obj` at None on the registry entry; K4 closes
    // the gap by allocating a synthetic `java.lang.Thread` mirror at
    // spawn time and threading it through `set_native_thread_java_obj`.
    // These tests assert the mirror is allocated, named, linked, and
    // accessible through the new APIs.
    // =======================================================================

    /// **T19_K4-1** — `spawn_vertx_event_loop_with_ctx` calls
    /// `set_native_thread_java_obj` exactly once with a non-null
    /// mirror after `register_native_thread`.
    #[test]
    fn t19_k4_spawn_with_ctx_registers_java_thread_mirror() {
        // FIX(test-isolation): shutdown pushes onto the shared dead-queue.
        let _guard = dead_queue_guard();
        let mut ctx = mock_ctx();
        let el =
            spawn_vertx_event_loop_with_ctx(&mut ctx, "k4-test-1", false).expect("spawn_with_ctx");
        let vm_tid = el.vm_thread_id.load(Ordering::Acquire);
        assert_ne!(vm_tid, 0, "vm_thread_id must be set");
        // The mock should have recorded the mirror pointer.
        let mirror_ptr = ctx.native_thread_java_obj_ptr(vm_tid);
        assert_ne!(
            mirror_ptr, 0,
            "set_native_thread_java_obj must have been called with a non-null mirror"
        );
        // And the loop's own field must mirror the same value.
        assert_eq!(
            mirror_ptr,
            el.java_thread_mirror_ptr.load(Ordering::Acquire),
            "VertxEventLoop.java_thread_mirror_ptr must equal the registered mirror"
        );
        shutdown_vertx_loop(el.id as i64);
    }

    /// **T19_K4-2** — The Thread mirror's `name` field is set to the
    /// loop's name string (round-tripped through ctx.create_string).
    #[test]
    fn t19_k4_thread_mirror_name_field_set_to_loop_name() {
        // FIX(test-isolation): shutdown pushes onto the shared dead-queue.
        let _guard = dead_queue_guard();
        let mut ctx = mock_ctx();
        let el = spawn_vertx_event_loop_with_ctx(&mut ctx, "k4-name-test", false).expect("spawn");
        let vm_tid = el.vm_thread_id.load(Ordering::Acquire);
        let mirror_ptr_usize = ctx.native_thread_java_obj_ptr(vm_tid);
        assert_ne!(mirror_ptr_usize, 0);
        let mirror = unsafe { ObjectRef::from_raw(mirror_ptr_usize as *mut u8) };
        let name_obj = match ctx.get_field(mirror, crate::SYNTHETIC_THREAD_MIRROR_NAME_SLOT) {
            Value::Object(Some(o)) => o,
            other => panic!("name slot must hold a String ref, got {other:?}"),
        };
        let read = ctx.read_string(name_obj).expect("string");
        assert_eq!(
            read, "k4-name-test",
            "Thread mirror name must equal the loop name"
        );
        shutdown_vertx_loop(el.id as i64);
    }

    /// **T19_K4-3** — The Thread mirror's `tid` field is set to the
    /// VM ThreadId so reflection / Thread.threadId() reads see a
    /// stable id without going through the registry.
    #[test]
    fn t19_k4_thread_mirror_tid_field_matches_vm_thread_id() {
        // FIX(test-isolation): shutdown pushes onto the shared dead-queue.
        let _guard = dead_queue_guard();
        let mut ctx = mock_ctx();
        let el = spawn_vertx_event_loop_with_ctx(&mut ctx, "k4-tid-test", false).expect("spawn");
        let vm_tid = el.vm_thread_id.load(Ordering::Acquire);
        assert_ne!(vm_tid, 0);
        let mirror_ptr = ctx.native_thread_java_obj_ptr(vm_tid);
        let mirror = unsafe { ObjectRef::from_raw(mirror_ptr as *mut u8) };
        match ctx.get_field(mirror, crate::SYNTHETIC_THREAD_MIRROR_TID_SLOT) {
            Value::Long(t) => assert_eq!(t as u64, vm_tid, "tid slot must equal vm_thread_id"),
            other => panic!("tid slot must hold a Long, got {other:?}"),
        }
        shutdown_vertx_loop(el.id as i64);
    }

    /// **T19_K4-4** — Each pool slot in `VertxImpl.init(N)` gets its
    /// own Thread mirror with a unique name. This is the "no name
    /// aliasing" check: 5 loops must have 5 distinct mirror
    /// pointers.
    #[test]
    fn t19_k4_vertx_init_creates_distinct_mirrors_per_slot() {
        // FIX(test-isolation): close() flushes the shared dead-queue; serialize.
        let _guard = dead_queue_guard();
        let mut ctx = mock_ctx();
        let mirror_obj =
            try_alloc_concurrent_synthetic(&mut ctx, CLS_VERTX_IMPL, VERTX_NUM_SLOTS).unwrap();
        let pool: i32 = 5;
        let before_thread_count = ctx.registered_native_threads().len();
        native_vertx_init(
            &mut ctx,
            &[Value::Object(Some(mirror_obj)), Value::Int(pool)],
        )
        .expect("init");
        let after_thread_count = ctx.registered_native_threads().len();
        assert_eq!(after_thread_count - before_thread_count, pool as usize);
        // Collect the registered tids and assert each has a distinct mirror.
        let mut mirror_ptrs = std::collections::HashSet::new();
        // mock allocates contiguous tids starting at 1 (or wherever
        // it was after previous tests). Walk the new range.
        for i in 0..pool {
            // FIX(test-isolation): tids are no longer 1-based — MockNativeContext
            // now assigns globally-unique tids from `native_tid_base`, so the
            // k-th registration has tid `native_tid_base + k` (read it back
            // rather than assuming a base of 1).
            let entries = ctx.registered_native_threads();
            let slot = before_thread_count + i as usize;
            let entry = &entries[slot];
            let actual_tid = ctx.native_tid_base + slot as u64;
            assert_eq!(entry.0, format!("vert.x-eventloop-{i}"));
            let ptr = ctx.native_thread_java_obj_ptr(actual_tid);
            assert_ne!(ptr, 0, "slot {i} must have a mirror");
            assert!(mirror_ptrs.insert(ptr), "slot {i} mirror must be unique");
        }
        native_vertx_close(&mut ctx, &[Value::Object(Some(mirror_obj))]).ok();
    }

    /// **T19_K4-5** — Legacy `spawn_vertx_event_loop` (no ctx) does
    /// NOT allocate a mirror — `java_thread_mirror_ptr` stays 0.
    /// Important so unit tests that use the legacy path don't
    /// accidentally pollute the K4 mock state.
    #[test]
    fn t19_k4_legacy_spawn_does_not_allocate_mirror() {
        // FIX(test-isolation): legacy spawn/shutdown still mutate the shared
        // loop registry / join-handle map / `NEXT_LOOP_ID`; serialize.
        let _guard = isolated_vertx_test();
        let el = spawn_vertx_event_loop("k4-legacy-spawn").expect("spawn");
        assert_eq!(
            el.java_thread_mirror_ptr.load(Ordering::Acquire),
            0,
            "legacy spawn must leave java_thread_mirror_ptr at 0"
        );
        shutdown_vertx_loop(el.id as i64);
    }

    /// **T19_K4-6** — `spawn_vertx_event_loop_with_ctx` registers as
    /// non-daemon by default (Vert.x event loops are non-daemon per
    /// JDK contract). Combined with a Thread mirror, this is what
    /// keeps the process alive past `main()`.
    #[test]
    fn t19_k4_spawn_default_is_non_daemon_with_mirror() {
        // FIX(test-isolation): shutdown pushes onto the shared dead-queue.
        let _guard = dead_queue_guard();
        let mut ctx = mock_ctx();
        let el = spawn_vertx_event_loop_with_ctx(&mut ctx, "k4-non-daemon", false).expect("spawn");
        let entries = ctx.registered_native_threads();
        let last = entries.last().unwrap();
        assert_eq!(last.0, "k4-non-daemon");
        assert!(!last.1, "must be non-daemon");
        assert!(last.2, "must be alive");
        let vm_tid = el.vm_thread_id.load(Ordering::Acquire);
        let ptr = ctx.native_thread_java_obj_ptr(vm_tid);
        assert_ne!(ptr, 0, "non-daemon spawn must still attach a mirror");
        shutdown_vertx_loop(el.id as i64);
    }

    /// **T19_K4-7** — Daemon flag set to `true` still allocates a
    /// mirror. (Mirror allocation is independent of daemon-ness; the
    /// daemon flag only affects whether `wait_for_non_daemon_threads`
    /// blocks on us.)
    #[test]
    fn t19_k4_daemon_spawn_still_attaches_mirror() {
        // FIX(test-isolation): shutdown pushes onto the shared dead-queue.
        let _guard = dead_queue_guard();
        let mut ctx = mock_ctx();
        let el = spawn_vertx_event_loop_with_ctx(&mut ctx, "k4-daemon-true", true).expect("spawn");
        let vm_tid = el.vm_thread_id.load(Ordering::Acquire);
        let ptr = ctx.native_thread_java_obj_ptr(vm_tid);
        assert_ne!(ptr, 0, "daemon=true must still attach a mirror");
        let entries = ctx.registered_native_threads();
        let last = entries.last().unwrap();
        assert!(last.1, "must be daemon");
        shutdown_vertx_loop(el.id as i64);
    }

    /// **T19_K4-8** — `set_native_thread_java_obj` rejects an unknown
    /// thread id. Defends against a hostile native that obtains a
    /// mirror reference and tries to forge a registration.
    #[test]
    fn t19_k4_set_native_thread_java_obj_rejects_unknown_id() {
        let mut ctx = mock_ctx();
        let mirror = ctx.alloc_object(cratonvm_types::ClassId::new(0), 5);
        // Bogus thread id past the high-water mark.
        let res = ctx.set_native_thread_java_obj(u64::MAX / 2, mirror);
        assert!(!res, "unknown thread id must return false");
        // Zero is also rejected.
        let res = ctx.set_native_thread_java_obj(0, mirror);
        assert!(!res, "zero thread id must return false");
    }

    /// **T19_K4-9** — Mirror pointer is stable across loop lifetime:
    /// re-reading `java_thread_mirror_ptr` returns the same value
    /// before and after the loop has scheduled some work. (No
    /// reallocation, no GC churn.)
    #[test]
    fn t19_k4_mirror_ptr_stable_over_loop_lifetime() {
        // FIX(test-isolation): shutdown pushes onto the shared dead-queue.
        let _guard = dead_queue_guard();
        let mut ctx = mock_ctx();
        let el = spawn_vertx_event_loop_with_ctx(&mut ctx, "k4-stable-ptr", false).expect("spawn");
        let initial = el.java_thread_mirror_ptr.load(Ordering::Acquire);
        assert_ne!(initial, 0);
        // Wait for loop to claim its thread id so the next checks
        // see a fully-running loop.
        for _ in 0..200 {
            if el.loop_thread_id.load(Ordering::Acquire) != 0 {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        // Schedule a few tasks.
        for _ in 0..3 {
            let _ = el.schedule_task(Box::new(|| {}));
        }
        thread::sleep(Duration::from_millis(20));
        let after = el.java_thread_mirror_ptr.load(Ordering::Acquire);
        assert_eq!(initial, after, "mirror_ptr must be stable across activity");
        shutdown_vertx_loop(el.id as i64);
    }

    /// **T19_K4-10** — Multiple VertxImpl instances each get distinct
    /// mirrors per slot — no leakage between groups.
    #[test]
    fn t19_k4_multiple_vertx_groups_have_disjoint_mirrors() {
        // FIX(test-isolation): close() flushes the shared dead-queue; serialize.
        let _guard = dead_queue_guard();
        let mut ctx = mock_ctx();
        let m_a =
            try_alloc_concurrent_synthetic(&mut ctx, CLS_VERTX_IMPL, VERTX_NUM_SLOTS).unwrap();
        let m_b =
            try_alloc_concurrent_synthetic(&mut ctx, CLS_VERTX_IMPL, VERTX_NUM_SLOTS).unwrap();
        let before = ctx.registered_native_threads().len();
        native_vertx_init(&mut ctx, &[Value::Object(Some(m_a)), Value::Int(2)]).expect("init A");
        native_vertx_init(&mut ctx, &[Value::Object(Some(m_b)), Value::Int(2)]).expect("init B");
        let after = ctx.registered_native_threads().len();
        assert_eq!(after - before, 4);
        // Collect mirror pointers for the 4 new entries.
        let mut ptrs = Vec::new();
        for i in 0..4 {
            // FIX(test-isolation): tids start at native_tid_base, not 1.
            let tid = ctx.native_tid_base + (before + i) as u64;
            let p = ctx.native_thread_java_obj_ptr(tid);
            assert_ne!(p, 0, "entry {i} must have a mirror");
            ptrs.push(p);
        }
        // All 4 must be distinct.
        let unique: std::collections::HashSet<_> = ptrs.iter().copied().collect();
        assert_eq!(
            unique.len(),
            4,
            "all 4 mirrors must be distinct: {:?}",
            ptrs
        );
        native_vertx_close(&mut ctx, &[Value::Object(Some(m_a))]).ok();
        native_vertx_close(&mut ctx, &[Value::Object(Some(m_b))]).ok();
    }

    /// **T19_K4-11** — `flush_native_thread_deaths` after shutdown
    /// does NOT clear the mirror pointer recorded on the loop. The
    /// loop's mirror_ptr is stable for the loop's lifetime; the
    /// registry's `is_alive` flips, but the mirror reference stays
    /// (so any caller still holding the loop ref can read the
    /// mirror).
    #[test]
    fn t19_k4_shutdown_does_not_clear_mirror_ptr() {
        // FIX(test-isolation): this test drains/flushes the shared dead-queue
        // waiting for its own shutdown event; serialize + reset.
        let _guard = dead_queue_guard();
        let mut ctx = mock_ctx();
        let el =
            spawn_vertx_event_loop_with_ctx(&mut ctx, "k4-shutdown-keep", false).expect("spawn");
        let mirror_before = el.java_thread_mirror_ptr.load(Ordering::Acquire);
        assert_ne!(mirror_before, 0);
        shutdown_vertx_loop(el.id as i64);
        // Drain the dead-queue so flush_native_thread_deaths sees
        // the shutdown event.
        let deadline = Instant::now() + Duration::from_millis(1_000);
        while Instant::now() < deadline {
            flush_native_thread_deaths(&mut ctx);
            // After shutdown the loop's vm_thread_id is still set;
            // mirror_ptr too. Only the registry's alive flag flips.
            if !drain_native_thread_dead_queue().is_empty() {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        let mirror_after = el.java_thread_mirror_ptr.load(Ordering::Acquire);
        assert_eq!(
            mirror_before, mirror_after,
            "mirror_ptr must not be reset on shutdown"
        );
    }
}
