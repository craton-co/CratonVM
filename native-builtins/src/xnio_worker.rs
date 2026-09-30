// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

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
//! See `roadmap-100.md` T19.7.b.

#![allow(clippy::needless_pass_by_value)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use cratonvm_native_api::{NativeContext, NativeHandleScope, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError, VmError};
use cratonvm_types::{ObjectRef, Value};

use crate::xnio_conduits::{
    alloc_sink_channel_obj, alloc_source_channel_obj, drop_sink_channel, drop_source_channel,
    notify_registered_sources_readable, register_sink_channel, register_source_channel,
    remember_sink_io_thread, remember_sink_paired_source, remember_source_io_thread,
    ConduitTransport, SETTER_FIELD_CHANNEL_HANDLE, SETTER_FIELD_LISTENER_SLOT_INDEX,
    SETTER_NUM_SLOTS,
};
use crate::xnio_io_thread::{
    iot_worker_mirror, IOT_FIELD_ID, IOT_FIELD_STATE, IOT_NUM_SLOTS,
    STATE_RUNNING as IOT_STATE_RUNNING,
};
use crate::{obj_arg, spawn_runnable_on_real_thread, try_alloc_concurrent_synthetic};

// ---------------------------------------------------------------------------
// Class names & field offsets (mirrored in class_manager.rs)
// ---------------------------------------------------------------------------

pub(crate) const CLS_XNIO: &str = "org/xnio/Xnio";
pub(crate) const CLS_XNIO_WORKER: &str = "org/xnio/XnioWorker";
pub(crate) const CLS_NIO_XNIO: &str = "org/xnio/nio/NioXnio";
pub(crate) const CLS_NIO_XNIO_WORKER: &str = "org/xnio/nio/NioXnioWorker";
pub(crate) const CLS_OPTION_MAP: &str = "org/xnio/OptionMap";
const CLS_ACCEPTING_CHANNEL: &str = "org/xnio/channels/AcceptingChannel";
const CLS_CONNECTED_CHANNEL: &str = "org/xnio/channels/ConnectedChannel";
const CLS_SIMPLE_ACCEPTING_CHANNEL: &str = "org/xnio/channels/SimpleAcceptingChannel";
const CLS_ACCEPT_PUMP: &str = "cratonvm/xnio/AcceptPump";
const CLS_SOURCE_POLLER: &str = "cratonvm/xnio/SourcePoller";
const CLS_SUSPENDABLE_ACCEPT_CHANNEL: &str = "org/xnio/channels/SuspendableAcceptChannel";
const CLS_BOUND_CHANNEL: &str = "org/xnio/channels/BoundChannel";
const CLS_CLOSEABLE_CHANNEL: &str = "org/xnio/channels/CloseableChannel";
const CLS_CONFIGURABLE_CHANNEL: &str = "org/xnio/channels/Configurable";
const CLS_QUEUED_NIO_TCP_SERVER2: &str = "org/xnio/nio/QueuedNioTcpServer2";

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

// AcceptingChannel mirror (management HTTP listener):
//   0 = localAddress (InetSocketAddress)
//   1 = acceptListener (ChannelListener)
//   2 = closeListener (ChannelListener)
//   3 = open flag (boolean as int)
//   4 = accepts-resumed flag (boolean as int)
//   5 = worker mirror (XnioWorker)
//   6 = listener id (Rust TcpListener registry key; 0 when unbound)
const ACCEPT_FIELD_LOCAL_ADDRESS: usize = 0;
const ACCEPT_FIELD_ACCEPT_LISTENER: usize = 1;
const ACCEPT_FIELD_CLOSE_LISTENER: usize = 2;
const ACCEPT_FIELD_OPEN: usize = 3;
const ACCEPT_FIELD_RESUMED: usize = 4;
const ACCEPT_FIELD_WORKER: usize = 5;
const ACCEPT_FIELD_LISTENER_ID: usize = 6;
const ACCEPT_NUM_SLOTS: usize = 7;

const ACCEPT_PUMP_FIELD_CHANNEL: usize = 0;
const ACCEPT_PUMP_NUM_SLOTS: usize = 1;
const SOURCE_POLLER_NUM_SLOTS: usize = 0;
const SOURCE_POLLER_INTERVAL_MS: u64 = 10;

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
        let raw = self
            .worker_task_core_threads
            .unwrap_or(DEFAULT_TASK_THREADS);
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
        let idx = self.io_dispatch_counter.fetch_add(1, Ordering::Relaxed) % self.io_threads.len();
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
    // clones). gc-common w23-b: a VM's workers go at its teardown
    // (`forget_vm_xnio_worker_rows`), and a worker whose mirror could not be
    // allocated goes at once (`publish_new_worker`).
}

fn remove_worker(id: u64) {
    worker_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&id);
}

// gc-common w24-a (`common-w23b-xnio-identity-hash-keyed-rows-cross-and-outlive-connections`):
// the worker-mirror and stream-connection-address rows are keyed by the
// object's WEAK LOCK KEY (`crate::gc_stable_weak_lock_key`, as
// `xnio_async::xnio_obj_key_for` since w17-b), not by `(vm, identity hash)`.
// An identity hash is not unique (the heap's counter wraps), so a later object
// with a dead one's hash found the dead one's rows -- another connection's
// peer / local address, another worker. A weak key is VM-scoped, re-addressed
// by the collector, never minted again once freed, and freed at the first
// sweep that finds its object dead; the sweep then calls
// [`forget_xnio_worker_obj_keys`], so a connection's address row goes with
// its `StreamConnection` object (and not at its close: `getPeerAddress` must
// keep answering after close -- Undertow's access log reads it at exchange
// completion). Lookups use `crate::existing_weak_lock_key`, which never
// mints: an object that was never filed has no row, and no slot is left
// behind for it. The key is always computed BEFORE a table guard is taken
// (the key registry is a leaf the sweeps take on their own).

/// Mint (or find) the weak lock key filing `obj`'s rows. `obj` must be current.
fn worker_obj_key(ctx: &dyn NativeContext, obj: ObjectRef) -> usize {
    crate::gc_stable_weak_lock_key(ctx, obj)
        .unwrap_or_else(|_| unreachable!("gc_stable_weak_lock_key never fails"))
}

/// The worker-mirror rows, under one lock.
#[derive(Default)]
struct WorkerObjects {
    /// Weak lock key of an `XnioWorker` mirror -> `(owning VM, worker id)`.
    by_key: HashMap<usize, (usize, u64)>,
    /// Owning VM -> the ids of the workers its mirrors were bound to
    /// ([`remember_worker_object`]).
    ///
    /// gc-common w24-a: VM teardown ([`forget_vm_xnio_worker_rows`]) found a
    /// VM's workers through its `by_key` rows. Those rows now go when the
    /// mirror dies, so the ownership is recorded here, where nothing but
    /// teardown removes it: a worker whose mirror died is still shut down
    /// with its VM, as before. (Kept under the same mutex as `by_key` so the
    /// w24-a change adds no raw lock: `lock_discipline_ratchet`.)
    by_vm: HashMap<usize, Vec<u64>>,
}

fn worker_object_registry() -> &'static Mutex<WorkerObjects> {
    static REG: OnceLock<Mutex<WorkerObjects>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(WorkerObjects::default()))
}

fn remember_worker_object(ctx: &dyn NativeContext, obj: ObjectRef, worker: &Arc<XnioWorker>) {
    // gc-common w23-b: key computed before the guard (no `ctx` call under it).
    let vm = ctx.vm_identity();
    let key = worker_obj_key(ctx, obj);
    let mut rows = worker_object_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    rows.by_key.insert(key, (vm, worker.id));
    let ids = rows.by_vm.entry(vm).or_default();
    if !ids.contains(&worker.id) {
        ids.push(worker.id);
    }
}

fn lookup_worker_object(ctx: &dyn NativeContext, obj: ObjectRef) -> Option<Arc<XnioWorker>> {
    let key = crate::existing_weak_lock_key(ctx, obj)?;
    let (_, id) = worker_object_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .by_key
        .get(&key)
        .copied()?;
    lookup_worker(id)
}

/// Drop the worker-mirror and stream-connection-address rows filed under
/// `keys`, which the lock-key registry has just freed (the mirror or the
/// `StreamConnection` died, or its VM is being torn down) and will never mint
/// again. The worker itself stays registered until its VM's teardown
/// (`WorkerObjects::by_vm`).
///
/// Called by `lib.rs::sweep_lock_keys` and `lib.rs::forget_vm_lock_keys` with
/// the key registry's lock released; each table lock is a leaf, taken on its
/// own. gc-common w24-a.
pub(crate) fn forget_xnio_worker_obj_keys(keys: &[usize]) {
    fn drop_rows<V>(table: &Mutex<HashMap<usize, V>>, keys: &[usize]) {
        let mut table = table.lock().unwrap_or_else(|e| e.into_inner());
        if table.is_empty() {
            return;
        }
        for key in keys {
            table.remove(key);
        }
    }
    if keys.is_empty() {
        return;
    }
    {
        let mut rows = worker_object_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for key in keys {
            rows.by_key.remove(key);
        }
    }
    drop_rows(stream_connection_address_registry(), keys);
}

fn adopt_worker_object(ctx: &dyn NativeContext, obj: ObjectRef) -> Arc<XnioWorker> {
    if let Some(worker) = lookup_worker_object(ctx, obj) {
        return worker;
    }
    let xnio = Xnio::get_instance();
    let worker = xnio.create_worker(OptionMap::default());
    register_worker(worker.clone());
    remember_worker_object(ctx, obj, &worker);
    worker
}

// ---------------------------------------------------------------------------
// Java ↔ Rust glue — natives registered with the method registry.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct StreamConnectionAddresses {
    local: Option<SocketAddr>,
    peer: Option<SocketAddr>,
}

/// Weak lock key of a `StreamConnection` -> `(owning VM, its addresses)`. A
/// row goes when the connection object dies ([`forget_xnio_worker_obj_keys`]),
/// not when it closes. gc-common w24-a.
fn stream_connection_address_registry(
) -> &'static Mutex<HashMap<usize, (usize, StreamConnectionAddresses)>> {
    static REG: OnceLock<Mutex<HashMap<usize, (usize, StreamConnectionAddresses)>>> =
        OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

fn remember_stream_connection_addresses(
    ctx: &dyn NativeContext,
    conn: ObjectRef,
    local: Option<SocketAddr>,
    peer: Option<SocketAddr>,
) {
    let vm = ctx.vm_identity();
    let key = worker_obj_key(ctx, conn);
    stream_connection_address_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, (vm, StreamConnectionAddresses { local, peer }));
}

fn stream_connection_addresses(
    ctx: &dyn NativeContext,
    conn: ObjectRef,
) -> Option<StreamConnectionAddresses> {
    let key = crate::existing_weak_lock_key(ctx, conn)?;
    stream_connection_address_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .map(|(_, addresses)| *addresses)
}

static NEXT_ACCEPTING_CHANNEL_ID: AtomicU64 = AtomicU64::new(1);

/// `listener id -> (owning VM, bound listener)`.
///
/// gc-common w23-b: the owner is what lets VM teardown close a listener whose
/// channel was never closed (the port stayed bound for the life of the
/// process).
fn accepting_listener_registry() -> &'static Mutex<HashMap<u64, (usize, TcpListener)>> {
    static REG: OnceLock<Mutex<HashMap<u64, (usize, TcpListener)>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

