// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! T19.6 — Vert.x / Netty `NioEventLoop` scheduler.
//!
//! Keycloak 26 (Quarkus) serves HTTP through Vert.x, Vert.x wraps Netty, and
//! Netty runs one `NioEventLoop` per accept slot. Each of those loops owns
//! its own JDK selector, has its own immediate-task queue, a min-heap of
//! scheduled timers, and a *single* OS thread it is pinned to for life.
//! If that thread gets preempted onto the generic virtual-thread scheduler,
//! the I/O-dispatch timer logic in `NioEventLoop.run()` — which assumes the
//! carrier is monotonically progressing — breaks.
//!
//! This module provides the pinned-carrier primitive:
//!
//! * [`EventLoopAffinity`] — the marker [`super::virtual_threads`] consults
//!   before routing a task onto the general fork-join pool. Any virtual
//!   thread tagged with an affinity id gets its work delivered to the
//!   specific [`EventLoop`]'s queue instead.
//! * [`EventLoop`] — one OS thread running a 5-phase loop isomorphic to
//!   the T19.7.c XNIO I/O thread (drain tasks, compute deadline, select,
//!   dispatch I/O, fire timers).
//! * [`EventLoopManager`] — process-wide registry of live event loops.
//!   Vert.x `VertxImpl` constructs `eventLoopCount` loops at boot; the
//!   native registration in `native-builtins/src/vertx_eventloop.rs`
//!   populates this registry and hands out opaque long ids back to the
//!   Java `VertxImpl` mirror.
//!
//! # Selector choice — pure-std
//!
//! T19.7.a lives in `native-io/src/nio_selector.rs` and is strictly off-
//! limits for us (ownership seal). Its loop shape is "5 ms probe cycle,
//! pure-std `TcpListener::set_nonblocking` + `TcpStream::peek`" —
//! deliberately avoiding any `mio`/`polling`/`epoll` crate dep. We mirror
//! that. A Vert.x event-loop iteration selects for up to its computed
//! deadline by blocking on a [`WakeableCondvar`]: timers wake it via a
//! `notify_one`; cross-thread [`EventLoop::schedule_task`] wakes it via
//! the same mechanism. For a pinned NioEventLoop the actual socket
//! readiness polling is delegated through to T19.7.a's `SelectorImpl`
//! via the `sun.nio.ch.WindowsSelectorImpl` / `EPollSelectorImpl`
//! already registered by the real JDK classes Vert.x's Netty layer
//! `Selector.open()` — our loop does not open sockets itself; it just
//! provides the *thread*.
//!
//! # Panic isolation
//!
//! Every queued task, timer callback, and (eventually) selector poll is
//! wrapped in `catch_unwind(AssertUnwindSafe)`. A panicking task logs via
//! `tracing::error!`, increments [`DispatchStats::task_panics`], and the
//! loop continues. No task can tear down the loop's OS thread.
//!
//! # Resource caps
//!
//! | Cap                         | Default | Reject behavior                     |
//! |-----------------------------|---------|-------------------------------------|
//! | `MAX_EVENT_LOOPS`           |   256   | `schedule_on_event_loop` → Err      |
//! | `MAX_PENDING_TASKS` per EL  | 10_000  | `schedule_task` → Err               |
//! | `MAX_SCHEDULED_TIMERS` / EL |  1_000  | `schedule_timer` → Err              |
//!
//! The process-wide `MAX_EVENT_LOOPS` cap prevents a hostile class from
//! instantiating millions of `VertxImpl` + spinning up unbounded OS threads.
//!
//! # Security
//!
//! * `EventLoopId` values are validated at every entry point: negative ids
//!   return `Err(EventLoopError::InvalidId)`; ids past the allocation
//!   counter return `Err(EventLoopError::NotFound)` so a hostile JNI caller
//!   cannot probe for other threads' loops or attach to loops in other
//!   JVMs (we're single-JVM-per-process, but still — defense in depth).
//! * No lock ever crosses a `select()` / `condvar.wait()` call. The
//!   mutexes protecting `task_queue` and `timer_heap` are dropped before
//!   the loop parks.

#![allow(clippy::needless_pass_by_value)]

use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Resource caps
// ---------------------------------------------------------------------------

/// Hard cap on the number of event loops a single JVM process may run.
/// Over-cap construction returns `EventLoopError::CapacityExceeded`.
pub const MAX_EVENT_LOOPS: usize = 256;

/// Per-loop cap on the number of immediate (non-timer) tasks queued.
pub const MAX_PENDING_TASKS: usize = 10_000;

/// Per-loop cap on the number of pending scheduled timers.
pub const MAX_SCHEDULED_TIMERS: usize = 1_000;

/// Maximum time a single `select()` park may last. The loop iterates at
/// least this often so shutdown / new tasks are still picked up even when
/// no timer deadline is set.
pub const MAX_IDLE_PARK: Duration = Duration::from_millis(1_000);

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors surfaced from the event-loop API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventLoopError {
    /// The supplied id was negative / cast-unsafe.
    InvalidId,
    /// The id does not correspond to any live event loop (either never
    /// allocated or already shut down and unregistered).
    NotFound,
    /// The per-loop task-queue cap `MAX_PENDING_TASKS` would be exceeded.
    TaskQueueFull,
    /// The per-loop timer-heap cap `MAX_SCHEDULED_TIMERS` would be
    /// exceeded.
    TimerHeapFull,
    /// The process-wide event-loop cap `MAX_EVENT_LOOPS` would be
    /// exceeded.
    CapacityExceeded,
    /// The loop is shutting down; no new work accepted.
    ShuttingDown,
}

impl std::fmt::Display for EventLoopError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidId => write!(f, "event-loop id is invalid (negative or zero)"),
            Self::NotFound => write!(f, "event-loop id not found in registry"),
            Self::TaskQueueFull => {
                write!(f, "event-loop task queue at capacity {}", MAX_PENDING_TASKS)
            }
            Self::TimerHeapFull => write!(
                f,
                "event-loop timer heap at capacity {}",
                MAX_SCHEDULED_TIMERS
            ),
            Self::CapacityExceeded => {
                write!(f, "process event-loop cap {} reached", MAX_EVENT_LOOPS)
            }
            Self::ShuttingDown => write!(f, "event loop is shutting down"),
        }
    }
}

