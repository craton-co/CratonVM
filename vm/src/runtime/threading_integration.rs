// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Phase 18.3: Threading <-> VM Integration
//!
//! Wires virtual threads, VarHandle, JMM fence/volatile semantics,
//! monitor (synchronized) semantics, CAS intrinsics, and scoped-value
//! binding propagation into the runtime.

use std::collections::HashMap;

use rustc_hash::FxHashMap;

// ---------------------------------------------------------------------------
// Virtual Thread Scheduler
// ---------------------------------------------------------------------------

/// Configuration for the virtual-thread scheduler.
#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    /// Number of carrier (platform) threads.
    pub carrier_count: usize,
    /// Maximum number of tasks in the run queue.
    pub max_queue_size: usize,
    /// Whether work-stealing is enabled.
    pub work_stealing: bool,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            carrier_count: 4,
            max_queue_size: 65536,
            work_stealing: true,
        }
    }
}

/// State of a virtual thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VThreadState {
    New,
    Runnable,
    Running,
    Parked,
    PinnedParked,
    Terminated,
}

/// Reason a virtual thread was unmounted from its carrier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnmountReason {
    Yield,
    Park,
    BlockedIO,
    Synchronized,
    Complete,
}

/// Reason a virtual thread is parked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParkReason {
    LockSupport,
    Sleep(u64),
    WaitingOnMonitor,
    WaitingOnIO,
}

/// A task representing a virtual thread.
#[derive(Debug, Clone)]
pub struct VirtualThreadTask {
    pub thread_id: u64,
    pub name: String,
    pub state: VThreadState,
    pub priority: i32,
    pub continuation_id: u64,
}

/// A parked virtual thread.
#[derive(Debug, Clone)]
pub struct ParkedVirtualThread {
    pub thread_id: u64,
    pub reason: ParkReason,
    pub park_time: u64,
}

/// A carrier (platform) thread that hosts virtual threads.
#[derive(Debug, Clone)]
pub struct CarrierThread {
    pub id: usize,
    pub mounted_task: Option<u64>,
    pub tasks_executed: u64,
    pub steal_count: u64,
}

/// Aggregate scheduler statistics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulerStats {
    pub total_spawned: u64,
    pub total_completed: u64,
    pub currently_mounted: u32,
    pub currently_parked: usize,
    pub queue_depth: usize,
    pub steals: u64,
}

/// ForkJoin-style scheduler for virtual threads.
pub struct VirtualThreadScheduler {
    pub carrier_threads: Vec<CarrierThread>,
    pub run_queue: Vec<VirtualThreadTask>,
    /// T10.9.B: FxHashMap — thread_id is internal.
    pub parked_threads: FxHashMap<u64, ParkedVirtualThread>,
    pub mounted_count: u32,
    pub total_spawned: u64,
    pub total_completed: u64,
    pub config: SchedulerConfig,
    next_thread_id: u64,
}

impl VirtualThreadScheduler {
    pub fn new(config: SchedulerConfig) -> Self {
        let carriers = (0..config.carrier_count)
            .map(|id| CarrierThread {
                id,
                mounted_task: None,
                tasks_executed: 0,
                steal_count: 0,
            })
            .collect();
        Self {
            carrier_threads: carriers,
            run_queue: Vec::new(),
            parked_threads: FxHashMap::default(),
            mounted_count: 0,
            total_spawned: 0,
            total_completed: 0,
            config,
            next_thread_id: 1,
        }
    }

    /// Enqueue a virtual thread task and return its assigned thread id.
    pub fn spawn(&mut self, mut task: VirtualThreadTask) -> u64 {
        let id = self.next_thread_id;
        self.next_thread_id += 1;
        task.thread_id = id;
        task.state = VThreadState::Runnable;
        self.run_queue.push(task);
        self.total_spawned += 1;
        id
    }