fn register_accepting_listener(vm: usize, listener: TcpListener) -> u64 {
    let id = NEXT_ACCEPTING_CHANNEL_ID.fetch_add(1, Ordering::SeqCst);
    accepting_listener_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id, (vm, listener));
    id
}

/// Close listener `id` and every accepted-but-not-yet-`accept()`ed socket
/// queued for it. gc-common w23-b: the queue used to outlive the listener, so
/// each connection accepted just before a close kept an open socket for the
/// life of the process.
fn remove_accepting_listener(id: u64) {
    if id == 0 {
        return;
    }
    let listener = accepting_listener_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&id);
    let pending = accepting_pending_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&id);
    drop(listener);
    drop(pending);
}

fn accepting_pending_registry() -> &'static Mutex<HashMap<u64, VecDeque<TcpStream>>> {
    static REG: OnceLock<Mutex<HashMap<u64, VecDeque<TcpStream>>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

fn accepting_listener_registered(id: u64) -> bool {
    accepting_listener_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&id)
}

fn clone_accepting_listener(id: u64) -> Option<TcpListener> {
    accepting_listener_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id)
        .and_then(|(_, listener)| listener.try_clone().ok())
}

/// VM teardown (`lib.rs::forget_vm_native_root_stores`): drop every row VM
/// `vm` filed in this module's process-wide tables -- its worker-mirror and
/// stream-connection-address rows, its source-poller mark, its accepting
/// listeners (and their queued sockets) -- and shut down the workers its
/// mirrors named. Sockets close and worker threads are joined as the dropped
/// values go, after every guard here is released. Idempotent.
///
/// gc-common w23-b: none of these were ever dropped. A later VM that reused
/// the identity found the poller mark already set and never started its
/// poller (every conduit it opened then relied on inline notifies alone); a
/// torn-down VM's workers kept their I/O and task threads, and its listeners
/// kept their ports bound.
pub fn forget_vm_xnio_worker_rows(vm: usize) {
    let mut worker_ids: Vec<u64> = Vec::new();
    {
        let mut rows = worker_object_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        rows.by_key.retain(|_, (owner, id)| {
            if *owner == vm {
                worker_ids.push(*id);
                false
            } else {
                true
            }
        });
        // gc-common w24-a: and the workers whose mirror already died (its
        // row went with the mirror's key).
        if let Some(owned) = rows.by_vm.remove(&vm) {
            worker_ids.extend(owned);
        }
    }
    stream_connection_address_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|_, (owner, _)| *owner != vm);
    source_poller_started_vms()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&vm);
    let mut listener_ids: Vec<u64> = Vec::new();
    let dropped_listeners: Vec<TcpListener> = {
        let mut listeners = accepting_listener_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        listener_ids.extend(
            listeners
                .iter()
                .filter(|(_, (owner, _))| *owner == vm)
                .map(|(id, _)| *id),
        );
        listener_ids
            .iter()
            .filter_map(|id| listeners.remove(id))
            .map(|(_, listener)| listener)
            .collect()
    };
    let dropped_pending: Vec<VecDeque<TcpStream>> = {
        let mut pending = accepting_pending_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        listener_ids
            .iter()
            .filter_map(|id| pending.remove(id))
            .collect()
    };
    worker_ids.sort_unstable();
    worker_ids.dedup();
    let dropped_workers: Vec<Arc<XnioWorker>> = {
        let mut workers = worker_registry().lock().unwrap_or_else(|e| e.into_inner());
        worker_ids
            .iter()
            .filter_map(|id| workers.remove(id))
            .collect()
    };
    for worker in &dropped_workers {
        worker.shutdown_now();
    }
    drop(dropped_listeners);
    drop(dropped_pending);
    drop(dropped_workers);
}

fn push_pending_accept(id: u64, stream: TcpStream) {
    accepting_pending_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(id)
        .or_default()
        .push_back(stream);
}

fn pop_pending_accept(id: u64) -> Option<TcpStream> {
    accepting_pending_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_mut(&id)
        .and_then(VecDeque::pop_front)
}

/// Allocate the Java `Xnio` mirror (singleton) — lazily created.
fn alloc_xnio_mirror(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, CLS_XNIO, XNIO_NUM_SLOTS)?;
    // Family-1 fix (cce0079): `create_string` allocates and can move the
    // still-unrooted `obj` — refresh before the stores/return.
    let obj_pin = ctx.pin_native_root(obj);
    let name = ctx.create_string("nio");
    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.unpin_native_roots(obj_pin);
    ctx.set_field(obj, XNIO_FIELD_NAME, Value::Object(Some(name)));
    ctx.set_field(obj, XNIO_FIELD_PROVIDER_HANDLE, Value::Long(1));
    Ok(obj)
}

/// Allocate the Java `XnioWorker` mirror and wire its options_handle
/// back to the Rust registry.
fn alloc_worker_mirror(
    ctx: &mut dyn NativeContext,
    class: &str,
    worker: &Arc<XnioWorker>,
) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, class, WORKER_NUM_SLOTS)?;
    // Family-1 fix (cce0079): `create_string` allocates and can move the
    // still-unrooted `obj` — refresh before the stores and the
    // identity-hash registry insert (`remember_worker_object` on a stale
    // ref registers the WRONG key, so every later `read_worker` misses).
    let obj_pin = ctx.pin_native_root(obj);
    let name = ctx.create_string(&worker.name);
    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.unpin_native_roots(obj_pin);
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
    remember_worker_object(ctx, obj, worker);
    Ok(obj)
}

/// Read the registered Rust worker out of a Java `XnioWorker` mirror.
fn read_worker(ctx: &dyn NativeContext, this: ObjectRef) -> Option<Arc<XnioWorker>> {
    let id = match ctx.get_field(this, WORKER_FIELD_OPTIONS_HANDLE) {
        Value::Long(v) => Some(v as u64),
        _ => None,
    };
    id.and_then(lookup_worker)
        .or_else(|| lookup_worker_object(ctx, this))
        .or_else(|| Some(adopt_worker_object(ctx, this)))
}

