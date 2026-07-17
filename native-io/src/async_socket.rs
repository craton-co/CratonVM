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

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ClassId, ObjectRef, Value};
use parking_lot::{Condvar, Mutex, RwLock};
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

/// Round-8 C29 fix: actually remove the entry from `aio_registry`
/// instead of leaving an `AioHandle::Closed` tombstone behind. Workers
/// that already cloned an `Arc<Mutex<TcpStream>>` out of the entry keep
/// the stream alive via their Arc — the registry slot is purely a
/// lookup table, and dropping the slot does not invalidate in-flight
/// worker handles. Other readers (Read/Write/Accept) match on the live
/// variants (`Stream(_)` / `Listener(_)`) and treat any other state as
/// "channel closed", so converting the previous `Closed` tombstone into
/// a true `remove` is observationally equivalent to callers but stops
/// the map from growing monotonically on heavy connect/close churn.
fn aio_remove(id: i32) {
    aio_registry().write().remove(&id);
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
    // Round-8 C29: apply any field resets parked by failed worker jobs
    // (e.g. clearing `F_CONNECTED` after a connect error) before we
    // dispatch the matching completion — handlers must observe the
    // post-failure state, not the pre-failure optimistic state.
    flush_pending_field_resets(ctx);
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
                        let ch =
                            alloc_obj(ctx, "java/nio/channels/AsynchronousSocketChannel", N_FIELDS);
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
// Handler-form read completion delivery (WP3.2 fix)
//
// The legacy `completion_queue` above is drained lazily by the next user-thread
// AIO native call. That never happens for a purely event-driven client (the
// Tomcat WebSocket client arms a read and then blocks on a latch), so those
// completions were never delivered. The path below fixes the dominant case —
// the handler-form `read` — with a dedicated queue, a condvar the AIO
// *dispatcher* thread waits on, and proactive delivery from that dispatcher
// (which holds a real `NativeContext` and can invoke the Java handler). The
// handler / attachment / target buffer are held as global GC roots across the
// blocking read so a moving collection cannot relocate them out from under us.
// ---------------------------------------------------------------------------

/// Outcome of a handler-form read, decided by the worker, delivered by the
/// dispatcher.
pub enum ReadOutcome {
    /// `n = bytes.len()` bytes were read; write them into the buffer and deliver
    /// `completed(n)`.
    Bytes(Vec<u8>),
    /// Peer closed (blocking read returned 0): deliver `completed(-1)`.
    Eof,
    /// Deliver `completed(count)` without touching the buffer (e.g. a zero-length
    /// read request ⇒ `completed(0)`).
    Count(i32),
    /// Deliver `failed(IOException(msg))`.
    Error(String),
}

/// A completed handler-form read awaiting delivery to its `CompletionHandler`.
pub struct ReadCompletion {
    /// Global-root handle for the `CompletionHandler` (never 0).
    handler_gref: usize,
    /// Global-root handle for the `attachment` (0 ⇒ null attachment).
    attachment_gref: usize,
    /// Global-root handle for the destination `ByteBuffer` (0 ⇒ none).
    buffer_gref: usize,
    /// What to deliver.
    outcome: ReadOutcome,
}

/// Result parked by a Future-form read or write.  The worker threads never
/// touch Java objects directly; the dispatcher resolves these roots and
/// completes the real JDK CompletableFuture on an attached VM thread.
pub enum FutureOutcome {
    Bytes(Vec<u8>),
    Count(i32),
    Eof,
    Error(String),
}

pub struct FutureCompletion {
    future_gref: usize,
    buffer_gref: usize,
    outcome: FutureOutcome,
}

enum DispatcherCompletion {
    Read(ReadCompletion),
    Future(FutureCompletion),
}

fn read_completion_state() -> &'static (
    Mutex<std::collections::VecDeque<DispatcherCompletion>>,
    Condvar,
) {
    static S: OnceLock<(
        Mutex<std::collections::VecDeque<DispatcherCompletion>>,
        Condvar,
    )> = OnceLock::new();
    S.get_or_init(|| {
        (
            Mutex::new(std::collections::VecDeque::new()),
            Condvar::new(),
        )
    })
}

/// Park a completed read and wake the dispatcher.
fn push_read_completion(c: ReadCompletion) {
    let (q, cv) = read_completion_state();
    q.lock().push_back(DispatcherCompletion::Read(c));
    cv.notify_one();
}

/// Park a Future-form completion and wake the VM-attached dispatcher.
fn push_future_completion(c: FutureCompletion) {
    let (q, cv) = read_completion_state();
    q.lock().push_back(DispatcherCompletion::Future(c));
    cv.notify_one();
}

/// Block up to `timeout` for at least one pending read completion. Returns
/// `true` if one is available. Called by the AIO dispatcher thread while it is
/// in the GC-blocked idle region (no `NativeContext` needed).
pub fn wait_for_pending(timeout: std::time::Duration) -> bool {
    let (q, cv) = read_completion_state();
    let mut guard = q.lock();
    if !guard.is_empty() {
        return true;
    }
    cv.wait_for(&mut guard, timeout);
    !guard.is_empty()
}