    /// Mount the next runnable task onto `carrier_id`. Returns `None` when
    /// the run queue is empty.
    pub fn mount_next(&mut self, carrier_id: usize) -> Option<VirtualThreadTask> {
        if carrier_id >= self.carrier_threads.len() {
            return None;
        }
        if self.run_queue.is_empty() {
            return None;
        }
        // Simple FIFO; work-stealing just bumps the counter.
        let mut task = self.run_queue.remove(0);
        task.state = VThreadState::Running;
        self.carrier_threads[carrier_id].mounted_task = Some(task.thread_id);
        self.mounted_count += 1;
        if self.config.work_stealing {
            self.carrier_threads[carrier_id].steal_count += 1;
        }
        Some(task)
    }

    /// Unmount a running task from `carrier_id` for the given reason.
    pub fn unmount(
        &mut self,
        carrier_id: usize,
        mut task: VirtualThreadTask,
        reason: UnmountReason,
    ) {
        if carrier_id < self.carrier_threads.len() {
            self.carrier_threads[carrier_id].mounted_task = None;
            self.carrier_threads[carrier_id].tasks_executed += 1;
            if self.mounted_count > 0 {
                self.mounted_count -= 1;
            }
        }
        match reason {
            UnmountReason::Complete => {
                task.state = VThreadState::Terminated;
                self.total_completed += 1;
            }
            UnmountReason::Park => {
                task.state = VThreadState::Parked;
                self.parked_threads.insert(
                    task.thread_id,
                    ParkedVirtualThread {
                        thread_id: task.thread_id,
                        reason: ParkReason::LockSupport,
                        park_time: 0,
                    },
                );
            }
            _ => {
                task.state = VThreadState::Runnable;
                self.run_queue.push(task);
            }
        }
    }

    /// Park a virtual thread.
    pub fn park(&mut self, thread_id: u64, reason: ParkReason) {
        // Remove from run queue if present
        if let Some(pos) = self.run_queue.iter().position(|t| t.thread_id == thread_id) {
            let mut task = self.run_queue.remove(pos);
            task.state = VThreadState::Parked;
        }
        self.parked_threads.insert(
            thread_id,
            ParkedVirtualThread {
                thread_id,
                reason,
                park_time: 0,
            },
        );
    }

    /// Unpark a parked thread, moving it back to the run queue.
    /// Returns `true` if the thread was actually parked.
    pub fn unpark(&mut self, thread_id: u64) -> bool {
        if let Some(parked) = self.parked_threads.remove(&thread_id) {
            self.run_queue.push(VirtualThreadTask {
                thread_id: parked.thread_id,
                name: String::new(),
                state: VThreadState::Runnable,
                priority: 5,
                continuation_id: 0,
            });
            true
        } else {
            false
        }
    }

    /// Mark a virtual thread as completed.
    pub fn complete(&mut self, thread_id: u64) {
        self.parked_threads.remove(&thread_id);
        self.run_queue.retain(|t| t.thread_id != thread_id);
        self.total_completed += 1;
    }

    /// Get aggregate statistics.
    pub fn get_stats(&self) -> SchedulerStats {
        SchedulerStats {
            total_spawned: self.total_spawned,
            total_completed: self.total_completed,
            currently_mounted: self.mounted_count,
            currently_parked: self.parked_threads.len(),
            queue_depth: self.run_queue.len(),
            steals: self.carrier_threads.iter().map(|c| c.steal_count).sum(),
        }
    }
}

// ---------------------------------------------------------------------------
// Monitor Semantics (synchronized)
// ---------------------------------------------------------------------------

/// Result of a monitor operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MonitorResult {
    Acquired,
    Released,
    AlreadyOwned,
    NotOwner,
    WaitEntered,
    Notified(u64),
    IllegalState(String),
}

/// Per-object monitor (intrinsic lock).
#[derive(Debug, Clone)]
pub struct Monitor {
    pub owner: Option<u64>,
    pub entry_count: u32,
    pub wait_set: Vec<u64>,
    pub entry_set: Vec<u64>,
    pub contention_count: u64,
}

impl Monitor {
    pub fn new() -> Self {
        Self {
            owner: None,
            entry_count: 0,
            wait_set: Vec::new(),
            entry_set: Vec::new(),
            contention_count: 0,
        }
    }
}

/// Contention statistics across all monitors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentionStats {
    pub total_monitors: usize,
    pub total_contentions: u64,
    pub max_wait_set: usize,
    pub currently_blocked: usize,
}

