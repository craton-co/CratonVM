//! Per-thread JVM execution state.
//!
//! Each Java thread has its own `JvmThread` containing:
//! - Call stack (for stack traces)
//! - Throwable stack traces captured by `fillInStackTrace`
//! - Test output buffer (`printed`)
//! - Thread identity and flags
//
// T1.8.2 — production-code panic gate. Per-thread state is on the
// hottest paths in the VM; an `.unwrap()` here would panic the entire
// runtime. Test code is allowed unwraps so the gate is opt-out under
// `#[cfg(test)]`.

#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
    )
)]

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use parking_lot::{Condvar as PLCondvar, Mutex as PLMutex};

use crate::classloading::resolution::InvokeCache;
use crate::native::registry::StackTraceEntry;
use crate::runtime::frame::Frame;
use crate::types::{ObjectRef, Value};

/// Pool type for SoA locals and stack vecs: (values, tags).
pub type SoaPool = Vec<(Vec<u64>, Vec<u8>)>;

// Pool size limits — prevent unbounded growth
const MAX_POOL_SIZE: usize = 64;

// ---------------------------------------------------------------------------
// ParkState — binary semaphore for LockSupport.park() / unpark()
// ---------------------------------------------------------------------------

/// Per-thread parking permit for `LockSupport.park()` / `unpark()`.
///
/// Implements a binary semaphore: `unpark()` sets the permit, `park()` consumes
/// it (or blocks until one is available). If `unpark()` is called before
/// `park()`, the next `park()` returns immediately.
pub struct ParkState {
    mutex: PLMutex<bool>,
    condvar: PLCondvar,
}

impl ParkState {
    /// Create a new ParkState with no permit available.
    pub fn new() -> Self {
        Self {
            mutex: PLMutex::new(false),
            condvar: PLCondvar::new(),
        }
    }

    /// Consume the permit or block until one is available.
    ///
    /// If a permit is available (from a prior `unpark()`), consumes it and
    /// returns immediately. Otherwise, blocks until `unpark()` is called or
    /// the timeout expires.
    pub fn park(&self, timeout: Option<std::time::Duration>) {
        let mut permit = self.mutex.lock();
        if *permit {
            *permit = false;
            return;
        }
        match timeout {
            Some(dur) if !dur.is_zero() => {
                self.condvar.wait_for(&mut permit, dur);
            }
            None => {
                self.condvar.wait(&mut permit);
            }
            _ => {} // zero timeout = no-op
        }
        *permit = false; // consume permit even on timeout/spurious wakeup
    }

    /// Consume the permit or block until one is available, with interrupt awareness.
    ///
    /// Like `park()`, but periodically checks the interrupt flag so that
    /// `Thread.interrupt()` can break a parked thread out of the wait.
    pub fn park_interruptible(
        &self,
        timeout: Option<std::time::Duration>,
        interrupted: &std::sync::atomic::AtomicBool,
    ) {
        let mut permit = self.mutex.lock();
        if *permit {
            *permit = false;
            return;
        }
        // Check interrupt before blocking
        if interrupted.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        let poll = std::time::Duration::from_millis(5);
        match timeout {
            Some(dur) if !dur.is_zero() => {
                let deadline = std::time::Instant::now() + dur;
                loop {
                    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                    if remaining.is_zero() || *permit {
                        break;
                    }
                    let wait_time = remaining.min(poll);
                    self.condvar.wait_for(&mut permit, wait_time);
                    if *permit || interrupted.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }
                }
            }
            None => {
                // Untimed park — poll for interrupt or unpark
                loop {
                    self.condvar.wait_for(&mut permit, poll);
                    if *permit || interrupted.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }
                }
            }
            _ => {} // zero timeout = no-op
        }
        *permit = false;
    }

    /// Make a permit available; unblock a parked thread.
    ///
    /// If the thread is currently parked, it will be unblocked. If not,
    /// the next call to `park()` will return immediately.
    pub fn unpark(&self) {
        let mut permit = self.mutex.lock();
        *permit = true;
        self.condvar.notify_one();
    }
}

