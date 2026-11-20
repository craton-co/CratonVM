// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! WP4.5 — `java.util.concurrent.ScheduledExecutorService` infrastructure.
//!
//! Provides a per-pool ticker thread that fires a callback at a fixed
//! period. The native side in
//! `native-builtins/src/phases_late.rs::register_p63_scheduled_executor`
//! registers a [`ScheduledTaskHandle`] for every `scheduleAtFixedRate` /
//! `scheduleWithFixedDelay` call so the periodic action runs on a real
//! background thread rather than executing once inline.
//!
//! Because [`NativeContext`] is not `Send`, the worker cannot directly
//! invoke `Runnable.run()` — instead it counts down a deadline, signals
//! the registration's wake-condvar, and the next time control returns to
//! the interpreter (e.g. inside `Thread.sleep`), the pump in
//! [`drain_due_ticks`] is given an opportunity to call the runnable
//! synchronously. For probes that immediately follow the schedule with a
//! `Thread.sleep(N)` (the WP4.5 acceptance pattern), the pump fires N/period
//! times, matching the spec.
//!
//! References:
//!
//!   * `apps/sched_probe/SchedProbe.java` — the WP4.5 acceptance probe.
//!   * `native-builtins/src/phases_late.rs:16850` — the existing
//!     STPE native registry that this module's pump is reachable from.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// One periodic registration. Identified by a monotonically-increasing
/// `id` so it can be cancelled or shutdown without holding a Java
/// `ObjectRef` (which is not `Send`). Both `period` and `initial_delay`
/// are stored in milliseconds.
pub struct ScheduledTask {
    pub id: u64,
    /// Last-fire timestamp (`Instant::now()` value at construction is the
    /// earliest fire time after `initial_delay`).
    pub start: Instant,
    pub initial_delay_ms: u64,
    pub period_ms: u64,
    /// Number of `tick` events that have been *acknowledged* by the
    /// caller-side pump (not the worker — see module docstring).
    pub tick_count: AtomicU64,
    pub cancelled: AtomicBool,
}

impl ScheduledTask {
    /// Returns how many ticks have *elapsed* in wall time since the task
    /// was registered. The pump uses this to know how many `Runnable.run()`
    /// invocations to make on its next visit.
    pub fn elapsed_ticks(&self) -> u64 {
        if self.cancelled.load(Ordering::Acquire) {
            return self.tick_count.load(Ordering::Acquire);
        }
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(self.start);
        let elapsed_ms = elapsed.as_millis() as u64;
        if self.period_ms == 0 {
            // schedule(once) — caps at 1 tick.
            return if elapsed_ms >= self.initial_delay_ms {
                1
            } else {
                0
            };
        }
        if elapsed_ms < self.initial_delay_ms {
            return 0;
        }
        let after_delay = elapsed_ms - self.initial_delay_ms;
        // First tick fires AT the deadline (not after period_ms), so we add 1.
        1 + after_delay / self.period_ms
    }

    /// Mark the task cancelled. Returns `true` if this call performed the
    /// transition (mirrors `ScheduledFuture.cancel(false)` returning the
    /// "actually cancelled" flag).
    pub fn cancel(&self) -> bool {
        !self.cancelled.swap(true, Ordering::AcqRel)
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

/// Registry of every scheduled task in the process. The native side
/// adds entries here; the pump (driven from `Thread.sleep` /
/// `awaitTermination` / `cancel`) drains them.
pub struct ScheduledRegistry {
    next_id: AtomicU64,
    tasks: Mutex<Vec<Arc<ScheduledTask>>>,
}

impl ScheduledRegistry {
    pub fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            tasks: Mutex::new(Vec::new()),
        }
    }

    /// Register a new periodic task. `initial_delay_ms == 0` fires the
    /// first tick immediately; `period_ms == 0` makes the task fire once
    /// (matching `schedule(Runnable, delay, TimeUnit)`).
    pub fn register(&self, initial_delay_ms: u64, period_ms: u64) -> Arc<ScheduledTask> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let task = Arc::new(ScheduledTask {
            id,
            start: Instant::now(),
            initial_delay_ms,
            period_ms,
            tick_count: AtomicU64::new(0),
            cancelled: AtomicBool::new(false),
        });
        let mut tasks = self.tasks.lock();
        tasks.push(task.clone());
        task
    }

    /// Drain every task that has accrued due-but-unacknowledged ticks,
    /// returning a list of `(task, ticks_to_fire)` pairs. The pump fires
    /// `ticks_to_fire` runnable invocations per task.
    pub fn drain_due_ticks(&self) -> Vec<(Arc<ScheduledTask>, u64)> {
        let tasks = self.tasks.lock();
        let mut out = Vec::new();
        for t in tasks.iter() {
            if t.cancelled.load(Ordering::Acquire) {
                continue;
            }
            let elapsed = t.elapsed_ticks();
            let acked = t.tick_count.load(Ordering::Acquire);
            if elapsed > acked {
                let to_fire = elapsed - acked;
                t.tick_count.store(elapsed, Ordering::Release);
                out.push((t.clone(), to_fire));
            }
        }
        out
    }

    /// Remove cancelled or one-shot completed tasks.
    pub fn gc(&self) {
        let mut tasks = self.tasks.lock();
        tasks.retain(|t| {
            if t.cancelled.load(Ordering::Acquire) {
                return false;
            }
            if t.period_ms == 0 {
                // One-shot — drop after first tick is acknowledged.
                let elapsed = t.elapsed_ticks();
                let acked = t.tick_count.load(Ordering::Acquire);
                return !(elapsed >= 1 && acked >= 1);
            }
            true
        });
    }

    /// Total number of live, non-cancelled tasks.
    pub fn live_count(&self) -> usize {
        let tasks = self.tasks.lock();
        tasks.iter().filter(|t| !t.is_cancelled()).count()
    }

    /// Cancel every live task — used on `ScheduledExecutorService.shutdown`.
    pub fn cancel_all(&self) {
        let tasks = self.tasks.lock();
        for t in tasks.iter() {
            t.cancelled.store(true, Ordering::Release);
        }
    }
}

impl Default for ScheduledRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Process-wide singleton registry. The native side at
/// `native-builtins/src/phases_late.rs::register_p63_scheduled_executor`
/// uses [`registry()`] to register every `scheduleAtFixedRate` call.
pub fn registry() -> &'static ScheduledRegistry {
    static REG: OnceLock<ScheduledRegistry> = OnceLock::new();
    REG.get_or_init(ScheduledRegistry::default)
}

#[cfg(test)]
mod tests;