/// Update the Java mirror's state ordinal after a transition.
fn reflect_worker_state(ctx: &dyn NativeContext, this: ObjectRef, worker: &Arc<XnioWorker>) {
    let has_synthetic_handle = matches!(
        ctx.get_field(this, WORKER_FIELD_OPTIONS_HANDLE),
        Value::Long(id) if lookup_worker(id as u64).is_some()
    );
    if !has_synthetic_handle {
        return;
    }
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

fn native_xnio_get_instance(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let _ = Xnio::get_instance(); // force singleton init.
    Ok(Some(Value::Object(Some(alloc_xnio_mirror(ctx)?))))
}

fn native_xnio_get_instance_named(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let _name = match args.first().copied() {
        Some(Value::Object(Some(s))) => ctx.read_string(s).unwrap_or_else(|| "nio".to_string()),
        _ => "nio".to_string(),
    };
    let _ = Xnio::get_instance();
    Ok(Some(Value::Object(Some(alloc_xnio_mirror(ctx)?))))
}

// --- Xnio.createWorker(OptionMap) ---

fn native_xnio_create_worker(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // The Java-facing `OptionMap` is the T19.7.e territory; we accept
    // any opaque object and read defaults for now.  When T19.7.e lands
    // it will populate `worker_io_threads` / `worker_task_core_threads`
    // from the OptionMap's internal map.
    publish_new_worker(ctx)
}

/// Create a default worker, register it, and hand back its Java mirror.
///
/// gc-common w23-b: when the mirror allocation failed, the registered worker
/// -- and its I/O and task threads -- stayed in the process-wide registry with
/// no mirror, and so no VM row, that could ever name it again. It is now
/// unregistered and shut down on that path.
fn publish_new_worker(ctx: &mut dyn NativeContext) -> MethodCallResult {
    let xnio = Xnio::get_instance();
    let worker = xnio.create_worker(OptionMap::default());
    register_worker(worker.clone());
    match alloc_worker_mirror(ctx, CLS_XNIO_WORKER, &worker) {
        Ok(obj) => Ok(Some(Value::Object(Some(obj)))),
        Err(e) => {
            remove_worker(worker.id);
            worker.shutdown_now();
            Err(e)
        }
    }
}

// --- Xnio.build(XnioWorker$Builder) ---
//
// The modern (XNIO 3.8.x) builder-style worker factory:
// `xnio.createWorkerBuilder().setWorkerName(...)...build()`, where
// `XnioWorker.Builder.build()` (real bytecode) calls back into
// `xnio.build(this)`. `Xnio.build` is declared `protected abstract` on
// the real `org.xnio.Xnio` class; the singleton this VM hands back from
// `getInstance()` (`alloc_xnio_mirror`, above) is stamped with the
// abstract `Xnio` class itself (not a concrete subclass), so any real
// dispatch of `build` legitimately has no Code attribute to find —
// same shape as `createWorker` needing its own native rather than real
// bytecode ever running. Same simplification as
// `native_xnio_create_worker`: build a default worker and ignore the
// `Builder`'s configured pool sizes/name for now.
fn native_xnio_build_worker(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    publish_new_worker(ctx)
}

fn decode_xnio_bind_address(
    ctx: &mut dyn NativeContext,
    addr: ObjectRef,
) -> Result<(String, u16), MethodCallFailed> {
    // Family-1 fix (cce0079): both `invoke_virtual`s below are GC-capable —
    // refresh `addr` after each, or the follow-up dispatch/field reads
    // operate on a stale receiver.
    let addr_pin = ctx.pin_native_root(addr);
    let port_via_method = match ctx.invoke_virtual(addr, "getPort", "()I", &[]) {
        Ok(Some(Value::Int(v))) if (0..=u16::MAX as i32).contains(&v) => Some(v as u16),
        _ => None,
    };
    let addr = ctx.read_native_pin(addr_pin, addr);
    let host_via_method =
        match ctx.invoke_virtual(addr, "getHostString", "()Ljava/lang/String;", &[]) {
            Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
            _ => None,
        };
    let addr = ctx.read_native_pin(addr_pin, addr);
    ctx.unpin_native_roots(addr_pin);
    if let Some(port) = port_via_method {
        let host = host_via_method.unwrap_or_else(|| "0.0.0.0".to_string());
        return Ok((
            if host.is_empty() {
                "0.0.0.0".to_string()
            } else {
                host
            },
            port,
        ));
    }

    let holder = match ctx.get_field_by_name(addr, "holder") {
        Value::Object(Some(h)) => h,
        _ => addr,
    };
    let port = match ctx.get_field_by_name(holder, "port") {
        Value::Int(v) if (0..=u16::MAX as i32).contains(&v) => Some(v as u16),
        _ => match ctx.get_field(addr, 1) {
            Value::Int(v) if (0..=u16::MAX as i32).contains(&v) => Some(v as u16),
            _ => None,
        },
    };
    let host = match ctx.get_field_by_name(holder, "hostname") {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => match ctx.get_field(addr, 0) {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        },
    }
    .unwrap_or_else(|| "0.0.0.0".to_string());
    let Some(port) = port else {
        return Err(mcf_io(
            "XnioWorker.createTcpConnectionServer: invalid bind address",
        ));
    };
    Ok((
        if host.is_empty() {
            "0.0.0.0".to_string()
        } else {
            host
        },
        port,
    ))
}

fn alloc_accepting_channel_mirror(
    ctx: &mut dyn NativeContext,
    worker: ObjectRef,
    local_address: ObjectRef,
    accept_listener: Value,
    listener_id: u64,
) -> Result<ObjectRef, MethodCallFailed> {
    // Family-1 fix (cce0079): the mirror alloc can move all three inputs —
    // pin and refresh them, or the field stores below write pre-GC
    // addresses (a stale acceptListener slot dispatches the accept on the
    // wrong object).
    let worker_pin = ctx.pin_native_root(worker);
    let la_pin = ctx.pin_native_root(local_address);
    let al_pin = match accept_listener {
        Value::Object(Some(o)) => Some(ctx.pin_native_root(o)),
        _ => None,
    };
    let obj = try_alloc_concurrent_synthetic(ctx, CLS_ACCEPTING_CHANNEL, ACCEPT_NUM_SLOTS)?;
    let worker = ctx.read_native_pin(worker_pin, worker);
    let local_address = ctx.read_native_pin(la_pin, local_address);
    let accept_listener = match (accept_listener, al_pin) {
        (Value::Object(Some(o)), Some(h)) => Value::Object(Some(ctx.read_native_pin(h, o))),
        (v, _) => v,
    };
    ctx.unpin_native_roots(worker_pin);
    ctx.set_field(
        obj,
        ACCEPT_FIELD_LOCAL_ADDRESS,
        Value::Object(Some(local_address)),
    );
    ctx.set_field(obj, ACCEPT_FIELD_ACCEPT_LISTENER, accept_listener);
    ctx.set_field(obj, ACCEPT_FIELD_CLOSE_LISTENER, Value::Object(None));
    ctx.set_field(obj, ACCEPT_FIELD_OPEN, Value::Int(1));
    ctx.set_field(obj, ACCEPT_FIELD_RESUMED, Value::Int(0));
    ctx.set_field(obj, ACCEPT_FIELD_WORKER, Value::Object(Some(worker)));
    ctx.set_field(
        obj,
        ACCEPT_FIELD_LISTENER_ID,
        Value::Long(listener_id as i64),
    );
    Ok(obj)
}

// --- XnioWorker.createTcpConnectionServer(InetSocketAddress, ChannelListener, OptionMap) ---
//
// The abstract XnioWorker base implementation throws XNIO000900. CratonVM's
// synthetic worker mirrors are stamped as that base class, so WildFly domain
// management hit the unsupported method while starting the HTTP interface.
// Bind a real TcpListener to keep the management port occupied and return a
// lightweight AcceptingChannel mirror. Full accept/StreamConnection plumbing is
// intentionally left to the existing XNIO event-loop work; this bridge supplies
// the startup contract Undertow needs to install and resume the listener.
fn native_xnio_create_tcp_connection_server(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let worker = obj_arg(args, 0)?;
    let bind_addr = obj_arg(args, 1)?;
    let accept_listener = args.get(2).copied().unwrap_or(Value::Object(None));
    // Family-1 fix (cce0079): `decode_xnio_bind_address` dispatches
    // getPort/getHostString (GC-capable) — refresh all three inputs before
    // handing them to the mirror alloc.
    let worker_pin = ctx.pin_native_root(worker);
    let ba_pin = ctx.pin_native_root(bind_addr);
    let al_pin = match accept_listener {
        Value::Object(Some(o)) => Some(ctx.pin_native_root(o)),
        _ => None,
    };
    let decoded = decode_xnio_bind_address(ctx, bind_addr);
    let worker = ctx.read_native_pin(worker_pin, worker);
    let bind_addr = ctx.read_native_pin(ba_pin, bind_addr);
    let accept_listener = match (accept_listener, al_pin) {
        (Value::Object(Some(o)), Some(h)) => Value::Object(Some(ctx.read_native_pin(h, o))),
        (v, _) => v,
    };
    ctx.unpin_native_roots(worker_pin);
    let (host, port) = decoded?;
    let listener = TcpListener::bind((host.as_str(), port)).map_err(|e| {
        mcf_io(format!(
            "XnioWorker.createTcpConnectionServer bind {host}:{port}: {e}"
        ))
    })?;
    let _ = listener.set_nonblocking(true);
    let listener_id = register_accepting_listener(ctx.vm_identity(), listener);
    // gc-common w23-b: a failed mirror allocation used to leave the listener
    // registered -- its port bound, with no channel that could close it.
    let channel = match alloc_accepting_channel_mirror(
        ctx,
        worker,
        bind_addr,
        accept_listener,
        listener_id,
    ) {
        Ok(channel) => channel,
        Err(e) => {
            remove_accepting_listener(listener_id);
            return Err(e);
        }
    };
    let channel_pin = ctx.pin_native_root(channel);
    let _ = start_accept_pump(ctx, channel, listener_id);
    let channel = ctx.read_native_pin(channel_pin, channel);
    ctx.unpin_native_roots(channel_pin);
    Ok(Some(Value::Object(Some(channel))))
}

fn start_accept_pump(
    ctx: &mut dyn NativeContext,
    channel: ObjectRef,
    listener_id: u64,
) -> MethodCallResult {
    // Family-1 fix (cce0079): the pump alloc can move `channel` (whose
    // pre-GC address would then be stored into the pump's channel field —
    // the accept loop later reads that field and operates on the wrong
    // object), and `create_string` can move the still-unrooted `pump`
    // BEFORE the old code ever pinned it. Pin `channel` first, pin `pump`
    // immediately after its alloc, refresh at each step.
    let channel_pin = ctx.pin_native_root(channel);
    let pump = try_alloc_concurrent_synthetic(ctx, CLS_ACCEPT_PUMP, ACCEPT_PUMP_NUM_SLOTS)?;
    let channel = ctx.read_native_pin(channel_pin, channel);
    let pump_pin = ctx.pin_native_root(pump);
    ctx.set_field(
        pump,
        ACCEPT_PUMP_FIELD_CHANNEL,
        Value::Object(Some(channel)),
    );
    let name = ctx.create_string(&format!("cratonvm-xnio-accept-{listener_id}"));
    let name_pin = ctx.pin_native_root(name);
    let pump = ctx.read_native_pin(pump_pin, pump);
    let name = ctx.read_native_pin(name_pin, name);
    let thread = ctx.new_object_initialized(
        "java/lang/Thread",
        "(Ljava/lang/Runnable;Ljava/lang/String;)V",
        &[Value::Object(Some(pump)), Value::Object(Some(name))],
    );
    ctx.unpin_native_roots(channel_pin);
    let thread = match thread? {
        Some(Value::Object(Some(thread))) => thread,
        _ => return Ok(None),
    };
    // `setDaemon` is a Java dispatch (GC-capable) — refresh `thread` before
    // `start`.
    let thread_pin = ctx.pin_native_root(thread);
    let _ = ctx.invoke_virtual(thread, "setDaemon", "(Z)V", &[Value::Int(1)]);
    let thread = ctx.read_native_pin(thread_pin, thread);
    ctx.unpin_native_roots(thread_pin);
    let _ = ctx.invoke_virtual(thread, "start", "()V", &[]);
    Ok(None)
}

fn make_accepting_listener_setter(
    ctx: &mut dyn NativeContext,
    channel: ObjectRef,
    listener_slot: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    let setter =
        try_alloc_concurrent_synthetic(ctx, "org/xnio/ChannelListener$Setter", SETTER_NUM_SLOTS)?;
    ctx.set_field(
        setter,
        SETTER_FIELD_CHANNEL_HANDLE,
        Value::Object(Some(channel)),
    );
    ctx.set_field(
        setter,
        SETTER_FIELD_LISTENER_SLOT_INDEX,
        Value::Int(listener_slot as i32),
    );
    Ok(setter)
}

fn native_accepting_get_accept_setter(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let setter = make_accepting_listener_setter(ctx, this, ACCEPT_FIELD_ACCEPT_LISTENER);
    Ok(Some(Value::Object(Some(setter?))))
}

fn native_accepting_get_close_setter(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let setter = make_accepting_listener_setter(ctx, this, ACCEPT_FIELD_CLOSE_LISTENER);
    Ok(Some(Value::Object(Some(setter?))))
}

fn source_poller_started_vms() -> &'static Mutex<HashSet<usize>> {
    static REG: OnceLock<Mutex<HashSet<usize>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashSet::new()))
}

fn ensure_source_poller_started(ctx: &mut dyn NativeContext) -> Result<(), MethodCallFailed> {
    let vm = ctx.vm_identity();
    {
        let mut started = source_poller_started_vms()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if !started.insert(vm) {
            return Ok(());
        }
    }

    let poller = try_alloc_concurrent_synthetic(ctx, CLS_SOURCE_POLLER, SOURCE_POLLER_NUM_SLOTS)?;
    // Family-1 fix (cce0079): pin `poller` BEFORE `create_string` — the
    // string alloc can move the still-unrooted poller, and the old
    // pin-after-alloc ordering then pinned an already-stale address.
    let poller_pin = ctx.pin_native_root(poller);
    let name = ctx.create_string(&format!("cratonvm-xnio-source-poll-{vm}"));
    let name_pin = ctx.pin_native_root(name);
    let poller = ctx.read_native_pin(poller_pin, poller);
    let name = ctx.read_native_pin(name_pin, name);
    let thread = ctx.new_object_initialized(
        "java/lang/Thread",
        "(Ljava/lang/Runnable;Ljava/lang/String;)V",
        &[Value::Object(Some(poller)), Value::Object(Some(name))],
    );
    ctx.unpin_native_roots(poller_pin);
    ctx.unpin_native_roots(name_pin);

    let thread = match thread {
        Ok(Some(Value::Object(Some(thread)))) => thread,
        _ => {
            source_poller_started_vms()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&vm);
            return Ok(());
        }
    };
    // `setDaemon` is a Java dispatch (GC-capable) — refresh `thread` before
    // `start` (Family-1 fix, cce0079).
    let thread_pin = ctx.pin_native_root(thread);
    let _ = ctx.invoke_virtual(thread, "setDaemon", "(Z)V", &[Value::Int(1)]);
    let thread = ctx.read_native_pin(thread_pin, thread);
    ctx.unpin_native_roots(thread_pin);
    let _ = ctx.invoke_virtual(thread, "start", "()V", &[]);
    Ok(())
}

fn alloc_stream_connection_for_tcp(
    ctx: &mut dyn NativeContext,
    io_thread: ObjectRef,
    stream: TcpStream,
) -> Result<ObjectRef, MethodCallFailed> {
    let local_addr = stream.local_addr().ok();
    let peer_addr = stream.peer_addr().ok();
    let _ = stream.set_nonblocking(true);
    let source_stream = stream
        .try_clone()
        .map_err(|e| mcf_io(format!("AcceptingChannel.accept: clone stream failed: {e}")))?;
    let source_id = register_source_channel(ConduitTransport::Tcp(source_stream));
    let sink_id = register_sink_channel(ConduitTransport::Tcp(stream));
    if crate::nbflags().dbg_xnio_tcp {
        eprintln!("[cratonvm:xnio-tcp] accepted_stream source_id={source_id} sink_id={sink_id}");
    }
    // Nothing between the entry and here can collect: `io_thread` is current.
    let conn = build_stream_connection(ctx, io_thread, source_id, sink_id, local_addr, peer_addr);
    if conn.is_err() {
        abandon_stream_connection(ctx, source_id, sink_id);
    }
    conn
}

/// Undo the two `register_*_channel` calls of a connection whose
/// `StreamConnection` could not be built: unregister both halves (releasing
/// whatever roots the partial build filed) and close the socket.
///
/// gc-common w23-b: an allocation failure part-way through used to leave both
/// channels -- and the accepted socket -- registered for the life of the VM,
/// with no Java object left that could ever close them.
fn abandon_stream_connection(ctx: &mut dyn NativeContext, source_id: u64, sink_id: u64) {
    drop_source_channel(ctx, source_id);
    drop_sink_channel(ctx, sink_id);
}

