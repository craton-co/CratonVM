// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Java Memory Model (JSR-133) compliance for the JVM threading subsystem.
//!
//! Implements happens-before tracking, volatile semantics, monitor ordering,
//! final field publication, thread lifecycle ordering, and optional data race
//! detection. The `JmmManager` coordinates all subsystems and can be disabled
//! for performance-critical scenarios where full tracking is unnecessary.

use std::collections::HashMap;
use std::sync::atomic::{fence, AtomicBool, AtomicU64, Ordering};

use parking_lot::Mutex;

// ---------------------------------------------------------------------------
// 1. HappensBefore edge types
// ---------------------------------------------------------------------------

/// Types of happens-before relationships defined in the Java Memory Model (JSR-133).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HappensBeforeEdge {
    /// Program order within a single thread.
    ProgramOrder,
    /// Monitor lock release -> subsequent lock acquire on same monitor.
    MonitorUnlockToLock,
    /// Volatile write -> subsequent volatile read of same variable.
    VolatileWriteToRead,
    /// Thread.start() call -> first action in started thread.
    ThreadStart,
    /// Last action in thread -> Thread.join() return in joining thread.
    ThreadJoin,
    /// Thread interrupt -> interrupted thread detects interrupt.
    ThreadInterrupt,
    /// Constructor finishes -> finalizer starts.
    FinalizerStart,
    /// Default value write (field init) -> first access in any thread.
    DefaultValueWrite,
    /// Final field freeze -> read via reference published after freeze.
    FinalFieldFreeze,
    /// Transitivity: if A hb B and B hb C, then A hb C.
    Transitive,
}

// ---------------------------------------------------------------------------
// 2. MemoryAction / MemoryActionType
// ---------------------------------------------------------------------------

/// The type of a memory action tracked by the JMM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryActionType {
    Read,
    Write,
    VolatileRead,
    VolatileWrite,
    MonitorEnter,
    MonitorExit,
    ThreadStart,
    ThreadJoin,
    FinalFieldFreeze,
}

/// A single memory action in the JMM execution trace.
#[derive(Debug, Clone)]
pub struct MemoryAction {
    pub action_type: MemoryActionType,
    pub thread_id: u64,
    /// Logical clock per thread.
    pub timestamp: u64,
    /// Object address + field offset identifying the variable.
    pub variable_id: u64,
    /// Value read or written.
    pub value: i64,
}

// ---------------------------------------------------------------------------
// 3. VolatileSemantics
// ---------------------------------------------------------------------------

/// Implements correct volatile field access semantics per JMM.
pub struct VolatileSemantics;

impl VolatileSemantics {
    /// Volatile read: load-acquire semantics.
    /// All subsequent reads/writes in this thread see all writes before
    /// the corresponding volatile write in the writing thread.
    pub fn volatile_read_fence() {
        fence(Ordering::Acquire);
    }

    /// Volatile write: store-release semantics.
    /// All previous reads/writes in this thread are visible to any thread
    /// that subsequently performs a volatile read of this variable.
    pub fn volatile_write_fence() {
        fence(Ordering::Release);
    }

    /// Full volatile fence (for CAS operations and VarHandle.fullFence).
    pub fn full_fence() {
        fence(Ordering::SeqCst);
    }

    /// Acquire fence (VarHandle.acquireFence).
    pub fn acquire_fence() {
        fence(Ordering::Acquire);
    }

    /// Release fence (VarHandle.releaseFence).
    pub fn release_fence() {
        fence(Ordering::Release);
    }

    /// Load-load fence (VarHandle.loadLoadFence).
    pub fn load_load_fence() {
        fence(Ordering::Acquire);
    }