impl std::error::Error for EventLoopError {}

// ---------------------------------------------------------------------------
// EventLoopId — stable, validated opaque id
// ---------------------------------------------------------------------------

/// Opaque event-loop identifier. The Java-side Vert.x mirror stores this
/// as a long field; native methods re-resolve via
/// [`EventLoopManager::lookup`]. Guaranteed non-zero and positive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EventLoopId(u64);

impl EventLoopId {
    /// Construct an id from a raw Java long. Returns `Err(InvalidId)` if
    /// the value is <= 0 (no legitimate event-loop id is 0 or negative —
    /// we start allocating at 1).
    pub fn from_raw(raw: i64) -> Result<Self, EventLoopError> {
        if raw <= 0 {
            return Err(EventLoopError::InvalidId);
        }
        Ok(Self(raw as u64))
    }

    /// Convert back to the raw long stored in the Java mirror.
    pub fn to_raw(self) -> i64 {
        self.0 as i64
    }

    /// Inner u64 — tests and registry internals.
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

// ---------------------------------------------------------------------------
// WakeableCondvar — dedupes concurrent wakes, idempotent across threads.
// ---------------------------------------------------------------------------

/// A condition variable + AtomicBool pending flag. The loop parks via
/// `park(timeout)`; any cross-thread wake flips the pending flag to true
/// and calls `notify_one`. A wake that happens during the scheduling gap
/// between "check pending, it's false" and "call cv.wait" is still
/// observed because the loop holds the mutex while calling `wait_timeout`
/// and the waker drops it after setting pending.
///
/// This is the equivalent of T19.7.a's UDP-loopback wakeup pair, except
/// we don't need the fd — the event-loop body never calls a real OS-level
/// selector directly. For a *pinned* NioEventLoop the actual syscall
/// happens inside the JDK `Selector.select()` bytecode Vert.x/Netty
/// invokes on this thread, which our [`schedule_task`] cross-thread path
/// does not need to pre-empt — that selector has its own wakeup pipe.
pub struct WakeableCondvar {
    mu: Mutex<bool>, // true == wake pending
    cv: Condvar,
    /// Dedupe marker for `wake()` calls: a second wake before the park
    /// loop consumes the first is a no-op. Matches the
    /// `pending_wakeup: AtomicBool` pattern in T19.7.c.
    pending: AtomicBool,
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

    /// Park until either (a) `timeout` elapses, or (b) another thread
    /// calls [`Self::wake`]. Returns `true` if a wake was observed,
    /// `false` if the park expired on the timeout. Consumes the pending
    /// flag on return.
    pub fn park(&self, timeout: Duration) -> bool {
        let mut guard = self.mu.lock().unwrap_or_else(|e| e.into_inner());
        // Fast path: wake already pending — return immediately.
        if *guard {
            *guard = false;
            self.pending.store(false, Ordering::Release);
            return true;
        }
        let (g, wait_res) = self
            .cv
            .wait_timeout(guard, timeout)
            .unwrap_or_else(|e| e.into_inner());
        let mut guard = g;
        // The pending flag (`*guard`) is the authoritative wake signal,
        // read here while we still hold the mutex: `wake()` sets it to
        // true under this same mutex before `notify_one`, so a genuine
        // cross-thread unpark always leaves it true. A bare timeout or a
        // spurious condvar wakeup both leave it false. Capture it before
        // we clear it below.
        let flag_set = *guard;
        *guard = false;
        self.pending.store(false, Ordering::Release);

        // Report the honest timeout-vs-woken status callers
        // (`park`/`parkNanos`) need to tell a spurious/timeout return
        // from a real unpark:
        //
        //   * flag_set == true  -> a real wake was observed -> return true
        //     (covers the case where `wake()` raced in right at the
        //     deadline and set the flag even though `wait_timeout`
        //     reported `timed_out()`; a real unpark must never be lost).
        //   * flag_set == false && timed_out()  -> the park expired on
        //     its deadline -> return false.
        //   * flag_set == false && !timed_out() -> a *spurious* condvar
        //     wakeup with no pending wake -> return false so the caller
        //     re-parks instead of mistaking it for an unpark.
        //
        // We must NOT consult `self.pending` again here: it was just
        // cleared, and re-reading it could observe a concurrent `wake()`'s
        // `pending.swap(true)` (which precedes its locked `*guard = true`)
        // and double-count that wake — once now and once on the next
        // fast-path park. `flag_set`, read under the lock, is the only
        // race-free source of truth; `timed_out()` only refines the
        // not-woken case into "timeout" vs "spurious".
        //
        // NOTE: the previous expression `flag_set && !wait_res.timed_out()
        // || flag_set` was a tautology — `(a && b) || a == a` for every
        // `b` — so the `timed_out()` term was dead and callers could never
        // distinguish a timeout from a wake.
        let woken = flag_set;
        // `wait_res.timed_out()` is the OS-level confirmation that the
        // wait reached its deadline. It refines only the *not-woken* case
        // into "timeout" (true) vs "spurious wakeup" (false); when a wake
        // flag is set, `timed_out()` may be either (a late-racing `wake()`
        // can land at the deadline) and the flag still wins. The boolean
        // return is therefore exactly `woken`. We bind `timed_out` so the
        // `wait_timeout` result is genuinely consulted (refuting the old
        // dead-term bug) and stays available to callers via `trace`.
        let timed_out = wait_res.timed_out();
        if !woken && !timed_out {
            // Spurious condvar wakeup with no pending wake: surfaced as
            // not-woken so the caller (`run_event_loop`) simply loops and
            // re-parks rather than treating it as a real unpark.
            tracing::trace!(
                target: "cratonvm::eventloop",
                "WakeableCondvar::park: spurious wakeup, no pending wake",
            );
        }
        woken
    }