/// Maximum read completions delivered per dispatcher wake-up.
const READ_DRAIN_LIMIT: usize = 256;

/// Deliver pending handler-form read completions, invoking
/// `CompletionHandler.completed` / `failed` on the calling (dispatcher) thread.
/// Requires a live `NativeContext`, so it must run on a VM/attached thread.
pub fn drain_completions_pub(ctx: &mut dyn NativeContext) {
    for _ in 0..READ_DRAIN_LIMIT {
        let next = read_completion_state().0.lock().pop_front();
        let Some(c) = next else { break };
        match c {
            DispatcherCompletion::Read(c) => deliver_read_completion(ctx, c),
            DispatcherCompletion::Future(c) => deliver_future_completion(ctx, c),
        }
    }
}

fn deliver_read_completion(ctx: &mut dyn NativeContext, c: ReadCompletion) {
    // Destructure up front: the gref handles are `Copy`, the outcome is moved
    // into the match — so the roots can still be released afterwards.
    let ReadCompletion {
        handler_gref,
        attachment_gref,
        buffer_gref,
        outcome,
    } = c;
    // Nothing to deliver to if the handler root is gone; just release.
    if ctx.resolve_global_root(handler_gref).is_none() {
        release_read_roots(ctx, handler_gref, attachment_gref, buffer_gref);
        return;
    }
    match outcome {
        ReadOutcome::Bytes(bytes) => {
            // The buffer write performs no Java allocation, so resolving the
            // buffer here (before the Integer box) is safe.
            let n = match (buffer_gref != 0)
                .then(|| ctx.resolve_global_root(buffer_gref))
                .flatten()
            {
                Some(bb) => write_into_buffer_and_advance(ctx, bb, &bytes),
                None => 0,
            };
            deliver_completed(ctx, handler_gref, attachment_gref, n);
        }
        // Peer closed — completed(-1) per the JDK contract.
        ReadOutcome::Eof => deliver_completed(ctx, handler_gref, attachment_gref, -1),
        ReadOutcome::Count(cnt) => deliver_completed(ctx, handler_gref, attachment_gref, cnt),
        ReadOutcome::Error(msg) => deliver_failed(ctx, handler_gref, attachment_gref, &msg),
    }
    release_read_roots(ctx, handler_gref, attachment_gref, buffer_gref);
}

fn deliver_future_completion(ctx: &mut dyn NativeContext, c: FutureCompletion) {
    let FutureCompletion {
        future_gref,
        buffer_gref,
        outcome,
    } = c;
    let completion = match outcome {
        FutureOutcome::Bytes(bytes) => {
            let n = ctx
                .resolve_global_root(buffer_gref)
                .map(|bb| write_into_buffer_and_advance(ctx, bb, &bytes))
                .unwrap_or(0);
            Ok(box_int(ctx, n))
        }
        FutureOutcome::Count(n) => {
            // BUG FIX (2026-07-17, StompWebSocketIntegrationTests premature-close
            // investigation): `Count(n)` is how `aio_asc_write_future`'s real
            // completion (`Job::WriteFutureFd`, below) reports a successful
            // write back to the Future -- but until this fix, that path never
            // advanced the SOURCE `ByteBuffer`'s `position`, unlike the sibling
            // `Bytes(..)` (read) arm above which correctly calls
            // `write_into_buffer_and_advance`. That violates
            // `AsynchronousByteChannel.write`'s documented contract ("the
            // buffer's position is updated to reflect the bytes written").
            // A conforming caller that loops `while (buf.hasRemaining())
            // channel.write(buf).get()` -- exactly Tomcat's own
            // `WsRemoteEndpointImplBase`/`WsRemoteEndpointImplClient` write
            // path -- therefore saw the SAME unconsumed-looking buffer after
            // every "successful" write and resubmitted it, physically
            // resending the same frame bytes on the wire many times (confirmed
            // via `CRATONVM_DBG_SC_READ`: a single server-side read of one
            // Jetty-backed connection contained the client's 35-byte STOMP
            // CONNECT WebSocket frame repeated exactly 67 times back-to-back,
            // 2345 = 35*67; the Tomcat backend hit the same bug thousands of
            // times per its own retry cadence). The server correctly rejects
            // each redundant CONNECT with STOMP's "Session already exists"
            // guard and closes -- the premature close chased across the
            // 2026-07-16 sessions. `Count(n)` is otherwise only ever produced
            // with `n == 0` for two degenerate early-outs (an empty write
            // buffer here, and `aio_asc_read_future`'s zero-capacity
            // destination case), so advancing by `n` is a harmless no-op
            // there and the correct fix for the real (n > 0) write-completion
            // case.
            if n > 0 {
                if let Some(bb) = ctx.resolve_global_root(buffer_gref) {
                    let position = match ctx.get_field_by_name(bb, "position") {
                        Value::Int(v) if v >= 0 => v,
                        _ => 0,
                    };
                    ctx.set_field_by_name(bb, "position", Value::Int(position + n));
                }
            }
            Ok(box_int(ctx, n))
        }
        FutureOutcome::Eof => Ok(box_int(ctx, -1)),
        FutureOutcome::Error(message) => Err(message),
    };

    match completion {
        Ok(value) => {
            if let Some(future) = ctx.resolve_global_root(future_gref) {
                let _ = ctx.invoke_virtual(future, "complete", "(Ljava/lang/Object;)Z", &[value]);
            }
        }
        Err(message) => {
            // Keep the message rooted across exception allocation: both allocations
            // can trigger a moving collection before the future is completed.
            let msg = ctx.create_string(&message);
            let msg_gref = ctx.add_global_root(msg);
            let throwable = match ctx.new_object("java/io/IOException") {
                Ok(Some(Value::Object(Some(t)))) => Some(t),
                _ => None,
            };
            if let Some(throwable) = throwable {
                let msg = ctx.resolve_global_root(msg_gref).unwrap_or(msg);
                ctx.set_field_by_name(throwable, "detailMessage", Value::Object(Some(msg)));
                if let Some(future) = ctx.resolve_global_root(future_gref) {
                    // `CompletableFuture.completeExceptionally` is also
                    // overridden for the VM's synthetic CF model. This is a
                    // real JDK CF, so go straight through its real private
                    // completion primitive and then release any waiter
                    // Signallers with `postComplete`, mirroring the
                    // real-aware `native_cf_complete` normal-success path.
                    let _ = ctx.invoke_virtual(
                        future,
                        "completeThrowable",
                        "(Ljava/lang/Throwable;)Z",
                        &[Value::Object(Some(throwable))],
                    );
                    let _ = ctx.invoke_virtual(future, "postComplete", "()V", &[]);
                }
            }
            ctx.remove_global_root(msg_gref);
        }
    }
    ctx.remove_global_root(future_gref);
    ctx.remove_global_root(buffer_gref);
}

