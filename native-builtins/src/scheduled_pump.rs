// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP4.5 — Scheduled-task pump for `ScheduledThreadPoolExecutor`.
//!
//! The native side cannot use the OS-thread approach the JDK uses because
//! `NativeContext` is not `Send` — a real worker thread cannot
//! `invoke_virtual` Java methods. Instead, we register periodic tasks
//! here at submission time and let the *current* native context drive the
//! pump every time control passes through `Thread.sleep` or
//! `awaitTermination`. For the WP4.5 acceptance probe (which does
//! `scheduleAtFixedRate(...) → Thread.sleep(250) → cancel(); shutdown();
//! awaitTermination(2,SECONDS)`), this fires the runnable the correct
//! number of times: ~5 ticks during the 250ms `Thread.sleep`.
//!
//! The registry is process-wide and indexed by a u64 id; the
//! `ScheduledFuture` synthetic object stores that id in its second slot
//! so `cancel(false)` flips the cancelled flag.
//!
//! See also `vm/src/threading/scheduled.rs` for a parallel pure-Rust
//! state machine that vm-internal benchmarks can exercise without going
//! through native dispatch.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use parking_lot::Mutex;

use cratonvm_native_api::NativeContext;
use cratonvm_types::{ObjectRef, Value};

/// One periodic registration. `runnable_ptr` is the raw `ObjectRef`
/// pointer kept as `usize` so the registration is `Send`+`Sync`. The
/// pump reconstructs the `ObjectRef` only while it holds a live
/// `NativeContext`, so the GC cannot have moved or freed the runnable
/// between calls — the `ScheduledThreadPoolExecutor` holds a strong
/// reference to the runnable, and the pump runs synchronously inside
/// the same Java thread that submitted the task.
pub struct ScheduledTask {
    pub id: u64,
    pub runnable_ptr: usize,
    pub start: Instant,
    pub initial_delay_ms: u64,
    pub period_ms: u64,
    /// Number of fires already acknowledged (incremented by the pump
    /// after `Runnable.run()` returns).
    pub fired: AtomicU64,
    pub cancelled: AtomicBool,
    /// `true` for `scheduleWithFixedDelay` — start the next period from
    /// the END of the previous run rather than the start.
    pub fixed_delay: bool,
    /// Last fire timestamp — used by fixed-delay scheduling.
    pub last_fire: Mutex<Option<Instant>>,
}

impl ScheduledTask {
    /// Returns how many ticks should have fired by `now` based on the
    /// configured policy. Bounded by `period_ms == 0` → one tick max.
    pub fn due_ticks(&self, now: Instant) -> u64 {
        if self.cancelled.load(Ordering::Acquire) {
            return self.fired.load(Ordering::Acquire);
        }
        let elapsed_ms = now.saturating_duration_since(self.start).as_millis() as u64;
        if elapsed_ms < self.initial_delay_ms {
            return 0;
        }
        let after_delay = elapsed_ms - self.initial_delay_ms;
        if self.period_ms == 0 {
            return 1;
        }
        if self.fixed_delay {
            // Fixed-delay: only the *next* tick is due if `period_ms` has
            // passed since the last fire. We can't anticipate >1 fire.
            let last = *self.last_fire.lock();
            let already = self.fired.load(Ordering::Acquire);
            match last {
                None => 1, // first tick is due (we already passed initial_delay)
                Some(last_t) => {
                    let since_last = now.saturating_duration_since(last_t).as_millis() as u64;
                    already + if since_last >= self.period_ms { 1 } else { 0 }
                }
            }
        } else {
            // Fixed-rate: accumulate ticks based on wall-clock.
            1 + after_delay / self.period_ms
        }
    }

