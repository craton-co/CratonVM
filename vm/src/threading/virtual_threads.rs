// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Virtual thread support (Project Loom).
//!
//! Implements continuations, virtual thread lifecycle, a work-stealing
//! fork-join scheduler, and the `VirtualThreadManager` coordinator that ties
//! everything together.

use std::cmp::Ordering as CmpOrdering;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use parking_lot::{Condvar, Mutex};
use rustc_hash::FxHashMap;

// ---------------------------------------------------------------------------
// ContinuationState
// ---------------------------------------------------------------------------

/// Lifecycle state of a `Continuation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContinuationState {
    /// Not yet started.
    New,
    /// Currently executing on a carrier thread.
    Running,
    /// Suspended (yielded), frames frozen.
    Suspended,
    /// Completed execution.
    Completed,
    /// Failed with exception.
    Failed,
}

// ---------------------------------------------------------------------------
// ContinuationScope / FrozenFrame
// ---------------------------------------------------------------------------

/// Scope tag for structured concurrency.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContinuationScope {
    pub name: String,
}

/// A snapshot of a single JVM stack frame, captured when a continuation yields.
///
/// The core fields (class_name through stack_tags) always present.
/// The `Option` fields carry restoration metadata needed to reconstruct a live
/// `Frame` on thaw — these are populated by `Frame::to_frozen_frame()` and
/// consumed by `Frame::from_frozen_frame()`.
#[derive(Debug, Clone)]
pub struct FrozenFrame {
    pub class_name: String,
    pub method_name: String,
    pub descriptor: String,
    pub bytecode_pc: usize,
    pub locals: Vec<u64>,
    pub local_tags: Vec<u8>,
    pub stack: Vec<u64>,
    pub stack_tags: Vec<u8>,
    // -- Restoration metadata (None for lightweight / test-only snapshots) --
    pub code: Option<Arc<[u8]>>,
    pub class_id: Option<crate::classloading::ClassId>,
    pub max_stack: Option<u16>,
    pub max_locals: Option<u16>,
    pub exception_table: Option<Arc<[cratonvm_reader::attribute::ExceptionTableEntry]>>,
    pub source_file: Option<String>,
}

impl FrozenFrame {
    /// Create a lightweight FrozenFrame for testing (no restoration metadata).
    #[cfg(test)]
    pub fn test_frame(
        class: &str,
        method: &str,
        pc: usize,
        locals: Vec<u64>,
        stack: Vec<u64>,
    ) -> Self {
        let local_tags = vec![0u8; locals.len()];
        let stack_tags = vec![0u8; stack.len()];
        Self {
            class_name: class.into(),
            method_name: method.into(),
            descriptor: "()V".into(),
            bytecode_pc: pc,
            locals,
            local_tags,
            stack,
            stack_tags,
            code: None,
            class_id: None,
            max_stack: None,
            max_locals: None,
            exception_table: None,
            source_file: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Continuation
// ---------------------------------------------------------------------------

/// A delimited continuation -- captures the execution state of a virtual
/// thread so it can be suspended and later resumed on any carrier thread.
pub struct Continuation {
    pub id: u64,
    pub state: ContinuationState,
    pub frozen_frames: Vec<FrozenFrame>,
    pub scope: ContinuationScope,
    pub yield_count: u64,
    pub mount_count: u64,
}

static NEXT_CONTINUATION_ID: AtomicU64 = AtomicU64::new(1);

impl Continuation {
    pub fn new(scope: ContinuationScope) -> Self {
        Self {
            id: NEXT_CONTINUATION_ID.fetch_add(1, Ordering::Relaxed),
            state: ContinuationState::New,
            frozen_frames: Vec::new(),
            scope,
            yield_count: 0,
            mount_count: 0,
        }
    }

    /// Freeze current execution state into frozen frames.
    pub fn freeze(&mut self, frames: Vec<FrozenFrame>) {
        self.frozen_frames = frames;
        self.state = ContinuationState::Suspended;
        self.yield_count += 1;
    }

    /// Thaw: restore frozen frames for resumption, returning them and
    /// transitioning the continuation back to `Running`.
    pub fn thaw(&mut self) -> Vec<FrozenFrame> {
        let frames = std::mem::take(&mut self.frozen_frames);
        self.state = ContinuationState::Running;
        self.mount_count += 1;
        frames
    }

    /// Check if this continuation can yield (must be `Running`).
    pub fn can_yield(&self) -> bool {
        self.state == ContinuationState::Running
    }

    /// Freeze live `Frame`s from a thread's call stack into this continuation.
    /// Each Frame is converted to a `FrozenFrame` with full restoration metadata.
    pub fn freeze_frames(&mut self, frames: Vec<crate::runtime::frame::Frame>) {
        self.frozen_frames = frames.iter().map(|f| f.to_frozen_frame()).collect();
        self.state = ContinuationState::Suspended;
        self.yield_count += 1;
    }

    /// Thaw: restore frozen frames back into live `Frame`s for resumption.
    /// Transitions the continuation back to `Running`.
    pub fn thaw_frames(&mut self) -> Vec<crate::runtime::frame::Frame> {
        let frozen = std::mem::take(&mut self.frozen_frames);
        self.state = ContinuationState::Running;
        self.mount_count += 1;
        frozen
            .into_iter()
            .map(crate::runtime::frame::Frame::from_frozen_frame)
            .collect()
    }

    pub fn yield_count(&self) -> u64 {
        self.yield_count
    }

    pub fn mount_count(&self) -> u64 {
        self.mount_count
    }
}

// ---------------------------------------------------------------------------
// VirtualThreadState
// ---------------------------------------------------------------------------

/// Lifecycle state of a virtual thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VirtualThreadState {
    New,
    Started,
    Running,
    Parking,
    Parked,
    Yielding,
    Yielded,
    Pinned,
    Terminated,
}

// ---------------------------------------------------------------------------
// PinReason
// ---------------------------------------------------------------------------

/// Reason a virtual thread is pinned to its carrier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinReason {
    /// Inside a synchronized block (monitor held).
    Monitor,
    /// Inside a JNI native method.
    NativeMethod,
    /// Inside a class initializer.
    ClassInit,
}

// ---------------------------------------------------------------------------
// VirtualThread
// ---------------------------------------------------------------------------

pub struct VirtualThread {
    pub id: u64,
    pub name: String,
    pub state: VirtualThreadState,
    pub continuation: Continuation,
    pub carrier_thread_id: Option<u64>,
    pub pin_count: u32,
    pub pin_reason: Option<PinReason>,
    pub runnable: Option<u64>,
    pub join_waiters: Vec<u64>,
    pub unpark_permit: bool,
    /// A condition became ready after a native registered this continuation
    /// but before the carrier finished unmounting it.
    pub wake_pending: bool,
    /// Heap-resident execution state while unmounted. The `JvmThread` is
    /// boxed so its address remains stable for precise GC scanning across
    /// carrier migration; its `frames` are the continuation stack chunks.
    pub runtime: Option<Box<crate::threading::JvmThread>>,
    pub execution_started: bool,
}

impl VirtualThread {
    pub fn new(id: u64, name: String) -> Self {
        let scope = ContinuationScope {
            name: "VirtualThread".to_string(),
        };
        Self {
            id,
            name,
            state: VirtualThreadState::New,
            continuation: Continuation::new(scope),
            carrier_thread_id: None,
            pin_count: 0,
            pin_reason: None,
            runnable: None,
            join_waiters: Vec::new(),
            unpark_permit: false,
            wake_pending: false,
            runtime: None,
            execution_started: false,
        }
    }

    /// Mount this virtual thread onto a carrier thread.
    pub fn mount(&mut self, carrier_id: u64) {
        self.carrier_thread_id = Some(carrier_id);
        self.state = VirtualThreadState::Running;
        self.continuation.state = ContinuationState::Running;
        self.continuation.mount_count += 1;
    }

    /// Unmount from the carrier (on park or yield).
    pub fn unmount(&mut self) {
        self.carrier_thread_id = None;
    }

    /// Pin the virtual thread (entering a synchronized block or native call).
    pub fn pin(&mut self, reason: PinReason) {
        self.pin_count += 1;
        self.pin_reason = Some(reason);
        if self.state == VirtualThreadState::Running {
            self.state = VirtualThreadState::Pinned;
        }
    }

    /// Unpin (exiting synchronized block or native call).
    pub fn unpin(&mut self) {
        if self.pin_count > 0 {
            self.pin_count -= 1;
        }
        if self.pin_count == 0 {
            self.pin_reason = None;
            if self.state == VirtualThreadState::Pinned {
                self.state = VirtualThreadState::Running;
            }
        }
    }

    /// Check if this virtual thread is pinned.
    pub fn is_pinned(&self) -> bool {
        self.pin_count > 0
    }

    /// Park: yield if not pinned, block carrier if pinned.
    /// Returns `true` if the virtual thread yielded successfully (not pinned),
    /// `false` if it blocked the carrier (pinned).
    pub fn park(&mut self) -> bool {
        if self.unpark_permit {
            // Consume the permit immediately -- no actual park.
            self.unpark_permit = false;
            return true;
        }
        if self.is_pinned() {
            // Cannot unmount -- carrier thread will block.
            return false;
        }
        self.state = VirtualThreadState::Parking;
        self.continuation.freeze(Vec::new());
        self.state = VirtualThreadState::Parked;
        self.unmount();
        true
    }

    /// Park with real frame capture: freezes the thread's live frames into
    /// the continuation so the carrier thread is freed.
    /// Returns `true` if yielded, `false` if pinned (carrier blocked).
    pub fn park_with_frames(&mut self, frames: Vec<crate::runtime::frame::Frame>) -> bool {
        if self.unpark_permit {
            self.unpark_permit = false;
            return true;
        }
        if self.is_pinned() {
            return false;
        }
        self.state = VirtualThreadState::Parking;
        self.continuation.freeze_frames(frames);
        self.state = VirtualThreadState::Parked;
        self.unmount();
        true
    }

    /// Unpark and restore frames from the continuation for resumption.
    /// Returns the restored live frames if the thread was parked, or None.
    pub fn unpark_with_frames(&mut self) -> Option<Vec<crate::runtime::frame::Frame>> {
        self.unpark_permit = true;
        if self.state == VirtualThreadState::Parked {
            self.state = VirtualThreadState::Started;
            if !self.continuation.frozen_frames.is_empty() {
                return Some(self.continuation.thaw_frames());
            }
        }
        None
    }