/// Build the Java `StreamConnection` over the registered channel pair
/// `source_id` / `sink_id`, bound to `io_thread` (current at the call), and
/// start the VM's source poller. Returns the connection, current. On `Err` the
/// caller owns the channels and unregisters them
/// ([`abandon_stream_connection`]).
///
/// gc-common w23-b: the shared body of `alloc_stream_connection_for_tcp` and
/// `native_iot_open_tcp_stream_connection`, which carried two copies of it.
/// Every pin it takes is released here, on every exit.
fn build_stream_connection(
    ctx: &mut dyn NativeContext,
    io_thread: ObjectRef,
    source_id: u64,
    sink_id: u64,
    local_addr: Option<SocketAddr>,
    peer_addr: Option<SocketAddr>,
) -> Result<ObjectRef, MethodCallFailed> {
    let io_thread_pin = ctx.pin_native_root(io_thread);
    let conn = build_stream_connection_pinned(
        ctx,
        io_thread_pin,
        io_thread,
        source_id,
        sink_id,
        local_addr,
        peer_addr,
    );
    // The oldest pin of the build: releases every newer one too.
    ctx.unpin_native_roots(io_thread_pin);
    conn
}

fn build_stream_connection_pinned(
    ctx: &mut dyn NativeContext,
    io_thread_pin: usize,
    io_thread: ObjectRef,
    source_id: u64,
    sink_id: u64,
    local_addr: Option<SocketAddr>,
    peer_addr: Option<SocketAddr>,
) -> Result<ObjectRef, MethodCallFailed> {
    let close_ref = match ctx.new_object_initialized(
        "java/util/concurrent/atomic/AtomicReference",
        "()V",
        &[],
    )? {
        Some(Value::Object(Some(o))) => o,
        _ => {
            return Err(mcf_runtime(
                "StreamConnection: failed to allocate close listener ref",
            ))
        }
    };
    let close_pin = ctx.pin_native_root(close_ref);

    let source_obj = alloc_source_channel_obj(ctx, source_id)?;
    let source_pin = ctx.pin_native_root(source_obj);
    let sink_obj = alloc_sink_channel_obj(ctx, sink_id)?;
    let sink_pin = ctx.pin_native_root(sink_obj);
    let conn = try_alloc_concurrent_synthetic(ctx, "org/xnio/StreamConnection", 5)?;
    let conn_pin = ctx.pin_native_root(conn);

    let close_ref = ctx.read_native_pin(close_pin, close_ref);
    let source_obj = ctx.read_native_pin(source_pin, source_obj);
    let sink_obj = ctx.read_native_pin(sink_pin, sink_obj);
    let conn = ctx.read_native_pin(conn_pin, conn);
    let io_thread = ctx.read_native_pin(io_thread_pin, io_thread);
    ctx.set_field_by_name(conn, "thread", Value::Object(Some(io_thread)));
    ctx.set_field_by_name(conn, "state", Value::Int(0));
    remember_source_io_thread(ctx, source_obj, io_thread);
    remember_sink_io_thread(ctx, sink_obj, io_thread);
    remember_sink_paired_source(ctx, sink_obj, source_obj);
    ctx.set_field_by_name(conn, "sourceChannel", Value::Object(Some(source_obj)));
    ctx.set_field_by_name(conn, "sinkChannel", Value::Object(Some(sink_obj)));
    ctx.set_field_by_name(conn, "closeListener", Value::Object(Some(close_ref)));
    remember_stream_connection_addresses(ctx, conn, local_addr, peer_addr);
    ensure_source_poller_started(ctx)?;
    Ok(ctx.read_native_pin(conn_pin, conn))
}

fn native_accepting_accept(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let listener_id = match ctx.get_field(this, ACCEPT_FIELD_LISTENER_ID) {
        Value::Long(id) if id > 0 => id as u64,
        _ => return Ok(Some(Value::Object(None))),
    };
    let Some(stream) = pop_pending_accept(listener_id) else {
        return Ok(Some(Value::Object(None)));
    };
    let io_thread = match native_accepting_get_io_thread(ctx, &[Value::Object(Some(this))])? {
        Some(Value::Object(Some(io_thread))) => io_thread,
        _ => return Ok(Some(Value::Object(None))),
    };
    let conn = alloc_stream_connection_for_tcp(ctx, io_thread, stream)?;
    Ok(Some(Value::Object(Some(conn))))
}

/// One idle tick of the accept pump: poll the registered sources, sleep inside
/// a blocking region, poll again.
///
/// gc-common w23-b: this used to take the pump's channel and hand it back
/// "refreshed" through the blocking region's ref list -- but the first
/// `notify_registered_sources_readable` runs listeners (Java, GC-capable)
/// before the channel was put in that list, so what came back could already
/// be stale. The pump holds the channel in a pin and re-reads it at the top of
/// every iteration; this helper no longer touches it.
fn accept_pump_blocked_sleep(ctx: &mut dyn NativeContext, duration: Duration) {
    notify_registered_sources_readable(ctx, false, None);
    let mut refs: [Value; 0] = [];
    ctx.begin_blocking_region();
    thread::sleep(duration);
    ctx.end_blocking_region_refs(&mut refs);
    notify_registered_sources_readable(ctx, false, None);
}

fn native_source_poller_run(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    loop {
        notify_registered_sources_readable(ctx, false, None);

        let mut refs: [Value; 0] = [];
        ctx.begin_blocking_region();
        thread::sleep(Duration::from_millis(SOURCE_POLLER_INTERVAL_MS));
        ctx.end_blocking_region_refs(&mut refs);
    }
}

fn native_accept_pump_run(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let pump = obj_arg(args, 0)?;
    let channel = match ctx.get_field(pump, ACCEPT_PUMP_FIELD_CHANNEL) {
        Value::Object(Some(channel)) => channel,
        _ => return Ok(None),
    };
    let channel_pin = ctx.pin_native_root(channel);
    let mut channel = channel;
    // gc-common w23-b: the pump's own handle on the listener, cloned once per
    // listener id. It used to `try_clone` the registered listener -- a socket
    // duplicate and close -- on every 10 ms tick. The registry still decides:
    // once the channel's `close` removes the listener, the pump stops.
    let mut pump_listener: Option<(u64, TcpListener)> = None;
    loop {
        channel = ctx.read_native_pin(channel_pin, channel);
        let open = matches!(ctx.get_field(channel, ACCEPT_FIELD_OPEN), Value::Int(v) if v != 0);
        if !open {
            break;
        }
        let listener_id = match ctx.get_field(channel, ACCEPT_FIELD_LISTENER_ID) {
            Value::Long(id) if id > 0 => id as u64,
            _ => break,
        };
        let resumed =
            matches!(ctx.get_field(channel, ACCEPT_FIELD_RESUMED), Value::Int(v) if v != 0);
        if !resumed {
            accept_pump_blocked_sleep(ctx, Duration::from_millis(10));
            continue;
        }
        if !accepting_listener_registered(listener_id) {
            break;
        }
        if pump_listener.as_ref().map(|(id, _)| *id) != Some(listener_id) {
            let Some(cloned) = clone_accepting_listener(listener_id) else {
                break;
            };
            pump_listener = Some((listener_id, cloned));
        }
        let Some((_, listener)) = pump_listener.as_ref() else {
            break;
        };
        match listener.accept() {
            Ok((stream, peer)) => {
                let _ = stream.set_nonblocking(true);
                if crate::nbflags().dbg_xnio_tcp {
                    eprintln!(
                        "[cratonvm:xnio-tcp] accept_pump listener_id={listener_id} peer={peer}"
                    );
                }
                push_pending_accept(listener_id, stream);
                channel = ctx.read_native_pin(channel_pin, channel);
                if let Value::Object(Some(accept_listener)) =
                    ctx.get_field(channel, ACCEPT_FIELD_ACCEPT_LISTENER)
                {
                    let _ = ctx.invoke_virtual(
                        accept_listener,
                        "handleEvent",
                        "(Ljava/nio/channels/Channel;)V",
                        &[Value::Object(Some(channel))],
                    );
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                accept_pump_blocked_sleep(ctx, Duration::from_millis(10));
            }
            Err(e) => {
                if crate::nbflags().dbg_xnio_tcp {
                    eprintln!(
                        "[cratonvm:xnio-tcp] accept_pump listener_id={listener_id} error={e}"
                    );
                }
                accept_pump_blocked_sleep(ctx, Duration::from_millis(100));
            }
        }
    }
    ctx.unpin_native_roots(channel_pin);
    Ok(None)
}

fn native_accepting_get_local_address(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(ctx.get_field(this, ACCEPT_FIELD_LOCAL_ADDRESS)))
}

fn new_inet_socket_address_value(
    ctx: &mut dyn NativeContext,
    addr: SocketAddr,
) -> MethodCallResult {
    let host = ctx.create_string(&addr.ip().to_string());
    ctx.new_object_initialized(
        "java/net/InetSocketAddress",
        "(Ljava/lang/String;I)V",
        &[Value::Object(Some(host)), Value::Int(addr.port() as i32)],
    )
}

fn native_connected_get_peer_address(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    match stream_connection_addresses(ctx, this).and_then(|a| a.peer) {
        Some(addr) => new_inet_socket_address_value(ctx, addr),
        None => Ok(Some(Value::Object(None))),
    }
}

fn native_connected_get_local_address(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    match stream_connection_addresses(ctx, this).and_then(|a| a.local) {
        Some(addr) => new_inet_socket_address_value(ctx, addr),
        None => Ok(Some(Value::Object(None))),
    }
}

fn native_connected_get_io_thread(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(ctx.get_field_by_name(this, "thread")))
}

/// The worker an `XnioIoThread` mirror names, from the mirror's own
/// (collector-remapped) fields. gc-common w29-d: the unrooted, unremapped
/// identity-hash registry this consulted first is gone; see
/// `xnio_io_thread::iot_worker_mirror`, which this now is.
fn io_thread_worker_mirror(ctx: &dyn NativeContext, io_thread: ObjectRef) -> Option<ObjectRef> {
    iot_worker_mirror(ctx, io_thread)
}

fn native_connected_get_worker(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let worker = match ctx.get_field_by_name(this, "thread") {
        Value::Object(Some(io_thread)) => io_thread_worker_mirror(ctx, io_thread),
        _ => None,
    };
    Ok(Some(Value::Object(worker)))
}

fn native_accepting_suspend_accepts(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    ctx.set_field(this, ACCEPT_FIELD_RESUMED, Value::Int(0));
    Ok(None)
}

fn native_accepting_resume_accepts(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    ctx.set_field(this, ACCEPT_FIELD_RESUMED, Value::Int(1));
    Ok(None)
}

fn native_accepting_is_accept_resumed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let resumed = matches!(ctx.get_field(this, ACCEPT_FIELD_RESUMED), Value::Int(v) if v != 0);
    Ok(Some(Value::Int(if resumed { 1 } else { 0 })))
}

fn native_accepting_wakeup_accepts(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(None)
}

fn native_accepting_await_acceptable(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(None)
}

fn native_accepting_get_worker(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(ctx.get_field(this, ACCEPT_FIELD_WORKER)))
}

