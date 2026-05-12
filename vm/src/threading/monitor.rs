//! JVM monitor (intrinsic lock) implementation.
//!
//! Every Java object can be used as a monitor. The JVM's `monitorenter` and
//! `monitorexit` instructions acquire and release these monitors. Monitors
//! are **reentrant**: a thread that already owns a monitor can enter it again,
//! incrementing an entry count.
//!
//! The monitor table is a global structure keyed by object identity (pointer
//! address as `usize`). Monitors are created lazily on first use.
//!
//! When multiple OS threads contend for the same monitor, `parking_lot::Condvar`
//! is used to block and wake threads. Two separate condvars are used:
//! - `entry_condvar`: wakes threads blocked on `monitorenter`
//! - `wait_condvar`: wakes threads blocked on `Object.wait()`

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::{Condvar, Mutex};
use rustc_hash::FxHashMap;

use crate::error::{MethodCallFailed, RuntimeError, VmError};
use crate::threading::jvm_thread::ThreadId;
use crate::types::ObjectRef;

// ---------------------------------------------------------------------------
// KC16-watchdog: global stack-dump-request flag for parked Object.wait()ers
// ---------------------------------------------------------------------------
//
// The interpreter top-of-loop polls `SharedVm::stack_dump_pending()` so any
// thread executing bytecode acks the watchdog within a single dispatch.
// Threads parked in `Object.wait()` are blocked OFF the interpreter loop
// (inside `parking_lot::Condvar::wait_for`) and therefore never reach the
// top-of-loop check. This static flag lets the watchdog wake them.
//
// `SharedVm::request_stack_dump()` sets this flag in addition to its own
// per-VM flag. The wait loop in `Monitor::wait` polls it every 5ms (the
// same cadence as the interrupt flag) and exits the wait when set; the
// monitor_wait caller then sees the `SharedVm` flag and dumps frames
// through the normal path.
static STACK_DUMP_WAIT_FLAG: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[inline]
pub fn stack_dump_wait_flag() -> &'static std::sync::atomic::AtomicBool {
    &STACK_DUMP_WAIT_FLAG
}

/// KC16-watchdog: signal all threads parked in `Object.wait()` to exit the
/// condvar wait and re-check their state (where the interpreter loop will
/// observe `SharedVm::stack_dump_pending()` and emit a frame snapshot).
///
/// Called by `SharedVm::request_stack_dump()`.
pub fn signal_stack_dump_to_waiters() {
    STACK_DUMP_WAIT_FLAG.store(true, std::sync::atomic::Ordering::Release);
}

/// KC16-watchdog: callback installed by the VM that emits the current
/// thread's frame chain from a wait-site context. Set by `SharedVm`
/// during construction so `Monitor::wait` can reach the per-thread
/// frame table held in the `JvmThread` registry.
type WaitSiteDumpFn = Arc<dyn Fn(ThreadId) + Send + Sync>;
static WAIT_SITE_DUMP: std::sync::OnceLock<WaitSiteDumpFn> = std::sync::OnceLock::new();

pub fn install_wait_site_dump<F>(f: F)
where
    F: Fn(ThreadId) + Send + Sync + 'static,
{
    let _ = WAIT_SITE_DUMP.set(Arc::new(f));
}

fn emit_wait_site_frames(thread_id: ThreadId) {
    if let Some(f) = WAIT_SITE_DUMP.get() {
        f(thread_id);
    } else {
        tracing::warn!(
            target: "kc16_watchdog",
            thread_id = ?thread_id,
            "Object.wait() observed stack_dump request but no dump callback installed"
        );
    }
}

// ---------------------------------------------------------------------------
// Monitor — per-object lock
// ---------------------------------------------------------------------------

/// A single JVM monitor (intrinsic lock).
///
/// Each monitor tracks its owner thread and re-entry count. Two condvars
/// separate the two kinds of blocking: monitor entry contention and
/// `Object.wait()`/`notify()`.
pub struct Monitor {
    state: Mutex<MonitorState>,
    /// Wakes threads blocked on `monitorenter` (waiting to acquire the lock).
    entry_condvar: Condvar,
    /// Wakes threads blocked on `Object.wait()`.
    wait_condvar: Condvar,
}

/// The mutable state protected by a monitor's mutex.
struct MonitorState {
    /// The thread that currently owns this monitor, or `None` if unlocked.
    owner: Option<ThreadId>,
    /// Re-entry count. Incremented on each `monitorenter`, decremented on
    /// `monitorexit`. The monitor is released when this reaches 0.
    entry_count: u32,
}