fn release_read_roots(
    ctx: &mut dyn NativeContext,
    handler_gref: usize,
    attachment_gref: usize,
    buffer_gref: usize,
) {
    ctx.remove_global_root(handler_gref);
    if attachment_gref != 0 {
        ctx.remove_global_root(attachment_gref);
    }
    if buffer_gref != 0 {
        ctx.remove_global_root(buffer_gref);
    }
}

/// Box the count, then resolve handler/attachment FRESH — *after* the allocation
/// — so a moving collection during boxing cannot leave them stale, and invoke
/// `CompletionHandler.completed(Integer, attachment)`.
fn deliver_completed(
    ctx: &mut dyn NativeContext,
    handler_gref: usize,
    attachment_gref: usize,
    n: i32,
) {
    let result_val = box_int(ctx, n);
    let Some(h) = ctx.resolve_global_root(handler_gref) else {
        return;
    };
    let attach = if attachment_gref != 0 {
        ctx.resolve_global_root(attachment_gref)
    } else {
        None
    };
    let _ = ctx.invoke(
        "java/nio/channels/CompletionHandler",
        "completed",
        "(Ljava/lang/Object;Ljava/lang/Object;)V",
        &[Value::Object(Some(h)), result_val, Value::Object(attach)],
    );
}

/// Build an `IOException(msg)`, then resolve handler/attachment last, and invoke
/// `CompletionHandler.failed(Throwable, attachment)`.
fn deliver_failed(
    ctx: &mut dyn NativeContext,
    handler_gref: usize,
    attachment_gref: usize,
    msg: &str,
) {
    // Temporarily root the message string so building the exception (an
    // allocation) can't strand it under a moving collector.
    let m = ctx.create_string(msg);
    let m_gref = ctx.add_global_root(m);
    let throwable = match ctx.new_object("java/io/IOException") {
        Ok(Some(Value::Object(Some(t)))) => Some(t),
        _ => None,
    };
    let Some(t) = throwable else {
        ctx.remove_global_root(m_gref);
        return;
    };
    let m_now = ctx.resolve_global_root(m_gref).unwrap_or(m);
    ctx.set_field_by_name(t, "detailMessage", Value::Object(Some(m_now)));
    ctx.remove_global_root(m_gref);
    let Some(h) = ctx.resolve_global_root(handler_gref) else {
        return;
    };
    let attach = if attachment_gref != 0 {
        ctx.resolve_global_root(attachment_gref)
    } else {
        None
    };
    let _ = ctx.invoke(
        "java/nio/channels/CompletionHandler",
        "failed",
        "(Ljava/lang/Throwable;Ljava/lang/Object;)V",
        &[
            Value::Object(Some(h)),
            Value::Object(Some(t)),
            Value::Object(attach),
        ],
    );
}

/// Box an `int` into a `java.lang.Integer` for `completed(Object, Object)`.
fn box_int(ctx: &mut dyn NativeContext, n: i32) -> Value {
    match ctx.new_object("java/lang/Integer") {
        Ok(Some(Value::Object(Some(boxed)))) => {
            ctx.set_field_by_name(boxed, "value", Value::Int(n));
            Value::Object(Some(boxed))
        }
        _ => Value::Int(n),
    }
}

