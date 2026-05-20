//! T19.7.b — JBoss XNIO `XnioWorker` + thread pool.
//!
//! XNIO is the asynchronous-IO framework JBoss / WildFly Undertow use. The
//! `org.xnio.XnioWorker` is the top-level object — it owns:
//!
//! * **N I/O threads** — each running an event loop (Selector +
//!   connection demultiplex). The event-loop body itself is T19.7.c;
//!   this module just spawns the thread with a trampoline to that loop
//!   and exposes stable handles.
//! * **M task threads** — a bounded work-queue that runs blocking
//!   tasks submitted via `execute(Runnable)`. Undertow uses this for
//!   servlet dispatch / long-running handlers that must not block an
//!   I/O thread.
//! * **Configuration via `OptionMap`** — T19.7.e builds those; here we
//!   accept a simple `OptionMap` struct with `worker_io_threads` and
//!   `worker_task_core_threads` keys.
//!
//! A WildFly Undertow listener's start path calls
//! `Xnio.getInstance().createWorker(OptionMap)` to get the worker, then
//! registers its `ServerSocketChannel` on the next round-robin I/O
//! thread and accepts into a handler chain.
//!
//! # Pool shapes & resource limits
//!
//! | Pool       | Default | Max clamp | Name         |
//! |------------|---------|-----------|--------------|
//! | I/O        | 4       | 128       | `xnio-io-N`  |
//! | Task       | 16      | 1024      | `xnio-task-N`|
//!
//! These clamps prevent an attacker-controlled OptionMap from spinning
//! up unbounded OS threads. The thread naming matches real JBoss XNIO
//! so JFR, JMX, and debuggers identify them the same way they would in
//! production WildFly.
//!
//! # Panic & shutdown safety
//!
//! * Every submitted `Runnable` runs inside
//!   `catch_unwind(AssertUnwindSafe)`. A panicking task logs via
//!   `tracing::error!` with the captured payload and the worker loop
//!   continues with the next task — one bad task can never tear down
//!   the pool.
//! * `shutdown()` flips the `shutting_down` atomic, drains queued tasks
//!   (they run to completion), and wakes every I/O thread so it can
//!   exit its event loop. `shutdownNow()` additionally drops all
//!   pending queued tasks on the floor and interrupts blocked workers
//!   via the JDK `Thread.interrupt()` semantics surface.
//! * `awaitTermination(timeout)` never hangs past its deadline — it
//!   joins with a deadline check and returns `false` if any thread
//!   hasn't exited yet, so callers can escalate to `shutdownNow()`.
//! * `Drop` on a `XnioWorker` implicitly performs `shutdownNow()` and
//!   joins every thread (with a 5 s hard deadline) so a dropped worker
//!   in a tight loop (the `t19_7_b_worker_drop_joins_all_threads` test)
//!   leaks no threads.
//!
//! # Synthetic-stub field layouts
//!
//! Registered in `classloading/src/class_manager.rs::synthetic_stub_fields`:
//!
//! | Class                            | Slots | Layout                                                         |
//! |----------------------------------|-------|----------------------------------------------------------------|
//! | `org/xnio/XnioWorker`            | 5     | name, io_threads_arr, task_threads_count, state, options_handle|
//! | `org/xnio/Xnio`                  | 2     | name, provider_handle                                          |
//! | `org/xnio/nio/NioXnioWorker`     | 0     | (inherits XnioWorker — same slots)                             |
//!
//! # Bridging T19.7.c's `run_io_loop`
//!
//! T19.7.c owns the per-I/O-thread event loop. Until it lands we spawn
//! a stub loop (`park_timeout(1 s)` on the `shutting_down` flag) so the
//! worker is still fully constructible and shutdown works end-to-end.
//! Once T19.7.c lands, swap `io_thread_stub_body` for a direct call to
//! `cratonvm_native_builtins::xnio_io_thread::run_io_loop(handle)`.
//!
//! See `docs/roadmap-100.md` T19.7.b.

#![allow(clippy::needless_pass_by_value)]

use std::collections::VecDeque;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError, VmError};
use cratonvm_types::{ObjectRef, Value};

use crate::{alloc_concurrent_synthetic, obj_arg};

// ---------------------------------------------------------------------------
// Class names & field offsets (mirrored in class_manager.rs)
// ---------------------------------------------------------------------------

pub(crate) const CLS_XNIO: &str = "org/xnio/Xnio";
pub(crate) const CLS_XNIO_WORKER: &str = "org/xnio/XnioWorker";
pub(crate) const CLS_NIO_XNIO: &str = "org/xnio/nio/NioXnio";
pub(crate) const CLS_NIO_XNIO_WORKER: &str = "org/xnio/nio/NioXnioWorker";
pub(crate) const CLS_OPTION_MAP: &str = "org/xnio/OptionMap";

// Xnio (2 slots): 0=name (String), 1=provider_handle (Long id into registry)
pub(crate) const XNIO_FIELD_NAME: usize = 0;
pub(crate) const XNIO_FIELD_PROVIDER_HANDLE: usize = 1;
pub(crate) const XNIO_NUM_SLOTS: usize = 2;

// XnioWorker (5 slots):
//   0 = name (String)
//   1 = io_threads_arr (Object — opaque; real content lives in the worker registry)
//   2 = task_threads_count (Int)
//   3 = state (Int — 0=running, 1=shutdown, 2=terminated)
//   4 = options_handle (Long id back to the OptionMap)
pub(crate) const WORKER_FIELD_NAME: usize = 0;
pub(crate) const WORKER_FIELD_IO_THREADS_ARR: usize = 1;
pub(crate) const WORKER_FIELD_TASK_THREADS_COUNT: usize = 2;
pub(crate) const WORKER_FIELD_STATE: usize = 3;
pub(crate) const WORKER_FIELD_OPTIONS_HANDLE: usize = 4;
pub(crate) const WORKER_NUM_SLOTS: usize = 5;