impl Monitor {
    /// Create a new, unlocked monitor.
    fn new() -> Self {
        Self {
            state: Mutex::new(MonitorState {
                owner: None,
                entry_count: 0,
            }),
            entry_condvar: Condvar::new(),
            wait_condvar: Condvar::new(),
        }
    }

    /// Returns true if this monitor is currently owned by the given
    /// thread. Non-blocking inspection — used by `Thread.holdsLock`.
    fn is_held_by(&self, thread_id: ThreadId) -> bool {
        let state = self.state.lock();
        state.owner == Some(thread_id)
    }

    /// Acquire this monitor for the given thread.
    ///
    /// If the monitor is unowned, the thread becomes the owner.
    /// If the monitor is already owned by this thread, the entry count is
    /// incremented (reentrant lock).
    /// If the monitor is owned by another thread, this blocks until the
    /// monitor is released.
    fn enter(&self, thread_id: ThreadId) {
        let mut state = self.state.lock();
        // Wait until the monitor is either unowned or owned by us
        while state.owner.is_some() && state.owner != Some(thread_id) {
            self.entry_condvar.wait(&mut state);
        }
        match state.owner {
            None => {
                // Unowned — acquire
                state.owner = Some(thread_id);
                state.entry_count = 1;
            }
            Some(_) => {
                // Reentrant — already owned by this thread
                state.entry_count += 1;
            }
        }
    }

    /// Release this monitor for the given thread.
    ///
    /// Decrements the entry count. When it reaches 0, the monitor is released
    /// and becomes unowned.
    ///
    /// Returns `Err` if the calling thread does not own the monitor
    /// (`IllegalMonitorStateException`).
    fn exit(&self, thread_id: ThreadId) -> Result<(), MonitorError> {
        let mut state = self.state.lock();
        match state.owner {
            Some(owner) if owner == thread_id => {
                state.entry_count -= 1;
                if state.entry_count == 0 {
                    state.owner = None;
                    // Wake one thread waiting to enter this monitor
                    self.entry_condvar.notify_one();
                }
                Ok(())
            }
            _ => Err(MonitorError::NotOwner),
        }
    }

    /// Object.wait() — release the monitor and block until notified.
    ///
    /// The calling thread must own this monitor. The entry count is saved,
    /// ownership is released, and the thread blocks on `wait_condvar`.
    /// When woken (by `notify`/`notifyAll`), it re-acquires the monitor
    /// with the original entry count restored.
    ///
    /// If `timeout_ms` is Some(millis) with millis > 0, the wait is bounded.
    ///
    /// If `interrupted` is provided, the wait periodically checks the flag
    /// and returns early if the thread has been interrupted (needed because
    /// Thread.interrupt() sets a flag but cannot directly wake a condvar).
    fn wait(
        &self,
        thread_id: ThreadId,
        timeout_ms: Option<u64>,
        interrupted: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<bool, MonitorError> {
        let mut state = self.state.lock();
        if state.owner != Some(thread_id) {
            return Err(MonitorError::NotOwner);
        }

        // Save and release
        let saved_count = state.entry_count;
        state.owner = None;
        state.entry_count = 0;
        self.entry_condvar.notify_one();

        // Block on wait_condvar with periodic interrupt checks.
        // We use short timed waits so that Thread.interrupt() (which only sets
        // a flag) can wake us within a bounded interval.
        //
        // KC16-watchdog: also poll the global stack-dump flag so a thread
        // parked in Object.wait() (e.g. AsyncFutureTask.await ->
        // EnhancedQueueExecutor handoff) observes the watchdog's request
        // and dumps its frame chain from the wait site. Without this
        // poll, a thread that entered wait() before the watchdog fired
        // sits forever in `wait_condvar.wait()` and ack_count stays at
        // 0, leaving the watchdog's only signal as the misleading
        // "main thread is in native (Rust) code" banner.
        //
        // Important: observing the dump flag does NOT consume it and
        // does NOT cause `wait()` to return spuriously. After emitting
        // one frame snapshot per wait call we set a local "already
        // dumped" guard and re-park; this preserves Java semantics
        // (a notify is still required to return) while letting the
        // watchdog see at least one snapshot before it aborts.
        let poll_interval = std::time::Duration::from_millis(5);
        let mut was_interrupted = false;
        let mut frames_dumped = false;
        match timeout_ms {
            Some(ms) if ms > 0 => {
                let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms);
                loop {
                    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                    if remaining.is_zero() {
                        break;
                    }
                    let wait_time = remaining.min(poll_interval);
                    self.wait_condvar.wait_for(&mut state, wait_time);
                    if let Some(flag) = interrupted {
                        if flag.load(std::sync::atomic::Ordering::Acquire) {
                            was_interrupted = true;
                            break;
                        }
                    }
                    if !frames_dumped
                        && stack_dump_wait_flag().load(std::sync::atomic::Ordering::Acquire)
                    {
                        emit_wait_site_frames(thread_id);
                        frames_dumped = true;
                    }
                }
            }
            _ => {
                // Untimed wait. Loop with periodic interrupt checks until
                // notify() wakes us or the interrupt flag is set.
                if let Some(flag) = interrupted {
                    loop {
                        let result = self.wait_condvar.wait_for(&mut state, poll_interval);
                        if flag.load(std::sync::atomic::Ordering::Acquire) {
                            was_interrupted = true;
                            break;
                        }
                        if !frames_dumped
                            && stack_dump_wait_flag()
                                .load(std::sync::atomic::Ordering::Acquire)
                        {
                            emit_wait_site_frames(thread_id);
                            frames_dumped = true;
                        }
                        // If the condvar was signalled (not timed out), break
                        // to allow the caller to re-check its condition.
                        if !result.timed_out() {
                            break;
                        }
                    }
                } else {
                    // No interrupt flag — use a real untimed wait (for unit tests etc.)
                    self.wait_condvar.wait(&mut state);
                }
            }
        }

        // Re-acquire: wait until monitor is unowned or owned by us
        while state.owner.is_some() && state.owner != Some(thread_id) {
            self.entry_condvar.wait(&mut state);
        }
        state.owner = Some(thread_id);
        state.entry_count = saved_count;

        Ok(was_interrupted)
    }