impl Default for ParkState {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether a thread is a platform (OS) thread or a virtual thread (JEP 444).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadKind {
    /// Traditional platform thread backed by an OS thread.
    Platform,
    /// Virtual thread (Java 21+) — lightweight, scheduled onto carrier threads.
    Virtual,
}

/// Unique identifier for a JVM thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ThreadId(pub u64);

impl std::fmt::Display for ThreadId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Thread-{}", self.0)
    }
}

/// Per-thread execution state.
///
/// Each Java thread (including the main thread) has its own `JvmThread`.
/// This struct holds all state that is local to a single thread of execution.
pub struct JvmThread {
    /// Unique thread identifier.
    pub thread_id: ThreadId,

    /// Human-readable thread name.
    pub name: String,

    /// Stack traces captured by `fillInStackTrace`, keyed by identity hash.
    pub throwable_stacks: HashMap<i32, Vec<StackTraceEntry>>,

    /// Live execution frames. The last element is the currently executing frame.
    /// Frames are pushed on method entry and popped on method return.
    /// Stack traces are derived from frames on demand (in capture_stack_trace).
    pub frames: Vec<Frame>,

    /// Pool of reusable locals (values, tags) pairs (avoids allocation on recursive calls).
    pub locals_pool: SoaPool,

    /// Pool of reusable stack (values, tags) pairs (avoids allocation on recursive calls).
    pub stacks_pool: SoaPool,

    /// Values printed by `tempPrint` — used by integration tests.
    pub printed: Vec<Value>,

    /// Lines printed via System.out.println — captured as plain Rust strings.
    pub printed_lines: Vec<String>,

    /// Whether this is a daemon thread.
    pub daemon: bool,

    /// Thread interrupt flag (shared with ThreadRegistry for cross-thread access).
    pub interrupted: Arc<AtomicBool>,

    /// Cached Java Thread object on the heap for this thread.
    pub java_thread_obj: Option<ObjectRef>,

    /// Parking permit for LockSupport.park()/unpark().
    pub park_state: Arc<ParkState>,

    /// Root snapshot: shared with ThreadRegistry for GC root scanning across threads.
    /// Updated at safepoints and before blocking operations.
    pub root_snapshot: Arc<parking_lot::Mutex<Vec<ObjectRef>>>,

    /// Thread-local invoke cache — maps (caller_class, cp_index) to resolved targets.
    /// No locking needed since each thread owns its cache.
    pub invoke_cache: InvokeCache,

    /// Whether this is a platform or virtual thread (JEP 444, Java 21).
    pub kind: ThreadKind,

    /// Pin depth for virtual threads (JEP 491).
    ///
    /// Each monitor enter increments this; each monitor exit decrements.
    /// When non-zero on a virtual thread, the thread is "pinned" to its
    /// carrier — attempting to park/sleep will keep the carrier blocked
    /// and emit a `jdk.VirtualThreadPinned` JFR event.
    pub pin_count: u32,

    /// Last pin reason (for JFR event classification).
    /// "Monitor", "NativeMethod", "ClassInit", or empty when not pinned.
    pub pin_reason: &'static str,

    /// Scoped value binding stack (JEP 446, Java 25).
    /// Each entry is (key_id, bound_value). Searched top-to-bottom.
    pub scoped_values: Vec<(u64, Value)>,

    /// Thread-local allocation buffer for lock-free young-gen allocation.
    pub tlab: rustjvm_gc::Tlab,

    /// T1.5.1 — pending asynchronous exception to deliver at the next
    /// safepoint.
    ///
    /// When `Some(throwable)`, the next call to `safepoint_check`
    /// clears this field and raises the throwable so it propagates
    /// through the interpreter's normal exception table walk.
    /// Used by `Thread.stop0` and by any future API that needs to
    /// deliver a Throwable to a running thread without corrupting
    /// stack state.
    ///
    /// The field is owned by the target thread; cross-thread setters
    /// (the `Thread.stop0` native) go through the thread registry
    /// which takes a brief lock and then sets this via an
    /// `AsyncExceptionSlot` (see `thread_registry.rs`).
    ///
    /// The stored `ObjectRef` must point to a live `Throwable`
    /// subclass. The field is cleared by the safepoint before raise,
    /// so a single `stop()` call delivers exactly once.
    pub pending_async_exception: Option<ObjectRef>,