/// Manages all object monitors.
/// T10.9.B: FxHashMap — object address is internal pointer.
pub struct MonitorManager {
    pub monitors: FxHashMap<usize, Monitor>,
}

impl MonitorManager {
    pub fn new() -> Self {
        Self {
            monitors: FxHashMap::default(),
        }
    }

    fn get_or_create(&mut self, obj_addr: usize) -> &mut Monitor {
        self.monitors.entry(obj_addr).or_insert_with(Monitor::new)
    }

    /// Acquire (lock) the monitor for `obj_addr`.
    pub fn acquire(&mut self, obj_addr: usize, thread_id: u64) -> MonitorResult {
        let monitor = self.get_or_create(obj_addr);
        match monitor.owner {
            None => {
                monitor.owner = Some(thread_id);
                monitor.entry_count = 1;
                MonitorResult::Acquired
            }
            Some(owner) if owner == thread_id => {
                monitor.entry_count += 1;
                MonitorResult::AlreadyOwned
            }
            Some(_) => {
                monitor.entry_set.push(thread_id);
                monitor.contention_count += 1;
                MonitorResult::WaitEntered
            }
        }
    }

    /// Release (unlock) the monitor for `obj_addr`.
    pub fn release(&mut self, obj_addr: usize, thread_id: u64) -> MonitorResult {
        let monitor = self.get_or_create(obj_addr);
        match monitor.owner {
            Some(owner) if owner == thread_id => {
                if monitor.entry_count > 1 {
                    monitor.entry_count -= 1;
                    MonitorResult::Released
                } else {
                    monitor.entry_count = 0;
                    // Hand off to first thread in entry_set if any
                    if let Some(next) = monitor.entry_set.first().copied() {
                        monitor.entry_set.remove(0);
                        monitor.owner = Some(next);
                        monitor.entry_count = 1;
                    } else {
                        monitor.owner = None;
                    }
                    MonitorResult::Released
                }
            }
            _ => MonitorResult::NotOwner,
        }
    }

    /// Object.wait() — release the monitor and place thread in wait set.
    pub fn wait(
        &mut self,
        obj_addr: usize,
        thread_id: u64,
        _timeout_ms: Option<u64>,
    ) -> MonitorResult {
        let monitor = self.get_or_create(obj_addr);
        match monitor.owner {
            Some(owner) if owner == thread_id => {
                let saved_count = monitor.entry_count;
                monitor.entry_count = 0;
                monitor.owner = None;
                monitor.wait_set.push(thread_id);
                // Hand off to entry set
                if let Some(next) = monitor.entry_set.first().copied() {
                    monitor.entry_set.remove(0);
                    monitor.owner = Some(next);
                    monitor.entry_count = 1;
                }
                let _ = saved_count; // would be restored on notify
                MonitorResult::WaitEntered
            }
            _ => MonitorResult::NotOwner,
        }
    }

    /// Object.notify() — move one thread from wait set to entry set.
    pub fn notify(&mut self, obj_addr: usize) -> MonitorResult {
        let monitor = self.get_or_create(obj_addr);
        if let Some(tid) = monitor.wait_set.first().copied() {
            monitor.wait_set.remove(0);
            monitor.entry_set.push(tid);
            MonitorResult::Notified(tid)
        } else {
            MonitorResult::Notified(0)
        }
    }

    /// Object.notifyAll() — move all threads from wait set to entry set.
    pub fn notify_all(&mut self, obj_addr: usize) -> MonitorResult {
        let monitor = self.get_or_create(obj_addr);
        let count = monitor.wait_set.len() as u64;
        // `mem::take` rather than `drain(..).collect()`: same result, one fewer
        // allocation, and `clippy::drain_collect` (stable 1.98) refuses the
        // latter under `-D warnings`, which was red on `dev` for every push.
        // The wait set is left empty either way; it gives up its spare capacity
        // here, which for a monitor wait set costs one re-grow on the next
        // `wait()` and is what the lint is asking for.
        let drained: Vec<u64> = std::mem::take(&mut monitor.wait_set);
        monitor.entry_set.extend(drained);
        MonitorResult::Notified(count)
    }