/// Write `bytes` into `bb` at its current position and advance `position` by the
/// number written, mirroring `AsynchronousSocketChannel.read` filling the
/// buffer. Returns the byte count written (clamped to the buffer's remaining).
fn write_into_buffer_and_advance(ctx: &mut dyn NativeContext, bb: ObjectRef, bytes: &[u8]) -> i32 {
    let position = match ctx.get_field_by_name(bb, "position") {
        Value::Int(v) if v >= 0 => v,
        _ => 0,
    };
    let (addr, arr, off, remaining) = decode_buffer(ctx, bb);
    if remaining <= 0 {
        return 0;
    }
    let n = bytes.len().min(remaining as usize);
    if n == 0 {
        return 0;
    }
    if addr != 0 {
        // Direct buffer: write into off-heap memory at the position offset.
        ctx.copy_to_native_memory(addr, &bytes[..n]);
    } else if let Some(a) = arr {
        ctx.write_byte_array_from(a, off as usize, &bytes[..n]);
    } else {
        return 0;
    }
    ctx.set_field_by_name(bb, "position", Value::Int(position + n as i32));
    n as i32
}

// ---------------------------------------------------------------------------
// AIO dispatcher launcher hook.
//
// The dispatcher thread (which attaches to the VM and can invoke Java) lives in
// the `vm` crate. It registers a launcher closure here at startup; the first
// handler-form read fires it exactly once via `ensure_dispatcher`.
// ---------------------------------------------------------------------------

type DispatcherLauncher = Box<dyn Fn() + Send + Sync + 'static>;

fn dispatcher_launcher() -> &'static Mutex<Option<DispatcherLauncher>> {
    static L: OnceLock<Mutex<Option<DispatcherLauncher>>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(None))
}

/// Install the AIO dispatcher launcher (called once by the VM at init).
pub fn set_dispatcher_launcher(f: DispatcherLauncher) {
    *dispatcher_launcher().lock() = Some(f);
}