    /// T17.Δ.3 — JVMTI single-step enable for this thread.
    ///
    /// When set, the interpreter's per-instruction dispatch fires
    /// `JvmtiEventKind::SingleStep` once for each bytecode.  Set by
    /// `JvmtiEnv::set_event_notification_mode(Enable, SingleStep, Some(tid))`
    /// and cleared by the corresponding `Disable` call.
    ///
    /// Stored as `AtomicBool` because the flag may be flipped from a
    /// debugger thread while the owning thread is executing bytecode; the
    /// owning thread reads it with `Ordering::Relaxed` on the dispatch-loop
    /// hot path (single-thread view of its own flag is always consistent).
    pub single_step_enabled: AtomicBool,

    /// T17.Δ.5 — per-thread JVMTI frame-pop requests.
    ///
    /// Each entry is a frame depth (index into `frames`) for which
    /// `NotifyFramePop` was called.  When the interpreter is about to drop
    /// a frame it consults this list; if the frame's depth matches any
    /// registered request, `FramePop` fires and the entry is removed.
    ///
    /// Implemented as a small `Vec<u32>` because in practice only a handful
    /// of frames are marked at any given time (debugger "step out" uses
    /// one; profilers typically use zero).  Linear scan cost is negligible
    /// next to the frame-pop work.
    pub frame_pop_requests: Vec<u32>,
}

impl JvmThread {
    /// Create a new thread with the given id and name.
    pub fn new(thread_id: ThreadId, name: &str) -> Self {
        Self {
            thread_id,
            name: name.to_string(),
            throwable_stacks: HashMap::new(),
            frames: Vec::new(),
            locals_pool: Vec::new(),
            stacks_pool: Vec::new(),
            printed: Vec::new(),
            printed_lines: Vec::new(),
            daemon: false,
            interrupted: Arc::new(AtomicBool::new(false)),
            java_thread_obj: None,
            park_state: Arc::new(ParkState::new()),
            root_snapshot: Arc::new(parking_lot::Mutex::new(Vec::new())),
            invoke_cache: InvokeCache::new(),
            kind: ThreadKind::Platform,
            pin_count: 0,
            pin_reason: "",
            scoped_values: Vec::new(),
            tlab: rustjvm_gc::Tlab::empty(),
            pending_async_exception: None,
            single_step_enabled: AtomicBool::new(false),
            frame_pop_requests: Vec::new(),
        }
    }

    /// Return a popped frame's Vec allocations to the pool for reuse.
    pub fn recycle_frame(&mut self, frame: Frame) {
        if self.locals_pool.len() < MAX_POOL_SIZE {
            frame.recycle(&mut self.locals_pool, &mut self.stacks_pool);
        }
        // If pool is full, frame's Vecs are simply dropped
    }

    /// T10.7 — recycle a frame, spilling any overflow into the VM-wide
    /// `VecPool`s on `SharedVm`.
    ///
    /// Semantics:
    ///   * When the thread-local `locals_pool` / `stacks_pool` has room, the
    ///     frame's Vecs are retained there exactly as before (zero locking).
    ///   * Once the thread-local pool is full (`MAX_POOL_SIZE`), we keep the
    ///     frame's Vecs alive by handing them to the VM-wide shared pools
    ///     (`operand_stack_pool` for the `Vec<u64>` halves and `tag_pool` for
    ///     the `Vec<u8>` halves).  Sibling threads later pull from the shared
    ///     pool when their own thread-local pool is empty (see
    ///     `refill_pools_from_shared`).
    ///
    /// This preserves allocations across thread boundaries without impacting
    /// the hot path — the shared mutex is only touched on the cold
    /// "thread-local pool full" edge.
    pub fn recycle_frame_with_shared(
        &mut self,
        frame: Frame,
        operand_stack_pool: &crate::runtime::alloc_fastpath::VecPool<u64>,
        tag_pool: &crate::runtime::alloc_fastpath::VecPool<u8>,
    ) {
        // Split the frame's inner Vecs out of it exactly once.
        let (local_vals, local_tags, stack_vals, stack_tags) = frame.take_pool_parts();
        if self.locals_pool.len() < MAX_POOL_SIZE {
            self.locals_pool.push((local_vals, local_tags));
            self.stacks_pool.push((stack_vals, stack_tags));
            return;
        }
        // Thread-local pool is full — spill into the shared pools.
        operand_stack_pool.release(local_vals);
        tag_pool.release(local_tags);
        operand_stack_pool.release(stack_vals);
        tag_pool.release(stack_tags);
    }

