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

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use parking_lot::Mutex;

use cratonvm_native_api::NativeContext;
use cratonvm_types::{ObjectRef, Value};

/// One periodic registration. `runnable_ptr` is the raw `ObjectRef`
/// pointer kept as an `AtomicUsize` so the registration is `Send`+`Sync`
/// *and* relocatable: the runnable is a live Java object that a moving
/// GC may compact, so the pointer is published as a GC root via
/// [`gc_scan_scheduled_roots`] and rewritten in place by
/// [`gc_update_scheduled_refs`] after every relocation. The pump always
/// reads the *current* value before reconstructing the `ObjectRef`, so
/// it can never invoke a stale or freed object.
pub struct ScheduledTask {
    pub id: u64,
    /// Raw `ObjectRef` pointer of the runnable, as a relocatable GC root.
    /// `0` means "no runnable" (used by unit tests / synthetic tasks).
    pub runnable_ptr: AtomicUsize,
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
            runnable_ptr: AtomicUsize::new(runnable.as_ptr() as usize),
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
                                                 // pointer. We read the *current* value (it may have been
                                                 // remapped by `gc_update_scheduled_refs` after a moving GC)
                                                 // and the pointer is kept live across collections by
                                                 // `gc_scan_scheduled_roots`, so it is neither stale nor
                                                 // freed.
            let runnable_ptr = task.runnable_ptr.load(Ordering::Acquire) as *mut u8;
            if runnable_ptr.is_null() {
                continue;
            }
            let mut runnable = unsafe { ObjectRef::from_raw(runnable_ptr) };
            let pin_base = ctx.pin_native_root(runnable);
            for _ in 0..to_fire {
                if task.is_cancelled() {
                    break;
                }
                runnable = ctx.read_native_pin(pin_base, runnable);
                let _ = ctx.invoke_virtual(runnable, "run", "()V", &[]);
                runnable = ctx.read_native_pin(pin_base, runnable);
                task.runnable_ptr
                    .store(runnable.as_ptr() as usize, Ordering::Release);
                task.fired.fetch_add(1, Ordering::AcqRel);
                if task.fixed_delay {
                    *task.last_fire.lock() = Some(Instant::now());
                }
            }
            ctx.unpin_native_roots(pin_base);
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

/// GC root scan: push every live scheduled runnable into `roots` so a
/// moving collector keeps it alive and records it for relocation.
///
/// Called by the VM's root-scan phase. Locks the process-wide registry
/// and emits one `ObjectRef` per task whose `runnable_ptr` is non-null.
/// `parking_lot::Mutex` does not poison, so a panicking holder cannot
/// make us skip a root; a GC must never miss a live root.
pub fn gc_scan_scheduled_roots(roots: &mut Vec<ObjectRef>) {
    let tasks = registry().tasks.lock();
    for task in tasks.iter() {
        let ptr = task.runnable_ptr.load(Ordering::Acquire);
        if ptr == 0 {
            continue;
        }
        // SAFETY: `ptr` is the raw address of a live Java object that the
        // registry is keeping reachable; reconstructing the `ObjectRef`
        // hands it to the collector as a root.
        let obj = unsafe { ObjectRef::from_raw(ptr as *mut u8) };
        roots.push(obj);
    }
}

/// GC remap: after a moving collection relocates objects, rewrite each
/// stored `runnable_ptr` from its old address to the new one.
///
/// `map` maps old raw address → new raw address. For each task whose
/// current `runnable_ptr` appears in `map`, the pointer is updated in
/// place; tasks not present in `map` (and null pointers) are left
/// untouched. Locks the process-wide registry; `parking_lot::Mutex`
/// does not poison.
pub fn gc_update_scheduled_refs(map: &cratonvm_types::PointerMap) {
    if map.is_empty() {
        return;
    }
    let tasks = registry().tasks.lock();
    for task in tasks.iter() {
        let old = task.runnable_ptr.load(Ordering::Acquire);
        if old == 0 {
            continue;
        }
        if let Some(&new) = map.get(&old) {
            task.runnable_ptr.store(new, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
    use cratonvm_types::error::MethodCallResult;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    static PUMP_RELOC_OLD: AtomicUsize = AtomicUsize::new(0);
    static PUMP_RELOC_NEW: AtomicUsize = AtomicUsize::new(0);
    static PUMP_RELOC_CALLS: AtomicUsize = AtomicUsize::new(0);
    static PUMP_RELOC_FIRST: AtomicUsize = AtomicUsize::new(0);
    static PUMP_RELOC_SECOND: AtomicUsize = AtomicUsize::new(0);

    fn relocating_run_hook(
        ctx: &mut MockNativeContext,
        receiver: ObjectRef,
        method_name: &str,
        descriptor: &str,
        _args: &[Value],
    ) -> Option<MethodCallResult> {
        if method_name != "run" || descriptor != "()V" {
            return None;
        }
        let call = PUMP_RELOC_CALLS.fetch_add(1, Ordering::SeqCst);
        match call {
            0 => {
                PUMP_RELOC_FIRST.store(receiver.as_ptr() as usize, Ordering::SeqCst);
                ctx.remap_native_pin_addr_for_test(
                    PUMP_RELOC_OLD.load(Ordering::SeqCst),
                    PUMP_RELOC_NEW.load(Ordering::SeqCst),
                );
            }
            1 => {
                PUMP_RELOC_SECOND.store(receiver.as_ptr() as usize, Ordering::SeqCst);
            }
            _ => {}
        }
        Some(Ok(None))
    }

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
            runnable_ptr: AtomicUsize::new(0),
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
            runnable_ptr: AtomicUsize::new(0),
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
            runnable_ptr: AtomicUsize::new(0),
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
    fn pump_re_reads_runnable_pin_after_callback_gc() {
        let old_addr = 0x5100usize;
        let new_addr = 0x6100usize;
        PUMP_RELOC_OLD.store(old_addr, Ordering::SeqCst);
        PUMP_RELOC_NEW.store(new_addr, Ordering::SeqCst);
        PUMP_RELOC_CALLS.store(0, Ordering::SeqCst);
        PUMP_RELOC_FIRST.store(0, Ordering::SeqCst);
        PUMP_RELOC_SECOND.store(0, Ordering::SeqCst);

        let reg = ScheduledRegistry::new();
        let task = Arc::new(ScheduledTask {
            id: 500,
            runnable_ptr: AtomicUsize::new(old_addr),
            start: Instant::now() - Duration::from_millis(10),
            initial_delay_ms: 0,
            period_ms: 1,
            fired: AtomicU64::new(0),
            cancelled: AtomicBool::new(false),
            fixed_delay: false,
            last_fire: Mutex::new(None),
        });
        reg.tasks.lock().push(task.clone());

        let mut ctx = MockNativeContext::new();
        ctx.set_invoke_virtual_hook(relocating_run_hook);
        reg.pump(&mut ctx);

        assert!(
            PUMP_RELOC_CALLS.load(Ordering::SeqCst) >= 2,
            "test setup must accrue at least two scheduled fires"
        );
        assert_eq!(PUMP_RELOC_FIRST.load(Ordering::SeqCst), old_addr);
        assert_eq!(PUMP_RELOC_SECOND.load(Ordering::SeqCst), new_addr);
        assert_eq!(task.runnable_ptr.load(Ordering::SeqCst), new_addr);
        assert_eq!(ctx.native_pin_count_for_test(), 0);
    }

    #[test]
    fn gc_drops_cancelled_tasks() {
        let reg = ScheduledRegistry::new();
        // Synthesize a task without a real ObjectRef.
        let id_a = {
            let task_a = Arc::new(ScheduledTask {
                id: 100,
                runnable_ptr: AtomicUsize::new(0),
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
            runnable_ptr: AtomicUsize::new(0),
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
    fn gc_update_remaps_runnable_ptr() {
        // The remap helper rewrites stored runnable pointers old->new and
        // leaves unmapped / null pointers untouched. This is the relocation
        // path that prevents the stale-pointer UAF under a moving GC. We
        // assert directly on the per-task pointer (no ObjectRef deref).
        let task_moved = Arc::new(ScheduledTask {
            id: 300,
            runnable_ptr: AtomicUsize::new(0x1000),
            start: Instant::now(),
            initial_delay_ms: 0,
            period_ms: 50,
            fired: AtomicU64::new(0),
            cancelled: AtomicBool::new(false),
            fixed_delay: false,
            last_fire: Mutex::new(None),
        });
        let task_stable = Arc::new(ScheduledTask {
            id: 301,
            runnable_ptr: AtomicUsize::new(0x2000),
            start: Instant::now(),
            initial_delay_ms: 0,
            period_ms: 50,
            fired: AtomicU64::new(0),
            cancelled: AtomicBool::new(false),
            fixed_delay: false,
            last_fire: Mutex::new(None),
        });

        let mut map = std::collections::HashMap::new();
        map.insert(0x1000usize, 0x9000usize);
        // 0x2000 deliberately absent.

        // Apply the remap to a local task set (mirrors the global-lock loop
        // body without touching the process-wide registry).
        for task in [&task_moved, &task_stable] {
            let old = task.runnable_ptr.load(Ordering::Acquire);
            if old == 0 {
                continue;
            }
            if let Some(&new) = map.get(&old) {
                task.runnable_ptr.store(new, Ordering::Release);
            }
        }

        assert_eq!(task_moved.runnable_ptr.load(Ordering::Acquire), 0x9000);
        assert_eq!(task_stable.runnable_ptr.load(Ordering::Acquire), 0x2000);
    }

    #[test]
    fn gc_update_global_registry_remaps_and_skips_null() {
        // Drive the real public helper against the process-wide registry.
        // Use a synthetic pointer value that is never dereferenced (the
        // remap path only loads/stores the integer).
        let reg = registry();
        let synthetic = 0x5A5A_0000usize;
        let task = Arc::new(ScheduledTask {
            id: 400,
            runnable_ptr: AtomicUsize::new(synthetic),
            start: Instant::now(),
            initial_delay_ms: 0,
            period_ms: 50,
            fired: AtomicU64::new(0),
            cancelled: AtomicBool::new(false),
            fixed_delay: false,
            last_fire: Mutex::new(None),
        });
        let null_task = Arc::new(ScheduledTask {
            id: 401,
            runnable_ptr: AtomicUsize::new(0),
            start: Instant::now(),
            initial_delay_ms: 0,
            period_ms: 50,
            fired: AtomicU64::new(0),
            cancelled: AtomicBool::new(false),
            fixed_delay: false,
            last_fire: Mutex::new(None),
        });
        reg.tasks.lock().push(task.clone());
        reg.tasks.lock().push(null_task.clone());

        let mut map = cratonvm_types::PointerMap::default();
        let relocated = 0x5A5A_1000usize;
        map.insert(synthetic, relocated);
        gc_update_scheduled_refs(&map);

        assert_eq!(task.runnable_ptr.load(Ordering::Acquire), relocated);
        // Null pointer left untouched (no spurious remap).
        assert_eq!(null_task.runnable_ptr.load(Ordering::Acquire), 0);
    }

    #[test]
    fn cancel_all_marks_every_task() {
        let reg = ScheduledRegistry::new();
        for i in 0..3 {
            reg.tasks.lock().push(Arc::new(ScheduledTask {
                id: 200 + i,
                runnable_ptr: AtomicUsize::new(0),
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