fn native_accepting_get_io_thread(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    match ctx.get_field(this, ACCEPT_FIELD_WORKER) {
        Value::Object(Some(worker)) => {
            native_worker_get_io_thread(ctx, &[Value::Object(Some(worker))])
        }
        _ => Ok(Some(Value::Object(None))),
    }
}

fn native_accepting_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if let Value::Long(id) = ctx.get_field(this, ACCEPT_FIELD_LISTENER_ID) {
        if id > 0 {
            remove_accepting_listener(id as u64);
        }
    }
    ctx.set_field(this, ACCEPT_FIELD_LISTENER_ID, Value::Long(0));
    ctx.set_field(this, ACCEPT_FIELD_OPEN, Value::Int(0));
    ctx.set_field(this, ACCEPT_FIELD_RESUMED, Value::Int(0));
    Ok(None)
}

fn native_accepting_is_open(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let open = matches!(ctx.get_field(this, ACCEPT_FIELD_OPEN), Value::Int(v) if v != 0);
    Ok(Some(Value::Int(if open { 1 } else { 0 })))
}

fn native_accepting_supports_option(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(0)))
}

fn native_accepting_get_option(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

fn native_accepting_set_option(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

// --- XnioWorker.getIoThread / getIoThreads ---

fn native_worker_get_io_thread(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let worker = read_worker(ctx, this)
        .ok_or_else(|| mcf_runtime("XnioWorker.getIoThread: not registered"))?;
    let handle = worker.get_io_thread();
    let this_pin = ctx.pin_native_root(this);
    let obj = try_alloc_concurrent_synthetic(ctx, "org/xnio/XnioIoThread", IOT_NUM_SLOTS)?;
    let this = ctx.read_native_pin(this_pin, this);
    // Pinned across `set_io_thread_mirror_fields` and re-read, so a store that
    // ever becomes a GC point cannot hand back a stale mirror.
    let obj_pin = ctx.pin_native_root(obj);
    set_io_thread_mirror_fields(ctx, obj, this, handle.id);
    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Object(Some(obj))))
}

fn set_io_thread_mirror_fields(
    ctx: &mut dyn NativeContext,
    io_thread: ObjectRef,
    worker: ObjectRef,
    id: usize,
) {
    // gc-common w29-d: the fields below are the only record of the worker
    // (the per-call identity-hash registry row that used to be filed here was
    // never rooted, remapped or removed).
    ctx.set_field_by_name(io_thread, "id", Value::Long(id as i64));
    ctx.set_field_by_name(io_thread, "number", Value::Int(id as i32));
    ctx.set_field_by_name(io_thread, "workerHandle", Value::Object(Some(worker)));
    ctx.set_field_by_name(io_thread, "worker", Value::Object(Some(worker)));
    ctx.set_field_by_name(io_thread, "state", Value::Int(IOT_STATE_RUNNING));
}

fn native_worker_get_io_threads(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let worker = read_worker(ctx, this)
        .ok_or_else(|| mcf_runtime("XnioWorker.getIoThreads: not registered"))?;
    let count = worker.io_threads.len();
    // The array and the receiver both outlive one mirror allocation per thread
    // (and the strings `set_io_thread_mirror_fields` makes), so both are held
    // in the scope rather than as addresses.
    let mut scope = NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    // `XnioIoThread[]`, the declared type (i7-L2), not `Object[]`. The lookup
    // may load, which is why the receiver is rooted first.
    let component = crate::lang_class::loading_component_id(&mut *scope, "org/xnio/XnioIoThread");
    let arr_obj = scope.new_ref_array(component, count);
    let arr_h = scope.root(arr_obj);
    for (i, h) in worker.io_threads.iter().enumerate() {
        let obj =
            try_alloc_concurrent_synthetic(&mut *scope, "org/xnio/XnioIoThread", IOT_NUM_SLOTS)?;
        let obj_h = scope.root(obj);
        let this_now = scope.get(&this_h);
        set_io_thread_mirror_fields(&mut *scope, obj, this_now, h.id);
        let arr = scope.get(&arr_h);
        let obj = scope.get(&obj_h);
        scope.set_array_element(arr, i, Value::Object(Some(obj)));
    }
    Ok(Some(Value::Object(Some(scope.get(&arr_h)))))
}

// --- XnioWorker.execute(Runnable) ---

fn native_worker_execute(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let runnable = match args.get(1) {
        Some(Value::Object(Some(runnable))) => *runnable,
        _ => {
            return Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NullPointerException {
                    message: Some("XnioWorker.execute: null Runnable".to_string()),
                },
            )));
        }
    };
    let worker =
        read_worker(ctx, this).ok_or_else(|| mcf_runtime("XnioWorker.execute: not registered"))?;
    if worker.is_shutdown() {
        return Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::IllegalStateException {
                message: "XnioWorker.execute: worker is shutdown".to_string(),
            },
        )));
    }
    spawn_runnable_on_real_thread(ctx, runnable)
}

// --- XnioWorker.shutdown / shutdownNow ---

fn native_worker_shutdown(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if let Some(worker) = read_worker(ctx, this) {
        worker.shutdown();
        reflect_worker_state(ctx, this, &worker);
    }
    Ok(None)
}

fn native_worker_shutdown_now(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

fn native_worker_is_shutdown(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let v = read_worker(ctx, this).map_or(false, |w| w.is_shutdown());
    Ok(Some(Value::Int(if v { 1 } else { 0 })))
}

fn native_worker_is_terminated(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let v = read_worker(ctx, this).map_or(false, |w| w.is_terminated());
    Ok(Some(Value::Int(if v { 1 } else { 0 })))
}

// --- XnioWorker.getMXBean() — JMX not exposed here, return null. ---

fn native_worker_get_mxbean(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

fn native_worker_get_bind_address_table(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if let Value::Object(Some(table)) = ctx.get_field_by_name(this, "bindAddressTable") {
        return Ok(Some(Value::Object(Some(table))));
    }

    let this_pin = ctx.pin_native_root(this);
    let table_result =
        ctx.new_object_initialized("org/wildfly/common/net/CidrAddressTable", "()V", &[]);
    let this = ctx.read_native_pin(this_pin, this);
    match table_result {
        Ok(Some(Value::Object(Some(table)))) => {
            ctx.set_field_by_name(this, "bindAddressTable", Value::Object(Some(table)));
            ctx.unpin_native_roots(this_pin);
            Ok(Some(Value::Object(Some(table))))
        }
        Ok(other) => {
            ctx.unpin_native_roots(this_pin);
            Ok(other)
        }
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            Err(e)
        }
    }
}

fn native_iot_open_tcp_stream_connection(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let io_thread = obj_arg(args, 0)?;
    let destination = obj_arg(args, 2)?;
    let open_listener = args.get(3).copied().unwrap_or(Value::Object(None));
    let bind_listener = args.get(4).copied().unwrap_or(Value::Object(None));

    // Family-1 fix (cce0079): the two listeners cross `decode`'s dispatches,
    // every alloc below, AND `ensure_source_poller_started` before they are
    // finally used as `handleEvent` receivers — pin them at entry and read
    // the current addresses at dispatch time. `pin_base` is the lowest live
    // handle; truncating it on every exit releases the whole set.
    let ol_pin = match open_listener {
        Value::Object(Some(o)) => Some(ctx.pin_native_root(o)),
        _ => None,
    };
    let bl_pin = match bind_listener {
        Value::Object(Some(o)) => Some(ctx.pin_native_root(o)),
        _ => None,
    };
    let io_thread_pin = ctx.pin_native_root(io_thread);
    let pin_base = ol_pin.or(bl_pin).unwrap_or(io_thread_pin);

    let (host, port) = match decode_xnio_bind_address(ctx, destination) {
        Ok(t) => t,
        Err(e) => {
            ctx.unpin_native_roots(pin_base);
            return Err(e);
        }
    };
    let stream = match TcpStream::connect((host.as_str(), port)) {
        Ok(s) => s,
        Err(e) => {
            ctx.unpin_native_roots(pin_base);
            return Err(mcf_io(format!(
                "XnioIoThread.openTcpStreamConnection: connect {host}:{port} failed: {e}"
            )));
        }
    };
    let local_addr = stream.local_addr().ok();
    let peer_addr = stream.peer_addr().ok();
    let _ = stream.set_nonblocking(true);
    let source_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            ctx.unpin_native_roots(pin_base);
            return Err(mcf_io(format!(
                "XnioIoThread.openTcpStreamConnection: clone stream failed: {e}"
            )));
        }
    };

    let source_id = register_source_channel(ConduitTransport::Tcp(source_stream));
    let sink_id = register_sink_channel(ConduitTransport::Tcp(stream));
    if crate::nbflags().dbg_xnio_tcp {
        eprintln!(
            "[cratonvm:xnio-tcp] open_tcp_stream host={host} port={port} source_id={source_id} sink_id={sink_id}"
        );
    }

    // gc-common w23-b: the shared builder (it was a second copy of
    // `alloc_stream_connection_for_tcp`'s body), and a failed build now
    // unregisters the pair instead of leaking both channels and the socket.
    let io_thread = ctx.read_native_pin(io_thread_pin, io_thread);
    let conn =
        match build_stream_connection(ctx, io_thread, source_id, sink_id, local_addr, peer_addr) {
            Ok(conn) => conn,
            Err(e) => {
                abandon_stream_connection(ctx, source_id, sink_id);
                ctx.unpin_native_roots(pin_base);
                return Err(e);
            }
        };
    let conn_pin = ctx.pin_native_root(conn);

    // The listeners run arbitrary Java — refresh `conn` and each listener
    // through their pins at dispatch time, and again between the two
    // dispatches.
    let mut conn = ctx.read_native_pin(conn_pin, conn);
    if let (Value::Object(Some(listener)), Some(h)) = (bind_listener, bl_pin) {
        let listener = ctx.read_native_pin(h, listener);
        let _ = ctx.invoke_virtual(
            listener,
            "handleEvent",
            "(Ljava/nio/channels/Channel;)V",
            &[Value::Object(Some(conn))],
        );
        conn = ctx.read_native_pin(conn_pin, conn);
    }
    if let (Value::Object(Some(listener)), Some(h)) = (open_listener, ol_pin) {
        let listener = ctx.read_native_pin(h, listener);
        let _ = ctx.invoke_virtual(
            listener,
            "handleEvent",
            "(Ljava/nio/channels/Channel;)V",
            &[Value::Object(Some(conn))],
        );
        conn = ctx.read_native_pin(conn_pin, conn);
    }

    let future = ctx.new_object_initialized(
        "org/xnio/FinishedIoFuture",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(conn))],
    );
    ctx.unpin_native_roots(pin_base);
    future
}

// Small helper for the runtime-error path above.
fn mcf_runtime(msg: &str) -> MethodCallFailed {
    MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::IllegalStateException {
        message: msg.to_string(),
    }))
}

fn mcf_io<S: Into<String>>(message: S) -> MethodCallFailed {
    RuntimeError::IOException {
        message: message.into(),
    }
    .into()
}

// ---------------------------------------------------------------------------
// Registration — called from `lib.rs` after the WildFly Undertow natives so
// T19.7 subsystems see the Xnio / XnioWorker surface.
// ---------------------------------------------------------------------------

