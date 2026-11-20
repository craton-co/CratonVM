// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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

use cratonvm_native_api::{NativeContext, NativeMethodRegistry, NativeThreadBlocker};
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

/// A `Runnable` queued by a nested `execute` / `schedule(0)` until the
/// outermost one drains it.
///
/// gc-common w19-f: the queue used to hold the bare `ObjectRef`. The outer
/// task's `run()` (Java) keeps running after the enqueue, so a collection could
/// move the queued task, or free it outright when the queue was its only
/// holder (a lambda handed straight to `execute`), and the drain then
/// dispatched `run()` on a dead address. It is now held by a global root for
/// the time it is queued; `fallback` is used only by a context without global
/// roots (handle 0), and `vm` keeps one VM from draining another's handle.
struct NelQueuedTask {
    vm: usize,
    root: usize,
    fallback: ObjectRef,
}

thread_local! {
    static NEL_EXEC_DEPTH: Cell<usize> = const { Cell::new(0) };
    static NEL_EXEC_QUEUE: RefCell<VecDeque<NelQueuedTask>> = RefCell::new(VecDeque::new());
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
    /// by unit tests). Read-only after spawn. The thread's death is told to
    /// the VM by its own retirement (`NativeThreadBlocker::retire`).
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
    run_vertx_event_loop_with_blocker(el, None);
}

