// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
    /// The frame's `Frame::redefine_stamp` (raw, obsolete bit included): which
    /// generation of its class's constant pool `code` indexes. `0` for a
    /// test-only snapshot with no code.
    pub redefine_stamp: u64,
    /// The frame's `Frame::replaced_body`: what a frame moved onto a body its
    /// class's redefinition replaced carries besides its code. `None` for
    /// every other frame.
    pub replaced_body: Option<Arc<crate::runtime::frame::ReplacedBody>>,
    /// The frame's monitors (interpreter round i6 wave 118, lane L6): its
    /// `Frame::monitor_on_exit` (an `ACC_SYNCHRONIZED` method's monitor,
    /// released at its return or unwind), its block-monitor record
    /// (`Frame::held_monitors`, oldest first) and that record's understated
    /// mark. Before wave 118 a freeze dropped all three, so a thawed frame
    /// holding a monitor never released a synchronized method's lock and
    /// threw `IllegalMonitorStateException` at a block's `monitorexit` under
    /// `CRATONVM_MONITOREXIT_FRAME_STRICT`. Like `locals`, these are not GC
    /// roots while frozen: no production yield freezes into a `FrozenFrame`
    /// (a bound virtual thread's frames stay in its boxed `JvmThread`), only
    /// the obsolete-frame tests and this module's do.
    pub monitor_on_exit: Option<crate::types::ObjectRef>,
    pub held_monitors: Vec<crate::types::ObjectRef>,
    pub held_monitors_understated: bool,
    /// The record's mark (`HeldMonitors::mark_code`: `2 * producer +
    /// understated`), `None` for an unmarked record (interpreter round i7 wave
    /// 136, lane L6). The thaw puts the same mark back
    /// (`HeldMonitors::from_frozen`): a trusted seed's provenance was dropped
    /// before, and the census then miscounted the thawed record.
    pub held_monitors_mark: Option<u8>,
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
            redefine_stamp: 0,
            replaced_body: None,
            monitor_on_exit: None,
            held_monitors: Vec::new(),
            held_monitors_understated: false,
            held_monitors_mark: None,
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
    /// The thread's ONE `LockSupport` permit (HotSpot's
    /// `VirtualThread.parkPermit`), whichever way it parks next: an
    /// unmounting park consumes it in [`VirtualThreadManager::suspend_runtime`],
    /// a park on the carrier in [`VirtualThreadManager::begin_carrier_park`].
    pub unpark_permit: bool,
    /// Set while the thread parks on its CARRIER (pinned: a held monitor, or a
    /// native beneath the park) between [`VirtualThreadManager::begin_carrier_park`]
    /// and [`VirtualThreadManager::end_carrier_park`]; an `unpark` then wakes
    /// the thread's `ParkState` as well ([`VirtualThreadManager::unpark_virtual_waking`]).
    /// Interpreter round i1 wave 28, lane L4.
    pub carrier_park: bool,
    /// A condition became ready after a native registered this continuation
    /// but before the carrier finished unmounting it.
    pub wake_pending: bool,
    /// Heap-resident execution state while unmounted. The `JvmThread` is
    /// boxed so its address remains stable for precise GC scanning across
    /// carrier migration; its `frames` are the continuation stack chunks.
    pub runtime: Option<Box<crate::threading::JvmThread>>,
    pub execution_started: bool,
    /// A debugger is reading the frames of the unmounted [`Self::runtime`]
    /// through its stable address (interpreter round i1 wave 48, lane L1;
    /// [`VirtualThreadManager::begin_debugger_read`]): a mount waits until
    /// the read ends ([`VirtualThreadManager::take_runtime_for_mount`]).
    pub debugger_reading: bool,
    /// A JDK-driven continuation's thread (`vm::jdk_continuation`): the JDK's
    /// own scheduler mounts it, so nothing here ever submits it to this
    /// manager's carrier pool (a park, an unpark, an interrupt, a wake of it
    /// is the JDK's business; this entry only holds its unmounted state).
    pub jdk: bool,
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
            carrier_park: false,
            wake_pending: false,
            runtime: None,
            execution_started: false,
            debugger_reading: false,
            jdk: false,
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
    /// Test only (gci i18/vp, the i18a red of
    /// `blocked_carrier_pool_is_grown_by_the_watchdog`): the watchdog of this
    /// scheduler ignores the PROCESS-wide pause signal
    /// (`gc_barrier::stw_pause_state`). In the `vm` unit-test binary every
    /// other test's VM pauses count as "this VM paused", so a scheduler test
    /// running beside GC tests can see every 50 ms sample discarded (or a
    /// pause in flight for the whole window) and never grow.
    #[cfg(test)]
    ignore_process_pauses: AtomicBool,
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
            #[cfg(test)]
            ignore_process_pauses: AtomicBool::new(false),
        }
    }

    /// Does this scheduler's watchdog discount the process-wide pause signal?
    /// Always `true` outside tests (see the field).
    #[inline]
    fn watchdog_honours_process_pauses(&self) -> bool {
        #[cfg(test)]
        {
            !self.ignore_process_pauses.load(Ordering::Acquire)
        }
        #[cfg(not(test))]
        {
            true
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
                let (pause_epoch, pause_now) = crate::threading::gc_barrier::stw_pause_state();
                let paused_in_interval = (pause_now || pause_epoch != last_pause_epoch)
                    && scheduler.watchdog_honours_process_pauses();
                last_pause_epoch = pause_epoch;
                // `CRATONVM_DBG_CARRIER=1` -- one line per sample, printed
                // BEFORE the skip above so a pause-contaminated sample is
                // visible as such rather than silently absent. The pool
                // climbing away from its base size while `dispatch` sits still
                // is the signature of the loop described above, and without
                // this reading it presents only as "the VM hung".
                if None::<std::ffi::OsString>.is_some() {
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

/// An unparked continuation waiting for a carrier is RUNNABLE, as HotSpot
/// reports an unparked virtual thread. Its `Thread.getState()` comes from the
/// deposited runtime's `java_state` (read while `in_blocked_region` is up,
/// which it stays until the remount), and the yield stored WAITING /
/// TIMED_WAITING there (`resume_virtual_continuation`); the remount resets it
/// too, this only closes the queued window (interpreter round i1 wave 23,
/// lane L7).
fn mark_unmounted_runnable(vt: &VirtualThread) {
    if let Some(runtime) = vt.runtime.as_ref() {
        runtime
            .gc_block_state
            .java_state
            .store(0, Ordering::Release);
    }
}

/// A mount of a continuation a debugger is reading waits here between two
/// looks (interpreter round i1 wave 48, lane L1): the read is one listing,
/// short, and runs on another thread.
#[cold]
#[inline(never)]
fn wait_for_debugger_read() {
    std::thread::sleep(std::time::Duration::from_micros(50));
}

// ---------------------------------------------------------------------------
// VirtualThreadManager
// ---------------------------------------------------------------------------

/// gci i14/ve: the deposited, not debugger-read JDK continuations of a
/// locked record map ([`VirtualThreadManager::deposited_jdk_continuation_ids`]).
fn deposited_jdk_ids(threads: &FxHashMap<u64, VirtualThread>) -> Vec<u64> {
    threads
        .iter()
        .filter(|(_, vt)| vt.jdk && vt.runtime.is_some() && !vt.debugger_reading)
        .map(|(&id, _)| id)
        .collect()
}

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
    /// Has this manager ever tracked a virtual thread? Raised (`SeqCst`)
    /// before the first one is inserted into `threads`, never lowered. Lets
    /// `Unsafe.unpark` skip the `threads` lock for a platform thread in a VM
    /// that never made a virtual thread ([`Self::may_have_virtual_threads`];
    /// interpreter round i3 wave 72, lane L6, `CRATONVM_UNPARK_ONE_LOOKUP`).
    ever_tracked: AtomicBool,
    /// Does the JDK drive this VM's virtual threads
    /// (`VmConfig::jdk_continuations` over a real JDK 21+; set once at VM
    /// init, `vm::jdk_continuation`)? Then this manager holds the JDK
    /// continuations' unmounted execution states ([`Self::deposit_continuation`])
    /// and lends their carriers a runner OS thread ([`Self::continuation_runners`]),
    /// and its own carrier pool is never started.
    jdk_continuations: bool,
    /// The runner OS threads a JDK continuation's slice runs on while its
    /// carrier waits in `Continuation.enterSpecial` (`vm::jdk_continuation`).
    continuation_runners: crate::vm::jdk_continuation::ContinuationRunners,
    /// gci i11/vt item 4 (`CRATONVM_VT_REAP_TERMINATED`, default ON since
    /// 2026-10-17, read once per VM at init): forget a JDK continuation's record once its slice
    /// ended ([`Self::forget_terminated`]) instead of keeping one
    /// `VirtualThread` per virtual thread ever started.
    reap_terminated: bool,
    /// `CRATONVM_VTHREAD_RETIRE_MANAGER_ENTRIES` (`VmConfig::vthread_retire_manager_entries`,
    /// default on; interpreter round i4 wave 95, lane L6; page
    /// `i94-L6-terminated-jdk-virtual-threads-stay-in-the-manager-map`):
    /// `Some(cap)` -- [`Self::terminate`] keeps at most `2 * cap` terminated
    /// JDK continuations' entries, purging back to the newest `cap` (by id).
    /// `None` (`=0`): every entry is kept for the life of the VM, as before.
    retired_jdk_entry_cap: Option<usize>,
    /// How many entries of `threads` are terminated JDK continuations'. Read
    /// and written only under the `threads` lock.
    retired_jdk_entries: AtomicUsize,
    /// Per-VM event-loop manager (V21), shut down when this manager drops.
    event_loops: super::event_loop::EventLoopManager,
}

/// How many terminated JDK continuations' entries the manager keeps (see
/// `VirtualThreadManager::retired_jdk_entry_cap`): four times the thread
/// registry's terminated tail (`TERMINATED_THREAD_ENTRY_CAP`, 256), so an id
/// the registry still answers for is, short of a contrived death order, one
/// the manager still answers `is_virtual` for.
pub const RETIRED_JDK_ENTRY_CAP: usize = 1024;

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
            ever_tracked: AtomicBool::new(false),
            jdk_continuations: false,
            continuation_runners: Default::default(),
            reap_terminated: false,
            retired_jdk_entry_cap: None,
            retired_jdk_entries: AtomicUsize::new(0),
            event_loops: super::event_loop::EventLoopManager::new(),
        }
    }

    /// See [`Self::jdk_continuations`]; VM init's builder step.
    pub fn with_jdk_continuations(mut self, on: bool) -> Self {
        self.jdk_continuations = on;
        self
    }

    /// See [`Self::reap_terminated`]; VM init's builder step.
    pub fn with_reap_terminated(mut self, on: bool) -> Self {
        self.reap_terminated = on;
        self
    }

    /// Does this VM forget terminated JDK continuations
    /// (`CRATONVM_VT_REAP_TERMINATED`; gci i11/vt item 4)?
    #[inline]
    pub fn reap_terminated(&self) -> bool {
        self.reap_terminated
    }

    /// Drop the record of JDK continuation `vt_id` after [`Self::terminate`]
    /// (gci i11/vt item 4): `true` when it was removed. Only a TERMINATED
    /// JDK continuation's, with no execution state deposited and no
    /// debugger reading it -- a bound virtual thread's record is this
    /// manager's own scheduling state (joins, keyed waits) and stays. After
    /// it, [`Self::is_virtual`] answers `false` for the dead id, as for any
    /// unknown one; every reader of that answer treats a dead thread as a
    /// no-op target (an interrupt, an unpark) or filters dead threads
    /// itself, and the caller skips the reap while a JDWP debugger is
    /// configured (`ThreadReference.IsVirtual` of a dead thread).
    pub fn forget_terminated(&self, vt_id: u64) -> bool {
        let mut threads = self.threads.lock();
        let reapable = threads.get(&vt_id).is_some_and(|vt| {
            vt.jdk
                && vt.state == VirtualThreadState::Terminated
                && vt.runtime.is_none()
                && !vt.debugger_reading
        });
        if reapable {
            threads.remove(&vt_id);
            // gci i20/mr finding 5 (applied by i20/st): `terminate` counted this
            // entry in `retired_jdk_entries`; uncount it, or the retire cap's
            // purge walks every entry each `2 * cap` deaths for nothing under
            // the reap.
            if self.retired_jdk_entry_cap.is_some() {
                let n = self.retired_jdk_entries.load(Ordering::Relaxed);
                self.retired_jdk_entries
                    .store(n.saturating_sub(1), Ordering::Relaxed);
            }
        }
        reapable
    }

    /// gci i14/ve (`CRATONVM_GC_VT_PARKED_MIRROR_KEYED`): the ids of the JDK
    /// continuations whose unmounted state is deposited here right now and
    /// that no debugger is reading -- the only ones a collection pause may
    /// key by their mirrors (`ThreadRegistry::arm_mirror_keyed_gather`). Read
    /// inside the pause; a state deposited or taken concurrently by a runner
    /// belongs to a slice whose carrier holds the mirror on its own stack, so
    /// the snapshot either way can only over-retain.
    pub(crate) fn deposited_jdk_continuation_ids(&self) -> Vec<u64> {
        deposited_jdk_ids(&self.threads.lock())
    }

    /// [`Self::deposited_jdk_continuation_ids`] for a caller INSIDE a pause:
    /// never blocks. `None` when the `threads` lock is held elsewhere (a
    /// stopped peer may hold it, and waiting for it would deadlock the
    /// pause); the caller then keys nothing this pause.
    pub(crate) fn try_deposited_jdk_continuation_ids(&self) -> Option<Vec<u64>> {
        self.threads.try_lock().map(|threads| deposited_jdk_ids(&threads))
    }

    /// gci i14/ve: take the deposited state of JDK continuation `vt_id`, whose
    /// mirror a collection found unreachable (its registry entry is already
    /// dead, `ThreadRegistry::sweep_mirror_keyed_parked`), for
    /// `jdk_continuation::retire_collected_continuations` to release. `None`
    /// when it is not a JDK continuation's, has no deposited state, or a
    /// debugger is reading it (left in place: nothing will mount it again).
    pub(crate) fn take_collected_continuation(
        &self,
        vt_id: u64,
    ) -> Option<Box<crate::threading::JvmThread>> {
        let mut threads = self.threads.lock();
        let vt = threads.get_mut(&vt_id)?;
        if !vt.jdk || vt.debugger_reading {
            return None;
        }
        vt.runtime.take()
    }

    /// See `retired_jdk_entry_cap`; VM init's builder step
    /// (`CRATONVM_VTHREAD_RETIRE_MANAGER_ENTRIES`). A cap of 0 is read as 1.
    pub fn with_retired_jdk_entry_cap(mut self, cap: Option<usize>) -> Self {
        self.retired_jdk_entry_cap = cap.map(|c| c.max(1));
        self
    }

    /// Does the JDK drive this VM's virtual threads (`vm::jdk_continuation`)?
    #[inline]
    pub fn jdk_continuations(&self) -> bool {
        self.jdk_continuations
    }

    /// The runner OS threads of the JDK continuations (`vm::jdk_continuation`).
    pub(crate) fn continuation_runners(&self) -> &crate::vm::jdk_continuation::ContinuationRunners {
        &self.continuation_runners
    }

    /// Hold the unmounted execution state of JDK continuation `vt_id` (a
    /// `java.lang.VirtualThread`'s, `vm::jdk_continuation`) until its next
    /// `Continuation.enterSpecial` takes it ([`Self::take_runtime_for_mount`]).
    /// Unlike [`Self::suspend_runtime`] nothing is scheduled: the JDK's own
    /// scheduler resubmits the thread. The caller has already retired its
    /// TLAB and then deposited its roots (the state is GC-blocked).
    pub fn deposit_continuation(&self, vt_id: u64, mut runtime: Box<crate::threading::JvmThread>) {
        // A no-op backstop (`Tlab::retire` is idempotent): the caller retires
        // BEFORE its deposit raises `in_blocked_region` (gci i11/vt finding 1).
        // A retire here, after the flag, races a pause that reads the tail
        // through the published `tlab_addr` -- it must never be the first.
        runtime.tlab.retire();
        let mut threads = self.threads.lock();
        if let Some(vt) = threads.get_mut(&vt_id) {
            vt.state = VirtualThreadState::Parked;
            vt.continuation.state = ContinuationState::Suspended;
            vt.continuation.yield_count = vt.continuation.yield_count.saturating_add(1);
            vt.unmount();
            vt.runtime = Some(runtime);
        } else {
            // gci i13/vg (review): no record to hold the state. Dropping the
            // box here would free a `JvmThread` whose registry entry is still
            // alive and still publishes its TLAB / `JvmThread` addresses (the
            // yield withdraws neither), which the next pause reads
            // (`collect_reserved_tlab_tails`, the take-over's frame walk).
            // Not reachable by reading (the record is created before the first
            // slice and reaped only once terminated); leak rather than free.
            tracing::warn!(
                "deposit_continuation: no manager record for virtual thread {vt_id}; \
                 its state is kept (leaked) instead of freed under a live registry entry"
            );
            let _ = Box::leak(runtime);
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
        self.ever_tracked.store(true, Ordering::SeqCst);
        self.threads.lock().insert(id, vt);
        id
    }

    /// Register a VM thread id as the virtual-thread id. Runtime integration
    /// uses the registry id directly so JFR, GC, park/unpark, and Java mirror
    /// lookups all share one stable identity.
    pub fn create_virtual_thread_with_id(&self, id: u64, name: &str) {
        self.ever_tracked.store(true, Ordering::SeqCst);
        let mut threads = self.threads.lock();
        threads
            .entry(id)
            .or_insert_with(|| VirtualThread::new(id, name.to_string()));
        self.next_id
            .fetch_max(id.saturating_add(1), Ordering::Relaxed);
    }

    /// Register JDK continuation `id` (a `java.lang.VirtualThread`'s,
    /// `vm::jdk_continuation`) with its first execution state, MOUNTED on
    /// carrier `carrier_id`: the state goes straight to the runner the caller
    /// lends the carrier, and comes back through [`Self::deposit_continuation`].
    pub fn create_jdk_continuation(&self, id: u64, name: &str, carrier_id: u64) {
        self.ever_tracked.store(true, Ordering::SeqCst);
        let mut threads = self.threads.lock();
        let vt = threads
            .entry(id)
            .or_insert_with(|| VirtualThread::new(id, name.to_string()));
        vt.jdk = true;
        vt.state = VirtualThreadState::Started;
        vt.execution_started = true;
        vt.mount(carrier_id);
        drop(threads);
        self.next_id
            .fetch_max(id.saturating_add(1), Ordering::Relaxed);
    }

    /// Register JDK continuation `id` with its first execution state, NOT
    /// mounted (a debugger named the started virtual thread before any
    /// carrier mounted it; `vm::jdk_continuation::ensure_continuation_thread`):
    /// its first `Continuation.enterSpecial` takes the state
    /// ([`Self::take_runtime_for_mount`]).
    pub fn register_unstarted_jdk_continuation(
        &self,
        id: u64,
        name: &str,
        runtime: Box<crate::threading::JvmThread>,
    ) {
        self.ever_tracked.store(true, Ordering::SeqCst);
        let mut threads = self.threads.lock();
        let vt = threads
            .entry(id)
            .or_insert_with(|| VirtualThread::new(id, name.to_string()));
        vt.jdk = true;
        vt.state = VirtualThreadState::Started;
        vt.runtime = Some(runtime);
        drop(threads);
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
        // Wave 48 (lane L1): a debugger reading the unmounted frames
        // (`begin_debugger_read`) finishes first. One load otherwise.
        while threads.get(&vt_id).is_some_and(|vt| vt.debugger_reading) {
            drop(threads);
            wait_for_debugger_read();
            threads = self.threads.lock();
        }
        let vt = threads.get_mut(&vt_id)?;
        // gci i24/vl (review): nothing to mount, nothing changes. A take that
        // found no state used to mount the record anyway -- `state` Running
        // (a terminated record revived, so `forget_terminated` and every
        // Terminated reader skipped it), the carrier id overwritten, and
        // `execution_started` set, so a later FIRST take of a state installed
        // after it answered `resumed` and ran no frames. The bound path's
        // spurious resubmits (`unpark_virtual`'s "Parked alone is the resubmit
        // condition") and `enterSpecial`'s no-state exit both reach it.
        let runtime = vt.runtime.take()?;
        let resumed = vt.execution_started;
        vt.execution_started = true;
        vt.mount(carrier_id);
        Some((runtime, resumed))
    }

    /// Begin a debugger's read of unmounted virtual thread `vt_id`'s frames
    /// (interpreter round i1 wave 48, lane L1): the address of its boxed
    /// execution state, which stays in place -- and is not mounted, so its
    /// frames are not run -- until [`Self::end_debugger_read`]. `None` when
    /// the thread is not one of this manager's, is mounted (no state
    /// deposited), or another read is in progress. The caller must be a
    /// registered mutator between two safepoint polls for the length of the
    /// read (no collection moves what the frames hold meanwhile), and must
    /// end the read.
    pub(crate) fn begin_debugger_read(
        &self,
        vt_id: u64,
    ) -> Option<*const crate::threading::JvmThread> {
        let mut threads = self.threads.lock();
        let vt = threads.get_mut(&vt_id)?;
        if vt.debugger_reading {
            return None;
        }
        let runtime: *const crate::threading::JvmThread = &**vt.runtime.as_ref()?;
        vt.debugger_reading = true;
        Some(runtime)
    }

    /// Is virtual thread `vt_id` unmounted, its continuation deposited here
    /// (interpreter round i1 wave 49, lane L1: a JDWP suspension's settle
    /// wait counts it stopped, `debug::await_suspension_settled`)?
    pub(crate) fn is_deposited(&self, vt_id: u64) -> bool {
        self.threads
            .lock()
            .get(&vt_id)
            .is_some_and(|vt| vt.runtime.is_some())
    }

    /// gci i25/vt: the mount count of JDK continuation `vt_id`'s record
    /// (`Continuation::mount_count`, raised by every
    /// [`Self::take_runtime_for_mount`] that mounts it) while its state is
    /// deposited here; `None` when it is mounted or unknown. Paired with
    /// [`Self::with_deposit_unmounted_since`]: "not mounted since".
    pub(crate) fn deposited_mount_count(&self, vt_id: u64) -> Option<u64> {
        let threads = self.threads.lock();
        let vt = threads.get(&vt_id)?;
        vt.runtime.as_ref()?;
        Some(vt.continuation.mount_count)
    }

    /// gci i25/vt: run `f` under the record lock when JDK continuation
    /// `vt_id`'s state is still deposited here with mount count `mounts`
    /// ([`Self::deposited_mount_count`]): it has not been mounted since, and
    /// cannot be until `f` returns (a mount takes this lock). `None`, and `f`
    /// not run, otherwise. `f` must not take this manager's lock, allocate,
    /// or wait for a pause.
    pub(crate) fn with_deposit_unmounted_since<R>(
        &self,
        vt_id: u64,
        mounts: u64,
        f: impl FnOnce() -> R,
    ) -> Option<R> {
        let threads = self.threads.lock();
        let vt = threads.get(&vt_id)?;
        if vt.runtime.is_none() || vt.continuation.mount_count != mounts {
            return None;
        }
        let answer = f();
        drop(threads);
        Some(answer)
    }

    /// End a read [`Self::begin_debugger_read`] began.
    pub(crate) fn end_debugger_read(&self, vt_id: u64) {
        if let Some(vt) = self.threads.lock().get_mut(&vt_id) {
            vt.debugger_reading = false;
        }
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
            // An interrupt raised while the continuation was still mounted
            // (after the yielding native's own interrupt check, before this
            // deposit) found the thread not `Parked`, so `interrupt_virtual`
            // could not resubmit it. Read the flag under the `threads` lock the
            // interrupter takes after setting it: either it is visible here,
            // or the interrupter's resubmit runs after this deposit and sees
            // `Parked`. A timed yield (a sleep) must not wait out its timer
            // either -- the remount raises the `InterruptedException`
            // (interpreter round i1 wave 24, lane L7).
            let interrupted = runtime.interrupted.load(Ordering::Acquire);
            // A `LockSupport` park, timed or not, returns on the permit
            // (wave 25, lane L7); a sleep or a keyed wait does not.
            let yielded_in_park = matches!(
                runtime.vt_yield,
                crate::threading::jvm_thread::VtYield::Park
            );
            // The frames stay in their boxed heap stack chunks. This preserves
            // precise GC visibility through the stable published JvmThread
            // address and avoids serializing raw ObjectRefs into an untracked
            // byte buffer.
            // A no-op backstop: `resume_virtual_continuation` retires the TLAB
            // before its deposit raises `in_blocked_region` (gci i11/vt
            // finding 1); a first retire here would race a pause.
            runtime.tlab.retire();
            vt.runtime = Some(runtime);
            if interrupted {
                vt.wake_pending = false;
                vt.unpark_permit = false;
                vt.state = VirtualThreadState::Started;
                mark_unmounted_runnable(vt);
                resubmit = true;
            } else if vt.wake_pending {
                vt.wake_pending = false;
                vt.state = VirtualThreadState::Started;
                resubmit = true;
            } else if vt.unpark_permit && yielded_in_park {
                // LOST-UNPARK FIX (2026-07-26): an `unpark` that landed while
                // this continuation was still mounted (or mid-unmount) left a
                // sticky permit rather than a submission — see
                // `unpark_virtual`. Consume it here, which is the earliest
                // point at which the runtime is deposited and the thread can
                // legally be resubmitted.
                //
                // Only a `LockSupport` park (`VtYield::Park`) returns on the
                // permit, timed or not (wave 25, lane L7). This used to be
                // "any UNTIMED yield": a timed `parkNanos` then waited out its
                // whole timeout (an `unpark` just before a `parkNanos(1 s)`
                // cost the full second; HotSpot returns at once), and an
                // untimed keyed wait (the synthetic `CountDownLatch.await`)
                // was resubmitted by a stray permit and, the remount running
                // on past its native, returned with the count still positive.
                // A `Thread.sleep` does not end on a permit either (it keeps
                // it; see `restore_unpark_permit`), so it still waits for its
                // `WakeupTimer`; a keyed wait for `wake_waiters`.
                vt.unpark_permit = false;
                vt.state = VirtualThreadState::Started;
                resubmit = true;
            }
        } else {
            // gci i14/ve (i13/vg review R2, the bound twin of
            // `deposit_continuation`'s leak): no record to hold the state.
            // Dropping the box here would free a `JvmThread` whose registry
            // entry is still alive and still publishes its TLAB / `JvmThread`
            // addresses, which the next pause reads. Not reachable by reading
            // (a bound virtual thread's record is never removed); leak rather
            // than free.
            tracing::warn!(
                "suspend_runtime: no manager record for virtual thread {vt_id}; \
                 its state is kept (leaked) instead of freed under a live registry entry"
            );
            let _ = Box::leak(runtime);
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
                if vt.jdk {
                    continue;
                }
                if vt.state == VirtualThreadState::Parked {
                    vt.state = VirtualThreadState::Started;
                    mark_unmounted_runnable(vt);
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
            if vt.jdk {
                return;
            }
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
        self.unpark_virtual_waking(vt_id, None);
    }

    /// [`Self::unpark_virtual`], also waking a park on the thread's CARRIER:
    /// `park_state` is the thread's own, signalled (under the `threads` lock,
    /// so no carrier park can end between the test and the signal) when
    /// [`VirtualThread::carrier_park`] is set. `LockSupport.unpark` passes it
    /// (`NativeThreadAccess::unpark`); the interrupt paths do not, because a
    /// carrier park polls the interrupt flag and they wake the `ParkState`
    /// themselves.
    ///
    /// Interpreter round i1 wave 28, lane L4: the `unpark` native used to
    /// signal the `ParkState` whatever the thread was doing, beside this
    /// function's resubmit or permit, so one `unpark` left TWO permits and the
    /// thread's next park of the other kind returned at once (an unmounting
    /// park after a pinned one found `unpark_permit`; a pinned park after an
    /// unmounting one found the `ParkState` permit).
    pub fn unpark_virtual_waking(
        &self,
        vt_id: u64,
        park_state: Option<&crate::threading::ParkState>,
    ) {
        self.unpark_virtual_waking_with(
            vt_id,
            park_state,
            crate::threading::jvm_thread::UnparkShape::LOCKED,
        );
    }

    /// [`Self::unpark_virtual_waking`] whose carrier-park wake takes `shape`
    /// (interpreter round i4 wave 91, lane L6; page
    /// `i77-L6-proposal-every-vm-unpark-through-the-shape`): `LockSupport.unpark`
    /// passes the VM's (`ThreadRegistry::unpark_shape`), so a pinned virtual
    /// thread's carrier park is woken as a platform thread's park is. The
    /// manager has no registry handle of its own.
    pub fn unpark_virtual_waking_with(
        &self,
        vt_id: u64,
        park_state: Option<&crate::threading::ParkState>,
        shape: crate::threading::jvm_thread::UnparkShape,
    ) {
        let mut resubmit = false;
        {
            let mut threads = self.threads.lock();
            let Some(vt) = threads.get_mut(&vt_id) else {
                return;
            };
            if vt.state == VirtualThreadState::Terminated || vt.jdk {
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
                mark_unmounted_runnable(vt);
                resubmit = true;
            } else {
                // Mounted, mid-unmount, or queued: `LockSupport` permits are
                // sticky, so record it and let the unmount path (or the next
                // park) observe it.
                vt.unpark_permit = true;
                // Parked on its carrier: the permit is set, and the park is
                // blocked on the `ParkState`, which nothing above reaches.
                // `end_carrier_park` consumes both, as HotSpot's
                // `parkOnCarrierThread` consumes `parkPermit` however it woke.
                if vt.carrier_park {
                    if let Some(park_state) = park_state {
                        park_state.unpark_with(shape);
                    }
                }
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

    /// The start of a park that blocks a virtual thread's CARRIER instead of
    /// unmounting it -- a `LockSupport` park while pinned (a held monitor, or,
    /// since wave 26, a native beneath the park), or a native's own carrier
    /// park -- `park_state` being the thread's own (`JvmThread::park_state`).
    /// `true`: the thread's permit was set and is now consumed; the park
    /// returns without blocking. Otherwise [`VirtualThread::carrier_park`] is
    /// set until [`Self::end_carrier_park`], and an `unpark` wakes the
    /// `ParkState` ([`Self::unpark_virtual_waking`]).
    ///
    /// A virtual thread has ONE permit, `unpark_permit` (HotSpot's
    /// `VirtualThread.parkPermit`), whichever way it parks; its `ParkState` is
    /// only the channel that ends a carrier park. So a `ParkState` permit left
    /// from before -- an interrupt's or a stack dump's wake, which are not
    /// `LockSupport` permits (an interrupt's is `unpark_permit`, through
    /// [`Self::interrupt_virtual`]) -- is dropped here rather than ending this
    /// park as a second permit (interpreter round i1 wave 28, lane L4).
    pub fn begin_carrier_park(&self, vt_id: u64, park_state: &crate::threading::ParkState) -> bool {
        let mut threads = self.threads.lock();
        let Some(vt) = threads.get_mut(&vt_id) else {
            return false;
        };
        park_state.take_permit();
        if vt.unpark_permit {
            vt.unpark_permit = false;
            return true;
        }
        vt.carrier_park = true;
        false
    }

    /// The end of a carrier park [`Self::begin_carrier_park`] began, however
    /// it ended (a wake, its timeout, an interrupt): the permit is consumed,
    /// as HotSpot's `parkOnCarrierThread` ends with `setParkPermit(false)`, so
    /// an `unpark` that raced the timeout does not outlive the park, and the
    /// `ParkState` wake it may have signalled is dropped with it.
    pub fn end_carrier_park(&self, vt_id: u64, park_state: &crate::threading::ParkState) {
        let mut threads = self.threads.lock();
        if let Some(vt) = threads.get_mut(&vt_id) {
            vt.carrier_park = false;
            vt.unpark_permit = false;
        }
        park_state.take_permit();
    }

    /// Consume a VM-scheduled virtual thread's `LockSupport` permit
    /// (`unpark_permit`) without parking: `true` when one was set. gci i23/pk
    /// (finding 5 of `gci-i22rv-i21-default-path-review-findings-FIXED-20261016`):
    /// a park that returns on a pending interrupt still takes the permit, as
    /// `VirtualThread.park` runs `getAndSetParkPermit(false) || interrupted`
    /// and `Parker::park` takes `_counter` before its interrupt test. Unlike a
    /// [`Self::begin_carrier_park`] / [`Self::end_carrier_park`] pair, it
    /// never raises `carrier_park`, so an `unpark` racing it is kept as the
    /// next permit, not dropped with the carrier park's wake.
    pub fn take_unpark_permit(&self, vt_id: u64) -> bool {
        let mut threads = self.threads.lock();
        threads
            .get_mut(&vt_id)
            .is_some_and(|vt| std::mem::replace(&mut vt.unpark_permit, false))
    }

    /// `Thread.interrupt()` of a virtual thread, after the caller set its
    /// interrupt flag (interpreter round i1 wave 24, lane L7).
    ///
    /// HotSpot's `JavaThread::interrupt` unparks the target, so a parked,
    /// sleeping or waiting thread wakes at once. The flag alone reached no
    /// unmounted continuation: a `Parked` one stayed parked (untimed) or slept
    /// out its `WakeupTimer` (timed). Cancel the timer and resubmit exactly as
    /// `LockSupport.unpark` does; the remount (`interpreter::remount_action`)
    /// then throws out of a sleep or keyed wait and returns from a park. A
    /// continuation still mounted keeps the sticky permit `unpark_virtual`
    /// records (HotSpot's interrupt sets the `Parker` permit too), and
    /// `suspend_runtime` reads the flag itself when it deposits.
    pub fn interrupt_virtual(&self, vt_id: u64) {
        if self.is_jdk_continuation(vt_id) {
            return;
        }
        self.cancel_wakeup(vt_id);
        self.unpark_virtual(vt_id);
    }

    /// Give a mounted virtual thread back the `LockSupport` permit an
    /// `unpark_virtual` consumed by resubmitting it out of a `Thread.sleep`
    /// (wave 24, lane L7). A sleep does not end on an unpark -- the remount
    /// sleeps out the rest (`interpreter::remount_action`) -- but the permit
    /// stays set, as HotSpot's `Parker` keeps it, so the thread's next untimed
    /// park returns at once (`suspend_runtime` consumes it).
    pub fn restore_unpark_permit(&self, vt_id: u64) {
        if let Some(vt) = self.threads.lock().get_mut(&vt_id) {
            if vt.state != VirtualThreadState::Terminated {
                vt.unpark_permit = true;
            }
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
        let mut retired_jdk = false;
        if let Some(vt) = threads.get_mut(&vt_id) {
            retired_jdk = vt.jdk && vt.state != VirtualThreadState::Terminated;
            vt.state = VirtualThreadState::Terminated;
            vt.continuation.state = ContinuationState::Completed;
            vt.unmount();
        }
        if retired_jdk {
            self.retire_jdk_entries_locked(&mut threads);
        }
        drop(threads);
        let mut keyed = self.keyed_waiters.lock();
        keyed.retain(|_, waiters| {
            waiters.retain(|candidate| *candidate != vt_id);
            !waiters.is_empty()
        });
    }

    /// A JDK continuation's entry just turned `Terminated` (the caller holds
    /// the `threads` lock): count it, and past `2 * cap` remove the oldest
    /// (lowest-id) terminated JDK continuations' entries until `cap` remain
    /// (`CRATONVM_VTHREAD_RETIRE_MANAGER_ENTRIES`; interpreter round i4 wave
    /// 95, lane L6). One collect-and-sort of the terminated entries per `cap`
    /// deaths. An entry a debugger is reading (`debugger_reading`) or that
    /// still holds a deposited state is kept. A removed id answers as one
    /// this manager never tracked: `is_virtual` / `is_jdk_continuation`
    /// `false`, `get_state` `None`, `join` `false` (as for a terminated one),
    /// and every park / unpark / interrupt of it is a no-op, as it already
    /// was for a JDK continuation (`jdk` entries are never submitted here).
    fn retire_jdk_entries_locked(&self, threads: &mut FxHashMap<u64, VirtualThread>) {
        let Some(cap) = self.retired_jdk_entry_cap else {
            return;
        };
        let retired = self.retired_jdk_entries.load(Ordering::Relaxed) + 1;
        if retired <= cap.saturating_mul(2) {
            self.retired_jdk_entries.store(retired, Ordering::Relaxed);
            return;
        }
        let mut dead: Vec<u64> = threads
            .iter()
            .filter(|(_, vt)| {
                vt.jdk
                    && vt.state == VirtualThreadState::Terminated
                    && vt.runtime.is_none()
                    && !vt.debugger_reading
            })
            .map(|(&id, _)| id)
            .collect();
        dead.sort_unstable();
        let drop_count = dead.len().saturating_sub(cap);
        for id in &dead[..drop_count] {
            threads.remove(id);
        }
        // Whatever stayed (kept entries included) counts toward the next purge.
        self.retired_jdk_entries
            .store(dead.len() - drop_count, Ordering::Relaxed);
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

    /// Is `vt_id` a JDK continuation's thread (`vm::jdk_continuation`),
    /// which the JDK schedules and this manager never submits?
    pub fn is_jdk_continuation(&self, vt_id: u64) -> bool {
        self.threads.lock().get(&vt_id).is_some_and(|vt| vt.jdk)
    }

    /// `false` only while this manager has never tracked a virtual thread,
    /// so [`Self::is_virtual`] is `false` for every id without taking the
    /// `threads` lock (one load). A thread whose mirror reached the asker was
    /// created, and its id inserted, before that mirror was published to it,
    /// so the raise (before the insert) is visible to any such asker.
    /// Interpreter round i3 wave 72, lane L6 (`CRATONVM_UNPARK_ONE_LOOKUP`).
    #[inline]
    pub fn may_have_virtual_threads(&self) -> bool {
        self.ever_tracked.load(Ordering::SeqCst)
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
    /// Per-VM event-loop manager.
    pub fn event_loops(&self) -> &super::event_loop::EventLoopManager {
        &self.event_loops
    }

    pub fn set_event_loop_affinity(
        &self,
        vt_id: u64,
        raw_id: i64,
    ) -> Result<(), super::event_loop::EventLoopError> {
        use super::event_loop::EventLoopId;
        let eid = EventLoopId::from_raw(raw_id)?;
        // Verify the id actually exists.
        self.event_loops.lookup(eid)?;
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
        use super::event_loop::EventLoopId;
        let eid = EventLoopId::from_raw(raw_id)?;
        self.event_loops.schedule_on_event_loop(eid, task)
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
mod tests;

#[cfg(test)]
mod w95_l6_retired_jdk_entry_tests;