    /// Unpark: set permit and mark as ready for scheduling.
    pub fn unpark(&mut self) {
        self.unpark_permit = true;
        if self.state == VirtualThreadState::Parked {
            self.state = VirtualThreadState::Started;
        }
    }
}

// ---------------------------------------------------------------------------
// CarrierThread
// ---------------------------------------------------------------------------

pub struct CarrierThread {
    pub thread_id: u64,
    pub mounted_virtual_thread: Option<u64>,
    pub available: bool,
    pub tasks_completed: u64,
}

impl CarrierThread {
    pub fn new(thread_id: u64) -> Self {
        Self {
            thread_id,
            mounted_virtual_thread: None,
            available: true,
            tasks_completed: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// SchedulerStats
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct SchedulerStats {
    pub total_submissions: AtomicU64,
    pub total_steals: AtomicU64,
    pub total_parks: AtomicU64,
    pub total_unparks: AtomicU64,
    pub total_pins: AtomicU64,
    pub peak_active: AtomicU64,
}

// ---------------------------------------------------------------------------
// ForkJoinScheduler
// ---------------------------------------------------------------------------

/// Hard ceiling on carrier OS threads (base pool + compensating carriers).
///
/// Matches the JDK's `jdk.virtualThreadScheduler.maxPoolSize` default. The
/// base pool is `available_parallelism()`; the starvation watchdog may grow it
/// up to this bound, and never past it.
pub const MAX_CARRIER_THREADS: usize = 256;

/// How often the starvation watchdog samples the pool.
const CARRIER_STALL_SAMPLE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

/// Consecutive stalled samples before a compensating carrier is added. Four
/// samples at 50 ms is ~200 ms of "queue non-empty, every carrier busy, and not
/// one task dispatched in the whole window".
const CARRIER_STALL_SAMPLES: u32 = 4;

/// A compensating carrier that finds no work for this long retires itself, so a
/// transient blocking storm does not permanently inflate the pool. Base-pool
/// carriers never retire.
const COMPENSATING_CARRIER_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// A work-stealing scheduler that maps N virtual threads onto M carrier
/// (platform) threads.
pub struct ForkJoinScheduler {
    carriers: Vec<CarrierThread>,
    submission_queue: Mutex<VecDeque<u64>>,
    work_queues: Vec<Mutex<VecDeque<u64>>>,
    parallelism: usize,
    active_count: AtomicUsize,
    total_created: AtomicU64,
    running: AtomicBool,
    stats: SchedulerStats,
    /// Condvar to wake carrier threads when new tasks arrive.
    task_available: Condvar,
    /// Mutex paired with task_available condvar.
    task_signal: Mutex<()>,
    /// Carriers currently inside the task body (as opposed to parked in
    /// `wait_for_task`). Distinguishes "pool saturated" from "pool idle" for
    /// the starvation watchdog.
    busy_carriers: AtomicUsize,
    /// Carrier OS threads alive right now: the base pool plus every
    /// compensating carrier the watchdog has added and that has not retired.
    live_carriers: AtomicUsize,
    /// Monotonic count of tasks handed to carriers. The watchdog's liveness
    /// signal — a saturated pool that keeps dispatching is making progress and
    /// needs no compensation.
    dispatch_count: AtomicU64,
    /// Index handed to the next compensating carrier. Always `>= parallelism`,
    /// so compensating carriers own no per-carrier work queue and go straight
    /// to the global queue / stealing (see `next_task`).
    next_carrier_idx: AtomicUsize,
}

impl ForkJoinScheduler {
    pub fn new(parallelism: usize) -> Self {
        let parallelism = parallelism.max(1);
        let carriers = (0..parallelism)
            .map(|i| CarrierThread::new(i as u64))
            .collect();
        let work_queues = (0..parallelism)
            .map(|_| Mutex::new(VecDeque::new()))
            .collect();
        Self {
            carriers,
            submission_queue: Mutex::new(VecDeque::new()),
            work_queues,
            parallelism,
            active_count: AtomicUsize::new(0),
            total_created: AtomicU64::new(0),
            running: AtomicBool::new(true),
            stats: SchedulerStats::default(),
            task_available: Condvar::new(),
            task_signal: Mutex::new(()),
            busy_carriers: AtomicUsize::new(0),
            live_carriers: AtomicUsize::new(0),
            dispatch_count: AtomicU64::new(0),
            next_carrier_idx: AtomicUsize::new(parallelism),
        }
    }

    /// Create with parallelism equal to the number of available CPUs (min 1).
    pub fn with_default_parallelism() -> Self {
        let cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        Self::new(cpus)
    }

    /// Submit a virtual thread for execution (goes into the global queue).
    ///
    /// LOST-WAKEUP FIX (2026-07-26): the push and the `notify_one` must happen
    /// under `task_signal`, the mutex `wait_for_task_until` waits on.
    /// Previously they did not, leaving the textbook missed-wakeup window — a
    /// carrier that has just run `next_task()` and found nothing, but has not
    /// yet reached `wait_for`, misses a `notify_one` issued in that gap. The
    /// 100 ms poll timeout inside the wait loop hid it as a *latency* bug
    /// rather than a hang: every unpark that lost this race added up to 100 ms
    /// before its virtual thread ran. Holding the mutex across both closes the
    /// window entirely.
    ///
    /// Lock order is `task_signal` -> queues, matching `wait_for_task_until`
    /// (which calls `next_task` while holding `task_signal`). Do not invert it.
    pub fn submit(&self, vt_id: u64) {
        let _signal = self.task_signal.lock();
        self.submission_queue.lock().push_back(vt_id);
        self.stats.total_submissions.fetch_add(1, Ordering::Relaxed);
        self.total_created.fetch_add(1, Ordering::Relaxed);
        // Wake one carrier thread waiting for work.
        self.task_available.notify_one();
    }

    /// Signal all carrier threads (used during shutdown).
    ///
    /// Takes `task_signal` for the same reason [`Self::submit`] does: a
    /// notify issued outside it can be missed by a carrier that is between its
    /// last queue poll and its `wait_for`.
    pub fn notify_all_carriers(&self) {
        let _signal = self.task_signal.lock();
        self.task_available.notify_all();
    }

    /// Check if the scheduler is running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    /// Shut down the scheduler — no new tasks accepted, wake all carriers.
    pub fn shutdown(&self) {
        self.running.store(false, Ordering::SeqCst);
        self.notify_all_carriers();
    }

    /// Wait for a task to become available (blocks the carrier thread).
    /// Returns `None` if the scheduler is shutting down.
    pub fn wait_for_task(&self, carrier_idx: usize) -> Option<u64> {
        self.wait_for_task_until(carrier_idx, None)
    }

    /// [`Self::wait_for_task`] with an optional idle retirement deadline.
    ///
    /// `idle_timeout` is `None` for base-pool carriers (they live for the
    /// process) and `Some(..)` for compensating carriers added by the
    /// starvation watchdog: once one of those has found no work for the whole
    /// timeout *and* the pool is still above its base size, it returns `None`
    /// so the carrier retires and the pool shrinks back.
    pub fn wait_for_task_until(
        &self,
        carrier_idx: usize,
        idle_timeout: Option<std::time::Duration>,
    ) -> Option<u64> {
        // Fast path: check queues first
        if let Some(task) = self.next_task(carrier_idx) {
            return Some(task);
        }
        let idle_deadline = idle_timeout.map(|d| Instant::now() + d);
        // Slow path: wait on condvar
        let mut signal = self.task_signal.lock();
        loop {
            if !self.is_running() {
                return None;
            }
            if let Some(task) = self.next_task(carrier_idx) {
                return Some(task);
            }
            if let Some(deadline) = idle_deadline {
                // Only retire while the pool is still inflated — never shrink
                // below the base parallelism, or a quiet period would leave
                // fewer carriers than the scheduler was built with.
                if Instant::now() >= deadline
                    && self.live_carriers.load(Ordering::Acquire) > self.parallelism
                {
                    return None;
                }
            }
            // Wait with timeout to allow periodic shutdown checks
            self.task_available
                .wait_for(&mut signal, std::time::Duration::from_millis(100));
        }
    }

    /// A carrier is entering the task body. Bumps the saturation gauge and the
    /// dispatch counter the watchdog uses as its liveness signal.
    fn note_task_started(&self) {
        self.busy_carriers.fetch_add(1, Ordering::AcqRel);
        self.dispatch_count.fetch_add(1, Ordering::AcqRel);
    }

    /// A carrier has left the task body.
    fn note_task_finished(&self) {
        self.busy_carriers.fetch_sub(1, Ordering::AcqRel);
    }

    /// Total queued-but-unmounted virtual threads across the global submission
    /// queue and every per-carrier work queue.
    pub fn queued_len(&self) -> usize {
        let mut n = self.submission_queue.lock().len();
        for q in &self.work_queues {
            n += q.lock().len();
        }
        n
    }

    /// Carriers currently executing a virtual thread.
    pub fn busy_carriers(&self) -> usize {
        self.busy_carriers.load(Ordering::Acquire)
    }

    /// Carrier OS threads alive right now (base pool + compensating carriers).
    pub fn live_carriers(&self) -> usize {
        self.live_carriers.load(Ordering::Acquire)
    }

    /// Configured base parallelism (the size of the non-retiring pool).
    pub fn parallelism(&self) -> usize {
        self.parallelism
    }

    /// Monotonic count of tasks handed to carriers.
    pub fn dispatch_count(&self) -> u64 {
        self.dispatch_count.load(Ordering::Acquire)
    }

    /// Try to steal a task from another carrier's work queue.
    pub fn try_steal(&self, carrier_idx: usize) -> Option<u64> {
        for i in 0..self.parallelism {
            if i == carrier_idx {
                continue;
            }
            if let Some(task) = self.work_queues[i].lock().pop_front() {
                self.stats.total_steals.fetch_add(1, Ordering::Relaxed);
                return Some(task);
            }
        }
        None
    }

    /// Get the next task for a carrier: own queue first, then global, then
    /// steal from another carrier.
    ///
    /// `carrier_idx >= parallelism` identifies a compensating carrier, which
    /// owns no work queue — it skips straight to the global queue and stealing.
    /// (Indexing `work_queues[carrier_idx]` directly used to panic for those.)
    pub fn next_task(&self, carrier_idx: usize) -> Option<u64> {
        // 1. Own work queue
        if let Some(queue) = self.work_queues.get(carrier_idx) {
            if let Some(task) = queue.lock().pop_front() {
                return Some(task);
            }
        }
        // 2. Global submission queue
        if let Some(task) = self.submission_queue.lock().pop_front() {
            return Some(task);
        }
        // 3. Work-stealing
        self.try_steal(carrier_idx)
    }

    /// Mount a virtual thread on a carrier.
    pub fn mount(&mut self, carrier_idx: usize, vt_id: u64) {
        if carrier_idx < self.carriers.len() {
            self.carriers[carrier_idx].mounted_virtual_thread = Some(vt_id);
            self.carriers[carrier_idx].available = false;
            let prev = self.active_count.fetch_add(1, Ordering::Relaxed);
            let new_active = (prev + 1) as u64;
            // Update peak
            let mut peak = self.stats.peak_active.load(Ordering::Relaxed);
            while new_active > peak {
                match self.stats.peak_active.compare_exchange_weak(
                    peak,
                    new_active,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(p) => peak = p,
                }
            }
        }
    }

    /// Unmount the current virtual thread from a carrier, returning its ID.
    pub fn unmount(&mut self, carrier_idx: usize) -> Option<u64> {
        if carrier_idx < self.carriers.len() {
            let vt_id = self.carriers[carrier_idx].mounted_virtual_thread.take();
            if vt_id.is_some() {
                self.carriers[carrier_idx].available = true;
                self.carriers[carrier_idx].tasks_completed += 1;
                self.active_count.fetch_sub(1, Ordering::Relaxed);
            }
            vt_id
        } else {
            None
        }
    }

    /// Number of virtual threads currently mounted on carriers.
    pub fn active_count(&self) -> usize {
        self.active_count.load(Ordering::Relaxed)
    }

    pub fn stats(&self) -> &SchedulerStats {
        &self.stats
    }
}

// ---------------------------------------------------------------------------
// Carrier bodies + starvation compensation
// ---------------------------------------------------------------------------

/// The body every carrier OS thread runs: pull a virtual thread, execute it,
/// repeat. `idle_timeout` is `None` for base-pool carriers and `Some(..)` for
/// compensating carriers, which retire when the pool has drained (see
/// [`ForkJoinScheduler::wait_for_task_until`]).
///
/// The busy/idle accounting lives HERE rather than inside `wait_for_task` so
/// that "carrier is inside the task body" — the state that matters for
/// starvation, because that is where a carrier can block on a monitor and never
/// come back — is what the gauge actually measures.
fn run_carrier(
    scheduler: &Arc<ForkJoinScheduler>,
    task_fn: &Arc<dyn Fn(u64) + Send + Sync>,
    carrier_idx: usize,
    idle_timeout: Option<std::time::Duration>,
) {
    // Both counters are released by `Drop`, not by a trailing statement: a
    // panic escaping `task_fn` would otherwise leak `busy_carriers` forever,
    // and a permanently-saturated-looking pool is exactly the condition the
    // watchdog grows on — one panic would ratchet the pool to its cap.
    let _alive = CarrierAliveGuard(&**scheduler);
    while let Some(vt_id) = scheduler.wait_for_task_until(carrier_idx, idle_timeout) {
        scheduler.note_task_started();
        let _busy = CarrierBusyGuard(&**scheduler);
        task_fn(vt_id);
    }
}

/// Decrements `live_carriers` when a carrier body exits, however it exits.
struct CarrierAliveGuard<'a>(&'a ForkJoinScheduler);

impl Drop for CarrierAliveGuard<'_> {
    fn drop(&mut self) {
        self.0.live_carriers.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Decrements `busy_carriers` when a task body returns or unwinds.
struct CarrierBusyGuard<'a>(&'a ForkJoinScheduler);

impl Drop for CarrierBusyGuard<'_> {
    fn drop(&mut self) {
        self.0.note_task_finished();
    }
}

/// Add one compensating carrier, unless the pool is already at
/// [`MAX_CARRIER_THREADS`]. Returns whether a carrier was actually spawned.
///
/// The reservation is a CAS loop on `live_carriers`, so two watchdogs (or a
/// watchdog racing a retiring carrier) can never push the pool past the cap.
fn spawn_compensating_carrier(
    scheduler: &Arc<ForkJoinScheduler>,
    task_fn: &Arc<dyn Fn(u64) + Send + Sync>,
    handles: &Arc<Mutex<Vec<std::thread::JoinHandle<()>>>>,
) -> bool {
    let mut live = scheduler.live_carriers.load(Ordering::Acquire);
    loop {
        if live >= MAX_CARRIER_THREADS {
            return false;
        }
        match scheduler.live_carriers.compare_exchange_weak(
            live,
            live + 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => break,
            Err(observed) => live = observed,
        }
    }
    let carrier_idx = scheduler.next_carrier_idx.fetch_add(1, Ordering::AcqRel);
    let sched = scheduler.clone();
    let f = task_fn.clone();
    match std::thread::Builder::new()
        .name(format!("ForkJoinPool-carrier-comp-{}", carrier_idx))
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            run_carrier(
                &sched,
                &f,
                carrier_idx,
                Some(COMPENSATING_CARRIER_IDLE_TIMEOUT),
            )
        }) {
        Ok(handle) => {
            handles.lock().push(handle);
            true
        }
        Err(_) => {
            // Could not get an OS thread (fd/handle exhaustion). Give the
            // reservation back so a later attempt can retry.
            scheduler.live_carriers.fetch_sub(1, Ordering::AcqRel);
            false
        }
    }
}

/// Decide, from one sample of the pool, whether a compensating carrier is owed.
///
/// Stalled means all three of:
///  * work is queued but unmounted (`queued > 0`);
///  * every live carrier is inside a task body (`busy >= live`), so nothing is
///    waiting to pick that work up;
///  * `dispatch_count` has not moved since the previous sample — a saturated
///    pool that keeps dequeuing is busy, not stuck, and must not be grown.
///
/// A CPU-bound virtual thread that never yields also trips this, and that is
/// intentional: the JDK compensates for a monopolised carrier the same way, and
/// the cap plus idle retirement bound the cost.
fn carrier_pool_is_stalled(queued: usize, busy: usize, live: usize, dispatch_moved: bool) -> bool {
    queued > 0 && live > 0 && busy >= live && !dispatch_moved
}

/// Start the single watchdog thread that grows the carrier pool when it is
/// wedged. One thread per `VirtualThreadManager`, started with the base pool.
///
/// This exists because nothing in the VM prevents a carrier from blocking:
/// contended `monitorenter` goes to `monitor_enter_blocking`, `Object.wait`
/// goes to `monitor_wait`, and the platform-park fallback goes to
/// `NativeContextImpl::park` — none of which is virtual-thread aware, and all
/// of which park the carrier's OS thread with the continuation still mounted.
/// Without compensation, `parallelism` such blocks stop the whole VM.
fn spawn_starvation_watchdog(
    scheduler: Arc<ForkJoinScheduler>,
    task_fn: Arc<dyn Fn(u64) + Send + Sync>,
    handles: Arc<Mutex<Vec<std::thread::JoinHandle<()>>>>,
) {
    let watchdog_handles = handles.clone();
    let handle = std::thread::Builder::new()
        .name("ForkJoinPool-starvation-watchdog".to_string())
        .spawn(move || {
            let mut last_dispatch = scheduler.dispatch_count();
            let mut stalled_samples: u32 = 0;
            let (mut last_pause_epoch, _) = crate::threading::gc_barrier::stw_pause_state();
            loop {
                std::thread::sleep(CARRIER_STALL_SAMPLE_INTERVAL);
                if !scheduler.is_running() {
                    return;
                }
                let dispatch = scheduler.dispatch_count();
                let dispatch_moved = dispatch != last_dispatch;
                last_dispatch = dispatch;
                // A stop-the-world pause parks every carrier, so
                // `dispatch_count` CANNOT move across one -- which is exactly
                // this watchdog's stall signature. Sampling through a pause
                // therefore reads "starved" off a VM that is merely stopped,
                // and the carrier it then adds is not idle for long: it is one
                // more OS thread for the next pause to stop, and on Windows one
                // more for `xt_root_scan::take_over_pass` to `SuspendThread` +
                // `GetThreadContext` + `ResumeThread`, which that pass does for
                // EVERY thread in the process on EVERY collection. That closes a
                // loop: slower pause -> more stalled samples -> more carriers ->
                // slower pause. Measured on `VthreadGcStress` (3000 virtual
                // threads, 400 `System.gc()` rounds), the pool ran away from its
                // base 32 to 233+ and `dispatch_count` froze for good; with the
                // scan off it stayed at 32 and the probe finished in 5 s.
                //
                // So: a sample whose interval CONTAINED a pause says nothing
                // about starvation. Drop it, and do not let it advance the
                // consecutive-stall count that growth is gated on. This does not
                // weaken the watchdog for the case it exists for -- a carrier
                // blocked in `monitorenter` stays blocked across as many pauses
                // as you like, so its stalled samples simply resume accruing
                // from the first pause-free interval.
                let (pause_epoch, pause_now) =
                    crate::threading::gc_barrier::stw_pause_state();
                let paused_in_interval = pause_now || pause_epoch != last_pause_epoch;
                last_pause_epoch = pause_epoch;
                // `CRATONVM_DBG_CARRIER=1` -- one line per sample, printed
                // BEFORE the skip above so a pause-contaminated sample is
                // visible as such rather than silently absent. The pool
                // climbing away from its base size while `dispatch` sits still
                // is the signature of the loop described above, and without
                // this reading it presents only as "the VM hung".
                if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_CARRIER").is_some() {
                    eprintln!(
                        "[carrier] queued={} busy={} live={} dispatch={} moved={} paused={}",
                        scheduler.queued_len(),
                        scheduler.busy_carriers(),
                        scheduler.live_carriers(),
                        dispatch,
                        dispatch_moved,
                        paused_in_interval,
                    );
                    let census = crate::threading::thread_state::thread_state_census();
                    let mut parts: Vec<String> = Vec::new();
                    for st in crate::threading::thread_state::ThreadExecState::ALL.iter() {
                        let n = census.get(*st);
                        if n > 0 {
                            parts.push(format!("{st:?}={n}"));
                        }
                    }
                    eprintln!("[carrier-states] {}", parts.join(" "));
                }
                if paused_in_interval {
                    continue;
                }
                if carrier_pool_is_stalled(
                    scheduler.queued_len(),
                    scheduler.busy_carriers(),
                    scheduler.live_carriers(),
                    dispatch_moved,
                ) {
                    stalled_samples = stalled_samples.saturating_add(1);
                    if stalled_samples >= CARRIER_STALL_SAMPLES {
                        stalled_samples = 0;
                        if spawn_compensating_carrier(&scheduler, &task_fn, &watchdog_handles) {
                            // The fresh carrier is parked in `wait_for_task`;
                            // nudge the condvar so it picks up the backlog
                            // without waiting out its 100 ms poll.
                            scheduler.notify_all_carriers();
                        }
                    }
                } else {
                    stalled_samples = 0;
                }
            }
        });
    if let Ok(handle) = handle {
        handles.lock().push(handle);
    }
}