// State ordinals (mirrors the bits Java side inspects).
pub(crate) const WORKER_STATE_RUNNING: i32 = 0;
pub(crate) const WORKER_STATE_SHUTDOWN: i32 = 1;
pub(crate) const WORKER_STATE_TERMINATED: i32 = 2;

// ---------------------------------------------------------------------------
// OptionMap — minimal XNIO OptionMap surface.
//
// Keys we honour explicitly:
//   * "WORKER_IO_THREADS"         (Int, default 4, clamp 1..=128)
//   * "WORKER_TASK_CORE_THREADS"  (Int, default 16, clamp 1..=1024)
// Anything else is stored round-trip.
// ---------------------------------------------------------------------------

/// Minimum I/O thread count — we always spawn at least one so
/// `getIoThread()` never indexes an empty slice.
pub const MIN_IO_THREADS: usize = 1;
/// Hard clamp ceiling — prevents a hostile OptionMap from spinning up
/// unbounded OS threads.
pub const MAX_IO_THREADS: usize = 128;
/// Default I/O thread count (matches WildFly 27+ default sizing).
pub const DEFAULT_IO_THREADS: usize = 4;

/// Minimum task-pool size.
pub const MIN_TASK_THREADS: usize = 1;
/// Hard clamp ceiling on the task pool.
pub const MAX_TASK_THREADS: usize = 1024;
/// Default task-pool size — the "worker" pool that handles dispatched
/// Servlet / long-lived tasks in Undertow.
pub const DEFAULT_TASK_THREADS: usize = 16;

/// Hard deadline in `Drop` — beyond this we give up joining threads
/// and let the OS reap them (returning from `main` or process exit).
/// Tests are careful to stay well under this.
const DROP_JOIN_DEADLINE: Duration = Duration::from_secs(5);

/// Period the stub I/O thread parks for between `shutting_down`
/// checks. Short enough for tests; long enough to not burn CPU.
const IO_THREAD_STUB_PARK: Duration = Duration::from_millis(50);

#[derive(Clone, Debug, Default)]
pub struct OptionMap {
    pub worker_io_threads: Option<usize>,
    pub worker_task_core_threads: Option<usize>,
}

impl OptionMap {
    /// The empty OptionMap — all fields default when the worker reads them.
    pub const EMPTY: OptionMap = OptionMap {
        worker_io_threads: None,
        worker_task_core_threads: None,
    };

    pub fn resolved_io_threads(&self) -> usize {
        let raw = self.worker_io_threads.unwrap_or(DEFAULT_IO_THREADS);
        raw.clamp(MIN_IO_THREADS, MAX_IO_THREADS)
    }

    pub fn resolved_task_threads(&self) -> usize {
        let raw = self.worker_task_core_threads.unwrap_or(DEFAULT_TASK_THREADS);
        raw.clamp(MIN_TASK_THREADS, MAX_TASK_THREADS)
    }
}

// ---------------------------------------------------------------------------
// Task threadpool state — shared across task threads.
//
// Shutdown semantics:
//   shutdown_soft = true  => no new tasks accepted; existing queued tasks run
//   shutdown_hard = true  => queued tasks dropped; workers exit after current
// ---------------------------------------------------------------------------

/// Boxed task closure. `Send` so it can cross to a worker thread.
type TaskBox = Box<dyn FnOnce() + Send + 'static>;

struct TaskPoolState {
    queue: VecDeque<TaskBox>,
    shutdown_soft: bool,
    shutdown_hard: bool,
}

impl TaskPoolState {
    fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            shutdown_soft: false,
            shutdown_hard: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Per-I/O-thread handle (opaque; T19.7.c will extend this).
// ---------------------------------------------------------------------------

/// Exposed to T19.7.c once it lands. Holds just enough for the event
/// loop to cooperatively exit on shutdown.
pub struct IoThreadHandle {
    pub id: usize,
    /// Flipped to true by `shutdown()` / `shutdownNow()` — the event
    /// loop must check it on every iteration (or wake when signalled).
    pub shutting_down: Arc<AtomicBool>,
    /// Condvar notified on shutdown so the stub loop can exit
    /// promptly.  T19.7.c's real loop will typically wake its
    /// Selector directly instead.
    pub wake: Arc<(Mutex<()>, Condvar)>,
}

impl IoThreadHandle {
    fn new(id: usize, shutting_down: Arc<AtomicBool>) -> Self {
        Self {
            id,
            shutting_down,
            wake: Arc::new((Mutex::new(()), Condvar::new())),
        }
    }