fn register_connected_channel_surface(r: &mut NativeMethodRegistry, cls: &str) {
    r.register(
        cls,
        "getPeerAddress",
        "()Ljava/net/SocketAddress;",
        native_connected_get_peer_address,
    );
    r.register(
        cls,
        "getPeerAddress",
        "(Ljava/lang/Class;)Ljava/net/SocketAddress;",
        native_connected_get_peer_address,
    );
    r.register(
        cls,
        "getLocalAddress",
        "()Ljava/net/SocketAddress;",
        native_connected_get_local_address,
    );
    r.register(
        cls,
        "getLocalAddress",
        "(Ljava/lang/Class;)Ljava/net/SocketAddress;",
        native_connected_get_local_address,
    );
    r.register(
        cls,
        "getIoThread",
        "()Lorg/xnio/XnioIoThread;",
        native_connected_get_io_thread,
    );
    r.register(
        cls,
        "getWorker",
        "()Lorg/xnio/XnioWorker;",
        native_connected_get_worker,
    );
}

fn register_accepting_channel_surface(r: &mut NativeMethodRegistry, cls: &str) {
    r.register(
        cls,
        "accept",
        "()Lorg/xnio/channels/ConnectedChannel;",
        native_accepting_accept,
    );
    r.register(
        cls,
        "accept",
        "()Lorg/xnio/channels/CloseableChannel;",
        native_accepting_accept,
    );
    r.register(
        cls,
        "accept",
        "()Lorg/xnio/StreamConnection;",
        native_accepting_accept,
    );
    r.register(
        cls,
        "getAcceptSetter",
        "()Lorg/xnio/ChannelListener$Setter;",
        native_accepting_get_accept_setter,
    );
    r.register(
        cls,
        "getCloseSetter",
        "()Lorg/xnio/ChannelListener$Setter;",
        native_accepting_get_close_setter,
    );
    r.register(
        cls,
        "getLocalAddress",
        "()Ljava/net/SocketAddress;",
        native_accepting_get_local_address,
    );
    r.register(
        cls,
        "getLocalAddress",
        "(Ljava/lang/Class;)Ljava/net/SocketAddress;",
        native_accepting_get_local_address,
    );
    r.register(
        cls,
        "suspendAccepts",
        "()V",
        native_accepting_suspend_accepts,
    );
    r.register(cls, "resumeAccepts", "()V", native_accepting_resume_accepts);
    r.register(
        cls,
        "isAcceptResumed",
        "()Z",
        native_accepting_is_accept_resumed,
    );
    r.register(cls, "wakeupAccepts", "()V", native_accepting_wakeup_accepts);
    r.register(
        cls,
        "awaitAcceptable",
        "()V",
        native_accepting_await_acceptable,
    );
    r.register(
        cls,
        "awaitAcceptable",
        "(JLjava/util/concurrent/TimeUnit;)V",
        native_accepting_await_acceptable,
    );
    r.register(
        cls,
        "getAcceptThread",
        "()Lorg/xnio/XnioExecutor;",
        native_accepting_get_io_thread,
    );
    r.register(
        cls,
        "getIoThread",
        "()Lorg/xnio/XnioIoThread;",
        native_accepting_get_io_thread,
    );
    r.register(
        cls,
        "getWorker",
        "()Lorg/xnio/XnioWorker;",
        native_accepting_get_worker,
    );
    r.register(cls, "close", "()V", native_accepting_close);
    r.register(cls, "isOpen", "()Z", native_accepting_is_open);
    r.register(
        cls,
        "supportsOption",
        "(Lorg/xnio/Option;)Z",
        native_accepting_supports_option,
    );
    r.register(
        cls,
        "getOption",
        "(Lorg/xnio/Option;)Ljava/lang/Object;",
        native_accepting_get_option,
    );
    r.register(
        cls,
        "setOption",
        "(Lorg/xnio/Option;Ljava/lang/Object;)Ljava/lang/Object;",
        native_accepting_set_option,
    );
}