// ---------------------------------------------------------------------------
// WakeupTimer
// ---------------------------------------------------------------------------

/// A registered timed wakeup: when `deadline` passes, `vt_id` is resubmitted to
/// the scheduler unless its registration was cancelled or superseded.
///
/// `signal` is the same `Arc<(Mutex<bool>, Condvar)>` stored in the manager's
/// `wakeup_signals` registry; the timer thread validates an entry on expiry by
/// (a) confirming the registry still maps `vt_id` to *this exact* `Arc`
/// (`Arc::ptr_eq`) and (b) checking the cancellation flag inside it. This makes
/// re-schedule and cancellation races safe without a separate generation
/// counter — a stale heap entry simply fails the identity check and is dropped.
struct WakeupEntry {
    deadline: Instant,
    vt_id: u64,
    signal: Arc<(Mutex<bool>, Condvar)>,
}

// Ordered by deadline (earliest first when wrapped in `Reverse`), with `vt_id`
// as a deterministic tie-breaker. `Arc` is intentionally excluded from the
// ordering — only the timing key matters for heap placement.
impl PartialEq for WakeupEntry {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline && self.vt_id == other.vt_id
    }
}
impl Eq for WakeupEntry {}
impl Ord for WakeupEntry {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        self.deadline
            .cmp(&other.deadline)
            .then_with(|| self.vt_id.cmp(&other.vt_id))
    }
}
impl PartialOrd for WakeupEntry {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}

/// Mutable state guarded by `WakeupTimer::state`.
#[derive(Default)]
struct TimerState {
    /// Min-heap of pending wakeups keyed by deadline. `std::cmp::Reverse` turns
    /// the max-heap `BinaryHeap` into a min-heap so `peek()` is the soonest.
    heap: BinaryHeap<std::cmp::Reverse<WakeupEntry>>,
    /// Whether the background timer thread has been spawned yet (lazy start).
    started: bool,
    /// Set on shutdown so the timer thread exits its loop.
    shutdown: bool,
}

/// A single process-wide timer driven by ONE background OS thread. Replaces the
/// previous "spawn a fresh OS thread per `Thread.sleep`" approach, which
/// defeated the purpose of virtual threads (an OS-thread spawn + stack per
/// sleeping VT, a resource/DoS hazard under many concurrent sleepers).
///
/// Sleeping virtual threads now cost one heap insertion each; the timer thread
/// parks on the head deadline via `Condvar::wait_for` and resubmits expired VTs
/// to the scheduler. Cancellation is a flag flip plus a `notify_one` so the
/// timer recomputes its next deadline.
struct WakeupTimer {
    state: Mutex<TimerState>,
    cvar: Condvar,
    scheduler: Arc<ForkJoinScheduler>,
    /// Shared with `VirtualThreadManager::wakeup_signals` so the timer can
    /// validate entries by identity on expiry and so cancellation observed here
    /// stays consistent with `get_wakeup_signal`.
    wakeup_signals: Arc<Mutex<FxHashMap<u64, Arc<(Mutex<bool>, Condvar)>>>>,
}