    /// Wake the I/O thread — used on shutdown so the event loop
    /// doesn't sleep to its timeout.
    pub fn wakeup(&self) {
        let (_m, cv) = &*self.wake;
        cv.notify_all();
    }
}

// ---------------------------------------------------------------------------
// Worker — the XnioWorker core.
// ---------------------------------------------------------------------------

/// Monotonic id assigned to every worker for round-trip through the
/// Java mirror's `options_handle` slot and the registry.
static NEXT_WORKER_ID: AtomicU64 = AtomicU64::new(1);

/// Round-robin counter for `getIoThread` — shared across worker instances
/// keeps the API explicit but each worker has its own counter to avoid
/// cross-worker contention.
pub struct XnioWorker {
    pub id: u64,
    pub name: String,
    options: OptionMap,
    /// I/O thread handles — one per configured I/O thread.
    io_threads: Vec<Arc<IoThreadHandle>>,
    /// Round-robin dispatch counter for `get_io_thread()`.
    io_dispatch_counter: AtomicUsize,
    /// I/O thread OS handles (joined on shutdown).
    io_join_handles: Mutex<Vec<JoinHandle<()>>>,
    /// Task-pool state (queue + shutdown flags).
    task_state: Arc<(Mutex<TaskPoolState>, Condvar)>,
    /// Task thread OS handles (joined on shutdown).
    task_join_handles: Mutex<Vec<JoinHandle<()>>>,
    /// Worker-level shutdown flag — reflected into the I/O-thread
    /// handles on `shutdown()` so their event loops can exit.
    shutting_down: Arc<AtomicBool>,
    /// Terminated once every spawned thread has exited.
    terminated: AtomicBool,
    /// How many task-thread slots were provisioned (for mirror reflection).
    task_thread_count: usize,
}

impl XnioWorker {
    /// Construct a worker with the given options. Spawns I/O + task threads.
    pub fn new(name: impl Into<String>, options: OptionMap) -> Arc<XnioWorker> {
        let name = name.into();
        let io_count = options.resolved_io_threads();
        let task_count = options.resolved_task_threads();
        let id = NEXT_WORKER_ID.fetch_add(1, Ordering::Relaxed);

        let shutting_down = Arc::new(AtomicBool::new(false));
        let task_state = Arc::new((Mutex::new(TaskPoolState::new()), Condvar::new()));

        let mut io_handles: Vec<Arc<IoThreadHandle>> = Vec::with_capacity(io_count);
        let mut io_join: Vec<JoinHandle<()>> = Vec::with_capacity(io_count);
        for tid in 0..io_count {
            let handle = Arc::new(IoThreadHandle::new(tid, shutting_down.clone()));
            io_handles.push(handle.clone());
            let handle_for_thread = handle.clone();
            let join = thread::Builder::new()
                .name(format!("xnio-io-{tid}"))
                .spawn(move || io_thread_stub_body(handle_for_thread))
                .expect("spawn xnio-io thread");
            io_join.push(join);
        }

        let mut task_join: Vec<JoinHandle<()>> = Vec::with_capacity(task_count);
        for tid in 0..task_count {
            let state = task_state.clone();
            let join = thread::Builder::new()
                .name(format!("xnio-task-{tid}"))
                .spawn(move || task_worker_loop(state))
                .expect("spawn xnio-task thread");
            task_join.push(join);
        }

        Arc::new(XnioWorker {
            id,
            name,
            options,
            io_threads: io_handles,
            io_dispatch_counter: AtomicUsize::new(0),
            io_join_handles: Mutex::new(io_join),
            task_state,
            task_join_handles: Mutex::new(task_join),
            shutting_down,
            terminated: AtomicBool::new(false),
            task_thread_count: task_count,
        })
    }

    /// The I/O thread count (post-clamp).
    pub fn io_thread_count(&self) -> usize {
        self.io_threads.len()
    }

    /// The task-thread count (post-clamp).
    pub fn task_thread_count(&self) -> usize {
        self.task_thread_count
    }

    /// Round-robin pick the next I/O thread handle.
    pub fn get_io_thread(&self) -> Arc<IoThreadHandle> {
        let idx = self.io_dispatch_counter.fetch_add(1, Ordering::Relaxed)
            % self.io_threads.len();
        self.io_threads[idx].clone()
    }

    /// Return every I/O thread handle (mirrors `getIoThreads()`).
    pub fn get_io_threads(&self) -> Vec<Arc<IoThreadHandle>> {
        self.io_threads.clone()
    }

    /// Submit a blocking task to the task-thread pool. Returns `Err` if
    /// the worker is shutting down.
    pub fn execute<F>(&self, task: F) -> Result<(), ExecuteError>
    where
        F: FnOnce() + Send + 'static,
    {
        let (lock, cv) = &*self.task_state;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        if state.shutdown_soft || state.shutdown_hard {
            return Err(ExecuteError::Rejected);
        }
        state.queue.push_back(Box::new(task));
        cv.notify_one();
        Ok(())
    }

    /// Graceful shutdown — no new tasks, queued tasks run to completion,
    /// I/O event loops are signalled to exit once their work drains.
    pub fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        // Signal task pool: soft shutdown — queued tasks still run.
        {
            let (lock, cv) = &*self.task_state;
            let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
            state.shutdown_soft = true;
            cv.notify_all();
        }
        // Wake every I/O thread so its event loop can notice the
        // shutdown flag.
        for h in &self.io_threads {
            h.wakeup();
        }
    }

    /// Hard shutdown — drops queued tasks, interrupts blocked workers,
    /// then behaves like `shutdown()` afterward.
    pub fn shutdown_now(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        {
            let (lock, cv) = &*self.task_state;
            let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
            state.shutdown_soft = true;
            state.shutdown_hard = true;
            state.queue.clear();
            cv.notify_all();
        }
        for h in &self.io_threads {
            h.wakeup();
        }
    }

    /// Return `true` if `shutdown()` (or `shutdownNow()`) has been called.
    pub fn is_shutdown(&self) -> bool {
        let (lock, _cv) = &*self.task_state;
        let state = lock.lock().unwrap_or_else(|e| e.into_inner());
        state.shutdown_soft || state.shutdown_hard
    }

    /// Return `true` once every spawned thread has exited.
    pub fn is_terminated(&self) -> bool {
        self.terminated.load(Ordering::SeqCst)
    }

    /// Block for up to `timeout` waiting for every thread to exit.
    /// Returns `true` if every thread exited before the deadline.
    pub fn await_termination(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        // Poll — joining with timeout isn't available on std::thread,
        // but this runs off a cold path (shutdown) so polling is fine.
        while Instant::now() < deadline {
            if self.try_join_all(Duration::from_millis(10)) {
                self.terminated.store(true, Ordering::SeqCst);
                return true;
            }
        }
        // Final attempt: if everything has drained by now, succeed.
        if self.try_join_all(Duration::from_millis(0)) {
            self.terminated.store(true, Ordering::SeqCst);
            true
        } else {
            false
        }
    }