pub fn register_xnio_worker_natives(r: &mut NativeMethodRegistry) {
    for cls in [
        "org/xnio/StreamConnection",
        "org/xnio/Connection",
        CLS_CONNECTED_CHANNEL,
        CLS_BOUND_CHANNEL,
    ] {
        register_connected_channel_surface(r, cls);
    }
    r.register(CLS_ACCEPT_PUMP, "run", "()V", native_accept_pump_run);
    r.register(CLS_SOURCE_POLLER, "run", "()V", native_source_poller_run);
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
        CLS_XNIO,
        "build",
        "(Lorg/xnio/XnioWorker$Builder;)Lorg/xnio/XnioWorker;",
        native_xnio_build_worker,
    );
    r.register(
        CLS_XNIO_WORKER,
        "createTcpConnectionServer",
        "(Ljava/net/InetSocketAddress;Lorg/xnio/ChannelListener;Lorg/xnio/OptionMap;)Lorg/xnio/channels/AcceptingChannel;",
        native_xnio_create_tcp_connection_server,
    );
    for cls in [CLS_XNIO_WORKER, CLS_NIO_XNIO_WORKER] {
        r.register(
            cls,
            "chooseThread",
            "()Lorg/xnio/XnioIoThread;",
            native_worker_get_io_thread,
        );
        r.register(
            cls,
            "getBindAddressTable",
            "()Lorg/wildfly/common/net/CidrAddressTable;",
            native_worker_get_bind_address_table,
        );
    }

    for cls in ["org/xnio/XnioIoThread", "org/xnio/nio/WorkerThread"] {
        r.register(
            cls,
            "openTcpStreamConnection",
            "(Ljava/net/InetSocketAddress;Ljava/net/InetSocketAddress;Lorg/xnio/ChannelListener;Lorg/xnio/ChannelListener;Lorg/xnio/OptionMap;)Lorg/xnio/IoFuture;",
            native_iot_open_tcp_stream_connection,
        );
    }

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
    r.register(CLS_XNIO_WORKER, "shutdown", "()V", native_worker_shutdown);
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
    for cls in [
        CLS_ACCEPTING_CHANNEL,
        CLS_SIMPLE_ACCEPTING_CHANNEL,
        CLS_SUSPENDABLE_ACCEPT_CHANNEL,
        CLS_BOUND_CHANNEL,
        CLS_CLOSEABLE_CHANNEL,
        CLS_CONFIGURABLE_CHANNEL,
        CLS_QUEUED_NIO_TCP_SERVER2,
    ] {
        register_accepting_channel_surface(r, cls);
    }

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
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::mock_ctx;
    use crate::xnio_conduits::{
        drop_source_channel, Pipe, SRC_FIELD_READ_LISTENER, SRC_FIELD_READ_READY_FLAG,
        SRC_FIELD_READ_SUSPENDED,
    };
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
    use std::sync::atomic::{AtomicI32, AtomicUsize};

    static NATIVE_EXECUTE_EXPECTED_RUNNABLE: AtomicUsize = AtomicUsize::new(0);
    static NATIVE_EXECUTE_SEEN_RUNNABLE: AtomicUsize = AtomicUsize::new(0);

    fn native_execute_runnable_hook(
        _ctx: &mut crate::test_utils::MockNativeContext,
        receiver: ObjectRef,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> Option<MethodCallResult> {
        let expected = NATIVE_EXECUTE_EXPECTED_RUNNABLE.load(Ordering::SeqCst);
        if expected == 0 {
            return None;
        }
        let receiver_matches = receiver.as_ptr() as usize == expected;
        let arg_matches = matches!(
            args.first(),
            Some(Value::Object(Some(arg))) if arg.as_ptr() as usize == expected
        );
        if receiver_matches && method_name == "run" && descriptor == "()V" {
            NATIVE_EXECUTE_SEEN_RUNNABLE.fetch_add(1, Ordering::SeqCst);
            return Some(Ok(None));
        }
        if arg_matches && method_name == "execute" && descriptor == "(Ljava/lang/Runnable;)V" {
            NATIVE_EXECUTE_SEEN_RUNNABLE.fetch_add(1, Ordering::SeqCst);
            return Some(Ok(None));
        }
        None
    }

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
        // Wait for the task to run. Generous deadline so the test is robust to
        // OS-thread scheduling delays when the suite runs under heavy parallel
        // load (a healthy worker runs the task in milliseconds).
        let deadline = Instant::now() + Duration::from_secs(30);
        while counter.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        w.shutdown_now();
        assert!(w.await_termination(Duration::from_secs(30)));
    }

    #[test]
    fn t19_7_b_native_execute_runs_java_runnable() {
        let w = XnioWorker::new(
            "t19_7_b_native_exec",
            OptionMap {
                worker_io_threads: Some(1),
                worker_task_core_threads: Some(1),
            },
        );
        let mut ctx = mock_ctx();
        let mirror = alloc_worker_mirror(&mut ctx, CLS_XNIO_WORKER, &w).unwrap();
        register_worker(w.clone());
        let runnable = ctx.fresh_object_ref();
        NATIVE_EXECUTE_EXPECTED_RUNNABLE.store(runnable.as_ptr() as usize, Ordering::SeqCst);
        NATIVE_EXECUTE_SEEN_RUNNABLE.store(0, Ordering::SeqCst);
        ctx.set_invoke_virtual_hook(native_execute_runnable_hook);

        native_worker_execute(
            &mut ctx,
            &[Value::Object(Some(mirror)), Value::Object(Some(runnable))],
        )
        .expect("native execute should accept runnable");

        assert_eq!(NATIVE_EXECUTE_SEEN_RUNNABLE.load(Ordering::SeqCst), 1);
        NATIVE_EXECUTE_EXPECTED_RUNNABLE.store(0, Ordering::SeqCst);
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
        let mirror = alloc_worker_mirror(&mut ctx, CLS_XNIO_WORKER, &w).unwrap();
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
        // Generous deadline: robust to OS-thread scheduling delays under heavy
        // parallel test load (worker shutdown completes near-instantly otherwise).
        assert!(w.await_termination(Duration::from_secs(30)));
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
    fn wf_domain_real_nio_worker_is_adopted_without_slot_handle() {
        let mut ctx = mock_ctx();
        let realish =
            try_alloc_concurrent_synthetic(&mut ctx, CLS_NIO_XNIO_WORKER, WORKER_NUM_SLOTS + 1)
                .unwrap();
        ctx.set_field(realish, WORKER_FIELD_OPTIONS_HANDLE, Value::Object(None));

        let worker = read_worker(&ctx, realish).expect("adopt real worker");
        let same_worker = read_worker(&ctx, realish).expect("read adopted worker");

        assert!(Arc::ptr_eq(&worker, &same_worker));
        assert!(matches!(
            ctx.get_field(realish, WORKER_FIELD_OPTIONS_HANDLE),
            Value::Object(None)
        ));
        worker.shutdown_now();
        assert!(worker.await_termination(Duration::from_secs(2)));
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
        assert!(r
            .find(CLS_XNIO_WORKER, "chooseThread", "()Lorg/xnio/XnioIoThread;")
            .is_some());
        assert!(r.find(CLS_XNIO_WORKER, "shutdown", "()V").is_some());
        assert!(r
            .find(CLS_XNIO_WORKER, "awaitTermination", "(J)Z")
            .is_some());
        assert!(r.find(CLS_XNIO_WORKER, "isShutdown", "()Z").is_some());
        assert!(r
            .find(
                CLS_NIO_XNIO_WORKER,
                "getIoThread",
                "()Lorg/xnio/XnioIoThread;"
            )
            .is_some());
        assert!(r
            .find(
                CLS_NIO_XNIO_WORKER,
                "chooseThread",
                "()Lorg/xnio/XnioIoThread;"
            )
            .is_some());
        assert!(r
            .find(
                CLS_XNIO_WORKER,
                "createTcpConnectionServer",
                "(Ljava/net/InetSocketAddress;Lorg/xnio/ChannelListener;Lorg/xnio/OptionMap;)Lorg/xnio/channels/AcceptingChannel;"
            )
            .is_some());
        assert!(r
            .find(
                CLS_ACCEPTING_CHANNEL,
                "getAcceptSetter",
                "()Lorg/xnio/ChannelListener$Setter;"
            )
            .is_some());
        assert!(r
            .find(
                "org/xnio/StreamConnection",
                "getWorker",
                "()Lorg/xnio/XnioWorker;"
            )
            .is_some());
    }

    #[test]
    fn wf_domain_create_tcp_connection_server_returns_accepting_channel() {
        let mut ctx = mock_ctx();
        let worker = XnioWorker::new("wf_domain_tcp_server", OptionMap::default());
        register_worker(worker.clone());
        let worker_mirror = alloc_worker_mirror(&mut ctx, CLS_XNIO_WORKER, &worker).unwrap();
        let bind_addr =
            try_alloc_concurrent_synthetic(&mut ctx, "java/net/InetSocketAddress", 2).unwrap();
        let host = ctx.create_string("127.0.0.1");
        ctx.set_field(bind_addr, 0, Value::Object(Some(host)));
        ctx.set_field(bind_addr, 1, Value::Int(0));
        let listener =
            try_alloc_concurrent_synthetic(&mut ctx, "org/xnio/ChannelListener", 0).unwrap();

        let channel = match native_xnio_create_tcp_connection_server(
            &mut ctx,
            &[
                Value::Object(Some(worker_mirror)),
                Value::Object(Some(bind_addr)),
                Value::Object(Some(listener)),
                Value::Object(None),
            ],
        )
        .unwrap()
        .unwrap()
        {
            Value::Object(Some(o)) => o,
            other => panic!("expected accepting channel, got {:?}", other),
        };
        assert_eq!(
            ctx.get_field(channel, ACCEPT_FIELD_LOCAL_ADDRESS),
            Value::Object(Some(bind_addr))
        );
        assert_eq!(
            ctx.get_field(channel, ACCEPT_FIELD_ACCEPT_LISTENER),
            Value::Object(Some(listener))
        );
        assert_eq!(
            native_accepting_is_open(&mut ctx, &[Value::Object(Some(channel))])
                .unwrap()
                .unwrap(),
            Value::Int(1)
        );

        let setter =
            match native_accepting_get_accept_setter(&mut ctx, &[Value::Object(Some(channel))])
                .unwrap()
                .unwrap()
            {
                Value::Object(Some(o)) => o,
                other => panic!("expected setter, got {:?}", other),
            };
        assert_eq!(
            ctx.get_field(setter, SETTER_FIELD_CHANNEL_HANDLE),
            Value::Object(Some(channel))
        );
        assert_eq!(
            ctx.get_field(setter, SETTER_FIELD_LISTENER_SLOT_INDEX),
            Value::Int(ACCEPT_FIELD_ACCEPT_LISTENER as i32)
        );

        native_accepting_close(&mut ctx, &[Value::Object(Some(channel))]).unwrap();
        assert_eq!(ctx.get_field(channel, ACCEPT_FIELD_OPEN), Value::Int(0));
        worker.shutdown_now();
        assert!(worker.await_termination(Duration::from_secs(2)));
    }

    fn mark_source_read_listener_invoked(
        ctx: &mut crate::test_utils::MockNativeContext,
        _receiver: ObjectRef,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> Option<MethodCallResult> {
        if method_name == "handleEvent" && descriptor == "(Ljava/nio/channels/Channel;)V" {
            if let Some(Value::Object(Some(source))) = args.first().copied() {
                ctx.set_field(source, SRC_FIELD_READ_READY_FLAG, Value::Int(2));
            }
            return Some(Ok(None));
        }
        None
    }

    #[test]
    fn wf_domain_accept_pump_sleep_polls_registered_sources() {
        let mut ctx = mock_ctx();
        let pipe = Arc::new(Pipe::new(1024));
        pipe.push(b"x");
        let source_id = register_source_channel(ConduitTransport::Pipe(pipe));
        let source = alloc_source_channel_obj(&mut ctx, source_id).unwrap();
        ctx.set_field(source, SRC_FIELD_READ_SUSPENDED, Value::Int(0));
        let listener =
            try_alloc_concurrent_synthetic(&mut ctx, "org/xnio/ChannelListener", 0).unwrap();
        ctx.set_field(
            source,
            SRC_FIELD_READ_LISTENER,
            Value::Object(Some(listener)),
        );
        ctx.set_invoke_virtual_hook(mark_source_read_listener_invoked);

        // gc-common w23-b: the helper no longer takes (or hands back) the
        // pump's channel; the pump re-reads it from its pin.
        accept_pump_blocked_sleep(&mut ctx, Duration::from_millis(0));

        assert_eq!(
            ctx.get_field(source, SRC_FIELD_READ_READY_FLAG),
            Value::Int(2)
        );
        drop_source_channel(&mut ctx, source_id);
    }

    #[test]
    fn wf_domain_outbound_tcp_stream_starts_source_poller() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback listener");
        let addr = listener.local_addr().expect("listener local addr");
        let accepted = Arc::new(AtomicBool::new(false));
        let accepted_for_thread = accepted.clone();
        let accept_thread = thread::spawn(move || {
            if let Ok((_stream, _peer)) = listener.accept() {
                accepted_for_thread.store(true, Ordering::SeqCst);
            }
        });

        fn synthetic_inet_socket_address_getters(
            ctx: &mut crate::test_utils::MockNativeContext,
            receiver: ObjectRef,
            method_name: &str,
            descriptor: &str,
            _args: &[Value],
        ) -> Option<MethodCallResult> {
            match (method_name, descriptor) {
                ("getPort", "()I") => Some(Ok(Some(ctx.get_field(receiver, 1)))),
                ("getHostString", "()Ljava/lang/String;") => {
                    Some(Ok(Some(ctx.get_field(receiver, 0))))
                }
                _ => None,
            }
        }

        let mut ctx = mock_ctx();
        ctx.set_invoke_virtual_hook(synthetic_inet_socket_address_getters);
        let vm = ctx.vm_identity();
        source_poller_started_vms()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&vm);

        let worker = XnioWorker::new(
            "wf_domain_outbound_poller",
            OptionMap {
                worker_io_threads: Some(1),
                worker_task_core_threads: Some(1),
            },
        );
        register_worker(worker.clone());
        let worker_mirror = alloc_worker_mirror(&mut ctx, CLS_XNIO_WORKER, &worker).unwrap();
        let io_thread =
            match native_worker_get_io_thread(&mut ctx, &[Value::Object(Some(worker_mirror))])
                .unwrap()
                .unwrap()
            {
                Value::Object(Some(o)) => o,
                other => panic!("expected XnioIoThread mirror, got {:?}", other),
            };
        let destination =
            try_alloc_concurrent_synthetic(&mut ctx, "java/net/InetSocketAddress", 2).unwrap();
        let host = ctx.create_string("127.0.0.1");
        ctx.set_field(destination, 0, Value::Object(Some(host)));
        ctx.set_field(destination, 1, Value::Int(addr.port() as i32));

        let result = native_iot_open_tcp_stream_connection(
            &mut ctx,
            &[
                Value::Object(Some(io_thread)),
                Value::Object(None),
                Value::Object(Some(destination)),
                Value::Object(None),
                Value::Object(None),
                Value::Object(None),
            ],
        );
        assert!(
            result.is_ok(),
            "openTcpStreamConnection must succeed: {result:?}"
        );
        assert!(source_poller_started_vms()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&vm));

        let _ = accept_thread.join();
        assert!(accepted.load(Ordering::SeqCst));
        worker.shutdown_now();
        assert!(worker.await_termination(Duration::from_secs(2)));
    }

    /// Declare the synthetic `XnioIoThread` layout
    /// (`classloading/src/class_manager.rs`: id, workerHandle, selectorHandle,
    /// state) for `sample`'s class on the mock, so `set_field_by_name` lands
    /// the way it does in the VM. gc-common w29-d: the mirror's fields are now
    /// the only record of its worker.
    fn declare_synthetic_iot_layout(ctx: &crate::test_utils::MockNativeContext, sample: ObjectRef) {
        let cid = ctx.class_id_of_object(sample);
        let field = |name: &str, desc: &str, slot: usize| cratonvm_native_api::FieldMetadata {
            name: name.to_string(),
            descriptor: desc.to_string(),
            access_flags: 0,
            slot_index: slot,
            declaring_class_id: cid,
            is_static: false,
        };
        ctx.set_declared_fields(
            cid,
            vec![
                field("id", "J", 0),
                field("workerHandle", "Lorg/xnio/XnioWorker;", 1),
                field("selectorHandle", "J", 2),
                field("state", "I", 3),
            ],
        );
    }

    fn get_io_thread_mirror(
        ctx: &mut crate::test_utils::MockNativeContext,
        worker_mirror: ObjectRef,
    ) -> ObjectRef {
        match native_worker_get_io_thread(ctx, &[Value::Object(Some(worker_mirror))])
            .unwrap()
            .unwrap()
        {
            Value::Object(Some(o)) => o,
            other => panic!("expected XnioIoThread mirror, got {:?}", other),
        }
    }

    #[test]
    fn wf_domain_stream_connection_worker_identity_uses_io_thread_mirror() {
        let mut ctx = mock_ctx();
        let worker = XnioWorker::new("wf_domain_stream_worker_identity", OptionMap::default());
        register_worker(worker.clone());
        let worker_mirror = alloc_worker_mirror(&mut ctx, CLS_XNIO_WORKER, &worker).unwrap();

        // The first mirror only tells us the class the mock allocates it as.
        let sample = get_io_thread_mirror(&mut ctx, worker_mirror);
        assert_ne!(
            ctx.class_id_of_object(sample),
            ctx.class_id_of_object(worker_mirror),
            "premise: the mirror and the worker are distinct mock classes"
        );
        declare_synthetic_iot_layout(&ctx, sample);
        let io_thread = get_io_thread_mirror(&mut ctx, worker_mirror);
        assert_eq!(
            io_thread_worker_mirror(&ctx, io_thread),
            Some(worker_mirror)
        );
        assert!(matches!(
            ctx.get_field_by_name(io_thread, "number"),
            Value::Int(_) | Value::Long(_)
        ));

        worker.shutdown_now();
        assert!(worker.await_termination(Duration::from_secs(2)));
    }

    /// gc-common w29-d
    /// (`common-w28b-xnio-io-thread-mirror-registry-holds-unremapped-worker-refs-FIXED-20260923`):
    /// a collection that moves the worker rewrites the mirror's `workerHandle`
    /// field; `getWorker()` must answer the NEW address. It used to answer the
    /// pre-move address from an unremapped identity-hash registry consulted
    /// before the field.
    #[test]
    fn w29d_io_thread_get_worker_follows_a_moved_worker() {
        let mut ctx = mock_ctx();
        let worker = XnioWorker::new("w29d_moved_worker", OptionMap::default());
        register_worker(worker.clone());
        let worker_mirror = alloc_worker_mirror(&mut ctx, CLS_XNIO_WORKER, &worker).unwrap();
        let sample = get_io_thread_mirror(&mut ctx, worker_mirror);
        declare_synthetic_iot_layout(&ctx, sample);
        let io_thread = get_io_thread_mirror(&mut ctx, worker_mirror);
        assert_eq!(io_thread_worker_mirror(&ctx, io_thread), Some(worker_mirror));

        // Stand in for the collector: the worker now lives elsewhere and the
        // remap rewrote the mirror's field (slot 1 of the synthetic layout).
        let moved = ctx.fresh_object_ref();
        ctx.set_field_by_name(io_thread, "workerHandle", Value::Object(Some(moved)));
        assert_eq!(io_thread_worker_mirror(&ctx, io_thread), Some(moved));
        assert_eq!(
            crate::xnio_io_thread::iot_worker_mirror(&ctx, io_thread),
            Some(moved),
            "XnioIoThread.getWorker() reads the same field"
        );

        // 10 s, not 2: this test starts the worker's I/O threads (the two
        // `getIoThread` calls), and under the full parallel suite their join
        // overran 2 s about one run in two. Termination is not what it checks.
        worker.shutdown_now();
        assert!(worker.await_termination(Duration::from_secs(10)));
    }

    #[test]
    fn t19_7_b_java_mirror_round_trip_through_registry() {
        // Verify the native surface can build + read a worker mirror,
        // find the backing Arc via the options_handle slot, and call
        // through to isShutdown / shutdown cleanly.
        let mut ctx = mock_ctx();
        let worker = XnioWorker::new("t19_7_b_mirror", OptionMap::default());
        register_worker(worker.clone());
        let mirror = alloc_worker_mirror(&mut ctx, CLS_XNIO_WORKER, &worker).unwrap();
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

    /// gc-common w23-b: a VM's teardown drops its worker-mirror, connection-
    /// address, source-poller and accepting-listener rows (closing the
    /// listener) and shuts its workers down; another VM's rows and workers
    /// are untouched.
    #[test]
    fn w23b_vm_teardown_drops_worker_rows_and_shuts_its_workers_down() {
        const VM_A: usize = 0x0B23_9A08;
        const VM_B: usize = 0x0B23_9B08;
        struct Forget;
        impl Drop for Forget {
            fn drop(&mut self) {
                forget_vm_xnio_worker_rows(VM_A);
                forget_vm_xnio_worker_rows(VM_B);
            }
        }
        let _forget = Forget;
        let mut a = mock_ctx();
        a.set_vm_identity(VM_A);
        let mut b = mock_ctx();
        b.set_vm_identity(VM_B);
        let opts = || OptionMap {
            worker_io_threads: Some(1),
            worker_task_core_threads: Some(1),
        };
        let worker_a = XnioWorker::new("w23b_teardown_a", opts());
        register_worker(worker_a.clone());
        let mirror_a = alloc_worker_mirror(&mut a, CLS_XNIO_WORKER, &worker_a).unwrap();
        let worker_b = XnioWorker::new("w23b_teardown_b", opts());
        register_worker(worker_b.clone());
        let mirror_b = alloc_worker_mirror(&mut b, CLS_XNIO_WORKER, &worker_b).unwrap();
        let conn_a = a.fresh_object_ref();
        remember_stream_connection_addresses(&a, conn_a, None, None);
        let conn_b = b.fresh_object_ref();
        remember_stream_connection_addresses(&b, conn_b, None, None);
        source_poller_started_vms()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(VM_A);
        source_poller_started_vms()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(VM_B);
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback listener");
        let listener_a = register_accepting_listener(VM_A, listener);
        assert!(accepting_listener_registered(listener_a));

        forget_vm_xnio_worker_rows(VM_A);

        assert!(lookup_worker(worker_a.id).is_none());
        assert!(worker_a.is_shutdown(), "A's worker is shut down");
        assert!(lookup_worker_object(&a, mirror_a).is_none());
        assert!(stream_connection_addresses(&a, conn_a).is_none());
        let a_poller = source_poller_started_vms()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&VM_A);
        assert!(
            !a_poller,
            "a VM reusing A's identity must start its own poller"
        );
        assert!(!accepting_listener_registered(listener_a));

        assert!(lookup_worker_object(&b, mirror_b).is_some());
        assert!(stream_connection_addresses(&b, conn_b).is_some());
        let b_poller = source_poller_started_vms()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&VM_B);
        assert!(b_poller);
        assert!(!worker_b.is_shutdown());

        assert!(worker_a.await_termination(Duration::from_secs(2)));
        worker_b.shutdown_now();
        assert!(worker_b.await_termination(Duration::from_secs(2)));
    }
}