    pub fn cancel(&self) -> bool {
        !self.cancelled.swap(true, Ordering::AcqRel)
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

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

    pub fn register(
        &self,
        runnable: ObjectRef,
        initial_delay_ms: u64,
        period_ms: u64,
        fixed_delay: bool,
    ) -> Arc<ScheduledTask> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let task = Arc::new(ScheduledTask {
            id,
            runnable_ptr: runnable.as_ptr() as usize,
            start: Instant::now(),
            initial_delay_ms,
            period_ms,
            fired: AtomicU64::new(0),
            cancelled: AtomicBool::new(false),
            fixed_delay,
            last_fire: Mutex::new(None),
        });
        let mut tasks = self.tasks.lock();
        tasks.push(task.clone());
        task
    }

    /// Look up a task by id (for `ScheduledFuture.cancel`).
    pub fn find(&self, id: u64) -> Option<Arc<ScheduledTask>> {
        let tasks = self.tasks.lock();
        tasks.iter().find(|t| t.id == id).cloned()
    }

    /// Drive the pump. For each live task, fire `runnable.run()` exactly
    /// the number of times it has accrued since the last pump call.
    /// Sleeping callers should poll this in a chunked loop so periodic
    /// tasks get dispatched on time even within a long `Thread.sleep`.
    pub fn pump(&self, ctx: &mut dyn NativeContext) {
        let tasks_snapshot: Vec<_> = {
            let tasks = self.tasks.lock();
            tasks.iter().cloned().collect()
        };
        for task in tasks_snapshot {
            if task.is_cancelled() {
                continue;
            }
            let now = Instant::now();
            let due = task.due_ticks(now);
            let fired = task.fired.load(Ordering::Acquire);
            if due <= fired {
                continue;
            }
            let to_fire = (due - fired).min(64); // safety cap per pump
                                                 // Reconstruct the runnable ObjectRef from the stored raw
                                                 // pointer. SAFETY: the `ScheduledThreadPoolExecutor` holds
                                                 // a strong reference to the runnable, and the pump runs
                                                 // inside the same Java thread that submitted, so the GC
                                                 // cannot have moved or freed the object.
            let runnable_ptr = task.runnable_ptr as *mut u8;
            if runnable_ptr.is_null() {
                continue;
            }
            let runnable = unsafe { ObjectRef::from_raw(runnable_ptr) };
            for _ in 0..to_fire {
                if task.is_cancelled() {
                    break;
                }
                let _ = ctx.invoke_virtual(runnable, "run", "()V", &[]);
                task.fired.fetch_add(1, Ordering::AcqRel);
                if task.fixed_delay {
                    *task.last_fire.lock() = Some(Instant::now());
                }
            }
        }
    }

    /// Number of live (non-cancelled) tasks.
    pub fn live_count(&self) -> usize {
        let tasks = self.tasks.lock();
        tasks.iter().filter(|t| !t.is_cancelled()).count()
    }

    pub fn cancel_all(&self) {
        let tasks = self.tasks.lock();
        for t in tasks.iter() {
            t.cancelled.store(true, Ordering::Release);
        }
    }

    /// Drop tasks that are cancelled or one-shot-completed so the registry
    /// does not grow without bound across long-running probes.
    pub fn gc(&self) {
        let mut tasks = self.tasks.lock();
        tasks.retain(|t| {
            if t.is_cancelled() {
                return false;
            }
            if t.period_ms == 0 && t.fired.load(Ordering::Acquire) >= 1 {
                return false;
            }
            true
        });
    }
}