/// Start the AIO dispatcher on first use (idempotent).
fn ensure_dispatcher() {
    static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    if let Some(launch) = dispatcher_launcher().lock().as_ref() {
        launch();
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
        /// Round-8 C29: the user-visible `AsynchronousSocketChannel`
        /// object whose `F_CONNECTED` flag was set optimistically by
        /// `aio_asc_connect` before this job ran. On connect failure
        /// the worker parks a `PendingFieldReset` so the next user-
        /// thread drain clears it back to 0 — otherwise the channel
        /// would lie to `isConnected()` after a failed connect.
        channel: Option<ObjectRef>,
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
    /// WP3.2 completion-delivery fix: a handler-form
    /// `AsynchronousSocketChannel.read(ByteBuffer, A, CompletionHandler)` whose
    /// underlying connection is owned by the VM `fd_table` (the channel was
    /// connected via the Future-form `connect`). `stream` is an independent
    /// `try_clone()` handle so this blocking read does not contend with the
    /// application's concurrent Future-form writes on the same fd. The
    /// handler / attachment / target buffer are held alive across the read as
    /// *global* GC roots (`*_gref`); the worker parks a [`ReadCompletion`] and
    /// the AIO dispatcher thread delivers it.
    ReadFd {
        stream: Arc<Mutex<TcpStream>>,
        len: usize,
        handler_gref: usize,
        attachment_gref: usize,
        buffer_gref: usize,
    },
    /// Future-form read against an fd_table-backed channel.  The returned
    /// CompletableFuture and target ByteBuffer stay globally rooted until the
    /// VM-attached dispatcher applies the result.
    ReadFutureFd {
        stream: Arc<Mutex<TcpStream>>,
        len: usize,
        future_gref: usize,
        buffer_gref: usize,
    },
    /// Future-form write against an fd_table-backed channel.  This is the
    /// important counterpart to `ReadFutureFd`: the caller must receive a
    /// pending Future immediately rather than block in `send()`.
    WriteFutureFd {
        stream: Arc<Mutex<TcpStream>>,
        data: Vec<u8>,
        future_gref: usize,
        buffer_gref: usize,
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
            channel,
        } => {
            // Task #16: route through the shared outbound-policy hook so
            // the async path matches the blocking-NIO path's SSRF posture.
            // The configured connect timeout (default 30 s) is honoured by
            // `policy_connect`. Policy denials surface as a "connect failed"
            // completion identical to a hard connect error — the JDK's
            // `AsynchronousSocketChannel.connect` already wraps that as a
            // failed `Future`.
            let result = match crate::outbound_policy::policy_connect(&addr) {
                Ok(s) => Ok(s),
                Err(crate::outbound_policy::PolicyConnectError::Denied(reason)) => {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        format!("connect denied by outbound policy: {reason}"),
                    ))
                }
                Err(crate::outbound_policy::PolicyConnectError::Io(e)) => Err(e),
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
                    // Round-8 C29: reset the optimistic `F_CONNECTED = 1`
                    // that `aio_asc_connect` set synchronously before
                    // enqueuing this job. Without this, `isConnected()`
                    // returns true after a failed connect — a JDK
                    // contract violation. We can't touch the Java object
                    // from this worker thread; park a field-reset that
                    // the next user-thread drain applies.
                    if let Some(ch) = channel {
                        pending_field_resets().lock().push(PendingFieldReset {
                            target: ch,
                            field: F_CONNECTED,
                            value: Value::Int(0),
                        });
                        // Also clear the registry id so callers don't
                        // try to look up a now-removed handle.
                        pending_field_resets().lock().push(PendingFieldReset {
                            target: ch,
                            field: F_REG_ID,
                            value: Value::Int(-1),
                        });
                    }
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
                        // R2 audit: this raw memcpy runs on the worker thread
                        // (no `NativeContext`), so it cannot route through
                        // `copy_to_native_memory`. That is safe because
                        // `bb_addr` here can only be a *real* direct-buffer
                        // pointer: we intercept at the public
                        // `AsynchronousSocketChannel.read` level, where a heap
                        // buffer is taken via `bb_arr` (parked above) and we
                        // never substitute a `Util.getTemporaryDirectBuffer`
                        // arena handle. So an arena handle never reaches here.
                        // SAFETY: caller-allocated direct buffer; the
                        // address is valid for `len` bytes.
                        unsafe {
                            std::ptr::copy_nonoverlapping(buf.as_ptr(), bb_addr as *mut u8, n);
                        }
                    } else if let Some(arr) = bb_arr {
                        pending_array_writes().lock().push(PendingArrayWrite {
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
                            e_outer = Some(std::io::Error::from(ErrorKind::WriteZero));
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
                    let new_id = aio_register(AioHandle::Stream(Arc::new(Mutex::new(stream))));
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
        Job::ReadFd {
            stream,
            len,
            handler_gref,
            attachment_gref,
            buffer_gref,
        } => {
            // Blocking read on the cloned handle. The clone is private to this
            // worker, so we hold its lock for the duration without blocking the
            // application's writes (which go through the original fd entry).
            let mut buf = vec![0u8; len.max(1)];
            let read_res = {
                let s = stream.lock();
                let mut r = &*s;
                r.read(&mut buf)
            };
            let outcome = match read_res {
                // 0 bytes from a blocking read == peer closed == EOF. Delivered
                // to the handler as `completed(-1)` (JDK contract).
                Ok(0) => ReadOutcome::Eof,
                Ok(n) => {
                    buf.truncate(n);
                    ReadOutcome::Bytes(buf)
                }
                Err(e) => ReadOutcome::Error(format!("read failed: {e}")),
            };
            push_read_completion(ReadCompletion {
                handler_gref,
                attachment_gref,
                buffer_gref,
                outcome,
            });
        }
        Job::ReadFutureFd {
            stream,
            len,
            future_gref,
            buffer_gref,
        } => {
            let mut buf = vec![0u8; len.max(1)];
            let read_res = {
                let s = stream.lock();
                let mut r = &*s;
                r.read(&mut buf)
            };
            let outcome = match read_res {
                Ok(0) => FutureOutcome::Eof,
                Ok(n) => {
                    buf.truncate(n);
                    FutureOutcome::Bytes(buf)
                }
                Err(e) => FutureOutcome::Error(format!("read failed: {e}")),
            };
            push_future_completion(FutureCompletion {
                future_gref,
                buffer_gref,
                outcome,
            });
        }
        Job::WriteFutureFd {
            stream,
            data,
            future_gref,
            buffer_gref,
        } => {
            let total = data.len();
            let mut written = 0;
            let write_res = {
                let s = stream.lock();
                let mut w = &*s;
                let mut failure = None;
                while written < total {
                    match w.write(&data[written..]) {
                        Ok(0) => {
                            failure = Some(std::io::Error::from(ErrorKind::WriteZero));
                            break;
                        }
                        Ok(n) => written += n,
                        Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                        Err(e) => {
                            failure = Some(e);
                            break;
                        }
                    }
                }
                failure.map_or(Ok(written), Err)
            };
            let outcome = match write_res {
                Ok(n) => FutureOutcome::Count(n as i32),
                Err(e) => FutureOutcome::Error(format!("write failed: {e}")),
            };
            push_future_completion(FutureCompletion {
                future_gref,
                buffer_gref,
                outcome,
            });
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

    impl<T> std::fmt::Display for SendError<T> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("send on closed channel")
        }
    }

    impl<T> std::fmt::Debug for SendError<T> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("SendError(<closed>)")
        }
    }

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

/// Round-8 C29 fix: workers can't touch the user-visible Java object
/// directly (no `&mut NativeContext`). When a worker needs to reset a
/// field on a channel — e.g. clearing `F_CONNECTED = 0` after a connect
/// failure — it parks a `PendingFieldReset` here and the next AIO native
/// call on the user thread drains it via `flush_pending_field_resets`.
struct PendingFieldReset {
    target: ObjectRef,
    field: usize,
    value: Value,
}

fn pending_field_resets() -> &'static Mutex<Vec<PendingFieldReset>> {
    static V: OnceLock<Mutex<Vec<PendingFieldReset>>> = OnceLock::new();
    V.get_or_init(|| Mutex::new(Vec::new()))
}

fn flush_pending_field_resets(ctx: &mut dyn NativeContext) {
    let parked = std::mem::take(&mut *pending_field_resets().lock());
    for r in parked {
        if ctx.object_num_fields(r.target) > r.field {
            ctx.set_field(r.target, r.field, r.value);
        }
    }
}

// ---------------------------------------------------------------------------
// Argument helpers
// ---------------------------------------------------------------------------