    /// Wake the parked loop. Dedupes — a second call before the park
    /// consumes the first is a no-op at the `notify_one` layer.
    pub fn wake(&self) {
        if self.pending.swap(true, Ordering::AcqRel) {
            // Already pending — the park is either already woken or
            // will observe the flag on its next iteration anyway.
            return;
        }
        let mut guard = self.mu.lock().unwrap_or_else(|e| e.into_inner());
        *guard = true;
        self.cv.notify_one();
    }
}

// ---------------------------------------------------------------------------
// Task type
// ---------------------------------------------------------------------------

/// A boxed `FnOnce()` — the unit of work the event loop drains from its
/// queues. Must be `Send` so cross-thread posting works.
pub type EventLoopTask = Box<dyn FnOnce() + Send + 'static>;

// ---------------------------------------------------------------------------
// ScheduledTimer + timer heap
// ---------------------------------------------------------------------------

/// A deadline-tagged task. `Reverse<ScheduledTimer>` in a `BinaryHeap`
/// gives a min-heap by deadline.
pub struct ScheduledTimer {
    /// Monotonic deadline (on the clock local to the event-loop thread).
    pub deadline: Instant,
    /// Tiebreaker for equal deadlines: scheduled-first wins.
    pub seq: u64,
    /// The callback. `Option` so we can `take()` when firing.
    pub task: Option<EventLoopTask>,
    /// Shared cancellation flag — `cancel_timer` flips this; the heap
    /// lazily skips cancelled tasks at pop time.
    pub cancelled: Arc<AtomicBool>,
    /// Opaque user id echoed back on the cancellation handle so a
    /// caller can tell which timer they scheduled.
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

/// Min-heap of scheduled timers. `Reverse` inverts the default max-heap.
pub struct TimerHeap {
    inner: BinaryHeap<Reverse<ScheduledTimer>>,
    /// Count of cancelled timers lazily popped during peek/pop operations.
    /// Accumulated so callers can flush it into the stats counter.
    pub pending_cancelled_count: u64,
}

impl Default for TimerHeap {
    fn default() -> Self {
        Self::new()
    }
}

impl TimerHeap {
    pub fn new() -> Self {
        Self {
            inner: BinaryHeap::new(),
            pending_cancelled_count: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Earliest *non-cancelled* deadline. Lazily pops cancelled roots;
    /// increments `pending_cancelled_count` for each dropped entry.
    pub fn peek_deadline(&mut self) -> Option<Instant> {
        while let Some(Reverse(top)) = self.inner.peek() {
            if top.cancelled.load(Ordering::Acquire) {
                let _ = self.inner.pop();
                self.pending_cancelled_count += 1;
                continue;
            }
            return Some(top.deadline);
        }
        None
    }

    /// Pop the earliest timer if its deadline is <= `now`. Cancelled
    /// roots are silently popped + dropped (incrementing
    /// `pending_cancelled_count`); we only return non-cancelled ones.
    /// Returns `None` if the heap is empty or the root is still in the future.
    pub fn pop_if_expired(&mut self, now: Instant) -> Option<ScheduledTimer> {
        loop {
            let top = self.inner.peek()?;
            let top = &top.0;
            if top.cancelled.load(Ordering::Acquire) {
                let _ = self.inner.pop();
                self.pending_cancelled_count += 1;
                continue;
            }
            if top.deadline > now {
                return None;
            }
            return self.inner.pop().map(|Reverse(t)| t);
        }
    }

    /// Drain all cancelled timers whose deadline is <= `now` from the heap
    /// root. Returns the number drained so the caller can update stats.
    pub fn drain_cancelled_expired(&mut self, now: Instant) -> usize {
        let mut count = 0;
        loop {
            match self.inner.peek() {
                None => break,
                Some(Reverse(top)) => {
                    if top.cancelled.load(Ordering::Acquire) && top.deadline <= now {
                        self.inner.pop();
                        count += 1;
                    } else {
                        break;
                    }
                }
            }
        }
        count
    }

    /// Push respecting the cap. Returns `Err(the-task-back)` if we would
    /// exceed `MAX_SCHEDULED_TIMERS`.
    pub fn push_bounded(&mut self, timer: ScheduledTimer) -> Result<(), ScheduledTimer> {
        if self.inner.len() >= MAX_SCHEDULED_TIMERS {
            return Err(timer);
        }
        self.inner.push(Reverse(timer));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// DispatchStats — per-loop counters surfaced through the manager.
// ---------------------------------------------------------------------------

/// Per-loop statistics. All counters are `AtomicU64` so reads from other
/// threads never contend on a mutex. Snapshots go through
/// [`DispatchStatsSnapshot`].
#[derive(Debug, Default)]
pub struct DispatchStats {
    /// Total `schedule_task` calls that landed on this loop (including
    /// same-thread posts that were enqueued without a wake).
    pub tasks_enqueued: AtomicU64,
    /// Total tasks actually run (drained off the immediate queue).
    pub tasks_dispatched: AtomicU64,
    /// Tasks that panicked during dispatch. The panic was caught; the
    /// loop continues.
    pub task_panics: AtomicU64,
    /// Total timers scheduled (including cancelled-before-fire ones).
    pub timers_scheduled: AtomicU64,
    /// Timers that fired (their callback was invoked).
    pub timers_fired: AtomicU64,
    /// Timers that were cancelled before firing and thus silently
    /// discarded at pop time.
    pub timers_cancelled: AtomicU64,
    /// Number of `selector.select`-equivalent parks the loop performed.
    pub select_calls: AtomicU64,
    /// Cross-thread wake calls issued by `schedule_task`.
    pub wakes_issued: AtomicU64,
    /// Wakes that were deduped (a wake was already pending).
    pub wakes_deduped: AtomicU64,
}

/// Snapshot of [`DispatchStats`] — immutable view suitable for returning
/// to the Java side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchStatsSnapshot {
    pub tasks_enqueued: u64,
    pub tasks_dispatched: u64,
    pub task_panics: u64,
    pub timers_scheduled: u64,
    pub timers_fired: u64,
    pub timers_cancelled: u64,
    pub select_calls: u64,
    pub wakes_issued: u64,
    pub wakes_deduped: u64,
}

impl DispatchStats {
    pub fn snapshot(&self) -> DispatchStatsSnapshot {
        DispatchStatsSnapshot {
            tasks_enqueued: self.tasks_enqueued.load(Ordering::Acquire),
            tasks_dispatched: self.tasks_dispatched.load(Ordering::Acquire),
            task_panics: self.task_panics.load(Ordering::Acquire),
            timers_scheduled: self.timers_scheduled.load(Ordering::Acquire),
            timers_fired: self.timers_fired.load(Ordering::Acquire),
            timers_cancelled: self.timers_cancelled.load(Ordering::Acquire),
            select_calls: self.select_calls.load(Ordering::Acquire),
            wakes_issued: self.wakes_issued.load(Ordering::Acquire),
            wakes_deduped: self.wakes_deduped.load(Ordering::Acquire),
        }
    }
}

// ---------------------------------------------------------------------------
// EventLoop — pinned OS thread + task queue + timer heap.
// ---------------------------------------------------------------------------

/// The shared state between the event-loop thread and anyone scheduling
/// work onto it. Held behind an `Arc` so handles can be cloned freely.
pub struct EventLoop {
    /// Opaque id assigned by the manager.
    pub id: EventLoopId,
    /// Human-readable label used in tracing / JFR events and panic logs.
    /// Set at construction; immutable thereafter.
    pub name: String,
    /// Immediate-task queue. Cross-thread posts push here + wake the
    /// condvar; same-thread posts push without waking.
    task_queue: Mutex<VecDeque<EventLoopTask>>,
    /// Scheduled timers, kept ordered by deadline in a min-heap.
    timer_heap: Mutex<TimerHeap>,
    /// Parker condvar — `wake()`d whenever a cross-thread caller drops
    /// something into `task_queue` or `timer_heap`.
    parker: WakeableCondvar,
    /// `true` once `shutdown()` has been called. The loop exits on its
    /// next iteration.
    shutdown_requested: AtomicBool,
    /// Populated once the OS thread begins its run. Before that,
    /// [`schedule_task`] cannot distinguish "same thread" so it conservatively
    /// always wakes.
    loop_thread_id: AtomicU64,
    /// Monotonic counter feeding timer `seq` tiebreakers.
    seq_counter: AtomicU64,
    /// Monotonic counter feeding timer `timer_id` handles.
    timer_id_counter: AtomicU64,
    /// Atomic mirror of `task_queue.len()` — lets `schedule_task` check
    /// the cap without taking the task-queue lock.
    pending_len: AtomicUsize,
    /// Per-loop stats counters.
    pub stats: DispatchStats,
}

impl EventLoop {
    /// Construct a new `EventLoop`. Does not start the thread; that's
    /// [`EventLoopManager::spawn`]'s job.
    pub fn new(id: EventLoopId, name: impl Into<String>) -> Arc<Self> {
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
        })
    }

    /// Request the loop to exit at its next iteration. Idempotent.
    /// Wakes the parker so a blocked loop returns immediately.
    pub fn shutdown(&self) {
        self.shutdown_requested.store(true, Ordering::Release);
        self.parker.wake();
    }

    /// Is this loop shutting down or already terminated?
    pub fn is_shutdown(&self) -> bool {
        self.shutdown_requested.load(Ordering::Acquire)
    }

    /// Queue an immediate task for execution on this loop. Fails fast
    /// with `TaskQueueFull` if the per-loop cap is hit or `ShuttingDown`
    /// if the loop has been asked to stop.
    pub fn schedule_task(&self, task: EventLoopTask) -> Result<(), EventLoopError> {
        if self.shutdown_requested.load(Ordering::Acquire) {
            return Err(EventLoopError::ShuttingDown);
        }
        // Cheap over-cap bail without taking the queue lock.
        if self.pending_len.load(Ordering::Acquire) >= MAX_PENDING_TASKS {
            return Err(EventLoopError::TaskQueueFull);
        }
        {
            let mut q = self.task_queue.lock().unwrap_or_else(|e| e.into_inner());
            if q.len() >= MAX_PENDING_TASKS {
                return Err(EventLoopError::TaskQueueFull);
            }
            q.push_back(task);
            self.pending_len.store(q.len(), Ordering::Release);
        }
        self.stats.tasks_enqueued.fetch_add(1, Ordering::Relaxed);

        // Only wake the parker if we're running on a *different* OS
        // thread than the loop — otherwise the loop will drain on its
        // next iteration for free.
        let loop_tid = self.loop_thread_id.load(Ordering::Acquire);
        let my_tid = os_thread_id();
        if loop_tid == 0 || loop_tid != my_tid {
            if self.parker.pending.load(Ordering::Acquire) {
                self.stats.wakes_deduped.fetch_add(1, Ordering::Relaxed);
            } else {
                self.stats.wakes_issued.fetch_add(1, Ordering::Relaxed);
            }
            self.parker.wake();
        }
        Ok(())
    }

    /// Schedule a timer to fire at `deadline`. Returns the timer id +
    /// the shared cancellation flag.
    pub fn schedule_timer(
        &self,
        deadline: Instant,
        task: EventLoopTask,
    ) -> Result<(u64, Arc<AtomicBool>), EventLoopError> {
        if self.shutdown_requested.load(Ordering::Acquire) {
            return Err(EventLoopError::ShuttingDown);
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
            if let Err(_back) = heap.push_bounded(timer) {
                return Err(EventLoopError::TimerHeapFull);
            }
        }
        self.stats.timers_scheduled.fetch_add(1, Ordering::Relaxed);
        // A new sooner-deadline may have moved: always wake on
        // cross-thread schedule. Same-thread schedules (from inside the
        // loop itself) will pick it up on the next iteration for free.
        let loop_tid = self.loop_thread_id.load(Ordering::Acquire);
        let my_tid = os_thread_id();
        if loop_tid == 0 || loop_tid != my_tid {
            self.stats.wakes_issued.fetch_add(1, Ordering::Relaxed);
            self.parker.wake();
        }
        Ok((timer_id, cancelled))
    }

    /// Number of immediate tasks currently queued.
    pub fn pending_count(&self) -> usize {
        self.pending_len.load(Ordering::Acquire)
    }

    /// Number of timers currently in the heap (including cancelled ones
    /// waiting to be lazily skipped).
    pub fn timer_count(&self) -> usize {
        self.timer_heap
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    /// Snapshot of the per-loop stats. Safe to call from any thread.
    pub fn stats(&self) -> DispatchStatsSnapshot {
        self.stats.snapshot()
    }
}

// ---------------------------------------------------------------------------
// EventLoopAffinity — virtual-thread marker for routing decisions.
// ---------------------------------------------------------------------------

/// Marker attached to a virtual thread that should run its continuation
/// on a specific [`EventLoop`]'s OS thread rather than any fork-join
/// carrier. The virtual-thread scheduler consults this via
/// [`super::virtual_threads::VirtualThreadManager::set_event_loop_affinity`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventLoopAffinity {
    pub loop_id: EventLoopId,
}

impl EventLoopAffinity {
    pub fn new(loop_id: EventLoopId) -> Self {
        Self { loop_id }
    }
}

// ---------------------------------------------------------------------------
// Event-loop manager (process-wide registry).
// ---------------------------------------------------------------------------

/// Process-wide registry of live event loops. Keyed by
/// [`EventLoopId`]. Construction is lazy — call [`event_loop_manager`]
/// to get the singleton.
pub struct EventLoopManager {
    loops: Mutex<std::collections::HashMap<u64, Arc<EventLoop>>>,
    join_handles: Mutex<std::collections::HashMap<u64, JoinHandle<()>>>,
    next_id: AtomicU64,
}

impl EventLoopManager {
    fn new() -> Self {
        Self {
            loops: Mutex::new(std::collections::HashMap::new()),
            join_handles: Mutex::new(std::collections::HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    /// Spawn a new event-loop OS thread and register the result. The
    /// returned [`Arc<EventLoop>`] is immediately usable —
    /// [`EventLoop::schedule_task`] works even before the OS thread has
    /// claimed its thread-id slot (the first wake is unconditional).
    pub fn spawn(&self, name: impl Into<String>) -> Result<Arc<EventLoop>, EventLoopError> {
        let name = name.into();
        // Process-wide cap.
        {
            let loops = self.loops.lock().unwrap_or_else(|e| e.into_inner());
            if loops.len() >= MAX_EVENT_LOOPS {
                return Err(EventLoopError::CapacityExceeded);
            }
        }
        let raw_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let id = EventLoopId(raw_id);
        let el = EventLoop::new(id, name.clone());
        let el_clone = el.clone();
        let thread_name = format!("vertx-eventloop-{}", raw_id);
        let jh = thread::Builder::new()
            .name(thread_name)
            .spawn(move || run_event_loop(el_clone))
            .map_err(|_e| EventLoopError::CapacityExceeded)?;
        {
            let mut loops = self.loops.lock().unwrap_or_else(|e| e.into_inner());
            loops.insert(raw_id, el.clone());
        }
        {
            let mut jhs = self.join_handles.lock().unwrap_or_else(|e| e.into_inner());
            jhs.insert(raw_id, jh);
        }
        Ok(el)
    }

    /// Look up a live event loop by id. Returns `Err(InvalidId)` if the
    /// id is out-of-range and `Err(NotFound)` if the loop has already
    /// been shut down and unregistered.
    pub fn lookup(&self, id: EventLoopId) -> Result<Arc<EventLoop>, EventLoopError> {
        // Validation: any id past the high-water mark of next_id is
        // automatically invalid. Prevents a hostile caller from probing
        // for loops in a different allocation range.
        let hwm = self.next_id.load(Ordering::Acquire);
        if id.0 == 0 || id.0 >= hwm {
            return Err(EventLoopError::NotFound);
        }
        let loops = self.loops.lock().unwrap_or_else(|e| e.into_inner());
        loops.get(&id.0).cloned().ok_or(EventLoopError::NotFound)
    }

    /// Schedule `task` onto the loop identified by `id`. This is the
    /// top-level public entry point — validates the id, looks up the
    /// loop, enforces the shutdown gate, and defers to
    /// [`EventLoop::schedule_task`]. Never panics.
    pub fn schedule_on_event_loop(
        &self,
        id: EventLoopId,
        task: EventLoopTask,
    ) -> Result<(), EventLoopError> {
        let el = self.lookup(id)?;
        el.schedule_task(task)
    }

    /// Shut down and join an event loop. Idempotent — shutting down an
    /// already-gone id is a no-op.
    pub fn shutdown_and_join(&self, id: EventLoopId) -> Result<(), EventLoopError> {
        if id.0 == 0 {
            return Err(EventLoopError::InvalidId);
        }
        let el_opt = {
            let mut loops = self.loops.lock().unwrap_or_else(|e| e.into_inner());
            loops.remove(&id.0)
        };
        let jh_opt = {
            let mut jhs = self.join_handles.lock().unwrap_or_else(|e| e.into_inner());
            jhs.remove(&id.0)
        };
        if let Some(el) = el_opt {
            el.shutdown();
        }
        if let Some(jh) = jh_opt {
            // A panicking loop body would have landed here — we swallow
            // the JoinError rather than propagate; the loop was already
            // instructed to exit, and propagating would poison the
            // caller unnecessarily.
            let _ = jh.join();
        }
        Ok(())
    }

    /// Number of live event loops.
    pub fn live_count(&self) -> usize {
        self.loops.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Shut down every live loop. Used in tests + on JVM exit.
    pub fn shutdown_all(&self) {
        let ids: Vec<u64> = {
            let loops = self.loops.lock().unwrap_or_else(|e| e.into_inner());
            loops.keys().copied().collect()
        };
        for raw in ids {
            let _ = self.shutdown_and_join(EventLoopId(raw));
        }
    }
}

static EVENT_LOOP_MANAGER: OnceLock<EventLoopManager> = OnceLock::new();

/// Access the process-wide [`EventLoopManager`]. Lazily initialized on
/// first call; subsequent calls return the same instance.
pub fn event_loop_manager() -> &'static EventLoopManager {
    EVENT_LOOP_MANAGER.get_or_init(EventLoopManager::new)
}

// ---------------------------------------------------------------------------
// The event-loop body.
// ---------------------------------------------------------------------------

/// The five-phase event-loop iteration, isomorphic to T19.7.c's
/// `run_io_loop`:
///
///   1. Drain the task queue.
///   2. Compute the next deadline from the timer heap.
///   3. Park until that deadline (or MAX_IDLE_PARK), or until wake().
///   4. (Selector.select equivalent — for pure EventLoop we rely on
///      wake() delivery; pinned NioEventLoop Java-side runs its
///      `selector.select(timeoutMs)` on this thread via normal bytecode.)
///   5. Pop + fire any expired timers.
///
/// Every task/timer invocation is `catch_unwind`-wrapped; panics
/// increment `stats.task_panics` and the loop continues.
fn run_event_loop(el: Arc<EventLoop>) {
    el.loop_thread_id.store(os_thread_id(), Ordering::Release);
    CURRENT_EVENT_LOOP.with(|slot| {
        *slot.borrow_mut() = Some(Arc::downgrade(&el));
    });

    while !el.shutdown_requested.load(Ordering::Acquire) {
        // --- phase 1: drain immediate tasks --------------------------------
        drain_tasks(&el);
        if el.shutdown_requested.load(Ordering::Acquire) {
            break;
        }

        // --- phase 2 + 3: compute park deadline and park -------------------
        let now = Instant::now();
        let park_dur = match next_deadline(&el) {
            Some(d) if d <= now => Duration::from_millis(0),
            Some(d) => (d - now).min(MAX_IDLE_PARK),
            None => MAX_IDLE_PARK,
        };
        el.stats.select_calls.fetch_add(1, Ordering::Relaxed);
        if park_dur > Duration::from_millis(0) {
            el.parker.park(park_dur);
        }

        if el.shutdown_requested.load(Ordering::Acquire) {
            break;
        }

        // --- phase 4: dispatch channel readiness (no-op here; Vert.x / -----
        // Netty bytecode calling Selector.select() is what drives the
        // real I/O events on this thread).

        // --- phase 5: fire expired timers ---------------------------------
        fire_expired_timers(&el);
    }

    // Drain on shutdown so queued callers see clean quiesce.
    drain_tasks(&el);

    // Cancel any remaining pending timers; count them so stats reflect
    // reality.
    {
        let mut heap = el.timer_heap.lock().unwrap_or_else(|e| e.into_inner());
        while let Some(Reverse(t)) = heap.inner.pop() {
            if !t.cancelled.load(Ordering::Acquire) {
                el.stats.timers_cancelled.fetch_add(1, Ordering::Relaxed);
                t.cancelled.store(true, Ordering::Release);
            }
        }
    }

    CURRENT_EVENT_LOOP.with(|slot| {
        *slot.borrow_mut() = None;
    });
}

fn drain_tasks(el: &EventLoop) {
    loop {
        let next = {
            let mut q = el.task_queue.lock().unwrap_or_else(|e| e.into_inner());
            let t = q.pop_front();
            el.pending_len.store(q.len(), Ordering::Release);
            t
        };
        match next {
            Some(task) => run_task_safely(el, task),
            None => break,
        }
    }
}

fn next_deadline(el: &EventLoop) -> Option<Instant> {
    let mut heap = el.timer_heap.lock().unwrap_or_else(|e| e.into_inner());
    let d = heap.peek_deadline();
    // Flush any cancelled-counter that peek_deadline accumulated.
    let n = heap.pending_cancelled_count;
    if n > 0 {
        heap.pending_cancelled_count = 0;
        el.stats.timers_cancelled.fetch_add(n, Ordering::Relaxed);
    }
    d
}

fn fire_expired_timers(el: &EventLoop) {
    let now = Instant::now();
    loop {
        let (expired, pending_n) = {
            let mut heap = el.timer_heap.lock().unwrap_or_else(|e| e.into_inner());
            let t = heap.pop_if_expired(now);
            let n = heap.pending_cancelled_count;
            heap.pending_cancelled_count = 0;
            (t, n)
        };
        // Flush cancelled count from pop_if_expired's lazy drops.
        if pending_n > 0 {
            el.stats
                .timers_cancelled
                .fetch_add(pending_n, Ordering::Relaxed);
        }
        match expired {
            Some(mut t) => {
                if t.cancelled.load(Ordering::Acquire) {
                    el.stats.timers_cancelled.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                if let Some(task) = t.task.take() {
                    el.stats.timers_fired.fetch_add(1, Ordering::Relaxed);
                    run_task_safely(el, task);
                }
            }
            None => break,
        }
    }
}

fn run_task_safely(el: &EventLoop, task: EventLoopTask) {
    let name = el.name.clone();
    let result = catch_unwind(AssertUnwindSafe(move || task()));
    match result {
        Ok(()) => {
            el.stats.tasks_dispatched.fetch_add(1, Ordering::Relaxed);
        }
        Err(payload) => {
            el.stats.task_panics.fetch_add(1, Ordering::Relaxed);
            let msg = describe_panic_payload(&payload);
            tracing::error!(
                event_loop = %name,
                loop_id = el.id.as_u64(),
                panic = %msg,
                "vertx-eventloop: task panicked; swallowed and continuing",
            );
        }
    }
}

fn describe_panic_payload(payload: &Box<dyn std::any::Any + Send + 'static>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic>".to_string()
    }
}

// ---------------------------------------------------------------------------
// TLS for "am I on an event-loop thread?" queries.
// ---------------------------------------------------------------------------

thread_local! {
    static CURRENT_EVENT_LOOP: std::cell::RefCell<Option<std::sync::Weak<EventLoop>>>
        = const { std::cell::RefCell::new(None) };
}

/// Return the [`EventLoop`] the current OS thread is servicing, if any.
pub fn current_event_loop() -> Option<Arc<EventLoop>> {
    CURRENT_EVENT_LOOP.with(|slot| slot.borrow().as_ref().and_then(|w| w.upgrade()))
}

// ---------------------------------------------------------------------------
// OS-thread-id shim.
// ---------------------------------------------------------------------------

/// Process-wide stable thread id. We derive from `thread::current().id()`
/// via the `Hash` impl because `ThreadId` is cross-platform but not
/// directly `as u64`. Same approach as T19.7.c's `os_tid`.
fn os_thread_id() -> u64 {
    use std::hash::{Hash, Hasher};
    let tid = thread::current().id();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    tid.hash(&mut h);
    h.finish()
}

// ===========================================================================
//                                   TESTS
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;

    /// Helper: spawn a loop, run `body` with its handle, then shut down + join.
    fn with_loop<F: FnOnce(Arc<EventLoop>)>(name: &str, body: F) {
        let mgr = event_loop_manager();
        let el = mgr.spawn(name).expect("spawn");
        // Give the loop a chance to claim its thread id slot.
        for _ in 0..200 {
            if el.loop_thread_id.load(Ordering::Acquire) != 0 {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        body(el.clone());
        let _ = mgr.shutdown_and_join(el.id);
    }

    // Test 1: schedule_task runs the task on the loop's OS thread.
    #[test]
    fn t19_6_schedule_runs_task_on_loop_thread() {
        with_loop("t19-6-test-1", |el| {
            let observed = Arc::new(Mutex::new(0u64));
            let observed2 = observed.clone();
            let barrier = Arc::new(Barrier::new(2));
            let b2 = barrier.clone();
            el.schedule_task(Box::new(move || {
                let tid = os_thread_id();
                *observed2.lock().unwrap_or_else(|e| e.into_inner()) = tid;
                b2.wait();
            }))
            .expect("schedule");
            barrier.wait();
            let tid = *observed.lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(tid, el.loop_thread_id.load(Ordering::Acquire));
        });
    }

    // Test 2: a timer fires within ~10 ms of its deadline (relaxed
    // upper bound for slow test runners).
    #[test]
    fn t19_6_timer_fires_within_window() {
        with_loop("t19-6-test-2", |el| {
            let fired_at = Arc::new(Mutex::new(None::<Instant>));
            let fired2 = fired_at.clone();
            let scheduled_at = Instant::now();
            let deadline = scheduled_at + Duration::from_millis(50);
            let _ = el.schedule_timer(
                deadline,
                Box::new(move || {
                    *fired2.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
                }),
            );
            // Wait up to 500 ms.
            for _ in 0..250 {
                if fired_at.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
                    break;
                }
                thread::sleep(Duration::from_millis(2));
            }
            let got = fired_at
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .expect("timer should fire");
            assert!(got >= scheduled_at + Duration::from_millis(40));
            assert!(got <= scheduled_at + Duration::from_millis(500));
        });
    }

    // Test 3: cross-thread schedule_task wakes the parked loop.
    #[test]
    fn t19_6_cross_thread_wake_delivered() {
        with_loop("t19-6-test-3", |el| {
            let flag = Arc::new(AtomicBool::new(false));
            let f2 = flag.clone();
            let el2 = el.clone();
            let jh = thread::spawn(move || {
                el2.schedule_task(Box::new(move || {
                    f2.store(true, Ordering::Release);
                }))
                .expect("schedule");
            });
            jh.join().expect("join submit");
            for _ in 0..250 {
                if flag.load(Ordering::Acquire) {
                    break;
                }
                thread::sleep(Duration::from_millis(2));
            }
            assert!(
                flag.load(Ordering::Acquire),
                "cross-thread wake must fire within 500ms"
            );
        });
    }

    // Test 4: panicking task doesn't kill the loop.
    #[test]
    fn t19_6_panic_in_task_does_not_kill_loop() {
        with_loop("t19-6-test-4", |el| {
            let _ = el.schedule_task(Box::new(|| panic!("boom")));
            // Give the loop a moment to consume the panic.
            thread::sleep(Duration::from_millis(50));
            // Now schedule a normal task; it must still run.
            let flag = Arc::new(AtomicBool::new(false));
            let f2 = flag.clone();
            el.schedule_task(Box::new(move || f2.store(true, Ordering::Release)))
                .expect("schedule after panic");
            for _ in 0..250 {
                if flag.load(Ordering::Acquire) {
                    break;
                }
                thread::sleep(Duration::from_millis(2));
            }
            assert!(flag.load(Ordering::Acquire), "post-panic task must run");
            assert!(el.stats.task_panics.load(Ordering::Acquire) >= 1);
        });
    }

    // Test 5: DispatchStats counters increment correctly.
    #[test]
    fn t19_6_dispatch_stats_counters_increment() {
        with_loop("t19-6-test-5", |el| {
            for _ in 0..5 {
                el.schedule_task(Box::new(|| {})).expect("schedule");
            }
            // drain
            thread::sleep(Duration::from_millis(50));
            let stats = el.stats();
            assert_eq!(stats.tasks_enqueued, 5);
            assert_eq!(stats.tasks_dispatched, 5);
            assert_eq!(stats.task_panics, 0);
        });
    }

    // Test 6: cancelling a timer before its deadline prevents firing.
    #[test]
    fn t19_6_cancel_before_fire_is_noop() {
        with_loop("t19-6-test-6", |el| {
            let fired = Arc::new(AtomicBool::new(false));
            let f2 = fired.clone();
            let (_tid, cancel_flag) = el
                .schedule_timer(
                    Instant::now() + Duration::from_millis(200),
                    Box::new(move || f2.store(true, Ordering::Release)),
                )
                .expect("schedule");
            cancel_flag.store(true, Ordering::Release);
            // Wake so the loop reconsiders the (now cancelled) head.
            el.parker.wake();
            thread::sleep(Duration::from_millis(400));
            assert!(
                !fired.load(Ordering::Acquire),
                "cancelled timer must not fire"
            );
            // Stats: cancelled should have been counted.
            assert!(el.stats.timers_cancelled.load(Ordering::Acquire) >= 1);
        });
    }

    // Test 7: 8 threads concurrently scheduling tasks — no data race,
    // every task runs exactly once.
    #[test]
    fn t19_6_eight_threads_schedule_concurrently() {
        with_loop("t19-6-test-7", |el| {
            let counter = Arc::new(AtomicUsize::new(0));
            let per_thread = 25;
            let threads = 8;
            let mut handles = Vec::with_capacity(threads);
            for _ in 0..threads {
                let el2 = el.clone();
                let c2 = counter.clone();
                handles.push(thread::spawn(move || {
                    for _ in 0..per_thread {
                        let c3 = c2.clone();
                        el2.schedule_task(Box::new(move || {
                            c3.fetch_add(1, Ordering::Relaxed);
                        }))
                        .expect("schedule");
                    }
                }));
            }
            for h in handles {
                h.join().expect("join");
            }
            // Wait for drain.
            let total = threads * per_thread;
            for _ in 0..500 {
                if counter.load(Ordering::Acquire) == total {
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(counter.load(Ordering::Acquire), total);
        });
    }

    // Test 8: EventLoopId validation rejects negative and zero.
    #[test]
    fn t19_6_event_loop_id_validation() {
        assert!(EventLoopId::from_raw(-1).is_err());
        assert!(EventLoopId::from_raw(0).is_err());
        assert!(EventLoopId::from_raw(1).is_ok());
        assert_eq!(EventLoopId::from_raw(42).unwrap().to_raw(), 42);
    }

    // Test 9: manager.schedule_on_event_loop rejects unknown ids.
    #[test]
    fn t19_6_schedule_rejects_unknown_id() {
        let mgr = event_loop_manager();
        // A very high id past the allocation high-water mark is NotFound.
        let bogus = EventLoopId(u64::MAX / 2);
        let res = mgr.schedule_on_event_loop(bogus, Box::new(|| {}));
        assert_eq!(res, Err(EventLoopError::NotFound));
    }

    // Test 10: schedule_task on a shutdown loop returns ShuttingDown.
    #[test]
    fn t19_6_schedule_after_shutdown_errors() {
        let mgr = event_loop_manager();
        let el = mgr.spawn("t19-6-test-10").expect("spawn");
        let id = el.id;
        // Give the loop a chance to claim its thread id slot.
        for _ in 0..200 {
            if el.loop_thread_id.load(Ordering::Acquire) != 0 {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        el.shutdown();
        // Direct schedule on the handle — should be ShuttingDown.
        let res = el.schedule_task(Box::new(|| {}));
        assert_eq!(res, Err(EventLoopError::ShuttingDown));
        // Clean up join.
        let _ = mgr.shutdown_and_join(id);
    }

    // Test 11: timer heap lazy cancellation frees root-position entries.
    #[test]
    fn t19_6_timer_heap_lazy_cancel() {
        let mut heap = TimerHeap::new();
        let now = Instant::now();
        let c1 = Arc::new(AtomicBool::new(false));
        let c2 = Arc::new(AtomicBool::new(false));
        heap.push_bounded(ScheduledTimer {
            deadline: now + Duration::from_millis(10),
            seq: 0,
            task: Some(Box::new(|| {})),
            cancelled: c1.clone(),
            timer_id: 1,
        })
        .map_err(|_| "cap hit")
        .unwrap();
        heap.push_bounded(ScheduledTimer {
            deadline: now + Duration::from_millis(20),
            seq: 1,
            task: Some(Box::new(|| {})),
            cancelled: c2.clone(),
            timer_id: 2,
        })
        .map_err(|_| "cap hit")
        .unwrap();
        assert_eq!(heap.len(), 2);
        // Cancel the first. peek_deadline should now skip it and return c2's.
        c1.store(true, Ordering::Release);
        let d = heap.peek_deadline().expect("has peek");
        assert_eq!(d, now + Duration::from_millis(20));
        // The cancelled root should have been popped; only c2 remains.
        assert_eq!(heap.len(), 1);
    }

    // Test 12: MAX_PENDING_TASKS cap bounces with TaskQueueFull.
    #[test]
    fn t19_6_task_queue_cap_enforced() {
        // Construct an EventLoop directly (no spawn) so we can stuff it
        // at will without racing a draining loop.
        let el = EventLoop::new(EventLoopId(99_999), "cap-test");
        for i in 0..MAX_PENDING_TASKS {
            let res = el.schedule_task(Box::new(|| {}));
            assert!(res.is_ok(), "push {} must succeed", i);
        }
        let res = el.schedule_task(Box::new(|| {}));
        assert_eq!(res, Err(EventLoopError::TaskQueueFull));
    }

    // Test 13: MAX_SCHEDULED_TIMERS cap bounces with TimerHeapFull.
    #[test]
    fn t19_6_timer_heap_cap_enforced() {
        let el = EventLoop::new(EventLoopId(99_998), "timer-cap-test");
        let d = Instant::now() + Duration::from_secs(3600);
        for _ in 0..MAX_SCHEDULED_TIMERS {
            let res = el.schedule_timer(d, Box::new(|| {}));
            assert!(res.is_ok());
        }
        let res = el.schedule_timer(d, Box::new(|| {}));
        assert_eq!(res.err(), Some(EventLoopError::TimerHeapFull));
    }

    // Test 14: wake deduplication — a second wake before park consumes the
    // first is a no-op at the wakes_issued counter.
    #[test]
    fn t19_6_wake_dedupes_concurrent_calls() {
        let wk = WakeableCondvar::new();
        wk.wake();
        wk.wake(); // second wake is a dedupe
        wk.wake(); // third too
                   // Now park should observe the wake on the fast path.
        let woken = wk.park(Duration::from_millis(50));
        assert!(woken);
        // After consumption, the next park should timeout.
        let t0 = Instant::now();
        let woken2 = wk.park(Duration::from_millis(50));
        assert!(!woken2);
        assert!(t0.elapsed() >= Duration::from_millis(40));
    }

    // Test 15: shutdown_and_join drains pending tasks before the loop exits.
    #[test]
    fn t19_6_shutdown_drains_remaining_tasks() {
        let mgr = event_loop_manager();
        let el = mgr.spawn("t19-6-test-15").expect("spawn");
        let id = el.id;
        // Queue a bunch of tasks WITHOUT waiting for them to drain.
        let c = Arc::new(AtomicUsize::new(0));
        for _ in 0..20 {
            let c2 = c.clone();
            el.schedule_task(Box::new(move || {
                c2.fetch_add(1, Ordering::Relaxed);
            }))
            .expect("schedule");
        }
        // Immediately shut down; the loop must still drain them per the
        // module contract.
        mgr.shutdown_and_join(id).expect("shutdown");
        assert_eq!(c.load(Ordering::Acquire), 20);
    }

    // Test 16: EventLoopAffinity round-trips an id.
    #[test]
    fn t19_6_event_loop_affinity_round_trip() {
        let id = EventLoopId::from_raw(7).unwrap();
        let aff = EventLoopAffinity::new(id);
        assert_eq!(aff.loop_id, id);
        let copy = aff;
        assert_eq!(copy, aff);
    }

    // Test 17: current_event_loop() is None on a non-loop thread and
    // Some on a loop thread.
    #[test]
    fn t19_6_current_event_loop_tls_tracking() {
        assert!(current_event_loop().is_none());
        with_loop("t19-6-test-17", |el| {
            let found = Arc::new(Mutex::new(false));
            let f2 = found.clone();
            let expected_id = el.id;
            el.schedule_task(Box::new(move || {
                if let Some(curr) = current_event_loop() {
                    if curr.id == expected_id {
                        *f2.lock().unwrap_or_else(|e| e.into_inner()) = true;
                    }
                }
            }))
            .expect("schedule");
            for _ in 0..250 {
                if *found.lock().unwrap_or_else(|e| e.into_inner()) {
                    break;
                }
                thread::sleep(Duration::from_millis(2));
            }
            assert!(*found.lock().unwrap_or_else(|e| e.into_inner()));
        });
    }
}