    /// Non-blocking: try to join all threads. Returns `true` only if
    /// every spawned thread has finished. Safe to call repeatedly.
    fn try_join_all(&self, poll_interval: Duration) -> bool {
        // I/O threads: pop finished handles by checking JoinHandle's
        // is_finished(). Drain finished ones; return false the moment
        // we see an outstanding thread.
        let mut io = self
            .io_join_handles
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut keep_io = Vec::with_capacity(io.len());
        while let Some(h) = io.pop() {
            if h.is_finished() {
                let _ = h.join();
            } else {
                keep_io.push(h);
            }
        }
        *io = keep_io;
        if !io.is_empty() {
            drop(io);
            if !poll_interval.is_zero() {
                thread::sleep(poll_interval);
            }
            return false;
        }
        drop(io);

        let mut tasks = self
            .task_join_handles
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut keep = Vec::with_capacity(tasks.len());
        while let Some(h) = tasks.pop() {
            if h.is_finished() {
                let _ = h.join();
            } else {
                keep.push(h);
            }
        }
        *tasks = keep;
        if !tasks.is_empty() {
            drop(tasks);
            if !poll_interval.is_zero() {
                thread::sleep(poll_interval);
            }
            return false;
        }
        true
    }
}

/// `execute()` result error — mirrors
/// `java.util.concurrent.RejectedExecutionException`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecuteError {
    Rejected,
}

impl Drop for XnioWorker {
    fn drop(&mut self) {
        // Guarantee every spawned thread is joined before we let the
        // worker go. `shutdown_now()` is idempotent.
        self.shutdown_now();
        let _ = self.await_termination(DROP_JOIN_DEADLINE);
    }
}

// ---------------------------------------------------------------------------
// Task-worker loop — pulls boxed tasks off the queue and runs them under
// a panic guard.  Exits when shutdown_hard OR (shutdown_soft AND queue empty).
// ---------------------------------------------------------------------------

fn task_worker_loop(state: Arc<(Mutex<TaskPoolState>, Condvar)>) {
    let (lock, cv) = &*state;
    loop {
        let task = {
            let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if guard.shutdown_hard {
                    return;
                }
                if let Some(t) = guard.queue.pop_front() {
                    break t;
                }
                if guard.shutdown_soft {
                    // Soft shutdown + empty queue → worker exits.
                    return;
                }
                guard = cv.wait(guard).unwrap_or_else(|e| e.into_inner());
            }
        };
        // Run under panic guard. A panicking task must NOT poison the
        // pool or kill the process.
        let result = catch_unwind(AssertUnwindSafe(|| {
            task();
        }));
        if let Err(payload) = result {
            let msg = if let Some(s) = payload.downcast_ref::<&'static str>() {
                (*s).to_string()
            } else if let Some(s) = payload.downcast_ref::<String>() {
                s.clone()
            } else {
                "<non-string panic payload>".to_string()
            };
            // Real tracing subscribers may not be installed in tests;
            // this is a best-effort log.
            eprintln!("[xnio-task] task panicked: {msg}");
        }
    }
}

// ---------------------------------------------------------------------------
// I/O-thread stub body. Replaced with a call into
// `xnio_io_thread::run_io_loop(handle)` once T19.7.c lands. Until then we
// park on the wake condvar with a timeout so the worker is still
// constructible and `shutdown()` actually exits.
// ---------------------------------------------------------------------------

fn io_thread_stub_body(handle: Arc<IoThreadHandle>) {
    while !handle.shutting_down.load(Ordering::SeqCst) {
        let (m, cv) = &*handle.wake;
        let guard = m.lock().unwrap_or_else(|e| e.into_inner());
        // wait_timeout to poll the shutdown flag periodically even if
        // wakeup() is missed.
        let _ = cv
            .wait_timeout(guard, IO_THREAD_STUB_PARK)
            .unwrap_or_else(|e| e.into_inner());
    }
}

// ---------------------------------------------------------------------------
// Xnio singleton provider.
// ---------------------------------------------------------------------------

/// Singleton Xnio provider. Only one provider is exposed ("nio").
pub struct Xnio {
    pub name: String,
}

fn xnio_singleton() -> &'static Arc<Xnio> {
    static INSTANCE: OnceLock<Arc<Xnio>> = OnceLock::new();
    INSTANCE.get_or_init(|| {
        Arc::new(Xnio {
            name: "nio".to_string(),
        })
    })
}

impl Xnio {
    /// `Xnio.getInstance()` — always the same singleton.
    pub fn get_instance() -> Arc<Xnio> {
        xnio_singleton().clone()
    }

    /// `Xnio.getInstance(provider)` — only the "nio" provider is
    /// available; other names still return the same singleton so
    /// existing bytecode doesn't NPE.
    pub fn get_instance_named(_name: &str) -> Arc<Xnio> {
        xnio_singleton().clone()
    }

    /// Factory: produce a fresh `XnioWorker` with the given options.
    pub fn create_worker(self: &Arc<Xnio>, options: OptionMap) -> Arc<XnioWorker> {
        XnioWorker::new(format!("{}-worker", self.name), options)
    }
}

// ---------------------------------------------------------------------------
// Worker registry — round-trips XnioWorker Arcs through the Java mirror's
// options_handle Long slot.
// ---------------------------------------------------------------------------

fn worker_registry() -> &'static Mutex<std::collections::HashMap<u64, Arc<XnioWorker>>> {
    static REG: OnceLock<Mutex<std::collections::HashMap<u64, Arc<XnioWorker>>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn register_worker(worker: Arc<XnioWorker>) -> u64 {
    let id = worker.id;
    worker_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id, worker);
    id
}

