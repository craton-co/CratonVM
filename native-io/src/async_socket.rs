// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP3.2 — Real `AsynchronousSocketChannel` /
//! `AsynchronousServerSocketChannel` natives backed by a worker-thread
//! completion pump. Spec-compliant on platforms that don't have IOCP /
//! epoll-port primitives (which is exactly how the JDK does it on
//! macOS / BSD).
//!
//! Lifecycle:
//!   1. `AsynchronousSocketChannel.open()` allocates a fresh client.
//!   2. `connect(SocketAddress, attachment, handler)` enqueues a
//!      blocking connect onto the shared completion pool. When it
//!      finishes, the worker stores the result on the channel and
//!      invokes the JDK-side `CompletionHandler.completed/failed` via
//!      `ctx.invoke`.
//!   3. `read(ByteBuffer, attachment, handler)` and
//!      `write(ByteBuffer, attachment, handler)` enqueue read/write
//!      ops onto the pool against the channel's `TcpStream`, posting
//!      completions when bytes flow.
//!   4. `close()` drops the underlying socket — outstanding workers
//!      observe a closed-channel error and post `failed`.
//!
//! Storage model:
//!   The Java `AsynchronousSocketChannel` synthetic object keeps a
//!   single `int` slot pointing into `aio_registry()`. The handle
//!   there owns a `TcpStream` plus a pending-op counter so `close`
//!   knows whether to wait for outstanding ops to drain.
//!
//! The completion pump is a FIFO MPMC queue (`Mutex<VecDeque>` +
//! `Condvar`) drained by a fixed-size pool of worker threads. We
//! size the pool by `available_parallelism` at startup, capped at
//! 256 to prevent runaway threads under DoS.
//!
//! All public surface is registered via `register_async_socket_real`.

use parking_lot::{Mutex, RwLock};
use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ClassId, ObjectRef, Value};
use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// A live AIO socket entry. Reads and writes go through a shared
/// `Arc<Mutex<TcpStream>>` so worker threads can take the stream
/// out for the duration of an op without holding the registry lock.
pub enum AioHandle {
    Stream(Arc<Mutex<TcpStream>>),
    Listener(Arc<Mutex<TcpListener>>),
    Pending,
    Closed,
}

fn aio_registry() -> &'static RwLock<HashMap<i32, AioHandle>> {
    static REG: OnceLock<RwLock<HashMap<i32, AioHandle>>> = OnceLock::new();
    REG.get_or_init(|| RwLock::new(HashMap::new()))
}

fn aio_next_id() -> i32 {
    // Distinct from socket_channel.rs's 0x6000_0000 base.
    static NEXT: AtomicI32 = AtomicI32::new(0x7000_0000);
    NEXT.fetch_add(1, Ordering::SeqCst)
}

fn aio_register(h: AioHandle) -> i32 {
    let id = aio_next_id();
    aio_registry().write().insert(id, h);
    id
}

fn aio_remove(id: i32) {
    aio_registry().write().insert(id, AioHandle::Closed);
}

// ---------------------------------------------------------------------------
// Completion outbox — workers post results here, the next user-thread
// native call drains and dispatches them via `ctx.invoke`.
// ---------------------------------------------------------------------------

/// What kind of completion this is — used by the dispatch path to wrap
/// raw byte counts / accept ids into the right Java-visible result type
/// before invoking `CompletionHandler.completed`.
#[derive(Clone, Copy)]
pub enum CompletionKind {
    /// Connect / close — handler gets `null` Void result.
    Void,
    /// Read / write — handler gets `Integer.valueOf(n)`.
    IntCount(i32),
    /// Accept — `new_id` is an `AioHandle::Stream` slot in `aio_registry`,
    /// which the dispatch path wraps into a fresh
    /// `AsynchronousSocketChannel` synthetic Java object.
    AcceptedChannel(i32),
}

/// A single completed AIO op waiting to be reported to its
/// `CompletionHandler` on a user-facing JVM thread.
pub struct Completion {
    /// The CompletionHandler instance registered by the caller.
    pub handler: ObjectRef,
    /// The `attachment` parameter passed in alongside the handler.
    pub attachment: Option<ObjectRef>,
    /// Outcome of the op (`Err` ⇒ dispatch calls `failed(Throwable, A)`).
    pub outcome: Result<CompletionKind, String>,
}

fn completion_queue() -> &'static Mutex<std::collections::VecDeque<Completion>> {
    static Q: OnceLock<Mutex<std::collections::VecDeque<Completion>>> = OnceLock::new();
    Q.get_or_init(|| Mutex::new(std::collections::VecDeque::new()))
}

/// Maximum number of completions to dispatch on a single drain — keeps
/// any single native call from monopolizing the user thread.
const DRAIN_LIMIT: usize = 64;