    /// Object.notify() — wake one thread waiting on this monitor.
    ///
    /// The calling thread must own this monitor.
    fn notify(&self, thread_id: ThreadId) -> Result<(), MonitorError> {
        let state = self.state.lock();
        if state.owner != Some(thread_id) {
            return Err(MonitorError::NotOwner);
        }
        self.wait_condvar.notify_one();
        Ok(())
    }

    /// Object.notifyAll() — wake all threads waiting on this monitor.
    ///
    /// The calling thread must own this monitor.
    fn notify_all(&self, thread_id: ThreadId) -> Result<(), MonitorError> {
        let state = self.state.lock();
        if state.owner != Some(thread_id) {
            return Err(MonitorError::NotOwner);
        }
        self.wait_condvar.notify_all();
        Ok(())
    }
}

/// Monitor operation error.
enum MonitorError {
    /// The current thread does not own the monitor.
    NotOwner,
}

// ---------------------------------------------------------------------------
// MonitorTable — global table of monitors keyed by object identity
// ---------------------------------------------------------------------------

/// Global table of JVM monitors, keyed by object pointer address.
///
/// Each Java object can be used as a monitor. Monitors are created lazily
/// when a `monitorenter` instruction first targets an object.
pub struct MonitorTable {
    /// Maps object identity (pointer address) to its monitor.
    /// T10.9.B: FxHashMap — keys are object pointer addresses (internal).
    monitors: Mutex<FxHashMap<usize, Arc<Monitor>>>,
    /// Per-object CAS locks for compareAndSwap operations.
    /// Provides mutual exclusion for non-atomic CAS emulation on Value slots.
    /// T10.9.B: FxHashMap — object pointer addresses (internal).
    cas_locks: Mutex<FxHashMap<usize, Arc<Mutex<()>>>>,
}

impl MonitorTable {
    /// Create an empty monitor table.
    pub fn new() -> Self {
        Self {
            monitors: Mutex::new(FxHashMap::default()),
            cas_locks: Mutex::new(FxHashMap::default()),
        }
    }

    /// Get or create the monitor for the given object.
    fn get_or_create(&self, obj_ref: ObjectRef) -> Arc<Monitor> {
        let key = obj_ref.as_ptr() as usize;
        let mut monitors = self.monitors.lock();
        monitors
            .entry(key)
            .or_insert_with(|| Arc::new(Monitor::new()))
            .clone()
    }