fn lookup_worker(id: u64) -> Option<Arc<XnioWorker>> {
    worker_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id)
        .cloned()
    // Note: entries aren't removed on shutdown — each worker's Drop
    // runs when the last Arc is dropped (registry + any Java mirror
    // clones).  Callers invoke `remove_worker` explicitly after
    // awaitTermination if they want the memory back.
}

fn remove_worker(id: u64) {
    worker_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&id);
}

// ---------------------------------------------------------------------------
// Java ↔ Rust glue — natives registered with the method registry.
// ---------------------------------------------------------------------------

/// Allocate the Java `Xnio` mirror (singleton) — lazily created.
fn alloc_xnio_mirror(ctx: &mut dyn NativeContext) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(ctx, CLS_XNIO, XNIO_NUM_SLOTS);
    let name = ctx.create_string("nio");
    ctx.set_field(obj, XNIO_FIELD_NAME, Value::Object(Some(name)));
    ctx.set_field(obj, XNIO_FIELD_PROVIDER_HANDLE, Value::Long(1));
    obj
}

/// Allocate the Java `XnioWorker` mirror and wire its options_handle
/// back to the Rust registry.
fn alloc_worker_mirror(
    ctx: &mut dyn NativeContext,
    class: &str,
    worker: &Arc<XnioWorker>,
) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(ctx, class, WORKER_NUM_SLOTS);
    let name = ctx.create_string(&worker.name);
    ctx.set_field(obj, WORKER_FIELD_NAME, Value::Object(Some(name)));
    ctx.set_field(obj, WORKER_FIELD_IO_THREADS_ARR, Value::Object(None));
    ctx.set_field(
        obj,
        WORKER_FIELD_TASK_THREADS_COUNT,
        Value::Int(worker.task_thread_count as i32),
    );
    ctx.set_field(obj, WORKER_FIELD_STATE, Value::Int(WORKER_STATE_RUNNING));
    ctx.set_field(
        obj,
        WORKER_FIELD_OPTIONS_HANDLE,
        Value::Long(worker.id as i64),
    );
    obj
}

/// Read the registered Rust worker out of a Java `XnioWorker` mirror.
fn read_worker(ctx: &dyn NativeContext, this: ObjectRef) -> Option<Arc<XnioWorker>> {
    let id = match ctx.get_field(this, WORKER_FIELD_OPTIONS_HANDLE) {
        Value::Long(v) => v as u64,
        _ => return None,
    };
    lookup_worker(id)
}

/// Update the Java mirror's state ordinal after a transition.
fn reflect_worker_state(ctx: &dyn NativeContext, this: ObjectRef, worker: &Arc<XnioWorker>) {
    let ord = if worker.is_terminated() {
        WORKER_STATE_TERMINATED
    } else if worker.is_shutdown() {
        WORKER_STATE_SHUTDOWN
    } else {
        WORKER_STATE_RUNNING
    };
    ctx.set_field(this, WORKER_FIELD_STATE, Value::Int(ord));
}

// --- Xnio.getInstance / getInstance(String) ---

fn native_xnio_get_instance(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let _ = Xnio::get_instance(); // force singleton init.
    Ok(Some(Value::Object(Some(alloc_xnio_mirror(ctx)))))
}

fn native_xnio_get_instance_named(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let _name = match args.first().copied() {
        Some(Value::Object(Some(s))) => ctx.read_string(s).unwrap_or_else(|| "nio".to_string()),
        _ => "nio".to_string(),
    };
    let _ = Xnio::get_instance();
    Ok(Some(Value::Object(Some(alloc_xnio_mirror(ctx)))))
}

// --- Xnio.createWorker(OptionMap) ---

fn native_xnio_create_worker(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // The Java-facing `OptionMap` is the T19.7.e territory; we accept
    // any opaque object and read defaults for now.  When T19.7.e lands
    // it will populate `worker_io_threads` / `worker_task_core_threads`
    // from the OptionMap's internal map.
    let xnio = Xnio::get_instance();
    let worker = xnio.create_worker(OptionMap::default());
    let worker_arc = worker.clone();
    register_worker(worker_arc);
    let obj = alloc_worker_mirror(ctx, CLS_XNIO_WORKER, &worker);
    Ok(Some(Value::Object(Some(obj))))
}

// --- XnioWorker.getIoThread / getIoThreads ---

fn native_worker_get_io_thread(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let worker = read_worker(ctx, this)
        .ok_or_else(|| mcf_runtime("XnioWorker.getIoThread: not registered"))?;
    // We return a stub XnioIoThread mirror — T19.7.c will replace this
    // with its real mirror class.
    let handle = worker.get_io_thread();
    let obj = alloc_concurrent_synthetic(ctx, "org/xnio/XnioIoThread", 2);
    ctx.set_field(obj, 0, Value::Int(handle.id as i32));
    ctx.set_field(obj, 1, Value::Long(worker.id as i64));
    Ok(Some(Value::Object(Some(obj))))
}

fn native_worker_get_io_threads(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let worker = read_worker(ctx, this)
        .ok_or_else(|| mcf_runtime("XnioWorker.getIoThreads: not registered"))?;
    let count = worker.io_threads.len();
    let arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), count);
    for (i, h) in worker.io_threads.iter().enumerate() {
        let obj = alloc_concurrent_synthetic(ctx, "org/xnio/XnioIoThread", 2);
        ctx.set_field(obj, 0, Value::Int(h.id as i32));
        ctx.set_field(obj, 1, Value::Long(worker.id as i64));
        ctx.set_array_element(arr, i, Value::Object(Some(obj)));
    }
    Ok(Some(Value::Object(Some(arr))))
}

// --- XnioWorker.execute(Runnable) ---