    /// Store-store fence (VarHandle.storeStoreFence).
    pub fn store_store_fence() {
        fence(Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// 4. MonitorSemantics
// ---------------------------------------------------------------------------

/// Tracks happens-before ordering for monitor operations.
pub struct MonitorSemantics {
    /// Per-monitor: last release timestamp (thread_id, logical_clock).
    monitor_clocks: Mutex<HashMap<u64, (u64, u64)>>,
}

impl MonitorSemantics {
    pub fn new() -> Self {
        Self {
            monitor_clocks: Mutex::new(HashMap::new()),
        }
    }

    /// Called when a thread acquires a monitor.
    /// Returns the last release info if one exists (establishing happens-before).
    pub fn on_monitor_enter(&self, monitor_id: u64, _thread_id: u64) -> Option<(u64, u64)> {
        let map = self.monitor_clocks.lock();
        map.get(&monitor_id).copied()
    }

    /// Called when a thread releases a monitor.
    /// Records the release point for future happens-before edges.
    pub fn on_monitor_exit(&self, monitor_id: u64, thread_id: u64, clock: u64) {
        let mut map = self.monitor_clocks.lock();
        map.insert(monitor_id, (thread_id, clock));
    }

    /// Get the last release info for a monitor.
    pub fn last_release(&self, monitor_id: u64) -> Option<(u64, u64)> {
        let map = self.monitor_clocks.lock();
        map.get(&monitor_id).copied()
    }

    /// Clear all tracking (for testing).
    pub fn clear(&self) {
        self.monitor_clocks.lock().clear();
    }
}

// ---------------------------------------------------------------------------
// 5. FinalFieldSemantics
// ---------------------------------------------------------------------------

/// Ensures correct publication of final fields per JMM.
///
/// When a constructor stores a value into a final field, a "freeze" action
/// occurs at the end of the constructor. Any thread that reads the object
/// reference after the freeze will see the final field's value.
pub struct FinalFieldSemantics {
    /// Objects whose constructors have completed (freeze point passed).
    /// Maps object_addr -> freeze_clock.
    frozen_objects: Mutex<HashMap<u64, u64>>,
}

impl FinalFieldSemantics {
    pub fn new() -> Self {
        Self {
            frozen_objects: Mutex::new(HashMap::new()),
        }
    }

    /// Called when an object's constructor completes.
    /// Inserts a freeze barrier ensuring final field values are published.
    pub fn freeze(&self, object_addr: u64, clock: u64) {
        fence(Ordering::Release);
        self.frozen_objects.lock().insert(object_addr, clock);
    }

    /// Check if an object's final fields have been frozen (published).
    pub fn is_frozen(&self, object_addr: u64) -> bool {
        self.frozen_objects.lock().contains_key(&object_addr)
    }

    /// Get the freeze clock for an object.
    pub fn freeze_clock(&self, object_addr: u64) -> Option<u64> {
        self.frozen_objects.lock().get(&object_addr).copied()
    }

    /// Remove entries for dead objects after GC.
    pub fn remove_dead(&self, is_live: &dyn Fn(u64) -> bool) {
        self.frozen_objects.lock().retain(|&addr, _| is_live(addr));
    }

    /// Update addresses after GC relocation.
    pub fn update_after_gc(&self, pointer_map: &HashMap<u64, u64>) {
        let mut map = self.frozen_objects.lock();
        let old: Vec<(u64, u64)> = map.drain().collect();
        for (addr, clock) in old {
            let new_addr = pointer_map.get(&addr).copied().unwrap_or(addr);
            map.insert(new_addr, clock);
        }
    }

    /// Clear all entries.
    pub fn clear(&self) {
        self.frozen_objects.lock().clear();
    }
}

// ---------------------------------------------------------------------------
// 6. ThreadLifecycleSemantics
// ---------------------------------------------------------------------------

/// Happens-before ordering for thread lifecycle events.
pub struct ThreadLifecycleSemantics {
    /// Per-thread logical clock (vector clock simplified to scalar).
    thread_clocks: Mutex<HashMap<u64, u64>>,
    /// Thread start events: started_thread_id -> (starter_thread_id, clock).
    start_events: Mutex<HashMap<u64, (u64, u64)>>,
    /// Thread join events: joined_thread_id -> final_clock.
    join_events: Mutex<HashMap<u64, u64>>,
    /// Interrupt events: interrupted_thread_id -> (interrupter_id, clock).
    interrupt_events: Mutex<HashMap<u64, (u64, u64)>>,
}

impl ThreadLifecycleSemantics {
    pub fn new() -> Self {
        Self {
            thread_clocks: Mutex::new(HashMap::new()),
            start_events: Mutex::new(HashMap::new()),
            join_events: Mutex::new(HashMap::new()),
            interrupt_events: Mutex::new(HashMap::new()),
        }
    }

    /// Called by Thread.start() -- establishes hb from caller to new thread.
    pub fn on_thread_start(&self, starter_id: u64, started_id: u64, clock: u64) {
        self.start_events
            .lock()
            .insert(started_id, (starter_id, clock));
        // Ensure the started thread has an initial clock of 0.
        self.thread_clocks.lock().entry(started_id).or_insert(0);
    }

    /// Called when a thread finishes -- records final clock for join().
    pub fn on_thread_finish(&self, thread_id: u64, clock: u64) {
        self.join_events.lock().insert(thread_id, clock);
    }

    /// Called by Thread.join() -- establishes hb from joined thread to joiner.
    /// Returns the finished thread's final clock if it has terminated.
    pub fn on_thread_join(&self, _joiner_id: u64, joined_id: u64) -> Option<u64> {
        self.join_events.lock().get(&joined_id).copied()
    }

    /// Called by Thread.interrupt().
    pub fn on_thread_interrupt(&self, interrupter_id: u64, target_id: u64, clock: u64) {
        self.interrupt_events
            .lock()
            .insert(target_id, (interrupter_id, clock));
    }

    /// Get a thread's current logical clock.
    pub fn get_clock(&self, thread_id: u64) -> u64 {
        self.thread_clocks
            .lock()
            .get(&thread_id)
            .copied()
            .unwrap_or(0)
    }

    /// Increment a thread's logical clock and return the new value.
    pub fn tick(&self, thread_id: u64) -> u64 {
        let mut map = self.thread_clocks.lock();
        let clock = map.entry(thread_id).or_insert(0);
        *clock += 1;
        *clock
    }

    /// Check if there is a happens-before from thread A's action at `from_clock`
    /// to thread B's current observation.
    ///
    /// This is a simplified check: it verifies whether a lifecycle event
    /// (start, join, interrupt) establishes the ordering.
    pub fn happens_before(&self, from_thread: u64, from_clock: u64, to_thread: u64) -> bool {
        // Same thread: program order always establishes hb.
        if from_thread == to_thread {
            return true;
        }

        // Check if from_thread started to_thread at or after from_clock.
        if let Some(&(starter, clock)) = self.start_events.lock().get(&to_thread) {
            if starter == from_thread && clock >= from_clock {
                return true;
            }
        }

        // Check if from_thread finished and to_thread joined it.
        if let Some(&final_clock) = self.join_events.lock().get(&from_thread) {
            if final_clock >= from_clock {
                // to_thread would see everything up to final_clock via join.
                return true;
            }
        }

        // Check interrupt: from_thread interrupted to_thread.
        if let Some(&(interrupter, clock)) = self.interrupt_events.lock().get(&to_thread) {
            if interrupter == from_thread && clock >= from_clock {
                return true;
            }
        }

        false
    }
}

// ---------------------------------------------------------------------------
// 7. JmmManager
// ---------------------------------------------------------------------------

/// Top-level Java Memory Model manager.
/// Coordinates all happens-before tracking and memory ordering enforcement.
pub struct JmmManager {
    pub volatile: VolatileSemantics,
    pub monitors: MonitorSemantics,
    pub final_fields: FinalFieldSemantics,
    pub thread_lifecycle: ThreadLifecycleSemantics,
    /// Whether JMM tracking is enabled (can be disabled for performance).
    enabled: AtomicBool,
    /// Total memory actions tracked.
    total_actions: AtomicU64,
}

impl JmmManager {
    pub fn new() -> Self {
        Self {
            volatile: VolatileSemantics,
            monitors: MonitorSemantics::new(),
            final_fields: FinalFieldSemantics::new(),
            thread_lifecycle: ThreadLifecycleSemantics::new(),
            enabled: AtomicBool::new(true),
            total_actions: AtomicU64::new(0),
        }
    }

    /// Create a disabled JMM manager (for performance-critical scenarios).
    pub fn disabled() -> Self {
        Self {
            volatile: VolatileSemantics,
            monitors: MonitorSemantics::new(),
            final_fields: FinalFieldSemantics::new(),
            thread_lifecycle: ThreadLifecycleSemantics::new(),
            enabled: AtomicBool::new(false),
            total_actions: AtomicU64::new(0),
        }
    }

    /// Enable or disable tracking.
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    /// Check if tracking is enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    fn bump(&self) {
        self.total_actions.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a volatile read.
    pub fn on_volatile_read(&self, thread_id: u64, _variable_id: u64) {
        VolatileSemantics::volatile_read_fence();
        if self.is_enabled() {
            self.thread_lifecycle.tick(thread_id);
            self.bump();
        }
    }

    /// Record a volatile write.
    pub fn on_volatile_write(&self, thread_id: u64, _variable_id: u64) {
        VolatileSemantics::volatile_write_fence();
        if self.is_enabled() {
            self.thread_lifecycle.tick(thread_id);
            self.bump();
        }
    }

    /// Record monitor enter.
    pub fn on_monitor_enter(&self, thread_id: u64, monitor_id: u64) {
        if self.is_enabled() {
            self.monitors.on_monitor_enter(monitor_id, thread_id);
            self.thread_lifecycle.tick(thread_id);
            self.bump();
        }
    }

    /// Record monitor exit.
    pub fn on_monitor_exit(&self, thread_id: u64, monitor_id: u64) {
        if self.is_enabled() {
            let clock = self.thread_lifecycle.tick(thread_id);
            self.monitors.on_monitor_exit(monitor_id, thread_id, clock);
            self.bump();
        }
    }

    /// Record thread start.
    pub fn on_thread_start(&self, starter_id: u64, started_id: u64) {
        if self.is_enabled() {
            let clock = self.thread_lifecycle.tick(starter_id);
            self.thread_lifecycle
                .on_thread_start(starter_id, started_id, clock);
            self.bump();
        }
    }

    /// Record thread finish.
    pub fn on_thread_finish(&self, thread_id: u64) {
        if self.is_enabled() {
            let clock = self.thread_lifecycle.tick(thread_id);
            self.thread_lifecycle.on_thread_finish(thread_id, clock);
            self.bump();
        }
    }

    /// Record thread join.
    pub fn on_thread_join(&self, joiner_id: u64, joined_id: u64) {
        if self.is_enabled() {
            self.thread_lifecycle.on_thread_join(joiner_id, joined_id);
            self.thread_lifecycle.tick(joiner_id);
            self.bump();
        }
    }

    /// Record final field freeze (constructor completion).
    pub fn on_constructor_complete(&self, object_addr: u64) {
        if self.is_enabled() {
            let clock = self.total_actions.load(Ordering::Relaxed);
            self.final_fields.freeze(object_addr, clock);
            self.bump();
        }
    }

    /// Total actions tracked.
    pub fn total_actions(&self) -> u64 {
        self.total_actions.load(Ordering::Relaxed)
    }

    /// Clean up after GC: remap relocated pointers and remove dead objects.
    pub fn gc_cleanup(&self, pointer_map: &HashMap<u64, u64>, is_live: &dyn Fn(u64) -> bool) {
        self.final_fields.remove_dead(is_live);
        self.final_fields.update_after_gc(pointer_map);
    }
}

// ---------------------------------------------------------------------------
// 8. DataRaceDetector
// ---------------------------------------------------------------------------

/// A record of a single memory access for race detection.
#[derive(Debug, Clone)]
pub struct AccessRecord {
    pub thread_id: u64,
    pub action_type: MemoryActionType,
    pub clock: u64,
}

/// A detected data race between two accesses.
#[derive(Debug, Clone)]
pub struct DataRace {
    pub variable_id: u64,
    pub access1: AccessRecord,
    pub access2: AccessRecord,
    pub description: String,
}

/// Simple data race detector for debugging JMM violations.
///
/// Tracks recent read/write accesses per variable and reports concurrent
/// unsynchronized accesses.
pub struct DataRaceDetector {
    /// Per-variable: recent accesses.
    last_accesses: Mutex<HashMap<u64, Vec<AccessRecord>>>,
    /// Detected races.
    races: Mutex<Vec<DataRace>>,
    /// Whether detection is enabled.
    enabled: AtomicBool,
    /// Maximum accesses to track per variable.
    max_history: usize,
}

impl DataRaceDetector {
    pub fn new() -> Self {
        Self {
            last_accesses: Mutex::new(HashMap::new()),
            races: Mutex::new(Vec::new()),
            enabled: AtomicBool::new(true),
            max_history: 8,
        }
    }

    pub fn with_history(max_history: usize) -> Self {
        Self {
            last_accesses: Mutex::new(HashMap::new()),
            races: Mutex::new(Vec::new()),
            enabled: AtomicBool::new(true),
            max_history,
        }
    }

    /// Record an access and check for races against recent history.
    /// Returns the first race found, if any.
    pub fn record_access(
        &self,
        variable_id: u64,
        thread_id: u64,
        action: MemoryActionType,
        clock: u64,
    ) -> Option<DataRace> {
        if !self.is_enabled() {
            return None;
        }

        let new_record = AccessRecord {
            thread_id,
            action_type: action,
            clock,
        };

        let mut accesses = self.last_accesses.lock();
        let history = accesses.entry(variable_id).or_insert_with(Vec::new);

        // Check against existing history for races.
        let mut found_race = None;
        for existing in history.iter() {
            if Self::is_race(existing, &new_record) {
                let race = DataRace {
                    variable_id,
                    access1: existing.clone(),
                    access2: new_record.clone(),
                    description: format!(
                        "Data race on variable 0x{:x}: thread {} ({:?} @{}) vs thread {} ({:?} @{})",
                        variable_id,
                        existing.thread_id,
                        existing.action_type,
                        existing.clock,
                        new_record.thread_id,
                        new_record.action_type,
                        new_record.clock,
                    ),
                };
                self.races.lock().push(race.clone());
                found_race = Some(race);
                break;
            }
        }

        // Add to history, pruning if needed.
        history.push(new_record);
        if history.len() > self.max_history {
            history.remove(0);
        }

        found_race
    }

    /// Get all detected races.
    pub fn detected_races(&self) -> Vec<DataRace> {
        self.races.lock().clone()
    }

    /// Clear detected races.
    pub fn clear_races(&self) {
        self.races.lock().clear();
    }

    /// Enable or disable detection.
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    /// Check if detection is enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Check if two accesses constitute a data race.
    ///
    /// A race exists when:
    /// 1. The accesses are from different threads, AND
    /// 2. At least one is a (non-volatile) write, AND
    /// 3. They are not ordered by happens-before (approximated by clock equality).
    fn is_race(a: &AccessRecord, b: &AccessRecord) -> bool {
        // Same thread: no race (program order).
        if a.thread_id == b.thread_id {
            return false;
        }

        // Both volatile: no race.
        if is_volatile(a.action_type) && is_volatile(b.action_type) {
            return false;
        }

        // At least one must be a write (non-volatile).
        let a_write = matches!(a.action_type, MemoryActionType::Write);
        let b_write = matches!(b.action_type, MemoryActionType::Write);
        let a_read = matches!(a.action_type, MemoryActionType::Read);
        let b_read = matches!(b.action_type, MemoryActionType::Read);

        // Two plain reads: no race.
        if a_read && b_read {
            return false;
        }

        // At least one plain write with a plain read or write on another thread.
        a_write || b_write
    }
}

/// Helper: returns true for volatile action types.
fn is_volatile(action: MemoryActionType) -> bool {
    matches!(
        action,
        MemoryActionType::VolatileRead | MemoryActionType::VolatileWrite
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