impl Default for ScheduledRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Process-wide singleton.
pub fn registry() -> &'static ScheduledRegistry {
    static REG: OnceLock<ScheduledRegistry> = OnceLock::new();
    REG.get_or_init(ScheduledRegistry::default)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn registry_singleton() {
        let r1 = registry() as *const _;
        let r2 = registry() as *const _;
        assert_eq!(r1, r2);
    }

    #[test]
    fn cancel_returns_true_only_first_time() {
        let task = Arc::new(ScheduledTask {
            id: 0,
            runnable_ptr: 0,
            start: Instant::now(),
            initial_delay_ms: 0,
            period_ms: 50,
            fired: AtomicU64::new(0),
            cancelled: AtomicBool::new(false),
            fixed_delay: false,
            last_fire: Mutex::new(None),
        });
        assert!(task.cancel());
        assert!(!task.cancel());
        assert!(task.is_cancelled());
    }

    #[test]
    fn fixed_rate_due_ticks_grow_with_time() {
        let task = ScheduledTask {
            id: 0,
            runnable_ptr: 0,
            start: Instant::now() - Duration::from_millis(150),
            initial_delay_ms: 0,
            period_ms: 50,
            fired: AtomicU64::new(0),
            cancelled: AtomicBool::new(false),
            fixed_delay: false,
            last_fire: Mutex::new(None),
        };
        // 150ms / 50ms + 1 = 4 ticks (1 immediate + 3 periodic).
        let n = task.due_ticks(Instant::now());
        assert!(n >= 3 && n <= 5, "expected 3..=5, got {n}");
    }

    #[test]
    fn one_shot_caps_at_one_tick() {
        let task = ScheduledTask {
            id: 0,
            runnable_ptr: 0,
            start: Instant::now() - Duration::from_secs(1),
            initial_delay_ms: 50,
            period_ms: 0,
            fired: AtomicU64::new(0),
            cancelled: AtomicBool::new(false),
            fixed_delay: false,
            last_fire: Mutex::new(None),
        };
        assert_eq!(task.due_ticks(Instant::now()), 1);
    }

    #[test]
    fn gc_drops_cancelled_tasks() {
        let reg = ScheduledRegistry::new();
        // Synthesize a task without a real ObjectRef.
        let id_a = {
            let task_a = Arc::new(ScheduledTask {
                id: 100,
                runnable_ptr: 0,
                start: Instant::now(),
                initial_delay_ms: 0,
                period_ms: 50,
                fired: AtomicU64::new(0),
                cancelled: AtomicBool::new(false),
                fixed_delay: false,
                last_fire: Mutex::new(None),
            });
            reg.tasks.lock().push(task_a.clone());
            task_a.cancel();
            task_a.id
        };
        let task_b = Arc::new(ScheduledTask {
            id: 101,
            runnable_ptr: 0,
            start: Instant::now(),
            initial_delay_ms: 0,
            period_ms: 50,
            fired: AtomicU64::new(0),
            cancelled: AtomicBool::new(false),
            fixed_delay: false,
            last_fire: Mutex::new(None),
        });
        reg.tasks.lock().push(task_b.clone());
        reg.gc();
        let remaining: Vec<_> = reg.tasks.lock().iter().map(|t| t.id).collect();
        assert!(!remaining.contains(&id_a));
        assert!(remaining.contains(&task_b.id));
    }

    #[test]
    fn cancel_all_marks_every_task() {
        let reg = ScheduledRegistry::new();
        for i in 0..3 {
            reg.tasks.lock().push(Arc::new(ScheduledTask {
                id: 200 + i,
                runnable_ptr: 0,
                start: Instant::now(),
                initial_delay_ms: 0,
                period_ms: 50,
                fired: AtomicU64::new(0),
                cancelled: AtomicBool::new(false),
                fixed_delay: false,
                last_fire: Mutex::new(None),
            }));
        }
        // Confirm pre-state by id.
        let live_before: Vec<u64> = reg
            .tasks
            .lock()
            .iter()
            .filter(|t| !t.is_cancelled() && t.id >= 200)
            .map(|t| t.id)
            .collect();
        assert_eq!(live_before.len(), 3);
        reg.cancel_all();
        let live_after: Vec<u64> = reg
            .tasks
            .lock()
            .iter()
            .filter(|t| !t.is_cancelled() && t.id >= 200)
            .map(|t| t.id)
            .collect();
        assert_eq!(live_after.len(), 0);
    }
}