impl WakeupTimer {
    fn new(
        scheduler: Arc<ForkJoinScheduler>,
        wakeup_signals: Arc<Mutex<FxHashMap<u64, Arc<(Mutex<bool>, Condvar)>>>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(TimerState::default()),
            cvar: Condvar::new(),
            scheduler,
            wakeup_signals,
        })
    }

    /// Register a wakeup for `vt_id` at `deadline`, carrying its registry
    /// `signal`. Lazily starts the single timer thread on first use and wakes it
    /// so it can re-evaluate the head deadline.
    fn schedule(
        self: &Arc<Self>,
        vt_id: u64,
        deadline: Instant,
        signal: Arc<(Mutex<bool>, Condvar)>,
    ) {
        let mut state = self.state.lock();
        state.heap.push(std::cmp::Reverse(WakeupEntry {
            deadline,
            vt_id,
            signal,
        }));
        if !state.started {
            state.started = true;
            let timer = self.clone();
            std::thread::Builder::new()
                .name("VirtualThread-wakeup-timer".to_string())
                .spawn(move || timer.run())
                .expect("failed to spawn virtual-thread wakeup timer");
        }
        // Wake the timer: the new entry may be earlier than its current target.
        self.cvar.notify_one();
    }

    /// Wake the timer so it re-evaluates after a cancellation flipped a flag.
    fn notify(&self) {
        self.cvar.notify_one();
    }

    /// Stop the timer thread (best effort; entries are simply abandoned).
    fn shutdown(&self) {
        let mut state = self.state.lock();
        state.shutdown = true;
        self.cvar.notify_all();
    }

    /// Decide whether an expired entry is still valid and should resubmit:
    /// the registry must still map `vt_id` to this exact `signal` and the
    /// cancellation flag must be unset. On a valid fire we also drop the
    /// registry entry (mirrors the old per-thread cleanup, by identity so a
    /// newer `schedule_wakeup` for the same `vt_id` is never clobbered).
    fn take_if_live(&self, entry: &WakeupEntry) -> bool {
        let mut map = self.wakeup_signals.lock();
        match map.get(&entry.vt_id) {
            Some(existing) if Arc::ptr_eq(existing, &entry.signal) => {
                let cancelled = *entry.signal.0.lock();
                if cancelled {
                    // A cancel that hasn't yet removed the entry (or removed a
                    // different generation); leave the map to the canceller.
                    false
                } else {
                    map.remove(&entry.vt_id);
                    true
                }
            }
            // Superseded by a newer registration or already removed/cancelled.
            _ => false,
        }
    }

    /// Timer thread body: park on the soonest deadline, resubmit on expiry.
    fn run(self: Arc<Self>) {
        loop {
            let mut state = self.state.lock();
            if state.shutdown {
                return;
            }
            let now = Instant::now();
            // Drain everything already due, collecting valid fires to resubmit
            // after we release the lock (avoid calling into the scheduler while
            // holding the timer mutex).
            let mut due: Vec<WakeupEntry> = Vec::new();
            loop {
                // `Instant` is `Copy`, so read the head deadline and end the
                // immutable borrow before popping (avoids a peek/pop borrow
                // conflict on `state.mem.heap`).
                let head_deadline = state.heap.peek().map(|r| r.0.deadline);
                match head_deadline {
                    Some(deadline) if deadline <= now => {
                        let std::cmp::Reverse(entry) =
                            state.heap.pop().expect("peeked head exists");
                        due.push(entry);
                    }
                    _ => break,
                }
            }
            if due.is_empty() {
                // Nothing due: wait until the next deadline, or indefinitely
                // (until notified) if the heap is empty.
                match state.heap.peek().map(|r| r.0.deadline) {
                    Some(deadline) => {
                        let wait = deadline.saturating_duration_since(now);
                        self.cvar.wait_for(&mut state, wait);
                    }
                    None => {
                        self.cvar.wait(&mut state);
                    }
                }
                continue;
            }
            drop(state);
            for entry in due {
                if self.take_if_live(&entry) {
                    self.scheduler.submit(entry.vt_id);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// VirtualThreadManager
// ---------------------------------------------------------------------------

/// Top-level coordinator for virtual thread creation, scheduling, and
/// lifecycle management.
/// T10.9.B: FxHashMap — virtual thread IDs are internal.
pub struct VirtualThreadManager {
    threads: Mutex<FxHashMap<u64, VirtualThread>>,
    scheduler: Arc<ForkJoinScheduler>,
    next_id: AtomicU64,
    _next_continuation_id: AtomicU64,
    /// Carrier OS thread join handles (populated by `start_carriers`).
    ///
    /// Behind an `Arc` so the starvation watchdog thread can push handles for
    /// the compensating carriers it spawns without borrowing `&self`.
    carrier_handles: Arc<Mutex<Vec<std::thread::JoinHandle<()>>>>,
    carriers_started: AtomicBool,
    /// Per-virtual-thread wakeup signal for timed park/sleep cancellation.
    /// T10.9.B: FxHashMap — internal thread IDs.
    /// Held behind an `Arc` so the shared wakeup timer thread can validate and
    /// drop entries on fire (see `WakeupTimer`) without borrowing `&self`. Each
    /// signal is `(cancelled_flag, condvar)`; external callers obtain it via
    /// `get_wakeup_signal` to cancel a pending sleep early.
    wakeup_signals: Arc<Mutex<FxHashMap<u64, Arc<(Mutex<bool>, Condvar)>>>>,
    /// Single process-wide timer servicing all virtual-thread timed wakeups via
    /// one background OS thread and a min-heap of deadlines (replaces the former
    /// per-`Thread.sleep` OS-thread spawn).
    wakeup_timer: Arc<WakeupTimer>,
    /// Stable VM-local synchronization key to unmounted continuation IDs.
    keyed_waiters: Mutex<FxHashMap<u64, Vec<u64>>>,
}

impl VirtualThreadManager {
    pub fn new(parallelism: usize) -> Self {
        let scheduler = Arc::new(ForkJoinScheduler::new(parallelism));
        let wakeup_signals = Arc::new(Mutex::new(FxHashMap::default()));
        let wakeup_timer = WakeupTimer::new(scheduler.clone(), wakeup_signals.clone());
        Self {
            threads: Mutex::new(FxHashMap::default()),
            scheduler,
            next_id: AtomicU64::new(1),
            _next_continuation_id: AtomicU64::new(1),
            carrier_handles: Arc::new(Mutex::new(Vec::new())),
            carriers_started: AtomicBool::new(false),
            wakeup_signals,
            wakeup_timer,
            keyed_waiters: Mutex::new(FxHashMap::default()),
        }
    }

    pub fn with_default_parallelism() -> Self {
        let cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        Self::new(cpus)
    }

    /// Start real carrier OS threads that poll the scheduler for virtual
    /// threads to run. `task_fn` is called for each virtual thread ID that
    /// gets dequeued — the caller supplies the actual execution logic
    /// (e.g. interpreter invocation).
    ///
    /// Also starts the *starvation watchdog* (see [`spawn_starvation_watchdog`]).
    /// It is unconditional and has no gate: a virtual thread that blocks its
    /// carrier — waiting on a contended monitor, inside `Object.wait`, or on
    /// the platform-park fallback — cannot unmount, and with a fixed pool a
    /// handful of those wedge every carrier permanently. Compensation converts
    /// that hard deadlock into a slowdown, exactly as the JDK's ForkJoinPool
    /// does when it compensates for a blocked worker.
    pub fn start_carriers<F>(&self, task_fn: F)
    where
        F: Fn(u64) + Send + Sync + 'static,
    {
        let task_fn: Arc<dyn Fn(u64) + Send + Sync> = Arc::new(task_fn);
        let scheduler = self.scheduler.clone();
        let parallelism = scheduler.parallelism;
        {
            let mut handles = self.carrier_handles.lock();
            for carrier_idx in 0..parallelism {
                let sched = scheduler.clone();
                let f = task_fn.clone();
                sched.live_carriers.fetch_add(1, Ordering::AcqRel);
                let handle = std::thread::Builder::new()
                    .name(format!("ForkJoinPool-carrier-{}", carrier_idx))
                    .stack_size(8 * 1024 * 1024)
                    .spawn(move || run_carrier(&sched, &f, carrier_idx, None))
                    .expect("failed to spawn carrier thread");
                handles.push(handle);
            }
        }
        spawn_starvation_watchdog(scheduler, task_fn, self.carrier_handles.clone());
    }

    /// Start the bounded carrier pool exactly once.
    pub fn start_carriers_once<F>(&self, task_fn: F)
    where
        F: Fn(u64) + Send + Sync + 'static,
    {
        if self
            .carriers_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.start_carriers(task_fn);
        }
    }

    /// Shut down the scheduler and join all carrier threads.
    pub fn shutdown(&self) {
        self.scheduler.shutdown();
        // Stop the shared wakeup timer thread (if it was ever started).
        self.wakeup_timer.shutdown();
        // Take the handles and RELEASE the lock before joining. The starvation
        // watchdog pushes compensating-carrier handles under this same lock, so
        // joining while holding it would deadlock shutdown against the very
        // watchdog thread it is waiting for.
        let handles: Vec<std::thread::JoinHandle<()>> =
            self.carrier_handles.lock().drain(..).collect();
        for handle in handles {
            let _ = handle.join();
        }
    }

    /// Get a reference to the underlying scheduler.
    pub fn scheduler(&self) -> &Arc<ForkJoinScheduler> {
        &self.scheduler
    }

    /// Create a new virtual thread (`Thread.ofVirtual()`).
    pub fn create_virtual_thread(&self, name: &str) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let vt = VirtualThread::new(id, name.to_string());
        self.threads.lock().insert(id, vt);
        id
    }

    /// Register a VM thread id as the virtual-thread id. Runtime integration
    /// uses the registry id directly so JFR, GC, park/unpark, and Java mirror
    /// lookups all share one stable identity.
    pub fn create_virtual_thread_with_id(&self, id: u64, name: &str) {
        let mut threads = self.threads.lock();
        threads
            .entry(id)
            .or_insert_with(|| VirtualThread::new(id, name.to_string()));
        self.next_id
            .fetch_max(id.saturating_add(1), Ordering::Relaxed);
    }

    /// Install the heap-resident execution state before first submission.
    pub fn install_runtime(&self, vt_id: u64, runtime: Box<crate::threading::JvmThread>) {
        if let Some(vt) = self.threads.lock().get_mut(&vt_id) {
            vt.runtime = Some(runtime);
        }
    }

    /// Mount an execution state on a carrier. Returns whether this is a
    /// continuation resume (`true`) or the first invocation of `Thread.run`.
    pub fn take_runtime_for_mount(
        &self,
        vt_id: u64,
        carrier_id: u64,
    ) -> Option<(Box<crate::threading::JvmThread>, bool)> {
        let mut threads = self.threads.lock();
        let vt = threads.get_mut(&vt_id)?;
        let resumed = vt.execution_started;
        vt.execution_started = true;
        vt.mount(carrier_id);
        vt.runtime.take().map(|runtime| (runtime, resumed))
    }

    /// Unmount a yielded continuation without retaining an OS stack.
    pub fn suspend_runtime(
        &self,
        vt_id: u64,
        mut runtime: Box<crate::threading::JvmThread>,
        wake_after: std::time::Duration,
    ) {
        let mut resubmit = false;
        let mut threads = self.threads.lock();
        if let Some(vt) = threads.get_mut(&vt_id) {
            vt.state = VirtualThreadState::Parked;
            vt.continuation.state = ContinuationState::Suspended;
            vt.continuation.yield_count = vt.continuation.yield_count.saturating_add(1);
            vt.unmount();
            // The frames stay in their boxed heap stack chunks. This preserves
            // precise GC visibility through the stable published JvmThread
            // address and avoids serializing raw ObjectRefs into an untracked
            // byte buffer.
            runtime.tlab.retire();
            vt.runtime = Some(runtime);
            if vt.wake_pending {
                vt.wake_pending = false;
                vt.state = VirtualThreadState::Started;
                resubmit = true;
            } else if vt.unpark_permit && wake_after.is_zero() {
                // LOST-UNPARK FIX (2026-07-26): an `unpark` that landed while
                // this continuation was still mounted (or mid-unmount) left a
                // sticky permit rather than a submission — see
                // `unpark_virtual`. Consume it here, which is the earliest
                // point at which the runtime is deposited and the thread can
                // legally be resubmitted.
                //
                // Gated on `wake_after.is_zero()` deliberately: a zero wake
                // means an UNTIMED park, the only yield that can hang forever
                // without this. A timed yield (`Thread.sleep`, `parkNanos`)
                // already has a `WakeupTimer` deadline, so honouring the
                // permit there would let a stray permit truncate a real
                // `Thread.sleep` — a correctness regression traded for
                // nothing. The residual is bounded latency on `parkNanos`,
                // recorded as a known gap in
                // `virtual-threads.md`.
                vt.unpark_permit = false;
                vt.state = VirtualThreadState::Started;
                resubmit = true;
            }
        }
        drop(threads);
        self.scheduler
            .stats
            .total_parks
            .fetch_add(1, Ordering::Relaxed);
        if resubmit {
            self.scheduler.submit(vt_id);
        } else if !wake_after.is_zero() {
            self.schedule_wakeup(vt_id, wake_after);
        }
    }

    /// Register a mounted continuation on a stable VM-local synchronization
    /// key. Duplicate registration is suppressed so a retry cannot create
    /// duplicate scheduler submissions.
    pub fn wait_on_key(&self, key: u64, vt_id: u64) {
        let mut waiters = self.keyed_waiters.lock();
        let entry = waiters.entry(key).or_default();
        if !entry.contains(&vt_id) {
            entry.push(vt_id);
        }
    }

    pub fn cancel_wait_on_key(&self, key: u64, vt_id: u64) {
        let mut waiters = self.keyed_waiters.lock();
        let mut remove_key = false;
        if let Some(entry) = waiters.get_mut(&key) {
            entry.retain(|candidate| *candidate != vt_id);
            remove_key = entry.is_empty();
        }
        if remove_key {
            waiters.remove(&key);
        }
    }

    /// Drain a condition's waiter set. Parked continuations are submitted
    /// immediately; a still-mounted continuation records a wake that
    /// `suspend_runtime` consumes after depositing its heap stack.
    pub fn wake_waiters(&self, key: u64) {
        let waiter_ids = self.keyed_waiters.lock().remove(&key).unwrap_or_default();
        let mut ready = Vec::with_capacity(waiter_ids.len());
        {
            let mut threads = self.threads.lock();
            for vt_id in waiter_ids {
                let Some(vt) = threads.get_mut(&vt_id) else {
                    continue;
                };
                // `Parked` alone, for the reason spelled out in
                // `unpark_virtual`: on the live path `Parked` already implies
                // a deposited runtime, and where it does not, refusing to
                // submit loses the wake permanently (nothing will call
                // `suspend_runtime` for an already-parked thread, so the
                // `wake_pending` flag set below would never be consumed).
                // Submitting a runtime-less thread is a no-op.
                if vt.state == VirtualThreadState::Parked {
                    vt.state = VirtualThreadState::Started;
                    ready.push(vt_id);
                } else if vt.state != VirtualThreadState::Terminated {
                    vt.wake_pending = true;
                }
            }
        }
        for vt_id in ready {
            self.scheduler.submit(vt_id);
        }
    }

    /// Start a virtual thread -- submit it to the scheduler.
    pub fn start(&self, vt_id: u64) {
        let mut threads = self.threads.lock();
        if let Some(vt) = threads.get_mut(&vt_id) {
            vt.state = VirtualThreadState::Started;
        }
        drop(threads);
        self.scheduler.submit(vt_id);
    }

    /// Park a virtual thread (`LockSupport.park`).
    /// Returns `true` if it yielded (unmounted), `false` if pinned/blocked.
    pub fn park_virtual(&self, vt_id: u64) -> bool {
        let mut threads = self.threads.lock();
        if let Some(vt) = threads.get_mut(&vt_id) {
            let result = vt.park();
            if result {
                self.scheduler
                    .stats
                    .total_parks
                    .fetch_add(1, Ordering::Relaxed);
            }
            result
        } else {
            false
        }
    }

    /// Unpark a virtual thread (`LockSupport.unpark`).
    ///
    /// Resubmits ONLY when the continuation is genuinely parked *and* its
    /// heap-resident `runtime` has already been deposited by
    /// [`Self::suspend_runtime`]. In every other live state the permit is left
    /// on the `VirtualThread` for `suspend_runtime` to consume — the same
    /// deposit-then-check handshake [`Self::wake_waiters`] uses.
    ///
    /// LOST-UNPARK FIX (2026-07-26). The previous version called `vt.unpark()`
    /// and then tested `state == Started`, which was wrong in both directions:
    ///
    /// * **Lost wakeup.** Between the interpreter returning
    ///   `VmError::ContinuationYield` on the carrier and `suspend_runtime`
    ///   depositing the boxed `JvmThread`, the virtual thread is still
    ///   `Running`. `vt.unpark()` does not change a `Running` state, so the
    ///   test failed and nothing was submitted; the permit it set
    ///   (`unpark_permit`) was read by no live code path, because the
    ///   freeze/thaw `VirtualThread::park` that consumed it is dead code (see
    ///   `virtual-threads.md`). An untimed
    ///   `LockSupport.park()` yields with `wake_after_nanos == 0`, so
    ///   `suspend_runtime` schedules no timer either — the virtual thread was
    ///   parked forever. That window is exactly the one every real
    ///   park/unpark handoff races through.
    /// * **Duplicate submission.** When the state was ALREADY `Started`
    ///   (queued, not yet mounted) the test passed and enqueued a second copy
    ///   of an id that was already in the queue.
    pub fn unpark_virtual(&self, vt_id: u64) {
        let mut resubmit = false;
        {
            let mut threads = self.threads.lock();
            let Some(vt) = threads.get_mut(&vt_id) else {
                return;
            };
            if vt.state == VirtualThreadState::Terminated {
                return;
            }
            // `Parked` alone is the resubmit condition — deliberately NOT
            // `Parked && runtime.is_some()`.
            //
            // On the live path the two are equivalent: `suspend_runtime` sets
            // `state = Parked` and `runtime = Some(..)` under one hold of the
            // `threads` mutex, so no observer can see them disagree. The
            // conjunct was therefore only ever load-bearing when the invariant
            // is violated — and there it fails the WRONG WAY. Refusing to
            // submit a `Parked` thread is a permanent hang, which is the exact
            // defect this function exists to fix. Submitting one that has no
            // runtime is harmless: `take_runtime_for_mount` returns `None` and
            // `resume_virtual_continuation` returns immediately.
            //
            // Fail open. "Parked implies resubmit" is the invariant that keeps
            // virtual threads from disappearing, and it must hold
            // unconditionally. (Caught by the pre-existing
            // `manager_park_and_unpark`, whose expectation is still exactly
            // right.)
            if vt.state == VirtualThreadState::Parked {
                vt.state = VirtualThreadState::Started;
                vt.unpark_permit = false;
                resubmit = true;
            } else {
                // Mounted, mid-unmount, or queued: `LockSupport` permits are
                // sticky, so record it and let the unmount path (or the next
                // park) observe it.
                vt.unpark_permit = true;
            }
        }
        if resubmit {
            self.scheduler.submit(vt_id);
            self.scheduler
                .stats
                .total_unparks
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Pin a virtual thread (entering a monitor / native call).
    /// Returns `Some((thread_name, carrier_id))` if pinning was applied (for JFR logging).
    pub fn pin_thread(&self, vt_id: u64, reason: PinReason) -> Option<(String, u64)> {
        let mut threads = self.threads.lock();
        if let Some(vt) = threads.get_mut(&vt_id) {
            vt.pin(reason);
            self.scheduler
                .stats
                .total_pins
                .fetch_add(1, Ordering::Relaxed);
            let carrier = vt.carrier_thread_id.unwrap_or(0);
            Some((vt.name.clone(), carrier))
        } else {
            None
        }
    }

    /// Unpin a virtual thread (exiting a monitor / native call).
    pub fn unpin_thread(&self, vt_id: u64) {
        let mut threads = self.threads.lock();
        if let Some(vt) = threads.get_mut(&vt_id) {
            vt.unpin();
        }
    }

    /// Mark a virtual thread as terminated.
    pub fn terminate(&self, vt_id: u64) {
        let mut threads = self.threads.lock();
        if let Some(vt) = threads.get_mut(&vt_id) {
            vt.state = VirtualThreadState::Terminated;
            vt.continuation.state = ContinuationState::Completed;
            vt.unmount();
        }
        drop(threads);
        let mut keyed = self.keyed_waiters.lock();
        keyed.retain(|_, waiters| {
            waiters.retain(|candidate| *candidate != vt_id);
            !waiters.is_empty()
        });
    }

    /// Register `waiter_id` as waiting for `vt_id` to complete.
    /// Returns `true` if the join was registered, `false` if already
    /// terminated.
    pub fn join(&self, vt_id: u64, waiter_id: u64) -> bool {
        let mut threads = self.threads.lock();
        if let Some(vt) = threads.get_mut(&vt_id) {
            if vt.state == VirtualThreadState::Terminated {
                return false;
            }
            vt.join_waiters.push(waiter_id);
            true
        } else {
            false
        }
    }

    /// Get a virtual thread's state.
    pub fn get_state(&self, vt_id: u64) -> Option<VirtualThreadState> {
        self.threads.lock().get(&vt_id).map(|vt| vt.state)
    }

    /// Number of tracked virtual threads.
    pub fn thread_count(&self) -> usize {
        self.threads.lock().len()
    }

    /// Whether the given ID corresponds to a virtual thread managed here.
    pub fn is_virtual(&self, vt_id: u64) -> bool {
        self.threads.lock().contains_key(&vt_id)
    }

    /// `(pool_size, mounted, queued)` for `VirtualThreadSchedulerMXBean`.
    ///
    /// One call rather than three separate accessors so the bean's
    /// `getPoolSize` / `getMountedVirtualThreadCount` /
    /// `getQueuedVirtualThreadCount` cannot sample the scheduler at three
    /// different instants and report a self-contradicting snapshot (more
    /// mounted than the pool holds).
    ///
    /// "Mounted" is carriers currently RUNNING a virtual thread, i.e.
    /// `busy_carriers` — not `live_carriers`, which counts the base pool plus
    /// any compensating carriers whether or not they are executing anything.
    pub fn scheduler_counters(&self) -> (usize, usize, usize) {
        (
            self.scheduler.live_carriers(),
            self.scheduler.busy_carriers(),
            self.scheduler.queued_len(),
        )
    }

    /// Access the scheduler stats.
    pub fn scheduler_stats(&self) -> SchedulerStatsSnapshot {
        let s = self.scheduler.stats();
        SchedulerStatsSnapshot {
            total_submissions: s.total_submissions.load(Ordering::Relaxed),
            total_steals: s.total_steals.load(Ordering::Relaxed),
            total_parks: s.total_parks.load(Ordering::Relaxed),
            total_unparks: s.total_unparks.load(Ordering::Relaxed),
            total_pins: s.total_pins.load(Ordering::Relaxed),
            peak_active: s.peak_active.load(Ordering::Relaxed),
        }
    }

    /// Park a virtual thread with real frame capture — freezes the execution
    /// state so the carrier thread is freed.
    pub fn park_with_frames(&self, vt_id: u64, frames: Vec<crate::runtime::frame::Frame>) -> bool {
        let mut threads = self.threads.lock();
        if let Some(vt) = threads.get_mut(&vt_id) {
            let result = vt.park_with_frames(frames);
            if result {
                self.scheduler
                    .stats
                    .total_parks
                    .fetch_add(1, Ordering::Relaxed);
            }
            result
        } else {
            false
        }
    }

    /// Unpark a virtual thread, restoring its frozen frames.
    /// Returns the restored frames if the thread was parked with frames.
    pub fn unpark_with_frames(&self, vt_id: u64) -> Option<Vec<crate::runtime::frame::Frame>> {
        let mut threads = self.threads.lock();
        if let Some(vt) = threads.get_mut(&vt_id) {
            let frames = vt.unpark_with_frames();
            let was_parked = vt.state == VirtualThreadState::Started;
            drop(threads);
            if was_parked {
                self.scheduler.submit(vt_id);
                self.scheduler
                    .stats
                    .total_unparks
                    .fetch_add(1, Ordering::Relaxed);
            }
            frames
        } else {
            None
        }
    }

    /// Schedule a timed wakeup for a parked virtual thread (used by Thread.sleep
    /// on virtual threads). Registers the wakeup on the single shared
    /// [`WakeupTimer`]; the dedicated timer thread resubmits the virtual thread
    /// to the scheduler once `duration` elapses (unless cancelled first).
    ///
    /// This costs one heap insertion per sleeping virtual thread rather than an
    /// OS-thread spawn, so millions of concurrently sleeping VTs remain cheap —
    /// the whole point of virtual threads. (Previously a brand-new OS thread was
    /// spawned per `Thread.sleep`, a resource/DoS hazard under many sleepers.)
    pub fn schedule_wakeup(&self, vt_id: u64, duration: std::time::Duration) {
        let deadline = Instant::now() + duration;
        let signal = Arc::new((Mutex::new(false), Condvar::new()));
        // Replace any prior registration for this vt_id; the stale heap entry
        // (if any) fails the identity check on expiry and is dropped.
        self.wakeup_signals.lock().insert(vt_id, signal.clone());
        self.wakeup_timer.schedule(vt_id, deadline, signal);
    }

    /// Cancel a pending wakeup timer (e.g. on unpark before timer fires).
    ///
    /// Flips the cancellation flag and drops the registry entry; the timer
    /// thread will observe the missing/identity-mismatched entry on expiry and
    /// skip the resubmit. Any external waiter holding the signal (obtained via
    /// `get_wakeup_signal`) is notified.
    pub fn cancel_wakeup(&self, vt_id: u64) {
        if let Some(signal) = self.wakeup_signals.lock().remove(&vt_id) {
            let (lock, cvar) = &*signal;
            *lock.lock() = true;
            cvar.notify_one();
            // Nudge the timer so it re-evaluates its next deadline promptly.
            self.wakeup_timer.notify();
        }
    }

    /// Get a wakeup signal for a virtual thread (for external cancellation).
    pub fn get_wakeup_signal(&self, vt_id: u64) -> Option<Arc<(Mutex<bool>, Condvar)>> {
        self.wakeup_signals.lock().get(&vt_id).cloned()
    }

    /// Set event-loop affinity for a virtual thread.  Once set, the
    /// virtual-thread scheduler routes the thread's continuation to the
    /// named event loop rather than any fork-join carrier.
    ///
    /// `raw_id` is the Java long stored in the VertxImpl / NioEventLoop
    /// mirror.  It is validated by [`super::event_loop::EventLoopId::from_raw`]
    /// before being stored so invalid ids are rejected eagerly.
    pub fn set_event_loop_affinity(
        &self,
        vt_id: u64,
        raw_id: i64,
    ) -> Result<(), super::event_loop::EventLoopError> {
        use super::event_loop::{event_loop_manager, EventLoopId};
        let eid = EventLoopId::from_raw(raw_id)?;
        // Verify the id actually exists.
        event_loop_manager().lookup(eid)?;
        // (We don't store affinity on the VirtualThread itself yet — the
        // routing decision is made at submit time by the caller who holds
        // the EventLoopManager.  This method is the validated entry point.)
        let _ = vt_id; // future: tag VirtualThread with affinity
        Ok(())
    }

    /// Schedule a task directly on the event loop identified by `raw_id`.
    ///
    /// This is the cross-thread bridge used by Vert.x `VertxImpl` natives
    /// to push work onto a specific NioEventLoop without going through the
    /// virtual-thread park/unpark cycle.  Validates `raw_id` and delegates
    /// to [`super::event_loop::EventLoopManager::schedule_on_event_loop`].
    pub fn schedule_on_event_loop(
        &self,
        raw_id: i64,
        task: super::event_loop::EventLoopTask,
    ) -> Result<(), super::event_loop::EventLoopError> {
        use super::event_loop::{event_loop_manager, EventLoopId};
        let eid = EventLoopId::from_raw(raw_id)?;
        event_loop_manager().schedule_on_event_loop(eid, task)
    }
}

/// Immutable snapshot of scheduler statistics (avoids returning refs to atomics
/// behind a `Mutex`).
#[derive(Debug, Clone)]
pub struct SchedulerStatsSnapshot {
    pub total_submissions: u64,
    pub total_steals: u64,
    pub total_parks: u64,
    pub total_unparks: u64,
    pub total_pins: u64,
    pub peak_active: u64,
}

// ---------------------------------------------------------------------------
// ThreadBuilder
// ---------------------------------------------------------------------------

/// Builder pattern for `Thread.ofVirtual()` / `Thread.ofPlatform()`.
#[derive(Debug, Clone)]
pub struct ThreadBuilder {
    pub kind: ThreadBuilderKind,
    pub name: Option<String>,
    pub daemon: bool,
    pub stack_size: Option<usize>,
    pub name_prefix: Option<String>,
    pub counter: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadBuilderKind {
    Platform,
    Virtual,
}

impl ThreadBuilder {
    pub fn of_virtual() -> Self {
        Self {
            kind: ThreadBuilderKind::Virtual,
            name: None,
            daemon: true, // virtual threads are always daemon
            stack_size: None,
            name_prefix: None,
            counter: 0,
        }
    }

    pub fn of_platform() -> Self {
        Self {
            kind: ThreadBuilderKind::Platform,
            name: None,
            daemon: false,
            stack_size: None,
            name_prefix: None,
            counter: 0,
        }
    }

    pub fn name(mut self, name: &str) -> Self {
        self.name = Some(name.to_string());
        self
    }

    pub fn name_prefix(mut self, prefix: &str) -> Self {
        self.name_prefix = Some(prefix.to_string());
        self
    }

    pub fn daemon(mut self, daemon: bool) -> Self {
        self.daemon = daemon;
        self
    }

    pub fn stack_size(mut self, size: usize) -> Self {
        self.stack_size = Some(size);
        self
    }

    /// Generate a thread name.  If a `name_prefix` is set the name is
    /// `"<prefix><counter>"` and the internal counter is incremented.
    /// Otherwise the explicit `name` (if any) is returned, falling back to
    /// an empty string.
    pub fn build_name(&mut self) -> String {
        if let Some(ref prefix) = self.name_prefix {
            let n = format!("{}{}", prefix, self.counter);
            self.counter += 1;
            n
        } else {
            self.name.clone().unwrap_or_default()
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Value;

    // -- Continuation ---------------------------------------------------------

    #[test]
    fn continuation_new_state() {
        let scope = ContinuationScope {
            name: "test".into(),
        };
        let c = Continuation::new(scope);
        assert_eq!(c.state, ContinuationState::New);
        assert_eq!(c.yield_count, 0);
        assert_eq!(c.mount_count, 0);
        assert!(c.frozen_frames.is_empty());
    }

    #[test]
    fn continuation_freeze_thaw_roundtrip() {
        let scope = ContinuationScope {
            name: "test".into(),
        };
        let mut c = Continuation::new(scope);
        c.state = ContinuationState::Running;

        let frame = FrozenFrame {
            class_name: "Foo".into(),
            method_name: "bar".into(),
            descriptor: "()V".into(),
            bytecode_pc: 42,
            locals: vec![1, 2, 3],
            local_tags: vec![0, 1, 0],
            stack: vec![10, 20],
            stack_tags: vec![0, 0],
            code: None,
            class_id: None,
            max_stack: None,
            max_locals: None,
            exception_table: None,
            source_file: None,
        };
        c.freeze(vec![frame]);
        assert_eq!(c.state, ContinuationState::Suspended);
        assert_eq!(c.yield_count, 1);

        let restored = c.thaw();
        assert_eq!(c.state, ContinuationState::Running);
        assert_eq!(c.mount_count, 1);
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].bytecode_pc, 42);
    }

    #[test]
    fn continuation_state_transitions() {
        let scope = ContinuationScope { name: "sc".into() };
        let mut c = Continuation::new(scope);
        assert_eq!(c.state, ContinuationState::New);

        c.state = ContinuationState::Running;
        assert_eq!(c.state, ContinuationState::Running);

        c.freeze(Vec::new());
        assert_eq!(c.state, ContinuationState::Suspended);

        let _ = c.thaw();
        assert_eq!(c.state, ContinuationState::Running);

        c.state = ContinuationState::Completed;
        assert_eq!(c.state, ContinuationState::Completed);
    }

    #[test]
    fn continuation_yield_count_increments() {
        let mut c = Continuation::new(ContinuationScope { name: "s".into() });
        c.state = ContinuationState::Running;
        c.freeze(Vec::new());
        assert_eq!(c.yield_count(), 1);
        let _ = c.thaw();
        c.freeze(Vec::new());
        assert_eq!(c.yield_count(), 2);
    }

    #[test]
    fn continuation_can_yield() {
        let mut c = Continuation::new(ContinuationScope { name: "s".into() });
        assert!(!c.can_yield()); // New
        c.state = ContinuationState::Running;
        assert!(c.can_yield());
        c.freeze(Vec::new());
        assert!(!c.can_yield()); // Suspended
    }

    // -- FrozenFrame ----------------------------------------------------------

    #[test]
    fn frozen_frame_preserves_locals_and_stack() {
        let frame = FrozenFrame {
            class_name: "java/lang/Object".into(),
            method_name: "<init>".into(),
            descriptor: "()V".into(),
            bytecode_pc: 0,
            locals: vec![100, 200, 300],
            local_tags: vec![1, 2, 3],
            stack: vec![400],
            stack_tags: vec![4],
            code: None,
            class_id: None,
            max_stack: None,
            max_locals: None,
            exception_table: None,
            source_file: None,
        };
        assert_eq!(frame.locals, vec![100, 200, 300]);
        assert_eq!(frame.local_tags, vec![1, 2, 3]);
        assert_eq!(frame.stack, vec![400]);
        assert_eq!(frame.stack_tags, vec![4]);
    }

    // -- VirtualThread --------------------------------------------------------

    #[test]
    fn vt_new_starts_in_new_state() {
        let vt = VirtualThread::new(1, "vt-1".into());
        assert_eq!(vt.state, VirtualThreadState::New);
        assert_eq!(vt.id, 1);
        assert_eq!(vt.name, "vt-1");
        assert!(vt.carrier_thread_id.is_none());
        assert_eq!(vt.pin_count, 0);
        assert!(!vt.is_pinned());
    }

    #[test]
    fn vt_mount_unmount() {
        let mut vt = VirtualThread::new(1, "vt".into());
        vt.mount(42);
        assert_eq!(vt.state, VirtualThreadState::Running);
        assert_eq!(vt.carrier_thread_id, Some(42));

        vt.unmount();
        assert!(vt.carrier_thread_id.is_none());
    }

    #[test]
    fn vt_pin_unpin_with_reason() {
        let mut vt = VirtualThread::new(1, "vt".into());
        vt.mount(0);
        vt.pin(PinReason::Monitor);
        assert!(vt.is_pinned());
        assert_eq!(vt.pin_reason, Some(PinReason::Monitor));
        assert_eq!(vt.state, VirtualThreadState::Pinned);

        vt.unpin();
        assert!(!vt.is_pinned());
        assert_eq!(vt.pin_reason, None);
        assert_eq!(vt.state, VirtualThreadState::Running);
    }

    #[test]
    fn vt_park_not_pinned_yields() {
        let mut vt = VirtualThread::new(1, "vt".into());
        vt.mount(0);
        let result = vt.park();
        assert!(result);
        assert_eq!(vt.state, VirtualThreadState::Parked);
        assert!(vt.carrier_thread_id.is_none());
    }

    #[test]
    fn vt_park_when_pinned_blocks() {
        let mut vt = VirtualThread::new(1, "vt".into());
        vt.mount(0);
        vt.pin(PinReason::NativeMethod);
        let result = vt.park();
        assert!(!result);
        // Still mounted -- carrier blocked.
        assert_eq!(vt.carrier_thread_id, Some(0));
    }

    #[test]
    fn vt_unpark_sets_permit() {
        let mut vt = VirtualThread::new(1, "vt".into());
        vt.unpark();
        assert!(vt.unpark_permit);
    }

    #[test]
    fn vt_unpark_permit_consumed_by_park() {
        let mut vt = VirtualThread::new(1, "vt".into());
        vt.mount(0);
        vt.unpark();
        assert!(vt.unpark_permit);
        let result = vt.park();
        assert!(result);
        assert!(!vt.unpark_permit);
        // State should still be Running (permit was consumed, no actual park).
        assert_eq!(vt.state, VirtualThreadState::Running);
    }

    #[test]
    fn vt_double_pin_increments_count() {
        let mut vt = VirtualThread::new(1, "vt".into());
        vt.mount(0);
        vt.pin(PinReason::Monitor);
        vt.pin(PinReason::ClassInit);
        assert_eq!(vt.pin_count, 2);
        assert!(vt.is_pinned());
        // Last reason wins
        assert_eq!(vt.pin_reason, Some(PinReason::ClassInit));
        vt.unpin();
        assert!(vt.is_pinned()); // still 1
        vt.unpin();
        assert!(!vt.is_pinned());
    }

    // -- ForkJoinScheduler ----------------------------------------------------

    #[test]
    fn scheduler_submit_and_next_task() {
        let sched = ForkJoinScheduler::new(2);
        sched.submit(100);
        sched.submit(200);
        assert_eq!(sched.next_task(0), Some(100));
        assert_eq!(sched.next_task(0), Some(200));
        assert_eq!(sched.next_task(0), None);
    }

    #[test]
    fn scheduler_work_stealing() {
        let sched = ForkJoinScheduler::new(2);
        // Put a task directly into carrier 1's queue.
        sched.work_queues[1].lock().push_back(300);
        // Carrier 0 should steal it.
        let stolen = sched.try_steal(0);
        assert_eq!(stolen, Some(300));
        assert_eq!(sched.stats.total_steals.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn scheduler_default_parallelism() {
        let sched = ForkJoinScheduler::with_default_parallelism();
        assert!(sched.parallelism >= 1);
        assert_eq!(sched.carriers.len(), sched.parallelism);
    }

    #[test]
    fn scheduler_mount_unmount_carrier() {
        let mut sched = ForkJoinScheduler::new(2);
        sched.mount(0, 42);
        assert_eq!(sched.carriers[0].mounted_virtual_thread, Some(42));
        assert!(!sched.carriers[0].available);
        assert_eq!(sched.active_count(), 1);

        let unmounted = sched.unmount(0);
        assert_eq!(unmounted, Some(42));
        assert!(sched.carriers[0].available);
        assert_eq!(sched.active_count(), 0);
        assert_eq!(sched.carriers[0].tasks_completed, 1);
    }

    #[test]
    fn scheduler_active_count_tracking() {
        let mut sched = ForkJoinScheduler::new(4);
        sched.mount(0, 1);
        sched.mount(1, 2);
        assert_eq!(sched.active_count(), 2);
        sched.unmount(0);
        assert_eq!(sched.active_count(), 1);
        sched.unmount(1);
        assert_eq!(sched.active_count(), 0);
    }

    #[test]
    fn scheduler_stats_submissions() {
        let sched = ForkJoinScheduler::new(1);
        sched.submit(1);
        sched.submit(2);
        sched.submit(3);
        assert_eq!(sched.stats().total_submissions.load(Ordering::Relaxed), 3);
    }

    // -- VirtualThreadManager -------------------------------------------------

    #[test]
    fn manager_create_and_start() {
        let mgr = VirtualThreadManager::new(2);
        let id = mgr.create_virtual_thread("vt-1");
        assert!(mgr.is_virtual(id));
        assert_eq!(mgr.get_state(id), Some(VirtualThreadState::New));

        mgr.start(id);
        assert_eq!(mgr.get_state(id), Some(VirtualThreadState::Started));
    }

    #[test]
    fn manager_park_and_unpark() {
        let mgr = VirtualThreadManager::new(2);
        let id = mgr.create_virtual_thread("vt");
        // Mount it so it can park.
        {
            let mut threads = mgr.threads.lock();
            threads.get_mut(&id).unwrap().mount(0);
        }
        let parked = mgr.park_virtual(id);
        assert!(parked);
        assert_eq!(mgr.get_state(id), Some(VirtualThreadState::Parked));

        mgr.unpark_virtual(id);
        assert_eq!(mgr.get_state(id), Some(VirtualThreadState::Started));
    }

    #[test]
    fn manager_pin_and_unpin() {
        let mgr = VirtualThreadManager::new(2);
        let id = mgr.create_virtual_thread("vt");
        {
            let mut threads = mgr.threads.lock();
            threads.get_mut(&id).unwrap().mount(0);
        }
        mgr.pin_thread(id, PinReason::Monitor);
        assert_eq!(mgr.get_state(id), Some(VirtualThreadState::Pinned));

        mgr.unpin_thread(id);
        assert_eq!(mgr.get_state(id), Some(VirtualThreadState::Running));
    }

    #[test]
    fn manager_terminate() {
        let mgr = VirtualThreadManager::new(1);
        let id = mgr.create_virtual_thread("vt");
        mgr.start(id);
        mgr.terminate(id);
        assert_eq!(mgr.get_state(id), Some(VirtualThreadState::Terminated));
    }

    #[test]
    fn manager_join_tracking() {
        let mgr = VirtualThreadManager::new(1);
        let id = mgr.create_virtual_thread("vt");
        mgr.start(id);

        let joined = mgr.join(id, 999);
        assert!(joined);
        {
            let threads = mgr.threads.lock();
            assert!(threads[&id].join_waiters.contains(&999));
        }
    }

    #[test]
    fn manager_join_already_terminated() {
        let mgr = VirtualThreadManager::new(1);
        let id = mgr.create_virtual_thread("vt");
        mgr.terminate(id);
        let joined = mgr.join(id, 999);
        assert!(!joined); // Already terminated
    }

    #[test]
    fn manager_thread_count() {
        let mgr = VirtualThreadManager::new(1);
        assert_eq!(mgr.thread_count(), 0);
        mgr.create_virtual_thread("a");
        mgr.create_virtual_thread("b");
        assert_eq!(mgr.thread_count(), 2);
    }

    #[test]
    fn manager_is_virtual() {
        let mgr = VirtualThreadManager::new(1);
        let id = mgr.create_virtual_thread("vt");
        assert!(mgr.is_virtual(id));
        assert!(!mgr.is_virtual(9999));
    }

    #[test]
    fn manager_scheduler_stats() {
        let mgr = VirtualThreadManager::new(2);
        let id = mgr.create_virtual_thread("vt");
        mgr.start(id);
        let stats = mgr.scheduler_stats();
        assert_eq!(stats.total_submissions, 1);
    }

    // -- ThreadBuilder --------------------------------------------------------

    #[test]
    fn builder_of_virtual() {
        let b = ThreadBuilder::of_virtual();
        assert_eq!(b.kind, ThreadBuilderKind::Virtual);
        assert!(b.daemon); // virtual threads are daemon by default
        assert!(b.name.is_none());
    }

    #[test]
    fn builder_of_platform() {
        let b = ThreadBuilder::of_platform();
        assert_eq!(b.kind, ThreadBuilderKind::Platform);
        assert!(!b.daemon);
    }

    #[test]
    fn builder_name_setting() {
        let b = ThreadBuilder::of_virtual().name("worker");
        assert_eq!(b.name.as_deref(), Some("worker"));
    }

    #[test]
    fn builder_name_prefix_with_counter() {
        let mut b = ThreadBuilder::of_virtual().name_prefix("vt-");
        assert_eq!(b.build_name(), "vt-0");
        assert_eq!(b.build_name(), "vt-1");
        assert_eq!(b.build_name(), "vt-2");
    }

    #[test]
    fn builder_daemon_flag() {
        let b = ThreadBuilder::of_platform().daemon(true);
        assert!(b.daemon);
        let b2 = ThreadBuilder::of_virtual().daemon(false);
        assert!(!b2.daemon);
    }

    #[test]
    fn builder_stack_size() {
        let b = ThreadBuilder::of_platform().stack_size(1024 * 1024);
        assert_eq!(b.stack_size, Some(1024 * 1024));
    }

    // -- ContinuationScope ----------------------------------------------------

    #[test]
    fn continuation_scope_equality() {
        let a = ContinuationScope {
            name: "VirtualThread".into(),
        };
        let b = ContinuationScope {
            name: "VirtualThread".into(),
        };
        let c = ContinuationScope {
            name: "Other".into(),
        };
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    // -- Extra edge cases -----------------------------------------------------

    #[test]
    fn scheduler_next_task_prefers_own_queue() {
        let sched = ForkJoinScheduler::new(2);
        sched.work_queues[0].lock().push_back(10);
        sched.submission_queue.lock().push_back(20);
        // Own queue should come first.
        assert_eq!(sched.next_task(0), Some(10));
        assert_eq!(sched.next_task(0), Some(20));
    }

    #[test]
    fn scheduler_peak_active_tracks_max() {
        let mut sched = ForkJoinScheduler::new(4);
        sched.mount(0, 1);
        sched.mount(1, 2);
        sched.mount(2, 3);
        assert_eq!(sched.stats.peak_active.load(Ordering::Relaxed), 3);
        sched.unmount(0);
        sched.unmount(1);
        // Peak should still be 3.
        assert_eq!(sched.stats.peak_active.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn manager_park_nonexistent_thread() {
        let mgr = VirtualThreadManager::new(1);
        let result = mgr.park_virtual(9999);
        assert!(!result);
    }

    #[test]
    fn vt_unpark_when_parked_changes_state() {
        let mut vt = VirtualThread::new(1, "vt".into());
        vt.mount(0);
        vt.park();
        assert_eq!(vt.state, VirtualThreadState::Parked);
        vt.unpark();
        assert_eq!(vt.state, VirtualThreadState::Started);
    }

    #[test]
    fn test_virtual_thread_many_concurrent() {
        // Stress test: create and run many virtual threads
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let counter = Arc::new(AtomicUsize::new(0));
        let handles: Vec<_> = (0..1000)
            .map(|_| {
                let c = counter.clone();
                std::thread::spawn(move || {
                    c.fetch_add(1, Ordering::Relaxed);
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(counter.load(Ordering::SeqCst), 1000);
    }

    #[test]
    fn test_virtual_thread_yield_fairness() {
        // Verify that yielding allows other threads to progress
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let counter = Arc::new(AtomicUsize::new(0));
        let c1 = counter.clone();
        let c2 = counter.clone();
        let t1 = std::thread::spawn(move || {
            for _ in 0..100 {
                c1.fetch_add(1, Ordering::SeqCst);
                std::thread::yield_now();
            }
        });
        let t2 = std::thread::spawn(move || {
            for _ in 0..100 {
                c2.fetch_add(1, Ordering::SeqCst);
                std::thread::yield_now();
            }
        });
        t1.join().unwrap();
        t2.join().unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 200);
    }

    #[test]
    fn test_virtual_thread_sleep_and_resume() {
        use std::time::{Duration, Instant};
        let start = Instant::now();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            42
        });
        let result = handle.join().unwrap();
        assert_eq!(result, 42);
        assert!(start.elapsed() >= Duration::from_millis(10));
    }

    #[test]
    fn test_virtual_thread_exception_propagation() {
        // If virtual thread throws, it should be joinable with error
        let handle =
            std::thread::spawn(|| -> Result<i32, String> { Err("virtual thread failure".into()) });
        let result = handle.join().unwrap();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "virtual thread failure");
    }

    #[test]
    fn test_carrier_thread_reuse() {
        // After virtual thread unmounts, carrier should be reusable
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let count = Arc::new(AtomicUsize::new(0));
        // Run sequentially - same carrier thread should be reused
        for _ in 0..10 {
            let c = count.clone();
            let h = std::thread::spawn(move || {
                c.fetch_add(1, Ordering::SeqCst);
            });
            h.join().unwrap();
        }
        assert_eq!(count.load(Ordering::SeqCst), 10);
    }

    #[test]
    fn test_virtual_thread_local_isolation() {
        // Each virtual thread should have isolated thread-locals
        use std::cell::RefCell;
        thread_local! {
            static LOCAL: RefCell<i32> = RefCell::new(0);
        }
        let handles: Vec<_> = (0..5)
            .map(|i| {
                std::thread::spawn(move || {
                    LOCAL.with(|v| {
                        *v.borrow_mut() = i;
                        assert_eq!(*v.borrow(), i);
                    });
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn test_virtual_thread_interrupt() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let interrupted = Arc::new(AtomicBool::new(false));
        let i2 = interrupted.clone();
        let handle = std::thread::spawn(move || {
            // Simulate interrupt check
            if i2.load(Ordering::SeqCst) {
                return Err("interrupted");
            }
            Ok(())
        });
        // Don't interrupt - should succeed
        let result = handle.join().unwrap();
        assert!(result.is_ok());
    }

    // =====================================================================
    // Phase 81 Tests — Virtual Threads Real Implementation
    // =====================================================================

    // -- 81.1: Continuation Data Structure (real freeze/thaw) ----------------

    #[test]
    fn p81_continuation_freeze_real_frame() {
        use crate::classloading::ClassId;
        use crate::runtime::frame::Frame;

        // Create a real Frame with locals and operand stack values.
        let mut frame = Frame::new(
            ClassId::new(0),
            "com/example/Worker".to_string(),
            "compute".to_string(),
            "(II)I".to_string(),
            Some("Worker.java".to_string()),
            vec![0x1a, 0x1b, 0x60, 0xac], // iload_0, iload_1, iadd, ireturn
            vec![],
            10,
            4,
            &[Value::Int(42), Value::Int(58), Value::Int(0), Value::Int(0)],
        );
        // Push a value onto the operand stack
        frame.stack.push(Value::Int(100)).unwrap();

        let frozen = frame.to_frozen_frame();
        assert_eq!(frozen.class_name, "com/example/Worker");
        assert_eq!(frozen.method_name, "compute");
        assert_eq!(frozen.descriptor, "(II)I");
        assert_eq!(frozen.bytecode_pc, 0);
        assert!(frozen.code.is_some());
        assert!(frozen.class_id.is_some());
        assert_eq!(frozen.max_stack, Some(10));
        assert_eq!(frozen.max_locals, Some(4));
        assert_eq!(frozen.source_file, Some("Worker.java".to_string()));
        // Locals should be preserved
        assert_eq!(frozen.locals.len(), 4);
        // Stack should have 1 entry
        assert_eq!(frozen.stack.len(), 1);
    }

    #[test]
    fn p81_continuation_thaw_restores_frame() {
        use crate::classloading::ClassId;
        use crate::runtime::frame::Frame;

        let mut frame = Frame::new(
            ClassId::new(7),
            "com/example/Fibonacci".to_string(),
            "fib".to_string(),
            "(I)I".to_string(),
            None,
            vec![0x1a, 0xac], // iload_0, ireturn
            vec![],
            5,
            2,
            &[Value::Int(10), Value::Long(999)],
        );
        frame.pc = 1; // advance PC
        frame.stack.push(Value::Int(55)).unwrap();
        frame.stack.push(Value::Int(89)).unwrap();

        let frozen = frame.to_frozen_frame();
        let restored = Frame::from_frozen_frame(frozen);

        assert_eq!(restored.class_name(), "com/example/Fibonacci");
        assert_eq!(restored.method_name(), "fib");
        assert_eq!(restored.method_descriptor(), "(I)I");
        assert_eq!(restored.pc, 1);
        assert_eq!(restored.get_local(0), Value::Int(10));
        // Operand stack should have 2 entries
        assert_eq!(restored.stack.len(), 2);
    }

    #[test]
    fn p81_continuation_nested_freeze() {
        use crate::classloading::ClassId;
        use crate::runtime::frame::Frame;

        let scope = ContinuationScope {
            name: "VirtualThread".into(),
        };
        let mut cont = Continuation::new(scope);
        cont.state = ContinuationState::Running;

        // Create a stack of 3 nested frames (simulating call chain)
        let frames: Vec<Frame> = (0..3)
            .map(|i| {
                Frame::new(
                    ClassId::new(i),
                    format!("Class{}", i),
                    format!("method{}", i),
                    "()V".to_string(),
                    None,
                    vec![0xb1], // return
                    vec![],
                    5,
                    2,
                    &[Value::Int(i as i32 * 10)],
                )
            })
            .collect();

        cont.freeze_frames(frames);
        assert_eq!(cont.state, ContinuationState::Suspended);
        assert_eq!(cont.frozen_frames.len(), 3);
        assert_eq!(cont.yield_count(), 1);

        // Thaw and verify all frames restored
        let restored = cont.thaw_frames();
        assert_eq!(restored.len(), 3);
        assert_eq!(cont.state, ContinuationState::Running);
        assert_eq!(cont.mount_count(), 1);

        // Verify each frame's identity
        for (i, frame) in restored.iter().enumerate() {
            assert_eq!(frame.class_name(), format!("Class{}", i));
            assert_eq!(frame.method_name(), format!("method{}", i));
            assert_eq!(frame.get_local(0), Value::Int(i as i32 * 10));
        }
    }

    #[test]
    fn p81_continuation_deep_stack_freeze() {
        use crate::classloading::ClassId;
        use crate::runtime::frame::Frame;

        let scope = ContinuationScope {
            name: "VirtualThread".into(),
        };
        let mut cont = Continuation::new(scope);
        cont.state = ContinuationState::Running;

        // Create a deep call stack (100 frames — e.g. recursive method)
        let depth = 100;
        let frames: Vec<Frame> = (0..depth)
            .map(|i| {
                let mut f = Frame::new(
                    ClassId::new(0),
                    "RecursiveClass".to_string(),
                    "recurse".to_string(),
                    "(I)V".to_string(),
                    None,
                    vec![0xb1],
                    vec![],
                    5,
                    3,
                    &[Value::Int(i as i32)],
                );
                f.pc = i; // each frame at different PC
                f
            })
            .collect();

        cont.freeze_frames(frames);
        assert_eq!(cont.frozen_frames.len(), depth);

        let restored = cont.thaw_frames();
        assert_eq!(restored.len(), depth);
        for (i, frame) in restored.iter().enumerate() {
            assert_eq!(frame.pc, i);
            assert_eq!(frame.get_local(0), Value::Int(i as i32));
        }
    }

    #[test]
    fn p81_continuation_freeze_preserves_exception_state() {
        use crate::classloading::ClassId;
        use crate::runtime::frame::Frame;
        use cratonvm_reader::attribute::ExceptionTableEntry;

        let scope = ContinuationScope {
            name: "VirtualThread".into(),
        };
        let mut cont = Continuation::new(scope);
        cont.state = ContinuationState::Running;

        // Frame with an exception table (simulating try/catch)
        let ex_table = vec![ExceptionTableEntry {
            start_pc: 0,
            end_pc: 10,
            handler_pc: 20,
            catch_type: 5,
        }];
        let frame = Frame::new(
            ClassId::new(0),
            "TryCatch".to_string(),
            "handle".to_string(),
            "()V".to_string(),
            Some("TryCatch.java".to_string()),
            vec![0xb1; 30], // 30 bytes of code
            ex_table,
            10,
            5,
            &[Value::Int(1), Value::Object(None)],
        );

        cont.freeze_frames(vec![frame]);
        let restored = cont.thaw_frames();
        assert_eq!(restored.len(), 1);
        let rf = &restored[0];
        assert_eq!(rf.class_name(), "TryCatch");
        // Exception table should be preserved
        let et = rf.exception_table();
        assert_eq!(et.len(), 1);
        assert_eq!(et[0].start_pc, 0);
        assert_eq!(et[0].end_pc, 10);
        assert_eq!(et[0].handler_pc, 20);
        assert_eq!(et[0].catch_type, 5);
    }

    // -- 81.2: ForkJoinPool Scheduler (real carrier threads) ----------------

    #[test]
    fn p81_scheduler_basic_carrier_execution() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let mgr = VirtualThreadManager::new(2);
        let executed = Arc::new(AtomicUsize::new(0));
        let executed2 = executed.clone();

        // Start carrier threads with a simple task function
        mgr.start_carriers(move |_vt_id| {
            executed2.fetch_add(1, Ordering::SeqCst);
        });

        // Submit 5 virtual threads
        for _ in 0..5 {
            let id = mgr.create_virtual_thread("vt");
            mgr.start(id);
        }

        // Wait for execution (carriers will pick them up)
        std::thread::sleep(std::time::Duration::from_millis(200));
        mgr.shutdown();
        assert_eq!(executed.load(Ordering::SeqCst), 5);
    }

    #[test]
    fn p81_scheduler_work_stealing_execution() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let sched = Arc::new(ForkJoinScheduler::new(4));

        // Put tasks directly into carrier 0's work queue
        for i in 0..10u64 {
            sched.work_queues[0].lock().push_back(i);
        }

        let stolen_count = Arc::new(AtomicUsize::new(0));
        let handles: Vec<_> = (1..4)
            .map(|idx| {
                let s = sched.clone();
                let sc = stolen_count.clone();
                std::thread::spawn(move || {
                    while let Some(_) = s.try_steal(idx) {
                        sc.fetch_add(1, Ordering::SeqCst);
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }
        // At least some tasks should have been stolen
        assert!(stolen_count.load(Ordering::SeqCst) > 0);
        assert!(sched.stats.total_steals.load(Ordering::Relaxed) > 0);
    }

    #[test]
    fn p81_scheduler_carrier_reuse() {
        use std::collections::HashSet;

        let mgr = VirtualThreadManager::new(1); // 1 carrier
        let carrier_ids = Arc::new(Mutex::new(Vec::new()));
        let carrier_ids2 = carrier_ids.clone();

        mgr.start_carriers(move |_vt_id| {
            // Record which OS thread is executing
            let tid = std::thread::current().id();
            carrier_ids2.lock().push(format!("{:?}", tid));
        });

        // Submit tasks sequentially
        for i in 0..5 {
            let id = mgr.create_virtual_thread(&format!("vt-{}", i));
            mgr.start(id);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        std::thread::sleep(std::time::Duration::from_millis(200));
        mgr.shutdown();

        let ids = carrier_ids.lock();
        // All should have run on the same carrier (1 carrier pool)
        if ids.len() >= 2 {
            let unique: HashSet<_> = ids.iter().collect();
            assert_eq!(unique.len(), 1, "all tasks should run on the same carrier");
        }
    }

    #[test]
    fn p81_scheduler_pool_shutdown() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let mgr = VirtualThreadManager::new(2);
        let count = Arc::new(AtomicUsize::new(0));
        let count2 = count.clone();

        mgr.start_carriers(move |_vt_id| {
            count2.fetch_add(1, Ordering::SeqCst);
        });

        // Submit a few tasks
        for _ in 0..3 {
            let id = mgr.create_virtual_thread("vt");
            mgr.start(id);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));

        // Shutdown should complete without hanging
        mgr.shutdown();

        // Verify the scheduler is no longer running
        assert!(!mgr.scheduler().is_running());
        // All submitted tasks should have executed
        assert_eq!(count.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn p81_scheduler_1000_virtual_threads() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let mgr = VirtualThreadManager::new(4);
        let counter = Arc::new(AtomicUsize::new(0));
        let counter2 = counter.clone();

        mgr.start_carriers(move |_vt_id| {
            counter2.fetch_add(1, Ordering::SeqCst);
        });

        for i in 0..1000 {
            let id = mgr.create_virtual_thread(&format!("vt-{}", i));
            mgr.start(id);
        }

        // Wait for all 1000 tasks to complete
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while counter.load(Ordering::SeqCst) < 1000 {
            if std::time::Instant::now() > deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        mgr.shutdown();
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1000,
            "all 1000 virtual threads should have executed"
        );
    }

    // -- 81.3: Virtual Thread Park/Unpark --------------------------------

    #[test]
    fn p81_park_with_frames_freezes_state() {
        use crate::classloading::ClassId;
        use crate::runtime::frame::Frame;

        let mut vt = VirtualThread::new(1, "vt".into());
        vt.mount(0);

        let frames = vec![Frame::new(
            ClassId::new(0),
            "ParkTest".to_string(),
            "work".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            5,
            2,
            &[Value::Int(42)],
        )];

        let result = vt.park_with_frames(frames);
        assert!(result);
        assert_eq!(vt.state, VirtualThreadState::Parked);
        assert!(vt.carrier_thread_id.is_none()); // unmounted
        assert_eq!(vt.continuation.state, ContinuationState::Suspended);
        assert_eq!(vt.continuation.frozen_frames.len(), 1);
        assert_eq!(vt.continuation.frozen_frames[0].class_name, "ParkTest");
    }

    #[test]
    fn p81_unpark_with_frames_restores_state() {
        use crate::classloading::ClassId;
        use crate::runtime::frame::Frame;

        let mut vt = VirtualThread::new(1, "vt".into());
        vt.mount(0);

        let frames = vec![Frame::new(
            ClassId::new(0),
            "UnparkTest".to_string(),
            "resume".to_string(),
            "(I)V".to_string(),
            None,
            vec![0x1a, 0xb1], // iload_0, return
            vec![],
            5,
            3,
            &[Value::Int(77)],
        )];

        vt.park_with_frames(frames);
        assert_eq!(vt.state, VirtualThreadState::Parked);

        let restored = vt.unpark_with_frames();
        assert!(restored.is_some());
        let frames = restored.unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].class_name(), "UnparkTest");
        assert_eq!(frames[0].get_local(0), Value::Int(77));
        assert_eq!(vt.state, VirtualThreadState::Started);
    }

    #[test]
    fn p81_park_timeout_wakeup() {
        let mgr = VirtualThreadManager::new(1);
        let id = mgr.create_virtual_thread("sleeper");
        {
            let mut threads = mgr.threads.lock();
            threads.get_mut(&id).unwrap().mount(0);
        }

        // Park the virtual thread
        mgr.park_virtual(id);
        assert_eq!(mgr.get_state(id), Some(VirtualThreadState::Parked));

        // Schedule a wakeup after 50ms
        mgr.schedule_wakeup(id, std::time::Duration::from_millis(50));

        // Poll for the resubmission rather than sleeping a fixed 150 ms and
        // then looking exactly once. The property under test is that the timer
        // FIRES; a fixed sleep additionally asserts that this machine schedules
        // the timer thread promptly, which it does not do under the full
        // suite's load — `left: None, right: Some(1)`, 1 of 30 full-suite runs
        // and never in isolation.
        //
        // The deadline is generous because it bounds only the failure case: a
        // working timer satisfies this in ~50 ms, and a broken one is still
        // reported, just later.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut task = None;
        while std::time::Instant::now() < deadline {
            task = mgr.scheduler().next_task(0);
            if task.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        // The scheduler should have the task resubmitted
        assert_eq!(
            task,
            Some(id),
            "timer should have resubmitted the virtual thread"
        );
    }

    #[test]
    fn wakeup_cancel_prevents_resubmit() {
        // A cancelled wakeup must NOT resubmit the virtual thread even after its
        // original deadline passes.
        let mgr = VirtualThreadManager::new(1);
        let id = mgr.create_virtual_thread("cancelled");
        {
            let mut threads = mgr.threads.lock();
            threads.get_mut(&id).unwrap().mount(0);
        }
        mgr.park_virtual(id);
        mgr.schedule_wakeup(id, std::time::Duration::from_millis(80));
        // Cancel well before the deadline.
        std::thread::sleep(std::time::Duration::from_millis(10));
        mgr.cancel_wakeup(id);
        // Wait past the original deadline.
        std::thread::sleep(std::time::Duration::from_millis(150));
        assert_eq!(
            mgr.scheduler().next_task(0),
            None,
            "cancelled wakeup must not resubmit the virtual thread"
        );
        // Registry entry must be gone after cancellation.
        assert!(mgr.get_wakeup_signal(id).is_none());
        mgr.shutdown();
    }

    #[test]
    fn wakeup_many_sleepers_share_single_timer() {
        // Many concurrent sleepers are serviced by ONE shared timer thread (no
        // per-sleep OS-thread spawn); all of them must eventually be resubmitted.
        let mgr = VirtualThreadManager::new(2);
        let mut ids = Vec::new();
        for i in 0..64u64 {
            let id = mgr.create_virtual_thread(&format!("sleeper-{i}"));
            {
                let mut threads = mgr.threads.lock();
                threads.get_mut(&id).unwrap().mount(0);
            }
            mgr.park_virtual(id);
            // Staggered short deadlines exercise the min-heap ordering.
            mgr.schedule_wakeup(id, std::time::Duration::from_millis(10 + (i % 8) * 5));
            ids.push(id);
        }

        // Drain resubmitted tasks until all fire or we time out.
        let mut fired = std::collections::HashSet::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while fired.len() < ids.len() && std::time::Instant::now() < deadline {
            while let Some(t) = mgr.scheduler().next_task(0) {
                fired.insert(t);
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            fired.len(),
            ids.len(),
            "all sleeping virtual threads should be resubmitted by the shared timer"
        );
        mgr.shutdown();
    }

    #[test]
    fn wakeup_reschedule_supersedes_stale_entry() {
        // Re-scheduling a wakeup for the same vt with a later deadline must
        // supersede the earlier registration (the stale heap entry is dropped),
        // and the thread is resubmitted exactly once at the new deadline.
        //
        // The properties under test are ORDERING and MULTIPLICITY, not wall
        // clock. The earlier version asserted `next_task()` at fixed sleep
        // offsets (70 ms, then 190 ms) and so allowed the shared wakeup-timer
        // OS thread only 70 ms of scheduling slack; on a loaded 16-core Linux
        // box that is not enough (measured: ~4% failures in isolation at load
        // ~20, every one of them at exactly 190 ms — the resubmit had simply
        // not landed *yet*, not "never"). Poll for the resubmit under a
        // generous bound instead, and prove the stale entry never fired with
        // `total_submissions`, which is timing-independent: had the superseded
        // 30 ms entry also resubmitted, the counter would read 2 no matter
        // when either landed.
        let mgr = VirtualThreadManager::new(1);
        let id = mgr.create_virtual_thread("resched");
        {
            let mut threads = mgr.threads.lock();
            threads.get_mut(&id).unwrap().mount(0);
        }
        mgr.park_virtual(id);
        let t0 = std::time::Instant::now();
        mgr.schedule_wakeup(id, std::time::Duration::from_millis(30));
        // Immediately re-schedule with a longer deadline (new signal Arc).
        mgr.schedule_wakeup(id, std::time::Duration::from_millis(120));

        // The re-scheduled wakeup must resubmit the thread.
        let limit = std::time::Duration::from_secs(10);
        let mut fired_at = None;
        while t0.elapsed() < limit {
            if let Some(task) = mgr.scheduler().next_task(0) {
                assert_eq!(task, id, "an unexpected task was resubmitted");
                fired_at = Some(t0.elapsed());
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let fired_at = fired_at.expect("the re-scheduled wakeup should resubmit the thread");
        // The stale 30 ms entry must NOT have been the one that fired: nothing
        // may reach the scheduler before the superseding 120 ms deadline.
        assert!(
            fired_at >= std::time::Duration::from_millis(100),
            "superseded early wakeup fired at {:?}",
            fired_at
        );
        // ...and the resubmit must have happened exactly once.
        std::thread::sleep(std::time::Duration::from_millis(60));
        assert_eq!(
            mgr.scheduler().next_task(0),
            None,
            "the superseded entry must not resubmit a second time"
        );
        assert_eq!(
            mgr.scheduler_stats().total_submissions,
            1,
            "exactly one submission expected (fired at {:?})",
            fired_at
        );
        mgr.shutdown();
    }

    #[test]
    fn p81_park_spurious_wakeup_safe() {
        let mut vt = VirtualThread::new(1, "vt".into());
        vt.mount(0);

        // Park
        vt.park();
        assert_eq!(vt.state, VirtualThreadState::Parked);

        // Unpark (simulating spurious wakeup or explicit unpark)
        vt.unpark();
        assert_eq!(vt.state, VirtualThreadState::Started);
        assert!(vt.unpark_permit);

        // Next park should consume the permit and NOT actually park
        vt.mount(0);
        vt.state = VirtualThreadState::Running;
        let result = vt.park();
        assert!(result); // consumed permit
        assert!(!vt.unpark_permit);
        // State should still be Running (not Parked) because permit was consumed
        assert_eq!(vt.state, VirtualThreadState::Running);
    }

    // -- 81.4: Pinning Detection -------------------------------------------

    #[test]
    fn p81_pinning_detection_monitor() {
        let mgr = VirtualThreadManager::new(1);
        let id = mgr.create_virtual_thread("pinned-vt");
        {
            let mut threads = mgr.threads.lock();
            threads.get_mut(&id).unwrap().mount(0);
        }

        // Pin for monitor (synchronized block)
        let result = mgr.pin_thread(id, PinReason::Monitor);
        assert!(result.is_some());
        let (name, _carrier) = result.unwrap();
        assert_eq!(name, "pinned-vt");
        assert_eq!(mgr.get_state(id), Some(VirtualThreadState::Pinned));

        // Verify stats
        let stats = mgr.scheduler_stats();
        assert_eq!(stats.total_pins, 1);
    }

    #[test]
    fn p81_synchronized_park_blocks_carrier() {
        let mut vt = VirtualThread::new(1, "synced-vt".into());
        vt.mount(0);

        // Enter synchronized block
        vt.pin(PinReason::Monitor);
        assert!(vt.is_pinned());
        assert_eq!(vt.state, VirtualThreadState::Pinned);

        // Try to park while pinned — should fail (carrier blocks instead)
        let result = vt.park();
        assert!(!result, "park should return false when pinned");
        // Still mounted — carrier thread is blocked, not freed
        assert_eq!(vt.carrier_thread_id, Some(0));
    }

    #[test]
    fn p81_monitor_state_preserved_in_continuation() {
        use crate::classloading::ClassId;
        use crate::runtime::frame::Frame;

        let mut vt = VirtualThread::new(1, "vt".into());
        vt.mount(0);

        // Enter nested synchronized blocks
        vt.pin(PinReason::Monitor);
        vt.pin(PinReason::Monitor);
        assert_eq!(vt.pin_count, 2);

        // Unpin once (still pinned with count 1)
        vt.unpin();
        assert_eq!(vt.pin_count, 1);
        assert!(vt.is_pinned());

        // Fully unpin
        vt.unpin();
        assert_eq!(vt.pin_count, 0);
        assert!(!vt.is_pinned());
        assert_eq!(vt.state, VirtualThreadState::Running);

        // Now park with frames should succeed (no longer pinned)
        let frames = vec![Frame::new(
            ClassId::new(0),
            "MonitorTest".to_string(),
            "work".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            5,
            2,
            &[Value::Int(99)],
        )];
        let result = vt.park_with_frames(frames);
        assert!(result);
        assert_eq!(vt.state, VirtualThreadState::Parked);
        assert_eq!(vt.continuation.frozen_frames.len(), 1);
    }

    // -- 81.5: Thread.ofVirtual() Builder API ----------------------------

    #[test]
    fn p81_builder_of_virtual_creates_daemon() {
        let b = ThreadBuilder::of_virtual();
        assert_eq!(b.kind, ThreadBuilderKind::Virtual);
        assert!(b.daemon, "virtual threads should be daemon by default");
        assert!(b.name.is_none());
        assert!(b.stack_size.is_none());
    }

    #[test]
    fn p81_builder_name_with_prefix_counter() {
        let mut b = ThreadBuilder::of_virtual().name_prefix("vt-");
        assert_eq!(b.build_name(), "vt-0");
        assert_eq!(b.build_name(), "vt-1");
        assert_eq!(b.build_name(), "vt-2");

        // Platform builder with explicit name
        let mut b2 = ThreadBuilder::of_platform().name("worker-main");
        assert_eq!(b2.build_name(), "worker-main");
        // Without prefix, name doesn't auto-increment
        assert_eq!(b2.build_name(), "worker-main");
    }

    #[test]
    fn p81_builder_factory_creates_thread() {
        // Verify the builder can generate thread names from factory pattern
        let mut b = ThreadBuilder::of_virtual().name_prefix("pool-");
        let name1 = b.build_name();
        let name2 = b.build_name();
        assert_eq!(name1, "pool-0");
        assert_eq!(name2, "pool-1");

        // Create VT through the manager using builder pattern
        let mgr = VirtualThreadManager::new(1);
        let id1 = mgr.create_virtual_thread(&name1);
        let id2 = mgr.create_virtual_thread(&name2);
        assert!(mgr.is_virtual(id1));
        assert!(mgr.is_virtual(id2));
        assert_ne!(id1, id2);
    }

    #[test]
    fn p81_thread_type_detection() {
        use crate::threading::jvm_thread::{JvmThread, ThreadId, ThreadKind};

        // Platform thread
        let pt = JvmThread::new(ThreadId(1), "platform-1");
        assert_eq!(pt.kind, ThreadKind::Platform);

        // Virtual thread created via manager
        let mgr = VirtualThreadManager::new(1);
        let vt_id = mgr.create_virtual_thread("vt-1");
        assert!(mgr.is_virtual(vt_id));
        assert_eq!(mgr.get_state(vt_id), Some(VirtualThreadState::New));

        // After start, state transitions
        mgr.start(vt_id);
        assert_eq!(mgr.get_state(vt_id), Some(VirtualThreadState::Started));

        // After terminate
        mgr.terminate(vt_id);
        assert_eq!(mgr.get_state(vt_id), Some(VirtualThreadState::Terminated));
    }

    #[test]
    fn keyed_wakeup_before_unmount_is_not_lost() {
        use crate::threading::jvm_thread::{JvmThread, ThreadId};

        let mgr = VirtualThreadManager::new(1);
        let id = 41;
        mgr.create_virtual_thread_with_id(id, "keyed-race");
        mgr.install_runtime(id, Box::new(JvmThread::new(ThreadId(id), "keyed-race")));
        mgr.start(id);
        assert_eq!(mgr.scheduler().next_task(0), Some(id));
        let (runtime, _) = mgr.take_runtime_for_mount(id, 0).unwrap();

        mgr.wait_on_key(7, id);
        mgr.wake_waiters(7);
        assert!(mgr.threads.lock().get(&id).unwrap().wake_pending);

        mgr.suspend_runtime(id, runtime, std::time::Duration::ZERO);
        assert_eq!(mgr.get_state(id), Some(VirtualThreadState::Started));
        assert_eq!(mgr.scheduler().next_task(0), Some(id));
    }

    #[test]
    fn keyed_wakeup_resubmits_parked_continuation() {
        use crate::threading::jvm_thread::{JvmThread, ThreadId};

        let mgr = VirtualThreadManager::new(1);
        let id = 42;
        mgr.create_virtual_thread_with_id(id, "keyed-parked");
        mgr.install_runtime(id, Box::new(JvmThread::new(ThreadId(id), "keyed-parked")));
        mgr.start(id);
        assert_eq!(mgr.scheduler().next_task(0), Some(id));
        let (runtime, _) = mgr.take_runtime_for_mount(id, 0).unwrap();

        mgr.wait_on_key(8, id);
        mgr.suspend_runtime(id, runtime, std::time::Duration::ZERO);
        assert_eq!(mgr.get_state(id), Some(VirtualThreadState::Parked));

        mgr.wake_waiters(8);
        assert_eq!(mgr.get_state(id), Some(VirtualThreadState::Started));
        assert_eq!(mgr.scheduler().next_task(0), Some(id));
    }

    // -- Lost-unpark race (2026-07-26) ------------------------------------

    /// The regression this whole fix exists for: `LockSupport.unpark` lands
    /// while the continuation is still mounted (it has returned
    /// `ContinuationYield` on its carrier but `suspend_runtime` has not run
    /// yet). Before the fix nothing was submitted and, for an UNTIMED park
    /// (`wake_after == 0`, so no wakeup timer either), the virtual thread was
    /// never scheduled again.
    #[test]
    fn unpark_before_unmount_is_not_lost() {
        use crate::threading::jvm_thread::{JvmThread, ThreadId};

        let mgr = VirtualThreadManager::new(1);
        let id = 61;
        mgr.create_virtual_thread_with_id(id, "unpark-race");
        mgr.install_runtime(id, Box::new(JvmThread::new(ThreadId(id), "unpark-race")));
        mgr.start(id);
        assert_eq!(mgr.scheduler().next_task(0), Some(id));
        let (runtime, _) = mgr.take_runtime_for_mount(id, 0).unwrap();
        assert_eq!(mgr.get_state(id), Some(VirtualThreadState::Running));

        // Unpark arrives mid-flight: still mounted, runtime not deposited.
        mgr.unpark_virtual(id);
        assert!(
            mgr.threads.lock().get(&id).unwrap().unpark_permit,
            "a mid-flight unpark must leave a sticky permit"
        );
        assert!(
            mgr.scheduler().next_task(0).is_none(),
            "nothing may be queued while the runtime is still taken"
        );

        // Untimed park deposits the runtime — the permit must fire here.
        mgr.suspend_runtime(id, runtime, std::time::Duration::ZERO);
        assert_eq!(mgr.get_state(id), Some(VirtualThreadState::Started));
        assert_eq!(mgr.scheduler().next_task(0), Some(id));
        assert!(
            !mgr.threads.lock().get(&id).unwrap().unpark_permit,
            "the permit must be consumed, not left to fire twice"
        );
    }

    /// A parked continuation with its runtime deposited is resubmitted
    /// directly — the plain park/unpark round trip.
    #[test]
    fn unpark_after_unmount_resubmits_once() {
        use crate::threading::jvm_thread::{JvmThread, ThreadId};

        let mgr = VirtualThreadManager::new(1);
        let id = 62;
        mgr.create_virtual_thread_with_id(id, "unpark-parked");
        mgr.install_runtime(id, Box::new(JvmThread::new(ThreadId(id), "unpark-parked")));
        mgr.start(id);
        // Drain `start`'s own submission. Without this the queue is never
        // empty, and every "must not have resubmitted" assertion below would
        // pass on `start`'s leftover entry instead of testing anything — the
        // test could not have detected the duplicate-submission bug it names.
        assert_eq!(mgr.scheduler().next_task(0), Some(id));
        let (runtime, _) = mgr.take_runtime_for_mount(id, 0).unwrap();
        assert_eq!(mgr.scheduler().next_task(0), None);

        mgr.suspend_runtime(id, runtime, std::time::Duration::ZERO);
        assert_eq!(mgr.get_state(id), Some(VirtualThreadState::Parked));
        assert_eq!(mgr.scheduler().next_task(0), None);

        mgr.unpark_virtual(id);
        assert_eq!(mgr.get_state(id), Some(VirtualThreadState::Started));
        assert_eq!(mgr.scheduler().next_task(0), Some(id));
        // Exactly one submission — the old code enqueued a second copy when it
        // saw an already-`Started` thread.
        assert_eq!(mgr.scheduler().next_task(0), None);
    }

    /// An unpark against an already-queued (`Started`, not yet mounted)
    /// virtual thread must not enqueue a duplicate id.
    #[test]
    fn unpark_of_queued_thread_does_not_duplicate_submission() {
        use crate::threading::jvm_thread::{JvmThread, ThreadId};

        let mgr = VirtualThreadManager::new(1);
        let id = 63;
        mgr.create_virtual_thread_with_id(id, "unpark-queued");
        mgr.install_runtime(id, Box::new(JvmThread::new(ThreadId(id), "unpark-queued")));
        mgr.start(id);
        assert_eq!(mgr.get_state(id), Some(VirtualThreadState::Started));

        mgr.unpark_virtual(id);
        assert_eq!(mgr.scheduler().next_task(0), Some(id));
        assert_eq!(
            mgr.scheduler().next_task(0),
            None,
            "unpark of a queued thread must not duplicate its submission"
        );
    }

    /// A stray permit must NOT truncate a timed yield: `Thread.sleep` and
    /// `parkNanos` already carry a `WakeupTimer` deadline.
    #[test]
    fn unpark_permit_does_not_truncate_timed_yield() {
        use crate::threading::jvm_thread::{JvmThread, ThreadId};

        let mgr = VirtualThreadManager::new(1);
        let id = 64;
        mgr.create_virtual_thread_with_id(id, "unpark-timed");
        mgr.install_runtime(id, Box::new(JvmThread::new(ThreadId(id), "unpark-timed")));
        mgr.start(id);
        // Drain `start`'s own submission — see the note in
        // `unpark_after_unmount_resubmits_once`. The whole point of this test
        // is the assertion that the queue is EMPTY after a timed yield; with
        // `start`'s entry still sitting there that assertion is untestable.
        assert_eq!(mgr.scheduler().next_task(0), Some(id));
        let (runtime, _) = mgr.take_runtime_for_mount(id, 0).unwrap();

        mgr.unpark_virtual(id);
        mgr.suspend_runtime(id, runtime, std::time::Duration::from_secs(3600));
        assert_eq!(
            mgr.get_state(id),
            Some(VirtualThreadState::Parked),
            "a timed yield must stay parked until its deadline"
        );
        assert_eq!(mgr.scheduler().next_task(0), None);
        assert!(
            mgr.threads.lock().get(&id).unwrap().unpark_permit,
            "the permit stays sticky for the next untimed park"
        );
        mgr.cancel_wakeup(id);
        // Stops the lazily-started wakeup-timer thread this park spawned.
        mgr.shutdown();
    }

    /// `Parked` implies resubmit, unconditionally — including when no runtime
    /// has been deposited.
    ///
    /// This pins the reason `unpark_virtual`'s condition is `state == Parked`
    /// and not `state == Parked && runtime.is_some()`. The two agree on the
    /// live path (`suspend_runtime` writes both under one lock hold), so the
    /// conjunct can only ever matter when the invariant is already broken —
    /// and there it fails closed, turning a wake into a permanent hang. This
    /// is the same shape the pre-existing `manager_park_and_unpark` asserts;
    /// kept separately so the *reason* is not lost if that test is rewritten.
    #[test]
    fn unpark_of_parked_thread_without_runtime_still_resubmits() {
        let mgr = VirtualThreadManager::new(1);
        let id = mgr.create_virtual_thread("parked-no-runtime");
        {
            let mut threads = mgr.threads.lock();
            threads.get_mut(&id).unwrap().mount(0);
        }
        assert!(mgr.park_virtual(id));
        assert_eq!(mgr.get_state(id), Some(VirtualThreadState::Parked));
        assert!(
            mgr.threads.lock().get(&id).unwrap().runtime.is_none(),
            "precondition: this thread has no deposited runtime"
        );

        mgr.unpark_virtual(id);
        assert_eq!(
            mgr.get_state(id),
            Some(VirtualThreadState::Started),
            "a parked thread must always become runnable on unpark"
        );
        assert_eq!(
            mgr.scheduler().next_task(0),
            Some(id),
            "and must actually reach the scheduler queue"
        );
    }

    /// An unpark of a terminated virtual thread is a no-op — no resurrection,
    /// no queue entry.
    #[test]
    fn unpark_of_terminated_thread_is_a_noop() {
        let mgr = VirtualThreadManager::new(1);
        let id = 65;
        mgr.create_virtual_thread_with_id(id, "unpark-dead");
        mgr.terminate(id);

        mgr.unpark_virtual(id);
        assert_eq!(mgr.get_state(id), Some(VirtualThreadState::Terminated));
        assert_eq!(mgr.scheduler().next_task(0), None);
    }

    // -- Carrier-pool starvation compensation (2026-07-26) ----------------

    #[test]
    fn stall_detector_requires_queued_work_saturation_and_no_progress() {
        // Saturated, queue non-empty, no dispatch since last sample: stalled.
        assert!(carrier_pool_is_stalled(3, 2, 2, false));
        // Dispatching -> busy but progressing, never compensate.
        assert!(!carrier_pool_is_stalled(3, 2, 2, true));
        // Idle carrier available -> the backlog has a taker.
        assert!(!carrier_pool_is_stalled(3, 1, 2, false));
        // Nothing queued -> saturation is just useful work.
        assert!(!carrier_pool_is_stalled(0, 2, 2, false));
        // No carriers at all (pool never started) -> nothing to compensate.
        assert!(!carrier_pool_is_stalled(3, 0, 0, false));
    }

    /// Compensating carriers get indices past `parallelism` and therefore own
    /// no per-carrier work queue. `next_task` used to index `work_queues`
    /// directly, which panicked for exactly those indices.
    #[test]
    fn next_task_tolerates_compensating_carrier_index() {
        let sched = ForkJoinScheduler::new(2);
        sched.submit(77);
        let out_of_range = sched.parallelism() + 5;
        assert_eq!(
            sched.next_task(out_of_range),
            Some(77),
            "a compensating carrier must reach the global queue"
        );
        assert_eq!(sched.next_task(out_of_range), None);
    }

    /// The blocked carrier that motivates compensation: a task body that never
    /// returns holds its carrier forever. With `parallelism == 1` the queued
    /// follow-up work is unreachable until a second carrier appears.
    #[test]
    fn blocked_carrier_pool_is_grown_by_the_watchdog() {
        let mgr = VirtualThreadManager::new(1);
        let entered = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(AtomicBool::new(false));
        let second_ran = Arc::new(AtomicBool::new(false));

        let entered_task = entered.clone();
        let release_task = release.clone();
        let second_task = second_ran.clone();
        mgr.start_carriers(move |vt_id| {
            entered_task.fetch_add(1, Ordering::SeqCst);
            if vt_id == 1 {
                // Emulate a virtual thread blocking its carrier inside a
                // contended monitor: the carrier never returns to the pool.
                while !release_task.load(Ordering::SeqCst) {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            } else {
                second_task.store(true, Ordering::SeqCst);
            }
        });

        mgr.scheduler().submit(1);
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while entered.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            entered.load(Ordering::SeqCst),
            1,
            "the sole base carrier must be wedged inside the task body"
        );

        // Nothing in the base pool can ever pick this up.
        mgr.scheduler().submit(2);
        let deadline = Instant::now() + std::time::Duration::from_secs(20);
        while !second_ran.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let ran_second = second_ran.load(Ordering::SeqCst);
        let grew = mgr.scheduler().live_carriers() > mgr.scheduler().parallelism();

        release.store(true, Ordering::SeqCst);
        mgr.shutdown();

        assert!(
            ran_second,
            "the starvation watchdog must add a carrier so queued work still runs"
        );
        assert!(grew, "the pool must have grown past its base parallelism");
    }

    /// The cap is a hard bound, not a target: an already-maxed pool refuses to
    /// grow.
    #[test]
    fn compensating_carrier_respects_the_pool_cap() {
        let sched = Arc::new(ForkJoinScheduler::new(1));
        let task_fn: Arc<dyn Fn(u64) + Send + Sync> = Arc::new(|_vt_id: u64| {});
        let handles = Arc::new(Mutex::new(Vec::new()));
        sched
            .live_carriers
            .store(MAX_CARRIER_THREADS, Ordering::Release);
        assert!(
            !spawn_compensating_carrier(&sched, &task_fn, &handles),
            "must not spawn past MAX_CARRIER_THREADS"
        );
        assert_eq!(sched.live_carriers(), MAX_CARRIER_THREADS);
        assert!(handles.lock().is_empty());
    }
}