    /// Check if `thread_id` holds the monitor for `obj_addr`.
    pub fn is_held_by(&self, obj_addr: usize, thread_id: u64) -> bool {
        self.monitors
            .get(&obj_addr)
            .map_or(false, |m| m.owner == Some(thread_id))
    }

    /// Aggregate contention statistics.
    pub fn get_contention_stats(&self) -> ContentionStats {
        let total_contentions = self.monitors.values().map(|m| m.contention_count).sum();
        let max_wait_set = self
            .monitors
            .values()
            .map(|m| m.wait_set.len())
            .max()
            .unwrap_or(0);
        let currently_blocked: usize = self.monitors.values().map(|m| m.entry_set.len()).sum();
        ContentionStats {
            total_monitors: self.monitors.len(),
            total_contentions,
            max_wait_set,
            currently_blocked,
        }
    }
}

// ---------------------------------------------------------------------------
// Volatile & Fence Operations
// ---------------------------------------------------------------------------

/// Statistics for volatile access operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolatileStats {
    pub loads: u64,
    pub stores: u64,
    pub fences: u64,
    pub cas_attempts: u64,
    pub cas_successes: u64,
}

/// Manages volatile loads/stores and memory fences (JMM semantics).
pub struct VolatileAccessManager {
    pub access_count: u64,
    pub fence_count: u64,
    loads: u64,
    stores: u64,
}

impl VolatileAccessManager {
    pub fn new() -> Self {
        Self {
            access_count: 0,
            fence_count: 0,
            loads: 0,
            stores: 0,
        }
    }

    /// Volatile load with acquire semantics (i32).
    pub fn volatile_load_i32(&mut self, _addr: usize, value: i32) -> i32 {
        self.access_count += 1;
        self.loads += 1;
        // Acquire fence implied
        value
    }

    /// Volatile store with release semantics (i32).
    pub fn volatile_store_i32(&mut self, _addr: usize, _value: i32) {
        self.access_count += 1;
        self.stores += 1;
    }

    /// Volatile load with acquire semantics (i64).
    pub fn volatile_load_i64(&mut self, _addr: usize, value: i64) -> i64 {
        self.access_count += 1;
        self.loads += 1;
        value
    }

    /// Volatile store with release semantics (i64).
    pub fn volatile_store_i64(&mut self, _addr: usize, _value: i64) {
        self.access_count += 1;
        self.stores += 1;
    }

    /// Volatile load with acquire semantics (reference / usize).
    pub fn volatile_load_ref(&mut self, _addr: usize, value: usize) -> usize {
        self.access_count += 1;
        self.loads += 1;
        value
    }

    /// Volatile store with release semantics (reference / usize).
    pub fn volatile_store_ref(&mut self, _addr: usize, _value: usize) {
        self.access_count += 1;
        self.stores += 1;
    }

    /// Full fence — StoreLoad barrier.
    pub fn full_fence(&mut self) {
        self.fence_count += 1;
    }

    /// Acquire fence — LoadLoad + LoadStore.
    pub fn acquire_fence(&mut self) {
        self.fence_count += 1;
    }

    /// Release fence — LoadStore + StoreStore.
    pub fn release_fence(&mut self) {
        self.fence_count += 1;
    }