/// [`run_vertx_event_loop`] for a carrier registered with the VM
/// ([`spawn_vertx_event_loop_with_ctx`]): `blocker` publishes the carrier's
/// OS thread id and brackets its idle park (phase 3) as a GC-blocked native
/// wait, the way the XNIO loop brackets its `select`. The carrier runs Rust
/// tasks only (a posted Java `Runnable` runs on the submitting thread,
/// `native_nel_execute`), so the park's end is its one transition back into
/// the VM: a JDWP suspension now holds it there, where it used to be counted
/// suspended while its timers kept firing (interpreter round i1 wave 19,
/// lane L3; `i9-L1-jdwp-suspension-does-not-reach-compiled-or-native-code`
/// item 4).
pub fn run_vertx_event_loop_with_blocker(
    el: Arc<VertxEventLoop>,
    blocker: Option<Arc<dyn NativeThreadBlocker>>,
) {
    el.loop_thread_id.store(os_tid(), Ordering::Release);
    if let Some(blocker) = blocker.as_deref() {
        blocker.publish_os_tid();
    }

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
            let _blocked = crate::xnio_io_thread::NativeSelectBlock::enter(blocker.as_deref());
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
            // `None` = the class could not be resolved (`--real-jdk` refuses to
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

    // A VM-registered carrier parks through the VM's native-thread blocker,
    // as the XNIO I/O thread selects through it (`xnio_io_thread`).
    let blocker_for_thread = match register.as_mut() {
        Some((ctx, _)) if registered_vm_tid != 0 => ctx.native_thread_blocker(registered_vm_tid),
        _ => None,
    };
    let el_for_thread = Arc::clone(&el);
    let jh = thread::Builder::new().name(name.clone()).spawn(move || {
        // gcj j2/cm2: kept for the retirement below.
        let retiring = blocker_for_thread.clone();
        run_vertx_event_loop_with_blocker(Arc::clone(&el_for_thread), blocker_for_thread);
        // gcj j2/cm2 + gcj j3/rt3 residual 5
        // (`gcj-j2cm2-proposal-native-threads-own-their-death`): the VM
        // marks this thread dead now, on this thread, under the VM that
        // registered it, on every backend -- no stop-the-world census
        // counts a thread that will never arrive, and no process-wide
        // queue carries its per-VM id to another VM.
        if let Some(blocker) = retiring.as_deref() {
            blocker.retire();
        }
    });
    let jh = match jh {
        Ok(jh) => jh,
        Err(e) => {
            // gcj j2/cm2: the entry registered above has no thread behind it;
            // left alive it would be counted by every later stop-the-world
            // census (`NativeContext::abandon_native_thread`).
            if let Some((ctx, _)) = register.as_mut() {
                if registered_vm_tid != 0 {
                    ctx.abandon_native_thread(registered_vm_tid);
                }
            }
            return Err(format!("spawn_vertx_event_loop: {e}"));
        }
    };

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
// Per-VertxImpl group: maps the VertxImpl's weak lock key → list of owned
// loop ids so close() can tear them all down.
//
// gch h1/ak: the key was the object's ADDRESS. A moving collection between
// `init` and `close` stranded the row (close found nothing and the loops kept
// running), and a later `VertxImpl` placed at a dead one's address could close
// the dead one's loops. A weak lock key follows the object across moves and is
// never re-minted. A dead, never-closed instance's row stays (its loops are
// not stopped: nothing in HotSpot stops a thread when an object dies either);
// it can no longer be read.
// ---------------------------------------------------------------------------

// addr-keyed: not an address -- VertxImpl weak lock keys (never re-minted); removed by close().
static VERTX_GROUPS: OnceLock<Mutex<HashMap<usize, Vec<u64>>>> = OnceLock::new();

// addr-keyed: the accessor of VERTX_GROUPS (see its disposition).
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
    // gch h1/ak: the group is filed under `this`'s weak lock key, minted
    // NOW, while `this` is current -- the spawns below can allocate, and the
    // registry follows the object across a move where a raw address does not.
    let group_key = crate::gc_stable_weak_lock_key(&*ctx, this)?;
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
    store_group(group_key, ids);
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
    if let Some(ids) = crate::existing_weak_lock_key(&*ctx, this).and_then(take_group) {
        for raw in ids {
            shutdown_vertx_loop(raw as i64);
        }
    }
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
            true
        } else {
            depth.set(1);
            false
        }
    });
    if nested {
        let task = NelQueuedTask {
            vm: ctx.vm_identity(),
            root: ctx.add_global_root(runnable),
            fallback: runnable,
        };
        NEL_EXEC_QUEUE.with(|queue| queue.borrow_mut().push_back(task));
        return;
    }

    invoke_nel_runnable(ctx, runnable, operation);

    let vm = ctx.vm_identity();
    let mut drained = 0usize;
    loop {
        let next = NEL_EXEC_QUEUE.with(|queue| queue.borrow_mut().pop_front());
        let Some(task) = next else { break };
        drained += 1;
        if drained > NEL_EXEC_DRAIN_LIMIT {
            // Put it back: the message promises the rest stays queued (the
            // popped task used to be dropped here).
            NEL_EXEC_QUEUE.with(|queue| queue.borrow_mut().push_front(task));
            tracing::warn!(
                limit = NEL_EXEC_DRAIN_LIMIT,
                "NioEventLoop execute trampoline reached drain limit; leaving remaining tasks queued",
            );
            break;
        }
        if task.vm != vm {
            // Another VM's handle means nothing here (a thread reused across
            // VMs in one process); its task cannot run on this heap.
            continue;
        }
        let next = if task.root != 0 {
            let current = ctx.resolve_global_root(task.root).unwrap_or(task.fallback);
            let _ = ctx.remove_global_root(task.root);
            current
        } else {
            task.fallback
        };
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
            return Ok(Some(Value::Object(Some(alloc_scheduled_future_obj(ctx)?))));
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
            return Ok(Some(Value::Object(Some(alloc_scheduled_future_obj(ctx)?))));
        }
    };
    let raw = match ctx.get_field(this, NEL_FIELD_EVENT_LOOP_ID) {
        Value::Long(v) => v,
        _ => {
            return Ok(Some(Value::Object(Some(alloc_scheduled_future_obj(ctx)?))));
        }
    };
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
    // gc-common w19-f: the future is allocated AFTER the task runs. It used to
    // be allocated first, so the `Runnable` was dispatched at its address from
    // before that allocation, and the future was returned at its address from
    // before `run()` (Java). Both effects happen on every path, as before.
    let sf = alloc_scheduled_future_obj(ctx)?;
    Ok(Some(Value::Object(Some(sf))))
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
            // VertxImpl
    registry.register(CLS_VERTX_IMPL, "init", "(I)V", native_vertx_init, cratonvm_native_api::NativeKind::Bridge);
    registry.register(
        CLS_VERTX_IMPL,
        "getEventLoopId",
        "()J",
        native_vertx_get_event_loop_id, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(CLS_VERTX_IMPL, "close", "()V", native_vertx_close, cratonvm_native_api::NativeKind::Bridge);

    // A real Netty DefaultEventLoop owns its queue, worker lifecycle and
    // shutdown state. Its former CratonVM override ran tasks synchronously on
    // the submitting thread, which can deadlock asynchronous clients such as
    // the Cassandra driver. Keep these helpers for direct synthetic tests, but
    // do not register them: production Netty must always execute its bytecode.
    }

// ===========================================================================
//                              UNIT TESTS
// ===========================================================================

#[cfg(test)]
mod tests;

#[cfg(test)]
mod w19f_nel_queue_tests;

#[cfg(test)]
mod gch_h1ak_vertx_group_key_tests;