    /// Acquire the monitor for the given object on behalf of the given thread.
    ///
    /// If the monitor is unowned, the thread becomes the owner.
    /// If already owned by this thread, the entry count is incremented (reentrant).
    pub fn enter(&self, obj_ref: ObjectRef, thread_id: ThreadId) {
        let monitor = self.get_or_create(obj_ref);
        monitor.enter(thread_id);
    }

    /// Release the monitor for the given object on behalf of the given thread.
    ///
    /// Returns `Err(MethodCallFailed)` with `IllegalMonitorStateException` if
    /// the calling thread does not own the monitor.
    pub fn exit(&self, obj_ref: ObjectRef, thread_id: ThreadId) -> Result<(), MethodCallFailed> {
        let key = obj_ref.as_ptr() as usize;
        let monitor = {
            let monitors = self.monitors.lock();
            monitors.get(&key).cloned()
        };

        match monitor {
            Some(m) => m.exit(thread_id).map_err(|MonitorError::NotOwner| {
                MethodCallFailed::InternalError(VmError::Runtime(
                    RuntimeError::IllegalMonitorStateException {
                        message: format!(
                            "thread {thread_id} does not own the monitor for object at {key:#x}"
                        ),
                    },
                ))
            }),
            None => {
                // No monitor exists for this object — thread never entered it
                Err(MethodCallFailed::InternalError(VmError::Runtime(
                    RuntimeError::IllegalMonitorStateException {
                        message: format!(
                            "monitorexit on object at {key:#x} that was never entered"
                        ),
                    },
                )))
            }
        }
    }

    /// Perform `Object.wait()` on the monitor for the given object.
    ///
    /// The calling thread must own the monitor. It releases ownership, blocks
    /// until notified (or timed out), then re-acquires the monitor.
    ///
    /// If `interrupted` is provided, the wait will periodically check the flag
    /// and return `Ok(true)` if the thread was interrupted during the wait.
    pub fn wait(
        &self,
        obj_ref: ObjectRef,
        thread_id: ThreadId,
        timeout_ms: Option<u64>,
        interrupted: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<bool, MethodCallFailed> {
        let monitor = self.get_or_create(obj_ref);
        monitor
            .wait(thread_id, timeout_ms, interrupted)
            .map_err(|MonitorError::NotOwner| {
                MethodCallFailed::InternalError(VmError::Runtime(
                    RuntimeError::IllegalMonitorStateException {
                        message: format!(
                            "thread {thread_id} called wait() without owning the monitor"
                        ),
                    },
                ))
            })
    }

    /// Perform `Object.notify()` on the monitor for the given object.
    ///
    /// Wakes one thread waiting on this monitor. The calling thread must own it.
    pub fn notify(&self, obj_ref: ObjectRef, thread_id: ThreadId) -> Result<(), MethodCallFailed> {
        let monitor = self.get_or_create(obj_ref);
        monitor.notify(thread_id).map_err(|MonitorError::NotOwner| {
            MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::IllegalMonitorStateException {
                    message: format!(
                        "thread {thread_id} called notify() without owning the monitor"
                    ),
                },
            ))
        })
    }

    /// Perform `Object.notifyAll()` on the monitor for the given object.
    ///
    /// Wakes all threads waiting on this monitor. The calling thread must own it.
    pub fn notify_all(
        &self,
        obj_ref: ObjectRef,
        thread_id: ThreadId,
    ) -> Result<(), MethodCallFailed> {
        let monitor = self.get_or_create(obj_ref);
        monitor
            .notify_all(thread_id)
            .map_err(|MonitorError::NotOwner| {
                MethodCallFailed::InternalError(VmError::Runtime(
                    RuntimeError::IllegalMonitorStateException {
                        message: format!(
                            "thread {thread_id} called notifyAll() without owning the monitor"
                        ),
                    },
                ))
            })
    }

    /// T1.6.7 — Implements `Thread.holdsLock(Object)`.
    ///
    /// Returns `true` if the given thread currently owns the monitor for
    /// `obj_ref`. Returns `false` if the object has never been entered or
    /// is currently owned by a different thread.
    ///
    /// This is a non-blocking inspection — it briefly takes the monitor
    /// state lock to read the owner field but never waits for entry.
    pub fn holds(&self, obj_ref: ObjectRef, thread_id: ThreadId) -> bool {
        let key = obj_ref.as_ptr() as usize;
        let monitor = {
            let monitors = self.monitors.lock();
            monitors.get(&key).cloned()
        };
        match monitor {
            Some(m) => m.is_held_by(thread_id),
            None => false,
        }
    }

    /// Execute a closure while holding a per-object CAS lock.
    ///
    /// Provides mutual exclusion for non-atomic compare-and-swap emulation.
    /// Each object gets its own lock (lazily created), so CAS operations on
    /// different objects do not contend.
    pub fn with_cas_lock<F, R>(&self, obj_ref: ObjectRef, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        let key = obj_ref.as_ptr() as usize;
        let lock = {
            let mut cas = self.cas_locks.lock();
            cas.entry(key)
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let _guard = lock.lock();
        f()
    }

    /// Remap monitor keys after GC has moved objects.
    ///
    /// Takes a mapping from old pointer addresses to new pointer addresses.
    /// Re-keys the internal HashMap so monitors remain associated with the
    /// correct (now-relocated) objects.
    pub fn remap_after_gc(&self, pointer_map: &std::collections::HashMap<usize, usize>) {
        if pointer_map.is_empty() {
            return;
        }
        let mut monitors = self.monitors.lock();
        // Drain all entries, re-key those whose address has changed
        let entries: Vec<(usize, Arc<Monitor>)> = monitors.drain().collect();
        for (old_key, monitor) in entries {
            let new_key = pointer_map.get(&old_key).copied().unwrap_or(old_key);
            monitors.insert(new_key, monitor);
        }

        // Also remap CAS locks
        let mut cas = self.cas_locks.lock();
        let cas_entries: Vec<(usize, Arc<Mutex<()>>)> = cas.drain().collect();
        for (old_key, lock) in cas_entries {
            let new_key = pointer_map.get(&old_key).copied().unwrap_or(old_key);
            cas.insert(new_key, lock);
        }
    }
}