fn ioex(msg: impl Into<String>) -> MethodCallFailed {
    RuntimeError::IOException {
        message: msg.into(),
    }
    .into()
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
    flush_pending_field_resets(ctx);
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
///
/// DF07: the previous implementation read `port` / `hostname` / `addr`
/// DIRECTLY off the `InetSocketAddress` object. Under CRATONVM_REAL_NET_SOCKETS
/// the real JDK `java.net.InetSocketAddress` keeps that state in an inner
/// `InetSocketAddressHolder holder` (and `InetAddress` keeps its host in its
/// own holder), so the direct reads all missed: `port` resolved to absent →
/// `Err("connect: bad port")`, or (when a literal-IP host partially decoded) a
/// wildcard `0.0.0.0` that `std::net` connect rejects with WSAEADDRNOTAVAIL
/// (os error 10049). The websocket client's `connectToServer` hits exactly this
/// (handler-form `AsynchronousSocketChannel.connect`).
///
/// Delegate to the blocking path's `decode_socket_address`, the single robust
/// decoder: it tries the public `getPort()` / `getHostString()` accessors
/// first, then the real-JDK `holder` fields, then the synthetic 2-field layout.
fn decode_addr(ctx: &mut dyn NativeContext, sa: ObjectRef) -> Result<String, MethodCallFailed> {
    let (host, port) = crate::socket_channel::decode_socket_address(ctx, sa)?;
    Ok(format!("{host}:{port}"))
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
    if let Err(e) = job_sender().send(Job::Connect {
        id,
        addr,
        handler,
        attachment,
        // Round-8 C29: pass the user-visible channel so the worker can
        // park a `F_CONNECTED = 0` reset on connect failure.
        channel: Some(this),
    }) {
        // 2026-05-24: previously swallowed with `Err(_)`; the caller
        // and the user-visible CompletionHandler had no way to
        // distinguish "channel just closed" from any other failure.
        // Log the structured error and surface it.
        eprintln!(
            "native-io: aio_asc_connect: job channel closed; \
             CompletionHandler will not fire (id={id}, err={e})"
        );
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
fn decode_buffer(ctx: &mut dyn NativeContext, bb: ObjectRef) -> (i64, Option<ObjectRef>, i32, i32) {
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
    // Heap buffer FIRST: a real-JDK `HeapByteBuffer` keeps its backing array in
    // `hb` and — crucially — sets `Buffer.address` to the array base offset
    // (`Unsafe.ARRAY_BYTE_BASE_OFFSET`, 16 on HotSpot), NOT 0. Checking
    // `address != 0` before `hb` would misclassify every heap buffer as a
    // direct buffer and write to address `0x10`. Only a true `DirectByteBuffer`
    // has `hb == null` and a real off-heap `address`.
    if let Value::Object(Some(arr)) = ctx.get_field_by_name(bb, "hb") {
        let base_off = match ctx.get_field_by_name(bb, "offset") {
            Value::Int(v) if v >= 0 => v,
            _ => 0,
        };
        return (0, Some(arr), base_off + position, length);
    }
    // Direct buffer: `address` is a real off-heap pointer; advance by position.
    if let Value::Long(addr) = ctx.get_field_by_name(bb, "address") {
        if addr != 0 {
            return (addr.wrapping_add(position as i64), None, 0, length);
        }
    }
    (0, None, 0, length)
}

fn read_buffer_bytes(ctx: &mut dyn NativeContext, bb: ObjectRef) -> Vec<u8> {
    let (addr, arr, off, len) = decode_buffer(ctx, bb);
    if len <= 0 {
        return Vec::new();
    }
    if addr != 0 {
        let mut v = vec![0u8; len as usize];
        // `addr` is a direct-buffer address. Route through the context so an
        // `Unsafe.allocateMemory` arena handle reads from the off-heap store
        // rather than being dereferenced raw; a real pointer falls through to
        // a raw copy. (The worker-thread write-back path below only ever sees
        // a real pointer or a parked heap array — see the Job::Read handler.)
        if !ctx.copy_from_native_memory(addr, &mut v) {
            return Vec::new();
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

/// Lowest `aio_registry` id (see `aio_next_id`). fd_table fds are always below
/// this, so a slot-2 value in `[0, AIO_REG_BASE)` is an fd_table fd (the channel
/// was connected via the Future-form `connect`), not a legacy registry id.
const AIO_REG_BASE: i64 = 0x7000_0000;

/// Allocate a real, initially-incomplete `CompletableFuture`. `new_object` is
/// intentional: the no-arg constructor has no semantic initialization beyond
/// the zero/null field values already installed by allocation, while using the
/// static `completedFuture` helper would make `get(timeout)` return eagerly.
fn aio_pending_future(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    match ctx.new_object("java/util/concurrent/CompletableFuture")? {
        Some(Value::Object(Some(future))) => Ok(future),
        _ => Err(ioex("could not allocate CompletableFuture")),
    }
}

/// Register a Future completion after the caller has received a pending real
/// JDK CompletableFuture. Both it and the ByteBuffer are global roots because
/// the worker can remain blocked in socket I/O across a moving collection.
fn aio_future_roots(
    ctx: &mut dyn NativeContext,
    bb: ObjectRef,
) -> Result<(ObjectRef, usize, usize), MethodCallFailed> {
    let buffer_gref = ctx.add_global_root(bb);
    let future = match aio_pending_future(ctx) {
        Ok(future) => future,
        Err(error) => {
            ctx.remove_global_root(buffer_gref);
            return Err(error);
        }
    };
    let future_gref = ctx.add_global_root(future);
    Ok((future, future_gref, buffer_gref))
}

/// Future-form `AsynchronousSocketChannel.read(ByteBuffer)`. Unlike the old
/// Phase-67 registration, this returns before the blocking recv runs, and its
/// `Future.get(timeout, unit)` therefore owns the timeout contract.
fn aio_asc_read_future(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_or_none(args, 0).ok_or_else(|| ioex("read: null channel"))?;
    let bb = obj_or_none(args, 1).ok_or_else(|| ioex("read: null ByteBuffer"))?;
    ensure_dispatcher();
    let (future, future_gref, buffer_gref) = aio_future_roots(ctx, bb)?;
    let post = |outcome| {
        push_future_completion(FutureCompletion {
            future_gref,
            buffer_gref,
            outcome,
        });
        Ok(Some(Value::Object(Some(future))))
    };
    let fd = match ctx.get_field(this, F_REG_ID) {
        Value::Int(v) if v >= 0 && (v as i64) < AIO_REG_BASE => v as u32,
        _ => return post(FutureOutcome::Error("read: not connected".to_string())),
    };
    let (_, _, _, length) = decode_buffer(ctx, bb);
    if length <= 0 {
        return post(FutureOutcome::Count(0));
    }
    let stream = match ctx.fd_table().try_clone_tcp(fd) {
        Ok(stream) => stream,
        Err(error) => return post(FutureOutcome::Error(format!("read: {error}"))),
    };
    if job_sender()
        .send(Job::ReadFutureFd {
            stream: Arc::new(Mutex::new(stream)),
            len: length as usize,
            future_gref,
            buffer_gref,
        })
        .is_err()
    {
        return post(FutureOutcome::Error(
            "read: aio worker pool unavailable".to_string(),
        ));
    }
    Ok(Some(Value::Object(Some(future))))
}

/// Future-form `AsynchronousSocketChannel.write(ByteBuffer)`. The previous
/// built-in performed `send()` inline and returned an already-completed Future,
/// which made callers such as Tomcat's WebSocket timeout test hang *before*
/// reaching `Future.get(timeout, unit)`. This queues the write, then completes
/// the Future from the VM-attached dispatcher when the worker finishes.
fn aio_asc_write_future(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_or_none(args, 0).ok_or_else(|| ioex("write: null channel"))?;
    let bb = obj_or_none(args, 1).ok_or_else(|| ioex("write: null ByteBuffer"))?;
    ensure_dispatcher();
    let (future, future_gref, buffer_gref) = aio_future_roots(ctx, bb)?;
    let post = |outcome| {
        push_future_completion(FutureCompletion {
            future_gref,
            buffer_gref,
            outcome,
        });
        Ok(Some(Value::Object(Some(future))))
    };
    let fd = match ctx.get_field(this, F_REG_ID) {
        Value::Int(v) if v >= 0 && (v as i64) < AIO_REG_BASE => v as u32,
        _ => return post(FutureOutcome::Error("write: not connected".to_string())),
    };
    let data = read_buffer_bytes(ctx, bb);
    if data.is_empty() {
        return post(FutureOutcome::Count(0));
    }
    let stream = match ctx.fd_table().try_clone_tcp(fd) {
        Ok(stream) => stream,
        Err(error) => return post(FutureOutcome::Error(format!("write: {error}"))),
    };
    if job_sender()
        .send(Job::WriteFutureFd {
            stream: Arc::new(Mutex::new(stream)),
            data,
            future_gref,
            buffer_gref,
        })
        .is_err()
    {
        return post(FutureOutcome::Error(
            "write: aio worker pool unavailable".to_string(),
        ));
    }
    Ok(Some(Value::Object(Some(future))))
}

/// Handler-form `AsynchronousSocketChannel.read(ByteBuffer, A, CompletionHandler)`.
///
/// The previous worker-pool implementation parked the completion in a queue that
/// was only drained on the next user-thread AIO native call — which never
/// happens for an event-driven client that arms a read then blocks (the Tomcat
/// WebSocket client), so server→client frames were silently dropped. This path
/// instead reads the connection's fd_table fd, performs the blocking read on a
/// `try_clone()` handle (a worker thread), and delivers the completion
/// proactively from the AIO dispatcher thread. The handler / attachment / buffer
/// are held as global GC roots across the read.
fn aio_asc_read(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Err(ioex("read: null channel")),
    };
    let bb = match obj_or_none(args, 1) {
        Some(o) => o,
        None => return Err(ioex("read: null ByteBuffer")),
    };
    let attachment = obj_or_none(args, 2);
    let handler = match obj_or_none(args, 3) {
        Some(h) => h,
        // No CompletionHandler ⇒ nothing to deliver (the Future-form read is a
        // separate native registered elsewhere).
        None => return Ok(Some(Value::Object(None))),
    };

    // Start the dispatcher on first use so parked completions get delivered.
    ensure_dispatcher();

    // Register global roots + park a completion for worker-free delivery —
    // empty-buffer and error cases that have no bytes to read.
    let post_immediate = |ctx: &mut dyn NativeContext, outcome: ReadOutcome| {
        let hg = ctx.add_global_root(handler);
        let ag = attachment.map(|a| ctx.add_global_root(a)).unwrap_or(0);
        push_read_completion(ReadCompletion {
            handler_gref: hg,
            attachment_gref: ag,
            buffer_gref: 0,
            outcome,
        });
    };

    // The channel was connected via the Future-form `connect`, which stores the
    // fd_table fd in slot 2.
    let fd = match ctx.get_field(this, 2) {
        Value::Int(v) if v >= 0 && (v as i64) < AIO_REG_BASE => v as u32,
        _ => {
            post_immediate(ctx, ReadOutcome::Error("read: not connected".to_string()));
            return Ok(Some(Value::Object(None)));
        }
    };

    let (_, _, _, length) = decode_buffer(ctx, bb);
    if length <= 0 {
        post_immediate(ctx, ReadOutcome::Count(0));
        return Ok(Some(Value::Object(None)));
    }

    // Independent read handle so the blocking read does not contend with the
    // application's Future-form writes on the same fd.
    let stream = match ctx.fd_table().try_clone_tcp(fd) {
        Ok(s) => s,
        Err(e) => {
            post_immediate(ctx, ReadOutcome::Error(format!("read: {e}")));
            return Ok(Some(Value::Object(None)));
        }
    };

    let handler_gref = ctx.add_global_root(handler);
    let attachment_gref = attachment.map(|a| ctx.add_global_root(a)).unwrap_or(0);
    let buffer_gref = ctx.add_global_root(bb);

    if job_sender()
        .send(Job::ReadFd {
            stream: Arc::new(Mutex::new(stream)),
            len: length as usize,
            handler_gref,
            attachment_gref,
            buffer_gref,
        })
        .is_err()
    {
        // Pool gone: report failure to the handler (roots already taken).
        push_read_completion(ReadCompletion {
            handler_gref,
            attachment_gref,
            buffer_gref,
            outcome: ReadOutcome::Error("read: aio worker pool unavailable".to_string()),
        });
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
    if let Err(e) = job_sender().send(Job::Write {
        id,
        data,
        bb_obj: bb,
        handler,
        attachment,
    }) {
        eprintln!(
            "native-io: aio_asc_write: job channel closed; \
             CompletionHandler will not fire (id={id}, err={e})"
        );
        return Err(ioex("write: aio worker pool unavailable"));
    }
    Ok(Some(Value::Object(None)))
}

// ---------------------------------------------------------------------------
// AsynchronousServerSocketChannel
// ---------------------------------------------------------------------------

fn aio_assc_open(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let _ = job_sender();
    let ch = alloc_obj(
        ctx,
        "java/nio/channels/AsynchronousServerSocketChannel",
        N_FIELDS,
    );
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
    let listener =
        TcpListener::bind(&bind_text).map_err(|e| ioex(format!("bind {bind_text}: {e}")))?;
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
    if let Err(e) = job_sender().send(Job::Accept {
        id,
        handler,
        attachment,
    }) {
        eprintln!(
            "native-io: aio_assc_accept: job channel closed; \
             CompletionHandler will not fire (id={id}, err={e})"
        );
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
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let asc = "java/nio/channels/AsynchronousSocketChannel";
    let assc = "java/nio/channels/AsynchronousServerSocketChannel";
    let acg = "java/nio/channels/AsynchronousChannelGroup";

    // -- AsynchronousSocketChannel --
    r.register(
        asc,
        "open",
        "()Ljava/nio/channels/AsynchronousSocketChannel;",
        aio_asc_open,
    );
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
        "(Ljava/nio/ByteBuffer;)Ljava/util/concurrent/Future;",
        aio_asc_read_future,
    );
    r.register(
        asc,
        "write",
        "(Ljava/nio/ByteBuffer;)Ljava/util/concurrent/Future;",
        aio_asc_write_future,
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
    for cls in [
        "sun/nio/ch/Iocp",
        "sun/nio/ch/EPollPort",
        "sun/nio/ch/KQueuePort",
    ] {
        r.register(cls, "open", &format!("()L{cls};"), iocp_open);
        r.register(cls, "close", "()V", iocp_close);
        // Both real platforms expose a `drain`/`poll` entry the JDK calls
        // from its dispatcher loop; we treat it as an opportunistic flush.
        r.register(cls, "drain", "()V", iocp_drain);
        r.register(cls, "poll", "()V", iocp_drain);
    }
    r.set_category(__prev_cat);
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
        assert!(r
            .find(
                "java/nio/channels/AsynchronousSocketChannel",
                "read",
                "(Ljava/nio/ByteBuffer;)Ljava/util/concurrent/Future;"
            )
            .is_some());
        assert!(r
            .find(
                "java/nio/channels/AsynchronousSocketChannel",
                "write",
                "(Ljava/nio/ByteBuffer;)Ljava/util/concurrent/Future;"
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
            channel: None,
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