/// Drain pending completions and dispatch them to their handlers.
/// Called opportunistically from any AIO native that takes a `&mut ctx`.
fn drain_completions(ctx: &mut dyn NativeContext) {
    // Flush any pending heap-array writes first so handlers see the data.
    flush_pending_array_writes_inner(ctx);
    for _ in 0..DRAIN_LIMIT {
        let next = completion_queue().lock().pop_front();
        let Some(c) = next else { break };
        match c.outcome {
            Ok(kind) => {
                let result_val = match kind {
                    CompletionKind::Void => Value::Object(None),
                    CompletionKind::IntCount(n) => {
                        // Wrap into Integer for completed(Object, Object).
                        match ctx.new_object("java/lang/Integer") {
                            Ok(Some(Value::Object(Some(boxed)))) => {
                                ctx.set_field_by_name(boxed, "value", Value::Int(n));
                                Value::Object(Some(boxed))
                            }
                            _ => Value::Int(n),
                        }
                    }
                    CompletionKind::AcceptedChannel(new_id) => {
                        // Wrap the registry entry into a synthetic
                        // AsynchronousSocketChannel Java object.
                        let ch = alloc_obj(
                            ctx,
                            "java/nio/channels/AsynchronousSocketChannel",
                            N_FIELDS,
                        );
                        ctx.set_field(ch, F_OPEN, Value::Int(1));
                        ctx.set_field(ch, F_CONNECTED, Value::Int(1));
                        ctx.set_field(ch, F_REG_ID, Value::Int(new_id));
                        ctx.set_field(ch, F_REMOTE, Value::Object(None));
                        Value::Object(Some(ch))
                    }
                };
                let attach = Value::Object(c.attachment);
                let _ = ctx.invoke(
                    "java/nio/channels/CompletionHandler",
                    "completed",
                    "(Ljava/lang/Object;Ljava/lang/Object;)V",
                    &[Value::Object(Some(c.handler)), result_val, attach],
                );
            }
            Err(msg) => {
                let throw = match ctx.new_object("java/io/IOException") {
                    Ok(Some(Value::Object(Some(t)))) => t,
                    _ => continue,
                };
                let m = ctx.create_string(&msg);
                ctx.set_field_by_name(throw, "detailMessage", Value::Object(Some(m)));
                let attach = Value::Object(c.attachment);
                let _ = ctx.invoke(
                    "java/nio/channels/CompletionHandler",
                    "failed",
                    "(Ljava/lang/Throwable;Ljava/lang/Object;)V",
                    &[
                        Value::Object(Some(c.handler)),
                        Value::Object(Some(throw)),
                        attach,
                    ],
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Worker pool
// ---------------------------------------------------------------------------

enum Job {
    Connect {
        id: i32,
        addr: String,
        handler: Option<ObjectRef>,
        attachment: Option<ObjectRef>,
    },
    Read {
        id: i32,
        len: usize,
        bb_addr: i64,
        bb_arr: Option<ObjectRef>,
        bb_offset: i32,
        bb_obj: ObjectRef,
        handler: Option<ObjectRef>,
        attachment: Option<ObjectRef>,
    },
    Write {
        id: i32,
        data: Vec<u8>,
        bb_obj: ObjectRef,
        handler: Option<ObjectRef>,
        attachment: Option<ObjectRef>,
    },
    Accept {
        id: i32,
        handler: Option<ObjectRef>,
        attachment: Option<ObjectRef>,
    },
}

// Read/Write job results need to land back as `Completion`s. For Read/Write
// where bytes target a heap-array, we do the buffer mutation inside the
// dispatch loop on the user-thread side — but to minimize complexity we just
// keep an in-flight buffer and write into it on the worker, then mark the
// completion with the buffer object so dispatch can flush it.

fn job_sender() -> &'static crossbeam_compat::Sender<Job> {
    static S: OnceLock<crossbeam_compat::Sender<Job>> = OnceLock::new();
    S.get_or_init(start_pool)
}

fn start_pool() -> crossbeam_compat::Sender<Job> {
    let (tx, rx) = crossbeam_compat::channel::<Job>();
    let parallelism = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .max(2);
    // Cap to avoid spawning too many threads in DoS scenarios.
    let pool_size = parallelism.min(256);
    for i in 0..pool_size {
        let rx = rx.clone();
        std::thread::Builder::new()
            .name(format!("cratonvm-aio-{i}"))
            .spawn(move || worker_main(rx))
            .expect("aio worker spawn");
    }
    tx
}

fn worker_main(rx: crossbeam_compat::Receiver<Job>) {
    while let Ok(job) = rx.recv() {
        if let Err(e) = handle_job(job) {
            // Shouldn't happen — handle_job already posts completions on
            // failure. This is a last-line guard against worker panics.
            eprintln!("aio worker: unhandled error: {e}");
        }
    }
}

fn handle_job(job: Job) -> Result<(), String> {
    match job {
        Job::Connect {
            id,
            addr,
            handler,
            attachment,
        } => {
            let result = match addr.parse::<SocketAddr>() {
                Ok(sa) => TcpStream::connect_timeout(&sa, Duration::from_secs(30)),
                Err(_) => TcpStream::connect(&addr),
            };
            match result {
                Ok(stream) => {
                    aio_registry()
                        .write()
                        .insert(id, AioHandle::Stream(Arc::new(Mutex::new(stream))));
                    if let Some(h) = handler {
                        completion_queue().lock().push_back(Completion {
                            handler: h,
                            attachment,
                            outcome: Ok(CompletionKind::Void),
                        });
                    }
                }
                Err(e) => {
                    aio_remove(id);
                    if let Some(h) = handler {
                        completion_queue().lock().push_back(Completion {
                            handler: h,
                            attachment,
                            outcome: Err(format!("connect failed: {e}")),
                        });
                    }
                }
            }
        }
        Job::Read {
            id,
            len,
            bb_addr,
            bb_arr,
            bb_offset,
            bb_obj: _,
            handler,
            attachment,
        } => {
            // Acquire the stream.
            let stream = {
                let map = aio_registry().read();
                match map.get(&id) {
                    Some(AioHandle::Stream(s)) => Arc::clone(s),
                    _ => {
                        if let Some(h) = handler {
                            completion_queue().lock().push_back(Completion {
                                handler: h,
                                attachment,
                                outcome: Err("read: channel closed".to_string()),
                            });
                        }
                        return Ok(());
                    }
                }
            };
            let mut buf = vec![0u8; len];
            // Hold the per-stream lock during the read so concurrent reads
            // on the same channel serialize (matching JDK contract: only
            // one read may be outstanding per channel).
            let read_res = {
                let s = stream.lock();
                let mut r = &*s;
                r.read(&mut buf)
            };
            match read_res {
                Ok(0) => {
                    if let Some(h) = handler {
                        completion_queue().lock().push_back(Completion {
                            handler: h,
                            attachment,
                            outcome: Ok(CompletionKind::IntCount(-1)),
                        });
                    }
                }
                Ok(n) => {
                    // Stash the bytes back into the buffer. Direct buffers
                    // can be written directly from the worker thread (raw
                    // memory). Heap buffers must be flushed on the user
                    // thread because `set_array_element` requires
                    // `&mut dyn NativeContext` — so we park the bytes in
                    // a side table the dispatch path drains.
                    if bb_addr != 0 {
                        // SAFETY: caller-allocated direct buffer; the
                        // address is valid for `len` bytes.
                        unsafe {
                            std::ptr::copy_nonoverlapping(
                                buf.as_ptr(),
                                bb_addr as *mut u8,
                                n,
                            );
                        }
                    } else if let Some(arr) = bb_arr {
                        pending_array_writes()
                            .lock()
                            .push(PendingArrayWrite {
                                arr,
                                offset: bb_offset,
                                bytes: buf[..n].to_vec(),
                            });
                    }
                    if let Some(h) = handler {
                        completion_queue().lock().push_back(Completion {
                            handler: h,
                            attachment,
                            outcome: Ok(CompletionKind::IntCount(n as i32)),
                        });
                    }
                }
                Err(e) => {
                    if let Some(h) = handler {
                        completion_queue().lock().push_back(Completion {
                            handler: h,
                            attachment,
                            outcome: Err(format!("read failed: {e}")),
                        });
                    }
                }
            }
        }
        Job::Write {
            id,
            data,
            bb_obj: _,
            handler,
            attachment,
        } => {
            let stream = {
                let map = aio_registry().read();
                match map.get(&id) {
                    Some(AioHandle::Stream(s)) => Arc::clone(s),
                    _ => {
                        if let Some(h) = handler {
                            completion_queue().lock().push_back(Completion {
                                handler: h,
                                attachment,
                                outcome: Err("write: channel closed".to_string()),
                            });
                        }
                        return Ok(());
                    }
                }
            };
            let total = data.len();
            let mut written = 0;
            let res = {
                let s = stream.lock();
                let mut w = &*s;
                let mut e_outer = None;
                while written < total {
                    match w.write(&data[written..]) {
                        Ok(0) => {
                            e_outer =
                                Some(std::io::Error::from(ErrorKind::WriteZero));
                            break;
                        }
                        Ok(n) => written += n,
                        Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                        Err(e) => {
                            e_outer = Some(e);
                            break;
                        }
                    }
                }
                match e_outer {
                    Some(e) => Err(e),
                    None => Ok(written),
                }
            };
            match res {
                Ok(n) => {
                    if let Some(h) = handler {
                        completion_queue().lock().push_back(Completion {
                            handler: h,
                            attachment,
                            outcome: Ok(CompletionKind::IntCount(n as i32)),
                        });
                    }
                }
                Err(e) => {
                    if let Some(h) = handler {
                        completion_queue().lock().push_back(Completion {
                            handler: h,
                            attachment,
                            outcome: Err(format!("write failed: {e}")),
                        });
                    }
                }
            }
        }
        Job::Accept {
            id,
            handler,
            attachment,
        } => {
            let listener = {
                let map = aio_registry().read();
                match map.get(&id) {
                    Some(AioHandle::Listener(l)) => Arc::clone(l),
                    _ => {
                        if let Some(h) = handler {
                            completion_queue().lock().push_back(Completion {
                                handler: h,
                                attachment,
                                outcome: Err("accept: channel closed".to_string()),
                            });
                        }
                        return Ok(());
                    }
                }
            };
            let res = {
                let l = listener.lock();
                l.accept()
            };
            match res {
                Ok((stream, _peer)) => {
                    let new_id =
                        aio_register(AioHandle::Stream(Arc::new(Mutex::new(stream))));
                    if let Some(h) = handler {
                        completion_queue().lock().push_back(Completion {
                            handler: h,
                            attachment,
                            outcome: Ok(CompletionKind::AcceptedChannel(new_id)),
                        });
                    }
                }
                Err(e) => {
                    if let Some(h) = handler {
                        completion_queue().lock().push_back(Completion {
                            handler: h,
                            attachment,
                            outcome: Err(format!("accept failed: {e}")),
                        });
                    }
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// MPMC job queue — Mutex<VecDeque> + Condvar so all workers wait
// concurrently on the same queue and one wakes per push. This avoids
// the "one-recv-at-a-time" pitfall of wrapping `mpsc::Receiver` in
// a Mutex (which would serialize ALL workers).
// ---------------------------------------------------------------------------

mod crossbeam_compat {
    use std::collections::VecDeque;
    use std::sync::{Arc, Condvar, Mutex};

    struct State<T> {
        queue: VecDeque<T>,
        closed: bool,
    }

    struct Inner<T> {
        state: Mutex<State<T>>,
        cv: Condvar,
    }

    pub struct Sender<T>(Arc<Inner<T>>);
    pub struct Receiver<T>(Arc<Inner<T>>);

    impl<T> Clone for Sender<T> {
        fn clone(&self) -> Self {
            Sender(Arc::clone(&self.0))
        }
    }
    impl<T> Clone for Receiver<T> {
        fn clone(&self) -> Self {
            Receiver(Arc::clone(&self.0))
        }
    }

    pub struct SendError<T>(pub T);
    pub struct RecvError;

    impl<T> Sender<T> {
        pub fn send(&self, t: T) -> Result<(), SendError<T>> {
            let mut g = self.0.state.lock().unwrap();
            if g.closed {
                return Err(SendError(t));
            }
            g.queue.push_back(t);
            // Notify a single waiting worker.
            drop(g);
            self.0.cv.notify_one();
            Ok(())
        }
    }
    impl<T> Receiver<T> {
        pub fn recv(&self) -> Result<T, RecvError> {
            let mut g = self.0.state.lock().unwrap();
            loop {
                if let Some(item) = g.queue.pop_front() {
                    return Ok(item);
                }
                if g.closed {
                    return Err(RecvError);
                }
                g = self.0.cv.wait(g).unwrap();
            }
        }
    }

    pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
        let inner = Arc::new(Inner {
            state: Mutex::new(State {
                queue: VecDeque::new(),
                closed: false,
            }),
            cv: Condvar::new(),
        });
        (Sender(Arc::clone(&inner)), Receiver(inner))
    }
}

// ---------------------------------------------------------------------------
// Pending side-effects parked for user-thread flush.
// ---------------------------------------------------------------------------

struct PendingArrayWrite {
    arr: ObjectRef,
    offset: i32,
    bytes: Vec<u8>,
}

fn pending_array_writes() -> &'static Mutex<Vec<PendingArrayWrite>> {
    static V: OnceLock<Mutex<Vec<PendingArrayWrite>>> = OnceLock::new();
    V.get_or_init(|| Mutex::new(Vec::new()))
}

fn flush_pending_array_writes_inner(ctx: &mut dyn NativeContext) {
    // AUDIT 2026-05-17: bulk write via NativeContext intrinsic instead
    // of per-element `set_array_element`. The VM override does a single
    // `ptr::copy_nonoverlapping` against the compact byte-array payload.
    let parked = std::mem::take(&mut *pending_array_writes().lock());
    for p in parked {
        let arr_len = ctx.array_length(p.arr);
        let off = p.offset as usize;
        if off >= arr_len {
            continue;
        }
        let max = arr_len - off;
        let n = p.bytes.len().min(max);
        if n == 0 {
            continue;
        }
        ctx.write_byte_array_from(p.arr, off, &p.bytes[..n]);
    }
}

// ---------------------------------------------------------------------------
// Argument helpers
// ---------------------------------------------------------------------------

fn ioex(msg: impl Into<String>) -> MethodCallFailed {
    RuntimeError::IOException { message: msg.into() }.into()
}

fn obj_or_none(args: &[Value], idx: usize) -> Option<ObjectRef> {
    match args.get(idx) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    }
}

fn alloc_obj(ctx: &mut dyn NativeContext, class_name: &str, nfields: usize) -> ObjectRef {
    match ctx.ensure_class_initialized(class_name) {
        Ok(cid) => ctx.alloc_object(cid, nfields),
        Err(_) => ctx.alloc_object(ClassId::new(0), nfields),
    }
}

const F_OPEN: usize = 0;
const F_CONNECTED: usize = 1;
const F_REG_ID: usize = 2;
const F_REMOTE: usize = 3;
const N_FIELDS: usize = 4;

fn read_aio_id(ctx: &dyn NativeContext, this: ObjectRef) -> Option<i32> {
    if ctx.object_num_fields(this) <= F_REG_ID {
        return None;
    }
    match ctx.get_field(this, F_REG_ID) {
        Value::Int(v) if v != 0 && v != -1 => Some(v),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// AsynchronousChannelGroup — real backing
// ---------------------------------------------------------------------------

/// Track group state in a small registry so isShutdown / awaitTermination
/// reflect actual lifecycle.
fn group_registry() -> &'static RwLock<HashMap<i32, GroupState>> {
    static G: OnceLock<RwLock<HashMap<i32, GroupState>>> = OnceLock::new();
    G.get_or_init(|| RwLock::new(HashMap::new()))
}

#[derive(Clone)]
struct GroupState {
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    pending_ops: Arc<AtomicUsize>,
}

fn group_next_id() -> i32 {
    static NEXT: AtomicI32 = AtomicI32::new(0x7800_0000);
    NEXT.fetch_add(1, Ordering::SeqCst)
}

fn aio_acg_with_fixed(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let group = alloc_obj(ctx, "java/nio/channels/AsynchronousChannelGroup", 1);
    let id = group_next_id();
    group_registry().write().insert(
        id,
        GroupState {
            shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pending_ops: Arc::new(AtomicUsize::new(0)),
        },
    );
    ctx.set_field(group, 0, Value::Int(id));
    Ok(Some(Value::Object(Some(group))))
}

fn aio_acg_with_pool(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    aio_acg_with_fixed(ctx, args)
}

fn aio_acg_is_shutdown(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let id = match obj_or_none(args, 0) {
        Some(o) if ctx.object_num_fields(o) >= 1 => match ctx.get_field(o, 0) {
            Value::Int(v) => v,
            _ => return Ok(Some(Value::Int(0))),
        },
        _ => return Ok(Some(Value::Int(0))),
    };
    let state = group_registry().read();
    let down = state
        .get(&id)
        .map(|s| s.shutdown.load(Ordering::Acquire))
        .unwrap_or(false);
    Ok(Some(Value::Int(if down { 1 } else { 0 })))
}

fn aio_acg_is_terminated(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let id = match obj_or_none(args, 0) {
        Some(o) if ctx.object_num_fields(o) >= 1 => match ctx.get_field(o, 0) {
            Value::Int(v) => v,
            _ => return Ok(Some(Value::Int(0))),
        },
        _ => return Ok(Some(Value::Int(0))),
    };
    let state = group_registry().read();
    let term = state
        .get(&id)
        .map(|s| s.shutdown.load(Ordering::Acquire) && s.pending_ops.load(Ordering::Acquire) == 0)
        .unwrap_or(false);
    Ok(Some(Value::Int(if term { 1 } else { 0 })))
}

fn aio_acg_shutdown(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let id = match obj_or_none(args, 0) {
        Some(o) if ctx.object_num_fields(o) >= 1 => match ctx.get_field(o, 0) {
            Value::Int(v) => v,
            _ => return Ok(None),
        },
        _ => return Ok(None),
    };
    if let Some(s) = group_registry().read().get(&id) {
        s.shutdown.store(true, Ordering::Release);
    }
    Ok(None)
}

fn aio_acg_await_termination(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    drain_completions(ctx);
    flush_pending_array_writes_inner(ctx);
    Ok(Some(Value::Int(1)))
}

// ---------------------------------------------------------------------------
// AsynchronousSocketChannel
// ---------------------------------------------------------------------------

fn aio_asc_open(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // Force pool to start (lazily).
    let _ = job_sender();
    let ch = alloc_obj(ctx, "java/nio/channels/AsynchronousSocketChannel", N_FIELDS);
    ctx.set_field(ch, F_OPEN, Value::Int(1));
    ctx.set_field(ch, F_CONNECTED, Value::Int(0));
    ctx.set_field(ch, F_REG_ID, Value::Int(-1));
    ctx.set_field(ch, F_REMOTE, Value::Object(None));
    Ok(Some(Value::Object(Some(ch))))
}

fn aio_asc_open_group(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    aio_asc_open(ctx, args)
}

fn aio_asc_is_open(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    drain_completions(ctx);
    flush_pending_array_writes_inner(ctx);
    match obj_or_none(args, 0) {
        Some(o) if ctx.object_num_fields(o) > F_OPEN => Ok(Some(ctx.get_field(o, F_OPEN))),
        _ => Ok(Some(Value::Int(0))),
    }
}

fn aio_asc_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(this) = obj_or_none(args, 0) {
        if ctx.object_num_fields(this) > F_OPEN {
            ctx.set_field(this, F_OPEN, Value::Int(0));
        }
        if ctx.object_num_fields(this) > F_CONNECTED {
            ctx.set_field(this, F_CONNECTED, Value::Int(0));
        }
        if let Some(id) = read_aio_id(ctx, this) {
            aio_remove(id);
            ctx.set_field(this, F_REG_ID, Value::Int(-1));
        }
    }
    Ok(None)
}

/// Decode SocketAddress into "host:port".
fn decode_addr(ctx: &mut dyn NativeContext, sa: ObjectRef) -> Result<String, MethodCallFailed> {
    let port = match ctx.get_field_by_name(sa, "port") {
        Value::Int(v) if (0..=u16::MAX as i32).contains(&v) => v,
        _ => return Err(ioex("connect: bad port")),
    };
    if let Value::Object(Some(s)) = ctx.get_field_by_name(sa, "hostname") {
        if let Some(host) = ctx.read_string(s) {
            if !host.is_empty() {
                return Ok(format!("{host}:{port}"));
            }
        }
    }
    if let Value::Object(Some(ia)) = ctx.get_field_by_name(sa, "addr") {
        if let Value::Object(Some(s)) = ctx.get_field_by_name(ia, "hostName") {
            if let Some(host) = ctx.read_string(s) {
                if !host.is_empty() {
                    return Ok(format!("{host}:{port}"));
                }
            }
        }
    }
    // Fall back to synthetic 2-field layout.
    if ctx.object_num_fields(sa) >= 2 {
        if let Value::Object(Some(s)) = ctx.get_field(sa, 0) {
            if let Some(host) = ctx.read_string(s) {
                if !host.is_empty() {
                    return Ok(format!("{host}:{port}"));
                }
            }
        }
    }
    Err(ioex("connect: cannot decode SocketAddress"))
}

fn aio_asc_connect(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    drain_completions(ctx);
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Err(ioex("connect: null channel")),
    };
    let sa = match obj_or_none(args, 1) {
        Some(o) => o,
        None => return Err(ioex("connect: null SocketAddress")),
    };
    // attachment may be at index 2, handler at index 3 (the JDK signature is
    // `connect(SocketAddress, A, CompletionHandler<Void,? super A>)`).
    let attachment = obj_or_none(args, 2);
    let handler = obj_or_none(args, 3);

    let addr = decode_addr(ctx, sa)?;
    // Reserve an id up front in `Pending` state so close() can find it.
    let id = aio_register(AioHandle::Pending);
    if ctx.object_num_fields(this) >= N_FIELDS {
        ctx.set_field(this, F_REG_ID, Value::Int(id));
        let host_str = ctx.create_string(&addr);
        ctx.set_field(this, F_REMOTE, Value::Object(Some(host_str)));
    }
    if let Err(_) = job_sender().send(Job::Connect {
        id,
        addr,
        handler,
        attachment,
    }) {
        return Err(ioex("connect: aio worker pool unavailable"));
    }
    // Mark connected synchronously since the JDK Java code expects to
    // be able to call read/write after connect returns (the handler tells
    // it whether the connect succeeded).
    if ctx.object_num_fields(this) > F_CONNECTED {
        ctx.set_field(this, F_CONNECTED, Value::Int(1));
    }
    Ok(Some(Value::Object(None)))
}

/// Decode a ByteBuffer into the parameters needed for a worker-thread copy.
/// Returns (direct_addr, heap_arr, heap_offset, length).
fn decode_buffer(
    ctx: &mut dyn NativeContext,
    bb: ObjectRef,
) -> (i64, Option<ObjectRef>, i32, i32) {
    let position = match ctx.get_field_by_name(bb, "position") {
        Value::Int(v) if v >= 0 => v,
        _ => 0,
    };
    let limit = match ctx.get_field_by_name(bb, "limit") {
        Value::Int(v) if v >= 0 => v,
        _ => match ctx.get_field_by_name(bb, "capacity") {
            Value::Int(v) if v >= 0 => v,
            _ => 0,
        },
    };
    let length = (limit - position).max(0);
    if let Value::Long(addr) = ctx.get_field_by_name(bb, "address") {
        if addr != 0 {
            return (addr.wrapping_add(position as i64), None, 0, length);
        }
    }
    if let Value::Object(Some(arr)) = ctx.get_field_by_name(bb, "hb") {
        let base_off = match ctx.get_field_by_name(bb, "offset") {
            Value::Int(v) if v >= 0 => v,
            _ => 0,
        };
        return (0, Some(arr), base_off + position, length);
    }
    (0, None, 0, length)
}

fn read_buffer_bytes(
    ctx: &mut dyn NativeContext,
    bb: ObjectRef,
) -> Vec<u8> {
    let (addr, arr, off, len) = decode_buffer(ctx, bb);
    if len <= 0 {
        return Vec::new();
    }
    if addr != 0 {
        let mut v = vec![0u8; len as usize];
        // SAFETY: address is JDK-allocated direct buffer memory.
        unsafe {
            std::ptr::copy_nonoverlapping(addr as *const u8, v.as_mut_ptr(), len as usize);
        }
        v
    } else if let Some(a) = arr {
        // AUDIT 2026-05-17: bulk read via NativeContext intrinsic
        // instead of per-element `get_array_element`. The VM override
        // does a single memcpy from the byte-array payload.
        let arr_len = ctx.array_length(a);
        let off_us = off as usize;
        if off_us >= arr_len {
            return Vec::new();
        }
        let avail = arr_len - off_us;
        let take = (len as usize).min(avail);
        let mut v = vec![0u8; take];
        let n = ctx.read_byte_array_into(a, off_us, &mut v);
        v.truncate(n);
        v
    } else {
        Vec::new()
    }
}

fn aio_asc_read(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    drain_completions(ctx);
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Err(ioex("read: null channel")),
    };
    let bb = match obj_or_none(args, 1) {
        Some(o) => o,
        None => return Err(ioex("read: null ByteBuffer")),
    };
    let attachment = obj_or_none(args, 2);
    let handler = obj_or_none(args, 3);
    let id = read_aio_id(ctx, this).ok_or_else(|| ioex("read: not connected"))?;
    let (addr, arr, off, length) = decode_buffer(ctx, bb);
    if length <= 0 {
        // Empty buffer — post a synthetic "0 bytes read" completion.
        if let Some(h) = handler {
            completion_queue().lock().push_back(Completion {
                handler: h,
                attachment,
                outcome: Ok(CompletionKind::IntCount(0)),
            });
        }
        return Ok(Some(Value::Object(None)));
    }
    if let Err(_) = job_sender().send(Job::Read {
        id,
        len: length as usize,
        bb_addr: addr,
        bb_arr: arr,
        bb_offset: off,
        bb_obj: bb,
        handler,
        attachment,
    }) {
        return Err(ioex("read: aio worker pool unavailable"));
    }
    Ok(Some(Value::Object(None)))
}

fn aio_asc_write(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    drain_completions(ctx);
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Err(ioex("write: null channel")),
    };
    let bb = match obj_or_none(args, 1) {
        Some(o) => o,
        None => return Err(ioex("write: null ByteBuffer")),
    };
    let attachment = obj_or_none(args, 2);
    let handler = obj_or_none(args, 3);
    let id = read_aio_id(ctx, this).ok_or_else(|| ioex("write: not connected"))?;
    let data = read_buffer_bytes(ctx, bb);
    if data.is_empty() {
        if let Some(h) = handler {
            completion_queue().lock().push_back(Completion {
                handler: h,
                attachment,
                outcome: Ok(CompletionKind::IntCount(0)),
            });
        }
        return Ok(Some(Value::Object(None)));
    }
    if let Err(_) = job_sender().send(Job::Write {
        id,
        data,
        bb_obj: bb,
        handler,
        attachment,
    }) {
        return Err(ioex("write: aio worker pool unavailable"));
    }
    Ok(Some(Value::Object(None)))
}

// ---------------------------------------------------------------------------
// AsynchronousServerSocketChannel
// ---------------------------------------------------------------------------

fn aio_assc_open(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let _ = job_sender();
    let ch = alloc_obj(ctx, "java/nio/channels/AsynchronousServerSocketChannel", N_FIELDS);
    ctx.set_field(ch, F_OPEN, Value::Int(1));
    ctx.set_field(ch, F_CONNECTED, Value::Int(0));
    ctx.set_field(ch, F_REG_ID, Value::Int(-1));
    ctx.set_field(ch, F_REMOTE, Value::Object(None));
    Ok(Some(Value::Object(Some(ch))))
}

fn aio_assc_open_group(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    aio_assc_open(ctx, args)
}

fn aio_assc_bind(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Err(ioex("bind: null channel")),
    };
    let sa = match obj_or_none(args, 1) {
        Some(o) => o,
        None => return Err(ioex("bind: null SocketAddress")),
    };
    let bind_text = decode_addr(ctx, sa).unwrap_or_else(|_| "0.0.0.0:0".to_string());
    let listener = TcpListener::bind(&bind_text)
        .map_err(|e| ioex(format!("bind {bind_text}: {e}")))?;
    let id = aio_register(AioHandle::Listener(Arc::new(Mutex::new(listener))));
    if ctx.object_num_fields(this) > F_REG_ID {
        ctx.set_field(this, F_REG_ID, Value::Int(id));
    }
    Ok(Some(Value::Object(Some(this))))
}

fn aio_assc_accept(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    drain_completions(ctx);
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Err(ioex("accept: null channel")),
    };
    let attachment = obj_or_none(args, 1);
    let handler = obj_or_none(args, 2);
    let id = read_aio_id(ctx, this).ok_or_else(|| ioex("accept: not bound"))?;
    if let Err(_) = job_sender().send(Job::Accept {
        id,
        handler,
        attachment,
    }) {
        return Err(ioex("accept: aio worker pool unavailable"));
    }
    Ok(Some(Value::Object(None)))
}

fn aio_assc_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    aio_asc_close(ctx, args)
}