    /// T10.7 — replenish the thread-local `locals_pool` / `stacks_pool` from
    /// the VM-wide shared pools when empty.
    ///
    /// Invoked just before a frame-push that will consume pooled Vecs so the
    /// shared-pool mutex is touched only on the cold refill path, not per call.
    pub fn refill_pools_from_shared(
        &mut self,
        operand_stack_pool: &crate::runtime::alloc_fastpath::VecPool<u64>,
        tag_pool: &crate::runtime::alloc_fastpath::VecPool<u8>,
        max_locals: usize,
        max_stack: usize,
    ) {
        if self.locals_pool.is_empty() {
            let vals = operand_stack_pool.acquire(max_locals);
            let tags = tag_pool.acquire(max_locals);
            self.locals_pool.push((vals, tags));
        }
        if self.stacks_pool.is_empty() {
            let vals = operand_stack_pool.acquire(max_stack);
            let tags = tag_pool.acquire(max_stack);
            self.stacks_pool.push((vals, tags));
        }
    }
}

impl Default for JvmThread {
    /// Creates a default "main" thread with id 0.
    /// Used with `std::mem::take` for borrow-splitting.
    fn default() -> Self {
        Self::new(ThreadId(0), "main")
    }
}

impl std::fmt::Debug for JvmThread {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JvmThread")
            .field("thread_id", &self.thread_id)
            .field("name", &self.name)
            .field("frames_depth", &self.frames.len())
            .field("daemon", &self.daemon)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_creation() {
        let thread = JvmThread::new(ThreadId(1), "worker-1");
        assert_eq!(thread.thread_id, ThreadId(1));
        assert_eq!(thread.name, "worker-1");
        assert!(thread.frames.is_empty());
        assert!(thread.printed.is_empty());
        assert!(!thread.daemon);
    }

    #[test]
    fn thread_default_is_main() {
        let thread = JvmThread::default();
        assert_eq!(thread.thread_id, ThreadId(0));
        assert_eq!(thread.name, "main");
    }

    #[test]
    fn thread_id_display() {
        assert_eq!(format!("{}", ThreadId(0)), "Thread-0");
        assert_eq!(format!("{}", ThreadId(42)), "Thread-42");
    }

    #[test]
    fn park_state_unpark_before_park() {
        let ps = ParkState::new();
        // Unpark first → next park returns immediately
        ps.unpark();
        ps.park(Some(std::time::Duration::from_millis(100)));
        // If park didn't return immediately, the test would timeout
    }

    #[test]
    fn park_state_park_with_timeout() {
        let ps = ParkState::new();
        let start = std::time::Instant::now();
        ps.park(Some(std::time::Duration::from_millis(50)));
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_millis() >= 40,
            "Park should have blocked for ~50ms, got {}ms",
            elapsed.as_millis()
        );
    }

    #[test]
    fn park_state_unpark_wakes_parked_thread() {
        let ps = Arc::new(ParkState::new());
        let ps2 = ps.clone();
        let handle = std::thread::spawn(move || {
            // This will block until unparked
            ps2.park(Some(std::time::Duration::from_secs(5)));
        });
        // Give the thread time to park
        std::thread::sleep(std::time::Duration::from_millis(20));
        ps.unpark();
        handle.join().unwrap();
    }
}