/// gc-common w24-a (`common-w23b-xnio-identity-hash-keyed-rows-cross-and-outlive-connections`).
/// The mock's identity hash is the address truncated to `i32`, so addresses
/// 4 GiB apart share one. The fake objects are never dereferenced: filing and
/// looking up these rows only keys them. Each test has its own VM identities
/// and forgets them in a guard.
#[cfg(test)]
mod w24a_worker_key_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    use crate::xnio_conduits::{drop_sink_channel, drop_source_channel, Pipe};
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    // Private VM identities: no other test uses them.
    const W24A_VM_A: usize = 0x0A24_9A08;
    const W24A_VM_B: usize = 0x0A24_9B08;
    const W24A_VM_C: usize = 0x0A24_9C08;
    const W24A_VM_D: usize = 0x0A24_9D08;
    const W24A_VM_E: usize = 0x0A24_9E08;

    struct Forget(&'static [usize]);
    impl Drop for Forget {
        fn drop(&mut self) {
            for &vm in self.0 {
                forget_vm_xnio_worker_rows(vm);
                crate::xnio_conduits::forget_vm_xnio_conduit_rows(vm);
                crate::forget_vm_lock_keys(vm);
            }
        }
    }

    fn obj(addr: usize) -> ObjectRef {
        // SAFETY: a key only; the key registry and the mock hash the address
        // and never dereference it.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    fn ctx_for(vm: usize) -> MockNativeContext {
        let c = crate::test_utils::mock_ctx();
        c.set_vm_identity(vm);
        c
    }

    fn peer(port: u16) -> Option<SocketAddr> {
        Some(SocketAddr::from(([127, 0, 0, 1], port)))
    }

    fn address_rows_of(vm: usize) -> usize {
        stream_connection_address_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter(|(owner, _)| *owner == vm)
            .count()
    }

    /// A connection's address row outlives its close -- `getPeerAddress`
    /// keeps answering while the `StreamConnection` lives -- and goes at the
    /// first sweep that finds the connection object dead. It used to stay
    /// until VM teardown: one row per connection ever accepted.
    #[test]
    fn w24a_an_address_row_outlives_the_close_and_goes_with_the_connection() {
        let _forget = Forget(&[W24A_VM_A]);
        let mut a = ctx_for(W24A_VM_A);
        let conn = a.fresh_object_ref();
        remember_stream_connection_addresses(&a, conn, peer(8080), peer(40001));

        // The connection's two halves close (the shutdown natives' path).
        let source_id = crate::xnio_conduits::register_source_channel(
            crate::xnio_conduits::ConduitTransport::Pipe(Arc::new(Pipe::new(16))),
        );
        let sink_id = crate::xnio_conduits::register_sink_channel(
            crate::xnio_conduits::ConduitTransport::Pipe(Arc::new(Pipe::new(16))),
        );
        drop_source_channel(&mut a, source_id);
        drop_sink_channel(&mut a, sink_id);
        let after_close = stream_connection_addresses(&a, conn).and_then(|x| x.peer);
        assert_eq!(
            after_close,
            peer(40001),
            "a closed connection keeps its peer"
        );
        assert_eq!(address_rows_of(W24A_VM_A), 1);

        let conn_addr = conn.as_ptr() as usize;
        assert_eq!(crate::gc_sweep_lock_keys(W24A_VM_A, &|x| x != conn_addr), 1);
        assert_eq!(
            address_rows_of(W24A_VM_A),
            0,
            "the row goes with the object"
        );
        assert!(stream_connection_addresses(&a, conn).is_none());
    }

    /// A dead connection's addresses are not answered for a later object
    /// carrying its identity hash (the old `(vm, identity hash)` key did).
    #[test]
    fn w24a_a_dead_connections_addresses_are_not_found_by_a_collider() {
        let _forget = Forget(&[W24A_VM_B]);
        let b = ctx_for(W24A_VM_B);
        let dead = 0x1_A249_1000usize;
        let newcomer = 0x2_A249_1000usize;
        assert_eq!(
            b.identity_hash_code(obj(dead)),
            b.identity_hash_code(obj(newcomer)),
            "premise: the two objects share an identity hash"
        );
        remember_stream_connection_addresses(&b, obj(dead), None, peer(40002));
        assert_eq!(crate::gc_sweep_lock_keys(W24A_VM_B, &|x| x != dead), 1);
        assert!(stream_connection_addresses(&b, obj(newcomer)).is_none());

        remember_stream_connection_addresses(&b, obj(newcomer), None, peer(40003));
        let answered = stream_connection_addresses(&b, obj(newcomer)).and_then(|x| x.peer);
        assert_eq!(answered, peer(40003));
    }

    /// Two VMs filing one address keep their own rows; one VM's sweep costs
    /// the other nothing.
    #[test]
    fn w24a_two_vms_keep_their_own_address_rows() {
        let _forget = Forget(&[W24A_VM_C, W24A_VM_D]);
        let c = ctx_for(W24A_VM_C);
        let d = ctx_for(W24A_VM_D);
        let addr = 0x1_A24A_1000usize;
        remember_stream_connection_addresses(&c, obj(addr), None, peer(40004));
        remember_stream_connection_addresses(&d, obj(addr), None, peer(40005));
        let c_peer = stream_connection_addresses(&c, obj(addr)).and_then(|x| x.peer);
        let d_peer = stream_connection_addresses(&d, obj(addr)).and_then(|x| x.peer);
        assert_eq!(c_peer, peer(40004));
        assert_eq!(d_peer, peer(40005));

        crate::gc_sweep_lock_keys(W24A_VM_C, &|x| x != addr);
        assert!(stream_connection_addresses(&c, obj(addr)).is_none());
        let d_peer = stream_connection_addresses(&d, obj(addr)).and_then(|x| x.peer);
        assert_eq!(d_peer, peer(40005));
        assert_eq!(address_rows_of(W24A_VM_D), 1);
    }

    /// A worker mirror's row goes with the mirror, and the worker is still
    /// shut down at its VM's teardown (teardown used to find it through that
    /// row alone).
    #[test]
    fn w24a_a_dead_mirrors_worker_is_still_shut_down_with_its_vm() {
        let _forget = Forget(&[W24A_VM_E]);
        let e = ctx_for(W24A_VM_E);
        let mirror = 0x1_A24B_1000usize;
        let worker = XnioWorker::new(
            "w24a_dead_mirror",
            OptionMap {
                worker_io_threads: Some(1),
                worker_task_core_threads: Some(1),
            },
        );
        register_worker(worker.clone());
        remember_worker_object(&e, obj(mirror), &worker);
        assert!(lookup_worker_object(&e, obj(mirror)).is_some());

        assert_eq!(crate::gc_sweep_lock_keys(W24A_VM_E, &|x| x != mirror), 1);
        assert!(lookup_worker_object(&e, obj(mirror)).is_none());
        let rows = worker_object_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .by_key
            .values()
            .filter(|(owner, _)| *owner == W24A_VM_E)
            .count();
        assert_eq!(rows, 0, "the mirror's row goes with the mirror");
        assert!(lookup_worker(worker.id).is_some());

        forget_vm_xnio_worker_rows(W24A_VM_E);
        assert!(lookup_worker(worker.id).is_none());
        assert!(worker.is_shutdown(), "the VM's teardown still finds it");
        assert!(worker.await_termination(Duration::from_secs(2)));
    }
}