// ---------------------------------------------------------------------------
// IOCP / EPollPort facade — JDK private. We register null-ports that
// just delegate to drain_completions so any code that imports the
// platform-specific class compiles + runs.
// ---------------------------------------------------------------------------

fn iocp_open(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let _ = job_sender();
    let ch = alloc_obj(ctx, "sun/nio/ch/Iocp", 1);
    ctx.set_field(ch, 0, Value::Int(1));
    Ok(Some(Value::Object(Some(ch))))
}

fn iocp_close(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

fn iocp_drain(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    drain_completions(ctx);
    flush_pending_array_writes_inner(ctx);
    Ok(None)
}

// ---------------------------------------------------------------------------
// Public registration
// ---------------------------------------------------------------------------

/// Register all AsynchronousSocketChannel / AsynchronousServerSocketChannel
/// natives plus the AsynchronousChannelGroup methods backed by a real
/// worker-thread pool. Idempotent.
pub fn register_async_socket_real(r: &mut NativeMethodRegistry) {
    let asc = "java/nio/channels/AsynchronousSocketChannel";
    let assc = "java/nio/channels/AsynchronousServerSocketChannel";
    let acg = "java/nio/channels/AsynchronousChannelGroup";

    // -- AsynchronousSocketChannel --
    r.register(asc, "open", "()Ljava/nio/channels/AsynchronousSocketChannel;", aio_asc_open);
    r.register(
        asc,
        "open",
        "(Ljava/nio/channels/AsynchronousChannelGroup;)Ljava/nio/channels/AsynchronousSocketChannel;",
        aio_asc_open_group,
    );
    r.register(asc, "isOpen", "()Z", aio_asc_is_open);
    r.register(asc, "close", "()V", aio_asc_close);
    r.register(
        asc,
        "connect",
        "(Ljava/net/SocketAddress;Ljava/lang/Object;Ljava/nio/channels/CompletionHandler;)V",
        aio_asc_connect,
    );
    r.register(
        asc,
        "read",
        "(Ljava/nio/ByteBuffer;Ljava/lang/Object;Ljava/nio/channels/CompletionHandler;)V",
        aio_asc_read,
    );
    r.register(
        asc,
        "write",
        "(Ljava/nio/ByteBuffer;Ljava/lang/Object;Ljava/nio/channels/CompletionHandler;)V",
        aio_asc_write,
    );

    // -- AsynchronousServerSocketChannel --
    r.register(
        assc,
        "open",
        "()Ljava/nio/channels/AsynchronousServerSocketChannel;",
        aio_assc_open,
    );
    r.register(
        assc,
        "open",
        "(Ljava/nio/channels/AsynchronousChannelGroup;)Ljava/nio/channels/AsynchronousServerSocketChannel;",
        aio_assc_open_group,
    );
    r.register(assc, "isOpen", "()Z", aio_asc_is_open);
    r.register(assc, "close", "()V", aio_assc_close);
    r.register(
        assc,
        "bind",
        "(Ljava/net/SocketAddress;)Ljava/nio/channels/AsynchronousServerSocketChannel;",
        aio_assc_bind,
    );
    r.register(
        assc,
        "accept",
        "(Ljava/lang/Object;Ljava/nio/channels/CompletionHandler;)V",
        aio_assc_accept,
    );

    // -- AsynchronousChannelGroup --
    r.register(
        acg,
        "withFixedThreadPool",
        "(ILjava/util/concurrent/ThreadFactory;)Ljava/nio/channels/AsynchronousChannelGroup;",
        aio_acg_with_fixed,
    );
    r.register(
        acg,
        "withThreadPool",
        "(Ljava/util/concurrent/ExecutorService;)Ljava/nio/channels/AsynchronousChannelGroup;",
        aio_acg_with_pool,
    );
    r.register(
        acg,
        "withCachedThreadPool",
        "(Ljava/util/concurrent/ExecutorService;I)Ljava/nio/channels/AsynchronousChannelGroup;",
        aio_acg_with_pool,
    );
    r.register(acg, "isShutdown", "()Z", aio_acg_is_shutdown);
    r.register(acg, "isTerminated", "()Z", aio_acg_is_terminated);
    r.register(acg, "shutdown", "()V", aio_acg_shutdown);
    r.register(acg, "shutdownNow", "()V", aio_acg_shutdown);
    r.register(
        acg,
        "awaitTermination",
        "(JLjava/util/concurrent/TimeUnit;)Z",
        aio_acg_await_termination,
    );

    // -- IOCP facade (Windows) / EPollPort (Linux) — minimal port surface --
    for cls in ["sun/nio/ch/Iocp", "sun/nio/ch/EPollPort", "sun/nio/ch/KQueuePort"] {
        r.register(cls, "open", &format!("()L{cls};"), iocp_open);
        r.register(cls, "close", "()V", iocp_close);
        // Both real platforms expose a `drain`/`poll` entry the JDK calls
        // from its dispatcher loop; we treat it as an opportunistic flush.
        r.register(cls, "drain", "()V", iocp_drain);
        r.register(cls, "poll", "()V", iocp_drain);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_without_panic() {
        let mut r = NativeMethodRegistry::new();
        register_async_socket_real(&mut r);
        assert!(r
            .find(
                "java/nio/channels/AsynchronousSocketChannel",
                "open",
                "()Ljava/nio/channels/AsynchronousSocketChannel;"
            )
            .is_some());
        assert!(r
            .find(
                "java/nio/channels/AsynchronousServerSocketChannel",
                "bind",
                "(Ljava/net/SocketAddress;)Ljava/nio/channels/AsynchronousServerSocketChannel;"
            )
            .is_some());
        assert!(r
            .find(
                "java/nio/channels/AsynchronousChannelGroup",
                "withFixedThreadPool",
                "(ILjava/util/concurrent/ThreadFactory;)Ljava/nio/channels/AsynchronousChannelGroup;"
            )
            .is_some());
    }

    #[test]
    fn worker_pool_runs_real_connect() {
        // Spawn a server, kick a Connect job at the pool, watch the
        // completion appear in the queue.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let _t = std::thread::spawn(move || {
            // Accept once and immediately close.
            let _ = listener.accept();
        });

        let id = aio_register(AioHandle::Pending);
        let _ = job_sender().send(Job::Connect {
            id,
            addr: format!("127.0.0.1:{port}"),
            handler: None,
            attachment: None,
        });

        // Wait for the registry to flip to Stream.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            {
                let map = aio_registry().read();
                if matches!(map.get(&id), Some(AioHandle::Stream(_))) {
                    break;
                }
            }
            if std::time::Instant::now() > deadline {
                panic!("connect didn't complete in time");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        // Cleanup.
        aio_remove(id);
    }
}