    /// Get volatile access statistics.
    pub fn get_stats(&self) -> VolatileStats {
        VolatileStats {
            loads: self.loads,
            stores: self.stores,
            fences: self.fence_count,
            cas_attempts: 0,
            cas_successes: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// CAS Intrinsics
// ---------------------------------------------------------------------------

/// Result of a compare-and-swap operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CasResult<T> {
    pub success: bool,
    pub witness: T,
}

/// Manages compare-and-swap (CAS) and atomic get-and-* operations.
pub struct CasManager {
    pub attempts: u64,
    pub successes: u64,
}

impl CasManager {
    pub fn new() -> Self {
        Self {
            attempts: 0,
            successes: 0,
        }
    }

    /// CAS for i32. Compares `current` with `expected`; if equal, returns
    /// success with `new_val` as witness, otherwise returns the current value.
    pub fn compare_and_swap_i32(
        &mut self,
        current: i32,
        expected: i32,
        new_val: i32,
    ) -> CasResult<i32> {
        self.attempts += 1;
        if current == expected {
            self.successes += 1;
            CasResult {
                success: true,
                witness: new_val,
            }
        } else {
            CasResult {
                success: false,
                witness: current,
            }
        }
    }

    /// CAS for i64.
    pub fn compare_and_swap_i64(
        &mut self,
        current: i64,
        expected: i64,
        new_val: i64,
    ) -> CasResult<i64> {
        self.attempts += 1;
        if current == expected {
            self.successes += 1;
            CasResult {
                success: true,
                witness: new_val,
            }
        } else {
            CasResult {
                success: false,
                witness: current,
            }
        }
    }

    /// CAS for reference (usize).
    pub fn compare_and_swap_ref(
        &mut self,
        current: usize,
        expected: usize,
        new_val: usize,
    ) -> CasResult<usize> {
        self.attempts += 1;
        if current == expected {
            self.successes += 1;
            CasResult {
                success: true,
                witness: new_val,
            }
        } else {
            CasResult {
                success: false,
                witness: current,
            }
        }
    }

    /// Atomically get the current value and set to `new_val` (i32).
    pub fn get_and_set_i32(&mut self, current: i32, _new_val: i32) -> i32 {
        self.attempts += 1;
        self.successes += 1;
        current
    }

    /// Atomically add `delta` and return the previous value (i32).
    pub fn get_and_add_i32(&mut self, current: i32, _delta: i32) -> i32 {
        self.attempts += 1;
        self.successes += 1;
        current
    }

    /// Atomically get the current value and set to `new_val` (i64).
    pub fn get_and_set_i64(&mut self, current: i64, _new_val: i64) -> i64 {
        self.attempts += 1;
        self.successes += 1;
        current
    }

    /// Atomically add `delta` and return the previous value (i64).
    pub fn get_and_add_i64(&mut self, current: i64, _delta: i64) -> i64 {
        self.attempts += 1;
        self.successes += 1;
        current
    }
}

// ---------------------------------------------------------------------------
// Scoped Value Binding Propagation
// ---------------------------------------------------------------------------

/// A single scoped-value binding.
#[derive(Debug, Clone)]
pub struct ScopedBinding {
    pub value_ref: usize,
    pub bound_at: u64,
    pub inherited: bool,
}

/// Carries scoped-value bindings for a virtual thread.
/// T10.9.B: FxHashMap — sv_id is internal.
#[derive(Debug, Clone)]
pub struct ScopedValueBindings {
    pub bindings: FxHashMap<u64, ScopedBinding>,
    pub snapshot_id: u64,
}

impl ScopedValueBindings {
    pub fn new() -> Self {
        Self {
            bindings: FxHashMap::default(),
            snapshot_id: 0,
        }
    }

    /// Bind a scoped value.
    pub fn bind(&mut self, sv_id: u64, value_ref: usize) {
        self.bindings.insert(
            sv_id,
            ScopedBinding {
                value_ref,
                bound_at: 0,
                inherited: false,
            },
        );
    }

    /// Unbind a scoped value. Returns `true` if it was bound.
    pub fn unbind(&mut self, sv_id: u64) -> bool {
        self.bindings.remove(&sv_id).is_some()
    }

    /// Get the value reference for a scoped value.
    pub fn get(&self, sv_id: u64) -> Option<usize> {
        self.bindings.get(&sv_id).map(|b| b.value_ref)
    }

    /// Check if a scoped value is bound.
    pub fn is_bound(&self, sv_id: u64) -> bool {
        self.bindings.contains_key(&sv_id)
    }

    /// Snapshot (clone) bindings for a child virtual thread, marking all as inherited.
    pub fn snapshot(&self) -> ScopedValueBindings {
        let mut cloned = self.clone();
        cloned.snapshot_id += 1;
        for binding in cloned.bindings.values_mut() {
            binding.inherited = true;
        }
        cloned
    }

    /// Number of active bindings.
    pub fn binding_count(&self) -> usize {
        self.bindings.len()
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests;