impl Default for MonitorTable {
    fn default() -> Self {
        Self::new()
    }
}

impl rustjvm_gc::MonitorCleanup for MonitorTable {
    fn remap_after_gc(&self, pointer_map: &std::collections::HashMap<usize, usize>) {
        self.remap_after_gc(pointer_map);
    }
}

impl std::fmt::Debug for MonitorTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.monitors.lock().len();
        f.debug_struct("MonitorTable")
            .field("active_monitors", &count)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classloading::ClassId;
    use crate::memory::heap::Heap;

    /// Helper to create a test object on the heap.
    fn test_object() -> ObjectRef {
        let heap = Heap::new();
        heap.alloc_object(ClassId::new(0), 0)
    }

    #[test]
    fn monitor_enter_exit_basic() {
        let table = MonitorTable::new();
        let obj = test_object();
        let tid = ThreadId(1);

        // Enter and exit should succeed
        table.enter(obj, tid);
        assert!(table.exit(obj, tid).is_ok());
    }

    #[test]
    fn monitor_reentrant() {
        let table = MonitorTable::new();
        let obj = test_object();
        let tid = ThreadId(1);

        // Enter twice, exit twice
        table.enter(obj, tid);
        table.enter(obj, tid);
        assert!(table.exit(obj, tid).is_ok());
        assert!(table.exit(obj, tid).is_ok());
    }

    #[test]
    fn monitor_reentrant_deep() {
        let table = MonitorTable::new();
        let obj = test_object();
        let tid = ThreadId(1);

        // Enter 10 times, exit 10 times
        for _ in 0..10 {
            table.enter(obj, tid);
        }
        for _ in 0..10 {
            assert!(table.exit(obj, tid).is_ok());
        }
    }

    #[test]
    fn monitor_exit_without_enter_fails() {
        let table = MonitorTable::new();
        let obj = test_object();
        let tid = ThreadId(1);

        // Exit without entering should fail
        let result = table.exit(obj, tid);
        assert!(result.is_err());
    }

    #[test]
    fn monitor_exit_wrong_thread_fails() {
        let table = MonitorTable::new();
        let obj = test_object();
        let tid1 = ThreadId(1);
        let tid2 = ThreadId(2);

        // Thread 1 enters
        table.enter(obj, tid1);

        // Thread 2 tries to exit — should fail
        let result = table.exit(obj, tid2);
        assert!(result.is_err());

        // Thread 1 can still exit
        assert!(table.exit(obj, tid1).is_ok());
    }

    #[test]
    fn monitor_different_objects_independent() {
        let heap = Heap::new();
        let obj1 = heap.alloc_object(ClassId::new(0), 0);
        let obj2 = heap.alloc_object(ClassId::new(0), 0);
        let table = MonitorTable::new();
        let tid = ThreadId(1);

        // Enter both objects
        table.enter(obj1, tid);
        table.enter(obj2, tid);

        // Exit them independently
        assert!(table.exit(obj2, tid).is_ok());
        assert!(table.exit(obj1, tid).is_ok());
    }

    #[test]
    fn monitor_reenter_after_release() {
        let table = MonitorTable::new();
        let obj = test_object();
        let tid = ThreadId(1);

        // Enter and exit
        table.enter(obj, tid);
        assert!(table.exit(obj, tid).is_ok());

        // Enter again — should work
        table.enter(obj, tid);
        assert!(table.exit(obj, tid).is_ok());
    }

    #[test]
    fn monitor_remap_after_gc() {
        let table = MonitorTable::new();
        let obj = test_object();
        let tid = ThreadId(1);

        // Enter the monitor
        table.enter(obj, tid);

        // Simulate GC moving the object to a new address
        let old_addr = obj.as_ptr() as usize;
        let heap2 = Heap::new();
        let new_obj = heap2.alloc_object(ClassId::new(0), 0);
        let new_addr = new_obj.as_ptr() as usize;

        let mut pointer_map = std::collections::HashMap::new();
        pointer_map.insert(old_addr, new_addr);

        table.remap_after_gc(&pointer_map);

        // Exit using the NEW address should succeed
        assert!(table.exit(new_obj, tid).is_ok());

        // Exit using the OLD address should fail (key was remapped)
        assert!(table.exit(obj, tid).is_err());
    }

    #[test]
    fn monitor_contention_two_threads() {
        use std::sync::atomic::{AtomicU32, Ordering};

        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let table = Arc::new(MonitorTable::new());
        let counter = Arc::new(AtomicU32::new(0));

        // Thread 1 grabs the monitor, increments counter, releases
        let table1 = table.clone();
        let counter1 = counter.clone();
        let h1 = std::thread::spawn(move || {
            for _ in 0..100 {
                table1.enter(obj, ThreadId(1));
                let val = counter1.load(Ordering::SeqCst);
                counter1.store(val + 1, Ordering::SeqCst);
                table1.exit(obj, ThreadId(1)).unwrap();
            }
        });

        // Thread 2 does the same
        let table2 = table.clone();
        let counter2 = counter.clone();
        let h2 = std::thread::spawn(move || {
            for _ in 0..100 {
                table2.enter(obj, ThreadId(2));
                let val = counter2.load(Ordering::SeqCst);
                counter2.store(val + 1, Ordering::SeqCst);
                table2.exit(obj, ThreadId(2)).unwrap();
            }
        });

        h1.join().unwrap();
        h2.join().unwrap();

        // Without proper locking, a non-atomic read-modify-write would lose increments
        assert_eq!(counter.load(Ordering::SeqCst), 200);
    }

    #[test]
    fn monitor_contention_reentrant() {
        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let table = Arc::new(MonitorTable::new());

        let table1 = table.clone();
        let h1 = std::thread::spawn(move || {
            table1.enter(obj, ThreadId(1));
            table1.enter(obj, ThreadId(1)); // reentrant
            std::thread::sleep(std::time::Duration::from_millis(10));
            table1.exit(obj, ThreadId(1)).unwrap();
            table1.exit(obj, ThreadId(1)).unwrap();
        });

        let table2 = table.clone();
        let h2 = std::thread::spawn(move || {
            // Give thread 1 a head start
            std::thread::sleep(std::time::Duration::from_millis(2));
            table2.enter(obj, ThreadId(2)); // should block until thread 1 exits
            table2.exit(obj, ThreadId(2)).unwrap();
        });

        h1.join().unwrap();
        h2.join().unwrap();
    }

    #[test]
    fn monitor_wait_notify() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let table = Arc::new(MonitorTable::new());
        let flag = Arc::new(AtomicBool::new(false));

        // Consumer: waits for notify
        let table1 = table.clone();
        let flag1 = flag.clone();
        let h1 = std::thread::spawn(move || {
            table1.enter(obj, ThreadId(1));
            // Wait until producer notifies
            while !flag1.load(Ordering::Acquire) {
                table1.wait(obj, ThreadId(1), Some(50), None).unwrap();
            }
            table1.exit(obj, ThreadId(1)).unwrap();
        });

        // Producer: sets flag and notifies
        let table2 = table.clone();
        let flag2 = flag.clone();
        let h2 = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            table2.enter(obj, ThreadId(2));
            flag2.store(true, Ordering::Release);
            table2.notify(obj, ThreadId(2)).unwrap();
            table2.exit(obj, ThreadId(2)).unwrap();
        });

        h1.join().unwrap();
        h2.join().unwrap();
        assert!(flag.load(Ordering::Acquire));
    }

    #[test]
    fn monitor_wait_timeout_expires() {
        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let table = MonitorTable::new();
        let tid = ThreadId(1);

        table.enter(obj, tid);
        // Wait with a short timeout — nobody notifies, so it should return after timeout
        let start = std::time::Instant::now();
        table.wait(obj, tid, Some(50), None).unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_millis() >= 40,
            "Wait should have blocked for ~50ms"
        );
        table.exit(obj, tid).unwrap();
    }

    #[test]
    fn monitor_notify_without_ownership_fails() {
        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let table = MonitorTable::new();

        // Try to notify without owning the monitor
        let result = table.notify(obj, ThreadId(1));
        assert!(result.is_err());
    }

    #[test]
    fn monitor_wait_without_ownership_fails() {
        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let table = MonitorTable::new();

        // Try to wait without owning the monitor
        let result = table.wait(obj, ThreadId(1), None, None);
        assert!(result.is_err());
    }

    // ── Additional edge case tests ────────────────────────────────────

    #[test]
    fn monitor_enter_exit_same_thread_repeated() {
        let table = MonitorTable::new();
        let obj = test_object();
        let tid = ThreadId(1);

        // Multiple enter/exit cycles on the same monitor
        for _ in 0..50 {
            table.enter(obj, tid);
            assert!(table.exit(obj, tid).is_ok());
        }
    }

    #[test]
    fn monitor_reentrant_count_two() {
        let table = MonitorTable::new();
        let obj = test_object();
        let tid = ThreadId(1);

        // Enter twice
        table.enter(obj, tid);
        table.enter(obj, tid);

        // First exit decrements count but monitor is still owned
        assert!(table.exit(obj, tid).is_ok());

        // Exiting again fully releases
        assert!(table.exit(obj, tid).is_ok());

        // Third exit should fail (no longer owned)
        assert!(table.exit(obj, tid).is_err());
    }

    #[test]
    fn monitor_state_after_full_exit() {
        let table = MonitorTable::new();
        let obj = test_object();
        let tid1 = ThreadId(1);
        let tid2 = ThreadId(2);

        // Thread 1 enters and exits
        table.enter(obj, tid1);
        assert!(table.exit(obj, tid1).is_ok());

        // Monitor is now unowned; thread 2 can enter
        table.enter(obj, tid2);
        assert!(table.exit(obj, tid2).is_ok());
    }

    #[test]
    fn monitor_table_creation_defaults() {
        let table = MonitorTable::new();
        // Default table should have no active monitors
        let debug_str = format!("{:?}", table);
        assert!(debug_str.contains("active_monitors"));
        assert!(debug_str.contains("0"));
    }

    #[test]
    fn monitor_table_default_trait() {
        let table = MonitorTable::default();
        let obj = test_object();
        let tid = ThreadId(1);

        // Should work the same as MonitorTable::new()
        table.enter(obj, tid);
        assert!(table.exit(obj, tid).is_ok());
    }

    #[test]
    fn monitor_notify_all_without_ownership_fails() {
        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let table = MonitorTable::new();

        let result = table.notify_all(obj, ThreadId(1));
        assert!(result.is_err());
    }

    #[test]
    fn monitor_cas_lock_basic() {
        let table = MonitorTable::new();
        let obj = test_object();

        // CAS lock should execute the closure and return its result
        let result = table.with_cas_lock(obj, || 42);
        assert_eq!(result, 42);
    }

    #[test]
    fn monitor_remap_empty_map_is_noop() {
        let table = MonitorTable::new();
        let obj = test_object();
        let tid = ThreadId(1);

        table.enter(obj, tid);
        let empty_map = std::collections::HashMap::new();
        table.remap_after_gc(&empty_map);
        // Monitor should still be accessible with original address
        assert!(table.exit(obj, tid).is_ok());
    }
}