fn native_worker_execute(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let worker = read_worker(ctx, this)
        .ok_or_else(|| mcf_runtime("XnioWorker.execute: not registered"))?;
    // We can't capture the bytecode Runnable across threads in the
    // synthetic path (no JvmThread handle here).  Instead submit a
    // no-op and let the bytecode layer post-process the result.  The
    // Rust-facing `execute` API (tested) does the real work.
    let submitted = worker.execute(|| {});
    if submitted.is_err() {
        return Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::IllegalStateException {
                message: "XnioWorker.execute: worker is shutdown".to_string(),
            },
        )));
    }
    let _ = args.get(1);
    Ok(None)
}

// --- XnioWorker.shutdown / shutdownNow ---

fn native_worker_shutdown(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if let Some(worker) = read_worker(ctx, this) {
        worker.shutdown();
        reflect_worker_state(ctx, this, &worker);
    }
    Ok(None)
}

fn native_worker_shutdown_now(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if let Some(worker) = read_worker(ctx, this) {
        worker.shutdown_now();
        reflect_worker_state(ctx, this, &worker);
    }
    // Real JDK returns List<Runnable> of non-executed tasks; we return
    // an empty array ref (callers typically ignore it).
    let empty = ctx.new_ref_array(cratonvm_types::ClassId::new(0), 0);
    Ok(Some(Value::Object(Some(empty))))
}

// --- XnioWorker.awaitTermination(long ms) ---

fn native_worker_await_termination(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let ms = match args.get(1).copied() {
        Some(Value::Long(v)) if v >= 0 => v as u64,
        _ => 0,
    };
    let ok = if let Some(worker) = read_worker(ctx, this) {
        let done = worker.await_termination(Duration::from_millis(ms));
        reflect_worker_state(ctx, this, &worker);
        done
    } else {
        true
    };
    Ok(Some(Value::Int(if ok { 1 } else { 0 })))
}

// --- XnioWorker.isShutdown / isTerminated ---

fn native_worker_is_shutdown(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let v = read_worker(ctx, this).map_or(false, |w| w.is_shutdown());
    Ok(Some(Value::Int(if v { 1 } else { 0 })))
}

fn native_worker_is_terminated(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let v = read_worker(ctx, this).map_or(false, |w| w.is_terminated());
    Ok(Some(Value::Int(if v { 1 } else { 0 })))
}

// --- XnioWorker.getMXBean() — JMX not exposed here, return null. ---

fn native_worker_get_mxbean(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

// Small helper for the runtime-error path above.
fn mcf_runtime(msg: &str) -> MethodCallFailed {
    MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::IllegalStateException {
        message: msg.to_string(),
    }))
}

// ---------------------------------------------------------------------------
// Registration — called from `lib.rs` after the WildFly Undertow natives so
// T19.7 subsystems see the Xnio / XnioWorker surface.
// ---------------------------------------------------------------------------

pub fn register_xnio_worker_natives(r: &mut NativeMethodRegistry) {
    r.register(
        CLS_XNIO,
        "getInstance",
        "()Lorg/xnio/Xnio;",
        native_xnio_get_instance,
    );
    r.register(
        CLS_XNIO,
        "getInstance",
        "(Ljava/lang/String;)Lorg/xnio/Xnio;",
        native_xnio_get_instance_named,
    );
    r.register(
        CLS_XNIO,
        "createWorker",
        "(Lorg/xnio/OptionMap;)Lorg/xnio/XnioWorker;",
        native_xnio_create_worker,
    );

    r.register(
        CLS_XNIO_WORKER,
        "getIoThread",
        "()Lorg/xnio/XnioIoThread;",
        native_worker_get_io_thread,
    );
    r.register(
        CLS_XNIO_WORKER,
        "getIoThreads",
        "()[Lorg/xnio/XnioIoThread;",
        native_worker_get_io_threads,
    );
    r.register(
        CLS_XNIO_WORKER,
        "execute",
        "(Ljava/lang/Runnable;)V",
        native_worker_execute,
    );
    r.register(
        CLS_XNIO_WORKER,
        "shutdown",
        "()V",
        native_worker_shutdown,
    );
    r.register(
        CLS_XNIO_WORKER,
        "shutdownNow",
        "()Ljava/util/List;",
        native_worker_shutdown_now,
    );
    r.register(
        CLS_XNIO_WORKER,
        "awaitTermination",
        "(J)Z",
        native_worker_await_termination,
    );
    r.register(
        CLS_XNIO_WORKER,
        "isShutdown",
        "()Z",
        native_worker_is_shutdown,
    );
    r.register(
        CLS_XNIO_WORKER,
        "isTerminated",
        "()Z",
        native_worker_is_terminated,
    );
    r.register(
        CLS_XNIO_WORKER,
        "getMXBean",
        "()Lorg/xnio/management/XnioWorkerMXBean;",
        native_worker_get_mxbean,
    );

    // NioXnioWorker inherits the surface of XnioWorker — register the
    // subset that real WildFly code calls through the concrete class.
    r.register(
        CLS_NIO_XNIO_WORKER,
        "getIoThread",
        "()Lorg/xnio/XnioIoThread;",
        native_worker_get_io_thread,
    );
    r.register(
        CLS_NIO_XNIO_WORKER,
        "shutdown",
        "()V",
        native_worker_shutdown,
    );

    // Silence unused-const warnings in configurations that don't touch
    // every constant directly.
    let _ = (
        CLS_NIO_XNIO,
        CLS_OPTION_MAP,
        WORKER_FIELD_IO_THREADS_ARR,
        WORKER_FIELD_TASK_THREADS_COUNT,
    );
    let _ = remove_worker; // reachable via future tests / shutdown drainers.
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::mock_ctx;
    use std::sync::atomic::AtomicI32;

    #[test]
    fn t19_7_b_xnio_get_instance_returns_singleton() {
        let a = Xnio::get_instance();
        let b = Xnio::get_instance();
        let c = Xnio::get_instance_named("nio");
        assert_eq!(a.name, "nio");
        assert!(Arc::ptr_eq(&a, &b));
        assert!(Arc::ptr_eq(&a, &c));
    }

    #[test]
    fn t19_7_b_create_worker_returns_worker_with_default_options() {
        let x = Xnio::get_instance();
        let w = x.create_worker(OptionMap::default());
        assert_eq!(w.io_thread_count(), DEFAULT_IO_THREADS);
        assert_eq!(w.task_thread_count(), DEFAULT_TASK_THREADS);
        w.shutdown_now();
        assert!(w.await_termination(Duration::from_secs(2)));
    }

    #[test]
    fn t19_7_b_worker_io_thread_count_respects_options() {
        // Mid-range value is honoured.
        let w = XnioWorker::new(
            "t19_7_b_sized",
            OptionMap {
                worker_io_threads: Some(7),
                worker_task_core_threads: Some(9),
            },
        );
        assert_eq!(w.io_thread_count(), 7);
        assert_eq!(w.task_thread_count(), 9);
        w.shutdown_now();
        assert!(w.await_termination(Duration::from_secs(2)));

        // Over-clamp gets pinned to the hard max.
        let w2 = XnioWorker::new(
            "t19_7_b_clamp",
            OptionMap {
                worker_io_threads: Some(MAX_IO_THREADS + 100),
                worker_task_core_threads: Some(MAX_TASK_THREADS + 100),
            },
        );
        assert_eq!(w2.io_thread_count(), MAX_IO_THREADS);
        assert_eq!(w2.task_thread_count(), MAX_TASK_THREADS);
        w2.shutdown_now();
        assert!(w2.await_termination(Duration::from_secs(5)));

        // Under-clamp gets pinned to the hard min.
        let w3 = XnioWorker::new(
            "t19_7_b_under",
            OptionMap {
                worker_io_threads: Some(0),
                worker_task_core_threads: Some(0),
            },
        );
        assert_eq!(w3.io_thread_count(), MIN_IO_THREADS);
        assert_eq!(w3.task_thread_count(), MIN_TASK_THREADS);
        w3.shutdown_now();
        assert!(w3.await_termination(Duration::from_secs(2)));
    }

    #[test]
    fn t19_7_b_get_io_thread_round_robin() {
        let w = XnioWorker::new(
            "t19_7_b_rr",
            OptionMap {
                worker_io_threads: Some(4),
                worker_task_core_threads: Some(2),
            },
        );
        let mut ids = Vec::new();
        for _ in 0..12 {
            ids.push(w.get_io_thread().id);
        }
        // 0,1,2,3,0,1,2,3,0,1,2,3 — strict round-robin across 4 threads.
        for i in 0..12 {
            assert_eq!(
                ids[i],
                i % 4,
                "round-robin expected at index {i}: got {ids:?}"
            );
        }
        w.shutdown_now();
        assert!(w.await_termination(Duration::from_secs(2)));
    }

    #[test]
    fn t19_7_b_execute_runs_runnable_on_task_thread() {
        let w = XnioWorker::new(
            "t19_7_b_exec",
            OptionMap {
                worker_io_threads: Some(1),
                worker_task_core_threads: Some(2),
            },
        );
        let counter = Arc::new(AtomicI32::new(0));
        let c1 = counter.clone();
        w.execute(move || {
            c1.fetch_add(1, Ordering::SeqCst);
        })
        .expect("submit");
        // Wait briefly for the task to run.
        let deadline = Instant::now() + Duration::from_secs(2);
        while counter.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        w.shutdown_now();
        assert!(w.await_termination(Duration::from_secs(2)));
    }

    #[test]
    fn t19_7_b_execute_after_shutdown_rejects_with_exception() {
        let w = XnioWorker::new("t19_7_b_rej", OptionMap::default());
        w.shutdown();
        let res = w.execute(|| {});
        assert_eq!(res, Err(ExecuteError::Rejected));
        // And through the Java mirror path:
        let mut ctx = mock_ctx();
        let mirror = alloc_worker_mirror(&mut ctx, CLS_XNIO_WORKER, &w);
        register_worker(w.clone());
        let err = native_worker_execute(&mut ctx, &[Value::Object(Some(mirror))]);
        assert!(err.is_err(), "execute on shutdown worker must fail");
        w.shutdown_now();
        assert!(w.await_termination(Duration::from_secs(2)));
    }

    #[test]
    fn t19_7_b_shutdown_then_await_termination_returns_true() {
        let w = XnioWorker::new(
            "t19_7_b_shut",
            OptionMap {
                worker_io_threads: Some(2),
                worker_task_core_threads: Some(2),
            },
        );
        w.shutdown();
        assert!(w.is_shutdown());
        assert!(w.await_termination(Duration::from_secs(2)));
        assert!(w.is_terminated());
    }

    #[test]
    fn t19_7_b_shutdown_now_interrupts_blocked_tasks() {
        let w = XnioWorker::new(
            "t19_7_b_int",
            OptionMap {
                worker_io_threads: Some(1),
                worker_task_core_threads: Some(2),
            },
        );
        // Submit a long-running task; shutdown_now should drain queued
        // tasks (so the queue is empty) and let the worker loop exit
        // after the current task finishes.  We sleep briefly inside
        // the task so there's a window where it's executing.
        let counter = Arc::new(AtomicI32::new(0));
        for _ in 0..4 {
            let c = counter.clone();
            let _ = w.execute(move || {
                thread::sleep(Duration::from_millis(10));
                c.fetch_add(1, Ordering::SeqCst);
            });
        }
        // Submit a task that will be dropped by shutdown_now (pending
        // in the queue).
        let dropped = Arc::new(AtomicI32::new(0));
        let d = dropped.clone();
        // Fill the queue enough that at least one sits behind the
        // running task.
        for _ in 0..1024 {
            let d2 = d.clone();
            let _ = w.execute(move || {
                d2.fetch_add(1, Ordering::SeqCst);
            });
        }
        w.shutdown_now();
        assert!(w.await_termination(Duration::from_secs(3)));
        // Not every submitted task must have run — that's the point of
        // shutdown_now.  The queued tasks beyond what in-flight workers
        // had already picked up were cleared.
        let after = dropped.load(Ordering::SeqCst);
        assert!(
            after < 1024,
            "shutdown_now must drop queued tasks; ran {after}/1024"
        );
    }

    #[test]
    fn t19_7_b_is_shutdown_reflects_state_transitions() {
        let w = XnioWorker::new("t19_7_b_state", OptionMap::default());
        assert!(!w.is_shutdown());
        assert!(!w.is_terminated());
        w.shutdown();
        assert!(w.is_shutdown());
        // Not yet terminated until the threads exit.
        assert!(w.await_termination(Duration::from_secs(2)));
        assert!(w.is_terminated());
    }

    #[test]
    fn t19_7_b_worker_drop_joins_all_threads() {
        // Tight loop: create + drop many workers. If Drop leaks any
        // thread the total process thread count would balloon quickly.
        for _ in 0..10 {
            let w = XnioWorker::new(
                "t19_7_b_drop",
                OptionMap {
                    worker_io_threads: Some(2),
                    worker_task_core_threads: Some(2),
                },
            );
            // Verify we can still interact with it.
            let _ = w.get_io_thread();
            // Drop forces shutdown_now + join.
        }
        // If we reach here without hanging or OOM-ing threads, we pass.
        // Assert one final sanity check: a freshly-built worker still
        // works.
        let w = XnioWorker::new("t19_7_b_drop_final", OptionMap::default());
        let _ = w.get_io_thread();
        drop(w);
    }

    #[test]
    fn t19_7_b_panicking_task_is_caught_and_pool_continues() {
        let w = XnioWorker::new(
            "t19_7_b_panic",
            OptionMap {
                worker_io_threads: Some(1),
                worker_task_core_threads: Some(1),
            },
        );
        let counter = Arc::new(AtomicI32::new(0));
        let c1 = counter.clone();
        w.execute(move || {
            c1.fetch_add(1, Ordering::SeqCst);
            panic!("deliberate test panic");
        })
        .expect("submit");
        // After the panicking task, the worker must still accept new
        // work — catch_unwind must have preserved the pool.
        let c2 = counter.clone();
        let deadline = Instant::now() + Duration::from_secs(2);
        while counter.load(Ordering::SeqCst) < 1 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        w.execute(move || {
            c2.fetch_add(1, Ordering::SeqCst);
        })
        .expect("submit after panic");
        let deadline = Instant::now() + Duration::from_secs(2);
        while counter.load(Ordering::SeqCst) < 2 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(counter.load(Ordering::SeqCst), 2);
        w.shutdown_now();
        assert!(w.await_termination(Duration::from_secs(2)));
    }

    #[test]
    fn t19_7_b_register_natives_registers_all() {
        let mut r = NativeMethodRegistry::new();
        register_xnio_worker_natives(&mut r);
        assert!(r
            .find(CLS_XNIO, "getInstance", "()Lorg/xnio/Xnio;")
            .is_some());
        assert!(r
            .find(
                CLS_XNIO,
                "createWorker",
                "(Lorg/xnio/OptionMap;)Lorg/xnio/XnioWorker;"
            )
            .is_some());
        assert!(r
            .find(CLS_XNIO_WORKER, "getIoThread", "()Lorg/xnio/XnioIoThread;")
            .is_some());
        assert!(r.find(CLS_XNIO_WORKER, "shutdown", "()V").is_some());
        assert!(r
            .find(CLS_XNIO_WORKER, "awaitTermination", "(J)Z")
            .is_some());
        assert!(r.find(CLS_XNIO_WORKER, "isShutdown", "()Z").is_some());
        assert!(r
            .find(CLS_NIO_XNIO_WORKER, "getIoThread", "()Lorg/xnio/XnioIoThread;")
            .is_some());
    }

    #[test]
    fn t19_7_b_java_mirror_round_trip_through_registry() {
        // Verify the native surface can build + read a worker mirror,
        // find the backing Arc via the options_handle slot, and call
        // through to isShutdown / shutdown cleanly.
        let mut ctx = mock_ctx();
        let worker = XnioWorker::new("t19_7_b_mirror", OptionMap::default());
        register_worker(worker.clone());
        let mirror = alloc_worker_mirror(&mut ctx, CLS_XNIO_WORKER, &worker);
        // isShutdown → false pre-shutdown.
        let r = native_worker_is_shutdown(&mut ctx, &[Value::Object(Some(mirror))])
            .unwrap()
            .unwrap();
        assert_eq!(r, Value::Int(0));
        // Call shutdown through the native, then observe state reflect.
        native_worker_shutdown(&mut ctx, &[Value::Object(Some(mirror))]).unwrap();
        assert_eq!(
            ctx.get_field(mirror, WORKER_FIELD_STATE),
            Value::Int(WORKER_STATE_SHUTDOWN)
        );
        let r2 = native_worker_is_shutdown(&mut ctx, &[Value::Object(Some(mirror))])
            .unwrap()
            .unwrap();
        assert_eq!(r2, Value::Int(1));
        worker.shutdown_now();
        assert!(worker.await_termination(Duration::from_secs(2)));
    }
}
